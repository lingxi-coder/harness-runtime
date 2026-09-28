//! App-private file operations of the `window.lingxi.v2` bridge.
//!
//! Files are deliberately rooted at `apps/<id>/files`, not at the generated
//! workspace. The page gets a small, binary-safe file store with no source,
//! build-output, dependency, or host metadata access.

use super::{BridgeFailure, LocalAppsHostBroker};
use base64::Engine as _;
use client::protocol::local_apps::AppCapabilityKindDto;
use local_apps::AppCapability;
use serde_json::{json, Value};
use std::path::{Component, Path, PathBuf};

/// Maximum raw bytes in one app-private file.
pub(super) const MAX_APP_FILE_BYTES: usize = 4 * 1024 * 1024;
/// Maximum UTF-8 byte length of one relative file path.
const MAX_APP_FILE_PATH_BYTES: usize = 1_024;
const FILES_DIR: &str = "files";
const REASON_FILES_READ: &str = "应用请求读取自己的私有文件。";
const REASON_FILES_WRITE: &str = "应用请求写入自己的私有文件。";

fn invalid(message: impl Into<String>) -> BridgeFailure {
    BridgeFailure::coded("invalid_request", message.into())
}

fn file_not_found(path: &str) -> BridgeFailure {
    BridgeFailure::coded("file_not_found", format!("file {path:?} was not found"))
}

fn file_root(broker: &LocalAppsHostBroker, app_id: &str) -> Result<PathBuf, BridgeFailure> {
    let layout = broker.layout(app_id).map_err(BridgeFailure::from)?;
    Ok(layout.root().join(layout.app_dir_rel()).join(FILES_DIR))
}

fn relative_path(payload: &Value) -> Result<(String, PathBuf), BridgeFailure> {
    let raw = payload
        .get("path")
        .and_then(Value::as_str)
        .ok_or_else(|| invalid("path is required"))?;
    if raw.is_empty() || raw.len() > MAX_APP_FILE_PATH_BYTES || raw.as_bytes().contains(&0) {
        return Err(invalid(format!(
            "path must be 1..={MAX_APP_FILE_PATH_BYTES} UTF-8 bytes"
        )));
    }
    let path = Path::new(raw);
    if path.is_absolute() {
        return Err(invalid("path must be relative to the app file store"));
    }
    // `Path::components()` deliberately normalizes `.` away before yielding
    // components, so validate the raw wire spelling first.  The file bridge
    // accepts ordinary relative names only; keeping dot segments out of the
    // contract prevents different native path implementations from silently
    // disagreeing about the same request.
    if raw
        .split('/')
        .any(|component| matches!(component, "." | ".."))
    {
        return Err(invalid(
            "path must contain only normalized relative components",
        ));
    }
    let mut normalized = PathBuf::new();
    for component in path.components() {
        match component {
            Component::Normal(part) => normalized.push(part),
            _ => {
                return Err(invalid(
                    "path must contain only normalized relative components",
                ))
            }
        }
    }
    if normalized.as_os_str().is_empty() {
        return Err(invalid("path must not be empty"));
    }
    Ok((raw.to_string(), normalized))
}

fn encoding(payload: &Value) -> Result<&str, BridgeFailure> {
    match payload.get("encoding") {
        None | Some(Value::Null) => Ok("utf8"),
        Some(value) => match value.as_str() {
            Some("utf8") => Ok("utf8"),
            Some("base64") => Ok("base64"),
            _ => Err(invalid("encoding must be utf8 or base64")),
        },
    }
}

fn decode_content(payload: &Value) -> Result<(String, Vec<u8>), BridgeFailure> {
    let encoding = encoding(payload)?.to_string();
    let content = payload
        .get("content")
        .and_then(Value::as_str)
        .ok_or_else(|| invalid("content is required"))?;
    let bytes = match encoding.as_str() {
        "utf8" => content.as_bytes().to_vec(),
        "base64" => base64::engine::general_purpose::STANDARD
            .decode(content)
            .map_err(|error| invalid(format!("content is not valid base64: {error}")))?,
        _ => unreachable!("encoding validates before decoding"),
    };
    if bytes.len() > MAX_APP_FILE_BYTES {
        return Err(BridgeFailure::coded(
            "file_too_large",
            format!(
                "file is {} bytes; the limit is {MAX_APP_FILE_BYTES}",
                bytes.len()
            ),
        ));
    }
    Ok((encoding, bytes))
}

fn ensure_directory(path: &Path) -> Result<(), BridgeFailure> {
    match std::fs::symlink_metadata(path) {
        Ok(metadata) if metadata.file_type().is_symlink() => Err(BridgeFailure::coded(
            "unsafe_path",
            "file store contains a symlink",
        )),
        Ok(metadata) if metadata.is_dir() => Ok(()),
        Ok(_) => Err(BridgeFailure::coded(
            "unsafe_path",
            "file store path is not a directory",
        )),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
            std::fs::create_dir(path).map_err(|error| {
                BridgeFailure::from(format!("create app file directory: {error}"))
            })?;
            Ok(())
        }
        Err(error) => Err(BridgeFailure::from(format!(
            "inspect app file directory: {error}"
        ))),
    }
}

fn ensure_parent(root: &Path, relative: &Path) -> Result<PathBuf, BridgeFailure> {
    ensure_directory(root)?;
    let mut current = root.to_path_buf();
    let mut components = relative.components().peekable();
    while let Some(Component::Normal(part)) = components.next() {
        current.push(part);
        if components.peek().is_some() {
            ensure_directory(&current)?;
        }
    }
    Ok(current)
}

fn read_regular_file(path: &Path, display: &str) -> Result<Vec<u8>, BridgeFailure> {
    let metadata = std::fs::symlink_metadata(path).map_err(|error| {
        if error.kind() == std::io::ErrorKind::NotFound {
            file_not_found(display)
        } else {
            BridgeFailure::from(format!("inspect app file {display:?}: {error}"))
        }
    })?;
    if metadata.file_type().is_symlink() || !metadata.is_file() {
        return Err(BridgeFailure::coded(
            "unsafe_path",
            format!("file {display:?} is not a regular file"),
        ));
    }
    if metadata.len() > MAX_APP_FILE_BYTES as u64 {
        return Err(BridgeFailure::coded(
            "file_too_large",
            format!("file {display:?} exceeds {MAX_APP_FILE_BYTES} bytes"),
        ));
    }
    std::fs::read(path)
        .map_err(|error| BridgeFailure::from(format!("read app file {display:?}: {error}")))
}

impl LocalAppsHostBroker {
    pub(super) async fn file_read_value(
        &self,
        app_id: &str,
        payload: &Value,
    ) -> Result<Value, BridgeFailure> {
        self.authorize_declared_capability(
            app_id,
            AppCapability::FilesRead,
            AppCapabilityKindDto::FilesRead,
            REASON_FILES_READ,
        )
        .await?;
        let (display, relative) = relative_path(payload)?;
        let encoding = encoding(payload)?;
        let root = file_root(self, app_id)?;
        let path = root.join(&relative);
        let bytes = read_regular_file(&path, &display)?;
        let content = match encoding {
            "utf8" => String::from_utf8(bytes.clone()).map_err(|_| {
                BridgeFailure::coded(
                    "invalid_encoding",
                    "file is not valid UTF-8; read it with encoding=base64",
                )
            })?,
            "base64" => base64::engine::general_purpose::STANDARD.encode(&bytes),
            _ => unreachable!("encoding validates before reading"),
        };
        Ok(json!({
            "path": display,
            "encoding": encoding,
            "content": content,
            "bytes": bytes.len(),
        }))
    }

    pub(super) async fn file_write_value(
        &self,
        app_id: &str,
        payload: &Value,
    ) -> Result<Value, BridgeFailure> {
        self.authorize_declared_capability(
            app_id,
            AppCapability::FilesWrite,
            AppCapabilityKindDto::FilesWrite,
            REASON_FILES_WRITE,
        )
        .await?;
        let (display, relative) = relative_path(payload)?;
        let (_encoding, bytes) = decode_content(payload)?;
        let root = file_root(self, app_id)?;
        let path = ensure_parent(&root, &relative)?;
        if let Ok(metadata) = std::fs::symlink_metadata(&path) {
            if metadata.file_type().is_symlink() || metadata.is_dir() {
                return Err(BridgeFailure::coded(
                    "unsafe_path",
                    format!("file {display:?} is not a regular file"),
                ));
            }
        }
        let file_name = path
            .file_name()
            .and_then(|name| name.to_str())
            .ok_or_else(|| invalid("path must end in a UTF-8 file name"))?;
        let temp_path =
            path.with_file_name(format!(".{file_name}.tmp-{}", self.request_id("file")));
        if let Err(error) = std::fs::write(&temp_path, &bytes) {
            return Err(BridgeFailure::from(format!(
                "write app file {display:?}: {error}"
            )));
        }
        if let Err(error) = std::fs::rename(&temp_path, &path) {
            let _ = std::fs::remove_file(&temp_path);
            return Err(BridgeFailure::from(format!(
                "replace app file {display:?}: {error}"
            )));
        }
        Ok(json!({ "path": display, "bytes": bytes.len() }))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn file_paths_are_normalized_relative_components_only() {
        assert!(relative_path(&json!({"path": "notes/today.txt"})).is_ok());
        for path in [
            "",
            "/tmp/secret",
            "../secret",
            "notes/../secret",
            "notes/./today",
        ] {
            assert!(relative_path(&json!({"path": path})).is_err(), "{path}");
        }
    }

    #[test]
    fn file_content_supports_utf8_and_bounded_base64() {
        let (encoding, utf8) = decode_content(&json!({
            "content": "你好",
            "encoding": "utf8"
        }))
        .expect("utf8 content");
        assert_eq!(encoding, "utf8");
        assert_eq!(utf8, "你好".as_bytes());

        let (encoding, binary) = decode_content(&json!({
            "content": "AP+A",
            "encoding": "base64"
        }))
        .expect("base64 content");
        assert_eq!(encoding, "base64");
        assert_eq!(binary, [0, 255, 128]);
    }
}
