//! The gauges of `GET /api/v1/metrics` (SPEC-M3.2 §1): counts, sizes and the times of the
//! last backup and observation, read in one call. The server adds its own counters and
//! renders the Prometheus text.

use super::*;

/// Point-in-time values behind the `kioku_*` gauges.
#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct MetricsSnapshot {
    /// Sessions whose status is `open`.
    pub sessions_open: u64,
    /// All sessions.
    pub sessions_total: u64,
    /// All observations.
    pub observations_total: u64,
    /// Handoffs nobody has accepted yet.
    pub handoffs_pending: u64,
    /// Documents in the search index.
    pub index_docs: u64,
    /// Disk usage (`db/`, `raw/`, `wiki/`, `backups/`, `index/`) and the last prune.
    pub storage: StorageReport,
    /// Time of the last completed backup.
    pub last_backup: Option<String>,
    /// Time the server last received an observation.
    pub last_observation: Option<String>,
    /// Failed wiki `git add` / `git commit` runs since the store was opened.
    pub git_commit_failures: u64,
}

impl Store {
    /// Everything the metrics endpoint reports about the store (SPEC-M3.2 §1).
    pub fn metrics_snapshot(&self) -> Result<MetricsSnapshot> {
        let storage = self.storage()?;
        let conn = self.db.lock();
        let scalar = |sql: &str| -> Result<u64> {
            let n: i64 = conn
                .query_row(sql, [], |r| r.get(0))
                .with_context(|| format!("metrics: {sql}"))?;
            Ok(n.max(0) as u64)
        };
        let meta = |key: &str| -> Result<Option<String>> {
            Ok(conn
                .query_row(
                    "SELECT value FROM reliability_meta WHERE key=?1",
                    [key],
                    |r| r.get(0),
                )
                .optional()
                .context("reading reliability metadata")?)
        };
        Ok(MetricsSnapshot {
            sessions_open: scalar("SELECT COUNT(*) FROM sessions WHERE status = 'open'")?,
            sessions_total: db::count(&conn, "sessions")?,
            observations_total: db::count(&conn, "observations")?,
            handoffs_pending: scalar("SELECT COUNT(*) FROM handoffs WHERE accepted_at IS NULL")?,
            index_docs: self.index.num_docs(),
            last_backup: meta("last_backup")?,
            last_observation: meta("last_received")?,
            git_commit_failures: self.git.commit_failures(),
            storage,
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{NewObservation, ObservationKind, ProjectIdentity, SessionStartRequest};

    #[test]
    fn snapshot_counts_open_sessions_and_observations() {
        let tmp = tempfile::tempdir().unwrap();
        let store = Store::open(Config::for_data_dir(tmp.path())).unwrap();
        let empty = store.metrics_snapshot().unwrap();
        assert_eq!(empty.sessions_total, 0);
        assert_eq!(empty.last_observation, None);
        let project = ProjectIdentity {
            id: "metrics-proj".into(),
            name: "記憶".into(),
            root: tmp.path().display().to_string(),
            remote: None,
        };
        store
            .start_session(&SessionStartRequest {
                session_id: "s-1".into(),
                agent: "claude-code".into(),
                cwd: tmp.path().display().to_string(),
                source: "startup".into(),
                project,
                lane: None,
                machine: None,
            })
            .unwrap();
        store
            .add_observation(&NewObservation {
                event_id: None,
                session_id: "s-1".into(),
                kind: ObservationKind::Prompt,
                ts: None,
                payload: serde_json::json!({"prompt": "検索を直す"}),
            })
            .unwrap();
        let s = store.metrics_snapshot().unwrap();
        assert_eq!((s.sessions_open, s.sessions_total), (1, 1));
        assert_eq!(s.observations_total, 1);
        assert_eq!(s.handoffs_pending, 0);
        assert!(s.last_observation.is_some());
        assert!(s.storage.db_bytes > 0);
    }
}
