//! `kioku rotate-token` (SPEC-M2 §21, SPEC-M2.7 §10, §12): replaces the server's auth token
//! in config.toml (only the `auth_token` lines; comments and everything else stay), restarts
//! the service, checks the new token against this server's own URL and prints a `kioku
//! invite` line (an invite with the new token, valid 30 minutes) for the other machines.
//! The token itself is printed only with `--show-token`. Agents hold no token since v0.4.0,
//! so config.toml is all there is to update.

use std::time::{Duration, Instant};

use kioku_core::config::CONFIG_FILE;
use kioku_core::{ClientConfig, Config};
use serde_json::{Value, json};

use crate::client::ApiClient;
use crate::invite::{own_server_url, render_invite};
use crate::setup::{SetupEnv, check_status, client_url, has_server_section, points_at_own_server};

/// Lifetime of the invite `rotate-token` prints, in minutes.
pub const ROTATE_INVITE_TTL: u32 = 30;

/// What `rotate-token` did: lines to print and the exit code.
#[derive(Debug, Default)]
pub struct RotateReport {
    /// Output lines (they contain the new token only with `--show-token`).
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

/// Runs `kioku rotate-token [--dry-run] [--show-token]` against the config in `env`.
pub fn run_rotate(dry_run: bool, show_token: bool, env: &SetupEnv) -> RotateReport {
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
    let cfg = match Config::load_file(&path) {
        Ok(c) => c,
        Err(e) => return r.fail(format!("{e:#}")),
    };
    let old = cfg.server.auth_token.clone().unwrap_or_default();
    let token = kioku_core::util::generate_token();
    let client_follows = points_at_own_server(&cfg.client.server_url, &cfg)
        || cfg.client.auth_token.as_deref() == Some(old.as_str());
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
    if let Err(e) = cfg.save_auth_tokens(&token, client_follows.then_some(token.as_str())) {
        return r.fail(format!("{e:#}"));
    }
    r.lines
        .push(format!("wrote a new auth token to {}", path.display()));

    // Restart and wait until the server accepts the new token — asked at the address it
    // actually listens on (its bind), not always 127.0.0.1.
    let own = ClientConfig {
        server_url: own_server_url(&cfg),
        auth_token: Some(token.clone()),
        ..cfg.client.clone()
    };
    let manager = env.service_manager(&cfg.data_dir);
    let restarted = manager.is_installed();
    if restarted {
        if let Err(e) = manager.restart() {
            return r.fail(format!(
                "{e:#}; restart it yourself (kioku service stop && kioku service start)"
            ));
        }
        let deadline = Instant::now() + env.poll_timeout;
        loop {
            if check_status(&own, env.request_timeout).is_ok() {
                r.lines.push(format!(
                    "restarted {}; the server accepts only the new token now",
                    manager.describe()
                ));
                break;
            }
            if Instant::now() >= deadline {
                return r.fail(format!(
                    "{} did not accept the new token at {} within {} s; see kioku service logs",
                    manager.describe(),
                    own.server_url,
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
    let url = client_url(&cfg);
    // SPEC-M2.7 §10: an invite carries the new token; the token itself is not printed.
    let invite = restarted
        .then(|| create_invite(&own, env.request_timeout))
        .and_then(Result::ok);
    match invite {
        Some((code, uses)) => {
            r.lines.push(
                "Update every other machine with this invite (valid 30 minutes; `kioku invite --uses <n>` makes more):"
                    .into(),
            );
            r.lines.extend(
                render_invite(&code, &url, ROTATE_INVITE_TTL, uses)
                    .lines()
                    .map(str::to_string),
            );
        }
        None => r.lines.push(
            "Update every other machine: once the server runs with the new token, run `kioku invite --uses <n>` here and paste the line it prints on each of them."
                .into(),
        ),
    }
    if show_token {
        r.lines.push(String::new());
        r.lines.push(
            "Manually, on every other machine (this contains the new token - keep it private):"
                .into(),
        );
        r.lines.push(format!(
            "  KIOKU_CLIENT_TOKEN='{token}' kioku setup --client-only {}",
            url.url
        ));
        r.lines.push(format!(
            "  PowerShell: $env:KIOKU_CLIENT_TOKEN='{token}'; kioku setup --client-only {}",
            url.url
        ));
        r.lines.extend(url.alternative_line());
    }
    r
}

/// `POST /api/v1/invites` on this server with the new token: `(code, uses)`.
fn create_invite(own: &ClientConfig, timeout: Duration) -> anyhow::Result<(String, u64)> {
    let resp = ApiClient::new(own, timeout.max(Duration::from_secs(5)))?.post(
        &["invites"],
        &json!({ "ttl_minutes": ROTATE_INVITE_TTL, "uses": 1 }),
    )?;
    let code = resp
        .get("code")
        .and_then(Value::as_str)
        .ok_or_else(|| anyhow::anyhow!("unexpected answer from POST /api/v1/invites"))?;
    Ok((
        code.to_string(),
        resp.get("uses").and_then(Value::as_u64).unwrap_or(1),
    ))
}
