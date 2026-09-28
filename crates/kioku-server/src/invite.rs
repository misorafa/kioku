//! One-command join (SPEC-M2.3 §3): in-memory invite codes, the rate limiter for the
//! unauthenticated invite routes, the installer scripts served at `GET /i/<code>[.ps1]`
//! (the repo's install.sh / install.ps1, embedded, with the join variables prepended) and
//! the handlers of `POST /api/v1/invites` (bearer) and `POST /api/v1/join` (public).

use std::collections::HashMap;
use std::net::{IpAddr, Ipv4Addr, SocketAddr};
use std::sync::Arc;

use axum::{
    Json, Router,
    body::Bytes,
    extract::{ConnectInfo, Path, Request, State},
    http::{HeaderMap, StatusCode, header},
    response::{IntoResponse, Response},
    routing::{get, post},
};
use chrono::{DateTime, Duration, Utc};
use parking_lot::Mutex;
use serde::Deserialize;
use serde_json::json;

/// install.sh from the repository root, served (with the join variables) at `GET /i/<code>`.
pub const INSTALL_SH: &str = include_str!("../../../install.sh");
/// install.ps1 from the repository root, served at `GET /i/<code>.ps1`.
pub const INSTALL_PS1: &str = include_str!("../../../install.ps1");

/// Crockford base32 without the ambiguous letters I, L, O and U (32 symbols).
pub const CODE_ALPHABET: &[u8; 32] = b"0123456789ABCDEFGHJKMNPQRSTVWXYZ";
/// Length of an invite code.
pub const CODE_LEN: usize = 8;
/// Default lifetime of an invite, in minutes.
pub const DEFAULT_TTL_MINUTES: u32 = 10;
/// Upper bound of `--ttl`, in minutes.
pub const MAX_TTL_MINUTES: u32 = 60;
/// Default number of machines an invite admits.
pub const DEFAULT_USES: u32 = 1;
/// Upper bound of `--uses`.
pub const MAX_USES: u32 = 20;
/// Failed code lookups one peer may make per minute before it gets 429s.
pub const MAX_FAILURES_PER_PEER: usize = 10;
/// Failed code lookups all peers together may make per minute before everyone gets 429s.
pub const MAX_FAILURES_TOTAL: usize = 30;

/// How long the failure window and a 429 block last.
const WINDOW_SECS: i64 = 60;

/// Returns the current time; replaceable in tests.
pub type Clock = Arc<dyn Fn() -> DateTime<Utc> + Send + Sync>;

/// One invite (SPEC-M2.3 §3.1).
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Invite {
    /// Upper-case code.
    pub code: String,
    /// Creation time.
    pub created_at: DateTime<Utc>,
    /// End of validity.
    pub expires_at: DateTime<Utc>,
    /// Remaining `POST /api/v1/join` calls.
    pub uses_left: u32,
}

/// Result of a code lookup.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Lookup {
    /// The invite exists, is unexpired and has uses left.
    Valid,
    /// Unknown, expired or used up (counts as a failure for the rate limit).
    Invalid,
    /// This peer (or everyone) made too many failed lookups: 429.
    Limited,
}

#[derive(Default)]
struct Inner {
    invites: HashMap<String, Invite>,
    failures: Vec<(DateTime<Utc>, IpAddr)>,
    blocked_peers: HashMap<IpAddr, DateTime<Utc>>,
    blocked_all: Option<DateTime<Utc>>,
}

/// The server's invites and the failure counters of the rate limit, behind one mutex. A
/// restart drops everything, which is fine for a 10-minute object.
pub struct Invites {
    state: Mutex<Inner>,
    clock: Clock,
}

impl Default for Invites {
    fn default() -> Self {
        Invites::new()
    }
}

impl std::fmt::Debug for Invites {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Invites").finish_non_exhaustive()
    }
}

/// A fresh random code of [`CODE_LEN`] symbols from [`CODE_ALPHABET`].
pub fn new_code() -> String {
    let mut bits = kioku_core::util::random_u64();
    (0..CODE_LEN)
        .map(|_| {
            let c = CODE_ALPHABET[(bits & 31) as usize] as char;
            bits >>= 5;
            c
        })
        .collect()
}

/// The canonical (upper-case) form of a code as typed or pasted, or `None` when it cannot
/// be a code at all.
pub fn normalize_code(code: &str) -> Option<String> {
    let code = code.trim().to_ascii_uppercase();
    (code.len() == CODE_LEN && code.bytes().all(|b| CODE_ALPHABET.contains(&b))).then_some(code)
}

impl Invites {
    /// Invites on the system clock.
    pub fn new() -> Invites {
        Invites::with_clock(Arc::new(Utc::now))
    }

    /// Invites on an injected clock (tests).
    pub fn with_clock(clock: Clock) -> Invites {
        Invites {
            state: Mutex::new(Inner::default()),
            clock,
        }
    }

    fn now(&self) -> DateTime<Utc> {
        (self.clock)()
    }

    /// Creates an invite; `ttl_minutes` is clamped to 1..=60 (default 10), `uses` to 1..=20
    /// (default 1).
    pub fn create(&self, ttl_minutes: Option<u32>, uses: Option<u32>) -> Invite {
        let now = self.now();
        let ttl = ttl_minutes
            .unwrap_or(DEFAULT_TTL_MINUTES)
            .clamp(1, MAX_TTL_MINUTES);
        let uses = uses.unwrap_or(DEFAULT_USES).clamp(1, MAX_USES);
        let mut st = self.state.lock();
        purge(&mut st, now);
        let code = loop {
            let c = new_code();
            if !st.invites.contains_key(&c) {
                break c;
            }
        };
        let invite = Invite {
            code: code.clone(),
            created_at: now,
            expires_at: now + Duration::minutes(i64::from(ttl)),
            uses_left: uses,
        };
        st.invites.insert(code, invite.clone());
        invite
    }

    /// Looks `code` up for `peer` without consuming it (fetching the installer script).
    pub fn check(&self, code: &str, peer: IpAddr) -> Lookup {
        self.lookup(code, peer, false).0
    }

    /// Consumes one use of `code` for `peer`; returns the lookup result.
    pub fn consume(&self, code: &str, peer: IpAddr) -> Lookup {
        self.lookup(code, peer, true).0
    }

    /// The code in canonical form when it is valid (used to render the script).
    fn lookup(&self, code: &str, peer: IpAddr, consume: bool) -> (Lookup, Option<String>) {
        let now = self.now();
        let mut st = self.state.lock();
        purge(&mut st, now);
        if st.blocked_all.is_some() || st.blocked_peers.contains_key(&peer) {
            return (Lookup::Limited, None);
        }
        let canonical = normalize_code(code);
        let found = canonical.as_ref().and_then(|c| st.invites.get_mut(c));
        match found {
            Some(inv) if inv.uses_left > 0 => {
                if consume {
                    inv.uses_left -= 1;
                    if inv.uses_left == 0 {
                        let c = inv.code.clone();
                        st.invites.remove(&c);
                    }
                }
                (Lookup::Valid, canonical)
            }
            _ => {
                st.failures.push((now, peer));
                let mine = st.failures.iter().filter(|(_, p)| *p == peer).count();
                if mine >= MAX_FAILURES_PER_PEER {
                    st.blocked_peers
                        .insert(peer, now + Duration::seconds(WINDOW_SECS));
                }
                if st.failures.len() >= MAX_FAILURES_TOTAL {
                    st.blocked_all = Some(now + Duration::seconds(WINDOW_SECS));
                }
                (Lookup::Invalid, None)
            }
        }
    }

    /// Number of live invites (tests and diagnostics).
    pub fn len(&self) -> usize {
        let now = self.now();
        let mut st = self.state.lock();
        purge(&mut st, now);
        st.invites.len()
    }

    /// True when there is no live invite.
    pub fn is_empty(&self) -> bool {
        self.len() == 0
    }
}

/// Drops expired invites, failures older than the window and ended blocks.
fn purge(st: &mut Inner, now: DateTime<Utc>) {
    st.invites.retain(|_, i| i.expires_at > now);
    let cutoff = now - Duration::seconds(WINDOW_SECS);
    st.failures.retain(|(t, _)| *t > cutoff);
    st.blocked_peers.retain(|_, until| *until > now);
    if st.blocked_all.is_some_and(|until| until <= now) {
        st.blocked_all = None;
    }
}

/// `http://<host>` from the request's `Host` header (or the URI authority); `None` when it
/// is missing or not a plain `host[:port]`. The characters allowed here are also safe
/// inside the single-quoted strings of both scripts.
pub fn join_url(headers: &HeaderMap, uri: &axum::http::Uri) -> Option<String> {
    let host = headers
        .get(header::HOST)
        .and_then(|v| v.to_str().ok())
        .map(str::to_string)
        .or_else(|| uri.authority().map(|a| a.to_string()))?;
    let host = host.trim();
    if host.is_empty() || host.len() > 255 {
        return None;
    }
    let ok = host
        .bytes()
        .all(|b| b.is_ascii_alphanumeric() || matches!(b, b'.' | b'-' | b':' | b'[' | b']'));
    if !ok || host.starts_with([':', '.', '-']) {
        return None;
    }
    // Brackets only around an IPv6 literal at the start: `[::1]:7391`.
    if host.contains(['[', ']']) {
        let rest = host.strip_prefix('[')?;
        let (ip, after) = rest.split_once(']')?;
        ip.parse::<std::net::Ipv6Addr>().ok()?;
        if !(after.is_empty() || after.strip_prefix(':').is_some_and(valid_port)) {
            return None;
        }
    } else {
        match host.split_once(':') {
            None => {}
            Some((name, port)) if !name.is_empty() && valid_port(port) => {}
            Some(_) => return None,
        }
    }
    Some(format!("http://{host}"))
}

fn valid_port(p: &str) -> bool {
    !p.is_empty() && p.len() <= 5 && p.parse::<u16>().is_ok()
}

/// install.sh with `KIOKU_JOIN_URL` / `KIOKU_JOIN_CODE` set right after the shebang.
pub fn sh_script(url: &str, code: &str) -> String {
    // LF only: a checkout with CRLF endings (Windows, autocrlf) must still serve a valid sh script.
    let body = INSTALL_SH
        .trim_start_matches('\u{feff}')
        .replace("\r\n", "\n");
    let body = body.as_str();
    let (first, rest) = match body.split_once('\n') {
        Some((f, r)) if f.starts_with("#!") => (format!("{f}\n"), r),
        _ => (String::new(), body),
    };
    format!(
        "{first}# kioku invite (SPEC-M2.3): install kioku and join {url}\nKIOKU_JOIN_URL='{url}'\nKIOKU_JOIN_CODE='{code}'\n{rest}"
    )
}

/// install.ps1 with `$KiokuJoinUrl` / `$KiokuJoinCode` set, all inside one script block so
/// `irm … | iex` leaves no variables, functions or preferences behind in the window.
pub fn ps1_script(url: &str, code: &str) -> String {
    let body = INSTALL_PS1.trim_start_matches('\u{feff}');
    format!(
        "& {{\n# kioku invite (SPEC-M2.3): install kioku and join {url}\n$KiokuJoinUrl = '{url}'\n$KiokuJoinCode = '{code}'\n{body}\n}}\n"
    )
}

/// The one sentence (ja + en) for a link that does not work.
const INVALID_JA: &str = "この招待は無効か、期限切れか、使用済みです。サーバーで kioku invite をもう一度実行し、表示された行を貼り付けてください。";
const INVALID_EN: &str = "This invite is invalid, expired or already used: run kioku invite on the server again and paste the new line.";
const LIMITED_JA: &str = "失敗した試行が多すぎます。1 分待ってから、サーバーで kioku invite をもう一度実行してください。";
const LIMITED_EN: &str =
    "Too many failed attempts: wait a minute, then run kioku invite on the server again.";
const HOST_JA: &str = "リクエストに正しい Host ヘッダーがありません。kioku invite が表示した行をそのまま使ってください。";
const HOST_EN: &str =
    "The request has no valid Host header: use the line kioku invite printed as it is.";

/// State of the public routes: the invites and the token handed out by `/api/v1/join`.
#[derive(Clone)]
pub struct JoinState {
    /// The invites.
    pub invites: Arc<Invites>,
    /// The server's `[server] auth_token`.
    pub token: Arc<str>,
}

/// `GET /i/{code}` and `POST /api/v1/join` (no token).
pub fn public_routes(state: JoinState) -> Router {
    Router::new()
        .route("/i/{code}", get(script))
        .route("/api/v1/join", post(join))
        .with_state(state)
}

/// `POST /api/v1/invites` (goes behind the bearer middleware).
pub fn protected_routes(state: JoinState) -> Router {
    Router::new()
        .route("/api/v1/invites", post(create_invite))
        .with_state(state)
}

/// The peer's address (IPv4-mapped IPv6 as IPv4); `0.0.0.0` when the server was started
/// without connect info (tests).
fn peer(req_ext: &axum::http::Extensions) -> IpAddr {
    req_ext
        .get::<ConnectInfo<SocketAddr>>()
        .map(|c| c.0.ip().to_canonical())
        .unwrap_or(IpAddr::V4(Ipv4Addr::UNSPECIFIED))
}

#[derive(Clone, Copy, PartialEq, Eq)]
enum Flavor {
    Sh,
    Ps1,
}

fn text(status: StatusCode, body: String) -> Response {
    (
        status,
        [
            (header::CONTENT_TYPE, "text/plain; charset=utf-8"),
            (header::CACHE_CONTROL, "no-store"),
        ],
        body,
    )
        .into_response()
}

/// A failure body: for sh a one-line script that explains and exits 1 (`curl … | sh` without
/// `-f` runs it); for PowerShell the plain sentences (`irm` shows the body of an error
/// response as the error message).
fn failure(flavor: Flavor, status: StatusCode, ja: &str, en: &str) -> Response {
    let body = match flavor {
        Flavor::Sh => format!("echo 'kioku: {ja}' >&2; echo 'kioku: {en}' >&2; exit 1\n"),
        Flavor::Ps1 => format!("kioku: {ja}\nkioku: {en}\n"),
    };
    text(status, body)
}

/// `GET /i/{code}` (sh) and `GET /i/{code}.ps1` (PowerShell): the installer with the join
/// variables for a valid invite. Does not consume a use.
async fn script(State(st): State<JoinState>, Path(raw): Path<String>, req: Request) -> Response {
    let (code, flavor) = match raw.strip_suffix(".ps1") {
        Some(c) => (c.to_string(), Flavor::Ps1),
        None => (
            raw.strip_suffix(".sh").unwrap_or(&raw).to_string(),
            Flavor::Sh,
        ),
    };
    let Some(url) = join_url(req.headers(), req.uri()) else {
        return failure(flavor, StatusCode::BAD_REQUEST, HOST_JA, HOST_EN);
    };
    let peer = peer(req.extensions());
    let (lookup, canonical) = st.invites.lookup(&code, peer, false);
    match (lookup, canonical) {
        (Lookup::Valid, Some(code)) => {
            let body = match flavor {
                Flavor::Sh => sh_script(&url, &code),
                Flavor::Ps1 => ps1_script(&url, &code),
            };
            text(StatusCode::OK, body)
        }
        (Lookup::Limited, _) => failure(
            flavor,
            StatusCode::TOO_MANY_REQUESTS,
            LIMITED_JA,
            LIMITED_EN,
        ),
        _ => failure(flavor, StatusCode::NOT_FOUND, INVALID_JA, INVALID_EN),
    }
}

fn json_error(status: StatusCode, message: String) -> Response {
    (status, Json(json!({ "error": message }))).into_response()
}

/// Body of `POST /api/v1/join`.
#[derive(Debug, Deserialize)]
struct JoinBody {
    code: String,
}

/// `POST /api/v1/join {code}` → `{token, server_url}`; consumes one use.
async fn join(State(st): State<JoinState>, req: Request) -> Response {
    let Some(url) = join_url(req.headers(), req.uri()) else {
        return json_error(StatusCode::BAD_REQUEST, HOST_EN.to_string());
    };
    let peer = peer(req.extensions());
    let bytes: Bytes = match axum::body::to_bytes(req.into_body(), 64 * 1024).await {
        Ok(b) => b,
        Err(e) => return json_error(StatusCode::BAD_REQUEST, format!("reading body: {e}")),
    };
    let body: JoinBody = match serde_json::from_slice(&bytes) {
        Ok(b) => b,
        Err(e) => return json_error(StatusCode::BAD_REQUEST, format!("invalid join body: {e}")),
    };
    match st.invites.consume(&body.code, peer) {
        Lookup::Valid => (
            StatusCode::OK,
            [(header::CACHE_CONTROL, "no-store")],
            Json(json!({ "token": &*st.token, "server_url": url })),
        )
            .into_response(),
        Lookup::Limited => json_error(StatusCode::TOO_MANY_REQUESTS, LIMITED_EN.to_string()),
        Lookup::Invalid => json_error(StatusCode::NOT_FOUND, INVALID_EN.to_string()),
    }
}

/// Body of `POST /api/v1/invites` (every field optional; an empty body is fine).
#[derive(Debug, Default, Deserialize)]
struct CreateBody {
    #[serde(default)]
    ttl_minutes: Option<u32>,
    #[serde(default)]
    uses: Option<u32>,
}

/// `POST /api/v1/invites {ttl_minutes?, uses?}` → `{code, expires_at, uses}`.
async fn create_invite(State(st): State<JoinState>, body: Bytes) -> Response {
    let parsed: CreateBody = if body.iter().all(u8::is_ascii_whitespace) {
        CreateBody::default()
    } else {
        match serde_json::from_slice(&body) {
            Ok(b) => b,
            Err(e) => {
                return json_error(StatusCode::BAD_REQUEST, format!("invalid invite body: {e}"));
            }
        }
    };
    if st.token.trim().is_empty() {
        return json_error(
            StatusCode::CONFLICT,
            "this server has no auth_token, so there is nothing to hand out: run kioku init".into(),
        );
    }
    let inv = st.invites.create(parsed.ttl_minutes, parsed.uses);
    Json(json!({
        "code": inv.code,
        "expires_at": kioku_core::util::fmt_ts_secs(inv.expires_at),
        "uses": inv.uses_left,
    }))
    .into_response()
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::atomic::{AtomicI64, Ordering};

    fn clocked() -> (Arc<Invites>, Arc<AtomicI64>) {
        let offset = Arc::new(AtomicI64::new(0));
        let base = Utc::now();
        let o = offset.clone();
        let clock: Clock = Arc::new(move || base + Duration::seconds(o.load(Ordering::SeqCst)));
        (Arc::new(Invites::with_clock(clock)), offset)
    }

    const PEER: IpAddr = IpAddr::V4(Ipv4Addr::new(192, 168, 1, 20));

    #[test]
    fn codes_are_crockford_without_ambiguous_letters() {
        assert_eq!(CODE_ALPHABET.len(), 32);
        for b in b"ILOU" {
            assert!(!CODE_ALPHABET.contains(b));
        }
        for _ in 0..200 {
            let c = new_code();
            assert_eq!(c.len(), CODE_LEN);
            assert_eq!(
                normalize_code(&c.to_lowercase()).as_deref(),
                Some(c.as_str())
            );
        }
        assert_eq!(normalize_code(" k7q2m9xd ").as_deref(), Some("K7Q2M9XD"));
        assert_eq!(normalize_code("K7Q2M9XO"), None, "O is not in the alphabet");
        assert_eq!(normalize_code("K7Q2M9X"), None);
        assert_eq!(normalize_code("招待コード"), None);
    }

    #[test]
    fn defaults_caps_uses_and_expiry() {
        let (inv, clock) = clocked();
        let i = inv.create(None, None);
        assert_eq!(i.uses_left, 1);
        assert_eq!(i.expires_at - i.created_at, Duration::minutes(10));
        let capped = inv.create(Some(600), Some(99));
        assert_eq!(capped.uses_left, MAX_USES);
        assert_eq!(capped.expires_at - capped.created_at, Duration::minutes(60));
        assert_eq!(inv.create(Some(0), Some(0)).uses_left, 1);

        let two = inv.create(Some(10), Some(2));
        let lower = two.code.to_lowercase();
        assert_eq!(inv.check(&lower, PEER), Lookup::Valid, "case-insensitive");
        assert_eq!(
            inv.check(&lower, PEER),
            Lookup::Valid,
            "check does not consume"
        );
        assert_eq!(inv.consume(&lower, PEER), Lookup::Valid);
        assert_eq!(inv.consume(&two.code, PEER), Lookup::Valid);
        assert_eq!(inv.consume(&two.code, PEER), Lookup::Invalid, "used up");

        assert_eq!(inv.check(&i.code, PEER), Lookup::Valid);
        clock.store(10 * 60, Ordering::SeqCst);
        assert_eq!(inv.check(&i.code, PEER), Lookup::Invalid, "expired");
        clock.store(61 * 60, Ordering::SeqCst);
        assert!(inv.is_empty(), "expired invites are purged");
    }

    #[test]
    fn rate_limit_per_peer_and_total() {
        let (inv, clock) = clocked();
        let good = inv.create(Some(60), Some(20));
        for _ in 0..MAX_FAILURES_PER_PEER {
            assert_eq!(inv.check("AAAAAAAA", PEER), Lookup::Invalid);
        }
        assert_eq!(inv.check(&good.code, PEER), Lookup::Limited);
        let other = IpAddr::V4(Ipv4Addr::new(10, 0, 0, 2));
        assert_eq!(inv.check(&good.code, other), Lookup::Valid, "per peer");
        clock.store(61, Ordering::SeqCst);
        assert_eq!(inv.check(&good.code, PEER), Lookup::Valid, "block lifted");

        // Overall: 30 failures from many peers block everyone.
        for n in 0..MAX_FAILURES_TOTAL {
            let p = IpAddr::V4(Ipv4Addr::new(10, 1, (n / 200) as u8, (n % 200) as u8));
            inv.check("ZZZZZZZZ", p);
        }
        assert_eq!(inv.check(&good.code, other), Lookup::Limited);
        clock.store(200, Ordering::SeqCst);
        assert_eq!(inv.check(&good.code, other), Lookup::Valid);
    }

    #[test]
    fn host_header_rules() {
        let uri: axum::http::Uri = "/i/X".parse().unwrap();
        let h = |v: &str| {
            let mut m = HeaderMap::new();
            m.insert(header::HOST, v.parse().unwrap());
            join_url(&m, &uri)
        };
        assert_eq!(
            h("192.168.1.240:7391").as_deref(),
            Some("http://192.168.1.240:7391")
        );
        assert_eq!(
            h("mini-M2.local:7391").as_deref(),
            Some("http://mini-M2.local:7391")
        );
        assert_eq!(
            h("[fe80::1]:7391").as_deref(),
            Some("http://[fe80::1]:7391")
        );
        assert_eq!(h("example.com").as_deref(), Some("http://example.com"));
        for bad in [
            "",
            "a b",
            "evil'; rm -rf ~",
            "h:99999",
            "h:",
            "user@h:1",
            "[zz]:1",
            ":7391",
            "h:1:2",
        ] {
            assert_eq!(h(bad), None, "{bad}");
        }
        assert_eq!(join_url(&HeaderMap::new(), &uri), None);
    }

    #[test]
    fn scripts_get_the_variables() {
        let sh = sh_script("http://192.168.1.240:7391", "K7Q2M9XD");
        let mut lines = sh.lines();
        assert_eq!(lines.next(), Some("#!/bin/sh"));
        assert!(!sh.contains('\r'), "LF only, whatever the checkout");
        assert!(sh.contains("\nKIOKU_JOIN_URL='http://192.168.1.240:7391'\n"));
        assert!(sh.contains("\nKIOKU_JOIN_CODE='K7Q2M9XD'\n"));
        assert!(sh.trim_end().ends_with("main \"$@\""));
        let ps = ps1_script("http://192.168.1.240:7391", "K7Q2M9XD");
        assert!(ps.starts_with("& {\n"));
        assert!(ps.contains("\n$KiokuJoinUrl = 'http://192.168.1.240:7391'\n"));
        assert!(ps.contains("\n$KiokuJoinCode = 'K7Q2M9XD'\n"));
        assert!(ps.trim_end().ends_with('}'));
    }
}
