//! `InteractivePromptingGate` — stdin/stderr prompt loop.
//!
//! Drives a `[Y/n]` permission prompt over injectable `AsyncRead` /
//! `AsyncWrite` endpoints. Tests pipe via `tokio::io::duplex`.
//!
//! ## Divergence — this is a LingXi plain-REPL surface, NOT claude-code parity
//!
//! Verified against the 2.1.211 binary: claude-code has **no** stdin `[Y/n]`
//! permission prompt. `"needs your permission to use"` and `"[Y/n]"` have
//! **zero** occurrences in the 2.1.211 strings corpus. In claude-code an
//! unresolvable ask is resolved interactively by the TUI dialog, and in a
//! non-interactive / headless context it is **denied** with the `GRu` message
//! (`Permission to use ${tool} has been denied. …` — see
//! [`crate::headless_gate::DenyOnAskGate`]). The lone sibling string
//! `"Agent tool requires permission to spawn subagents."` is the Agent tool's
//! auto-mode `checkPermissions` *passthrough message*, not a prompt.
//!
//! This gate is therefore an **intentional LingXi divergence** that backs the
//! plain (non-TUI) REPL surface, where there is no dialog to render. The prompt
//! literals below are LingXi's own strings — they are byte-locked by the tests
//! only so the REPL UX stays stable, not because they mirror claude-code.
//!
//! M5-05 task progression:
//! - Task 5: `format_prompt` byte-locks the prompt literals.
//! - Task 6: `parse_user_input` + `resolve_outcome`.
//! - Task 7 (RED) → Task 8 (GREEN): `PromptingGate::prompt_user` impl.
//! - Tasks 9-10: empty-input and retry-then-error tests.
//! - Task 11: `PermissionGate` upcast impl + orchestrator wiring.
//! - Task 12: telemetry emission for `permission_prompted` /
//!   `permission_answered`.
#![forbid(unsafe_code)]

use std::sync::Arc;

use async_trait::async_trait;
use tokio::io::{AsyncBufRead, AsyncBufReadExt, AsyncWrite, AsyncWriteExt, BufReader};
use tokio::sync::Mutex;

use crate::gate::{
    PermissionDecision, PermissionGate, PermissionRequest, PromptDecision, PromptDefault,
    PromptError, PromptingGate,
};

/// Maximum number of consecutive invalid inputs the gate tolerates before
/// erroring out. This is a LingXi plain-REPL convention (claude-code has no
/// stdin prompt loop, hence no such retry cap — see the module docs), fixed
/// at 3.
const MAX_RETRIES: u32 = 3;

/// Format the prompt for the stdio path's
/// `PermissionRequest::ToolUseConfirm` variant.
///
/// **Divergence:** these are LingXi's own plain-REPL strings — claude-code has
/// no stdin `[Y/n]` permission prompt (see the module docs). They are locked by
/// the tests to keep the REPL UX stable, not to mirror the binary.
///
/// - Generic tools: `"{PRODUCT_NAME} needs your permission to use {tool_name}\n[Y/n] "`
///   or `[y/N]` depending on the tool's default.
/// - `Agent`:
///   `"Agent tool requires permission to spawn subagents.\n[Y/n] "`.
///
/// The suffix bracket pair is always followed by a single space.
///
/// Only meaningful for `ToolUseConfirm` — the other variants
/// (`ExitPlanMode`, `BypassPermissionsMode`) are TUI-only and return
/// `PromptError::Cancelled` at the `PromptingGate` layer.
pub(crate) fn format_prompt_tool_use(tool_name: &str, default_decision: PromptDefault) -> String {
    let suffix = match default_decision {
        PromptDefault::AllowByDefault => "[Y/n] ",
        PromptDefault::DenyByDefault => "[y/N] ",
    };
    if tool_name == "Agent" {
        format!("Agent tool requires permission to spawn subagents.\n{suffix}")
    } else {
        format!(
            "{} needs your permission to use {tool_name}\n{suffix}",
            branding::PRODUCT_NAME
        )
    }
}

/// Interactive permission gate driven by stdin/stderr.
///
/// Production wiring (M5-12) constructs with `tokio::io::stdin()` +
/// `tokio::io::stderr()`. Tests use `tokio::io::duplex` scripts.
pub struct InteractivePromptingGate {
    // A SHARED `AsyncBufRead` over fd 0. The REPL loop owns the single
    // `BufReader<Stdin>` and clones the `Arc<Mutex<…>>` into the gate, so
    // both sides read from the SAME buffer — no second `BufReader` is created
    // here (that would double-buffer and strand the REPL's type-ahead bytes).
    pub(crate) stdin: Arc<Mutex<dyn AsyncBufRead + Send + Unpin>>,
    pub(crate) stderr: Arc<Mutex<dyn AsyncWrite + Send + Unpin>>,
}

impl InteractivePromptingGate {
    /// Construct a new interactive gate over the given stdin / stderr endpoints.
    ///
    /// `stdin` is a SHARED `AsyncBufRead` — the caller (the REPL loop) owns the
    /// single buffered reader over fd 0 and hands a clone here so the gate reads
    /// from the same buffer rather than wrapping a fresh one.
    #[must_use]
    pub fn new(
        stdin: Arc<Mutex<dyn AsyncBufRead + Send + Unpin>>,
        stderr: Arc<Mutex<dyn AsyncWrite + Send + Unpin>>,
    ) -> Self {
        Self { stdin, stderr }
    }

    /// Convenience constructor over the process's real stdin / stderr.
    ///
    /// Used by M5-12 CLI binary when
    /// [`OrchestratorConfig::interactive_permissions`] is `true`.
    /// Wraps `tokio::io::stdin()` and `tokio::io::stderr()` in the
    /// `Arc<Mutex<…>>` newtype the trait surface expects.
    ///
    /// [`OrchestratorConfig::interactive_permissions`]: ../../lingxi_orchestrator/struct.OrchestratorConfig.html#structfield.interactive_permissions
    #[must_use]
    pub fn with_stdio() -> Self {
        Self::new(
            Arc::new(Mutex::new(BufReader::new(tokio::io::stdin()))),
            Arc::new(Mutex::new(tokio::io::stderr())),
        )
    }
}

/// One step of input parsing. `Valid*` means the user produced a definitive
/// answer; `Invalid` means we should re-prompt.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum ParseOutcome {
    /// User typed a yes-variant (`y/Y/yes/YES/…`).
    ValidYes,
    /// User typed a no-variant (`n/N/no/NO/…`).
    ValidNo,
    /// User just pressed Enter — caller resolves against the default.
    Empty,
    /// Anything else.
    Invalid,
}

/// Parse a single line of user input.
///
/// Trims trailing `\r?\n` and any surrounding whitespace. Empty (after
/// trim) ⇒ [`ParseOutcome::Empty`]. Otherwise compares the lowercased
/// token against `y` / `yes` / `n` / `no`.
pub(crate) fn parse_user_input(line: &str) -> ParseOutcome {
    let trimmed = line.trim();
    if trimmed.is_empty() {
        return ParseOutcome::Empty;
    }
    match trimmed.to_ascii_lowercase().as_str() {
        "y" | "yes" => ParseOutcome::ValidYes,
        "n" | "no" => ParseOutcome::ValidNo,
        _ => ParseOutcome::Invalid,
    }
}

/// Resolve a [`ParseOutcome`] into a definitive Allow/Deny against a
/// default. `Invalid` is the only outcome that returns `None` (caller
/// re-prompts).
pub(crate) fn resolve_outcome(outcome: ParseOutcome, default: PromptDefault) -> Option<bool> {
    match outcome {
        ParseOutcome::ValidYes => Some(true),
        ParseOutcome::ValidNo => Some(false),
        ParseOutcome::Empty => Some(matches!(default, PromptDefault::AllowByDefault)),
        ParseOutcome::Invalid => None,
    }
}

#[async_trait]
impl PermissionGate for InteractivePromptingGate {
    async fn check(&self, name: &str, input: &serde_json::Value) -> PermissionDecision {
        let request = PermissionRequest::ToolUseConfirm {
            tool_name: name.to_string(),
            tool_input: input.clone(),
            default_decision: crate::defaults_per_tool::tool_default(name),
            // This gate is a bare stdin y/N prompt with no persistent-rule
            // concept at all (`PromptDecision::persist` is always false — see
            // its doc comment), so there is nothing to suppress.
            suppress_always_allow_rule: false,
        };
        match self.prompt_user(&request).await {
            Ok(d) if d.allow => PermissionDecision::Allow,
            Ok(d) => PermissionDecision::Deny { reason: d.reason },
            Err(PromptError::InvalidInput { attempts }) => PermissionDecision::Deny {
                reason: format!("invalid permission input after {attempts} attempts"),
            },
            Err(PromptError::Cancelled { reason }) => PermissionDecision::Deny {
                reason: format!("prompt cancelled: {reason}"),
            },
            Err(PromptError::Io(reason)) => PermissionDecision::Deny {
                reason: format!("prompt io: {reason}"),
            },
        }
    }

    async fn check_exit_plan_mode(
        &self,
        plan: &str,
        _ctx: &crate::gate::PermissionCheckContext,
    ) -> crate::gate::PermissionOutcome {
        // Keep the dedicated request variant all the way through this
        // transport.  `prompt_user` intentionally rejects multiline plan
        // dialogs on plain stdio, so this resolves to a fail-closed denial
        // instead of silently degrading to a generic ToolUseConfirm prompt.
        let request = PermissionRequest::ExitPlanMode {
            plan: plan.to_string(),
        };
        match self.prompt_user(&request).await {
            Ok(decision) if decision.allow => crate::gate::PermissionOutcome::Allow {
                updated_input: None,
                permission_updates: Vec::new(),
                decision_classification: None,
            },
            Ok(decision) => crate::gate::PermissionOutcome::Deny {
                reason: decision.reason,
            },
            Err(PromptError::InvalidInput { attempts }) => crate::gate::PermissionOutcome::Deny {
                reason: format!("invalid permission input after {attempts} attempts"),
            },
            Err(PromptError::Cancelled { reason }) => crate::gate::PermissionOutcome::Deny {
                reason: format!("prompt cancelled: {reason}"),
            },
            Err(PromptError::Io(reason)) => crate::gate::PermissionOutcome::Deny {
                reason: format!("prompt io: {reason}"),
            },
        }
    }
}

#[async_trait]
impl PromptingGate for InteractivePromptingGate {
    async fn prompt_user(
        &self,
        request: &PermissionRequest,
    ) -> Result<PromptDecision, PromptError> {
        // M6-05: the stdio gate only handles the ToolUseConfirm variant.
        // ExitPlanMode and BypassPermissionsMode require multiline dialogs
        // that the TUI gate owns; the stdio path returns a structured
        // cancellation so the orchestrator falls through to Deny.
        let (tool_name, default_decision) = match request {
            PermissionRequest::ToolUseConfirm {
                tool_name,
                default_decision,
                ..
            } => (tool_name.as_str(), *default_decision),
            PermissionRequest::ExitPlanMode { .. } | PermissionRequest::BypassPermissionsMode => {
                return Err(PromptError::Cancelled {
                    reason: "stdio gate cannot render multiline permission dialogs".to_string(),
                });
            }
        };
        let prompt = format_prompt_tool_use(tool_name, default_decision);
        let default_allow = matches!(default_decision, PromptDefault::AllowByDefault);
        let mut attempts: u32 = 0;
        // Acquire the SHARED stdin lock ONCE for the full prompt round-trip.
        // The guard is already an `AsyncBufRead` (the REPL owns the single
        // `BufReader<Stdin>`), so we read `read_line` directly off it — no
        // second `BufReader`. Wrapping a fresh buffer here would silently
        // strand bytes the shared reader had pre-fetched past the first
        // newline: on retry inputs (`foo\nbar\nbaz\n`) only the first `foo\n`
        // would be consumed, and any REPL type-ahead would be lost too.
        let mut in_guard = self.stdin.lock().await;
        loop {
            // Telemetry: prompt about to be shown (once per attempt).
            tracing::info!(
                event = telemetry::tengu::orchestrator::PERMISSION_PROMPTED,
                tool_name = %tool_name,
                default_allow,
            );

            // 1. Write the prompt to stderr.
            {
                let mut err = self.stderr.lock().await;
                err.write_all(prompt.as_bytes())
                    .await
                    .map_err(|e| PromptError::Io(e.to_string()))?;
                err.flush()
                    .await
                    .map_err(|e| PromptError::Io(e.to_string()))?;
            }
            // 2. Read one line from stdin.
            let line = {
                let mut buf = String::new();
                let n = (*in_guard)
                    .read_line(&mut buf)
                    .await
                    .map_err(|e| PromptError::Io(e.to_string()))?;
                if n == 0 {
                    return Err(PromptError::Cancelled {
                        reason: "stdin closed".to_string(),
                    });
                }
                buf
            };
            // 3. Classify and decide.
            let outcome = parse_user_input(&line);
            if let Some(allow) = resolve_outcome(outcome, default_decision) {
                let reason = match outcome {
                    ParseOutcome::ValidYes => "user typed 'y'".to_string(),
                    ParseOutcome::ValidNo => "user typed 'n'".to_string(),
                    ParseOutcome::Empty => format!(
                        "user pressed Enter (default = {})",
                        if allow { "allow" } else { "deny" }
                    ),
                    ParseOutcome::Invalid => {
                        unreachable!("resolve_outcome returned Some for Invalid")
                    }
                };
                // Telemetry: definitive answer (NOT fired on retry).
                tracing::info!(
                    event = telemetry::tengu::orchestrator::PERMISSION_ANSWERED,
                    tool_name = %tool_name,
                    allowed = allow,
                    attempts = attempts + 1,
                );
                return Ok(PromptDecision {
                    allow,
                    reason,
                    persist: false,
                });
            }
            attempts += 1;
            if attempts >= MAX_RETRIES {
                return Err(PromptError::InvalidInput { attempts });
            }
        }
    }
}

// PermissionGate upcast lands in Task 11.

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn format_prompt_generic_allow_default_byte_locked() {
        let s = format_prompt_tool_use("Read", PromptDefault::AllowByDefault);
        assert_eq!(
            s.as_bytes(),
            format!(
                "{} needs your permission to use Read\n[Y/n] ",
                branding::PRODUCT_NAME
            )
            .as_bytes()
        );
    }

    #[test]
    fn format_prompt_generic_deny_default_byte_locked() {
        let s = format_prompt_tool_use("Bash", PromptDefault::DenyByDefault);
        assert_eq!(
            s.as_bytes(),
            format!(
                "{} needs your permission to use Bash\n[y/N] ",
                branding::PRODUCT_NAME
            )
            .as_bytes()
        );
    }

    #[test]
    fn format_prompt_agent_special_byte_locked() {
        let s = format_prompt_tool_use("Agent", PromptDefault::AllowByDefault);
        assert_eq!(
            s.as_bytes(),
            b"Agent tool requires permission to spawn subagents.\n[Y/n] "
        );
    }

    /// Divergence guard (PERM-GATE-PROMPT-01): these prompt literals are
    /// LingXi's own plain-REPL strings, NOT claude-code parity. claude-code
    /// 2.1.211 has no stdin `[Y/n]` permission prompt — the string
    /// "needs your permission to use" and "[Y/n]" have zero occurrences in the
    /// binary. This test documents that the strings are intentionally
    /// LingXi-shaped so no future audit re-flags them as a byte-mismatch.
    #[test]
    fn prompt_strings_are_lingxi_divergence_not_cc_byte_lock() {
        let s = format_prompt_tool_use("Read", PromptDefault::AllowByDefault);
        // The literal contains the LingXi-only "[Y/n]" affordance that has no
        // counterpart in the claude-code binary.
        assert!(s.contains("[Y/n] "));
        assert!(s.starts_with(branding::PRODUCT_NAME));
        assert!(s.contains(" needs your permission to use"));
    }

    #[test]
    fn removed_task_alias_uses_generic_message() {
        let s = format_prompt_tool_use("Task", PromptDefault::AllowByDefault);
        assert_eq!(
            s.as_bytes(),
            format!(
                "{} needs your permission to use Task\n[Y/n] ",
                branding::PRODUCT_NAME
            )
            .as_bytes()
        );
    }

    // M6-05 Task 2: ExitPlanMode and BypassPermissionsMode variants return
    // PromptError::Cancelled at the stdio gate (TUI dialog owns them).

    #[tokio::test]
    async fn stdio_gate_returns_cancelled_for_exit_plan_mode() {
        use std::sync::Arc;
        use tokio::io::{duplex, AsyncWriteExt};
        use tokio::sync::Mutex;

        let (mut script, in_end) = duplex(64);
        script.write_all(b"").await.unwrap();
        drop(script);
        let (out_end, _drain) = duplex(64);
        let gate = InteractivePromptingGate::new(
            Arc::new(Mutex::new(BufReader::new(in_end))),
            Arc::new(Mutex::new(out_end)),
        );
        let req = PermissionRequest::ExitPlanMode {
            plan: "x".to_string(),
        };
        let err = gate.prompt_user(&req).await.unwrap_err();
        assert!(matches!(err, PromptError::Cancelled { .. }));
    }

    #[tokio::test]
    async fn stdio_gate_returns_cancelled_for_bypass_permissions() {
        use std::sync::Arc;
        use tokio::io::duplex;
        use tokio::sync::Mutex;

        let (_script, in_end) = duplex(64);
        let (out_end, _drain) = duplex(64);
        let gate = InteractivePromptingGate::new(
            Arc::new(Mutex::new(BufReader::new(in_end))),
            Arc::new(Mutex::new(out_end)),
        );
        let req = PermissionRequest::BypassPermissionsMode;
        let err = gate.prompt_user(&req).await.unwrap_err();
        assert!(matches!(err, PromptError::Cancelled { .. }));
    }

    // ---- parse_user_input + resolve_outcome (Task 6) ----

    #[test]
    fn parse_y_lowercase_is_valid_yes() {
        assert_eq!(parse_user_input("y\n"), ParseOutcome::ValidYes);
    }

    #[test]
    fn parse_y_uppercase_is_valid_yes() {
        assert_eq!(parse_user_input("Y\n"), ParseOutcome::ValidYes);
    }

    #[test]
    fn parse_yes_mixed_case_is_valid_yes() {
        assert_eq!(parse_user_input("YES\n"), ParseOutcome::ValidYes);
        assert_eq!(parse_user_input("Yes\n"), ParseOutcome::ValidYes);
        assert_eq!(parse_user_input("yEs\n"), ParseOutcome::ValidYes);
    }

    #[test]
    fn parse_n_lowercase_is_valid_no() {
        assert_eq!(parse_user_input("n\n"), ParseOutcome::ValidNo);
    }

    #[test]
    fn parse_no_uppercase_is_valid_no() {
        assert_eq!(parse_user_input("NO\n"), ParseOutcome::ValidNo);
    }

    #[test]
    fn parse_empty_is_empty() {
        assert_eq!(parse_user_input("\n"), ParseOutcome::Empty);
        assert_eq!(parse_user_input(""), ParseOutcome::Empty);
        assert_eq!(parse_user_input("   \n"), ParseOutcome::Empty);
    }

    #[test]
    fn parse_garbage_is_invalid() {
        assert_eq!(parse_user_input("maybe\n"), ParseOutcome::Invalid);
        assert_eq!(parse_user_input("42\n"), ParseOutcome::Invalid);
        assert_eq!(parse_user_input("yy\n"), ParseOutcome::Invalid);
    }

    #[test]
    fn parse_handles_carriage_return() {
        assert_eq!(parse_user_input("y\r\n"), ParseOutcome::ValidYes);
        assert_eq!(parse_user_input("\r\n"), ParseOutcome::Empty);
    }

    #[test]
    fn resolve_empty_with_allow_default_is_true() {
        assert_eq!(
            resolve_outcome(ParseOutcome::Empty, PromptDefault::AllowByDefault),
            Some(true)
        );
    }

    #[test]
    fn resolve_empty_with_deny_default_is_false() {
        assert_eq!(
            resolve_outcome(ParseOutcome::Empty, PromptDefault::DenyByDefault),
            Some(false)
        );
    }

    #[test]
    fn resolve_yes_overrides_deny_default() {
        assert_eq!(
            resolve_outcome(ParseOutcome::ValidYes, PromptDefault::DenyByDefault),
            Some(true)
        );
    }

    #[test]
    fn resolve_no_overrides_allow_default() {
        assert_eq!(
            resolve_outcome(ParseOutcome::ValidNo, PromptDefault::AllowByDefault),
            Some(false)
        );
    }

    #[test]
    fn resolve_invalid_is_none() {
        assert_eq!(
            resolve_outcome(ParseOutcome::Invalid, PromptDefault::AllowByDefault),
            None
        );
    }

    /// A SINGLE shared `BufReader` carrying `"y\n"` followed by a second line
    /// is read by the gate without losing the follow-up: after `prompt_user`
    /// consumes `"y\n"`, the SAME shared reader still yields the follow-up
    /// line. This proves the gate no longer wraps its own inner `BufReader`
    /// (which would strand the follow-up behind a dropped buffer).
    #[tokio::test]
    async fn shared_bufreader_does_not_lose_follow_up_line() {
        use std::sync::Arc;
        use tokio::io::{duplex, AsyncBufReadExt, AsyncWriteExt, BufReader};
        use tokio::sync::Mutex;

        let (mut writer, client) = duplex(1024);
        // Queue BOTH the gate's answer and a follow-up line up front.
        writer.write_all(b"y\nnext-line\n").await.unwrap();
        drop(writer);

        let shared: Arc<Mutex<dyn AsyncBufRead + Send + Unpin>> =
            Arc::new(Mutex::new(BufReader::new(client)));
        let (out_end, _drain) = duplex(1024);
        let gate = InteractivePromptingGate::new(shared.clone(), Arc::new(Mutex::new(out_end)));

        let decision = gate
            .prompt_user(&PermissionRequest::ToolUseConfirm {
                tool_name: "Bash".to_string(),
                tool_input: serde_json::json!({}),
                default_decision: PromptDefault::DenyByDefault,
                suppress_always_allow_rule: false,
            })
            .await
            .expect("prompt_user should succeed on `y`");
        assert!(decision.allow, "y should map to allow=true");

        // The follow-up line is still readable from the SAME shared reader —
        // the gate did not strand it behind a private buffer.
        let mut follow = String::new();
        let mut guard = shared.lock().await;
        let n = guard.read_line(&mut follow).await.unwrap();
        assert_eq!(n, "next-line\n".len(), "follow-up bytes must survive");
        assert_eq!(follow, "next-line\n");
    }
}
