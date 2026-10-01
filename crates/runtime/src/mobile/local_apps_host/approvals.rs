use super::manifest_migration_reason;
use super::raise_decision;
use super::BridgeFailure;
use super::LocalAppsHostBroker;
use super::PendingNativeApproval;
use super::UiResolution;
use super::APPROVAL_TIMEOUT;
use local_app_contracts::approvals::{AuthorizationDecision, CapabilityKind, CapabilityRequest};
use local_app_contracts::events::PluginErrorCode;
use local_app_service::host::HostEvent;
use local_apps::load_manifest;
use local_apps::load_permissions;
use local_apps::save_permissions;
use local_apps::AppCapability;
use local_apps::AppPermissions;
use local_apps::DataMigrationPreview;
use local_apps::PermissionDecision;
use std::collections::HashMap;
use tokio::sync::oneshot;
use tokio::sync::Mutex;
use tokio::time::timeout;
use tokio::time::Duration;

impl LocalAppsHostBroker {
    pub(crate) async fn reset_permissions(&self, app_id: &str) -> Result<(), String> {
        self.service()?
            .record(app_id)
            .await
            .map_err(|error| error.to_string())?;
        let layout = self.layout(app_id)?;
        let current = load_permissions(&layout).map_err(|error| error.to_string())?;
        let mut reset = AppPermissions::default();
        reset.grant_epoch = current.grant_epoch.saturating_add(1).max(1);
        save_permissions(&layout, &reset).map_err(|error| error.to_string())?;
        self.session_permissions.lock().await.revoke_app(app_id);
        for outcome in self
            .cancel_background_tasks_for_revoked_schedule(
                app_id,
                "background scheduling permission was revoked",
            )
            .await?
        {
            self.emit_background_task_changed(&outcome).await;
        }
        Ok(())
    }
    pub(super) async fn wait_for_native_approval(
        &self,
        pending: &Mutex<HashMap<String, PendingNativeApproval>>,
        request_id: String,
        app_id: &str,
        event: HostEvent,
    ) -> Result<bool, String> {
        self.wait_for_native_approval_with_timeout(
            pending,
            request_id,
            app_id,
            event,
            APPROVAL_TIMEOUT,
        )
        .await
    }
    /// Same as [`Self::wait_for_native_approval`] with an injectable deadline.
    /// Production always goes through the wrapper above, which fixes it at
    /// `APPROVAL_TIMEOUT`; tests call this directly with a short duration so
    /// the `Err(_)` timeout arm (r4-tests-honesty-05) does not cost the suite
    /// five real minutes.
    pub(super) async fn wait_for_native_approval_with_timeout(
        &self,
        pending: &Mutex<HashMap<String, PendingNativeApproval>>,
        request_id: String,
        app_id: &str,
        event: HostEvent,
        deadline: Duration,
    ) -> Result<bool, String> {
        let (sender, receiver) = oneshot::channel();
        {
            let mut requests = pending.lock().await;
            // r1-backlog-native-confirmation-04: a caller that drops this
            // future mid-await (task cancellation, request abandonment) skips
            // every arm below, so the entry it inserted here would otherwise
            // never be removed — its `sender` is dead (the paired `receiver`
            // dropped with the future) but the map still holds it forever,
            // and the duplicate guard below would then refuse every later
            // approval for this app. Prune dead entries first so a stale one
            // never blocks a fresh request.
            requests.retain(|_, request| !request.sender.is_closed());
            if requests.values().any(|request| request.app_id == app_id) {
                return Err(
                    "approval_pending: this Local App already has a pending approval".into(),
                );
            }
            requests.insert(
                request_id.clone(),
                PendingNativeApproval {
                    app_id: app_id.to_string(),
                    sender,
                    event: event.clone(),
                },
            );
        }
        self.event_sink.emit(event).await;
        match timeout(deadline, receiver).await {
            Ok(Ok(approved)) => Ok(approved),
            Ok(Err(_)) => {
                pending.lock().await.remove(&request_id);
                let message = "native Local App approval was cancelled".to_string();
                // r1-failure-paths-002: proactively retract the native sheet on
                // every client. Clients discard their pending approval keyed on
                // `request_id` (e.g. iOS's `discardPendingApproval`), so a bare
                // `Err` here left a stale sheet on screen indefinitely — the
                // caller only learns the workflow failed, never the client.
                //
                // ⚠️ KNOWN-WRONG `code`, deliberately left as-is:
                // `PluginErrorCode` (client-protocol/src/local_apps.rs)
                // has NO cancelled/aborted/timed-out member, and `code` is not
                // optional. Android DOES render it —
                // `LocalAppsViewModel.localizedPluginError` maps
                // `PROPOSAL_INVALID` to `local_apps_error_proposal_invalid`
                // ("MCP 提案未通过校验，需要继续修改。"), which is wrong copy for a
                // cancelled/timed-out sheet and doubly wrong on the CREATE
                // confirmation path below (`wait_for_native_approval` also serves
                // `CreateConfirmationRequested`, which has no MCP proposal at
                // all). iOS ignores `code` and shows `message`, so iOS is correct
                // already; the dismissal itself keys on `request_id` on BOTH
                // platforms and works regardless of `code`.
                // Fixing the copy is a four-file, cross-platform change this
                // module cannot land alone: append (never insert — UniFFI encodes
                // by declaration ordinal) an `ApprovalAborted` member at the END
                // of `PluginErrorCode`; add its string to the five
                // `clients/translations/*.json` sources and regenerate the iOS
                // `.xcstrings` / Android `strings.xml` catalogs; add the arm to
                // `LocalAppsViewModel.localizedPluginError`; then emit it here and
                // in the timeout arm below.
                self.event_sink
                    .emit(HostEvent::PluginOperationFailed {
                        app_id: Some(app_id.to_string()),
                        code: PluginErrorCode::ProposalInvalid,
                        message: message.clone(),
                        request_id: Some(request_id.clone()),
                    })
                    .await;
                Err(message)
            }
            Err(_) => {
                pending.lock().await.remove(&request_id);
                let message = "native Local App approval timed out".to_string();
                // r1-failure-paths-002: same retraction as the cancelled arm
                // above, for the timeout arm — including its KNOWN-WRONG `code`
                // and the four-file fix that would correct it.
                self.event_sink
                    .emit(HostEvent::PluginOperationFailed {
                        app_id: Some(app_id.to_string()),
                        code: PluginErrorCode::ProposalInvalid,
                        message: message.clone(),
                        request_id: Some(request_id.clone()),
                    })
                    .await;
                Err(message)
            }
        }
    }
    pub(crate) async fn resolve_capability(
        &self,
        request_id: &str,
        decision: AuthorizationDecision,
    ) -> bool {
        self.pending_capabilities
            .lock()
            .await
            .remove(request_id)
            .is_some_and(|sender| sender.send(decision).is_ok())
    }
    /// Resolve one native dependency-change confirmation request.  This is a
    /// separate one-shot channel from generic capability approvals so the
    /// package diff and supply-chain policy shown by the client cannot be
    /// replaced by a generic allow/deny response.
    pub(crate) async fn resolve_dependency_change_confirmation(
        &self,
        request_id: &str,
        approved: bool,
    ) -> bool {
        self.pending_dependency_change_confirmations
            .lock()
            .await
            .remove(request_id)
            .is_some_and(|sender| sender.send(approved).is_ok())
    }
    pub(crate) async fn resolve_create_confirmation(
        &self,
        request_id: &str,
        approved: bool,
    ) -> bool {
        Self::resolve_native_approval(&self.pending_create_confirmations, request_id, approved)
            .await
    }
    pub(crate) async fn resolve_mcp_proposal_approval(
        &self,
        request_id: &str,
        approved: bool,
    ) -> bool {
        Self::resolve_native_approval(&self.pending_mcp_proposal_approvals, request_id, approved)
            .await
    }
    /// Re-announce every native approval this broker is still blocked on.
    ///
    /// r3-failure-paths-02: the reattach path. `PluginCommandDto::
    /// GetManagedMcpInventory` is the snapshot command both clients already
    /// send when they (re)bind — Android's `LocalAppsViewModel.
    /// requestSnapshots`, iOS's `refreshManagedMcpInventory` — so the pending
    /// sheets ride the channel that already exists rather than a new command
    /// this lane cannot add to the wire.
    ///
    /// Every event carries the SAME `request_id` as the original emission, so a
    /// client that never lost the sheet just re-renders the one it has (both
    /// clients key their pending approval on `request_id`), and a client that
    /// did lose it gets it back and can still answer it.
    ///
    /// Dead entries are pruned first for the same reason
    /// `wait_for_native_approval_with_timeout` prunes them: a waiter whose
    /// future was dropped leaves a closed sender behind, and re-emitting a
    /// sheet nobody is listening for would strand the user in front of a
    /// prompt whose answer goes nowhere.
    pub(crate) async fn reemit_pending_native_approvals(&self) {
        let mut events = Vec::new();
        for pending in [
            &self.pending_create_confirmations,
            &self.pending_mcp_proposal_approvals,
        ] {
            let mut requests = pending.lock().await;
            requests.retain(|_, request| !request.sender.is_closed());
            events.extend(requests.values().map(|request| request.event.clone()));
        }
        for event in events {
            self.event_sink.emit(event).await;
        }
    }
    pub(super) async fn resolve_native_approval(
        pending: &Mutex<HashMap<String, PendingNativeApproval>>,
        request_id: &str,
        approved: bool,
    ) -> bool {
        pending
            .lock()
            .await
            .remove(request_id)
            .is_some_and(|request| request.sender.send(approved).is_ok())
    }
    pub(crate) async fn resolve_ui(
        &self,
        request_id: &str,
        decision: AuthorizationDecision,
        result_json: Option<String>,
        error: Option<String>,
    ) -> bool {
        self.pending_ui
            .lock()
            .await
            .remove(request_id)
            .is_some_and(|sender| {
                sender
                    .send(UiResolution {
                        decision,
                        result_json,
                        error,
                    })
                    .is_ok()
            })
    }
    pub(super) async fn request_capability(
        &self,
        app_id: &str,
        capability: CapabilityKind,
        domain: Option<String>,
        reason: &str,
    ) -> Result<AuthorizationDecision, String> {
        let request_id = self.request_id("app-capability");
        let (sender, receiver) = oneshot::channel();
        self.pending_capabilities
            .lock()
            .await
            .insert(request_id.clone(), sender);
        self.event_sink
            .emit(HostEvent::CapabilityRequested(CapabilityRequest {
                request_id: request_id.clone(),
                app_id: app_id.to_string(),
                capability,
                domain,
                reason: reason.to_string(),
            }))
            .await;
        match timeout(APPROVAL_TIMEOUT, receiver).await {
            Ok(Ok(decision)) => Ok(decision),
            Ok(Err(_)) => Err("capability request was cancelled".into()),
            Err(_) => {
                self.pending_capabilities.lock().await.remove(&request_id);
                Err("capability request timed out".into())
            }
        }
    }
    pub(super) async fn authorize_capability(
        &self,
        app_id: &str,
        capability: AppCapability,
        wire_capability: CapabilityKind,
        reason: &str,
    ) -> Result<(), String> {
        let layout = self.layout(app_id)?;
        let persisted = load_permissions(&layout).map_err(|error| error.to_string())?;
        if persisted.allows(capability)
            || self
                .session_permissions
                .lock()
                .await
                .allows(app_id, capability)
        {
            return Ok(());
        }
        let decision = self
            .request_capability(app_id, wire_capability, None, reason)
            .await?;
        match raise_decision(decision) {
            PermissionDecision::Deny => Err(Self::DENIED_CAPABILITY_MESSAGE.into()),
            PermissionDecision::AllowOnce => Ok(()),
            PermissionDecision::AllowSession => {
                self.session_permissions
                    .lock()
                    .await
                    .grant(app_id, capability);
                Ok(())
            }
            PermissionDecision::AlwaysAllow => {
                let mut permissions = persisted;
                permissions.grant(capability);
                save_permissions(&layout, &permissions).map_err(|error| error.to_string())
            }
        }
    }
    /// Manifest-only gate for read-only capabilities that do not need a
    /// separate user prompt. The declaration is still required so a generated
    /// app cannot silently discover host state it did not request.
    pub(super) fn ensure_declared_capability(
        &self,
        app_id: &str,
        capability: AppCapability,
    ) -> Result<(), BridgeFailure> {
        let layout = self.layout(app_id)?;
        let manifest = load_manifest(&layout).map_err(|error| error.to_string())?;
        if !manifest.capabilities.contains(&capability) {
            return Err(BridgeFailure::coded(
                "capability_not_declared",
                format!("capability {capability:?} is not declared in the app manifest"),
            ));
        }
        Ok(())
    }
    /// Declared-then-prompt gate shared by every plan-declared capability
    /// (device, llm, agent_notify): an app may only ever be ASKED about a
    /// capability its confirmed plan declared. An undeclared capability fails
    /// typed (`capability_not_declared`) WITHOUT raising a prompt — the same
    /// manifest-first contract [`Self::authorize_domain`] applies to network
    /// hosts. Declared capabilities then ride the existing persisted →
    /// session → prompt ladder unchanged.
    pub(super) async fn authorize_declared_capability(
        &self,
        app_id: &str,
        capability: AppCapability,
        wire_capability: CapabilityKind,
        reason: &str,
    ) -> Result<(), BridgeFailure> {
        let layout = self.layout(app_id)?;
        let manifest = load_manifest(&layout).map_err(|error| error.to_string())?;
        if !manifest.capabilities.contains(&capability) {
            return Err(BridgeFailure::coded(
                "capability_not_declared",
                format!("capability {capability:?} is not declared in the app manifest"),
            ));
        }
        self.authorize_capability(app_id, capability, wire_capability, reason)
            .await
            .map_err(|message| {
                if message == Self::DENIED_CAPABILITY_MESSAGE {
                    BridgeFailure::coded("permission_denied", message)
                } else {
                    BridgeFailure::from(message)
                }
            })
    }
    pub(super) async fn authorize_domain(&self, app_id: &str, domain: &str) -> Result<(), String> {
        let layout = self.layout(app_id)?;
        let manifest = load_manifest(&layout).map_err(|error| error.to_string())?;
        if !manifest
            .allowed_domains
            .iter()
            .any(|allowed| allowed == domain)
        {
            return Err(format!(
                "HTTPS domain {domain:?} is not declared in the app manifest"
            ));
        }
        let persisted = load_permissions(&layout).map_err(|error| error.to_string())?;
        if persisted.allows_domain(domain)
            || self
                .session_permissions
                .lock()
                .await
                .allows_domain(app_id, domain)
        {
            return Ok(());
        }
        let decision = self
            .request_capability(
                app_id,
                CapabilityKind::NetworkDomain,
                Some(domain.to_string()),
                "The local app requested first-time access to this HTTPS domain.",
            )
            .await?;
        match raise_decision(decision) {
            PermissionDecision::Deny => Err("user denied access to the network domain".into()),
            PermissionDecision::AllowOnce => Ok(()),
            PermissionDecision::AllowSession => self
                .session_permissions
                .lock()
                .await
                .grant_domain(app_id, domain)
                .map_err(|error| error.to_string()),
            PermissionDecision::AlwaysAllow => {
                let mut permissions = persisted;
                permissions
                    .grant_domain(domain)
                    .map_err(|error| error.to_string())?;
                save_permissions(&layout, &permissions).map_err(|error| error.to_string())
            }
        }
    }
    pub(crate) async fn approve_destructive_manifest_migration(
        &self,
        app_id: &str,
        preview: &DataMigrationPreview,
    ) -> Result<(), String> {
        if !preview.destructive {
            return Ok(());
        }
        let decision = self
            .request_capability(
                app_id,
                CapabilityKind::DataMutation,
                None,
                &manifest_migration_reason(preview),
            )
            .await?;
        if matches!(raise_decision(decision), PermissionDecision::Deny) {
            return Err("user denied destructive manifest migration".into());
        }
        Ok(())
    }
}
