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

// Expected outputs were evaluated against ud from the official 2.1.286
// binary (SHA-256 75e3016e9d2570767b08e43a7467d4817a4f149232c169ca295f2c95fef21433),
// extracted chunk src_179601033.js. This is the error-text surface; the
// separate Rs transcript redactor has different output markers and limits.
#[test]
fn error_text_matches_2_1_286_oracle_bytes() {
    let cases = [
        (
            "Authorization: Bearer secret",
            "Authorization: Bearer [REDACTED]",
        ),
        (
            "Authorization: Basic c2VjcmV0",
            "Authorization: Basic [REDACTED]",
        ),
        ("Bearer token=secret", "Bearer [REDACTED]"),
        ("Basic api_key=secret", "Basic api_key=[REDACTED]"),
        (
            "Authorization: Bearer token=secret",
            "Authorization: Bearer [REDACTED]",
        ),
        (
            "Authorization: Basic api_key=secret",
            "Authorization: Basic [REDACTED]",
        ),
        (
            "Bearer abcdefghijklmnop123456%20secret",
            "Bearer [REDACTED]",
        ),
        ("api\u{200b}_key=secret", "api\u{200b}_key=[REDACTED]"),
        ("api_key\u{feff}=secret", "api_key\u{feff}=[REDACTED]"),
        ("B\u{200b}earer secret", "B\u{200b}earer [REDACTED]"),
        (
            "Authorization: B\u{200b}asic secret",
            "Authorization: B\u{200b}asic [REDACTED]",
        ),
        (
            "https://user:p@ss@host.example/path",
            "https://***:***@host.example/path",
        ),
        (
            "https://user:p%40ss%40host.example/path",
            "https://***:***%40host.example/path",
        ),
        ("https://user:p;ss@[::1]/path", "https://***:***@[::1]/path"),
        ("https://user:p]ss@[::1]/path", "https://***:***@[::1]/path"),
        ("token=secret safe=visible", "token=[REDACTED] safe=visible"),
        ("safe=api_key=secret", "safe=api_key=[REDACTED]"),
        ("sk-ant-api03-test", "[REDACTED]"),
        ("eyJabcdefgh.eyJijklmnop.qrstuvwx", "[REDACTED-JWT]"),
        ("ghp_12345678901234567890", "[REDACTED-PAT]"),
        ("glrt-12345678901234567890.abcdef123", "[REDACTED-PAT]"),
        ("safe text user@example.com", "safe text user@example.com"),
    ];
    for (input, expected) in cases {
        assert_eq!(Redactor.redact_error_text(input), expected, "{input:?}");
    }
}

#[test]
fn error_text_next_line_boundaries_match_2_1_286_oracle_bytes() {
    // Evaluated against the same native ud oracle as above. ECMAScript \S
    // includes U+0085; replacing it with Rust's \S leaked each "two" suffix.
    let cases = [
        ("Bearer one\u{0085}two", "Bearer [REDACTED]"),
        ("Bearer \u{0085}secret", "Bearer [REDACTED]"),
        ("password=one\u{0085}two", "password=[REDACTED]"),
        (
            "token=one\u{0085}two safe=visible",
            "token=[REDACTED] safe=visible",
        ),
        (
            "Authorization: Basic one\u{0085}two",
            "Authorization: Basic [REDACTED]",
        ),
        ("B\u{200b}earer one\u{0085}two", "B\u{200b}earer [REDACTED]"),
        (
            "Authorization: B\u{200b}asic one\u{0085}two",
            "Authorization: B\u{200b}asic [REDACTED]",
        ),
        ("password=one\u{feff}two", "password=[REDACTED]"),
        ("Bearer one\u{2028}two", "Bearer [REDACTED]\u{2028}two"),
    ];
    for (input, expected) in cases {
        assert_eq!(Redactor.redact_error_text(input), expected, "{input:?}");
        assert_eq!(
            Redactor.redact_json(&serde_json::json!({"message":input}))["message"],
            expected
        );
        assert_eq!(
            Redactor.redact_headers(&BTreeMap::from([("x-diagnostic".into(), input.into())]))
                ["x-diagnostic"],
            expected
        );
    }
    // Preserve our existing stronger treatment of scheme-prefixed key values;
    // ud masks only "Bearer"/"Basic" here, while the runtime masks the value.
    for input in [
        "password=Bearer one\u{0085}two",
        "password=Basic one\u{0085}two",
    ] {
        assert_eq!(Redactor.redact_error_text(input), "password=[REDACTED]");
    }
}

#[test]
fn error_text_whitespace_fixture_preserves_native_and_documented_stronger_outputs() {
    let fixture: serde_json::Value = serde_json::from_str(include_str!(
        "fixtures/claude_2_1_286_redaction_whitespace.json"
    ))
    .unwrap();
    assert_eq!(fixture["version"], "2.1.286");
    assert_eq!(
        fixture["sha256"],
        "75e3016e9d2570767b08e43a7467d4817a4f149232c169ca295f2c95fef21433"
    );
    let mut exact = 0;
    let mut strengthened = 0;
    for case in fixture["cases"].as_array().unwrap() {
        let input = case["input"].as_str().unwrap();
        let expected = case["rustExpected"].as_str().unwrap();
        match case["alignment"].as_str().unwrap() {
            "exact" => {
                exact += 1;
                assert_eq!(case["rustExpected"], case["nativeExpected"]);
            }
            "strengthened" => {
                strengthened += 1;
                assert_ne!(case["rustExpected"], case["nativeExpected"]);
                assert!(!case["reason"].as_str().unwrap().is_empty());
            }
            alignment => panic!("unknown alignment {alignment}"),
        }
        assert_eq!(
            Redactor.redact_error_text(input),
            expected,
            "oracle case {} ({})",
            case["id"],
            case["alignment"]
        );
        assert_eq!(
            Redactor.redact_json(&serde_json::json!({"message":input}))["message"],
            expected,
            "structured diagnostic value: {}",
            case["id"]
        );
    }
    assert_eq!(fixture["exactCases"].as_u64().unwrap(), exact);
    assert_eq!(fixture["strengthenedCases"].as_u64().unwrap(), strengthened);
    for id in [
        "U+FEFF/bearer_value",
        "U+FEFF/basic_value",
        "U+FEFF/key_value",
    ] {
        let case = fixture["cases"]
            .as_array()
            .unwrap()
            .iter()
            .find(|case| case["id"] == id)
            .unwrap();
        assert_eq!(case["invisiblePass"], true);
        assert_ne!(
            case["firstPassExpected"], case["nativeExpected"],
            "FEFF proof requires the actual invisible second pass"
        );
        assert!(!case["nativeExpected"].as_str().unwrap().contains("two"));
    }
}

#[test]
fn security_strengthening_does_not_reproduce_upstream_partial_masking() {
    // ud leaves " secret" after the marker in the first two cases and
    // " world\"" in the third. Rs has its own quoted-value and encoded-Bearer
    // gaps. Exact parity never requires disclosing the remainder of a secret.
    for input in [
        "api_key=Bearer secret",
        "api_key=Basic secret",
        "api_key=\"hello world\"",
    ] {
        assert_eq!(Redactor.redact_error_text(input), "api_key=[REDACTED]");
    }
    assert_eq!(
        Redactor.redact_error_text(r#"password="say \"hello\" then goodbye" safe=visible"#),
        "password=[REDACTED] safe=visible"
    );
    // ud leaves slash-bearing SSH userinfo unchanged. Rs masks this case.
    assert_eq!(
        Redactor.redact_error_text("ssh://user:p/ss@[::1]/path"),
        "ssh://***:***@[::1]/path"
    );
    assert_eq!(
        Redactor.redact_error_text("ssh://user:p/ss%40%5B::1%5D/path"),
        "ssh://***:***%40%5B::1%5D/path"
    );
}

#[test]
fn structured_secret_key_names_match_2_1_286_j3_and_k6t_oracles() {
    // j3 / K6t, src_176142273.js; values deliberately contain no incidental
    // token patterns so this isolates the key-name classification surface.
    let value = serde_json::json!({
        "api\u{200b}_key": "secret",
        "Auth-Header": "secret",
        "connection_string": "secret",
        "sessionId": "secret",
        "private-key": "secret",
        "safe": "visible"
    });
    let expected = serde_json::json!({
        "api\u{200b}_key": "[REDACTED]",
        "Auth-Header": "[REDACTED]",
        "connection_string": "[REDACTED]",
        "sessionId": "[REDACTED]",
        "private-key": "[REDACTED]",
        "safe": "visible"
    });
    assert_eq!(Redactor.redact_json(&value), expected);
    let headers = value
        .as_object()
        .unwrap()
        .iter()
        .map(|(key, value)| (key.clone(), value.as_str().unwrap().to_string()))
        .collect();
    let expected_headers = expected
        .as_object()
        .unwrap()
        .iter()
        .map(|(key, value)| (key.clone(), value.as_str().unwrap().to_string()))
        .collect::<BTreeMap<_, _>>();
    assert_eq!(Redactor.redact_headers(&headers), expected_headers);
}

#[test]
fn invisible_controls_inside_secret_names_do_not_leak() {
    for invisible in [
        '\u{200b}',
        '\u{2060}',
        '\u{feff}',
        '\u{034f}',
        '\u{2800}',
        '\u{e0020}',
    ] {
        let key = format!("pass{invisible}word");
        let value = serde_json::json!({ &key: "hidden-secret" });
        assert_eq!(Redactor.redact_json(&value)[&key], "[REDACTED]");
        assert_eq!(
            Redactor.redact_error_text(&format!("{key}=hidden-secret")),
            format!("{key}=[REDACTED]")
        );
    }
    assert_eq!(
        Redactor.redact_url("/path?api%E2%80%8B_key=hidden-secret&safe=visible"),
        "/path?api%E2%80%8B_key=[REDACTED]&safe=visible"
    );
    assert_eq!(
        Redactor.redact_error_text("Authorization:\u{200b}Basic hidden-secret"),
        "Authorization:\u{200b}Basic [REDACTED]"
    );
    assert_eq!(
        Redactor.redact_error_text("api_key=\u{200b}Bearer hidden-secret"),
        "api_key=\u{200b}[REDACTED]"
    );
}

#[test]
fn url_credentials_are_scrubbed_before_returning_diagnostics() {
    for input in [
        "https://user:secret@host.example/path",
        "https://user:p@ss@host.example/path",
        "https://user:p;ss@host.example/path",
        "https://user:p]ss@host.example/path",
    ] {
        assert_eq!(
            Redactor.redact_url(input),
            "https://[REDACTED]@host.example/path"
        );
    }
    assert_eq!(
        Redactor.redact_url("https://user:p%40ss%40host.example/path?token=hidden"),
        "https://[REDACTED]%40host.example/path?token=[REDACTED]"
    );
    assert_eq!(
        Redactor.redact_url("https://user:p;ss@[::1]/path?token=hidden&safe=visible#fragment"),
        "https://[REDACTED]@[::1]/path?token=[REDACTED]&safe=visible#fragment"
    );
    assert_eq!(
        Redactor.redact_url("ssh://user:p/ss@[::1]/path"),
        "ssh://[REDACTED]@[::1]/path"
    );
}

#[test]
fn redact_json_before_serialization_preserves_valid_json_lines() {
    let records = [
        serde_json::json!({
            "type": "error",
            "message": "Authorization: Bearer abc%20def",
            "password": "a\"b\\c\nsecond line",
            "items": ["Bearer token=secret", {"safe": "visible"}],
            "count": 2,
            "enabled": true,
            "empty": null,
        }),
        serde_json::json!({"message": "Basic api_key=secret", "sessionId": "quoted\"id"}),
    ];
    let jsonl = records
        .iter()
        .map(|record| serde_json::to_string(&Redactor.redact_json(record)).unwrap())
        .collect::<Vec<_>>()
        .join("\n");
    let parsed = jsonl
        .lines()
        .map(|line| serde_json::from_str::<serde_json::Value>(line).unwrap())
        .collect::<Vec<_>>();
    assert_eq!(parsed.len(), 2);
    assert_eq!(parsed[0]["message"], "Authorization: Bearer [REDACTED]");
    assert_eq!(parsed[0]["password"], "[REDACTED]");
    assert_eq!(parsed[0]["items"][0], "Bearer [REDACTED]");
    assert_eq!(parsed[0]["items"][1]["safe"], "visible");
    assert_eq!(parsed[0]["count"], 2);
    assert_eq!(parsed[0]["enabled"], true);
    assert!(parsed[0]["empty"].is_null());
    assert_eq!(parsed[1]["sessionId"], "[REDACTED]");
    assert!(!jsonl.contains("secret"));
    assert!(!jsonl.contains("abc%20def"));
}
