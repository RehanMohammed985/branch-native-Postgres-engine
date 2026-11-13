//! A simulated disk for deterministic crash testing.
//!
//! Writes land in a volatile layer until the matching sync call. A simulated
//! crash keeps a random subset of the unsynced data blocks and a random
//! prefix of the unsynced journal bytes (so records can be torn mid-way),
//! which is the freedom a real disk and page cache have on power loss.

use std::collections::BTreeMap;
use std::io::{self, Cursor, Read};
use std::sync::Mutex;

use crate::disk::Disk;
use crate::BLOCK_SIZE;

/// Small deterministic RNG (SplitMix64) so every run is reproducible from a seed.
#[derive(Clone)]
pub struct Rng(pub u64);

impl Rng {
    pub fn next_u64(&mut self) -> u64 {
        self.0 = self.0.wrapping_add(0x9e3779b97f4a7c15);
        let mut z = self.0;
        z = (z ^ (z >> 30)).wrapping_mul(0xbf58476d1ce4e5b9);
        z = (z ^ (z >> 27)).wrapping_mul(0x94d049bb133111eb);
        z ^ (z >> 31)
    }
    /// Uniform in `0..n` (`n > 0`).
    pub fn below(&mut self, n: u64) -> u64 {
        self.next_u64() % n
    }
    pub fn chance(&mut self, num: u64, den: u64) -> bool {
        self.below(den) < num
    }
}

#[derive(Default)]
struct State {
    durable: BTreeMap<u64, Vec<u8>>,
    volatile: BTreeMap<u64, Vec<u8>>,
    journal_durable: Vec<u8>,
    journal_volatile: Vec<u8>,
    /// When set, the Nth next disk call fails with an I/O error.
    fail_in: Option<u64>,
}

impl State {
    fn tick(&mut self) -> io::Result<()> {
        if let Some(n) = self.fail_in.as_mut() {
            if *n == 0 {
                return Err(io::Error::other("injected disk failure"));
            }
            *n -= 1;
        }
        Ok(())
    }
}

#[derive(Default)]
pub struct SimDisk {
    st: Mutex<State>,
}

impl SimDisk {
    pub fn new() -> Self {
        Self::default()
    }

    /// Simulate power loss: unsynced data blocks each survive or vanish at
    /// random, and a random prefix of the unsynced journal survives.
    pub fn crash(&self, rng: &mut Rng) {
        let mut st = self.st.lock().unwrap();
        let volatile = std::mem::take(&mut st.volatile);
        for (slot, data) in volatile {
            if rng.chance(1, 2) {
                st.durable.insert(slot, data);
            }
        }
        let pending = std::mem::take(&mut st.journal_volatile);
        let keep = rng.below(pending.len() as u64 + 1) as usize;
        st.journal_durable.extend_from_slice(&pending[..keep]);
        st.fail_in = None;
    }

    /// Make the Nth next disk operation (and all after it) fail.
    pub fn fail_after(&self, ops: u64) {
        self.st.lock().unwrap().fail_in = Some(ops);
    }

    pub fn durable_blocks(&self) -> usize {
        self.st.lock().unwrap().durable.len()
    }
}

impl Disk for SimDisk {
    fn read_block(&self, slot: u64, buf: &mut [u8]) -> io::Result<()> {
        let mut st = self.st.lock().unwrap();
        st.tick()?;
        match st.volatile.get(&slot).or_else(|| st.durable.get(&slot)) {
            Some(d) => buf.copy_from_slice(&d[..buf.len()]),
            None => buf.fill(0),
        }
        Ok(())
    }

    fn write_block(&self, slot: u64, buf: &[u8]) -> io::Result<()> {
        assert_eq!(buf.len(), BLOCK_SIZE);
        let mut st = self.st.lock().unwrap();
        st.tick()?;
        st.volatile.insert(slot, buf.to_vec());
        Ok(())
    }

    fn sync_data(&self) -> io::Result<()> {
        let mut st = self.st.lock().unwrap();
        st.tick()?;
        let volatile = std::mem::take(&mut st.volatile);
        st.durable.extend(volatile);
        Ok(())
    }

    fn journal_reader(&self) -> io::Result<Box<dyn Read + '_>> {
        let st = self.st.lock().unwrap();
        let mut all = st.journal_durable.clone();
        all.extend_from_slice(&st.journal_volatile);
        Ok(Box::new(Cursor::new(all)))
    }

    fn journal_append(&self, bytes: &[u8]) -> io::Result<()> {
        let mut st = self.st.lock().unwrap();
        st.tick()?;
        st.journal_volatile.extend_from_slice(bytes);
        Ok(())
    }

    fn journal_sync(&self) -> io::Result<()> {
        let mut st = self.st.lock().unwrap();
        st.tick()?;
        let pending = std::mem::take(&mut st.journal_volatile);
        st.journal_durable.extend_from_slice(&pending);
        Ok(())
    }

    fn journal_truncate(&self, len: u64) -> io::Result<()> {
        let mut st = self.st.lock().unwrap();
        let len = len as usize;
        let durable = st.journal_durable.len();
        if len <= durable {
            st.journal_durable.truncate(len);
            st.journal_volatile.clear();
        } else {
            st.journal_volatile.truncate(len - durable);
        }
        Ok(())
    }

    fn journal_len(&self) -> io::Result<u64> {
        let st = self.st.lock().unwrap();
        Ok((st.journal_durable.len() + st.journal_volatile.len()) as u64)
    }
}
