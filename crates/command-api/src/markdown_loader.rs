//! Filesystem loader for custom markdown slash commands (`.lingxi/commands/**.md`).
//!
//! Faithful port of the discovery half of
//! `claude-code/src/utils/markdownConfigLoader.ts` plus the namespacing helpers
//! from `claude-code/src/skills/loadSkillsDir.ts`:
//!
//! * [`project_dirs_up_to_home`] — the git-root-bounded upward walk collecting
//!   `.lingxi/<subdir>` directories (TS `getProjectDirsUpToHome`).
//! * [`load_command_markdown_files`] — walks managed / user / project dirs,
//!   recursively finds `*.md` via native `std::fs` (TS `findMarkdownFilesNative`
//!   — the `CLAUDE_CODE_USE_NATIVE_FILE_SEARCH` path), parses frontmatter, and
//!   deduplicates by `(dev, ino)` with `managed > user > project` priority
//!   (TS `loadMarkdownFilesForSubdir`).
//! * [`command_name_from_path`] — `buildNamespace` + `getRegularCommandName`
//!   (`:`-joined namespace from the relative directory path).
//! * [`extract_description_from_markdown`] — first non-empty line, `#`-header
//!   stripped, truncated to 100 chars (TS `extractDescriptionFromMarkdown`).
//! * [`build_markdown_command`] — assembles a [`SlashCommand`] of kind
//!   [`SlashCommandKind::Markdown`].
//!
//! ## Divergence from TS (documented intentionally)
//!
//! * **No `walkdir`/`ripgrep`.** Recursion is hand-written with `std::fs`,
//!   mirroring `findMarkdownFilesNative` (`--no-ignore --hidden --follow`):
//!   hidden files are not skipped, `.gitignore` is not consulted, symlinks are
//!   followed, and directory cycles are broken by a `(dev, ino)` visited-set.
//! * **`resolveStopBoundary` submodule refinement is simplified.** Without the
//!   `findCanonicalGitRoot`/`getProjectRoot` helpers (other crates), the upward
//!   walk stops at the nearest `.git` above `cwd`. The worktree fallback in
//!   [`load_command_markdown_files`] is ported in shape: when the nearest `.git`
//!   is a *file* (worktree marker) pointing at a `gitdir`, the resolved main
//!   repo's `.lingxi/<subdir>` is added iff the worktree itself lacks one.
//! * Frontmatter parsing reuses the existing `serde_yaml` dependency and the
//!   `---`/`\n---\n` splitter pattern from `skill::frontmatter`
//!   (`parse_skill_markdown`) without depending on `skill`. It mirrors the
//!   Native YAML repair retry for selected plain scalars and leading tabs; a
//!   closed block still separates its body when metadata parsing ultimately
//!   fails.
//! * Directory-format `.lingxi/skills/<name>/SKILL.md` files use a separate
//!   loader path because their command name comes from the skill directory, not
//!   the `SKILL.md` file stem.

use crate::argument_substitution::{parse_argument_names, FrontmatterArgs};
use crate::model::{
    CommandFrontmatter, CommandSource, NativeCommandType, SlashCommand, SlashCommandKind,
};
use crate::native_locale_collation::native_skill_name_collator;
use icu_collator::CollatorBorrowed;
use lingxi_core::types::utf16_json::Utf16JsonProjection;
use serde::Deserialize;
use std::collections::HashSet;
use std::path::{Component, Path, PathBuf};

const UTF8_BOM: char = '\u{feff}';

#[cfg(unix)]
use std::os::unix::fs::MetadataExt;

/// A loaded markdown command file with its parsed frontmatter and body.
///
/// Mirrors the TS `MarkdownFile` record (minus the analytics `source` enum,
/// which is folded into [`CommandSource`] when building the command).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct MarkdownCommandFile {
    /// Absolute path to the `.md` file on disk.
    pub file_path: PathBuf,
    /// The `.lingxi/<subdir>` directory the file was discovered under, used as
    /// the namespace base (TS `baseDir`).
    pub base_dir: PathBuf,
    /// Parsed YAML frontmatter (defaults when absent).
    pub frontmatter: CommandFrontmatter,
    /// Native plugin prompt factory's `X8t(frontmatter.name, …)` result. Other
    /// command producers may ignore this and keep their own name contract.
    pub frontmatter_name: Option<Utf16JsonProjection>,
    /// Markdown body after the frontmatter block.
    pub content: String,
    /// Which configuration layer the file came from.
    pub source: CommandSource,
}

/// A loaded directory-format skill markdown file.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SkillMarkdownCommandFile {
    /// Absolute path to `<skill>/SKILL.md`.
    pub file_path: PathBuf,
    /// Absolute path to the skill directory.
    pub skill_root: PathBuf,
    /// Parsed YAML frontmatter.
    pub frontmatter: CommandFrontmatter,
    /// Native `Ibe` display name parsed from frontmatter `name`, before the
    /// command factory's fallback to the skill-directory name. Values longer
    /// than Native's 256 UTF-16-unit cap are omitted at parse time.
    pub frontmatter_name: Option<Utf16JsonProjection>,
    /// Markdown body after the frontmatter block.
    pub content: String,
    /// Raw file byte length.
    pub content_length: usize,
    /// Which configuration layer the skill came from.
    pub source: CommandSource,
}

/// Raw YAML frontmatter shape mirroring the TS `FrontmatterData` slice that
/// slash commands consult. Kebab-case keys match the on-disk format. Unknown
/// keys are ignored (TS `[key: string]: unknown`).
#[derive(Debug, Default, Deserialize)]
struct RawFrontmatter {
    /// Native `X8t` display-name source. Its JS `String(...)` coercion is
    /// applied before the 256 UTF-16-unit cap.
    #[serde(default)]
    name: Option<serde_yaml::Value>,
    #[serde(default)]
    description: Option<String>,
    #[serde(default, rename = "allowed-tools")]
    allowed_tools: Option<ToolsField>,
    /// Tools REMOVED from the command's agent. For a `context: fork` skill this
    /// is half of the scoping the fork runs under, so it must survive to the
    /// spawn — a fork that drops it runs with the parent's tools.
    #[serde(default, rename = "disallowed-tools", alias = "disallowedTools")]
    disallowed_tools: Option<ToolsField>,
    #[serde(default)]
    model: Option<String>,
    #[serde(default, rename = "session-modes")]
    session_modes: Option<ToolsField>,
    #[serde(default, rename = "argument-hint")]
    argument_hint: Option<String>,
    #[serde(default)]
    arguments: Option<ArgumentsField>,
    #[serde(default)]
    shell: Option<String>,
    /// SLASH.1: TS `disable-model-invocation` (boolean or the string `"true"`).
    #[serde(default, rename = "disable-model-invocation")]
    disable_model_invocation: Option<Boolish>,
    /// SLASH.4: TS `when_to_use` (`snake_case` key, free-form string).
    #[serde(default)]
    when_to_use: Option<String>,
    /// Gitignore-style patterns that make this a CONDITIONAL skill: it stays
    /// unlisted until the session touches a matching file (claude-code `lhr`).
    #[serde(default)]
    paths: Option<Vec<String>>,
    /// `context: fork` runs the skill as a subagent under its own permission
    /// scoping instead of expanding it inline.
    #[serde(default)]
    context: Option<String>,
    /// Whether a forking skill runs in the background. Absent ⇒ background
    /// (claude's `background ?? true`).
    #[serde(default)]
    background: Option<Boolish>,
    /// The agent type a forking skill spawns.
    #[serde(default)]
    agent: Option<String>,
    /// `effort` — the reasoning effort a forked skill runs under. Carried
    /// RAW; the value domain (`low|medium|high|xhigh|max`, or an integer
    /// 1..=1000) is enforced where it is converted, so an unparseable value
    /// degrades to "declared none" exactly as upstream's `Gx` returns
    /// `undefined`, rather than refusing the skill.
    #[serde(default)]
    effort: Option<EffortField>,
    /// `user-invocable` — whether the skill appears in the `/` menu at all.
    /// Absent means yes; upstream 2.1.267 is
    /// `let dt=v["user-invocable"], en=dt===void 0?!0:htt(dt)`
    /// (`src_163219561.js` @4578657), and the command it builds carries
    /// `isHidden:!(userInvocable??!0)`.
    #[serde(default, rename = "user-invocable")]
    user_invocable: Option<Boolish>,
}

/// `effort` as YAML spells it: `effort: high` is a string, `effort: 500` is an
/// integer. Both are legal in the union upstream validates against, so the raw
/// carrier accepts either and normalizes to text.
#[derive(Debug, Deserialize)]
#[serde(untagged)]
enum EffortField {
    Num(i64),
    Str(String),
}

impl EffortField {
    fn into_raw(self) -> String {
        match self {
            Self::Num(n) => n.to_string(),
            Self::Str(s) => s,
        }
    }
}

/// A frontmatter value the boolean coercer accepts: a real YAML bool, a
/// string, or a bare number (cc 2.1.218 `Kde` takes `boolean|string|number` —
/// unquoted `1`/`0` must not fail the untagged deserialization).
#[derive(Debug, Deserialize)]
#[serde(untagged)]
enum Boolish {
    Bool(bool),
    Num(serde_yaml::Number),
    Str(String),
}

impl Boolish {
    /// cc 2.1.218 `Kde` (`r0e` when introduced): coerce a frontmatter scalar to
    /// a boolean. Bools pass through; strings/numbers are stringified then
    /// trim+lowercase matched against the truthy set `{"true","1","yes","on"}`
    /// and the falsy set `{"false","0","no","off"}` (`Yt`/`su`). Anything else
    /// is `None` — the field is treated as UNDECLARED, not `false`.
    fn coerce(&self) -> Option<bool> {
        let s = match self {
            Self::Bool(b) => return Some(*b),
            // JS `String(number)`: integers print without a fraction. YAML
            // floats keep their dot (`1.0` → "1" in JS vs "1.0" here) — a
            // fractional spelling matches neither set either way, and the
            // integral spellings the changelog names (`1`/`0`) parse as YAML
            // integers, so the sets line up.
            Self::Num(n) => n.to_string(),
            Self::Str(s) => s.clone(),
        };
        match s.trim().to_lowercase().as_str() {
            "true" | "1" | "yes" | "on" => Some(true),
            "false" | "0" | "no" | "off" => Some(false),
            _ => None,
        }
    }
}

/// `allowed-tools` accepts either a single string or a list of strings
/// (TS `string | string[] | null`).
#[derive(Debug, Deserialize)]
#[serde(untagged)]
enum ToolsField {
    One(String),
    Many(Vec<String>),
}

/// `arguments` accepts either a space-separated string or a list of names
/// (TS `string | string[] | undefined`).
#[derive(Debug, Deserialize)]
#[serde(untagged)]
enum ArgumentsField {
    One(String),
    Many(Vec<String>),
}

/// Extract a description from markdown content. Faithful port of TS
/// `extractDescriptionFromMarkdown`: the first non-empty (trimmed) line, with a
/// leading `#`-header prefix stripped, truncated to 100 chars (`> 100` →
/// `substring(0, 97) + "..."`). Falls back to `default_description` when every
/// line is blank.
#[must_use]
pub fn extract_description_from_markdown(content: &str, default_description: &str) -> String {
    for line in content.split('\n') {
        let trimmed = line.trim();
        if trimmed.is_empty() {
            continue;
        }
        // `^#+\s+(.+)$`: one-or-more '#', then one-or-more whitespace, then the
        // rest. When it matches, use the captured tail; otherwise the whole line.
        let text = strip_header_prefix(trimmed).unwrap_or(trimmed);

        return if text.chars().count() > 100 {
            // TS `substring(0, 97) + '...'` — by UTF-16 code units in JS; here
            // we truncate by `char` (Unicode scalar) which matches for the BMP
            // text these descriptions contain.
            let head: String = text.chars().take(97).collect();
            format!("{head}...")
        } else {
            text.to_string()
        };
    }
    default_description.to_string()
}

/// Emulate `^#+\s+(.+)$` on an already-trimmed line. Returns the captured tail
/// (group 1) when the line is a header, else `None`.
fn strip_header_prefix(trimmed: &str) -> Option<&str> {
    let bytes = trimmed.as_bytes();
    let mut i = 0;
    while i < bytes.len() && bytes[i] == b'#' {
        i += 1;
    }
    if i == 0 {
        return None; // no leading '#'
    }
    let after_hashes = i;
    while i < bytes.len() && is_regex_ws(bytes[i]) {
        i += 1;
    }
    // Need at least one whitespace char and a non-empty tail (`(.+)`).
    if i == after_hashes || i >= bytes.len() {
        return None;
    }
    Some(&trimmed[i..])
}

/// `\s` as the JS regex recognises it (ASCII subset that appears in practice).
fn is_regex_ws(b: u8) -> bool {
    matches!(b, b' ' | b'\t' | b'\r' | b'\n' | 0x0b | 0x0c)
}

/// Compute the namespaced command name for a regular `.md` command file.
///
/// Faithful port of TS `getRegularCommandName` + `buildNamespace`: the file name
/// minus the `.md` suffix, prefixed by the `:`-joined path of intermediate
/// directories between `base_dir` and the file's directory. A file directly in
/// `base_dir` has no namespace.
#[must_use]
pub fn command_name_from_path(file: &Path, base_dir: &Path) -> String {
    let file_name = file
        .file_name()
        .map_or_else(String::new, |s| s.to_string_lossy().into_owned());
    let command_base_name = file_name
        .strip_suffix(".md")
        .unwrap_or(&file_name)
        .to_string();

    let file_directory = file.parent().unwrap_or(file);
    let namespace = build_namespace(file_directory, base_dir);
    if namespace.is_empty() {
        command_base_name
    } else {
        format!("{namespace}:{command_base_name}")
    }
}

/// Port of TS `buildNamespace`: the relative path from `base_dir` to
/// `target_dir`, with path separators replaced by `:`. Empty when they are the
/// same directory.
fn build_namespace(target_dir: &Path, base_dir: &Path) -> String {
    // TS strips a single trailing separator from baseDir before comparison; the
    // PathBuf comparison below already ignores trailing separators.
    match target_dir.strip_prefix(base_dir) {
        Ok(rel) => rel
            .components()
            .filter_map(|c| match c {
                Component::Normal(s) => Some(s.to_string_lossy().into_owned()),
                _ => None,
            })
            .collect::<Vec<_>>()
            .join(":"),
        // Not under base_dir (shouldn't happen): no namespace (TS returns '').
        Err(_) => String::new(),
    }
}

/// Traverse from `cwd` up to the git root (or home if not in a git repo),
/// collecting existing `.lingxi/<subdir>` directories. Faithful port of TS
/// `getProjectDirsUpToHome` with the simplified `resolveStopBoundary` (nearest
/// `.git` above `cwd`; see the module divergence note).
///
/// Result ordering is most-specific (cwd) to least-specific, matching TS.
#[must_use]
pub fn project_dirs_up_to_home(subdir: &str, cwd: &Path, home: &Path) -> Vec<PathBuf> {
    let git_root = nearest_git_root(cwd);
    let home_norm = normalize_for_comparison(home);
    let mut current = cwd.to_path_buf();
    let mut dirs: Vec<PathBuf> = Vec::new();

    loop {
        // Stop at home (loaded separately as the user dir).
        if normalize_for_comparison(&current) == home_norm {
            break;
        }

        let lingxi_subdir = current.join(branding::DOT_DIR).join(subdir);
        // Perf filter: only existing dirs (the worktree fallback relies on this).
        if std::fs::metadata(&lingxi_subdir).is_ok() {
            dirs.push(lingxi_subdir);
        }

        // Stop after processing the git-root directory.
        if let Some(root) = &git_root {
            if normalize_for_comparison(&current) == normalize_for_comparison(root) {
                break;
            }
        }

        match current.parent() {
            Some(parent) if parent != current => current = parent.to_path_buf(),
            _ => break,
        }
    }

    dirs
}

/// Find the nearest ancestor (inclusive of `start`) containing a `.git` entry
/// (file or directory). Returns the directory holding `.git`, or `None` if none
/// is found before the filesystem root. Mirrors TS `findGitRoot`.
fn nearest_git_root(start: &Path) -> Option<PathBuf> {
    let mut current = start.to_path_buf();
    loop {
        if std::fs::symlink_metadata(current.join(".git")).is_ok() {
            return Some(current);
        }
        match current.parent() {
            Some(parent) if parent != current => current = parent.to_path_buf(),
            _ => return None,
        }
    }
}

/// Resolve a git worktree's main repository root, if `git_root` holds a `.git`
/// *file* (the worktree marker `gitdir: <path>`). Returns the canonical main
/// repo working tree, or `None` when `.git` is a normal directory (not a
/// worktree). Approximates TS `findCanonicalGitRoot` for the worktree fallback.
fn canonical_git_root(git_root: &Path) -> Option<PathBuf> {
    let git_path = git_root.join(".git");
    let meta = std::fs::symlink_metadata(&git_path).ok()?;
    if !meta.file_type().is_file() {
        return None; // a real .git directory: not a worktree
    }
    let contents = std::fs::read_to_string(&git_path).ok()?;
    // Format: `gitdir: /path/to/main/.git/worktrees/<name>`
    let gitdir = contents
        .lines()
        .find_map(|l| l.strip_prefix("gitdir:"))
        .map(str::trim)?;
    let gitdir_path = Path::new(gitdir);
    // The main repo's working tree is the dir two levels above `worktrees/<name>`
    // relative to the main `.git`. i.e. `<main>/.git/worktrees/<name>` -> `<main>`.
    let worktrees_dir = gitdir_path.parent()?; // .git/worktrees
    let main_git_dir = worktrees_dir.parent()?; // .git
    if main_git_dir.file_name().map(|s| s == ".git") != Some(true) {
        return None;
    }
    main_git_dir.parent().map(Path::to_path_buf)
}

/// Load all custom markdown command files from the managed, user, and project
/// directories rooted at `cwd`. Faithful port of TS `loadMarkdownFilesForSubdir`
/// for `subdir == "commands"`.
///
/// * `cwd` — the session working directory (drives the project upward walk).
/// * `lingxi_home` — the user config dir (TS `getClaudeConfigHomeDir()`); the
///   user layer is `lingxi_home/commands`.
/// * `managed_dir` — the managed-policy root (TS `getManagedFilePath()`); the
///   managed layer is `managed_dir/.lingxi/commands`.
/// * `home` — the user's home directory, the upward-walk stop boundary.
///
/// Files are returned deduplicated by `(dev, ino)` with `managed > user >
/// project` priority and source order preserved within each layer.
#[must_use]
pub async fn load_command_markdown_files(
    cwd: &Path,
    lingxi_home: &Path,
    managed_dir: &Path,
    home: &Path,
) -> Vec<MarkdownCommandFile> {
    const SUBDIR: &str = "commands";

    let user_dir = lingxi_home.join(SUBDIR);
    let managed_commands_dir = managed_dir.join(branding::DOT_DIR).join(SUBDIR);
    let mut project_dirs = project_dirs_up_to_home(SUBDIR, cwd, home);

    // Worktree fallback: when cwd's nearest .git is a worktree marker whose main
    // repo differs, and the worktree lacks its own `.lingxi/<subdir>`, add the
    // main repo's copy (TS `loadMarkdownFilesForSubdir` lines 320-335).
    if let Some(git_root) = nearest_git_root(cwd) {
        if let Some(canonical_root) = canonical_git_root(&git_root) {
            if normalize_for_comparison(&canonical_root) != normalize_for_comparison(&git_root) {
                let worktree_subdir =
                    normalize_for_comparison(&git_root.join(branding::DOT_DIR).join(SUBDIR));
                let worktree_has_subdir = project_dirs
                    .iter()
                    .any(|d| normalize_for_comparison(d) == worktree_subdir);
                if !worktree_has_subdir {
                    let main_lingxi_subdir = canonical_root.join(branding::DOT_DIR).join(SUBDIR);
                    if !project_dirs.contains(&main_lingxi_subdir) {
                        project_dirs.push(main_lingxi_subdir);
                    }
                }
            }
        }
    }

    // Answers: which tier's markdown command wins a name clash.
    // One of several orderings over these rungs; `lingxi_core::types::scope`'s module docs index them all and say which question each answers.
    //
    // Load each layer. Order: managed, user, then project dirs (most- to
    // least-specific). Combined priority: managed > user > project.
    let mut all_files: Vec<MarkdownCommandFile> = Vec::new();
    all_files.extend(load_markdown_dir(
        &managed_commands_dir,
        CommandSource::Settings(lingxi_core::types::SettingsScope::Managed),
    ));
    all_files.extend(load_markdown_dir(
        &user_dir,
        CommandSource::Settings(lingxi_core::types::SettingsScope::User),
    ));
    for project_dir in &project_dirs {
        all_files.extend(load_markdown_dir(
            project_dir,
            CommandSource::Settings(lingxi_core::types::SettingsScope::Project),
        ));
    }

    deduplicate_by_inode(all_files)
}

/// Load only managed-policy markdown commands without touching user or project
/// customization roots. Used by `strictPluginOnlyCustomization:["skills"]`.
#[must_use]
pub async fn load_managed_command_markdown_files(managed_dir: &Path) -> Vec<MarkdownCommandFile> {
    deduplicate_by_inode(load_markdown_dir(
        &managed_dir.join(branding::DOT_DIR).join("commands"),
        CommandSource::Settings(lingxi_core::types::SettingsScope::Managed),
    ))
}

/// Load directory-format `.lingxi/skills/<name>/SKILL.md` files from user and
/// project skill directories.
#[must_use]
pub async fn load_skill_markdown_files(
    cwd: &Path,
    lingxi_home: &Path,
    home: &Path,
) -> Vec<SkillMarkdownCommandFile> {
    load_skill_markdown_files_with_roots(cwd, lingxi_home, None, home, &[]).await
}

/// Load directory-format `.lingxi/skills/<name>/SKILL.md` files from managed,
/// user, project, and additional skill directories.
///
/// The returned order is the command-resolution priority order:
/// managed > user > project > additional.
#[must_use]
pub async fn load_skill_markdown_files_with_roots(
    cwd: &Path,
    lingxi_home: &Path,
    managed_dir: Option<&Path>,
    home: &Path,
    additional_skill_dirs: &[PathBuf],
) -> Vec<SkillMarkdownCommandFile> {
    let mut all_files = Vec::new();
    let collator = native_skill_name_collator();
    if let Some(managed_dir) = managed_dir {
        all_files.extend(load_skill_dir(
            &managed_dir.join(branding::DOT_DIR).join("skills"),
            CommandSource::Settings(lingxi_core::types::SettingsScope::Managed),
            &collator,
        ));
    }
    all_files.extend(load_skill_dir(
        &lingxi_home.join("skills"),
        CommandSource::Settings(lingxi_core::types::SettingsScope::User),
        &collator,
    ));
    for project_dir in project_dirs_up_to_home("skills", cwd, home) {
        all_files.extend(load_skill_dir(
            &project_dir,
            CommandSource::Settings(lingxi_core::types::SettingsScope::Project),
            &collator,
        ));
    }
    for dir in additional_skill_dirs {
        all_files.extend(load_skill_dir(
            dir,
            CommandSource::Settings(lingxi_core::types::SettingsScope::Project),
            &collator,
        ));
    }
    deduplicate_skill_files_by_inode(all_files)
}

/// Load only managed-policy skills without probing ambient customization
/// directories.
#[must_use]
pub async fn load_managed_skill_markdown_files(
    managed_dir: &Path,
) -> Vec<SkillMarkdownCommandFile> {
    let collator = native_skill_name_collator();
    deduplicate_skill_files_by_inode(load_skill_dir(
        &managed_dir.join(branding::DOT_DIR).join("skills"),
        CommandSource::Settings(lingxi_core::types::SettingsScope::Managed),
        &collator,
    ))
}

fn load_skill_dir(
    dir: &Path,
    source: CommandSource,
    collator: &CollatorBorrowed<'static>,
) -> Vec<SkillMarkdownCommandFile> {
    let Ok(entries) = std::fs::read_dir(dir) else {
        return Vec::new();
    };
    let mut out = Vec::new();
    for entry in entries.flatten() {
        let skill_root = entry.path();
        if !std::fs::metadata(&skill_root).is_ok_and(|m| m.is_dir()) {
            continue;
        }
        let file_path = skill_root.join("SKILL.md");
        let Ok(raw) = std::fs::read_to_string(&file_path) else {
            continue;
        };
        let (frontmatter, frontmatter_name, content) = parse_skill_frontmatter(&raw);
        out.push(SkillMarkdownCommandFile {
            file_path,
            skill_root,
            frontmatter,
            frontmatter_name,
            content,
            content_length: raw.len(),
            source,
        });
    }
    out.sort_by(|left, right| {
        let left_name = left
            .skill_root
            .file_name()
            .map(|name| name.to_string_lossy());
        let right_name = right
            .skill_root
            .file_name()
            .map(|name| name.to_string_lossy());
        collator.compare(
            left_name.as_deref().unwrap_or(""),
            right_name.as_deref().unwrap_or(""),
        )
    });
    out
}

fn deduplicate_skill_files_by_inode(
    all_files: Vec<SkillMarkdownCommandFile>,
) -> Vec<SkillMarkdownCommandFile> {
    let mut seen: HashSet<(u64, u64)> = HashSet::new();
    let mut out = Vec::with_capacity(all_files.len());
    for file in all_files {
        match file_identity(&file.file_path) {
            Some(id) => {
                if !seen.insert(id) {
                    continue;
                }
                out.push(file);
            }
            None => out.push(file),
        }
    }
    out
}

/// Deduplicate files that resolve to the same physical file (same `(dev, ino)`),
/// keeping the first occurrence (highest priority). Files whose identity cannot
/// be determined are kept (fail open). Faithful port of TS dedup loop using
/// `getFileIdentity`.
fn deduplicate_by_inode(all_files: Vec<MarkdownCommandFile>) -> Vec<MarkdownCommandFile> {
    let mut seen: HashSet<(u64, u64)> = HashSet::new();
    let mut out = Vec::with_capacity(all_files.len());
    for file in all_files {
        match file_identity(&file.file_path) {
            Some(id) => {
                if !seen.insert(id) {
                    continue; // duplicate inode already loaded from higher priority
                }
                out.push(file);
            }
            // Cannot identify -> include (fail open).
            None => out.push(file),
        }
    }
    out
}

/// `(dev, ino)` identity of a file (following symlinks via `lstat`-equivalent).
///
/// TS uses `lstat` with bigint dev/ino and treats `dev==0 && ino==0` as
/// unidentifiable (network mounts). On non-unix targets identity is always
/// `None` (dedup disabled), matching the TS Windows fail-open note.
#[cfg(unix)]
fn file_identity(path: &Path) -> Option<(u64, u64)> {
    let meta = std::fs::symlink_metadata(path).ok()?;
    let dev = meta.dev();
    let ino = meta.ino();
    if dev == 0 && ino == 0 {
        return None;
    }
    Some((dev, ino))
}

#[cfg(not(unix))]
fn file_identity(_path: &Path) -> Option<(u64, u64)> {
    None
}

/// Recursively load every `*.md` file under `dir`, parsing frontmatter. A
/// missing/inaccessible directory yields an empty vector (TS `isFsInaccessible`
/// fail-open). Symlinks are followed; directory cycles are broken by a
/// `(dev, ino)` visited-set.
fn load_markdown_dir(dir: &Path, source: CommandSource) -> Vec<MarkdownCommandFile> {
    let mut files: Vec<PathBuf> = Vec::new();
    let mut visited: HashSet<(u64, u64)> = HashSet::new();
    walk_markdown_files(dir, &mut files, &mut visited);

    let mut out = Vec::with_capacity(files.len());
    for file_path in files {
        match std::fs::read_to_string(&file_path) {
            Ok(raw) => {
                let (frontmatter, frontmatter_name, content) = parse_skill_frontmatter(&raw);
                out.push(MarkdownCommandFile {
                    file_path,
                    base_dir: dir.to_path_buf(),
                    frontmatter,
                    frontmatter_name,
                    content,
                    source,
                });
            }
            // Read/parse failure: skip this file (TS returns null and filters).
            Err(_) => continue,
        }
    }
    out
}

/// Recursive `*.md` finder. Faithful to TS `findMarkdownFilesNative`: follows
/// symlinks, tracks visited directories by `(dev, ino)` to break cycles, does
/// not skip hidden entries, and does not consult `.gitignore`.
fn walk_markdown_files(
    current_dir: &Path,
    files: &mut Vec<PathBuf>,
    visited: &mut HashSet<(u64, u64)>,
) {
    // Cycle detection via the directory's identity (follows symlinks: `stat`).
    if let Some(id) = dir_identity(current_dir) {
        if !visited.insert(id) {
            return;
        }
    }

    let Ok(entries) = std::fs::read_dir(current_dir) else {
        return;
    };

    for entry in entries.flatten() {
        let full_path = entry.path();
        // `stat` follows symlinks (TS `stat(fullPath)` for the symlink branch and
        // the plain isFile/isDirectory checks below collapse to the same logic).
        let Ok(meta) = std::fs::metadata(&full_path) else {
            continue;
        };
        if meta.is_dir() {
            walk_markdown_files(&full_path, files, visited);
        } else if meta.is_file() && has_md_extension(&full_path) {
            files.push(full_path);
        }
    }
}

/// `true` when the file name ends with the literal `.md` suffix. TS uses
/// case-sensitive `name.endsWith('.md')`; this preserves that (a `.MD` file is
/// not matched), so the clippy case-insensitive suggestion is intentionally not
/// taken.
#[allow(clippy::case_sensitive_file_extension_comparisons)]
fn has_md_extension(path: &Path) -> bool {
    path.file_name()
        .and_then(|n| n.to_str())
        .is_some_and(|n| n.ends_with(".md"))
}

/// `(dev, ino)` of a directory (following symlinks) for cycle detection.
#[cfg(unix)]
fn dir_identity(path: &Path) -> Option<(u64, u64)> {
    let meta = std::fs::metadata(path).ok()?;
    if !meta.is_dir() {
        return None;
    }
    Some((meta.dev(), meta.ino()))
}

#[cfg(not(unix))]
fn dir_identity(path: &Path) -> Option<(u64, u64)> {
    // Fall back to canonical path hashing-free cycle detection: use realpath via
    // canonicalize. Encode as (hash, 0); collisions are acceptable for cycles.
    let real = std::fs::canonicalize(path).ok()?;
    use std::hash::{Hash, Hasher};
    let mut h = std::collections::hash_map::DefaultHasher::new();
    real.hash(&mut h);
    Some((h.finish(), 0))
}

/// Parse a single command markdown `raw` string (already read from
/// `file_path`, namespaced under `base_dir`) into a [`MarkdownCommandFile`].
///
/// This is the per-file primitive behind [`load_command_markdown_files`],
/// exposed so out-of-tree loaders (e.g. the plugin materialiser, which reads a
/// plugin's own `commands/` dir) can build a faithful command — body +
/// frontmatter — instead of stamping empty strings. Combine with
/// [`build_markdown_command`] to obtain the full [`SlashCommand`].
#[must_use]
pub fn parse_command_markdown(
    raw: &str,
    file_path: PathBuf,
    base_dir: PathBuf,
    source: CommandSource,
) -> MarkdownCommandFile {
    let (frontmatter, frontmatter_name, content) = parse_skill_frontmatter(raw);
    MarkdownCommandFile {
        file_path,
        base_dir,
        frontmatter,
        frontmatter_name,
        content,
        source,
    }
}

/// Native plugin prompt commands namespace a valid frontmatter display name
/// with the plugin prefix and expose a bare alias only when the source name has
/// no colon. The canonical command name is passed after the plugin loader has
/// applied its actual command-path name.
#[must_use]
pub fn plugin_prompt_name_metadata(
    canonical_name: &Utf16JsonProjection,
    frontmatter_name: Option<&Utf16JsonProjection>,
) -> (Utf16JsonProjection, Vec<Utf16JsonProjection>) {
    let Some(frontmatter_name) = frontmatter_name else {
        return (canonical_name.clone(), Vec::new());
    };
    let Some(display_units) = frontmatter_name.string_units("") else {
        return (canonical_name.clone(), Vec::new());
    };
    if display_units.is_empty() {
        return (canonical_name.clone(), Vec::new());
    }

    let canonical_units = canonical_name.string_units("").unwrap_or_default();
    let prefix_end = canonical_units
        .iter()
        .rposition(|unit| *unit == b':' as u16)
        .map_or(0, |index| index + 1);
    let prefix = &canonical_units[..prefix_end];
    let mut user_facing_units = if display_units.starts_with(prefix) {
        display_units.to_vec()
    } else {
        let mut namespaced = prefix.to_vec();
        namespaced.extend_from_slice(&display_units);
        namespaced
    };
    let user_facing_name = Utf16JsonProjection::root_string(
        String::from_utf16_lossy(&user_facing_units),
        std::mem::take(&mut user_facing_units),
    )
    .expect("Native plugin display name is a root string");
    let aliases = if display_units.contains(&(b':' as u16)) {
        Vec::new()
    } else {
        vec![frontmatter_name.clone()]
    };
    (user_facing_name, aliases)
}

/// Parse one directory-format skill markdown buffer that was discovered by an
/// external loader (for example an installed plugin).
///
/// Keeping this primitive beside [`parse_command_markdown`] ensures plugin
/// skills use the same frontmatter coercion, argument metadata, and body
/// splitting as project/user skills instead of maintaining a second parser.
#[must_use]
pub fn parse_skill_command_markdown(
    raw: &str,
    file_path: PathBuf,
    skill_root: PathBuf,
    source: CommandSource,
) -> SkillMarkdownCommandFile {
    let (frontmatter, frontmatter_name, content) = parse_skill_frontmatter(raw);
    SkillMarkdownCommandFile {
        file_path,
        skill_root,
        frontmatter,
        frontmatter_name,
        content,
        content_length: raw.len(),
        source,
    }
}

/// Split a raw markdown string. Mirrors the
/// `skill::frontmatter::parse_skill_markdown` `---`/`\n---\n` splitter (the
/// shape the spec asks to reuse without depending on `skill`). Closed
/// frontmatter keeps its body even when both metadata parse attempts fail;
/// absent or unterminated frontmatter yields defaults and the raw input.
#[cfg(test)]
fn parse_frontmatter(raw: &str) -> (CommandFrontmatter, String) {
    let (frontmatter, _, content) = parse_skill_frontmatter(raw);
    (frontmatter, content)
}

/// Parse the common markdown frontmatter while retaining the `name` value used
/// by Native's prompt-command factory as its separate `userFacingName` result.
fn parse_skill_frontmatter(raw: &str) -> (CommandFrontmatter, Option<Utf16JsonProjection>, String) {
    let raw = raw.strip_prefix(UTF8_BOM).unwrap_or(raw);
    if let Some(rest) = raw.strip_prefix("---") {
        if let Some(end) = rest.find("\n---\n") {
            let yaml = &rest[..end];
            let body = &rest[end + 5..];
            let (frontmatter, frontmatter_name) = parse_frontmatter_yaml(yaml)
                .map(|mut raw_frontmatter| {
                    let frontmatter_name = raw_frontmatter
                        .name
                        .take()
                        .and_then(native_frontmatter_name)
                        .filter(|name| name.encode_utf16().count() <= 256)
                        .map(Into::into);
                    (build_frontmatter(raw_frontmatter), frontmatter_name)
                })
                .unwrap_or_default();
            return (frontmatter, frontmatter_name, body.trim_start().to_string());
        }
    }
    (CommandFrontmatter::default(), None, raw.to_string())
}

/// JS `String(value)` coercion used by Native `X8t(frontmatter.name, …)`.
fn native_frontmatter_name(value: serde_yaml::Value) -> Option<String> {
    use serde_yaml::Value;

    match value {
        Value::Null => None,
        Value::Bool(value) => Some(value.to_string()),
        Value::Number(value) => Some(value.to_string()),
        Value::String(value) => Some(value),
        Value::Sequence(values) => Some(
            values
                .into_iter()
                .map(|value| native_array_element_string(value).unwrap_or_default())
                .collect::<Vec<_>>()
                .join(","),
        ),
        Value::Mapping(_) => Some("[object Object]".to_string()),
        Value::Tagged(value) => native_frontmatter_name(value.value),
    }
}

fn native_array_element_string(value: serde_yaml::Value) -> Option<String> {
    match value {
        serde_yaml::Value::Null => None,
        value => native_frontmatter_name(value),
    }
}

fn native_effective_user_facing_name(
    canonical_name: &Utf16JsonProjection,
    frontmatter_name: Option<&Utf16JsonProjection>,
) -> Utf16JsonProjection {
    frontmatter_name
        .filter(|name| name.string_units("").is_some_and(|units| !units.is_empty()))
        .cloned()
        .unwrap_or_else(|| canonical_name.clone())
}

/// Parse Native's frontmatter YAML with its one repair retry. Native 2.1.295
/// first parses the original block; only on failure does it quote selected
/// YAML-sensitive plain scalars, expand leading tabs, and retry. Invalid
/// quoted values (including an escaped lone surrogate) remain invalid, so a
/// failed retry returns no metadata to [`parse_frontmatter`].
fn parse_frontmatter_yaml(yaml: &str) -> Option<RawFrontmatter> {
    if let Ok(value) = serde_yaml::from_str::<serde_yaml::Value>(yaml) {
        return serde_yaml::from_value(value).ok();
    }

    let repaired = quote_native_yaml_sensitive_scalars(yaml);
    let repaired = expand_native_yaml_leading_tabs(&repaired);
    let value = serde_yaml::from_str::<serde_yaml::Value>(&repaired).ok()?;
    serde_yaml::from_value(value).ok()
}

/// Native 2.1.295's `I` helper. It quotes unquoted top-level-looking lines
/// whose values contain YAML-sensitive punctuation, except values already
/// quoted and complete bracketed arrays. Keeping this retry general is
/// important: the malformed `allowed-tools` capture is repaired to a scalar,
/// while a quoted invalid Unicode escape is deliberately left for YAML to
/// reject.
fn quote_native_yaml_sensitive_scalars(yaml: &str) -> String {
    let mut output = String::with_capacity(yaml.len());
    for (index, line) in yaml.split('\n').enumerate() {
        if index != 0 {
            output.push('\n');
        }

        let Some((key, value)) = native_yaml_key_value_line(line) else {
            output.push_str(line);
            continue;
        };
        let already_quoted = (value.starts_with('"') && value.ends_with('"'))
            || (value.starts_with('\'') && value.ends_with('\''));
        if already_quoted {
            output.push_str(line);
            continue;
        }
        if value.starts_with('[')
            && value.ends_with(']')
            && serde_yaml::from_str::<serde_yaml::Value>(value)
                .is_ok_and(|parsed| matches!(parsed, serde_yaml::Value::Sequence(_)))
        {
            output.push_str(line);
            continue;
        }

        let yaml_sensitive = value.chars().any(|character| {
            matches!(
                character,
                '{' | '}' | '[' | ']' | '*' | '&' | '#' | '!' | '|' | '>' | '%' | '@' | '`'
            )
        }) || value.contains(": ");
        if !yaml_sensitive {
            output.push_str(line);
            continue;
        }

        let escaped = value.replace('\\', "\\\\").replace('"', "\\\"");
        output.push_str(key);
        output.push_str(": \"");
        output.push_str(&escaped);
        output.push('"');
    }
    output
}

/// Match Native's `^([a-zA-Z_-]+):\\s+(\\S.*)$` line prefix without adding a
/// regex dependency to this platform-agnostic crate.
fn native_yaml_key_value_line(line: &str) -> Option<(&str, &str)> {
    let colon = line.find(':')?;
    let key = &line[..colon];
    if key.is_empty()
        || !key
            .bytes()
            .all(|byte| byte.is_ascii_alphabetic() || matches!(byte, b'_' | b'-'))
    {
        return None;
    }

    let after_colon = &line[colon + 1..];
    let (value_offset, _) = after_colon
        .char_indices()
        .find(|(_, character)| !is_native_yaml_regex_whitespace(*character))?;
    if value_offset == 0 {
        return None;
    }
    Some((key, &after_colon[value_offset..]))
}

/// ECMAScript regular-expression `\s` set used by Native's repair matcher.
fn is_native_yaml_regex_whitespace(character: char) -> bool {
    matches!(
        character,
        '\u{0009}'..='\u{000d}'
            | '\u{0020}'
            | '\u{00a0}'
            | '\u{1680}'
            | '\u{2000}'..='\u{200a}'
            | '\u{2028}'
            | '\u{2029}'
            | '\u{202f}'
            | '\u{205f}'
            | '\u{3000}'
            | '\u{feff}'
    )
}

/// Native's `^\\t+` multiline replacement: each indentation tab becomes two
/// spaces before its YAML retry.
fn expand_native_yaml_leading_tabs(yaml: &str) -> String {
    let mut output = String::with_capacity(yaml.len());
    for (index, line) in yaml.split('\n').enumerate() {
        if index != 0 {
            output.push('\n');
        }
        let leading_tabs = line.bytes().take_while(|byte| *byte == b'\t').count();
        for _ in 0..leading_tabs {
            output.push_str("  ");
        }
        output.push_str(&line[leading_tabs..]);
    }
    output
}

/// Map the raw kebab-case YAML frontmatter onto the typed [`CommandFrontmatter`].
fn build_frontmatter(raw: RawFrontmatter) -> CommandFrontmatter {
    // SLASH.5: TS stores `allowed-tools` via
    // `parseSlashCommandToolsFromFrontmatter`, which flattens the value to a
    // `string[]` then runs it through `parseToolListFromCLI` (paren-aware
    // comma/space split, trimmed) instead of keeping it verbatim. Both a single
    // string and a YAML list go through the same splitter.
    let allowed_tools = match raw.allowed_tools {
        None => None,
        Some(ToolsField::One(s)) => Some(parse_slash_command_tools_from_frontmatter(&[s])),
        Some(ToolsField::Many(v)) => Some(parse_slash_command_tools_from_frontmatter(&v)),
    };
    let disallowed_tools = match raw.disallowed_tools {
        None => None,
        Some(ToolsField::One(s)) => Some(parse_slash_command_tools_from_frontmatter(&[s])),
        Some(ToolsField::Many(v)) => Some(parse_slash_command_tools_from_frontmatter(&v)),
    };
    let session_modes = match raw.session_modes {
        None => None,
        Some(ToolsField::One(s)) => Some(parse_session_modes_from_frontmatter(&[s])),
        Some(ToolsField::Many(v)) => Some(parse_session_modes_from_frontmatter(&v)),
    };
    let argument_hints = raw.argument_hint.map(|h| vec![h]).unwrap_or_default();
    let argument_names = match raw.arguments {
        None => Vec::new(),
        Some(ArgumentsField::One(s)) => parse_argument_names(Some(&FrontmatterArgs::Str(s))),
        Some(ArgumentsField::Many(v)) => parse_argument_names(Some(&FrontmatterArgs::List(v))),
    };
    let shell = raw.shell.and_then(|s| match s.as_str() {
        "bash" => Some(crate::model::FrontmatterShell::Bash),
        "powershell" => Some(crate::model::FrontmatterShell::PowerShell),
        _ => None,
    });
    // SLASH.1 (updated for cc 2.1.218): frontmatter booleans coerce via `Kde`
    // — `yes`/`no`/`on`/`off`/`1`/`0` (case-insensitive, trimmed) join
    // `true`/`false`. `disable-model-invocation` uses `rtr` = `Kde(v) ?? false`
    // (a garbage value that coerces to neither set falls back to false).
    let disable_model_invocation = raw
        .disable_model_invocation
        .as_ref()
        .and_then(Boolish::coerce)
        .unwrap_or(false);
    // `background` uses bare `Kde` (claude `background:Kde(…)`): a value that
    // coerces to neither set is not a declaration at all and leaves the
    // default (background) in force — NOT `false`, which would silently
    // un-background a forking skill over a typo.
    let background = raw.background.as_ref().and_then(Boolish::coerce);
    // `en = dt === void 0 ? !0 : htt(dt)` where `htt(e) = c1(e) ?? !1`. So an
    // ABSENT key is undeclared (`None`, read as invocable at the use site),
    // while a key that is present but coerces to neither set is `false` — the
    // author said something, and upstream reads anything unparseable as "hide
    // it". This is `rtr`, not the bare `Kde` that `background` uses.
    let user_invocable = raw
        .user_invocable
        .as_ref()
        .map(|value| Boolish::coerce(value).unwrap_or(false));
    CommandFrontmatter {
        disallowed_tools,
        context: raw.context,
        background,
        agent: raw.agent,
        description: raw.description.unwrap_or_default(),
        allowed_tools,
        model: raw.model,
        session_modes,
        argument_hints,
        argument_names,
        thinking: None,
        shell,
        disable_model_invocation,
        // SLASH.4: TS copies `frontmatter.when_to_use` verbatim.
        when_to_use: raw.when_to_use,
        // Carried so the model-facing listing can withhold a conditional skill
        // until one of these matches; the command record is what that listing is
        // built from, so the field has to reach it or the filter has no key.
        paths: raw.paths.filter(|p| !p.is_empty()),
        user_invocable,
        effort: raw.effort.map(EffortField::into_raw),
    }
}

/// Port of TS `parseSlashCommandToolsFromFrontmatter` (slice that applies once
/// the value is already a `string[]`): run the tools through
/// [`parse_tool_list_from_cli`], then collapse to `["*"]` if any parsed entry is
/// the `*` wildcard (TS `parseToolListString` line `if (parsedTools.includes('*'))
/// return ['*']`).
fn parse_slash_command_tools_from_frontmatter(tools: &[String]) -> Vec<String> {
    let parsed = parse_tool_list_from_cli(tools);
    if parsed.iter().any(|t| t == "*") {
        vec!["*".to_string()]
    } else {
        parsed
    }
}

fn parse_session_modes_from_frontmatter(values: &[String]) -> Vec<String> {
    values
        .iter()
        .flat_map(|value| value.split([',', ' ']))
        .map(str::trim)
        .filter(|value| !value.is_empty())
        .map(|value| value.to_ascii_lowercase())
        .collect()
}

/// Faithful port of TS `parseToolListFromCLI` (`permissionSetup.ts`): split each
/// string on top-level commas and spaces, trimming each tool, while keeping any
/// separators that appear inside parentheses (e.g. `Bash(git log, foo)` stays one
/// entry). Empty strings are skipped and empty/whitespace-only tools are dropped.
fn parse_tool_list_from_cli(tools: &[String]) -> Vec<String> {
    if tools.is_empty() {
        return Vec::new();
    }

    let mut result: Vec<String> = Vec::new();

    for tool_string in tools {
        if tool_string.is_empty() {
            continue;
        }

        let mut current = String::new();
        let mut is_in_parens = false;

        for ch in tool_string.chars() {
            match ch {
                '(' => {
                    is_in_parens = true;
                    current.push(ch);
                }
                ')' => {
                    is_in_parens = false;
                    current.push(ch);
                }
                ',' => {
                    if is_in_parens {
                        current.push(ch);
                    } else {
                        // Comma separator — push current tool and start a new one.
                        if !current.trim().is_empty() {
                            result.push(current.trim().to_string());
                        }
                        current.clear();
                    }
                }
                ' ' => {
                    if is_in_parens {
                        current.push(ch);
                    } else if !current.trim().is_empty() {
                        // Space separator — push current tool and start a new one.
                        result.push(current.trim().to_string());
                        current.clear();
                    }
                }
                _ => current.push(ch),
            }
        }

        // Push any remaining tool.
        if !current.trim().is_empty() {
            result.push(current.trim().to_string());
        }
    }

    result
}

/// Build a [`SlashCommand`] of kind [`SlashCommandKind::Markdown`] from a loaded
/// file. The `name` is computed from the file path relative to its base dir
/// (namespaced); the description is the frontmatter `description` when set, else
/// [`extract_description_from_markdown`] over the body (TS `Custom command`
/// fallback).
#[must_use]
pub fn build_markdown_command(file: &MarkdownCommandFile, source: CommandSource) -> SlashCommand {
    let name = command_name_from_path(&file.file_path, &file.base_dir);
    let name_source: Utf16JsonProjection = name.clone().into();
    let user_facing_name =
        native_effective_user_facing_name(&name_source, file.frontmatter_name.as_ref());
    // Mirrors TS `hasUserSpecifiedDescription`: true when the frontmatter
    // carried an explicit `description`, false when we auto-derive it from the
    // body (`Custom command` fallback).
    let has_user_specified_description = !file.frontmatter.description.is_empty();
    let description = if file.frontmatter.description.is_empty() {
        extract_description_from_markdown(&file.content, "Custom command")
    } else {
        file.frontmatter.description.clone()
    };
    SlashCommand {
        name: name.into(),
        description: description.into(),
        source,
        native_source: Some(native_skill_listing_source(&source)),
        kind: SlashCommandKind::Markdown {
            file_path: file.file_path.clone(),
            frontmatter: file.frontmatter.clone(),
            prompt_template: file.content.clone().into(),
        },
        // Markdown-backed custom commands are Native prompt commands. Some
        // hosts replace Markdown with a bundled executor after construction,
        // so set this source classification before that conversion.
        native_type: Some(NativeCommandType::Prompt),
        has_user_specified_description,
        // SLASH.2: TS `createSkillCommand` copies the frontmatter `argument-hint`
        // onto the top-level command (`argumentHint`). The frontmatter parser
        // stores it as a one-element `argument_hints` vec (empty when the key is
        // absent), so its first element is the hint — mirroring TS
        // `frontmatter['argument-hint'] != null ? String(...) : undefined`.
        argument_hint: file
            .frontmatter
            .argument_hints
            .first()
            .cloned()
            .map(Into::into),
        // ARGS.3: TS `createSkillCommand` copies the parsed `argNames`
        // (`parseArgumentNames(frontmatter.arguments)`) onto the top-level
        // command. The frontmatter parser already stores that filtered list in
        // `argument_names`, so carry it through verbatim. This is what drives the
        // in-TUI progressive argument-hint (`generateProgressiveArgumentHint`).
        argument_names: file
            .frontmatter
            .argument_names
            .iter()
            .cloned()
            .map(Into::into)
            .collect(),
        // SLASH.3: legacy `.lingxi/commands/**.md` files load through TS
        // `loadSkillsFromCommandsDir`, which tags every command it builds with
        // `loadedFrom: 'commands_DEPRECATED'`.
        loaded_from: Some("commands_DEPRECATED".to_string()),
        // SLASH.1/SLASH.4: TS `createSkillCommand` copies `disable-model-invocation`
        // and `when_to_use` from the parsed frontmatter onto the top-level command.
        disable_model_invocation: file.frontmatter.disable_model_invocation,
        when_to_use: file.frontmatter.when_to_use.clone(),
        paths: file.frontmatter.paths.clone(),
        user_facing_name: Some(user_facing_name),
        ..SlashCommand::default()
    }
}

/// Build a [`SlashCommand`] from a directory-format skill file.
#[must_use]
pub fn build_skill_command(file: &SkillMarkdownCommandFile, source: CommandSource) -> SlashCommand {
    let name = file
        .skill_root
        .file_name()
        .map_or_else(String::new, |s| s.to_string_lossy().into_owned());
    let name_source: Utf16JsonProjection = name.clone().into();
    let user_facing_name =
        native_effective_user_facing_name(&name_source, file.frontmatter_name.as_ref());
    let has_user_specified_description = !file.frontmatter.description.is_empty();
    let description = if file.frontmatter.description.is_empty() {
        extract_description_from_markdown(&file.content, "Custom command")
    } else {
        file.frontmatter.description.clone()
    };
    SlashCommand {
        name: name.into(),
        description: description.into(),
        source,
        native_source: Some(native_skill_listing_source(&source)),
        kind: SlashCommandKind::Markdown {
            file_path: file.file_path.clone(),
            frontmatter: file.frontmatter.clone(),
            prompt_template: file.content.clone().into(),
        },
        // Directory-format skills are Native prompt commands regardless of
        // the dispatch kind chosen by the host. Mobile/visualization hosts
        // replace Markdown with a bundled executor after construction.
        native_type: Some(NativeCommandType::Prompt),
        has_user_specified_description,
        argument_hint: file
            .frontmatter
            .argument_hints
            .first()
            .cloned()
            .map(Into::into),
        argument_names: file
            .frontmatter
            .argument_names
            .iter()
            .cloned()
            .map(Into::into)
            .collect(),
        loaded_from: Some("skills".to_string()),
        disable_model_invocation: file.frontmatter.disable_model_invocation,
        when_to_use: file.frontmatter.when_to_use.clone(),
        paths: file.frontmatter.paths.clone(),
        skill_root: Some(file.skill_root.clone()),
        // `userInvocable: en` — was hardcoded `true`, which made an on-disk
        // `user-invocable: false` a no-op and left the skill in the `/` menu.
        user_invocable: Some(file.frontmatter.user_invocable.unwrap_or(true)),
        content_length: Some(file.content_length),
        user_facing_name: Some(user_facing_name),
        ..SlashCommand::default()
    }
}

/// Project this command factory's authoritative source into Native m$o's
/// `skillFrontmatter.source` value. Native collapses in-binary `builtin` and
/// `bundled` prompt sources to `built-in`; settings tiers keep their exact
/// source names.
fn native_skill_listing_source(source: &CommandSource) -> Utf16JsonProjection {
    let source = match source {
        CommandSource::Builtin | CommandSource::Bundled => "built-in",
        CommandSource::Settings(lingxi_core::types::SettingsScope::User) => "userSettings",
        CommandSource::Settings(lingxi_core::types::SettingsScope::Project) => "projectSettings",
        CommandSource::Settings(lingxi_core::types::SettingsScope::Local) => "localSettings",
        CommandSource::Settings(lingxi_core::types::SettingsScope::Managed) => "policySettings",
        CommandSource::Plugin => "plugin",
        CommandSource::Mcp => "mcp",
    };
    source.into()
}

/// Normalize a path for case/separator-insensitive comparison. Approximates TS
/// `normalizePathForComparison`: resolves the path lexically and lowercases it
/// on case-insensitive platforms. On unix we keep case (case-sensitive FS) but
/// strip a trailing separator for stable comparisons.
fn normalize_for_comparison(p: &Path) -> String {
    let s = p.to_string_lossy();
    let trimmed = s.strip_suffix('/').unwrap_or(&s);
    if cfg!(windows) {
        trimmed.to_lowercase()
    } else {
        trimmed.to_string()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs;

    #[test]
    fn native_skill_listing_source_uses_exact_context_tags() {
        use lingxi_core::types::SettingsScope;

        for (source, expected) in [
            (CommandSource::Builtin, "built-in"),
            (CommandSource::Bundled, "built-in"),
            (CommandSource::Settings(SettingsScope::User), "userSettings"),
            (
                CommandSource::Settings(SettingsScope::Project),
                "projectSettings",
            ),
            (
                CommandSource::Settings(SettingsScope::Local),
                "localSettings",
            ),
            (
                CommandSource::Settings(SettingsScope::Managed),
                "policySettings",
            ),
            (CommandSource::Plugin, "plugin"),
            (CommandSource::Mcp, "mcp"),
        ] {
            assert_eq!(
                native_skill_listing_source(&source).string_at(""),
                Some(expected),
                "{source:?}"
            );
        }
    }

    // ---------- extract_description_from_markdown ----------

    #[test]
    fn extract_description_strips_header_prefix() {
        assert_eq!(
            extract_description_from_markdown("# My Command\n\nbody", "Custom command"),
            "My Command"
        );
        assert_eq!(
            extract_description_from_markdown("### Deep Header   \nmore", "Custom command"),
            "Deep Header"
        );
    }

    #[test]
    fn extract_description_uses_first_non_empty_line() {
        assert_eq!(
            extract_description_from_markdown("\n\n   \nplain text line\nnext", "Custom command"),
            "plain text line"
        );
    }

    #[test]
    fn extract_description_falls_back_to_default() {
        assert_eq!(
            extract_description_from_markdown("\n  \n\t\n", "Custom command"),
            "Custom command"
        );
    }

    #[test]
    fn extract_description_truncates_at_100() {
        // 120 'a' chars, no header -> first 97 + "..." (length 100).
        let long = "a".repeat(120);
        let out = extract_description_from_markdown(&long, "Custom command");
        assert_eq!(out.chars().count(), 100);
        assert!(out.ends_with("..."));
        assert_eq!(&out[..97], &"a".repeat(97));
    }

    #[test]
    fn extract_description_exactly_100_not_truncated() {
        let exactly = "a".repeat(100);
        let out = extract_description_from_markdown(&exactly, "Custom command");
        assert_eq!(out, exactly);
        assert!(!out.ends_with("..."));
    }

    #[test]
    fn extract_description_hash_without_space_is_not_header() {
        // `#no-space` does not match `^#+\s+(.+)$`; the whole trimmed line is used.
        assert_eq!(
            extract_description_from_markdown("#nospace text", "Custom command"),
            "#nospace text"
        );
    }

    // ---------- command_name_from_path ----------

    #[test]
    fn command_name_top_level() {
        let base = Path::new("/root/.lingxi/commands");
        let file = base.join("foo.md");
        assert_eq!(command_name_from_path(&file, base), "foo");
    }

    #[test]
    fn command_name_namespaced() {
        let base = Path::new("/root/.lingxi/commands");
        let file = base.join("sub").join("bar.md");
        assert_eq!(command_name_from_path(&file, base), "sub:bar");
    }

    #[test]
    fn command_name_deep_namespace() {
        let base = Path::new("/root/.lingxi/commands");
        let file = base.join("a").join("b").join("c.md");
        assert_eq!(command_name_from_path(&file, base), "a:b:c");
    }

    // ---------- build_frontmatter ----------

    #[test]
    fn frontmatter_allowed_tools_model_argument_hint_parse() {
        let raw = "---\nallowed-tools:\n  - Bash\n  - Edit\nmodel: opus\nargument-hint: <file>\narguments: first second\n---\nbody here\n";
        let (fm, body) = parse_frontmatter(raw);
        assert_eq!(
            fm.allowed_tools,
            Some(vec!["Bash".to_string(), "Edit".to_string()])
        );
        assert_eq!(fm.model.as_deref(), Some("opus"));
        assert_eq!(fm.argument_hints, vec!["<file>".to_string()]);
        assert_eq!(
            fm.argument_names,
            vec!["first".to_string(), "second".to_string()]
        );
        // Body is `trim_start`-ed only (mirrors skill's splitter), so the
        // trailing newline from the file is preserved.
        assert_eq!(body, "body here\n");
    }

    #[test]
    fn frontmatter_allowed_tools_single_string() {
        let raw = "---\nallowed-tools: Bash\n---\nx";
        let (fm, _) = parse_frontmatter(raw);
        assert_eq!(fm.allowed_tools, Some(vec!["Bash".to_string()]));
    }

    // ---------- SLASH.5: allowed-tools is split via parseToolListFromCLI ----------

    #[test]
    fn frontmatter_allowed_tools_splits_comma_and_space_string() {
        // A single comma/space separated string must split into individual tools
        // (TS `parseToolListFromCLI`), each trimmed — not kept verbatim.
        let (fm, _) = parse_frontmatter("---\nallowed-tools: Bash, Edit  Read\n---\nx");
        assert_eq!(
            fm.allowed_tools,
            Some(vec![
                "Bash".to_string(),
                "Edit".to_string(),
                "Read".to_string()
            ])
        );
    }

    #[test]
    fn frontmatter_allowed_tools_keeps_commas_inside_parens() {
        // Commas/spaces inside `(...)` are part of the tool spec, not separators.
        let (fm, _) = parse_frontmatter("---\nallowed-tools: Bash(git log, foo), Read\n---\nx");
        assert_eq!(
            fm.allowed_tools,
            Some(vec!["Bash(git log, foo)".to_string(), "Read".to_string()])
        );
    }

    #[test]
    fn frontmatter_allowed_tools_list_entries_are_each_split() {
        // Each YAML list entry is itself run through the splitter.
        let (fm, _) = parse_frontmatter("---\nallowed-tools:\n  - Bash, Edit\n  - Read\n---\nx");
        assert_eq!(
            fm.allowed_tools,
            Some(vec![
                "Bash".to_string(),
                "Edit".to_string(),
                "Read".to_string()
            ])
        );
    }

    #[test]
    fn frontmatter_allowed_tools_wildcard_collapses() {
        // Any `*` among the parsed tools collapses the whole list to `["*"]`
        // (TS `parseToolListString`).
        let (fm, _) = parse_frontmatter("---\nallowed-tools: Bash, *\n---\nx");
        assert_eq!(fm.allowed_tools, Some(vec!["*".to_string()]));
    }

    #[test]
    fn frontmatter_session_modes_parse_from_list_and_string() {
        let (fm, _) = parse_frontmatter("---\nsession-modes:\n  - chat\n  - code\n---\nx");
        assert_eq!(
            fm.session_modes,
            Some(vec!["chat".to_string(), "code".to_string()])
        );

        let (fm, _) = parse_frontmatter("---\nsession-modes: chat, code\n---\nx");
        assert_eq!(
            fm.session_modes,
            Some(vec!["chat".to_string(), "code".to_string()])
        );
    }

    #[test]
    fn parse_tool_list_from_cli_matches_ts_examples() {
        assert_eq!(
            parse_tool_list_from_cli(&["A, B C".to_string()]),
            vec!["A".to_string(), "B".to_string(), "C".to_string()]
        );
        // Leading/trailing whitespace and empty segments are dropped.
        assert_eq!(
            parse_tool_list_from_cli(&["  Foo ,, , Bar  ".to_string()]),
            vec!["Foo".to_string(), "Bar".to_string()]
        );
        // Empty strings in the array are skipped entirely.
        assert_eq!(
            parse_tool_list_from_cli(&[String::new(), "X".to_string()]),
            vec!["X".to_string()]
        );
        assert!(parse_tool_list_from_cli(&[]).is_empty());
    }

    #[test]
    fn frontmatter_absent_yields_defaults() {
        let (fm, body) = parse_frontmatter("just a body\nwith lines");
        assert_eq!(fm.allowed_tools, None);
        assert_eq!(fm.model, None);
        assert!(fm.argument_hints.is_empty());
        assert!(fm.argument_names.is_empty());
        assert_eq!(body, "just a body\nwith lines");
    }

    #[test]
    fn skill_prompt_display_name_uses_frontmatter_name_then_native_fallback() {
        let explicit = parse_skill_command_markdown(
            "---\nname: Friendly\n---\nbody",
            PathBuf::from("/skills/folder/SKILL.md"),
            PathBuf::from("/skills/folder"),
            CommandSource::Settings(lingxi_core::types::SettingsScope::Project),
        );
        let explicit = build_skill_command(
            &explicit,
            CommandSource::Settings(lingxi_core::types::SettingsScope::Project),
        );
        assert_eq!(explicit.name.string_at(""), Some("folder"));
        assert_eq!(
            explicit
                .user_facing_name
                .as_ref()
                .and_then(|n| n.string_at("")),
            Some("Friendly")
        );

        let missing = parse_skill_command_markdown(
            "---\ndescription: no name\n---\nbody",
            PathBuf::from("/skills/folder/SKILL.md"),
            PathBuf::from("/skills/folder"),
            CommandSource::Settings(lingxi_core::types::SettingsScope::Project),
        );
        let missing = build_skill_command(
            &missing,
            CommandSource::Settings(lingxi_core::types::SettingsScope::Project),
        );
        assert_eq!(
            missing
                .user_facing_name
                .as_ref()
                .and_then(|n| n.string_at("")),
            Some("folder")
        );

        let too_long_name = "a".repeat(257);
        let too_long = parse_skill_command_markdown(
            &format!("---\nname: {too_long_name}\n---\nbody"),
            PathBuf::from("/skills/folder/SKILL.md"),
            PathBuf::from("/skills/folder"),
            CommandSource::Settings(lingxi_core::types::SettingsScope::Project),
        );
        assert!(too_long.frontmatter_name.is_none());
    }

    #[test]
    fn plugin_prompt_display_name_is_namespaced_with_bare_alias_only_when_unqualified() {
        let canonical: Utf16JsonProjection = "acme:folder".into();
        let bare: Utf16JsonProjection = "Friendly".into();
        let (display, aliases) = plugin_prompt_name_metadata(&canonical, Some(&bare));
        assert_eq!(display.string_at(""), Some("acme:Friendly"));
        assert_eq!(aliases.len(), 1);
        assert_eq!(aliases[0].string_at(""), Some("Friendly"));

        let qualified: Utf16JsonProjection = "other:Friendly".into();
        let (display, aliases) = plugin_prompt_name_metadata(&canonical, Some(&qualified));
        assert_eq!(display.string_at(""), Some("acme:other:Friendly"));
        assert!(aliases.is_empty());
    }

    #[test]
    fn invalid_native_unicode_escape_defaults_metadata_but_keeps_closed_body() {
        let raw = include_str!(
            "../../../docs/parity/current-alignment/fixtures/commands/native-baseline/2.1.295/skill-description-width/lone-surrogate-escape/SKILL.md.raw"
        );
        let (frontmatter, body) = parse_frontmatter(raw);

        assert_eq!(frontmatter, CommandFrontmatter::default());
        assert_eq!(body.trim_end(), "Return the fixed skill-width response.");
    }

    #[test]
    fn invalid_native_unicode_escape_discards_metadata_in_either_field_order() {
        for raw in [
            r#"---
description: "\uD800"
disable-model-invocation: true
---
body after the closed fence
"#,
            r#"---
disable-model-invocation: true
description: "\uD800"
---
body after the closed fence
"#,
        ] {
            let (frontmatter, body) = parse_frontmatter(raw);
            assert_eq!(frontmatter, CommandFrontmatter::default(), "raw: {raw}");
            assert_eq!(body, "body after the closed fence\n", "raw: {raw}");
        }
    }

    #[test]
    fn native_retry_quotes_malformed_allowed_tools_and_preserves_other_fields() {
        // Native 2.1.295's accepted capture retains this valid description
        // despite the open flow sequence. The selected-scalar retry turns the
        // malformed value into a string rather than dropping the whole block.
        let raw = r#"---
description: "frontmatter-description-must-not-be-listed"
allowed-tools: [Bash, Read
---
Return the fixed ordinary-malformed-frontmatter body fallback.
"#;
        let (frontmatter, body) = parse_frontmatter(raw);

        assert_eq!(
            frontmatter.description,
            "frontmatter-description-must-not-be-listed"
        );
        assert_eq!(
            frontmatter.allowed_tools,
            Some(vec!["[Bash".to_string(), "Read".to_string()])
        );
        assert_eq!(
            body,
            "Return the fixed ordinary-malformed-frontmatter body fallback.\n"
        );

        let parsed = parse_frontmatter_yaml("description: \"d\"\nallowed-tools: [Bash, Read")
            .expect("Native's retry should retain this block");
        assert_eq!(parsed.description.as_deref(), Some("d"));
        assert!(matches!(
            parsed.allowed_tools,
            Some(ToolsField::One(value)) if value == "[Bash, Read"
        ));
    }

    #[test]
    fn valid_yaml_with_invalid_typed_field_does_not_trigger_native_retry() {
        // Native retries only when generic YAML parsing fails. This mapping is
        // valid YAML, so it must not be reinterpreted as a scalar merely to fit
        // the Rust frontmatter carrier's String field.
        let (frontmatter, body) =
            parse_frontmatter("---\ndescription: { nested: value }\n---\nbody\n");

        assert_eq!(frontmatter, CommandFrontmatter::default());
        assert_eq!(body, "body\n");
    }

    #[test]
    fn valid_control_frontmatter_preserves_description_and_disable_flag() {
        let (frontmatter, body) = parse_frontmatter(
            "---\ndescription: control description\ndisable-model-invocation: true\n---\nbody\n",
        );

        assert_eq!(frontmatter.description, "control description");
        assert!(frontmatter.disable_model_invocation);
        assert_eq!(body, "body\n");
    }

    #[test]
    fn unterminated_frontmatter_remains_raw_input() {
        let raw = "---\ndescription: unclosed\nbody without a closing fence";
        let (frontmatter, body) = parse_frontmatter(raw);

        assert_eq!(frontmatter, CommandFrontmatter::default());
        assert_eq!(body, raw);
    }

    #[test]
    fn frontmatter_shell_parses() {
        let (fm, _) = parse_frontmatter("---\nshell: powershell\n---\nx");
        assert_eq!(fm.shell, Some(crate::model::FrontmatterShell::PowerShell));
    }

    #[test]
    fn frontmatter_with_utf8_bom_parses() {
        let raw = "\u{feff}---\ndescription: demo\nallowed-tools: Bash\n---\nbody";
        let (fm, body) = parse_frontmatter(raw);
        assert_eq!(fm.description, "demo");
        assert_eq!(fm.allowed_tools, Some(vec!["Bash".to_string()]));
        assert_eq!(body, "body");
    }

    // ---------- build_markdown_command ----------

    #[test]
    fn build_markdown_command_uses_extracted_description() {
        let file = MarkdownCommandFile {
            file_path: PathBuf::from("/root/.lingxi/commands/foo.md"),
            base_dir: PathBuf::from("/root/.lingxi/commands"),
            frontmatter: CommandFrontmatter::default(),
            frontmatter_name: None,
            content: "# Foo Title\n\nHello $1".to_string(),
            source: CommandSource::Settings(lingxi_core::types::SettingsScope::Project),
        };
        let cmd = build_markdown_command(
            &file,
            CommandSource::Settings(lingxi_core::types::SettingsScope::Project),
        );
        assert_eq!(cmd.name.string_at(""), Some("foo"));
        assert_eq!(
            cmd.user_facing_name
                .as_ref()
                .and_then(|name| name.string_at("")),
            Some("foo"),
            "Native prompt factories return the canonical name when frontmatter name is absent"
        );
        assert_eq!(cmd.description.string_at(""), Some("Foo Title"));
        match cmd.kind {
            SlashCommandKind::Markdown {
                prompt_template, ..
            } => assert_eq!(
                prompt_template.string_at(""),
                Some("# Foo Title\n\nHello $1")
            ),
            _ => panic!("expected Markdown kind"),
        }
    }

    #[test]
    fn markdown_prompt_preserves_frontmatter_display_name_separately_from_path_name() {
        let file = parse_command_markdown(
            "---\nname: Friendly\n---\nbody",
            PathBuf::from("/root/.lingxi/commands/folder/file.md"),
            PathBuf::from("/root/.lingxi/commands"),
            CommandSource::Settings(lingxi_core::types::SettingsScope::Project),
        );
        let command = build_markdown_command(
            &file,
            CommandSource::Settings(lingxi_core::types::SettingsScope::Project),
        );
        assert_eq!(command.name.string_at(""), Some("folder:file"));
        assert_eq!(
            command
                .user_facing_name
                .as_ref()
                .and_then(|name| name.string_at("")),
            Some("Friendly")
        );
    }

    #[test]
    fn build_markdown_command_prefers_frontmatter_description() {
        let fm = CommandFrontmatter {
            description: "explicit".to_string(),
            ..CommandFrontmatter::default()
        };
        let file = MarkdownCommandFile {
            file_path: PathBuf::from("/r/.lingxi/commands/x.md"),
            base_dir: PathBuf::from("/r/.lingxi/commands"),
            frontmatter: fm,
            frontmatter_name: None,
            content: "# Ignored Title".to_string(),
            source: CommandSource::Settings(lingxi_core::types::SettingsScope::User),
        };
        let cmd = build_markdown_command(
            &file,
            CommandSource::Settings(lingxi_core::types::SettingsScope::User),
        );
        assert_eq!(cmd.description.string_at(""), Some("explicit"));
    }

    #[test]
    fn build_markdown_command_copies_argument_hint_and_sets_loaded_from() {
        // SLASH.2: the frontmatter `argument-hint` is surfaced on the top-level
        // command. SLASH.3: legacy commands are tagged `commands_DEPRECATED`.
        let fm = CommandFrontmatter {
            argument_hints: vec!["<file> [flags]".to_string()],
            ..CommandFrontmatter::default()
        };
        let file = MarkdownCommandFile {
            file_path: PathBuf::from("/r/.lingxi/commands/x.md"),
            base_dir: PathBuf::from("/r/.lingxi/commands"),
            frontmatter: fm,
            frontmatter_name: None,
            content: "# Body".to_string(),
            source: CommandSource::Settings(lingxi_core::types::SettingsScope::Project),
        };
        let cmd = build_markdown_command(
            &file,
            CommandSource::Settings(lingxi_core::types::SettingsScope::Project),
        );
        assert_eq!(
            cmd.argument_hint
                .as_ref()
                .and_then(|hint| hint.string_at("")),
            Some("<file> [flags]")
        );
        assert_eq!(cmd.loaded_from.as_deref(), Some("commands_DEPRECATED"));
    }

    #[test]
    fn build_markdown_command_copies_argument_names_from_frontmatter() {
        // ARGS.3: the parsed `argNames` are surfaced on the top-level command so
        // the TUI can render the progressive argument-hint.
        let fm = CommandFrontmatter {
            argument_names: vec!["first".to_string(), "second".to_string()],
            ..CommandFrontmatter::default()
        };
        let file = MarkdownCommandFile {
            file_path: PathBuf::from("/r/.lingxi/commands/x.md"),
            base_dir: PathBuf::from("/r/.lingxi/commands"),
            frontmatter: fm,
            frontmatter_name: None,
            content: "# Body".to_string(),
            source: CommandSource::Settings(lingxi_core::types::SettingsScope::Project),
        };
        let cmd = build_markdown_command(
            &file,
            CommandSource::Settings(lingxi_core::types::SettingsScope::Project),
        );
        assert_eq!(
            cmd.argument_names
                .iter()
                .map(|name| name.string_at(""))
                .collect::<Vec<_>>(),
            vec![Some("first"), Some("second")]
        );
    }

    #[test]
    fn build_markdown_command_defaults_argument_names_to_empty() {
        // No `arguments` frontmatter → empty list (the built-in default).
        let file = MarkdownCommandFile {
            file_path: PathBuf::from("/r/.lingxi/commands/x.md"),
            base_dir: PathBuf::from("/r/.lingxi/commands"),
            frontmatter: CommandFrontmatter::default(),
            frontmatter_name: None,
            content: "# Body".to_string(),
            source: CommandSource::Settings(lingxi_core::types::SettingsScope::Project),
        };
        let cmd = build_markdown_command(
            &file,
            CommandSource::Settings(lingxi_core::types::SettingsScope::Project),
        );
        assert!(cmd.argument_names.is_empty());
    }

    #[test]
    fn slash_command_serde_omits_empty_argument_names() {
        // Parity guard: an empty `argument_names` must NOT appear in the wire
        // JSON, keeping the serialized shape byte-identical to before this field
        // existed (which is what keeps parity_slash_commands*.json unchanged).
        let cmd = crate::model::SlashCommand {
            name: "help".into(),
            description: "Show help".into(),
            ..crate::model::SlashCommand::default()
        };
        let json = serde_json::to_string(&cmd).expect("serialize");
        assert!(
            !json.contains("argument_names"),
            "empty argument_names must be omitted, got: {json}"
        );
        // A non-empty list round-trips.
        let cmd2 = crate::model::SlashCommand {
            name: "deploy".into(),
            argument_names: vec!["env".into()],
            ..crate::model::SlashCommand::default()
        };
        let json2 = serde_json::to_string(&cmd2).expect("serialize");
        assert!(
            json2.contains("\"argument_names\":[\"env\"]"),
            "got: {json2}"
        );
    }

    #[test]
    fn frontmatter_disable_model_invocation_bool_and_string_true() {
        // SLASH.1 / cc 2.1.218 `Kde`: `true`, "true", and the 2.1.218 truthy
        // spellings ("yes"/"on"/"1", case-insensitive) all coerce to true.
        for raw in [
            "---\ndisable-model-invocation: true\n---\nx",
            "---\ndisable-model-invocation: \"true\"\n---\nx",
            "---\ndisable-model-invocation: yes\n---\nx",
            "---\ndisable-model-invocation: \"ON\"\n---\nx",
            "---\ndisable-model-invocation: 1\n---\nx",
        ] {
            let (fm, _) = parse_frontmatter(raw);
            assert!(fm.disable_model_invocation, "raw: {raw}");
        }
    }

    #[test]
    fn frontmatter_disable_model_invocation_false_and_absent() {
        // cc 2.1.218: the falsy set ("false"/"no"/"off"/"0"), garbage (rtr →
        // Kde ?? false), and absent are all false.
        for raw in [
            "---\ndisable-model-invocation: false\n---\nx",
            "---\ndisable-model-invocation: \"No\"\n---\nx",
            "---\ndisable-model-invocation: off\n---\nx",
            "---\ndisable-model-invocation: 0\n---\nx",
            "---\ndisable-model-invocation: \"garbage\"\n---\nx",
            "---\ndescription: d\n---\nx",
        ] {
            let (fm, _) = parse_frontmatter(raw);
            assert!(!fm.disable_model_invocation, "raw: {raw}");
        }
    }

    #[test]
    fn frontmatter_background_coercion_and_undeclared() {
        // `background` uses bare `Kde`: truthy/falsy spellings declare it;
        // garbage leaves it UNDECLARED (None) — never false.
        let (fm, _) = parse_frontmatter("---\nbackground: yes\n---\nx");
        assert_eq!(fm.background, Some(true));
        let (fm, _) = parse_frontmatter("---\nbackground: \"off\"\n---\nx");
        assert_eq!(fm.background, Some(false));
        let (fm, _) = parse_frontmatter("---\nbackground: 0\n---\nx");
        assert_eq!(fm.background, Some(false));
        let (fm, _) = parse_frontmatter("---\nbackground: maybe\n---\nx");
        assert_eq!(fm.background, None);
        let (fm, _) = parse_frontmatter("---\ndescription: d\n---\nx");
        assert_eq!(fm.background, None);
    }

    #[test]
    fn frontmatter_when_to_use_parsed_and_copied_to_command() {
        // SLASH.4: `when_to_use` is parsed and surfaced on the top-level command.
        let (fm, _) = parse_frontmatter("---\nwhen_to_use: use for X\n---\nbody");
        assert_eq!(fm.when_to_use.as_deref(), Some("use for X"));
        let file = MarkdownCommandFile {
            file_path: PathBuf::from("/r/.lingxi/commands/x.md"),
            base_dir: PathBuf::from("/r/.lingxi/commands"),
            frontmatter: fm,
            frontmatter_name: None,
            content: "# Body".to_string(),
            source: CommandSource::Settings(lingxi_core::types::SettingsScope::Project),
        };
        let cmd = build_markdown_command(
            &file,
            CommandSource::Settings(lingxi_core::types::SettingsScope::Project),
        );
        assert_eq!(cmd.when_to_use.as_deref(), Some("use for X"));
    }

    #[test]
    fn disable_model_invocation_excludes_command_from_model_invocable_set() {
        // SLASH.1 end-to-end: the parsed flag flows onto SlashCommand so the
        // registry model-invocable filter (`!disable_model_invocation`) drops it.
        let (fm, _) = parse_frontmatter("---\ndisable-model-invocation: true\n---\nx");
        let file = MarkdownCommandFile {
            file_path: PathBuf::from("/r/.lingxi/commands/hidden.md"),
            base_dir: PathBuf::from("/r/.lingxi/commands"),
            frontmatter: fm,
            frontmatter_name: None,
            content: "# Body".to_string(),
            source: CommandSource::Settings(lingxi_core::types::SettingsScope::Project),
        };
        let cmd = build_markdown_command(
            &file,
            CommandSource::Settings(lingxi_core::types::SettingsScope::Project),
        );
        assert!(cmd.disable_model_invocation);
    }

    #[test]
    fn build_markdown_command_argument_hint_absent_is_none() {
        // No `argument-hint` frontmatter -> top-level hint stays `None`, but the
        // `commands_DEPRECATED` marker is still set.
        let file = MarkdownCommandFile {
            file_path: PathBuf::from("/r/.lingxi/commands/y.md"),
            base_dir: PathBuf::from("/r/.lingxi/commands"),
            frontmatter: CommandFrontmatter::default(),
            frontmatter_name: None,
            content: "# Body".to_string(),
            source: CommandSource::Settings(lingxi_core::types::SettingsScope::Project),
        };
        let cmd = build_markdown_command(
            &file,
            CommandSource::Settings(lingxi_core::types::SettingsScope::Project),
        );
        assert_eq!(cmd.argument_hint, None);
        assert_eq!(cmd.loaded_from.as_deref(), Some("commands_DEPRECATED"));
    }

    // ---------- filesystem loader (integration) ----------

    /// Unique temp dir under the OS temp root.
    fn temp_dir(tag: &str) -> PathBuf {
        let base = std::env::temp_dir().join(format!(
            "cmdapi-mdtest-{tag}-{}-{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        fs::create_dir_all(&base).unwrap();
        base
    }

    fn write(path: &Path, contents: &str) {
        if let Some(parent) = path.parent() {
            fs::create_dir_all(parent).unwrap();
        }
        fs::write(path, contents).unwrap();
    }

    #[tokio::test]
    async fn loads_top_level_and_namespaced_commands() {
        let root = temp_dir("names");
        let cmds = root.join("proj").join(".lingxi").join("commands");
        write(&cmds.join("foo.md"), "Hello $1");
        write(&cmds.join("sub").join("bar.md"), "Bar body");

        let home = root.join("home");
        fs::create_dir_all(&home).unwrap();
        let lingxi_home = root.join("home").join(".lingxi");
        let managed = root.join("managed-none");

        let files =
            load_command_markdown_files(&root.join("proj"), &lingxi_home, &managed, &home).await;
        let mut names: Vec<String> = files
            .iter()
            .map(|f| command_name_from_path(&f.file_path, &f.base_dir))
            .collect();
        names.sort();
        assert_eq!(names, vec!["foo".to_string(), "sub:bar".to_string()]);

        fs::remove_dir_all(&root).ok();
    }

    #[tokio::test]
    async fn managed_user_project_precedence_via_dedup() {
        // Same physical file shared (hardlink) across managed and project: managed
        // wins (loaded first), project duplicate dropped by inode dedup.
        let root = temp_dir("prec");
        let home = root.join("home");
        fs::create_dir_all(&home).unwrap();

        let project = root.join("proj");
        let proj_cmds = project.join(".lingxi").join("commands");
        write(&proj_cmds.join("shared.md"), "shared body");

        let managed = root.join("managed");
        let managed_cmds = managed.join(".lingxi").join("commands");
        fs::create_dir_all(&managed_cmds).unwrap();
        // Hardlink the same inode into the managed dir.
        fs::hard_link(proj_cmds.join("shared.md"), managed_cmds.join("shared.md")).unwrap();

        let lingxi_home = root.join("home").join(".lingxi");
        let files = load_command_markdown_files(&project, &lingxi_home, &managed, &home).await;
        let shared: Vec<&MarkdownCommandFile> = files
            .iter()
            .filter(|f| f.content == "shared body")
            .collect();
        assert_eq!(shared.len(), 1, "inode dedup should keep exactly one");
        assert_eq!(
            shared[0].source,
            CommandSource::Settings(lingxi_core::types::SettingsScope::Managed),
            "managed must win over project"
        );

        fs::remove_dir_all(&root).ok();
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn inode_dedup_drops_symlinked_duplicate() {
        // Mirrors the TS scenario: a *directory* symlink causes the same physical
        // file to be discovered through two paths. `lstat` of the file reached
        // via the symlinked parent resolves to the real file's inode (only the
        // parent dir was a symlink), so the duplicate is dropped.
        let root = temp_dir("symlink");
        let home = root.join("home");
        fs::create_dir_all(&home).unwrap();

        let project = root.join("proj");
        let proj_cmds = project.join(".lingxi").join("commands");
        write(&proj_cmds.join("real.md"), "real body");

        // The user commands dir is a SYMLINK to the project's commands dir, so the
        // same `real.md` is reachable as both project and user.
        let lingxi_home = root.join("home").join(".lingxi");
        fs::create_dir_all(&lingxi_home).unwrap();
        std::os::unix::fs::symlink(&proj_cmds, lingxi_home.join("commands")).unwrap();

        let managed = root.join("managed-none");
        let files = load_command_markdown_files(&project, &lingxi_home, &managed, &home).await;
        let bodies: Vec<&MarkdownCommandFile> =
            files.iter().filter(|f| f.content == "real body").collect();
        assert_eq!(
            bodies.len(),
            1,
            "symlinked duplicate (same inode) must be dropped"
        );

        fs::remove_dir_all(&root).ok();
    }

    #[tokio::test]
    async fn missing_dirs_yield_empty() {
        let root = temp_dir("empty");
        let home = root.join("home");
        fs::create_dir_all(&home).unwrap();
        let lingxi_home = root.join("home").join(".lingxi");
        let managed = root.join("nope");
        let files =
            load_command_markdown_files(&root.join("noproj"), &lingxi_home, &managed, &home).await;
        assert!(files.is_empty());
        fs::remove_dir_all(&root).ok();
    }

    #[test]
    fn frontmatter_effort_accepts_both_yaml_spellings() {
        // `effort: high` is a YAML string, `effort: 500` a YAML integer. Both
        // are legal arms of upstream's union, so the raw carrier must take
        // either -- an `Option<String>` field alone would fail to deserialize
        // the integer form and silently lose it.
        for (raw, want) in [
            ("---\neffort: high\n---\nx", Some("high")),
            ("---\neffort: 500\n---\nx", Some("500")),
            ("---\neffort: \"max\"\n---\nx", Some("max")),
            ("---\ndescription: d\n---\nx", None),
        ] {
            let (fm, _) = parse_frontmatter(raw);
            assert_eq!(fm.effort.as_deref(), want, "raw: {raw}");
        }
    }

    #[test]
    fn frontmatter_effort_is_not_validated_at_parse_time() {
        // Upstream validates in `Gx` at the conversion, not in the frontmatter
        // reader; keeping this layer raw is what lets an unusable value degrade
        // to "declared none" instead of failing the whole skill.
        let (fm, _) = parse_frontmatter("---\neffort: nonsense\n---\nx");
        assert_eq!(fm.effort.as_deref(), Some("nonsense"));
    }

    #[test]
    fn frontmatter_user_invocable_absent_is_undeclared_not_false() {
        // `dt === void 0 ? !0 : htt(dt)` — an absent key must stay
        // distinguishable from a declared `false`, or every skill that never
        // mentions the key would be hidden.
        let (fm, _) = parse_frontmatter("---\ndescription: d\n---\nx");
        assert_eq!(fm.user_invocable, None, "absent key is undeclared");
    }

    #[test]
    fn frontmatter_user_invocable_coerces_like_htt() {
        // `htt(e) = c1(e) ?? !1`: the truthy set, the falsy set, and — unlike
        // `background`, which uses the bare `c1` — garbage lands on FALSE.
        for (raw, want) in [
            ("---\nuser-invocable: true\n---\nx", Some(true)),
            ("---\nuser-invocable: \"Yes\"\n---\nx", Some(true)),
            ("---\nuser-invocable: on\n---\nx", Some(true)),
            ("---\nuser-invocable: 1\n---\nx", Some(true)),
            ("---\nuser-invocable: false\n---\nx", Some(false)),
            ("---\nuser-invocable: \"No\"\n---\nx", Some(false)),
            ("---\nuser-invocable: off\n---\nx", Some(false)),
            ("---\nuser-invocable: 0\n---\nx", Some(false)),
            ("---\nuser-invocable: \"garbage\"\n---\nx", Some(false)),
        ] {
            let (fm, _) = parse_frontmatter(raw);
            assert_eq!(fm.user_invocable, want, "raw: {raw}");
        }
    }

    #[tokio::test]
    async fn a_skill_declaring_user_invocable_false_is_not_user_invocable() {
        // The defect this pins: `build_skill_command` hardcoded `Some(true)`,
        // so a skill asking to be hidden stayed in the `/` menu. Upstream
        // carries the parsed value (`userInvocable: en`) and derives
        // `isHidden: !(userInvocable ?? !0)` from it.
        let root = temp_dir("skills_user_invocable");
        let home = root.join("home");
        fs::create_dir_all(&home).unwrap();
        let repo = root.join("repo");
        fs::create_dir_all(repo.join(".git")).unwrap();
        let skills = repo.join(".lingxi").join("skills");
        write(
            &skills.join("hidden").join("SKILL.md"),
            "---\ndescription: Hidden\nuser-invocable: false\n---\nbody\n",
        );
        write(
            &skills.join("shown").join("SKILL.md"),
            "---\ndescription: Shown\n---\nbody\n",
        );

        let lingxi_home = home.join(".lingxi");
        let files = load_skill_markdown_files(&repo, &lingxi_home, &home).await;
        assert_eq!(files.len(), 2, "both skills load");

        for file in &files {
            let cmd = build_skill_command(
                file,
                CommandSource::Settings(lingxi_core::types::SettingsScope::Project),
            );
            let want = match cmd.name.string_at("").unwrap_or_default() {
                "hidden" => Some(false),
                "shown" => Some(true),
                other => panic!("unexpected skill {other}"),
            };
            assert_eq!(
                cmd.user_invocable,
                want,
                "{} must honour its frontmatter",
                cmd.name.string_at("").unwrap_or_default()
            );
        }
    }

    #[tokio::test]
    async fn loads_directory_format_skills_as_skill_commands_with_metadata() {
        let root = temp_dir("skills");
        let home = root.join("home");
        fs::create_dir_all(&home).unwrap();

        let repo = root.join("repo");
        fs::create_dir_all(repo.join(".git")).unwrap();
        let skill_dir = repo.join(".lingxi").join("skills").join("demo");
        let skill_markdown = "---\nname: Demo display\ndescription: Demo skill\nwhen_to_use: when demo is useful\n---\nUse $ARGUMENTS well\n";
        write(&skill_dir.join("SKILL.md"), skill_markdown);

        write(
            &repo.join(".lingxi").join("commands").join("demo.md"),
            "# Legacy demo\n",
        );

        let lingxi_home = home.join(".lingxi");
        let skill_files = load_skill_markdown_files(&repo, &lingxi_home, &home).await;
        assert_eq!(skill_files.len(), 1);

        let cmd = build_skill_command(
            &skill_files[0],
            CommandSource::Settings(lingxi_core::types::SettingsScope::Project),
        );
        assert_eq!(cmd.name.string_at(""), Some("demo"));
        assert_eq!(
            cmd.user_facing_name
                .as_ref()
                .and_then(|name| name.string_at("")),
            Some("Demo display")
        );
        assert_eq!(cmd.description.string_at(""), Some("Demo skill"));
        assert_eq!(cmd.loaded_from.as_deref(), Some("skills"));
        assert_eq!(cmd.skill_root.as_deref(), Some(skill_dir.as_path()));
        assert_eq!(cmd.content_length, Some(skill_markdown.len()));
        assert_eq!(cmd.when_to_use.as_deref(), Some("when demo is useful"));
        match &cmd.kind {
            SlashCommandKind::Markdown {
                file_path,
                prompt_template,
                ..
            } => {
                assert_eq!(file_path.as_path(), skill_dir.join("SKILL.md").as_path());
                assert_eq!(prompt_template.string_at(""), Some("Use $ARGUMENTS well\n"));
            }
            other => panic!("expected Markdown kind, got {other:?}"),
        }

        let command_files = load_command_markdown_files(&repo, &lingxi_home, &root, &home).await;
        let legacy = command_files
            .iter()
            .find(|f| f.file_path.ends_with("demo.md"))
            .expect("legacy command should load");
        let legacy_cmd = build_markdown_command(legacy, legacy.source);
        assert_eq!(
            legacy_cmd.loaded_from.as_deref(),
            Some("commands_DEPRECATED")
        );
        assert!(legacy_cmd.skill_root.is_none());
        assert_eq!(legacy_cmd.content_length, None);

        fs::remove_dir_all(&root).ok();
    }

    #[test]
    fn native_skill_directory_sort_uses_basename_not_frontmatter_display_name() {
        let root = temp_dir("skill-locale-sort");
        let skills = root.join("skills");
        let entries = [
            ("file2", "display-z-file2"),
            ("file10", "display-a-file10"),
            ("foo-2", "display-y-hyphen"),
            ("foo_2", "display-b-underscore"),
            ("ä", "display-x-umlaut"),
            ("ae", "display-c-ae"),
            ("skill-🧪-probe", "display-w-emoji"),
            ("skill.alpha+beta", "display-d-punctuation"),
        ];
        for (directory_name, display_name) in entries {
            write(
                &skills.join(directory_name).join("SKILL.md"),
                &format!("---\nname: {display_name}\n---\nbody\n"),
            );
        }

        let collator = crate::native_locale_collation::collator_for_locale("en-US");
        let loaded = load_skill_dir(
            &skills,
            CommandSource::Settings(lingxi_core::types::SettingsScope::Project),
            &collator,
        );
        let actual: Vec<(String, String)> = loaded
            .iter()
            .map(|file| {
                (
                    file.skill_root
                        .file_name()
                        .expect("skill directory name")
                        .to_string_lossy()
                        .into_owned(),
                    file.frontmatter_name
                        .as_ref()
                        .and_then(|name| name.string_at(""))
                        .expect("frontmatter display name")
                        .to_owned(),
                )
            })
            .collect();

        assert_eq!(
            actual,
            [
                ("ä".to_owned(), "display-x-umlaut".to_owned()),
                ("ae".to_owned(), "display-c-ae".to_owned()),
                ("file10".to_owned(), "display-a-file10".to_owned()),
                ("file2".to_owned(), "display-z-file2".to_owned()),
                ("foo_2".to_owned(), "display-b-underscore".to_owned()),
                ("foo-2".to_owned(), "display-y-hyphen".to_owned()),
                ("skill-🧪-probe".to_owned(), "display-w-emoji".to_owned()),
                (
                    "skill.alpha+beta".to_owned(),
                    "display-d-punctuation".to_owned()
                ),
            ]
        );

        fs::remove_dir_all(&root).ok();
    }

    #[tokio::test]
    async fn user_skill_precedes_same_named_project_skill() {
        let root = temp_dir("skill-collision");
        let home = root.join("home");
        fs::create_dir_all(&home).unwrap();
        let lingxi_home = home.join(".lingxi");

        let repo = root.join("repo");
        fs::create_dir_all(repo.join(".git")).unwrap();
        write(
            &lingxi_home.join("skills").join("dup").join("SKILL.md"),
            "---\ndescription: User skill\n---\nUSER body\n",
        );
        write(
            &repo
                .join(".lingxi")
                .join("skills")
                .join("dup")
                .join("SKILL.md"),
            "---\ndescription: Project skill\n---\nPROJECT body\n",
        );

        let skill_files = load_skill_markdown_files(&repo, &lingxi_home, &home).await;
        let first_dup = skill_files
            .iter()
            .find(|f| f.skill_root.file_name().is_some_and(|n| n == "dup"))
            .expect("dup skill should load");
        assert_eq!(
            first_dup.source,
            CommandSource::Settings(lingxi_core::types::SettingsScope::User)
        );
        assert_eq!(first_dup.frontmatter.description, "User skill");

        fs::remove_dir_all(&root).ok();
    }

    #[tokio::test]
    async fn managed_user_project_additional_skill_order_is_preserved() {
        let root = temp_dir("skill-layering");
        let home = root.join("home");
        fs::create_dir_all(&home).unwrap();
        let lingxi_home = home.join(".lingxi");
        let managed = root.join("managed");
        let additional = root.join("extra-skills");

        let repo = root.join("repo");
        fs::create_dir_all(repo.join(".git")).unwrap();
        write(
            &managed
                .join(".lingxi")
                .join("skills")
                .join("dup")
                .join("SKILL.md"),
            "---\ndescription: Managed skill\n---\nMANAGED body\n",
        );
        write(
            &lingxi_home.join("skills").join("dup").join("SKILL.md"),
            "---\ndescription: User skill\n---\nUSER body\n",
        );
        write(
            &repo
                .join(".lingxi")
                .join("skills")
                .join("dup")
                .join("SKILL.md"),
            "---\ndescription: Project skill\n---\nPROJECT body\n",
        );
        write(
            &additional.join("extra").join("SKILL.md"),
            "---\ndescription: Extra skill\n---\nEXTRA body\n",
        );

        let skill_files = load_skill_markdown_files_with_roots(
            &repo,
            &lingxi_home,
            Some(&managed),
            &home,
            std::slice::from_ref(&additional),
        )
        .await;
        let names: Vec<String> = skill_files
            .iter()
            .map(|f| {
                f.skill_root
                    .file_name()
                    .expect("skill dir")
                    .to_string_lossy()
                    .into_owned()
            })
            .collect();
        assert_eq!(names, vec!["dup", "dup", "dup", "extra"]);
        assert_eq!(
            skill_files[0].source,
            CommandSource::Settings(lingxi_core::types::SettingsScope::Managed)
        );
        assert_eq!(
            skill_files[1].source,
            CommandSource::Settings(lingxi_core::types::SettingsScope::User)
        );
        assert_eq!(
            skill_files[2].source,
            CommandSource::Settings(lingxi_core::types::SettingsScope::Project)
        );
        assert_eq!(
            skill_files[3].source,
            CommandSource::Settings(lingxi_core::types::SettingsScope::Project)
        );

        fs::remove_dir_all(&root).ok();
    }

    #[test]
    fn project_dirs_stops_at_git_root() {
        let root = temp_dir("gitstop");
        let home = root.join("home");
        fs::create_dir_all(&home).unwrap();

        // root/repo/.git , root/repo/.lingxi/commands , root/repo/nested/.lingxi/commands
        let repo = root.join("repo");
        fs::create_dir_all(repo.join(".git")).unwrap();
        fs::create_dir_all(repo.join(".lingxi").join("commands")).unwrap();
        let nested = repo.join("nested");
        fs::create_dir_all(nested.join(".lingxi").join("commands")).unwrap();
        // Above the repo: a .lingxi/commands that must NOT be collected.
        fs::create_dir_all(root.join(".lingxi").join("commands")).unwrap();

        let dirs = project_dirs_up_to_home("commands", &nested, &home);
        // Most-specific first: nested, then repo. Stops at repo (git root).
        assert_eq!(
            dirs,
            vec![
                nested.join(".lingxi").join("commands"),
                repo.join(".lingxi").join("commands"),
            ]
        );

        fs::remove_dir_all(&root).ok();
    }
}
