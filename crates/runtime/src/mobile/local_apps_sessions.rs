//! The Local App service's view of the conversations the engine keeps for apps.
//!
//! Every app has a conversation (an engine session whose transcript lives under
//! `<lingxi_home>/projects/`), and a few things about an app have to be done to
//! that transcript: a created-then-abandoned session file is removed, the
//! pinned init session is renamed once the scaffold commits, and the boot
//! backfill sweep repairs titles a crash left behind. The service knows none of
//! that: it is told, through [`ConversationHost`], that a scaffold committed.

use async_trait::async_trait;
use local_app_service::broker::canonical_cwd_string;
use local_app_service::host::ConversationHost;
use serde_json::Value;
use std::sync::Arc;

/// Renames an app's pinned init session when the service reports its scaffold.
pub(crate) struct SessionTitles {
    catalog: SessionCatalog,
    data_root: std::path::PathBuf,
}

impl SessionTitles {
    pub(crate) fn host(
        catalog: SessionCatalog,
        data_root: std::path::PathBuf,
    ) -> Arc<dyn ConversationHost> {
        Arc::new(Self { catalog, data_root })
    }
}

#[async_trait]
impl ConversationHost for SessionTitles {
    async fn app_scaffolded(&self, record: &local_apps::AppRecord) -> Result<bool, String> {
        reconcile_app_init_session_title(
            &self.catalog.lingxi_home,
            &self.data_root,
            self.catalog.fs.clone(),
            record,
        )
        .await
    }
}

/// Delete a session file this host minted into an app's workspace catalog.
/// Used by both create paths when `set_init_session` refuses their id — the
/// set-once pin is the arbiter, and the loser's file would otherwise linger as
/// a phantom conversation row in the app's session list.
pub(crate) fn remove_app_session_file(
    lingxi_home: &std::path::Path,
    data_root: &std::path::Path,
    record: &local_apps::AppRecord,
    session_id: &str,
) -> bool {
    std::fs::remove_file(app_session_file(lingxi_home, data_root, record, session_id)).is_ok()
}

/// The app's whole session CATALOG directory — `<lingxi_home>/projects/<dir>`,
/// where `<dir>` is the sanitized spelling of the app workspace's canonical
/// cwd.
///
/// This directory lives OUTSIDE the app's own `apps/<id>` tree, so
/// `AppService::delete_app` cannot reach it: every transcript the host minted
/// for the app — including a chat-origin app's full FORK of the user's
/// conversation — survives the app unless a caller removes this directory
/// explicitly (`handle_delete_app` does).
///
/// One derivation, three callers ([`remove_app_session_file`],
/// [`app_session_file`] and the delete path), so none of them can disagree
/// about which directory is the app's catalog. `canonical_cwd_string`'s
/// ancestor walk means this stays the SAME spelling after the workspace has
/// been deleted, which is exactly the state the delete path reads it in.
pub(crate) fn app_session_dir(
    lingxi_home: &std::path::Path,
    data_root: &std::path::Path,
    record: &local_apps::AppRecord,
) -> std::path::PathBuf {
    let workspace_cwd = canonical_cwd_string(&data_root.join(&record.workspace_rel));
    lingxi_home
        .join("projects")
        .join(session::jsonl::path::project_dir_name(&workspace_cwd))
}

/// Where a session this host minted for an app lives on disk. The one spelling
/// [`remove_app_session_file`] and [`reconcile_app_init_session_title`] both
/// derive their path from, so they can never disagree about which file is the
/// app's pinned init session.
fn app_session_file(
    lingxi_home: &std::path::Path,
    data_root: &std::path::Path,
    record: &local_apps::AppRecord,
    session_id: &str,
) -> std::path::PathBuf {
    app_session_dir(lingxi_home, data_root, record).join(format!("{session_id}.jsonl"))
}

/// The session-catalog facts the `LocalAppScaffold` commit point needs in order
/// to rename an app's pinned init session: where transcripts live
/// (`<lingxi_home>/projects/…`) and the filesystem that reads and appends them.
///
/// Attached by the engine builder, which owns both. `self.root` is already the
/// apps data root, so `lingxi_home` is the only path the broker is missing —
/// and it is deliberately passed rather than re-derived from `root`, because
/// `mobile_apps_data_root` degrades to `cwd` when `lingxi_home` has no usable
/// parent, and inverting that guess would point the rename at the wrong
/// catalog on exactly the configuration that already went wrong.
#[derive(Clone)]
pub(crate) struct SessionCatalog {
    /// The engine's per-profile data dir — `projects/` hangs off it.
    pub(crate) lingxi_home: std::path::PathBuf,
    /// The filesystem transcripts are read and appended through.
    pub(crate) fs: Arc<dyn lingxi_core::host::FileSystem>,
}

/// The latest effective `custom-title` for `session_id` in a transcript: the
/// title it resolves to, and whether that title is still one MOBILE wrote —
/// i.e. whether the user has never renamed this session themselves.
///
/// "Latest effective" mirrors [`session::jsonl::reader`] exactly: it folds
/// every `custom-title` line whose `sessionId` matches into one map slot, so
/// the LAST one on disk wins, and a record whose `customTitle` is not a string
/// is skipped (the reader's `and_then(Value::as_str)` drops it too).
///
/// ⚠️ The second half deliberately does NOT read the marker off the last
/// record. It cannot: the transcript writer's own 32 KiB metadata backstop
/// re-emits the CURRENT title as a PLAIN, unmarked `custom-title`
/// (`session::jsonl::re_append::plan_re_append` rebuilds the record from
/// `{type, customTitle, sessionId}` and has no marker to carry), so in any
/// interview long enough to trip it the last record is unmarked even though
/// nobody renamed anything. Reading the marker off the last record alone made
/// [`reconcile_app_init_session_title`] unreachable in production — see that
/// function and [`latest_custom_title_is_mobile_placeholder`].
///
/// So the scan tracks the ANCHOR — the title on the most recent marked record
/// — and treats an unmarked record as a user rename only when its text
/// DIFFERS from the anchor. A backstop echo copies the anchor's text verbatim;
/// a `/rename` writes something else.
pub(crate) fn latest_custom_title(transcript: &str, session_id: &str) -> Option<(String, bool)> {
    let mut latest: Option<String> = None;
    // The title on the most recent record that carried the mobile marker.
    // `None` until one is seen — an unmarked record BEFORE any anchor
    // (a `session::branch` fork's title, say) is superseded by the anchor and
    // must not poison it.
    let mut anchor: Option<String> = None;
    let mut user_renamed = false;
    for line in transcript.lines() {
        let Ok(value) = serde_json::from_str::<Value>(line) else {
            continue;
        };
        if value.get("type").and_then(Value::as_str) != Some("custom-title") {
            continue;
        }
        if value.get("sessionId").and_then(Value::as_str) != Some(session_id) {
            continue;
        }
        let Some(title) = value.get("customTitle").and_then(Value::as_str) else {
            continue;
        };
        if value.get("mobileEmptySession").and_then(Value::as_u64) == Some(1) {
            // Mobile is the only writer that marks, and it only marks a title
            // it was entitled to write, so its own record re-establishes the
            // baseline.
            anchor = Some(title.to_string());
            user_renamed = false;
        } else if anchor.as_deref().is_some_and(|anchored| anchored != title) {
            user_renamed = true;
        }
        latest = Some(title.to_string());
    }
    latest.map(|title| (title, anchor.is_some() && !user_renamed))
}

/// Whether this session's title is still one MOBILE wrote, i.e. the user has
/// never renamed it.
///
/// ⚠️ The title TEXT cannot decide this and must never be used to.
/// `/rename` (`orchestrator`'s `append_custom_title`), a hook's `sessionTitle`
/// and mobile's own placeholder anchor all write the SAME `custom-title`
/// channel with the same shape; the only discriminator is the extra
/// `"mobileEmptySession":1` field that
/// [`session::jsonl::writer::JsonlWriter::append_mobile_empty_session`] adds.
/// An ordinary `custom-title` carrying text mobile never wrote, anywhere after
/// the anchor, turns this `false` and keeps it `false` — which is the point.
///
/// ⛔ It is NOT enough to look at the marker on the LAST record, and that
/// mistake made this whole path dead code in production. `JsonlWriter`'s
/// metadata backstop fires once
/// [`session::jsonl::re_append::METADATA_REAPPEND_BACKSTOP_BYTES`] (32 KiB)
/// have been appended, re-emitting the current title as a PLAIN `custom-title`
/// — [`session::jsonl::re_append::plan_re_append`] rebuilds the record from
/// `{type, customTitle, sessionId}` and has no marker to carry. Worse, mobile
/// keeps ONE writer across sessions and `JsonlWriter::retarget` does not reset
/// that counter, so a user who chatted before pressing "+" can trip the
/// backstop on the interview's very FIRST append. An interview therefore
/// strips the marker as a matter of course, and a last-record test would make
/// every app created through this flow keep `untitled` forever.
///
/// So [`latest_custom_title`] anchors on the most recent MARKED record and
/// only counts a LATER unmarked record as a user rename when its text differs
/// from that anchor. A backstop echo copies the anchor verbatim; a `/rename`
/// does not.
///
/// The one case this cannot separate is a user who runs `/rename` and types
/// the placeholder string EXACTLY: `append_custom_title` then emits a record
/// byte-identical (modulo timestamp) to a backstop echo, so no reader can tell
/// them apart. Clause 3 of [`reconcile_app_init_session_title`] still declines
/// whenever the title already equals `record.name`, so the residue is a user
/// who deliberately renamed their session to `untitled` and then confirmed a
/// different app name.
///
/// Known cases where this declines for a session the user never touched. The
/// bias is deliberate and one-directional: a false negative costs a stale
/// title, a false positive overwrites something a user typed.
/// - a transcript with no `custom-title` at all — nothing this host anchored,
///   so nothing for it to reconcile;
/// - a CHAT-ORIGIN app, whose init session is forked by
///   `session::branch::create_branch_to_cwd`. That fork writes its own
///   unmarked `custom-title` (from `record.name`, i.e. the placeholder), so a
///   chat-origin shell keeps its `untitled` session title. Closing that would
///   mean either marking a forked, non-empty session as a mobile empty session
///   — which is what `mobileEmptySession` means elsewhere — or reasoning from
///   the title text, which is exactly what this function exists to avoid. It
///   is left open rather than papered over.
pub(crate) fn latest_custom_title_is_mobile_placeholder(
    transcript: &str,
    session_id: &str,
) -> bool {
    latest_custom_title(transcript, session_id).is_some_and(|(_, marker)| marker)
}

/// The ONE reconciliation between an app's pinned init session title and
/// `record.name`, shared by the `LocalAppScaffold` commit point (which calls it
/// immediately) and the boot backfill sweep (which is the retry that makes a
/// failed immediate rename recoverable rather than permanent).
///
/// The full predicate, all three clauses required:
/// 1. `record.scaffolded` — an app still in its interview is SUPPOSED to read
///    `untitled`; renaming it early would put a real name in the library on a
///    record that still opens the interview.
/// 2. the session's effective title is still one MOBILE wrote — the user has
///    not renamed it. Anchored on the most recent `mobileEmptySession: 1`
///    record, NOT on the marker of the last record: the transcript writer's
///    32 KiB metadata backstop re-emits the title unmarked, which is exactly
///    what an interview does. See [`latest_custom_title`] and
///    [`latest_custom_title_is_mobile_placeholder`].
/// 3. that title differs from `record.name` — otherwise there is nothing to do,
///    and this is also what makes the boot sweep idempotent.
///
/// The rename is written with `append_mobile_empty_session` again, KEEPING the
/// marker: the user still has not renamed anything, so a later `/rename` must
/// still be able to take precedence over a subsequent reconcile.
///
/// Returns `Ok(true)` when a rename was written, `Ok(false)` when the predicate
/// declined. A missing transcript is `Ok(false)`, not an error — the sweep's
/// re-anchor step, which runs before this one, writes `record.name` directly.
pub(crate) async fn reconcile_app_init_session_title(
    lingxi_home: &std::path::Path,
    data_root: &std::path::Path,
    fs: Arc<dyn lingxi_core::host::FileSystem>,
    record: &local_apps::AppRecord,
) -> Result<bool, String> {
    // Clause 1. Today no production state can reach this with a name that
    // differs from the session title — a shell is minted with `record.name`,
    // and `record.name` cannot change before the scaffold commits — so the
    // guard is unobservable through the app paths. It is still load-bearing as
    // a specification, and
    // `reconciliation_waits_for_the_scaffold_commit_before_renaming` pins it
    // directly so it cannot be deleted as dead code: a record that is still in
    // its interview must keep showing the placeholder, whatever its name says.
    if !record.scaffolded {
        return Ok(false);
    }
    let Some(init_id) = record.init_session_id.as_deref() else {
        return Ok(false);
    };
    let path = app_session_file(lingxi_home, data_root, record, init_id);
    let Some(path_str) = path.to_str() else {
        return Err(format!(
            "init-session path is not UTF-8: {}",
            path.display()
        ));
    };
    let Ok(file) = fs.read_file(path_str, None, None).await else {
        return Ok(false);
    };
    let Some((title, still_mobile_placeholder)) = latest_custom_title(&file.content, init_id)
    else {
        return Ok(false);
    };
    if !still_mobile_placeholder || title == record.name {
        return Ok(false);
    }
    session::jsonl::writer::JsonlWriter::new(path, fs)
        .append_mobile_empty_session(init_id, &record.name)
        .await
        .map_err(|error| format!("rename pinned init session: {error}"))?;
    Ok(true)
}

#[cfg(test)]
mod tests;
