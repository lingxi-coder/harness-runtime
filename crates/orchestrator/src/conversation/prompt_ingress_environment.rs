//! Native model-facing environment and model identity prompt-ingress rows.

use super::*;
use crate::request_context::{rendered_system_message, PromptIngressAttachment};
use lingxi_core::types::utf16_json::{JsonValueView, Utf16JsonProjection as Source};
use std::path::Path;

const WORKTREE_NOTICE: &str = "This is a git worktree \u{2014} an isolated copy of the repository. Run all commands from this directory. Do NOT `cd` to the original repository root.";
const GIT_STASH_NOTICE: &str = "The git stash stack is shared with the main checkout and all other worktrees, and other LingXi sessions may push or pop it concurrently. Never use bare `git stash` / `git stash pop` \u{2014} you could pop another session's changes. Prefer a temporary WIP commit to set work aside; if you must stash, use `git stash push -u -m \"<unique-tag>\"`, immediately capture your entry's SHA via `git stash list --format='%H %gs'`, restore with `git stash apply <sha>` (not pop), and afterwards drop the entry, re-finding its current `stash@{n}` by tag first.";
const NO_LONGER_WORKTREE: &str = "The primary working directory is no longer a git worktree.";
const NO_LONGER_SCRATCHPAD: &str =
    "The scratchpad directory announced earlier is no longer available; use it no further.";
const GITHUB_CLI_NO_LONGER_ACTIVE: &str =
    "The built-in `gh api` announced earlier is not active in this run; check `command -v gh`.";

#[derive(Clone, Debug, PartialEq, Eq)]
struct EnvironmentSnapshot {
    working_directory: Option<String>,
    is_worktree: bool,
    is_git_repo: bool,
    additional_working_directories: Vec<String>,
    platform: String,
    shell: Option<String>,
    os_version: Option<String>,
    scratchpad_directory: Option<String>,
    github_cli: Option<String>,
}

#[derive(Default)]
struct PriorEnvironment {
    latest: Option<EnvironmentSnapshot>,
    newest_readable: Option<EnvironmentSnapshot>,
    has_whole_block: bool,
    standing: Option<EnvironmentMention>,
    #[allow(dead_code)] // Used by Native resume reconciliation, outside the live ingress caller.
    mentioned: Option<EnvironmentMention>,
    #[allow(dead_code)] // Used by Native resume reconciliation, outside the live ingress caller.
    told_of_builtin_gh: bool,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
struct EnvironmentMention {
    with_scratchpad: bool,
}

#[derive(Clone, Debug, PartialEq, Eq)]
struct ModelIdentity {
    model_id: String,
    marketing_name: Option<String>,
    knowledge_cutoff: Option<String>,
}

#[derive(Clone, Debug, PartialEq, Eq)]
enum EnvironmentChange {
    WorkingDirectory {
        from: Option<String>,
    },
    IsWorktree,
    IsGitRepo,
    AdditionalWorkingDirectories {
        added: Vec<String>,
        removed: Vec<String>,
    },
    ScratchpadDirectory,
    GithubCli,
}

impl ConversationOrchestrator {
    /// Return zero, one, or two ordered Native environment rows. A full snapshot
    /// is first; when recovery facts require it, a separate change row follows.
    /// A normal delta is a single row. This follows 2.1.295 `_gn` (binary byte
    /// 198066434), `Ast`/`Mst` (local source offsets 118609/119583), and the
    /// `xst` renderer (local offset 118465) in `src_194447854.js`.
    pub(crate) async fn environment_prompt_ingress_attachments(
        &self,
    ) -> Vec<PromptIngressAttachment> {
        if !self.prompt_ingress_environment_enabled() {
            return Vec::new();
        }

        let (cwd, trusted_dirs) = self.session_cwd.snapshot();
        let probe_cwd = self
            .prompt_probe_cwd_resolver
            .as_ref()
            .map(|resolve| resolve(&cwd))
            .unwrap_or_else(|| cwd.clone());
        let (git_status, _) = self.cached_git_status(&probe_cwd).await;
        let cwd_string = physical_path_string(&cwd);
        let additional_working_directories = trusted_dirs
            .into_iter()
            .filter(|path| path != &cwd)
            .map(|path| path.display().to_string())
            .collect();
        let current = EnvironmentSnapshot {
            working_directory: Some(cwd_string),
            is_worktree: probe_cwd.join(".git").is_file(),
            is_git_repo: git_status.is_some(),
            additional_working_directories,
            platform: super::node_platform_name(std::env::consts::OS).to_owned(),
            shell: Some(environment_shell_description(
                cfg!(windows) && self.tools.all_names().iter().any(|name| name == "Bash"),
            )),
            os_version: Some(crate::prompt::env_meta::os_version_string()),
            // This host currently has no session scratchpad allocator or
            // GitHub-proxy stand-in producer; keep these optional Native facts
            // absent until the host can supply authoritative values.
            scratchpad_directory: None,
            github_cli: None,
        };
        let prior = self.prior_environment().await;
        environment_ingress_attachment(current, prior).unwrap_or_default()
    }

    /// Return the model identity row when the live request identity differs
    /// from the last model attachment in visible history order. This follows
    /// 2.1.295 `kgn` (binary byte 198068058) and identity comparator `rit`
    /// (source offset 123753; binary byte 194571607) in `src_194447854.js`.
    pub(crate) async fn model_prompt_ingress_attachment(&self) -> Option<PromptIngressAttachment> {
        if !self.prompt_ingress_environment_enabled() {
            return None;
        }

        let (model_id, profile, history) = {
            let session = self.session.lock().await;
            (
                session.model.clone(),
                session.model_profile.clone(),
                session.model_context_history(),
            )
        };
        let (model_id, profile) = self
            .effective_main_loop_selection_for(&model_id, profile.as_deref(), &history, true)
            .await;
        self.model_prompt_ingress_attachment_for(&model_id, profile.as_deref())
            .await
    }

    /// Render the identity for a caller-supplied effective request model. The
    /// request-context staging path can use the same route selection as the
    /// later model preparation instead of deriving a second model alias here.
    pub(crate) async fn model_prompt_ingress_attachment_for(
        &self,
        model_id: &str,
        profile: Option<&str>,
    ) -> Option<PromptIngressAttachment> {
        if !self.prompt_ingress_environment_enabled() {
            return None;
        }

        let identity = model_identity_for_route(self.api.as_ref(), model_id, profile);
        let previous = self.prior_model_identity().await;
        model_ingress_attachment(identity, previous)
    }

    fn prompt_ingress_environment_enabled(&self) -> bool {
        !self.config.bare
            && !lingxi_core::host::env::is_env_truthy(
                std::env::var(branding::SIMPLE_ENV).ok().as_deref(),
            )
    }

    async fn prior_environment(&self) -> PriorEnvironment {
        let session = self.session.lock().await;
        let history = session.model_context_history();
        let attachments = self
            .transcript
            .model_reminder_attachments
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);

        let mut prior = PriorEnvironment::default();
        for message in &history {
            let Some(payload) = attachments.get(&message.id()) else {
                continue;
            };
            if payload.string_at("/type") != Some("environment") {
                continue;
            }
            record_environment_row(&mut prior, message, payload);
        }
        prior
    }

    async fn prior_model_identity(&self) -> Option<ModelIdentity> {
        let session = self.session.lock().await;
        let history = session.model_context_history();
        let attachments = self
            .transcript
            .model_reminder_attachments
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);

        let mut previous = None;
        for message in &history {
            let Some(payload) = attachments.get(&message.id()) else {
                continue;
            };
            if payload.string_at("/type") == Some("model") {
                // Native findLast selects the latest model row first. An
                // unreadable latest identity therefore causes a resend.
                previous = model_identity_from_payload(payload);
            }
        }
        previous
    }
}

fn physical_path_string(path: &Path) -> String {
    path.canonicalize()
        .unwrap_or_else(|_| path.to_path_buf())
        .display()
        .to_string()
}

fn environment_shell_description(bash_tool_available: bool) -> String {
    if cfg!(windows) {
        if bash_tool_available {
            "PowerShell (primary); Bash tool also available for POSIX scripts — each takes its own syntax.".into()
        } else {
            "PowerShell".into()
        }
    } else {
        crate::prompt::env_meta::detect_shell()
    }
}

fn live_model_listing(
    api: &dyn OrchestratorApiClient,
    model_id: &str,
    profile: Option<&str>,
) -> Option<lingxi_core::host::orchestrator::ModelListing> {
    let matches = api
        .list_model_listings()
        .into_iter()
        .filter(|listing| listing.request_model.eq_ignore_ascii_case(model_id))
        .filter(|listing| {
            profile.is_none_or(|profile| listing.provider_id.eq_ignore_ascii_case(profile))
        })
        .collect::<Vec<_>>();
    (matches.len() == 1).then(|| {
        matches
            .into_iter()
            .next()
            .expect("one matching model listing")
    })
}

fn model_identity_for_route(
    api: &dyn OrchestratorApiClient,
    model_id: &str,
    profile: Option<&str>,
) -> ModelIdentity {
    let foundry = api.protocol_for_model(model_id, profile)
        == Some(llm_runtime::ProtocolFamily::FoundryClaude);
    let live_listing = live_model_listing(api, model_id, profile);
    let marketing_name = if foundry {
        None
    } else {
        crate::prompt::env_meta::marketing_name_for_model(model_id)
            .map(str::to_owned)
            .or_else(|| {
                live_listing
                    .as_ref()
                    .map(|listing| listing.display_model.clone())
            })
    };
    let knowledge_cutoff = crate::prompt::env_meta::knowledge_cutoff_for_model(model_id)
        .map(str::to_owned)
        .or_else(|| live_listing.and_then(|listing| listing.metadata.knowledge_cutoff));
    ModelIdentity {
        model_id: model_id.to_owned(),
        marketing_name,
        knowledge_cutoff,
    }
}

fn environment_snapshot_from_payload(payload: &Source) -> Option<EnvironmentSnapshot> {
    let snapshot = payload.get_property("snapshot")?;
    let string = |key: &str| snapshot.get_property(key).and_then(exact_string);
    let directories = snapshot.get_property("additionalWorkingDirectories")?;
    let directories = (0..directories.array_len()?)
        .map(|index| exact_string(directories.get_index(index)?))
        .collect::<Option<Vec<_>>>()?;
    Some(EnvironmentSnapshot {
        working_directory: string("workingDirectory"),
        is_worktree: snapshot.get_property("isWorktree")?.as_bool()?,
        is_git_repo: snapshot.get_property("isGitRepo")?.as_bool()?,
        additional_working_directories: directories,
        platform: string("platform")?,
        shell: string("shell"),
        os_version: string("osVersion"),
        scratchpad_directory: string("scratchpadDirectory"),
        github_cli: string("githubCli"),
    })
}

fn environment_change_fields(payload: &Source) -> Vec<String> {
    let Some(changes) = payload.get_property("changes") else {
        return Vec::new();
    };
    let Some(count) = changes.array_len() else {
        return Vec::new();
    };
    (0..count)
        .filter_map(|index| exact_string(changes.get_index(index)?.get_property("field")?))
        .collect()
}

fn exact_string(value: JsonValueView<'_>) -> Option<String> {
    String::from_utf16(value.code_units()?).ok()
}

fn record_environment_row(
    prior: &mut PriorEnvironment,
    message: &ConversationMessage,
    payload: &Source,
) {
    let snapshot = environment_snapshot_from_payload(payload);
    let change_fields = environment_change_fields(payload);
    prior.latest = snapshot.clone();
    if let Some(snapshot) = snapshot {
        if payload.get_property("changes").is_none() {
            prior.has_whole_block = true;
        }
        prior.told_of_builtin_gh |=
            snapshot.github_cli.is_some() || change_fields.iter().any(|field| field == "githubCli");
        if let Some(mention) = environment_mention(&snapshot) {
            prior.standing = Some(mention);
            prior.mentioned = Some(mention);
        } else if environment_withdrew_directory(&snapshot, &change_fields) {
            prior.standing = None;
            prior.mentioned = Some(EnvironmentMention {
                with_scratchpad: change_fields
                    .iter()
                    .any(|field| field == "scratchpadDirectory"),
            });
        }
        prior.newest_readable = Some(snapshot);
    } else if message_shows_environment(message) {
        // Native treats a rendered but schema-unreadable environment row as
        // standing context, conservatively assuming it included a scratchpad.
        let mention = EnvironmentMention {
            with_scratchpad: true,
        };
        prior.standing = Some(mention);
        prior.mentioned = Some(mention);
    }
}

fn message_shows_environment(message: &ConversationMessage) -> bool {
    match message {
        ConversationMessage::User { content, .. } => content.iter().any(|block| {
            matches!(block, lingxi_core::types::ContentBlock::Text { text, .. } if text.contains("# Environment"))
        }),
        _ => false,
    }
}

fn environment_mention(snapshot: &EnvironmentSnapshot) -> Option<EnvironmentMention> {
    snapshot.working_directory.as_ref()?;
    Some(EnvironmentMention {
        with_scratchpad: snapshot.scratchpad_directory.is_some(),
    })
}

fn environment_withdrew_directory(
    snapshot: &EnvironmentSnapshot,
    change_fields: &[String],
) -> bool {
    snapshot.working_directory.is_none()
        && change_fields
            .iter()
            .any(|field| field == "workingDirectory")
}

fn environment_ingress_attachment(
    current: EnvironmentSnapshot,
    prior: PriorEnvironment,
) -> Option<Vec<PromptIngressAttachment>> {
    let directory_appeared_after_withdrawal = current.working_directory.is_some()
        && prior
            .newest_readable
            .as_ref()
            .is_some_and(|snapshot| snapshot.working_directory.is_none());
    let full_snapshot = match prior.latest.as_ref() {
        Some(previous)
            if prior.has_whole_block
                && same_environment_identity(previous, &current)
                && !directory_appeared_after_withdrawal =>
        {
            None
        }
        _ => Some(current.clone()),
    };

    let current_has_directory = current.working_directory.is_some();
    let use_standing_context =
        !current_has_directory || (current.shell.is_none() && current.os_version.is_none());
    let standing = use_standing_context.then_some(prior.standing).flatten();
    let restoration = standing
        .filter(|mention| mention.with_scratchpad && current.scratchpad_directory.is_none())
        .map(|_| EnvironmentChange::ScratchpadDirectory);
    let github_removed = prior
        .newest_readable
        .as_ref()
        .is_some_and(|snapshot| snapshot.github_cli.is_some())
        && current.github_cli.is_none();
    let mut recovery_changes = Vec::new();
    if standing.is_some() && !current_has_directory {
        recovery_changes.push(EnvironmentChange::WorkingDirectory { from: None });
    }
    if let Some(change) = restoration {
        recovery_changes.push(change);
    }
    if github_removed {
        recovery_changes.push(EnvironmentChange::GithubCli);
    }

    if let Some(snapshot) = full_snapshot {
        let mut rows = vec![environment_attachment(
            &snapshot,
            None,
            render_environment_snapshot(&snapshot),
        )?];
        if !recovery_changes.is_empty() {
            let text = render_environment_changes(&current, &recovery_changes);
            rows.push(environment_attachment(
                &current,
                Some(&recovery_changes),
                text,
            )?);
        }
        return Some(rows);
    }

    let previous = prior.latest.as_ref()?;
    let mut changes = environment_changes(previous, &current);
    for recovery_change in recovery_changes {
        let field = recovery_change.field();
        if !changes.iter().any(|change| change.field() == field) {
            changes.push(recovery_change);
        }
    }
    if changes.is_empty() {
        return Some(Vec::new());
    }
    let text = render_environment_changes(&current, &changes);
    Some(vec![environment_attachment(
        &current,
        Some(&changes),
        text,
    )?])
}

fn environment_attachment(
    snapshot: &EnvironmentSnapshot,
    changes: Option<&[EnvironmentChange]>,
    text: String,
) -> Option<PromptIngressAttachment> {
    let mut fields = vec![
        ("type".into(), Source::from("environment")),
        ("snapshot".into(), environment_snapshot_projection(snapshot)),
    ];
    if let Some(changes) = changes {
        fields.push((
            "changes".into(),
            Source::array(changes.iter().map(EnvironmentChange::projection).collect()).ok()?,
        ));
    }
    prompt_ingress_attachment(Source::object(fields).ok()?, text)
}

fn same_environment_identity(left: &EnvironmentSnapshot, right: &EnvironmentSnapshot) -> bool {
    left.platform == right.platform
        && left.shell == right.shell
        && left.os_version == right.os_version
}

fn environment_changes(
    previous: &EnvironmentSnapshot,
    current: &EnvironmentSnapshot,
) -> Vec<EnvironmentChange> {
    let mut changes = Vec::new();
    if previous.working_directory != current.working_directory {
        changes.push(EnvironmentChange::WorkingDirectory {
            from: previous.working_directory.clone(),
        });
    }
    if previous.is_worktree != current.is_worktree {
        changes.push(EnvironmentChange::IsWorktree);
    }
    if previous.is_git_repo != current.is_git_repo {
        changes.push(EnvironmentChange::IsGitRepo);
    }
    let added = current
        .additional_working_directories
        .iter()
        .filter(|path| !previous.additional_working_directories.contains(path))
        .cloned()
        .collect::<Vec<_>>();
    let removed = previous
        .additional_working_directories
        .iter()
        .filter(|path| !current.additional_working_directories.contains(path))
        .cloned()
        .collect::<Vec<_>>();
    if !added.is_empty() || !removed.is_empty() {
        changes.push(EnvironmentChange::AdditionalWorkingDirectories { added, removed });
    }
    if previous.scratchpad_directory != current.scratchpad_directory {
        changes.push(EnvironmentChange::ScratchpadDirectory);
    }
    if previous.github_cli != current.github_cli {
        changes.push(EnvironmentChange::GithubCli);
    }
    changes
}

impl EnvironmentChange {
    fn field(&self) -> &'static str {
        match self {
            Self::WorkingDirectory { .. } => "workingDirectory",
            Self::IsWorktree => "isWorktree",
            Self::IsGitRepo => "isGitRepo",
            Self::AdditionalWorkingDirectories { .. } => "additionalWorkingDirectories",
            Self::ScratchpadDirectory => "scratchpadDirectory",
            Self::GithubCli => "githubCli",
        }
    }

    fn projection(&self) -> Source {
        match self {
            Self::WorkingDirectory { from } => {
                let mut fields = vec![("field".into(), Source::from("workingDirectory"))];
                if let Some(from) = from {
                    fields.push(("from".into(), Source::from(from.clone())));
                }
                Source::object(fields).expect("valid environment change")
            }
            Self::IsWorktree => Source::object(vec![("field".into(), Source::from("isWorktree"))])
                .expect("valid environment change"),
            Self::IsGitRepo => Source::object(vec![("field".into(), Source::from("isGitRepo"))])
                .expect("valid environment change"),
            Self::AdditionalWorkingDirectories { added, removed } => Source::object(vec![
                ("field".into(), Source::from("additionalWorkingDirectories")),
                ("added".into(), string_array(added)),
                ("removed".into(), string_array(removed)),
            ])
            .expect("valid environment change"),
            Self::ScratchpadDirectory => {
                Source::object(vec![("field".into(), Source::from("scratchpadDirectory"))])
                    .expect("valid environment change")
            }
            Self::GithubCli => Source::object(vec![("field".into(), Source::from("githubCli"))])
                .expect("valid environment change"),
        }
    }
}

fn environment_snapshot_projection(snapshot: &EnvironmentSnapshot) -> Source {
    let mut fields = Vec::new();
    if let Some(cwd) = &snapshot.working_directory {
        fields.push(("workingDirectory".into(), Source::from(cwd.clone())));
    }
    fields.extend([
        ("isWorktree".into(), bool_value(snapshot.is_worktree)),
        ("isGitRepo".into(), bool_value(snapshot.is_git_repo)),
        (
            "additionalWorkingDirectories".into(),
            string_array(&snapshot.additional_working_directories),
        ),
        ("platform".into(), Source::from(snapshot.platform.clone())),
    ]);
    if let Some(shell) = &snapshot.shell {
        fields.push(("shell".into(), Source::from(shell.clone())));
    }
    if let Some(os_version) = &snapshot.os_version {
        fields.push(("osVersion".into(), Source::from(os_version.clone())));
    }
    if let Some(scratchpad_directory) = &snapshot.scratchpad_directory {
        fields.push((
            "scratchpadDirectory".into(),
            Source::from(scratchpad_directory.clone()),
        ));
    }
    if let Some(github_cli) = &snapshot.github_cli {
        fields.push(("githubCli".into(), Source::from(github_cli.clone())));
    }
    Source::object(fields).expect("valid Native environment snapshot")
}

fn render_environment_snapshot(snapshot: &EnvironmentSnapshot) -> String {
    let mut lines = Vec::new();
    if let Some(cwd) = &snapshot.working_directory {
        lines.push(format!(" - Primary working directory: {cwd}"));
        if snapshot.is_worktree {
            lines.push(format!(" - {WORKTREE_NOTICE}"));
            lines.push(format!(" - {GIT_STASH_NOTICE}"));
        }
        lines.push(format!(" - Is a git repository: {}", snapshot.is_git_repo));
    }
    if !snapshot.additional_working_directories.is_empty() {
        lines.push(" - Additional working directories:".to_owned());
        lines.extend(
            snapshot
                .additional_working_directories
                .iter()
                .map(|path| format!("  - {path}")),
        );
    }
    lines.push(format!(" - Platform: {}", snapshot.platform));
    if let Some(shell) = &snapshot.shell {
        lines.push(format!(" - Shell: {shell}"));
    }
    if let Some(os_version) = &snapshot.os_version {
        lines.push(format!(" - OS Version: {os_version}"));
    }
    if let Some(directory) = &snapshot.scratchpad_directory {
        lines.push(render_scratchpad_notice(directory));
    }
    if let Some(github_cli) = &snapshot.github_cli {
        if let Some(notice) = render_github_cli_notice(github_cli) {
            lines.push(notice);
        }
    }
    format!(
        "# Environment\nYou have been invoked in the following environment: \n{}",
        lines.join("\n")
    )
}

fn render_environment_changes(
    current: &EnvironmentSnapshot,
    changes: &[EnvironmentChange],
) -> String {
    let mut lines = Vec::new();
    for change in changes {
        match change {
            EnvironmentChange::WorkingDirectory { from } => {
                if let Some(cwd) = &current.working_directory {
                    let previous = from
                        .as_ref()
                        .map(|path| format!(" (was {path})"))
                        .unwrap_or_default();
                    lines.push(format!(" - Primary working directory: {}{previous}", cwd));
                } else {
                    lines.push(
                        "The primary working directory announced earlier no longer applies."
                            .to_owned(),
                    );
                }
            }
            EnvironmentChange::IsWorktree => {
                if current.working_directory.is_some() {
                    if current.is_worktree {
                        lines.push(format!(" - {WORKTREE_NOTICE}"));
                        lines.push(format!(" - {GIT_STASH_NOTICE}"));
                    } else {
                        lines.push(format!(" - {NO_LONGER_WORKTREE}"));
                    }
                }
            }
            EnvironmentChange::IsGitRepo => {
                if current.working_directory.is_some() {
                    lines.push(format!(
                        " - Is a git repository: {} (was {})",
                        current.is_git_repo, !current.is_git_repo
                    ));
                }
            }
            EnvironmentChange::AdditionalWorkingDirectories { added, removed } => {
                if !added.is_empty() {
                    lines.push(" - Additional working directories added:".to_owned());
                    lines.extend(added.iter().map(|path| format!("  - {path}")));
                }
                if !removed.is_empty() {
                    lines.push(" - Additional working directories removed:".to_owned());
                    lines.extend(removed.iter().map(|path| format!("  - {path}")));
                }
            }
            EnvironmentChange::ScratchpadDirectory => {
                if let Some(directory) = &current.scratchpad_directory {
                    lines.push(render_scratchpad_notice(directory));
                } else {
                    lines.push(NO_LONGER_SCRATCHPAD.to_owned());
                }
            }
            EnvironmentChange::GithubCli => {
                if let Some(github_cli) = &current.github_cli {
                    if let Some(notice) = render_github_cli_notice(github_cli) {
                        lines.push(format!(
                            "{notice} This replaces what this session was told of `gh` earlier."
                        ));
                    }
                } else {
                    lines.push(GITHUB_CLI_NO_LONGER_ACTIVE.to_owned());
                }
            }
        }
    }
    format!("# Environment update\n{}", lines.join("\n"))
}

fn render_scratchpad_notice(directory: &str) -> String {
    format!("Scratchpad directory: {directory} — always use it for temporary files (intermediate results, scripts, outputs that don't belong in the project) instead of `/tmp` or other system temp directories; it is session-specific, isolated from the project, and can generally be used without permission prompts. Only use `/tmp` if the user explicitly asks.")
}

fn render_github_cli_notice(encoded: &str) -> Option<String> {
    let parts = encoded.split('+').collect::<Vec<_>>();
    if parts.first().copied() != Some("builtin-api") {
        return None;
    }
    let has = |flag: &str| parts.iter().skip(1).any(|part| *part == flag);
    let enterprise = has("enterprise");
    let any_host = has("any-host");
    let runner = has("runner");
    let no_jq = has("no-jq");
    let mut text = String::from("`gh` here is LingXi's built-in GitHub client, not the GitHub CLI (none was found on this machine when the session started). `gh api <REST endpoint>` works against github.com");
    if enterprise {
        text.push_str(" and, with `--hostname <host>`, the GitHub Enterprise Server hosts this session's GitHub proxy serves");
    }
    if runner {
        text.push_str(", already authenticated through this session's GitHub proxy (run `gh api --help` for its flags)");
    } else if any_host {
        text.push_str(", or against the GitHub Enterprise host of this checkout's remote (or `--hostname <host>`), through this session's GitHub proxy, which supplies the credential for the hosts and repositories this session is connected to (run `gh api --help` for its flags)");
    } else {
        text.push_str(if enterprise {
            ", through that proxy,"
        } else {
            ", through this session's GitHub proxy,"
        });
        text.push_str(" which supplies the credential for the repositories this session can reach (run `gh api --help` for its flags)");
    }
    text.push_str(" This holds even if an earlier instruction in this prompt says you have no `gh` CLI or GitHub API access; where the prompt tells you to prefer GitHub MCP tools, keep preferring them. A 403 from the proxy says what this session lacks; retrying does not fix it. Other gh commands are not available: use `gh api` (the error from a common one, such as `gh pr view` or `gh run list`, names its `gh api` form).");
    if no_jq {
        text.push_str(" `--jq` is unavailable because jq is not installed: run without it and pipe the output to a JSON tool that is installed.");
    }
    if runner {
        text.push_str(" Do not install a GitHub CLI to get other gh commands: one installed during the session is not routed through this proxy and has no GitHub credentials unless the runner's operator provides them.");
    }
    Some(text)
}

fn model_identity_from_payload(payload: &Source) -> Option<ModelIdentity> {
    let identity = payload.get_property("identity")?;
    Some(ModelIdentity {
        model_id: exact_string(identity.get_property("modelId")?)?,
        marketing_name: nullable_string(identity.get_property("marketingName")?)?,
        knowledge_cutoff: nullable_string(identity.get_property("knowledgeCutoff")?)?,
    })
}

fn nullable_string(value: JsonValueView<'_>) -> Option<Option<String>> {
    if value.is_null() {
        Some(None)
    } else {
        exact_string(value).map(Some)
    }
}

fn model_ingress_attachment(
    identity: ModelIdentity,
    previous: Option<ModelIdentity>,
) -> Option<PromptIngressAttachment> {
    if previous.as_ref() == Some(&identity) {
        return None;
    }
    let text = render_model_identity(&identity);
    let identity_payload = Source::object(vec![
        ("modelId".into(), Source::from(identity.model_id.clone())),
        (
            "marketingName".into(),
            nullable_string_projection(identity.marketing_name.as_deref()),
        ),
        (
            "knowledgeCutoff".into(),
            nullable_string_projection(identity.knowledge_cutoff.as_deref()),
        ),
    ])
    .ok()?;
    let payload = Source::object(vec![
        ("type".into(), Source::from("model")),
        ("identity".into(), identity_payload),
        ("text".into(), Source::from(text.clone())),
    ])
    .ok()?;
    prompt_ingress_attachment(payload, text)
}

fn render_model_identity(identity: &ModelIdentity) -> String {
    let model = match identity.marketing_name.as_deref() {
        Some(name) if !name.is_empty() => format!(
            "You are powered by the model named {name}. The exact model ID is {}.",
            identity.model_id
        ),
        _ => format!("You are powered by the model {}.", identity.model_id),
    };
    match identity.knowledge_cutoff.as_deref() {
        Some(cutoff) if !cutoff.is_empty() => {
            format!("{model} Assistant knowledge cutoff is {cutoff}.")
        }
        _ => model,
    }
}

fn prompt_ingress_attachment(payload: Source, text: String) -> Option<PromptIngressAttachment> {
    let message = ConversationMessage::user_meta(
        MessageId::new(),
        format!("<system-reminder>\n{text}\n</system-reminder>"),
    );
    let rendered = rendered_system_message(&message)?;
    Some(PromptIngressAttachment {
        message,
        payload,
        rendered,
    })
}

fn string_array(values: &[String]) -> Source {
    Source::array(
        values
            .iter()
            .cloned()
            .map(|value| Source::from(value))
            .collect(),
    )
    .expect("valid Native string array")
}

fn nullable_string_projection(value: Option<&str>) -> Source {
    value.map_or_else(null_value, |value| Source::from(value.to_owned()))
}

fn null_value() -> Source {
    Source::parse("null").expect("valid JSON null")
}

fn bool_value(value: bool) -> Source {
    Source::parse(if value { "true" } else { "false" }).expect("valid JSON boolean")
}

#[cfg(test)]
mod tests {
    use super::*;

    fn environment(cwd: &str, platform: &str, additional: &[&str]) -> EnvironmentSnapshot {
        EnvironmentSnapshot {
            working_directory: Some(cwd.into()),
            is_worktree: false,
            is_git_repo: true,
            additional_working_directories: additional.iter().map(|path| (*path).into()).collect(),
            platform: platform.into(),
            shell: Some("zsh".into()),
            os_version: Some("Darwin 26.2".into()),
            scratchpad_directory: None,
            github_cli: None,
        }
    }

    #[test]
    fn environment_attachment_emits_full_snapshot_then_ordered_delta() {
        let initial = environment("/workspace/one", "darwin", &["/workspace/shared"]);
        let full = environment_ingress_attachment(initial.clone(), PriorEnvironment::default())
            .expect("initial snapshot");
        assert_eq!(full.len(), 1);
        assert_eq!(full[0].payload.string_at("/type"), Some("environment"));
        assert_eq!(
            full[0].payload.string_at("/snapshot/workingDirectory"),
            Some("/workspace/one")
        );
        assert!(!full[0].payload.contains_property("changes"));
        assert!(full[0]
            .rendered
            .string_at("/0/content")
            .unwrap()
            .contains("# Environment\nYou have been invoked"));

        let current = environment(
            "/workspace/two",
            "darwin",
            &["/workspace/new", "/workspace/shared"],
        );
        let delta = environment_ingress_attachment(
            current,
            PriorEnvironment {
                latest: Some(initial.clone()),
                newest_readable: Some(initial),
                has_whole_block: true,
                ..PriorEnvironment::default()
            },
        )
        .expect("cwd and additional-dir delta");
        assert_eq!(delta.len(), 1);
        assert_eq!(
            delta[0].payload.string_at("/changes/0/field"),
            Some("workingDirectory")
        );
        assert_eq!(
            delta[0].payload.string_at("/changes/0/from"),
            Some("/workspace/one")
        );
        assert_eq!(
            delta[0].payload.string_at("/changes/1/field"),
            Some("additionalWorkingDirectories")
        );
        assert_eq!(
            delta[0].payload.string_at("/changes/1/added/0"),
            Some("/workspace/new")
        );
        assert_eq!(
            delta[0].rendered.string_at("/0/content"),
            Some("<system-reminder>\n# Environment update\n - Primary working directory: /workspace/two (was /workspace/one)\n - Additional working directories added:\n  - /workspace/new\n</system-reminder>")
        );
    }

    #[test]
    fn environment_sends_full_snapshot_when_host_identity_changes_and_suppresses_noop() {
        let initial = environment("/workspace/one", "darwin", &[]);
        assert!(environment_ingress_attachment(
            initial.clone(),
            PriorEnvironment {
                latest: Some(initial.clone()),
                newest_readable: Some(initial.clone()),
                has_whole_block: true,
                ..PriorEnvironment::default()
            }
        )
        .expect("valid environment state")
        .is_empty());

        let changed_platform = environment("/workspace/one", "linux", &[]);
        let full = environment_ingress_attachment(
            changed_platform,
            PriorEnvironment {
                latest: Some(initial.clone()),
                newest_readable: Some(initial),
                has_whole_block: true,
                ..PriorEnvironment::default()
            },
        )
        .expect("changed host identity resends full context");
        assert_eq!(full.len(), 1);
        assert!(!full[0].payload.contains_property("changes"));
        assert_eq!(
            full[0].payload.string_at("/snapshot/platform"),
            Some("linux")
        );
    }

    #[test]
    fn environment_recovery_emits_full_snapshot_then_separate_removal_row() {
        let mut previously_announced = environment("/workspace/one", "darwin", &[]);
        previously_announced.scratchpad_directory = Some("/scratch/session".into());
        previously_announced.github_cli = Some("builtin-api+enterprise+runner".into());

        let mut current = environment("/workspace/one", "darwin", &[]);
        current.working_directory = None;
        current.shell = None;
        current.os_version = None;

        let rows = environment_ingress_attachment(
            current,
            PriorEnvironment {
                newest_readable: Some(previously_announced),
                standing: Some(EnvironmentMention {
                    with_scratchpad: true,
                }),
                mentioned: Some(EnvironmentMention {
                    with_scratchpad: true,
                }),
                told_of_builtin_gh: true,
                ..PriorEnvironment::default()
            },
        )
        .expect("valid recovery snapshot");

        assert_eq!(rows.len(), 2);
        assert!(!rows[0].payload.contains_property("changes"));
        assert!(rows[0]
            .payload
            .string_at("/snapshot/workingDirectory")
            .is_none());
        assert_eq!(
            rows[1].payload.string_at("/changes/0/field"),
            Some("workingDirectory")
        );
        assert_eq!(
            rows[1].payload.string_at("/changes/1/field"),
            Some("scratchpadDirectory")
        );
        assert_eq!(
            rows[1].payload.string_at("/changes/2/field"),
            Some("githubCli")
        );
        assert_ne!(rows[0].message.id(), rows[1].message.id());
        let text = rows[1].rendered.string_at("/0/content").unwrap();
        assert!(text.contains(NO_LONGER_SCRATCHPAD));
        assert!(text.contains(GITHUB_CLI_NO_LONGER_ACTIVE));
    }

    #[test]
    fn environment_optional_fields_render_and_diff_with_nested_list_indentation() {
        let mut initial = environment("/workspace/one", "darwin", &["/workspace/shared"]);
        initial.scratchpad_directory = Some("/scratch/session".into());
        initial.github_cli = Some("builtin-api+enterprise+runner+no-jq".into());
        let full = environment_ingress_attachment(initial.clone(), PriorEnvironment::default())
            .expect("full optional-field snapshot");
        let full_text = full[0].rendered.string_at("/0/content").unwrap();
        assert!(full_text.contains("  - /workspace/shared"));
        assert!(full_text.contains("Scratchpad directory: /scratch/session"));
        assert!(full_text.contains("LingXi's built-in GitHub client"));
        assert!(full_text.contains("`--jq` is unavailable"));

        let current = environment("/workspace/one", "darwin", &[]);
        let delta = environment_ingress_attachment(
            current,
            PriorEnvironment {
                latest: Some(initial.clone()),
                newest_readable: Some(initial),
                has_whole_block: true,
                told_of_builtin_gh: true,
                ..PriorEnvironment::default()
            },
        )
        .expect("optional-field removal delta");
        assert_eq!(delta.len(), 1);
        assert_eq!(
            delta[0].payload.string_at("/changes/0/field"),
            Some("additionalWorkingDirectories")
        );
        assert_eq!(
            delta[0].payload.string_at("/changes/1/field"),
            Some("scratchpadDirectory")
        );
        assert_eq!(
            delta[0].payload.string_at("/changes/2/field"),
            Some("githubCli")
        );
        let delta_text = delta[0].rendered.string_at("/0/content").unwrap();
        assert!(delta_text.contains("  - /workspace/shared"));
        assert!(delta_text.contains(NO_LONGER_SCRATCHPAD));
        assert!(delta_text.contains(GITHUB_CLI_NO_LONGER_ACTIVE));
    }

    #[test]
    fn environment_change_fields_read_exact_utf16_strings() {
        let payload = Source::parse(
            r#"{"changes":[{"field":"workingDirectory"},{"field":"other-\ud83e\udd16"},{"field":"\ud800"}]}"#,
        )
        .unwrap();

        assert_eq!(
            environment_change_fields(&payload),
            vec!["workingDirectory".to_owned(), "other-🤖".to_owned()]
        );
    }

    #[test]
    fn environment_history_tracks_standing_context_across_withdrawal_and_unreadable_rows() {
        let mut announced = environment("/workspace/one", "darwin", &[]);
        announced.scratchpad_directory = Some("/scratch/session".into());
        announced.github_cli = Some("builtin-api+runner".into());
        let initial_payload = Source::object(vec![
            ("type".into(), Source::from("environment")),
            (
                "snapshot".into(),
                environment_snapshot_projection(&announced),
            ),
        ])
        .expect("valid environment payload");
        let initial_message = ConversationMessage::user_meta(
            MessageId::new(),
            "<system-reminder>\n# Environment\ncontext\n</system-reminder>".into(),
        );

        let mut prior = PriorEnvironment::default();
        record_environment_row(&mut prior, &initial_message, &initial_payload);
        assert!(prior.has_whole_block);
        assert_eq!(
            prior.standing,
            Some(EnvironmentMention {
                with_scratchpad: true
            })
        );
        assert!(prior.told_of_builtin_gh);

        let mut withdrawn = announced.clone();
        withdrawn.working_directory = None;
        withdrawn.scratchpad_directory = None;
        withdrawn.github_cli = None;
        let changes = vec![
            EnvironmentChange::WorkingDirectory { from: None },
            EnvironmentChange::ScratchpadDirectory,
            EnvironmentChange::GithubCli,
        ];
        let withdrawal = environment_attachment(
            &withdrawn,
            Some(&changes),
            render_environment_changes(&withdrawn, &changes),
        )
        .expect("valid withdrawal row");
        record_environment_row(&mut prior, &withdrawal.message, &withdrawal.payload);
        assert!(prior.standing.is_none());
        assert_eq!(
            prior.mentioned,
            Some(EnvironmentMention {
                with_scratchpad: true
            })
        );
        assert!(prior.told_of_builtin_gh);
        assert!(prior
            .newest_readable
            .as_ref()
            .is_some_and(|snapshot| snapshot.working_directory.is_none()));

        let unreadable_payload = Source::object(vec![("type".into(), Source::from("environment"))])
            .expect("valid attachment kind with unreadable snapshot");
        let unreadable_message = ConversationMessage::user_meta(
            MessageId::new(),
            "<system-reminder>\n# Environment update\nvisible fallback\n</system-reminder>".into(),
        );
        record_environment_row(&mut prior, &unreadable_message, &unreadable_payload);
        assert!(prior.latest.is_none());
        assert!(prior.newest_readable.is_some());
        assert_eq!(
            prior.standing,
            Some(EnvironmentMention {
                with_scratchpad: true
            })
        );
    }

    #[test]
    fn model_attachment_uses_generic_provider_neutral_copy_and_identity_dedupe() {
        let identity = ModelIdentity {
            model_id: "vendor/model-v2".into(),
            marketing_name: Some("Model V2".into()),
            knowledge_cutoff: Some("May 2026".into()),
        };
        let row = model_ingress_attachment(identity.clone(), None).expect("initial model row");
        assert_eq!(row.payload.string_at("/type"), Some("model"));
        assert_eq!(
            row.rendered.string_at("/0/content"),
            Some("<system-reminder>\nYou are powered by the model named Model V2. The exact model ID is vendor/model-v2. Assistant knowledge cutoff is May 2026.\n</system-reminder>")
        );
        assert!(model_ingress_attachment(identity.clone(), Some(identity.clone())).is_none());

        let changed = ModelIdentity {
            knowledge_cutoff: Some("June 2026".into()),
            ..identity
        };
        assert!(model_ingress_attachment(
            changed,
            Some(ModelIdentity {
                model_id: "vendor/model-v2".into(),
                marketing_name: Some("Model V2".into()),
                knowledge_cutoff: Some("May 2026".into()),
            })
        )
        .is_some());
    }

    #[test]
    fn model_identity_uses_exact_resolved_route_listing_for_display_and_cutoff() {
        let api = crate::test_support::MockApiClient::new(Vec::new());
        api.set_model_listings(vec![
            lingxi_core::host::orchestrator::ModelListing {
                display_model: "Vendor Model A".into(),
                request_model: "vendor/model-v2".into(),
                provider_id: "provider-a".into(),
                metadata: lingxi_core::host::ModelMetadata {
                    knowledge_cutoff: Some("March 2025".into()),
                    ..Default::default()
                },
                ..Default::default()
            },
            lingxi_core::host::orchestrator::ModelListing {
                display_model: "Vendor Model B".into(),
                request_model: "vendor/model-v2".into(),
                provider_id: "provider-b".into(),
                metadata: lingxi_core::host::ModelMetadata {
                    knowledge_cutoff: Some("October 2025".into()),
                    ..Default::default()
                },
                ..Default::default()
            },
        ]);

        let identity = model_identity_for_route(&api, "vendor/model-v2", Some("provider-b"));
        assert_eq!(identity.model_id, "vendor/model-v2");
        assert_eq!(identity.marketing_name.as_deref(), Some("Vendor Model B"));
        assert_eq!(identity.knowledge_cutoff.as_deref(), Some("October 2025"));
        assert!(
            model_identity_for_route(&api, "vendor/model-v2", None)
                .marketing_name
                .is_none(),
            "unqualified duplicate routes must not choose a listing by iteration order"
        );
    }
}
