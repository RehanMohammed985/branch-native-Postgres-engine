#!/usr/bin/env bash
# End-to-end test: a real daemon, a real FUSE mount, real Postgres.
#
#   tests/e2e.sh [path-to-banyan-binary]
#
# Needs Linux with FUSE (fusermount3) and PostgreSQL binaries, and must run
# as a non-root user. Exits non-zero on the first failed check.
set -euo pipefail

BANYAN=${1:-target/release/banyan}
WORK=${E2E_DIR:-$(mktemp -d /tmp/banyan-e2e.XXXXXX)}
PORT=${E2E_PORT:-55432}
API=127.0.0.1:${E2E_API_PORT:-58432}
export BANYAN_API=$API
# Prefer the real binaries over Debian's wrapper scripts when both exist.
PGBIN=$(ls -d /usr/lib/postgresql/*/bin 2>/dev/null | sort -V | tail -1)
[ -n "$PGBIN" ] || PGBIN=$(dirname "$(command -v psql)")
export PATH=$PGBIN:$PATH
DAEMON_PID=
mkdir -p "$WORK"

pass() { printf '  ok    %s\n' "$1"; }
failed=0
fail() { failed=1; printf '  FAIL  %s\n' "$1"; echo "--- daemon log"; tail -40 "$WORK/daemon.log" || true; exit 1; }
check() { # check <description> <expected> <actual>
  if [ "$2" = "$3" ]; then pass "$1"; else fail "$1: expected '$2', got '$3'"; fi
}
sql() { # sql <branch> <statement>
  psql "host=127.0.0.1 port=$PORT user=postgres dbname=$1" -v ON_ERROR_STOP=1 -qAtc "$2"
}
start() {
  "$BANYAN" serve --data "$WORK/vol" --mount "$WORK/mnt" --run-dir "$WORK/run" \
    --listen 127.0.0.1:$PORT --api $API --max-running 4 "$@" >>"$WORK/daemon.log" 2>&1 &
  DAEMON_PID=$!
  for _ in $(seq 1 600); do
    if "$BANYAN" status >/dev/null 2>&1; then return 0; fi
    kill -0 $DAEMON_PID 2>/dev/null || fail "daemon exited during startup"
    sleep 0.1
  done
  fail "daemon did not come up"
}
cleanup() {
  [ -n "$DAEMON_PID" ] && kill $DAEMON_PID 2>/dev/null && wait $DAEMON_PID 2>/dev/null || true
  fusermount3 -u -z "$WORK/mnt" 2>/dev/null || true
  [ -n "${E2E_KEEP:-}" ] || [ $failed = 1 ] || rm -rf "$WORK"
}
trap cleanup EXIT

echo "== start on an empty volume (initdb into branch main)"
start
check "main answers queries" "1" "$(sql main 'select 1')"
check "data checksums are on" "on" "$(sql main 'show data_checksums')"

echo "== branch isolation"
sql main "create table items(id int primary key, note text); insert into items select g, 'main' from generate_series(1,1000) g"
"$BANYAN" branch create dev >/dev/null
check "dev starts with main's rows" "1000" "$(sql dev 'select count(*) from items')"
sql dev "update items set note='dev' where id <= 10; insert into items values (1001,'dev'); create table only_dev(x int)"
sql main "delete from items where id > 990"
check "main does not see dev's update" "0" "$(sql main "select count(*) from items where note='dev'")"
check "main does not see dev's table" "f" "$(sql main "select to_regclass('only_dev') is not null")"
check "dev does not see main's delete" "1001" "$(sql dev 'select count(*) from items')"
check "main sees its own delete" "990" "$(sql main 'select count(*) from items')"
check "unknown branch is refused cleanly" "1" "$(sql nope 'select 1' 2>&1 | grep -c 'does not exist')"
check "database@branch routing" "1001" "$(sql postgres@dev 'select count(*) from items')"

echo "== branch of a branch, 25 levels deep"
prev=dev
for i in $(seq 1 25); do
  "$BANYAN" branch create "chain-$i" --from "$prev" >/dev/null
  sql "chain-$i" "insert into items values ($((2000 + i)), 'chain-$i')"
  prev="chain-$i"
done
check "deepest branch has every ancestor's row" "1026" "$(sql chain-25 'select count(*) from items')"
check "a middle branch has only its ancestors' rows" "1011" "$(sql chain-10 'select count(*) from items')"
check "dev is unchanged by its descendants" "1001" "$(sql dev 'select count(*) from items')"

echo "== 200 branches from one parent"
t0=$(date +%s%N)
for i in $(seq 1 200); do "$BANYAN" branch create "fan-$i" --from main --no-sync >/dev/null; done
t1=$(date +%s%N)
echo "        200 branches via the CLI in $(( (t1 - t0) / 1000000 )) ms"
check "all branches are listed" "227" "$("$BANYAN" branch list | tail -n +2 | wc -l | tr -d ' ')"
for i in 7 77 177; do sql "fan-$i" "insert into items values (9000 + $i, 'fan')"; done
check "fan branches are writable and independent" "991 991 990" "$(sql fan-7 'select count(*) from items') $(sql fan-177 'select count(*) from items') $(sql fan-100 'select count(*) from items')"
running=$("$BANYAN" status | awk '/^running/ {print $2}')
[ "$running" -le 4 ] && pass "at most --max-running instances run ($running)" || fail "$running instances running, limit is 4"

echo "== concurrent load on two branches"
pgbench -h 127.0.0.1 -p $PORT -U postgres -i -s 2 -q main >/dev/null 2>&1
"$BANYAN" branch create bench >/dev/null
pgbench -h 127.0.0.1 -p $PORT -U postgres -T 5 -c 4 -j 2 main >"$WORK/pgbench-main.log" 2>&1 &
pgbench -h 127.0.0.1 -p $PORT -U postgres -T 5 -c 4 -j 2 bench >"$WORK/pgbench-bench.log" 2>&1
wait %2 2>/dev/null || wait
check "pgbench on main had no failed transactions" "1" "$(grep -c 'number of failed transactions: 0 ' "$WORK/pgbench-main.log")"
check "pgbench on bench had no failed transactions" "1" "$(grep -c 'number of failed transactions: 0 ' "$WORK/pgbench-bench.log")"
echo "        main:  $(grep '^tps' "$WORK/pgbench-main.log")"
echo "        bench: $(grep '^tps' "$WORK/pgbench-bench.log")"
check "pgbench invariant holds on main" "t" "$(sql main 'select (select sum(abalance) from pgbench_accounts) = (select sum(delta) from pgbench_history)')"
check "pgbench invariant holds on bench" "t" "$(sql bench 'select (select sum(abalance) from pgbench_accounts) = (select sum(delta) from pgbench_history)')"
"$BANYAN" branch create bench-snap --from bench >/dev/null
check "a branch taken after load passes the invariant" "t" "$(sql bench-snap 'select (select sum(abalance) from pgbench_accounts) = (select sum(delta) from pgbench_history)')"
sql bench-snap "create extension if not exists amcheck"
# bt_index_check raises an error on a corrupt index; verify_heapam returns one row per problem.
check "every btree index verifies against its heap" "t" "$(sql bench-snap "select count(*) > 0 from (select bt_index_check(c.oid, true) from pg_index i join pg_class c on c.oid = i.indexrelid join pg_am am on am.oid = c.relam where am.amname = 'btree' and i.indisvalid) s")"
check "heap verification finds no corruption" "0" "$(sql bench-snap "select count(*) from pg_class c, lateral verify_heapam(c.oid) v where c.relkind = 'r'")"

echo "== delete and reclaim"
before=$("$BANYAN" status | awk '/^in use/ {print $3}')
"$BANYAN" branch delete bench >/dev/null
"$BANYAN" branch delete bench-snap >/dev/null
for i in $(seq 1 200); do "$BANYAN" branch delete "fan-$i" >/dev/null; done
"$BANYAN" gc >/dev/null
after=$("$BANYAN" status | awk '/^in use/ {print $3}')
echo "        in use: $before MiB -> $after MiB"
awk "BEGIN { exit !($after < $before) }" && pass "gc reclaimed space" || fail "gc reclaimed nothing"
check "deleting a parent keeps its children" "1026" "$("$BANYAN" branch delete dev >/dev/null; sql chain-25 'select count(*) from items')"
check "remaining branches are intact after gc" "990" "$(sql main 'select count(*) from items')"

echo "== kill -9 the daemon, then restart"
sql main "insert into items values (5000, 'before-crash')"
"$BANYAN" branch create survivor --from main >/dev/null
kill -9 $DAEMON_PID; wait $DAEMON_PID 2>/dev/null || true
sleep 1
check "no postgres process outlives the daemon" "0" "$(pgrep -f "$WORK/mnt" | wc -l | tr -d ' ')"
start
check "committed row survives the crash" "before-crash" "$(sql main 'select note from items where id = 5000')"
check "branch created before the crash survives" "991" "$(sql survivor 'select count(*) from items')"
check "deep branch survives the crash" "1026" "$(sql chain-25 'select count(*) from items')"
"$BANYAN" branch create after-crash --from chain-25 >/dev/null
check "new branches work after recovery" "1026" "$(sql after-crash 'select count(*) from items')"

echo "== clean shutdown and restart"
kill $DAEMON_PID; wait $DAEMON_PID 2>/dev/null || true
start
check "data is intact after a clean restart" "991" "$(sql main 'select count(*) from items')"

echo
echo "all end-to-end checks passed"
