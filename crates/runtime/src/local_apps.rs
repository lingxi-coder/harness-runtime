//! Built-in local apps, wired for the desktop and mobile compositions.
//!
//! Hosts opt in with their `local_apps` config flag once they can open apps
//! (a card under the tool call and an Apps page); CLI and headless leave it
//! off, so neither the `LocalApp` tool nor the `local-app` skill exists there.

use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex, OnceLock, PoisonError};

use command_api::{BundledPromptFn, CommandRegistry, CommandSource, SlashCommandKind};
use lingxi_core::host::FileSystem;
use local_app::LocalAppStore;

/// `loaded_from` tag of the registered skill; `/reload-skills` leaves it alone.
pub const SKILL_LOADED_FROM: &str = "local-app";

/// The one library per config home in this process, so the tool and the host
/// service share the store's in-process writer serialization.
pub fn shared_store(fs: Arc<dyn FileSystem>, config_home: &Path) -> Arc<LocalAppStore> {
    static STORES: OnceLock<Mutex<HashMap<PathBuf, Arc<LocalAppStore>>>> = OnceLock::new();
    let mut stores = STORES
        .get_or_init(Mutex::default)
        .lock()
        .unwrap_or_else(PoisonError::into_inner);
    stores
        .entry(config_home.to_path_buf())
        .or_insert_with(|| Arc::new(LocalAppStore::new(fs, config_home.to_path_buf())))
        .clone()
}

/// Register the `LocalApp` tool over `store`.
pub fn register_tool(
    registry: &mut tool_api::ToolRegistry,
    ctx: tool_api::BuiltinToolContext,
    store: Arc<LocalAppStore>,
    visualizations: Option<Arc<visualization::VisualizationStore>>,
) {
    let mut tool = tool_ui::LocalAppTool::new(ctx, store);
    if let Some(visualizations) = visualizations {
        tool = tool.with_visualizations(visualizations);
    }
    registry.register_builtin(Arc::new(tool));
}

struct LocalAppPrompt(String);

impl BundledPromptFn for LocalAppPrompt {
    fn build(&self, args: &str) -> String {
        if args.trim().is_empty() {
            self.0.clone()
        } else {
            format!("{}\n\n## User Request\n\n{args}", self.0)
        }
    }
}

/// (Re)register `/local-app` when the host offers the `LocalApp` tool.
/// `shell_tool` names the host's shell tool for the optional check step.
pub fn register_skill(
    registry: &mut CommandRegistry,
    available_tool_names: &[String],
    shell_tool: &str,
) {
    registry.unregister_loaded_from(SKILL_LOADED_FROM);
    if !available_tool_names
        .iter()
        .any(|name| name == local_app::TOOL_NAME)
    {
        return;
    }
    let shell = available_tool_names
        .iter()
        .any(|name| name == shell_tool)
        .then_some(shell_tool);
    let markdown = local_app::skill::skill_markdown(shell);
    let skill_root = PathBuf::from(SKILL_LOADED_FROM).join(local_app::skill::SKILL_NAME);
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
        unreachable!("the local-app skill is a Markdown prompt")
    };
    command.kind = SlashCommandKind::Bundled {
        frontmatter,
        prompt_fn: Some(Arc::new(LocalAppPrompt(prompt_template))),
    };
    command.loaded_from = Some(SKILL_LOADED_FROM.into());
    command.skill_root = None;
    registry.register_command(command);
}
