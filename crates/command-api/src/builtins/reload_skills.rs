//! `/reload-skills` — re-scan disk skill directories and refresh the live
//! command registry's skill-sourced entries.
//!
//! Ported from the claude-code TS `local` / `post-text` command
//! `reload-skills/index.ts` (binary v2.1.198: name registered as `Xtf` /
//! `gur`, handler body `Qtf` in module `zrc`):
//!
//! ```js
//! Jtf = async (e, t) => {
//!   let n = Lt(), r = await ZR(n), o = new Set(r.map((p) => p.name));
//!   GI(), Bj();
//!   let s = await ZR(n), i = new Set(s.map((p) => p.name));
//!   T2.emit();
//!   a = Un(s, (p) => !o.has(p.name)),
//!   l = Un(r, (p) => !i.has(p.name)),
//!   c = [];
//!   if (a > 0) c.push(`${a} added`);
//!   if (l > 0) c.push(`${l} removed`);
//!   let u = c.length > 0 ? c.join(", ") : "no changes",
//!       d = Ql() ? " (custom skills are disabled in safe mode)" : "";
//!   return { type: "text", value: `Reloaded skills: ${s.length} ${un(s.length,"skill")} available (${u})${d}` };
//! };
//! ```
//!
//! `ZR(n)` reads the current *live* skill-command list (cached), `GI()`
//! invalidates that cache, and `Bj()` re-populates it from disk — the second
//! `ZR(n)` call then reflects the fresh on-disk state. `Un` is a count-matching
//! filter (added = names in the new set absent from the old one; removed =
//! names in the old set absent from the new one), `un` is the singular/plural
//! word helper, and `Ql()` is the CLI `--safe-mode` flag.
//!
//! ## Port mapping
//!
//! LingXi has no separate skill-list cache to invalidate: the skill loader
//! ([`crate::builtins::custom_commands::load_and_register_skill_commands_with_roots`])
//! already re-walks the filesystem on every call, so there is no `GI()`
//! equivalent — this handler goes straight from "read the registry" to
//! "reload it". The before/after snapshots instead come from the live
//! [`crate::CommandRegistry`] itself, filtered to entries whose
//! [`SlashCommand::loaded_from`](crate::model::SlashCommand::loaded_from)
//! is `Some("skills")` (set by
//! [`crate::markdown_loader::build_skill_command`]):
//!
//! 1. Read-lock the registry and collect `before` = skill-loaded names.
//! 2. Write-lock the registry and call
//!    [`load_and_register_skill_commands_with_roots`](crate::builtins::custom_commands::load_and_register_skill_commands_with_roots)
//!    with the handler's roots (mirrors [`crate::builtins::skills::SkillsHandler`]'s
//!    `cwd` / `lingxi_home` / `managed_dir` / `additional_skill_dirs`, plus the
//!    `home` upward-walk boundary the loader itself requires).
//! 3. Read-lock again and collect `after` = skill-loaded names.
//! 4. `added = |after − before|`, `removed = |before − after|`.
//! 5. Render the locked message via [`format_reload_message`].
//!
//! The registry's skill partition is cleared before the re-scan. This is what
//! makes removals observable and keeps completion/model snapshots from
//! retaining deleted skill files.

use crate::model::{BuiltinCommandHandler, CommandResult};
use crate::parser::ParsedSlashCommand;
use crate::CommandRegistry;
use async_trait::async_trait;
use std::collections::HashSet;
use std::path::PathBuf;
use std::sync::Arc;
use tokio::sync::RwLock;

type PostReloadFinalizer = Arc<dyn Fn(&mut CommandRegistry) + Send + Sync>;

/// Verbatim TS metadata (`reload-skills/index.ts`, v2.1.198).
const DESCRIPTION: &str = "Pick up skills added or changed on disk during this session";

/// Filesystem roots the skill loader re-scans on every invocation. Mirrors
/// [`crate::builtins::skills::SkillsHandler`]'s `SkillsRoots`, plus `home` — the
/// upward-walk boundary [`crate::markdown_loader::load_skill_markdown_files_with_roots`]
/// requires that the `/skills` viewer doesn't need (it only lists, it doesn't
/// call the registry-mutating loader).
#[derive(Debug, Clone, Default)]
struct ReloadSkillsRoots {
    cwd: PathBuf,
    lingxi_home: PathBuf,
    managed_dir: Option<PathBuf>,
    home: PathBuf,
    additional_skill_dirs: Vec<PathBuf>,
}

/// `/reload-skills` handler — a 1:1 port of `Jtf` (`reload-skills/index.ts`).
///
/// Holds a shared handle to the live command registry (the `plugin::manager`
/// pattern: `Arc<RwLock<CommandRegistry>>` mutated live from a command path)
/// so the reload actually changes what the rest of the session sees.
pub struct ReloadSkillsHandler {
    registry: Arc<RwLock<CommandRegistry>>,
    roots: ReloadSkillsRoots,
    /// `Ql()` — the CLI `--safe-mode` flag. When set, the skill partition stays
    /// empty: reload may invalidate stale entries but must not rediscover
    /// on-disk customization that boot intentionally disabled.
    safe_mode: bool,
    /// Mobile-only opt-in: also reload legacy `.lingxi/commands/*.md` entries
    /// inside the same locked mutation so decoy command names cannot surface
    /// between disk reload and bundled restoration.
    reload_legacy_commands: bool,
    /// Optional synchronous finalizer that runs while the registry write lock is
    /// still held, after disk-backed prompt commands have been reloaded.
    post_reload_finalizer: Option<PostReloadFinalizer>,
}

impl ReloadSkillsHandler {
    /// Construct a handler with default roots (current working directory /
    /// `$LINGXI_CONFIG_DIR` or `~/.claude` / no managed dir / no additional
    /// dirs / safe mode off). Mirrors [`crate::builtins::skills::SkillsHandler::new`].
    #[must_use]
    pub fn new(registry: Arc<RwLock<CommandRegistry>>) -> Self {
        Self {
            registry,
            roots: ReloadSkillsRoots {
                cwd: std::env::current_dir().unwrap_or_else(|_| PathBuf::from(".")),
                lingxi_home: lingxi_home_dir(),
                managed_dir: None,
                home: home_dir(),
                additional_skill_dirs: Vec::new(),
            },
            safe_mode: false,
            reload_legacy_commands: false,
            post_reload_finalizer: None,
        }
    }

    /// Construct a handler pinned to explicit roots, including the managed
    /// dir, additional skill dirs, and the safe-mode display gate. Mirrors
    /// [`crate::builtins::skills::SkillsHandler::with_all_roots`] with the extra `home`
    /// boundary the registry-mutating loader needs, and `safe_mode` for the
    /// `Ql()` note.
    #[must_use]
    pub fn with_all_roots(
        registry: Arc<RwLock<CommandRegistry>>,
        cwd: PathBuf,
        lingxi_home: PathBuf,
        managed_dir: Option<PathBuf>,
        home: PathBuf,
        additional_skill_dirs: Vec<PathBuf>,
        safe_mode: bool,
    ) -> Self {
        Self {
            registry,
            roots: ReloadSkillsRoots {
                cwd,
                lingxi_home,
                managed_dir,
                home,
                additional_skill_dirs,
            },
            safe_mode,
            reload_legacy_commands: false,
            post_reload_finalizer: None,
        }
    }

    /// Opt into reloading legacy `.lingxi/commands/*.md` entries and then run a
    /// synchronous finalizer before releasing the registry write lock.
    #[must_use]
    pub fn with_locked_post_reload_finalizer(
        mut self,
        reload_legacy_commands: bool,
        finalizer: PostReloadFinalizer,
    ) -> Self {
        self.reload_legacy_commands = reload_legacy_commands;
        self.post_reload_finalizer = Some(finalizer);
        self
    }
}

/// `$LINGXI_CONFIG_DIR` when set (claude-code `tr()` `??`: an empty value is
/// honored verbatim), else `$HOME/.claude` (`$USERPROFILE` fallback for
/// Windows) — byte-identical helper to [`crate::builtins::skills::lingxi_home_dir`]
/// (private to that module, so duplicated here per the existing per-module
/// convention).
fn lingxi_home_dir() -> PathBuf {
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

/// The bare home directory (upward-walk stop boundary for the project skill
/// layer), independent of `lingxi_home`'s `.lingxi`/`$LINGXI_CONFIG_DIR`
/// suffix. No `dirs` crate dependency in this crate, so resolved the same way
/// `lingxi_home_dir` falls back — from `$HOME` / `$USERPROFILE`.
fn home_dir() -> PathBuf {
    std::env::var_os("HOME")
        .or_else(|| std::env::var_os("USERPROFILE"))
        .map_or_else(|| PathBuf::from("."), PathBuf::from)
}

/// Snapshot the names of every registry entry loaded from the skills layer
/// (`SlashCommand.loaded_from == Some("skills")`) — the Rust analog of the TS
/// `new Set(r.map((p) => p.name))` over `ZR(n)`'s result.
fn skill_loaded_names(reg: &CommandRegistry) -> HashSet<String> {
    reg.list_all()
        .into_iter()
        .filter(|c| c.loaded_from.as_deref() == Some("skills"))
        .map(|c| c.name.clone())
        .collect()
}

/// Port of `Jtf`'s final template (v2.1.198):
/// `` `Reloaded skills: ${s.length} ${un(s.length,"skill")} available (${u})${d}` ``
/// where `u` is the added/removed summary (or `"no changes"`) and `d` is the
/// safe-mode note. `total` drives both the count and the `un()` pluralization
/// (singular only at exactly `1`).
fn format_reload_message(total: usize, added: usize, removed: usize, safe_mode: bool) -> String {
    let mut parts: Vec<String> = Vec::with_capacity(2);
    if added > 0 {
        parts.push(format!("{added} added"));
    }
    if removed > 0 {
        parts.push(format!("{removed} removed"));
    }
    let changes = if parts.is_empty() {
        "no changes".to_string()
    } else {
        parts.join(", ")
    };
    let word = if total == 1 { "skill" } else { "skills" };
    let safe_suffix = if safe_mode {
        " (custom skills are disabled in safe mode)"
    } else {
        ""
    };
    format!("Reloaded skills: {total} {word} available ({changes}){safe_suffix}")
}

#[async_trait]
impl BuiltinCommandHandler for ReloadSkillsHandler {
    async fn handle(&self, _args: &ParsedSlashCommand) -> CommandResult {
        // `r = await ZR(n)`, `o = new Set(r.map(name))` — snapshot before reload.
        let before = {
            let reg = self.registry.read().await;
            skill_loaded_names(&reg)
        };

        // `GI(); Bj();` — invalidate the live skill partition, then re-walk
        // disk and repopulate it.
        {
            let mut reg = self.registry.write().await;
            reg.unregister_loaded_from("skills");
            if self.reload_legacy_commands {
                reg.unregister_loaded_from("commands_DEPRECATED");
            }
            if !self.safe_mode {
                if self.reload_legacy_commands {
                    let no_managed = self
                        .roots
                        .lingxi_home
                        .join("__lingxi_no_managed_settings__");
                    let managed_dir = self.roots.managed_dir.as_deref().unwrap_or(&no_managed);
                    crate::builtins::custom_commands::load_and_register_custom_commands(
                        &mut reg,
                        &self.roots.cwd,
                        &self.roots.lingxi_home,
                        managed_dir,
                        &self.roots.home,
                    )
                    .await;
                }
                crate::builtins::custom_commands::load_and_register_skill_commands_with_roots(
                    &mut reg,
                    &self.roots.cwd,
                    &self.roots.lingxi_home,
                    self.roots.managed_dir.as_deref(),
                    &self.roots.home,
                    &self.roots.additional_skill_dirs,
                )
                .await;
            }
            if let Some(finalizer) = &self.post_reload_finalizer {
                finalizer(&mut reg);
            }
        }

        // `s = await ZR(n)`, `i = new Set(s.map(name))` — snapshot after reload.
        let after = {
            let reg = self.registry.read().await;
            skill_loaded_names(&reg)
        };

        // `a = Un(s, (p) => !o.has(p.name))`, `l = Un(r, (p) => !i.has(p.name))`.
        let added = after.difference(&before).count();
        let removed = before.difference(&after).count();

        CommandResult::Done {
            display: Some(format_reload_message(
                after.len(),
                added,
                removed,
                self.safe_mode,
            )),
        }
    }

    fn name(&self) -> &str {
        "reload-skills"
    }

    fn description(&self) -> &str {
        DESCRIPTION
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::model::{CommandFrontmatter, CommandSource, SlashCommand, SlashCommandKind};
    use std::fs;
    use std::sync::atomic::{AtomicBool, Ordering};
    use std::sync::{Condvar, Mutex};
    use std::time::Duration;

    fn tmp_root(name: &str) -> PathBuf {
        let nanos = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .expect("clock before epoch")
            .as_nanos();
        let dir = std::env::temp_dir().join(format!(
            "lingxi-reload-skills-{name}-{}-{nanos}",
            std::process::id()
        ));
        fs::create_dir_all(&dir).expect("create temp root");
        dir
    }

    fn write_skill(base: &std::path::Path, name: &str, description: &str) {
        let dir = base.join(name);
        fs::create_dir_all(&dir).expect("create skill dir");
        fs::write(
            dir.join("SKILL.md"),
            format!("---\ndescription: {description}\n---\nBody\n"),
        )
        .expect("write skill");
    }

    fn args() -> ParsedSlashCommand {
        ParsedSlashCommand {
            name: "reload-skills".to_string(),
            raw_args: String::new(),
            positional_args: Vec::new(),
        }
    }

    fn bundled_cmd(name: &str) -> SlashCommand {
        SlashCommand {
            name: name.to_string(),
            description: format!("{name} bundled"),
            source: CommandSource::Bundled,
            kind: SlashCommandKind::Bundled {
                frontmatter: CommandFrontmatter::default(),
                prompt_fn: None,
            },
            loaded_from: Some("bundled".into()),
            ..SlashCommand::default()
        }
    }

    async fn run(h: &ReloadSkillsHandler) -> String {
        match h.handle(&args()).await {
            CommandResult::Done { display: Some(s) } => s,
            other => panic!("expected Done with display, got {other:?}"),
        }
    }

    #[tokio::test]
    async fn handler_name_and_description() {
        let h = ReloadSkillsHandler::new(Arc::new(RwLock::new(CommandRegistry::new())));
        assert_eq!(h.name(), "reload-skills");
        assert_eq!(
            h.description(),
            "Pick up skills added or changed on disk during this session"
        );
    }

    // ---- format_reload_message: byte-exact against the binary's 4 shapes ----

    #[test]
    fn format_no_changes_zero_skills() {
        assert_eq!(
            format_reload_message(0, 0, 0, false),
            "Reloaded skills: 0 skills available (no changes)"
        );
    }

    #[test]
    fn format_one_added_singular() {
        assert_eq!(
            format_reload_message(1, 1, 0, false),
            "Reloaded skills: 1 skill available (1 added)"
        );
    }

    #[test]
    fn format_added_and_removed() {
        assert_eq!(
            format_reload_message(3, 1, 1, false),
            "Reloaded skills: 3 skills available (1 added, 1 removed)"
        );
    }

    #[test]
    fn format_no_changes_with_safe_mode_suffix() {
        assert_eq!(
            format_reload_message(2, 0, 0, true),
            "Reloaded skills: 2 skills available (no changes) (custom skills are disabled in safe mode)"
        );
    }

    // ---- end-to-end: real disk + registry ----

    #[tokio::test]
    async fn no_skills_on_disk_reports_no_changes() {
        let root = tmp_root("empty");
        let cwd = root.join("repo");
        let home = root.join("home");
        let lingxi_home = home.join(".lingxi");
        fs::create_dir_all(cwd.join(".git")).expect("git marker");
        fs::create_dir_all(&lingxi_home).expect("lingxi home");

        let h = ReloadSkillsHandler::with_all_roots(
            Arc::new(RwLock::new(CommandRegistry::new())),
            cwd,
            lingxi_home,
            None,
            home,
            Vec::new(),
            false,
        );
        assert_eq!(
            run(&h).await,
            "Reloaded skills: 0 skills available (no changes)"
        );
        fs::remove_dir_all(root).ok();
    }

    #[tokio::test]
    async fn discovers_new_skill_added_since_construction() {
        let root = tmp_root("added");
        let cwd = root.join("repo");
        let home = root.join("home");
        let lingxi_home = home.join(".lingxi");
        fs::create_dir_all(cwd.join(".git")).expect("git marker");
        fs::create_dir_all(&lingxi_home).expect("lingxi home");
        write_skill(&cwd.join(".lingxi").join("skills"), "demo", "Demo skill");

        let h = ReloadSkillsHandler::with_all_roots(
            Arc::new(RwLock::new(CommandRegistry::new())),
            cwd,
            lingxi_home,
            None,
            home,
            Vec::new(),
            false,
        );
        assert_eq!(
            run(&h).await,
            "Reloaded skills: 1 skill available (1 added)"
        );
        fs::remove_dir_all(root).ok();
    }

    #[tokio::test]
    async fn second_reload_with_unchanged_disk_reports_no_changes() {
        let root = tmp_root("stable");
        let cwd = root.join("repo");
        let home = root.join("home");
        let lingxi_home = home.join(".lingxi");
        fs::create_dir_all(cwd.join(".git")).expect("git marker");
        fs::create_dir_all(&lingxi_home).expect("lingxi home");
        write_skill(&cwd.join(".lingxi").join("skills"), "demo", "Demo skill");

        let h = ReloadSkillsHandler::with_all_roots(
            Arc::new(RwLock::new(CommandRegistry::new())),
            cwd,
            lingxi_home,
            None,
            home,
            Vec::new(),
            false,
        );
        assert_eq!(
            run(&h).await,
            "Reloaded skills: 1 skill available (1 added)"
        );
        // Nothing changed on disk between calls -> the second reload sees the
        // same name in `before` and `after`.
        assert_eq!(
            run(&h).await,
            "Reloaded skills: 1 skill available (no changes)"
        );
        fs::remove_dir_all(root).ok();
    }

    #[tokio::test]
    async fn deleted_skill_is_removed_from_live_registry() {
        let root = tmp_root("removed");
        let cwd = root.join("repo");
        let home = root.join("home");
        let lingxi_home = home.join(".lingxi");
        let skills = cwd.join(".lingxi").join("skills");
        fs::create_dir_all(cwd.join(".git")).expect("git marker");
        fs::create_dir_all(&lingxi_home).expect("lingxi home");
        write_skill(&skills, "demo", "Demo skill");

        let registry = Arc::new(RwLock::new(CommandRegistry::new()));
        let h = ReloadSkillsHandler::with_all_roots(
            registry.clone(),
            cwd,
            lingxi_home,
            None,
            home,
            Vec::new(),
            false,
        );
        assert_eq!(
            run(&h).await,
            "Reloaded skills: 1 skill available (1 added)"
        );
        fs::remove_dir_all(skills.join("demo")).expect("remove skill");
        assert_eq!(
            run(&h).await,
            "Reloaded skills: 0 skills available (1 removed)"
        );
        assert!(registry.read().await.resolve("demo").is_none());
        fs::remove_dir_all(root).ok();
    }

    #[tokio::test]
    async fn safe_mode_appends_suffix_end_to_end() {
        let root = tmp_root("safe");
        let cwd = root.join("repo");
        let home = root.join("home");
        let lingxi_home = home.join(".lingxi");
        fs::create_dir_all(cwd.join(".git")).expect("git marker");
        fs::create_dir_all(&lingxi_home).expect("lingxi home");
        write_skill(&cwd.join(".lingxi").join("skills"), "a", "A");
        write_skill(&cwd.join(".lingxi").join("skills"), "b", "B");

        let h = ReloadSkillsHandler::with_all_roots(
            Arc::new(RwLock::new(CommandRegistry::new())),
            cwd,
            lingxi_home,
            None,
            home,
            Vec::new(),
            true,
        );
        assert_eq!(
            run(&h).await,
            "Reloaded skills: 0 skills available (no changes) (custom skills are disabled in safe mode)"
        );
        fs::remove_dir_all(root).ok();
    }

    #[tokio::test]
    async fn new_uses_default_roots_without_panicking() {
        // Smoke test for the zero-arg constructor's fallback roots (whatever
        // the real cwd/HOME happen to be in the test environment) — just
        // asserts it runs and returns a well-formed message shape.
        let h = ReloadSkillsHandler::new(Arc::new(RwLock::new(CommandRegistry::new())));
        let out = run(&h).await;
        assert!(out.starts_with("Reloaded skills: "));
        assert!(out.contains(" available ("));
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn locked_post_reload_finalizer_blocks_readers_until_bundled_restore_finishes() {
        let root = tmp_root("atomic");
        let cwd = root.join("repo");
        let home = root.join("home");
        let lingxi_home = home.join(".lingxi");
        let commands = cwd.join(".lingxi").join("commands");
        let skills = cwd.join(".lingxi").join("skills");
        fs::create_dir_all(cwd.join(".git")).expect("git marker");
        fs::create_dir_all(&lingxi_home).expect("lingxi home");
        fs::create_dir_all(&commands).expect("commands dir");
        fs::write(
            commands.join("demo.md"),
            "---\ndescription: decoy command\n---\nDECOY\n",
        )
        .expect("write decoy command");
        write_skill(&skills, "demo", "Demo skill");

        let registry = Arc::new(RwLock::new(CommandRegistry::new()));
        let finalizer_entered = Arc::new((Mutex::new(false), Condvar::new()));
        let finalizer_release = Arc::new((Mutex::new(false), Condvar::new()));
        let reader_finished = Arc::new(AtomicBool::new(false));
        let reader_finished_flag = reader_finished.clone();
        let entered_wait = finalizer_entered.clone();
        let release_wait = finalizer_release.clone();
        let handler = ReloadSkillsHandler::with_all_roots(
            registry.clone(),
            cwd,
            lingxi_home,
            None,
            home,
            Vec::new(),
            false,
        )
        .with_locked_post_reload_finalizer(
            true,
            Arc::new(move |reg| {
                reg.register_command(bundled_cmd("demo"));
                let (entered_lock, entered_cv) = &*entered_wait;
                *entered_lock.lock().expect("entered lock") = true;
                entered_cv.notify_one();

                let (release_lock, release_cv) = &*release_wait;
                let mut released = release_lock.lock().expect("release lock");
                while !*released {
                    released = release_cv.wait(released).expect("release wait");
                }
            }),
        );

        let handler_task = tokio::spawn(async move { run(&handler).await });
        tokio::task::spawn_blocking(move || {
            let (entered_lock, entered_cv) = &*finalizer_entered;
            let entered = entered_lock.lock().expect("entered lock");
            let (_entered, timeout) = entered_cv
                .wait_timeout_while(entered, Duration::from_secs(5), |entered| !*entered)
                .expect("wait finalizer entered");
            assert!(
                !timeout.timed_out(),
                "finalizer did not enter within the timeout"
            );
        })
        .await
        .expect("wait for finalizer");

        let read_registry = registry.clone();
        let reader = tokio::spawn(async move {
            let reg = read_registry.read().await;
            reader_finished_flag.store(true, Ordering::SeqCst);
            reg.resolve("demo")
                .and_then(|cmd| cmd.loaded_from.clone())
                .expect("demo resolves")
        });
        tokio::task::yield_now().await;
        assert!(
            !reader_finished.load(Ordering::SeqCst),
            "reader must stay blocked until the locked finalizer returns"
        );

        let (release_lock, release_cv) = &*finalizer_release;
        *release_lock.lock().expect("release lock") = true;
        release_cv.notify_one();
        assert_eq!(
            handler_task.await.expect("handler task"),
            "Reloaded skills: 0 skills available (no changes)"
        );
        assert_eq!(
            reader.await.expect("reader task"),
            "bundled".to_string(),
            "reader must only observe the post-finalizer bundled winner"
        );
        fs::remove_dir_all(root).ok();
    }
}
