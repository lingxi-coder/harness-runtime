//! `/resume` — list prior resumable sessions (non-interactive).
//!
//! Port of the claude-code `type: 'local-jsx'` resume command
//! (`src/commands/resume/resume.tsx`, aliases `['continue']`, `argumentHint`
//! `'[conversation id or search term]'`). The TS command opens an interactive
//! React picker listing prior conversations discovered from the on-disk JSONL
//! session store, optionally filtered by a search term, and loads the chosen
//! session back into the live app.
//!
//! The interactive picker and the load-selected-session step have no CLI text
//! analogue and are deferred. This port delivers the enumeration half: a
//! newest-first `id` + `label` listing from the additive
//! [`OrchestratorHandle::list_resumable_sessions`], optionally filtered by a
//! substring search term (the TS `[search term]` arg).

use crate::model::{BuiltinCommandHandler, CommandResult};
use crate::parser::ParsedSlashCommand;
use async_trait::async_trait;
use lingxi_core::host::OrchestratorHandle;
use std::fmt::Write as _;
use std::sync::Arc;

/// `/resume` handler — renders the resumable-session listing.
#[derive(Clone)]
pub struct ResumeHandler {
    handle: Arc<dyn OrchestratorHandle>,
}

impl ResumeHandler {
    /// Construct a `ResumeHandler` bound to the given orchestrator handle.
    #[must_use]
    pub fn new(handle: Arc<dyn OrchestratorHandle>) -> Self {
        Self { handle }
    }
}

#[async_trait]
impl BuiltinCommandHandler for ResumeHandler {
    async fn handle(&self, args: &ParsedSlashCommand) -> CommandResult {
        let sessions = self.handle.list_resumable_sessions().await;
        CommandResult::Done {
            display: Some(render_sessions(&sessions, args.raw_args.trim())),
        }
    }
    fn name(&self) -> &str {
        "resume"
    }
    fn description(&self) -> &str {
        "Resume a previous conversation"
    }
}

/// Render the `(id, label)` listing newest-first, one `"id  label"` per line.
///
/// When `search` is non-empty, only entries whose `id` or `label` contains
/// `search` (case-insensitive) are shown — the TS `[search term]` filter. An
/// empty result renders a locked one-line notice (no interactive picker).
#[must_use]
fn render_sessions(sessions: &[(String, String)], search: &str) -> String {
    let needle = search.to_lowercase();
    let matched: Vec<&(String, String)> = sessions
        .iter()
        .filter(|(id, label)| {
            needle.is_empty()
                || id.to_lowercase().contains(&needle)
                || label.to_lowercase().contains(&needle)
        })
        .collect();

    if matched.is_empty() {
        return if search.is_empty() {
            "No resumable sessions found.".to_string()
        } else {
            format!("No resumable sessions matching {search}.")
        };
    }

    let mut out = String::from("Resumable sessions:\n");
    for (id, label) in matched {
        // When the label equals the id (first-prompt extraction deferred),
        // show the id alone to avoid a redundant "id  id" line. `writeln!`
        // into a `String` is infallible, so the result is discarded.
        if label == id {
            let _ = writeln!(out, "  {id}");
        } else {
            let _ = writeln!(out, "  {id}  {label}");
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;
    use orchestrator::test_support::MockOrchestratorHandle;

    fn args(raw: &str) -> ParsedSlashCommand {
        ParsedSlashCommand {
            name: "resume".to_string(),
            raw_args: raw.to_string(),
            positional_args: raw.split_whitespace().map(str::to_string).collect(),
        }
    }

    #[test]
    fn empty_no_arg_renders_none_found() {
        let s = render_sessions(&[], "");
        assert_eq!(s, "No resumable sessions found.");
    }

    #[test]
    fn empty_with_arg_renders_no_match() {
        let s = render_sessions(&[], "abc");
        assert_eq!(s, "No resumable sessions matching abc.");
    }

    #[test]
    fn lists_id_only_when_label_equals_id() {
        let sessions = vec![
            ("sess-aaa".to_string(), "sess-aaa".to_string()),
            ("sess-bbb".to_string(), "sess-bbb".to_string()),
        ];
        let s = render_sessions(&sessions, "");
        assert_eq!(s, "Resumable sessions:\n  sess-aaa\n  sess-bbb\n");
    }

    #[test]
    fn lists_id_and_label_when_distinct() {
        let sessions = vec![("sess-aaa".to_string(), "fix the bug".to_string())];
        let s = render_sessions(&sessions, "");
        assert_eq!(s, "Resumable sessions:\n  sess-aaa  fix the bug\n");
    }

    #[test]
    fn filters_by_search_term() {
        let sessions = vec![
            ("alpha-1".to_string(), "alpha-1".to_string()),
            ("beta-2".to_string(), "beta-2".to_string()),
        ];
        let s = render_sessions(&sessions, "beta");
        assert_eq!(s, "Resumable sessions:\n  beta-2\n");
    }

    #[test]
    fn search_matches_label_case_insensitively() {
        let sessions = vec![("sess-1".to_string(), "Fix The Bug".to_string())];
        let s = render_sessions(&sessions, "bug");
        assert_eq!(s, "Resumable sessions:\n  sess-1  Fix The Bug\n");
    }

    #[tokio::test]
    async fn default_handle_renders_none_found() {
        // The mock inherits the trait default: no on-disk sessions.
        let mock = Arc::new(MockOrchestratorHandle::new());
        let h = ResumeHandler::new(mock);
        if let CommandResult::Done { display: Some(s) } = h.handle(&args("")).await {
            assert_eq!(s, "No resumable sessions found.");
        } else {
            panic!();
        }
    }

    #[test]
    fn name_and_description() {
        let mock = Arc::new(MockOrchestratorHandle::new());
        let h = ResumeHandler::new(mock);
        assert_eq!(h.name(), "resume");
        assert_eq!(h.description(), "Resume a previous conversation");
    }
}
