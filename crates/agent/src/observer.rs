//! Observer declaration validation and descendant propagation.

use crate::definition::{AgentDefinition, ObserverSpec};
use std::collections::{HashMap, HashSet};

/// Observer fanout is bounded independently from ordinary subagent recursion.
pub const DEFAULT_OBSERVER_FANOUT_DEPTH: u32 = 3;

#[cfg(test)]
pub(crate) fn observer_env_lock() -> &'static std::sync::Mutex<()> {
    static LOCK: std::sync::OnceLock<std::sync::Mutex<()>> = std::sync::OnceLock::new();
    LOCK.get_or_init(|| std::sync::Mutex::new(()))
}

/// Observer agents remain experimental in claude-code 2.1.270 (`PZr`): the
/// parser accepts the declaration unconditionally, but the runtime only arms
/// it when the experimental env toggle is on and background tasks are not
/// globally disabled. LingXi mirrors that externally observable GATE, under
/// its own name.
///
/// The toggle is `LINGXI_CODE_EXPERIMENTAL_OBSERVER_AGENTS`. Upstream spells
/// it `CLAUDE_CODE_EXPERIMENTAL_OBSERVER_AGENTS`; that name is deliberately no
/// longer read. It is an opt-in for an experimental LingXi subsystem, not an
/// inbound contract a third-party process writes, so it is not in the brand
/// gate's `KEEP_CLAUDE_ENV` list — unlike
/// `CLAUDE_CODE_DISABLE_BACKGROUND_TASKS` just above, which IS honoured
/// because a user migrating from claude-code sets it to mean "no background
/// work", and silently ignoring that would start work they asked to stop.
#[must_use]
pub fn observer_agents_enabled() -> bool {
    let disabled = platform_api::env::is_env_truthy(
        std::env::var("LINGXI_DISABLE_BACKGROUND_TASKS")
            .ok()
            .as_deref(),
    ) || platform_api::env::is_env_truthy(
        std::env::var("CLAUDE_CODE_DISABLE_BACKGROUND_TASKS")
            .ok()
            .as_deref(),
    );
    if disabled {
        return false;
    }
    platform_api::env::is_env_truthy(
        std::env::var("LINGXI_CODE_EXPERIMENTAL_OBSERVER_AGENTS")
            .ok()
            .as_deref(),
    )
}

/// A validated observer declaration inherited by a child spawn.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ObserverPropagation {
    /// Effective observer declaration.
    pub spec: ObserverSpec,
    /// Agent that originally declared the observer.
    pub origin_agent: String,
    /// Agent types traversed so far, for cycle rejection and diagnostics.
    pub chain: Vec<String>,
    /// Current observer-only fanout depth.
    pub fanout_depth: u32,
}

/// Invalid observer graph.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum ObserverValidationError {
    /// Observer target does not resolve.
    #[error("agent '{owner}' declares missing observer '{observer}'")]
    Missing {
        /// Agent carrying the declaration.
        owner: String,
        /// Missing observer agent type.
        observer: String,
    },
    /// An agent cannot observe itself.
    #[error("agent '{0}' cannot observe itself")]
    SelfObserver(String),
    /// Observer declarations form a cycle.
    #[error("observer cycle: {0}")]
    Cycle(String),
    /// Persisted observer schema is newer than this runtime.
    #[error("agent '{owner}' uses unsupported observer schema version {version}")]
    UnsupportedSchema {
        /// Agent carrying the declaration.
        owner: String,
        /// Unsupported persisted version.
        version: u32,
    },
}

/// Validate every observer target and reject self/cycles before launch.
pub fn validate_observer_graph(
    definitions: &[AgentDefinition],
) -> Result<(), ObserverValidationError> {
    let by_name: HashMap<&str, &AgentDefinition> = definitions
        .iter()
        .map(|definition| (definition.agent_type.as_str(), definition))
        .collect();
    for definition in definitions {
        validate_observer_chain(definition, &by_name)?;
    }
    Ok(())
}

/// Validate only the observer chain reachable from `agent_type`.
///
/// Spawn-time validation must not let an unrelated malformed catalog entry
/// disable a valid agent. Full catalog diagnostics can continue to use
/// [`validate_observer_graph`].
pub fn validate_observer_for(
    definitions: &[AgentDefinition],
    agent_type: &str,
) -> Result<(), ObserverValidationError> {
    let by_name: HashMap<&str, &AgentDefinition> = definitions
        .iter()
        .map(|definition| (definition.agent_type.as_str(), definition))
        .collect();
    let Some(definition) = by_name.get(agent_type).copied() else {
        return Ok(());
    };
    validate_observer_chain(definition, &by_name)
}

fn validate_observer_chain<'a>(
    definition: &'a AgentDefinition,
    by_name: &HashMap<&'a str, &'a AgentDefinition>,
) -> Result<(), ObserverValidationError> {
    let Some(observer) = definition.observer.as_ref() else {
        return Ok(());
    };
    if observer.schema_version != platform_api::subagent_spawn::OBSERVER_SCHEMA_VERSION {
        return Err(ObserverValidationError::UnsupportedSchema {
            owner: definition.agent_type.clone(),
            version: observer.schema_version,
        });
    }
    if observer.agent == definition.agent_type {
        return Err(ObserverValidationError::SelfObserver(
            definition.agent_type.clone(),
        ));
    }
    if !by_name.contains_key(observer.agent.as_str()) {
        return Err(ObserverValidationError::Missing {
            owner: definition.agent_type.clone(),
            observer: observer.agent.clone(),
        });
    }

    let mut path = vec![definition.agent_type.as_str()];
    let mut seen = HashSet::from([definition.agent_type.as_str()]);
    let mut cursor = observer.agent.as_str();
    loop {
        if !seen.insert(cursor) {
            path.push(cursor);
            return Err(ObserverValidationError::Cycle(path.join(" -> ")));
        }
        path.push(cursor);
        let Some(next) = by_name
            .get(cursor)
            .and_then(|candidate| candidate.observer.as_ref())
        else {
            break;
        };
        if next.schema_version != platform_api::subagent_spawn::OBSERVER_SCHEMA_VERSION {
            return Err(ObserverValidationError::UnsupportedSchema {
                owner: cursor.to_string(),
                version: next.schema_version,
            });
        }
        cursor = next.agent.as_str();
        if !by_name.contains_key(cursor) {
            return Err(ObserverValidationError::Missing {
                owner: path.last().copied().unwrap_or_default().to_string(),
                observer: cursor.to_string(),
            });
        }
    }
    Ok(())
}

/// Resolve a direct or inherited observer declaration for a spawn.
///
/// A direct declaration wins. Inheritance stops when `observeSubagents:false`,
/// at the depth cap, or when the next owner would repeat an existing chain.
pub fn propagation_for_spawn(
    definition: &AgentDefinition,
    inherited: Option<&ObserverPropagation>,
) -> Option<ObserverPropagation> {
    if let Some(spec) = definition.observer.clone() {
        return Some(ObserverPropagation {
            spec,
            origin_agent: definition.agent_type.clone(),
            chain: vec![definition.agent_type.clone()],
            fanout_depth: 0,
        });
    }
    let inherited = inherited?;
    if !inherited.spec.observe_subagents
        || inherited.fanout_depth >= DEFAULT_OBSERVER_FANOUT_DEPTH
        || inherited.chain.contains(&definition.agent_type)
    {
        tracing::debug!(
            "[agentObserver] Agent {} not fanning out to observer agent (no chaining)",
            definition.agent_type
        );
        return None;
    }
    let mut next = inherited.clone();
    next.chain.push(definition.agent_type.clone());
    next.fanout_depth += 1;
    Some(next)
}

/// A passive lifecycle tap that never alters the observed agent's result.
pub(crate) struct ActivityObserver {
    /// The OBSERVER's spawn request — its declaration and `name` are cleared
    /// and its `description` is rewritten to `"<observer>@<observed>"`, so
    /// nothing about the OBSERVED agent survives on it. That is what
    /// [`Self::seed`] is for.
    pub request: platform_api::SubagentSpawnRequest,
    pub inheritance: platform_api::SubagentInheritance,
    pub registry: std::sync::Weak<dyn platform_api::task_registry::TaskRegistryHandle>,
    /// The observed agent's identity and declaration, captured while its own
    /// request was still intact.
    pub seed: platform_api::observer_pairing::ObserverPairingSeed,
}

#[async_trait::async_trait]
impl platform_api::subagent_spawn::SubagentSpawnObserver for ActivityObserver {
    async fn on_event(&self, event: platform_api::subagent_spawn::SubagentObservation) {
        use crate::observer_delivery::activity_from_message;
        use crate::observer_text::{build_digest, envelope_name, ObservedActivity};
        use platform_api::subagent_spawn::SubagentObservation;

        // claude-code 2.1.270 digests what the observed agent DID, rendered one
        // entry per activity (`UIo`) inside a `<name-activity>` envelope with
        // the `ebn` postamble — NOT a JSON dump of the runner event, which is
        // what this used to send. A terminal event is a `<turn-ended>` whose
        // reason names how it ended.
        let (agent_id, activity) = match event {
            SubagentObservation::Message { agent_id, message } => {
                (agent_id, activity_from_message(&message))
            }
            SubagentObservation::Completed { agent_id, .. } => (
                agent_id,
                vec![ObservedActivity::TurnEnded {
                    reason: "completed".into(),
                }],
            ),
            SubagentObservation::Failed { agent_id, error } => (
                agent_id,
                vec![
                    ObservedActivity::TurnEnded {
                        reason: "failed".into(),
                    },
                    ObservedActivity::AssistantText { text: error },
                ],
            ),
            SubagentObservation::Killed { agent_id } => (
                agent_id,
                vec![ObservedActivity::TurnEnded {
                    reason: "killed".into(),
                }],
            ),
            _ => return,
        };
        // Oracle `cBn`: a turn with no activity and no trigger wakes nobody.
        if activity.is_empty() {
            return;
        }
        let Some(registry) = self.registry.upgrade() else {
            return;
        };
        // Both of these come from the SEED, not from `self.request`. The
        // request is the OBSERVER's: its `name` is cleared and its
        // `description` is "<observer>@<observed>", so naming the envelope from
        // it named the envelope after the observer; and its `observer` field is
        // cleared, so the declaration's own instruction never reached the
        // digest at all.
        let digest = build_digest(
            &envelope_name(&self.seed.observed_name),
            None,
            &activity,
            self.seed.spec.message.as_deref(),
            true,
        );
        if let Err(error) = registry
            .observe_agent_activity(
                self.request.clone(),
                self.inheritance.clone(),
                agent_id,
                digest,
                Some(self.seed.clone()),
            )
            .await
        {
            tracing::warn!(%agent_id, %error, "observer activity could not be delivered");
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::definition::{
        AgentModel, AgentPermissionMode, AgentSource, AgentToolPolicy, ObserverSpec,
    };
    use std::path::PathBuf;

    fn definition(name: &str, observer: Option<ObserverSpec>) -> AgentDefinition {
        AgentDefinition {
            cache_ttl: None,
            agent_type: name.into(),
            when_to_use: String::new(),
            tools: AgentToolPolicy::All {
                use_exact_tools: false,
            },
            max_turns: 1,
            model: AgentModel::Inherit,
            permission_mode: AgentPermissionMode::Bubble,
            source: AgentSource::Settings(protocol::SettingsScope::User),
            base_dir: PathBuf::new(),
            system_prompt: None,
            mcp_servers: Vec::new(),
            frontmatter_hooks: Vec::new(),
            icon: None,
            allowed_tools: Vec::new(),
            worktree_requirement: None,
            disallowed_tools: Vec::new(),
            skills: Vec::new(),
            required_mcp_servers: Vec::new(),
            background: false,
            isolation: None,
            memory: None,
            effort: None,
            initial_prompt: None,
            color: None,
            observer,
        }
    }

    #[test]
    fn rejects_missing_self_and_cycles() {
        let missing = vec![definition("worker", Some(ObserverSpec::new("missing")))];
        assert!(matches!(
            validate_observer_graph(&missing),
            Err(ObserverValidationError::Missing { .. })
        ));

        let self_observed = vec![definition("worker", Some(ObserverSpec::new("worker")))];
        assert!(matches!(
            validate_observer_graph(&self_observed),
            Err(ObserverValidationError::SelfObserver(_))
        ));

        let cycle = vec![
            definition("a", Some(ObserverSpec::new("b"))),
            definition("b", Some(ObserverSpec::new("a"))),
        ];
        assert!(matches!(
            validate_observer_graph(&cycle),
            Err(ObserverValidationError::Cycle(_))
        ));
    }

    #[test]
    fn inherited_observer_defaults_to_three_levels_and_false_stops() {
        let root = definition("root", Some(ObserverSpec::new("reviewer")));
        let mut state = propagation_for_spawn(&root, None).expect("root observer");
        for (index, name) in ["child", "grandchild", "great-grandchild"]
            .into_iter()
            .enumerate()
        {
            let child = definition(name, None);
            state = propagation_for_spawn(&child, Some(&state)).expect("within cap");
            assert_eq!(state.fanout_depth, index as u32 + 1);
        }
        assert!(propagation_for_spawn(&definition("too-deep", None), Some(&state)).is_none());

        let mut no_fanout = ObserverSpec::new("reviewer");
        no_fanout.observe_subagents = false;
        let root = definition("root", Some(no_fanout));
        let state = propagation_for_spawn(&root, None).unwrap();
        assert!(propagation_for_spawn(&definition("child", None), Some(&state)).is_none());
    }

    #[test]
    fn observer_gate_requires_experimental_env_and_respects_background_disable() {
        let _guard = observer_env_lock().lock().unwrap();
        const FLAG: &str = "LINGXI_CODE_EXPERIMENTAL_OBSERVER_AGENTS";
        for var in [
            FLAG,
            "CLAUDE_CODE_EXPERIMENTAL_OBSERVER_AGENTS",
            "LINGXI_DISABLE_BACKGROUND_TASKS",
            "CLAUDE_CODE_DISABLE_BACKGROUND_TASKS",
        ] {
            std::env::remove_var(var);
        }

        assert!(!observer_agents_enabled(), "default external run stays off");

        // The upstream spelling is NOT read any more. Without this the rename
        // would be indistinguishable from having added a second alias.
        std::env::set_var("CLAUDE_CODE_EXPERIMENTAL_OBSERVER_AGENTS", "1");
        assert!(
            !observer_agents_enabled(),
            "the upstream name must no longer enable observers"
        );
        std::env::remove_var("CLAUDE_CODE_EXPERIMENTAL_OBSERVER_AGENTS");

        std::env::set_var(FLAG, "1");
        assert!(
            observer_agents_enabled(),
            "the LingXi flag enables observers"
        );

        // Both background-disable spellings still force it off. The upstream
        // one is honoured on purpose: a user migrating from claude-code sets it
        // to mean "no background work", and ignoring that would start work they
        // asked to stop.
        for disable in [
            "LINGXI_DISABLE_BACKGROUND_TASKS",
            "CLAUDE_CODE_DISABLE_BACKGROUND_TASKS",
        ] {
            std::env::set_var(disable, "1");
            assert!(
                !observer_agents_enabled(),
                "{disable} must force observers off"
            );
            std::env::remove_var(disable);
        }

        // Truthiness is the shared parser's, not a bare `is_set` check.
        std::env::set_var(FLAG, "true");
        assert!(observer_agents_enabled(), "`true` enables observers");
        std::env::set_var(FLAG, "0");
        assert!(!observer_agents_enabled(), "`0` does not enable observers");

        std::env::remove_var(FLAG);
    }
}
