//! File-based skill listing utilities shared by TUI and command handlers.
//!
//! This is the non-UI half of Claude Code's `loadSkillsFromSkillsDir` +
//! `getProjectDirsUpToHome('skills', cwd)` behavior: read directory-format
//! skills from project `.lingxi/skills/` directories and user
//! `~/.lingxi/skills/`, parse each `SKILL.md`, deduplicate by canonical path,
//! and return rows sorted by directory name.

use crate::{parse_skill_markdown, LoadedFrom, SkillSource};
use std::collections::HashSet;
use std::path::{Path, PathBuf};

/// One file-based skill row reduced to display/listing fields.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct FileSkillRow {
    /// Skill display name. Matches the directory entry name, not frontmatter.
    pub name: String,
    /// Short frontmatter description.
    pub description: String,
    /// Optional long-form frontmatter usage hint.
    pub when_to_use: Option<String>,
    /// The skill's own directory on disk (e.g. `<root>/skills/<name>`), as
    /// discovered by the scan — not the section's aggregate root. Used by the
    /// desktop Skills settings listing to show where each skill came from.
    pub source_dir: PathBuf,
}

/// One source section in Claude Code render order.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct FileSkillSection {
    /// Section title, e.g. `"Project skills"`.
    pub title: String,
    /// Rows sorted by name.
    pub rows: Vec<FileSkillRow>,
    /// (skills-section-subtitle-missing) The section's skills directory —
    /// claude-code `getSourceSubtitle`'s display-path subtitle. The nearest
    /// (most cwd-specific) directory when a section aggregates several
    /// ancestor dirs (project). `None` for an empty section.
    pub path: Option<PathBuf>,
}

/// Load project and user file-based skills for display.
///
/// Project skills come from every existing `<ancestor>/.lingxi/skills` from
/// `cwd` up to the nearest git root, most-specific first. User skills come from
/// `<lingxi_home>/skills`. Empty sections are omitted.
#[must_use]
pub fn load_file_skill_sections(cwd: &Path, lingxi_home: &Path) -> Vec<FileSkillSection> {
    load_file_skill_sections_with_roots(cwd, lingxi_home, None, &[])
}

/// Load managed, project, user, and additional file-based skills for display.
///
/// Empty sections are omitted. Additional roots are explicit `skills/`
/// directories and are rendered after the standard roots.
#[must_use]
pub fn load_file_skill_sections_with_roots(
    cwd: &Path,
    lingxi_home: &Path,
    managed_dir: Option<&Path>,
    additional_skill_dirs: &[PathBuf],
) -> Vec<FileSkillSection> {
    let mut seen: HashSet<PathBuf> = HashSet::new();
    let managed_skills_dir = managed_dir.map(|dir| dir.join(branding::DOT_DIR).join("skills"));
    let managed_rows = managed_skills_dir.as_ref().map_or_else(Vec::new, |dir| {
        load_skills_from_dirs(
            std::slice::from_ref(dir),
            SkillSource::Settings(protocol::SettingsScope::Managed),
            &mut seen,
        )
    });
    let project_dirs = project_skills_dirs(cwd, lingxi_home);
    let project_rows = load_skills_from_dirs(
        &project_dirs,
        SkillSource::Settings(protocol::SettingsScope::Project),
        &mut seen,
    );
    let user_skills_dir = lingxi_home.join("skills");
    let user_rows = load_skills_from_dirs(
        std::slice::from_ref(&user_skills_dir),
        SkillSource::Settings(protocol::SettingsScope::User),
        &mut seen,
    );
    let additional_rows = load_skills_from_dirs(
        additional_skill_dirs,
        SkillSource::Settings(protocol::SettingsScope::Project),
        &mut seen,
    );

    let mut sections = Vec::with_capacity(4);
    if !managed_rows.is_empty() {
        sections.push(FileSkillSection {
            title: "Managed skills".to_string(),
            rows: managed_rows,
            path: managed_skills_dir,
        });
    }
    if !project_rows.is_empty() {
        sections.push(FileSkillSection {
            title: "Project skills".to_string(),
            rows: project_rows,
            path: project_dirs.into_iter().next(),
        });
    }
    if !user_rows.is_empty() {
        sections.push(FileSkillSection {
            title: "User skills".to_string(),
            rows: user_rows,
            path: Some(user_skills_dir),
        });
    }
    if !additional_rows.is_empty() {
        sections.push(FileSkillSection {
            title: "Additional skills".to_string(),
            rows: additional_rows,
            path: additional_skill_dirs.first().cloned(),
        });
    }
    sections
}

fn project_skills_dirs(cwd: &Path, lingxi_home: &Path) -> Vec<PathBuf> {
    let home = lingxi_home.parent();
    let git_root = nearest_git_root(cwd);
    let mut dirs = Vec::new();
    let mut current = Some(cwd);

    while let Some(dir) = current {
        if Some(dir) == home {
            break;
        }
        let candidate = dir.join(branding::DOT_DIR).join("skills");
        if candidate.is_dir() {
            dirs.push(candidate);
        }
        if git_root.as_deref() == Some(dir) {
            break;
        }
        current = dir.parent();
    }
    dirs
}

fn nearest_git_root(cwd: &Path) -> Option<PathBuf> {
    let mut current = Some(cwd);
    while let Some(dir) = current {
        if dir.join(".git").exists() {
            return Some(dir.to_path_buf());
        }
        current = dir.parent();
    }
    None
}

fn load_skills_from_dirs(
    dirs: &[PathBuf],
    source: SkillSource,
    seen: &mut HashSet<PathBuf>,
) -> Vec<FileSkillRow> {
    let mut rows = Vec::new();
    for base in dirs {
        let Ok(entries) = std::fs::read_dir(base) else {
            continue;
        };
        for entry in entries.flatten() {
            let path = entry.path();
            if !std::fs::metadata(&path).is_ok_and(|m| m.is_dir()) {
                continue;
            }

            let skill_file = path.join("SKILL.md");
            let Ok(raw) = std::fs::read_to_string(&skill_file) else {
                continue;
            };
            let identity = std::fs::canonicalize(&skill_file).unwrap_or(skill_file);
            if !seen.insert(identity) {
                continue;
            }

            let dir_name = entry.file_name().to_string_lossy().into_owned();
            let source_dir = path.clone();
            let Ok(skill) = parse_skill_markdown(&raw, path, source, LoadedFrom::Skills) else {
                continue;
            };
            rows.push(FileSkillRow {
                name: dir_name,
                description: skill.description,
                when_to_use: skill.frontmatter.when_to_use,
                source_dir,
            });
        }
    }
    rows.sort_by(|a, b| a.name.cmp(&b.name));
    rows
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
            "lingxi-skill-api-listing-{name}-{}-{nanos}",
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
    fn lists_project_then_user_skills() {
        let root = tmp_root("sections");
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

        let sections = load_file_skill_sections(&cwd, &home);
        assert_eq!(sections.len(), 2);
        assert_eq!(sections[0].title, "Project skills");
        assert_eq!(sections[0].rows[0].name, "alpha");
        assert_eq!(sections[1].title, "User skills");
        assert_eq!(sections[1].rows[0].when_to_use.as_deref(), Some("beta"));
        fs::remove_dir_all(root).ok();
    }

    #[test]
    fn roots_variant_lists_managed_project_user_then_additional_skills() {
        let root = tmp_root("roots");
        let repo = root.join("repo");
        let cwd = repo.join("nested");
        let home = root.join("home").join(".lingxi");
        let managed = root.join("managed");
        let additional = root.join("additional-skills");
        fs::create_dir_all(repo.join(".git")).expect("create git marker");
        fs::create_dir_all(&cwd).expect("create cwd");
        fs::create_dir_all(&home).expect("create home");

        write_skill(
            &managed.join(".lingxi").join("skills"),
            "managed",
            "managed skill",
            None,
        );
        write_skill(
            &cwd.join(".lingxi").join("skills"),
            "project",
            "project skill",
            None,
        );
        write_skill(&home.join("skills"), "user", "user skill", None);
        write_skill(&additional, "extra", "extra skill", None);

        let sections = load_file_skill_sections_with_roots(
            &cwd,
            &home,
            Some(&managed),
            std::slice::from_ref(&additional),
        );
        let titles: Vec<&str> = sections.iter().map(|s| s.title.as_str()).collect();
        assert_eq!(
            titles,
            vec![
                "Managed skills",
                "Project skills",
                "User skills",
                "Additional skills"
            ]
        );
        assert_eq!(sections[0].rows[0].name, "managed");
        assert_eq!(sections[3].rows[0].name, "extra");
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

        assert!(load_file_skill_sections(&cwd, &home).is_empty());
        fs::remove_dir_all(root).ok();
    }
}
