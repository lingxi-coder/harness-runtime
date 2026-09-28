//! `/files` — list the files currently tracked in the read-file-state cache.
//!
//! 1:1 behavioral port of the claude-code `type: 'local'` command
//! (`src/commands/files/files.ts`). The TS `call()` reads
//! `context.readFileState` cache keys and returns either the literal
//! `"No files in context"` (empty) or `"Files in context:\n{relative-path}"`
//! (one path per line, each rendered relative to the cwd).
//!
//! Backing data comes from the additive
//! [`OrchestratorHandle::files_in_context`] (cache keys) and the cwd from
//! [`OrchestratorHandle::get_status_snapshot`]. Until the orchestrator's
//! tool-execution path populates a read-file-state cache, `files_in_context`
//! returns an empty `Vec`, so this command renders the locked
//! `"No files in context"` branch — exactly matching the TS empty case.

use crate::model::{BuiltinCommandHandler, CommandResult};
use crate::parser::ParsedSlashCommand;
use async_trait::async_trait;
use platform_api::OrchestratorHandle;
use std::path::{Component, Path, PathBuf};
use std::sync::Arc;

/// `/files` handler — renders the read-file-state cache as a path listing.
#[derive(Clone)]
pub struct FilesHandler {
    handle: Arc<dyn OrchestratorHandle>,
}

impl FilesHandler {
    /// Construct a `FilesHandler` bound to the given orchestrator handle.
    #[must_use]
    pub fn new(handle: Arc<dyn OrchestratorHandle>) -> Self {
        Self { handle }
    }
}

#[async_trait]
impl BuiltinCommandHandler for FilesHandler {
    async fn handle(&self, _args: &ParsedSlashCommand) -> CommandResult {
        let files = self.handle.files_in_context().await;
        let snap = self.handle.get_status_snapshot().await;
        CommandResult::Done {
            display: Some(render_files(&files, &snap.cwd)),
        }
    }
    fn name(&self) -> &str {
        "files"
    }
    fn description(&self) -> &str {
        "List all files currently in context"
    }
}

/// Render the file listing relative to `cwd`.
///
/// Empty input → the locked `"No files in context"` literal. Otherwise
/// `"Files in context:\n{rel}\n{rel}…"` with each path relativized against
/// `cwd` (1:1 with the TS `files.map(f => relative(getCwd(), f)).join('\n')`).
#[must_use]
fn render_files(files: &[PathBuf], cwd: &Path) -> String {
    if files.is_empty() {
        return "No files in context".to_string();
    }
    let list = files
        .iter()
        .map(|f| relative(cwd, f).display().to_string())
        .collect::<Vec<_>>()
        .join("\n");
    format!("Files in context:\n{list}")
}

/// Compute `target` relative to `base`, mirroring Node's `path.relative`
/// for the cases `/files` produces: absolute `base` + absolute `target`.
///
/// Walks past the shared prefix, then emits one `..` per remaining `base`
/// component followed by the remaining `target` components. Falls back to
/// `target` unchanged when either path is not absolute (so a relative cache
/// key is shown verbatim, matching the TS string output).
fn relative(base: &Path, target: &Path) -> PathBuf {
    if !base.is_absolute() || !target.is_absolute() {
        return target.to_path_buf();
    }
    let base_parts: Vec<Component<'_>> = base.components().collect();
    let target_parts: Vec<Component<'_>> = target.components().collect();

    let mut common = 0;
    while common < base_parts.len()
        && common < target_parts.len()
        && base_parts[common] == target_parts[common]
    {
        common += 1;
    }

    let mut rel = PathBuf::new();
    for _ in common..base_parts.len() {
        rel.push("..");
    }
    for part in &target_parts[common..] {
        rel.push(part.as_os_str());
    }
    // Identical paths → Node returns "" ; preserve that.
    rel
}

#[cfg(test)]
mod tests {
    use super::*;

    fn args() -> ParsedSlashCommand {
        ParsedSlashCommand {
            name: "files".to_string(),
            raw_args: String::new(),
            positional_args: vec![],
        }
    }

    #[test]
    fn empty_renders_no_files_literal() {
        let s = render_files(&[], Path::new("/repo"));
        assert_eq!(s, "No files in context");
    }

    #[test]
    fn lists_paths_relative_to_cwd() {
        let cwd = PathBuf::from("/repo");
        let files = vec![
            PathBuf::from("/repo/src/main.rs"),
            PathBuf::from("/repo/Cargo.toml"),
        ];
        let s = render_files(&files, &cwd);
        assert_eq!(s, "Files in context:\nsrc/main.rs\nCargo.toml");
    }

    #[test]
    fn relativizes_paths_above_cwd() {
        let cwd = PathBuf::from("/repo/crate");
        let files = vec![PathBuf::from("/repo/shared/util.rs")];
        let s = render_files(&files, &cwd);
        assert_eq!(s, "Files in context:\n../shared/util.rs");
    }

    #[test]
    fn non_absolute_target_shown_verbatim() {
        // A relative cache key is rendered unchanged (matches the TS string).
        let cwd = PathBuf::from("/repo");
        let files = vec![PathBuf::from("src/main.rs")];
        let s = render_files(&files, &cwd);
        assert_eq!(s, "Files in context:\nsrc/main.rs");
    }

    #[tokio::test]
    async fn default_handle_renders_no_files() {
        use orchestrator::test_support::MockOrchestratorHandle;
        // The mock inherits the trait default: an empty file list.
        let mock = Arc::new(MockOrchestratorHandle::new());
        let h = FilesHandler::new(mock);
        if let CommandResult::Done { display: Some(s) } = h.handle(&args()).await {
            assert_eq!(s, "No files in context");
        } else {
            panic!();
        }
    }

    #[tokio::test]
    async fn handle_renders_files_relative_to_cwd() {
        use orchestrator::test_support::MockOrchestratorHandle;
        use platform_api::StatusSnapshot;
        // Pre-seed the read-file-state cache (absolute keys, insertion order)
        // and a matching cwd, then drive the real handler: proves the
        // `files_in_context` override + `render_files` compose 1:1 with the
        // TS `Files in context:\n${relative(getCwd(), f)}` output.
        let mock = MockOrchestratorHandle::new();
        mock.set_files_in_context(vec![
            PathBuf::from("/repo/src/a.rs"),
            PathBuf::from("/repo/Cargo.toml"),
        ]);
        mock.set_status_snapshot(StatusSnapshot {
            cwd: PathBuf::from("/repo"),
            ..StatusSnapshot::default()
        });
        let h = FilesHandler::new(Arc::new(mock));
        if let CommandResult::Done { display: Some(s) } = h.handle(&args()).await {
            assert_eq!(s, "Files in context:\nsrc/a.rs\nCargo.toml");
        } else {
            panic!();
        }
    }

    #[test]
    fn name_and_description() {
        use orchestrator::test_support::MockOrchestratorHandle;
        let mock = Arc::new(MockOrchestratorHandle::new());
        let h = FilesHandler::new(mock);
        assert_eq!(h.name(), "files");
        assert_eq!(h.description(), "List all files currently in context");
    }
}
