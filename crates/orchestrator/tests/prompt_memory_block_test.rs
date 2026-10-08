//! Memory files render through the prompt memory block.

use memory::lingxi_md::LingxiMdTier;
use orchestrator::prompt::{memory_block, MemoryFile};
use std::path::PathBuf;

fn mf(path: &str, body: &str, tier: LingxiMdTier) -> MemoryFile {
    MemoryFile {
        parent: None,
        source_content: None,
        path: PathBuf::from(path),
        body: body.into(),
        is_local_override: tier == LingxiMdTier::Local,
        tier,
        globs: None,
        raw_content: body.into(),
        content_differs_from_disk: false,
    }
}

// The verbatim preamble (claudemd.ts:89-90) that opens the memory section.
const PREAMBLE: &str = "Codebase and user instructions are shown below. Be sure to adhere to these instructions. IMPORTANT: These instructions OVERRIDE any default behavior and you MUST follow them exactly as written.";

#[test]
fn empty_input_returns_empty_string_no_tags() {
    let out = memory_block::format(&[]);
    assert_eq!(out, "");
}

#[test]
fn single_entry_shape() {
    // GAP 3: 1:1 with claude-code getLingxiMds — preamble + `Contents of …`,
    // tier description, trimmed body. NO enclosing tag, NO trailing newline.
    let out = memory_block::format(&[mf(
        "/home/u/.lingxi/LINGXI.md",
        "global notes",
        LingxiMdTier::User,
    )]);
    let expected = format!(
        "{PREAMBLE}\n\n\
Contents of /home/u/.lingxi/LINGXI.md (user's private global instructions for all projects):\n\n\
global notes"
    );
    assert_eq!(out, expected);
}

#[test]
fn multi_entry_splice_order_locked() {
    // Caller is responsible for ordering — formatter just emits.
    // Order verified here: User, then Project, then Local. Each tier gets its
    // own description; blocks are joined by a double newline; no trailing newline.
    let out = memory_block::format(&[
        mf("/home/u/.lingxi/LINGXI.md", "home", LingxiMdTier::User),
        mf("/proj/LINGXI.md", "repo", LingxiMdTier::Project),
        mf("/proj/LINGXI.local.md", "local", LingxiMdTier::Local),
    ]);
    let expected = format!(
        "{PREAMBLE}\n\n\
Contents of /home/u/.lingxi/LINGXI.md (user's private global instructions for all projects):\n\n\
home\n\n\
Contents of /proj/LINGXI.md (project instructions, checked into the codebase):\n\n\
repo\n\n\
Contents of /proj/LINGXI.local.md (user's private project instructions, not checked in):\n\n\
local"
    );
    assert_eq!(out, expected);
}

#[test]
fn managed_tier_uses_organization_managed_description() {
    // Binary `getLingxiMds` (`nUt`) gives Managed its OWN description
    // "(organization-managed policy instructions)" — it does NOT share the User
    // "global instructions" wording (the prior claudemd.ts:1177 citation was
    // stale src; verified against the v2.1.193 binary switch).
    let out = memory_block::format(&[mf(
        "/Library/Application Support/LingXi/LINGXI.md",
        "policy",
        LingxiMdTier::Managed,
    )]);
    let expected = format!(
        "{PREAMBLE}\n\n\
Contents of /Library/Application Support/LingXi/LINGXI.md (organization-managed policy instructions):\n\n\
policy"
    );
    assert_eq!(out, expected);
}

#[test]
fn acquired_named_instruction_with_paths_is_rendered_eagerly() {
    // Current native xJ/L0n tests acquired content truthiness; the distinct
    // conditional rule-directory filter runs during acquisition.
    let included = MemoryFile {
        parent: None,
        source_content: None,
        path: PathBuf::from("/proj/.lingxi/rules/always.md"),
        body: "always".into(),
        is_local_override: false,
        tier: LingxiMdTier::Project,
        globs: None,
        raw_content: "always".into(),
        content_differs_from_disk: false,
    };
    let named = MemoryFile {
        parent: None,
        source_content: None,
        path: PathBuf::from("/proj/LINGXI.md"),
        body: "scoped".into(),
        is_local_override: false,
        tier: LingxiMdTier::Project,
        globs: Some(vec!["src".into()]),
        raw_content: "scoped".into(),
        content_differs_from_disk: false,
    };
    let out = memory_block::format(&[included, named]);
    assert_eq!(out, format!("{PREAMBLE}\n\nContents of /proj/.lingxi/rules/always.md (project instructions, checked into the codebase):\n\nalways\n\nContents of /proj/LINGXI.md (project instructions, checked into the codebase):\n\nscoped"));
}

#[tokio::test]
async fn real_provider_loads_in_spec_splice_order_via_temp_repo() {
    use orchestrator::prompt::memory_block::{
        MemoryHierarchyProvider, RealMemoryHierarchyProvider,
    };

    let tmp = tempfile::TempDir::new().unwrap();
    let home = tmp.path().join("home");
    std::fs::create_dir_all(home.join(".lingxi")).unwrap();
    std::fs::write(home.join(".lingxi").join("LINGXI.md"), "HOME").unwrap();

    let proj = tmp.path().join("proj");
    let rules = proj.join(".lingxi").join("rules");
    std::fs::create_dir_all(&rules).unwrap();
    std::fs::write(proj.join("LINGXI.md"), "REPO").unwrap();
    std::fs::write(proj.join("LINGXI.local.md"), "LOCAL").unwrap();
    // Native eager acquisition retains unconditional rules and excludes
    // conditional rule-directory projections; lazy activation walks afresh.
    std::fs::write(rules.join("a-always.md"), "ALWAYS").unwrap();
    std::fs::write(
        rules.join("b-scoped.md"),
        "---\npaths: src/**\n---\nSCOPED\n",
    )
    .unwrap();

    // Override HOME so dirs::home_dir() points at our temp home, and point the
    // managed dir at an empty temp dir so the real platform managed path (which
    // may exist on the test machine) does not leak entries into this assertion.
    // SAFETY: env-var mutation in a test is acceptable; tests run
    // single-threaded by default in cargo test's default runner unless
    // explicitly configured. If a parallel runner races other tests
    // also touching HOME, this test may flake — acceptable for M5-03.
    std::env::set_var("HOME", &home);
    let empty_managed = tmp.path().join("no-managed");
    std::env::set_var(
        memory::lingxi_md::hierarchy::MANAGED_DIR_ENV,
        &empty_managed,
    );

    let p = RealMemoryHierarchyProvider;
    let files = p.load(&proj).await;
    std::env::remove_var(memory::lingxi_md::hierarchy::MANAGED_DIR_ENV);
    let bodies: Vec<String> = files.iter().map(|f| f.body.clone()).collect();
    // Eager splice order: HOME → REPO → unconditional rule → LOCAL.
    assert_eq!(bodies, vec!["HOME", "REPO", "ALWAYS", "LOCAL"]);
    // The included unconditional rule carries no globs; tiers are tagged.
    let always = files.iter().find(|f| f.body == "ALWAYS").unwrap();
    assert!(always.globs.is_none());
    assert_eq!(always.tier, LingxiMdTier::Project);
    // Native eager rule-directory acquisition excludes the conditional rule.
    assert!(!files.iter().any(|file| file.path.ends_with("b-scoped.md")));

    let eager = memory_block::format(&files);
    assert!(eager.contains("ALWAYS"));
    assert!(
        !eager.contains("SCOPED"),
        "conditional (paths:-gated) rule must NOT appear in the eager block"
    );
}

#[test]
fn rendered_into_context_matches_format_output() {
    // Anti-drift: the seeding site (`seed_memory_read_state`) must decide
    // `seededFromContext` with the SAME predicate the memory-block renderer
    // uses to decide what the model actually sees. The oracle hand-writes
    // `MLu` (@230809370) as a duplicate of the renderer's drops; LingXi
    // DERIVES the renderer's filter from `is_rendered_into_context`, so this
    // test pins that they cannot diverge.
    let mut cond = mf(
        "/proj/.lingxi/rules/cond.md",
        "conditional body",
        LingxiMdTier::Project,
    );
    cond.globs = Some(vec!["src".into()]);
    let mut blank = mf("/proj/blank.md", "   \n  ", LingxiMdTier::Project);
    blank.globs = None;
    let files = vec![
        mf("/home/u/.lingxi/LINGXI.md", "home", LingxiMdTier::User),
        cond,
        mf("/proj/LINGXI.md", "repo", LingxiMdTier::Project),
        blank,
    ];

    let rendered = memory_block::format(&files);
    for f in &files {
        let header = format!("Contents of {}", f.path.display());
        assert_eq!(
            memory_block::is_rendered_into_context(f),
            rendered.contains(&header),
            "predicate disagrees with the renderer for {}",
            f.path.display()
        );
    }
    // Acquired nonempty content is truthy, including whitespace and globs.
    assert!(memory_block::is_rendered_into_context(&files[0]));
    assert!(memory_block::is_rendered_into_context(&files[1]));
    assert!(memory_block::is_rendered_into_context(&files[2]));
    assert!(memory_block::is_rendered_into_context(&files[3]));
}
