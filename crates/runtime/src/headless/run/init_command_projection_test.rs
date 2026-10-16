use super::init_command_projections;
use super::tests::fixture_runtime;
use command_api::model::{
    CommandFrontmatter, CommandSource, NativeCommandType, SlashCommand, SlashCommandKind,
};
use lingxi_core::host::OrchestratorHandle;
use lingxi_core::types::utf16_json::Utf16JsonProjection;
use lingxi_core::types::SettingsScope;
use std::path::PathBuf;
use std::sync::Arc;

fn prompt_skill(name: Utf16JsonProjection, disable_non_interactive: bool) -> SlashCommand {
    SlashCommand {
        name,
        description: "init projection fixture skill".into(),
        source: CommandSource::Settings(SettingsScope::Project),
        kind: SlashCommandKind::Markdown {
            file_path: PathBuf::from("/fixture/skills/init-projection/SKILL.md"),
            frontmatter: CommandFrontmatter::default(),
            prompt_template: "fixture prompt".into(),
        },
        native_type: Some(NativeCommandType::Prompt),
        disable_non_interactive,
        has_user_specified_description: true,
        loaded_from: Some("skills".into()),
        user_invocable: Some(true),
        ..SlashCommand::default()
    }
}

fn terminal_local(name: &str) -> SlashCommand {
    SlashCommand {
        name: name.into(),
        description: "init projection terminal fixture".into(),
        source: CommandSource::Settings(SettingsScope::Project),
        kind: SlashCommandKind::Builtin {
            handler_id: name.into(),
        },
        native_type: Some(NativeCommandType::Local),
        supports_non_interactive: true,
        terminal_oriented: true,
        user_invocable: Some(true),
        ..SlashCommand::default()
    }
}

fn projected_units(values: &[Utf16JsonProjection]) -> Vec<Vec<u16>> {
    values
        .iter()
        .map(|value| value.string_units("").expect("projected command name"))
        .collect()
}

fn units(value: &str) -> Vec<u16> {
    value.encode_utf16().collect()
}

fn isolated_surrogate_skill_name() -> Vec<u16> {
    "odd-"
        .encode_utf16()
        .chain([0xd800])
        .chain("-skill".encode_utf16())
        .collect()
}

fn starts_with_units(value: &[u16], prefix: &[u16]) -> bool {
    value.starts_with(prefix)
}

fn selected_in_order(values: &[Vec<u16>], selected: &[Vec<u16>]) -> Vec<Vec<u16>> {
    values
        .iter()
        .filter(|value| selected.contains(*value))
        .cloned()
        .collect()
}

#[tokio::test]
async fn assembled_init_projection_applies_off_and_preserves_native_pools_and_order() {
    let root = tempfile::tempdir().expect("isolated headless runtime root");
    let home = root.path().join("home");
    std::fs::create_dir_all(&home).expect("create isolated settings home");
    std::fs::write(
        home.join("settings.json"),
        r#"{"skillOverrides":{"model":"off","auto-mode-setup":"off","odd-\ud800-skill":"off"}}"#,
    )
    .expect("write the settings consumed by the assembled runtime");

    let stream = Arc::new(
        crate::headless::stream_json::StreamJsonStream::new_placeholder(
            crate::headless::io::Output::new(tokio::io::sink()),
        ),
    );
    let runtime = fixture_runtime(stream, root.path()).await;

    let registry = runtime.dispatcher.registry();
    {
        let mut registry = registry.write().await;
        registry.register_command(prompt_skill("init-projection-visible".into(), false));
        registry.register_command(prompt_skill("init-projection-pre-bge".into(), true));
        registry.register_command(prompt_skill(
            Utf16JsonProjection::parse(r#""odd-\ud800-skill""#)
                .expect("isolated surrogate prompt name"),
            false,
        ));
        let mut raw_visible = prompt_skill(
            Utf16JsonProjection::parse(r#""odd-\ud800-visible""#)
                .expect("visible isolated surrogate prompt name"),
            false,
        );
        raw_visible.description = Utf16JsonProjection::parse(r#""desc-\ud800""#).unwrap();
        raw_visible.argument_hint = Some(Utf16JsonProjection::parse(r#""\udfff""#).unwrap());
        registry.register_command(raw_visible);
        registry.register_command(prompt_skill("odd-�-skill".into(), false));
        registry.register_command(terminal_local("init-projection-terminal"));

        assert!(
            registry
                .headless_init_commands()
                .iter()
                .any(|command| command.name.string_at("") == Some("auto-mode-setup")),
            "the runtime admits auto-mode-setup before the init Off projection"
        );
    }

    let (slash_commands, terminal_slash_commands, skills) = init_command_projections(&runtime)
        .await
        .expect("load current command visibility settings");
    let slash_units = projected_units(&slash_commands);
    let terminal_units = projected_units(&terminal_slash_commands);
    let skill_units = projected_units(&skills);
    let visible_units = units("init-projection-visible");
    let pre_bge_units = units("init-projection-pre-bge");
    let raw_visible_units = "odd-"
        .encode_utf16()
        .chain([0xd800])
        .chain("-visible".encode_utf16())
        .collect::<Vec<_>>();
    let replacement_units = units("odd-�-skill");
    let terminal_name_units = units("init-projection-terminal");
    let prefix = units("init-projection-");
    let projected_names = |values: &[Vec<u16>]| {
        values
            .iter()
            .filter(|name| starts_with_units(name, &prefix))
            .cloned()
            .collect::<Vec<_>>()
    };

    assert!(
        slash_units.contains(&units("model")),
        "local /model ignores skillOverrides Off"
    );
    assert!(
        !slash_units.contains(&units("auto-mode-setup")),
        "the admitted local /auto-mode-setup is hidden by its exact Off setting"
    );
    assert!(
        !slash_units.contains(&isolated_surrogate_skill_name()),
        "the isolated-surrogate skill name is filtered by its exact UTF-16 key"
    );
    assert!(
        slash_units.contains(&replacement_units),
        "the replacement-character near miss remains visible"
    );
    assert!(
        slash_units.contains(&raw_visible_units),
        "visible command names retain their isolated UTF-16 code unit"
    );
    assert_eq!(
        selected_in_order(
            &slash_units,
            &[
                visible_units.clone(),
                raw_visible_units.clone(),
                replacement_units.clone(),
                terminal_name_units.clone(),
            ],
        ),
        vec![
            visible_units.clone(),
            raw_visible_units.clone(),
            replacement_units.clone(),
            terminal_name_units.clone(),
        ],
        "slash order preserves UTF-16 names and same-source registration order"
    );
    assert_eq!(
        projected_names(&terminal_units),
        vec![units("init-projection-terminal")],
        "terminal names remain the ordered subset of projected slash commands"
    );
    assert_eq!(
        selected_in_order(
            &skill_units,
            &[
                visible_units.clone(),
                pre_bge_units.clone(),
                raw_visible_units.clone(),
                replacement_units.clone(),
            ],
        ),
        vec![
            visible_units,
            pre_bge_units,
            raw_visible_units.clone(),
            replacement_units.clone(),
        ],
        "the skill projection uses the pre-bge prompt pool, then applies Off and userInvocable"
    );

    assert!(
        !skill_units.contains(&isolated_surrogate_skill_name()),
        "the hidden name is compared by source UTF-16 units"
    );
    assert!(
        skill_units.contains(&replacement_units),
        "the replacement-character skill is distinct from the isolated-surrogate key"
    );
    assert!(
        skill_units.contains(&raw_visible_units),
        "the pre-bge skill list preserves the raw UTF-16 spelling"
    );

    {
        let visibility = runtime.command_visibility_snapshot().unwrap();
        let registry = registry.read().await;
        let descriptors = super::initialize_commands::collect(&registry, &visibility);
        let raw = descriptors
            .iter()
            .find(|command| command.string_units("/name") == Some(raw_visible_units.clone()))
            .expect("control initialize retains the source command name");
        assert_eq!(
            raw.string_units("/description"),
            Some(
                "desc-"
                    .encode_utf16()
                    .chain([0xd800])
                    .chain(" (project)".encode_utf16())
                    .collect()
            )
        );
        assert_eq!(raw.string_units("/argumentHint"), Some(vec![0xdfff]));
        assert!(raw.at_pointer("/builtin").is_none());
        assert!(descriptors.iter().all(|command| {
            command.string_units("/name") != Some(isolated_surrogate_skill_name())
        }));
    }

    runtime
        .orchestrator
        .set_output_style("Concise")
        .await
        .unwrap();
    assert_eq!(
        super::init_output_style(&runtime.orchestrator)
            .await
            .string_at(""),
        Some("Concise"),
        "initialization reads the active style instead of a fixed default"
    );

    assert!(
        runtime
            .session_lifecycle
            .shutdown_and_drain()
            .await
            .complete
    );
}
