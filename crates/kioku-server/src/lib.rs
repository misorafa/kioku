//! kioku-server: the axum HTTP API (spec §9) and the MCP streamable HTTP endpoint (spec §10)
//! on one port. No business logic lives here: every handler is a thin `spawn_blocking`
//! around one `kioku_core::Store` method.

#![warn(missing_docs)]

mod api;
mod auth;
pub mod invite;
pub mod mcp;
mod shared;
mod update;

use std::sync::Arc;

use anyhow::Context;
use axum::Router;
use kioku_core::Store;
use rmcp::transport::streamable_http_server::StreamableHttpServerConfig;

pub use api::ApiError;
pub use invite::Invites;
pub use mcp::{INSTRUCTIONS, KiokuMcp};
pub use update::{SHUTDOWN_GRACE, ServeOptions, SharedUpdateStatus, UpdateStatus};

/// Builds the full router: `/api/v1/*` + `/mcp`, bearer auth on everything but health and
/// the join route `POST /api/v1/join` (SPEC-M2.3 §3.2; `GET /i/<code>` is gone, SPEC-M2.7 §4).
///
/// An empty `auth_token` disables auth (spec §9: auth is enforced when a token is set);
/// `serve` refuses that combination on non-loopback binds.
pub fn build_app(store: Arc<Store>, auth_token: String) -> Router {
    build_app_with_invites(store, auth_token, Arc::new(Invites::new()))
}

/// [`build_app`] with a given invite store (tests inject one with a fake clock).
pub fn build_app_with_invites(
    store: Arc<Store>,
    auth_token: String,
    invites: Arc<Invites>,
) -> Router {
    let update = UpdateStatus::shared_for(&store);
    app(store, auth_token, invites, update, mcp::transport_config())
}

/// [`build_app`] with a given update status (`GET /api/v1/status` reports it).
pub fn build_app_with_update(
    store: Arc<Store>,
    auth_token: String,
    update: SharedUpdateStatus,
) -> Router {
    app(
        store,
        auth_token,
        Arc::new(Invites::new()),
        update,
        mcp::transport_config(),
    )
}

fn app(
    store: Arc<Store>,
    auth_token: String,
    invites: Arc<Invites>,
    update: SharedUpdateStatus,
    mcp_config: StreamableHttpServerConfig,
) -> Router {
    let join = invite::JoinState {
        invites,
        token: Arc::from(auth_token.as_str()),
    };
    let mut protected = api::protected_routes(store.clone(), update)
        .merge(invite::protected_routes(join.clone()))
        .nest_service("/mcp", mcp::service(store, mcp_config));
    if auth_token.is_empty() {
        tracing::warn!("no auth_token configured: the API and /mcp are unauthenticated");
    } else {
        let token: Arc<str> = Arc::from(auth_token);
        protected = protected.layer(axum::middleware::from_fn_with_state(
            token,
            auth::require_bearer,
        ));
    }
    api::public_routes()
        .merge(invite::public_routes(join))
        .merge(protected)
}

/// Binds `bind:port` and serves the app until Ctrl-C / SIGTERM, using the store's
/// `[server] auth_token`. Refuses to start on a non-loopback bind without a token.
pub async fn serve(store: Arc<Store>, bind: String, port: u16) -> anyhow::Result<()> {
    let opts = ServeOptions::new(UpdateStatus::shared_for(&store));
    serve_with(store, bind, port, opts).await
}

/// [`serve`] with an update status to report and a shutdown request (SPEC-M2.5 §3.1: a
/// server that replaced its binary finishes in-flight requests, at most
/// [`SHUTDOWN_GRACE`], then returns so the caller can exit 75).
pub async fn serve_with(
    store: Arc<Store>,
    bind: String,
    port: u16,
    opts: ServeOptions,
) -> anyhow::Result<()> {
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
    let reindex_store = store.clone();
    let app = app(
        store,
        token,
        Arc::new(Invites::new()),
        opts.update.clone(),
        mcp_config,
    );
    let listener = bind_listener(&bind, port).await?;
    let addr = listener.local_addr().context("reading local address")?;
    tracing::info!(%addr, "kioku server listening (API /api/v1, MCP /mcp)");
    // SPEC-M2.8 §5: an index built by an older kioku is rebuilt now that the server
    // answers; search serves the old index until the rebuild commits.
    tokio::task::spawn_blocking(move || {
        if let Err(e) = reindex_store.reindex_if_outdated() {
            tracing::error!(
                error = format!("{e:#}"),
                "rebuilding the outdated index failed"
            );
        }
    });
    // Connect info: the invite routes rate-limit failed lookups per peer (SPEC-M2.3 §3.2).
    let requested = opts.shutdown.clone();
    let server = axum::serve(
        listener,
        app.into_make_service_with_connect_info::<std::net::SocketAddr>(),
    )
    .with_graceful_shutdown(async move {
        tokio::select! {
            _ = shutdown_signal() => {},
            _ = update::requested(requested) => {},
        }
        tracing::info!("shutting down");
        // Closes open MCP SSE streams so graceful shutdown does not wait on them.
        mcp_cancel.cancel();
    });
    let grace = async move {
        update::requested(opts.shutdown).await;
        tokio::time::sleep(SHUTDOWN_GRACE).await;
    };
    tokio::select! {
        r = server.into_future() => r.context("serving HTTP")?,
        _ = grace => tracing::warn!("in-flight requests did not finish in time; exiting anyway"),
    }
    Ok(())
}

/// Binds the listener (SPEC-M2 §19.1): `0.0.0.0` becomes the dual-stack `[::]` where IPv6
/// sockets accept IPv4 too, so clients reaching the server by an IPv6 address of its name
/// get through; without IPv6 it falls back to `0.0.0.0`. Other binds are used as given.
pub async fn bind_listener(bind: &str, port: u16) -> anyhow::Result<tokio::net::TcpListener> {
    if bind.trim() == "0.0.0.0" && dual_stack_default() {
        match tokio::net::TcpListener::bind(("::", port)).await {
            Ok(l) => return Ok(l),
            Err(e) => tracing::info!(error = %e, "IPv6 unavailable, listening on 0.0.0.0 only"),
        }
    }
    tokio::net::TcpListener::bind((bind, port))
        .await
        .with_context(|| format!("binding {bind}:{port}"))
}

/// True where a `[::]` socket also accepts IPv4 by default: macOS, and Linux with
/// `net.ipv6.bindv6only = 0`.
fn dual_stack_default() -> bool {
    if cfg!(target_os = "macos") {
        return true;
    }
    if cfg!(target_os = "linux") {
        return std::fs::read_to_string("/proc/sys/net/ipv6/bindv6only")
            .is_ok_and(|v| v.trim() == "0");
    }
    false
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

    /// `0.0.0.0` answers over IPv4 and, where dual stack is the default, over IPv6 too.
    #[tokio::test]
    async fn any_address_bind_is_dual_stack() {
        let listener = bind_listener("0.0.0.0", 0).await.unwrap();
        let port = listener.local_addr().unwrap().port();
        let accept = tokio::spawn(async move {
            for _ in 0..2 {
                let _ = listener.accept().await;
            }
        });
        tokio::net::TcpStream::connect(("127.0.0.1", port))
            .await
            .unwrap();
        if dual_stack_default() && std::net::TcpListener::bind("[::1]:0").is_ok() {
            tokio::net::TcpStream::connect(("::1", port)).await.unwrap();
        }
        accept.abort();
    }

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
