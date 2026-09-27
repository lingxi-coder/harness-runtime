//! A well-formed payload must compile cleanly.

fn main() {
    telemetry_macros::_audit_source!(r#"
        #[derive(serde::Serialize, serde::Deserialize)]
        #[serde(deny_unknown_fields)]
        pub struct GoodPayload {
            pub model: Verified,
            pub input_tokens: u64,
            pub is_stream: bool,
        }
    "#);
}
