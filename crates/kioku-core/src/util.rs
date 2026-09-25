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
    home_dir_opt().unwrap_or_else(|| std::path::PathBuf::from("."))
}

/// Home directory from `$HOME` (or `%USERPROFILE%`); `None` when neither is set (or empty).
pub fn home_dir_opt() -> Option<std::path::PathBuf> {
    std::env::var_os("HOME")
        .filter(|h| !h.is_empty())
        .or_else(|| std::env::var_os("USERPROFILE").filter(|h| !h.is_empty()))
        .map(std::path::PathBuf::from)
}

/// Creates `dir` (with parents) and restricts it to its owner (0700 on unix). Tightening
/// the mode of an existing directory we do not own fails silently (logged).
pub fn create_private_dir(dir: &std::path::Path) -> std::io::Result<()> {
    std::fs::create_dir_all(dir)?;
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        let private = std::fs::Permissions::from_mode(0o700);
        if let Err(e) = std::fs::set_permissions(dir, private) {
            tracing::warn!(dir = %dir.display(), error = %e, "could not restrict directory to 0700");
        }
    }
    Ok(())
}

/// Writes `text` to `path`, readable and writable only by the owner (0600 on unix, also
/// when the file already existed with a wider mode).
pub fn write_private_file(path: &std::path::Path, text: &str) -> std::io::Result<()> {
    use std::io::Write;
    let mut opts = std::fs::OpenOptions::new();
    opts.write(true).create(true).truncate(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::{OpenOptionsExt, PermissionsExt};
        opts.mode(0o600);
        // An existing file keeps its mode on open: tighten it before writing the secret.
        if path.exists() {
            std::fs::set_permissions(path, std::fs::Permissions::from_mode(0o600))?;
        }
    }
    let mut f = opts.open(path)?;
    f.write_all(text.as_bytes())?;
    Ok(())
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

    #[cfg(unix)]
    #[test]
    fn private_files_and_dirs() {
        use std::os::unix::fs::PermissionsExt;
        let tmp = tempfile::tempdir().unwrap();
        let dir = tmp.path().join("a/b");
        create_private_dir(&dir).unwrap();
        let mode = |p: &std::path::Path| std::fs::metadata(p).unwrap().permissions().mode() & 0o777;
        assert_eq!(mode(&dir), 0o700);
        let f = dir.join("config.toml");
        std::fs::write(&f, "old").unwrap();
        std::fs::set_permissions(&f, std::fs::Permissions::from_mode(0o644)).unwrap();
        write_private_file(&f, "token").unwrap();
        assert_eq!(mode(&f), 0o600);
        assert_eq!(std::fs::read_to_string(&f).unwrap(), "token");
        let g = dir.join("new.toml");
        write_private_file(&g, "x").unwrap();
        assert_eq!(mode(&g), 0o600);
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
