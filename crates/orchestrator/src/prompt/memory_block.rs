//! Memory-section formatter (claude-code `getLingxiMds`) +
//! `MemoryHierarchyProvider` trait.
//!
//! The trait abstracts the M3-02 `lingxi_md::walk` + `load_file` pair
//! so the orchestrator can take a `Arc<dyn MemoryHierarchyProvider>`
//! field and tests can substitute a static fixture without touching
//! the filesystem. Production impl: [`RealMemoryHierarchyProvider`].
//! The formatter ([`format`]) emits the preamble + per-file
//! `Contents of …:` blocks (GAP 3 — no enclosing tag, no trailing newline).
#![forbid(unsafe_code)]

use crate::prompt::MemoryFile;
use async_trait::async_trait;
pub use memory::lingxi_md::agents::{
    options_from_tiers as instruction_options_from_tiers, InstructionFilesMode,
};
use std::path::Path;
use std::sync::Arc;
#[cfg(test)]
#[path = "memory_block_agents_tests.rs"]
mod agents_tests;

/// Loads the LINGXI.md hierarchy for a given cwd.
///
/// Implementations MUST return the files in claude-code splice order:
/// the Managed tier first (`<managed>/LINGXI.md` + rules), then User
/// (`~/.lingxi/LINGXI.md` + rules), then Project (`<repo>/LINGXI.md`, …),
/// then Local (`<repo>/LINGXI.local.md`) — innermost last so it wins the
/// model's recency attention.
///
/// Native 2.1.287 eager acquisition excludes conditional rule-directory
/// projections after expanding their imports. Named instruction files and
/// their imports keep any `paths:` metadata and still render eagerly. Fresh
/// conditional acquisition is a separate capability below.
#[async_trait]
pub trait MemoryHierarchyProvider: Send + Sync {
    /// Load all LINGXI.md files relevant to `cwd`. May be empty.
    ///
    /// Native 2.1.286 Hot and LMe turn file and rule-walk I/O failures into
    /// successful soft skips. Unexpected failures still have distinct health
    /// reports; the read_eacces/read_failed/rules_walk_failed family is not
    /// implemented here. Non-regular and oversized files are reported by
    /// `loader::report_skipped_memory_file` with the 4 MiB stat guard intact.
    async fn load(&self, cwd: &Path) -> Vec<MemoryFile>;

    /// Custom providers retain control of their data; only the filesystem
    /// provider adds AGENTS discovery. Managed-only never renders user files.
    async fn load_with_mode(&self, cwd: &Path, mode: InstructionFilesMode) -> Vec<MemoryFile> {
        let mut files = self.load(cwd).await;
        if mode == InstructionFilesMode::ManagedOnly {
            files.retain(|file| file.tier == memory::lingxi_md::LingxiMdTier::Managed);
        }
        files
    }

    async fn load_managed(&self, cwd: &Path) -> Result<Vec<MemoryFile>, String> {
        Ok(self
            .load_with_mode(cwd, InstructionFilesMode::ManagedOnly)
            .await)
    }

    /// Fresh conditional rules for one touched path. This current capability
    /// is independent of eager file and context caches; missing/unreadable
    /// rule files remain successful soft skips.
    async fn load_conditional_rules(
        &self,
        cwd: &Path,
        trigger: &Path,
        mode: InstructionFilesMode,
    ) -> Vec<MemoryFile>;

    fn instruction_files_mode(&self) -> InstructionFilesMode {
        InstructionFilesMode::LingxiMd
    }

    fn filesystem_discovery(&self) -> bool {
        false
    }

    fn hierarchy_roots(&self) -> Option<(std::path::PathBuf, Option<std::path::PathBuf>)> {
        dirs::home_dir().map(|home| (home, Some(memory::lingxi_md::hierarchy::managed_path())))
    }

    /// Compiled `lingxiMdExcludes` matcher shared with lazy/nested loaders.
    /// Static and custom providers default to no exclusions.
    fn excluder(&self) -> Option<memory::lingxi_md::LingxiMdExcluder> {
        None
    }
}

/// Host authority for the current first-party account's user-context identity.
/// An absent result omits the identity; it is never inferred from another LLM
/// provider or from model-authored context.
#[async_trait]
pub trait InstructionUserEmailProvider: Send + Sync {
    /// Read the current account after the eager file load, as native yLn does.
    async fn current_user_email(&self) -> Option<String>;
}

/// Production implementation — wraps `memory::lingxi_md::walk` +
/// `expand_memory_file`. Reverses the walk order so the returned vec is in
/// claude-code splice order (managed → home → repo → local-override),
/// recursively splices each file's `@import` references directly after it
/// (parity with claude-code `processMemoryFile`), and tags each file with its
/// [`memory::lingxi_md::LingxiMdTier`]. The rule-directory eager filter applies
/// to each expanded projection; named files retain their globs. Conditional
/// activation uses a fresh rule-only walk, independently of this eager set.
pub struct RealMemoryHierarchyProvider;

fn load_memory_files(
    cwd: &Path,
    excluder: Option<&memory::lingxi_md::LingxiMdExcluder>,
    mode: InstructionFilesMode,
) -> Vec<MemoryFile> {
    // LINGXI_DISABLE_LINGXI_MDS (binary `yOe` @208938221:
    // `je.LINGXI_DISABLE_LINGXI_MDS ? [] : await Mv()`). A plain truthy
    // env check — ANY non-empty value (incl. "0") disables all LINGXI.md
    // loading; safe-mode sets it to "1".
    if std::env::var_os("LINGXI_DISABLE_LINGXI_MDS").is_some_and(|v| !v.is_empty()) {
        return Vec::new();
    }
    let Some(home) = dirs::home_dir() else {
        return Vec::new();
    };
    let managed = memory::lingxi_md::hierarchy::managed_path();
    let approved = migrations::global_config::global_config_path().is_some_and(|path| {
        migrations::global_config::check_has_lingxi_md_external_includes_approved(&path, cwd)
    });
    load_memory_files_at(cwd, &home, &managed, excluder, mode, approved)
}

fn load_memory_files_at(
    cwd: &Path,
    home: &Path,
    managed: &Path,
    excluder: Option<&memory::lingxi_md::LingxiMdExcluder>,
    mode: InstructionFilesMode,
    external_includes_approved: bool,
) -> Vec<MemoryFile> {
    let user_config_dir = memory::lingxi_md::hierarchy::user_config_dir(home);
    load_memory_files_at_with_user_config_dir(
        cwd,
        home,
        &user_config_dir,
        managed,
        excluder,
        mode,
        external_includes_approved,
    )
}

fn load_memory_files_at_with_user_config_dir(
    cwd: &Path,
    home: &Path,
    user_config_dir: &Path,
    managed: &Path,
    excluder: Option<&memory::lingxi_md::LingxiMdExcluder>,
    mode: InstructionFilesMode,
    external_includes_approved: bool,
) -> Vec<MemoryFile> {
    let h = if mode == InstructionFilesMode::ManagedOnly {
        memory::lingxi_md::hierarchy::walk_managed(managed)
    } else {
        memory::lingxi_md::hierarchy::walk_with_user_config_dir(
            cwd,
            home,
            user_config_dir,
            Some(managed),
        )
    };
    let mut entries = h.entries;
    entries.reverse();

    let mut processed: std::collections::HashSet<std::path::PathBuf> =
        std::collections::HashSet::new();
    let mut out = Vec::new();
    let mut origins = Vec::new();
    for e in entries {
        let include_external = include_external_for(e.tier, external_includes_approved);
        let expanded = memory::lingxi_md::loader::expand_memory_file_with_excluder(
            &e.path,
            &mut processed,
            include_external,
            cwd,
            Some(home),
            0,
            e.tier,
            excluder,
        );
        for (idx, entry) in expanded.into_iter().enumerate() {
            // Native eager M0n calls the rule walker with conditionalRule:false.
            // Imported projections are filtered individually after expansion;
            // a named instruction file keeps its separate top-level semantics.
            if e.origin == memory::lingxi_md::hierarchy::HierarchyEntryOrigin::RuleDirectory
                && entry.globs.is_some()
            {
                continue;
            }
            let body = entry.body;
            if lingxi_core::host::instruction_announcements::js_trim(&body).is_empty() {
                continue;
            }
            out.push(MemoryFile {
                path: entry.path,
                parent: entry.parent,
                source_content: Some(body.clone()),
                body,
                is_local_override: idx == 0 && e.is_local_override,
                tier: e.tier,
                globs: entry.globs,
                raw_content: entry.raw_content,
                content_differs_from_disk: entry.content_differs_from_disk,
            });
            origins.push(memory::lingxi_md::agents::identity(
                &instruction_project_dir(&e.path),
            ));
        }
    }
    if matches!(
        mode,
        InstructionFilesMode::LingxiMdOrAgentsMd | InstructionFilesMode::LingxiMdAndAgentsMd
    ) {
        let branded = memory::lingxi_md::agents::ancestors(
            cwd,
            None,
            home,
            false,
            external_includes_approved,
            excluder,
        );
        if mode == InstructionFilesMode::LingxiMdAndAgentsMd || branded.is_empty() {
            let groups = memory::lingxi_md::agents::ancestors(
                cwd,
                None,
                home,
                true,
                external_includes_approved,
                excluder,
            );
            let mut paths: std::collections::HashSet<_> = out
                .iter()
                .map(|file| memory::lingxi_md::agents::identity(&file.path))
                .collect();
            let bodies: std::collections::HashSet<_> = out
                .iter()
                .filter(|file| is_project(file.tier))
                .map(|file| file.body.trim().to_string())
                .collect();
            for group in groups {
                let files: Vec<_> = group
                    .files
                    .into_iter()
                    .filter(|file| {
                        !bodies.contains(file.body.trim())
                            && paths.insert(memory::lingxi_md::agents::identity(&file.path))
                    })
                    .map(agent_memory_file)
                    .collect();
                let index = out
                    .iter()
                    .enumerate()
                    .find(|(i, file)| {
                        is_project(file.tier)
                            && origins[*i] != group.dir
                            && origins[*i].starts_with(&group.dir)
                    })
                    .map(|(i, _)| i)
                    .unwrap_or_else(|| {
                        out.iter()
                            .rposition(|file| is_project(file.tier))
                            .map_or(out.len(), |i| i + 1)
                    });
                origins.splice(index..index, std::iter::repeat_n(group.dir, files.len()));
                out.splice(index..index, files);
            }
        }
    }
    out
}

fn is_project(tier: memory::lingxi_md::LingxiMdTier) -> bool {
    matches!(
        tier,
        memory::lingxi_md::LingxiMdTier::Project | memory::lingxi_md::LingxiMdTier::Local
    )
}

fn instruction_project_dir(path: &Path) -> std::path::PathBuf {
    let mut cursor = path.parent().unwrap_or(path);
    while let Some(parent) = cursor.parent() {
        if cursor
            .file_name()
            .is_some_and(|name| name == branding::DOT_DIR)
        {
            return parent.to_path_buf();
        }
        cursor = parent;
    }
    path.parent().unwrap_or(path).to_path_buf()
}

pub(crate) fn agent_memory_file(entry: memory::lingxi_md::loader::MemoryEntry) -> MemoryFile {
    MemoryFile {
        source_content: Some(entry.body.clone()),
        path: entry.path,
        parent: entry.parent,
        body: entry.body,
        is_local_override: false,
        tier: memory::lingxi_md::LingxiMdTier::Project,
        globs: None,
        raw_content: entry.raw_content,
        content_differs_from_disk: entry.content_differs_from_disk,
    }
}

/// Discover external `@import` targets that would require project approval.
///
/// User-tier memory is intentionally excluded because Claude Code always
/// allows its imports. Managed, project, and local files are scanned without
/// opening any target outside `cwd`; the returned paths are de-duplicated in
/// hierarchy order for the startup warning.
#[must_use]
pub fn pending_external_include_paths(cwd: &Path) -> Vec<std::path::PathBuf> {
    pending_external_include_paths_with_excluder(cwd, None)
}

/// Exclude-aware variant of [`pending_external_include_paths`].
#[must_use]
pub fn pending_external_include_paths_with_excluder(
    cwd: &Path,
    excluder: Option<&memory::lingxi_md::LingxiMdExcluder>,
) -> Vec<std::path::PathBuf> {
    if std::env::var_os("LINGXI_DISABLE_LINGXI_MDS").is_some_and(|v| !v.is_empty()) {
        return Vec::new();
    }
    let Some(home) = dirs::home_dir() else {
        return Vec::new();
    };
    let managed = memory::lingxi_md::hierarchy::managed_path();
    let mut entries = memory::lingxi_md::hierarchy::walk(cwd, &home, Some(&managed)).entries;
    entries.reverse();

    let mut seen = std::collections::HashSet::new();
    let mut paths = Vec::new();
    for entry in entries {
        if matches!(entry.tier, memory::lingxi_md::LingxiMdTier::User) {
            continue;
        }
        for path in memory::lingxi_md::loader::discover_external_include_paths_with_excluder(
            &entry.path,
            cwd,
            Some(&home),
            entry.tier,
            excluder,
        ) {
            if seen.insert(path.clone()) {
                paths.push(path);
            }
        }
    }
    paths
}

#[async_trait]
impl MemoryHierarchyProvider for RealMemoryHierarchyProvider {
    async fn load(&self, cwd: &Path) -> Vec<MemoryFile> {
        load_memory_files(cwd, None, InstructionFilesMode::default())
    }
    async fn load_with_mode(&self, cwd: &Path, mode: InstructionFilesMode) -> Vec<MemoryFile> {
        load_memory_files(cwd, None, mode)
    }
    async fn load_conditional_rules(
        &self,
        cwd: &Path,
        trigger: &Path,
        mode: InstructionFilesMode,
    ) -> Vec<MemoryFile> {
        load_conditional_rules(cwd, trigger, None, mode)
    }
    fn instruction_files_mode(&self) -> InstructionFilesMode {
        InstructionFilesMode::default()
    }
    fn filesystem_discovery(&self) -> bool {
        true
    }
}

/// Convenience constructor: returns an `Arc<dyn MemoryHierarchyProvider>`
/// wrapping a fresh [`RealMemoryHierarchyProvider`]. Used by the
/// production constructor of `ConversationOrchestrator`.
#[must_use]
pub fn real_provider() -> Arc<dyn MemoryHierarchyProvider> {
    Arc::new(RealMemoryHierarchyProvider)
}

/// Like [`real_provider`] but DROPS the `LINGXI.md` files whose path matches the
/// `lingxiMdExcludes` settings patterns (claude-code `isLingxiMdExcluded` runs
/// inside `processMemoryFile`, so excluded User/Project/Local files never reach
/// the system prompt; Managed is never excludable). Returns the unfiltered
/// [`real_provider`] when no patterns are configured (byte-identical to before).
#[must_use]
pub fn real_provider_with_excludes(excludes: Vec<String>) -> Arc<dyn MemoryHierarchyProvider> {
    let matcher = memory::lingxi_md::LingxiMdExcluder::new(&excludes);
    if matcher.is_empty() {
        return real_provider();
    }
    Arc::new(ExcludeFilterProvider { excluder: matcher })
}

/// Wraps a [`MemoryHierarchyProvider`] and filters its result through the
/// `lingxiMdExcludes` gate (see [`real_provider_with_excludes`]).
struct ExcludeFilterProvider {
    excluder: memory::lingxi_md::LingxiMdExcluder,
}

#[async_trait]
impl MemoryHierarchyProvider for ExcludeFilterProvider {
    async fn load(&self, cwd: &Path) -> Vec<MemoryFile> {
        load_memory_files(cwd, Some(&self.excluder), InstructionFilesMode::default())
    }
    async fn load_with_mode(&self, cwd: &Path, mode: InstructionFilesMode) -> Vec<MemoryFile> {
        load_memory_files(cwd, Some(&self.excluder), mode)
    }
    async fn load_conditional_rules(
        &self,
        cwd: &Path,
        trigger: &Path,
        mode: InstructionFilesMode,
    ) -> Vec<MemoryFile> {
        load_conditional_rules(cwd, trigger, Some(&self.excluder), mode)
    }
    fn instruction_files_mode(&self) -> InstructionFilesMode {
        InstructionFilesMode::default()
    }
    fn filesystem_discovery(&self) -> bool {
        true
    }

    fn excluder(&self) -> Option<memory::lingxi_md::LingxiMdExcluder> {
        Some(self.excluder.clone())
    }
}

/// Configure an injected provider without replacing custom/empty providers.
#[must_use]
pub fn configure_instruction_files(
    provider: Arc<dyn MemoryHierarchyProvider>,
    options: &serde_json::Value,
) -> Arc<dyn MemoryHierarchyProvider> {
    Arc::new(InstructionModeProvider {
        provider,
        mode: InstructionFilesMode::from_options(options),
    })
}

struct InstructionModeProvider {
    provider: Arc<dyn MemoryHierarchyProvider>,
    mode: InstructionFilesMode,
}

#[async_trait]
impl MemoryHierarchyProvider for InstructionModeProvider {
    async fn load(&self, cwd: &Path) -> Vec<MemoryFile> {
        self.provider.load_with_mode(cwd, self.mode).await
    }
    async fn load_with_mode(&self, cwd: &Path, mode: InstructionFilesMode) -> Vec<MemoryFile> {
        self.provider.load_with_mode(cwd, mode).await
    }
    async fn load_managed(&self, cwd: &Path) -> Result<Vec<MemoryFile>, String> {
        self.provider.load_managed(cwd).await
    }
    async fn load_conditional_rules(
        &self,
        cwd: &Path,
        trigger: &Path,
        mode: InstructionFilesMode,
    ) -> Vec<MemoryFile> {
        self.provider
            .load_conditional_rules(cwd, trigger, mode)
            .await
    }
    fn instruction_files_mode(&self) -> InstructionFilesMode {
        self.mode
    }
    fn filesystem_discovery(&self) -> bool {
        self.provider.filesystem_discovery()
    }
    fn hierarchy_roots(&self) -> Option<(std::path::PathBuf, Option<std::path::PathBuf>)> {
        self.provider.hierarchy_roots()
    }
    fn excluder(&self) -> Option<memory::lingxi_md::LingxiMdExcluder> {
        self.provider.excluder()
    }
}

fn load_conditional_rules(
    cwd: &Path,
    trigger: &Path,
    excluder: Option<&memory::lingxi_md::LingxiMdExcluder>,
    mode: InstructionFilesMode,
) -> Vec<MemoryFile> {
    if std::env::var_os("LINGXI_DISABLE_LINGXI_MDS").is_some_and(|value| !value.is_empty()) {
        return Vec::new();
    }
    let Some(home) = dirs::home_dir() else {
        return Vec::new();
    };
    super::nested_memory::discover_conditional_rules(
        trigger,
        cwd,
        &home,
        Some(&memory::lingxi_md::hierarchy::managed_path()),
        excluder,
        mode,
    )
}

/// Native xJ/iAo: retain acquired eager descriptor order and trim content.
/// Rules-directory conditional filtering happens during acquisition; a named
/// instruction file's glob metadata does not suppress its eager descriptor.
pub(crate) fn instruction_file_descriptors(
    files: &[MemoryFile],
) -> Vec<lingxi_core::host::instructions::InstructionFile> {
    use lingxi_core::host::instructions::{InstructionFile, InstructionFileType};
    use memory::lingxi_md::LingxiMdTier;
    files
        .iter()
        .filter(|file| is_rendered_into_context(file))
        .map(|file| InstructionFile {
            path: file.path.to_string_lossy().into_owned(),
            kind: match file.tier {
                LingxiMdTier::Managed => InstructionFileType::Managed,
                LingxiMdTier::User => InstructionFileType::User,
                LingxiMdTier::Project => InstructionFileType::Project,
                LingxiMdTier::Local => InstructionFileType::Local,
            },
            content: lingxi_core::host::instruction_announcements::js_trim(&file.body).to_owned(),
        })
        .collect()
}

fn context_from_instruction_files(
    cwd: &Path,
    files: &[MemoryFile],
    managed: bool,
) -> lingxi_core::host::instructions::InstructionContext {
    use lingxi_core::host::instructions::InstructionContext;
    let mut context = InstructionContext {
        eager_instructions: Some(instruction_file_descriptors(files)),
        managed_instructions_only: managed,
        instructions_root: Some(memory::lingxi_md::agents::identity(cwd)),
        ..InstructionContext::default()
    };
    context
        .user_context
        .insert("instructions".into(), format(files));
    context.user_context_order.push("instructions".into());
    for file in files.iter().filter(|file| is_rendered_into_context(file)) {
        context
            .sent_paths
            .insert(memory::lingxi_md::agents::identity(&file.path));
        if is_project(file.tier) {
            context
                .project_instruction_bodies
                .insert(file.body.trim().to_owned());
        }
    }
    context
}

/// A child reader bound once to the actual owning root's current Gv Promise.
/// Caller cwd and child budget identifiers never define its cache authority.
#[derive(Default)]
pub struct RootInstructionContextProvider {
    root: std::sync::OnceLock<std::sync::Weak<crate::ConversationOrchestrator>>,
}

impl RootInstructionContextProvider {
    /// Create the provider before root assembly; loads fail until it is bound.
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }

    /// Bind exactly one root after its Arc is constructed, before child ingress.
    pub fn bind(&self, root: &Arc<crate::ConversationOrchestrator>) -> Result<(), String> {
        self.root
            .set(Arc::downgrade(root))
            .map_err(|_| "instruction context root was already bound".to_owned())
    }

    /// Whether the bound owner is currently alive. This is only readiness;
    /// load still captures the actual current owner and typed session identity.
    #[must_use]
    pub fn is_bound(&self) -> bool {
        self.root.get().is_some_and(|root| root.strong_count() > 0)
    }

    fn owner(&self) -> Result<Arc<crate::ConversationOrchestrator>, String> {
        self.root
            .get()
            .and_then(std::sync::Weak::upgrade)
            .ok_or_else(|| "instruction context root is unavailable".to_owned())
    }
}

#[async_trait]
impl lingxi_core::host::instructions::InstructionContextProvider
    for RootInstructionContextProvider
{
    async fn load(
        &self,
        _cwd: &Path,
        scope: lingxi_core::host::instructions::InstructionScope,
    ) -> Result<lingxi_core::host::instructions::InstructionContext, String> {
        use lingxi_core::host::instructions::{InstructionFileType, InstructionScope};
        let root = self.owner()?;
        let (_, load) = root.main_instruction_load().await;
        // A pending detached build owns independent dependencies. Keeping the
        // root here would create a cache/producer/root lifetime cycle.
        drop(root);
        let mut context = load.get().await?.as_ref().clone();
        if scope == InstructionScope::Full || context.eager_instructions.is_none() {
            return Ok(context);
        }
        let managed = load
            .memory_files()
            .await
            .iter()
            .filter(|file| file.tier == memory::lingxi_md::LingxiMdTier::Managed)
            .cloned()
            .collect::<Vec<_>>();
        let body = format(&managed);
        context.eager_instructions = context.eager_instructions.map(|files| {
            files
                .into_iter()
                .filter(|file| file.kind == InstructionFileType::Managed)
                .collect()
        });
        context.managed_instructions_only = !body.is_empty();
        if body.is_empty() {
            context.user_context.remove("instructions");
            context.user_context_order.retain(|name| name != "instructions");
        } else {
            context.user_context.insert("instructions".into(), body);
        }
        Ok(context)
    }

    async fn after_read(
        &self,
        cwd: &Path,
        path: &Path,
        is_partial: bool,
        context: &mut lingxi_core::host::instructions::InstructionContext,
    ) -> lingxi_core::host::instructions::InstructionReadContext {
        let Ok(root) = self.owner() else {
            return Default::default();
        };
        let provider = root.memory.clone();
        drop(root);
        instruction_context_after_read(provider, cwd, path, is_partial, context).await
    }
}

async fn instruction_context_after_read(
    provider: Arc<dyn MemoryHierarchyProvider>,
    cwd: &Path,
    path: &Path,
    is_partial: bool,
    context: &mut lingxi_core::host::instructions::InstructionContext,
) -> lingxi_core::host::instructions::InstructionReadContext {
    use lingxi_core::host::instructions::InstructionReadContext;
    if !provider.filesystem_discovery()
        || !memory::lingxi_md::agents::attachments_enabled()
        || std::env::var_os("LINGXI_DISABLE_LINGXI_MDS").is_some_and(|value| !value.is_empty())
    {
        return InstructionReadContext::default();
    }
    let Some((home, managed)) = provider.hierarchy_roots() else {
        return InstructionReadContext::default();
    };
    let root = context
        .instructions_root
        .clone()
        .unwrap_or_else(|| memory::lingxi_md::agents::identity(cwd));
    let trigger = if let Ok(relative) = path.strip_prefix("~") {
        home.join(relative)
    } else if path.is_absolute() {
        path.to_path_buf()
    } else {
        cwd.join(path)
    };
    let mode = if context.managed_instructions_only {
        InstructionFilesMode::ManagedOnly
    } else {
        provider.instruction_files_mode()
    };
    let excluder = provider.excluder();
    let legacy = super::nested_memory::discover_with_mode(
        &trigger,
        &root,
        &home,
        managed.as_deref(),
        excluder.as_ref(),
        mode,
    );
    let agents = super::nested_memory::discover_agents(
        &trigger,
        &root,
        &home,
        mode,
        excluder.as_ref(),
        &context.sent_paths,
        &context.project_instruction_bodies,
    );
    let trigger = memory::lingxi_md::agents::identity(&trigger);
    let mut delivered = InstructionReadContext::default();
    for (file, is_agents_context) in legacy
        .into_iter()
        .map(|file| (file, false))
        .chain(agents.into_iter().map(|file| (file, true)))
    {
        let key = memory::lingxi_md::agents::identity(&file.path);
        if context.sent_paths.contains(&key) {
            continue;
        }
        if key == trigger {
            if !is_partial {
                context.sent_paths.insert(key);
            }
            continue;
        }
        context.sent_paths.insert(key);
        if is_agents_context {
            delivered.agents_context.push(format!(
                "Contents of {}:\n\n{}",
                file.path.display(),
                file.body
            ));
        } else {
            delivered
                .legacy_reminders
                .push(super::conditional_rules::render_reminder(&file));
        }
    }
    delivered
}

impl crate::ConversationOrchestrator {
    /// Claude 2.1.286 `Ye` adds AGENTS context to every successful Read result,
    /// including media and unchanged results that do not update readFileState.
    /// Its self-read cursor depends only on the original offset/limit fields.
    pub(crate) async fn agents_context_after_read(&self, input: &serde_json::Value) -> Vec<String> {
        if !self.memory.filesystem_discovery()
            || !memory::lingxi_md::agents::attachments_enabled()
            || std::env::var_os("LINGXI_DISABLE_LINGXI_MDS").is_some_and(|value| !value.is_empty())
        {
            return Vec::new();
        }
        let Some(path) = input.get("file_path").and_then(serde_json::Value::as_str) else {
            return Vec::new();
        };
        let Some((home, _)) = self.memory.hierarchy_roots() else {
            return Vec::new();
        };
        let cwd = self.prompt_probe_cwd(&self.session_cwd.cwd());
        let trigger = if path == "~" {
            home.clone()
        } else if let Some(relative) = path.strip_prefix("~/") {
            home.join(relative)
        } else if Path::new(path).is_absolute() {
            self.prompt_probe_cwd(Path::new(path))
        } else {
            cwd.join(path)
        };
        let trigger = memory::lingxi_md::agents::identity(&trigger);
        // Initialize from the frozen parent request, then serialize cursor
        // updates across concurrent successful Read callbacks.
        self.instruction_context_snapshot().await;
        let mut stored = self.prompt_runtime.instruction_context.lock().await;
        let Some(context) = stored.as_mut() else {
            return Vec::new();
        };
        let mode = if context.managed_instructions_only {
            InstructionFilesMode::ManagedOnly
        } else {
            self.memory.instruction_files_mode()
        };
        let root = context.instructions_root.as_deref().unwrap_or(&cwd);
        let excluder = self.memory.excluder();
        let files = super::nested_memory::discover_agents(
            &trigger,
            root,
            &home,
            mode,
            excluder.as_ref(),
            &context.sent_paths,
            &context.project_instruction_bodies,
        );
        let full_self_read = input.get("offset").is_none() && input.get("limit").is_none();
        let mut contexts = Vec::new();
        for file in files {
            let key = memory::lingxi_md::agents::identity(&file.path);
            if key == trigger {
                if full_self_read {
                    context.sent_paths.insert(key);
                }
                continue;
            }
            context.sent_paths.insert(key);
            contexts.push(format!(
                "Contents of {}:\n\n{}",
                file.path.display(),
                file.body
            ));
        }
        contexts
    }

    /// Install the real host identity producer before the first context load.
    #[must_use]
    pub fn with_instruction_user_email_provider(
        mut self,
        provider: Arc<dyn InstructionUserEmailProvider>,
    ) -> Self {
        self.prompt_runtime.instruction_user_email_provider = Some(provider);
        self
    }

    /// Native pT: retire user-context Promises under this explicit root only.
    /// Eager-file Promises and already running producers retain their lifetime.
    pub fn invalidate_instruction_context(
        &self,
        reason: lingxi_core::host::instructions::InstructionRefreshReason,
    ) {
        self.prompt_runtime
            .instruction_cache
            .invalidate_context(reason);
    }

    /// Native WC: clear eager-file Promises while retaining Gv user context.
    pub fn clear_instruction_files(&self) {
        self.prompt_runtime.instruction_cache.clear_files();
    }

    /// One synchronous native event which performs WC followed by pT. The
    /// shared root lock prevents another Rust thread from observing half of it.
    pub fn refresh_instruction_context(
        &self,
        reason: lingxi_core::host::instructions::InstructionRefreshReason,
    ) {
        self.prompt_runtime
            .instruction_cache
            .invalidate_context_with_files(reason);
    }

    /// Native Ctn: clear identities and immediately fence older completions.
    /// File-cache invalidation remains a distinct event.
    pub fn clear_instruction_scope(&self) {
        self.prompt_runtime.instruction_cache.clear_scope();
    }

    pub(crate) async fn instruction_file_snapshot(&self) -> Result<Arc<Vec<MemoryFile>>, String> {
        use lingxi_core::host::instruction_context_cache::InstructionFileCacheKey;
        use lingxi_core::host::instructions::InstructionScope;
        let session = self.session.lock().await;
        let cwd = self.prompt_probe_cwd(&self.session_cwd.cwd());
        let provider = self.memory.clone();
        let scope = if provider.instruction_files_mode() == InstructionFilesMode::ManagedOnly {
            InstructionScope::ManagedOnly
        } else {
            InstructionScope::Full
        };
        let files = self.prompt_runtime.instruction_cache.begin_files(
            InstructionFileCacheKey {
                scope,
                force_external: false,
            },
            move || async move {
                if scope == InstructionScope::ManagedOnly {
                    provider.load_managed(&cwd).await
                } else {
                    Ok(provider.load(&cwd).await)
                }
            },
        );
        drop(session);
        files.get().await
    }

    /// Obtain all Gv projections from one original load handle. Reserving the
    /// context and its file Promise together preserves ingress across a
    /// concurrent root refresh before the detached producer is first polled.
    pub(crate) async fn main_instruction_load(
        &self,
    ) -> (
        lingxi_core::host::instructions::InstructionContextKey,
        lingxi_core::host::instruction_context_cache::InstructionContextLoad<MemoryFile>,
    ) {
        use lingxi_core::host::instruction_context_cache::{
            InstructionContextBuild, InstructionFileCacheKey,
        };
        use lingxi_core::host::instructions::{InstructionContextKey, InstructionScope};
        // Session activation retires Gv/qb under this same identity guard.
        // Keep it through reservation so a captured old id cannot be inserted
        // as a new build after the lifecycle classifier fence has advanced.
        let session = self.session.lock().await;
        let session_id = session.session_id;
        let key = InstructionContextKey {
            session_id,
            agent_id: None,
        };
        let cwd = self.prompt_probe_cwd(&self.session_cwd.cwd());
        let provider = self.memory.clone();
        let managed = provider.instruction_files_mode() == InstructionFilesMode::ManagedOnly;
        let file_scope = if managed {
            InstructionScope::ManagedOnly
        } else {
            InstructionScope::Full
        };
        let file_cwd = cwd.clone();
        let email_provider = self.prompt_runtime.instruction_user_email_provider.clone();
        let configured_email = self.config.user_email.clone();
        let date = self.session_start_date(session_id);
        let load = self.prompt_runtime.instruction_cache.begin_context_with_files(
            key,
            InstructionFileCacheKey { scope: file_scope, force_external: false },
            move || async move {
                if managed { provider.load_managed(&file_cwd).await }
                else { Ok(provider.load(&file_cwd).await) }
            },
            move |files| async move {
                let files = files.get().await?;
                let mut context = context_from_instruction_files(&cwd, &files, managed);
                let email = if let Some(provider) = email_provider {
                    provider.current_user_email().await
                } else {
                    configured_email
                };
                if let Some(email) = email.as_deref().filter(|email| !email.is_empty()) {
                    context.user_context.insert("userEmail".into(), format!("The user's email address is {email}. Use it only to identify the user, such as for authorship, attribution, or filtering their own work. Never send it to an unrelated service, such as in a request header, URL, or payload, unless the user explicitly asks."));
                    context.user_context_order.push("userEmail".into());
                }
                context.user_context.insert("currentDate".into(), format!("Today's date is {date}."));
                context.user_context_order.push("currentDate".into());
                // The runtime has no native classifier or plugin instruction
                // acquisition producer here. Their optional facts remain absent.
                Ok(InstructionContextBuild::new(context, files))
            },
        );
        drop(session);
        (key, load)
    }

    pub(crate) async fn main_instruction_files(&self) -> Result<Arc<Vec<MemoryFile>>, String> {
        let (_, load) = self.main_instruction_load().await;
        load.get().await?;
        Ok(load.memory_files().await)
    }

    pub(crate) async fn capture_cached_instruction_context(
        &self,
        key: lingxi_core::host::instructions::InstructionContextKey,
        cached: &Arc<lingxi_core::host::instructions::InstructionContext>,
    ) {
        let session = self.session.lock().await;
        if session.session_id != key.session_id {
            return;
        }
        let mut stored = self.prompt_runtime.instruction_context.lock().await;
        if !self
            .prompt_runtime
            .instruction_cache
            .is_current_context(key, cached)
        {
            return;
        }
        let mut origin = self
            .prompt_runtime
            .instruction_context_origin
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        if origin
            .as_ref()
            .is_some_and(|prior| Arc::ptr_eq(prior, cached))
            && stored.is_some()
        {
            return;
        }
        let mut context = cached.as_ref().clone();
        context.rendering = self.config.context_rendering;
        let same_identity = *self
            .prompt_runtime
            .instruction_context_key
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            == Some(key);
        if let Some(previous) = stored.as_ref().filter(|_| same_identity) {
            // Gv's frozen descriptors are independent of the lazy discovery
            // cursor and transcript-derived attachment baseline.
            context
                .sent_paths
                .extend(previous.sent_paths.iter().cloned());
            context
                .announcement_history
                .clone_from(&previous.announcement_history);
            if self.config.context_rendering
                != lingxi_core::host::instructions::InstructionRendering::Inline
            {
                context.rendering = previous.rendering;
            }
        }
        *stored = Some(context);
        *origin = Some(cached.clone());
        *self
            .prompt_runtime
            .instruction_context_key
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner) = Some(key);
        *self
            .prompt_runtime
            .instruction_context_reason
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner) =
            self.prompt_runtime.instruction_cache.reason(key);
    }

    pub(crate) fn frozen_instruction_refresh_reason(
        &self,
    ) -> lingxi_core::host::instructions::InstructionRefreshReason {
        *self
            .prompt_runtime
            .instruction_context_reason
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
    }

    /// Frozen eager instructions plus the main agent's current lazy cursor.
    pub(crate) async fn instruction_context_snapshot(
        &self,
    ) -> lingxi_core::host::instructions::InstructionContext {
        use lingxi_core::host::instructions::InstructionContextKey;
        let current = InstructionContextKey {
            session_id: self.session.lock().await.session_id,
            agent_id: None,
        };
        let needs_load = {
            let cursor = self.prompt_runtime.instruction_context.lock().await;
            let key = self
                .prompt_runtime
                .instruction_context_key
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner);
            cursor.is_none() || *key != Some(current)
        };
        if needs_load {
            let (key, load) = self.main_instruction_load().await;
            if let Ok(context) = load.get().await {
                self.capture_cached_instruction_context(key, &context).await;
            }
        }
        let rendering = self.current_context_rendering().await;
        let announcement_history = {
            let session = self.session.lock().await;
            let attachments = self
                .transcript
                .model_reminder_attachments
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner);
            session
                .model_context_history()
                .iter()
                .filter_map(|message| attachments.get(&message.id()).cloned())
                .collect::<Vec<_>>()
        };
        let mut context = {
            let mut stored = self.prompt_runtime.instruction_context.lock().await;
            if let Some(stored) = stored.as_mut() {
                stored.rendering = rendering;
            }
            stored.clone().unwrap_or_default()
        };
        context.rendering = rendering;
        context.announcement_history = announcement_history;
        context
    }
}

/// claude-code external-`@import` gate (claudemd.ts:826-846): the User tier
/// always resolves external includes; every other tier (Managed/Project/Local)
/// does so ONLY when `hasLingxiMdExternalIncludesApproved` is set for the
/// project. (claude-code also has an internal `forceIncludeExternal`; LingXi has
/// no caller that sets it, so it is omitted.)
#[must_use]
fn include_external_for(tier: memory::lingxi_md::LingxiMdTier, approved: bool) -> bool {
    matches!(tier, memory::lingxi_md::LingxiMdTier::User) || approved
}

#[cfg(test)]
mod external_include_tests {
    use super::{
        include_external_for, pending_external_include_paths,
        pending_external_include_paths_with_excluder,
    };
    use memory::lingxi_md::LingxiMdExcluder;
    use memory::lingxi_md::LingxiMdTier::{Local, Managed, Project, User};

    #[test]
    fn user_tier_always_allows_external_others_only_when_approved() {
        // User tier: external `@import`s always allowed (unconditional).
        assert!(include_external_for(User, false));
        assert!(include_external_for(User, true));
        // Managed/Project/Local: gated on the per-project approval flag.
        for tier in [Managed, Project, Local] {
            assert!(
                !include_external_for(tier, false),
                "{tier:?} gated when unapproved"
            );
            assert!(
                include_external_for(tier, true),
                "{tier:?} allowed when approved"
            );
        }
    }

    #[test]
    fn pending_scan_reports_project_external_imports_once() {
        let root = tempfile::tempdir().expect("tempdir");
        let cwd = root.path().join("project");
        std::fs::create_dir_all(&cwd).expect("mkdir project");
        let outside = root.path().join("shared.md");
        std::fs::write(
            cwd.join(branding::MEMORY_FILE),
            format!("@{}\n@{}\n", outside.display(), outside.display()),
        )
        .expect("write memory file");

        assert_eq!(pending_external_include_paths(&cwd), vec![outside]);
    }

    #[test]
    fn pending_scan_with_excluder_skips_excluded_subtree() {
        let root = tempfile::tempdir().expect("tempdir");
        let cwd = root.path().join("project");
        let nested = cwd.join("secret");
        std::fs::create_dir_all(&nested).expect("mkdir nested");
        let outside = root.path().join("shared.md");
        std::fs::write(cwd.join(branding::MEMORY_FILE), "@./secret/LINGXI.md\n")
            .expect("write root memory");
        std::fs::write(
            nested.join("LINGXI.md"),
            format!("@{}\n", outside.display()),
        )
        .expect("write nested memory");

        let excluder = LingxiMdExcluder::new(&["**/secret/LINGXI.md".to_string()]);
        assert_eq!(
            pending_external_include_paths_with_excluder(&cwd, Some(&excluder)),
            Vec::<std::path::PathBuf>::new()
        );
    }
}

#[cfg(test)]
mod exclude_filter_tests {
    use super::*;
    use memory::lingxi_md::LingxiMdTier;

    #[test]
    fn filter_drops_matching_user_project_local_keeps_managed() {
        let excluder = memory::lingxi_md::LingxiMdExcluder::new(&["**/LINGXI.md".to_string()]);
        assert!(!excluder.is_excluded(Path::new("/mgr/LINGXI.md"), LingxiMdTier::Managed));
        assert!(excluder.is_excluded(Path::new("/a/secret/LINGXI.md"), LingxiMdTier::Project));
        assert!(excluder.is_excluded(Path::new("/a/public/LINGXI.md"), LingxiMdTier::User));
    }

    #[test]
    fn empty_excludes_returns_unfiltered_provider() {
        // No patterns ⇒ the plain real_provider (no wrapper), byte-identical path.
        let _ = real_provider_with_excludes(vec![]);
    }
}

/// Build a memdir-backed memory prefetcher for the composition root — the P0.1
/// activation of the `relevant_memories` surfacing channel.
///
/// Wires the LLM memory selector through the session's provider service over the
/// user memdir (`<home>/.lingxi/memdir`) so that, each turn,
/// [`MemoryPrefetch::start`](memory::prefetch::MemoryPrefetch::start) scans the
/// memdir, asks the selector which entries are relevant to the turn query, and
/// surfaces them through
/// [`ConversationOrchestrator::relevant_memory_reminder_messages`](crate::ConversationOrchestrator).
/// Hand the returned handle to
/// [`ConversationOrchestrator::with_memory_prefetch`](crate::ConversationOrchestrator).
///
/// Centralised here (not inlined at each composition root) so desktop / bridge /
/// mobile build the prefetch identically and the engine apps need no direct
/// dependency on the `memory` crate's internals.
///
/// The composition root applies the auto-memory feature gate before building
/// this handle. The orchestrator supplies the live model and profile for each
/// prefetch. Team memory is disabled here until the host configures it.
#[must_use]
pub fn build_memdir_prefetch(
    side_query_client: Arc<dyn sidequery::SideQueryClient>,
    runtime: Arc<dyn lingxi_core::host::RuntimeSpawner>,
    home: &std::path::Path,
    cwd: &std::path::Path,
) -> Arc<memory::prefetch::MemoryPrefetch> {
    // MEM-2: the User tier is scoped to `cwd`'s project, so memories written
    // while working on one repository are not recalled in another.
    let roots = memory::memdir::memdir_path(home, cwd, false);
    let selector = Arc::new(memory::selector::MemorySelector::new(side_query_client));
    Arc::new(memory::prefetch::MemoryPrefetch::new(
        selector, runtime, roots,
    ))
}

/// Build a [`SessionMemoryHandle`](crate::SessionMemoryHandle) for the
/// composition root — the §6.5 standalone session-memory extractor + its forked
/// runner. Hand to [`ConversationOrchestrator::with_session_memory`](crate::ConversationOrchestrator);
/// the composition root gates the call (default OFF). `config_home` is the
/// resolved `$LINGXI_CONFIG_DIR ?? ~/.lingxi` dir (the write base — pass
/// [`user_config_dir`](memory::lingxi_md::user_config_dir)`(dirs::home_dir())`).
/// The runner inherits the live parent route from its cache-safe snapshot;
/// `default_model` is the configured session model used if that snapshot is cold.
/// `config` supplies the current token-growth and activity gates.
#[must_use]
pub fn build_session_memory_handle(
    side_query_client: Arc<dyn sidequery::SideQueryClient>,
    default_model: String,
    config: memory::session_memory::SessionMemoryConfig,
    home: &std::path::Path,
    runtime: Arc<dyn lingxi_core::host::RuntimeSpawner>,
) -> Arc<crate::SessionMemoryHandle> {
    // The `$LINGXI_CONFIG_DIR`-aware config-home is the SAME
    // base the Session-tier memdir scan reads, so writes re-load next session.
    let config_home = memory::lingxi_md::user_config_dir(home);
    let runner = Arc::new(
        sidequery::ForkedAgentRunner::new()
            .with_side_query_client(side_query_client, default_model),
    );
    Arc::new(crate::SessionMemoryHandle {
        extractor: tokio::sync::Mutex::new(memory::session_memory::SessionMemoryExtractor::new(
            config,
        )),
        runner,
        config_home,
        runtime,
        in_flight: std::sync::atomic::AtomicBool::new(false),
        generation: std::sync::atomic::AtomicU64::new(0),
    })
}

/// Verbatim preamble that precedes the memory blocks.
///
/// 1:1 with claude-code `MEMORY_INSTRUCTION_PROMPT` (claudemd.ts:89-90).
const MEMORY_INSTRUCTION_PROMPT: &str = "Codebase and user instructions are shown below. Be sure to adhere to these instructions. IMPORTANT: These instructions OVERRIDE any default behavior and you MUST follow them exactly as written.";

/// The per-file injection description for a given tier (claudemd.ts:1168-1186).
/// Includes the leading space, exactly as TS concatenates `${file.path}${description}`.
fn tier_description(tier: memory::lingxi_md::LingxiMdTier) -> &'static str {
    use memory::lingxi_md::LingxiMdTier;
    match tier {
        LingxiMdTier::Project => " (project instructions, checked into the codebase)",
        LingxiMdTier::Local => " (user's private project instructions, not checked in)",
        // Binary `getLingxiMds` (`nUt`) 5-way switch on `o.type`: Managed has its
        // OWN description; only the default (User) gets the global-instructions
        // wording. (Previously Managed was folded into the User arm — a
        // divergence whenever an org-managed LINGXI.md is loaded.)
        LingxiMdTier::Managed => " (organization-managed policy instructions)",
        LingxiMdTier::User => " (user's private global instructions for all projects)",
    }
}

/// Native 2.1.287 L0n/bxr test content truthiness after acquisition applied
/// rule-directory filtering. Glob metadata on named files is retained. The
/// same predicate supplies eager seeding and rendering. AutoMem and the
/// paper-halyard feature producer remain outside this provider's surface.
#[must_use]
pub fn is_rendered_into_context(f: &MemoryFile) -> bool {
    !f.body.is_empty()
}

/// Format the memory section from a slice of loaded files, 1:1 with claude-code
/// `getLingxiMds` (claudemd.ts:1153-1195).
///
/// Shape (NO enclosing tag, NO trailing newline; DOUBLE-newline separators):
/// ```text
/// {MEMORY_INSTRUCTION_PROMPT}
///
/// Contents of {p1}{desc1}:
///
/// {body1}
///
/// Contents of {p2}{desc2}:
///
/// {body2}
/// ```
/// where `{descN}` is the tier description (project / local / global) and each
/// body is `.trim()`med. Blocks are joined by `"\n\n"` (binary `_9t` tail
/// `${$ip}\n\n${n.join(`\n\n`)}`, and each block is `…:\n\n${i}` — all DOUBLE
/// newlines, verified via `od -c` on the 2.1.195 binary; the `strings` dump
/// misled an earlier pass into single `\n`). When `files` is empty, returns the
/// EMPTY STRING and the caller MUST elide the section.
///
/// Native xJ filters acquired files by content truthiness. Conditional
/// rules-directory entries were filtered upstream; named instruction files and
/// their imports keep their acquisition metadata at this rendering boundary.
#[must_use]
pub fn format(files: &[MemoryFile]) -> String {
    let blocks: Vec<String> = files
        .iter()
        // Apply the same post-acquisition truthiness predicate as seeding.
        .filter(|f| is_rendered_into_context(f))
        .map(|f| {
            // Binary `_9t` (getLingxiMds): `Contents of ${o.path}${s}:\n\n${i}`
            // — DOUBLE `\n` between the header and the trimmed body (od -c
            // verified on 2.1.195).
            format!(
                "Contents of {}{}:\n\n{}",
                f.path.display(),
                tier_description(f.tier),
                lingxi_core::host::instruction_announcements::js_trim(&f.body)
            )
        })
        .collect();
    if blocks.is_empty() {
        return String::new();
    }
    // Binary `_9t` tail: `${$ip}\n\n${n.join(`\n\n`)}` — DOUBLE `\n` both for the
    // preamble→blocks separator and the block join (od -c verified on 2.1.195).
    format!("{MEMORY_INSTRUCTION_PROMPT}\n\n{}", blocks.join("\n\n"))
}

#[cfg(test)]
mod native_eager_origin_tests {
    use super::*;

    fn write(path: &Path, body: &str) {
        std::fs::create_dir_all(path.parent().unwrap()).unwrap();
        std::fs::write(path, body).unwrap();
    }

    #[test]
    fn actual_287_parser_acquisition_and_render_goldens_match_filesystem() {
        let oracle: serde_json::Value = serde_json::from_str(include_str!(
            "../../tests/fixtures/instruction_eager_2_1_287.json"
        ))
        .unwrap();
        assert_eq!(oracle["cases"].as_array().unwrap().len(), 12);
        for case in oracle["cases"].as_array().unwrap() {
            let temp = tempfile::tempdir().unwrap();
            let root = std::fs::canonicalize(temp.path()).unwrap();
            let cwd = root.join("repo");
            std::fs::create_dir_all(&cwd).unwrap();
            let files = if let Some(info) = case["input"].get("virtual") {
                vec![MemoryFile {
                    source_content: None,
                    path: info["path"].as_str().unwrap().into(),
                    parent: None,
                    body: info["content"].as_str().unwrap().into(),
                    is_local_override: false,
                    tier: memory::lingxi_md::LingxiMdTier::Managed,
                    globs: Some(Vec::new()),
                    raw_content: info["rawContent"].as_str().unwrap().into(),
                    content_differs_from_disk: true,
                }]
            } else {
                let path = if case["input"]["origin"] == "rules" {
                    cwd.join(branding::DOT_DIR).join("rules/scoped.md")
                } else {
                    cwd.join(branding::MEMORY_FILE)
                };
                write(&path, case["input"]["raw"].as_str().unwrap());
                if let Some(child) = case["input"].get("childRaw") {
                    write(&cwd.join("imported.md"), child.as_str().unwrap());
                }
                load_memory_files_at_with_user_config_dir(
                    &cwd,
                    &root.join("home"),
                    &root.join("home").join(branding::DOT_DIR),
                    &root.join("managed"),
                    None,
                    InstructionFilesMode::LingxiMd,
                    false,
                )
            };
            let replace = |text: &str| text.replace("/repo", &cwd.display().to_string());
            let expected = case["expected"]["acquired"].as_array().unwrap();
            assert_eq!(files.len(), expected.len(), "{}", case["name"]);
            for (file, native) in files.iter().zip(expected) {
                assert_eq!(
                    file.path,
                    std::path::PathBuf::from(replace(native["path"].as_str().unwrap()))
                );
                assert_eq!(file.body, native["content"].as_str().unwrap());
                assert_eq!(serde_json::to_value(file.tier).unwrap(), native["type"]);
                assert_eq!(
                    serde_json::to_value(&file.globs).unwrap(),
                    native
                        .get("globs")
                        .cloned()
                        .unwrap_or(serde_json::Value::Null)
                );
                assert_eq!(
                    file.content_differs_from_disk,
                    native["contentDiffersFromDisk"].as_bool().unwrap()
                );
                assert_eq!(
                    file.parent.as_ref().map(|path| path.display().to_string()),
                    native
                        .get("parent")
                        .and_then(serde_json::Value::as_str)
                        .map(replace)
                );
                if let Some(raw) = native.get("rawContent") {
                    assert_eq!(file.raw_content, raw.as_str().unwrap());
                }
            }
            assert_eq!(
                format(&files),
                replace(case["expected"]["rendered"].as_str().unwrap())
            );
            let descriptors = instruction_file_descriptors(&files);
            let mut expected_descriptors = case["expected"]["descriptors"].clone();
            for file in expected_descriptors.as_array_mut().unwrap() {
                file["path"] = serde_json::Value::String(replace(file["path"].as_str().unwrap()));
            }
            assert_eq!(
                serde_json::to_value(descriptors).unwrap(),
                expected_descriptors
            );
        }
    }

    #[test]
    fn eager_rules_filter_conditional_projections_without_filtering_named_files() {
        let temp = tempfile::tempdir().unwrap();
        let root = std::fs::canonicalize(temp.path()).unwrap();
        let cwd = root.join("repo");
        let named = cwd.join(branding::MEMORY_FILE);
        let rule = cwd.join(branding::DOT_DIR).join("rules/scoped.md");
        let imported = cwd.join("imported.md");
        write(&named, "---\npaths: src/**\n---\nNamed guide\n");
        write(&imported, "  Imported guide\n");
        write(&rule, "---\npaths: src/**\n---\n@../../imported.md\n");
        let home = root.join("home");
        let files = load_memory_files_at_with_user_config_dir(
            &cwd,
            &home,
            &home.join(branding::DOT_DIR),
            &root.join("managed"),
            None,
            InstructionFilesMode::LingxiMd,
            false,
        );
        assert_eq!(files.len(), 2);
        let top = files.iter().find(|file| file.path == named).unwrap();
        assert_eq!(top.body, "Named guide\n");
        assert_eq!(top.globs.as_deref(), Some(["src".to_owned()].as_slice()));
        assert!(!files.iter().any(|file| file.path == rule));
        let child = files.iter().find(|file| file.path == imported).unwrap();
        assert_eq!(child.body, "  Imported guide\n");
        assert_eq!(child.parent.as_ref(), Some(&rule));
    }

    #[test]
    fn named_instruction_imports_keep_their_conditional_acquisition_metadata() {
        let temp = tempfile::tempdir().unwrap();
        let root = std::fs::canonicalize(temp.path()).unwrap();
        let cwd = root.join("repo");
        let named = cwd.join(branding::MEMORY_FILE);
        let child = cwd.join("conditional.md");
        write(&named, "Top guide\n@./conditional.md\n");
        write(&child, "---\npaths: src/**\n---\n  Child guide\n");
        let home = root.join("home");
        let files = load_memory_files_at_with_user_config_dir(
            &cwd,
            &home,
            &home.join(branding::DOT_DIR),
            &root.join("managed"),
            None,
            InstructionFilesMode::LingxiMd,
            false,
        );
        assert_eq!(files.len(), 2);
        let imported = files.iter().find(|file| file.path == child).unwrap();
        assert_eq!(imported.body, "Child guide\n");
        assert_eq!(imported.parent.as_ref(), Some(&named));
        assert_eq!(
            imported.globs.as_deref(),
            Some(["src".to_owned()].as_slice())
        );
    }
}
