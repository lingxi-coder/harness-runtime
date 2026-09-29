//! `Zl` (2.1.266 `src_160529511.js` @323641) — the session's plan files, the one
//! write carve-out plan mode grants.
//!
//! ```js
//! function Zl(e,n){let r=i8();                 // the session's plan slug
//!   if(!r)return!1;
//!   let o=je(e);                               // resolved path
//!   if(dp(o)!==je(Ra()))return!1;              // dirname(path) === plansDir()
//!   let d=up(o);                               // basename
//!   return d===`${r}.md`
//!       || d===`${r}.workshop.md` && n?.includeWorkshopDoc===!0 && ZUe()
//!       || d.startsWith(`${r}-agent-`)&&d.endsWith(".md")}
//! ```
//!
//! Two resolvers consult it (`src_160529511.js`):
//!
//! * `LZe` @351498, the WRITE resolver, with
//!   `includeWorkshopDoc: permissionMode==="plan"` →
//!   [`PLAN_FILE_WRITE_ALLOW_REASON`];
//! * the READ resolver @352778, with `includeWorkshopDoc:!0` →
//!   [`PLAN_FILE_READ_ALLOW_REASON`].
//!
//! Both sit AFTER the deny/ask rule walks and BEFORE the safety check and the
//! plan-mode mutation ask, so a plan file inside an otherwise protected
//! directory is still writable — which matters here, because LingXi's default
//! plans directory is `<project-root>/.lingxi/plans`, and `.lingxi` is one of
//! the protected directory components.
//!
//! ## Divergence (reason)
//!
//! `ZUe()` gates the `.workshop.md` sibling on the workshop skill being present.
//! LingXi ships no workshop skill (`git grep workshop -- commands/ skill-api/`
//! → 0 hits), so [`PlanFileIdentity::workshop_enabled`] is `false` everywhere
//! and that arm is unreachable — exactly as it is unreachable upstream without
//! the skill. The arm is implemented anyway so enabling the skill later needs no
//! change here.

use std::path::{Path, PathBuf};
use std::sync::RwLock;

/// `Oe(n,"Plan files for current session are allowed for writing")` — `LZe` @351534.
pub const PLAN_FILE_WRITE_ALLOW_REASON: &str =
    "Plan files for current session are allowed for writing";

/// `Oe(n,"Plan files for current session are allowed for reading")` — the read
/// resolver @352778.
pub const PLAN_FILE_READ_ALLOW_REASON: &str =
    "Plan files for current session are allowed for reading";

/// The `(plansDir, slug)` pair `Zl` closes over (`Ra()` and `i8()`).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PlanFileIdentity {
    /// `Ra()` — the resolved plans directory.
    pub plans_dir: PathBuf,
    /// `i8()` — the session's plan slug (the plan file's stem).
    pub slug: String,
    /// `ZUe()` — whether the `.workshop.md` sibling participates. Always
    /// `false` in LingXi; see the module docs.
    pub workshop_enabled: bool,
}

impl PlanFileIdentity {
    /// `ay(void 0)` — `<plansDir>/<slug>.md`, the main session's plan file.
    #[must_use]
    pub fn plan_file(&self) -> PathBuf {
        self.plans_dir.join(format!("{}.md", self.slug))
    }

    /// `ay(agentId)` — `<plansDir>/<slug>-agent-<agentId>.md`.
    #[must_use]
    pub fn agent_plan_file(&self, agent_id: &str) -> PathBuf {
        self.plans_dir
            .join(format!("{}-agent-{agent_id}.md", self.slug))
    }
}

/// Session-scoped holder for [`PlanFileIdentity`], shared (behind an `Arc`)
/// between the permission policy, the tool context and the host.
///
/// It starts UNSET and matches nothing, so a host that never publishes an
/// identity behaves exactly as before this carve-out existed. The slug is
/// re-publishable because the upstream slug is derived from the transcript and
/// can change within a session (`getSlugFromLog`).
#[derive(Debug, Default)]
pub struct PlanFileMatcher {
    identity: RwLock<Option<PlanFileIdentity>>,
}

impl PlanFileMatcher {
    /// An unset matcher: [`Self::matches`] is `false` for every path.
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }

    /// An already-published matcher, for hosts and tests that know the identity
    /// up front.
    #[must_use]
    pub fn with_identity(identity: PlanFileIdentity) -> Self {
        Self {
            identity: RwLock::new(Some(identity)),
        }
    }

    /// Publish (or replace) the session's plan-file identity.
    pub fn publish(&self, identity: PlanFileIdentity) {
        *self.identity.write().unwrap_or_else(|e| e.into_inner()) = Some(identity);
    }

    /// The current identity, if one has been published.
    #[must_use]
    pub fn identity(&self) -> Option<PlanFileIdentity> {
        self.identity
            .read()
            .unwrap_or_else(|e| e.into_inner())
            .clone()
    }

    /// `ay(agentId)` — the plan file this session (or one of its agents) owns.
    /// `None` until an identity is published.
    #[must_use]
    pub fn plan_file(&self, agent_id: Option<&str>) -> Option<PathBuf> {
        let identity = self.identity()?;
        Some(match agent_id {
            Some(id) => identity.agent_plan_file(id),
            None => identity.plan_file(),
        })
    }

    /// `Zl(path, {includeWorkshopDoc})`.
    ///
    /// `path` is compared after lexical normalisation only — no `canonicalize`,
    /// because the plan file may not exist yet (the first `Write` creates it),
    /// and because a filesystem round-trip inside a permission check is a
    /// TOCTOU surface. A relative path is resolved against `cwd` first, the way
    /// `je()` resolves before comparing.
    #[must_use]
    pub fn matches(&self, path: &Path, cwd: Option<&Path>, include_workshop_doc: bool) -> bool {
        let Some(identity) = self.identity() else {
            return false;
        };
        let resolved = resolve(path, cwd);
        // `dp(o)!==je(Ra())` — the file must sit DIRECTLY in the plans dir.
        if resolved.parent() != Some(normalize(&identity.plans_dir).as_path()) {
            return false;
        }
        let Some(name) = resolved.file_name().and_then(|n| n.to_str()) else {
            return false;
        };
        let slug = identity.slug.as_str();
        if name == format!("{slug}.md") {
            return true;
        }
        if include_workshop_doc
            && identity.workshop_enabled
            && name == format!("{slug}.workshop.md")
        {
            return true;
        }
        name.starts_with(&format!("{slug}-agent-")) && name.ends_with(".md")
    }
}

/// `je(e)` — absolutise against `cwd`, then normalise lexically.
fn resolve(path: &Path, cwd: Option<&Path>) -> PathBuf {
    if path.is_absolute() {
        return normalize(path);
    }
    match cwd {
        Some(base) => normalize(&base.join(path)),
        None => normalize(path),
    }
}

/// Lexical `.`/`..` collapse. Deliberately not `canonicalize`: see
/// [`PlanFileMatcher::matches`].
fn normalize(path: &Path) -> PathBuf {
    use std::path::Component;
    let mut out = PathBuf::new();
    for component in path.components() {
        match component {
            Component::CurDir => {}
            Component::ParentDir => {
                if !out.pop() {
                    out.push("..");
                }
            }
            other => out.push(other.as_os_str()),
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    fn matcher() -> PlanFileMatcher {
        PlanFileMatcher::with_identity(PlanFileIdentity {
            plans_dir: PathBuf::from("/home/u/.claude/plans"),
            slug: "abc-123".into(),
            workshop_enabled: false,
        })
    }

    #[test]
    fn an_unset_matcher_matches_nothing() {
        let m = PlanFileMatcher::new();
        assert!(!m.matches(Path::new("/home/u/.claude/plans/abc-123.md"), None, true));
        assert_eq!(m.plan_file(None), None);
    }

    #[test]
    fn the_session_plan_file_matches() {
        assert!(matcher().matches(Path::new("/home/u/.claude/plans/abc-123.md"), None, false));
    }

    #[test]
    fn an_agent_plan_file_matches() {
        assert!(matcher().matches(
            Path::new("/home/u/.claude/plans/abc-123-agent-a1.md"),
            None,
            false
        ));
    }

    #[test]
    fn another_sessions_plan_file_does_not_match() {
        assert!(!matcher().matches(Path::new("/home/u/.claude/plans/other-999.md"), None, true));
    }

    #[test]
    fn a_nested_path_under_the_plans_dir_does_not_match() {
        // `dp(o)!==je(Ra())` — direct children only.
        assert!(!matcher().matches(
            Path::new("/home/u/.claude/plans/nested/abc-123.md"),
            None,
            true
        ));
    }

    #[test]
    fn a_traversal_back_into_the_plans_dir_still_matches() {
        assert!(matcher().matches(
            Path::new("/home/u/.claude/plans/../plans/abc-123.md"),
            None,
            false
        ));
    }

    #[test]
    fn a_traversal_out_of_the_plans_dir_does_not_match() {
        assert!(!matcher().matches(Path::new("/home/u/.claude/plans/../abc-123.md"), None, true));
    }

    #[test]
    fn a_relative_path_resolves_against_cwd() {
        assert!(matcher().matches(
            Path::new("abc-123.md"),
            Some(Path::new("/home/u/.claude/plans")),
            false
        ));
    }

    #[test]
    fn the_workshop_sibling_needs_both_the_flag_and_the_skill() {
        let path = Path::new("/home/u/.claude/plans/abc-123.workshop.md");
        // LingXi ships no workshop skill ⇒ `ZUe()` is false ⇒ never matches.
        assert!(!matcher().matches(path, None, true));
        let with_skill = PlanFileMatcher::with_identity(PlanFileIdentity {
            plans_dir: PathBuf::from("/home/u/.claude/plans"),
            slug: "abc-123".into(),
            workshop_enabled: true,
        });
        assert!(with_skill.matches(path, None, true));
        // `includeWorkshopDoc:false` (a write outside plan mode) still refuses.
        assert!(!with_skill.matches(path, None, false));
    }

    #[test]
    fn plan_file_paths_follow_ay() {
        let m = matcher();
        assert_eq!(
            m.plan_file(None).unwrap(),
            PathBuf::from("/home/u/.claude/plans/abc-123.md")
        );
        assert_eq!(
            m.plan_file(Some("a1")).unwrap(),
            PathBuf::from("/home/u/.claude/plans/abc-123-agent-a1.md")
        );
    }

    #[test]
    fn publishing_replaces_the_identity() {
        let m = PlanFileMatcher::new();
        m.publish(PlanFileIdentity {
            plans_dir: PathBuf::from("/p"),
            slug: "one".into(),
            workshop_enabled: false,
        });
        assert!(m.matches(Path::new("/p/one.md"), None, false));
        m.publish(PlanFileIdentity {
            plans_dir: PathBuf::from("/p"),
            slug: "two".into(),
            workshop_enabled: false,
        });
        assert!(!m.matches(Path::new("/p/one.md"), None, false));
        assert!(m.matches(Path::new("/p/two.md"), None, false));
    }
}
