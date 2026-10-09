//! Session-cached `command.describe` projection for slash menus and `/help`.

use super::runtime_impl::{
    ModCommandDescribeAnswer, ModCommandDescribeCache, ModCommandDescribeEntry,
};
use super::*;
use lingxi_core::types::utf16_json::Utf16JsonProjection as Ujp;

fn exact_member(source: &Ujp, field: &str) -> Option<Ujp> {
    source.subprojection(&format!("/{field}")).ok()
}

fn valid_description(source: &Ujp, answer: &Ujp) -> bool {
    let Some(description) = answer.string_units("/description") else {
        return false;
    };
    if answer
        .at_pointer("/isHidden")
        .and_then(|value| value.as_bool())
        .is_none()
    {
        return false;
    }
    if source.string_units("/description").as_deref() != Some(description.as_slice())
        && description.len() > 4096
    {
        return false;
    }
    if answer.contains_property("argumentHint") {
        let Some(hint) = answer.string_units("/argumentHint") else {
            return false;
        };
        if source.string_units("/argumentHint").as_deref() != Some(hint.as_slice())
            && hint.len() > 4096
        {
            return false;
        }
    }
    true
}

async fn describe_once(
    host: &hooks::mods::ModHost,
    session: &dyn hooks::mods::ModSessionContext,
    output: Arc<dyn OutputStream>,
    input: Ujp,
    command_units: Vec<u16>,
    command_display: String,
) -> ModCommandDescribeAnswer {
    let pinned_immediate = exact_member(&input, "immediate").expect("validated immediate");
    let pinned_provider = exact_member(&input, "provider").expect("validated provider");
    let source = input.clone();
    let log_output = output.clone();
    let toast_output = output.clone();
    let status_output = output;
    let answer = host
        .dispatch_with_utf16_at_context_scope(
            "command.describe",
            input.clone(),
            &session.cwd(),
            Some(session),
            hooks::mods::ModDispatchScope::default(),
            lingxi_core::host::task_registry::FieldPresence::Missing,
            move |forwarded| {
                let command_units = command_units.clone();
                let immediate = pinned_immediate.clone();
                let provider = pinned_provider.clone();
                let source = source.clone();
                async move {
                    let Some(forwarded) = forwarded else {
                        return Err(hooks::mods::ModError::Hook(
                            "command.describe removed its input".into(),
                        ));
                    };
                    if forwarded.string_units("/command").as_deref()
                        != Some(command_units.as_slice())
                        || exact_member(&forwarded, "immediate")
                            .is_none_or(|value| !value.semantically_eq(&immediate))
                        || exact_member(&forwarded, "provider")
                            .is_none_or(|value| !value.semantically_eq(&provider))
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
                    let mut result = Ujp::parse("{}").expect("static object");
                    result
                        .set_field(
                            "description",
                            exact_member(&forwarded, "description").expect("validated description"),
                        )
                        .expect("description result object");
                    result
                        .set_field(
                            "isHidden",
                            exact_member(&forwarded, "isHidden").expect("validated isHidden"),
                        )
                        .expect("description result object");
                    if let Some(hint) = exact_member(&forwarded, "argumentHint") {
                        result
                            .set_field("argumentHint", hint)
                            .expect("description result object");
                    }
                    Ok(Some(result))
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
                async move { output.emit_mod_status(&plugin, text.as_ref()).await }
            },
        )
        .await.map(|outcome| outcome.result_projection);
    let mut described = input.clone();
    let cacheable = match answer {
        Ok(answer) if valid_description(&input, &answer) => {
            let applied = exact_member(&answer, "description")
                .and_then(|value| described.set_field("description", value).ok())
                .is_some()
                && exact_member(&answer, "isHidden")
                    .and_then(|value| described.set_field("isHidden", value).ok())
                    .is_some();
            let hint_applied = if let Some(hint) = exact_member(&answer, "argumentHint") {
                described.set_field("argumentHint", hint).is_ok()
            } else {
                described.remove_field("argumentHint").is_ok()
            };
            if applied && hint_applied {
                true
            } else {
                tracing::warn!(command = %command_display, "command.describe result could not be projected");
                false
            }
        }
        Ok(_) => {
            tracing::warn!(command = %command_display, "command.describe Mod returned an invalid answer");
            false
        }
        Err(error) => {
            tracing::warn!(command = %command_display, %error, "command.describe Mod failed");
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
    pub(crate) async fn apply_mod_command_description(&self, input: Ujp) -> Ujp {
        let Some(command_units) = input.string_units("/command") else {
            return input;
        };
        let Some(immediate) = input
            .at_pointer("/immediate")
            .and_then(|value| value.as_bool())
        else {
            return input;
        };
        if input.subprojection("/provider").is_err() || !valid_description(&input, &input) {
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
        let key = input.to_json_string().unwrap_or_default();
        let command_display = input.string_at("/command").unwrap_or_default().to_owned();
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
                let task_command_units = command_units.clone();
                let task_command_display = command_display.clone();
                let task_key = key.clone();
                let task_cache = self.prompt_runtime.mod_command_descriptions.clone();
                tokio::spawn(async move {
                    let answer = describe_once(
                        &task_host,
                        session.as_ref(),
                        task_output,
                        task_input,
                        task_command_units,
                        task_command_display,
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
                describe_once(
                    &host,
                    self,
                    self.output.clone(),
                    input.clone(),
                    command_units,
                    command_display,
                ),
            )
            .await;
        }
        loop {
            if let Some(answer) = result_rx.borrow().clone() {
                return answer.value.clone();
            }
            if result_rx.changed().await.is_err() {
                return input;
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::test_support::{
        noop_hook_executor, MockApiClient, MockOutputStream, NoOpPermissionGate,
        StaticMemoryProvider,
    };

    fn projection(json: &str) -> Ujp {
        Ujp::parse(json).expect("valid exact test JSON")
    }

    fn string_value(value: &str) -> Ujp {
        Ujp::root_string(value.to_owned(), value.encode_utf16().collect()).unwrap()
    }

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

        async fn messages(&self, _input: Option<Ujp>) -> Result<Ujp, hooks::mods::ModError> {
            self.dispatch_started.notify_one();
            self.release_dispatch.notified().await;
            Ok(projection("[]"))
        }
    }

    #[tokio::test]
    async fn command_describe_keeps_exact_strings_through_mod_dispatch_and_cache() {
        let dir = tempfile::tempdir().unwrap();
        let module = dir.path().join("describe-command.js");
        std::fs::write(
            &module,
            r#"export function register(on) {
              let calls = 0;
              on('command.describe', { command: 'hello\ud800' }, async ($, e, next) => {
                const call = ++calls;
                await $.clock.sleep(30);
                return next({ ...e, description: `${e.description}:\udfff:${call}`, isHidden: true,
                  argumentHint: `${e.argumentHint}:\ud800` });
              });
            }"#,
        )
        .unwrap();
        let host = hooks::mods::ModHost::start(None).await.unwrap();
        host.load(
            "describe-command",
            dir.path(),
            &module,
            Ujp::parse("{}").unwrap(),
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
                output,
                Arc::new(StaticMemoryProvider::empty()),
                dir.path().to_path_buf(),
            )
            .with_hook_registry(registry),
        );
        let input = projection(
            r#"{"command":"hello\ud800","description":"base\ud800","argumentHint":"[name]\ud800","isHidden":false,"immediate":false,"provider":{"plugin":"engine","tier":"core"}}"#,
        );
        let (first, concurrent) = tokio::join!(
            orch.apply_mod_command_description(input.clone()),
            orch.apply_mod_command_description(input.clone())
        );
        assert_eq!(first.string_at("/description"), Some("base�:�:1"));
        assert_eq!(
            first.string_units("/description").unwrap(),
            vec![
                b'b' as u16,
                b'a' as u16,
                b's' as u16,
                b'e' as u16,
                0xd800,
                b':' as u16,
                0xdfff,
                b':' as u16,
                b'1' as u16,
            ]
        );
        assert_eq!(
            first.string_units("/argumentHint").unwrap().last(),
            Some(&0xd800)
        );
        assert!(first.semantically_eq(&concurrent));
        assert_eq!(
            first
                .at_pointer("/isHidden")
                .and_then(|value| value.as_bool()),
            Some(true)
        );
        assert!(orch
            .apply_mod_command_description(input.clone())
            .await
            .semantically_eq(&first));
        hooks::mods::ModSessionContext::invalidate_command_describe(&*orch)
            .await
            .unwrap();
        let second = orch.apply_mod_command_description(input).await;
        assert!(second.string_at("/description").unwrap().ends_with(":2"));
    }

    #[tokio::test]
    async fn failed_command_description_is_evicted_without_overwriting_source() {
        let dir = tempfile::tempdir().unwrap();
        let module = dir.path().join("describe-command.js");
        std::fs::write(
            &module,
            r#"export function register(on) {
              let retries = 0;
              on('command.describe', { command: 'retry' }, ($, e, next) => {
                if (++retries === 1) throw new Error('retry once');
                return next({ ...e, description: 'retry-ok' });
              });
            }"#,
        )
        .unwrap();
        let host = hooks::mods::ModHost::start(None).await.unwrap();
        host.load(
            "describe-retry",
            dir.path(),
            &module,
            Ujp::parse("{}").unwrap(),
        )
        .await
        .unwrap();
        let mut registry = hooks::HookRegistry::new();
        registry.set_mod_host(host);
        let registry = Arc::new(tokio::sync::RwLock::new(registry));
        let orch = ConversationOrchestrator::new(
            crate::OrchestratorConfig::default(),
            Arc::new(MockApiClient::new(vec![])),
            Arc::new(tool_api::registry::ToolRegistry::new()),
            noop_hook_executor(),
            Arc::new(NoOpPermissionGate),
            Arc::new(MockOutputStream::new()),
            Arc::new(StaticMemoryProvider::empty()),
            dir.path().to_path_buf(),
        )
        .with_hook_registry(registry);
        let input = projection(
            r#"{"command":"retry","description":"base","isHidden":false,"immediate":false,"provider":{"plugin":"engine"}}"#,
        );
        assert!(orch
            .apply_mod_command_description(input.clone())
            .await
            .semantically_eq(&input));
        assert_eq!(
            orch.apply_mod_command_description(input)
                .await
                .string_at("/description"),
            Some("retry-ok")
        );
    }

    #[tokio::test]
    async fn inline_dispatch_gate_retries_after_cancellation() {
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
                        value: projection(r#"{"description":"canceled"}"#),
                        cacheable: true,
                    }
                },
            )
            .await;
        });
        tokio::time::timeout(std::time::Duration::from_secs(2), started.notified())
            .await
            .expect("dispatch should start before the waiter is canceled");
        first_caller.abort();
        assert!(first_caller.await.unwrap_err().is_cancelled());
        run_inline_command_description(&cache, generation, &key, identity, &entry, async {
            attempts.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
            ModCommandDescribeAnswer {
                value: projection(r#"{"description":"retried"}"#),
                cacheable: true,
            }
        })
        .await;
        assert_eq!(attempts.load(std::sync::atomic::Ordering::SeqCst), 2);
        assert_eq!(
            entry
                .result
                .subscribe()
                .borrow()
                .as_ref()
                .unwrap()
                .value
                .string_at("/description"),
            Some("retried")
        );
        let _ = string_value("generated test helper remains explicit");
    }
}
