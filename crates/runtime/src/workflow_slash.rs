//! Native-shaped slash-command projection for validated workflow descriptors.
//!
//! The catalog is supplied by `tool-workflow`; this module only translates the
//! descriptor into the command API and builds the prompt for the exact request.

use std::sync::Arc;

use command_api::{
    BundledPromptContext, BundledPromptFn, CommandFrontmatter, CommandSource, NativeCommandType,
    NativeSlashKind, SlashCommand, SlashCommandKind,
};
use lingxi_core::types::utf16_json::Utf16JsonProjection;
use lingxi_core::types::SettingsScope;
use tool_workflow::{WorkflowCatalogSource, WorkflowSlashDescriptor};

const AUTHORING_SKILL_TAIL: &str = "\n\nIf the user asks you to modify this workflow or write a new script, load the `workflow-authoring` skill first.";

/// Convert the validated current workflow snapshot to prompt commands.
pub(crate) fn workflow_slash_commands(
    descriptors: &[WorkflowSlashDescriptor],
) -> Vec<SlashCommand> {
    descriptors.iter().map(workflow_slash_command).collect()
}

fn workflow_slash_command(descriptor: &WorkflowSlashDescriptor) -> SlashCommand {
    let source = command_source(&descriptor.source);
    let native_source = native_source(&descriptor.source);
    let loaded_from = match &descriptor.source {
        WorkflowCatalogSource::Builtin => "bundled",
        WorkflowCatalogSource::Plugin => "plugin",
        WorkflowCatalogSource::ProjectSettings | WorkflowCatalogSource::UserSettings => "skills",
    };
    let prompt_fn = Arc::new(WorkflowPromptFn::from_descriptor(descriptor));

    SlashCommand {
        name: descriptor.name.clone(),
        description: descriptor.description.clone(),
        source,
        kind: SlashCommandKind::Bundled {
            frontmatter: CommandFrontmatter::default(),
            prompt_fn: Some(prompt_fn),
        },
        native_type: Some(NativeCommandType::Prompt),
        disable_model_invocation: descriptor.disable_model_invocation,
        has_user_specified_description: true,
        loaded_from: Some(loaded_from.to_owned()),
        when_to_use: descriptor.when_to_use.clone(),
        plugin_name: descriptor.plugin_name.clone(),
        native_source,
        native_slash_kind: Some(NativeSlashKind::Workflow),
        content_length: Some(descriptor.content_length),
        progress_message: Some("running dynamic workflow".into()),
        ..SlashCommand::default()
    }
}

fn command_source(source: &WorkflowCatalogSource) -> CommandSource {
    match source {
        WorkflowCatalogSource::ProjectSettings => CommandSource::Settings(SettingsScope::Project),
        WorkflowCatalogSource::UserSettings => CommandSource::Settings(SettingsScope::User),
        WorkflowCatalogSource::Plugin => CommandSource::Plugin,
        WorkflowCatalogSource::Builtin => CommandSource::Bundled,
    }
}

fn native_source(source: &WorkflowCatalogSource) -> Option<Utf16JsonProjection> {
    let tag = match source {
        WorkflowCatalogSource::Builtin => source.host_native_source()?,
        WorkflowCatalogSource::ProjectSettings | WorkflowCatalogSource::UserSettings => {
            source.source_tag()
        }
        WorkflowCatalogSource::Plugin => "plugin",
    };
    Some(tag.into())
}

struct WorkflowPromptFn {
    descriptor: WorkflowSlashDescriptor,
}

impl WorkflowPromptFn {
    fn from_descriptor(descriptor: &WorkflowSlashDescriptor) -> Self {
        Self {
            descriptor: descriptor.clone(),
        }
    }
}

impl BundledPromptFn for WorkflowPromptFn {
    fn build(&self, args: &Utf16JsonProjection) -> Utf16JsonProjection {
        render_workflow_prompt(&self.descriptor, args, false)
    }

    fn try_build_at(
        &self,
        args: &Utf16JsonProjection,
        context: BundledPromptContext<'_>,
    ) -> std::io::Result<Utf16JsonProjection> {
        Ok(render_workflow_prompt(
            &self.descriptor,
            args,
            context.workflow_authoring_available(),
        ))
    }
}

fn render_workflow_prompt(
    descriptor: &WorkflowSlashDescriptor,
    args: &Utf16JsonProjection,
    workflow_authoring_available: bool,
) -> Utf16JsonProjection {
    let trimmed_args = trim_javascript_string(args);
    let name_json = descriptor
        .name
        .to_json_string()
        .expect("workflow names are root JSON strings");
    let args_json = trimmed_args
        .to_json_string()
        .expect("workflow arguments are root JSON strings");

    let mut units = Vec::new();
    append_text(&mut units, "Run the \"");
    append_projection(&mut units, &descriptor.name);
    append_text(&mut units, "\" workflow.\n\n");
    append_projection(&mut units, &descriptor.description);

    if descriptor
        .when_to_use
        .as_ref()
        .is_some_and(|value| !value.string_units("").unwrap_or_default().is_empty())
    {
        append_text(&mut units, "\n\n");
        append_projection(
            &mut units,
            descriptor.when_to_use.as_ref().expect("checked above"),
        );
    }

    if let Some(phases) = descriptor.phases.as_ref() {
        append_text(&mut units, "\n\nPhases:\n");
        for (index, phase) in phases.iter().enumerate() {
            if index != 0 {
                append_text(&mut units, "\n");
            }
            append_text(&mut units, "- ");
            append_projection(&mut units, &phase.title);
            if phase
                .detail
                .as_ref()
                .is_some_and(|value| !value.string_units("").unwrap_or_default().is_empty())
            {
                append_text(&mut units, ": ");
                append_projection(&mut units, phase.detail.as_ref().expect("checked above"));
            }
        }
    }

    append_text(&mut units, "\n\nInvoke: Workflow({ name: ");
    append_text(&mut units, &name_json);
    if trimmed_args
        .string_units("")
        .is_some_and(|value| !value.is_empty())
    {
        append_text(&mut units, ", args: ");
        append_text(&mut units, &args_json);
    }
    append_text(&mut units, " })");
    if workflow_authoring_available {
        append_text(&mut units, AUTHORING_SKILL_TAIL);
    }

    projection_from_units(units)
}

fn trim_javascript_string(value: &Utf16JsonProjection) -> Utf16JsonProjection {
    let units = value.string_units("").unwrap_or_default();
    let mut start = 0;
    let mut end = units.len();
    while start < end && is_javascript_whitespace(units[start]) {
        start += 1;
    }
    while end > start && is_javascript_whitespace(units[end - 1]) {
        end -= 1;
    }
    projection_from_units(units[start..end].to_vec())
}

fn is_javascript_whitespace(unit: u16) -> bool {
    char::from_u32(u32::from(unit)).is_some_and(lingxi_core::host::effort::javascript_whitespace)
}

fn append_text(units: &mut Vec<u16>, text: &str) {
    units.extend(text.encode_utf16());
}

fn append_projection(units: &mut Vec<u16>, source: &Utf16JsonProjection) {
    units.extend_from_slice(&source.string_units("").unwrap_or_default());
}

fn projection_from_units(units: Vec<u16>) -> Utf16JsonProjection {
    Utf16JsonProjection::root_string(String::from_utf16_lossy(&units), units)
        .expect("display text is derived from the same source UTF-16 units")
}

#[cfg(test)]
mod tests {
    use super::*;

    fn text(value: &str) -> Utf16JsonProjection {
        Utf16JsonProjection::root_string(value.to_owned(), value.encode_utf16().collect())
            .expect("test string is valid source text")
    }

    fn text_units(units: &[u16]) -> Utf16JsonProjection {
        projection_from_units(units.to_vec())
    }

    fn descriptor(source: WorkflowCatalogSource) -> WorkflowSlashDescriptor {
        WorkflowSlashDescriptor {
            name: text("review"),
            description: text("Review the change."),
            when_to_use: Some(text("Use for a focused review.")),
            phases: Some(vec![
                workflow::WorkflowMetaPhase {
                    title: text("Inspect"),
                    detail: Some(text("Find concrete issues.")),
                    model: None,
                },
                workflow::WorkflowMetaPhase {
                    title: text("Verify"),
                    detail: Some(text("Check each finding.")),
                    model: None,
                },
            ]),
            disable_model_invocation: false,
            content_length: 37,
            plugin_name: matches!(&source, WorkflowCatalogSource::Plugin).then(|| text("example")),
            source,
            script: "export const meta = {};".to_owned(),
        }
    }

    fn prompt_source(descriptor: &WorkflowSlashDescriptor) -> WorkflowSlashDescriptor {
        descriptor.clone()
    }

    fn context<'a>(
        workflow_authoring_base_allowed: bool,
        available_tool_names: Option<&'a [Utf16JsonProjection]>,
    ) -> BundledPromptContext<'a> {
        let root = std::path::Path::new("/project");
        BundledPromptContext {
            project_root: root,
            cwd: root,
            is_preload: false,
            main_loop_model: None,
            available_tool_names,
            workflow_authoring_base_allowed,
        }
    }

    #[test]
    fn maps_each_catalog_source_to_native_workflow_metadata() {
        let cases = [
            (
                WorkflowCatalogSource::Builtin,
                CommandSource::Bundled,
                "built-in",
                "bundled",
            ),
            (
                WorkflowCatalogSource::ProjectSettings,
                CommandSource::Settings(SettingsScope::Project),
                "projectSettings",
                "skills",
            ),
            (
                WorkflowCatalogSource::UserSettings,
                CommandSource::Settings(SettingsScope::User),
                "userSettings",
                "skills",
            ),
            (
                WorkflowCatalogSource::Plugin,
                CommandSource::Plugin,
                "plugin",
                "plugin",
            ),
        ];

        for (catalog_source, expected_source, expected_native_source, expected_loaded_from) in cases
        {
            let command = workflow_slash_commands(&[descriptor(catalog_source)])[0].clone();
            assert_eq!(command.source, expected_source);
            assert_eq!(
                command.native_source.as_ref().unwrap().string_at(""),
                Some(expected_native_source)
            );
            assert_eq!(command.loaded_from.as_deref(), Some(expected_loaded_from));
            assert_eq!(command.native_type, Some(NativeCommandType::Prompt));
            assert_eq!(command.native_slash_kind, Some(NativeSlashKind::Workflow));
            assert!(command.has_user_specified_description);
            assert!(!command.disable_model_invocation);
            assert_eq!(command.argument_hint, None);
            assert_eq!(command.content_length, Some(37));
            assert_eq!(
                command.progress_message.as_ref().unwrap().string_at(""),
                Some("running dynamic workflow")
            );
            assert_eq!(
                command.when_to_use.as_ref().unwrap().string_at(""),
                Some("Use for a focused review.")
            );
            assert!(matches!(command.kind, SlashCommandKind::Bundled { .. }));
        }
    }

    #[test]
    fn plugin_workflow_command_keeps_its_source_and_prompt_snapshot() {
        let mut workflow_descriptor = descriptor(WorkflowCatalogSource::Plugin);
        workflow_descriptor.phases.as_mut().unwrap()[0].model = Some(text("sonnet"));
        workflow_descriptor.script = "export const meta = { title: 'Review' };".to_owned();

        let command =
            workflow_slash_commands(std::slice::from_ref(&workflow_descriptor))[0].clone();
        assert_eq!(
            command.plugin_name.as_ref().unwrap().string_at(""),
            Some("example")
        );
        workflow_descriptor.name = text("mutated after registration");
        let SlashCommandKind::Bundled {
            prompt_fn: Some(prompt_fn),
            ..
        } = command.kind
        else {
            panic!("workflow command uses a dynamic bundled prompt");
        };
        let prompt = prompt_fn
            .try_build_at(&text(""), context(false, Some(&[])))
            .unwrap();
        assert_eq!(
            prompt.string_at(""),
            Some("Run the \"review\" workflow.\n\nReview the change.\n\nUse for a focused review.\n\nPhases:\n- Inspect: Find concrete issues.\n- Verify: Check each finding.\n\nInvoke: Workflow({ name: \"review\" })")
        );
    }

    #[test]
    fn prompt_renders_phases_args_and_authoring_tail_from_per_call_context() {
        let command =
            workflow_slash_commands(&[descriptor(WorkflowCatalogSource::Builtin)])[0].clone();
        let SlashCommandKind::Bundled {
            prompt_fn: Some(prompt_fn),
            ..
        } = command.kind
        else {
            panic!("workflow command uses a dynamic bundled prompt");
        };
        let args = text("  investigate this  ");
        let normal = prompt_fn
            .try_build_at(&args, context(false, Some(&[])))
            .unwrap();
        assert_eq!(
            normal.string_at(""),
            Some("Run the \"review\" workflow.\n\nReview the change.\n\nUse for a focused review.\n\nPhases:\n- Inspect: Find concrete issues.\n- Verify: Check each finding.\n\nInvoke: Workflow({ name: \"review\", args: \"investigate this\" })")
        );

        let available_tool = text("Skill");
        let authoring = prompt_fn
            .try_build_at(
                &args,
                context(true, Some(std::slice::from_ref(&available_tool))),
            )
            .unwrap();
        assert_eq!(
            authoring.string_at(""),
            Some("Run the \"review\" workflow.\n\nReview the change.\n\nUse for a focused review.\n\nPhases:\n- Inspect: Find concrete issues.\n- Verify: Check each finding.\n\nInvoke: Workflow({ name: \"review\", args: \"investigate this\" })\n\nIf the user asks you to modify this workflow or write a new script, load the `workflow-authoring` skill first.")
        );
    }

    #[test]
    fn empty_args_omit_the_args_property_and_empty_phases_still_render_header() {
        let mut descriptor = descriptor(WorkflowCatalogSource::UserSettings);
        descriptor.when_to_use = Some(text(""));
        descriptor.phases = Some(Vec::new());
        let prompt =
            render_workflow_prompt(&prompt_source(&descriptor), &text(" \t\u{feff}\n"), false);
        assert_eq!(
            prompt.string_at(""),
            Some("Run the \"review\" workflow.\n\nReview the change.\n\nPhases:\n\nInvoke: Workflow({ name: \"review\" })")
        );
        assert!(!prompt.string_at("").unwrap().contains("args:"));

        descriptor.phases = Some(vec![workflow::WorkflowMetaPhase {
            title: text("Only phase"),
            detail: Some(text("")),
            model: None,
        }]);
        let prompt = render_workflow_prompt(&prompt_source(&descriptor), &text(""), false);
        assert_eq!(
            prompt.string_at(""),
            Some("Run the \"review\" workflow.\n\nReview the change.\n\nPhases:\n- Only phase\n\nInvoke: Workflow({ name: \"review\" })")
        );
    }

    #[test]
    fn authoring_tail_distinguishes_omitted_tools_from_empty_tools() {
        let descriptor = descriptor(WorkflowCatalogSource::Builtin);
        let args = text("");

        let omitted_tools = context(true, None);
        assert!(omitted_tools.workflow_authoring_available());

        let no_tools: Vec<Utf16JsonProjection> = Vec::new();
        let empty_tools = context(true, Some(&no_tools));
        assert!(!empty_tools.workflow_authoring_available());

        let skill_tool = text("Skill");
        let skill_only = context(true, Some(std::slice::from_ref(&skill_tool)));
        assert!(skill_only.workflow_authoring_available());

        let blocked_policy = context(false, None);
        assert!(!blocked_policy.workflow_authoring_available());

        let rendered = render_workflow_prompt(
            &prompt_source(&descriptor),
            &args,
            skill_only.workflow_authoring_available(),
        );
        assert!(rendered
            .string_at("")
            .unwrap()
            .ends_with(AUTHORING_SKILL_TAIL));
    }

    #[test]
    fn prompt_and_json_arguments_retain_lone_surrogates_and_js_trim_boundaries() {
        let name_units = ["flow-".encode_utf16().collect::<Vec<_>>(), vec![0xd800]].concat();
        let description_units =
            ["summary-".encode_utf16().collect::<Vec<_>>(), vec![0xdfff]].concat();
        let when_units = ["when-".encode_utf16().collect::<Vec<_>>(), vec![0xdfff]].concat();
        let phase_title_units =
            ["stage-".encode_utf16().collect::<Vec<_>>(), vec![0xd800]].concat();
        let phase_detail_units =
            ["detail-".encode_utf16().collect::<Vec<_>>(), vec![0xdfff]].concat();
        let arg_units = [
            vec![0x0020, 0xfeff],
            "arg".encode_utf16().collect(),
            vec![0xd800, 0x0085, 0x0020],
        ]
        .concat();
        let mut descriptor = descriptor(WorkflowCatalogSource::Builtin);
        descriptor.name = text_units(&name_units);
        descriptor.description = text_units(&description_units);
        descriptor.when_to_use = Some(text_units(&when_units));
        descriptor.phases = Some(vec![workflow::WorkflowMetaPhase {
            title: text_units(&phase_title_units),
            detail: Some(text_units(&phase_detail_units)),
            model: None,
        }]);
        let args = text_units(&arg_units);
        let prompt = render_workflow_prompt(&prompt_source(&descriptor), &args, false);
        let units = prompt.string_units("").unwrap();

        let mut expected = "Run the \"".encode_utf16().collect::<Vec<_>>();
        expected.extend_from_slice(&name_units);
        expected.extend("\" workflow.\n\n".encode_utf16());
        expected.extend_from_slice(&description_units);
        expected.extend("\n\n".encode_utf16());
        expected.extend_from_slice(&when_units);
        expected.extend("\n\nPhases:\n- ".encode_utf16());
        expected.extend_from_slice(&phase_title_units);
        expected.extend(": ".encode_utf16());
        expected.extend_from_slice(&phase_detail_units);
        expected.extend("\n\nInvoke: Workflow({ name: ".encode_utf16());
        expected.extend(descriptor.name.to_json_string().unwrap().encode_utf16());
        expected.extend(", args: ".encode_utf16());
        expected.extend(
            [
                vec![0x0022],
                "arg".encode_utf16().collect(),
                "\\ud800".encode_utf16().collect(),
                vec![0x0085, 0x0022],
            ]
            .concat(),
        );
        expected.extend(" })".encode_utf16());
        assert_eq!(units, expected);
        assert_eq!(
            descriptor.name.to_json_string().unwrap(),
            r#""flow-\ud800""#
        );

        let nel_args = text_units(&[0x0085, b'x' as u16, 0x0085]);
        let trimmed = trim_javascript_string(&nel_args);
        assert_eq!(
            trimmed.string_units(""),
            Some(vec![0x0085, b'x' as u16, 0x0085])
        );
    }
}
