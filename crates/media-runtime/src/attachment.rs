//! Per-engine ownership and revocation for a host-shared media service.
//!
//! The host keeps [`MediaAttachmentHandle`] until revocation is confirmed. An
//! engine receives only the scoped [`MediaService`] returned by the handle.
//! The wrapper namespaces every owner and rejects calls after revocation starts.

use async_trait::async_trait;
use media_api::audio::{
    AudioCapabilitySnapshot, AudioError, AudioErrorKind, AudioOperation, AudioOperationContext,
    AudioOperationId, AudioOperationSuccess, AudioService,
};
use media_api::{
    MediaAttachmentId, MediaAttachmentIssuer, MediaAudioChannel, MediaError, MediaErrorKind,
    MediaOperation, MediaOperationContext, MediaOperationId, MediaOwner, MediaPcmFrame,
    MediaRenderReceipt, MediaResourceRef, MediaResourceState, MediaResult, MediaService,
};
use std::collections::{HashMap, HashSet};
use std::fmt;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex, PoisonError, Weak};
use std::time::Duration;
use tokio::sync::Notify;

static NEXT_DRAIN_OPERATION: AtomicU64 = AtomicU64::new(1);
const AUDIO_DRAIN_BUDGET: Duration = Duration::from_secs(3);
const ATTACHMENT_DRAIN_BUDGET: Duration = Duration::from_secs(30);

/// Process-local issuer held by the trusted app/platform host, not by an
/// engine or an LLM-facing tool. A new host process gets a new identity.
pub struct MediaAttachmentHost {
    issuer: Arc<MediaAttachmentIssuer>,
    attachments: Arc<Mutex<HashMap<MediaAttachmentId, Arc<ScopedMediaService>>>>,
}

impl Default for MediaAttachmentHost {
    fn default() -> Self {
        Self::new()
    }
}

impl MediaAttachmentHost {
    /// Create the attachment issuer for one host lifetime.
    #[must_use]
    pub fn new() -> Self {
        Self {
            issuer: Arc::new(MediaAttachmentIssuer::new()),
            attachments: Arc::new(Mutex::new(HashMap::new())),
        }
    }

    /// Create a distinct per-engine service scope over a shared service.
    ///
    /// The returned handle is host-owned. Pass only [`Self::service`] to the
    /// engine; keep the handle to revoke and drain that engine attachment.
    pub fn attach(
        &self,
        shared: Arc<dyn MediaService>,
    ) -> Result<MediaAttachmentHandle, MediaError> {
        let mut attachments = lock(&self.attachments);
        if attachments.values().any(|service| {
            matches!(
                lock(&service.lifecycle.state).phase,
                MediaAttachmentPhase::Revoking | MediaAttachmentPhase::Unknown
            )
        }) {
            return Err(MediaError::new(
                MediaErrorKind::Unavailable,
                "a previous media attachment has an unconfirmed drain",
            ));
        }
        let id = self.issuer.issue().ok_or_else(|| {
            MediaError::new(
                MediaErrorKind::Unavailable,
                "media attachment generation is exhausted",
            )
        })?;
        let service = Arc::new(ScopedMediaService {
            inner: shared.clone(),
            root_service: shared,
            id: id.clone(),
            lifecycle: Arc::new(Lifecycle {
                state: Mutex::new(LifecycleState {
                    phase: MediaAttachmentPhase::Active,
                    active_calls: 0,
                    host_cleanup_fences: 0,
                    media_drain_confirmed: false,
                    owners: HashSet::new(),
                    operations: HashMap::new(),
                    audio_owners: HashSet::new(),
                    audio_operations: HashMap::new(),
                    audio_end_owner_attempts: HashMap::new(),
                    children: HashMap::new(),
                    stop_attempts: HashMap::new(),
                    drain_started: false,
                    drain_running: false,
                    failure: None,
                }),
                changed: Notify::new(),
                admission_closed: Notify::new(),
            }),
            parent_lifecycle: None,
            attachments: Arc::downgrade(&self.attachments),
            audio_service: Mutex::new(None),
        });
        attachments.insert(id, service.clone());
        Ok(MediaAttachmentHandle {
            service,
            issuer: self.issuer.clone(),
            attachments: self.attachments.clone(),
        })
    }

    /// Recover host-owned handles after an engine wrapper was dropped or
    /// rebuilt. Confirmed attachments are removed; attachments whose drain
    /// reaches `Unknown` remain retained and cannot admit new work.
    #[must_use]
    pub fn attachments(&self) -> Vec<MediaAttachmentHandle> {
        lock(&self.attachments)
            .values()
            .cloned()
            .map(|service| MediaAttachmentHandle {
                service,
                issuer: self.issuer.clone(),
                attachments: self.attachments.clone(),
            })
            .collect()
    }

    /// Recover one host-owned attachment by its opaque identity.
    #[must_use]
    pub fn attachment(&self, id: &MediaAttachmentId) -> Option<MediaAttachmentHandle> {
        lock(&self.attachments)
            .get(id)
            .cloned()
            .map(|service| MediaAttachmentHandle {
                service,
                issuer: self.issuer.clone(),
                attachments: self.attachments.clone(),
            })
    }
}

/// Host-visible attachment lifecycle. `Unknown` and `Revoking` both reject
/// engine calls and retain the same attachment ownership.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum MediaAttachmentPhase {
    /// Calls are admitted.
    Active,
    /// Revocation has begun; no new calls are admitted.
    Revoking,
    /// Drain did not receive enough evidence to release ownership.
    Unknown,
    /// All tracked owner resources were confirmed stopped.
    Revoked,
}

/// Result visible to the host that owns an attachment handle.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum MediaAttachmentRevocation {
    /// All tracked active/stopping resources were confirmed drained.
    Confirmed,
    /// The caller timed out or drain could not prove shutdown. The attachment
    /// remains closed to new calls and retains its ownership record.
    Unknown {
        /// Why the attachment's resource cleanup remains unconfirmed.
        reason: String,
    },
}

/// Whether one admitted operation still has unconfirmed work.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum MediaOperationCleanup {
    /// The call and all of its native work have settled.
    Confirmed,
    /// A call or native cleanup is still in flight.
    Pending,
    /// A dropped call or native callback has no completion evidence.
    Unknown,
}

/// Host-owned lifecycle handle. Its ID can be used by trusted provider-route
/// composition; the engine receives only `service()` and cannot mint another ID.
#[derive(Clone)]
pub struct MediaAttachmentHandle {
    service: Arc<ScopedMediaService>,
    issuer: Arc<MediaAttachmentIssuer>,
    attachments: Arc<Mutex<HashMap<MediaAttachmentId, Arc<ScopedMediaService>>>>,
}

/// A host-owned fence for cleanup beyond MediaService resources, such as a
/// remote route. Dropping it does not claim cleanup succeeded; the attachment
/// stays retained until this fence is explicitly confirmed.
pub struct MediaAttachmentCleanupFence {
    service: Arc<ScopedMediaService>,
    confirmed: bool,
}

impl MediaAttachmentCleanupFence {
    /// Confirm the host cleanup represented by this fence, after media drain.
    pub fn confirm(&mut self) -> Result<(), MediaError> {
        if self.confirmed {
            return Ok(());
        }
        let complete = {
            let mut state = lock(&self.service.lifecycle.state);
            if !state.media_drain_confirmed || state.phase != MediaAttachmentPhase::Revoking {
                return Err(MediaError::new(
                    MediaErrorKind::Unavailable,
                    "media drain has not been confirmed for host cleanup",
                ));
            }
            state.host_cleanup_fences -= 1;
            self.confirmed = true;
            if state.host_cleanup_fences == 0 {
                state.phase = MediaAttachmentPhase::Revoked;
                true
            } else {
                false
            }
        };
        self.service.lifecycle.changed.notify_waiters();
        if complete {
            self.service.remove_from_host();
        }
        Ok(())
    }
}

/// Short-lived host-side permit for registration work associated with one
/// owner, such as installing its provider route. Revocation waits for every
/// permit already issued and rejects later permits.
pub struct MediaAttachmentPermit {
    attachment_id: MediaAttachmentId,
    owner: MediaOwner,
    _call: CallPermit,
}

impl fmt::Debug for MediaAttachmentPermit {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("MediaAttachmentPermit")
            .field("attachment_id", &self.attachment_id)
            .field("owner", &self.owner)
            .finish()
    }
}

impl MediaAttachmentPermit {
    /// The attachment whose route is being registered.
    #[must_use]
    pub fn attachment_id(&self) -> &MediaAttachmentId {
        &self.attachment_id
    }

    /// Attachment-scoped owner for registry keys and route checks.
    #[must_use]
    pub fn owner(&self) -> &MediaOwner {
        &self.owner
    }

    /// Logical owner inside this attachment.
    #[must_use]
    pub fn logical_owner(&self) -> &MediaOwner {
        self.owner.logical_owner()
    }
}

impl fmt::Debug for MediaAttachmentHandle {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("MediaAttachmentHandle")
            .field("id", &self.service.id)
            .field("phase", &self.phase())
            .finish()
    }
}

impl MediaAttachmentHandle {
    /// Return the opaque identity for trusted host route registration.
    #[must_use]
    pub fn id(&self) -> &MediaAttachmentId {
        &self.service.id
    }

    /// Return the engine-facing, owner-scoping media service.
    #[must_use]
    pub fn service(&self) -> Arc<dyn MediaService> {
        self.service.clone()
    }

    /// Inspect an operation originally submitted to this attachment.
    #[must_use]
    pub fn operation_cleanup(&self, context: &MediaOperationContext) -> MediaOperationCleanup {
        if !context.attachment_admitted() {
            // A separately reconstructed cancellation context has no shared
            // admission bit and cannot prove this call was never dispatched.
            return MediaOperationCleanup::Unknown;
        }
        let owner = match context.owner.attachment_id() {
            Some(id) if id == self.id() => context.owner.clone(),
            None => context.owner.clone().in_attachment(self.id().clone()),
            Some(_) => return MediaOperationCleanup::Unknown,
        };
        let state = lock(&self.service.lifecycle.state);
        match state.operations.get(&context.identity) {
            None => MediaOperationCleanup::Confirmed,
            Some(lease)
                if lease.owner != owner || lease.handle != context.operation_incarnation() =>
            {
                MediaOperationCleanup::Unknown
            }
            Some(lease) if lease.native_unknown || lease.unwitnessed_abandonment => {
                MediaOperationCleanup::Unknown
            }
            Some(lease) if lease.active_calls > 0 || lease.native_pending > 0 => {
                MediaOperationCleanup::Pending
            }
            Some(_) => MediaOperationCleanup::Confirmed,
        }
    }

    /// Return the audio service scoped to this attachment's owner namespace
    /// and lifecycle. Repeated composition must use the same underlying app
    /// audio service instance.
    pub fn audio_service(
        &self,
        shared: Arc<dyn AudioService>,
    ) -> Result<Arc<dyn AudioService>, AudioError> {
        let mut configured = lock(&self.service.audio_service);
        if let Some(existing) = configured.as_ref() {
            if !Arc::ptr_eq(existing, &shared) {
                return Err(AudioError::new(
                    AudioErrorKind::InvalidRequest,
                    "media attachment is already bound to a different audio service",
                ));
            }
        } else {
            *configured = Some(shared.clone());
        }
        drop(configured);
        Ok(Arc::new(AttachmentScopedAudioService {
            inner: shared,
            attachment: self.service.clone(),
        }))
    }

    /// Issue an independent child scope over this attachment's underlying
    /// host service. The child receives a new owner namespace and lifecycle;
    /// revoking this parent closes child admission and drains every child.
    ///
    /// This capability is intended for trusted host-created sub-runtimes such
    /// as scheduled turns. Engines and tools receive only [`Self::service`].
    pub fn fork_child(&self) -> Result<Self, MediaError> {
        let mut attachments = lock(&self.attachments);
        if attachments.values().any(|service| {
            matches!(
                lock(&service.lifecycle.state).phase,
                MediaAttachmentPhase::Revoking | MediaAttachmentPhase::Unknown
            )
        }) {
            return Err(MediaError::new(
                MediaErrorKind::Unavailable,
                "a previous media attachment has an unconfirmed drain",
            ));
        }
        let mut parent = lock(&self.service.lifecycle.state);
        if parent.phase != MediaAttachmentPhase::Active {
            return Err(MediaError::new(
                MediaErrorKind::Cancelled,
                "parent media attachment is revoking or has been revoked",
            ));
        }
        let id = self.issuer.issue().ok_or_else(|| {
            MediaError::new(
                MediaErrorKind::Unavailable,
                "media attachment generation is exhausted",
            )
        })?;
        let service = Arc::new(ScopedMediaService {
            inner: self.service.root_service.clone(),
            root_service: self.service.root_service.clone(),
            id: id.clone(),
            lifecycle: Arc::new(Lifecycle {
                state: Mutex::new(LifecycleState {
                    phase: MediaAttachmentPhase::Active,
                    active_calls: 0,
                    host_cleanup_fences: 0,
                    media_drain_confirmed: false,
                    owners: HashSet::new(),
                    operations: HashMap::new(),
                    audio_owners: HashSet::new(),
                    audio_operations: HashMap::new(),
                    audio_end_owner_attempts: HashMap::new(),
                    children: HashMap::new(),
                    stop_attempts: HashMap::new(),
                    drain_started: false,
                    drain_running: false,
                    failure: None,
                }),
                changed: Notify::new(),
                admission_closed: Notify::new(),
            }),
            parent_lifecycle: Some(Arc::downgrade(&self.service.lifecycle)),
            attachments: Arc::downgrade(&self.attachments),
            audio_service: Mutex::new(None),
        });
        parent.children.insert(id.clone(), Arc::downgrade(&service));
        attachments.insert(id, service.clone());
        Ok(Self {
            service,
            issuer: self.issuer.clone(),
            attachments: self.attachments.clone(),
        })
    }

    /// Current lifecycle state. Unknown is not equivalent to closed.
    #[must_use]
    pub fn phase(&self) -> MediaAttachmentPhase {
        lock(&self.service.lifecycle.state).phase
    }

    /// Snapshot whether both this attachment and its parent admit work.
    /// Actual admission still requires [`Self::authorize_owner`]; the result
    /// can change immediately after this method returns.
    #[must_use]
    pub fn admission_is_open(&self) -> bool {
        self.service.is_active()
    }

    /// Close admission synchronously before scheduling teardown work.
    ///
    /// Hosts with an asynchronous object destructor should call this before
    /// returning from that destructor, then call [`Self::revoke`] on their
    /// process-lifetime executor. The attachment remains host-owned if the
    /// drain later reports `Unknown`.
    pub fn begin_revoke(&self) {
        self.service.close_admission_tree();
    }

    /// Wait until this attachment, or its parent, stops admitting new work.
    ///
    /// This is an admission notification, not a drain or cancellation
    /// receipt. It also completes for a late subscriber. Work already admitted
    /// may be in a synchronous poll when admission closes; the caller must
    /// retain its permit until that work and its resources have stopped.
    pub async fn wait_for_admission_closed(&self) {
        loop {
            let parent = self
                .service
                .parent_lifecycle
                .as_ref()
                .and_then(Weak::upgrade);
            let changed = self.service.lifecycle.admission_closed.notified();
            tokio::pin!(changed);
            changed.as_mut().enable();
            let mut parent_changed = parent
                .as_ref()
                .map(|parent| Box::pin(parent.admission_closed.notified()));
            if let Some(notified) = parent_changed.as_mut() {
                notified.as_mut().enable();
            }

            // Register both notifications before checking the phases, so a
            // concurrent begin_revoke cannot be lost between check and wait.
            if !self.service.is_active() {
                return;
            }
            tokio::select! {
                _ = changed => {},
                _ = async {
                    match parent_changed {
                        Some(notified) => notified.await,
                        None => std::future::pending::<()>().await,
                    }
                } => {},
            }
        }
    }

    /// Wait until the current drain flight leaves `Revoking`.
    ///
    /// This is a completion watcher for hosts that keep an attachment while a
    /// bounded observation times out. It does not start or cancel the drain,
    /// and returns `Unknown` as soon as a failed drain reaches its stable
    /// fail-closed state.
    pub async fn wait_for_drain_completion(&self) -> MediaAttachmentPhase {
        loop {
            let changed = self.service.lifecycle.changed.notified();
            tokio::pin!(changed);
            changed.as_mut().enable();
            let phase = self.phase();
            if phase != MediaAttachmentPhase::Revoking {
                return phase;
            }
            changed.await;
        }
    }

    /// Admit one host-side registration step under the same revocation fence
    /// as engine media calls. Keep the returned permit alive through the full
    /// registry update; dropping it completes that admitted registration step.
    pub fn authorize_owner(&self, owner: &MediaOwner) -> Result<MediaAttachmentPermit, MediaError> {
        let (call, scoped_owner) = self.service.enter(Some(owner), None, None)?;
        Ok(MediaAttachmentPermit {
            attachment_id: self.service.id.clone(),
            owner: scoped_owner.expect("owner was scoped"),
            _call: call,
        })
    }

    /// Retain this attachment until an independent host cleanup is confirmed.
    /// Acquire while active, before publishing routes or other host resources.
    pub fn retain_for_host_cleanup(&self) -> Result<MediaAttachmentCleanupFence, MediaError> {
        let mut state = lock(&self.service.lifecycle.state);
        if state.phase != MediaAttachmentPhase::Active {
            return Err(MediaError::new(
                MediaErrorKind::Unavailable,
                "cannot retain a closed attachment for host cleanup",
            ));
        }
        state.host_cleanup_fences = state.host_cleanup_fences.checked_add(1).ok_or_else(|| {
            MediaError::new(
                MediaErrorKind::Unavailable,
                "host cleanup fence capacity exhausted",
            )
        })?;
        Ok(MediaAttachmentCleanupFence {
            service: self.service.clone(),
            confirmed: false,
        })
    }

    /// Start the shared drain and observe its MediaService resource receipt.
    /// Host cleanup fences may still be outstanding, so a confirmed receipt here
    /// does not mean the attachment is Revoked or available for replacement.
    pub async fn drain_media(&self, wait_timeout: Duration) -> MediaAttachmentRevocation {
        self.service.start_drain();
        let wait = async {
            loop {
                let notified = self.service.lifecycle.changed.notified();
                tokio::pin!(notified);
                notified.as_mut().enable();
                let outcome = {
                    let state = lock(&self.service.lifecycle.state);
                    if state.media_drain_confirmed {
                        Some(MediaAttachmentRevocation::Confirmed)
                    } else if state.phase == MediaAttachmentPhase::Unknown && !state.drain_running {
                        Some(MediaAttachmentRevocation::Unknown {
                            reason: state
                                .failure
                                .clone()
                                .unwrap_or_else(|| "media drain is unconfirmed".into()),
                        })
                    } else {
                        None
                    }
                };
                if let Some(outcome) = outcome {
                    return outcome;
                }
                notified.await;
            }
        };
        tokio::time::timeout(wait_timeout, wait)
            .await
            .unwrap_or_else(|_| MediaAttachmentRevocation::Unknown {
                reason: "media drain remains in progress; ownership is retained".into(),
            })
    }

    /// Whether the exact attachment has a confirmed MediaService drain receipt.
    #[must_use]
    pub fn media_drain_confirmed(&self) -> bool {
        lock(&self.service.lifecycle.state).media_drain_confirmed
    }

    /// Revoke this attachment and wait at most `wait_timeout` for the shared
    /// drain flight. A caller timeout does not itself cancel the flight or free
    /// ownership. The flight has an independent budget and retains ownership
    /// as `Unknown` if that budget expires without the required receipts.
    pub async fn revoke(&self, wait_timeout: Duration) -> MediaAttachmentRevocation {
        self.service.start_drain();
        let wait = async {
            loop {
                let notified = self.service.lifecycle.changed.notified();
                tokio::pin!(notified);
                notified.as_mut().enable();
                let outcome = {
                    let state = lock(&self.service.lifecycle.state);
                    match state.phase {
                        MediaAttachmentPhase::Revoked => Some(MediaAttachmentRevocation::Confirmed),
                        MediaAttachmentPhase::Unknown if !state.drain_running => {
                            Some(MediaAttachmentRevocation::Unknown {
                                reason: state
                                    .failure
                                    .clone()
                                    .unwrap_or_else(|| "attachment drain is unconfirmed".into()),
                            })
                        }
                        MediaAttachmentPhase::Active
                        | MediaAttachmentPhase::Revoking
                        | MediaAttachmentPhase::Unknown => None,
                    }
                };
                if let Some(outcome) = outcome {
                    return outcome;
                }
                notified.await;
            }
        };
        let outcome = match tokio::time::timeout(wait_timeout, wait).await {
            Ok(outcome) => outcome,
            Err(_) => MediaAttachmentRevocation::Unknown {
                reason: "attachment drain remains in progress; ownership is retained".into(),
            },
        };
        if outcome == MediaAttachmentRevocation::Confirmed {
            lock(&self.attachments).remove(&self.service.id);
        }
        outcome
    }
}

struct ScopedMediaService {
    inner: Arc<dyn MediaService>,
    root_service: Arc<dyn MediaService>,
    id: MediaAttachmentId,
    lifecycle: Arc<Lifecycle>,
    parent_lifecycle: Option<Weak<Lifecycle>>,
    attachments: Weak<Mutex<HashMap<MediaAttachmentId, Arc<ScopedMediaService>>>>,
    audio_service: Mutex<Option<Arc<dyn AudioService>>>,
}

struct Lifecycle {
    state: Mutex<LifecycleState>,
    changed: Notify,
    admission_closed: Notify,
}

struct LifecycleState {
    phase: MediaAttachmentPhase,
    active_calls: usize,
    host_cleanup_fences: usize,
    media_drain_confirmed: bool,
    owners: HashSet<MediaOwner>,
    operations: HashMap<MediaOperationId, OperationLease>,
    audio_owners: HashSet<MediaOwner>,
    audio_operations: HashMap<AudioOperationId, AudioOperationLease>,
    audio_end_owner_attempts: HashMap<MediaOwner, AudioEndOwnerState>,
    children: HashMap<MediaAttachmentId, Weak<ScopedMediaService>>,
    stop_attempts: HashMap<StopAttemptKey, StopAttemptState>,
    drain_started: bool,
    drain_running: bool,
    failure: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Eq, Hash)]
struct StopAttemptKey {
    owner: MediaOwner,
    target: MediaResourceRef,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum StopAttemptState {
    InFlight,
    Unknown,
}

struct StopAttemptGuard {
    lifecycle: Arc<Lifecycle>,
    key: StopAttemptKey,
    settled: bool,
}

impl StopAttemptGuard {
    fn finish(&mut self, confirmed: bool) {
        if self.settled {
            return;
        }
        let mut state = lock(&self.lifecycle.state);
        if matches!(
            state.stop_attempts.get(&self.key),
            Some(StopAttemptState::InFlight)
        ) {
            if confirmed {
                state.stop_attempts.remove(&self.key);
            } else {
                state
                    .stop_attempts
                    .insert(self.key.clone(), StopAttemptState::Unknown);
            }
        }
        self.settled = true;
        drop(state);
        self.lifecycle.changed.notify_waiters();
    }
}

impl Drop for StopAttemptGuard {
    fn drop(&mut self) {
        self.finish(false);
    }
}

struct AudioEndOwnerGuard {
    lifecycle: Arc<Lifecycle>,
    owner: MediaOwner,
    settled: bool,
}

impl AudioEndOwnerGuard {
    fn finish(&mut self, confirmed: bool) {
        if self.settled {
            return;
        }
        let mut state = lock(&self.lifecycle.state);
        if matches!(
            state.audio_end_owner_attempts.get(&self.owner),
            Some(AudioEndOwnerState::InFlight)
        ) {
            if confirmed {
                state.audio_end_owner_attempts.remove(&self.owner);
                state.audio_owners.remove(self.owner.logical_owner());
            } else {
                state
                    .audio_end_owner_attempts
                    .insert(self.owner.clone(), AudioEndOwnerState::Unknown);
            }
        }
        self.settled = true;
        drop(state);
        self.lifecycle.changed.notify_waiters();
    }
}

impl Drop for AudioEndOwnerGuard {
    fn drop(&mut self) {
        self.finish(false);
    }
}

struct AudioCancelAttemptGuard {
    lifecycle: Arc<Lifecycle>,
    identity: AudioOperationId,
    owner: MediaOwner,
    handle: media_api::MediaOperationIncarnation,
    settled: bool,
}

enum AudioCancelAdmission {
    Complete,
    Wait,
    Dispatch(CallPermit, AudioCancelAttemptGuard),
}

impl AudioCancelAttemptGuard {
    fn finish(&mut self, confirmed: bool) {
        if self.settled {
            return;
        }
        let mut state = lock(&self.lifecycle.state);
        if let Some(lease) = state.audio_operations.get_mut(&self.identity) {
            if lease.owner == self.owner && lease.handle == self.handle && lease.cancel_in_flight {
                lease.cancel_in_flight = false;
                lease.cancel_unknown = !confirmed;
                lease.cancel_confirmed = confirmed;
            }
        }
        self.settled = true;
        drop(state);
        self.lifecycle.changed.notify_waiters();
    }
}

impl Drop for AudioCancelAttemptGuard {
    fn drop(&mut self) {
        self.finish(false);
    }
}

struct OperationLease {
    owner: MediaOwner,
    handle: media_api::MediaOperationIncarnation,
    active_calls: usize,
    abandoned: bool,
    unwitnessed_abandonment: bool,
    native_pending: usize,
    native_unknown: bool,
}

struct AttachmentNativeWorkTracker {
    lifecycle: Arc<Lifecycle>,
    parent_lifecycle: Option<Weak<Lifecycle>>,
    identity: MediaOperationId,
    owner: MediaOwner,
    handle: media_api::MediaOperationIncarnation,
    started: Arc<std::sync::atomic::AtomicBool>,
}

impl fmt::Debug for AttachmentNativeWorkTracker {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("AttachmentNativeWorkTracker")
            .finish_non_exhaustive()
    }
}

impl media_api::MediaNativeWorkTracker for AttachmentNativeWorkTracker {
    fn begin(&self) -> Result<Box<dyn media_api::MediaNativeWork>, MediaError> {
        let parent = match &self.parent_lifecycle {
            Some(parent) => Some(parent.upgrade().ok_or_else(|| {
                MediaError::new(MediaErrorKind::Cancelled, "parent media attachment is gone")
            })?),
            None => None,
        };
        let parent_state = parent.as_ref().map(|parent| lock(&parent.state));
        if parent_state
            .as_ref()
            .is_some_and(|state| state.phase != MediaAttachmentPhase::Active)
        {
            return Err(MediaError::new(
                MediaErrorKind::Cancelled,
                "parent media attachment is revoking",
            ));
        }
        let mut state = lock(&self.lifecycle.state);
        if state.phase != MediaAttachmentPhase::Active {
            return Err(MediaError::new(
                MediaErrorKind::Cancelled,
                "media attachment is revoking",
            ));
        }
        let lease = state
            .operations
            .get_mut(&self.identity)
            .filter(|lease| {
                lease.owner == self.owner && lease.handle == self.handle && lease.active_calls > 0
            })
            .ok_or_else(|| {
                MediaError::new(
                    MediaErrorKind::Unavailable,
                    "native work has no admitted operation",
                )
            })?;
        lease.native_pending = lease.native_pending.checked_add(1).ok_or_else(|| {
            MediaError::new(MediaErrorKind::Busy, "native work count is exhausted")
        })?;
        self.started.store(true, Ordering::Release);
        Ok(Box::new(AttachmentNativeWork {
            lifecycle: self.lifecycle.clone(),
            identity: self.identity.clone(),
            owner: self.owner.clone(),
            handle: self.handle,
            confirmed: false,
        }))
    }
}

struct AttachmentNativeWork {
    lifecycle: Arc<Lifecycle>,
    identity: MediaOperationId,
    owner: MediaOwner,
    handle: media_api::MediaOperationIncarnation,
    confirmed: bool,
}

impl media_api::MediaNativeWork for AttachmentNativeWork {
    fn confirm(mut self: Box<Self>) {
        self.confirmed = true;
    }
}

impl Drop for AttachmentNativeWork {
    fn drop(&mut self) {
        let mut state = lock(&self.lifecycle.state);
        let remove = state
            .operations
            .get_mut(&self.identity)
            .is_some_and(|lease| {
                if lease.owner != self.owner || lease.handle != self.handle {
                    return false;
                }
                lease.native_pending = lease.native_pending.saturating_sub(1);
                lease.native_unknown |= !self.confirmed;
                lease.active_calls == 0
                    && lease.native_pending == 0
                    && !lease.native_unknown
                    && !lease.unwitnessed_abandonment
            });
        if remove {
            state.operations.remove(&self.identity);
        }
        drop(state);
        self.lifecycle.changed.notify_waiters();
    }
}

struct AudioOperationLease {
    owner: MediaOwner,
    handle: media_api::MediaOperationIncarnation,
    active_calls: usize,
    is_end_owner: bool,
    recording: Option<media_api::RecordingRef>,
    cancel_in_flight: bool,
    cancel_unknown: bool,
    cancel_confirmed: bool,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum AudioEndOwnerState {
    InFlight,
    Unknown,
}

struct CallPermit {
    lifecycle: Arc<Lifecycle>,
    operation: Option<MediaOperationId>,
    audio_operation: Option<AudioOperationId>,
    dispatched: bool,
    finished: bool,
    native_started: Option<Arc<std::sync::atomic::AtomicBool>>,
}

impl CallPermit {
    fn begin_dispatch(&mut self) {
        self.dispatched = true;
    }

    fn finish(&mut self) {
        self.finished = true;
    }
}

impl Drop for CallPermit {
    fn drop(&mut self) {
        let mut state = lock(&self.lifecycle.state);
        state.active_calls = state.active_calls.saturating_sub(1);
        if let Some(operation) = &self.operation {
            let remove = if let Some(lease) = state.operations.get_mut(operation) {
                lease.active_calls = lease.active_calls.saturating_sub(1);
                if self.dispatched && !self.finished {
                    lease.abandoned = true;
                    if !self
                        .native_started
                        .as_ref()
                        .is_some_and(|started| started.load(Ordering::Acquire))
                    {
                        lease.unwitnessed_abandonment = true;
                    }
                }
                lease.active_calls == 0
                    && lease.native_pending == 0
                    && !lease.native_unknown
                    && !lease.unwitnessed_abandonment
            } else {
                false
            };
            if remove {
                state.operations.remove(operation);
            }
        }
        if let Some(operation) = &self.audio_operation {
            let remove = if let Some(lease) = state.audio_operations.get_mut(operation) {
                lease.active_calls = lease.active_calls.saturating_sub(1);
                if self.dispatched && !self.finished {
                    lease.cancel_unknown = true;
                    lease.cancel_confirmed = false;
                }
                lease.active_calls == 0
                    && !lease.cancel_in_flight
                    && !lease.cancel_unknown
                    && (lease.recording.is_none() || lease.cancel_confirmed)
            } else {
                false
            };
            if remove {
                state.audio_operations.remove(operation);
            }
        }
        drop(state);
        self.lifecycle.changed.notify_waiters();
    }
}

impl ScopedMediaService {
    fn bind_native_tracker(
        &self,
        context: MediaOperationContext,
        permit: &CallPermit,
    ) -> MediaOperationContext {
        let tracker = AttachmentNativeWorkTracker {
            lifecycle: self.lifecycle.clone(),
            parent_lifecycle: self.parent_lifecycle.clone(),
            identity: context.identity.clone(),
            owner: context.owner.clone(),
            handle: context.operation_incarnation(),
            started: permit
                .native_started
                .as_ref()
                .expect("media operation permit has native state")
                .clone(),
        };
        context.with_native_work_tracker(Arc::new(tracker))
    }

    fn scoped_owner(&self, owner: &MediaOwner) -> Result<MediaOwner, MediaError> {
        if owner.attachment_id().is_some() {
            return Err(MediaError::new(
                MediaErrorKind::InvalidRequest,
                "media owner is already scoped to an attachment",
            ));
        }
        Ok(owner.clone().in_attachment(self.id.clone()))
    }

    fn enter(
        &self,
        owner: Option<&MediaOwner>,
        operation: Option<&MediaOperationId>,
        operation_handle: Option<media_api::MediaOperationIncarnation>,
    ) -> Result<(CallPermit, Option<MediaOwner>), MediaError> {
        self.enter_with_owner_tracking(owner, operation, operation_handle, true)
    }

    fn enter_with_owner_tracking(
        &self,
        owner: Option<&MediaOwner>,
        operation: Option<&MediaOperationId>,
        operation_handle: Option<media_api::MediaOperationIncarnation>,
        track_owner: bool,
    ) -> Result<(CallPermit, Option<MediaOwner>), MediaError> {
        let scoped_owner = owner.map(|owner| self.scoped_owner(owner)).transpose()?;
        let parent = match &self.parent_lifecycle {
            Some(parent) => Some(parent.upgrade().ok_or_else(|| {
                MediaError::new(
                    MediaErrorKind::Cancelled,
                    "parent media attachment is no longer available",
                )
            })?),
            None => None,
        };
        let _parent_state = parent.as_ref().map(|parent| lock(&parent.state));
        if _parent_state
            .as_ref()
            .is_some_and(|state| state.phase != MediaAttachmentPhase::Active)
        {
            return Err(MediaError::new(
                MediaErrorKind::Cancelled,
                "parent media attachment is revoking or has been revoked",
            ));
        }
        let mut state = lock(&self.lifecycle.state);
        if state.phase != MediaAttachmentPhase::Active {
            return Err(MediaError::new(
                MediaErrorKind::Cancelled,
                "media attachment is revoking or has been revoked",
            ));
        }
        let operation = operation.cloned();
        if let Some(operation) = &operation {
            let Some(owner) = scoped_owner.as_ref() else {
                return Err(MediaError::new(
                    MediaErrorKind::InvalidRequest,
                    "media operation has no owner",
                ));
            };
            if state.operations.get(operation).is_some_and(|lease| {
                lease.owner != *owner || Some(lease.handle) != operation_handle
            }) {
                return Err(MediaError::new(
                    MediaErrorKind::InvalidRequest,
                    "media operation identity is already active for another owner",
                ));
            }
        }
        state.active_calls += 1;
        // A host registration permit fences its registry update (or other
        // permit-owned work) via active_calls; it does not itself admit a
        // media operation or create a device resource. Only owners of actual
        // media operations need media status and resource drain receipts.
        if let (Some(owner), Some(_), true) = (owner, operation.as_ref(), track_owner) {
            state.owners.insert(owner.clone());
        }
        if let Some(operation) = &operation {
            match state.operations.get_mut(operation) {
                Some(lease) => lease.active_calls += 1,
                None => {
                    state.operations.insert(
                        operation.clone(),
                        OperationLease {
                            owner: scoped_owner
                                .as_ref()
                                .expect("operation owner was checked above")
                                .clone(),
                            handle: operation_handle
                                .expect("operation handle is required for an operation lease"),
                            active_calls: 1,
                            abandoned: false,
                            unwitnessed_abandonment: false,
                            native_pending: 0,
                            native_unknown: false,
                        },
                    );
                }
            }
        }
        let native_started = operation
            .as_ref()
            .map(|_| Arc::new(std::sync::atomic::AtomicBool::new(false)));
        Ok((
            CallPermit {
                lifecycle: self.lifecycle.clone(),
                operation,
                audio_operation: None,
                dispatched: false,
                finished: false,
                native_started,
            },
            scoped_owner,
        ))
    }

    fn enter_audio(
        &self,
        owner: &MediaOwner,
        identity: &AudioOperationId,
        handle: media_api::MediaOperationIncarnation,
        is_end_owner: bool,
    ) -> Result<(CallPermit, MediaOwner, Option<AudioEndOwnerGuard>), AudioError> {
        let scoped_owner = self
            .scoped_owner(owner)
            .map_err(|error| audio_error_from_media(error.kind, error.message))?;
        let parent = match &self.parent_lifecycle {
            Some(parent) => Some(parent.upgrade().ok_or_else(|| {
                AudioError::new(
                    AudioErrorKind::Cancelled,
                    "parent media attachment is no longer available",
                )
            })?),
            None => None,
        };
        let _parent_state = parent.as_ref().map(|parent| lock(&parent.state));
        if _parent_state
            .as_ref()
            .is_some_and(|state| state.phase != MediaAttachmentPhase::Active)
        {
            return Err(AudioError::new(
                AudioErrorKind::Cancelled,
                "parent media attachment is revoking or has been revoked",
            ));
        }
        let mut state = lock(&self.lifecycle.state);
        if state.phase != MediaAttachmentPhase::Active {
            return Err(AudioError::new(
                AudioErrorKind::Cancelled,
                "media attachment is revoking or has been revoked",
            ));
        }
        if let Some(previous) = state.audio_end_owner_attempts.get(&scoped_owner) {
            let (kind, message) = match previous {
                AudioEndOwnerState::InFlight => (
                    AudioErrorKind::Busy,
                    "audio owner teardown is already in flight",
                ),
                AudioEndOwnerState::Unknown => (
                    AudioErrorKind::Unavailable,
                    "a previous audio owner teardown has no completion receipt",
                ),
            };
            return Err(AudioError::new(kind, message));
        }
        if state
            .audio_operations
            .get(identity)
            .is_some_and(|lease| lease.owner != scoped_owner || lease.handle != handle)
        {
            return Err(AudioError::new(
                AudioErrorKind::InvalidRequest,
                "audio operation identity is already active for another owner",
            ));
        }
        if let Some(lease) = state.audio_operations.get(identity) {
            if lease.cancel_in_flight {
                return Err(AudioError::new(
                    AudioErrorKind::Busy,
                    "audio operation cancellation is still in flight",
                ));
            }
            if lease.cancel_unknown {
                return Err(AudioError::new(
                    AudioErrorKind::Unavailable,
                    "a previous audio cancellation has no completion receipt",
                ));
            }
            if lease.cancel_confirmed {
                return Err(AudioError::new(
                    AudioErrorKind::Cancelled,
                    "audio operation has already been cancelled",
                ));
            }
        }
        state.active_calls += 1;
        state.audio_owners.insert(owner.clone());
        match state.audio_operations.get_mut(identity) {
            Some(lease) => lease.active_calls += 1,
            None => {
                state.audio_operations.insert(
                    identity.clone(),
                    AudioOperationLease {
                        owner: scoped_owner.clone(),
                        handle,
                        active_calls: 1,
                        is_end_owner,
                        recording: None,
                        cancel_in_flight: false,
                        cancel_unknown: false,
                        cancel_confirmed: false,
                    },
                );
            }
        }
        let end_owner_attempt = if is_end_owner {
            state
                .audio_end_owner_attempts
                .insert(scoped_owner.clone(), AudioEndOwnerState::InFlight);
            Some(AudioEndOwnerGuard {
                lifecycle: self.lifecycle.clone(),
                owner: scoped_owner.clone(),
                settled: false,
            })
        } else {
            None
        };
        Ok((
            CallPermit {
                lifecycle: self.lifecycle.clone(),
                operation: None,
                audio_operation: Some(identity.clone()),
                dispatched: false,
                finished: false,
                native_started: None,
            },
            scoped_owner,
            end_owner_attempt,
        ))
    }

    fn enter_audio_cancel(
        &self,
        owner: &MediaOwner,
        identity: &AudioOperationId,
        handle: media_api::MediaOperationIncarnation,
    ) -> Result<AudioCancelAdmission, AudioError> {
        let scoped_owner = self
            .scoped_owner(owner)
            .map_err(|error| audio_error_from_media(error.kind, error.message))?;
        let parent = match &self.parent_lifecycle {
            Some(parent) => Some(parent.upgrade().ok_or_else(|| {
                AudioError::new(
                    AudioErrorKind::Cancelled,
                    "parent media attachment is no longer available",
                )
            })?),
            None => None,
        };
        let _parent_state = parent.as_ref().map(|parent| lock(&parent.state));
        if _parent_state
            .as_ref()
            .is_some_and(|state| state.phase != MediaAttachmentPhase::Active)
        {
            return Err(AudioError::new(
                AudioErrorKind::Cancelled,
                "parent media attachment is revoking or has been revoked",
            ));
        }
        let mut state = lock(&self.lifecycle.state);
        if state.phase != MediaAttachmentPhase::Active {
            return Err(AudioError::new(
                AudioErrorKind::Cancelled,
                "media attachment is revoking or has been revoked",
            ));
        }
        if let Some(previous) = state.audio_end_owner_attempts.get(&scoped_owner) {
            let (kind, message) = match previous {
                AudioEndOwnerState::InFlight => (
                    AudioErrorKind::Busy,
                    "audio owner teardown is already in flight",
                ),
                AudioEndOwnerState::Unknown => (
                    AudioErrorKind::Unavailable,
                    "a previous audio owner teardown has no completion receipt",
                ),
            };
            return Err(AudioError::new(kind, message));
        }
        let Some(lease) = state.audio_operations.get(identity) else {
            return Ok(AudioCancelAdmission::Complete);
        };
        if lease.owner != scoped_owner || lease.handle != handle {
            return Ok(AudioCancelAdmission::Complete);
        }
        if lease.cancel_in_flight || (lease.cancel_confirmed && lease.active_calls > 0) {
            return Ok(AudioCancelAdmission::Wait);
        }
        if lease.cancel_confirmed {
            return Ok(AudioCancelAdmission::Complete);
        }
        if lease.active_calls == 0 && !lease.cancel_unknown && lease.recording.is_none() {
            return Ok(AudioCancelAdmission::Complete);
        }
        let lease = state
            .audio_operations
            .get_mut(identity)
            .expect("audio operation lease was checked above");
        lease.cancel_in_flight = true;
        lease.active_calls += 1;
        state.active_calls += 1;
        let permit = CallPermit {
            lifecycle: self.lifecycle.clone(),
            operation: None,
            audio_operation: Some(identity.clone()),
            dispatched: false,
            finished: false,
            native_started: None,
        };
        let attempt = AudioCancelAttemptGuard {
            lifecycle: self.lifecycle.clone(),
            identity: identity.clone(),
            owner: scoped_owner,
            handle,
            settled: false,
        };
        Ok(AudioCancelAdmission::Dispatch(permit, attempt))
    }

    async fn wait_existing_audio_cancel(
        &self,
        identity: &AudioOperationId,
        owner: &MediaOwner,
        handle: media_api::MediaOperationIncarnation,
    ) -> Result<(), AudioError> {
        loop {
            let changed = self.lifecycle.changed.notified();
            tokio::pin!(changed);
            changed.as_mut().enable();
            let outcome = {
                let state = lock(&self.lifecycle.state);
                match state.audio_operations.get(identity) {
                    None => Some(Ok(())),
                    Some(lease) if lease.owner != *owner || lease.handle != handle => Some(Ok(())),
                    Some(lease) if lease.cancel_in_flight || lease.active_calls > 0 => None,
                    Some(lease) if lease.cancel_unknown => Some(Err(AudioError::new(
                        AudioErrorKind::Unavailable,
                        "audio operation cleanup has no completion receipt",
                    ))),
                    Some(lease) if lease.cancel_confirmed => Some(Ok(())),
                    Some(_) => Some(Err(AudioError::new(
                        AudioErrorKind::Unavailable,
                        "audio cancellation has no completion receipt",
                    ))),
                }
            };
            if let Some(outcome) = outcome {
                return outcome;
            }
            changed.await;
        }
    }

    fn begin_stop_attempt(
        &self,
        owner: &MediaOwner,
        target: &MediaResourceRef,
    ) -> Result<StopAttemptGuard, MediaError> {
        let key = StopAttemptKey {
            owner: owner.clone(),
            target: target.clone(),
        };
        let mut state = lock(&self.lifecycle.state);
        if let Some(previous) = state.stop_attempts.get(&key) {
            let (kind, message) = match previous {
                StopAttemptState::InFlight => (
                    MediaErrorKind::Busy,
                    "a stop for this attachment resource is already in flight",
                ),
                StopAttemptState::Unknown => (
                    MediaErrorKind::Unavailable,
                    "a previous stop has no completion receipt; the resource remains fenced",
                ),
            };
            return Err(MediaError::new(kind, message));
        }
        state
            .stop_attempts
            .insert(key.clone(), StopAttemptState::InFlight);
        drop(state);
        Ok(StopAttemptGuard {
            lifecycle: self.lifecycle.clone(),
            key,
            settled: false,
        })
    }

    fn stop_attempt_state(
        &self,
        owner: &MediaOwner,
        target: &MediaResourceRef,
    ) -> Option<StopAttemptState> {
        let key = StopAttemptKey {
            owner: owner.clone(),
            target: target.clone(),
        };
        lock(&self.lifecycle.state).stop_attempts.get(&key).copied()
    }

    fn begin_audio_end_owner(&self, owner: &MediaOwner) -> Result<AudioEndOwnerGuard, AudioError> {
        let mut state = lock(&self.lifecycle.state);
        if let Some(previous) = state.audio_end_owner_attempts.get(owner) {
            let (kind, message) = match previous {
                AudioEndOwnerState::InFlight => (
                    AudioErrorKind::Busy,
                    "audio owner teardown is already in flight",
                ),
                AudioEndOwnerState::Unknown => (
                    AudioErrorKind::Unavailable,
                    "a previous audio owner teardown has no completion receipt",
                ),
            };
            return Err(AudioError::new(kind, message));
        }
        state
            .audio_end_owner_attempts
            .insert(owner.clone(), AudioEndOwnerState::InFlight);
        Ok(AudioEndOwnerGuard {
            lifecycle: self.lifecycle.clone(),
            owner: owner.clone(),
            settled: false,
        })
    }

    fn audio_end_owner_state(&self, owner: &MediaOwner) -> Option<AudioEndOwnerState> {
        lock(&self.lifecycle.state)
            .audio_end_owner_attempts
            .get(owner)
            .copied()
    }

    // Never hold a parent lock while acquiring a child lock or the global
    // attachments map. `fork_child` holds the map, then one parent lock;
    // taking the child snapshot under its own lock makes the race either
    // register that child before this visit or reject after admission closes.
    fn close_admission_tree(self: &Arc<Self>) {
        let mut pending = vec![self.clone()];
        while let Some(service) = pending.pop() {
            let (closed, children) = {
                let mut state = lock(&service.lifecycle.state);
                let closed = state.phase == MediaAttachmentPhase::Active;
                if closed {
                    state.phase = MediaAttachmentPhase::Revoking;
                }
                let children = state
                    .children
                    .values()
                    .filter_map(Weak::upgrade)
                    .collect::<Vec<_>>();
                (closed, children)
            };
            if closed {
                service.lifecycle.admission_closed.notify_waiters();
                service.lifecycle.changed.notify_waiters();
            }
            pending.extend(children);
        }
    }

    fn start_drain(self: &Arc<Self>) {
        self.close_admission_tree();
        self.start_closed_drain();
    }

    // The caller has already synchronously closed the whole subtree.
    fn start_closed_drain(self: &Arc<Self>) {
        let should_spawn = {
            let mut state = lock(&self.lifecycle.state);
            if state.phase == MediaAttachmentPhase::Revoking && !state.drain_started {
                state.drain_started = true;
                state.drain_running = true;
                true
            } else {
                false
            }
        };
        if should_spawn {
            let this = self.clone();
            tokio::spawn(async move {
                if tokio::time::timeout(ATTACHMENT_DRAIN_BUDGET, this.clone().drain())
                    .await
                    .is_err()
                {
                    // A timed-out native call has no trustworthy completion
                    // receipt. Keep every lease and route fenced even if that
                    // call later finishes outside this drain future.
                    let mut state = lock(&this.lifecycle.state);
                    state.drain_running = false;
                    state.phase = MediaAttachmentPhase::Unknown;
                    state.failure = Some("attachment drain exceeded its completion budget".into());
                    drop(state);
                    this.lifecycle.changed.notify_waiters();
                }
            });
            self.lifecycle.changed.notify_waiters();
        }
    }

    async fn drain(self: Arc<Self>) {
        let mut failures = Vec::new();
        let children = lock(&self.lifecycle.state)
            .children
            .values()
            .filter_map(Weak::upgrade)
            .collect::<Vec<_>>();
        for child in &children {
            child.start_closed_drain();
        }
        for child in children {
            if child.wait_for_drain_completion().await != MediaAttachmentPhase::Revoked {
                failures.push(format!(
                    "child media attachment {} could not be drained",
                    child.id.generation()
                ));
            }
        }
        let operations = lock(&self.lifecycle.state)
            .operations
            .iter()
            // Completed recording starts retain native work until their
            // registered resource is stopped below. Cancelling their origin
            // here would race that stop and erase its exact start identity.
            .filter(|(_, lease)| lease.active_calls > 0)
            .map(|(identity, lease)| (identity.clone(), lease.owner.clone(), lease.handle))
            .collect::<Vec<_>>();
        for (identity, owner, handle) in operations {
            let context = MediaOperationContext::new(identity, owner).with_operation_handle(handle);
            if let Err(error) = self.inner.cancel(context).await {
                failures.push(format!("operation cancellation was not confirmed: {error}"));
            }
        }
        self.cancel_audio_operations(&mut failures).await;

        let owners = lock(&self.lifecycle.state)
            .owners
            .iter()
            .cloned()
            .collect::<Vec<_>>();
        let mut attempted = HashMap::<MediaOwner, HashSet<MediaResourceRef>>::new();

        // Stop known resources before joining in-flight channel calls. Stop may
        // be what unblocks a native read; joining first can deadlock shutdown.
        for owner in &owners {
            let scoped = owner.clone().in_attachment(self.id.clone());
            match self.status(&scoped).await {
                Ok(resources) => {
                    let attempted_for_owner = attempted.entry(owner.clone()).or_default();
                    for resource in resources.into_iter().filter(|resource| {
                        matches!(
                            resource.state,
                            MediaResourceState::Active | MediaResourceState::Stopping
                        ) && is_stoppable(&resource.resource)
                    }) {
                        if let Some(previous) = self.stop_attempt_state(&scoped, &resource.resource)
                        {
                            attempted_for_owner.insert(resource.resource.clone());
                            if previous == StopAttemptState::Unknown {
                                failures
                                    .push("a prior resource stop has no completion receipt".into());
                            }
                            continue;
                        }
                        let target = resource.resource;
                        attempted_for_owner.insert(target.clone());
                        if let Err(error) = self.stop(&scoped, target.clone()).await {
                            let still_in_flight = error.kind == MediaErrorKind::Busy
                                && self.stop_attempt_state(&scoped, &target)
                                    == Some(StopAttemptState::InFlight);
                            if !still_in_flight {
                                failures.push(format!("media stop was not confirmed: {error}"));
                            }
                        }
                    }
                }
                Err(error) => failures.push(format!("media status was not confirmed: {error}")),
            }
        }

        self.wait_until_calls_finish().await;
        self.drain_audio_resources(&mut failures).await;

        // A call admitted before revocation can finish after the first status
        // snapshot. Drain newly visible resources once; never retry a resource
        // whose prior close is unknown.
        for owner in &owners {
            let scoped = owner.clone().in_attachment(self.id.clone());
            match self.status(&scoped).await {
                Ok(resources) => {
                    let attempted_for_owner = attempted.entry(owner.clone()).or_default();
                    for resource in resources.into_iter().filter(|resource| {
                        matches!(
                            resource.state,
                            MediaResourceState::Active | MediaResourceState::Stopping
                        ) && is_stoppable(&resource.resource)
                    }) {
                        if attempted_for_owner.contains(&resource.resource) {
                            failures.push(format!(
                                "resource remains active after its close attempt: {:?}",
                                resource.resource
                            ));
                            continue;
                        }
                        attempted_for_owner.insert(resource.resource.clone());
                        if let Err(error) = self.stop(&scoped, resource.resource).await {
                            failures.push(format!("late media stop was not confirmed: {error}"));
                        }
                    }
                }
                Err(error) => failures.push(format!("final media status failed: {error}")),
            }
        }

        // Native open may continue after the operation future has been dropped.
        // Join its witness only after stopping registered sessions: their stop
        // receipts are what release witnesses for normally live device leases.
        self.wait_until_native_work_finishes().await;
        if lock(&self.lifecycle.state)
            .operations
            .values()
            .any(|lease| lease.native_unknown || lease.unwitnessed_abandonment)
        {
            failures.push("a media operation or native callback has no completion receipt".into());
        }

        // A final snapshot is the only service-level evidence that no tracked
        // operation, recording or session remains active. Android adapters
        // must themselves wait for their actual native close callbacks.
        for owner in &owners {
            let scoped = owner.clone().in_attachment(self.id.clone());
            match self.status(&scoped).await {
                Ok(resources)
                    if resources.iter().any(|resource| {
                        matches!(
                            resource.state,
                            MediaResourceState::Active | MediaResourceState::Stopping
                        )
                    }) =>
                {
                    failures.push("active resources remain after drain".into())
                }
                Ok(_) => {}
                Err(error) => failures.push(format!("final media status failed: {error}")),
            }
        }

        if !lock(&self.lifecycle.state).stop_attempts.is_empty() {
            failures
                .push("one or more attachment resource stops have no completion receipt".into());
        }
        {
            let state = lock(&self.lifecycle.state);
            if !state.audio_operations.is_empty() {
                failures.push("one or more direct audio operations remain active".into());
            }
            if !state.audio_owners.is_empty() {
                failures.push("one or more direct audio owners remain active".into());
            }
            if state
                .audio_end_owner_attempts
                .values()
                .any(|attempt| *attempt == AudioEndOwnerState::InFlight)
            {
                failures.push("a direct audio owner teardown is still in flight".into());
            }
            if state
                .audio_end_owner_attempts
                .values()
                .any(|attempt| *attempt == AudioEndOwnerState::Unknown)
            {
                failures.push("a prior direct audio owner teardown is unconfirmed".into());
            }
        }

        let mut state = lock(&self.lifecycle.state);
        state.drain_running = false;
        if failures.is_empty() {
            state.media_drain_confirmed = true;
            state.phase = if state.host_cleanup_fences == 0 {
                MediaAttachmentPhase::Revoked
            } else {
                MediaAttachmentPhase::Revoking
            };
            state.failure = None;
        } else {
            state.media_drain_confirmed = false;
            state.phase = MediaAttachmentPhase::Unknown;
            state.failure = Some(failures.join("; "));
        }
        drop(state);
        self.lifecycle.changed.notify_waiters();
        if lock(&self.lifecycle.state).phase == MediaAttachmentPhase::Revoked {
            self.remove_from_host();
        }
    }

    async fn wait_for_drain_completion(&self) -> MediaAttachmentPhase {
        loop {
            let changed = self.lifecycle.changed.notified();
            tokio::pin!(changed);
            changed.as_mut().enable();
            let phase = lock(&self.lifecycle.state).phase;
            if phase != MediaAttachmentPhase::Revoking {
                return phase;
            }
            changed.await;
        }
    }

    fn remove_from_host(&self) {
        if let Some(attachments) = self.attachments.upgrade() {
            lock(&attachments).remove(&self.id);
        }
        if let Some(parent) = self.parent_lifecycle.as_ref().and_then(Weak::upgrade) {
            lock(&parent.state).children.remove(&self.id);
        }
    }

    fn is_active(&self) -> bool {
        let parent = match &self.parent_lifecycle {
            Some(parent) => match parent.upgrade() {
                Some(parent) => Some(parent),
                None => return false,
            },
            None => None,
        };
        let _parent_state = parent.as_ref().map(|parent| lock(&parent.state));
        _parent_state
            .as_ref()
            .is_none_or(|state| state.phase == MediaAttachmentPhase::Active)
            && lock(&self.lifecycle.state).phase == MediaAttachmentPhase::Active
    }

    async fn status(
        &self,
        owner: &MediaOwner,
    ) -> Result<Vec<media_api::MediaResourceStatus>, MediaError> {
        let context = self.drain_context(owner);
        let query_identity = context.identity.clone();
        let query_incarnation = context.operation_incarnation();
        match self
            .inner
            .execute(context, MediaOperation::Status { target: None })
            .await?
        {
            MediaResult::Status { mut resources } => {
                // UnifiedMediaService records each Status operation before
                // building its snapshot, so its result contains this exact
                // query as Active. Exclude only this ID; other active
                // operations remain eligible for cancellation and stop.
                resources.retain(|resource| {
                    !matches!(
                        &resource.resource,
                        MediaResourceRef::Operation { identity, incarnation }
                            if identity == &query_identity && *incarnation == query_incarnation
                    )
                });
                Ok(resources)
            }
            _ => Err(MediaError::new(
                MediaErrorKind::NativeFailure,
                "media service returned an unexpected drain status result",
            )),
        }
    }

    async fn stop(&self, owner: &MediaOwner, target: MediaResourceRef) -> Result<(), MediaError> {
        let expected = target.clone();
        // The drain passes the already scoped owner. Reuse the same key as
        // an admitted Stop so both paths share one cancellation fence.
        let mut attempt = self.begin_stop_attempt(owner, &expected)?;
        let result = self
            .inner
            .execute(self.drain_context(owner), MediaOperation::Stop { target })
            .await;
        let confirmed = stop_result_matches(&expected, &result);
        attempt.finish(confirmed);
        result?;
        if confirmed {
            Ok(())
        } else {
            Err(MediaError::new(
                MediaErrorKind::NativeFailure,
                "media service did not confirm the requested resource stop",
            ))
        }
    }

    fn drain_context(&self, owner: &MediaOwner) -> MediaOperationContext {
        let generation = NEXT_DRAIN_OPERATION.fetch_add(1, Ordering::Relaxed).max(1);
        let epoch = self.inner.capabilities_for(owner).service_epoch;
        MediaOperationContext::new(MediaOperationId::new(generation, epoch), owner.clone())
    }

    async fn wait_until_calls_finish(&self) {
        loop {
            let notified = self.lifecycle.changed.notified();
            tokio::pin!(notified);
            notified.as_mut().enable();
            if lock(&self.lifecycle.state).active_calls == 0 {
                return;
            }
            notified.await;
        }
    }

    async fn wait_until_native_work_finishes(&self) {
        loop {
            let notified = self.lifecycle.changed.notified();
            tokio::pin!(notified);
            notified.as_mut().enable();
            if !lock(&self.lifecycle.state)
                .operations
                .values()
                .any(|lease| lease.native_pending > 0)
            {
                return;
            }
            notified.await;
        }
    }

    async fn cancel_audio_operations(&self, failures: &mut Vec<String>) {
        let Some(audio) = lock(&self.audio_service).clone() else {
            if !lock(&self.lifecycle.state).audio_operations.is_empty() {
                failures.push("direct audio operations have no service for cancellation".into());
            }
            return;
        };
        let operations = lock(&self.lifecycle.state)
            .audio_operations
            .iter()
            .filter(|(_, lease)| !lease.is_end_owner)
            .map(|(identity, lease)| (identity.clone(), lease.owner.clone(), lease.handle))
            .collect::<Vec<_>>();
        for (identity, owner, handle) in operations {
            let deadline = tokio::time::Instant::now() + AUDIO_DRAIN_BUDGET;
            if let Err(error) = self
                .cancel_audio_operation(&audio, identity, owner, handle, deadline)
                .await
            {
                failures.push(format!(
                    "direct audio cancellation remains unconfirmed: {error}"
                ));
            }
        }
    }

    async fn cancel_audio_operation(
        &self,
        audio: &Arc<dyn AudioService>,
        identity: AudioOperationId,
        owner: MediaOwner,
        handle: media_api::MediaOperationIncarnation,
        deadline: tokio::time::Instant,
    ) -> Result<(), AudioError> {
        loop {
            let changed = self.lifecycle.changed.notified();
            tokio::pin!(changed);
            changed.as_mut().enable();
            enum Decision {
                Wait,
                Skip,
                Dispatch(CallPermit, AudioCancelAttemptGuard),
            }
            let decision = {
                let mut state = lock(&self.lifecycle.state);
                let Some(lease) = state.audio_operations.get(&identity) else {
                    return Ok(());
                };
                if lease.owner != owner || lease.handle != handle || lease.cancel_confirmed {
                    Decision::Skip
                } else if lease.cancel_in_flight {
                    Decision::Wait
                } else if lease.active_calls == 0
                    && !lease.cancel_unknown
                    && lease.recording.is_none()
                {
                    Decision::Skip
                } else {
                    let lease = state
                        .audio_operations
                        .get_mut(&identity)
                        .expect("audio operation lease was checked above");
                    lease.cancel_in_flight = true;
                    lease.active_calls += 1;
                    state.active_calls += 1;
                    let permit = CallPermit {
                        lifecycle: self.lifecycle.clone(),
                        operation: None,
                        audio_operation: Some(identity.clone()),
                        dispatched: false,
                        finished: false,
                        native_started: None,
                    };
                    let attempt = AudioCancelAttemptGuard {
                        lifecycle: self.lifecycle.clone(),
                        identity: identity.clone(),
                        owner: owner.clone(),
                        handle,
                        settled: false,
                    };
                    Decision::Dispatch(permit, attempt)
                }
            };
            match decision {
                Decision::Skip => return Ok(()),
                Decision::Wait => {
                    tokio::time::timeout_at(deadline, changed)
                        .await
                        .map_err(|_| {
                            AudioError::new(
                                AudioErrorKind::Timeout,
                                "audio cancellation receipt wait exceeded its budget",
                            )
                        })?;
                }
                Decision::Dispatch(permit, mut attempt) => {
                    let remaining = deadline.saturating_duration_since(tokio::time::Instant::now());
                    if remaining.is_zero() {
                        attempt.finish(false);
                        drop(permit);
                        return Err(AudioError::new(
                            AudioErrorKind::Timeout,
                            "audio cancellation budget elapsed",
                        ));
                    }
                    let context = AudioOperationContext::new(identity, owner)
                        .with_operation_handle(handle)
                        .with_timeout_budget_ms(
                            u64::try_from(remaining.as_millis()).unwrap_or(u64::MAX),
                        );
                    let result =
                        match tokio::time::timeout_at(deadline, audio.cancel(context)).await {
                            Ok(result) => result,
                            Err(_) => Err(AudioError::new(
                                AudioErrorKind::Timeout,
                                "audio cancellation exceeded its budget",
                            )),
                        };
                    attempt.finish(result.is_ok());
                    drop(permit);
                    return result;
                }
            }
        }
    }

    async fn cancel_and_join_audio_owner(
        &self,
        audio: &Arc<dyn AudioService>,
        owner: &MediaOwner,
        excluded_identity: &AudioOperationId,
        excluded_handle: media_api::MediaOperationIncarnation,
        deadline: tokio::time::Instant,
    ) -> Result<(), AudioError> {
        let operations = lock(&self.lifecycle.state)
            .audio_operations
            .iter()
            .filter(|(identity, lease)| {
                lease.owner == *owner
                    && !(*identity == excluded_identity && lease.handle == excluded_handle)
            })
            .map(|(identity, lease)| (identity.clone(), lease.owner.clone(), lease.handle))
            .collect::<Vec<_>>();
        for (identity, owner, handle) in operations {
            self.cancel_audio_operation(audio, identity, owner, handle, deadline)
                .await?;
        }

        loop {
            let changed = self.lifecycle.changed.notified();
            tokio::pin!(changed);
            changed.as_mut().enable();
            let pending =
                lock(&self.lifecycle.state)
                    .audio_operations
                    .iter()
                    .any(|(identity, lease)| {
                        lease.owner == *owner
                            && !(identity == excluded_identity && lease.handle == excluded_handle)
                            && lease.active_calls > 0
                    });
            if !pending {
                return Ok(());
            }
            tokio::time::timeout_at(deadline, changed)
                .await
                .map_err(|_| {
                    AudioError::new(
                        AudioErrorKind::Timeout,
                        "audio owner operations did not finish within the teardown budget",
                    )
                })?;
        }
    }

    async fn drain_audio_resources(&self, failures: &mut Vec<String>) {
        let Some(audio) = lock(&self.audio_service).clone() else {
            if !lock(&self.lifecycle.state).audio_owners.is_empty() {
                failures.push("direct audio owners have no service for teardown".into());
            }
            return;
        };
        let owners = lock(&self.lifecycle.state)
            .audio_owners
            .iter()
            .map(|owner| owner.clone().in_attachment(self.id.clone()))
            .collect::<Vec<_>>();
        for owner in owners {
            match self.audio_end_owner_state(&owner) {
                Some(AudioEndOwnerState::Unknown) => {
                    failures.push(
                        "a prior direct audio owner teardown has no completion receipt".into(),
                    );
                    continue;
                }
                Some(AudioEndOwnerState::InFlight) => {
                    failures.push("a direct audio owner teardown is still in flight".into());
                    continue;
                }
                None => {}
            }
            let mut attempt = match self.begin_audio_end_owner(&owner) {
                Ok(attempt) => attempt,
                Err(error) => {
                    failures.push(format!(
                        "direct audio owner teardown could not start: {error}"
                    ));
                    continue;
                }
            };
            let ended = match tokio::time::timeout(
                AUDIO_DRAIN_BUDGET,
                audio.execute(
                    next_audio_drain_context(audio.as_ref(), owner.clone()),
                    AudioOperation::EndOwner,
                ),
            )
            .await
            {
                Ok(Ok(AudioOperationSuccess::OwnerEnded)) => true,
                Ok(Ok(_)) => {
                    failures.push("audio service returned no owner teardown receipt".into());
                    false
                }
                Ok(Err(error)) => {
                    failures.push(format!("direct audio owner teardown failed: {error}"));
                    false
                }
                Err(_) => {
                    failures.push("direct audio owner teardown timed out".into());
                    false
                }
            };
            if !ended {
                attempt.finish(false);
                continue;
            }

            let status_confirmed = match tokio::time::timeout(
                AUDIO_DRAIN_BUDGET,
                audio.execute(
                    next_audio_drain_context(audio.as_ref(), owner.clone()),
                    AudioOperation::Status { handle: None },
                ),
            )
            .await
            {
                Ok(Ok(AudioOperationSuccess::Status { status }))
                    if !status.recording && !status.playing =>
                {
                    true
                }
                Ok(Ok(AudioOperationSuccess::Status { .. })) => {
                    failures.push("direct audio remains active after owner teardown".into());
                    false
                }
                Ok(Ok(_)) => {
                    failures.push("audio service returned an invalid teardown status".into());
                    false
                }
                Ok(Err(error)) => {
                    failures.push(format!("direct audio status failed: {error}"));
                    false
                }
                Err(_) => {
                    failures.push("direct audio status timed out".into());
                    false
                }
            };
            attempt.finish(status_confirmed);
        }
    }
}

fn next_audio_drain_context(audio: &dyn AudioService, owner: MediaOwner) -> AudioOperationContext {
    let capabilities = audio.capabilities_for(&owner);
    let generation = NEXT_DRAIN_OPERATION.fetch_add(1, Ordering::Relaxed).max(1);
    AudioOperationContext::new(
        AudioOperationId::new(generation, capabilities.service_epoch),
        owner,
    )
    .with_timeout_budget_ms(u64::try_from(AUDIO_DRAIN_BUDGET.as_millis()).unwrap_or(u64::MAX))
    .with_max_payload_bytes(capabilities.max_payload_bytes)
}

fn remaining_audio_budget_ms(deadline: tokio::time::Instant) -> Result<u64, AudioError> {
    let remaining = deadline.saturating_duration_since(tokio::time::Instant::now());
    if remaining.is_zero() {
        return Err(AudioError::new(
            AudioErrorKind::Timeout,
            "audio teardown budget has elapsed",
        ));
    }
    Ok(u64::try_from(remaining.as_millis())
        .unwrap_or(u64::MAX)
        .max(1))
}

fn is_stoppable(resource: &MediaResourceRef) -> bool {
    matches!(
        resource,
        MediaResourceRef::Operation { .. }
            | MediaResourceRef::Recording(_)
            | MediaResourceRef::Session(_)
    )
}

fn stop_result_matches(
    target: &MediaResourceRef,
    result: &Result<MediaResult, MediaError>,
) -> bool {
    match result {
        Ok(MediaResult::Stopped { resource }) => resource == target,
        Ok(MediaResult::RecordingStopped { recording, .. }) => {
            target == &MediaResourceRef::Recording(recording.clone())
        }
        Ok(_) | Err(_) => false,
    }
}

fn lock<T>(mutex: &Mutex<T>) -> std::sync::MutexGuard<'_, T> {
    mutex.lock().unwrap_or_else(PoisonError::into_inner)
}

fn unavailable(capabilities: &mut media_api::MediaCapabilities) {
    capabilities.supported_operations.clear();
    capabilities.supported_media_kinds.clear();
    capabilities.readiness.clear();
}

fn unavailable_audio(capabilities: &mut AudioCapabilitySnapshot) {
    capabilities.supported_operations.clear();
    capabilities.readiness.clear();
}

fn audio_error_from_media(kind: MediaErrorKind, message: impl Into<String>) -> AudioError {
    let kind = match kind {
        MediaErrorKind::PermissionDenied => AudioErrorKind::PermissionDenied,
        MediaErrorKind::Busy => AudioErrorKind::Busy,
        MediaErrorKind::Cancelled => AudioErrorKind::Cancelled,
        MediaErrorKind::Timeout => AudioErrorKind::Timeout,
        MediaErrorKind::NoData => AudioErrorKind::NoSpeech,
        MediaErrorKind::NotFound => AudioErrorKind::NotRecording,
        MediaErrorKind::Unavailable => AudioErrorKind::Unavailable,
        MediaErrorKind::Unsupported => AudioErrorKind::Unsupported,
        MediaErrorKind::ModelMissing => AudioErrorKind::ModelMissing,
        MediaErrorKind::InvalidRequest => AudioErrorKind::InvalidRequest,
        MediaErrorKind::ProcessingFailed => AudioErrorKind::SynthesisFailed,
        MediaErrorKind::NativeFailure
        | MediaErrorKind::StoreFailure
        | MediaErrorKind::MediaTooLarge => AudioErrorKind::NativeFailure,
    };
    AudioError::new(kind, message)
}

#[async_trait]
impl MediaService for ScopedMediaService {
    fn capabilities(&self) -> media_api::MediaCapabilities {
        let mut capabilities = self.inner.capabilities();
        if !self.is_active() {
            unavailable(&mut capabilities);
        }
        capabilities
    }

    fn attachment_id(&self) -> Option<MediaAttachmentId> {
        Some(self.id.clone())
    }

    fn capabilities_for(&self, owner: &MediaOwner) -> media_api::MediaCapabilities {
        let mut capabilities = match self.scoped_owner(owner) {
            Ok(owner) => self.inner.capabilities_for(&owner),
            Err(_) => self.inner.capabilities(),
        };
        if !self.is_active() || owner.attachment_id().is_some() {
            unavailable(&mut capabilities);
        }
        capabilities
    }

    async fn execute(
        &self,
        mut context: MediaOperationContext,
        operation: MediaOperation,
    ) -> Result<MediaResult, MediaError> {
        let stop_target = match &operation {
            MediaOperation::Stop { target } => Some(target.clone()),
            _ => None,
        };
        // Inspecting one stored asset cannot create a media device or session.
        // Keep the operation permit and native witness, but do not add a new
        // resource owner solely for this read-only query.
        let track_owner = !matches!(
            &operation,
            MediaOperation::Status {
                target: Some(MediaResourceRef::Media(_))
            }
        );
        let (mut permit, owner) = self.enter_with_owner_tracking(
            Some(&context.owner),
            Some(&context.identity),
            Some(context.operation_incarnation()),
            track_owner,
        )?;
        context.mark_attachment_admitted();
        context.owner = owner.expect("owner was scoped");
        context = self.bind_native_tracker(context, &permit);
        let mut stop_attempt = stop_target
            .as_ref()
            .map(|target| self.begin_stop_attempt(&context.owner, target))
            .transpose()?;
        permit.begin_dispatch();
        let result = self.inner.execute(context, operation).await;
        permit.finish();
        if let (Some(target), Some(attempt)) = (stop_target.as_ref(), stop_attempt.as_mut()) {
            let confirmed = stop_result_matches(target, &result);
            attempt.finish(confirmed);
            if !confirmed && result.is_ok() {
                drop(permit);
                return Err(MediaError::new(
                    MediaErrorKind::NativeFailure,
                    "media stop returned no matching completion receipt",
                ));
            }
        }
        drop(permit);
        result
    }

    async fn attach_session(
        &self,
        mut context: MediaOperationContext,
        session: Box<dyn media_api::MediaSession>,
    ) -> Result<media_api::MediaSessionRef, MediaError> {
        let (mut permit, owner) = self.enter(
            Some(&context.owner),
            Some(&context.identity),
            Some(context.operation_incarnation()),
        )?;
        context.mark_attachment_admitted();
        context.owner = owner.expect("owner was scoped");
        context = self.bind_native_tracker(context, &permit);
        permit.begin_dispatch();
        let result = self.inner.attach_session(context, session).await;
        permit.finish();
        drop(permit);
        result
    }

    async fn session_audio(
        &self,
        mut context: MediaOperationContext,
        session: media_api::MediaSessionRef,
    ) -> Result<Arc<dyn MediaAudioChannel>, MediaError> {
        let (mut permit, owner) = self.enter(
            Some(&context.owner),
            Some(&context.identity),
            Some(context.operation_incarnation()),
        )?;
        context.mark_attachment_admitted();
        context.owner = owner.expect("owner was scoped");
        context = self.bind_native_tracker(context, &permit);
        permit.begin_dispatch();
        let result = self.inner.session_audio(context, session).await;
        permit.finish();
        drop(permit);
        result.map(|inner| {
            Arc::new(AttachmentScopedAudioChannel {
                inner,
                lifecycle: self.lifecycle.clone(),
                parent_lifecycle: self.parent_lifecycle.clone(),
            }) as Arc<dyn MediaAudioChannel>
        })
    }

    async fn read_media(
        &self,
        mut context: MediaOperationContext,
        media: media_api::MediaRef,
    ) -> Result<media_api::MediaContent, MediaError> {
        let (mut permit, owner) = self.enter(
            Some(&context.owner),
            Some(&context.identity),
            Some(context.operation_incarnation()),
        )?;
        context.mark_attachment_admitted();
        context.owner = owner.expect("owner was scoped");
        context = self.bind_native_tracker(context, &permit);
        permit.begin_dispatch();
        let result = self.inner.read_media(context, media).await;
        permit.finish();
        drop(permit);
        result
    }

    async fn cancel(&self, mut context: MediaOperationContext) -> Result<(), MediaError> {
        let owner = self.scoped_owner(&context.owner)?;
        let (permit, _) = self.enter(None, None, None)?;
        context.owner = owner.clone();
        let identity = context.identity.clone();
        let incarnation = context.operation_incarnation();
        let (owned, unresolved) = lock(&self.lifecycle.state)
            .operations
            .get(&identity)
            .filter(|lease| lease.owner == owner && lease.handle == incarnation)
            .map_or((false, false), |lease| {
                (
                    lease.active_calls > 0,
                    lease.abandoned || lease.native_pending > 0 || lease.native_unknown,
                )
            });
        let result = if owned || unresolved {
            let result = self.inner.cancel(context).await;
            // Cancellation can race native admission and may return before
            // the original call or its cleanup settles. Recheck the exact
            // incarnation after forwarding rather than trusting a snapshot.
            let still_unresolved = lock(&self.lifecycle.state)
                .operations
                .get(&identity)
                .filter(|lease| lease.owner == owner && lease.handle == incarnation)
                .is_some_and(|lease| {
                    lease.active_calls > 0
                        || lease.abandoned
                        || lease.native_pending > 0
                        || lease.native_unknown
                        || lease.unwitnessed_abandonment
                });
            if result.is_ok() && still_unresolved {
                Err(MediaError::new(
                    MediaErrorKind::Busy,
                    "media operation cleanup remains unconfirmed",
                ))
            } else {
                result
            }
        } else {
            Ok(())
        };
        drop(permit);
        result
    }
}

struct AttachmentScopedAudioService {
    inner: Arc<dyn AudioService>,
    attachment: Arc<ScopedMediaService>,
}

impl AttachmentScopedAudioService {
    async fn execute_scoped(
        &self,
        mut context: AudioOperationContext,
        operation: AudioOperation,
        host_configured_local: bool,
    ) -> Result<AudioOperationSuccess, AudioError> {
        let operation_handle = context.operation_incarnation();
        if !context.runtime_admitted() && !context.claim_operation_admission() {
            return Err(AudioError::new(
                AudioErrorKind::InvalidRequest,
                "audio context has already been admitted",
            ));
        }
        context = context.with_operation_incarnation(operation_handle);
        let is_end_owner = matches!(&operation, AudioOperation::EndOwner);
        let stopped_recording = match &operation {
            AudioOperation::StopRecording { handle } => Some(handle.clone()),
            _ => None,
        };
        let (mut permit, owner, end_owner) = self.attachment.enter_audio(
            &context.owner,
            &context.identity,
            context.operation_incarnation(),
            is_end_owner,
        )?;
        context.owner = owner.clone();
        if let Some(mut attempt) = end_owner {
            let budget = Duration::from_millis(context.timeout_budget_ms.unwrap_or(30_000))
                .min(AUDIO_DRAIN_BUDGET);
            if budget.is_zero() {
                return Err(AudioError::new(
                    AudioErrorKind::Timeout,
                    "audio owner teardown budget has elapsed",
                ));
            }
            let deadline = tokio::time::Instant::now() + budget;
            self.attachment
                .cancel_and_join_audio_owner(
                    &self.inner,
                    &owner,
                    &context.identity,
                    context.operation_incarnation(),
                    deadline,
                )
                .await?;

            context.timeout_budget_ms = Some(remaining_audio_budget_ms(deadline)?);
            let ended = tokio::time::timeout_at(deadline, async {
                if host_configured_local {
                    self.inner
                        .execute_host_configured_local(context, AudioOperation::EndOwner)
                        .await
                } else {
                    self.inner.execute(context, AudioOperation::EndOwner).await
                }
            })
            .await
            .map_err(|_| {
                AudioError::new(
                    AudioErrorKind::Timeout,
                    "audio owner teardown exceeded its budget",
                )
            })??;
            if !matches!(ended, AudioOperationSuccess::OwnerEnded) {
                return Err(AudioError::new(
                    AudioErrorKind::NativeFailure,
                    "audio service did not confirm owner teardown",
                ));
            }

            let status_context = next_audio_drain_context(self.inner.as_ref(), owner.clone())
                .with_timeout_budget_ms(remaining_audio_budget_ms(deadline)?);
            let status = tokio::time::timeout_at(
                deadline,
                self.inner
                    .execute(status_context, AudioOperation::Status { handle: None }),
            )
            .await
            .map_err(|_| {
                AudioError::new(
                    AudioErrorKind::Timeout,
                    "audio owner status did not complete within its teardown budget",
                )
            })??;
            match status {
                AudioOperationSuccess::Status { status }
                    if !status.recording && !status.playing => {}
                AudioOperationSuccess::Status { .. } => {
                    return Err(AudioError::new(
                        AudioErrorKind::NativeFailure,
                        "audio owner remains active after teardown",
                    ));
                }
                _ => {
                    return Err(AudioError::new(
                        AudioErrorKind::NativeFailure,
                        "audio service returned an invalid owner status",
                    ));
                }
            }
            attempt.finish(true);
            drop(permit);
            return Ok(AudioOperationSuccess::OwnerEnded);
        }
        let identity = context.identity.clone();
        let incarnation = context.operation_incarnation();
        // Pin the matching start before awaiting Stop. A later start may reuse
        // the same backend handle and must never be cancelled by this stop.
        let recording_origin = if let Some(recording) = &stopped_recording {
            let matches = lock(&self.attachment.lifecycle.state)
                .audio_operations
                .iter()
                .filter(|(_, lease)| {
                    lease.owner == owner && lease.recording.as_ref() == Some(recording)
                })
                .map(|(identity, lease)| (identity.clone(), lease.handle))
                .collect::<Vec<_>>();
            match matches.as_slice() {
                [] => None,
                [origin] => Some(origin.clone()),
                _ => {
                    return Err(AudioError::new(
                        AudioErrorKind::NativeFailure,
                        "recording origin is ambiguous",
                    ))
                }
            }
        } else {
            None
        };
        permit.begin_dispatch();
        let result = if host_configured_local {
            self.inner
                .execute_host_configured_local(context, operation)
                .await
        } else {
            self.inner.execute(context, operation).await
        };
        {
            let mut state = lock(&self.attachment.lifecycle.state);
            if let Some(lease) = state.audio_operations.get_mut(&identity) {
                if lease.owner == owner && lease.handle == incarnation {
                    match &result {
                        Ok(AudioOperationSuccess::RecordingStarted { handle }) => {
                            lease.recording = Some(handle.clone());
                            if lease.cancel_confirmed {
                                lease.cancel_confirmed = false;
                                lease.cancel_unknown = true;
                            }
                        }
                        Err(_) if !lease.cancel_confirmed => lease.cancel_unknown = true,
                        _ => {}
                    }
                }
            }
        }
        permit.finish();
        drop(permit);
        if result.is_err() {
            if let Ok(runtime) = tokio::runtime::Handle::try_current() {
                let attachment = self.attachment.clone();
                let audio = self.inner.clone();
                let owner = owner.clone();
                let identity = identity.clone();
                runtime.spawn(async move {
                    let deadline = tokio::time::Instant::now() + AUDIO_DRAIN_BUDGET;
                    let _ = attachment
                        .cancel_audio_operation(&audio, identity, owner, incarnation, deadline)
                        .await;
                });
            }
        }
        if let (Some((identity, incarnation)), Ok(AudioOperationSuccess::Recording { .. })) =
            (recording_origin, &result)
        {
            self.attachment
                .cancel_audio_operation(
                    &self.inner,
                    identity,
                    owner,
                    incarnation,
                    tokio::time::Instant::now() + AUDIO_DRAIN_BUDGET,
                )
                .await?;
        }
        result
    }
}

#[async_trait]
impl AudioService for AttachmentScopedAudioService {
    fn capabilities(&self) -> AudioCapabilitySnapshot {
        let mut capabilities = self.inner.capabilities();
        if !self.attachment.is_active() {
            unavailable_audio(&mut capabilities);
        }
        capabilities
    }

    fn capabilities_for(&self, owner: &MediaOwner) -> AudioCapabilitySnapshot {
        let mut capabilities = match self.attachment.scoped_owner(owner) {
            Ok(owner) => self.inner.capabilities_for(&owner),
            Err(_) => self.inner.capabilities(),
        };
        if !self.attachment.is_active() || owner.attachment_id().is_some() {
            unavailable_audio(&mut capabilities);
        }
        capabilities
    }

    async fn execute(
        &self,
        context: AudioOperationContext,
        operation: AudioOperation,
    ) -> Result<AudioOperationSuccess, AudioError> {
        self.execute_scoped(context, operation, false).await
    }

    async fn execute_host_configured_local(
        &self,
        context: AudioOperationContext,
        operation: AudioOperation,
    ) -> Result<AudioOperationSuccess, AudioError> {
        self.execute_scoped(context, operation, true).await
    }

    async fn cancel(&self, mut context: AudioOperationContext) -> Result<(), AudioError> {
        let owner = self
            .attachment
            .scoped_owner(&context.owner)
            .map_err(|error| audio_error_from_media(error.kind, error.message))?;
        match self.attachment.enter_audio_cancel(
            &context.owner,
            &context.identity,
            context.operation_incarnation(),
        )? {
            AudioCancelAdmission::Complete => Ok(()),
            AudioCancelAdmission::Wait => {
                self.attachment
                    .wait_existing_audio_cancel(
                        &context.identity,
                        &owner,
                        context.operation_incarnation(),
                    )
                    .await
            }
            AudioCancelAdmission::Dispatch(permit, mut attempt) => {
                let identity = context.identity.clone();
                let handle = context.operation_incarnation();
                context.owner = owner.clone();
                let result = self.inner.cancel(context).await;
                attempt.finish(result.is_ok());
                drop(permit);
                result?;
                self.attachment
                    .wait_existing_audio_cancel(&identity, &owner, handle)
                    .await
            }
        }
    }
}

struct AttachmentScopedAudioChannel {
    inner: Arc<dyn MediaAudioChannel>,
    lifecycle: Arc<Lifecycle>,
    parent_lifecycle: Option<Weak<Lifecycle>>,
}

impl AttachmentScopedAudioChannel {
    fn enter(&self) -> Result<CallPermit, MediaError> {
        let parent = match &self.parent_lifecycle {
            Some(parent) => Some(parent.upgrade().ok_or_else(|| {
                MediaError::new(
                    MediaErrorKind::Cancelled,
                    "parent media attachment is no longer available",
                )
            })?),
            None => None,
        };
        let _parent_state = parent.as_ref().map(|parent| lock(&parent.state));
        if _parent_state
            .as_ref()
            .is_some_and(|state| state.phase != MediaAttachmentPhase::Active)
        {
            return Err(MediaError::new(
                MediaErrorKind::Cancelled,
                "parent media attachment is revoking or has been revoked",
            ));
        }
        let mut state = lock(&self.lifecycle.state);
        if state.phase != MediaAttachmentPhase::Active {
            return Err(MediaError::new(
                MediaErrorKind::Cancelled,
                "media attachment is revoking or has been revoked",
            ));
        }
        state.active_calls += 1;
        Ok(CallPermit {
            lifecycle: self.lifecycle.clone(),
            operation: None,
            audio_operation: None,
            dispatched: false,
            finished: false,
            native_started: None,
        })
    }
}

#[async_trait]
impl MediaAudioChannel for AttachmentScopedAudioChannel {
    async fn read(&self) -> Result<Option<media_api::MediaFrame>, MediaError> {
        let permit = self.enter()?;
        let result = self.inner.read().await;
        drop(permit);
        result
    }

    async fn write(&self, frame: MediaPcmFrame) -> Result<MediaRenderReceipt, MediaError> {
        let permit = self.enter()?;
        let result = self.inner.write(frame).await;
        drop(permit);
        result
    }

    async fn flush(&self) -> Result<MediaRenderReceipt, MediaError> {
        let permit = self.enter()?;
        let result = self.inner.flush().await;
        drop(permit);
        result
    }

    async fn interrupt(&self) -> Result<MediaRenderReceipt, MediaError> {
        let permit = self.enter()?;
        let result = self.inner.interrupt().await;
        drop(permit);
        result
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use media_api::{MediaAsset, MediaKind, MediaMetadata, MediaPayload, MediaRef};
    use std::sync::atomic::{AtomicBool, AtomicUsize};
    use tokio::sync::Notify;

    struct FailedAudioReceiptDevice {
        confirm_cancel: bool,
        start_succeeds: bool,
        cancelled: Notify,
        cancellations: Mutex<Vec<AudioOperationContext>>,
        cancel_gate: Mutex<Option<Arc<Notify>>>,
    }

    #[async_trait]
    impl AudioService for FailedAudioReceiptDevice {
        fn capabilities(&self) -> AudioCapabilitySnapshot {
            AudioCapabilitySnapshot {
                service_epoch: 23,
                support_revision: 1,
                supported_operations: vec![
                    media_api::audio::AudioOperationKind::Play,
                    media_api::audio::AudioOperationKind::Record,
                ],
                readiness: Vec::new(),
                max_payload_bytes: 1024,
            }
        }

        async fn execute(
            &self,
            _context: AudioOperationContext,
            operation: AudioOperation,
        ) -> Result<AudioOperationSuccess, AudioError> {
            match operation {
                AudioOperation::StartRecording { .. } if self.start_succeeds => {
                    Ok(AudioOperationSuccess::RecordingStarted {
                        handle: media_api::RecordingRef(media_api::RecordingId::from(
                            "receipt-recording",
                        )),
                    })
                }
                AudioOperation::StopRecording { .. } if self.start_succeeds => {
                    Ok(AudioOperationSuccess::Recording {
                        recording: media_api::voice::VoiceRecording {
                            audio_bytes: vec![1, 2],
                            mime_type: "audio/wav".into(),
                        },
                    })
                }
                AudioOperation::Play { .. } | AudioOperation::StartRecording { .. } => Err(
                    AudioError::new(AudioErrorKind::NativeFailure, "device operation failed"),
                ),
                AudioOperation::EndOwner => Ok(AudioOperationSuccess::OwnerEnded),
                AudioOperation::Status { .. } => Ok(AudioOperationSuccess::Status {
                    status: media_api::audio::AudioStatus::default(),
                }),
                _ => Err(AudioError::new(AudioErrorKind::Unsupported, "unused")),
            }
        }

        async fn cancel(&self, context: AudioOperationContext) -> Result<(), AudioError> {
            lock(&self.cancellations).push(context);
            self.cancelled.notify_one();
            let gate = lock(&self.cancel_gate).clone();
            if let Some(gate) = gate {
                gate.notified().await;
            }
            if self.confirm_cancel {
                Ok(())
            } else {
                Err(AudioError::new(
                    AudioErrorKind::NativeFailure,
                    "no close receipt",
                ))
            }
        }
    }

    struct DroppedAudioDevice {
        confirm_cancel: bool,
        started: Notify,
        cancelled: Notify,
    }

    struct LateStartedAudioDevice {
        started: Notify,
        release: Notify,
        cancelled: Notify,
        cancel_calls: AtomicUsize,
    }

    struct BlockingAudioDrop {
        entered: Arc<Notify>,
        released: Arc<(std::sync::Mutex<bool>, std::sync::Condvar)>,
    }

    impl Drop for BlockingAudioDrop {
        fn drop(&mut self) {
            self.entered.notify_one();
            let (lock, changed) = &*self.released;
            let mut released = lock.lock().unwrap();
            while !*released {
                released = changed.wait(released).unwrap();
            }
        }
    }

    struct ReleaseBlockedAudioDrop(Arc<(std::sync::Mutex<bool>, std::sync::Condvar)>);

    impl Drop for ReleaseBlockedAudioDrop {
        fn drop(&mut self) {
            let (lock, changed) = &*self.0;
            *lock.lock().unwrap() = true;
            changed.notify_all();
        }
    }

    struct BlockedDropAudioDevice {
        started: Notify,
        cancelled: Notify,
        drop_entered: Arc<Notify>,
        released: Arc<(std::sync::Mutex<bool>, std::sync::Condvar)>,
    }

    #[async_trait]
    impl AudioService for BlockedDropAudioDevice {
        fn capabilities(&self) -> AudioCapabilitySnapshot {
            AudioCapabilitySnapshot {
                service_epoch: 23,
                support_revision: 1,
                supported_operations: vec![media_api::audio::AudioOperationKind::Listen],
                readiness: Vec::new(),
                max_payload_bytes: 1024,
            }
        }

        async fn execute(
            &self,
            _: AudioOperationContext,
            _: AudioOperation,
        ) -> Result<AudioOperationSuccess, AudioError> {
            let _guard = BlockingAudioDrop {
                entered: self.drop_entered.clone(),
                released: self.released.clone(),
            };
            self.started.notify_one();
            std::future::pending().await
        }

        async fn cancel(&self, _: AudioOperationContext) -> Result<(), AudioError> {
            self.cancelled.notify_one();
            Ok(())
        }
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 3)]
    async fn unified_cancel_waits_for_the_cancelled_execute_future_to_drop() {
        let native = Arc::new(BlockedDropAudioDevice {
            started: Notify::new(),
            cancelled: Notify::new(),
            drop_entered: Arc::new(Notify::new()),
            released: Arc::new((std::sync::Mutex::new(false), std::sync::Condvar::new())),
        });
        let _release_on_drop = ReleaseBlockedAudioDrop(native.released.clone());
        let service = Arc::new(crate::audio::UnifiedAudioService::new(
            native.clone(),
            Vec::new(),
            None,
            crate::audio::AudioRoutingSettings::default(),
        ));
        let context = AudioOperationContext::new(AudioOperationId::new(148, 23), owner());
        let starting_service = service.clone();
        let starting_context = context.clone();
        let start = tokio::spawn(async move {
            starting_service
                .execute(starting_context, AudioOperation::Listen { language: None })
                .await
        });
        wait_for_audio_notification(&native.started, "unified Listen start").await;
        let cancelling_service = service.clone();
        let mut cancel = tokio::spawn(async move { cancelling_service.cancel(context).await });
        wait_for_audio_notification(&native.cancelled, "unified native cancel receipt").await;
        wait_for_audio_notification(&native.drop_entered, "unified execution drop").await;
        let pending = tokio::time::timeout(Duration::from_millis(20), &mut cancel)
            .await
            .is_err();
        let (lock, changed) = &*native.released;
        *lock.lock().unwrap() = true;
        changed.notify_all();
        assert!(
            pending,
            "cancel cannot confirm while the execute future is dropping"
        );
        cancel.await.unwrap().unwrap();
        assert_eq!(
            start.await.unwrap().unwrap_err().kind,
            AudioErrorKind::Cancelled
        );
    }

    #[async_trait]
    impl AudioService for LateStartedAudioDevice {
        fn capabilities(&self) -> AudioCapabilitySnapshot {
            AudioCapabilitySnapshot {
                service_epoch: 23,
                support_revision: 1,
                supported_operations: vec![media_api::audio::AudioOperationKind::Record],
                readiness: Vec::new(),
                max_payload_bytes: 1024,
            }
        }

        async fn execute(
            &self,
            _: AudioOperationContext,
            operation: AudioOperation,
        ) -> Result<AudioOperationSuccess, AudioError> {
            match operation {
                AudioOperation::StartRecording { .. } => {
                    self.started.notify_one();
                    self.release.notified().await;
                    Ok(AudioOperationSuccess::RecordingStarted {
                        handle: media_api::RecordingRef(media_api::RecordingId::from("late-start")),
                    })
                }
                AudioOperation::EndOwner => Ok(AudioOperationSuccess::OwnerEnded),
                AudioOperation::Status { .. } => Ok(AudioOperationSuccess::Status {
                    status: media_api::audio::AudioStatus::default(),
                }),
                _ => Err(AudioError::new(AudioErrorKind::Unsupported, "unused")),
            }
        }

        async fn cancel(&self, _: AudioOperationContext) -> Result<(), AudioError> {
            self.cancel_calls.fetch_add(1, Ordering::SeqCst);
            self.cancelled.notify_one();
            Ok(())
        }
    }

    #[tokio::test]
    async fn cancel_cannot_confirm_before_the_original_execute_finishes() {
        let native = Arc::new(LateStartedAudioDevice {
            started: Notify::new(),
            release: Notify::new(),
            cancelled: Notify::new(),
            cancel_calls: AtomicUsize::new(0),
        });
        let host = MediaAttachmentHost::new();
        let attachment = host.attach(Arc::new(FakeService::default())).unwrap();
        let audio = attachment.audio_service(native.clone()).unwrap();
        let context = AudioOperationContext::new(AudioOperationId::new(149, 23), owner());
        let starting_audio = audio.clone();
        let starting_context = context.clone();
        let start = tokio::spawn(async move {
            starting_audio
                .execute(
                    starting_context,
                    AudioOperation::StartRecording {
                        sample_rate_hz: 24_000,
                        format: "wav".into(),
                    },
                )
                .await
        });
        wait_for_audio_notification(&native.started, "late recording start").await;
        let cancelling_audio = audio.clone();
        let cancelling_context = context.clone();
        let mut cancel =
            tokio::spawn(async move { cancelling_audio.cancel(cancelling_context).await });
        wait_for_audio_notification(&native.cancelled, "early native receipt").await;
        let joining_audio = audio.clone();
        let joining_context = context.clone();
        let mut joined = tokio::spawn(async move { joining_audio.cancel(joining_context).await });
        assert!(tokio::time::timeout(Duration::from_millis(20), &mut cancel)
            .await
            .is_err());
        assert!(tokio::time::timeout(Duration::from_millis(20), &mut joined)
            .await
            .is_err());
        native.release.notify_one();
        assert!(matches!(
            start.await.unwrap().unwrap(),
            AudioOperationSuccess::RecordingStarted { .. }
        ));
        assert!(
            cancel.await.unwrap().is_err(),
            "late recording invalidates the earlier receipt"
        );
        assert!(
            joined.await.unwrap().is_err(),
            "concurrent observer must not confirm"
        );
        audio.cancel(context).await.unwrap();
        assert_eq!(native.cancel_calls.load(Ordering::SeqCst), 2);
        assert_eq!(
            attachment.revoke(Duration::from_secs(1)).await,
            MediaAttachmentRevocation::Confirmed
        );
    }

    #[async_trait]
    impl AudioService for DroppedAudioDevice {
        fn capabilities(&self) -> AudioCapabilitySnapshot {
            AudioCapabilitySnapshot {
                service_epoch: 23,
                support_revision: 1,
                supported_operations: vec![media_api::audio::AudioOperationKind::Listen],
                readiness: Vec::new(),
                max_payload_bytes: 1024,
            }
        }

        async fn execute(
            &self,
            _: AudioOperationContext,
            operation: AudioOperation,
        ) -> Result<AudioOperationSuccess, AudioError> {
            match operation {
                AudioOperation::Listen { .. } => {
                    self.started.notify_one();
                    std::future::pending().await
                }
                AudioOperation::EndOwner => Ok(AudioOperationSuccess::OwnerEnded),
                AudioOperation::Status { .. } => Ok(AudioOperationSuccess::Status {
                    status: media_api::audio::AudioStatus::default(),
                }),
                _ => Err(AudioError::new(AudioErrorKind::Unsupported, "unused")),
            }
        }

        async fn cancel(&self, _: AudioOperationContext) -> Result<(), AudioError> {
            self.cancelled.notify_one();
            if self.confirm_cancel {
                Ok(())
            } else {
                Err(AudioError::new(
                    AudioErrorKind::NativeFailure,
                    "no close receipt",
                ))
            }
        }
    }

    #[tokio::test]
    async fn dropped_unified_audio_future_requires_its_exact_close_receipt() {
        for confirmed in [false, true] {
            let native = Arc::new(DroppedAudioDevice {
                confirm_cancel: confirmed,
                started: Notify::new(),
                cancelled: Notify::new(),
            });
            let unified = Arc::new(crate::audio::UnifiedAudioService::new(
                native.clone(),
                Vec::new(),
                None,
                crate::audio::AudioRoutingSettings::default(),
            ));
            let host = MediaAttachmentHost::new();
            let attachment = host.attach(Arc::new(FakeService::default())).unwrap();
            let audio = attachment.audio_service(unified).unwrap();
            let task = tokio::spawn(async move {
                audio
                    .execute(
                        AudioOperationContext::new(AudioOperationId::new(150, 23), owner()),
                        AudioOperation::Listen { language: None },
                    )
                    .await
            });
            wait_for_audio_notification(&native.started, "dropped origin start").await;
            task.abort();
            assert!(task.await.unwrap_err().is_cancelled());
            wait_for_audio_notification(&native.cancelled, "dropped origin cancel").await;
            let outcome = attachment.revoke(Duration::from_secs(1)).await;
            if confirmed {
                assert_eq!(outcome, MediaAttachmentRevocation::Confirmed);
            } else {
                assert!(matches!(outcome, MediaAttachmentRevocation::Unknown { .. }));
            }
        }
    }

    #[tokio::test]
    async fn failed_unified_audio_without_exact_close_must_not_confirm_drain() {
        for operation in [
            AudioOperation::Play {
                audio: media_api::tts::TtsAudio {
                    pcm: vec![1, 2],
                    sample_rate_hz: 24_000,
                },
            },
            AudioOperation::StartRecording {
                sample_rate_hz: 24_000,
                format: "wav".into(),
            },
        ] {
            let native = Arc::new(FailedAudioReceiptDevice {
                confirm_cancel: false,
                start_succeeds: false,
                cancelled: Notify::new(),
                cancellations: Mutex::new(Vec::new()),
                cancel_gate: Mutex::new(None),
            });
            let unified = Arc::new(crate::audio::UnifiedAudioService::new(
                native.clone(),
                Vec::new(),
                None,
                crate::audio::AudioRoutingSettings::default(),
            ));
            let host = MediaAttachmentHost::new();
            let attachment = host.attach(Arc::new(FakeService::default())).unwrap();
            let audio = attachment.audio_service(unified).unwrap();
            let context = AudioOperationContext::new(AudioOperationId::new(151, 23), owner());
            let result = audio.execute(context, operation).await;
            assert!(result.is_err());
            wait_for_audio_notification(&native.cancelled, "failed origin cancel").await;
            assert!(matches!(
                attachment.revoke(Duration::from_secs(1)).await,
                MediaAttachmentRevocation::Unknown { .. }
            ));
        }
    }

    #[tokio::test]
    async fn failed_without_resource_receipts_release_the_exact_origin() {
        // AudioBridge maps both result variants to the same domain error; an
        // exact cancel returns Ok only for its typed no-resource receipt.
        for operation in [
            AudioOperation::Play {
                audio: media_api::tts::TtsAudio {
                    pcm: vec![1, 2],
                    sample_rate_hz: 24_000,
                },
            },
            AudioOperation::StartRecording {
                sample_rate_hz: 24_000,
                format: "wav".into(),
            },
        ] {
            let native = Arc::new(FailedAudioReceiptDevice {
                confirm_cancel: true,
                start_succeeds: false,
                cancelled: Notify::new(),
                cancellations: Mutex::new(Vec::new()),
                cancel_gate: Mutex::new(None),
            });
            let unified = Arc::new(crate::audio::UnifiedAudioService::new(
                native.clone(),
                Vec::new(),
                None,
                crate::audio::AudioRoutingSettings::default(),
            ));
            let host = MediaAttachmentHost::new();
            let attachment = host.attach(Arc::new(FakeService::default())).unwrap();
            let audio = attachment.audio_service(unified).unwrap();
            let context = AudioOperationContext::new(AudioOperationId::new(152, 23), owner());
            assert!(audio.execute(context, operation).await.is_err());
            wait_for_audio_notification(&native.cancelled, "confirmed failed origin").await;
            assert_eq!(
                attachment.revoke(Duration::from_secs(1)).await,
                MediaAttachmentRevocation::Confirmed
            );
        }
    }

    #[tokio::test]
    async fn successful_recording_origin_requires_its_own_close_receipt() {
        for confirmed in [false, true] {
            let native = Arc::new(FailedAudioReceiptDevice {
                confirm_cancel: confirmed,
                start_succeeds: true,
                cancelled: Notify::new(),
                cancellations: Mutex::new(Vec::new()),
                cancel_gate: Mutex::new(None),
            });
            let unified = Arc::new(crate::audio::UnifiedAudioService::new(
                native.clone(),
                Vec::new(),
                None,
                crate::audio::AudioRoutingSettings::default(),
            ));
            let host = MediaAttachmentHost::new();
            let attachment = host.attach(Arc::new(FakeService::default())).unwrap();
            let audio = attachment.audio_service(unified).unwrap();
            let context = AudioOperationContext::new(AudioOperationId::new(153, 23), owner());
            assert!(matches!(
                audio
                    .execute(
                        context.clone(),
                        AudioOperation::StartRecording {
                            sample_rate_hz: 24_000,
                            format: "wav".into(),
                        },
                    )
                    .await
                    .unwrap(),
                AudioOperationSuccess::RecordingStarted { .. }
            ));
            assert!(
                lock(&native.cancellations).is_empty(),
                "start must remain live"
            );
            let result = attachment.revoke(Duration::from_secs(1)).await;
            if confirmed {
                assert_eq!(result, MediaAttachmentRevocation::Confirmed);
            } else {
                assert!(matches!(result, MediaAttachmentRevocation::Unknown { .. }));
            }
            let cancellations = lock(&native.cancellations);
            assert!(!cancellations.is_empty());
            if confirmed {
                assert_eq!(cancellations.len(), 1);
            }
            assert!(cancellations.iter().all(|cancel| {
                cancel.identity == context.identity
                    && cancel.owner == owner().in_attachment(attachment.id().clone())
                    && cancel.operation_incarnation() == context.operation_incarnation()
            }));
        }
    }

    #[tokio::test]
    async fn confirmed_recording_stops_release_origins_without_exhausting_slots() {
        let native = Arc::new(FailedAudioReceiptDevice {
            confirm_cancel: true,
            start_succeeds: true,
            cancelled: Notify::new(),
            cancellations: Mutex::new(Vec::new()),
            cancel_gate: Mutex::new(None),
        });
        let unified = Arc::new(crate::audio::UnifiedAudioService::new(
            native.clone(),
            Vec::new(),
            None,
            crate::audio::AudioRoutingSettings::default(),
        ));
        let host = MediaAttachmentHost::new();
        let attachment = host.attach(Arc::new(FakeService::default())).unwrap();
        let audio = attachment.audio_service(unified).unwrap();
        for generation in 1000..1513 {
            let start = AudioOperationContext::new(AudioOperationId::new(generation, 23), owner());
            let AudioOperationSuccess::RecordingStarted { handle } = audio
                .execute(
                    start,
                    AudioOperation::StartRecording {
                        sample_rate_hz: 24_000,
                        format: "wav".into(),
                    },
                )
                .await
                .unwrap()
            else {
                panic!("start must return its recording handle")
            };
            let stop =
                AudioOperationContext::new(AudioOperationId::new(generation + 10_000, 23), owner());
            assert!(matches!(
                audio
                    .execute(stop, AudioOperation::StopRecording { handle })
                    .await
                    .unwrap(),
                AudioOperationSuccess::Recording { .. }
            ));
            assert!(lock(&attachment.service.lifecycle.state)
                .audio_operations
                .is_empty());
        }
        assert_eq!(lock(&native.cancellations).len(), 513);
        assert_eq!(
            attachment.revoke(Duration::from_secs(1)).await,
            MediaAttachmentRevocation::Confirmed
        );
    }

    #[tokio::test]
    async fn confirmed_failed_origins_do_not_accumulate() {
        let native = Arc::new(FailedAudioReceiptDevice {
            confirm_cancel: true,
            start_succeeds: false,
            cancelled: Notify::new(),
            cancellations: Mutex::new(Vec::new()),
            cancel_gate: Mutex::new(None),
        });
        let unified = Arc::new(crate::audio::UnifiedAudioService::new(
            native.clone(),
            Vec::new(),
            None,
            crate::audio::AudioRoutingSettings::default(),
        ));
        let host = MediaAttachmentHost::new();
        let attachment = host.attach(Arc::new(FakeService::default())).unwrap();
        let audio = attachment.audio_service(unified).unwrap();
        for generation in 2000..2513 {
            let result = audio
                .execute(
                    AudioOperationContext::new(AudioOperationId::new(generation, 23), owner()),
                    AudioOperation::StartRecording {
                        sample_rate_hz: 24_000,
                        format: "wav".into(),
                    },
                )
                .await;
            assert_eq!(result.unwrap_err().kind, AudioErrorKind::NativeFailure);
            tokio::time::timeout(Duration::from_secs(1), async {
                while !lock(&attachment.service.lifecycle.state)
                    .audio_operations
                    .is_empty()
                {
                    tokio::task::yield_now().await;
                }
            })
            .await
            .expect("confirmed failed origin is released");
        }
        assert_eq!(lock(&native.cancellations).len(), 513);
        assert_eq!(
            attachment.revoke(Duration::from_secs(1)).await,
            MediaAttachmentRevocation::Confirmed
        );
    }

    #[tokio::test]
    async fn failed_origin_drain_joins_one_inflight_exact_cancel() {
        let gate = Arc::new(Notify::new());
        let native = Arc::new(FailedAudioReceiptDevice {
            confirm_cancel: true,
            start_succeeds: false,
            cancelled: Notify::new(),
            cancellations: Mutex::new(Vec::new()),
            cancel_gate: Mutex::new(Some(gate.clone())),
        });
        let unified = Arc::new(crate::audio::UnifiedAudioService::new(
            native.clone(),
            Vec::new(),
            None,
            crate::audio::AudioRoutingSettings::default(),
        ));
        let host = MediaAttachmentHost::new();
        let attachment = host.attach(Arc::new(FakeService::default())).unwrap();
        let audio = attachment.audio_service(unified).unwrap();
        let context = AudioOperationContext::new(AudioOperationId::new(154, 23), owner());
        assert!(audio
            .execute(
                context.clone(),
                AudioOperation::Play {
                    audio: media_api::tts::TtsAudio {
                        pcm: vec![1, 2],
                        sample_rate_hz: 24_000
                    },
                },
            )
            .await
            .is_err());
        wait_for_audio_notification(&native.cancelled, "held exact cancel").await;
        let joining_audio = audio.clone();
        let mut joining = tokio::spawn(async move { joining_audio.cancel(context).await });
        assert!(
            tokio::time::timeout(Duration::from_millis(20), &mut joining)
                .await
                .is_err()
        );
        let draining = attachment.clone();
        let mut task = tokio::spawn(async move { draining.revoke(Duration::from_secs(2)).await });
        assert!(tokio::time::timeout(Duration::from_millis(20), &mut task)
            .await
            .is_err());
        assert_eq!(lock(&native.cancellations).len(), 1);
        gate.notify_one();
        joining.await.unwrap().unwrap();
        assert_eq!(task.await.unwrap(), MediaAttachmentRevocation::Confirmed);
        assert_eq!(lock(&native.cancellations).len(), 1);
    }

    #[tokio::test]
    async fn recording_close_with_same_handle_stays_with_its_attachment_owner() {
        let native = Arc::new(FailedAudioReceiptDevice {
            confirm_cancel: true,
            start_succeeds: true,
            cancelled: Notify::new(),
            cancellations: Mutex::new(Vec::new()),
            cancel_gate: Mutex::new(None),
        });
        let unified = Arc::new(crate::audio::UnifiedAudioService::new(
            native.clone(),
            Vec::new(),
            None,
            crate::audio::AudioRoutingSettings::default(),
        ));
        let host = MediaAttachmentHost::new();
        let a = host.attach(Arc::new(FakeService::default())).unwrap();
        let b = host.attach(Arc::new(FakeService::default())).unwrap();
        let audio_a = a.audio_service(unified.clone()).unwrap();
        let audio_b = b.audio_service(unified).unwrap();
        let request = || AudioOperation::StartRecording {
            sample_rate_hz: 24_000,
            format: "wav".into(),
        };
        let start_a = AudioOperationContext::new(AudioOperationId::new(201, 23), owner());
        let start_b = AudioOperationContext::new(AudioOperationId::new(202, 23), owner());
        let AudioOperationSuccess::RecordingStarted { handle: handle_a } =
            audio_a.execute(start_a.clone(), request()).await.unwrap()
        else {
            panic!("start a")
        };
        let AudioOperationSuccess::RecordingStarted { handle: handle_b } =
            audio_b.execute(start_b.clone(), request()).await.unwrap()
        else {
            panic!("start b")
        };
        assert_eq!(handle_a, handle_b);
        audio_a
            .execute(
                AudioOperationContext::new(AudioOperationId::new(203, 23), owner()),
                AudioOperation::StopRecording { handle: handle_a },
            )
            .await
            .unwrap();
        let cancelled = lock(&native.cancellations);
        assert_eq!(cancelled.len(), 1);
        assert_eq!(cancelled[0].owner, owner().in_attachment(a.id().clone()));
        assert_eq!(cancelled[0].identity, start_a.identity);
        assert_eq!(
            cancelled[0].operation_incarnation(),
            start_a.operation_incarnation()
        );
        drop(cancelled);
        assert_eq!(
            a.revoke(Duration::from_secs(1)).await,
            MediaAttachmentRevocation::Confirmed
        );
        assert_eq!(lock(&native.cancellations).len(), 1);
        audio_b
            .execute(
                AudioOperationContext::new(AudioOperationId::new(204, 23), owner()),
                AudioOperation::StopRecording { handle: handle_b },
            )
            .await
            .unwrap();
        assert_eq!(
            b.revoke(Duration::from_secs(1)).await,
            MediaAttachmentRevocation::Confirmed
        );
        let cancelled = lock(&native.cancellations);
        assert_eq!(cancelled.len(), 2);
        assert_eq!(cancelled[1].owner, owner().in_attachment(b.id().clone()));
        assert_eq!(cancelled[1].identity, start_b.identity);
        assert_eq!(
            cancelled[1].operation_incarnation(),
            start_b.operation_incarnation()
        );
    }

    async fn wait_for_audio_notification(notification: &Notify, label: &str) {
        tokio::time::timeout(Duration::from_secs(2), notification.notified())
            .await
            .unwrap_or_else(|_| panic!("timed out waiting for {label}"));
    }

    #[derive(Default)]
    struct FakeService {
        owners: Mutex<Vec<MediaOwner>>,
        assets: Mutex<HashMap<MediaRef, (MediaOwner, MediaPayload)>>,
        active: Mutex<HashMap<MediaOwner, MediaResourceRef>>,
        active_operations: Mutex<HashMap<MediaOperationId, MediaOwner>>,
        cancel_contexts: Mutex<Vec<MediaOperationContext>>,
        cancelled_operations: Mutex<HashSet<MediaOperationId>>,
        capture_started: Notify,
        capture_gate: Notify,
        session_audio_started: Notify,
        session_audio_gate: Notify,
        cancel_started: Notify,
        cancel_gate: Notify,
        stop_started: Notify,
        stop_gate: Notify,
        asset_status_started: Notify,
        asset_status_gate: Notify,
        block_capture: AtomicBool,
        block_session_audio: AtomicBool,
        block_cancel: AtomicBool,
        block_stop: AtomicBool,
        block_asset_status: AtomicBool,
        fail_stop: AtomicBool,
        fail_status: AtomicBool,
        stop_calls: AtomicUsize,
        include_status_operation: AtomicBool,
    }

    #[tokio::test]
    async fn asset_status_revoke_waits_for_the_admitted_query() {
        let shared = Arc::new(FakeService::default());
        shared.block_asset_status.store(true, Ordering::SeqCst);
        let host = MediaAttachmentHost::new();
        let attachment = host.attach(shared.clone()).unwrap();
        let scoped = attachment.service();
        let query_context = context(owner(), 155);
        let query = tokio::spawn(async move {
            scoped
                .execute(
                    query_context,
                    MediaOperation::Status {
                        target: Some(MediaResourceRef::Media(MediaRef::from("read-only-asset"))),
                    },
                )
                .await
        });
        wait_for_audio_notification(&shared.asset_status_started, "held asset status").await;
        let draining = attachment.clone();
        let mut revoke = tokio::spawn(async move { draining.revoke(Duration::from_secs(2)).await });
        assert!(tokio::time::timeout(Duration::from_millis(20), &mut revoke)
            .await
            .is_err());
        shared.asset_status_gate.notify_one();
        assert!(matches!(
            query.await.unwrap(),
            Ok(MediaResult::Status { .. })
        ));
        assert_eq!(revoke.await.unwrap(), MediaAttachmentRevocation::Confirmed);
    }

    #[tokio::test]
    async fn dropped_asset_status_keeps_the_unknown_operation_fenced() {
        let shared = Arc::new(FakeService::default());
        shared.block_asset_status.store(true, Ordering::SeqCst);
        let host = MediaAttachmentHost::new();
        let attachment = host.attach(shared.clone()).unwrap();
        let scoped = attachment.service();
        let query_context = context(owner(), 156);
        let task_context = query_context.clone();
        let query = tokio::spawn(async move {
            scoped
                .execute(
                    task_context,
                    MediaOperation::Status {
                        target: Some(MediaResourceRef::Media(MediaRef::from("read-only-asset"))),
                    },
                )
                .await
        });
        wait_for_audio_notification(&shared.asset_status_started, "dropped asset status").await;
        query.abort();
        assert!(query.await.unwrap_err().is_cancelled());
        assert_eq!(
            attachment.operation_cleanup(&query_context),
            MediaOperationCleanup::Unknown
        );
        assert!(matches!(
            attachment.revoke(Duration::from_secs(1)).await,
            MediaAttachmentRevocation::Unknown { .. }
        ));
    }

    fn capabilities() -> media_api::MediaCapabilities {
        media_api::MediaCapabilities {
            service_epoch: 11,
            support_revision: 1,
            supported_operations: vec![
                media_api::MediaOperationKind::Sources,
                media_api::MediaOperationKind::Status,
                media_api::MediaOperationKind::Capture,
                media_api::MediaOperationKind::Stop,
            ],
            supported_media_kinds: vec![MediaKind::Image],
            readiness: Vec::new(),
            max_payload_bytes: 1024,
        }
    }

    fn metadata() -> MediaMetadata {
        MediaMetadata {
            kind: MediaKind::Image,
            mime_type: "image/png".into(),
            byte_len: 3,
            captured_at_unix_ms: None,
            duration_ms: None,
            width_px: Some(1),
            height_px: Some(1),
            sample_rate_hz: None,
            channels: None,
        }
    }

    #[async_trait]
    impl MediaService for FakeService {
        fn capabilities(&self) -> media_api::MediaCapabilities {
            capabilities()
        }

        fn capabilities_for(&self, _owner: &MediaOwner) -> media_api::MediaCapabilities {
            capabilities()
        }

        async fn execute(
            &self,
            context: MediaOperationContext,
            operation: MediaOperation,
        ) -> Result<MediaResult, MediaError> {
            lock(&self.owners).push(context.owner.clone());
            match operation {
                MediaOperation::Status { target } => {
                    if matches!(target, Some(MediaResourceRef::Media(_)))
                        && self.block_asset_status.load(Ordering::SeqCst)
                    {
                        self.asset_status_started.notify_one();
                        self.asset_status_gate.notified().await;
                    }
                    if self.fail_status.load(Ordering::SeqCst) {
                        return Err(MediaError::new(
                            MediaErrorKind::Unavailable,
                            "fake media status receipt unavailable",
                        ));
                    }
                    let mut active: Vec<media_api::MediaResourceStatus> = lock(&self.active)
                        .get(&context.owner)
                        .cloned()
                        .map(|resource| media_api::MediaResourceStatus {
                            resource,
                            state: MediaResourceState::Active,
                            started_at_unix_ms: None,
                            updated_at_unix_ms: None,
                        })
                        .into_iter()
                        .collect();
                    if self.include_status_operation.load(Ordering::SeqCst) {
                        active.push(media_api::MediaResourceStatus {
                            resource: MediaResourceRef::Operation {
                                identity: context.identity.clone(),
                                incarnation: context.operation_incarnation(),
                            },
                            state: MediaResourceState::Active,
                            started_at_unix_ms: None,
                            updated_at_unix_ms: None,
                        });
                    }
                    Ok(MediaResult::Status { resources: active })
                }
                MediaOperation::Capture { .. } => {
                    let reference = MediaRef::from("attachment-test-media");
                    let payload = MediaPayload {
                        metadata: metadata(),
                        bytes: vec![1, 2, 3],
                    };
                    lock(&self.assets)
                        .insert(reference.clone(), (context.owner.clone(), payload.clone()));
                    lock(&self.active_operations)
                        .insert(context.identity.clone(), context.owner.clone());
                    if self.block_capture.load(Ordering::SeqCst) {
                        lock(&self.active).insert(
                            context.owner.clone(),
                            MediaResourceRef::Operation {
                                identity: context.identity.clone(),
                                incarnation: context.operation_incarnation(),
                            },
                        );
                        self.capture_started.notify_one();
                        self.capture_gate.notified().await;
                    }
                    lock(&self.active_operations).remove(&context.identity);
                    Ok(MediaResult::Captured {
                        asset: MediaAsset {
                            reference,
                            metadata: payload.metadata,
                        },
                    })
                }
                MediaOperation::Stop { target } => {
                    self.stop_calls.fetch_add(1, Ordering::SeqCst);
                    if self.block_stop.load(Ordering::SeqCst) {
                        self.stop_started.notify_one();
                        self.stop_gate.notified().await;
                    }
                    if self.fail_stop.load(Ordering::SeqCst) {
                        self.capture_gate.notify_one();
                        return Err(MediaError::new(
                            MediaErrorKind::NativeFailure,
                            "fake close is unconfirmed",
                        ));
                    }
                    lock(&self.active).remove(&context.owner);
                    if let MediaResourceRef::Operation { identity, .. } = &target {
                        lock(&self.active_operations).remove(identity);
                    }
                    self.capture_gate.notify_one();
                    Ok(MediaResult::Stopped { resource: target })
                }
                _ => Err(MediaError::new(
                    MediaErrorKind::Unsupported,
                    "fake service operation unsupported",
                )),
            }
        }

        async fn read_media(
            &self,
            context: MediaOperationContext,
            media: MediaRef,
        ) -> Result<media_api::MediaContent, MediaError> {
            lock(&self.owners).push(context.owner.clone());
            let assets = lock(&self.assets);
            let Some((owner, payload)) = assets.get(&media) else {
                return Err(MediaError::new(MediaErrorKind::NotFound, "media missing"));
            };
            if owner != &context.owner {
                return Err(MediaError::new(MediaErrorKind::NotFound, "media missing"));
            }
            Ok(media_api::MediaContent {
                reference: media,
                metadata: payload.metadata.clone(),
                bytes: payload.bytes.clone(),
            })
        }

        async fn session_audio(
            &self,
            context: MediaOperationContext,
            _session: media_api::MediaSessionRef,
        ) -> Result<Arc<dyn MediaAudioChannel>, MediaError> {
            if self.block_session_audio.load(Ordering::SeqCst) {
                self.session_audio_started.notify_one();
                self.session_audio_gate.notified().await;
            }
            lock(&self.owners).push(context.owner);
            Err(MediaError::new(
                MediaErrorKind::Unsupported,
                "fake session audio is not configured",
            ))
        }

        async fn cancel(&self, context: MediaOperationContext) -> Result<(), MediaError> {
            lock(&self.cancel_contexts).push(context.clone());
            if self.block_cancel.load(Ordering::SeqCst) {
                self.cancel_started.notify_one();
                self.cancel_gate.notified().await;
            }
            let owns_operation = lock(&self.active_operations)
                .get(&context.identity)
                .is_some_and(|owner| owner == &context.owner);
            if owns_operation {
                lock(&self.cancelled_operations).insert(context.identity);
            }
            Ok(())
        }
    }

    /// Models a native Stop whose service future is dropped at the attachment
    /// drain deadline while the native close itself completes later without a
    /// deliverable receipt.
    struct LateNativeStopService {
        inner: Arc<FakeService>,
        stop_gate: Arc<Notify>,
        stop_started: Arc<Notify>,
        stop_completed: Arc<Notify>,
        stop_dispatches: AtomicUsize,
    }

    #[async_trait]
    impl MediaService for LateNativeStopService {
        fn capabilities(&self) -> media_api::MediaCapabilities {
            self.inner.capabilities()
        }

        fn capabilities_for(&self, owner: &MediaOwner) -> media_api::MediaCapabilities {
            self.inner.capabilities_for(owner)
        }

        async fn execute(
            &self,
            context: MediaOperationContext,
            operation: MediaOperation,
        ) -> Result<MediaResult, MediaError> {
            let target = match operation {
                MediaOperation::Stop { target } => target,
                operation => return self.inner.execute(context, operation).await,
            };
            self.stop_dispatches.fetch_add(1, Ordering::SeqCst);
            self.stop_started.notify_one();

            let native = self.inner.clone();
            let gate = self.stop_gate.clone();
            let completed = self.stop_completed.clone();
            let owner = context.owner;
            tokio::spawn(async move {
                gate.notified().await;
                lock(&native.active).remove(&owner);
                if let MediaResourceRef::Operation { identity, .. } = &target {
                    lock(&native.active_operations).remove(identity);
                }
                native.capture_gate.notify_one();
                completed.notify_one();
            });

            // The detached native close can complete, but this future never
            // receives that result after the drain timeout drops it.
            std::future::pending::<Result<MediaResult, MediaError>>().await
        }

        async fn read_media(
            &self,
            context: MediaOperationContext,
            media: MediaRef,
        ) -> Result<media_api::MediaContent, MediaError> {
            self.inner.read_media(context, media).await
        }

        async fn cancel(&self, context: MediaOperationContext) -> Result<(), MediaError> {
            self.inner.cancel(context).await
        }
    }

    fn context(owner: MediaOwner, generation: u64) -> MediaOperationContext {
        MediaOperationContext::new(MediaOperationId::new(generation, 11), owner)
    }

    fn capture() -> MediaOperation {
        MediaOperation::Capture {
            source: media_api::MediaSourceRef(media_api::MediaSourceId::from("camera")),
            options: Default::default(),
        }
    }

    fn owner() -> MediaOwner {
        MediaOwner::Session {
            session_id: "same-conversation".into(),
        }
    }

    #[derive(Default)]
    struct CountingAudioChannel {
        calls: AtomicUsize,
    }

    struct FakeAudioOperation {
        owner: MediaOwner,
        handle: media_api::MediaOperationIncarnation,
        cancel: tokio_util::sync::CancellationToken,
    }

    #[derive(Default)]
    struct FakeDirectAudioService {
        active: Mutex<HashMap<AudioOperationId, FakeAudioOperation>>,
        recordings: Mutex<HashSet<MediaOwner>>,
        owners: Mutex<Vec<MediaOwner>>,
        cancel_contexts: Mutex<Vec<AudioOperationContext>>,
        ended_owners: Mutex<Vec<MediaOwner>>,
        listen_started: Notify,
        cancel_started: Notify,
        cancel_gate: Mutex<Option<Arc<Notify>>>,
        fail_cancel: AtomicBool,
        fail_end_owner: AtomicBool,
        fail_status: AtomicBool,
        end_owner_started: Notify,
        end_owner_gate: Mutex<Option<Arc<Notify>>>,
        end_owner_calls: AtomicUsize,
    }

    impl FakeDirectAudioService {
        fn finish_active(&self, identity: &AudioOperationId) {
            if let Some(operation) = lock(&self.active).get(identity) {
                operation.cancel.cancel();
            }
        }
    }

    #[async_trait]
    impl AudioService for FakeDirectAudioService {
        fn capabilities(&self) -> AudioCapabilitySnapshot {
            AudioCapabilitySnapshot {
                service_epoch: 23,
                support_revision: 1,
                supported_operations: vec![
                    media_api::audio::AudioOperationKind::Listen,
                    media_api::audio::AudioOperationKind::Record,
                ],
                readiness: Vec::new(),
                max_payload_bytes: 1024,
            }
        }

        async fn execute(
            &self,
            context: AudioOperationContext,
            operation: AudioOperation,
        ) -> Result<AudioOperationSuccess, AudioError> {
            lock(&self.owners).push(context.owner.clone());
            match operation {
                AudioOperation::Listen { .. } => {
                    let cancel = tokio_util::sync::CancellationToken::new();
                    lock(&self.active).insert(
                        context.identity.clone(),
                        FakeAudioOperation {
                            owner: context.owner.clone(),
                            handle: context.operation_incarnation(),
                            cancel: cancel.clone(),
                        },
                    );
                    self.listen_started.notify_one();
                    cancel.cancelled().await;
                    lock(&self.active).remove(&context.identity);
                    Err(AudioError::new(
                        AudioErrorKind::Cancelled,
                        "fake direct audio operation cancelled",
                    ))
                }
                AudioOperation::StartRecording { .. } => {
                    lock(&self.recordings).insert(context.owner);
                    Ok(AudioOperationSuccess::RecordingStarted {
                        handle: media_api::RecordingRef(media_api::RecordingId::from(
                            "attachment-audio-recording",
                        )),
                    })
                }
                AudioOperation::EndOwner => {
                    self.end_owner_calls.fetch_add(1, Ordering::SeqCst);
                    self.end_owner_started.notify_one();
                    let gate = lock(&self.end_owner_gate).clone();
                    if let Some(gate) = gate {
                        gate.notified().await;
                    }
                    if self.fail_end_owner.load(Ordering::SeqCst) {
                        return Err(AudioError::new(
                            AudioErrorKind::NativeFailure,
                            "fake EndOwner has no receipt",
                        ));
                    }
                    let active = lock(&self.active)
                        .values()
                        .filter(|operation| operation.owner == context.owner)
                        .map(|operation| operation.cancel.clone())
                        .collect::<Vec<_>>();
                    for cancel in active {
                        cancel.cancel();
                    }
                    lock(&self.recordings).remove(&context.owner);
                    lock(&self.ended_owners).push(context.owner);
                    Ok(AudioOperationSuccess::OwnerEnded)
                }
                AudioOperation::Status { .. } => {
                    if self.fail_status.load(Ordering::SeqCst) {
                        return Err(AudioError::new(
                            AudioErrorKind::NativeFailure,
                            "fake audio status has no receipt",
                        ));
                    }
                    let recording = lock(&self.recordings).contains(&context.owner);
                    let playing = lock(&self.active)
                        .values()
                        .any(|operation| operation.owner == context.owner);
                    Ok(AudioOperationSuccess::Status {
                        status: media_api::audio::AudioStatus { recording, playing },
                    })
                }
                _ => Err(AudioError::new(
                    AudioErrorKind::Unsupported,
                    "fake direct audio operation unsupported",
                )),
            }
        }

        async fn cancel(&self, context: AudioOperationContext) -> Result<(), AudioError> {
            lock(&self.cancel_contexts).push(context.clone());
            self.cancel_started.notify_one();
            let gate = lock(&self.cancel_gate).clone();
            if let Some(gate) = gate {
                gate.notified().await;
            }
            if self.fail_cancel.load(Ordering::SeqCst) {
                return Err(AudioError::new(
                    AudioErrorKind::NativeFailure,
                    "fake audio cancellation has no receipt",
                ));
            }
            if let Some(operation) = lock(&self.active).get(&context.identity) {
                if operation.owner == context.owner
                    && operation.handle == context.operation_incarnation()
                {
                    operation.cancel.cancel();
                }
            }
            Ok(())
        }
    }

    #[async_trait]
    impl MediaAudioChannel for CountingAudioChannel {
        async fn read(&self) -> Result<Option<media_api::MediaFrame>, MediaError> {
            self.calls.fetch_add(1, Ordering::SeqCst);
            Ok(None)
        }

        async fn write(&self, _frame: MediaPcmFrame) -> Result<MediaRenderReceipt, MediaError> {
            self.calls.fetch_add(1, Ordering::SeqCst);
            Ok(MediaRenderReceipt {
                accepted_sample_frames: 0,
                played_sample_frames: 0,
                rendered_at_unix_ms: None,
            })
        }

        async fn flush(&self) -> Result<MediaRenderReceipt, MediaError> {
            self.calls.fetch_add(1, Ordering::SeqCst);
            Ok(MediaRenderReceipt {
                accepted_sample_frames: 0,
                played_sample_frames: 0,
                rendered_at_unix_ms: None,
            })
        }

        async fn interrupt(&self) -> Result<MediaRenderReceipt, MediaError> {
            self.calls.fetch_add(1, Ordering::SeqCst);
            Ok(MediaRenderReceipt {
                accepted_sample_frames: 0,
                played_sample_frames: 0,
                rendered_at_unix_ms: None,
            })
        }
    }

    #[test]
    fn begin_revoke_closes_admission_without_a_tokio_runtime() {
        let shared = Arc::new(FakeService::default());
        let host = MediaAttachmentHost::new();
        let attachment = host.attach(shared.clone()).unwrap();

        attachment.begin_revoke();

        assert_eq!(attachment.phase(), MediaAttachmentPhase::Revoking);
        assert!(host.attach(shared).is_err());
    }

    #[tokio::test]
    async fn begin_revoke_synchronously_closes_admission_before_drain_starts() {
        let shared = Arc::new(FakeService::default());
        let host = MediaAttachmentHost::new();
        let attachment = host.attach(shared.clone()).unwrap();

        attachment.begin_revoke();
        assert_eq!(attachment.phase(), MediaAttachmentPhase::Revoking);
        assert_eq!(
            attachment
                .service()
                .execute(context(owner(), 1), capture())
                .await
                .unwrap_err()
                .kind,
            MediaErrorKind::Cancelled
        );
        assert!(host.attach(shared).is_err());
        assert_eq!(
            attachment.revoke(Duration::from_secs(1)).await,
            MediaAttachmentRevocation::Confirmed
        );
    }

    #[tokio::test]
    async fn admission_closed_notifies_existing_and_late_waiters() {
        let host = MediaAttachmentHost::new();
        let attachment = host.attach(Arc::new(FakeService::default())).unwrap();
        let mut waiter = Box::pin(attachment.wait_for_admission_closed());
        std::future::poll_fn(|cx| {
            assert!(std::future::Future::poll(waiter.as_mut(), cx).is_pending());
            std::task::Poll::Ready(())
        })
        .await;

        attachment.begin_revoke();
        std::future::poll_fn(|cx| {
            assert!(std::future::Future::poll(waiter.as_mut(), cx).is_ready());
            std::task::Poll::Ready(())
        })
        .await;
        tokio::time::timeout(
            Duration::from_millis(100),
            attachment.wait_for_admission_closed(),
        )
        .await
        .expect("a late admission watcher completes immediately");
        assert_eq!(
            attachment.revoke(Duration::from_secs(1)).await,
            MediaAttachmentRevocation::Confirmed
        );
    }

    #[tokio::test]
    async fn child_admission_watcher_wakes_when_parent_closes_before_child_drain() {
        let host = MediaAttachmentHost::new();
        let parent = host.attach(Arc::new(FakeService::default())).unwrap();
        let child = parent.fork_child().unwrap();
        let mut waiter = Box::pin(child.wait_for_admission_closed());
        std::future::poll_fn(|cx| {
            assert!(std::future::Future::poll(waiter.as_mut(), cx).is_pending());
            std::task::Poll::Ready(())
        })
        .await;

        parent.begin_revoke();
        assert_eq!(child.phase(), MediaAttachmentPhase::Revoking);
        assert!(!lock(&child.service.lifecycle.state).drain_started);
        std::future::poll_fn(|cx| {
            assert!(std::future::Future::poll(waiter.as_mut(), cx).is_ready());
            std::task::Poll::Ready(())
        })
        .await;
        tokio::time::timeout(
            Duration::from_millis(100),
            child.wait_for_admission_closed(),
        )
        .await
        .expect("a late child watcher sees closed parent admission");
        assert!(child.authorize_owner(&owner()).is_err());
        assert_eq!(
            parent.revoke(Duration::from_secs(1)).await,
            MediaAttachmentRevocation::Confirmed
        );
    }

    #[tokio::test]
    async fn starting_drain_without_begin_revoke_closes_admission_watchers() {
        let host = MediaAttachmentHost::new();
        let attachment = host.attach(Arc::new(FakeService::default())).unwrap();
        let mut waiter = Box::pin(attachment.wait_for_admission_closed());
        std::future::poll_fn(|cx| {
            assert!(std::future::Future::poll(waiter.as_mut(), cx).is_pending());
            std::task::Poll::Ready(())
        })
        .await;
        attachment.service.start_drain();
        std::future::poll_fn(|cx| {
            assert!(std::future::Future::poll(waiter.as_mut(), cx).is_ready());
            std::task::Poll::Ready(())
        })
        .await;
        assert_eq!(
            attachment.revoke(Duration::from_secs(1)).await,
            MediaAttachmentRevocation::Confirmed
        );
    }

    #[tokio::test]
    async fn root_revoke_synchronously_fences_all_descendants_and_waits_for_grandchild_permit() {
        let host = MediaAttachmentHost::new();
        let root = host.attach(Arc::new(FakeService::default())).unwrap();
        let child = root.fork_child().unwrap();
        let grandchild = child.fork_child().unwrap();
        let permit = grandchild.authorize_owner(&owner()).unwrap();
        let mut waiter = Box::pin(grandchild.wait_for_admission_closed());
        std::future::poll_fn(|cx| {
            assert!(std::future::Future::poll(waiter.as_mut(), cx).is_pending());
            std::task::Poll::Ready(())
        })
        .await;

        root.begin_revoke();
        assert_eq!(child.phase(), MediaAttachmentPhase::Revoking);
        assert_eq!(grandchild.phase(), MediaAttachmentPhase::Revoking);
        assert!(!grandchild.admission_is_open());
        assert!(grandchild.authorize_owner(&owner()).is_err());
        std::future::poll_fn(|cx| {
            assert!(std::future::Future::poll(waiter.as_mut(), cx).is_ready());
            std::task::Poll::Ready(())
        })
        .await;
        assert!(matches!(
            root.revoke(Duration::from_millis(10)).await,
            MediaAttachmentRevocation::Unknown { .. }
        ));
        drop(permit);
        assert_eq!(
            root.revoke(Duration::from_secs(1)).await,
            MediaAttachmentRevocation::Confirmed
        );
        assert_eq!(grandchild.phase(), MediaAttachmentPhase::Revoked);
    }

    #[tokio::test]
    async fn grandchild_fork_racing_root_revoke_is_registered_and_closed_or_rejected() {
        for _ in 0..100 {
            let host = MediaAttachmentHost::new();
            let root = host.attach(Arc::new(FakeService::default())).unwrap();
            let child = root.fork_child().unwrap();
            let barrier = Arc::new(std::sync::Barrier::new(3));
            let fork_child = child.clone();
            let fork_barrier = barrier.clone();
            let fork = std::thread::spawn(move || {
                fork_barrier.wait();
                fork_child.fork_child()
            });
            let revoke_root = root.clone();
            let revoke_barrier = barrier.clone();
            let revoke = std::thread::spawn(move || {
                revoke_barrier.wait();
                revoke_root.begin_revoke();
            });
            barrier.wait();
            let grandchild = fork.join().unwrap().ok();
            revoke.join().unwrap();
            if let Some(grandchild) = grandchild {
                assert_eq!(grandchild.phase(), MediaAttachmentPhase::Revoking);
                assert!(grandchild.authorize_owner(&owner()).is_err());
                tokio::time::timeout(
                    Duration::from_millis(100),
                    grandchild.wait_for_admission_closed(),
                )
                .await
                .unwrap();
            }
            assert_eq!(
                root.revoke(Duration::from_secs(1)).await,
                MediaAttachmentRevocation::Confirmed
            );
        }
    }

    #[tokio::test(start_paused = true)]
    async fn drain_timeout_keeps_an_unconfirmed_audio_operation_and_owner() {
        let shared = Arc::new(FakeService::default());
        let host = MediaAttachmentHost::new();
        let attachment = host.attach(shared.clone()).unwrap();
        let audio = Arc::new(FakeDirectAudioService::default());
        let scoped_audio = attachment.audio_service(audio.clone()).unwrap();
        let logical_owner = owner();
        let context =
            AudioOperationContext::new(AudioOperationId::new(104, 23), logical_owner.clone());
        let operation = tokio::spawn(async move {
            scoped_audio
                .execute(context, AudioOperation::Listen { language: None })
                .await
        });
        wait_for_audio_notification(&audio.listen_started, "timeout Listen").await;
        audio.fail_cancel.store(true, Ordering::SeqCst);
        attachment.service.start_drain();
        wait_for_audio_notification(&audio.cancel_started, "timeout audio cancel").await;

        tokio::time::advance(ATTACHMENT_DRAIN_BUDGET + Duration::from_secs(1)).await;
        assert_eq!(
            tokio::time::timeout(
                Duration::from_secs(1),
                attachment.wait_for_drain_completion()
            )
            .await
            .expect("drain must leave Revoking after its budget"),
            MediaAttachmentPhase::Unknown
        );
        {
            let state = lock(&attachment.service.lifecycle.state);
            assert!(state.audio_owners.contains(&logical_owner));
            assert_eq!(state.audio_operations.len(), 1);
            assert!(state.active_calls > 0);
        }
        assert!(host.attachment(attachment.id()).is_some());
        assert!(host.attach(shared).is_err());
        operation.abort();
    }

    #[tokio::test(start_paused = true)]
    async fn drain_timeout_keeps_a_hanging_media_stop_unknown() {
        let shared = Arc::new(FakeService::default());
        shared.block_capture.store(true, Ordering::SeqCst);
        shared.block_stop.store(true, Ordering::SeqCst);
        let host = MediaAttachmentHost::new();
        let attachment = host.attach(shared.clone()).unwrap();
        let service = attachment.service();
        let operation =
            tokio::spawn(async move { service.execute(context(owner(), 105), capture()).await });
        wait_for_audio_notification(&shared.capture_started, "timeout Capture").await;
        attachment.service.start_drain();
        wait_for_audio_notification(&shared.stop_started, "timeout media Stop").await;

        tokio::time::advance(ATTACHMENT_DRAIN_BUDGET + Duration::from_secs(1)).await;
        assert_eq!(
            tokio::time::timeout(
                Duration::from_secs(1),
                attachment.wait_for_drain_completion()
            )
            .await
            .expect("drain must leave Revoking after its budget"),
            MediaAttachmentPhase::Unknown
        );
        {
            let state = lock(&attachment.service.lifecycle.state);
            assert!(state.active_calls > 0);
            assert!(state
                .stop_attempts
                .values()
                .any(|attempt| *attempt == StopAttemptState::Unknown));
        }
        assert!(host.attachment(attachment.id()).is_some());
        assert!(host.attach(shared).is_err());
        operation.abort();
    }

    #[tokio::test(start_paused = true)]
    async fn total_drain_timeout_then_late_native_stop_keeps_unknown_and_owner_fenced() {
        let native = Arc::new(FakeService::default());
        native.block_capture.store(true, Ordering::SeqCst);
        let service = Arc::new(LateNativeStopService {
            inner: native.clone(),
            stop_gate: Arc::new(Notify::new()),
            stop_started: Arc::new(Notify::new()),
            stop_completed: Arc::new(Notify::new()),
            stop_dispatches: AtomicUsize::new(0),
        });
        let host = MediaAttachmentHost::new();
        let attachment = host.attach(service.clone()).unwrap();
        let attachment_id = attachment.id().clone();
        let operation_service = attachment.service();
        let operation = tokio::spawn(async move {
            operation_service
                .execute(context(owner(), 106), capture())
                .await
        });
        wait_for_audio_notification(&native.capture_started, "late-stop Capture").await;

        attachment.service.start_drain();
        wait_for_audio_notification(&service.stop_started, "late native Stop").await;
        tokio::time::advance(ATTACHMENT_DRAIN_BUDGET + Duration::from_secs(1)).await;
        assert_eq!(
            tokio::time::timeout(
                Duration::from_secs(1),
                attachment.wait_for_drain_completion()
            )
            .await
            .expect("the total drain budget must settle its observer"),
            MediaAttachmentPhase::Unknown
        );
        assert_eq!(attachment.phase(), MediaAttachmentPhase::Unknown);
        assert_eq!(service.stop_dispatches.load(Ordering::SeqCst), 1);
        assert!(host.attachment(&attachment_id).is_some());

        // The detached native operation succeeds after its Rust future has
        // been dropped. No receipt reaches the drain, so this cannot promote
        // the attachment to Confirmed or release its owner fence.
        service.stop_gate.notify_one();
        wait_for_audio_notification(&service.stop_completed, "late native Stop completion").await;
        assert!(matches!(
            tokio::time::timeout(Duration::from_secs(1), operation)
                .await
                .expect("the late native Stop should unblock Capture")
                .unwrap(),
            Ok(MediaResult::Captured { .. })
        ));
        assert!(lock(&native.active).is_empty());
        assert_eq!(attachment.phase(), MediaAttachmentPhase::Unknown);
        assert!(matches!(
            attachment.revoke(Duration::from_secs(1)).await,
            MediaAttachmentRevocation::Unknown { .. }
        ));
        assert_eq!(service.stop_dispatches.load(Ordering::SeqCst), 1);
        assert!(host.attachment(&attachment_id).is_some());
        assert!(host.attach(Arc::new(FakeService::default())).is_err());
    }

    #[tokio::test]
    async fn same_session_id_isolated_across_engine_attachments() {
        let shared = Arc::new(FakeService::default());
        let host = MediaAttachmentHost::new();
        let first = host.attach(shared.clone()).unwrap();
        let second = host.attach(shared).unwrap();
        assert_ne!(first.id(), second.id());

        let captured = first
            .service()
            .execute(context(owner(), 1), capture())
            .await
            .unwrap();
        let MediaResult::Captured { asset } = captured else {
            panic!("fake capture should return an asset");
        };
        assert_eq!(
            first
                .service()
                .read_media(context(owner(), 2), asset.reference.clone())
                .await
                .unwrap()
                .bytes,
            vec![1, 2, 3]
        );
        assert_eq!(
            second
                .service()
                .read_media(context(owner(), 3), asset.reference)
                .await
                .unwrap_err()
                .kind,
            MediaErrorKind::NotFound
        );

        // The caller cannot replace the wrapper's host-issued namespace with
        // another attachment identity.
        let forged = owner().in_attachment(first.id().clone());
        assert_eq!(
            second
                .service()
                .execute(context(forged, 4), capture())
                .await
                .unwrap_err()
                .kind,
            MediaErrorKind::InvalidRequest
        );
    }

    #[tokio::test]
    async fn direct_audio_is_scoped_and_drained_with_cancel_end_owner_and_status_receipts() {
        let media = Arc::new(FakeService::default());
        let audio = Arc::new(FakeDirectAudioService::default());
        let host = MediaAttachmentHost::new();
        let first = host.attach(media.clone()).unwrap();
        let second = host.attach(media).unwrap();
        let first_audio = first.audio_service(audio.clone()).unwrap();
        let second_audio = second.audio_service(audio.clone()).unwrap();
        let logical_owner = owner();

        first_audio
            .execute(
                AudioOperationContext::new(AudioOperationId::new(1, 23), logical_owner.clone()),
                AudioOperation::StartRecording {
                    sample_rate_hz: 24_000,
                    format: "wav".into(),
                },
            )
            .await
            .unwrap();
        second_audio
            .execute(
                AudioOperationContext::new(AudioOperationId::new(2, 23), logical_owner.clone()),
                AudioOperation::StartRecording {
                    sample_rate_hz: 24_000,
                    format: "wav".into(),
                },
            )
            .await
            .unwrap();

        let listen_context =
            AudioOperationContext::new(AudioOperationId::new(3, 23), logical_owner.clone());
        let pending_audio = first_audio.clone();
        let listen = tokio::spawn(async move {
            pending_audio
                .execute(listen_context, AudioOperation::Listen { language: None })
                .await
        });
        audio.listen_started.notified().await;
        let first_owner = logical_owner.clone().in_attachment(first.id().clone());
        let second_owner = logical_owner.clone().in_attachment(second.id().clone());
        assert!(lock(&audio.owners).contains(&first_owner));
        assert!(lock(&audio.owners).contains(&second_owner));

        assert_eq!(
            first.revoke(Duration::from_secs(1)).await,
            MediaAttachmentRevocation::Confirmed
        );
        assert!(matches!(
            listen.await.unwrap(),
            Err(AudioError {
                kind: AudioErrorKind::Cancelled,
                ..
            })
        ));
        assert!(lock(&audio.cancel_contexts)
            .iter()
            .any(|context| context.owner == first_owner));
        assert!(lock(&audio.ended_owners).contains(&first_owner));
        assert!(!lock(&audio.recordings).contains(&first_owner));
        assert!(lock(&audio.recordings).contains(&second_owner));

        let status = second_audio
            .execute(
                AudioOperationContext::new(AudioOperationId::new(4, 23), logical_owner),
                AudioOperation::Status { handle: None },
            )
            .await
            .unwrap();
        assert!(matches!(
            status,
            AudioOperationSuccess::Status {
                status: media_api::audio::AudioStatus {
                    recording: true,
                    ..
                }
            }
        ));
        assert_eq!(
            second.revoke(Duration::from_secs(1)).await,
            MediaAttachmentRevocation::Confirmed
        );
        assert!(lock(&audio.recordings).is_empty());
        assert!(host.attachments().is_empty());
    }

    #[tokio::test]
    async fn unknown_direct_audio_end_owner_keeps_attachment_fenced_without_retry() {
        let media = Arc::new(FakeService::default());
        let audio = Arc::new(FakeDirectAudioService::default());
        let host = MediaAttachmentHost::new();
        let attachment = host.attach(media).unwrap();
        let scoped_audio = attachment.audio_service(audio.clone()).unwrap();
        let logical_owner = owner();
        scoped_audio
            .execute(
                AudioOperationContext::new(AudioOperationId::new(5, 23), logical_owner.clone()),
                AudioOperation::StartRecording {
                    sample_rate_hz: 24_000,
                    format: "wav".into(),
                },
            )
            .await
            .unwrap();
        audio.fail_end_owner.store(true, Ordering::SeqCst);

        assert!(matches!(
            attachment.revoke(Duration::from_secs(1)).await,
            MediaAttachmentRevocation::Unknown { .. }
        ));
        assert_eq!(attachment.phase(), MediaAttachmentPhase::Unknown);
        assert_eq!(audio.end_owner_calls.load(Ordering::SeqCst), 1);
        assert!(host.attachment(attachment.id()).is_some());
        assert!(matches!(
            scoped_audio
                .execute(
                    AudioOperationContext::new(AudioOperationId::new(6, 23), logical_owner,),
                    AudioOperation::Listen { language: None },
                )
                .await,
            Err(AudioError {
                kind: AudioErrorKind::Cancelled,
                ..
            })
        ));
        assert!(matches!(
            attachment.revoke(Duration::from_secs(1)).await,
            MediaAttachmentRevocation::Unknown { .. }
        ));
        assert_eq!(audio.end_owner_calls.load(Ordering::SeqCst), 1);
        assert!(lock(&audio.recordings).contains(&owner().in_attachment(attachment.id().clone())));
    }

    #[tokio::test]
    async fn direct_audio_cancel_pins_its_incarnation_until_its_receipt() {
        let host = MediaAttachmentHost::new();
        let attachment = host.attach(Arc::new(FakeService::default())).unwrap();
        let audio = Arc::new(FakeDirectAudioService::default());
        let scoped_audio = attachment.audio_service(audio.clone()).unwrap();
        let owner = owner();
        let context = AudioOperationContext::new(AudioOperationId::new(91, 23), owner.clone());
        let execute_context = context.clone();
        let execute_audio = scoped_audio.clone();
        let operation = tokio::spawn(async move {
            execute_audio
                .execute(execute_context, AudioOperation::Listen { language: None })
                .await
        });
        audio.listen_started.notified().await;

        let gate = Arc::new(Notify::new());
        *lock(&audio.cancel_gate) = Some(gate.clone());
        let cancel_audio = scoped_audio.clone();
        let cancel_context = context.clone();
        let cancel = tokio::spawn(async move { cancel_audio.cancel(cancel_context).await });
        audio.cancel_started.notified().await;

        // Complete A independently while its exact cancel is still blocked
        // below the attachment facade. The facade must retain A's lease.
        audio.finish_active(&context.identity);
        assert!(matches!(
            operation.await.unwrap(),
            Err(AudioError {
                kind: AudioErrorKind::Cancelled,
                ..
            })
        ));

        let reused = scoped_audio
            .execute(
                AudioOperationContext::new(context.identity.clone(), owner.clone()),
                AudioOperation::Listen { language: None },
            )
            .await
            .unwrap_err();
        assert_eq!(reused.kind, AudioErrorKind::InvalidRequest);

        gate.notify_one();
        cancel.await.unwrap().unwrap();
        *lock(&audio.cancel_gate) = None;

        let reuse_audio = scoped_audio.clone();
        let reuse_context = AudioOperationContext::new(context.identity.clone(), owner);
        let reuse = tokio::spawn(async move {
            reuse_audio
                .execute(reuse_context, AudioOperation::Listen { language: None })
                .await
        });
        audio.listen_started.notified().await;
        audio.finish_active(&context.identity);
        assert!(matches!(
            reuse.await.unwrap(),
            Err(AudioError {
                kind: AudioErrorKind::Cancelled,
                ..
            })
        ));
    }

    #[tokio::test]
    async fn direct_audio_admission_rejects_replay_of_a_cloned_context() {
        let host = MediaAttachmentHost::new();
        let attachment = host.attach(Arc::new(FakeService::default())).unwrap();
        let audio = Arc::new(FakeDirectAudioService::default());
        let scoped_audio = attachment.audio_service(audio).unwrap();
        let context = AudioOperationContext::new(AudioOperationId::new(93, 23), owner());

        scoped_audio
            .execute(context.clone(), AudioOperation::Status { handle: None })
            .await
            .unwrap();
        assert_eq!(
            scoped_audio
                .execute(context, AudioOperation::Status { handle: None })
                .await
                .unwrap_err()
                .kind,
            AudioErrorKind::InvalidRequest
        );
    }

    #[tokio::test]
    async fn end_owner_fences_status_and_joins_an_existing_exact_cancel() {
        let host = MediaAttachmentHost::new();
        let attachment = host.attach(Arc::new(FakeService::default())).unwrap();
        let audio = Arc::new(FakeDirectAudioService::default());
        let scoped_audio = attachment.audio_service(audio.clone()).unwrap();
        let owner = owner();
        let operation_context =
            AudioOperationContext::new(AudioOperationId::new(94, 23), owner.clone());
        let operation_audio = scoped_audio.clone();
        let operation_context_for_task = operation_context.clone();
        let listen = tokio::spawn(async move {
            operation_audio
                .execute(
                    operation_context_for_task,
                    AudioOperation::Listen { language: None },
                )
                .await
        });
        wait_for_audio_notification(&audio.listen_started, "owner-fence Listen").await;

        let gate = Arc::new(Notify::new());
        *lock(&audio.cancel_gate) = Some(gate.clone());
        let cancel_audio = scoped_audio.clone();
        let cancel_context = operation_context.clone();
        let cancel = tokio::spawn(async move { cancel_audio.cancel(cancel_context).await });
        wait_for_audio_notification(&audio.cancel_started, "preexisting exact cancel").await;

        let end_audio = scoped_audio.clone();
        let end_context = AudioOperationContext::new(AudioOperationId::new(95, 23), owner.clone());
        let mut end_owner = tokio::spawn(async move {
            end_audio
                .execute(end_context, AudioOperation::EndOwner)
                .await
        });
        let scoped_owner = owner.clone().in_attachment(attachment.id().clone());
        tokio::time::timeout(Duration::from_secs(2), async {
            while attachment.service.audio_end_owner_state(&scoped_owner)
                != Some(AudioEndOwnerState::InFlight)
            {
                tokio::task::yield_now().await;
            }
        })
        .await
        .expect("EndOwner must establish its fence before Status is tested");
        let status = scoped_audio
            .execute(
                AudioOperationContext::new(AudioOperationId::new(96, 23), owner),
                AudioOperation::Status { handle: None },
            )
            .await
            .unwrap_err();
        assert_eq!(status.kind, AudioErrorKind::Busy);
        assert!(
            tokio::time::timeout(Duration::from_millis(25), &mut end_owner)
                .await
                .is_err()
        );
        assert_eq!(lock(&audio.cancel_contexts).len(), 1);

        gate.notify_one();
        tokio::time::timeout(Duration::from_secs(2), cancel)
            .await
            .expect("preexisting cancel must settle after release")
            .unwrap()
            .unwrap();
        assert!(matches!(
            tokio::time::timeout(Duration::from_secs(2), listen)
                .await
                .expect("Listen must finish after cancel receipt")
                .unwrap(),
            Err(AudioError {
                kind: AudioErrorKind::Cancelled,
                ..
            })
        ));
        assert!(matches!(
            tokio::time::timeout(Duration::from_secs(2), &mut end_owner)
                .await
                .expect("EndOwner must settle after old call joins")
                .unwrap()
                .unwrap(),
            AudioOperationSuccess::OwnerEnded
        ));
        assert_eq!(lock(&audio.cancel_contexts).len(), 1);
        assert_eq!(audio.end_owner_calls.load(Ordering::SeqCst), 1);
    }

    #[tokio::test]
    async fn attachment_drain_waits_for_an_existing_exact_cancel_before_retrying() {
        let host = MediaAttachmentHost::new();
        let attachment = host.attach(Arc::new(FakeService::default())).unwrap();
        let audio = Arc::new(FakeDirectAudioService::default());
        let scoped_audio = attachment.audio_service(audio.clone()).unwrap();
        let owner = owner();
        let context = AudioOperationContext::new(AudioOperationId::new(97, 23), owner);
        let operation_audio = scoped_audio.clone();
        let operation_context = context.clone();
        let listen = tokio::spawn(async move {
            operation_audio
                .execute(operation_context, AudioOperation::Listen { language: None })
                .await
        });
        wait_for_audio_notification(&audio.listen_started, "drain-race Listen").await;

        let gate = Arc::new(Notify::new());
        *lock(&audio.cancel_gate) = Some(gate.clone());
        let cancel_audio = scoped_audio.clone();
        let cancel = tokio::spawn(async move { cancel_audio.cancel(context).await });
        wait_for_audio_notification(&audio.cancel_started, "drain-race exact cancel").await;

        assert!(matches!(
            attachment.revoke(Duration::from_millis(25)).await,
            MediaAttachmentRevocation::Unknown { .. }
        ));
        assert_eq!(lock(&audio.cancel_contexts).len(), 1);
        gate.notify_one();
        tokio::time::timeout(Duration::from_secs(2), cancel)
            .await
            .expect("exact cancel must settle")
            .unwrap()
            .unwrap();
        assert!(matches!(
            tokio::time::timeout(Duration::from_secs(2), listen)
                .await
                .expect("Listen must finish after exact cancel")
                .unwrap(),
            Err(AudioError {
                kind: AudioErrorKind::Cancelled,
                ..
            })
        ));
        assert_eq!(
            attachment.revoke(Duration::from_secs(2)).await,
            MediaAttachmentRevocation::Confirmed
        );
        assert_eq!(lock(&audio.cancel_contexts).len(), 1);
    }

    #[tokio::test]
    async fn parent_revoke_waits_for_an_admitted_child_end_owner_receipt() {
        let host = MediaAttachmentHost::new();
        let parent = host.attach(Arc::new(FakeService::default())).unwrap();
        let child = parent.fork_child().unwrap();
        let audio = Arc::new(FakeDirectAudioService::default());
        let child_audio = child.audio_service(audio.clone()).unwrap();
        let owner = owner();
        let gate = Arc::new(Notify::new());
        *lock(&audio.end_owner_gate) = Some(gate.clone());
        let end_context = AudioOperationContext::new(AudioOperationId::new(101, 23), owner);
        let end = tokio::spawn(async move {
            child_audio
                .execute(end_context, AudioOperation::EndOwner)
                .await
        });
        wait_for_audio_notification(&audio.end_owner_started, "child EndOwner start").await;

        assert!(matches!(
            parent.revoke(Duration::from_millis(25)).await,
            MediaAttachmentRevocation::Unknown { .. }
        ));
        assert!(lock(&audio.cancel_contexts).is_empty());
        gate.notify_one();
        assert!(matches!(
            tokio::time::timeout(Duration::from_secs(2), end)
                .await
                .expect("child EndOwner must settle after receipt gate opens")
                .unwrap(),
            Ok(AudioOperationSuccess::OwnerEnded)
        ));
        assert_eq!(
            parent.revoke(Duration::from_secs(2)).await,
            MediaAttachmentRevocation::Confirmed
        );
        assert_eq!(parent.phase(), MediaAttachmentPhase::Revoked);
        assert_eq!(child.phase(), MediaAttachmentPhase::Revoked);
        assert!(lock(&audio.cancel_contexts).is_empty());
        assert_eq!(audio.end_owner_calls.load(Ordering::SeqCst), 1);
    }

    #[tokio::test(start_paused = true)]
    async fn parent_total_drain_timeout_then_late_child_success_keeps_parent_unknown() {
        let host = MediaAttachmentHost::new();
        let parent = host.attach(Arc::new(FakeService::default())).unwrap();
        let child = parent.fork_child().unwrap();
        let child_id = child.id().clone();

        // Hold a child drain flight open independently of the parent's worker.
        // This isolates the parent join timeout from the child's own deadline,
        // so its later completion is deterministic.
        {
            let mut state = lock(&child.service.lifecycle.state);
            state.phase = MediaAttachmentPhase::Revoking;
            state.drain_started = true;
            state.drain_running = true;
        }
        let release_child = Arc::new(Notify::new());
        let child_finished = Arc::new(Notify::new());
        let child_to_finish = child.clone();
        let release = release_child.clone();
        let finished = child_finished.clone();
        let child_completion = tokio::spawn(async move {
            release.notified().await;
            {
                let mut state = lock(&child_to_finish.service.lifecycle.state);
                assert_eq!(state.phase, MediaAttachmentPhase::Revoking);
                assert!(state.drain_running);
                state.drain_running = false;
                state.phase = MediaAttachmentPhase::Revoked;
                state.failure = None;
            }
            child_to_finish.service.lifecycle.changed.notify_waiters();
            child_to_finish.service.remove_from_host();
            finished.notify_one();
        });

        parent.service.start_drain();
        assert!(
            tokio::time::timeout(Duration::from_millis(1), parent.wait_for_drain_completion())
                .await
                .is_err(),
            "parent drain should remain pending on the child flight"
        );
        tokio::time::advance(ATTACHMENT_DRAIN_BUDGET + Duration::from_secs(1)).await;
        assert_eq!(
            tokio::time::timeout(Duration::from_secs(1), parent.wait_for_drain_completion())
                .await
                .expect("parent total budget must settle its observer"),
            MediaAttachmentPhase::Unknown
        );
        assert_eq!(child.phase(), MediaAttachmentPhase::Revoking);
        assert!(host.attachment(parent.id()).is_some());
        assert!(host.attachment(&child_id).is_some());

        release_child.notify_one();
        tokio::time::timeout(Duration::from_secs(1), child_completion)
            .await
            .expect("the child may finish after the parent flight was dropped")
            .unwrap();
        tokio::time::timeout(Duration::from_secs(1), child_finished.notified())
            .await
            .expect("late child completion should be observable");
        assert_eq!(child.phase(), MediaAttachmentPhase::Revoked);
        assert!(host.attachment(&child_id).is_none());
        assert_eq!(parent.phase(), MediaAttachmentPhase::Unknown);
        assert!(matches!(
            parent.revoke(Duration::from_secs(1)).await,
            MediaAttachmentRevocation::Unknown { .. }
        ));
        assert!(host.attachment(parent.id()).is_some());
        assert!(host.attach(Arc::new(FakeService::default())).is_err());
    }

    #[tokio::test]
    async fn unknown_cancel_during_end_owner_keeps_owner_unknown_and_blocks_status() {
        let host = MediaAttachmentHost::new();
        let attachment = host.attach(Arc::new(FakeService::default())).unwrap();
        let audio = Arc::new(FakeDirectAudioService::default());
        let scoped_audio = attachment.audio_service(audio.clone()).unwrap();
        let owner = owner();
        let context = AudioOperationContext::new(AudioOperationId::new(98, 23), owner.clone());
        let operation_audio = scoped_audio.clone();
        let operation_context = context.clone();
        let listen = tokio::spawn(async move {
            operation_audio
                .execute(operation_context, AudioOperation::Listen { language: None })
                .await
        });
        wait_for_audio_notification(&audio.listen_started, "unknown EndOwner Listen").await;

        audio.fail_cancel.store(true, Ordering::SeqCst);
        let ended = scoped_audio
            .execute(
                AudioOperationContext::new(AudioOperationId::new(99, 23), owner.clone()),
                AudioOperation::EndOwner,
            )
            .await
            .unwrap_err();
        assert_eq!(ended.kind, AudioErrorKind::NativeFailure);
        assert_eq!(audio.end_owner_calls.load(Ordering::SeqCst), 0);
        assert_eq!(
            scoped_audio
                .execute(
                    AudioOperationContext::new(AudioOperationId::new(100, 23), owner),
                    AudioOperation::Status { handle: None },
                )
                .await
                .unwrap_err()
                .kind,
            AudioErrorKind::Unavailable
        );

        audio.finish_active(&context.identity);
        assert!(matches!(
            tokio::time::timeout(Duration::from_secs(2), listen)
                .await
                .expect("Listen must finish when test releases its token")
                .unwrap(),
            Err(AudioError {
                kind: AudioErrorKind::Cancelled,
                ..
            })
        ));
        assert!(matches!(
            attachment.revoke(Duration::from_secs(2)).await,
            MediaAttachmentRevocation::Unknown { .. }
        ));
        assert!(host.attachment(attachment.id()).is_some());
    }

    #[tokio::test]
    async fn unknown_status_after_drain_end_owner_retains_owner_fence() {
        let host = MediaAttachmentHost::new();
        let attachment = host.attach(Arc::new(FakeService::default())).unwrap();
        let audio = Arc::new(FakeDirectAudioService::default());
        let scoped_audio = attachment.audio_service(audio.clone()).unwrap();
        let logical_owner = owner();
        scoped_audio
            .execute(
                AudioOperationContext::new(AudioOperationId::new(102, 23), logical_owner.clone()),
                AudioOperation::StartRecording {
                    sample_rate_hz: 24_000,
                    format: "wav".into(),
                },
            )
            .await
            .unwrap();
        audio.fail_status.store(true, Ordering::SeqCst);

        assert!(matches!(
            attachment.revoke(Duration::from_secs(2)).await,
            MediaAttachmentRevocation::Unknown { .. }
        ));
        let scoped_owner = logical_owner.clone().in_attachment(attachment.id().clone());
        let lifecycle = &attachment.service.lifecycle;
        assert_eq!(
            lock(&lifecycle.state)
                .audio_end_owner_attempts
                .get(&scoped_owner),
            Some(&AudioEndOwnerState::Unknown)
        );
        assert!(lock(&lifecycle.state).audio_owners.contains(&logical_owner));
        assert_eq!(audio.end_owner_calls.load(Ordering::SeqCst), 1);
        assert!(!lock(&audio.recordings).contains(&scoped_owner));
        assert!(host.attachment(attachment.id()).is_some());
    }

    #[tokio::test]
    async fn unknown_direct_audio_cancel_keeps_identity_fenced_until_exact_retry() {
        let host = MediaAttachmentHost::new();
        let attachment = host.attach(Arc::new(FakeService::default())).unwrap();
        let audio = Arc::new(FakeDirectAudioService::default());
        let scoped_audio = attachment.audio_service(audio.clone()).unwrap();
        let owner = owner();
        let context = AudioOperationContext::new(AudioOperationId::new(92, 23), owner.clone());
        let execute_audio = scoped_audio.clone();
        let execute_context = context.clone();
        let operation = tokio::spawn(async move {
            execute_audio
                .execute(execute_context, AudioOperation::Listen { language: None })
                .await
        });
        tokio::time::timeout(Duration::from_secs(2), audio.listen_started.notified())
            .await
            .expect("the original Listen must start");

        audio.fail_cancel.store(true, Ordering::SeqCst);
        assert!(
            tokio::time::timeout(Duration::from_secs(2), scoped_audio.cancel(context.clone()))
                .await
                .expect("the initial cancellation must complete")
                .is_err()
        );
        audio.finish_active(&context.identity);
        assert!(matches!(
            tokio::time::timeout(Duration::from_secs(2), operation)
                .await
                .expect("the original Listen must finish")
                .unwrap(),
            Err(AudioError {
                kind: AudioErrorKind::Cancelled,
                ..
            })
        ));

        let reused = tokio::time::timeout(
            Duration::from_secs(2),
            scoped_audio.execute(
                AudioOperationContext::new(context.identity.clone(), owner.clone()),
                AudioOperation::Listen { language: None },
            ),
        )
        .await
        .expect("unconfirmed identity reuse must be rejected without waiting")
        .unwrap_err();
        assert_eq!(reused.kind, AudioErrorKind::InvalidRequest);

        audio.fail_cancel.store(false, Ordering::SeqCst);
        let reuse_id = context.identity.clone();
        tokio::time::timeout(Duration::from_secs(2), scoped_audio.cancel(context))
            .await
            .expect("exact cancellation retry must complete")
            .expect("the exact unresolved cancellation may be retried");
        let reuse_audio = scoped_audio.clone();
        let reuse_context = AudioOperationContext::new(reuse_id.clone(), owner);
        let reuse = tokio::spawn(async move {
            reuse_audio
                .execute(reuse_context, AudioOperation::Listen { language: None })
                .await
        });
        tokio::time::timeout(Duration::from_secs(2), audio.listen_started.notified())
            .await
            .expect("the replacement Listen must start");
        audio.finish_active(&reuse_id);
        assert!(matches!(
            tokio::time::timeout(Duration::from_secs(2), reuse)
                .await
                .expect("the replacement Listen must finish")
                .unwrap(),
            Err(AudioError {
                kind: AudioErrorKind::Cancelled,
                ..
            })
        ));
    }

    #[tokio::test]
    async fn child_attachment_is_owner_isolated_and_parent_revoke_cascades() {
        let shared = Arc::new(FakeService::default());
        shared.block_capture.store(true, Ordering::SeqCst);
        let host = MediaAttachmentHost::new();
        let parent = host.attach(shared.clone()).unwrap();
        let child = parent.fork_child().unwrap();
        assert_ne!(parent.id(), child.id());

        let service = child.service();
        let capture_task =
            tokio::spawn(async move { service.execute(context(owner(), 77), capture()).await });
        shared.capture_started.notified().await;
        let child_owner = owner().in_attachment(child.id().clone());
        assert!(lock(&shared.owners).contains(&child_owner));

        // Parent closure is synchronous. It fences both future service calls
        // and already-returned child audio channels before child drain is
        // polled on the executor.
        let native_channel = Arc::new(CountingAudioChannel::default());
        let scoped_channel = AttachmentScopedAudioChannel {
            inner: native_channel.clone(),
            lifecycle: child.service.lifecycle.clone(),
            parent_lifecycle: child.service.parent_lifecycle.clone(),
        };
        parent.begin_revoke();
        assert_eq!(
            child
                .service()
                .execute(context(owner(), 78), capture())
                .await
                .unwrap_err()
                .kind,
            MediaErrorKind::Cancelled
        );
        assert_eq!(
            scoped_channel.read().await.unwrap_err().kind,
            MediaErrorKind::Cancelled
        );
        assert_eq!(native_channel.calls.load(Ordering::SeqCst), 0);

        assert_eq!(
            parent.revoke(Duration::from_secs(1)).await,
            MediaAttachmentRevocation::Confirmed
        );
        assert_eq!(parent.phase(), MediaAttachmentPhase::Revoked);
        assert_eq!(child.phase(), MediaAttachmentPhase::Revoked);
        assert!(host.attachments().is_empty());
        assert!(matches!(
            capture_task.await.unwrap(),
            Ok(MediaResult::Captured { .. })
        ));
    }

    #[tokio::test]
    async fn fork_racing_parent_begin_revoke_is_either_registered_or_rejected() {
        let host = MediaAttachmentHost::new();
        let parent = host.attach(Arc::new(FakeService::default())).unwrap();
        let barrier = Arc::new(std::sync::Barrier::new(3));
        let fork_parent = parent.clone();
        let fork_barrier = barrier.clone();
        let fork = std::thread::spawn(move || {
            fork_barrier.wait();
            fork_parent.fork_child()
        });
        let revoke_parent = parent.clone();
        let revoke_barrier = barrier.clone();
        let close = std::thread::spawn(move || {
            revoke_barrier.wait();
            revoke_parent.begin_revoke();
        });

        barrier.wait();
        let child = fork.join().unwrap().ok();
        close.join().unwrap();
        assert_eq!(
            parent.revoke(Duration::from_secs(1)).await,
            MediaAttachmentRevocation::Confirmed
        );
        if let Some(child) = child {
            assert_eq!(child.phase(), MediaAttachmentPhase::Revoked);
        }
        assert!(host.attachments().is_empty());
    }

    #[tokio::test]
    async fn every_logical_owner_kind_gets_the_same_attachment_namespace() {
        let shared = Arc::new(FakeService::default());
        let attachment = MediaAttachmentHost::new().attach(shared.clone()).unwrap();
        let owners = [
            owner(),
            MediaOwner::Ui {
                instance_id: "same-ui".into(),
            },
            MediaOwner::System {
                instance_id: "same-system".into(),
            },
        ];
        for (generation, logical_owner) in owners.iter().enumerate() {
            attachment
                .service()
                .execute(
                    context(logical_owner.clone(), generation as u64 + 1),
                    MediaOperation::Status { target: None },
                )
                .await
                .unwrap();
        }
        let captured_owners = lock(&shared.owners);
        assert_eq!(captured_owners.len(), 3);
        for (logical_owner, captured_owner) in owners.iter().zip(captured_owners.iter()) {
            assert_eq!(
                captured_owner,
                &logical_owner.clone().in_attachment(attachment.id().clone())
            );
        }
    }

    #[tokio::test]
    async fn host_route_permit_is_fenced_and_retained_until_registration_drains() {
        let shared = Arc::new(FakeService::default());
        let host = MediaAttachmentHost::new();
        let attachment = host.attach(shared).unwrap();
        let logical_owner = owner();
        let permit = attachment.authorize_owner(&logical_owner).unwrap();
        assert_eq!(permit.attachment_id(), attachment.id());
        assert_eq!(permit.logical_owner(), &logical_owner);
        assert_eq!(
            permit.owner(),
            &logical_owner.clone().in_attachment(attachment.id().clone())
        );

        let revoking = attachment.clone();
        let revoke_task =
            tokio::spawn(async move { revoking.revoke(Duration::from_millis(5)).await });
        assert!(matches!(
            revoke_task.await.unwrap(),
            MediaAttachmentRevocation::Unknown { .. }
        ));
        assert_eq!(attachment.phase(), MediaAttachmentPhase::Revoking);
        assert_eq!(
            attachment.authorize_owner(&logical_owner).unwrap_err().kind,
            MediaErrorKind::Cancelled
        );
        assert_eq!(host.attachments().len(), 1);

        drop(permit);
        assert_eq!(
            attachment.revoke(Duration::from_secs(1)).await,
            MediaAttachmentRevocation::Confirmed
        );
        assert!(host.attachments().is_empty());
    }

    #[tokio::test]
    async fn registration_only_parent_and_child_wait_for_permits_without_media_status() {
        let shared = Arc::new(FakeService::default());
        let host = MediaAttachmentHost::new();
        let root = host.attach(shared.clone()).unwrap();
        let child = root.fork_child().unwrap();
        let root_owner = owner();
        let child_owner = MediaOwner::Ui {
            instance_id: "child-registration".into(),
        };
        let root_permit = root.authorize_owner(&root_owner).unwrap();
        let child_permit = child.authorize_owner(&child_owner).unwrap();
        assert_eq!(
            root_permit.owner(),
            &root_owner.clone().in_attachment(root.id().clone())
        );
        assert_eq!(
            child_permit.owner(),
            &child_owner.clone().in_attachment(child.id().clone())
        );

        root.begin_revoke();
        assert!(root.authorize_owner(&root_owner).is_err());
        assert!(child.authorize_owner(&child_owner).is_err());
        assert!(matches!(
            root.revoke(Duration::from_millis(10)).await,
            MediaAttachmentRevocation::Unknown { .. }
        ));
        assert_eq!(root.phase(), MediaAttachmentPhase::Revoking);
        assert_eq!(child.phase(), MediaAttachmentPhase::Revoking);

        drop(child_permit);
        assert!(matches!(
            root.revoke(Duration::from_millis(10)).await,
            MediaAttachmentRevocation::Unknown { .. }
        ));
        drop(root_permit);
        assert_eq!(
            root.revoke(Duration::from_secs(1)).await,
            MediaAttachmentRevocation::Confirmed
        );
        assert_eq!(child.phase(), MediaAttachmentPhase::Revoked);
        assert!(
            lock(&shared.owners).is_empty(),
            "registration alone must not query media status"
        );
        assert!(host.attachments().is_empty());
    }

    #[tokio::test]
    async fn media_operations_still_drain_only_their_own_owner_after_registration() {
        let shared = Arc::new(FakeService::default());
        let host = MediaAttachmentHost::new();
        let attachment = host.attach(shared.clone()).unwrap();
        let registration_owner = MediaOwner::Ui {
            instance_id: "registration-only".into(),
        };
        let media_owner = owner();
        drop(attachment.authorize_owner(&registration_owner).unwrap());
        attachment
            .service()
            .execute(context(media_owner.clone(), 31), capture())
            .await
            .unwrap();
        lock(&shared.owners).clear();

        assert_eq!(
            attachment.revoke(Duration::from_secs(1)).await,
            MediaAttachmentRevocation::Confirmed
        );
        let queried = lock(&shared.owners);
        assert!(
            !queried.is_empty(),
            "actual media work still needs a drain status"
        );
        let scoped_media_owner = media_owner.in_attachment(attachment.id().clone());
        assert!(queried.iter().all(|owner| owner == &scoped_media_owner));
    }

    #[tokio::test]
    async fn media_operation_with_unavailable_status_retains_unknown_attachment() {
        let shared = Arc::new(FakeService::default());
        let host = MediaAttachmentHost::new();
        let attachment = host.attach(shared.clone()).unwrap();
        attachment
            .service()
            .execute(context(owner(), 32), capture())
            .await
            .unwrap();
        shared.fail_status.store(true, Ordering::SeqCst);

        assert!(matches!(
            attachment.revoke(Duration::from_secs(1)).await,
            MediaAttachmentRevocation::Unknown { .. }
        ));
        assert_eq!(attachment.phase(), MediaAttachmentPhase::Unknown);
        assert!(host.attachment(attachment.id()).is_some());
        assert_eq!(
            host.attach(shared).unwrap_err().kind,
            MediaErrorKind::Unavailable
        );
    }

    #[tokio::test]
    async fn host_cleanup_fence_blocks_replacement_after_media_drain() {
        let shared = Arc::new(FakeService::default());
        let host = MediaAttachmentHost::new();
        let attachment = host.attach(shared.clone()).unwrap();
        let mut fence = attachment.retain_for_host_cleanup().unwrap();
        assert!(fence.confirm().is_err());
        assert_eq!(
            attachment.drain_media(Duration::from_secs(1)).await,
            MediaAttachmentRevocation::Confirmed
        );
        assert_eq!(attachment.phase(), MediaAttachmentPhase::Revoking);
        assert!(matches!(
            attachment.revoke(Duration::from_millis(5)).await,
            MediaAttachmentRevocation::Unknown { .. }
        ));
        assert!(host.attach(shared.clone()).is_err());
        fence.confirm().unwrap();
        assert_eq!(attachment.phase(), MediaAttachmentPhase::Revoked);
        assert!(host.attachment(attachment.id()).is_none());
        assert!(host.attach(shared).is_ok());
    }

    #[tokio::test]
    async fn media_drain_waits_for_registration_before_host_cleanup() {
        let shared = Arc::new(FakeService::default());
        let host = MediaAttachmentHost::new();
        let attachment = host.attach(shared.clone()).unwrap();
        let mut fence = attachment.retain_for_host_cleanup().unwrap();
        let permit = attachment.authorize_owner(&owner()).unwrap();
        let drain_handle = attachment.clone();
        let mut drain =
            tokio::spawn(async move { drain_handle.drain_media(Duration::from_secs(1)).await });
        attachment.wait_for_admission_closed().await;
        assert!(tokio::time::timeout(Duration::from_millis(10), &mut drain)
            .await
            .is_err());
        assert!(host.attach(shared.clone()).is_err());
        drop(permit);
        assert_eq!(drain.await.unwrap(), MediaAttachmentRevocation::Confirmed);
        assert!(
            lock(&shared.owners).is_empty(),
            "registration alone owes no media status"
        );
        assert_eq!(attachment.phase(), MediaAttachmentPhase::Revoking);
        assert!(
            host.attach(shared.clone()).is_err(),
            "route cleanup is still outstanding"
        );
        fence.confirm().unwrap();
        assert!(host.attach(shared).is_ok());
    }

    #[tokio::test]
    async fn every_independent_host_cleanup_must_confirm() {
        let shared = Arc::new(FakeService::default());
        let host = MediaAttachmentHost::new();
        let attachment = host.attach(shared.clone()).unwrap();
        let mut first = attachment.retain_for_host_cleanup().unwrap();
        let mut second = attachment.retain_for_host_cleanup().unwrap();
        assert_eq!(
            attachment.drain_media(Duration::from_secs(1)).await,
            MediaAttachmentRevocation::Confirmed
        );
        first.confirm().unwrap();
        first.confirm().unwrap();
        assert_eq!(attachment.phase(), MediaAttachmentPhase::Revoking);
        assert!(host.attach(shared.clone()).is_err());
        second.confirm().unwrap();
        assert_eq!(attachment.phase(), MediaAttachmentPhase::Revoked);
        assert!(host.attach(shared).is_ok());
    }

    #[tokio::test]
    async fn parent_drain_waits_for_child_host_cleanup_fence() {
        let shared = Arc::new(FakeService::default());
        let host = MediaAttachmentHost::new();
        let parent = host.attach(shared.clone()).unwrap();
        let child = parent.fork_child().unwrap();
        let mut child_fence = child.retain_for_host_cleanup().unwrap();
        assert!(matches!(
            parent.drain_media(Duration::from_millis(10)).await,
            MediaAttachmentRevocation::Unknown { .. }
        ));
        tokio::time::timeout(Duration::from_secs(1), async {
            while !child.media_drain_confirmed() {
                tokio::task::yield_now().await;
            }
        })
        .await
        .unwrap();
        assert_eq!(parent.phase(), MediaAttachmentPhase::Revoking);
        assert_eq!(child.phase(), MediaAttachmentPhase::Revoking);
        assert!(host.attach(shared).is_err());
        child_fence.confirm().unwrap();
        assert_eq!(
            parent.revoke(Duration::from_secs(1)).await,
            MediaAttachmentRevocation::Confirmed
        );
        assert_eq!(child.phase(), MediaAttachmentPhase::Revoked);
        assert!(host.attachments().is_empty());
    }

    #[tokio::test]
    async fn parent_host_cleanup_fence_survives_confirmed_child_drain() {
        let shared = Arc::new(FakeService::default());
        let host = MediaAttachmentHost::new();
        let parent = host.attach(shared.clone()).unwrap();
        let child = parent.fork_child().unwrap();
        let mut parent_fence = parent.retain_for_host_cleanup().unwrap();
        assert_eq!(
            parent.drain_media(Duration::from_secs(1)).await,
            MediaAttachmentRevocation::Confirmed
        );
        assert_eq!(child.phase(), MediaAttachmentPhase::Revoked);
        assert_eq!(parent.phase(), MediaAttachmentPhase::Revoking);
        assert!(host.attach(shared).is_err());
        parent_fence.confirm().unwrap();
        assert_eq!(parent.phase(), MediaAttachmentPhase::Revoked);
        assert!(host.attachments().is_empty());
    }

    #[tokio::test]
    async fn lost_host_cleanup_fence_never_claims_confirmation() {
        let shared = Arc::new(FakeService::default());
        let host = MediaAttachmentHost::new();
        let attachment = host.attach(shared.clone()).unwrap();
        let fence = attachment.retain_for_host_cleanup().unwrap();
        drop(fence);
        assert_eq!(
            attachment.drain_media(Duration::from_secs(1)).await,
            MediaAttachmentRevocation::Confirmed
        );
        assert_eq!(attachment.phase(), MediaAttachmentPhase::Revoking);
        assert!(host.attachment(attachment.id()).is_some());
        assert!(host.attach(shared).is_err());
    }

    #[tokio::test]
    async fn host_cleanup_fence_cannot_confirm_unknown_media_drain() {
        let shared = Arc::new(FakeService::default());
        let host = MediaAttachmentHost::new();
        let attachment = host.attach(shared.clone()).unwrap();
        let mut fence = attachment.retain_for_host_cleanup().unwrap();
        attachment
            .service()
            .execute(context(owner(), 35), capture())
            .await
            .unwrap();
        shared.fail_status.store(true, Ordering::SeqCst);
        assert!(matches!(
            attachment.drain_media(Duration::from_secs(1)).await,
            MediaAttachmentRevocation::Unknown { .. }
        ));
        assert!(fence.confirm().is_err());
        assert_eq!(attachment.phase(), MediaAttachmentPhase::Unknown);
        assert!(host.attach(shared).is_err());
    }

    #[tokio::test]
    async fn timed_out_revoke_keeps_single_flight_and_rejects_late_calls() {
        let shared = Arc::new(FakeService::default());
        shared.block_capture.store(true, Ordering::SeqCst);
        shared.block_stop.store(true, Ordering::SeqCst);
        let host = MediaAttachmentHost::new();
        let attachment = host.attach(shared.clone()).unwrap();
        let attachment_id = attachment.id().clone();
        let service = attachment.service();
        let capture_task =
            tokio::spawn(async move { service.execute(context(owner(), 10), capture()).await });
        shared.capture_started.notified().await;

        let first = attachment.revoke(Duration::from_millis(5)).await;
        assert!(matches!(first, MediaAttachmentRevocation::Unknown { .. }));
        assert_eq!(attachment.phase(), MediaAttachmentPhase::Revoking);
        assert_eq!(host.attachments().len(), 1);
        shared.stop_started.notified().await;
        assert_eq!(shared.stop_calls.load(Ordering::SeqCst), 1);
        let recovered = host.attachment(&attachment_id).unwrap();
        drop(attachment);
        assert_eq!(
            recovered
                .service()
                .execute(context(owner(), 11), capture())
                .await
                .unwrap_err()
                .kind,
            MediaErrorKind::Cancelled
        );

        // The same background flight remains responsible for settling the
        // accepted call; no second native stop flight is created.
        shared.stop_gate.notify_one();
        shared.capture_gate.notify_one();
        capture_task.await.unwrap().unwrap();
        assert_eq!(
            recovered.revoke(Duration::from_secs(1)).await,
            MediaAttachmentRevocation::Confirmed
        );
        assert_eq!(recovered.phase(), MediaAttachmentPhase::Revoked);
        assert!(host.attachments().is_empty());
    }

    #[tokio::test]
    async fn failed_close_stays_unknown_and_is_not_reissued() {
        let shared = Arc::new(FakeService::default());
        shared.block_capture.store(true, Ordering::SeqCst);
        shared.fail_stop.store(true, Ordering::SeqCst);
        let host = MediaAttachmentHost::new();
        let attachment = host.attach(shared.clone()).unwrap();
        let service = attachment.service();
        let capture_task =
            tokio::spawn(async move { service.execute(context(owner(), 20), capture()).await });
        shared.capture_started.notified().await;

        let first = attachment.revoke(Duration::from_secs(1)).await;
        assert!(matches!(first, MediaAttachmentRevocation::Unknown { .. }));
        assert_eq!(attachment.phase(), MediaAttachmentPhase::Unknown);
        assert_eq!(host.attachments().len(), 1);
        assert_eq!(shared.stop_calls.load(Ordering::SeqCst), 1);
        assert_eq!(
            host.attach(Arc::new(FakeService::default()))
                .unwrap_err()
                .kind,
            MediaErrorKind::Unavailable
        );
        let recovered = host.attachment(attachment.id()).unwrap();
        assert_eq!(recovered.phase(), MediaAttachmentPhase::Unknown);
        assert!(matches!(
            recovered.revoke(Duration::from_secs(1)).await,
            MediaAttachmentRevocation::Unknown { .. }
        ));
        assert_eq!(shared.stop_calls.load(Ordering::SeqCst), 1);
        assert_eq!(
            attachment
                .service()
                .execute(context(owner(), 21), capture())
                .await
                .unwrap_err()
                .kind,
            MediaErrorKind::Cancelled
        );
        shared.capture_gate.notify_one();
        let _ = capture_task.await;
    }

    #[tokio::test]
    async fn drain_stop_shares_the_active_scoped_owner_fence() {
        let shared = Arc::new(FakeService::default());
        let host = MediaAttachmentHost::new();
        let attachment = host.attach(shared.clone()).unwrap();
        let scoped_owner = owner().in_attachment(attachment.id().clone());
        let target = MediaResourceRef::Recording(media_api::RecordingRef(
            media_api::RecordingId::from("stop-fence-race"),
        ));
        let in_flight = attachment
            .service
            .begin_stop_attempt(&scoped_owner, &target)
            .unwrap();

        let error = attachment
            .service
            .stop(&scoped_owner, target)
            .await
            .unwrap_err();
        assert_eq!(error.kind, MediaErrorKind::Busy);
        assert_eq!(shared.stop_calls.load(Ordering::SeqCst), 0);
        drop(in_flight);
    }

    #[tokio::test]
    async fn failed_active_stop_is_not_reissued_by_the_first_revoke_drain() {
        let shared = Arc::new(FakeService::default());
        let host = MediaAttachmentHost::new();
        let attachment = host.attach(shared.clone()).unwrap();
        let logical_owner = owner();
        let scoped_owner = logical_owner.clone().in_attachment(attachment.id().clone());
        let target = MediaResourceRef::Recording(media_api::RecordingRef(
            media_api::RecordingId::from("recording-before-revoke"),
        ));
        lock(&shared.active).insert(scoped_owner, target.clone());
        shared.fail_stop.store(true, Ordering::SeqCst);

        let failed_stop = attachment
            .service()
            .execute(
                context(logical_owner, 25),
                MediaOperation::Stop {
                    target: target.clone(),
                },
            )
            .await
            .unwrap_err();
        assert_eq!(failed_stop.kind, MediaErrorKind::NativeFailure);
        assert_eq!(shared.stop_calls.load(Ordering::SeqCst), 1);

        // The first error had no completion receipt. Even if a retry could now
        // return success, revoke must retain Unknown and never send a second Stop.
        shared.fail_stop.store(false, Ordering::SeqCst);
        assert!(matches!(
            attachment.revoke(Duration::from_secs(1)).await,
            MediaAttachmentRevocation::Unknown { .. }
        ));
        assert_eq!(shared.stop_calls.load(Ordering::SeqCst), 1);
        assert_eq!(attachment.phase(), MediaAttachmentPhase::Unknown);
        assert!(host.attachment(attachment.id()).is_some());
    }

    #[tokio::test]
    async fn drain_ignores_only_its_own_status_operation_and_stops_other_active_work() {
        let shared = Arc::new(FakeService::default());
        shared.block_capture.store(true, Ordering::SeqCst);
        shared
            .include_status_operation
            .store(true, Ordering::SeqCst);
        let attachment = MediaAttachmentHost::new().attach(shared.clone()).unwrap();
        let service = attachment.service();
        let capture_task =
            tokio::spawn(async move { service.execute(context(owner(), 44), capture()).await });
        shared.capture_started.notified().await;

        assert_eq!(
            attachment.revoke(Duration::from_secs(1)).await,
            MediaAttachmentRevocation::Confirmed
        );
        assert_eq!(shared.stop_calls.load(Ordering::SeqCst), 1);
        assert!(matches!(
            capture_task.await.unwrap(),
            Ok(MediaResult::Captured { .. })
        ));
    }

    #[tokio::test]
    async fn a_pending_session_audio_cancel_cannot_cancel_another_attachment_operation() {
        let shared = Arc::new(FakeService::default());
        shared.block_capture.store(true, Ordering::SeqCst);
        shared.block_session_audio.store(true, Ordering::SeqCst);
        let host = MediaAttachmentHost::new();
        let attachment_a = host.attach(shared.clone()).unwrap();
        let attachment_b = host.attach(shared.clone()).unwrap();
        let identity = MediaOperationId::new(45, 11);
        let owner_a = owner();
        let owner_b = owner();

        let service_b = attachment_b.service();
        let capture_context = MediaOperationContext::new(identity.clone(), owner_b.clone());
        let capture_task =
            tokio::spawn(async move { service_b.execute(capture_context, capture()).await });
        shared.capture_started.notified().await;

        let service_a = attachment_a.service();
        let session_audio_context = MediaOperationContext::new(identity.clone(), owner_a.clone());
        let cancel_context = session_audio_context.clone();
        let session_audio_task = tokio::spawn(async move {
            service_a
                .session_audio(
                    session_audio_context,
                    media_api::MediaSessionRef(media_api::MediaSessionId::from("session-a")),
                )
                .await
        });
        shared.session_audio_started.notified().await;

        assert_eq!(
            attachment_a
                .service()
                .cancel(cancel_context.clone())
                .await
                .unwrap_err()
                .kind,
            MediaErrorKind::Busy
        );
        let forwarded = lock(&shared.cancel_contexts);
        assert_eq!(forwarded.len(), 1);
        assert_eq!(forwarded[0].identity, identity);
        assert_eq!(
            forwarded[0].owner,
            owner().in_attachment(attachment_a.id().clone())
        );
        drop(forwarded);
        assert!(!lock(&shared.cancelled_operations).contains(&identity));

        shared.session_audio_gate.notify_one();
        match session_audio_task.await.unwrap() {
            Err(error) => assert_eq!(error.kind, MediaErrorKind::Unsupported),
            Ok(_) => panic!("fake session audio should remain unsupported"),
        }
        shared.capture_gate.notify_one();
        assert!(matches!(
            capture_task.await.unwrap(),
            Ok(MediaResult::Captured { .. })
        ));
        assert_ne!(attachment_a.id(), attachment_b.id());
    }

    #[tokio::test]
    async fn cancelling_a_shared_identity_remains_available_until_the_last_call_returns() {
        let shared = Arc::new(FakeService::default());
        shared.block_session_audio.store(true, Ordering::SeqCst);
        let host = MediaAttachmentHost::new();
        let attachment = host.attach(shared.clone()).unwrap();
        let service = attachment.service();
        let identity = MediaOperationId::new(46, 11);
        let context = MediaOperationContext::new(identity.clone(), owner());
        let session = media_api::MediaSessionRef(media_api::MediaSessionId::from("session-a"));

        let first_service = service.clone();
        let first_context = context.clone();
        let first_session = session.clone();
        let mut first = tokio::spawn(async move {
            first_service
                .session_audio(first_context, first_session)
                .await
        });
        tokio::time::timeout(
            Duration::from_secs(1),
            shared.session_audio_started.notified(),
        )
        .await
        .expect("first call must start");

        let second_service = service.clone();
        let second_context = context.clone();
        let mut second =
            tokio::spawn(
                async move { second_service.session_audio(second_context, session).await },
            );
        tokio::time::timeout(
            Duration::from_secs(1),
            shared.session_audio_started.notified(),
        )
        .await
        .expect("second call must start");

        shared.session_audio_gate.notify_one();
        let (first_finished, completed) = tokio::time::timeout(Duration::from_secs(1), async {
            tokio::select! {
                result = &mut first => (true, result),
                result = &mut second => (false, result),
            }
        })
        .await
        .expect("one call must return");
        assert!(completed.unwrap().is_err());
        assert_eq!(
            service.cancel(context.clone()).await.unwrap_err().kind,
            MediaErrorKind::Busy
        );
        assert_eq!(lock(&shared.cancel_contexts).len(), 1);

        shared.session_audio_gate.notify_one();
        let remaining = if first_finished {
            second.await
        } else {
            first.await
        };
        assert!(remaining.unwrap().is_err());
        service.cancel(context).await.unwrap();
        assert_eq!(lock(&shared.cancel_contexts).len(), 1);
    }

    #[tokio::test]
    async fn cancellation_waits_for_live_call_and_rechecks_after_forwarding() {
        let shared = Arc::new(FakeService::default());
        shared.block_session_audio.store(true, Ordering::SeqCst);
        shared.block_cancel.store(true, Ordering::SeqCst);
        let host = MediaAttachmentHost::new();
        let attachment = host.attach(shared.clone()).unwrap();
        let service = attachment.service();
        let context = MediaOperationContext::new(MediaOperationId::new(46, 11), owner());
        let task_service = service.clone();
        let task_context = context.clone();
        let call = tokio::spawn(async move {
            task_service
                .session_audio(
                    task_context,
                    media_api::MediaSessionRef(media_api::MediaSessionId::from("session-a")),
                )
                .await
        });
        shared.session_audio_started.notified().await;
        let cancel_service = service.clone();
        let cancel = tokio::spawn(async move { cancel_service.cancel(context).await });
        shared.cancel_started.notified().await;
        shared.session_audio_gate.notify_one();
        assert!(call.await.unwrap().is_err());
        shared.cancel_gate.notify_one();
        assert!(cancel.await.unwrap().is_ok());
    }

    #[tokio::test]
    async fn dropped_operation_cannot_turn_missing_cancel_into_a_receipt() {
        let shared = Arc::new(FakeService::default());
        shared.block_session_audio.store(true, Ordering::SeqCst);
        let host = MediaAttachmentHost::new();
        let attachment = host.attach(shared.clone()).unwrap();
        let service = attachment.service();
        let context = MediaOperationContext::new(MediaOperationId::new(47, 11), owner());
        let task_service = service.clone();
        let task_context = context.clone();
        let task = tokio::spawn(async move {
            task_service
                .session_audio(
                    task_context,
                    media_api::MediaSessionRef(media_api::MediaSessionId::from("session-a")),
                )
                .await
        });
        shared.session_audio_started.notified().await;
        task.abort();
        assert!(matches!(task.await, Err(error) if error.is_cancelled()));
        assert_eq!(
            attachment.operation_cleanup(&context),
            MediaOperationCleanup::Unknown
        );
        assert_eq!(
            service.cancel(context).await.unwrap_err().kind,
            MediaErrorKind::Busy
        );
        assert!(matches!(
            attachment.drain_media(Duration::from_secs(1)).await,
            MediaAttachmentRevocation::Unknown { .. }
        ));
        assert_eq!(attachment.phase(), MediaAttachmentPhase::Unknown);
        assert!(host.attach(shared).is_err());
    }

    #[tokio::test]
    async fn dropped_operation_with_native_witness_waits_for_confirmation() {
        let host = MediaAttachmentHost::new();
        let attachment = host.attach(Arc::new(FakeService::default())).unwrap();
        let mut context = MediaOperationContext::new(MediaOperationId::new(48, 11), owner());
        let (mut permit, scoped_owner) = attachment
            .service
            .enter(
                Some(&context.owner),
                Some(&context.identity),
                Some(context.operation_incarnation()),
            )
            .unwrap();
        context.mark_attachment_admitted();
        context.owner = scoped_owner.unwrap();
        context = attachment.service.bind_native_tracker(context, &permit);
        permit.begin_dispatch();
        let work = context.begin_native_work().unwrap().unwrap();
        drop(permit);
        assert_eq!(
            attachment.operation_cleanup(&context),
            MediaOperationCleanup::Pending
        );
        assert!(matches!(
            attachment.drain_media(Duration::from_millis(10)).await,
            MediaAttachmentRevocation::Unknown { .. }
        ));
        assert_eq!(attachment.phase(), MediaAttachmentPhase::Revoking);
        work.confirm();
        assert_eq!(
            attachment.drain_media(Duration::from_secs(1)).await,
            MediaAttachmentRevocation::Confirmed
        );
        assert_eq!(attachment.phase(), MediaAttachmentPhase::Revoked);
    }
}
