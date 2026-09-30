//! Parity fixture: `find_relevant` ranking determinism.
//!
//! Locks fixed-point u64 scoring (jaccard × age × tier × `team_boost`) against
//! claude-code's reference. Each scenario is platform-independent: we
//! construct `MemoryEntry` literals directly with explicit `age_days` so
//! filesystem mtime drift can't affect the assertion. Drift in any factor
//! manifests as an `expected_order` mismatch with the exact failing scenario.

use lingxi_core::types::{MemoryEntry, MemoryEntryTier};
use memory::memdir::find::{find_relevant, RelevanceInputs};
use serde::Deserialize;
use test_harness::parity::load_fixture;

#[derive(Deserialize)]
struct Fixture {
    scenarios: Vec<Scenario>,
}

#[derive(Deserialize)]
struct Scenario {
    name: String,
    prompt: String,
    k: Option<usize>,
    team_boost_enabled: bool,
    entries: Vec<FixtureEntry>,
    expected_order: Vec<String>,
}

#[derive(Deserialize)]
struct FixtureEntry {
    path: String,
    tier: String,
    body: String,
    age_days: u64,
    size_bytes: u64,
}

fn parse_tier(s: &str) -> MemoryEntryTier {
    match s {
        "Session" => MemoryEntryTier::Session,
        "Project" => MemoryEntryTier::Project,
        "Team" => MemoryEntryTier::Team,
        "User" => MemoryEntryTier::User,
        other => panic!("unknown tier {other:?}"),
    }
}

#[test]
fn memory_relevance_matches_claude_code() {
    let fx: Fixture = load_fixture("memory_relevance");
    for sc in &fx.scenarios {
        let entries: Vec<MemoryEntry> = sc
            .entries
            .iter()
            .map(|e| MemoryEntry {
                path: e.path.clone().into(),
                tier: parse_tier(&e.tier),
                body: e.body.clone(),
                age_days: e.age_days,
                size_bytes: e.size_bytes,
            })
            .collect();
        let out = find_relevant(
            &entries,
            &RelevanceInputs {
                prompt: &sc.prompt,
                k: sc.k,
                team_boost_enabled: sc.team_boost_enabled,
            },
        );
        let got: Vec<String> = out
            .iter()
            .map(|e| e.path.to_string_lossy().to_string())
            .collect();
        assert_eq!(
            got, sc.expected_order,
            "scenario {}: ranking must match claude-code byte-for-byte",
            sc.name
        );
    }
}

#[test]
fn ranking_is_byte_identical_across_repeated_runs() {
    // Same fixture, two passes — output must be identical (no clock or
    // hash-randomization influence).
    let fx: Fixture = load_fixture("memory_relevance");
    let sc = fx
        .scenarios
        .iter()
        .find(|s| s.name == "age_penalty_shifts_ordering")
        .expect("age_penalty_shifts_ordering scenario present");
    let entries: Vec<MemoryEntry> = sc
        .entries
        .iter()
        .map(|e| MemoryEntry {
            path: e.path.clone().into(),
            tier: parse_tier(&e.tier),
            body: e.body.clone(),
            age_days: e.age_days,
            size_bytes: e.size_bytes,
        })
        .collect();
    let mk_inputs = || RelevanceInputs {
        prompt: &sc.prompt,
        k: sc.k,
        team_boost_enabled: sc.team_boost_enabled,
    };
    let a = find_relevant(&entries, &mk_inputs());
    let b = find_relevant(&entries, &mk_inputs());
    let paths = |v: &[MemoryEntry]| {
        v.iter()
            .map(|e| e.path.to_string_lossy().to_string())
            .collect::<Vec<_>>()
    };
    assert_eq!(paths(&a), paths(&b));
}
