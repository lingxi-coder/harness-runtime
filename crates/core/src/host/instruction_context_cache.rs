//! Explicitly root-owned instruction caches matching native Gv and qb.
//!
//! Context invalidation and eager-file invalidation are separate operations.
//! Producers run in detached tasks, so dropping a waiter does not erase the
//! shared Promise, including its rejected result. No root is inferred from cwd.

use super::instructions::{
    InstructionContext, InstructionContextKey, InstructionFile, InstructionRefreshReason,
    InstructionScope,
};
use indexmap::IndexMap;
use std::future::Future;
use std::sync::{Arc, Mutex};
use tokio::sync::watch;

/// An eager-file Promise key inside one root. Cwd is loader provenance only.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub struct InstructionFileCacheKey {
    /// The host's current managed-policy restriction.
    pub scope: InstructionScope,
    /// Native qb keeps external and ordinary traversal in distinct slots.
    pub force_external: bool,
}

/// Hook reasons are distinct from user-context attachment refresh reasons.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum EagerInstructionLoadReason {
    #[default]
    SessionStart,
    Compact,
}

/// Actual plugin-file acquisition facts supplied by a context loader.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct InstructionPluginFileSummary {
    /// Successfully acquired files.
    pub file_count: usize,
    /// Native JavaScript content length, in UTF-16 code units.
    pub total_content_length: usize,
}

/// All projections of one successful user-context build.
pub struct InstructionContextBuild<F> {
    /// Frozen context returned to every waiter of this build.
    pub context: Arc<InstructionContext>,
    /// The same file snapshot used to construct the context.
    pub memory_files: Arc<Vec<F>>,
    /// Descriptor identity is retained independently of context rendering.
    pub descriptors: Option<Arc<Vec<InstructionFile>>>,
    /// A real classifier projection, when the host produced one.
    pub classifier_context: Option<String>,
    /// A real plugin acquisition result, when the host produced one.
    pub plugin_instruction_files: Option<InstructionPluginFileSummary>,
}

impl<F> InstructionContextBuild<F> {
    /// Freeze the context and its successful eager descriptor snapshot.
    #[must_use]
    pub fn new(context: InstructionContext, memory_files: Arc<Vec<F>>) -> Self {
        let descriptors = context.eager_instructions.clone().map(Arc::new);
        Self {
            context: Arc::new(context),
            memory_files,
            descriptors,
            classifier_context: None,
            plugin_instruction_files: None,
        }
    }

    /// Attach the classifier result actually produced by this build.
    #[must_use]
    pub fn with_classifier_context(mut self, context: Option<String>) -> Self {
        self.classifier_context = context;
        self
    }

    /// Attach actual plugin acquisition facts to the native one-time gate.
    #[must_use]
    pub fn with_plugin_instruction_files(
        mut self,
        files: Option<InstructionPluginFileSummary>,
    ) -> Self {
        self.plugin_instruction_files = files;
        self
    }
}

/// A completion accepted by the native classifier ordinal fence.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct InstructionContextPublication {
    /// Monotonically accepted build ordinal within this root.
    pub ordinal: u64,
    /// The loader's classifier projection, including an explicit absent value.
    pub classifier_context: Option<String>,
    /// Plugin telemetry is present only on its first nonempty accepted build.
    pub plugin_instruction_files: Option<InstructionPluginFileSummary>,
}

/// Optional host effects for real classifier/project-cache capabilities.
/// Callbacks run synchronously in native completion order and must not reenter
/// this cache. They never execute while awaiting an asynchronous loader.
pub trait InstructionContextObserver: Send + Sync {
    /// Publish a completion accepted by the root ordinal fence.
    fn context_completed(&self, publication: InstructionContextPublication);
    /// Purge project context; account changes additionally purge account detail.
    /// `None` is the native clear-scope operation.
    fn context_invalidated(&self, reason: Option<InstructionRefreshReason>);
}

type BuildResult<T> = Option<Result<Arc<T>, String>>;

struct SharedBuild<T> {
    result: watch::Receiver<BuildResult<T>>,
}

impl<T> SharedBuild<T> {
    fn pending() -> (Arc<Self>, watch::Sender<BuildResult<T>>) {
        let (sender, result) = watch::channel(None);
        (Arc::new(Self { result }), sender)
    }

    async fn get(&self) -> Result<Arc<T>, String> {
        let mut receiver = self.result.clone();
        loop {
            if let Some(result) = receiver.borrow_and_update().clone() {
                return result;
            }
            receiver
                .changed()
                .await
                .map_err(|_| "instruction cache producer terminated without a result".to_owned())?;
        }
    }
}

/// A shared eager-file build; its identity survives successful and failed loads.
pub struct InstructionFileLoad<F> {
    build: Arc<SharedBuild<Vec<F>>>,
}

impl<F> Clone for InstructionFileLoad<F> {
    fn clone(&self) -> Self {
        Self {
            build: self.build.clone(),
        }
    }
}

impl<F> InstructionFileLoad<F> {
    /// Await the original successful snapshot or original sticky failure.
    pub async fn get(&self) -> Result<Arc<Vec<F>>, String> {
        self.build.get().await
    }

    /// Compare the identity of the native shared Promise.
    #[must_use]
    pub fn same_build(&self, other: &Self) -> bool {
        Arc::ptr_eq(&self.build, &other.build)
    }
}

/// One shared context build and all of its native projections.
pub struct InstructionContextLoad<F> {
    build: Arc<SharedBuild<InstructionContextBuild<F>>>,
}

impl<F> Clone for InstructionContextLoad<F> {
    fn clone(&self) -> Self {
        Self {
            build: self.build.clone(),
        }
    }
}

impl<F> InstructionContextLoad<F> {
    /// Await the frozen context or its original sticky failure.
    pub async fn get(&self) -> Result<Arc<InstructionContext>, String> {
        self.build.get().await.map(|build| build.context.clone())
    }

    /// Native descriptor projection converts a failed context into absence.
    pub async fn descriptors(&self) -> Option<Arc<Vec<InstructionFile>>> {
        self.build
            .get()
            .await
            .ok()
            .and_then(|build| build.descriptors.clone())
    }

    /// Native memory projection converts a failed context into an empty array.
    pub async fn memory_files(&self) -> Arc<Vec<F>> {
        self.build
            .get()
            .await
            .map_or_else(|_| Arc::new(Vec::new()), |build| build.memory_files.clone())
    }

    /// Compare the identity of the native shared Promise.
    #[must_use]
    pub fn same_build(&self, other: &Self) -> bool {
        Arc::ptr_eq(&self.build, &other.build)
    }
}

struct ContextEntry<F> {
    load: Option<InstructionContextLoad<F>>,
    reason: InstructionRefreshReason,
}

impl<F> Default for ContextEntry<F> {
    fn default() -> Self {
        Self {
            load: None,
            reason: InstructionRefreshReason::SessionStart,
        }
    }
}

struct EagerState<F> {
    files: IndexMap<InstructionFileCacheKey, InstructionFileLoad<F>>,
    next_reason: EagerInstructionLoadReason,
    should_fire_hook: bool,
    walk_epoch: u64,
}

impl<F> Default for EagerState<F> {
    fn default() -> Self {
        Self {
            files: IndexMap::new(),
            next_reason: EagerInstructionLoadReason::SessionStart,
            should_fire_hook: true,
            walk_epoch: 0,
        }
    }
}

struct RootState<F> {
    contexts: IndexMap<InstructionContextKey, ContextEntry<F>>,
    builds_started: u64,
    classifier_ordinal: u64,
    logged_plugin_files: bool,
    eager: Option<EagerState<F>>,
}

impl<F> Default for RootState<F> {
    fn default() -> Self {
        Self {
            contexts: IndexMap::new(),
            builds_started: 0,
            classifier_ordinal: 0,
            logged_plugin_files: false,
            eager: None,
        }
    }
}

/// The composition root owns one Arc of this cache and shares it explicitly.
/// Different Arc instances remain isolated even when they load the same cwd.
pub struct InstructionContextCache<F> {
    state: Mutex<RootState<F>>,
    observer: Option<Arc<dyn InstructionContextObserver>>,
}

impl<F> Default for InstructionContextCache<F> {
    fn default() -> Self {
        Self::new()
    }
}

impl<F> InstructionContextCache<F> {
    /// Construct an independent root cache with no invented host side effects.
    #[must_use]
    pub fn new() -> Self {
        Self {
            state: Mutex::new(RootState::default()),
            observer: None,
        }
    }

    /// Construct an independent root backed by real host projection callbacks.
    #[must_use]
    pub fn with_observer(observer: Arc<dyn InstructionContextObserver>) -> Self {
        Self {
            state: Mutex::new(RootState::default()),
            observer: Some(observer),
        }
    }

    /// Ensure the current session identity exists without initiating a build.
    pub fn ensure_entry(&self, key: InstructionContextKey) {
        self.state
            .lock()
            .expect("instruction cache poisoned")
            .contexts
            .entry(key)
            .or_default();
    }

    /// Read the current refresh reason without creating an entry.
    #[must_use]
    pub fn reason(&self, key: InstructionContextKey) -> InstructionRefreshReason {
        self.cached_reason(key).unwrap_or_default()
    }

    /// Read native ibo absence for a session which has never had an entry.
    #[must_use]
    pub fn cached_reason(&self, key: InstructionContextKey) -> Option<InstructionRefreshReason> {
        self.state
            .lock()
            .expect("instruction cache poisoned")
            .contexts
            .get(&key)
            .map(|entry| entry.reason)
    }

    /// Invalidate all contexts under this root, preserving the independent qb
    /// cache. An unloaded entry keeps its first invalidation reason. This does
    /// not fence a pending classifier completion.
    pub fn invalidate_context(&self, reason: InstructionRefreshReason) {
        let mut state = self.state.lock().expect("instruction cache poisoned");
        self.invalidate_context_locked(&mut state, reason);
    }

    fn invalidate_context_locked(
        &self,
        state: &mut RootState<F>,
        reason: InstructionRefreshReason,
    ) {
        if let Some(observer) = &self.observer {
            observer.context_invalidated(Some(reason));
        }
        for entry in state.contexts.values_mut() {
            if entry.load.take().is_some() {
                entry.reason = reason;
            }
        }
    }

    /// Retire Gv and qb atomically for native events that execute WC then pT.
    /// A concurrent new build cannot retain a file slot from half of the event.
    pub fn invalidate_context_with_files(&self, reason: InstructionRefreshReason) {
        let mut state = self.state.lock().expect("instruction cache poisoned");
        state
            .eager
            .get_or_insert_with(EagerState::default)
            .files
            .clear();
        self.invalidate_context_locked(&mut state, reason);
    }

    /// Clear eager-file Promises only; existing Gv contexts remain frozen.
    pub fn clear_files(&self) {
        self.state
            .lock()
            .expect("instruction cache poisoned")
            .eager
            .get_or_insert_with(EagerState::default)
            .files
            .clear();
    }

    /// Clear context identities and fence every build already started. The
    /// eager-file cache and plugin telemetry gate are deliberately retained.
    pub fn clear_scope(&self) {
        let mut state = self.state.lock().expect("instruction cache poisoned");
        self.clear_scope_locked(&mut state);
    }

    fn clear_scope_locked(&self, state: &mut RootState<F>) {
        if let Some(observer) = &self.observer {
            observer.context_invalidated(None);
        }
        state.contexts.clear();
        state.classifier_ordinal = state.builds_started;
    }

    /// Native compaction executes pT("compaction") followed by KPt("compact")
    /// without an asynchronous boundary. Hold one root lock for that event so
    /// no context build can retain a file Promise from its intermediate state.
    /// Pending older completions remain eligible for classifier publication.
    pub fn apply_compaction(&self) {
        let mut state = self.state.lock().expect("instruction cache poisoned");
        self.invalidate_context_locked(&mut state, InstructionRefreshReason::Compaction);
        Self::reset_eager_locked(&mut state, EagerInstructionLoadReason::Compact);
    }

    /// Native fresh-session handling executes Ctn then KPt("session_start")
    /// as one synchronous event. Fence every already-started classifier build
    /// while retaining pending results for their original callers and the
    /// root's once-only plugin telemetry gate.
    pub fn reset_session_context(&self) {
        let mut state = self.state.lock().expect("instruction cache poisoned");
        self.clear_scope_locked(&mut state);
        Self::reset_eager_locked(&mut state, EagerInstructionLoadReason::SessionStart);
    }

    /// Return only a settled result from the active context slot. Detached old
    /// builds still resolve to their waiters but cannot appear in this view.
    #[must_use]
    pub fn current_context(
        &self,
        key: InstructionContextKey,
    ) -> Option<Result<Arc<InstructionContext>, String>> {
        let state = self.state.lock().expect("instruction cache poisoned");
        let load = state.contexts.get(&key)?.load.as_ref()?;
        let result = load.build.result.borrow().clone();
        result.map(|result| result.map(|build| build.context.clone()))
    }

    /// Check authority after an awaited detached build before updating a live
    /// orchestrator cursor or session-owned prompt state.
    #[must_use]
    pub fn is_current_context(
        &self,
        key: InstructionContextKey,
        context: &Arc<InstructionContext>,
    ) -> bool {
        self.current_context(key)
            .is_some_and(|result| result.is_ok_and(|current| Arc::ptr_eq(&current, context)))
    }

    /// Native memory-state transitions clear qb and Gv without rearming hooks.
    pub fn memory_state_changed(&self, reason: InstructionRefreshReason) {
        self.invalidate_context_with_files(reason);
    }

    /// Rearm hook traversal and clear qb without invalidating a Gv context.
    pub fn reset_eager(&self, reason: EagerInstructionLoadReason) {
        let mut state = self.state.lock().expect("instruction cache poisoned");
        Self::reset_eager_locked(&mut state, reason);
    }

    fn reset_eager_locked(state: &mut RootState<F>, reason: EagerInstructionLoadReason) {
        let eager = state.eager.get_or_insert_with(EagerState::default);
        eager.next_reason = reason;
        eager.should_fire_hook = true;
        eager.walk_epoch += 1;
        eager.files.clear();
    }

    /// Consume the current eager hook reason once; next rearm starts at startup.
    #[must_use]
    pub fn consume_eager_load_reason(&self) -> Option<EagerInstructionLoadReason> {
        let mut state = self.state.lock().expect("instruction cache poisoned");
        let eager = state.eager.get_or_insert_with(EagerState::default);
        if !eager.should_fire_hook {
            return None;
        }
        eager.should_fire_hook = false;
        Some(std::mem::take(&mut eager.next_reason))
    }

    /// Reset the root's once-only plugin acquisition telemetry gate.
    pub fn reset_plugin_log(&self) {
        self.state
            .lock()
            .expect("instruction cache poisoned")
            .logged_plugin_files = false;
    }

    /// Return the last accepted classifier completion or clear-scope fence.
    #[must_use]
    pub fn classifier_ordinal(&self) -> u64 {
        self.state
            .lock()
            .expect("instruction cache poisoned")
            .classifier_ordinal
    }

    /// Return all started context builds, including invalidated pending builds.
    #[must_use]
    pub fn builds_started(&self) -> u64 {
        self.state
            .lock()
            .expect("instruction cache poisoned")
            .builds_started
    }
}

impl<F: Send + Sync + 'static> InstructionContextCache<F> {
    /// Begin or reuse the exact eager-file Promise for this current key.
    pub fn begin_files<L, Fut>(
        self: &Arc<Self>,
        key: InstructionFileCacheKey,
        loader: L,
    ) -> InstructionFileLoad<F>
    where
        L: FnOnce() -> Fut,
        Fut: Future<Output = Result<Vec<F>, String>> + Send + 'static,
    {
        let mut state = self.state.lock().expect("instruction cache poisoned");
        let eager = state.eager.get_or_insert_with(EagerState::default);
        if let Some(load) = eager.files.get(&key) {
            return load.clone();
        }
        let (build, sender) = SharedBuild::pending();
        let load = InstructionFileLoad { build };
        eager.files.insert(key, load.clone());
        drop(state);
        let future = loader();
        tokio::spawn(async move {
            sender.send_replace(Some(future.await.map(Arc::new)));
        });
        load
    }

    /// Await an eager-file Promise without owning the lifetime of its producer.
    pub async fn load_files<L, Fut>(
        self: &Arc<Self>,
        key: InstructionFileCacheKey,
        loader: L,
    ) -> Result<Arc<Vec<F>>, String>
    where
        L: FnOnce() -> Fut,
        Fut: Future<Output = Result<Vec<F>, String>> + Send + 'static,
    {
        self.begin_files(key, loader).get().await
    }

    /// Begin or reuse one context Promise. All successful projections share this
    /// build, and its failure remains cached until explicit invalidation.
    pub fn begin_context<L, Fut>(
        self: &Arc<Self>,
        key: InstructionContextKey,
        loader: L,
    ) -> InstructionContextLoad<F>
    where
        L: FnOnce() -> Fut,
        Fut: Future<Output = Result<InstructionContextBuild<F>, String>> + Send + 'static,
    {
        let mut state = self.state.lock().expect("instruction cache poisoned");
        let entry = state.contexts.entry(key).or_default();
        if let Some(load) = &entry.load {
            return load.clone();
        }
        let (build, sender) = SharedBuild::pending();
        let load = InstructionContextLoad { build };
        entry.load = Some(load.clone());
        state.builds_started += 1;
        let ordinal = state.builds_started;
        drop(state);
        let future = loader();
        self.spawn_context(ordinal, future, sender);
        load
    }

    /// Reserve a Gv build and its qb snapshot in one synchronous transaction.
    /// The context builder receives the reserved handle even if an invalidation
    /// event runs before either detached producer is first polled.
    pub fn begin_context_with_files<LF, FF, LC, FC>(
        self: &Arc<Self>,
        key: InstructionContextKey,
        file_key: InstructionFileCacheKey,
        file_loader: LF,
        context_builder: LC,
    ) -> InstructionContextLoad<F>
    where
        LF: FnOnce() -> FF,
        FF: Future<Output = Result<Vec<F>, String>> + Send + 'static,
        LC: FnOnce(InstructionFileLoad<F>) -> FC,
        FC: Future<Output = Result<InstructionContextBuild<F>, String>> + Send + 'static,
    {
        let mut state = self.state.lock().expect("instruction cache poisoned");
        let entry = state.contexts.entry(key).or_default();
        if let Some(load) = &entry.load {
            return load.clone();
        }
        let (build, context_sender) = SharedBuild::pending();
        let context = InstructionContextLoad { build };
        entry.load = Some(context.clone());
        state.builds_started += 1;
        let ordinal = state.builds_started;
        let eager = state.eager.get_or_insert_with(EagerState::default);
        let (files, file_sender) = match eager.files.get(&file_key) {
            Some(load) => (load.clone(), None),
            None => {
                let (build, sender) = SharedBuild::pending();
                let load = InstructionFileLoad { build };
                eager.files.insert(file_key, load.clone());
                (load, Some(sender))
            }
        };
        drop(state);
        if let Some(sender) = file_sender {
            let future = file_loader();
            tokio::spawn(async move {
                sender.send_replace(Some(future.await.map(Arc::new)));
            });
        }
        self.spawn_context(ordinal, context_builder(files), context_sender);
        context
    }

    /// Await the atomically reserved context/file build while leaving its
    /// producer detached from this waiter's cancellation lifetime.
    pub async fn load_context_with_files<LF, FF, LC, FC>(
        self: &Arc<Self>,
        key: InstructionContextKey,
        file_key: InstructionFileCacheKey,
        file_loader: LF,
        context_builder: LC,
    ) -> Result<Arc<InstructionContext>, String>
    where
        LF: FnOnce() -> FF,
        FF: Future<Output = Result<Vec<F>, String>> + Send + 'static,
        LC: FnOnce(InstructionFileLoad<F>) -> FC,
        FC: Future<Output = Result<InstructionContextBuild<F>, String>> + Send + 'static,
    {
        self.begin_context_with_files(key, file_key, file_loader, context_builder)
            .get()
            .await
    }

    fn spawn_context<FC>(
        self: &Arc<Self>,
        ordinal: u64,
        future: FC,
        sender: watch::Sender<BuildResult<InstructionContextBuild<F>>>,
    ) where
        FC: Future<Output = Result<InstructionContextBuild<F>, String>> + Send + 'static,
    {
        let cache = self.clone();
        tokio::spawn(async move {
            let result = future.await.map(Arc::new);
            if let Ok(build) = &result {
                let mut state = cache.state.lock().expect("instruction cache poisoned");
                if ordinal > state.classifier_ordinal {
                    state.classifier_ordinal = ordinal;
                    let plugin_instruction_files =
                        build.plugin_instruction_files.and_then(|files| {
                            if state.logged_plugin_files {
                                None
                            } else {
                                state.logged_plugin_files = true;
                                (files.file_count > 0).then_some(files)
                            }
                        });
                    if let Some(observer) = &cache.observer {
                        observer.context_completed(InstructionContextPublication {
                            ordinal,
                            classifier_context: build.classifier_context.clone(),
                            plugin_instruction_files,
                        });
                    }
                }
            }
            sender.send_replace(Some(result));
        });
    }

    /// Await a context Promise without owning the lifetime of its producer.
    pub async fn load_context<L, Fut>(
        self: &Arc<Self>,
        key: InstructionContextKey,
        loader: L,
    ) -> Result<Arc<InstructionContext>, String>
    where
        L: FnOnce() -> Fut,
        Fut: Future<Output = Result<InstructionContextBuild<F>, String>> + Send + 'static,
    {
        self.begin_context(key, loader).get().await
    }

    /// Read the current cached descriptor projection without starting a build.
    pub async fn descriptors(
        &self,
        key: InstructionContextKey,
    ) -> Option<Arc<Vec<InstructionFile>>> {
        let load = self
            .state
            .lock()
            .expect("instruction cache poisoned")
            .contexts
            .get(&key)
            .and_then(|entry| entry.load.clone());
        match load {
            Some(load) => load.descriptors().await,
            None => None,
        }
    }

    /// Read the current cached memory projection without starting a build.
    pub async fn memory_files(&self, key: InstructionContextKey) -> Arc<Vec<F>> {
        let load = self
            .state
            .lock()
            .expect("instruction cache poisoned")
            .contexts
            .get(&key)
            .and_then(|entry| entry.load.clone());
        match load {
            Some(load) => load.memory_files().await,
            None => Arc::new(Vec::new()),
        }
    }
}

#[cfg(test)]
#[path = "instruction_context_cache_test.rs"]
mod tests;
