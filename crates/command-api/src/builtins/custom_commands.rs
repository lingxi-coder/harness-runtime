//! Filesystem discovery + registration of custom markdown slash commands
//! (`.lingxi/commands/**.md`).
//!
//! The discovery half of this feature already lives — fully ported and tested —
//! in [`crate::markdown_loader`]
//! ([`load_command_markdown_files`](crate::markdown_loader::load_command_markdown_files),
//! [`build_markdown_command`](crate::markdown_loader::build_markdown_command),
//! the namespacing / inode-dedup / worktree-fallback helpers). It was never
//! wired into a [`CommandRegistry`], so custom commands could be *loaded* but
//! never *resolved* alongside the builtins. This module supplies the missing
//! glue: walk the managed/user/project dirs, build each markdown command, and
//! register it.
//!
//! ## Collision / precedence (claude-code parity)
//!
//! Two distinct dedup layers, both ported faithfully from claude-code:
//!
//! 1. **Same physical file, two paths** (e.g. a symlinked dir, or a hardlink
//!    into the managed dir). Resolved *inside the loader* by `(dev, ino)`
//!    identity with `managed > user > project` priority
//!    (`utils/markdownConfigLoader.ts` `loadMarkdownFilesForSubdir`, the
//!    `seenFileIds` loop over `[...managedFiles, ...userFiles, ...projectFiles]`).
//!    The loader already returns a deduplicated list, so we never see the
//!    duplicate here.
//!
//! 2. **Different files, same command name** (e.g. a user `foo.md` and a
//!    project `foo.md`). claude-code does *not* dedup these in the loader; the
//!    list keeps both and resolution is **first-wins** via
//!    `commands.ts::findCommand` (`commands.find(_ => _.name === name …)`) over
//!    the assembled list, where the legacy `/commands/` entries appear in
//!    `[managed, user, project]` order (`commands.ts::loadAllCommands` →
//!    `skillDirCommands`). First-wins over that order means
//!    **managed > user > project** (a managed `foo` shadows a user `foo` shadows
//!    a project `foo`). We reproduce that here explicitly: the loader hands back
//!    `[managed, user, project]` order, and we register only the *first*
//!    occurrence of each name (a `HashSet` guard), because
//!    [`CommandRegistry::register_command`] is last-wins and would otherwise let
//!    the lowest-priority (project) copy clobber the highest (managed).
//!
//! Custom commands are registered *after* the builtins, so a custom command
//! whose name equals a builtin's **overrides** the builtin in `resolve()` — this
//! also matches claude-code, where `findCommand` scans `skillDirCommands`
//! (custom) *before* `COMMANDS()` (builtins) and takes the first match. The
//! builtin handler entry is left in place (`register_command` only touches the
//! `commands` map, not `builtin_handlers`); the registered [`SlashCommand`] is of
//! kind `Markdown`, so the dispatcher routes it through the markdown-expansion
//! path by kind rather than through the shadowed builtin handler.

use crate::markdown_loader::{
    build_markdown_command, build_skill_command, load_command_markdown_files,
    load_managed_command_markdown_files, load_managed_skill_markdown_files,
    load_skill_markdown_files_with_roots,
};
use crate::CommandRegistry;
use std::collections::HashSet;
use std::path::{Path, PathBuf};

/// Discover every `.lingxi/commands/**.md` custom command reachable from `cwd`
/// (project dirs up to the git root / `home`, plus the user and managed layers)
/// and register each one into `reg` so it becomes resolvable and listable
/// exactly like a builtin.
///
/// Returns the number of custom commands registered (distinct names).
///
/// # Parameters
///
/// The directory parameters mirror
/// [`load_command_markdown_files`](crate::markdown_loader::load_command_markdown_files)
/// one-for-one (the loader is the source of truth for the discovery surface, and
/// taking the dirs explicitly keeps this function host-agnostic and unit
/// testable — the composition root supplies the real
/// `getClaudeConfigHomeDir()` / `getManagedFilePath()` / `homedir()` paths):
///
/// * `reg` — the registry to populate.
/// * `cwd` — session working directory (drives the project upward walk).
/// * `lingxi_home` — user config dir; the user layer is `lingxi_home/commands`.
/// * `managed_dir` — managed-policy root; the managed layer is
///   `managed_dir/.lingxi/commands`.
/// * `home` — the user's home directory, the upward-walk stop boundary.
///
/// # Async
///
/// `async` solely because the underlying loader is `async fn` (its body is
/// blocking `std::fs`, but the signature is part of the locked
/// [`command_api`] surface). No work is awaited beyond that single call.
///
/// # Idempotence
///
/// Re-running against the same registry + dirs is safe: each name is
/// re-inserted in-place (`HashMap::insert` semantics), yielding the same final
/// state.
pub async fn load_and_register_custom_commands(
    reg: &mut CommandRegistry,
    cwd: &Path,
    lingxi_home: &Path,
    managed_dir: &Path,
    home: &Path,
) -> usize {
    let files = load_command_markdown_files(cwd, lingxi_home, managed_dir, home).await;

    // First-wins over the loader's `[managed, user, project]` order (see the
    // module docs): `register_command` is last-wins, so we must skip a name once
    // a higher-priority layer has already claimed it.
    let mut seen: HashSet<String> = HashSet::new();
    let mut registered = 0usize;
    for file in &files {
        let command = build_markdown_command(file, file.source);
        if seen.insert(command.name.clone()) {
            reg.register_command(command);
            registered += 1;
        }
    }
    registered
}

/// Register only managed-policy commands. This path deliberately avoids
/// reading user/project roots while the strict plugin-only skills slot is set.
pub async fn load_and_register_managed_custom_commands(
    reg: &mut CommandRegistry,
    managed_dir: &Path,
) -> usize {
    let files = load_managed_command_markdown_files(managed_dir).await;
    let mut seen = HashSet::new();
    let mut registered = 0;
    for file in &files {
        let command = build_markdown_command(file, file.source);
        if seen.insert(command.name.clone()) {
            reg.register_command(command);
            registered += 1;
        }
    }
    registered
}

/// Discover every directory-format `.lingxi/skills/<name>/SKILL.md` command and
/// register it into `reg`.
pub async fn load_and_register_skill_commands(
    reg: &mut CommandRegistry,
    cwd: &Path,
    lingxi_home: &Path,
    home: &Path,
) -> usize {
    load_and_register_skill_commands_with_roots(reg, cwd, lingxi_home, None, home, &[]).await
}

/// Discover every directory-format `.lingxi/skills/<name>/SKILL.md` command
/// from managed, user, project, and additional skill directories and register it
/// into `reg`.
pub async fn load_and_register_skill_commands_with_roots(
    reg: &mut CommandRegistry,
    cwd: &Path,
    lingxi_home: &Path,
    managed_dir: Option<&Path>,
    home: &Path,
    additional_skill_dirs: &[PathBuf],
) -> usize {
    let files = load_skill_markdown_files_with_roots(
        cwd,
        lingxi_home,
        managed_dir,
        home,
        additional_skill_dirs,
    )
    .await;
    let mut seen: HashSet<String> = HashSet::new();
    let mut registered = 0usize;
    for file in &files {
        let command = build_skill_command(file, file.source);
        if seen.insert(command.name.clone()) {
            reg.register_command(command);
            registered += 1;
        }
    }
    registered
}

/// Register only managed-policy skills without probing ambient roots.
pub async fn load_and_register_managed_skill_commands(
    reg: &mut CommandRegistry,
    managed_dir: &Path,
) -> usize {
    let files = load_managed_skill_markdown_files(managed_dir).await;
    let mut seen = HashSet::new();
    let mut registered = 0;
    for file in &files {
        let command = build_skill_command(file, file.source);
        if seen.insert(command.name.clone()) {
            reg.register_command(command);
            registered += 1;
        }
    }
    registered
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::model::SlashCommandKind;
    use std::fs;
    use std::path::PathBuf;

    /// Unique temp dir under the OS temp root (mirrors the loader's own test
    /// helper so these stay self-contained and parallel-safe).
    fn temp_dir(tag: &str) -> PathBuf {
        let base = std::env::temp_dir().join(format!(
            "cmdcore-customcmd-{tag}-{}-{}",
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

    /// A tempdir `.lingxi/commands/foo.md` becomes resolvable as `/foo` after
    /// registration, as a `Markdown`-kind command carrying the file body.
    #[tokio::test]
    async fn registers_project_command_so_it_resolves() {
        let root = temp_dir("resolve");
        let home = root.join("home");
        fs::create_dir_all(&home).unwrap();
        let lingxi_home = home.join(".lingxi");
        let managed = root.join("managed-none");

        let project = root.join("proj");
        let cmds = project.join(".lingxi").join("commands");
        write(&cmds.join("foo.md"), "# Foo\n\nDo the foo with $1");
        // A namespaced one too: `sub/bar.md` -> `/sub:bar`.
        write(&cmds.join("sub").join("bar.md"), "Bar body");

        let mut reg = CommandRegistry::new();
        let n =
            load_and_register_custom_commands(&mut reg, &project, &lingxi_home, &managed, &home)
                .await;
        assert_eq!(n, 2, "two custom commands registered");

        let foo = reg.resolve("foo").expect("/foo should resolve");
        // Description strips the leading `#` header; the prompt template keeps the
        // full markdown body verbatim (header line included).
        assert_eq!(foo.description, "Foo");
        match &foo.kind {
            SlashCommandKind::Markdown {
                prompt_template, ..
            } => assert_eq!(prompt_template, "# Foo\n\nDo the foo with $1"),
            other => panic!("expected Markdown kind, got {other:?}"),
        }

        assert!(
            reg.resolve("sub:bar").is_some(),
            "namespaced /sub:bar should resolve"
        );

        fs::remove_dir_all(&root).ok();
    }

    #[tokio::test]
    async fn registers_project_skill_so_it_resolves_with_skill_metadata() {
        let root = temp_dir("skill-resolve");
        let home = root.join("home");
        fs::create_dir_all(&home).unwrap();
        let lingxi_home = home.join(".lingxi");
        let project = root.join("proj");
        fs::create_dir_all(project.join(".git")).unwrap();
        let skill_dir = project.join(".lingxi").join("skills").join("demo");
        write(
            &skill_dir.join("SKILL.md"),
            "---\ndescription: Demo skill\n---\nUse this skill\n",
        );

        let mut reg = CommandRegistry::new();
        let n = load_and_register_skill_commands(&mut reg, &project, &lingxi_home, &home).await;
        assert_eq!(n, 1);

        let cmd = reg.resolve("demo").expect("/demo should resolve");
        assert_eq!(cmd.loaded_from.as_deref(), Some("skills"));
        assert_eq!(cmd.skill_root.as_deref(), Some(skill_dir.as_path()));
        assert_eq!(cmd.description, "Demo skill");
        assert!(matches!(cmd.kind, SlashCommandKind::Markdown { .. }));

        fs::remove_dir_all(&root).ok();
    }

    #[tokio::test]
    async fn user_skill_overrides_same_named_project_skill() {
        let root = temp_dir("skill-collide");
        let home = root.join("home");
        fs::create_dir_all(&home).unwrap();
        let lingxi_home = home.join(".lingxi");
        let project = root.join("proj");
        fs::create_dir_all(project.join(".git")).unwrap();
        let user_skill_dir = lingxi_home.join("skills").join("dup");
        let project_skill_dir = project.join(".lingxi").join("skills").join("dup");
        write(
            &user_skill_dir.join("SKILL.md"),
            "---\ndescription: User skill\n---\nUSER body\n",
        );
        write(
            &project_skill_dir.join("SKILL.md"),
            "---\ndescription: Project skill\n---\nPROJECT body\n",
        );

        let mut reg = CommandRegistry::new();
        let n = load_and_register_skill_commands(&mut reg, &project, &lingxi_home, &home).await;
        assert_eq!(n, 1);

        let cmd = reg.resolve("dup").expect("/dup should resolve");
        assert_eq!(cmd.description, "User skill");
        assert_eq!(cmd.skill_root.as_deref(), Some(user_skill_dir.as_path()));
        match &cmd.kind {
            SlashCommandKind::Markdown {
                prompt_template, ..
            } => assert_eq!(prompt_template, "USER body\n"),
            other => panic!("expected Markdown kind, got {other:?}"),
        }

        fs::remove_dir_all(&root).ok();
    }

    #[tokio::test]
    async fn managed_skill_overrides_same_named_user_project_and_additional_skill() {
        let root = temp_dir("skill-managed-collide");
        let home = root.join("home");
        fs::create_dir_all(&home).unwrap();
        let lingxi_home = home.join(".lingxi");
        let managed = root.join("managed");
        let additional = root.join("additional-skills");
        let project = root.join("proj");
        fs::create_dir_all(project.join(".git")).unwrap();

        let managed_skill_dir = managed.join(".lingxi").join("skills").join("dup");
        write(
            &managed_skill_dir.join("SKILL.md"),
            "---\ndescription: Managed skill\n---\nMANAGED body\n",
        );
        write(
            &lingxi_home.join("skills").join("dup").join("SKILL.md"),
            "---\ndescription: User skill\n---\nUSER body\n",
        );
        write(
            &project
                .join(".lingxi")
                .join("skills")
                .join("dup")
                .join("SKILL.md"),
            "---\ndescription: Project skill\n---\nPROJECT body\n",
        );
        write(
            &additional.join("dup").join("SKILL.md"),
            "---\ndescription: Additional skill\n---\nADDITIONAL body\n",
        );

        let mut reg = CommandRegistry::new();
        let n = load_and_register_skill_commands_with_roots(
            &mut reg,
            &project,
            &lingxi_home,
            Some(&managed),
            &home,
            std::slice::from_ref(&additional),
        )
        .await;
        assert_eq!(n, 1);

        let cmd = reg.resolve("dup").expect("/dup should resolve");
        assert_eq!(
            cmd.source,
            crate::CommandSource::Settings(lingxi_core::types::SettingsScope::Managed)
        );
        assert_eq!(cmd.description, "Managed skill");
        assert_eq!(cmd.skill_root.as_deref(), Some(managed_skill_dir.as_path()));
        match &cmd.kind {
            SlashCommandKind::Markdown {
                prompt_template, ..
            } => assert_eq!(prompt_template, "MANAGED body\n"),
            other => panic!("expected Markdown kind, got {other:?}"),
        }

        fs::remove_dir_all(&root).ok();
    }

    /// Project beats... no — claude-code first-wins over `[managed, user,
    /// project]` makes the **user** copy win over a same-named **project** copy.
    /// Two *different* files named `dup.md` (user + project): the user body must
    /// be the one that resolves.
    #[tokio::test]
    async fn user_overrides_project_on_name_collision() {
        let root = temp_dir("collide");
        let home = root.join("home");
        fs::create_dir_all(&home).unwrap();
        let lingxi_home = home.join(".lingxi");
        let managed = root.join("managed-none");

        // User layer: lingxi_home/commands/dup.md
        write(&lingxi_home.join("commands").join("dup.md"), "USER body");
        // Project layer: <proj>/.lingxi/commands/dup.md
        let project = root.join("proj");
        write(
            &project.join(".lingxi").join("commands").join("dup.md"),
            "PROJECT body",
        );

        let mut reg = CommandRegistry::new();
        load_and_register_custom_commands(&mut reg, &project, &lingxi_home, &managed, &home).await;

        let dup = reg.resolve("dup").expect("/dup should resolve");
        match &dup.kind {
            SlashCommandKind::Markdown {
                prompt_template, ..
            } => assert_eq!(
                prompt_template, "USER body",
                "first-wins over [managed,user,project] => user wins over project"
            ),
            other => panic!("expected Markdown kind, got {other:?}"),
        }

        fs::remove_dir_all(&root).ok();
    }

    /// A custom command whose name equals a builtin's overrides the builtin in
    /// `resolve()` (claude-code scans custom before builtins, first-wins).
    #[tokio::test]
    async fn custom_command_overrides_builtin_name() {
        let root = temp_dir("override");
        let home = root.join("home");
        fs::create_dir_all(&home).unwrap();
        let lingxi_home = home.join(".lingxi");
        let managed = root.join("managed-none");

        let project = root.join("proj");
        // `commit` is a real builtin name; a custom `commit.md` must shadow it.
        write(
            &project.join(".lingxi").join("commands").join("commit.md"),
            "# Custom commit\n\ncustom commit body",
        );

        let mut reg = CommandRegistry::new();
        crate::builtins::register_all_builtin_commands(&mut reg);
        assert!(
            matches!(
                reg.resolve("commit").map(|c| &c.kind),
                Some(SlashCommandKind::Builtin { .. })
            ),
            "precondition: /commit is a builtin before custom registration"
        );

        load_and_register_custom_commands(&mut reg, &project, &lingxi_home, &managed, &home).await;

        let commit = reg.resolve("commit").expect("/commit still resolves");
        assert!(
            matches!(commit.kind, SlashCommandKind::Markdown { .. }),
            "custom /commit must shadow the builtin (Markdown kind)"
        );

        fs::remove_dir_all(&root).ok();
    }

    /// Empty / missing command dirs => no-op (no commands registered, no panic).
    #[tokio::test]
    async fn empty_dirs_are_a_noop() {
        let root = temp_dir("empty");
        let home = root.join("home");
        fs::create_dir_all(&home).unwrap();
        let lingxi_home = home.join(".lingxi");
        let managed = root.join("managed-none");
        let project = root.join("noproj");

        let mut reg = CommandRegistry::new();
        let n =
            load_and_register_custom_commands(&mut reg, &project, &lingxi_home, &managed, &home)
                .await;
        assert_eq!(n, 0, "no custom commands discovered");
        assert!(reg.resolve("anything").is_none());

        fs::remove_dir_all(&root).ok();
    }
}
