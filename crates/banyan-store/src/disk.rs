//! Backing storage abstraction.
//!
//! The store keeps two things on disk: a data area addressed in fixed-size
//! slots, and an append-only metadata journal. Putting both behind a trait
//! lets the same store code run against real files and against a simulated
//! disk that loses unsynced writes on a crash (see [`crate::sim`]).

use std::fs::{File, OpenOptions};
use std::io::{self, BufReader, Read, Seek, SeekFrom, Write};
use std::os::unix::fs::FileExt;
use std::path::Path;

use crate::BLOCK_SIZE;

pub trait Disk: Send + Sync {
    /// Read one slot. Slots that were never written read as zeros.
    fn read_block(&self, slot: u64, buf: &mut [u8]) -> io::Result<()>;
    fn write_block(&self, slot: u64, buf: &[u8]) -> io::Result<()>;
    /// Make every completed `write_block` durable.
    fn sync_data(&self) -> io::Result<()>;
    /// Stream the journal payload from the start.
    fn journal_reader(&self) -> io::Result<Box<dyn Read + '_>>;
    fn journal_append(&self, bytes: &[u8]) -> io::Result<()>;
    fn journal_sync(&self) -> io::Result<()>;
    /// Drop everything after `len` payload bytes (used to cut a torn tail).
    fn journal_truncate(&self, len: u64) -> io::Result<()>;
    fn journal_len(&self) -> io::Result<u64>;
}

const MAGIC: &[u8; 8] = b"BANYANJ1";
const HEADER_LEN: u64 = 16;

/// A volume stored as two files in a directory: `data` and `journal`.
pub struct FileDisk {
    data: File,
    journal: File,
}

impl FileDisk {
    /// Open the volume in `dir`, creating it if it does not exist. Takes an
    /// exclusive lock so two processes cannot open the same volume.
    pub fn open(dir: &Path) -> io::Result<Self> {
        std::fs::create_dir_all(dir)?;
        let data = OpenOptions::new().read(true).write(true).create(true).truncate(false).open(dir.join("data"))?;
        let mut journal = OpenOptions::new().read(true).append(true).create(true).open(dir.join("journal"))?;
        if journal.try_lock().is_err() {
            return Err(io::Error::new(io::ErrorKind::WouldBlock, format!("volume {} is in use by another process", dir.display())));
        }
        let len = journal.metadata()?.len();
        if len < HEADER_LEN {
            // New volume, or a crash before the header was complete.
            journal.set_len(0)?;
            let mut header = [0u8; HEADER_LEN as usize];
            header[..8].copy_from_slice(MAGIC);
            header[8..12].copy_from_slice(&1u32.to_le_bytes());
            header[12..16].copy_from_slice(&(BLOCK_SIZE as u32).to_le_bytes());
            journal.write_all(&header)?;
            journal.sync_all()?;
            File::open(dir)?.sync_all()?;
        } else {
            let mut header = [0u8; HEADER_LEN as usize];
            journal.read_exact_at(&mut header, 0)?;
            let block = u32::from_le_bytes(header[12..16].try_into().unwrap());
            if &header[..8] != MAGIC || block as usize != BLOCK_SIZE {
                return Err(io::Error::new(io::ErrorKind::InvalidData, "not a banyan volume (bad journal header)"));
            }
        }
        Ok(FileDisk { data, journal })
    }
}

impl Disk for FileDisk {
    fn read_block(&self, slot: u64, buf: &mut [u8]) -> io::Result<()> {
        let mut off = slot * BLOCK_SIZE as u64;
        let mut done = 0;
        while done < buf.len() {
            match self.data.read_at(&mut buf[done..], off) {
                Ok(0) => {
                    buf[done..].fill(0);
                    break;
                }
                Ok(n) => {
                    done += n;
                    off += n as u64;
                }
                Err(e) if e.kind() == io::ErrorKind::Interrupted => {}
                Err(e) => return Err(e),
            }
        }
        Ok(())
    }

    fn write_block(&self, slot: u64, buf: &[u8]) -> io::Result<()> {
        self.data.write_all_at(buf, slot * BLOCK_SIZE as u64)
    }

    fn sync_data(&self) -> io::Result<()> {
        self.data.sync_data()
    }

    fn journal_reader(&self) -> io::Result<Box<dyn Read + '_>> {
        let mut f = self.journal.try_clone()?;
        f.seek(SeekFrom::Start(HEADER_LEN))?;
        Ok(Box::new(BufReader::with_capacity(1 << 20, f)))
    }

    fn journal_append(&self, bytes: &[u8]) -> io::Result<()> {
        (&self.journal).write_all(bytes)
    }

    fn journal_sync(&self) -> io::Result<()> {
        self.journal.sync_data()
    }

    fn journal_truncate(&self, len: u64) -> io::Result<()> {
        self.journal.set_len(HEADER_LEN + len)?;
        self.journal.sync_all()
    }

    fn journal_len(&self) -> io::Result<u64> {
        Ok(self.journal.metadata()?.len().saturating_sub(HEADER_LEN))
    }
}
