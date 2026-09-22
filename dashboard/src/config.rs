//! Dashboard configuration.
//!
//! There is deliberately no authentication here: the dashboard is read-only
//! and the load balancer in front of `ADMIN_PORT` owns access control.

use std::net::SocketAddr;

use anyhow::{Context, Result};

#[derive(Debug, Clone)]
pub struct Config {
    pub admin_bind: SocketAddr,
    /// Where to read the indexer's own health and per-source metrics.
    pub bot_health_url: String,
}

impl Config {
    pub fn from_env() -> Result<Self> {
        let host = std::env::var("BIND_HOST").unwrap_or_else(|_| "0.0.0.0".into());
        let admin_port: u16 = match std::env::var("ADMIN_PORT") {
            Ok(v) => v
                .trim()
                .parse()
                .map_err(|e| anyhow::anyhow!("ADMIN_PORT={v:?} is not a port: {e}"))?,
            Err(_) => 8090,
        };
        Ok(Self {
            admin_bind: format!("{host}:{admin_port}")
                .parse()
                .with_context(|| format!("cannot parse {host}:{admin_port}"))?,
            bot_health_url: std::env::var("BOT_HEALTH_URL")
                .unwrap_or_else(|_| "http://bot:8081/healthz".into()),
        })
    }
}
