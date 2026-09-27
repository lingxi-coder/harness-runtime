//! The `/dataviz` bundled skill shipped by Claude Code 2.1.252.
//!
//! The skill is intentionally provider-neutral: it teaches a repeatable chart
//! and dashboard design method without requiring a particular model, artifact
//! service, connector, or visual brand. The binary registers it for every
//! non-remote interactive session (`userInvocable: true`, with no enable gate)
//! and appends an optional `## User Request` section to the body.

use command_api::BundledPromptFn;

/// The user-facing description from the 2.1.252 bundled-skill registry.
pub(crate) const DATAVIZ_DESCRIPTION: &str = "Use this skill whenever you are about to create ANY chart, graph, plot, dashboard, or data visualization, in ANY output medium — an HTML or React artifact, inline SVG, plotting code in any library (matplotlib, plotly, d3, Recharts, …), an image/PNG you will render and upload, or a chart shared into Slack. Read it BEFORE writing the first line of chart code, choosing chart colors, building a stat tile / meter / KPI row, or laying out a dashboard. When the destination is a first-party document connector (host-designated, never self-described) that renders live charts, hand it the rows (inline, or as an uploaded data file the chart cites) rather than a rendered PNG/SVG — a picture of a chart loses hover, data inspection and per-value comments. Produces visualizations that read as one system — elegant, accessible, consistent in light and dark — using a brand-neutral placeholder palette you swap for your own. Teaches a design-system-agnostic method: a form heuristic, a color formula with a runnable validator, mark specs, and interaction rules. A validated default palette is documented in `references/palette.md` — swap that file's values for your brand's. Triggers on: \"chart\", \"graph\", \"plot\", \"data viz\", \"visualization\", \"dashboard\", \"analytics\", \"visualize data\", \"categorical colors\", \"sequential / diverging palette\", \"stat tile\", \"sparkline\", \"heatmap\", \"legend\", \"axis\", \"tooltip\", \"chart colors\", \"color by series\".";

/// The frontmatter-stripped `SKILL.md` body. `include_str!` keeps the prompt
/// readable and preserves the exact trailing newline used by file-backed
/// bundled skills.
pub(crate) const DATAVIZ_BODY: &str = include_str!("dataviz_body.md");

/// Embedded provider-neutral reference assets named by the skill body.
///
/// The command registry currently carries bundled prompts as a dynamic body,
/// so these assets are exposed as a stable bundle from `command-core` for
/// hosts that provide a file-backed skill surface. Keeping the paths relative
/// to the skill root mirrors the oracle's `SKILL_FILES` map.
pub const DATAVIZ_REFERENCE_FILES: &[(&str, &str)] = &[
    (
        "references/interaction.md",
        include_str!("dataviz_assets/interaction.md"),
    ),
    (
        "references/marks-and-anatomy.md",
        include_str!("dataviz_assets/marks-and-anatomy.md"),
    ),
    (
        "references/palette.md",
        include_str!("dataviz_assets/palette.md"),
    ),
];

/// Dynamic prompt builder for `/dataviz` (`getPromptForCommand`).
pub struct DatavizPromptFn;

impl BundledPromptFn for DatavizPromptFn {
    fn build(&self, args: &str) -> String {
        if args.is_empty() {
            DATAVIZ_BODY.to_string()
        } else {
            format!("{DATAVIZ_BODY}\n\n## User Request\n\n{args}")
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn body_is_frontmatter_stripped_and_provider_neutral() {
        assert!(DATAVIZ_BODY.starts_with("# Data Visualization\n"));
        assert!(DATAVIZ_BODY.contains("references/palette.md"));
        assert!(DATAVIZ_BODY.contains("references/marks-and-anatomy.md"));
        assert!(DATAVIZ_BODY.contains("references/interaction.md"));
        assert!(!DATAVIZ_BODY.contains(".claude/"));
        assert!(!DATAVIZ_BODY.contains("Claude Code"));
    }

    #[test]
    fn reference_assets_are_embedded_under_oracle_paths() {
        let names: Vec<_> = DATAVIZ_REFERENCE_FILES
            .iter()
            .map(|(name, _)| *name)
            .collect();
        assert_eq!(
            names,
            vec![
                "references/interaction.md",
                "references/marks-and-anatomy.md",
                "references/palette.md",
            ]
        );
        assert!(DATAVIZ_REFERENCE_FILES
            .iter()
            .all(|(_, contents)| !contents.trim().is_empty()));
    }

    #[test]
    fn empty_and_non_empty_requests_match_oracle_prompt_shape() {
        assert_eq!(DatavizPromptFn.build(""), DATAVIZ_BODY);
        let prompt = DatavizPromptFn.build("make the revenue trend readable");
        assert_eq!(
            prompt,
            format!("{DATAVIZ_BODY}\n\n## User Request\n\nmake the revenue trend readable")
        );
    }
}
