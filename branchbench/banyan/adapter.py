"""Banyan backend for BranchBench: a branch is a Banyan branch.

Banyan (https://github.com/RehanMohammed985/branch-native-Postgres-engine)
runs an unmodified Postgres on each copy-on-write branch behind one endpoint,
where the database name in the connection string selects the branch. Its one
branching primitive is an O(1) fork of a whole branch, exposed over HTTP.

That is the same shape as the ``file_copy`` backend, whose one primitive is
``CREATE DATABASE ... TEMPLATE``: one Postgres per ref, no native commit,
diff or merge. This adapter therefore subclasses ``FileCopyToolSuite`` and
replaces only the primitive:

    clone a database     POST /v1/branches  {"name": target, "from": template}
    drop a database      DELETE /v1/branches/<name>
    list databases       GET /v1/branches
    storage              GET /v1/status (physical bytes, shared blocks once)

Everything built on top (the ``_bb_commits`` log, snapshots for commits and
merge bases, SQL three-way merge and diff over postgres_fdw) is the harness's
own machinery, unchanged, so the simulated verbs do the same work here as on
plain Postgres and differ only in what a clone costs.

Differences from ``file_copy`` that follow from the primitive:

* A fork needs no exclusive access to its source, so the source's
  connections are not parked for a branch or a commit.
* Snapshots are not marked read-only. On Banyan a fork inherits everything,
  including per-database settings, so a branch reset from a read-only
  snapshot would itself be read-only. Nothing writes to snapshots.

Environment: ``BANYAN_HOST`` / ``BANYAN_PORT`` (the Postgres endpoint,
default 127.0.0.1:5432), ``BANYAN_API`` (the control API, default
127.0.0.1:8432), ``BANYAN_TEMPLATE_BRANCH`` (the empty branch new benchmark
databases are forked from, default ``main``).

Importing this module points the shared ``file_copy`` plumbing at the Banyan
endpoint, so the two backends cannot be used in one process.
"""

import json
import os
import re
import urllib.error
import urllib.request

from dblib.systems.file_copy import adapter as fc

BANYAN_HOST = os.environ.get("BANYAN_HOST", "127.0.0.1")
BANYAN_PORT = int(os.environ.get("BANYAN_PORT", "5432"))
BANYAN_API = os.environ.get("BANYAN_API", "127.0.0.1:8432").removeprefix("http://").rstrip("/")
BANYAN_TEMPLATE_BRANCH = os.environ.get("BANYAN_TEMPLATE_BRANCH", "main")

# ``fc._uri(db)`` and the postgres_fdw server options read these at call time.
fc.PGSQL_HOST = BANYAN_HOST
fc.PGSQL_PORT = BANYAN_PORT
fc.PGSQL_USER = "postgres"
fc.PGSQL_PASSWORD = ""
fc.PGSQL_DATA_DIR = ""

_DROP = re.compile(r'^DROP DATABASE (?:IF EXISTS )?"((?:[^"]|"")+)"(?: WITH \(FORCE\))?$')
_LIST = "SELECT datname FROM pg_database WHERE datname LIKE %s"


class BanyanAPIError(RuntimeError):
    def __init__(self, status: int, message: str):
        super().__init__(f"banyan API {status}: {message}")
        self.status = status


def api(method: str, path: str, body: dict = None):
    """One request to the Banyan control API; returns the decoded JSON."""
    data = json.dumps(body).encode() if body is not None else None
    req = urllib.request.Request(f"http://{BANYAN_API}{path}", data=data, method=method,
                                 headers={"content-type": "application/json"})
    try:
        with urllib.request.urlopen(req, timeout=600) as resp:
            return json.loads(resp.read() or b"null")
    except urllib.error.HTTPError as e:
        text = e.read().decode(errors="replace")
        try:
            text = json.loads(text).get("error", text)
        except ValueError:
            pass
        raise BanyanAPIError(e.code, text) from None


def branch_names() -> list:
    return [b["name"] for b in api("GET", "/v1/branches")]


def run_branches(db_name: str) -> list:
    """The run's root branch and everything derived from it, longest first."""
    names = [n for n in branch_names() if n == db_name or n.startswith(db_name + "__")]
    return sorted(names, key=len, reverse=True)


class BanyanToolSuite(fc.FileCopyToolSuite):
    BACKEND_NAME = "banyan"
    # Storage is the volume's physical usage: every branch, shared blocks once.
    STORAGE_SCOPE = "server"
    IMPLEMENTATION = {
        "commit": "simulated",
        "diff": "simulated",
        "log": "simulated",
        "merge": "simulated",
        "rebase": "simulated",
        "revert": "simulated",
        "reset": "simulated",
        "commit_refs": "simulated",
        "multi_ref_exec": "simulated",
    }
    IMPLEMENTATION_NOTES = {
        "branch": "one O(1) copy-on-write fork of the parent branch (after a CHECKPOINT on the "
                  "parent if it has written since its last one), plus, for a fork from a branch "
                  "head, a second fork of the child kept as the merge base, and a _bb_commits row. "
                  "The parent's connections stay open",
        "commit": "_bb_commits row, then an O(1) fork of the branch as the commit's snapshot",
        "log": "SELECT from the _bb_commits bookkeeping table",
        "diff": "the harness's SQL diff (dblib/pg_merge.py): per-table row hashes per key bucket "
                "on each branch's Postgres, differing buckets pulled over postgres_fdw",
        "merge": "the harness's SQL three-way merge on the target branch; base = the fork or "
                 "rebase snapshot, theirs read over postgres_fdw",
        "rebase": "a fresh fork of the upstream becomes the branch's base and is merged in "
                  "three-way; commits are not replayed",
        "revert": "inverse of (predecessor snapshot -> commit snapshot) applied through "
                  "postgres_fdw, recorded as a commit",
        "reset": "delete the branch, then fork it again from the commit's snapshot",
        "delete": "delete the branch and the snapshots only it references; space is reclaimed "
                  "by Banyan's garbage collection",
        "commit_refs": "a commit ref connects to the commit's snapshot branch",
        "multi_ref_exec": "postgres_fdw: db.table(ref, t) imports the other branch's table "
                          "through the Banyan endpoint",
    }

    # ------------------------------------------------------------------
    # The primitive: fork, drop and list branches through the control API
    # ------------------------------------------------------------------

    def _clone(self, template: str, target: str, exclusive: bool = True) -> None:
        if exclusive:
            # A fork carries the parent's planner statistics, as a file copy
            # does: analyze never-analyzed tables first, like file_copy.
            self._analyze_unanalyzed(template)
        self._check_cancel()
        try:
            api("POST", "/v1/branches", {"name": target, "from": template})
        except BanyanAPIError as e:
            if e.status == 409:
                raise fc.psycopg2.errors.DuplicateDatabase(str(e)) from None
            raise

    def _mark_snapshot(self, db: str) -> None:
        """Not marked read-only: a fork would inherit the setting."""

    def _admin(self, sql: str, args=None):
        """file_copy runs server-wide statements on the ``postgres``
        database. The ones left after the overrides above map to the API."""
        statement = sql.strip().rstrip(";")
        dropped = _DROP.match(statement)
        if dropped:
            name = dropped.group(1).replace('""', '"')
            try:
                api("DELETE", f"/v1/branches/{name}?force=true")
            except BanyanAPIError as e:
                if not (e.status == 404 and "IF EXISTS" in statement):
                    raise
            return None
        if statement == _LIST:
            # The pattern is "<escaped prefix>%": list the branches under it.
            prefix = args[0][:-1].replace("\\_", "_")
            return [(n,) for n in branch_names() if n.startswith(prefix)]
        if statement.upper() == "CHECKPOINT":
            api("POST", "/v1/gc", {})
            return None
        raise RuntimeError(f"banyan adapter: no mapping for admin statement: {statement[:80]}")

    # ------------------------------------------------------------------
    # A fork does not need the source's connections closed
    # ------------------------------------------------------------------

    def _park_self(self) -> None:
        if getattr(self, "_forking", False):
            return
        super()._park_self()

    def _branch_impl(self, name, from_ref) -> None:
        self._forking = True
        try:
            super()._branch_impl(name, from_ref)
        finally:
            self._forking = False

    def _commit_impl(self, ref, message: str) -> str:
        self._forking = True
        try:
            return super()._commit_impl(ref, message)
        finally:
            self._forking = False

    # ------------------------------------------------------------------
    # Storage
    # ------------------------------------------------------------------

    def _storage_bytes(self) -> int:
        # Blocks of deleted branches count as used until a collection runs.
        api("POST", "/v1/gc", {})
        return int(api("GET", "/v1/status")["used_bytes"])

    def _gc_impl(self) -> None:
        api("POST", "/v1/gc", {})

    # ------------------------------------------------------------------
    # Run-level helpers
    # ------------------------------------------------------------------

    @classmethod
    def drop_run_databases(cls, db_name: str) -> int:
        names = run_branches(db_name)
        for name in names:
            try:
                api("DELETE", f"/v1/branches/{name}?force=true")
            except BanyanAPIError as e:
                if e.status != 404:
                    raise
        if names:
            api("POST", "/v1/gc", {})
        with fc._REG.cond:
            fc._REG.branches.pop(db_name, None)
        return len(names)
