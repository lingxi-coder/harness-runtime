//! `/status` oversized-memory-file warnings — claude-code `htf()`.
//!
//! Lives here, not in the CLI, because BOTH producers need it: the launch-time
//! capture in `apps/cli` and the mount-time recompute in the `/status` screen.
//! One implementation — two would drift, and a divergence between what the
//! panel shows at launch and on reopen is exactly the bug this module closes.

use super::MemoryFile;

/// claude-code `Ad(e)` (2.1.220 binary offset 226626865) — shorten an absolute
/// path for display: cwd-relative when it does not escape upward, else
/// `~`-abbreviated when under `$HOME`, else the absolute path verbatim.
///
/// `home` is an `Option` because Rust's `dirs::home_dir()` can fail where
/// Node's `os.homedir()` cannot (a service account with no `$HOME` and no
/// passwd entry). `None` simply skips the `~` branch — it must NOT suppress the
/// caller's whole result, since the cwd-relative branch is the one that names
/// project-tier files.
pub fn shorten_memory_path(
    path: &std::path::Path,
    cwd: &std::path::Path,
    home: Option<&std::path::Path>,
) -> String {
    // `y0h(e).relativePath` then `if(t && !t.startsWith(".."))`.
    if let Ok(rel) = path.strip_prefix(cwd) {
        if !rel.as_os_str().is_empty() {
            return rel.display().to_string();
        }
    }
    // `if(e.startsWith(homedir()+sep)) return "~"+e.slice(homedir().length)`.
    // Note the oracle requires the SEPARATOR, so `$HOME` itself is not
    // abbreviated — `strip_prefix` on a `Path` has exactly that component
    // boundary semantics.
    if let Some(home) = home {
        if let Ok(rel) = path.strip_prefix(home) {
            if !rel.as_os_str().is_empty() {
                return format!("~{}{}", std::path::MAIN_SEPARATOR, rel.display());
            }
        }
    }
    path.display().to_string()
}

/// claude-code `htf()` (2.1.220 binary offset 241152161) over
/// `fJr(memoryFiles)` (@230809207): one `/status` warning row per LOADED memory
/// file whose body exceeds `pJr()` (@230802563) characters.
///
/// `files` must be the set the system prompt was actually built from (the
/// oracle's `await ZH()`); every gate that decides membership — the
/// `LINGXI_DISABLE_LINGXI_MDS` kill-switch, the Managed tier, `@import`
/// expansion, the loader's 4 MiB `MEMORY_FILE_BYTE_LIMIT` skip — belongs to the
/// provider, not here. This function is deliberately PURE so it stays hermetic
/// under test.
///
/// `model` derives the threshold: `max(40000, round(contextWindow * 0.05 *
/// charsPerToken))`. For every 200k-context model that collapses to the 40 000
/// floor; it only rises for 1M-context models. Pass a CONCRETE model id — the
/// caller owns alias resolution (`Ei`), per
/// [`memory::memory_chars_per_token`]'s contract.
///
/// The `> max_chars` test is [`memory::get_large_memory_files`]' 1:1 port of
/// `fJr` inlined: that helper takes `memory::file::MemoryFile` (mtime +
/// frontmatter), which cannot be built from a loaded LINGXI.md without
/// fabricating fields. Its `tHt`/`LLu` adjudication still applies verbatim —
/// [`memory::lingxi_md::LingxiMdTier`] has exactly `LLu`'s four variants, and
/// LingXi has no synthetic managed path for `tHt` to skip.
pub fn large_memory_warning_rows(
    files: &[MemoryFile],
    cwd: &std::path::Path,
    os_home: Option<&std::path::Path>,
    model: &str,
    active_betas: &[String],
) -> Vec<String> {
    let max_chars = memory::max_memory_character_count(
        llm_runtime::model::context_window::context_window_for_model(model, active_betas),
        memory::memory_chars_per_token(model),
    );
    let max_u64 = u64::try_from(max_chars).unwrap_or(u64::MAX);
    files
        .iter()
        .filter_map(|file| {
            // `fJr` compares `content.length` — UTF-16 code units — on the
            // STRIPPED body (`bn_`'s `content`), never on `rawContent`.
            let chars = memory::memory_file_char_count(&file.body);
            (chars > max_chars).then(|| {
                memory::format_large_memory_file_status_row(
                    &shorten_memory_path(&file.path, cwd, os_home),
                    u64::try_from(chars).unwrap_or(u64::MAX),
                    max_u64,
                )
            })
        })
        .collect()
}
