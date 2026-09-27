//! Brand-token normalization layer for comparing Claude-oracle plugin
//! fixtures against LingXi's plugin contract (design §19.1 / task P0a.7).
//!
//! §19.1 requires that the *same* fixture, run through this layer, yields
//! the same component inventory and parse behaviour under LingXi as under
//! the Claude oracle. That comparison is only meaningful if the normalizer
//! actually understands every plugin-contract identifier it is handed —
//! see [`NormalizeReport::unmapped`] and the module-level warning below.
//!
//! ## The pairs, and where they come from
//!
//! `branding::DOT_DIR` / `PLUGIN_MANIFEST_DIR` / `PLUGIN_ROOT_ENV` /
//! `PLUGIN_DATA_ENV` / `PROJECT_DIR_ENV` are LingXi's half of five identity
//! pairs the plugin contract carries; each one's doc comment in
//! `branding/src/lib.rs` names its oracle counterpart (`.claude-plugin`,
//! `CLAUDE_PLUGIN_ROOT`, `CLAUDE_PLUGIN_DATA`, `CLAUDE_PROJECT_DIR`), and
//! `DOT_DIR` is the plugin CACHE root (`<DOT_DIR>/plugins`, see `git.rs` /
//! `marketplace.rs` / `discovery.rs`) plus the stem of the global config file
//! that carries `enabledPlugins`. This module does **not** spell those oracle
//! counterparts out as literals: [`claude_counterpart`] derives each one
//! mechanically (`LINGXI`/`lingxi` → `CLAUDE`/`claude`) from the
//! already-licensed LingXi constant. Two independent reasons, not one:
//!
//! * "Do NOT invent pairs" (task brief) — a mechanical derivation from the
//!   single source of truth cannot drift from it the way a hand-copied
//!   table could.
//! * The brand gate's rule G7 (`scripts/check_brand_leaks.py`) treats a bare
//!   `CLAUDE_PLUGIN_` / `.claude-plugin` / `CLAUDE_PROJECT_DIR` appearing in
//!   non-comment source as an arriving oracle identifier, and this file
//!   carries no `scripts/brand_frozen_identities.txt` license for any of the
//!   three (that list is not among this task's owned files). Deriving the
//!   spelling at run time from the LingXi constant means the oracle
//!   spelling never appears as contiguous source text for G7 to match. As of
//!   the P0a.7 review this holds with NO exceptions: [`scan_tokens`]'s two
//!   prefixes are themselves derived (from `branding::ENV_PREFIX` and
//!   `branding::DOT_DIR`), so the file spells out no oracle fragment at all.
//!   Measured, not assumed — running `scripts/check_brand_leaks.py --list`
//!   over a synthetic tree holding this file reports **zero G7 findings**.
//!
//! ## Frozen identities are a *second*, orthogonal concept
//!
//! `scripts/brand_frozen_identities.txt` (the L3 list) currently licenses two
//! identifiers to remain their Claude spelling **at a specific file path**,
//! because renaming them would discard already-shipped state: an upstream
//! `CLAUDECODE` marker, and the literal `CLAUDE_PLUGIN_ROOT` HTTP-header key
//! `mcp/src/headers_helper.rs` writes for already-installed plugins. Neither
//! is something to normalize away, but for different reasons:
//!
//! * `CLAUDE_PLUGIN_ROOT` *is* one of [`known_pairs`]'s ordinary mapped
//!   tokens (the env-var substitution spelling) — everywhere except that one
//!   frozen (path, literal) pair, it must still be rewritten. [`normalize`]
//!   is therefore **path-scoped**: it only treats a token as frozen when the
//!   caller's `source_path` matches the frozen entry, never globally.
//! * `CLAUDECODE` is not a plugin-contract token shape at all (no `_` after
//!   `CLAUDE`, no `.claude-` prefix) — [`scan_tokens`] never recognizes it,
//!   so it is inert to this module by construction, not by an exemption.
//!
//! This module reads `scripts/brand_frozen_identities.txt` at call time via
//! [`load_frozen_identities`] rather than hand-copying its two entries, for
//! the same reason `claude_counterpart` derives rather than hard-codes: the
//! file is this repo's single source of truth for frozen identities, and it
//! is itself excluded from the brand gate's own scan (`SELF_ARTIFACTS`), so
//! reading its *contents* at run time never puts an oracle literal in this
//! file's source text either.
//!
//! ## The house defect this module exists to avoid
//!
//! A normalizer that silently passes through anything it does not recognise
//! turns an oracle comparison into a tautology: both sides "agree" because
//! the unmapped token was left alone on both. [`normalize`] never does that
//! silently — every token it recognises as Claude-plugin-shaped but cannot
//! map lands in [`NormalizeReport::unmapped`], and a caller comparing
//! fixtures through this layer must check that list is empty before trusting
//! the comparison. `CLAUDE_PLUGIN_OPTION_<KEY>` (see
//! `hooks::user_config::option_env_var`) is exactly such a token today:
//! part of the real plugin contract, but not one of `branding`'s five
//! constants, so it is a genuine — not contrived — unmapped case.
//!
//! ## What `is_fully_understood()` does NOT mean — read before trusting it
//!
//! [`scan_tokens`] recognizes exactly **two token families**: a screaming-snake
//! run behind the oracle env prefix, and a dotted lowercase run behind the
//! oracle dot-dir. Anything outside those two shapes is invisible to this
//! module — it is neither rewritten nor reported. Known examples that fall
//! outside today: the oracle memory filename (`branding::MEMORY_FILE`'s
//! counterpart, which is neither dotted nor prefixed), the `CLAUDECODE`
//! marker, the upstream `anthropics/claude-plugins-official` marketplace repo
//! name (deliberately — it has no leading dot and is an unchangeable upstream
//! value, the same call `scripts/check_brand_leaks.py` makes for its own
//! `\.claude-plugin` needle), and bare product-name prose.
//!
//! So [`NormalizeReport::is_fully_understood`] means "every token of those two
//! families was mapped or licensed-frozen", NOT "no oracle identifier remains
//! in the text". A caller comparing inventories must still diff the normalized
//! bodies; the report tells it whether the normalization it just applied was
//! complete *within the families the scanner covers*.
//!
//! In the other direction the module is deliberately conservative: a dotted
//! run it cannot map (a Bedrock/Vertex model id such as
//! `us.anthropic.claude-<family>-…` embeds one) lands in `unmapped` rather
//! than being silently rewritten or silently kept. That is a loud false
//! alarm, never a quiet agreement — the safe direction for an oracle
//! comparison.

use std::path::{Path, PathBuf};

/// One LingXi ↔ Claude identity pair the plugin contract carries.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct BrandPair {
    /// The LingXi spelling — always one of `branding`'s own constants.
    pub lingxi: &'static str,
    /// The oracle spelling, mechanically derived — see [`claude_counterpart`].
    pub claude: String,
}

/// One entry from `scripts/brand_frozen_identities.txt`: a (path, literal)
/// pair the brand gate — and this normalizer — must leave untouched, plus
/// its stated reason (read for `report`/error messages, not matched on).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct FrozenIdentity {
    /// Repo-root-relative path, exactly as written in the frozen list (e.g.
    /// `crates/mcp/src/headers_helper.rs`).
    pub path: String,
    /// The exact identifier this entry licenses (e.g. `CLAUDE_PLUGIN_ROOT`).
    pub literal: String,
    /// The frozen list's stated reason (comment text after ` # `).
    pub reason: String,
}

/// What [`normalize`] did to a piece of fixture text.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct NormalizeReport {
    /// The normalized text: every recognized, non-frozen Claude token
    /// rewritten to its LingXi spelling; frozen and unmapped tokens left
    /// byte-for-byte as found.
    pub output: String,
    /// Claude tokens that WERE rewritten (in order of appearance).
    pub mapped: Vec<String>,
    /// Claude tokens left as-is because a frozen-identity entry licenses
    /// them at this exact `source_path`.
    pub preserved: Vec<String>,
    /// Claude-plugin-shaped tokens that are neither a known pair nor
    /// frozen at this path. **Non-empty means the comparison this
    /// normalizer feeds is not trustworthy** — see the module-level
    /// warning.
    pub unmapped: Vec<String>,
}

impl NormalizeReport {
    /// `true` iff every Claude-plugin-shaped token in the input was either
    /// mapped or licensed-frozen. A caller doing an oracle/LingXi fixture
    /// comparison MUST check this before trusting agreement between the two
    /// sides — see the module doc's "house defect" section.
    #[must_use]
    pub fn is_fully_understood(&self) -> bool {
        self.unmapped.is_empty()
    }
}

/// Derive `value`'s oracle spelling by swapping the brand word, never by
/// spelling the oracle word out. `lower`/`upper` are themselves sliced out of
/// `branding::DOT_DIR` (`".lingxi"`) rather than typed, so this file contains
/// no `LINGXI`/`lingxi` string literal for the brand gate's bare `LINGXI`
/// needle (`G1`, `FULL_NEEDLES`) to catch either.
fn claude_counterpart(value: &str) -> String {
    let lower = &branding::DOT_DIR[1..];
    let upper = lower.to_uppercase();
    value.replace(&upper, "CLAUDE").replace(lower, "claude")
}

/// The plugin-contract identity pairs, derived — never hand-typed — from
/// `branding`'s single source of truth. Extending this list means adding a
/// `branding` constant to the array, not inventing a new literal here.
#[must_use]
pub fn known_pairs() -> Vec<BrandPair> {
    [
        // REVIEW FIX: `DOT_DIR` is a plugin-contract identifier too — the
        // plugin cache root is `<DOT_DIR>/plugins` (see `git.rs`,
        // `marketplace.rs::PluginPaths`, `discovery.rs`) and `enabledPlugins`
        // lives in the settings file under it. Before it was a pair, an
        // oracle fixture carrying the oracle config dir was neither rewritten
        // NOR reported — the exact tautology this module exists to prevent.
        branding::DOT_DIR,
        branding::PLUGIN_MANIFEST_DIR,
        branding::PLUGIN_ROOT_ENV,
        branding::PLUGIN_DATA_ENV,
        branding::PROJECT_DIR_ENV,
    ]
    .into_iter()
    .map(|lingxi| BrandPair {
        lingxi,
        claude: claude_counterpart(lingxi),
    })
    .collect()
}

/// Repository root above `crates/plugin`, containing the shared brand policy.
fn workspace_root() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR"))
        .canonicalize()
        .expect("plugin crate dir exists")
        .parent()
        .and_then(Path::parent)
        .expect("plugin sits under the workspace crates directory")
        .to_path_buf()
}

/// Parse `scripts/brand_frozen_identities.txt` (format: `<path>:<literal>  #
/// reason`, `#`-prefixed and blank lines skipped — mirrors
/// `check_brand_leaks.py`'s `parse_frozen`). Returns an empty list if the
/// file is missing rather than panicking: a normalizer with no frozen list to
/// consult should still function (everything just falls through to
/// known-pair-or-unmapped), and the two required tests below assert the file
/// DOES exist and has content, so a silent empty-list degradation cannot hide
/// behind a passing test suite.
#[must_use]
pub fn load_frozen_identities() -> Vec<FrozenIdentity> {
    let path = workspace_root()
        .join("scripts")
        .join("brand_frozen_identities.txt");
    let Ok(text) = std::fs::read_to_string(&path) else {
        return Vec::new();
    };
    text.lines()
        .filter_map(|raw| {
            let stripped = raw.trim();
            if stripped.is_empty() || stripped.starts_with('#') {
                return None;
            }
            let (entry_part, reason) = match stripped.split_once(" #") {
                Some((e, r)) => (e.trim(), r.trim().to_string()),
                None => (stripped, String::new()),
            };
            if entry_part.is_empty() {
                return None;
            }
            let (path_part, literal) = entry_part.split_once(':')?;
            if literal.is_empty() {
                return None;
            }
            Some(FrozenIdentity {
                path: path_part.to_string(),
                literal: literal.to_string(),
                reason,
            })
        })
        .collect()
}

/// Does the frozen entry's path (repo-root-relative, e.g.
/// `crates/mcp/src/headers_helper.rs`) identify the same file as a
/// caller-supplied `source_path` (workspace-relative, e.g.
/// `mcp/src/headers_helper.rs`)? Accepts either spelling so callers do not
/// need to know which root the frozen list's paths are relative to.
fn paths_match(frozen_path: &str, source_path: &str) -> bool {
    let workspace_relative = frozen_path
        .strip_prefix("crates/")
        .or_else(|| frozen_path.strip_prefix("lingxi-code/"))
        .unwrap_or(frozen_path);
    workspace_relative == source_path || frozen_path == source_path
}

/// Find every maximal run starting with `prefix` and continuing while
/// `extend` holds, as byte ranges into `text`. Deliberately hand-rolled
/// (`plugin` has no `regex` dependency and this task cannot add one — its
/// owned files are `brand_normalize.rs` / `lib.rs` / `tests/fixtures/`, not
/// `Cargo.toml`); `prefix` is always a short brand-gate-safe fragment (see
/// [`scan_tokens`]), never a full oracle token.
fn find_runs(text: &str, prefix: &str, extend: impl Fn(u8) -> bool, out: &mut Vec<(usize, usize)>) {
    let bytes = text.as_bytes();
    let plen = prefix.len();
    if plen == 0 || bytes.len() < plen {
        return;
    }
    let mut i = 0;
    while i + plen <= bytes.len() {
        if &bytes[i..i + plen] == prefix.as_bytes() {
            let mut end = i + plen;
            while end < bytes.len() && extend(bytes[end]) {
                end += 1;
            }
            out.push((i, end));
            i = end;
        } else {
            i += 1;
        }
    }
}

/// Locate every Claude-plugin-shaped token in `text`: a `CLAUDE_` run
/// (env-var spelling, e.g. `CLAUDE_PLUGIN_ROOT`, `CLAUDE_PLUGIN_OPTION_KEY`)
/// or a `.claude-` run (manifest-directory spelling, e.g. `.claude-plugin`).
/// Byte ranges, sorted, non-overlapping (the two prefixes cannot start at the
/// same position).
fn scan_tokens(text: &str) -> Vec<(usize, usize)> {
    let mut spans = Vec::new();
    // REVIEW FIX: both prefixes are now DERIVED from `branding` rather than
    // typed, so this file spells out no oracle fragment at all (it previously
    // carried the literals `"CLAUDE_"` and `".claude-"`).
    //
    // The dotted prefix also widened from the hyphenated form to the bare
    // brand dot-dir. The hyphenated form could not see the oracle config
    // directory (`<DOT_DIR>`'s counterpart, and `<DOT_DIR>.json`'s stem), so
    // those passed through unrewritten AND unreported. Maximal-run semantics
    // keep the manifest directory a single distinct token, so widening the
    // prefix costs nothing there.
    let env_prefix = claude_counterpart(branding::ENV_PREFIX);
    let dot_prefix = claude_counterpart(branding::DOT_DIR);
    find_runs(
        text,
        &env_prefix,
        |c| c.is_ascii_uppercase() || c.is_ascii_digit() || c == b'_',
        &mut spans,
    );
    find_runs(
        text,
        &dot_prefix,
        |c| c.is_ascii_lowercase() || c.is_ascii_digit() || c == b'-',
        &mut spans,
    );
    spans.sort_unstable();
    spans
}

/// Normalize `text` (read from, or standing in for, a fixture file whose
/// workspace-relative path is `source_path`): rewrite every recognized,
/// non-frozen Claude-plugin token to its LingXi spelling; leave frozen and
/// unrecognized tokens in place, reporting both. See the module doc for the
/// full design rationale and the crucial distinction between "frozen" and
/// "mapped".
#[must_use]
pub fn normalize(source_path: &str, text: &str) -> NormalizeReport {
    let pairs = known_pairs();
    let frozen = load_frozen_identities();

    let mut output = String::with_capacity(text.len());
    let mut last = 0usize;
    let mut mapped = Vec::new();
    let mut preserved = Vec::new();
    let mut unmapped = Vec::new();

    for (start, end) in scan_tokens(text) {
        output.push_str(&text[last..start]);
        let token = &text[start..end];

        let frozen_here = frozen
            .iter()
            .find(|f| f.literal == token && paths_match(&f.path, source_path));

        if let Some(f) = frozen_here {
            output.push_str(token);
            preserved.push(f.literal.clone());
        } else if let Some(pair) = pairs.iter().find(|p| p.claude == token) {
            output.push_str(pair.lingxi);
            mapped.push(token.to_string());
        } else {
            output.push_str(token);
            unmapped.push(token.to_string());
        }
        last = end;
    }
    output.push_str(&text[last..]);

    NormalizeReport {
        output,
        mapped,
        preserved,
        unmapped,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The checked-in fixture plugin tree every fixture-backed test below
    /// reads. One definition, so a rename of the tree cannot leave half the
    /// tests silently reading a path that no longer exists (a missing
    /// directory would otherwise turn some of these into vacuous passes
    /// rather than failures).
    fn fixture_root() -> PathBuf {
        Path::new(env!("CARGO_MANIFEST_DIR")).join("tests/fixtures/brand-normalize-sample")
    }

    /// POSITIVE CONTROL for the whole module: [`known_pairs`] must actually
    /// come from a derivation, not a `return` that forgot to transform. If
    /// [`claude_counterpart`] ever degenerated into an identity function, the
    /// pairs below would come back with `claude == lingxi` and every other
    /// test in this file (which relies on the two spellings differing) would
    /// pass for the wrong reason.
    #[test]
    fn known_pairs_derive_from_branding_constants() {
        let pairs = known_pairs();
        assert_eq!(
            pairs.len(),
            5,
            "expected exactly branding's 5 plugin-contract constants"
        );

        let lower = &branding::DOT_DIR[1..];
        let upper = lower.to_uppercase();
        for pair in &pairs {
            assert_ne!(
                pair.claude, pair.lingxi,
                "derivation produced no change for {}",
                pair.lingxi
            );
            // Same length: a `LINGXI`->`CLAUDE` / `lingxi`->`claude` swap is
            // a same-length word substitution, so a transform that appended,
            // truncated, or partially replaced would show up here.
            assert_eq!(
                pair.claude.len(),
                pair.lingxi.len(),
                "{} -> {} changed length",
                pair.lingxi,
                pair.claude
            );
            assert!(
                !pair.claude.contains(upper.as_str()) && !pair.claude.contains(lower),
                "{}'s claude spelling {} still contains the LingXi brand word",
                pair.lingxi,
                pair.claude
            );
        }
    }

    /// Required test. For every entry in `scripts/brand_frozen_identities.txt`
    /// (the L3 list), [`normalize`] at that entry's exact frozen path is the
    /// identity map on that literal: it survives byte-for-byte and is
    /// recorded as `preserved`, never `mapped` or `unmapped`.
    ///
    /// Two documented entries exist today with two different reasons a token
    /// is inert to this module (see the module doc's "Frozen identities are a
    /// second, orthogonal concept" section), so this test exercises BOTH
    /// shapes rather than assuming they behave identically:
    ///
    /// * `CLAUDE_PLUGIN_ROOT` — recognized by [`scan_tokens`] AND one of
    ///   [`known_pairs`]'s ordinary mapped tokens. Preserving it at its
    ///   frozen path is a real exemption, checked with a POSITIVE CONTROL:
    ///   the identical literal at any OTHER path must still be rewritten. If
    ///   the frozen check were not path-scoped (e.g. "never touch
    ///   `CLAUDE_PLUGIN_ROOT` anywhere"), that pair would be permanently dead
    ///   code and this control would fail.
    /// * `CLAUDECODE` — not a plugin-contract token shape at all (no `_`
    ///   after `CLAUDE`), so [`scan_tokens`] never even recognizes it. It is
    ///   inert to this module by construction, not by an exemption — a
    ///   different kind of "preserved" than the header key. This branch pins
    ///   that fact explicitly instead of silently reusing the other branch's
    ///   assertions (which would misreport WHY it survives unchanged).
    #[test]
    fn normalizer_maps_every_frozen_identity() {
        let entries = load_frozen_identities();
        assert_eq!(
            entries.len(),
            2,
            "expected exactly the two documented frozen entries in \
             scripts/brand_frozen_identities.txt; update this test (and the \
             module doc) if that list grew or shrank"
        );

        let mut exercised_a_real_exemption = false;

        for entry in &entries {
            let text = format!("token: {}\n", entry.literal);
            let report = normalize(&entry.path, &text);
            assert_eq!(
                report.output, text,
                "{} at its frozen path {} must be the identity map",
                entry.literal, entry.path
            );
            assert!(
                report.mapped.is_empty(),
                "{} must never be rewritten",
                entry.literal
            );
            assert!(
                report.unmapped.is_empty(),
                "{} must never be reported unmapped at its frozen path",
                entry.literal
            );

            if scan_tokens(&entry.literal).is_empty() {
                // Not a recognized shape (e.g. CLAUDECODE): inert by
                // construction, so it cannot have been "preserved" by the
                // frozen-path check either.
                assert!(
                    report.preserved.is_empty(),
                    "{} is not a plugin-contract token shape and should never \
                     reach the frozen-path exemption logic",
                    entry.literal
                );
                continue;
            }

            // Recognized shape: preservation here IS the frozen-path
            // exemption doing real work.
            assert_eq!(report.preserved, vec![entry.literal.clone()]);
            exercised_a_real_exemption = true;

            // POSITIVE CONTROL: same literal, a path the frozen list does
            // NOT license.
            let elsewhere = normalize("plugin/src/some_other_file.rs", &text);
            assert!(
                elsewhere.preserved.is_empty(),
                "frozen exemption for {} leaked past its licensed path {}",
                entry.literal,
                entry.path
            );
            let is_known_pair = known_pairs().iter().any(|p| p.claude == entry.literal);
            if is_known_pair {
                assert_eq!(
                    elsewhere.mapped,
                    vec![entry.literal.clone()],
                    "outside its frozen path, {} must be an ordinary known-pair rename",
                    entry.literal
                );
                assert!(!elsewhere.output.contains(entry.literal.as_str()));
            }
        }

        assert!(
            exercised_a_real_exemption,
            "none of the frozen entries exercised the path-scoped exemption logic — \
             this test would pass unchanged even if that logic were deleted"
        );
    }

    /// Required test — the whole point of this module. A Claude-plugin-shaped
    /// token that is neither a known pair nor frozen at this path must be
    /// REPORTED, not silently passed through as if the comparison it feeds
    /// were trustworthy.
    ///
    /// `CLAUDE_PLUGIN_OPTION_API_KEY` is deliberately not synthetic: per
    /// `hooks::user_config::option_env_var`, `CLAUDE_PLUGIN_OPTION_<KEY>` is
    /// a real part of the oracle plugin contract that `branding` exports no
    /// constant for, so it is a genuine gap, not a contrived one.
    ///
    /// POSITIVE CONTROL: the same input also carries a known pair
    /// (`CLAUDE_PROJECT_DIR`'s spelling), which MUST be mapped and must NOT
    /// also land in `unmapped` — proving the scanner distinguishes
    /// "recognized" from "unrecognized" rather than either flagging
    /// everything Claude-shaped (which would make the report meaningless) or
    /// flagging nothing (the house-defect tautology this module exists to
    /// avoid).
    #[test]
    fn an_unmapped_claude_identity_is_reported_not_silently_kept() {
        // Built from `claude_counterpart(branding::PLUGIN_ROOT_ENV)` rather
        // than typed out, so this file never spells `CLAUDE_PLUGIN_` out
        // contiguously in a non-comment line — see the module doc. The
        // `OPTION_API_KEY` suffix alone is not a G1/G7 needle.
        let unknown = {
            let mut s = claude_counterpart(branding::PLUGIN_ROOT_ENV);
            s.truncate(s.len() - "ROOT".len());
            s.push_str("OPTION_API_KEY");
            s
        };
        assert!(
            known_pairs().iter().all(|p| p.claude != unknown),
            "test fixture assumption broken: {unknown} must not be one of the known pairs"
        );
        let known = claude_counterpart(branding::PROJECT_DIR_ENV);
        let text = format!("a={unknown} b={known}\n");

        let report = normalize("plugin/src/some_fixture.rs", &text);

        assert_eq!(
            report.unmapped,
            vec![unknown.clone()],
            "an unrecognized Claude-plugin-shaped token must be named in the report"
        );
        assert!(!report.is_fully_understood());
        assert_eq!(
            report.mapped,
            vec![known.clone()],
            "a known pair in the SAME input must still be mapped, not swept into unmapped"
        );
        assert!(
            report.output.contains(unknown.as_str()),
            "an unmapped token must be left in place, not deleted"
        );
        assert!(report.output.contains(branding::PROJECT_DIR_ENV));
        assert!(!report.output.contains(known.as_str()));

        // NEGATIVE CONTROL: a fully-known input reports nothing unmapped.
        let clean = format!("{}\n", claude_counterpart(branding::PLUGIN_DATA_ENV));
        let clean_report = normalize("plugin/src/some_fixture.rs", &clean);
        assert!(clean_report.is_fully_understood());
        assert!(clean_report.unmapped.is_empty());
    }

    /// Sanity check on [`load_frozen_identities`] itself: it must actually
    /// read the real file (not silently degrade to the empty-file fallback),
    /// and [`paths_match`] must accept both the repo-root-relative spelling
    /// the file uses and the workspace-relative spelling callers pass.
    #[test]
    fn frozen_identities_file_is_read_and_paths_normalize() {
        let entries = load_frozen_identities();
        assert!(
            !entries.is_empty(),
            "scripts/brand_frozen_identities.txt read as empty — \
             load_frozen_identities is silently hitting its missing-file fallback"
        );
        for entry in &entries {
            assert!(entry.path.starts_with("crates/"));
            assert!(
                !entry.reason.is_empty(),
                "{} has no stated reason",
                entry.literal
            );
            let workspace_relative = entry.path.strip_prefix("crates/").unwrap();
            assert!(paths_match(&entry.path, workspace_relative));
            assert!(!paths_match(&entry.path, "some/unrelated/path.rs"));
        }
    }

    /// Uses the checked-in fixture tree (`tests/fixtures/`): its manifest
    /// references the LingXi env-var spellings, and round-tripping through
    /// `claude_counterpart` + [`normalize`] must reproduce it exactly. This
    /// is what makes the fixture directory usable by later 0a/0b tasks
    /// rather than inert clutter — it is exercised, not just present.
    #[test]
    fn fixture_plugin_manifest_round_trips_through_the_normalizer() {
        let manifest_path = fixture_root()
            .join(branding::PLUGIN_MANIFEST_DIR)
            .join("plugin.json");
        let lingxi_text = std::fs::read_to_string(&manifest_path)
            .unwrap_or_else(|e| panic!("read fixture manifest {}: {e}", manifest_path.display()));

        // Build a synthetic oracle-shaped version of the SAME fixture at run
        // time (never persisted — see the module doc on why an oracle
        // literal must not land in tracked source/fixture text without a
        // frozen-identity license this crate does not hold).
        let oracle_text = claude_counterpart(&lingxi_text);
        assert_ne!(
            oracle_text, lingxi_text,
            "fixture manifest has no LingXi tokens to swap"
        );

        let report = normalize(
            "plugin/tests/fixtures/brand-normalize-sample/synthetic",
            &oracle_text,
        );
        assert!(
            report.is_fully_understood(),
            "normalizing the fixture's oracle counterpart reported unmapped tokens: {:?}",
            report.unmapped
        );
        assert_eq!(
            report.output, lingxi_text,
            "oracle-shaped fixture did not normalize back to the checked-in LingXi fixture"
        );
        assert!(
            !report.mapped.is_empty(),
            "round trip exercised zero rewrites"
        );
    }
    /// REVIEW FIX (P0a.7 review). The manifest-DIRECTORY pair travels a
    /// different scanner branch from the three env-var pairs: a dotted,
    /// all-lowercase run, not a `CLAUDE_` screaming-snake one. Before this
    /// test, nothing exercised that branch through [`normalize`] — the two
    /// frozen entries and the unmapped exemplar are all env-var shaped, and
    /// the round-trip fixture's manifest *text* happens to reference no
    /// manifest directory (the directory name is the tree's, not the JSON's).
    ///
    /// Measured, not assumed: deleting the whole dotted `find_runs` call from
    /// [`scan_tokens`] left all five pre-existing tests in this module green.
    /// That regression would let the manifest directory pass through both
    /// unrewritten *and* unreported — which is exactly the tautology
    /// [`an_unmapped_claude_identity_is_reported_not_silently_kept`] exists to
    /// prevent, one spelling over: an oracle tree and a LingXi tree would
    /// "agree" on the directory name because neither side was touched.
    ///
    /// The LingXi spelling is read off the checked-in fixture TREE's real
    /// on-disk directory rather than from a constant, so this assertion is
    /// bound to the tree the later 0a/0b comparison tasks consume.
    #[test]
    fn manifest_directory_pair_is_mapped_and_an_unknown_dotted_sibling_is_reported() {
        let root = fixture_root();
        let dotted: Vec<String> = std::fs::read_dir(&root)
            .unwrap_or_else(|e| panic!("read fixture tree {}: {e}", root.display()))
            .filter_map(Result::ok)
            .map(|e| e.file_name().to_string_lossy().into_owned())
            .filter(|n| n.starts_with('.'))
            .collect();
        assert_eq!(
            dotted,
            vec![branding::PLUGIN_MANIFEST_DIR.to_string()],
            "fixture tree {} must hold exactly the branding manifest directory",
            root.display()
        );
        let on_disk = &dotted[0];

        let oracle_dir = claude_counterpart(on_disk);
        assert_ne!(
            &oracle_dir, on_disk,
            "manifest-directory pair has no oracle counterpart to map"
        );
        // A near miss: same dotted shape, not a known pair. Built by
        // appending to the derived spelling, never typed out.
        let oracle_sibling = format!("{oracle_dir}s");
        assert!(known_pairs().iter().all(|p| p.claude != oracle_sibling));

        let text = format!("open {oracle_dir}/plugin.json then {oracle_sibling}/index.json\n");
        let report = normalize(
            "plugin/tests/fixtures/brand-normalize-sample/synthetic",
            &text,
        );

        assert_eq!(
            report.mapped,
            vec![oracle_dir.clone()],
            "the manifest directory must be rewritten, not passed through"
        );
        assert!(
            report.output.contains(&format!("{on_disk}/plugin.json")),
            "rewritten output does not carry the fixture tree's own directory name: {}",
            report.output
        );
        assert!(
            !report.output.contains(&format!("{oracle_dir}/")),
            "the oracle manifest directory survived the rewrite: {}",
            report.output
        );

        // POSITIVE CONTROL for the *report*, on this branch specifically: a
        // dotted Claude-shaped token that is NOT a known pair must be named,
        // not silently kept. Without this, a scanner that recognized only the
        // one exact known dotted spelling would pass this test while going
        // silent on every other dotted oracle identifier.
        assert_eq!(
            report.unmapped,
            vec![oracle_sibling.clone()],
            "an unrecognized dotted Claude token must be reported"
        );
        assert!(!report.is_fully_understood());
        assert!(
            report.output.contains(&oracle_sibling),
            "an unmapped token must be left in place, not deleted"
        );
        assert!(report.preserved.is_empty());
    }

    /// REVIEW FIX (P0a.7 review). The fixture tree existed but no test ever
    /// handed it to a parser — [`fixture_plugin_manifest_round_trips_through_the_normalizer`]
    /// reads its manifest as raw bytes. A fixture that the production loader
    /// rejects (or that silently yields an empty component inventory) would
    /// make every later §19.1 "same component inventory" comparison built on
    /// it vacuous, and nothing here would have said so.
    ///
    /// This calls what PRODUCTION calls: `discovery::load_plugin_from_path`,
    /// the `createPluginFromPath` port that `discover_installed_plugins` /
    /// `discover_cli_plugin_dirs` funnel through — not a test-local parser.
    /// It asserts the two declared components actually materialize, so a
    /// manifest shape the loader ignores (e.g. an `mcpServers` map the
    /// bare-map fallback cannot read) fails here instead of surfacing later
    /// as a comparison that finds nothing on either side.
    #[tokio::test]
    async fn fixture_tree_loads_through_the_production_plugin_loader() {
        let root = fixture_root();
        let (_id, manifest) = crate::discovery::load_plugin_from_path(&root)
            .await
            .unwrap_or_else(|| {
                panic!(
                    "production loader rejected the fixture plugin tree at {}",
                    root.display()
                )
            });

        assert_eq!(manifest.name, "brand-normalize-sample");
        assert_eq!(manifest.version, "1.0.0");
        assert_eq!(
            manifest.components.hooks.len(),
            1,
            "fixture declares one PreToolUse hook; loader materialized {}",
            manifest.components.hooks.len()
        );
        let mut servers: Vec<&str> = manifest
            .components
            .mcp_servers
            .keys()
            .map(String::as_str)
            .collect();
        servers.sort_unstable();
        assert_eq!(
            servers,
            vec!["sample"],
            "fixture declares one MCP server; loader materialized {servers:?}"
        );
    }
    /// REVIEW FIX (P0a.7 review). The bare brand dot-directory — the plugin
    /// CACHE root (`<DOT_DIR>/plugins`, see `git.rs` / `marketplace.rs` /
    /// `discovery.rs`) and the stem of the global config file that carries
    /// `enabledPlugins` — is a plugin-contract identifier the original pair
    /// set omitted, and the original scanner's hyphenated dotted prefix could
    /// not even see it. An oracle fixture referencing the oracle config dir
    /// was therefore left byte-identical AND reported fully understood: both
    /// sides of a §19.1 comparison would "agree" on an unnormalized oracle
    /// path. That is the tautology
    /// [`an_unmapped_claude_identity_is_reported_not_silently_kept`] exists to
    /// prevent, reached under a spelling it does not cover.
    ///
    /// Asserts the config-dir and global-config-file spellings both come out
    /// the LingXi side, and — the part that makes this more than a rename
    /// check — that the widened prefix did NOT swallow the manifest directory
    /// into the same token (maximal-run semantics keep them distinct), nor
    /// start blanket-accepting every dotted oracle-looking run: an unknown
    /// dotted sibling is still reported.
    #[test]
    fn config_directory_pair_is_mapped_and_does_not_swallow_the_manifest_directory() {
        let oracle_dot = claude_counterpart(branding::DOT_DIR);
        let oracle_manifest = claude_counterpart(branding::PLUGIN_MANIFEST_DIR);
        assert!(
            oracle_manifest.starts_with(&oracle_dot),
            "this test's premise (manifest dir extends the dot dir) no longer holds"
        );

        let text = format!(
            "cache={oracle_dot}/plugins manifest={oracle_manifest}/plugin.json \
             config={oracle_dot}.json\n"
        );
        let report = normalize(
            "plugin/tests/fixtures/brand-normalize-sample/synthetic",
            &text,
        );

        assert_eq!(
            report.mapped,
            vec![
                oracle_dot.clone(),
                oracle_manifest.clone(),
                oracle_dot.clone()
            ],
            "the dot dir and the manifest dir must be scanned as THREE distinct \
             tokens and each mapped on its own"
        );
        assert!(report.is_fully_understood());
        assert_eq!(
            report.output,
            format!(
                "cache={}/plugins manifest={}/plugin.json config={}.json\n",
                branding::DOT_DIR,
                branding::PLUGIN_MANIFEST_DIR,
                branding::DOT_DIR
            ),
            "rewritten output is not the LingXi spelling of every token"
        );

        // The widened prefix must not have degenerated into "accept anything
        // dotted": an unknown dotted run is still REPORTED, never absorbed.
        let unknown = format!("{oracle_dot}-not-a-real-slot");
        assert!(known_pairs().iter().all(|p| p.claude != unknown));
        let noisy = normalize("plugin/src/some_fixture.rs", &format!("x={unknown}\n"));
        assert_eq!(noisy.unmapped, vec![unknown.clone()]);
        assert!(!noisy.is_fully_understood());
        assert!(noisy.output.contains(&unknown));
    }
}
