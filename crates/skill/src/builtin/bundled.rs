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
        name: "create-bot",
        raw: include_str!("create-bot/SKILL.md"),
        triggers: &[
            "/create-bot",
            "create a bot",
            "创建bot",
            "创建 bot",
            "新建 Bot",
        ],
        references: &[],
    },
    BundledSkill {
        name: "artifact-design",
        raw: include_str!("artifact-design/SKILL.md"),
        triggers: &[
            "/artifact-design",
            "publish an artifact",
            "hosted web page",
            "发布成网页",
        ],
        references: &[],
    },
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
        triggers: &["lingxi-api", "lingxi", branding::PRODUCT_NAME_ZH],
        references: &[],
    },
    BundledSkill {
        name: "plan-artifact",
        raw: include_str!("plan-artifact/SKILL.md"),
        triggers: &[
            "/plan-artifact",
            "plan as an artifact",
            "publish the plan as a page",
            "把计划发布成页面",
        ],
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
