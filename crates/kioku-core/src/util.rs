//! Small shared helpers: timestamps, ids, randomness, hashing, truncation.

use std::collections::hash_map::RandomState;
use std::hash::{BuildHasher, Hasher};
use std::io::Read;
use std::sync::atomic::{AtomicU64, Ordering};

use chrono::{DateTime, SecondsFormat, Utc};
use sha2::{Digest, Sha256};

/// Current time in UTC.
pub fn now() -> DateTime<Utc> {
    Utc::now()
}

/// Formats a timestamp as RFC 3339 UTC with millisecond precision.
pub fn fmt_ts(ts: DateTime<Utc>) -> String {
    ts.to_rfc3339_opts(SecondsFormat::Millis, true)
}

/// Formats a timestamp as RFC 3339 UTC with second precision (used in frontmatter).
pub fn fmt_ts_secs(ts: DateTime<Utc>) -> String {
    ts.to_rfc3339_opts(SecondsFormat::Secs, true)
}

/// Current time formatted with [`fmt_ts`].
pub fn now_ts() -> String {
    fmt_ts(now())
}

/// Parses an RFC 3339 timestamp into UTC; `None` when malformed.
pub fn parse_ts(s: &str) -> Option<DateTime<Utc>> {
    DateTime::parse_from_rfc3339(s)
        .ok()
        .map(|d| d.with_timezone(&Utc))
}

/// `YYYY-MM-DD HH:MM` (UTC) rendering of an RFC 3339 timestamp, or the input when unparsable.
pub fn display_minute(ts: &str) -> String {
    parse_ts(ts)
        .map(|d| d.format("%Y-%m-%d %H:%M").to_string())
        .unwrap_or_else(|| ts.to_string())
}

/// `YYYY-MM-DD` (UTC) rendering of an RFC 3339 timestamp, or the first 10 chars of the input.
pub fn display_date(ts: &str) -> String {
    parse_ts(ts)
        .map(|d| d.format("%Y-%m-%d").to_string())
        .unwrap_or_else(|| ts.chars().take(10).collect())
}

/// A random `u64`, from `/dev/urandom` when available, else from std's seeded hasher.
pub fn random_u64() -> u64 {
    let mut buf = [0u8; 8];
    if let Ok(mut f) = std::fs::File::open("/dev/urandom")
        && f.read_exact(&mut buf).is_ok()
    {
        return u64::from_le_bytes(buf);
    }
    static COUNTER: AtomicU64 = AtomicU64::new(0);
    let mut h = RandomState::new().build_hasher();
    h.write_u64(COUNTER.fetch_add(1, Ordering::Relaxed));
    h.write_i64(Utc::now().timestamp_nanos_opt().unwrap_or_default());
    h.write_u32(std::process::id());
    h.finish()
}

/// ULID-like id: 12 hex digits of epoch milliseconds followed by 6 random hex digits.
pub fn new_id() -> String {
    let ms = Utc::now().timestamp_millis().max(0) as u64;
    format!("{ms:012x}{:06x}", random_u64() & 0xff_ffff)
}

/// A 64-hex-char random secret suitable for `auth_token`.
pub fn generate_token() -> String {
    (0..4).map(|_| format!("{:016x}", random_u64())).collect()
}

/// Lowercase hex SHA-256 of a string.
pub fn sha256_hex(s: &str) -> String {
    let digest = Sha256::digest(s.as_bytes());
    digest.iter().map(|b| format!("{b:02x}")).collect()
}

/// Truncates to at most `max` chars; when cut, the last char is replaced by `…`.
pub fn truncate_chars(s: &str, max: usize) -> String {
    if s.chars().count() <= max {
        return s.to_string();
    }
    if max == 0 {
        return String::new();
    }
    let mut out: String = s.chars().take(max - 1).collect();
    out.push('…');
    out
}

/// Collapses all whitespace runs (including newlines) into single spaces.
pub fn one_line(s: &str) -> String {
    s.split_whitespace().collect::<Vec<_>>().join(" ")
}

/// Home directory from `$HOME` (or `%USERPROFILE%`), falling back to `.`.
pub fn home_dir() -> std::path::PathBuf {
    std::env::var_os("HOME")
        .or_else(|| std::env::var_os("USERPROFILE"))
        .map(std::path::PathBuf::from)
        .unwrap_or_else(|| std::path::PathBuf::from("."))
}

/// Expands a leading `~` or `~/` against [`home_dir`].
pub fn expand_tilde(p: &str) -> std::path::PathBuf {
    if p == "~" {
        home_dir()
    } else if let Some(rest) = p.strip_prefix("~/") {
        home_dir().join(rest)
    } else {
        std::path::PathBuf::from(p)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn truncate_is_char_safe_and_bounded() {
        assert_eq!(truncate_chars("引き継ぎ書", 10), "引き継ぎ書");
        assert_eq!(truncate_chars("引き継ぎ書", 3), "引き…");
        assert_eq!(truncate_chars("abc", 0), "");
        let t = truncate_chars(&"x".repeat(50), 10);
        assert_eq!(t.chars().count(), 10);
        assert_eq!(truncate_chars(&t, 10), t);
    }

    #[test]
    fn ids_are_unique_and_sortable() {
        let a = new_id();
        let b = new_id();
        assert_eq!(a.len(), 18);
        assert_ne!(a, b);
        assert_eq!(generate_token().len(), 64);
    }
}
