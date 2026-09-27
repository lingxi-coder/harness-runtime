//! `BriefTool` — sends a message the user will read (wire name
//! `SendUserMessage`). Byte-faithful port of claude-code `BriefTool`.
//!
//! Wire identifiers locked in spec §7 (asserted by
//! `test-harness/tests/parity_system_tools.rs`): the `brief` subdir literal,
//! `.txt` suffix, and `[b]…` task-id prefix. These are retained as `pub`
//! constants even though the tool no longer writes a file — the parity
//! fixtures still cover them.

use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::{Duration, Instant, SystemTime};

use async_trait::async_trait;
use once_cell::sync::Lazy;
use permission::result::PermissionMetadata;
use permission::{PermissionDecisionReason, PermissionResult};
use serde_json::{json, Value};
use telemetry::pii::{PiiTagged, Verified};
use telemetry::sink::{AnalyticsValue, LogEventMetadata};
use telemetry::tengu::tool::{BRIEF_COMPLETED, BRIEF_FAILED, BRIEF_STARTED};
use telemetry::AnalyticsBus;

use tool_api::context::ToolUseContext;
use tool_api::progress::ToolProgressSender;
use tool_api::tool_trait::{
    DescriptionOptions, InterruptBehavior, PromptOptions, Tool, ToolCallResult, ToolError,
    ToolStaticContext, ValidationError,
};

/// Tool name byte-lock. The model addresses this tool by its wire name
/// `SendUserMessage` (claude-code `BriefTool/prompt.ts` `BRIEF_TOOL_NAME`).
pub const BRIEF_TOOL_NAME: &str = "SendUserMessage";
/// Legacy wire name still accepted as an alias (claude-code
/// `LEGACY_BRIEF_TOOL_NAME = "Brief"`).
pub const LEGACY_BRIEF_TOOL_NAME: &str = "Brief";
/// Subdirectory under `~/.lingxi/` (wire identifier, parity-locked).
pub const BRIEF_SUBDIR: &str = "brief";
/// File extension (wire identifier, parity-locked).
pub const BRIEF_FILE_SUFFIX: &str = ".txt";
/// 9-char task-id prefix character (`b` for brief; wire identifier,
/// parity-locked).
pub const BRIEF_TASK_ID_PREFIX: char = 'b';
/// Maximum message length (sane upper bound; 1 MB matches BashTool output cap).
/// Rust-side guard only — the TS `message: z.string()` has no max.
pub const MAX_BRIEF_BODY_LEN: usize = 1_048_576;

/// Model-facing one-liner — claude-code `BriefTool/prompt.ts` `DESCRIPTION`.
const DESCRIPTION: &str = "Send a message to the user";

/// Statsig gate name — claude-code `aKr()` reads `dge("tengu_kairos_brief", !1)`
/// (code-default `false`, no flag backend here). `tengu_*` keys are kept verbatim.
const BRIEF_GATE_FLAG: &str = "tengu_kairos_brief";

/// Port of claude-code `isBriefEnabled` / `aKr()`:
/// `return Z.CLAUDE_CODE_BRIEF || dge("tengu_kairos_brief", !1)`; LingXi
/// publishes the equivalent live session value through `session_flags`.
///
/// The `SendUserMessage` (Brief) tool is exposed to the model ONLY when the
/// session's live Brief-only mode is enabled — by `--brief` at startup or the
/// `/brief` runtime toggle — OR the `tengu_kairos_brief` Statsig gate is on
/// (code-default `false`). This is DEFAULT-OFF, matching the shipped binary:
/// without either trigger the tool is invisible to the model. The state lives
/// in `platform_api::session_flags`, rather than a process-environment snapshot,
/// so a runtime toggle is observed by subsequent tool-list assemblies.
#[must_use]
pub fn brief_tool_enabled() -> bool {
    platform_api::session_flags::brief_mode_enabled()
        || telemetry::flag_bool(BRIEF_GATE_FLAG, false)
}

/// Model-facing tool prompt — byte-faithful port of `BriefTool/prompt.ts`
/// `BRIEF_TOOL_PROMPT`. The `\u{2014}` are em-dashes (U+2014) and the
/// backtick-wrapped field names are reproduced exactly.
const BRIEF_TOOL_PROMPT: &str = "Send a message the user will read. Text outside this tool is visible in the detail view, but most won't open it \u{2014} the answer lives here.\n\n`message` supports markdown. `attachments` takes file paths (absolute or cwd-relative) for images, diffs, logs.\n\n`status` labels intent: 'normal' when replying to what they just asked; 'proactive' when you're initiating \u{2014} a scheduled task finished, a blocker surfaced during background work, you need input on something they haven't asked about. Set it honestly; downstream routing uses it.";

/// `BriefTool` — sends a message the user will read (wire `SendUserMessage`).
pub struct BriefTool {
    pub(crate) ctx: tool_api::BuiltinToolContext,
}

impl BriefTool {
    /// Construct.
    #[must_use]
    pub fn new(ctx: tool_api::BuiltinToolContext) -> Self {
        Self { ctx }
    }
}

static SCHEMA: Lazy<Value> = Lazy::new(|| {
    // TS `z.strictObject({ message, attachments?, status })`
    // (`BriefTool.ts:20-39`). Byte-faithful field descriptions.
    json!({
        "type": "object",
        "additionalProperties": false,
        "properties": {
            "message": {
                "type": "string",
                "description": "The message for the user. Supports markdown formatting."
            },
            "attachments": {
                "type": "array",
                "items": { "type": "string" },
                "description": "Optional file paths (absolute or relative to cwd) to attach. Use for photos, screenshots, diffs, logs, or any file the user should see alongside your message."
            },
            "status": {
                "type": "string",
                "enum": ["normal", "proactive"],
                "description": "Use 'proactive' when you're surfacing something the user hasn't asked for and needs to see now — task completion while they're away, a blocker you hit, an unsolicited status update. Use 'normal' when replying to something the user just said."
            }
        },
        "required": ["message", "status"]
    })
});

fn pii_str(s: &str) -> AnalyticsValue {
    AnalyticsValue::String(PiiTagged::assert_pii_tagged_column(s.to_string()).into_inner())
}

fn verified_str(s: &str) -> AnalyticsValue {
    AnalyticsValue::String(Verified::assert_safe(s.to_string()).into_inner())
}

async fn emit_failed(bus: &Arc<AnalyticsBus>, kind: &str, duration_ms: u64) {
    let mut md: LogEventMetadata = HashMap::new();
    md.insert("error_kind".into(), verified_str(kind));
    md.insert(
        "duration_ms".into(),
        AnalyticsValue::Int(duration_ms as i64),
    );
    bus.log_event(BRIEF_FAILED, md).await;
}

/// `n === 1 ? word : word + 's'` — port of TS `plural()` (`stringUtils.ts:32`).
fn plural(n: usize, word: &str) -> String {
    if n == 1 {
        word.to_string()
    } else {
        format!("{word}s")
    }
}

/// Extension-based image test — port of TS `IMAGE_EXTENSION_REGEX`
/// (`/\.(png|jpe?g|gif|webp)$/i`).
fn is_image_path(path: &str) -> bool {
    let lower = path.to_ascii_lowercase();
    [".png", ".jpg", ".jpeg", ".gif", ".webp"]
        .iter()
        .any(|ext| lower.ends_with(ext))
}

/// Port of TS `resolveAttachments` — `{ path, size, isImage }`.
///
/// PARITY-GAP: TS additionally uploads each attachment in `BRIDGE_MODE` and
/// attaches a `file_uuid`; that belongs to Anthropic's private bridge protocol
/// and is intentionally not emulated. Local attachment paths still follow the
/// public behavior: `~` and cwd-relative inputs become absolute paths before
/// metadata is read and before the result reaches the UI.
fn resolve_attachment_path(path: &str, cwd: &Path) -> PathBuf {
    if path == "~" {
        return std::env::var_os("HOME")
            .or_else(|| std::env::var_os("USERPROFILE"))
            .map(PathBuf::from)
            .unwrap_or_else(|| cwd.join(path));
    }
    if let Some(rest) = path.strip_prefix("~/") {
        if let Some(home) = std::env::var_os("HOME").or_else(|| std::env::var_os("USERPROFILE")) {
            return PathBuf::from(home).join(rest);
        }
    }
    let path = Path::new(path);
    if path.is_absolute() {
        path.to_path_buf()
    } else {
        cwd.join(path)
    }
}

fn resolve_attachments(paths: &[String], cwd: &Path) -> Vec<Value> {
    paths
        .iter()
        .map(|p| {
            let resolved = resolve_attachment_path(p, cwd);
            let size = std::fs::metadata(&resolved).map(|m| m.len()).unwrap_or(0);
            let rendered = resolved.to_string_lossy();
            json!({
                "path": rendered,
                "size": size,
                "isImage": is_image_path(&rendered),
            })
        })
        .collect()
}

/// Civil date `(year, month, day)` from days since the Unix epoch — port of
/// Howard Hinnant's `civil_from_days`. Valid for the proleptic Gregorian
/// calendar; we only ever feed it non-negative (post-1970) day counts.
fn civil_from_days(z: i64) -> (i64, u32, u32) {
    let z = z + 719_468;
    let era = if z >= 0 { z } else { z - 146_096 } / 146_097;
    let doe = z - era * 146_097; // [0, 146096]
    let yoe = (doe - doe / 1460 + doe / 36_524 - doe / 146_096) / 365; // [0, 399]
    let y = yoe + era * 400;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100); // [0, 365]
    let mp = (5 * doy + 2) / 153; // [0, 11]
    let d = (doy - (153 * mp + 2) / 5 + 1) as u32; // [1, 31]
    let m = (if mp < 10 { mp + 3 } else { mp - 9 }) as u32; // [1, 12]
    (if m <= 2 { y + 1 } else { y }, m, d)
}

/// ISO-8601 UTC timestamp with millisecond precision
/// (e.g. `2026-06-07T12:34:56.789Z`) — port of TS `new Date().toISOString()`.
/// Std-only; deterministic for a fixed `SystemTime`.
fn iso8601_utc(t: SystemTime) -> String {
    let dur = t
        .duration_since(SystemTime::UNIX_EPOCH)
        .unwrap_or(Duration::ZERO);
    let secs = dur.as_secs() as i64;
    let millis = dur.subsec_millis();
    let days = secs.div_euclid(86_400);
    let tod = secs.rem_euclid(86_400);
    let (hh, mm, ss) = (tod / 3600, (tod % 3600) / 60, tod % 60);
    let (y, m, d) = civil_from_days(days);
    format!("{y:04}-{m:02}-{d:02}T{hh:02}:{mm:02}:{ss:02}.{millis:03}Z")
}

#[async_trait]
impl Tool for BriefTool {
    fn name(&self) -> &str {
        BRIEF_TOOL_NAME
    }
    fn aliases(&self) -> &[&str] {
        const ALIASES: &[&str] = &[LEGACY_BRIEF_TOOL_NAME];
        ALIASES
    }
    fn input_schema(&self) -> &Value {
        &SCHEMA
    }
    fn is_enabled(&self, _: &ToolStaticContext) -> bool {
        // PARITY: claude-code `isBriefEnabled` (`aKr()`). Default-off — the tool
        // is exposed only under the live Brief-only session flag (startup
        // `--brief` or `/brief`) or the `tengu_kairos_brief` gate.
        brief_tool_enabled()
    }
    fn max_result_size_chars(&self) -> usize {
        tool_api::util::output_truncation::MAX_TOOL_OUTPUT_LENGTH
    }
    fn is_concurrency_safe(&self, _: &Value) -> bool {
        true
    }
    fn is_read_only(&self, _: &Value) -> bool {
        // TS `isReadOnly() { return true }` — Brief never writes the filesystem.
        true
    }
    fn is_destructive(&self, _: &Value) -> bool {
        false
    }
    fn is_open_world(&self, _: &Value) -> bool {
        false
    }
    fn interrupt_behavior(&self, _: &Value) -> InterruptBehavior {
        InterruptBehavior::Block
    }

    async fn check_permissions(&self, _: &Value, _: &ToolUseContext) -> PermissionResult {
        PermissionResult::Allow {
            reason: PermissionDecisionReason::Other {
                reason: "SendUserMessage delivers a message to the user (no filesystem write)"
                    .into(),
            },
            updated_input: None,
            update_destination: None,
            metadata: PermissionMetadata::default(),
        }
    }

    async fn description(&self, _: &Value, _: &DescriptionOptions) -> String {
        DESCRIPTION.into()
    }

    async fn prompt(&self, _: &PromptOptions) -> String {
        BRIEF_TOOL_PROMPT.into()
    }

    async fn validate_input(
        &self,
        input: &Value,
        _: &ToolUseContext,
    ) -> Result<(), ValidationError> {
        // TS schema: `message: z.string()` (no minLength). Require a string but
        // accept empty. `MAX_BRIEF_BODY_LEN` is a Rust-side guard.
        let message = input
            .get("message")
            .and_then(Value::as_str)
            .ok_or_else(|| ValidationError("Brief: missing or non-string message".into()))?;
        if message.len() > MAX_BRIEF_BODY_LEN {
            return Err(ValidationError(format!(
                "Brief: message length {} exceeds max {}",
                message.len(),
                MAX_BRIEF_BODY_LEN
            )));
        }
        Ok(())
    }

    async fn call(
        &self,
        input: Value,
        ctx: ToolUseContext,
        _progress: ToolProgressSender,
    ) -> Result<ToolCallResult, ToolError> {
        let started = Instant::now();
        let bus = self.ctx.bus.clone();

        let message = match input.get("message").and_then(Value::as_str) {
            Some(s) => s.to_string(),
            None => {
                emit_failed(
                    &bus,
                    "missing_message",
                    started.elapsed().as_millis() as u64,
                )
                .await;
                return Err(ToolError::InvalidInput(
                    "Brief: missing or non-string message".into(),
                ));
            }
        };
        if message.len() > MAX_BRIEF_BODY_LEN {
            emit_failed(
                &bus,
                "message_too_large",
                started.elapsed().as_millis() as u64,
            )
            .await;
            return Err(ToolError::InvalidInput(format!(
                "Brief: message length {} exceeds max {MAX_BRIEF_BODY_LEN}",
                message.len()
            )));
        }

        // `status === 'proactive'` flag (TS `logEvent('tengu_brief_send', …)`).
        let proactive = input.get("status").and_then(Value::as_str) == Some("proactive");
        // Input attachment paths (optional array of strings).
        let attachment_paths: Vec<String> = input
            .get("attachments")
            .and_then(Value::as_array)
            .map(|arr| {
                arr.iter()
                    .filter_map(|v| v.as_str().map(str::to_string))
                    .collect()
            })
            .unwrap_or_default();
        let n = attachment_paths.len();

        let preview: String = message.chars().take(80).collect();
        let mut md: LogEventMetadata = HashMap::new();
        md.insert(
            "message_len".into(),
            AnalyticsValue::Int(message.len() as i64),
        );
        md.insert("_PROTO_message_preview".into(), pii_str(&preview));
        bus.log_event(BRIEF_STARTED, md).await;

        // TS `const sentAt = new Date().toISOString()` — deterministic clock.
        let sent_at = iso8601_utc(self.ctx.clock.now());

        // Output shape mirrors TS `{ data: { message, [attachments], sentAt } }`.
        let mut data = json!({
            "message": message,
            "sentAt": sent_at,
        });
        if n > 0 {
            let cwd = ctx.cwd.clone().unwrap_or_else(|| self.ctx.cwd());
            data["attachments"] = Value::Array(resolve_attachments(&attachment_paths, &cwd));
        }
        // `mapToolResultToToolResultBlockParam`: tool-result content text
        // (`BriefTool.ts:175-181`). We surface it on `data` so hosts can read it.
        let suffix = if n == 0 {
            String::new()
        } else {
            format!(" ({n} {} included)", plural(n, "attachment"))
        };
        data["content"] = json!(format!("Message delivered to user.{suffix}"));

        let mut md: LogEventMetadata = HashMap::new();
        md.insert(
            "duration_ms".into(),
            AnalyticsValue::Int(started.elapsed().as_millis() as i64),
        );
        md.insert("attachment_count".into(), AnalyticsValue::Int(n as i64));
        md.insert("proactive".into(), AnalyticsValue::Bool(proactive));
        bus.log_event(BRIEF_COMPLETED, md).await;

        Ok(ToolCallResult {
            data,
            model_content: None,
            new_messages: vec![],
            context_modifier: None,
            is_error: false,
            mcp_meta: None,
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use platform_api::process::ProcessOutput;
    use tool_api::test_support::{fresh_ctx, fresh_tx, shell_test_ctx};

    fn dummy_out() -> ProcessOutput {
        ProcessOutput {
            stdout: String::new(),
            stderr: String::new(),
            exit_code: 0,
            timed_out: false,
        }
    }

    /// Matches `^\d{4}-\d{2}-\d{2}T\d{2}:\d{2}:\d{2}\.\d{3}Z$` (no regex dep).
    fn iso_shape_ok(s: &str) -> bool {
        let b = s.as_bytes();
        if b.len() != 24 {
            return false;
        }
        b[4] == b'-'
            && b[7] == b'-'
            && b[10] == b'T'
            && b[13] == b':'
            && b[16] == b':'
            && b[19] == b'.'
            && b[23] == b'Z'
            && [0, 1, 2, 3, 5, 6, 8, 9, 11, 12, 14, 15, 17, 18, 20, 21, 22]
                .iter()
                .all(|&i| b[i].is_ascii_digit())
    }

    #[test]
    fn constants_locked() {
        assert_eq!(BRIEF_TOOL_NAME, "SendUserMessage");
        assert_eq!(LEGACY_BRIEF_TOOL_NAME, "Brief");
        assert_eq!(BRIEF_SUBDIR, "brief");
        assert_eq!(BRIEF_FILE_SUFFIX, ".txt");
        assert_eq!(BRIEF_TASK_ID_PREFIX, 'b');
    }

    #[test]
    fn advertises_send_user_message_name_with_brief_alias() {
        let tool = BriefTool::new(shell_test_ctx(dummy_out()));
        assert_eq!(tool.name(), "SendUserMessage");
        assert_eq!(tool.aliases(), &["Brief"]);
        // The legacy wire name resolves through the alias list.
        assert!(tool.aliases().contains(&"Brief"));
    }

    #[test]
    fn schema_is_strict_object_with_message_and_status() {
        // TS `z.strictObject({ message, attachments?, status })`.
        let tool = BriefTool::new(shell_test_ctx(dummy_out()));
        let schema = tool.input_schema();
        assert_eq!(schema["type"], json!("object"));
        assert_eq!(schema["additionalProperties"], json!(false));
        assert_eq!(schema["required"], json!(["message", "status"]));
        assert_eq!(schema["properties"]["message"]["type"], json!("string"));
        assert_eq!(
            schema["properties"]["status"]["enum"],
            json!(["normal", "proactive"])
        );
        assert_eq!(
            schema["properties"]["attachments"]["items"]["type"],
            json!("string")
        );
        // minLength dropped from the message field.
        assert!(schema["properties"]["message"].get("minLength").is_none());
    }

    #[test]
    fn is_read_only_true() {
        let tool = BriefTool::new(shell_test_ctx(dummy_out()));
        assert!(tool.is_read_only(&json!({})));
    }

    /// Serialize the flag-mutating gate tests and reset shared state (the live
    /// Brief mode flag + the `tengu_kairos_brief` gate) so they cannot leak
    /// across tests. Mirrors `push_notification::tests::guard`.
    fn brief_guard() -> std::sync::MutexGuard<'static, ()> {
        static SERIAL: std::sync::Mutex<()> = std::sync::Mutex::new(());
        let g = SERIAL.lock().unwrap_or_else(|e| e.into_inner());
        platform_api::session_flags::set_brief_mode_enabled(false);
        telemetry::test_clear_flag(BRIEF_GATE_FLAG);
        g
    }

    #[test]
    fn disabled_by_default_enabled_by_live_brief_mode() {
        let _g = brief_guard();
        let tool = BriefTool::new(shell_test_ctx(dummy_out()));
        let ctx = ToolStaticContext::default();
        // Default: no live --brief state, gate off → invisible to the model
        // (default-off, so the default toolset is unchanged from the binary).
        assert!(!tool.is_enabled(&ctx));
        assert!(!brief_tool_enabled());

        // Startup `--brief` and runtime `/brief` both publish the same live
        // session flag → tool enabled.
        platform_api::session_flags::set_brief_mode_enabled(true);
        assert!(tool.is_enabled(&ctx));
        platform_api::session_flags::set_brief_mode_enabled(false);
        assert!(!tool.is_enabled(&ctx));

        // The Statsig gate `tengu_kairos_brief` (default false) also enables it.
        assert!(!tool.is_enabled(&ctx));
        telemetry::test_set_flag(BRIEF_GATE_FLAG, true);
        assert!(tool.is_enabled(&ctx));
        telemetry::test_clear_flag(BRIEF_GATE_FLAG);
        assert!(!tool.is_enabled(&ctx));
    }

    #[test]
    fn iso8601_utc_known_values() {
        // 0 secs → epoch.
        assert_eq!(
            iso8601_utc(SystemTime::UNIX_EPOCH),
            "1970-01-01T00:00:00.000Z"
        );
        // 1_000_000_000 secs → the Unix billennium.
        assert_eq!(
            iso8601_utc(SystemTime::UNIX_EPOCH + Duration::from_secs(1_000_000_000)),
            "2001-09-09T01:46:40.000Z"
        );
        // Sub-second millis are rendered with zero-padding.
        assert_eq!(
            iso8601_utc(SystemTime::UNIX_EPOCH + Duration::from_millis(789)),
            "1970-01-01T00:00:00.789Z"
        );
    }

    #[tokio::test]
    async fn delivers_message_with_sent_at_and_content() {
        let tool = BriefTool::new(shell_test_ctx(dummy_out()));
        let message = "# Brief\n\nHello world.";
        let out = tool
            .call(
                json!({"message": message, "status": "normal"}),
                fresh_ctx(),
                fresh_tx(),
            )
            .await
            .expect("ok");
        // Output shape: { message, sentAt, content } — no attachments key.
        assert_eq!(out.data["message"], json!(message));
        let sent_at = out.data["sentAt"].as_str().expect("sentAt string");
        assert!(iso_shape_ok(sent_at), "{sent_at}");
        // Deterministic stub clock is anchored at the Unix epoch.
        assert_eq!(sent_at, "1970-01-01T00:00:00.000Z");
        let content = out.data["content"].as_str().expect("content string");
        assert!(content.starts_with("Message delivered to user."));
        assert_eq!(content, "Message delivered to user.");
        assert!(out.data.get("attachments").is_none());
    }

    #[tokio::test]
    async fn includes_attachments_array_and_suffix() {
        let tmp = tempfile::tempdir().unwrap();
        let img = tmp.path().join("shot.PNG");
        std::fs::write(&img, b"x").unwrap();
        let tool = BriefTool::new(shell_test_ctx(dummy_out()));
        let out = tool
            .call(
                json!({
                    "message": "see attached",
                    "status": "proactive",
                    "attachments": [img.to_str().unwrap()],
                }),
                fresh_ctx(),
                fresh_tx(),
            )
            .await
            .expect("ok");
        let atts = out.data["attachments"]
            .as_array()
            .expect("attachments array");
        assert_eq!(atts.len(), 1);
        assert_eq!(atts[0]["path"].as_str().unwrap(), img.to_str().unwrap());
        assert_eq!(atts[0]["size"], json!(1));
        assert_eq!(atts[0]["isImage"], json!(true)); // .PNG matches case-insensitively
        assert_eq!(
            out.data["content"].as_str().unwrap(),
            "Message delivered to user. (1 attachment included)"
        );
    }

    #[tokio::test]
    async fn resolves_relative_attachment_against_invocation_cwd() {
        let tmp = tempfile::tempdir().unwrap();
        let file = tmp.path().join("notes.log");
        std::fs::write(&file, b"hello").unwrap();
        let tool = BriefTool::new(shell_test_ctx(dummy_out()));
        let mut call_ctx = fresh_ctx();
        call_ctx.cwd = Some(tmp.path().to_path_buf());

        let out = tool
            .call(
                json!({
                    "message": "see attached",
                    "status": "normal",
                    "attachments": ["notes.log"],
                }),
                call_ctx,
                fresh_tx(),
            )
            .await
            .expect("ok");

        assert_eq!(out.data["attachments"][0]["path"].as_str(), file.to_str());
        assert_eq!(out.data["attachments"][0]["size"], 5);
    }

    #[tokio::test]
    async fn rejects_missing_message() {
        let tool = BriefTool::new(shell_test_ctx(dummy_out()));
        let err = tool
            .call(json!({"status": "normal"}), fresh_ctx(), fresh_tx())
            .await
            .expect_err("missing");
        assert!(format!("{err}").contains("missing or non-string message"));
    }

    #[tokio::test]
    async fn accepts_empty_message() {
        // minLength dropped — an empty message now validates (TS `z.string()`).
        let tool = BriefTool::new(shell_test_ctx(dummy_out()));
        tool.validate_input(&json!({"message": "", "status": "normal"}), &fresh_ctx())
            .await
            .expect("empty message validates");
    }
}
