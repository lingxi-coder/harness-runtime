//! The `/debug` bundled skill — port of Claude Code 2.1.267's `Ao()` registrar
//! (`src_172124278.js` @98826).
//!
//! ```js
//! uo({ name:"debug",
//!      menuDescription:"Turn on debug logging and investigate problems",
//!      description:"Enable debug logging for this session and help diagnose issues",
//!      allowedTools:["Read","Grep","Glob"],
//!      argumentHint:"[issue description]",
//!      disableModelInvocation:!0, userInvocable:!0,
//!      async getPromptForCommand(e,o){ … } })
//! ```
//!
//! # Why this could not be ported before
//!
//! The recorded blocker was that this port has no session debug log to read.
//! That was true: `memory::retention` swept `<config-home>/debug/` and
//! preserved `latest` while nothing ever wrote there. The log now exists
//! (`apps/cli/src/logging.rs`), so the skill has something real to point at
//! instead of instructions aimed at an empty directory.
//!
//! # Two deliberate adaptations
//!
//! ⚠️ **Upstream ENABLES debug logging from inside the skill** (`await WY()`)
//! and branches its prompt on whether it was already on. This port installs its
//! tracing subscriber once at startup, so a running session cannot turn file
//! logging on mid-flight. Rather than pretend, the prompt says plainly that the
//! log only covers runs started with `--debug`, and tells the user how to get
//! one. ⛔ Do not "fix" this by claiming logging was just enabled — the log
//! would stay empty and the model would hunt for entries that cannot exist.
//!
//! ⚠️ Upstream step 3 suggests launching its `claude-code-guide` subagent. That
//! agent is a recorded LingXi divergence (removed on purpose), so the step is
//! dropped rather than pointed at an agent this port does not register.

use command_api::BundledPromptFn;
use std::path::{Path, PathBuf};

/// Upstream `pe` — how many trailing lines of the log to inline.
const TAIL_LINES: usize = 20;

pub(crate) const DEBUG_DESCRIPTION: &str =
    "Enable debug logging for this session and help diagnose issues";
pub(crate) const DEBUG_MENU_DESCRIPTION: &str = "Turn on debug logging and investigate problems";
pub(crate) const DEBUG_ARGUMENT_HINT: &str = "[issue description]";

/// `<config-home>` — same resolution as the rest of this crate.
fn lingxi_home_dir() -> PathBuf {
    if let Some(dir) = std::env::var_os(branding::CONFIG_DIR_ENV) {
        return PathBuf::from(dir);
    }
    std::env::var_os("HOME")
        .or_else(|| std::env::var_os("USERPROFILE"))
        .map_or_else(
            || PathBuf::from(".").join(branding::DOT_DIR),
            |h| PathBuf::from(h).join(branding::DOT_DIR),
        )
}

/// Resolve the current run's log through the `latest` pointer that
/// `apps/cli/src/logging.rs` writes and `memory::retention` preserves.
fn current_log_path(home: &Path) -> Option<PathBuf> {
    let pointer = home.join("debug").join("latest");
    let raw = std::fs::read_to_string(pointer).ok()?;
    let path = PathBuf::from(raw.trim());
    path.is_file().then_some(path)
}

/// Last [`TAIL_LINES`] lines, or a note saying the log is empty.
fn tail(path: &Path) -> String {
    let Ok(body) = std::fs::read_to_string(path) else {
        return "(the debug log could not be read)".to_string();
    };
    let lines: Vec<&str> = body.lines().collect();
    if lines.is_empty() {
        return "(the debug log is empty — this session may have started without `--debug`)"
            .to_string();
    }
    let start = lines.len().saturating_sub(TAIL_LINES);
    lines[start..].join("\n")
}

/// Build the skill body for a given home — pure, so tests need no `set_var`.
#[must_use]
pub(crate) fn render(home: &Path, args: &str, project_dir: &Path) -> String {
    let mut out = String::from(
        "# Debug Skill\n\nHelp the user debug an issue they're encountering in this current \
         LingXi session.\n",
    );

    match current_log_path(home) {
        Some(path) => {
            out.push_str(&format!(
                "\n## Session Debug Log\n\nThe debug log for the current session is at: `{}`\n\n\
                 ```\n{}\n```\n\nFor additional context, grep for [ERROR] and [WARN] lines across \
                 the full file.\n",
                path.display(),
                tail(&path)
            ));
        }
        None => {
            // ⛔ Never claim logging was just switched on: this port cannot do
            // that mid-session, and a model told the log is live would hunt for
            // entries that will never appear.
            out.push_str(
                "\n## No Debug Log For This Session\n\nThis session was not started with \
                 `--debug`, so nothing was captured. Debug logging cannot be switched on \
                 mid-session here — ask the user to restart with `lingxi --debug`, reproduce the \
                 issue, and run `/debug` again. Until then, diagnose from what the user describes \
                 and from the settings files below.\n",
            );
        }
    }

    out.push_str(&format!(
        "\n## Issue Description\n\n{}\n",
        if args.trim().is_empty() {
            "The user did not describe a specific issue. Read the debug log and summarize any \
             errors, warnings, or notable issues."
        } else {
            args.trim()
        }
    ));

    out.push_str(&format!(
        "\n## Settings\n\nRemember that settings are in:\n* user - {}\n* project - {}\n* local - {}\n",
        lingxi_core::settings::loader::user_settings_path()
            .map_or_else(|| "(unresolved)".to_string(), |p| p.display().to_string()),
        lingxi_core::settings::loader::project_settings_path(project_dir).display(),
        lingxi_core::settings::loader::local_settings_path(project_dir).display(),
    ));

    out.push_str(&format!(
        "\n## Instructions\n\n1. Review the user's issue description\n2. The last {TAIL_LINES} \
         lines show the debug file format. Look for [ERROR] and [WARN] entries, stack traces, and \
         failure patterns across the file\n3. Explain what you found in plain language\n4. Suggest \
         concrete fixes or next steps\n",
    ));
    out
}

/// Dynamic prompt builder for `/debug`.
pub struct DebugPromptFn;

impl BundledPromptFn for DebugPromptFn {
    fn build(&self, args: &str) -> String {
        let cwd = std::env::current_dir().unwrap_or_else(|_| PathBuf::from("."));
        render(&lingxi_home_dir(), args, &cwd)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn temp_home(tag: &str) -> PathBuf {
        let dir =
            std::env::temp_dir().join(format!("lingxi-debugskill-{tag}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(dir.join("debug")).unwrap();
        dir
    }

    fn seed_log(home: &Path, body: &str) -> PathBuf {
        let log = home.join("debug").join("run.log");
        std::fs::write(&log, body).unwrap();
        std::fs::write(
            home.join("debug").join("latest"),
            log.to_string_lossy().as_bytes(),
        )
        .unwrap();
        log
    }

    /// The skill must quote the REAL log, reached through the same `latest`
    /// pointer the CLI writes — not a path it merely guessed.
    #[test]
    fn the_prompt_quotes_the_current_session_log() {
        let home = temp_home("has-log");
        let log = seed_log(&home, "[INFO] boot\n[ERROR] something broke\n");
        let out = render(&home, "it crashed", Path::new("/repo"));
        assert!(
            out.contains(&log.display().to_string()),
            "must name the log path"
        );
        assert!(
            out.contains("[ERROR] something broke"),
            "must inline the tail"
        );
        assert!(
            out.contains("it crashed"),
            "must carry the user's description"
        );
        let _ = std::fs::remove_dir_all(&home);
    }

    /// 🚨 With no log, the prompt must NOT claim logging was just enabled — this
    /// port cannot enable it mid-session, and saying so would send the model
    /// looking for entries that can never appear.
    #[test]
    fn without_a_log_it_says_so_instead_of_claiming_logging_is_now_on() {
        let home = temp_home("no-log");
        let out = render(&home, "", Path::new("/repo"));
        assert!(out.contains("was not started with `--debug`"));
        assert!(out.contains("restart with `lingxi --debug`"));
        assert!(
            !out.to_lowercase().contains("just enabled"),
            "this port cannot enable debug logging mid-session"
        );
        assert!(!out.contains("```"), "there is no log to quote");
        let _ = std::fs::remove_dir_all(&home);
    }

    /// A `latest` that points at a deleted file is the same as having no log —
    /// retention sweeps old runs, so a stale pointer is expected, not exotic.
    #[test]
    fn a_stale_latest_pointer_is_treated_as_no_log() {
        let home = temp_home("stale");
        std::fs::write(
            home.join("debug").join("latest"),
            home.join("debug")
                .join("gone.log")
                .to_string_lossy()
                .as_bytes(),
        )
        .unwrap();
        let out = render(&home, "", Path::new("/repo"));
        assert!(out.contains("was not started with `--debug`"));
        let _ = std::fs::remove_dir_all(&home);
    }

    /// Only the tail is inlined; the prompt tells the model to grep the rest.
    #[test]
    fn only_the_tail_is_inlined() {
        let home = temp_home("tail");
        let body: String = (0..100).map(|i| format!("line {i}\n")).collect();
        seed_log(&home, &body);
        let out = render(&home, "", Path::new("/repo"));
        assert!(out.contains("line 99"), "the newest line must be present");
        assert!(!out.contains("line 5\n"), "old lines must not be inlined");
        assert!(out.contains("grep for [ERROR]"));
        let _ = std::fs::remove_dir_all(&home);
    }

    #[test]
    fn the_body_is_branded_and_drops_the_removed_agent() {
        let home = temp_home("brand");
        let out = render(&home, "", Path::new("/repo"));
        assert!(!out.contains("Claude Code"));
        assert!(
            !out.contains("claude-code-guide"),
            "that agent is a recorded divergence and is not registered here"
        );
        let _ = std::fs::remove_dir_all(&home);
    }
}
