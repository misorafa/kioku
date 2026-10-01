//! The client machine's name, sent with every session start (SPEC-M3.0 §6) so recent
//! sessions, session pages and handoff headings say where the work happened.

use std::time::Duration;

use kioku_core::session::normalize_machine;

use crate::event::HookEnv;

/// How long the `hostname` fallback may take.
const HOSTNAME_DEADLINE: Duration = Duration::from_millis(300);

/// The machine name: `KIOKU_MACHINE` verbatim when set, else the host name up to its first
/// dot (`mini.local` → `mini`) from `COMPUTERNAME` (Windows), `HOSTNAME`,
/// `/proc/sys/kernel/hostname` or `/etc/hostname`, and only then the `hostname` command
/// (macOS has no file for it). At most 64 chars; `None` when nothing is found.
pub fn machine_name(env: &HookEnv) -> Option<String> {
    if let Some(name) = env.var("KIOKU_MACHINE").and_then(normalize_machine) {
        return Some(name);
    }
    detected(env).and_then(|h| short_host(&h))
}

fn detected(env: &HookEnv) -> Option<String> {
    let from_env = ["COMPUTERNAME", "HOSTNAME"]
        .iter()
        .find_map(|k| env.var(k).map(str::to_string));
    if from_env.is_some() {
        return from_env;
    }
    for file in ["/proc/sys/kernel/hostname", "/etc/hostname"] {
        if let Ok(text) = std::fs::read_to_string(file)
            && !text.trim().is_empty()
        {
            return Some(text);
        }
    }
    if cfg!(windows) {
        return None;
    }
    let out = kioku_core::util::output_with_deadline(
        kioku_core::util::quiet_command("hostname"),
        HOSTNAME_DEADLINE,
    )?;
    out.status
        .success()
        .then(|| String::from_utf8_lossy(&out.stdout).into_owned())
}

/// A host name without its domain, normalized ([`normalize_machine`]); an IP address is kept
/// whole.
pub fn short_host(host: &str) -> Option<String> {
    let host = host.trim();
    let is_ip = host.parse::<std::net::IpAddr>().is_ok();
    let short = if is_ip {
        host
    } else {
        host.split('.').next().unwrap_or(host)
    };
    normalize_machine(short)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn env(vars: &[(&str, &str)]) -> HookEnv {
        HookEnv {
            vars: vars
                .iter()
                .map(|(k, v)| (k.to_string(), v.to_string()))
                .collect(),
            ..HookEnv::default()
        }
    }

    #[test]
    fn override_then_environment_then_detection() {
        assert_eq!(
            machine_name(&env(&[
                ("KIOKU_MACHINE", " 自宅の mini.local "),
                ("HOSTNAME", "x")
            ])),
            Some("自宅の mini.local".into()),
            "the override is taken verbatim (trimmed)"
        );
        assert_eq!(
            machine_name(&env(&[("COMPUTERNAME", "WIN-PC")])),
            Some("WIN-PC".into())
        );
        assert_eq!(
            machine_name(&env(&[("HOSTNAME", "mini.local\n")])),
            Some("mini".into())
        );
        let long = "m".repeat(100);
        assert_eq!(
            machine_name(&env(&[("KIOKU_MACHINE", &long)])).map(|m| m.chars().count()),
            Some(64)
        );
        // detection on this machine yields something short and clean (or nothing)
        if let Some(m) = machine_name(&env(&[])) {
            assert!(!m.is_empty() && m.chars().count() <= 64 && !m.contains('\n'));
        }
    }

    #[test]
    fn short_host_drops_the_domain_but_keeps_ips() {
        assert_eq!(short_host("build.example.com"), Some("build".into()));
        assert_eq!(short_host("192.168.1.20"), Some("192.168.1.20".into()));
        assert_eq!(short_host("  "), None);
    }
}
