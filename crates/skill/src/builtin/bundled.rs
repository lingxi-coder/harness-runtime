//! Compiled-in builtin skill templates.
//!
//! Shared prompt sources for the builtin Skill and slash-command registries.
//! Product guidance also reaches mobile through the shared bundled commands;
//! device-specific skills remain with their owning runtime/plugin components.

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
pub(crate) const BUILTIN_DESKTOP: &[BundledSkill] = &[
    BundledSkill {
        name: "document-authoring",
        raw: include_str!("document-authoring/SKILL.md"),
        triggers: &[
            "/document-authoring",
            "create a document",
            "edit a document",
            "创建文档",
            "修改文档",
        ],
        references: &[],
    },
    BundledSkill {
        name: "lingxi-api",
        raw: include_str!("lingxi-api/SKILL.md"),
        triggers: &["lingxi-api", "lingxi", "灵犀"],
        references: &[],
    },
    BundledSkill {
        name: "plugin-authoring",
        raw: include_str!("plugin-authoring.md"),
        triggers: &[
            "plugin-authoring",
            "write a LingXi plugin",
            "programmatic hooks",
            "hooks module",
        ],
        references: &[],
    },
];
