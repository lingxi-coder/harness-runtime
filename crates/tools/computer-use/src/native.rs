use crate::{json, ComputerTool, ToolError, ToolUseContext, Value};
use lingxi_core::host::computer_control::ComputerFrameGeometry;
use lingxi_llm_client::protocol::computer::{
    ComputerCapabilities, ComputerFrame, ComputerMouseButton, ComputerOperation,
    ComputerOperationKind, ComputerPoint, ComputerScrollDirection, ComputerTarget,
};

#[derive(Clone)]
pub(crate) struct ObservedFrame {
    pub frame: ComputerFrame,
    pub source: ComputerFrameGeometry,
    pub owner: String,
    pub image_fingerprint: [u8; 32],
    pub model_visible: bool,
    pub desktop_generation: u64,
}
impl ComputerTool {
    pub(crate) fn desktop_generation(&self) -> Result<u64, ToolError> {
        let runtime = self
            .runtime
            .lock()
            .map_err(|_| ToolError::Internal("computer state poisoned".into()))?;
        if let Some(lease) = &runtime.lease {
            return lease
                .generation()
                .map_err(|e| ToolError::Internal(e.to_string()));
        }
        drop(runtime);
        let scope = self
            .ctx
            .computer_control
            .as_ref()
            .and_then(|cc| cc.desktop_lock_scope())
            .unwrap_or_else(|| self.lock_home.clone());
        let lease = crate::lock::DesktopLease::acquire(&scope).map_err(|e| {
            ToolError::InvalidInput(format!("computer observation cannot acquire desktop: {e}"))
        })?;
        lease
            .generation()
            .map_err(|e| ToolError::Internal(e.to_string()))
    }

    fn validate_observation_generation(&self, snapshot: &ObservedFrame) -> Result<(), ToolError> {
        let generation = self.desktop_generation()?;
        let runtime = self
            .runtime
            .lock()
            .map_err(|_| ToolError::Internal("computer state poisoned".into()))?;
        let frozen = runtime.sequence
            && runtime.owner.as_ref() == Some(&snapshot.owner)
            && runtime.sequence_generation == Some(snapshot.desktop_generation);
        if snapshot.desktop_generation != generation && !frozen {
            return Err(ToolError::InvalidInput(
                "computer desktop changed since observation; take a fresh screenshot".into(),
            ));
        }
        Ok(())
    }

    pub(crate) fn native_capabilities(&self) -> Option<ComputerCapabilities> {
        let cc = self.ctx.computer_control.as_ref()?;
        let capabilities = cc.capabilities();
        if !capabilities.frame_geometry {
            return None;
        }
        let mut operations = vec![
            ComputerOperationKind::Click,
            ComputerOperationKind::Move,
            ComputerOperationKind::Drag,
            ComputerOperationKind::ScrollWheel,
            ComputerOperationKind::Key,
            ComputerOperationKind::Type,
            ComputerOperationKind::Wait,
            ComputerOperationKind::Screenshot,
            ComputerOperationKind::Zoom,
            ComputerOperationKind::MouseDown,
            ComputerOperationKind::MouseUp,
            ComputerOperationKind::CursorPosition,
        ];
        if capabilities.held_keys {
            operations.extend([
                ComputerOperationKind::KeyDown,
                ComputerOperationKind::KeyUp,
                ComputerOperationKind::HoldKey,
            ]);
        }
        if capabilities.pixel_scroll {
            operations.push(ComputerOperationKind::Scroll);
        }
        Some(ComputerCapabilities { operations })
    }
    pub(crate) async fn invalidate_frame(&self, ctx: &ToolUseContext) {
        let owner = Self::owner(ctx).await;
        if let Ok(mut runtime) = self.runtime.lock() {
            runtime.frames.remove(&owner);
            runtime.pending_observations.remove(&owner);
            runtime
                .snapshots
                .retain(|_, snapshot| snapshot.owner != owner);
        }
    }
    pub(crate) async fn mark_input_changed(&self, ctx: &ToolUseContext) -> Result<(), ToolError> {
        let owner = Self::owner(ctx).await;
        let mut runtime = self
            .runtime
            .lock()
            .map_err(|_| ToolError::Internal("computer state poisoned".into()))?;
        runtime
            .lease
            .as_ref()
            .ok_or_else(|| ToolError::Internal("computer input requires the desktop lease".into()))?
            .advance_generation()
            .map_err(|e| ToolError::Internal(e.to_string()))?;
        runtime.pending_observations.clear();
        // Every owner observes the same physical desktop. Only the current
        // sequence may retain its frozen snapshots while its lease is held.
        runtime
            .frames
            .retain(|frame_owner, _| frame_owner == &owner);
        let sequence = runtime.sequence;
        runtime
            .snapshots
            .retain(|_, frame| sequence && frame.owner == owner);
        if runtime.sequence && runtime.owner.as_ref() == Some(&owner) {
            runtime.sequence_dirty = true;
            // The sequence continues using its frozen immutable snapshot.
            // The current image is no longer eligible for another request.
            if let Some(frame) = runtime.frames.get_mut(&owner) {
                frame.model_visible = false;
            }
        } else {
            runtime.frames.remove(&owner);
            runtime
                .snapshots
                .retain(|_, snapshot| snapshot.owner != owner);
        }
        Ok(())
    }
    pub(crate) async fn record_frame(
        &self,
        data: &mut Value,
        ctx: &ToolUseContext,
    ) -> Result<(), ToolError> {
        let Some(cc) = &self.ctx.computer_control else {
            return Ok(());
        };
        let Some(source) = cc.frame_geometry().await.map_err(|e| crate::map_err(&e))? else {
            return Ok(());
        };
        let width = data
            .pointer("/file/dimensions/displayWidth")
            .or_else(|| data.get("width"))
            .and_then(Value::as_u64)
            .and_then(|v| u32::try_from(v).ok())
            .unwrap_or(0);
        let height = data
            .pointer("/file/dimensions/displayHeight")
            .or_else(|| data.get("height"))
            .and_then(Value::as_u64)
            .and_then(|v| u32::try_from(v).ok())
            .unwrap_or(0);
        if data.get("width").and_then(Value::as_u64) != Some(u64::from(source.pixel_width))
            || data.get("height").and_then(Value::as_u64) != Some(u64::from(source.pixel_height))
        {
            return Err(ToolError::Internal(
                "screenshot and current display geometry disagree; observe again".into(),
            ));
        }
        let owner = Self::owner(ctx).await;
        let desktop_generation = self.desktop_generation()?;
        use std::hash::{Hash, Hasher};
        let mut hasher = std::collections::hash_map::DefaultHasher::new();
        owner.hash(&mut hasher);
        let mut runtime = self
            .runtime
            .lock()
            .map_err(|_| ToolError::Internal("computer state poisoned".into()))?;
        runtime.capture_generation = runtime.capture_generation.wrapping_add(1);
        if runtime.owner.as_ref() == Some(&owner) {
            runtime.sequence_dirty = false;
        }
        let frame = ComputerFrame {
            width,
            height,
            geometry_version: format!(
                "{}:{width}x{height}:{:x}:{}",
                source.version,
                hasher.finish(),
                runtime.capture_generation
            ),
        };
        data["computer_frame"] = json!({"width":width,"height":height,"geometry_version":frame.geometry_version,"display_id":source.display_id,"capture_width":source.pixel_width,"capture_height":source.pixel_height,"origin":[source.origin_x,source.origin_y],"scale":source.scale,"crop":[0,0,source.pixel_width,source.pixel_height]});
        let image_blocks = tool_api::tool_result_media::image_content_blocks(data)
            .ok_or_else(|| ToolError::Internal("screenshot has no model image".into()))?;
        let image_fingerprint = image_fingerprint(&image_blocks)
            .ok_or_else(|| ToolError::Internal("screenshot image source unavailable".into()))?;
        let observed = ObservedFrame {
            frame: frame.clone(),
            source,
            owner: owner.clone(),
            image_fingerprint,
            model_visible: false,
            desktop_generation,
        };
        runtime.pending_observations.insert(
            owner.clone(),
            ctx.tool_use_id.as_ref().map(ToString::to_string),
        );
        runtime
            .snapshots
            .insert(frame.geometry_version, observed.clone());
        runtime.frames.insert(owner, observed);
        if !runtime.sequence {
            let current = runtime
                .frames
                .values()
                .map(|frame| frame.frame.geometry_version.clone())
                .collect::<std::collections::HashSet<_>>();
            runtime
                .snapshots
                .retain(|version, _| current.contains(version));
        }
        Ok(())
    }
    pub(crate) async fn validate_geometry(
        &self,
        input: &Value,
        ctx: &ToolUseContext,
    ) -> Result<(), ToolError> {
        let Some(version) = input.get("geometry_version").and_then(Value::as_str) else {
            return Ok(());
        };
        let owner = Self::owner(ctx).await;
        let snapshot = self
            .runtime
            .lock()
            .map_err(|_| ToolError::Internal("computer state poisoned".into()))?
            .snapshots
            .get(version)
            .filter(|f| f.owner == owner)
            .cloned()
            .ok_or_else(|| {
                ToolError::InvalidInput(
                    "computer geometry belongs to another agent or is stale".into(),
                )
            })?;
        let current = self
            .ctx
            .computer_control
            .as_ref()
            .ok_or_else(|| ToolError::Internal("computer unavailable".into()))?
            .frame_geometry()
            .await
            .map_err(|e| crate::map_err(&e))?;
        self.validate_observation_generation(&snapshot)?;
        if current.as_ref() != Some(&snapshot.source) {
            return Err(ToolError::InvalidInput(
                "computer display geometry changed; take a fresh screenshot".into(),
            ));
        }
        Ok(())
    }
    pub(crate) async fn apply_model_observation(
        &self,
        ctx: &ToolUseContext,
        content: &str,
        blocks: Option<&[Value]>,
    ) -> Result<(), ToolError> {
        let owner = Self::owner(ctx).await;
        let call_id = ctx.tool_use_id.as_ref().map(ToString::to_string);
        let observed = {
            let runtime = self
                .runtime
                .lock()
                .map_err(|_| ToolError::Internal("computer state poisoned".into()))?;
            if runtime.pending_observations.get(&owner) != Some(&call_id) {
                return Ok(());
            }
            runtime.frames.get(&owner).cloned()
        };
        let Some(observed) = observed else {
            return Ok(());
        };
        let parsed = serde_json::from_str::<Value>(content).ok();
        let derived = parsed.as_ref().and_then(|data| {
            if let Some(array) = data.as_array() {
                Some(array.clone())
            } else {
                tool_api::tool_result_media::media_content_blocks_for_tool(
                    "computer", data, content,
                )
            }
        });
        let batch_images = parsed
            .as_ref()
            .filter(|data| data["stepsCompleted"].is_u64())
            .and_then(|data| data["results"].as_array())
            .map(|results| {
                results
                    .iter()
                    .filter(|item| {
                        matches!(item["action"].as_str(), Some("screenshot" | "zoom"))
                            && item["result"]["type"] == "image"
                    })
                    .collect::<Vec<_>>()
            });
        let visible = blocks.or(derived.as_deref());
        let actual = if let Some(images) = &batch_images {
            visible.and_then(|blocks| {
                let actual: Vec<_> = blocks
                    .iter()
                    .filter(|block| block["type"] == "image")
                    .collect();
                if actual.len() != images.len()
                    || images
                        .last()
                        .is_none_or(|item| item["action"] != "screenshot")
                {
                    return None;
                }
                image_fingerprint(std::slice::from_ref(*actual.last()?))
            })
        } else {
            visible.and_then(image_fingerprint)
        };
        let frame_metadata = batch_images
            .as_ref()
            .and_then(|images| images.last())
            .and_then(|item| item["result"].get("computer_frame"))
            .or_else(|| parsed.as_ref().and_then(|data| data.get("computer_frame")));
        let metadata_matches=frame_metadata.is_none_or(|frame| {
            // JavaScript JSON renders 2.0 as 2. Normalize only floating-point
            // geometry fields; all other fields and unknown keys remain exact.
            let Some(origin) = frame["origin"].as_array() else { return false; };
            let [x, y] = origin.as_slice() else { return false; };
            let (Some(x), Some(y), Some(scale)) = (x.as_f64(), y.as_f64(), frame["scale"].as_f64()) else { return false; };
            let mut actual = frame.clone();
            actual["origin"] = json!([x, y]);
            actual["scale"] = json!(scale);
            actual==json!({"width":observed.frame.width,"height":observed.frame.height,"geometry_version":observed.frame.geometry_version,"display_id":observed.source.display_id,"capture_width":observed.source.pixel_width,"capture_height":observed.source.pixel_height,"origin":[observed.source.origin_x,observed.source.origin_y],"scale":observed.source.scale,"crop":[0,0,observed.source.pixel_width,observed.source.pixel_height]})
        });
        let mut runtime = self
            .runtime
            .lock()
            .map_err(|_| ToolError::Internal("computer state poisoned".into()))?;
        if runtime.pending_observations.get(&owner) != Some(&call_id)
            || runtime
                .frames
                .get(&owner)
                .is_none_or(|frame| frame.frame.geometry_version != observed.frame.geometry_version)
        {
            return Ok(());
        }
        if actual != Some(observed.image_fingerprint) || !metadata_matches {
            runtime.pending_observations.remove(&owner);
            runtime.frames.remove(&owner);
            runtime.snapshots.retain(|_, frame| frame.owner != owner);
            return Ok(());
        }
        runtime.pending_observations.remove(&owner);
        if let Some(frame) = runtime.frames.get_mut(&owner) {
            frame.model_visible = true;
        }
        if let Some(frame) = runtime.snapshots.get_mut(&observed.frame.geometry_version) {
            frame.model_visible = true;
        }
        Ok(())
    }
    pub(crate) async fn ready_frame(
        &self,
        ctx: &ToolUseContext,
    ) -> Result<Option<ComputerFrame>, ToolError> {
        self.check_cancel(ctx)?;
        let Some(cc) = &self.ctx.computer_control else {
            return Ok(None);
        };
        if self.native_capabilities().is_none() {
            return Ok(None);
        }
        if cc
            .check_os_permissions()
            .await
            .is_some_and(|(a, r)| !a || !r)
        {
            return Ok(None);
        }
        if self.enforce_tier("key").await.is_err() {
            return Ok(None);
        }
        let owner = Self::owner(ctx).await;
        let observed = self
            .runtime
            .lock()
            .map_err(|_| ToolError::Internal("computer state poisoned".into()))?
            .frames
            .get(&owner)
            .cloned();
        let Some(observed) = observed.filter(|frame| frame.model_visible) else {
            return Ok(None);
        };
        if self.validate_observation_generation(&observed).is_err() {
            self.invalidate_frame(ctx).await;
            return Ok(None);
        }
        if cc
            .frame_geometry()
            .await
            .map_err(|e| crate::map_err(&e))?
            .as_ref()
            != Some(&observed.source)
        {
            self.invalidate_frame(ctx).await;
            return Ok(None);
        }
        Ok(Some(observed.frame))
    }
    pub(crate) fn lower_native(
        &self,
        operation: &ComputerOperation,
        frame: &ComputerFrame,
    ) -> Result<Value, ToolError> {
        operation
            .validate(frame)
            .map_err(|e| ToolError::InvalidInput(e.to_string()))?;
        let snapshot = {
            let runtime = self
                .runtime
                .lock()
                .map_err(|_| ToolError::Internal("computer state poisoned".into()))?;
            runtime
                .snapshots
                .get(&frame.geometry_version)
                .filter(|observed| {
                    observed.model_visible
                        && observed.frame.geometry_version == frame.geometry_version
                        && observed.frame.width == frame.width
                        && observed.frame.height == frame.height
                })
                .cloned()
                .ok_or_else(|| {
                    ToolError::InvalidInput(
                        "computer screenshot geometry is unavailable; take a fresh screenshot"
                            .into(),
                    )
                })?
        };
        self.validate_observation_generation(&snapshot)?;
        let source = snapshot.source;
        let point = |p: &ComputerPoint| -> Result<Value, ToolError> {
            if !p.x.is_finite()
                || !p.y.is_finite()
                || p.x < 0.0
                || p.y < 0.0
                || p.x >= f64::from(frame.width)
                || p.y >= f64::from(frame.height)
            {
                return Err(ToolError::InvalidInput(
                    "computer point lies outside the observed frame".into(),
                ));
            }
            Ok(json!([
                (p.x * f64::from(source.pixel_width) / f64::from(frame.width)).floor() as u32,
                (p.y * f64::from(source.pixel_height) / f64::from(frame.height)).floor() as u32
            ]))
        };
        let target = |value: &mut Value, t: &ComputerTarget| -> Result<(), ToolError> {
            match t {
                ComputerTarget::Position { point: p } => value["coordinate"] = point(p)?,
                ComputerTarget::CurrentCursor => value["use_current_cursor"] = json!(true),
            };
            Ok(())
        };
        let button = |b: &ComputerMouseButton| match b {
            ComputerMouseButton::Left => "left",
            ComputerMouseButton::Right => "right",
            ComputerMouseButton::Middle => "middle",
            ComputerMouseButton::Back => "back",
            ComputerMouseButton::Forward => "forward",
        };
        let mut value = match operation {
            ComputerOperation::Click {
                target: t,
                button: b,
                count,
                modifiers,
            } => {
                if *count == 0
                    || *count > 3
                    || *count > 1 && !matches!(b, ComputerMouseButton::Left)
                {
                    return Err(ToolError::InvalidInput(
                        "computer click count/button combination is unsupported".into(),
                    ));
                }
                if matches!(b, ComputerMouseButton::Back | ComputerMouseButton::Forward)
                    && !self
                        .ctx
                        .computer_control
                        .as_ref()
                        .is_some_and(|cc| cc.capabilities().side_buttons)
                {
                    return Err(ToolError::InvalidInput(
                        "computer backend does not support side buttons".into(),
                    ));
                }
                let mut v = json!({"action":match count {2=>"double_click",3=>"triple_click",_=>"mouse_click"},"modifiers":modifiers});
                if *count == 1 {
                    v["button"] = json!(button(b));
                }
                target(&mut v, t)?;
                v
            }
            ComputerOperation::Move {
                point: p,
                modifiers,
            } => json!({"action":"mouse_move","coordinate":point(p)?,"modifiers":modifiers}),
            ComputerOperation::Drag { path, modifiers } => {
                json!({"action":"left_click_drag","path":path.iter().map(&point).collect::<Result<Vec<_>,_>>()?,"modifiers":modifiers})
            }
            ComputerOperation::Scroll {
                target: t,
                delta_x,
                delta_y,
                modifiers,
            } => {
                let delta = |v: f64| -> Result<i32, ToolError> {
                    if !v.is_finite() || v.fract() != 0.0 || v.abs() > 100000.0 {
                        return Err(ToolError::InvalidInput(
                            "pixel delta must be an integer within 100000".into(),
                        ));
                    }
                    Ok(v as i32)
                };
                let mut v = json!({"action":"scroll","pixel_delta":[delta(*delta_x)?,delta(*delta_y)?],"modifiers":modifiers});
                target(&mut v, t)?;
                v
            }
            ComputerOperation::ScrollWheel {
                target: t,
                direction,
                amount,
                modifiers,
            } => {
                let direction = match direction {
                    ComputerScrollDirection::Up => "up",
                    ComputerScrollDirection::Down => "down",
                    ComputerScrollDirection::Left => "left",
                    ComputerScrollDirection::Right => "right",
                };
                let mut v = json!({"action":"scroll","scroll_direction":direction,"scroll_amount":amount,"modifiers":modifiers});
                target(&mut v, t)?;
                v
            }
            ComputerOperation::Key { keys } => json!({"action":"key","keys":keys}),
            ComputerOperation::KeyDown { key } => json!({"action":"key_down","text":key}),
            ComputerOperation::KeyUp { key } => json!({"action":"key_up","text":key}),
            ComputerOperation::HoldKey {
                keys,
                duration_seconds,
            } => json!({"action":"hold_key","keys":keys,"duration":duration_seconds}),
            ComputerOperation::Type { text, press_enter } => {
                json!({"action":"type","text":text,"press_enter":press_enter})
            }
            ComputerOperation::Wait { duration_seconds } => {
                json!({"action":"wait","duration":duration_seconds})
            }
            ComputerOperation::Screenshot => json!({"action":"screenshot"}),
            ComputerOperation::Zoom { region } => {
                let p0 = point(&ComputerPoint {
                    x: region[0],
                    y: region[1],
                })?;
                // The far edge is exclusive and may equal the frame size.
                if region[2] > f64::from(frame.width)
                    || region[3] > f64::from(frame.height)
                    || !region[2].is_finite()
                    || !region[3].is_finite()
                {
                    return Err(ToolError::InvalidInput("zoom exceeds frame".into()));
                }
                json!({"action":"zoom","region":[p0[0],p0[1],(region[2]*f64::from(source.pixel_width)/f64::from(frame.width)).floor() as u32,(region[3]*f64::from(source.pixel_height)/f64::from(frame.height)).floor() as u32]})
            }
            ComputerOperation::MouseDown {
                target: t,
                modifiers,
            }
            | ComputerOperation::MouseUp {
                target: t,
                modifiers,
            } => {
                let mut v = json!({"action":if matches!(operation,ComputerOperation::MouseDown{..}){"left_mouse_down"}else{"left_mouse_up"},"modifiers":modifiers});
                if let Some(p) = t {
                    v["coordinate"] = point(p)?;
                }
                v
            }
            ComputerOperation::CursorPosition => json!({"action":"cursor_position"}),
        };
        self.validate_final_input(value["action"].as_str().unwrap_or_default(), &value)?;
        // Keep geometry validation attached through hooks to the final input.
        value["geometry_version"] = json!(frame.geometry_version);
        Ok(value)
    }
}

fn image_fingerprint(blocks: &[Value]) -> Option<[u8; 32]> {
    use sha2::{Digest, Sha256};
    let images = blocks
        .iter()
        .filter(|block| block.get("type").and_then(Value::as_str) == Some("image"))
        .collect::<Vec<_>>();
    if images.len() != 1 {
        return None;
    }
    let image = images[0];
    let data = image
        .pointer("/source/data")
        .or_else(|| image.get("data"))
        .and_then(Value::as_str)?;
    let media_type = image
        .pointer("/source/media_type")
        .or_else(|| image.get("mimeType"))
        .and_then(Value::as_str)?;
    if data.is_empty() {
        return None;
    }
    let mut digest = Sha256::new();
    digest.update(media_type.as_bytes());
    digest.update([0]);
    digest.update(data.as_bytes());
    Some(digest.finalize().into())
}
