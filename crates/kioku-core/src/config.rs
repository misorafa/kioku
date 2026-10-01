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
    /// Snapshots `POST /api/v1/backup` keeps (SPEC-M2.7 §12). Superseded by
    /// `[retention] backups_keep` (SPEC-M2.8 §3) and read only when that is not set; see
    /// [`Config::backups_keep`]. Not written while it is the default.
    #[serde(skip_serializing_if = "is_default_backup_keep")]
    pub backup_keep: usize,
}

/// Default of `[server] backup_keep`.
pub const DEFAULT_BACKUP_KEEP: usize = 10;

fn is_default_backup_keep(n: &usize) -> bool {
    *n == DEFAULT_BACKUP_KEEP
}

impl Default for ServerConfig {
    fn default() -> ServerConfig {
        ServerConfig {
            bind: "127.0.0.1".to_string(),
            port: DEFAULT_PORT,
            auth_token: None,
            data_dir: None,
            summary_lang: Lang::Ja,
            backup_keep: DEFAULT_BACKUP_KEEP,
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
    /// Append every raw hook payload to `logs/hook-dump.jsonl` (M2 §3.8; also `KIOKU_HOOK_DUMP=1`).
    pub hook_dump: bool,
    /// Deliver the `<kioku>` block on the first Cursor tool use of a session (M2 §5.6).
    pub cursor_late_context: bool,
}

impl Default for ClientConfig {
    fn default() -> ClientConfig {
        ClientConfig {
            server_url: format!("http://127.0.0.1:{DEFAULT_PORT}"),
            auth_token: None,
            timeout_ms: DEFAULT_TIMEOUT_MS,
            stop_nudge: true,
            lang: Lang::Ja,
            hook_dump: false,
            cursor_late_context: true,
        }
    }
}

/// `[update]` section (SPEC-M2.5 §2): automatic updates, read by both roles.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default)]
pub struct UpdateConfig {
    /// Update automatically (`false` = notify only). Env override `KIOKU_AUTO_UPDATE`.
    pub auto: bool,
    /// Release channel; only `stable` (tags without `-`) exists in M2.5.
    pub channel: String,
    /// Honour `KIOKU_DOWNLOAD_BASE` / `KIOKU_REPO` (a release mirror or fork; SPEC-M2.7 §11).
    /// Off by default: an environment variable alone cannot redirect updates.
    pub allow_mirror: bool,
}

impl Default for UpdateConfig {
    fn default() -> UpdateConfig {
        UpdateConfig {
            auto: true,
            channel: "stable".to_string(),
            allow_mirror: false,
        }
    }
}

impl UpdateConfig {
    /// True when every field has its default (the table is then not written).
    pub fn is_default(&self) -> bool {
        *self == UpdateConfig::default()
    }
}

/// `[retention]` section (SPEC-M2.8 §3): how long the server keeps raw logs, observation
/// payloads, backups and hook dumps. `0` days turns that category off.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default)]
pub struct RetentionConfig {
    /// `raw/<project>/<session>.jsonl` older than this are gzipped, and deleted at twice
    /// this age.
    pub raw_days: u32,
    /// Payloads of finalized sessions older than this are reduced to a stub.
    pub observations_days: u32,
    /// Backups kept (`None` = `[server] backup_keep`, which defaults to 10).
    #[serde(skip_serializing_if = "Option::is_none")]
    pub backups_keep: Option<usize>,
    /// `logs/hook-dump.jsonl*` older than this are deleted.
    pub hook_dump_days: u32,
    /// Run the policy daily inside `kioku serve`.
    pub auto: bool,
}

/// Default of `[retention] raw_days`.
pub const DEFAULT_RAW_DAYS: u32 = 90;
/// Default of `[retention] observations_days`.
pub const DEFAULT_OBSERVATIONS_DAYS: u32 = 180;
/// Default of `[retention] hook_dump_days`.
pub const DEFAULT_HOOK_DUMP_DAYS: u32 = 7;

impl Default for RetentionConfig {
    fn default() -> RetentionConfig {
        RetentionConfig {
            raw_days: DEFAULT_RAW_DAYS,
            observations_days: DEFAULT_OBSERVATIONS_DAYS,
            backups_keep: None,
            hook_dump_days: DEFAULT_HOOK_DUMP_DAYS,
            auto: true,
        }
    }
}

impl RetentionConfig {
    /// True when every field has its default (the table is then not written).
    pub fn is_default(&self) -> bool {
        *self == RetentionConfig::default()
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
    /// `[update]` settings (not written while they are the defaults: setup never adds it).
    #[serde(default, skip_serializing_if = "UpdateConfig::is_default")]
    pub update: UpdateConfig,
    /// `[retention]` settings (SPEC-M2.8 §3; not written while they are the defaults).
    #[serde(default, skip_serializing_if = "RetentionConfig::is_default")]
    pub retention: RetentionConfig,
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
    #[serde(skip_serializing_if = "UpdateConfig::is_default")]
    update: &'a UpdateConfig,
}

impl Config {
    /// Defaults rooted at `dir` (used for tests and as the base of loading).
    pub fn for_data_dir(dir: &Path) -> Config {
        Config {
            server: ServerConfig::default(),
            client: ClientConfig::default(),
            update: UpdateConfig::default(),
            retention: RetentionConfig::default(),
            data_dir: dir.to_path_buf(),
            config_file: dir.join(CONFIG_FILE),
        }
    }

    /// Loads config using the real process environment.
    pub fn load() -> Result<Config> {
        let env: HashMap<String, String> = util::env_vars();
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
        if let Some(v) = get("KIOKU_AUTO_UPDATE") {
            self.update.auto = !matches!(v.as_str(), "0" | "false" | "off" | "no");
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
                update: &config.update,
            })
            .context("serializing client config")?
        };
        write_file(path, &text)?;
        Ok(config)
    }

    /// Writes a client-only config.toml pointing at `server_url`, dropping any `[server]`
    /// section: the machine becomes a client of another server (SPEC-M2 §11 step 2).
    pub fn write_client_only_dropping_server(
        path: &Path,
        server_url: &str,
        token: &str,
    ) -> Result<Config> {
        let mut config = Config::load_file(path)?;
        config.client.server_url = server_url.to_string();
        config.client.auth_token = Some(token.to_string());
        let text = toml::to_string_pretty(&ClientOnlyFile {
            client: &config.client,
            update: &config.update,
        })
        .context("serializing client config")?;
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

    /// Backups to keep: `[retention] backups_keep`, else the older `[server] backup_keep`
    /// (SPEC-M2.8 §3); at least 1.
    pub fn backups_keep(&self) -> usize {
        self.retention
            .backups_keep
            .unwrap_or(self.server.backup_keep)
            .max(1)
    }
}

/// `text` (a config.toml) with the `auth_token` of `[server]` and, when given, of `[client]`
/// replaced; every other byte (comments, order, line endings) is kept (SPEC-M2.7 §12). A
/// section without an `auth_token` line gets one right after its header; a missing
/// `[client]` section is appended.
pub fn replace_auth_tokens(text: &str, server: &str, client: Option<&str>) -> String {
    fn header(line: &str) -> Option<String> {
        let t = line.trim();
        (t.starts_with('[') && !t.starts_with("[[")).then(|| {
            t.trim_start_matches('[')
                .split(']')
                .next()
                .unwrap_or("")
                .trim()
                .to_string()
        })
    }
    fn is_token_line(line: &str) -> bool {
        line.trim()
            .strip_prefix("auth_token")
            .is_some_and(|rest| rest.trim_start().starts_with('='))
    }
    fn eol_of(line: &str) -> &'static str {
        if line.ends_with("\r\n") { "\r\n" } else { "\n" }
    }
    let token_line = |indent: &str, token: &str, eol: &str| {
        format!(
            "{indent}auth_token = {}{eol}",
            toml::Value::String(token.to_string())
        )
    };
    let want = |section: &str| match section {
        "server" => Some(server),
        "client" => client,
        _ => None,
    };
    // Sections that already have an `auth_token` line.
    let mut has_line: Vec<String> = Vec::new();
    let mut section = String::new();
    for line in text.split_inclusive('\n') {
        if let Some(name) = header(line) {
            section = name;
        } else if is_token_line(line) {
            has_line.push(section.clone());
        }
    }
    let mut out = String::with_capacity(text.len() + 64);
    let mut done: Vec<String> = Vec::new();
    section.clear();
    for line in text.split_inclusive('\n') {
        if let Some(name) = header(line) {
            section = name;
            out.push_str(line);
            if let Some(token) = want(&section)
                && !has_line.contains(&section)
                && !done.contains(&section)
            {
                if !line.ends_with('\n') {
                    out.push('\n');
                }
                out.push_str(&token_line("", token, eol_of(line)));
                done.push(section.clone());
            }
            continue;
        }
        if is_token_line(line)
            && let Some(token) = want(&section)
            && !done.contains(&section)
        {
            let indent = &line[..line.len() - line.trim_start().len()];
            let eol = if line.ends_with('\n') {
                eol_of(line)
            } else {
                ""
            };
            out.push_str(&token_line(indent, token, eol));
            done.push(section.clone());
            continue;
        }
        out.push_str(line);
    }
    if let Some(token) = client
        && !done.iter().any(|s| s == "client")
    {
        if !out.is_empty() && !out.ends_with('\n') {
            out.push('\n');
        }
        out.push_str(&format!("\n[client]\n{}", token_line("", token, "\n")));
    }
    out
}

impl Config {
    /// Writes new auth tokens into `config_file` by editing only the `auth_token` lines
    /// ([`replace_auth_tokens`]); the rest of the file stays byte-identical.
    pub fn save_auth_tokens(&self, server: &str, client: Option<&str>) -> Result<()> {
        let text = std::fs::read_to_string(&self.config_file)
            .with_context(|| format!("reading {}", self.config_file.display()))?;
        write_file(
            &self.config_file,
            &replace_auth_tokens(&text, server, client),
        )
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
        assert!(!c.client.hook_dump);
        assert!(c.client.cursor_late_context);
        assert!(c.is_loopback_bind());
    }

    #[test]
    fn file_then_env_overrides() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(
            dir.path().join(CONFIG_FILE),
            "[server]\nbind = \"0.0.0.0\"\nport = 9000\nauth_token = \"filetok\"\nsummary_lang = \"en\"\n\n[client]\nserver_url = \"http://home:9000\"\ntimeout_ms = 500\nlang = \"en\"\nhook_dump = true\ncursor_late_context = false\n",
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
        assert!(c.client.hook_dump);
        assert!(!c.client.cursor_late_context);

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
    fn update_table_defaults_file_and_env() {
        let dir = tempfile::tempdir().unwrap();
        let d = dir.path().to_str().unwrap();
        let c = Config::load_with_env(&env(&[("KIOKU_DATA_DIR", d)])).unwrap();
        assert!(c.update.auto);
        assert_eq!(c.update.channel, "stable");
        // Defaults are never written (setup / join do not add the table).
        assert!(!c.to_toml().unwrap().contains("[update]"));
        std::fs::write(
            dir.path().join(CONFIG_FILE),
            "[client]\nserver_url = \"http://h:1\"\n\n[update]\nauto = false\n",
        )
        .unwrap();
        let c = Config::load_with_env(&env(&[("KIOKU_DATA_DIR", d)])).unwrap();
        assert!(!c.update.auto);
        assert_eq!(c.update.channel, "stable");
        let on = env(&[("KIOKU_DATA_DIR", d), ("KIOKU_AUTO_UPDATE", "1")]);
        assert!(Config::load_with_env(&on).unwrap().update.auto);
        let off = env(&[("KIOKU_DATA_DIR", d), ("KIOKU_AUTO_UPDATE", "0")]);
        assert!(!Config::load_with_env(&off).unwrap().update.auto);
        // A user's `auto = false` survives rewriting the client-only file.
        let path = dir.path().join(CONFIG_FILE);
        Config::write_client_only(&path, "http://h:2", "t").unwrap();
        assert!(
            std::fs::read_to_string(&path)
                .unwrap()
                .contains("auto = false")
        );
    }

    /// SPEC-M2.8 §3: `[retention]` with defaults; `backups_keep` falls back to the M2.7
    /// `[server] backup_keep`.
    #[test]
    fn retention_table_and_backup_keep_fallback() {
        let dir = tempfile::tempdir().unwrap();
        let cfg = Config::load_from_dir(dir.path(), &HashMap::new()).unwrap();
        assert_eq!(cfg.retention, RetentionConfig::default());
        assert_eq!(cfg.backups_keep(), DEFAULT_BACKUP_KEEP);
        assert!(!cfg.to_toml().unwrap().contains("[retention]"));
        std::fs::write(
            dir.path().join(CONFIG_FILE),
            "[server]\nbackup_keep = 4\n\n[retention]\nraw_days = 30\nauto = false\n",
        )
        .unwrap();
        let cfg = Config::load_from_dir(dir.path(), &HashMap::new()).unwrap();
        assert_eq!(cfg.retention.raw_days, 30);
        assert_eq!(cfg.retention.observations_days, DEFAULT_OBSERVATIONS_DAYS);
        assert!(!cfg.retention.auto);
        assert_eq!(cfg.backups_keep(), 4, "the old setting is still read");
        std::fs::write(
            dir.path().join(CONFIG_FILE),
            "[server]\nbackup_keep = 4\n\n[retention]\nbackups_keep = 7\n",
        )
        .unwrap();
        let cfg = Config::load_from_dir(dir.path(), &HashMap::new()).unwrap();
        assert_eq!(cfg.backups_keep(), 7, "[retention] wins");
        assert!(cfg.to_toml().unwrap().contains("backups_keep = 7"));
    }

    #[test]
    fn data_dir_override_from_file() {
        let dir = tempfile::tempdir().unwrap();
        let other = dir.path().join("elsewhere");
        std::fs::write(
            dir.path().join(CONFIG_FILE),
            // A TOML literal string, so Windows backslashes are not escapes.
            format!("[server]\ndata_dir = '{}'\n", other.display()),
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

    /// SPEC-M2.7 §12: rotating the token keeps comments and everything else byte-identical.
    #[test]
    fn replacing_auth_tokens_keeps_the_rest_of_the_file() {
        let text = "# kioku の設定（手で書いたコメント）\n[server]\nbind = \"0.0.0.0\"   # LAN\nauth_token = \"old\"\n\n[client]\n  auth_token=\"old\"\nserver_url = \"http://127.0.0.1:7391\"\n\n[update]\nauto = false # 手動\n";
        let out = replace_auth_tokens(text, "new-s", Some("new-c"));
        assert_eq!(
            out,
            text.replace("auth_token = \"old\"", "auth_token = \"new-s\"")
                .replace("  auth_token=\"old\"", "  auth_token = \"new-c\"")
        );
        // Without the client: only [server] changes.
        let out = replace_auth_tokens(text, "new-s", None);
        assert!(out.contains("  auth_token=\"old\"\n"));
        // CRLF files stay CRLF; a section without the line gets it after its header; a
        // missing [client] is appended.
        let crlf = "[server]\r\nport = 7391\r\n# auth_token = \"commented\"\r\n";
        let out = replace_auth_tokens(crlf, "t", Some("c"));
        assert_eq!(
            out,
            "[server]\r\nauth_token = \"t\"\r\nport = 7391\r\n# auth_token = \"commented\"\r\n\n[client]\nauth_token = \"c\"\n"
        );
        let dir = tempfile::tempdir().unwrap();
        let mut c = Config::for_data_dir(dir.path());
        std::fs::write(&c.config_file, text).unwrap();
        c.save_auth_tokens("x", Some("x")).unwrap();
        c = Config::load_file(&c.config_file).unwrap();
        assert_eq!(c.server.auth_token.as_deref(), Some("x"));
        assert_eq!(c.client.auth_token.as_deref(), Some("x"));
        assert!(!c.update.auto);
        let saved = std::fs::read_to_string(&c.config_file).unwrap();
        assert!(saved.starts_with("# kioku の設定（手で書いたコメント）\n"));
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
