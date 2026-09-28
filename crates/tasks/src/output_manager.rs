//! Sandboxed spool-file manager for task stdout/stderr.
//!
//! See spec §6.6 / D8 — task output is materialized as files under a
//! sandbox directory, with a per-file and total byte budget.

use platform_api::FileSystem;
use std::collections::{HashMap, HashSet};
use std::path::{Path, PathBuf};
use std::sync::Arc;
use thiserror::Error;
use tokio::sync::Mutex;

/// Disk cap for a single task's output file. Mirrors claude-code's
/// `MAX_TASK_OUTPUT_BYTES = 5 * 1024 * 1024 * 1024` — upstream `WUe`
/// (2.1.267 `src_163219561.js`, exported as `MAX_TASK_OUTPUT_BYTES` beside
/// `pKt = "5GB"`). Past it, [`TaskOutputManager::append`] drops further chunks
/// and writes a single truncation marker, matching `DiskTaskOutput.append`.
///
/// 🚨 **This one constant is deliberately spent in TWO different units, and
/// that is upstream's own shape — do not "unify" them.** Upstream measures it
///
/// * against a UTF-16 accumulator on the write side —
///   `append(t){ this.#f += t.length; … }`, `t` being a JS string; and
/// * against real BYTES on the read side — `Gvt(path, WUe)` compares the
///   filesystem `stat` `size` and calls `truncate(WUe)`.
///
/// So [`TaskOutputManager::append`] counting [`utf16_units`] and
/// [`TaskOutputManager::validate_terminal_result`] counting `str::len` are BOTH
/// faithful; they are the port's halves of those two upstream consumers. The
/// name says "BYTES" because upstream's does.
///
/// ## TL-6: the 1 GiB tool-result cap belongs to a different file, and a
/// surface this port has no consumer for
///
/// Verified at the 2.1.270 oracle 2026-09-14. The backlog asks for the missing
/// "1 GB tool-result disk cap", which is `E2`'s `fAr = 1073741824` default —
/// but `E2` is a GENERIC "persist a tool result to disk" helper returning
/// `{filepath, originalSize, isJson, preview, hasMore, truncatedAtBytes}`, and
/// it is NOT this file. The caps here are the background-task spool
/// (`diskOutput.ts`), which is a different upstream file with different
/// numbers, so adding `fAr` alongside them would put an unrelated constant in
/// the one place it does not belong.
///
/// Its real consumers upstream are `persistedToolResultFiles` — the Artifact
/// tool saving published HTML and raw attachment bytes, and WebFetch saving a
/// raw body it cannot render. Neither exists here: there is no `Artifact` tool
/// in the registry, and this port's WebFetch caps the transfer at
/// `WEBFETCH_MAX_TRANSFER_BYTES` (10 MB) and TRUNCATES rather than persisting.
///
/// ⇒ The cap guards a surface with no consumer, and the surface's consumers
/// are themselves unbuilt or deliberately divergent. Build a consumer first, or
/// the constant is dead the day it lands.
///
/// The consequence is real and inherited: a spool of CJK text passes 5 GB on
/// disk at roughly 1.7 G code units, so the write-side marker fires late (an
/// emoji-heavy spool later still). That is upstream behaviour, not a port bug.
pub const MAX_TASK_OUTPUT_BYTES: u64 = 5 * 1024 * 1024 * 1024;

/// The cap's unit: JS `String.length`, i.e. UTF-16 code units. A CJK character
/// is 3 UTF-8 bytes but 1 unit, and an emoji is 4 bytes but 2 — counting bytes
/// tripped the cap early for any non-ASCII spool.
fn utf16_units(content: &str) -> u64 {
    content.chars().map(|c| c.len_utf16() as u64).sum()
}

fn complete_utf8_prefix(bytes: &[u8]) -> usize {
    match std::str::from_utf8(bytes) {
        Err(error) if error.error_len().is_none() => error.valid_up_to(),
        _ => bytes.len(),
    }
}

/// Display string for [`MAX_TASK_OUTPUT_BYTES`] used in the truncation marker
/// (claude-code `MAX_TASK_OUTPUT_BYTES_DISPLAY = '5GB'`, `diskOutput.ts:31`).
pub const MAX_TASK_OUTPUT_BYTES_DISPLAY: &str = "5GB";

/// claude-code `wt` — the one thing a failed write still tries to get onto disk,
/// so a reader can tell truncation from silence. Leading AND trailing newline.
const OUTPUT_OMITTED_MARKER: &str = "\n[output omitted: it could not be written to disk]\n";

/// claude-code `Bt = 8388608` — `getTaskOutput` never returns more than the
/// LAST 8 MiB of a spool, however large the file is
/// (`Rbt(t, e = Bt)` → `k_(handle, e)`).
///
/// Distinct from [`MAX_TASK_OUTPUT_BYTES`], which is the 5GB WRITE cap: that
/// one stops the spool growing, this one stops a full read from materialising
/// gigabytes into memory on the way to a caller that will truncate it to tens
/// of thousands of characters anyway.
pub const MAX_TASK_OUTPUT_READ_BYTES: u64 = 8 * 1024 * 1024;

/// Keep at most the last [`MAX_TASK_OUTPUT_READ_BYTES`] and announce what was
/// dropped, claude-code `Rbt`:
///
/// ```js
/// let{content:r,bytesTotal:s,bytesRead:c}=o;
/// if(s>c)return`[${Math.round((s-c)/1024)}KB of earlier output omitted]\n${r}`;
/// return r
/// ```
///
/// The oracle seeks, so its `bytesRead` is exact; this trims an
/// already-materialised string, so the cut is snapped FORWARD to a UTF-8
/// boundary and the omitted count is recomputed from what actually survived —
/// the header must describe the real cut, not the requested one.
fn apply_read_tail_cap(content: String) -> String {
    let total = content.len() as u64;
    if total <= MAX_TASK_OUTPUT_READ_BYTES {
        return content;
    }
    let mut cut = (total - MAX_TASK_OUTPUT_READ_BYTES) as usize;
    while cut < content.len() && !content.is_char_boundary(cut) {
        cut += 1;
    }
    let omitted = cut as f64 / 1024.0;
    format!(
        "[{}KB of earlier output omitted]\n{}",
        omitted.round() as u64,
        &content[cut..]
    )
}

/// Owner of the task-output sandbox directory.
pub struct TaskOutputManager {
    output_dir: PathBuf,
    linked_transcripts: Mutex<HashMap<PathBuf, (PathBuf, std::fs::File)>>,
    fs: Arc<dyn FileSystem>,
    /// Identity of the output directory observed by this manager. The
    /// platform rooted operations still perform handle-relative I/O; this
    /// lightweight pin catches a directory that was moved and replaced
    /// between calls before a new rooted handle is opened.
    root_pin: Mutex<OutputRootState>,
    /// Per-spool bytes-written counter + capped flag, keyed by spool path.
    /// Backs the write-side 5GB disk cap ([`MAX_TASK_OUTPUT_BYTES`]): once a
    /// spool crosses the cap its entry is marked capped and further appends are
    /// dropped (after a single truncation marker is written), mirroring
    /// claude-code's `DiskTaskOutput.#bytesWritten` / `#capped`.
    caps: Mutex<HashMap<PathBuf, CapState>>,
    /// Per-spool serialization for append/terminal replacement operations. A
    /// terminal Local App result must replace its raw payload as one
    /// indivisible write; reads also take this lock so a fail-closed terminal
    /// override cannot race a stale filesystem read. Unrelated task spools
    /// remain independent.
    writers: Mutex<HashMap<PathBuf, Arc<OutputWriter>>>,
    /// Spools whose terminal payload has been replaced. Further handler
    /// appends are ignored so a late raw chunk cannot follow the canonical
    /// result back into the externally readable spool.
    terminal: Mutex<HashSet<PathBuf>>,
    supervised_terminals: Mutex<HashSet<PathBuf>>,
    /// Authoritative terminal payloads. These are recorded before attempting
    /// the best-effort on-disk rewrite, so a failed rewrite cannot leave stale
    /// bytes readable through this manager or advertise them as canonical.
    terminal_overrides: Mutex<HashMap<PathBuf, TerminalOverride>>,
}

const MAX_UNWRITTEN_CHARS: u64 = 16 * 1024 * 1024;

#[derive(Default)]
struct OutputWriter {
    serial: Arc<Mutex<()>>,
    queue: std::sync::Mutex<WriterQueue>,
}

#[derive(Default)]
struct WriterQueue {
    chunks: std::collections::VecDeque<String>,
    chars: u64,
    generation: u64,
    last_error: Option<OutputError>,
    in_flight: bool,
    retired: bool,
    lost_output: bool,
}

impl OutputWriter {
    fn queue(&self) -> std::sync::MutexGuard<'_, WriterQueue> {
        self.queue
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
    }
}

/// Cancellation cannot replay an uncertain partially-written batch. Preserve a
/// loss marker and release this generation so concurrent appenders never hang.
struct DrainGuard {
    writer: Arc<OutputWriter>,
    finished: bool,
}
impl Drop for DrainGuard {
    fn drop(&mut self) {
        if self.finished {
            return;
        }
        let mut state = self.writer.queue();
        if state.in_flight {
            state.chunks.push_front(OUTPUT_OMITTED_MARKER.into());
            state.chars += utf16_units(OUTPUT_OMITTED_MARKER);
            state.lost_output = true;
            state.in_flight = false;
        }
        state.generation = state.generation.wrapping_add(1);
        state.last_error = Some(OutputError::Io("task-output drain cancelled".into()));
    }
}

#[derive(Debug, Clone)]
struct TerminalOverride {
    content: String,
    physical_spool_authoritative: bool,
}

/// Per-spool write-side cap state (claude-code `DiskTaskOutput` `#bytesWritten`
/// + `#capped`).
#[derive(Debug, Default, Clone, Copy)]
struct CapState {
    /// Running size in UTF-16 CODE UNITS — the unit claude-code's
    /// `DiskTaskOutput.append` accumulates (`this.#f += t.length` over JS
    /// strings), not bytes on disk.
    /// Running total for the [`MAX_TASK_OUTPUT_BYTES`] write-side cap, in
    /// UTF-16 CODE UNITS (upstream's `this.#f += t.length`) — NOT bytes,
    /// despite the name it inherits from the constant. See [`utf16_units`].
    bytes_written: u64,
    capped: bool,
    /// claude-code `lostOutput` — a write for this spool has failed at least
    /// once, so its contents are known-incomplete. See
    /// [`TaskOutputManager::lost_output`]; set-once, never cleared.
    lost_output: bool,
}

/// Errors produced by [`TaskOutputManager`].
#[derive(Debug, Clone, Error)]
pub enum OutputError {
    /// I/O failure forwarded from the [`FileSystem`] trait.
    #[error("io: {0}")]
    Io(String),
    /// Refusal to write to a path that escapes the sandbox root.
    #[error("path escapes output dir: {0}")]
    PathEscape(String),
    /// A spool file already exists for this id — the exclusive (`O_EXCL`)
    /// create refused to clobber it. Guards the double-allocate truncate race:
    /// a second [`TaskOutputManager::allocate`] of the same id errors here
    /// instead of silently truncating output a worker already appended.
    #[error("spool already allocated: {0}")]
    AlreadyExists(String),
    /// The output directory no longer names the directory owned by this
    /// manager, or an unsafe symlink was introduced. The recovery guidance is
    /// intentionally explicit about removing the link itself, never its target.
    #[error("{0}")]
    SwapRefused(String),
}

#[derive(Debug, Default)]
struct OutputRootState {
    initialized: bool,
    identity: Option<platform_api::rooted_fs::RootIdentity>,
}

/// Read options for [`TaskOutputManager::read`].
#[derive(Debug, Clone, Default)]
pub struct OutputOptions {
    /// Number of LINES to skip from the start.
    ///
    /// These two said "bytes" for as long as they have existed, while `read`
    /// has always forwarded them to `platform_api::apply_line_window`, whose
    /// own doc says "line-indexed offset/limit window". Nothing in the tree
    /// constructs either as `Some`, so no caller was ever wrong — but one
    /// following the old contract (resume a poll at the byte count already
    /// consumed) would have skipped that many LINES and read an empty tail
    /// while `truncated` reported the read complete.
    ///
    /// Upstream's disk read is a different API and is genuinely byte-based:
    /// `gTn(path, n)` takes a tail length in bytes and walks UTF-8
    /// continuation bytes off the front so the window cannot split a
    /// codepoint. It is not what this struct drives.
    pub offset: Option<u64>,
    /// Maximum number of LINES to return. See [`Self::offset`].
    pub limit: Option<u64>,
}

/// Output payload returned by [`TaskOutputManager::read`].
#[derive(Debug, Clone)]
pub struct TaskOutput {
    /// Raw text content (already trimmed by `limit`).
    pub content: String,
    /// Total line count of the authoritative output projection.
    pub total_lines: u64,
    /// True if `content` is a prefix of the authoritative output projection.
    pub truncated: bool,
    /// Whether the physical spool contains the same authoritative bytes.
    pub physical_spool_authoritative: bool,
}

impl TaskOutputManager {
    /// N7e/Ibt returns the original stat size even after truncating the file.
    pub async fn finalize_persisted_output(
        &self,
        path: &Path,
        max_bytes: u64,
    ) -> Result<u64, OutputError> {
        self.flush_writer(path).await?;
        let relative = self.relative_path_for(path)?;
        let lock = self.write_lock_for(path).await;
        let _guard = lock.lock().await;
        let identity = self.check_output_root().await?;
        if let Some(identity) = identity {
            // Truncate the same no-follow inode we stat, never a second path
            // lookup that could follow a swapped output into another file.
            let file = platform_api::rooted_fs::open_append_file_pinned(
                &self.output_dir,
                &relative,
                Some(&identity),
            )
            .map_err(|error| OutputError::Io(error.to_string()))?;
            let size = file
                .metadata()
                .map_err(|error| OutputError::Io(error.to_string()))?
                .len();
            if size > max_bytes {
                file.set_len(max_bytes)
                    .map_err(|error| OutputError::Io(error.to_string()))?;
            }
            return Ok(size);
        }
        // Virtual filesystems without native inode identities implement their
        // own path policy; preserve the injected filesystem seam for them.
        let size = self
            .fs
            .file_size(&path.to_string_lossy())
            .await
            .map_err(|error| OutputError::Io(error.to_string()))?;
        if size > max_bytes {
            self.fs
                .truncate(&path.to_string_lossy(), max_bytes)
                .await
                .map_err(|error| OutputError::Io(error.to_string()))?;
        }
        Ok(size)
    }

    /// Construct a manager rooted at `output_dir`.
    #[must_use]
    pub fn new(output_dir: PathBuf, fs: Arc<dyn FileSystem>) -> Self {
        Self {
            output_dir,
            linked_transcripts: Mutex::new(HashMap::new()),
            fs,
            root_pin: Mutex::new(OutputRootState::default()),
            caps: Mutex::new(HashMap::new()),
            writers: Mutex::new(HashMap::new()),
            terminal: Mutex::new(HashSet::new()),
            supervised_terminals: Mutex::new(HashSet::new()),
            terminal_overrides: Mutex::new(HashMap::new()),
        }
    }

    /// Atomically install/recover an authenticated adopted shell's output link.
    /// No allocated empty-file stage exists, so a crash is safely retryable.
    #[cfg(any(unix, windows))]
    pub async fn adopt_output(&self, output_file: &Path, target: &Path) -> Result<(), OutputError> {
        if !target.is_absolute() {
            return Err(OutputError::PathEscape(target.display().to_string()));
        }
        let relative = self.relative_path_for(output_file)?;
        let lock = self.write_lock_for(output_file).await;
        let _guard = lock.lock().await;
        if let Some((prior, _)) = self.linked_transcripts.lock().await.get(output_file) {
            if prior != target {
                return Err(OutputError::PathEscape(target.display().to_string()));
            }
            #[cfg(unix)]
            return Ok(());
        }
        let root = self.check_output_root().await?;
        let file = platform_api::rooted_fs::adopt_task_output_link(
            &self.output_dir,
            &relative,
            root.as_ref(),
            target,
        )
        .map_err(|error| OutputError::Io(error.to_string()))?;
        self.linked_transcripts
            .lock()
            .await
            .insert(output_file.to_owned(), (target.to_owned(), file));
        Ok(())
    }

    #[cfg(not(any(unix, windows)))]
    pub async fn adopt_output(
        &self,
        _output_file: &Path,
        _target: &Path,
    ) -> Result<(), OutputError> {
        Err(OutputError::Io(
            "adopted shell output links unsupported on this platform".into(),
        ))
    }

    /// Register a transcript-backed output. Only the trusted spawner supplies
    /// targets; arbitrary symlinks remain refused by normal spool reads.
    #[cfg(any(unix, windows))]
    pub async fn link_transcript(
        &self,
        output_file: &Path,
        target: &Path,
    ) -> Result<(), OutputError> {
        if !target.is_absolute() {
            return Err(OutputError::PathEscape(target.display().to_string()));
        }
        let relative = self.relative_path_for(output_file)?;
        let lock = self.write_lock_for(output_file).await;
        let _guard = lock.lock().await;
        #[cfg(unix)]
        if self
            .linked_transcripts
            .lock()
            .await
            .contains_key(output_file)
        {
            return Ok(());
        }
        let root = self.check_output_root().await?;
        let file = platform_api::rooted_fs::link_task_transcript(
            &self.output_dir,
            &relative,
            root.as_ref(),
            target,
        )
        .map_err(|e| OutputError::Io(e.to_string()))?;
        self.linked_transcripts
            .lock()
            .await
            .insert(output_file.to_owned(), (target.to_owned(), file));
        Ok(())
    }

    /// Hosts without a native link backend retain their existing spool.
    #[cfg(not(any(unix, windows)))]
    pub async fn link_transcript(
        &self,
        _output_file: &Path,
        _target: &Path,
    ) -> Result<(), OutputError> {
        Err(OutputError::Io(
            "transcript links unsupported on this host".into(),
        ))
    }

    /// `BSn`: offsets count bytes and advance only when bytes were read.
    pub async fn read_delta(
        &self,
        output_file: &Path,
        offset: u64,
    ) -> Result<(String, u64), OutputError> {
        let relative = self.relative_path_for(output_file)?;
        let identity = self.check_output_root().await?;
        #[cfg(any(unix, windows))]
        {
            use std::io::{Read, Seek, SeekFrom};
            #[cfg(unix)]
            use std::os::unix::fs::MetadataExt;
            let mut links = self.linked_transcripts.lock().await;
            if let Some((target, file)) = links.get_mut(output_file) {
                let pinned = file
                    .metadata()
                    .map_err(|e| OutputError::Io(e.to_string()))?;
                #[cfg(unix)]
                {
                    let advertised = std::fs::read_link(output_file)
                        .map_err(|e| OutputError::Io(e.to_string()))?;
                    let current = std::fs::symlink_metadata(&*target)
                        .map_err(|e| OutputError::Io(e.to_string()))?;
                    if advertised != *target
                        || !current.is_file()
                        || current.ino() != pinned.ino()
                        || current.dev() != pinned.dev()
                    {
                        return Err(OutputError::SwapRefused(
                            "task output link identity changed".into(),
                        ));
                    }
                }
                #[cfg(windows)]
                {
                    let pin = self.check_output_root().await?;
                    platform_api::rooted_fs::validate_task_output_link(
                        &self.output_dir,
                        &relative,
                        pin.as_ref(),
                        target,
                        file,
                    )
                    .map_err(|error| OutputError::SwapRefused(error.to_string()))?;
                }
                file.seek(SeekFrom::Start(offset))
                    .map_err(|e| OutputError::Io(e.to_string()))?;
                let mut bytes = Vec::new();
                file.take(MAX_TASK_OUTPUT_READ_BYTES)
                    .read_to_end(&mut bytes)
                    .map_err(|e| OutputError::Io(e.to_string()))?;
                let complete = complete_utf8_prefix(&bytes);
                return Ok((
                    String::from_utf8_lossy(&bytes[..complete]).into_owned(),
                    offset.saturating_add(complete as u64),
                ));
            }
        }
        let bytes = self
            .fs
            .read_file_rooted_byte_window_pinned(
                &self.output_dir,
                &relative,
                identity.as_ref(),
                offset,
                MAX_TASK_OUTPUT_READ_BYTES,
            )
            .await
            .map_err(|e| OutputError::Io(e.to_string()))?;
        let complete = complete_utf8_prefix(&bytes);
        let end = offset.saturating_add(complete as u64);
        Ok((
            String::from_utf8_lossy(&bytes[..complete]).into_owned(),
            end,
        ))
    }

    /// A separate supervisor owns this shell's terminal trailer, including
    /// stops initiated before its task row has been registered.
    pub(crate) async fn mark_supervised_terminal(&self, output: &Path) {
        self.supervised_terminals
            .lock()
            .await
            .insert(output.to_path_buf());
    }

    pub(crate) async fn shell_terminal_is_supervised(&self, output: &Path) -> bool {
        self.supervised_terminals.lock().await.contains(output)
    }

    pub(crate) async fn append_shell_terminal(&self, output: &Path, trailer: &str) {
        if !self.supervised_terminals.lock().await.contains(output) {
            let _ = self.append(output, trailer).await;
        }
    }

    /// Release in-memory output state after the registry has removed the row.
    /// Keep the on-disk transcript link available to explicit historical reads.
    pub async fn release_output_state(&self, output_file: &Path) {
        self.evict_writer(output_file).await;
        self.linked_transcripts.lock().await.remove(output_file);
        self.terminal.lock().await.remove(output_file);
        self.terminal_overrides.lock().await.remove(output_file);
        self.supervised_terminals.lock().await.remove(output_file);
    }

    /// The absolute spool directory this manager owns.
    #[must_use]
    pub fn output_dir(&self) -> &Path {
        &self.output_dir
    }

    /// Return the spool path for `task_id` WITHOUT creating the file. Refuses
    /// any `..` or absolute leak (D8).
    ///
    /// Use this to recover the path a handler already allocated (the spool path
    /// is a deterministic function of the id), so the registry never calls
    /// [`allocate`](Self::allocate) a second time for the same id — the
    /// double-allocate truncate race fix.
    pub fn path_for(&self, task_id: &str) -> Result<PathBuf, OutputError> {
        // Extension `.output` byte-aligns with claude-code's
        // `getTaskOutputPath` (`diskOutput.ts:72-74` → `${taskId}.output`).
        let filename = platform_api::task_output::output_filename(task_id);
        let relative = Path::new(&filename);
        platform_api::rooted_fs::validate_relative_path(relative)
            .map_err(|_| OutputError::PathEscape(filename.clone()))?;
        if relative.components().count() != 1 {
            return Err(OutputError::PathEscape(filename));
        }
        Ok(self.output_dir.join(relative))
    }

    fn relative_path_for(&self, output_file: &Path) -> Result<PathBuf, OutputError> {
        let relative = output_file
            .strip_prefix(&self.output_dir)
            .map_err(|_| OutputError::PathEscape(output_file.display().to_string()))?
            .to_path_buf();
        platform_api::rooted_fs::validate_relative_path(&relative)
            .map_err(|_| OutputError::PathEscape(relative.display().to_string()))?;
        Ok(relative)
    }

    /// claude-code `b(path, reason, recovery?)`:
    ///
    /// ```js
    /// let o=`task output swap refused (${e}): ${t}`+(i===void 0?"":`. To recover: ${i}.`)
    /// ```
    ///
    /// The recovery clause is OPTIONAL, and the oracle attaches it to exactly
    /// ONE of its eight refusals — `tasks dir moved or linked`, the only one a
    /// user can act on. The port appended it to every reason, so a refusal that
    /// says a single output FILE changed identity underneath us told the user to
    /// restart with a fresh temp directory, which does not address that at all.
    fn swap_refused(&self, reason: &str) -> OutputError {
        OutputError::SwapRefused(format!(
            "task output swap refused ({reason}): {}",
            self.output_dir.display(),
        ))
    }

    /// The one refusal that carries the oracle's recovery clause. `LINGXI_TMPDIR`
    /// is the port's spelling of `CLAUDE_CODE_TMPDIR`; naming it is the point of
    /// the sentence — "its temporary-directory setting" left the reader with
    /// nothing to set. The oracle strips trailing separators from the temp root
    /// before interpolating it.
    fn swap_refused_dir_moved(&self) -> OutputError {
        let root = self.output_dir.display().to_string();
        let root = root.trim_end_matches(['/', '\\']);
        OutputError::SwapRefused(format!(
            "task output swap refused (tasks dir moved or linked): {root}. To recover: restart with LINGXI_TMPDIR set to a fresh directory; or, if {root} is a stray directory or a symbolic link that should not be there, remove that entry itself (not what it points to) and restart.",
        ))
    }

    async fn check_output_root(
        &self,
    ) -> Result<Option<platform_api::rooted_fs::RootIdentity>, OutputError> {
        let mut state = self.root_pin.lock().await;
        let current = match self.fs.root_identity_no_follow(&self.output_dir).await {
            Ok(identity) => identity,
            Err(error) if state.initialized => {
                // The pin no longer resolves — the directory was replaced by a
                // symlink or removed under us. Same actionable refusal as an
                // identity CHANGE, and the same one the oracle raises when its
                // own pin is refused; it logs the cause separately rather than
                // folding it into the message, which is why the errno-carrying
                // `lstat refused a swapped path (${a})` family is per-FILE.
                tracing::warn!(
                    "task output: pin of {} refused: {error}",
                    self.output_dir.display()
                );
                return Err(self.swap_refused_dir_moved());
            }
            Err(error) => return Err(self.map_rooted_error(error)),
        };
        if state.initialized {
            if state.identity != current {
                // The one actionable refusal — the tasks directory itself was
                // replaced. This is the oracle's `tasks dir moved or linked`.
                return Err(self.swap_refused_dir_moved());
            }
        } else {
            state.initialized = true;
            state.identity = current;
        }
        Ok(state.identity)
    }

    fn map_rooted_error(&self, error: platform_api::FsError) -> OutputError {
        match error {
            platform_api::FsError::OutsideWorkspace(_) => {
                // A LingXi-specific check (the port validates the relative path
                // before opening, where the oracle relies on `O_NOFOLLOW`), so
                // the reason stays truthful to what was refused rather than
                // borrowing an oracle string that describes an open() failure.
                self.swap_refused("a parent or final path component is unsafe")
            }
            other => OutputError::Io(other.to_string()),
        }
    }

    async fn writer_for(&self, output_file: &Path) -> Arc<OutputWriter> {
        self.writers
            .lock()
            .await
            .entry(output_file.to_path_buf())
            .or_insert_with(|| Arc::new(OutputWriter::default()))
            .clone()
    }

    async fn write_lock_for(&self, output_file: &Path) -> Arc<Mutex<()>> {
        self.writer_for(output_file).await.serial.clone()
    }

    #[cfg(test)]
    pub(crate) async fn has_writer_for_test(&self, output_file: &Path) -> bool {
        self.writers.lock().await.contains_key(output_file)
    }

    /// Wait for a currently accepted generation without reopening a failed one.
    pub async fn flush_writer(&self, output_file: &Path) -> Result<(), OutputError> {
        let Some(writer) = self.writers.lock().await.get(output_file).cloned() else {
            return Ok(());
        };
        let _serial = writer.serial.lock().await;
        let error = writer.queue().last_error.clone();
        error.map_or(Ok(()), Err)
    }

    /// Finish the current generation and evict its writer, without restarting
    /// an exhausted retry. The spool remains readable (`Sd` keeps its file).
    pub async fn evict_writer(&self, output_file: &Path) {
        let Some(writer) = self.writers.lock().await.get(output_file).cloned() else {
            return;
        };
        let _serial = writer.serial.lock().await;
        {
            let mut queue = writer.queue();
            if queue.last_error.is_some() && queue.chars > 0 {
                tracing::error!(
                    unwritten_chars = queue.chars,
                    "Task output writer evicted while failing; discarded unwritten output"
                );
            }
            queue.retired = true;
            queue.chunks.clear();
            queue.chars = 0;
        }
        let mut writers = self.writers.lock().await;
        if writers
            .get(output_file)
            .is_some_and(|current| Arc::ptr_eq(current, &writer))
        {
            writers.remove(output_file);
        }
        self.caps.lock().await.remove(output_file);
    }

    /// Allocate a fresh spool file inside `output_dir`. Refuse any `..` or
    /// absolute leak (D8).
    ///
    /// The file is created with `O_CREAT | O_EXCL | O_NOFOLLOW`
    /// ([`FileSystem::create_new_file`]), byte-aligned with claude-code's
    /// `initTaskOutput`:
    /// - Exclusive create makes allocation non-destructive — a second
    ///   `allocate` for the same id returns [`OutputError::AlreadyExists`]
    ///   rather than truncating output a worker already appended (T4).
    /// - `O_NOFOLLOW` refuses a pre-planted symlink at the spool path, closing
    ///   the symlink-follow write vector (T18).
    pub async fn allocate(&self, task_id: &str) -> Result<PathBuf, OutputError> {
        let path = self.path_for(task_id)?;
        let relative = self.relative_path_for(&path)?;
        let root_identity = self.check_output_root().await?;
        self.fs
            .create_new_file_rooted_no_follow_pinned(
                &self.output_dir,
                &relative,
                root_identity.as_ref(),
            )
            .await
            .map_err(|e| match e {
                platform_api::FsError::AlreadyExists(p) => OutputError::AlreadyExists(p),
                other => self.map_rooted_error(other),
            })?;
        Ok(path)
    }

    /// Delete a spool that turned out to be unused.
    ///
    /// claude-code deletes a shell's task-output file when it is redundant —
    /// the command finished in the foreground and its output was returned
    /// inline, so nothing will ever read the file (`deleteOutputFile`, guarded
    /// by `outputFileRedundant`). Without this, minting one output file per
    /// Bash call would leave a file behind for every command in the session.
    ///
    /// Best-effort and idempotent: a spool that is already gone is not an error.
    ///
    /// # Errors
    /// Returns [`OutputError::PathEscape`] when the path is not inside this
    /// manager's output directory.
    pub async fn discard(&self, output_file: &Path) -> Result<(), OutputError> {
        let relative = self.relative_path_for(output_file)?;
        let write_lock = self.write_lock_for(output_file).await;
        let _writes = write_lock.lock().await;
        self.linked_transcripts.lock().await.remove(output_file);
        if let Some(writer) = self.writers.lock().await.remove(output_file) {
            let mut queue = writer.queue();
            queue.retired = true;
            queue.chunks.clear();
            queue.chars = 0;
        }
        self.caps.lock().await.remove(output_file);
        self.terminal.lock().await.remove(output_file);
        self.terminal_overrides.lock().await.remove(output_file);
        self.supervised_terminals.lock().await.remove(output_file);
        match self
            .fs
            .delete_file_rooted_no_follow(&self.output_dir, &relative)
            .await
        {
            Ok(()) | Err(platform_api::FsError::NotFound(_)) => Ok(()),
            Err(other) => Err(self.map_rooted_error(other)),
        }
    }

    /// Append a chunk to a task's spool, enforcing the per-file 5GB disk cap
    /// ([`MAX_TASK_OUTPUT_BYTES`]) on the WRITE side.
    ///
    /// Aligned with claude-code's `DiskTaskOutput.append`: the running count is
    /// `this.#f += t.length` where `t` is a JS STRING, so the unit is UTF-16
    /// CODE UNITS, not bytes — upstream re-measures real bytes only when it
    /// builds the Buffer to write. Counting UTF-8 bytes here tripped the cap
    /// early for any non-ASCII spool (a CJK character is 3 bytes but 1 unit).
    /// Once the count would cross the cap the spool is marked capped —
    /// a single truncation marker
    /// `\n[output truncated: exceeded 5GB disk cap]\n` is written and all
    /// subsequent appends are dropped. The write itself uses
    /// [`FileSystem::append_file_no_follow`] so a symlink planted at the spool
    /// path from inside the sandbox cannot redirect it (T18).
    pub async fn append(&self, output_file: &Path, content: &str) -> Result<(), OutputError> {
        let relative = self.relative_path_for(output_file)?;
        if self
            .linked_transcripts
            .lock()
            .await
            .contains_key(output_file)
            || self.terminal.lock().await.contains(output_file)
        {
            return Ok(());
        }
        let writer = self.writer_for(output_file).await;
        let generation = {
            let mut caps = self.caps.lock().await;
            let mut queue = writer.queue();
            if queue.retired {
                return Ok(());
            }
            let state = caps.entry(output_file.to_path_buf()).or_default();
            if state.capped {
                return Ok(());
            }
            state.bytes_written = state.bytes_written.saturating_add(utf16_units(content));
            let to_write = if state.bytes_written > MAX_TASK_OUTPUT_BYTES {
                state.capped = true;
                format!("\n[output truncated: exceeded {MAX_TASK_OUTPUT_BYTES_DISPLAY} disk cap]\n")
            } else {
                content.to_string()
            };
            queue.chars += utf16_units(&to_write);
            queue.chunks.push_back(to_write);
            queue.generation
        };
        // Enqueue BEFORE waiting: concurrent producers can accumulate real
        // unwritten output while an open/write future is pending.
        let _serial = writer.serial.lock().await;
        let ignored = self.terminal.lock().await.contains(output_file)
            || self
                .linked_transcripts
                .lock()
                .await
                .contains_key(output_file);
        {
            let mut queue = writer.queue();
            if queue.retired {
                return Ok(());
            }
            if queue.generation != generation {
                return queue.last_error.clone().map_or(Ok(()), Err);
            }
            if ignored {
                queue.chunks.clear();
                queue.chars = 0;
                return Ok(());
            }
        }
        let mut guard = DrainGuard {
            writer: writer.clone(),
            finished: false,
        };
        let mut failures = 0;
        loop {
            let (body, chars) = {
                let mut queue = writer.queue();
                if queue.chunks.is_empty() {
                    queue.generation = queue.generation.wrapping_add(1);
                    queue.last_error = None;
                    guard.finished = true;
                    return Ok(());
                }
                let body = queue.chunks.drain(..).collect::<Vec<_>>().concat();
                let chars = std::mem::take(&mut queue.chars);
                queue.in_flight = true;
                (body, chars)
            };
            let result = match self.check_output_root().await {
                Ok(identity) => {
                    self.fs
                        .append_file_rooted_staged(
                            &self.output_dir,
                            &relative,
                            &body,
                            identity.as_ref(),
                        )
                        .await
                }
                Err(error) => {
                    let mut queue = writer.queue();
                    queue.in_flight = false;
                    queue.chunks.push_front(body);
                    queue.chars += chars;
                    queue.last_error = Some(error.clone());
                    queue.generation = queue.generation.wrapping_add(1);
                    guard.finished = true;
                    return Err(error);
                }
            };
            match result {
                Ok(()) => {
                    writer.queue().in_flight = false;
                }
                Err(failure) => {
                    let error = self.map_rooted_error(failure.error);
                    let is_write =
                        failure.stage == platform_api::filesystem::FileAppendStage::Write;
                    {
                        let mut queue = writer.queue();
                        queue.in_flight = false;
                        if is_write {
                            queue.lost_output = true;
                            queue.chunks.push_front(OUTPUT_OMITTED_MARKER.into());
                            queue.chars += utf16_units(OUTPUT_OMITTED_MARKER);
                        } else {
                            // Opening consumed no bytes; restore this batch
                            // ahead of appends queued while the open awaited.
                            queue.chunks.push_front(body);
                            queue.chars += chars;
                        }
                    }
                    if is_write {
                        self.caps
                            .lock()
                            .await
                            .entry(output_file.to_path_buf())
                            .or_default()
                            .lost_output = true;
                    }
                    failures += 1;
                    if failures == 1 {
                        tracing::error!("Task output drain failed (will retry once): {error}");
                        continue;
                    }
                    let mut queue = writer.queue();
                    if queue.chars > MAX_UNWRITTEN_CHARS {
                        tracing::error!(
                            unwritten_chars = queue.chars,
                            "Task output still cannot be written; dropped unwritten output"
                        );
                        queue.chunks.clear();
                        queue.chunks.push_back(OUTPUT_OMITTED_MARKER.into());
                        queue.chars = utf16_units(OUTPUT_OMITTED_MARKER);
                        queue.lost_output = true;
                    }
                    queue.last_error = Some(error.clone());
                    queue.generation = queue.generation.wrapping_add(1);
                    guard.finished = true;
                    return Err(error);
                }
            }
        }
    }

    /// Whether this spool has ever failed a write — claude-code `lostOutput`.
    ///
    /// SET-ONCE: the oracle stamps it in the drain's inner catch and never
    /// clears it, so a spool that lost a chunk keeps saying so even after later
    /// writes succeed. Its reader upstream is the `TaskOutput` footer, which
    /// swaps "Full output saved to: {path}" for a sentence saying the file may
    /// be incomplete — a surface this port does not have yet, which is why this
    /// accessor currently has no production caller.
    pub async fn lost_output(&self, output_file: &Path) -> bool {
        if self
            .writers
            .lock()
            .await
            .get(output_file)
            .is_some_and(|writer| writer.queue().lost_output)
        {
            return true;
        }
        self.caps
            .lock()
            .await
            .get(output_file)
            .is_some_and(|state| state.lost_output)
    }

    /// Replace a terminal task payload in its already-allocated spool.
    ///
    /// This is deliberately narrower than the generic task-output API: the
    /// mobile Local App completion sink calls it only after authenticating a
    /// Host QA result (or constructing a fail-closed terminal error). The
    /// path is validated against this manager's output root, the root identity
    /// is pinned before and after the rooted atomic write, and replacement is
    /// serialized with appends so raw handler output cannot win a race with
    /// the checked terminal payload. Once a fail-closed failure override has
    /// been installed, this method refuses a late success replacement.
    pub async fn replace_terminal_result(
        &self,
        output_file: &Path,
        content: &str,
    ) -> Result<(), OutputError> {
        let relative = self.relative_path_for(output_file)?;
        let write_lock = self.write_lock_for(output_file).await;
        let _writes = write_lock.lock().await;
        let _root_identity = self.check_output_root().await?;
        if self
            .terminal_overrides
            .lock()
            .await
            .contains_key(output_file)
        {
            return Err(OutputError::Io(
                "terminal override is already authoritative".into(),
            ));
        }
        self.validate_terminal_result(output_file, content)?;
        self.fs
            .write_file_rooted_atomic(&self.output_dir, &relative, content)
            .await
            .map_err(|e| self.map_rooted_error(e))?;
        // The rooted atomic write is platform-confined; this second identity
        // check also rejects a directory swap that raced the operation before
        // the next append/read can proceed.
        self.check_output_root().await?;
        self.terminal.lock().await.insert(output_file.to_path_buf());
        let mut caps = self.caps.lock().await;
        caps.insert(
            output_file.to_path_buf(),
            CapState {
                // Same unit as `append`: UTF-16 code units.
                bytes_written: utf16_units(content),
                capped: false,
                // A terminal replacement is a fresh, complete payload.
                lost_output: false,
            },
        );
        Ok(())
    }

    /// Refuse a terminal payload that would not fit the spool.
    ///
    /// Counts real BYTES, deliberately: this is the port's half of upstream's
    /// READ-side use of the cap (`Gvt(path, WUe)` measuring the filesystem
    /// `size`), not of the UTF-16 write accumulator in [`Self::append`]. The
    /// two units against one constant are upstream's own — see
    /// [`MAX_TASK_OUTPUT_BYTES`].
    pub(crate) fn validate_terminal_result(
        &self,
        output_file: &Path,
        content: &str,
    ) -> Result<(), OutputError> {
        self.relative_path_for(output_file)?;
        if content.len() as u64 > MAX_TASK_OUTPUT_BYTES {
            return Err(OutputError::Io(format!(
                "terminal output exceeds {MAX_TASK_OUTPUT_BYTES_DISPLAY} disk cap"
            )));
        }
        Ok(())
    }

    /// Make a bounded terminal payload authoritative before best-effort disk
    /// persistence. An error from this method means the in-memory projection is
    /// still authoritative but the physical spool must not be exposed as such.
    pub(crate) async fn replace_terminal_result_authoritative(
        &self,
        output_file: &Path,
        content: &str,
    ) -> Result<(), OutputError> {
        let relative = self.relative_path_for(output_file)?;
        let write_lock = self.write_lock_for(output_file).await;
        let _writes = write_lock.lock().await;
        self.validate_terminal_result(output_file, content)?;

        self.terminal_overrides.lock().await.insert(
            output_file.to_path_buf(),
            TerminalOverride {
                content: content.to_string(),
                physical_spool_authoritative: false,
            },
        );
        self.terminal.lock().await.insert(output_file.to_path_buf());

        self.check_output_root().await?;
        self.fs
            .write_file_rooted_atomic(&self.output_dir, &relative, content)
            .await
            .map_err(|error| self.map_rooted_error(error))?;
        self.check_output_root().await?;

        if let Some(terminal_override) = self.terminal_overrides.lock().await.get_mut(output_file) {
            terminal_override.physical_spool_authoritative = true;
        }
        let mut caps = self.caps.lock().await;
        caps.insert(
            output_file.to_path_buf(),
            CapState {
                // Same unit as `append`: UTF-16 code units.
                bytes_written: utf16_units(content),
                capped: false,
                // A terminal replacement is a fresh, complete payload.
                lost_output: false,
            },
        );
        Ok(())
    }

    /// Publish a terminal failure with a fail-closed read path.
    ///
    /// The in-memory override becomes authoritative before the filesystem
    /// write. If the write fails (for example, because the disk is full), all
    /// subsequent reads still return this failure payload instead of stale
    /// success bytes left in the spool. The override also makes late handler
    /// appends terminal and is intentionally absorbing for this spool.
    pub async fn replace_terminal_result_fail_closed(
        &self,
        output_file: &Path,
        content: &str,
    ) -> Result<(), OutputError> {
        self.replace_terminal_result_authoritative(output_file, content)
            .await
    }

    pub(crate) async fn physical_output_is_authoritative(&self, output_file: &Path) -> bool {
        let override_state = self
            .terminal_overrides
            .lock()
            .await
            .get(output_file)
            .map(|terminal_override| terminal_override.physical_spool_authoritative);
        match override_state {
            None => true,
            Some(false) => false,
            Some(true) => self.check_output_root().await.is_ok(),
        }
    }

    /// Test-only accessor for the backing filesystem (so M5-01 tests can
    /// seed spool content directly without going through a handler).
    #[doc(hidden)]
    pub fn fs_for_test(&self) -> Arc<dyn FileSystem> {
        self.fs.clone()
    }

    /// Test-only: pre-seed the per-path write-side byte counter, so the 5GB cap
    /// can be exercised without materializing 5GB of output.
    #[cfg(test)]
    async fn seed_bytes_for_test(&self, output_file: &Path, bytes: u64) {
        let mut caps = self.caps.lock().await;
        caps.entry(output_file.to_path_buf())
            .or_default()
            .bytes_written = bytes;
    }

    /// Read a window of the task's spool file.
    ///
    /// The per-spool lock is intentional: terminal publication records its
    /// authoritative override before attempting the filesystem rewrite, and a
    /// read must observe either that override or the pre-terminal file, never
    /// stale physical bytes after the registry has committed a terminal task.
    pub async fn read(
        &self,
        output_file: &Path,
        opts: OutputOptions,
    ) -> Result<TaskOutput, OutputError> {
        let relative = self.relative_path_for(output_file)?;
        let write_lock = self.write_lock_for(output_file).await;
        let _writes = write_lock.lock().await;
        if let Some(terminal_override) = self
            .terminal_overrides
            .lock()
            .await
            .get(output_file)
            .cloned()
        {
            let physical_spool_authoritative = terminal_override.physical_spool_authoritative
                && self.check_output_root().await.is_ok();
            let fc =
                platform_api::apply_line_window(terminal_override.content, opts.offset, opts.limit);
            return Ok(TaskOutput {
                content: cap_full_read(fc.content, &opts),
                total_lines: fc.total_lines,
                truncated: fc.truncated,
                physical_spool_authoritative,
            });
        }
        #[cfg(any(unix, windows))]
        {
            use std::io::{Read, Seek, SeekFrom};
            #[cfg(unix)]
            use std::os::unix::fs::MetadataExt;
            let mut links = self.linked_transcripts.lock().await;
            if let Some((target, file)) = links.get_mut(output_file) {
                self.check_output_root().await?;
                let pinned = file
                    .metadata()
                    .map_err(|e| OutputError::Io(e.to_string()))?;
                #[cfg(unix)]
                {
                    let advertised = std::fs::read_link(output_file)
                        .map_err(|e| OutputError::Io(e.to_string()))?;
                    let current = std::fs::symlink_metadata(&*target)
                        .map_err(|e| OutputError::Io(e.to_string()))?;
                    if advertised != *target
                        || !current.is_file()
                        || current.ino() != pinned.ino()
                        || current.dev() != pinned.dev()
                    {
                        return Err(OutputError::SwapRefused(
                            "task output link identity changed".into(),
                        ));
                    }
                }
                #[cfg(windows)]
                {
                    let pin = self.check_output_root().await?;
                    platform_api::rooted_fs::validate_task_output_link(
                        &self.output_dir,
                        &relative,
                        pin.as_ref(),
                        target,
                        file,
                    )
                    .map_err(|error| OutputError::SwapRefused(error.to_string()))?;
                }
                if opts.offset.is_some() || opts.limit.is_some() {
                    use std::io::BufRead;
                    file.seek(SeekFrom::Start(0))
                        .map_err(|e| OutputError::Io(e.to_string()))?;
                    let reader = std::io::BufReader::new(&mut *file);
                    let mut content = String::new();
                    let mut total_lines = 0u64;
                    let mut taken = 0u64;
                    for line in reader.lines() {
                        let line = line.map_err(|e| OutputError::Io(e.to_string()))?;
                        if total_lines >= opts.offset.unwrap_or(0)
                            && taken < opts.limit.unwrap_or(u64::MAX)
                        {
                            if taken > 0 {
                                content.push('\n');
                            }
                            content.push_str(&line);
                            taken += 1;
                        }
                        total_lines += 1;
                    }
                    return Ok(TaskOutput {
                        content,
                        total_lines,
                        truncated: false,
                        physical_spool_authoritative: true,
                    });
                }
                let start = pinned.len().saturating_sub(MAX_TASK_OUTPUT_READ_BYTES);
                file.seek(SeekFrom::Start(start))
                    .map_err(|e| OutputError::Io(e.to_string()))?;
                let mut bytes = Vec::new();
                file.take(MAX_TASK_OUTPUT_READ_BYTES)
                    .read_to_end(&mut bytes)
                    .map_err(|e| OutputError::Io(e.to_string()))?;
                let skip = if start > 0 {
                    bytes
                        .iter()
                        .take_while(|byte| **byte & 0xc0 == 0x80)
                        .count()
                } else {
                    0
                };
                let content = String::from_utf8_lossy(&bytes[skip..]).into_owned();
                let omitted = start.saturating_add(skip as u64);
                let content = if omitted > 0 && opts.offset.is_none() && opts.limit.is_none() {
                    format!(
                        "[{}KB of earlier output omitted]\n{content}",
                        ((omitted as f64) / 1024.0).round() as u64
                    )
                } else {
                    content
                };
                let fc = platform_api::apply_line_window(content, opts.offset, opts.limit);
                return Ok(TaskOutput {
                    content: fc.content,
                    total_lines: fc.total_lines,
                    truncated: fc.truncated || start > 0,
                    physical_spool_authoritative: true,
                });
            }
        }
        let root_identity = self.check_output_root().await?;
        let fc = self
            .fs
            .read_file_rooted_no_follow_window_pinned(
                &self.output_dir,
                &relative,
                opts.offset,
                opts.limit,
                root_identity.as_ref(),
            )
            .await
            .map_err(|e| OutputError::Io(e.to_string()))?;
        Ok(TaskOutput {
            content: cap_full_read(fc.content, &opts),
            total_lines: fc.total_lines,
            truncated: fc.truncated,
            physical_spool_authoritative: true,
        })
    }
}

/// Apply [`apply_read_tail_cap`] only to a FULL read.
///
/// A windowed read is the caller asking for a specific slice; the oracle's
/// byte cap belongs to `getTaskOutput`, which takes no window. `total_lines`
/// deliberately still counts the whole file — the header says what was dropped,
/// and the line total is what the caller pages against.
fn cap_full_read(content: String, opts: &OutputOptions) -> String {
    if opts.offset.is_some() || opts.limit.is_some() {
        return content;
    }
    apply_read_tail_cap(content)
}

/// Framed-output owner used by an independent native shell supervisor. The
/// supervisor has no session registry; its receipt carries completion instead.
pub struct TaskOutputSink {
    manager: Arc<TaskOutputManager>,
    path: PathBuf,
    terminal: Mutex<bool>,
}

impl TaskOutputSink {
    #[must_use]
    pub fn new(manager: Arc<TaskOutputManager>, path: PathBuf) -> Self {
        Self {
            manager,
            path,
            terminal: Mutex::new(false),
        }
    }

    fn validate(&self, task_id: &str) -> Result<(), mobile_linux_api::ProcessError> {
        if self
            .manager
            .path_for(task_id)
            .map_err(|error| mobile_linux_api::ProcessError::Io(error.to_string()))?
            != self.path
        {
            return Err(mobile_linux_api::ProcessError::Io(
                "supervisor task output identity mismatch".into(),
            ));
        }
        Ok(())
    }
}

#[async_trait::async_trait]
impl platform_api::BackgroundExitSink for TaskOutputSink {
    fn manages_output(&self) -> bool {
        true
    }
    async fn append_output(
        &self,
        task_id: &str,
        content: &str,
    ) -> Result<(), mobile_linux_api::ProcessError> {
        self.validate(task_id)?;
        self.manager
            .append(&self.path, content)
            .await
            .map_err(|error| mobile_linux_api::ProcessError::Io(error.to_string()))
    }
    async fn flush_output(&self, task_id: &str) -> Result<(), mobile_linux_api::ProcessError> {
        self.validate(task_id)?;
        self.manager
            .flush_writer(&self.path)
            .await
            .map_err(|error| mobile_linux_api::ProcessError::Io(error.to_string()))
    }
    async fn finalize_persisted_output(
        &self,
        task_id: &str,
        max_bytes: u64,
    ) -> Result<Option<u64>, mobile_linux_api::ProcessError> {
        self.validate(task_id)?;
        self.manager
            .finalize_persisted_output(&self.path, max_bytes)
            .await
            .map(Some)
            .map_err(|error| mobile_linux_api::ProcessError::Io(error.to_string()))
    }
    async fn on_exit(&self, task_id: &str, exit_code: Option<i32>) {
        self.on_exit_with_status(task_id, exit_code, false).await;
    }
    async fn on_exit_with_status(&self, task_id: &str, exit_code: Option<i32>, killed: bool) {
        if self.validate(task_id).is_err() {
            return;
        }
        let mut terminal = self.terminal.lock().await;
        if *terminal {
            return;
        }
        let trailer = if killed {
            "\n[killed]\n".to_string()
        } else {
            format!(
                "\n[exited with code {}]\n",
                exit_code.map_or_else(|| "unknown".into(), |code| code.to_string())
            )
        };
        if let Err(error) = self.manager.append(&self.path, &trailer).await {
            tracing::error!(%error, "supervisor terminal output append failed");
        }
        if let Err(error) = self.manager.flush_writer(&self.path).await {
            tracing::error!(%error, "supervisor terminal output flush failed");
        }
        self.manager.evict_writer(&self.path).await;
        *terminal = true;
    }
}

#[cfg(test)]
#[allow(clippy::unwrap_used)]
mod tests {
    use super::*;
    use async_trait::async_trait;
    use platform_api::filesystem::{FileContent, FileEvent, FlockGuard, FsError};
    use std::collections::HashMap;
    use std::sync::atomic::{AtomicUsize, Ordering};
    use tokio::sync::Mutex;

    /// In-memory [`FileSystem`] with REAL exclusive-create semantics: a
    /// `create_new_file` for a path that already has a key fails with
    /// [`FsError::AlreadyExists`] (mirroring `O_EXCL`), and never overwrites the
    /// stored bytes. Also counts `create_new_file` calls so a test can assert
    /// the registry does not re-allocate.
    struct ExclusiveFs {
        files: Mutex<HashMap<String, String>>,
        creates: AtomicUsize,
        /// TOF-05: fail the next N appends, so the retry path is reachable.
        fail_appends: AtomicUsize,
        fail_opens: AtomicUsize,
        staged_attempts: AtomicUsize,
        block_attempt: AtomicUsize,
        stage_entered: tokio::sync::Notify,
        stage_resume: tokio::sync::Notify,

        /// Every body `append_file` was ASKED to write, failed ones included —
        /// the retry writes a different body from the first attempt, and that
        /// difference is the whole point.
        append_bodies: Mutex<Vec<String>>,
    }
    impl ExclusiveFs {
        fn new() -> Arc<Self> {
            Arc::new(Self {
                files: Mutex::new(HashMap::new()),
                creates: AtomicUsize::new(0),
                fail_appends: AtomicUsize::new(0),
                fail_opens: AtomicUsize::new(0),
                staged_attempts: AtomicUsize::new(0),
                block_attempt: AtomicUsize::new(0),
                stage_entered: tokio::sync::Notify::new(),
                stage_resume: tokio::sync::Notify::new(),

                append_bodies: Mutex::new(Vec::new()),
            })
        }
        fn create_count(&self) -> usize {
            self.creates.load(Ordering::SeqCst)
        }
        fn fail_next_appends(&self, n: usize) {
            self.fail_appends.store(n, Ordering::SeqCst);
        }
        async fn append_bodies(&self) -> Vec<String> {
            self.append_bodies.lock().await.clone()
        }
    }
    #[async_trait]
    impl FileSystem for ExclusiveFs {
        async fn root_identity_no_follow(
            &self,
            root: &Path,
        ) -> Result<Option<platform_api::rooted_fs::RootIdentity>, FsError> {
            if root.exists() {
                platform_api::rooted_fs::root_identity(root).map(Some)
            } else {
                Ok(None)
            }
        }

        async fn read_file(
            &self,
            path: &str,
            _offset: Option<u64>,
            _limit: Option<u64>,
        ) -> Result<FileContent, FsError> {
            let map = self.files.lock().await;
            let content = map.get(path).cloned().unwrap_or_default();
            let total_lines = content.lines().count() as u64;
            Ok(FileContent {
                content,
                truncated: false,
                total_lines,
            })
        }
        async fn write_file(&self, path: &str, body: &str) -> Result<(), FsError> {
            self.files
                .lock()
                .await
                .insert(path.to_string(), body.to_string());
            Ok(())
        }
        async fn create_new_file(&self, path: &str) -> Result<(), FsError> {
            self.creates.fetch_add(1, Ordering::SeqCst);
            let mut map = self.files.lock().await;
            if map.contains_key(path) {
                // O_EXCL collision — refuse, and DO NOT touch the stored bytes.
                return Err(FsError::AlreadyExists(path.to_string()));
            }
            map.insert(path.to_string(), String::new());
            Ok(())
        }
        fn is_within_workspace(&self, _: &str) -> bool {
            true
        }
        async fn watch(
            &self,
            _: &str,
        ) -> Result<std::pin::Pin<Box<dyn futures::Stream<Item = FileEvent> + Send>>, FsError>
        {
            Err(FsError::Io("not supported".into()))
        }
        async fn append_file_rooted_staged(
            &self,
            root: &Path,
            relative: &Path,
            content: &str,
            expected: Option<&platform_api::rooted_fs::RootIdentity>,
        ) -> Result<(), platform_api::filesystem::FileAppendError> {
            use platform_api::filesystem::{FileAppendError, FileAppendStage};
            let attempt = self.staged_attempts.fetch_add(1, Ordering::SeqCst) + 1;
            if self.block_attempt.load(Ordering::SeqCst) == attempt {
                self.stage_entered.notify_one();
                self.stage_resume.notified().await;
            }
            if self
                .fail_opens
                .fetch_update(Ordering::SeqCst, Ordering::SeqCst, |count| {
                    count.checked_sub(1)
                })
                .is_ok()
            {
                return Err(FileAppendError {
                    stage: FileAppendStage::Open,
                    error: FsError::Io("injected open exhaustion".into()),
                });
            }
            self.append_file_rooted_no_follow_pinned(root, relative, content, expected)
                .await
                .map_err(|error| FileAppendError {
                    stage: FileAppendStage::Write,
                    error,
                })
        }

        async fn append_file(&self, path: &str, body: &str) -> Result<(), FsError> {
            self.append_bodies.lock().await.push(body.to_string());
            if self
                .fail_appends
                .fetch_update(Ordering::SeqCst, Ordering::SeqCst, |n| n.checked_sub(1))
                .is_ok()
            {
                return Err(FsError::Io("no space left on device".into()));
            }
            self.files
                .lock()
                .await
                .entry(path.to_string())
                .or_default()
                .push_str(body);
            Ok(())
        }
        async fn truncate(&self, _: &str, _: u64) -> Result<(), FsError> {
            Ok(())
        }
        async fn file_mtime(&self, _: &str) -> Result<std::time::SystemTime, FsError> {
            Ok(std::time::SystemTime::UNIX_EPOCH)
        }
        async fn file_size(&self, path: &str) -> Result<u64, FsError> {
            let map = self.files.lock().await;
            Ok(map.get(path).map_or(0, |s| s.len() as u64))
        }
        async fn delete_file(&self, path: &str) -> Result<(), FsError> {
            self.files.lock().await.remove(path);
            Ok(())
        }
        async fn symlink(&self, _: &str, _: &str) -> Result<(), FsError> {
            Ok(())
        }
        async fn flock_exclusive(&self, _: &str) -> Result<Box<dyn FlockGuard>, FsError> {
            Err(FsError::Io("not supported".into()))
        }
        async fn fsync(&self, _: &str) -> Result<(), FsError> {
            Ok(())
        }
    }

    fn manager() -> (Arc<ExclusiveFs>, TaskOutputManager) {
        let fs = ExclusiveFs::new();
        let mgr = TaskOutputManager::new(PathBuf::from("/spool"), fs.clone());
        (fs, mgr)
    }

    #[tokio::test]
    async fn writer_open_failure_retains_batch_and_large_single_append_triggers_drop() {
        let dir = tempfile::tempdir().unwrap();
        let fs = ExclusiveFs::new();
        let mgr = TaskOutputManager::new(dir.path().into(), fs.clone());
        let path = mgr.allocate("bopenfail").await.unwrap();
        fs.fail_opens.store(2, Ordering::SeqCst);
        assert!(mgr
            .append(&path, &"x".repeat(16 * 1024 * 1024 + 1))
            .await
            .is_err());
        assert_eq!(fs.staged_attempts.load(Ordering::SeqCst), 2);
        assert!(
            fs.append_bodies.lock().await.is_empty(),
            "no payload write was attempted"
        );
        let writer = mgr.writer_for(&path).await;
        assert_eq!(writer.queue().chars, utf16_units(OUTPUT_OMITTED_MARKER));
        assert!(mgr.lost_output(&path).await);
        mgr.append(&path, "recovered").await.unwrap();
        assert_eq!(
            fs.append_bodies.lock().await.as_slice(),
            [format!("{OUTPUT_OMITTED_MARKER}recovered")]
        );
    }

    #[tokio::test]
    async fn writer_open_failure_below_gate_preserves_output_without_claiming_loss() {
        let dir = tempfile::tempdir().unwrap();
        let fs = ExclusiveFs::new();
        let mgr = TaskOutputManager::new(dir.path().into(), fs.clone());
        let path = mgr.allocate("bopenkeep").await.unwrap();
        fs.fail_opens.store(2, Ordering::SeqCst);
        assert!(mgr.append(&path, "not consumed").await.is_err());
        assert!(!mgr.lost_output(&path).await);
        mgr.append(&path, " + next").await.unwrap();
        assert_eq!(
            fs.append_bodies.lock().await.as_slice(),
            ["not consumed + next"]
        );
    }

    #[tokio::test]
    async fn writer_concurrent_success_flushes_every_chunk_once_for_both_callers() {
        let dir = tempfile::tempdir().unwrap();
        let fs = ExclusiveFs::new();
        let mgr = Arc::new(TaskOutputManager::new(dir.path().into(), fs.clone()));
        let path = mgr.allocate("bsuccess1").await.unwrap();
        fs.block_attempt.store(1, Ordering::SeqCst);
        let (first_mgr, first_path) = (mgr.clone(), path.clone());
        let first = tokio::spawn(async move { first_mgr.append(&first_path, "first").await });
        fs.stage_entered.notified().await;
        let (second_mgr, second_path) = (mgr.clone(), path.clone());
        let second = tokio::spawn(async move { second_mgr.append(&second_path, "second").await });
        let writer = mgr.writer_for(&path).await;
        tokio::time::timeout(std::time::Duration::from_secs(5), async {
            while writer.queue().chars != 6 {
                tokio::task::yield_now().await;
            }
        })
        .await
        .unwrap();
        fs.stage_resume.notify_one();
        first.await.unwrap().unwrap();
        second.await.unwrap().unwrap();
        assert_eq!(
            fs.append_bodies.lock().await.as_slice(),
            ["first", "second"]
        );
        assert_eq!(mgr.caps.lock().await.get(&path).unwrap().bytes_written, 11);
        assert!(!mgr.lost_output(&path).await);
    }

    #[tokio::test]
    async fn writer_concurrent_append_during_failing_retry_is_bounded_and_shares_error() {
        let dir = tempfile::tempdir().unwrap();
        let fs = ExclusiveFs::new();
        let mgr = Arc::new(TaskOutputManager::new(dir.path().into(), fs.clone()));
        let path = mgr.allocate("bconcur01").await.unwrap();
        fs.fail_next_appends(2);
        fs.block_attempt.store(2, Ordering::SeqCst);
        let (first_mgr, first_path) = (mgr.clone(), path.clone());
        let first = tokio::spawn(async move { first_mgr.append(&first_path, "first").await });
        fs.stage_entered.notified().await;
        let (second_mgr, second_path) = (mgr.clone(), path.clone());
        let second = tokio::spawn(async move {
            second_mgr
                .append(&second_path, &"x".repeat(16 * 1024 * 1024 + 1))
                .await
        });
        let writer = mgr.writer_for(&path).await;
        tokio::time::timeout(std::time::Duration::from_secs(5), async {
            while writer.queue().chars <= 16 * 1024 * 1024 {
                tokio::task::yield_now().await;
            }
        })
        .await
        .unwrap();
        fs.stage_resume.notify_one();
        assert!(first.await.unwrap().is_err());
        assert!(
            second.await.unwrap().is_err(),
            "coalesced caller observes same failed generation"
        );
        assert_eq!(
            fs.staged_attempts.load(Ordering::SeqCst),
            2,
            "waiting append must not silently start another retry"
        );
        assert_eq!(writer.queue().chars, utf16_units(OUTPUT_OMITTED_MARKER));
        mgr.evict_writer(&path).await;
        assert!(!mgr.writers.lock().await.contains_key(&path));
        assert_eq!(
            fs.staged_attempts.load(Ordering::SeqCst),
            2,
            "eviction flush does not retry a failed writer"
        );
    }

    #[tokio::test]
    async fn output_delta_preserves_unicode_across_byte_window_boundary() {
        let (_fs, manager) = manager();
        let output = manager.allocate("butf80001").await.unwrap();
        let content = format!("{}中", "a".repeat(MAX_TASK_OUTPUT_READ_BYTES as usize - 1));
        manager.append(&output, &content).await.unwrap();
        let (first, offset) = manager.read_delta(&output, 0).await.unwrap();
        let (second, end) = manager.read_delta(&output, offset).await.unwrap();
        assert_eq!(first + &second, content);
        assert_eq!(end, content.len() as u64);
    }

    #[tokio::test]
    async fn output_delta_counts_utf8_bytes_and_does_not_repeat_text() {
        let (_fs, manager) = manager();
        let output = manager.allocate("bdelta001").await.unwrap();
        manager.append(&output, "你好\n").await.unwrap();
        let (content, offset) = manager.read_delta(&output, 0).await.unwrap();
        assert_eq!(content, "你好\n");
        assert_eq!(offset, 7);
        assert_eq!(
            manager.read_delta(&output, offset).await.unwrap(),
            (String::new(), offset)
        );
        manager.append(&output, "next").await.unwrap();
        assert_eq!(
            manager.read_delta(&output, offset).await.unwrap(),
            ("next".into(), 11)
        );
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn registered_transcript_link_reads_live_inode_and_refuses_retarget() {
        use std::io::Write;
        let directory = tempfile::tempdir().unwrap();
        let root = directory.path().join("tasks");
        std::fs::create_dir(&root).unwrap();
        let target = directory.path().join("agent-real.jsonl");
        std::fs::write(&target, "first\n").unwrap();
        let manager = TaskOutputManager::new(root.clone(), ExclusiveFs::new());
        let output = manager.allocate("alink0001").await.unwrap();
        manager.link_transcript(&output, &target).await.unwrap();
        assert_eq!(std::fs::read_link(&output).unwrap(), target);
        manager
            .append(&output, "must not contaminate transcript")
            .await
            .unwrap();
        let mut writer = std::fs::OpenOptions::new()
            .append(true)
            .open(&target)
            .unwrap();
        writer.write_all(b"second\n").unwrap();
        assert_eq!(
            manager
                .read(&output, OutputOptions::default())
                .await
                .unwrap()
                .content,
            "first\nsecond\n"
        );
        assert_eq!(
            manager
                .read(
                    &output,
                    OutputOptions {
                        offset: Some(1),
                        limit: Some(1)
                    }
                )
                .await
                .unwrap()
                .content,
            "second"
        );
        let other = directory.path().join("other.jsonl");
        std::fs::write(&other, "secret").unwrap();
        std::fs::remove_file(&output).unwrap();
        std::os::unix::fs::symlink(&other, &output).unwrap();
        assert!(matches!(
            manager.read(&output, OutputOptions::default()).await,
            Err(OutputError::SwapRefused(_))
        ));
        assert_eq!(std::fs::read_to_string(&target).unwrap(), "first\nsecond\n");
        // Final registry eviction releases the retained descriptor, but never
        // deletes the historical file or follows the now-retargeted link.
        manager.release_output_state(&output).await;
        assert!(manager.linked_transcripts.lock().await.is_empty());
        assert!(output.symlink_metadata().unwrap().file_type().is_symlink());
        assert_eq!(std::fs::read_to_string(&other).unwrap(), "secret");
    }

    #[cfg(any(unix, windows))]
    #[tokio::test]
    async fn adopted_output_recovers_final_link_after_manager_restart() {
        let directory = tempfile::tempdir().unwrap();
        let root = directory.path().join("tasks");
        std::fs::create_dir(&root).unwrap();
        let target = directory.path().join("source.output");
        std::fs::write(&target, "before restart\n").unwrap();
        let manager = TaskOutputManager::new(root.clone(), ExclusiveFs::new());
        let output = manager.path_for("badopt001").unwrap();
        manager.adopt_output(&output, &target).await.unwrap();
        assert_eq!(
            manager
                .read(&output, OutputOptions::default())
                .await
                .unwrap()
                .content,
            "before restart\n"
        );
        drop(manager);
        std::fs::write(&target, "before restart\nafter restart\n").unwrap();
        let restored = TaskOutputManager::new(root, ExclusiveFs::new());
        restored.adopt_output(&output, &target).await.unwrap();
        assert_eq!(
            restored
                .read(&output, OutputOptions::default())
                .await
                .unwrap()
                .content,
            "before restart\nafter restart\n"
        );
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn discarded_transcript_task_can_reuse_id_without_old_pin_or_writer() {
        use std::io::Write;
        use std::os::unix::fs::MetadataExt;
        // Exercise real allocation/deletion as well as real transcript pins.
        // ExclusiveFs deliberately keeps spool bytes in memory, so it cannot
        // witness unlink/reallocation of the physical symlink in this test.
        struct RealTranscriptFs;
        #[async_trait]
        impl FileSystem for RealTranscriptFs {
            async fn root_identity_no_follow(
                &self,
                root: &Path,
            ) -> Result<Option<platform_api::rooted_fs::RootIdentity>, FsError> {
                platform_api::rooted_fs::root_identity(root).map(Some)
            }
            async fn read_file(
                &self,
                path: &str,
                offset: Option<u64>,
                limit: Option<u64>,
            ) -> Result<FileContent, FsError> {
                let content = std::fs::read_to_string(path)
                    .map_err(|error| FsError::Io(error.to_string()))?;
                Ok(platform_api::apply_line_window(content, offset, limit))
            }
            async fn write_file(&self, path: &str, content: &str) -> Result<(), FsError> {
                std::fs::write(path, content).map_err(|error| FsError::Io(error.to_string()))
            }
            async fn create_new_file(&self, path: &str) -> Result<(), FsError> {
                std::fs::OpenOptions::new()
                    .write(true)
                    .create_new(true)
                    .open(path)
                    .map(|_| ())
                    .map_err(|error| FsError::Io(error.to_string()))
            }
            async fn create_new_file_rooted_no_follow_pinned(
                &self,
                root: &Path,
                relative: &Path,
                expected: Option<&platform_api::rooted_fs::RootIdentity>,
            ) -> Result<(), FsError> {
                platform_api::rooted_fs::create_new_file_pinned(root, relative, expected)
            }
            async fn append_file(&self, path: &str, content: &str) -> Result<(), FsError> {
                std::fs::OpenOptions::new()
                    .append(true)
                    .open(path)
                    .and_then(|mut file| file.write_all(content.as_bytes()))
                    .map_err(|error| FsError::Io(error.to_string()))
            }
            async fn append_file_rooted_no_follow_pinned(
                &self,
                root: &Path,
                relative: &Path,
                content: &str,
                expected: Option<&platform_api::rooted_fs::RootIdentity>,
            ) -> Result<(), FsError> {
                platform_api::rooted_fs::append_file_pinned(root, relative, content, expected)
            }
            async fn delete_file(&self, path: &str) -> Result<(), FsError> {
                std::fs::remove_file(path).map_err(|error| FsError::Io(error.to_string()))
            }
            async fn delete_file_rooted_no_follow(
                &self,
                root: &Path,
                relative: &Path,
            ) -> Result<(), FsError> {
                platform_api::rooted_fs::remove_file(root, relative)
            }
            async fn truncate(&self, path: &str, len: u64) -> Result<(), FsError> {
                std::fs::OpenOptions::new()
                    .write(true)
                    .open(path)
                    .and_then(|file| file.set_len(len))
                    .map_err(|error| FsError::Io(error.to_string()))
            }
            async fn file_mtime(&self, path: &str) -> Result<std::time::SystemTime, FsError> {
                std::fs::metadata(path)
                    .and_then(|metadata| metadata.modified())
                    .map_err(|error| FsError::Io(error.to_string()))
            }
            async fn file_size(&self, path: &str) -> Result<u64, FsError> {
                std::fs::metadata(path)
                    .map(|metadata| metadata.len())
                    .map_err(|error| FsError::Io(error.to_string()))
            }
            async fn symlink(&self, target: &str, link: &str) -> Result<(), FsError> {
                std::os::unix::fs::symlink(target, link)
                    .map_err(|error| FsError::Io(error.to_string()))
            }
            async fn fsync(&self, path: &str) -> Result<(), FsError> {
                std::fs::File::open(path)
                    .and_then(|file| file.sync_all())
                    .map_err(|error| FsError::Io(error.to_string()))
            }
            fn is_within_workspace(&self, _: &str) -> bool {
                true
            }
            async fn watch(
                &self,
                _: &str,
            ) -> Result<std::pin::Pin<Box<dyn futures::Stream<Item = FileEvent> + Send>>, FsError>
            {
                Err(FsError::Io("unused by test".into()))
            }
            async fn flock_exclusive(&self, _: &str) -> Result<Box<dyn FlockGuard>, FsError> {
                Err(FsError::Io("unused by test".into()))
            }
        }
        let directory = tempfile::tempdir().unwrap();
        let root = directory.path().join("tasks");
        std::fs::create_dir(&root).unwrap();
        let old_target = directory.path().join("old-agent.jsonl");
        let new_target = directory.path().join("new-agent.jsonl");
        std::fs::write(&old_target, "old transcript\n").unwrap();
        std::fs::write(&new_target, "new transcript\n").unwrap();
        let manager = TaskOutputManager::new(root, Arc::new(RealTranscriptFs));
        let output = manager.allocate("areuse001").await.unwrap();
        manager.append(&output, "original spool\n").await.unwrap();
        manager.link_transcript(&output, &old_target).await.unwrap();
        assert_eq!(
            manager
                .read(&output, OutputOptions::default())
                .await
                .unwrap()
                .content,
            "old transcript\n"
        );
        let old_writer = {
            let writers = manager.writers.lock().await;
            Arc::downgrade(writers.get(&output).unwrap())
        };
        let old_inode = manager
            .linked_transcripts
            .lock()
            .await
            .get(&output)
            .unwrap()
            .1
            .metadata()
            .unwrap()
            .ino();

        manager.discard(&output).await.unwrap();
        assert!(output.symlink_metadata().is_err());
        assert!(!manager
            .linked_transcripts
            .lock()
            .await
            .contains_key(&output));
        assert!(
            old_writer.upgrade().is_none(),
            "discard releases the writer generation, not just its bytes"
        );
        assert_eq!(
            std::fs::read_to_string(&old_target).unwrap(),
            "old transcript\n"
        );

        let reused = manager.allocate("areuse001").await.unwrap();
        assert_eq!(reused, output);
        assert_eq!(
            manager
                .read(&reused, OutputOptions::default())
                .await
                .unwrap()
                .content,
            ""
        );
        manager
            .append(&reused, "new spool generation\n")
            .await
            .unwrap();
        assert_eq!(
            manager
                .read(&reused, OutputOptions::default())
                .await
                .unwrap()
                .content,
            "new spool generation\n"
        );
        manager.link_transcript(&reused, &new_target).await.unwrap();
        assert_eq!(std::fs::read_link(&reused).unwrap(), new_target);
        let new_inode = manager
            .linked_transcripts
            .lock()
            .await
            .get(&reused)
            .unwrap()
            .1
            .metadata()
            .unwrap()
            .ino();
        assert_ne!(new_inode, old_inode);
        assert_eq!(
            manager
                .read(&reused, OutputOptions::default())
                .await
                .unwrap()
                .content,
            "new transcript\n"
        );
        std::fs::OpenOptions::new()
            .append(true)
            .open(&new_target)
            .unwrap()
            .write_all(b"new tail\n")
            .unwrap();
        assert_eq!(
            manager
                .read(&reused, OutputOptions::default())
                .await
                .unwrap()
                .content,
            "new transcript\nnew tail\n"
        );
        assert_eq!(
            std::fs::read_to_string(&old_target).unwrap(),
            "old transcript\n"
        );
    }

    #[tokio::test]
    async fn allocate_creates_an_empty_spool_file() {
        let (fs, mgr) = manager();
        let path = mgr.allocate("bdeadbeef").await.expect("first allocate");
        assert_eq!(path, PathBuf::from("/spool/bdeadbeef.output"));
        // The file exists and is empty.
        assert!(fs
            .files
            .lock()
            .await
            .contains_key("/spool/bdeadbeef.output"));
    }

    #[tokio::test]
    async fn second_allocate_of_same_id_errors_not_truncates() {
        // T4: a second allocate of the SAME id must surface AlreadyExists, not
        // silently truncate output a worker already appended.
        let (fs, mgr) = manager();
        let path = mgr.allocate("babc123").await.unwrap();

        // A worker appends output AFTER the first allocate.
        fs.append_file_no_follow(path.to_str().unwrap(), "important output\n")
            .await
            .unwrap();

        // The second allocate must error rather than clobber.
        let err = mgr
            .allocate("babc123")
            .await
            .expect_err("a second allocate of the same id must error");
        assert!(
            matches!(err, OutputError::AlreadyExists(_)),
            "second allocate returns AlreadyExists; got {err:?}"
        );

        // The worker's output survived — the refused allocate touched no bytes.
        let read = mgr.read(&path, OutputOptions::default()).await.unwrap();
        assert!(
            read.content.contains("important output"),
            "the existing output was NOT truncated by the refused allocate; got {:?}",
            read.content
        );
    }

    #[test]
    fn path_for_rejects_parent_and_nested_task_ids() {
        let (_, manager) = manager();
        for id in ["../outside", "nested/task", "/absolute", "./task"] {
            assert!(
                matches!(manager.path_for(id), Err(OutputError::PathEscape(_))),
                "{id}"
            );
        }
        assert_eq!(
            manager.path_for("b12345678").unwrap(),
            PathBuf::from("/spool/b12345678.output")
        );
    }

    #[tokio::test]
    async fn path_for_does_not_create_or_count_a_spool_file() {
        // `path_for` (the registry's consume-the-handler's-path seam) only
        // reconstructs the deterministic path — it allocates nothing, so a
        // worker's already-allocated spool is never re-created.
        let (fs, mgr) = manager();
        let path = mgr.path_for("bxyz").unwrap();
        assert_eq!(path, PathBuf::from("/spool/bxyz.output"));
        assert_eq!(fs.create_count(), 0, "path_for must not create a file");
        assert!(
            !fs.files.lock().await.contains_key("/spool/bxyz.output"),
            "path_for must not materialize the spool"
        );
    }

    #[tokio::test]
    async fn append_writes_through_and_under_the_cap_is_unmarked() {
        // Below the 5GB cap, `append` writes the chunk verbatim — no marker.
        let (_fs, mgr) = manager();
        let path = mgr.allocate("bappend01").await.unwrap();
        mgr.append(&path, "hello ").await.unwrap();
        mgr.append(&path, "world\n").await.unwrap();
        let read = mgr.read(&path, OutputOptions::default()).await.unwrap();
        assert_eq!(read.content, "hello world\n");
        assert!(
            !read.content.contains("disk cap"),
            "no truncation marker under the cap; got {:?}",
            read.content
        );
    }

    #[tokio::test]
    async fn terminal_replacement_overwrites_raw_and_blocks_late_append() {
        let (_fs, mgr) = manager();
        let path = mgr.allocate("bterminal1").await.unwrap();
        mgr.append(&path, "{\"ok\":true,\"raw\":true}\n")
            .await
            .unwrap();

        mgr.replace_terminal_result(&path, "{\"ok\":true,\"verified\":true}\n")
            .await
            .unwrap();
        // A handler chunk racing after terminal publication must not make the
        // unverified payload externally visible again.
        mgr.append(&path, "forged late chunk\n").await.unwrap();

        let read = mgr.read(&path, OutputOptions::default()).await.unwrap();
        assert_eq!(read.content, "{\"ok\":true,\"verified\":true}\n");
    }

    #[tokio::test]
    async fn fail_closed_terminal_override_absorbs_a_late_success_replacement() {
        let (_fs, mgr) = manager();
        let path = mgr.allocate("bterminal2").await.unwrap();

        mgr.replace_terminal_result_fail_closed(&path, "{\"ok\":false}\n")
            .await
            .unwrap();
        let error = mgr
            .replace_terminal_result(&path, "{\"ok\":true}\n")
            .await
            .expect_err("a late success must not replace an authoritative failure");
        assert!(error.to_string().contains("already authoritative"));
        assert_eq!(
            mgr.read(&path, OutputOptions::default())
                .await
                .unwrap()
                .content,
            "{\"ok\":false}\n"
        );
    }

    #[tokio::test]
    async fn different_spools_progress_during_terminal_replacement() {
        let (_fs, mgr) = manager();
        let left = mgr.allocate("bleft0001").await.unwrap();
        let right = mgr.allocate("bright001").await.unwrap();
        let (left_result, right_result) = tokio::join!(
            mgr.replace_terminal_result(&left, "verified-left\n"),
            mgr.append(&right, "independent-right\n"),
        );
        left_result.unwrap();
        right_result.unwrap();
        assert_eq!(
            mgr.read(&left, OutputOptions::default())
                .await
                .unwrap()
                .content,
            "verified-left\n"
        );
        assert_eq!(
            mgr.read(&right, OutputOptions::default())
                .await
                .unwrap()
                .content,
            "independent-right\n"
        );
    }

    /// TOF-05: a failed write retries EXACTLY once, and the retry does NOT
    /// re-issue the chunk — the oracle splices its buffer before awaiting, so
    /// the original body is already gone by the time the catch runs. The retry
    /// exists to get the MARKER on disk, not the output. Getting this backwards
    /// (retrying the same body, or writing the marker only after a second
    /// failure) is the natural reading and it is wrong in both directions.
    #[tokio::test]
    async fn a_failed_write_retries_once_with_the_marker_not_the_chunk() {
        let (fs, mgr) = manager();
        let path = mgr.allocate("bfail0001").await.unwrap();
        fs.fail_next_appends(1);

        mgr.append(&path, "the chunk that is lost\n").await.unwrap();

        let attempts = fs.append_bodies().await;
        assert_eq!(attempts.len(), 2, "one failure, one retry: {attempts:?}");
        assert_eq!(
            attempts[0], "the chunk that is lost\n",
            "the chunk is tried first"
        );
        assert_eq!(
            attempts[1], "\n[output omitted: it could not be written to disk]\n",
            "the RETRY carries the marker, not the chunk"
        );

        // What actually landed is the marker alone — the chunk is gone.
        let read = mgr.read(&path, OutputOptions::default()).await.unwrap();
        assert_eq!(
            read.content,
            "\n[output omitted: it could not be written to disk]\n"
        );
        assert!(
            mgr.lost_output(&path).await,
            "the spool is known-incomplete"
        );
    }

    /// `lostOutput` is SET-ONCE: the oracle stamps it in the drain's inner catch
    /// and never clears it — `#w()` resets `failing` and the reported set,
    /// `cancel()` resets the queue, neither touches it. A later good write does
    /// not make the spool complete again.
    #[tokio::test]
    async fn lost_output_survives_a_later_successful_write() {
        let (fs, mgr) = manager();
        let path = mgr.allocate("bfail0002").await.unwrap();
        fs.fail_next_appends(1);
        mgr.append(&path, "lost\n").await.unwrap();
        assert!(mgr.lost_output(&path).await);

        mgr.append(&path, "this one lands\n").await.unwrap();
        assert!(
            mgr.lost_output(&path).await,
            "a good write does not un-lose earlier output"
        );
        let read = mgr.read(&path, OutputOptions::default()).await.unwrap();
        assert!(
            read.content.ends_with("this one lands\n"),
            "got: {:?}",
            read.content
        );
    }

    /// A spool that never failed reports nothing lost — the flag must not be a
    /// constant `true` hiding behind two passing failure tests.
    #[tokio::test]
    async fn a_healthy_spool_reports_no_lost_output() {
        let (_fs, mgr) = manager();
        let path = mgr.allocate("bfail0003").await.unwrap();
        mgr.append(&path, "all good\n").await.unwrap();
        assert!(!mgr.lost_output(&path).await);
    }

    /// When the RETRY fails too the error surfaces to the caller — the oracle
    /// only swallows the first failure.
    #[tokio::test]
    async fn a_second_failure_is_reported_to_the_caller() {
        let (fs, mgr) = manager();
        let path = mgr.allocate("bfail0004").await.unwrap();
        fs.fail_next_appends(2);
        let err = mgr
            .append(&path, "gone\n")
            .await
            .expect_err("both attempts failed");
        assert!(matches!(err, OutputError::Io(_)), "got: {err:?}");
        assert!(mgr.lost_output(&path).await);
    }

    /// TOF-04: a full read never returns more than the last 8 MiB, and says how
    /// much it dropped. Without this a 5GB spool (which the WRITE cap happily
    /// allows) was materialised whole on its way to a caller that truncates to
    /// tens of thousands of characters.
    #[tokio::test]
    async fn a_full_read_keeps_the_last_8mb_and_announces_the_rest() {
        let (_fs, mgr) = manager();
        let path = mgr.allocate("btail0001").await.unwrap();
        // 8 MiB + 2 KiB, with a marker at each end.
        let over = 2 * 1024;
        let filler =
            "A".repeat(MAX_TASK_OUTPUT_READ_BYTES as usize + over - "HEAD".len() - "TAIL".len());
        mgr.append(&path, &format!("HEAD{filler}TAIL"))
            .await
            .unwrap();

        let read = mgr.read(&path, OutputOptions::default()).await.unwrap();
        assert!(
            read.content
                .starts_with("[2KB of earlier output omitted]\n"),
            "got: {:?}",
            &read.content[..60.min(read.content.len())]
        );
        assert!(!read.content.contains("HEAD"), "the head must be dropped");
        assert!(read.content.ends_with("TAIL"), "the tail must survive");
        assert_eq!(
            read.content.len(),
            "[2KB of earlier output omitted]\n".len() + MAX_TASK_OUTPUT_READ_BYTES as usize,
            "exactly the cap survives, plus the header"
        );
    }

    /// Under the cap the content is returned verbatim — no header.
    #[tokio::test]
    async fn a_full_read_under_the_cap_is_verbatim() {
        let (_fs, mgr) = manager();
        let path = mgr.allocate("btail0002").await.unwrap();
        mgr.append(&path, "small output\n").await.unwrap();
        let read = mgr.read(&path, OutputOptions::default()).await.unwrap();
        assert_eq!(read.content, "small output\n");
    }

    /// A WINDOWED read is the caller asking for a slice; the oracle's byte cap
    /// belongs to `getTaskOutput`, which takes no window, so the header must not
    /// appear there.
    #[tokio::test]
    async fn a_windowed_read_is_not_tail_capped() {
        let (_fs, mgr) = manager();
        let path = mgr.allocate("btail0003").await.unwrap();
        let line = "B".repeat(1024);
        let body: String = std::iter::repeat(line.as_str())
            .take((MAX_TASK_OUTPUT_READ_BYTES as usize / 1025) + 64)
            .collect::<Vec<_>>()
            .join("\n");
        mgr.append(&path, &body).await.unwrap();

        let read = mgr
            .read(
                &path,
                OutputOptions {
                    offset: Some(0),
                    limit: Some(2),
                },
            )
            .await
            .unwrap();
        assert!(
            !read.content.contains("of earlier output omitted"),
            "got: {:?}",
            &read.content[..60.min(read.content.len())]
        );
    }

    /// The cap counts UTF-16 code units (JS `String.length`), not UTF-8 bytes.
    /// A CJK character is 3 bytes but 1 unit, so a byte-counting port trips the
    /// 5GB cap almost three times early on a non-ASCII spool.
    #[tokio::test]
    async fn the_cap_counts_utf16_code_units_not_bytes() {
        let (_fs, mgr) = manager();
        let path = mgr.allocate("bu16000001").await.unwrap();
        // Two units short of the cap.
        mgr.seed_bytes_for_test(&path, MAX_TASK_OUTPUT_BYTES - 2)
            .await;

        // Two CJK characters = 6 UTF-8 bytes but only 2 units, so this lands
        // exactly ON the cap and is written verbatim.
        mgr.append(&path, "\u{4f60}\u{597d}").await.unwrap();
        let body = mgr
            .read(&path, OutputOptions::default())
            .await
            .unwrap()
            .content;
        assert!(
            body.contains('\u{4f60}'),
            "at the cap the chunk is written, got: {body:?}"
        );
        assert!(
            !body.contains("output truncated"),
            "a byte count would have capped here, got: {body:?}"
        );

        // One more unit crosses it.
        mgr.append(&path, "x").await.unwrap();
        let body = mgr
            .read(&path, OutputOptions::default())
            .await
            .unwrap()
            .content;
        assert!(body.contains("output truncated"), "got: {body:?}");
    }

    #[tokio::test]
    async fn append_caps_at_5gb_and_writes_marker_then_drops() {
        // T17: claude-code `DiskTaskOutput.append` caps a single spool at
        // MAX_TASK_OUTPUT_BYTES (5GB). The byte counter uses chunk length, so
        // we exercise the boundary with a pre-seeded counter instead of
        // materializing 5GB. The first chunk that crosses the cap writes the
        // EXACT marker `\n[output truncated: exceeded 5GB disk cap]\n`;
        // subsequent appends are dropped entirely.
        let (_fs, mgr) = manager();
        let path = mgr.allocate("bcap00001").await.unwrap();

        // Seed the counter one byte below the cap, then append two bytes — the
        // running total crosses MAX_TASK_OUTPUT_BYTES in a single append.
        mgr.seed_bytes_for_test(&path, MAX_TASK_OUTPUT_BYTES - 1)
            .await;
        mgr.append(&path, "ab").await.unwrap();

        // A further write must be dropped (the spool is now capped).
        mgr.append(&path, "this must be dropped\n").await.unwrap();

        let read = mgr.read(&path, OutputOptions::default()).await.unwrap();
        assert_eq!(
            read.content, "\n[output truncated: exceeded 5GB disk cap]\n",
            "the crossing append is REPLACED by the exact marker (the raw \"ab\" \
             is not written), and post-cap appends drop"
        );
        assert_eq!(
            MAX_TASK_OUTPUT_BYTES,
            5 * 1024 * 1024 * 1024,
            "cap constant is 5GB"
        );
        assert_eq!(MAX_TASK_OUTPUT_BYTES_DISPLAY, "5GB");
    }

    /// TOF-08: the recovery clause belongs to exactly ONE refusal. Every other
    /// reason ends at the path — telling a user whose output FILE changed
    /// identity to restart with a fresh temp directory addresses nothing.
    #[cfg(unix)]
    #[tokio::test]
    async fn only_the_moved_directory_refusal_carries_recovery_guidance() {
        let dir = tempfile::tempdir().unwrap();
        let manager = TaskOutputManager::new(dir.path().join("tasks"), ExclusiveFs::new());
        let plain = manager.swap_refused("not a regular nlink-1 file");
        let OutputError::SwapRefused(message) = plain else {
            panic!("expected SwapRefused");
        };
        assert!(
            message.starts_with("task output swap refused (not a regular nlink-1 file): "),
            "got: {message}"
        );
        assert!(!message.contains("To recover"), "got: {message}");
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn output_root_swap_is_refused_with_recovery_guidance() {
        let parent = tempfile::tempdir().unwrap();
        let output_dir = parent.path().join("tasks");
        std::fs::create_dir(&output_dir).unwrap();
        let victim = tempfile::tempdir().unwrap();
        let fs = ExclusiveFs::new();
        let manager = TaskOutputManager::new(output_dir.clone(), fs.clone());

        // The first operation pins the real task-output directory identity.
        let terminal_path = manager.allocate("bpin0001").await.unwrap();
        manager
            .replace_terminal_result_fail_closed(&terminal_path, "authoritative\n")
            .await
            .unwrap();
        std::fs::rename(&output_dir, parent.path().join("tasks-moved")).unwrap();
        std::os::unix::fs::symlink(victim.path(), &output_dir).unwrap();

        let error = manager
            .allocate("bpin0002")
            .await
            .expect_err("a moved/symlinked task directory must fail closed");
        let OutputError::SwapRefused(message) = error else {
            panic!("expected SwapRefused, got {error:?}");
        };
        // The one refusal that carries a recovery clause — and it NAMES the
        // env var, so the reader has something to set.
        assert!(
            message.starts_with("task output swap refused (tasks dir moved or linked): "),
            "got: {message}"
        );
        assert!(
            message.contains("restart with LINGXI_TMPDIR set to a fresh directory"),
            "got: {message}"
        );
        assert!(message.contains("remove that entry itself"));
        assert!(!victim.path().join("bpin0002.output").exists());
        assert!(!fs
            .files
            .lock()
            .await
            .contains_key(&output_dir.join("bpin0002.output").display().to_string()));

        let terminal = manager
            .read(&terminal_path, OutputOptions::default())
            .await
            .expect("the authoritative in-memory terminal result remains readable");
        assert_eq!(terminal.content, "authoritative\n");
        assert!(
            !terminal.physical_spool_authoritative,
            "a replaced output root makes the previously written path stale"
        );
        assert!(
            !manager
                .physical_output_is_authoritative(&terminal_path)
                .await
        );
    }

    #[tokio::test]
    async fn supervisor_sink_preserves_framed_bytes_and_rejects_wrong_task_identity() {
        use platform_api::BackgroundExitSink;
        let root = tempfile::tempdir().unwrap();
        let manager = Arc::new(TaskOutputManager::new(
            root.path().into(),
            ExclusiveFs::new(),
        ));
        let path = manager.allocate("bsuper001").await.unwrap();
        let sink = TaskOutputSink::new(manager.clone(), path.clone());
        assert!(sink.manages_output());
        assert!(sink
            .append_output("bother001", "must not be written")
            .await
            .is_err());
        sink.append_output("bsuper001", "stdout\n[stderr] problem\n")
            .await
            .unwrap();
        sink.flush_output("bsuper001").await.unwrap();
        assert_eq!(
            manager
                .read(&path, OutputOptions::default())
                .await
                .unwrap()
                .content,
            "stdout\n[stderr] problem\n"
        );
        sink.on_exit("bsuper001", Some(0)).await;
        sink.on_exit("bsuper001", Some(0)).await;
        assert_eq!(
            manager
                .read(&path, OutputOptions::default())
                .await
                .unwrap()
                .content
                .matches("[exited with code 0]")
                .count(),
            1
        );
        let killed_path = manager.allocate("bsuper002").await.unwrap();
        let killed = TaskOutputSink::new(manager.clone(), killed_path.clone());
        killed.on_exit_with_status("bsuper002", None, true).await;
        assert_eq!(
            manager
                .read(&killed_path, OutputOptions::default())
                .await
                .unwrap()
                .content,
            "\n[killed]\n"
        );
    }
}
