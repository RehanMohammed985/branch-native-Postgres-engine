use std::net::SocketAddr;
use std::path::PathBuf;
use std::time::Duration;

use anyhow::Result;
use banyan::api_types::{Branch, CreateBranch, Created, Gc, Status};
use banyan::client::Client;
use banyan::manager::Config;
use clap::{Args, Parser, Subcommand};

#[derive(Parser)]
#[command(name = "banyan", version, about = "Branch-native Postgres: instant copy-on-write database branches")]
struct Cli {
    /// Address of a running daemon's control API (for every command except `serve`).
    #[arg(long, global = true, env = "BANYAN_API", default_value = "127.0.0.1:8432")]
    api_url: String,
    #[command(subcommand)]
    command: Cmd,
}

#[derive(Subcommand)]
enum Cmd {
    /// Run the daemon: mount the volume and serve Postgres connections.
    Serve(Serve),
    /// Create, list and delete branches.
    #[command(subcommand)]
    Branch(BranchCmd),
    /// Reclaim the space of deleted branches.
    Gc,
    /// Show volume and daemon status.
    Status,
}

#[derive(Args)]
struct Serve {
    /// Directory holding the volume (created on first start).
    #[arg(long, env = "BANYAN_DATA", default_value = "/var/lib/banyan")]
    data: PathBuf,
    /// Where to mount the branch filesystem.
    #[arg(long, env = "BANYAN_MOUNT", default_value = "/mnt/banyan")]
    mount: PathBuf,
    /// Address of the Postgres endpoint.
    #[arg(long, env = "BANYAN_LISTEN", default_value = "127.0.0.1:5432")]
    listen: SocketAddr,
    /// Address of the HTTP control API.
    #[arg(long, env = "BANYAN_API_LISTEN", default_value = "127.0.0.1:8432")]
    api: SocketAddr,
    /// Postgres instances allowed to run at once. Idle ones are stopped to
    /// make room; branches themselves are not limited.
    #[arg(long, env = "BANYAN_MAX_RUNNING", default_value_t = 8)]
    max_running: usize,
    /// Stop a branch's Postgres after this many seconds without clients.
    #[arg(long, default_value_t = 300)]
    idle_timeout: u64,
    /// Directory containing `postgres` and `initdb` (auto-detected if omitted).
    #[arg(long, env = "BANYAN_PG_BIN")]
    pg_bin: Option<PathBuf>,
    /// shared_buffers for each Postgres instance.
    #[arg(long, default_value = "32MB")]
    shared_buffers: String,
    /// max_connections for each Postgres instance.
    #[arg(long, default_value_t = 50)]
    max_connections: u32,
    /// Extra Postgres setting for every instance, as name=value. Repeatable.
    #[arg(long = "pg-option", value_name = "NAME=VALUE")]
    pg_options: Vec<String>,
    /// Database to use when a client names only a branch.
    #[arg(long, default_value = "postgres")]
    default_database: String,
    /// Directory for instance sockets and logs.
    #[arg(long, default_value = "/tmp/banyan")]
    run_dir: PathBuf,
}

#[derive(Subcommand)]
enum BranchCmd {
    /// Create a branch as an instant copy of another.
    Create {
        name: String,
        /// Branch to copy.
        #[arg(long, default_value = "main")]
        from: String,
        /// Skip the CHECKPOINT on the parent (faster create, slower first connect).
        #[arg(long)]
        no_checkpoint: bool,
        /// Answer before the new branch is synced to disk.
        #[arg(long)]
        no_sync: bool,
    },
    /// List branches.
    #[command(alias = "ls")]
    List,
    /// Delete a branch. Branches forked from it are unaffected.
    #[command(alias = "rm")]
    Delete {
        name: String,
        /// Disconnect clients that are still connected.
        #[arg(long)]
        force: bool,
    },
    /// Stop the branch's Postgres instance (it restarts on the next connection).
    Stop {
        name: String,
        #[arg(long)]
        force: bool,
    },
}

fn main() {
    env_logger::Builder::from_env(env_logger::Env::default().default_filter_or("info")).format_target(false).init();
    if let Err(e) = run(Cli::parse()) {
        eprintln!("error: {e:#}");
        std::process::exit(1);
    }
}

fn run(cli: Cli) -> Result<()> {
    let client = Client::new(&cli.api_url);
    match cli.command {
        Cmd::Serve(s) => banyan::server::run(Config {
            pg_bin: banyan::server::find_pg_bin(s.pg_bin)?,
            data_dir: s.data,
            mount_dir: s.mount,
            log_dir: s.run_dir.join("log"),
            run_dir: s.run_dir,
            listen: s.listen,
            api: s.api,
            max_running: s.max_running.max(1),
            idle_timeout: Duration::from_secs(s.idle_timeout),
            shared_buffers: s.shared_buffers,
            max_connections: s.max_connections,
            pg_options: s.pg_options,
            default_database: s.default_database,
        }),
        Cmd::Branch(BranchCmd::Create { name, from, no_checkpoint, no_sync }) => {
            let c: Created = client.post("/v1/branches", &CreateBranch { name, from, checkpoint: !no_checkpoint, durable: !no_sync })?;
            println!(
                "created branch {} from {} (fork {} us, checkpoint {:.1} ms, sync {:.1} ms)",
                c.name,
                c.parent,
                c.fork_micros,
                c.checkpoint_micros as f64 / 1e3,
                c.sync_micros as f64 / 1e3
            );
            println!("connect with: psql -h <host> -p <port> -U postgres -d {}", c.name);
            Ok(())
        }
        Cmd::Branch(BranchCmd::List) => {
            let branches: Vec<Branch> = client.get("/v1/branches")?;
            let width = branches.iter().map(|b| b.name.len()).max().unwrap_or(4).max(4);
            println!("{:width$}  {:width$}  {:8}  CLIENTS", "NAME", "PARENT", "COMPUTE");
            for b in branches {
                println!("{:width$}  {:width$}  {:8}  {}", b.name, b.parent.as_deref().unwrap_or("-"), b.compute, b.connections);
            }
            Ok(())
        }
        Cmd::Branch(BranchCmd::Delete { name, force }) => {
            let _: serde_json::Value = client.delete(&format!("/v1/branches/{name}?force={force}"))?;
            println!("deleted branch {name}");
            Ok(())
        }
        Cmd::Branch(BranchCmd::Stop { name, force }) => {
            let v: serde_json::Value = client.post(&format!("/v1/branches/{name}/stop?force={force}"), &serde_json::json!({}))?;
            println!("{}", if v["stopped"] == true { format!("stopped postgres for {name}") } else { format!("{name} was not running") });
            Ok(())
        }
        Cmd::Gc => {
            let g: Gc = client.post("/v1/gc", &serde_json::json!({}))?;
            println!("freed {} blocks in {} ms ({} live, {} free)", g.newly_freed, g.millis, g.live_blocks, g.free_blocks);
            Ok(())
        }
        Cmd::Status => {
            let s: Status = client.get("/v1/status")?;
            println!("banyan {}", s.version);
            println!("branches   {}", s.branches);
            println!("running    {} of {} postgres instances", s.running, s.max_running);
            println!("in use     {:.1} MiB", s.used_bytes as f64 / 1048576.0);
            println!("free       {:.1} MiB reusable", (s.free_blocks * s.block_size) as f64 / 1048576.0);
            println!("journal    {:.1} MiB", s.journal_bytes as f64 / 1048576.0);
            Ok(())
        }
    }
}
