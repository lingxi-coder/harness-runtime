//! Guest→host path translation for permission checks.
//!
//! MOBILE DIVERGENCE. There is no oracle counterpart: claude-code 2.1.235 has
//! a single filesystem coordinate space, so `filesystem.ts` never has to ask
//! "which side of a mount is this path on".
//!
//! On mobile the model names files in GUEST coordinates (`/workspace/<id>/…`,
//! the same spelling the shell and the file tools use) while every root in
//! [`crate::FsRoots`] is a HOST path. `expand_path` keeps an absolute path
//! verbatim, so `path_matches_rule_pattern` relativizes a guest path against a
//! host root, gets a `../`-prefixed string, and bails — no allow rule can ever
//! match and every edit prompts. Measured on device:
//!
//! ```text
//! host_cwd  = /private/var/mobile/Containers/Data/Application/…/LingxiCode
//! model_cwd = /workspace/30c17980-f397-434b-8dba-4c3690a9f60a
//! ```
//!
//! The fix reuses the SAME `translate_model_path` seam the file tools call
//! (`tools/file/src/write.rs`, `edit.rs`), so the gate and the tool can never
//! disagree about which mount a path belongs to.

use std::sync::Arc;

/// Answers "does this model-supplied path have a host twin?" without teaching
/// `permission` about I/O. [`lingxi_core::host::FileSystem::translate_model_path`] is a
/// sync trait method, so this is callable from the sync gate.
pub trait ModelPathTranslator: Send + Sync {
    /// Resolve a model-supplied path against the guest/host mount table.
    fn translate(&self, model_path: &str, write: bool) -> ModelPathOutcome;

    /// The host twin, or `None` for anything else. Convenience for callers
    /// that treat "not a guest path" and "fenced" alike.
    fn to_host(&self, model_path: &str, write: bool) -> Option<String> {
        match self.translate(model_path, write) {
            ModelPathOutcome::Host(host) => Some(host),
            _ => None,
        }
    }
}

/// Why a translation did or did not produce a host path.
///
/// The two non-success cases are NOT interchangeable, and collapsing them is a
/// fail-open on the enumeration path: `NotGuest` means the caller already holds
/// a host path and may proceed, while `Fenced` means the guest region must not
/// be reached from the host at all — treating that as "no host twin, carry on"
/// silently produces zero read-deny exclusions, which is indistinguishable from
/// "this policy has no deny rules".
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ModelPathOutcome {
    /// The path is in guest space and this is its host twin.
    Host(String),
    /// The path is not in guest space; the caller's own path is already host.
    NotGuest,
    /// Inside the model-visible space, but must not be touched from the host.
    Fenced,
}

/// Production adapter over the session's [`lingxi_core::host::FileSystem`].
pub struct FileSystemPathTranslator(pub Arc<dyn lingxi_core::host::FileSystem>);

impl ModelPathTranslator for FileSystemPathTranslator {
    fn translate(&self, model_path: &str, write: bool) -> ModelPathOutcome {
        // `Err(..)` means the path is inside the model-visible space but must
        // not be touched from the host (iSH fakefs regions, or a read-only
        // mount for `write == true`). It is reported as `Fenced`, never as a
        // rewrite and never as a verdict: on the authorize path the raw guest
        // path then flows to the policy exactly as it does today (relativizes
        // to `../…`, matches no allow rule, prompts) and the tool refuses it a
        // moment later with the filesystem's own message. Turning a
        // translation refusal into an allow would authorize a path the host
        // must not reach.
        match self.0.translate_model_path(model_path, write) {
            Ok(Some(host)) => ModelPathOutcome::Host(host),
            Ok(None) => ModelPathOutcome::NotGuest,
            Err(_) => ModelPathOutcome::Fenced,
        }
    }
}

/// Rewrite ONLY the tool's declared path field. `None` means nothing changed.
///
/// The path field comes from [`crate::filesystem::input_path_field_for_tool`],
/// the same helper the permission check itself uses, so the two can never
/// disagree about which field to read.
pub(crate) fn rewrite_tool_input(
    translator: &dyn ModelPathTranslator,
    tool_name: &str,
    input: &serde_json::Value,
) -> Option<serde_json::Value> {
    let write = match crate::filesystem::file_tool_kind(tool_name) {
        crate::filesystem::FileToolKind::Editor => true,
        crate::filesystem::FileToolKind::Reader => false,
        // Bash / PowerShell / WebFetch / MCP are NEVER touched: a host
        // container path spliced into a guest shell string names a file that
        // does not exist inside the guest.
        crate::filesystem::FileToolKind::NonFile => return None,
    };
    let field = crate::filesystem::input_path_field_for_tool(tool_name);
    let raw = input.get(field)?.as_str()?;
    if !std::path::Path::new(raw).is_absolute() {
        // A relative path needs no rewrite: `expand_path` joins it onto
        // `roots.cwd`, which is already the workspace host root.
        return None;
    }
    let host = translator.to_host(raw, write)?;
    let mut out = input.clone();
    out[field] = serde_json::Value::String(host);
    Some(out)
}
