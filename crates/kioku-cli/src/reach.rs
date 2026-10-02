//! Client reachability diagnosis (SPEC-M3.2 §3.1): when this machine's `kioku` cannot
//! connect to a server on the private network (`EHOSTUNREACH` / `ENETUNREACH`) while a system
//! binary can, the cause is a per-process network permission — macOS "Local Network"
//! privacy bound to the *responsible app* that launched kioku, or a Little Snitch rule — not
//! the server. This module classifies the error, probes with `/usr/bin/nc`, walks the parent
//! processes to the first `.app` bundle, and notices a *stale* app: one whose processes
//! started before the bundle was updated (the 2026-10-02 field case).

use std::net::IpAddr;
use std::path::{Path, PathBuf};

use crate::service::Runner;

/// The system `nc` (Apple-signed, exempt from Local Network privacy).
pub const SYSTEM_NC: &str = "/usr/bin/nc";
/// Where the toggle lives (ja / en), printed by `doctor` and `doctor --fix`.
pub const LOCAL_NETWORK_SETTINGS: &str = "システム設定 › プライバシーとセキュリティ › ローカルネットワーク / System Settings › Privacy & Security › Local Network";
/// Parent processes `ps` is asked about at most.
const MAX_WALK: usize = 32;
/// Slack between a process's start and the bundle's `Info.plist` change (`ps` etime is in
/// whole seconds).
const STALE_SLACK_SECS: i64 = 2;

/// What kind of connect error a request ended with.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ConnectError {
    /// `EHOSTUNREACH` ("No route to host").
    HostUnreachable,
    /// `ENETUNREACH` ("Network is unreachable").
    NetUnreachable,
    /// Anything else (refused, timed out, DNS, TLS, …).
    Other,
}

/// Reads the error kind from an error chain's text (reqwest hides the `io::Error`): the
/// errno of macOS (65 / 51), Linux (113 / 101) and Windows (10065 / 10051), or the
/// strerror text.
pub fn connect_error(text: &str) -> ConnectError {
    let t = text.to_ascii_lowercase();
    let errno = |n: u32| t.contains(&format!("os error {n})"));
    if errno(65) || errno(113) || errno(10065) || t.contains("no route to host") {
        ConnectError::HostUnreachable
    } else if errno(51) || errno(101) || errno(10051) || t.contains("network is unreachable") {
        ConnectError::NetUnreachable
    } else {
        ConnectError::Other
    }
}

/// Whether an address is on the local network (what Local Network privacy guards).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum AddressClass {
    /// RFC 1918, link-local, IPv6 unique-local / link-local, or a `.local` name.
    Private,
    /// Anything else.
    Other,
}

/// True for RFC 1918, IPv4 / IPv6 link-local and IPv6 unique-local addresses.
pub fn is_private_ip(ip: IpAddr) -> bool {
    match ip {
        IpAddr::V4(v4) => v4.is_private() || v4.is_link_local(),
        IpAddr::V6(v6) => {
            if let Some(v4) = v6.to_ipv4_mapped() {
                return v4.is_private() || v4.is_link_local();
            }
            let first = v6.segments()[0];
            (first & 0xffc0) == 0xfe80 || (first & 0xfe00) == 0xfc00
        }
    }
}

/// Classifies `host` (`.local` names by their suffix, IP literals by their range) with the
/// addresses it resolved to.
pub fn address_class(host: &str, resolved: &[IpAddr]) -> AddressClass {
    let host = host.trim_start_matches('[').trim_end_matches(']');
    let private = host
        .to_ascii_lowercase()
        .trim_end_matches('.')
        .ends_with(".local")
        || host.parse::<IpAddr>().is_ok_and(is_private_ip)
        || resolved.iter().any(|ip| is_private_ip(*ip));
    if private {
        AddressClass::Private
    } else {
        AddressClass::Other
    }
}

/// What the `/usr/bin/nc -z` probe of the same address said.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Probe {
    /// The system binary connected.
    Reached,
    /// It could not connect either.
    Failed,
    /// No system `nc` here (or not a Unix machine).
    Unavailable,
}

/// The app that launched this process (the first `.app` bundle among its ancestors).
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ResponsibleApp {
    /// The outermost bundle (`/Applications/Orca.app`).
    pub bundle: PathBuf,
    /// Display name (`Orca`).
    pub name: String,
    /// The ancestor process inside that bundle (`… Orca Helper`).
    pub process: String,
    /// True when that process started before the bundle's `Contents/Info.plist` last
    /// changed: the app was updated while its old processes kept running.
    pub stale: bool,
}

/// The diagnosis `doctor`'s `server` check prints instead of "check the server".
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Diagnosis {
    /// One line (ja / en).
    pub message: String,
    /// What to do.
    pub fix: String,
    /// True when the cause is this machine's per-process permission, not the server.
    pub local_permission: bool,
}

/// The classifier (§3.1): `None` unless the error is "unreachable" towards a private
/// address. With `nc` reaching the address, the cause is a per-process permission and the
/// message never suggests checking the server.
pub fn classify(
    error: ConnectError,
    class: AddressClass,
    nc: Probe,
    target: &str,
    app: Option<&ResponsibleApp>,
) -> Option<Diagnosis> {
    if error == ConnectError::Other || class != AddressClass::Private {
        return None;
    }
    match nc {
        Probe::Reached => {
            let mut message = format!(
                "kioku cannot reach {target}, but {SYSTEM_NC} from this machine can: this machine's kioku alone is kept off the LAN (per-process network permission). \
                 この端末の kioku だけが LAN に出られません。macOS の「プライバシーとセキュリティ › ローカルネットワーク」で、kioku を起動したアプリ（例: Orca / Ghostty / Claude / Codex）を許可してください。既に許可済みに見える場合は、そのアプリを一度オフにしてからオンにし、アプリを再起動してください（アプリの更新後に起きます）。Little Snitch を使っている場合は kioku 実行ファイル自体に許可ルールを作ってください"
            );
            if let Some(app) = app {
                message.push_str(&format!(
                    " / responsible app 起動元アプリ: {} ({})",
                    app.name,
                    app.bundle.display()
                ));
                if app.stale {
                    message.push_str(&format!(
                        " / {name} は更新されましたが、更新前に起動したプロセス（{process}）が動き続けています。{name} を完全に終了してから起動し直してください。 {name} was updated while its old processes kept running: quit it completely and start it again",
                        name = app.name,
                        process = app.process
                    ));
                }
            }
            let who = app
                .map(|a| a.name.clone())
                .unwrap_or_else(|| "the app that runs kioku (terminal / IDE / agent app)".into());
            Some(Diagnosis {
                message,
                fix: format!(
                    "{LOCAL_NETWORK_SETTINGS}: allow {who} (if it already looks allowed: turn it off and on again, then quit and relaunch {who}); Little Snitch: add an allow rule for the kioku binary. Hooks fail open meanwhile; queued observations are sent once it can connect"
                ),
                local_permission: true,
            })
        }
        Probe::Failed => Some(Diagnosis {
            message: format!(
                "{target} is unreachable from this machine ({SYSTEM_NC} cannot connect either)"
            ),
            fix: format!(
                "check that the server at {target} is running and that this machine is on its network (Wi-Fi / VPN / WireGuard)"
            ),
            local_permission: false,
        }),
        Probe::Unavailable => Some(Diagnosis {
            message: format!(
                "no route to {target} (a private address); if curl or nc reach it from this shell, a per-process network permission blocks kioku"
            ),
            fix: format!(
                "check that the server at {target} is running; if other programs reach it, allow the app that runs kioku in {LOCAL_NETWORK_SETTINGS} (Little Snitch: allow the kioku binary)"
            ),
            local_permission: false,
        }),
    }
}

/// Runs `/usr/bin/nc -z` against `host:port` with a short timeout.
pub fn probe_nc(runner: &Runner, host: &str, port: u16) -> Probe {
    if !cfg!(unix) || (!runner.is_recording() && !Path::new(SYSTEM_NC).is_file()) {
        return Probe::Unavailable;
    }
    let port = port.to_string();
    let host = host.trim_start_matches('[').trim_end_matches(']');
    let out = if cfg!(target_os = "macos") {
        runner.run(&[SYSTEM_NC, "-z", "-G", "3", "-w", "3", host, &port])
    } else {
        runner.run(&[SYSTEM_NC, "-z", "-w", "3", host, &port])
    };
    if out.success {
        Probe::Reached
    } else {
        Probe::Failed
    }
}

/// One process as `ps -o ppid=,etime=,comm=` reports it.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ProcInfo {
    /// Process id.
    pub pid: u32,
    /// Parent process id.
    pub ppid: u32,
    /// Seconds since it started.
    pub elapsed_secs: Option<u64>,
    /// Executable path (macOS prints the full path).
    pub path: String,
}

/// Parses `ps` `etime` (`[[dd-]hh:]mm:ss`) into seconds.
pub fn parse_etime(s: &str) -> Option<u64> {
    let (days, rest) = match s.trim().split_once('-') {
        Some((d, r)) => (d.parse::<u64>().ok()?, r),
        None => (0, s.trim()),
    };
    let parts: Vec<u64> = rest
        .split(':')
        .map(|p| p.parse::<u64>().ok())
        .collect::<Option<_>>()?;
    let secs = match parts.as_slice() {
        [m, s] => m * 60 + s,
        [h, m, s] => h * 3600 + m * 60 + s,
        _ => return None,
    };
    Some(days * 86400 + secs)
}

/// Parses one `ps -o ppid=,etime=,comm= -p <pid>` line (the path may contain spaces).
pub fn parse_ps_line(pid: u32, line: &str) -> Option<ProcInfo> {
    let line = line.trim();
    let (ppid, rest) = line.split_once(char::is_whitespace)?;
    let rest = rest.trim_start();
    let (etime, path) = rest.split_once(char::is_whitespace)?;
    Some(ProcInfo {
        pid,
        ppid: ppid.trim().parse().ok()?,
        elapsed_secs: parse_etime(etime),
        path: path.trim().to_string(),
    })
}

/// The ancestors of `start` (itself first), asking `ps` one process at a time.
pub fn process_chain(runner: &Runner, start: u32) -> Vec<ProcInfo> {
    let mut out = Vec::new();
    let mut pid = start;
    while pid > 1 && out.len() < MAX_WALK {
        let pid_s = pid.to_string();
        let res = runner.run(&["ps", "-o", "ppid=,etime=,comm=", "-p", &pid_s]);
        if !res.success {
            break;
        }
        let Some(info) = res
            .stdout
            .lines()
            .find(|l| !l.trim().is_empty())
            .and_then(|l| parse_ps_line(pid, l))
        else {
            break;
        };
        pid = info.ppid;
        out.push(info);
    }
    out
}

/// The outermost `.app` bundle in an executable path (`/A/Orca.app/…/Helper.app/…` →
/// `/A/Orca.app`).
pub fn app_bundle(path: &str) -> Option<PathBuf> {
    let mut acc = PathBuf::new();
    for comp in Path::new(path).components() {
        acc.push(comp);
        if comp
            .as_os_str()
            .to_string_lossy()
            .to_ascii_lowercase()
            .ends_with(".app")
        {
            return Some(acc);
        }
    }
    None
}

/// The responsible app of a process chain: the first ancestor inside a `.app` bundle. It is
/// stale when that process started (`now - elapsed`) before the bundle's
/// `Contents/Info.plist` was last modified.
pub fn responsible_app(chain: &[ProcInfo], now: std::time::SystemTime) -> Option<ResponsibleApp> {
    let (proc_, bundle) = chain
        .iter()
        .find_map(|p| app_bundle(&p.path).map(|b| (p, b)))?;
    let name = bundle
        .file_stem()
        .map(|s| s.to_string_lossy().into_owned())
        .unwrap_or_default();
    let unix = |t: std::time::SystemTime| {
        t.duration_since(std::time::UNIX_EPOCH)
            .map(|d| d.as_secs() as i64)
            .unwrap_or(0)
    };
    let plist = std::fs::metadata(bundle.join("Contents").join("Info.plist"))
        .and_then(|m| m.modified())
        .ok()
        .map(unix);
    let started = proc_.elapsed_secs.map(|e| unix(now) - e as i64);
    let stale = match (started, plist) {
        (Some(started), Some(changed)) => started + STALE_SLACK_SECS < changed,
        _ => false,
    };
    Some(ResponsibleApp {
        bundle,
        name,
        process: Path::new(&proc_.path)
            .file_name()
            .map(|n| n.to_string_lossy().into_owned())
            .unwrap_or_else(|| proc_.path.clone()),
        stale,
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::service::CmdOutput;
    use std::time::{Duration, SystemTime};

    #[test]
    fn error_kinds_from_text() {
        let mac = "error sending request: client error (Connect): tcp connect error: No route to host (os error 65)";
        assert_eq!(connect_error(mac), ConnectError::HostUnreachable);
        assert_eq!(
            connect_error("tcp connect error (os error 113)"),
            ConnectError::HostUnreachable
        );
        assert_eq!(
            connect_error("tcp connect error: Network is unreachable (os error 51)"),
            ConnectError::NetUnreachable
        );
        assert_eq!(
            connect_error("tcp connect error: Connection refused (os error 61)"),
            ConnectError::Other
        );
        // 650 is not 65.
        assert_eq!(connect_error("(os error 650)"), ConnectError::Other);
    }

    #[test]
    fn address_classes() {
        let none: [IpAddr; 0] = [];
        for host in [
            "192.168.1.20",
            "10.0.0.5",
            "172.20.1.1",
            "169.254.3.4",
            "fe80::1",
            "[fd12:3456::1]",
            "mac-mini.local",
            "Mac-Mini.LOCAL.",
        ] {
            assert_eq!(address_class(host, &none), AddressClass::Private, "{host}");
        }
        for host in ["8.8.8.8", "172.32.0.1", "example.com", "2001:db8::1"] {
            assert_eq!(address_class(host, &none), AddressClass::Other, "{host}");
        }
        // A name is private when it resolves into the LAN.
        let lan: [IpAddr; 1] = ["192.168.0.9".parse().unwrap()];
        assert_eq!(address_class("home", &lan), AddressClass::Private);
    }

    fn orca(stale: bool) -> ResponsibleApp {
        ResponsibleApp {
            bundle: PathBuf::from("/Applications/Orca.app"),
            name: "Orca".into(),
            process: "Orca Helper".into(),
            stale,
        }
    }

    /// error kind + address class + nc result (+ responsible app) → message.
    #[test]
    fn classifier_matrix() {
        let t = "http://192.168.1.20:7391";
        let h = ConnectError::HostUnreachable;
        let p = AddressClass::Private;
        assert_eq!(
            classify(ConnectError::Other, p, Probe::Reached, t, None),
            None
        );
        assert_eq!(
            classify(h, AddressClass::Other, Probe::Reached, t, None),
            None
        );

        let d = classify(h, p, Probe::Reached, t, Some(&orca(false))).unwrap();
        assert!(d.local_permission);
        assert!(
            d.message
                .contains("この端末の kioku だけが LAN に出られません")
        );
        assert!(
            d.message.contains("Orca (/Applications/Orca.app)"),
            "{}",
            d.message
        );
        assert!(d.message.contains("Little Snitch"));
        assert!(!d.message.contains("更新前に起動したプロセス"));
        assert!(d.fix.contains("ローカルネットワーク") && d.fix.contains("Local Network"));
        // Never "check the server" when a system binary gets through.
        for text in [&d.message, &d.fix] {
            assert!(
                !text.to_lowercase().contains("check that the server"),
                "{text}"
            );
            assert!(!text.to_lowercase().contains("check the server"), "{text}");
        }

        let stale = classify(
            ConnectError::NetUnreachable,
            p,
            Probe::Reached,
            t,
            Some(&orca(true)),
        )
        .unwrap();
        assert!(
            stale
                .message
                .contains("Orca を完全に終了してから起動し直してください")
        );
        assert!(
            stale
                .message
                .contains("was updated while its old processes kept running")
        );
        assert!(stale.message.contains("Orca Helper"));

        let down = classify(h, p, Probe::Failed, t, None).unwrap();
        assert!(!down.local_permission);
        assert!(down.fix.contains("check that the server"));
        let unknown = classify(h, p, Probe::Unavailable, t, None).unwrap();
        assert!(!unknown.local_permission);
        assert!(unknown.fix.contains("Local Network"));
    }

    #[test]
    fn etime_and_ps_lines() {
        assert_eq!(parse_etime("00:07"), Some(7));
        assert_eq!(parse_etime("09:21"), Some(561));
        assert_eq!(parse_etime("01:00:00"), Some(3600));
        assert_eq!(
            parse_etime("2-03:04:05"),
            Some(2 * 86400 + 3 * 3600 + 4 * 60 + 5)
        );
        assert_eq!(parse_etime("x"), None);
        let p = parse_ps_line(
            42,
            "  777 1-00:00:00 /Applications/Orca.app/Contents/Frameworks/Orca Helper.app/Contents/MacOS/Orca Helper",
        )
        .unwrap();
        assert_eq!(p.ppid, 777);
        assert_eq!(p.elapsed_secs, Some(86400));
        assert!(p.path.ends_with("MacOS/Orca Helper"));
        assert_eq!(
            app_bundle(&p.path),
            Some(PathBuf::from("/Applications/Orca.app"))
        );
        assert_eq!(app_bundle("/bin/zsh"), None);
    }

    /// The ppid walk on a fixture: kioku ← zsh ← Orca Helper (started a day ago) ← launchd;
    /// the bundle's Info.plist changed an hour ago → stale; a fresh helper → not stale.
    #[test]
    fn ppid_walk_finds_the_stale_responsible_app() {
        let tmp = tempfile::tempdir().unwrap();
        let bundle = tmp.path().join("Orca.app");
        let plist = bundle.join("Contents").join("Info.plist");
        std::fs::create_dir_all(plist.parent().unwrap()).unwrap();
        std::fs::write(&plist, "<plist/>").unwrap();
        let now = SystemTime::now();
        let updated = now - Duration::from_secs(3600);
        std::fs::File::options()
            .write(true)
            .open(&plist)
            .unwrap()
            .set_modified(updated)
            .unwrap();
        let helper = bundle
            .join("Contents/Frameworks/Orca Helper.app/Contents/MacOS/Orca Helper")
            .display()
            .to_string();
        let walk = |helper_etime: &'static str| {
            let helper = helper.clone();
            Runner::recording(move |argv| match argv.last().map(String::as_str) {
                Some("100") => CmdOutput::ok("  200 00:01 /Users/me/.local/bin/kioku\n"),
                Some("200") => CmdOutput::ok("  300 05:00 /bin/zsh\n"),
                Some("300") => CmdOutput::ok(&format!("    1 {helper_etime} {helper}\n")),
                _ => CmdOutput::fail("no such process"),
            })
        };
        let runner = walk("1-00:00:00");
        let chain = process_chain(&runner, 100);
        assert_eq!(
            chain.iter().map(|p| p.pid).collect::<Vec<_>>(),
            [100, 200, 300]
        );
        assert!(runner.calls().iter().all(|c| c[0] == "ps"));
        let app = responsible_app(&chain, now).unwrap();
        assert_eq!(app.bundle, bundle);
        assert_eq!(app.name, "Orca");
        assert_eq!(app.process, "Orca Helper");
        assert!(app.stale, "started a day ago, updated an hour ago");

        let fresh = process_chain(&walk("10:00"), 100);
        let app = responsible_app(&fresh, now).unwrap();
        assert!(!app.stale, "started 10 minutes ago, after the update");

        // No bundle among the ancestors (ssh session, Linux): nothing to name.
        let plain = Runner::recording(|argv| match argv.last().map(String::as_str) {
            Some("100") => CmdOutput::ok("  1 00:01 /usr/bin/kioku\n"),
            _ => CmdOutput::fail("no such process"),
        });
        assert_eq!(responsible_app(&process_chain(&plain, 100), now), None);
    }
}
