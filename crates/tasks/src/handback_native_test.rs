//! Native root/inode swaps and a torn append inside a UTF-8 code point.
use super::*;
use lingxi_core::host::rooted_fs::{
    self, JournalCursor, JournalRead, JournalWriteError, RootIdentity,
};
use std::io::Write;
use std::path::PathBuf;

#[derive(Default)]
struct NativeJournalFs {
    unused: JournalFs,
    swap_before_append: AtomicBool,
    swap_before_truncate: AtomicBool,
    swaps: AtomicUsize,
}

fn native_error(error: impl std::fmt::Display) -> FsError {
    FsError::Io(error.to_string())
}

fn retained_root(root: &Path) -> PathBuf {
    root.with_extension("retained")
}

impl NativeJournalFs {
    fn replace_root(&self, root: &Path, relative: &Path) -> Result<(), FsError> {
        std::fs::rename(root, retained_root(root)).map_err(native_error)?;
        std::fs::create_dir(root).map_err(native_error)?;
        std::fs::write(root.join(relative), b"replacement journal must survive\n")
            .map_err(native_error)?;
        self.swaps.fetch_add(1, Ordering::AcqRel);
        Ok(())
    }
}

#[async_trait]
impl FileSystem for NativeJournalFs {
    // Only the new pinned APIs below operate on native files. Falling back
    // to a legacy API uses the empty mock and makes these regressions fail.
    async fn read_file(
        &self,
        path: &str,
        offset: Option<u64>,
        limit: Option<u64>,
    ) -> Result<FileContent, FsError> {
        self.unused.read_file(path, offset, limit).await
    }
    async fn write_file(&self, path: &str, content: &str) -> Result<(), FsError> {
        self.unused.write_file(path, content).await
    }
    fn is_within_workspace(&self, _: &str) -> bool {
        true
    }
    async fn watch(
        &self,
        path: &str,
    ) -> Result<Pin<Box<dyn futures::Stream<Item = FileEvent> + Send>>, FsError> {
        self.unused.watch(path).await
    }
    async fn append_file(&self, path: &str, content: &str) -> Result<(), FsError> {
        self.unused.append_file(path, content).await
    }
    async fn truncate(&self, path: &str, len: u64) -> Result<(), FsError> {
        self.unused.truncate(path, len).await
    }
    async fn file_mtime(&self, path: &str) -> Result<SystemTime, FsError> {
        self.unused.file_mtime(path).await
    }
    async fn file_size(&self, path: &str) -> Result<u64, FsError> {
        self.unused.file_size(path).await
    }
    async fn delete_file(&self, path: &str) -> Result<(), FsError> {
        self.unused.delete_file(path).await
    }
    async fn symlink(&self, from: &str, to: &str) -> Result<(), FsError> {
        self.unused.symlink(from, to).await
    }
    async fn flock_exclusive(&self, path: &str) -> Result<Box<dyn FlockGuard>, FsError> {
        self.unused.flock_exclusive(path).await
    }
    async fn fsync(&self, path: &str) -> Result<(), FsError> {
        self.unused.fsync(path).await
    }

    async fn root_identity_no_follow(&self, root: &Path) -> Result<Option<RootIdentity>, FsError> {
        rooted_fs::root_identity(root).map(Some)
    }

    async fn write_file_rooted_atomic_pinned(
        &self,
        root: &Path,
        relative: &Path,
        content: &str,
        expected: Option<&RootIdentity>,
    ) -> Result<(), FsError> {
        rooted_fs::atomic_write_pinned(
            root,
            relative,
            content.as_bytes(),
            rooted_fs::AtomicWriteOptions::default(),
            expected.expect("native inbox retains its root identity"),
        )
    }

    async fn read_file_rooted_journal_pinned(
        &self,
        root: &Path,
        relative: &Path,
        expected: Option<&RootIdentity>,
    ) -> Result<JournalRead, FsError> {
        rooted_fs::read_journal_pinned(root, relative, expected)
    }

    async fn append_file_rooted_journal_durable(
        &self,
        root: &Path,
        relative: &Path,
        expected: &JournalCursor,
        content: &str,
    ) -> Result<JournalCursor, JournalWriteError> {
        if self.swap_before_append.swap(false, Ordering::AcqRel) {
            self.replace_root(root, relative)
                .map_err(|error| JournalWriteError {
                    error,
                    uncertain: false,
                    recovered: Some(*expected),
                })?;
        }
        rooted_fs::append_journal_durable(root, relative, expected, content)
    }

    async fn truncate_file_rooted_journal_durable(
        &self,
        root: &Path,
        relative: &Path,
        expected: &JournalCursor,
        length: u64,
    ) -> Result<JournalCursor, JournalWriteError> {
        if self.swap_before_truncate.swap(false, Ordering::AcqRel) {
            self.replace_root(root, relative)
                .map_err(|error| JournalWriteError {
                    error,
                    uncertain: false,
                    recovered: Some(*expected),
                })?;
        }
        rooted_fs::truncate_journal_durable(root, relative, expected, length)
    }

    async fn sync_file_rooted_journal_durable(
        &self,
        root: &Path,
        relative: &Path,
        expected: &JournalCursor,
    ) -> Result<(), FsError> {
        rooted_fs::sync_journal_durable(root, relative, expected)
    }
}

fn native_root() -> (tempfile::TempDir, PathBuf) {
    let dir = tempfile::tempdir().unwrap();
    let root = dir.path().join("inbox");
    std::fs::create_dir(&root).unwrap();
    (dir, root)
}

#[tokio::test]
async fn handback_native_root_swap_before_first_or_hot_append_never_writes_replacement() {
    for hot in [false, true] {
        let (_dir, root) = native_root();
        let fs = Arc::new(NativeJournalFs::default());
        let scope = scope();
        let caller = AgentId::new();
        let sender = AgentId::new();
        let (registry, _owner) = make_registry(&root, fs.clone(), scope);
        add_local(&registry, "caller", caller).await;
        let first = peer_envelope(scope, caller, sender, 1, MessageId::new());
        if hot {
            registry.send_peer("caller", first.clone()).await.unwrap();
        }
        fs.swap_before_append.store(true, Ordering::Release);
        let next = peer_envelope(scope, caller, sender, 2, MessageId::new());
        assert!(registry.send_peer("caller", next).await.is_err());
        assert_eq!(fs.swaps.load(Ordering::Acquire), 1);
        assert_eq!(
            std::fs::read(inbox_path(&root, scope, caller, "jsonl")).unwrap(),
            b"replacement journal must survive\n"
        );
        assert!(!Path::new(&inbox_path(&root, scope, caller, "json")).exists());
        assert_eq!(
            registry.pending_handback_reports_for(caller).await,
            if hot { vec![first] } else { Vec::new() }
        );
        if hot {
            let (cold, _owner) = make_registry(&retained_root(&root), fs.clone(), scope);
            add_local(&cold, "caller", caller).await;
            restore_actor(&cold, caller, scope).await.unwrap();
            assert_eq!(cold.pending_handback_reports_for(caller).await.len(), 1);
        }
    }
}

#[tokio::test]
async fn handback_native_replaced_journal_inode_is_not_appended_to() {
    let (_dir, root) = native_root();
    let fs = Arc::new(NativeJournalFs::default());
    let scope = scope();
    let caller = AgentId::new();
    let sender = AgentId::new();
    let (registry, _owner) = make_registry(&root, fs, scope);
    add_local(&registry, "caller", caller).await;
    let first = peer_envelope(scope, caller, sender, 1, MessageId::new());
    registry.send_peer("caller", first.clone()).await.unwrap();
    let path = PathBuf::from(inbox_path(&root, scope, caller, "jsonl"));
    let original = path.with_extension("saved");
    std::fs::rename(&path, &original).unwrap();
    std::fs::write(&path, b"unrelated replacement\n").unwrap();
    let original_bytes = std::fs::read(&original).unwrap();
    let second = peer_envelope(scope, caller, sender, 2, MessageId::new());
    assert!(registry.send_peer("caller", second).await.is_err());
    assert_eq!(std::fs::read(&path).unwrap(), b"unrelated replacement\n");
    assert_eq!(std::fs::read(original).unwrap(), original_bytes);
    assert_eq!(
        registry.pending_handback_reports_for(caller).await,
        vec![first]
    );
}

#[tokio::test]
async fn handback_native_torn_utf8_suffix_recovers_but_complete_utf8_corruption_is_preserved() {
    let (_dir, root) = native_root();
    let fs = Arc::new(NativeJournalFs::default());
    let scope = scope();
    let caller = AgentId::new();
    let sender = AgentId::new();
    let (registry, _owner) = make_registry(&root, fs.clone(), scope);
    add_local(&registry, "caller", caller).await;
    let envelope = peer_envelope(scope, caller, sender, 1, MessageId::new());
    registry
        .send_peer("caller", envelope.clone())
        .await
        .unwrap();
    let path = inbox_path(&root, scope, caller, "jsonl");
    let complete_bytes = std::fs::metadata(&path).unwrap().len();
    let mut file = std::fs::OpenOptions::new()
        .append(true)
        .open(&path)
        .unwrap();
    file.write_all(b"{\"incomplete\":\"\xf0\x9f").unwrap();
    file.sync_all().unwrap();
    assert!(
        std::fs::read_to_string(&path).is_err(),
        "suffix splits a UTF-8 code point"
    );
    let (cold, _owner) = make_registry(&root, fs.clone(), scope);
    add_local(&cold, "caller", caller).await;
    restore_actor(&cold, caller, scope).await.unwrap();
    assert_eq!(std::fs::metadata(&path).unwrap().len(), complete_bytes);
    assert_eq!(
        cold.pending_handback_reports_for(caller).await,
        vec![envelope.clone()]
    );
    assert!(
        cold.acknowledge_handback_consumption(caller, &envelope.receipt)
            .await
    );
    let mut file = std::fs::OpenOptions::new()
        .append(true)
        .open(&path)
        .unwrap();
    file.write_all(b"{\"complete\":\"\xf0\x9f\"}\n").unwrap();
    file.sync_all().unwrap();
    let corrupt_bytes = std::fs::metadata(&path).unwrap().len();
    let (corrupt, _owner) = make_registry(&root, fs, scope);
    add_local(&corrupt, "caller", caller).await;
    assert!(restore_actor(&corrupt, caller, scope).await.is_err());
    assert_eq!(std::fs::metadata(&path).unwrap().len(), corrupt_bytes);
}

#[tokio::test]
async fn handback_native_root_swap_during_torn_tail_repair_cannot_truncate_replacement() {
    let (_dir, root) = native_root();
    let fs = Arc::new(NativeJournalFs::default());
    let scope = scope();
    let caller = AgentId::new();
    let sender = AgentId::new();
    let (registry, _owner) = make_registry(&root, fs.clone(), scope);
    add_local(&registry, "caller", caller).await;
    registry
        .send_peer(
            "caller",
            peer_envelope(scope, caller, sender, 1, MessageId::new()),
        )
        .await
        .unwrap();
    let path = inbox_path(&root, scope, caller, "jsonl");
    let mut file = std::fs::OpenOptions::new()
        .append(true)
        .open(&path)
        .unwrap();
    file.write_all(b"\xf0\x9f").unwrap();
    file.sync_all().unwrap();
    let retained_bytes = std::fs::read(&path).unwrap();
    let (cold, _owner) = make_registry(&root, fs.clone(), scope);
    add_local(&cold, "caller", caller).await;
    fs.swap_before_truncate.store(true, Ordering::Release);
    assert!(restore_actor(&cold, caller, scope).await.is_err());
    assert_eq!(
        std::fs::read(&path).unwrap(),
        b"replacement journal must survive\n"
    );
    assert_eq!(
        std::fs::read(inbox_path(&retained_root(&root), scope, caller, "jsonl")).unwrap(),
        retained_bytes
    );
}
