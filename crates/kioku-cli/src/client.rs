//! Blocking HTTP client for the kioku server API (`[client]` config), with a hard deadline.
//!
//! Hooks and the `search` / `status` / `reindex` commands all go through this client: a
//! laptop running only hooks has no data dir, so nothing here touches the store directly.
//! It also owns connection robustness (SPEC-M2 §19.2): the connect timeout is split across
//! a name's addresses, and a named server's last-good addresses are tried first.

use std::collections::BTreeMap;
use std::net::SocketAddr;
use std::path::PathBuf;
use std::time::{Duration, Instant};

use anyhow::{Context, anyhow};
use kioku_core::ClientConfig;
use reqwest::Url;
use reqwest::blocking::{Client, RequestBuilder};
use serde_json::Value;

/// Timeout used by interactive commands (`search`, `status`, `reindex`, `init --client-only`).
pub const COMMAND_TIMEOUT: Duration = Duration::from_secs(600);

/// An error response from the server (non-2xx status).
#[derive(Debug)]
pub struct HttpError {
    /// HTTP status code.
    pub status: u16,
    /// The server's `{error}` message (or the raw body).
    pub message: String,
}

impl std::fmt::Display for HttpError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "server returned HTTP {}: {}", self.status, self.message)
    }
}

impl std::error::Error for HttpError {}

/// The request reached the server but no response arrived before the deadline: the server
/// may still be working on it (a finalize runs to the end on its own).
#[derive(Debug)]
pub struct TimedOut {
    /// The transport error, with its cause.
    pub message: String,
}

impl std::fmt::Display for TimedOut {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "request failed: {}", self.message)
    }
}

impl std::error::Error for TimedOut {}

/// Turns a transport error into [`TimedOut`] (sent, no answer in time) or a plain error,
/// keeping the cause (`operation timed out`, `connection refused`, …) in the message.
fn transport_error(e: reqwest::Error) -> anyhow::Error {
    let mut message = e.to_string();
    let mut source = std::error::Error::source(&e);
    while let Some(s) = source {
        message.push_str(&format!(": {s}"));
        source = s.source();
    }
    if e.is_timeout() && !e.is_connect() {
        TimedOut { message }.into()
    } else {
        anyhow!("request failed: {message}")
    }
}

/// File under `~/.kioku/state/` holding named servers' last-good addresses (SPEC-M2 §19.2).
pub const ADDR_CACHE_FILE: &str = "server-addrs.json";

/// Upper bound of the connect timeout: a LAN or VPN connect never needs longer, and a long
/// command deadline must not let one dead address stall it (SPEC-M2 §19.2).
const CONNECT_TIMEOUT_CAP: Duration = Duration::from_secs(4);

/// Connect timeout for the cached-address attempt: half of that attempt's budget (a third of
/// the deadline), so an address that no longer answers fails as a *connect* error, which
/// falls back to DNS even for a POST. When it equalled the budget, the request timeout fired
/// first, the error was not `is_connect`, and POSTs never fell back (review of ca8afe5).
fn pinned_connect_timeout(total: Duration) -> Duration {
    total / 6
}

/// Addresses kept per server in [`ADDR_CACHE_FILE`].
const ADDR_CACHE_MAX: usize = 4;

/// API client bound to one server URL, token and deadline.
#[derive(Clone, Debug)]
pub struct ApiClient {
    base: Url,
    token: Option<String>,
    http: Client,
    /// Same client with DNS replaced by the cached last-good addresses (named hosts only).
    pinned: Option<Client>,
    /// `host:port` key of a named server in the address cache.
    cache_key: Option<String>,
    cache_path: Option<PathBuf>,
    deadline: Instant,
}

impl ApiClient {
    /// Builds a client whose requests must all finish within `total` from now, with the
    /// address cache in `~/.kioku/state/`.
    pub fn new(cfg: &ClientConfig, total: Duration) -> anyhow::Result<ApiClient> {
        let cache = kioku_core::util::home_dir_opt()
            .map(|h| h.join(".kioku").join("state").join(ADDR_CACHE_FILE));
        ApiClient::with_addr_cache(cfg, total, cache)
    }

    /// [`ApiClient::new`] with an explicit address-cache file (`None`: no cache).
    pub fn with_addr_cache(
        cfg: &ClientConfig,
        total: Duration,
        cache_path: Option<PathBuf>,
    ) -> anyhow::Result<ApiClient> {
        let base = Url::parse(cfg.server_url.trim())
            .with_context(|| format!("invalid [client] server_url: {}", cfg.server_url))?;
        if !matches!(base.scheme(), "http" | "https") {
            anyhow::bail!("[client] server_url must be http(s): {}", cfg.server_url);
        }
        let local = is_local_url(&base);
        let builder = |connect: Duration| {
            // hyper splits the connect timeout across a name's addresses and races IPv6
            // against IPv4, so one dead address cannot use up the whole deadline.
            let mut b = Client::builder().timeout(total).connect_timeout(connect);
            if local {
                // A proxy from the environment must never see requests to a server on this
                // machine or the local network (it could not reach it, and would see the
                // token). Other hosts follow HTTP(S)_PROXY / NO_PROXY as usual.
                b = b.no_proxy();
            }
            b
        };
        let http = builder((total * 2 / 3).min(CONNECT_TIMEOUT_CAP))
            .build()
            .context("building HTTP client")?;
        let cache_key = named_host_key(&base);
        let cached = match (&cache_key, &cache_path) {
            (Some(key), Some(path)) => read_addr_cache(path).remove(key).unwrap_or_default(),
            _ => Vec::new(),
        };
        let pinned = match (&cache_key, base.host_str()) {
            (Some(_), Some(host)) if !cached.is_empty() => Some(
                builder(pinned_connect_timeout(total))
                    .resolve_to_addrs(host, &cached)
                    .build()
                    .context("building HTTP client")?,
            ),
            _ => None,
        };
        Ok(ApiClient {
            base,
            token: cfg
                .auth_token
                .as_deref()
                .map(str::trim)
                .filter(|t| !t.is_empty())
                .map(str::to_string),
            http,
            pinned,
            cache_key,
            cache_path,
            deadline: Instant::now() + total,
        })
    }

    /// Absolute URL for `/api/v1/<segments…>` (each segment percent-encoded).
    pub fn api_url(&self, segments: &[&str]) -> anyhow::Result<Url> {
        let mut url = self.base.clone();
        {
            let mut path = url
                .path_segments_mut()
                .map_err(|_| anyhow!("server_url cannot be a base URL: {}", self.base))?;
            path.pop_if_empty().extend(["api", "v1"]).extend(segments);
        }
        Ok(url)
    }

    /// Authenticated GET of `/api/v1/<segments>` with query parameters; returns the JSON body.
    pub fn get(&self, segments: &[&str], query: &[(&str, String)]) -> anyhow::Result<Value> {
        let url = self.api_url(segments)?;
        self.send(true, |c| c.get(url.clone()).query(query))
    }

    /// Authenticated POST of a JSON body to `/api/v1/<segments>`; returns the JSON body.
    pub fn post(&self, segments: &[&str], body: &Value) -> anyhow::Result<Value> {
        let url = self.api_url(segments)?;
        self.send(false, |c| c.post(url.clone()).json(body))
    }

    /// Authenticated PUT of a JSON body to `/api/v1/<segments>`; returns the JSON body.
    pub fn put(&self, segments: &[&str], body: &Value) -> anyhow::Result<Value> {
        let url = self.api_url(segments)?;
        self.send(false, |c| c.put(url.clone()).json(body))
    }

    /// Sends the request built by `make`: first to the cached last-good addresses (with a
    /// third of the remaining time), then — if those do not connect — with normal DNS.
    fn send(
        &self,
        retry_timeout: bool,
        make: impl Fn(&Client) -> RequestBuilder,
    ) -> anyhow::Result<Value> {
        let prepare = |c: &Client, budget: Duration| -> anyhow::Result<RequestBuilder> {
            let remaining = self.deadline.saturating_duration_since(Instant::now());
            if remaining.is_zero() {
                anyhow::bail!("deadline exceeded before the request was sent");
            }
            let mut req = make(c).timeout(remaining.min(budget));
            if let Some(token) = &self.token {
                req = req.bearer_auth(token);
            }
            Ok(req)
        };
        if let Some(pinned) = &self.pinned {
            let third = self.deadline.saturating_duration_since(Instant::now()) / 3;
            match prepare(pinned, third)?.send() {
                Ok(resp) => return self.finish(resp),
                // Moved server or a different network: fall through to normal resolution.
                Err(e) if e.is_connect() || (retry_timeout && e.is_timeout()) => {}
                Err(e) => return Err(transport_error(e)),
            }
        }
        let resp = prepare(&self.http, Duration::MAX)?
            .send()
            .map_err(transport_error)?;
        self.finish(resp)
    }

    fn finish(&self, resp: reqwest::blocking::Response) -> anyhow::Result<Value> {
        if let (Some(key), Some(path), Some(addr)) =
            (&self.cache_key, &self.cache_path, resp.remote_addr())
        {
            remember_addr(path, key, addr, self.base.host_str().unwrap_or_default());
        }
        let status = resp.status();
        let text = resp.text().context("reading response body")?;
        let body: Value = serde_json::from_str(&text).unwrap_or(Value::String(text));
        if !status.is_success() {
            let message = body
                .get("error")
                .and_then(Value::as_str)
                .map(str::to_string)
                .unwrap_or_else(|| match &body {
                    Value::String(s) => s.clone(),
                    other => other.to_string(),
                });
            return Err(HttpError {
                status: status.as_u16(),
                message,
            }
            .into());
        }
        Ok(body)
    }
}

/// `host:port` of a URL whose host is a name (IP literals need no cache), else `None`.
fn named_host_key(url: &Url) -> Option<String> {
    let host = url.host_str()?;
    let bare = host.trim_start_matches('[').trim_end_matches(']');
    if bare.parse::<std::net::IpAddr>().is_ok() {
        return None;
    }
    Some(format!(
        "{}:{}",
        host.to_ascii_lowercase(),
        url.port_or_known_default()?
    ))
}

/// The address cache (§19.2); unreadable or corrupt → empty.
fn read_addr_cache(path: &std::path::Path) -> BTreeMap<String, Vec<SocketAddr>> {
    std::fs::read_to_string(path)
        .ok()
        .and_then(|t| serde_json::from_str::<BTreeMap<String, Vec<String>>>(&t).ok())
        .map(|m| {
            m.into_iter()
                .map(|(k, v)| (k, v.iter().filter_map(|a| a.parse().ok()).collect()))
                .collect()
        })
        .unwrap_or_default()
}

/// Puts `addr` first in `key`'s list (at most [`ADDR_CACHE_MAX`]); writes only on change.
/// A link-local IPv6 peer only works on this LAN, so the name's IPv4 addresses are kept
/// behind it (resolved once, while the list holds no IPv4) for use over a VPN.
/// Best effort: errors are ignored.
fn remember_addr(path: &std::path::Path, key: &str, addr: SocketAddr, host: &str) {
    let mut cache = read_addr_cache(path);
    let old = cache.get(key).cloned().unwrap_or_default();
    let mut new_addrs = vec![unmap(addr)];
    if is_link_local(&new_addrs[0]) && !old.iter().any(SocketAddr::is_ipv4) {
        use std::net::ToSocketAddrs;
        if let Ok(resolved) = (host, addr.port()).to_socket_addrs() {
            new_addrs.extend(resolved.filter(SocketAddr::is_ipv4));
        }
    }
    let merged = merge_addrs(&old, &new_addrs);
    if merged == old {
        return;
    }
    cache.insert(key.to_string(), merged);
    let text: BTreeMap<&String, Vec<String>> = cache
        .iter()
        .map(|(k, v)| (k, v.iter().map(ToString::to_string).collect()))
        .collect();
    if let Some(dir) = path.parent() {
        let _ = kioku_core::util::create_private_dir(dir);
    }
    if let Ok(json) = serde_json::to_string_pretty(&text) {
        let _ = kioku_core::util::write_private_file(path, &(json + "\n"));
    }
}

/// IPv4-mapped IPv6 peers (dual-stack servers) as plain IPv4.
fn unmap(addr: SocketAddr) -> SocketAddr {
    match addr {
        SocketAddr::V6(v6) => match v6.ip().to_ipv4_mapped() {
            Some(v4) => SocketAddr::new(v4.into(), v6.port()),
            None => addr,
        },
        v4 => v4,
    }
}

/// `fe80::/10`: reachable on this link only.
fn is_link_local(addr: &SocketAddr) -> bool {
    matches!(addr, SocketAddr::V6(v6) if (v6.ip().segments()[0] & 0xffc0) == 0xfe80)
}

/// `new` (in order, deduplicated) ahead of the old entries not in it, at most
/// [`ADDR_CACHE_MAX`] — but a new IPv4 address is never dropped for an old one.
fn merge_addrs(old: &[SocketAddr], new: &[SocketAddr]) -> Vec<SocketAddr> {
    let mut out: Vec<SocketAddr> = Vec::new();
    for a in new.iter().chain(old) {
        if !out.contains(a) {
            out.push(*a);
        }
    }
    let keep = ADDR_CACHE_MAX.max(new.len().min(ADDR_CACHE_MAX + 2));
    out.truncate(keep);
    out
}

/// True when the URL's host is `localhost` or a loopback IP.
pub fn is_loopback_url(url: &Url) -> bool {
    let Some(host) = url.host_str() else {
        return false;
    };
    let host = host.trim_start_matches('[').trim_end_matches(']');
    host.eq_ignore_ascii_case("localhost")
        || host
            .parse::<std::net::IpAddr>()
            .map(|ip| ip.is_loopback())
            .unwrap_or(false)
}

/// True when the URL points at this machine or a private network: loopback, `localhost`,
/// 10/8, 172.16/12, 192.168/16, fc00::/7, fe80::/10, or a `*.local` / `*.lan` /
/// `*.internal` name. Such requests bypass any proxy from the environment.
pub fn is_local_url(url: &Url) -> bool {
    use std::net::IpAddr;
    let Some(host) = url.host_str() else {
        return false;
    };
    let host = host
        .trim_start_matches('[')
        .trim_end_matches(']')
        .trim_end_matches('.')
        .to_ascii_lowercase();
    if is_loopback_url(url) {
        return true;
    }
    match host.parse::<IpAddr>() {
        Ok(IpAddr::V4(ip)) => ip.is_private(),
        Ok(IpAddr::V6(ip)) => {
            let first = ip.segments()[0];
            (first & 0xfe00) == 0xfc00 || (first & 0xffc0) == 0xfe80
        }
        Err(_) => [".local", ".lan", ".internal"]
            .iter()
            .any(|suffix| host.ends_with(suffix)),
    }
}

/// Whether an [`ApiClient`] error is a [`TimedOut`] (sent, no answer before the deadline).
pub fn timed_out(err: &anyhow::Error) -> bool {
    err.downcast_ref::<TimedOut>().is_some()
}

/// The HTTP status of an error produced by [`ApiClient`], if it was a server response.
pub fn http_status(err: &anyhow::Error) -> Option<u16> {
    err.downcast_ref::<HttpError>().map(|e| e.status)
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A tiny HTTP server on 127.0.0.1 answering every request with `{"ok":true}`.
    fn tiny_server() -> u16 {
        use std::io::{Read, Write};
        let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        let port = listener.local_addr().unwrap().port();
        std::thread::spawn(move || {
            for mut stream in listener.incoming().flatten() {
                let mut buf = [0u8; 4096];
                let _ = stream.read(&mut buf);
                let body = r#"{"ok":true}"#;
                let _ = write!(
                    stream,
                    "HTTP/1.1 200 OK\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}",
                    body.len()
                );
            }
        });
        port
    }

    fn dead_port() -> u16 {
        let l = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        l.local_addr().unwrap().port()
    }

    fn cfg(url: String) -> ClientConfig {
        ClientConfig {
            server_url: url,
            ..ClientConfig::default()
        }
    }

    /// A server that accepts and reads the request but never answers.
    fn silent_server() -> u16 {
        let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        let port = listener.local_addr().unwrap().port();
        std::thread::spawn(move || {
            let mut held = Vec::new();
            for s in listener.incoming().flatten() {
                held.push(s);
            }
        });
        port
    }

    #[test]
    fn a_request_the_server_never_answers_is_timed_out() {
        let c = ApiClient::new(
            &cfg(format!("http://127.0.0.1:{}", silent_server())),
            Duration::from_millis(300),
        )
        .unwrap();
        let err = c
            .post(&["sessions", "s", "finalize"], &Value::Null)
            .unwrap_err();
        assert!(timed_out(&err), "{err:#}");
        assert!(format!("{err}").contains("timed out"), "{err}");
    }

    #[test]
    fn a_refused_connection_is_not_timed_out() {
        let c = ApiClient::new(
            &cfg(format!("http://127.0.0.1:{}", dead_port())),
            Duration::from_millis(300),
        )
        .unwrap();
        let err = c
            .post(&["sessions", "s", "finalize"], &Value::Null)
            .unwrap_err();
        assert!(!timed_out(&err), "{err:#}");
        assert!(format!("{err}").starts_with("request failed: "), "{err}");
    }

    #[test]
    fn a_post_with_a_lost_response_is_not_retried_by_dns_fallback() {
        use std::sync::{
            Arc,
            atomic::{AtomicUsize, Ordering},
        };
        let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        listener.set_nonblocking(true).unwrap();
        let port = listener.local_addr().unwrap().port();
        let accepted = Arc::new(AtomicUsize::new(0));
        let seen = accepted.clone();
        let thread = std::thread::spawn(move || {
            let until = Instant::now() + Duration::from_millis(600);
            let mut held = Vec::new();
            while Instant::now() < until {
                if let Ok((stream, _)) = listener.accept() {
                    seen.fetch_add(1, Ordering::SeqCst);
                    held.push(stream);
                }
                std::thread::sleep(Duration::from_millis(5));
            }
        });
        let dir = tempfile::tempdir().unwrap();
        let cache = dir.path().join(ADDR_CACHE_FILE);
        write_cache(
            &cache,
            &format!("localhost:{port}"),
            &[format!("127.0.0.1:{port}")],
        );
        let c = ApiClient::with_addr_cache(
            &cfg(format!("http://localhost:{port}")),
            Duration::from_millis(300),
            Some(cache),
        )
        .unwrap();
        assert!(
            c.post(&["observations"], &serde_json::json!({"test":true}))
                .is_err()
        );
        thread.join().unwrap();
        assert_eq!(
            accepted.load(Ordering::SeqCst),
            1,
            "response timeout must not replay a mutation"
        );
    }

    fn write_cache(path: &std::path::Path, key: &str, addrs: &[String]) {
        let v = serde_json::json!({ key: addrs });
        std::fs::write(path, v.to_string()).unwrap();
    }

    /// WireGuard case (SPEC-M2 §19.2): the name no longer resolves, the cached LAN address works.
    #[test]
    fn unresolvable_name_uses_the_cached_address() {
        let port = tiny_server();
        let dir = tempfile::tempdir().unwrap();
        let cache = dir.path().join(ADDR_CACHE_FILE);
        let key = format!("kioku-test.invalid:{port}");
        write_cache(&cache, &key, &[format!("127.0.0.1:{port}")]);
        let c = ApiClient::with_addr_cache(
            &cfg(format!("http://kioku-test.invalid:{port}")),
            Duration::from_secs(3),
            Some(cache.clone()),
        )
        .unwrap();
        assert_eq!(c.get(&["health"], &[]).unwrap()["ok"], true);
        // Without the cache the same name fails.
        let bare = ApiClient::with_addr_cache(
            &cfg(format!("http://kioku-test.invalid:{port}")),
            Duration::from_secs(3),
            None,
        )
        .unwrap();
        assert!(bare.get(&["health"], &[]).is_err());
    }

    /// A moved server: the cached address is dead, normal resolution finds it and the cache
    /// is updated.
    #[test]
    fn stale_cache_falls_back_to_dns_and_is_updated() {
        let port = tiny_server();
        let dir = tempfile::tempdir().unwrap();
        let cache = dir.path().join(ADDR_CACHE_FILE);
        let key = format!("localhost:{port}");
        write_cache(&cache, &key, &[format!("127.0.0.1:{}", dead_port())]);
        let c = ApiClient::with_addr_cache(
            &cfg(format!("http://localhost:{port}")),
            Duration::from_secs(3),
            Some(cache.clone()),
        )
        .unwrap();
        assert_eq!(c.get(&["health"], &[]).unwrap()["ok"], true);
        let now = read_addr_cache(&cache);
        assert_eq!(
            now[&key].first().unwrap().to_string(),
            format!("127.0.0.1:{port}"),
            "{now:?}"
        );
        assert_eq!(
            now[&key].len(),
            2,
            "the old address is kept behind: {now:?}"
        );
    }

    #[test]
    fn a_dead_cached_address_fails_at_connect_before_the_attempt_times_out() {
        for total in [Duration::from_millis(900), Duration::from_secs(5)] {
            assert!(pinned_connect_timeout(total) < total / 3);
        }
    }

    #[test]
    fn link_local_peers_keep_the_ipv4_addresses_for_vpn_use() {
        let a = |s: &str| s.parse::<SocketAddr>().unwrap();
        let ll = a("[fe80::1%14]:7391");
        assert!(is_link_local(&ll));
        assert!(!is_link_local(&a("192.168.1.240:7391")));
        assert_eq!(
            unmap(a("[::ffff:192.168.1.240]:7391")),
            a("192.168.1.240:7391")
        );
        // Link-local first, the name's IPv4 addresses kept behind it, old entries after.
        let merged = merge_addrs(
            &[a("10.0.0.9:7391")],
            &[ll, a("192.168.1.240:7391"), a("192.168.1.57:7391")],
        );
        assert_eq!(
            merged,
            [
                ll,
                a("192.168.1.240:7391"),
                a("192.168.1.57:7391"),
                a("10.0.0.9:7391")
            ]
        );
        // Re-learning the same first address changes nothing.
        assert_eq!(merge_addrs(&merged, &[ll]), merged);
    }

    #[test]
    fn names_are_learned_ip_literals_are_not() {
        let port = tiny_server();
        let dir = tempfile::tempdir().unwrap();
        let cache = dir.path().join("state").join(ADDR_CACHE_FILE);
        let ip = ApiClient::with_addr_cache(
            &cfg(format!("http://127.0.0.1:{port}")),
            Duration::from_secs(3),
            Some(cache.clone()),
        )
        .unwrap();
        ip.get(&["health"], &[]).unwrap();
        assert!(!cache.exists(), "IP literals are not cached");
        let named = ApiClient::with_addr_cache(
            &cfg(format!("http://localhost:{port}")),
            Duration::from_secs(3),
            Some(cache.clone()),
        )
        .unwrap();
        named.get(&["health"], &[]).unwrap();
        let learned = read_addr_cache(&cache);
        assert_eq!(
            learned[&format!("localhost:{port}")][0].to_string(),
            format!("127.0.0.1:{port}")
        );
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            let mode = std::fs::metadata(&cache).unwrap().permissions().mode() & 0o777;
            assert_eq!(mode, 0o600);
        }
        // A corrupt cache is ignored, not fatal.
        std::fs::write(&cache, "{not json").unwrap();
        named.get(&["health"], &[]).unwrap();
    }

    fn client(url: &str) -> ApiClient {
        let cfg = ClientConfig {
            server_url: url.to_string(),
            ..ClientConfig::default()
        };
        ApiClient::new(&cfg, Duration::from_secs(1)).unwrap()
    }

    #[test]
    fn api_urls_encode_segments_and_keep_prefix() {
        let c = client("http://127.0.0.1:7391");
        assert_eq!(
            c.api_url(&["sessions", "a/b"]).unwrap().as_str(),
            "http://127.0.0.1:7391/api/v1/sessions/a%2Fb"
        );
        let c = client("https://home.example/kioku/");
        assert_eq!(
            c.api_url(&["status"]).unwrap().as_str(),
            "https://home.example/kioku/api/v1/status"
        );
    }

    #[test]
    fn loopback_urls() {
        for u in [
            "http://127.0.0.1:1",
            "http://localhost",
            "http://[::1]:7391",
        ] {
            assert!(is_loopback_url(&Url::parse(u).unwrap()), "{u}");
        }
        assert!(!is_loopback_url(
            &Url::parse("http://192.168.1.5:7391").unwrap()
        ));
    }

    #[test]
    fn local_network_urls_bypass_the_proxy() {
        for u in [
            "http://127.0.0.1:7391",
            "http://localhost:7391",
            "http://10.0.0.5:7391",
            "http://172.16.0.1",
            "http://172.31.255.254",
            "http://192.168.1.5:7391",
            "http://[fd12:3456::1]:7391",
            "http://[fe80::1]",
            "http://homeserver.local:7391",
            "https://nas.lan",
            "http://kioku.internal.",
            "http://KIOKU.Internal",
        ] {
            assert!(is_local_url(&Url::parse(u).unwrap()), "{u}");
        }
        for u in [
            "http://172.32.0.1",
            "http://8.8.8.8",
            "https://kioku.example.com",
            "http://[2001:db8::1]",
            "http://local.example.com",
            "http://mylan",
        ] {
            assert!(!is_local_url(&Url::parse(u).unwrap()), "{u}");
        }
    }

    #[test]
    fn rejects_bad_urls() {
        for url in ["not a url", "ftp://x"] {
            let cfg = ClientConfig {
                server_url: url.into(),
                ..ClientConfig::default()
            };
            assert!(
                ApiClient::new(&cfg, Duration::from_secs(1)).is_err(),
                "{url}"
            );
        }
    }
}
