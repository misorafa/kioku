//! `config.toml` (spec §3): server + client settings, environment overrides, data dir resolution.

use std::collections::HashMap;
use std::path::{Path, PathBuf};

use anyhow::Context;
use serde::{Deserialize, Serialize};

use crate::error::Result;
use crate::strings::Lang;
use crate::util;

/// Default HTTP port.
pub const DEFAULT_PORT: u16 = 7391;
/// Default client timeout in milliseconds.
pub const DEFAULT_TIMEOUT_MS: u64 = 3000;
/// File name of the config inside the config directory.
pub const CONFIG_FILE: &str = "config.toml";

/// `[server]` section.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct ServerConfig {
    /// Address to bind (`127.0.0.1` by default).
    pub bind: String,
    /// TCP port.
    pub port: u16,
    /// Bearer token; required when `bind` is not loopback.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub auth_token: Option<String>,
    /// Optional override of the data directory (`~` is expanded).
    #[serde(skip_serializing_if = "Option::is_none")]
    pub data_dir: Option<String>,
    /// Language of generated summaries (`ja` | `en`).
    pub summary_lang: Lang,
}

impl Default for ServerConfig {
    fn default() -> ServerConfig {
        ServerConfig {
            bind: "127.0.0.1".to_string(),
            port: DEFAULT_PORT,
            auth_token: None,
            data_dir: None,
            summary_lang: Lang::Ja,
        }
    }
}

/// `[client]` section, used by hooks and `kioku search`.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct ClientConfig {
    /// Base URL of the kioku server.
    pub server_url: String,
    /// Bearer token sent to the server.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub auth_token: Option<String>,
    /// Hard timeout for hook requests.
    pub timeout_ms: u64,
    /// Whether the Stop hook may nudge the agent to write a handoff.
    pub stop_nudge: bool,
    /// Language of the SessionStart context block and the Stop nudge (`ja` | `en`).
    pub lang: Lang,
}

impl Default for ClientConfig {
    fn default() -> ClientConfig {
        ClientConfig {
            server_url: format!("http://127.0.0.1:{DEFAULT_PORT}"),
            auth_token: None,
            timeout_ms: DEFAULT_TIMEOUT_MS,
            stop_nudge: true,
            lang: Lang::Ja,
        }
    }
}

/// Full configuration plus the resolved locations it was loaded from.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct Config {
    /// `[server]` settings.
    #[serde(default)]
    pub server: ServerConfig,
    /// `[client]` settings.
    #[serde(default)]
    pub client: ClientConfig,
    /// Resolved data directory (env > file `server.data_dir` > config dir).
    #[serde(skip)]
    pub data_dir: PathBuf,
    /// Path of the `config.toml` this config belongs to.
    #[serde(skip)]
    pub config_file: PathBuf,
}

/// On-disk shape used for `--client-only` configs (no `[server]` section).
#[derive(Serialize)]
struct ClientOnlyFile<'a> {
    client: &'a ClientConfig,
}

impl Config {
    /// Defaults rooted at `dir` (used for tests and as the base of loading).
    pub fn for_data_dir(dir: &Path) -> Config {
        Config {
            server: ServerConfig::default(),
            client: ClientConfig::default(),
            data_dir: dir.to_path_buf(),
            config_file: dir.join(CONFIG_FILE),
        }
    }

    /// Loads config using the real process environment.
    pub fn load() -> Result<Config> {
        let env: HashMap<String, String> = std::env::vars().collect();
        Config::load_with_env(&env)
    }

    /// Loads config with an explicit environment snapshot (tests never mutate the real env).
    pub fn load_with_env(env: &HashMap<String, String>) -> Result<Config> {
        let config_dir = env
            .get("KIOKU_DATA_DIR")
            .filter(|v| !v.is_empty())
            .map(|v| util::expand_tilde(v))
            .unwrap_or_else(|| util::home_dir().join(".kioku"));
        Config::load_from_dir(&config_dir, env)
    }

    /// Loads `<config_dir>/config.toml`, resolves the data dir, then applies `env`.
    pub fn load_from_dir(config_dir: &Path, env: &HashMap<String, String>) -> Result<Config> {
        let mut config = Config::load_file(&config_dir.join(CONFIG_FILE))?;
        config.data_dir = config
            .server
            .data_dir
            .as_deref()
            .map(util::expand_tilde)
            .unwrap_or_else(|| config_dir.to_path_buf());
        config.apply_env(env)?;
        Ok(config)
    }

    /// Reads a config file (missing file → defaults); `data_dir` = the file's directory.
    pub fn load_file(path: &Path) -> Result<Config> {
        let dir = path.parent().unwrap_or(Path::new(".")).to_path_buf();
        let mut config = if path.exists() {
            let text = std::fs::read_to_string(path)
                .with_context(|| format!("reading {}", path.display()))?;
            toml::from_str::<Config>(&text)
                .with_context(|| format!("parsing {}", path.display()))?
        } else {
            Config::for_data_dir(&dir)
        };
        config.data_dir = dir;
        config.config_file = path.to_path_buf();
        Ok(config)
    }

    /// Applies `KIOKU_*` environment overrides (env beats file).
    pub fn apply_env(&mut self, env: &HashMap<String, String>) -> Result<()> {
        let get = |k: &str| env.get(k).filter(|v| !v.is_empty()).cloned();
        if let Some(dir) = get("KIOKU_DATA_DIR") {
            self.data_dir = util::expand_tilde(&dir);
        }
        if let Some(bind) = get("KIOKU_BIND") {
            self.server.bind = bind;
        }
        if let Some(port) = get("KIOKU_PORT") {
            self.server.port = port
                .parse()
                .map_err(|_| crate::Error::invalid(format!("KIOKU_PORT is not a port: {port}")))?;
        }
        if let Some(token) = get("KIOKU_AUTH_TOKEN") {
            self.server.auth_token = Some(token.clone());
            self.client.auth_token = Some(token);
        }
        if let Some(url) = get("KIOKU_SERVER_URL") {
            self.client.server_url = url;
        }
        if let Some(v) = get("KIOKU_STOP_NUDGE") {
            self.client.stop_nudge = !matches!(v.as_str(), "0" | "false" | "off" | "no");
        }
        Ok(())
    }

    /// Serializes the full config as TOML.
    pub fn to_toml(&self) -> Result<String> {
        Ok(toml::to_string_pretty(self).context("serializing config")?)
    }

    /// Writes the full config to `config_file`, creating parent directories.
    pub fn save(&self) -> Result<()> {
        write_file(&self.config_file, &self.to_toml()?)
    }

    /// Writes (or merges into) a config file that only has a `[client]` section.
    pub fn write_client_only(path: &Path, server_url: &str, token: &str) -> Result<Config> {
        let mut config = Config::load_file(path)?;
        config.client.server_url = server_url.to_string();
        config.client.auth_token = Some(token.to_string());
        let text = if path.exists() && has_server_section(path) {
            config.to_toml()?
        } else {
            toml::to_string_pretty(&ClientOnlyFile {
                client: &config.client,
            })
            .context("serializing client config")?
        };
        write_file(path, &text)?;
        Ok(config)
    }

    /// True when `server.bind` is a loopback address.
    pub fn is_loopback_bind(&self) -> bool {
        let b = self.server.bind.trim();
        b == "localhost" || b == "::1" || b.starts_with("127.")
    }

    /// Summary language shortcut.
    pub fn lang(&self) -> Lang {
        self.server.summary_lang
    }
}

fn has_server_section(path: &Path) -> bool {
    std::fs::read_to_string(path)
        .map(|t| t.lines().any(|l| l.trim() == "[server]"))
        .unwrap_or(false)
}

/// Writes config.toml (it holds the auth token): the file is 0600 and a newly created
/// parent directory 0700 on unix.
fn write_file(path: &Path, text: &str) -> Result<()> {
    if let Some(parent) = path.parent()
        && !parent.as_os_str().is_empty()
        && !parent.exists()
    {
        crate::util::create_private_dir(parent)
            .with_context(|| format!("creating {}", parent.display()))?;
    }
    crate::util::write_private_file(path, text)
        .with_context(|| format!("writing {}", path.display()))?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn env(pairs: &[(&str, &str)]) -> HashMap<String, String> {
        pairs
            .iter()
            .map(|(k, v)| (k.to_string(), v.to_string()))
            .collect()
    }

    #[test]
    fn defaults_when_file_missing() {
        let dir = tempfile::tempdir().unwrap();
        let d = dir.path().to_str().unwrap();
        let c = Config::load_with_env(&env(&[("KIOKU_DATA_DIR", d)])).unwrap();
        assert_eq!(c.server.port, DEFAULT_PORT);
        assert_eq!(c.server.bind, "127.0.0.1");
        assert_eq!(c.data_dir, dir.path());
        assert_eq!(c.client.timeout_ms, 3000);
        assert!(c.client.stop_nudge);
        assert_eq!(c.client.lang, Lang::Ja);
        assert!(c.is_loopback_bind());
    }

    #[test]
    fn file_then_env_overrides() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(
            dir.path().join(CONFIG_FILE),
            "[server]\nbind = \"0.0.0.0\"\nport = 9000\nauth_token = \"filetok\"\nsummary_lang = \"en\"\n\n[client]\nserver_url = \"http://home:9000\"\ntimeout_ms = 500\nlang = \"en\"\n",
        )
        .unwrap();
        let d = dir.path().to_str().unwrap();
        let c = Config::load_with_env(&env(&[("KIOKU_DATA_DIR", d)])).unwrap();
        assert_eq!(c.server.bind, "0.0.0.0");
        assert!(!c.is_loopback_bind());
        assert_eq!(c.server.port, 9000);
        assert_eq!(c.server.auth_token.as_deref(), Some("filetok"));
        assert_eq!(c.lang(), Lang::En);
        assert_eq!(c.client.timeout_ms, 500);
        assert_eq!(c.client.lang, Lang::En);

        let c = Config::load_with_env(&env(&[
            ("KIOKU_DATA_DIR", d),
            ("KIOKU_BIND", "127.0.0.1"),
            ("KIOKU_PORT", "7000"),
            ("KIOKU_AUTH_TOKEN", "envtok"),
            ("KIOKU_SERVER_URL", "http://x:1"),
            ("KIOKU_STOP_NUDGE", "0"),
        ]))
        .unwrap();
        assert_eq!(c.server.bind, "127.0.0.1");
        assert_eq!(c.server.port, 7000);
        assert_eq!(c.server.auth_token.as_deref(), Some("envtok"));
        assert_eq!(c.client.auth_token.as_deref(), Some("envtok"));
        assert_eq!(c.client.server_url, "http://x:1");
        assert!(!c.client.stop_nudge);
        assert!(
            Config::load_with_env(&env(&[("KIOKU_DATA_DIR", d), ("KIOKU_PORT", "x")])).is_err()
        );
    }

    #[test]
    fn data_dir_override_from_file() {
        let dir = tempfile::tempdir().unwrap();
        let other = dir.path().join("elsewhere");
        std::fs::write(
            dir.path().join(CONFIG_FILE),
            format!("[server]\ndata_dir = \"{}\"\n", other.display()),
        )
        .unwrap();
        let c = Config::load_from_dir(dir.path(), &HashMap::new()).unwrap();
        assert_eq!(c.data_dir, other);
        assert_eq!(c.config_file, dir.path().join(CONFIG_FILE));
        // env beats file
        let d = dir.path().to_str().unwrap();
        let c = Config::load_with_env(&env(&[("KIOKU_DATA_DIR", d)])).unwrap();
        assert_eq!(c.data_dir, dir.path());
    }

    #[test]
    fn save_roundtrip_and_client_only() {
        let dir = tempfile::tempdir().unwrap();
        let mut c = Config::for_data_dir(dir.path());
        c.server.auth_token = Some("t".into());
        c.save().unwrap();
        let back = Config::load_file(&c.config_file).unwrap();
        assert_eq!(back, c);

        let path = dir.path().join("client").join(CONFIG_FILE);
        let cc = Config::write_client_only(&path, "http://home:7391", "tok").unwrap();
        let text = std::fs::read_to_string(&path).unwrap();
        assert!(text.contains("[client]"));
        assert!(!text.contains("[server]"));
        assert_eq!(cc.client.auth_token.as_deref(), Some("tok"));
        let back = Config::load_file(&path).unwrap();
        assert_eq!(back.client.server_url, "http://home:7391");
    }
}
