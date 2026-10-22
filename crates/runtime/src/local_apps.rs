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
use lingxi_core::types::utf16_json::Utf16JsonProjection;
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

struct LocalAppPrompt(Utf16JsonProjection);

impl BundledPromptFn for LocalAppPrompt {
    fn build(&self, args: &Utf16JsonProjection) -> Utf16JsonProjection {
        let args = args.string_units("").expect("prompt arguments are text");
        if args
            .iter()
            .all(|unit| char::from_u32(u32::from(*unit)).is_some_and(char::is_whitespace))
        {
            return self.0.clone();
        }

        let mut units = self.0.string_units("").expect("prompt template is text");
        units.extend("\n\n## User Request\n\n".encode_utf16());
        units.extend(args);
        Utf16JsonProjection::root_string(String::from_utf16_lossy(&units), units)
            .expect("prompt display matches its source units")
    }
}

/// (Re)register `/local-app` when the host offers the `LocalApp` tool.
/// `shell_tool` names the host's shell tool for the optional check step.
pub fn register_skill(
    registry: &mut CommandRegistry,
    available_tool_names: &[String],
    shell_tool: &str,
) {
    register_skill_text(
        registry,
        available_tool_names,
        &local_app::skill::skill_markdown(
            available_tool_names
                .iter()
                .any(|name| name == shell_tool)
                .then_some(shell_tool),
        ),
    );
}

/// Desktop projects use a different workflow and capability contract.
pub fn register_desktop_skill(registry: &mut CommandRegistry, available_tool_names: &[String]) {
    register_skill_text(
        registry,
        available_tool_names,
        include_str!("../assets/desktop-local-app.md"),
    );
}

fn register_skill_text(
    registry: &mut CommandRegistry,
    available_tool_names: &[String],
    markdown: &str,
) {
    registry.unregister_loaded_from(SKILL_LOADED_FROM);
    if !available_tool_names
        .iter()
        .any(|name| name == local_app::TOOL_NAME)
    {
        return;
    }
    let skill_root = PathBuf::from(SKILL_LOADED_FROM).join(local_app::skill::SKILL_NAME);
    let parsed = command_api::parse_skill_command_markdown(
        markdown,
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

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn prompt_appends_exact_argument_and_template_units() {
        let template_units = vec![b't' as u16, 0xd800];
        let template = Utf16JsonProjection::root_string(
            String::from_utf16_lossy(&template_units),
            template_units.clone(),
        )
        .unwrap();
        let prompt = LocalAppPrompt(template);
        let argument_units = vec![b'x' as u16, 0xd83d, 0xde03, 0xdfff];
        let args = Utf16JsonProjection::root_string(
            String::from_utf16_lossy(&argument_units),
            argument_units.clone(),
        )
        .unwrap();

        let expanded = prompt.build(&args);
        let mut expected = template_units;
        expected.extend("\n\n## User Request\n\n".encode_utf16());
        expected.extend(argument_units);
        assert_eq!(expanded.string_units(""), Some(expected));
    }

    #[test]
    fn prompt_omits_whitespace_only_arguments() {
        let template = Utf16JsonProjection::root_string(
            "template".into(),
            "template".encode_utf16().collect(),
        )
        .unwrap();
        let prompt = LocalAppPrompt(template.clone());
        let args =
            Utf16JsonProjection::root_string(" \n\t".into(), " \n\t".encode_utf16().collect())
                .unwrap();

        assert_eq!(
            prompt.build(&args).string_units(""),
            template.string_units("")
        );
    }
}
