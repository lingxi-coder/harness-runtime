//! Regression tests for redaction test.

use std::collections::BTreeMap;

use llm_runtime::Redactor;

#[test]
fn redacts_secret_headers_case_insensitively() {
    let mut headers = BTreeMap::new();
    headers.insert(
        "Authorization".to_string(),
        "Bearer secret-token".to_string(),
    );
    headers.insert("x-api-key".to_string(), "secret-key".to_string());
    headers.insert("content-type".to_string(), "application/json".to_string());

    let redacted = Redactor.redact_headers(&headers);

    assert_eq!(
        redacted.get("Authorization"),
        Some(&"[REDACTED]".to_string())
    );
    assert_eq!(redacted.get("x-api-key"), Some(&"[REDACTED]".to_string()));
    assert_eq!(
        redacted.get("content-type"),
        Some(&"application/json".to_string())
    );
}

#[test]
fn redacts_secret_query_parameters() {
    let url = "https://example.com/v1/messages?api_key=secret&model=claude&access_token=token&signature=sig";

    let redacted = Redactor.redact_url(url);

    assert!(redacted.contains("api_key=[REDACTED]"));
    assert!(redacted.contains("model=claude"));
    assert!(redacted.contains("access_token=[REDACTED]"));
    assert!(redacted.contains("signature=[REDACTED]"));
    assert!(!redacted.contains("=secret"));
    assert!(!redacted.contains("=token"));
    assert!(!redacted.contains("=sig"));
}

#[test]
fn unparseable_urls_still_get_query_values_redacted() {
    let relative = "/v1/messages?api_key=secret&model=claude";

    let redacted = Redactor.redact_url(relative);

    assert!(redacted.contains("api_key=[REDACTED]"));
    assert!(redacted.contains("model=claude"));
    assert!(!redacted.contains("secret"));
}

#[test]
fn extended_secret_headers_and_query_keys_are_redacted() {
    let mut headers = BTreeMap::new();
    headers.insert(
        "Proxy-Authorization".to_string(),
        "Basic secret".to_string(),
    );
    headers.insert("Cookie".to_string(), "session=secret".to_string());
    headers.insert("Set-Cookie".to_string(), "session=secret".to_string());
    let redacted_headers = Redactor.redact_headers(&headers);
    assert_eq!(
        redacted_headers.get("Proxy-Authorization"),
        Some(&"[REDACTED]".to_string())
    );
    assert_eq!(
        redacted_headers.get("Cookie"),
        Some(&"[REDACTED]".to_string())
    );
    assert_eq!(
        redacted_headers.get("Set-Cookie"),
        Some(&"[REDACTED]".to_string())
    );

    let url =
        "https://example.com/blob?sig=sas-secret&client_secret=oauth-secret&token=plain-secret&x=1";
    let redacted_url = Redactor.redact_url(url);
    assert!(redacted_url.contains("sig=[REDACTED]"));
    assert!(redacted_url.contains("client_secret=[REDACTED]"));
    assert!(redacted_url.contains("token=[REDACTED]"));
    assert!(redacted_url.contains("x=1"));
    assert!(!redacted_url.contains("sas-secret"));
    assert!(!redacted_url.contains("oauth-secret"));
    assert!(!redacted_url.contains("plain-secret"));
}

#[test]
fn redacts_secret_json_fields_recursively() {
    let value = serde_json::json!({
        "api_key": "secret-key",
        "nested": {
            "access_token": "secret-token",
            "safe": "visible"
        },
        "items": [
            {"refresh_token": "refresh-secret"},
            {"name": "plain"}
        ]
    });

    let redacted = Redactor.redact_json(&value);

    assert_eq!(redacted["api_key"], "[REDACTED]");
    assert_eq!(redacted["nested"]["access_token"], "[REDACTED]");
    assert_eq!(redacted["nested"]["safe"], "visible");
    assert_eq!(redacted["items"][0]["refresh_token"], "[REDACTED]");
    assert_eq!(redacted["items"][1]["name"], "plain");
}
