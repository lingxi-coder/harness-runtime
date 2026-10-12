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
use crate::{CommandRegistry, SlashCommand};
use lingxi_core::types::utf16_json::Utf16JsonProjection;
use std::collections::{HashMap, HashSet};
use std::path::{Component, Path, PathBuf};

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
    let mut seen: HashSet<Vec<u16>> = HashSet::new();
    let mut registered = 0usize;
    for file in &files {
        let command = build_markdown_command(file, file.source);
        if seen.insert(command.name.string_units("").unwrap_or_default()) {
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
        if seen.insert(command.name.string_units("").unwrap_or_default()) {
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
    let mut duplicate_counts: HashMap<Vec<u16>, usize> = HashMap::new();
    let mut commands = Vec::with_capacity(files.len());
    for file in &files {
        let command = build_skill_command(file, file.source);
        if let Some(name) = command.name.string_units("") {
            *duplicate_counts.entry(name).or_default() += 1;
        }
        commands.push(command);
    }

    // Native Ybo/Qbo keeps the original skill name when it has no competing
    // command. For collisions, a skill rooted below the current project gets
    // a path scope (`relative-root/name`); a competing unscoped skill is
    // dropped. The full existing registry is the closest Harness projection
    // of Native's already-loaded command catalog.
    let mut occupied: HashSet<Vec<u16>> = reg
        .registration_ordered_commands()
        .into_iter()
        .flat_map(|command| {
            std::iter::once(command.name.string_units("").unwrap_or_default()).chain(
                command
                    .aliases
                    .iter()
                    .filter_map(|alias| alias.string_units("")),
            )
        })
        .collect();
    let mut registered = 0usize;
    for (file, mut command) in files.iter().zip(commands) {
        let Some(name_units) = command.name.string_units("") else {
            continue;
        };
        if name_units.is_empty() {
            continue;
        }
        let was_conflicting =
            occupied.contains(&name_units) || reg.resolve(&command.name).is_some();
        let duplicated = duplicate_counts
            .get(&name_units)
            .copied()
            .unwrap_or_default()
            > 1;
        let scope = native_additional_skill_scope(cwd, &file.skill_root)
            .filter(|scope| scope_can_be_native_namespace(scope, &name_units));

        if !was_conflicting && !duplicated {
            if let Some(scope) = scope {
                append_unscoped_root_description(&mut command, &scope);
            }
            occupied.insert(name_units);
            reg.register_command(command);
            registered += 1;
            continue;
        }

        let Some(scope) = scope else {
            // Native keeps the first member of an unscoped same-name group,
            // but drops a name that already belongs to the prior catalog.
            if was_conflicting {
                continue;
            }
            occupied.insert(name_units);
            reg.register_command(command);
            registered += 1;
            continue;
        };

        let original_name = command.name.clone();
        let Some(original_name_text) = original_name.string_at("") else {
            continue;
        };
        let scoped_name = format!("{scope}:{original_name_text}");
        let scoped_projection: Utf16JsonProjection = scoped_name.into();
        let Some(scoped_units) = scoped_projection.string_units("") else {
            continue;
        };
        if occupied.contains(&scoped_units) || reg.resolve(&scoped_projection).is_some() {
            continue;
        }

        append_scoped_root_description(&mut command, &scope, original_name_text, was_conflicting);
        command.name = scoped_projection.clone();
        command.unqualified_name = Some(original_name);
        command.user_facing_name = Some(scoped_projection);
        command.aliases.clear();
        occupied.insert(scoped_units);
        reg.register_command(command);
        registered += 1;
    }
    registered
}

/// Native Xbo derives a scope from the directory before `.claude/skills` and
/// accepts it only when that directory is a non-empty descendant of the
/// session project root. Harness's branded equivalent is
/// `<root>/<DOT_DIR>/skills`; only additional roots nested below `cwd` can
/// produce a scope because the ordinary project walk visits `cwd` and its
/// ancestors.
fn native_additional_skill_scope(cwd: &Path, skill_root: &Path) -> Option<String> {
    let skills_dir = skill_root.parent()?;
    if skills_dir.file_name()? != "skills" {
        return None;
    }
    let config_dir = skills_dir.parent()?;
    if config_dir.file_name()? != branding::DOT_DIR {
        return None;
    }
    let root = config_dir.parent()?;
    let project_root = normalize_lexical_path(cwd);
    let resolved_root = if root.is_absolute() {
        root.to_path_buf()
    } else {
        project_root.join(root)
    };
    let root = normalize_lexical_path(&resolved_root);
    let relative = root.strip_prefix(&project_root).ok()?;
    let mut components = Vec::new();
    for component in relative.components() {
        let Component::Normal(value) = component else {
            return None;
        };
        components.push(value.to_str()?.to_string());
    }
    if components.is_empty() {
        return None;
    }
    Some(components.join("/"))
}

fn normalize_lexical_path(path: &Path) -> PathBuf {
    let mut normalized = PathBuf::new();
    for component in path.components() {
        match component {
            Component::CurDir => {}
            Component::ParentDir => {
                if normalized.file_name().is_some() {
                    normalized.pop();
                } else if !normalized.is_absolute() {
                    normalized.push(component.as_os_str());
                }
            }
            Component::Prefix(_) | Component::RootDir | Component::Normal(_) => {
                normalized.push(component.as_os_str());
            }
        }
    }
    normalized
}

fn scope_can_be_native_namespace(scope: &str, original_name: &[u16]) -> bool {
    let Ok(original_name) = String::from_utf16(original_name) else {
        return true;
    };
    let namespaced = format!("{scope}:{original_name}");
    // Native qCe defaults on and rejects a scoped spelling that would enter
    // the account-synced `anthropic-skills:` namespace. Keep the exact ASCII
    // reserved prefix here; Native's additional Unicode-confusable matcher is
    // outside this local filesystem-name projection.
    !telemetry::flag_bool("tengu_plaid_harbor", true)
        || !namespaced.starts_with("anthropic-skills:")
}

fn append_unscoped_root_description(command: &mut SlashCommand, scope: &str) {
    let description = command.description.string_at("").unwrap_or_default();
    command.description = format!(
        "{description} (from {scope}/{}/skills — applies when working on files under {scope}/)",
        branding::DOT_DIR
    )
    .into();
}

fn append_scoped_root_description(
    command: &mut SlashCommand,
    scope: &str,
    original_name: &str,
    already_conflicting: bool,
) {
    let description = command.description.string_at("").unwrap_or_default();
    let note = if already_conflicting {
        format!(
            "scoped to {scope}/ — use this instead of the unscoped \"{original_name}\" skill when the files being changed are under {scope}/"
        )
    } else {
        format!(
            "from {scope}/{}/skills — applies when working on files under {scope}/",
            branding::DOT_DIR
        )
    };
    command.description = format!("{description} ({note})").into();
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
        if seen.insert(command.name.string_units("").unwrap_or_default()) {
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

        let foo = reg.resolve(&"foo".into()).expect("/foo should resolve");
        // Description strips the leading `#` header; the prompt template keeps the
        // full markdown body verbatim (header line included).
        assert_eq!(foo.description.string_at(""), Some("Foo"));
        match &foo.kind {
            SlashCommandKind::Markdown {
                prompt_template, ..
            } => assert_eq!(
                prompt_template.string_at(""),
                Some("# Foo\n\nDo the foo with $1")
            ),
            other => panic!("expected Markdown kind, got {other:?}"),
        }

        assert!(
            reg.resolve(&"sub:bar".into()).is_some(),
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

        let cmd = reg.resolve(&"demo".into()).expect("/demo should resolve");
        assert_eq!(cmd.loaded_from.as_deref(), Some("skills"));
        assert_eq!(cmd.skill_root.as_deref(), Some(skill_dir.as_path()));
        assert_eq!(cmd.description.string_at(""), Some("Demo skill"));
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

        let cmd = reg.resolve(&"dup".into()).expect("/dup should resolve");
        assert_eq!(cmd.description.string_at(""), Some("User skill"));
        assert_eq!(cmd.skill_root.as_deref(), Some(user_skill_dir.as_path()));
        match &cmd.kind {
            SlashCommandKind::Markdown {
                prompt_template, ..
            } => assert_eq!(prompt_template.string_at(""), Some("USER body\n")),
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

        let cmd = reg.resolve(&"dup".into()).expect("/dup should resolve");
        assert_eq!(
            cmd.source,
            crate::CommandSource::Settings(lingxi_core::types::SettingsScope::Managed)
        );
        assert_eq!(cmd.description.string_at(""), Some("Managed skill"));
        assert_eq!(cmd.skill_root.as_deref(), Some(managed_skill_dir.as_path()));
        match &cmd.kind {
            SlashCommandKind::Markdown {
                prompt_template, ..
            } => assert_eq!(prompt_template.string_at(""), Some("MANAGED body\n")),
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

        let dup = reg.resolve(&"dup".into()).expect("/dup should resolve");
        match &dup.kind {
            SlashCommandKind::Markdown {
                prompt_template, ..
            } => assert_eq!(
                prompt_template.string_at(""),
                Some("USER body"),
                "first-wins over [managed,user,project] => user wins over project"
            ),
            other => panic!("expected Markdown kind, got {other:?}"),
        }

        fs::remove_dir_all(&root).ok();
    }

    /// A custom command whose name equals a builtin's overrides the builtin in
    /// `resolve()` (claude-code scans custom before builtins, first-wins).
    #[tokio::test]
    async fn custom_command_overrides_bundled_prompt_name() {
        let root = temp_dir("override");
        let home = root.join("home");
        fs::create_dir_all(&home).unwrap();
        let lingxi_home = home.join(".lingxi");
        let managed = root.join("managed-none");

        let project = root.join("proj");
        // `/commit` is a Native bundled prompt; a custom `commit.md` must shadow it.
        write(
            &project.join(".lingxi").join("commands").join("commit.md"),
            "# Custom commit\n\ncustom commit body",
        );

        let mut reg = CommandRegistry::new();
        crate::builtins::register_all_builtin_commands(&mut reg);
        crate::builtins::register_bundled_skills(&mut reg, true);
        let builtin_commit = reg
            .resolve(&"commit".into())
            .expect("Native /commit bundled prompt is registered");
        assert!(
            matches!(&builtin_commit.kind, SlashCommandKind::Bundled { .. }),
            "precondition: /commit is a bundled prompt before custom registration"
        );

        load_and_register_custom_commands(&mut reg, &project, &lingxi_home, &managed, &home).await;

        let commit = reg
            .resolve(&"commit".into())
            .expect("/commit still resolves");
        assert!(
            matches!(commit.kind, SlashCommandKind::Markdown { .. }),
            "custom /commit must shadow the builtin (Markdown kind)"
        );

        fs::remove_dir_all(&root).ok();
    }

    #[tokio::test]
    async fn nested_additional_skill_collision_uses_native_scoped_name() {
        let root = temp_dir("native-scope");
        let home = root.join("home");
        let project = root.join("project");
        let lingxi_home = home.join(".lingxi");
        let nested = project.join("packages").join("api");
        let outside = root.join("outside");
        let project_skill = project
            .join(branding::DOT_DIR)
            .join("skills")
            .join("deploy");
        let nested_skill = nested.join(branding::DOT_DIR).join("skills").join("deploy");
        let nested_noncolliding_skill =
            nested.join(branding::DOT_DIR).join("skills").join("polish");
        let outside_skill = outside
            .join(branding::DOT_DIR)
            .join("skills")
            .join("deploy");
        write(
            &project_skill.join("SKILL.md"),
            "---\ndescription: Project deploy\nname: friendly deploy\n---\nproject body\n",
        );
        write(
            &nested_skill.join("SKILL.md"),
            "---\ndescription: Nested deploy\nname: Nested display\n---\nnested body\n",
        );
        write(
            &nested_noncolliding_skill.join("SKILL.md"),
            "---\ndescription: Nested polish\nname: Polish display\n---\npolish body\n",
        );
        write(
            &outside_skill.join("SKILL.md"),
            "---\ndescription: Outside deploy\n---\noutside body\n",
        );

        let mut registry = CommandRegistry::new();
        let registered = load_and_register_skill_commands_with_roots(
            &mut registry,
            &project,
            &lingxi_home,
            None,
            &home,
            &[
                nested.join(branding::DOT_DIR).join("skills"),
                outside.join(branding::DOT_DIR).join("skills"),
            ],
        )
        .await;

        assert_eq!(registered, 3);
        assert!(registry.resolve(&"deploy".into()).is_some());
        let scoped = registry
            .resolve(&"packages/api:deploy".into())
            .expect("a colliding nested add-dir skill gets the Native Qbo scope");
        assert_eq!(scoped.name.string_at(""), Some("packages/api:deploy"));
        assert_eq!(
            scoped
                .unqualified_name
                .as_ref()
                .and_then(|name| name.string_at("")),
            Some("deploy")
        );
        assert_eq!(
            scoped
                .user_facing_name
                .as_ref()
                .and_then(|name| name.string_at("")),
            Some("packages/api:deploy"),
            "Qbo's display callback returns the scoped canonical spelling"
        );
        assert!(scoped.aliases.is_empty(), "Qbo clears aliases");
        assert!(scoped
            .description
            .string_at("")
            .unwrap_or_default()
            .contains("scoped to packages/api/"));
        assert!(registry.resolve(&"outside:deploy".into()).is_none());
        let nested_plain = registry
            .resolve(&"polish".into())
            .expect("a noncolliding nested skill keeps its unscoped canonical name");
        assert_eq!(
            nested_plain
                .user_facing_name
                .as_ref()
                .and_then(|name| name.string_at("")),
            Some("Polish display"),
            "GHt keeps the ordinary factory's frontmatter display name"
        );
        assert!(nested_plain
            .description
            .string_at("")
            .unwrap_or_default()
            .contains(&format!("from packages/api/{}/skills", branding::DOT_DIR)));

        fs::remove_dir_all(&root).ok();
    }

    #[test]
    fn only_nonempty_project_descendants_are_scope_candidates() {
        let root = temp_dir("native-scope-path");
        let skill_root = |base: &Path| base.join(branding::DOT_DIR).join("skills").join("skill");

        assert_eq!(
            native_additional_skill_scope(&root, &skill_root(&root.join("subproject").join("pkg"))),
            Some("subproject/pkg".to_string())
        );
        assert_eq!(
            native_additional_skill_scope(&root, &skill_root(&root)),
            None
        );
        assert_eq!(
            native_additional_skill_scope(&root, &skill_root(&root.with_file_name("outside"))),
            None,
            "an additional root outside projectRoot has no Native scope"
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
        assert!(reg.resolve(&"anything".into()).is_none());

        fs::remove_dir_all(&root).ok();
    }
}
