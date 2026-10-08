//! `ReportFindingsTool` — 2.1.238 `ReportFindings` (`pJf`, name const `UY`).
//!
//! 1:1 port of the oracle tool object at `cc-238.js @229541319`:
//!
//! ```js
//! pJf=es({name:UY,searchHint:"report code-review findings as a structured list",
//!   maxResultSizeChars:256,strict:!0,
//!   async description(){return q7a},async prompt(){return q7a},
//!   get inputSchema(){return uJf()},get outputSchema(){return iGv()},
//!   isReadOnly(){return!0},isConcurrencySafe(){return!0},
//!   toAutoClassifierInput(e){return `${e.findings.length} findings`},
//!   userFacingName(){return"Code review"},
//!   renderToolUseMessage(e){let t=e.findings?.length??0;
//!     return `${e.level??"review"} \xB7 ${t} ${Et(t,"finding")}`},
//!   async call({findings:e,level:t}){return{data:{count:e.length,level:t,findings:e}}},
//!   mapToolResultToToolResultBlockParam({count:e},t){return{tool_use_id:t,
//!     type:"tool_result",content:e===0?"No findings reported.":
//!     `${e} ${Et(e,"finding")} reported.`}}})
//! ```
//!
//! **Always advertised.** The object defines no `isEnabled`, so `es()`'s default
//! `MFb={isEnabled:()=>!0,…}` (`bin @284321674`) applies — the tool is in every
//! session's `tools` array. Likewise no `checkPermissions`, so the `es()` default
//! `(e,t)=>Promise.resolve({behavior:"allow",updatedInput:e})` applies.
//!
//! The call is a pure echo: no filesystem, no network, no session state. The
//! host UI is what renders the typed list; the model only gets the one-line
//! `"<n> finding(s) reported."` acknowledgement back.
//!
//! NOT PORTED (no seam in this port, matching every sibling tool ported since):
//! - `toAutoClassifierInput` — the `Tool` trait has no auto-classifier hook.
//! - `renderToolUseMessage` — the TUI has no per-tool render table (`ListAgents`
//!   and `PushNotification` are in the same position). Its byte-exact output is
//!   still produced by [`render_tool_use_message`] so the copy is pinned here.

use async_trait::async_trait;
use lingxi_core::types::utf16_json::Utf16JsonProjection;
use once_cell::sync::Lazy;
use permission::result::PermissionMetadata;
use permission::{PermissionDecisionReason, PermissionResult};
use serde_json::{json, Value};

use tool_api::context::ToolUseContext;
use tool_api::progress::ToolProgressSender;
use tool_api::tool_trait::{
    generated_builtin_result, generated_builtin_schema, source_text_projection, DescriptionOptions,
    PromptOptions, Tool, ToolCallResult, ToolError, ToolInputRepairMetadata, ToolInputRepairSchema,
    ToolInputSchemaProvenance, ToolStaticContext,
};
use tool_api::BuiltinToolContext;

/// Model-facing name (oracle `UY`, `cc-238.js @226675444`).
pub const REPORT_FINDINGS_TOOL_NAME: &str = "ReportFindings";

/// Oracle `q7a` (`cc-238.js @226675464`) — a plain double-quoted string constant
/// (verified with `od -c`: no template literal, no interpolation slot). Used for
/// BOTH `description()` and `prompt()`.
const DESCRIPTION: &str = "Report code-review findings as a typed list so the host UI can render them. Use this only when the active code-review instructions tell you to report findings with this tool; otherwise follow whatever output format those instructions specify. When reporting a review's results, call it once with the verified findings ranked most-severe first (empty array if nothing survived verification) and do not also print the findings as text. When re-reporting after applying fixes (only if the apply instructions ask for it), set `outcome` on each finding to what actually happened.";

/// Oracle `maxResultSizeChars:256`.
const MAX_RESULT_SIZE_CHARS: usize = 256;

/// Oracle `Gul` (`cc-238.js @229539930`) — one finding. Key order follows the
/// zod declaration (`file,line,summary,short_summary,failure_scenario,category,
/// verdict,outcome`), matching this port's convention of emitting schema keys in
/// definition order rather than alphabetically.
fn finding_schema() -> Value {
    json!({
        "type": "object",
        "additionalProperties": false,
        "properties": {
            "file": {
                "type": "string",
                "description": "Repo-relative path of the file the finding is in"
            },
            "line": {
                "type": "integer",
                "description": "1-indexed line the finding anchors to"
            },
            "summary": {
                "type": "string",
                "description": "One-sentence statement of the defect"
            },
            "short_summary": {
                "type": "string",
                "maxLength": 60,
                "description": "Compressed label for compact UI (\u{2264}60 chars): the claim alone, no rationale or consequence clause"
            },
            "failure_scenario": {
                "type": "string",
                "description": "Concrete inputs/state \u{2192} wrong output/crash"
            },
            "category": {
                "type": "string",
                "maxLength": 40,
                "description": "Short kebab-case slug of the finding type, e.g. \"correctness\", \"simplification\", \"efficiency\", \"test-coverage\""
            },
            "verdict": {
                "type": "string",
                "enum": ["CONFIRMED", "PLAUSIBLE"],
                "description": "Set when a verify pass ran; absent on inline-only reviews"
            },
            "outcome": {
                "type": "string",
                "enum": ["fixed", "skipped", "no_change_needed"],
                "description": "Set ONLY when re-reporting after applying fixes: what happened to this finding"
            }
        },
        "required": ["file", "summary", "failure_scenario"]
    })
}

/// Oracle `uJf` (`cc-238.js @229540815`) — the input schema.
///
/// `ci(...)` is the same zod-shim object constructor the oracle uses for the
/// `Edit` input schema (`p0i=we(()=>ci({file_path:…}))`, `cc-238.js @226392438`),
/// which this port already renders with `additionalProperties:false`
/// (`tools/file/src/edit.rs:322`) — so the closed form is used here too.
static INPUT_SCHEMA: Lazy<Utf16JsonProjection> = Lazy::new(|| {
    generated_builtin_schema(&json!({
        "type": "object",
        "additionalProperties": false,
        "properties": {
            "level": {
                "type": "string",
                "enum": ["low", "medium", "high", "xhigh", "max"],
                "description": "Effort level the review ran at"
            },
            "findings": {
                "type": "array",
                "items": finding_schema(),
                "maxItems": 32,
                "description": "Verified findings, most-severe first; empty if none survived"
            }
        },
        "required": ["findings"]
    }))
});

/// Oracle `iGv` (`cc-238.js @229541090`) — the output schema (`be(...)` ⇒ strict).
static OUTPUT_SCHEMA: Lazy<Utf16JsonProjection> = Lazy::new(|| {
    generated_builtin_schema(&json!({
        "type": "object",
        "additionalProperties": false,
        "properties": {
            "count": {
                "type": "number",
                "description": "Number of findings reported"
            },
            "level": {
                "type": "string",
                "enum": ["low", "medium", "high", "xhigh", "max"],
                "description": "Effort level the review ran at"
            },
            "findings": {
                "type": "array",
                "items": finding_schema(),
                "description": "Echoed for the result body"
            }
        },
        "required": ["count", "findings"]
    }))
});

/// `n === 1 ? word : word + 's'` — port of the oracle's `Et()`
/// (TS `plural()`, `stringUtils.ts:32`).
fn plural(n: usize, word: &str) -> String {
    if n == 1 {
        word.to_string()
    } else {
        format!("{word}s")
    }
}

/// Oracle `mapToolResultToToolResultBlockParam({count:e},t)` — the model-facing
/// `tool_result` content. Byte-exact.
#[must_use]
pub fn render_tool_result(count: usize) -> String {
    if count == 0 {
        "No findings reported.".to_string()
    } else {
        format!("{count} {} reported.", plural(count, "finding"))
    }
}

/// Oracle `renderToolUseMessage(e)` — the transcript header line.
/// `\xB7` is U+00B7 MIDDLE DOT.
#[must_use]
pub fn render_tool_use_message(level: Option<&str>, count: usize) -> String {
    format!(
        "{} \u{b7} {count} {}",
        level.unwrap_or("review"),
        plural(count, "finding")
    )
}

/// `ReportFindings` — echo a typed code-review finding list to the host UI.
pub struct ReportFindingsTool {
    _ctx: BuiltinToolContext,
}

impl ReportFindingsTool {
    /// Construct the tool over the builtin context.
    #[must_use]
    pub fn new(ctx: BuiltinToolContext) -> Self {
        Self { _ctx: ctx }
    }
}

#[async_trait]
impl Tool for ReportFindingsTool {
    fn name(&self) -> &str {
        REPORT_FINDINGS_TOOL_NAME
    }

    fn search_hint(&self) -> Option<Utf16JsonProjection> {
        Some("report code-review findings as a structured list".into())
    }

    fn user_facing_name(&self) -> Option<&str> {
        // Oracle `userFacingName(){return"Code review"}` — overrides the `es()`
        // default `userFacingName:()=>e.name`.
        Some("Code review")
    }

    fn input_schema(&self) -> &Utf16JsonProjection {
        &INPUT_SCHEMA
    }

    fn input_repair_metadata(&self) -> ToolInputRepairMetadata {
        ToolInputRepairMetadata::Known {
            schema: ToolInputRepairSchema::Advertised,
            schema_provenance: ToolInputSchemaProvenance::GeneratedBuiltin,
            exempt_top_level_keys: None,
        }
    }

    fn output_schema(&self) -> Option<&Utf16JsonProjection> {
        Some(&OUTPUT_SCHEMA)
    }

    async fn parse_output(
        &self,
        output: &Utf16JsonProjection,
    ) -> Result<Utf16JsonProjection, ToolError> {
        let invalid = || invalid_tool_input("ReportFindings returned invalid output");
        let count = output
            .subprojection("/count")
            .ok()
            .filter(|_| output.number_at("/count").is_some())
            .ok_or_else(invalid)?;
        let mut fields = vec![("count".to_owned(), count)];
        if let Some(level) = output.get_property("level") {
            let value = level.as_str().ok_or_else(invalid)?;
            if !["low", "medium", "high", "xhigh", "max"].contains(&value) {
                return Err(invalid());
            }
            fields.push((
                "level".to_owned(),
                output.subprojection("/level").map_err(|_| invalid())?,
            ));
        }
        let findings_count = output.array_len("/findings").ok_or_else(invalid)?;
        let mut findings = Vec::with_capacity(findings_count);
        for index in 0..findings_count {
            let pointer = format!("/findings/{index}");
            let view = output
                .at_pointer(&pointer)
                .filter(|value| value.object_entries().is_some())
                .ok_or_else(invalid)?;
            let mut finding_fields = Vec::new();
            for key in [
                "file",
                "line",
                "summary",
                "short_summary",
                "failure_scenario",
                "category",
                "verdict",
                "outcome",
            ] {
                let Some(value) = view.get_property(key) else {
                    if matches!(key, "file" | "summary" | "failure_scenario") {
                        return Err(invalid());
                    }
                    continue;
                };
                match key {
                    "file" | "summary" | "failure_scenario" | "short_summary" | "category" => {
                        let Some(text) = value.code_units() else {
                            return Err(invalid());
                        };
                        if (key == "short_summary" && text.len() > 60)
                            || (key == "category" && text.len() > 40)
                        {
                            return Err(invalid());
                        }
                    }
                    "line" => {
                        if !value
                            .as_number()
                            .is_some_and(|number| number.fract() == 0.0)
                        {
                            return Err(invalid());
                        }
                    }
                    "verdict" => {
                        if !value
                            .as_str()
                            .is_some_and(|text| matches!(text, "CONFIRMED" | "PLAUSIBLE"))
                        {
                            return Err(invalid());
                        }
                    }
                    "outcome" => {
                        if !value.as_str().is_some_and(|text| {
                            matches!(text, "fixed" | "skipped" | "no_change_needed")
                        }) {
                            return Err(invalid());
                        }
                    }
                    _ => {}
                }
                finding_fields.push((
                    key.to_owned(),
                    output
                        .subprojection(&format!("{pointer}/{key}"))
                        .map_err(|_| invalid())?,
                ));
            }
            findings.push(Utf16JsonProjection::object(finding_fields).map_err(|_| invalid())?);
        }
        fields.push((
            "findings".to_owned(),
            Utf16JsonProjection::array(findings).map_err(|_| invalid())?,
        ));
        Utf16JsonProjection::object(fields).map_err(|_| invalid())
    }

    fn is_enabled(&self, _ctx: &ToolStaticContext) -> bool {
        // Oracle: no `isEnabled` key ⇒ `es()` default `isEnabled:()=>!0`.
        true
    }

    fn strict(&self) -> bool {
        // Oracle `strict:!0`.
        true
    }

    fn max_result_size_chars(&self) -> usize {
        MAX_RESULT_SIZE_CHARS
    }

    fn is_concurrency_safe(&self, _input: &Utf16JsonProjection) -> bool {
        true
    }

    fn is_read_only(&self, _input: &Utf16JsonProjection) -> bool {
        true
    }

    async fn check_permissions(
        &self,
        _input: &Utf16JsonProjection,
        _ctx: &ToolUseContext,
    ) -> PermissionResult {
        // Oracle: no `checkPermissions` key ⇒ `es()` default
        // `(e,t)=>Promise.resolve({behavior:"allow",updatedInput:e})`.
        PermissionResult::Allow {
            reason: PermissionDecisionReason::Other {
                reason: "ReportFindings: always allowed".into(),
            },
            updated_input: None,
            update_destination: None,
            metadata: PermissionMetadata::default(),
        }
    }

    async fn description(
        &self,
        _input: &Utf16JsonProjection,
        _opts: &DescriptionOptions,
    ) -> String {
        DESCRIPTION.to_string()
    }

    async fn prompt(&self, _opts: &PromptOptions) -> Utf16JsonProjection {
        DESCRIPTION.to_string().into()
    }

    async fn call(
        &self,
        input: Utf16JsonProjection,
        _ctx: ToolUseContext,
        _progress: ToolProgressSender,
    ) -> Result<ToolCallResult, ToolError> {
        // Oracle `async call({findings:e,level:t}){return{data:{count:e.length,
        // level:t,findings:e}}}` — a pure echo. `findings` is schema-required;
        // an absent array degrades to empty rather than erroring, matching the
        // destructure of a validated input.
        let count = input.array_len("/findings").unwrap_or_default();
        let findings = input
            .subprojection("/findings")
            .unwrap_or_else(|_| generated_builtin_result(&json!([])));
        let mut data = vec![("count".to_owned(), generated_builtin_result(&json!(count)))];
        // JS `{level: undefined}` is dropped by JSON.stringify — omit when absent.
        if input
            .get_property("level")
            .is_some_and(|level| !level.is_null())
        {
            data.push((
                "level".to_owned(),
                input
                    .subprojection("/level")
                    .expect("present level subtree"),
            ));
        }
        data.push(("findings".to_owned(), findings));
        let data = Utf16JsonProjection::object(data).expect("ReportFindings output is valid JSON");

        Ok(ToolCallResult {
            data,
            model_content: Some(source_text_projection(&render_tool_result(count))),
            new_messages: vec![],
            context_modifier: None,
            is_error: false,
            mcp_meta: None,
            subagent_hooks_fired: false,
        })
    }
}

fn invalid_tool_input(message: impl Into<String>) -> ToolError {
    ToolError::InvalidInput(source_text_projection(&message.into()))
}

#[cfg(test)]
mod tests {
    use super::*;
    use mobile_linux_api::ProcessOutput;
    use tool_api::test_support::{fresh_ctx, fresh_tx, shell_test_ctx};

    fn fixture(value: Value) -> Utf16JsonProjection {
        generated_builtin_result(&value)
    }

    fn test_value(value: &Utf16JsonProjection) -> Value {
        serde_json::from_str(&value.to_json_string().unwrap()).unwrap()
    }

    fn dummy_out() -> ProcessOutput {
        ProcessOutput {
            stdout: String::new(),
            stderr: String::new(),
            exit_code: 0,
            timed_out: false,
        }
    }

    fn tool() -> ReportFindingsTool {
        ReportFindingsTool::new(shell_test_ctx(dummy_out()))
    }

    #[tokio::test]
    async fn output_parser_retains_unpaired_finding_strings() {
        let file_units = vec![b'f' as u16, 0xD800, b'.' as u16, b'r' as u16, b's' as u16];
        let summary_units = vec![b's' as u16, 0xDC00];
        let output = Utf16JsonProjection::object(vec![
            ("count".into(), generated_builtin_result(&json!(1))),
            (
                "findings".into(),
                Utf16JsonProjection::array(vec![Utf16JsonProjection::object(vec![
                    (
                        "file".into(),
                        tool_api::tool_trait::source_text_projection_units(&file_units),
                    ),
                    (
                        "summary".into(),
                        tool_api::tool_trait::source_text_projection_units(&summary_units),
                    ),
                    (
                        "failure_scenario".into(),
                        source_text_projection("scenario"),
                    ),
                ])
                .unwrap()])
                .unwrap(),
            ),
        ])
        .unwrap();

        let parsed = tool().parse_output(&output).await.expect("valid findings");
        assert_eq!(parsed.string_units("/findings/0/file"), Some(file_units));
        assert_eq!(
            parsed.string_units("/findings/0/summary"),
            Some(summary_units)
        );
    }

    #[test]
    fn name_and_metadata_byte_exact() {
        let t = tool();
        assert_eq!(t.name(), "ReportFindings");
        assert_eq!(
            t.search_hint().as_ref().and_then(|hint| hint.string_at("")),
            Some("report code-review findings as a structured list")
        );
        assert_eq!(t.user_facing_name(), Some("Code review"));
        assert_eq!(t.max_result_size_chars(), 256);
        assert!(t.strict());
        let input = fixture(json!({}));
        assert!(t.is_read_only(&input));
        assert!(t.is_concurrency_safe(&input));
        // `es()` default `isEnabled:()=>!0` — advertised in every session.
        assert!(t.is_enabled(&ToolStaticContext::default()));
        // No `shouldDefer` in the oracle object ⇒ loaded up front.
        assert!(!t.should_defer());
    }

    #[tokio::test]
    async fn description_and_prompt_are_the_same_oracle_constant() {
        let t = tool();
        let d = t
            .description(
                &fixture(json!({})),
                &DescriptionOptions {
                    is_non_interactive_session: false,
                },
            )
            .await;
        let prompt = t.prompt(&PromptOptions::default()).await;
        let p = prompt
            .string_at("")
            .expect("generated prompt is a root string");
        assert_eq!(d, p);
        assert_eq!(
            d,
            "Report code-review findings as a typed list so the host UI can render them. Use this only when the active code-review instructions tell you to report findings with this tool; otherwise follow whatever output format those instructions specify. When reporting a review's results, call it once with the verified findings ranked most-severe first (empty array if nothing survived verification) and do not also print the findings as text. When re-reporting after applying fixes (only if the apply instructions ask for it), set `outcome` on each finding to what actually happened."
        );
    }

    #[test]
    fn input_schema_matches_the_oracle_zod() {
        let t = tool();
        let s = test_value(t.input_schema());
        assert_eq!(s["type"], json!("object"));
        // Closed object, matching the port's rendering of the oracle's `ci(...)`
        // constructor for `Edit` (`tools/file/src/edit.rs`).
        assert_eq!(s["additionalProperties"], json!(false));
        assert_eq!(s["required"], json!(["findings"]));
        assert_eq!(s["properties"]["findings"]["maxItems"], json!(32));
        assert_eq!(
            s["properties"]["level"]["enum"],
            json!(["low", "medium", "high", "xhigh", "max"])
        );
        let item = &s["properties"]["findings"]["items"];
        assert_eq!(item["additionalProperties"], json!(false));
        assert_eq!(
            item["required"],
            json!(["file", "summary", "failure_scenario"])
        );
        assert_eq!(item["properties"]["line"]["type"], json!("integer"));
        assert_eq!(item["properties"]["short_summary"]["maxLength"], json!(60));
        assert_eq!(item["properties"]["category"]["maxLength"], json!(40));
        assert_eq!(
            item["properties"]["verdict"]["enum"],
            json!(["CONFIRMED", "PLAUSIBLE"])
        );
        assert_eq!(
            item["properties"]["outcome"]["enum"],
            json!(["fixed", "skipped", "no_change_needed"])
        );
        // Unicode in the descriptions is byte-exact (U+2264, U+2192).
        assert_eq!(
            item["properties"]["short_summary"]["description"],
            json!(
                "Compressed label for compact UI (≤60 chars): the claim alone, no rationale or consequence clause"
            )
        );
        assert_eq!(
            item["properties"]["failure_scenario"]["description"],
            json!("Concrete inputs/state → wrong output/crash")
        );
    }

    #[test]
    fn output_schema_matches_the_oracle_zod() {
        let t = tool();
        let s = test_value(t.output_schema().expect("outputSchema is defined"));
        assert_eq!(s["additionalProperties"], json!(false));
        assert_eq!(s["required"], json!(["count", "findings"]));
        assert_eq!(s["properties"]["count"]["type"], json!("number"));
        assert_eq!(
            s["properties"]["findings"]["description"],
            json!("Echoed for the result body")
        );
        // The output array carries no `maxItems` (the input one does).
        assert!(s["properties"]["findings"].get("maxItems").is_none());
    }

    #[tokio::test]
    async fn call_echoes_and_renders_the_acknowledgement() {
        let t = tool();
        let out = t
            .call(
                fixture(json!({
                    "level": "high",
                    "findings": [{
                        "file": "src/a.rs",
                        "summary": "off-by-one",
                        "failure_scenario": "len==0 → panic"
                    }]
                })),
                fresh_ctx(),
                fresh_tx(),
            )
            .await
            .expect("echo call cannot fail");
        let data = test_value(&out.data);
        assert_eq!(data["count"], json!(1));
        assert_eq!(data["level"], json!("high"));
        assert_eq!(data["findings"].as_array().map(Vec::len), Some(1));
        assert_eq!(
            out.model_content
                .as_ref()
                .and_then(|text| text.string_at("")),
            Some("1 finding reported.")
        );
        assert!(!out.is_error);
    }

    #[tokio::test]
    async fn absent_level_is_omitted_like_json_stringify() {
        let t = tool();
        let out = t
            .call(fixture(json!({ "findings": [] })), fresh_ctx(), fresh_tx())
            .await
            .expect("echo call cannot fail");
        let data = test_value(&out.data);
        assert!(data.get("level").is_none());
        assert_eq!(data["count"], json!(0));
        assert_eq!(
            out.model_content
                .as_ref()
                .and_then(|text| text.string_at("")),
            Some("No findings reported.")
        );
    }

    #[test]
    fn render_branches_byte_exact() {
        assert_eq!(render_tool_result(0), "No findings reported.");
        assert_eq!(render_tool_result(1), "1 finding reported.");
        assert_eq!(render_tool_result(3), "3 findings reported.");
        // `${e.level??"review"} · ${t} ${Et(t,"finding")}` — U+00B7 middle dot.
        assert_eq!(render_tool_use_message(None, 0), "review · 0 findings");
        assert_eq!(render_tool_use_message(Some("max"), 1), "max · 1 finding");
    }
}
