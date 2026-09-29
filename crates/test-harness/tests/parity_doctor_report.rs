//! Byte-locked /doctor report layout vs golden text fixture (M5-11 T14).

use command_api::builtins::doctor::render_doctor;
use lingxi_core::host::{CheckStatus, DoctorCheck, DoctorReport, DoctorSummary};

const GOLDEN: &str = include_str!("../src/parity/fixtures/parity_doctor_report.txt");

#[test]
fn doctor_layout_matches_golden() {
    let r = DoctorReport {
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
    };
    assert_eq!(render_doctor(&r), GOLDEN);
}
