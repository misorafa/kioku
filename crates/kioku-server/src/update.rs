//! The server's automatic-update state (SPEC-M2.5 §3.1, §3.4): what `GET /api/v1/status`
//! reports under `update`, and the shutdown request a self-updated server uses to stop
//! serving before it exits 75. The update itself (download, verify, swap) lives in the CLI.

use std::sync::Arc;
use std::time::Duration;

use kioku_core::Store;
use parking_lot::Mutex;
use serde::{Deserialize, Serialize};

/// How long a server that requested shutdown waits for in-flight requests (§3.1 step 3).
pub const SHUTDOWN_GRACE: Duration = Duration::from_secs(10);

/// `update` block of `GET /api/v1/status`.
#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct UpdateStatus {
    /// Effective `[update] auto`.
    pub auto: bool,
    /// True when this server runs under the service manager (`KIOKU_SERVICE=1`), the only
    /// case in which it checks for releases.
    #[serde(default)]
    pub managed: bool,
    /// Newest stable release tag seen by the last check.
    pub latest_seen: Option<String>,
    /// RFC 3339 time of the last check.
    pub last_check: Option<String>,
    /// Error of the last check or update attempt (`None` after a clean one).
    pub last_error: Option<String>,
}

/// Update status shared between the update task and the status handler.
pub type SharedUpdateStatus = Arc<Mutex<UpdateStatus>>;

impl UpdateStatus {
    /// A fresh status for `store`'s config (`auto` from `[update]`, nothing checked yet).
    pub fn shared_for(store: &Store) -> SharedUpdateStatus {
        Arc::new(Mutex::new(UpdateStatus {
            auto: store.config().update.auto,
            ..UpdateStatus::default()
        }))
    }
}

/// What [`crate::serve_with`] needs besides the store and address.
#[derive(Clone, Debug)]
pub struct ServeOptions {
    /// Reported by `GET /api/v1/status`.
    pub update: SharedUpdateStatus,
    /// Becomes `true` when the server should stop (graceful, at most [`SHUTDOWN_GRACE`]);
    /// `None` = only Ctrl-C / SIGTERM stop it.
    pub shutdown: Option<tokio::sync::watch::Receiver<bool>>,
}

impl ServeOptions {
    /// Options with `update` and no shutdown request.
    pub fn new(update: SharedUpdateStatus) -> ServeOptions {
        ServeOptions {
            update,
            shutdown: None,
        }
    }
}

/// Resolves once `rx` turns `true`; never without a receiver, or when its sender is gone
/// without doing so.
pub(crate) async fn requested(rx: Option<tokio::sync::watch::Receiver<bool>>) {
    if let Some(mut rx) = rx
        && rx.wait_for(|v| *v).await.is_ok()
    {
        return;
    }
    std::future::pending::<()>().await;
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn shutdown_request_fires_only_when_set() {
        let (tx, rx) = tokio::sync::watch::channel(false);
        let waiting = tokio::spawn(requested(Some(rx.clone())));
        tokio::time::sleep(Duration::from_millis(20)).await;
        assert!(!waiting.is_finished());
        tx.send(true).unwrap();
        tokio::time::timeout(Duration::from_secs(2), waiting)
            .await
            .unwrap()
            .unwrap();
        assert!(
            tokio::time::timeout(Duration::from_millis(20), requested(None))
                .await
                .is_err()
        );
        // A dropped sender is not a request.
        let (tx, rx) = tokio::sync::watch::channel(false);
        drop(tx);
        assert!(
            tokio::time::timeout(Duration::from_millis(50), requested(Some(rx)))
                .await
                .is_err()
        );
    }
}
