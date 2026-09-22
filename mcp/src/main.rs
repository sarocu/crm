//! MCP server for the CRM.
//!
//! Exposes prospecting, dossier and pipeline tools over Meilisearch, via
//! MCP's Streamable HTTP transport, so a BDR agent can reach it at `/mcp`
//! once the VM is up.

mod auth;
mod config;
mod filter;
mod render;
mod score;
mod server;
mod state;

use std::sync::Arc;
use std::time::Duration;

use anyhow::{Context, Result};
use axum::{Json, Router, extract::State, middleware, routing::get};
use crm_core::index::ensure_indexes;
use crm_core::market::MarketConfig;
use crm_core::meili;
use rmcp::transport::streamable_http_server::{
    StreamableHttpServerConfig, StreamableHttpService, session::local::LocalSessionManager,
};
use serde_json::json;
use tower_http::trace::TraceLayer;

use crate::config::Config;
use crate::server::CrmServer;
use crate::state::AppState;

/// How long to wait for Meilisearch on boot. Generous because on heyo the
/// database VM may still be starting when this one comes up.
const MEILI_BOOT_WAIT: Duration = Duration::from_secs(120);

#[tokio::main]
async fn main() -> Result<()> {
    init_tracing();

    let cfg = Arc::new(Config::from_env()?);
    let market = MarketConfig::from_env().context("loading the market config")?;
    tracing::info!(
        market = %market.name,
        verticals = market.verticals.len(),
        profiles = market.profiles.len(),
        "loaded market config"
    );

    // Two scoped keys, because a Meilisearch key grants every action it
    // lists on every index it lists: one reads everything, the other writes
    // only CRM state. deploy/create-mcp-keys.sh mints both.
    let client = meili::client_from_env("MEILI_READ_KEY")?;
    let writer = meili::client_from_env("MEILI_WRITE_KEY")?;
    meili::wait_healthy(&client, MEILI_BOOT_WAIT).await?;

    // Applying settings is idempotent and whichever service boots first
    // wins. The scoped keys cannot do it, which is fine — the indexer will.
    // Failing here would take down the CRM over a permission we do not
    // actually need.
    if let Err(e) = ensure_indexes(&client, &market).await {
        tracing::warn!(error = %e, "could not apply index settings (expected with a scoped key); continuing");
    }

    let state = Arc::new(AppState::new(client, writer, market));
    let ct = tokio_util::sync::CancellationToken::new();

    let mcp_state = state.clone();
    // rmcp's Host allowlist is loopback-only by default, so behind a load
    // balancer every request arrives with a hostname it rejects. Anything
    // listed in MCP_ALLOWED_HOSTS replaces that list; `*` turns the check
    // off, which is only safe because the bearer check runs in front of it.
    //
    // Stateless, because every tool call stands alone — all state lives in
    // Meilisearch — and a session would hold nothing. Sessions live in one process's memory,
    // so any rollout, VM recycle or second replica turns a client's session
    // id into a 404, and clients that don't re-initialize stay broken.
    let mut http_config = StreamableHttpServerConfig::default()
        .with_cancellation_token(ct.child_token())
        .with_legacy_session_mode(false);
    if cfg.allowed_hosts.iter().any(|h| h == "*") {
        tracing::warn!("MCP_ALLOWED_HOSTS=* — Host validation is disabled");
        http_config = http_config.disable_allowed_hosts();
    } else if !cfg.allowed_hosts.is_empty() {
        tracing::info!(hosts = ?cfg.allowed_hosts, "restricting the MCP Host allowlist");
        http_config = http_config.with_allowed_hosts(cfg.allowed_hosts.clone());
    }
    let mcp_service = StreamableHttpService::new(
        move || Ok(CrmServer::new(mcp_state.clone())),
        LocalSessionManager::default().into(),
        http_config,
    );

    // Auth wraps only the MCP route: heyo's `--health-path` probe has no
    // credentials to present.
    let mcp_router = Router::new()
        .fallback_service(mcp_service)
        .layer(middleware::from_fn_with_state(
            cfg.clone(),
            auth::require_bearer,
        ))
        .with_state(cfg.clone());

    let app = Router::new()
        .route("/healthz", get(healthz))
        .with_state(state.clone())
        .nest("/mcp", mcp_router)
        .layer(TraceLayer::new_for_http());

    let listener = tokio::net::TcpListener::bind(cfg.bind)
        .await
        .with_context(|| format!("cannot bind {}", cfg.bind))?;
    tracing::info!(
        addr = %cfg.bind,
        auth = if cfg.requires_auth() { "bearer" } else { "ANONYMOUS" },
        "mcp server listening on /mcp"
    );

    let shutdown = ct.clone();
    axum::serve(listener, app)
        .with_graceful_shutdown(async move {
            shutdown_signal().await;
            tracing::info!("shutting down");
            shutdown.cancel();
        })
        .await
        .context("server error")?;
    Ok(())
}

/// Liveness for heyo's health check and for compose's `depends_on`.
async fn healthz(State(state): State<Arc<AppState>>) -> Json<serde_json::Value> {
    let meili_ok = state.client.health().await.is_ok();
    Json(json!({
        "status": if meili_ok { "ok" } else { "degraded" },
        "market": state.market.name,
        "meilisearch": if meili_ok { "available" } else { "unreachable" },
        "version": env!("CARGO_PKG_VERSION"),
    }))
}

async fn shutdown_signal() {
    let ctrl_c = async {
        let _ = tokio::signal::ctrl_c().await;
    };
    #[cfg(unix)]
    let terminate = async {
        if let Ok(mut s) = tokio::signal::unix::signal(tokio::signal::unix::SignalKind::terminate())
        {
            s.recv().await;
        }
    };
    #[cfg(not(unix))]
    let terminate = std::future::pending::<()>();

    tokio::select! {
        _ = ctrl_c => {}
        _ = terminate => {}
    }
}

fn init_tracing() {
    use tracing_subscriber::{EnvFilter, fmt, prelude::*};
    tracing_subscriber::registry()
        .with(EnvFilter::try_from_default_env().unwrap_or_else(|_| "info".into()))
        .with(fmt::layer())
        .init();
}
