//! Constant-size identity cursors for append-only durable journals.
use crate::{
    map_io, open_create_new_file_pinned, open_existing_file_for_update_pinned,
    open_read_file_pinned, opened_file_identity, root_identity, sync_parent_pinned,
    validate_open_file_single_link, FsError, RootIdentity,
};
use std::io::{Read, Seek, SeekFrom, Write};
use std::path::Path;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct JournalCursor {
    pub root: Option<RootIdentity>,
    pub file: Option<RootIdentity>,
    pub length: u64,
    pub present: bool,
}

impl JournalCursor {
    pub fn virtual_cursor(length: u64, present: bool) -> Self {
        Self {
            root: None,
            file: None,
            length,
            present,
        }
    }

    pub fn matches_root(&self, expected: Option<&RootIdentity>) -> bool {
        self.root.as_ref() == expected
    }

    pub fn len(&self) -> u64 {
        self.length
    }

    pub fn is_empty(&self) -> bool {
        self.length == 0
    }
}

#[derive(Debug)]
pub struct JournalRead {
    pub bytes: Option<Vec<u8>>,
    pub cursor: JournalCursor,
}

#[derive(Debug)]
pub struct JournalWriteError {
    pub error: FsError,
    pub uncertain: bool,
    /// A proven rollback can leave a newly-created empty inode. The owner
    /// retains this cursor so its next append need not reopen or scan history.
    pub recovered: Option<JournalCursor>,
}

impl JournalWriteError {
    pub fn uncertain(error: FsError) -> Self {
        Self {
            error,
            uncertain: true,
            recovered: None,
        }
    }

    pub fn certain(error: FsError, recovered: Option<JournalCursor>) -> Self {
        Self {
            error,
            uncertain: false,
            recovered,
        }
    }
}

fn changed(path: &Path) -> FsError {
    FsError::OutsideWorkspace(format!(
        "journal identity or length changed: {}",
        path.display()
    ))
}

fn native_root(cursor: &JournalCursor, relative: &Path) -> Result<RootIdentity, FsError> {
    cursor.root.ok_or_else(|| changed(relative))
}

fn verify(
    root: &Path,
    relative: &Path,
    cursor: &JournalCursor,
    file: &std::fs::File,
) -> Result<(), FsError> {
    let pin = native_root(cursor, relative)?;
    validate_open_file_single_link(file)?;
    if !cursor.present
        || Some(opened_file_identity(file)?) != cursor.file
        || file
            .metadata()
            .map_err(|error| map_io(relative, error))?
            .len()
            != cursor.length
    {
        return Err(changed(relative));
    }
    let current = open_read_file_pinned(root, relative, Some(&pin))?;
    validate_open_file_single_link(&current)?;
    if Some(opened_file_identity(&current)?) != cursor.file {
        return Err(changed(relative));
    }
    Ok(())
}

pub fn read_journal_pinned(
    root: &Path,
    relative: &Path,
    expected_root: Option<&RootIdentity>,
) -> Result<JournalRead, FsError> {
    let pin = match expected_root {
        Some(pin) => *pin,
        None => root_identity(root)?,
    };
    let file = match open_read_file_pinned(root, relative, Some(&pin)) {
        Ok(file) => file,
        Err(FsError::NotFound(_)) => {
            if root_identity(root)? != pin {
                return Err(changed(relative));
            }
            return Ok(JournalRead {
                bytes: None,
                cursor: JournalCursor {
                    root: Some(pin),
                    file: None,
                    length: 0,
                    present: false,
                },
            });
        }
        Err(error) => return Err(error),
    };
    let cursor = JournalCursor {
        root: Some(pin),
        file: Some(opened_file_identity(&file)?),
        length: file
            .metadata()
            .map_err(|error| map_io(relative, error))?
            .len(),
        present: true,
    };
    verify(root, relative, &cursor, &file)?;
    let mut bytes = Vec::new();
    (&file)
        .take(cursor.length)
        .read_to_end(&mut bytes)
        .map_err(|error| map_io(relative, error))?;
    if bytes.len() as u64 != cursor.length {
        return Err(changed(relative));
    }
    verify(root, relative, &cursor, &file)?;
    Ok(JournalRead {
        bytes: Some(bytes),
        cursor,
    })
}

pub fn append_journal_durable(
    root: &Path,
    relative: &Path,
    expected: &JournalCursor,
    content: &str,
) -> Result<JournalCursor, JournalWriteError> {
    append_inner(root, relative, expected, content, || Ok(()))
}

fn append_inner(
    root: &Path,
    relative: &Path,
    expected: &JournalCursor,
    content: &str,
    after_sync: impl FnOnce() -> Result<(), FsError>,
) -> Result<JournalCursor, JournalWriteError> {
    let pin = native_root(expected, relative).map_err(JournalWriteError::uncertain)?;
    let mut file = if expected.present {
        open_existing_file_for_update_pinned(root, relative, &pin)
    } else if expected.file.is_none() && expected.length == 0 {
        open_create_new_file_pinned(root, relative, Some(&pin))
    } else {
        return Err(JournalWriteError::uncertain(changed(relative)));
    }
    .map_err(JournalWriteError::uncertain)?;
    let before = if expected.present {
        *expected
    } else {
        JournalCursor {
            root: Some(pin),
            file: Some(opened_file_identity(&file).map_err(JournalWriteError::uncertain)?),
            length: 0,
            present: true,
        }
    };
    verify(root, relative, &before, &file).map_err(JournalWriteError::uncertain)?;
    let length = before
        .length
        .checked_add(content.len() as u64)
        .ok_or_else(|| {
            JournalWriteError::certain(FsError::Io("journal length exhausted".into()), Some(before))
        })?;
    let after = JournalCursor { length, ..before };
    let result = (|| {
        file.seek(SeekFrom::End(0))
            .map_err(|error| map_io(relative, error))?;
        file.write_all(content.as_bytes())
            .map_err(|error| map_io(relative, error))?;
        file.sync_all().map_err(|error| map_io(relative, error))?;
        after_sync()?;
        verify(root, relative, &after, &file)?;
        sync_parent_pinned(root, relative, Some(&pin))?;
        verify(root, relative, &after, &file)
    })();
    if let Err(error) = result {
        // Repair the descriptor actually written, never an unpinned path that
        // could now name another journal. A root/name swap remains uncertain
        // even when this old descriptor could be restored successfully.
        let rollback = (|| {
            file.set_len(before.length)
                .map_err(|error| map_io(relative, error))?;
            file.sync_all().map_err(|error| map_io(relative, error))?;
            verify(root, relative, &before, &file)?;
            sync_parent_pinned(root, relative, Some(&pin))?;
            verify(root, relative, &before, &file)
        })();
        return Err(if rollback.is_ok() {
            JournalWriteError::certain(error, Some(before))
        } else {
            JournalWriteError::uncertain(error)
        });
    }
    Ok(after)
}

pub fn truncate_journal_durable(
    root: &Path,
    relative: &Path,
    expected: &JournalCursor,
    length: u64,
) -> Result<JournalCursor, JournalWriteError> {
    if length > expected.length {
        return Err(JournalWriteError::certain(
            FsError::Io("journal truncate would extend file".into()),
            None,
        ));
    }
    let pin = native_root(expected, relative).map_err(JournalWriteError::uncertain)?;
    let file = open_existing_file_for_update_pinned(root, relative, &pin)
        .map_err(JournalWriteError::uncertain)?;
    verify(root, relative, expected, &file).map_err(JournalWriteError::uncertain)?;
    let after = JournalCursor {
        length,
        ..*expected
    };
    let result = (|| {
        file.set_len(length)
            .map_err(|error| map_io(relative, error))?;
        file.sync_all().map_err(|error| map_io(relative, error))?;
        verify(root, relative, &after, &file)?;
        sync_parent_pinned(root, relative, Some(&pin))?;
        verify(root, relative, &after, &file)
    })();
    result.map_err(JournalWriteError::uncertain)?;
    Ok(after)
}

pub fn sync_journal_durable(
    root: &Path,
    relative: &Path,
    expected: &JournalCursor,
) -> Result<(), FsError> {
    let pin = native_root(expected, relative)?;
    let file = open_existing_file_for_update_pinned(root, relative, &pin)?;
    verify(root, relative, expected, &file)?;
    file.sync_all().map_err(|error| map_io(relative, error))?;
    sync_parent_pinned(root, relative, Some(&pin))?;
    verify(root, relative, expected, &file)
}

#[cfg(all(test, unix))]
mod tests {
    use super::*;

    #[test]
    fn root_swap_after_synced_append_repairs_original_descriptor_and_remains_uncertain() {
        let directory = tempfile::tempdir().unwrap();
        let root = directory.path().join("inbox");
        let moved = directory.path().join("moved");
        std::fs::create_dir(&root).unwrap();
        let relative = Path::new("journal.jsonl");
        std::fs::write(root.join(relative), b"old\n").unwrap();
        let cursor = read_journal_pinned(&root, relative, None).unwrap().cursor;
        let error = append_inner(&root, relative, &cursor, "new\n", || {
            std::fs::rename(&root, &moved).unwrap();
            std::fs::create_dir(&root).unwrap();
            std::fs::write(root.join(relative), b"unrelated\n").unwrap();
            Ok(())
        })
        .unwrap_err();
        assert!(error.uncertain);
        assert!(error.recovered.is_none());
        assert_eq!(std::fs::read(moved.join(relative)).unwrap(), b"old\n");
        assert_eq!(std::fs::read(root.join(relative)).unwrap(), b"unrelated\n");
    }

    #[test]
    fn failed_first_append_returns_empty_inode_cursor_for_retry() {
        let directory = tempfile::tempdir().unwrap();
        let relative = Path::new("journal.jsonl");
        let cursor = read_journal_pinned(directory.path(), relative, None)
            .unwrap()
            .cursor;
        let error = append_inner(directory.path(), relative, &cursor, "partial\n", || {
            Err(FsError::Io("injected sync failure".into()))
        })
        .unwrap_err();
        assert!(!error.uncertain);
        let recovered = error.recovered.unwrap();
        assert!(recovered.present);
        assert!(recovered.is_empty());
        let final_cursor =
            append_journal_durable(directory.path(), relative, &recovered, "retry\n").unwrap();
        assert_eq!(final_cursor.len(), 6);
        assert_eq!(
            std::fs::read(directory.path().join(relative)).unwrap(),
            b"retry\n"
        );
    }

    #[test]
    fn raw_reads_keep_torn_utf8_and_checked_updates_refuse_replaced_leaf() {
        let directory = tempfile::tempdir().unwrap();
        let relative = Path::new("journal.jsonl");
        let path = directory.path().join(relative);
        std::fs::write(&path, b"complete\n\xf0\x9f").unwrap();
        let read = read_journal_pinned(directory.path(), relative, None).unwrap();
        assert_eq!(read.bytes.as_deref(), Some(&b"complete\n\xf0\x9f"[..]));
        std::fs::rename(&path, directory.path().join("old")).unwrap();
        std::fs::write(&path, b"unrelated\n").unwrap();
        assert!(
            truncate_journal_durable(directory.path(), relative, &read.cursor, 0)
                .unwrap_err()
                .uncertain
        );
        assert!(
            append_journal_durable(directory.path(), relative, &read.cursor, "new\n")
                .unwrap_err()
                .uncertain
        );
        assert_eq!(std::fs::read(&path).unwrap(), b"unrelated\n");
    }
}
