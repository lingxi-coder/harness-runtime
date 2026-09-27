//! Regression guard for the product decision that removed the fixed local-app
//! template catalog. The conversational brief/questionnaire/plan flow is the
//! only protocol surface; reintroducing one of these symbols requires an
//! explicit protocol redesign instead of an accidental DTO/command addition.

const PROTOCOL_SOURCES: &[(&str, &str)] = &[
    ("local_apps.rs", include_str!("../src/local_apps.rs")),
    ("commands.rs", include_str!("../src/commands.rs")),
    ("events.rs", include_str!("../src/events.rs")),
];

#[test]
fn fixed_local_app_template_catalog_stays_removed() {
    for (file, source) in PROTOCOL_SOURCES {
        for removed in [
            "AppTemplateKind",
            "AppTemplateDto",
            "ListAppTemplates",
            "AppTemplatesChanged",
        ] {
            assert!(
                !source.contains(removed),
                "{removed} was reintroduced in {file}; local apps use the dynamic brief/questionnaire/plan flow"
            );
        }
    }
}
