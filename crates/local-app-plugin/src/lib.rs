//! Where the checked-in Local App plugin tree is, for the consumers that need its files.
//!
//! The tree (`crates/plugins/lingxi-local-app`) is data, not Rust. A host that compiles it into
//! its binary (a build script packing the bundle) or validates it in a test needs a path, and a
//! relative path out of the consumer's own crate stops working the day the tree lives in another
//! repository. The path is resolved from this crate's manifest instead, so it follows whichever
//! checkout Cargo resolved this crate from, and a `[patch]` to a local clone moves it too.
//!
//! The schemas and workflow scripts the runtime embeds are `include_str!`ed here, next to the tree
//! they come from, so the consumer reads a constant instead of reaching for a file.

#![forbid(unsafe_code)]

use std::path::Path;

/// The plugin tree: `.lingxi-plugin/`, `skills/`, `agents/`, `workflows/`, `schemas/`, `assets/`.
#[must_use]
pub fn root() -> &'static Path {
    Path::new(concat!(
        env!("CARGO_MANIFEST_DIR"),
        "/../plugins/lingxi-local-app"
    ))
}

/// The declared file list of the tree: what a packer must find, and nothing else.
#[must_use]
pub fn inventory_path() -> &'static Path {
    Path::new(concat!(
        env!("CARGO_MANIFEST_DIR"),
        "/../runtime/builtin-plugin-inventory.txt"
    ))
}

/// The ids the plugin's workflows run under: the plugin's name, a colon, the script's name.
///
/// `local-app-build` has no script any more (the main session drives a build through the service's operations), but its id
/// stays in the list: a host that still receives it must refuse it, and prose that names it would send a model to a
/// workflow that does not exist. A host keeps its own registry of these ids and tests it against this list; a service tests
/// that its prose names none of them.
pub const WORKFLOW_IDS: [&str; 3] = [
    "lingxi-local-app:local-app-build",
    "lingxi-local-app:local-app-use-test",
    "lingxi-local-app:local-app-mcp-authoring",
];

/// JSON Schemas the plugin owns.
pub mod schemas {
    /// `authoring-spec.schema.json`.
    pub const AUTHORING_SPEC: &str =
        include_str!("../../plugins/lingxi-local-app/schemas/authoring-spec.schema.json");
    /// `qa-report.schema.json`.
    pub const QA_REPORT: &str =
        include_str!("../../plugins/lingxi-local-app/schemas/qa-report.schema.json");
    /// `use-test-report.schema.json`.
    pub const USE_TEST_REPORT: &str =
        include_str!("../../plugins/lingxi-local-app/schemas/use-test-report.schema.json");
    /// `mcp-proposal.schema.json`.
    pub const MCP_PROPOSAL: &str =
        include_str!("../../plugins/lingxi-local-app/schemas/mcp-proposal.schema.json");
    /// `workflow-agent-results.schema.json`.
    pub const WORKFLOW_AGENT_RESULTS: &str =
        include_str!("../../plugins/lingxi-local-app/schemas/workflow-agent-results.schema.json");
}

/// Workflow scripts the plugin ships.
pub mod workflows {
    /// `local-app-use-test.js`.
    pub const USE_TEST: &str =
        include_str!("../../plugins/lingxi-local-app/workflows/local-app-use-test.js");
    /// `local-app-mcp-authoring.js`.
    pub const MCP_AUTHORING: &str =
        include_str!("../../plugins/lingxi-local-app/workflows/local-app-mcp-authoring.js");
}

/// Checked-in examples of the contract that the host's own tests validate its workflow scripts
/// against. They are the fixtures of `local-apps`; they are embedded here, next to the plugin
/// they exercise, so a consumer reads a constant instead of reaching into another crate's
/// directory.
pub mod fixtures {
    /// An authoring spec the host accepts (canvas present, null).
    pub const AUTHORING_SPEC_VALID_NULL_CANVAS: &str =
        include_str!("../../local-apps/tests/fixtures/authoring-spec.valid-null-canvas.json");
    /// A QA result the host accepts.
    pub const QA_RESULT_VALID: &str =
        include_str!("../../local-apps/tests/fixtures/qa-result.valid.json");
    /// A QA result the host refuses: its verification scope is empty.
    pub const QA_RESULT_INVALID_EMPTY_SCOPE: &str =
        include_str!("../../local-apps/tests/fixtures/qa-result.invalid-empty-scope.json");
}

#[cfg(test)]
mod tests {
    use super::*;

    fn on_disk(relative_to_crate: &str) -> String {
        let path = Path::new(env!("CARGO_MANIFEST_DIR")).join(relative_to_crate);
        std::fs::read_to_string(&path).unwrap_or_else(|error| panic!("{}: {error}", path.display()))
    }

    #[test]
    fn the_tree_and_its_inventory_are_where_the_paths_say() {
        assert!(root().join(".lingxi-plugin/plugin.json").is_file());
        assert!(inventory_path().is_file());
    }

    /// The two workflows that have a script declare the id the list gives them.
    #[test]
    fn the_workflow_ids_are_the_ones_the_scripts_declare() {
        for (script, id) in [
            (workflows::USE_TEST, WORKFLOW_IDS[1]),
            (workflows::MCP_AUTHORING, WORKFLOW_IDS[2]),
        ] {
            assert!(
                script.contains(&format!("const WORKFLOW_ID = '{id}';")),
                "{id} is not what its script declares"
            );
        }
        assert_eq!(WORKFLOW_IDS.len(), 3);
        assert!(WORKFLOW_IDS
            .iter()
            .all(|id| id.starts_with("lingxi-local-app:")));
    }

    /// An `include_str!` that names the wrong (but existing) file would still compile; compare each
    /// constant with the file its name promises.
    #[test]
    fn every_embedded_constant_is_the_file_its_name_promises() {
        let plugin = "../plugins/lingxi-local-app";
        let fixtures = "../local-apps/tests/fixtures";
        for (embedded, path) in [
            (
                schemas::AUTHORING_SPEC,
                format!("{plugin}/schemas/authoring-spec.schema.json"),
            ),
            (
                schemas::QA_REPORT,
                format!("{plugin}/schemas/qa-report.schema.json"),
            ),
            (
                schemas::USE_TEST_REPORT,
                format!("{plugin}/schemas/use-test-report.schema.json"),
            ),
            (
                schemas::MCP_PROPOSAL,
                format!("{plugin}/schemas/mcp-proposal.schema.json"),
            ),
            (
                schemas::WORKFLOW_AGENT_RESULTS,
                format!("{plugin}/schemas/workflow-agent-results.schema.json"),
            ),
            (
                workflows::USE_TEST,
                format!("{plugin}/workflows/local-app-use-test.js"),
            ),
            (
                workflows::MCP_AUTHORING,
                format!("{plugin}/workflows/local-app-mcp-authoring.js"),
            ),
            (
                fixtures::AUTHORING_SPEC_VALID_NULL_CANVAS,
                format!("{fixtures}/authoring-spec.valid-null-canvas.json"),
            ),
            (
                fixtures::QA_RESULT_VALID,
                format!("{fixtures}/qa-result.valid.json"),
            ),
            (
                fixtures::QA_RESULT_INVALID_EMPTY_SCOPE,
                format!("{fixtures}/qa-result.invalid-empty-scope.json"),
            ),
        ] {
            assert_eq!(embedded, on_disk(&path), "{path}");
        }
    }
}
