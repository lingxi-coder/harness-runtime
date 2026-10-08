//! Live slash-command projection for `$.command.list()`.

use crate::{
    BuiltinCommandHandler, CommandRegistry, CommandResult, CommandSource, ParsedSlashCommand,
    SlashCommand, SlashCommandKind,
};
use hooks::mods::{ModCommandCatalog, ModError};
use serde_json::{json, Value};
use std::future::Future;
use std::sync::Arc;
use tokio::sync::RwLock;

/// Facts stamped by the host entry that admitted one slash invocation.
#[derive(Clone, Debug, PartialEq)]
pub struct ModCommandRunContext {
    /// The command's authenticated ingress (`composer`, `bridge`, `sdk`, or plugin).
    pub origin: Value,
    /// Whether its answer is being shown on a fullscreen terminal surface.
    pub is_fullscreen: bool,
    /// Terminal width available to the command answer.
    pub columns: u16,
}

impl Default for ModCommandRunContext {
    fn default() -> Self {
        Self {
            origin: json!({"kind":"unclassified"}),
            is_fullscreen: false,
            columns: 80,
        }
    }
}

tokio::task_local! {
    static COMMAND_RUN_CONTEXT: ModCommandRunContext;
    static COMMAND_RUN_SETTLEMENT: Arc<std::sync::Mutex<Option<Value>>>;
}

/// Scope a host-supplied command origin and presentation to one dispatch.
pub async fn with_mod_command_context<F: Future>(
    context: ModCommandRunContext,
    dispatch: F,
) -> F::Output {
    COMMAND_RUN_CONTEXT.scope(context, dispatch).await
}

/// Capture the raw `command.run` result separately from its attributed display
/// text when a queued `$.command.run()` invocation is settled.
pub async fn with_mod_command_capture<F: Future>(
    context: ModCommandRunContext,
    dispatch: F,
) -> (F::Output, Option<Value>) {
    let settlement = Arc::new(std::sync::Mutex::new(None));
    let output = COMMAND_RUN_SETTLEMENT
        .scope(
            settlement.clone(),
            with_mod_command_context(context, dispatch),
        )
        .await;
    let captured = settlement
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner)
        .take();
    (output, captured)
}

/// Record the unprefixed API result for the queued command's waiter.
pub fn record_mod_command_settlement(result: Value) {
    let _ = COMMAND_RUN_SETTLEMENT.try_with(|settlement| {
        *settlement
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner) = Some(result);
    });
}

pub(crate) fn current_command_context() -> ModCommandRunContext {
    COMMAND_RUN_CONTEXT
        .try_with(Clone::clone)
        .unwrap_or_default()
}

/// Host callback that runs a registered command through its Mod chain.
#[async_trait::async_trait]
pub trait ModCommandExecutor: Send + Sync {
    /// Execute a typed slash command through the registered Mod hook chain.
    async fn run(
        &self,
        plugin: &str,
        command: &str,
        args: &str,
        context: ModCommandRunContext,
    ) -> Result<String, String>;
}

/// Host queue that admits a plugin-origin slash invocation and settles after
/// the queue consumer runs it. This must not execute the dispatcher inline:
/// `$.command.run` participates in the same ordering as person inputs.
#[async_trait::async_trait]
pub trait ModCommandQueue: Send + Sync {
    /// Admit a command under the invoking plugin's origin and return its
    /// settled output after a queue consumer executes it.
    async fn enqueue(&self, plugin: &str, command: &str, args: &str) -> Result<Value, String>;
    /// Queue text for the model. The answer settles when the host takes the
    /// prompt from its queue, before the model turn has finished.
    async fn enqueue_prompt(
        &self,
        _plugin: &str,
        _text: &str,
        _as_user: bool,
    ) -> Result<Value, String> {
        Err("prompt.submit needs a host prompt queue".into())
    }
}

type ExecutorSlot = Arc<std::sync::RwLock<Option<Arc<dyn ModCommandExecutor>>>>;

struct ModCommandHandler {
    plugin: String,
    name: String,
    description: String,
    argument_hint: Option<String>,
    executor: ExecutorSlot,
}

#[async_trait::async_trait]
impl BuiltinCommandHandler for ModCommandHandler {
    fn name(&self) -> &str {
        &self.name
    }
    fn description(&self) -> &str {
        &self.description
    }
    fn argument_hint(&self) -> Option<&str> {
        self.argument_hint.as_deref()
    }
    async fn handle(&self, args: &ParsedSlashCommand) -> CommandResult {
        let executor = self
            .executor
            .read()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .clone();
        let display = match executor {
            Some(executor) => executor
                .run(
                    &self.plugin,
                    &self.name,
                    &args.raw_args,
                    current_command_context(),
                )
                .await
                .unwrap_or_else(|error| format!("/{}: {error}", self.name)),
            None => format!("/{}: Mod command executor unavailable", self.name),
        };
        CommandResult::Done {
            display: Some(display),
        }
    }
}

/// Reads the same registry as slash dispatch and the user-facing typeahead.
pub struct RegistryModCommandCatalog {
    registry: Arc<RwLock<CommandRegistry>>,
    executor: ExecutorSlot,
    queue: Arc<std::sync::RwLock<Option<Arc<dyn ModCommandQueue>>>>,
}

impl RegistryModCommandCatalog {
    /// Bind the session's shared command registry.
    #[must_use]
    pub fn new(registry: Arc<RwLock<CommandRegistry>>) -> Self {
        Self {
            registry,
            executor: Arc::new(std::sync::RwLock::new(None)),
            queue: Arc::new(std::sync::RwLock::new(None)),
        }
    }

    /// Bind the live session executor once its orchestrator exists.
    pub fn bind_executor(&self, executor: Arc<dyn ModCommandExecutor>) {
        *self
            .executor
            .write()
            .unwrap_or_else(std::sync::PoisonError::into_inner) = Some(executor);
    }

    /// Bind the active host's command queue once it has a consumer.
    pub fn bind_queue(&self, queue: Arc<dyn ModCommandQueue>) {
        *self
            .queue
            .write()
            .unwrap_or_else(std::sync::PoisonError::into_inner) = Some(queue);
    }
}

#[async_trait::async_trait]
impl ModCommandCatalog for RegistryModCommandCatalog {
    async fn list(&self) -> Result<Value, ModError> {
        let registry = self.registry.read().await;
        let mut commands = registry.palette_commands();
        commands.sort_by(|left, right| left.name.cmp(&right.name));
        Ok(Value::Array(
            commands
                .into_iter()
                .map(|command| {
                    let source = match command.source {
                        CommandSource::Builtin | CommandSource::Bundled => "builtin",
                        CommandSource::Plugin => "plugin",
                        CommandSource::Mcp => "mcp",
                        CommandSource::Settings(_) => "user",
                    };
                    let mut info = json!({
                        "name":command.name,
                        "description":command.description,
                        "source":source,
                    });
                    if source == "plugin" {
                        if let Some(plugin) = registry
                            .mod_owner_of(&command.name)
                            .or_else(|| command.name.split_once(':').map(|(plugin, _)| plugin))
                        {
                            info["plugin"] = Value::String(plugin.to_owned());
                        }
                    }
                    info
                })
                .collect(),
        ))
    }

    async fn register(&self, plugin: &str, spec: Value) -> Result<Value, ModError> {
        let name = spec
            .get("name")
            .and_then(Value::as_str)
            .ok_or_else(|| ModError::Hook("command.register name must be a string".into()))?;
        if name.is_empty()
            || name.len() > 64
            || !name
                .bytes()
                .all(|byte| byte.is_ascii_alphanumeric() || byte == b'_' || byte == b'-')
        {
            return Err(ModError::Hook(
                "command.register name uses letters, digits, _, or - and has at most 64 characters"
                    .into(),
            ));
        }
        let description = spec
            .get("description")
            .and_then(Value::as_str)
            .ok_or_else(|| {
                ModError::Hook("command.register description must be a string".into())
            })?;
        if description.trim().is_empty() {
            return Err(ModError::Hook(format!(
                "{plugin}: $.command.register: {name} needs a description (what the menu shows)"
            )));
        }
        let argument_hint = match spec.get("argumentHint") {
            None => None,
            Some(Value::String(hint)) => Some(hint.clone()),
            _ => {
                return Err(ModError::Hook(
                    "command.register argumentHint must be a string".into(),
                ))
            }
        };
        let immediate = match spec.get("immediate") {
            None => false,
            Some(Value::Bool(value)) => *value,
            // Native `command.register` rejects non-booleans before the
            // command worker projects `immediate === true` into its listing.
            _ => {
                return Err(ModError::Hook(
                    "command.register immediate must be true or false".into(),
                ))
            }
        };
        let mut registry = self.registry.write().await;
        if let Some(owner) = registry.mod_owner_of(name) {
            if owner != plugin {
                return Err(ModError::Hook(format!(
                    "{plugin}: $.command.register: \"/{name}\" refused: the plugin {owner} registered it already"
                )));
            }
        } else if let Some(existing) = registry.resolve(name) {
            let source = match existing.source {
                CommandSource::Builtin | CommandSource::Bundled => "built-in",
                CommandSource::Plugin => "plugin's",
                CommandSource::Settings(_) => "user's",
                CommandSource::Mcp => "MCP server's",
            };
            return Err(ModError::Hook(format!(
                "{plugin}: $.command.register: \"/{name}\" refused: it is the {source} /{}",
                existing.name
            )));
        }
        let command = SlashCommand {
            name: name.to_owned(),
            description: description.to_owned(),
            argument_hint: argument_hint.clone(),
            source: CommandSource::Plugin,
            kind: SlashCommandKind::Builtin {
                handler_id: name.to_owned(),
            },
            loaded_from: Some("plugin".into()),
            ..SlashCommand::default()
        };
        let handler = Arc::new(ModCommandHandler {
            plugin: plugin.to_owned(),
            name: name.to_owned(),
            description: description.to_owned(),
            argument_hint,
            executor: self.executor.clone(),
        });
        registry.register_mod_command(plugin, command, handler, immediate);
        Ok(json!({"command":name}))
    }

    async fn run(&self, plugin: &str, command: &str, args: &str) -> Result<Value, ModError> {
        let exists = {
            let registry = self.registry.read().await;
            registry.resolve(command).is_some()
        };
        if !exists {
            return Err(ModError::Hook(format!(
                "{plugin}: $.command.run: no command named /{command} in this session"
            )));
        }
        let queue = self
            .queue
            .read()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .clone()
            .ok_or_else(|| {
                ModError::Unavailable("command.run needs a host command queue".into())
            })?;
        queue
            .enqueue(plugin, command, args)
            .await
            .map_err(ModError::Hook)
    }

    async fn submit_prompt(
        &self,
        plugin: &str,
        text: &str,
        as_user: bool,
    ) -> Result<Value, ModError> {
        let queue = self
            .queue
            .read()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .clone()
            .ok_or_else(|| {
                ModError::Unavailable("prompt.submit needs a host prompt queue".into())
            })?;
        queue
            .enqueue_prompt(plugin, text, as_user)
            .await
            .map_err(ModError::Hook)
    }

    async fn unregister_plugin(&self, plugin: &str) {
        self.registry.write().await.unregister_mod_owner(plugin);
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{SlashCommand, SlashCommandKind};
    use lingxi_core::host::{SlashCommandDispatcher, SlashDispatchResult};
    use lingxi_core::types::PluginId;

    struct EchoExecutor;

    struct ContextExecutor;

    #[async_trait::async_trait]
    impl ModCommandExecutor for ContextExecutor {
        async fn run(
            &self,
            _plugin: &str,
            _command: &str,
            args: &str,
            context: ModCommandRunContext,
        ) -> Result<String, String> {
            Ok(format!(
                "{}:{}:{}:{args}",
                context.origin["kind"].as_str().unwrap(),
                context.is_fullscreen,
                context.columns
            ))
        }
    }

    #[derive(Default)]
    struct RecordingQueue(std::sync::Mutex<Vec<(String, String, String)>>);

    #[derive(Default)]
    struct PromptRecordingQueue(std::sync::Mutex<Vec<(String, String, bool)>>);

    #[async_trait::async_trait]
    impl ModCommandQueue for PromptRecordingQueue {
        async fn enqueue(&self, _: &str, _: &str, _: &str) -> Result<Value, String> {
            unreachable!("prompt fixture never queues a command")
        }

        async fn enqueue_prompt(
            &self,
            plugin: &str,
            text: &str,
            as_user: bool,
        ) -> Result<Value, String> {
            self.0
                .lock()
                .unwrap()
                .push((plugin.to_owned(), text.to_owned(), as_user));
            Ok(json!({"text":text}))
        }
    }

    #[async_trait::async_trait]
    impl ModCommandQueue for RecordingQueue {
        async fn enqueue(&self, plugin: &str, command: &str, args: &str) -> Result<Value, String> {
            self.0
                .lock()
                .unwrap()
                .push((plugin.to_owned(), command.to_owned(), args.to_owned()));
            Ok(json!({"text":format!("ran /{command} {args}")}))
        }
    }

    #[async_trait::async_trait]
    impl ModCommandExecutor for EchoExecutor {
        async fn run(
            &self,
            plugin: &str,
            command: &str,
            args: &str,
            _context: ModCommandRunContext,
        ) -> Result<String, String> {
            Ok(format!("{plugin}:{command}:{args}"))
        }
    }

    #[tokio::test]
    async fn registered_mod_command_dispatches_and_unloads_with_owner() {
        let registry = Arc::new(RwLock::new(CommandRegistry::new()));
        let catalog = RegistryModCommandCatalog::new(registry.clone());
        catalog.bind_executor(Arc::new(EchoExecutor));
        let spec = json!({"name":"hello","description":"Say hello","argumentHint":"[name]","immediate":true});
        assert_eq!(
            catalog.register("demo", spec.clone()).await.unwrap(),
            json!({"command":"hello"})
        );
        assert_eq!(registry.read().await.mod_immediate_of("hello"), Some(true));
        assert_eq!(
            catalog.list().await.unwrap(),
            json!([
                {"name":"hello","description":"Say hello","source":"plugin","plugin":"demo"}
            ])
        );
        assert!(catalog
            .register("other", spec)
            .await
            .unwrap_err()
            .to_string()
            .contains("demo registered it already"));
        let dispatcher = crate::RegistrySlashDispatcher::new(registry.clone());
        assert_eq!(
            dispatcher.dispatch("/hello Ada").await,
            SlashDispatchResult::Handled {
                display: "demo:hello:Ada".into()
            }
        );
        catalog.unregister_plugin("demo").await;
        assert!(registry.read().await.resolve("hello").is_none());
        assert!(matches!(
            dispatcher.dispatch("/hello Ada").await,
            SlashDispatchResult::Unknown { .. }
        ));
    }

    #[tokio::test]
    async fn dispatch_scope_passes_origin_and_presentation_without_leaking_to_next_call() {
        let registry = Arc::new(RwLock::new(CommandRegistry::new()));
        let catalog = RegistryModCommandCatalog::new(registry.clone());
        catalog.bind_executor(Arc::new(ContextExecutor));
        catalog
            .register("demo", json!({"name":"hello","description":"Greet"}))
            .await
            .unwrap();
        let dispatcher = crate::RegistrySlashDispatcher::new(registry);
        let context = ModCommandRunContext {
            origin: json!({"kind":"plugin","name":"caller"}),
            is_fullscreen: true,
            columns: 123,
        };
        assert_eq!(
            with_mod_command_context(context, dispatcher.dispatch("/hello Ada")).await,
            SlashDispatchResult::Handled {
                display: "plugin:true:123:Ada".into()
            }
        );
        assert_eq!(
            dispatcher.dispatch("/hello Ada").await,
            SlashDispatchResult::Handled {
                display: "unclassified:false:80:Ada".into()
            }
        );
    }

    #[tokio::test]
    async fn command_register_accepts_false_immediate_and_rejects_blank_description() {
        let registry = Arc::new(RwLock::new(CommandRegistry::new()));
        let catalog = RegistryModCommandCatalog::new(registry.clone());
        assert_eq!(
            catalog
                .register(
                    "demo",
                    json!({"name":"later","description":"Run later","immediate":false}),
                )
                .await
                .unwrap(),
            json!({"command":"later"})
        );
        assert_eq!(registry.read().await.mod_owner_of("later"), Some("demo"));
        assert_eq!(registry.read().await.mod_immediate_of("later"), Some(false));
        let error = catalog
            .register("demo", json!({"name":"empty","description":" \t "}))
            .await
            .unwrap_err()
            .to_string();
        assert!(error.contains("empty needs a description"), "{error}");
        assert!(registry.read().await.resolve("empty").is_none());
    }

    #[tokio::test]
    async fn command_register_immediate_defaults_false_and_requires_a_boolean() {
        let registry = Arc::new(RwLock::new(CommandRegistry::new()));
        let catalog = RegistryModCommandCatalog::new(registry.clone());
        catalog
            .register("demo", json!({"name":"default","description":"Run later"}))
            .await
            .unwrap();
        assert_eq!(
            registry.read().await.mod_immediate_of("default"),
            Some(false)
        );

        for (name, immediate) in [
            ("null", Value::Null),
            ("number", json!(1)),
            ("string", json!("true")),
            ("array", json!([true])),
            ("object", json!({"value":true})),
        ] {
            let error = catalog
                .register(
                    "demo",
                    json!({"name":name,"description":"Run later","immediate":immediate}),
                )
                .await
                .unwrap_err()
                .to_string();
            assert!(
                error.contains("immediate must be true or false"),
                "{name}: {error}"
            );
            assert!(registry.read().await.resolve(name).is_none(), "{name}");
        }
    }

    #[tokio::test]
    async fn prompt_submit_forwards_plugin_text_and_as_user_to_live_queue() {
        let registry = Arc::new(RwLock::new(CommandRegistry::new()));
        let catalog = RegistryModCommandCatalog::new(registry);
        let queue = Arc::new(PromptRecordingQueue::default());
        catalog.bind_queue(queue.clone());
        assert_eq!(
            catalog
                .submit_prompt("demo", "follow up", true)
                .await
                .unwrap(),
            json!({"text":"follow up"})
        );
        assert_eq!(
            queue.0.lock().unwrap().as_slice(),
            &[("demo".to_owned(), "follow up".to_owned(), true)]
        );
    }

    #[tokio::test]
    async fn command_run_checks_live_catalog_before_queueing_and_preserves_plugin_origin() {
        let registry = Arc::new(RwLock::new(CommandRegistry::new()));
        let catalog = RegistryModCommandCatalog::new(registry);
        let queue = Arc::new(RecordingQueue::default());
        catalog.bind_queue(queue.clone());
        let unknown = catalog.run("demo", "missing", "Ada").await.unwrap_err();
        assert!(unknown
            .to_string()
            .contains("demo: $.command.run: no command named /missing in this session"));
        assert!(queue.0.lock().unwrap().is_empty());
        catalog
            .register("demo", json!({"name":"hello","description":"Greet"}))
            .await
            .unwrap();
        assert_eq!(
            catalog.run("caller", "hello", "Ada").await.unwrap(),
            json!({"text":"ran /hello Ada"})
        );
        assert_eq!(
            *queue.0.lock().unwrap(),
            vec![("caller".into(), "hello".into(), "Ada".into())]
        );
    }

    #[tokio::test]
    async fn mod_command_keeps_its_name_through_alias_and_disk_catalog_refresh() {
        let registry = Arc::new(RwLock::new(CommandRegistry::new()));
        let catalog = RegistryModCommandCatalog::new(registry.clone());
        catalog
            .register("demo", json!({"name":"hello","description":"Greet"}))
            .await
            .unwrap();
        let mut commands = registry.write().await;
        commands.register_alias("hello".into(), "other".into());
        commands.register_command(SlashCommand {
            name: "other".into(),
            aliases: vec!["hello".into()],
            loaded_from: Some("skills".into()),
            ..SlashCommand::default()
        });
        commands.register_alias("greet".into(), "hello".into());
        assert_eq!(commands.resolve("greet").unwrap().name, "hello");
        assert_eq!(commands.resolve("hello").unwrap().name, "hello");
        assert_eq!(commands.unregister_loaded_from("plugin"), 0);
        assert_eq!(commands.resolve("hello").unwrap().name, "hello");
        drop(commands);
        catalog.unregister_plugin("demo").await;
        assert!(registry.read().await.resolve("hello").is_none());
        let mut commands = registry.write().await;
        commands.register_command(SlashCommand {
            name: "hello".into(),
            ..SlashCommand::default()
        });
        assert!(commands.resolve("greet").is_none());
    }

    #[tokio::test]
    async fn live_catalog_maps_sources_and_uses_slash_visibility() {
        let mut registry = CommandRegistry::new();
        registry.register_command(SlashCommand {
            name: "zeta".into(),
            description: "Built in".into(),
            source: CommandSource::Builtin,
            ..SlashCommand::default()
        });
        registry.register_command(SlashCommand {
            name: "demo:hello".into(),
            description: "Plugin command".into(),
            source: CommandSource::Plugin,
            kind: SlashCommandKind::Plugin {
                plugin_id: PluginId::new(),
                file_path: "hello.md".into(),
                frontmatter: Default::default(),
                prompt_template: String::new(),
            },
            ..SlashCommand::default()
        });
        registry.register_command(SlashCommand {
            name: "hidden".into(),
            description: "Unavailable".into(),
            user_invocable: Some(false),
            ..SlashCommand::default()
        });
        let registry = Arc::new(RwLock::new(registry));
        let catalog = RegistryModCommandCatalog::new(registry.clone());
        assert_eq!(
            catalog.list().await.unwrap(),
            json!([
                {"name":"demo:hello","description":"Plugin command","source":"plugin","plugin":"demo"},
                {"name":"zeta","description":"Built in","source":"builtin"}
            ])
        );
        registry.write().await.register_command(SlashCommand {
            name: "alpha".into(),
            description: "Added later".into(),
            source: CommandSource::Settings(lingxi_core::types::SettingsScope::User),
            ..SlashCommand::default()
        });
        assert_eq!(catalog.list().await.unwrap()[0]["name"], "alpha");
    }
}
