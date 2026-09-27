// lingxi-code/crates/core/src/settings/loader.rs
//! Settings-file path + file-read primitives.
//!
//! [`crate::settings::Settings`] owns the merge precedence across defaults,
//! user, project, local, CLI, managed, and env layers. This module provides
//! the per-file read guardrails and path helpers those folds rely on.

use crate::settings::schema::SettingsJson;
use crate::settings::SettingsError;
use std::path::{Path, PathBuf};

/// Claude Code rejects settings files above 2 MiB before reading/parsing them.
pub const MAX_SETTINGS_FILE_BYTES: u64 = 2 * 1024 * 1024;

/// Read one settings file from disk.
///
/// # Errors
///
/// - [`SettingsError::ParseError`] on malformed JSON or a known field whose
///   value has the wrong type. Unknown fields are NOT an error — they are
///   tolerated-and-ignored, matching claude-code's zod `.passthrough()`
///   schema (`types.ts:1072`; `safeParse` at `settings.ts:219`).
/// - [`SettingsError::Io`] on permission-denied or other non-`NotFound` I/O
///   failure. `NotFound` is NOT an error — it returns `Ok(None)`.
/// - [`SettingsError::SchemaViolation`] when [`SettingsJson::validate`] rejects
///   the parsed file.
pub fn read_settings_file(path: &Path) -> Result<Option<SettingsJson>, SettingsError> {
    let metadata = match std::fs::metadata(path) {
        Ok(meta) => meta,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(None),
        Err(e) => {
            return Err(SettingsError::Io {
                path: path.to_path_buf(),
                source: e,
            })
        }
    };
    if !metadata.is_file() {
        return Err(SettingsError::Io {
            path: path.to_path_buf(),
            source: std::io::Error::new(
                std::io::ErrorKind::InvalidData,
                "settings file must be a regular file",
            ),
        });
    }
    if metadata.len() > MAX_SETTINGS_FILE_BYTES {
        return Err(SettingsError::Io {
            path: path.to_path_buf(),
            source: settings_size_limit_error(),
        });
    }
    let bytes = match std::fs::read(path) {
        Ok(b) => b,
        Err(e) => {
            return Err(SettingsError::Io {
                path: path.to_path_buf(),
                source: e,
            })
        }
    };
    if u64::try_from(bytes.len()).unwrap_or(u64::MAX) > MAX_SETTINGS_FILE_BYTES {
        return Err(SettingsError::Io {
            path: path.to_path_buf(),
            source: settings_size_limit_error(),
        });
    }
    let parsed: SettingsJson =
        serde_json::from_slice(&bytes).map_err(|e| SettingsError::ParseError {
            path: path.to_path_buf(),
            source: e,
        })?;
    parsed.validate()?;
    Ok(Some(parsed))
}

/// Path to the user settings file: `~/.lingxi/settings.json`.
///
/// Uses `HOME` env var with a `dirs::home_dir`-equivalent fallback. Returns
/// `None` if neither resolves (e.g. on a misconfigured CI runner).
#[must_use]
pub fn user_settings_path() -> Option<PathBuf> {
    config_home_dir().map(|h| h.join("settings.json"))
}

/// User config-home: `$branding::CONFIG_DIR_ENV` when set (`??`: an empty value
/// is honored verbatim → cwd-relative), else `<home>/<branding::DOT_DIR>`.
/// Returns `None` only when neither the env override nor `$HOME` resolves.
#[must_use]
fn config_home_dir() -> Option<PathBuf> {
    if let Some(dir) = std::env::var_os(branding::CONFIG_DIR_ENV) {
        return Some(PathBuf::from(dir));
    }
    home_dir().map(|h| branding::config_home(&h, None))
}

/// Path to the project settings file: `<project_dir>/<branding::DOT_DIR>/settings.json`.
#[must_use]
pub fn project_settings_path(project_dir: &Path) -> PathBuf {
    project_dir.join(branding::DOT_DIR).join("settings.json")
}

/// Path to the project-local settings file:
/// `<project_dir>/<branding::DOT_DIR>/settings.local.json`.
#[must_use]
pub fn local_settings_path(project_dir: &Path) -> PathBuf {
    project_dir
        .join(branding::DOT_DIR)
        .join("settings.local.json")
}

/// Set ONE top-level key in a settings file, read-modify-write, preserving
/// every other key — including fields this build's schema does not know about
/// (the reader tolerates-and-ignores them, so the writer must not delete them).
///
/// The file is parsed as a raw JSON object rather than through
/// [`SettingsJson`]: round-tripping through the typed schema would silently
/// drop unknown keys and rewrite the ones it does know in schema order. A
/// missing file is created; an empty file is treated as `{}`. Passing
/// `Value::Null` REMOVES the key, which is how a setting is reset to its
/// inherited value rather than pinned to a literal null.
///
/// Size-guarded by [`MAX_SETTINGS_FILE_BYTES`] on the read, like every other
/// settings read, so a hostile file cannot be loaded here either.
///
/// # Errors
///
/// - [`SettingsError::Io`] when the file cannot be read, its parent cannot be
///   created, or the write fails.
/// - [`SettingsError::ParseError`] when the existing file is not valid JSON.
/// - [`SettingsError::SchemaViolation`] when the existing file parses as JSON
///   but is not an object — overwriting it would destroy whatever it holds.
pub fn set_settings_key(
    path: &Path,
    key: &str,
    value: serde_json::Value,
) -> Result<(), SettingsError> {
    let mut object = match std::fs::metadata(path) {
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => serde_json::Map::new(),
        Err(e) => {
            return Err(SettingsError::Io {
                path: path.to_path_buf(),
                source: e,
            })
        }
        Ok(meta) if meta.len() > MAX_SETTINGS_FILE_BYTES => {
            return Err(SettingsError::SchemaViolation(format!(
                "settings file at {} exceeds {MAX_SETTINGS_FILE_BYTES} bytes",
                path.display()
            )))
        }
        Ok(_) => {
            let bytes = std::fs::read(path).map_err(|e| SettingsError::Io {
                path: path.to_path_buf(),
                source: e,
            })?;
            if bytes.is_empty() {
                serde_json::Map::new()
            } else {
                match serde_json::from_slice::<serde_json::Value>(&bytes).map_err(|source| {
                    SettingsError::ParseError {
                        path: path.to_path_buf(),
                        source,
                    }
                })? {
                    serde_json::Value::Object(m) => m,
                    _ => {
                        return Err(SettingsError::SchemaViolation(format!(
                            "settings file at {} is not a JSON object",
                            path.display()
                        )))
                    }
                }
            }
        }
    };

    if value.is_null() {
        object.remove(key);
    } else {
        object.insert(key.to_string(), value);
    }

    if let Some(dir) = path.parent() {
        std::fs::create_dir_all(dir).map_err(|e| SettingsError::Io {
            path: dir.to_path_buf(),
            source: e,
        })?;
    }
    let mut body = serde_json::to_vec_pretty(&serde_json::Value::Object(object)).map_err(|e| {
        SettingsError::Io {
            path: path.to_path_buf(),
            source: std::io::Error::other(e),
        }
    })?;
    body.push(b'\n');
    std::fs::write(path, &body).map_err(|e| SettingsError::Io {
        path: path.to_path_buf(),
        source: e,
    })
}

fn home_dir() -> Option<PathBuf> {
    std::env::var_os("HOME").map(PathBuf::from)
}

/// Byte-exact claude-code size-limit copy (`Settings file exceeds the 2MiB limit`).
pub const SETTINGS_FILE_SIZE_LIMIT_MESSAGE: &str = "Settings file exceeds the 2MiB limit";

pub(crate) fn settings_size_limit_error() -> std::io::Error {
    std::io::Error::new(
        std::io::ErrorKind::InvalidData,
        SETTINGS_FILE_SIZE_LIMIT_MESSAGE,
    )
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Write;

    #[test]
    fn returns_none_when_file_missing() {
        let tmp = tempfile::tempdir().unwrap();
        let path = tmp.path().join("does_not_exist.json");
        let parsed = read_settings_file(&path).unwrap();
        assert!(parsed.is_none());
    }

    #[test]
    fn returns_settings_for_valid_file() {
        let tmp = tempfile::tempdir().unwrap();
        let path = tmp.path().join("settings.json");
        let mut f = std::fs::File::create(&path).unwrap();
        writeln!(f, r#"{{"trustedDirectories": ["/foo"]}}"#).unwrap();
        let parsed = read_settings_file(&path).unwrap().unwrap();
        assert_eq!(
            parsed.trusted_directories.as_deref(),
            Some(&["/foo".to_string()][..])
        );
    }

    #[test]
    fn returns_parse_error_for_malformed_json() {
        let tmp = tempfile::tempdir().unwrap();
        let path = tmp.path().join("settings.json");
        let mut f = std::fs::File::create(&path).unwrap();
        writeln!(f, "{{ not valid json").unwrap();
        let err = read_settings_file(&path).unwrap_err();
        assert!(
            matches!(err, crate::settings::SettingsError::ParseError { .. }),
            "expected ParseError, got: {err:?}"
        );
    }

    #[test]
    fn tolerates_unknown_fields_like_zod_passthrough() {
        // claude-code's zod SettingsSchema is `.passthrough()` (types.ts:1072;
        // safeParse at settings.ts:219) — unknown keys never fail a TS load.
        // The old strictness was a parity divergence that made e.g. /effort's
        // persisted `effortLevel` silently kill the whole settings load.
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("settings.json");
        std::fs::write(
            &path,
            r#"{"model": "opus", "effortLevel": "high", "skipDangerousModePermissionPrompt": true, "env": {"DISABLE_AUTOUPDATER": "1"}, "futureKey": [1,2]}"#,
        )
        .unwrap();
        let settings = read_settings_file(&path)
            .expect("must load")
            .expect("must be Some");
        assert_eq!(settings.model.as_deref(), Some("opus"));
    }

    #[test]
    fn wrong_typed_known_field_is_still_a_parse_error() {
        // Unknown-key tolerance must NOT loosen typed parses: a KNOWN field
        // with the wrong type still fails the load.
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("settings.json");
        std::fs::write(&path, r#"{"model": 5}"#).unwrap();
        let err = read_settings_file(&path).unwrap_err();
        assert!(
            matches!(err, crate::settings::SettingsError::ParseError { .. }),
            "expected ParseError, got: {err:?}"
        );
    }

    #[test]
    fn rejects_files_larger_than_two_mebibytes_before_parsing() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("settings.json");
        std::fs::write(
            &path,
            vec![b' '; usize::try_from(MAX_SETTINGS_FILE_BYTES).unwrap() + 1],
        )
        .unwrap();
        let err = read_settings_file(&path).unwrap_err();
        assert!(
            matches!(err, crate::settings::SettingsError::Io { ref source, .. }
                if source.kind() == std::io::ErrorKind::InvalidData
                    && source.to_string().contains("2MiB limit")),
            "expected InvalidData 2MiB limit error, got: {err:?}"
        );
    }
}
