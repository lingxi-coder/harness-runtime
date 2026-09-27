//! Nested memory discovery — the memory that governs a TOUCHED file's location.
//!
//! Ports claude-code `Rop` (2.1.220 @237715260), which runs three passes over a
//! trigger file and returns the memory files that apply to it:
//!
//! ```js
//! let s=await NLu(e,o);                    n.push(...k$o(s,t,e));
//! let {nestedDirs:a,cwdLevelDirs:l}=Aop(e,i);
//! for(let u of a){ let d=await ffo(u,e,o); n.push(...k$o(d,t,e)) }
//! for(let u of l){ let d=await FLu(u,e,o); n.push(...k$o(d,t,e)) }
//! ```
//!
//! | Pass | Oracle | Loads |
//! |---|---|---|
//! | the file itself | `NLu` @230809780 | Managed + User rules, CONDITIONAL only |
//! | each nested dir | `ffo` @230809989 | memory file, dot-dir memory file, local override (all unconditional), then rules (unconditional + conditional) |
//! | each cwd-level dir | `FLu` @230810574 | rules, CONDITIONAL only |
//!
//! Filenames come from [`memory::lingxi_md::hierarchy`]'s `branding` constants —
//! `LINGXI.md` / `.lingxi`, not the oracle's literals.
//!
//! DIVERGENCE(reason): the oracle filters every pass by the
//! `tengu_paper_halyard` gate, dropping Project/Local when it is on. LingXi has
//! no such gate and the oracle's default is OFF, so all tiers load. No gate was
//! invented.

use super::MemoryFile;
use memory::lingxi_md::hierarchy::{self, HierarchyEntry};
use memory::lingxi_md::LingxiMdExcluder;
use std::collections::HashSet;
use std::path::{Path, PathBuf};

/// Expand probed hierarchy entries into [`MemoryFile`]s.
///
/// Mirrors `RealMemoryHierarchyProvider::load`'s mapping so both paths agree on
/// tier inheritance, the local-override flag and the disk-fidelity pair.
///
/// `include_external` is passed per CALL SITE, not derived from the tier: the
/// oracle hardcodes `includeExternal:!1` in `ffo` and `FLu` and in `NLu`'s
/// Managed half, and `!0` in `NLu`'s User half alone
/// (`lfo(e,o,"User",t,!0)`). It is deliberately NOT
/// `memory_block::include_external_for`, which the EAGER block uses: that
/// helper also returns `true` for Managed/Project/Local once the project has
/// approved external includes, and nested discovery never opts into that.
///
/// The `seen` set is threaded in from [`discover`] — the SAME set the probes
/// use, matching `ffo(u,e,o)`, where one `o` guards both halves.
fn expand(
    entries: Vec<HierarchyEntry>,
    cwd: &Path,
    home: &Path,
    include_external: bool,
    seen: &mut HashSet<PathBuf>,
    excluder: Option<&LingxiMdExcluder>,
) -> Vec<MemoryFile> {
    let mut out = Vec::new();
    for e in entries {
        // The oracle's `processMemoryFile` tests-and-inserts `processedPaths`
        // ITSELF, then recurses into `@import`s — one set, one insertion point.
        // LingXi splits that in two: the probe inserted `e.path` when it emitted
        // this entry, and `expand_memory_file` tests the same set on the way in.
        // Hand the entry back so the expander can claim it, or every file is
        // skipped as "already processed" and discovery returns nothing.
        //
        // Safe because a probe only emits paths it did NOT find in `seen`, so an
        // entry here is always one this probe just claimed — never one a prior
        // `@import` expansion already spliced.
        seen.remove(&e.path);
        let expanded = memory::lingxi_md::loader::expand_memory_file_with_excluder(
            &e.path,
            seen,
            include_external,
            cwd,
            Some(home),
            0,
            e.tier,
            excluder,
        );
        for (idx, entry) in expanded.into_iter().enumerate() {
            let body = entry.body.trim().to_string();
            if body.is_empty() {
                continue;
            }
            out.push(MemoryFile {
                path: entry.path,
                body,
                is_local_override: idx == 0 && e.is_local_override,
                tier: e.tier,
                globs: entry.globs,
                raw_content: entry.raw_content,
                content_differs_from_disk: entry.content_differs_from_disk,
            });
        }
    }
    out
}

/// Keep the conditional (`paths:`-gated) files whose globs match `trigger`.
fn matching_conditional(files: Vec<MemoryFile>, trigger: &Path, cwd: &Path) -> Vec<MemoryFile> {
    files
        .into_iter()
        .filter(|f| {
            f.globs.is_some()
                && super::conditional_rules::rule_matches_touched_file(f, trigger, cwd)
        })
        .collect()
}

/// Discover every memory file that governs `trigger`'s location.
///
/// Returns them in the oracle's emission order. PURE and STATELESS across
/// calls: `seen` is created here, exactly as `Rop` does (`let o=new Set()`),
/// so re-running re-finds the same files and a memory file created mid-session
/// is picked up. The session-level "already sent to the model" set is the
/// oracle's `loadedNestedMemoryPaths`, which lives at the CALLER
/// (`ConversationOrchestrator::nested_memory_reminder_message`) — conflating
/// the two here would freeze discovery at whatever existed on turn one.
///
/// Callers likewise own the "has the model already read this" question; this
/// does NOT consult `read_file_state`.
#[must_use]
pub fn discover(
    trigger: &Path,
    cwd: &Path,
    home: &Path,
    managed_dir: Option<&Path>,
) -> Vec<MemoryFile> {
    discover_with_excludes(trigger, cwd, home, managed_dir, None)
}

/// Exclude-aware variant of [`discover`].
#[must_use]
pub fn discover_with_excludes(
    trigger: &Path,
    cwd: &Path,
    home: &Path,
    managed_dir: Option<&Path>,
    excluder: Option<&LingxiMdExcluder>,
) -> Vec<MemoryFile> {
    let mut out: Vec<MemoryFile> = Vec::new();
    let mut seen: HashSet<PathBuf> = HashSet::new();

    // Pass 1 — `NLu`: Managed + User rules, CONDITIONAL only, matched against
    // the trigger. Unconditional Managed/User memory is already in the eager
    // block, so only the glob-gated half can be news here.
    {
        // `NLu` calls `lfo` TWICE with different `includeExternal`:
        // `lfo(e,n,"Managed",t,!1)` then `lfo(e,o,"User",t,!0)`. The User tier's
        // external `@import`s always resolve; Managed's never do. Two expands,
        // not one, or the split is lost.
        let mut managed_entries = Vec::new();
        if let Some(managed) = managed_dir {
            hierarchy::probe_managed_rules(managed, &mut managed_entries, &mut seen);
        }
        let managed_files = expand(managed_entries, cwd, home, false, &mut seen, excluder);
        out.extend(matching_conditional(managed_files, trigger, cwd));

        let mut user_entries = Vec::new();
        hierarchy::probe_user_rules(home, &mut user_entries, &mut seen);
        let user_files = expand(user_entries, cwd, home, true, &mut seen, excluder);
        out.extend(matching_conditional(user_files, trigger, cwd));
    }

    let ancestors = hierarchy::split_ancestors(trigger, cwd);

    // Pass 2 — `ffo` per nested dir. Unconditional files first, then the
    // matching conditional ones: the oracle scans `rules` twice (`ZPt` then
    // `lfo`) and `probe_dir_nested` folds that into one scan, so the split has
    // to happen here to preserve ordering.
    for dir in &ancestors.nested {
        let mut entries = Vec::new();
        hierarchy::probe_dir_nested(dir, &mut entries, &mut seen);
        let files = expand(entries, cwd, home, false, &mut seen, excluder);
        let (conditional, unconditional): (Vec<_>, Vec<_>) =
            files.into_iter().partition(|f| f.globs.is_some());
        out.extend(unconditional);
        out.extend(matching_conditional(conditional, trigger, cwd));
    }

    // Pass 3 — `FLu` per cwd-level dir: rules only, conditional only.
    for dir in &ancestors.cwd_level {
        let mut entries = Vec::new();
        hierarchy::probe_dir_cwd_level(dir, &mut entries, &mut seen);
        let files = expand(entries, cwd, home, false, &mut seen, excluder);
        out.extend(matching_conditional(files, trigger, cwd));
    }

    out
}

#[cfg(test)]
mod tests {
    use super::{discover_with_excludes, MemoryFile};
    use memory::lingxi_md::LingxiMdExcluder;
    use std::fs;
    use std::path::PathBuf;

    fn names(files: &[MemoryFile]) -> Vec<PathBuf> {
        files.iter().map(|f| f.path.clone()).collect()
    }

    #[test]
    fn discover_with_excludes_skips_excluded_nested_rule_and_its_imports() {
        let tmp = tempfile::tempdir().expect("tempdir");
        let cwd = tmp.path().join("repo");
        let managed = tmp.path().join("managed");
        let home = tmp.path().join("home");
        let trigger = cwd.join("src/lib.rs");
        let nested_rules = cwd.join(".lingxi/rules");
        fs::create_dir_all(&nested_rules).expect("mkdir nested rules");
        fs::create_dir_all(trigger.parent().expect("parent")).expect("mkdir trigger parent");
        fs::create_dir_all(home.join(".lingxi/rules")).expect("mkdir home rules");
        fs::create_dir_all(managed.join(".lingxi/rules")).expect("mkdir managed rules");
        fs::write(&trigger, "fn main() {}\n").expect("write trigger");
        fs::write(
            nested_rules.join("secret.md"),
            "---\npaths: src/**\n---\nsecret\n@./child.md\n",
        )
        .expect("write secret rule");
        fs::write(nested_rules.join("child.md"), "child\n").expect("write child");

        let excluder = LingxiMdExcluder::new(&["**/secret.md".to_string()]);
        let files = discover_with_excludes(&trigger, &cwd, &home, Some(&managed), Some(&excluder));

        assert!(
            names(&files).is_empty(),
            "excluded nested rules must not survive discovery"
        );
    }
}
