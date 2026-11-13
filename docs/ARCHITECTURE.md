# Architecture

Banyan is three layers. Each is a separate crate and can be read on its own.

```
 clients (psql, any driver)            banyan CLI / HTTP
          |                                   |
          v                                   v
 +-------------------- banyan (daemon) ---------------------+
 |  proxy: database name -> branch      API: create/delete  |
 |  manager: one Postgres per branch, started on demand     |
 +--------+------------------------------------+------------+
          | unix socket                        | fork / delete / gc
          v                                    |
   postgres -D /mnt/banyan/<branch>/pgdata     |
          | file I/O                           |
          v                                    v
 +------------------ banyan-fuse -------------------------+
 |  /mnt/banyan/<branch>/...  one directory per branch    |
 +-----------------------------+--------------------------+
                               v
 +------------------ banyan-store ------------------------+
 |  persistent trees per branch, copy-on-write blocks     |
 |  data file (8 KiB slots)   +   metadata journal        |
 +--------------------------------------------------------+
```

Postgres is not modified and not linked against. A branch is a directory
that contains an ordinary Postgres data directory; the directory happens to
live in a filesystem where copying a whole tree costs one pointer copy.

## banyan-store

### Data model

A volume is a set of branches. A branch is a file tree:

- an **inode table**: a persistent radix tree from inode number to inode;
- per regular file, a **block map**: a persistent radix tree from block
  index to a slot in the data file;
- per directory, a sorted map from name to inode number.

"Persistent" means structurally shared and copy-on-write. The radix tree has
64-way fanout. Cloning it copies one `Arc`. Changing a key in a clone copies
only the nodes on the path to that key (at most 11), and only those still
shared with another tree; `Arc::make_mut` makes that decision per node.

Three properties follow directly:

1. **Fork is O(1).** `fork(child, parent)` clones the parent's inode table
   root. Nothing else is copied, whatever the size of the parent.
2. **Reads do not depend on depth.** A branch does not point at its parent;
   it has its own complete tree that happens to share nodes. Resolving a
   block is the same walk in the root branch and in a branch 1,000 forks deep.
3. **Deleting a parent is harmless.** Children hold their own references.

### Copy-on-write and in-place overwrite

Blocks are 8 KiB, the Postgres page size, so one page write is one block.

Every fork assigns a new **epoch** to both the parent and the child. Each
slot records the epoch that allocated it. When a branch writes a block:

- if the block's slot was allocated in the branch's current epoch, no other
  branch can reference it, and it is **overwritten in place**;
- otherwise a new slot is allocated, the full block is written there, and
  the block map is updated (copying the shared tree nodes on the path).

The in-place path matters for Postgres: WAL pages and hot heap pages are
rewritten constantly. With pure copy-on-write each rewrite would allocate a
slot and a journal record; with epochs only the first write after a fork does.

### On-disk format

Two files in the volume directory:

- `data`: an array of 8 KiB slots. No header, no per-slot metadata.
- `journal`: a 16-byte header, then records `[len u32][crc32 u32][body]`.
  A record is one metadata change: fork, delete branch, create, unlink,
  rename, setattr, map block, or write (the block mappings of one `write`
  call plus the new file size, as a single record).

There is no other persistent state. On open, the journal is replayed from the
start through the same `apply` function that live operations use, so replay
and live execution cannot diverge. The free-slot list is not stored: it is
recomputed after replay by marking every slot reachable from any branch.

### Sync protocol

Metadata changes are applied to memory and appended to an in-memory journal
buffer. Data blocks are written to the data file immediately (buffered by the
OS). A sync does, in this order:

1. `fdatasync` the data file;
2. append the buffered journal bytes to the journal file;
3. `fdatasync` the journal file.

So a journal record is never durable before the data it refers to. `fsync`
on any file in any branch runs this protocol for the whole volume; callers
that arrive while a sync is in flight are answered by the next one (group
commit), and the disk I/O happens without holding the store lock.

### Crash model

After power loss the volume comes back with:

- exactly the branches, names and file sizes of the last completed sync
  (or, if the crash interrupted step 2 or 3, of some operation boundary after
  it: a prefix of the journal being written, with a torn final record dropped);
- file contents where each byte is either the synced byte or a byte written
  to that position since the sync. An unsynced in-place overwrite may or may
  not survive. This is the contract of an ordinary filesystem, and it is the
  one Postgres is designed for (WAL plus full-page writes).

Two rules make this hold:

- **Data before journal** (above).
- **No slot reuse before sync.** Slots are only returned to the free list by
  garbage collection, which syncs first. Without that, a slot that the durable
  journal still maps to file A could be handed to file B, and a crash would
  leave A pointing at B's bytes.

One more detail: bytes past end-of-file inside the last block of a file are
unspecified (a truncate leaves them; a crash can revert a size change after
the data was overwritten). They are zeroed whenever the file grows, so they
are never readable.

### Garbage collection

`gc` syncs, then marks from every branch root. The marker keeps a set of
visited node addresses and skips subtrees it has seen, so 1,000 branches that
share 99% of their data cost about as much to mark as one. Every unmarked
slot goes on the free list.

### Testing

`tests/dst.rs` is a deterministic simulation: one seed drives a long random
sequence of creates, writes at awkward offsets, truncates, renames, unlinks,
forks, branch deletions, collections, clean restarts, injected I/O errors and
power losses, against the store on a simulated disk and against a reference
model that keeps a full copy of every file in every branch. The simulated
disk keeps unsynced writes in a volatile layer; a power loss keeps a random
subset of unsynced data blocks and a random prefix of unsynced journal bytes.

After each power loss the test checks the crash model above, byte by byte.

The simulation was itself tested by injecting bugs (overwrite shared blocks
in place; collect without syncing; sync journal before data; skip zeroing
past EOF). It caught each within a handful of seeds. CI runs 1,000 seeds.

## banyan-fuse

The mount point has one directory per branch. Kernel inode numbers pack
`(branch id + 1) << 40 | inode`, so branches never collide and a new branch
needs no mount call: it is simply a name that starts resolving.

- Writes are passed straight through (no kernel write-back cache). A fork
  therefore contains every write the kernel has acknowledged to Postgres.
- `fsync` and `fsyncdir` are handed to a sync thread and answered when the
  volume sync completes.
- Unlinked files stay in the inode table until the kernel forgets them, so
  Postgres can keep using a file descriptor after the name is gone.
- Hard links, extended attributes and special files are not supported;
  Postgres does not need them.

Requests are handled on one thread today (see the roadmap).

## banyan (daemon)

### Branches are storage; compute is attached on demand

The manager keeps at most `--max-running` Postgres instances alive. A
connection for a branch without an instance starts one; if the limit is
reached, the least recently used instance with no clients is stopped first
(a normal fast shutdown). Idle instances are stopped after `--idle-timeout`.
The number of branches is bounded by storage, not by memory.

Starting an instance:

1. remove a leftover `postmaster.pid` (after a fork it is a copy of the
   parent's; the volume lock guarantees no other postmaster uses the directory);
2. spawn `postgres -D <branch>/pgdata` on a private unix socket, with
   `PR_SET_PDEATHSIG` so it cannot outlive the daemon;
3. poll the socket until the server stops answering "starting up".

### Forking a running database

A fork of a running parent is a crash-consistent image: the child sees
exactly what the parent had written at that instant, and Postgres recovers
from it the way it recovers from a power cut. Uncommitted transactions in the
parent do not exist in the child.

By default the daemon runs `CHECKPOINT` on the parent first, so the child has
no WAL to replay on its first start. The fork itself is still the O(1) store
operation; the checkpoint costs what flushing the parent's dirty buffers
costs. `--no-checkpoint` skips it.

### Proxy

The proxy reads the startup packet, takes the branch from the database name
(`branch` or `database@branch`), makes sure the branch has an instance,
rewrites the database name, and then splices bytes between the client and the
instance's unix socket. It never parses queries.
