//! Compiled-in builtin skill templates.
//!
//! The mobile product's skills are file-backed Plugin components. Keeping only the
//! unrelated desktop catalog here prevents a second mobile body/reference
//! source from drifting away from the plugin tree.

/// A compiled-in skill template and its registry-only discovery phrases.
pub(crate) struct BundledSkill {
    pub(crate) name: &'static str,
    pub(crate) raw: &'static str,
    pub(crate) triggers: &'static [&'static str],
    pub(crate) references: &'static [BundledResource],
}

/// A markdown reference for a compiled-in skill.
pub(crate) struct BundledResource {
    pub(crate) path: &'static str,
    pub(crate) content: &'static str,
}

/// Desktop builtin skill templates.
pub(crate) const BUILTIN_DESKTOP: &[BundledSkill] = &[BundledSkill {
    name: "claude-api",
    raw: include_str!("claude-api.md"),
    triggers: &[
        "claude-api",
        "/claude-api",
        "/claude-api upgrade python",
        "claude api upgrade python",
        "anthropic sdk migration",
    ],
    references: &[],
}];

/// Mobile has no bundled skills of its own. They are registered from the
/// verified Plugin package by the mobile composition root.
pub(crate) const BUILTIN_MOBILE: &[BundledSkill] = &[];
