//! Prompt expansion for markdown-defined slash commands.
//!
//! Faithful port of the `getPromptForCommand` closure built in
//! `claude-code/src/skills/loadSkillsDir.ts:344-399` (the regular-command path,
//! i.e. `loadedFrom !== 'mcp'` and no `skillRoot`/`${LINGXI_SKILL_DIR}` — that
//! substitution applies only to MCP/skill commands, not the `.lingxi/commands`
//! files this loader produces). The order of operations is:
//!
//! 1. [`crate::substitute_arguments_faithful`] over the markdown body with the
//!    raw argument string, `append_if_no_placeholder = true`, and the command's
//!    declared `argument_names`.
//! 2. Replace every literal `${LINGXI_SESSION_ID}` with the session id (TS
//!    `finalContent.replace(/\$\{CLAUDE_SESSION_ID\}/g, getSessionId())`).
//! 3. [`crate::execute_shell_commands_in_prompt`] to run any embedded inline
//!    bang-backtick commands or fenced bang blocks, routed through the
//!    frontmatter `shell`.
//!
//! `${LINGXI_SKILL_DIR}` is intentionally NOT substituted here: regular commands
//! have no skill root (TS only does it `if (baseDir)`, which is `undefined` for
//! non-`SKILL.md` command files).

use crate::model::{SlashCommand, SlashCommandKind};
use crate::parser::ParsedSlashCommand;
use crate::shell_expansion::{
    execute_shell_commands_in_prompt, ShellExpansionCtx, ShellExpansionError,
};

/// Context for [`expand_markdown_command`]. Carries the session id (for the
/// `${LINGXI_SESSION_ID}` token) and the injected shell-expansion dependencies.
pub struct ExpandCtx<'a> {
    /// Current session id substituted for `${LINGXI_SESSION_ID}`.
    pub session_id: &'a str,
    /// Injected runner + permission gate for embedded shell commands.
    pub shell: &'a ShellExpansionCtx,
}

/// Why expanding a markdown command failed.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum ExpandError {
    /// The supplied command is not a markdown (or plugin-markdown) command and
    /// therefore has no body to expand.
    #[error("command '{0}' is not a markdown command")]
    NotMarkdown(String),
    /// A frontmatter argument name formed an invalid regular expression during
    /// argument substitution. Mirrors the `SyntaxError` TS throws from
    /// `new RegExp(...)`, aborting expansion with a user-facing error.
    #[error(transparent)]
    Substitution(#[from] crate::argument_substitution::SubstitutionError),
    /// An embedded shell command was denied or failed during expansion.
    #[error(transparent)]
    Shell(#[from] ShellExpansionError),
}

/// The literal token replaced with the session id. Mirrors the TS regex
/// `/\$\{CLAUDE_SESSION_ID\}/g` (a literal substring, no metacharacters).
const SESSION_ID_TOKEN: &str = "${LINGXI_SESSION_ID}";

/// Expand a markdown slash command into the final model prompt.
///
/// Faithful to the regular-command `getPromptForCommand`: argument substitution,
/// then `${LINGXI_SESSION_ID}` replacement, then embedded shell-command
/// execution. Returns [`ExpandError::NotMarkdown`] for non-markdown commands.
///
/// # Errors
///
/// Returns [`ExpandError::NotMarkdown`] if `cmd` is not a
/// [`SlashCommandKind::Markdown`] / [`SlashCommandKind::Plugin`] command, or
/// [`ExpandError::Shell`] if an embedded shell command is denied or fails.
pub async fn expand_markdown_command(
    cmd: &SlashCommand,
    args: &ParsedSlashCommand,
    ctx: &ExpandCtx<'_>,
) -> Result<String, ExpandError> {
    let (SlashCommandKind::Markdown {
        frontmatter,
        prompt_template,
        ..
    }
    | SlashCommandKind::Plugin {
        frontmatter,
        prompt_template,
        ..
    }) = &cmd.kind
    else {
        return Err(ExpandError::NotMarkdown(cmd.name.clone()));
    };

    // (1) Argument substitution over the body. TS passes the raw arguments
    // string (`args` from the dispatch line), `appendIfNoPlaceholder = true`,
    // and the command's declared argument names.
    let mut content = crate::substitute_arguments_faithful(
        prompt_template,
        Some(&args.raw_args),
        true,
        &frontmatter.argument_names,
    )?;

    // (2) ${LINGXI_SESSION_ID} -> session id (global literal replace).
    if content.contains(SESSION_ID_TOKEN) {
        content = content.replace(SESSION_ID_TOKEN, ctx.session_id);
    }

    // (3) Embedded shell-command execution, routed through the frontmatter shell.
    content = execute_shell_commands_in_prompt(
        &content,
        ctx.shell,
        &format!("/{}", cmd.name),
        frontmatter.shell,
    )
    .await?;

    Ok(content)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::model::{CommandFrontmatter, CommandSource, FrontmatterShell};
    use crate::parser::parse_slash_command;
    use crate::shell_expansion::{
        ShellOut, ShellPermissionDecision, ShellPermissionGate, ShellRunError, ShellRunner,
    };
    use std::path::PathBuf;
    use std::sync::{Arc, Mutex};

    struct EchoRunner {
        calls: Mutex<Vec<String>>,
    }
    #[async_trait::async_trait]
    impl ShellRunner for EchoRunner {
        async fn run(
            &self,
            command: &str,
            _shell: Option<FrontmatterShell>,
        ) -> Result<ShellOut, ShellRunError> {
            self.calls.lock().unwrap().push(command.to_string());
            Ok(ShellOut {
                stdout: format!("OUT[{command}]"),
                stderr: String::new(),
                interrupted: false,
            })
        }
    }

    struct AllowAll;
    impl ShellPermissionGate for AllowAll {
        fn check(&self, _c: &str, _s: Option<FrontmatterShell>) -> ShellPermissionDecision {
            ShellPermissionDecision::Allow
        }
    }

    fn markdown_cmd(name: &str, body: &str, argument_names: Vec<String>) -> SlashCommand {
        SlashCommand {
            name: name.to_string(),
            description: String::new(),
            source: CommandSource::Settings(lingxi_core::types::SettingsScope::Project),
            kind: SlashCommandKind::Markdown {
                file_path: PathBuf::from(format!("/x/{name}.md")),
                frontmatter: CommandFrontmatter {
                    argument_names,
                    ..CommandFrontmatter::default()
                },
                prompt_template: body.to_string(),
            },
            ..SlashCommand::default()
        }
    }

    fn make_ctx<'a>(session_id: &'a str, shell: &'a ShellExpansionCtx) -> ExpandCtx<'a> {
        ExpandCtx { session_id, shell }
    }

    fn shell_ctx(runner: Arc<dyn ShellRunner>) -> ShellExpansionCtx {
        ShellExpansionCtx {
            runner,
            permission_gate: Arc::new(AllowAll),
        }
    }

    #[tokio::test]
    async fn end_to_end_hello_world_via_arguments() {
        // Spec fixture intent: body referencing the first arg + dispatch
        // "/foo world" -> "Hello world". `$ARGUMENTS` is the full raw arg string
        // ("world"). NOTE: TS `$1` is index 1 (the SECOND token); the spec's
        // literal "Hello $1" is the 1-based reading, but the faithful Batch-1
        // semantics make `$0` the first token (see `end_to_end_hello_world_via_index`).
        let cmd = markdown_cmd("foo", "Hello $ARGUMENTS", vec![]);
        let parsed = parse_slash_command("/foo world").unwrap();
        let runner = Arc::new(EchoRunner {
            calls: Mutex::new(Vec::new()),
        });
        let sc = shell_ctx(runner);
        let ctx = make_ctx("sess-123", &sc);
        let out = expand_markdown_command(&cmd, &parsed, &ctx).await.unwrap();
        assert_eq!(out, "Hello world");
    }

    #[tokio::test]
    async fn end_to_end_hello_world_via_index() {
        // Faithful index semantics: `$0` is the first positional token. body
        // "Hello $0" + "/foo world" -> "Hello world".
        let cmd = markdown_cmd("foo", "Hello $0", vec![]);
        let parsed = parse_slash_command("/foo world").unwrap();
        let runner = Arc::new(EchoRunner {
            calls: Mutex::new(Vec::new()),
        });
        let sc = shell_ctx(runner);
        let ctx = make_ctx("sess-123", &sc);
        let out = expand_markdown_command(&cmd, &parsed, &ctx).await.unwrap();
        assert_eq!(out, "Hello world");
    }

    #[tokio::test]
    async fn end_to_end_dollar1_is_second_token() {
        // Documenting the faithful `$1 == index 1` behaviour: with two args, $1
        // selects the second. body "Hello $1" + "/foo first world" -> "Hello world".
        let cmd = markdown_cmd("foo", "Hello $1", vec![]);
        let parsed = parse_slash_command("/foo first world").unwrap();
        let runner = Arc::new(EchoRunner {
            calls: Mutex::new(Vec::new()),
        });
        let sc = shell_ctx(runner);
        let ctx = make_ctx("sess-123", &sc);
        let out = expand_markdown_command(&cmd, &parsed, &ctx).await.unwrap();
        assert_eq!(out, "Hello world");
    }

    #[tokio::test]
    async fn end_to_end_through_loader_build() {
        // Full pipeline: build the command from a loaded markdown file, then expand.
        use crate::markdown_loader::{build_markdown_command, MarkdownCommandFile};
        let file = MarkdownCommandFile {
            file_path: PathBuf::from("/x/.lingxi/commands/foo.md"),
            base_dir: PathBuf::from("/x/.lingxi/commands"),
            frontmatter: CommandFrontmatter::default(),
            content: "Hello $0".to_string(),
            source: CommandSource::Settings(lingxi_core::types::SettingsScope::Project),
        };
        let cmd = build_markdown_command(
            &file,
            CommandSource::Settings(lingxi_core::types::SettingsScope::Project),
        );
        assert_eq!(cmd.name, "foo");
        let parsed = parse_slash_command("/foo world").unwrap();
        let runner = Arc::new(EchoRunner {
            calls: Mutex::new(Vec::new()),
        });
        let sc = shell_ctx(runner);
        let ctx = make_ctx("s", &sc);
        let out = expand_markdown_command(&cmd, &parsed, &ctx).await.unwrap();
        assert_eq!(out, "Hello world");
    }

    #[tokio::test]
    async fn end_to_end_named_first_arg() {
        // body "Hello $name" + frontmatter arguments: [name] + args "world".
        let cmd = markdown_cmd("foo", "Hello $name", vec!["name".to_string()]);
        let parsed = parse_slash_command("/foo world").unwrap();
        let runner = Arc::new(EchoRunner {
            calls: Mutex::new(Vec::new()),
        });
        let sc = shell_ctx(runner);
        let ctx = make_ctx("sess-1", &sc);
        let out = expand_markdown_command(&cmd, &parsed, &ctx).await.unwrap();
        assert_eq!(out, "Hello world");
    }

    #[tokio::test]
    async fn session_id_substituted() {
        let cmd = markdown_cmd("foo", "session=${LINGXI_SESSION_ID}", vec![]);
        let parsed = parse_slash_command("/foo").unwrap();
        let runner = Arc::new(EchoRunner {
            calls: Mutex::new(Vec::new()),
        });
        let sc = shell_ctx(runner);
        let ctx = make_ctx("the-session", &sc);
        let out = expand_markdown_command(&cmd, &parsed, &ctx).await.unwrap();
        assert_eq!(out, "session=the-session");
    }

    #[tokio::test]
    async fn shell_commands_executed_after_substitution() {
        let cmd = markdown_cmd("foo", "before !`echo $ARGUMENTS` after", vec![]);
        let parsed = parse_slash_command("/foo hi").unwrap();
        let runner = Arc::new(EchoRunner {
            calls: Mutex::new(Vec::new()),
        });
        let runner_dyn: Arc<dyn ShellRunner> = runner.clone();
        let sc = shell_ctx(runner_dyn);
        let ctx = make_ctx("s", &sc);
        let out = expand_markdown_command(&cmd, &parsed, &ctx).await.unwrap();
        // $ARGUMENTS -> "hi" first, THEN the shell command runs on the substituted
        // text, so the runner sees `echo hi`.
        assert_eq!(out, "before OUT[echo hi] after");
        assert_eq!(*runner.calls.lock().unwrap(), vec!["echo hi".to_string()]);
    }

    #[tokio::test]
    async fn ordering_session_id_before_shell() {
        // ${LINGXI_SESSION_ID} is replaced before shell execution, so the shell
        // command body can reference it.
        let cmd = markdown_cmd("foo", "!`echo ${LINGXI_SESSION_ID}`", vec![]);
        let parsed = parse_slash_command("/foo").unwrap();
        let runner = Arc::new(EchoRunner {
            calls: Mutex::new(Vec::new()),
        });
        let runner_dyn: Arc<dyn ShellRunner> = runner.clone();
        let sc = shell_ctx(runner_dyn);
        let ctx = make_ctx("XYZ", &sc);
        let out = expand_markdown_command(&cmd, &parsed, &ctx).await.unwrap();
        assert_eq!(out, "OUT[echo XYZ]");
        assert_eq!(*runner.calls.lock().unwrap(), vec!["echo XYZ".to_string()]);
    }

    #[tokio::test]
    async fn invalid_argument_name_surfaces_substitution_error() {
        // ARGS.2: a frontmatter argument name that forms an invalid regex
        // (`a[b` -> unterminated character class) aborts expansion with an
        // error, mirroring the TS `new RegExp(...)` SyntaxError throw.
        let cmd = markdown_cmd("foo", "Hello $a[b", vec!["a[b".to_string()]);
        let parsed = parse_slash_command("/foo world").unwrap();
        let runner = Arc::new(EchoRunner {
            calls: Mutex::new(Vec::new()),
        });
        let sc = shell_ctx(runner);
        let ctx = make_ctx("s", &sc);
        let err = expand_markdown_command(&cmd, &parsed, &ctx)
            .await
            .unwrap_err();
        assert!(matches!(err, ExpandError::Substitution(_)), "got {err:?}");
    }

    #[tokio::test]
    async fn non_markdown_command_errors() {
        let cmd = SlashCommand {
            name: "help".to_string(),
            description: String::new(),
            source: CommandSource::Builtin,
            kind: SlashCommandKind::Builtin {
                handler_id: "help".to_string(),
            },
            ..SlashCommand::default()
        };
        let parsed = parse_slash_command("/help").unwrap();
        let runner = Arc::new(EchoRunner {
            calls: Mutex::new(Vec::new()),
        });
        let sc = shell_ctx(runner);
        let ctx = make_ctx("s", &sc);
        let err = expand_markdown_command(&cmd, &parsed, &ctx)
            .await
            .unwrap_err();
        assert_eq!(err, ExpandError::NotMarkdown("help".to_string()));
    }
}
