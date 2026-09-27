//! `/output-style` — list the available output styles, or switch to one.
//!
//! Reintroduced upstream in 2.1.269 after its 2.1.183 removal, and the shape it
//! came back as matters: the shipping command object is
//! `{type:"local", name:"output-style", supportsNonInteractive:!0,
//!   description:"List output styles or switch to one", argumentHint:"[style]"}`
//! — a plain TEXT command, not the `local-jsx` picker an earlier survey of this
//! port assumed. There IS a `local-jsx` sibling ("Output style moved to
//! /config"), but it is gated on `tengu_maple_sundial`, a rollout flag that
//! defaults FALSE, so it is dormant in the shipping binary. The text command's
//! own gate (`isEnabled: () => Ae() || !x8()`) is therefore always true.
//!
//! Byte-faithful behaviour of upstream's handler:
//!
//! - an argument that names a style (case-insensitively) switches to it;
//! - an argument in the "show me" set (`list`, `show`, `current`, `?`, …) or the
//!   help set (`help`, `-h`, `--help`) renders the LISTING rather than erroring;
//! - any other argument is an error naming the available styles;
//! - switching to the style already in force says so instead of rewriting it.

use async_trait::async_trait;
use command_api::builtin_support::names::core_description;
use command_api::model::{BuiltinCommandHandler, CommandResult};
use command_api::parser::ParsedSlashCommand;
use platform_api::OrchestratorHandle;
use std::sync::Arc;

/// Arguments that ask for the listing instead of naming a style — upstream
/// `u1` (`["list","show","display","current","view","get","check","describe",
/// "print","version","about","status","?"]`).
const LISTING_ARGS: [&str; 13] = [
    "list", "show", "display", "current", "view", "get", "check", "describe", "print", "version",
    "about", "status", "?",
];

/// Arguments that ask for help — upstream `Zw` (`["help","-h","--help"]`).
/// Upstream folds these into the same listing render.
const HELP_ARGS: [&str; 3] = ["help", "-h", "--help"];

/// Where a switch is written back to, if anywhere.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Persistence {
    /// Switch for this SESSION only — the default.
    ///
    /// 🚨 This default is deliberately the inert one. An earlier draft resolved
    /// the settings path from the process cwd unconditionally, which made the
    /// handler's own unit tests overwrite the REPO's
    /// `.lingxi/settings.local.json` — a real 5 KB file — with
    /// `{"outputStyle": …}`. A command that writes a user's settings file must
    /// be told to, never work it out on its own.
    SessionOnly,
    /// Write `outputStyle` into the project-local settings file under the
    /// engine's cwd, resolved per call.
    LocalSettingsUnderCwd,
}

/// `/output-style` handler.
#[derive(Clone)]
pub struct OutputStyleHandler {
    handle: Arc<dyn OrchestratorHandle>,
    persistence: Persistence,
}

impl OutputStyleHandler {
    /// Construct an `OutputStyleHandler` bound to the given orchestrator handle.
    /// Switches apply to this session only; see [`Self::persisting_to_local_settings`].
    #[must_use]
    pub fn new(handle: Arc<dyn OrchestratorHandle>) -> Self {
        Self {
            handle,
            persistence: Persistence::SessionOnly,
        }
    }

    /// Also write the choice into the project-local settings file, so it
    /// survives the session — upstream `Kt("localSettings", {outputStyle: t})`.
    ///
    /// The "project" layer in this port follows the ENGINE's cwd rather than a
    /// separately-tracked project, which is the convention every other settings
    /// writer here uses; the path is resolved per call so a `/cd` is honoured
    /// instead of a boot-time directory being pinned.
    #[must_use]
    pub fn persisting_to_local_settings(mut self) -> Self {
        self.persistence = Persistence::LocalSettingsUnderCwd;
        self
    }
}

/// Render the listing — upstream's no-match arm, verbatim in shape:
/// `Output style: {current}\n\nAvailable styles:\n{lines}\n\nUsage: /output-style <style>`,
/// each line `- {name}[ (current)][: {description}]`.
fn render_listing(listing: &platform_api::OutputStyleListing) -> String {
    let lines: Vec<String> = listing
        .styles
        .iter()
        .map(|(name, description)| {
            let marker = if *name == listing.current {
                " (current)"
            } else {
                ""
            };
            match description {
                Some(description) => format!("- {name}{marker}: {description}"),
                None => format!("- {name}{marker}"),
            }
        })
        .collect();
    format!(
        "Output style: {}\n\nAvailable styles:\n{}\n\nUsage: /output-style <style>",
        listing.current,
        lines.join("\n")
    )
}

/// Write the choice into the project-local settings file.
///
/// Returns the error TEXT on failure. A failure is reported, never swallowed:
/// silently keeping only the in-session switch would tell the user their choice
/// was saved when it was not.
fn persist_choice(name: &str) -> Option<String> {
    let cwd = std::env::current_dir().ok()?;
    let path = lingxi_core::settings::loader::local_settings_path(&cwd);
    match lingxi_core::settings::loader::set_settings_key(
        &path,
        "outputStyle",
        serde_json::Value::String(name.to_string()),
    ) {
        Ok(()) => None,
        Err(error) => Some(format!("Could not save output style: {error}")),
    }
}

#[async_trait]
impl BuiltinCommandHandler for OutputStyleHandler {
    async fn handle(&self, args: &ParsedSlashCommand) -> CommandResult {
        let Some(listing) = self.handle.output_styles().await else {
            return CommandResult::Done {
                display: Some("Output styles are not available in this session.".to_string()),
            };
        };

        let raw = args.raw_args.trim();
        let lowered = raw.to_ascii_lowercase();
        let chosen = if raw.is_empty() {
            None
        } else {
            listing
                .styles
                .iter()
                .find(|(name, _)| name.eq_ignore_ascii_case(raw))
                .map(|(name, _)| name.clone())
        };

        let Some(chosen) = chosen else {
            // An argument that is neither a style nor one of the listing/help
            // words is a mistake worth naming, not a silent listing.
            if !raw.is_empty()
                && !LISTING_ARGS.contains(&lowered.as_str())
                && !HELP_ARGS.contains(&lowered.as_str())
            {
                let names: Vec<&str> = listing
                    .styles
                    .iter()
                    .map(|(name, _)| name.as_str())
                    .collect();
                return CommandResult::Done {
                    display: Some(format!(
                        "Unknown output style \"{raw}\". Available styles: {}",
                        names.join(", ")
                    )),
                };
            }
            return CommandResult::Done {
                display: Some(render_listing(&listing)),
            };
        };

        if chosen == listing.current {
            return CommandResult::Done {
                display: Some(format!("Output style is already {chosen}")),
            };
        }

        if let Err(error) = self.handle.set_output_style(&chosen).await {
            return CommandResult::Done {
                display: Some(format!("Could not switch output style: {error}")),
            };
        }
        if self.persistence == Persistence::LocalSettingsUnderCwd {
            if let Some(failure) = persist_choice(&chosen) {
                return CommandResult::Done {
                    display: Some(failure),
                };
            }
        }
        CommandResult::Done {
            display: Some(format!("Output style set to {chosen}")),
        }
    }

    fn name(&self) -> &str {
        "output-style"
    }

    fn description(&self) -> &str {
        core_description("output-style")
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use command_api::model::CommandResult;
    use orchestrator::test_support::MockOrchestratorHandle;

    fn handler(current: &str) -> (OutputStyleHandler, Arc<MockOrchestratorHandle>) {
        let mock = Arc::new(MockOrchestratorHandle::new());
        mock.set_output_style_listing(platform_api::OutputStyleListing {
            current: current.to_string(),
            styles: vec![
                ("default".to_string(), None),
                (
                    "Explanatory".to_string(),
                    Some("Claude explains its implementation choices".to_string()),
                ),
                ("Learning".to_string(), None),
            ],
        });
        (
            OutputStyleHandler::new(mock.clone() as Arc<dyn OrchestratorHandle>),
            mock,
        )
    }

    fn args(raw: &str) -> ParsedSlashCommand {
        ParsedSlashCommand {
            name: "output-style".to_string(),
            raw_args: raw.to_string(),
            positional_args: raw.split_whitespace().map(str::to_string).collect(),
        }
    }

    async fn display(h: &OutputStyleHandler, raw: &str) -> String {
        match h.handle(&args(raw)).await {
            CommandResult::Done { display: Some(s) } => s,
            other => panic!("expected a display, got {other:?}"),
        }
    }

    #[tokio::test]
    async fn no_argument_lists_the_styles_and_marks_the_current_one() {
        let (h, _) = handler("Explanatory");
        assert_eq!(
            display(&h, "").await,
            "Output style: Explanatory\n\nAvailable styles:\n\
             - default\n\
             - Explanatory (current): Claude explains its implementation choices\n\
             - Learning\n\n\
             Usage: /output-style <style>"
        );
    }

    #[tokio::test]
    async fn the_listing_words_render_the_listing_rather_than_an_error() {
        // Upstream folds `u1` (list/show/current/…/?) and `Zw` (help/-h/--help)
        // into the SAME no-match arm. Treating them as style names would give
        // `Unknown output style "help"`, which reads like a broken command.
        let (h, _) = handler("default");
        for word in [
            "list", "show", "current", "?", "help", "-h", "--help", "STATUS",
        ] {
            assert!(
                display(&h, word).await.starts_with("Output style: default"),
                "{word:?} must render the listing"
            );
        }
    }

    #[tokio::test]
    async fn an_unknown_name_names_the_available_styles() {
        let (h, _) = handler("default");
        assert_eq!(
            display(&h, "Verbose").await,
            "Unknown output style \"Verbose\". Available styles: default, Explanatory, Learning"
        );
    }

    #[tokio::test]
    async fn switching_reports_the_canonical_spelling_and_calls_the_handle() {
        let (h, mock) = handler("default");
        // Matching is case-insensitive; the CANONICAL name is what is stored
        // and echoed, because the style resolvers look names up case-sensitively
        // and would select nothing from what the user typed.
        assert!(display(&h, "explanatory")
            .await
            .starts_with("Output style set to Explanatory"));
        assert_eq!(
            mock.output_style_switches(),
            vec!["Explanatory".to_string()]
        );
    }

    #[tokio::test]
    async fn switching_to_the_current_style_is_a_no_op_that_says_so() {
        let (h, mock) = handler("Learning");
        assert_eq!(
            display(&h, "learning").await,
            "Output style is already Learning"
        );
        assert!(
            mock.output_style_switches().is_empty(),
            "an already-current style must not be re-applied"
        );
    }

    /// 🚨 Pins the SAFE default. The first draft of this handler resolved the
    /// settings path from the process cwd unconditionally, and the switch test
    /// above overwrote the repository's own `.lingxi/settings.local.json`.
    /// A bare handler must touch nothing on disk.
    #[tokio::test]
    async fn a_bare_handler_writes_no_settings_file() {
        let dir = std::env::temp_dir().join(format!(
            "lingxi-output-style-{}-{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .map(|d| d.as_nanos())
                .unwrap_or_default()
        ));
        std::fs::create_dir_all(&dir).expect("scratch dir");
        let previous = std::env::current_dir().expect("cwd");
        std::env::set_current_dir(&dir).expect("enter scratch dir");

        let (h, _) = handler("default");
        let result = display(&h, "Explanatory").await;

        let wrote = dir.join(branding::DOT_DIR).join("settings.local.json");
        let existed = wrote.exists();
        std::env::set_current_dir(previous).expect("restore cwd");
        let _ = std::fs::remove_dir_all(&dir);

        assert_eq!(result, "Output style set to Explanatory");
        assert!(
            !existed,
            "a handler built with `new()` must not write a settings file"
        );
    }

    #[tokio::test]
    async fn an_engine_with_no_styles_says_so_instead_of_listing_nothing() {
        let mock = Arc::new(MockOrchestratorHandle::new());
        let h = OutputStyleHandler::new(mock as Arc<dyn OrchestratorHandle>);
        assert_eq!(
            display(&h, "").await,
            "Output styles are not available in this session."
        );
    }
}
