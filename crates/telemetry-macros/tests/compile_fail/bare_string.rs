//! Bare `String` in a payload struct must be rejected by the audit.

fn main() {
    telemetry_macros::_audit_source!(r#"
        #[derive(serde::Serialize, serde::Deserialize)]
        #[serde(deny_unknown_fields)]
        pub struct BadPayload {
            pub model: String,
        }
    "#);
}
