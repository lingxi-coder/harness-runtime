//! Shared `/media` workflow for the host media tools on desktop and mobile.

use std::path::PathBuf;
use std::sync::Arc;

use command_api::{BundledPromptFn, CommandRegistry, CommandSource, SlashCommandKind};
use lingxi_core::types::utf16_json::Utf16JsonProjection;
use tool_api::{tool_trait::ToolStaticContext, ToolRegistry};

const LOADED_FROM: &str = "cross-platform-media-skill";

struct MediaPrompt {
    body: String,
    fallback_tool_names: Vec<String>,
    live_tools: Option<Arc<ToolRegistry>>,
}

impl BundledPromptFn for MediaPrompt {
    fn build(&self, args: &Utf16JsonProjection) -> Utf16JsonProjection {
        let available_tool_names = self.live_tools.as_ref().map_or_else(
            || self.fallback_tool_names.clone(),
            |tools| {
                tools
                    .available_tools(&ToolStaticContext::default())
                    .into_iter()
                    .map(|tool| tool.name().to_owned())
                    .collect()
            },
        );
        let body = supported_sections(&self.body, &available_tool_names);
        let args = args
            .string_units("")
            .expect("media skill arguments are text");
        if args
            .iter()
            .all(|unit| char::from_u32(u32::from(*unit)).is_some_and(char::is_whitespace))
        {
            return body.into();
        }
        let mut units = body.encode_utf16().collect::<Vec<_>>();
        units.extend("\n\n## User Request\n\n".encode_utf16());
        units.extend(args);
        source_from_units(units)
    }
}

fn source_from_units(units: Vec<u16>) -> Utf16JsonProjection {
    Utf16JsonProjection::root_string(String::from_utf16_lossy(&units), units)
        .expect("media skill source text")
}

fn supported_sections(body: &str, available_tool_names: &[String]) -> String {
    let mut include = true;
    let mut result = String::new();
    for line in body.split_inclusive('\n') {
        if let Some(tool_name) = line.strip_prefix("## ") {
            include = available_tool_names
                .iter()
                .any(|name| name == tool_name.trim());
        }
        if include {
            result.push_str(line);
        }
    }
    result
}

/// Register `/media` when at least one media operation is available.
///
/// Reuse after a tool-catalog reload so withdrawn operations remove only their
/// instruction sections and an empty media catalog removes the skill itself.
#[cfg(any(feature = "mobile", test))]
pub(crate) fn register_media_skill(
    registry: &mut CommandRegistry,
    available_tool_names: &[String],
) {
    register_media_skill_inner(registry, available_tool_names, None);
}

/// Register `/media` against a live tool registry so the workflow prompt tracks
/// capability changes without requiring a command-registry reload.
pub(crate) fn register_media_skill_for_tools(
    registry: &mut CommandRegistry,
    tools: Arc<ToolRegistry>,
) {
    let available_tool_names = tools
        .available_tools(&ToolStaticContext::default())
        .into_iter()
        .map(|tool| tool.name().to_owned())
        .collect::<Vec<_>>();
    register_media_skill_inner(registry, &available_tool_names, Some(tools));
}

fn register_media_skill_inner(
    registry: &mut CommandRegistry,
    available_tool_names: &[String],
    live_tools: Option<Arc<ToolRegistry>>,
) {
    registry.unregister_loaded_from(LOADED_FROM);
    let has_media_tools = tool_media::TOOL_NAMES.iter().any(|name| {
        if let Some(tools) = live_tools.as_ref() {
            tools.find_registered(name).is_some()
        } else {
            available_tool_names
                .iter()
                .any(|candidate| candidate == *name)
        }
    });
    if !has_media_tools {
        return;
    }

    let skill_root = PathBuf::from(LOADED_FROM).join(tool_media::skill::NAME);
    let parsed = command_api::parse_skill_command_markdown(
        tool_media::skill::MARKDOWN,
        skill_root.join("SKILL.md"),
        skill_root,
        CommandSource::Bundled,
    );
    let mut command = command_api::build_skill_command(&parsed, CommandSource::Bundled);
    let SlashCommandKind::Markdown {
        mut frontmatter, ..
    } = command.kind
    else {
        unreachable!("the bundled media workflow is a Markdown skill")
    };
    frontmatter.session_modes = Some(vec!["chat".into(), "code".into()]);
    let mut description = command
        .description
        .string_units("")
        .expect("media skill description");
    description.extend(
        " Available workflows are filtered from the current media capabilities.".encode_utf16(),
    );
    command.description = source_from_units(description);
    command.kind = SlashCommandKind::Bundled {
        frontmatter,
        prompt_fn: Some(Arc::new(MediaPrompt {
            body: parsed.content.clone(),
            fallback_tool_names: available_tool_names.to_vec(),
            live_tools,
        })),
    };
    command.loaded_from = Some(LOADED_FROM.into());
    command.skill_root = None;
    registry.register_command(command);
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::atomic::{AtomicBool, Ordering};
    use tool_api::tool_trait::{
        DescriptionOptions, PromptOptions, Tool, ToolCallResult, ToolError,
        ToolInputRepairMetadata, ToolInputRepairSchema, ToolInputSchemaProvenance, ValidationError,
    };
    use tool_api::{ToolProgressSender, ToolUseContext};

    fn names(names: &[&str]) -> Vec<String> {
        names.iter().map(|name| (*name).to_owned()).collect()
    }

    fn skill_body(registry: &CommandRegistry) -> Option<String> {
        let command = registry.resolve(&tool_media::skill::NAME.into())?;
        let SlashCommandKind::Bundled {
            prompt_fn: Some(prompt),
            ..
        } = &command.kind
        else {
            panic!("media skill should have a bundled prompt")
        };
        Some(prompt.build(&"".into()).string_at("")?.to_owned())
    }

    struct ToggleMediaTool {
        enabled: Arc<AtomicBool>,
        schema: Utf16JsonProjection,
    }

    #[async_trait::async_trait]
    impl Tool for ToggleMediaTool {
        fn name(&self) -> &str {
            "media_capture"
        }

        fn input_schema(&self) -> &Utf16JsonProjection {
            &self.schema
        }

        fn input_repair_metadata(&self) -> ToolInputRepairMetadata {
            ToolInputRepairMetadata::Known {
                schema: ToolInputRepairSchema::Advertised,
                schema_provenance: ToolInputSchemaProvenance::GeneratedBuiltin,
                exempt_top_level_keys: None,
            }
        }

        fn is_enabled(&self, _: &ToolStaticContext) -> bool {
            self.enabled.load(Ordering::SeqCst)
        }

        fn max_result_size_chars(&self) -> usize {
            1024
        }

        fn is_concurrency_safe(&self, _: &Utf16JsonProjection) -> bool {
            true
        }

        fn is_read_only(&self, _: &Utf16JsonProjection) -> bool {
            true
        }

        async fn check_permissions(
            &self,
            _: &Utf16JsonProjection,
            _: &ToolUseContext,
        ) -> permission::PermissionResult {
            permission::PermissionResult::Allow {
                reason: permission::PermissionDecisionReason::Other {
                    reason: "live prompt capability fixture".into(),
                },
                updated_input: None,
                update_destination: None,
                metadata: Default::default(),
            }
        }

        async fn description(&self, _: &Utf16JsonProjection, _: &DescriptionOptions) -> String {
            "toggleable media capability fixture".into()
        }

        async fn prompt(&self, _: &PromptOptions) -> Utf16JsonProjection {
            "toggleable media capability fixture".into()
        }

        async fn validate_input(
            &self,
            _: &Utf16JsonProjection,
            _: &ToolUseContext,
        ) -> Result<(), ValidationError> {
            Ok(())
        }

        async fn call(
            &self,
            _: Utf16JsonProjection,
            _: ToolUseContext,
            _: ToolProgressSender,
        ) -> Result<ToolCallResult, ToolError> {
            Err(ToolError::Aborted)
        }
    }

    #[test]
    fn media_skill_is_shared_filters_sections_and_grants_no_tool_list() {
        let mut registry = CommandRegistry::new();
        register_media_skill(
            &mut registry,
            &names(&[
                "media_capture",
                "media_transcribe",
                "media_session_start",
                "media_stop",
            ]),
        );
        let command = registry.resolve(&"media".into()).unwrap();
        let SlashCommandKind::Bundled { frontmatter, .. } = &command.kind else {
            panic!("media skill should be bundled")
        };
        assert!(frontmatter.allowed_tools.is_none());
        assert!(frontmatter.disallowed_tools.is_none());
        assert_eq!(
            frontmatter
                .session_modes
                .as_ref()
                .unwrap()
                .iter()
                .map(String::as_str)
                .collect::<Vec<_>>(),
            ["chat", "code"]
        );
        let body = skill_body(&registry).unwrap();
        assert!(body.contains("## media_capture\n"));
        assert!(body.contains("## media_transcribe\n"));
        assert!(body.contains("## media_session_start\n"));
        assert!(body.contains("## media_stop\n"));
        assert!(!body.contains("## media_record\n"));
        assert!(!body.contains("## media_session_update\n"));
        assert!(body.contains("do not invent references"));
        assert!(body.contains("do not grant OS permissions"));
    }

    #[test]
    fn media_skill_reload_removes_missing_sections_and_then_removes_skill() {
        let mut registry = CommandRegistry::new();
        register_media_skill(&mut registry, &names(&["media_capture", "media_record"]));
        let body = skill_body(&registry).unwrap();
        assert!(body.contains("## media_capture\n"));
        assert!(body.contains("## media_record\n"));
        assert!(!body.contains("## media_speak\n"));

        register_media_skill(&mut registry, &names(&["Read", "Shell"]));
        assert!(skill_body(&registry).is_none());
    }

    #[test]
    fn live_media_prompt_tracks_capability_changes_without_reloading_command() {
        let enabled = Arc::new(AtomicBool::new(false));
        let mut tools = ToolRegistry::new();
        tools.register_builtin(Arc::new(ToggleMediaTool {
            enabled: enabled.clone(),
            schema: Utf16JsonProjection::from_serde_value(&serde_json::json!({
                "type":"object"
            }))
            .unwrap(),
        }));
        let tools = Arc::new(tools);
        let mut registry = CommandRegistry::new();
        register_media_skill_for_tools(&mut registry, tools.clone());

        assert!(!skill_body(&registry)
            .unwrap()
            .contains("## media_capture\n"));
        enabled.store(true, Ordering::SeqCst);
        assert!(skill_body(&registry)
            .unwrap()
            .contains("## media_capture\n"));
        enabled.store(false, Ordering::SeqCst);
        assert!(!skill_body(&registry)
            .unwrap()
            .contains("## media_capture\n"));
    }
}
