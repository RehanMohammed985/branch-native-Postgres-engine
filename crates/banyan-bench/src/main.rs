//! Branching workloads shaped like what coding and data agents do.
//!
//! Four workloads, each runnable against banyan or against a vanilla
//! Postgres baseline that "branches" with `CREATE DATABASE ... TEMPLATE`
//! (a full file copy, the only way to clone a database in stock Postgres):
//!
//! * `tree`: Monte-Carlo-tree-search style exploration. Pick a promising
//!   state, branch it, mutate the branch, score it, sometimes prune. This is
//!   the shape of BranchBench's MCTS workload.
//! * `create`: branch-creation latency as the database grows.
//! * `fanout`: thousands of branches of one parent, then work in a sample.
//! * `depth`: a chain of branches-of-branches, measuring whether reads slow
//!   down with depth.
//!
//! These are this project's own workloads. They are not the BranchBench
//! harness and the numbers are not BranchBench results.

use std::time::{Duration, Instant};

use anyhow::{bail, Context, Result};
use banyan::api_types::{CreateBranch, Created, Gc, Status};
use banyan::client::Client as Api;
use clap::{Parser, Subcommand, ValueEnum};
use postgres::{Client, NoTls};
use serde::Serialize;

#[derive(Parser)]
#[command(name = "banyan-bench", about = "Agent-style branching workloads")]
struct Cli {
    /// What to benchmark.
    #[arg(long, value_enum, default_value_t = Mode::Banyan, global = true)]
    mode: Mode,
    /// Postgres endpoint: the banyan proxy, or the vanilla server for the baseline.
    #[arg(long, default_value = "127.0.0.1", global = true)]
    host: String,
    #[arg(long, default_value_t = 5432, global = true)]
    port: u16,
    /// banyan control API (banyan mode only).
    #[arg(long, default_value = "127.0.0.1:8432", global = true)]
    api: String,
    /// Write a JSON report here.
    #[arg(long, global = true)]
    json: Option<String>,
    #[arg(long, default_value_t = 42, global = true)]
    seed: u64,
    #[command(subcommand)]
    workload: Workload,
}

#[derive(Clone, Copy, ValueEnum, PartialEq)]
enum Mode {
    /// banyan copy-on-write branches.
    Banyan,
    /// Vanilla Postgres, one database per branch via CREATE DATABASE ... TEMPLATE.
    Template,
}

#[derive(Subcommand)]
enum Workload {
    /// Tree search: branch, mutate, evaluate, prune.
    Tree {
        #[arg(long, default_value_t = 1000)]
        steps: usize,
        /// Rows in the accounts table.
        #[arg(long, default_value_t = 200_000)]
        rows: u64,
        /// Row updates per step.
        #[arg(long, default_value_t = 50)]
        updates: usize,
        /// Children per node before the search goes deeper.
        #[arg(long, default_value_t = 3)]
        width: usize,
        /// Prune this many of the worst leaves every 50 steps.
        #[arg(long, default_value_t = 20)]
        prune: usize,
        /// Stop early after this many seconds.
        #[arg(long, default_value_t = 7200)]
        timeout: u64,
    },
    /// Branch-creation latency versus database size.
    Create {
        /// Table sizes to test, in rows.
        #[arg(long, value_delimiter = ',', default_value = "10000,100000,1000000,4000000")]
        rows: Vec<u64>,
        /// Branches created per size.
        #[arg(long, default_value_t = 20)]
        samples: usize,
    },
    /// Many branches of one parent: create thousands, then use a sample.
    Fanout {
        #[arg(long, default_value_t = 5000)]
        branches: usize,
        /// Branches to connect to and write in after all are created.
        #[arg(long, default_value_t = 100)]
        sample: usize,
        #[arg(long, default_value_t = 200_000)]
        rows: u64,
    },
    /// Read latency along a chain of branches-of-branches.
    Depth {
        #[arg(long, default_value_t = 500)]
        depth: usize,
        #[arg(long, default_value_t = 200_000)]
        rows: u64,
    },
}

// ---- backends ---------------------------------------------------------------

trait Backend {
    /// Name of the branch that holds the initial data.
    fn root(&self) -> &'static str;
    fn prepare(&mut self) -> Result<()>;
    fn create(&mut self, name: &str, from: &str) -> Result<()>;
    fn delete(&mut self, name: &str) -> Result<()>;
    fn connect(&mut self, name: &str) -> Result<Client>;
    /// Bytes of storage in use by all branches.
    fn storage(&mut self) -> Result<u64>;
    /// Called after deletions so space accounting is comparable.
    fn reclaim(&mut self) -> Result<()>;
}

struct Banyan {
    api: Api,
    host: String,
    port: u16,
}

impl Backend for Banyan {
    fn root(&self) -> &'static str {
        "main"
    }
    fn prepare(&mut self) -> Result<()> {
        let _: Status = self.api.get("/v1/status")?;
        Ok(())
    }
    fn create(&mut self, name: &str, from: &str) -> Result<()> {
        let _: Created = self.api.post("/v1/branches", &CreateBranch { name: name.into(), from: from.into(), checkpoint: true, durable: true })?;
        Ok(())
    }
    fn delete(&mut self, name: &str) -> Result<()> {
        let _: serde_json::Value = self.api.delete(&format!("/v1/branches/{name}?force=true"))?;
        Ok(())
    }
    fn connect(&mut self, name: &str) -> Result<Client> {
        Client::connect(&format!("host={} port={} user=postgres dbname={name} connect_timeout=600", self.host, self.port), NoTls)
            .with_context(|| format!("connect to branch {name}"))
    }
    fn storage(&mut self) -> Result<u64> {
        let s: Status = self.api.get("/v1/status")?;
        Ok(s.used_bytes)
    }
    fn reclaim(&mut self) -> Result<()> {
        let _: Gc = self.api.post("/v1/gc", &serde_json::json!({}))?;
        Ok(())
    }
}

/// Vanilla Postgres: a branch is a database copied from a template database.
struct Template {
    host: String,
    port: u16,
    admin: Option<Client>,
}

impl Template {
    fn admin(&mut self) -> Result<&mut Client> {
        if self.admin.is_none() {
            self.admin = Some(Client::connect(&format!("host={} port={} user=postgres dbname=postgres", self.host, self.port), NoTls)?);
        }
        Ok(self.admin.as_mut().unwrap())
    }
}

impl Backend for Template {
    fn root(&self) -> &'static str {
        "root"
    }
    fn prepare(&mut self) -> Result<()> {
        let admin = self.admin()?;
        let stale: Vec<String> = admin
            .query("select datname from pg_database where datname = 'root' or datname like 'n%' or datname like 'c%' or datname like 'd%'", &[])?
            .iter()
            .map(|r| r.get(0))
            .filter(|n: &String| n != "postgres")
            .collect();
        for db in stale {
            admin.batch_execute(&format!("drop database if exists \"{db}\" with (force)"))?;
        }
        admin.batch_execute("create database root")?;
        Ok(())
    }
    fn create(&mut self, name: &str, from: &str) -> Result<()> {
        // Requires that nobody is connected to `from`; the workloads always
        // close their connection before branching.
        self.admin()?.batch_execute(&format!("create database \"{name}\" template \"{from}\" strategy = file_copy"))?;
        Ok(())
    }
    fn delete(&mut self, name: &str) -> Result<()> {
        self.admin()?.batch_execute(&format!("drop database \"{name}\" with (force)"))?;
        Ok(())
    }
    fn connect(&mut self, name: &str) -> Result<Client> {
        Ok(Client::connect(&format!("host={} port={} user=postgres dbname={name}", self.host, self.port), NoTls)?)
    }
    fn storage(&mut self) -> Result<u64> {
        let row = self.admin()?.query_one(
            "select coalesce(sum(pg_database_size(datname)), 0)::bigint from pg_database where datname not in ('postgres', 'template0', 'template1')",
            &[],
        )?;
        Ok(row.get::<_, i64>(0) as u64)
    }
    fn reclaim(&mut self) -> Result<()> {
        Ok(())
    }
}

// ---- helpers ----------------------------------------------------------------

struct Rng(u64);

impl Rng {
    fn next(&mut self) -> u64 {
        self.0 ^= self.0 << 13;
        self.0 ^= self.0 >> 7;
        self.0 ^= self.0 << 17;
        self.0
    }
    fn below(&mut self, n: u64) -> u64 {
        self.next() % n
    }
}

#[derive(Serialize, Clone, Default)]
struct Summary {
    count: usize,
    mean_ms: f64,
    p50_ms: f64,
    p95_ms: f64,
    p99_ms: f64,
    max_ms: f64,
}

fn summarize(samples: &[Duration]) -> Summary {
    if samples.is_empty() {
        return Summary::default();
    }
    let mut ms: Vec<f64> = samples.iter().map(|d| d.as_secs_f64() * 1e3).collect();
    ms.sort_by(|a, b| a.total_cmp(b));
    let at = |q: f64| ms[((ms.len() as f64 * q) as usize).min(ms.len() - 1)];
    Summary {
        count: ms.len(),
        mean_ms: ms.iter().sum::<f64>() / ms.len() as f64,
        p50_ms: at(0.50),
        p95_ms: at(0.95),
        p99_ms: at(0.99),
        max_ms: *ms.last().unwrap(),
    }
}

fn row(label: &str, s: &Summary) {
    println!("  {label:<18} {:>7}  {:>9.2}  {:>9.2}  {:>9.2}  {:>9.2}  {:>9.2}", s.count, s.mean_ms, s.p50_ms, s.p95_ms, s.p99_ms, s.max_ms);
}

fn header() {
    println!("  {:<18} {:>7}  {:>9}  {:>9}  {:>9}  {:>9}  {:>9}", "phase", "count", "mean ms", "p50 ms", "p95 ms", "p99 ms", "max ms");
}

fn mib(bytes: u64) -> f64 {
    bytes as f64 / 1048576.0
}

fn timed<T>(f: impl FnOnce() -> Result<T>) -> Result<(T, Duration)> {
    let t = Instant::now();
    let v = f()?;
    Ok((v, t.elapsed()))
}

/// Load the dataset into the root branch and leave it checkpointed and idle.
fn load(be: &mut dyn Backend, rows: u64) -> Result<()> {
    let mut c = be.connect(be.root())?;
    c.batch_execute(&format!(
        "drop table if exists moves; drop table if exists accounts;
         create table accounts (id int primary key, balance bigint not null, grp int not null, filler text not null);
         insert into accounts select g, 1000, g % 100, repeat('x', 80) from generate_series(1, {rows}) g;
         create table moves (id bigserial primary key, step int not null, acct int not null, delta bigint not null);"
    ))?;
    // Sent one at a time: VACUUM cannot run inside the implicit transaction
    // of a multi-statement query.
    c.batch_execute("vacuum analyze accounts")?;
    c.batch_execute("checkpoint")?;
    Ok(())
}

// ---- tree search ------------------------------------------------------------

struct Node {
    name: String,
    parent: Option<usize>,
    children: Vec<usize>,
    visits: f64,
    value: f64,
    alive: bool,
}

#[derive(Serialize)]
struct TreeReport {
    workload: &'static str,
    mode: String,
    rows: u64,
    updates_per_step: usize,
    steps_requested: usize,
    steps_completed: usize,
    wall_seconds: f64,
    timed_out: bool,
    branches_created: usize,
    branches_deleted: usize,
    branches_alive: usize,
    max_depth: usize,
    storage_start_mib: f64,
    storage_end_mib: f64,
    create: Summary,
    connect: Summary,
    mutate: Summary,
    evaluate: Summary,
    delete: Summary,
    step: Summary,
}

#[allow(clippy::too_many_arguments)]
fn tree(
    be: &mut dyn Backend,
    mode: &str,
    steps: usize,
    rows: u64,
    updates: usize,
    width: usize,
    prune: usize,
    timeout: u64,
    seed: u64,
) -> Result<TreeReport> {
    println!("loading {rows} rows into {}", be.root());
    load(be, rows)?;
    let storage_start = be.storage()?;
    let mut rng = Rng(seed | 1);
    let mut nodes = vec![Node { name: be.root().to_string(), parent: None, children: vec![], visits: 1.0, value: 0.0, alive: true }];
    let (mut t_create, mut t_connect, mut t_mutate, mut t_eval, mut t_delete, mut t_step) = (vec![], vec![], vec![], vec![], vec![], vec![]);
    let (mut deleted, mut max_depth, mut done) = (0, 0, 0);
    let started = Instant::now();
    let mut timed_out = false;

    for step in 1..=steps {
        if started.elapsed().as_secs() > timeout {
            timed_out = true;
            break;
        }
        let step_started = Instant::now();
        // Selection: walk down by UCB1 until a node with room for a child.
        let mut cur = 0;
        let mut depth = 0;
        loop {
            let kids: Vec<usize> = nodes[cur].children.iter().copied().filter(|k| nodes[*k].alive).collect();
            if kids.len() < width {
                break;
            }
            let total = nodes[cur].visits.ln();
            cur = *kids
                .iter()
                .max_by(|a, b| {
                    let score = |n: &Node| n.value / n.visits + 1.2 * (total / n.visits).sqrt();
                    score(&nodes[**a]).total_cmp(&score(&nodes[**b]))
                })
                .unwrap();
            depth += 1;
        }
        max_depth = max_depth.max(depth + 1);

        // Expansion: a new branch from the selected state.
        let name = format!("n{step}");
        let parent = nodes[cur].name.clone();
        let ((), d) = timed(|| be.create(&name, &parent))?;
        t_create.push(d);

        // Simulation: change the branch, then score it.
        let (mut conn, d) = timed(|| be.connect(&name))?;
        t_connect.push(d);
        let ((), d) = timed(|| {
            let mut tx = conn.transaction()?;
            for _ in 0..updates {
                let acct = 1 + rng.below(rows) as i32;
                let delta = rng.below(200) as i64 - 100;
                tx.execute("update accounts set balance = balance + $1 where id = $2", &[&delta, &acct])?;
                tx.execute("insert into moves (step, acct, delta) values ($1, $2, $3)", &[&(step as i32), &acct, &delta])?;
            }
            tx.commit()?;
            Ok(())
        })?;
        t_mutate.push(d);
        let (reward, d) = timed(|| {
            let r = conn.query_one(
                "select (select max(s) from (select sum(balance) s from accounts group by grp) g)::float8,
                        (select count(*) from moves)::float8",
                &[],
            )?;
            let (best, moves): (f64, f64) = (r.get(0), r.get(1));
            Ok((best / (rows as f64 * 10.0)) + moves * 1e-9)
        })?;
        t_eval.push(d);
        drop(conn);

        // Back-propagation.
        let id = nodes.len();
        nodes.push(Node { name, parent: Some(cur), children: vec![], visits: 1.0, value: reward, alive: true });
        nodes[cur].children.push(id);
        let mut up = Some(cur);
        while let Some(n) = up {
            nodes[n].visits += 1.0;
            nodes[n].value += reward;
            up = nodes[n].parent;
        }

        // Pruning: drop the least promising leaves.
        if prune > 0 && step % 50 == 0 {
            let mut leaves: Vec<usize> =
                (1..nodes.len()).filter(|i| nodes[*i].alive && nodes[*i].children.iter().all(|k| !nodes[*k].alive)).collect();
            leaves.sort_by(|a, b| (nodes[*a].value / nodes[*a].visits).total_cmp(&(nodes[*b].value / nodes[*b].visits)));
            for leaf in leaves.into_iter().take(prune) {
                let name = nodes[leaf].name.clone();
                let ((), d) = timed(|| be.delete(&name))?;
                t_delete.push(d);
                nodes[leaf].alive = false;
                deleted += 1;
            }
        }
        t_step.push(step_started.elapsed());
        done = step;
        if step % 100 == 0 {
            println!("  step {step:>5}  {:>7.1} s elapsed  {:>8.1} MiB in use", started.elapsed().as_secs_f64(), mib(be.storage()?));
        }
    }
    let wall = started.elapsed().as_secs_f64();
    be.reclaim()?;
    let storage_end = be.storage()?;
    let report = TreeReport {
        workload: "tree",
        mode: mode.to_string(),
        rows,
        updates_per_step: updates,
        steps_requested: steps,
        steps_completed: done,
        wall_seconds: wall,
        timed_out,
        branches_created: nodes.len() - 1,
        branches_deleted: deleted,
        branches_alive: nodes.iter().filter(|n| n.alive).count(),
        max_depth,
        storage_start_mib: mib(storage_start),
        storage_end_mib: mib(storage_end),
        create: summarize(&t_create),
        connect: summarize(&t_connect),
        mutate: summarize(&t_mutate),
        evaluate: summarize(&t_eval),
        delete: summarize(&t_delete),
        step: summarize(&t_step),
    };
    println!();
    println!("tree search ({mode}): {} of {} steps in {:.1} s{}", done, steps, wall, if timed_out { " (timed out)" } else { "" });
    println!("  branches: {} created, {} pruned, {} alive, deepest {} levels", report.branches_created, deleted, report.branches_alive, max_depth);
    println!(
        "  storage:  {:.1} MiB before, {:.1} MiB after ({:+.1} MiB for {} live branches)",
        report.storage_start_mib,
        report.storage_end_mib,
        report.storage_end_mib - report.storage_start_mib,
        report.branches_alive - 1
    );
    header();
    row("create branch", &report.create);
    row("first connect", &report.connect);
    row("mutate", &report.mutate);
    row("evaluate", &report.evaluate);
    row("delete branch", &report.delete);
    row("whole step", &report.step);
    Ok(report)
}

// ---- create scaling ---------------------------------------------------------

#[derive(Serialize)]
struct CreatePoint {
    rows: u64,
    database_mib: f64,
    create: Summary,
    first_query: Summary,
}

#[derive(Serialize)]
struct CreateReport {
    workload: &'static str,
    mode: String,
    points: Vec<CreatePoint>,
}

fn create(be: &mut dyn Backend, mode: &str, sizes: &[u64], samples: usize) -> Result<CreateReport> {
    let mut points = Vec::new();
    println!("  {:>10}  {:>12}  {:>14}  {:>14}  {:>18}", "rows", "database MiB", "create p50 ms", "create p99 ms", "first query p50 ms");
    for (i, rows) in sizes.iter().enumerate() {
        load(be, *rows)?;
        let size: i64 = be.connect(be.root())?.query_one("select pg_database_size(current_database())", &[])?.get(0);
        let (mut t_create, mut t_first) = (vec![], vec![]);
        for s in 0..samples {
            let name = format!("c{i}x{s}");
            let ((), d) = timed(|| be.create(&name, be.root()))?;
            t_create.push(d);
            let ((), d) = timed(|| {
                let n: i64 = be.connect(&name)?.query_one("select count(*) from accounts where id <= 100", &[])?.get(0);
                if n != 100 {
                    bail!("branch {name} is missing rows");
                }
                Ok(())
            })?;
            t_first.push(d);
            be.delete(&name)?;
        }
        be.reclaim()?;
        let p = CreatePoint { rows: *rows, database_mib: mib(size as u64), create: summarize(&t_create), first_query: summarize(&t_first) };
        println!("  {:>10}  {:>12.1}  {:>14.2}  {:>14.2}  {:>18.2}", p.rows, p.database_mib, p.create.p50_ms, p.create.p99_ms, p.first_query.p50_ms);
        points.push(p);
    }
    Ok(CreateReport { workload: "create", mode: mode.to_string(), points })
}

// ---- depth ------------------------------------------------------------------

#[derive(Serialize)]
struct DepthPoint {
    depth: usize,
    first_connect_ms: f64,
    point_query_ms: f64,
    full_scan_ms: f64,
    rows_seen: i64,
}

#[derive(Serialize)]
struct DepthReport {
    workload: &'static str,
    mode: String,
    rows: u64,
    create: Summary,
    points: Vec<DepthPoint>,
}

fn depth(be: &mut dyn Backend, mode: &str, levels: usize, rows: u64, seed: u64) -> Result<DepthReport> {
    load(be, rows)?;
    let mut rng = Rng(seed | 1);
    let mut probes: Vec<usize> = [1, 2, 5, 10, 25, 50, 100, 250, 500, 1000, 2000].into_iter().filter(|d| *d < levels).collect();
    probes.push(levels);
    let mut t_create = vec![];
    let mut points = vec![];
    let mut prev = be.root().to_string();
    println!("  {:>6}  {:>16}  {:>20}  {:>14}", "depth", "first connect ms", "point query ms (avg)", "full scan ms");
    for level in 1..=levels {
        let name = format!("d{level}");
        let ((), d) = timed(|| be.create(&name, &prev))?;
        t_create.push(d);
        let (mut conn, connect) = timed(|| be.connect(&name))?;
        // Every level leaves its own mark, so depth N must show N extra rows.
        conn.execute("insert into moves (step, acct, delta) values ($1, 1, 1)", &[&(level as i32)])?;
        if probes.contains(&level) {
            let stmt = conn.prepare("select balance from accounts where id = $1")?;
            for _ in 0..200 {
                conn.query_one(&stmt, &[&(1 + rng.below(rows) as i32)])?;
            }
            let t = Instant::now();
            for _ in 0..2000 {
                conn.query_one(&stmt, &[&(1 + rng.below(rows) as i32)])?;
            }
            let point = t.elapsed().as_secs_f64() * 1e3 / 2000.0;
            let t = Instant::now();
            let seen: i64 = conn.query_one("select count(*) from moves", &[])?.get(0);
            let _: i64 = conn.query_one("select sum(balance)::bigint from accounts", &[])?.get(0);
            let scan = t.elapsed().as_secs_f64() * 1e3;
            if seen != level as i64 {
                bail!("depth {level}: expected {level} rows from ancestors, found {seen}");
            }
            println!("  {:>6}  {:>16.1}  {:>20.4}  {:>14.1}", level, connect.as_secs_f64() * 1e3, point, scan);
            points.push(DepthPoint {
                depth: level,
                first_connect_ms: connect.as_secs_f64() * 1e3,
                point_query_ms: point,
                full_scan_ms: scan,
                rows_seen: seen,
            });
        }
        drop(conn);
        prev = name;
    }
    let report = DepthReport { workload: "depth", mode: mode.to_string(), rows, create: summarize(&t_create), points };
    header();
    row("create branch", &report.create);
    Ok(report)
}

// ---- fan-out ----------------------------------------------------------------

#[derive(Serialize)]
struct FanoutReport {
    workload: &'static str,
    mode: String,
    rows: u64,
    branches: usize,
    create_wall_seconds: f64,
    storage_before_mib: f64,
    storage_with_branches_mib: f64,
    storage_after_sample_mib: f64,
    create: Summary,
    first_connect: Summary,
    write: Summary,
    delete: Summary,
}

fn fanout(be: &mut dyn Backend, mode: &str, branches: usize, sample: usize, rows: u64, seed: u64) -> Result<FanoutReport> {
    load(be, rows)?;
    let before = be.storage()?;
    let mut rng = Rng(seed | 1);
    let mut t_create = vec![];
    let started = Instant::now();
    for i in 0..branches {
        let ((), d) = timed(|| be.create(&format!("f{i}"), be.root()))?;
        t_create.push(d);
    }
    let create_wall = started.elapsed().as_secs_f64();
    let with_branches = be.storage()?;
    let (mut t_connect, mut t_write) = (vec![], vec![]);
    for _ in 0..sample.min(branches) {
        let name = format!("f{}", rng.below(branches as u64));
        let (mut conn, d) = timed(|| be.connect(&name))?;
        t_connect.push(d);
        let ((), d) = timed(|| {
            conn.execute("update accounts set balance = balance + 1 where id = $1", &[&(1 + rng.below(rows) as i32)])?;
            Ok(())
        })?;
        t_write.push(d);
    }
    let after_sample = be.storage()?;
    let mut t_delete = vec![];
    for i in 0..branches {
        let ((), d) = timed(|| be.delete(&format!("f{i}")))?;
        t_delete.push(d);
    }
    be.reclaim()?;
    let report = FanoutReport {
        workload: "fanout",
        mode: mode.to_string(),
        rows,
        branches,
        create_wall_seconds: create_wall,
        storage_before_mib: mib(before),
        storage_with_branches_mib: mib(with_branches),
        storage_after_sample_mib: mib(after_sample),
        create: summarize(&t_create),
        first_connect: summarize(&t_connect),
        write: summarize(&t_write),
        delete: summarize(&t_delete),
    };
    println!("fan-out ({mode}): {branches} branches of one parent created in {create_wall:.1} s");
    println!(
        "  storage: {:.1} MiB before, {:.1} MiB with {branches} branches, {:.1} MiB after writing in {} of them",
        report.storage_before_mib,
        report.storage_with_branches_mib,
        report.storage_after_sample_mib,
        t_connect.len()
    );
    header();
    row("create branch", &report.create);
    row("first connect", &report.first_connect);
    row("first write", &report.write);
    row("delete branch", &report.delete);
    Ok(report)
}

fn main() -> Result<()> {
    let cli = Cli::parse();
    let (mut be, mode): (Box<dyn Backend>, &str) = match cli.mode {
        Mode::Banyan => (Box::new(Banyan { api: Api::new(&cli.api), host: cli.host.clone(), port: cli.port }), "banyan"),
        Mode::Template => (Box::new(Template { host: cli.host.clone(), port: cli.port, admin: None }), "postgres-template"),
    };
    be.prepare()?;
    let json = match cli.workload {
        Workload::Tree { steps, rows, updates, width, prune, timeout } => {
            serde_json::to_string_pretty(&tree(be.as_mut(), mode, steps, rows, updates, width, prune, timeout, cli.seed)?)?
        }
        Workload::Create { rows, samples } => serde_json::to_string_pretty(&create(be.as_mut(), mode, &rows, samples)?)?,
        Workload::Fanout { branches, sample, rows } => serde_json::to_string_pretty(&fanout(be.as_mut(), mode, branches, sample, rows, cli.seed)?)?,
        Workload::Depth { depth: levels, rows } => serde_json::to_string_pretty(&depth(be.as_mut(), mode, levels, rows, cli.seed)?)?,
    };
    if let Some(path) = cli.json {
        std::fs::write(&path, json).with_context(|| format!("write {path}"))?;
        println!("report written to {path}");
    }
    Ok(())
}
