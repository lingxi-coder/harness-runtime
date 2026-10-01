//! The session-title side of the Local App conversation seam: what the runtime
//! does to an app's pinned init session when the service reports a scaffold, and
//! what its boot sweep repairs afterwards.
//!
//! The service's half of the seam (it reports the committed record once and
//! survives a host that cannot rename) is tested in `local-app-service`; these
//! tests run the real transcript writer and the real sweep against real files.

use super::*;
use async_trait::async_trait;
use local_app_service::broker::LocalAppsHostBroker;
use local_app_service::host::{HostEvent, HostEventSink};
use local_apps::test_support::FixedClock;
use local_apps::{AppLayout, AppService, NoopAppEventObserver};
use serde_json::json;
use std::fs;
use std::path::PathBuf;
use tempfile::TempDir;

/// For the tests that do not look at what the service reports.
struct DiscardSink;

#[async_trait]
impl HostEventSink for DiscardSink {
    async fn emit(&self, _event: HostEvent) {}
}

/// What a scaffold has persisted by the time the service commits it: a runtime
/// profile, the dependency snapshot that matches it, and a ready dependency
/// record. The service checks exactly these before it will flip `scaffolded`;
/// writing them here stands in for the build the service's own tests run.
fn seed_commit_readiness(root: &std::path::Path, record: &local_apps::AppRecord) {
    let layout = AppLayout::new(root.to_path_buf(), record.id.clone()).expect("layout");
    let binding = local_app_service::runtime_profiles::current_binding_for_family(
        local_apps::AppRuntimeProfile::ReactDom,
    )
    .expect("published react-dom runtime profile");
    let snapshot = local_apps::AppDependencySnapshot {
        requested_sha256: "0".repeat(64),
        package_sha256: "1".repeat(64),
        lockfile_sha256: "2".repeat(64),
        dependency_tree_sha256: "3".repeat(64),
        sbom_sha256: "4".repeat(64),
        toolchain_key: local_app_service::runtime_profiles::RUNTIME_PROFILE_TOOLCHAIN_KEY
            .to_string(),
        verified_profile_contract_sha256: binding.contract_sha256.clone(),
    };
    let mut manifest = local_apps::load_manifest(&layout).expect("manifest");
    manifest.surface = Some(binding.family.surface());
    manifest.template_origin = Some(local_apps::AppTemplateOrigin {
        plugin_id: local_apps::AppTemplateOrigin::BUILTIN_PLUGIN_ID.into(),
        plugin_version: "builtin".into(),
        template_id: format!(
            "{}-r{}",
            binding.family.as_str().replace('_', "-"),
            binding.revision
        ),
        template_sha256: binding.contract_sha256.clone(),
    });
    manifest.runtime_profile = Some(binding);
    manifest.dependency_snapshot = Some(snapshot.clone());
    local_apps::save_manifest(&layout, &manifest).expect("save the manifest");
    local_apps::storage::save_dependency_record(
        root,
        &local_apps::AppDependencyRecord {
            schema_version: local_apps::APPS_SCHEMA_VERSION,
            app_id: record.id.clone(),
            state: local_apps::AppDependencyState::Ready,
            lockfile_sha256: Some(snapshot.lockfile_sha256),
            toolchain_key: Some(snapshot.toolchain_key),
            install_attempts: 1,
            last_error: None,
            updated_at_ms: record.updated_at_ms,
        },
    )
    .expect("save the dependency record");
}

fn workspace_of(root: &std::path::Path, app_id: &str) -> PathBuf {
    let layout = AppLayout::new(root.to_path_buf(), app_id.to_string()).expect("layout");
    root.join(layout.workspace_rel())
}

// ------------------------------------------------------------------
// Task 9: the pinned init session's title.
//
// A shell app's init session is minted while `record.name` is still the
// `untitled` placeholder, and that title lands in a PERSISTED session
// directory. Scaffolding renames it — but only when the user has not
// renamed it first, and the boot sweep must apply the SAME rule.
// ------------------------------------------------------------------

/// A shell app with a pinned init session, plus everything needed to read
/// and rewrite that session's title.
struct PinnedShell {
    root: TempDir,
    service: Arc<AppService>,
    broker: Arc<LocalAppsHostBroker>,
    /// What the service tells when a scaffold commits.
    titles: Arc<dyn ConversationHost>,
    lingxi_home: PathBuf,
    fs: Arc<dyn lingxi_core::host::FileSystem>,
    app_id: String,
    init_session_id: String,
    /// Captured at creation so the transcript path is derived exactly the
    /// way production derives it, from the record's own workspace.
    workspace_rel: String,
}

impl PinnedShell {
    fn transcript(&self) -> PathBuf {
        self.lingxi_home
            .join("projects")
            .join(session::jsonl::path::project_dir_name(
                &canonical_cwd_string(&self.root.path().join(&self.workspace_rel)),
            ))
            .join(format!("{}.jsonl", self.init_session_id))
    }

    /// The title the session catalog would resolve for this session.
    fn title(&self) -> String {
        let transcript = fs::read_to_string(self.transcript()).expect("read the pinned transcript");
        latest_custom_title(&transcript, &self.init_session_id)
            .expect("the pinned session always carries a custom-title")
            .0
    }

    /// The user renaming the session themselves — `/rename`'s channel
    /// (`append_custom_title`), which carries NO `mobileEmptySession`.
    async fn user_rename(&self, title: &str) {
        session::jsonl::writer::JsonlWriter::new(self.transcript(), self.fs.clone())
            .append_custom_title(&self.init_session_id, title)
            .await
            .expect("user rename");
    }

    /// Run the transcript past `JsonlWriter`'s REAL 32 KiB metadata
    /// backstop, which is what an interview of any length does to this
    /// transcript.
    ///
    /// Deliberately NOT a hand-written unmarked `custom-title` line: the
    /// record has to come out of `plan_re_append` itself, so the test
    /// keeps pinning the production behaviour if that rebuild ever changes
    /// shape. `append_file_history_snapshot` accounts its bytes against
    /// the backstop counter without polling it; the next side-record
    /// append is what fires the poll. Both are ordinary public writer
    /// calls — no test-only hook.
    async fn trip_the_metadata_backstop(&self) {
        let writer = session::jsonl::writer::JsonlWriter::new(self.transcript(), self.fs.clone());
        writer
            .append_file_history_snapshot(&json!({
                "type": "file-history-snapshot",
                "sessionId": self.init_session_id,
                "messageId": "interview",
                "snapshot": "x".repeat(
                    session::jsonl::re_append::METADATA_REAPPEND_BACKSTOP_BYTES,
                ),
            }))
            .await
            .expect("bulk interview transcript");
        writer
            .append_permission_mode("default")
            .await
            .expect("the append that polls the backstop");
        assert!(
            !self.latest_title_record_carries_the_marker(),
            "the backstop must really have re-emitted the title UNMARKED — without \
             that this test proves nothing"
        );
    }

    /// Whether the LAST `custom-title` on disk still carries
    /// `mobileEmptySession`. Only a probe: nothing in production may
    /// decide anything from the last record alone.
    fn latest_title_record_carries_the_marker(&self) -> bool {
        let transcript = fs::read_to_string(self.transcript()).expect("read the pinned transcript");
        let mut marked = false;
        for line in transcript.lines() {
            let Ok(value) = serde_json::from_str::<Value>(line) else {
                continue;
            };
            if value.get("type").and_then(Value::as_str) != Some("custom-title")
                || value.get("sessionId").and_then(Value::as_str)
                    != Some(self.init_session_id.as_str())
            {
                continue;
            }
            marked = value.get("mobileEmptySession").and_then(Value::as_u64) == Some(1);
        }
        marked
    }

    /// What a scaffold does to this app's conversation: the record commits, then
    /// the service reports the committed record to its conversation host. A
    /// failed rename is logged and never undoes the scaffold, exactly as the
    /// broker treats it; the boot sweep is what repairs it.
    async fn scaffold(&self, name: &str) -> Result<local_apps::AppRecord, String> {
        let record = self
            .service
            .record(&self.app_id)
            .await
            .map_err(|error| error.to_string())?;
        seed_commit_readiness(self.root.path(), &record);
        let committed = self
            .service
            .commit_scaffold(&self.app_id, name, "a confirmed brief", None, None)
            .await
            .map_err(|error| error.to_string())?;
        if let Err(error) = self.titles.app_scaffolded(&committed).await {
            tracing::warn!(%error, "pinned init-session rename failed; the boot sweep will retry");
        }
        Ok(committed)
    }

    async fn run_boot_backfill_sweep(&self) {
        crate::mobile::host::run_app_boot_backfill_sweep(
            self.lingxi_home.clone(),
            self.root.path().to_string_lossy().to_string(),
            self.root.path().to_path_buf(),
            self.fs.clone(),
            self.service.clone(),
            self.broker.clone(),
        )
        .await;
    }
}

/// The "+" button's state: an unscaffolded shell whose pinned init session
/// is titled with the `untitled` placeholder.
async fn pinned_shell() -> PinnedShell {
    let root = TempDir::new().expect("tempdir");
    let lingxi_home = root.path().join(".lingxi");
    fs::create_dir_all(&lingxi_home).expect("create lingxi home");
    let fs_impl: Arc<dyn lingxi_core::host::FileSystem> = Arc::new(
        platform_posix_minimal::PosixFileSystem::new(root.path().to_path_buf()),
    );
    let service = Arc::new(
        AppService::load(
            root.path(),
            Arc::new(FixedClock::new(1)),
            Arc::new(NoopAppEventObserver),
        )
        .await
        .expect("load app service"),
    );
    // The broker is only here for the boot sweep, which asks it to rewrite a
    // lost guided contract; nothing in these tests builds or publishes.
    let broker = LocalAppsHostBroker::new_with_physical_memory(
        root.path().to_path_buf(),
        Arc::new(DiscardSink),
        None,
        false,
        None,
        0,
    );
    assert!(broker.attach_service(service.clone()).is_ok());
    let titles = SessionTitles::host(
        SessionCatalog {
            lingxi_home: lingxi_home.clone(),
            fs: fs_impl.clone(),
        },
        root.path().to_path_buf(),
    );

    let record = service
        .create_app_with_mode(None, "", None, local_apps::CreateMode::Shell, None)
        .await
        .expect("create the shell app");
    assert!(!record.scaffolded);
    assert_eq!(record.name, local_apps::PLACEHOLDER_APP_NAME);

    let init_session_id = crate::mobile::host::mint_app_init_session(
        &lingxi_home,
        &root.path().to_string_lossy(),
        root.path(),
        fs_impl.clone(),
        &record,
    )
    .await
    .expect("mint the pinned init session");
    service
        .set_init_session(&record.id, &init_session_id)
        .await
        .expect("pin the init session");

    let shell = PinnedShell {
        root,
        service,
        broker,
        titles,
        lingxi_home,
        fs: fs_impl,
        app_id: record.id,
        init_session_id,
        workspace_rel: record.workspace_rel.clone(),
    };
    // The defect this task exists for: the placeholder is already on disk.
    assert_eq!(shell.title(), local_apps::PLACEHOLDER_APP_NAME);
    shell
}

#[tokio::test]
async fn scaffold_renames_the_pinned_session_when_the_user_never_renamed_it() {
    let shell = pinned_shell().await;

    shell.scaffold("打飞机").await.expect("scaffold");

    assert_eq!(shell.title(), "打飞机");
}

/// A rename that fails is only "retryable" if something actually retries
/// it. The scaffold has already committed by then and is NOT rolled back,
/// so the boot sweep is the whole of that guarantee.
#[cfg(unix)]
#[tokio::test]
async fn the_boot_sweep_reconciles_a_title_a_failed_rename_left_behind() {
    use std::os::unix::fs::PermissionsExt;

    let shell = pinned_shell().await;
    // Make the append genuinely fail: a read-only transcript cannot be
    // opened for append. This is the real failure path, not a skipped one.
    let transcript = shell.transcript();
    fs::set_permissions(&transcript, fs::Permissions::from_mode(0o444))
        .expect("make the transcript read-only");

    shell
        .scaffold("打飞机")
        .await
        .expect("a failed rename must not roll the scaffold back");

    fs::set_permissions(&transcript, fs::Permissions::from_mode(0o644))
        .expect("restore the transcript");
    assert_eq!(
        shell.title(),
        local_apps::PLACEHOLDER_APP_NAME,
        "the rename really did fail, so the retry has something to repair"
    );
    assert!(
        shell
            .service
            .record(&shell.app_id)
            .await
            .expect("record")
            .scaffolded,
        "the scaffold itself committed"
    );

    shell.run_boot_backfill_sweep().await;

    assert_eq!(
        shell.title(),
        "打飞机",
        "a failed rename must have a real trigger that fixes it later"
    );
}

/// The real flow, not the shortest one: an interview long enough to trip
/// the transcript writer's 32 KiB metadata backstop still gets its title.
///
/// This is the test whose absence made the whole reconciliation invisible.
/// The backstop re-emits the title as a PLAIN `custom-title`, so a
/// predicate that read the marker off the LAST record declined for every
/// app created through this flow and they all kept `untitled` forever —
/// with `scaffold_renames_the_pinned_session_when_the_user_never_renamed_it`
/// (a transcript of two lines) staying green throughout.
#[tokio::test]
async fn the_rename_survives_the_metadata_backstop_a_real_interview_trips() {
    let shell = pinned_shell().await;
    shell.trip_the_metadata_backstop().await;

    shell.scaffold("打飞机").await.expect("scaffold");

    assert_eq!(
        shell.title(),
        "打飞机",
        "an interview longer than 32 KiB must not cost the app its name"
    );
}

/// The other half, and the one that must never regress: tolerating the
/// backstop's unmarked echo must not make a real `/rename` overwritable.
///
/// After `/rename`, the backstop echoes the USER'S title unmarked — text
/// the anchor never carried — so the predicate declines, immediately and
/// on every later boot sweep.
#[tokio::test]
async fn a_user_rename_still_wins_after_the_backstop_echoes_it() {
    let shell = pinned_shell().await;
    shell.user_rename("我的宝贝项目").await;
    shell.trip_the_metadata_backstop().await;
    assert_eq!(
        shell.title(),
        "我的宝贝项目",
        "the backstop echoes the user's title, so that is what the scaffold sees"
    );

    shell.scaffold("打飞机").await.expect("scaffold");
    shell.run_boot_backfill_sweep().await;

    assert_eq!(shell.title(), "我的宝贝项目");
}

#[tokio::test]
async fn an_immediate_rename_never_clobbers_a_user_rename() {
    let shell = pinned_shell().await;
    shell.user_rename("我的宝贝项目").await;

    shell.scaffold("打飞机").await.expect("scaffold");

    assert_eq!(shell.title(), "我的宝贝项目");
}

#[tokio::test]
async fn the_boot_sweep_never_clobbers_a_user_rename_either() {
    let shell = pinned_shell().await;
    shell.user_rename("我的宝贝项目").await;
    shell.scaffold("打飞机").await.expect("scaffold");

    shell.run_boot_backfill_sweep().await;

    assert_eq!(shell.title(), "我的宝贝项目");
}

/// `workspace/LINGXI.md` is the ONE channel that reaches the model for an
/// unscaffolded shell (`r3-e2e-trace-01`). Nothing in the create
/// transaction ever revisits it after the initial write, so if it is ever
/// lost — a partial restore, a wiped workspace mount — the boot sweep must
/// be the thing that notices and rewrites it; otherwise the interview
/// never restarts and the agent sees an ordinary empty directory.
#[tokio::test]
async fn boot_sweep_repairs_a_missing_guided_workspace_contract() {
    let shell = pinned_shell().await;
    let lingxi_md = workspace_of(shell.root.path(), &shell.app_id).join("LINGXI.md");
    // `pinned_shell()` builds its record straight off `AppService`,
    // bypassing the broker's create-time initializer hook (the one that
    // normally writes the guided contract) — so this fixture's workspace
    // starts with no `LINGXI.md` at all, which is exactly the "lost"
    // state this test needs. Removing it too makes that starting point
    // explicit regardless of what the fixture happens to do.
    let _ = fs::remove_file(&lingxi_md);
    assert!(
        !lingxi_md.exists(),
        "the guided contract must be absent before the sweep runs"
    );

    shell.run_boot_backfill_sweep().await;

    let repaired = fs::read_to_string(&lingxi_md)
        .expect("the boot sweep must rewrite a missing guided workspace contract");
    assert!(
        repaired.contains("has no shape yet"),
        "the repaired file must be the real guided contract, not a stub: {repaired}"
    );
}

/// Same repair, but for a TRUNCATED file rather than an absent one — an
/// interrupted write can leave bytes on disk that are not the contract.
#[tokio::test]
async fn boot_sweep_repairs_a_truncated_guided_workspace_contract() {
    let shell = pinned_shell().await;
    let lingxi_md = workspace_of(shell.root.path(), &shell.app_id).join("LINGXI.md");
    fs::write(&lingxi_md, "").expect("truncate the guided contract to simulate a partial write");

    shell.run_boot_backfill_sweep().await;

    let repaired =
        fs::read_to_string(&lingxi_md).expect("guided contract still present after repair");
    assert!(
        repaired.contains("has no shape yet"),
        "a truncated guided contract must be rewritten, not left empty: {repaired}"
    );
}

/// The repair must be scoped to UNSCAFFOLDED shells: once an app is
/// formed, `workspace/LINGXI.md` carries the FORMAL contract, and step 0
/// rewriting it back to the guided text on every boot would erase the
/// surface-specific rules the formal contract exists to state.
#[tokio::test]
async fn boot_sweep_never_rewrites_a_formed_apps_formal_contract() {
    let shell = pinned_shell().await;
    shell.scaffold("打飞机").await.expect("scaffold");
    let lingxi_md = workspace_of(shell.root.path(), &shell.app_id).join("LINGXI.md");
    // What the service's scaffold writes over the guided contract is the
    // service's own business (its tests pin the text); this test only needs a
    // formed app whose contract is not the guided one.
    fs::write(
        &lingxi_md,
        "# Local App: 打飞机\n\nThis app's surface is `canvas`.\n",
    )
    .expect("write a formal contract");
    let formal_before = fs::read_to_string(&lingxi_md).expect("formal contract");
    assert!(
        !formal_before.contains("has no shape yet"),
        "a formed app's contract must already be the FORMAL one: {formal_before}"
    );

    shell.run_boot_backfill_sweep().await;

    let formal_after = fs::read_to_string(&lingxi_md).expect("formal contract after sweep");
    assert_eq!(
        formal_before, formal_after,
        "step 0 must never overwrite a formed app's formal contract with the guided one"
    );
}

/// Clause 1 of the predicate, pinned directly: an app still in its
/// interview keeps the placeholder title even when its record already
/// carries a real name. Driven through `reconcile_app_init_session_title`
/// rather than a whole scaffold, because the app paths cannot currently
/// produce this state — the point is that the rule survives a refactor
/// that lets them.
#[tokio::test]
async fn reconciliation_waits_for_the_scaffold_commit_before_renaming() {
    let shell = pinned_shell().await;
    let mut record = shell.service.record(&shell.app_id).await.expect("record");
    record.name = "打飞机".into();
    assert!(!record.scaffolded);

    let renamed = reconcile_app_init_session_title(
        &shell.lingxi_home,
        shell.root.path(),
        shell.fs.clone(),
        &record,
    )
    .await
    .expect("reconcile");

    assert!(!renamed, "an unscaffolded shell is not renamed");
    assert_eq!(shell.title(), local_apps::PLACEHOLDER_APP_NAME);

    // The same record, one field later: the commit is the only thing that
    // was missing.
    record.scaffolded = true;
    assert!(reconcile_app_init_session_title(
        &shell.lingxi_home,
        shell.root.path(),
        shell.fs.clone(),
        &record,
    )
    .await
    .expect("reconcile"));
    assert_eq!(shell.title(), "打飞机");
}

/// The discriminator, stated as a unit. Three writers share the
/// `custom-title` channel, only one of them marks its records, and a
/// fourth — the writer's own 32 KiB metadata backstop — re-emits whatever
/// the title currently is, UNMARKED. So the question is never "is the last
/// record marked" but "did anyone write text mobile did not".
#[test]
fn a_placeholder_is_told_from_a_user_rename_by_text_against_the_anchor() {
    let session = "11111111-2222-3333-4444-555555555555";
    let anchor = format!(
        r#"{{"type":"custom-title","customTitle":"untitled","sessionId":"{session}","mobileEmptySession":1}}"#
    );
    // What `plan_re_append` writes when the backstop fires: the anchor's
    // own text, rebuilt without the marker.
    let backstop_echo =
        format!(r#"{{"type":"custom-title","customTitle":"untitled","sessionId":"{session}"}}"#);
    let user_rename = format!(
        r#"{{"type":"custom-title","customTitle":"我的宝贝项目","sessionId":"{session}"}}"#
    );
    let other_session = r#"{"type":"custom-title","customTitle":"elsewhere","sessionId":"99999999-2222-3333-4444-555555555555"}"#;

    assert!(latest_custom_title_is_mobile_placeholder(&anchor, session));
    // An unmarked record echoing the anchor's text is the backstop, not a
    // user. Reading the marker off the last record here is what made
    // `reconcile_app_init_session_title` unreachable in production.
    assert!(latest_custom_title_is_mobile_placeholder(
        &format!("{anchor}\n{backstop_echo}"),
        session
    ));
    // Text mobile never wrote, after the anchor: a user rename, and it
    // stays one however many times the backstop echoes it afterwards.
    assert!(!latest_custom_title_is_mobile_placeholder(
        &format!("{anchor}\n{user_rename}"),
        session
    ));
    assert!(!latest_custom_title_is_mobile_placeholder(
        &format!("{anchor}\n{user_rename}\n{user_rename}"),
        session
    ));
    // An unmarked record with no anchor before it — a `session::branch`
    // fork's title — is superseded by an anchor that follows it.
    assert!(!latest_custom_title_is_mobile_placeholder(
        &user_rename,
        session
    ));
    assert!(latest_custom_title_is_mobile_placeholder(
        &format!("{user_rename}\n{anchor}"),
        session
    ));
    // A record for another session never decides this one.
    assert!(latest_custom_title_is_mobile_placeholder(
        &format!("{anchor}\n{other_session}"),
        session
    ));
    // Nothing this host anchored: leave it alone.
    assert!(!latest_custom_title_is_mobile_placeholder("", session));
    // The effective title is still the LAST record's, marked or not.
    assert_eq!(
        latest_custom_title(&format!("{anchor}\n{user_rename}"), session)
            .expect("a title")
            .0,
        "我的宝贝项目"
    );
}

/// The sweep decides a guided contract is healthy by the first line it expects
/// to find. That line is the service's text, so the one thing keeping the two
/// in step is this test: a contract the service wrote must be left alone, or the
/// sweep would "repair" every healthy shell on every launch.
#[tokio::test]
async fn the_boot_sweep_recognises_the_guided_contract_the_service_writes() {
    let shell = pinned_shell().await;
    let record = shell.service.record(&shell.app_id).await.expect("record");
    shell
        .broker
        .write_guided_contract_value(&record)
        .await
        .expect("the service writes its guided contract");
    let lingxi_md = workspace_of(shell.root.path(), &shell.app_id).join("LINGXI.md");
    let mut contract = fs::read_to_string(&lingxi_md).expect("read the guided contract");
    // A rewrite would drop this line; a recognised contract keeps it.
    contract.push_str("\n<!-- left alone by the sweep -->\n");
    fs::write(&lingxi_md, &contract).expect("mark the contract");

    shell.run_boot_backfill_sweep().await;

    assert_eq!(
        fs::read_to_string(&lingxi_md).expect("read the contract after the sweep"),
        contract,
        "the sweep's idea of a guided contract's first line no longer matches the \
         service's, so it rewrites every healthy shell on every launch"
    );
}

#[test]
fn removing_a_minted_session_file_deletes_that_transcript_and_nothing_else() {
    let root = TempDir::new().expect("tempdir");
    let lingxi_home = root.path().join(".lingxi");
    let record = local_apps::AppState::create(
        "abc12345".into(),
        "Notes".into(),
        "a notes app".into(),
        None,
        1,
    )
    .record;
    let catalog = app_session_dir(&lingxi_home, root.path(), &record);
    fs::create_dir_all(&catalog).expect("create the app's session catalog");
    fs::write(catalog.join("minted.jsonl"), "").expect("write the minted transcript");
    fs::write(catalog.join("kept.jsonl"), "").expect("write another transcript");

    assert!(remove_app_session_file(
        &lingxi_home,
        root.path(),
        &record,
        "minted"
    ));

    assert!(!catalog.join("minted.jsonl").exists());
    assert!(catalog.join("kept.jsonl").exists());
    assert!(
        !remove_app_session_file(&lingxi_home, root.path(), &record, "minted"),
        "a file that is already gone is reported as not removed"
    );
}
