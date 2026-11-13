//! Branch and compute management.
//!
//! A branch is storage only: a directory in the copy-on-write volume holding
//! a Postgres data directory. *Compute* (a Postgres process) is attached to a
//! branch on demand, when the first connection for it arrives, and detached
//! again when it has been idle or when another branch needs the slot. That is
//! what lets a volume hold thousands of branches on a machine that can only
//! run a handful of Postgres instances at once.

use std::collections::HashMap;
use std::net::SocketAddr;
use std::path::PathBuf;
use std::process::Stdio;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use anyhow::{anyhow, bail, Context, Result};
use banyan_store::{BranchId, SharedStore};
use tokio::process::{Child, Command};

use crate::api_types::{Branch, CreateBranch, Created, Gc, Status};
use crate::pgwire::{self, Probe};

#[derive(Clone, Debug)]
pub struct Config {
    pub data_dir: PathBuf,
    pub mount_dir: PathBuf,
    /// Unix sockets of the Postgres instances.
    pub run_dir: PathBuf,
    pub log_dir: PathBuf,
    pub pg_bin: PathBuf,
    pub listen: SocketAddr,
    pub api: SocketAddr,
    pub max_running: usize,
    pub idle_timeout: Duration,
    pub shared_buffers: String,
    pub max_connections: u32,
    /// Extra `name=value` settings passed to every instance.
    pub pg_options: Vec<String>,
    /// Database used when a client names only a branch.
    pub default_database: String,
}

/// Errors the API maps to specific HTTP statuses.
#[derive(Debug)]
pub enum Refusal {
    NotFound(String),
    Conflict(String),
    Bad(String),
}

impl std::fmt::Display for Refusal {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Refusal::NotFound(m) | Refusal::Conflict(m) | Refusal::Bad(m) => f.write_str(m),
        }
    }
}

impl std::error::Error for Refusal {}

struct Running {
    child: Child,
    port: u16,
}

struct Slot {
    /// Held while the instance is being started or stopped.
    state: tokio::sync::Mutex<Option<Running>>,
    /// The branch's write counter right after its last CHECKPOINT. While the
    /// counter still has this value, another checkpoint would be a no-op.
    checkpointed_at: Mutex<Option<u64>>,
    connections: AtomicUsize,
    last_used: Mutex<Instant>,
}

pub struct Manager {
    pub cfg: Config,
    pub store: Arc<SharedStore>,
    slots: Mutex<HashMap<String, Arc<Slot>>>,
    running: AtomicUsize,
    uid: u32,
    gid: u32,
}

/// Decrements a branch's connection count when a client goes away.
pub struct ConnectionGuard(Arc<Slot>);

impl Drop for ConnectionGuard {
    fn drop(&mut self) {
        *self.0.last_used.lock().unwrap() = Instant::now();
        self.0.connections.fetch_sub(1, Ordering::SeqCst);
    }
}

impl Manager {
    pub fn new(cfg: Config, store: Arc<SharedStore>) -> Self {
        let (uid, gid) = unsafe { (libc::getuid(), libc::getgid()) };
        Manager { cfg, store, slots: Mutex::new(HashMap::new()), running: AtomicUsize::new(0), uid, gid }
    }

    fn slot(&self, name: &str) -> Arc<Slot> {
        self.slots
            .lock()
            .unwrap()
            .entry(name.to_string())
            .or_insert_with(|| {
                Arc::new(Slot {
                    state: tokio::sync::Mutex::new(None),
                    checkpointed_at: Mutex::new(None),
                    connections: AtomicUsize::new(0),
                    last_used: Mutex::new(Instant::now()),
                })
            })
            .clone()
    }

    fn socket(&self, port: u16) -> PathBuf {
        self.cfg.run_dir.join(format!(".s.PGSQL.{port}"))
    }

    pub fn data_directory(&self, branch: &str) -> PathBuf {
        self.cfg.mount_dir.join(branch).join("pgdata")
    }

    fn branch_id(&self, name: &str) -> Result<BranchId> {
        self.store.lock().branch_id(name).ok_or_else(|| Refusal::NotFound(format!("branch \"{name}\" does not exist")).into())
    }

    /// Register a client connection for `branch`, starting its Postgres
    /// instance if needed. Returns the instance's socket path.
    pub async fn connect(&self, branch: &str) -> Result<(PathBuf, ConnectionGuard)> {
        let id = self.branch_id(branch)?;
        let slot = self.slot(branch);
        // Count the connection first so the instance cannot be chosen for
        // eviction between starting and being used.
        slot.connections.fetch_add(1, Ordering::SeqCst);
        let guard = ConnectionGuard(slot.clone());
        let port = self.ensure_running(branch, id, &slot).await?;
        Ok((self.socket(port), guard))
    }

    async fn ensure_running(&self, branch: &str, id: BranchId, slot: &Arc<Slot>) -> Result<u16> {
        let mut state = slot.state.lock().await;
        if let Some(r) = state.as_mut() {
            if r.child.try_wait()?.is_none() {
                return Ok(r.port);
            }
            log::warn!("postgres for branch {branch} exited unexpectedly; restarting it");
            *state = None;
            self.running.fetch_sub(1, Ordering::SeqCst);
        }
        self.make_room(branch).await;
        let started = Instant::now();
        let running = self.start_postgres(branch, id).await?;
        let port = running.port;
        *state = Some(running);
        self.running.fetch_add(1, Ordering::SeqCst);
        log::info!("branch {branch}: postgres ready in {:?}", started.elapsed());
        Ok(port)
    }

    /// Stop least-recently-used idle instances until there is a free slot.
    async fn make_room(&self, except: &str) {
        while self.running.load(Ordering::SeqCst) >= self.cfg.max_running {
            let candidates: Vec<(String, Arc<Slot>)> = self.slots.lock().unwrap().iter().map(|(n, s)| (n.clone(), s.clone())).collect();
            let victim = candidates
                .into_iter()
                .filter(|(n, s)| n != except && s.connections.load(Ordering::SeqCst) == 0)
                .filter(|(_, s)| s.state.try_lock().map(|st| st.is_some()).unwrap_or(false))
                .min_by_key(|(_, s)| *s.last_used.lock().unwrap());
            let Some((name, slot)) = victim else {
                log::warn!("all {} running instances have clients; starting one more than --max-running", self.cfg.max_running);
                return;
            };
            let mut state = slot.state.lock().await;
            if slot.connections.load(Ordering::SeqCst) == 0 {
                if let Some(r) = state.take() {
                    log::info!("branch {name}: stopping idle postgres to free a slot");
                    self.stop_postgres(r).await;
                }
            }
        }
    }

    async fn start_postgres(&self, branch: &str, id: BranchId) -> Result<Running> {
        let data = self.data_directory(branch);
        if !tokio::fs::try_exists(data.join("PG_VERSION")).await.unwrap_or(false) {
            bail!("branch \"{branch}\" has no Postgres data directory");
        }
        // The volume is locked by this daemon, so no other postmaster can be
        // using this directory. A pid file here is a leftover: from the parent
        // branch at the moment of the fork, or from an unclean stop.
        let _ = tokio::fs::remove_file(data.join("postmaster.pid")).await;
        let port = 1024 + (id % 60000) as u16;
        let _ = tokio::fs::remove_file(self.socket(port)).await;
        let log = std::fs::OpenOptions::new().create(true).append(true).open(self.cfg.log_dir.join(format!("{branch}.log")))?;
        let mut cmd = Command::new(self.cfg.pg_bin.join("postgres"));
        cmd.arg("-D").arg(&data).arg("-p").arg(port.to_string());
        let settings = [
            "listen_addresses=".to_string(),
            format!("unix_socket_directories={}", self.cfg.run_dir.display()),
            format!("shared_buffers={}", self.cfg.shared_buffers),
            format!("max_connections={}", self.cfg.max_connections),
            "logging_collector=off".to_string(),
            // A branch forked from a running parent starts with crash
            // recovery. By default Postgres first fsyncs every file in the
            // data directory, one call each, which dominates start-up time
            // here. One syncfs call replaces that sweep. Nothing is lost: an
            // fsync of any file syncs the whole volume, and recovery ends
            // with a checkpoint that fsyncs before the instance takes writes.
            "recovery_init_sync_method=syncfs".to_string(),
        ];
        for s in settings.iter().chain(self.cfg.pg_options.iter()) {
            cmd.arg("-c").arg(s);
        }
        cmd.stdin(Stdio::null()).stdout(log.try_clone()?).stderr(log);
        unsafe {
            cmd.pre_exec(|| {
                // If the daemon dies, take the instance down with it: its
                // storage is gone the moment the filesystem is unmounted.
                libc::prctl(libc::PR_SET_PDEATHSIG, libc::SIGQUIT);
                Ok(())
            });
        }
        let mut child = cmd.spawn().with_context(|| format!("start {}", self.cfg.pg_bin.join("postgres").display()))?;
        let socket = self.socket(port);
        let deadline = Instant::now() + Duration::from_secs(300);
        loop {
            if let Some(status) = child.try_wait()? {
                bail!(
                    "postgres for branch \"{branch}\" exited during startup ({status}); see {}",
                    self.cfg.log_dir.join(format!("{branch}.log")).display()
                );
            }
            if let Probe::Ready = pgwire::probe(&socket).await {
                return Ok(Running { child, port });
            }
            if Instant::now() > deadline {
                let _ = child.start_kill();
                bail!("postgres for branch \"{branch}\" did not become ready in time");
            }
            tokio::time::sleep(Duration::from_millis(5)).await;
        }
    }

    async fn stop_postgres(&self, mut r: Running) {
        if let Some(pid) = r.child.id() {
            // SIGINT = fast shutdown: disconnect clients, checkpoint, exit.
            unsafe { libc::kill(pid as i32, libc::SIGINT) };
        }
        if tokio::time::timeout(Duration::from_secs(120), r.child.wait()).await.is_err() {
            log::warn!("postgres did not stop in time; killing it");
            let _ = r.child.kill().await;
        }
        self.running.fetch_sub(1, Ordering::SeqCst);
    }

    /// Stop a branch's instance. With `force` false this fails while clients
    /// are connected.
    pub async fn stop(&self, branch: &str, force: bool) -> Result<bool> {
        self.branch_id(branch)?;
        let slot = self.slot(branch);
        let mut state = slot.state.lock().await;
        let clients = slot.connections.load(Ordering::SeqCst);
        if clients > 0 && !force {
            return Err(Refusal::Conflict(format!("branch \"{branch}\" has {clients} open connection(s)")).into());
        }
        match state.take() {
            Some(r) => {
                self.stop_postgres(r).await;
                Ok(true)
            }
            None => Ok(false),
        }
    }

    /// Create `req.name` as a copy of `req.from` at this instant.
    pub async fn create_branch(&self, req: &CreateBranch) -> Result<Created> {
        self.branch_id(&req.from)?;
        let mut checkpoint_micros = 0;
        if req.checkpoint {
            let from_id = self.branch_id(&req.from)?;
            let slot = self.slot(&req.from);
            let state = slot.state.lock().await;
            if let Some(r) = state.as_ref() {
                // Skip the checkpoint when the parent has not written anything
                // since the previous one: forking the same idle branch many
                // times should not cost a checkpoint each time.
                let writes = self.store.lock().branch_writes(from_id);
                if writes.is_none() || *slot.checkpointed_at.lock().unwrap() != writes {
                    let t = Instant::now();
                    match pgwire::simple_query(&self.socket(r.port), "postgres", "CHECKPOINT").await {
                        Ok(()) => *slot.checkpointed_at.lock().unwrap() = self.store.lock().branch_writes(from_id),
                        Err(e) => log::warn!("checkpoint of {} before fork failed ({e:#}); the new branch will replay WAL on first start", req.from),
                    }
                    checkpoint_micros = t.elapsed().as_micros() as u64;
                }
            }
        }
        let t = Instant::now();
        let id = self.store.lock().fork(&req.name, Some(&req.from), self.uid, self.gid).map_err(|e| match e {
            banyan_store::Error::BranchExists => anyhow!(Refusal::Conflict(format!("branch \"{}\" already exists", req.name))),
            banyan_store::Error::BadBranchName => anyhow!(Refusal::Bad(e.to_string())),
            other => anyhow!(other),
        })?;
        let fork_micros = t.elapsed().as_micros() as u64;
        let mut sync_micros = 0;
        if req.durable {
            let t = Instant::now();
            let store = self.store.clone();
            tokio::task::spawn_blocking(move || store.sync()).await??;
            sync_micros = t.elapsed().as_micros() as u64;
        }
        Ok(Created { name: req.name.clone(), id, parent: req.from.clone(), fork_micros, checkpoint_micros, sync_micros })
    }

    pub async fn delete_branch(&self, branch: &str, force: bool) -> Result<()> {
        self.stop(branch, force).await?;
        self.store.lock().delete_branch(branch).map_err(|e| anyhow!(e))?;
        self.slots.lock().unwrap().remove(branch);
        Ok(())
    }

    pub fn branches(&self) -> Vec<Branch> {
        let infos = self.store.lock().branches();
        let slots = self.slots.lock().unwrap();
        infos
            .into_iter()
            .map(|b| {
                let slot = slots.get(&b.name);
                let running = slot.map(|s| s.state.try_lock().map(|st| st.is_some()).unwrap_or(true)).unwrap_or(false);
                Branch {
                    id: b.id,
                    parent: b.parent,
                    created: b.created as f64 / 1e9,
                    compute: if running { "running" } else { "stopped" }.to_string(),
                    connections: slot.map(|s| s.connections.load(Ordering::SeqCst)).unwrap_or(0),
                    name: b.name,
                }
            })
            .collect()
    }

    pub fn status(&self) -> Status {
        let st = self.store.lock().stats();
        Status {
            version: env!("CARGO_PKG_VERSION").to_string(),
            branches: st.branches,
            running: self.running.load(Ordering::SeqCst),
            max_running: self.cfg.max_running,
            block_size: st.block_size,
            allocated_blocks: st.allocated_blocks,
            free_blocks: st.free_blocks,
            used_bytes: (st.allocated_blocks - st.free_blocks) * st.block_size,
            journal_bytes: st.journal_bytes,
        }
    }

    pub async fn gc(&self) -> Result<Gc> {
        let store = self.store.clone();
        let t = Instant::now();
        let r = tokio::task::spawn_blocking(move || store.gc()).await??;
        Ok(Gc { live_blocks: r.live_blocks, free_blocks: r.free_blocks, newly_freed: r.newly_freed, millis: t.elapsed().as_millis() as u64 })
    }

    /// Stop instances that have had no clients for the idle timeout.
    pub async fn reap_idle(&self) {
        let slots: Vec<(String, Arc<Slot>)> = self.slots.lock().unwrap().iter().map(|(n, s)| (n.clone(), s.clone())).collect();
        for (name, slot) in slots {
            if slot.connections.load(Ordering::SeqCst) > 0 || slot.last_used.lock().unwrap().elapsed() < self.cfg.idle_timeout {
                continue;
            }
            let Ok(mut state) = slot.state.try_lock() else { continue };
            if slot.connections.load(Ordering::SeqCst) == 0 {
                if let Some(r) = state.take() {
                    log::info!("branch {name}: stopping postgres after {:?} idle", self.cfg.idle_timeout);
                    self.stop_postgres(r).await;
                }
            }
        }
    }

    /// Stop every instance (used on shutdown).
    pub async fn stop_all(&self) {
        let slots: Vec<Arc<Slot>> = self.slots.lock().unwrap().values().cloned().collect();
        let mut stopping = Vec::new();
        for slot in slots {
            if let Some(r) = slot.state.lock().await.take() {
                stopping.push(self.stop_postgres(r));
            }
        }
        for s in stopping {
            s.await;
        }
    }
}
