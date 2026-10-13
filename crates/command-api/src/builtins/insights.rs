//! `/insights` command API: injects the completed report returned by a
//! runtime-owned producer. Report discovery, analysis, caching, and HTML output
//! stay behind [`InsightsReportProvider`].

use crate::builtin_support::names::core_description;
use crate::model::{
    BuiltinCommandHandler, CommandResult, NativeCommandPolicyGate, NativeCommandRequirements,
    NativeCommandType, PromptPart, source_concat,
};
use crate::parser::ParsedSlashCommand;
use async_trait::async_trait;
use lingxi_core::types::utf16_json::Utf16JsonProjection;
use std::sync::Arc;

/// Completed source data consumed by Native 2.1.295's `qt` response frame.
///
/// These are separate fields because the Native producer passes `header` and
/// `summaryText` independently and concatenates them without a separator.
#[derive(Debug, Clone)]
pub struct InsightsReport {
    /// Pretty-printed aggregate insights JSON (`JSON.stringify(data, null, 2)`).
    pub insights_json: Utf16JsonProjection,
    /// Native local `file://` URL for the generated timestamped HTML report.
    pub report_url: Utf16JsonProjection,
    /// Filesystem path to the generated timestamped HTML report.
    pub html_path: Utf16JsonProjection,
    /// Filesystem path to the local facets directory.
    pub facets_directory: Utf16JsonProjection,
    /// Native `header` portion of the at-a-glance frame.
    pub summary_header: Utf16JsonProjection,
    /// Native `summaryText` portion of the at-a-glance frame.
    pub summary_text: Utf16JsonProjection,
    /// Optional Native `Mt(recommendation)` line. An empty string is falsey
    /// under the source's JavaScript conditional and is omitted.
    pub auto_mode_recommendation: Option<Utf16JsonProjection>,
}

/// Host facts required before the Native `insights` prompt enters the command
/// pool. `None` is unknown and therefore fails closed for this command.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct InsightsCommandAvailability {
    /// The current workspace exists and satisfies the host's trust rule.
    pub workspace_available: Option<bool>,
    /// Local host policy verdict for Native's `allow_insights` gate.
    pub allow_insights: Option<bool>,
}

impl InsightsCommandAvailability {
    /// Native command availability: both independent facts must be true.
    #[must_use]
    pub const fn is_available(self) -> bool {
        matches!(self.workspace_available, Some(true)) && matches!(self.allow_insights, Some(true))
    }
}

/// Runtime-owned producer for the locally generated Insights report.
///
/// Implementations own the local session scan, facet cache, model analysis,
/// and report file creation. The command API only frames a successful result.
#[async_trait]
pub trait InsightsReportProvider: Send + Sync {
    /// Current workspace and local policy facts used by command discovery and
    /// the final invocation check. Unknown facts remain unavailable.
    fn command_availability(&self) -> InsightsCommandAvailability;

    /// Generate a report for the current host/session context.
    async fn generate_report(&self) -> Result<InsightsReport, Utf16JsonProjection>;
}

/// `/insights` handler bound to a real host report producer.
pub struct InsightsHandler {
    provider: Arc<dyn InsightsReportProvider>,
}

impl std::fmt::Debug for InsightsHandler {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("InsightsHandler").finish_non_exhaustive()
    }
}

impl InsightsHandler {
    /// Construct the handler with its required runtime-owned producer.
    #[must_use]
    pub fn new(provider: Arc<dyn InsightsReportProvider>) -> Self {
        Self { provider }
    }
}

#[async_trait]
impl BuiltinCommandHandler for InsightsHandler {
    async fn handle(&self, _args: &ParsedSlashCommand) -> CommandResult {
        if !self.provider.command_availability().is_available() {
            return CommandResult::Done {
                display: Some("Insights is unavailable for this workspace.".into()),
            };
        }
        match self.provider.generate_report().await {
            Ok(report) => CommandResult::InjectMessage {
                content: render_report(&report),
            },
            Err(display) => CommandResult::Done {
                display: Some(display),
            },
        }
    }

    fn name(&self) -> &str {
        "insights"
    }

    fn description(&self) -> &str {
        core_description("insights")
    }

    fn native_type(&self) -> Option<NativeCommandType> {
        Some(NativeCommandType::Prompt)
    }

    fn disable_model_invocation(&self) -> bool {
        true
    }

    fn is_enabled(&self) -> bool {
        self.provider.command_availability().is_available()
    }

    fn content_length(&self) -> Option<usize> {
        Some(0)
    }

    fn progress_message(&self) -> Option<Utf16JsonProjection> {
        Some("analyzing your sessions".into())
    }

    fn requires(&self) -> Option<NativeCommandRequirements> {
        Some(NativeCommandRequirements {
            workspace: Some(true),
        })
    }

    fn policy_gate(&self) -> Option<NativeCommandPolicyGate> {
        Some(NativeCommandPolicyGate {
            policy: "allow_insights".to_owned(),
        })
    }
}

/// Render the exact Native 2.1.295 `qt` frame while preserving source UTF-16
/// units in every provider-returned field.
fn render_report(report: &InsightsReport) -> Utf16JsonProjection {
    let mut parts = vec![
        PromptPart::Text(
            "The user just ran /insights to generate a usage report analyzing their LingXi sessions.\n\n\
             Here is the full insights data:\n",
        ),
        PromptPart::Source(&report.insights_json),
        PromptPart::Text("\n\nReport URL: "),
        PromptPart::Source(&report.report_url),
        PromptPart::Text("\nHTML file: "),
        PromptPart::Source(&report.html_path),
        PromptPart::Text("\nFacets directory: "),
        PromptPart::Source(&report.facets_directory),
        PromptPart::Text(
            "\n\nAt-a-glance summary (for your context only — the user has not seen any output yet):\n",
        ),
        PromptPart::Source(&report.summary_header),
        PromptPart::Source(&report.summary_text),
        PromptPart::Text(
            "\n\nRespond with exactly the following, and nothing else. Do not add, omit, or reword any line:\n\n\
             Your shareable insights report is ready:\n",
        ),
        PromptPart::Source(&report.report_url),
        PromptPart::Text("\n"),
    ];

    if let Some(recommendation) = report
        .auto_mode_recommendation
        .as_ref()
        .filter(|line| line.string_units("").is_some_and(|units| !units.is_empty()))
    {
        parts.push(PromptPart::Text("\n"));
        parts.push(PromptPart::Source(recommendation));
        parts.push(PromptPart::Text("\n"));
    }
    // The Native template has a literal newline after the optional expression
    // as well as the newline before it, yielding a blank line with no reco and
    // blank lines around a present reco.
    parts.push(PromptPart::Text("\n"));

    parts.push(PromptPart::Text(
        "Want to dig into any section or try one of the suggestions?",
    ));
    source_concat(parts)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::model::BuiltinCommandHandler;
    use std::sync::Arc;

    struct FakeProvider(Result<InsightsReport, Utf16JsonProjection>);

    #[async_trait]
    impl InsightsReportProvider for FakeProvider {
        fn command_availability(&self) -> InsightsCommandAvailability {
            InsightsCommandAvailability {
                workspace_available: Some(true),
                allow_insights: Some(true),
            }
        }

        async fn generate_report(&self) -> Result<InsightsReport, Utf16JsonProjection> {
            self.0.clone()
        }
    }

    fn report(recommendation: Option<&str>) -> InsightsReport {
        InsightsReport {
            insights_json: r#"{"total_sessions":2}"#.into(),
            report_url: "file:///tmp/report-2026-10-12.html".into(),
            html_path: "/tmp/report-2026-10-12.html".into(),
            facets_directory: "/tmp/usage-data/facets".into(),
            summary_header: "HEADER\n".into(),
            summary_text: "SUMMARY".into(),
            auto_mode_recommendation: recommendation.map(Into::into),
        }
    }

    fn args(raw_args: &str) -> ParsedSlashCommand {
        ParsedSlashCommand {
            name: "insights".into(),
            raw_args: raw_args.into(),
            positional_args: vec![],
        }
    }

    fn units(source: &Utf16JsonProjection) -> Vec<u16> {
        source.string_units("").expect("root string units")
    }

    fn source_from_units(code_units: Vec<u16>) -> Utf16JsonProjection {
        let display = String::from_utf16_lossy(&code_units);
        Utf16JsonProjection::root_string(display, code_units).expect("valid exact source string")
    }

    fn contains_units(haystack: &[u16], needle: &[u16]) -> bool {
        haystack
            .windows(needle.len())
            .any(|window| window == needle)
    }

    #[tokio::test]
    async fn renders_native_qt_frame_exactly_and_ignores_raw_args() {
        let handler = InsightsHandler::new(Arc::new(FakeProvider(Ok(report(None)))));
        let result = handler
            .handle(&args("--homespaces arbitrary user text"))
            .await;
        let CommandResult::InjectMessage { content } = result else {
            panic!("expected a completed report prompt, got {result:?}");
        };
        assert_eq!(
            content.string_at(""),
            Some(
                "The user just ran /insights to generate a usage report analyzing their LingXi sessions.\n\n\
                 Here is the full insights data:\n{\"total_sessions\":2}\n\n\
                 Report URL: file:///tmp/report-2026-10-12.html\n\
                 HTML file: /tmp/report-2026-10-12.html\n\
                 Facets directory: /tmp/usage-data/facets\n\n\
                 At-a-glance summary (for your context only — the user has not seen any output yet):\n\
                 HEADER\nSUMMARY\n\n\
                 Respond with exactly the following, and nothing else. Do not add, omit, or reword any line:\n\n\
                 Your shareable insights report is ready:\n\
                 file:///tmp/report-2026-10-12.html\n\n\
                 Want to dig into any section or try one of the suggestions?"
            )
        );
        assert!(
            !content
                .string_at("")
                .expect("valid display text")
                .contains("Additional user input")
        );
    }

    #[tokio::test]
    async fn recommendation_uses_source_truthiness_and_exact_blank_lines() {
        let empty = InsightsHandler::new(Arc::new(FakeProvider(Ok(report(Some(""))))));
        let CommandResult::InjectMessage { content } = empty.handle(&args("ignored")).await else {
            panic!("expected injected report");
        };
        assert!(!content.string_at("").unwrap().contains("\n\n\nWant to dig"));

        let with_line = InsightsHandler::new(Arc::new(FakeProvider(Ok(report(Some(
            "Tip: try auto mode.",
        ))))));
        let CommandResult::InjectMessage { content } = with_line.handle(&args("")).await else {
            panic!("expected injected report");
        };
        assert!(
            content.string_at("").unwrap().contains(
                "file:///tmp/report-2026-10-12.html\n\nTip: try auto mode.\n\nWant to dig"
            )
        );
    }

    #[tokio::test]
    async fn preserves_unpaired_utf16_units_from_provider_fields() {
        let high = 0xd800;
        let mut json_units = r#"{"x":"""#.encode_utf16().collect::<Vec<_>>();
        json_units.push(high);
        json_units.extend(r#""}"#.encode_utf16());
        let mut url_units = "file:///tmp/".encode_utf16().collect::<Vec<_>>();
        url_units.push(high);
        url_units.extend(".html".encode_utf16());
        let report = InsightsReport {
            insights_json: source_from_units(json_units.clone()),
            report_url: source_from_units(url_units.clone()),
            html_path: "/tmp/report.html".into(),
            facets_directory: "/tmp/facets".into(),
            summary_header: "header".into(),
            summary_text: "summary".into(),
            auto_mode_recommendation: None,
        };
        let rendered = render_report(&report);
        let output_units = units(&rendered);
        assert!(contains_units(&output_units, &json_units));
        assert_eq!(
            output_units
                .windows(url_units.len())
                .filter(|window| *window == url_units.as_slice())
                .count(),
            2,
            "reportUrl is present in both Native qt locations without replacement"
        );
    }

    #[tokio::test]
    async fn provider_failure_is_display_only_and_preserves_raw_utf16() {
        let mut error_units = "local scan failed: ".encode_utf16().collect::<Vec<_>>();
        error_units.push(0xd800);
        let handler = InsightsHandler::new(Arc::new(FakeProvider(Err(source_from_units(
            error_units.clone(),
        )))));
        match handler.handle(&args("anything")).await {
            CommandResult::Done {
                display: Some(message),
            } => {
                assert_eq!(units(&message), error_units);
            }
            other => panic!("provider failure must not inject a success frame: {other:?}"),
        }
    }

    #[test]
    fn native_command_metadata_matches_the_fixed_prompt_object() {
        let handler = InsightsHandler::new(Arc::new(FakeProvider(Ok(report(None)))));
        assert_eq!(handler.name(), "insights");
        assert_eq!(handler.description(), core_description("insights"));
        assert_eq!(handler.native_type(), Some(NativeCommandType::Prompt));
        assert!(!handler.disable_non_interactive());
        assert!(!handler.supports_non_interactive());
        assert!(handler.disable_model_invocation());
        assert_eq!(handler.content_length(), Some(0));
        assert_eq!(
            handler.progress_message().unwrap().string_at(""),
            Some("analyzing your sessions")
        );
        assert_eq!(
            handler.requires(),
            Some(NativeCommandRequirements {
                workspace: Some(true)
            })
        );
        assert_eq!(
            handler.policy_gate(),
            Some(NativeCommandPolicyGate {
                policy: "allow_insights".into()
            })
        );
        assert!(handler.is_enabled());
    }

    #[tokio::test]
    async fn explicit_registration_replaces_the_placeholder_with_provider_handler() {
        let mut registry = crate::CommandRegistry::new();
        assert!(crate::builtins::register_insights_command(
            &mut registry,
            Arc::new(FakeProvider(Ok(report(None)))),
        ));
        let command = registry
            .resolve(&"insights".into())
            .expect("explicitly registered insights command");
        assert_eq!(command.native_type, Some(NativeCommandType::Prompt));
        assert!(command.disable_model_invocation);
        let handler = registry
            .get_handler(&"insights".into())
            .expect("provider-backed handler");
        assert!(matches!(
            handler.handle(&args("")).await,
            CommandResult::InjectMessage { .. }
        ));
    }

    #[tokio::test]
    async fn registered_insights_prompt_dispatches_as_native_header_and_meta_body() {
        use lingxi_core::host::{SlashCommandDispatcher as _, SlashDispatchResult};

        let mut registry = crate::CommandRegistry::new();
        assert!(crate::builtins::register_insights_command(
            &mut registry,
            Arc::new(FakeProvider(Ok(report(None)))),
        ));
        let dispatcher =
            crate::RegistrySlashDispatcher::new(Arc::new(tokio::sync::RwLock::new(registry)));

        let expected_body = render_report(&report(None));
        match dispatcher
            .dispatch("/insights arbitrary user text".into())
            .await
        {
            SlashDispatchResult::RunSkillAsTurn {
                command,
                body,
                hook_messages,
            } => {
                assert_eq!(
                    command.string_at(""),
                    Some(
                        "<command-message>insights</command-message>\n<command-name>/insights</command-name>\n<command-args>arbitrary user text</command-args>"
                    )
                );
                assert_eq!(body, expected_body);
                assert!(hook_messages.is_empty());
            }
            other => panic!("expected a Native prompt turn, got {other:?}"),
        }
    }

    #[tokio::test]
    async fn native_insights_body_does_not_enter_generic_shell_expansion() {
        use lingxi_core::host::{SlashCommandDispatcher as _, SlashDispatchResult};

        struct ShellExpansionMustNotRun;
        #[async_trait]
        impl crate::shell_expansion::ShellExpansionProvider for ShellExpansionMustNotRun {
            async fn git_worktree_context(&self) -> crate::shell_expansion::GitWorktreeContext {
                panic!("Native getPromptForCommand does not query Git worktree state here")
            }

            fn build(
                &self,
                _allowed_tools: &[String],
                _shell: Option<crate::FrontmatterShell>,
            ) -> crate::shell_expansion::ShellExpansionCtx {
                panic!("Native getPromptForCommand does not scan report text for shell blocks")
            }
        }

        let mut source_report = report(None);
        source_report.insights_json = r#"{"snippet":"!`echo MUST_NOT_RUN`"}"#.into();
        let mut registry = crate::CommandRegistry::new();
        assert!(crate::builtins::register_insights_command(
            &mut registry,
            Arc::new(FakeProvider(Ok(source_report.clone()))),
        ));
        let dispatcher =
            crate::RegistrySlashDispatcher::new(Arc::new(tokio::sync::RwLock::new(registry)))
                .with_shell_expansion(Arc::new(ShellExpansionMustNotRun));

        match dispatcher.dispatch("/insights".into()).await {
            SlashDispatchResult::RunSkillAsTurn { body, .. } => {
                assert!(body.string_at("").unwrap().contains("!`echo MUST_NOT_RUN`"));
            }
            other => panic!("expected Native Insights prompt turn, got {other:?}"),
        }
    }

    #[tokio::test]
    async fn denied_host_facts_do_not_replace_the_non_success_placeholder() {
        struct DeniedProvider;

        #[async_trait]
        impl InsightsReportProvider for DeniedProvider {
            fn command_availability(&self) -> InsightsCommandAvailability {
                InsightsCommandAvailability {
                    workspace_available: Some(true),
                    allow_insights: Some(false),
                }
            }

            async fn generate_report(&self) -> Result<InsightsReport, Utf16JsonProjection> {
                panic!("the report producer must not run behind a denied policy gate")
            }
        }

        let mut registry = crate::CommandRegistry::new();
        crate::builtins::register_all_builtin_commands(&mut registry);
        assert!(!crate::builtins::register_insights_command(
            &mut registry,
            Arc::new(DeniedProvider),
        ));
        let command = registry
            .resolve(&"insights".into())
            .expect("the metadata-only row remains");
        assert!(!command.is_enabled);
        let handler = registry
            .get_handler(&"insights".into())
            .expect("the disabled placeholder remains");
        assert!(matches!(
            handler.handle(&args("")).await,
            CommandResult::Done { .. }
        ));
    }

    #[tokio::test]
    async fn unknown_or_denied_host_facts_never_generate_a_report() {
        struct DeniedProvider;

        #[async_trait]
        impl InsightsReportProvider for DeniedProvider {
            fn command_availability(&self) -> InsightsCommandAvailability {
                InsightsCommandAvailability {
                    workspace_available: Some(true),
                    allow_insights: None,
                }
            }

            async fn generate_report(&self) -> Result<InsightsReport, Utf16JsonProjection> {
                panic!("the producer must not run when the gate is unknown")
            }
        }

        let handler = InsightsHandler::new(Arc::new(DeniedProvider));
        assert!(!handler.is_enabled());
        assert!(matches!(
            handler.handle(&args("")).await,
            CommandResult::Done { .. }
        ));
    }
}
