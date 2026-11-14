"""Render downloaded BranchBench summaries as Markdown tables.

    gh run download <run id> --dir results      # one folder per scenario
    python3 branchbench/report.py results > tables.md

Needs only the standard library: it reads the ``summary.json`` files that
``summarize.py`` wrote inside each artifact.
"""

import glob
import json
import os
import sys

ORDER = ["rl_env", "context_mgmt", "multi_agent", "dev_agent", "ops_agent", "data_agent"]
TITLES = {"rl_env": "S1 RL environment", "context_mgmt": "S2 context management", "multi_agent": "S3 multi-agent",
          "dev_agent": "S4 dev agent", "ops_agent": "S5 ops agent", "data_agent": "S6 data agent"}
VERBS = ["BRANCH", "CONNECT", "COMMIT", "DIFF", "MERGE", "REBASE", "REVERT", "RESET", "DELETE"]
SQL = ["READ", "INSERT", "UPDATE", "DELETE_ROWS", "DDL"]


def load(root):
    macro, verbs = {}, None
    for path in sorted(glob.glob(os.path.join(root, "*", "summary.json"))):
        for r in json.load(open(path)):
            if r.get("scenario") == "verbs":
                verbs = r
            else:
                macro[r["dir"]] = r
    return macro, verbs


def ms(v):
    if v is None:
        return ""
    return f"{v:,.0f}" if v >= 100 else f"{v:.1f}" if v >= 10 else f"{v:.2f}"


def mib(b):
    return "" if not b else f"{b / 1048576:,.0f}"


def total(r, status):
    return sum(c.get(status, 0) for c in r["counts"].values())


def main():
    macro, verbs = load(sys.argv[1])
    names = [n for n in ORDER if n in macro] + sorted(n for n in macro if n not in ORDER)
    print("| Scenario | Outcome | Wall time | Units of work | Operations ok / failed / unsupported | Invariants | Storage before, after |")
    print("|---|---|---:|---:|---:|---:|---:|")
    for n in names:
        r = macro[n]
        outcome = "timed out" if r.get("timed_out") else r["status"]
        w = r.get("work") or {}
        inv = r.get("invariants") or {}
        print(f"| {TITLES.get(n, n)} | {outcome} | {r['elapsed_sec']:,.0f} s | {w.get('succeeded')} of {w.get('expected')} "
              f"| {total(r, 'OK'):,} / {total(r, 'FAILED'):,} / {total(r, 'UNSUPPORTED'):,} "
              f"| {inv.get('passed')} of {inv.get('checked')} | {mib(r.get('storage_before_bytes'))}, {mib(r.get('storage_after_bytes'))} MiB |")

    for title, ops in (("Branch verbs", VERBS), ("SQL statements", SQL)):
        print(f"\n**{title}: count, p50 / p95 latency in ms**\n")
        present = [o for o in ops if any(o in macro[n]["latency"] for n in names)]
        print("| Scenario | " + " | ".join(o.replace("_", " ").title() for o in present) + " |")
        print("|---|" + "---:|" * len(present))
        for n in names:
            cells = []
            for o in present:
                q = macro[n]["latency"].get(o)
                cells.append(f"{q['n']:,}: {ms(q['p50_ms'])} / {ms(q['p95_ms'])}" if q else "")
            print(f"| {TITLES.get(n, n)} | " + " | ".join(cells) + " |")

    print("\n**Distinct errors**\n")
    any_error = False
    for n in names:
        for e in macro[n]["errors"]:
            any_error = True
            print(f"- {TITLES.get(n, n)}, {e['op']} x{e['count']}: `{e['error']}`")
        for i in macro[n].get("invariants_failed") or []:
            any_error = True
            print(f"- {TITLES.get(n, n)}, INVARIANT FAILED: {i.get('name')}: {i.get('detail')}")
    if not any_error:
        print("None.")

    if verbs:
        print(f"\n**Verb microbenchmark** ({verbs.get('status')}, {len(verbs['points'])} points)\n")
        print("| Point | Status | Reps | p50 ms | p90 ms | max ms |")
        print("|---|---|---:|---:|---:|---:|")
        for p in verbs["points"]:
            print(f"| `{p['id']}` | {p['status']} | {p['n'] or 0} | {ms(p['p50_ms'])} | {ms(p['p90_ms'])} | {ms(p['max_ms'])} |")


if __name__ == "__main__":
    main()
