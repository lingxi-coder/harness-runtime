//! `REPLTool` — runs a snippet in `python`, `node`, or `ruby`.
//!
//! Languages and execution modes:
//! - python  → `python3` (Unix) / `python.exe` (Win); code via stdin (`-` arg).
//! - node    → `node`; code via stdin.
//! - ruby    → `ruby`; code via `-e "<code>"`.
//!
//! See spec §7 wire identifiers (REPL row) and claude-code/src/tools/REPLTool/.

use crate::shared::strip_ansi_count;
use async_trait::async_trait;
use once_cell::sync::Lazy;
use permission::result::PermissionMetadata;
use permission::{PermissionDecisionReason, PermissionResult};
use serde_json::{json, Value};
use std::collections::HashMap;
use std::time::Duration;
use telemetry::sink::{AnalyticsValue, LogEventMetadata};
use telemetry::tengu::tool::{REPL_COMPLETED, REPL_FAILED, REPL_STARTED};
use tool_api::context::ToolUseContext;
use tool_api::progress::ToolProgressSender;
use tool_api::tool_trait::{
    DescriptionOptions, PromptOptions, Tool, ToolCallResult, ToolError, ToolStaticContext,
    ValidationError,
};
use tool_api::util::output_truncation::{truncate_shell_output, MAX_TOOL_OUTPUT_LENGTH};
use tool_api::BuiltinToolContext;

/// 2-minute REPL timeout (no per-call override).
pub const REPL_DEFAULT_TIMEOUT_MS: u64 = 120_000;
/// Tool name byte-lock.
pub const TOOL_NAME: &str = "REPL";

/// Whether the REPL tool is available — 1:1 port of claude-code 2.1.206 `kO()`.
///
/// The REPL tool is **default-OFF** (experimental). It is registered only when:
/// - `LINGXI_REPL` (claude-code `CLAUDE_CODE_REPL`) is truthy (`1`/`true`/…) — and
///   an explicitly-falsy value (`0`/`false`/`no`/`off`) force-disables it; else
/// - the entrypoint is `cli` or `remote` AND the `tengu_slate_harbor` statsig flag
///   is on (default `false`).
///
/// `kO()`'s leading `mQ()` guard is `return true` in 2.1.206, so it is omitted.
#[must_use]
pub fn is_repl_enabled() -> bool {
    is_repl_enabled_from(
        std::env::var("LINGXI_REPL").ok().as_deref(),
        std::env::var("CLAUDE_CODE_ENTRYPOINT").ok().as_deref(),
        telemetry::flag_bool("tengu_slate_harbor", false),
    )
}

/// Pure core of [`is_repl_enabled`] (`kO()` with its inputs injected, so it is
/// testable without mutating process-global env).
fn is_repl_enabled_from(
    repl_env: Option<&str>,
    entrypoint: Option<&str>,
    slate_harbor: bool,
) -> bool {
    // `su(CLAUDE_CODE_REPL)`: a defined, explicitly-falsy value disables.
    if is_env_defined_falsy(repl_env) {
        return false;
    }
    // `ut(CLAUDE_CODE_REPL)`: a truthy value enables.
    if platform_api::env::is_env_truthy(repl_env) {
        return true;
    }
    // Entrypoint `cli`/`remote` → the `tengu_slate_harbor` flag (default `false`).
    matches!(entrypoint, Some("cli" | "remote")) && slate_harbor
}

/// Port of claude-code `isEnvDefinedFalsy` (`envUtils.ts`): a defined, non-empty
/// value that normalizes (lowercase + trim) to `0`/`false`/`no`/`off`. `None` or
/// an empty string is NOT falsy.
fn is_env_defined_falsy(env_var: Option<&str>) -> bool {
    match env_var {
        None => false,
        Some(v) if v.is_empty() => false,
        Some(v) => matches!(v.to_lowercase().trim(), "0" | "false" | "no" | "off"),
    }
}

/// `(executable, args, stdin?)` for the given language. Returns `None` for unsupported.
#[must_use]
pub fn lang_exec(lang: &str, code: &str) -> Option<(&'static str, Vec<String>, Option<String>)> {
    match lang {
        "python" => Some((
            if cfg!(target_os = "windows") {
                "python.exe"
            } else {
                "python3"
            },
            vec!["-".into()],
            Some(code.into()),
        )),
        "node" => Some(("node", vec![], Some(code.into()))),
        "ruby" => Some(("ruby", vec!["-e".into(), code.into()], None)),
        _ => None,
    }
}

/// `REPLTool` — runs python/node/ruby snippets through the configured runner.
#[derive(Clone)]
pub struct REPLTool {
    ctx: BuiltinToolContext,
}

impl REPLTool {
    /// Construct a fresh tool.
    #[must_use]
    pub fn new(ctx: BuiltinToolContext) -> Self {
        Self { ctx }
    }
}

static INPUT_SCHEMA: Lazy<Value> = Lazy::new(|| {
    json!({
        "type": "object",
        "properties": {
            "language": { "type": "string", "enum": ["python", "node", "ruby"] },
            "code":     { "type": "string" }
        },
        "required": ["language", "code"]
    })
});

#[async_trait]
impl Tool for REPLTool {
    fn name(&self) -> &str {
        TOOL_NAME
    }
    fn input_schema(&self) -> &Value {
        &INPUT_SCHEMA
    }
    fn is_enabled(&self, _: &ToolStaticContext) -> bool {
        true
    }
    fn max_result_size_chars(&self) -> usize {
        MAX_TOOL_OUTPUT_LENGTH
    }
    fn is_concurrency_safe(&self, _: &Value) -> bool {
        false
    }
    fn is_read_only(&self, _: &Value) -> bool {
        false
    }
    fn is_destructive(&self, _: &Value) -> bool {
        false
    }

    async fn check_permissions(&self, _: &Value, _: &ToolUseContext) -> PermissionResult {
        PermissionResult::Allow {
            reason: PermissionDecisionReason::Other {
                reason: "allow-all-gate (M4-02 default)".into(),
            },
            updated_input: None,
            update_destination: None,
            metadata: PermissionMetadata::default(),
        }
    }

    async fn description(&self, input: &Value, _: &DescriptionOptions) -> String {
        let lang = input.get("language").and_then(Value::as_str).unwrap_or("?");
        format!("Running {lang} REPL")
    }

    async fn prompt(&self, _: &PromptOptions) -> String {
        "Execute a code snippet in python, node, or ruby.".into()
    }

    async fn validate_input(
        &self,
        input: &Value,
        _: &ToolUseContext,
    ) -> Result<(), ValidationError> {
        let lang = input
            .get("language")
            .and_then(Value::as_str)
            .ok_or_else(|| ValidationError("missing `language`".into()))?;
        if !matches!(lang, "python" | "node" | "ruby") {
            return Err(ValidationError(format!(
                "unsupported REPL language: {lang}"
            )));
        }
        input
            .get("code")
            .and_then(Value::as_str)
            .ok_or_else(|| ValidationError("missing `code`".into()))?;
        Ok(())
    }

    async fn call(
        &self,
        input: Value,
        _ctx: ToolUseContext,
        _progress_tx: ToolProgressSender,
    ) -> Result<ToolCallResult, ToolError> {
        use platform_api::sandbox::ProcessCommand as SbxCommand;

        let lang = input
            .get("language")
            .and_then(Value::as_str)
            .ok_or_else(|| ToolError::InvalidInput("missing language".into()))?
            .to_string();
        let code = input
            .get("code")
            .and_then(Value::as_str)
            .ok_or_else(|| ToolError::InvalidInput("missing code".into()))?;

        let (exe, args, stdin_payload) = lang_exec(&lang, code)
            .ok_or_else(|| ToolError::InvalidInput(format!("unsupported REPL language: {lang}")))?;

        let mut meta_start: LogEventMetadata = HashMap::new();
        meta_start.insert("language".into(), AnalyticsValue::String(lang.clone()));
        self.ctx.bus.log_event(REPL_STARTED, meta_start).await;

        let pcmd = SbxCommand {
            command: exe.into(),
            args,
            cwd: Some(self.ctx.cwd()),
            env: HashMap::new(),
            timeout: Some(Duration::from_millis(REPL_DEFAULT_TIMEOUT_MS)),
            stdin: stdin_payload,
        };
        let sandboxed = self.ctx.sandbox.bypass_with_audit(pcmd, "repl_tool_call");

        match self.ctx.process.run(&sandboxed).await {
            Ok(out) => {
                let (stdout_clean, _) = strip_ansi_count(&out.stdout);
                let (stderr_clean, _) = strip_ansi_count(&out.stderr);
                let (stdout_final, truncated) =
                    truncate_shell_output(stdout_clean, MAX_TOOL_OUTPUT_LENGTH);
                let is_error = out.exit_code != 0 || out.timed_out;

                let mut meta: LogEventMetadata = HashMap::new();
                meta.insert("language".into(), AnalyticsValue::String(lang.clone()));
                meta.insert(
                    "exit_code".into(),
                    AnalyticsValue::Int(i64::from(out.exit_code)),
                );
                meta.insert("truncated".into(), AnalyticsValue::Bool(truncated));
                self.ctx.bus.log_event(REPL_COMPLETED, meta).await;

                Ok(ToolCallResult {
                    data: json!({
                        "language":  lang,
                        "exit_code": out.exit_code,
                        "stdout":    stdout_final,
                        "stderr":    stderr_clean,
                        "is_error":  is_error,
                        "truncated": truncated,
                    }),
                    model_content: None,
                    new_messages: vec![],
                    context_modifier: None,
                    is_error: false,
                    mcp_meta: None,
                })
            }
            Err(e) => {
                let mut meta: LogEventMetadata = HashMap::new();
                meta.insert("language".into(), AnalyticsValue::String(lang));
                self.ctx.bus.log_event(REPL_FAILED, meta).await;
                Err(ToolError::Io(format!("{e}")))
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use platform_api::process::ProcessOutput;
    use tool_api::test_support::{fresh_ctx, fresh_tx, shell_test_ctx};

    #[test]
    fn lang_exec_python_routes_via_stdin() {
        let (exe, args, stdin) = lang_exec("python", "print(1)").unwrap();
        if cfg!(target_os = "windows") {
            assert_eq!(exe, "python.exe");
        } else {
            assert_eq!(exe, "python3");
        }
        assert_eq!(args, vec!["-".to_string()]);
        assert_eq!(stdin.as_deref(), Some("print(1)"));
    }

    #[test]
    fn lang_exec_ruby_routes_via_dash_e() {
        let (exe, args, stdin) = lang_exec("ruby", "puts 1").unwrap();
        assert_eq!(exe, "ruby");
        assert_eq!(args, vec!["-e".to_string(), "puts 1".to_string()]);
        assert!(stdin.is_none());
    }

    #[test]
    fn lang_exec_node_routes_via_stdin_no_args() {
        let (exe, args, stdin) = lang_exec("node", "console.log(1)").unwrap();
        assert_eq!(exe, "node");
        assert!(args.is_empty());
        assert_eq!(stdin.as_deref(), Some("console.log(1)"));
    }

    #[test]
    fn lang_exec_unknown_returns_none() {
        assert!(lang_exec("rust", "fn main(){}").is_none());
    }

    #[test]
    fn locked_constants_unchanged() {
        assert_eq!(REPL_DEFAULT_TIMEOUT_MS, 120_000);
        assert_eq!(TOOL_NAME, "REPL");
    }

    #[test]
    fn is_env_defined_falsy_semantics() {
        assert!(!is_env_defined_falsy(None));
        assert!(!is_env_defined_falsy(Some("")));
        for v in ["0", "false", "no", "off", "OFF", " False "] {
            assert!(is_env_defined_falsy(Some(v)), "{v} should be falsy");
        }
        for v in ["1", "true", "yes", "on", "anything"] {
            assert!(!is_env_defined_falsy(Some(v)), "{v} should NOT be falsy");
        }
    }

    #[test]
    fn repl_gate_default_off() {
        // No LINGXI_REPL + no cli/remote entrypoint + flag off → disabled (default).
        assert!(!is_repl_enabled_from(None, None, false));
        // cli/remote entrypoint but `tengu_slate_harbor` flag off → still off.
        assert!(!is_repl_enabled_from(None, Some("cli"), false));
        assert!(!is_repl_enabled_from(None, Some("remote"), false));
        // A non-cli/remote entrypoint never enables via the flag.
        assert!(!is_repl_enabled_from(None, Some("sdk-ts"), true));
    }

    #[test]
    fn repl_gate_flag_enables_in_cli_remote() {
        assert!(is_repl_enabled_from(None, Some("cli"), true));
        assert!(is_repl_enabled_from(None, Some("remote"), true));
    }

    #[test]
    fn repl_gate_env_truthy_enables_falsy_disables() {
        // Truthy LINGXI_REPL enables regardless of entrypoint/flag.
        assert!(is_repl_enabled_from(Some("1"), None, false));
        assert!(is_repl_enabled_from(Some("true"), None, false));
        // Explicitly-falsy LINGXI_REPL disables, even in cli with the flag on.
        assert!(!is_repl_enabled_from(Some("0"), Some("cli"), true));
        assert!(!is_repl_enabled_from(Some("off"), Some("remote"), true));
    }

    #[tokio::test]
    async fn validate_rejects_unknown_language() {
        let ctx = shell_test_ctx(ProcessOutput {
            stdout: String::new(),
            stderr: String::new(),
            exit_code: 0,
            timed_out: false,
        });
        let tool = REPLTool::new(ctx);
        let r = tool
            .validate_input(
                &json!({"language": "rust", "code": "fn main(){}"}),
                &fresh_ctx(),
            )
            .await;
        assert!(r.is_err());
        assert!(r.unwrap_err().to_string().contains("unsupported"));
    }

    #[tokio::test]
    async fn foreground_python_returns_stdout() {
        let ctx = shell_test_ctx(ProcessOutput {
            stdout: "hi\n".into(),
            stderr: String::new(),
            exit_code: 0,
            timed_out: false,
        });
        let tool = REPLTool::new(ctx);
        let r = tool
            .call(
                json!({"language": "python", "code": "print('hi')"}),
                fresh_ctx(),
                fresh_tx(),
            )
            .await
            .expect("ok");
        assert_eq!(r.data["stdout"], "hi\n");
        assert_eq!(r.data["language"], "python");
    }
}
