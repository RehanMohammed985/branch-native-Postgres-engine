//! FUSE front end for a banyan volume.
//!
//! The mount point contains one directory per branch:
//!
//! ```text
//! /mnt/banyan/main/...      <- the branch called "main"
//! /mnt/banyan/feature-x/... <- a branch forked from it
//! ```
//!
//! Branches are created and deleted through the store API, not with `mkdir`;
//! a new branch shows up as a new directory without any mount call, which is
//! what keeps branch creation in the microsecond range.
//!
//! Writes go straight through to the store (no kernel write-back cache), so a
//! fork taken at any moment contains every write the kernel has acknowledged.
//! `fsync` is answered from a separate thread that batches all waiting
//! callers into one disk sync.

use std::collections::HashMap;
use std::ffi::OsStr;
use std::os::unix::ffi::OsStrExt;
use std::path::Path;
use std::sync::mpsc;
use std::sync::Arc;
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use banyan_store::{Attr, BranchId, Error, Kind, SetAttr, SharedStore, BLOCK_SIZE, ROOT_INO};
use fuser::{
    FileAttr, FileType, Filesystem, MountOption, ReplyAttr, ReplyCreate, ReplyData, ReplyDirectory, ReplyEmpty, ReplyEntry, ReplyOpen, ReplyStatfs,
    ReplyWrite, Request, TimeOrNow,
};

/// Kernel inode numbers pack `(branch id + 1, inode within branch)`.
const INO_BITS: u32 = 40;
const INO_MASK: u64 = (1 << INO_BITS) - 1;
const TOP: u64 = fuser::FUSE_ROOT_ID;

/// How long the kernel may cache names and attributes inside a branch. Safe
/// because every change inside a branch goes through this filesystem.
const TTL: Duration = Duration::from_secs(1);
/// Branch directories come and go behind the kernel's back: never cache them.
const TOP_TTL: Duration = Duration::ZERO;

fn fuse_ino(b: BranchId, ino: u64) -> u64 {
    ((b as u64 + 1) << INO_BITS) | ino
}

/// Split a kernel inode number into branch and branch-local inode.
fn local(ino: u64) -> Option<(BranchId, u64)> {
    let b = ino >> INO_BITS;
    if b == 0 {
        None
    } else {
        Some(((b - 1) as BranchId, ino & INO_MASK))
    }
}

fn errno(e: &Error) -> i32 {
    match e {
        Error::NotFound | Error::NoBranch => libc::ENOENT,
        Error::Exists | Error::BranchExists => libc::EEXIST,
        Error::NotDir => libc::ENOTDIR,
        Error::IsDir => libc::EISDIR,
        Error::NotEmpty => libc::ENOTEMPTY,
        Error::Invalid | Error::BadBranchName => libc::EINVAL,
        Error::Io(_) | Error::Corrupt(_) | Error::Failed => libc::EIO,
    }
}

fn time(ns: u64) -> SystemTime {
    UNIX_EPOCH + Duration::from_nanos(ns)
}

fn nanos(t: SystemTime) -> u64 {
    t.duration_since(UNIX_EPOCH).map(|d| d.as_nanos() as u64).unwrap_or(0)
}

fn file_type(kind: Kind) -> FileType {
    match kind {
        Kind::File => FileType::RegularFile,
        Kind::Dir => FileType::Directory,
        Kind::Symlink => FileType::Symlink,
    }
}

fn file_attr(b: BranchId, a: &Attr) -> FileAttr {
    FileAttr {
        ino: fuse_ino(b, a.ino),
        size: a.size,
        blocks: a.size.div_ceil(512),
        atime: time(a.mtime),
        mtime: time(a.mtime),
        ctime: time(a.ctime),
        crtime: time(a.ctime),
        kind: file_type(a.kind),
        perm: (a.mode & 0o7777) as u16,
        nlink: if a.kind == Kind::Dir { 2 } else { 1 },
        uid: a.uid,
        gid: a.gid,
        rdev: 0,
        blksize: BLOCK_SIZE as u32,
        flags: 0,
    }
}

type DirListing = Vec<(u64, FileType, Vec<u8>)>;

pub struct BanyanFs {
    store: Arc<SharedStore>,
    /// Kernel lookup counts, so unlinked files live until their last close.
    lookups: HashMap<u64, u64>,
    /// Directory listings captured at `opendir`, so a listing stays stable
    /// while entries are added or removed.
    dirs: HashMap<u64, DirListing>,
    next_fh: u64,
    sync_tx: mpsc::Sender<ReplyEmpty>,
    uid: u32,
    gid: u32,
    started: SystemTime,
}

impl BanyanFs {
    pub fn new(store: Arc<SharedStore>) -> Self {
        let (sync_tx, sync_rx) = mpsc::channel::<ReplyEmpty>();
        let syncer = store.clone();
        std::thread::Builder::new()
            .name("banyan-sync".into())
            .spawn(move || {
                // Group commit: everyone who asked for an fsync while the
                // previous one was running is answered by a single sync.
                while let Ok(first) = sync_rx.recv() {
                    let mut waiting = vec![first];
                    while let Ok(more) = sync_rx.try_recv() {
                        waiting.push(more);
                    }
                    match syncer.sync() {
                        Ok(()) => waiting.into_iter().for_each(|r| r.ok()),
                        Err(e) => {
                            log::error!("sync failed: {e}");
                            waiting.into_iter().for_each(|r| r.error(libc::EIO));
                        }
                    }
                }
            })
            .expect("spawn sync thread");
        BanyanFs {
            store,
            lookups: HashMap::new(),
            dirs: HashMap::new(),
            next_fh: 1,
            sync_tx,
            uid: unsafe { libc::getuid() },
            gid: unsafe { libc::getgid() },
            started: SystemTime::now(),
        }
    }

    fn top_attr(&self) -> FileAttr {
        FileAttr {
            ino: TOP,
            size: BLOCK_SIZE as u64,
            blocks: 0,
            atime: self.started,
            mtime: self.started,
            ctime: self.started,
            crtime: self.started,
            kind: FileType::Directory,
            perm: 0o755,
            nlink: 2,
            uid: self.uid,
            gid: self.gid,
            rdev: 0,
            blksize: BLOCK_SIZE as u32,
            flags: 0,
        }
    }

    fn remember(&mut self, ino: u64) {
        *self.lookups.entry(ino).or_insert(0) += 1;
    }

    fn entry(&mut self, b: BranchId, res: Result<Attr, Error>, reply: ReplyEntry) {
        match res {
            Ok(a) => {
                let attr = file_attr(b, &a);
                self.remember(attr.ino);
                reply.entry(&TTL, &attr, 0);
            }
            Err(e) => reply.error(errno(&e)),
        }
    }
}

fn empty(res: Result<(), Error>, reply: ReplyEmpty) {
    match res {
        Ok(()) => reply.ok(),
        Err(e) => reply.error(errno(&e)),
    }
}

impl Filesystem for BanyanFs {
    fn lookup(&mut self, _req: &Request<'_>, parent: u64, name: &OsStr, reply: ReplyEntry) {
        let Some((b, dir)) = local(parent) else {
            // Top level: names are branches.
            let store = self.store.lock();
            let found = name.to_str().and_then(|n| store.branch_id(n)).and_then(|b| store.getattr(b, ROOT_INO).ok().map(|a| (b, a)));
            drop(store);
            return match found {
                Some((b, a)) => {
                    let attr = file_attr(b, &a);
                    self.remember(attr.ino);
                    reply.entry(&TOP_TTL, &attr, 0)
                }
                None => reply.error(libc::ENOENT),
            };
        };
        let res = self.store.lock().lookup(b, dir, name.as_bytes());
        self.entry(b, res, reply);
    }

    fn forget(&mut self, _req: &Request<'_>, ino: u64, nlookup: u64) {
        if let Some(count) = self.lookups.get_mut(&ino) {
            *count = count.saturating_sub(nlookup);
            if *count == 0 {
                self.lookups.remove(&ino);
                if let Some((b, i)) = local(ino) {
                    self.store.lock().forget_orphan(b, i);
                }
            }
        }
    }

    fn getattr(&mut self, _req: &Request<'_>, ino: u64, _fh: Option<u64>, reply: ReplyAttr) {
        let Some((b, i)) = local(ino) else { return reply.attr(&TOP_TTL, &self.top_attr()) };
        match self.store.lock().getattr(b, i) {
            Ok(a) => reply.attr(&TTL, &file_attr(b, &a)),
            Err(e) => reply.error(errno(&e)),
        }
    }

    fn setattr(
        &mut self,
        _req: &Request<'_>,
        ino: u64,
        mode: Option<u32>,
        uid: Option<u32>,
        gid: Option<u32>,
        size: Option<u64>,
        _atime: Option<TimeOrNow>,
        mtime: Option<TimeOrNow>,
        _ctime: Option<SystemTime>,
        _fh: Option<u64>,
        _crtime: Option<SystemTime>,
        _chgtime: Option<SystemTime>,
        _bkuptime: Option<SystemTime>,
        _flags: Option<u32>,
        reply: ReplyAttr,
    ) {
        let Some((b, i)) = local(ino) else { return reply.error(libc::EPERM) };
        let mtime = mtime.map(|t| match t {
            TimeOrNow::SpecificTime(t) => nanos(t),
            TimeOrNow::Now => nanos(SystemTime::now()),
        });
        let mut store = self.store.lock();
        let res = if mode.is_none() && uid.is_none() && gid.is_none() && size.is_none() && mtime.is_none() {
            store.getattr(b, i)
        } else {
            store.setattr(b, i, SetAttr { mode: mode.map(|m| m & 0o7777), uid, gid, size, mtime })
        };
        drop(store);
        match res {
            Ok(a) => reply.attr(&TTL, &file_attr(b, &a)),
            Err(e) => reply.error(errno(&e)),
        }
    }

    fn readlink(&mut self, _req: &Request<'_>, ino: u64, reply: ReplyData) {
        let Some((b, i)) = local(ino) else { return reply.error(libc::EINVAL) };
        match self.store.lock().readlink(b, i) {
            Ok(t) => reply.data(&t),
            Err(e) => reply.error(errno(&e)),
        }
    }

    fn mknod(&mut self, req: &Request<'_>, parent: u64, name: &OsStr, mode: u32, umask: u32, _rdev: u32, reply: ReplyEntry) {
        let Some((b, dir)) = local(parent) else { return reply.error(libc::EPERM) };
        if mode & libc::S_IFMT != libc::S_IFREG {
            return reply.error(libc::EPERM);
        }
        let res = self.store.lock().create(b, dir, name.as_bytes(), Kind::File, mode & 0o7777 & !umask, req.uid(), req.gid(), b"");
        self.entry(b, res, reply);
    }

    fn mkdir(&mut self, req: &Request<'_>, parent: u64, name: &OsStr, mode: u32, umask: u32, reply: ReplyEntry) {
        let Some((b, dir)) = local(parent) else { return reply.error(libc::EPERM) };
        let res = self.store.lock().create(b, dir, name.as_bytes(), Kind::Dir, mode & 0o7777 & !umask, req.uid(), req.gid(), b"");
        self.entry(b, res, reply);
    }

    fn symlink(&mut self, req: &Request<'_>, parent: u64, link_name: &OsStr, target: &Path, reply: ReplyEntry) {
        let Some((b, dir)) = local(parent) else { return reply.error(libc::EPERM) };
        let res = self.store.lock().create(b, dir, link_name.as_bytes(), Kind::Symlink, 0o777, req.uid(), req.gid(), target.as_os_str().as_bytes());
        self.entry(b, res, reply);
    }

    fn create(&mut self, req: &Request<'_>, parent: u64, name: &OsStr, mode: u32, umask: u32, _flags: i32, reply: ReplyCreate) {
        let Some((b, dir)) = local(parent) else { return reply.error(libc::EPERM) };
        let res = self.store.lock().create(b, dir, name.as_bytes(), Kind::File, mode & 0o7777 & !umask, req.uid(), req.gid(), b"");
        match res {
            Ok(a) => {
                let attr = file_attr(b, &a);
                self.remember(attr.ino);
                reply.created(&TTL, &attr, 0, 0, 0);
            }
            Err(e) => reply.error(errno(&e)),
        }
    }

    fn unlink(&mut self, _req: &Request<'_>, parent: u64, name: &OsStr, reply: ReplyEmpty) {
        let Some((b, dir)) = local(parent) else { return reply.error(libc::EPERM) };
        let res = self.store.lock().unlink(b, dir, name.as_bytes(), false);
        if let Ok(ino) = res {
            // If the kernel never looked the file up, nothing will forget it.
            if !self.lookups.contains_key(&fuse_ino(b, ino)) {
                self.store.lock().forget_orphan(b, ino);
            }
        }
        empty(res.map(|_| ()), reply);
    }

    fn rmdir(&mut self, _req: &Request<'_>, parent: u64, name: &OsStr, reply: ReplyEmpty) {
        let Some((b, dir)) = local(parent) else { return reply.error(libc::EPERM) };
        let res = self.store.lock().unlink(b, dir, name.as_bytes(), true);
        if let Ok(ino) = res {
            if !self.lookups.contains_key(&fuse_ino(b, ino)) {
                self.store.lock().forget_orphan(b, ino);
            }
        }
        empty(res.map(|_| ()), reply);
    }

    fn rename(&mut self, _req: &Request<'_>, parent: u64, name: &OsStr, newparent: u64, newname: &OsStr, flags: u32, reply: ReplyEmpty) {
        let (Some((b, dir)), Some((nb, ndir))) = (local(parent), local(newparent)) else { return reply.error(libc::EPERM) };
        if b != nb {
            return reply.error(libc::EXDEV);
        }
        if flags & !libc::RENAME_NOREPLACE != 0 {
            return reply.error(libc::EINVAL);
        }
        let mut store = self.store.lock();
        if flags & libc::RENAME_NOREPLACE != 0 && store.lookup(b, ndir, newname.as_bytes()).is_ok() {
            return reply.error(libc::EEXIST);
        }
        let res = store.rename(b, dir, name.as_bytes(), ndir, newname.as_bytes());
        if let Ok(Some(replaced)) = res {
            if !self.lookups.contains_key(&fuse_ino(b, replaced)) {
                store.forget_orphan(b, replaced);
            }
        }
        drop(store);
        empty(res.map(|_| ()), reply);
    }

    fn open(&mut self, _req: &Request<'_>, _ino: u64, _flags: i32, reply: ReplyOpen) {
        reply.opened(0, 0);
    }

    fn read(&mut self, _req: &Request<'_>, ino: u64, _fh: u64, offset: i64, size: u32, _flags: i32, _lock: Option<u64>, reply: ReplyData) {
        let Some((b, i)) = local(ino) else { return reply.error(libc::EISDIR) };
        let mut buf = vec![0u8; size as usize];
        match self.store.lock().read(b, i, offset.max(0) as u64, &mut buf) {
            Ok(n) => reply.data(&buf[..n]),
            Err(e) => reply.error(errno(&e)),
        }
    }

    fn write(
        &mut self,
        _req: &Request<'_>,
        ino: u64,
        _fh: u64,
        offset: i64,
        data: &[u8],
        _write_flags: u32,
        _flags: i32,
        _lock: Option<u64>,
        reply: ReplyWrite,
    ) {
        let Some((b, i)) = local(ino) else { return reply.error(libc::EISDIR) };
        match self.store.lock().write(b, i, offset.max(0) as u64, data) {
            Ok(n) => reply.written(n as u32),
            Err(e) => reply.error(errno(&e)),
        }
    }

    fn flush(&mut self, _req: &Request<'_>, _ino: u64, _fh: u64, _lock_owner: u64, reply: ReplyEmpty) {
        reply.ok();
    }

    fn release(&mut self, _req: &Request<'_>, _ino: u64, _fh: u64, _flags: i32, _lock: Option<u64>, _flush: bool, reply: ReplyEmpty) {
        reply.ok();
    }

    fn fsync(&mut self, _req: &Request<'_>, _ino: u64, _fh: u64, _datasync: bool, reply: ReplyEmpty) {
        if let Err(mpsc::SendError(reply)) = self.sync_tx.send(reply) {
            reply.error(libc::EIO);
        }
    }

    fn fsyncdir(&mut self, _req: &Request<'_>, _ino: u64, _fh: u64, _datasync: bool, reply: ReplyEmpty) {
        if let Err(mpsc::SendError(reply)) = self.sync_tx.send(reply) {
            reply.error(libc::EIO);
        }
    }

    fn opendir(&mut self, _req: &Request<'_>, ino: u64, _flags: i32, reply: ReplyOpen) {
        let store = self.store.lock();
        let mut list: DirListing = vec![(ino, FileType::Directory, b".".to_vec()), (ino, FileType::Directory, b"..".to_vec())];
        match local(ino) {
            None => {
                for br in store.branches() {
                    list.push((fuse_ino(br.id, ROOT_INO), FileType::Directory, br.name.into_bytes()));
                }
            }
            Some((b, i)) => match store.readdir(b, i) {
                Ok(entries) => list.extend(entries.into_iter().map(|(name, child, kind)| (fuse_ino(b, child), file_type(kind), name))),
                Err(e) => return reply.error(errno(&e)),
            },
        }
        drop(store);
        let fh = self.next_fh;
        self.next_fh += 1;
        self.dirs.insert(fh, list);
        reply.opened(fh, 0);
    }

    fn readdir(&mut self, _req: &Request<'_>, _ino: u64, fh: u64, offset: i64, mut reply: ReplyDirectory) {
        let Some(list) = self.dirs.get(&fh) else { return reply.error(libc::EBADF) };
        for (idx, (ino, kind, name)) in list.iter().enumerate().skip(offset.max(0) as usize) {
            if reply.add(*ino, idx as i64 + 1, *kind, OsStr::from_bytes(name)) {
                break;
            }
        }
        reply.ok();
    }

    fn releasedir(&mut self, _req: &Request<'_>, _ino: u64, fh: u64, _flags: i32, reply: ReplyEmpty) {
        self.dirs.remove(&fh);
        reply.ok();
    }

    fn statfs(&mut self, _req: &Request<'_>, _ino: u64, reply: ReplyStatfs) {
        let st = self.store.lock().stats();
        // The volume is thin-provisioned on the backing directory; report a
        // large fixed capacity minus what is in use.
        let total: u64 = 1 << 31;
        let used = st.allocated_blocks - st.free_blocks;
        let free = total.saturating_sub(used);
        reply.statfs(total, free, free, 1 << 32, 1 << 31, BLOCK_SIZE as u32, 255, BLOCK_SIZE as u32);
    }

    fn access(&mut self, _req: &Request<'_>, _ino: u64, _mask: i32, reply: ReplyEmpty) {
        reply.ok();
    }

    fn fallocate(&mut self, _req: &Request<'_>, ino: u64, _fh: u64, offset: i64, length: i64, mode: i32, reply: ReplyEmpty) {
        let Some((b, i)) = local(ino) else { return reply.error(libc::EISDIR) };
        if mode & !libc::FALLOC_FL_KEEP_SIZE != 0 {
            return reply.error(libc::EOPNOTSUPP);
        }
        if mode & libc::FALLOC_FL_KEEP_SIZE != 0 {
            // Space is allocated on write; there is nothing to reserve.
            return reply.ok();
        }
        let end = (offset.max(0) as u64).saturating_add(length.max(0) as u64);
        let mut store = self.store.lock();
        let res = match store.getattr(b, i) {
            Ok(a) if a.size < end => store.setattr(b, i, SetAttr { size: Some(end), ..Default::default() }).map(|_| ()),
            Ok(_) => Ok(()),
            Err(e) => Err(e),
        };
        drop(store);
        empty(res, reply);
    }
}

/// Mount `store` at `mountpoint`. The filesystem is served from a background
/// thread and unmounted when the returned session is dropped.
pub fn mount(store: Arc<SharedStore>, mountpoint: &Path, allow_other: bool) -> std::io::Result<fuser::BackgroundSession> {
    let mut options = vec![
        MountOption::FSName("banyan".into()),
        MountOption::Subtype("banyan".into()),
        MountOption::DefaultPermissions,
        MountOption::NoAtime,
        MountOption::NoDev,
        MountOption::NoSuid,
    ];
    if allow_other {
        options.push(MountOption::AllowOther);
    }
    fuser::spawn_mount2(BanyanFs::new(store), mountpoint, &options)
}
