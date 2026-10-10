//! Inline-visualization wiring shared by the desktop and mobile compositions.
//!
//! Hosts opt in with their `inline_visualization` config flag once their
//! WebView host can render widgets; CLI and headless leave it off, so neither
//! the `Visualization` tool nor the `visualize` skill exists there.

use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex, OnceLock, PoisonError};

use command_api::{BundledPromptFn, CommandRegistry, CommandSource, SlashCommandKind};
use lingxi_core::host::FileSystem;
use visualization::VisualizationStore;

/// `loaded_from` tag of the registered skill; `/reload-skills` leaves it alone.
pub const SKILL_LOADED_FROM: &str = "inline-visualization";

/// The one store per config home in this process, so the tool and the host
/// service share the store's in-process writer serialization.
pub fn shared_store(fs: Arc<dyn FileSystem>, config_home: &Path) -> Arc<VisualizationStore> {
    static STORES: OnceLock<Mutex<HashMap<PathBuf, Arc<VisualizationStore>>>> = OnceLock::new();
    let mut stores = STORES
        .get_or_init(Mutex::default)
        .lock()
        .unwrap_or_else(PoisonError::into_inner);
    stores
        .entry(config_home.to_path_buf())
        .or_insert_with(|| Arc::new(VisualizationStore::new(fs, config_home.to_path_buf())))
        .clone()
}

/// Register the `Visualization` tool over `store`.
pub fn register_tool(
    registry: &mut tool_api::ToolRegistry,
    ctx: tool_api::BuiltinToolContext,
    store: Arc<VisualizationStore>,
) {
    registry.register_builtin(Arc::new(tool_ui::VisualizationTool::new(ctx, store)));
}

/// The prompt text to submit for a `SendPrompt` that carries a
/// `visualization_context`: the revision's confirmed `modelContent` is fenced
/// ahead of the typed text, snapshotted now (at send or enqueue time). An
/// unknown session, id or revision leaves the text unchanged.
pub async fn followup_text(
    fs: Arc<dyn FileSystem>,
    config_home: &Path,
    root_session: &str,
    context: Option<&client::protocol::message::VisualizationRefDto>,
    text: String,
) -> String {
    let Some(context) = context else {
        return text;
    };
    let (Some(id), Some(session)) = (
        visualization::VisualizationId::parse(&context.id),
        lingxi_core::types::SessionId::parse_prefixed(root_session),
    ) else {
        return text;
    };
    let reference = visualization::VisualizationRef {
        id,
        revision: context.revision,
    };
    let store = shared_store(fs, config_home);
    visualization::context::followup_prompt(&store, session.as_uuid(), &reference, &text).await
}

struct VisualizePrompt(String);

impl BundledPromptFn for VisualizePrompt {
    fn build(&self, args: &str) -> String {
        if args.trim().is_empty() {
            self.0.clone()
        } else {
            format!("{}\n\n## User Request\n\n{args}", self.0)
        }
    }
}

/// (Re)register `/visualize` when the host offers the `Visualization` tool.
/// `shell_tool` names the host's shell tool for the optional check step.
pub fn register_skill(
    registry: &mut CommandRegistry,
    available_tool_names: &[String],
    shell_tool: &str,
) {
    registry.unregister_loaded_from(SKILL_LOADED_FROM);
    if !available_tool_names
        .iter()
        .any(|name| name == visualization::TOOL_NAME)
    {
        return;
    }
    let shell = available_tool_names
        .iter()
        .any(|name| name == shell_tool)
        .then_some(shell_tool);
    let markdown = visualization::skill::skill_markdown(shell);
    let skill_root = PathBuf::from(SKILL_LOADED_FROM).join(visualization::skill::SKILL_NAME);
    let parsed = command_api::parse_skill_command_markdown(
        &markdown,
        skill_root.join("SKILL.md"),
        skill_root,
        CommandSource::Bundled,
    );
    let mut command = command_api::build_skill_command(&parsed, CommandSource::Bundled);
    let SlashCommandKind::Markdown {
        frontmatter,
        prompt_template,
        ..
    } = command.kind
    else {
        unreachable!("the visualize skill is a Markdown prompt")
    };
    command.kind = SlashCommandKind::Bundled {
        frontmatter,
        prompt_fn: Some(Arc::new(VisualizePrompt(prompt_template))),
    };
    command.loaded_from = Some(SKILL_LOADED_FROM.into());
    command.skill_root = None;
    registry.register_command(command);
}

/// How long an orphaned visualization directory survives before the sweep
/// removes it.
pub const ORPHAN_RETENTION: std::time::Duration = std::time::Duration::from_secs(30 * 24 * 60 * 60);

/// Transcript trees whose `.jsonl` files keep a root session's
/// visualizations alive: the project catalogs plus the rollout trees.
const TRANSCRIPT_ROOTS: [&str; 3] = ["projects", "sessions", "archived_sessions"];

/// Every UUID named by a `.jsonl` file name below `dir`, at most `depth`
/// levels down. Unreadable directories contribute nothing.
fn transcript_uuids(dir: &Path, depth: usize, out: &mut std::collections::HashSet<uuid::Uuid>) {
    let Ok(entries) = std::fs::read_dir(dir) else {
        return;
    };
    for entry in entries.flatten() {
        let path = entry.path();
        let Ok(kind) = entry.file_type() else {
            continue;
        };
        if kind.is_dir() {
            if depth > 0 {
                transcript_uuids(&path, depth - 1, out);
            }
            continue;
        }
        let Some(stem) = path
            .extension()
            .filter(|ext| *ext == "jsonl")
            .and_then(|_| path.file_stem())
            .and_then(|stem| stem.to_str())
        else {
            continue;
        };
        // `<uuid>.jsonl` in a catalog, `rollout-<ts>-<uuid>.jsonl` in a rollout tree.
        let tail = stem
            .len()
            .checked_sub(36)
            .and_then(|start| stem.get(start..));
        if let Some(id) = tail.and_then(|tail| uuid::Uuid::parse_str(tail).ok()) {
            out.insert(id);
        }
    }
}

/// Root sessions whose transcripts live in one project `catalog` directory.
#[must_use]
pub fn catalog_sessions(catalog: &Path) -> Vec<uuid::Uuid> {
    let mut sessions = std::collections::HashSet::new();
    transcript_uuids(catalog, 0, &mut sessions);
    sessions.into_iter().collect()
}

/// Remove every visualization of `sessions` (a deleted session catalog).
/// Best effort: a directory that refuses to go is a leak the sweep retries.
pub async fn delete_sessions(store: &VisualizationStore, sessions: &[uuid::Uuid]) {
    for &session in sessions {
        if let Err(error) = store.delete_session(session).await {
            tracing::warn!(%session, %error, "visualization: session delete failed");
        }
        match std::fs::remove_dir_all(store.session_dir(session)) {
            Ok(()) => {}
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
            Err(error) => {
                tracing::warn!(%session, %error, "visualization: session directory removal failed")
            }
        }
    }
}

/// Remove visualization directories of root sessions that no transcript
/// names any more and that nothing touched for `retention`. Returns how many
/// directories were removed. Runs on a blocking thread.
pub fn sweep_orphans(
    config_home: &Path,
    retention: std::time::Duration,
    now: std::time::SystemTime,
) -> usize {
    let root = config_home.join(branding::VISUALIZATIONS_DIR);
    let Ok(entries) = std::fs::read_dir(&root) else {
        return 0;
    };
    let mut live = std::collections::HashSet::new();
    for tree in TRANSCRIPT_ROOTS {
        transcript_uuids(&config_home.join(tree), 4, &mut live);
    }
    let mut removed = 0;
    for entry in entries.flatten() {
        let Some(session) = entry
            .file_name()
            .to_str()
            .and_then(|name| uuid::Uuid::parse_str(name).ok())
        else {
            continue;
        };
        if live.contains(&session) {
            continue;
        }
        // The newest of the directory and its index: a write refreshes either.
        let path = entry.path();
        let touched = [path.clone(), path.join("index.json")]
            .iter()
            .filter_map(|p| std::fs::symlink_metadata(p).and_then(|m| m.modified()).ok())
            .max();
        let Some(touched) = touched else {
            continue;
        };
        if now
            .duration_since(touched)
            .is_ok_and(|age| age >= retention)
            && entry.file_type().is_ok_and(|kind| kind.is_dir())
            && std::fs::remove_dir_all(&path).is_ok()
        {
            removed += 1;
        }
    }
    removed
}
