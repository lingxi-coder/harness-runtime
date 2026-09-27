//! `/doctor` — render the diagnostic report (one row per check).
//!
//! See plan M5-11 T0 step 4 for the locked layout (one block per check;
//! `[OK]` / `[!!]` / `[XX]` glyphs; optional detail indented 6 spaces;
//! Summary line at the bottom).
//! Failure prefix: `"Could not run doctor: "` (currently unreachable —
//! `run_doctor_checks` is infallible).

use async_trait::async_trait;
use command_api::builtin_support::names::core_description;
use command_api::model::{BuiltinCommandHandler, CommandResult};
use command_api::parser::ParsedSlashCommand;
use platform_api::{CheckStatus, DoctorReport, OrchestratorHandle};
use std::sync::Arc;
use telemetry::tengu::command as cmd_evt;

/// `/doctor` handler — renders the locked diagnostic panel.
#[derive(Clone)]
pub struct DoctorHandler {
    handle: Arc<dyn OrchestratorHandle>,
}

impl DoctorHandler {
    /// Construct a `DoctorHandler` bound to the given orchestrator handle.
    #[must_use]
    pub fn new(handle: Arc<dyn OrchestratorHandle>) -> Self {
        Self { handle }
    }
}

#[async_trait]
impl BuiltinCommandHandler for DoctorHandler {
    async fn handle(&self, _args: &ParsedSlashCommand) -> CommandResult {
        telemetry::emit_command_started(cmd_evt::DOCTOR_STARTED);
        let report = self.handle.run_doctor_checks().await;
        telemetry::emit_command_completed(cmd_evt::DOCTOR_COMPLETED, "");
        CommandResult::Done {
            display: Some(render_doctor(&report)),
        }
    }
    fn name(&self) -> &str {
        "doctor"
    }
    fn description(&self) -> &str {
        core_description("doctor")
    }
}

/// Render the locked `/doctor` report.
#[must_use]
pub fn render_doctor(r: &DoctorReport) -> String {
    let mut out = String::from("Doctor:\n");
    for c in &r.checks {
        let (glyph, status_text) = match &c.status {
            CheckStatus::Pass => ("OK", "ok"),
            CheckStatus::Warn => ("!!", "warning"),
            CheckStatus::Fail => ("XX", "failed"),
        };
        out.push_str(&format!("  [{glyph}] {}: {status_text}\n", c.name));
        if let Some(detail) = &c.detail {
            out.push_str(&format!("      {detail}\n"));
        }
    }
    out.push_str(&format!(
        "  Summary: {} passed, {} warnings, {} failed\n",
        r.summary.passed, r.summary.warnings, r.summary.failed
    ));
    out
}

#[cfg(test)]
mod tests {
    use super::*;
    use orchestrator::test_support::MockOrchestratorHandle;
    use platform_api::{DoctorCheck, DoctorSummary};

    fn args() -> ParsedSlashCommand {
        ParsedSlashCommand {
            name: "doctor".into(),
            raw_args: String::new(),
            positional_args: vec![],
        }
    }

    #[tokio::test]
    async fn all_pass_report() {
        let mock = Arc::new(MockOrchestratorHandle::new());
        mock.set_doctor_report(DoctorReport {
            checks: vec![
                DoctorCheck {
                    name: "config-dir".into(),
                    status: CheckStatus::Pass,
                    detail: None,
                },
                DoctorCheck {
                    name: "api-key".into(),
                    status: CheckStatus::Pass,
                    detail: None,
                },
            ],
            summary: DoctorSummary {
                passed: 2,
                warnings: 0,
                failed: 0,
            },
        });
        let h = DoctorHandler::new(mock);
        if let CommandResult::Done { display: Some(s) } = h.handle(&args()).await {
            assert_eq!(
                s,
                "\
Doctor:
  [OK] config-dir: ok
  [OK] api-key: ok
  Summary: 2 passed, 0 warnings, 0 failed
"
            );
        } else {
            panic!();
        }
    }

    #[tokio::test]
    async fn mixed_with_details() {
        let mock = Arc::new(MockOrchestratorHandle::new());
        mock.set_doctor_report(DoctorReport {
            checks: vec![
                DoctorCheck {
                    name: "config-dir".into(),
                    status: CheckStatus::Pass,
                    detail: None,
                },
                DoctorCheck {
                    name: "api-key".into(),
                    status: CheckStatus::Warn,
                    detail: Some("ANTHROPIC_API_KEY not set".into()),
                },
                DoctorCheck {
                    name: "network".into(),
                    status: CheckStatus::Fail,
                    detail: Some("ping timed out".into()),
                },
            ],
            summary: DoctorSummary {
                passed: 1,
                warnings: 1,
                failed: 1,
            },
        });
        let h = DoctorHandler::new(mock);
        if let CommandResult::Done { display: Some(s) } = h.handle(&args()).await {
            assert_eq!(
                s,
                "\
Doctor:
  [OK] config-dir: ok
  [!!] api-key: warning
      ANTHROPIC_API_KEY not set
  [XX] network: failed
      ping timed out
  Summary: 1 passed, 1 warnings, 1 failed
"
            );
        } else {
            panic!();
        }
    }

    #[tokio::test]
    async fn name_and_description() {
        let mock = Arc::new(MockOrchestratorHandle::new());
        let h = DoctorHandler::new(mock);
        assert_eq!(h.name(), "doctor");
        assert_eq!(
            h.description(),
            "Diagnose and verify your LingXi installation and settings"
        );
    }
}
