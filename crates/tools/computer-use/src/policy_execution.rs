use super::{device_policy, ComputerTool, ToolError, ToolUseContext};

impl ComputerTool {
    pub(crate) async fn revoke_os_permissions(
        &self,
        ctx: &ToolUseContext,
    ) -> Result<(), ToolError> {
        self.os_permissions_revoked
            .store(true, std::sync::atomic::Ordering::SeqCst);
        self.state
            .lock()
            .map_err(|_| ToolError::Internal("computer state poisoned".into()))?
            .revoke_grants();
        {
            let mut runtime = self
                .runtime
                .lock()
                .map_err(|_| ToolError::Internal("computer state poisoned".into()))?;
            runtime.frames.clear();
            runtime.snapshots.clear();
            runtime.pending_observations.clear();
        }
        // Cleanup must remain unconditional after revocation, including when a
        // mouse button or modifier is down at the instant the OS state changes.
        self.release_owned_inputs(ctx).await
    }

    pub(crate) async fn read_os_permissions(
        &self,
        ctx: &ToolUseContext,
    ) -> Result<Option<(bool, bool)>, ToolError> {
        if let Some(backend) = &self.ctx.computer_control {
            match backend.query_os_permissions().await {
                Ok(state) => return Ok(state),
                Err(error) => {
                    // A busy or unavailable AX service blocks this operation,
                    // but is not evidence that the user revoked permission.
                    {
                        let mut runtime = self
                            .runtime
                            .lock()
                            .map_err(|_| ToolError::Internal("computer state poisoned".into()))?;
                        runtime.frames.clear();
                        runtime.snapshots.clear();
                        runtime.pending_observations.clear();
                    }
                    self.release_owned_inputs(ctx).await?;
                    return Err(super::map_err(&error));
                }
            }
        }
        Ok(None)
    }

    pub(crate) async fn check_current_os_permissions(
        &self,
        ctx: &ToolUseContext,
    ) -> Result<(), ToolError> {
        if self
            .read_os_permissions(ctx)
            .await?
            .is_some_and(|(accessibility, recording)| !accessibility || !recording)
        {
            self.revoke_os_permissions(ctx).await?;
            return Err(ToolError::PermissionDenied("macOS Accessibility / Screen Recording access was revoked. Restore it in System Settings, then call request_access again.".into()));
        }
        Ok(())
    }

    pub(crate) async fn check_live_os_permissions(
        &self,
        ctx: &ToolUseContext,
    ) -> Result<(), ToolError> {
        self.check_current_os_permissions(ctx).await?;
        if self
            .os_permissions_revoked
            .load(std::sync::atomic::Ordering::SeqCst)
        {
            return Err(ToolError::PermissionDenied(
                "OS permissions changed. Call request_access again to select applications.".into(),
            ));
        }
        Ok(())
    }
    pub(crate) async fn pin_pointer_input(
        &self,
        action: &str,
        input: &lingxi_core::types::utf16_json::Utf16JsonProjection,
    ) -> Result<Option<lingxi_core::types::utf16_json::Utf16JsonProjection>, ToolError> {
        let Some(backend) = self
            .ctx
            .computer_control
            .as_ref()
            .filter(|backend| backend.supports_target_application())
        else {
            return Ok(None);
        };
        if !matches!(
            action,
            "mouse_move"
                | "mouse_click"
                | "left_click"
                | "right_click"
                | "middle_click"
                | "double_click"
                | "triple_click"
                | "scroll"
                | "left_click_drag"
                | "left_mouse_down"
                | "left_mouse_up"
        ) {
            return Ok(None);
        }
        if super::extensions::path(input)?.is_some() {
            return Ok(None);
        }
        let current = input
            .get_property("use_current_cursor")
            .and_then(|value| value.as_bool())
            == Some(true)
            || matches!(action, "left_mouse_down" | "left_mouse_up")
                && super::coord(input, "coordinate").is_none();
        let drag_start =
            action == "left_click_drag" && super::coord(input, "start_coordinate").is_none();
        if !current && !drag_start {
            return Ok(None);
        }
        let (x, y) = backend
            .cursor_position()
            .await
            .map_err(|error| super::map_err(&error))?;
        let mut pinned = input.clone();
        let point = lingxi_core::types::utf16_json::Utf16JsonProjection::from_serde_value(
            &serde_json::json!([x, y]),
        )
        .map_err(|_| ToolError::Internal("cannot pin computer coordinates".into()))?;
        pinned
            .set_field(
                if current {
                    "coordinate"
                } else {
                    "start_coordinate"
                },
                point,
            )
            .map_err(|_| ToolError::Internal("cannot pin computer coordinates".into()))?;
        if current {
            let disabled = lingxi_core::types::utf16_json::Utf16JsonProjection::from_serde_value(
                &serde_json::json!(false),
            )
            .map_err(|_| ToolError::Internal("cannot pin computer coordinates".into()))?;
            pinned
                .set_field("use_current_cursor", disabled)
                .map_err(|_| ToolError::Internal("cannot pin computer coordinates".into()))?;
        }
        Ok(Some(pinned))
    }
    /// Revalidate immediately before each effect in a compound operation.
    /// Release operations deliberately bypass this guard so revocation cannot strand input.
    pub(crate) async fn guard_effect(
        &self,
        action: &str,
        input: &lingxi_core::types::utf16_json::Utf16JsonProjection,
        required_action: &str,
        point: Option<(u32, u32)>,
        ctx: &ToolUseContext,
        revision: Option<&str>,
    ) -> Result<(), ToolError> {
        self.check_cancel(ctx)?;
        self.check_policy_revision(revision)?;
        if !matches!(action, "key_up" | "left_mouse_up") {
            self.check_live_os_permissions(ctx).await?;
        }
        let backend = self.ctx.computer_control.as_ref().ok_or_else(|| {
            ToolError::PermissionDenied("Computer control is unavailable.".into())
        })?;
        if backend
            .screen_locked()
            .await
            .map_err(|error| super::map_err(&error))?
        {
            return Err(ToolError::PermissionDenied("The screen is locked.".into()));
        }
        if backend.supports_target_application()
            && matches!(
                action,
                "mouse_move"
                    | "mouse_click"
                    | "left_click"
                    | "right_click"
                    | "middle_click"
                    | "double_click"
                    | "triple_click"
                    | "scroll"
                    | "left_click_drag"
                    | "left_mouse_down"
                    | "left_mouse_up"
            )
        {
            let single = point
                .map(|(x, y)| {
                    lingxi_core::types::utf16_json::Utf16JsonProjection::from_serde_value(
                        &serde_json::json!({"coordinate":[x,y]}),
                    )
                    .map_err(|_| ToolError::Internal("cannot validate input point".into()))
                })
                .transpose()?;
            self.enforce_target(
                single.as_ref().unwrap_or(input),
                required_action,
                ctx,
                revision,
            )
            .await?;
        } else {
            self.enforce_tier(required_action).await?;
        }
        self.check_cancel(ctx)?;
        self.check_policy_revision(revision)?;
        if !matches!(action, "key_up" | "left_mouse_up") {
            self.check_live_os_permissions(ctx).await?;
        }
        self.check_cancel(ctx)?;
        self.check_policy_revision(revision)
    }
    pub(crate) fn allowed_bundle_ids(&self) -> Result<Vec<String>, ToolError> {
        Ok(self
            .state
            .lock()
            .map_err(|_| ToolError::Internal("computer state poisoned".into()))?
            .allowed_apps
            .iter()
            .map(|app| app.bundle_id.clone())
            .collect())
    }

    pub(crate) async fn enforce_target(
        &self,
        input: &lingxi_core::types::utf16_json::Utf16JsonProjection,
        action: &str,
        ctx: &ToolUseContext,
        revision: Option<&str>,
    ) -> Result<Option<lingxi_core::host::computer_control::AppInfo>, ToolError> {
        let backend = self.ctx.computer_control.as_ref().ok_or_else(|| {
            ToolError::PermissionDenied("Computer control is unavailable.".into())
        })?;
        let mut points = if let Some(path) = super::extensions::path(input)? {
            path
        } else {
            vec![self.target_coordinate(input).await?]
        };
        if let Some(point) = super::coord(input, "start_coordinate") {
            points.push(point);
        }
        let required = if matches!(
            action,
            "key" | "right_click" | "middle_click" | "mouse_click"
        ) {
            super::AppTier::Full
        } else {
            super::AppTier::Click
        };
        let policy = self.policy_snapshot()?;
        let mut target = None;
        for (x, y) in points {
            self.check_cancel(ctx)?;
            self.check_policy_revision(revision)?;
            let app = backend.target_application(x,y).await.map_err(|error| super::map_err(&error))?
                .ok_or_else(|| ToolError::PermissionDenied("Could not determine the application at the target location. Take a fresh screenshot.".into()))?;
            self.check_cancel(ctx)?;
            self.check_policy_revision(revision)?;
            if super::app_policy::blocked(Some(&app.bundle_id), &app.display_name)
                || policy
                    .as_ref()
                    .is_some_and(|(_, p)| p.denies(&app.bundle_id, &app.display_name))
            {
                return Err(ToolError::PermissionDenied(
                    "The target application is blocked by computer access policy.".into(),
                ));
            }
            let state = self
                .state
                .lock()
                .map_err(|_| ToolError::Internal("computer state poisoned".into()))?;
            if state
                .tier_for(&app.bundle_id)
                .is_none_or(|tier| tier < required)
            {
                return Err(ToolError::PermissionDenied(
                    format!(
                        "\"{}\" is not granted for this action at the target location.",
                        app.display_name
                    )
                    .into(),
                ));
            }
            target = Some(app);
        }
        Ok(target)
    }
    pub(crate) fn policy_snapshot(
        &self,
    ) -> Result<Option<(String, device_policy::DevicePolicy)>, ToolError> {
        self.policy_path
            .as_deref()
            .map(device_policy::read)
            .transpose()
    }

    pub(crate) fn check_policy_revision(&self, expected: Option<&str>) -> Result<(), ToolError> {
        let current = self.policy_snapshot()?;
        if current.as_ref().map(|(digest, _)| digest.as_str()) != expected
            || current.as_ref().is_some_and(|(_, policy)| !policy.enabled)
        {
            return Err(ToolError::PermissionDenied(
                "Computer access settings changed. Request access again.".into(),
            ));
        }
        Ok(())
    }

    /// Called at each entry point and batch step; never holds a state lock over await.
    pub(crate) async fn sync_policy(
        &self,
        ctx: &ToolUseContext,
    ) -> Result<Option<String>, ToolError> {
        let snapshot = match self.policy_snapshot() {
            Ok(snapshot) => snapshot,
            Err(error) => {
                {
                    let mut state = self
                        .state
                        .lock()
                        .map_err(|_| ToolError::Internal("computer state poisoned".into()))?;
                    state.revoke_grants();
                    state.policy_revision = None;
                }
                {
                    let mut runtime = self
                        .runtime
                        .lock()
                        .map_err(|_| ToolError::Internal("computer state poisoned".into()))?;
                    runtime.frames.clear();
                    runtime.snapshots.clear();
                    runtime.pending_observations.clear();
                }
                self.release_owned_inputs(ctx).await?;
                return Err(error);
            }
        };
        let revision = snapshot.as_ref().map(|(digest, _)| digest.clone());
        let changed = {
            let mut state = self
                .state
                .lock()
                .map_err(|_| ToolError::Internal("computer state poisoned".into()))?;
            let changed = state.policy_revision != revision;
            if changed {
                state.revoke_grants();
                state.policy_revision = revision.clone();
            }
            if let Some((_, policy)) = &snapshot {
                if policy.enabled
                    && !self
                        .os_permissions_revoked
                        .load(std::sync::atomic::Ordering::SeqCst)
                {
                    for (id, tier) in policy.permanent_grants() {
                        state.grant_app(id.to_owned(), tier);
                    }
                }
            }
            changed
        };
        if changed {
            {
                let mut runtime = self
                    .runtime
                    .lock()
                    .map_err(|_| ToolError::Internal("computer state poisoned".into()))?;
                runtime.frames.clear();
                runtime.snapshots.clear();
                runtime.pending_observations.clear();
            }
            self.release_owned_inputs(ctx).await?;
        }
        if snapshot.as_ref().is_some_and(|(_, policy)| !policy.enabled) {
            return Err(ToolError::PermissionDenied(
                "Computer use is turned off in Desktop settings.".into(),
            ));
        }
        Ok(revision)
    }
}
