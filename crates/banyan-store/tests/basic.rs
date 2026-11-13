//! Behaviour tests for the store API, on a real file-backed volume where it matters.

use std::sync::Arc;

use banyan_store::sim::SimDisk;
use banyan_store::{Error, FileDisk, Kind, SetAttr, SharedStore, Store, BLOCK_SIZE, ROOT_INO};

fn temp_dir(name: &str) -> std::path::PathBuf {
    let dir = std::env::temp_dir().join(format!("banyan-test-{name}-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    dir
}

fn read_all(store: &Store, b: u32, ino: u64) -> Vec<u8> {
    let size = store.getattr(b, ino).unwrap().size as usize;
    let mut buf = vec![0u8; size];
    assert_eq!(store.read(b, ino, 0, &mut buf).unwrap(), size);
    buf
}

#[test]
fn branches_are_isolated_in_both_directions() {
    let mut s = Store::open(Arc::new(SimDisk::new())).unwrap();
    let main = s.fork("main", None, 0, 0).unwrap();
    let f = s.create(main, ROOT_INO, b"f", Kind::File, 0o644, 0, 0, b"").unwrap().ino;
    s.write(main, f, 0, &vec![1u8; 3 * BLOCK_SIZE]).unwrap();

    let dev = s.fork("dev", Some("main"), 0, 0).unwrap();
    s.write(dev, f, BLOCK_SIZE as u64, &[2u8; 10]).unwrap();
    s.write(main, f, 0, &[3u8; 10]).unwrap();
    s.create(dev, ROOT_INO, b"only-dev", Kind::File, 0o644, 0, 0, b"").unwrap();

    let m = read_all(&s, main, f);
    let d = read_all(&s, dev, f);
    assert_eq!(&m[..10], &[3u8; 10]);
    assert_eq!(&m[BLOCK_SIZE..BLOCK_SIZE + 10], &[1u8; 10]);
    assert_eq!(&d[..10], &[1u8; 10]);
    assert_eq!(&d[BLOCK_SIZE..BLOCK_SIZE + 10], &[2u8; 10]);
    assert!(matches!(s.lookup(main, ROOT_INO, b"only-dev"), Err(Error::NotFound)));
}

#[test]
fn fork_shares_data_until_written() {
    let mut s = Store::open(Arc::new(SimDisk::new())).unwrap();
    let main = s.fork("main", None, 0, 0).unwrap();
    let f = s.create(main, ROOT_INO, b"f", Kind::File, 0o644, 0, 0, b"").unwrap().ino;
    s.write(main, f, 0, &vec![7u8; 100 * BLOCK_SIZE]).unwrap();
    let before = s.stats().allocated_blocks;
    assert_eq!(before, 100);

    // A thousand branches of a 100-block file allocate nothing.
    for i in 0..1000 {
        s.fork(&format!("b{i}"), Some("main"), 0, 0).unwrap();
    }
    assert_eq!(s.stats().allocated_blocks, before);

    // Writing one block in one branch allocates exactly one block.
    let b7 = s.branch_id("b7").unwrap();
    s.write(b7, f, 5 * BLOCK_SIZE as u64, &vec![9u8; BLOCK_SIZE]).unwrap();
    assert_eq!(s.stats().allocated_blocks, before + 1);
    // Rewriting it again reuses the same block (it is private to b7 now).
    s.write(b7, f, 5 * BLOCK_SIZE as u64, &vec![8u8; BLOCK_SIZE]).unwrap();
    assert_eq!(s.stats().allocated_blocks, before + 1);
}

#[test]
fn deleting_a_parent_keeps_children_and_gc_reclaims() {
    let mut s = Store::open(Arc::new(SimDisk::new())).unwrap();
    let main = s.fork("main", None, 0, 0).unwrap();
    let f = s.create(main, ROOT_INO, b"f", Kind::File, 0o644, 0, 0, b"").unwrap().ino;
    s.write(main, f, 0, &vec![1u8; 10 * BLOCK_SIZE]).unwrap();
    let child = s.fork("child", Some("main"), 0, 0).unwrap();
    s.write(child, f, 0, &vec![2u8; 4 * BLOCK_SIZE]).unwrap();

    s.delete_branch("main").unwrap();
    assert!(s.branch_id("main").is_none());
    let data = read_all(&s, child, f);
    assert!(data[..4 * BLOCK_SIZE].iter().all(|b| *b == 2));
    assert!(data[4 * BLOCK_SIZE..].iter().all(|b| *b == 1));

    // Only the 4 blocks main alone referenced become free.
    let report = s.gc().unwrap();
    assert_eq!(report.live_blocks, 10);
    assert_eq!(report.newly_freed, 4);
    assert_eq!(read_all(&s, child, f), data);

    s.delete_branch("child").unwrap();
    assert_eq!(s.gc().unwrap().live_blocks, 0);
}

#[test]
fn unlinked_file_stays_usable_until_forgotten() {
    let mut s = Store::open(Arc::new(SimDisk::new())).unwrap();
    let b = s.fork("main", None, 0, 0).unwrap();
    let f = s.create(b, ROOT_INO, b"f", Kind::File, 0o644, 0, 0, b"").unwrap().ino;
    s.write(b, f, 0, b"hello").unwrap();
    assert_eq!(s.unlink(b, ROOT_INO, b"f", false).unwrap(), f);
    assert!(matches!(s.lookup(b, ROOT_INO, b"f"), Err(Error::NotFound)));

    // Still readable and writable through the inode, like an open file.
    s.write(b, f, 5, b" world").unwrap();
    assert_eq!(read_all(&s, b, f), b"hello world");

    // A fork taken now does not inherit the unlinked file.
    let c = s.fork("c", Some("main"), 0, 0).unwrap();
    assert!(matches!(s.getattr(c, f), Err(Error::NotFound)));

    s.forget_orphan(b, f);
    assert!(matches!(s.getattr(b, f), Err(Error::NotFound)));
}

#[test]
fn namespace_rules() {
    let mut s = Store::open(Arc::new(SimDisk::new())).unwrap();
    let b = s.fork("main", None, 0, 0).unwrap();
    let d = s.create(b, ROOT_INO, b"d", Kind::Dir, 0o755, 0, 0, b"").unwrap().ino;
    let f = s.create(b, d, b"f", Kind::File, 0o644, 0, 0, b"").unwrap().ino;
    let l = s.create(b, ROOT_INO, b"link", Kind::Symlink, 0o777, 0, 0, b"d/f").unwrap();
    assert_eq!(s.readlink(b, l.ino).unwrap(), b"d/f");
    assert_eq!(l.size, 3);

    assert!(matches!(s.create(b, d, b"f", Kind::File, 0o644, 0, 0, b""), Err(Error::Exists)));
    assert!(matches!(s.create(b, f, b"x", Kind::File, 0o644, 0, 0, b""), Err(Error::NotDir)));
    assert!(matches!(s.create(b, d, b"a/b", Kind::File, 0o644, 0, 0, b""), Err(Error::Invalid)));
    assert!(matches!(s.unlink(b, ROOT_INO, b"d", true), Err(Error::NotEmpty)));
    assert!(matches!(s.unlink(b, ROOT_INO, b"d", false), Err(Error::IsDir)));
    assert!(matches!(s.unlink(b, d, b"f", true), Err(Error::NotDir)));
    assert!(matches!(s.write(b, d, 0, b"x"), Err(Error::IsDir)));
    // A directory cannot be moved into itself.
    assert!(matches!(s.rename(b, ROOT_INO, b"d", d, b"inside"), Err(Error::Invalid)));
    assert_eq!(s.resolve(b, "d/f").unwrap().ino, f);
    assert_eq!(s.resolve(b, "/").unwrap().ino, ROOT_INO);

    assert!(matches!(s.fork("main", None, 0, 0), Err(Error::BranchExists)));
    assert!(matches!(s.fork("has space", None, 0, 0), Err(Error::BadBranchName)));
    assert!(matches!(s.fork(".hidden", None, 0, 0), Err(Error::BadBranchName)));
    assert!(matches!(s.fork("x", Some("missing"), 0, 0), Err(Error::NoBranch)));
}

#[test]
fn truncate_then_extend_reads_zeros() {
    let mut s = Store::open(Arc::new(SimDisk::new())).unwrap();
    let b = s.fork("main", None, 0, 0).unwrap();
    let f = s.create(b, ROOT_INO, b"f", Kind::File, 0o644, 0, 0, b"").unwrap().ino;
    s.write(b, f, 0, &vec![0xEEu8; 2 * BLOCK_SIZE]).unwrap();
    s.setattr(b, f, SetAttr { size: Some(100), ..Default::default() }).unwrap();
    s.setattr(b, f, SetAttr { size: Some(2 * BLOCK_SIZE as u64), ..Default::default() }).unwrap();
    let data = read_all(&s, b, f);
    assert!(data[..100].iter().all(|x| *x == 0xEE));
    assert!(data[100..].iter().all(|x| *x == 0), "bytes beyond the truncation point must read as zeros");
}

#[test]
fn file_volume_survives_reopen_and_rejects_second_opener() {
    let dir = temp_dir("reopen");
    {
        let mut s = Store::open(Arc::new(FileDisk::open(&dir).unwrap())).unwrap();
        let main = s.fork("main", None, 1000, 1000).unwrap();
        let f = s.create(main, ROOT_INO, b"f", Kind::File, 0o600, 1000, 1000, b"").unwrap().ino;
        s.write(main, f, 0, &vec![5u8; 3 * BLOCK_SIZE + 17]).unwrap();
        let dev = s.fork("dev", Some("main"), 1000, 1000).unwrap();
        s.write(dev, f, 1, b"dev").unwrap();
        s.sync().unwrap();

        // The volume is locked while open.
        assert!(FileDisk::open(&dir).is_err());
    }
    let s = Store::open(Arc::new(FileDisk::open(&dir).unwrap())).unwrap();
    let (main, dev) = (s.branch_id("main").unwrap(), s.branch_id("dev").unwrap());
    let f = s.lookup(main, ROOT_INO, b"f").unwrap();
    assert_eq!((f.mode, f.uid, f.size), (0o600, 1000, 3 * BLOCK_SIZE as u64 + 17));
    assert!(read_all(&s, main, f.ino).iter().all(|b| *b == 5));
    assert_eq!(&read_all(&s, dev, f.ino)[..5], &[5, b'd', b'e', b'v', 5]);
    assert_eq!(s.branches().iter().find(|b| b.name == "dev").unwrap().parent.as_deref(), Some("main"));
    drop(s);
    std::fs::remove_dir_all(&dir).unwrap();
}

#[test]
fn torn_journal_tail_is_discarded() {
    let dir = temp_dir("torn");
    {
        let mut s = Store::open(Arc::new(FileDisk::open(&dir).unwrap())).unwrap();
        let main = s.fork("main", None, 0, 0).unwrap();
        s.create(main, ROOT_INO, b"kept", Kind::File, 0o644, 0, 0, b"").unwrap();
        s.sync().unwrap();
    }
    // Simulate a crash in the middle of appending a record.
    use std::io::Write;
    let mut j = std::fs::OpenOptions::new().append(true).open(dir.join("journal")).unwrap();
    j.write_all(&[40, 0, 0, 0, 1, 2, 3, 4, 9, 9, 9]).unwrap();
    drop(j);
    {
        let mut s = Store::open(Arc::new(FileDisk::open(&dir).unwrap())).unwrap();
        let main = s.branch_id("main").unwrap();
        assert!(s.lookup(main, ROOT_INO, b"kept").is_ok());
        // New records are appended after the last complete one and survive.
        s.create(main, ROOT_INO, b"after", Kind::File, 0o644, 0, 0, b"").unwrap();
        s.sync().unwrap();
    }
    let s = Store::open(Arc::new(FileDisk::open(&dir).unwrap())).unwrap();
    assert!(s.lookup(s.branch_id("main").unwrap(), ROOT_INO, b"after").is_ok());
    drop(s);
    std::fs::remove_dir_all(&dir).unwrap();
}

#[test]
fn concurrent_writers_and_syncs() {
    let dir = temp_dir("concurrent");
    let store = Arc::new(SharedStore::open(Arc::new(FileDisk::open(&dir).unwrap())).unwrap());
    store.lock().fork("main", None, 0, 0).unwrap();
    let threads: Vec<_> = (0..8u8)
        .map(|t| {
            let store = store.clone();
            std::thread::spawn(move || {
                let name = format!("t{t}");
                let b = store.lock().fork(&name, Some("main"), 0, 0).unwrap();
                let f = store.lock().create(b, ROOT_INO, b"f", Kind::File, 0o644, 0, 0, b"").unwrap().ino;
                for i in 0..50u64 {
                    store.lock().write(b, f, i * BLOCK_SIZE as u64, &vec![t; BLOCK_SIZE]).unwrap();
                    if i % 5 == 0 {
                        store.sync().unwrap();
                    }
                }
                store.sync().unwrap();
            })
        })
        .collect();
    threads.into_iter().for_each(|t| t.join().unwrap());
    drop(store);

    let s = Store::open(Arc::new(FileDisk::open(&dir).unwrap())).unwrap();
    for t in 0..8u8 {
        let b = s.branch_id(&format!("t{t}")).unwrap();
        let f = s.lookup(b, ROOT_INO, b"f").unwrap();
        assert_eq!(f.size, 50 * BLOCK_SIZE as u64);
        assert!(read_all(&s, b, f.ino).iter().all(|x| *x == t));
    }
    drop(s);
    std::fs::remove_dir_all(&dir).unwrap();
}
