//! A payload struct without `#[serde(deny_unknown_fields)]` must be rejected.

fn main() {
    telemetry_macros::_audit_source!(r#"
        #[derive(serde::Serialize, serde::Deserialize)]
        pub struct UngatedPayload {
            pub model: Verified,
        }
    "#);
}
