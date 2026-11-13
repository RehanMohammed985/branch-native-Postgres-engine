//! The small part of the Postgres wire protocol the daemon needs: reading a
//! client's startup packet, and running a statement on a local instance.

use std::path::Path;

use anyhow::{anyhow, bail, Context, Result};
use tokio::io::{AsyncRead, AsyncReadExt, AsyncWrite, AsyncWriteExt};
use tokio::net::UnixStream;

pub const PROTOCOL_3: u32 = 196608;
pub const SSL_REQUEST: u32 = 80877103;
pub const GSSENC_REQUEST: u32 = 80877104;
pub const CANCEL_REQUEST: u32 = 80877102;

/// First packet of a connection: a length-prefixed body without a type byte.
pub async fn read_startup<S: AsyncRead + Unpin>(s: &mut S) -> Result<Vec<u8>> {
    let len = s.read_u32().await? as usize;
    if !(8..=16 * 1024).contains(&len) {
        bail!("bad startup packet length {len}");
    }
    let mut body = vec![0u8; len - 4];
    s.read_exact(&mut body).await?;
    Ok(body)
}

/// Parse the `key\0value\0...\0` section of a protocol 3.0 startup packet.
pub fn startup_params(body: &[u8]) -> Vec<(String, String)> {
    let mut parts = body[4..].split(|b| *b == 0).map(|p| String::from_utf8_lossy(p).into_owned());
    let mut out = Vec::new();
    while let (Some(k), Some(v)) = (parts.next(), parts.next()) {
        if k.is_empty() {
            break;
        }
        out.push((k, v));
    }
    out
}

pub fn startup_packet(params: &[(String, String)]) -> Vec<u8> {
    let mut body = PROTOCOL_3.to_be_bytes().to_vec();
    for (k, v) in params {
        body.extend_from_slice(k.as_bytes());
        body.push(0);
        body.extend_from_slice(v.as_bytes());
        body.push(0);
    }
    body.push(0);
    let mut out = ((body.len() + 4) as u32).to_be_bytes().to_vec();
    out.extend_from_slice(&body);
    out
}

/// A FATAL ErrorResponse, for refusing a client before it reaches Postgres.
pub fn error_response(code: &str, message: &str) -> Vec<u8> {
    let mut body = Vec::new();
    for (field, value) in [(b'S', "FATAL"), (b'V', "FATAL"), (b'C', code), (b'M', message)] {
        body.push(field);
        body.extend_from_slice(value.as_bytes());
        body.push(0);
    }
    body.push(0);
    let mut out = vec![b'E'];
    out.extend_from_slice(&((body.len() + 4) as u32).to_be_bytes());
    out.extend_from_slice(&body);
    out
}

async fn read_message<S: AsyncRead + Unpin>(s: &mut S) -> Result<(u8, Vec<u8>)> {
    let kind = s.read_u8().await?;
    let len = s.read_u32().await? as usize;
    if !(4..=1 << 20).contains(&len) {
        bail!("bad message length {len}");
    }
    let mut body = vec![0u8; len - 4];
    s.read_exact(&mut body).await?;
    Ok((kind, body))
}

fn error_field(body: &[u8], field: u8) -> Option<String> {
    body.split(|b| *b == 0).find(|f| f.first() == Some(&field)).map(|f| String::from_utf8_lossy(&f[1..]).into_owned())
}

async fn start_session(socket: &Path, database: &str) -> Result<UnixStream> {
    let mut s = UnixStream::connect(socket).await?;
    let params = [("user", "postgres"), ("database", database), ("application_name", "banyan")].map(|(k, v)| (k.to_string(), v.to_string()));
    s.write_all(&startup_packet(&params)).await?;
    Ok(s)
}

pub enum Probe {
    Ready,
    Starting,
}

/// Is the instance behind `socket` accepting connections yet?
pub async fn probe(socket: &Path) -> Probe {
    let Ok(mut s) = start_session(socket, "postgres").await else { return Probe::Starting };
    match read_message(&mut s).await {
        // 57P03 = cannot_connect_now: still starting up or in recovery.
        Ok((b'E', body)) if error_field(&body, b'C').as_deref() == Some("57P03") => Probe::Starting,
        Ok(_) => {
            let _ = s.write_all(&[b'X', 0, 0, 0, 4]).await;
            Probe::Ready
        }
        Err(_) => Probe::Starting,
    }
}

async fn until_ready<S: AsyncRead + Unpin>(s: &mut S) -> Result<()> {
    let mut error = None;
    loop {
        match read_message(s).await? {
            (b'Z', _) => return error.map_or(Ok(()), Err),
            (b'E', body) => error = Some(anyhow!(error_field(&body, b'M').unwrap_or_else(|| "server error".into()))),
            (b'R', body) if body.len() >= 4 && body[..4] != [0, 0, 0, 0] => {
                bail!("instance asks for a password; internal connections need trust authentication on the local socket")
            }
            _ => {}
        }
    }
}

/// Run one statement over the instance's local socket and wait for it to finish.
pub async fn simple_query(socket: &Path, database: &str, sql: &str) -> Result<()> {
    let mut s = start_session(socket, database).await.context("connect to instance")?;
    until_ready(&mut s).await.context("start session")?;
    let mut msg = vec![b'Q'];
    msg.extend_from_slice(&((sql.len() + 5) as u32).to_be_bytes());
    msg.extend_from_slice(sql.as_bytes());
    msg.push(0);
    s.write_all(&msg).await?;
    until_ready(&mut s).await.with_context(|| format!("run {sql}"))?;
    let _ = s.write_all(&[b'X', 0, 0, 0, 4]).await;
    Ok(())
}

pub async fn send<S: AsyncWrite + Unpin>(s: &mut S, bytes: &[u8]) -> Result<()> {
    s.write_all(bytes).await?;
    Ok(s.flush().await?)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn startup_round_trip() {
        let params = vec![("user".to_string(), "alice".to_string()), ("database".to_string(), "app@dev".to_string())];
        let packet = startup_packet(&params);
        assert_eq!(u32::from_be_bytes(packet[..4].try_into().unwrap()) as usize, packet.len());
        assert_eq!(startup_params(&packet[4..]), params);
    }

    #[test]
    fn error_response_is_well_formed() {
        let e = error_response("3D000", "branch \"x\" does not exist");
        assert_eq!(e[0], b'E');
        assert_eq!(u32::from_be_bytes(e[1..5].try_into().unwrap()) as usize, e.len() - 1);
        assert_eq!(error_field(&e[5..], b'C').as_deref(), Some("3D000"));
        assert_eq!(error_field(&e[5..], b'M').as_deref(), Some("branch \"x\" does not exist"));
    }
}
