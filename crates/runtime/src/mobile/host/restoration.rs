use client::protocol::listings::SessionModeDto;

pub(super) fn lower_session_mode(mode: session::jsonl::SessionMode) -> SessionModeDto {
    match mode {
        session::jsonl::SessionMode::Chat => SessionModeDto::Chat,
        session::jsonl::SessionMode::Code => SessionModeDto::Code,
    }
}

/// ONE canonical spelling for a session-catalog cwd key. `canonicalize`
/// collapses the platform's symlink split (`/var` vs `/private/var` on
/// iOS/macOS), so listing, resume and the cwd gates all derive the SAME
/// sanitized `projects/` directory.
///
/// A bare `canonicalize(path).unwrap_or(raw)` disagrees with itself across a
/// path's lifetime: while the directory exists it spells `/private/var/...`, but
/// once it is gone `canonicalize` fails and the raw fallback spells a different
/// catalog directory. Walk up to the nearest surviving ancestor, canonicalize
/// THAT, and reappend the removed suffix, so both calls agree.
pub(crate) fn canonical_cwd_string(path: &std::path::Path) -> String {
    let mut removed_suffix: Vec<std::ffi::OsString> = Vec::new();
    let mut ancestor = path.to_path_buf();
    loop {
        match std::fs::canonicalize(&ancestor) {
            Ok(mut canonical) => {
                for name in removed_suffix.into_iter().rev() {
                    canonical.push(name);
                }
                return canonical.to_string_lossy().to_string();
            }
            Err(_) => match ancestor.file_name().map(std::ffi::OsString::from) {
                Some(name) => {
                    removed_suffix.push(name);
                    if !ancestor.pop() {
                        break;
                    }
                }
                None => break,
            },
        }
    }
    path.to_string_lossy().to_string()
}
