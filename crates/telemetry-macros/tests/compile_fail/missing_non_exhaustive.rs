//! A payload enum referenced from a payload struct must be `#[non_exhaustive]`.

fn main() {
    telemetry_macros::_audit_source!(r#"
        #[derive(serde::Serialize, serde::Deserialize)]
        pub enum Kind { A, B }

        #[derive(serde::Serialize, serde::Deserialize)]
        #[serde(deny_unknown_fields)]
        pub struct UsesKindPayload {
            pub kind: Kind,
        }
    "#);
}
