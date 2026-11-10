//! Source-preserving OAuth schemas from the hash-pinned 2.1.322 SDK.
//!
//! Validation is independent of the selected model/provider. The original JSON
//! source and the native schema's normalized projection are retained separately.
use lingxi_core::types::utf16_json::{JsonValueView, Utf16JsonProjection};

#[derive(Debug, Clone)]
pub(crate) struct ValidatedOAuthJson {
    source: String,
    projection: Utf16JsonProjection,
}

impl ValidatedOAuthJson {
    pub(crate) fn source(&self) -> &str {
        &self.source
    }
    pub(crate) fn projection(&self) -> &Utf16JsonProjection {
        &self.projection
    }
    pub(crate) fn into_projection(self) -> Utf16JsonProjection {
        self.projection
    }
    pub(crate) fn json(&self) -> Result<String, String> {
        self.projection
            .to_json_string()
            .map_err(|error| error.to_string())
    }
}

#[derive(Clone, Copy)]
enum Kind {
    String,
    NonemptyString,
    Url,
    SafeUrl,
    EmptyOrSafeUrl,
    Strings,
    SafeUrls,
    Boolean,
    CatchBoolean,
    Number,
    CoercedNumber,
    Any,
    IdJagLiteral,
    DefaultBearer,
}

#[derive(Clone, Copy)]
struct Field {
    name: &'static str,
    kind: Kind,
    required: bool,
}
const fn req(name: &'static str, kind: Kind) -> Field {
    Field {
        name,
        kind,
        required: true,
    }
}
const fn opt(name: &'static str, kind: Kind) -> Field {
    Field {
        name,
        kind,
        required: false,
    }
}

const PRM: &[Field] = &[
    req("resource", Kind::Url),
    opt("authorization_servers", Kind::SafeUrls),
    opt("jwks_uri", Kind::Url),
    opt("scopes_supported", Kind::Strings),
    opt("bearer_methods_supported", Kind::Strings),
    opt("resource_signing_alg_values_supported", Kind::Strings),
    opt("resource_name", Kind::String),
    opt("resource_documentation", Kind::String),
    opt("resource_policy_uri", Kind::Url),
    opt("resource_tos_uri", Kind::Url),
    opt("tls_client_certificate_bound_access_tokens", Kind::Boolean),
    opt("authorization_details_types_supported", Kind::Strings),
    opt("dpop_signing_alg_values_supported", Kind::Strings),
    opt("dpop_bound_access_tokens_required", Kind::Boolean),
];
const AUTHORIZATION: &[Field] = &[
    req("issuer", Kind::String),
    req("authorization_endpoint", Kind::SafeUrl),
    req("token_endpoint", Kind::SafeUrl),
    opt("registration_endpoint", Kind::SafeUrl),
    opt("scopes_supported", Kind::Strings),
    req("response_types_supported", Kind::Strings),
    opt("response_modes_supported", Kind::Strings),
    opt("grant_types_supported", Kind::Strings),
    opt("token_endpoint_auth_methods_supported", Kind::Strings),
    opt(
        "token_endpoint_auth_signing_alg_values_supported",
        Kind::Strings,
    ),
    opt("service_documentation", Kind::SafeUrl),
    opt("revocation_endpoint", Kind::SafeUrl),
    opt("revocation_endpoint_auth_methods_supported", Kind::Strings),
    opt(
        "revocation_endpoint_auth_signing_alg_values_supported",
        Kind::Strings,
    ),
    opt("introspection_endpoint", Kind::String),
    opt(
        "introspection_endpoint_auth_methods_supported",
        Kind::Strings,
    ),
    opt(
        "introspection_endpoint_auth_signing_alg_values_supported",
        Kind::Strings,
    ),
    opt("code_challenge_methods_supported", Kind::Strings),
    opt("client_id_metadata_document_supported", Kind::Boolean),
    opt(
        "authorization_response_iss_parameter_supported",
        Kind::CatchBoolean,
    ),
];
const OIDC: &[Field] = &[
    req("issuer", Kind::String),
    req("authorization_endpoint", Kind::SafeUrl),
    req("token_endpoint", Kind::SafeUrl),
    opt("userinfo_endpoint", Kind::SafeUrl),
    req("jwks_uri", Kind::SafeUrl),
    opt("registration_endpoint", Kind::SafeUrl),
    opt("scopes_supported", Kind::Strings),
    req("response_types_supported", Kind::Strings),
    opt("response_modes_supported", Kind::Strings),
    opt("grant_types_supported", Kind::Strings),
    opt("acr_values_supported", Kind::Strings),
    req("subject_types_supported", Kind::Strings),
    req("id_token_signing_alg_values_supported", Kind::Strings),
    opt("id_token_encryption_alg_values_supported", Kind::Strings),
    opt("id_token_encryption_enc_values_supported", Kind::Strings),
    opt("userinfo_signing_alg_values_supported", Kind::Strings),
    opt("userinfo_encryption_alg_values_supported", Kind::Strings),
    opt("userinfo_encryption_enc_values_supported", Kind::Strings),
    opt("request_object_signing_alg_values_supported", Kind::Strings),
    opt(
        "request_object_encryption_alg_values_supported",
        Kind::Strings,
    ),
    opt(
        "request_object_encryption_enc_values_supported",
        Kind::Strings,
    ),
    opt("token_endpoint_auth_methods_supported", Kind::Strings),
    opt(
        "token_endpoint_auth_signing_alg_values_supported",
        Kind::Strings,
    ),
    opt("display_values_supported", Kind::Strings),
    opt("claim_types_supported", Kind::Strings),
    opt("claims_supported", Kind::Strings),
    opt("service_documentation", Kind::String),
    opt("claims_locales_supported", Kind::Strings),
    opt("ui_locales_supported", Kind::Strings),
    opt("claims_parameter_supported", Kind::Boolean),
    opt("request_parameter_supported", Kind::Boolean),
    opt("request_uri_parameter_supported", Kind::Boolean),
    opt("require_request_uri_registration", Kind::Boolean),
    opt("op_policy_uri", Kind::SafeUrl),
    opt("op_tos_uri", Kind::SafeUrl),
    opt("client_id_metadata_document_supported", Kind::Boolean),
    opt(
        "authorization_response_iss_parameter_supported",
        Kind::CatchBoolean,
    ),
    opt("code_challenge_methods_supported", Kind::Strings),
];
const TOKENS: &[Field] = &[
    req("access_token", Kind::String),
    opt("id_token", Kind::String),
    req("token_type", Kind::String),
    opt("expires_in", Kind::CoercedNumber),
    opt("scope", Kind::String),
    opt("refresh_token", Kind::String),
];
const CLIENT_METADATA: &[Field] = &[
    req("redirect_uris", Kind::SafeUrls),
    opt("token_endpoint_auth_method", Kind::String),
    opt("grant_types", Kind::Strings),
    opt("response_types", Kind::Strings),
    opt("application_type", Kind::String),
    opt("client_name", Kind::String),
    opt("client_uri", Kind::SafeUrl),
    opt("logo_uri", Kind::EmptyOrSafeUrl),
    opt("scope", Kind::String),
    opt("contacts", Kind::Strings),
    opt("tos_uri", Kind::EmptyOrSafeUrl),
    opt("policy_uri", Kind::String),
    opt("jwks_uri", Kind::SafeUrl),
    opt("jwks", Kind::Any),
    opt("software_id", Kind::String),
    opt("software_version", Kind::String),
    opt("software_statement", Kind::String),
];
const CLIENT_INFORMATION: &[Field] = &[
    req("client_id", Kind::String),
    opt("client_secret", Kind::String),
    opt("client_id_issued_at", Kind::Number),
    opt("client_secret_expires_at", Kind::Number),
];
const OAUTH_ERROR: &[Field] = &[
    req("error", Kind::String),
    opt("error_description", Kind::String),
    opt("error_uri", Kind::String),
];
const REGISTRATION_ERROR: &[Field] = &[
    req("error", Kind::String),
    opt("error_description", Kind::String),
];
const REVOCATION: &[Field] = &[
    req("token", Kind::String),
    opt("token_type_hint", Kind::String),
];
const ID_JAG: &[Field] = &[
    req("issued_token_type", Kind::IdJagLiteral),
    req("access_token", Kind::String),
    opt("token_type", Kind::String),
    opt("expires_in", Kind::Number),
    opt("scope", Kind::String),
];
const XAA_EXCHANGE: &[Field] = &[
    opt("access_token", Kind::String),
    opt("issued_token_type", Kind::String),
    opt("expires_in", Kind::CoercedNumber),
    opt("scope", Kind::String),
];
const XAA_TOKENS: &[Field] = &[
    req("access_token", Kind::NonemptyString),
    opt("token_type", Kind::DefaultBearer),
    opt("expires_in", Kind::CoercedNumber),
    opt("scope", Kind::String),
    opt("refresh_token", Kind::String),
];

pub(crate) fn validate_prm(raw: &str) -> Result<ValidatedOAuthJson, String> {
    validate(raw, PRM, true)
}
pub(crate) fn validate_authorization(
    raw: &str,
    is_oidc: bool,
) -> Result<ValidatedOAuthJson, String> {
    validate(raw, if is_oidc { OIDC } else { AUTHORIZATION }, !is_oidc)
}
pub(crate) fn validate_client_metadata(raw: &str) -> Result<ValidatedOAuthJson, String> {
    validate(raw, CLIENT_METADATA, false)
}
pub(crate) fn validate_client_information(raw: &str) -> Result<ValidatedOAuthJson, String> {
    validate(raw, CLIENT_INFORMATION, false)
}
pub(crate) fn validate_registration(raw: &str) -> Result<ValidatedOAuthJson, String> {
    let fields = CLIENT_METADATA
        .iter()
        .chain(CLIENT_INFORMATION)
        .copied()
        .collect::<Vec<_>>();
    validate(raw, &fields, false)
}
pub(crate) fn validate_tokens(raw: &str) -> Result<ValidatedOAuthJson, String> {
    validate(raw, TOKENS, false)
}
pub(crate) fn validate_oauth_error(raw: &str) -> Result<ValidatedOAuthJson, String> {
    validate(raw, OAUTH_ERROR, false)
}
pub(crate) fn validate_registration_error(raw: &str) -> Result<ValidatedOAuthJson, String> {
    validate(raw, REGISTRATION_ERROR, false)
}
pub(crate) fn validate_revocation(raw: &str) -> Result<ValidatedOAuthJson, String> {
    validate(raw, REVOCATION, false)
}
pub(crate) fn validate_id_jag(raw: &str) -> Result<ValidatedOAuthJson, String> {
    validate(raw, ID_JAG, false)
}
pub(crate) fn validate_xaa_exchange(raw: &str) -> Result<ValidatedOAuthJson, String> {
    validate(raw, XAA_EXCHANGE, false)
}
pub(crate) fn validate_xaa_tokens(raw: &str) -> Result<ValidatedOAuthJson, String> {
    validate(raw, XAA_TOKENS, false)
}

fn validate(raw: &str, fields: &[Field], passthrough: bool) -> Result<ValidatedOAuthJson, String> {
    let input = Utf16JsonProjection::parse(raw).map_err(|error| error.to_string())?;
    let root = input.at_pointer("").expect("parsed JSON root");
    let entries = input.object_entries();
    let mut issues = Vec::new();
    if entries.is_none() {
        issues.push(type_issue("object", Some(root), &[]));
    } else {
        for field in fields {
            let value = input.get_property(field.name);
            if value.is_some() || field.required {
                collect_issues(
                    value,
                    field.kind,
                    &[serde_json::json!(field.name)],
                    &mut issues,
                );
            }
        }
    }
    if !issues.is_empty() {
        return Err(serde_json::to_string_pretty(&issues).expect("serializable schema issues"));
    }
    let entries = entries.expect("validated object");
    let mut output = Vec::new();
    for field in fields {
        let value = input.get_property(field.name);
        if value.is_none() {
            if matches!(field.kind, Kind::DefaultBearer) {
                output.push((
                    field.name.encode_utf16().collect(),
                    Utf16JsonProjection::from("Bearer"),
                ));
            } else if field.required {
                return Err(format!("OAuth schema is missing {}", field.name));
            }
            continue;
        }
        if let Some(value) = validate_value(value.unwrap(), field.kind)
            .map_err(|error| format!("{}: {error}", field.name))?
        {
            output.push((field.name.encode_utf16().collect(), value));
        }
    }
    if passthrough {
        for entry in entries {
            if fields.iter().any(|field| {
                field
                    .name
                    .encode_utf16()
                    .eq(entry.code_units.iter().copied())
            }) || entry
                .code_units
                .iter()
                .copied()
                .eq("__proto__".encode_utf16())
            {
                continue;
            }
            output.push((
                entry.code_units.to_vec(),
                entry
                    .value
                    .to_projection()
                    .map_err(|error| error.to_string())?,
            ));
        }
    }
    let projection =
        Utf16JsonProjection::object_units(output).map_err(|error| error.to_string())?;
    Ok(ValidatedOAuthJson {
        source: raw.to_owned(),
        projection,
    })
}

fn validate_value(
    value: JsonValueView<'_>,
    kind: Kind,
) -> Result<Option<Utf16JsonProjection>, String> {
    match kind {
        Kind::String | Kind::NonemptyString | Kind::DefaultBearer => {
            let units = value
                .code_units()
                .ok_or_else(|| "expected string".to_owned())?;
            if matches!(kind, Kind::NonemptyString) && units.is_empty() {
                return Err("expected nonempty string".into());
            }
        }
        Kind::Url | Kind::SafeUrl | Kind::EmptyOrSafeUrl => {
            let units = value
                .code_units()
                .ok_or_else(|| "expected URL string".to_owned())?;
            if matches!(kind, Kind::EmptyOrSafeUrl) && units.is_empty() {
                return Ok(None);
            }
            let text = String::from_utf16_lossy(units);
            let url = url::Url::parse(&text).map_err(|_| "invalid URL".to_owned())?;
            if !matches!(kind, Kind::Url)
                && matches!(url.scheme(), "javascript" | "data" | "vbscript")
            {
                return Err("URL cannot use javascript:, data:, or vbscript: scheme".into());
            }
        }
        Kind::Strings | Kind::SafeUrls => {
            let length = value
                .array_len()
                .ok_or_else(|| "expected string array".to_owned())?;
            for index in 0..length {
                validate_value(
                    value
                        .get_index(index)
                        .ok_or_else(|| "missing array item".to_owned())?,
                    if matches!(kind, Kind::SafeUrls) {
                        Kind::SafeUrl
                    } else {
                        Kind::String
                    },
                )?;
            }
        }
        Kind::Boolean | Kind::CatchBoolean => {
            if value.as_bool().is_none() {
                if matches!(kind, Kind::CatchBoolean) {
                    return Ok(None);
                } else {
                    return Err("expected boolean".into());
                }
            }
        }
        Kind::Number => {
            if !value.as_number().is_some_and(f64::is_finite) {
                return Err("expected finite number".into());
            }
        }
        Kind::CoercedNumber => {
            return Ok(Some(Utf16JsonProjection::plain(serde_json::json!(
                finite_js_number(value)?
            ))));
        }
        Kind::IdJagLiteral => {
            if value.as_str() != Some("urn:ietf:params:oauth:token-type:id-jag") {
                return Err("invalid issued_token_type".into());
            }
        }
        Kind::Any => {}
    }
    value
        .to_projection()
        .map(Some)
        .map_err(|error| error.to_string())
}

fn received_type(value: Option<JsonValueView<'_>>) -> &'static str {
    let Some(value) = value else {
        return "undefined";
    };
    if value.is_null() {
        "null"
    } else if value.as_str().is_some() {
        "string"
    } else if value.as_number().is_some() {
        "number"
    } else if value.as_bool().is_some() {
        "boolean"
    } else if value.array_len().is_some() {
        "array"
    } else {
        "Object"
    }
}

fn type_issue(
    expected: &str,
    value: Option<JsonValueView<'_>>,
    path: &[serde_json::Value],
) -> serde_json::Value {
    let received = received_type(value);
    serde_json::json!({"expected":expected,"code":"invalid_type","path":path,"message":format!("Invalid input: expected {expected}, received {received}")})
}

fn number_issue(number: f64, path: &[serde_json::Value]) -> serde_json::Value {
    let received = if number.is_nan() { "NaN" } else { "Infinity" };
    serde_json::json!({"expected":"number","code":"invalid_type","received":received,"path":path,"message":format!("Invalid input: expected number, received {received}")})
}

fn collect_issues(
    value: Option<JsonValueView<'_>>,
    kind: Kind,
    path: &[serde_json::Value],
    issues: &mut Vec<serde_json::Value>,
) {
    use serde_json::json;
    if matches!(kind, Kind::Any | Kind::CatchBoolean) {
        return;
    }
    if matches!(kind, Kind::IdJagLiteral) {
        if value.and_then(JsonValueView::as_str) != Some("urn:ietf:params:oauth:token-type:id-jag")
        {
            issues.push(json!({"code":"invalid_value","values":["urn:ietf:params:oauth:token-type:id-jag"],"path":path,"message":"Invalid input: expected \"urn:ietf:params:oauth:token-type:id-jag\""}));
        }
        return;
    }
    let expected = match kind {
        Kind::Strings | Kind::SafeUrls => "array",
        Kind::Boolean => "boolean",
        Kind::Number | Kind::CoercedNumber => "number",
        _ => "string",
    };
    if matches!(kind, Kind::CoercedNumber) {
        let number = value.map(coerced_js_number).unwrap_or(f64::NAN);
        if !number.is_finite() {
            issues.push(number_issue(number, path));
        }
        return;
    }
    if matches!(kind, Kind::EmptyOrSafeUrl) {
        if value.and_then(JsonValueView::as_str) == Some("") {
            return;
        }
        let mut first = Vec::new();
        collect_issues(value, Kind::SafeUrl, &[], &mut first);
        if first.is_empty() {
            return;
        }
        let aborts = first
            .iter()
            .any(|issue| issue["fatal"].as_bool() == Some(true) || issue["code"] == "invalid_type");
        if aborts {
            issues.push(json!({"code":"invalid_union","errors":[first,[{"code":"invalid_value","values":[""],"path":[],"message":"Invalid input: expected \"\""}]],"path":path,"message":"Invalid input"}));
        } else {
            for mut issue in first {
                issue["path"] = json!(path);
                issues.push(issue);
            }
        }
        return;
    }
    let valid_type = value.is_some_and(|value| match kind {
        Kind::Strings | Kind::SafeUrls => value.array_len().is_some(),
        Kind::Boolean => value.as_bool().is_some(),
        Kind::Number => value.as_number().is_some(),
        _ => value.code_units().is_some(),
    });
    if !valid_type {
        issues.push(type_issue(expected, value, path));
        return;
    }
    let value = value.expect("validated present field");
    match kind {
        Kind::Strings|Kind::SafeUrls=>{
            for index in 0..value.array_len().unwrap() {
                let mut item_path=path.to_vec();item_path.push(json!(index));
                collect_issues(value.get_index(index),if matches!(kind,Kind::SafeUrls){Kind::SafeUrl}else{Kind::String},&item_path,issues);
            }
        }
        Kind::Url|Kind::SafeUrl=>{
            let text=String::from_utf16_lossy(value.code_units().unwrap());
            match url::Url::parse(&text) {
                Err(_)=>{
                    issues.push(json!({"code":"invalid_format","format":"url","path":path,"message":"Invalid URL"}));
                    if matches!(kind,Kind::SafeUrl) { issues.push(json!({"code":"custom","message":"URL must be parseable","fatal":true,"path":path})); }
                }
                Ok(url) if matches!(kind,Kind::SafeUrl)&&matches!(url.scheme(),"javascript"|"data"|"vbscript")=>issues.push(json!({"code":"custom","path":path,"message":"URL cannot use javascript:, data:, or vbscript: scheme"})),
                Ok(_)=>{}
            }
        }
        Kind::Number=>{let number=value.as_number().unwrap();if !number.is_finite(){issues.push(number_issue(number,path));}}
        Kind::NonemptyString if value.code_units().unwrap().is_empty()=>issues.push(json!({"origin":"string","code":"too_small","minimum":1,"inclusive":true,"path":path,"message":"Too small: expected string to have >=1 characters"})),
        _=>{}
    }
}

/// The bundled Vqe is a coerced finite JS Number. Strings and arrays follow
/// JSON.parse/Number semantics; failures do not replace the source value.
pub(crate) fn finite_js_number(value: JsonValueView<'_>) -> Result<f64, String> {
    let number = coerced_js_number(value);
    number
        .is_finite()
        .then_some(number)
        .ok_or_else(|| "expected finite number".to_owned())
}

fn coerced_js_number(mut value: JsonValueView<'_>) -> f64 {
    let mut in_array = false;
    while let Some(length) = value.array_len() {
        in_array = true;
        match length {
            0 => return 0.0,
            1 => value = value.get_index(0).expect("JSON array element"),
            _ => return f64::NAN,
        }
    }
    if let Some(number) = value.as_number() {
        number
    } else if value.is_null() {
        0.0
    } else if let Some(boolean) = value.as_bool() {
        if in_array {
            f64::NAN
        } else if boolean {
            1.0
        } else {
            0.0
        }
    } else if let Some(string) = value.as_str() {
        js_number_string(string)
    } else {
        f64::NAN
    }
}

fn js_number_string(value: &str) -> f64 {
    let value = value.trim_matches(|character: char| {
        (character.is_whitespace() && character != '\u{0085}') || character == '\u{FEFF}'
    });
    if value.is_empty() {
        return 0.0;
    }
    for (prefixes, radix) in [(["0x", "0X"], 16), (["0b", "0B"], 2), (["0o", "0O"], 8)] {
        if let Some(digits) = prefixes
            .iter()
            .find_map(|prefix| value.strip_prefix(prefix))
        {
            if digits.is_empty() {
                return f64::NAN;
            }
            let mut result = 0.0;
            for character in digits.chars() {
                let Some(digit) = character.to_digit(radix) else {
                    return f64::NAN;
                };
                result = result * f64::from(radix) + f64::from(digit);
            }
            return result;
        }
    }
    if matches!(value, "Infinity" | "+Infinity") {
        return f64::INFINITY;
    }
    if value == "-Infinity" {
        return f64::NEG_INFINITY;
    }
    static DECIMAL: std::sync::OnceLock<regex::Regex> = std::sync::OnceLock::new();
    let decimal = DECIMAL.get_or_init(|| {
        regex::Regex::new(r"^[+-]?(?:[0-9]+\.?[0-9]*|\.[0-9]+)(?:[eE][+-]?[0-9]+)?$")
            .expect("fixed JS numeric grammar")
    });
    if decimal.is_match(value) {
        value.parse::<f64>().unwrap_or(f64::NAN)
    } else {
        f64::NAN
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn native_322_real_schema_fixture_acceptance_and_normalized_output() {
        let fixture: serde_json::Value = serde_json::from_str(include_str!(
            "../../tests/fixtures/mcp_oauth_schemas_2_1_322.json"
        ))
        .unwrap();
        for case in fixture["cases"].as_array().unwrap() {
            let raw = case["input"].as_str().unwrap();
            let result = match case["schema"].as_str().unwrap() {
                "prm" => validate_prm(raw),
                "authorization" => validate_authorization(raw, false),
                "oidc" => validate_authorization(raw, true),
                "tokens" => validate_tokens(raw),
                "clientMetadata" => validate_client_metadata(raw),
                "clientInformation" => validate_client_information(raw),
                "registration" => validate_registration(raw),
                "error" => validate_oauth_error(raw),
                "registrationError" => validate_registration_error(raw),
                "revocation" => validate_revocation(raw),
                "idJag" => validate_id_jag(raw),
                _ => panic!("unknown schema fixture"),
            };
            assert_eq!(
                result.is_ok(),
                case["success"].as_bool().unwrap(),
                "{} {}: {:?}",
                case["schema"],
                case["name"],
                result
            );
            if let Err(error) = &result {
                if let Some(expected) = case.get("issues") {
                    let issues: serde_json::Value = serde_json::from_str(error)
                        .unwrap_or_else(|_| panic!("non-native schema issue text: {error}"));
                    assert_eq!(&issues, expected, "{} {}", case["schema"], case["name"]);
                }
            }
            if let Ok(validated) = result {
                assert_eq!(validated.source(), raw);
                assert_eq!(
                    validated.json().unwrap(),
                    case["output"].as_str().unwrap(),
                    "{} {}",
                    case["schema"],
                    case["name"]
                );
                assert_eq!(validated.projection(), &validated.clone().into_projection());
            }
        }
    }
}
