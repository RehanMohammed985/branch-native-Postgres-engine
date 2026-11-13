//! Deterministic simulation test.
//!
//! Runs long random sequences of file operations, forks, branch deletions,
//! garbage collections, clean restarts, injected disk failures and simulated
//! power losses against the store, and checks every result against a naive
//! reference model (one full copy of every file per branch).
//!
//! Everything is driven by one seed, so a failure is reproduced exactly with
//! `BANYAN_DST_SEED=<seed> cargo test -p banyan-store --test dst`.
//!
//! After a power loss the store must come back with:
//!   * exactly the branches, directories, file names and file sizes that
//!     existed at the last successful sync (or, if the crash interrupted a
//!     sync, at some operation boundary between that sync and the crash), and
//!   * file bytes that are each either the byte at that boundary or a byte
//!     written to that same position of that same file since the last sync
//!     (an unsynced overwrite may or may not survive, but nothing else may
//!     appear).

use std::collections::{BTreeMap, BTreeSet, HashMap};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::Arc;

use banyan_store::sim::{Rng, SimDisk};
use banyan_store::{BranchId, Error, Kind, SetAttr, Store, BLOCK_SIZE, ROOT_INO};

const BS: usize = BLOCK_SIZE;

#[derive(Clone)]
struct MFile {
    fid: u64,
    data: Arc<Vec<u8>>,
}

#[derive(Clone, Default)]
struct MBranch {
    dirs: BTreeSet<String>,
    files: BTreeMap<String, MFile>,
}

type Model = BTreeMap<String, MBranch>;
/// What the store holds: per branch, its directories and its files' bytes.
type Snapshot = BTreeMap<String, (BTreeSet<String>, BTreeMap<String, Vec<u8>>)>;

fn block_of(data: &[u8], i: usize) -> Vec<u8> {
    let mut b = vec![0u8; BS];
    let start = i * BS;
    if start < data.len() {
        let end = (start + BS).min(data.len());
        b[..end - start].copy_from_slice(&data[start..end]);
    }
    b
}

fn split(path: &str) -> (&str, &str) {
    let i = path.rfind('/').unwrap();
    (&path[..i], &path[i + 1..])
}

struct Sim {
    seed: u64,
    disk: Arc<SimDisk>,
    store: Option<Store>,
    clock: Arc<AtomicU64>,
    model: Model,
    synced: Model,
    /// Model after each operation since the last sync.
    pending: Vec<Model>,
    /// A sync was interrupted by an injected failure since the last good one.
    sync_interrupted: bool,
    /// Block contents written since the last sync, per (branch, file id, block).
    hist: HashMap<(String, u64, u64), Vec<Vec<u8>>>,
    rng: Rng,
    counter: u64,
    step: u64,
    crashes: u64,
    /// Unlinked inodes the simulated kernel still holds open.
    open_orphans: Vec<(BranchId, u64)>,
}

type OpResult = Result<(), Error>;

impl Sim {
    fn new(seed: u64) -> Self {
        let disk = Arc::new(SimDisk::new());
        let clock = Arc::new(AtomicU64::new(1));
        let mut sim = Sim {
            seed,
            disk,
            store: None,
            clock,
            model: Model::new(),
            synced: Model::new(),
            pending: Vec::new(),
            sync_interrupted: false,
            hist: HashMap::new(),
            rng: Rng(seed),
            counter: 0,
            step: 0,
            crashes: 0,
            open_orphans: Vec::new(),
        };
        sim.open();
        sim
    }

    fn open(&mut self) {
        self.open_orphans.clear();
        let clock = self.clock.clone();
        let store = Store::open_with_clock(self.disk.clone(), Box::new(move || clock.fetch_add(1, Ordering::Relaxed)))
            .unwrap_or_else(|e| panic!("seed {} step {}: reopen failed: {e}", self.seed, self.step));
        self.store = Some(store);
    }

    fn st(&mut self) -> &mut Store {
        self.store.as_mut().unwrap()
    }

    fn fresh(&mut self, prefix: &str) -> String {
        self.counter += 1;
        format!("{prefix}{}", self.counter)
    }

    fn bid(&self, branch: &str) -> BranchId {
        self.store.as_ref().unwrap().branch_id(branch).unwrap_or_else(|| panic!("seed {}: branch {branch} missing", self.seed))
    }

    fn ino(&self, branch: &str, path: &str) -> u64 {
        let b = self.bid(branch);
        self.store
            .as_ref()
            .unwrap()
            .resolve(b, path)
            .unwrap_or_else(|e| panic!("seed {} step {}: resolve {branch}:{path}: {e}", self.seed, self.step))
            .ino
    }

    fn pick<T: Clone>(&mut self, items: &[T]) -> Option<T> {
        if items.is_empty() {
            None
        } else {
            Some(items[self.rng.below(items.len() as u64) as usize].clone())
        }
    }

    fn pick_branch(&mut self) -> Option<String> {
        let names: Vec<String> = self.model.keys().cloned().collect();
        self.pick(&names)
    }

    fn pick_file(&mut self) -> Option<(String, String)> {
        let b = self.pick_branch()?;
        let files: Vec<String> = self.model[&b].files.keys().cloned().collect();
        Some((b, self.pick(&files)?))
    }

    fn pick_dir(&mut self, b: &str) -> String {
        let dirs: Vec<String> = self.model[b].dirs.iter().cloned().collect();
        self.pick(&dirs).unwrap()
    }

    fn random_bytes(&mut self, len: usize) -> Vec<u8> {
        let mut v = Vec::with_capacity(len + 8);
        while v.len() < len {
            v.extend_from_slice(&self.rng.next_u64().to_le_bytes());
        }
        v.truncate(len);
        v
    }

    /// Record every block of `(branch, fid)` that differs between `old` and `new`.
    fn record(&mut self, branch: &str, fid: u64, old: &[u8], new: &[u8]) {
        let blocks = old.len().max(new.len()).div_ceil(BS);
        for i in 0..blocks {
            let nb = block_of(new, i);
            if block_of(old, i) != nb {
                self.hist.entry((branch.to_string(), fid, i as u64)).or_default().push(nb);
            }
        }
    }

    // ---- operations -------------------------------------------------------

    fn op_fork(&mut self) -> OpResult {
        if self.model.len() >= 7 {
            return Ok(());
        }
        let name = self.fresh("b");
        let from = if self.model.is_empty() || self.rng.chance(1, 25) { None } else { self.pick_branch() };
        self.st().fork(&name, from.as_deref(), 1000, 1000)?;
        let mb = match &from {
            Some(p) => self.model[p].clone(),
            None => MBranch { dirs: BTreeSet::from([String::new()]), files: BTreeMap::new() },
        };
        self.model.insert(name, mb);
        Ok(())
    }

    fn op_delete_branch(&mut self) -> OpResult {
        if self.model.len() < 2 {
            return Ok(());
        }
        let b = self.pick_branch().unwrap();
        self.st().delete_branch(&b)?;
        self.model.remove(&b);
        Ok(())
    }

    fn op_mkdir(&mut self) -> OpResult {
        let Some(b) = self.pick_branch() else { return Ok(()) };
        let dir = self.pick_dir(&b);
        let name = self.fresh("d");
        let (bid, parent) = (self.bid(&b), self.ino(&b, &dir));
        self.st().create(bid, parent, name.as_bytes(), Kind::Dir, 0o755, 1000, 1000, b"")?;
        self.model.get_mut(&b).unwrap().dirs.insert(format!("{dir}/{name}"));
        Ok(())
    }

    fn op_create(&mut self) -> OpResult {
        let Some(b) = self.pick_branch() else { return Ok(()) };
        let dir = self.pick_dir(&b);
        let name = self.fresh("f");
        let (bid, parent) = (self.bid(&b), self.ino(&b, &dir));
        self.st().create(bid, parent, name.as_bytes(), Kind::File, 0o644, 1000, 1000, b"")?;
        self.counter += 1;
        let fid = self.counter;
        self.model.get_mut(&b).unwrap().files.insert(format!("{dir}/{name}"), MFile { fid, data: Arc::new(Vec::new()) });
        Ok(())
    }

    fn op_write(&mut self) -> OpResult {
        let Some((b, path)) = self.pick_file() else { return Ok(()) };
        let MFile { fid, data: old } = self.model[&b].files[&path].clone();
        let size = old.len() as u64;
        let offset = match self.rng.below(10) {
            0..=3 => self.rng.below(size + 1),
            4..=5 => size,
            6 => (self.rng.below(size / BS as u64 + 2)) * BS as u64,
            7..=8 => size + self.rng.below(3 * BS as u64),
            _ => (64 + self.rng.below(140)) * BS as u64 + self.rng.below(BS as u64),
        };
        let len = match self.rng.below(6) {
            0 => 1 + self.rng.below(16),
            1 => BS as u64,
            2 => 2 * BS as u64,
            _ => 1 + self.rng.below(3 * BS as u64),
        } as usize;
        let bytes = self.random_bytes(len);
        let mut new = (*old).clone();
        if new.len() < offset as usize + len {
            new.resize(offset as usize + len, 0);
        }
        new[offset as usize..offset as usize + len].copy_from_slice(&bytes);
        let (bid, ino) = (self.bid(&b), self.ino(&b, &path));
        let res = self.st().write(bid, ino, offset, &bytes);
        // Record even on failure: part of the write may have reached the disk.
        self.record(&b, fid, &old, &new);
        assert_eq!(res?, len);
        self.model.get_mut(&b).unwrap().files.get_mut(&path).unwrap().data = Arc::new(new);
        Ok(())
    }

    fn op_truncate(&mut self) -> OpResult {
        let Some((b, path)) = self.pick_file() else { return Ok(()) };
        let MFile { fid, data: old } = self.model[&b].files[&path].clone();
        let new_size = match self.rng.below(4) {
            0 => 0,
            1 => (self.rng.below(old.len() as u64 / BS as u64 + 2)) * BS as u64,
            _ => self.rng.below(old.len() as u64 + 2 * BS as u64),
        } as usize;
        let mut new = (*old).clone();
        new.resize(new_size, 0);
        let (bid, ino) = (self.bid(&b), self.ino(&b, &path));
        let res = self.st().setattr(bid, ino, SetAttr { size: Some(new_size as u64), ..Default::default() });
        self.record(&b, fid, &old, &new);
        assert_eq!(res?.size, new_size as u64);
        self.model.get_mut(&b).unwrap().files.get_mut(&path).unwrap().data = Arc::new(new);
        Ok(())
    }

    fn op_rename_file(&mut self) -> OpResult {
        let Some((b, path)) = self.pick_file() else { return Ok(()) };
        let dir = self.pick_dir(&b);
        let existing: Vec<String> = self.model[&b].files.keys().filter(|p| split(p).0 == dir).cloned().collect();
        let target = if !existing.is_empty() && self.rng.chance(1, 3) {
            self.pick(&existing).unwrap()
        } else {
            let name = self.fresh("f");
            format!("{dir}/{name}")
        };
        let (sp, sn) = split(&path);
        let (tp, tn) = split(&target);
        let (bid, spi, tpi) = (self.bid(&b), self.ino(&b, sp), self.ino(&b, tp));
        if let Some(replaced) = self.st().rename(bid, spi, sn.as_bytes(), tpi, tn.as_bytes())? {
            self.open_orphans.push((bid, replaced));
        }
        if path != target {
            let mb = self.model.get_mut(&b).unwrap();
            let f = mb.files.remove(&path).unwrap();
            mb.files.insert(target, f);
        }
        Ok(())
    }

    fn op_rename_dir(&mut self) -> OpResult {
        let Some(b) = self.pick_branch() else { return Ok(()) };
        let src = self.pick_dir(&b);
        if src.is_empty() {
            return Ok(());
        }
        let dst_parent = self.pick_dir(&b);
        let inside = dst_parent == src || dst_parent.starts_with(&format!("{src}/"));
        let name = self.fresh("d");
        let dst = format!("{dst_parent}/{name}");
        let (sp, sn) = split(&src);
        let (bid, spi, tpi) = (self.bid(&b), self.ino(&b, sp), self.ino(&b, &dst_parent));
        let res = self.st().rename(bid, spi, sn.as_bytes(), tpi, name.as_bytes());
        if inside {
            assert!(matches!(res, Err(Error::Invalid)), "seed {}: cyclic rename must be rejected", self.seed);
            return Ok(());
        }
        res?;
        let mb = self.model.get_mut(&b).unwrap();
        let prefix = format!("{src}/");
        let moved_dirs: Vec<String> = mb.dirs.iter().filter(|d| **d == src || d.starts_with(&prefix)).cloned().collect();
        for d in moved_dirs {
            mb.dirs.remove(&d);
            mb.dirs.insert(format!("{dst}{}", &d[src.len()..]));
        }
        let moved_files: Vec<String> = mb.files.keys().filter(|f| f.starts_with(&prefix)).cloned().collect();
        for f in moved_files {
            let v = mb.files.remove(&f).unwrap();
            mb.files.insert(format!("{dst}{}", &f[src.len()..]), v);
        }
        Ok(())
    }

    fn op_unlink(&mut self) -> OpResult {
        let Some((b, path)) = self.pick_file() else { return Ok(()) };
        let (p, n) = split(&path);
        let (bid, pi) = (self.bid(&b), self.ino(&b, p));
        let ino = self.st().unlink(bid, pi, n.as_bytes(), false)?;
        if self.rng.chance(1, 2) {
            // Half the time the "kernel" forgets the inode straight away.
            self.st().forget_orphan(bid, ino);
        } else {
            self.open_orphans.push((bid, ino));
        }
        self.model.get_mut(&b).unwrap().files.remove(&path);
        Ok(())
    }

    fn op_rmdir(&mut self) -> OpResult {
        let Some(b) = self.pick_branch() else { return Ok(()) };
        let dir = self.pick_dir(&b);
        if dir.is_empty() {
            return Ok(());
        }
        let prefix = format!("{dir}/");
        let mb = &self.model[&b];
        let empty = !mb.dirs.iter().any(|d| d.starts_with(&prefix)) && !mb.files.keys().any(|f| f.starts_with(&prefix));
        let (p, n) = split(&dir);
        let (bid, pi) = (self.bid(&b), self.ino(&b, p));
        let res = self.st().unlink(bid, pi, n.as_bytes(), true);
        if !empty {
            assert!(matches!(res, Err(Error::NotEmpty)), "seed {}: rmdir of non-empty dir", self.seed);
            return Ok(());
        }
        res?;
        self.model.get_mut(&b).unwrap().dirs.remove(&dir);
        Ok(())
    }

    fn op_read(&mut self) -> OpResult {
        let Some((b, path)) = self.pick_file() else { return Ok(()) };
        let data = self.model[&b].files[&path].data.clone();
        let offset = self.rng.below(data.len() as u64 + BS as u64);
        let len = self.rng.below(3 * BS as u64 + 1) as usize;
        let mut buf = vec![0xAAu8; len];
        let (bid, ino) = (self.bid(&b), self.ino(&b, &path));
        let n = self.st().read(bid, ino, offset, &mut buf)?;
        let want: &[u8] = if (offset as usize) < data.len() { &data[offset as usize..(offset as usize + len).min(data.len())] } else { &[] };
        assert!(buf[..n] == *want, "seed {} step {}: read mismatch {b}:{path} @{offset}+{len}", self.seed, self.step);
        Ok(())
    }

    fn op_sync(&mut self) -> OpResult {
        self.try_sync()?;
        self.mark_synced();
        Ok(())
    }

    fn op_gc(&mut self) -> OpResult {
        // Close every unlinked-but-open file so the space bound below holds.
        for (b, ino) in std::mem::take(&mut self.open_orphans) {
            self.st().forget_orphan(b, ino);
        }
        let report = self.st().gc().inspect_err(|_| self.sync_interrupted = true)?;
        self.mark_synced();
        // After a collection, live blocks can never exceed what a full copy
        // of every branch would need.
        let upper: u64 = self.model.values().flat_map(|b| b.files.values()).map(|f| f.data.len().div_ceil(BS) as u64).sum();
        assert!(report.live_blocks <= upper, "seed {}: gc reports {} live blocks, model bound {upper}", self.seed, report.live_blocks);
        Ok(())
    }

    fn try_sync(&mut self) -> OpResult {
        let res = self.st().sync();
        if res.is_err() {
            self.sync_interrupted = true;
        }
        res
    }

    fn mark_synced(&mut self) {
        self.synced = self.model.clone();
        self.pending.clear();
        self.sync_interrupted = false;
        self.hist.clear();
    }

    fn op_reopen(&mut self) -> OpResult {
        self.try_sync()?;
        self.mark_synced();
        // Injected failures stop here: the comparison itself must not fail.
        self.disk.fail_after(u64::MAX);
        self.store = None;
        self.open();
        self.verify();
        Ok(())
    }

    // ---- checking ---------------------------------------------------------

    fn snapshot_branch(&self, branch: &str) -> (BTreeSet<String>, BTreeMap<String, Vec<u8>>) {
        let store = self.store.as_ref().unwrap();
        let b = self.bid(branch);
        let mut dirs = BTreeSet::from([String::new()]);
        let mut files = BTreeMap::new();
        let mut stack = vec![(String::new(), ROOT_INO)];
        while let Some((path, ino)) = stack.pop() {
            for (name, child, kind) in store.readdir(b, ino).unwrap() {
                let p = format!("{path}/{}", String::from_utf8(name).unwrap());
                match kind {
                    Kind::Dir => {
                        dirs.insert(p.clone());
                        stack.push((p, child));
                    }
                    Kind::File => {
                        let size = store.getattr(b, child).unwrap().size as usize;
                        let mut data = vec![0u8; size];
                        let n = store.read(b, child, 0, &mut data).unwrap();
                        assert_eq!(n, size);
                        files.insert(p, data);
                    }
                    Kind::Symlink => unreachable!(),
                }
            }
        }
        (dirs, files)
    }

    /// Everything the store currently holds: per branch, its directories and files.
    fn snapshot(&self) -> Snapshot {
        let store = self.store.as_ref().unwrap();
        store.branches().into_iter().map(|b| (b.name.clone(), self.snapshot_branch(&b.name))).collect()
    }

    /// Compare names and sizes (not contents) with a model.
    fn structure_diff(have: &Snapshot, want: &Model) -> Option<String> {
        let (hb, wb): (Vec<&String>, Vec<&String>) = (have.keys().collect(), want.keys().collect());
        if hb != wb {
            return Some(format!("branches {hb:?} != {wb:?}"));
        }
        for (name, mb) in want {
            let (dirs, files) = &have[name];
            if *dirs != mb.dirs {
                return Some(format!("dirs in {name}: {dirs:?} != {:?}", mb.dirs));
            }
            let (hf, wf): (Vec<&String>, Vec<&String>) = (files.keys().collect(), mb.files.keys().collect());
            if hf != wf {
                return Some(format!("files in {name}: {hf:?} != {wf:?}"));
            }
            for (path, f) in &mb.files {
                if files[path].len() != f.data.len() {
                    return Some(format!("size of {name}:{path}: {} != {}", files[path].len(), f.data.len()));
                }
            }
        }
        None
    }

    /// The store must equal the model exactly.
    fn verify(&self) {
        let have = self.snapshot();
        if let Some(diff) = Self::structure_diff(&have, &self.model) {
            panic!("seed {} step {}: {diff}", self.seed, self.step);
        }
        for (name, mb) in &self.model {
            for (path, f) in &mb.files {
                assert!(have[name].1[path] == *f.data, "seed {} step {}: content of {name}:{path} differs", self.seed, self.step);
            }
        }
    }

    fn crash(&mut self) {
        self.crashes += 1;
        self.store = None;
        let mut rng = Rng(self.rng.next_u64());
        self.disk.crash(&mut rng);
        self.open();
        let have = self.snapshot();
        // Normally the durable state is exactly the last synced one. If the
        // crash hit in the middle of a sync, any prefix of the journal being
        // written may have survived, i.e. any later operation boundary.
        let mut candidates = vec![&self.synced];
        if self.sync_interrupted {
            candidates.extend(self.pending.iter());
        }
        let Some(base) = candidates.iter().find(|m| Self::structure_diff(&have, m).is_none()) else {
            panic!(
                "seed {} step {}: state after crash matches no valid point ({} candidates); versus last sync: {}",
                self.seed,
                self.step,
                candidates.len(),
                Self::structure_diff(&have, &self.synced).unwrap()
            );
        };
        let mut model = (*base).clone();
        for (name, mb) in model.iter_mut() {
            for (path, f) in mb.files.iter_mut() {
                let got = &have[name].1[path];
                for i in 0..got.len().div_ceil(BS) {
                    let key = (name.clone(), f.fid, i as u64);
                    let versions = self.hist.get(&key);
                    let start = i * BS;
                    for pos in start..(start + BS).min(got.len()) {
                        let byte = got[pos];
                        let ok = byte == f.data[pos] || versions.is_some_and(|vs| vs.iter().any(|v| v[pos - start] == byte));
                        assert!(
                            ok,
                            "seed {} step {}: after crash, {name}:{path} byte {pos} is {byte:#04x}, which was never written there",
                            self.seed, self.step
                        );
                    }
                }
                f.data = Arc::new(got.clone());
            }
        }
        self.model = model;
        self.mark_synced();
    }

    fn run(&mut self, steps: u64) {
        for step in 0..steps {
            self.step = step;
            let roll = self.rng.below(1000);
            let res = match roll {
                0..=44 => self.op_fork(),
                45..=59 => self.op_delete_branch(),
                60..=119 => self.op_mkdir(),
                120..=219 => self.op_create(),
                220..=519 => self.op_write(),
                520..=599 => self.op_truncate(),
                600..=649 => self.op_rename_file(),
                650..=679 => self.op_rename_dir(),
                680..=729 => self.op_unlink(),
                730..=749 => self.op_rmdir(),
                750..=849 => self.op_read(),
                850..=909 => self.op_sync(),
                910..=939 => self.op_gc(),
                940..=954 => self.op_reopen(),
                955..=984 => {
                    self.crash();
                    Ok(())
                }
                985..=989 => {
                    let n = self.rng.below(12);
                    self.disk.fail_after(n);
                    Ok(())
                }
                _ => {
                    self.disk.fail_after(u64::MAX);
                    self.verify();
                    Ok(())
                }
            };
            match res {
                Ok(()) => self.pending.push(self.model.clone()),
                // An injected disk failure: the only safe continuation is a restart.
                Err(Error::Io(_)) | Err(Error::Failed) => self.crash(),
                Err(e) => panic!("seed {} step {step}: unexpected error {e}", self.seed),
            }
        }
        // Finish with a clean restart and a full comparison.
        self.disk.fail_after(u64::MAX);
        if self.op_reopen().is_err() {
            self.crash();
        }
        self.verify();
    }
}

fn env(name: &str, default: u64) -> u64 {
    std::env::var(name).ok().and_then(|v| v.parse().ok()).unwrap_or(default)
}

#[test]
fn simulation() {
    let steps = env("BANYAN_DST_STEPS", 600);
    let seeds: Vec<u64> = match std::env::var("BANYAN_DST_SEED") {
        Ok(s) => vec![s.parse().expect("BANYAN_DST_SEED must be a number")],
        Err(_) => (0..env("BANYAN_DST_SEEDS", 150)).collect(),
    };
    let mut crashes = 0;
    for seed in &seeds {
        let mut sim = Sim::new(*seed);
        sim.run(steps);
        crashes += sim.crashes;
    }
    println!("dst: {} seeds x {steps} steps, {crashes} simulated crashes, all consistent", seeds.len());
}
