//! The Postgres endpoint.
//!
//! Clients connect here with any Postgres driver. The database name in the
//! startup packet selects the branch:
//!
//! * `feature-x`        -> branch `feature-x`, default database
//! * `shop@feature-x`   -> branch `feature-x`, database `shop`
//!
//! The proxy makes sure the branch has a running Postgres instance, rewrites
//! the database name, and from then on copies bytes in both directions
//! without looking at them.

use std::sync::Arc;

use anyhow::Result;
use tokio::io::AsyncWriteExt;
use tokio::net::{TcpListener, TcpStream, UnixStream};

use crate::manager::{Manager, Refusal};
use crate::pgwire;

pub async fn serve(manager: Arc<Manager>, listener: TcpListener) {
    loop {
        let (client, peer) = match listener.accept().await {
            Ok(c) => c,
            Err(e) => {
                log::warn!("accept failed: {e}");
                tokio::time::sleep(std::time::Duration::from_millis(50)).await;
                continue;
            }
        };
        let manager = manager.clone();
        tokio::spawn(async move {
            if let Err(e) = handle(manager, client).await {
                log::debug!("connection from {peer} ended: {e:#}");
            }
        });
    }
}

/// Split a client's database name into `(database, branch)`.
pub fn route(requested: &str, default_database: &str) -> (String, String) {
    match requested.rsplit_once('@') {
        Some((db, branch)) if !db.is_empty() => (db.to_string(), branch.to_string()),
        _ => (default_database.to_string(), requested.trim_start_matches('@').to_string()),
    }
}

async fn handle(manager: Arc<Manager>, mut client: TcpStream) -> Result<()> {
    client.set_nodelay(true).ok();
    let body = loop {
        let body = pgwire::read_startup(&mut client).await?;
        match u32::from_be_bytes(body[..4].try_into().unwrap()) {
            // No TLS or GSS encryption on this endpoint: tell the client to
            // carry on in clear text.
            pgwire::SSL_REQUEST | pgwire::GSSENC_REQUEST => client.write_all(b"N").await?,
            // Cancel requests arrive on their own connection and cannot be
            // routed without tracking backend keys. Not supported yet.
            pgwire::CANCEL_REQUEST => return Ok(()),
            pgwire::PROTOCOL_3 => break body,
            other => {
                let msg = format!("unsupported frontend protocol {}.{}", other >> 16, other & 0xffff);
                return pgwire::send(&mut client, &pgwire::error_response("0A000", &msg)).await;
            }
        }
    };
    let mut params = pgwire::startup_params(&body);
    let user = params.iter().find(|(k, _)| k == "user").map(|(_, v)| v.clone()).unwrap_or_default();
    let requested = params.iter().find(|(k, _)| k == "database").map(|(_, v)| v.clone()).unwrap_or(user);
    let (database, branch) = route(&requested, &manager.cfg.default_database);

    let (socket, _guard) = match manager.connect(&branch).await {
        Ok(ok) => ok,
        Err(e) => {
            let code = if e.downcast_ref::<Refusal>().is_some() { "3D000" } else { "57P03" };
            log::warn!("cannot serve branch {branch}: {e:#}");
            return pgwire::send(&mut client, &pgwire::error_response(code, &format!("{e:#}"))).await;
        }
    };
    params.retain(|(k, _)| k != "database");
    params.push(("database".to_string(), database));
    let mut upstream = UnixStream::connect(&socket).await?;
    upstream.write_all(&pgwire::startup_packet(&params)).await?;
    tokio::io::copy_bidirectional(&mut client, &mut upstream).await?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::route;

    #[test]
    fn routing() {
        assert_eq!(route("main", "postgres"), ("postgres".into(), "main".into()));
        assert_eq!(route("shop@feature-x", "postgres"), ("shop".into(), "feature-x".into()));
        assert_eq!(route("@dev", "postgres"), ("postgres".into(), "dev".into()));
        assert_eq!(route("a@b@c", "postgres"), ("a@b".into(), "c".into()));
    }
}
