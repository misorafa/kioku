//! `kioku invite` and `kioku join` (SPEC-M2.3 §5): the server owner gets one line to paste
//! on a new machine; that machine exchanges the one-time code for the token and runs the
//! `setup --client-only` flow. The token is never printed.

use std::time::Duration;

/// install.ps1 on GitHub (https): what the Windows invite line fetches (SPEC-M2.3 §9).
pub const WINDOWS_INSTALLER: &str =
    "https://raw.githubusercontent.com/misorafa/kioku/main/install.ps1";
/// install.sh on GitHub (https): what the macOS / Linux invite line fetches (SPEC-M2.7 §4).
pub const UNIX_INSTALLER: &str = "https://raw.githubusercontent.com/misorafa/kioku/main/install.sh";

use kioku_core::config::CONFIG_FILE;
use kioku_core::{ClientConfig, Config};
use serde_json::{Value, json};

use crate::client::{ApiClient, http_status};
use crate::event::{ALL_AGENTS, Agent};
use crate::setup::{Mark, SetupEnv, SetupOptions, client_url, has_server_section, run_setup};

/// Flags of `kioku invite`.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct InviteOptions {
    /// `--ttl <minutes>` (the server caps it at 60).
    pub ttl_minutes: u32,
    /// `--uses <n>` (the server caps it at 20).
    pub uses: u32,
    /// `--host <addr[:port]>`: the address the other machine uses (SPEC-M2.7 §4).
    pub host: Option<String>,
}

impl Default for InviteOptions {
    fn default() -> Self {
        InviteOptions {
            ttl_minutes: 10,
            uses: 1,
            host: None,
        }
    }
}

/// Output of `invite` / `join`: text for stdout, text for stderr, exit code.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct CommandReport {
    /// Printed on stdout.
    pub stdout: String,
    /// Printed on stderr.
    pub stderr: String,
    /// Process exit code.
    pub exit_code: i32,
}

impl CommandReport {
    fn fail(msg: impl Into<String>) -> CommandReport {
        let mut stderr = msg.into();
        if !stderr.ends_with('\n') {
            stderr.push('\n');
        }
        CommandReport {
            stdout: String::new(),
            stderr,
            exit_code: 1,
        }
    }
}

/// The URL this machine's own server answers on: loopback unless it binds one address.
pub fn own_server_url(cfg: &Config) -> String {
    let bind = cfg.server.bind.trim();
    let host = match bind {
        "" | "0.0.0.0" => "127.0.0.1".to_string(),
        "::" | "[::]" => "[::1]".to_string(),
        b if b.eq_ignore_ascii_case("localhost") => "127.0.0.1".to_string(),
        b if b.contains(':') && !b.starts_with('[') => format!("[{b}]"),
        b => b.to_string(),
    };
    format!("http://{host}:{}", cfg.server.port)
}

/// Runs `kioku invite`: creates an invite on this machine's own server and returns the
/// lines to paste on the new machine.
pub fn run_invite(opts: &InviteOptions, env: &SetupEnv) -> CommandReport {
    let config_dir = env.config_dir();
    let path = config_dir.join(CONFIG_FILE);
    // Docker: no config.toml, the server's token comes from KIOKU_AUTH_TOKEN.
    let env_token = env
        .vars
        .get("KIOKU_AUTH_TOKEN")
        .is_some_and(|t| !t.trim().is_empty());
    let is_server = if path.exists() {
        has_server_section(&path)
    } else {
        env_token
    };
    if !is_server {
        return CommandReport::fail(format!(
            "kioku: この機械はサーバーではありません。kioku invite はサーバー機で実行してください。\n\
             kioku: {} has no [server] section: run kioku invite on the server machine.",
            path.display()
        ));
    }
    let cfg = match Config::load_from_dir(&config_dir, &env.vars) {
        Ok(c) => c,
        Err(e) => return CommandReport::fail(format!("kioku: error: {e:#}")),
    };
    let token = cfg.server.auth_token.clone().unwrap_or_default();
    if token.trim().is_empty() {
        return CommandReport::fail(format!(
            "kioku: {} has no [server] auth_token: run kioku setup first.",
            path.display()
        ));
    }
    let own = own_server_url(&cfg);
    let client = ClientConfig {
        server_url: own.clone(),
        auth_token: Some(token),
        ..cfg.client.clone()
    };
    let body = json!({ "ttl_minutes": opts.ttl_minutes, "uses": opts.uses });
    let created = ApiClient::new(&client, env.request_timeout.max(Duration::from_secs(5)))
        .and_then(|c| c.post(&["invites"], &body));
    let resp = match created {
        Ok(v) => v,
        Err(e) => {
            let why = match http_status(&e) {
                Some(404) => "the running server is older than this kioku and has no invites: restart it (kioku service stop && kioku service start)".to_string(),
                Some(401) => "the running server rejected this machine's [server] auth_token (it runs with another token): restart it (kioku service stop && kioku service start)".to_string(),
                Some(_) => format!("{e:#}"),
                None => format!("the kioku server does not answer at {own} ({e:#}): start it with kioku service start (or kioku setup)"),
            };
            return CommandReport::fail(format!("kioku: error: {why}"));
        }
    };
    let Some(code) = resp.get("code").and_then(Value::as_str) else {
        return CommandReport::fail("kioku: error: unexpected answer from POST /api/v1/invites");
    };
    let uses = resp.get("uses").and_then(Value::as_u64).unwrap_or(1);
    let minutes = opts.ttl_minutes.clamp(1, 60);
    let (url, others) = match opts.host.as_deref() {
        Some(host) => match host_url(host, cfg.server.port) {
            Some(u) => (
                crate::setup::ClientUrl {
                    url: u,
                    mdns_alternative: None,
                    loopback_note: None,
                },
                Vec::new(),
            ),
            None => {
                return CommandReport::fail(format!(
                    "kioku: error: --host {host:?} is not a host name or address (e.g. 192.168.1.5 or mini.local:7391)"
                ));
            }
        },
        None => {
            let url = client_url(&cfg);
            let wildcard = matches!(cfg.server.bind.trim(), "0.0.0.0" | "::" | "[::]" | "");
            let others = if wildcard {
                alternative_urls(&url.url, &local_ipv4s(), cfg.server.port)
            } else {
                Vec::new()
            };
            (url, others)
        }
    };
    let mut stdout = render_invite(code, &url, minutes, uses);
    if !others.is_empty() {
        stdout.push_str(
            "\nこの機械の他のアドレス（上の行のアドレスを置き換えて使う）/ Other addresses of this machine (replace the address in the line above):\n",
        );
        for o in &others {
            stdout.push_str(&format!("  {o}\n"));
        }
        stdout.push_str("（kioku invite --host <address> で行そのものを変えられます / kioku invite --host <address> prints the lines for one of them）\n");
    }
    CommandReport {
        stdout,
        stderr: String::new(),
        exit_code: 0,
    }
}

/// `http://<host>[:port]` for `--host` (a name, an IPv4/IPv6 address, optionally with a
/// port; `http://` may be given); `None` when it is not a plain host.
pub fn host_url(host: &str, default_port: u16) -> Option<String> {
    let h = host.trim().trim_end_matches('/');
    let h = h.strip_prefix("http://").unwrap_or(h);
    if h.is_empty() || h.contains(['/', '@', ' ', '\'', '"', '`', '$']) {
        return None;
    }
    let with_port = if h.starts_with('[') {
        if h.contains("]:") {
            h.to_string()
        } else {
            format!("{h}:{default_port}")
        }
    } else if h.parse::<std::net::Ipv6Addr>().is_ok() {
        format!("[{h}]:{default_port}")
    } else if h.contains(':') {
        h.to_string()
    } else {
        format!("{h}:{default_port}")
    };
    let url = format!("http://{with_port}");
    let parsed = reqwest::Url::parse(&url).ok()?;
    (parsed.host_str().is_some() && parsed.path() == "/").then_some(url)
}

/// This machine's IPv4 addresses that other machines might use: no loopback, link-local
/// or unspecified; private (LAN) ones first, then the rest (e.g. a VPN), each once. Read
/// from `ip` / `ifconfig` / `ipconfig` output (no crate; empty when none of them runs).
pub fn local_ipv4s() -> Vec<std::net::Ipv4Addr> {
    let tries: Vec<(&str, &[&str])> = if cfg!(windows) {
        vec![("ipconfig", &[])]
    } else {
        vec![
            ("ip", &["-4", "-o", "addr", "show"]),
            ("ifconfig", &[]),
            ("/sbin/ifconfig", &[]),
        ]
    };
    for (program, args) in tries {
        let mut cmd = kioku_core::util::quiet_command(program);
        cmd.args(args);
        if let Some(out) = kioku_core::util::output_with_deadline(cmd, Duration::from_secs(3))
            && out.status.success()
        {
            let found = parse_ipv4s(&String::from_utf8_lossy(&out.stdout));
            if !found.is_empty() {
                return found;
            }
        }
    }
    Vec::new()
}

/// The usable IPv4 addresses in `ip -o addr` / `ifconfig` / `ipconfig` output, LAN first.
pub fn parse_ipv4s(text: &str) -> Vec<std::net::Ipv4Addr> {
    let mut found: Vec<std::net::Ipv4Addr> = Vec::new();
    for line in text.lines() {
        let words: Vec<&str> = line
            .split(|c: char| c.is_whitespace() || c == ':')
            .filter(|w| !w.is_empty())
            .collect();
        // `inet 192.168.1.5/24` (ip), `inet 192.168.1.5 netmask …` (ifconfig), or an
        // `IPv4 …: 192.168.1.5` line of ipconfig (any language; "(Preferred)" is cut).
        let candidate = if let Some(i) = words.iter().position(|w| *w == "inet") {
            words.get(i + 1).copied()
        } else if line.contains("IPv4") {
            words.last().copied()
        } else {
            None
        };
        let Some(c) = candidate else { continue };
        let c = c
            .split(['/', '('])
            .next()
            .unwrap_or(c)
            .trim_start_matches("addr");
        let Ok(ip) = c.parse::<std::net::Ipv4Addr>() else {
            continue;
        };
        if ip.is_loopback() || ip.is_link_local() || ip.is_unspecified() || found.contains(&ip) {
            continue;
        }
        found.push(ip);
    }
    found.sort_by_key(|ip| !ip.is_private());
    found
}

/// `http://<ip>:<port>` for every address in `ips` that is not already `current`'s host.
pub fn alternative_urls(current: &str, ips: &[std::net::Ipv4Addr], port: u16) -> Vec<String> {
    ips.iter()
        .map(|ip| format!("http://{ip}:{port}"))
        .filter(|u| u != current)
        .collect()
}

/// The text `kioku invite` prints (SPEC-M2.3 §2).
pub fn render_invite(code: &str, url: &crate::setup::ClientUrl, minutes: u32, uses: u64) -> String {
    let (valid_ja, valid_en) = if uses <= 1 {
        (
            format!("{minutes} 分間・1 回だけ有効"),
            format!("valid {minutes} minutes, once"),
        )
    } else {
        (
            format!("{minutes} 分間・{uses} 台まで有効"),
            format!("valid {minutes} minutes, up to {uses} machines"),
        )
    };
    let mut out = String::new();
    if let Some(note) = &url.loopback_note {
        out.push_str(note);
        out.push_str("\n\n");
    }
    out.push_str(&format!(
        "追加するマシンで、次のどちらか 1 行を貼り付けてください（{valid_ja}）:\n"
    ));
    out.push_str(&format!(
        "Paste ONE of these on the machine to add ({valid_en}):\n\n"
    ));
    // SPEC-M2.3 §9: the Windows line fetches install.ps1 from GitHub over https and passes the
    // invite in KIOKU_JOIN — Defender flagged `powershell -ExecutionPolicy Bypass -c irm
    // http://<LAN IP>/… | iex` as Trojan:Win32/Commando.A!ml.
    // SPEC-M2.7 §4: the macOS / Linux line works the same way — the script from GitHub over
    // https, only the code over the LAN (the server no longer serves a script over http).
    let join = url.url.trim_start_matches("http://");
    out.push_str(&format!(
        "  Windows (PowerShell):  $env:KIOKU_JOIN='{join}/{code}'; irm {WINDOWS_INSTALLER} | iex\n"
    ));
    out.push_str(&format!(
        "  macOS / Linux / Git Bash:  KIOKU_JOIN='{join}/{code}' sh -c \"$(curl -fsSL {UNIX_INSTALLER})\"\n"
    ));
    if let Some(alt) = &url.mdns_alternative {
        out.push_str(&format!(
            "\n(On this LAN you can also use {alt}/…; over a VPN use the IP.)\n"
        ));
    }
    out
}

/// Flags of `kioku join` (the `kioku setup` flags that make sense for a client).
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct JoinOptions {
    /// `--agents a,b`.
    pub agents: Vec<Agent>,
    /// `--no-agents`.
    pub no_agents: bool,
    /// `--no-instructions`.
    pub no_instructions: bool,
    /// `--mcp-http`.
    pub mcp_http: bool,
}

/// `http://` added when the scheme is missing; no trailing slash.
fn normalize_url(url: &str) -> String {
    let url = url.trim().trim_end_matches('/');
    if url.contains("://") {
        url.to_string()
    } else {
        format!("http://{url}")
    }
}

/// The one sentence (ja + en) for an invite the server refused.
const INVALID: &str = "kioku: この招待コードは無効か、期限切れか、使用済みです。サーバーで kioku invite をもう一度実行してもらい、新しい行を貼り付けてください。\n\
kioku: This invite code is invalid, expired or already used: ask for a new kioku invite on the server and paste the new line.";

/// Runs `kioku join <url> <code>`: exchanges the code for the token, then does what
/// `kioku setup --client-only <url> <token>` does. Nothing printed contains the token.
pub fn run_join(url: &str, code: &str, opts: &JoinOptions, env: &SetupEnv) -> CommandReport {
    let url = normalize_url(url);
    let parsed = match reqwest::Url::parse(&url) {
        Ok(u) if matches!(u.scheme(), "http" | "https") => u,
        _ => {
            return CommandReport::fail(format!(
                "kioku: サーバーの URL が正しくありません: {url}\nkioku: invalid server URL: {url} (paste the line kioku invite printed as it is)"
            ));
        }
    };
    let port = parsed.port_or_known_default().unwrap_or(7391);
    let client = ClientConfig {
        server_url: url.clone(),
        auth_token: None,
        ..ClientConfig::default()
    };
    let answer = ApiClient::new(&client, env.request_timeout.max(Duration::from_secs(5)))
        .and_then(|c| c.post(&["join"], &json!({ "code": code.trim() })));
    let token = match answer {
        Ok(v) => match v.get("token").and_then(Value::as_str) {
            Some(t) if !t.trim().is_empty() => t.trim().to_string(),
            _ => return CommandReport::fail("kioku: error: the server's join answer has no token"),
        },
        Err(e) => {
            return CommandReport::fail(match http_status(&e) {
                Some(404) => INVALID.to_string(),
                Some(429) => "kioku: 失敗した試行が多すぎます。1 分待ってから、サーバーで kioku invite をもう一度実行してもらってください。\n\
                     kioku: Too many failed attempts: wait a minute, then ask for a new kioku invite on the server."
                    .to_string(),
                Some(status) => format!(
                    "kioku: サーバーが招待を受け付けませんでした (HTTP {status})。\nkioku: The server at {url} refused the invite: {e:#}"
                ),
                None => format!(
                    "kioku: {url} に接続できません。サーバー機が起動していて同じネットワーク（または VPN）にあり、ファイアウォールがポート {port} を許可しているか確認してください。\n\
                     kioku: Cannot reach {url}: check that the server machine is on, on this network (or VPN), and that its firewall allows port {port} (macOS: System Settings > Network > Firewall)."
                ),
            });
        }
    };

    let setup_opts = SetupOptions {
        client_only: Some((url.clone(), token.clone())),
        agents: opts.agents.clone(),
        no_agents: opts.no_agents,
        no_instructions: opts.no_instructions,
        mcp_http: opts.mcp_http,
        ..SetupOptions::default()
    };
    let report = run_setup(&setup_opts, env);
    let mut stdout = format!("kioku join (v{})\n", crate::setup::VERSION);
    for line in report.summary_lines() {
        stdout.push_str(&line);
        stdout.push('\n');
    }
    let code = report.exit_code();
    let agents: Vec<&str> = ALL_AGENTS
        .iter()
        .filter(|a| {
            report
                .lines
                .iter()
                .any(|l| l.step == a.as_str() && l.mark == Mark::Ok)
        })
        .map(|a| a.display_name())
        .collect();
    stdout.push('\n');
    if code != 0 {
        stdout.push_str(
            "kioku の設定が完了しませんでした。上の xx の行を確認してください。\n\
             kioku is not ready: see the xx line above.\n",
        );
    } else if opts.no_agents {
        stdout.push_str("kioku の準備ができました。/ kioku is ready.\n");
    } else if agents.is_empty() {
        stdout.push_str(
            "kioku の準備ができましたが、エージェントが見つかりませんでした。Claude Code などを入れたら kioku setup を実行してください。\n\
             kioku is ready, but no agent was found: install Claude Code, Codex, … and run kioku setup.\n",
        );
    } else {
        let list = agents.join(" / ");
        stdout.push_str(&format!(
            "kioku の準備ができました。{list} を再起動してください。\nkioku is ready - restart {}.\n",
            agents.join(", ")
        ));
    }
    // Belt and braces: nothing we print may contain the token.
    let stdout = stdout.replace(&token, "<token>");
    CommandReport {
        stdout,
        stderr: String::new(),
        exit_code: code,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn own_url_and_normalization() {
        let mut cfg = Config::for_data_dir(std::path::Path::new("/tmp/k"));
        cfg.server.port = 7391;
        for (bind, want) in [
            ("0.0.0.0", "http://127.0.0.1:7391"),
            ("127.0.0.1", "http://127.0.0.1:7391"),
            ("::", "http://[::1]:7391"),
            ("192.168.1.5", "http://192.168.1.5:7391"),
            ("fe80::1", "http://[fe80::1]:7391"),
        ] {
            cfg.server.bind = bind.into();
            assert_eq!(own_server_url(&cfg), want, "{bind}");
        }
        assert_eq!(
            normalize_url("192.168.1.240:7391/"),
            "http://192.168.1.240:7391"
        );
        assert_eq!(normalize_url("https://k.example"), "https://k.example");
    }

    #[test]
    fn invite_text() {
        // (SPEC-M2.3 §9 shapes)
        let url = crate::setup::ClientUrl {
            url: "http://192.168.1.240:7391".into(),
            mdns_alternative: Some("http://mini-M2.local:7391".into()),
            loopback_note: None,
        };
        let t = render_invite("K7Q2M9XD", &url, 10, 1);
        assert!(t.contains(
            "  Windows (PowerShell):  $env:KIOKU_JOIN='192.168.1.240:7391/K7Q2M9XD'; irm https://raw.githubusercontent.com/misorafa/kioku/main/install.ps1 | iex\n"
        ), "{t}");
        assert!(
            t.contains(
                "  macOS / Linux / Git Bash:  KIOKU_JOIN='192.168.1.240:7391/K7Q2M9XD' sh -c \"$(curl -fsSL https://raw.githubusercontent.com/misorafa/kioku/main/install.sh)\"\n"
            ),
            "{t}"
        );
        // SPEC-M2.7 §4: no script over plain http.
        assert!(!t.contains("http://192.168.1.240:7391/i/"), "{t}");
        assert!(!t.contains("/i/"), "{t}");
        assert!(
            !t.contains("Bypass") && !t.contains("/i/K7Q2M9XD.ps1"),
            "{t}"
        );
        assert!(t.contains("(valid 10 minutes, once)"));
        assert!(t.contains("10 分間・1 回だけ有効"));
        assert!(t.contains("http://mini-M2.local:7391/…"));
        let t = render_invite("K7Q2M9XD", &url, 30, 3);
        assert!(t.contains("valid 30 minutes, up to 3 machines"));
    }

    /// SPEC-M2.7 §4: `--host` values and the list of this machine's addresses.
    #[test]
    fn host_flag_and_address_list() {
        assert_eq!(
            host_url("192.168.1.5", 7391).as_deref(),
            Some("http://192.168.1.5:7391")
        );
        assert_eq!(
            host_url("mini.local:8000", 7391).as_deref(),
            Some("http://mini.local:8000")
        );
        assert_eq!(
            host_url("http://10.0.0.2:7391/", 7391).as_deref(),
            Some("http://10.0.0.2:7391")
        );
        assert_eq!(
            host_url("fe80::1", 7391).as_deref(),
            Some("http://[fe80::1]:7391")
        );
        assert_eq!(
            host_url("[fd00::5]:9", 7391).as_deref(),
            Some("http://[fd00::5]:9")
        );
        for bad in ["", "a b", "h/x", "u@h", "h'; rm", "$(x)"] {
            assert_eq!(host_url(bad, 7391), None, "{bad}");
        }
        let ip = "1: lo    inet 127.0.0.1/8 scope host lo\n2: eth0    inet 100.101.5.6/32 scope global tailscale0\n3: wlan0    inet 192.168.1.240/24 brd 192.168.1.255 scope global wlan0\n";
        let mac = "lo0: flags=8049<UP,LOOPBACK>\n\tinet 127.0.0.1 netmask 0xff000000\nen0: flags=8863<UP>\n\tinet6 fe80::1%en0 prefixlen 64\n\tinet 192.168.1.57 netmask 0xffffff00 broadcast 192.168.1.255\nbridge0:\n\tinet 169.254.3.4 netmask 0xffff0000\nutun3:\n\tinet 10.8.0.2 --> 10.8.0.1 netmask 0xffffffff\n";
        let win = "Wireless LAN adapter Wi-Fi:\r\n   IPv4 アドレス . . . . . . . . . . . .: 192.168.0.12(優先)\r\n   サブネット マスク . . . . . . . . . .: 255.255.255.0\r\nEthernet adapter vEthernet:\r\n   IPv4 Address. . . . . . . . . . . : 172.20.1.1\r\n";
        let s = |v: Vec<std::net::Ipv4Addr>| v.iter().map(|i| i.to_string()).collect::<Vec<_>>();
        assert_eq!(
            s(parse_ipv4s(ip)),
            ["192.168.1.240", "100.101.5.6"],
            "LAN first"
        );
        assert_eq!(s(parse_ipv4s(mac)), ["192.168.1.57", "10.8.0.2"]);
        assert_eq!(s(parse_ipv4s(win)), ["192.168.0.12", "172.20.1.1"]);
        let alts = alternative_urls("http://192.168.1.240:7391", &parse_ipv4s(ip), 7391);
        assert_eq!(alts, ["http://100.101.5.6:7391"]);
    }
}
