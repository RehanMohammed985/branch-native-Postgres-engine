from __future__ import annotations

from contextlib import closing
from pathlib import Path

import psycopg2

from dblib.connector import Connector, DatabaseHandle
from dblib.target import PreparedTarget, SuiteRequest
from util.import_db import load_sql_file

from . import adapter as banyan
from .adapter import BanyanToolSuite


class Banyan(Connector):
    """Banyan copy-on-write Postgres branches: backend banyan."""

    environment = ("BANYAN_",)
    needs_psql = True
    version_sql = "SELECT current_setting('server_version')"
    root_ref = ""

    def scope(self) -> dict:
        return {"host": banyan.BANYAN_HOST, "port": banyan.BANYAN_PORT, "api": banyan.BANYAN_API,
                "template_branch": banyan.BANYAN_TEMPLATE_BRANCH}

    def preflight(self) -> None:
        banyan.api("GET", "/v1/status")

    @staticmethod
    def _validate_name(db: str) -> None:
        # Longer names lose the db__ ownership prefix when branch names are shortened.
        if len(db) > 30:
            raise ValueError("banyan db_name must contain at most 30 characters")

    def provision(self, db: str) -> DatabaseHandle:
        """A benchmark database is a fork of the empty template branch."""
        self._validate_name(db)
        banyan.api("POST", "/v1/branches", {"name": db, "from": banyan.BANYAN_TEMPLATE_BRANCH})
        return DatabaseHandle(db, db)

    def attach(self, db: str, existing: object) -> DatabaseHandle:
        self._validate_name(db)
        if existing.branch_id and existing.branch_id != db:
            raise ValueError("banyan existing_db supports only the root branch")
        return super().attach(db, existing)

    def load(self, database: DatabaseHandle, dump: Path) -> None:
        load_sql_file(BanyanToolSuite.get_initial_connection_uri(database.name), dump)

    def prepare_target(self, database: DatabaseHandle) -> PreparedTarget:
        def open_suite(request: SuiteRequest):
            suite = BanyanToolSuite.init_for_bench(
                request.result_collector,
                database.name,
                request.initial_ref,
                measure_storage=request.measure_storage,
            )
            if request.worker_index > 0 and request.setup_refs:
                suite.exec([], refs=[request.setup_refs[0]], timed=False)[0].raise_for_status()
            return suite

        return PreparedTarget(database.root_ref, BanyanToolSuite, open_suite)

    def delete(self, db: str) -> None:
        BanyanToolSuite.drop_run_databases(db)
        if self.exists(db):
            raise RuntimeError(f"banyan branches of '{db}' still exist after deletion")

    def exists(self, db: str) -> bool:
        return bool(banyan.run_branches(db))

    def version(self) -> str:
        daemon = banyan.api("GET", "/v1/status")["version"]
        uri = BanyanToolSuite.get_initial_connection_uri(banyan.BANYAN_TEMPLATE_BRANCH)
        with closing(psycopg2.connect(uri)) as conn, conn.cursor() as cur:
            cur.execute(self.version_sql)
            return f"banyan {daemon} (PostgreSQL {cur.fetchone()[0]})"


CONNECTOR = Banyan
