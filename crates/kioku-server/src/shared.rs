//! Helpers shared by the HTTP handlers and the MCP tools: running `Store` calls on the
//! blocking pool, resolving search scopes, and turning "unknown project" errors into
//! messages that list the known project ids.

use std::sync::Arc;

use kioku_core::{Error, SearchScope, Store};

/// Default number of hits for `GET /api/v1/search`.
pub const DEFAULT_HTTP_LIMIT: usize = 10;
/// Default number of hits for the `kioku_query` tool (spec §10).
pub const DEFAULT_MCP_LIMIT: usize = 8;
/// Upper bound for any search limit.
pub const MAX_LIMIT: usize = 100;

/// Runs a blocking `Store` call on tokio's blocking pool.
pub async fn blocking<T, F>(store: &Arc<Store>, f: F) -> Result<T, Error>
where
    T: Send + 'static,
    F: FnOnce(&Store) -> Result<T, Error> + Send + 'static,
{
    let store = store.clone();
    match tokio::task::spawn_blocking(move || f(&store)).await {
        Ok(result) => result,
        Err(e) => Err(Error::Internal(anyhow::anyhow!(
            "blocking task failed: {e}"
        ))),
    }
}

/// Maps `scope` (`project` | `global` | `all`) and `project` to a search scope.
///
/// An explicit scope wins; without one, a given project means `project` (that project plus
/// global pages) and no project means `all`.
pub fn resolve_scope(scope: Option<&str>, project: Option<&str>) -> Result<SearchScope, Error> {
    let project = project.map(str::trim).filter(|p| !p.is_empty());
    let scope = scope.map(|s| s.trim().to_ascii_lowercase());
    match (scope.as_deref(), project) {
        (None | Some(""), Some(p)) | (Some("project"), Some(p)) => {
            Ok(SearchScope::Project(p.to_string()))
        }
        (Some("project"), None) => Err(Error::invalid("scope=project requires a project")),
        (Some("global"), _) => Ok(SearchScope::Global),
        (None | Some("") | Some("all"), _) => Ok(SearchScope::All),
        (Some(other), _) => Err(Error::invalid(format!(
            "unknown scope: {other} (expected project, global or all)"
        ))),
    }
}

/// Clamps a requested limit to `1..=MAX_LIMIT`, using `default` when absent.
pub fn clamp_limit(limit: Option<usize>, default: usize) -> usize {
    limit.unwrap_or(default).clamp(1, MAX_LIMIT)
}

/// If `err` is a `NotFound` for `project`, appends the list of known project ids so an
/// agent can retry with a valid one. Must run on the blocking pool (it queries SQLite).
pub fn with_project_hint(store: &Store, err: Error, project: Option<&str>) -> Error {
    let Error::NotFound(msg) = err else {
        return err;
    };
    let Some(project) = project else {
        return Error::NotFound(msg);
    };
    let projects = match store.list_projects() {
        Ok(p) => p,
        Err(_) => return Error::NotFound(msg),
    };
    if projects.iter().any(|p| p.id == project) {
        return Error::NotFound(msg);
    }
    if projects.is_empty() {
        return Error::NotFound(format!(
            "{msg} — no projects are registered yet (a project is registered when a session \
             starts through the SessionStart hook)"
        ));
    }
    let known: Vec<String> = projects
        .iter()
        .map(|p| format!("{} ({})", p.id, p.name))
        .collect();
    Error::NotFound(format!(
        "{msg} — known projects: {}. Pass one of these ids as `project`.",
        known.join(", ")
    ))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn scope_resolution() {
        assert_eq!(
            resolve_scope(None, Some("p-1")).unwrap(),
            SearchScope::Project("p-1".into())
        );
        assert_eq!(resolve_scope(None, None).unwrap(), SearchScope::All);
        assert_eq!(
            resolve_scope(Some("all"), Some("p")).unwrap(),
            SearchScope::All
        );
        assert_eq!(
            resolve_scope(Some("Global"), None).unwrap(),
            SearchScope::Global
        );
        assert!(matches!(
            resolve_scope(Some("project"), None),
            Err(Error::InvalidInput(_))
        ));
        assert!(matches!(
            resolve_scope(Some("everything"), None),
            Err(Error::InvalidInput(_))
        ));
    }

    #[test]
    fn limit_clamping() {
        assert_eq!(clamp_limit(None, 8), 8);
        assert_eq!(clamp_limit(Some(0), 8), 1);
        assert_eq!(clamp_limit(Some(1000), 8), MAX_LIMIT);
    }
}
