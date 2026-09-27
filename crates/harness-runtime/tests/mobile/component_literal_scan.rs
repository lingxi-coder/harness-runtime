//! P-1.0 — component-literal scanner.
//!
//! Fails when a Local App component NAME (skill / agent / workflow) appears
//! as a literal in PRODUCTION source outside an explicit, counted allowlist
//! (`tests/component_literal_allowlist.txt`).
//!
//! ## Needle derivation — the one thing that makes this non-trivial
//!
//! The needle set is derived from the LIVE registries at test time, never
//! from a name list hand-typed into this file:
//!
//! - skill basenames come from the build-time verified Plugin inventory exposed
//!   by [`harness_runtime::mobile::mobile_plugin_skill_names`];
//! - Local App workflow basenames come from [`local_app_workflow_basenames`],
//!   which lists the three plugin-owned workflow IDENTITIES
//!   (`lingxi-local-app:local-app-build`, `:local-app-use-test`,
//!   `:local-app-mcp-authoring`) and strips the namespace off each.
//!
//!   ⚠️ Read that literally: for WORKFLOWS this file does keep a list, and the
//!   "no hardcoded name array" rule below is satisfied only in the narrow sense
//!   that no OTHER copy is being twinned. Phase 9 moved the Local App workflows
//!   out of the binary — `tools/workflow/src/builtins.rs` now ships
//!   `deep-research` alone, and its `BuiltinWorkflowDescriptor` has exactly four
//!   fields (`name`, `description`, `script`, `manual_only`) — so no live
//!   in-process registry can answer "which workflows build a Local App" any
//!   more. `scanner_workflow_needles_survive_the_name_list_deletion` pins the
//!   list to exactly those three basenames and proves the derived needles still
//!   catch a planted name, which is the only guard this shape can carry.
//!
//!   An earlier revision of this paragraph claimed the source was
//!   `tool_workflow::BUILTIN_WORKFLOWS.local_app_build_workflow_names()` reading
//!   a typed `is_local_app_build` field. NEITHER symbol exists anywhere in this
//!   tree, and neither ever did — `git grep -P` finds them only in prose like
//!   this. Grep before restating any version of that claim.
//!
//! Design doc `docs/local-apps/LOCAL-APP-PLUGIN-DESIGN-V2.md` §8.5/§19.3:
//! "Phase -1 尚无 Plugin inventory，scanner 暂时读取现有 live builtin
//! skill/workflow registries；两阶段都禁止在 scanner 内维护第二份名称数组。"
//! Phase 2 swaps this needle-derivation seam for the verified Plugin
//! inventory; this file must not grow a hardcoded name array in the
//! meantime — see `scanner_needle_comes_from_discovery_not_from_a_static_array`
//! below, which drives the WHOLE production pipeline
//! ([`needle_set_from`] + [`scan_source_tree`]) off a grown registry and off
//! the real [`production_needle_set`], so narrowing either one is caught.
//!
//! Each basename is doubled into its namespaced FQN (`lingxi-local-app:
//! <basename>`) because a scanner that only matched bare basenames would miss
//! (or under-report) a host module that spells out the Plugin-qualified form —
//! §8.5: "needle 必须同时覆盖 basename 与 namespaced FQN".
//!
//! ## The two views every file is scanned through
//!
//! §8.5's full sentence is "否则 host 模块拼 FQN 或**拆串**即可绕过按
//! basename 的匹配" — the FQN half AND the split-string half. A per-line
//! substring match answers only the first: `concat!("local-app", "-build")`,
//! a rustfmt-wrapped long string literal, or any other assembly of the name
//! out of adjacent fragments walks straight past it. So every file is scanned
//! twice:
//!
//! 1. the RAW view, line by line;
//! 2. a NORMALIZED view of the whole file with `"`, `\`, `,`, `)`, `+`, all
//!    whitespace and the no-op assembly tokens in [`STRIPPED_TOKENS`]
//!    (`concat!(`, `.to_string(`, `String::from(`, …) removed, matched across
//!    line boundaries and reported at the line where the match STARTS.
//!
//! View 2 is a strict superset of view 1 for these needles (none of them
//! contains a stripped character), so occurrences are de-duplicated by their
//! byte span and an occurrence seen only in view 2 is reported as a SPLIT
//! literal. `concat!("local-app", "-build")`, a rustfmt-wrapped long literal
//! and `"local-app".to_string() + "-build"` are all caught this way.
//!
//! TWO shapes remain out of reach, named here rather than left to look covered
//! (`normalization_has_exactly_these_two_documented_limits` pins both, so
//! strengthening the normalizer forces this paragraph to be rewritten):
//!
//! - PLACEHOLDER SUBSTITUTION — `format!("local-{}-build", "app")`, or the JS
//!   template literal `` `local-${x}-build` ``. Catching these needs argument
//!   substitution, which is evaluation, not normalization.
//! - FRAGMENTS SEPARATED BY CODE THAT IS NOT A NO-OP — a statement-level
//!   assembly (`let mut s = String::from("local-app"); s.push_str("-build");`)
//!   leaves `;s.push_str(` between the halves, and single-quoted JS fragments
//!   (`'local-app' + '-build'`) keep their quotes because `'` is a Rust
//!   lifetime sigil and cannot be stripped globally.
//!
//! ## Scan surface
//!
//! Deny-by-default directory enumeration over THREE roots, not one:
//! `harness-runtime/src/mobile`, `tasks/src` and `tools/workflow/src`. §19.3's
//! last bullet ("`tasks/src` 与 `tools/workflow/src` 不含 Local App workflow
//! basename/list") and P-1.9's completion condition are stated over the latter
//! two, so a scanner rooted only at `harness-runtime::mobile/src` could never gate them.
//! A missing root is a hard panic: a root that silently resolves to nothing
//! is the exact no-op that makes a gate green for the wrong reason.
//!
//! Deny-by-default applies to file TYPES too: the extension filter is a
//! DENY-list ([`SKIPPED_EXTENSIONS`]) of formats that cannot be UTF-8 source,
//! not an allow-list of the ones we happened to think of. An UNKNOWN extension
//! is scanned. It used to be `["rs", "js"]`, which left
//! `workflow/src/workflow_description.txt` and
//! `workflow_input_schema.json` — both `include_str!`ed into production, the
//! first being the Workflow tool's own model-facing description — unscanned
//! INSIDE a declared root.
//!
//! `#[cfg(test)]` regions are skipped ("测试 fixtures" are the one place
//! component names may live unlisted — §8.5), including OUT-OF-LINE test
//! modules (`#[cfg(test)] #[path = "registry_test.rs"] mod registry_test;`),
//! whose whole file is excluded — but only when EVERY `mod` declaration that
//! reaches that file is itself `#[cfg(test)]`, so a second, test-gated `mod`
//! aimed at a production file cannot smuggle it out of the scan.
//!
//! Both of those are RUST-ONLY and are not run over other file types. There is
//! no `#[cfg(test)]` in JS or JSON, and [`code_view`] lexes Rust comments and
//! Rust string literals — not JS template or regex literals — so running the
//! detector there could only ever HIDE production text: a template literal
//! containing a line that trims to exactly `#[cfg(test)]` followed by a line
//! ending in `{` would open a skip range in a file that has no such construct.
//!
//! ## Allowlist keying and counting — the load-bearing part
//!
//! Phase -1 deliberately KEEPS the two pre-existing Local App workflow
//! registrations (`tools/workflow/src/builtins.rs` plus the three `.js`
//! files it `include_str!`s). So this scanner CANNOT be green at zero
//! occurrences today, by design.
//!
//! This paragraph used to give a SECOND reason — the skill-name mirror in
//! `register_mobile_skill_commands`, which carried its own hardcoded copy of
//! the ten bundled mobile skill names. P-1.5 deleted that copy: the mirror now
//! iterates `mobile_skill_registry()` itself and names no skill, so it
//! contributes no allowlisted literal and is no longer part of why this
//! scanner's baseline is non-zero.
//!
//! The allowlist is therefore:
//!
//! - EXPLICIT: one entry per `path:literal` pair, matched by EXACT equality on
//!   both — no globs, no directory entries, no prefix matching;
//! - COUNTED TWICE: each entry carries the `expected_count` of occurrences of
//!   that literal in that file (so one entry can never cover an unbounded
//!   number of occurrences), and
//!   `allowlist_entry_count_matches_the_committed_baseline` pins the number of
//!   entries to [`ALLOWLIST_BASELINE_COUNT`];
//! - EXACT, NOT PERMISSIVE: an entry whose count no longer matches — in EITHER
//!   direction — fails, and an entry that matches nothing at all fails
//!   (`every_allowlist_entry_still_matches_a_real_occurrence`). A permitted
//!   SUPERSET would mean deleting the scanned code, or dropping a file from
//!   the walk, leaves the gate green with a stale pre-approved hole sitting at
//!   a fixed location forever.
//!
//! Entries are deliberately NOT keyed on line numbers. A line-keyed allowlist
//! turns every unrelated edit above an allowlisted line into a spurious leak
//! whose cheapest fix is re-typing line numbers with the count unchanged —
//! which is the re-baseline reflex this gate exists to prevent. The FAILURE
//! OUTPUT is still line-numbered: "name the file and the line" is a
//! requirement on what the gate SAYS, not on what the allowlist is keyed by.
#![cfg(feature = "mobile")]

use std::collections::{BTreeMap, BTreeSet};
use std::fs;
use std::path::{Path, PathBuf};

/// Plugin namespace prefix a Local App component carries once addressed
/// through the Plugin surface (design doc §8.5/§19.3: `lingxi-local-app:
/// local-app-build`).
const PLUGIN_NAMESPACE: &str = "lingxi-local-app";

/// Committed baseline for `component_literal_allowlist.txt`'s entry count.
/// See the module doc comment above for why this number — not the scanner's
/// exit code — is the thing that actually gates a regression. Phase 9 is
/// expected to drive this to zero; any change to this constant must be
/// accompanied by an equal change in the allowlist file, in the same diff.
///
/// 24 → 3 (Phase 9 Local App plugin-only cleanup): deleting the stale built-in
/// workflow entries from `tools/workflow/src`, the retired canvas workflow
/// alias, and their dead allowlist rows leaves only the real remaining
/// production component-name literals.
///
/// 3 → 5 (WP8, 2026-09-02 create-flow audit): two new entries name the
/// create skill by its PLUGIN-QUALIFIED registration
/// `lingxi-local-app:create-local-app` — never a build workflow id — in
/// `local_apps_host.rs` and `local_apps_mcp.rs`, the only way left for a
/// model stuck on an unscaffolded shell to find the create flow now that
/// steps 4-5 of the guided workspace contract no longer send it to a
/// "runtime confirmation tool" that does not exist. See the allowlist file's
/// own comments on both entries for why this does not reopen P-1.4.
///
/// 6 → 11 (2026-09-06): three in `host.rs`, where the session-mode command
/// filter keeps exactly the three plugin entry routers visible outside an app
/// workspace — a membership test on `c.name`, so the names ARE the datum — and
/// two in `workflow/src/workflow_description_divergences.json` (moved there
/// from `tools/workflow/src` on 2026-09-10), the
/// register that composes the Workflow tool's shipped description. The latter
/// is the arrival this file's scan-surface note predicted when it put `.txt`
/// and `.json` under `tools/workflow/src` into the scan: prose selecting a
/// workflow moved out of an allowlisted `.js` script and into the description
/// text, and it showed up here as a LEAK. See the allowlist file's comments on
/// all five for why none of them can be reworded away.
///
/// 11 → 13: the retired CREATE launch does NOT retire the `local-app-build`
/// identity — the same id still gates the named/scriptPath UPDATE and VERIFY
/// resumes through `apply_materialized_local_app_collections_with_identity`,
/// so `local_app_plugin_binding.rs`'s entry is RESTORED — and the on-demand
/// testing hand-off prose in `local_apps_host.rs` names the `frontend-qa` /
/// `local-app-test` plugin skills, which is the only way the model can reach
/// the app's use-test path (testing is never host-initiated).
const ALLOWLIST_BASELINE_COUNT: usize = 13;

/// Scan roots, relative to the workspace root. Deny-by-default directory
/// enumeration: every source file under each of these is scanned unless it is
/// an out-of-line `#[cfg(test)]` module.
const SCAN_ROOTS: &[&str] = &[
    "harness-runtime/src/mobile",
    "tasks/src",
    "tools/workflow/src",
    // 2026-09-10: the Workflow tool's two model-facing oracle texts and their
    // divergence register moved here from `tools/workflow/src` (§8.1 forbids a
    // command crate depending on a tool crate, and the `workflow-authoring`
    // bundled skill reads the same texts). Without this root they would have
    // left the scan surface entirely — the exact asymmetry the SKIPPED_EXTENSIONS
    // note below describes, and the STALE entries were the only thing that said so.
    "workflow/src",
];

/// File extensions that are NOT scanned — a DENY-list, deliberately, so that
/// "deny-by-default" holds for file TYPES the way it already holds for files.
///
/// This used to be an allow-list (`["rs", "js"]`), and that was a hole INSIDE
/// the declared roots: `tools/workflow/src` also holds
/// `workflow_description.txt` (the Workflow tool's own description, 19 KB of
/// text handed straight to the model) and `workflow_input_schema.json`, both
/// `include_str!`ed into production (the description text now from
/// `workflow/src/description.rs`, the input schema from
/// `tools/workflow/src/lib.rs`).
/// The rationale that put `.js` on the allow-list — "production source that
/// merely happens not to be Rust", and §19.3 stating its criterion over the
/// DIRECTORY — applies to those two verbatim, yet a component name planted in
/// either was invisible. Worse, it was invisible ASYMMETRICALLY: moving prose
/// out of an allowlisted `.js` script into the tool-description text fired
/// STALE for the departure while the arrival went unreported.
///
/// So an UNKNOWN extension (and a file with no extension at all) is SCANNED.
/// Only the entries below — image / font / archive / compiled-artifact / media
/// / database formats, none of which can be a UTF-8 source file — are skipped.
/// A file that survives this filter but is not valid UTF-8 makes
/// [`scan_tree`] panic BY NAME rather than be silently dropped.
const SKIPPED_EXTENSIONS: &[&str] = &[
    // images
    "png", "jpg", "jpeg", "gif", "webp", "bmp", "tif", "tiff", "ico", "icns", "avif", //
    // fonts
    "ttf", "otf", "woff", "woff2", "eot", //
    // archives / compressed
    "zip", "gz", "tgz", "bz2", "xz", "zst", "7z", "tar", "rar", //
    // compiled artifacts
    "so", "dylib", "dll", "a", "o", "rlib", "rmeta", "exe", "bin", "wasm", "class", "jar", "pdb",
    "pyc", //
    // media
    "mp3", "mp4", "m4a", "mov", "wav", "ogg", "webm", "avi", "flac", //
    // documents / databases
    "pdf", "db", "sqlite", "sqlite3", "bru",
];

/// Files skipped by NAME. Only OS/editor droppings that are never source and
/// are never valid UTF-8 — kept separate from [`SKIPPED_EXTENSIONS`] because
/// they carry no extension at all and would otherwise turn a stray Finder visit
/// into a red gate for a reason that has nothing to do with component names.
const SKIPPED_FILE_NAMES: &[&str] = &[".DS_Store", "Thumbs.db"];

/// True when `path`'s extension is one this scan deliberately ignores, or its
/// file name is one of [`SKIPPED_FILE_NAMES`]. Everything else is scanned.
fn is_skipped_file(path: &Path) -> bool {
    if path
        .file_name()
        .and_then(|n| n.to_str())
        .is_some_and(|name| SKIPPED_FILE_NAMES.contains(&name))
    {
        return true;
    }
    path.extension()
        .and_then(|e| e.to_str())
        .map(str::to_ascii_lowercase)
        .is_some_and(|ext| SKIPPED_EXTENSIONS.contains(&ext.as_str()))
}

/// True for a Rust source path. Rust-only lexical machinery
/// ([`test_skip_ranges`] and the out-of-line `mod` resolution) must not be run
/// over other file types, where its constructs do not exist and its patterns
/// can only ever produce false positives.
fn is_rust_source(path: &Path) -> bool {
    path.extension()
        .and_then(|e| e.to_str())
        .is_some_and(|e| e.eq_ignore_ascii_case("rs"))
}

fn manifest_dir() -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR"))
}

/// `harness-runtime` → the cargo workspace root (`lingxi-code/`).
fn workspace_root() -> PathBuf {
    manifest_dir()
        .parent()
        .expect("harness-runtime is a workspace member")
        .to_path_buf()
}

/// The absolute scan roots. Panics if any of them is missing: a scan root that
/// silently resolves to nothing scans nothing and is green forever.
fn scan_roots() -> Vec<PathBuf> {
    let base = workspace_root();
    SCAN_ROOTS
        .iter()
        .map(|rel| {
            let path = base.join(rel);
            assert!(
                path.is_dir(),
                "scan root `{rel}` does not exist at {} — a missing root scans \
                 NOTHING and would make this gate green for the wrong reason. \
                 If the directory moved, update SCAN_ROOTS in this file.",
                path.display()
            );
            path
        })
        .collect()
}

fn allowlist_file_path() -> PathBuf {
    manifest_dir().join("tests/mobile/component_literal_allowlist.txt")
}

// ---------------------------------------------------------------------------
// Needle derivation (from the LIVE registries, not a static array)
// ---------------------------------------------------------------------------

/// Extract every registered skill's basename from a live [`skill_api::SkillRegistry`]
/// instance.
fn skill_basenames(reg: &skill_api::SkillRegistry) -> BTreeSet<String> {
    reg.names()
        .into_iter()
        .filter(|name| is_local_app_component_skill(name))
        .map(str::to_owned)
        .collect()
}

fn is_local_app_component_skill(name: &str) -> bool {
    name.contains("local-app")
        || name.ends_with("-local-app")
        || name.starts_with("mcp-")
        || matches!(
            name,
            "accessibility"
                | "frontend-design"
                | "frontend-qa"
                | "react-best-practices"
                | "expose-as-mcp"
        )
}

/// The Local App plugin workflow basenames. Phase 9 removed the standalone
/// core workflows, so the scan seeds now come from the plugin-owned workflow
/// identities rather than `tool_workflow::BUILTIN_WORKFLOWS`.
fn local_app_workflow_basenames() -> BTreeSet<String> {
    [
        "lingxi-local-app:local-app-build",
        "lingxi-local-app:local-app-use-test",
        "lingxi-local-app:local-app-mcp-authoring",
    ]
    .into_iter()
    .filter_map(|name| name.rsplit(':').next())
    .map(str::to_owned)
    .collect()
}

/// Double every basename into its namespaced FQN (`lingxi-local-app:<name>`)
/// and return the union of both spellings.
fn expand_with_namespace(basenames: &BTreeSet<String>) -> BTreeSet<String> {
    let mut needles = basenames.clone();
    for name in basenames {
        needles.insert(format!("{PLUGIN_NAMESPACE}:{name}"));
    }
    needles
}

/// The WHOLE needle derivation, parameterised on the skill registry it reads.
///
/// [`production_needle_set`] is nothing but this function applied to the real
/// registry. `scanner_needle_comes_from_discovery_not_from_a_static_array`
/// covers narrowing HERE by running `needle_set_from(&grown)` through the whole
/// scan; narrowing one level UP, inside [`production_needle_set`] itself, is
/// covered by the superset assertion in that same test — see its doc comment
/// for why the first check alone was not enough.
fn needle_set_from(reg: &skill_api::SkillRegistry) -> BTreeSet<String> {
    let mut basenames = skill_basenames(reg);
    basenames.extend(local_app_workflow_basenames());
    expand_with_namespace(&basenames)
}

/// The full needle set the production scan runs with: every Local App
/// component skill basename and Local App workflow basename, plus each one's
/// namespaced FQN.
fn production_needle_set() -> BTreeSet<String> {
    let mut basenames = harness_runtime::mobile::mobile_plugin_skill_names()
        .into_iter()
        .filter(|name| is_local_app_component_skill(name))
        .collect::<BTreeSet<_>>();
    basenames.extend(local_app_workflow_basenames());
    expand_with_namespace(&basenames)
}

// ---------------------------------------------------------------------------
// Lexical view: blank out string / char / comment content so brace tracking
// and attribute detection cannot be fooled by source text that merely LOOKS
// like code (a `}` at column 0 inside a `br#"{ ... }"#` literal, say).
// ---------------------------------------------------------------------------

/// Per-line view of `src` with every byte that belongs to a comment, a string
/// literal (including raw and byte strings), or a char literal replaced by a
/// space. Newlines are preserved so line numbering is unchanged.
fn code_view(src: &str) -> Vec<String> {
    let chars: Vec<char> = src.chars().collect();
    let n = chars.len();
    let mut out = chars.clone();
    let blank = |out: &mut Vec<char>, from: usize, to: usize| {
        for (k, slot) in out.iter_mut().enumerate().take(to.min(n)).skip(from) {
            if chars[k] != '\n' {
                *slot = ' ';
            }
        }
    };

    let mut i = 0;
    while i < n {
        let c = chars[i];
        // Line comment.
        if c == '/' && i + 1 < n && chars[i + 1] == '/' {
            let mut j = i;
            while j < n && chars[j] != '\n' {
                j += 1;
            }
            blank(&mut out, i, j);
            i = j;
            continue;
        }
        // Block comment (Rust nests them).
        if c == '/' && i + 1 < n && chars[i + 1] == '*' {
            let mut depth = 1usize;
            let mut j = i + 2;
            while j < n && depth > 0 {
                if chars[j] == '/' && j + 1 < n && chars[j + 1] == '*' {
                    depth += 1;
                    j += 2;
                    continue;
                }
                if chars[j] == '*' && j + 1 < n && chars[j + 1] == '/' {
                    depth -= 1;
                    j += 2;
                    continue;
                }
                j += 1;
            }
            blank(&mut out, i, j);
            i = j;
            continue;
        }
        // Raw / raw-byte string: `r"..."`, `r#"..."#`, `br##"..."##`.
        if (c == 'r' || (c == 'b' && i + 1 < n && chars[i + 1] == 'r'))
            && !(i > 0 && (chars[i - 1].is_alphanumeric() || chars[i - 1] == '_'))
        {
            let mut k = if c == 'b' { i + 2 } else { i + 1 };
            let hash_start = k;
            while k < n && chars[k] == '#' {
                k += 1;
            }
            if k < n && chars[k] == '"' {
                let hashes = k - hash_start;
                let mut j = k + 1;
                let mut end = n;
                while j < n {
                    if chars[j] == '"' {
                        let mut h = 0;
                        while h < hashes && j + 1 + h < n && chars[j + 1 + h] == '#' {
                            h += 1;
                        }
                        if h == hashes {
                            end = j + 1 + hashes;
                            break;
                        }
                    }
                    j += 1;
                }
                blank(&mut out, i, end);
                i = end;
                continue;
            }
        }
        // Ordinary (or byte) string literal.
        if c == '"' {
            let mut j = i + 1;
            while j < n {
                if chars[j] == '\\' {
                    j += 2;
                    continue;
                }
                if chars[j] == '"' {
                    j += 1;
                    break;
                }
                j += 1;
            }
            blank(&mut out, i, j.min(n));
            i = j.min(n);
            continue;
        }
        // Char literal vs lifetime: `'}'` must not be read as a brace, while
        // `'a` in `&'a str` must not swallow the rest of the file.
        if c == '\'' {
            if i + 1 < n && chars[i + 1] == '\\' {
                let mut j = i + 2;
                while j < n && chars[j] != '\'' {
                    j += 1;
                }
                let end = (j + 1).min(n);
                blank(&mut out, i, end);
                i = end;
                continue;
            }
            if i + 2 < n && chars[i + 2] == '\'' {
                blank(&mut out, i, i + 3);
                i += 3;
                continue;
            }
            i += 1;
            continue;
        }
        i += 1;
    }

    out.into_iter()
        .collect::<String>()
        .lines()
        .map(str::to_owned)
        .collect()
}

/// Compute the 1-based, INCLUSIVE line ranges this file's INLINE `#[cfg(test)]`
/// items occupy.
///
/// A range is opened ONLY when the line following the attribute ends in `{` —
/// i.e. the annotated item actually opens a block. Everything else
/// (`#[cfg(test)] use std::io;`, `mod foo;`, `const`, `type`, a multi-line `fn`
/// signature) skips the attribute line ALONE.
///
/// This is the whole point of the rewrite: the previous implementation looked
/// forward for the first line whose content was exactly `<indent>}`, so for a
/// `#[cfg(test)]` on a non-brace item that line was the NEXT item's closing
/// brace and the skip swallowed every line of production code in between.
/// Two lines (`#[cfg(test)]` + `use std::io;`) dropped anywhere in a module
/// were enough to hide an arbitrary span from the scan. The idiom is live in
/// this workspace (`apps/cli/src/commands/plugin_prune.rs:39`,
/// `apps/cli/src/stream_json_input.rs:45`, `tools/web/src/lib.rs:23`), so this
/// was not a theoretical shape.
///
/// The end of an opened range is found by BRACE DEPTH over [`code_view`], not
/// by string-matching a closing line, so a `}` at column 0 inside a raw string
/// (`local_apps_mcp.rs`'s `br#"{ ... }"#` fixtures) can no longer terminate the
/// range hundreds of lines early and leave real test code being scanned as
/// production.
fn test_skip_ranges(src: &str) -> Vec<(usize, usize)> {
    let code = code_view(src);
    let mut ranges = Vec::new();
    let mut i = 0;
    while i < code.len() {
        if code[i].trim() != "#[cfg(test)]" {
            i += 1;
            continue;
        }
        let start_line = i + 1; // 1-based
        let opens_a_block = code
            .get(i + 1)
            .is_some_and(|next| next.trim_end().ends_with('{'));
        if !opens_a_block {
            ranges.push((start_line, start_line));
            i += 1;
            continue;
        }
        let mut depth: i64 = 0;
        let mut close_idx = None;
        let mut j = i + 1;
        while j < code.len() {
            for ch in code[j].chars() {
                match ch {
                    '{' => depth += 1,
                    '}' => depth -= 1,
                    _ => {}
                }
            }
            if depth <= 0 {
                close_idx = Some(j);
                break;
            }
            j += 1;
        }
        match close_idx {
            Some(close_idx) => {
                ranges.push((start_line, close_idx + 1));
                i = close_idx + 1;
            }
            None => {
                // Unbalanced source: skip to EOF rather than guess.
                ranges.push((start_line, code.len()));
                break;
            }
        }
    }
    ranges
}

fn line_is_skipped(line_no: usize, ranges: &[(usize, usize)]) -> bool {
    ranges
        .iter()
        .any(|(start, end)| line_no >= *start && line_no <= *end)
}

// ---------------------------------------------------------------------------
// Out-of-line `#[cfg(test)] mod x;` resolution
// ---------------------------------------------------------------------------

/// One `mod <name>;` declaration (out-of-line only — `mod x { .. }` is handled
/// by the inline skip ranges).
#[derive(Debug, Clone)]
struct ModDecl {
    /// Candidate files this declaration could resolve to.
    candidates: Vec<PathBuf>,
    cfg_test: bool,
}

/// Strip a leading visibility qualifier from a trimmed source line.
fn strip_visibility(line: &str) -> &str {
    let line = line.trim();
    if let Some(rest) = line.strip_prefix("pub") {
        let rest = rest.trim_start();
        if let Some(inner) = rest.strip_prefix('(') {
            if let Some(close) = inner.find(')') {
                return inner[close + 1..].trim_start();
            }
            return rest;
        }
        return rest;
    }
    line
}

/// `mod foo;` → `Some("foo")`. Anything else → `None`.
fn parse_out_of_line_mod(line: &str) -> Option<&str> {
    let rest = strip_visibility(line).strip_prefix("mod")?;
    let rest = rest.trim();
    let name = rest.strip_suffix(';')?.trim();
    if name.is_empty()
        || !name.chars().all(|c| c.is_ascii_alphanumeric() || c == '_')
        || name.chars().next().is_some_and(|c| c.is_ascii_digit())
    {
        return None;
    }
    Some(name)
}

/// Where `mod <name>;` declared inside `file` can live on disk.
fn mod_candidates(file: &Path, name: &str, path_attr: Option<&str>) -> Vec<PathBuf> {
    let dir = file.parent().unwrap_or(Path::new("."));
    if let Some(rel) = path_attr {
        return vec![dir.join(rel)];
    }
    let stem = file
        .file_stem()
        .map(|s| s.to_string_lossy().into_owned())
        .unwrap_or_default();
    let base = if matches!(stem.as_str(), "lib" | "main" | "mod") {
        dir.to_path_buf()
    } else {
        dir.join(&stem)
    };
    vec![
        base.join(format!("{name}.rs")),
        base.join(name).join("mod.rs"),
    ]
}

/// Collect every out-of-line `mod` declaration in `src`, remembering whether it
/// carried `#[cfg(test)]`.
fn out_of_line_mod_decls(src: &str, file: &Path) -> Vec<ModDecl> {
    let mut decls = Vec::new();
    let mut cfg_test = false;
    let mut path_attr: Option<String> = None;
    for raw in src.lines() {
        let line = raw.trim();
        if line.is_empty() || line.starts_with("//") {
            continue;
        }
        if line == "#[cfg(test)]" {
            cfg_test = true;
            continue;
        }
        if line.starts_with("#[path") {
            let open = line.find('"');
            let close = line.rfind('"');
            if let (Some(a), Some(b)) = (open, close) {
                if b > a {
                    path_attr = Some(line[a + 1..b].to_string());
                }
            }
            continue;
        }
        if line.starts_with("#[") || line.starts_with("#![") {
            continue;
        }
        if let Some(name) = parse_out_of_line_mod(line) {
            decls.push(ModDecl {
                candidates: mod_candidates(file, name, path_attr.as_deref()),
                cfg_test,
            });
        }
        cfg_test = false;
        path_attr = None;
    }
    decls
}

/// Files that are reached ONLY through `#[cfg(test)]` `mod` declarations.
///
/// The `ONLY` is load-bearing: a file is excluded when it has at least one
/// declaration pointing at it AND every such declaration is test-gated. So
/// dropping an extra `#[cfg(test)] #[path = "local_apps_host.rs"] mod evil;`
/// somewhere does NOT remove `local_apps_host.rs` from the scan — the real,
/// ungated `mod local_apps_host;` still counts.
fn cfg_test_only_module_files(files: &[PathBuf]) -> BTreeSet<PathBuf> {
    let present: BTreeSet<&PathBuf> = files.iter().collect();
    let mut refs: BTreeMap<PathBuf, (usize, usize)> = BTreeMap::new();
    for file in files {
        if !is_rust_source(file) {
            continue;
        }
        let Ok(src) = fs::read_to_string(file) else {
            continue;
        };
        for decl in out_of_line_mod_decls(&src, file) {
            for candidate in decl.candidates {
                if present.contains(&candidate) {
                    let slot = refs.entry(candidate).or_insert((0, 0));
                    slot.0 += 1;
                    if decl.cfg_test {
                        slot.1 += 1;
                    }
                }
            }
        }
    }
    refs.into_iter()
        .filter(|(_, (total, cfg))| *total > 0 && total == cfg)
        .map(|(path, _)| path)
        .collect()
}

// ---------------------------------------------------------------------------
// Scan surface: deny-by-default directory enumeration (design doc §8.5:
// "扫描面是 deny-by-default 的目录枚举，不是文件白名单；新增 harness-runtime::mobile
// 模块默认被扫").
// ---------------------------------------------------------------------------

fn collect_source_files(dir: &Path, out: &mut Vec<PathBuf>) {
    let entries =
        fs::read_dir(dir).unwrap_or_else(|e| panic!("read_dir({}) failed: {e}", dir.display()));
    let mut paths: Vec<PathBuf> = entries.map(|e| e.expect("dir entry").path()).collect();
    paths.sort();
    for path in paths {
        if path.is_dir() {
            collect_source_files(&path, out);
        } else if !is_skipped_file(&path) {
            out.push(path);
        }
    }
}

// ---------------------------------------------------------------------------
// Matching
// ---------------------------------------------------------------------------

/// One occurrence of a needle in production source.
#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord)]
struct Violation {
    /// Path relative to the workspace root, forward-slash separated.
    rel_path: String,
    /// 1-based source line where the match STARTS.
    line: usize,
    /// The exact needle matched.
    literal: String,
    /// True when this occurrence exists only in the normalized view — i.e. the
    /// name is assembled out of fragments (`concat!`, a wrapped string literal)
    /// rather than written out on one line.
    split: bool,
}

impl std::fmt::Display for Violation {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(
            f,
            "{}:{}: literal `{}`",
            self.rel_path, self.line, self.literal
        )?;
        if self.split {
            write!(
                f,
                " (SPLIT literal — assembled from fragments; matched only after \
                 normalization)"
            )?;
        }
        Ok(())
    }
}

fn is_component_name_char(ch: char) -> bool {
    ch.is_ascii_alphanumeric() || matches!(ch, '_' | '-')
}

/// Bare component basenames are common English-like substrings (`local-app-run`
/// is a prefix of `local-app-runtime`), so accept them only as complete name
/// tokens. Plugin-qualified FQNs are unambiguous identities and remain exact
/// substring needles; [`keep_longest`] reports the FQN instead of its trailing
/// basename when both are present.
fn has_component_name_boundaries(hay: &str, start: usize, end: usize, needle: &str) -> bool {
    if needle.contains(':') {
        return true;
    }
    let before = hay[..start].chars().next_back();
    let after = hay[end..].chars().next();
    before.is_none_or(|ch| !is_component_name_char(ch))
        && after.is_none_or(|ch| !is_component_name_char(ch))
}

/// Every `(start, end, needle)` span of a needle in `hay`.
fn find_spans(hay: &str, needles: &BTreeSet<String>) -> Vec<(usize, usize, String)> {
    let mut spans = Vec::new();
    for needle in needles {
        let mut from = 0;
        while let Some(pos) = hay[from..].find(needle.as_str()) {
            let start = from + pos;
            let end = start + needle.len();
            if has_component_name_boundaries(hay, start, end, needle) {
                spans.push((start, end, needle.clone()));
            }
            from = start + 1;
        }
    }
    spans
}

/// Keep only the LONGEST match at each starting position (so a namespaced-FQN
/// occurrence — which always contains its own basename as a trailing substring
/// — is reported once, as the FQN, not twice).
fn keep_longest(mut spans: Vec<(usize, usize, String)>) -> Vec<(usize, usize, String)> {
    spans.sort();
    spans.dedup();
    spans.sort_by(|a, b| a.0.cmp(&b.0).then(b.1.cmp(&a.1)));
    let mut kept: Vec<(usize, usize, String)> = Vec::new();
    for span in spans {
        let covered = kept
            .iter()
            .any(|(ks, ke, _)| span.0 >= *ks && span.1 <= *ke);
        if !covered {
            kept.push(span);
        }
    }
    kept.sort();
    kept
}

/// Find every needle occurrence on one line (raw view).
fn matches_on_line(line: &str, needles: &BTreeSet<String>) -> Vec<String> {
    keep_longest(find_spans(line, needles))
        .into_iter()
        .map(|(_, _, needle)| needle)
        .collect()
}

/// Characters removed when building the normalized view. `concat!(` is removed
/// as a token in a second pass (its `(` is deliberately NOT stripped globally,
/// so ordinary call syntax still separates identifiers).
///
/// `+` is in here because `"local-app".to_string() + "-build"` evades a
/// per-line substring match exactly the way `concat!` does. Stripping `+` is
/// necessary but NOT sufficient for that shape: `.to_string(` survives
/// character stripping and would still sit between the two fragments, which is
/// why it is in [`STRIPPED_TOKENS`] below.
fn is_stripped(ch: char) -> bool {
    matches!(ch, '"' | '\\' | ',' | ')' | '+') || ch.is_whitespace()
}

/// Token sequences removed from the normalized view in a second pass, matched
/// against the character stream [`is_stripped`] has already thinned (so
/// `concat! (` and `. to_string ()` are covered as well as the tight spellings).
///
/// Every entry is a string-assembly NO-OP: it sits between two fragments of a
/// name without contributing a character to the string that results. Their `(`
/// is removed only as part of the token — `(` is deliberately NOT stripped
/// globally, so ordinary call syntax still separates identifiers.
const STRIPPED_TOKENS: &[&str] = &[
    "concat!(",
    "String::from(",
    ".to_string(",
    ".to_owned(",
    ".as_str(",
    ".as_ref(",
    ".into(",
];

/// The normalized whole-file view plus, for each BYTE of it, the byte offset it
/// came from in the original source.
fn normalized_view(src: &str) -> (String, Vec<usize>) {
    let mut kept: Vec<(char, usize)> = Vec::new();
    for (offset, ch) in src.char_indices() {
        if is_stripped(ch) {
            continue;
        }
        kept.push((ch, offset));
    }
    let tokens: Vec<Vec<char>> = STRIPPED_TOKENS
        .iter()
        .map(|t| t.chars().collect::<Vec<char>>())
        .collect();
    let mut text = String::new();
    let mut map: Vec<usize> = Vec::new();
    let mut i = 0;
    while i < kept.len() {
        // Longest match wins, so a token that is a prefix of another cannot
        // shadow it.
        let token_len = tokens
            .iter()
            .filter(|tok| {
                i + tok.len() <= kept.len() && (0..tok.len()).all(|k| kept[i + k].0 == tok[k])
            })
            .map(Vec::len)
            .max();
        if let Some(len) = token_len {
            i += len;
            continue;
        }
        let (ch, offset) = kept[i];
        let before = text.len();
        text.push(ch);
        for _ in before..text.len() {
            map.push(offset);
        }
        i += 1;
    }
    (text, map)
}

/// Byte offset of the start of every line, so a match offset maps to a line.
fn line_start_offsets(src: &str) -> Vec<usize> {
    let mut starts = vec![0usize];
    for (offset, ch) in src.char_indices() {
        if ch == '\n' {
            starts.push(offset + 1);
        }
    }
    starts
}

fn line_of(offset: usize, starts: &[usize]) -> usize {
    starts.partition_point(|start| *start <= offset)
}

/// Scan ONE file's contents through both views and return its occurrences.
fn scan_text(rel_path: &str, src: &str, needles: &BTreeSet<String>) -> Vec<Violation> {
    // `#[cfg(test)]` is a RUST construct. Running the skip-range detector over a
    // non-Rust file could only ever hide production text: [`code_view`] lexes
    // Rust comments and Rust string literals, not JS template literals or regex
    // literals, so a `.js` template literal containing a line that trims to
    // exactly `#[cfg(test)]` followed by a line ending in `{` would open a skip
    // range in a file that has no such concept. Not running the detector there
    // removes the class instead of patching one instance of it.
    let skip_ranges = if is_rust_source(Path::new(rel_path)) {
        test_skip_ranges(src)
    } else {
        Vec::new()
    };
    let starts = line_start_offsets(src);

    // View 1 — raw, line by line.
    let mut raw_spans: Vec<(usize, usize, String)> = Vec::new();
    for (idx, line) in src.lines().enumerate() {
        let offset = starts.get(idx).copied().unwrap_or(0);
        for (s, e, needle) in find_spans(line, needles) {
            raw_spans.push((offset + s, offset + e, needle));
        }
    }
    let raw_set: BTreeSet<(usize, usize, String)> = raw_spans.iter().cloned().collect();

    // View 2 — normalized, across line boundaries.
    let (normalized, map) = normalized_view(src);
    let mut norm_spans: Vec<(usize, usize, String)> = Vec::new();
    for (s, e, needle) in find_spans(&normalized, needles) {
        if e == 0 || e > map.len() {
            continue;
        }
        // Needles are ASCII, so the last matched byte is a whole character.
        norm_spans.push((map[s], map[e - 1] + 1, needle));
    }

    let mut all = raw_spans;
    all.extend(norm_spans);
    let mut violations = Vec::new();
    for (start, end, literal) in keep_longest(all) {
        let line = line_of(start, &starts);
        if line_is_skipped(line, &skip_ranges) {
            continue;
        }
        violations.push(Violation {
            rel_path: rel_path.to_string(),
            line,
            literal: literal.clone(),
            split: !raw_set.contains(&(start, end, literal)),
        });
    }
    violations
}

/// Scan every source file under `root` for `needles`, skipping `#[cfg(test)]`
/// regions and out-of-line test modules. Reported paths are relative to
/// `rel_base`.
fn scan_tree(root: &Path, rel_base: &Path, needles: &BTreeSet<String>) -> Vec<Violation> {
    assert!(
        root.is_dir(),
        "scan root {} does not exist — refusing to scan nothing",
        root.display()
    );
    let mut files = Vec::new();
    collect_source_files(root, &mut files);
    let excluded = cfg_test_only_module_files(&files);
    let mut violations = Vec::new();
    for file in files {
        if excluded.contains(&file) {
            continue;
        }
        let src = fs::read_to_string(&file).unwrap_or_else(|e| {
            panic!(
                "read {} failed: {e}. The extension filter is a DENY-list: an \
                 unknown extension is scanned, so a NEW binary file type under \
                 a scan root shows up here as a hard failure NAMING the file \
                 rather than as a silently-skipped hole. If this file genuinely \
                 cannot contain a component name, add its extension to \
                 SKIPPED_EXTENSIONS in this file, in the same diff.",
                file.display()
            )
        });
        let rel_path = file
            .strip_prefix(rel_base)
            .unwrap_or(&file)
            .to_string_lossy()
            .replace('\\', "/");
        violations.extend(scan_text(&rel_path, &src, needles));
    }
    violations.sort();
    violations
}

/// Scan a single standalone tree (used by the planted-fixture tests), with
/// paths relative to that tree.
fn scan_source_tree(root: &Path, needles: &BTreeSet<String>) -> Vec<Violation> {
    scan_tree(root, root, needles)
}

/// The real production scan: all [`SCAN_ROOTS`], paths relative to the
/// workspace root.
fn scan_production_trees(needles: &BTreeSet<String>) -> Vec<Violation> {
    let base = workspace_root();
    let mut violations = Vec::new();
    for root in scan_roots() {
        violations.extend(scan_tree(&root, &base, needles));
    }
    violations.sort();
    violations
}

// ---------------------------------------------------------------------------
// Allowlist
// ---------------------------------------------------------------------------

/// One `path:literal:expected_count` allowlist entry. Matched by EXACT
/// equality on both `rel_path` and `literal` — never a prefix, never a glob,
/// never a directory — and the occurrence count must match exactly.
#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord)]
struct AllowlistEntry {
    rel_path: String,
    literal: String,
    expected: usize,
}

impl std::fmt::Display for AllowlistEntry {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "{}:{}:{}", self.rel_path, self.literal, self.expected)
    }
}

/// Parse the allowlist file: one `path:literal:expected_count` per non-comment,
/// non-blank line. `#` starts a whole-line comment (only at line start, after
/// trimming) so header prose can live in the same file as the entries it
/// explains.
///
/// `path` never contains `:` and `expected_count` is a bare integer, while the
/// literal MAY contain one (`lingxi-local-app:local-app-build`), so the split
/// is: everything before the FIRST colon is the path, everything after the LAST
/// colon is the count, and what remains in between is the literal.
fn parse_allowlist(text: &str) -> Vec<AllowlistEntry> {
    let mut entries = Vec::new();
    for (idx, raw_line) in text.lines().enumerate() {
        let line = raw_line.trim();
        if line.is_empty() || line.starts_with('#') {
            continue;
        }
        let no = idx + 1;
        let (path, rest) = line
            .split_once(':')
            .unwrap_or_else(|| panic!("allowlist line {no}: expected `path:literal:count`"));
        let (literal, count) = rest
            .rsplit_once(':')
            .unwrap_or_else(|| panic!("allowlist line {no}: expected `path:literal:count`"));
        let expected: usize = count
            .trim()
            .parse()
            .unwrap_or_else(|e| panic!("allowlist line {no}: bad expected_count `{count}`: {e}"));
        assert!(
            !path.is_empty() && !literal.is_empty(),
            "allowlist line {no}: empty path or literal in `{line}`"
        );
        assert!(
            expected > 0,
            "allowlist line {no}: expected_count must be > 0 — an entry that \
             permits zero occurrences is a stale hole, delete it instead"
        );
        entries.push(AllowlistEntry {
            rel_path: path.to_string(),
            literal: literal.to_string(),
            expected,
        });
    }
    entries
}

fn load_allowlist() -> Vec<AllowlistEntry> {
    let path = allowlist_file_path();
    let text = fs::read_to_string(&path)
        .unwrap_or_else(|e| panic!("read allowlist {}: {e}", path.display()));
    parse_allowlist(&text)
}

/// A single reason the scan and the allowlist disagree.
#[derive(Debug, Clone, PartialEq, Eq)]
enum Finding {
    /// An occurrence in a `(path, literal)` group the allowlist never mentions.
    Leak(Violation),
    /// The allowlist permits this `(path, literal)` but the count moved.
    CountMismatch {
        entry: AllowlistEntry,
        found: Vec<Violation>,
    },
    /// The allowlist permits a `(path, literal)` that no longer occurs at all.
    Stale(AllowlistEntry),
    /// Two entries for the same `(path, literal)`.
    DuplicateEntry(AllowlistEntry),
}

impl std::fmt::Display for Finding {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Finding::Leak(v) => write!(f, "LEAK           {v}"),
            Finding::CountMismatch { entry, found } => {
                write!(
                    f,
                    "COUNT MISMATCH {}:{} — allowlist expects {} occurrence(s), \
                     source has {}: [{}]",
                    entry.rel_path,
                    entry.literal,
                    entry.expected,
                    found.len(),
                    found
                        .iter()
                        .map(|v| {
                            if v.split {
                                format!("line {} (SPLIT literal)", v.line)
                            } else {
                                format!("line {}", v.line)
                            }
                        })
                        .collect::<Vec<_>>()
                        .join(", ")
                )
            }
            Finding::Stale(entry) => write!(
                f,
                "STALE ENTRY    {entry} — the allowlist still pre-approves \
                 `{}` in `{}`, but the scan found NO such occurrence. Either \
                 the file dropped out of the scan surface (which would hide \
                 every future leak in it), or the code moved and the entry \
                 must be deleted in the same diff.",
                entry.literal, entry.rel_path
            ),
            Finding::DuplicateEntry(entry) => write!(
                f,
                "DUPLICATE      {entry} — two entries for the same \
                 path:literal; counts must live in ONE entry"
            ),
        }
    }
}

/// Reconcile the scan against the allowlist in BOTH directions.
///
/// A permitted-superset allowlist (filter only) would let P-1.3/P-1.4/P-1.5
/// delete the very occurrences these entries point at and stay green with a
/// pre-approved hole left behind, and would let a file quietly drop out of the
/// walk without anything noticing — the real-tree scan only ever sees FEWER
/// violations in that case. So an entry that over- or under-counts, or matches
/// nothing at all, is itself a failure.
fn reconcile(violations: &[Violation], allowlist: &[AllowlistEntry]) -> Vec<Finding> {
    let mut findings = Vec::new();

    let mut by_key: BTreeMap<(String, String), &AllowlistEntry> = BTreeMap::new();
    for entry in allowlist {
        let key = (entry.rel_path.clone(), entry.literal.clone());
        if by_key.insert(key, entry).is_some() {
            findings.push(Finding::DuplicateEntry(entry.clone()));
        }
    }

    let mut groups: BTreeMap<(String, String), Vec<Violation>> = BTreeMap::new();
    for v in violations {
        groups
            .entry((v.rel_path.clone(), v.literal.clone()))
            .or_default()
            .push(v.clone());
    }

    for (key, found) in &groups {
        match by_key.get(key) {
            None => findings.extend(found.iter().cloned().map(Finding::Leak)),
            Some(entry) if entry.expected != found.len() => {
                findings.push(Finding::CountMismatch {
                    entry: (*entry).clone(),
                    found: found.clone(),
                });
            }
            Some(_) => {}
        }
    }

    for (key, entry) in &by_key {
        if !groups.contains_key(key) {
            findings.push(Finding::Stale((*entry).clone()));
        }
    }

    findings
}

fn render(findings: &[Finding]) -> String {
    findings
        .iter()
        .map(|f| format!("  {f}"))
        .collect::<Vec<_>>()
        .join("\n")
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

/// The house rule: this is a PLANTED-POSITIVE gate, so it must itself be
/// proven capable of failing, not merely proven to pass today. This test IS
/// the real-tree half: it must currently be green against the checked-in
/// allowlist, and `/tmp/p1_0_fix_*.txt` (outside this repo) carries the planted
/// runs that show it goes red for each attack shape.
#[test]
fn today_the_real_src_tree_matches_its_committed_allowlist() {
    let needles = production_needle_set();
    let violations = scan_production_trees(&needles);
    let findings = reconcile(&violations, &load_allowlist());
    assert!(
        findings.is_empty(),
        "component literal scan disagrees with its committed allowlist \
         ({} finding(s)):\n{}",
        findings.len(),
        render(&findings)
    );
}

/// (F2) — an allowlist entry is a PRE-APPROVED HOLE at a fixed location. If
/// the occurrence it points at is gone, the hole must go with it, in the same
/// diff. Without this, three separate regressions are invisible: a later phase
/// deleting the scanned code, a file dropping out of the directory walk, and
/// an entry that never matched anything in the first place.
#[test]
fn every_allowlist_entry_still_matches_a_real_occurrence() {
    let needles = production_needle_set();
    let violations = scan_production_trees(&needles);
    let stale: Vec<Finding> = reconcile(&violations, &load_allowlist())
        .into_iter()
        .filter(|f| matches!(f, Finding::Stale(_)))
        .collect();
    assert!(
        stale.is_empty(),
        "{} allowlist entr(y|ies) no longer match any occurrence in the \
         scanned source:\n{}",
        stale.len(),
        render(&stale)
    );
}

/// Every scan root must exist and actually contain source. A root that
/// silently resolves to nothing scans nothing, and every gate built on it is
/// green for the wrong reason.
#[test]
fn every_scan_root_exists_and_contains_source() {
    for (rel, root) in SCAN_ROOTS.iter().zip(scan_roots()) {
        let mut files = Vec::new();
        collect_source_files(&root, &mut files);
        assert!(
            !files.is_empty(),
            "scan root `{rel}` ({}) contains no scannable source file",
            root.display()
        );
    }
}

/// (D1) — the extension filter is a DENY-list, so a file type nobody thought
/// of is SCANNED, not skipped. Two files inside a declared scan root prove the
/// point concretely: `workflow/src/workflow_description.txt` is the
/// Workflow tool's own description — 19 KB of text `include_str!`ed at
/// `workflow/src/description.rs` and handed to the model — and
/// `workflow_input_schema.json` is `include_str!`ed at `:68`. Under the old
/// `SCANNED_EXTENSIONS = ["rs", "js"]` allow-list, planting a component name in
/// either left this gate green. A later phase moving workflow-selection prose
/// out of the allowlisted `local_app_build_workflow.js` and into the
/// description text is exactly the migration that would have exploited it: the
/// departure fires STALE, the arrival was invisible.
#[test]
fn known_include_str_data_files_are_inside_the_scan_surface() {
    let base = workspace_root();
    let mut files = Vec::new();
    for root in scan_roots() {
        collect_source_files(&root, &mut files);
    }
    for rel in [
        "workflow/src/workflow_description.txt",
        "workflow/src/workflow_authoring_skill.txt",
        "workflow/src/workflow_description_divergences.json",
        "tools/workflow/src/workflow_input_schema.json",
    ] {
        let path = base.join(rel);
        assert!(
            path.is_file(),
            "{rel} no longer exists; if it moved, this test must follow it \
             rather than be deleted"
        );
        assert!(
            files.contains(&path),
            "`{rel}` is include_str!ed into production and sits inside a \
             declared scan root, but the directory walk did NOT collect it. \
             The extension filter must stay a DENY-list (SKIPPED_EXTENSIONS); \
             an allow-list re-opens exactly this hole."
        );
    }
}

/// (D1) — the general form of the same rule, on file types this repository does
/// not contain today: an extension the scanner has never heard of must be
/// scanned, and a file with no extension at all must be scanned.
#[test]
fn a_literal_in_an_unknown_extension_is_reported() {
    let tmp = tempfile::tempdir().expect("tempdir");
    fs::write(
        tmp.path().join("tool_description.txt"),
        "Consult the threejs-local-app skill before starting.\n",
    )
    .expect("write .txt fixture");
    fs::write(
        tmp.path().join("schema.json"),
        "{\n  \"workflow\": \"local-app-use-test\"\n}\n",
    )
    .expect("write .json fixture");
    fs::write(
        tmp.path().join("prompt.mustache"),
        "surface uses phaser-2d-local-app\n",
    )
    .expect("write unknown-extension fixture");
    fs::write(tmp.path().join("PROMPT"), "run local-app-build now\n")
        .expect("write no-extension fixture");
    // …while a known-binary extension stays out of the walk.
    fs::write(tmp.path().join("logo.png"), "local-app-build\n").expect("write .png fixture");

    let needles = production_needle_set();
    let violations = scan_source_tree(tmp.path(), &needles);
    let reported: BTreeSet<(String, String)> = violations
        .iter()
        .map(|v| (v.rel_path.clone(), v.literal.clone()))
        .collect();

    for (file, literal) in [
        ("tool_description.txt", "threejs-local-app"),
        ("schema.json", "local-app-use-test"),
        ("prompt.mustache", "phaser-2d-local-app"),
        ("PROMPT", "local-app-build"),
    ] {
        assert!(
            reported.contains(&(file.to_string(), literal.to_string())),
            "a `{literal}` literal planted in `{file}` was NOT reported — the \
             extension filter must be a deny-list, so an unknown (or absent) \
             extension is scanned. Got {violations:?}"
        );
    }
    assert!(
        !reported.iter().any(|(path, _)| path == "logo.png"),
        "a known-binary extension must stay out of the walk, got {violations:?}"
    );
}

/// (b) — plant the namespaced FQN, not the basename, and require it to be
/// caught and reported AS the FQN (not silently subsumed into a basename-only
/// hit, and not missed because the needle set never grew the `lingxi-local-app:`
/// spelling).
#[test]
fn scanner_rejects_namespaced_fqn_not_just_basename() {
    let tmp = tempfile::tempdir().expect("tempdir");
    let planted = format!("pub const PLANTED: &str = \"{PLUGIN_NAMESPACE}:local-app-build\";\n");
    fs::write(tmp.path().join("planted.rs"), planted).expect("write planted fixture");

    let needles = production_needle_set();
    assert!(
        needles.contains(&format!("{PLUGIN_NAMESPACE}:local-app-build")),
        "needle set must contain the namespaced FQN spelling, not just the basename"
    );

    let violations = scan_source_tree(tmp.path(), &needles);
    assert_eq!(
        violations.len(),
        1,
        "expected exactly one violation, got {violations:?}"
    );
    let v = &violations[0];
    assert_eq!(v.rel_path, "planted.rs");
    assert_eq!(
        v.literal,
        format!("{PLUGIN_NAMESPACE}:local-app-build"),
        "the reported literal must be the namespaced FQN itself, not just the \
         basename it contains as a substring — proving the scanner matches the \
         FQN spelling specifically, not only the shorter basename"
    );
}

/// (c) — a brand-new harness-runtime::mobile module, mentioned nowhere in the
/// allowlist, must be scanned by default (deny-by-default directory
/// enumeration, not a file allowlist) and must fail.
#[test]
fn scanner_rejects_a_literal_in_a_module_no_allowlist_mentions() {
    let tmp = tempfile::tempdir().expect("tempdir");
    // A brand-new module the allowlist has never heard of.
    fs::write(
        tmp.path().join("brand_new_module.rs"),
        "pub const NAME: &str = \"babylon-3d-local-app\";\n",
    )
    .expect("write brand-new module fixture");

    let needles = production_needle_set();
    let violations = scan_source_tree(tmp.path(), &needles);
    // The REAL, checked-in allowlist: it must not cover this file.
    let leaks: Vec<Finding> = reconcile(&violations, &load_allowlist())
        .into_iter()
        .filter(|f| matches!(f, Finding::Leak(_)))
        .collect();

    assert_eq!(
        leaks.len(),
        1,
        "a literal in a module the allowlist never mentions must be reported, got {leaks:?}"
    );
    let Finding::Leak(v) = &leaks[0] else {
        unreachable!()
    };
    assert_eq!(v.rel_path, "brand_new_module.rs");
    assert_eq!(v.line, 1);
    assert_eq!(v.literal, "babylon-3d-local-app");
}

/// (F5) — §8.5 names TWO bypasses, "拼 FQN" and "拆串". This is the second
/// one: a name assembled out of fragments must still be caught, and must be
/// reported as a SPLIT literal so a reader is not left hunting for a string
/// that does not literally exist on that line.
#[test]
fn scanner_rejects_a_split_literal_assembled_from_fragments() {
    let tmp = tempfile::tempdir().expect("tempdir");
    fs::write(
        tmp.path().join("split.rs"),
        "pub const A: &str = concat!(\"local-app\", \"-build\");\n\
         pub const B: &str =\n    \"local-app-use\\\n     -test\";\n",
    )
    .expect("write split fixture");

    let needles = production_needle_set();
    let violations = scan_source_tree(tmp.path(), &needles);

    let concat_hit = violations
        .iter()
        .find(|v| v.literal == "local-app-build")
        .unwrap_or_else(|| {
            panic!("concat!-assembled `local-app-build` was not caught: {violations:?}")
        });
    assert_eq!(concat_hit.line, 1);
    assert!(
        concat_hit.split,
        "a concat!-assembled name must be flagged as a split literal"
    );

    let wrapped_hit = violations
        .iter()
        .find(|v| v.literal == "local-app-use-test")
        .unwrap_or_else(|| {
            panic!("line-wrapped `local-app-use-test` was not caught: {violations:?}")
        });
    assert_eq!(
        wrapped_hit.line, 3,
        "a wrapped literal must be reported at the line the match STARTS on"
    );
    assert!(wrapped_hit.split);
}

/// (a) — the needle set must come from discovery, not a static array.
///
/// The earlier version of this test only called `skill_basenames` and asserted
/// the returned set grew, which proved nothing about the scanner. This one has
/// TWO halves, because narrowing can happen at two different levels and the
/// first half only reaches one of them:
///
/// 1. GROWTH THROUGH THE PIPELINE — a skill registered after `register_mobile`
///    must propagate through [`needle_set_from`] into [`scan_source_tree`] and
///    be REPORTED, and must NOT be reported by the real production needle set.
///    This covers narrowing inside [`needle_set_from`].
/// 2. `production_needle_set()` ⊇ `needle_set_from(&fresh_registry)` — because
///    half 1 never calls [`production_needle_set`] on the POSITIVE side, and
///    dropping the skill half INSIDE `production_needle_set` (one level above
///    `needle_set_from`) left half 1 green when measured. The gate as a whole
///    still went red, but only because all ten current mobile skill basenames
///    happen to have allowlist entries that then fire STALE. A skill registered
///    tomorrow has no such anchor, so filtering it out at that level would have
///    been silent. This half removes that dependency on the allowlist's current
///    contents.
#[test]
fn scanner_needle_comes_from_discovery_not_from_a_static_array() {
    let new_skill_name = "planted-extra-local-app-skill";
    let production_needles = production_needle_set();
    assert!(
        !production_needles.contains(new_skill_name),
        "fixture name must not already collide with a real bundled skill"
    );

    // Half 2, run first because it is the cheaper failure to read: whatever
    // the verified Plugin inventory derives must survive into the set the REAL
    // scan runs with.
    let fresh_skills: BTreeSet<String> = harness_runtime::mobile::mobile_plugin_skill_names()
        .into_iter()
        .filter(|name| is_local_app_component_skill(name))
        .collect();
    let workflows = local_app_workflow_basenames();
    let fresh_only_skills: Vec<&String> = fresh_skills
        .iter()
        .filter(|n| !workflows.contains(*n))
        .collect();
    assert!(
        !fresh_only_skills.is_empty(),
        "the verified Plugin inventory must contribute at least one skill basename that is \
         not also a workflow name, or the superset assertion below would be \
         satisfied by the workflow half alone and could not detect the skill \
         half being dropped"
    );
    let mut fresh_needles = fresh_skills.clone();
    fresh_needles.extend(workflows);
    let fresh_needles = expand_with_namespace(&fresh_needles);
    let missing: Vec<&String> = fresh_needles
        .iter()
        .filter(|n| !production_needles.contains(*n))
        .collect();
    assert!(
        missing.is_empty(),
        "production_needle_set() is NOT a superset of \
         needle_set_from(&fresh_registry): {} needle(s) the live registries \
         supply never reach the set the real scan runs with: {missing:?}. \
         Something between `needle_set_from` and `production_needle_set` is \
         intersecting, filtering or dropping needles — which would make every \
         occurrence of those names invisible to this gate.",
        missing.len()
    );

    let mut grown = skill_api::SkillRegistry::new();
    grown.register(skill_api::Skill {
        name: new_skill_name.to_string(),
        description: "planted for scanner_needle_comes_from_discovery test".to_string(),
        frontmatter: skill_api::SkillFrontmatter {
            name: new_skill_name.to_string(),
            description: "planted".to_string(),
            ..Default::default()
        },
        content: String::new(),
        source: skill_api::SkillSource::Bundled,
        loaded_from: skill_api::LoadedFrom::Bundled,
        plugin_id: None,
        file_path: "<planted-for-test>".into(),
    });
    let grown_needles = needle_set_from(&grown);

    let tmp = tempfile::tempdir().expect("tempdir");
    fs::write(
        tmp.path().join("grown.rs"),
        format!("pub const NAME: &str = \"{new_skill_name}\";\n"),
    )
    .expect("write grown fixture");

    // The scanner, run with needles derived from the GROWN registry, reports it…
    let grown_hits = scan_source_tree(tmp.path(), &grown_needles);
    assert_eq!(
        grown_hits.len(),
        1,
        "a skill registered into the discovery registry must propagate all the way \
         into the scan, got {grown_hits:?}"
    );
    assert_eq!(grown_hits[0].literal, new_skill_name);
    assert!(
        grown_needles.contains(&format!("{PLUGIN_NAMESPACE}:{new_skill_name}")),
        "the grown name must also gain its namespaced FQN spelling"
    );

    // …and the REAL production needle set, which knows nothing about it, does
    // not. Both halves matter: the first proves the derivation is live, the
    // second proves the first is not passing because the needle set already
    // contains everything.
    let production_hits = scan_source_tree(tmp.path(), &production_needles);
    assert!(
        production_hits.is_empty(),
        "production_needle_set() must not already contain the planted name, \
         got {production_hits:?}"
    );
    let base_needles = needle_set_from(&skill_api::SkillRegistry::new());
    assert!(grown_needles.len() > base_needles.len());
}

/// P-1.9: `tasks::LOCAL_APP_BUILD_WORKFLOWS` and
/// `tool_workflow::LOCAL_APP_BUILD_WORKFLOWS` -- the two hand-maintained name
/// arrays this module doc's "must not grow a hardcoded name array" rule was
/// written against -- are BOTH gone now. This pins that this file's own
/// needle derivation (`local_app_workflow_basenames`) survived that deletion
/// by reading the plugin-owned workflow identities instead of growing its own
/// copied list elsewhere -- which is exactly the forbidden resolution the
/// module doc calls out.
#[test]
fn scanner_workflow_needles_survive_the_name_list_deletion() {
    let workflows = local_app_workflow_basenames();
    assert_eq!(
        workflows,
        BTreeSet::from([
            "local-app-build".to_string(),
            "local-app-mcp-authoring".to_string(),
            "local-app-use-test".to_string(),
        ]),
        "the needle set must be exactly the current plugin workflow basenames -- no more, no fewer"
    );
    assert!(
        !workflows.contains("deep-research"),
        "deep-research has no Local App identity and must not be pulled in just because \
         the generic workflow registry also lists it"
    );

    // The needles this function derives must still drive the real scan: a
    // build-workflow basename planted in a fresh source tree is caught, using
    // ONLY the typed derivation (never a name literal written in this test).
    let needles = expand_with_namespace(&workflows);
    let tmp = tempfile::tempdir().expect("tempdir");
    for name in &workflows {
        fs::write(
            tmp.path().join(format!("{name}.rs")),
            format!("pub const WORKFLOW: &str = \"{name}\";\n"),
        )
        .expect("write fixture");
    }
    let hits = scan_source_tree(tmp.path(), &needles);
    assert_eq!(
        hits.len(),
        workflows.len(),
        "every current plugin workflow basename must still be caught by the derived \
         needle set: {hits:?}"
    );
}

// ---------------------------------------------------------------------------
// P-1.11 — a skill trigger must never bind to a Local App workflow's name.
//
// `skill-api/src/builtin/bundled.rs` compiles each `BundledSkill`'s
// `triggers` array straight into `Skill::frontmatter.triggers`
// (`skill-api/src/builtin/mod.rs`'s `parse_builtin` ASSIGNS that field after
// parsing, so a bundled SKILL.md's own frontmatter cannot be a second source
// for it — `bundled.rs` is the only construction path for a bundled skill's
// triggers). That file lives OUTSIDE every root in [`SCAN_ROOTS`]
// (`skill-api/src`, not `harness-runtime/src/mobile` / `tasks/src` /
// `tools/workflow/src`), so the literal-scan gate above has no reach into it
// at all — the trigger array is a blind spot the file-literal scan was never
// going to cover, which is why this defect needed its own narrower check
// rather than an allowlist entry, and why the allowlist baseline is untouched
// by it.
//
// If a trigger literal embeds a live Local App build workflow basename
// ([`local_app_workflow_basenames`]), the skill becomes discoverable by a
// phrase that is really someone ELSE's identifier, and it only looks correct
// because that workflow has not been renamed yet — design doc §19.3's
// cross-component name binding, aimed at a trigger phrase instead of a
// match/dispatch arm.
//
// BOTH registries `bundled.rs` feeds are checked, not just the mobile one:
// `BUILTIN_DESKTOP` and `BUILTIN_MOBILE` are two arrays in the SAME file
// behind two public entry points (`skill_api::register_desktop` /
// `skill_api::register_mobile`), and a failure message that names the file
// while reading only one of its two arrays would leave the identical defect
// reachable one array down.
// ---------------------------------------------------------------------------

/// Every bundled skill registry `skill-api/src/builtin/bundled.rs` feeds,
/// labelled by the entry point that assembles it.
fn bundled_skill_registries() -> Vec<(&'static str, skill_api::SkillRegistry)> {
    let mut desktop = skill_api::SkillRegistry::new();
    skill_api::register_desktop(&mut desktop);
    vec![
        (
            "skill_api::register_mobile (BUILTIN_MOBILE)",
            harness_runtime::mobile::mobile_skill_registry(),
        ),
        ("skill_api::register_desktop (BUILTIN_DESKTOP)", desktop),
    ]
}

/// Every `(skill, trigger)` pair in `reg` whose trigger EMBEDS one of
/// `workflow_names`.
///
/// Containment, not equality: a trigger spelled `"run local-app-build"`
/// carries the workflow's identity exactly as much as one spelled
/// `"local-app-build"` does, and `SkillRegistry::discover` substring-matches
/// triggers against the query anyway — so an equality-only check would leave
/// the same cross-component binding reachable by adding one word to the
/// literal. Compared case-insensitively because `SkillRegistry::register`
/// lowercases triggers when it indexes them, so case is not a distinction the
/// runtime honours either.
fn triggers_colliding_with_workflow_names(
    reg: &skill_api::SkillRegistry,
    workflow_names: &BTreeSet<String>,
) -> Vec<String> {
    let mut offenders = Vec::new();
    for name in reg.names() {
        let skill = reg.get(name).expect("just listed by SkillRegistry::names");
        for trigger in &skill.frontmatter.triggers {
            let lowered = trigger.to_lowercase();
            for workflow in workflow_names {
                if lowered.contains(&workflow.to_lowercase()) {
                    offenders.push(format!(
                        "skill `{name}` trigger {trigger:?} embeds workflow name {workflow:?}"
                    ));
                }
            }
        }
    }
    offenders
}

/// One real, NON-EMPTY trigger literal, read out of the live registry, for
/// use as a positive-control needle. Sorted so the choice is deterministic
/// rather than dependent on `SkillRegistry::names`' unspecified ordering, and
/// empty triggers are skipped because `"".contains("")` would make the
/// control pass without the detector reading anything real.
fn a_live_trigger_of(label: &str, reg: &skill_api::SkillRegistry) -> String {
    let mut triggers: Vec<String> = reg
        .names()
        .into_iter()
        .flat_map(|name| {
            reg.get(name)
                .expect("just listed by SkillRegistry::names")
                .frontmatter
                .triggers
                .clone()
        })
        .filter(|trigger| !trigger.trim().is_empty())
        .collect();
    triggers.sort();
    triggers.into_iter().next().unwrap_or_else(|| {
        panic!(
            "{label} registered no skill trigger at all -- \
             skill-api/src/builtin/bundled.rs is the source of every bundled \
             trigger, so an empty set here means the two P-1.11 tests below \
             would pass by reading nothing"
        )
    })
}

fn registry_has_any_trigger(reg: &skill_api::SkillRegistry) -> bool {
    reg.names().into_iter().any(|name| {
        reg.get(name)
            .expect("just listed by SkillRegistry::names")
            .frontmatter
            .triggers
            .iter()
            .any(|trigger| !trigger.trim().is_empty())
    })
}

/// Prove `triggers_colliding_with_workflow_names` can actually SEE this
/// registry's triggers and report a collision, by handing it a needle taken
/// from the registry itself — a needle it MUST flag.
///
/// Without this control both P-1.11 tests would still pass if the helper
/// silently read nothing (a renamed field, an empty registry, a `names()`
/// that stopped listing bundled skills): "no offenders" and "no visibility"
/// produce the identical green. This is what stops each assertion below from
/// being a tautology that holds regardless of what `bundled.rs` says.
fn assert_collision_detector_is_live(label: &str, reg: &skill_api::SkillRegistry) {
    let needle = a_live_trigger_of(label, reg);
    let control = BTreeSet::from([needle.clone()]);
    let flagged = triggers_colliding_with_workflow_names(reg, &control);
    assert!(
        !flagged.is_empty(),
        "positive control failed for {label}: \
         triggers_colliding_with_workflow_names did not flag {needle:?}, a \
         trigger literal read out of that very registry. The detector is \
         blind, so the P-1.11 assertions in this file prove nothing about \
         skill-api/src/builtin/bundled.rs until it is fixed."
    );
}

/// The direct catch: no bundled skill — mobile or desktop — may carry a
/// trigger embedding one of today's real Local App plugin workflow basenames.
/// Before P-1.11 this failed for real: `create-local-app` carried
/// `"local-app-build"` (a WORKFLOW's name) as its third trigger, alongside
/// its own two genuine discovery phrases.
#[test]
fn a_workflow_name_is_not_a_skill_trigger() {
    let workflows = local_app_workflow_basenames();
    assert!(
        !workflows.is_empty(),
        "local_app_workflow_basenames() is empty, so this test would pass \
         without reading a single trigger. The plugin workflow registry must \
         still expose at least one Local App workflow name for \
         skill-api/src/builtin/bundled.rs to be gated against."
    );

    let mut positive_controls = 0;
    for (label, reg) in bundled_skill_registries() {
        if registry_has_any_trigger(&reg) {
            assert_collision_detector_is_live(label, &reg);
            positive_controls += 1;
        }
        let offenders = triggers_colliding_with_workflow_names(&reg, &workflows);
        assert!(
            offenders.is_empty(),
            "skill-api/src/builtin/bundled.rs binds a Local App plugin \
             workflow's own name as a skill trigger, reached via {label} -- \
             exactly the cross-component name binding design doc §19.3 \
             forbids ({} offender(s)): {offenders:?}. A workflow's name is \
             that workflow's own identity; give the skill a discovery phrase \
             that does not double as someone else's identifier.",
            offenders.len()
        );
    }
    assert!(
        positive_controls > 0,
        "no bundled registry exposed a trigger, so the collision detector's positive control ran zero times"
    );
}

/// The generalization `a_workflow_name_is_not_a_skill_trigger` alone cannot
/// prove: if the plugin later renames or adds a Local App workflow, a trigger
/// written to coincide with that future spelling is the same silent binding
/// P-1.11 removed, just aimed at tomorrow's name. The simulated name is
/// deliberately disjoint from today's basenames in BOTH directions (neither
/// contains the other), so a pass here cannot be explained by the literals the
/// test above already covers; the positive control is what keeps that from
/// making this a tautology.
#[test]
fn skill_triggers_survive_a_workflow_rename() {
    let today = local_app_workflow_basenames();
    let renamed: BTreeSet<String> = BTreeSet::from(["local-app-unified-build".to_string()]);
    for candidate in &renamed {
        for real in &today {
            assert!(
                !candidate.contains(real.as_str()) && !real.contains(candidate.as_str()),
                "fixture bug: simulated post-rename name {candidate:?} overlaps \
                 real basename {real:?}, so this test would prove nothing \
                 beyond a_workflow_name_is_not_a_skill_trigger"
            );
        }
    }

    let mut positive_controls = 0;
    for (label, reg) in bundled_skill_registries() {
        if registry_has_any_trigger(&reg) {
            assert_collision_detector_is_live(label, &reg);
            positive_controls += 1;
        }
        let offenders = triggers_colliding_with_workflow_names(&reg, &renamed);
        assert!(
            offenders.is_empty(),
            "skill-api/src/builtin/bundled.rs has a trigger matching a \
             simulated post-rename Local App workflow name, reached via \
             {label} ({} offender(s)): {offenders:?}. A trigger tied to \
             whichever spelling Phase 4's merge lands on is the same \
             cross-component binding P-1.11 removed, just deferred until \
             after the rename instead of caught before it.",
            offenders.len()
        );
    }
    assert!(
        positive_controls > 0,
        "no bundled registry exposed a trigger, so the rename detector's positive control ran zero times"
    );
}

/// The companion the two tests above need in order not to be gameable.
///
/// The cheapest way to green either of them is to DELETE the offending
/// trigger's whole array: a skill with no triggers can never collide with a
/// workflow name. That would also make the skill undiscoverable —
/// `SkillPrefetch` (`skill-api/src/prefetch.rs`) reaches bundled skills only
/// through `SkillRegistry::discover`, which matches nothing but triggers — and
/// nothing else in the tree asserts that `create-local-app` in particular is
/// still reachable (`local_app_specialists_are_independently_discoverable` in
/// `skill-api/src/builtin/mod.rs` covers the specialists, not the
/// coordinator). So P-1.11's fix — dropping ONE trigger from that array —
/// must be shown to have left the skill's own discovery phrases behind, and
/// that has to hold for every bundled skill rather than for one name spelled
/// out here, so this file keeps deriving from the live registry instead of
/// hardcoding a component name.
#[test]
fn every_bundled_skill_keeps_a_discovery_phrase_of_its_own() {
    for (label, reg) in bundled_skill_registries() {
        for name in reg.names() {
            let skill = reg.get(name).expect("just listed by SkillRegistry::names");
            assert!(
                !skill.frontmatter.triggers.is_empty(),
                "skill-api/src/builtin/bundled.rs leaves skill `{name}` \
                 ({label}) with an EMPTY triggers array, so \
                 SkillRegistry::discover can never surface it again. Removing \
                 a cross-component trigger binding means replacing it with a \
                 phrase the skill owns, not emptying the array to satisfy \
                 a_workflow_name_is_not_a_skill_trigger."
            );
            for trigger in &skill.frontmatter.triggers {
                assert!(
                    !trigger.trim().is_empty(),
                    "skill `{name}` ({label}) carries a BLANK trigger in \
                     skill-api/src/builtin/bundled.rs. `SkillRegistry::discover` \
                     asks `query.contains(trigger)`, and every string contains \
                     the empty one, so a blank trigger makes this skill match \
                     every query ever asked -- the opposite failure from an \
                     empty array, and just as silent."
                );
                assert!(
                    reg.discover(trigger).iter().any(|hit| hit.name == name),
                    "skill `{name}` ({label}) carries trigger {trigger:?} in \
                     skill-api/src/builtin/bundled.rs, but querying \
                     SkillRegistry::discover with that very phrase does not \
                     return it -- the trigger is decorative, so the skill is \
                     one array edit away from being unreachable with nothing \
                     to notice."
                );
            }
        }
    }
}

/// The load-bearing count. See the module doc comment: without pinning this
/// number, the cheapest way to green a red scanner is to widen an allowlist
/// entry, which would disable scanning for everything it now covers and never
/// have to say so. This test makes any change to the allowlist's SIZE visible
/// as a deliberate, committed decision.
#[test]
fn allowlist_entry_count_matches_the_committed_baseline() {
    let allowlist = load_allowlist();
    assert_eq!(
        allowlist.len(),
        ALLOWLIST_BASELINE_COUNT,
        "component_literal_allowlist.txt now has {} entries; the committed \
         baseline is {ALLOWLIST_BASELINE_COUNT}. If this grew because you \
         widened an entry to cover more than one exact occurrence, that is \
         exactly what this test exists to catch — narrow it back down. If a \
         genuinely new, reviewed exemption was added, update \
         ALLOWLIST_BASELINE_COUNT in this file in the SAME diff.",
        allowlist.len()
    );
}

// ---------------------------------------------------------------------------
// P1.7 (§19.3) — `register_verified_builtin` has exactly one call site.
//
// `plugin::PluginManager::register_verified_builtin` (`plugin/src/
// manager.rs`) is the one door into the plugin registry that bypasses
// `install`'s validation path — P0a.6 added it deliberately for the single
// compiled-in mobile plugin, whose `(id, manifest, install_dir)` triple never
// passed through untrusted input. Its whole safety argument rests on there
// being exactly one, auditable caller, in the composition module
// (`harness-runtime/src/mobile/mod.rs`'s `register_mobile_builtin_plugin`). A
// second call site anywhere is an unaudited second door, and nothing before
// this gate would have noticed one.
//
// This is a COUNTING gate of the same shape as the literal scanner above —
// deny-by-default directory enumeration, `#[cfg(test)]` regions and
// out-of-line test modules excluded, `code_view`'s comment/string blanking
// reused so a match inside a doc comment or a string literal can never reach
// the matcher — but it counts CALL SITES of one specific symbol, not
// occurrences of a set of string literals, so it needs one more thing the
// literal scanner does not: telling a CALL apart from a MENTION.
//
// `register_verified_builtin` appears five times as plain PROSE on this
// gate's scan surface for `lib.rs` — four in genuinely production code (one
// `//` line comment, three `///` doc comments) plus one in a comment inside
// `#[cfg(all(test, feature = "mobile"))] mod mobile_plugin_composition_tests`,
// which `test_skip_ranges` does not treat as a test region because it matches
// only the exact attribute `#[cfg(test)]`.
// `verify_mention_shapes_in_lib_rs_are_what_this_gate_assumes` below pins
// every one of those shapes and counts, so this paragraph cannot rot into
// fiction and so the mention-rejection rule cannot quietly stop being
// exercised on the real tree. A rule that counted every appearance of the
// word would therefore report SIX where there is truly one call — worse than
// reporting zero, because it would never be able to go green again without
// deleting the documentation that explains why the one real call site is
// safe. The distinguishing rule
// this file uses: scan the SAME blanked view [`code_view`] already lexes
// (comments and string/char literal content replaced with spaces, so a doc
// comment or an error-message string never reaches the matcher at all), then
// require a whole-word occurrence of the symbol to be followed — after only
// whitespace, possibly across a line break — by `(`. A doc mention is never
// followed by `(` in the source text itself (its "call-like" reading only
// exists in the reader's head), so it is excluded twice over: once by the
// comment blanking, and again by the trailing-`(` requirement even for the
// (impossible in Rust) case of a bare mention sitting in live code.
//
// The scan root is deliberately narrower than [`SCAN_ROOTS`]: the
// requirement is stated over `harness-runtime::mobile` specifically ("`register_
// verified_builtin` is called exactly once in `harness-runtime::mobile`, from the
// composition module") — not over `tasks/src` or `tools/workflow/src`,
// neither of which depends on the `plugin` crate at all. Scanning only
// `harness-runtime/src/mobile` also means the function's own DEFINITION
// (`plugin/src/manager.rs`, `pub async fn register_verified_builtin(`) can
// never enter this scan, so `0`-vs-`1` here can never be explained by having
// walked into the definition instead of a caller. The `fn`-keyword guard in
// [`find_call_sites`] is kept anyway, as defense in depth against a
// same-named wrapper `fn` ever being added inside this root — untested by a
// dedicated fixture (nothing in this root has ever needed it), but cheap
// enough to keep rather than assume the shape of tomorrow's code from here.
// ---------------------------------------------------------------------------

/// The one function this gate audits a single call site for.
const REGISTER_VERIFIED_BUILTIN_SYMBOL: &str = "register_verified_builtin";

/// Scan root for the call-site gate, relative to the workspace root. See the
/// section doc comment above for why this is narrower than [`SCAN_ROOTS`].
const REGISTER_VERIFIED_BUILTIN_SCAN_ROOT: &str = "harness-runtime/src/mobile";

/// One call site of [`REGISTER_VERIFIED_BUILTIN_SYMBOL`] found in production
/// source — as opposed to a doc-comment or string-literal MENTION of the same
/// name, which [`find_call_sites`] must not (and, by construction, cannot)
/// count.
#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord)]
struct CallSite {
    /// Path relative to the workspace root (or to the fixture root, for a
    /// planted-tree test), forward-slash separated.
    rel_path: String,
    /// 1-based source line the call's SYMBOL starts on.
    line: usize,
}

impl std::fmt::Display for CallSite {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "{}:{}", self.rel_path, self.line)
    }
}

/// True for an ASCII Rust identifier character (`[A-Za-z0-9_]`). Used for
/// word-boundary checks so a match against [`REGISTER_VERIFIED_BUILTIN_SYMBOL`]
/// cannot be fooled by being a strict substring of a longer identifier —
/// concretely, `harness-runtime/src/mobile/mod.rs`'s own doc comment names
/// `plugin::manager::register_verified_builtin_tests`, the module in
/// `plugin/src/manager.rs` that holds that function's OWN tests, which
/// contains this symbol as a prefix followed immediately by `_tests`. Without
/// this check that occurrence would be a candidate match at all (it is still
/// excluded twice over here, since it also sits inside a doc comment and is
/// not followed by `(`).
fn is_ident_char(c: char) -> bool {
    c.is_ascii_alphanumeric() || c == '_'
}

/// The whole-file [`code_view`] blanking (comments and string/char literal
/// content replaced with spaces — the exact same lexer the literal scanner
/// above runs, not a second copy of it) rejoined into ONE string, plus — for
/// every character emitted — the 1-based source LINE it came from.
///
/// Line numbers, not byte offsets: blanking replaces some multi-byte source
/// characters (this very file's Chinese-language doc comments and design-doc
/// quotes, in particular) with a single-byte `' '`, which would desynchronize
/// a byte offset from `src`'s own byte offsets on any line mixing ASCII and
/// non-ASCII text. [`find_call_sites`] only ever needs to report a LINE
/// number, never a column, so tracking line number directly here sidesteps
/// that mismatch entirely rather than working around it.
fn blanked_with_line_map(src: &str) -> (String, Vec<usize>) {
    let lines = code_view(src);
    let mut text = String::new();
    let mut map = Vec::new();
    for (idx, line) in lines.iter().enumerate() {
        for ch in line.chars() {
            text.push(ch);
            map.push(idx + 1);
        }
        text.push('\n');
        map.push(idx + 1);
    }
    (text, map)
}

/// Every CALL SITE of `symbol` in `src`: a whole-word occurrence that
/// survives [`blanked_with_line_map`] (so comment and string/char-literal
/// content has already become spaces and can never reach the matcher) and
/// falls outside a `#[cfg(test)]` region ([`test_skip_ranges`]), EXCEPT the
/// symbol's own definition — a match immediately preceded, past whitespace,
/// by the keyword `fn`.
///
/// There is deliberately NO "must be followed by `(`" requirement, though an
/// earlier draft of this file had one. Two reasons, in order of weight:
///
/// 1. It would not be load-bearing. Mutating `is_call` to a constant `true`
///    left all 36 tests in this file green, because every non-call mention
///    that exists — on the real tree and in the fixtures below — lives in a
///    comment or a string literal and has therefore already been blanked
///    away before the matcher runs. A condition that is vacuously true in
///    every state the suite reaches is not a tested condition; it is
///    decoration that reads like a safeguard.
/// 2. It would be WRONG for this symbol. `register_verified_builtin` is an
///    inherent `async fn` on `PluginManager`, so in live code its name can
///    only be its definition or a USE of it — and a use spelled without
///    parentheses is still a door: `let door =
///    plugin::PluginManager::register_verified_builtin;` yields a callable
///    function item, and `door(&manager, id, manifest, dir).await` opens the
///    registry just as wide as the method-call spelling. A paren rule would
///    have made that second-door spelling invisible to the very gate whose
///    job is to find second doors. Inherent methods also cannot be `use`d,
///    so there is no import spelling that would need excusing.
///
/// The call-versus-mention distinction the requirement asks for therefore
/// rests entirely on the comment/string blanking plus the whole-word check,
/// both of which `call_site_detector_counts_calls_not_mentions_of_register_verified_builtin`
/// exercises with mentions written in CALL SHAPE precisely so that neither
/// can be deleted while the suite stays green.
fn find_call_sites(rel_path: &str, src: &str, symbol: &str) -> Vec<CallSite> {
    let skip_ranges = if is_rust_source(Path::new(rel_path)) {
        test_skip_ranges(src)
    } else {
        Vec::new()
    };
    let (text, line_map) = blanked_with_line_map(src);
    let chars: Vec<char> = text.chars().collect();
    let symbol_chars: Vec<char> = symbol.chars().collect();
    let n = chars.len();
    let m = symbol_chars.len();
    let mut sites = Vec::new();
    if m == 0 || m > n {
        return sites;
    }
    let mut i = 0;
    while i + m <= n {
        if chars[i..i + m] != symbol_chars[..] {
            i += 1;
            continue;
        }
        let boundary_before = i == 0 || !is_ident_char(chars[i - 1]);
        let boundary_after = i + m == n || !is_ident_char(chars[i + m]);
        if !boundary_before || !boundary_after {
            i += 1;
            continue;
        }

        // Look behind past whitespace for a `fn` keyword: a DEFINITION, not a
        // call. See this function's doc comment.
        let mut k = i;
        while k > 0 && chars[k - 1].is_whitespace() {
            k -= 1;
        }
        let is_definition = k >= 2
            && chars[k - 2] == 'f'
            && chars[k - 1] == 'n'
            && (k == 2 || !is_ident_char(chars[k - 3]));

        if !is_definition {
            let line = line_map.get(i).copied().unwrap_or(1);
            if !line_is_skipped(line, &skip_ranges) {
                sites.push(CallSite {
                    rel_path: rel_path.to_string(),
                    line,
                });
            }
        }
        i += 1;
    }
    sites.sort();
    sites
}

/// Scan every RUST source file under `root` for call sites of `symbol`,
/// skipping `#[cfg(test)]` regions and out-of-line test modules exactly like
/// [`scan_tree`]. Reported paths are relative to `rel_base`.
///
/// Restricted to `.rs` files (unlike [`scan_tree`], which deliberately scans
/// every file type by default): a call site is a Rust-syntax concept, and a
/// non-Rust file under this scan root cannot invoke a Rust `async fn` at all,
/// so widening the walk to other extensions here could only ever manufacture
/// false matches, never catch a real one.
fn scan_call_sites(root: &Path, rel_base: &Path, symbol: &str) -> Vec<CallSite> {
    assert!(
        root.is_dir(),
        "scan root {} does not exist — refusing to scan nothing",
        root.display()
    );
    let mut files = Vec::new();
    collect_source_files(root, &mut files);
    let excluded = cfg_test_only_module_files(&files);
    let mut sites = Vec::new();
    for file in files {
        if excluded.contains(&file) || !is_rust_source(&file) {
            continue;
        }
        let src = fs::read_to_string(&file)
            .unwrap_or_else(|e| panic!("read {} failed: {e}", file.display()));
        let rel_path = file
            .strip_prefix(rel_base)
            .unwrap_or(&file)
            .to_string_lossy()
            .replace('\\', "/");
        sites.extend(find_call_sites(&rel_path, &src, symbol));
    }
    sites.sort();
    sites
}

/// The absolute, asserted-to-exist scan root for the call-site gate.
fn register_verified_builtin_scan_root() -> PathBuf {
    let path = workspace_root().join(REGISTER_VERIFIED_BUILTIN_SCAN_ROOT);
    assert!(
        path.is_dir(),
        "scan root `{REGISTER_VERIFIED_BUILTIN_SCAN_ROOT}` does not exist at {} \
         — a missing root scans NOTHING and would make this gate green for the \
         wrong reason. If the directory moved, update \
         REGISTER_VERIFIED_BUILTIN_SCAN_ROOT in this file.",
        path.display()
    );
    path
}

/// The real production scan: every call site of `register_verified_builtin`
/// under [`REGISTER_VERIFIED_BUILTIN_SCAN_ROOT`], paths relative to the
/// workspace root.
fn production_register_verified_builtin_call_sites() -> Vec<CallSite> {
    let base = workspace_root();
    scan_call_sites(
        &register_verified_builtin_scan_root(),
        &base,
        REGISTER_VERIFIED_BUILTIN_SYMBOL,
    )
}

/// The assertion the required gate test makes, factored out so
/// `a_second_call_site_in_a_non_composition_module_makes_the_gate_red_with_file_and_line`
/// below can prove — by actually invoking this same function and catching the
/// panic — that a second call site makes it fail, printing every site's file
/// and line, rather than asserting a count by hand in two places that could
/// silently drift apart from what the real gate test checks.
fn assert_exactly_one_register_verified_builtin_call_site(sites: &[CallSite]) {
    assert_eq!(
        sites.len(),
        1,
        "expected exactly ONE call site of `{REGISTER_VERIFIED_BUILTIN_SYMBOL}` \
         (plugin/src/manager.rs's verified-builtin door, whose whole safety \
         argument rests on having a single auditable caller), found {}:\n{}",
        sites.len(),
        sites
            .iter()
            .map(|s| format!("  {s}"))
            .collect::<Vec<_>>()
            .join("\n")
    );
}

/// P1.7 §19.3 — the required gate. `register_verified_builtin` must be
/// called exactly once inside `harness-runtime::mobile`, and that one call must be in
/// the composition module.
///
/// Strict equality, not `<= 1`: the module doc comment above explains why a
/// zero count (an empty file list, a scanner that reads nothing, or a rule
/// that matches only occurrences it can't find) must be exactly as much of a
/// failure as a count above one — `assert_eq!(_, 1)` fails on either side,
/// where `<= 1` would not.
#[test]
fn register_verified_builtin_has_exactly_one_call_site() {
    let sites = production_register_verified_builtin_call_sites();
    assert_exactly_one_register_verified_builtin_call_site(&sites);
    assert_eq!(
        sites[0].rel_path, "harness-runtime/src/mobile/mod.rs",
        "the one call site of `register_verified_builtin` must be in the \
         composition module (lib.rs's `register_mobile_builtin_plugin`), got \
         {sites:?} — a call from anywhere else is an unaudited second door \
         into the plugin registry"
    );
}

/// The house-defect check for the test above: prove the detector can actually
/// SEE a call, not merely that today's real tree happens to contain one. A
/// tempdir with the SAME shape as the real composition call
/// (`.register_verified_builtin(..).await` reached through a `manager`
/// receiver) alongside a file containing ONLY prose mentions — a `//!` module
/// doc, a `///` doc comment, and a string literal, mirroring the three forms
/// `lib.rs` actually carries today — must report exactly the one real call,
/// naming the file that holds it, and nothing from the prose-only file.
///
/// This is the test the brief asks for containing both a real call and a doc
/// mention in one place: the composition fixture below carries BOTH (a doc
/// comment directly above its real call), so a rule that regressed to
/// counting mentions would trip on that single file already.
#[test]
fn call_site_detector_counts_calls_not_mentions_of_register_verified_builtin() {
    let tmp = tempfile::tempdir().expect("tempdir");
    fs::write(
        tmp.path().join("composition.rs"),
        "/// Composition calls register_verified_builtin(id, manifest, dir) once.\n\
         async fn compose(manager: &Manager) {\n\
         \x20   manager\n\
         \x20       .register_verified_builtin(id, manifest, dir)\n\
         \x20       .await;\n\
         }\n",
    )
    .expect("write composition fixture");
    fs::write(
        tmp.path().join("prose_only.rs"),
        "//! See register_verified_builtin(id, manifest, dir) for trusted boot.\n\
         /* register_verified_builtin(a, b, c) in a block comment, too. */\n\
         /// Docs also say register_verified_builtin(x) is careful about source.\n\
         pub const NOTE: &str = \"call register_verified_builtin(id, dir) yourself\";\n\
         pub const TRAILING: u8 = 1; // register_verified_builtin(y) after code\n",
    )
    .expect("write prose-only fixture");

    let sites = scan_call_sites(tmp.path(), tmp.path(), REGISTER_VERIFIED_BUILTIN_SYMBOL);
    assert_eq!(
        sites,
        vec![CallSite {
            rel_path: "composition.rs".to_string(),
            line: 4,
        }],
        "got {sites:?}. EVERY mention in this fixture is deliberately \
         CALL-SHAPED — `register_verified_builtin(..)`, parentheses and all — \
         in a `//!` module doc, a `/* */` block comment, a `///` doc comment, \
         a string literal, and a trailing `//` comment after live code, plus \
         one more in composition.rs's own doc comment directly above the real \
         call. That is the whole point: if the mentions were written WITHOUT \
         parentheses, the trailing-`(` rule alone would reject them and \
         `code_view`'s comment/string blanking — the other half of the rule — \
         could be deleted with every test still green. Written this way, the \
         blanking is the ONLY thing standing between six occurrences and one \
         reported call site."
    );
}

/// The evidence P1.7 asks for directly: a SECOND real call site, planted in a
/// file that is not the composition module, must make the gate's own
/// assertion helper panic — not merely change a number nothing reads — and
/// the panic message must name BOTH sites by file and line.
#[test]
fn a_second_call_site_in_a_non_composition_module_makes_the_gate_red_with_file_and_line() {
    let tmp = tempfile::tempdir().expect("tempdir");
    fs::write(
        tmp.path().join("composition.rs"),
        "async fn compose(manager: &Manager) {\n\
         \x20   manager\n\
         \x20       .register_verified_builtin(id, manifest, dir)\n\
         \x20       .await;\n\
         }\n",
    )
    .expect("write composition fixture");
    fs::write(
        tmp.path().join("rogue_second_door.rs"),
        "async fn sneak_in(manager: &Manager) {\n\
         \x20   manager.register_verified_builtin(id2, manifest2, dir2).await;\n\
         }\n",
    )
    .expect("write rogue fixture");

    let sites = scan_call_sites(tmp.path(), tmp.path(), REGISTER_VERIFIED_BUILTIN_SYMBOL);
    assert_eq!(
        sites.len(),
        2,
        "fixture sanity: both the composition call and the rogue call must be \
         found before checking that the gate assertion reacts to them, got \
         {sites:?}"
    );

    let result = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
        assert_exactly_one_register_verified_builtin_call_site(&sites);
    }));
    let panic_payload = result.expect_err(
        "a second call site must make assert_exactly_one_register_verified_builtin_call_site \
         PANIC — the exact assertion register_verified_builtin_has_exactly_one_call_site \
         runs in production — not merely leave `sites.len()` at 2 for something \
         else to notice",
    );
    let message = panic_payload
        .downcast_ref::<String>()
        .cloned()
        .or_else(|| {
            panic_payload
                .downcast_ref::<&str>()
                .map(|s| (*s).to_string())
        })
        .unwrap_or_default();
    assert!(
        message.contains("composition.rs:3"),
        "failure message must name the first (legitimate) call site's file \
         and line, got: {message}"
    );
    assert!(
        message.contains("rogue_second_door.rs:2"),
        "failure message must name the SECOND (unaudited) call site's file \
         and line, got: {message}"
    );
}

/// Whole-word occurrences of `symbol` in one RAW source line — comments and
/// string literals included, unlike [`find_call_sites`], which is the point:
/// this is how [`verify_mention_shapes_in_lib_rs_are_what_this_gate_assumes`]
/// sees the mentions the gate must reject.
fn whole_word_occurrences(line: &str, symbol: &str) -> usize {
    let chars: Vec<char> = line.chars().collect();
    let sym: Vec<char> = symbol.chars().collect();
    let (n, m) = (chars.len(), sym.len());
    let mut count = 0;
    let mut i = 0;
    while m > 0 && i + m <= n {
        if chars[i..i + m] == sym[..]
            && (i == 0 || !is_ident_char(chars[i - 1]))
            && (i + m == n || !is_ident_char(chars[i + m]))
        {
            count += 1;
        }
        i += 1;
    }
    count
}

/// The POSITIVE CONTROL for the mention-rejection half of this gate.
///
/// `register_verified_builtin_has_exactly_one_call_site` reports `1` for two
/// very different reasons that it cannot tell apart on its own: because the
/// detector correctly rejected four prose mentions and kept one real call, or
/// because there was never anything to reject in the first place. Today the
/// first is true — but nothing pinned it, so deleting lib.rs's documentation
/// (or moving it into the `#[cfg(test)]` module) would silently retire the
/// only place on the REAL tree where call-versus-mention is exercised, and
/// every test here would stay green.
///
/// So: assert the mentions are still there, in the shapes the section doc
/// above claims, and that exactly one of the five whole-word occurrences the
/// scan surface sees is the call.
///
/// The current production surface carries four prose mentions: two line
/// comments and two doc comments. Pinning both the total and the shapes keeps
/// the call detector's comment/string rejection exercised against the real
/// tree instead of only against planted fixtures.
#[test]
fn verify_mention_shapes_in_lib_rs_are_what_this_gate_assumes() {
    let rel = "harness-runtime/src/mobile/mod.rs";
    let path = workspace_root().join(rel);
    let src =
        fs::read_to_string(&path).unwrap_or_else(|e| panic!("read {} failed: {e}", path.display()));
    let skip = test_skip_ranges(&src);

    let mut occurrences: Vec<(usize, String)> = Vec::new();
    for (idx, line) in src.lines().enumerate() {
        let line_no = idx + 1;
        if line_is_skipped(line_no, &skip) {
            continue;
        }
        for _ in 0..whole_word_occurrences(line, REGISTER_VERIFIED_BUILTIN_SYMBOL) {
            occurrences.push((line_no, line.trim_start().to_string()));
        }
    }

    let calls = find_call_sites(rel, &src, REGISTER_VERIFIED_BUILTIN_SYMBOL);
    assert_eq!(
        calls.len(),
        1,
        "lib.rs must hold exactly one CALL; got {calls:?}"
    );
    let call_line = calls[0].line;

    let mentions: Vec<&(usize, String)> = occurrences
        .iter()
        .filter(|(line_no, _)| *line_no != call_line)
        .collect();
    assert_eq!(
        mentions.len(),
        4,
        "`register_verified_builtin` must still appear as PROSE on the scan \
         surface of lib.rs — two line comments plus two doc comments. This \
         gate's `1` only proves that mentions are EXCLUDED for as long as \
         there are mentions to exclude. Found {} instead: {mentions:?}. If \
         the documentation legitimately changed, update the section doc above \
         AND this count together — do not just relax the number, or the \
         mention-rejection rule stops being exercised on the real tree at \
         all.",
        mentions.len()
    );

    let doc_comments = mentions
        .iter()
        .filter(|(_, text)| text.starts_with("///"))
        .count();
    let line_comments = mentions
        .iter()
        .filter(|(_, text)| text.starts_with("//") && !text.starts_with("///"))
        .count();
    assert_eq!(
        (line_comments, doc_comments),
        (2, 2),
        "expected two `//` line comments and two `///` doc comments, got \
         ({line_comments}, {doc_comments}) from {mentions:?} — every prose \
         mention must sit at the START of a comment line, because a mention \
         sharing a line with live code is a shape this gate has never been \
         tested against"
    );
}

/// The word-boundary half of the matcher, made load-bearing.
///
/// Nothing on the real tree exercises it: the longer identifiers that contain
/// `register_verified_builtin` as a strict prefix — `plugin/src/manager.rs`'s
/// `mod register_verified_builtin_tests`, and lib.rs's doc reference to it —
/// are respectively inside a `#[cfg(test)]` skip region and inside a `///`
/// comment, so both are already gone before the boundary check runs.
/// Mutating `boundary_after` to a constant `true` therefore left all 37 tests
/// green, which is exactly the shape of untested condition this file is
/// supposed to be allergic to. This fixture puts both boundary directions in
/// LIVE code, where nothing else can excuse them.
///
/// Branch A: a `mod register_verified_builtin_tests` item, a
/// `register_verified_builtin_v2` call and a `pre_register_verified_builtin`
/// call — all live code, none of them this symbol — must yield ZERO. Branch
/// B: the same file plus one genuine call must yield exactly ONE, so branch A
/// cannot be passing because the scan silently read nothing.
#[test]
fn find_call_sites_requires_a_whole_word_match_not_a_substring_of_a_longer_name() {
    let neighbours = "mod register_verified_builtin_tests {\n\
         \x20   fn helper() {}\n\
         }\n\
         async fn other(manager: &Manager) {\n\
         \x20   manager.register_verified_builtin_v2(id, manifest).await;\n\
         \x20   manager.pre_register_verified_builtin(id, manifest).await;\n\
         }\n";
    let sites_a = find_call_sites(
        "neighbours.rs",
        neighbours,
        REGISTER_VERIFIED_BUILTIN_SYMBOL,
    );
    assert!(
        sites_a.is_empty(),
        "`register_verified_builtin_tests` (trailing `_tests`), \
         `register_verified_builtin_v2` (trailing `_v2`) and \
         `pre_register_verified_builtin` (leading `pre_`) are three different \
         identifiers that merely CONTAIN this symbol, all written in live \
         code where no comment or string blanking can excuse them. None is a \
         call site of `register_verified_builtin`; got {sites_a:?}"
    );

    let with_call = format!(
        "{neighbours}\nasync fn real(manager: &Manager) {{\n\
             \x20   manager.register_verified_builtin(id, manifest, dir).await;\n\
             }}\n"
    );
    let sites_b = find_call_sites(
        "neighbours.rs",
        &with_call,
        REGISTER_VERIFIED_BUILTIN_SYMBOL,
    );
    assert_eq!(
        sites_b,
        vec![CallSite {
            rel_path: "neighbours.rs".to_string(),
            line: 10,
        }],
        "the same three near-miss identifiers plus ONE real call must report \
         exactly that call — otherwise branch A's empty result would be \
         consistent with a matcher that finds nothing at all; got {sites_b:?}"
    );
}

/// A second door does not have to be spelled with parentheses.
/// `plugin::PluginManager::register_verified_builtin` used as a VALUE is a
/// function item; binding it and calling the binding later opens the registry
/// exactly as wide as `manager.register_verified_builtin(..)` does, and a
/// detector that required a following `(` would have reported zero extra call
/// sites for it. This test pins that this spelling counts.
///
/// It is also the branch that keeps the paren-free rule honest in the other
/// direction: the same fixture's comment and string mentions — identical text
/// — must still NOT count, so "we stopped requiring `(`" cannot quietly have
/// become "we count every occurrence".
#[test]
fn a_parenless_function_value_reference_is_counted_as_a_second_door() {
    let tmp = tempfile::tempdir().expect("tempdir");
    fs::write(
        tmp.path().join("composition.rs"),
        "async fn compose(manager: &Manager) {\n\
         \x20   manager.register_verified_builtin(id, manifest, dir).await;\n\
         }\n",
    )
    .expect("write composition fixture");
    fs::write(
        tmp.path().join("sneaky.rs"),
        "// Only a comment about register_verified_builtin, not a door.\n\
         const DOC: &str = \"register_verified_builtin\";\n\
         async fn sneak(manager: &PluginManager) {\n\
         \x20   let door = plugin::PluginManager::register_verified_builtin;\n\
         \x20   door(manager, id, manifest, dir).await;\n\
         }\n",
    )
    .expect("write sneaky fixture");

    let sites = scan_call_sites(tmp.path(), tmp.path(), REGISTER_VERIFIED_BUILTIN_SYMBOL);
    assert_eq!(
        sites,
        vec![
            CallSite {
                rel_path: "composition.rs".to_string(),
                line: 2,
            },
            CallSite {
                rel_path: "sneaky.rs".to_string(),
                line: 4,
            },
        ],
        "the paren-free function-VALUE reference on sneaky.rs line 4 must be \
         reported as a second door, while the comment on line 1 and the string \
         literal on line 2 — the same identifier, in non-code positions — must \
         not be; got {sites:?}"
    );
}

/// The A/B pair that makes ZERO and ONE an OBSERVED distinction rather than
/// an arithmetic property of `assert_eq!(_, 1)`, and the only test that
/// exercises [`find_call_sites`]'s `fn`-keyword guard directly.
///
/// The definition `pub async fn register_verified_builtin(` IS followed by
/// `(`, so the trailing-paren rule alone would count it as a call — only the
/// `fn` lookback rejects it. Branch A (definition alone) must yield zero;
/// branch B (the same text plus one real call) must yield exactly one, at the
/// CALL's line, not the definition's. Deleting the `fn` guard turns A into 1
/// and B into 2, so neither branch can survive that deletion.
#[test]
fn find_call_sites_rejects_a_definition_and_still_counts_a_call_in_the_same_file() {
    let definition_only = "pub async fn register_verified_builtin(\n\
         \x20   &self,\n\
         \x20   id: &PluginId,\n\
         ) -> Result<(), PluginManagerError> {\n\
         \x20   Ok(())\n\
         }\n";
    let sites_a = find_call_sites(
        "plugin/src/manager.rs",
        definition_only,
        REGISTER_VERIFIED_BUILTIN_SYMBOL,
    );
    assert!(
        sites_a.is_empty(),
        "a function DEFINITION is not a call site — it is followed by `(` \
         exactly like a call, and only the `fn` lookback separates them, so \
         this is the branch that proves that lookback runs: got {sites_a:?}"
    );

    let with_call = format!(
        "{definition_only}\nasync fn caller(manager: &PluginManager) {{\n\
         \x20   manager.register_verified_builtin(id, manifest, dir).await;\n\
         }}\n"
    );
    let sites_b = find_call_sites(
        "plugin/src/manager.rs",
        &with_call,
        REGISTER_VERIFIED_BUILTIN_SYMBOL,
    );
    assert_eq!(
        sites_b,
        vec![CallSite {
            rel_path: "plugin/src/manager.rs".to_string(),
            line: 9,
        }],
        "the same text plus ONE real call must report exactly that call, at \
         the call's line (9) and not the definition's (1) — zero and one must \
         be distinguishable outcomes of the detector, not just two different \
         numbers on the left of an `assert_eq!`"
    );
}

// ---------------------------------------------------------------------------
// The same gate, widened from `harness-runtime::mobile` to the WHOLE workspace.
//
// §19.3 is stated over `harness-runtime::mobile`, and
// `register_verified_builtin_has_exactly_one_call_site` above enforces
// exactly that. But the SAFETY argument the requirement rests on is not
// scoped to one crate: `PluginManager::register_verified_builtin` is `pub`,
// so any crate in this workspace can open a second door, and the
// harness-runtime::mobile-scoped gate would stay green while it did.
// `harness-runtime/src/desktop/mod.rs` already NAMES the symbol in a doc comment
// today, which is how close the neighbouring crate already is to it.
//
// Two things this wider walk buys beyond coverage, both of which the narrow
// gate cannot have: it is the only test where [`find_call_sites`]'s
// `fn`-keyword guard has to reject a REAL definition (`plugin/src/manager.rs`
// line ~580) and where [`test_skip_ranges`] has to exclude REAL in-tree calls
// (that file's `#[cfg(test)] mod register_verified_builtin_tests`, which
// calls the function twice). Under the narrow root neither guard ever fires
// on production source, so neither is load-bearing there.
// ---------------------------------------------------------------------------

/// Directory names the crate-root walk never descends into. `target` alone is
/// tens of thousands of files; the rest are build/tool output that contains
/// no first-party Rust source. Any directory whose name starts with `.` is
/// skipped too (`.git`, `.worktrees`, editor state).
const WORKSPACE_WALK_SKIP_DIRS: &[&str] = &[
    "target",
    "node_modules",
    "codegraph-out",
    "graphify-out",
    "dist",
    "build",
];

/// Every directory named `src` under the workspace root — i.e. every crate's
/// source root, found by enumeration rather than from a hand-maintained list,
/// so a crate added tomorrow is inside this gate without anyone remembering
/// to add it.
fn collect_crate_src_dirs(dir: &Path, out: &mut Vec<PathBuf>) {
    let Ok(entries) = fs::read_dir(dir) else {
        return;
    };
    let mut paths: Vec<PathBuf> = entries.filter_map(|e| e.ok()).map(|e| e.path()).collect();
    paths.sort();
    for path in paths {
        if !path.is_dir() {
            continue;
        }
        let Some(name) = path.file_name().and_then(|n| n.to_str()) else {
            continue;
        };
        if name.starts_with('.') || WORKSPACE_WALK_SKIP_DIRS.contains(&name) {
            continue;
        }
        if name == "src" {
            out.push(path);
        } else {
            collect_crate_src_dirs(&path, out);
        }
    }
}

/// §19.3's safety argument, enforced over every crate in the workspace rather
/// than only `harness-runtime::mobile`: `register_verified_builtin` has ONE production
/// caller anywhere, and it is the mobile composition module.
#[test]
fn register_verified_builtin_has_exactly_one_call_site_in_the_whole_workspace() {
    let base = workspace_root();
    let mut roots = Vec::new();
    collect_crate_src_dirs(&base, &mut roots);
    roots.sort();
    assert!(
        roots.len() > 40,
        "expected the crate-root walk to find every crate's `src` in {}, found \
         only {} ({roots:?}) — a walk that finds nothing scans nothing and is \
         green forever",
        base.display(),
        roots.len()
    );

    let mut files = Vec::new();
    for root in &roots {
        collect_source_files(root, &mut files);
    }
    let excluded = cfg_test_only_module_files(&files);

    // Prefilter on the raw bytes: a file that does not contain the symbol at
    // all cannot contain a call site, and lexing 1400 files to learn that is
    // pure cost. A BROKEN prefilter shows up as an empty candidate set, which
    // the assertion below rejects by name.
    let mut candidates: Vec<(PathBuf, String)> = Vec::new();
    for file in &files {
        if excluded.contains(file) || !is_rust_source(file) {
            continue;
        }
        let src = fs::read_to_string(file)
            .unwrap_or_else(|e| panic!("read {} failed: {e}", file.display()));
        if src.contains(REGISTER_VERIFIED_BUILTIN_SYMBOL) {
            candidates.push((file.clone(), src));
        }
    }
    let candidate_rels: BTreeSet<String> = candidates
        .iter()
        .map(|(file, _)| {
            file.strip_prefix(&base)
                .unwrap_or(file)
                .to_string_lossy()
                .replace('\\', "/")
        })
        .collect();

    // The load-bearing control: the walk must actually have REACHED the two
    // files whose exclusion this gate's `1` depends on. Without this, a walk
    // that only ever saw `harness-runtime::mobile` would produce the identical `1` and
    // this test would be a slower copy of the narrow one.
    for required in [
        "plugin/src/manager.rs",
        "harness-runtime/src/desktop/mod.rs",
        "harness-runtime/src/mobile/mod.rs",
    ] {
        assert!(
            candidate_rels.contains(required),
            "the workspace walk must reach `{required}` — it names \
             `register_verified_builtin` (as the DEFINITION plus two \
             `#[cfg(test)]` calls, as a doc mention in a NEIGHBOURING crate, \
             and as the one real call, respectively), and this gate's answer \
             is only meaningful if those files were actually examined and \
             excluded on their merits. Reached: {candidate_rels:?}"
        );
    }

    let mut sites = Vec::new();
    for (file, src) in &candidates {
        let rel = file
            .strip_prefix(&base)
            .unwrap_or(file)
            .to_string_lossy()
            .replace('\\', "/");
        sites.extend(find_call_sites(&rel, src, REGISTER_VERIFIED_BUILTIN_SYMBOL));
    }
    sites.sort();

    assert_exactly_one_register_verified_builtin_call_site(&sites);
    assert_eq!(
        sites[0].rel_path, "harness-runtime/src/mobile/mod.rs",
        "the workspace's only production call site of \
         `register_verified_builtin` must be the mobile composition module, \
         got {sites:?}"
    );
}

#[cfg(test)]
mod scan_mechanics_tests {
    use super::*;

    #[test]
    fn test_skip_ranges_covers_a_top_level_mod_block() {
        let src = "\
fn production() {}

#[cfg(test)]
mod tests {
    fn helper() {}
}

fn also_production() {}
";
        let ranges = test_skip_ranges(src);
        assert_eq!(ranges, vec![(3, 6)]);
        assert!(!line_is_skipped(1, &ranges));
        assert!(line_is_skipped(3, &ranges));
        assert!(line_is_skipped(6, &ranges));
        assert!(!line_is_skipped(8, &ranges));
    }

    #[test]
    fn test_skip_ranges_covers_a_nested_cfg_test_fn() {
        let src = "\
impl Thing {
    #[cfg(test)]
    fn helper(&self) {
        1
    }

    fn production(&self) {}
}
";
        let ranges = test_skip_ranges(src);
        assert_eq!(ranges, vec![(2, 5)]);
        assert!(!line_is_skipped(7, &ranges));
    }

    /// (F1) — the regression the whole rewrite exists for. `#[cfg(test)]` on a
    /// NON-brace item must skip the attribute line and nothing else. The old
    /// implementation scanned forward for the first `<indent>}` line, which
    /// here is the closing brace of a LATER production item, so everything in
    /// between vanished from the scan.
    #[test]
    fn cfg_test_on_a_non_brace_item_skips_only_the_attribute_line() {
        for item in [
            "use std::io;",
            "mod helpers;",
            "const X: u8 = 1;",
            "type T = u8;",
        ] {
            let src = format!(
                "\
#[cfg(test)]
{item}

fn production() {{
    let leak = \"local-app-build\";
}}
"
            );
            let ranges = test_skip_ranges(&src);
            assert_eq!(
                ranges,
                vec![(1, 1)],
                "`#[cfg(test)] {item}` must skip only its own line, got {ranges:?}"
            );
            assert!(
                !line_is_skipped(5, &ranges),
                "`#[cfg(test)] {item}` must not swallow the production line below it"
            );
        }
    }

    /// (F1, end to end) — the exact planted shape from the review: two lines
    /// dropped into a file must not hide a literal that follows them.
    #[test]
    fn a_planted_cfg_test_use_cannot_hide_a_literal_below_it() {
        let needles: BTreeSet<String> =
            ["local-app-build"].into_iter().map(str::to_owned).collect();
        let src = "\
fn a() {}

#[cfg(test)]
use std::io;

fn b() {
    let x = \"local-app-build\";
}
";
        let found = scan_text("planted.rs", src, &needles);
        assert_eq!(found.len(), 1, "got {found:?}");
        assert_eq!(found[0].line, 7);
        assert_eq!(found[0].literal, "local-app-build");
    }

    /// (F7) — a `}` at column 0 INSIDE a raw string must not terminate a skip
    /// range early and re-expose the rest of the test module as production.
    #[test]
    fn a_close_brace_inside_a_raw_string_does_not_end_the_skip_range() {
        let src = "\
fn production() {}

#[cfg(test)]
mod tests {
    const FIXTURE: &[u8] = br#\"{
}
\"#;
    fn later_test() {
        let _ = \"local-app-build\";
    }
}

fn also_production() {}
";
        let ranges = test_skip_ranges(src);
        assert_eq!(
            ranges,
            vec![(3, 11)],
            "brace depth must be tracked over code only, got {ranges:?}"
        );
        let needles: BTreeSet<String> =
            ["local-app-build"].into_iter().map(str::to_owned).collect();
        assert!(
            scan_text("raw.rs", src, &needles).is_empty(),
            "test-module code after a raw-string brace must stay skipped"
        );
    }

    /// (F1's mirror) — an out-of-line `#[cfg(test)] mod x;` excludes the file
    /// it points at, but only when NO ungated declaration also reaches it.
    #[test]
    fn cfg_test_only_modules_are_excluded_but_shared_ones_are_not() {
        let tmp = tempfile::tempdir().expect("tempdir");
        let root = tmp.path();
        fs::write(
            root.join("lib.rs"),
            "pub mod real;\n#[cfg(test)]\n#[path = \"only_test.rs\"]\nmod only_test;\n",
        )
        .unwrap();
        fs::write(
            root.join("real.rs"),
            "pub const N: &str = \"local-app-build\";\n",
        )
        .unwrap();
        fs::write(
            root.join("only_test.rs"),
            "const T: &str = \"local-app-use-test\";\n",
        )
        .unwrap();

        let mut files = Vec::new();
        collect_source_files(root, &mut files);
        let excluded = cfg_test_only_module_files(&files);
        assert!(excluded.contains(&root.join("only_test.rs")));
        assert!(!excluded.contains(&root.join("real.rs")));

        // Now aim a SECOND, test-gated declaration at the production file: it
        // must NOT drop out, because `pub mod real;` is ungated.
        fs::write(
            root.join("lib.rs"),
            "pub mod real;\n#[cfg(test)]\n#[path = \"real.rs\"]\nmod sneaky;\n",
        )
        .unwrap();
        let mut files = Vec::new();
        collect_source_files(root, &mut files);
        let excluded = cfg_test_only_module_files(&files);
        assert!(
            !excluded.contains(&root.join("real.rs")),
            "a file reachable through an UNGATED mod declaration must stay in the scan"
        );
    }

    #[test]
    fn matches_on_line_prefers_the_namespaced_fqn_over_the_bare_basename() {
        let needles: BTreeSet<String> = ["local-app-build", "lingxi-local-app:local-app-build"]
            .into_iter()
            .map(str::to_owned)
            .collect();
        let found = matches_on_line("let x = \"lingxi-local-app:local-app-build\";", &needles);
        assert_eq!(found, vec!["lingxi-local-app:local-app-build".to_string()]);
    }

    #[test]
    fn matches_on_line_still_catches_a_bare_basename_alone() {
        let needles: BTreeSet<String> = ["local-app-build", "lingxi-local-app:local-app-build"]
            .into_iter()
            .map(str::to_owned)
            .collect();
        let found = matches_on_line("workflow_id: \"local-app-build\".into(),", &needles);
        assert_eq!(found, vec!["local-app-build".to_string()]);

        let ambiguous = BTreeSet::from(["local-app-run".to_string()]);
        assert!(
            matches_on_line("stage the local-app-runtime first", &ambiguous).is_empty(),
            "a basename prefix inside a longer hyphenated token is not a component identity"
        );
    }

    #[test]
    fn matches_on_line_reports_two_distinct_literals_on_one_line() {
        let needles: BTreeSet<String> = ["local-app-build", "local-app-use-test"]
            .into_iter()
            .map(str::to_owned)
            .collect();
        let found = matches_on_line(
            "use-test is `local-app-use-test` — NOT `local-app-build`.",
            &needles,
        );
        assert_eq!(
            found,
            vec![
                "local-app-use-test".to_string(),
                "local-app-build".to_string()
            ]
        );
    }

    /// (F6) — a line carrying the SAME literal twice is two occurrences, and
    /// the per-entry count is what makes one entry unable to cover an
    /// unbounded number of them.
    #[test]
    fn two_occurrences_of_one_literal_on_one_line_are_counted_twice() {
        let needles: BTreeSet<String> =
            ["local-app-build"].into_iter().map(str::to_owned).collect();
        let found = scan_text(
            "dup.rs",
            "let pair = (\"local-app-build\", \"local-app-build\");\n",
            &needles,
        );
        assert_eq!(found.len(), 2, "got {found:?}");
        let entry = AllowlistEntry {
            rel_path: "dup.rs".into(),
            literal: "local-app-build".into(),
            expected: 1,
        };
        let findings = reconcile(&found, std::slice::from_ref(&entry));
        assert!(
            matches!(findings.as_slice(), [Finding::CountMismatch { .. }]),
            "one entry must not silently cover both occurrences, got {findings:?}"
        );
    }

    /// (F2) — an entry pointing at nothing is a standing pre-approved hole.
    #[test]
    fn an_entry_that_matches_nothing_is_reported_as_stale() {
        let entry = AllowlistEntry {
            rel_path: "gone.rs".into(),
            literal: "local-app-build".into(),
            expected: 1,
        };
        let findings = reconcile(&[], std::slice::from_ref(&entry));
        assert!(
            matches!(findings.as_slice(), [Finding::Stale(_)]),
            "{findings:?}"
        );
        assert!(format!("{}", findings[0]).contains("gone.rs"));
    }

    #[test]
    fn allowlist_parsing_keeps_a_colon_bearing_literal_intact() {
        let entries = parse_allowlist(
            "# comment\n\nharness-runtime/src/mobile/mod.rs:lingxi-local-app:local-app-build:3\n",
        );
        assert_eq!(entries.len(), 1);
        assert_eq!(entries[0].rel_path, "harness-runtime/src/mobile/mod.rs");
        assert_eq!(entries[0].literal, "lingxi-local-app:local-app-build");
        assert_eq!(entries[0].expected, 3);
    }

    /// (D3a) — `+` concatenation is the third assembly shape, alongside
    /// `concat!` and the wrapped literal. Stripping `+` alone is not enough:
    /// `.to_string(` survives character stripping, so it is removed as a token.
    #[test]
    fn a_plus_concatenated_literal_is_caught_as_a_split_literal() {
        let needles: BTreeSet<String> = ["local-app-build", "local-app-use-test"]
            .into_iter()
            .map(str::to_owned)
            .collect();
        let src = "\
pub fn a() -> String {
    \"local-app\".to_string() + \"-build\"
}
pub fn b() -> String {
    String::from(\"local-app-use\") + \"-test\"
}
";
        let found = scan_text("plus.rs", src, &needles);

        let a = found
            .iter()
            .find(|v| v.literal == "local-app-build")
            .unwrap_or_else(|| {
                panic!("`\"local-app\".to_string() + \"-build\"` was not caught: {found:?}")
            });
        assert_eq!(a.line, 2);
        assert!(
            a.split,
            "a `+`-assembled name must be flagged as a split literal"
        );

        let b = found
            .iter()
            .find(|v| v.literal == "local-app-use-test")
            .unwrap_or_else(|| panic!("`String::from(..) + \"-test\"` was not caught: {found:?}"));
        assert_eq!(b.line, 5);
        assert!(b.split);
    }

    /// (D3a) — the module doc claims exactly TWO shapes stay out of reach. A
    /// doc that names a limit is only worth its paper if the limit is pinned:
    /// if someone strengthens the normalizer, this test fails and the paragraph
    /// has to be rewritten instead of quietly going stale.
    #[test]
    fn normalization_has_exactly_these_two_documented_limits() {
        let needles: BTreeSet<String> =
            ["local-app-build"].into_iter().map(str::to_owned).collect();

        // Limit 1 — placeholder substitution.
        assert!(
            scan_text(
                "fmt.rs",
                "let x = format!(\"local-{}-build\", \"app\");\n",
                &needles
            )
            .is_empty(),
            "the module doc says `format!` substitution is NOT caught; it now is \
             — update the doc"
        );

        // Limit 2 — fragments separated by code that is not an assembly no-op.
        assert!(
            scan_text(
                "push.rs",
                "let mut s = String::from(\"local-app\");\ns.push_str(\"-build\");\n",
                &needles
            )
            .is_empty(),
            "the module doc says statement-level assembly is NOT caught; it now \
             is — update the doc"
        );
        assert!(
            scan_text("quote.js", "const w = 'local-app' + '-build';\n", &needles).is_empty(),
            "the module doc says single-quoted JS fragments are NOT caught; they \
             now are — update the doc"
        );
    }

    /// (D3b) — `#[cfg(test)]` is a Rust construct, and [`code_view`] lexes Rust
    /// comments and Rust string literals, not JS template or regex literals. A
    /// `.js` template literal whose text happens to contain a line trimming to
    /// exactly `#[cfg(test)]` followed by a line ending in `{` would therefore
    /// have opened a skip range and hidden the rest of the script. Not running
    /// the detector on non-Rust files at all removes the class.
    #[test]
    fn a_js_template_literal_cannot_open_a_cfg_test_skip_range() {
        let needles: BTreeSet<String> =
            ["local-app-build"].into_iter().map(str::to_owned).collect();
        let src = "\
export const meta = { name: 'demo' };
const help = `
#[cfg(test)]
mod tests {
`;
export const step = { workflow: \"local-app-build\" };
";
        // The same bytes in a .rs file DO open a skip range — this is the
        // construct the detector is supposed to react to…
        let rust_ranges = test_skip_ranges(src);
        assert!(
            !rust_ranges.is_empty(),
            "fixture must actually be able to open a skip range, else this test \
             proves nothing; got {rust_ranges:?}"
        );
        assert!(
            scan_text("evasion.rs", src, &needles).is_empty(),
            "fixture sanity: as Rust, the literal below the attribute is skipped"
        );

        // …but as JS it must not, and the literal must be reported.
        let found = scan_text("evasion.js", src, &needles);
        assert_eq!(
            found.len(),
            1,
            "a JS template literal must not be able to open a `#[cfg(test)]` \
             skip range and hide the rest of the script, got {found:?}"
        );
        assert_eq!(found[0].line, 6);
        assert_eq!(found[0].literal, "local-app-build");
    }

    #[test]
    fn normalized_view_strips_only_the_documented_characters() {
        let (text, map) = normalized_view("concat!(\"ab\", \"cd\")\n");
        assert_eq!(text, "abcd");
        // `a` is the 9th byte of the input (0-based 9): c-o-n-c-a-t-!-( -> 8, then `"`.
        assert_eq!(map[0], 9);
    }
}
