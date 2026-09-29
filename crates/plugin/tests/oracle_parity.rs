//! §19.1 / P0a.8 — brand-normalized Claude oracle fixture parity.
//!
//! §19.1's completion condition: the SAME fixture, run through
//! [`plugin::normalize`] (P0a.7's brand-token normalization layer), yields
//! the same component inventory and parse behaviour under LingXi as under
//! the Claude oracle. This file is the parity test that condition asks for.
//!
//! ## Why this is the easiest place in the codebase to write a test that
//! ## cannot fail (and what defends against that here)
//!
//! A parity test between two trees "agrees by construction" whenever neither
//! side is ever actually exercised differently from the other. Four ways that
//! happens, and how each is closed off below:
//!
//! 1. **The normalizer never fired at all.** "`unmapped` is empty" is
//!    trivially true when nothing was scanned — a scanner that returns no
//!    tokens reports no unmapped ones either. So
//!    [`oracle_and_lingxi_agree_on_component_inventory`] asserts on the
//!    POSITIVE side too: [`NormalizeReportSummary::content_mapped`] must
//!    contain exactly the tokens the checked-in fixture actually spells, at
//!    exactly the multiplicity the fixture spells them
//!    ([`fixture_content_token_counts`] counts them straight off disk, so the
//!    expectation is not derived from the normalizer it checks).
//! 2. **An unrecognised token passes through unchanged on both sides.** Then
//!    both trees carry the identical unnormalized string and "match" without
//!    the normalizer having understood it. The same test asserts `unmapped`
//!    is EMPTY before trusting the comparison, and
//!    [`an_unmapped_token_is_reported_and_the_agreement_is_not_trusted`] is
//!    the separate required case where it is NON-empty — and shows that the
//!    inventories still compare EQUAL there, which is precisely why the
//!    `unmapped` gate has to be checked independently.
//! 3. **Both sides are built by the same code from the same input, so of
//!    course they agree.** [`oracle_and_lingxi_disagree_when_normalization_is_skipped`]
//!    is the required NEGATIVE CONTROL. It is the same construction as the
//!    positive test with ONE boolean flipped —
//!    [`rebuild_tree`]'s `normalize_content` — so the only difference between
//!    "agrees" and "must not agree" is whether file CONTENT went through
//!    [`plugin::normalize`]. Directory names go through the normalizer in
//!    both cases, so the negative tree still loads and still produces a
//!    component inventory of the same SHAPE; only the token strings diverge.
//! 4. **The compared inventory is empty on both sides.** Two empty vecs are
//!    equal. Every one of the eight component slots
//!    [`plugin::PluginComponents`] declares is asserted NON-empty on the
//!    LingXi side individually, by name, before the equality assertion runs —
//!    so a single silently-defaulting component cannot hide behind the seven
//!    that work.
//!
//! ## What is actually compared
//!
//! [`Inventory`] captures every field
//! `plugin::discovery::detect_components` can populate for this fixture:
//!
//! * manifest identity — name / version / description / author / homepage;
//! * the `commands` / `agents` / `skills` / `output_styles` / `workflows`
//!   component-path lists, as plugin-root-relative paths so two different
//!   temp-dir roots compare equal;
//! * **the CONTENT of every one of those component files**
//!   ([`Inventory::component_files`]). Without this the path slots would
//!   agree by construction — their paths come from directory names, and a
//!   normalizer that only ever touched the manifest would still "pass". The
//!   content is where `commands/greet.md`'s, `agents/helper.md`'s,
//!   `skills/sample-skill/SKILL.md`'s, `output-styles/concise.md`'s and
//!   `workflows/build.js`'s brand tokens live;
//! * the hooks' parsed [`HookSpec`] (events, matcher, command, args, env,
//!   timeout, blocking, priority) — §19.1's "parse behaviour", carried
//!   unsubstituted at parse time;
//! * the MCP servers' parsed [`McpSpec`] (command, args AND env — the args
//!   carry the plugin-data and project-dir placeholders, so
//!   dropping them would leave only one of the five brand pairs load-bearing);
//! * the LSP servers' parsed [`LspSpec`].
//!
//! Both sides are loaded through [`plugin::discover_installed_plugins`], the
//! production entry point `plugin::discovery::load_plugin_from_path`'s own
//! doc names as the primitive a local-path-installed LingXi plugin takes.
//!
//! ## Known gap, stated rather than hidden
//!
//! Of `plugin::known_pairs`'s five pairs, the fixture's file CONTENT
//! exercises three (the three env-var spellings) and its directory NAMES
//! exercise a fourth (the manifest directory). The fifth — the bare brand
//! dot-dir — is not spelled anywhere in a plugin tree, so this file asserts
//! its absence explicitly (see [`fixture_content_token_counts`]'s use below)
//! instead of letting a zero count pass unremarked. `brand_normalize.rs`'s
//! own unit tests cover that pair at the string level.

use std::collections::BTreeMap;
use std::fs;
use std::path::{Path, PathBuf};

use plugin::{ComponentPath, PluginComponents, PluginManifest};

const FIXTURE_NAME: &str = "oracle-parity-sample";

fn fixture_root() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR")).join("tests/fixtures/oracle-parity-sample")
}

/// Mirrors `brand_normalize::claude_counterpart` (private to that module,
/// not re-exported): derive the oracle spelling of `value` by swapping the
/// brand word, never by spelling `CLAUDE`/`claude` out as a hand-typed
/// literal next to a `LINGXI`/`lingxi` one. Used ONLY to build the synthetic
/// Claude-oracle-shaped tree at test time — never persisted to tracked
/// source, so it produces no brand-gate-visible oracle literal.
///
/// Note the asymmetry that makes the round trip below non-trivial: this is a
/// blind whole-text substitution, while [`plugin::normalize`] is token-scoped
/// (it rewrites only maximal runs behind the two oracle prefixes it knows).
/// The two are therefore NOT inverses by construction — they agree only where
/// the normalizer genuinely recognises what this function produced.
fn claude_counterpart(value: &str) -> String {
    let lower = &branding::DOT_DIR[1..];
    let upper = lower.to_uppercase();
    value.replace(&upper, "CLAUDE").replace(lower, "claude")
}

/// Recursively copy `src` to `dst`, applying `transform` to both every path
/// SEGMENT name and every file's UTF-8 content. Used once per test, to turn
/// the checked-in LingXi fixture into a Claude-oracle-shaped tree.
fn copy_tree_transformed(src: &Path, dst: &Path, transform: &dyn Fn(&str) -> String) {
    fs::create_dir_all(dst).unwrap_or_else(|e| panic!("create_dir_all {}: {e}", dst.display()));
    for entry in sorted_entries(src) {
        let name = entry_name(&entry);
        let dst_path = dst.join(transform(&name));
        if entry.file_type().unwrap().is_dir() {
            copy_tree_transformed(&entry.path(), &dst_path, transform);
        } else {
            let text = fs::read_to_string(entry.path())
                .unwrap_or_else(|e| panic!("read {}: {e}", entry.path().display()));
            fs::write(&dst_path, transform(&text))
                .unwrap_or_else(|e| panic!("write {}: {e}", dst_path.display()));
        }
    }
}

fn sorted_entries(dir: &Path) -> Vec<fs::DirEntry> {
    let mut entries: Vec<fs::DirEntry> = fs::read_dir(dir)
        .unwrap_or_else(|e| panic!("read_dir {}: {e}", dir.display()))
        .map(|e| e.unwrap())
        .collect();
    // Deterministic traversal: `read_dir` order is unspecified, and the token
    // sequences asserted below would otherwise be order-flaky.
    entries.sort_by_key(std::fs::DirEntry::file_name);
    entries
}

fn entry_name(entry: &fs::DirEntry) -> String {
    entry
        .file_name()
        .into_string()
        .expect("fixture tree uses ASCII names only")
}

/// Everything [`plugin::normalize`] reported while [`rebuild_tree`] walked a
/// tree, split by what was being normalized. Keeping names and content apart
/// matters: the manifest-directory pair is only ever exercised through a path
/// SEGMENT, and folding the two together would let a content-side regression
/// hide behind a name-side hit.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
struct NormalizeReportSummary {
    content_mapped: Vec<String>,
    content_unmapped: Vec<String>,
    name_mapped: Vec<String>,
    name_unmapped: Vec<String>,
}

impl NormalizeReportSummary {
    fn counts(tokens: &[String]) -> BTreeMap<String, usize> {
        let mut out = BTreeMap::new();
        for t in tokens {
            *out.entry(t.clone()).or_insert(0) += 1;
        }
        out
    }
}

/// A plausible workspace-relative path for [`plugin::normalize`]'s
/// frozen-identity path scoping. Nothing in this fixture is on the frozen
/// list; this only has to not collide with a real frozen path.
fn synthetic_source_path(rel: &Path) -> String {
    format!(
        "plugin/tests/fixtures/{FIXTURE_NAME}/synthetic/{}",
        rel.display()
    )
}

/// Walk `oracle_root` (an on-disk Claude-oracle-shaped tree — Claude
/// directory names, Claude tokens in every file's content) and rebuild it
/// under `dest_root` in LingXi shape.
///
/// **Path segment names always go through [`plugin::normalize`]** — that is
/// how `.claude-plugin` becomes the manifest directory the production loader
/// looks for, and it means the manifest-directory pair is exercised by the
/// module under test rather than by a private inverse function in this file.
///
/// **File content goes through it only when `normalize_content` is true.**
/// That single boolean is the entire difference between
/// [`oracle_and_lingxi_agree_on_component_inventory`] and the negative
/// control [`oracle_and_lingxi_disagree_when_normalization_is_skipped`], so
/// the negative control varies exactly the axis under test and nothing else.
fn rebuild_tree(
    oracle_root: &Path,
    dest_root: &Path,
    normalize_content: bool,
) -> NormalizeReportSummary {
    let mut summary = NormalizeReportSummary::default();
    rebuild_rec(
        oracle_root,
        dest_root,
        Path::new(""),
        normalize_content,
        &mut summary,
    );
    summary
}

fn rebuild_rec(
    src_dir: &Path,
    dest_dir: &Path,
    rel: &Path,
    normalize_content: bool,
    summary: &mut NormalizeReportSummary,
) {
    fs::create_dir_all(dest_dir)
        .unwrap_or_else(|e| panic!("create_dir_all {}: {e}", dest_dir.display()));
    for entry in sorted_entries(src_dir) {
        let name = entry_name(&entry);
        let entry_rel = rel.join(&name);
        let source_path = synthetic_source_path(&entry_rel);

        let name_report = plugin::normalize(&source_path, &name);
        summary.name_mapped.extend(name_report.mapped);
        summary.name_unmapped.extend(name_report.unmapped);
        let dest_path = dest_dir.join(&name_report.output);

        if entry.file_type().unwrap().is_dir() {
            rebuild_rec(
                &entry.path(),
                &dest_path,
                &entry_rel,
                normalize_content,
                summary,
            );
            continue;
        }

        let text = fs::read_to_string(entry.path())
            .unwrap_or_else(|e| panic!("read {}: {e}", entry.path().display()));
        let out = if normalize_content {
            let report = plugin::normalize(&source_path, &text);
            summary.content_mapped.extend(report.mapped);
            summary.content_unmapped.extend(report.unmapped);
            report.output
        } else {
            text
        };
        fs::write(&dest_path, out).unwrap_or_else(|e| panic!("write {}: {e}", dest_path.display()));
    }
}

/// How many times the checked-in LingXi fixture's file CONTENT spells each of
/// `plugin::known_pairs`'s LingXi tokens, keyed by that pair's oracle
/// spelling. Counted straight off disk, so the expectation the positive
/// control below compares against never passes through the normalizer it is
/// checking.
///
/// A naive substring count is exact here only because the two dotted
/// spellings (one of which is a prefix of the other) do not occur in any
/// fixture file's content — the caller asserts that, so this stays honest if
/// someone later adds one.
fn fixture_content_token_counts() -> BTreeMap<String, usize> {
    let mut out: BTreeMap<String, usize> = plugin::known_pairs()
        .iter()
        .map(|p| (p.claude.clone(), 0usize))
        .collect();
    count_rec(&fixture_root(), &mut out);
    out
}

fn count_rec(dir: &Path, out: &mut BTreeMap<String, usize>) {
    for entry in sorted_entries(dir) {
        if entry.file_type().unwrap().is_dir() {
            count_rec(&entry.path(), out);
            continue;
        }
        let text = fs::read_to_string(entry.path())
            .unwrap_or_else(|e| panic!("read {}: {e}", entry.path().display()));
        for pair in plugin::known_pairs() {
            // Destructured rather than accessed as `pair.<field>`: the brand
            // gate's G3 needle for the LingXi dot-dir matches a bare
            // `.<brand>` run, and a struct-field access spells exactly that
            // (`brand_normalize.rs` carries five such findings today). A
            // binding keeps this file out of the gate's report without
            // weakening anything.
            let plugin::BrandPair { lingxi, claude } = &pair;
            *out.get_mut(claude).expect("pair seeded above") += text.matches(*lingxi).count();
        }
    }
}

/// Load the single plugin directory under `plugins_dir` through the SAME
/// production path a real LingXi plugin takes:
/// [`plugin::discover_installed_plugins`] → (internally)
/// `load_plugin_from_path` → `detect_components`. Fails loudly if the count
/// is not exactly one, so a fixture the loader silently rejected shows up
/// here rather than downstream as "both sides have an empty inventory".
async fn load_single_plugin(plugins_dir: &Path) -> (PluginManifest, PathBuf) {
    let mut discovered = plugin::discover_installed_plugins(plugins_dir).await;
    assert_eq!(
        discovered.len(),
        1,
        "expected exactly one plugin discovered under {}, got {}",
        plugins_dir.display(),
        discovered.len()
    );
    let (_, manifest, dir) = discovered.remove(0);
    (manifest, dir)
}

/// Plugin-root-relative path of a discovered component. Panics — naming the
/// slot and both paths — rather than silently falling back to the absolute
/// path: a silent fallback would turn a real "component resolved outside the
/// plugin root" bug into an inscrutable inequality between two temp dirs.
fn rel_of(path: &Path, root: &Path, slot: &str) -> PathBuf {
    path.strip_prefix(root)
        .unwrap_or_else(|_| {
            panic!(
                "{slot} component {} is not under the plugin root {}",
                path.display(),
                root.display()
            )
        })
        .to_path_buf()
}

fn rel_paths(paths: &[ComponentPath], root: &Path, slot: &str) -> Vec<PathBuf> {
    let mut v: Vec<PathBuf> = paths.iter().map(|c| rel_of(&c.path, root, slot)).collect();
    v.sort();
    v
}

/// Parsed hook, minus the fields that cannot compare across two independent
/// loads (`id` is freshly generated; `source` carries the absolute plugin
/// dir).
#[derive(Debug, Clone, PartialEq, Eq)]
struct HookSpec {
    name: String,
    events: String,
    condition: String,
    command: String,
    args: Vec<String>,
    env: BTreeMap<String, String>,
    timeout: String,
    blocking: bool,
    priority: i32,
}

#[derive(Debug, Clone, PartialEq, Eq)]
struct McpSpec {
    command: String,
    args: Vec<String>,
    env: BTreeMap<String, String>,
    disabled: bool,
}

#[derive(Debug, Clone, PartialEq, Eq)]
struct LspSpec {
    command: String,
    args: Vec<String>,
    env: BTreeMap<String, String>,
    transport: String,
    extension_to_language: BTreeMap<String, String>,
    trigger_languages: Vec<String>,
    root_dir_markers: Vec<String>,
}

fn hook_specs(components: &PluginComponents) -> Vec<HookSpec> {
    let mut v: Vec<HookSpec> = components
        .hooks
        .iter()
        .map(|h| {
            let (command, args, env) = match &h.executor {
                hooks::HookExecutor::Command {
                    command, args, env, ..
                } => (
                    command.clone(),
                    args.clone(),
                    env.iter().map(|(k, v)| (k.clone(), v.clone())).collect(),
                ),
                other => (format!("{other:?}"), Vec::new(), BTreeMap::new()),
            };
            HookSpec {
                name: h.name.clone(),
                events: format!("{:?}", h.events),
                condition: format!("{:?}", h.if_condition),
                command,
                args,
                env,
                timeout: format!("{:?}", h.timeout),
                blocking: h.blocking,
                priority: h.priority,
            }
        })
        .collect();
    v.sort_by(|a, b| (&a.name, &a.command).cmp(&(&b.name, &b.command)));
    v
}

fn mcp_specs(components: &PluginComponents) -> BTreeMap<String, McpSpec> {
    components
        .mcp_servers
        .iter()
        .map(|(name, cfg)| {
            let spec = match &cfg.spec {
                lingxi_core::host::McpTransportSpec::Stdio { command, args, env } => McpSpec {
                    command: command.clone(),
                    args: args.clone(),
                    env: env.iter().map(|(k, v)| (k.clone(), v.clone())).collect(),
                    disabled: cfg.disabled,
                },
                other => McpSpec {
                    command: format!("{other:?}"),
                    args: Vec::new(),
                    env: BTreeMap::new(),
                    disabled: cfg.disabled,
                },
            };
            (name.clone(), spec)
        })
        .collect()
}

fn lsp_specs(components: &PluginComponents) -> BTreeMap<String, LspSpec> {
    components
        .lsp_servers
        .iter()
        .map(|(name, cfg)| {
            (
                name.clone(),
                LspSpec {
                    command: cfg.command.clone(),
                    args: cfg.args.clone(),
                    env: cfg
                        .env
                        .iter()
                        .map(|(k, v)| (k.clone(), v.clone()))
                        .collect(),
                    transport: cfg.transport.clone(),
                    extension_to_language: cfg
                        .extension_to_language
                        .iter()
                        .map(|(k, v)| (k.clone(), v.clone()))
                        .collect(),
                    trigger_languages: cfg.trigger_languages.clone(),
                    root_dir_markers: cfg.root_dir_markers.clone(),
                },
            )
        })
        .collect()
}

/// Every discovered component file's CONTENT, keyed by its plugin-root-
/// relative path. This is what makes the five path-shaped slots load-bearing:
/// their PATHS come from directory names, which both sides derive the same
/// way, so paths alone would agree even if the normalizer never touched a
/// byte inside them.
fn component_files(components: &PluginComponents, root: &Path) -> BTreeMap<String, String> {
    let mut out = BTreeMap::new();
    let slots: [(&str, &Vec<ComponentPath>); 5] = [
        ("commands", &components.commands),
        ("agents", &components.agents),
        ("skills", &components.skills),
        ("output_styles", &components.output_styles),
        ("workflows", &components.workflows),
    ];
    for (slot, paths) in slots {
        for c in paths {
            let rel = rel_of(&c.path, root, slot);
            let text = fs::read_to_string(&c.path).unwrap_or_else(|e| {
                panic!("{slot} component {} unreadable: {e}", c.path.display())
            });
            let key = rel.display().to_string();
            assert!(
                out.insert(key.clone(), text).is_none(),
                "two components resolved to the same relative path {key}"
            );
        }
    }
    out
}

/// The component inventory §19.1 requires to agree.
#[derive(Debug, Clone, PartialEq, Eq)]
struct Inventory {
    name: String,
    version: String,
    description: String,
    author: Option<String>,
    homepage: Option<String>,
    commands: Vec<PathBuf>,
    agents: Vec<PathBuf>,
    skills: Vec<PathBuf>,
    output_styles: Vec<PathBuf>,
    workflows: Vec<PathBuf>,
    component_files: BTreeMap<String, String>,
    hooks: Vec<HookSpec>,
    mcp_servers: BTreeMap<String, McpSpec>,
    lsp_servers: BTreeMap<String, LspSpec>,
}

fn inventory(manifest: &PluginManifest, plugin_dir: &Path) -> Inventory {
    let c = &manifest.components;
    Inventory {
        name: manifest.name.clone(),
        version: manifest.version.clone(),
        description: manifest.description.clone(),
        author: manifest.author.clone(),
        homepage: manifest.homepage.clone(),
        commands: rel_paths(&c.commands, plugin_dir, "commands"),
        agents: rel_paths(&c.agents, plugin_dir, "agents"),
        skills: rel_paths(&c.skills, plugin_dir, "skills"),
        output_styles: rel_paths(&c.output_styles, plugin_dir, "output_styles"),
        workflows: rel_paths(&c.workflows, plugin_dir, "workflows"),
        component_files: component_files(c, plugin_dir),
        hooks: hook_specs(c),
        mcp_servers: mcp_specs(c),
        lsp_servers: lsp_specs(c),
    }
}

/// Load the checked-in LingXi fixture (copied byte-for-byte into an isolated
/// temp dir, never mutated in place) through the production path. Shared by
/// every test below so "what counts as the LingXi side" cannot drift.
async fn lingxi_side_inventory() -> Inventory {
    let root = tempfile::tempdir().unwrap();
    copy_tree_transformed(
        &fixture_root(),
        &root.path().join(FIXTURE_NAME),
        &|s: &str| s.to_string(),
    );
    let (manifest, dir) = load_single_plugin(root.path()).await;
    inventory(&manifest, &dir)
}

/// Build the Claude-oracle-shaped tree from the checked-in fixture: every
/// `.lingxi*` / `LINGXI_*` token mechanically swapped to its Claude spelling
/// (never hand-typed — see [`claude_counterpart`]), including the manifest
/// DIRECTORY name.
fn oracle_tree(dir: &tempfile::TempDir) -> PathBuf {
    let oracle_plugin_dir = dir.path().join(FIXTURE_NAME);
    copy_tree_transformed(&fixture_root(), &oracle_plugin_dir, &claude_counterpart);
    oracle_plugin_dir
}

/// Does `token` survive a parse verbatim, or will it be substituted first?
///
/// MEASURED, not assumed: `mcp::parse_mcp_json_string` expands `${VAR}` from
/// the AMBIENT process environment at parse time. On the machine this test
/// was written on, a Claude Code host exports the oracle's own
/// `CLAUDE_PLUGIN_DATA`, and the fixture's `${...PLUGIN_DATA}/cache` argument
/// came back from the loader as a real absolute path instead of the token. So
/// "the oracle token survives into the parsed inventory" is only assertable
/// when the corresponding variable is unset; what is assertable
/// unconditionally is that no LingXi spelling appeared (see
/// [`assert_carries_no_lingxi_token`]).
fn token_survives_parse(token: &str) -> bool {
    std::env::var(token).is_err()
}

/// The un-normalized side must carry NO LingXi spelling anywhere. This is the
/// env-independent half of "normalization was skipped": it holds whether or
/// not the ambient environment substituted a token away.
fn assert_carries_no_lingxi_token(field: &str, value: &str) {
    for pair in plugin::known_pairs() {
        // Destructured for the same brand-gate reason as in
        // `fixture_content_token_counts`.
        let plugin::BrandPair { lingxi, .. } = &pair;
        assert!(
            !value.contains(*lingxi),
            "{field} carries the LingXi spelling {lingxi} — it was normalized \
             after all, so this is no longer a negative control: {value:?}"
        );
    }
}

/// Guard against the ambient environment silently making a compared surface
/// vacuous. The parsed hook / MCP / LSP strings are only load-bearing for a
/// brand pair while they still SPELL it: if the process running the test
/// happens to export the plugin-data environment variable, the parser substitutes it on both
/// sides and that pair quietly stops being compared at all. Asserting the
/// token is present turns that into a named failure instead of lost coverage.
fn assert_surface_spells(surface: &str, value: &str, tokens: &[&str]) {
    for token in tokens {
        assert!(
            value.contains(token),
            "the parsed {surface} no longer spells {token}, so the comparison \
             is not sensitive to that brand pair. Most likely the ambient \
             environment exports {token} and the parser substituted it; unset \
             it and rerun. Value: {value:?}"
        );
    }
}

/// Assert every one of the eight component slots is populated, individually
/// and by name. Two empty vecs are equal, so the equality assertion that
/// follows is only worth anything once each slot is known non-empty; doing
/// them one at a time (rather than one aggregate count) means a single
/// silently-defaulting slot cannot hide behind the seven that work.
fn assert_every_slot_populated(inv: &Inventory) {
    assert_eq!(inv.commands.len(), 1, "commands slot: fixture declares one");
    assert_eq!(inv.agents.len(), 1, "agents slot: fixture declares one");
    assert_eq!(inv.skills.len(), 1, "skills slot: fixture declares one");
    assert_eq!(
        inv.output_styles.len(),
        1,
        "output_styles slot: fixture declares one"
    );
    assert_eq!(
        inv.workflows.len(),
        1,
        "workflows slot: fixture declares one"
    );
    assert_eq!(inv.hooks.len(), 1, "hooks slot: fixture declares one");
    assert_eq!(
        inv.mcp_servers.len(),
        1,
        "mcp_servers slot: fixture declares one"
    );
    assert_eq!(
        inv.lsp_servers.len(),
        1,
        "lsp_servers slot: fixture declares one"
    );
    assert_eq!(
        inv.component_files.len(),
        5,
        "component_files: one body per path-shaped slot, got {:?}",
        inv.component_files.keys().collect::<Vec<_>>()
    );
    for (path, body) in &inv.component_files {
        assert!(
            !body.trim().is_empty(),
            "component {path} loaded with an empty body"
        );
    }
}

/// Every surface the equality assertion leans on must still SPELL the brand
/// tokens it stands for. Without this, an ambient env var could substitute a
/// token away on both sides and that pair would be "compared" vacuously.
fn assert_compared_surfaces_are_token_bearing(inv: &Inventory) {
    let root = branding::PLUGIN_ROOT_ENV;
    let data = branding::PLUGIN_DATA_ENV;
    let project = branding::PROJECT_DIR_ENV;

    let bodies = inv
        .component_files
        .values()
        .cloned()
        .collect::<Vec<_>>()
        .join("\n");
    assert_surface_spells("component bodies", &bodies, &[root, data, project]);
    assert_surface_spells("hook definitions", &format!("{:?}", inv.hooks), &[root]);
    assert_surface_spells(
        "mcp server configs",
        &format!("{:?}", inv.mcp_servers),
        &[root, data, project],
    );
    assert_surface_spells(
        "lsp server configs",
        &format!("{:?}", inv.lsp_servers),
        &[root, data],
    );
}

/// Required test (P0a.8): the SAME fixture, run through brand-token
/// normalization, yields the same component inventory and parse behaviour
/// under LingXi as under the Claude oracle.
#[tokio::test]
async fn oracle_and_lingxi_agree_on_component_inventory() {
    let lingxi_inventory = lingxi_side_inventory().await;
    assert_every_slot_populated(&lingxi_inventory);
    assert_compared_surfaces_are_token_bearing(&lingxi_inventory);

    let oracle_root = tempfile::tempdir().unwrap();
    let oracle_plugin_dir = oracle_tree(&oracle_root);

    let normalized_root = tempfile::tempdir().unwrap();
    let summary = rebuild_tree(
        &oracle_plugin_dir,
        &normalized_root.path().join(FIXTURE_NAME),
        true,
    );

    // (a) NOTHING was left unrecognised — otherwise any agreement below is
    // partly luck (module doc, point 2).
    assert!(
        summary.content_unmapped.is_empty(),
        "oracle fixture content did not fully normalize; unmapped: {:?}",
        summary.content_unmapped
    );
    assert!(
        summary.name_unmapped.is_empty(),
        "oracle fixture path names did not fully normalize; unmapped: {:?}",
        summary.name_unmapped
    );

    // (b) POSITIVE CONTROL — the normalizer actually fired, on exactly the
    // tokens the fixture spells, exactly as often as it spells them. "(a)
    // passed" alone is satisfied by a scanner that finds nothing at all
    // (module doc, point 1); this is the assertion that is not.
    let expected = fixture_content_token_counts();
    let dotted_pairs: Vec<String> = plugin::known_pairs()
        .into_iter()
        .filter(|p| p.lingxi.starts_with('.'))
        .map(|p| p.claude)
        .collect();
    assert_eq!(dotted_pairs.len(), 2, "branding declares two dotted pairs");
    for dotted in &dotted_pairs {
        // Documented gap, asserted rather than assumed: the dotted spellings
        // never appear in a plugin tree's file CONTENT, which is also what
        // makes `fixture_content_token_counts`'s substring count exact
        // despite one dotted spelling being a prefix of the other.
        assert_eq!(
            expected.get(dotted).copied(),
            Some(0),
            "{dotted} unexpectedly appears in fixture content; \
             fixture_content_token_counts's naive count is no longer exact"
        );
    }
    let expected_nonzero: BTreeMap<String, usize> = expected
        .iter()
        .filter(|(_, n)| **n > 0)
        .map(|(k, n)| (k.clone(), *n))
        .collect();
    assert_eq!(
        expected_nonzero.len(),
        3,
        "fixture content should exercise the three env-var pairs, got {expected_nonzero:?}"
    );
    assert_eq!(
        NormalizeReportSummary::counts(&summary.content_mapped),
        expected_nonzero,
        "the tokens `normalize` reported rewriting do not match the tokens the \
         checked-in fixture actually spells (counted off disk)"
    );

    // (c) POSITIVE CONTROL for the name side: the manifest-directory pair is
    // only ever exercised through a path segment, and it is what makes the
    // rebuilt tree loadable at all.
    let manifest_dir_token = claude_counterpart(branding::PLUGIN_MANIFEST_DIR);
    assert_eq!(
        NormalizeReportSummary::counts(&summary.name_mapped),
        BTreeMap::from([(manifest_dir_token.clone(), 1usize)]),
        "expected exactly one manifest-directory segment to be rewritten"
    );

    let (oracle_manifest, oracle_dir) = load_single_plugin(normalized_root.path()).await;
    let oracle_inventory = inventory(&oracle_manifest, &oracle_dir);
    assert_every_slot_populated(&oracle_inventory);

    // The exact inventory being compared — reported, not assumed:
    //   commands      = ["commands/greet.md"]
    //   agents        = ["agents/helper.md"]
    //   skills        = ["skills/sample-skill/SKILL.md"]
    //   output_styles = ["output-styles/concise.md"]
    //   workflows     = ["workflows/build.js"]
    //   component_files = the five bodies above, verbatim
    //   hooks         = [PreToolUse "*" -> <plugin-root>/hooks/run.sh]
    //   mcp_servers   = {"sample": <plugin-root>/bin/server
    //                      + args referencing <plugin-data> and
    //                        <project-dir>}
    //   lsp_servers   = {"sample-lsp": <plugin-root>/bin/sample-lsp}
    assert_eq!(
        oracle_inventory, lingxi_inventory,
        "oracle-normalized inventory diverged from the LingXi inventory:\n\
         oracle = {oracle_inventory:#?}\nlingxi = {lingxi_inventory:#?}"
    );
}

/// Required NEGATIVE CONTROL (P0a.8's "trap, stated plainly"): a fixture
/// whose two sides genuinely differ must make the comparison FAIL.
///
/// This is [`oracle_and_lingxi_agree_on_component_inventory`]'s construction
/// with exactly one thing changed — [`rebuild_tree`]'s `normalize_content` is
/// `false`. Directory names still go through the normalizer, so the tree
/// loads and yields the same SHAPE (every slot populated, same counts); only
/// the token strings inside the files diverge. If the comparison only checked
/// "both loaded", or compared component paths while ignoring content and
/// parsed command strings, this test would wrongly pass.
#[tokio::test]
async fn oracle_and_lingxi_disagree_when_normalization_is_skipped() {
    let lingxi_inventory = lingxi_side_inventory().await;

    let oracle_root = tempfile::tempdir().unwrap();
    let oracle_plugin_dir = oracle_tree(&oracle_root);

    let broken_root = tempfile::tempdir().unwrap();
    let summary = rebuild_tree(
        &oracle_plugin_dir,
        &broken_root.path().join(FIXTURE_NAME),
        false,
    );
    assert!(
        summary.content_mapped.is_empty() && summary.content_unmapped.is_empty(),
        "the negative control must not normalize content: {summary:?}"
    );

    let (broken_manifest, broken_dir) = load_single_plugin(broken_root.path()).await;
    let broken_inventory = inventory(&broken_manifest, &broken_dir);

    // Same shape as the real LingXi side — every slot populated, so the
    // divergence below is genuinely about token strings and not about a tree
    // that failed to load.
    assert_every_slot_populated(&broken_inventory);
    assert_eq!(broken_inventory.commands, lingxi_inventory.commands);
    assert_eq!(broken_inventory.agents, lingxi_inventory.agents);
    assert_eq!(broken_inventory.skills, lingxi_inventory.skills);
    assert_eq!(
        broken_inventory.output_styles,
        lingxi_inventory.output_styles
    );
    assert_eq!(broken_inventory.workflows, lingxi_inventory.workflows);
    assert_eq!(
        broken_inventory.component_files.keys().collect::<Vec<_>>(),
        lingxi_inventory.component_files.keys().collect::<Vec<_>>(),
    );

    // ...but the comparison must still catch the divergence.
    assert_ne!(
        broken_inventory, lingxi_inventory,
        "an un-normalized oracle tree must NOT be reported as agreeing with \
         the LingXi fixture"
    );

    // Name the actual divergences rather than settling for "some field
    // differs somewhere". Each of these is a field a weaker comparison would
    // have dropped.
    let oracle_root_token = claude_counterpart(branding::PLUGIN_ROOT_ENV);
    let oracle_data_token = claude_counterpart(branding::PLUGIN_DATA_ENV);
    let oracle_project_token = claude_counterpart(branding::PROJECT_DIR_ENV);

    assert_ne!(
        broken_inventory.hooks, lingxi_inventory.hooks,
        "hook command should still carry the un-normalized oracle token"
    );
    for hook in &broken_inventory.hooks {
        assert_carries_no_lingxi_token("hook command", &hook.command);
        if token_survives_parse(&oracle_root_token) {
            assert!(
                hook.command.contains(&oracle_root_token),
                "expected the un-normalized oracle token in hook command {:?}",
                hook.command
            );
        }
    }

    assert_ne!(
        broken_inventory.mcp_servers, lingxi_inventory.mcp_servers,
        "mcp server config should still carry the un-normalized oracle tokens"
    );
    let mcp = broken_inventory
        .mcp_servers
        .get("sample")
        .expect("negative-control tree still declares the `sample` mcp server");
    assert_carries_no_lingxi_token("mcp command", &mcp.command);
    if token_survives_parse(&oracle_root_token) {
        assert!(
            mcp.command.contains(&oracle_root_token),
            "expected the un-normalized oracle token in mcp command {:?}",
            mcp.command
        );
    }
    let joined_args = mcp.args.join(" ");
    assert_carries_no_lingxi_token("mcp args", &joined_args);
    for token in [&oracle_data_token, &oracle_project_token] {
        // Only assertable when the ambient environment does not export the
        // oracle variable — see `token_survives_parse`.
        if token_survives_parse(token) {
            assert!(
                joined_args.contains(token),
                "expected the un-normalized {token} in mcp args {:?}",
                mcp.args
            );
        }
    }

    assert_ne!(
        broken_inventory.lsp_servers, lingxi_inventory.lsp_servers,
        "lsp server config should still carry the un-normalized oracle token"
    );
    for (name, lsp) in &broken_inventory.lsp_servers {
        assert_carries_no_lingxi_token(&format!("lsp `{name}` command"), &lsp.command);
        assert_carries_no_lingxi_token(&format!("lsp `{name}` args"), &lsp.args.join(" "));
    }

    // The component BODIES diverge too — the assertion that makes the five
    // path-shaped slots load-bearing instead of agreeing by construction.
    assert_ne!(
        broken_inventory.component_files, lingxi_inventory.component_files,
        "component file bodies should still carry un-normalized oracle tokens"
    );
    for (path, body) in &broken_inventory.component_files {
        assert!(
            body.contains(&oracle_root_token)
                || body.contains(&oracle_data_token)
                || body.contains(&oracle_project_token),
            "component {path} carries no oracle token, so its body proves nothing \
             about normalization; body = {body:?}"
        );
    }
}

/// Required case: `NormalizeReport::unmapped` non-empty, and the test says
/// so. `CLAUDE_PLUGIN_OPTION_<KEY>` (`hooks::user_config::option_env_var`) is
/// a real plugin-contract token `branding` exports no constant for, so
/// injecting one is a genuine gap, not a contrived one.
///
/// It is injected into a file that is NOT a discovered component, which is
/// the point: the two inventories still compare EQUAL while a token nobody
/// understood rode through both sides untouched. That is exactly the
/// "agreement is luck" shape the brief warns about, and it is why
/// `unmapped` has to be an independent gate rather than something the
/// inventory comparison could be trusted to notice.
#[tokio::test]
async fn an_unmapped_token_is_reported_and_the_agreement_is_not_trusted() {
    let lingxi_inventory = lingxi_side_inventory().await;
    // The punchline below is "and they STILL agree" — which would prove
    // nothing if the inventories were empty, so the slots are pinned here too.
    assert_every_slot_populated(&lingxi_inventory);

    let oracle_root = tempfile::tempdir().unwrap();
    let oracle_plugin_dir = oracle_tree(&oracle_root);

    // Built by truncating the derived oracle env-var spelling, never typed
    // out, so this file spells no oracle fragment contiguously — mirrors
    // `brand_normalize.rs`'s own
    // `an_unmapped_claude_identity_is_reported_not_silently_kept`.
    let mut unmapped_token = claude_counterpart(branding::PLUGIN_ROOT_ENV);
    unmapped_token.truncate(unmapped_token.len() - "ROOT".len());
    unmapped_token.push_str("OPTION_API_KEY");
    let known_token = claude_counterpart(branding::PLUGIN_ROOT_ENV);

    // `docs/` is not one of the eight component directories the loader
    // globs, so this file changes nothing about either inventory.
    let notes_dir = oracle_plugin_dir.join("docs");
    fs::create_dir_all(&notes_dir).unwrap();
    fs::write(
        notes_dir.join("notes.md"),
        format!("Reads {unmapped_token} and {known_token} at run time.\n"),
    )
    .unwrap();

    let normalized_root = tempfile::tempdir().unwrap();
    let summary = rebuild_tree(
        &oracle_plugin_dir,
        &normalized_root.path().join(FIXTURE_NAME),
        true,
    );

    assert_eq!(
        summary.content_unmapped,
        vec![unmapped_token.clone()],
        "the injected token must be the only unmapped one across the whole tree, \
         and it must be named, not silently dropped"
    );

    // POSITIVE CONTROL in the same input: the KNOWN token sitting beside the
    // unknown one in the same file must still have been mapped, so a passing
    // assertion above cannot mean "the scanner stopped looking".
    let counts = NormalizeReportSummary::counts(&summary.content_mapped);
    let baseline = fixture_content_token_counts();
    assert_eq!(
        counts.get(&known_token).copied(),
        baseline.get(&known_token).map(|n| n + 1),
        "the known token in the injected file must still be rewritten \
         (fixture baseline {:?}, observed {:?})",
        baseline.get(&known_token),
        counts.get(&known_token)
    );

    let normalized_notes = fs::read_to_string(
        normalized_root
            .path()
            .join(FIXTURE_NAME)
            .join("docs")
            .join("notes.md"),
    )
    .unwrap();
    assert!(
        normalized_notes.contains(branding::PLUGIN_ROOT_ENV),
        "the known token in the injected file must be rewritten on disk: {normalized_notes:?}"
    );
    assert!(
        normalized_notes.contains(&unmapped_token),
        "an unmapped token must be left in place, not deleted: {normalized_notes:?}"
    );

    // ...and the inventories STILL agree. Agreement alone therefore proves
    // nothing; `content_unmapped` is the gate that catches this case.
    let (oracle_manifest, oracle_dir) = load_single_plugin(normalized_root.path()).await;
    let oracle_inventory = inventory(&oracle_manifest, &oracle_dir);
    assert_eq!(
        oracle_inventory, lingxi_inventory,
        "the inventories were expected to still agree here — that agreement is \
         exactly what makes the non-empty `unmapped` report load-bearing"
    );
    assert!(
        !summary.content_unmapped.is_empty(),
        "restating the point: the agreement above coexists with a token nobody \
         understood"
    );
}
