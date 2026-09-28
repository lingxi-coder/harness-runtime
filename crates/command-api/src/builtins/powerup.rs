//! /powerup — a small, local lesson view for discovering LingXi features.
//!
//! The upstream command is an interactive picker backed by the persisted
//! powerupsUnlocked user setting. The registry dispatcher has no renderer
//! handle, so this implementation presents the same list/detail flow as
//! plain text and accepts "done <lesson-id>" as its completion action. The
//! setting is kept in the normal user settings.json, preserving unrelated
//! keys and making completion survive a new process.

use crate::builtin_support::names::core_description;
use crate::model::{BuiltinCommandHandler, CommandResult};
use crate::parser::ParsedSlashCommand;
use async_trait::async_trait;
use serde_json::{Map, Value};
use std::collections::BTreeSet;
use std::path::{Path, PathBuf};

/// User-setting key used by Claude Code's storageV5 power-up view.
pub const POWERUPS_UNLOCKED_KEY: &str = "powerupsUnlocked";

/// One lesson shown by /powerup.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct PowerupLesson {
    /// Stable persisted identifier.
    pub id: &'static str,
    /// Short title shown in the list.
    pub title: &'static str,
    /// One-line list subtitle.
    pub tagline: &'static str,
    /// Plain-text detail body.
    pub body: &'static str,
}

/// The ten lessons shipped by the 2.1.252 oracle. The cross-device lesson is
/// adapted to LingXi's local background/task workflow because the oracle's
/// cloud/mobile controls are not part of this provider-neutral runtime.
pub const POWERUP_LESSONS: &[PowerupLesson] = &[
    PowerupLesson {
        id: "at-mentions",
        title: "Talk to your codebase",
        tagline: "@ files, line refs",
        body: "Type @ anywhere in your prompt to fuzzy-find and attach a file. The file is read before answering, so you do not need to paste code. Reference specific lines such as src/app.ts:42 to jump straight there. You can also use @folder/ to attach a directory tree.",
    },
    PowerupLesson {
        id: "modes",
        title: "Steer with modes",
        tagline: "shift+tab, plan, auto",
        body: "Press shift+tab to cycle permission modes. Default asks before every edit; accept edits lets the agent edit freely while still asking for commands; plan researches and proposes without touching files; auto lets the agent decide what is safe. Use plan for big refactors you want to review first and auto for long unattended tasks. Run /permissions to pre-allow specific commands.",
    },
    PowerupLesson {
        id: "undo",
        title: "Undo anything",
        tagline: "/rewind, Esc-Esc",
        body: "The session checkpoints files before edits. Press Esc Esc to open /rewind and roll back to a prior point in the code, conversation, or both. Your git history stays clean. /clear wipes the conversation but keeps files, while /branch forks the conversation to try another approach.",
    },
    PowerupLesson {
        id: "background",
        title: "Run in the background",
        tagline: "tasks, /tasks",
        body: "Long builds and test suites do not have to block you. Add & to a shell command to run it in the background while you keep chatting. Run /tasks to inspect work in flight and read task output. Subagents also run as tasks, so they share one queue.",
    },
    PowerupLesson {
        id: "memory",
        title: "Teach LingXi your rules",
        tagline: "LINGXI.md, /memory",
        body: "Drop a LINGXI.md file in your repository and LingXi reads it at the start of every session. Put conventions there: test commands, style rules, and do-not-touch directories. Run /init to generate a starter file or /memory to edit it. Rules can live at repo, home, and per-directory levels.",
    },
    PowerupLesson {
        id: "mcp",
        title: "Extend with tools",
        tagline: "MCP, /mcp",
        body: "MCP servers give the agent new tools for services such as chat, databases, and browsers. Run /mcp to browse and connect servers. Once connected, the tools appear automatically. From a shell, use lingxi-cli mcp add <name> -- <command> to wire one up without leaving the terminal.",
    },
    PowerupLesson {
        id: "automate",
        title: "Automate your workflow",
        tagline: "skills, hooks",
        body: "Save a prompt to .lingxi/skills/deploy/SKILL.md and it becomes /deploy. Run /skills to see what you have. Hooks run your scripts around events such as a tool call, response, or session start; use /hooks to inspect them. Bundled skills can give the agent repeatable project workflows.",
    },
    PowerupLesson {
        id: "subagents",
        title: "Multiply yourself",
        tagline: "subagents",
        body: "The agent can spawn copies of itself to work in parallel. Ask it to use subagents to search several directories and watch the fan-out. Subagents run in isolated context; use /agents to inspect them and /tasks to follow their progress.",
    },
    PowerupLesson {
        id: "cross-device",
        title: "Code from anywhere",
        tagline: "background tasks, worktrees",
        body: "Keep long work moving while you change contexts: put a long build or test in the background, then use /tasks to read its progress. Use /branch or a git worktree to continue a separate line of work without disturbing this session. The local task and transcript files keep the work recoverable on this machine.",
    },
    PowerupLesson {
        id: "model-dial",
        title: "Dial the model",
        tagline: "/model, /effort",
        body: "Run /model to switch among the models configured for your provider. Use /effort to control how much reasoning a request gets: choose a higher level for tricky bugs and a lower level for quick edits. /fast opts into faster output when your provider supports it.",
    },
];

const LIST_INTRO: &str = "Each power-up teaches one thing LingXi can do that most people miss. Open one, read it, try it, and mark it done.";
const USAGE: &str = "Usage: /powerup [lesson-id | done <lesson-id>]";

/// /powerup handler.
#[derive(Debug, Clone, Default)]
pub struct PowerupHandler {
    /// Explicit settings path is used by tests and embedders. None resolves
    /// the normal config home at invocation time.
    settings_path: Option<PathBuf>,
}

impl PowerupHandler {
    /// Construct a handler using the process's configured user settings path.
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }

    /// Construct a handler pinned to a settings file.
    #[must_use]
    pub fn with_settings_path(path: PathBuf) -> Self {
        Self {
            settings_path: Some(path),
        }
    }

    /// Read the persisted completion set from a settings file.
    pub fn load_unlocked(path: &Path) -> Result<BTreeSet<String>, String> {
        read_unlocked(path)
    }

    /// Persist a completion set into a settings file without dropping any
    /// unrelated user settings.
    pub fn persist_unlocked(path: &Path, unlocked: &BTreeSet<String>) -> Result<(), String> {
        write_unlocked(path, unlocked)
    }

    fn resolved_settings_path(&self) -> Option<PathBuf> {
        self.settings_path.clone().or_else(user_settings_path)
    }
}

#[async_trait]
impl BuiltinCommandHandler for PowerupHandler {
    async fn handle(&self, args: &ParsedSlashCommand) -> CommandResult {
        let path = self.resolved_settings_path();
        let mut unlocked = match path.as_deref() {
            Some(path) => match read_unlocked(path) {
                Ok(unlocked) => unlocked,
                Err(error) => {
                    return CommandResult::Done {
                        display: Some(format!("/powerup: {error}")),
                    };
                }
            },
            None => BTreeSet::new(),
        };

        let raw = args.raw_args.trim();
        if raw.is_empty() || raw.eq_ignore_ascii_case("list") {
            return CommandResult::Done {
                display: Some(render_index(&unlocked)),
            };
        }

        let mut parts = raw.split_whitespace();
        let first = parts.next().unwrap_or_default();
        if first.eq_ignore_ascii_case("done") || first.eq_ignore_ascii_case("complete") {
            let Some(id) = parts.next() else {
                return CommandResult::Done {
                    display: Some(USAGE.to_string()),
                };
            };
            if parts.next().is_some() || !is_known_lesson(id) {
                return CommandResult::Done {
                    display: Some(USAGE.to_string()),
                };
            }
            let was_new = unlocked.insert(id.to_string());
            if was_new {
                if let Some(path) = path.as_deref() {
                    if let Err(error) = write_unlocked(path, &unlocked) {
                        return CommandResult::Done {
                            display: Some(format!("/powerup: {error}")),
                        };
                    }
                }
            }
            let title = lesson(id).map_or(id, |lesson| lesson.title);
            let prefix = if was_new {
                format!("Marked {title} done.")
            } else {
                format!("{title} is already done.")
            };
            return CommandResult::Done {
                display: Some(format!("{prefix}\n\n{}", render_index(&unlocked))),
            };
        }

        if parts.next().is_some() {
            return CommandResult::Done {
                display: Some(USAGE.to_string()),
            };
        }
        let Some(lesson) = lesson(first) else {
            return CommandResult::Done {
                display: Some(USAGE.to_string()),
            };
        };
        CommandResult::Done {
            display: Some(render_detail(lesson, unlocked.contains(lesson.id))),
        }
    }

    fn name(&self) -> &str {
        "powerup"
    }

    fn description(&self) -> &str {
        core_description("powerup")
    }
}

fn lesson(id: &str) -> Option<&'static PowerupLesson> {
    POWERUP_LESSONS.iter().find(|lesson| lesson.id == id)
}

fn is_known_lesson(id: &str) -> bool {
    lesson(id).is_some()
}

fn render_index(unlocked: &BTreeSet<String>) -> String {
    let total = POWERUP_LESSONS.len();
    let count = POWERUP_LESSONS
        .iter()
        .filter(|lesson| unlocked.contains(lesson.id))
        .count();
    let heading = if count == total {
        "All powered up"
    } else {
        "Power-ups"
    };
    let mut output = format!("{heading}\n{count}/{total} unlocked\n\n");
    output.push_str(if count == total {
        "Now go build something."
    } else {
        LIST_INTRO
    });
    output.push_str("\n\n");
    for lesson in POWERUP_LESSONS {
        let marker = if unlocked.contains(lesson.id) {
            "[x]"
        } else {
            "[ ]"
        };
        output.push_str(&format!(
            "{marker} {} — {} ({})\n",
            lesson.title, lesson.tagline, lesson.id
        ));
    }
    output.push_str(
        "\nOpen a lesson with /powerup <lesson-id>; mark it with /powerup done <lesson-id>.",
    );
    output
}

fn render_detail(lesson: &PowerupLesson, unlocked: bool) -> String {
    let marker = if unlocked { "[x]" } else { "[ ]" };
    let action = if unlocked {
        "This lesson is complete.\nUse /powerup to return to the lesson list.".to_string()
    } else {
        format!(
            "Use /powerup done {} to mark this lesson complete.",
            lesson.id
        )
    };
    format!(
        "{marker} {}\n{}\n\n{}\n\n{}",
        lesson.title, lesson.tagline, lesson.body, action
    )
}

fn user_settings_path() -> Option<PathBuf> {
    if let Some(dir) = std::env::var_os(branding::CONFIG_DIR_ENV) {
        return Some(PathBuf::from(dir).join("settings.json"));
    }
    std::env::var_os("HOME").map(|home| {
        PathBuf::from(home)
            .join(branding::DOT_DIR)
            .join("settings.json")
    })
}

fn read_unlocked(path: &Path) -> Result<BTreeSet<String>, String> {
    let content = match std::fs::read_to_string(path) {
        Ok(content) if content.trim().is_empty() => return Ok(BTreeSet::new()),
        Ok(content) => content,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(BTreeSet::new()),
        Err(error) => return Err(format!("failed to read {}: {error}", path.display())),
    };
    let value: Value = serde_json::from_str(&content)
        .map_err(|error| format!("invalid JSON in {}: {error}", path.display()))?;
    let Some(values) = value.get(POWERUPS_UNLOCKED_KEY).and_then(Value::as_array) else {
        return Ok(BTreeSet::new());
    };
    Ok(values
        .iter()
        .filter_map(Value::as_str)
        .filter(|id| is_known_lesson(id))
        .map(str::to_string)
        .collect())
}

fn write_unlocked(path: &Path, unlocked: &BTreeSet<String>) -> Result<(), String> {
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent)
            .map_err(|error| format!("failed to create {}: {error}", parent.display()))?;
    }
    let mut object: Map<String, Value> = match std::fs::read_to_string(path) {
        Ok(content) if content.trim().is_empty() => Map::new(),
        Ok(content) => serde_json::from_str::<Value>(&content)
            .map_err(|error| format!("invalid JSON in {}: {error}", path.display()))?
            .as_object()
            .cloned()
            .ok_or_else(|| format!("settings root in {} is not an object", path.display()))?,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => Map::new(),
        Err(error) => return Err(format!("failed to read {}: {error}", path.display())),
    };
    object.insert(
        POWERUPS_UNLOCKED_KEY.to_string(),
        Value::Array(
            unlocked
                .iter()
                .map(|id| Value::String(id.clone()))
                .collect(),
        ),
    );
    let serialized = serde_json::to_string_pretty(&Value::Object(object))
        .map_err(|error| format!("failed to serialize {}: {error}", path.display()))?;
    std::fs::write(path, format!("{serialized}\n"))
        .map_err(|error| format!("failed to write {}: {error}", path.display()))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::model::CommandResult;

    fn temp_root(label: &str) -> PathBuf {
        let nanos = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_nanos();
        let path = std::env::temp_dir().join(format!(
            "lingxi-powerup-{label}-{}-{nanos}",
            std::process::id()
        ));
        std::fs::create_dir_all(&path).unwrap();
        path
    }

    fn args(raw_args: &str) -> ParsedSlashCommand {
        ParsedSlashCommand {
            name: "powerup".to_string(),
            raw_args: raw_args.to_string(),
            positional_args: raw_args.split_whitespace().map(str::to_string).collect(),
        }
    }

    fn display(result: CommandResult) -> String {
        match result {
            CommandResult::Done {
                display: Some(display),
            } => display,
            other => panic!("expected display result, got {other:?}"),
        }
    }

    #[tokio::test]
    async fn list_is_a_lesson_view_with_progress() {
        let root = temp_root("list");
        let handler = PowerupHandler::with_settings_path(root.join("settings.json"));
        let output = display(handler.handle(&args("")).await);
        assert!(output.starts_with("Power-ups\n0/10 unlocked"));
        assert!(output.contains("Talk to your codebase"));
        assert!(output.contains("Multiply yourself"));
    }

    #[tokio::test]
    async fn completion_persists_and_is_loaded_by_a_new_handler() {
        let root = temp_root("complete");
        let path = root.join("settings.json");
        let handler = PowerupHandler::with_settings_path(path.clone());
        let output = display(handler.handle(&args("done undo")).await);
        assert!(output.starts_with("Marked Undo anything done."));
        assert_eq!(
            serde_json::from_str::<Value>(&std::fs::read_to_string(&path).unwrap()).unwrap()
                [POWERUPS_UNLOCKED_KEY],
            serde_json::json!(["undo"])
        );

        let reloaded = PowerupHandler::with_settings_path(path);
        let output = display(reloaded.handle(&args("")).await);
        assert!(output.starts_with("Power-ups\n1/10 unlocked"));
        assert!(output.contains("[x] Undo anything"));
    }

    #[test]
    fn ships_ten_unique_provider_neutral_lessons() {
        assert_eq!(POWERUP_LESSONS.len(), 10);
        let ids: BTreeSet<_> = POWERUP_LESSONS.iter().map(|lesson| lesson.id).collect();
        assert_eq!(ids.len(), POWERUP_LESSONS.len());
        assert!(POWERUP_LESSONS
            .iter()
            .all(|lesson| !lesson.body.contains("Claude Code")));
        assert!(POWERUP_LESSONS
            .iter()
            .any(|lesson| lesson.id == "cross-device"));
        assert!(POWERUP_LESSONS
            .iter()
            .any(|lesson| lesson.id == "model-dial"));
    }

    #[tokio::test]
    async fn malformed_settings_are_not_overwritten() {
        let root = temp_root("malformed");
        let path = root.join("settings.json");
        std::fs::write(&path, "{not json").unwrap();
        let handler = PowerupHandler::with_settings_path(path.clone());
        let output = display(handler.handle(&args("done undo")).await);
        assert!(output.contains("invalid JSON"));
        assert_eq!(std::fs::read_to_string(path).unwrap(), "{not json");
    }

    #[test]
    fn metadata_matches_oracle_command() {
        let handler = PowerupHandler::new();
        assert_eq!(handler.name(), "powerup");
        assert_eq!(
            handler.description(),
            "Discover LingXi features through quick interactive lessons"
        );
    }
}
