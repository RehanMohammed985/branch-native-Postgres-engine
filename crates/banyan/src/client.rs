//! Minimal blocking HTTP/1.1 client for the daemon's local API.

use std::io::{Read, Write};
use std::net::TcpStream;
use std::time::Duration;

use anyhow::{anyhow, bail, Context, Result};
use serde::de::DeserializeOwned;

use crate::api_types::ApiError;

#[derive(Clone)]
pub struct Client {
    /// `host:port`
    addr: String,
}

impl Client {
    /// Accepts `host:port` or `http://host:port`.
    pub fn new(url: &str) -> Self {
        let addr = url.trim_start_matches("http://").trim_end_matches('/').to_string();
        Client { addr }
    }

    pub fn get<T: DeserializeOwned>(&self, path: &str) -> Result<T> {
        self.request("GET", path, None)
    }

    pub fn post<T: DeserializeOwned>(&self, path: &str, body: &impl serde::Serialize) -> Result<T> {
        self.request("POST", path, Some(serde_json::to_string(body)?))
    }

    pub fn delete<T: DeserializeOwned>(&self, path: &str) -> Result<T> {
        self.request("DELETE", path, None)
    }

    fn request<T: DeserializeOwned>(&self, method: &str, path: &str, body: Option<String>) -> Result<T> {
        let mut s = TcpStream::connect(&self.addr)
            .with_context(|| format!("cannot reach the banyan daemon at {} (is `banyan serve` running?)", self.addr))?;
        s.set_nodelay(true).ok();
        s.set_read_timeout(Some(Duration::from_secs(600))).ok();
        let body = body.unwrap_or_default();
        let req = format!(
            "{method} {path} HTTP/1.1\r\nHost: {}\r\nConnection: close\r\nContent-Type: application/json\r\nContent-Length: {}\r\n\r\n{body}",
            self.addr,
            body.len()
        );
        s.write_all(req.as_bytes())?;
        let mut raw = Vec::new();
        s.read_to_end(&mut raw)?;
        let text = String::from_utf8_lossy(&raw);
        let (head, payload) = text.split_once("\r\n\r\n").ok_or_else(|| anyhow!("malformed response from daemon"))?;
        let status: u16 = head.split_whitespace().nth(1).and_then(|c| c.parse().ok()).ok_or_else(|| anyhow!("malformed status line"))?;
        if !(200..300).contains(&status) {
            match serde_json::from_str::<ApiError>(payload) {
                Ok(e) => bail!("{}", e.error),
                Err(_) => bail!("daemon answered HTTP {status}: {payload}"),
            }
        }
        serde_json::from_str(payload).with_context(|| format!("unexpected response: {payload}"))
    }
}
