//! ORIGIN TRUST for an agent definition's frontmatter `hooks:` — claude-code
//! 2.1.218 `mvo` / `psd` / `hvo`.
//!
//! # Why this exists
//!
//! An agent definition (`<dir>/agents/*.md`) may declare a `hooks:` frontmatter
//! block, and registering it installs hook COMMANDS that run on the user's
//! machine. Through 2.1.217 that registration was UNCONDITIONAL:
//!
//! ```text
//! let Ft = !CR("hooks") || qje(e.source);
//! if (e.hooks && Ft) SQu(r.sessionHooksRegistry, oe, e.hooks, `agent '…'`, !0);
//! ```
//!
//! 2.1.218 put a folder-trust gate in front of both registration sites. The
//! sharp case it closes: `--add-dir <untrusted-repo>` where that repo ships
//! `.claude/agents/*.md` carrying a `hooks:` block — previously that installed
//! arbitrary commands with no trust prompt (the new telemetry even carries a
//! dedicated `fromAdditionalDirectory` tag). This is an arbitrary-code-execution
//! provenance guard, so it fails CLOSED: anything we cannot positively establish
//! as trusted does not get its hooks registered.
//!
//! # The gate (`mvo`)
//!
//! ```text
//! function mvo(e){
//!   if (J0e(e.source)) return !0;                                  // trusted source set
//!   if (e.source==="userSettings" || e.source==="flagSettings") return !0;
//!   if (!e.baseDir) return iB();                                   // no base dir ⇒ trust of cwd
//!   return tdr(psd(e.baseDir));
//! }
//! ```
//!
//! `J0e` is the set `{plugin, policySettings, built-in, builtin, bundled}`; the
//! second arm adds `userSettings`/`flagSettings`. Mapping those onto
//! [`AgentSource`] via `agent_source_to_claude_str` leaves exactly ONE source
//! that must prove folder trust: [`AgentSource::Settings(protocol::SettingsScope::Project)`] (`projectSettings`) —
//! precisely the definitions that come from a workspace/`--add-dir` tree.

use crate::definition::{AgentDefinition, AgentSource};
use std::path::{Path, PathBuf};

/// claude-code `psd` — the folder whose trust grant governs a definition loaded
/// from `base_dir`.
///
/// ```text
/// function psd(e){
///   let t = dirname(e);
///   if (basename(e)==="agents" && basename(t)===".claude") return dirname(t);
///   return e;
/// }
/// ```
///
/// i.e. a `<X>/<dot-dir>/agents` definition directory resolves to the project
/// root `<X>`; any other directory is its own trust root. The port's dot-dir is
/// [`branding::DOT_DIR`] (`.lingxi`) under the accepted naming divergence.
#[must_use]
pub fn hooks_trust_dir(base_dir: &Path) -> PathBuf {
    let parent = base_dir.parent();
    let is_agents = base_dir.file_name().is_some_and(|n| n == "agents");
    let parent_is_dot_dir = parent
        .and_then(Path::file_name)
        .is_some_and(|n| n == branding::DOT_DIR);
    if is_agents && parent_is_dot_dir {
        if let Some(root) = parent.and_then(Path::parent) {
            return root.to_path_buf();
        }
    }
    base_dir.to_path_buf()
}

/// `true` when the definition's SOURCE alone establishes trust — claude-code's
/// `J0e(e.source)` set plus the `userSettings`/`flagSettings` arm of `mvo`.
///
/// Only [`AgentSource::Settings(protocol::SettingsScope::Project)`] (`"projectSettings"`) falls through to the
/// folder-trust check.
#[must_use]
pub fn source_is_self_trusting(source: AgentSource) -> bool {
    match source {
        // `J0e` = {plugin, policySettings, built-in, builtin, bundled}
        AgentSource::BuiltIn
        | AgentSource::Plugin
        | AgentSource::Settings(protocol::SettingsScope::Managed) => true,
        // `e.source==="userSettings" || e.source==="flagSettings"`
        AgentSource::Settings(protocol::SettingsScope::User)
        | AgentSource::Flag
        | AgentSource::AdditionalDirectory => true,
        // `localSettings` is in neither claude set — not in `J0e`
        // {plugin, policySettings, built-in, builtin, bundled}, and not in
        // `mvo`'s userSettings/flagSettings arm — so like `projectSettings` it
        // falls through to the folder-trust check. Unreachable today; erring
        // toward requiring trust is also the safe direction if it ever is not.
        AgentSource::Settings(protocol::SettingsScope::Local)
        | AgentSource::Settings(protocol::SettingsScope::Project) => false,
    }
}

/// claude-code `mvo` — may this definition's frontmatter `hooks:` be registered?
///
/// `cwd` is the fallback trust root for a definition carrying no `base_dir`
/// (`if(!e.baseDir) return iB()`, where `iB` checks the trust of the cwd).
///
/// FAIL-CLOSED: when the global config cannot be located, the trust grant cannot
/// be established, so this returns `false` — mirroring `tdr`, whose
/// `xt().projects?.[key]?.hasTrustDialogAccepted===!0` is likewise false against
/// a missing/unreadable config. (The port's
/// [`migrations::global_config::check_has_trust_dialog_accepted`] already
/// degrades the same way for a broken config, and honours in-memory session
/// trust as a first short-circuit.)
#[must_use]
pub fn agent_hooks_origin_trusted(def: &AgentDefinition, cwd: &Path) -> bool {
    // FAIL-CLOSED: no locatable global config ⇒ no provable grant ⇒ no hooks.
    migrations::global_config::global_config_path()
        .is_some_and(|cfg| agent_hooks_origin_trusted_with_config(def, cwd, &cfg))
}

/// [`agent_hooks_origin_trusted`] against an EXPLICIT global-config path.
///
/// Split out so the decision is testable without touching the process
/// environment (`global_config_path` resolves through `HOME`) — the same
/// inject-the-root discipline the CLI tests use.
#[must_use]
pub fn agent_hooks_origin_trusted_with_config(
    def: &AgentDefinition,
    cwd: &Path,
    config_path: &Path,
) -> bool {
    if source_is_self_trusting(def.source) {
        return true;
    }
    project_trusted_exact(config_path, &trust_dir_for(def, cwd))
}

/// claude-code `tdr` — `xt().projects?.[Frn(e)]?.hasTrustDialogAccepted===!0`.
///
/// DELIBERATELY **exact-key**: `mvo` calls `tdr`, NOT the ancestor-walking
/// `EUe` (`checkHasTrustDialogAccepted`), and `tdr` consults no in-process
/// session trust. Using the walker here would fail OPEN in two everyday ways:
///
/// * **ancestor leak** — trusting `~/Projects` once would silently trust every
///   repository cloned beneath it, so any of them could install agent hooks;
/// * **session-trust blanket** — a session started with `cwd == $HOME` sets the
///   process-wide session-trust flag, which would then trust *every* folder.
///
/// Both would defeat the guard this module exists for, so the exact-key form is
/// load-bearing, not an optimisation.
fn project_trusted_exact(config_path: &Path, dir: &Path) -> bool {
    let key = migrations::global_config::project_path_for_config(dir);
    matches!(
        migrations::global_config::get_project_config(config_path, &key),
        Ok(p) if p.get("hasTrustDialogAccepted") == Some(&serde_json::Value::Bool(true))
    )
}

/// The folder whose trust grant governs this definition — `psd(e.baseDir)`, or
/// the cwd when the definition carries no base dir (`if(!e.baseDir) return iB()`).
#[must_use]
pub fn trust_dir_for(def: &AgentDefinition, cwd: &Path) -> PathBuf {
    if def.base_dir.as_os_str().is_empty() {
        cwd.to_path_buf()
    } else {
        hooks_trust_dir(&def.base_dir)
    }
}

/// The surface a skipped registration is reported on — claude-code `hvo`'s `t`
/// parameter, which also selects the noun in the log line.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum HooksTrustSurface {
    /// A spawned subagent's definition (`"subagent"`, noun `"agent"`).
    Subagent,
    /// The main-thread `--agent` definition (`"mainThread"`, noun
    /// `"main-thread agent"`).
    MainThread,
}

impl HooksTrustSurface {
    /// The `surface` telemetry tag.
    #[must_use]
    pub fn tag(self) -> &'static str {
        match self {
            Self::Subagent => "subagent",
            Self::MainThread => "mainThread",
        }
    }

    /// The noun interpolated into the log line (`${n}`).
    #[must_use]
    pub fn noun(self) -> &'static str {
        match self {
            Self::Subagent => "agent",
            Self::MainThread => "main-thread agent",
        }
    }
}

/// claude-code `hvo` — log + count a SKIPPED frontmatter-hook registration.
///
/// The message is byte-faithful to the binary:
///
/// ```text
/// Skipping frontmatter hooks for ${n} '${agentType}': the folder its definition
/// file came from is not trusted (source: ${source}, trust key: ${dsd(r)}). Run
/// Claude Code there once and accept the trust dialog, or set
/// projects[${dsd(r)}].hasTrustDialogAccepted: true in ${nE()}.
/// ```
///
/// `r` is `AI_(e) ?? aq()` — the `psd`-derived trust dir, falling back to cwd.
///
/// `from_additional_directory` fills the binary's third telemetry tag. The
/// catalog loader does not yet track whether a definition came from an
/// `--add-dir` directory, so both call sites currently pass `false`; the
/// parameter exists so that plumbing lands without touching this signature.
pub fn report_untrusted_hooks(
    def: &AgentDefinition,
    cwd: &Path,
    surface: HooksTrustSurface,
    from_additional_directory: bool,
) {
    // `hvo` prints `dsd(AI_(e) ?? aq())` where `AI_ = Frn(psd(baseDir))` — the
    // NORMALIZED project key, i.e. exactly the key the gate reads. Printing the
    // raw dir would hand the user a `projects[...]` path that doesn't match.
    let dir = trust_dir_for(def, cwd);
    let trust_key = migrations::global_config::project_path_for_config(&dir);
    let config = migrations::global_config::global_config_path().map_or_else(
        || "<no global config>".to_string(),
        |p| p.display().to_string(),
    );
    tracing::error!(
        "Skipping frontmatter hooks for {} '{}': the folder its definition file came from is not \
         trusted (source: {}, trust key: {trust_key}). Run Claude Code there once and accept the \
         trust dialog, or set projects[{trust_key}].hasTrustDialogAccepted: true in {config}.",
        surface.noun(),
        def.agent_type,
        crate::handle::agent_source_to_claude_str(def.source),
    );
    telemetry::emit_agent_hooks_origin_untrusted(
        crate::handle::agent_source_to_claude_str(def.source),
        surface.tag(),
        from_additional_directory,
    );
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn psd_resolves_dot_dir_agents_to_project_root() {
        // `<X>/<dot>/agents` → `<X>` (the claude-code `.claude/agents` case).
        let base = PathBuf::from("/proj/work")
            .join(branding::DOT_DIR)
            .join("agents");
        assert_eq!(hooks_trust_dir(&base), PathBuf::from("/proj/work"));
    }

    #[test]
    fn psd_leaves_other_dirs_as_their_own_trust_root() {
        // Not `<dot>/agents` ⇒ the directory itself is the trust root.
        assert_eq!(
            hooks_trust_dir(Path::new("/somewhere/agents")),
            PathBuf::from("/somewhere/agents")
        );
        assert_eq!(
            hooks_trust_dir(Path::new("/proj/work")),
            PathBuf::from("/proj/work")
        );
        // `agents` under a NON-dot dir must not climb two levels.
        assert_eq!(
            hooks_trust_dir(Path::new("/proj/.other/agents")),
            PathBuf::from("/proj/.other/agents")
        );
    }

    #[test]
    fn only_project_source_requires_folder_trust() {
        // `J0e` + the userSettings/flagSettings arm ⇒ self-trusting.
        for s in [
            AgentSource::BuiltIn,
            AgentSource::Plugin,
            AgentSource::Settings(protocol::SettingsScope::Managed),
            AgentSource::Settings(protocol::SettingsScope::User),
            AgentSource::Flag,
        ] {
            assert!(source_is_self_trusting(s), "{s:?} must be self-trusting");
        }
        // The workspace/`--add-dir` case is the one that must prove trust.
        assert!(!source_is_self_trusting(AgentSource::Settings(
            protocol::SettingsScope::Project
        )));
    }

    /// Build a definition rooted at `<proj>/<dot>/agents` with the given source.
    fn def_at(proj: &Path, source: AgentSource) -> AgentDefinition {
        let mut d = crate::builtins::workflow_subagent_definition();
        d.agent_type = "rogue".to_string();
        d.source = source;
        d.base_dir = proj.join(branding::DOT_DIR).join("agents");
        d
    }

    /// THE security property: a project-source definition sitting in a folder
    /// that was never trusted must NOT get its frontmatter hooks registered, and
    /// must start doing so once the trust dialog is accepted for that folder.
    /// Uses a real temp config + the real trust writer (no env mutation).
    #[test]
    fn project_agent_hooks_require_folder_trust() {
        let tmp = tempfile::tempdir().unwrap();
        let proj = tmp.path().join("untrusted-repo");
        std::fs::create_dir_all(proj.join(branding::DOT_DIR).join("agents")).unwrap();
        let cfg = tmp.path().join(".lingxi.json");
        std::fs::write(&cfg, "{}").unwrap();

        let def = def_at(
            &proj,
            AgentSource::Settings(protocol::SettingsScope::Project),
        );

        // Before any trust grant: hooks are refused.
        assert!(
            !agent_hooks_origin_trusted_with_config(&def, &proj, &cfg),
            "an untrusted folder's agent must NOT get its hooks registered"
        );

        // The trust key is the PROJECT ROOT, not the `<dot>/agents` dir.
        assert_eq!(trust_dir_for(&def, &proj), proj);

        // Grant trust for that folder through the real writer, then it registers.
        migrations::global_config::mark_trust_dialog_accepted(&cfg, &proj).unwrap();
        assert!(
            agent_hooks_origin_trusted_with_config(&def, &proj, &cfg),
            "after the trust dialog is accepted the agent's hooks register"
        );
    }

    /// A plugin/policy/built-in definition is trusted by SOURCE and must never be
    /// blocked by folder trust — otherwise the gate would break bundled agents.
    #[test]
    fn self_trusting_sources_bypass_folder_trust() {
        let tmp = tempfile::tempdir().unwrap();
        let proj = tmp.path().join("untrusted-repo");
        std::fs::create_dir_all(&proj).unwrap();
        let cfg = tmp.path().join(".lingxi.json");
        std::fs::write(&cfg, "{}").unwrap();

        for s in [
            AgentSource::BuiltIn,
            AgentSource::Plugin,
            AgentSource::Settings(protocol::SettingsScope::Managed),
            AgentSource::Settings(protocol::SettingsScope::User),
            AgentSource::Flag,
        ] {
            assert!(
                agent_hooks_origin_trusted_with_config(&def_at(&proj, s), &proj, &cfg),
                "{s:?} is trusted by source even in an untrusted folder"
            );
        }
    }

    /// REGRESSION (adversarial review, fail-open #2): `mvo` uses the EXACT-key
    /// `tdr`, not the ancestor-walking `EUe`. Trusting a PARENT directory must
    /// NOT trust a repo nested inside it — otherwise trusting `~/Projects` once
    /// would let every repo cloned under it install agent hooks.
    #[test]
    fn parent_directory_trust_does_not_leak_to_nested_repo() {
        let tmp = tempfile::tempdir().unwrap();
        let parent = tmp.path().join("Projects");
        let nested = parent.join("evil-repo");
        std::fs::create_dir_all(nested.join(branding::DOT_DIR).join("agents")).unwrap();
        let cfg = tmp.path().join(".lingxi.json");
        std::fs::write(&cfg, "{}").unwrap();

        // Trust ONLY the parent.
        migrations::global_config::mark_trust_dialog_accepted(&cfg, &parent).unwrap();

        let def = def_at(
            &nested,
            AgentSource::Settings(protocol::SettingsScope::Project),
        );
        assert!(
            !agent_hooks_origin_trusted_with_config(&def, &nested, &cfg),
            "trusting a PARENT dir must not trust a nested repo's agent hooks"
        );

        // …and trusting the repo itself does grant it (proves the test isn't
        // just asserting a broken lookup).
        migrations::global_config::mark_trust_dialog_accepted(&cfg, &nested).unwrap();
        assert!(
            agent_hooks_origin_trusted_with_config(&def, &nested, &cfg),
            "trusting the repo itself grants its agent hooks"
        );
    }

    /// The remediation line must print the SAME key the gate reads
    /// (`Frn(psd(baseDir))`), or the user's `projects[...]` edit won't take.
    #[test]
    fn reported_trust_key_matches_the_key_the_gate_reads() {
        let tmp = tempfile::tempdir().unwrap();
        let proj = tmp.path().join("repo");
        std::fs::create_dir_all(proj.join(branding::DOT_DIR).join("agents")).unwrap();
        let def = def_at(
            &proj,
            AgentSource::Settings(protocol::SettingsScope::Project),
        );
        let dir = trust_dir_for(&def, &proj);
        // What the gate reads:
        let gate_key = migrations::global_config::project_path_for_config(&dir);
        // The reporter formats this same normalized key.
        assert!(!gate_key.is_empty());
        assert_eq!(
            gate_key,
            migrations::global_config::project_path_for_config(&proj),
            "the trust key is the project root, normalized"
        );
    }

    /// Missing/unreadable global config ⇒ no provable grant ⇒ FAIL CLOSED.
    #[test]
    fn missing_config_fails_closed_for_project_source() {
        let tmp = tempfile::tempdir().unwrap();
        let proj = tmp.path().join("repo");
        std::fs::create_dir_all(&proj).unwrap();
        let missing = tmp.path().join("does-not-exist.json");
        assert!(
            !agent_hooks_origin_trusted_with_config(
                &def_at(
                    &proj,
                    AgentSource::Settings(protocol::SettingsScope::Project)
                ),
                &proj,
                &missing
            ),
            "an absent config must not imply trust"
        );
    }

    #[test]
    fn surface_tags_and_nouns_match_the_binary() {
        assert_eq!(HooksTrustSurface::Subagent.tag(), "subagent");
        assert_eq!(HooksTrustSurface::MainThread.tag(), "mainThread");
        assert_eq!(HooksTrustSurface::Subagent.noun(), "agent");
        assert_eq!(HooksTrustSurface::MainThread.noun(), "main-thread agent");
    }
}
