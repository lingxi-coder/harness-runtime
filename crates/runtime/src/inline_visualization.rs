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
