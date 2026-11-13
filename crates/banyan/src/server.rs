//! `banyan serve`: open the volume, mount it, and run the proxy and API.

use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::{Duration, Instant};

use anyhow::{bail, Context, Result};
use banyan_store::{FileDisk, SharedStore};
use tokio::net::TcpListener;
use tokio::signal::unix::{signal, SignalKind};

use crate::manager::{Config, Manager};
use crate::{api, proxy};

/// Locate the directory holding `postgres` and `initdb`.
pub fn find_pg_bin(explicit: Option<PathBuf>) -> Result<PathBuf> {
    let has = |dir: &Path| dir.join("postgres").is_file() && dir.join("initdb").is_file();
    if let Some(dir) = explicit {
        if has(&dir) {
            return Ok(dir);
        }
        bail!("{} does not contain postgres and initdb", dir.display());
    }
    if let Some(paths) = std::env::var_os("PATH") {
        if let Some(dir) = std::env::split_paths(&paths).find(|d| has(d)) {
            return Ok(dir);
        }
    }
    // Debian/Ubuntu and RHEL-style layouts, newest version first.
    let mut found: Vec<(u32, PathBuf)> = Vec::new();
    for (root, prefix) in [("/usr/lib/postgresql", ""), ("/usr", "pgsql-")] {
        let Ok(entries) = std::fs::read_dir(root) else { continue };
        for e in entries.flatten() {
            let name = e.file_name().to_string_lossy().into_owned();
            let Some(version) = name.strip_prefix(prefix).and_then(|v| v.parse::<u32>().ok()) else { continue };
            let bin = e.path().join("bin");
            if has(&bin) {
                found.push((version, bin));
            }
        }
    }
    found.sort();
    found.pop().map(|(_, dir)| dir).context("cannot find Postgres binaries; install PostgreSQL or pass --pg-bin <dir>")
}

fn human(bytes: u64) -> String {
    const UNITS: [&str; 5] = ["B", "KiB", "MiB", "GiB", "TiB"];
    let mut v = bytes as f64;
    let mut u = 0;
    while v >= 1024.0 && u < UNITS.len() - 1 {
        v /= 1024.0;
        u += 1;
    }
    if u == 0 {
        format!("{bytes} B")
    } else {
        format!("{v:.1} {}", UNITS[u])
    }
}

pub fn run(cfg: Config) -> Result<()> {
    if unsafe { libc::getuid() } == 0 {
        bail!("banyan must run as a regular user: Postgres refuses to run as root");
    }
    // A previous daemon that was killed leaves a dead mount behind; clear it
    // before touching the mount point.
    let _ = std::process::Command::new("fusermount3").arg("-u").arg("-z").arg(&cfg.mount_dir).stderr(std::process::Stdio::null()).status();
    for dir in [&cfg.data_dir, &cfg.mount_dir, &cfg.run_dir, &cfg.log_dir] {
        std::fs::create_dir_all(dir).with_context(|| format!("create {}", dir.display()))?;
    }

    let opened = Instant::now();
    let disk = FileDisk::open(&cfg.data_dir).with_context(|| format!("open volume {}", cfg.data_dir.display()))?;
    let store = Arc::new(SharedStore::open(Arc::new(disk)).context("replay journal")?);
    let stats = store.lock().stats();
    log::info!(
        "volume {}: {} branch(es), {} in use, journal {} replayed in {:?}",
        cfg.data_dir.display(),
        stats.branches,
        human((stats.allocated_blocks - stats.free_blocks) * stats.block_size),
        human(stats.journal_bytes),
        opened.elapsed()
    );

    let session = banyan_fuse::mount(store.clone(), &cfg.mount_dir, false)
        .with_context(|| format!("mount {} (is FUSE available? try: --device /dev/fuse --cap-add SYS_ADMIN)", cfg.mount_dir.display()))?;

    let manager = Arc::new(Manager::new(cfg.clone(), store.clone()));
    if stats.branches == 0 {
        init_main(&manager)?;
    }

    let runtime = tokio::runtime::Builder::new_multi_thread().enable_all().build()?;
    let result = runtime.block_on(serve(manager.clone()));
    runtime.block_on(manager.stop_all());
    if let Err(e) = store.sync() {
        log::error!("final sync failed: {e}");
    }
    drop(session);
    log::info!("stopped");
    result
}

/// First start on an empty volume: create `main` and initialise a cluster in it.
fn init_main(manager: &Manager) -> Result<()> {
    let (uid, gid) = unsafe { (libc::getuid(), libc::getgid()) };
    manager.store.lock().fork("main", None, uid, gid)?;
    let data = manager.data_directory("main");
    log::info!("new volume: running initdb in {}", data.display());
    let out = std::process::Command::new(manager.cfg.pg_bin.join("initdb"))
        .arg("-D")
        .arg(&data)
        .args(["-U", "postgres", "--auth=trust", "--data-checksums", "--encoding=UTF8", "--locale=C.UTF-8"])
        .output()
        .context("run initdb")?;
    if !out.status.success() {
        let _ = manager.store.lock().delete_branch("main");
        bail!("initdb failed:\n{}{}", String::from_utf8_lossy(&out.stdout), String::from_utf8_lossy(&out.stderr));
    }
    manager.store.sync()?;
    Ok(())
}

async fn serve(manager: Arc<Manager>) -> Result<()> {
    let cfg = &manager.cfg;
    let pg = TcpListener::bind(cfg.listen).await.with_context(|| format!("listen on {}", cfg.listen))?;
    let http = TcpListener::bind(cfg.api).await.with_context(|| format!("listen on {}", cfg.api))?;
    log::info!("postgres endpoint  {}   e.g. psql \"host={} port={} user=postgres dbname=main\"", cfg.listen, cfg.listen.ip(), cfg.listen.port());
    log::info!("control API        http://{}", cfg.api);
    if !cfg.listen.ip().is_loopback() {
        log::warn!(
            "the Postgres endpoint is not bound to loopback and this version does not authenticate clients; restrict access at the network level"
        );
    }

    tokio::spawn(proxy::serve(manager.clone(), pg));
    let app = api::router(manager.clone());
    tokio::spawn(async move {
        if let Err(e) = axum::serve(http, app).await {
            log::error!("API server stopped: {e}");
        }
    });

    // Housekeeping: stop idle instances, and bound how long acknowledged but
    // unsynced metadata can sit in memory.
    let reaper = manager.clone();
    tokio::spawn(async move {
        loop {
            tokio::time::sleep(Duration::from_secs(5)).await;
            reaper.reap_idle().await;
        }
    });
    let (failed_tx, mut failed_rx) = tokio::sync::mpsc::channel::<String>(1);
    let store = manager.store.clone();
    std::thread::Builder::new().name("banyan-flush".into()).spawn(move || loop {
        std::thread::sleep(Duration::from_millis(500));
        if let Err(e) = store.sync() {
            let _ = failed_tx.blocking_send(e.to_string());
            return;
        }
    })?;

    let mut term = signal(SignalKind::terminate())?;
    let mut int = signal(SignalKind::interrupt())?;
    tokio::select! {
        _ = term.recv() => log::info!("SIGTERM: shutting down"),
        _ = int.recv() => log::info!("SIGINT: shutting down"),
        Some(e) = failed_rx.recv() => {
            log::error!("storage failure: {e}");
            bail!("storage failure: {e}");
        }
    }
    Ok(())
}
