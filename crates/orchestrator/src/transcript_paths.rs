//! Composition-root helpers for deriving session transcript paths from the
//! resolved claude-home + cwd + session id, WITHOUT the composition root
//! (`engine-desktop`) needing a direct dependency on the `session` crate (the
//! `session::jsonl::path` helpers live behind this thin facade so the leaf-firer
//! / subagent-spawner wiring can compute the same paths the orchestrator's
//! [`crate::conversation::ConversationOrchestrator::computed_transcript_path`]
//! produces).
//!
//! Parity: 1:1 with claude-code's `getTranscriptPathForSession(sessionId)`
//! (`utils/sessionStorage.ts:207`, joined by `createBaseHookInput`,
//! `utils/hooks.ts:322`) and `getAgentTranscriptPath(agentId)`
//! (`utils/sessionStorage.ts:247`). Both anchor on
//! `getProjectDir(cwd) = <lingxi_home>/projects/<sanitizePath(cwd)>`.

use std::path::{Path, PathBuf};

/// `<lingxi_home>/projects/<sanitize(cwd)>/<session_uuid>.jsonl` — the session's
/// JSONL transcript, the value claude-code's `createBaseHookInput` stamps on
/// EVERY hook payload's `transcript_path`. `session_id` is the BARE uuid (no
/// `sess:` prefix) so the filename matches the on-disk JSONL the writer/loader
/// use and claude-code's `${sessionId}.jsonl`.
#[must_use]
pub fn main_transcript_path(lingxi_home: &Path, cwd: &str, session_uuid: &str) -> PathBuf {
    session::jsonl::path::session_path(lingxi_home, cwd, session_uuid)
}

/// `<lingxi_home>/projects/<sanitize(cwd)>/<session_uuid>/subagents` — the
/// directory under which a spawned subagent's `agent-<id>.jsonl` transcript
/// lives (claude-code `getAgentTranscriptPath`'s `base` with no per-agent
/// subdir). Threaded into the subagent spawn so `agent_transcript_path` =
/// `<this dir>/agent-<id>.jsonl` (the value the agent-scoped `SubagentStop`
/// carries), replacing the prior `/tmp` placeholder.
#[must_use]
pub fn subagents_dir(lingxi_home: &Path, cwd: &str, session_uuid: &str) -> PathBuf {
    let mut p = lingxi_home.to_path_buf();
    p.push("projects");
    p.push(session::jsonl::path::project_dir_name(cwd));
    p.push(session_uuid);
    p.push("subagents");
    p
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn main_transcript_path_matches_session_path_shape() {
        let home = Path::new("/home/.lingxi");
        let got = main_transcript_path(home, "/Users/me/proj", "abc-123");
        assert_eq!(
            got,
            PathBuf::from("/home/.lingxi/projects/-Users-me-proj/abc-123.jsonl"),
        );
    }

    #[test]
    fn subagents_dir_nests_session_then_subagents() {
        let home = Path::new("/home/.lingxi");
        let got = subagents_dir(home, "/Users/me/proj", "abc-123");
        assert_eq!(
            got,
            PathBuf::from("/home/.lingxi/projects/-Users-me-proj/abc-123/subagents"),
        );
        // The leaf `agent-<id>.jsonl` then sits directly under it — the value
        // `agent_transcript_path` resolves to (claude-code getAgentTranscriptPath).
        assert_eq!(
            got.join("agent-xyz.jsonl"),
            PathBuf::from(
                "/home/.lingxi/projects/-Users-me-proj/abc-123/subagents/agent-xyz.jsonl"
            ),
        );
    }
}
