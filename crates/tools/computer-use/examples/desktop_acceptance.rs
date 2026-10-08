//! Exercise the existing ComputerTool against a disposable macOS target.
//! Run only through scripts/tests/computer_desktop_acceptance.py --run.
#[cfg(not(target_os = "macos"))]
fn main() {
    eprintln!("macOS is required");
    std::process::exit(2);
}

#[cfg(target_os = "macos")]
mod acceptance {
    use lingxi_llm_client::protocol::computer::*;
    use serde_json::{json, Value};
    use std::{
        path::Path,
        sync::Arc,
        time::{Duration, Instant},
    };
    use tool_api::{
        context::ToolUseContext,
        tool_trait::{Tool, ToolError},
    };
    use tool_computer_use::{AutoGrantResolver, ComputerTool};

    async fn invoke(
        tool: &ComputerTool,
        input: Value,
        ctx: &ToolUseContext,
    ) -> Result<Value, String> {
        tool.validate_input(&input, ctx)
            .await
            .map_err(|e| e.to_string())?;
        let result = tool
            .call(input, ctx.clone(), tool_api::test_support::fresh_tx())
            .await
            .map_err(|e| e.to_string())?;
        if result.is_error {
            return Err(result.model_content.unwrap_or_else(|| "tool result marked as error".into()));
        }
        let blocks = tool_api::tool_result_media::image_content_blocks(&result.data);
        tool.computer_model_output(
            ctx,
            result.model_content.as_deref().unwrap_or_default(),
            blocks.as_deref(),
        )
        .await
        .map_err(|e| e.to_string())?;
        Ok(result.data)
    }

    pub async fn run(dir: &Path) -> Result<(), String> {
        let ready: Value = serde_json::from_slice(
            &std::fs::read(dir.join("ready.json")).map_err(|e| e.to_string())?,
        )
        .map_err(|e| e.to_string())?;
        let bundle = ready["bundle_id"]
            .as_str()
            .ok_or("probe bundle id is missing")?;
        if !bundle.starts_with("com.lingxi.computer-use.acceptance.") {
            return Err("unexpected target application".into());
        }
        let backend = platform_macos_computer_control::new_if_supported()
            .ok_or("macOS backend unavailable")?;
        let mut builtins = tool_api::test_support::ctx_for_file_tools(
            tool_api::test_support::make_dummy_fs(),
            Arc::new(telemetry::AnalyticsBus::new()),
            vec![dir.to_path_buf()],
        );
        builtins.computer_control = Some(backend.clone());
        let tool = ComputerTool::with_access_resolver(builtins, Arc::new(AutoGrantResolver));
        let ctx = tool_api::test_support::fresh_ctx();
        let mut checks = Vec::new();
        let result: Result<(), String> = async {
            let permissions = backend.check_os_permissions().await;
            checks.push(json!({"operation":"os_permissions","permissions":permissions}));
            if permissions.is_some_and(|(accessibility, recording)| !accessibility || !recording) {
                return Err(format!("macOS permissions unavailable for the acceptance executable: {permissions:?}"));
            }
            invoke(&tool, json!({"action":"request_access","apps":[bundle],"tier":"full"}), &ctx).await?;
            let source = backend.frame_geometry().await.map_err(|e| e.to_string())?.ok_or("missing physical geometry")?;
            let before=backend.screenshot().await.map_err(|e|e.to_string())?;
            std::fs::write(dir.join("before.png"),before.png_bytes).map_err(|e|e.to_string())?;
            let shot = invoke(&tool, json!({"action":"screenshot"}), &ctx).await?;
            let frame = tool.native_computer_frame(&ctx).await.map_err(|e| e.to_string())?.ok_or("no model-visible frame")?;
            checks.push(json!({"operation":"screenshot","frame":frame,"physical_scale":source.scale,"width":shot["width"],"height":shot["height"]}));
            let point = |v: &Value| -> ComputerPoint {
                ComputerPoint {x:(v[0].as_f64().unwrap()-source.origin_x)*source.scale* f64::from(frame.width)/f64::from(source.pixel_width),
                    y:(v[1].as_f64().unwrap()-source.origin_y)*source.scale*f64::from(frame.height)/f64::from(source.pixel_height)}
            };
            let at = |v: &Value| ComputerTarget::Position {point:point(v)};
            let path: Vec<_> = ready["path"].as_array().ok_or("missing drag path")?.iter().map(point).collect();
            let operations = vec![
                ComputerOperation::Click{target:at(&ready["button"]),button:ComputerMouseButton::Left,count:1,modifiers:vec![]},
                ComputerOperation::Click{target:at(&ready["field"]),button:ComputerMouseButton::Left,count:1,modifiers:vec![]},
                ComputerOperation::Type{text:"Computer Use desktop acceptance".into(),press_enter:false},
                ComputerOperation::ScrollWheel{target:at(&ready["scroll"]),direction:ComputerScrollDirection::Down,amount:3,modifiers:vec![]},
                ComputerOperation::Scroll{target:at(&ready["scroll"]),delta_x:0.0,delta_y:120.0,modifiers:vec![]},
                ComputerOperation::Drag{path:path.clone(),modifiers:vec![]},
                ComputerOperation::Click{target:at(&ready["path"][2]),button:ComputerMouseButton::Back,count:1,modifiers:vec![]},
                ComputerOperation::Click{target:at(&ready["path"][2]),button:ComputerMouseButton::Forward,count:1,modifiers:vec![]},
                ComputerOperation::KeyDown{key:"shift".into()},
                ComputerOperation::KeyUp{key:"shift".into()},
                ComputerOperation::Screenshot];
            tool.begin_computer_sequence(&ctx).await.map_err(|e| e.to_string())?;
            for operation in &operations {
                let input = tool.lower_computer_operation(operation, &frame).map_err(|e| e.to_string())?;
                invoke(&tool, input, &ctx).await?;
                checks.push(json!({"operation":operation,"status":"executed"}));
                tokio::time::sleep(Duration::from_millis(120)).await;
            }
            tool.end_computer_sequence(&ctx).await.map_err(|e| e.to_string())?;
            std::fs::copy(dir.join("observed.json"),dir.join("pre-cancel.json")).map_err(|e|e.to_string())?;
            let cancel = tokio_util::sync::CancellationToken::new();
            let mut held_ctx=ctx.clone();held_ctx.cancel=Some(cancel.clone());
            let trigger=cancel.clone();
            tokio::spawn(async move {tokio::time::sleep(Duration::from_millis(100)).await;trigger.cancel();});
            let started=Instant::now();
            let held = tool.call(json!({"action":"hold_key","text":"shift","duration":1.0}), held_ctx,
                tool_api::test_support::fresh_tx()).await;
            if !matches!(held, Err(ToolError::Aborted)) || !cancel.is_cancelled() { return Err("hold did not abort from the requested cancellation".into()); }
            checks.push(json!({"operation":"cancel_hold","status":"cancelled","elapsed_ms":started.elapsed().as_millis(),"token_cancelled":cancel.is_cancelled()}));
            tokio::time::sleep(Duration::from_millis(150)).await;
            std::fs::copy(dir.join("observed.json"),dir.join("post-cancel.json")).map_err(|e|e.to_string())?;
            invoke(&tool, json!({"action":"mouse_move","coordinate":[(path[0].x*f64::from(source.pixel_width)/f64::from(frame.width)).round() as u32,
                (path[0].y*f64::from(source.pixel_height)/f64::from(frame.height)).round() as u32]}), &ctx).await?;
            invoke(&tool, json!({"action":"left_mouse_down"}), &ctx).await?;
            tool.cleanup_computer_inputs(&ctx).await.map_err(|e| e.to_string())?;
            tokio::time::sleep(Duration::from_millis(200)).await;
            invoke(&tool, json!({"action":"list_granted_applications"}), &ctx).await?;
            checks.push(json!({"operation":"function_management","status":"passed"}));
            let after=backend.screenshot().await.map_err(|e|e.to_string())?;
            std::fs::write(dir.join("after.png"),after.png_bytes).map_err(|e|e.to_string())?;
            Ok(())
        }.await;
        let cleanup = tool
            .cleanup_computer_inputs(&ctx)
            .await
            .map_err(|e| e.to_string());
        let report = json!({"status":if result.is_ok()&&cleanup.is_ok(){"executed"}else{"failed"},
            "checks":checks,"error":result.as_ref().err(),"cleanup_error":cleanup.as_ref().err(),
            "scope":"real ComputerTool and macOS backend; no provider API, Orchestrator, hooks or durable journal exercised"});
        std::fs::write(
            dir.join("execution.json"),
            serde_json::to_vec_pretty(&report).unwrap(),
        )
        .map_err(|e| e.to_string())?;
        result.and(cleanup)
    }
}

#[cfg(target_os = "macos")]
#[tokio::main]
async fn main() {
    let args: Vec<_> = std::env::args().skip(1).collect();
    if args.len() != 2 || args[0] != "--run" {
        eprintln!("Usage: desktop_acceptance --run <probe-output-directory>");
        std::process::exit(2);
    }
    if let Err(error) = acceptance::run(std::path::Path::new(&args[1])).await {
        eprintln!("{error}");
        std::process::exit(1);
    }
}
