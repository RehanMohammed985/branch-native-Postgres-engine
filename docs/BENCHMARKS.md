# Benchmarks

Everything here was measured, on one machine, with the tools in this
repository. The raw reports are in [`docs/bench/`](bench/) and
[`bench/run.sh`](../bench/run.sh) reproduces them.

**Read this first**

- These are this project's own workloads. They are shaped after the workload
  categories in [BranchBench](https://daplab.cs.columbia.edu/general/2026/05/26/branchable-databases-arent-ready-for-agentic-workloads.html)
  (tree search, fan-out, deep chains, branch lifecycle), but they are **not
  BranchBench runs** and must not be compared with the numbers in that paper.
  Running the official harness is tracked in
  [#7](https://github.com/RehanMohammed985/branch-native-Postgres-engine/issues/7).
- One machine, one run per configuration, a laptop-class Linux VM. Treat the
  figures as orders of magnitude, not as a leaderboard entry.
- The baseline is stock Postgres, where the only way to clone a database is
  `CREATE DATABASE ... TEMPLATE ... STRATEGY = FILE_COPY`. It is not a
  comparison against other branching systems (Neon, Xata, Dolt); none of them
  was run here.

## Setup

| | |
|---|---|
| Host | Apple M3, 8 cores, 8 GB RAM |
| Environment | Docker Desktop Linux VM (aarch64, kernel 6.10), 8 vCPUs, 4 GB RAM |
| Postgres | 16.15, data checksums on, `fsync` on, `synchronous_commit` on |
| banyan | `--max-running 8`, `shared_buffers=32MB` per instance, branches created with checkpoint and sync (the defaults) |
| Baseline | one Postgres 16.15 on the VM's own disk, default settings (`shared_buffers=128MB`) |
| Dataset | `accounts(id, balance, grp, filler)` with 200,000 rows (about 37 MiB as a database) unless stated |

## 1. Tree search: 1,000 steps of branch, mutate, evaluate, prune

Each step picks a promising node by UCB1, creates a branch from it, opens a
connection to the new branch, runs 50 updates and 50 inserts in one
transaction, scores the branch with two aggregate queries, and closes the
connection. Every 50 steps the 20 worst leaves are deleted. 600 branches are
alive at the end; the tree is 6 levels deep.

| | banyan | Postgres template copy |
|---|---:|---:|
| Steps completed | 1,000 of 1,000 | 1,000 of 1,000 |
| Wall time | **107.7 s** | 421.9 s |
| Storage added for the 600 live branches | **1,081 MiB** | 22,504 MiB |
| Create branch, p50 / p99 | **1.09 / 9.74 ms** | 300 / 1,194 ms |
| First connection to the new branch, p50 / p99 | 46.3 / 83.0 ms | **2.0 / 6.9 ms** |
| Mutate (100 statements), p50 | 21.1 ms | **11.2 ms** |
| Evaluate (2 aggregates), p50 | 33.3 ms | **25.9 ms** |
| Delete branch, p50 | **0.31 ms** | 13.9 ms |
| Whole step, p50 / p99 | **103 / 187 ms** | 343 / 1,397 ms |

What this shows: the search runs about 3.9 times faster end to end and uses
about 21 times less storage. What it also shows: once a branch exists, banyan
is the slower place to run queries. The first connection pays for starting a
Postgres instance (about 45 ms), and reads and writes go through FUSE with a
cold cache (about 1.3 to 1.9 times slower here).

Storage per branch is about 1.8 MiB. Roughly 0.4 MiB of that is what any
Postgres start writes (control file, WAL, statistics); the rest is the pages
the step changed plus their full-page images in the WAL.

## 2. Branch creation does not depend on database size

Ten branches created per size, each from an idle parent.

| Rows | Database size | banyan create, p50 | Template copy, p50 |
|---:|---:|---:|---:|
| 10,000 | 9 MiB | 0.85 ms | 161 ms |
| 100,000 | 23 MiB | 0.85 ms | 144 ms |
| 1,000,000 | 157 MiB | 1.12 ms | 334 ms |
| 3,000,000 | 456 MiB | 0.86 ms | 1,005 ms |

The banyan figure is one HTTP request including the durable sync. The fork
inside the store is a pointer copy and takes microseconds at any size.

First query on the new branch (which includes starting its Postgres) took
about 40 ms at every size on banyan, and 3 to 27 ms on the baseline.

## 3. Reads do not slow down with branch depth

A chain of 1,000 branches, each forked from the previous one, each adding a
row. At selected depths: time to first connection, 2,000 primary-key lookups,
and a full scan of both tables.

| Depth | First connection | Point query (average) | Full scan |
|---:|---:|---:|---:|
| 1 | 178 ms | 0.107 ms | 55 ms |
| 10 | 39 ms | 0.098 ms | 33 ms |
| 100 | 44 ms | 0.099 ms | 42 ms |
| 500 | 35 ms | 0.096 ms | 49 ms |
| 1,000 | 60 ms | 0.127 ms | 56 ms |

No trend with depth. The slow first connection at depth 1 is an outlier that
has not been investigated. The branch at depth 1,000 returns all
1,000 rows its ancestors added, which the benchmark asserts.

Branch creation in this chain has a p50 of 5.3 ms rather than 1 ms, because
every parent has just been written and is checkpointed before the fork.

## 4. Thousands of branches of one parent

| | |
|---|---:|
| Branches created from one parent | 5,000 |
| Total time | 4.8 s |
| Create, p50 / p99 | 0.50 / 4.79 ms |
| Storage added by 5,000 unused branches | 0.02 MiB |
| First connection to a random branch, p50 | 44.6 ms |
| Storage added by connecting and writing one row in 100 of them | 35.5 MiB (0.36 MiB each) |
| Delete, p50 | 0.05 ms |

## 5. Steady-state throughput on a single branch

This is the cost side. `pgbench` at scale 10 (about 150 MiB), 4 clients,
against one banyan branch through the proxy and against the baseline Postgres.

| | banyan branch | Vanilla Postgres |
|---|---:|---:|
| TPC-B-like, 30 s | 4,584 tps | **9,199 tps** |
| Select-only, 15 s | 40,794 tps | **72,828 tps** |

A branch delivers about half of vanilla throughput here. Every page read or
written crosses FUSE on a single thread, connections go through the proxy,
and the instance has a quarter of the baseline's shared buffers. If your workload is one long-lived database under steady load, run
plain Postgres.

## Reproducing

```bash
docker/dev.sh bash bench/run.sh
```

runs all of the above inside the development container (about 20 minutes; the
template baseline writes about 23 GB). `bench/run.sh banyan` runs only the
banyan side.
