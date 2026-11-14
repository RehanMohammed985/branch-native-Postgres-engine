#!/usr/bin/env bash
# Run BranchBench against Banyan. Meant to run inside the banyan-branchbench
# image (see branchbench/README.md); from the repository root:
#
#   docker run --rm --device /dev/fuse --cap-add SYS_ADMIN --security-opt apparmor:unconfined \
#     -v "$PWD/branchbench":/adapter:ro -v "$PWD/bench-out/branchbench":/out \
#     banyan-branchbench /adapter/run.sh smoke
#
# Arguments: any of
#   smoke            the six *_mini scenarios
#   verbs            the branch-verb microbenchmark grid
#   <scenario>       one full scenario: rl_env context_mgmt multi_agent dev_agent ops_agent data_agent
#   full             all six full scenarios
set -uo pipefail
OUT=${OUT:-/out}
ENGINE=${ENGINE:-/opt/db-fork}
MAX_RUNTIME=${MAX_RUNTIME_SEC:-7200}
mkdir -p "$OUT"

# Install the adapter into the engine checkout.
rm -rf "$ENGINE/dblib/systems/banyan"
cp -R "$(dirname "$0")/banyan" "$ENGINE/dblib/systems/banyan"

banyan serve --max-running "${BANYAN_MAX_RUNNING:-16}" >"$OUT/daemon.log" 2>&1 &
DAEMON=$!
trap 'kill $DAEMON 2>/dev/null; wait $DAEMON 2>/dev/null' EXIT
for _ in $(seq 1 300); do banyan status >/dev/null 2>&1 && break; sleep 0.2; done
banyan status >/dev/null || { echo "banyan daemon did not start"; tail -20 "$OUT/daemon.log"; exit 1; }

cd "$ENGINE"
macro() { # macro <config name>
  local name=$1 dir="$OUT/$1"
  mkdir -p "$dir"
  echo "=== macrobench $name"
  local started=$SECONDS
  uv run --frozen python -m macrobench.runner \
    --config "macrobench/configs/$name.textproto" --backend banyan --db-name "bb_$name" \
    --outdir "$dir" --no-progress --max-runtime-sec "$MAX_RUNTIME" >"$dir/run.log" 2>&1
  echo "    exit $? after $((SECONDS - started)) s (log: $dir/run.log)"
  tail -3 "$dir/run.log" | sed 's/^/    /'
}
verbs() {
  mkdir -p "$OUT/verbs"
  echo "=== microbench verbs"
  uv run --frozen python -m microbench.verbs --backend banyan --db-name bb_verbs --scale 1 \
    --outdir "$OUT/verbs" --reps "${VERB_REPS:-10}" --warmup "${VERB_WARMUP:-3}" >"$OUT/verbs/run.log" 2>&1
  echo "    exit $? (log: $OUT/verbs/run.log)"
  tail -3 "$OUT/verbs/run.log" | sed 's/^/    /'
}

for what in "${@:-smoke}"; do
  case "$what" in
    smoke) for s in rl_env context_mgmt multi_agent dev_agent ops_agent data_agent; do macro "${s}_mini"; done ;;
    full)  for s in rl_env context_mgmt multi_agent dev_agent ops_agent data_agent; do macro "$s"; done ;;
    verbs) verbs ;;
    *)     macro "$what" ;;
  esac
done
banyan status
