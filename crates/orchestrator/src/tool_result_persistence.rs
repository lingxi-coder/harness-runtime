//! Tool-result persistence — the `<persisted-output>` substitution.
//!
//! Full content exceeding a tool's persistence threshold is stored under
//! `<config_home>/projects/<project>/<session>/tool-results/<id>.txt`
//! (or `.json` for content arrays) and replaced by a preview envelope.
//!
//! Oracle: Claude Code 2.1.319, binary offsets 267397971..267414315:
//! `LK` persists exclusively, `LCe` builds the preview, and `Zye` wraps it.
//! The source-locked `tool_result_persistence_319_oracle.mjs` executes these
//! functions and the imported `Q` / `v4` Unicode helpers.
//!
//! `LK` returns `originalSize:h.length`, measured in JavaScript UTF-16 units,
//! while its `v4` cap limits encoded UTF-8 bytes. `LCe` measures its preview in
//! UTF-16 units and drops a trailing high surrogate when cutting the source.

use std::path::{Path, PathBuf};

/// `Clt` (2.1.220 @ 230276246) — opening tag of the substituted envelope.
pub const PERSISTED_OUTPUT_OPEN: &str = "<persisted-output>";

/// `Cas` — closing tag of the substituted envelope.
pub const PERSISTED_OUTPUT_CLOSE: &str = "</persisted-output>";

/// `_or = 2000` — how much of the persisted body is echoed back to the model.
pub const PREVIEW_CHARS: usize = 2_000;

/// `AKr = 50000` — the default `persistenceThresholdCeiling` applied by `M0u`
/// when a tool does not declare its own.
pub const DEFAULT_PERSISTENCE_CEILING: usize = 50_000;

/// `RKr = 4` — chars per token, used only for the analytics estimate.
pub const CHARS_PER_TOKEN: usize = 4;

/// `was = "tool-results"` — the per-session subdirectory name.
pub const TOOL_RESULTS_DIR: &str = "tool-results";

/// `xJi = 1073741824` — `LK`'s UTF-8 byte cap in Claude Code 2.1.319.
pub const MAX_PERSIST_BYTES: usize = 1_073_741_824;

/// Compatibility name retained for callers of the earlier UTF-16 cap API.
/// The limit is now applied to encoded UTF-8 bytes, matching 2.1.319 `v4`.
pub const MAX_PERSIST_UTF16_UNITS: usize = MAX_PERSIST_BYTES;

/// Source-preserving equivalent of native `v4` / `TextEncoder.encodeInto`.
/// Lone units consume three UTF-8 bytes at the file boundary, while valid
/// surrogate pairs consume four bytes and are never divided by the cap.
#[must_use]
fn truncate_utf8_bytes(source: &[u16], limit: usize) -> &[u16] {
    if source.len() <= limit / 3 {
        return source;
    }
    let mut bytes = 0;
    let mut end = 0;
    for decoded in char::decode_utf16(source.iter().copied()) {
        let (unit_count, byte_count) = match decoded {
            Ok(character) => (character.len_utf16(), character.len_utf8()),
            Err(_) => (1, char::REPLACEMENT_CHARACTER.len_utf8()),
        };
        if byte_count > limit - bytes {
            break;
        }
        bytes += byte_count;
        end += unit_count;
    }
    &source[..end]
}

/// `M0u`'s ceiling fold (2.1.220 @ 230269046):
/// `Math.min(tool.maxResultSizeChars, tool.persistenceThresholdCeiling ?? AKr)`.
///
/// `Number.isFinite` fails for the `1/0` tools (`Read`), which `None` models —
/// they never persist. Callers hand the already-folded value to
/// [`crate::turn_loop`] via `Tool::persistence_threshold`, so this is the
/// arithmetic each tool's override performs.
///
/// The per-tool `tengu_velvet_ibis` numeric override that `M0u` consults ahead
/// of the fold is NOT modelled: the gate map is `{}` in the binary default and
/// `{}` in the live `~/.claude.json` `.cachedGrowthBookFeatures`, so it always
/// falls through to this `min`.
#[must_use]
pub fn resolve_threshold(max_result_size_chars: usize, ceiling: Option<usize>) -> usize {
    max_result_size_chars.min(ceiling.unwrap_or(DEFAULT_PERSISTENCE_CEILING))
}

/// Result of a successful [`persist`] call — the `x2e` return shape.
#[derive(Debug, Clone)]
pub struct Persisted {
    /// Absolute path the content was written to. "Full" only when
    /// [`Self::truncated_at`] is `None`.
    pub filepath: PathBuf,
    /// `truncatedAtBytes` — the cap, when the body exceeded it and was cut to
    /// fit; `None` when the whole body was written.
    ///
    /// Upstream computes it as `c === p ? undefined : l`: the CAP, not the
    /// written length, so the envelope can say "only the first {cap} were
    /// saved" using one number twice.
    pub truncated_at: Option<usize>,
    /// UTF-16 code-unit length of the persisted body (`l.length` in `tG`).
    pub original_size: usize,
    /// Whether the body was serialized from a content ARRAY (`.json`).
    pub is_json: bool,
    /// The leading slice echoed back to the model.
    pub preview: String,
    /// Exact JavaScript preview, retaining interior lone units.
    pub preview_utf16: Vec<u16>,
    /// Whether the body was longer than the preview.
    pub has_more: bool,
}

/// Port of claude-code `pl` (2.1.220 @ 226176853):
/// `t=n/1024; t<1 -> "${n} bytes"; t<1024 -> "${t.toFixed(1).replace(/\.0$/,"")}KB"`,
/// then MB, then GB.
#[must_use]
#[allow(clippy::cast_precision_loss)]
pub fn format_bytes(n: usize) -> String {
    /// `${x.toFixed(1).replace(/\.0$/,"")}` — one decimal, trailing `.0` dropped.
    fn fixed1(x: f64) -> String {
        // Sizes divided by powers of 1024 are dyadic. JS toFixed rounds
        // exact decimal ties upward, whereas Rust formatting uses ties-to-even.
        let rounded = (x * 10.0).round() / 10.0;
        let s = format!("{rounded:.1}");
        match s.strip_suffix(".0") {
            Some(trimmed) => trimmed.to_string(),
            None => s,
        }
    }
    let kb = n as f64 / 1024.0;
    if kb < 1.0 {
        return format!("{n} bytes");
    }
    if kb < 1024.0 {
        return format!("{}KB", fixed1(kb));
    }
    let mb = kb / 1024.0;
    if mb < 1024.0 {
        return format!("{}MB", fixed1(mb));
    }
    format!("{}GB", fixed1(mb / 1024.0))
}

/// Display view of native `LCe` (2.1.319) with its UTF-16-safe `Q` prefix.
///
/// ```text
/// if(e.length<=t)return{preview:e,hasMore:!1};
/// let n=e.slice(0,t).lastIndexOf("\n"),o=n>t*0.5?n:t;
/// return{preview:Q(e,o),hasMore:!0}
/// ```
///
/// Display view of the exact JavaScript preview. Use `preview_utf16` for wire data.
#[must_use]
pub fn preview(s: &str, limit: usize) -> (String, bool) {
    let (units, more) = preview_utf16(&s.encode_utf16().collect::<Vec<_>>(), limit);
    (String::from_utf16_lossy(&units), more)
}

/// Native `LCe` / `Q`: UTF-16 newline midpoint and a safe high-surrogate edge.
#[must_use]
pub fn preview_utf16(source: &[u16], limit: usize) -> (Vec<u16>, bool) {
    if source.len() <= limit {
        return (source.to_vec(), false);
    }
    let mut end = limit;
    if let Some(index) = source[..limit]
        .iter()
        .rposition(|unit| *unit == u16::from(b'\n'))
    {
        if index > limit / 2 {
            end = index;
        }
    }
    if source[..end]
        .last()
        .is_some_and(|unit| (0xd800..=0xdbff).contains(unit))
    {
        end -= 1;
    }
    (source[..end].to_vec(), true)
}

/// Compose the envelope without converting a lone surrogate into UTF-8.
#[must_use]
pub fn wrap_utf16(
    original_size: usize,
    filepath: &str,
    preview: &[u16],
    has_more: bool,
    truncated_at: Option<usize>,
) -> Vec<u16> {
    let mut units: Vec<u16> = format!(
        "{PERSISTED_OUTPUT_OPEN}\n{}Preview (first {}):\n",
        persisted_lead(original_size, filepath, truncated_at),
        format_bytes(PREVIEW_CHARS)
    )
    .encode_utf16()
    .collect();
    units.extend_from_slice(preview);
    units.extend(if has_more { "\n...\n" } else { "\n" }.encode_utf16());
    units.extend(PERSISTED_OUTPUT_CLOSE.encode_utf16());
    units
}

/// Port of claude-code `Alt` (2.1.220 @ 230269820, byte-verified via `od -c`).
#[must_use]
pub fn wrap(
    original_size: usize,
    filepath: &str,
    preview: &str,
    has_more: bool,
    truncated_at: Option<usize>,
) -> String {
    // `let t=`${Clt}\n`;
    //  t+= truncatedAtBytes===undefined
    //      ? `Output too large (${pl(e.originalSize)}). Full output saved to: ${e.filepath}\n\n`
    //      : `Output exceeded the ${pl(e.truncatedAtBytes)} persist limit; only the first ${pl(e.truncatedAtBytes)} were saved to: ${e.filepath}\n\n`;
    //  t+=`Preview (first ${pl(_or)}):\n`; t+=e.preview;
    //  t+=e.hasMore?`\n...\n`:`\n`; t+=Cas`
    let tail = if has_more { "\n...\n" } else { "\n" };
    format!(
        "{PERSISTED_OUTPUT_OPEN}\n{}Preview (first {}):\n{preview}{tail}{PERSISTED_OUTPUT_CLOSE}",
        persisted_lead(original_size, filepath, truncated_at),
        format_bytes(PREVIEW_CHARS),
    )
}

/// The substitution a keep-recent microcompact leaves in place of a cleared
/// tool result whose content was persisted — upstream `ZMy`:
///
/// ```js
/// `${nle}Tool result saved to: ${s.filepath}${d}\n\nUse ${rt} to view${VPn}`
/// ```
///
/// where `d` is `""` when the body fit and
/// `" (truncated to the first ${N} of the output)"` when it did not.
///
/// ⚠️ Note the shape differs from [`wrap`]: no newline after the opening tag,
/// and it names the `Read` tool rather than previewing the content. This is the
/// "it is gone, here is where it went" form, not the "here is a preview" form.
#[must_use]
pub fn microcompact_replacement(filepath: &str, truncated_at: Option<usize>) -> String {
    let truncated = match truncated_at {
        None => String::new(),
        Some(cap) => format!(
            " (truncated to the first {} of the output)",
            format_bytes(cap)
        ),
    };
    format!(
        "{PERSISTED_OUTPUT_OPEN}Tool result saved to: {filepath}{truncated}\n\nUse Read to view{PERSISTED_OUTPUT_CLOSE}"
    )
}

/// The envelope's first sentence — `hee`'s ternary. The truncated arm prints
/// the CAP twice and never mentions the original size, which is upstream's
/// choice: once a body is cut, "how big it was" is less actionable than "how
/// much of it is in the file".
#[must_use]
fn persisted_lead(original_size: usize, filepath: &str, truncated_at: Option<usize>) -> String {
    match truncated_at {
        None => format!(
            "Output too large ({}). Full output saved to: {filepath}\n\n",
            format_bytes(original_size)
        ),
        Some(cap) => format!(
            "Output exceeded the {} persist limit; only the first {} were saved to: {filepath}\n\n",
            format_bytes(cap),
            format_bytes(cap)
        ),
    }
}

/// `xke()` — `<config_home>/projects/<project_dir_name(cwd)>/<session_uuid>/tool-results`.
///
/// Reuses [`session::jsonl::path::project_dir_name`] so the sanitizer can
/// never drift from the transcript writer's.
#[must_use]
pub fn tool_results_dir(config_home: &Path, cwd: &str, session_uuid: &str) -> PathBuf {
    let mut p = config_home.to_path_buf();
    p.push("projects");
    p.push(session::jsonl::path::project_dir_name(cwd));
    p.push(session_uuid);
    p.push(TOOL_RESULTS_DIR);
    p
}

fn result_file_stem(id: &str) -> std::borrow::Cow<'_, str> {
    let windows_device = id.len() <= 4
        && matches!(
            id.to_ascii_uppercase().as_str(),
            "CON"
                | "PRN"
                | "AUX"
                | "NUL"
                | "COM1"
                | "COM2"
                | "COM3"
                | "COM4"
                | "COM5"
                | "COM6"
                | "COM7"
                | "COM8"
                | "COM9"
                | "LPT1"
                | "LPT2"
                | "LPT3"
                | "LPT4"
                | "LPT5"
                | "LPT6"
                | "LPT7"
                | "LPT8"
                | "LPT9"
        );
    // Keep lowercase provider filenames. Any uppercase byte must enter the
    // reserved encoded namespace so distinct exact IDs cannot alias on a
    // case-insensitive filesystem. Reserve that namespace for all provider IDs,
    // including ones that already look like an encoded stem.
    if !id.is_empty()
        && id.len() <= 200
        && id.bytes().all(|byte| {
            byte.is_ascii_lowercase() || byte.is_ascii_digit() || matches!(byte, b'_' | b'-')
        })
        && !windows_device
        && !id
            .get(..7)
            .is_some_and(|prefix| prefix.eq_ignore_ascii_case("opaque-"))
        && !id.starts_with("mod-run-")
    {
        return std::borrow::Cow::Borrowed(id);
    }
    use sha2::Digest as _;
    std::borrow::Cow::Owned(format!("opaque-{:x}", sha2::Sha256::digest(id.as_bytes())))
}

/// Check descendants of the configured root (`PIn`). The root itself may be
/// a symlink; paths outside it are not checked by the oracle helper.
async fn check_directory(config_home: &Path, dir: &Path) -> Result<(), String> {
    let Ok(relative) = dir.strip_prefix(config_home) else {
        return Ok(());
    };
    if relative
        .components()
        .any(|part| matches!(part, std::path::Component::ParentDir))
    {
        return Ok(());
    }
    let mut current = config_home.to_path_buf();
    let mut paths = Vec::new();
    for part in relative.components() {
        if matches!(part, std::path::Component::Normal(_)) {
            current.push(part);
            paths.push(current.clone());
        }
    }
    for path in paths {
        match tokio::fs::symlink_metadata(&path).await {
            Ok(meta) if meta.file_type().is_symlink() || !meta.is_dir() => {
                return Err(format!(
                    "tool-results path refused: {} is a link or not a directory",
                    path.display()
                ));
            }
            Ok(_) => {}
            Err(err) if err.kind() == std::io::ErrorKind::NotFound => break,
            Err(err) => return Err(err.to_string()),
        }
    }
    Ok(())
}

fn check_link_count(count: u64) -> Result<(), String> {
    if count > 1 {
        return Err("tool result path has another name; not persisted".into());
    }
    Ok(())
}

fn check_existing_file(meta: &std::fs::Metadata) -> Result<(), String> {
    if meta.file_type().is_symlink() {
        return Err("tool result path is a link; not persisted".into());
    }
    if !meta.is_file() {
        return Err("tool result path is not a regular file; not persisted".into());
    }
    #[cfg(unix)]
    {
        use std::os::unix::fs::MetadataExt as _;
        check_link_count(meta.nlink())?;
    }
    Ok(())
}

#[cfg(windows)]
fn check_existing_windows_file(path: &Path) -> Result<(), String> {
    use std::os::windows::fs::{MetadataExt as _, OpenOptionsExt as _};

    const FILE_FLAG_OPEN_REPARSE_POINT: u32 = 0x0020_0000;
    const FILE_ATTRIBUTE_REPARSE_POINT: u32 = 0x0000_0400;
    let checked = || -> std::io::Result<(std::fs::Metadata, u32)> {
        // Metadata-only access also permits existing files without read-data
        // permission. Inspect the leaf itself, never a reparse-point target.
        let file = std::fs::OpenOptions::new()
            .access_mode(0)
            .custom_flags(FILE_FLAG_OPEN_REPARSE_POINT)
            .open(path)?;
        let metadata = file.metadata()?;
        let links = lingxi_core::host::rooted_fs::file_link_count(&file)?;
        Ok((metadata, links))
    };
    let (metadata, links) = checked()
        .map_err(|_| "tool result path could not be checked; not persisted".to_string())?;
    if metadata.file_attributes() & FILE_ATTRIBUTE_REPARSE_POINT != 0 {
        return Err("tool result path is a link; not persisted".into());
    }
    check_existing_file(&metadata)?;
    check_link_count(u64::from(links))
}

/// Port of 2.1.263 `tG`: persist exclusively after checking the directory and
/// removing an existing leaf symlink (`PIn` / `OIn`). Existing regular files
/// with a single name are tolerated, preserving their contents.
///
/// # Errors
/// Returns an error if the path is unsafe or the exclusive write fails.
pub async fn persist(
    config_home: &Path,
    dir: &Path,
    id: &str,
    body: &lingxi_core::types::utf16_json::Utf16JsonProjection,
    is_json: bool,
    cap_bytes: usize,
) -> Result<Persisted, String> {
    let stem = result_file_stem(id);
    persist_with_stem(config_home, dir, &stem, body, is_json, cap_bytes).await
}

/// Persist one physical Mod core execution in a namespace disjoint from
/// ordinary provider IDs. A retry shares the logical tool-use ID but owns
/// different complete output, so its preview must reference its own file.
pub(crate) async fn persist_mod_run(
    config_home: &Path,
    dir: &Path,
    run_id: uuid::Uuid,
    body: &lingxi_core::types::utf16_json::Utf16JsonProjection,
    is_json: bool,
    cap_bytes: usize,
) -> Result<Persisted, String> {
    let stem = format!("mod-run-{run_id}");
    persist_with_stem(config_home, dir, &stem, body, is_json, cap_bytes).await
}

async fn persist_with_stem(
    config_home: &Path,
    dir: &Path,
    stem: &str,
    body: &lingxi_core::types::utf16_json::Utf16JsonProjection,
    is_json: bool,
    cap_bytes: usize,
) -> Result<Persisted, String> {
    // `y = v4(h, a)` — cut the body to the byte cap BEFORE anything is written, so
    // a runaway tool result cannot fill the disk. `originalSize` still reports
    // the FULL length (`p.length`), which is what makes the envelope able to
    // say how much was dropped.
    let units = body
        .at_pointer("")
        .and_then(|value| value.code_units())
        .ok_or_else(|| "persisted tool body must be a JSON string".to_owned())?;
    let capped = truncate_utf8_bytes(units, cap_bytes);
    let truncated_at = (capped.len() != units.len()).then_some(cap_bytes);
    let original_size = units.len();
    // The OS byte boundary follows JS UTF-8 encoding; preview and lengths
    // remain based on the source string before this conversion.
    check_directory(config_home, dir).await?;
    let _ = tokio::fs::create_dir_all(dir).await;
    check_directory(config_home, dir).await?;
    let filepath = dir.join(format!("{stem}.{}", if is_json { "json" } else { "txt" }));
    match tokio::fs::symlink_metadata(&filepath).await {
        Ok(meta) if meta.file_type().is_symlink() => {
            match tokio::fs::remove_file(&filepath).await {
                Ok(()) => {}
                Err(err) if err.kind() == std::io::ErrorKind::NotFound => {}
                Err(err) => return Err(err.to_string()),
            }
        }
        Ok(_) => {}
        Err(err) if err.kind() == std::io::ErrorKind::NotFound => {}
        Err(err) => return Err(err.to_string()),
    }

    match tokio::fs::OpenOptions::new()
        .write(true)
        .create_new(true)
        .open(&filepath)
        .await
    {
        Ok(mut file) => {
            use tokio::io::AsyncWriteExt as _;
            // Decode continuously (including pairs crossing chunk boundaries),
            // but retain only a bounded UTF-8 chunk while awaiting disk I/O.
            let mut encoded = String::with_capacity(16 * 1024);
            for decoded in char::decode_utf16(capped.iter().copied()) {
                encoded.push(decoded.unwrap_or(char::REPLACEMENT_CHARACTER));
                if encoded.len() >= 16 * 1024 {
                    file.write_all(encoded.as_bytes())
                        .await
                        .map_err(|error| error.to_string())?;
                    encoded.clear();
                }
            }
            file.write_all(encoded.as_bytes())
                .await
                .map_err(|error| error.to_string())?;
            if let Err(e) = file.flush().await {
                return Err(e.to_string());
            }
        }
        Err(e) if e.kind() == std::io::ErrorKind::AlreadyExists => {
            let meta = tokio::fs::symlink_metadata(&filepath)
                .await
                .map_err(|_| "tool result path could not be checked; not persisted".to_string())?;
            check_existing_file(&meta)?;
            #[cfg(windows)]
            check_existing_windows_file(&filepath)?;
        }
        Err(e) => return Err(e.to_string()),
    }

    // `Ytt(c, …)` — the preview comes off the WRITTEN body, not the original,
    // so a capped result previews what the file actually contains.
    let (preview_utf16, has_more) = preview_utf16(capped, PREVIEW_CHARS);
    Ok(Persisted {
        filepath,
        original_size,
        is_json,
        preview: String::from_utf16_lossy(&preview_utf16),
        preview_utf16,
        has_more,
        truncated_at,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    // T1 — `pl`. Values byte-locked against the real binary's own on-disk
    // census (`~/.claude/projects/**/tool-results/`) plus the transcript
    // strings that reference those files.
    #[test]
    fn format_bytes_matches_oracle_pl() {
        assert_eq!(format_bytes(500), "500 bytes");
        assert_eq!(format_bytes(1023), "1023 bytes");
        // `pl(_or)` — the literal that appears in every real envelope.
        assert_eq!(format_bytes(2_000), "2KB");
        // b25o2lm9g.txt: 30 075 bytes on disk.
        assert_eq!(format_bytes(30_075), "29.4KB");
        // Largest persisted file observed with a MB rendering.
        assert_eq!(format_bytes(3_485_677), "3.3MB");
        assert_eq!(format_bytes(2 * 1024 * 1024 * 1024), "2GB");
        // `.toFixed(1).replace(/\.0$/,"")` — the trailing `.0` is dropped.
        assert_eq!(format_bytes(1024), "1KB");
        assert_eq!(format_bytes(1280), "1.3KB");
        assert_eq!(format_bytes(2304), "2.3KB");
    }

    // T2 — `xKr`.
    #[test]
    fn preview_cuts_at_last_newline_past_the_midpoint() {
        // Newline at index 1500 (> 2000 * 0.5) → cut there, newline excluded.
        let mut s = "a".repeat(1_500);
        s.push('\n');
        s.push_str(&"b".repeat(2_000));
        let (p, more) = preview(&s, PREVIEW_CHARS);
        assert!(more);
        assert_eq!(p.len(), 1_500);
        assert_eq!(p, "a".repeat(1_500));
    }

    #[test]
    fn preview_falls_back_to_the_hard_limit_when_the_newline_is_too_early() {
        // Newline at index 10 (<= 2000 * 0.5) → hard cut at 2000.
        let mut s = "a".repeat(10);
        s.push('\n');
        s.push_str(&"b".repeat(4_000));
        let (p, more) = preview(&s, PREVIEW_CHARS);
        assert!(more);
        assert_eq!(p.len(), PREVIEW_CHARS);
    }

    #[test]
    fn preview_passes_short_input_through() {
        let (p, more) = preview("hello", PREVIEW_CHARS);
        assert_eq!(p, "hello");
        assert!(!more);
        // `e.length<=t` — exactly at the limit is NOT truncated.
        let s = "x".repeat(PREVIEW_CHARS);
        let (p, more) = preview(&s, PREVIEW_CHARS);
        assert_eq!(p.len(), PREVIEW_CHARS);
        assert!(!more);
    }

    #[test]
    fn preview_uses_utf16_units_and_keeps_valid_unicode() {
        let s = "→".repeat(700);
        assert_eq!(preview(&s, PREVIEW_CHARS), (s.clone(), false));
        assert_eq!(preview("😀ab", 2), ("😀".to_string(), true));
        assert_eq!(preview("😀ab", 1), ("".to_string(), true));
        assert_eq!(preview_utf16(&[0xd83d, 0xde00, 97, 98], 1), (vec![], true));
        assert_eq!(preview("→→→\nabcd", 6), ("→→→\nab".to_string(), true));
        assert_eq!(preview("→→→→\nabcd", 6), ("→→→→".to_string(), true));
        assert_eq!(preview("", 0), ("".to_string(), false));
        assert_eq!(preview("a", 0), ("".to_string(), true));
    }

    // T3 — `Alt`, byte-exact.
    #[test]
    fn wrap_is_byte_exact() {
        let out = wrap(30_075, "/tmp/s/tool-results/x.txt", "PREVIEW", true, None);
        assert_eq!(
            out,
            "<persisted-output>\nOutput too large (29.4KB). Full output saved to: \
             /tmp/s/tool-results/x.txt\n\nPreview (first 2KB):\nPREVIEW\n...\n</persisted-output>"
        );
    }

    #[test]
    fn wrap_without_more_uses_a_single_newline_tail() {
        let out = wrap(1_024, "/p.txt", "P", false, None);
        assert_eq!(
            out,
            "<persisted-output>\nOutput too large (1KB). Full output saved to: \
             /p.txt\n\nPreview (first 2KB):\nP\n</persisted-output>"
        );
    }

    #[tokio::test]
    async fn persist_keeps_source_preview_until_the_utf8_file_boundary() {
        let root = tempfile::tempdir().unwrap();
        let dir = root.path().join("tool-results");
        let body =
            lingxi_core::types::utf16_json::Utf16JsonProjection::parse(r#""before \ud800 after""#)
                .unwrap();
        let persisted = persist(root.path(), &dir, "source", &body, false, 100)
            .await
            .unwrap();
        assert_eq!(persisted.preview_utf16, body.string_units("").unwrap());
        assert_eq!(persisted.original_size, 14);
        assert_eq!(
            std::fs::read(persisted.filepath).unwrap(),
            "before \u{fffd} after".as_bytes()
        );
    }

    // T4 — `x2e`.
    #[tokio::test]
    async fn persist_writes_txt_and_creates_the_directory() {
        let tmp = tempfile::tempdir().expect("tempdir");
        let dir = tmp.path().join("sess").join(TOOL_RESULTS_DIR);
        let body = "hello world";
        let p = persist(
            tmp.path(),
            &dir,
            "toolu_abc",
            &(body).into(),
            false,
            MAX_PERSIST_UTF16_UNITS,
        )
        .await
        .expect("persist ok");
        assert_eq!(p.filepath, dir.join("toolu_abc.txt"));
        assert_eq!(p.original_size, body.len());
        assert!(!p.is_json);
        assert!(!p.has_more);
        assert_eq!(p.preview, body);
        assert_eq!(
            std::fs::read_to_string(&p.filepath).expect("read back"),
            body
        );
    }

    #[tokio::test]
    async fn opaque_ids_stay_inside_the_result_directory_and_do_not_alias() {
        let root = tempfile::tempdir().unwrap();
        let home = root.path().join("config");
        let dir = home.join("projects/project/session/tool-results");
        let absolute = root.path().join("outside").to_string_lossy().into_owned();
        let mut ids = vec![
            absolute,
            "../../../../../outside".into(),
            "nested/../../../../../outside".into(),
            r"..\..\outside".into(),
            r"C:\outside".into(),
            r"\\server\share\outside".into(),
            "name:stream".into(),
            "CON".into(),
            "nul".into(),
            "COM1".into(),
            "lpt9".into(),
            "..".into(),
            String::new(),
            "nul\0byte".into(),
            "opaque-provider-id".into(),
            "OPAQUE-provider-id".into(),
            "名字😀".into(),
            "x".repeat(300),
        ];
        let generated = result_file_stem(&ids[1]).into_owned();
        ids.push(generated.clone());
        ids.push(generated.to_ascii_uppercase());
        let mut names = std::collections::HashSet::new();
        for (index, id) in ids.iter().enumerate() {
            for is_json in [false, true] {
                let body = format!("body-{index}-{is_json}");
                let result = persist(&home, &dir, id, &body.clone().into(), is_json, 1024)
                    .await
                    .unwrap();
                assert_eq!(result.filepath.parent(), Some(dir.as_path()));
                assert!(result
                    .filepath
                    .canonicalize()
                    .unwrap()
                    .starts_with(home.canonicalize().unwrap()));
                let name = result.filepath.file_name().unwrap().to_str().unwrap();
                assert!(
                    names.insert(name.to_ascii_lowercase()),
                    "filename alias for {id:?}"
                );
                assert_eq!(std::fs::read_to_string(result.filepath).unwrap(), body);
            }
        }
        assert!(!root.path().join("outside.txt").exists());
        assert!(!root.path().join("outside.json").exists());
    }

    #[tokio::test]
    async fn encoded_id_keeps_the_first_file_and_conventional_ids_keep_their_names() {
        let root = tempfile::tempdir().unwrap();
        let dir = root.path().join("tool-results");
        for id in ["toolu_abc", "call-42", "computer-a1b2"] {
            let result = persist(root.path(), &dir, id, &"body".into(), false, 1024)
                .await
                .unwrap();
            assert_eq!(result.filepath, dir.join(format!("{id}.txt")));
        }
        let first = persist(root.path(), &dir, "../opaque", &"first".into(), false, 1024)
            .await
            .unwrap();
        let second = persist(
            root.path(),
            &dir,
            "../opaque",
            &"second".into(),
            false,
            1024,
        )
        .await
        .unwrap();
        assert_eq!(first.filepath, second.filepath);
        assert_eq!(std::fs::read_to_string(first.filepath).unwrap(), "first");
    }

    #[tokio::test]
    async fn physical_mod_run_files_are_disjoint_from_exact_provider_ids() {
        let root = tempfile::tempdir().unwrap();
        let dir = root.path().join("tool-results");
        let run_id = uuid::Uuid::new_v4();
        let provider_id = format!("mod-run-{run_id}");
        let run = persist_mod_run(root.path(), &dir, run_id, &"run".into(), false, 1024)
            .await
            .unwrap();
        assert_eq!(run.filepath, dir.join(format!("{provider_id}.txt")));
        let provider = persist(
            root.path(),
            &dir,
            &provider_id,
            &"provider-first".into(),
            false,
            1024,
        )
        .await
        .unwrap();
        let provider_retry = persist(
            root.path(),
            &dir,
            &provider_id,
            &"provider-second".into(),
            false,
            1024,
        )
        .await
        .unwrap();
        assert_ne!(provider.filepath, run.filepath);
        assert_eq!(provider.filepath, provider_retry.filepath);
        assert_eq!(
            std::fs::read_to_string(provider.filepath).unwrap(),
            "provider-first"
        );
        assert_eq!(std::fs::read_to_string(run.filepath).unwrap(), "run");

        let units = vec![0xd800, b'-' as u16, b'x' as u16];
        let exact_body = lingxi_core::types::utf16_json::Utf16JsonProjection::root_string(
            "\u{fffd}-x".into(),
            units.clone(),
        )
        .unwrap();
        let exact = persist_mod_run(
            root.path(),
            &dir,
            uuid::Uuid::new_v4(),
            &exact_body,
            false,
            1024,
        )
        .await
        .unwrap();
        assert_eq!(exact.preview_utf16, units);
        assert_eq!(
            std::fs::read_to_string(exact.filepath).unwrap(),
            "\u{fffd}-x"
        );
    }

    #[tokio::test]
    async fn case_distinct_provider_ids_use_disjoint_reserved_filenames() {
        let root = tempfile::tempdir().unwrap();
        let dir = root.path().join("tool-results");
        let encoded = result_file_stem("toolu_Aa").into_owned();
        assert!(encoded.starts_with("opaque-"));
        let ids = ["toolu_Aa", "toolu_aa", encoded.as_str()];
        let mut names = std::collections::HashSet::new();
        for (index, id) in ids.into_iter().enumerate() {
            let body = format!("result-{index}");
            let first = persist(root.path(), &dir, id, &body.clone().into(), false, 1024)
                .await
                .unwrap();
            let name = first.filepath.file_name().unwrap().to_str().unwrap();
            assert!(names.insert(name.to_ascii_lowercase()), "alias for {id}");
            assert_eq!(std::fs::read_to_string(&first.filepath).unwrap(), body);

            let retry = persist(root.path(), &dir, id, &"ignored retry".into(), false, 1024)
                .await
                .unwrap();
            assert_eq!(retry.filepath, first.filepath);
            assert_eq!(std::fs::read_to_string(&retry.filepath).unwrap(), body);
        }
    }

    #[tokio::test]
    async fn persist_uses_a_json_extension_for_array_bodies() {
        let tmp = tempfile::tempdir().expect("tempdir");
        let dir = tmp.path().join(TOOL_RESULTS_DIR);
        let p = persist(
            tmp.path(),
            &dir,
            "id1",
            &("[\n  1\n]").into(),
            true,
            MAX_PERSIST_UTF16_UNITS,
        )
        .await
        .expect("persist ok");
        assert_eq!(p.filepath, dir.join("id1.json"));
        assert!(p.is_json);
    }

    #[tokio::test]
    async fn persist_treats_an_existing_file_as_success() {
        let tmp = tempfile::tempdir().expect("tempdir");
        let dir = tmp.path().join(TOOL_RESULTS_DIR);
        std::fs::create_dir_all(&dir).expect("mkdir");
        std::fs::write(dir.join("id1.txt"), "OLD").expect("seed");
        let p = persist(
            tmp.path(),
            &dir,
            "id1",
            &("NEW-BODY").into(),
            false,
            MAX_PERSIST_UTF16_UNITS,
        )
        .await
        .expect("EEXIST is success");
        assert_eq!(p.filepath, dir.join("id1.txt"));
        // The oracle does NOT overwrite: `writeExclusive` threw, the catch
        // swallowed EEXIST, and the pre-existing file stays as it was.
        assert_eq!(
            std::fs::read_to_string(dir.join("id1.txt")).expect("read back"),
            "OLD"
        );
        // …but the envelope still describes the NEW body (`o.length`).
        assert_eq!(p.original_size, "NEW-BODY".len());
    }

    #[tokio::test]
    async fn persisted_size_counts_utf16_units() {
        let tmp = tempfile::tempdir().expect("tempdir");
        let dir = tmp.path().join(TOOL_RESULTS_DIR);
        let result = persist(
            tmp.path(),
            &dir,
            "unicode",
            &("中😀").into(),
            false,
            MAX_PERSIST_UTF16_UNITS,
        )
        .await
        .unwrap();
        assert_eq!(result.original_size, 3);
        assert_eq!(std::fs::read_to_string(result.filepath).unwrap(), "中😀");
    }

    #[tokio::test]
    async fn persist_rejects_existing_directory() {
        let tmp = tempfile::tempdir().unwrap();
        let dir = tmp.path().join(TOOL_RESULTS_DIR);
        std::fs::create_dir_all(dir.join("id.txt")).unwrap();
        assert_eq!(
            persist(
                tmp.path(),
                &dir,
                "id",
                &("new").into(),
                false,
                MAX_PERSIST_UTF16_UNITS
            )
            .await
            .unwrap_err(),
            "tool result path is not a regular file; not persisted"
        );
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn persist_replaces_leaf_symlink_without_touching_target() {
        use std::os::unix::fs::symlink;
        let tmp = tempfile::tempdir().unwrap();
        let dir = tmp.path().join(TOOL_RESULTS_DIR);
        std::fs::create_dir(&dir).unwrap();
        let target = tmp.path().join("target");
        std::fs::write(&target, "old").unwrap();
        symlink(&target, dir.join("id.txt")).unwrap();
        let result = persist(
            tmp.path(),
            &dir,
            "id",
            &("new").into(),
            false,
            MAX_PERSIST_UTF16_UNITS,
        )
        .await
        .unwrap();
        assert_eq!(std::fs::read_to_string(target).unwrap(), "old");
        assert_eq!(std::fs::read_to_string(result.filepath).unwrap(), "new");
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn persist_allows_config_root_symlink_but_rejects_collision_symlink() {
        use std::os::unix::fs::symlink;
        let tmp = tempfile::tempdir().unwrap();
        let real = tmp.path().join("real");
        std::fs::create_dir(&real).unwrap();
        let root = tmp.path().join("config");
        symlink(&real, &root).unwrap();
        let dir = root.join(TOOL_RESULTS_DIR);
        persist(
            &root,
            &dir,
            "id",
            &("new").into(),
            false,
            MAX_PERSIST_UTF16_UNITS,
        )
        .await
        .unwrap();
        let link = dir.join("collision.txt");
        symlink(dir.join("id.txt"), &link).unwrap();
        // `OIn` removes preexisting symlinks. A symlink appearing at the
        // subsequent EEXIST check must instead be rejected.
        assert_eq!(
            check_existing_file(&std::fs::symlink_metadata(link).unwrap()).unwrap_err(),
            "tool result path is a link; not persisted"
        );
    }

    #[test]
    fn existing_file_link_count_rejects_additional_names() {
        assert!(check_link_count(1).is_ok());
        assert_eq!(
            check_link_count(2).unwrap_err(),
            "tool result path has another name; not persisted"
        );
        assert!(check_link_count(u64::MAX).is_err());
    }

    #[cfg(windows)]
    #[tokio::test]
    async fn persist_windows_checks_existing_file_handle_link_count() {
        let tmp = tempfile::tempdir().unwrap();
        let dir = tmp.path().join(TOOL_RESULTS_DIR);
        std::fs::create_dir(&dir).unwrap();
        let target = tmp.path().join("target");
        std::fs::write(&target, "old").unwrap();
        let collision = dir.join("id.txt");
        std::fs::hard_link(&target, &collision).unwrap();
        assert_eq!(
            persist(
                tmp.path(),
                &dir,
                "id",
                &("new").into(),
                false,
                MAX_PERSIST_UTF16_UNITS
            )
            .await
            .unwrap_err(),
            "tool result path has another name; not persisted"
        );
        assert_eq!(std::fs::read_to_string(&target).unwrap(), "old");
        std::fs::remove_file(target).unwrap();
        persist(
            tmp.path(),
            &dir,
            "id",
            &("new").into(),
            false,
            MAX_PERSIST_UTF16_UNITS,
        )
        .await
        .unwrap();
        assert_eq!(std::fs::read_to_string(collision).unwrap(), "old");
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn persist_rejects_hardlink_and_symlinked_parent() {
        use std::os::unix::fs::symlink;
        let tmp = tempfile::tempdir().unwrap();
        let dir = tmp.path().join(TOOL_RESULTS_DIR);
        std::fs::create_dir(&dir).unwrap();
        let target = tmp.path().join("target");
        std::fs::write(&target, "old").unwrap();
        std::fs::hard_link(&target, dir.join("id.txt")).unwrap();
        assert_eq!(
            persist(
                tmp.path(),
                &dir,
                "id",
                &("new").into(),
                false,
                MAX_PERSIST_UTF16_UNITS
            )
            .await
            .unwrap_err(),
            "tool result path has another name; not persisted"
        );
        let link = tmp.path().join("linked");
        symlink(&dir, &link).unwrap();
        let err = persist(
            tmp.path(),
            &link.join("nested"),
            "other",
            &("new").into(),
            false,
            MAX_PERSIST_UTF16_UNITS,
        )
        .await
        .unwrap_err();
        assert!(err.contains("is a link or not a directory"));
        assert!(!dir.join("nested").exists());
        assert_eq!(std::fs::read_to_string(target).unwrap(), "old");
    }

    /// `M0u`'s fold, and the two constants it is built from.
    #[test]
    fn resolve_threshold_is_the_m0u_fold() {
        // `AKr` / `RKr` (2.1.220 @ 230268660).
        assert_eq!(DEFAULT_PERSISTENCE_CEILING, 50_000);
        assert_eq!(CHARS_PER_TOKEN, 4);
        // Bash / PowerShell: `min(30000, AKr)`.
        assert_eq!(resolve_threshold(30_000, None), 30_000);
        // A tool declaring more than the default ceiling is clamped to it.
        assert_eq!(resolve_threshold(300_000, None), 50_000);
        // The MCP factory raises the ceiling to `gor = 500000`.
        assert_eq!(resolve_threshold(300_000, Some(500_000)), 300_000);
        assert_eq!(resolve_threshold(900_000, Some(500_000)), 500_000);
    }

    #[test]
    fn tool_results_dir_is_session_scoped() {
        let d = tool_results_dir(Path::new("/home/u/.lingxi"), "/w/p", "uuid-1");
        assert_eq!(
            d,
            PathBuf::from("/home/u/.lingxi/projects/-w-p/uuid-1/tool-results")
        );
    }

    // ---- Current byte cap (`xJi` / `v4`, 2.1.319) ----------------------

    #[test]
    fn the_cap_is_the_oracle_constant_and_is_not_the_spool_cap() {
        assert_eq!(MAX_PERSIST_BYTES, 1_073_741_824);
        assert_eq!(MAX_PERSIST_UTF16_UNITS, MAX_PERSIST_BYTES);
        // ⛔ 1 GiB, NOT the background-task spool's 5 GB write cap. The two
        // guard different surfaces and live in different upstream files; the
        // backlog conflated them, which is how this constant went missing.
        assert_ne!(MAX_PERSIST_BYTES, 5 * 1024 * 1024 * 1024);
    }

    #[test]
    fn truncation_counts_utf8_bytes_and_never_splits_a_surrogate() {
        assert_eq!(
            truncate_utf8_bytes(&"a\u{1F600}b".encode_utf16().collect::<Vec<_>>(), 5),
            "a\u{1F600}".encode_utf16().collect::<Vec<_>>()
        );
        assert_eq!(
            truncate_utf8_bytes(&"a\u{1F600}b".encode_utf16().collect::<Vec<_>>(), 4),
            "a".encode_utf16().collect::<Vec<_>>()
        );
        assert_eq!(
            truncate_utf8_bytes(&"a\u{1F600}b".encode_utf16().collect::<Vec<_>>(), 6),
            "a\u{1F600}b".encode_utf16().collect::<Vec<_>>()
        );
        assert_eq!(
            truncate_utf8_bytes(&"abc".encode_utf16().collect::<Vec<_>>(), 0),
            "".encode_utf16().collect::<Vec<_>>()
        );
        // Under the cap is returned untouched.
        assert_eq!(
            truncate_utf8_bytes(&"abc".encode_utf16().collect::<Vec<_>>(), 99),
            "abc".encode_utf16().collect::<Vec<_>>()
        );
    }

    fn native_319_fixture() -> serde_json::Value {
        serde_json::from_str(include_str!(
            "../tests/fixtures/tool_result_persistence_319.json"
        ))
        .unwrap()
    }

    fn fixture_units(value: &serde_json::Value) -> Vec<u16> {
        value
            .as_array()
            .unwrap()
            .iter()
            .map(|unit| u16::try_from(unit.as_u64().unwrap()).unwrap())
            .collect()
    }

    #[test]
    fn native_319_preview_unicode_and_newline_boundaries() {
        for case in native_319_fixture()["previewCases"].as_array().unwrap() {
            let input = fixture_units(&case["inputUnits"]);
            let limit = usize::try_from(case["limit"].as_u64().unwrap()).unwrap();
            assert_eq!(
                preview_utf16(&input, limit),
                (
                    fixture_units(&case["expectedPreviewUnits"]),
                    case["hasMore"].as_bool().unwrap()
                ),
                "{}",
                case["name"]
            );
        }
    }

    #[tokio::test]
    async fn native_319_caps_keep_source_units_and_write_exact_utf8_bytes() {
        let root = tempfile::tempdir().unwrap();
        let dir = root.path().join(TOOL_RESULTS_DIR);
        for case in native_319_fixture()["capCases"].as_array().unwrap() {
            let name = case["name"].as_str().unwrap();
            let units = fixture_units(&case["inputUnits"]);
            let source = lingxi_core::types::utf16_json::Utf16JsonProjection::root_string(
                String::from_utf16_lossy(&units),
                units,
            )
            .unwrap();
            let cap = usize::try_from(case["capBytes"].as_u64().unwrap()).unwrap();
            let persisted = persist(root.path(), &dir, name, &source, false, cap)
                .await
                .unwrap();
            let written = std::fs::read(&persisted.filepath).unwrap();
            let hex = written
                .iter()
                .map(|byte| format!("{byte:02x}"))
                .collect::<String>();
            assert_eq!(hex, case["expectedFileHex"].as_str().unwrap(), "{name}");
            assert!(written.len() <= cap, "{name}");
            assert_eq!(
                persisted.original_size,
                usize::try_from(case["originalSize"].as_u64().unwrap()).unwrap(),
                "{name}"
            );
            assert_eq!(
                persisted.truncated_at,
                case["truncatedAtBytes"]
                    .as_u64()
                    .map(|value| usize::try_from(value).unwrap()),
                "{name}"
            );
            assert_eq!(
                persisted.preview_utf16,
                fixture_units(&case["expectedPreviewUnits"]),
                "{name}"
            );
            assert_eq!(
                persisted.has_more,
                case["hasMore"].as_bool().unwrap(),
                "{name}"
            );
        }
    }

    #[tokio::test]
    async fn chunked_utf8_persistence_keeps_pairs_lone_units_and_exact_preview() {
        let temp = tempfile::tempdir().unwrap();
        let dir = temp.path().join("tool-results");
        let mut units = vec![u16::from(b'a'); 16 * 1024 - 1];
        units.extend([0xd83d, 0xde00, 0xd800, u16::from(b'z'), 0xdc00]);
        let body = lingxi_core::types::utf16_json::Utf16JsonProjection::root_string(
            String::from_utf16_lossy(&units),
            units.clone(),
        )
        .unwrap();
        let persisted = persist(temp.path(), &dir, "chunked", &body, false, usize::MAX)
            .await
            .unwrap();
        assert_eq!(
            std::fs::read_to_string(&persisted.filepath).unwrap(),
            String::from_utf16_lossy(&units)
        );
        assert_eq!(persisted.original_size, units.len());
        assert_eq!(persisted.preview_utf16, units[..PREVIEW_CHARS]);
        assert!(persisted.has_more);
    }

    #[tokio::test]
    async fn a_body_over_the_cap_is_cut_before_it_reaches_disk() {
        let tmp = tempfile::tempdir().expect("temp");
        let dir = tmp.path().join("projects/p/s/tool-results");
        let body = "x".repeat(5_000);

        let p = persist(
            tmp.path(),
            &dir,
            "capped",
            &body.as_str().into(),
            false,
            1_000,
        )
        .await
        .expect("persist");

        // The FILE holds only what the cap allowed…
        let written = std::fs::read_to_string(&p.filepath).expect("read back");
        assert_eq!(
            written.len(),
            1_000,
            "the cap must be applied BEFORE the write, not after"
        );
        // …while `original_size` still reports the whole body, which is what
        // lets the envelope say how much was dropped.
        assert_eq!(p.original_size, 5_000);
        assert_eq!(p.truncated_at, Some(1_000));
    }

    #[tokio::test]
    async fn a_body_under_the_cap_is_untouched_and_unmarked() {
        let tmp = tempfile::tempdir().expect("temp");
        let dir = tmp.path().join("projects/p/s/tool-results");
        let p = persist(tmp.path(), &dir, "small", &("hello").into(), false, 1_000)
            .await
            .expect("persist");
        assert_eq!(
            std::fs::read_to_string(&p.filepath).expect("read back"),
            "hello"
        );
        assert_eq!(p.original_size, 5);
        assert_eq!(
            p.truncated_at, None,
            "an uncapped body must not claim it was truncated"
        );
    }

    #[test]
    fn the_envelope_says_which_kind_of_large_it_was() {
        // Untruncated: the size that did not fit, and "Full output".
        let full = wrap(30_075, "/t/x.txt", "P", false, None);
        assert!(full.contains("Output too large (29.4KB). Full output saved to: /t/x.txt"));
        assert!(!full.contains("persist limit"));

        // Truncated: the CAP, twice, and no claim that the file is complete.
        let cut = wrap(5_000_000_000, "/t/x.txt", "P", false, Some(1_073_741_824));
        assert!(
            cut.contains(
                "Output exceeded the 1GB persist limit; only the first 1GB were saved to: /t/x.txt"
            ),
            "{cut}"
        );
        assert!(
            !cut.contains("Full output saved"),
            "a truncated file must never be described as the full output: {cut}"
        );
        // The UTF-16 twin renders the same lead.
        let cut16 = String::from_utf16_lossy(&wrap_utf16(
            5_000_000_000,
            "/t/x.txt",
            &"P".encode_utf16().collect::<Vec<_>>(),
            false,
            Some(1_073_741_824),
        ));
        assert_eq!(cut16, cut);
    }
}
