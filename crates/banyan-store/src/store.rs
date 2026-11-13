//! The branchable store: a set of independent file trees ("branches") that
//! share unmodified data.
//!
//! # How branching works
//!
//! Each branch owns a persistent inode table ([`Trie`]) and each file owns a
//! persistent block map. Forking a branch clones the inode table root, which
//! is one reference-count increment, so it costs the same for a 1 MB database
//! and a 1 TB one. The first write to a shared block copies the handful of
//! tree nodes on the path to it and allocates a fresh data slot.
//!
//! Lookups never walk a chain of ancestors: a branch that is 1,000 forks deep
//! resolves a block in the same number of steps as the root branch.
//!
//! # Overwrite in place
//!
//! Every fork gives both the parent and the child a new *epoch*. A data slot
//! remembers the epoch that allocated it. A branch may overwrite a slot in
//! place only if the slot's epoch equals the branch's current epoch, which is
//! true exactly when no other branch can see that slot. Hot blocks (such as
//! the Postgres WAL) are therefore rewritten in place and do not generate
//! garbage or journal traffic.
//!
//! # Durability
//!
//! Metadata changes are appended to an in-memory journal buffer. A sync first
//! makes the data area durable and only then appends and syncs the journal,
//! so a journal record never becomes durable before the data it points to.
//! Slots are only recycled by [`Store::gc`], which syncs first, so a slot the
//! durable journal still references is never overwritten with unrelated data.
//! After a crash the store is rebuilt by replaying the journal; an incomplete
//! record at the tail is discarded.

use std::collections::{BTreeMap, HashMap, HashSet};
use std::io::Read;
use std::num::NonZeroU64;
use std::sync::Arc;

use serde::{Deserialize, Serialize};

use crate::disk::Disk;
use crate::trie::Trie;
use crate::BLOCK_SIZE;

pub type BranchId = u32;
/// Inode number of every branch's root directory.
pub const ROOT_INO: u64 = 1;

const BS: u64 = BLOCK_SIZE as u64;
const MAX_RECORD: u32 = 1 << 20;
const MAX_NAME: usize = 255;

#[derive(Debug, thiserror::Error)]
pub enum Error {
    #[error("no such file or directory")]
    NotFound,
    #[error("file exists")]
    Exists,
    #[error("not a directory")]
    NotDir,
    #[error("is a directory")]
    IsDir,
    #[error("directory not empty")]
    NotEmpty,
    #[error("invalid argument")]
    Invalid,
    #[error("no such branch")]
    NoBranch,
    #[error("branch already exists")]
    BranchExists,
    #[error("invalid branch name (use 1-63 of A-Z a-z 0-9 . _ -, not starting with a dot)")]
    BadBranchName,
    #[error("I/O error: {0}")]
    Io(#[from] std::io::Error),
    #[error("corrupt journal: {0}")]
    Corrupt(String),
    #[error("store is read-only after an earlier I/O failure; restart to recover")]
    Failed,
}

pub type Result<T> = std::result::Result<T, Error>;

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub enum Kind {
    File,
    Dir,
    Symlink,
}

/// File attributes. Times are nanoseconds since the Unix epoch.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Attr {
    pub ino: u64,
    pub kind: Kind,
    pub mode: u32,
    pub uid: u32,
    pub gid: u32,
    pub size: u64,
    pub mtime: u64,
    pub ctime: u64,
}

#[derive(Clone, Debug, Default)]
pub struct SetAttr {
    pub mode: Option<u32>,
    pub uid: Option<u32>,
    pub gid: Option<u32>,
    pub size: Option<u64>,
    pub mtime: Option<u64>,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct BranchInfo {
    pub id: BranchId,
    pub name: String,
    pub parent: Option<String>,
    /// Nanoseconds since the Unix epoch.
    pub created: u64,
}

#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct Stats {
    pub branches: usize,
    /// Slots ever allocated in the data area (its high-water mark).
    pub allocated_blocks: u64,
    /// Slots known to be reusable. Exact right after [`Store::gc`].
    pub free_blocks: u64,
    pub journal_bytes: u64,
    pub block_size: u64,
}

#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct GcReport {
    pub live_blocks: u64,
    pub free_blocks: u64,
    pub newly_freed: u64,
}

#[derive(Clone)]
struct DirEnt {
    ino: u64,
    kind: Kind,
}

#[derive(Clone)]
struct Inode {
    kind: Kind,
    mode: u32,
    uid: u32,
    gid: u32,
    size: u64,
    mtime: u64,
    ctime: u64,
    /// Parent directory (directories only; used to reject cyclic renames).
    parent: u64,
    /// Block index -> slot + 1.
    blocks: Trie<NonZeroU64>,
    entries: Arc<BTreeMap<Vec<u8>, DirEnt>>,
    target: Arc<Vec<u8>>,
}

impl Inode {
    fn attr(&self, ino: u64) -> Attr {
        Attr {
            ino,
            kind: self.kind,
            mode: self.mode,
            uid: self.uid,
            gid: self.gid,
            size: match self.kind {
                Kind::Symlink => self.target.len() as u64,
                Kind::Dir => BS,
                Kind::File => self.size,
            },
            mtime: self.mtime,
            ctime: self.ctime,
        }
    }
}

struct Branch {
    name: String,
    parent: Option<String>,
    created: u64,
    epoch: u64,
    inodes: Trie<Arc<Inode>>,
    next_ino: u64,
    /// Unlinked inodes kept alive because something may still hold them open.
    orphans: HashSet<u64>,
}

/// One metadata change. The journal is a sequence of these.
#[derive(Serialize, Deserialize, Debug, Clone)]
enum Rec {
    Fork {
        id: BranchId,
        parent: Option<BranchId>,
        name: String,
        uid: u32,
        gid: u32,
        time: u64,
    },
    DeleteBranch {
        id: BranchId,
    },
    Create {
        b: BranchId,
        parent: u64,
        name: Vec<u8>,
        ino: u64,
        kind: Kind,
        mode: u32,
        uid: u32,
        gid: u32,
        target: Vec<u8>,
        time: u64,
    },
    Unlink {
        b: BranchId,
        parent: u64,
        name: Vec<u8>,
        time: u64,
    },
    Rename {
        b: BranchId,
        parent: u64,
        name: Vec<u8>,
        new_parent: u64,
        new_name: Vec<u8>,
        time: u64,
    },
    SetAttr {
        b: BranchId,
        ino: u64,
        mode: Option<u32>,
        uid: Option<u32>,
        gid: Option<u32>,
        size: Option<u64>,
        mtime: Option<u64>,
        time: u64,
    },
    Map {
        b: BranchId,
        ino: u64,
        block: u64,
        slot: u64,
    },
    /// One `write` call: the blocks it moved to new slots and the new file
    /// size, as a single record so a crash keeps all of it or none of it.
    Write {
        b: BranchId,
        ino: u64,
        maps: Vec<(u64, u64)>,
        size: Option<u64>,
        time: u64,
    },
}

/// Journal bytes handed out by [`Store::begin_sync`] to be written without
/// holding the store lock.
pub struct SyncBatch {
    pub(crate) bytes: Vec<u8>,
    pub(crate) gen: u64,
}

/// Write one batch durably: data first, then the journal records that refer
/// to it.
pub(crate) fn write_batch(disk: &dyn Disk, batch: &SyncBatch) -> std::io::Result<()> {
    disk.sync_data()?;
    if !batch.bytes.is_empty() {
        disk.journal_append(&batch.bytes)?;
        disk.journal_sync()?;
    }
    Ok(())
}

pub struct Store {
    disk: Arc<dyn Disk>,
    branches: BTreeMap<BranchId, Branch>,
    names: HashMap<String, BranchId>,
    next_branch_id: BranchId,
    next_epoch: u64,
    next_slot: u64,
    /// Reusable slots, highest first so `pop` hands out the lowest.
    free: Vec<u64>,
    /// Epoch that allocated each slot.
    slot_epoch: Vec<u64>,
    jbuf: Vec<u8>,
    journal_bytes: u64,
    /// Bumped on every change that a sync must persist.
    write_gen: u64,
    synced_gen: u64,
    failed: bool,
    clock: Box<dyn Fn() -> u64 + Send>,
}

fn system_clock() -> u64 {
    std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).map(|d| d.as_nanos() as u64).unwrap_or(0)
}

fn corrupt(what: &str) -> Error {
    Error::Corrupt(what.to_string())
}

fn valid_branch_name(name: &str) -> bool {
    !name.is_empty()
        && name.len() <= 63
        && !name.starts_with('.')
        && name.bytes().all(|c| c.is_ascii_alphanumeric() || matches!(c, b'.' | b'_' | b'-'))
}

fn valid_name(name: &[u8]) -> bool {
    !name.is_empty() && name.len() <= MAX_NAME && name != b"." && name != b".." && !name.contains(&b'/') && !name.contains(&0)
}

fn read_full(r: &mut dyn Read, buf: &mut [u8]) -> std::io::Result<usize> {
    let mut done = 0;
    while done < buf.len() {
        match r.read(&mut buf[done..]) {
            Ok(0) => break,
            Ok(n) => done += n,
            Err(e) if e.kind() == std::io::ErrorKind::Interrupted => {}
            Err(e) => return Err(e),
        }
    }
    Ok(done)
}

impl Store {
    /// Open a volume, replaying its journal. A new volume has no branches;
    /// create the first one with `fork(name, None, ..)`.
    pub fn open(disk: Arc<dyn Disk>) -> Result<Store> {
        Self::open_with_clock(disk, Box::new(system_clock))
    }

    /// Like [`Store::open`] with an injected clock (for deterministic tests).
    pub fn open_with_clock(disk: Arc<dyn Disk>, clock: Box<dyn Fn() -> u64 + Send>) -> Result<Store> {
        let mut s = Store {
            disk: disk.clone(),
            branches: BTreeMap::new(),
            names: HashMap::new(),
            next_branch_id: 0,
            next_epoch: 1,
            next_slot: 0,
            free: Vec::new(),
            slot_epoch: Vec::new(),
            jbuf: Vec::new(),
            journal_bytes: 0,
            write_gen: 0,
            synced_gen: 0,
            failed: false,
            clock,
        };
        let mut valid = 0u64;
        {
            let mut r = disk.journal_reader()?;
            let mut body = Vec::new();
            loop {
                let mut hdr = [0u8; 8];
                if read_full(&mut *r, &mut hdr)? != hdr.len() {
                    break;
                }
                let len = u32::from_le_bytes(hdr[..4].try_into().unwrap());
                let crc = u32::from_le_bytes(hdr[4..].try_into().unwrap());
                if len == 0 || len > MAX_RECORD {
                    break;
                }
                body.resize(len as usize, 0);
                if read_full(&mut *r, &mut body)? != body.len() || crc32fast::hash(&body) != crc {
                    break;
                }
                let Ok(rec) = bincode::deserialize::<Rec>(&body) else { break };
                s.apply(&rec)?;
                valid += 8 + len as u64;
            }
        }
        if disk.journal_len()? != valid {
            // A crash left an incomplete record at the tail. Cut it off so
            // new records are appended after the last complete one.
            disk.journal_truncate(valid)?;
        }
        s.journal_bytes = valid;
        // Nothing can hold an unlinked file open across a restart.
        for br in s.branches.values_mut() {
            for ino in std::mem::take(&mut br.orphans) {
                br.inodes.remove(ino);
            }
        }
        s.rebuild_free(&s.mark());
        Ok(s)
    }

    fn now(&self) -> u64 {
        (self.clock)()
    }

    fn check(&self) -> Result<()> {
        if self.failed {
            Err(Error::Failed)
        } else {
            Ok(())
        }
    }

    fn branch(&self, b: BranchId) -> Result<&Branch> {
        self.branches.get(&b).ok_or(Error::NoBranch)
    }

    fn inode(&self, b: BranchId, ino: u64) -> Result<&Inode> {
        self.branch(b)?.inodes.get(ino).map(|a| &**a).ok_or(Error::NotFound)
    }

    fn dir(&self, b: BranchId, ino: u64) -> Result<&Inode> {
        let i = self.inode(b, ino)?;
        if i.kind != Kind::Dir {
            return Err(Error::NotDir);
        }
        Ok(i)
    }

    // ---- journal ----------------------------------------------------------

    fn commit(&mut self, rec: Rec) -> Result<()> {
        self.apply(&rec)?;
        self.journal(&rec);
        Ok(())
    }

    /// Append a record whose effect is already in memory.
    fn journal(&mut self, rec: &Rec) {
        let body = bincode::serialize(rec).expect("record serializes");
        self.jbuf.extend_from_slice(&(body.len() as u32).to_le_bytes());
        self.jbuf.extend_from_slice(&crc32fast::hash(&body).to_le_bytes());
        self.jbuf.extend_from_slice(&body);
        self.journal_bytes += 8 + body.len() as u64;
        self.write_gen += 1;
    }

    fn map_block(&mut self, b: BranchId, ino: u64, block: u64, slot: u64) -> Result<()> {
        let br = self.branches.get_mut(&b).ok_or_else(|| corrupt("missing branch"))?;
        let epoch = br.epoch;
        let i = br.inodes.get_mut(ino).map(Arc::make_mut).ok_or_else(|| corrupt("missing inode"))?;
        i.blocks.insert(block, NonZeroU64::new(slot + 1).unwrap());
        let idx = slot as usize;
        if self.slot_epoch.len() <= idx {
            self.slot_epoch.resize(idx + 1, 0);
        }
        self.slot_epoch[idx] = epoch;
        self.next_slot = self.next_slot.max(slot + 1);
        Ok(())
    }

    fn set_size(&mut self, b: BranchId, ino: u64, size: u64, time: u64) -> Result<()> {
        let br = self.branches.get_mut(&b).ok_or_else(|| corrupt("missing branch"))?;
        let i = br.inodes.get_mut(ino).map(Arc::make_mut).ok_or_else(|| corrupt("missing inode"))?;
        i.size = size;
        i.mtime = time;
        Ok(())
    }

    /// Apply one record to the in-memory state. Used both for live
    /// operations (after validation) and for journal replay, so the two can
    /// never disagree.
    fn apply(&mut self, rec: &Rec) -> Result<()> {
        fn inode_mut(br: &mut Branch, ino: u64) -> Result<&mut Inode> {
            br.inodes.get_mut(ino).map(Arc::make_mut).ok_or_else(|| corrupt("missing inode"))
        }
        fn branch_mut(branches: &mut BTreeMap<BranchId, Branch>, b: BranchId) -> Result<&mut Branch> {
            branches.get_mut(&b).ok_or_else(|| corrupt("missing branch"))
        }
        match rec {
            Rec::Fork { id, parent, name, uid, gid, time } => {
                if self.branches.contains_key(id) || self.names.contains_key(name) {
                    return Err(corrupt("duplicate branch"));
                }
                let (inodes, next_ino, parent_name) = match parent {
                    Some(p) => {
                        let pb = branch_mut(&mut self.branches, *p)?;
                        // Both sides of a fork get a new epoch: neither may
                        // overwrite blocks the other can still see.
                        pb.epoch = self.next_epoch;
                        self.next_epoch += 1;
                        let mut inodes = pb.inodes.clone();
                        for o in &pb.orphans {
                            inodes.remove(*o);
                        }
                        (inodes, pb.next_ino, Some(pb.name.clone()))
                    }
                    None => {
                        let mut t = Trie::new();
                        t.insert(
                            ROOT_INO,
                            Arc::new(Inode {
                                kind: Kind::Dir,
                                mode: 0o755,
                                uid: *uid,
                                gid: *gid,
                                size: 0,
                                mtime: *time,
                                ctime: *time,
                                parent: ROOT_INO,
                                blocks: Trie::new(),
                                entries: Arc::new(BTreeMap::new()),
                                target: Arc::new(Vec::new()),
                            }),
                        );
                        (t, ROOT_INO + 1, None)
                    }
                };
                let epoch = self.next_epoch;
                self.next_epoch += 1;
                self.branches.insert(
                    *id,
                    Branch { name: name.clone(), parent: parent_name, created: *time, epoch, inodes, next_ino, orphans: HashSet::new() },
                );
                self.names.insert(name.clone(), *id);
                self.next_branch_id = self.next_branch_id.max(id + 1);
            }
            Rec::DeleteBranch { id } => {
                let br = self.branches.remove(id).ok_or_else(|| corrupt("missing branch"))?;
                self.names.remove(&br.name);
            }
            Rec::Create { b, parent, name, ino, kind, mode, uid, gid, target, time } => {
                let br = branch_mut(&mut self.branches, *b)?;
                let dir = inode_mut(br, *parent)?;
                Arc::make_mut(&mut dir.entries).insert(name.clone(), DirEnt { ino: *ino, kind: *kind });
                dir.mtime = *time;
                dir.ctime = *time;
                br.inodes.insert(
                    *ino,
                    Arc::new(Inode {
                        kind: *kind,
                        mode: *mode,
                        uid: *uid,
                        gid: *gid,
                        size: 0,
                        mtime: *time,
                        ctime: *time,
                        parent: *parent,
                        blocks: Trie::new(),
                        entries: Arc::new(BTreeMap::new()),
                        target: Arc::new(target.clone()),
                    }),
                );
                br.next_ino = br.next_ino.max(ino + 1);
            }
            Rec::Unlink { b, parent, name, time } => {
                let br = branch_mut(&mut self.branches, *b)?;
                let dir = inode_mut(br, *parent)?;
                let ent = Arc::make_mut(&mut dir.entries).remove(name).ok_or_else(|| corrupt("unlink of missing name"))?;
                dir.mtime = *time;
                dir.ctime = *time;
                br.orphans.insert(ent.ino);
            }
            Rec::Rename { b, parent, name, new_parent, new_name, time } => {
                let br = branch_mut(&mut self.branches, *b)?;
                let src = inode_mut(br, *parent)?;
                let ent = Arc::make_mut(&mut src.entries).remove(name).ok_or_else(|| corrupt("rename of missing name"))?;
                src.mtime = *time;
                src.ctime = *time;
                let dst = inode_mut(br, *new_parent)?;
                let replaced = Arc::make_mut(&mut dst.entries).insert(new_name.clone(), ent.clone());
                dst.mtime = *time;
                dst.ctime = *time;
                if let Some(old) = replaced {
                    br.orphans.insert(old.ino);
                }
                let moved = inode_mut(br, ent.ino)?;
                moved.ctime = *time;
                if moved.kind == Kind::Dir {
                    moved.parent = *new_parent;
                }
            }
            Rec::SetAttr { b, ino, mode, uid, gid, size, mtime, time } => {
                let i = inode_mut(branch_mut(&mut self.branches, *b)?, *ino)?;
                if let Some(m) = mode {
                    i.mode = *m;
                }
                if let Some(u) = uid {
                    i.uid = *u;
                }
                if let Some(g) = gid {
                    i.gid = *g;
                }
                if let Some(sz) = size {
                    if *sz < i.size {
                        i.blocks.remove_from(sz.div_ceil(BS));
                    }
                    i.size = *sz;
                    i.mtime = *time;
                }
                if let Some(t) = mtime {
                    i.mtime = *t;
                }
                i.ctime = *time;
            }
            Rec::Map { b, ino, block, slot } => self.map_block(*b, *ino, *block, *slot)?,
            Rec::Write { b, ino, maps, size, time } => {
                for (block, slot) in maps {
                    self.map_block(*b, *ino, *block, *slot)?;
                }
                if let Some(size) = size {
                    self.set_size(*b, *ino, *size, *time)?;
                }
            }
        }
        Ok(())
    }

    // ---- branches ---------------------------------------------------------

    /// Create a branch. With `from = Some(parent)` the new branch starts as
    /// an exact copy of the parent at this instant, in O(1). With `None` it
    /// starts as an empty tree whose root is owned by `uid:gid`.
    pub fn fork(&mut self, name: &str, from: Option<&str>, uid: u32, gid: u32) -> Result<BranchId> {
        self.check()?;
        if !valid_branch_name(name) {
            return Err(Error::BadBranchName);
        }
        if self.names.contains_key(name) {
            return Err(Error::BranchExists);
        }
        let parent = match from {
            Some(p) => Some(*self.names.get(p).ok_or(Error::NoBranch)?),
            None => None,
        };
        let id = self.next_branch_id;
        let time = self.now();
        self.commit(Rec::Fork { id, parent, name: name.to_string(), uid, gid, time })?;
        Ok(id)
    }

    /// Delete a branch. Other branches are unaffected, including branches
    /// that were forked from it. Space is reclaimed by the next [`Store::gc`].
    pub fn delete_branch(&mut self, name: &str) -> Result<()> {
        self.check()?;
        let id = *self.names.get(name).ok_or(Error::NoBranch)?;
        self.commit(Rec::DeleteBranch { id })
    }

    pub fn branch_id(&self, name: &str) -> Option<BranchId> {
        self.names.get(name).copied()
    }

    pub fn branch_name(&self, id: BranchId) -> Option<&str> {
        self.branches.get(&id).map(|b| b.name.as_str())
    }

    pub fn branches(&self) -> Vec<BranchInfo> {
        self.branches.iter().map(|(id, b)| BranchInfo { id: *id, name: b.name.clone(), parent: b.parent.clone(), created: b.created }).collect()
    }

    // ---- namespace --------------------------------------------------------

    pub fn getattr(&self, b: BranchId, ino: u64) -> Result<Attr> {
        Ok(self.inode(b, ino)?.attr(ino))
    }

    pub fn lookup(&self, b: BranchId, parent: u64, name: &[u8]) -> Result<Attr> {
        let ent = self.dir(b, parent)?.entries.get(name).ok_or(Error::NotFound)?;
        self.getattr(b, ent.ino)
    }

    /// Resolve a `/`-separated path relative to the branch root.
    pub fn resolve(&self, b: BranchId, path: &str) -> Result<Attr> {
        let mut ino = ROOT_INO;
        for part in path.split('/').filter(|p| !p.is_empty()) {
            ino = self.lookup(b, ino, part.as_bytes())?.ino;
        }
        self.getattr(b, ino)
    }

    pub fn readdir(&self, b: BranchId, ino: u64) -> Result<Vec<(Vec<u8>, u64, Kind)>> {
        Ok(self.dir(b, ino)?.entries.iter().map(|(n, e)| (n.clone(), e.ino, e.kind)).collect())
    }

    pub fn readlink(&self, b: BranchId, ino: u64) -> Result<Vec<u8>> {
        let i = self.inode(b, ino)?;
        if i.kind != Kind::Symlink {
            return Err(Error::Invalid);
        }
        Ok((*i.target).clone())
    }

    #[allow(clippy::too_many_arguments)]
    pub fn create(&mut self, b: BranchId, parent: u64, name: &[u8], kind: Kind, mode: u32, uid: u32, gid: u32, target: &[u8]) -> Result<Attr> {
        self.check()?;
        if !valid_name(name) {
            return Err(Error::Invalid);
        }
        if self.dir(b, parent)?.entries.contains_key(name) {
            return Err(Error::Exists);
        }
        let ino = self.branch(b)?.next_ino;
        let time = self.now();
        self.commit(Rec::Create { b, parent, name: name.to_vec(), ino, kind, mode, uid, gid, target: target.to_vec(), time })?;
        self.getattr(b, ino)
    }

    /// Remove a name. `dir` selects rmdir semantics. Returns the inode, which
    /// stays readable and writable until [`Store::forget_orphan`].
    pub fn unlink(&mut self, b: BranchId, parent: u64, name: &[u8], dir: bool) -> Result<u64> {
        self.check()?;
        let ent = self.dir(b, parent)?.entries.get(name).ok_or(Error::NotFound)?.clone();
        match (dir, ent.kind) {
            (true, Kind::Dir) => {
                if !self.inode(b, ent.ino)?.entries.is_empty() {
                    return Err(Error::NotEmpty);
                }
            }
            (true, _) => return Err(Error::NotDir),
            (false, Kind::Dir) => return Err(Error::IsDir),
            (false, _) => {}
        }
        let time = self.now();
        self.commit(Rec::Unlink { b, parent, name: name.to_vec(), time })?;
        Ok(ent.ino)
    }

    /// Rename, replacing the destination if it exists. Returns the inode
    /// that was replaced, if any.
    pub fn rename(&mut self, b: BranchId, parent: u64, name: &[u8], new_parent: u64, new_name: &[u8]) -> Result<Option<u64>> {
        self.check()?;
        if !valid_name(new_name) {
            return Err(Error::Invalid);
        }
        let ent = self.dir(b, parent)?.entries.get(name).ok_or(Error::NotFound)?.clone();
        let existing = self.dir(b, new_parent)?.entries.get(new_name).cloned();
        if let Some(old) = &existing {
            if old.ino == ent.ino {
                return Ok(None);
            }
            match (ent.kind, old.kind) {
                (Kind::Dir, Kind::Dir) => {
                    if !self.inode(b, old.ino)?.entries.is_empty() {
                        return Err(Error::NotEmpty);
                    }
                }
                (Kind::Dir, _) => return Err(Error::NotDir),
                (_, Kind::Dir) => return Err(Error::IsDir),
                _ => {}
            }
        }
        if ent.kind == Kind::Dir {
            // A directory must not be moved into itself or a descendant.
            let mut cur = new_parent;
            loop {
                if cur == ent.ino {
                    return Err(Error::Invalid);
                }
                if cur == ROOT_INO {
                    break;
                }
                cur = self.inode(b, cur)?.parent;
            }
        }
        let time = self.now();
        self.commit(Rec::Rename { b, parent, name: name.to_vec(), new_parent, new_name: new_name.to_vec(), time })?;
        Ok(existing.map(|e| e.ino))
    }

    /// Drop an unlinked inode once nothing can reference it any more.
    pub fn forget_orphan(&mut self, b: BranchId, ino: u64) {
        if let Some(br) = self.branches.get_mut(&b) {
            if br.orphans.remove(&ino) {
                br.inodes.remove(ino);
            }
        }
    }

    pub fn setattr(&mut self, b: BranchId, ino: u64, set: SetAttr) -> Result<Attr> {
        self.check()?;
        let cur = self.inode(b, ino)?;
        if let Some(size) = set.size {
            if cur.kind != Kind::File {
                return Err(if cur.kind == Kind::Dir { Error::IsDir } else { Error::Invalid });
            }
            let old = cur.size;
            if size > old {
                self.zero_stale_tail(b, ino, old)?;
            }
        }
        let time = self.now();
        self.commit(Rec::SetAttr { b, ino, mode: set.mode, uid: set.uid, gid: set.gid, size: set.size, mtime: set.mtime, time })?;
        self.getattr(b, ino)
    }

    // ---- data -------------------------------------------------------------

    fn slot_of(&self, b: BranchId, ino: u64, block: u64) -> Result<Option<u64>> {
        Ok(self.inode(b, ino)?.blocks.get(block).map(|s| s.get() - 1))
    }

    fn alloc(&mut self) -> u64 {
        self.free.pop().unwrap_or_else(|| {
            let s = self.next_slot;
            self.next_slot += 1;
            s
        })
    }

    /// Store one full block for `(b, ino, block)`: in place if this branch
    /// is the only one that can see the current slot, otherwise in a new slot.
    ///
    /// A new mapping is applied to memory at once. It is journaled here when
    /// `maps` is `None`; otherwise it is added to `maps` for the caller to
    /// journal together with the rest of its operation.
    fn put_block(&mut self, b: BranchId, ino: u64, block: u64, existing: Option<u64>, buf: &[u8], maps: Option<&mut Vec<(u64, u64)>>) -> Result<()> {
        let epoch = self.branch(b)?.epoch;
        match existing {
            Some(slot) if self.slot_epoch[slot as usize] == epoch => {
                self.disk.write_block(slot, buf)?;
                self.write_gen += 1;
            }
            _ => {
                let slot = self.alloc();
                if let Err(e) = self.disk.write_block(slot, buf) {
                    self.free.push(slot);
                    return Err(e.into());
                }
                self.map_block(b, ino, block, slot)?;
                match maps {
                    Some(m) => m.push((block, slot)),
                    None => self.journal(&Rec::Map { b, ino, block, slot }),
                }
            }
        }
        Ok(())
    }

    /// Bytes past end-of-file inside the last block are unspecified (left
    /// behind by a truncate, or by a size update lost in a crash). Before a
    /// file grows, clear them so the new range reads as zeros.
    fn zero_stale_tail(&mut self, b: BranchId, ino: u64, old_size: u64) -> Result<()> {
        let in_off = (old_size % BS) as usize;
        if in_off == 0 {
            return Ok(());
        }
        let block = old_size / BS;
        let Some(slot) = self.slot_of(b, ino, block)? else { return Ok(()) };
        let mut buf = [0u8; BLOCK_SIZE];
        self.disk.read_block(slot, &mut buf)?;
        if buf[in_off..].iter().all(|x| *x == 0) {
            return Ok(());
        }
        buf[in_off..].fill(0);
        self.put_block(b, ino, block, Some(slot), &buf, None)
    }

    pub fn read(&self, b: BranchId, ino: u64, offset: u64, buf: &mut [u8]) -> Result<usize> {
        let i = self.inode(b, ino)?;
        match i.kind {
            Kind::File => {}
            Kind::Dir => return Err(Error::IsDir),
            Kind::Symlink => return Err(Error::Invalid),
        }
        if offset >= i.size {
            return Ok(0);
        }
        let len = (buf.len() as u64).min(i.size - offset) as usize;
        let mut done = 0;
        let mut tmp = [0u8; BLOCK_SIZE];
        while done < len {
            let pos = offset + done as u64;
            let in_off = (pos % BS) as usize;
            let n = (BLOCK_SIZE - in_off).min(len - done);
            let out = &mut buf[done..done + n];
            match i.blocks.get(pos / BS) {
                Some(slot) if n == BLOCK_SIZE => self.disk.read_block(slot.get() - 1, out)?,
                Some(slot) => {
                    self.disk.read_block(slot.get() - 1, &mut tmp)?;
                    out.copy_from_slice(&tmp[in_off..in_off + n]);
                }
                None => out.fill(0),
            }
            done += n;
        }
        Ok(len)
    }

    pub fn write(&mut self, b: BranchId, ino: u64, offset: u64, data: &[u8]) -> Result<usize> {
        self.check()?;
        let i = self.inode(b, ino)?;
        match i.kind {
            Kind::File => {}
            Kind::Dir => return Err(Error::IsDir),
            Kind::Symlink => return Err(Error::Invalid),
        }
        let old_size = i.size;
        let end = offset.checked_add(data.len() as u64).ok_or(Error::Invalid)?;
        if data.is_empty() {
            return Ok(0);
        }
        if end > old_size {
            self.zero_stale_tail(b, ino, old_size)?;
        }
        let mut maps = Vec::new();
        let mut done = 0;
        let res = self.write_blocks(b, ino, offset, data, &mut maps, &mut done);
        // Journal what actually happened, even if a block write failed part
        // way through, so the journal always agrees with memory.
        let reached = offset + done as u64;
        let size = (reached > old_size).then_some(reached);
        if !maps.is_empty() || size.is_some() {
            let time = self.now();
            if let Some(size) = size {
                self.set_size(b, ino, size, time)?;
            }
            self.journal(&Rec::Write { b, ino, maps, size, time });
        }
        res?;
        Ok(data.len())
    }

    fn write_blocks(&mut self, b: BranchId, ino: u64, offset: u64, data: &[u8], maps: &mut Vec<(u64, u64)>, done: &mut usize) -> Result<()> {
        let mut buf = [0u8; BLOCK_SIZE];
        while *done < data.len() {
            let pos = offset + *done as u64;
            let block = pos / BS;
            let in_off = (pos % BS) as usize;
            let n = (BLOCK_SIZE - in_off).min(data.len() - *done);
            let existing = self.slot_of(b, ino, block)?;
            let chunk = &data[*done..*done + n];
            if n == BLOCK_SIZE {
                self.put_block(b, ino, block, existing, chunk, Some(maps))?;
            } else {
                match existing {
                    Some(slot) => self.disk.read_block(slot, &mut buf)?,
                    None => buf.fill(0),
                }
                buf[in_off..in_off + n].copy_from_slice(chunk);
                self.put_block(b, ino, block, existing, &buf, Some(maps))?;
            }
            *done += n;
        }
        Ok(())
    }

    // ---- durability -------------------------------------------------------

    /// Token for "everything written so far". See [`Store::is_synced`].
    pub fn write_mark(&self) -> u64 {
        self.write_gen
    }

    pub fn is_synced(&self, mark: u64) -> bool {
        self.synced_gen >= mark
    }

    /// Take the pending journal bytes so they can be written without holding
    /// the store lock. Returns `None` when there is nothing to sync. Only one
    /// batch may be in flight at a time; see [`crate::SharedStore`].
    pub fn begin_sync(&mut self) -> Result<Option<SyncBatch>> {
        self.check()?;
        if self.synced_gen == self.write_gen {
            return Ok(None);
        }
        Ok(Some(SyncBatch { bytes: std::mem::take(&mut self.jbuf), gen: self.write_gen }))
    }

    pub fn finish_sync(&mut self, batch: &SyncBatch, ok: bool) {
        if ok {
            self.synced_gen = self.synced_gen.max(batch.gen);
        } else {
            // The journal bytes of this batch may be lost or half-written.
            // Refuse further changes; a restart recovers the durable state.
            self.failed = true;
        }
    }

    /// Make everything written so far durable.
    pub fn sync(&mut self) -> Result<()> {
        if let Some(batch) = self.begin_sync()? {
            let res = write_batch(&*self.disk, &batch);
            self.finish_sync(&batch, res.is_ok());
            res?;
        }
        Ok(())
    }

    // ---- space ------------------------------------------------------------

    /// Bitmap of slots reachable from any branch. Shared subtrees are walked
    /// once, so the cost follows the amount of stored data, not the number of
    /// branches.
    fn mark(&self) -> Vec<u64> {
        let mut live = vec![0u64; (self.next_slot as usize).div_ceil(64)];
        let mut seen_nodes = HashSet::new();
        let mut seen_inodes = HashSet::new();
        let mut seen_blocks = HashSet::new();
        for br in self.branches.values() {
            br.inodes.walk_unique(&mut seen_nodes, &mut |inode: &Arc<Inode>| {
                if !seen_inodes.insert(Arc::as_ptr(inode) as usize) {
                    return;
                }
                inode.blocks.walk_unique(&mut seen_blocks, &mut |slot: &NonZeroU64| {
                    let s = slot.get() - 1;
                    live[(s / 64) as usize] |= 1 << (s % 64);
                });
            });
        }
        live
    }

    fn rebuild_free(&mut self, live: &[u64]) -> u64 {
        self.free.clear();
        for s in (0..self.next_slot).rev() {
            if live[(s / 64) as usize] & (1 << (s % 64)) == 0 {
                self.free.push(s);
            }
        }
        self.next_slot - self.free.len() as u64
    }

    /// Find slots no branch references and make them reusable. Syncs first:
    /// a slot may only be recycled once the journal that stopped referencing
    /// it is durable.
    pub fn gc(&mut self) -> Result<GcReport> {
        self.sync()?;
        let before = self.free.len() as u64;
        let live_blocks = self.rebuild_free(&self.mark());
        let free_blocks = self.free.len() as u64;
        Ok(GcReport { live_blocks, free_blocks, newly_freed: free_blocks.saturating_sub(before) })
    }

    pub fn stats(&self) -> Stats {
        Stats {
            branches: self.branches.len(),
            allocated_blocks: self.next_slot,
            free_blocks: self.free.len() as u64,
            journal_bytes: self.journal_bytes,
            block_size: BS,
        }
    }
}
