//! Single source of truth for the product namespace: on-disk directory names,
//! well-known filenames, the config-dir env override name, the env-var prefix,
//! and the brand name. Every crate that needs one of these values imports it
//! from here so the namespace is defined in exactly one place.
//!
//! These hold LingXi's namespace values. (They were introduced holding the
//! Claude values so routing the scattered literals through this crate was a
//! pure refactor; this commit flips them to LingXi, which is why the parity
//! fixtures/tests were updated together.)
//!
//! The Anthropic *protocol* layer (model IDs, `anthropic` host/provider,
//! `tengu_*`, beta headers, `claude-cli` User-Agent, OAuth, `ANTHROPIC_*` env)
//! is deliberately NOT defined here — those must stay Claude/Anthropic for the
//! backend to work and live in their own modules.

use std::ffi::OsString;
use std::path::{Path, PathBuf};

/// User config directory name under `$HOME` (e.g. `~/.lingxi`). Also the
/// per-project config dir name (`<repo>/.lingxi/`).
pub const DOT_DIR: &str = ".lingxi";

/// Global config file, a sibling of [`DOT_DIR`] in `$HOME` (e.g. `~/.lingxi.json`).
pub const GLOBAL_CONFIG_FILE: &str = ".lingxi.json";

/// Legacy global-config filename checked *inside* the config-home before
/// [`GLOBAL_CONFIG_FILE`] (e.g. `<config-home>/.config.json`). The filename
/// itself is not brand-specific; named here so the resolver has one source.
pub const LEGACY_GLOBAL_CONFIG_FILE: &str = ".config.json";

/// Environment variable that overrides the config-home directory.
pub const CONFIG_DIR_ENV: &str = "LINGXI_CONFIG_DIR";

/// Project memory filename (case-sensitive).
pub const MEMORY_FILE: &str = "LINGXI.md";

/// Local-override memory filename.
pub const MEMORY_LOCAL_FILE: &str = "LINGXI.local.md";

/// Manifest directory name inside a plugin / marketplace package.
pub const PLUGIN_MANIFEST_DIR: &str = ".lingxi-plugin";

/// Env var naming a plugin's materialized (read-only) install root. Used for
/// `${LINGXI_PLUGIN_ROOT}` token substitution in skill/agent prompts, hook
/// commands, MCP `command`/`args`/`env`/`url`/`headers`, LSP
/// `command`/`args`/`env`/`workspaceFolder` and monitor commands, and injected
/// under this name into the spawned child's environment.
///
/// Oracle counterpart: `CLAUDE_PLUGIN_ROOT`. The Claude spelling is
/// **replaced, not aliased** — LingXi neither reads `.claude-plugin` nor
/// exports any `CLAUDE_PLUGIN_*` alias, and the brand gate
/// (`scripts/checks/check_brand_leaks.py`, rule G2) treats a `CLAUDE_PLUGIN_*` read
/// outside the brand-normalization fixtures as a leak. Oracle fixtures are
/// compared only after brand-token normalization.
pub const PLUGIN_ROOT_ENV: &str = "LINGXI_PLUGIN_ROOT";

/// Env var naming a plugin's *writable* per-plugin data directory, which is
/// deliberately a separate tree from the read-only root named by
/// [`PLUGIN_ROOT_ENV`] (plugin data must never be able to overwrite the
/// materialized install). Same substitution surfaces as [`PLUGIN_ROOT_ENV`].
///
/// Oracle counterpart: `CLAUDE_PLUGIN_DATA`, replaced rather than aliased —
/// see [`PLUGIN_ROOT_ENV`].
pub const PLUGIN_DATA_ENV: &str = "LINGXI_PLUGIN_DATA";

/// Env var naming the current project directory. Same substitution surfaces as
/// [`PLUGIN_ROOT_ENV`], plus injection into hook child environments.
///
/// Oracle counterpart: `CLAUDE_PROJECT_DIR`, replaced rather than aliased —
/// see [`PLUGIN_ROOT_ENV`].
pub const PROJECT_DIR_ENV: &str = "LINGXI_PROJECT_DIR";

/// Human-facing product name (banners, system-prompt identity, help text).
pub const PRODUCT_NAME: &str = "LingXi";

/// Prefix for the product's own (non-protocol) environment variables, e.g.
/// `LINGXI_ENABLE_TASKS`. Protocol env vars (`ANTHROPIC_*`, the kept
/// `CLAUDE_CODE_*` SDK contract vars) are excluded from this prefix by design.
pub const ENV_PREFIX: &str = "LINGXI_";

/// Managed/enterprise policy directory per platform (admin-provisioned).
/// macOS Application Support location.
pub const MANAGED_DIR_MACOS: &str = "/Library/Application Support/LingXi";
/// Windows Program Files location.
pub const MANAGED_DIR_WINDOWS: &str = r"C:\Program Files\LingXi";
/// Other (Linux/BSD) location.
pub const MANAGED_DIR_UNIX: &str = "/etc/lingxi";

/// Resolve the user config-home: `$LINGXI_CONFIG_DIR` when the env value is
/// supplied (honored verbatim, including an empty value — matching the upstream
/// `??` semantics), else `home.join(DOT_DIR)`.
///
/// This is the pure core: the env value is injected so callers stay testable
/// without mutating process env. Callers that historically treated an *empty*
/// value as unset (the `||`-shaped resolvers, e.g. `migrations`) should filter
/// the env value to `None` before calling and pass their own home, preserving
/// that deliberate divergence.
#[must_use]
pub fn config_home(home: &Path, config_dir_env: Option<OsString>) -> PathBuf {
    match config_dir_env {
        Some(dir) => PathBuf::from(dir),
        None => home.join(DOT_DIR),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::{BTreeMap, BTreeSet};
    use std::path::Path;

    #[test]
    fn config_home_defaults_to_home_join_dot_dir() {
        let got = config_home(Path::new("/home/u"), None);
        assert_eq!(got, Path::new("/home/u").join(DOT_DIR));
    }

    #[test]
    fn config_home_honors_env_verbatim_including_empty() {
        // `??` semantics: a SET value wins verbatim, even when empty.
        let got = config_home(Path::new("/home/u"), Some("/custom".into()));
        assert_eq!(got, Path::new("/custom"));
        let empty = config_home(Path::new("/home/u"), Some(OsString::from("")));
        assert_eq!(empty, Path::new(""));
    }

    #[test]
    fn namespace_values_are_lingxi() {
        assert_eq!(DOT_DIR, ".lingxi");
        assert_eq!(GLOBAL_CONFIG_FILE, ".lingxi.json");
        assert_eq!(CONFIG_DIR_ENV, "LINGXI_CONFIG_DIR");
        assert_eq!(MEMORY_FILE, "LINGXI.md");
        assert_eq!(MEMORY_LOCAL_FILE, "LINGXI.local.md");
        assert_eq!(PLUGIN_MANIFEST_DIR, ".lingxi-plugin");
        assert_eq!(PLUGIN_ROOT_ENV, "LINGXI_PLUGIN_ROOT");
        assert_eq!(PLUGIN_DATA_ENV, "LINGXI_PLUGIN_DATA");
        assert_eq!(PROJECT_DIR_ENV, "LINGXI_PROJECT_DIR");
        assert_eq!(PRODUCT_NAME, "LingXi");
        assert_eq!(ENV_PREFIX, "LINGXI_");
        // No Claude namespace leaks in our own values.
        for v in [
            DOT_DIR,
            GLOBAL_CONFIG_FILE,
            CONFIG_DIR_ENV,
            MEMORY_FILE,
            MEMORY_LOCAL_FILE,
            PLUGIN_MANIFEST_DIR,
            PLUGIN_ROOT_ENV,
            PLUGIN_DATA_ENV,
            PROJECT_DIR_ENV,
            ENV_PREFIX,
        ] {
            assert!(!v.to_lowercase().contains("claude"), "claude leak: {v}");
        }
    }

    // ---------------------------------------------------------------------
    // §19.1 / P0a.1 — the three Plugin substitution env names are declared
    // here and nowhere else.
    //
    // `branding` has zero dependencies and 42+ dependents, so there is no
    // Cargo-graph path from here to "what does crate X do with this string".
    // The only single-source assertion reachable from inside `branding` is to
    // read the other crates' source text off disk, rooted at
    // `CARGO_MANIFEST_DIR` — the same thing `scripts/checks/check_brand_leaks.py`
    // does for the brand-namespace literals. That is what these tests do.
    //
    // Two tests, deliberately:
    //   * `the_single_source_scan_sees_a_hardcoded_spelling` proves the
    //     scanner CAN fire, on a synthetic tree it builds itself. Without it a
    //     green `plugin_env_constants_are_the_single_source` would be
    //     indistinguishable from a scanner that read nothing.
    //   * `plugin_env_constants_are_the_single_source` runs that same scanner
    //     over the real workspace.
    // ---------------------------------------------------------------------

    /// One source scan: first-hit line per `(workspace-relative path, needle)`,
    /// plus how many files were actually read (so "found nothing" can be told
    /// apart from "read nothing").
    struct Scan {
        hits: BTreeMap<(String, String), usize>,
        files_read: usize,
    }

    /// Walk `root` for `.rs` files and record which of `needles` each one
    /// spells out literally. `exclude` (if given) prunes one subtree — the
    /// `branding` crate itself, where the literals are declared.
    ///
    /// Skips `target`, `node_modules` and any dot-directory, matching the
    /// enumeration `scripts/checks/check_brand_leaks.py` performs.
    fn scan_rs_sources(root: &Path, exclude: Option<&Path>, needles: &[&str]) -> Scan {
        const SKIP_DIRS: [&str; 2] = ["target", "node_modules"];

        let mut hits: BTreeMap<(String, String), usize> = BTreeMap::new();
        let mut files_read = 0usize;
        let mut stack = vec![root.to_path_buf()];
        while let Some(dir) = stack.pop() {
            let Ok(entries) = std::fs::read_dir(&dir) else {
                continue;
            };
            for entry in entries.flatten() {
                let path = entry.path();
                let name = entry.file_name();
                let name = name.to_string_lossy().into_owned();
                if path.is_dir() {
                    let excluded = exclude.is_some_and(|e| path == e);
                    if excluded || SKIP_DIRS.contains(&name.as_str()) || name.starts_with('.') {
                        continue;
                    }
                    stack.push(path);
                    continue;
                }
                if path.extension().and_then(|e| e.to_str()) != Some("rs") {
                    continue;
                }
                let Ok(text) = std::fs::read_to_string(&path) else {
                    continue;
                };
                files_read += 1;
                let rel = path
                    .strip_prefix(root)
                    .unwrap_or(&path)
                    .to_string_lossy()
                    .replace('\\', "/");
                for (lineno, line) in text.lines().enumerate() {
                    for needle in needles {
                        if line.contains(needle) {
                            hits.entry((rel.clone(), (*needle).to_string()))
                                .or_insert(lineno + 1);
                        }
                    }
                }
            }
        }
        Scan { hits, files_read }
    }

    /// `branding`'s own directory and the workspace root above it.
    fn workspace_paths() -> (PathBuf, PathBuf) {
        let branding_dir = Path::new(env!("CARGO_MANIFEST_DIR"))
            .canonicalize()
            .expect("branding crate dir exists");
        let workspace_root = branding_dir
            .parent()
            .expect("branding sits directly under the crates source root")
            .to_path_buf();
        assert!(
            workspace_root
                .parent()
                .unwrap()
                .join("Cargo.toml")
                .is_file(),
            "expected {} to be the crates source root (no Cargo.toml in its parent)",
            workspace_root.display()
        );
        (workspace_root, branding_dir)
    }

    /// A throwaway directory under the system temp dir, created fresh.
    fn scratch_dir(tag: &str) -> PathBuf {
        let nonce = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|d| d.as_nanos())
            .unwrap_or(0);
        let dir = std::env::temp_dir().join(format!(
            "branding-{tag}-{pid}-{nonce}",
            pid = std::process::id()
        ));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).expect("create scratch dir");
        dir
    }

    fn write_file(root: &Path, rel: &str, body: &str) {
        let path = root.join(rel);
        std::fs::create_dir_all(path.parent().expect("file has a parent")).expect("mkdir -p");
        std::fs::write(&path, body).expect("write scratch file");
    }

    /// POSITIVE CONTROL for [`plugin_env_constants_are_the_single_source`].
    ///
    /// The workspace test is an *absence* assertion, and an absence assertion
    /// is worthless unless the probe demonstrably fires. So run the exact same
    /// scanner over a tree built here, containing one file per behaviour the
    /// scanner must get right, and assert the hit set EXACTLY.
    ///
    /// This is what makes the workspace test falsifiable without planting into
    /// a file owned by another task.
    #[test]
    fn the_single_source_scan_sees_a_hardcoded_spelling() {
        let root = scratch_dir("single-source-control");

        // MUST be flagged: a literal spelling at a plausible use site.
        write_file(
            &root,
            "crate_a/src/lib.rs",
            "fn a() -> Option<String> {\n    \
             std::env::var(\"LINGXI_PLUGIN_ROOT\").ok()\n}\n",
        );
        // MUST be flagged, and on the right line: the `${...}` token form.
        write_file(
            &root,
            "crate_a/src/nested/sub.rs",
            "// line 1\n// line 2\nconst T: &str = \"${LINGXI_PROJECT_DIR}\";\n",
        );
        // MUST NOT be flagged: this is the correct thing to do.
        write_file(
            &root,
            "crate_b/src/lib.rs",
            "fn b() -> Option<String> {\n    \
             std::env::var(branding::PLUGIN_DATA_ENV).ok()\n}\n",
        );
        // MUST NOT be flagged: not a `.rs` file (data/fixtures legitimately
        // carry the token — they are what gets substituted).
        write_file(
            &root,
            "crate_b/fixture.json",
            "{\"x\":\"${LINGXI_PLUGIN_ROOT}\"}",
        );
        // MUST NOT be flagged: build output and dot-dirs are not source.
        write_file(&root, "target/debug/gen.rs", "\"LINGXI_PLUGIN_DATA\"");
        write_file(&root, ".hidden/h.rs", "\"LINGXI_PLUGIN_DATA\"");
        write_file(&root, "node_modules/n.rs", "\"LINGXI_PLUGIN_DATA\"");
        // MUST NOT be flagged: the excluded subtree (stands in for `branding`).
        write_file(&root, "excluded/src/lib.rs", "\"LINGXI_PLUGIN_ROOT\"");

        let needles = [PLUGIN_ROOT_ENV, PLUGIN_DATA_ENV, PROJECT_DIR_ENV];
        let scan = scan_rs_sources(&root, Some(&root.join("excluded")), &needles);
        let _ = std::fs::remove_dir_all(&root);

        // Exactly three `.rs` files survive pruning: crate_a/src/lib.rs,
        // crate_a/src/nested/sub.rs and crate_b/src/lib.rs. excluded/,
        // target/, .hidden/ and node_modules/ are pruned; fixture.json is not
        // `.rs`. Pinning the COUNT (not just the hit set) is what makes an
        // over-eager prune visible instead of silently green.
        assert_eq!(
            scan.files_read, 3,
            "scanner read the wrong file set: {} files",
            scan.files_read
        );

        let got: Vec<(String, String, usize)> = scan
            .hits
            .iter()
            .map(|((p, n), l)| (p.clone(), n.clone(), *l))
            .collect();
        assert_eq!(
            got,
            vec![
                (
                    "crate_a/src/lib.rs".to_string(),
                    "LINGXI_PLUGIN_ROOT".to_string(),
                    2
                ),
                (
                    "crate_a/src/nested/sub.rs".to_string(),
                    "LINGXI_PROJECT_DIR".to_string(),
                    3
                ),
            ],
            "scanner did not report exactly the planted hardcodes with their lines"
        );
    }

    /// §19.1 / P0a.1: no crate in the `lingxi-code` workspace other than
    /// `branding` may spell out `LINGXI_PLUGIN_ROOT` / `LINGXI_PLUGIN_DATA` /
    /// `LINGXI_PROJECT_DIR` in Rust source; they must go through
    /// [`PLUGIN_ROOT_ENV`] / [`PLUGIN_DATA_ENV`] / [`PROJECT_DIR_ENV`].
    ///
    /// `TOLERATED` is the pre-existing debt this task inherits and is not
    /// permitted to touch: `hooks/`, `orchestrator/`, `platforms/posix/`,
    /// `apps/cli`'s `plugin_init` scaffold and `plugin/`'s materializer all
    /// implement `${LINGXI_PLUGIN_*}` / `${LINGXI_PROJECT_DIR}` substitution by
    /// hand today, predating these constants. Migrating them is follow-up work
    /// on the crates that own those files.
    ///
    /// The check is deliberately ONE-WAY (`found ⊆ tolerated`), not set
    /// equality:
    ///   * growth is the failure this test exists to catch, and it fails
    ///     naming `path:line` and the needle;
    ///   * shrinkage is the migration succeeding. Failing on shrinkage would
    ///     make `branding` — a crate with 42 dependents — go red because a
    ///     *different* crate did the right thing, and every task in this phase
    ///     that edits `plugin/src/*` is a candidate. That is an unattributable
    ///     red for a non-defect.
    ///
    /// One-way subset checking never hides a new violation; the only thing it
    /// tolerates is a `TOLERATED` line outliving its use site. The cost of that
    /// is bounded to the exact `(file, needle)` pair listed.
    #[test]
    fn plugin_env_constants_are_the_single_source() {
        /// `(workspace-relative path, needle)` pairs that hardcode one of the
        /// three spellings today. File-level, not line-level: an unrelated edit
        /// above a hit must not trip this gate.
        const TOLERATED: &[(&str, &str)] = &[
            ("apps/cli/src/commands/plugin_init.rs", "LINGXI_PLUGIN_ROOT"),
            ("hooks/src/attachment.rs", "LINGXI_PLUGIN_ROOT"),
            ("hooks/src/executor.rs", "LINGXI_PLUGIN_DATA"),
            ("hooks/src/executor.rs", "LINGXI_PLUGIN_ROOT"),
            ("hooks/src/executor.rs", "LINGXI_PROJECT_DIR"),
            ("hooks/src/executor_test.rs", "LINGXI_PLUGIN_DATA"),
            ("hooks/src/executor_test.rs", "LINGXI_PLUGIN_ROOT"),
            ("hooks/src/executor_test.rs", "LINGXI_PROJECT_DIR"),
            ("hooks/src/registry.rs", "LINGXI_PROJECT_DIR"),
            ("hooks/src/user_config.rs", "LINGXI_PLUGIN_ROOT"),
            (
                "orchestrator/src/cwd_changed_firer.rs",
                "LINGXI_PROJECT_DIR",
            ),
            (
                "orchestrator/src/file_changed_firer.rs",
                "LINGXI_PROJECT_DIR",
            ),
            (
                "orchestrator/src/mcp_hook_dispatcher.rs",
                "LINGXI_PROJECT_DIR",
            ),
            (
                "orchestrator/src/task_completed_firer.rs",
                "LINGXI_PROJECT_DIR",
            ),
            (
                "orchestrator/src/task_created_firer.rs",
                "LINGXI_PROJECT_DIR",
            ),
            (
                "orchestrator/src/task_lifecycle_hook_firer.rs",
                "LINGXI_PROJECT_DIR",
            ),
            (
                "orchestrator/src/teammate_idle_firer.rs",
                "LINGXI_PROJECT_DIR",
            ),
            (
                "platforms/posix/src/process/runner.rs",
                "LINGXI_PROJECT_DIR",
            ),
            ("plugin/src/manager.rs", "LINGXI_PLUGIN_DATA"),
            ("plugin/src/manager.rs", "LINGXI_PLUGIN_ROOT"),
            ("plugin/src/manager.rs", "LINGXI_PROJECT_DIR"),
            ("plugin/tests/materialize.rs", "LINGXI_PLUGIN_DATA"),
            ("plugin/tests/materialize.rs", "LINGXI_PLUGIN_ROOT"),
        ];

        let (workspace_root, branding_dir) = workspace_paths();
        let needles = [PLUGIN_ROOT_ENV, PLUGIN_DATA_ENV, PROJECT_DIR_ENV];
        let scan = scan_rs_sources(&workspace_root, Some(&branding_dir), &needles);

        // POSITIVE CONTROL: this workspace has ~1.8k `.rs` files. A scanner
        // that walked the wrong root, or was pruned to nothing, would report
        // zero offenders and look green — so refuse to draw any conclusion
        // from a scan that plainly did not read this workspace.
        assert!(
            scan.files_read >= 500,
            "single-source scan only read {} .rs files under {} — it did not \
             traverse this workspace, so its 'no offenders' result means \
             nothing",
            scan.files_read,
            workspace_root.display()
        );
        // POSITIVE CONTROL: read + match really fired on those files. Keyed on
        // a needle that cannot go to zero as the migration proceeds.
        let control = scan_rs_sources(&workspace_root, Some(&branding_dir), &["pub fn "]);
        assert!(
            control.hits.len() >= 200,
            "single-source scan matched `pub fn ` in only {} files — the \
             read/match path is broken, so a clean result proves nothing",
            control.hits.len()
        );

        let tolerated: BTreeSet<(&str, &str)> = TOLERATED.iter().copied().collect();
        let offenders: Vec<String> = scan
            .hits
            .iter()
            .filter(|((path, needle), _)| !tolerated.contains(&(path.as_str(), needle.as_str())))
            .map(|((path, needle), line)| format!("{path}:{line} hardcodes {needle}"))
            .collect();

        assert!(
            offenders.is_empty(),
            "{} crate source file(s) spell a Plugin env name out instead of \
             referencing the branding constant — use \
             branding::{{PLUGIN_ROOT_ENV, PLUGIN_DATA_ENV, PROJECT_DIR_ENV}}:\n  {}",
            offenders.len(),
            offenders.join("\n  ")
        );
    }
}
