"""Summarise BranchBench result files: per scenario, the outcome, the latency
of each branch verb, and every distinct error.

    uv run --frozen python /adapter/summarize.py /out [--json /out/summary.json] [--check]

``--check`` exits non-zero if a run did not complete, timed out, failed an
invariant, hit an unsupported verb or lost a worker.

Reads the ``*_e2e_stats.json`` and ``*.parquet`` files the engine writes.
"""

import collections
import glob
import json
import os
import statistics
import sys

import pyarrow.parquet as pq

VERBS = ["BRANCH", "CONNECT", "COMMIT", "DIFF", "LOG", "MERGE", "REBASE", "REVERT", "RESET", "DELETE"]
SQL = ["READ", "INSERT", "UPDATE", "DELETE_ROWS", "DDL"]


def quantiles(xs):
    xs = sorted(xs)
    at = lambda q: xs[min(len(xs) - 1, int(len(xs) * q))]
    return {"n": len(xs), "mean_ms": statistics.fmean(xs) * 1e3, "p50_ms": at(0.50) * 1e3,
            "p95_ms": at(0.95) * 1e3, "p99_ms": at(0.99) * 1e3, "max_ms": xs[-1] * 1e3}


def summarise(stats_path):
    stats = json.load(open(stats_path))
    table = pq.read_table(stats_path.replace("_e2e_stats.json", ".parquet"),
                          columns=["op_name", "status", "latency", "error_message"])
    ops, status, latency, error = (table.column(c).to_pylist() for c in table.schema.names)
    lat = collections.defaultdict(list)
    counts = collections.defaultdict(collections.Counter)
    errors = collections.Counter()
    for op, st, l, err in zip(ops, status, latency, error):
        counts[op][st] += 1
        if st == "OK":
            lat[op].append(l)
        elif err:
            errors[(op, " ".join(err.split())[:140])] += 1
    storage = stats.get("storage") or {}
    return {
        "scenario": stats["scenario"],
        "run_id": stats["run_id"],
        "status": stats["status"],
        "timed_out": stats.get("timed_out"),
        "stop_reason": stats.get("stop_reason"),
        "elapsed_sec": stats["elapsed_sec"],
        "workflow_supported": stats.get("workflow_supported"),
        "unsupported_ops": stats.get("unsupported_ops"),
        "work": stats.get("work"),
        "invariants": {k: v for k, v in (stats.get("invariants") or {}).items() if k != "results"},
        "invariants_failed": [i for i in (stats.get("invariants") or {}).get("results", []) if not i.get("passed")],
        "metrics": stats.get("metrics"),
        "stalls": len(stats.get("stalls") or []),
        "worker_errors": len(stats.get("worker_errors") or []),
        "storage_before_bytes": stats.get("storage_before_bytes"),
        "storage_after_bytes": stats.get("storage_after_bytes"),
        "storage_peak_bytes": storage.get("peak_bytes") if isinstance(storage, dict) else None,
        "rows": table.num_rows,
        "counts": {op: dict(c) for op, c in counts.items()},
        "latency": {op: quantiles(v) for op, v in lat.items() if v},
        "errors": [{"op": op, "error": e, "count": n} for (op, e), n in errors.most_common(15)],
    }


def summarise_verbs(path):
    """The verb microbenchmark writes one stats file with a list of points."""
    stats = json.load(open(path))
    points = []
    for p in stats.get("points", []):
        lat = p.get("latency") or {}
        points.append({
            "id": p.get("id"),
            "status": p.get("status"),
            "error": " ".join(str(p.get("error") or "").split())[:140],
            "n": lat.get("n"),
            "p50_ms": lat["p50"] * 1e3 if "p50" in lat else None,
            "p90_ms": lat["p90"] * 1e3 if "p90" in lat else None,
            "max_ms": lat["max"] * 1e3 if "max" in lat else None,
            "storage_p50_bytes": (p.get("storage") or {}).get("p50"),
        })
    return {"scenario": "verbs", "run_id": stats.get("run_id"), "status": stats.get("status"),
            "implementation": stats.get("implementation"), "setup": stats.get("setup"), "points": points}


def main():
    root = sys.argv[1]
    out = sys.argv[sys.argv.index("--json") + 1] if "--json" in sys.argv else None
    check = "--check" in sys.argv
    problems = []
    results = []
    for path in sorted(glob.glob(os.path.join(root, "verbs", "*_stats.json"))):
        v = summarise_verbs(path)
        v["dir"] = "verbs"
        results.append(v)
        bad = [p for p in v["points"] if p["status"] != "ok"]
        print(f"\n## verbs: {v['status']}; {len(v['points'])} points, {len(bad)} not ok")
        print(f"   {'point':<34}{'status':<14}{'reps':>5}{'p50 ms':>10}{'p90 ms':>10}{'max ms':>10}")
        for p in v["points"]:
            line = f"   {str(p['id']):<34}{str(p['status']):<14}{p['n'] or 0:>5}"
            if p["p50_ms"] is not None:
                line += f"{p['p50_ms']:>10.2f}{p['p90_ms']:>10.2f}{p['max_ms']:>10.1f}"
            print(line + (f"  {p['error']}" if p["error"] else ""))
        if v["status"] != "completed" or bad:
            problems.append(f"verbs: status {v['status']}, {len(bad)} points not ok")
    for path in sorted(glob.glob(os.path.join(root, "*", "*_e2e_stats.json"))):
        r = summarise(path)
        r["dir"] = os.path.basename(os.path.dirname(path))
        results.append(r)
        ok = sum(c.get("OK", 0) for c in r["counts"].values())
        failed = sum(c.get("FAILED", 0) for c in r["counts"].values())
        unsupported = sum(c.get("UNSUPPORTED", 0) for c in r["counts"].values())
        print(f"\n## {r['dir']}: {r['status']} in {r['elapsed_sec']:.1f} s"
              f"{' (TIMED OUT)' if r['timed_out'] else ''}; work {r['work']}; "
              f"{ok} ops ok, {failed} failed, {unsupported} unsupported; "
              f"stalls {r['stalls']}, worker errors {r['worker_errors']}")
        inv = r["invariants"]
        print(f"   invariants: {inv.get('passed')} of {inv.get('checked')} passed")
        for i in r["invariants_failed"]:
            print(f"   INVARIANT FAILED: {i.get('name')}: {i.get('detail')}")
        print(f"   {'op':<12}{'ok':>8}{'failed':>8}{'p50 ms':>10}{'p95 ms':>10}{'p99 ms':>10}{'max ms':>10}")
        for op in VERBS + SQL + ["EXEC"]:
            if op not in r["counts"]:
                continue
            c, q = r["counts"][op], r["latency"].get(op)
            line = f"   {op:<12}{c.get('OK', 0):>8}{c.get('FAILED', 0) + c.get('UNSUPPORTED', 0):>8}"
            if q:
                line += f"{q['p50_ms']:>10.2f}{q['p95_ms']:>10.2f}{q['p99_ms']:>10.2f}{q['max_ms']:>10.1f}"
            print(line)
        for e in r["errors"]:
            print(f"   error x{e['count']} [{e['op']}] {e['error']}")
        if r["status"] != "completed" or r["timed_out"]:
            problems.append(f"{r['dir']}: status {r['status']}, timed_out {r['timed_out']}")
        if r["invariants_failed"] or r["unsupported_ops"] or r["worker_errors"]:
            problems.append(f"{r['dir']}: {len(r['invariants_failed'])} invariants failed, "
                            f"unsupported {r['unsupported_ops']}, worker errors {r['worker_errors']}")
    if out:
        json.dump(results, open(out, "w"), indent=1)
        print(f"\nsummary written to {out}")
    if not results:
        problems.append("no result files found")
    if check and problems:
        print("\nPROBLEMS:\n  " + "\n  ".join(problems))
        sys.exit(1)


if __name__ == "__main__":
    main()
