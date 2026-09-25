//! Data directory layout (spec §2) and `kioku init`.

use std::path::{Path, PathBuf};

use anyhow::Context;
use serde::{Deserialize, Serialize};

use crate::config::Config;
use crate::error::Result;
use crate::store::Store;
use crate::util::generate_token;

/// Paths inside the data directory.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct DataDir {
    root: PathBuf,
}

impl DataDir {
    /// Wraps a data directory root.
    pub fn new(root: &Path) -> DataDir {
        DataDir {
            root: root.to_path_buf(),
        }
    }

    /// The data directory itself.
    pub fn root(&self) -> PathBuf {
        self.root.clone()
    }

    /// `wiki/` — the git repository, source of truth.
    pub fn wiki(&self) -> PathBuf {
        self.root.join("wiki")
    }

    /// `raw/` — append-only raw observations.
    pub fn raw(&self) -> PathBuf {
        self.root.join("raw")
    }

    /// `raw/<project>/<session>.jsonl`.
    pub fn raw_file(&self, project: &str, session: &str) -> PathBuf {
        self.raw().join(project).join(format!("{session}.jsonl"))
    }

    /// `db/kioku.sqlite`.
    pub fn db_file(&self) -> PathBuf {
        self.root.join("db").join("kioku.sqlite")
    }

    /// `index/tantivy/`.
    pub fn index_dir(&self) -> PathBuf {
        self.root.join("index").join("tantivy")
    }

    /// `logs/`.
    pub fn logs_dir(&self) -> PathBuf {
        self.root.join("logs")
    }

    /// `logs/hook.log`.
    pub fn hook_log(&self) -> PathBuf {
        self.logs_dir().join("hook.log")
    }

    /// Creates every directory of the layout (idempotent).
    pub fn ensure(&self) -> Result<()> {
        for dir in [
            self.wiki().join(crate::page::GLOBAL_DIR),
            self.raw(),
            self.root.join("db"),
            self.index_dir(),
            self.logs_dir(),
        ] {
            std::fs::create_dir_all(&dir).with_context(|| format!("creating {}", dir.display()))?;
        }
        Ok(())
    }
}

/// What `init` did.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct InitReport {
    /// Resolved data directory.
    pub data_dir: String,
    /// Config file path.
    pub config_file: String,
    /// True when config.toml was created or updated.
    pub config_written: bool,
    /// True when a new auth token was generated.
    pub token_generated: bool,
    /// True when the wiki is a git repository kioku commits to.
    pub git_enabled: bool,
}

/// `kioku init`: creates the layout, git-inits the wiki, writes config.toml with a token,
/// and creates the SQLite schema and the index. Idempotent; never replaces an existing token.
pub fn init(config: &mut Config) -> Result<InitReport> {
    DataDir::new(&config.data_dir).ensure()?;
    let mut config_written = !config.config_file.exists();
    let mut token_generated = false;
    if config.server.auth_token.is_none() {
        let token = config
            .client
            .auth_token
            .clone()
            .unwrap_or_else(generate_token);
        token_generated = config.client.auth_token.is_none();
        config.server.auth_token = Some(token);
        config_written = true;
    }
    if config.client.auth_token.is_none() {
        config.client.auth_token = config.server.auth_token.clone();
        config_written = true;
    }
    if config_written {
        config.save()?;
    }
    let store = Store::open(config.clone())?;
    Ok(InitReport {
        data_dir: config.data_dir.display().to_string(),
        config_file: config.config_file.display().to_string(),
        config_written,
        token_generated,
        git_enabled: store.git_enabled(),
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn init_creates_layout_and_is_idempotent() {
        let tmp = tempfile::tempdir().unwrap();
        let mut config = Config::for_data_dir(tmp.path());
        let report = init(&mut config).unwrap();
        assert!(report.config_written);
        assert!(report.token_generated);
        let d = DataDir::new(tmp.path());
        assert!(d.wiki().join("_global").is_dir());
        assert!(d.db_file().is_file());
        assert!(d.index_dir().join("meta.json").is_file());
        assert!(d.logs_dir().is_dir());
        if report.git_enabled {
            assert!(d.wiki().join(".git").exists());
        }
        let token = config.server.auth_token.clone().unwrap();
        assert_eq!(config.client.auth_token.as_deref(), Some(token.as_str()));

        let mut again = Config::load_file(&config.config_file).unwrap();
        let report = init(&mut again).unwrap();
        assert!(!report.config_written);
        assert!(!report.token_generated);
        assert_eq!(again.server.auth_token.as_deref(), Some(token.as_str()));
    }
}
