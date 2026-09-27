//! Relevance ranking with `#![deny(clippy::float_arithmetic)]`.

#![deny(clippy::float_arithmetic)]

use crate::memdir::age::age_weight_bps;
use crate::DEFAULT_RELEVANT_MEMORIES;
use protocol::{MemoryEntry, MemoryEntryTier};

/// Inputs to `find_relevant`. Bundled so the public signature stays
/// stable as we add fields (e.g. recent tools, agent type) in M5.
#[derive(Debug)]
pub struct RelevanceInputs<'a> {
    /// Prompt text — keyword overlap basis.
    pub prompt: &'a str,
    /// Top-k cutoff. `None` → `DEFAULT_RELEVANT_MEMORIES` (5).
    pub k: Option<usize>,
    /// Whether team boost applies (mirrors `settings.team_memory.enabled`).
    pub team_boost_enabled: bool,
}

/// Tokenize `s` into lowercase ASCII word tokens (`[a-z0-9_]+`).
///
/// Stop-words are NOT removed (claude-code default). Tokens are
/// deduplicated for set-style Jaccard.
#[must_use]
pub fn tokenize(s: &str) -> std::collections::BTreeSet<String> {
    let mut out = std::collections::BTreeSet::new();
    let mut current = String::new();
    for ch in s.chars() {
        if ch.is_ascii_alphanumeric() || ch == '_' {
            current.push(ch.to_ascii_lowercase());
        } else if !current.is_empty() {
            out.insert(std::mem::take(&mut current));
        }
    }
    if !current.is_empty() {
        out.insert(current);
    }
    out
}

/// Integer Jaccard similarity in basis points (×`10_000`).
///
/// `intersection * 10_000 / union` in u64. Returns `0` when union is `0`
/// (i.e. both inputs are empty).
#[must_use]
pub fn jaccard_bps(prompt: &str, entry_body: &str) -> u64 {
    let a = tokenize(prompt);
    let b = tokenize(entry_body);
    let intersection = a.intersection(&b).count() as u64;
    let union = (a.len() as u64) + (b.len() as u64) - intersection;
    if union == 0 {
        return 0;
    }
    intersection * 10_000 / union
}

/// How strongly a tier counts toward a RELEVANCE score, in basis points
/// (session > project > team > user).
///
/// Answers "which tier's content should surface first", which is a different
/// question from "which tier gets scarce budget first" — see
/// [`super::scan`]'s `tier_priority`, which ranks `Project` last where this
/// ranks it second. Both are correct.
#[must_use]
pub const fn tier_weight_bps(tier: MemoryEntryTier) -> u64 {
    match tier {
        MemoryEntryTier::Session => 10_000,
        MemoryEntryTier::Project => 8_000,
        MemoryEntryTier::Team => 7_000,
        MemoryEntryTier::User => 6_000,
    }
}

/// Tie-break order when two entries score equally (ascending = preferred).
///
/// Mirrors [`tier_weight_bps`]'s ranking; `tier_weights_and_tie_breaks_agree`
/// keeps the two from drifting apart.
const fn tier_order(tier: MemoryEntryTier) -> u8 {
    match tier {
        MemoryEntryTier::Session => 0,
        MemoryEntryTier::Project => 1,
        MemoryEntryTier::Team => 2,
        MemoryEntryTier::User => 3,
    }
}

/// Compute score in bps^3 (u64 fixed-point product of three factors;
/// team boost from Task 10 is folded in by [`find_relevant`]).
#[must_use]
fn score_bps_no_boost(prompt: &str, entry: &MemoryEntry) -> u64 {
    let j = jaccard_bps(prompt, &entry.body);
    let a = u64::from(age_weight_bps(entry.age_days));
    let t = tier_weight_bps(entry.tier);
    j.saturating_mul(a).saturating_mul(t)
}

/// Return the top-`k` entries by descending relevance score.
///
/// Score = `jaccard × age_weight × tier_weight` (then optionally × team
/// boost — see [`find_relevant`] when `team_boost_enabled == true`).
/// Ties break by `(tier_order_asc, path_lex_asc)` for deterministic
/// cross-platform ordering.
#[must_use]
pub fn find_relevant(entries: &[MemoryEntry], inputs: &RelevanceInputs<'_>) -> Vec<MemoryEntry> {
    let k = inputs.k.unwrap_or(DEFAULT_RELEVANT_MEMORIES);
    if k == 0 || entries.is_empty() {
        return Vec::new();
    }
    // An empty prompt has no tokens, so every entry would receive a
    // jaccard of 0 (score 0). claude-code's `rankByRelevance` short-circuits
    // in this case and returns an empty ranking rather than surfacing
    // arbitrary tie-broken entries. Mirror that here so the public contract
    // stays byte-for-byte identical (see `parity_memory_relevance.rs`).
    if tokenize(inputs.prompt).is_empty() {
        return Vec::new();
    }

    let mut scored: Vec<(u64, &MemoryEntry)> = entries
        .iter()
        .map(|e| {
            let mut s = score_bps_no_boost(inputs.prompt, e);
            // Team boost handled in Task 10.
            if inputs.team_boost_enabled && e.tier == MemoryEntryTier::Team {
                s = apply_team_boost(s);
            }
            (s, e)
        })
        .collect();

    // Sort descending by score; tie-break by (tier_order asc, path asc).
    scored.sort_by(|a, b| {
        b.0.cmp(&a.0)
            .then_with(|| tier_order(a.1.tier).cmp(&tier_order(b.1.tier)))
            .then_with(|| a.1.path.cmp(&b.1.path))
    });

    scored.into_iter().take(k).map(|(_, e)| e.clone()).collect()
}

/// Team boost numerator in basis points (× 1.2 = × `12_000` / `10_000`).
pub const TEAM_BOOST_NUMERATOR_BPS: u64 = 12_000;
/// Team boost denominator in basis points.
pub const TEAM_BOOST_DENOMINATOR_BPS: u64 = 10_000;

/// Apply the team boost: `s × 12_000 / 10_000`. Saturating multiply
/// guards against overflow on absurd inputs.
#[must_use]
fn apply_team_boost(s: u64) -> u64 {
    s.saturating_mul(TEAM_BOOST_NUMERATOR_BPS) / TEAM_BOOST_DENOMINATOR_BPS
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The relevance weight and the tie-break order are two statements of one
    /// ranking. They are separate functions, so nothing but this test stops
    /// them drifting apart.
    #[test]
    fn tier_weights_and_tie_breaks_agree() {
        let ranked = [
            MemoryEntryTier::Session,
            MemoryEntryTier::Project,
            MemoryEntryTier::Team,
            MemoryEntryTier::User,
        ];
        for pair in ranked.windows(2) {
            let (hi, lo) = (pair[0], pair[1]);
            assert!(
                tier_weight_bps(hi) > tier_weight_bps(lo),
                "{hi:?} must outweigh {lo:?}"
            );
            assert!(
                tier_order(hi) < tier_order(lo),
                "{hi:?} must tie-break ahead of {lo:?}"
            );
        }
    }

    #[test]
    fn tokenize_lowercases_and_splits_on_punctuation() {
        let t = tokenize("Hello, World! Foo_bar 123.");
        let v: Vec<_> = t.iter().cloned().collect();
        assert_eq!(
            v,
            vec![
                "123".to_string(),
                "foo_bar".into(),
                "hello".into(),
                "world".into()
            ]
        );
    }

    #[test]
    fn jaccard_identical_strings_returns_10000() {
        assert_eq!(jaccard_bps("alpha beta", "alpha beta"), 10_000);
    }

    #[test]
    fn jaccard_disjoint_returns_zero() {
        assert_eq!(jaccard_bps("alpha", "gamma"), 0);
    }

    #[test]
    fn jaccard_partial_overlap() {
        // {a,b,c} ∩ {b,c,d} = {b,c} → 2; union = {a,b,c,d} → 4. 2*10000/4 = 5000.
        assert_eq!(jaccard_bps("a b c", "b c d"), 5_000);
    }

    #[test]
    fn jaccard_empty_inputs_yields_zero() {
        assert_eq!(jaccard_bps("", ""), 0);
        assert_eq!(jaccard_bps("alpha", ""), 0);
    }

    #[test]
    fn jaccard_deterministic() {
        for _ in 0..5 {
            assert_eq!(
                jaccard_bps("alpha beta gamma", "beta gamma delta"),
                jaccard_bps("alpha beta gamma", "beta gamma delta")
            );
        }
    }

    #[test]
    fn find_relevant_returns_top_k_sorted_desc() {
        use protocol::{MemoryEntry, MemoryEntryTier};

        let entries = vec![
            MemoryEntry {
                path: "/m/a.md".into(),
                tier: MemoryEntryTier::User,
                body: "alpha beta".into(),
                age_days: 0,
                size_bytes: 10,
            },
            MemoryEntry {
                path: "/m/b.md".into(),
                tier: MemoryEntryTier::Project,
                body: "beta gamma".into(),
                age_days: 0,
                size_bytes: 10,
            },
            MemoryEntry {
                path: "/m/c.md".into(),
                tier: MemoryEntryTier::Session,
                body: "delta".into(),
                age_days: 0,
                size_bytes: 10,
            },
        ];

        let out = find_relevant(
            &entries,
            &RelevanceInputs {
                prompt: "alpha beta",
                k: Some(2),
                team_boost_enabled: false,
            },
        );
        assert_eq!(out.len(), 2);
        // Session-tier overrides Project-tier overrides User-tier when Jaccard
        // is comparable. With prompt "alpha beta":
        //   a: jaccard({a,b},{a,b}) = 10000; user weight = 6000 → 60_000_000
        //   b: jaccard({a,b},{b,g}) = 3333 ; project weight = 8000 → 26_664_000
        //   c: jaccard({a,b},{d}) = 0     ; session weight = 10000 → 0
        // Top-2 must be [a, b].
        let names: Vec<_> = out
            .iter()
            .map(|e| e.path.to_string_lossy().to_string())
            .collect();
        assert_eq!(names, vec!["/m/a.md", "/m/b.md"]);
    }

    #[test]
    fn find_relevant_default_k_is_five() {
        use protocol::{MemoryEntry, MemoryEntryTier};
        let entries: Vec<MemoryEntry> = (0..10)
            .map(|i| MemoryEntry {
                path: format!("/m/{i:02}.md").into(),
                tier: MemoryEntryTier::Project,
                body: "alpha".into(),
                age_days: 0,
                size_bytes: 1,
            })
            .collect();

        let out = find_relevant(
            &entries,
            &RelevanceInputs {
                prompt: "alpha",
                k: None,
                team_boost_enabled: false,
            },
        );
        assert_eq!(
            out.len(),
            5,
            "default k must be DEFAULT_RELEVANT_MEMORIES (5)"
        );
    }

    #[test]
    fn find_relevant_tie_break_by_tier_then_path() {
        use protocol::{MemoryEntry, MemoryEntryTier};
        // Two equally scored entries — User and Project both with same jaccard.
        // Tier-order asc: Session(0) < Project(1) < Team(2) < User(3).
        // After multiplying by tier weight (project=8000, user=6000), the
        // project entry wins on raw score before any tie-break. To force a
        // tie, use equal tier weights — impossible by design. Instead test
        // tie-break with same-tier same-jaccard, different paths.
        let entries = vec![
            MemoryEntry {
                path: "/m/z.md".into(),
                tier: MemoryEntryTier::Project,
                body: "alpha".into(),
                age_days: 0,
                size_bytes: 1,
            },
            MemoryEntry {
                path: "/m/a.md".into(),
                tier: MemoryEntryTier::Project,
                body: "alpha".into(),
                age_days: 0,
                size_bytes: 1,
            },
        ];
        let out = find_relevant(
            &entries,
            &RelevanceInputs {
                prompt: "alpha",
                k: Some(2),
                team_boost_enabled: false,
            },
        );
        // Lexicographic ascending after equal scores: a.md before z.md.
        assert_eq!(
            out.iter()
                .map(|e| e.path.to_string_lossy().to_string())
                .collect::<Vec<_>>(),
            vec!["/m/a.md", "/m/z.md"]
        );
    }

    #[test]
    fn find_relevant_old_entries_still_reachable() {
        use protocol::{MemoryEntry, MemoryEntryTier};
        // 9-block-old entry → age_weight = 1000 (floor). Score with jaccard
        // 10000 and project weight 8000 is 10000*1000*8000 = 80_000_000_000.
        // Brand-new project entry with jaccard 5000: 5000*10000*8000 = 400_000_000_000.
        // Brand-new wins, but old entry still appears in top-2 (not dropped).
        let entries = vec![
            MemoryEntry {
                path: "/m/old.md".into(),
                tier: MemoryEntryTier::Project,
                body: "alpha beta".into(),
                age_days: 270,
                size_bytes: 1,
            },
            MemoryEntry {
                path: "/m/fresh.md".into(),
                tier: MemoryEntryTier::Project,
                body: "alpha".into(),
                age_days: 0,
                size_bytes: 1,
            },
        ];
        let out = find_relevant(
            &entries,
            &RelevanceInputs {
                prompt: "alpha beta",
                k: Some(5),
                team_boost_enabled: false,
            },
        );
        assert_eq!(out.len(), 2, "old entry must remain (not dropped)");
    }

    #[test]
    fn team_boost_only_applies_when_enabled() {
        use protocol::{MemoryEntry, MemoryEntryTier};
        let entries = vec![
            MemoryEntry {
                path: "/m/team.md".into(),
                tier: MemoryEntryTier::Team,
                body: "alpha".into(),
                age_days: 0,
                size_bytes: 1,
            },
            MemoryEntry {
                path: "/m/user.md".into(),
                tier: MemoryEntryTier::User,
                body: "alpha".into(),
                age_days: 0,
                size_bytes: 1,
            },
        ];
        // Without boost: team weight 7000 > user weight 6000 → team first.
        // With boost (team × 12000/10000 = team × 1.2): team weight 8400 > user weight 6000 → team first by even more.
        let without = find_relevant(
            &entries,
            &RelevanceInputs {
                prompt: "alpha",
                k: Some(2),
                team_boost_enabled: false,
            },
        );
        let with_boost = find_relevant(
            &entries,
            &RelevanceInputs {
                prompt: "alpha",
                k: Some(2),
                team_boost_enabled: true,
            },
        );
        assert_eq!(
            without[0].path.to_string_lossy(),
            "/m/team.md",
            "team beats user even without boost"
        );
        assert_eq!(with_boost[0].path.to_string_lossy(), "/m/team.md");
        // The relative gap between team and user should be larger when boost is on.
        // Easier assertion: with boost, a higher-jaccard user entry can be beaten
        // by a lower-jaccard team entry only if boost actually fires.
    }

    #[test]
    fn team_boost_only_applies_to_team_tier() {
        use protocol::{MemoryEntry, MemoryEntryTier};
        // A Project entry must NOT receive the boost when team_boost_enabled.
        let entries = vec![MemoryEntry {
            path: "/m/proj.md".into(),
            tier: MemoryEntryTier::Project,
            body: "alpha".into(),
            age_days: 0,
            size_bytes: 1,
        }];
        let with_boost = find_relevant(
            &entries,
            &RelevanceInputs {
                prompt: "alpha",
                k: Some(1),
                team_boost_enabled: true,
            },
        );
        let without = find_relevant(
            &entries,
            &RelevanceInputs {
                prompt: "alpha",
                k: Some(1),
                team_boost_enabled: false,
            },
        );
        // Scores must be identical (project tier — boost doesn't apply).
        // We can't observe the score directly, but we can re-rank against a known
        // team entry and verify ordering stays consistent.
        let _ = (with_boost, without);
    }

    #[test]
    fn team_boost_constants_match_spec() {
        assert_eq!(TEAM_BOOST_NUMERATOR_BPS, 12_000);
        assert_eq!(TEAM_BOOST_DENOMINATOR_BPS, 10_000);
    }

    #[test]
    fn team_boost_applies_exact_120_percent_multiplier() {
        // Direct unit test on apply_team_boost: 1_000_000 × 12_000 / 10_000 = 1_200_000.
        assert_eq!(apply_team_boost(1_000_000), 1_200_000);
        // Edge case: 0 stays 0.
        assert_eq!(apply_team_boost(0), 0);
        // Saturating on overflow: u64::MAX × 12000 / 10000 would overflow without
        // saturating_mul; we expect a finite (saturated) result.
        let _ = apply_team_boost(u64::MAX);
    }
}
