//! Mount a volume and keep it mounted until the process is killed.
//!
//!   cargo run -p banyan-fuse --example mount -- <volume-dir> <mountpoint>
//!
//! Creates a `main` branch on a new volume. Fork it from another shell by
//! sending SIGUSR1: each signal creates `fork-N` from `main`.

use std::path::Path;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;

use banyan_store::{FileDisk, SharedStore};

static FORK: AtomicBool = AtomicBool::new(false);

extern "C" fn on_usr1(_: libc::c_int) {
    FORK.store(true, Ordering::SeqCst);
}

fn main() {
    let args: Vec<String> = std::env::args().collect();
    if args.len() != 3 {
        eprintln!("usage: mount <volume-dir> <mountpoint>");
        std::process::exit(2);
    }
    let store = Arc::new(SharedStore::open(Arc::new(FileDisk::open(Path::new(&args[1])).expect("open volume"))).expect("replay journal"));
    let (uid, gid) = unsafe { (libc::getuid(), libc::getgid()) };
    if store.lock().branch_id("main").is_none() {
        store.lock().fork("main", None, uid, gid).expect("create main");
    }
    let _session = banyan_fuse::mount(store.clone(), Path::new(&args[2]), false).expect("mount");
    unsafe { libc::signal(libc::SIGUSR1, on_usr1 as *const () as usize) };
    println!("mounted {} at {}", args[1], args[2]);
    let mut n = 0;
    loop {
        std::thread::sleep(std::time::Duration::from_millis(50));
        if FORK.swap(false, Ordering::SeqCst) {
            n += 1;
            let t = std::time::Instant::now();
            store.lock().fork(&format!("fork-{n}"), Some("main"), uid, gid).expect("fork");
            println!("fork-{n} created in {:?}", t.elapsed());
        }
    }
}
