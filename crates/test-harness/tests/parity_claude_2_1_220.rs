//! Contract fixtures captured from the Claude Code 2.1.220 oracle.
//!
//! These are release captures and keep their capture version in the name: each
//! asserts what 2.1.220 said, and that does not move when the port aligns
//! forward. The prompt byte locks that used to share this file are NOT of that
//! kind — they hash this port's own output and have been re-based through
//! 2.1.238, 2.1.263 and 2.1.267 — so they now live in
//! `parity_prompt_byte_locks.rs` under a name that cannot go stale.

use std::collections::BTreeSet;

use serde_json::Value;

const BACKLOG_CONTRACTS: &str =
    include_str!("../src/parity/fixtures/claude_2_1_220_backlog_contracts.json");
const GAP_ORACLE: &str = include_str!("../src/parity/fixtures/claude_2_1_220_gap_oracle.json");

fn backlog_contracts() -> Value {
    serde_json::from_str(BACKLOG_CONTRACTS).expect("2.1.220 backlog fixture must be valid JSON")
}

fn gap_oracle() -> Value {
    serde_json::from_str(GAP_ORACLE).expect("2.1.220 gap oracle fixture must be valid JSON")
}

/// Wave 0 maps every approved engineering item to exactly one implementation
/// wave. This is inventory validation only: a fixture's `target` field is not
/// accepted as proof that the corresponding production behavior is complete.
/// The private remote-memory protocol is deliberately tracked outside this
/// list as a single explicit divergence.
#[test]
fn approved_backlog_inventory_has_25_unique_items() {
    let fixture = backlog_contracts();
    let items = fixture["items"].as_array().expect("items array");
    assert_eq!(items.len(), 25);

    let ids = items
        .iter()
        .map(|item| item["id"].as_str().expect("item id"))
        .collect::<BTreeSet<_>>();
    assert_eq!(ids.len(), 25, "backlog IDs must be unique");
    assert!(
        items
            .iter()
            .all(|item| matches!(item["wave"].as_u64(), Some(1..=4))),
        "every item must be assigned to Wave 1-4"
    );
}

#[test]
fn private_remote_memory_is_one_explicit_divergence() {
    let fixture = backlog_contracts();
    let divergences = fixture["divergences"]
        .as_array()
        .expect("divergences array");
    assert_eq!(divergences.len(), 1);
    assert_eq!(divergences[0]["id"], "N-env-3/N-protocol-8");
    assert!(divergences[0]["reason"]
        .as_str()
        .expect("divergence reason")
        .contains("private account remote-memory"));
}

/// Clean-room oracle inventory for stateful behavior. This pins the oracle
/// itself; production behavior is proved by the subsystem tests that execute
/// each state machine, not by these fixture values.
#[test]
fn stateful_2_1_220_oracle_inventory_is_pinned() {
    let fixture = backlog_contracts();
    let contracts = &fixture["pinned_contracts"];

    assert_eq!(
        contracts["ultracode"]["transitions"],
        serde_json::json!(["enter", "sparse", "exit"])
    );
    assert_eq!(contracts["ultracode"]["default_sparse_cadence"], 10);
    assert_eq!(contracts["observer"]["default_max_depth"], 3);
    assert_eq!(
        contracts["deep_research"]["stages"],
        serde_json::json!(["Scope", "Search", "Fetch", "Verify", "Synthesize"])
    );
    assert_eq!(contracts["deep_research"]["votes_per_claim"], 3);
    assert_eq!(contracts["deep_research"]["max_fetch"], 15);
    assert_eq!(
        contracts["opus_5_bash_addition"],
        "Command output is displayed to you, not reliably to the user."
    );
    assert_eq!(contracts["attached_left_arrow"]["outcome"], "detach");
    assert_eq!(
        contracts["accessibility"]["announces_edit_delta_only"],
        true
    );
}

/// Wave 0 retains only clean-room observables from the local 2.1.220 binary:
/// hashes/lengths/section names rather than the private prompt bodies.
#[test]
fn gap_oracle_is_pinned_without_private_prompt_text() {
    let fixture = gap_oracle();
    assert_eq!(fixture["oracle"]["version"], "2.1.220");
    assert_eq!(
        fixture["oracle"]["binary_sha256"],
        "8addc857f3fe64d5a0368af9ee50321b50afb4a6918ba3ef018ab84f5dbbe081"
    );
    assert_eq!(fixture["oracle"]["network"], "loopback-only");
    // Read from the binary this fixture pins (@225785080), not from a
    // changelog for another version — see the entry's `supersedes` note.
    assert_eq!(fixture["fast_mode"]["claude-opus-4-7"], "on");

    let prompts = fixture["system_prompt_manifests"]
        .as_array()
        .expect("system prompt manifests");
    assert_eq!(prompts.len(), 4);
    assert!(prompts.iter().all(|entry| {
        entry["length_bytes"]
            .as_u64()
            .is_some_and(|len| len > 1_000)
            && entry["sha256"]
                .as_str()
                .is_some_and(|digest| digest.len() == 64)
            && entry["headings"]
                .as_array()
                .is_some_and(|headings| !headings.is_empty())
    }));
    assert_ne!(
        prompts[0]["sha256"], prompts[1]["sha256"],
        "Claude model families must retain distinct prompt manifests"
    );
    assert_ne!(
        prompts[0]["sha256"], prompts[3]["sha256"],
        "the lean Opus prompt must not be treated as the Sonnet prompt"
    );

    let options = fixture["plugin_eval"]["options"]
        .as_array()
        .expect("plugin eval options");
    assert!(options.iter().any(|option| option == "--json"));
    assert!(options.iter().any(|option| option == "--model"));
    assert_eq!(fixture["plugin_eval"]["help_exit_code"], 0);
    assert_eq!(
        fixture["plugin_eval"]["early_access_gate"],
        "CLAUDE_CODE_WALNUT_SPIRE"
    );
    assert_eq!(
        fixture["plugin_eval"]["bare_template"]["prompt_frontmatter"]["max_turns"],
        10
    );
    assert_eq!(
        fixture["plugin_eval"]["bare_template"]["grader_frontmatter"]["type"],
        "llm"
    );
    assert_eq!(fixture["plugin_eval"]["empty_suite"]["exit_code"], 1);
    assert_eq!(fixture["plugin_eval"]["empty_suite"]["schema_version"], 1);
}
