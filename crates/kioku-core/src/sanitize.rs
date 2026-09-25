//! Secret redaction and size limits applied to hook payloads before they leave the machine (spec §8.2).
//!
//! Redaction is applied to every string leaf of a JSON value (and to object values whose key
//! looks like a secret) so the payload stays structured JSON that the digest can still read.

use std::sync::OnceLock;

use regex::Regex;
use serde_json::Value;

use crate::util::truncate_chars;

/// Replacement text for redacted secrets.
pub const REDACTED: &str = "[REDACTED]";
/// Max serialized chars kept for `tool_response`.
pub const TOOL_RESPONSE_MAX: usize = 2000;
/// Max serialized chars kept for `tool_input`.
pub const TOOL_INPUT_MAX: usize = 4000;

struct Patterns {
    whole: Vec<Regex>,
    key_value: Regex,
    secret_key: Regex,
}

fn patterns() -> &'static Patterns {
    static P: OnceLock<Patterns> = OnceLock::new();
    P.get_or_init(|| {
        let whole = [
            r"-----BEGIN [A-Z ]*PRIVATE KEY-----[\s\S]*?-----END [A-Z ]*PRIVATE KEY-----",
            r"AKIA[0-9A-Z]{16}",
            r"sk-[A-Za-z0-9_-]{16,}",
            r"ghp_[A-Za-z0-9]{36}",
            r"gho_[A-Za-z0-9]{36}",
            r"xox[bap]-[A-Za-z0-9-]{10,}",
        ]
        .iter()
        .map(|p| Regex::new(p).expect("valid secret regex"))
        .collect();
        Patterns {
            whole,
            key_value: Regex::new(
                r"(?i)(?P<key>authorization|api[_-]?key|secret|password|token)(?P<sep>\s*[:=]\s*)(?P<value>(?:(?:bearer|basic|token)\s+)?\S+)",
            )
            .expect("valid key/value regex"),
            secret_key: Regex::new(r"(?i)^(authorization|api[_-]?key|secret|password|token)$")
                .expect("valid key regex"),
        }
    })
}

/// Redacts secrets in free text (tokens, private keys, `password=…`-style values).
///
/// For `authorization: Bearer <token>` the scheme word is swallowed with the value so the
/// token itself does not survive.
pub fn redact(text: &str) -> String {
    let p = patterns();
    let mut out = text.to_string();
    for re in &p.whole {
        if re.is_match(&out) {
            out = re.replace_all(&out, REDACTED).into_owned();
        }
    }
    if p.key_value.is_match(&out) {
        out = p
            .key_value
            .replace_all(&out, |c: &regex::Captures| {
                if &c["value"] == REDACTED {
                    c[0].to_string()
                } else {
                    format!("{}{}{REDACTED}", &c["key"], &c["sep"])
                }
            })
            .into_owned();
    }
    out
}

/// Redacts every string in a JSON value, including values stored under secret-looking keys.
pub fn redact_value(value: &Value) -> Value {
    let p = patterns();
    match value {
        Value::String(s) => Value::String(redact(s)),
        Value::Array(items) => Value::Array(items.iter().map(redact_value).collect()),
        Value::Object(map) => Value::Object(
            map.iter()
                .map(|(k, v)| {
                    let v = if p.secret_key.is_match(k) && (v.is_string() || v.is_number()) {
                        Value::String(REDACTED.to_string())
                    } else {
                        redact_value(v)
                    };
                    (k.clone(), v)
                })
                .collect(),
        ),
        other => other.clone(),
    }
}

/// Redacts and shrinks a JSON value until its serialized form is at most `max_chars` chars.
///
/// Long string leaves are truncated first (keeping keys and small fields such as
/// `file_path` / `is_error` intact); if the skeleton alone is too big the whole value
/// becomes a truncated string.
pub fn sanitize_json(value: &Value, max_chars: usize) -> Value {
    let mut v = redact_value(value);
    for _ in 0..64 {
        let len = serialized_len(&v);
        if len <= max_chars {
            return v;
        }
        let excess = len - max_chars;
        let Some(longest) = longest_string_len(&v) else {
            break;
        };
        if longest <= 1 {
            break;
        }
        let target = longest.saturating_sub(excess + 1).max(1);
        shrink_longest(&mut v, longest, target);
    }
    if serialized_len(&v) <= max_chars {
        return v;
    }
    let text = match &v {
        Value::String(s) => s.clone(),
        other => other.to_string(),
    };
    let mut max = max_chars.saturating_sub(2);
    loop {
        let s = Value::String(truncate_chars(&text, max));
        if serialized_len(&s) <= max_chars || max == 0 {
            return s;
        }
        max = max.saturating_sub((serialized_len(&s) - max_chars).max(1));
    }
}

/// Sanitizes a hook payload in place: `prompt` is redacted, `tool_input` / `tool_response`
/// are redacted and size-limited (4 000 / 2 000 chars).
pub fn sanitize_payload(payload: &Value) -> Value {
    let Value::Object(map) = payload else {
        return redact_value(payload);
    };
    let mut out = serde_json::Map::new();
    for (k, v) in map {
        let v = match k.as_str() {
            "tool_input" => sanitize_json(v, TOOL_INPUT_MAX),
            "tool_response" => sanitize_json(v, TOOL_RESPONSE_MAX),
            _ => redact_value(v),
        };
        out.insert(k.clone(), v);
    }
    Value::Object(out)
}

fn serialized_len(v: &Value) -> usize {
    match v {
        Value::String(s) => serde_json::to_string(s)
            .map(|x| x.chars().count().saturating_sub(2))
            .unwrap_or(0),
        other => other.to_string().chars().count(),
    }
}

fn longest_string_len(v: &Value) -> Option<usize> {
    match v {
        Value::String(s) => Some(s.chars().count()),
        Value::Array(items) => items.iter().filter_map(longest_string_len).max(),
        Value::Object(map) => map.values().filter_map(longest_string_len).max(),
        _ => None,
    }
}

fn shrink_longest(v: &mut Value, longest: usize, target: usize) -> bool {
    match v {
        Value::String(s) if s.chars().count() == longest => {
            *s = truncate_chars(s, target);
            true
        }
        Value::Array(items) => items
            .iter_mut()
            .any(|item| shrink_longest(item, longest, target)),
        Value::Object(map) => map
            .values_mut()
            .any(|item| shrink_longest(item, longest, target)),
        _ => false,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn redacted_all(input: &str, secret: &str) {
        let out = redact(input);
        assert!(!out.contains(secret), "{input} -> {out}");
        assert!(out.contains(REDACTED), "{input} -> {out}");
    }

    #[test]
    fn aws_access_key() {
        redacted_all("key AKIAIOSFODNN7EXAMPLE here", "AKIAIOSFODNN7EXAMPLE");
        assert_eq!(redact("AKIA-short"), "AKIA-short");
    }

    #[test]
    fn openai_style_key() {
        redacted_all(
            "use sk-proj_abcdefghijklmnop123 now",
            "sk-proj_abcdefghijklmnop123",
        );
        assert_eq!(redact("sk-short"), "sk-short");
    }

    #[test]
    fn github_tokens() {
        let ghp = format!("ghp_{}", "a1B2".repeat(9));
        let gho = format!("gho_{}", "Z9y8".repeat(9));
        redacted_all(&format!("t={ghp}"), &ghp);
        redacted_all(&format!("oauth {gho} end"), &gho);
    }

    #[test]
    fn slack_tokens() {
        for t in [
            "xoxb-1234567890-abcdef",
            "xoxa-2-abcdefghijkl",
            "xoxp-11111-22222-abc",
        ] {
            redacted_all(&format!("slack {t}"), t);
        }
    }

    #[test]
    fn private_key_block() {
        let key =
            "-----BEGIN RSA PRIVATE KEY-----\nMIIEowIBAAKCAQEA\nabc\n-----END RSA PRIVATE KEY-----";
        let out = redact(&format!("before\n{key}\nafter"));
        assert_eq!(out, format!("before\n{REDACTED}\nafter"));
        let ossh = "-----BEGIN OPENSSH PRIVATE KEY-----\nb3Bl\n-----END OPENSSH PRIVATE KEY-----";
        assert_eq!(redact(ossh), REDACTED);
    }

    #[test]
    fn key_value_redacts_only_the_value() {
        assert_eq!(redact("password=hunter2"), "password=[REDACTED]");
        assert_eq!(redact("API_KEY: abc123 rest"), "API_KEY: [REDACTED] rest");
        assert_eq!(redact("api-key=abc"), "api-key=[REDACTED]");
        assert_eq!(redact("apikey = xyz"), "apikey = [REDACTED]");
        assert_eq!(redact("Secret: s3cr3t"), "Secret: [REDACTED]");
        assert_eq!(
            redact("export GITHUB_TOKEN=abc"),
            "export GITHUB_TOKEN=[REDACTED]"
        );
        assert_eq!(
            redact("Authorization: Bearer abc"),
            "Authorization: [REDACTED]"
        );
        // idempotent
        let once = redact("token: abc");
        assert_eq!(redact(&once), once);
        // plain prose untouched
        assert_eq!(redact("トークンの設計について"), "トークンの設計について");
    }

    #[test]
    fn json_keys_and_leaves() {
        let v = json!({
            "command": "curl -H 'Authorization: Bearer x' -d password=pw https://x",
            "api_key": "abc",
            "nested": [{"token": 123}, "AKIAIOSFODNN7EXAMPLE"],
            "file_path": "/src/main.rs"
        });
        let out = redact_value(&v);
        let s = out.to_string();
        assert!(!s.contains("pw "), "{s}");
        assert!(!s.contains("AKIAIOSFODNN7EXAMPLE"));
        assert_eq!(out["api_key"], REDACTED);
        assert_eq!(out["nested"][0]["token"], REDACTED);
        assert_eq!(out["file_path"], "/src/main.rs");
    }

    #[test]
    fn truncation_limits_and_keeps_structure() {
        let payload = json!({
            "prompt": "password=pw",
            "tool_name": "Write",
            "tool_input": {"file_path": "/a.rs", "content": "あ".repeat(10_000)},
            "tool_response": {"is_error": true, "stdout": "x".repeat(5_000), "stderr": "Error: boom"},
        });
        let out = sanitize_payload(&payload);
        assert_eq!(out["prompt"], "password=[REDACTED]");
        let input = &out["tool_input"];
        assert!(input.to_string().chars().count() <= TOOL_INPUT_MAX);
        assert_eq!(input["file_path"], "/a.rs");
        let resp = &out["tool_response"];
        assert!(resp.to_string().chars().count() <= TOOL_RESPONSE_MAX);
        assert_eq!(resp["is_error"], true);
        assert_eq!(resp["stderr"], "Error: boom");
        // already-sanitized payloads are a fixed point
        assert_eq!(sanitize_payload(&out), out);
    }

    #[test]
    fn string_response_truncated() {
        let out = sanitize_json(&json!("y".repeat(3000)), TOOL_RESPONSE_MAX);
        let s = out.as_str().unwrap();
        assert!(s.chars().count() <= TOOL_RESPONSE_MAX);
        assert!(s.ends_with('…'));
        let huge_keys: serde_json::Map<String, Value> =
            (0..500).map(|i| (format!("key{i}"), json!(i))).collect();
        let out = sanitize_json(&Value::Object(huge_keys), 200);
        assert!(serialized_len(&out) <= 200);
    }
}
