//! Conditional skills: the ones that stay invisible until the session touches a
//! file they claim.
//!
//! A skill whose frontmatter carries `paths:` is NOT listed on load. It sits in
//! a separate bucket and becomes available the first time a touched path matches
//! one of its gitignore-style patterns (claude-code `lhr`,
//! `src_163219561.js` @4573642):
//!
//! ```js
//! let p = uhr.default().add(y8(d.paths, "skill_paths"));
//! for (let y of e) { let v = sCt(y) ? dhr(n, y) : y;
//!   if (!v || v.startsWith("..") || sCt(v)) continue;
//!   if (p.ignores(v)) { PR().dynamicSkills.set(MGe(d), d);
//!     PR().conditionalSkills.delete(o);
//!     PR().activatedConditionalSkillNames.add(o); … break } }
//! ```
//!
//! The move is one-way and once per skill: upstream deletes it from
//! `conditionalSkills` and records the name, so a later non-match cannot
//! withdraw a skill the model has already been shown.

use crate::model::Skill;
use std::collections::HashSet;
use std::path::{Path, PathBuf};

/// Which conditional skills this session has activated.
#[derive(Debug, Default, Clone)]
pub struct ConditionalSkills {
    activated: HashSet<String>,
}

/// Does this skill withhold itself until a path matches?
///
/// Upstream also requires `d.type === "prompt"`; a skill loaded from markdown
/// is that type, and the port has no non-prompt skill carrying frontmatter, so
/// the discriminator here is simply a non-empty `paths`.
#[must_use]
pub fn is_conditional(skill: &Skill) -> bool {
    skill
        .frontmatter
        .paths
        .as_ref()
        .is_some_and(|p| !p.is_empty())
}

impl ConditionalSkills {
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }

    /// May this skill be listed / invoked yet?
    ///
    /// Unconditional skills are always available; a conditional one only after
    /// [`Self::activate_for_paths`] has matched it.
    #[must_use]
    pub fn is_available(&self, skill: &Skill) -> bool {
        !is_conditional(skill) || self.activated.contains(&skill.name)
    }

    /// Whether `name` has been activated (upstream
    /// `activatedConditionalSkillNames`).
    #[must_use]
    pub fn is_activated(&self, name: &str) -> bool {
        self.activated.contains(name)
    }

    /// Availability for a listing built from records that are not [`Skill`]s —
    /// the model-facing listing comes from command records, which carry the same
    /// `paths` patterns but not the skill type.
    ///
    /// Returns whether `name` may be listed now, activating it if one of
    /// `touched` matches. Empty `patterns` means unconditional. The activation
    /// is recorded, so it survives a later turn whose touched set no longer
    /// contains the matching file — the model has already been shown the skill.
    pub fn is_available_named(
        &mut self,
        name: &str,
        patterns: &[String],
        touched: &[PathBuf],
        root: &Path,
    ) -> bool {
        if patterns.is_empty() || self.activated.contains(name) {
            return true;
        }
        let matcher = build_matcher(root, patterns);
        for path in touched {
            let Some(rel) = relative_within(root, path) else {
                continue;
            };
            if matcher.matched_path_or_any_parents(&rel, false).is_ignore() {
                self.activated.insert(name.to_string());
                tracing::info!(
                    "[skills] Activated conditional skill '{}' (matched path: {})",
                    name,
                    rel.display()
                );
                return true;
            }
        }
        false
    }

    /// Offer `touched` paths to every still-dormant conditional skill in
    /// `skills`, returning the names activated by this call (in the order they
    /// matched), so the caller can log and count them.
    ///
    /// `root` anchors relative matching; a path that escapes it is skipped
    /// rather than matched, mirroring upstream's `v.startsWith("..")` guard.
    pub fn activate_for_paths(
        &mut self,
        skills: &[&Skill],
        touched: &[PathBuf],
        root: &Path,
    ) -> Vec<String> {
        let mut newly = Vec::new();
        for skill in skills {
            if !is_conditional(skill) || self.activated.contains(&skill.name) {
                continue;
            }
            let Some(patterns) = skill.frontmatter.paths.as_ref() else {
                continue;
            };
            let matcher = build_matcher(root, patterns);
            for path in touched {
                let Some(rel) = relative_within(root, path) else {
                    continue;
                };
                // `matched_path_or_any_parents`, not `matched`: a directory
                // pattern like `docs/` is meant to claim everything beneath it,
                // and plain `matched` only ever tests the final component.
                if matcher.matched_path_or_any_parents(&rel, false).is_ignore() {
                    self.activated.insert(skill.name.clone());
                    newly.push(skill.name.clone());
                    tracing::info!(
                        "[skills] Activated conditional skill '{}' (matched path: {})",
                        skill.name,
                        rel.display()
                    );
                    break;
                }
            }
        }
        newly
    }
}

/// The path as seen from `root`, or `None` when it escapes — a file outside the
/// workspace must not activate a skill scoped to it.
///
/// Upstream is `v = sCt(y) ? dhr(n, y) : y` followed by
/// `if (!v || v.startsWith("..") || sCt(v)) continue` — an absolute path is made
/// relative, and anything STILL absolute (or climbing out) is skipped. An
/// absolute path that is not under `root` cannot be made relative to it, so it
/// is skipped rather than matched as-is: treating it as a bare filename is how
/// `/elsewhere/secret.rs` would activate a skill scoped to this repo.
fn relative_within(root: &Path, path: &Path) -> Option<PathBuf> {
    let rel = if path.is_absolute() {
        path.strip_prefix(root).ok()?.to_path_buf()
    } else {
        path.to_path_buf()
    };
    if rel.as_os_str().is_empty() || rel.starts_with("..") || rel.is_absolute() {
        return None;
    }
    Some(rel)
}

fn build_matcher(root: &Path, patterns: &[String]) -> ignore::gitignore::Gitignore {
    let mut builder = ignore::gitignore::GitignoreBuilder::new(root);
    for pattern in patterns {
        if pattern.is_empty() {
            continue;
        }
        if let Err(error) = builder.add_line(None, pattern) {
            tracing::warn!(
                pattern = %pattern,
                error = %error,
                "[skill_paths] gitignore-style pattern is unusable; treating it as matching nothing"
            );
            telemetry::emit_uncompilable_ignore_pattern(
                telemetry::tengu::ignore_pattern::SITE_SKILL_PATHS,
            );
        }
    }
    builder
        .build()
        .unwrap_or_else(|_| ignore::gitignore::Gitignore::empty())
}

#[cfg(test)]
mod tests {
    use super::{is_conditional, ConditionalSkills};
    use crate::model::{LoadedFrom, Skill, SkillFrontmatter, SkillSource};
    use std::path::{Path, PathBuf};

    fn skill(name: &str, paths: Option<Vec<&str>>) -> Skill {
        Skill {
            name: name.to_string(),
            description: "d".into(),
            frontmatter: SkillFrontmatter {
                name: name.to_string(),
                description: "d".into(),
                paths: paths.map(|p| p.into_iter().map(str::to_string).collect()),
                ..SkillFrontmatter::default()
            },
            content: String::new(),
            source: SkillSource::Settings(protocol::SettingsScope::User),
            loaded_from: LoadedFrom::Skills,
            plugin_id: None,
            file_path: PathBuf::from("/work/repo/.lingxi/skills/x/SKILL.md"),
        }
    }

    #[test]
    fn a_skill_without_paths_is_always_available() {
        let s = skill("plain", None);
        assert!(!is_conditional(&s));
        assert!(ConditionalSkills::new().is_available(&s));
        // An EMPTY list is not conditional either — upstream's guard is
        // `!d.paths || d.paths.length === 0`.
        let empty = skill("empty", Some(vec![]));
        assert!(!is_conditional(&empty));
        assert!(ConditionalSkills::new().is_available(&empty));
    }

    #[test]
    fn a_conditional_skill_is_withheld_until_a_path_matches() {
        let s = skill("rust", Some(vec!["*.rs"]));
        let mut state = ConditionalSkills::new();
        assert!(is_conditional(&s));
        assert!(
            !state.is_available(&s),
            "a conditional skill is not listed on load"
        );

        let root = Path::new("/work/repo");
        assert!(
            state
                .activate_for_paths(&[&s], &[PathBuf::from("/work/repo/README.md")], root)
                .is_empty(),
            "an unrelated file must not activate it"
        );
        assert!(!state.is_available(&s));

        let activated =
            state.activate_for_paths(&[&s], &[PathBuf::from("/work/repo/src/main.rs")], root);
        assert_eq!(activated, vec!["rust".to_string()]);
        assert!(state.is_available(&s));
    }

    /// One-way and once: upstream deletes the entry and records the name, so a
    /// later non-matching turn cannot withdraw a skill the model has been shown.
    #[test]
    fn activation_is_permanent_and_not_repeated() {
        let s = skill("rust", Some(vec!["*.rs"]));
        let mut state = ConditionalSkills::new();
        let root = Path::new("/work/repo");
        let first = state.activate_for_paths(&[&s], &[PathBuf::from("/work/repo/a.rs")], root);
        assert_eq!(first.len(), 1);

        // Matching again reports nothing new…
        let again = state.activate_for_paths(&[&s], &[PathBuf::from("/work/repo/b.rs")], root);
        assert!(again.is_empty(), "already activated ⇒ not reported twice");
        // …and a turn touching nothing relevant does not take it away.
        let _ = state.activate_for_paths(&[&s], &[PathBuf::from("/work/repo/x.md")], root);
        assert!(state.is_available(&s));
    }

    /// `v.startsWith("..")` — a file outside the workspace must not activate a
    /// skill scoped to it.
    #[test]
    fn a_path_escaping_the_root_does_not_activate() {
        let s = skill("rust", Some(vec!["*.rs"]));
        let mut state = ConditionalSkills::new();
        assert!(state
            .activate_for_paths(
                &[&s],
                &[PathBuf::from("/elsewhere/../secret.rs")],
                Path::new("/work/repo"),
            )
            .is_empty());
        assert!(!state.is_available(&s));
    }

    /// The listing-facing entry point: the same rules, keyed by name for
    /// records that are commands rather than `Skill`s.
    #[test]
    fn the_named_form_withholds_then_remembers() {
        let mut state = ConditionalSkills::new();
        let root = Path::new("/work/repo");
        let patterns = vec!["*.rs".to_string()];

        assert!(
            state.is_available_named("rust", &[], &[], root),
            "no patterns ⇒ unconditional"
        );
        assert!(!state.is_available_named("rust", &patterns, &[], root));
        assert!(state.is_available_named(
            "rust",
            &patterns,
            &[PathBuf::from("/work/repo/src/a.rs")],
            root
        ));
        // The read-state map is an LRU, so the matching path can age out of the
        // touched set. The skill must stay listed anyway.
        assert!(
            state.is_available_named("rust", &patterns, &[], root),
            "activation is remembered, not recomputed from the current touched set"
        );
    }

    /// Gitignore semantics, not plain globbing: a directory pattern matches
    /// files beneath it.
    #[test]
    fn patterns_use_gitignore_semantics() {
        let s = skill("docs", Some(vec!["docs/"]));
        let mut state = ConditionalSkills::new();
        let activated = state.activate_for_paths(
            &[&s],
            &[PathBuf::from("/work/repo/docs/deep/page.md")],
            Path::new("/work/repo"),
        );
        assert_eq!(activated, vec!["docs".to_string()]);
    }
}
