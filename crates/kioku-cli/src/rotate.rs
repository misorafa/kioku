//! `kioku rotate-token` (SPEC-M2 §21): replaces the server's auth token in config.toml,
//! restarts the service and prints the `kioku setup --client-only` line for every other
//! machine. Agents hold no token since v0.4.0, so config.toml is all there is to update.

use std::time::Instant;

use anyhow::Context;
use kioku_core::Config;
use kioku_core::config::CONFIG_FILE;

use crate::setup::{SetupEnv, check_status, client_url, has_server_section, points_at_own_server};

/// What `rotate-token` did: lines to print and the exit code.
#[derive(Debug, Default)]
pub struct RotateReport {
    /// Output lines (some contain the new token).
    pub lines: Vec<String>,
    /// 0 on success.
    pub exit_code: i32,
}

impl RotateReport {
    fn fail(mut self, msg: impl Into<String>) -> RotateReport {
        self.lines.push(format!("kioku: error: {}", msg.into()));
        self.exit_code = 1;
        self
    }
}

/// Runs `kioku rotate-token [--dry-run]` against the config in `env`.
pub fn run_rotate(dry_run: bool, env: &SetupEnv) -> RotateReport {
    let mut r = RotateReport::default();
    let path = env.config_dir().join(CONFIG_FILE);
    if !path.exists() || !has_server_section(&path) {
        return r.fail(format!(
            "{} has no [server] section: run kioku rotate-token on the server machine",
            path.display()
        ));
    }
    if env
        .vars
        .get("KIOKU_AUTH_TOKEN")
        .is_some_and(|v| !v.is_empty())
    {
        return r.fail(
            "KIOKU_AUTH_TOKEN is set in the environment and overrides config.toml: change that variable (e.g. in the container) instead",
        );
    }
    let mut cfg = match Config::load_file(&path) {
        Ok(c) => c,
        Err(e) => return r.fail(format!("{e:#}")),
    };
    let old = cfg.server.auth_token.clone().unwrap_or_default();
    let token = kioku_core::util::generate_token();
    let client_follows = points_at_own_server(&cfg.client.server_url, &cfg)
        || cfg.client.auth_token.as_deref() == Some(old.as_str());
    let url = client_url(&cfg);
    if dry_run {
        r.lines.push(format!(
            "would write a new [server] auth_token{} to {} and restart the service",
            if client_follows {
                " (and [client])"
            } else {
                ""
            },
            path.display()
        ));
        return r;
    }
    cfg.server.auth_token = Some(token.clone());
    if client_follows {
        cfg.client.auth_token = Some(token.clone());
    }
    if let Err(e) = cfg
        .save()
        .with_context(|| format!("writing {}", path.display()))
    {
        return r.fail(format!("{e:#}"));
    }
    r.lines
        .push(format!("wrote a new auth token to {}", path.display()));

    // Restart and wait until the server accepts the new token.
    let manager = env.service_manager(&cfg.data_dir);
    if manager.is_installed() {
        if let Err(e) = manager.restart() {
            return r.fail(format!(
                "{e:#}; restart it yourself (kioku service stop && kioku service start)"
            ));
        }
        let mut local = cfg.client.clone();
        local.server_url = format!("http://127.0.0.1:{}", cfg.server.port);
        local.auth_token = Some(token.clone());
        let deadline = Instant::now() + env.poll_timeout;
        loop {
            if check_status(&local, env.request_timeout).is_ok() {
                r.lines.push(format!(
                    "restarted {}; the server accepts only the new token now",
                    manager.describe()
                ));
                break;
            }
            if Instant::now() >= deadline {
                return r.fail(format!(
                    "{} did not accept the new token within {} s; see kioku service logs",
                    manager.describe(),
                    env.poll_timeout.as_secs()
                ));
            }
            std::thread::sleep(env.poll_interval);
        }
    } else {
        r.lines.push(
            "no kioku service installed: restart kioku serve (or its container) so it loads the new token"
                .into(),
        );
    }
    r.lines.push(String::new());
    r.lines.push(
        "On every other machine (this line contains the new token - keep it private):".into(),
    );
    r.lines
        .push(format!("  kioku setup --client-only {} {token}", url.url));
    r.lines.extend(url.alternative_line());
    r
}
