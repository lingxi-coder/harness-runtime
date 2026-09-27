//! Secret redaction utilities for diagnostics.

use std::collections::BTreeMap;

use serde_json::{Map, Value};
use url::form_urlencoded::byte_serialize;

const REDACTED: &str = "[REDACTED]";

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
                    value.clone()
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
            return redact_raw_query(url);
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
        redacted
    }

    /// Redact secret-bearing fields from arbitrary JSON.
    #[must_use]
    pub fn redact_json(&self, value: &Value) -> Value {
        redact_json_value(value)
    }
}

fn redact_json_value(value: &Value) -> Value {
    match value {
        Value::Object(map) => Value::Object(redact_json_object(map)),
        Value::Array(items) => Value::Array(items.iter().map(redact_json_value).collect()),
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
    matches!(
        name.to_ascii_lowercase().as_str(),
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
    matches!(
        name.to_ascii_lowercase().as_str(),
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
