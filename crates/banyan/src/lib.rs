//! Banyan: branch-native Postgres.
//!
//! See the repository README for what this is and how to run it. The crate
//! is split into:
//!
//! * [`manager`] - branches, and the Postgres instance attached to each
//! * [`proxy`]   - the Postgres endpoint that routes by branch name
//! * [`api`]     - the HTTP control API
//! * [`client`]  - a small client for that API, used by the CLI

pub mod api;
pub mod api_types;
pub mod client;
pub mod manager;
pub mod pgwire;
pub mod proxy;
pub mod server;
