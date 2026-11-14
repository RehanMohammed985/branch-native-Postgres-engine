# BranchBench on Banyan

This directory runs the official [BranchBench](https://github.com/ElaineAng/db-fork)
engine against Banyan. The engine is fetched at a pinned commit when the image
is built; none of its code is in this repository.

| File | What it is |
|---|---|
| `banyan/adapter.py`, `banyan/connector.py`, `banyan/system.json` | The Banyan integration, in the layout the engine expects under `dblib/systems/banyan/` |
| `Dockerfile` | The engine, its Python environment and a Banyan daemon in one image |
| `run.sh` | Starts the daemon, installs the adapter into the engine, runs scenarios |
| `summarize.py` | Turns the engine's result files into tables and a JSON summary |

## Running it

On GitHub (no load on your machine): Actions, **BranchBench**, *Run workflow*,
or

```bash
gh workflow run branchbench.yml -f scenarios="rl_env context_mgmt multi_agent dev_agent ops_agent data_agent verbs"
```

Each scenario runs as its own job and uploads its result files as an
artifact. Pull requests that touch this directory run the six `*_mini`
scenarios.

Locally, on Linux with Docker (a full run starts dozens of Postgres
instances; give it at least 8 GB of free memory):

```bash
docker build -t banyan .
docker build -t banyan-branchbench branchbench
mkdir -p out && chmod 777 out
docker run --rm --device /dev/fuse --cap-add SYS_ADMIN --security-opt apparmor:unconfined \
  -v "$PWD/branchbench":/adapter:ro -v "$PWD/out":/out \
  banyan-branchbench /adapter/run.sh smoke
```

## How the adapter works

BranchBench drives every database through a git-like API: `branch`,
`commit`, `diff`, `log`, `merge`, `rebase`, `revert`, `reset`, `delete`,
plus SQL on a ref. Banyan has one branching primitive, an O(1) fork of a
whole branch, and no notion of commits or merges.

That is the same situation as the engine's own plain-Postgres backend
(`file_copy`), whose one primitive is `CREATE DATABASE ... TEMPLATE`. That
backend builds everything else on top of the primitive: a commit is a
snapshot clone plus a row in a bookkeeping table, and diff, merge, rebase and
revert are SQL run over `postgres_fdw` between the Postgres instances of two
refs.

The Banyan adapter subclasses that backend and replaces only the primitive:

| `file_copy` | Banyan |
|---|---|
| `CREATE DATABASE x TEMPLATE y` | `POST /v1/branches {"name": x, "from": y}` |
| `DROP DATABASE x` | `DELETE /v1/branches/x` |
| list databases | `GET /v1/branches` |
| volume usage | `GET /v1/status` after a garbage collection |

So the verbs the engine reports as **simulated** (commit, diff, log, merge,
rebase, revert, reset, commit refs, multi-ref exec) do the same SQL work on
Banyan as on plain Postgres, and differ only in what a clone costs. `branch`
and `delete` are native. With `DBFORK_NATIVE_ONLY=1` the engine treats the
simulated verbs as unsupported.

Two deliberate differences from `file_copy`:

- A Banyan fork does not need exclusive access to its source, so connections
  to the source are left open during a branch or a commit.
- Snapshots are not marked read-only, because a Banyan fork inherits
  per-database settings and a branch reset from a read-only snapshot would
  be read-only too.

## Reading the results

- "Completed" is not enough. `summarize.py --check` also requires that no
  invariant failed, no verb was unsupported and no worker died.
- Some statements fail by design: the scenarios provoke schema-conflict
  rebases and probe for columns a bad deploy dropped. The summary lists every
  distinct error so these can be told apart from real faults.
- The numbers published with the BranchBench paper come from an earlier
  version of the engine with different workloads (its 1,000-step MCTS
  workload is not in the current engine) and from different hardware. They
  cannot be compared with results from this version.
