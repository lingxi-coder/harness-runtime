//! Multi-process regression for the task store's cross-process lock
//! (parity 2.1.207 P1-13).
//!
//! claude-code serialises concurrent processes on one task list with an on-disk
//! `proper-lockfile`. LingXi's earlier in-process-only mutex gave two OS
//! processes (a `--bg` worker + an `--resume` attach, or an env-shared list id)
//! no protection, so `create` produced duplicate ids and `update` lost writes.
//! This test re-execs the test binary as N worker processes that hammer one
//! shared tasks dir and asserts the store stays race-free.
//!
//! The test double-dispatches on `LINGXI_TODO_MP_ROLE`: the parent run spawns
//! children with that var set; a child performs its worker role and
//! `process::exit`s before the harness proceeds.

use std::path::{Path, PathBuf};
use std::process::Command;

use serde_json::{json, Map};
use task_store::todo_store::{ClaimOptions, ClaimResult, TodoStore, TodoTask};

const ROLE_ENV: &str = "LINGXI_TODO_MP_ROLE";
const DIR_ENV: &str = "LINGXI_TODO_MP_DIR";
const CLAIM_ID_ENV: &str = "LINGXI_TODO_MP_CLAIM_ID";
/// Worker processes.
const N: usize = 4;
/// Tasks each process creates (create phase) / increments (update phase).
const PER_PROC: usize = 25;
/// The single shared task every update-worker races on.
const SHARED_ID: &str = "1";

#[test]
fn multiprocess_create_and_update_is_race_free() {
    // ── child mode ──────────────────────────────────────────────────────────
    if let Ok(role) = std::env::var(ROLE_ENV) {
        let dir = PathBuf::from(std::env::var(DIR_ENV).expect("child needs dir"));
        let rt = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .expect("child runtime");
        let code = rt.block_on(async move {
            match role.as_str() {
                "create" => child_create(&dir).await,
                "update" => child_update(&dir).await,
                "claim" => child_claim(&dir).await,
                other => {
                    eprintln!("unknown role {other}");
                    1
                }
            }
        });
        std::process::exit(code);
    }

    // ── parent mode ─────────────────────────────────────────────────────────
    let dir = unique_dir();
    std::fs::create_dir_all(&dir).expect("mk shared dir");

    // Phase 1: N processes concurrently create PER_PROC tasks each.
    run_workers("create", &dir);

    let total = N * PER_PROC;
    let mut ids: Vec<i64> = Vec::new();
    for entry in std::fs::read_dir(&dir).expect("read dir").flatten() {
        let name = entry.file_name().to_string_lossy().into_owned();
        if let Some(stem) = name.strip_suffix(".json") {
            // Every task file must be complete, parseable JSON (no torn writes).
            let content = std::fs::read_to_string(entry.path()).expect("read task");
            let _: TodoTask = serde_json::from_str(&content)
                .unwrap_or_else(|e| panic!("task {name} is not valid JSON: {e}\n{content}"));
            ids.push(stem.parse::<i64>().expect("numeric id"));
        }
    }
    ids.sort_unstable();
    // Unique, gap-free ids 1..=total prove the list lock prevented duplicates.
    assert_eq!(
        ids.len(),
        total,
        "expected {total} distinct task files, got {}: {ids:?}",
        ids.len()
    );
    assert_eq!(
        ids,
        (1..=total as i64).collect::<Vec<_>>(),
        "ids must be the unique sequence 1..={total} (duplicates collapse files)"
    );

    // Phase 2: N processes each apply PER_PROC read-modify-write increments to
    // the SAME task. With the per-task file lock every increment lands.
    run_workers("update", &dir);

    let shared = read_task(&dir, SHARED_ID);
    let counter = shared
        .metadata
        .get("counter")
        .and_then(serde_json::Value::as_i64)
        .unwrap_or(0);
    assert_eq!(
        counter, total as i64,
        "lost update: {N} procs * {PER_PROC} increments should total {total}, got {counter}"
    );

    // Phase 3: N processes race ONE `claim_task` each on a fresh pending task
    // (oracle QOd). The per-task file lock makes check+write atomic, so exactly
    // one claimer may win; the rest must observe `already_claimed`.
    let rt = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .expect("parent runtime");
    let claim_target = rt.block_on(async {
        let store = TodoStore::in_dir(dir.clone());
        store
            .create(TodoTask::new(
                "contended".into(),
                "d".into(),
                None,
                Map::new(),
            ))
            .await
            .expect("create claim target")
    });
    let exit_codes = run_workers_with_codes("claim", &dir, &claim_target);
    let wins = exit_codes.iter().filter(|&&c| c == 0).count();
    let losses = exit_codes.iter().filter(|&&c| c == 2).count();
    assert_eq!(
        (wins, losses),
        (1, N - 1),
        "exactly one claimer must win; exit codes: {exit_codes:?}"
    );
    let owner = read_task(&dir, &claim_target).owner.expect("owner set");
    assert!(
        owner.starts_with("claimer-"),
        "winning claimer recorded on disk, got {owner}"
    );

    let _ = std::fs::remove_dir_all(&dir);
}

async fn child_create(dir: &Path) -> i32 {
    let store = TodoStore::in_dir(dir.to_path_buf());
    for i in 0..PER_PROC {
        let task = TodoTask::new(
            format!("proc-{}-{i}", std::process::id()),
            "d".into(),
            None,
            Map::new(),
        );
        if store.create(task).await.is_err() {
            return 1;
        }
    }
    0
}

async fn child_update(dir: &Path) -> i32 {
    let store = TodoStore::in_dir(dir.to_path_buf());
    for _ in 0..PER_PROC {
        let updated = store
            .update(SHARED_ID, |t| {
                let c = t
                    .metadata
                    .get("counter")
                    .and_then(serde_json::Value::as_i64)
                    .unwrap_or(0);
                t.metadata.insert("counter".into(), json!(c + 1));
            })
            .await;
        if updated.is_none() {
            return 1;
        }
    }
    0
}

/// One contended claim (phase 3). Exit code: 0 = won the claim, 2 = lost to
/// another claimer (`already_claimed`), 1 = anything else (a bug).
async fn child_claim(dir: &Path) -> i32 {
    let store = TodoStore::in_dir(dir.to_path_buf());
    let target = std::env::var(CLAIM_ID_ENV).expect("child needs claim target id");
    let owner = format!("claimer-{}", std::process::id());
    match store
        .claim_task(&target, &owner, ClaimOptions::default())
        .await
    {
        ClaimResult::Success { .. } => 0,
        ClaimResult::AlreadyClaimed { .. } => 2,
        other => {
            eprintln!("unexpected claim outcome: {other:?}");
            1
        }
    }
}

fn read_task(dir: &Path, id: &str) -> TodoTask {
    let content =
        std::fs::read_to_string(dir.join(format!("{id}.json"))).expect("read shared task");
    serde_json::from_str(&content).expect("shared task JSON")
}

/// Spawn N copies of this test binary as `role` workers against `dir` and wait
/// for each to exit successfully.
fn run_workers(role: &str, dir: &Path) {
    let codes = spawn_workers(role, dir, None);
    for code in codes {
        assert_eq!(code, 0, "{role} worker failed with exit code {code}");
    }
}

/// Like [`run_workers`] but collects raw exit codes (phase 3 uses them to
/// distinguish claim wins from losses) and threads the claim-target id.
fn run_workers_with_codes(role: &str, dir: &Path, claim_id: &str) -> Vec<i32> {
    spawn_workers(role, dir, Some(claim_id))
}

fn spawn_workers(role: &str, dir: &Path, claim_id: Option<&str>) -> Vec<i32> {
    let exe = std::env::current_exe().expect("current_exe");
    let children: Vec<_> = (0..N)
        .map(|_| {
            let mut cmd = Command::new(&exe);
            cmd.arg("multiprocess_create_and_update_is_race_free")
                .arg("--exact")
                .arg("--nocapture")
                .env(ROLE_ENV, role)
                .env(DIR_ENV, dir);
            if let Some(id) = claim_id {
                cmd.env(CLAIM_ID_ENV, id);
            }
            cmd.spawn().expect("spawn worker")
        })
        .collect();
    children
        .into_iter()
        .map(|mut child| {
            let status = child.wait().expect("await worker");
            status.code().unwrap_or(-1)
        })
        .collect()
}

fn unique_dir() -> PathBuf {
    let mut dir = std::env::temp_dir();
    dir.push(format!(
        "lingxi-todo-mp-{}-{}",
        std::process::id(),
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|d| d.as_nanos())
            .unwrap_or(0)
    ));
    dir
}
