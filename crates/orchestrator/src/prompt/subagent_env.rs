//! Subagent `<env>` block formatter — the environment details claude-code
//! appends to a NON-fork subagent's system prompt (after the `Notes:` trailer).
//!
//! Byte-locked against claude-code v2.1.186 `tIm` (binary offset ~206671222).
//! The 2.1.186 subagent assembler returns `[...agentBody, notes, envBlock, ...]`
//! (`…not files you create.`,s=await tIm(t,n),i=N4l(t);return[...e,o,s,…]`), so
//! the subagent now carries the FULL `<env>` block — a change from v2.1.183,
//! where the subagent path selected the static `zym` (model + cutoff only). The
//! MAIN-prompt `# Environment` form lives in [`super::env_block`]; this is the
//! distinct `<env>`-wrapped subagent form (capitalized `Yes`/`No`, no static
//! model-IDs / Fast-mode trailer).
//!
//! Template (`tIm`):
//! ```text
//! Here is useful information about the environment you are running in:
//! <env>
//! Working directory: {cwd}
//! Is directory a git repo: {Yes|No}
//! {Additional working directories: …\n}Platform: {platform}
//! Shell: {shell}
//! OS Version: {os_version}
//! </env>
//! {model line}{\n\nAssistant knowledge cutoff is {cutoff}.}
//! ```
//! - the git line renders `Yes`/`No` (capitalized — NOT `true`/`false`);
//! - the additional-working-dirs element is omitted when empty;
//! - `${l}` (`Hbn()`, an extra-append slot) is empty in practice and omitted;
//! - the model + cutoff lines are OUTSIDE `</env>`: the model line directly
//!   follows the `</env>\n`, and the cutoff (when known) carries a leading
//!   `\n\n`. The block carries NO trailing newline (the assembler joins it).
#![forbid(unsafe_code)]

use crate::prompt::env_meta::{detect_shell, knowledge_cutoff_for_model, marketing_name_for_model};
use std::fmt::Write;
use std::path::{Path, PathBuf};

/// Map Rust's `std::env::consts::OS` (`macos`/`windows`/…) to the node
/// `process.platform` token (`darwin`/`win32`/…) claude-code emits. Mirrors the
/// main-prompt `node_platform_name` (conversation.rs).
#[must_use]
fn node_platform_name(rust_os: &str) -> &str {
    match rust_os {
        "macos" => "darwin",
        "windows" => "win32",
        other => other,
    }
}

/// Build a boot-time renderer closure for the subagent `<env>` block: probe the
/// (boot-stable) environment ONCE — cwd, git-repo-ness, node platform, shell, OS
/// version — and return a `Fn(model_id) -> String` that the subagent spawner
/// invokes per spawn with the spawn's RESOLVED model id. Reuses the orchestrator's
/// own env probes ([`crate::prompt::git_status::probe`] /
/// [`crate::prompt::env_meta::os_version_string`]) so the bytes match the main
/// prompt's environment values.
/// Build a boot-time renderer closure for the subagent `<env>` block: probe the
/// (boot-stable) environment ONCE — git-repo-ness, node platform, shell, OS
/// version — and return a `Fn(model_id, cwd_override) -> String` that the
/// subagent spawner invokes per spawn with the spawn's RESOLVED model id and an
/// optional per-agent cwd. `cwd_override = Some(p)` (a worktree-isolated or
/// explicit-`cwd` agent) renders `Working directory: <p>` + the "This is a git
/// worktree …" notice so the agent forms absolute paths under `p`; `None` uses
/// the boot cwd. Reuses the orchestrator's own env probes so the bytes match the
/// main prompt's environment values.
#[must_use]
pub fn boot_renderer(
    cwd: PathBuf,
) -> impl Fn(&str, Option<&Path>) -> String + Send + Sync + 'static {
    let is_git_repo = crate::prompt::git_status::probe(&cwd).is_some();
    let platform = node_platform_name(std::env::consts::OS).to_string();
    let shell = detect_shell();
    let os_version = crate::prompt::env_meta::os_version_string();
    move |model_id: &str, cwd_override: Option<&Path>| {
        let (effective_cwd, in_worktree) = match cwd_override {
            Some(p) => (p, true),
            None => (cwd.as_path(), false),
        };
        subagent_env_block(
            model_id,
            effective_cwd,
            is_git_repo,
            &platform,
            &shell,
            &os_version,
            &[],
            in_worktree,
        )
    }
}

/// Render the subagent `<env>` block for a RESOLVED model id + environment.
///
/// `platform` is the node `process.platform` value (`darwin`/`linux`/`win32`);
/// the caller maps `std::env::consts::OS` to it (same as the main path).
/// `shell` is the bare value (`zsh`/`bash`/raw `$SHELL`) — the `Shell: ` label
/// is added here. The marketing name + knowledge cutoff are derived from
/// `model_id` via [`marketing_name_for_model`] / [`knowledge_cutoff_for_model`].
#[must_use]
pub fn subagent_env_block(
    model_id: &str,
    cwd: &Path,
    is_git_repo: bool,
    platform: &str,
    shell: &str,
    os_version: &str,
    additional_dirs: &[String],
    in_worktree: bool,
) -> String {
    let mut s = String::with_capacity(512);
    s.push_str("Here is useful information about the environment you are running in:\n<env>\n");
    writeln!(&mut s, "Working directory: {}", cwd.display()).unwrap();
    // claude-code worktree notice (`nIm`, emitted when in a git worktree): tells
    // the isolated agent to run everything from the worktree and never `cd` back
    // to the original checkout. The em-dash is U+2014.
    if in_worktree {
        s.push_str(
            "This is a git worktree \u{2014} an isolated copy of the repository. \
Run all commands from this directory. Do NOT `cd` to the original repository root.\n",
        );
    }
    writeln!(
        &mut s,
        "Is directory a git repo: {}",
        if is_git_repo { "Yes" } else { "No" }
    )
    .unwrap();
    // Optional additional-working-dirs element (prefixes the Platform line).
    if !additional_dirs.is_empty() {
        writeln!(
            &mut s,
            "Additional working directories: {}",
            additional_dirs.join(", ")
        )
        .unwrap();
    }
    writeln!(&mut s, "Platform: {platform}").unwrap();
    writeln!(&mut s, "Shell: {shell}").unwrap();
    writeln!(&mut s, "OS Version: {os_version}").unwrap();
    s.push_str("</env>\n");
    // Model line `o` — named form when the marketing name is known, else the
    // bare-id fallback (claude-code `og(e)` truthy/falsy). NO leading newline:
    // it directly follows `</env>\n`.
    match marketing_name_for_model(model_id) {
        Some(name) => write!(
            &mut s,
            "You are powered by the model named {name}. The exact model ID is {model_id}."
        )
        .unwrap(),
        None => write!(&mut s, "You are powered by the model {model_id}.").unwrap(),
    }
    // Cutoff line `a` — leading `\n\n`, omitted entirely when unknown.
    if let Some(cutoff) = knowledge_cutoff_for_model(model_id) {
        write!(&mut s, "\n\nAssistant knowledge cutoff is {cutoff}.").unwrap();
    }
    s
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::path::PathBuf;

    #[test]
    fn named_model_with_cutoff_is_byte_exact() {
        let out = subagent_env_block(
            "claude-opus-4-8[1m]",
            &PathBuf::from("/work/proj"),
            true,
            "darwin",
            "zsh",
            "Darwin 24.6.0",
            &[],
            false,
        );
        assert_eq!(
            out,
            "Here is useful information about the environment you are running in:\n\
<env>\n\
Working directory: /work/proj\n\
Is directory a git repo: Yes\n\
Platform: darwin\n\
Shell: zsh\n\
OS Version: Darwin 24.6.0\n\
</env>\n\
You are powered by the model named Opus 4.8 (1M context). The exact model ID is claude-opus-4-8[1m].\n\n\
Assistant knowledge cutoff is January 2026."
        );
    }

    #[test]
    fn non_git_unknown_model_no_cutoff() {
        let out = subagent_env_block(
            "some-unknown-model",
            &PathBuf::from("/x"),
            false,
            "linux",
            "bash",
            "Linux 6.6",
            &[],
            false,
        );
        assert_eq!(
            out,
            "Here is useful information about the environment you are running in:\n\
<env>\n\
Working directory: /x\n\
Is directory a git repo: No\n\
Platform: linux\n\
Shell: bash\n\
OS Version: Linux 6.6\n\
</env>\n\
You are powered by the model some-unknown-model."
        );
        // No cutoff line when the model is unknown.
        assert!(!out.contains("knowledge cutoff"));
    }

    #[test]
    fn additional_working_dirs_prefix_platform() {
        let out = subagent_env_block(
            "claude-opus-4-8[1m]",
            &PathBuf::from("/x"),
            true,
            "darwin",
            "zsh",
            "Darwin 24.6.0",
            &["/a".to_string(), "/b".to_string()],
            false,
        );
        assert!(out.contains("Additional working directories: /a, /b\nPlatform: darwin\n"));
    }

    #[test]
    fn worktree_agent_shows_worktree_cwd_and_notice() {
        let out = subagent_env_block(
            "claude-opus-4-8[1m]",
            &PathBuf::from("/repo/.lingxi/worktrees/agent-x"),
            true,
            "darwin",
            "zsh",
            "Darwin 24.6.0",
            &[],
            true,
        );
        assert!(out.contains("Working directory: /repo/.lingxi/worktrees/agent-x\n"));
        // The worktree notice follows the working-directory line (em-dash U+2014).
        assert!(out.contains(
            "This is a git worktree \u{2014} an isolated copy of the repository. \
Run all commands from this directory. Do NOT `cd` to the original repository root.\n"
        ));
        // A non-worktree agent does NOT get the notice.
        let plain = subagent_env_block(
            "claude-opus-4-8[1m]",
            &PathBuf::from("/x"),
            true,
            "darwin",
            "zsh",
            "Darwin 24.6.0",
            &[],
            false,
        );
        assert!(!plain.contains("This is a git worktree"));
    }
}
