//! Process-global invoked-skill registry — the Rust mirror of the binary's
//! module-level `Pt.invokedSkills` Map.
//!
//! A skill's expanded content is registered here when the model invokes it
//! (`zSr`, from the Skill tool) and consumed after a compaction by
//! [`crate::restore_post_compact_skills`] (`rRg`) so the model re-reads the
//! recently invoked skill instructions across the compact boundary — the skill
//! content survives compaction even though the boundary summary drops the
//! verbatim skill body.
//!
//! Legacy rows keep the binary's `"{agentId}:{skillName}"` key. Production rows
//! additionally carry the owning session because `LingXi` can host multiple
//! orchestrators in one process. An invocation is effective until an explicit
//! task or mode boundary retires it. Compaction restores only effective
//! activations; it does not clear the registry. Session teardown removes both
//! registry entries and their bounded, metadata-only lifecycle history.
//!
//! 1:1 with the binary quartet (`bin/claude.exe`, v2.1.207):
//! - `zSr(e,t,r,n=null)` → [`register`] (stamps `invokedAt = Date.now()`).
//! - `kGo(e)` → [`filter_for_agent`] (rows whose `agentId` matches).
//! - `n_n(e,t)` → [`write_back`] (overwrite an existing row's content; `""`
//!   clears it, a no-op when the key is absent).
//! - `N$t()` → the whole map, exposed here only via the test inspectors.

use once_cell::sync::Lazy;
use sha2::{Digest, Sha256};
use std::collections::{HashMap, HashSet, VecDeque};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::Mutex;
use std::time::{SystemTime, UNIX_EPOCH};

use lingxi_core::types::utf16_json::Utf16JsonProjection;
use lingxi_core::types::MessageId;

use crate::post_compact::SkillRestoreCandidate;

/// One current registry row, including the activation that owns its body.
#[derive(Debug, Clone)]
struct InvokedSkillEntry {
    skill_name: Utf16JsonProjection,
    skill_path: PathBuf,
    content: Utf16JsonProjection,
    invoked_at_ms: i64,
    insertion_order: u64,
    session_id: Option<String>,
    agent_id: Option<String>,
    activation_id: u64,
    effective: bool,
    message_id: Option<MessageId>,
    // The fingerprint describes the invocation body, not any later budget
    // write-back. Expired entries keep only this fixed-size content identity.
    content_sha256: [u8; 32],
    content_version: u64,
    definition_sha256: [u8; 32],
    definition_version: u64,
}

/// Why an effective skill activation was retired.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SkillExpirationReason {
    /// The main thread changed operating mode.
    ModeChange,
    /// The owning scope explicitly completed its task.
    TaskCompleted,
    /// A new invocation replaced the activation for this skill and scope.
    Reinvoked,
}

/// A lifecycle event records state without retaining a skill body or arguments.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SkillLifecycleEventKind {
    /// An explicit invocation made the skill effective.
    Activated,
    /// A formerly effective activation ceased to apply.
    Expired(SkillExpirationReason),
}

/// Bounded audit metadata, separate from the registry used for restoration.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SkillLifecycleEvent {
    /// Invoked skill name.
    pub skill_name: Utf16JsonProjection,
    /// Skill source path supplied by the loader.
    pub skill_path: PathBuf,
    /// Owning session/conversation identity, if known.
    pub session_id: Option<String>,
    /// Owning subagent identity, if known.
    pub agent_id: Option<String>,
    /// Identity of this particular activation, unique while the registry lives.
    pub activation_id: u64,
    /// Host-generated injected message, when this activation has one.
    pub message_id: Option<MessageId>,
    /// Monotonic version for this skill and scope, advanced only when the exact
    /// expanded invocation content changes. It restarts after an expired entry
    /// is evicted; `content_sha256` remains stable across eviction and sessions.
    pub content_version: u64,
    /// SHA-256 of the expanded invocation's exact UTF-16 code units encoded
    /// little-endian, including any unpaired surrogates.
    pub content_sha256: [u8; 32],
    /// Version of the loader-supplied definition body within this retained entry.
    pub definition_version: u64,
    /// SHA-256 of the loader-supplied definition body's exact UTF-16 units in
    /// little-endian order; defaults to `content_sha256` when unavailable.
    pub definition_sha256: [u8; 32],
    /// Event time in milliseconds since the Unix epoch.
    pub occurred_at_ms: i64,
    /// Lifecycle transition recorded by this event.
    pub kind: SkillLifecycleEventKind,
}

impl InvokedSkillEntry {
    fn lifecycle_event(
        &self,
        kind: SkillLifecycleEventKind,
        occurred_at_ms: i64,
    ) -> SkillLifecycleEvent {
        SkillLifecycleEvent {
            skill_name: self.skill_name.clone(),
            skill_path: self.skill_path.clone(),
            session_id: self.session_id.clone(),
            agent_id: self.agent_id.clone(),
            activation_id: self.activation_id,
            message_id: self.message_id,
            content_version: self.content_version,
            content_sha256: self.content_sha256,
            definition_version: self.definition_version,
            definition_sha256: self.definition_sha256,
            occurred_at_ms,
            kind,
        }
    }
}

/// Current metadata for a skill in the registry. Bodies and invocation
/// arguments are not exposed by this diagnostic view.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SkillActivationMetadata {
    /// Invoked skill name.
    pub skill_name: Utf16JsonProjection,
    /// Loader-supplied skill path.
    pub skill_path: PathBuf,
    /// Identity of this particular activation.
    pub activation_id: u64,
    /// Host-generated instruction message, when available.
    pub message_id: Option<MessageId>,
    /// Whether this activation is currently effective.
    pub effective: bool,
    /// Version and SHA-256 of the expanded invocation content.
    pub content_version: u64,
    /// SHA-256 of exact expanded UTF-16 units encoded little-endian.
    pub content_sha256: [u8; 32],
    /// Version of the loader-supplied definition body.
    pub definition_version: u64,
    /// SHA-256 of exact loader-supplied UTF-16 units encoded little-endian.
    pub definition_sha256: [u8; 32],
}

/// Scope that owns an invoked-skill registry row.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct InvokedSkillScopeRef<'a> {
    /// Owning session/conversation identity, if known.
    pub session_id: Option<&'a str>,
    /// Owning subagent identity, if known.
    pub agent_id: Option<&'a str>,
}

impl<'a> InvokedSkillScopeRef<'a> {
    /// Construct a borrowed registry scope.
    #[must_use]
    pub const fn new(session_id: Option<&'a str>, agent_id: Option<&'a str>) -> Self {
        Self {
            session_id,
            agent_id,
        }
    }
}

/// History is diagnostic metadata, never the source of effective state.
const MAX_LIFECYCLE_EVENTS: usize = 256;
const MAX_EXPIRED_ENTRIES: usize = 256;

#[derive(Default)]
struct InvokedSkillRegistry {
    entries: HashMap<Vec<u16>, InvokedSkillEntry>,
    history: VecDeque<SkillLifecycleEvent>,
    context_epochs: HashMap<(Option<String>, Option<String>), u64>,
    // Projection uses exact host-generated message identities, never body text.
    // Unlike the bounded diagnostic history, these must survive until their
    // corresponding messages are no longer retained in the session.
    retired_message_ids: HashMap<(Option<String>, Option<String>), HashSet<MessageId>>,
    next_activation_id: u64,
}

impl InvokedSkillRegistry {
    fn record(&mut self, event: SkillLifecycleEvent) {
        if matches!(event.kind, SkillLifecycleEventKind::Expired(_)) {
            if let Some(message_id) = event.message_id {
                self.retired_message_ids
                    .entry((event.session_id.clone(), event.agent_id.clone()))
                    .or_default()
                    .insert(message_id);
            }
        }
        if self.history.len() == MAX_LIFECYCLE_EVENTS {
            self.history.pop_front();
        }
        self.history.push_back(event);
    }

    fn prune_expired_entries(&mut self) {
        let mut expired = self
            .entries
            .iter()
            .filter(|(_, entry)| !entry.effective)
            .map(|(key, entry)| (entry.activation_id, key.clone()))
            .collect::<Vec<_>>();
        if expired.len() <= MAX_EXPIRED_ENTRIES {
            return;
        }
        let remove_count = expired.len() - MAX_EXPIRED_ENTRIES;
        expired.sort_unstable_by_key(|(activation_id, _)| *activation_id);
        for (_, key) in expired.into_iter().take(remove_count) {
            self.entries.remove(&key);
        }
    }
}

/// Registry and history share a lock so lifecycle transitions are atomic.
static REGISTRY: Lazy<Mutex<InvokedSkillRegistry>> =
    Lazy::new(|| Mutex::new(InvokedSkillRegistry::default()));

// Native v2.1.261 `le.record` uses Map.set (byte 156638452), retaining the
// original position when an existing key is overwritten. `JLn` preserves that
// order before `bXo` stably sorts by invokedAt, so same-millisecond invocations
// must not inherit HashMap's randomized iteration order.
static NEXT_INSERTION_ORDER: AtomicU64 = AtomicU64::new(0);

fn lock() -> std::sync::MutexGuard<'static, InvokedSkillRegistry> {
    REGISTRY
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner)
}

/// Registry key for a scoped invoked skill.
///
/// Legacy agent-only rows keep the historical `${agentId ?? ""}:${skillName}`
/// shape so existing tests/helpers still see `":skill"` for main-thread rows
/// when no session identity is available. Session-scoped rows add an explicit
/// `session:...|agent:...|skill:...` prefix so two orchestrators or a leader
/// and subagent can invoke the same skill name without colliding.
#[must_use]
pub fn registry_key(scope: InvokedSkillScopeRef<'_>, skill_name: &Utf16JsonProjection) -> Vec<u16> {
    let prefix = match scope.session_id {
        Some(session_id) => format!(
            "session:{session_id}|agent:{}|skill:",
            scope.agent_id.unwrap_or("")
        ),
        None => format!("{}:", scope.agent_id.unwrap_or("")),
    };
    let mut key = prefix.encode_utf16().collect::<Vec<_>>();
    key.extend(
        skill_name
            .string_units("")
            .expect("invoked skill name must be a root JSON string"),
    );
    key
}

/// `Date.now()` in ms since the Unix epoch (saturating for the pre-1970 edge).
/// Stamps `invokedAt`; restoration sorts descending on it.
fn now_ms() -> i64 {
    match SystemTime::now().duration_since(UNIX_EPOCH) {
        Ok(d) => i64::try_from(d.as_millis()).unwrap_or(i64::MAX),
        Err(_) => 0,
    }
}

fn content_sha256(content: &Utf16JsonProjection) -> [u8; 32] {
    let units = content
        .string_units("")
        .expect("invoked skill content must be a root JSON string");
    let mut hasher = Sha256::new();
    for unit in units {
        hasher.update(unit.to_le_bytes());
    }
    hasher.finalize().into()
}

/// `zSr(name, path, content, agentId)` — register (or overwrite) an invoked
/// skill, stamping `invokedAt = Date.now()`.
///
/// The Skill tool calls this at invocation time with the expanded skill content
/// the model receives; `agent_id` is `None` for the main thread (every `LingXi`
/// orchestrator runs as the main thread → key `":{name}"`).
pub fn register(
    skill_name: &Utf16JsonProjection,
    skill_path: &Path,
    content: &Utf16JsonProjection,
    agent_id: Option<&str>,
) {
    register_scoped(
        skill_name,
        skill_path,
        content,
        InvokedSkillScopeRef::new(None, agent_id),
    );
}

/// Register a skill under the provided conversation/session + agent scope.
pub fn register_scoped(
    skill_name: &Utf16JsonProjection,
    skill_path: &Path,
    content: &Utf16JsonProjection,
    scope: InvokedSkillScopeRef<'_>,
) {
    register_scoped_inner(skill_name, skill_path, content, None, scope, None);
}

/// Register an invocation and its host-generated injected message identity.
/// The identity permits request projection to retire that exact instruction
/// row without removing unrelated user-authored text with the same content.
pub fn register_scoped_for_message(
    skill_name: &Utf16JsonProjection,
    skill_path: &Path,
    content: &Utf16JsonProjection,
    scope: InvokedSkillScopeRef<'_>,
    message_id: MessageId,
) {
    register_scoped_inner(
        skill_name,
        skill_path,
        content,
        None,
        scope,
        Some(message_id),
    );
}

/// Register an activation with a host-generated message identity and, when
/// available, the loader-supplied unexpanded definition body. Its digest is
/// distinct from the digest of the argument- and shell-expanded content.
pub fn register_activation_with_message(
    skill_name: &Utf16JsonProjection,
    skill_path: &Path,
    content: &Utf16JsonProjection,
    definition_content: Option<&Utf16JsonProjection>,
    scope: InvokedSkillScopeRef<'_>,
    message_id: MessageId,
) {
    register_scoped_inner(
        skill_name,
        skill_path,
        content,
        definition_content,
        scope,
        Some(message_id),
    );
}

fn register_scoped_inner(
    skill_name: &Utf16JsonProjection,
    skill_path: &Path,
    content: &Utf16JsonProjection,
    definition_content: Option<&Utf16JsonProjection>,
    scope: InvokedSkillScopeRef<'_>,
    message_id: Option<MessageId>,
) {
    let key = registry_key(scope, skill_name);
    let invocation_hash = content_sha256(content);
    let definition_sha256 = definition_content.map_or(invocation_hash, content_sha256);
    let mut registry = lock();
    // A passive restore can revisit the same retained row more than once.
    // Do not retire the very message that still owns the active instruction,
    // or replace a known definition digest with a fallback expanded-body hash.
    if message_id.is_some()
        && registry.entries.get(&key).is_some_and(|entry| {
            entry.effective
                && entry.message_id == message_id
                && entry.content_sha256 == invocation_hash
        })
    {
        return;
    }
    let insertion_order = registry.entries.get(&key).map_or_else(
        || NEXT_INSERTION_ORDER.fetch_add(1, Ordering::Relaxed),
        |entry| entry.insertion_order,
    );
    let timestamp = now_ms();
    let content_version = registry.entries.get(&key).map_or(1, |entry| {
        if entry.content_sha256 == invocation_hash {
            entry.content_version
        } else {
            entry.content_version.saturating_add(1)
        }
    });
    let definition_version = registry.entries.get(&key).map_or(1, |entry| {
        if entry.definition_sha256 == definition_sha256 {
            entry.definition_version
        } else {
            entry.definition_version.saturating_add(1)
        }
    });
    if let Some(event) = registry
        .entries
        .get(&key)
        .filter(|entry| entry.effective)
        .map(|entry| {
            entry.lifecycle_event(
                SkillLifecycleEventKind::Expired(SkillExpirationReason::Reinvoked),
                timestamp,
            )
        })
    {
        registry.record(event);
    }
    registry.next_activation_id = registry
        .next_activation_id
        .checked_add(1)
        .expect("invoked skill activation identity exhausted");
    let entry = InvokedSkillEntry {
        skill_name: skill_name.clone(),
        skill_path: skill_path.to_path_buf(),
        content: content.clone(),
        invoked_at_ms: timestamp,
        insertion_order,
        session_id: scope.session_id.map(str::to_string),
        agent_id: scope.agent_id.map(str::to_string),
        activation_id: registry.next_activation_id,
        effective: true,
        message_id,
        content_sha256: invocation_hash,
        content_version,
        definition_sha256,
        definition_version,
    };
    registry.record(entry.lifecycle_event(SkillLifecycleEventKind::Activated, timestamp));
    registry.entries.insert(key, entry);
}

/// Return current activation metadata for the exact scope, including retained
/// expired entries. Expired entries are bounded; active entries remain while
/// effective. This diagnostic view is not restoration input.
#[must_use]
pub fn activation_metadata_for_scope(
    scope: InvokedSkillScopeRef<'_>,
) -> Vec<SkillActivationMetadata> {
    let registry = lock();
    let mut entries = registry
        .entries
        .values()
        .filter(|entry| {
            entry.session_id.as_deref() == scope.session_id
                && entry.agent_id.as_deref() == scope.agent_id
        })
        .collect::<Vec<_>>();
    entries.sort_unstable_by_key(|entry| entry.insertion_order);
    entries
        .into_iter()
        .map(|entry| SkillActivationMetadata {
            skill_name: entry.skill_name.clone(),
            skill_path: entry.skill_path.clone(),
            activation_id: entry.activation_id,
            message_id: entry.message_id,
            effective: entry.effective,
            content_version: entry.content_version,
            content_sha256: entry.content_sha256,
            definition_version: entry.definition_version,
            definition_sha256: entry.definition_sha256,
        })
        .collect()
}

/// `kGo(agentId)` — the registry rows whose `agentId` matches, projected into
/// [`SkillRestoreCandidate`]s (each carrying its registry key) for
/// [`crate::restore_post_compact_skills`]. `agent_id = None` selects the
/// main-thread rows.
#[must_use]
pub fn filter_for_agent(agent_id: Option<&str>) -> Vec<SkillRestoreCandidate> {
    filter_for_scope(InvokedSkillScopeRef::new(None, agent_id))
}

/// Effective registry rows whose session + agent scope matches exactly.
#[must_use]
pub fn filter_for_scope(scope: InvokedSkillScopeRef<'_>) -> Vec<SkillRestoreCandidate> {
    let registry = lock();
    let mut rows = registry
        .entries
        .iter()
        .filter(|(_, e)| {
            e.effective
                && e.session_id.as_deref() == scope.session_id
                && e.agent_id.as_deref() == scope.agent_id
        })
        .collect::<Vec<_>>();
    rows.sort_unstable_by_key(|(_, entry)| entry.insertion_order);
    rows.into_iter()
        .map(|(key, e)| SkillRestoreCandidate {
            key: key.clone(),
            name: e.skill_name.clone(),
            path: e.skill_path.clone(),
            content: e.content.clone(),
            invoked_at_ms: e.invoked_at_ms,
            activation_id: Some(e.activation_id),
        })
        .collect()
}

/// Retire main-thread activations when this session changes operating mode.
/// Other sessions and background agents retain their own effective state.
pub fn expire_for_mode_change(session_id: &str) {
    expire_scope(
        InvokedSkillScopeRef::new(Some(session_id), None),
        SkillExpirationReason::ModeChange,
    );
}

/// Retire the activations for a task that the caller knows is complete.
/// A normal follow-up message is not, by itself, a task completion.
pub fn complete_task(scope: InvokedSkillScopeRef<'_>) {
    expire_scope(scope, SkillExpirationReason::TaskCompleted);
}

/// Set the active context epoch for an exact session and agent scope. The host
/// restores this value from its durable profile or updates it when a mode
/// change is admitted, before producing more skill-instruction messages.
pub fn set_context_epoch(scope: InvokedSkillScopeRef<'_>, epoch: u64) {
    let key = (
        scope.session_id.map(str::to_string),
        scope.agent_id.map(str::to_string),
    );
    let mut registry = lock();
    if epoch == 0 {
        registry.context_epochs.remove(&key);
    } else {
        registry.context_epochs.insert(key, epoch);
    }
}

/// Return the active context epoch for the exact scope, defaulting to zero.
#[must_use]
pub fn context_epoch(scope: InvokedSkillScopeRef<'_>) -> u64 {
    lock()
        .context_epochs
        .get(&(
            scope.session_id.map(str::to_string),
            scope.agent_id.map(str::to_string),
        ))
        .copied()
        .unwrap_or(0)
}

/// Host-owned provenance for an injected skill instruction. Store it in the
/// message's `origin` metadata, outside provider-visible content, so it survives
/// transcript serialization and can be compared after a cold resume.
#[must_use]
pub fn skill_activation_origin(epoch: u64) -> Utf16JsonProjection {
    Utf16JsonProjection::from_serde_value(&serde_json::json!({
        "type": "skill_activation",
        "context_epoch": epoch,
    }))
    .expect("skill activation origin is valid JSON")
}

/// Provenance for a single skill body, including its source identity for
/// passive registry reconstruction after resume. The skill name retains exact
/// UTF-16 units; the path uses the same display representation as restoration.
#[must_use]
pub fn skill_activation_origin_with_skill(
    epoch: u64,
    skill_name: &Utf16JsonProjection,
    skill_path: &Path,
) -> Utf16JsonProjection {
    let epoch = Utf16JsonProjection::from_serde_value(&serde_json::json!(epoch))
        .expect("skill context epoch is valid JSON");
    Utf16JsonProjection::object(vec![
        ("type".to_string(), "skill_activation".into()),
        ("context_epoch".to_string(), epoch),
        ("skill_name".to_string(), skill_name.clone()),
        (
            "skill_path".to_string(),
            skill_path.to_string_lossy().into_owned().into(),
        ),
    ])
    .expect("skill activation origin is valid JSON")
}

fn expire_scope(scope: InvokedSkillScopeRef<'_>, reason: SkillExpirationReason) {
    let mut registry = lock();
    let timestamp = now_ms();
    let events = registry
        .entries
        .values_mut()
        .filter_map(|entry| {
            if entry.effective
                && entry.session_id.as_deref() == scope.session_id
                && entry.agent_id.as_deref() == scope.agent_id
            {
                entry.effective = false;
                let event =
                    entry.lifecycle_event(SkillLifecycleEventKind::Expired(reason), timestamp);
                // Retired bodies are no longer restoration inputs. The digest
                // and version are enough to identify a later re-invocation.
                entry.content = "".into();
                Some(event)
            } else {
                None
            }
        })
        .collect::<Vec<_>>();
    for event in events {
        registry.record(event);
    }
    registry.prune_expired_entries();
}

/// Return recent metadata for this exact scope, oldest first. History is bounded
/// process-wide and may be evicted; it is never used to determine effective state.
#[must_use]
pub fn activation_history_for_scope(scope: InvokedSkillScopeRef<'_>) -> Vec<SkillLifecycleEvent> {
    lock()
        .history
        .iter()
        .filter(|event| {
            event.session_id.as_deref() == scope.session_id
                && event.agent_id.as_deref() == scope.agent_id
        })
        .cloned()
        .collect()
}

/// Host-generated instruction rows retired in this exact scope. The caller can
/// exclude these identities from a model request while preserving the transcript.
#[must_use]
pub fn expired_message_ids_for_scope(scope: InvokedSkillScopeRef<'_>) -> HashSet<MessageId> {
    lock()
        .retired_message_ids
        .get(&(
            scope.session_id.map(str::to_string),
            scope.agent_id.map(str::to_string),
        ))
        .cloned()
        .unwrap_or_default()
}

/// Forget retired identities no longer present in the authoritative retained
/// session history. Call only with the complete retained set, under the same
/// session synchronization used to update that history.
pub fn prune_retired_message_ids(scope: InvokedSkillScopeRef<'_>, retained: &HashSet<MessageId>) {
    let mut registry = lock();
    let key = (
        scope.session_id.map(str::to_string),
        scope.agent_id.map(str::to_string),
    );
    if let Some(ids) = registry.retired_message_ids.get_mut(&key) {
        ids.retain(|id| retained.contains(id));
        if ids.is_empty() {
            registry.retired_message_ids.remove(&key);
        }
    }
}

/// Remove every invoked-skill row owned by a completed/replaced session.
pub fn clear_session(session_id: &str) {
    let mut registry = lock();
    registry
        .entries
        .retain(|_, entry| entry.session_id.as_deref() != Some(session_id));
    registry
        .history
        .retain(|event| event.session_id.as_deref() != Some(session_id));
    registry
        .retired_message_ids
        .retain(|(session, _), _| session.as_deref() != Some(session_id));
    registry
        .context_epochs
        .retain(|(session, _), _| session.as_deref() != Some(session_id));
}

/// RAII owner for one orchestrator's session-scoped registry rows.
///
/// Hosts that expose an explicit session-end seam still clear eagerly, but
/// desktop/mobile runtimes can also be dropped or rebuilt without firing that
/// seam. Keeping this guard inside the orchestrator makes teardown synchronous
/// and unconditional; replacing the live session clears the previous scope.
pub struct InvokedSkillSessionGuard {
    session_id: Mutex<String>,
}

impl InvokedSkillSessionGuard {
    /// Bind a guard to the orchestrator's initial session id.
    #[must_use]
    pub fn new(session_id: String) -> Self {
        Self {
            session_id: Mutex::new(session_id),
        }
    }

    /// Move ownership to `session_id`, clearing the previously-owned rows.
    pub fn replace(&self, session_id: String) {
        let mut current = self
            .session_id
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        if *current == session_id {
            return;
        }
        clear_session(&current);
        *current = session_id;
    }
}

impl Drop for InvokedSkillSessionGuard {
    fn drop(&mut self) {
        let session_id = self
            .session_id
            .get_mut()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        clear_session(session_id);
    }
}

/// `n_n(key, content)` — overwrite an existing entry's content. A no-op when the
/// key is absent (`if(r)…`, exactly like the binary). An empty root string clears
/// the stored body so a later compaction won't re-attach it.
pub fn write_back(key: &[u16], content: &Utf16JsonProjection) {
    if let Some(e) = lock().entries.get_mut(key) {
        e.content = content.clone();
    }
}

/// Check that a compaction snapshot still refers to the effective activation.
pub(crate) fn is_effective_activation(key: &[u16], activation_id: u64) -> bool {
    lock()
        .entries
        .get(key)
        .is_some_and(|entry| entry.effective && entry.activation_id == activation_id)
}

/// Apply a compact write-back only to the activation the snapshot came from.
pub(crate) fn write_back_if_current(
    key: &[u16],
    activation_id: u64,
    content: &Utf16JsonProjection,
) -> bool {
    let mut registry = lock();
    let Some(entry) = registry.entries.get_mut(key) else {
        return false;
    };
    if !entry.effective || entry.activation_id != activation_id {
        return false;
    }
    entry.content = content.clone();
    true
}

/// Test-only: clear the whole registry. Exposed (not `cfg(test)`) so downstream
/// crates' tests can isolate their view of this process-global. Not part of the
/// binary surface.
#[doc(hidden)]
pub fn reset_for_test() {
    let mut registry = lock();
    *registry = InvokedSkillRegistry::default();
    NEXT_INSERTION_ORDER.store(0, Ordering::Relaxed);
}

/// Test-only: the stored source content for `key`, or `None` if absent. Lets tests
/// assert the `n_n` write-back / clear behaviour of [`write_back`] and
/// [`crate::restore_post_compact_skills`].
#[doc(hidden)]
#[must_use]
pub fn content_for_test(key: &[u16]) -> Option<Utf16JsonProjection> {
    lock().entries.get(key).map(|e| e.content.clone())
}

/// Test-only serial lock guarding the process-global registry so tests across
/// this crate (here and `post_compact`) that register/write-back real rows don't
/// race. Not part of the binary surface.
#[doc(hidden)]
pub static TEST_LOCK: Mutex<()> = Mutex::new(());

#[cfg(test)]
mod tests {
    use super::*;
    use crate::post_compact::{
        restore_post_compact_skills, AttachedSkillContent, SKILL_TRUNCATION_MARKER,
    };

    // The registry is a process-global; serialize the tests that mutate it so
    // they don't race, and `reset_for_test()` at the top of each for a clean
    // view.
    fn guard() -> std::sync::MutexGuard<'static, ()> {
        let g = TEST_LOCK
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        reset_for_test();
        g
    }

    fn source(text: &str) -> Utf16JsonProjection {
        text.into()
    }

    fn units(text: &str) -> Vec<u16> {
        text.encode_utf16().collect()
    }

    fn text(value: &Utf16JsonProjection) -> &str {
        value.string_at("").expect("root string")
    }

    #[test]
    fn register_sets_key_and_filter_by_agent() {
        let _g = guard();
        register(
            &source("deploy"),
            Path::new("/skills/deploy"),
            &source("deploy body"),
            None,
        );
        register(
            &source("build"),
            Path::new("/skills/build"),
            &source("build body"),
            Some("agent:x"),
        );

        // Main-thread filter (None) sees only the `:deploy` row.
        let main = filter_for_agent(None);
        assert_eq!(main.len(), 1);
        assert_eq!(main[0].key, units(":deploy"));
        assert_eq!(text(&main[0].name), "deploy");
        assert_eq!(text(&main[0].content), "deploy body");
        assert_eq!(main[0].path, PathBuf::from("/skills/deploy"));

        // Agent filter sees only its own row.
        let agent = filter_for_agent(Some("agent:x"));
        assert_eq!(agent.len(), 1);
        assert_eq!(agent[0].key, units("agent:x:build"));
    }

    #[test]
    fn equal_timestamps_restore_in_registry_insertion_order() {
        let _g = guard();
        let expected = (0..12).map(|i| format!("skill-{i}")).collect::<Vec<_>>();
        for name in &expected {
            register(&source(name), Path::new("/s"), &source("body"), None);
        }
        // Force the millisecond collision that the native Map + stable sort
        // handles without relying on wall-clock scheduling in this test.
        for entry in lock().entries.values_mut() {
            entry.invoked_at_ms = 42;
        }
        let restored = restore_post_compact_skills(filter_for_agent(None), &[]);
        assert_eq!(
            restored
                .iter()
                .map(|skill| text(&skill.name))
                .collect::<Vec<_>>(),
            expected.iter().map(String::as_str).collect::<Vec<_>>()
        );
    }

    #[test]
    fn overwriting_a_skill_preserves_its_map_position_on_timestamp_ties() {
        let _g = guard();
        for name in ["first", "second", "third"] {
            register(&source(name), Path::new("/s"), &source("body"), None);
        }
        register(
            &source("first"),
            Path::new("/updated"),
            &source("updated"),
            None,
        );
        write_back(&units(":second"), &source("persisted"));
        for entry in lock().entries.values_mut() {
            entry.invoked_at_ms = 42;
        }
        let restored = restore_post_compact_skills(filter_for_agent(None), &[]);
        assert_eq!(
            restored
                .iter()
                .map(|skill| text(&skill.name))
                .collect::<Vec<_>>(),
            ["first", "second", "third"]
        );
        assert_eq!(text(&restored[0].content), "updated");
        assert_eq!(text(&restored[1].content), "persisted");
    }

    #[test]
    fn register_overwrites_same_key() {
        let _g = guard();
        register(&source("s"), Path::new("/s"), &source("first"), None);
        register(&source("s"), Path::new("/s"), &source("second"), None);
        let rows = filter_for_agent(None);
        assert_eq!(rows.len(), 1, "same key overwrites, not appends");
        assert_eq!(text(&rows[0].content), "second");
    }

    #[test]
    fn write_back_overwrites_and_clears() {
        let _g = guard();
        register(&source("s"), Path::new("/s"), &source("body"), None);
        let exact =
            Utf16JsonProjection::root_string("t�".to_string(), vec![0x0074, 0xD83D]).unwrap();
        write_back(&units(":s"), &exact);
        assert_eq!(content_for_test(&units(":s")), Some(exact));
        write_back(&units(":s"), &source(""));
        assert_eq!(text(&content_for_test(&units(":s")).unwrap()), "");
    }

    #[test]
    fn write_back_absent_key_is_noop() {
        let _g = guard();
        write_back(&units(":missing"), &source("x"));
        assert!(content_for_test(&units(":missing")).is_none());
    }

    #[test]
    fn registry_key_uses_empty_prefix_for_main_thread() {
        assert_eq!(
            registry_key(InvokedSkillScopeRef::new(None, None), &source("foo")),
            units(":foo")
        );
        assert_eq!(
            registry_key(
                InvokedSkillScopeRef::new(None, Some("agent:a")),
                &source("foo")
            ),
            units("agent:a:foo")
        );
    }

    #[test]
    fn scoped_rows_do_not_collide_across_session_or_agent() {
        let _g = guard();
        let session_a_main = InvokedSkillScopeRef::new(Some("sess:a"), None);
        let other_session_main = InvokedSkillScopeRef::new(Some("sess:b"), None);
        let session_a_agent = InvokedSkillScopeRef::new(Some("sess:a"), Some("agent:x"));

        register_scoped(
            &source("build"),
            Path::new("/sa"),
            &source("main a"),
            session_a_main,
        );
        register_scoped(
            &source("build"),
            Path::new("/sb"),
            &source("main b"),
            other_session_main,
        );
        register_scoped(
            &source("build"),
            Path::new("/sx"),
            &source("agent a"),
            session_a_agent,
        );

        let rows = filter_for_scope(session_a_main);
        assert_eq!(rows.len(), 1);
        assert_eq!(text(&rows[0].content), "main a");
        assert_eq!(
            rows[0].key,
            registry_key(session_a_main, &source("build")),
            "scoped key must be stable for write-back"
        );

        let rows = filter_for_scope(other_session_main);
        assert_eq!(rows.len(), 1);
        assert_eq!(text(&rows[0].content), "main b");

        let rows = filter_for_scope(session_a_agent);
        assert_eq!(rows.len(), 1);
        assert_eq!(text(&rows[0].content), "agent a");
    }

    #[test]
    fn mode_change_retires_only_the_sessions_main_thread() {
        let _g = guard();
        let main_a = InvokedSkillScopeRef::new(Some("a"), None);
        let agent_a = InvokedSkillScopeRef::new(Some("a"), Some("child"));
        let main_b = InvokedSkillScopeRef::new(Some("b"), None);
        let legacy = InvokedSkillScopeRef::new(None, None);
        let ids = [
            MessageId::new(),
            MessageId::new(),
            MessageId::new(),
            MessageId::new(),
        ];
        for (scope, id) in [
            (main_a, ids[0]),
            (agent_a, ids[1]),
            (main_b, ids[2]),
            (legacy, ids[3]),
        ] {
            register_scoped_for_message(
                &source("same"),
                Path::new("/s"),
                &source("body"),
                scope,
                id,
            );
        }
        let before = filter_for_scope(main_a);

        expire_for_mode_change("a");
        expire_for_mode_change("a"); // retirement is idempotent

        assert!(filter_for_scope(main_a).is_empty());
        assert!(restore_post_compact_skills(before, &[]).is_empty());
        for scope in [agent_a, main_b, legacy] {
            assert_eq!(filter_for_scope(scope).len(), 1);
            assert!(expired_message_ids_for_scope(scope).is_empty());
        }
        assert_eq!(
            expired_message_ids_for_scope(main_a),
            HashSet::from([ids[0]])
        );
        let history = activation_history_for_scope(main_a);
        assert_eq!(history.len(), 2);
        assert_eq!(history[0].kind, SkillLifecycleEventKind::Activated);
        assert_eq!(
            history[1].kind,
            SkillLifecycleEventKind::Expired(SkillExpirationReason::ModeChange)
        );
        assert_eq!(history[1].message_id, Some(ids[0]));
    }

    #[test]
    fn context_epochs_are_scoped_and_cleared_with_the_session() {
        let _g = guard();
        let main_a = InvokedSkillScopeRef::new(Some("a"), None);
        let agent_a = InvokedSkillScopeRef::new(Some("a"), Some("child"));
        let main_b = InvokedSkillScopeRef::new(Some("b"), None);
        assert_eq!(context_epoch(main_a), 0);
        set_context_epoch(main_a, 7);
        set_context_epoch(agent_a, 8);
        set_context_epoch(main_b, 9);
        assert_eq!(context_epoch(main_a), 7);
        assert_eq!(context_epoch(agent_a), 8);
        assert_eq!(context_epoch(main_b), 9);
        clear_session("a");
        assert_eq!(context_epoch(main_a), 0);
        assert_eq!(context_epoch(agent_a), 0);
        assert_eq!(context_epoch(main_b), 9);
        set_context_epoch(main_b, 0);
        assert_eq!(context_epoch(main_b), 0);
    }

    #[test]
    fn reinvocation_reactivates_and_versions_original_exact_content() {
        let _g = guard();
        let scope = InvokedSkillScopeRef::new(Some("session"), None);
        let name = source("build");
        let full_body = source(&"x".repeat(40_000));
        let first_id = MessageId::new();
        let second_id = MessageId::new();
        let third_id = MessageId::new();
        register_scoped_for_message(&name, Path::new("/s"), &full_body, scope, first_id);
        let old_snapshot = filter_for_scope(scope);
        let old_activation = old_snapshot[0].activation_id.unwrap();
        // A budget write-back changes the stored restoration body, not the
        // version of the content originally produced by the Skill tool.
        assert_eq!(
            restore_post_compact_skills(old_snapshot.clone(), &[]).len(),
            1
        );
        expire_for_mode_change("session");
        register_scoped_for_message(&name, Path::new("/s"), &full_body, scope, second_id);
        let current = filter_for_scope(scope);
        assert_eq!(current.len(), 1);
        assert_eq!(current[0].content, full_body);
        assert_ne!(current[0].activation_id, Some(old_activation));
        assert!(restore_post_compact_skills(old_snapshot, &[]).is_empty());
        assert!(!write_back_if_current(
            &current[0].key,
            old_activation,
            &source("stale")
        ));
        assert_eq!(filter_for_scope(scope)[0].content, full_body);

        let changed = Utf16JsonProjection::root_string("�".into(), vec![0xD800]).unwrap();
        register_scoped_for_message(&name, Path::new("/s"), &changed, scope, third_id);
        let history = activation_history_for_scope(scope);
        let activations: Vec<_> = history
            .iter()
            .filter(|event| event.kind == SkillLifecycleEventKind::Activated)
            .collect();
        assert_eq!(
            activations
                .iter()
                .map(|event| event.content_version)
                .collect::<Vec<_>>(),
            [1, 1, 2]
        );
        assert_eq!(filter_for_scope(scope)[0].content, changed);
        assert_eq!(
            expired_message_ids_for_scope(scope),
            HashSet::from([first_id, second_id])
        );
    }

    #[test]
    fn content_hash_uses_exact_utf16_and_is_stable_across_scopes() {
        let _g = guard();
        let a = InvokedSkillScopeRef::new(Some("a"), None);
        let b = InvokedSkillScopeRef::new(Some("b"), None);
        let first = Utf16JsonProjection::root_string("�".into(), vec![0xD800]).unwrap();
        let second = Utf16JsonProjection::root_string("�".into(), vec![0xD801]).unwrap();
        register_scoped(&source("s"), Path::new("/s"), &first, a);
        register_scoped(&source("s"), Path::new("/s"), &first, b);
        let first_hash = activation_history_for_scope(a)[0].content_sha256;
        assert_eq!(
            first_hash,
            activation_history_for_scope(b)[0].content_sha256
        );
        let hex = first_hash
            .iter()
            .map(|byte| format!("{byte:02x}"))
            .collect::<String>();
        assert_eq!(
            hex,
            "205022e3428b7c8276cf247b36e4e512db5651e5cb3472c253d9ee893a8ac750"
        );
        register_scoped(&source("s"), Path::new("/s"), &second, a);
        let history = activation_history_for_scope(a);
        let second_activation = history.last().unwrap();
        assert_ne!(first_hash, second_activation.content_sha256);
        assert_eq!(second_activation.content_version, 2);
    }

    #[test]
    fn definition_and_expanded_content_have_independent_versions() {
        let _g = guard();
        let scope = InvokedSkillScopeRef::new(Some("session"), None);
        let name = source("s");
        let definition = source("Run with $ARGUMENTS");
        register_activation_with_message(
            &name,
            Path::new("/s"),
            &source("Run with first"),
            Some(&definition),
            scope,
            MessageId::new(),
        );
        register_activation_with_message(
            &name,
            Path::new("/s"),
            &source("Run with second"),
            Some(&definition),
            scope,
            MessageId::new(),
        );
        let rows = activation_metadata_for_scope(scope);
        assert_eq!(rows.len(), 1);
        assert!(rows[0].effective);
        assert_eq!(rows[0].content_version, 2);
        assert_eq!(rows[0].definition_version, 1);
        assert_eq!(rows[0].definition_sha256, content_sha256(&definition));
        assert_ne!(rows[0].content_sha256, rows[0].definition_sha256);

        let revised = source("Changed source body");
        register_activation_with_message(
            &name,
            Path::new("/s"),
            &source("Run with second"),
            Some(&revised),
            scope,
            MessageId::new(),
        );
        let row = activation_metadata_for_scope(scope).remove(0);
        assert_eq!(row.content_version, 2);
        assert_eq!(row.definition_version, 2);
        assert_eq!(row.definition_sha256, content_sha256(&revised));
        expire_for_mode_change("session");
        assert!(!activation_metadata_for_scope(scope)[0].effective);
    }

    #[test]
    fn passive_restore_of_the_same_message_is_idempotent() {
        let _g = guard();
        let scope = InvokedSkillScopeRef::new(Some("session"), None);
        let name = source("s");
        let body = source("expanded value");
        let definition = source("template $ARGUMENTS");
        let id = MessageId::new();
        register_activation_with_message(
            &name,
            Path::new("/s"),
            &body,
            Some(&definition),
            scope,
            id,
        );
        let before = activation_metadata_for_scope(scope);
        register_activation_with_message(&name, Path::new("/s"), &body, None, scope, id);
        assert_eq!(activation_metadata_for_scope(scope), before);
        assert_eq!(activation_history_for_scope(scope).len(), 1);
        assert!(expired_message_ids_for_scope(scope).is_empty());
        assert_eq!(filter_for_scope(scope).len(), 1);
    }

    #[test]
    fn expired_entries_drop_bodies_and_are_bounded_without_losing_message_ids() {
        let _g = guard();
        let main = InvokedSkillScopeRef::new(Some("session"), None);
        let child = InvokedSkillScopeRef::new(Some("session"), Some("child"));
        let mut ids = Vec::new();
        for i in 0..(MAX_EXPIRED_ENTRIES + 4) {
            let id = MessageId::new();
            ids.push(id);
            register_scoped_for_message(
                &source(&format!("skill-{i}")),
                Path::new("/s"),
                &source("body"),
                main,
                id,
            );
        }
        register_scoped(
            &source("child"),
            Path::new("/s"),
            &source("still effective"),
            child,
        );
        expire_for_mode_change("session");
        {
            let registry = lock();
            let expired = registry
                .entries
                .values()
                .filter(|entry| !entry.effective)
                .collect::<Vec<_>>();
            assert_eq!(expired.len(), MAX_EXPIRED_ENTRIES);
            assert!(expired.iter().all(|entry| text(&entry.content).is_empty()));
        }
        assert_eq!(filter_for_scope(child).len(), 1);
        let retired = expired_message_ids_for_scope(main);
        assert_eq!(retired.len(), ids.len());
        assert!(
            retired.contains(&ids[0]),
            "entry eviction must not revive retained rows"
        );
        register_scoped(&source("skill-0"), Path::new("/s"), &source("body"), main);
        let reactivated = activation_history_for_scope(main).last().unwrap().clone();
        assert_eq!(
            reactivated.content_version, 1,
            "a local version restarts after eviction"
        );
        assert_eq!(reactivated.content_sha256, content_sha256(&source("body")));
    }

    #[test]
    fn completed_task_retires_exact_scope_and_history_is_bounded() {
        let _g = guard();
        let main = InvokedSkillScopeRef::new(Some("session"), None);
        let child = InvokedSkillScopeRef::new(Some("session"), Some("child"));
        let mut ids = Vec::new();
        for _ in 0..(MAX_LIFECYCLE_EVENTS + 2) {
            let id = MessageId::new();
            ids.push(id);
            register_scoped_for_message(&source("s"), Path::new("/s"), &source("same"), main, id);
        }
        register_scoped(&source("s"), Path::new("/s"), &source("child"), child);
        complete_task(main);
        assert!(filter_for_scope(main).is_empty());
        assert_eq!(filter_for_scope(child).len(), 1);
        assert_eq!(lock().history.len(), MAX_LIFECYCLE_EVENTS);
        let retired = expired_message_ids_for_scope(main);
        assert_eq!(retired.len(), ids.len());
        assert!(
            retired.contains(&ids[0]),
            "audit eviction must not revive a retained message"
        );
        assert_eq!(
            activation_history_for_scope(main).last().unwrap().kind,
            SkillLifecycleEventKind::Expired(SkillExpirationReason::TaskCompleted)
        );

        prune_retired_message_ids(main, &HashSet::from([ids[0]]));
        assert_eq!(expired_message_ids_for_scope(main), HashSet::from([ids[0]]));
        clear_session("session");
        assert!(activation_history_for_scope(main).is_empty());
        assert!(expired_message_ids_for_scope(main).is_empty());
    }

    #[test]
    fn clearing_one_session_preserves_other_scopes() {
        let _g = guard();
        let session_a = InvokedSkillScopeRef::new(Some("sess:a"), None);
        let session_b = InvokedSkillScopeRef::new(Some("sess:b"), None);
        register_scoped(&source("build"), Path::new("/a"), &source("a"), session_a);
        register_scoped(&source("build"), Path::new("/b"), &source("b"), session_b);

        clear_session("sess:a");

        assert!(filter_for_scope(session_a).is_empty());
        assert_eq!(filter_for_scope(session_b).len(), 1);
    }

    #[test]
    fn session_guard_clears_on_replace_and_drop() {
        let _g = guard();
        let session_a = InvokedSkillScopeRef::new(Some("sess:a"), None);
        let session_b = InvokedSkillScopeRef::new(Some("sess:b"), None);
        register_scoped(&source("build"), Path::new("/a"), &source("a"), session_a);
        register_scoped(&source("build"), Path::new("/b"), &source("b"), session_b);

        let owner = InvokedSkillSessionGuard::new("sess:a".to_string());
        owner.replace("sess:b".to_string());
        assert!(filter_for_scope(session_a).is_empty());
        assert_eq!(filter_for_scope(session_b).len(), 1);

        drop(owner);
        assert!(filter_for_scope(session_b).is_empty());
    }

    // --- rRg write-back semantics (n_n) via restore_post_compact_skills ----- //

    #[test]
    fn restore_writes_back_truncated_content() {
        // A kept-and-truncated candidate persists the truncated content back to
        // the registry (`if(!c&&u!==a.content)n_n(s,u)`).
        let _g = guard();
        let big = "x".repeat(40_000); // caps to ~5_000 tokens
        register(&source("trunc"), Path::new("/t"), &source(&big), None);
        let restored = restore_post_compact_skills(filter_for_agent(None), &[]);
        assert_eq!(restored.len(), 1);
        let stored = content_for_test(&units(":trunc")).expect("row present");
        assert!(
            text(&stored).ends_with(SKILL_TRUNCATION_MARKER),
            "truncated content persisted to the registry"
        );
        assert_eq!(stored, restored[0].content);
        assert_ne!(
            text(&stored),
            big,
            "the full body no longer occupies the row"
        );
    }

    #[test]
    fn restore_does_not_write_back_short_content() {
        // Short content is returned verbatim (`u===a.content`) → no write-back.
        let _g = guard();
        register(
            &source("short"),
            Path::new("/s"),
            &source("tiny body"),
            None,
        );
        let restored = restore_post_compact_skills(filter_for_agent(None), &[]);
        assert_eq!(restored.len(), 1);
        assert_eq!(text(&restored[0].content), "tiny body");
        assert_eq!(
            text(&content_for_test(&units(":short")).unwrap()),
            "tiny body"
        );
    }

    #[test]
    fn restore_body_match_counts_but_does_not_write_back() {
        // A body match (`c=true`) restores + counts toward the budget but never
        // writes back — the registry keeps the FULL (untruncated) body.
        let _g = guard();
        let big = "x".repeat(40_000);
        register(&source("bm"), Path::new("/b"), &source(&big), None);
        let already = [AttachedSkillContent::Body(source(&big))];
        let restored = restore_post_compact_skills(filter_for_agent(None), &already);
        assert_eq!(restored.len(), 1);
        assert!(text(&restored[0].content).ends_with(SKILL_TRUNCATION_MARKER));
        assert_eq!(
            text(&content_for_test(&units(":bm")).unwrap()),
            big.as_str(),
            "body match → no write-back; registry unchanged"
        );
    }

    #[test]
    fn restore_clears_registry_on_budget_overflow() {
        // Six equal ~5_000-token skills: exactly 5 fit (25_000) and 1 overflows;
        // the overflowed row's content is CLEARED (`n_n(s,"")`).
        let _g = guard();
        for i in 0..6 {
            register(
                &source(&format!("s{i}")),
                Path::new("/s"),
                &source(&"x".repeat(40_000)),
                None,
            );
        }
        let restored = restore_post_compact_skills(filter_for_agent(None), &[]);
        assert_eq!(restored.len(), 5, "5 fit, 6th overflows");
        let cleared: Vec<String> = (0..6)
            .map(|i| format!(":s{i}"))
            .filter(|k| content_for_test(&units(k)).is_some_and(|value| text(&value).is_empty()))
            .collect();
        assert_eq!(cleared.len(), 1, "exactly the overflowed row is cleared");
    }

    #[test]
    fn restore_sorts_by_invoked_at_desc() {
        // DESC by invokedAt: the most recently invoked skill leads the result.
        let _g = guard();
        register(&source("old"), Path::new("/o"), &source("old body"), None);
        std::thread::sleep(std::time::Duration::from_millis(3));
        register(&source("new"), Path::new("/n"), &source("new body"), None);
        let restored = restore_post_compact_skills(filter_for_agent(None), &[]);
        assert_eq!(restored.len(), 2);
        assert_eq!(text(&restored[0].name), "new", "most recent first");
        assert_eq!(text(&restored[1].name), "old");
    }

    #[test]
    fn restore_preserves_exact_utf16_across_repeated_compactions() {
        let _g = guard();
        let big = format!("{}😀{}", "x".repeat(19_899), "y".repeat(200));
        register(&source("repeat"), Path::new("/r"), &source(&big), None);

        let first = restore_post_compact_skills(filter_for_agent(None), &[]);
        assert_eq!(first.len(), 1);
        let first_exact = first[0].content.string_units("").unwrap();
        assert!(
            first_exact
                .windows(2)
                .any(|w| w == [0xD83D, 0x000A] || w == [0xD83D, 0x005B]),
            "wire image must retain the split surrogate at the truncation boundary"
        );

        let second = restore_post_compact_skills(filter_for_agent(None), &[]);
        assert_eq!(second.len(), 1);
        assert_eq!(second[0].content.string_units("").unwrap(), first_exact);
    }

    #[test]
    fn skill_name_and_body_keep_unpaired_units_and_distinct_keys() {
        let _g = guard();
        let scope = InvokedSkillScopeRef::new(Some("sess"), None);
        let replacement =
            Utf16JsonProjection::root_string("�a".into(), vec![0xD800, 0x0061]).unwrap();
        let other = Utf16JsonProjection::root_string("�a".into(), vec![0xD801, 0x0061]).unwrap();
        let body =
            Utf16JsonProjection::root_string("x�y".into(), vec![0x0078, 0xDFFF, 0x0079]).unwrap();
        register_scoped(&replacement, Path::new("/one"), &body, scope);
        register_scoped(&other, Path::new("/two"), &source("other"), scope);
        let restored = restore_post_compact_skills(filter_for_scope(scope), &[]);
        assert_eq!(restored.len(), 2);
        let first = restored
            .iter()
            .find(|skill| skill.name.string_units("").unwrap() == vec![0xD800, 0x0061])
            .expect("first exact key survives");
        let second = restored
            .iter()
            .find(|skill| skill.name.string_units("").unwrap() == vec![0xD801, 0x0061])
            .expect("second exact key survives");
        assert_eq!(first.name.string_units("").unwrap(), vec![0xD800, 0x0061]);
        assert_eq!(
            first.content.string_units("").unwrap(),
            vec![0x0078, 0xDFFF, 0x0079]
        );
        assert_eq!(second.name.string_units("").unwrap(), vec![0xD801, 0x0061]);
    }
}
