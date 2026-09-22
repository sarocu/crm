//! Read-only operator dashboard for the CRM stack: indexer health, the
//! pipeline, company dossiers and add-company requests, on `ADMIN_PORT`
//! (8090). No authentication — the load balancer in front of this VM owns
//! access control.

mod admin;
mod config;
mod state;
mod store;
mod views;

use std::net::SocketAddr;
use std::sync::Arc;
use std::time::Duration;

use anyhow::{Context, Result};
use axum::Json;
use axum::Router;
use crm_core::index::ensure_indexes;
use crm_core::market::MarketConfig;
use crm_core::meili;
use serde_json::json;
use tower_http::trace::TraceLayer;

use crate::config::Config;
use crate::state::AppState;
use crate::store::Store;

const MEILI_BOOT_WAIT: Duration = Duration::from_secs(120);

#[tokio::main]
async fn main() -> Result<()> {
    init_tracing();

    let config = Config::from_env()?;
    let market = MarketConfig::from_env().context("loading the market config")?;

    // Read-only, but it reads every index, including the stats the scoped
    // MCP key cannot see, so it takes the master key like the indexer.
    let client = meili::client_from_env("MEILI_MASTER_KEY")?;
    meili::wait_healthy(&client, MEILI_BOOT_WAIT).await?;
    if let Err(e) = ensure_indexes(&client, &market).await {
        tracing::warn!(error = %e, "could not apply index settings; continuing");
    }

    tracing::info!(
        market = %market.name,
        admin = %config.admin_bind,
        "starting the dashboard"
    );

    let state = Arc::new(AppState {
        store: Store::new(client, market, config.bot_health_url.clone()),
    });

    tokio::select! {
        r = serve("admin", config.admin_bind, admin::router(state.clone())) => r.context("admin listener")?,
        _ = shutdown_signal() => tracing::info!("shutting down"),
    }
    Ok(())
}

async fn serve(name: &'static str, addr: SocketAddr, router: Router) -> Result<()> {
    let app = router.layer(TraceLayer::new_for_http());
    let listener = tokio::net::TcpListener::bind(addr)
        .await
        .with_context(|| format!("cannot bind the {name} port at {addr}"))?;
    tracing::info!(%addr, surface = name, "listening");
    axum::serve(listener, app)
        .await
        .with_context(|| format!("{name} listener failed"))
}

/// Load-balancer health target.
pub async fn healthz(
    axum::extract::State(state): axum::extract::State<Arc<AppState>>,
) -> Json<serde_json::Value> {
    let meili_ok = state.store.client.health().await.is_ok();
    Json(json!({
        "status": if meili_ok { "ok" } else { "degraded" },
        "market": state.store.market.name,
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
