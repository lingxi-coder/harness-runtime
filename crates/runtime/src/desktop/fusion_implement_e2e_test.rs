//! Fusion implement mode against a real git repository.
//!
//! The orchestrator, the analyst packing, the material renderer and the
//! production `PosixWorktreeManager` run for real. What is faked is only what
//! needs a provider or the desktop session: the panels are a spawner that
//! edits the files in the worktree it is given (as a model's Edit/Write calls
//! would), the analyst is a canned side query, and verification runs each
//! command with a plain `sh -c` in the worktree. The OS sandbox is therefore
//! NOT exercised here (it needs a live session tool context); its own tests
//! live in `fusion_implement.rs` and `sandbox`.
//!
//! What this pins is the part unit fakes cannot: that the snapshot base really
//! contains the user's uncommitted work, that each panel's worktree starts
//! from it, that patches are taken against it and apply to it, that the main
//! working tree is never touched, and that worktrees are discarded or kept as
//! designed.

use async_trait::async_trait;
use fusion::{CatalogModel, FusionOrchestrator, FusionRuntimeConfig, ModelLimits};
use lingxi_core::host::budget::{BudgetEnforcerHandle, BudgetError};
use lingxi_core::host::subagent_spawn::{
    SubagentInheritance, SubagentResult, SubagentSpawnError, SubagentSpawnRequest, SubagentSpawner,
    SubagentUsage,
};
use lingxi_core::host::tool_invoker::{SubagentInvocationContext, ToolInvoker, ToolInvokerError};
use lingxi_core::host::{
    FusionExecutor, FusionImplementHost, FusionInheritance, FusionModelHints, FusionModelRef,
    FusionOrigin, FusionPanelMode, FusionPreset, FusionRequest, FusionStatus, PanelClaim,
    PanelEvidence, PanelReport, PanelVerification, VerificationOutcome, VerificationRun,
    WorktreeManager, DEFAULT_IMPLEMENT_FUSION_DIMENSIONS,
};
use lingxi_core::types::AgentId;
use platform_posix::worktree::PosixWorktreeManager;
use serde_json::{json, Value};
use sidequery::{
    SideQueryClient, SideQueryError, SideQueryRequest, SideQueryResponse,
    StrictStructuredQueryRequest, StrictStructuredQueryResponse,
};
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};
use std::time::Duration;
use tokio_util::sync::CancellationToken;

const MODELS: [&str; 3] = ["m-a", "m-b", "m-c"];

fn git(dir: &Path, args: &[&str]) -> String {
    let out = std::process::Command::new("git")
        .args(["-c", "user.name=t", "-c", "user.email=t@example.com"])
        .args(["-c", "commit.gpgsign=false", "-c", "core.autocrlf=false"])
        .args(args)
        .current_dir(dir)
        .env("GIT_CONFIG_GLOBAL", "/dev/null")
        .env("GIT_CONFIG_SYSTEM", "/dev/null")
        .output()
        .expect("git runs");
    assert!(
        out.status.success(),
        "git {args:?} failed: {}",
        String::from_utf8_lossy(&out.stderr)
    );
    String::from_utf8_lossy(&out.stdout).into_owned()
}

fn write(root: &Path, rel: &str, body: &[u8]) {
    let path = root.join(rel);
    std::fs::create_dir_all(path.parent().unwrap()).unwrap();
    std::fs::write(path, body).unwrap();
}

/// A repository with committed history plus the user's uncommitted work: an
/// edited tracked file, an untracked file, and an ignored build directory.
fn user_repo(root: &Path) {
    git(root, &["init", "-q", "-b", "main"]);
    write(root, ".gitignore", b"target/\n");
    write(root, "src/lib.rs", b"pub fn retries() -> u32 { 1 }\n");
    write(root, "old.txt", b"obsolete\n");
    write(root, "data.bin", &[0, 1, 2, 3, 0, 255, 254, 0]);
    git(root, &["add", "-A"]);
    git(root, &["commit", "-q", "-m", "initial"]);
    // What the user has not committed yet.
    write(root, "src/lib.rs", b"pub fn retries() -> u32 { 2 }\n");
    write(root, "scratch.txt", b"my notes\n");
    write(
        root,
        "target/junk.o",
        b"build output that must never be copied\n",
    );
}

struct InertInvoker;
#[async_trait]
impl ToolInvoker for InertInvoker {
    async fn invoke(
        &self,
        _: &str,
        _: Value,
        _: SubagentInvocationContext,
    ) -> Result<Value, ToolInvokerError> {
        Ok(Value::Null)
    }
    fn as_any(&self) -> &dyn std::any::Any {
        self
    }
}

struct InertBudget;
#[async_trait]
impl BudgetEnforcerHandle for InertBudget {
    async fn check_and_charge(&self, _: u64) -> Result<(), BudgetError> {
        Ok(())
    }
    async fn snapshot_total_nano_usd(&self) -> u64 {
        0
    }
}

/// Panels that change the code the way a model's edit tools would.
struct EditingPanels {
    /// This model writes a file and then fails, as a panel that runs out of
    /// turns or hits a provider error mid-change does.
    fail_after_editing: Option<&'static str>,
    /// `(model, cwd)` of every implementer spawn.
    spawns: Mutex<Vec<(String, PathBuf)>>,
    /// Contents of `src/lib.rs` each panel saw when it started.
    saw_at_start: Mutex<Vec<(String, String, bool, bool)>>,
}

fn panel_report(answer: &str) -> PanelReport {
    PanelReport {
        schema_version: 1,
        summary: format!("summary {answer}"),
        candidate_answer: answer.into(),
        claims: vec![PanelClaim {
            statement: "the retry count changed".into(),
            evidence_refs: vec!["e1".into()],
            confidence: 80,
        }],
        evidence: vec![PanelEvidence {
            id: "e1".into(),
            kind: lingxi_core::host::EvidenceKind::File,
            locator: "src/lib.rs".into(),
            excerpt: None,
        }],
        assumptions: vec![],
        risks: vec![],
        unresolved_questions: vec![],
    }
}

#[async_trait]
impl SubagentSpawner for EditingPanels {
    async fn spawn(
        &self,
        request: SubagentSpawnRequest,
        _: SubagentInheritance,
    ) -> Result<SubagentResult, SubagentSpawnError> {
        assert_eq!(
            request.subagent_type,
            lingxi_core::host::FUSION_IMPLEMENTER_TYPE
        );
        let model = request.model.clone().unwrap_or_default();
        let cwd = PathBuf::from(
            request
                .cwd
                .clone()
                .expect("an implementer gets its worktree"),
        );
        self.spawns
            .lock()
            .unwrap()
            .push((model.clone(), cwd.clone()));

        // The panel starts from the user's working tree as it was.
        let lib = std::fs::read_to_string(cwd.join("src/lib.rs")).unwrap_or_default();
        self.saw_at_start.lock().unwrap().push((
            model.clone(),
            lib,
            cwd.join("scratch.txt").exists(),
            cwd.join("target/junk.o").exists(),
        ));

        match model.as_str() {
            "m-a" => {
                write(&cwd, "src/lib.rs", b"pub fn retries() -> u32 { 3 }\n");
                write(&cwd, "src/retry.rs", b"pub const CAP: u32 = 3;\n");
            }
            "m-b" => {
                write(&cwd, "src/lib.rs", b"pub fn retries() -> u32 { 4 }\n");
                std::fs::remove_file(cwd.join("old.txt")).unwrap();
                write(&cwd, "data.bin", &[9, 9, 0, 9]);
            }
            // m-c looks and changes nothing.
            _ => {}
        }
        let usage = SubagentUsage {
            total_tokens: 12,
            input_tokens: 8,
            output_tokens: 4,
            ..SubagentUsage::default()
        };
        if self.fail_after_editing == Some(model.as_str()) {
            write(&cwd, "partial.txt", b"half a change\n");
            return Ok(SubagentResult::Failed {
                agent_id: AgentId::new(),
                reason: "provider error after billed turns".into(),
                usage: SubagentUsage::default(),
            });
        }
        Ok(SubagentResult::Completed {
            agent_id: AgentId::new(),
            content: serde_json::to_value(panel_report(&format!("done by {model}"))).unwrap(),
            usage: usage.clone(),
            total_tool_use_count: 3,
            total_duration_ms: 1,
            total_tokens: 12,
            assistant_message_count: 2,
            response_char_count: 1,
            last_request_id: None,
            cumulative_usage: usage,
            usage_complete: true,
        })
    }
}

/// The analyst: scores whichever panels its payload names, and remembers the
/// payload so the test can read what the analyst was shown.
struct CannedAnalyst {
    payloads: Mutex<Vec<String>>,
}

#[async_trait]
impl SideQueryClient for CannedAnalyst {
    async fn query(&self, _: SideQueryRequest) -> Result<SideQueryResponse, SideQueryError> {
        panic!("implement runs have no synthesizer call")
    }

    async fn query_json_schema(
        &self,
        request: StrictStructuredQueryRequest,
    ) -> Result<StrictStructuredQueryResponse, SideQueryError> {
        let user = match request.messages.first() {
            Some(lingxi_core::types::ConversationMessage::User { content, .. }) => content
                .iter()
                .find_map(|block| match block {
                    lingxi_core::types::ContentBlock::Text { text, .. } => Some(text.clone()),
                    _ => None,
                })
                .unwrap_or_default(),
            _ => String::new(),
        };
        self.payloads.lock().unwrap().push(user.clone());
        let payload: Value = serde_json::from_str(&user).unwrap_or(Value::Null);
        let mut scores = serde_json::Map::new();
        for panel in payload["panels"].as_array().cloned().unwrap_or_default() {
            let id = panel["panel_id"].as_str().unwrap_or_default().to_string();
            let row: serde_json::Map<String, Value> = DEFAULT_IMPLEMENT_FUSION_DIMENSIONS
                .iter()
                .map(|dim| ((*dim).to_string(), json!(70)))
                .collect();
            scores.insert(id, Value::Object(row));
        }
        Ok(StrictStructuredQueryResponse {
            value: json!({
                "consensus": [], "contradictions": [], "partial_coverage": [],
                "unique_insights": [], "blind_spots": [], "scores": scores,
            }),
            usage: cost::Usage::default(),
            model: request.model,
            profile: request.profile,
            request_id: None,
            retry_count: 0,
        })
    }
}

/// The production worktree manager with a verification that runs `sh -c`.
struct GitHost {
    worktrees: Arc<PosixWorktreeManager>,
}

#[async_trait]
impl FusionImplementHost for GitHost {
    fn worktrees(&self) -> Arc<dyn WorktreeManager> {
        self.worktrees.clone()
    }
    async fn preflight(&self, _: u64) -> Result<(), String> {
        Ok(())
    }
    async fn verify(
        &self,
        worktree: &Path,
        command: &str,
        timeout: Duration,
        _cancel: CancellationToken,
    ) -> VerificationRun {
        let started = std::time::Instant::now();
        let run = tokio::process::Command::new("sh")
            .args(["-c", command])
            .current_dir(worktree)
            .output();
        let outcome = match tokio::time::timeout(timeout, run).await {
            Err(_) => VerificationOutcome::TimedOut,
            Ok(Err(error)) => VerificationOutcome::Error {
                message: error.to_string(),
            },
            Ok(Ok(out)) if out.status.success() => VerificationOutcome::Passed,
            Ok(Ok(out)) => VerificationOutcome::Failed {
                exit_code: out.status.code(),
            },
        };
        VerificationRun {
            command: command.to_string(),
            outcome,
            duration_ms: u64::try_from(started.elapsed().as_millis()).unwrap_or(u64::MAX),
            output_tail: String::new(),
        }
    }
}

fn catalog() -> Vec<CatalogModel> {
    MODELS
        .iter()
        .chain(["m-judge"].iter())
        .map(|model| CatalogModel {
            profile: "p".into(),
            model: (*model).into(),
            hints: FusionModelHints {
                eligible: true,
                quality_rank: 90,
                judge_eligible: true,
                ..FusionModelHints::default()
            },
            structured_output: true,
            limits: ModelLimits {
                context_window_tokens: Some(200_000),
                max_input_tokens: Some(180_000),
                max_output_tokens: Some(32_000),
            },
        })
        .collect()
}

fn config() -> FusionRuntimeConfig {
    let mut cfg = FusionRuntimeConfig::defaults();
    cfg.panel_models = MODELS
        .iter()
        .map(|model| lingxi_core::host::FusionModelChoice::new("p", *model))
        .collect();
    cfg.analyst_model = Some(lingxi_core::host::FusionModelChoice::new("p", "m-judge"));
    cfg.min_successful_panels = 2;
    cfg.implement.verify_commands = vec![
        // Only m-a's worktree has this file.
        "test -f src/retry.rs".into(),
        // m-a wrote 3, m-b wrote 4, m-c left the user's 2.
        "grep -q '{ 3 }' src/lib.rs".into(),
    ];
    cfg
}

fn request() -> FusionRequest {
    FusionRequest {
        verify_claims: false,
        schema_version: lingxi_core::host::FUSION_SCHEMA_VERSION,
        origin: FusionOrigin::Slash,
        prompt: "raise the retry cap to 3".into(),
        preset: FusionPreset::Quality,
        models: Some(
            MODELS
                .iter()
                .map(|model| FusionModelRef {
                    profile: Some("p".into()),
                    model: (*model).into(),
                })
                .collect(),
        ),
        dimensions: DEFAULT_IMPLEMENT_FUSION_DIMENSIONS
            .iter()
            .map(|dim| (*dim).to_string())
            .collect(),
        partial_ok: true,
        max_panel: None,
        cross_provider: true,
        parent_profile: "p".into(),
        parent_model: "m-parent".into(),
        mode: FusionPanelMode::Implement,
        verify_commands: Vec::new(),
    }
}

fn entries_under_worktrees(repo: &Path) -> Vec<String> {
    let base = repo.join(".lingxi/worktrees");
    let mut names: Vec<String> = std::fs::read_dir(base)
        .map(|entries| {
            entries
                .filter_map(Result::ok)
                .map(|entry| entry.file_name().to_string_lossy().into_owned())
                .collect()
        })
        .unwrap_or_default();
    names.sort();
    names
}

/// Worktree directories only, without the `.patch` files beside them.
fn worktree_dirs(repo: &Path) -> Vec<String> {
    let mut dirs = entries_under_worktrees(repo);
    dirs.retain(|name| !name.ends_with(".patch"));
    dirs
}

async fn run_fusion(
    repo: &Path,
) -> (
    lingxi_core::host::FusionResult,
    Arc<EditingPanels>,
    Arc<CannedAnalyst>,
) {
    run_fusion_with(repo, None).await
}

async fn run_fusion_with(
    repo: &Path,
    fail_after_editing: Option<&'static str>,
) -> (
    lingxi_core::host::FusionResult,
    Arc<EditingPanels>,
    Arc<CannedAnalyst>,
) {
    let panels = Arc::new(EditingPanels {
        fail_after_editing,
        spawns: Mutex::new(Vec::new()),
        saw_at_start: Mutex::new(Vec::new()),
    });
    let analyst = Arc::new(CannedAnalyst {
        payloads: Mutex::new(Vec::new()),
    });
    let host = Arc::new(GitHost {
        worktrees: Arc::new(PosixWorktreeManager::new(repo.to_path_buf())),
    });
    let orchestrator = FusionOrchestrator::new(
        panels.clone(),
        analyst.clone(),
        Arc::new(config()),
        Arc::new(catalog()),
    )
    .with_implement_host(host);
    let inherit = FusionInheritance::new(
        SubagentInheritance {
            tool_invoker: Arc::new(InertInvoker),
            budget: Arc::new(InertBudget),
        },
        CancellationToken::new(),
    );

    // The same entry a host uses: prepare, then activate.
    let identity = lingxi_core::host::FusionRunIdentity::new(
        lingxi_core::host::FusionRunId::generated(),
        None,
        FusionOrigin::Slash,
        None,
    );
    let submission =
        lingxi_core::host::FusionSubmission::new(request(), inherit, identity).unwrap();
    let result = Arc::new(orchestrator)
        .prepare(submission)
        .expect("the run prepares")
        .activate(lingxi_core::host::FusionActivation::now(), None)
        .await
        .result
        .expect("the run completes");

    (result, panels, analyst)
}

#[tokio::test]
async fn implement_run_against_a_real_repo_snapshots_the_users_work_and_leaves_it_alone() {
    let dir = tempfile::tempdir().unwrap();
    // Canonicalize: macOS temp dirs sit behind a /var -> /private/var symlink.
    let repo = dir.path().canonicalize().unwrap();
    user_repo(&repo);
    let status_before = git(&repo, &["status", "--porcelain"]);
    let head_before = git(&repo, &["rev-parse", "HEAD"]);
    let branches_before = git(
        &repo,
        &["for-each-ref", "--format=%(refname:short)", "refs/heads"],
    );
    let stash_before = git(&repo, &["stash", "list"]);

    let (result, panels, analyst) = run_fusion(&repo).await;

    // ── The run itself ────────────────────────────────────────────────────
    assert_eq!(result.mode, FusionPanelMode::Implement);
    assert_eq!(result.status, FusionStatus::Analyzed);
    assert_eq!(result.responses.len(), 3);

    // Each panel started from the user's working tree, uncommitted work
    // included, and without ignored build output.
    let saw = panels.saw_at_start.lock().unwrap().clone();
    assert_eq!(saw.len(), 3);
    for (model, lib, has_scratch, has_ignored) in &saw {
        assert_eq!(
            lib, "pub fn retries() -> u32 { 2 }\n",
            "{model} starts from the edit"
        );
        assert!(has_scratch, "{model} sees the user's untracked file");
        assert!(
            !has_ignored,
            "{model} must not receive ignored build output"
        );
    }
    // Every panel worked in its own directory, and none of them was the repo.
    let spawns = panels.spawns.lock().unwrap().clone();
    let mut dirs: Vec<&PathBuf> = spawns.iter().map(|(_, cwd)| cwd).collect();
    dirs.sort();
    dirs.dedup();
    assert_eq!(dirs.len(), 3);
    assert!(dirs
        .iter()
        .all(|cwd| *cwd != &repo && cwd.starts_with(&repo)));

    // ── What each panel's patch says ──────────────────────────────────────
    // Panel ids are anonymous; tell the panels apart by what they changed.
    let by_change = |paths: &[&str]| {
        result
            .responses
            .iter()
            .find(|panel| {
                panel.patch.as_ref().is_some_and(|patch| {
                    let mut got: Vec<&str> = patch.files.iter().map(|f| f.path.as_str()).collect();
                    got.sort_unstable();
                    got == paths
                })
            })
            .unwrap_or_else(|| panic!("no panel changed exactly {paths:?}"))
    };
    let a = by_change(&["src/lib.rs", "src/retry.rs"]);
    let b = by_change(&["data.bin", "old.txt", "src/lib.rs"]);
    let c = result
        .responses
        .iter()
        .find(|panel| {
            panel
                .patch
                .as_ref()
                .is_some_and(|patch| patch.files.is_empty())
        })
        .expect("the panel that changed nothing still reports an empty patch");
    let patch_c = c.patch.as_ref().unwrap();
    assert!(patch_c.diff.is_empty() && patch_c.patch_file.is_none());
    assert_eq!((patch_c.insertions, patch_c.deletions), (0, 0));

    let patch_a = a.patch.as_ref().unwrap();
    // The patch is against the snapshot, so the user's `{ 2 }` is context and
    // only the panel's own line change appears.
    assert!(
        patch_a.diff.contains("-pub fn retries() -> u32 { 2 }"),
        "{}",
        patch_a.diff
    );
    assert!(patch_a.diff.contains("+pub fn retries() -> u32 { 3 }"));
    assert!(
        !patch_a.diff.contains("{ 1 }"),
        "the committed version is not in the diff"
    );
    assert!(
        !patch_a.diff.contains("scratch"),
        "the user's untracked file is not the panel's change"
    );
    assert_eq!(patch_a.insertions, 2);
    assert_eq!(patch_a.deletions, 1);

    let patch_b = b.patch.as_ref().unwrap();
    let status_of = |path: &str| {
        patch_b
            .files
            .iter()
            .find(|f| f.path == path)
            .map(|f| (f.status.clone(), f.binary))
            .unwrap()
    };
    assert_eq!(
        status_of("old.txt").0,
        lingxi_core::host::PatchFileStatus::Deleted
    );
    assert!(
        status_of("data.bin").1,
        "the binary change is flagged binary"
    );
    assert_eq!(
        status_of("src/lib.rs").0,
        lingxi_core::host::PatchFileStatus::Modified
    );
    assert_ne!(
        patch_a.base_commit, "",
        "patches name the snapshot they are against"
    );
    assert_eq!(patch_a.base_commit, patch_b.base_commit);
    assert_ne!(
        patch_a.base_commit,
        head_before.trim(),
        "the base is a snapshot commit, not the user's HEAD"
    );

    // ── Verification ran per worktree ─────────────────────────────────────
    let outcomes = |panel: &lingxi_core::host::PanelMaterial| match panel.verification.as_ref() {
        Some(PanelVerification::Runs(runs)) => runs
            .iter()
            .map(|run| run.outcome.label())
            .collect::<Vec<_>>(),
        other => panic!("expected runs, got {other:?}"),
    };
    assert_eq!(outcomes(a), ["passed", "passed"]);
    assert_eq!(outcomes(b), ["failed", "failed"]);
    // Verification is spent only on panels that changed something.
    assert!(c.verification.is_none(), "{:?}", c.verification);

    // ── What the analyst was shown ────────────────────────────────────────
    let payloads = analyst.payloads.lock().unwrap().clone();
    assert_eq!(payloads.len(), 1);
    assert!(
        payloads[0].contains("src/retry.rs"),
        "the analyst sees the changed files"
    );
    assert!(
        payloads[0].contains("\"failed\""),
        "and how verification came out"
    );

    // ── The patches apply to the snapshot, and only to it ─────────────────
    // Rebuild the user's tree as a fresh checkout at the snapshot commit, then
    // apply each panel's saved patch file with git itself.
    for panel in [a, b] {
        let patch = panel.patch.as_ref().unwrap();
        let file = patch
            .patch_file
            .as_ref()
            .expect("a panel with changes has a patch file");
        assert!(Path::new(file).exists());
        let check = tempfile::tempdir().unwrap();
        let check_dir = check.path().canonicalize().unwrap();
        git(&repo, &["worktree", "prune"]);
        let clone = std::process::Command::new("git")
            .args(["clone", "-q", "--no-checkout"])
            .arg(&repo)
            .arg(&check_dir)
            .output()
            .unwrap();
        assert!(
            clone.status.success(),
            "{}",
            String::from_utf8_lossy(&clone.stderr)
        );
        // The snapshot commit is not on any branch of the clone; fetch it from
        // the worktree's own object database instead.
        git(
            &check_dir,
            &["fetch", "-q", patch.worktree.as_str(), &patch.base_commit],
        );
        git(
            &check_dir,
            &["checkout", "-q", "--detach", &patch.base_commit],
        );
        assert_eq!(
            std::fs::read_to_string(check_dir.join("src/lib.rs")).unwrap(),
            "pub fn retries() -> u32 { 2 }\n",
            "the snapshot holds the user's uncommitted edit"
        );
        git(&check_dir, &["apply", "--check", "--binary", file.as_str()]);
        git(&check_dir, &["apply", "--binary", file.as_str()]);
    }

    // ── The user's tree is exactly as it was ──────────────────────────────
    // The kept worktrees live under `.lingxi/worktrees`, which git shows as
    // untracked (the same layout `EnterWorktree` has always had). Nothing else
    // about the user's status may change.
    assert_eq!(
        git(&repo, &["status", "--porcelain"]).replace("?? .lingxi/\n", ""),
        status_before
    );
    assert_eq!(git(&repo, &["rev-parse", "HEAD"]), head_before);
    assert_eq!(git(&repo, &["stash", "list"]), stash_before);
    assert_eq!(
        std::fs::read_to_string(repo.join("src/lib.rs")).unwrap(),
        "pub fn retries() -> u32 { 2 }\n"
    );
    assert!(repo.join("old.txt").exists());
    assert!(!repo.join("src/retry.rs").exists());
    // Only the worktree branches of panels that changed something remain.
    let branches_after = git(
        &repo,
        &["for-each-ref", "--format=%(refname:short)", "refs/heads"],
    );
    let new_branches: Vec<&str> = branches_after
        .lines()
        .filter(|line| !branches_before.lines().any(|before| before == *line))
        .map(str::trim)
        .collect();
    assert_eq!(new_branches.len(), 2, "kept branches: {new_branches:?}");
    assert!(new_branches
        .iter()
        .all(|b| b.starts_with("worktree-fusion-")));

    // ── Kept worktrees are exactly the ones with changes ──────────────────
    let kept = worktree_dirs(&repo);
    assert_eq!(
        kept.len(),
        2,
        "the unchanged panel's worktree is gone: {kept:?}"
    );
    let patch_files = entries_under_worktrees(&repo)
        .into_iter()
        .filter(|name| name.ends_with(".patch"))
        .count();
    assert_eq!(patch_files, 2, "one saved patch beside each kept worktree");
    assert!(
        kept.iter().all(|name| name.starts_with("fusion-")),
        "{kept:?}"
    );
    for panel in [a, b] {
        assert!(Path::new(&panel.patch.as_ref().unwrap().worktree).is_dir());
    }

    // ── A second run while the first run's worktrees are still there ──────
    // Its snapshot must not swallow the first run's worktrees (each a nested
    // checkout), or panels would start with copies of them.
    let (second, second_panels, _) = run_fusion(&repo).await;
    assert_eq!(second.status, FusionStatus::Analyzed);
    for (model, lib, has_scratch, _) in second_panels.saw_at_start.lock().unwrap().iter() {
        assert_eq!(
            lib, "pub fn retries() -> u32 { 2 }\n",
            "{model} again starts from the user's edit"
        );
        assert!(has_scratch);
    }
    for (model, cwd) in second_panels.spawns.lock().unwrap().iter() {
        assert!(
            !cwd.join(".lingxi").exists(),
            "{model}'s worktree holds a copy of the managed worktrees"
        );
    }
    let second_base = &second.responses[0].patch.as_ref().unwrap().base_commit;
    let tree = git(&repo, &["ls-tree", "-r", "--name-only", second_base]);
    assert!(!tree.contains(".lingxi"), "snapshot tree:\n{tree}");
    assert!(
        tree.contains("scratch.txt") && !tree.contains("junk.o"),
        "{tree}"
    );
    assert_eq!(worktree_dirs(&repo).len(), 4, "two kept per run");

    // ── /fusion clean removes them and their branches ─────────────────────
    let host = GitHost {
        worktrees: Arc::new(PosixWorktreeManager::new(repo.clone())),
    };
    let removed = fusion::clean_worktrees(&host).await;
    assert_eq!(removed, 4);
    assert!(worktree_dirs(&repo).is_empty());
    assert_eq!(
        entries_under_worktrees(&repo),
        Vec::<String>::new(),
        "saved patches go too"
    );
    assert_eq!(
        git(
            &repo,
            &["for-each-ref", "--format=%(refname:short)", "refs/heads"]
        ),
        branches_before
    );
    assert_eq!(git(&repo, &["status", "--porcelain"]), status_before);
}

#[tokio::test]
async fn a_panel_that_fails_midway_still_hands_over_what_it_changed() {
    let dir = tempfile::tempdir().unwrap();
    let repo = dir.path().canonicalize().unwrap();
    user_repo(&repo);

    let (result, _, _) = run_fusion_with(&repo, Some("m-c")).await;

    assert_eq!(result.status, FusionStatus::Analyzed);
    let failed: Vec<_> = result
        .panels
        .iter()
        .filter(|panel| panel.status != lingxi_core::host::PanelRunStatus::Completed)
        .collect();
    assert_eq!(failed.len(), 1, "{:?}", result.panels);
    // Its half change is in the material, marked incomplete, and was verified
    // like any other change.
    let incomplete: Vec<_> = result.responses.iter().filter(|p| p.incomplete).collect();
    assert_eq!(incomplete.len(), 1);
    let patch = incomplete[0]
        .patch
        .as_ref()
        .expect("the partial patch is kept");
    assert_eq!(
        patch
            .files
            .iter()
            .map(|f| f.path.as_str())
            .collect::<Vec<_>>(),
        ["partial.txt"]
    );
    assert!(patch.diff.contains("+half a change"));
    assert!(matches!(
        incomplete[0].verification,
        Some(PanelVerification::Runs(_))
    ));
    // The change sits in a kept worktree the parent can read, never in the
    // user's tree.
    assert!(Path::new(&patch.worktree).join("partial.txt").exists());
    assert!(!repo.join("partial.txt").exists());
    assert_eq!(
        worktree_dirs(&repo).len(),
        3,
        "all three panels changed something"
    );
}
