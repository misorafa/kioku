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

/// Secret-looking key names (substring, case-insensitive) shared by the text and JSON-key rules.
const SECRET_WORD: &str = r"(secret|token|passw(?:or)?d|api[_-]?key|access[_-]?key|private[_-]?key|credential|authorization)";

/// Whole key names that hold a secret but are too short or common to match as substrings
/// (SPEC-M2.7 §9): `pass` / `pwd` / `passphrase` (optionally prefixed, `DB_PASS`,
/// `MYSQL_PWD`), any `*_key` (`encryption_key`, `signing_key`, `master_key`,
/// `supabase_key`), `*accountkey` and cookies.
const SECRET_NAME: &str =
    r"(?:[\w.-]*[_.-])?(?:pass|pwd|passphrase)|[\w.-]*_key|[\w.-]*accountkey|(?:set-)?cookie";

/// `*_key` names that are identifiers, not credentials (`primary_key`, `sort_key`, …).
const BENIGN_KEY_PREFIXES: [&str; 12] = [
    "primary",
    "foreign",
    "sort",
    "partition",
    "cache",
    "public",
    "map",
    "hash",
    "lookup",
    "unique",
    "group",
    "idempotency",
];

struct Patterns {
    whole: Vec<Regex>,
    unterminated_key: Regex,
    url_userinfo: Regex,
    key_value: Regex,
    name_value: Regex,
    cookie: Regex,
    flag_value: Regex,
    secret_key: Regex,
    secret_name: Regex,
}

fn patterns() -> &'static Patterns {
    static P: OnceLock<Patterns> = OnceLock::new();
    P.get_or_init(|| {
        let whole = [
            r"-----BEGIN [A-Z ]*PRIVATE KEY-----[\s\S]*?-----END [A-Z ]*PRIVATE KEY-----",
            r"AKIA[0-9A-Z]{16}",
            r"sk-[A-Za-z0-9_-]{16,}",
            r"sk_(?:live|test)_[A-Za-z0-9]{10,}",
            r"github_pat_[A-Za-z0-9_]{20,}",
            r"gh[opusr]_[A-Za-z0-9]{36}",
            r"xox[bap]-[A-Za-z0-9-]{10,}",
            r"AIza[0-9A-Za-z_-]{35}",
            r"eyJ[A-Za-z0-9_-]{10,}\.[A-Za-z0-9_-]{10,}\.[A-Za-z0-9_-]{10,}",
            // SPEC-M2.7 §9
            r"ASIA[0-9A-Z]{16}",
            r"npm_[A-Za-z0-9]{36}",
            r"glpat-[A-Za-z0-9_-]{20,}",
            r"hf_[A-Za-z0-9]{30,}",
            r"pypi-AgEI[A-Za-z0-9_-]{20,}",
            r"SG\.[A-Za-z0-9_-]{20,}\.[A-Za-z0-9_-]{20,}",
            r"AGE-SECRET-KEY-1[A-Z0-9]{50,}",
        ]
        .iter()
        .map(|p| Regex::new(p).expect("valid secret regex"))
        .collect();
        // Quoted values (also JSON-escaped `\"…\"`) are one unit; otherwise up to whitespace.
        let value = r#"(?P<value>\\?"[^"\n]*"|\\?'[^'\n]*'|(?:(?:bearer|basic|token)\s+)?\S+)"#;
        Patterns {
            whole,
            unterminated_key: Regex::new(r"-----BEGIN [A-Z ]*PRIVATE KEY-----[\s\S]*")
                .expect("valid key regex"),
            url_userinfo: Regex::new(
                r"(?i)(?P<pre>[a-z][a-z0-9+.-]*://[^/\s:@]+:)(?P<value>[^@\s]+)@",
            )
            .expect("valid url regex"),
            key_value: Regex::new(&format!(
                r#"(?i)(?P<key>[\w.-]*{SECRET_WORD}[\w.-]*)(?P<sep>\\?["']?\s*[:=]\s*){value}"#
            ))
            .expect("valid key/value regex"),
            // Whole-word names only (`\b`): `compass: north` or `passing: 3` stay.
            name_value: Regex::new(&format!(
                r#"(?i)\b(?P<key>{SECRET_NAME})(?P<sep>\\?["']?\s*[:=]\s*){value}"#
            ))
            .expect("valid name/value regex"),
            // A cookie header's value runs to the end of the line (`a=1; b=2`).
            cookie: Regex::new(
                r#"(?im)\b(?P<key>(?:set-)?cookie)(?P<sep>\s*:\s*)(?P<value>[^\r\n"\\]+)"#,
            )
            .expect("valid cookie regex"),
            flag_value: Regex::new(&format!(
                r#"(?i)(?P<key>--(?:password|token|api-key))(?P<sep>\s+){value}"#
            ))
            .expect("valid flag regex"),
            secret_key: Regex::new(&format!("(?i){SECRET_WORD}")).expect("valid key regex"),
            secret_name: Regex::new(&format!("(?i)^(?:{SECRET_NAME})$")).expect("valid name regex"),
        }
    })
}

/// True for key names that contain a secret word but name something harmless
/// (`max_tokens`, `input_tokens`, `tokenizer`): those are counts/config, not credentials.
fn benign_key(key: &str) -> bool {
    let k = key.to_ascii_lowercase();
    let k = k.trim_matches(|c: char| c == '"' || c == '\'' || c == '\\');
    if k.ends_with("tokens") || k.contains("tokenizer") {
        return true;
    }
    k.strip_suffix("_key").is_some_and(|head| {
        let last = head.rsplit(['_', '.', '-']).next().unwrap_or(head);
        BENIGN_KEY_PREFIXES.contains(&last)
    })
}

/// Replaces the `value` group of every match with `[REDACTED]`, keeping quotes around
/// quoted values; leaves already-redacted values alone (idempotent).
fn redact_values(re: &Regex, text: &str) -> String {
    if !re.is_match(text) {
        return text.to_string();
    }
    re.replace_all(text, |c: &regex::Captures| {
        let whole = c.get(0).map_or("", |m| m.as_str()).to_string();
        if c.name("key").is_some_and(|k| benign_key(k.as_str())) {
            return whole;
        }
        let Some(value) = c.name("value") else {
            return whole;
        };
        let v = value.as_str();
        let (open, close): (&str, &str) =
            if v.len() >= 4 && v.starts_with("\\\"") && v.ends_with("\\\"") {
                ("\\\"", "\\\"")
            } else if v.len() >= 3 && v.starts_with("\\\"") && v.ends_with('"') {
                ("\\\"", "\"")
            } else if v.len() >= 2 && v.starts_with('"') && v.ends_with('"') {
                ("\"", "\"")
            } else if v.len() >= 2 && v.starts_with('\'') && v.ends_with('\'') {
                ("'", "'")
            } else {
                ("", "")
            };
        let inner = v
            .strip_prefix(open)
            .and_then(|x| x.strip_suffix(close))
            .unwrap_or(v);
        if inner == REDACTED {
            return whole;
        }
        let m = c.get(0).expect("whole match");
        let prefix = &text[m.start()..value.start()];
        let suffix = &text[value.end()..m.end()];
        format!("{prefix}{open}{REDACTED}{close}{suffix}")
    })
    .into_owned()
}

/// Redacts secrets in free text (tokens, private keys, URL passwords, `password=…`-style
/// values and `--password <v>` flags).
///
/// For `authorization: Bearer <token>` the scheme word is swallowed with the value so the
/// token itself does not survive; quoted values are redacted as one unit.
pub fn redact(text: &str) -> String {
    let p = patterns();
    let mut out = text.to_string();
    for re in &p.whole {
        if re.is_match(&out) {
            out = re.replace_all(&out, REDACTED).into_owned();
        }
    }
    if p.unterminated_key.is_match(&out) {
        out = p.unterminated_key.replace_all(&out, REDACTED).into_owned();
    }
    out = redact_values(&p.url_userinfo, &out);
    out = redact_values(&p.cookie, &out);
    out = redact_values(&p.key_value, &out);
    out = redact_values(&p.name_value, &out);
    out = redact_values(&p.flag_value, &out);
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
                    let v = if (p.secret_key.is_match(k) || p.secret_name.is_match(k))
                        && !benign_key(k)
                        && (v.is_string() || v.is_number())
                    {
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
    fn key_value_variants_from_review() {
        let cases = [
            (r#"{"password": "hunter2"}"#, "hunter2"),
            (r#""access_token": "ya29.a0AfH6SMBx""#, "ya29.a0AfH6SMBx"),
            (
                "AWS_SECRET_ACCESS_KEY=wJalrXUtnFEMI/K7MDENG",
                "wJalrXUtnFEMI",
            ),
            (
                "STRIPE_SECRET_KEY=sk_live_abcdef1234567",
                "sk_live_abcdef1234567",
            ),
            ("mysql --password hunter3 -u root", "hunter3"),
            ("cli --token 'abc def' x", "abc def"),
            ("cli --api-key=zzz999", "zzz999"),
            (r#"password = "correct horse battery staple""#, "horse"),
            (r#"echo "{\"password\": \"s3cr3t pw\"}""#, "s3cr3t"),
            ("db.credentials: topsecretvalue", "topsecretvalue"),
            ("private_key='-abc-'", "-abc-"),
        ];
        for (input, secret) in cases {
            redacted_all(input, secret);
            let once = redact(input);
            assert_eq!(redact(&once), once, "idempotent: {input}");
        }
        assert_eq!(
            redact(r#"{"password": "hunter2"}"#),
            r#"{"password": "[REDACTED]"}"#
        );
        assert_eq!(
            redact(r#"password = "correct horse battery staple" rest"#),
            r#"password = "[REDACTED]" rest"#
        );
        assert_eq!(redact("--password hunter3 -v"), "--password [REDACTED] -v");
    }

    #[test]
    fn new_token_patterns() {
        let cases = [
            format!("github_pat_{}", "A1b2_".repeat(10)),
            format!("ghs_{}", "a1B2".repeat(9)),
            format!("ghu_{}", "a1B2".repeat(9)),
            format!("ghr_{}", "a1B2".repeat(9)),
            "sk_test_abcdefghij12".to_string(),
            format!("AIza{}", "Sy0_-abcdefghijklmnopqrstuvwxyz0123"),
            "eyJhbGciOiJIUzI1NiJ9.eyJzdWIiOiIxMjM0NTY3ODkwIn0.dozjgNryP4J3jVmNHl0w5N_XgL0n3I9PlFUP0THsR8U"
                .to_string(),
        ];
        for t in &cases {
            redacted_all(&format!("value {t} end"), t);
        }
    }

    /// SPEC-M2.7 §9: the new token shapes.
    #[test]
    fn m27_token_patterns() {
        let cases = [
            format!("ASIA{}", "ABCDEFGHIJ234567"),
            format!("npm_{}", "a1B2c3".repeat(6)),
            format!("glpat-{}", "xY_z-12345".repeat(2)),
            format!("hf_{}", "abcDEF1234".repeat(3)),
            format!("pypi-AgEI{}", "cHlwaS5vcmc_-x".repeat(2)),
            format!("SG.{}.{}", "abcdefghij_-KLMNOPQRS", "tuvwxyz0123456789-_AB"),
            format!("AGE-SECRET-KEY-1{}", "QZ7J".repeat(14)),
        ];
        for t in &cases {
            redacted_all(&format!("値 {t} です"), t);
            let once = redact(t);
            assert_eq!(redact(&once), once, "idempotent: {t}");
        }
        assert_eq!(redact("ASIA-short"), "ASIA-short");
        assert_eq!(redact("hf_short"), "hf_short");
    }

    /// SPEC-M2.7 §9: short key names and `*_key` as whole words; cookies to end of line.
    #[test]
    fn m27_key_names_and_cookies() {
        for (input, secret) in [
            ("DB_PASS=hunter2", "hunter2"),
            ("pass: s3cr3t", "s3cr3t"),
            ("MYSQL_PWD=abc123", "abc123"),
            ("pwd = 'p w'", "p w"),
            ("passphrase: correct-horse", "correct-horse"),
            ("encryption_key=0123abcd", "0123abcd"),
            ("signing_key: zzz", "zzz"),
            ("MASTER_KEY=mk", "mk"),
            ("supabase_key: eyJx", "eyJx"),
            ("AccountKey=azure+key/==", "azure+key/=="),
            ("Cookie: session=abc; csrftoken=def", "session=abc"),
            ("Set-Cookie: sid=xyz; HttpOnly", "sid=xyz"),
        ] {
            redacted_all(input, secret);
            let once = redact(input);
            assert_eq!(redact(&once), once, "idempotent: {input}");
        }
        assert_eq!(
            redact("Cookie: a=1; b=2\n次の行"),
            "Cookie: [REDACTED]\n次の行"
        );
        // Words that merely contain the names, and identifier keys, survive.
        for s in [
            "compass: north",
            "passing: 3 tests",
            "bypass = true",
            "primary_key: id",
            "sort_key=created_at",
            "パスワードの設計",
            "the cookie banner",
        ] {
            assert_eq!(redact(s), s);
        }
        let out = redact_value(
            &json!({"pass": "x", "signing_key": "y", "sort_key": "z", "cookie": "c=1"}),
        );
        assert_eq!(out["pass"], REDACTED);
        assert_eq!(out["signing_key"], REDACTED);
        assert_eq!(out["sort_key"], "z");
        assert_eq!(out["cookie"], REDACTED);
    }

    #[test]
    fn url_userinfo_password_only() {
        assert_eq!(
            redact("psql postgres://app:pa55w0rd@db.local:5432/x"),
            "psql postgres://app:[REDACTED]@db.local:5432/x"
        );
        assert_eq!(
            redact("git clone https://user:tok3n@github.com/a/b"),
            "git clone https://user:[REDACTED]@github.com/a/b"
        );
        let url = "https://example.com:8080/path";
        assert_eq!(redact(url), url);
        let once = redact("redis://u:p@h");
        assert_eq!(redact(&once), once);
    }

    #[test]
    fn unterminated_private_key() {
        let out = redact("key:\n-----BEGIN OPENSSH PRIVATE KEY-----\nb3BlbnNzaC1rZXk\nAAAA");
        assert_eq!(out, format!("key:\n{REDACTED}"));
    }

    #[test]
    fn prose_and_harmless_keys_survive() {
        for s in [
            r#"git commit -m "fix token parsing""#,
            "the password reset flow is broken",
            "トークンの設計について",
            "file_path=/src/main.rs",
        ] {
            assert_eq!(redact(s), s);
        }
        let v = json!({
            "file_path": "/a/tokenizer.rs",
            "is_error": false,
            "command": "git commit -m \"fix token parsing\"",
            "max_tokens": 1024,
            "session_token": "abc",
            "X-Api-Key": "k",
            "AWS_SECRET_ACCESS_KEY": "v",
        });
        let out = redact_value(&v);
        assert_eq!(out["file_path"], "/a/tokenizer.rs");
        assert_eq!(out["is_error"], false);
        assert_eq!(out["command"], "git commit -m \"fix token parsing\"");
        assert_eq!(out["max_tokens"], 1024);
        assert_eq!(out["session_token"], REDACTED);
        assert_eq!(out["X-Api-Key"], REDACTED);
        assert_eq!(out["AWS_SECRET_ACCESS_KEY"], REDACTED);
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
