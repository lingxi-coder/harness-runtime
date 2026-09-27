//! `/release-notes` — show the LingXi release notes (or a link to them).
//!
//! Ported 1:1 from the claude-code TS local command
//! `src/commands/release-notes/release-notes.ts` (and its changelog util
//! `src/utils/releaseNotes.ts`). The TS `call()` races a 500ms changelog
//! fetch, then formats `getAllReleaseNotes(getStoredChangelog())` into
//! `"Version {v}:\n· {note}\n· {note}"` blocks joined by `"\n\n"`; when no
//! notes are available it falls back to `"See the full changelog at: {URL}"`.
//!
//! In a **non-interactive** session the TS `fetchAndStoreChangelog()`
//! early-returns (`getIsNonInteractiveSession()`), so the network fetch never
//! runs. There is no bundled changelog and — in this Rust workspace — no
//! in-tree changelog source or cached-changelog config field. Consequently the
//! only reachable branch of the non-interactive path is the URL fallback, which
//! this handle-free port emits verbatim as `Done`.

use async_trait::async_trait;
use command_api::model::{BuiltinCommandHandler, CommandResult};
use command_api::parser::ParsedSlashCommand;

/// Canonical changelog URL, verbatim from `releaseNotes.ts` (L28-29).
const CHANGELOG_URL: &str = "https://github.com/anthropics/claude-code/blob/main/CHANGELOG.md";

/// `/release-notes` handler — returns the changelog link as `Done`.
///
/// No orchestrator dependency: this is a static display command. With no
/// in-tree changelog source the faithful non-interactive output is the
/// TS URL-fallback string.
#[derive(Debug, Default)]
pub struct ReleaseNotesHandler;

impl ReleaseNotesHandler {
    /// Construct a new `ReleaseNotesHandler`.
    #[must_use]
    pub fn new() -> Self {
        Self
    }
}

#[async_trait]
impl BuiltinCommandHandler for ReleaseNotesHandler {
    async fn handle(&self, _args: &ParsedSlashCommand) -> CommandResult {
        CommandResult::Done {
            display: Some(format!("See the full changelog at: {CHANGELOG_URL}")),
        }
    }

    fn name(&self) -> &str {
        "release-notes"
    }

    fn description(&self) -> &str {
        // Verbatim TS metadata (release-notes/index.ts) — implemented, so it
        // carries the real description, not the `core_description` fallback.
        "View release notes"
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Port of the TS `formatReleaseNotes` helper
    /// (`release-notes.ts` L9-17): each `(version, notes)` block renders as
    /// `"Version {v}:\n· {note}\n· {note}"`, blocks joined by `"\n\n"`.
    ///
    /// Kept inside the test module (rather than as a `#[allow(dead_code)]`
    /// non-test fn) so the formatting logic is exercised and ready to wire when
    /// a changelog source lands, without tripping `-D warnings` on dead code.
    fn format_release_notes(notes: &[(String, Vec<String>)]) -> String {
        notes
            .iter()
            .map(|(version, bullets)| {
                let header = format!("Version {version}:");
                let body = bullets
                    .iter()
                    .map(|n| format!("· {n}"))
                    .collect::<Vec<_>>()
                    .join("\n");
                format!("{header}\n{body}")
            })
            .collect::<Vec<_>>()
            .join("\n\n")
    }

    fn args() -> ParsedSlashCommand {
        ParsedSlashCommand {
            name: "release-notes".to_string(),
            raw_args: String::new(),
            positional_args: vec![],
        }
    }

    #[tokio::test]
    async fn returns_url_fallback() {
        let h = ReleaseNotesHandler::new();
        match h.handle(&args()).await {
            CommandResult::Done { display: Some(s) } => {
                assert_eq!(
                    s,
                    "See the full changelog at: \
                     https://github.com/anthropics/claude-code/blob/main/CHANGELOG.md"
                );
            }
            other => panic!("expected Done with display, got {other:?}"),
        }
    }

    #[test]
    fn changelog_url_matches_ts_source() {
        assert_eq!(
            CHANGELOG_URL,
            "https://github.com/anthropics/claude-code/blob/main/CHANGELOG.md"
        );
    }

    #[test]
    fn name_and_description() {
        let h = ReleaseNotesHandler::new();
        assert_eq!(h.name(), "release-notes");
        assert_eq!(h.description(), "View release notes");
    }

    #[test]
    fn format_single_version_single_note() {
        let notes = vec![("1.0.0".to_string(), vec!["note".to_string()])];
        assert_eq!(format_release_notes(&notes), "Version 1.0.0:\n· note");
    }

    #[test]
    fn format_multi_version_multi_note() {
        let notes = vec![
            (
                "1.1.0".to_string(),
                vec!["added X".to_string(), "fixed Y".to_string()],
            ),
            ("1.0.0".to_string(), vec!["first release".to_string()]),
        ];
        assert_eq!(
            format_release_notes(&notes),
            "Version 1.1.0:\n· added X\n· fixed Y\n\nVersion 1.0.0:\n· first release"
        );
    }
}
