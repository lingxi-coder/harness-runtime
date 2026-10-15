//! Tests for `skill.rs`, extracted from inline `#[cfg(test)]` blocks. Included via `#[path] mod skill_test;`.

use super::*;
use lingxi_core::types::utf16_json::Utf16JsonProjection;
use serde_json::json;

macro_rules! source_json {
    ($($tokens:tt)*) => {
        Utf16JsonProjection::from_serde_value(&json!($($tokens)*)).expect("source test JSON")
    };
}

fn source_text<'a>(value: &'a Utf16JsonProjection, pointer: &str) -> Option<&'a str> {
    value.string_at(pointer)
}

#[cfg(test)]
mod tests {
    use super::*;
    fn checked_source(
        source: &lingxi_core::types::utf16_json::Utf16JsonProjection,
    ) -> (String, Vec<u16>) {
        let display = source
            .string_at("")
            .expect("error source is a root string")
            .to_owned();
        let units = source
            .string_units("")
            .expect("error source retains UTF-16 units");
        assert_eq!(String::from_utf16_lossy(&units), display);
        (display, units)
    }

    fn checked_source_text(source: &lingxi_core::types::utf16_json::Utf16JsonProjection) -> String {
        checked_source(source).0
    }

    use lingxi_core::types::{AgentId, SessionId};
    use mobile_linux_api::ProcessOutput;
    use tool_api::test_support::{fresh_ctx, fresh_tx, shell_test_ctx, StubProcess};

    fn dummy_out() -> ProcessOutput {
        ProcessOutput {
            stdout: String::new(),
            stderr: String::new(),
            exit_code: 0,
            timed_out: false,
        }
    }

    /// A loader that returns a fixed descriptor for any name.
    struct FixedLoader(Option<SkillDescriptor>);
    #[async_trait]
    impl SkillLoader for FixedLoader {
        async fn load(
            &self,
            _name: &Utf16JsonProjection,
        ) -> Result<Option<SkillDescriptor>, ToolError> {
            Ok(self.0.clone())
        }
    }

    /// A loader that captures the (normalized) name it was asked to load.
    struct CapturingLoader {
        seen: std::sync::Mutex<Option<Vec<u16>>>,
        desc: Option<SkillDescriptor>,
    }
    #[async_trait]
    impl SkillLoader for CapturingLoader {
        async fn load(
            &self,
            name: &Utf16JsonProjection,
        ) -> Result<Option<SkillDescriptor>, ToolError> {
            *self.seen.lock().unwrap() = Some(name.string_units("").expect("source name"));
            Ok(self.desc.clone())
        }
    }

    fn prompt_desc(name: &str) -> SkillDescriptor {
        SkillDescriptor {
            name: name.into(),
            description: "a skill".into(),
            body: "body here".into(),
            ..SkillDescriptor::default()
        }
    }

    #[tokio::test]
    async fn side_rejects_fork_before_shell_and_keeps_inline_broad_grants() {
        let mut context = shell_test_ctx(dummy_out());
        context.side_chat = true;
        let mut desc = prompt_desc("forker");
        desc.context = Some("fork".into());
        desc.body = "!`must-not-execute`".into();
        let tool = SkillTool::with_loader(context.clone(), Arc::new(FixedLoader(Some(desc))));
        let error = tool
            .call(source_json!({"skill":"forker"}), fresh_ctx(), fresh_tx())
            .await
            .unwrap_err();
        assert!(error.to_string().contains("unavailable in Side Chat"));
        let mut inline = prompt_desc("inline");
        inline.allowed_tools = vec!["*".into()];
        let tool = SkillTool::with_loader(context, Arc::new(FixedLoader(Some(inline))));
        assert!(tool
            .call(source_json!({"skill":"inline"}), fresh_ctx(), fresh_tx())
            .await
            .is_ok());
    }

    #[test]
    fn constants_locked() {
        assert_eq!(SKILL_TOOL_NAME, "Skill");
        assert_eq!(MAX_SKILL_DESCRIPTOR_LEN, 1024);
    }

    #[test]
    fn normalize_strips_one_invocation_prefix_and_trims() {
        assert_eq!(
            normalize_skill_name(&"/commit".into()).string_at(""),
            Some("commit")
        );
        assert_eq!(
            normalize_skill_name(&"  /commit  ".into()).string_at(""),
            Some("commit")
        );
        assert_eq!(
            normalize_skill_name(&"$commit".into()).string_at(""),
            Some("commit")
        );
        assert_eq!(
            normalize_skill_name(&"  $commit  ".into()).string_at(""),
            Some("commit")
        );
        assert_eq!(
            normalize_skill_name(&"commit".into()).string_at(""),
            Some("commit")
        );
        // Only a single invocation prefix is stripped.
        assert_eq!(
            normalize_skill_name(&"//commit".into()).string_at(""),
            Some("/commit")
        );
        assert_eq!(
            normalize_skill_name(&"$$commit".into()).string_at(""),
            Some("$commit")
        );
    }

    #[test]
    fn skill_name_dimension_keeps_builtin_and_sanitizes_custom() {
        // A known builtin command name passes through unchanged...
        let a_builtin = command_api::builtin_support::names::BUILTIN_COMMAND_NAMES[0];
        assert_eq!(skill_name_dimension(a_builtin), a_builtin);
        // ...while any non-builtin (custom) skill collapses to "custom" — the
        // Verified/whitelisted SKILL_INVOKED dimension never leaks a raw PII name.
        assert_eq!(
            skill_name_dimension("definitely-not-a-builtin-skill-xyz"),
            "custom"
        );
    }

    #[test]
    fn schema_uses_skill_and_optional_args() {
        let props = &SCHEMA["properties"];
        assert_eq!(props["skill"]["type"], json!("string"));
        // TS `inputSchema` is `skill: z.string().describe(...)` with NO `.min(1)`
        // (SkillTool.ts:291-298) — the delivered schema must NOT carry a
        // `minLength` constraint. The runtime "Invalid skill format" check
        // (validate_input/call) is what enforces non-blankness, not the schema.
        assert!(
            props["skill"].get("minLength").is_none(),
            "skill schema must not constrain minLength (TS has no .min(1))"
        );
        assert_eq!(props["args"]["type"], json!("string"));
        assert_eq!(SCHEMA["required"], json!(["skill"]));
        // No legacy `name` field remains.
        assert!(props.get("name").is_none());
    }

    #[tokio::test]
    async fn slash_prefixed_skill_is_normalized_before_lookup() {
        let loader = Arc::new(CapturingLoader {
            seen: std::sync::Mutex::new(None),
            desc: Some(prompt_desc("commit")),
        });
        let tool = SkillTool::with_loader(shell_test_ctx(dummy_out()), loader.clone());
        let out = tool
            .call(source_json!({"skill": "/commit"}), fresh_ctx(), fresh_tx())
            .await
            .expect("ok");
        // Loader saw the slash-stripped name.
        assert_eq!(
            loader.seen.lock().unwrap().as_ref().unwrap(),
            &"commit".encode_utf16().collect::<Vec<_>>()
        );
        // commandName is the normalized name + inline status.
        assert_eq!(source_text(&out.data, "/commandName"), Some("commit"));
        assert_eq!(source_text(&out.data, "/status"), Some("inline"));
        assert_eq!(
            out.data.at_pointer("/success").and_then(|v| v.as_bool()),
            Some(true)
        );
    }

    #[tokio::test]
    async fn dollar_prefixed_skill_is_normalized_before_lookup() {
        let loader = Arc::new(CapturingLoader {
            seen: std::sync::Mutex::new(None),
            desc: Some(prompt_desc("frontend-design")),
        });
        let tool = SkillTool::with_loader(shell_test_ctx(dummy_out()), loader.clone());
        let out = tool
            .call(
                source_json!({"skill": "$frontend-design"}),
                fresh_ctx(),
                fresh_tx(),
            )
            .await
            .expect("the coordinator shorthand resolves");
        assert_eq!(
            loader.seen.lock().unwrap().as_ref().unwrap(),
            &"frontend-design".encode_utf16().collect::<Vec<_>>()
        );
        assert_eq!(
            source_text(&out.data, "/commandName"),
            Some("frontend-design")
        );
        assert_eq!(source_text(&out.data, "/status"), Some("inline"));
        assert_eq!(
            out.data.at_pointer("/success").and_then(|v| v.as_bool()),
            Some(true)
        );
    }

    #[tokio::test]
    async fn exact_skill_name_units_reach_the_result_without_utf8_lowering() {
        let name =
            Utf16JsonProjection::root_string(String::from_utf16_lossy(&[0xd800]), vec![0xd800])
                .expect("exact skill name fixture");
        let input = Utf16JsonProjection::object(vec![("skill".to_owned(), name.clone())])
            .expect("skill input fixture");
        let tool = SkillTool::with_loader(
            shell_test_ctx(dummy_out()),
            Arc::new(FixedLoader(Some(prompt_desc("source-name")))),
        );
        let result = tool
            .call(input, fresh_ctx(), fresh_tx())
            .await
            .expect("call");
        assert_eq!(
            result
                .data
                .at_pointer("/commandName")
                .and_then(|value| value.code_units())
                .map(|units| units.to_vec()),
            name.string_units("")
        );
    }

    #[tokio::test]
    async fn unknown_skill_locked_error() {
        let tool = SkillTool::new(shell_test_ctx(dummy_out()));
        let err = tool
            .call(source_json!({"skill": "absent"}), fresh_ctx(), fresh_tx())
            .await
            .expect_err("unknown");
        assert!(checked_source_text(&err.model_facing_message()).contains("Unknown skill: absent"));
    }

    #[tokio::test]
    async fn disable_model_invocation_locked_error() {
        let desc = SkillDescriptor {
            disable_model_invocation: true,
            ..prompt_desc("locked")
        };
        let tool = SkillTool::with_loader(
            shell_test_ctx(dummy_out()),
            Arc::new(FixedLoader(Some(desc))),
        );
        let err = tool
            .call(source_json!({"skill": "locked"}), fresh_ctx(), fresh_tx())
            .await
            .expect_err("disabled");
        assert!(checked_source_text(&err.model_facing_message()).contains(
            "Skill locked cannot be used with Skill tool due to disable-model-invocation"
        ));
    }

    #[tokio::test]
    async fn non_prompt_skill_locked_error() {
        let desc = SkillDescriptor {
            command_type: SkillCommandType::Other,
            ..prompt_desc("local-cmd")
        };
        let tool = SkillTool::with_loader(
            shell_test_ctx(dummy_out()),
            Arc::new(FixedLoader(Some(desc))),
        );
        let err = tool
            .call(
                source_json!({"skill": "local-cmd"}),
                fresh_ctx(),
                fresh_tx(),
            )
            .await
            .expect_err("non-prompt");
        assert!(checked_source_text(&err.model_facing_message()).contains(
            "local-cmd is a built-in CLI command, not a skill. Ask the user to run /local-cmd themselves — it cannot be invoked via the Skill tool."
        ));
    }

    #[tokio::test]
    async fn result_surfaces_model_and_allowed_tools_and_status_inline() {
        let desc = SkillDescriptor {
            model: Some("opus".into()),
            allowed_tools: vec!["Bash".into(), "Read".into()],
            ..prompt_desc("rich")
        };
        let tool = SkillTool::with_loader(
            shell_test_ctx(dummy_out()),
            Arc::new(FixedLoader(Some(desc))),
        );
        let out = tool
            .call(source_json!({"skill": "rich"}), fresh_ctx(), fresh_tx())
            .await
            .expect("ok");
        assert_eq!(source_text(&out.data, "/status"), Some("inline"));
        assert_eq!(source_text(&out.data, "/model"), Some("opus"));
        let allowed = out
            .data
            .at_pointer("/allowedTools")
            .expect("allowed tools array");
        assert_eq!(allowed.array_len(), Some(2));
        assert_eq!(allowed.get_index(0).and_then(|v| v.as_str()), Some("Bash"));
        assert_eq!(allowed.get_index(1).and_then(|v| v.as_str()), Some("Read"));
        assert_eq!(source_text(&out.data, "/commandName"), Some("rich"));
    }

    #[tokio::test]
    async fn model_and_allowed_tools_omitted_when_absent() {
        let tool = SkillTool::with_loader(
            shell_test_ctx(dummy_out()),
            Arc::new(FixedLoader(Some(prompt_desc("plain")))),
        );
        let out = tool
            .call(source_json!({"skill": "plain"}), fresh_ctx(), fresh_tx())
            .await
            .expect("ok");
        // Optional fields are omitted (matches TS `.optional()` surfacing).
        assert!(out.data.at_pointer("/model").is_none());
        assert!(out.data.at_pointer("/allowedTools").is_none());
        assert_eq!(source_text(&out.data, "/body"), Some("body here"));
    }

    #[tokio::test]
    async fn model_frontmatter_returns_context_modifier_switching_main_loop_model() {
        // Frontmatter is passed to the host without provider or model defaults.
        let desc = SkillDescriptor {
            model: Some("balanced".into()),
            ..prompt_desc("switcher")
        };
        let tool = SkillTool::with_loader(
            shell_test_ctx(dummy_out()),
            Arc::new(FixedLoader(Some(desc))),
        );
        let out = tool
            .call(source_json!({"skill": "switcher"}), fresh_ctx(), fresh_tx())
            .await
            .expect("ok");
        let modifier = out
            .context_modifier
            .expect("a skill with model: returns a context_modifier");
        let modified = modifier(fresh_ctx());
        assert_eq!(modified.options.main_loop_model, "balanced");
        assert_eq!(modified.options.model_profile, None);
    }

    #[tokio::test]
    async fn model_frontmatter_modifier_defers_route_and_context_window_resolution() {
        let desc = SkillDescriptor {
            model: Some("other/shared".into()),
            ..prompt_desc("switcher")
        };
        let tool = SkillTool::with_loader(
            shell_test_ctx(dummy_out()),
            Arc::new(FixedLoader(Some(desc))),
        );
        let out = tool
            .call(source_json!({"skill": "switcher"}), fresh_ctx(), fresh_tx())
            .await
            .expect("ok");
        let modifier = out.context_modifier.expect("context_modifier present");
        let mut seed = fresh_ctx();
        seed.options.main_loop_model = "parent[1m]".into();
        seed.options.model_profile = Some("current".into());
        let modified = modifier(seed);
        assert_eq!(modified.options.main_loop_model, "other/shared");
        assert_eq!(modified.options.model_profile, None);
    }

    #[tokio::test]
    async fn no_model_frontmatter_returns_no_context_modifier() {
        // Byte-identical guard: a skill WITHOUT a `model:` frontmatter returns
        // `context_modifier: None`, so the turn loop's no-override path is
        // untouched (session.model never changes).
        let tool = SkillTool::with_loader(
            shell_test_ctx(dummy_out()),
            Arc::new(FixedLoader(Some(prompt_desc("plain")))),
        );
        let out = tool
            .call(source_json!({"skill": "plain"}), fresh_ctx(), fresh_tx())
            .await
            .expect("ok");
        assert!(
            out.context_modifier.is_none(),
            "no model: frontmatter → no context_modifier (byte-identical)"
        );
    }

    #[tokio::test]
    async fn model_content_is_inline_launching_line_and_omits_body() {
        // TS inline path (SkillTool.ts:856-861): the model's tool_result content
        // is exactly `Launching skill: ${commandName}` — never the JSON dump of
        // the output object (which would leak the full skill `body`).
        let desc = SkillDescriptor {
            body: "SECRET FULL SKILL BODY that must not reach the model".into(),
            ..prompt_desc("commit")
        };
        let tool = SkillTool::with_loader(
            shell_test_ctx(dummy_out()),
            Arc::new(FixedLoader(Some(desc))),
        );
        // Slash-prefixed input: model_content uses the normalized name.
        let out = tool
            .call(source_json!({"skill": "/commit"}), fresh_ctx(), fresh_tx())
            .await
            .expect("ok");
        let mc = source_text(&out.data, "/model_content").expect("model_content is a string");
        assert_eq!(mc, "Launching skill: commit");
        // The body still rides along for non-model consumers, but is NOT in the
        // model-facing string.
        assert!(!mc.contains("SECRET FULL SKILL BODY"));
        assert_eq!(
            source_text(&out.data, "/body"),
            Some("SECRET FULL SKILL BODY that must not reach the model")
        );
    }

    #[tokio::test]
    async fn args_accepted_optionally_and_echoed() {
        let tool = SkillTool::with_loader(
            shell_test_ctx(dummy_out()),
            Arc::new(FixedLoader(Some(prompt_desc("commit")))),
        );
        // With args.
        let out = tool
            .call(
                source_json!({"skill": "commit", "args": "--amend"}),
                fresh_ctx(),
                fresh_tx(),
            )
            .await
            .expect("ok");
        assert_eq!(source_text(&out.data, "/args"), Some("--amend"));
        // Without args — no `args` key.
        let out2 = tool
            .call(source_json!({"skill": "commit"}), fresh_ctx(), fresh_tx())
            .await
            .expect("ok");
        assert!(out2.data.at_pointer("/args").is_none());
    }

    /// SKILLEXEC.3: a model-invocable Prompt skill expands its body's
    /// `$ARGUMENTS` into a `new_messages` user message the turn loop injects, so
    /// the model acts on the expanded skill prompt.
    #[tokio::test]
    async fn expands_arguments_into_new_messages() {
        let desc = SkillDescriptor {
            body: "Review PR $ARGUMENTS now".into(),
            ..prompt_desc("review-pr")
        };
        let tool = SkillTool::with_loader(
            shell_test_ctx(dummy_out()),
            Arc::new(FixedLoader(Some(desc))),
        );
        let out = tool
            .call(
                source_json!({"skill": "review-pr", "args": "123"}),
                fresh_ctx(),
                fresh_tx(),
            )
            .await
            .expect("ok");
        assert_eq!(out.new_messages.len(), 1, "expanded prompt injected");
        match &out.new_messages[0] {
            lingxi_core::types::ConversationMessage::User { content, .. } => {
                match content.first() {
                    Some(lingxi_core::types::ContentBlock::TextJsUtf16 { text, .. }) => {
                        assert_eq!(text, "Review PR 123 now");
                    }
                    other => panic!("expected leading Text block, got {other:?}"),
                }
            }
            other => panic!("expected injected User message, got {other:?}"),
        }
        // The inline model-facing string is still the launch line — the body is
        // NOT leaked into the tool_result content (SKILLEXEC.1 invariant holds).
        assert_eq!(
            source_text(&out.data, "/model_content"),
            Some("Launching skill: review-pr")
        );
    }

    #[tokio::test]
    async fn raw_utf16_arguments_survive_prompt_expansion_and_result_projection() {
        let mut argument_units = "run ".encode_utf16().collect::<Vec<_>>();
        argument_units.push(0xd800);
        argument_units.extend(" exactly".encode_utf16());
        let raw_args = Utf16JsonProjection::root_string(
            String::from_utf16_lossy(&argument_units),
            argument_units.clone(),
        )
        .expect("raw argument fixture");
        let input = Utf16JsonProjection::object(vec![
            ("skill".to_owned(), "raw-args".into()),
            ("args".to_owned(), raw_args),
        ])
        .expect("skill input fixture");
        let desc = SkillDescriptor {
            body: "$ARGUMENTS".into(),
            ..prompt_desc("raw-args")
        };
        let tool = SkillTool::with_loader(
            shell_test_ctx(dummy_out()),
            Arc::new(FixedLoader(Some(desc))),
        );
        let out = tool
            .call(input, fresh_ctx(), fresh_tx())
            .await
            .expect("skill call");
        assert_eq!(
            out.data
                .at_pointer("/args")
                .and_then(|value| value.code_units())
                .map(|units| units.to_vec()),
            Some(argument_units.clone())
        );
        let lingxi_core::types::ConversationMessage::User { content, .. } = &out.new_messages[0]
        else {
            panic!("skill prompt is a user message")
        };
        let Some(lingxi_core::types::ContentBlock::TextJsUtf16 {
            utf16_code_units, ..
        }) = content.first()
        else {
            panic!("skill prompt is an exact UTF-16 text block")
        };
        assert_eq!(utf16_code_units, &argument_units);
    }

    #[tokio::test]
    async fn raw_utf16_skill_body_survives_substitution_and_result_projection() {
        let mut body_units = "prompt ".encode_utf16().collect::<Vec<_>>();
        body_units.push(0xd800);
        body_units.extend(" $ARGUMENTS".encode_utf16());
        let body = Utf16JsonProjection::root_string(
            String::from_utf16_lossy(&body_units),
            body_units.clone(),
        )
        .expect("source skill body");
        let expected_prompt = [
            "prompt ".encode_utf16().collect::<Vec<_>>(),
            vec![0xd800],
            " run".encode_utf16().collect(),
        ]
        .concat();
        let input = Utf16JsonProjection::object(vec![
            ("skill".to_owned(), "raw-body".into()),
            ("args".to_owned(), "run".into()),
        ])
        .expect("skill input fixture");
        let descriptor = SkillDescriptor {
            body: body.clone(),
            ..prompt_desc("raw-body")
        };
        let tool = SkillTool::with_loader(
            shell_test_ctx(dummy_out()),
            Arc::new(FixedLoader(Some(descriptor))),
        );

        let result = tool
            .call(input, fresh_ctx(), fresh_tx())
            .await
            .expect("skill invocation");

        assert_eq!(
            result
                .data
                .at_pointer("/body")
                .and_then(|value| value.code_units())
                .map(|units| units.to_vec()),
            Some(body_units)
        );
        let lingxi_core::types::ConversationMessage::User { content, .. } = &result.new_messages[0]
        else {
            panic!("skill prompt is a user message")
        };
        let Some(lingxi_core::types::ContentBlock::TextJsUtf16 {
            utf16_code_units, ..
        }) = content.first()
        else {
            panic!("skill prompt is an exact UTF-16 text block")
        };
        assert_eq!(utf16_code_units, &expected_prompt);
    }

    /// Named frontmatter arguments (`$name`) resolve via the descriptor's
    /// `argument_names` (TS `processPromptSlashCommand` passes the command's
    /// `argNames`).
    #[tokio::test]
    async fn expands_named_argument_into_new_messages() {
        let desc = SkillDescriptor {
            body: "Hello $name".into(),
            argument_names: vec!["name".into()],
            ..prompt_desc("greet")
        };
        let tool = SkillTool::with_loader(
            shell_test_ctx(dummy_out()),
            Arc::new(FixedLoader(Some(desc))),
        );
        let out = tool
            .call(
                source_json!({"skill": "greet", "args": "world"}),
                fresh_ctx(),
                fresh_tx(),
            )
            .await
            .expect("ok");
        match &out.new_messages[0] {
            lingxi_core::types::ConversationMessage::User { content, .. } => {
                match content.first() {
                    Some(lingxi_core::types::ContentBlock::TextJsUtf16 { text, .. }) => {
                        assert_eq!(text, "Hello world")
                    }
                    other => panic!("expected leading Text block, got {other:?}"),
                }
            }
            other => panic!("expected injected User message, got {other:?}"),
        }
    }

    #[tokio::test]
    async fn dynamic_body_checked_error_is_not_injected_as_instructions() {
        struct Checked;
        impl command_api::BundledPromptFn for Checked {
            fn build(&self, _: &Utf16JsonProjection) -> Utf16JsonProjection {
                panic!("unchecked expansion");
            }
            fn try_build_at(
                &self,
                _: &Utf16JsonProjection,
                context: command_api::BundledPromptContext<'_>,
            ) -> std::io::Result<Utf16JsonProjection> {
                let root = context.project_root;
                let cwd = context.cwd;
                let preload = context.is_preload;
                assert_eq!(context.main_loop_model, Some("claude-fable-5"));
                assert_eq!(root, std::path::Path::new("/project"));
                assert_eq!(cwd, std::path::Path::new("/project/subdir"));
                assert!(!preload);
                Err(std::io::Error::new(
                    std::io::ErrorKind::PermissionDenied,
                    "loop.md denied",
                ))
            }
        }
        let desc = SkillDescriptor {
            dynamic_body: Some(Arc::new(Checked)),
            ..prompt_desc("loop")
        };
        let mut builtin = shell_test_ctx(dummy_out());
        builtin.session_cwd = tool_api::SessionCwd::new("/project".into(), Vec::new());
        let tool = SkillTool::with_loader(builtin, Arc::new(FixedLoader(Some(desc))));
        let mut call = fresh_ctx();
        call.cwd = Some("/project/subdir".into());
        call.options.main_loop_model = "claude-fable-5".into();
        let error = tool
            .call(source_json!({"skill":"loop"}), call, fresh_tx())
            .await
            .unwrap_err();
        assert!(error.to_string().contains("loop.md denied"));
    }

    /// SKILLEXEC.3 (bundled): when a descriptor carries `dynamic_body`, the Skill
    /// tool calls the builder with the raw args INSTEAD of static templating, so
    /// the bundled-skill two-branch behavior (empty→usage, else→buildPrompt) is
    /// honored end-to-end (port of `getPromptForCommand`, loop.ts:84). The
    /// descriptor `body` is ignored on this path.
    #[tokio::test]
    async fn dynamic_body_replaces_static_templating() {
        // Test builder mirroring loop.ts's empty→usage vs non-empty branch.
        struct TestBuilder;
        impl command_api::BundledPromptFn for TestBuilder {
            fn build(&self, args: &Utf16JsonProjection) -> Utf16JsonProjection {
                let t = args
                    .string_at("")
                    .expect("fixture arguments are valid UTF-16")
                    .trim();
                if t.is_empty() {
                    "USAGE".into()
                } else {
                    format!("BUILT[{t}]").into()
                }
            }
        }
        let desc = SkillDescriptor {
            // `body` carries a `$ARGUMENTS` placeholder that MUST be ignored.
            body: "STATIC $ARGUMENTS".into(),
            dynamic_body: Some(Arc::new(TestBuilder)),
            ..prompt_desc("loop")
        };
        let tool = SkillTool::with_loader(
            shell_test_ctx(dummy_out()),
            Arc::new(FixedLoader(Some(desc.clone()))),
        );

        // Empty args → usage branch.
        let out = tool
            .call(source_json!({"skill": "loop"}), fresh_ctx(), fresh_tx())
            .await
            .expect("ok");
        match &out.new_messages[0] {
            lingxi_core::types::ConversationMessage::User { content, .. } => {
                match content.first() {
                    Some(lingxi_core::types::ContentBlock::TextJsUtf16 { text, .. }) => {
                        assert_eq!(text, "USAGE")
                    }
                    other => panic!("expected leading Text block, got {other:?}"),
                }
            }
            other => panic!("expected injected User message, got {other:?}"),
        }

        // Non-empty args → buildPrompt branch; raw args reach the builder.
        let tool = SkillTool::with_loader(
            shell_test_ctx(dummy_out()),
            Arc::new(FixedLoader(Some(desc))),
        );
        let out = tool
            .call(
                source_json!({"skill": "loop", "args": "  check the deploy  "}),
                fresh_ctx(),
                fresh_tx(),
            )
            .await
            .expect("ok");
        match &out.new_messages[0] {
            lingxi_core::types::ConversationMessage::User { content, .. } => {
                match content.first() {
                    Some(lingxi_core::types::ContentBlock::TextJsUtf16 { text, .. }) => {
                        assert_eq!(text, "BUILT[check the deploy]");
                    }
                    other => panic!("expected leading Text block, got {other:?}"),
                }
            }
            other => panic!("expected injected User message, got {other:?}"),
        }
    }

    /// A skill with no placeholders and empty args injects the body verbatim
    /// (no `$ARGUMENTS` tail append — TS appends only when args is non-empty).
    #[tokio::test]
    async fn no_placeholder_no_args_injects_body_verbatim() {
        let tool = SkillTool::with_loader(
            shell_test_ctx(dummy_out()),
            Arc::new(FixedLoader(Some(prompt_desc("plain")))),
        );
        let out = tool
            .call(source_json!({"skill": "plain"}), fresh_ctx(), fresh_tx())
            .await
            .expect("ok");
        match &out.new_messages[0] {
            lingxi_core::types::ConversationMessage::User { content, .. } => {
                match content.first() {
                    Some(lingxi_core::types::ContentBlock::TextJsUtf16 { text, .. }) => {
                        assert_eq!(text, "body here")
                    }
                    other => panic!("expected leading Text block, got {other:?}"),
                }
            }
            other => panic!("expected injected User message, got {other:?}"),
        }
    }

    fn out_with_stdout(stdout: &str) -> ProcessOutput {
        ProcessOutput {
            stdout: stdout.to_string(),
            stderr: String::new(),
            exit_code: 0,
            timed_out: false,
        }
    }

    /// SKILLEXEC.6: a skill body with an embedded inline `!`…`` block runs the
    /// command through the injected shell runner and splices its stdout into the
    /// expanded prompt (the fake `ProcessRunner` returns "hi" for any command).
    #[tokio::test]
    async fn expands_inline_shell_command_into_new_messages() {
        let desc = SkillDescriptor {
            body: "before !`echo hi` after".into(),
            ..prompt_desc("sh")
        };
        // Snapshot creation consumes the first process result; because the stub
        // does not materialize the requested file, execution falls back to the
        // login shell and consumes the second result.
        let mut ctx = shell_test_ctx(dummy_out());
        ctx.process = Arc::new(StubProcess::with(vec![
            dummy_out(),
            out_with_stdout("hi\n"),
        ]));
        let tool = SkillTool::with_loader(ctx, Arc::new(FixedLoader(Some(desc))));
        let out = tool
            .call(source_json!({"skill": "sh"}), fresh_ctx(), fresh_tx())
            .await
            .expect("ok");
        match &out.new_messages[0] {
            lingxi_core::types::ConversationMessage::User { content, .. } => {
                match content.first() {
                    Some(lingxi_core::types::ContentBlock::TextJsUtf16 { text, .. }) => {
                        // format_bash_output trims the stdout -> "hi".
                        assert_eq!(text, "before hi after");
                    }
                    other => panic!("expected leading Text block, got {other:?}"),
                }
            }
            other => panic!("expected injected User message, got {other:?}"),
        }
        // The model-facing line is still the launch string (body not leaked).
        assert_eq!(
            source_text(&out.data, "/model_content"),
            Some("Launching skill: sh")
        );
    }

    /// SKILLEXEC #26: the shell snapshot is SESSION-scoped — its creation shell
    /// runs at most once per `SkillTool`, not once per `call()`. Two skill calls
    /// with embedded commands therefore consume 3 process runs (1 snapshot + 2
    /// commands), not 4 (which is what re-creating the provider per call did,
    /// re-sourcing the user's rc file every invocation).
    #[tokio::test]
    async fn shell_snapshot_is_created_once_per_session_not_per_call() {
        let desc = SkillDescriptor {
            body: "before !`echo hi` after".into(),
            ..prompt_desc("sh")
        };
        let stub = std::sync::Arc::new(StubProcess::with(vec![
            dummy_out(),             // snapshot creation — expected exactly ONCE
            out_with_stdout("hi\n"), // call 1's embedded command
            out_with_stdout("hi\n"), // call 2's embedded command
            dummy_out(),             // slack: consumed only if the snapshot re-runs
        ]));
        let mut ctx = shell_test_ctx(dummy_out());
        ctx.process = stub.clone();
        let tool = SkillTool::with_loader(ctx, Arc::new(FixedLoader(Some(desc))));

        for _ in 0..2 {
            let out = tool
                .call(source_json!({"skill": "sh"}), fresh_ctx(), fresh_tx())
                .await
                .expect("ok");
            match &out.new_messages[0] {
                lingxi_core::types::ConversationMessage::User { content, .. } => {
                    match content.first() {
                        Some(lingxi_core::types::ContentBlock::TextJsUtf16 { text, .. }) => {
                            assert_eq!(text, "before hi after");
                        }
                        other => panic!("expected leading Text block, got {other:?}"),
                    }
                }
                other => panic!("expected injected User message, got {other:?}"),
            }
        }

        // 4 queued − 3 consumed (1 snapshot + 2 commands) = 1 left. Re-creating
        // the provider per call would have consumed all 4.
        assert_eq!(
            stub.remaining(),
            1,
            "the snapshot shell must run once per session, not once per skill call"
        );
    }

    /// SAFETY: a skill body with NO `!command` is byte-identical to the
    /// argument-substituted text — the shell-expansion engine returns the input
    /// unchanged and the fake process is NEVER invoked (it would error if it were:
    /// the stub is exhausted after one call, but no call happens).
    #[tokio::test]
    async fn no_shell_command_is_byte_identical_to_arg_substituted() {
        let body = "Review PR $ARGUMENTS now (no shell here)";
        let desc = SkillDescriptor {
            body: body.into(),
            ..prompt_desc("nb")
        };
        // Independently compute the pure arg-substitution result.
        let expected = command_api::substitute_arguments_source(
            &Utf16JsonProjection::from(body),
            &Utf16JsonProjection::from("123"),
            true,
            &[],
        )
        .unwrap()
        .string_at("")
        .unwrap()
        .to_owned();
        // dummy_out() has empty stdout; if the runner were ever called and then
        // called AGAIN, the stub would error — proving the no-op path.
        let tool = SkillTool::with_loader(
            shell_test_ctx(dummy_out()),
            Arc::new(FixedLoader(Some(desc))),
        );
        let out = tool
            .call(
                source_json!({"skill": "nb", "args": "123"}),
                fresh_ctx(),
                fresh_tx(),
            )
            .await
            .expect("ok");
        match &out.new_messages[0] {
            lingxi_core::types::ConversationMessage::User { content, .. } => {
                match content.first() {
                    Some(lingxi_core::types::ContentBlock::TextJsUtf16 { text, .. }) => {
                        assert_eq!(text, &expected);
                        assert_eq!(text, "Review PR 123 now (no shell here)");
                    }
                    other => panic!("expected leading Text block, got {other:?}"),
                }
            }
            other => panic!("expected injected User message, got {other:?}"),
        }
    }

    /// SAFETY: an MCP-sourced skill (`skip_shell_expansion = true`) is
    /// byte-identical to the arg-substituted text even when its body LOOKS like it
    /// has an embedded `!command` — the expansion call is skipped entirely (TS
    /// `loadedFrom !== 'mcp'` gate).
    #[tokio::test]
    async fn mcp_skip_shell_expansion_is_byte_identical() {
        let desc = SkillDescriptor {
            body: "before !`echo hi` after".into(),
            skip_shell_expansion: true,
            ..prompt_desc("mcpish")
        };
        // dummy_out(): the runner must NOT be called, so its (empty) stdout never
        // matters; a call would not change the body, but skip proves no execution.
        let tool = SkillTool::with_loader(
            shell_test_ctx(dummy_out()),
            Arc::new(FixedLoader(Some(desc))),
        );
        let out = tool
            .call(source_json!({"skill": "mcpish"}), fresh_ctx(), fresh_tx())
            .await
            .expect("ok");
        match &out.new_messages[0] {
            lingxi_core::types::ConversationMessage::User { content, .. } => {
                match content.first() {
                    Some(lingxi_core::types::ContentBlock::TextJsUtf16 { text, .. }) => {
                        // Body is verbatim — the `!`echo hi`` block is NOT expanded.
                        assert_eq!(text, "before !`echo hi` after");
                    }
                    other => panic!("expected leading Text block, got {other:?}"),
                }
            }
            other => panic!("expected injected User message, got {other:?}"),
        }
    }

    #[tokio::test]
    async fn descriptor_truncated_at_1024() {
        let desc = SkillDescriptor {
            description: "x".repeat(MAX_SKILL_DESCRIPTOR_LEN + 100).into(),
            ..prompt_desc("huge")
        };
        let tool = SkillTool::with_loader(
            shell_test_ctx(dummy_out()),
            Arc::new(FixedLoader(Some(desc))),
        );
        let out = tool
            .call(source_json!({"skill": "huge"}), fresh_ctx(), fresh_tx())
            .await
            .expect("ok");
        assert!(source_text(&out.data, "/body").is_some(), "body present");
        assert_eq!(
            out.data
                .at_pointer("/descriptor_truncated")
                .and_then(|v| v.as_bool()),
            Some(true)
        );
    }

    #[tokio::test]
    async fn blank_skill_rejected() {
        let tool = SkillTool::new(shell_test_ctx(dummy_out()));
        let err = tool
            .call(source_json!({"skill": "   "}), fresh_ctx(), fresh_tx())
            .await
            .expect_err("blank");
        assert!(checked_source_text(&err.model_facing_message()).contains("Invalid skill format"));
    }

    #[tokio::test]
    async fn rejects_missing_skill() {
        let tool = SkillTool::new(shell_test_ctx(dummy_out()));
        let err = tool
            .call(source_json!({}), fresh_ctx(), fresh_tx())
            .await
            .expect_err("missing");
        assert!(checked_source_text(&err.model_facing_message())
            .contains("missing or non-string skill"));
    }

    #[tokio::test]
    async fn validate_input_applies_locked_rejections() {
        // Unknown via empty loader.
        let tool = SkillTool::new(shell_test_ctx(dummy_out()));
        let err = tool
            .validate_input(&source_json!({"skill": "/absent"}), &fresh_ctx())
            .await
            .expect_err("unknown");
        assert!(err
            .0
            .string_at("")
            .is_some_and(|text| text.contains("Unknown skill: absent")));

        // disable-model-invocation.
        let desc = SkillDescriptor {
            disable_model_invocation: true,
            ..prompt_desc("x")
        };
        let tool = SkillTool::with_loader(
            shell_test_ctx(dummy_out()),
            Arc::new(FixedLoader(Some(desc))),
        );
        let err = tool
            .validate_input(&source_json!({"skill": "x"}), &fresh_ctx())
            .await
            .expect_err("disabled");
        assert!(err
            .0
            .string_at("")
            .expect("validation error is a string")
            .contains("cannot be used with Skill tool due to disable-model-invocation"));
    }

    // ========================================================================
    // ${LINGXI_SKILL_DIR} / ${LINGXI_SESSION_ID} token substitution
    // (TS getPromptForCommand steps 2-3, loadSkillsDir.ts:359-369). The tokens
    // are substituted AFTER argument substitution and BEFORE shell expansion.
    // ========================================================================

    /// Extract the leading Text block of the single injected user message.
    fn injected_text(out: &ToolCallResult) -> String {
        match &out.new_messages[0] {
            lingxi_core::types::ConversationMessage::User { content, .. } => {
                match content.first() {
                    Some(lingxi_core::types::ContentBlock::TextJsUtf16 { text, .. }) => {
                        text.clone()
                    }
                    other => panic!("expected leading Text block, got {other:?}"),
                }
            }
            other => panic!("expected injected User message, got {other:?}"),
        }
    }

    /// Step 2: `${LINGXI_SKILL_DIR}` is replaced with `skill_root` when present.
    #[tokio::test]
    async fn skill_dir_token_replaced_when_skill_root_present() {
        let desc = SkillDescriptor {
            body: "scripts live in ${LINGXI_SKILL_DIR}/bin".into(),
            skill_root: Some(std::path::PathBuf::from("/skills/foo")),
            ..prompt_desc("dir")
        };
        let tool = SkillTool::with_loader(
            shell_test_ctx(dummy_out()),
            Arc::new(FixedLoader(Some(desc))),
        );
        let out = tool
            .call(source_json!({"skill": "dir"}), fresh_ctx(), fresh_tx())
            .await
            .expect("ok");
        assert_eq!(
            injected_text(&out),
            "Base directory for this skill: /skills/foo\n\nscripts live in /skills/foo/bin"
        );
    }

    #[tokio::test]
    async fn file_backed_skill_prompt_includes_base_directory_prefix() {
        let desc = SkillDescriptor {
            body: "Use the local scripts".into(),
            skill_root: Some(std::path::PathBuf::from("/skills/foo")),
            ..prompt_desc("dir")
        };
        let tool = SkillTool::with_loader(
            shell_test_ctx(dummy_out()),
            Arc::new(FixedLoader(Some(desc))),
        );
        let out = tool
            .call(source_json!({"skill": "dir"}), fresh_ctx(), fresh_tx())
            .await
            .expect("ok");
        assert_eq!(
            injected_text(&out),
            "Base directory for this skill: /skills/foo\n\nUse the local scripts"
        );
    }

    /// Step 2 (gate): with NO `skill_root` (e.g. MCP / non-file skills), the
    /// `${LINGXI_SKILL_DIR}` token is left untouched (TS gates on `baseDir`).
    #[tokio::test]
    async fn skill_dir_token_left_as_is_when_skill_root_absent() {
        let desc = SkillDescriptor {
            body: "scripts live in ${LINGXI_SKILL_DIR}/bin".into(),
            skill_root: None,
            ..prompt_desc("dir")
        };
        let tool = SkillTool::with_loader(
            shell_test_ctx(dummy_out()),
            Arc::new(FixedLoader(Some(desc))),
        );
        let out = tool
            .call(source_json!({"skill": "dir"}), fresh_ctx(), fresh_tx())
            .await
            .expect("ok");
        assert_eq!(
            injected_text(&out),
            "scripts live in ${LINGXI_SKILL_DIR}/bin"
        );
    }

    /// Step 2: ALL occurrences of `${LINGXI_SKILL_DIR}` are replaced (global,
    /// matching the TS `/…/g` regex).
    #[tokio::test]
    async fn skill_dir_token_replaced_globally() {
        let desc = SkillDescriptor {
            body: "${LINGXI_SKILL_DIR}/a and ${LINGXI_SKILL_DIR}/b".into(),
            skill_root: Some(std::path::PathBuf::from("/r")),
            ..prompt_desc("dir")
        };
        let tool = SkillTool::with_loader(
            shell_test_ctx(dummy_out()),
            Arc::new(FixedLoader(Some(desc))),
        );
        let out = tool
            .call(source_json!({"skill": "dir"}), fresh_ctx(), fresh_tx())
            .await
            .expect("ok");
        assert_eq!(
            injected_text(&out),
            "Base directory for this skill: /r\n\n/r/a and /r/b"
        );
    }

    /// Step 3: `${LINGXI_SESSION_ID}` is replaced with the session id (always,
    /// when one is wired) — including every occurrence.
    #[tokio::test]
    async fn session_id_token_replaced() {
        let desc = SkillDescriptor {
            body: "session ${LINGXI_SESSION_ID} = ${LINGXI_SESSION_ID}".into(),
            session_id: Some("sess:abc-123".into()),
            ..prompt_desc("sid")
        };
        let tool = SkillTool::with_loader(
            shell_test_ctx(dummy_out()),
            Arc::new(FixedLoader(Some(desc))),
        );
        let out = tool
            .call(source_json!({"skill": "sid"}), fresh_ctx(), fresh_tx())
            .await
            .expect("ok");
        assert_eq!(injected_text(&out), "session sess:abc-123 = sess:abc-123");
    }

    /// Step 3 (gate): with NO `session_id` wired (hermetic loader), the token is
    /// left untouched rather than substituting an empty string.
    #[tokio::test]
    async fn session_id_token_left_as_is_when_unset() {
        let desc = SkillDescriptor {
            body: "session ${LINGXI_SESSION_ID}".into(),
            session_id: None,
            ..prompt_desc("sid")
        };
        let tool = SkillTool::with_loader(
            shell_test_ctx(dummy_out()),
            Arc::new(FixedLoader(Some(desc))),
        );
        let out = tool
            .call(source_json!({"skill": "sid"}), fresh_ctx(), fresh_tx())
            .await
            .expect("ok");
        assert_eq!(injected_text(&out), "session ${LINGXI_SESSION_ID}");
    }

    /// A `ProcessRunner` that records the embedded shell command it was handed
    /// (the last `args` element, which `SkillShellRunner` fills with the expanded
    /// `-c` command string) so a test can assert WHAT the shell saw.
    struct CapturingProcess {
        seen: std::sync::Mutex<Vec<String>>,
        stdout: String,
    }
    #[async_trait]
    impl lingxi_core::host::process::ProcessRunner for CapturingProcess {
        async fn run(
            &self,
            cmd: &lingxi_core::host::sandbox::SandboxedCommand,
        ) -> Result<ProcessOutput, mobile_linux_api::ProcessError> {
            if let Some(last) = cmd.inner().args.last() {
                self.seen.lock().unwrap().push(last.clone());
            }
            Ok(ProcessOutput {
                stdout: self.stdout.clone(),
                stderr: String::new(),
                exit_code: 0,
                timed_out: false,
            })
        }
        async fn spawn_background(
            &self,
            _cmd: &lingxi_core::host::sandbox::SandboxedCommand,
        ) -> Result<lingxi_core::host::process::ProcessHandle, mobile_linux_api::ProcessError>
        {
            Err(mobile_linux_api::ProcessError::Unsupported)
        }
        async fn kill(
            &self,
            _handle: &lingxi_core::host::process::ProcessHandle,
        ) -> Result<(), mobile_linux_api::ProcessError> {
            Ok(())
        }
        fn is_available(&self) -> bool {
            true
        }
    }

    /// ORDERING: a `!command` block that references `${LINGXI_SESSION_ID}` sees
    /// the SUBSTITUTED value — token replacement (step 3) runs BEFORE the embedded
    /// `!command` shell expansion (step 4). We capture the command string the
    /// shell runner is handed and assert the token is already substituted there.
    #[tokio::test]
    async fn token_substitution_precedes_shell_expansion() {
        let capture = Arc::new(CapturingProcess {
            seen: std::sync::Mutex::new(Vec::new()),
            stdout: "OUT\n".into(),
        });
        let mut ctx = shell_test_ctx(dummy_out());
        ctx.process = capture.clone();
        let desc = SkillDescriptor {
            body: "pre !`echo ${LINGXI_SESSION_ID}` post".into(),
            session_id: Some("sess:zzz".into()),
            ..prompt_desc("ord")
        };
        let tool = SkillTool::with_loader(ctx, Arc::new(FixedLoader(Some(desc))));
        let out = tool
            .call(source_json!({"skill": "ord"}), fresh_ctx(), fresh_tx())
            .await
            .expect("ok");
        // The expanded prompt splices the (trimmed) stdout.
        assert_eq!(injected_text(&out), "pre OUT post");
        // The command the shell actually ran already had the token substituted.
        let seen = capture.seen.lock().unwrap();
        assert_eq!(seen.len(), 2, "snapshot plus embedded command ran");
        let command = seen.last().expect("embedded command");
        assert!(
            command.contains("echo sess:zzz"),
            "shell saw substituted session id, got: {}",
            command
        );
        assert!(
            !command.contains("${LINGXI_SESSION_ID}"),
            "token must be substituted BEFORE shell expansion, got: {}",
            command
        );
    }

    /// Records `wrap`/`cleanup_after_command` calls so a test can prove the
    /// skill `!command` expansion routes through `ctx.sandbox_runner`.
    struct WrapCall {
        command: String,
        bin_shell: Option<String>,
        cwd: Option<std::path::PathBuf>,
    }

    #[derive(Default)]
    struct RecordingSandboxRunner {
        wrap_calls: std::sync::Mutex<Vec<WrapCall>>,
        cleanups: std::sync::atomic::AtomicUsize,
    }

    #[async_trait]
    impl tool_api::SandboxRunner for RecordingSandboxRunner {
        async fn wrap(
            &self,
            command: &str,
            _cfg: &sandbox::runtime_config::SandboxRuntimeConfig,
            _platform: sandbox::runtime_config::Platform,
            bin_shell: Option<&str>,
            cwd: Option<&std::path::Path>,
        ) -> Result<String, sandbox::wrap::SandboxWrapError> {
            self.wrap_calls.lock().unwrap().push(WrapCall {
                command: command.to_string(),
                bin_shell: bin_shell.map(ToString::to_string),
                cwd: cwd.map(std::path::Path::to_path_buf),
            });
            Ok(format!("WRAPPED::{command}"))
        }

        async fn cleanup_after_command(&self) {
            self.cleanups
                .fetch_add(1, std::sync::atomic::Ordering::SeqCst);
        }
    }

    /// The embedded `!command` shell expansion routes through the injected
    /// `ctx.sandbox_runner` (not the sync free fn): the runner's wrapped output
    /// is what gets spawned, it sees the resolved shell + workspace cwd, and
    /// `cleanup_after_command` runs after the command finishes.
    #[tokio::test]
    async fn shell_expansion_routes_through_injected_runner_and_cleans_up() {
        let capture = Arc::new(CapturingProcess {
            seen: std::sync::Mutex::new(Vec::new()),
            stdout: "OUT\n".into(),
        });
        let runner = Arc::new(RecordingSandboxRunner::default());
        let mut ctx = shell_test_ctx(dummy_out());
        ctx.process = capture.clone();
        // Force the Sandbox branch: a non-empty, non-excluded command runs
        // through the wrap whenever the host has a working sandbox backend.
        ctx.sandbox_available = true;
        ctx.session_cwd
            .swap(std::path::PathBuf::from("/tmp"), ctx.trusted_dirs());
        ctx.sandbox_runner = runner.clone();

        let desc = SkillDescriptor {
            body: "pre !`echo hi` post".into(),
            ..prompt_desc("wrap")
        };
        let tool = SkillTool::with_loader(ctx, Arc::new(FixedLoader(Some(desc))));
        tool.call(source_json!({"skill": "wrap"}), fresh_ctx(), fresh_tx())
            .await
            .expect("ok");

        let calls = runner.wrap_calls.lock().unwrap();
        assert_eq!(calls.len(), 1, "wrap should be called once");
        let call = &calls[0];
        assert!(
            call.command.contains("echo hi"),
            "runner received the embedded command, got: {}",
            call.command
        );
        assert_eq!(
            call.bin_shell.as_deref(),
            Some(crate::prompt_shell::resolve_shell_path())
        );
        assert_eq!(call.cwd.as_deref(), Some(std::path::Path::new("/tmp")));

        // The runner's wrapped output (sentinel) is what actually got spawned.
        let seen = capture.seen.lock().unwrap();
        assert_eq!(seen.len(), 2, "snapshot plus embedded command ran");
        let command = seen.last().expect("embedded command");
        assert!(
            command.contains("WRAPPED::"),
            "the runner's wrapped command must be spawned, got: {}",
            command
        );
        drop(seen);

        assert_eq!(
            runner.cleanups.load(std::sync::atomic::Ordering::SeqCst),
            1,
            "cleanup_after_command must be invoked once"
        );
    }

    #[tokio::test]
    async fn file_backed_body_without_tokens_still_gets_base_directory_prefix() {
        let body = "Plain body $ARGUMENTS, no tokens here at all.";
        let desc = SkillDescriptor {
            body: body.into(),
            skill_root: Some(std::path::PathBuf::from("/r")),
            session_id: Some("sess:abc".into()),
            ..prompt_desc("plain")
        };
        let expected = command_api::substitute_arguments_source(
            &Utf16JsonProjection::from(body),
            &Utf16JsonProjection::from("X"),
            true,
            &[],
        )
        .unwrap()
        .string_at("")
        .unwrap()
        .to_owned();
        let tool = SkillTool::with_loader(
            shell_test_ctx(dummy_out()),
            Arc::new(FixedLoader(Some(desc))),
        );
        let out = tool
            .call(
                source_json!({"skill": "plain", "args": "X"}),
                fresh_ctx(),
                fresh_tx(),
            )
            .await
            .expect("ok");
        assert_eq!(
            injected_text(&out),
            format!("Base directory for this skill: /r\n\n{expected}")
        );
    }

    // ========================================================================
    // SKILLEXEC.6 sandbox parity: the embedded `!command` runner mirrors
    // `BashTool::call`'s `should_use_sandbox` + `wrap_with_sandbox` decision.
    // ========================================================================

    /// The platform wrapper prefix `wrap_with_sandbox` emits, so the assertions
    /// below stay host-agnostic: `sandbox-exec -f` on macOS, `bwrap ` elsewhere.
    fn sandbox_wrap_prefix() -> &'static str {
        if cfg!(target_os = "macos") {
            "sandbox-exec -f"
        } else {
            "bwrap "
        }
    }

    /// With a config that WOULD sandbox (`sandbox_available = true`, default
    /// permission mode, classifier `None` so the trusted-safe shortcut never
    /// fires), the embedded command is run through `wrap_with_sandbox` — the
    /// spawned command string is the WRAPPED form, exactly as `BashTool::call`
    /// would produce. We capture the spawned `-c -l` payload and assert it is the
    /// platform sandbox wrapper, with the original command nested inside.
    #[tokio::test]
    async fn embedded_command_is_sandbox_wrapped_when_decision_says_sandbox() {
        let capture = Arc::new(CapturingProcess {
            seen: std::sync::Mutex::new(Vec::new()),
            stdout: "OUT\n".into(),
        });
        let mut ctx = shell_test_ctx(dummy_out());
        ctx.process = capture.clone();
        // Flip the one input that moves the decision from NoSandbox -> Sandbox:
        // a working sandbox backend. (Default mode + classifier `None` means the
        // trusted-safe shortcut is skipped, so the decision lands on Sandbox.)
        ctx.sandbox_available = true;
        let desc = SkillDescriptor {
            body: "pre !`echo hi` post".into(),
            ..prompt_desc("sbx")
        };
        let tool = SkillTool::with_loader(ctx, Arc::new(FixedLoader(Some(desc))));
        let out = tool
            .call(source_json!({"skill": "sbx"}), fresh_ctx(), fresh_tx())
            .await
            .expect("ok");
        // The splice still works (the stub stdout is "OUT").
        assert_eq!(injected_text(&out), "pre OUT post");
        let seen = capture.seen.lock().unwrap();
        assert_eq!(seen.len(), 2, "snapshot plus embedded command ran");
        let command = seen.last().expect("embedded command");
        // The spawned payload is the platform sandbox wrapper — NOT the raw
        // command — proving the should_use_sandbox + wrap_with_sandbox path ran.
        assert!(
            command.starts_with(sandbox_wrap_prefix()),
            "embedded command must be sandbox-wrapped, got: {}",
            command
        );
        // The original command (plus the BASH.1 extglob guard) is nested inside
        // the wrapper's `/bin/sh -c '…'` payload.
        assert!(
            command.contains("echo hi"),
            "wrapped command should still carry the original command, got: {}",
            command
        );
    }

    /// With the DEFAULT config (`sandbox_available = false`), the decision is
    /// `NoSandbox`, so the embedded command is run UNWRAPPED — byte-identical to
    /// the pre-sandbox-parity behavior. The spawned payload is the bare
    /// extglob-guarded command, never the platform wrapper.
    #[tokio::test]
    async fn embedded_command_is_unwrapped_when_decision_says_no_sandbox() {
        let capture = Arc::new(CapturingProcess {
            seen: std::sync::Mutex::new(Vec::new()),
            stdout: "OUT\n".into(),
        });
        // shell_test_ctx defaults: sandbox_available = false -> NoSandbox.
        let mut ctx = shell_test_ctx(dummy_out());
        ctx.process = capture.clone();
        let desc = SkillDescriptor {
            body: "pre !`echo hi` post".into(),
            ..prompt_desc("nosbx")
        };
        let tool = SkillTool::with_loader(ctx, Arc::new(FixedLoader(Some(desc))));
        let out = tool
            .call(source_json!({"skill": "nosbx"}), fresh_ctx(), fresh_tx())
            .await
            .expect("ok");
        assert_eq!(injected_text(&out), "pre OUT post");
        let seen = capture.seen.lock().unwrap();
        assert_eq!(seen.len(), 2, "snapshot plus embedded command ran");
        let command = seen.last().expect("embedded command");
        // No sandbox wrapper — the raw command runs directly.
        assert!(
            !command.starts_with(sandbox_wrap_prefix()),
            "NoSandbox path must run the command unwrapped, got: {}",
            command
        );
        assert!(
            command.contains("echo hi"),
            "unwrapped command should be the raw command, got: {}",
            command
        );
    }

    // ========================================================================
    // Parity: prompt() byte-exact comparison (cc 2.1.220 `Xh_`, bytes 231510981)
    // ========================================================================

    /// The `prompt()` string must match the 2.1.220 binary's Skill tool
    /// description exactly (byte-for-byte, verified against a live-captured
    /// request body). This test locks the full text so any future change
    /// requires an explicit binary re-audit. The background-return sentence is
    /// the cc 2.1.218 addition; the template literal ends with a trailing
    /// newline that IS sent on the wire.
    #[tokio::test]
    async fn prompt_is_binary_faithful() {
        let tool = SkillTool::new(shell_test_ctx(dummy_out()));
        let p = tool.prompt(&PromptOptions::default()).await;
        let expected = "Invoke a skill.\n\nA skill is a packaged set of instructions the \
user or project has set up for a particular kind of task (deploy steps, a review \
checklist, a repo-specific workflow). Available skills appear in a system-reminder \
listing with one-line descriptions. When the task at hand is one a listed skill covers, \
call this tool first — the skill's instructions load into the turn for you to follow in \
place of your default approach; some skills instead run in a subagent and return the \
finished result. A skill that runs in the background returns only the agent's name — its \
result arrives later as a task notification, so don't wait on it or invoke it again in \
the meantime. Users may also ask for one by name (`/<name>`, or \"slash command\"); \
that's a request to invoke it.\n\n- `skill`: exact name from the listing, no leading \
slash. Plugin skills use `plugin:skill`. Directory-scoped skills are listed with a path \
prefix (`apps/web:deploy`); when both scoped and unscoped variants of a name exist, pick \
the one whose directory contains the files you're working on (most specific wins; \
unscoped otherwise).\n- `args`: optional arguments to pass through.\n\nOnly names from \
the listing (or that the user typed explicitly) are valid. Built-in CLI commands \
(`/help`, `/clear`, …) aren't skills. If a `<command-name>` block is already present \
this turn, the skill is loaded — follow it directly rather than calling again.\n";
        assert_eq!(
            p.string_at(""),
            Some(expected),
            "Skill tool prompt must be byte-exact vs 2.1.220"
        );
    }

    // P2-12 / `zSr`: a successful skill invocation records the skill in the
    // process-global invoked-skill registry (main thread → key `":{name}"`), so
    // its content can be re-injected after a compaction (`rRg`).
    #[tokio::test]
    async fn invocation_registers_skill_in_invoked_registry() {
        let _g = compaction::invoked_skills::TEST_LOCK
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        compaction::invoked_skills::reset_for_test();

        let desc = SkillDescriptor {
            skill_root: Some(std::path::PathBuf::from("/skills/regtest")),
            ..prompt_desc("regtest")
        };
        let tool = SkillTool::with_loader(
            shell_test_ctx(dummy_out()),
            Arc::new(FixedLoader(Some(desc))),
        );
        tool.call(source_json!({"skill": "regtest"}), fresh_ctx(), fresh_tx())
            .await
            .expect("ok");

        // The registry now carries the main-thread row keyed `":regtest"` with
        // the expanded skill content the model received (`body here`).
        let skill_name = Utf16JsonProjection::from("regtest");
        let main_thread_key = compaction::invoked_skills::registry_key(
            compaction::invoked_skills::InvokedSkillScopeRef::new(None, None),
            &skill_name,
        );
        let content = compaction::invoked_skills::content_for_test(&main_thread_key)
            .expect("skill registered under main-thread key");
        assert!(
            content
                .string_at("")
                .is_some_and(|text| text.contains("body here")),
            "registered content must carry the expanded skill body; got: {content:?}"
        );
        // A skill NOT invoked leaves no row.
        let other_key = compaction::invoked_skills::registry_key(
            compaction::invoked_skills::InvokedSkillScopeRef::new(None, None),
            &Utf16JsonProjection::from("other"),
        );
        assert!(compaction::invoked_skills::content_for_test(&other_key).is_none());

        compaction::invoked_skills::reset_for_test();
    }

    #[tokio::test]
    #[allow(clippy::await_holding_lock)]
    async fn invocation_registry_preserves_raw_skill_name_and_expanded_prompt_units() {
        let _g = compaction::invoked_skills::TEST_LOCK
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        compaction::invoked_skills::reset_for_test();

        let name_units = vec![0xd800];
        let skill_name = Utf16JsonProjection::root_string(
            String::from_utf16_lossy(&name_units),
            name_units.clone(),
        )
        .expect("raw UTF-16 skill name");
        let prompt_units = vec![u16::from(b'R'), 0xdfff, u16::from(b'!')];
        let args = Utf16JsonProjection::root_string(
            String::from_utf16_lossy(&prompt_units),
            prompt_units.clone(),
        )
        .expect("raw UTF-16 skill argument");
        let input = Utf16JsonProjection::object(vec![
            ("skill".to_owned(), skill_name.clone()),
            ("args".to_owned(), args),
        ])
        .expect("raw skill input");
        let descriptor = SkillDescriptor {
            body: "$ARGUMENTS".into(),
            ..prompt_desc("raw-name")
        };
        let tool = SkillTool::with_loader(
            shell_test_ctx(dummy_out()),
            Arc::new(FixedLoader(Some(descriptor))),
        );
        let result = tool
            .call(input, fresh_ctx(), fresh_tx())
            .await
            .expect("raw skill invocation");

        let key = compaction::invoked_skills::registry_key(
            compaction::invoked_skills::InvokedSkillScopeRef::new(None, None),
            &skill_name,
        );
        let registered = compaction::invoked_skills::content_for_test(&key)
            .expect("raw skill invocation is registered");
        assert_eq!(registered.string_units(""), Some(prompt_units));
        assert_eq!(
            result
                .data
                .at_pointer("/commandName")
                .and_then(|value| value.code_units())
                .map(|units| units.to_vec()),
            Some(name_units)
        );

        compaction::invoked_skills::reset_for_test();
    }

    #[tokio::test]
    #[allow(clippy::await_holding_lock)]
    async fn invocation_registers_skill_under_session_and_agent_scope() {
        let _g = compaction::invoked_skills::TEST_LOCK
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        compaction::invoked_skills::reset_for_test();

        let desc = SkillDescriptor {
            skill_root: Some(std::path::PathBuf::from("/skills/regtest")),
            ..prompt_desc("regtest")
        };
        let session_id = SessionId::nil();
        let agent_id = AgentId::nil();
        let mut builtin_ctx = shell_test_ctx(dummy_out());
        builtin_ctx.session_id = Some(session_id);
        let tool = SkillTool::with_loader(builtin_ctx, Arc::new(FixedLoader(Some(desc))));
        let mut call_ctx = fresh_ctx();
        call_ctx.agent_id = Some(agent_id);

        tool.call(source_json!({"skill": "regtest"}), call_ctx, fresh_tx())
            .await
            .expect("ok");

        let scoped_key = compaction::invoked_skills::registry_key(
            compaction::invoked_skills::InvokedSkillScopeRef::new(
                Some(&session_id.to_string()),
                Some(&agent_id.to_string()),
            ),
            &Utf16JsonProjection::from("regtest"),
        );
        let content = compaction::invoked_skills::content_for_test(&scoped_key)
            .expect("skill registered under scoped key");
        assert!(
            content
                .string_at("")
                .is_some_and(|text| text.contains("body here")),
            "registered content must carry the expanded skill body; got: {content:?}"
        );
        let main_thread_key = compaction::invoked_skills::registry_key(
            compaction::invoked_skills::InvokedSkillScopeRef::new(None, None),
            &Utf16JsonProjection::from("regtest"),
        );
        assert!(
            compaction::invoked_skills::content_for_test(&main_thread_key).is_none(),
            "scoped registration must not leak into the legacy main-thread key"
        );

        compaction::invoked_skills::reset_for_test();
    }

    #[tokio::test]
    #[allow(clippy::await_holding_lock)]
    async fn invocation_prefers_the_live_call_session_over_the_construction_session() {
        let _g = compaction::invoked_skills::TEST_LOCK
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        compaction::invoked_skills::reset_for_test();

        let desc = SkillDescriptor {
            skill_root: Some(std::path::PathBuf::from("/skills/regtest")),
            ..prompt_desc("regtest")
        };
        let stale_session_id = SessionId::new();
        let live_session_id = SessionId::new();
        let mut builtin_ctx = shell_test_ctx(dummy_out());
        builtin_ctx.session_id = Some(stale_session_id);
        let tool = SkillTool::with_loader(builtin_ctx, Arc::new(FixedLoader(Some(desc))));
        let mut call_ctx = fresh_ctx();
        call_ctx.session = Some(Arc::new(tokio::sync::Mutex::new(
            lingxi_core::SessionState::empty(live_session_id, "model".into()),
        )));

        tool.call(source_json!({"skill": "regtest"}), call_ctx, fresh_tx())
            .await
            .expect("ok");

        let live_key = compaction::invoked_skills::registry_key(
            compaction::invoked_skills::InvokedSkillScopeRef::new(
                Some(&live_session_id.to_string()),
                None,
            ),
            &Utf16JsonProjection::from("regtest"),
        );
        let stale_key = compaction::invoked_skills::registry_key(
            compaction::invoked_skills::InvokedSkillScopeRef::new(
                Some(&stale_session_id.to_string()),
                None,
            ),
            &Utf16JsonProjection::from("regtest"),
        );
        assert!(compaction::invoked_skills::content_for_test(&live_key).is_some());
        assert!(compaction::invoked_skills::content_for_test(&stale_key).is_none());

        compaction::invoked_skills::reset_for_test();
    }
}

/// `context: fork` end-to-end through `SkillTool::call`.
#[cfg(test)]
mod fork_dispatch_tests {
    use super::*;
    use lingxi_core::host::budget::{BudgetEnforcerHandle, BudgetError};
    use lingxi_core::host::subagent_spawn::{
        AsyncLaunch, SubagentInheritance, SubagentListingEntry, SubagentResult, SubagentSpawnError,
        SubagentSpawnRequest, SubagentSpawner, SubagentUsage,
    };
    use lingxi_core::host::task_registry::{
        TaskCreateInput, TaskListFilter, TaskRecord, TaskRegistryError, TaskRegistryHandle,
        TaskUpdatePatch,
    };
    use mobile_linux_api::ProcessOutput;
    use std::sync::Arc;
    use tool_api::test_support::{fresh_ctx, fresh_tx, shell_test_ctx};

    fn out() -> ProcessOutput {
        ProcessOutput {
            stdout: String::new(),
            stderr: String::new(),
            exit_code: 0,
            timed_out: false,
        }
    }

    struct Loader(Option<SkillDescriptor>);
    #[async_trait]
    impl SkillLoader for Loader {
        async fn load(
            &self,
            _name: &Utf16JsonProjection,
        ) -> Result<Option<SkillDescriptor>, ToolError> {
            Ok(self.0.clone())
        }
    }

    /// Records the request `spawn_async` received; the sync `spawn` is
    /// unreachable on this path and says so.
    #[derive(Default)]
    struct RecordingSpawner {
        seen: std::sync::Mutex<Option<SubagentSpawnRequest>>,
        /// Canned final content for the SYNCHRONOUS fork path.
        sync_content: Option<Utf16JsonProjection>,
    }
    #[async_trait]
    impl SubagentSpawner for RecordingSpawner {
        async fn spawn(
            &self,
            request: SubagentSpawnRequest,
            _i: SubagentInheritance,
        ) -> Result<SubagentResult, SubagentSpawnError> {
            *self.seen.lock().unwrap() = Some(request);
            let content = self
                .sync_content
                .clone()
                .unwrap_or_else(|| source_json!([]));
            Ok(SubagentResult::Completed {
                handback: None,
                agent_id: lingxi_core::types::AgentId::nil(),
                content,
                usage: Default::default(),
                total_tool_use_count: 0,
                total_duration_ms: 0,
                total_tokens: 0,
                assistant_message_count: 0,
                response_char_count: 0,
                last_request_id: None,
                cumulative_usage: SubagentUsage::default(),
                usage_complete: true,
            })
        }
        async fn agent_listing(&self) -> Vec<SubagentListingEntry> {
            vec![]
        }
        async fn spawn_async(
            &self,
            request: SubagentSpawnRequest,
            _i: SubagentInheritance,
        ) -> Result<AsyncLaunch, SubagentSpawnError> {
            *self.seen.lock().unwrap() = Some(request);
            Ok(AsyncLaunch {
                agent_id: lingxi_core::types::AgentId::nil(),
                output_file: "/tmp/a.output".into(),
            })
        }
    }

    /// The fork path reads the live task list (duplicate guard) and the spawn
    /// counter (cap); nothing else.
    struct StubRegistry {
        tasks: Vec<TaskRecord>,
        spawns: std::sync::atomic::AtomicU64,
    }
    impl StubRegistry {
        fn new(tasks: Vec<TaskRecord>) -> Self {
            Self {
                tasks,
                spawns: std::sync::atomic::AtomicU64::new(0),
            }
        }
    }
    #[async_trait]
    impl TaskRegistryHandle for StubRegistry {
        async fn create(&self, _i: TaskCreateInput) -> Result<TaskRecord, TaskRegistryError> {
            Err(TaskRegistryError::Internal("unused".into()))
        }
        async fn get(&self, _id: &str) -> Result<Option<TaskRecord>, TaskRegistryError> {
            Ok(None)
        }
        async fn list(&self, _f: TaskListFilter) -> Result<Vec<TaskRecord>, TaskRegistryError> {
            Ok(self.tasks.clone())
        }
        async fn update(
            &self,
            _id: &str,
            _p: TaskUpdatePatch,
        ) -> Result<TaskRecord, TaskRegistryError> {
            Err(TaskRegistryError::Internal("unused".into()))
        }
        async fn set_status(&self, _id: &str, _s: &str) -> Result<TaskRecord, TaskRegistryError> {
            Err(TaskRegistryError::Internal("unused".into()))
        }
        async fn kill(&self, _id: &str) -> Result<TaskRecord, TaskRegistryError> {
            Err(TaskRegistryError::Internal("unused".into()))
        }
        async fn output(
            &self,
            _id: &str,
            _offset: Option<u64>,
        ) -> Result<lingxi_core::host::task_registry::TaskOutputChunk, TaskRegistryError> {
            Err(TaskRegistryError::Internal("unused".into()))
        }
        fn get_total_agent_spawns(&self) -> u64 {
            self.spawns.load(std::sync::atomic::Ordering::SeqCst)
        }
        fn increment_total_agent_spawns(&self) {
            self.spawns
                .fetch_add(1, std::sync::atomic::Ordering::SeqCst);
        }
        fn release_total_agent_spawn_reservation(&self) {
            self.spawns
                .fetch_sub(1, std::sync::atomic::Ordering::SeqCst);
        }
    }

    struct NoBudget;
    #[async_trait]
    impl BudgetEnforcerHandle for NoBudget {
        async fn check_and_charge(&self, _n: u64) -> Result<(), BudgetError> {
            Ok(())
        }
        async fn snapshot_total_nano_usd(&self) -> u64 {
            0
        }
    }

    fn fork_desc(name: &str, background: Option<bool>) -> SkillDescriptor {
        SkillDescriptor {
            name: name.into(),
            description: "a forking skill".into(),
            body: "do the review".into(),
            context: Some("fork".into()),
            background,
            agent: Some("code-reviewer".into()),
            ..SkillDescriptor::default()
        }
    }

    fn inline_desc(name: &str) -> SkillDescriptor {
        SkillDescriptor {
            name: name.into(),
            description: "a skill".into(),
            body: "body here".into(),
            ..SkillDescriptor::default()
        }
    }

    fn tool_with(
        spawner: Arc<RecordingSpawner>,
        desc: SkillDescriptor,
        tasks: Vec<TaskRecord>,
    ) -> SkillTool {
        let mut ctx = shell_test_ctx(out());
        ctx.subagent_spawner = Some(spawner as Arc<dyn SubagentSpawner>);
        ctx.task_registry = Some(Arc::new(StubRegistry::new(tasks)));
        ctx.budget_enforcer = Some(Arc::new(NoBudget));
        SkillTool::with_loader(ctx, Arc::new(Loader(Some(desc))))
    }

    fn ctx_with_registry() -> ToolUseContext {
        let mut ctx = fresh_ctx();
        ctx.subagent_registry = Some(Arc::new(tool_api::ToolRegistry::new()));
        ctx
    }

    /// A `context: fork` skill launches a BACKGROUND subagent instead of
    /// expanding into this conversation, and reports it in claude's shape.
    #[tokio::test]
    async fn a_forking_skill_launches_a_background_subagent() {
        let spawner: Arc<RecordingSpawner> = Arc::default();
        let tool = tool_with(spawner.clone(), fork_desc("review", None), vec![]);
        let mut caller = ctx_with_registry();
        caller.options.main_loop_model = "gpt-4o".into();
        caller.options.model_profile = Some("openai".into());
        let res = tool
            .call(source_json!({"skill": "review"}), caller, fresh_tx())
            .await
            .expect("fork dispatch succeeds");

        assert_eq!(source_text(&res.data, "/status"), Some("forked"));
        assert_eq!(
            res.data.at_pointer("/background").and_then(|v| v.as_bool()),
            Some(true)
        );
        assert_eq!(source_text(&res.data, "/commandName"), Some("review"));
        assert_eq!(
            source_text(&res.data, "/result"),
            Some("Running in the background as @review")
        );
        assert_eq!(
            res.model_content
                .as_ref()
                .and_then(|value| value.string_at("")),
            Some(
                "Skill \"review\" launched (forked execution, running in the background).\n\n\
                 Running in the background as @review"
            )
        );
        // The whole point of forking: nothing lands in this conversation.
        assert!(
            res.new_messages.is_empty(),
            "a forked skill injects no messages into the parent"
        );

        let req = spawner.seen.lock().unwrap().clone().expect("spawned");
        assert_eq!(
            req.forked_skill_name
                .as_ref()
                .and_then(|name| name.string_at("")),
            Some("review")
        );
        assert_eq!(
            req.forked_skill_attribution
                .as_ref()
                .and_then(|name| name.string_at("")),
            Some("review")
        );
        assert_eq!(req.subagent_type, "code-reviewer");
        assert_eq!(req.parent_model_override.as_deref(), Some("gpt-4o"));
        assert_eq!(req.parent_model_profile_override.as_deref(), Some("openai"));
        assert!(req.model_profile.is_none());
        assert!(req.run_in_background);
        // The `SendMessage` handle is the skill name, so `@review` resolves.
        assert_eq!(req.name.as_deref(), Some("review"));
    }

    #[tokio::test]
    async fn a_forked_skill_keeps_unpaired_utf16_name_as_its_identity() {
        let mut units = "skill-".encode_utf16().collect::<Vec<_>>();
        units.push(0xd800);
        units.extend("-probe".encode_utf16());
        let exact_name =
            Utf16JsonProjection::root_string("skill-�-probe".to_owned(), units.clone())
                .expect("exact UTF-16 skill name");
        let desc = SkillDescriptor {
            name: exact_name.clone(),
            description: "a forking skill".into(),
            body: "do the work".into(),
            context: Some("fork".into()),
            ..SkillDescriptor::default()
        };
        let spawner: Arc<RecordingSpawner> = Arc::default();
        let tool = tool_with(spawner.clone(), desc, vec![]);
        let input = Utf16JsonProjection::object(vec![("skill".into(), exact_name.clone())])
            .expect("source-preserving skill input");
        let result = tool
            .call(input, ctx_with_registry(), fresh_tx())
            .await
            .expect("surrogate-containing name still launches as a fork");

        assert_eq!(source_text(&result.data, "/status"), Some("forked"));
        let request = spawner.seen.lock().unwrap().clone().expect("spawned");
        assert_eq!(
            request
                .forked_skill_name
                .as_ref()
                .and_then(|name| name.string_units("")),
            Some(units.clone())
        );
        assert_eq!(
            request
                .forked_skill_attribution
                .as_ref()
                .and_then(|name| name.string_units("")),
            Some(units)
        );
        // The string-only task handle is for display/routing. Fork identity
        // remains the exact source projection above.
        assert_eq!(request.name.as_deref(), Some("skill-�-probe"));
    }

    /// One live fork per skill: a second invocation while the first is still
    /// running falls back to inline instead of racing it over the same scoping
    /// record and the same `SendMessage` name.
    #[tokio::test]
    async fn a_second_invocation_of_a_live_fork_runs_inline() {
        let live = TaskRecord {
            task_id: "a00000001".into(),
            task_type: "local_agent".into(),
            status: "running".into(),
            forked_skill_name: Some("review".into()),
            agent_facts: None,
            ..Default::default()
        };
        let spawner: Arc<RecordingSpawner> = Arc::default();
        let tool = tool_with(spawner.clone(), fork_desc("review", None), vec![live]);
        let res = tool
            .call(
                source_json!({"skill": "review"}),
                ctx_with_registry(),
                fresh_tx(),
            )
            .await
            .expect("inline");
        assert_eq!(source_text(&res.data, "/status"), Some("inline"));
        assert!(spawner.seen.lock().unwrap().is_none(), "no second spawn");
    }

    /// An ordinary (non-forking) skill is untouched by any of this.
    #[tokio::test]
    async fn an_inline_skill_is_unaffected() {
        let spawner: Arc<RecordingSpawner> = Arc::default();
        let tool = tool_with(spawner.clone(), inline_desc("commit"), vec![]);
        let res = tool
            .call(
                source_json!({"skill": "commit"}),
                ctx_with_registry(),
                fresh_tx(),
            )
            .await
            .expect("inline");
        assert_eq!(source_text(&res.data, "/status"), Some("inline"));
        assert!(spawner.seen.lock().unwrap().is_none());
    }

    /// A host with no spawner wired falls back to inline rather than failing —
    /// the skill still runs, just here.
    #[tokio::test]
    async fn no_spawner_falls_back_to_inline() {
        let tool = SkillTool::with_loader(
            shell_test_ctx(out()),
            Arc::new(Loader(Some(fork_desc("review", None)))),
        );
        let res = tool
            .call(source_json!({"skill": "review"}), fresh_ctx(), fresh_tx())
            .await
            .expect("inline");
        assert_eq!(source_text(&res.data, "/status"), Some("inline"));
    }

    /// Records every `set_command_input_denies` the Skill tool makes.
    #[derive(Default)]
    struct RecordingDenyGate {
        calls: std::sync::Mutex<
            Vec<(
                Vec<lingxi_core::types::utf16_json::Utf16JsonProjection>,
                bool,
            )>,
        >,
    }
    #[async_trait::async_trait]
    impl lingxi_core::host::permission_gate::PermissionGate for RecordingDenyGate {
        async fn check(
            &self,
            _name: &str,
            _input: &lingxi_core::types::utf16_json::Utf16JsonProjection,
        ) -> lingxi_core::host::permission_gate::PermissionDecision {
            lingxi_core::host::permission_gate::PermissionDecision::Allow
        }
        fn set_command_input_denies(
            &self,
            specs: &[lingxi_core::types::utf16_json::Utf16JsonProjection],
            union: bool,
        ) {
            self.calls
                .lock()
                .unwrap_or_else(|e| e.into_inner())
                .push((specs.to_vec(), union));
        }
    }

    /// An INLINE skill narrows THIS conversation — upstream
    /// `Tbt(o.setToolPermissionContext, T, "union")`. The fork path (below)
    /// scopes a different pool, so without this the field was parsed and then
    /// ignored for every skill that does not declare `context: fork`.
    #[tokio::test]
    async fn an_inline_skill_unions_its_disallowed_tools_into_this_session() {
        let gate = Arc::new(RecordingDenyGate::default());
        let mut desc = inline_desc("tighten");
        desc.disallowed_tools = vec!["Bash".into(), "Write".into()];
        let mut ctx = shell_test_ctx(out());
        ctx.permission_gate = Some(gate.clone());
        let tool = SkillTool::with_loader(ctx, Arc::new(Loader(Some(desc))));
        tool.call(source_json!({"skill": "tighten"}), fresh_ctx(), fresh_tx())
            .await
            .expect("inline skill runs");
        assert_eq!(
            gate.calls.lock().unwrap().clone(),
            vec![(vec!["Bash".into(), "Write".into()], true)],
            "an inline skill must UNION its denies onto the current input's"
        );
    }

    /// A skill that declares nothing must not touch the permission context at
    /// all — publishing an empty union would be harmless, but publishing
    /// anything here at all would mean the call is not reading the descriptor.
    #[tokio::test]
    async fn an_inline_skill_with_no_denies_leaves_the_context_alone() {
        let gate = Arc::new(RecordingDenyGate::default());
        let mut ctx = shell_test_ctx(out());
        ctx.permission_gate = Some(gate.clone());
        let tool = SkillTool::with_loader(ctx, Arc::new(Loader(Some(inline_desc("plain")))));
        tool.call(source_json!({"skill": "plain"}), fresh_ctx(), fresh_tx())
            .await
            .expect("inline skill runs");
        assert!(gate.calls.lock().unwrap().is_empty());
    }

    /// The skill's own `disallowed-tools` ride onto the spawned agent. Without
    /// this the fork runs with the PARENT's tools — strictly wider than the
    /// scoping the fork exists to narrow, and the launch is the only place it
    /// can be applied.
    #[tokio::test]
    async fn a_forking_skill_scopes_the_agent_to_its_disallowed_tools() {
        let spawner: Arc<RecordingSpawner> = Arc::default();
        let mut desc = fork_desc("review", None);
        desc.disallowed_tools = vec!["Write".into(), "Bash".into()];
        let tool = tool_with(spawner.clone(), desc, vec![]);
        tool.call(
            source_json!({"skill": "review"}),
            ctx_with_registry(),
            fresh_tx(),
        )
        .await
        .expect("fork dispatch succeeds");

        let req = spawner.seen.lock().unwrap().clone().expect("spawned");
        assert_eq!(
            req.additional_disallowed_tools,
            vec!["Write".to_string(), "Bash".to_string()],
            "the skill's disallowed-tools scope the forked agent"
        );
    }

    /// The command denies in force at LAUNCH are snapshotted onto the spawn, so
    /// the scoping record persists them. Deterministic order — `deny_rules` is
    /// keyed by source and a HashMap has no stable iteration order.
    #[tokio::test]
    async fn a_fork_freezes_the_command_denies_in_force_at_launch() {
        use permission::rule::{
            PermissionBehavior, PermissionRule, PermissionRuleSource, PermissionRuleValue,
        };
        let spawner: Arc<RecordingSpawner> = Arc::default();
        let mut ctx = shell_test_ctx(out());
        let mut policy = permission::PermissionPolicy::new(permission::PermissionMode::Default);
        // `alwaysDenyRules.command` is keyed by rule SOURCE, not by tool. The
        // `Command` bucket is frozen WHATEVER tool its rules name — it is the
        // volatile one (`/permissions add`, skill frontmatter), which a resume
        // would otherwise lose. Settings-file buckets are NOT frozen: a resume
        // re-reads them from disk, so freezing them would pin stale copies.
        //
        // This test used to put both rules in `ProjectSettings` and assert that
        // only the `Bash` one was frozen — encoding the very misreading (that
        // `command` names the Bash tool) that this fixture now guards against.
        policy.deny_rules.insert(
            PermissionRuleSource::Command,
            vec![
                PermissionRule {
                    value: PermissionRuleValue::from_rule_string("Bash(rm:*)"),
                    behavior: PermissionBehavior::Deny,
                    source: PermissionRuleSource::Command,
                },
                // A non-Bash rule in the command bucket IS frozen.
                PermissionRule {
                    value: PermissionRuleValue::from_rule_string("Write"),
                    behavior: PermissionBehavior::Deny,
                    source: PermissionRuleSource::Command,
                },
            ],
        );
        // A different SOURCE is not frozen, even for the same tool.
        policy.deny_rules.insert(
            PermissionRuleSource::Settings(lingxi_core::types::SettingsScope::Project),
            vec![PermissionRule {
                value: PermissionRuleValue::from_rule_string("Bash(curl:*)"),
                behavior: PermissionBehavior::Deny,
                source: PermissionRuleSource::Settings(lingxi_core::types::SettingsScope::Project),
            }],
        );
        ctx.permission_policy = Arc::new(policy);
        ctx.subagent_spawner = Some(spawner.clone() as Arc<dyn SubagentSpawner>);
        ctx.task_registry = Some(Arc::new(StubRegistry::new(vec![])));
        ctx.budget_enforcer = Some(Arc::new(NoBudget));
        let tool = SkillTool::with_loader(ctx, Arc::new(Loader(Some(fork_desc("review", None)))));

        tool.call(
            source_json!({"skill": "review"}),
            ctx_with_registry(),
            fresh_tx(),
        )
        .await
        .expect("fork dispatch succeeds");

        let req = spawner.seen.lock().unwrap().clone().expect("spawned");
        assert_eq!(
            req.frozen_command_denies,
            vec!["Bash(rm:*)".to_string(), "Write".to_string()],
            "the whole command SOURCE bucket is frozen, regardless of tool, and \
             nothing from another source is"
        );
    }

    /// `background: false` still FORKS — it runs the subagent to completion
    /// here and returns its answer, under the skill's scoping, with nothing
    /// injected into this conversation. It is not the inline path.
    #[tokio::test]
    async fn a_non_background_fork_runs_the_subagent_synchronously() {
        let spawner = Arc::new(RecordingSpawner {
            seen: std::sync::Mutex::new(None),
            sync_content: Some(source_json!([
                { "type": "text", "text": "first" },
                { "type": "tool_use", "name": "Bash", "input": {} },
                { "type": "text", "text": "second" }
            ])),
        });
        let mut desc = fork_desc("review", Some(false));
        desc.disallowed_tools = vec!["Write".into()];
        let tool = tool_with(spawner.clone(), desc, vec![]);
        let res = tool
            .call(
                source_json!({"skill": "review"}),
                ctx_with_registry(),
                fresh_tx(),
            )
            .await
            .expect("sync fork succeeds");

        assert_eq!(source_text(&res.data, "/status"), Some("forked"));
        assert!(
            res.data.at_pointer("/background").is_none(),
            "the synchronous fork omits the background key"
        );
        assert_eq!(source_text(&res.data, "/result"), Some("first\nsecond"));
        assert_eq!(
            res.model_content
                .as_ref()
                .and_then(|value| value.string_at("")),
            Some("Skill \"review\" completed (forked execution).\n\nResult:\nfirst\nsecond")
        );
        assert!(
            res.new_messages.is_empty(),
            "still a fork: nothing injected"
        );

        let req = spawner.seen.lock().unwrap().clone().expect("spawned");
        assert!(!req.run_in_background);
        assert_eq!(req.additional_disallowed_tools, vec!["Write".to_string()]);
        // Not addressable and not resumable: no SendMessage name, no fork
        // identity on the task index.
        assert!(req.name.is_none());
        assert!(req.forked_skill_name.is_none());
    }

    /// A subagent that produced no text still reports something — claude's
    /// `"Skill execution completed"` default, not an empty result.
    #[tokio::test]
    async fn a_sync_fork_with_no_text_reports_the_default_result() {
        let spawner = Arc::new(RecordingSpawner {
            seen: std::sync::Mutex::new(None),
            sync_content: Some(source_json!([])),
        });
        let tool = tool_with(spawner, fork_desc("review", Some(false)), vec![]);
        let res = tool
            .call(
                source_json!({"skill": "review"}),
                ctx_with_registry(),
                fresh_tx(),
            )
            .await
            .unwrap();
        assert_eq!(
            source_text(&res.data, "/result"),
            Some("Skill execution completed")
        );
    }

    /// A fork that declines to launch must RELEASE its spawn reservation —
    /// otherwise every duplicate invocation of a live fork would silently burn
    /// a slot of the session's agent budget.
    #[tokio::test]
    async fn a_declined_fork_releases_its_spawn_reservation() {
        let live = TaskRecord {
            task_id: "a00000001".into(),
            task_type: "local_agent".into(),
            status: "running".into(),
            forked_skill_name: Some("review".into()),
            agent_facts: None,
            ..Default::default()
        };
        let registry = Arc::new(StubRegistry::new(vec![live]));
        let spawner: Arc<RecordingSpawner> = Arc::default();
        let mut ctx = shell_test_ctx(out());
        ctx.subagent_spawner = Some(spawner.clone() as Arc<dyn SubagentSpawner>);
        ctx.task_registry = Some(registry.clone());
        ctx.budget_enforcer = Some(Arc::new(NoBudget));
        let tool = SkillTool::with_loader(ctx, Arc::new(Loader(Some(fork_desc("review", None)))));

        let res = tool
            .call(
                source_json!({"skill": "review"}),
                ctx_with_registry(),
                fresh_tx(),
            )
            .await
            .expect("inline");
        assert_eq!(source_text(&res.data, "/status"), Some("inline"));
        assert_eq!(
            registry.get_total_agent_spawns(),
            0,
            "the duplicate-loser gave its reservation back"
        );
    }

    /// A launched fork KEEPS its reservation — the slot really was consumed.
    #[tokio::test]
    async fn a_launched_fork_consumes_one_spawn() {
        let registry = Arc::new(StubRegistry::new(vec![]));
        let spawner: Arc<RecordingSpawner> = Arc::default();
        let mut ctx = shell_test_ctx(out());
        ctx.subagent_spawner = Some(spawner.clone() as Arc<dyn SubagentSpawner>);
        ctx.task_registry = Some(registry.clone());
        ctx.budget_enforcer = Some(Arc::new(NoBudget));
        let tool = SkillTool::with_loader(ctx, Arc::new(Loader(Some(fork_desc("review", None)))));

        tool.call(
            source_json!({"skill": "review"}),
            ctx_with_registry(),
            fresh_tx(),
        )
        .await
        .expect("fork");
        assert_eq!(registry.get_total_agent_spawns(), 1);
    }
}

/// `parse_declared_effort` is this port's `Gx`.
///
/// The union upstream validates against is
/// `[enum(low|medium|high|xhigh|max), int().min(1).max(1000)]`, and a value
/// matching neither arm yields `undefined` rather than an error — a skill with
/// a typo'd effort still runs, it just declares none.
#[test]
fn declared_effort_matches_the_oracle_union() {
    use session::forked_skill::Effort;

    for level in ["none", "minimal", "low", "medium", "high", "xhigh", "max"] {
        assert_eq!(
            super::parse_declared_effort(level),
            Some(Effort::Level(level.to_string())),
            "{level} is a named arm"
        );
    }
    for steps in [1_i64, 2, 500, 1000] {
        assert_eq!(
            super::parse_declared_effort(&steps.to_string()),
            Some(Effort::Steps(steps)),
            "{steps} is inside 1..=1000"
        );
    }
    // Outside either arm ⇒ declared nothing usable, NOT an error.
    for bad in ["0", "1001", "-1", "highest", "HIGH", "", "  ", "1.5"] {
        assert_eq!(
            super::parse_declared_effort(bad),
            None,
            "{bad:?} matches neither arm"
        );
    }
    // Surrounding whitespace is not a declaration error.
    assert_eq!(
        super::parse_declared_effort("  high  "),
        Some(Effort::Level("high".to_string()))
    );
}

/// A bare name that is the tail of exactly one plugin-qualified name gets that
/// full name back: `foo` alone does not resolve, but `some-plugin:foo` does,
/// and the miss should say so.
#[test]
fn a_single_suffix_match_is_named() {
    let names = vec!["some-plugin:deploy".into(), "other:unrelated".into()];
    assert_eq!(
        super::unknown_skill_suffix_hint(&"deploy".into(), &names),
        Some(" Did you mean some-plugin:deploy? Invoke it by that full name.".into())
    );
}

/// Several matches are listed instead, with the oracle's em-dash.
#[test]
fn several_suffix_matches_are_listed() {
    let names = vec!["b-plugin:deploy".into(), "a-plugin:deploy".into()];
    assert_eq!(
        super::unknown_skill_suffix_hint(&"deploy".into(), &names),
        Some(" Several skills match that name: a-plugin:deploy, b-plugin:deploy \u{2014} invoke one by its full name.".into())
    );
}

/// Only a `:`-qualified SUFFIX counts. A name that merely contains the request
/// is not a match, or `redeploy` would be offered for `deploy`.
#[test]
fn only_a_colon_qualified_suffix_counts() {
    let names = vec![
        "redeploy".into(),
        "plugin:redeploy".into(),
        "deployment".into(),
    ];
    assert_eq!(
        super::unknown_skill_suffix_hint(&"deploy".into(), &names),
        None
    );
    assert_eq!(
        super::unknown_skill_suffix_hint(&"deploy".into(), &[]),
        None
    );
}

/// Oracle `Fee`. Skill names come from plugins and marketplaces, so they are
/// third-party input; an unsafe one is never quoted back. Upstream refuses
/// rather than sanitising, and ONE bad candidate suppresses the whole hint
/// rather than being quietly dropped from the list.
#[test]
fn an_undisplayable_name_is_never_quoted_back() {
    // The unsafe characters must sit in the PLUGIN part, so each of these is a
    // genuine `:deploy` suffix match that the guard then has to reject. (Put
    // them after the colon and they are not matches at all, and the test passes
    // without exercising the guard.)
    for bad in [
        "ev\u{1b}[2Jil:deploy",
        "<script>:deploy",
        "a\u{2028}b:deploy",
        "a\u{0}b:deploy",
    ] {
        assert_eq!(
            super::unknown_skill_suffix_hint(&"deploy".into(), &[bad.to_string().into()]),
            None,
            "{bad:?} must not be echoed into a user-facing message"
        );
    }
    let mixed = vec!["good:deploy".into(), "<x>:deploy".into()];
    assert_eq!(
        super::unknown_skill_suffix_hint(&"deploy".into(), &mixed),
        None
    );
}

/// The failed request itself is not a suggestion.
#[test]
fn the_requested_name_is_not_offered_back() {
    assert_eq!(
        super::unknown_skill_suffix_hint(&"deploy".into(), &["deploy".into()]),
        None
    );
}

#[test]
fn an_unpaired_surrogate_name_is_preserved_in_a_suggestion() {
    let name_units = [0xd800, u16::from(b':')]
        .into_iter()
        .chain("deploy".encode_utf16())
        .collect::<Vec<_>>();
    let source_name =
        Utf16JsonProjection::root_string(String::from_utf16_lossy(&name_units), name_units.clone())
            .expect("raw root string fixture");
    let suggestion = super::unknown_skill_suffix_hint(&"deploy".into(), &[source_name])
        .expect("the exact source name remains a valid suggestion");
    let mut expected_units = " Did you mean ".encode_utf16().collect::<Vec<_>>();
    expected_units.extend(name_units);
    expected_units.extend("? Invoke it by that full name.".encode_utf16());
    assert_eq!(suggestion.string_units(""), Some(expected_units));
}
