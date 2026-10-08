//! Session-cached `command.describe` projection for slash menus and `/help`.

use super::runtime_impl::{
    ModCommandDescribeAnswer, ModCommandDescribeCache, ModCommandDescribeEntry,
};
use super::*;

fn valid_description(source: &serde_json::Value, answer: &serde_json::Value) -> bool {
    let Some(description) = answer
        .get("description")
        .and_then(serde_json::Value::as_str)
    else {
        return false;
    };
    if answer
        .get("isHidden")
        .and_then(serde_json::Value::as_bool)
        .is_none()
    {
        return false;
    }
    if source
        .get("description")
        .and_then(serde_json::Value::as_str)
        != Some(description)
        && description.encode_utf16().count() > 4096
    {
        return false;
    }
    match answer.get("argumentHint") {
        None => true,
        Some(hint) => hint.as_str().is_some_and(|hint| {
            source
                .get("argumentHint")
                .and_then(serde_json::Value::as_str)
                == Some(hint)
                || hint.encode_utf16().count() <= 4096
        }),
    }
}

async fn describe_once(
    host: &hooks::mods::ModHost,
    session: &dyn hooks::mods::ModSessionContext,
    output: Arc<dyn OutputStream>,
    input: serde_json::Value,
    command: &str,
) -> ModCommandDescribeAnswer {
    let pinned_command = command.to_owned();
    let pinned_immediate = input["immediate"].clone();
    let pinned_provider = input["provider"].clone();
    let source = input.clone();
    let log_output = output.clone();
    let toast_output = output.clone();
    let status_output = output;
    let answer = host
        .dispatch_with_ui_at_session(
            "command.describe",
            input.clone(),
            session,
            move |forwarded| {
                let command = pinned_command.clone();
                let immediate = pinned_immediate.clone();
                let provider = pinned_provider.clone();
                let source = source.clone();
                async move {
                    if forwarded.get("command").and_then(serde_json::Value::as_str)
                        != Some(command.as_str())
                        || forwarded.get("immediate") != Some(&immediate)
                        || forwarded.get("provider") != Some(&provider)
                    {
                        return Err(hooks::mods::ModError::Hook(
                            "command.describe command, immediate, and provider are pinned".into(),
                        ));
                    }
                    if !valid_description(&source, &forwarded) {
                        return Err(hooks::mods::ModError::Hook(
                            "command.describe needs description, optional argumentHint, and isHidden"
                                .into(),
                        ));
                    }
                    let mut result = serde_json::json!({
                        "description":forwarded["description"],
                        "isHidden":forwarded["isHidden"],
                    });
                    if let Some(hint) = forwarded.get("argumentHint") {
                        result["argumentHint"] = hint.clone();
                    }
                    Ok(result)
                }
            },
            move |plugin, text| {
                let output = log_output.clone();
                async move { output.emit_mod_log(&plugin, &text).await }
            },
            move |plugin, text, timeout_ms| {
                let output = toast_output.clone();
                async move { output.emit_mod_toast(&plugin, &text, timeout_ms).await }
            },
            move |plugin, text| {
                let output = status_output.clone();
                async move { output.emit_mod_status(&plugin, text.as_deref()).await }
            },
        )
        .await;
    let mut described = input.clone();
    let cacheable = match answer {
        Ok(answer) if valid_description(&input, &answer) => {
            described["description"] = answer["description"].clone();
            described["isHidden"] = answer["isHidden"].clone();
            if let Some(hint) = answer.get("argumentHint") {
                described["argumentHint"] = hint.clone();
            } else if let Some(object) = described.as_object_mut() {
                object.remove("argumentHint");
            }
            true
        }
        Ok(_) => {
            tracing::warn!(command, "command.describe Mod returned an invalid answer");
            false
        }
        Err(error) => {
            tracing::warn!(command, %error, "command.describe Mod failed");
            false
        }
    };
    ModCommandDescribeAnswer {
        value: described,
        cacheable,
    }
}

async fn evict_failed_command_description(
    cache: &Arc<tokio::sync::Mutex<ModCommandDescribeCache>>,
    generation: u64,
    key: &str,
    identity: (u64, u64),
    entry: &Arc<ModCommandDescribeEntry>,
) {
    let mut cache = cache.lock().await;
    if cache.generation == generation
        && cache
            .answers
            .get(key)
            .is_some_and(|(catalog, current)| *catalog == identity && Arc::ptr_eq(current, entry))
    {
        cache.answers.remove(key);
    }
}

async fn complete_command_description(
    cache: &Arc<tokio::sync::Mutex<ModCommandDescribeCache>>,
    generation: u64,
    key: &str,
    identity: (u64, u64),
    entry: &Arc<ModCommandDescribeEntry>,
    answer: ModCommandDescribeAnswer,
) {
    if !answer.cacheable {
        evict_failed_command_description(cache, generation, key, identity, entry).await;
    }
    entry.result.send_replace(Some(Arc::new(answer)));
}

async fn run_inline_command_description<F>(
    cache: &Arc<tokio::sync::Mutex<ModCommandDescribeCache>>,
    generation: u64,
    key: &str,
    identity: (u64, u64),
    entry: &Arc<ModCommandDescribeEntry>,
    dispatch: F,
) where
    F: std::future::Future<Output = ModCommandDescribeAnswer>,
{
    let _gate = entry.inline_init.lock().await;
    if entry.result.subscribe().borrow().is_none() {
        let answer = dispatch.await;
        complete_command_description(cache, generation, key, identity, entry, answer).await;
    }
}

impl ConversationOrchestrator {
    pub(crate) async fn apply_mod_command_description(
        &self,
        input: serde_json::Value,
    ) -> serde_json::Value {
        let Some(command) = input.get("command").and_then(serde_json::Value::as_str) else {
            return input;
        };
        if input
            .get("immediate")
            .and_then(serde_json::Value::as_bool)
            .is_none()
            || input.get("provider").is_none()
            || !valid_description(&input, &input)
        {
            return input;
        }
        let host = if let Some(registry) = &self.lifecycle_runtime.hook_registry {
            registry.read().await.mod_host()
        } else {
            None
        };
        let Some(host) = host.filter(|host| host.has_event("command.describe")) else {
            return input;
        };
        let key = serde_json::to_string(&input).unwrap_or_default();
        let identity = host.registration_identity();
        let (generation, entry, is_new) = {
            let mut cache = self.prompt_runtime.mod_command_descriptions.lock().await;
            let (entry, is_new) = match cache.answers.get(&key) {
                Some((catalog, entry)) if catalog == &identity => (entry.clone(), false),
                _ => {
                    let (result, _) = tokio::sync::watch::channel(None);
                    let entry = Arc::new(ModCommandDescribeEntry {
                        result,
                        inline_init: tokio::sync::Mutex::new(()),
                    });
                    cache.answers.insert(key.clone(), (identity, entry.clone()));
                    (entry, true)
                }
            };
            (cache.generation, entry, is_new)
        };
        let mut result_rx = entry.result.subscribe();
        if let Some(session) = host.bound_session() {
            if is_new {
                // The cache owns the pending Promise. Keep the equivalent
                // dispatch alive when a menu or help request stops waiting.
                let task_entry = entry.clone();
                let task_host = host.clone();
                let task_output = self.output.clone();
                let task_input = input.clone();
                let task_command = command.to_owned();
                let task_key = key.clone();
                let task_cache = self.prompt_runtime.mod_command_descriptions.clone();
                tokio::spawn(async move {
                    let answer = describe_once(
                        &task_host,
                        session.as_ref(),
                        task_output,
                        task_input,
                        &task_command,
                    )
                    .await;
                    complete_command_description(
                        &task_cache,
                        generation,
                        &task_key,
                        identity,
                        &task_entry,
                        answer,
                    )
                    .await;
                });
            }
        } else {
            // Without an owning session, each caller can only dispatch inline.
            // The entry gate lets a later caller retry if its predecessor was
            // canceled before publishing a result.
            run_inline_command_description(
                &self.prompt_runtime.mod_command_descriptions,
                generation,
                &key,
                identity,
                &entry,
                describe_once(&host, self, self.output.clone(), input.clone(), command),
            )
            .await;
        }
        let result = loop {
            if let Some(answer) = result_rx.borrow().clone() {
                break answer;
            }
            if result_rx.changed().await.is_err() {
                return input;
            }
        };
        result.value.clone()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::test_support::{
        noop_hook_executor, MockApiClient, MockOutputStream, NoOpPermissionGate,
        StaticMemoryProvider,
    };

    #[derive(Default)]
    struct DescribeSessionContext {
        dispatch_started: tokio::sync::Notify,
        release_dispatch: tokio::sync::Notify,
    }

    #[async_trait::async_trait]
    impl hooks::mods::ModSessionContext for DescribeSessionContext {
        fn cwd(&self) -> std::path::PathBuf {
            std::env::current_dir().unwrap()
        }

        fn root(&self) -> std::path::PathBuf {
            std::env::current_dir().unwrap()
        }

        async fn model(&self) -> String {
            "test-model".to_owned()
        }

        async fn id(&self) -> String {
            "describe-test-session".to_owned()
        }

        async fn turns(&self) -> u64 {
            0
        }

        async fn messages(
            &self,
            _input: serde_json::Value,
        ) -> Result<hooks::mods::ModUtf16ValueProjection, hooks::mods::ModError> {
            self.dispatch_started.notify_one();
            self.release_dispatch.notified().await;
            Ok(hooks::mods::ModUtf16ValueProjection::plain(
                serde_json::json!([]),
            ))
        }
    }

    #[tokio::test]
    async fn command_describe_caches_by_source_and_invalidates_on_request() {
        let dir = tempfile::tempdir().unwrap();
        let module = dir.path().join("describe-command.js");
        std::fs::write(
            &module,
            r#"export function register(on) {
              let calls = 0;
              on('command.describe', { command: 'hello' }, async ($, e, next) => {
                const call = ++calls;
                await $.clock.sleep(30);
                return next({ ...e, description: `${e.description}:${call}`, isHidden: true });
              });
              let retries = 0;
              on('command.describe', { command: 'retry' }, ($, e, next) => {
                if (++retries === 1) throw new Error('retry once');
                return next({ ...e, description: 'retry-ok' });
              });
              let slowCalls = 0;
              on('command.describe', { command: 'slow' }, async ($, e, next) => {
                const call = ++slowCalls;
                await $.clock.sleep(60);
                return next({ ...e, description: `slow:${call}` });
              });
              let cancelCalls = 0;
              on('command.describe', { command: 'cancel' }, async ($, e, next) => {
                const call = ++cancelCalls;
                await $.session.messages({});
                await $.clock.sleep(100);
                return next({ ...e, description: `cancel:${call}`, isHidden: true });
              });
            }"#,
        )
        .unwrap();
        let host = hooks::mods::ModHost::start(None).await.unwrap();
        host.load(
            "describe-command",
            dir.path(),
            &module,
            serde_json::json!({}),
        )
        .await
        .unwrap();
        let mut registry = hooks::HookRegistry::new();
        registry.set_mod_host(host);
        let session_context = Arc::new(DescribeSessionContext::default());
        let session_trait: Arc<dyn hooks::mods::ModSessionContext> = session_context.clone();
        registry.attach_mod_background_context(Arc::downgrade(&session_trait));
        let registry = Arc::new(tokio::sync::RwLock::new(registry));
        let output = Arc::new(MockOutputStream::new());
        let orch = Arc::new(
            ConversationOrchestrator::new(
                crate::OrchestratorConfig::default(),
                Arc::new(MockApiClient::new(vec![])),
                Arc::new(tool_api::registry::ToolRegistry::new()),
                noop_hook_executor(),
                Arc::new(NoOpPermissionGate),
                output.clone(),
                Arc::new(StaticMemoryProvider::empty()),
                dir.path().to_path_buf(),
            )
            .with_hook_registry(registry.clone()),
        );
        let input = serde_json::json!({
            "command":"hello", "description":"base", "argumentHint":"[name]",
            "isHidden":false, "immediate":false,
            "provider":{"plugin":"engine","tier":"core"}
        });
        let (first, concurrent) = tokio::join!(
            orch.apply_mod_command_description(input.clone()),
            orch.apply_mod_command_description(input.clone())
        );
        assert_eq!(first["description"], "base:1");
        assert_eq!(concurrent, first);
        assert_eq!(first["isHidden"], true);
        assert_eq!(
            orch.apply_mod_command_description(input.clone()).await,
            first
        );
        hooks::mods::ModSessionContext::invalidate_command_describe(&*orch)
            .await
            .unwrap();
        assert_eq!(
            orch.apply_mod_command_description(input.clone()).await["description"],
            "base:2"
        );
        let mut changed = input.clone();
        changed["description"] = serde_json::json!("other");
        assert_eq!(
            orch.apply_mod_command_description(changed).await["description"],
            "other:3"
        );

        let mut retry = input.clone();
        retry["command"] = serde_json::json!("retry");
        assert_eq!(
            orch.apply_mod_command_description(retry.clone()).await,
            retry
        );
        assert_eq!(
            orch.apply_mod_command_description(retry.clone()).await,
            retry
        );
        hooks::mods::ModSessionContext::invalidate_command_describe(&*orch)
            .await
            .unwrap();
        assert_eq!(
            orch.apply_mod_command_description(retry).await["description"],
            "retry-ok"
        );

        let mut slow = input;
        slow["command"] = serde_json::json!("slow");
        let (before_invalidation, after_invalidation) =
            tokio::join!(orch.apply_mod_command_description(slow.clone()), async {
                tokio::time::sleep(std::time::Duration::from_millis(10)).await;
                hooks::mods::ModSessionContext::invalidate_command_describe(&*orch)
                    .await
                    .unwrap();
                orch.apply_mod_command_description(slow).await
            });
        assert_eq!(before_invalidation["description"], "slow:1");
        assert_eq!(after_invalidation["description"], "slow:2");

        let cancel_input = serde_json::json!({
            "command":"cancel", "description":"base", "isHidden":false,
            "immediate":false, "provider":{"plugin":"engine","tier":"core"}
        });
        let first_orch = orch.clone();
        let first_input = cancel_input.clone();
        let first_caller =
            tokio::spawn(
                async move { first_orch.apply_mod_command_description(first_input).await },
            );
        tokio::time::timeout(
            std::time::Duration::from_secs(2),
            session_context.dispatch_started.notified(),
        )
        .await
        .expect("cancel hook should start before its caller is canceled");
        first_caller.abort();
        assert!(first_caller.await.unwrap_err().is_cancelled());
        session_context.release_dispatch.notify_one();

        let resumed = orch
            .apply_mod_command_description(cancel_input.clone())
            .await;
        assert_eq!(resumed["description"], "cancel:1");
        assert_eq!(
            orch.apply_mod_command_description(cancel_input.clone())
                .await,
            resumed
        );
    }

    #[tokio::test]
    async fn command_describe_failure_eviction_respects_invalidation_generation() {
        let cache = Arc::new(tokio::sync::Mutex::new(ModCommandDescribeCache::default()));
        let identity = (7, 11);
        let key = "command.describe failure";
        let generation = cache.lock().await.generation;
        let (result, _) = tokio::sync::watch::channel(None);
        let failed_entry = Arc::new(ModCommandDescribeEntry {
            result,
            inline_init: tokio::sync::Mutex::new(()),
        });
        let failed_result = failed_entry.result.subscribe();
        cache
            .lock()
            .await
            .answers
            .insert(key.to_owned(), (identity, failed_entry.clone()));

        complete_command_description(
            &cache,
            generation,
            key,
            identity,
            &failed_entry,
            ModCommandDescribeAnswer {
                value: serde_json::json!({"description":"base","isHidden":false}),
                cacheable: false,
            },
        )
        .await;
        assert!(!cache.lock().await.answers.contains_key(key));
        assert!(
            !failed_result
                .borrow()
                .as_ref()
                .expect("failed result should still wake existing waiters")
                .cacheable
        );

        let (result, _) = tokio::sync::watch::channel(None);
        let invalidated_entry = Arc::new(ModCommandDescribeEntry {
            result,
            inline_init: tokio::sync::Mutex::new(()),
        });
        cache
            .lock()
            .await
            .answers
            .insert(key.to_owned(), (identity, failed_entry.clone()));
        {
            let mut cache = cache.lock().await;
            cache.generation = cache.generation.wrapping_add(1);
            cache.answers.clear();
        }
        let new_generation = cache.lock().await.generation;
        cache
            .lock()
            .await
            .answers
            .insert(key.to_owned(), (identity, invalidated_entry.clone()));

        complete_command_description(
            &cache,
            generation,
            key,
            identity,
            &failed_entry,
            ModCommandDescribeAnswer {
                value: serde_json::json!({"description":"base","isHidden":false}),
                cacheable: false,
            },
        )
        .await;
        let entries = cache.lock().await;
        assert_eq!(entries.generation, new_generation);
        assert!(entries
            .answers
            .get(key)
            .is_some_and(|(_, current)| Arc::ptr_eq(current, &invalidated_entry)));
    }

    #[tokio::test]
    async fn command_describe_unbound_cancellation_retries_inline_dispatch() {
        let cache = Arc::new(tokio::sync::Mutex::new(ModCommandDescribeCache::default()));
        let identity = (13, 17);
        let key = "command.describe inline".to_owned();
        let generation = cache.lock().await.generation;
        let (result, _) = tokio::sync::watch::channel(None);
        let entry = Arc::new(ModCommandDescribeEntry {
            result,
            inline_init: tokio::sync::Mutex::new(()),
        });
        cache
            .lock()
            .await
            .answers
            .insert(key.clone(), (identity, entry.clone()));

        let started = Arc::new(tokio::sync::Notify::new());
        let release = Arc::new(tokio::sync::Notify::new());
        let attempts = Arc::new(std::sync::atomic::AtomicUsize::new(0));
        let first_cache = cache.clone();
        let first_entry = entry.clone();
        let first_started = started.clone();
        let first_release = release.clone();
        let first_attempts = attempts.clone();
        let first_key = key.clone();
        let first_caller = tokio::spawn(async move {
            run_inline_command_description(
                &first_cache,
                generation,
                &first_key,
                identity,
                &first_entry,
                async move {
                    first_attempts.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
                    first_started.notify_one();
                    first_release.notified().await;
                    ModCommandDescribeAnswer {
                        value: serde_json::json!({"description":"canceled"}),
                        cacheable: true,
                    }
                },
            )
            .await;
        });
        tokio::time::timeout(std::time::Duration::from_secs(2), started.notified())
            .await
            .expect("inline dispatch should begin before its caller is canceled");
        first_caller.abort();
        assert!(first_caller.await.unwrap_err().is_cancelled());

        let second_attempts = attempts.clone();
        run_inline_command_description(&cache, generation, &key, identity, &entry, async move {
            second_attempts.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
            ModCommandDescribeAnswer {
                value: serde_json::json!({"description":"retried"}),
                cacheable: true,
            }
        })
        .await;

        let third_attempts = attempts.clone();
        run_inline_command_description(&cache, generation, &key, identity, &entry, async move {
            third_attempts.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
            ModCommandDescribeAnswer {
                value: serde_json::json!({"description":"unexpected"}),
                cacheable: true,
            }
        })
        .await;

        assert_eq!(attempts.load(std::sync::atomic::Ordering::SeqCst), 2);
        assert_eq!(
            entry.result.subscribe().borrow().as_ref().unwrap().value["description"],
            "retried"
        );
    }
}
