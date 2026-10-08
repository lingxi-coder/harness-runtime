//! Replay actual native Gv/qb operations with explicitly controlled host loaders.

use super::*;
use crate::types::SessionId;
use serde_json::{json, Value};
use std::collections::{BTreeMap, HashMap};
use tokio::sync::oneshot;

#[derive(Default)]
struct Effects {
    classifier_publications: Vec<Option<String>>,
    telemetry: Vec<Value>,
    project_cache_purges: Vec<String>,
    account_project_detail_purges: Vec<String>,
}

struct FixtureObserver {
    root: String,
    effects: Arc<Mutex<Effects>>,
}

impl InstructionContextObserver for FixtureObserver {
    fn context_completed(&self, publication: InstructionContextPublication) {
        let mut effects = self.effects.lock().unwrap();
        effects
            .classifier_publications
            .push(publication.classifier_context);
        if let Some(files) = publication.plugin_instruction_files {
            effects.telemetry.push(json!({
                "event": "tengu_instruction_files_from_plugins",
                "payload": {
                    "file_count": files.file_count,
                    "total_content_length": files.total_content_length,
                },
            }));
        }
    }

    fn context_invalidated(&self, reason: Option<InstructionRefreshReason>) {
        let mut effects = self.effects.lock().unwrap();
        effects.project_cache_purges.push(self.root.clone());
        if reason == Some(InstructionRefreshReason::AccountChange) {
            effects
                .account_project_detail_purges
                .push(self.root.clone());
        }
    }
}

struct FileRecord {
    snapshot: Vec<InstructionFile>,
    sender: Option<oneshot::Sender<Result<Vec<InstructionFile>, String>>>,
}

#[derive(Default)]
struct Loaders {
    disk: HashMap<String, Vec<InstructionFile>>,
    contexts: HashMap<String, Value>,
    files: Vec<FileRecord>,
    contexts_pending: Vec<Option<oneshot::Sender<Result<(), String>>>>,
}

#[derive(Clone)]
struct FixtureSession {
    root: String,
    native_id: String,
    key: InstructionContextKey,
}

enum Handle {
    Context(InstructionContextLoad<InstructionFile>),
    Files(InstructionFileLoad<InstructionFile>),
}

impl Handle {
    fn same_build(&self, other: &Self) -> bool {
        match (self, other) {
            (Self::Context(left), Self::Context(right)) => left.same_build(right),
            (Self::Files(left), Self::Files(right)) => left.same_build(right),
            _ => false,
        }
    }
}

struct Fixture {
    roots: HashMap<String, Arc<InstructionContextCache<InstructionFile>>>,
    sessions: HashMap<String, FixtureSession>,
    first_session: String,
    loaders: Arc<Mutex<Loaders>>,
    effects: Arc<Mutex<Effects>>,
    handles: HashMap<String, Handle>,
    values: HashMap<String, Option<Arc<Vec<InstructionFile>>>>,
    subscriptions: HashMap<String, String>,
}

impl Fixture {
    fn new(case: &Value) -> Self {
        let effects = Arc::new(Mutex::new(Effects::default()));
        let roots = case["roots"]
            .as_array()
            .unwrap()
            .iter()
            .map(|root| {
                let root = root.as_str().unwrap().to_owned();
                let cache = Arc::new(InstructionContextCache::with_observer(Arc::new(
                    FixtureObserver {
                        root: root.clone(),
                        effects: effects.clone(),
                    },
                )));
                (root, cache)
            })
            .collect();
        let mut ids = HashMap::new();
        let mut sessions = HashMap::new();
        for session in case["sessions"].as_array().unwrap() {
            let native_id = session["id"].as_str().unwrap().to_owned();
            let ordinal = ids.len() as u128 + 1;
            let session_id = *ids
                .entry(native_id.clone())
                .or_insert_with(|| SessionId::from_uuid(uuid::Uuid::from_u128(ordinal)));
            sessions.insert(
                session["key"].as_str().unwrap().to_owned(),
                FixtureSession {
                    root: session["root"].as_str().unwrap().to_owned(),
                    native_id,
                    key: InstructionContextKey {
                        session_id,
                        agent_id: None,
                    },
                },
            );
        }
        Self {
            roots,
            sessions,
            first_session: case["sessions"][0]["key"].as_str().unwrap().to_owned(),
            loaders: Arc::new(Mutex::new(Loaders::default())),
            effects,
            handles: HashMap::new(),
            values: HashMap::new(),
            subscriptions: HashMap::new(),
        }
    }

    fn pending_files(
        root: String,
        loaders: Arc<Mutex<Loaders>>,
    ) -> impl Future<Output = Result<Vec<InstructionFile>, String>> + Send {
        let (sender, receiver) = oneshot::channel();
        let mut state = loaders.lock().unwrap();
        let snapshot = state.disk.get(&root).cloned().unwrap_or_default();
        state.files.push(FileRecord {
            snapshot,
            sender: Some(sender),
        });
        async move { receiver.await.expect("fixture file event sender retained") }
    }

    fn begin_files(
        cache: &Arc<InstructionContextCache<InstructionFile>>,
        root: String,
        force_external: bool,
        loaders: Arc<Mutex<Loaders>>,
    ) -> InstructionFileLoad<InstructionFile> {
        cache.begin_files(
            InstructionFileCacheKey {
                scope: InstructionScope::Full,
                force_external,
            },
            move || Self::pending_files(root, loaders),
        )
    }

    fn begin_context(&self, session: &FixtureSession) -> InstructionContextLoad<InstructionFile> {
        let cache = self.roots[&session.root].clone();
        let root = session.root.clone();
        let loaders = self.loaders.clone();
        let file_root = root.clone();
        let file_loaders = loaders.clone();
        cache.begin_context_with_files(
            session.key,
            InstructionFileCacheKey {
                scope: InstructionScope::Full,
                force_external: false,
            },
            move || Self::pending_files(file_root, file_loaders),
            move |files| {
                let (sender, receiver) = oneshot::channel();
                let snapshot = {
                    let mut state = loaders.lock().unwrap();
                    let snapshot = state
                        .contexts
                        .get(&root)
                        .cloned()
                        .unwrap_or_else(|| json!({}));
                    state.contexts_pending.push(Some(sender));
                    snapshot
                };
                async move {
                    let released = async move {
                        receiver
                            .await
                            .expect("fixture context event sender retained")
                    };
                    let (_, files) = tokio::try_join!(released, files.get())?;
                    let body = files
                        .iter()
                        .map(|file| file.content.as_str())
                        .collect::<Vec<_>>()
                        .join("\n");
                    let mut blocks: BTreeMap<String, String> = snapshot
                        .get("blocks")
                        .cloned()
                        .map(serde_json::from_value)
                        .transpose()
                        .unwrap()
                        .unwrap_or_default();
                    blocks.insert("instructions".to_owned(), body.clone());
                    let context = InstructionContext {
                        user_context: blocks,
                        eager_instructions: Some(files.as_ref().clone()),
                        ..InstructionContext::default()
                    };
                    let plugin = snapshot.get("plugin_instruction_files").map(|value| {
                        InstructionPluginFileSummary {
                            file_count: value["fileCount"].as_u64().unwrap() as usize,
                            total_content_length: value["totalContentLength"].as_u64().unwrap()
                                as usize,
                        }
                    });
                    Ok(InstructionContextBuild::new(context, files)
                        .with_classifier_context((!body.is_empty()).then_some(body))
                        .with_plugin_instruction_files(plugin))
                }
            },
        )
    }

    fn inspect(&self, session: &FixtureSession) -> Value {
        let cache = &self.roots[&session.root];
        let state = cache.state.lock().unwrap();
        let entry = state.contexts.get(&session.key);
        let ids: Vec<_> = state
            .contexts
            .keys()
            .map(|key| {
                self.sessions
                    .values()
                    .find(|value| value.root == session.root && value.key == *key)
                    .unwrap()
                    .native_id
                    .clone()
            })
            .collect();
        let keys: Vec<_> = state
            .eager
            .as_ref()
            .map(|eager| eager.files.keys().map(|key| key.force_external).collect())
            .unwrap_or_default();
        let effects = self.effects.lock().unwrap();
        let loaders = self.loaders.lock().unwrap();
        json!({
            "refresh_reason": entry.map(|entry| entry.reason),
            "context_cached": entry.is_some_and(|entry| entry.load.is_some()),
            "memory_files_cached": entry.is_some_and(|entry| entry.load.is_some()),
            "descriptors_cached": entry.is_some_and(|entry| entry.load.is_some()),
            "scope_session_ids": ids,
            "context_builds_started": state.builds_started,
            "classifier_build_ordinal": state.classifier_ordinal,
            "logged_plugin_instruction_files": state.logged_plugin_files,
            "eager_cache_keys": keys,
            "eager_next_load_reason": state.eager.as_ref().map(|eager| eager.next_reason),
            "eager_should_fire_hook": state.eager.as_ref().map(|eager| eager.should_fire_hook),
            "eager_walk_epoch": state.eager.as_ref().map_or(0, |eager| eager.walk_epoch),
            "context_load_count": loaders.contexts_pending.len(),
            "file_load_count": loaders.files.len(),
            "classifier_publications": effects.classifier_publications,
            "telemetry": effects.telemetry,
            "project_cache_purges": effects.project_cache_purges,
            "account_project_detail_purges": effects.account_project_detail_purges,
        })
    }

    async fn operation(&mut self, operation: &Value) -> Value {
        let session = self.sessions[operation
            .get("session")
            .and_then(Value::as_str)
            .unwrap_or(&self.first_session)]
        .clone();
        let cache = self.roots[&session.root].clone();
        let name = operation["op"].as_str().unwrap();
        let handle = || operation["handle"].as_str().unwrap().to_owned();
        let build = || operation["build"].as_u64().unwrap() as usize - 1;
        match name {
            "set_files" => {
                let root = operation
                    .get("root")
                    .and_then(Value::as_str)
                    .unwrap_or(&session.root);
                self.loaders.lock().unwrap().disk.insert(
                    root.to_owned(),
                    serde_json::from_value(operation["files"].clone()).unwrap(),
                );
            }
            "set_context" => {
                let root = operation
                    .get("root")
                    .and_then(Value::as_str)
                    .unwrap_or(&session.root);
                self.loaders
                    .lock()
                    .unwrap()
                    .contexts
                    .insert(root.to_owned(), operation["context"].clone());
            }
            "load" => {
                let load = self.begin_context(&session);
                self.handles.insert(handle(), Handle::Context(load));
            }
            "file_load" => {
                let load = Self::begin_files(
                    &cache,
                    session.root.clone(),
                    operation["include_external"].as_bool().unwrap_or(false),
                    self.loaders.clone(),
                );
                self.handles.insert(handle(), Handle::Files(load));
            }
            "resolve_file_load" => {
                let mut state = self.loaders.lock().unwrap();
                let record = &mut state.files[build()];
                record
                    .sender
                    .take()
                    .unwrap()
                    .send(Ok(record.snapshot.clone()))
                    .unwrap();
            }
            "reject_file_load" => {
                self.loaders.lock().unwrap().files[build()]
                    .sender
                    .take()
                    .unwrap()
                    .send(Err(operation["error"].as_str().unwrap().to_owned()))
                    .unwrap();
            }
            "release_context_load" => {
                self.loaders.lock().unwrap().contexts_pending[build()]
                    .take()
                    .unwrap()
                    .send(Ok(()))
                    .unwrap();
            }
            "reject_context_load" => {
                self.loaders.lock().unwrap().contexts_pending[build()]
                    .take()
                    .unwrap()
                    .send(Err(operation["error"].as_str().unwrap().to_owned()))
                    .unwrap();
            }
            "await_load" | "await_file_load" => {
                return match &self.handles[&handle()] {
                    Handle::Context(load) => match load.get().await {
                        Ok(context) => {
                            json!({"status": "fulfilled", "value": context.user_context})
                        }
                        Err(error) => json!({"status": "rejected", "error": error}),
                    },
                    Handle::Files(load) => match load.get().await {
                        Ok(files) => json!({"status": "fulfilled", "value": files.as_ref()}),
                        Err(error) => json!({"status": "rejected", "error": error}),
                    },
                };
            }
            "compare_handles" => {
                return json!({"same_promise": self.handles[operation["left"].as_str().unwrap()].same_build(&self.handles[operation["right"].as_str().unwrap()])});
            }
            "read_descriptors" => {
                let value = cache.descriptors(session.key).await;
                if operation.get("handle").is_some() {
                    self.values.insert(handle(), value.clone());
                }
                return json!({"value": value.as_deref()});
            }
            "read_memory_files" => {
                let load = cache
                    .state
                    .lock()
                    .unwrap()
                    .contexts
                    .get(&session.key)
                    .and_then(|entry| entry.load.clone());
                let value = match load {
                    Some(load) => Some(load.memory_files().await),
                    None => None,
                };
                if operation.get("handle").is_some() {
                    self.values.insert(handle(), value.clone());
                }
                return json!({"value": value.as_deref()});
            }
            "compare_values" => {
                let left = &self.values[operation["left"].as_str().unwrap()];
                let right = &self.values[operation["right"].as_str().unwrap()];
                let same = match (left, right) {
                    (Some(left), Some(right)) => Arc::ptr_eq(left, right),
                    (None, None) => true,
                    _ => false,
                };
                return json!({"same_identity": same});
            }
            "ensure_entry" => cache.ensure_entry(session.key),
            "invalidate_user_context" => cache
                .invalidate_context(serde_json::from_value(operation["reason"].clone()).unwrap()),
            "clear_files" => cache.clear_files(),
            "clear_scope" => cache.clear_scope(),
            "reset_eager" => {
                cache.reset_eager(serde_json::from_value(operation["reason"].clone()).unwrap())
            }
            "consume_eager_load_reason" => {
                return json!({"reason": cache.consume_eager_load_reason()});
            }
            "reset_plugin_log" => cache.reset_plugin_log(),
            "subscribe_memory_state" => {
                self.subscriptions.insert(handle(), session.root.clone());
            }
            "unsubscribe_memory_state" => {
                self.subscriptions.remove(&handle()).unwrap();
            }
            "memory_paused" | "auto_memory_disabled" => {
                let reason = match (name, operation["value"].as_bool().unwrap()) {
                    ("memory_paused", true) => InstructionRefreshReason::MemoryPaused,
                    ("memory_paused", false) => InstructionRefreshReason::MemoryResumed,
                    ("auto_memory_disabled", true) => InstructionRefreshReason::AutoMemoryOff,
                    ("auto_memory_disabled", false) => InstructionRefreshReason::AutoMemoryBackOn,
                    _ => unreachable!(),
                };
                for root in self.subscriptions.values() {
                    if *root == session.root {
                        cache.memory_state_changed(reason);
                    }
                }
            }
            "inspect" => {}
            _ => panic!("unknown native operation: {name}"),
        }
        // Flush detached producers on a single-thread runtime, without sleeps or
        // wall-clock assertions. This models the oracle's Promise microtasks.
        for _ in 0..16 {
            tokio::task::yield_now().await;
        }
        self.inspect(&session)
    }
}

#[tokio::test]
async fn current_native_fixture_replays_all_root_cache_operations() {
    // Native fixtures keep their captured bytes; project the product instruction key here.
    let fixture: Value = serde_json::from_str(
        &include_str!("../../../orchestrator/tests/fixtures/instruction_cache_2_1_286.json")
            .replace("claudeMd", "instructions"),
    )
    .unwrap();
    assert_eq!(fixture["version"], "2.1.286");
    assert_eq!(fixture["cases"].as_array().unwrap().len(), 20);
    let mut count = 0;
    for case in fixture["cases"].as_array().unwrap() {
        let mut host = Fixture::new(case);
        for (index, operation) in case["operations"].as_array().unwrap().iter().enumerate() {
            let actual =
                tokio::time::timeout(std::time::Duration::from_secs(5), host.operation(operation))
                    .await
                    .expect("released fixture operation settles");
            assert_eq!(
                actual, case["expected"][index]["result"],
                "native case {} operation {index}: {operation}",
                case["name"]
            );
            count += 1;
        }
    }
    assert_eq!(count, 300);
}

#[tokio::test]
async fn cancelling_first_waiter_keeps_the_original_detached_producer() {
    let cache = Arc::new(InstructionContextCache::<InstructionFile>::new());
    let key = InstructionContextKey {
        session_id: SessionId::new(),
        agent_id: None,
    };
    let (release, receiver) = oneshot::channel();
    let load = cache.begin_context(key, || async move {
        receiver.await.unwrap();
        Ok(InstructionContextBuild::new(
            InstructionContext::default(),
            Arc::new(Vec::new()),
        ))
    });
    let first = load.clone();
    let waiter = tokio::spawn(async move { first.get().await });
    waiter.abort();
    assert!(waiter.await.unwrap_err().is_cancelled());
    let second = cache.begin_context(key, || async { panic!("cached loader must not run") });
    assert!(load.same_build(&second));
    release.send(()).unwrap();
    let completed = second.get().await.unwrap();
    assert!(cache.is_current_context(key, &completed));
    assert_eq!(cache.builds_started(), 1);
    cache.invalidate_context(InstructionRefreshReason::PolicyRefresh);
    assert!(!cache.is_current_context(key, &completed));
}

#[tokio::test]
async fn managed_and_full_file_slots_are_distinct_current_capabilities() {
    let cache = Arc::new(InstructionContextCache::<InstructionFile>::new());
    let full = cache.begin_files(
        InstructionFileCacheKey {
            scope: InstructionScope::Full,
            force_external: false,
        },
        || async { Ok(Vec::new()) },
    );
    let managed = cache.begin_files(
        InstructionFileCacheKey {
            scope: InstructionScope::ManagedOnly,
            force_external: false,
        },
        || async { Ok(Vec::new()) },
    );
    assert!(!full.same_build(&managed));
    full.get().await.unwrap();
    managed.get().await.unwrap();
    cache.clear_files();
    let full_after_clear = cache.begin_files(
        InstructionFileCacheKey {
            scope: InstructionScope::Full,
            force_external: false,
        },
        || async { Ok(Vec::new()) },
    );
    assert!(!full.same_build(&full_after_clear));
    full_after_clear.get().await.unwrap();
}

#[tokio::test]
async fn atomic_reservation_keeps_old_files_when_refresh_precedes_first_producer_poll() {
    let cache = Arc::new(InstructionContextCache::<String>::new());
    let key = InstructionContextKey {
        session_id: SessionId::new(),
        agent_id: None,
    };
    let file_key = InstructionFileCacheKey {
        scope: InstructionScope::Full,
        force_external: false,
    };
    let refreshing_cache = cache.clone();
    let old = cache.begin_context_with_files(
        key,
        file_key,
        move || {
            // A native synchronous event occurs after the atomic reservation,
            // before either detached future can first run on this runtime.
            refreshing_cache.invalidate_context_with_files(InstructionRefreshReason::PolicyRefresh);
            async { Ok(vec!["old".to_owned()]) }
        },
        |files| async move {
            let files = files.get().await?;
            Ok(InstructionContextBuild::new(
                InstructionContext::default(),
                files,
            ))
        },
    );
    assert!(cache.current_context(key).is_none());
    assert_eq!(cache.reason(key), InstructionRefreshReason::PolicyRefresh);
    let new = cache.begin_context_with_files(
        key,
        file_key,
        || async { Ok(vec!["new".to_owned()]) },
        |files| async move {
            let files = files.get().await?;
            Ok(InstructionContextBuild::new(
                InstructionContext::default(),
                files,
            ))
        },
    );
    assert!(!old.same_build(&new));
    assert_eq!(*old.memory_files().await, vec!["old"]);
    assert_eq!(*new.memory_files().await, vec!["new"]);
    assert!(!cache.is_current_context(key, &old.get().await.unwrap()));
    assert!(cache.is_current_context(key, &new.get().await.unwrap()));
    assert_eq!(*cache.memory_files(key).await, vec!["new"]);
}

fn lifecycle_file(content: &str) -> InstructionFile {
    InstructionFile {
        path: "/fixture/instructions.md".to_owned(),
        kind: super::super::instructions::InstructionFileType::Project,
        content: content.to_owned(),
    }
}

fn lifecycle_build(files: Arc<Vec<InstructionFile>>) -> InstructionContextBuild<InstructionFile> {
    let body = files
        .iter()
        .map(|file| file.content.as_str())
        .collect::<Vec<_>>()
        .join("\n");
    let context = InstructionContext {
        eager_instructions: Some(files.as_ref().clone()),
        user_context: BTreeMap::from([("instructions".to_owned(), body.clone())]),
        ..Default::default()
    };
    InstructionContextBuild::new(context, files)
        .with_classifier_context((!body.is_empty()).then_some(body))
}

#[tokio::test]
async fn compound_lifecycles_keep_original_results_with_distinct_classifier_fences() {
    let effects = Arc::new(Mutex::new(Effects::default()));
    let cache = Arc::new(InstructionContextCache::with_observer(Arc::new(
        FixtureObserver {
            root: "root-a".to_owned(),
            effects: effects.clone(),
        },
    )));
    let key = InstructionContextKey {
        session_id: SessionId::new(),
        agent_id: None,
    };
    let file_key = InstructionFileCacheKey {
        scope: InstructionScope::Full,
        force_external: false,
    };
    let initial = cache.begin_context_with_files(
        key,
        file_key,
        || async { Ok(vec![lifecycle_file("before compaction")]) },
        |files| async move {
            Ok(
                lifecycle_build(files.get().await?).with_plugin_instruction_files(Some(
                    InstructionPluginFileSummary {
                        file_count: 2,
                        total_content_length: 12,
                    },
                )),
            )
        },
    );
    initial.get().await.unwrap();
    assert_eq!(cache.classifier_ordinal(), 1);
    assert_eq!(
        cache.consume_eager_load_reason(),
        Some(EagerInstructionLoadReason::SessionStart)
    );

    cache.invalidate_context(InstructionRefreshReason::HooksInvalidate);
    let (release_compact, pending_compact) = oneshot::channel();
    let retired_by_compaction = cache.begin_context_with_files(
        key,
        file_key,
        || async { panic!("pT retains the original qb Promise") },
        |files| async move {
            let files = files.get().await?;
            pending_compact.await.unwrap();
            Ok(lifecycle_build(files))
        },
    );
    cache.apply_compaction();
    assert!(cache.current_context(key).is_none());
    assert_eq!(cache.reason(key), InstructionRefreshReason::Compaction);
    assert_eq!(cache.classifier_ordinal(), 1);
    assert_eq!(
        cache.consume_eager_load_reason(),
        Some(EagerInstructionLoadReason::Compact)
    );
    assert_eq!(cache.consume_eager_load_reason(), None);
    {
        let state = cache.state.lock().unwrap();
        let eager = state.eager.as_ref().unwrap();
        assert!(eager.files.is_empty());
        assert_eq!(eager.walk_epoch, 1);
        assert!(state.logged_plugin_files);
    }
    release_compact.send(()).unwrap();
    let old_context = retired_by_compaction.get().await.unwrap();
    assert_eq!(
        old_context.user_context["instructions"],
        "before compaction"
    );
    assert!(!cache.is_current_context(key, &old_context));
    // Native pT does not fence a completion just because its entry was retired.
    assert_eq!(cache.classifier_ordinal(), 2);
    assert_eq!(effects.lock().unwrap().classifier_publications.len(), 2);

    let after_compaction = cache.begin_context_with_files(
        key,
        file_key,
        || async { Ok(vec![lifecycle_file("after compaction")]) },
        |files| async move { Ok(lifecycle_build(files.get().await?)) },
    );
    let after_context = after_compaction.get().await.unwrap();
    assert!(cache.is_current_context(key, &after_context));
    assert_eq!(
        *cache.memory_files(key).await,
        vec![lifecycle_file("after compaction")]
    );

    cache.invalidate_context(InstructionRefreshReason::HooksInvalidate);
    let (release_reset, pending_reset) = oneshot::channel();
    let retired_by_reset = cache.begin_context_with_files(
        key,
        file_key,
        || async { panic!("pT retains the post-compact qb Promise") },
        |files| async move {
            let files = files.get().await?;
            pending_reset.await.unwrap();
            Ok(lifecycle_build(files))
        },
    );
    cache.reset_session_context();
    assert!(cache.current_context(key).is_none());
    assert_eq!(cache.cached_reason(key), None);
    assert_eq!(cache.classifier_ordinal(), 4);
    assert_eq!(
        cache.consume_eager_load_reason(),
        Some(EagerInstructionLoadReason::SessionStart)
    );
    assert_eq!(cache.consume_eager_load_reason(), None);
    {
        let state = cache.state.lock().unwrap();
        let eager = state.eager.as_ref().unwrap();
        assert!(state.contexts.is_empty());
        assert!(eager.files.is_empty());
        assert_eq!(eager.walk_epoch, 2);
        assert!(state.logged_plugin_files);
    }
    release_reset.send(()).unwrap();
    let reset_old_context = retired_by_reset.get().await.unwrap();
    assert_eq!(
        reset_old_context.user_context["instructions"],
        "after compaction"
    );
    assert!(!cache.is_current_context(key, &reset_old_context));
    // Ctn fences the same detached completion which pT would have published.
    assert_eq!(cache.classifier_ordinal(), 4);
    assert_eq!(effects.lock().unwrap().classifier_publications.len(), 3);

    let new_key = InstructionContextKey {
        session_id: SessionId::new(),
        agent_id: None,
    };
    let fresh = cache.begin_context_with_files(
        new_key,
        file_key,
        || async { Ok(vec![lifecycle_file("new session")]) },
        |files| async move { Ok(lifecycle_build(files.get().await?)) },
    );
    let fresh_context = fresh.get().await.unwrap();
    assert!(cache.is_current_context(new_key, &fresh_context));
    assert_eq!(
        cache.reason(new_key),
        InstructionRefreshReason::SessionStart
    );
    let observed = effects.lock().unwrap();
    assert_eq!(observed.project_cache_purges, vec!["root-a"; 4]);
    assert_eq!(
        observed.classifier_publications,
        vec![
            Some("before compaction".to_owned()),
            Some("before compaction".to_owned()),
            Some("after compaction".to_owned()),
            Some("new session".to_owned()),
        ]
    );
    assert_eq!(observed.telemetry.len(), 1);
}
