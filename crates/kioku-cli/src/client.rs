//! Blocking HTTP client for the kioku server API (`[client]` config), with a hard deadline.
//!
//! Hooks and the `search` / `status` / `reindex` commands all go through this client: a
//! laptop running only hooks has no data dir, so nothing here touches the store directly.

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

/// API client bound to one server URL, token and deadline.
#[derive(Clone, Debug)]
pub struct ApiClient {
    base: Url,
    token: Option<String>,
    http: Client,
    deadline: Instant,
}

impl ApiClient {
    /// Builds a client whose requests must all finish within `total` from now.
    pub fn new(cfg: &ClientConfig, total: Duration) -> anyhow::Result<ApiClient> {
        let base = Url::parse(cfg.server_url.trim())
            .with_context(|| format!("invalid [client] server_url: {}", cfg.server_url))?;
        if !matches!(base.scheme(), "http" | "https") {
            anyhow::bail!("[client] server_url must be http(s): {}", cfg.server_url);
        }
        let mut builder = Client::builder().timeout(total);
        if is_local_url(&base) {
            // A proxy from the environment must never see requests to a server on this
            // machine or the local network (it could not reach it, and would see the token).
            // Other hosts follow HTTP(S)_PROXY / NO_PROXY as usual.
            builder = builder.no_proxy();
        }
        let http = builder.build().context("building HTTP client")?;
        Ok(ApiClient {
            base,
            token: cfg
                .auth_token
                .as_deref()
                .map(str::trim)
                .filter(|t| !t.is_empty())
                .map(str::to_string),
            http,
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
        self.send(self.http.get(url).query(query))
    }

    /// Authenticated POST of a JSON body to `/api/v1/<segments>`; returns the JSON body.
    pub fn post(&self, segments: &[&str], body: &Value) -> anyhow::Result<Value> {
        let url = self.api_url(segments)?;
        self.send(self.http.post(url).json(body))
    }

    fn send(&self, req: RequestBuilder) -> anyhow::Result<Value> {
        let remaining = self.deadline.saturating_duration_since(Instant::now());
        if remaining.is_zero() {
            anyhow::bail!("deadline exceeded before the request was sent");
        }
        let mut req = req.timeout(remaining);
        if let Some(token) = &self.token {
            req = req.bearer_auth(token);
        }
        let resp = req.send().map_err(|e| anyhow!("request failed: {e}"))?;
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

/// The HTTP status of an error produced by [`ApiClient`], if it was a server response.
pub fn http_status(err: &anyhow::Error) -> Option<u16> {
    err.downcast_ref::<HttpError>().map(|e| e.status)
}

#[cfg(test)]
mod tests {
    use super::*;

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
