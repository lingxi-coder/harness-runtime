//! Nested-memory discovery — claude-code `Rop` (2.1.220 @237715260).
//!
//! Filenames come from the `branding` constants, so these fixtures build
//! `LINGXI.md` / `.lingxi` trees, not the oracle's literals.

use orchestrator::prompt::nested_memory::discover;
use std::path::{Path, PathBuf};
use tempfile::TempDir;

fn touch(p: &Path, body: &str) {
    std::fs::create_dir_all(p.parent().unwrap()).unwrap();
    std::fs::write(p, body).unwrap();
}

const MEM: &str = branding::MEMORY_FILE;
const DOT: &str = branding::DOT_DIR;

/// Layout: cwd=`repo`, trigger `repo/pkg/api/handler.rs`.
fn fixture() -> (TempDir, PathBuf, PathBuf, PathBuf) {
    let tmp = TempDir::new().unwrap();
    // Canonicalize the root ONCE so cwd and the trigger agree; the realpath
    // branch of `split_ancestors` has its own unit test.
    let root = std::fs::canonicalize(tmp.path()).unwrap();
    let cwd = root.join("repo");
    let home = root.join("home");
    std::fs::create_dir_all(&home).unwrap();
    let trigger = cwd.join("pkg").join("api").join("handler.rs");
    touch(&trigger, "fn main(){}");
    (tmp, cwd, home, trigger)
}

/// The headline behaviour: unconditional memory in EVERY directory between cwd
/// and the touched file is discovered, outermost-first.
#[test]
fn nested_dirs_between_cwd_and_the_file_are_discovered_outermost_first() {
    let (_tmp, cwd, home, trigger) = fixture();
    touch(&cwd.join("pkg").join(MEM), "pkg guidance");
    touch(&cwd.join("pkg").join("api").join(MEM), "api guidance");

    let got = discover(&trigger, &cwd, &home, None);
    let paths: Vec<PathBuf> = got.iter().map(|f| f.path.clone()).collect();

    assert_eq!(
        paths,
        vec![
            cwd.join("pkg").join(MEM),
            cwd.join("pkg").join("api").join(MEM)
        ],
        "outermost-first"
    );
}

/// cwd's OWN memory file is not re-surfaced: it is already in the eager block,
/// and `split_ancestors` excludes cwd from the nested list.
#[test]
fn cwd_s_own_memory_file_is_not_surfaced() {
    let (_tmp, cwd, home, trigger) = fixture();
    touch(&cwd.join(MEM), "repo-root guidance");
    touch(&cwd.join("pkg").join(MEM), "pkg guidance");

    let got = discover(&trigger, &cwd, &home, None);
    let paths: Vec<PathBuf> = got.iter().map(|f| f.path.clone()).collect();

    assert_eq!(paths, vec![cwd.join("pkg").join(MEM)]);
}

/// Discovery is PURE and re-runs from scratch: the oracle's `seen` set is
/// `let o=new Set()` INSIDE `Rop`, created per call, so a second call re-finds
/// the same files. Session-level "don't send this twice" is
/// `loadedNestedMemoryPaths`, which lives at the CALLER (see
/// `nested_memory_reminder_messages`) — not here. Keeping the two apart is what
/// lets a LINGXI.md created mid-session still be discovered.
#[test]
fn discovery_is_per_call_and_re_finds_the_same_files() {
    let (_tmp, cwd, home, trigger) = fixture();
    touch(&cwd.join("pkg").join(MEM), "pkg guidance");

    assert_eq!(discover(&trigger, &cwd, &home, None).len(), 1);
    assert_eq!(
        discover(&trigger, &cwd, &home, None).len(),
        1,
        "`seen` is per-call (oracle `Rop`: `let o=new Set()`), not session-scoped"
    );
}

/// A memory file that appears mid-session is discovered on the NEXT call — the
/// direct consequence of the per-call `seen` set, and the behaviour a
/// session-scoped probe set would have silently broken.
#[test]
fn a_memory_file_created_mid_session_is_discovered() {
    let (_tmp, cwd, home, trigger) = fixture();
    assert!(discover(&trigger, &cwd, &home, None).is_empty());

    touch(&cwd.join("pkg").join(MEM), "pkg guidance");
    assert_eq!(discover(&trigger, &cwd, &home, None).len(), 1);
}

/// A `paths:`-gated rule in a nested dir is surfaced ONLY when its globs match
/// the trigger — and it comes AFTER the unconditional files from the same
/// directory, mirroring the oracle's two-pass `rules` scan.
#[test]
fn conditional_rules_are_matched_against_the_trigger_and_ordered_last() {
    let (_tmp, cwd, home, trigger) = fixture();
    let pkg = cwd.join("pkg");
    touch(&pkg.join(MEM), "unconditional pkg guidance");
    // Globs on a Project-tier rule resolve against the rule's OWN project base
    // — `dirname^3` of `<base>/.lingxi/rules/x.md`, i.e. `pkg` here — NOT
    // against cwd. So the pattern is `api/**`, not `pkg/api/**`.
    touch(
        &pkg.join(DOT).join("rules").join("api.md"),
        "---\npaths:\n  - \"api/**\"\n---\napi rule\n",
    );
    touch(
        &pkg.join(DOT).join("rules").join("web.md"),
        "---\npaths:\n  - \"web/**\"\n---\nweb rule\n",
    );

    let got = discover(&trigger, &cwd, &home, None);
    let names: Vec<String> = got
        .iter()
        .map(|f| f.path.file_name().unwrap().to_string_lossy().to_string())
        .collect();

    assert!(
        names.contains(&MEM.to_string()),
        "unconditional file present: {names:?}"
    );
    assert!(
        names.contains(&"api.md".to_string()),
        "matching rule: {names:?}"
    );
    assert!(
        !names.contains(&"web.md".to_string()),
        "non-matching rule must NOT be surfaced: {names:?}"
    );
    assert!(
        names.iter().position(|n| n == MEM).unwrap()
            < names.iter().position(|n| n == "api.md").unwrap(),
        "unconditional before conditional: {names:?}"
    );
}

/// The USER tier resolves external `@import`s; every other tier does not.
///
/// `NLu` is explicit: `lfo(e,n,"Managed",t,!1)` then `lfo(e,o,"User",t,!0)`.
/// The only OBSERVABLE consequence is an imported file that carries its OWN
/// matching `paths:` — `lfo`'s tail filter (`if(!l.globs||l.globs.length===0)
/// return!1`) drops every parentless import, so a plain `@import` inside a
/// conditional rule never surfaces regardless of the flag. This test therefore
/// gives the import its own globs; without them it would pass vacuously.
#[test]
fn a_user_tier_rule_resolves_its_external_imports() {
    let (_tmp, cwd, home, trigger) = fixture();
    // A User-tier rule's globs resolve against CWD (oracle: the Project branch
    // takes `dirname^2` of the rules dir, everything else takes cwd).
    let outside = home.join("shared-rule.md");
    touch(
        &outside,
        "---\npaths:\n  - \"pkg/**\"\n---\nshared rule body\n",
    );
    touch(
        &home.join(DOT).join("rules").join("u.md"),
        &format!(
            "---\npaths:\n  - \"pkg/**\"\n---\nuser rule\n@{}\n",
            outside.display()
        ),
    );

    let got = discover(&trigger, &cwd, &home, None);
    let names: Vec<String> = got
        .iter()
        .map(|f| f.path.file_name().unwrap().to_string_lossy().to_string())
        .collect();

    assert!(
        names.contains(&"u.md".to_string()),
        "the rule itself: {names:?}"
    );
    assert!(
        names.contains(&"shared-rule.md".to_string()),
        "its external @import must be spliced in: {names:?}"
    );
}

/// The MANAGED half of the same pass gets `includeExternal:!1`, so the mirror
/// fixture must NOT surface its import. This is the half that would silently
/// break if discovery routed through `memory_block::include_external_for`,
/// which returns `true` for Managed once the project approves external
/// includes.
#[test]
fn a_managed_tier_rule_does_not_resolve_external_imports() {
    let (_tmp, cwd, home, trigger) = fixture();
    let managed = home.parent().unwrap().join("managed");
    let outside = managed.join("shared-rule.md");
    touch(
        &outside,
        "---\npaths:\n  - \"pkg/**\"\n---\nshared rule body\n",
    );
    touch(
        &managed.join(DOT).join("rules").join("m.md"),
        &format!(
            "---\npaths:\n  - \"pkg/**\"\n---\nmanaged rule\n@{}\n",
            outside.display()
        ),
    );

    let got = discover(&trigger, &cwd, &home, Some(&managed));
    let names: Vec<String> = got
        .iter()
        .map(|f| f.path.file_name().unwrap().to_string_lossy().to_string())
        .collect();

    assert!(
        names.contains(&"m.md".to_string()),
        "the rule itself: {names:?}"
    );
    assert!(
        !names.contains(&"shared-rule.md".to_string()),
        "Managed gets includeExternal:!1 — the import must NOT surface: {names:?}"
    );
}

/// A file outside cwd has no nested ancestors under cwd, so nothing is
/// discovered from the nested pass.
#[test]
fn a_trigger_outside_cwd_discovers_no_nested_memory() {
    let (_tmp, cwd, home, _trigger) = fixture();
    let outside = cwd.parent().unwrap().join("elsewhere").join("x.rs");
    touch(&outside, "x");
    touch(&cwd.join("pkg").join(MEM), "pkg guidance");

    assert!(discover(&outside, &cwd, &home, None).is_empty());
}
