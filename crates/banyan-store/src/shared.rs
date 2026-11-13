//! Thread-safe handle with group commit.

use std::sync::{Arc, Mutex, MutexGuard};

use crate::disk::Disk;
use crate::store::{write_batch, GcReport, Result, Store};

/// A [`Store`] behind a mutex, plus a sync path that does its disk I/O
/// without holding that mutex.
///
/// Many callers can wait in [`SharedStore::sync`] at once. The first one
/// writes a batch covering everything pending; the others find their writes
/// already durable when they get their turn and return without touching the
/// disk. Reads and writes on other branches keep running during the fsync.
pub struct SharedStore {
    inner: Mutex<Store>,
    /// Serialises batches so journal bytes reach the disk in order.
    /// Lock order: `sync_lock` before `inner`.
    sync_lock: Mutex<()>,
    disk: Arc<dyn Disk>,
}

impl SharedStore {
    pub fn open(disk: Arc<dyn Disk>) -> Result<Self> {
        let store = Store::open(disk.clone())?;
        Ok(SharedStore { inner: Mutex::new(store), sync_lock: Mutex::new(()), disk })
    }

    pub fn lock(&self) -> MutexGuard<'_, Store> {
        self.inner.lock().unwrap_or_else(|e| e.into_inner())
    }

    /// Make every write that completed before this call durable.
    pub fn sync(&self) -> Result<()> {
        let mark = self.lock().write_mark();
        let _turn = self.sync_lock.lock().unwrap_or_else(|e| e.into_inner());
        let batch = {
            let mut s = self.lock();
            if s.is_synced(mark) {
                return Ok(());
            }
            match s.begin_sync()? {
                Some(b) => b,
                None => return Ok(()),
            }
        };
        let res = write_batch(&*self.disk, &batch);
        self.lock().finish_sync(&batch, res.is_ok());
        Ok(res?)
    }

    /// Reclaim unreferenced blocks. Blocks other operations while it runs.
    pub fn gc(&self) -> Result<GcReport> {
        let _turn = self.sync_lock.lock().unwrap_or_else(|e| e.into_inner());
        self.lock().gc()
    }
}
