//! `/doctor` check runners. Ships 7 checks (T2 step 6 + T12; plus the global
//! `~/.lingxi.json` config probe — claude-code's doctor diagnoses both the
//! `tr()`-rooted config-home tree AND the global config file).
//!
//! Each check returns a [`DoctorCheck`] with a `Pass`/`Warn`/`Fail` status.
//! The aggregate [`DoctorReport`] is what the `/doctor` slash-command
//! handler renders (one row per check) into the Summary text panel.

use lingxi_core::host::{CheckStatus, DoctorCheck, DoctorReport, DoctorSummary};
use std::path::Path;

/// Run all doctor checks against the supplied config-dir root (the `tr()`-rooted
/// config-home tree) plus the global config file (`~/.lingxi.json`, passed
/// separately because it is the one path NOT under `tr()`), and aggregate the
/// results.
pub async fn run_all(config_dir: &Path, global_config_path: Option<&Path>) -> DoctorReport {
    let checks = vec![
        check_config_dir(config_dir).await,
        check_global_config(global_config_path).await,
        check_api_key(),
        check_network().await,
        check_disk_space(config_dir).await,
        check_git().await,
        check_telemetry_schema(),
    ];

    let mut summary = DoctorSummary::default();
    for c in &checks {
        match c.status {
            CheckStatus::Pass => summary.passed += 1,
            CheckStatus::Warn => summary.warnings += 1,
            CheckStatus::Fail => summary.failed += 1,
        }
    }
    DoctorReport { checks, summary }
}

async fn check_config_dir(p: &Path) -> DoctorCheck {
    let exists = tokio::fs::metadata(p).await.is_ok();
    if !exists {
        // Try to create it — many users won't have run /memory or /config yet.
        if tokio::fs::create_dir_all(p).await.is_err() {
            return DoctorCheck {
                name: "config-dir".to_string(),
                status: CheckStatus::Fail,
                detail: Some(format!(
                    "{} does not exist and cannot be created",
                    p.display()
                )),
            };
        }
    }
    let probe = p.join(".lingxi_writable_probe");
    let writable = tokio::fs::write(&probe, b"").await.is_ok();
    let _ = tokio::fs::remove_file(&probe).await;
    DoctorCheck {
        name: "config-dir".to_string(),
        status: if writable {
            CheckStatus::Pass
        } else {
            CheckStatus::Fail
        },
        detail: if writable {
            None
        } else {
            Some(format!("{} is not writable", p.display()))
        },
    }
}

/// Probe the global config file `~/.lingxi.json` — claude-code's doctor names
/// this file literally in its writability error and diagnoses it alongside the
/// config-home tree. A corrupt (unparseable) global config breaks every config
/// read, so it Fails; a present-but-read-only file Fails; an absent file is fine
/// (it is created on the first config write). `None` (path unresolvable) Warns.
async fn check_global_config(path: Option<&Path>) -> DoctorCheck {
    let name = "global-config".to_string();
    let Some(path) = path else {
        return DoctorCheck {
            name,
            status: CheckStatus::Warn,
            detail: Some("could not resolve the global config (~/.lingxi.json) path".to_string()),
        };
    };
    match tokio::fs::read(path).await {
        Ok(bytes) => {
            if serde_json::from_slice::<serde_json::Value>(&bytes).is_err() {
                return DoctorCheck {
                    name,
                    status: CheckStatus::Fail,
                    detail: Some(format!(
                        "{} is not valid JSON (corrupt global config)",
                        path.display()
                    )),
                };
            }
            // Writability probe: append-open neither modifies nor truncates the
            // file (we write nothing), it only proves the file can be opened for
            // writing — non-destructive, unlike the config-dir scratch probe.
            let writable = tokio::fs::OpenOptions::new()
                .append(true)
                .open(path)
                .await
                .is_ok();
            DoctorCheck {
                name,
                status: if writable {
                    CheckStatus::Pass
                } else {
                    CheckStatus::Fail
                },
                detail: if writable {
                    None
                } else {
                    Some(format!("{} is not writable", path.display()))
                },
            }
        }
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => DoctorCheck {
            name,
            status: CheckStatus::Pass,
            detail: Some(format!("{} not yet created", path.display())),
        },
        Err(e) => DoctorCheck {
            name,
            status: CheckStatus::Fail,
            detail: Some(format!("{} is unreadable: {e}", path.display())),
        },
    }
}

fn check_api_key() -> DoctorCheck {
    let has_env = std::env::var_os("ANTHROPIC_API_KEY").is_some_and(|v| !v.is_empty());
    // M5-11 only checks the env var. A full keychain check (M2-06) is
    // tracked as a M5-14 follow-up.
    DoctorCheck {
        name: "api-key".to_string(),
        status: if has_env {
            CheckStatus::Pass
        } else {
            CheckStatus::Warn
        },
        detail: if has_env {
            None
        } else {
            Some("ANTHROPIC_API_KEY not set (sign in via /login or export the env var)".to_string())
        },
    }
}

async fn check_network() -> DoctorCheck {
    // TCP-reach probe rather than a full HTTPS request — avoids dragging an
    // HTTP transport into the doctor path. Uses std::net via spawn_blocking
    // because `lingxi-orchestrator`'s tokio feature set does not include
    // `net`. 5-second timeout.
    let res = tokio::task::spawn_blocking(|| {
        use std::net::ToSocketAddrs;
        let timeout = std::time::Duration::from_secs(5);
        let addrs: Vec<_> = match "api.anthropic.com:443".to_socket_addrs() {
            Ok(it) => it.collect(),
            Err(e) => return Err(format!("DNS failure: {e}")),
        };
        let addr = match addrs.first() {
            Some(a) => *a,
            None => return Err("DNS returned no addresses".to_string()),
        };
        match std::net::TcpStream::connect_timeout(&addr, timeout) {
            Ok(_) => Ok(()),
            Err(e) => Err(format!("connect failed: {e}")),
        }
    })
    .await;
    match res {
        Ok(Ok(())) => DoctorCheck {
            name: "network".to_string(),
            status: CheckStatus::Pass,
            detail: None,
        },
        Ok(Err(detail)) => DoctorCheck {
            name: "network".to_string(),
            status: CheckStatus::Fail,
            detail: Some(detail),
        },
        Err(e) => DoctorCheck {
            name: "network".to_string(),
            status: CheckStatus::Fail,
            detail: Some(format!("probe task panicked: {e}")),
        },
    }
}

async fn check_disk_space(p: &Path) -> DoctorCheck {
    // Simple probe: try to write a 1-byte test file. If that succeeds we
    // can't *measure* free space without an extra crate, so we report Pass
    // as a coarse Boolean. M5-14 may add a `fs2`-based size check.
    let probe = p.join(".lingxi_disk_probe");
    let _ = tokio::fs::create_dir_all(p).await;
    let writable = tokio::fs::write(&probe, b"\0").await.is_ok();
    let _ = tokio::fs::remove_file(&probe).await;
    DoctorCheck {
        name: "disk-space".to_string(),
        status: if writable {
            CheckStatus::Pass
        } else {
            CheckStatus::Warn
        },
        detail: if writable {
            None
        } else {
            Some(format!(
                "cannot write to {} (disk may be full)",
                p.display()
            ))
        },
    }
}

async fn check_git() -> DoctorCheck {
    let output = tokio::process::Command::new("git")
        .arg("--version")
        .output()
        .await;
    match output {
        Ok(o) if o.status.success() => DoctorCheck {
            name: "git".to_string(),
            status: CheckStatus::Pass,
            detail: Some(String::from_utf8_lossy(&o.stdout).trim().to_string()),
        },
        _ => DoctorCheck {
            name: "git".to_string(),
            status: CheckStatus::Warn,
            detail: Some("git not found (some features will be limited)".to_string()),
        },
    }
}

fn check_telemetry_schema() -> DoctorCheck {
    let actual = telemetry::tengu::ALL_EVENT_NAMES.len();
    // M5-14: +1 release marker (lingxi_core_v0_6_0_released) → 315 total.
    // M6-01: +4 TUI lifecycle events (tengu_tui_*) → 319 total.
    // M6-03: +2 TUI streaming render events → 321 total.
    // M6-05: +2 TUI permission dialog events → 323 total.
    // M6-09: +1 release marker (lingxi_core_v0_7_0_released) +
    //        2 TUI scroll events (scroll_started/ended) → 326 total.
    //        (tengu_tui_key_pressed deferred to M7.)
    // M7-01..M7-15: +0 (every TUI event candidate deferred to the M7-16 audit).
    // M7-16: +1 release marker (lingxi_core_v0_8_0_released) +
    //        3 TUI events (screen_opened/screen_closed/search_opened, real
    //        emit sites) → 330 total. (command_palette_opened/vim_mode_entered/
    //        key_pressed deferred to M8 — no clean/aggregated emit site.)
    // grep/glob removal: −6 fabricated tengu_tool_grep_*/glob_* events (#29) → 324.
    // LSP.7b: +6 CronDelete/CronList tool events → 330.
    // FileRead analytics: +4 global-tail events (tengu_file_read_dedup/
    //        session_file_read/file_read_limits_override/file_read_reread #13) → 334.
    // Config migrations: +9 (migration::NAMES, runMigrations port) → 343.
    // Permission flow: +1 (permission::NAMES, bypass dialog accept) → 344.
    // Coordinator swarm: +3 (coordinator::NAMES — team_created/team_deleted/
    //        coordinator_mode_switched) → 347.
    // Strict-parity (2.1.195) removals reconciled against the real per-block
    // NAMES (see telemetry::tengu::ALL_EVENT_NAMES): D1 dropped the 3
    // tengu_tool_todo_write_* events (tool 134 → 131), D2 dropped port-only
    // tengu_cost_recorded (cost 10 → 9), D3 consolidated session-resume
    // (session 20 → 18). 347 − 3 − 1 − 2 = 341.
    // cc 2.1.198 M2: +2 AWS auth-refresh trust-gate events
    //        (tengu_awsAuthRefresh_missing_trust /
    //        tengu_awsCredentialExport_missing_trust) → 343.
    // 2.1.206 worktree lifecycle (merged from main): +4 tengu_worktree_ events
    //        (created / entered_existing / kept / removed) → 347.
    // 2.1.251 mcp/plugin byte alignment: +3 mcp::NAMES
    //        (tengu_mcp_server_config_invalid / tengu_mcp_tools_listed /
    //        tengu_mcp_degraded) +14 plugin::NAMES
    //        (tengu_plugin_enabled_for_session and its 13 siblings) → 364.
    // §11 discovery-cache wiring: +1 mcp::NAMES
    //        (tengu_mcp_discovery_source) → 365.
    //
    // 🚨 THIS COUNT IS PINNED IN FOUR PLACES. They must move together, and
    // they have now gone out of sync twice on the 2.1.251 backlog — this one
    // is the easiest to miss because it lives in `orchestrator`, not
    // `telemetry`, and it drives a USER-VISIBLE `/doctor` check rather than
    // only a test:
    //   1. telemetry/tests/event_name_completeness_test.rs  (the canonical one)
    //   2. telemetry/tests/settings_schema_test.rs           (a duplicate)
    //   3. telemetry/tests/mcp_schema_test.rs                (per-block: mcp::NAMES)
    //   4. HERE
    // Re-count against `mcp::NAMES.len()` / `plugin::NAMES.len()`; never paste
    // the number a failing assertion printed.
    // 2026-09-08 recount: tool::NAMES is 129 (not 135); all blocks
    // sum to 410, matching the checked-in tengu_events.json fixture.
    let expected = 410;
    DoctorCheck {
        name: "telemetry-schema".to_string(),
        status: if actual == expected {
            CheckStatus::Pass
        } else {
            CheckStatus::Fail
        },
        detail: if actual == expected {
            None
        } else {
            Some(format!(
                "ALL_EVENT_NAMES.len() = {actual}; expected {expected}"
            ))
        },
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn check_telemetry_schema_passes_at_expected_count() {
        let c = check_telemetry_schema();
        assert_eq!(c.name, "telemetry-schema");
        assert!(matches!(c.status, CheckStatus::Pass));
    }

    #[test]
    fn check_api_key_returns_pass_or_warn() {
        // We cannot safely mutate env vars in this crate (forbids unsafe).
        // Just check the call returns one of the expected statuses and the
        // correct name — both Pass and Warn are valid depending on whether
        // the test runner has ANTHROPIC_API_KEY set.
        let c = check_api_key();
        assert_eq!(c.name, "api-key");
        assert!(matches!(c.status, CheckStatus::Pass | CheckStatus::Warn));
    }

    #[tokio::test]
    async fn run_all_returns_7_checks() {
        let tmp = std::env::temp_dir().join("lingxi_diag_test");
        let global = tmp.join(".lingxi.json");
        let report = run_all(&tmp, Some(&global)).await;
        assert_eq!(report.checks.len(), 7, "doctor must run 7 checks");
        // Summary tallies match check count.
        let total = report.summary.passed + report.summary.warnings + report.summary.failed;
        assert_eq!(total, 7);
    }

    #[tokio::test]
    async fn global_config_check_corrupt_is_fail() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join(".lingxi.json");
        tokio::fs::write(&path, b"{ not json").await.unwrap();
        let c = check_global_config(Some(&path)).await;
        assert_eq!(c.name, "global-config");
        assert!(matches!(c.status, CheckStatus::Fail));
    }

    #[tokio::test]
    async fn global_config_check_valid_is_pass() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join(".lingxi.json");
        tokio::fs::write(&path, br#"{"numStartups":1}"#)
            .await
            .unwrap();
        let c = check_global_config(Some(&path)).await;
        assert!(matches!(c.status, CheckStatus::Pass));
    }

    #[tokio::test]
    async fn global_config_check_absent_is_pass() {
        // Fresh install: `~/.lingxi.json` not yet created — must Pass (it is
        // written on the first config change), not Fail.
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join(".lingxi.json"); // never created
        let c = check_global_config(Some(&path)).await;
        assert!(matches!(c.status, CheckStatus::Pass));
    }

    #[tokio::test]
    async fn global_config_check_none_path_is_warn() {
        let c = check_global_config(None).await;
        assert!(matches!(c.status, CheckStatus::Warn));
    }
}
