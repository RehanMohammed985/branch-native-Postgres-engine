//! JSON shapes shared by the HTTP API, the CLI and the benchmark tool.

use serde::{Deserialize, Serialize};

#[derive(Serialize, Deserialize, Debug, Clone)]
pub struct Branch {
    pub name: String,
    pub id: u32,
    pub parent: Option<String>,
    /// Seconds since the Unix epoch.
    pub created: f64,
    /// `running` or `stopped`.
    pub compute: String,
    pub connections: usize,
}

#[derive(Serialize, Deserialize, Debug, Clone)]
pub struct CreateBranch {
    pub name: String,
    #[serde(default = "default_from")]
    pub from: String,
    /// Run CHECKPOINT on the parent first, so the new branch starts without
    /// replaying WAL. Skipping it makes creation faster and the first
    /// connection slower.
    #[serde(default = "yes")]
    pub checkpoint: bool,
    /// Wait until the new branch is on disk before answering.
    #[serde(default = "yes")]
    pub durable: bool,
}

fn default_from() -> String {
    "main".into()
}

fn yes() -> bool {
    true
}

#[derive(Serialize, Deserialize, Debug, Clone)]
pub struct Created {
    pub name: String,
    pub id: u32,
    pub parent: String,
    /// Time spent in the copy-on-write fork itself.
    pub fork_micros: u64,
    pub checkpoint_micros: u64,
    pub sync_micros: u64,
}

#[derive(Serialize, Deserialize, Debug, Clone)]
pub struct Status {
    pub version: String,
    pub branches: usize,
    pub running: usize,
    pub max_running: usize,
    pub block_size: u64,
    pub allocated_blocks: u64,
    pub free_blocks: u64,
    pub used_bytes: u64,
    pub journal_bytes: u64,
}

#[derive(Serialize, Deserialize, Debug, Clone)]
pub struct Gc {
    pub live_blocks: u64,
    pub free_blocks: u64,
    pub newly_freed: u64,
    pub millis: u64,
}

#[derive(Serialize, Deserialize, Debug, Clone)]
pub struct ApiError {
    pub error: String,
}
