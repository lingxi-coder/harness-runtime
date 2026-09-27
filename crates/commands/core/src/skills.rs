//! `/skills` — list available file-based skills in non-TUI command paths.
//!
//! Claude Code's `/skills` command is a `local-jsx` command that opens
//! `SkillsMenu`. The `LingXi` TUI already intercepts `/skills` and opens the
//! full-screen viewer from `tui::screens::skills`; this handler covers the
//! registry/bridge/headless path where no interactive screen can be opened.
//!
//! The data model mirrors the TUI viewer: project `.lingxi/skills/` directories
//! from the current directory up to the nearest git root, then
//! `~/.lingxi/skills/`, directory-format skills only, and each skill's display
//! name comes from the directory name rather than frontmatter.

use async_trait::async_trait;
use command_api::model::{BuiltinCommandHandler, CommandResult};
use command_api::parser::ParsedSlashCommand;
use std::path::{Path, PathBuf};

const DESCRIPTION: &str = "List available skills";
const TITLE: &str = "Skills";
const EMPTY_SUBTITLE: &str = "No skills found";
const EMPTY_BODY: &str = "Create skills in .lingxi/skills/ or ~/.lingxi/skills/";

trait SkillRowExt {
    fn estimated_tokens(&self) -> usize;
}

impl SkillRowExt for skill_api::FileSkillRow {
    fn estimated_tokens(&self) -> usize {
        let mut parts: Vec<&str> = Vec::with_capacity(3);
        if !self.name.is_empty() {
            parts.push(&self.name);
        }
        if !self.description.is_empty() {
            parts.push(&self.description);
        }
        if let Some(w) = &self.when_to_use {
            if !w.is_empty() {
                parts.push(w);
            }
        }
        rough_token_count(&parts.join(" "))
    }
}

/// `/skills` handler.
#[derive(Debug, Default)]
pub struct SkillsHandler {
    roots: Option<SkillsRoots>,
}

#[derive(Debug, Clone, Default)]
struct SkillsRoots {
    cwd: PathBuf,
    lingxi_home: PathBuf,
    managed_dir: Option<PathBuf>,
    additional_skill_dirs: Vec<PathBuf>,
}

impl SkillsHandler {
    /// Construct a new `SkillsHandler`.
    #[must_use]
    pub fn new() -> Self {
        Self { roots: None }
    }

    /// Construct a handler pinned to explicit `(cwd, lingxi_home)` roots.
    #[must_use]
    pub fn with_roots(cwd: PathBuf, lingxi_home: PathBuf) -> Self {
        Self {
            roots: Some(SkillsRoots {
                cwd,
                lingxi_home,
                managed_dir: None,
                additional_skill_dirs: Vec::new(),
            }),
        }
    }

    /// Construct a handler pinned to explicit roots, including managed and
    /// additional skills directories.
    #[must_use]
    pub fn with_all_roots(
        cwd: PathBuf,
        lingxi_home: PathBuf,
        managed_dir: Option<PathBuf>,
        additional_skill_dirs: Vec<PathBuf>,
    ) -> Self {
        Self {
            roots: Some(SkillsRoots {
                cwd,
                lingxi_home,
                managed_dir,
                additional_skill_dirs,
            }),
        }
    }
}

#[async_trait]
impl BuiltinCommandHandler for SkillsHandler {
    async fn handle(&self, _args: &ParsedSlashCommand) -> CommandResult {
        let roots = self.roots.clone().unwrap_or_else(|| SkillsRoots {
            cwd: std::env::current_dir().unwrap_or_else(|_| PathBuf::from(".")),
            lingxi_home: lingxi_home_dir(),
            managed_dir: None,
            additional_skill_dirs: Vec::new(),
        });
        CommandResult::Done {
            display: Some(render_available_skills_with_roots(
                &roots.cwd,
                &roots.lingxi_home,
                roots.managed_dir.as_deref(),
                &roots.additional_skill_dirs,
            )),
        }
    }

    fn name(&self) -> &str {
        "skills"
    }

    fn description(&self) -> &str {
        DESCRIPTION
    }
}

fn lingxi_home_dir() -> PathBuf {
    // claude-code `tr()`: `$LINGXI_CONFIG_DIR` when set wins (`??`: an empty value
    // is honored verbatim → cwd-relative), else `$HOME/.claude` (with a
    // `$USERPROFILE` fallback for Windows, matching tools/file + tools/task).
    if let Some(dir) = std::env::var_os(branding::CONFIG_DIR_ENV) {
        return PathBuf::from(dir);
    }
    std::env::var_os("HOME")
        .or_else(|| std::env::var_os("USERPROFILE"))
        .map_or_else(
            || PathBuf::from(".").join(branding::DOT_DIR),
            |h| PathBuf::from(h).join(branding::DOT_DIR),
        )
}

#[cfg(test)]
fn render_available_skills(cwd: &Path, lingxi_home: &Path) -> String {
    render_sections(&skill_api::load_file_skill_sections(cwd, lingxi_home))
}

fn render_available_skills_with_roots(
    cwd: &Path,
    lingxi_home: &Path,
    managed_dir: Option<&Path>,
    additional_skill_dirs: &[PathBuf],
) -> String {
    render_sections(&skill_api::load_file_skill_sections_with_roots(
        cwd,
        lingxi_home,
        managed_dir,
        additional_skill_dirs,
    ))
}

fn render_sections(sections: &[skill_api::FileSkillSection]) -> String {
    let total: usize = sections.iter().map(|s| s.rows.len()).sum();
    if total == 0 {
        return format!("{TITLE}\n{EMPTY_SUBTITLE}\n{EMPTY_BODY}\nEsc to close");
    }

    let mut out = format!("{TITLE}\n{total} {}\n", plural(total, "skill"));
    for section in sections {
        if section.rows.is_empty() {
            continue;
        }
        out.push_str(&section.title);
        out.push('\n');
        for row in &section.rows {
            out.push_str(&render_skill_row(row));
            out.push('\n');
        }
    }
    out.push_str("Esc to close");
    out
}

fn render_skill_row(row: &skill_api::FileSkillRow) -> String {
    format!(
        "{} \u{00B7} ~{} description tokens",
        row.name,
        format_tokens(row.estimated_tokens())
    )
}

fn plural(n: usize, word: &str) -> String {
    if n == 1 {
        word.to_string()
    } else {
        format!("{word}s")
    }
}

fn rough_token_count(content: &str) -> usize {
    let chars = content.chars().count();
    (chars + 2) / 4
}

#[allow(clippy::cast_precision_loss)]
fn format_tokens(count: usize) -> String {
    if count < 1000 {
        return count.to_string();
    }
    let (value, suffix) = if count >= 1_000_000 {
        (count as f64 / 1_000_000.0, 'm')
    } else {
        (count as f64 / 1000.0, 'k')
    };
    let mut s = format!("{value:.1}");
    if let Some(stripped) = s.strip_suffix(".0") {
        s = stripped.to_string();
    }
    format!("{s}{suffix}")
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs;
    use std::time::{SystemTime, UNIX_EPOCH};

    fn tmp_root(name: &str) -> PathBuf {
        let nanos = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .expect("clock before epoch")
            .as_nanos();
        let dir = std::env::temp_dir().join(format!(
            "lingxi-skills-{name}-{}-{nanos}",
            std::process::id()
        ));
        fs::create_dir_all(&dir).expect("create temp root");
        dir
    }

    fn write_skill(base: &Path, name: &str, description: &str, when_to_use: Option<&str>) {
        let dir = base.join(name);
        fs::create_dir_all(&dir).expect("create skill dir");
        let when = when_to_use.map_or(String::new(), |w| format!("when_to_use: {w}\n"));
        fs::write(
            dir.join("SKILL.md"),
            format!("---\ndescription: {description}\n{when}---\nBody\n"),
        )
        .expect("write skill");
    }

    #[test]
    fn empty_state_matches_tui_copy() {
        let root = tmp_root("empty");
        let cwd = root.join("repo");
        let home = root.join("home").join(".lingxi");
        fs::create_dir_all(&cwd).expect("create cwd");
        fs::create_dir_all(&home).expect("create home");

        assert_eq!(
            render_available_skills(&cwd, &home),
            "Skills\nNo skills found\nCreate skills in .lingxi/skills/ or ~/.lingxi/skills/\nEsc to close"
        );
        fs::remove_dir_all(root).ok();
    }

    #[test]
    fn lists_project_then_user_skills() {
        let root = tmp_root("list");
        let repo = root.join("repo");
        let cwd = repo.join("nested");
        let home = root.join("home").join(".lingxi");
        fs::create_dir_all(repo.join(".git")).expect("create git marker");
        fs::create_dir_all(&cwd).expect("create cwd");
        fs::create_dir_all(&home).expect("create home");

        write_skill(
            &cwd.join(".lingxi").join("skills"),
            "alpha",
            "does alpha things",
            None,
        );
        write_skill(
            &home.join("skills"),
            "beta",
            "does beta things",
            Some("beta"),
        );

        let out = render_available_skills(&cwd, &home);
        assert!(out.starts_with("Skills\n2 skills\n"));
        assert!(out.contains("Project skills\nalpha \u{00B7} ~"));
        assert!(out.contains("User skills\nbeta \u{00B7} ~"));
        assert!(
            out.find("Project skills") < out.find("User skills"),
            "project skills should render before user skills: {out}"
        );
        fs::remove_dir_all(root).ok();
    }

    #[test]
    fn ignores_loose_markdown_files() {
        let root = tmp_root("loose");
        let cwd = root.join("repo");
        let home = root.join("home").join(".lingxi");
        let skills = cwd.join(".lingxi").join("skills");
        fs::create_dir_all(&skills).expect("create skills dir");
        fs::create_dir_all(&home).expect("create home");
        fs::write(skills.join("loose.md"), "---\ndescription: x\n---\n").expect("write loose");

        assert!(skill_api::load_file_skill_sections(&cwd, &home).is_empty());
        fs::remove_dir_all(root).ok();
    }

    #[tokio::test]
    async fn handler_name_and_description() {
        let h = SkillsHandler::new();
        assert_eq!(h.name(), "skills");
        assert_eq!(h.description(), DESCRIPTION);
    }

    #[tokio::test]
    async fn handler_with_roots_uses_configured_roots() {
        let root = tmp_root("configured-roots");
        let cwd = root.join("repo");
        let home = root.join("home").join(".lingxi");
        fs::create_dir_all(cwd.join(".git")).expect("create git marker");
        fs::create_dir_all(&home).expect("create home");
        write_skill(
            &cwd.join(".lingxi").join("skills"),
            "configured",
            "configured skill",
            None,
        );
        let h = SkillsHandler::with_roots(cwd, home);
        match h
            .handle(&ParsedSlashCommand {
                name: "skills".to_string(),
                raw_args: String::new(),
                positional_args: Vec::new(),
            })
            .await
        {
            CommandResult::Done { display: Some(s) } => {
                assert!(s.contains("configured \u{00B7}"));
            }
            other => panic!("expected Done display, got {other:?}"),
        }
        fs::remove_dir_all(root).ok();
    }

    #[tokio::test]
    async fn handler_with_all_roots_lists_managed_and_additional_sections() {
        let root = tmp_root("handler-all-roots");
        let cwd = root.join("repo");
        let home = root.join("home").join(".lingxi");
        let managed = root.join("managed");
        let additional = root.join("extra-skills");
        fs::create_dir_all(cwd.join(".git")).expect("create git marker");
        fs::create_dir_all(&home).expect("create home");

        write_skill(
            &managed.join(".lingxi").join("skills"),
            "org",
            "managed skill",
            None,
        );
        write_skill(&additional, "extra", "additional skill", None);

        let h = SkillsHandler::with_all_roots(cwd, home, Some(managed), vec![additional]);
        match h
            .handle(&ParsedSlashCommand {
                name: "skills".to_string(),
                raw_args: String::new(),
                positional_args: Vec::new(),
            })
            .await
        {
            CommandResult::Done { display: Some(s) } => {
                assert!(s.contains("Managed skills\norg \u{00B7} ~"));
                assert!(s.contains("Additional skills\nextra \u{00B7} ~"));
                assert!(
                    s.find("Managed skills") < s.find("Additional skills"),
                    "managed should render before additional: {s}"
                );
            }
            other => panic!("expected Done display, got {other:?}"),
        }
        fs::remove_dir_all(root).ok();
    }
}
