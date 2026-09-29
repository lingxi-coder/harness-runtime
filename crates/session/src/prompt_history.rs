//! Persistent prompt history — the global `~/.lingxi/history.jsonl` store the
//! composer's up-arrow recall reads and every typed prompt appends to.
//!
//! 1:1 port of the claude-code 2.1.220 prompt-history module (the 2.1.218
//! changelog's "prompt history entries being dropped or duplicated when
//! history writes raced or failed" fix), scoped to the surfaces the port has:
//!
//! - **Entry shape** (`cuy`): `{display, pastedContents, timestamp, project,
//!   sessionId}` JSONL rows, key order locked by struct order. The external
//!   paste-content store (`HFd`/`ouy`, hash-externalised text pastes) is
//!   unported — the port always writes an empty `pastedContents` object, which
//!   is also CC's shape for every plain typed prompt.
//! - **In-memory queue + locked append** (`zIe` + `auy`): entries queue in
//!   memory and are flushed by (1) ensuring the file exists in append mode
//!   `0o600` (`Gi().append(r,"",384)`), (2) taking a `history.jsonl.lock`
//!   lockfile with `stale: 10s`, `retries: 3`, `minTimeout: 50ms` and an
//!   error-level `History lock compromised: {err}` report, (3) appending the
//!   queued rows, then removing exactly the written entries from the queue —
//!   a failed write keeps them queued (the 2.1.218 no-drop half).
//! - **Read-merge dedupe** (`bHo`): reads yield the in-memory queue
//!   newest-first, then the on-disk rows newest-first, suppressing any disk row
//!   whose `${timestamp}\x00${sessionId ?? ""}` key is already QUEUED — the
//!   2.1.218 no-duplicate half (a row that raced into the file while still
//!   queued is yielded once). CC's set `t` is seeded from the queue and never
//!   gains a disk key, so two identical on-disk rows are both yielded.
//!   Unparseable lines log `Failed to parse history line: {err}` and are
//!   skipped.
//! - **Recall order** (`THo`): project-filtered, current-session entries
//!   first, capped at 100 (`mHo`) — the composer seeds its recall list from
//!   this.
//! - **Consecutive-duplicate suppression** (`luy`): re-submitting the same
//!   display text in the same project+session (with no pasted contents on
//!   either side) does not append a second row.
//!
//! The `CLAUDE_CODE_SKIP_PROMPT_HISTORY` env escape hatch (`cgr`) is honored
//! under both the `LINGXI_` and `CLAUDE_CODE_` prefixes.

use std::collections::HashSet;
use std::fs;
use std::io::Write as _;
use std::path::{Path, PathBuf};
use std::sync::Mutex;
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use serde::{Deserialize, Serialize};

/// Recall cap (`mHo = 100`).
const MAX_RECALL_ENTRIES: usize = 100;

/// Lockfile staleness (`stale: 1e4` ms).
const LOCK_STALE: Duration = Duration::from_millis(10_000);

/// Lock retry schedule (`retries: {retries: 3, minTimeout: 50}` — p-retry's
/// default factor-2 backoff: 50ms, 100ms, 200ms).
const LOCK_RETRY_DELAYS_MS: [u64; 3] = [50, 100, 200];

/// One `history.jsonl` row. Field order matches CC's `cuy` object literal
/// (`{display, pastedContents, timestamp, project, sessionId}`).
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct PromptHistoryEntry {
    /// The prompt as typed (paste placeholders included).
    pub display: String,
    /// Pasted-content map — always serialized (CC writes `{}` for plain
    /// prompts); the externalised paste store is unported so the port never
    /// populates it.
    #[serde(rename = "pastedContents", default)]
    pub pasted_contents: serde_json::Map<String, serde_json::Value>,
    /// `Date.now()` milliseconds.
    pub timestamp: i64,
    /// The project directory the prompt was typed in (`Dl()`).
    #[serde(default)]
    pub project: Option<String>,
    /// The owning session (`kt()`); part of the dedupe key.
    #[serde(rename = "sessionId", default)]
    pub session_id: Option<String>,
}

impl PromptHistoryEntry {
    /// The merge/dedupe key — `${timestamp}\x00${sessionId ?? ""}`.
    #[must_use]
    fn dedupe_key(&self) -> String {
        format!(
            "{}\u{0}{}",
            self.timestamp,
            self.session_id.as_deref().unwrap_or("")
        )
    }
}

#[derive(Debug, Default)]
struct PendingState {
    /// Queued, not-yet-flushed entries (`zIe`), oldest first.
    queue: Vec<PromptHistoryEntry>,
    /// The most recently enqueued entry (`Qcn`) for the consecutive-duplicate
    /// suppression (`luy`) — kept across flushes.
    last: Option<PromptHistoryEntry>,
}

/// The process-wide prompt-history store. Cheap to share behind an `Arc`; all
/// methods are `&self` (internal mutex).
#[derive(Debug)]
pub struct PromptHistoryStore {
    /// `~/.lingxi/history.jsonl` (`join(fn(), "history.jsonl")` — the GLOBAL
    /// config root, not per-project; rows carry a `project` field instead).
    path: PathBuf,
    /// This process's project key (`Dl()` — the session cwd).
    project: String,
    /// This process's session id (`kt()`).
    session_id: Option<String>,
    pending: Mutex<PendingState>,
    /// Serializes concurrent [`Self::flush`] calls within this process (CC
    /// wraps the flusher in `foe(auy)` — a single-flight guard — so two racing
    /// flushes can never double-append one batch).
    flush_gate: Mutex<()>,
}

impl PromptHistoryStore {
    /// Build a store rooted at `home` (the `~/.lingxi` config dir) for
    /// `project` (the session cwd) and `session_id`.
    #[must_use]
    pub fn new(home: &Path, project: &Path, session_id: Option<String>) -> Self {
        PromptHistoryStore {
            path: home.join("history.jsonl"),
            project: project.to_string_lossy().into_owned(),
            session_id,
            pending: Mutex::new(PendingState::default()),
            flush_gate: Mutex::new(()),
        }
    }

    /// Whether prompt-history persistence is disabled by env — CC's
    /// `Yt(process.env.CLAUDE_CODE_SKIP_PROMPT_HISTORY)` gate in `cgr`. `Yt` is
    /// the TRUTHY set (`1`/`true`/`yes`/`on`), NOT the complement of the falsy
    /// set, so `=2` / `=y` / `=enabled` keep recording.
    #[must_use]
    pub fn disabled_by_env() -> bool {
        [
            "LINGXI_SKIP_PROMPT_HISTORY",
            "CLAUDE_CODE_SKIP_PROMPT_HISTORY",
        ]
        .iter()
        .any(|var| lingxi_core::host::env::is_env_truthy(std::env::var(var).ok().as_deref()))
    }

    /// Queue one typed prompt (`cuy`). Applies the consecutive-duplicate
    /// suppression (`luy`): the same display text re-submitted in the same
    /// project+session, with no pasted contents, is dropped. Does NOT write —
    /// call [`Self::flush`] (typically from a background thread, and once on
    /// exit, mirroring CC's flush pump + exit flush).
    pub fn enqueue(&self, display: &str) {
        if display.trim().is_empty() {
            return;
        }
        let mut pending = self
            .pending
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        if let Some(last) = &pending.last {
            // `luy`: prev.display === next.display, same project+session, and
            // neither side has pastedContents (the port never populates them).
            if last.display == display
                && last.project.as_deref() == Some(self.project.as_str())
                && last.session_id == self.session_id
                && last.pasted_contents.is_empty()
            {
                return;
            }
        }
        let entry = PromptHistoryEntry {
            display: display.to_string(),
            pasted_contents: serde_json::Map::new(),
            timestamp: SystemTime::now()
                .duration_since(UNIX_EPOCH)
                .map_or(0, |d| d.as_millis().min(i64::MAX as u128) as i64),
            project: Some(self.project.clone()),
            session_id: self.session_id.clone(),
        };
        pending.last = Some(entry.clone());
        pending.queue.push(entry);
    }

    /// Flush the queued entries to `history.jsonl` under the lockfile (`auy`).
    /// Returns `true` when nothing remained queued afterwards. A failure keeps
    /// the queue intact (no drops); the read side dedupes by key (no dupes) —
    /// the 2.1.218 race-fix pair.
    pub fn flush(&self) -> bool {
        let _gate = self
            .flush_gate
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        let batch: Vec<PromptHistoryEntry> = {
            let pending = self
                .pending
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner);
            if pending.queue.is_empty() {
                return true;
            }
            pending.queue.clone()
        };

        // (1) Ensure the file exists, append mode 0o600 (`Gi().append(r,"",384)`).
        if let Err(err) = ensure_file(&self.path) {
            tracing::warn!("Failed to write prompt history: {err}");
            return false;
        }
        // (2) Take the lockfile.
        let lock = match LockGuard::acquire(&self.path) {
            Ok(lock) => lock,
            Err(err) => {
                tracing::warn!("Failed to write prompt history: {err}");
                return false;
            }
        };
        // (3) Append the batch as JSONL (`e.map((i) => Ie(i) + "\n")`).
        let mut buf = String::new();
        for entry in &batch {
            match serde_json::to_string(entry) {
                Ok(line) => {
                    buf.push_str(&line);
                    buf.push('\n');
                }
                Err(err) => {
                    tracing::warn!("Failed to write prompt history: {err}");
                }
            }
        }
        let write_result = fs::OpenOptions::new()
            .append(true)
            .open(&self.path)
            .and_then(|mut file| file.write_all(buf.as_bytes()));
        drop(lock);
        match write_result {
            Ok(()) => {
                // Remove exactly the written entries from the queue (`zIe =
                // zIe.filter((i) => !o.has(i))` — an IDENTITY set in CC).
                // The batch is a snapshot of the queue's prefix and enqueue
                // only appends, so dropping the first `batch.len()` entries is
                // the identity-equivalent: entries enqueued DURING the write
                // stay queued for the next flush.
                let mut pending = self
                    .pending
                    .lock()
                    .unwrap_or_else(std::sync::PoisonError::into_inner);
                let written = batch.len().min(pending.queue.len());
                pending.queue.drain(..written);
                pending.queue.is_empty()
            }
            Err(err) => {
                tracing::warn!("Failed to write prompt history: {err}");
                false
            }
        }
    }

    /// Read-merge iterator (`bHo`): queued entries newest-first, then on-disk
    /// rows newest-first, minus every disk row whose
    /// `${timestamp}\x00${sessionId ?? ""}` key is already QUEUED.
    /// Unparseable lines log `Failed to parse history line: {err}` and are
    /// skipped; a missing file yields the queue only.
    #[must_use]
    pub fn read_merged(&self) -> Vec<PromptHistoryEntry> {
        // `t = new Set(e.map(…))` — seeded from the QUEUE snapshot and never
        // added to afterwards, so a disk row is only ever tested against the
        // queue; two identical on-disk rows are both yielded.
        let mut queued: HashSet<String> = HashSet::new();
        let mut out = Vec::new();
        {
            let pending = self
                .pending
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner);
            for entry in pending.queue.iter().rev() {
                queued.insert(entry.dedupe_key());
                out.push(entry.clone());
            }
        }
        let Ok(contents) = fs::read_to_string(&self.path) else {
            return out; // ENOENT ⇒ the queue alone (`$t(i)==="ENOENT"`).
        };
        // Newest rows are appended last — iterate the lines in reverse (CC's
        // `HUn` backward line reader).
        for line in contents.lines().rev() {
            if line.trim().is_empty() {
                continue;
            }
            match serde_json::from_str::<PromptHistoryEntry>(line) {
                Ok(entry) => {
                    if !queued.contains(&entry.dedupe_key()) {
                        out.push(entry);
                    }
                }
                Err(err) => {
                    tracing::warn!("Failed to parse history line: {err}");
                }
            }
        }
        out
    }

    /// The composer recall list (`THo`): this project's entries, current
    /// session first, each block newest-first, capped at 100 (`mHo`). Returns
    /// display strings in RECALL order (most recent recall candidate first).
    #[must_use]
    pub fn recall_displays(&self) -> Vec<String> {
        let mut session_first: Vec<String> = Vec::new();
        let mut rest: Vec<String> = Vec::new();
        for entry in self.read_merged() {
            let Some(project) = &entry.project else {
                continue; // `typeof o.project !== "string"` guard
            };
            if project != &self.project {
                continue;
            }
            if entry.session_id.is_some() && entry.session_id == self.session_id {
                session_first.push(entry.display);
            } else {
                rest.push(entry.display);
            }
            if session_first.len() + rest.len() >= MAX_RECALL_ENTRIES {
                break;
            }
        }
        session_first.extend(rest);
        session_first.truncate(MAX_RECALL_ENTRIES);
        session_first
    }

    /// Count this project's history rows (the cleanup UI's
    /// `"{y} prompt(s) typed in this project"` source, `$Fd` with no filter).
    #[must_use]
    pub fn project_prompt_count(&self) -> usize {
        self.read_merged()
            .iter()
            .filter(|entry| entry.project.as_deref() == Some(self.project.as_str()))
            .take(MAX_RECALL_ENTRIES)
            .count()
    }
}

impl Drop for PromptHistoryStore {
    /// Exit flush (`uuy`, registered via `va(...)` in CC): persist anything
    /// still queued when the last handle drops.
    fn drop(&mut self) {
        let _ = self.flush();
    }
}

/// Create the history file if missing, with mode `0o600` (CC appends `""` with
/// mode `384`); never truncates an existing file.
fn ensure_file(path: &Path) -> std::io::Result<()> {
    if let Some(parent) = path.parent() {
        fs::create_dir_all(parent)?;
    }
    let mut options = fs::OpenOptions::new();
    options.append(true).create(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt as _;
        options.mode(0o600);
    }
    options.open(path).map(drop)
}

/// A `history.jsonl.lock` mkdir-lockfile with proper-lockfile's stale/retry
/// semantics: a lock dir older than [`LOCK_STALE`] is broken; acquisition
/// retries on the [`LOCK_RETRY_DELAYS_MS`] backoff. Releasing a lock that has
/// vanished (another process broke it as stale) reports the CC
/// `onCompromised` line at error level.
struct LockGuard {
    lock_dir: PathBuf,
}

impl LockGuard {
    fn acquire(target: &Path) -> std::io::Result<LockGuard> {
        let lock_dir = lock_dir_for(target);
        // proper-lockfile's `iig`: `nig.operation({retries: 3, minTimeout: 50})`
        // wraps ONE whole `n9i` pass per attempt and retries EVERY error
        // (`if (i.retry(s)) return`), surfacing the last one when the budget is
        // spent (`r(i.mainError())`). Acquisition is therefore bounded at four
        // passes / ~350ms whatever state the lock path is in.
        let mut attempt = 0usize;
        loop {
            match acquire_once(&lock_dir) {
                Ok(()) => return Ok(LockGuard { lock_dir }),
                Err(err) => {
                    if attempt >= LOCK_RETRY_DELAYS_MS.len() {
                        return Err(err);
                    }
                    std::thread::sleep(Duration::from_millis(LOCK_RETRY_DELAYS_MS[attempt]));
                    attempt += 1;
                }
            }
        }
    }
}

/// One proper-lockfile `n9i` pass: mkdir the lock dir, and on `EEXIST` break it
/// at most ONCE when it is stale. The break is `sIc` (rmdir — `ENOENT` counts as
/// success, every other error PROPAGATES) followed by the `{...t, stale: 0}`
/// re-entry, so a lock that survives the break is a hard `ELOCKED` rather than
/// another staleness round. Both halves are load-bearing: an entry the process
/// cannot remove (a plain file at the lock path, an unwritable parent) is a
/// fixed point — discarding the rmdir error and re-testing staleness spins
/// forever at 100% CPU while holding `flush_gate`.
fn acquire_once(lock_dir: &Path) -> std::io::Result<()> {
    let Err(err) = fs::create_dir(lock_dir) else {
        return Ok(());
    };
    if err.kind() != std::io::ErrorKind::AlreadyExists {
        return Err(err);
    }
    // `t.fs.stat(n, …)`: a lock that vanished between mkdir and stat re-enters
    // with `stale: 0` (`i.code === "ENOENT"`); any other stat error propagates.
    let mtime = match fs::metadata(lock_dir).and_then(|meta| meta.modified()) {
        Ok(mtime) => mtime,
        Err(err) if err.kind() == std::io::ErrorKind::NotFound => return fs::create_dir(lock_dir),
        Err(err) => return Err(err),
    };
    // `iIc`: `mtime < Date.now() - stale` — a future mtime is NOT stale.
    let stale = SystemTime::now()
        .duration_since(mtime)
        .is_ok_and(|age| age > LOCK_STALE);
    if !stale {
        return Err(err); // ELOCKED
    }
    match fs::remove_dir(lock_dir) {
        Ok(()) => {}
        Err(err) if err.kind() == std::io::ErrorKind::NotFound => {}
        Err(err) => return Err(err),
    }
    fs::create_dir(lock_dir)
}

impl Drop for LockGuard {
    fn drop(&mut self) {
        match fs::remove_dir(&self.lock_dir) {
            Ok(()) => {}
            Err(err) if err.kind() == std::io::ErrorKind::NotFound => {
                // Another process broke our lock as stale mid-write — the
                // proper-lockfile `onCompromised` path.
                tracing::error!("History lock compromised: {err}");
            }
            Err(err) => {
                tracing::error!("History lock compromised: {err}");
            }
        }
    }
}

fn lock_dir_for(target: &Path) -> PathBuf {
    let mut name = target.file_name().map_or_else(
        || std::ffi::OsString::from("history.jsonl"),
        std::ffi::OsStr::to_os_string,
    );
    name.push(".lock");
    target.with_file_name(name)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn store_in(dir: &Path, session: &str) -> PromptHistoryStore {
        PromptHistoryStore::new(dir, Path::new("/proj/a"), Some(session.to_string()))
    }

    #[test]
    fn enqueue_flush_and_reload_round_trips() {
        let dir = tempfile::tempdir().unwrap();
        let store = store_in(dir.path(), "s-1");
        store.enqueue("first prompt");
        // The dedupe key is `${Date.now()}\x00${sessionId}` — same-ms entries
        // in one session collide EXACTLY as in CC, so space the test enqueues.
        std::thread::sleep(Duration::from_millis(2));
        store.enqueue("second prompt");
        assert!(store.flush());

        let raw = fs::read_to_string(dir.path().join("history.jsonl")).unwrap();
        assert_eq!(raw.lines().count(), 2);
        // Key order locked to CC's object literal.
        let first = raw.lines().next().unwrap();
        assert!(
            first.starts_with("{\"display\":\"first prompt\",\"pastedContents\":{},\"timestamp\":")
        );
        assert!(first.contains("\"project\":\"/proj/a\""));
        assert!(first.contains("\"sessionId\":\"s-1\""));

        // A fresh store (new session) recalls the project entries newest-first.
        let fresh = store_in(dir.path(), "s-2");
        assert_eq!(
            fresh.recall_displays(),
            vec!["second prompt", "first prompt"]
        );
    }

    #[test]
    fn consecutive_duplicate_is_suppressed() {
        let dir = tempfile::tempdir().unwrap();
        let store = store_in(dir.path(), "s-1");
        store.enqueue("same");
        store.enqueue("same");
        store.enqueue("different");
        store.enqueue("same");
        assert!(store.flush());
        let raw = fs::read_to_string(dir.path().join("history.jsonl")).unwrap();
        assert_eq!(
            raw.lines().count(),
            3,
            "only the CONSECUTIVE dupe is dropped"
        );
    }

    #[test]
    fn read_merge_dedupes_queued_rows_already_on_disk() {
        let dir = tempfile::tempdir().unwrap();
        let store = store_in(dir.path(), "s-1");
        store.enqueue("raced prompt");
        // Simulate the 2.1.218 race: the entry reached disk (another flusher)
        // while still queued in memory.
        {
            let queued = store.read_merged();
            let line = serde_json::to_string(&queued[0]).unwrap();
            fs::write(dir.path().join("history.jsonl"), format!("{line}\n")).unwrap();
        }
        let merged = store.read_merged();
        assert_eq!(
            merged
                .iter()
                .filter(|e| e.display == "raced prompt")
                .count(),
            1,
            "queued + on-disk copy must merge to ONE entry"
        );
    }

    #[test]
    fn failed_flush_keeps_entries_queued() {
        let dir = tempfile::tempdir().unwrap();
        let store = store_in(dir.path(), "s-1");
        store.enqueue("kept");
        // Hold the lock from "another process" (fresh mtime ⇒ not stale).
        let lock_dir = dir.path().join("history.jsonl.lock");
        fs::create_dir(&lock_dir).unwrap();
        assert!(!store.flush(), "lock held ⇒ flush fails");
        fs::remove_dir(&lock_dir).unwrap();
        assert!(store.flush(), "retry after release succeeds");
        let raw = fs::read_to_string(dir.path().join("history.jsonl")).unwrap();
        assert_eq!(raw.lines().count(), 1, "no drop, no duplicate");
    }

    #[test]
    fn stale_lock_is_broken() {
        let dir = tempfile::tempdir().unwrap();
        let store = store_in(dir.path(), "s-1");
        store.enqueue("through the stale lock");
        let lock_dir = dir.path().join("history.jsonl.lock");
        fs::create_dir(&lock_dir).unwrap();
        // Age the lock past the 10s staleness cutoff.
        let old = filetime::FileTime::from_unix_time(
            (SystemTime::now()
                .duration_since(UNIX_EPOCH)
                .unwrap()
                .as_secs()
                - 60) as i64,
            0,
        );
        filetime::set_file_mtime(&lock_dir, old).unwrap();
        assert!(
            store.flush(),
            "stale lock must be broken and the write proceed"
        );
    }

    /// A lock entry the process cannot remove must FAIL the acquisition, not
    /// spin: `remove_dir` reports ENOTDIR for a plain file at the lock path, and
    /// proper-lockfile propagates that (`sIc` → `if (a) return r(a)`).
    #[test]
    fn unremovable_stale_lock_fails_instead_of_spinning() {
        let dir = tempfile::tempdir().unwrap();
        let store = store_in(dir.path(), "s-1");
        store.enqueue("blocked");
        // A stale REGULAR FILE at the lock path: mkdir → EEXIST, stat succeeds
        // (so the staleness test fires), rmdir → ENOTDIR forever.
        let lock_path = dir.path().join("history.jsonl.lock");
        fs::write(&lock_path, b"not a dir").unwrap();
        let old = filetime::FileTime::from_unix_time(
            (SystemTime::now()
                .duration_since(UNIX_EPOCH)
                .unwrap()
                .as_secs()
                - 60) as i64,
            0,
        );
        filetime::set_file_mtime(&lock_path, old).unwrap();

        // Flush off-thread so a regression reports a timeout instead of hanging
        // the whole suite: bounded by the 50/100/200ms budget, a spin never
        // returns at all.
        let store = std::sync::Arc::new(store);
        let (tx, rx) = std::sync::mpsc::channel();
        let flusher = std::sync::Arc::clone(&store);
        std::thread::spawn(move || {
            let _ = tx.send(flusher.flush());
        });
        assert!(
            !rx.recv_timeout(Duration::from_secs(5))
                .expect("acquisition must terminate"),
            "an unbreakable lock must report failure"
        );
        // No drop: the batch is still queued for the next flush.
        fs::remove_file(&lock_path).unwrap();
        assert!(store.flush());
        let raw = fs::read_to_string(dir.path().join("history.jsonl")).unwrap();
        assert_eq!(raw.lines().count(), 1);
    }

    /// `n9i`'s arms, one pass each — every one of them TERMINATES.
    #[test]
    fn acquire_once_matches_n9i_arms() {
        let dir = tempfile::tempdir().unwrap();
        let lock_dir = dir.path().join("history.jsonl.lock");
        let stale = filetime::FileTime::from_unix_time(
            (SystemTime::now()
                .duration_since(UNIX_EPOCH)
                .unwrap()
                .as_secs()
                - 60) as i64,
            0,
        );

        // Free path ⇒ acquired.
        acquire_once(&lock_dir).unwrap();
        // Held, fresh ⇒ ELOCKED, and the holder's lock is left alone.
        assert_eq!(
            acquire_once(&lock_dir).unwrap_err().kind(),
            std::io::ErrorKind::AlreadyExists
        );
        assert!(lock_dir.is_dir());
        // Held, stale ⇒ broken ONCE (`sIc`) and re-taken (`stale: 0` re-entry).
        filetime::set_file_mtime(&lock_dir, stale).unwrap();
        acquire_once(&lock_dir).unwrap();
        assert!(lock_dir.is_dir());
        // `stat` → ENOENT (a dangling symlink at the lock path): re-enter with
        // `stale: 0`, whose mkdir fails EEXIST — bounded, not a staleness loop.
        fs::remove_dir(&lock_dir).unwrap();
        #[cfg(unix)]
        {
            std::os::unix::fs::symlink(dir.path().join("nowhere"), &lock_dir).unwrap();
            assert_eq!(
                acquire_once(&lock_dir).unwrap_err().kind(),
                std::io::ErrorKind::AlreadyExists
            );
        }
    }

    /// `Yt` (the oracle's truthiness helper) is the TRUTHY set, not the
    /// complement of the falsy set: `=2` keeps recording.
    #[test]
    fn skip_env_uses_the_oracle_truthy_set() {
        static SERIAL: Mutex<()> = Mutex::new(());
        let _guard = SERIAL
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        let var = "CLAUDE_CODE_SKIP_PROMPT_HISTORY";
        std::env::remove_var(var);
        assert!(!PromptHistoryStore::disabled_by_env());
        for truthy in ["1", "true", "YES", " on "] {
            std::env::set_var(var, truthy);
            assert!(
                PromptHistoryStore::disabled_by_env(),
                "{truthy:?} is in `Yt`'s truthy set"
            );
        }
        for other in ["0", "false", "no", "off", "", "2", "y", "enabled", "maybe"] {
            std::env::set_var(var, other);
            assert!(
                !PromptHistoryStore::disabled_by_env(),
                "{other:?} is NOT in `Yt`'s truthy set — CC keeps recording"
            );
        }
        std::env::remove_var(var);
    }

    /// `bHo`'s dedupe set is seeded from the QUEUE and never gains a disk key,
    /// so two identical on-disk rows are both yielded.
    #[test]
    fn read_merge_keeps_duplicate_on_disk_rows() {
        let dir = tempfile::tempdir().unwrap();
        let store = store_in(dir.path(), "s-1");
        store.enqueue("retried prompt");
        // The partial-write retry: the batch stayed queued and was re-appended
        // verbatim, so the same row is on disk twice.
        let line = serde_json::to_string(&store.read_merged()[0]).unwrap();
        fs::write(
            dir.path().join("history.jsonl"),
            format!("{line}\n{line}\n"),
        )
        .unwrap();

        let fresh = store_in(dir.path(), "s-2");
        assert_eq!(
            fresh.read_merged().len(),
            2,
            "disk rows are never deduped against each other"
        );
        // …but the queue still suppresses its own copy on disk.
        assert_eq!(
            store
                .read_merged()
                .iter()
                .filter(|e| e.display == "retried prompt")
                .count(),
            1,
            "the queued copy shadows BOTH disk copies of its own key"
        );
    }

    #[test]
    fn read_merge_keeps_a_single_history_row_larger_than_256_kib() {
        let dir = tempfile::tempdir().unwrap();
        let store = store_in(dir.path(), "s-large");
        let display = "x".repeat(300 * 1024);
        store.enqueue(&display);
        assert!(store.flush());

        let fresh = store_in(dir.path(), "s-fresh");
        let merged = fresh.read_merged();
        assert_eq!(merged.len(), 1);
        assert_eq!(merged[0].display, display);
    }

    #[test]
    fn recall_prioritizes_current_session_and_filters_project() {
        let dir = tempfile::tempdir().unwrap();
        // Another project's entry must not appear.
        let other = PromptHistoryStore::new(dir.path(), Path::new("/proj/b"), Some("s-9".into()));
        other.enqueue("other project");
        assert!(other.flush());
        // Older cross-session entry, then a current-session one.
        let past = store_in(dir.path(), "s-old");
        past.enqueue("from old session");
        assert!(past.flush());
        let store = store_in(dir.path(), "s-new");
        store.enqueue("mine");
        // Current-session entries come FIRST even while still queued.
        assert_eq!(store.recall_displays(), vec!["mine", "from old session"]);
        assert_eq!(store.project_prompt_count(), 2);
    }

    #[test]
    fn unparseable_lines_are_skipped() {
        let dir = tempfile::tempdir().unwrap();
        let store = store_in(dir.path(), "s-1");
        store.enqueue("good");
        assert!(store.flush());
        let path = dir.path().join("history.jsonl");
        let mut raw = fs::read_to_string(&path).unwrap();
        raw.push_str("{not json\n");
        fs::write(&path, raw).unwrap();
        let fresh = store_in(dir.path(), "s-2");
        assert_eq!(fresh.recall_displays(), vec!["good"]);
    }

    #[test]
    fn concurrent_writers_neither_drop_nor_duplicate() {
        // The 2.1.218 fix, exercised with two racing threads on one file.
        let dir = tempfile::tempdir().unwrap();
        let dir_path = dir.path().to_path_buf();
        let mut handles = Vec::new();
        for writer in 0..2 {
            let dir_path = dir_path.clone();
            handles.push(std::thread::spawn(move || {
                let store = PromptHistoryStore::new(
                    &dir_path,
                    Path::new("/proj/a"),
                    Some(format!("s-{writer}")),
                );
                for i in 0..25 {
                    store.enqueue(&format!("w{writer} p{i}"));
                    if i % 5 == 0 {
                        let _ = store.flush();
                    }
                }
                assert!(store.flush());
            }));
        }
        for handle in handles {
            handle.join().unwrap();
        }
        let raw = fs::read_to_string(dir_path.join("history.jsonl")).unwrap();
        let mut displays: Vec<String> = raw
            .lines()
            .map(|l| {
                serde_json::from_str::<PromptHistoryEntry>(l)
                    .unwrap()
                    .display
            })
            .collect();
        displays.sort();
        let mut expected: Vec<String> = (0..2)
            .flat_map(|w| (0..25).map(move |i| format!("w{w} p{i}")))
            .collect();
        expected.sort();
        assert_eq!(displays, expected, "every prompt exactly once");
    }
}
