//! Process-global invoked-skill registry — the Rust mirror of the binary's
//! module-level `Pt.invokedSkills` Map.
//!
//! A skill's expanded content is registered here when the model invokes it
//! (`zSr`, from the Skill tool) and consumed after a compaction by
//! `restore_post_compact_skills` (`rRg`) so the model re-reads the
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

use sha2::{Digest, Sha256};
use std::collections::{HashMap, HashSet, VecDeque};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{LazyLock, Mutex};
use std::time::{SystemTime, UNIX_EPOCH};

use crate::types::utf16_json::Utf16JsonProjection;
use crate::types::MessageId;

/// A registry snapshot eligible for post-compaction restoration.
#[derive(Debug, Clone)]
pub struct SkillRestoreCandidate {
    /// Key used to write updates back to this activation.
    pub key: Vec<u16>,
    /// Canonical Skill name.
    pub name: Utf16JsonProjection,
    /// Source path.
    pub path: PathBuf,
    /// Invoked content.
    pub content: Utf16JsonProjection,
    /// Invocation time in milliseconds.
    pub invoked_at_ms: i64,
    /// Activation identity, if backed by the live registry.
    pub activation_id: Option<u64>,
}

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
#[doc(hidden)]
pub const MAX_LIFECYCLE_EVENTS: usize = 256;
#[doc(hidden)]
pub const MAX_EXPIRED_ENTRIES: usize = 256;

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
static REGISTRY: LazyLock<Mutex<InvokedSkillRegistry>> =
    LazyLock::new(|| Mutex::new(InvokedSkillRegistry::default()));

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
    register_scoped_inner(skill_name, skill_path, content, None, None, scope, None);
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
        None,
        scope,
        Some(message_id),
    );
}

/// Register a producer-identified Skill row after the owning host has accepted
/// its rewritten contents and committed its transcript (or its in-memory row
/// when no writer exists). Callers must first verify the row came from the
/// built-in Skill tool; origin data alone is not an authorization boundary.
#[must_use]
pub fn is_individual_skill_activation(message: &crate::types::ConversationMessage) -> bool {
    matches!(message, crate::types::ConversationMessage::User { origin: Some(origin), .. }
        if origin.string_at("/type") == Some("skill_activation")
            && origin.string_units("/skill_name").is_some()
            && origin.string_at("/skill_path").is_some())
}

/// Remove a Skill marker supplied by any non-Skill tool, so tool-created task
/// evidence is never mistaken for host-owned Skill instructions.
pub fn clear_untrusted_skill_origin(message: &mut crate::types::ConversationMessage) {
    if let crate::types::ConversationMessage::User { origin, .. } = message {
        if origin
            .as_ref()
            .is_some_and(|value| value.string_at("/type") == Some("skill_activation"))
        {
            *origin = None;
        }
    }
}

/// Verify that an individual producer-identified Skill belongs to the current
/// context before beginning its durable publication.
#[must_use]
pub fn is_current_individual_skill_activation(
    message: &crate::types::ConversationMessage,
    scope: InvokedSkillScopeRef<'_>,
) -> bool {
    matches!(message, crate::types::ConversationMessage::User { origin: Some(origin), .. }
        if is_individual_skill_activation(message)
            && origin.number_at("/context_epoch") == Some(context_epoch(scope) as f64))
}

/// Register the accepted body and host origin of a committed Skill row.
pub fn register_accepted_skill_message(
    message: &crate::types::ConversationMessage,
    scope: InvokedSkillScopeRef<'_>,
) -> bool {
    use crate::types::{ContentBlock, ConversationMessage};
    let ConversationMessage::User {
        id,
        content,
        origin: Some(origin),
        ..
    } = message
    else {
        return false;
    };
    if origin.string_at("/type") != Some("skill_activation")
        || origin.number_at("/context_epoch") != Some(context_epoch(scope) as f64)
    {
        return false;
    }
    let Ok(name) = origin.subprojection("/skill_name") else {
        return false;
    };
    if name.string_units("").is_none() {
        return false;
    }
    let Some(path) = origin.string_at("/skill_path") else {
        return false;
    };
    let units = content
        .iter()
        .flat_map(|block| match block {
            ContentBlock::Text { text, .. } => text.encode_utf16().collect::<Vec<_>>(),
            ContentBlock::TextJsUtf16 {
                utf16_code_units, ..
            } => utf16_code_units.clone(),
            _ => Vec::new(),
        })
        .collect::<Vec<_>>();
    let body = Utf16JsonProjection::root_string(String::from_utf16_lossy(&units), units)
        .expect("accepted Skill content preserves its UTF-16 source");
    register_restored_skill(
        &name,
        Path::new(path),
        &body,
        origin.string_at("/skill_definition_sha256"),
        scope,
        *id,
    );
    true
}

fn parse_definition_digest(hex: &str) -> Option<[u8; 32]> {
    if hex.len() != 64 || !hex.is_ascii() {
        return None;
    }
    let mut digest = [0u8; 32];
    for (index, byte) in digest.iter_mut().enumerate() {
        *byte = u8::from_str_radix(&hex[index * 2..index * 2 + 2], 16).ok()?;
    }
    Some(digest)
}

/// Restore a trusted host-persisted individual member of an aggregate.
pub fn register_restored_skill(
    name: &Utf16JsonProjection,
    path: &Path,
    body: &Utf16JsonProjection,
    definition_digest: Option<&str>,
    scope: InvokedSkillScopeRef<'_>,
    id: MessageId,
) {
    register_scoped_inner(
        name,
        path,
        body,
        None,
        definition_digest.and_then(parse_definition_digest),
        scope,
        Some(id),
    );
}

/// Structured source metadata for a post-compaction Skill attachment.
#[must_use]
pub fn skill_source_metadata(
    name: &Utf16JsonProjection,
    path: &Path,
    scope: InvokedSkillScopeRef<'_>,
) -> Utf16JsonProjection {
    let mut fields = vec![
        ("name".into(), name.clone()),
        ("path".into(), path.to_string_lossy().into_owned().into()),
    ];
    let definition = lock()
        .entries
        .get(&registry_key(scope, name))
        .filter(|entry| entry.effective)
        .map(|entry| entry.definition_sha256);
    if let Some(definition) = definition {
        let digest = definition
            .iter()
            .map(|byte| format!("{byte:02x}"))
            .collect::<String>();
        fields.push(("definition_sha256".into(), digest.into()));
    }
    Utf16JsonProjection::object(fields).expect("skill source metadata has unique keys")
}

fn register_scoped_inner(
    skill_name: &Utf16JsonProjection,
    skill_path: &Path,
    content: &Utf16JsonProjection,
    definition_content: Option<&Utf16JsonProjection>,
    definition_digest: Option<[u8; 32]>,
    scope: InvokedSkillScopeRef<'_>,
    message_id: Option<MessageId>,
) {
    let key = registry_key(scope, skill_name);
    let invocation_hash = content_sha256(content);
    let definition_sha256 = definition_digest
        .unwrap_or_else(|| definition_content.map_or(invocation_hash, content_sha256));
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
/// `restore_post_compact_skills`. `agent_id = None` selects the
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

/// Include the loader's unexpanded definition fingerprint without persisting
/// its body or exposing it to the model.
#[must_use]
pub fn skill_activation_origin_with_definition(
    epoch: u64,
    skill_name: &Utf16JsonProjection,
    skill_path: &Path,
    definition: &Utf16JsonProjection,
) -> Utf16JsonProjection {
    let mut origin = skill_activation_origin_with_skill(epoch, skill_name, skill_path);
    let digest = content_sha256(definition)
        .iter()
        .map(|byte| format!("{byte:02x}"))
        .collect::<String>();
    origin
        .set_field("skill_definition_sha256", digest.into())
        .expect("skill origin is an object");
    origin
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
pub fn is_effective_activation(key: &[u16], activation_id: u64) -> bool {
    lock()
        .entries
        .get(key)
        .is_some_and(|entry| entry.effective && entry.activation_id == activation_id)
}

/// Apply a compact write-back only to the activation the snapshot came from.
pub fn write_back_if_current(
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

/// Apply a staged compact write-back only while both activation and original
/// content still match the snapshot used to prepare it.
pub fn write_back_if_unchanged(
    key: &[u16],
    activation_id: u64,
    expected: &Utf16JsonProjection,
    replacement: &Utf16JsonProjection,
) -> bool {
    let mut registry = lock();
    let Some(entry) = registry.entries.get_mut(key) else {
        return false;
    };
    if !entry.effective
        || entry.activation_id != activation_id
        || !entry.content.semantically_eq(expected)
    {
        return false;
    }
    entry.content = replacement.clone();
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
/// `restore_post_compact_skills`.
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

/// Set invocation times for deterministic downstream ordering tests.
#[doc(hidden)]
pub fn set_invocation_times_for_test(value: i64) {
    for entry in lock().entries.values_mut() {
        entry.invoked_at_ms = value;
    }
}

/// Report count and body-clearing state of retained expired entries.
#[doc(hidden)]
pub fn expired_entries_for_test() -> (usize, bool) {
    let registry = lock();
    let entries = registry
        .entries
        .values()
        .filter(|entry| !entry.effective)
        .collect::<Vec<_>>();
    (
        entries.len(),
        entries
            .iter()
            .all(|entry| entry.content.string_at("") == Some("")),
    )
}

/// Report the number of retained lifecycle events.
#[doc(hidden)]
pub fn history_len_for_test() -> usize {
    lock().history.len()
}

/// Compute the exact UTF-16 digest for downstream assertions.
#[doc(hidden)]
pub fn content_sha256_for_test(content: &Utf16JsonProjection) -> [u8; 32] {
    content_sha256(content)
}
