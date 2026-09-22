//! Bearer-token gate for the MCP endpoint, and who is calling.

use std::sync::Arc;

use axum::body::Body;
use axum::extract::State;
use axum::http::{Request, StatusCode, header};
use axum::middleware::Next;
use axum::response::{IntoResponse, Response};
use subtle::ConstantTimeEq;

use crate::config::Config;

/// The name of the token a request authenticated with. Recorded as
/// `updated_by` / `actor` on everything the request writes, so the activity
/// log says which agent (or person) did what.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Actor(pub String);

impl Actor {
    pub const ANONYMOUS: &'static str = "anonymous";
}

/// Reject anything under the MCP route without a valid bearer token, and
/// tag the request with the token's name.
///
/// `/healthz` is deliberately not behind this layer: heyo's `--health-path`
/// probe has no credentials.
pub async fn require_bearer(
    State(cfg): State<Arc<Config>>,
    mut req: Request<Body>,
    next: Next,
) -> Response {
    if !cfg.requires_auth() {
        req.extensions_mut()
            .insert(Actor(Actor::ANONYMOUS.to_string()));
        return next.run(req).await;
    }

    let presented = req
        .headers()
        .get(header::AUTHORIZATION)
        .and_then(|v| v.to_str().ok())
        .and_then(|v| v.strip_prefix("Bearer "))
        .map(str::trim)
        .unwrap_or_default();

    let Some(name) = match_token(presented, &cfg.tokens) else {
        tracing::warn!(path = %req.uri().path(), "rejected unauthenticated MCP request");
        return unauthorized();
    };
    req.extensions_mut().insert(Actor(name.to_string()));
    next.run(req).await
}

/// The name of the token that matches, checking every one in constant time
/// so a caller cannot learn which prefix is right, or which token exists,
/// from how long a rejection takes.
fn match_token<'a>(presented: &str, tokens: &'a [(String, String)]) -> Option<&'a str> {
    let mut found = None;
    for (name, secret) in tokens {
        if token_matches(presented, secret) && found.is_none() {
            found = Some(name.as_str());
        }
    }
    found
}

fn token_matches(presented: &str, expected: &str) -> bool {
    if presented.is_empty() || expected.is_empty() {
        return false;
    }
    // `ct_eq` is only constant-time for equal-length inputs; the length
    // check leaks the token length, which is not a secret.
    presented.len() == expected.len() && presented.as_bytes().ct_eq(expected.as_bytes()).into()
}

fn unauthorized() -> Response {
    (
        StatusCode::UNAUTHORIZED,
        [(header::WWW_AUTHENTICATE, "Bearer realm=\"crm-mcp\"")],
        "missing or invalid bearer token",
    )
        .into_response()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn only_an_exact_token_matches() {
        assert!(token_matches("s3cret", "s3cret"));
        assert!(!token_matches("s3cret", "s3crey"));
        assert!(!token_matches("s3cre", "s3cret"));
        assert!(!token_matches("s3cretx", "s3cret"));
        assert!(!token_matches("", "s3cret"));
        assert!(!token_matches("s3cret", ""));
    }

    #[test]
    fn the_matching_token_names_the_actor() {
        let tokens = vec![
            ("bdr".to_string(), "aaa".to_string()),
            ("sam".to_string(), "bbb".to_string()),
        ];
        assert_eq!(match_token("bbb", &tokens), Some("sam"));
        assert_eq!(match_token("aaa", &tokens), Some("bdr"));
        assert_eq!(match_token("ccc", &tokens), None);
    }
}
