# Banyan

[![CI](https://github.com/RehanMohammed985/branch-native-Postgres-engine/actions/workflows/ci.yml/badge.svg)](https://github.com/RehanMohammed985/branch-native-Postgres-engine/actions/workflows/ci.yml)
[![License](https://img.shields.io/badge/license-Apache--2.0-blue.svg)](LICENSE)

**Branch-native Postgres.** Create a full, writable copy of a Postgres
database in about a millisecond, whatever its size, and connect to it by name.
Make thousands of them. Throw them away.

```console
$ banyan branch create experiment
created branch experiment from main (fork 4 us, checkpoint 9.4 ms, sync 0.6 ms)

$ psql -h 127.0.0.1 -U postgres -d experiment
```

`experiment` is a real Postgres 16 with all of `main`'s data. Nothing you do
in it is visible in `main`, and it costs no storage until it changes something.

Banyan is built for workloads where a database needs many cheap alternate
versions of itself: AI agents trying a migration or a bulk edit before
committing to it, tree search over database states, a fresh copy of staging
for every test.

> **Status: v0.1, experimental.** It works, it is tested, and the numbers
> below are real. It has no authentication, runs on a single node, needs
> Linux with FUSE, and nobody runs it in production. Read
> [Limitations](#limitations) before you rely on it.

## Why

In May 2026 a group at Columbia published
[BranchBench](https://daplab.cs.columbia.edu/general/2026/05/26/branchable-databases-arent-ready-for-agentic-workloads.html),
a benchmark for agent-style database branching. Their conclusion was that a
system supporting "thousands of cheap alternate realities" at normal query
speed "does not exist yet": in their runs, existing systems hit limits on
active branches or timed out on a 1,000-step tree search.

Banyan is an attempt at that system with a deliberately small design:

1. A copy-on-write block store where **creating a branch copies one pointer**
   and reading from a branch never walks a chain of ancestors.
2. A filesystem on top of it, so **unmodified Postgres** runs on each branch.
3. A daemon that gives every branch a Postgres **on demand**, behind one
   endpoint, so branches cost storage, not memory.

## Measured

One laptop-class Linux VM, Postgres 16, this repository's own benchmark tool.
The baseline is stock Postgres cloning databases with `CREATE DATABASE ...
TEMPLATE`, the only way to copy a database it has. Full method, raw data and
the caveats are in [docs/BENCHMARKS.md](docs/BENCHMARKS.md).

**A 1,000-step tree search** (branch a promising state, change it, score it,
prune the worst), 600 branches alive at the end:

| | Banyan | Postgres template copy |
|---|---:|---:|
| Wall time | **108 s** | 422 s |
| Storage for the 600 live branches | **1.1 GiB** | 22 GiB |
| Create a branch (p50) | **1.1 ms** | 300 ms |
| First connection to a new branch (p50) | 46 ms | **2 ms** |
| Mutate, then evaluate (p50 each) | 21 ms, 33 ms | **11 ms, 26 ms** |

**Branch creation versus database size:**

| Database size | Banyan (p50) | Postgres template copy (p50) |
|---:|---:|---:|
| 9 MiB | 0.85 ms | 161 ms |
| 157 MiB | 1.12 ms | 334 ms |
| 456 MiB | 0.86 ms | 1,005 ms |

**Also measured:** 5,000 branches of one database created in 4.8 s using
0.02 MiB of extra storage; a chain 1,000 branches deep with no slowdown in
point queries (0.10 ms at depth 10, 0.13 ms at depth 1,000).

**Where Banyan is slower:** running queries. Every page crosses FUSE, and a
branch's first connection starts a Postgres instance. For one long-lived
database under steady load, plain Postgres is the better choice: in a
4-client `pgbench` run a Banyan branch did 4,584 transactions per second and
vanilla Postgres 9,199
([details](docs/BENCHMARKS.md#5-steady-state-throughput-on-a-single-branch)).

These are not BranchBench results. Running the official harness is
[issue #7](https://github.com/RehanMohammed985/branch-native-Postgres-engine/issues/7).

## Quick start

You need Docker. (On macOS and Windows, Docker Desktop provides the Linux
kernel and FUSE that Banyan needs.)

```bash
git clone https://github.com/RehanMohammed985/branch-native-Postgres-engine.git
cd branch-native-Postgres-engine
docker build -t banyan .

docker run -d --name banyan \
  --device /dev/fuse --cap-add SYS_ADMIN --security-opt apparmor:unconfined \
  -p 127.0.0.1:5432:5432 -p 127.0.0.1:8432:8432 \
  -v banyan-data:/var/lib/banyan \
  banyan
```

If port 5432 is already taken on your machine, change the left-hand side of
the mapping (for example `-p 127.0.0.1:15432:5432`).

The first start creates a branch called `main` and runs `initdb` in it. Then:

```bash
# A shell with psql and the banyan CLI, inside the container
docker exec -it banyan bash

psql -h 127.0.0.1 -U postgres -d main -c "create table notes(id serial primary key, body text)"
psql -h 127.0.0.1 -U postgres -d main -c "insert into notes(body) values ('written on main')"

banyan branch create experiment
psql -h 127.0.0.1 -U postgres -d experiment -c "insert into notes(body) values ('only on experiment')"

psql -h 127.0.0.1 -U postgres -d experiment -c "select * from notes"   # two rows
psql -h 127.0.0.1 -U postgres -d main       -c "select * from notes"   # one row

banyan branch list
banyan branch delete experiment
```

If you have `psql` on the host, the same commands work from outside the
container: the endpoint is published on `127.0.0.1:5432`.

## Using it

### Connecting

There is one Postgres endpoint. The **database name selects the branch**:

| `dbname` | Branch | Database inside the branch |
|---|---|---|
| `experiment` | `experiment` | `postgres` (the default) |
| `shop@experiment` | `experiment` | `shop` |

Any driver works; the proxy only reads the startup packet and then passes
bytes through.

```
postgresql://postgres@127.0.0.1:5432/experiment
```

### CLI

```
banyan serve                              run the daemon
banyan branch create <name> [--from main] instant copy of a branch
banyan branch list
banyan branch delete <name> [--force]     children of the branch are unaffected
banyan branch stop <name>                 stop its Postgres; restarts on next connection
banyan gc                                 reclaim the space of deleted branches
banyan status
```

`banyan serve --help` lists the server options. The ones that matter:

| Option | Default | Meaning |
|---|---|---|
| `--data` | `/var/lib/banyan` | Where the volume lives |
| `--max-running` | `8` | Postgres instances allowed at once. Branches are not limited; idle instances are stopped to make room |
| `--idle-timeout` | `300` | Seconds without clients before a branch's instance is stopped |
| `--shared-buffers` | `32MB` | Per instance |
| `--pg-option name=value` | | Any other Postgres setting, for every instance |

### HTTP API

```bash
curl -s -X POST localhost:8432/v1/branches -d '{"name":"experiment","from":"main"}' -H 'content-type: application/json'
curl -s localhost:8432/v1/branches
curl -s -X DELETE localhost:8432/v1/branches/experiment
curl -s -X POST localhost:8432/v1/gc
curl -s localhost:8432/v1/status
```

### What a branch is, precisely

A branch is a snapshot of its parent's files at one instant. If the parent
was running, the branch is what the parent would be after a power cut at that
instant: every committed transaction is there, in-flight transactions are
not. Postgres recovers it the usual way on first start. By default Banyan
checkpoints the parent first so there is nothing to replay.

Deleting a branch never affects branches created from it.

## How it works

```
   psql / any Postgres driver                     banyan CLI / HTTP API
               |  dbname = branch                           |
               v                                            v
+----------------------------- banyan daemon ------------------------------+
|  proxy                              manager                              |
|  routes by branch name              starts and stops Postgres per branch |
+----------+----------------+------------------------------------+---------+
           |                |                                    |
           v                v                                    |
   postgres (main)   postgres (experiment)   ... on demand       |
           |                |                                    |
           v                v                                    | fork
+--------------------- banyan-fuse ----------------------+       | delete
|  /mnt/banyan/<branch>/      one directory per branch   |       | gc
+---------------------------+----------------------------+       |
                            |                                    |
                            v                                    v
+----------------------------- banyan-store -------------------------------+
|  a persistent tree per branch, copy-on-write 8 KiB blocks, a journal     |
+--------------------------------------------------------------------------+
```

**The store** ([`crates/banyan-store`](crates/banyan-store)). Every branch
owns a persistent radix tree of inodes, and every file a persistent tree of
blocks. Persistent means structurally shared: forking clones the root
pointer, and a later write copies only the few nodes on the path to the block
it changes. A branch does not point at its parent, so a lookup in a branch
1,000 forks deep takes the same steps as in the root.

Blocks are 8 KiB, one Postgres page each. A block that only one branch can
see is overwritten in place (tracked with a per-fork epoch), so the WAL and
hot pages do not turn into copy-on-write garbage.

Metadata goes through a journal that is made durable only after the data it
points to, and freed blocks are recycled only after a sync. That is the whole
crash-safety argument; [docs/ARCHITECTURE.md](docs/ARCHITECTURE.md) spells it
out.

**The filesystem** ([`crates/banyan-fuse`](crates/banyan-fuse)). One mount,
one directory per branch. A new branch is a new name, not a new mount.
Postgres is not patched or linked against; it sees an ordinary data directory.

**The daemon** ([`crates/banyan`](crates/banyan)). Branches are storage.
A Postgres instance is attached to a branch when a connection for it arrives
(about 20 to 50 ms) and detached when it has been idle or another branch needs the
slot. A volume can hold thousands of branches on a machine that fits eight
instances in memory.

## How it is tested

- **Crash simulation.** A deterministic simulator
  ([`tests/dst.rs`](crates/banyan-store/tests/dst.rs)) runs random file
  operations, forks, branch deletions, garbage collections, restarts, injected
  disk errors and simulated power losses against the store and against a
  naive reference model, and compares them byte for byte. A failing run
  prints a seed that reproduces it exactly. CI runs 1,000 seeds per pull
  request.
- **The simulator is itself tested.** Four bugs were injected on purpose
  (overwrite a shared block in place; recycle blocks before syncing; sync the
  journal before the data; leave stale bytes past end-of-file). It caught each
  within a few seeds. While the store was being written it also found a real
  one: after a crash, a write's block mappings could be replayed without its
  size update, leaking blocks. A write is now a single journal record.
- **End to end against real Postgres.** [`tests/e2e.sh`](tests/e2e.sh) starts
  the daemon on a real FUSE mount and checks branch isolation, a 25-deep
  chain, 200 branches of one parent, concurrent `pgbench` followed by
  `amcheck` verification of every index and table, space reclaim, and
  recovery after `kill -9`. It runs in CI.

What this does not cover: real power loss on real disks, torn 8 KiB writes
(the simulator treats a block write as atomic; Postgres protects itself with
full-page writes), long soak runs, and the Postgres regression suite
([#13](https://github.com/RehanMohammed985/branch-native-Postgres-engine/issues/13)).

## Limitations

- **No authentication, no TLS.** Anyone who can reach the endpoint is a
  Postgres superuser. It binds to loopback by default; keep it that way
  ([#10](https://github.com/RehanMohammed985/branch-native-Postgres-engine/issues/10)).
- **Linux only**, with FUSE 3. Docker Desktop works on macOS and Windows.
- **Single node.** No replication, no object-storage tier.
- **Query speed.** The filesystem serves requests on one thread
  ([#9](https://github.com/RehanMohammed985/branch-native-Postgres-engine/issues/9))
  and pages are not shared between branches in the page cache
  ([#15](https://github.com/RehanMohammed985/branch-native-Postgres-engine/issues/15)).
- **Restart time grows with history.** The metadata journal is replayed on
  start and never truncated
  ([#8](https://github.com/RehanMohammed985/branch-native-Postgres-engine/issues/8)).
- **Space is reused, not returned.** `banyan gc` frees blocks inside the
  volume; the volume file does not shrink
  ([#14](https://github.com/RehanMohammed985/branch-native-Postgres-engine/issues/14)).
- **No diff or merge** between branches
  ([#16](https://github.com/RehanMohammed985/branch-native-Postgres-engine/issues/16)).
- **Ctrl-C does not cancel a query** through the proxy
  ([#11](https://github.com/RehanMohammed985/branch-native-Postgres-engine/issues/11)),
  and tools that reconnect to other databases, such as `pg_dumpall`, cannot
  be routed
  ([#12](https://github.com/RehanMohammed985/branch-native-Postgres-engine/issues/12)).
- **Each branch that is started costs about 0.4 MiB**, which is what a
  Postgres start writes, before your own changes.

## Building and testing from source

Banyan needs Linux. On macOS, `docker/dev.sh` runs any command in a container
with Rust, FUSE and Postgres 16:

```bash
docker/dev.sh cargo test --workspace          # unit tests and crash simulation
docker/dev.sh bash bench/run.sh banyan        # benchmarks
```

On Linux with `fuse3` and PostgreSQL installed:

```bash
cargo build --release
cargo test --workspace
tests/e2e.sh target/release/banyan            # as a non-root user
```

To reproduce a crash-simulation failure, or run more seeds:

```bash
BANYAN_DST_SEED=1234 cargo test -p banyan-store --test dst
BANYAN_DST_SEEDS=10000 cargo test --release -p banyan-store --test dst
```

## Repository layout

```
crates/banyan-store   copy-on-write block store, journal, garbage collection, simulator
crates/banyan-fuse    FUSE filesystem: one directory per branch
crates/banyan         daemon, proxy, HTTP API, CLI
crates/banyan-bench   benchmark workloads
tests/e2e.sh          end-to-end test against real Postgres
docs/                 architecture and benchmark write-ups
```

## License

Apache-2.0. See [LICENSE](LICENSE).
