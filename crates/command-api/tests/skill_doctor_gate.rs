//! Exercise the live command catalog gate in an isolated test process.

use command_api::{
    CommandRegistry, CommandSource, NativeCommandType, SlashCommand, SlashCommandKind,
};

#[test]
fn skill_doctor_accepts_the_current_flag_or_product_override() {
    let previous = std::env::var_os(branding::ENABLE_SKILL_DOCTOR_ENV);
    std::env::remove_var(branding::ENABLE_SKILL_DOCTOR_ENV);
    let registry = CommandRegistry::new();
    let command = SlashCommand {
        name: "skill-doctor".into(),
        source: CommandSource::Builtin,
        kind: SlashCommandKind::Builtin {
            handler_id: "skill-doctor".into(),
        },
        native_type: Some(NativeCommandType::Local),
        supports_non_interactive: true,
        is_enabled: true,
        ..SlashCommand::default()
    };

    telemetry::test_set_flag("tengu_lantern_prism", false);
    assert!(!command_api::builtin_support::skill_doctor_enabled());
    assert!(!registry.headless_command_admitted(&command));

    telemetry::test_set_flag("tengu_lantern_prism", true);
    assert!(command_api::builtin_support::skill_doctor_enabled());
    assert!(registry.headless_command_admitted(&command));

    telemetry::test_set_flag("tengu_lantern_prism", false);
    for (value, expected) in [
        ("true", true),
        (" YES ", true),
        ("0", false),
        ("false", false),
    ] {
        std::env::set_var(branding::ENABLE_SKILL_DOCTOR_ENV, value);
        assert_eq!(
            command_api::builtin_support::skill_doctor_enabled(),
            expected,
            "{value}"
        );
        assert_eq!(
            registry.headless_command_admitted(&command),
            expected,
            "{value}"
        );
    }
    telemetry::test_clear_flag("tengu_lantern_prism");
    match previous {
        Some(value) => std::env::set_var(branding::ENABLE_SKILL_DOCTOR_ENV, value),
        None => std::env::remove_var(branding::ENABLE_SKILL_DOCTOR_ENV),
    }
}
