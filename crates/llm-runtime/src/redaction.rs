//! Secret redaction utilities for diagnostics.

use std::collections::BTreeMap;
use std::sync::LazyLock;

use regex::Regex;
use serde_json::{Map, Value};
use url::form_urlencoded::byte_serialize;

const REDACTED: &str = "[REDACTED]";
// ECMAScript WhiteSpace + LineTerminator, used by the native diagnostic
// redactor. Unlike Rust's Unicode \s, it includes FEFF and excludes U+0085.
const ECMASCRIPT_WHITESPACE: &str = r"\t\n\x0b\x0c\r \u{00a0}\u{1680}\u{2000}-\u{200a}\u{2028}\u{2029}\u{202f}\u{205f}\u{3000}\u{feff}";

/// Redacts configured secret-bearing keys from diagnostic data.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct Redactor;

impl Redactor {
    /// Redact secret-bearing HTTP headers.
    #[must_use]
    pub fn redact_headers(&self, headers: &BTreeMap<String, String>) -> BTreeMap<String, String> {
        headers
            .iter()
            .map(|(name, value)| {
                let redacted = if is_secret_header(name) {
                    REDACTED.to_string()
                } else {
                    self.redact_error_text(value)
                };
                (name.clone(), redacted)
            })
            .collect()
    }

    /// Redact secret-bearing query parameters from a URL string.
    #[must_use]
    pub fn redact_url(&self, url: &str) -> String {
        let Ok(mut parsed) = url::Url::parse(url) else {
            // Relative or malformed URLs still get their query scrubbed —
            // returning the input unredacted would leak secrets.
            return redact_url_credentials(&redact_raw_query(url), REDACTED);
        };

        let pairs: Vec<(String, String)> = parsed
            .query_pairs()
            .map(|(key, value)| {
                let value = if is_secret_key(&key) {
                    REDACTED.to_string()
                } else {
                    value.into_owned()
                };
                (key.into_owned(), value)
            })
            .collect();

        let fragment = parsed.fragment().map(ToOwned::to_owned);
        parsed.set_query(None);
        parsed.set_fragment(None);

        let mut redacted = parsed.to_string();
        if !pairs.is_empty() {
            redacted.push('?');
            redacted.push_str(&encode_pairs(&pairs));
        }
        if let Some(fragment) = fragment {
            redacted.push('#');
            redacted.push_str(&fragment);
        }
        redact_url_credentials(&redacted, REDACTED)
    }

    /// Redact secret-bearing fields from arbitrary JSON.
    #[must_use]
    pub fn redact_json(&self, value: &Value) -> Value {
        redact_json_value(value)
    }

    /// Redact credentials in a diagnostic error message.
    ///
    /// This follows Claude Code 2.1.286's error-message redactor (`ud`), whose
    /// output differs from its transcript redactor (`Rs`): URL credentials use
    /// `***:***`, and JWTs and provider tokens have distinct markers. Bearer
    /// values include percent-encoded bytes all the way to the next whitespace.
    /// Use [`Self::redact_json`] before serializing structured records; applying
    /// a text redactor to serialized JSON can consume its closing quotes.
    #[must_use]
    pub fn redact_error_text(&self, text: &str) -> String {
        let mut output = redact_url_credentials(text, "***:***");
        for (pattern, replacement) in ERROR_PATTERNS.iter() {
            output = pattern.replace_all(&output, *replacement).into_owned();
        }
        output
    }
}

fn redact_json_value(value: &Value) -> Value {
    match value {
        Value::Object(map) => Value::Object(redact_json_object(map)),
        Value::Array(items) => Value::Array(items.iter().map(redact_json_value).collect()),
        Value::String(text) => Value::String(Redactor.redact_error_text(text)),
        _ => value.clone(),
    }
}

fn redact_json_object(map: &Map<String, Value>) -> Map<String, Value> {
    map.iter()
        .map(|(key, value)| {
            let redacted = if is_secret_key(key) {
                Value::String(REDACTED.to_string())
            } else {
                redact_json_value(value)
            };
            (key.clone(), redacted)
        })
        .collect()
}

fn redact_raw_query(url: &str) -> String {
    let Some((base, rest)) = url.split_once('?') else {
        return url.to_string();
    };
    let (query, fragment) = match rest.split_once('#') {
        Some((query, fragment)) => (query, Some(fragment)),
        None => (rest, None),
    };

    let pairs: Vec<(String, String)> = url::form_urlencoded::parse(query.as_bytes())
        .map(|(key, value)| {
            let value = if is_secret_key(&key) {
                REDACTED.to_string()
            } else {
                value.into_owned()
            };
            (key.into_owned(), value)
        })
        .collect();

    let mut redacted = format!("{base}?{}", encode_pairs(&pairs));
    if let Some(fragment) = fragment {
        redacted.push('#');
        redacted.push_str(fragment);
    }
    redacted
}

fn encode_pairs(pairs: &[(String, String)]) -> String {
    pairs
        .iter()
        .map(|(key, value)| {
            let key = percent_encode(key);
            let value = if value == REDACTED {
                REDACTED.to_string()
            } else {
                percent_encode(value)
            };
            format!("{key}={value}")
        })
        .collect::<Vec<_>>()
        .join("&")
}

fn percent_encode(value: &str) -> String {
    byte_serialize(value.as_bytes()).collect()
}

fn is_secret_header(name: &str) -> bool {
    is_secret_key(name)
        || matches!(
            normalized_key(name).as_str(),
            "authorization"
                | "proxy-authorization"
                | "x-api-key"
                | "api-key"
                | "x-goog-api-key"
                | "x-auth-token"
                | "x-amz-security-token"
                | "cookie"
                | "set-cookie"
        )
}

fn is_secret_key(name: &str) -> bool {
    let name = normalized_key(name);
    SENSITIVE_KEY.is_match(&name)
        || matches!(
            name.as_str(),
            "api_key"
                | "apikey"
                | "key"
                | "access_token"
                | "refresh_token"
                | "id_token"
                | "token"
                | "authorization"
                | "assertion"
                | "client_secret"
                | "secret_access_key"
                | "password"
                | "signature"
                | "sig"
        )
}

// These names follow the structured diagnostic redactor (K6t / j3). The
// explicit names in is_secret_key retain the runtime's existing query policy.
static SENSITIVE_KEY: LazyLock<Regex> = LazyLock::new(|| {
    Regex::new(concat!(
        r"(?i)api[_-]?key|secret|token|password|passwd|credential|bearer|",
        r"authorization|auth[_-]?header|cookie|session[_-]?(?:id|key)|",
        r"connection[_-]?string|(?:private|ssh|encryption|signing|access|",
        r"deploy|master|license)[_-]?key|client[_-]?secret"
    ))
    .expect("valid diagnostic secret-key pattern")
});

// Format controls and default-ignorable Unicode characters can occur inside a
// key (api<ZWSP>_key), not only around it. Strip them only for classification so
// the original field name and the shape of structured data remain unchanged.
static INVISIBLE: LazyLock<Regex> = LazyLock::new(|| {
    Regex::new(r"[\p{Cf}\p{Default_Ignorable_Code_Point}\u{2800}]")
        .expect("valid invisible-character pattern")
});

fn normalized_key(name: &str) -> String {
    INVISIBLE.replace_all(name, "").to_ascii_lowercase()
}

fn error_keyword(word: &str) -> String {
    word.chars()
        .map(|character| regex::escape(&character.to_string()))
        .collect::<Vec<_>>()
        .join(r"[\p{Cf}\p{Default_Ignorable_Code_Point}\u{2800}]*")
}

static ERROR_PATTERNS: LazyLock<Vec<(Regex, &'static str)>> = LazyLock::new(|| {
    let keywords = ["secret", "key", "token", "password", "credential"]
        .map(error_keyword)
        .join("|");
    let invisible = r"[\p{Cf}\p{Default_Ignorable_Code_Point}\u{2800}]*";
    let spacing = r"[\s\p{Cf}\p{Default_Ignorable_Code_Point}\u{2800}]*";
    let value_spacing =
        format!(r"[{ECMASCRIPT_WHITESPACE}\p{{Cf}}\p{{Default_Ignorable_Code_Point}}\u{{2800}}]*");
    let bearer = error_keyword("Bearer");
    let authorization = error_keyword("Authorization");
    let basic = error_keyword("Basic");
    // ECMAScript \S includes NEXT LINE (U+0085), unlike Rust's Unicode \S.
    // Treat it as part of secret values at every error-text entrypoint so a
    // diagnostic cannot expose the suffix of a credential containing it.
    // FEFF triggers ud's second, invisible-aware pass, whose token class DOES
    // include FEFF. Thus `Bearer one<FEFF>two` must mask both parts even though
    // the first pass's ECMAScript \S would stop at FEFF.
    let token = r"[\S\u{0085}]+";
    // Consume whole quoted or scheme-prefixed values. Upstream ud stops at
    // their first whitespace and can expose the remainder (api_key=Bearer s).
    // U+0085 is a value byte, so it must not survive in a preserved separator.
    // The optional prefix keeps our stronger quoted/scheme masking when that
    // value starts with U+0085 (the native token pass alone is insufficient).
    let value = format!(
        r#"[\u{{0085}}\p{{Cf}}\p{{Default_Ignorable_Code_Point}}\u{{2800}}]*(?:"(?:[^"\\]|\\.)*"|'(?:[^'\\]|\\.)*'|(?:{bearer}|{basic}){invisible}[{ECMASCRIPT_WHITESPACE}\u{{0085}}]+{token}|{token})"#
    );
    let patterns = [
        (
            format!(
                r"(?i)((?:^|[\s=:])[^=:\s]*?(?:{keywords})[^=:\s]*\s*[=:]{value_spacing}){value}"
            ),
            "${1}[REDACTED]",
        ),
        (r"sk-ant-[A-Za-z0-9_.-]+".to_string(), REDACTED),
        (
            format!(r"(?i)({bearer}{invisible} {invisible}){token}"),
            "${1}[REDACTED]",
        ),
        (
            r"eyJ[A-Za-z0-9_-]{8,}\.eyJ[A-Za-z0-9_-]{8,}\.[A-Za-z0-9_-]{8,}".to_string(),
            "[REDACTED-JWT]",
        ),
        (
            concat!(
                r"gh[pousr]_[A-Za-z0-9]{20,}|github_pat_[A-Za-z0-9_]{82,}|",
                r"gl(?:pat|dt|rt|ft|soat|oas|agent|ptt|cbt|imt|ffct)-",
                r"[A-Za-z0-9_=-]{20,}(?:\.[0-9a-z]{9})?|",
                r"xox[a-z]-[A-Za-z0-9+/=%_-]{10,}|xapp-[A-Za-z0-9_-]{10,}|",
                r"xwfp-[A-Za-z0-9_-]{10,}|[Hh][Oo][Oo][Kk][Ss]\.",
                r"[Ss][Ll][Aa][Cc][Kk]\.[Cc][Oo][Mm]/(?:services|workflows|triggers)/",
                r"[A-Za-z0-9+/_-]{20,}|sq0(?:atp|csp)-[A-Za-z0-9_-]{22,}|",
                r"EAAA[A-Za-z0-9+/=%_-]{56,}|AIza[A-Za-z0-9_-]{35}|",
                r"GOCSPX-[A-Za-z0-9_-]{28}|[sr]k_(?:live|test|prod)_[A-Za-z0-9]{24,}"
            )
            .to_string(),
            "[REDACTED-PAT]",
        ),
        (
            format!(
                // Keep accepting a U+0085-only Basic separator as the runtime
                // already did, while ECMAScript whitespace handles FEFF and
                // leaves leading U+0085 secret bytes inside the masked value.
                r"(?i)({authorization}{invisible}:{spacing}{basic}{invisible}(?:[{ECMASCRIPT_WHITESPACE}]+|\u{{0085}}+){value_spacing}){token}"
            ),
            "${1}[REDACTED]",
        ),
    ];
    patterns
        .into_iter()
        .map(|(pattern, replacement)| {
            (
                Regex::new(&pattern).expect("valid diagnostic redaction pattern"),
                replacement,
            )
        })
        .collect()
});

static URL_SCHEME: LazyLock<Regex> = LazyLock::new(|| {
    Regex::new(r"(?i)\b[a-z][a-z0-9+.-]{0,31}://").expect("valid URL-scheme pattern")
});

static URL_TOKEN_WHITESPACE: LazyLock<Regex> = LazyLock::new(|| {
    Regex::new(&format!("[{ECMASCRIPT_WHITESPACE}]")).expect("valid ECMAScript whitespace pattern")
});

/// Mask the original authority span, before URL parsing can normalize it.
/// The final raw or percent-encoded @ ends the credentials, including embedded
/// @ and punctuation in passwords. A bracketed SSH host also bounds malformed
/// userinfo containing slashes, which the URL parser otherwise rejects.
fn redact_url_credentials(text: &str, marker: &str) -> String {
    let mut output = String::with_capacity(text.len());
    let mut copied = 0;
    for scheme in URL_SCHEME.find_iter(text) {
        if scheme.start() < copied {
            continue;
        }
        let start = scheme.end();
        let tail = &text[start..];
        let token_end = URL_TOKEN_WHITESPACE
            .find(tail)
            .map_or(tail.len(), |delimiter| delimiter.start());
        let token = &tail[..token_end];
        let mut authority_end = token.find('/').unwrap_or(token.len());
        let protocol = scheme.as_str().to_ascii_lowercase();
        let bracketed_host = matches!(
            protocol.as_str(),
            "ssh://" | "git://" | "git+ssh://" | "ssh+git://"
        )
        .then(|| BRACKET_HOST_START.find_iter(token).last())
        .flatten()
        .and_then(|host_start| {
            BRACKET_HOST_END
                .find(&token[host_start.end()..])
                .map(|host_end| host_start.end() + host_end.end())
        });
        if let Some(host_end) = bracketed_host {
            authority_end = host_end;
        }
        let authority = &token[..authority_end];
        let separator = authority
            .match_indices('@')
            .map(|(index, _)| (index, 1))
            .chain(authority.match_indices("%40").map(|(index, _)| (index, 3)))
            .max_by_key(|(index, _)| *index);
        if let Some((separator, width)) = separator {
            output.push_str(&text[copied..start]);
            output.push_str(marker);
            output.push_str(&authority[separator..separator + width]);
            copied = start + separator + width;
        }
    }
    output.push_str(&text[copied..]);
    output
}

static BRACKET_HOST_START: LazyLock<Regex> =
    LazyLock::new(|| Regex::new(r"(?i)(?:@|%40)(?:\[|%5b)").expect("valid bracketed host prefix"));

static BRACKET_HOST_END: LazyLock<Regex> =
    LazyLock::new(|| Regex::new(r"(?i)\]|%5d").expect("valid bracketed host suffix"));
