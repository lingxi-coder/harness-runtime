//! Tool-result persistence — the `<persisted-output>` substitution.
//!
//! Full content exceeding a tool's persistence threshold is stored under
//! `<config_home>/projects/<project>/<session>/tool-results/<id>.txt`
//! (or `.json` for content arrays) and replaced by a preview envelope.
//!
//! Oracle: Claude Code 2.1.263, `src_160701526.js`, `tG` at character
//! offset 5118 (exclusive persistence), `_7e` at 7648 (preview), and `Vpe`
//! (envelope). Imported `PIn` / `OIn` / `_L` in `src_157669121.js` supply
//! directory checking, leaf symlink removal, and checked directory creation.
//!
//! `tG` returns `originalSize:l.length` and `_7e` slices JavaScript UTF-16
//! code units. The envelope labels these values as bytes, but they are not
//! UTF-8 file sizes. Externally persisted Bash output has a separate byte-size
//! path and must not be used to infer these semantics.

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

/// `fAr = 1073741824` — `E2`'s default persist cap (TL-6, 2.1.266).
///
/// ⚠️ The name says BYTES because upstream's envelope does, but the value is
/// compared against `p.length` — JavaScript UTF-16 code units — exactly like
/// [`Persisted::original_size`] and [`PREVIEW_CHARS`]. A 1 GiB cap over UTF-16
/// units is up to 3 GiB of UTF-8 on disk for CJK text; that is upstream's
/// behaviour, inherited deliberately rather than "fixed" into a byte cap that
/// would truncate a non-ASCII result earlier than the oracle does.
///
/// ⛔ Do NOT confuse this with the background-task spool caps in
/// `tasks::output_manager` (5 GB write / 8 MiB read). Those are `diskOutput.ts`
/// — a different upstream file guarding a different surface.
pub const MAX_PERSIST_UTF16_UNITS: usize = 1_073_741_824;

/// Truncate to at most `limit` UTF-16 code units without splitting a surrogate
/// pair — the slice half of upstream's cap helper, WITHOUT the newline-midpoint
/// trim [`preview_utf16`] applies. A preview may end on a tidy line; a
/// truncated persisted body must keep every unit that fits.
#[must_use]
fn truncate_utf16_units(s: &str, limit: usize) -> String {
    if limit == 0 {
        return String::new();
    }
    let mut units: Vec<u16> = s.encode_utf16().collect();
    if units.len() <= limit {
        return s.to_string();
    }
    units.truncate(limit);
    // A trailing HIGH surrogate has lost its partner; dropping it is what keeps
    // the written file valid text rather than ending in a replacement char.
    if units.last().is_some_and(|u| (0xD800..=0xDBFF).contains(u)) {
        units.pop();
    }
    String::from_utf16_lossy(&units)
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
    /// Exact JavaScript preview, including a split surrogate at the slice boundary.
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

/// Port of claude-code `xKr` (2.1.220 @ 230270990):
///
/// ```text
/// if(e.length<=t)return{preview:e,hasMore:!1};
/// let n=e.slice(0,t).lastIndexOf("\n"),o=n>t*0.5?n:t;
/// return{preview:e.slice(0,o),hasMore:!0}
/// ```
///
/// Display view of the exact JavaScript preview. Use `preview_utf16` for wire data.
#[must_use]
pub fn preview(s: &str, limit: usize) -> (String, bool) {
    let (units, more) = preview_utf16(s, limit);
    (String::from_utf16_lossy(&units), more)
}

/// JavaScript `slice` and newline midpoint measured in UTF-16 code units.
#[must_use]
pub fn preview_utf16(s: &str, limit: usize) -> (Vec<u16>, bool) {
    let mut units: Vec<u16> = s.encode_utf16().collect();
    if units.len() <= limit {
        return (units, false);
    }
    units.truncate(limit);
    if let Some(index) = units.iter().rposition(|unit| *unit == u16::from(b'\n')) {
        if index > limit / 2 {
            units.truncate(index);
        }
    }
    (units, true)
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
    body: &str,
    is_json: bool,
    cap_utf16_units: usize,
) -> Result<Persisted, String> {
    // `c = hFt(p, l)` — cut the body to the cap BEFORE anything is written, so
    // a runaway tool result cannot fill the disk. `originalSize` still reports
    // the FULL length (`p.length`), which is what makes the envelope able to
    // say how much was dropped.
    let capped = truncate_utf16_units(body, cap_utf16_units);
    let truncated_at = (capped.len() != body.len()).then_some(cap_utf16_units);
    let original_size = body.encode_utf16().count();
    let body: &str = &capped;
    check_directory(config_home, dir).await?;
    let _ = tokio::fs::create_dir_all(dir).await;
    check_directory(config_home, dir).await?;
    let filepath = dir.join(format!("{id}.{}", if is_json { "json" } else { "txt" }));
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
            if let Err(e) = file.write_all(body.as_bytes()).await {
                return Err(e.to_string());
            }
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
    let (preview_utf16, has_more) = preview_utf16(body, PREVIEW_CHARS);
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
        assert_eq!(preview("😀ab", 1), ("\u{fffd}".to_string(), true));
        assert_eq!(preview_utf16("😀ab", 1), (vec![0xd83d], true));
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
            body,
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
    async fn persist_uses_a_json_extension_for_array_bodies() {
        let tmp = tempfile::tempdir().expect("tempdir");
        let dir = tmp.path().join(TOOL_RESULTS_DIR);
        let p = persist(
            tmp.path(),
            &dir,
            "id1",
            "[\n  1\n]",
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
            "NEW-BODY",
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
            "中😀",
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
                "new",
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
            "new",
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
        persist(&root, &dir, "id", "new", false, MAX_PERSIST_UTF16_UNITS)
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
                "new",
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
            "new",
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
                "new",
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
            "new",
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

    // ---- TL-6: the persist cap (oracle `fAr`, 2.1.266) --------------------

    #[test]
    fn the_cap_is_the_oracle_constant_and_is_not_the_spool_cap() {
        assert_eq!(MAX_PERSIST_UTF16_UNITS, 1_073_741_824);
        // ⛔ 1 GiB, NOT the background-task spool's 5 GB write cap. The two
        // guard different surfaces and live in different upstream files; the
        // backlog conflated them, which is how this constant went missing.
        assert_ne!(MAX_PERSIST_UTF16_UNITS, 5 * 1024 * 1024 * 1024);
    }

    #[test]
    fn truncation_counts_utf16_units_and_never_splits_a_surrogate() {
        // An astral char is ONE `char` but TWO UTF-16 units, so a cap of 3 over
        // "a😀b" must keep "a😀" — cutting at 3 would leave a lone high
        // surrogate, which is how a persisted file ends in a replacement char.
        assert_eq!(truncate_utf16_units("a\u{1F600}b", 3), "a\u{1F600}");
        assert_eq!(truncate_utf16_units("a\u{1F600}b", 2), "a");
        assert_eq!(truncate_utf16_units("a\u{1F600}b", 4), "a\u{1F600}b");
        assert_eq!(truncate_utf16_units("abc", 0), "");
        // Under the cap is returned untouched.
        assert_eq!(truncate_utf16_units("abc", 99), "abc");
    }

    #[tokio::test]
    async fn a_body_over_the_cap_is_cut_before_it_reaches_disk() {
        let tmp = tempfile::tempdir().expect("temp");
        let dir = tmp.path().join("projects/p/s/tool-results");
        let body = "x".repeat(5_000);

        let p = persist(tmp.path(), &dir, "capped", &body, false, 1_000)
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
        let p = persist(tmp.path(), &dir, "small", "hello", false, 1_000)
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
