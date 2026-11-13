#!/usr/bin/env bash
# Reproduce the numbers in docs/BENCHMARKS.md.
#
#   docker/dev.sh bash bench/run.sh            # everything (about 20 minutes, ~25 GB of disk)
#   docker/dev.sh bash bench/run.sh banyan     # only the banyan runs (about 6 minutes)
#   docker/dev.sh bash bench/run.sh pgbench    # only the pgbench comparison
#
# Runs a banyan daemon and a vanilla Postgres side by side in the dev
# container and writes JSON reports to bench-out/.
set -euo pipefail
WHAT=${1:-all}
SRC=$(cd "$(dirname "$0")/.." && pwd)

if [ "$(id -u)" = 0 ]; then
  cd "$SRC"
  cargo build --release -p banyan -p banyan-bench
  install -m 755 "${CARGO_TARGET_DIR:-target}"/release/banyan "${CARGO_TARGET_DIR:-target}"/release/banyan-bench /usr/local/bin/
  mkdir -p /work "$SRC/bench-out" && chown postgres:postgres /work "$SRC/bench-out"
  exec su postgres -c "bash '$SRC/bench/run.sh' $WHAT"
fi

export PATH=$(ls -d /usr/lib/postgresql/*/bin | sort -V | tail -1):/usr/local/bin:$PATH
W=/work/bench
OUT=$SRC/bench-out
rm -rf "$W" && mkdir -p "$W"

banyan serve --data "$W/vol" --mount "$W/mnt" --run-dir "$W/run" --max-running 8 >"$W/daemon.log" 2>&1 &
DAEMON=$!
for _ in $(seq 1 300); do banyan status >/dev/null 2>&1 && break; sleep 0.1; done
initdb -D "$W/vanilla" -U postgres --auth=trust --data-checksums >/dev/null
pg_ctl -D "$W/vanilla" -o "-p 5433 -k $W -c listen_addresses=127.0.0.1" -l "$W/vanilla.log" -w start >/dev/null
trap 'kill $DAEMON 2>/dev/null; wait $DAEMON 2>/dev/null; pg_ctl -D "$W/vanilla" -m fast stop >/dev/null 2>&1' EXIT

run() { echo; echo "### $*"; "$@"; }

if [ "$WHAT" = all ] || [ "$WHAT" = banyan ]; then
  run banyan-bench --json "$OUT/tree-banyan.json" tree --steps 1000 --rows 200000
  run banyan-bench --json "$OUT/depth-banyan.json" depth --depth 1000 --rows 200000
  run banyan-bench --json "$OUT/fanout-banyan.json" fanout --branches 5000 --sample 100 --rows 200000
  run banyan-bench --json "$OUT/create-banyan.json" create --rows 10000,100000,1000000,3000000 --samples 10
fi
if [ "$WHAT" = all ]; then
  run banyan-bench --mode template --port 5433 --json "$OUT/create-template.json" create --rows 10000,100000,1000000,3000000 --samples 10
  run banyan-bench --mode template --port 5433 --json "$OUT/tree-template.json" tree --steps 1000 --rows 200000
fi
if [ "$WHAT" = all ] || [ "$WHAT" = pgbench ]; then
  # Steady-state OLTP on one branch versus the same Postgres on a plain disk.
  banyan branch create pgbench >/dev/null
  for target in "5432 pgbench banyan" "5433 postgres vanilla"; do
    set -- $target
    pgbench -h 127.0.0.1 -p "$1" -U postgres -i -s 10 -q "$2" >/dev/null 2>&1
    echo; echo "### pgbench, scale 10, 4 clients, 30 s: $3"
    pgbench -h 127.0.0.1 -p "$1" -U postgres -T 30 -c 4 -j 2 "$2" 2>&1 | grep -E "latency average|^tps|failed"
    echo "### pgbench select-only, 4 clients, 15 s: $3"
    pgbench -h 127.0.0.1 -p "$1" -U postgres -T 15 -c 4 -j 2 -S "$2" 2>&1 | grep -E "latency average|^tps"
  done
fi
