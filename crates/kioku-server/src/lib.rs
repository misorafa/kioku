//! kioku-server: the axum HTTP API (spec §9) and the MCP streamable HTTP endpoint (spec §10)
//! on one port. No business logic lives here: every handler is a thin `spawn_blocking`
//! around one `kioku_core::Store` method.

#![warn(missing_docs)]

mod api;
mod auth;
mod mcp;
mod shared;

use std::sync::Arc;

use anyhow::Context;
use axum::Router;
use kioku_core::Store;
use rmcp::transport::streamable_http_server::StreamableHttpServerConfig;

pub use api::ApiError;
pub use mcp::{INSTRUCTIONS, KiokuMcp};

/// Builds the full router: `/api/v1/*` + `/mcp`, bearer auth on everything but health.
///
/// An empty `auth_token` disables auth (spec §9: auth is enforced when a token is set);
/// `serve` refuses that combination on non-loopback binds.
pub fn build_app(store: Arc<Store>, auth_token: String) -> Router {
    app(store, auth_token, mcp::transport_config())
}

fn app(store: Arc<Store>, auth_token: String, mcp_config: StreamableHttpServerConfig) -> Router {
    let mut protected =
        api::protected_routes(store.clone()).nest_service("/mcp", mcp::service(store, mcp_config));
    if auth_token.is_empty() {
        tracing::warn!("no auth_token configured: the API and /mcp are unauthenticated");
    } else {
        let token: Arc<str> = Arc::from(auth_token);
        protected = protected.layer(axum::middleware::from_fn_with_state(
            token,
            auth::require_bearer,
        ));
    }
    api::public_routes().merge(protected)
}

/// Binds `bind:port` and serves the app until Ctrl-C / SIGTERM, using the store's
/// `[server] auth_token`. Refuses to start on a non-loopback bind without a token.
pub async fn serve(store: Arc<Store>, bind: String, port: u16) -> anyhow::Result<()> {
    let token = store
        .config()
        .server
        .auth_token
        .map(|t| t.trim().to_string())
        .unwrap_or_default();
    if token.is_empty() && !is_loopback(&bind) {
        anyhow::bail!(
            "refusing to listen on {bind} without an auth_token (set [server] auth_token or KIOKU_AUTH_TOKEN)"
        );
    }
    let mcp_config = mcp::transport_config();
    let mcp_cancel = mcp_config.cancellation_token.clone();
    let app = app(store, token, mcp_config);
    let listener = tokio::net::TcpListener::bind((bind.as_str(), port))
        .await
        .with_context(|| format!("binding {bind}:{port}"))?;
    let addr = listener.local_addr().context("reading local address")?;
    tracing::info!(%addr, "kioku server listening (API /api/v1, MCP /mcp)");
    axum::serve(listener, app)
        .with_graceful_shutdown(async move {
            shutdown_signal().await;
            tracing::info!("shutting down");
            // Closes open MCP SSE streams so graceful shutdown does not wait on them.
            mcp_cancel.cancel();
        })
        .await
        .context("serving HTTP")?;
    Ok(())
}

/// True for `localhost`, `127.*` and `::1` (with or without brackets).
fn is_loopback(bind: &str) -> bool {
    let b = bind.trim().trim_start_matches('[').trim_end_matches(']');
    b.eq_ignore_ascii_case("localhost")
        || b.parse::<std::net::IpAddr>()
            .map(|ip| ip.is_loopback())
            .unwrap_or(false)
}

async fn shutdown_signal() {
    let ctrl_c = async {
        if let Err(e) = tokio::signal::ctrl_c().await {
            tracing::warn!(error = %e, "cannot listen for Ctrl-C");
            std::future::pending::<()>().await;
        }
    };
    #[cfg(unix)]
    let term = async {
        match tokio::signal::unix::signal(tokio::signal::unix::SignalKind::terminate()) {
            Ok(mut s) => {
                s.recv().await;
            }
            Err(e) => {
                tracing::warn!(error = %e, "cannot listen for SIGTERM");
                std::future::pending::<()>().await;
            }
        }
    };
    #[cfg(not(unix))]
    let term = std::future::pending::<()>();
    tokio::select! {
        _ = ctrl_c => {},
        _ = term => {},
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn loopback_detection() {
        for b in ["127.0.0.1", "localhost", "::1", "[::1]", "127.1.2.3"] {
            assert!(is_loopback(b), "{b}");
        }
        for b in ["0.0.0.0", "192.168.1.10", "::", "example.com"] {
            assert!(!is_loopback(b), "{b}");
        }
    }
}
