//! Opt-in physical transcript ownership for independently persisted Chats.
//!
//! This is deliberately separate from the ordinary, rewrite-capable writer.
//! The host creates these files only inside its dedicated AppAgent Chat store,
//! separate from the ordinary projects catalog and rewrite-capable writers.
//! Appends and captures use descriptors pinned to one physical file. A sealed
//! writer cannot acquire an unverified tail after its host commits success.

use super::*;
use lingxi_core::host::rooted_fs::{self, RootIdentity, RootedFileLock};
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use std::io::{Read as _, Write as _};

/// Bound memory and work when verifying an independently persisted Chat.
const MAX_CHAT_TRANSCRIPT_BYTES: u64 = 64 * 1024 * 1024;

/// Host-private evidence for an exact transcript revision. This contains no
/// browser-controlled path or model/tool input.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct ChatTranscriptCheckpoint {
    /// Length of the complete captured transcript, including final newline.
    pub byte_len: u64,
    /// SHA-256 of those exact UTF-8 bytes.
    pub sha256: String,
}

/// One immutable capture read and synchronized under a controlled writer.
/// Only the writer can construct this value. Engine validation must replay
/// this source directly instead of reopening its path.
#[derive(Clone, Debug)]
pub struct CapturedChatTranscript {
    path: PathBuf,
    session_id: uuid::Uuid,
    source: String,
    checkpoint: ChatTranscriptCheckpoint,
}

impl CapturedChatTranscript {
    /// Exact synchronized source, suitable for `route_lines` and pure replay.
    pub fn source(&self) -> &str {
        &self.source
    }

    /// The physically pinned transcript's host-owned path.
    pub fn path(&self) -> &Path {
        &self.path
    }

    /// Canonical session identity required by this writer's file name.
    pub fn session_id(&self) -> uuid::Uuid {
        self.session_id
    }

    /// Evidence to persist in the host's task authority after validation.
    pub fn checkpoint(&self) -> &ChatTranscriptCheckpoint {
        &self.checkpoint
    }
}

pub(super) struct PersistentChatWriter {
    path: PathBuf,
    root: PathBuf,
    relative: PathBuf,
    root_identity: RootIdentity,
    file_identity: RootIdentity,
    append: std::fs::File,
    session_id: uuid::Uuid,
    checkpoint: Option<ChatTranscriptCheckpoint>,
    sealed: bool,
    poisoned: bool,
    #[cfg(test)]
    identity_test_gate: Option<IdentityIoTestGate>,
    #[cfg(test)]
    append_test_gate: Option<IdentityIoTestGate>,
    // This is the SAME sidecar used by ordinary non-durable JsonlWriters.
    // Keep it until the writer is dropped, including after terminal sealing.
    lease: RootedFileLock,
}

fn unavailable(reason: &str) -> WriterError {
    FsError::Io(format!("persistent Chat transcript unavailable: {reason}")).into()
}

fn checkpoint(source: &[u8]) -> ChatTranscriptCheckpoint {
    ChatTranscriptCheckpoint {
        byte_len: source.len() as u64,
        sha256: format!("{:x}", Sha256::digest(source)),
    }
}

/// Cancelled async callers release the public append mutex. Keep strict
/// identity I/O serialized with physical appends/captures in the worker too.
pub(super) fn identity_io_guard<'a>(
    owner: &'a Option<Arc<std::sync::Mutex<PersistentChatWriter>>>,
    path: &Path,
) -> Result<Option<std::sync::MutexGuard<'a, PersistentChatWriter>>, FsError> {
    let Some(owner) = owner else {
        return Ok(None);
    };
    let state = owner
        .lock()
        .map_err(|_| FsError::Io("poisoned strict identity owner".into()))?;
    if state.sealed || path != state.path {
        return Err(FsError::Io(
            "strict identity writer was sealed or retargeted".into(),
        ));
    }
    state
        .check_binding()
        .map_err(|error| FsError::Io(error.to_string()))?;
    Ok(Some(state))
}

fn validate_dedicated_path(path: &Path) -> Result<(), WriterError> {
    let text = path
        .to_str()
        .ok_or_else(|| unavailable("non-UTF-8 Chat path"))?;
    if !path.is_absolute()
        || text
            .split(['/', '\\'])
            .any(|part| matches!(part, "." | ".."))
    {
        return Err(unavailable("Chat path is not an absolute dedicated path"));
    }
    let chat = path
        .parent()
        .ok_or_else(|| unavailable("missing Chat directory"))?;
    let chat_id = chat
        .file_name()
        .and_then(|part| part.to_str())
        .ok_or_else(|| unavailable("missing Chat identity"))?;
    if !uuid::Uuid::parse_str(chat_id).is_ok_and(|id| id.to_string() == chat_id) {
        return Err(unavailable("noncanonical Chat directory"));
    }
    let chats = chat
        .parent()
        .ok_or_else(|| unavailable("missing chats directory"))?;
    let store = chats
        .parent()
        .ok_or_else(|| unavailable("missing private store"))?;
    let agent = store
        .parent()
        .ok_or_else(|| unavailable("missing Agent directory"))?;
    if chats.file_name() != Some(std::ffi::OsStr::new("chats"))
        || store.file_name() != Some(std::ffi::OsStr::new("agui-v1"))
        || agent.file_name() != Some(std::ffi::OsStr::new("agent"))
    {
        return Err(unavailable(
            "transcript is outside the dedicated AppAgent Chat layout",
        ));
    }
    Ok(())
}

impl PersistentChatWriter {
    fn open(
        path: PathBuf,
        expected: Option<ChatTranscriptCheckpoint>,
    ) -> Result<Self, WriterError> {
        validate_dedicated_path(&path)?;
        if !path.is_absolute() || path.extension().and_then(|ext| ext.to_str()) != Some("jsonl") {
            return Err(unavailable("invalid physical transcript path"));
        }
        let stem = path
            .file_stem()
            .and_then(|stem| stem.to_str())
            .ok_or_else(|| unavailable("invalid transcript session identity"))?;
        let session_id = uuid::Uuid::parse_str(stem)
            .ok()
            .filter(|id| id.to_string() == stem)
            .ok_or_else(|| unavailable("invalid transcript session identity"))?;
        let root = path
            .parent()
            .ok_or_else(|| unavailable("missing parent"))?
            .to_path_buf();
        let relative = PathBuf::from(
            path.file_name()
                .ok_or_else(|| unavailable("missing file name"))?,
        );
        let root_identity = rooted_fs::root_identity(&root)?;
        let lease = rooted_fs::try_lock_exclusive(
            &root,
            &message_identity::lock_path(&path)?,
            rooted_fs::PRIVATE_DIR_MODE,
            rooted_fs::PRIVATE_FILE_MODE,
        )?;
        if rooted_fs::root_identity(&root)? != root_identity {
            return Err(unavailable("parent changed while acquiring ownership"));
        }
        // Never create a replacement for an expected, missing transcript.
        match rooted_fs::open_read_file_pinned(&root, &relative, Some(&root_identity)) {
            Ok(file) => {
                if expected.is_none()
                    && file
                        .metadata()
                        .map_err(|e| FsError::Io(e.to_string()))?
                        .len()
                        != 0
                {
                    return Err(unavailable(
                        "nonempty transcript requires committed evidence",
                    ));
                }
            }
            Err(FsError::NotFound(_)) if expected.is_none() => {}
            Err(error) => return Err(error.into()),
        }
        let append = rooted_fs::open_append_file_pinned(&root, &relative, Some(&root_identity))?;
        if expected.is_none()
            && append
                .metadata()
                .map_err(|e| FsError::Io(e.to_string()))?
                .len()
                != 0
        {
            return Err(unavailable("fresh transcript changed while opening"));
        }
        rooted_fs::validate_file_single_link(&root, &relative)?;
        let file_identity = rooted_fs::opened_file_identity(&append)?;
        let mut writer = Self {
            path,
            root,
            relative,
            root_identity,
            file_identity,
            append,
            session_id,
            checkpoint: None,
            sealed: false,
            poisoned: false,
            #[cfg(test)]
            identity_test_gate: None,
            #[cfg(test)]
            append_test_gate: None,
            lease,
        };
        if let Some(expected) = expected {
            if expected.byte_len == 0 {
                return Err(unavailable("empty committed checkpoint"));
            }
            let source = writer.read_source()?;
            if checkpoint(source.as_bytes()) != expected {
                return Err(unavailable("committed transcript revision does not match"));
            }
            writer.checkpoint = Some(expected);
        }
        writer.check_binding()?;
        Ok(writer)
    }

    fn check_binding(&self) -> Result<(), WriterError> {
        if self.poisoned {
            return Err(unavailable("writer was fenced"));
        }
        let file = rooted_fs::open_recovery_file(
            &self.root,
            &self.relative,
            &self.root_identity,
            &self.file_identity,
        )?;
        if rooted_fs::opened_file_identity(&file)? != self.file_identity {
            return Err(unavailable("transcript identity changed"));
        }
        rooted_fs::validate_file_single_link(&self.root, &self.relative)?;
        Ok(())
    }

    fn read_source(&self) -> Result<String, WriterError> {
        self.check_binding()?;
        let file = rooted_fs::open_recovery_file(
            &self.root,
            &self.relative,
            &self.root_identity,
            &self.file_identity,
        )?;
        let before = file
            .metadata()
            .map_err(|error| FsError::Io(error.to_string()))?
            .len();
        if before > MAX_CHAT_TRANSCRIPT_BYTES {
            return Err(unavailable("transcript exceeds checkpoint limit"));
        }
        let mut bytes = Vec::new();
        (&file)
            .take(MAX_CHAT_TRANSCRIPT_BYTES + 1)
            .read_to_end(&mut bytes)
            .map_err(|error| FsError::Io(error.to_string()))?;
        let after = file
            .metadata()
            .map_err(|error| FsError::Io(error.to_string()))?
            .len();
        if before != after || after != bytes.len() as u64 {
            return Err(unavailable("transcript changed during capture"));
        }
        if let Some(previous) = &self.checkpoint {
            let prefix = usize::try_from(previous.byte_len)
                .ok()
                .and_then(|length| bytes.get(..length))
                .ok_or_else(|| unavailable("committed prefix was truncated"))?;
            if checkpoint(prefix) != *previous {
                return Err(unavailable("committed prefix changed"));
            }
        }
        String::from_utf8(bytes).map_err(|_| unavailable("transcript is not UTF-8"))
    }

    fn append(&mut self, path: &Path, payload: &str) -> Result<(), WriterError> {
        #[cfg(test)]
        if let Some(gate) = self.append_test_gate.take() {
            gate.wait();
        }
        let result = (|| {
            if self.sealed || path != self.path || !payload.ends_with('\n') {
                return Err(unavailable(
                    "sealed writer, changed target, or incomplete record",
                ));
            }
            self.check_binding()?;
            if self
                .append
                .metadata()
                .map_err(|e| FsError::Io(e.to_string()))?
                .len()
                .saturating_add(payload.len() as u64)
                > MAX_CHAT_TRANSCRIPT_BYTES
            {
                return Err(unavailable("transcript exceeds checkpoint limit"));
            }
            self.append
                .write_all(payload.as_bytes())
                .map_err(|error| FsError::Io(error.to_string()))?;
            self.check_binding()
        })();
        if result.is_err() {
            self.poisoned = true;
        }
        result
    }

    fn capture(&mut self, seal: bool) -> Result<CapturedChatTranscript, WriterError> {
        // Seal before any fallible work: a failed terminal checkpoint must not
        // leave a writable authority behind for a late callback.
        self.sealed |= seal;
        let captured = (|| {
            let source = self.read_source()?;
            if source.is_empty() || !source.ends_with('\n') {
                return Err(unavailable("empty transcript or incomplete final record"));
            }
            self.append
                .sync_all()
                .map_err(|error| FsError::Io(error.to_string()))?;
            rooted_fs::sync_parent_pinned(&self.root, &self.relative, Some(&self.root_identity))?;
            self.check_binding()?;
            let proof = checkpoint(source.as_bytes());
            self.checkpoint = Some(proof.clone());
            Ok(CapturedChatTranscript {
                path: self.path.clone(),
                session_id: self.session_id,
                source,
                checkpoint: proof,
            })
        })();
        if captured.is_err() {
            self.poisoned = true;
        }
        captured
    }
}

struct HeldChatIdentityGuard {
    path: String,
    // Identity work may outlive the awaiting writer. Retain the actual OS
    // lease through that worker, rather than only pretending it is held.
    _owner: Arc<std::sync::Mutex<PersistentChatWriter>>,
}
impl FlockGuard for HeldChatIdentityGuard {
    fn path(&self) -> &str {
        &self.path
    }
}

#[cfg(test)]
pub(super) struct IdentityIoTestGate {
    pub entered: tokio::sync::oneshot::Sender<()>,
    pub resume: Arc<std::sync::Barrier>,
}

#[cfg(test)]
impl IdentityIoTestGate {
    pub fn wait(self) {
        let _ = self.entered.send(());
        self.resume.wait();
    }
}

impl JsonlWriter {
    /// Open an exclusively owned physical Chat transcript. The parent must
    /// already exist, under `agent/agui-v1/chats/<canonical Chat UUID>/`.
    /// `None` accepts only an empty/new file; recovery requires
    /// exact complete-file evidence from the host ledger. No suffix is adopted.
    ///
    /// This opt-in API bypasses virtual filesystem appends and supports only
    /// root-confined physical files. Share this exact writer with the resumed
    /// orchestrator; opening a second writer fails while its lease is alive.
    pub async fn open_persistent_chat(
        path: PathBuf,
        fs: Arc<dyn FileSystem>,
        expected: Option<ChatTranscriptCheckpoint>,
    ) -> Result<Self, WriterError> {
        let state_path = path.clone();
        let state =
            tokio::task::spawn_blocking(move || PersistentChatWriter::open(state_path, expected))
                .await
                .map_err(|error| unavailable(&error.to_string()))??;
        let mut writer = Self::new(path, fs);
        writer.persistent_chat = Some(Arc::new(std::sync::Mutex::new(state)));
        Ok(writer)
    }

    /// Whether this writer owns the dedicated, physically pinned Chat path.
    pub fn is_persistent_chat(&self) -> bool {
        self.persistent_chat.is_some()
    }

    /// Synchronize and capture one revision while holding the writer's append
    /// mutex. With `seal`, every subsequent append fails, including metadata.
    /// This proves bytes only; use the orchestrator checkpoint API to prove
    /// that those bytes reconstruct the live model history.
    pub async fn capture_persistent_chat_snapshot(
        &self,
        seal: bool,
    ) -> Result<CapturedChatTranscript, WriterError> {
        let _guard = self.lock.lock().await;
        let state = self
            .persistent_chat
            .clone()
            .ok_or_else(|| unavailable("uncontrolled writer"))?;
        tokio::task::spawn_blocking(move || {
            let mut state = state
                .lock()
                .map_err(|_| unavailable("poisoned writer lock"))?;
            state.capture(seal)
        })
        .await
        .map_err(|error| unavailable(&error.to_string()))?
    }

    /// Permanently close the controlled writer after a failed engine check.
    pub async fn seal_persistent_chat(&self) -> Result<(), WriterError> {
        let _guard = self.lock.lock().await;
        let state = self
            .persistent_chat
            .as_ref()
            .ok_or_else(|| unavailable("uncontrolled writer"))?;
        state
            .lock()
            .map_err(|_| unavailable("poisoned writer lock"))?
            .sealed = true;
        Ok(())
    }

    pub(super) fn reject_persistent_chat_mutation(
        &self,
        operation: &str,
    ) -> Result<(), WriterError> {
        if let Some(state) = &self.persistent_chat {
            state
                .lock()
                .map_err(|_| unavailable("poisoned writer lock"))?
                .poisoned = true;
            return Err(unavailable(operation));
        }
        Ok(())
    }

    pub(super) fn persistent_chat_identity_guard(
        &self,
        path: &Path,
    ) -> Result<Option<Box<dyn FlockGuard>>, WriterError> {
        let Some(owner) = &self.persistent_chat else {
            return Ok(None);
        };
        let state = owner
            .lock()
            .map_err(|_| unavailable("poisoned writer lock"))?;
        if state.sealed || path != state.path {
            return Err(unavailable("foreign identity target"));
        }
        state.check_binding()?;
        // The writer owns the actual OS guard for its entire lifetime. Do not
        // reacquire that guard on each append and deadlock against ourselves.
        Ok(Some(Box::new(HeldChatIdentityGuard {
            path: state.lease.path().to_owned(),
            _owner: owner.clone(),
        })))
    }

    #[cfg(test)]
    pub(super) fn take_persistent_chat_identity_test_gate(&self) -> Option<IdentityIoTestGate> {
        self.persistent_chat
            .as_ref()?
            .lock()
            .ok()?
            .identity_test_gate
            .take()
    }

    pub(super) async fn append_persistent_chat_payload(
        &self,
        path: &Path,
        payload: &str,
    ) -> Result<bool, WriterError> {
        let Some(state) = self.persistent_chat.clone() else {
            return Ok(false);
        };
        let path = path.to_path_buf();
        let payload = payload.to_owned();
        tokio::task::spawn_blocking(move || {
            let mut state = state
                .lock()
                .map_err(|_| unavailable("poisoned writer lock"))?;
            state.append(&path, &payload)
        })
        .await
        .map_err(|error| unavailable(&error.to_string()))??;
        Ok(true)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn fixture() -> (tempfile::TempDir, PathBuf, Arc<dyn FileSystem>) {
        let root = tempfile::tempdir().unwrap();
        let directory = root
            .path()
            .join("apps/test-app/agent/agui-v1/chats")
            .join(uuid::Uuid::new_v4().to_string());
        std::fs::create_dir_all(&directory).unwrap();
        let path = directory.join(format!("{}.jsonl", uuid::Uuid::new_v4()));
        let fs = Arc::new(platform_posix::fs::PosixFileSystem::new(
            root.path().to_path_buf(),
        ));
        (root, path, fs)
    }

    async fn append_user(writer: &JsonlWriter, text: &str) {
        let session_id = writer.path.file_stem().unwrap().to_str().unwrap();
        let row: JsonlMessage = serde_json::from_value(json!({
            "type":"user", "uuid":uuid::Uuid::new_v4(), "parentUuid":null,
            "sessionId":session_id, "isSidechain":false,
            "timestamp":"2026-10-28T00:00:00.000Z", "cwd":"/chat", "version":"test",
            "message":{"role":"user","content":text}
        }))
        .unwrap();
        writer.append(&row).await.unwrap();
    }

    #[tokio::test]
    async fn capture_is_exact_and_terminal_seal_rejects_late_metadata() {
        let (_root, path, fs) = fixture();
        let writer = JsonlWriter::open_persistent_chat(path.clone(), fs, None)
            .await
            .unwrap();
        append_user(&writer, "hello").await;
        let captured = writer.capture_persistent_chat_snapshot(true).await.unwrap();
        assert_eq!(captured.source().as_bytes(), std::fs::read(&path).unwrap());
        assert_eq!(
            captured.checkpoint(),
            &checkpoint(captured.source().as_bytes())
        );
        assert!(writer
            .append_custom_title(
                &captured.session_id().to_string(),
                Utf16JsonProjection::from("late")
            )
            .await
            .is_err());
        assert_eq!(captured.source().as_bytes(), std::fs::read(&path).unwrap());
    }

    #[tokio::test]
    async fn ownership_excludes_a_second_dedicated_writer() {
        let (_root, path, fs) = fixture();
        let writer = JsonlWriter::open_persistent_chat(path.clone(), fs.clone(), None)
            .await
            .unwrap();
        append_user(&writer, "exclusive").await;
        let captured = writer
            .capture_persistent_chat_snapshot(false)
            .await
            .unwrap();
        assert!(JsonlWriter::open_persistent_chat(
            path.clone(),
            fs.clone(),
            Some(captured.checkpoint().clone())
        )
        .await
        .is_err());
        assert_eq!(captured.source().as_bytes(), std::fs::read(&path).unwrap());
        drop(writer);
        assert!(
            JsonlWriter::open_persistent_chat(path, fs, Some(captured.checkpoint().clone()))
                .await
                .is_ok()
        );
    }

    #[tokio::test]
    async fn constructor_rejects_ordinary_and_escaping_paths() {
        let (root, path, fs) = fixture();
        let ordinary = root.path().join(format!("{}.jsonl", uuid::Uuid::new_v4()));
        assert!(
            JsonlWriter::open_persistent_chat(ordinary.clone(), fs.clone(), None)
                .await
                .is_err()
        );
        assert!(!ordinary.exists());
        let escaped = path
            .parent()
            .unwrap()
            .join("..")
            .join(path.file_name().unwrap());
        assert!(JsonlWriter::open_persistent_chat(escaped, fs, None)
            .await
            .is_err());
        assert!(!path.exists());
    }

    #[test]
    fn cancelled_identity_worker_keeps_the_real_lease_until_io_finishes() {
        let runtime = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .max_blocking_threads(1)
            .build()
            .unwrap();
        runtime.block_on(async {
            let (_root, path, fs) = fixture();
            let writer = Arc::new(
                JsonlWriter::open_persistent_chat(path.clone(), fs.clone(), None)
                    .await
                    .unwrap(),
            );
            append_user(&writer, "committed prefix").await;
            let captured = writer
                .capture_persistent_chat_snapshot(false)
                .await
                .unwrap();
            let state = writer.persistent_chat.as_ref().unwrap();
            let weak = Arc::downgrade(state);
            let (entered_tx, entered_rx) = tokio::sync::oneshot::channel();
            let resume = Arc::new(std::sync::Barrier::new(2));
            state.lock().unwrap().identity_test_gate = Some(IdentityIoTestGate {
                entered: entered_tx,
                resume: resume.clone(),
            });
            let pending_writer = writer.clone();
            let pending = tokio::spawn(async move {
                append_user(&pending_writer, "cancelled before transcript append").await;
            });
            entered_rx.await.unwrap();
            pending.abort();
            assert!(pending.await.unwrap_err().is_cancelled());
            drop(writer);
            assert!(
                weak.upgrade().is_some(),
                "the worker lost the real OS lease owner"
            );
            // The only blocking worker is parked before identity I/O. A
            // synchronous try-open proves file ownership independently of
            // the pool queue, without starting an uncancellable lock wait.
            let competing =
                PersistentChatWriter::open(path.clone(), Some(captured.checkpoint().clone()));
            let excluded = competing.is_err();
            drop(competing);
            resume.wait();
            tokio::time::timeout(std::time::Duration::from_secs(2), async {
                while weak.strong_count() != 0 {
                    tokio::task::yield_now().await;
                }
            })
            .await
            .unwrap();
            let recovered = JsonlWriter::open_persistent_chat(
                path.clone(),
                fs,
                Some(captured.checkpoint().clone()),
            )
            .await
            .unwrap();
            assert!(excluded);
            assert_eq!(
                recovered
                    .capture_persistent_chat_snapshot(false)
                    .await
                    .unwrap()
                    .source(),
                captured.source()
            );
        });
    }

    #[test]
    fn cancelled_transcript_worker_keeps_the_lease_and_rejects_its_unknown_tail() {
        let runtime = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .max_blocking_threads(1)
            .build()
            .unwrap();
        runtime.block_on(async {
            let (_root, path, fs) = fixture();
            let writer = Arc::new(
                JsonlWriter::open_persistent_chat(path.clone(), fs.clone(), None)
                    .await
                    .unwrap(),
            );
            append_user(&writer, "committed prefix").await;
            let captured = writer
                .capture_persistent_chat_snapshot(false)
                .await
                .unwrap();
            let state = writer.persistent_chat.as_ref().unwrap();
            let weak = Arc::downgrade(state);
            let (entered_tx, entered_rx) = tokio::sync::oneshot::channel();
            let resume = Arc::new(std::sync::Barrier::new(2));
            state.lock().unwrap().append_test_gate = Some(IdentityIoTestGate {
                entered: entered_tx,
                resume: resume.clone(),
            });
            let pending_writer = writer.clone();
            let pending = tokio::spawn(async move {
                append_user(&pending_writer, "uncommitted tail").await;
            });
            entered_rx.await.unwrap();
            pending.abort();
            assert!(pending.await.unwrap_err().is_cancelled());
            drop(writer);
            let competing =
                PersistentChatWriter::open(path.clone(), Some(captured.checkpoint().clone()));
            let excluded = competing.is_err();
            drop(competing);
            let before_io = std::fs::read(&path).unwrap();
            resume.wait();
            tokio::time::timeout(std::time::Duration::from_secs(2), async {
                while weak.strong_count() != 0 {
                    tokio::task::yield_now().await;
                }
            })
            .await
            .unwrap();
            assert!(excluded);
            assert_eq!(before_io, captured.source().as_bytes());
            let after_io = std::fs::read_to_string(&path).unwrap();
            assert!(after_io.starts_with(captured.source()));
            assert!(after_io.contains("uncommitted tail"));
            assert!(JsonlWriter::open_persistent_chat(
                path,
                fs,
                Some(captured.checkpoint().clone())
            )
            .await
            .is_err());
        });
    }

    #[tokio::test]
    async fn recovery_rejects_unknown_suffix_and_preserves_its_bytes() {
        let (_root, path, fs) = fixture();
        let writer = JsonlWriter::open_persistent_chat(path.clone(), fs.clone(), None)
            .await
            .unwrap();
        append_user(&writer, "committed").await;
        let captured = writer.capture_persistent_chat_snapshot(true).await.unwrap();
        drop(writer);
        let mut file = std::fs::OpenOptions::new()
            .append(true)
            .open(&path)
            .unwrap();
        file.write_all(b"{\"type\":\"unknown-tail\"}\n").unwrap();
        drop(file);
        let evidence = std::fs::read(&path).unwrap();
        assert!(JsonlWriter::open_persistent_chat(
            path.clone(),
            fs,
            Some(captured.checkpoint().clone())
        )
        .await
        .is_err());
        assert_eq!(std::fs::read(path).unwrap(), evidence);
    }

    #[tokio::test]
    async fn removal_and_retarget_cannot_change_committed_bytes() {
        for mutation in ["tombstone", "retry", "retarget"] {
            let (_root, path, fs) = fixture();
            let writer = JsonlWriter::open_persistent_chat(path.clone(), fs, None)
                .await
                .unwrap();
            append_user(&writer, "preserve").await;
            let captured = writer
                .capture_persistent_chat_snapshot(false)
                .await
                .unwrap();
            match mutation {
                "tombstone" => assert!(writer.remove_message_by_uuid("unused").await.is_err()),
                "retry" => assert!(writer.remove_retry_attempt("unused").await.is_err()),
                _ => writer.retarget(path.with_extension("other.jsonl")).await,
            }
            assert_eq!(std::fs::read(&path).unwrap(), captured.source().as_bytes());
            assert!(writer
                .capture_persistent_chat_snapshot(false)
                .await
                .is_err());
        }
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn replacement_file_is_not_written_or_adopted() {
        let (_root, path, fs) = fixture();
        let writer = JsonlWriter::open_persistent_chat(path.clone(), fs, None)
            .await
            .unwrap();
        append_user(&writer, "original").await;
        let old = path.with_extension("original");
        std::fs::rename(&path, &old).unwrap();
        std::fs::write(&path, b"replacement\n").unwrap();
        assert!(writer.capture_persistent_chat_snapshot(true).await.is_err());
        assert_eq!(std::fs::read(&path).unwrap(), b"replacement\n");
        assert!(std::fs::read_to_string(old).unwrap().contains("original"));
    }
}
