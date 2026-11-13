//! `banyan-store`: a copy-on-write, branchable block store.
//!
//! A volume holds any number of *branches*. Each branch is a complete,
//! independent file tree. Creating a branch from another takes constant time
//! and no extra space; the two share all data until one of them writes.
//!
//! This crate has no knowledge of Postgres or FUSE. `banyan-fuse` exposes a
//! volume as a filesystem, and `banyan` runs a Postgres instance on each
//! branch.
//!
//! ```
//! use std::sync::Arc;
//! use banyan_store::{sim::SimDisk, Kind, Store, ROOT_INO};
//!
//! let mut store = Store::open(Arc::new(SimDisk::new())).unwrap();
//! let main = store.fork("main", None, 0, 0).unwrap();
//! let file = store.create(main, ROOT_INO, b"hello", Kind::File, 0o644, 0, 0, b"").unwrap();
//! store.write(main, file.ino, 0, b"from main").unwrap();
//!
//! // O(1), however much data `main` holds.
//! let dev = store.fork("dev", Some("main"), 0, 0).unwrap();
//! store.write(dev, file.ino, 5, b"dev!").unwrap();
//!
//! let mut buf = [0u8; 9];
//! store.read(main, file.ino, 0, &mut buf).unwrap();
//! assert_eq!(&buf, b"from main");
//! store.read(dev, file.ino, 0, &mut buf).unwrap();
//! assert_eq!(&buf, b"from dev!");
//! ```

pub mod disk;
mod shared;
pub mod sim;
mod store;
pub mod trie;

/// Size of one data block. Matches the Postgres page size so a page write
/// maps to exactly one block.
pub const BLOCK_SIZE: usize = 8192;

pub use disk::{Disk, FileDisk};
pub use shared::SharedStore;
pub use store::{Attr, BranchId, BranchInfo, Error, GcReport, Kind, Result, SetAttr, Stats, Store, SyncBatch, ROOT_INO};
