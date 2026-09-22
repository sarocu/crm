//! Runtime configuration, all from the environment.
//!
//! heyo passes the listen port on the command line to the VM and injects
//! secrets from HeyoSecret, so nothing here is ever baked into an image.

use std::net::SocketAddr;

use anyhow::{Context, Result, bail};

#[derive(Debug, Clone)]
pub struct Config {
    pub bind: SocketAddr,
    /// Accepted bearer tokens, each with the name recorded as the actor on
    /// anything written with it. Empty only when the operator explicitly
    /// opted out of auth.
    pub tokens: Vec<(String, String)>,
    pub allow_anonymous: bool,
    /// Hostnames the Streamable HTTP transport will answer on.
    ///
    /// rmcp defends against DNS rebinding by refusing any `Host` it does not
    /// recognise, and its default list is loopback only — right for a server
    /// on a laptop, fatal behind a load balancer, which forwards the public
    /// hostname and gets `Forbidden: Host header is not allowed`. Empty
    /// means "keep rmcp's default"; `*` disables the check entirely.
    pub allowed_hosts: Vec<String>,
}

impl Config {
    pub fn from_env() -> Result<Self> {
        let port: u16 = match std::env::var("PORT") {
            Ok(p) => p
                .parse()
                .with_context(|| format!("PORT={p:?} is not a port number"))?,
            Err(_) => 8080,
        };
        let host = std::env::var("BIND_HOST").unwrap_or_else(|_| "0.0.0.0".into());
        let bind: SocketAddr = format!("{host}:{port}")
            .parse()
            .with_context(|| format!("cannot parse bind address {host}:{port}"))?;

        let tokens = parse_tokens(
            std::env::var("MCP_TOKENS").ok().as_deref(),
            std::env::var("MCP_AUTH_TOKEN").ok().as_deref(),
        )?;
        let allow_anonymous = matches!(
            std::env::var("MCP_ALLOW_ANONYMOUS").as_deref(),
            Ok("1") | Ok("true") | Ok("yes")
        );

        // Fail closed. This endpoint can write to the CRM once it is
        // deployed, so an unset token must stop the server rather than
        // quietly hand the pipeline to anyone who finds it.
        if tokens.is_empty() && !allow_anonymous {
            bail!(
                "no MCP token is set. Set MCP_TOKENS=name:secret,... (or MCP_AUTH_TOKEN for a \
                 single agent), or set MCP_ALLOW_ANONYMOUS=1 to deliberately serve this \
                 endpoint without auth."
            );
        }
        if !tokens.is_empty() && allow_anonymous {
            tracing::warn!("MCP_ALLOW_ANONYMOUS is set, so MCP tokens will not be enforced");
        }

        // Comma-separated, `host` or `host:port`, e.g.
        // `MCP_ALLOWED_HOSTS=crm-mcp.us2.heyo.work`.
        let allowed_hosts: Vec<String> = std::env::var("MCP_ALLOWED_HOSTS")
            .unwrap_or_default()
            .split(',')
            .map(str::trim)
            .filter(|s| !s.is_empty())
            .map(str::to_string)
            .collect();

        Ok(Self {
            bind,
            tokens,
            allow_anonymous,
            allowed_hosts,
        })
    }

    pub fn requires_auth(&self) -> bool {
        !self.allow_anonymous && !self.tokens.is_empty()
    }
}

/// `MCP_TOKENS=bdr-agent:s3cret,sam:other` plus the older single
/// `MCP_AUTH_TOKEN`, which is recorded as the actor `agent`.
fn parse_tokens(list: Option<&str>, single: Option<&str>) -> Result<Vec<(String, String)>> {
    let mut out = Vec::new();
    for entry in list
        .unwrap_or_default()
        .split(',')
        .map(str::trim)
        .filter(|s| !s.is_empty())
    {
        let (name, secret) = entry
            .split_once(':')
            .with_context(|| "MCP_TOKENS entries must be name:secret".to_string())?;
        let (name, secret) = (name.trim(), secret.trim());
        if name.is_empty() || secret.is_empty() {
            bail!("MCP_TOKENS has an entry with an empty name or secret");
        }
        out.push((name.to_string(), secret.to_string()));
    }
    if let Some(t) = single.map(str::trim).filter(|t| !t.is_empty()) {
        out.push(("agent".to_string(), t.to_string()));
    }
    Ok(out)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn tokens_parse_from_both_variables() {
        let t = parse_tokens(Some("bdr:abc, sam:def"), Some("xyz")).unwrap();
        assert_eq!(
            t,
            vec![
                ("bdr".to_string(), "abc".to_string()),
                ("sam".to_string(), "def".to_string()),
                ("agent".to_string(), "xyz".to_string()),
            ]
        );
        assert!(parse_tokens(Some("nocolon"), None).is_err());
        assert!(parse_tokens(Some(":secret"), None).is_err());
        assert!(parse_tokens(None, Some("  ")).unwrap().is_empty());
    }
}
