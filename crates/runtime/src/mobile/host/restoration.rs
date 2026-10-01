use crate::mobile::local_apps_host::{canonical_cwd_string, LocalAppsHostBroker};
use crate::mobile::local_apps_sessions::remove_app_session_file;
use client::protocol::listings::SessionModeDto;
use std::sync::{Arc, Mutex as StdMutex};

use super::MobileConfig;

/// As [`build_mobile_engine`], but allows a test to substitute the streaming
/// client (plan F3-06 — the off-device walking skeleton). Production callers use
/// [`build_mobile_engine`] (`streaming_override == None`); the host skeleton test
/// passes a scripted
/// [`orchestrator::test_support_stream::MockStreamingApiClient`] so
/// `submit(SendPrompt)` drives a deterministic turn without a network. The whole
/// session-host wiring (the runtime, the recording permission sink, the adapter
/// sinks) is identical to production — only the stream's source differs.
/// LOCAL-APPS (phase 1): the per-profile data root the apps store lives under
/// (`<root>/apps/index.json`, `<root>/apps/<id>/…`).
///
/// The engine's per-profile data dir is the app-files root: every production
/// path sets `lingxi_home = <app_files_root>/<DOT_DIR>` (android-aar
/// `build_android_engine*`; the host `test_config` mirrors it under a temp
/// root), so its parent IS the profile root — deliberately independent of
/// `cwd`, which may point at a per-project workspace while apps are a
/// profile-global capability. A degenerate `lingxi_home` (empty / no parent,
/// only reachable through a hand-rolled `MobileConfig`) falls back to `cwd`,
/// which equals the app-files root whenever no project workspace is selected.
/// v3 Phase 4: mint an app's pinned init session (bare uuid) in the app's
/// workspace-scoped catalog. Chat-origin creates (a `conversation_id` bound
/// at create) FORK that conversation out of `source_cwd`'s catalog into the
/// workspace — history follows the user, the source session stays put; a
/// library create (or a fork that fails, e.g. an empty source) anchors an
/// empty mobile session instead. Returns the minted uuid; the caller pins it
pub(crate) async fn mint_app_init_session(
    lingxi_home: &std::path::Path,
    source_cwd: &str,
    data_root: &std::path::Path,
    fs: Arc<dyn lingxi_core::host::FileSystem>,
    record: &local_apps::AppRecord,
) -> Result<String, String> {
    let workspace_cwd = canonical_cwd_string(&data_root.join(&record.workspace_rel));
    // r1-backlog-engine-create-10: fork from the cwd the app was CREATED from
    // when the record remembers it, and fall back to the caller's own cwd when
    // it does not. `None` means "origin scope unknown" (a record written before
    // the field, or a create with no chat behind it) — never an empty path, so
    // this is the only fallback trigger and it reproduces exactly the previous
    // behaviour. The stored string is a REMEMBERED path, not a validated live
    // directory: it is used only to name a transcript catalog, and a fork
    // against a catalog that no longer exists degrades to an empty anchor
    // through the `Err` arm below rather than failing the create.
    let source_cwd = record.origin_cwd.as_deref().unwrap_or(source_cwd);
    if let Some(source) = record.conversation_id.as_deref() {
        if let Ok(source_uuid) = uuid::Uuid::parse_str(source) {
            match session::branch::create_branch_to_cwd(
                lingxi_home,
                source_cwd,
                &workspace_cwd,
                source_uuid,
                Some(&record.name),
                fs.clone(),
            )
            .await
            {
                Ok(result) => {
                    let session_id = result.new_session_id.to_string();
                    let path = orchestrator::transcript_paths::main_transcript_path(
                        lingxi_home,
                        &workspace_cwd,
                        &session_id,
                    );
                    let writer = session::jsonl::writer::JsonlWriter::new(path, fs.clone());
                    if let Err(error) = writer
                        .append_session_mode(session::jsonl::SessionMode::Code.as_str())
                        .await
                    {
                        // r1-engine-core-002: `create_branch_to_cwd` already
                        // wrote the forked transcript to disk; a half-minted
                        // session must not strand it as an orphan the caller
                        // never learns the id of.
                        remove_app_session_file(lingxi_home, data_root, record, &session_id);
                        return Err(format!("persist app init session mode: {error}"));
                    }
                    return Ok(session_id);
                }
                Err(error) => {
                    // Degrade to an empty anchor — a brand-new conversation
                    // has nothing to fork, and that must not fail the create.
                    // r1-backlog-engine-create-10: `warn!`, not `debug!` — a
                    // record here always claims a `conversation_id`, so this
                    // is never the ordinary no-source-to-fork case; it is
                    // either a genuinely vanished source session, or a fork
                    // against the wrong catalog — which now happens only when
                    // the record remembered NO `origin_cwd` and the caller's
                    // own cwd (for the boot sweep, the connection's) had to
                    // stand in for it. A silent degrade to an empty anchor
                    // here has swallowed the user's real transcript before; it
                    // must be visible by default.
                    tracing::warn!(
                        app_id = %record.id,
                        %error,
                        "init-session fork degraded to an empty anchor"
                    );
                }
            }
        }
    }
    let init_id = uuid::Uuid::new_v4().to_string();
    let path =
        orchestrator::transcript_paths::main_transcript_path(lingxi_home, &workspace_cwd, &init_id);
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent)
            .map_err(|error| format!("create app session catalog dir: {error}"))?;
    }
    let writer = session::jsonl::writer::JsonlWriter::new(path, fs);
    if let Err(error) = writer
        .append_mobile_empty_session(&init_id, &record.name)
        .await
    {
        // r1-engine-core-002: best-effort — the file may not exist yet if
        // this failed before any bytes landed — but if the writer got far
        // enough to create it, a half-completed anchor must not strand it.
        remove_app_session_file(lingxi_home, data_root, record, &init_id);
        return Err(format!("anchor app init session: {error}"));
    }
    if let Err(error) = writer
        .append_session_mode(session::jsonl::SessionMode::Code.as_str())
        .await
    {
        remove_app_session_file(lingxi_home, data_root, record, &init_id);
        return Err(format!("persist app init session mode: {error}"));
    }
    Ok(init_id)
}

/// r1-backlog-engine-create-11: `run_app_boot_backfill_sweep`'s own doc says
/// "once per launch", but its only caller sits inside
/// `build_mobile_engine_inner`, which re-runs on every scope switch /
/// reconnect within one process, not just at process start. Keyed by the
/// apps data root (not a single flag) because more than one profile/scope
/// can share a process. Returns `true` the first time a given root is seen
/// in this process, `false` on every later call for the same root — which is
/// exactly what "once per launch" means for a process that never restarts
/// between reconnects.
pub(super) fn boot_backfill_sweep_should_run(data_root: &std::path::Path) -> bool {
    static STARTED: std::sync::OnceLock<StdMutex<std::collections::HashSet<std::path::PathBuf>>> =
        std::sync::OnceLock::new();
    let started = STARTED.get_or_init(|| StdMutex::new(std::collections::HashSet::new()));
    let mut started = started
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner);
    started.insert(data_root.to_path_buf())
}

/// r1-engine-core-013: when the boot sweep's promote-rename fails, the
/// candidate that HOLDS the pinned init session (`base`) must be merged
/// FIRST, not last. The merge loop right after this call is first-writer-wins
/// (`if target.exists() { continue; }`), so whichever directory lands in
/// `drifted` earliest is the one whose files survive a name collision.
/// `base` is the one carrying the forked user transcript the promotion
/// exists to protect — pushing it to the back handed that protection to
/// whichever unrelated drifted directory happened to read-dir first instead.
pub(super) fn requeue_failed_catalog_promotion(
    base: std::path::PathBuf,
    drifted: &mut Vec<std::path::PathBuf>,
) {
    drifted.insert(0, base);
}

/// The boot backfill sweep, as a named function so it has a test.
///
/// Walks every app record once per launch. Per record, in this order:
/// 0. for an unscaffolded shell, repair `workspace/LINGXI.md` if it is
///    missing, empty, or missing the guided-contract header;
/// 1. migrate/merge a session catalog stranded under a drifted directory name;
/// 2. re-anchor a pinned init session whose transcript file is gone;
/// 3. reconcile a pinned init session still carrying the shell placeholder
///    title (the retry behind `LocalAppScaffold`'s immediate rename);
/// 4. for a record with NO pin at all, mint one and set it (set-once, so a
///    concurrent `CreateApp` arbitrates and the loser drops its file).
///
/// Every step is best-effort per app — a failure is logged and re-attempted on
/// the next launch, which is what makes each of them genuinely retryable
/// rather than merely described as such.
///
/// Steps 0-3 run for EVERY record; step 4 is the only one gated on the pin
/// being absent, and step 3 deliberately runs before that gate because every
/// record it can help already has a pin.
///
/// Step 0 matters because `workspace/LINGXI.md` is the ONE channel that
/// reaches the model on every turn for a shell app (auto-loaded by the memory
/// hierarchy): every other repair in this sweep is session bookkeeping, but a
/// lost or truncated guided contract leaves the create interview with nothing
/// to read at all, and nothing else in the create transaction ever revisits
/// it after the initial write.
pub(crate) async fn run_app_boot_backfill_sweep(
    backfill_home: std::path::PathBuf,
    backfill_cwd: String,
    backfill_root: std::path::PathBuf,
    backfill_fs: Arc<dyn lingxi_core::host::FileSystem>,
    backfill_service: Arc<local_apps::AppService>,
    backfill_host: Arc<LocalAppsHostBroker>,
) {
    for record in backfill_service.records().await {
        // Step 0: an unscaffolded shell's ONLY channel to the model is
        // `workspace/LINGXI.md`. If it is gone, empty, or missing the guided
        // header, the create interview has nothing to read and the agent
        // sees an ordinary empty directory. Rewriting it is idempotent and
        // safe to retry every launch. The header literal below is the first
        // line of `guided_workspace_contract` in `local_apps_host.rs`; the two
        // must stay in lockstep, or this check calls a healthy contract
        // malformed and rewrites it on every boot.
        if !record.scaffolded {
            let workspace = backfill_root.join(&record.workspace_rel);
            let lingxi_md = workspace.join("LINGXI.md");
            let needs_repair = match std::fs::read_to_string(&lingxi_md) {
                Ok(contents) => !contents.contains("# Local App (new, not yet shaped)"),
                Err(_) => true,
            };
            if needs_repair {
                // `write_guided_contract_value` writes with `std::fs::write`,
                // which does NOT create parents — its own doc comment states
                // it "runs inside the create transaction, after
                // `layout.initialize()` (so the workspace directory exists)".
                // This sweep has no such guarantee: the very failure it
                // repairs can have taken the directory along with the file,
                // and `write` would then fail with NotFound on every launch
                // forever. Best-effort, like the write itself.
                let _ = std::fs::create_dir_all(&workspace);
                match backfill_host.write_guided_contract_value(&record).await {
                    Ok(()) => tracing::info!(
                        app_id = %record.id,
                        "boot sweep repaired a missing or malformed guided workspace contract"
                    ),
                    Err(error) => tracing::warn!(
                        app_id = %record.id,
                        %error,
                        "boot sweep guided workspace contract repair failed"
                    ),
                }
            }
        }
        // Self-heal the app's catalog location FIRST. Two
        // real-world drifts strand it: (a) an app reinstall
        // changes the iOS data-container UUID, so the old
        // absolute-path key never matches again; (b) the
        // `/var` vs `/private/var` symlink split minted the
        // catalog under one spelling while resume looked
        // under the other. Expected dir = today's CANONICAL
        // spelling; any older dir whose name ends with this
        // app's workspace suffix is renamed onto it.
        {
            let workspace_cwd = canonical_cwd_string(&backfill_root.join(&record.workspace_rel));
            let projects = backfill_home.join("projects");
            let expected = projects.join(session::jsonl::path::project_dir_name(&workspace_cwd));
            let suffix = format!("-apps-{}-workspace", record.id);
            // There can be MORE than one drifted directory —
            // the two documented drifts compound (an old
            // container UUID AND the pre-canonical `/var`
            // spelling). Collect them all: migrating only the
            // first `read_dir` yields would orphan the rest
            // permanently, because the rename makes
            // `expected` exist and this block never runs
            // again.
            let mut drifted: Vec<std::path::PathBuf> = match std::fs::read_dir(&projects) {
                Ok(entries) => entries
                    .flatten()
                    .filter(|entry| {
                        entry.file_name().to_string_lossy().ends_with(&suffix)
                            && entry.path() != expected
                            && entry.path().is_dir()
                    })
                    .map(|entry| entry.path())
                    .collect(),
                Err(_) => Vec::new(),
            };
            let init_file_name = record
                .init_session_id
                .as_deref()
                .map(|id| format!("{id}.jsonl"));
            if !drifted.is_empty() && !expected.exists() {
                // Promote the candidate that actually HOLDS
                // the pinned init session: a chat-origin app
                // forked its whole transcript there, and an
                // arbitrary `read_dir` winner would bury it.
                let base_index = init_file_name
                    .as_deref()
                    .and_then(|file| drifted.iter().position(|dir| dir.join(file).exists()))
                    .unwrap_or(0);
                let base = drifted.remove(base_index);
                match std::fs::rename(&base, &expected) {
                    Ok(()) => tracing::info!(
                        app_id = %record.id,
                        from = %base.display(),
                        "migrated drifted app session catalog"
                    ),
                    Err(error) => {
                        tracing::warn!(
                            app_id = %record.id,
                            error = %error,
                            "app catalog migration rename failed; merging instead"
                        );
                        // r1-engine-core-013: requeue at the FRONT, not the
                        // back — see `requeue_failed_catalog_promotion`.
                        requeue_failed_catalog_promotion(base, &mut drifted);
                    }
                }
            }
            // Fold every remaining drifted catalog into the
            // expected one. Moves are per-file and NEVER
            // overwrite, so a name collision leaves both
            // copies on disk instead of destroying one.
            for dir in drifted {
                if std::fs::create_dir_all(&expected).is_err() {
                    break;
                }
                let Ok(entries) = std::fs::read_dir(&dir) else {
                    continue;
                };
                for entry in entries.flatten() {
                    let target = expected.join(entry.file_name());
                    if target.exists() {
                        continue;
                    }
                    if let Err(error) = std::fs::rename(entry.path(), &target) {
                        tracing::warn!(
                            app_id = %record.id,
                            error = %error,
                            "app catalog merge failed for one session"
                        );
                    }
                }
                // Only removes it when the merge emptied it.
                let _ = std::fs::remove_dir(&dir);
                tracing::info!(
                    app_id = %record.id,
                    from = %dir.display(),
                    "merged drifted app session catalog"
                );
            }
            // A pinned init session whose file is STILL
            // missing after migration (deleted container,
            // partial restore) gets re-anchored in place so
            // resume always has a target. This runs LAST, and
            // only on genuine absence: re-anchoring over a
            // catalog that still had the real transcript
            // would replace the user's history with an empty
            // session AND make the migration above
            // unreachable forever.
            if let Some(init_id) = record.init_session_id.as_deref() {
                let expected_file = expected.join(format!("{init_id}.jsonl"));
                if !expected_file.exists() {
                    if let Err(error) = std::fs::create_dir_all(&expected) {
                        tracing::warn!(
                            app_id = %record.id,
                            error = %error,
                            "app catalog dir create failed"
                        );
                    } else {
                        let writer = session::jsonl::writer::JsonlWriter::new(
                            expected_file,
                            backfill_fs.clone(),
                        );
                        if let Err(error) = writer
                            .append_mobile_empty_session(init_id, &record.name)
                            .await
                        {
                            tracing::warn!(
                                app_id = %record.id,
                                error = %error,
                                "init-session re-anchor failed"
                            );
                        } else if let Err(error) = writer
                            .append_session_mode(session::jsonl::SessionMode::Code.as_str())
                            .await
                        {
                            tracing::warn!(
                                app_id = %record.id,
                                error = %error,
                                "init-session mode re-anchor failed"
                            );
                        } else {
                            tracing::info!(
                                app_id = %record.id,
                                "re-anchored missing init session"
                            );
                        }
                    }
                }
            }
        }
        // Reconcile the pinned init session's TITLE. This is the retry that
        // makes `LocalAppScaffold`'s immediate rename recoverable: that rename
        // runs after the scaffold has already committed and is deliberately
        // not rolled back on failure, so without a trigger here a title left
        // reading `untitled` would stay that way for the life of the app.
        //
        // ⚠️ It runs BEFORE the `init_session_id.is_some()` early-continue
        // below, because every record it can help is one that already HAS a
        // pin — putting it after that `continue` would make it dead code.
        //
        // It shares one predicate with the immediate rename
        // (`reconcile_app_init_session_title`), so neither can decide
        // differently about whether the user renamed the session themselves.
        match crate::mobile::local_apps_sessions::reconcile_app_init_session_title(
            &backfill_home,
            &backfill_root,
            backfill_fs.clone(),
            &record,
        )
        .await
        {
            Ok(true) => tracing::info!(
                app_id = %record.id,
                "boot sweep reconciled a pinned init-session title"
            ),
            Ok(false) => {}
            Err(error) => tracing::warn!(
                app_id = %record.id,
                %error,
                "boot sweep init-session title reconciliation failed"
            ),
        }
        if record.init_session_id.is_some() {
            continue;
        }
        // Re-read before minting: `record` is a snapshot from
        // the list at the top of this sweep, and a CreateApp
        // landing in between commits its record BEFORE it
        // pins. Trusting the snapshot makes both paths mint an
        // anchor for the same app; the pin arbitrates and the
        // loser cleans up, but the app's session list would
        // still show the loser's row until it does.
        let record = match backfill_service.record(&record.id).await {
            Ok(fresh) if fresh.init_session_id.is_none() => fresh,
            _ => continue,
        };
        // r1-failure-paths-012: a pin-less record is not necessarily an
        // empty shell — a create that minted a REAL conversation (the
        // chat-origin fork, or a session the user already had in this
        // workspace) and then failed before `set_init_session` leaves
        // exactly this state. Listing the workspace's own session catalog
        // and adopting the most recent non-empty row there (instead of
        // always minting a fresh empty anchor over it) is what keeps that
        // conversation from being silently orphaned.
        let backfill_workspace_cwd =
            canonical_cwd_string(&backfill_root.join(&record.workspace_rel));
        let existing_conversation: Option<session::jsonl::SessionMetadata> =
            match session::jsonl::list_recent_sessions(
                &backfill_home,
                &backfill_workspace_cwd,
                50,
                backfill_fs.clone(),
            )
            .await
            {
                Ok(rows) => rows.into_iter().find(|row| row.message_count > 0),
                Err(session::jsonl::LoaderError::EmptyDirectory) => None,
                Err(error) => {
                    tracing::warn!(
                        app_id = %record.id,
                        %error,
                        "init-session backfill catalog listing failed"
                    );
                    None
                }
            };
        if let Some(existing) = existing_conversation {
            let session_id = existing.uuid.to_string();
            if let Err(error) = backfill_service
                .set_init_session(&record.id, &session_id)
                .await
            {
                tracing::warn!(
                    app_id = %record.id,
                    %error,
                    session_id = %session_id,
                    "init-session backfill adoption pin failed"
                );
            } else {
                tracing::info!(
                    app_id = %record.id,
                    session_id = %session_id,
                    "boot sweep adopted an existing unpinned conversation instead of minting"
                );
            }
            continue;
        }
        match mint_app_init_session(
            &backfill_home,
            &backfill_cwd,
            &backfill_root,
            backfill_fs.clone(),
            &record,
        )
        .await
        {
            Ok(init_id) => {
                if let Err(error) = backfill_service
                    .set_init_session(&record.id, &init_id)
                    .await
                {
                    // The mint is only half a transaction: an
                    // unpinned session file is unreachable
                    // (nothing references it) and this sweep
                    // would mint ANOTHER one — for a
                    // chat-origin app, a full transcript copy
                    // — on every single boot. Drop the orphan
                    // so the retry stays bounded. The same
                    // cleanup on the CreateApp path settles
                    // the race between the two: whoever loses
                    // `set_init_session` takes its file back.
                    let removed =
                        remove_app_session_file(&backfill_home, &backfill_root, &record, &init_id);
                    tracing::warn!(
                        app_id = %record.id,
                        error = %error,
                        orphan_removed = removed,
                        "init-session backfill pin failed"
                    );
                }
            }
            Err(error) => tracing::warn!(
                app_id = %record.id,
                error = %error,
                "init-session backfill mint failed"
            ),
        }
    }
}

pub(super) fn mobile_apps_data_root(cfg: &MobileConfig) -> std::path::PathBuf {
    cfg.lingxi_home
        .parent()
        .filter(|parent| !parent.as_os_str().is_empty())
        .map_or_else(|| cfg.cwd.clone(), std::path::Path::to_path_buf)
}

pub(super) fn lower_session_mode(mode: session::jsonl::SessionMode) -> SessionModeDto {
    match mode {
        session::jsonl::SessionMode::Chat => SessionModeDto::Chat,
        session::jsonl::SessionMode::Code => SessionModeDto::Code,
    }
}
