//! Real, isolated fixture acceptance. Does not request/reset TCC permission.
//! See examples/fixtures/README.md before running.
#[cfg(target_os = "macos")]
fn main() -> Result<(), Box<dyn std::error::Error>> {
    use base64::Engine;
    use lingxi_core::{
        host::computer_control::ComputerControl, types::utf16_json::Utf16JsonProjection,
    };
    use platform_macos_computer_control::MacosComputerControl;
    use serde_json::{json, Value};
    use std::{
        path::PathBuf,
        time::{Duration, Instant},
    };
    let mut args = std::env::args().skip(1);
    let app = args.next().ok_or("supply fixture bundle id")?;
    if app != "com.lingxi.acceptance.app-scoped-fixture" {
        return Err("only the isolated fixture bundle is accepted".into());
    }
    let receipt = PathBuf::from(args.next().ok_or("supply fixture receipt path")?);
    let output = PathBuf::from(args.next().ok_or("supply new output directory")?);
    std::fs::create_dir(&output)?;
    let runtime = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()?;
    runtime.block_on(async {
        let backend = MacosComputerControl::new();
        if backend.query_os_permissions().await? != Some((true, true)) { return Err::<(),Box<dyn std::error::Error>>("acceptance backend process lacks TCC access; no prompt was requested".into()); }
        let call = |action: &'static str, input: Value| {
            let backend = &backend; let app = &app;
            async move {
                let result = backend.app_scoped(action, app, &Utf16JsonProjection::from_serde_value(&input)?).await?;
                Ok::<Value, Box<dyn std::error::Error>>(serde_json::from_str(&result.to_json_string()?)?)
            }
        };
        let front_before = backend.frontmost_app().await?.map(|a|a.bundle_id);
        let cursor_before = backend.cursor_position().await?;
        let windows = call("list_windows", json!({})).await?;
        let window = windows.as_array().and_then(|w|w.iter().find(|w|w["isMain"]==true).or_else(||w.first())).ok_or("fixture has no window")?;
        let id = window["windowId"].as_u64().ok_or("missing window id")?;
        let mut capture = call("capture_window", json!({"window_id":id,"quality":80})).await?;
        let image = base64::engine::general_purpose::STANDARD.decode(capture["base64"].as_str().ok_or("missing capture")?)?;
        let decoded = image::load_from_memory(&image)?;
        if decoded.width()==0||decoded.height()==0 { return Err("empty fixture capture".into()); }
        std::fs::write(output.join("fixture.jpg"), &image)?;
        capture.as_object_mut().unwrap().remove("base64");
        let nodes=capture["axSummary"].as_array().ok_or("missing AX summary")?;
        let point=|role:&str,title:Option<&str>|->Result<Value,Box<dyn std::error::Error>> {
            let node=nodes.iter().find(|n|n["role"]==role && title.is_none_or(|title|n["title"]==title)).ok_or("fixture control absent from AX summary")?;
            Ok(json!({"x":node["x"].as_f64().ok_or("x")?+node["w"].as_f64().ok_or("w")?/2.,"y":node["y"].as_f64().ok_or("y")?+node["h"].as_f64().ok_or("h")?/2.}))
        };
        let typed=call("dispatch",json!({"window_id":id,"point":point("AXTextField",None)?,"intent":{"kind":"type","text":"fixture native AX verified","mode":"replace","overwriteExisting":true}})).await?;
        if typed["outcome"]!="ok" { return Err(format!("fixture text dispatch refused: {typed}").into()); }
        let wait_state=|key:&str,expected:Value|->Result<Value,Box<dyn std::error::Error>>{
            let deadline=Instant::now()+Duration::from_secs(3);
            loop {
                if let Ok(bytes) = std::fs::read(&receipt) {
                    if let Ok(state) = serde_json::from_slice::<Value>(&bytes) {
                        if state[key] == expected {
                            return Ok(state);
                        }
                    }
                }
                if Instant::now() > deadline {
                    return Err(format!("fixture state did not change: {key}").into());
                }
                std::thread::sleep(Duration::from_millis(50));
            }
        };
        let text_receipt=wait_state("text",json!("fixture native AX verified"))?;
        let clicked=call("dispatch",json!({"window_id":id,"point":point("AXButton",Some("Apply"))?,"intent":{"kind":"click","button":"left","count":1}})).await?;
        if clicked["outcome"]!="ok" { return Err(format!("fixture button dispatch refused: {clicked}").into()); }
        let click_receipt=wait_state("applied",json!(1))?;
        let menu=call("menu",json!({"mode":"press","path":["Fixture","Set via Menu"]})).await?;
        if menu["outcome"]!="ok" { return Err(format!("fixture menu refused: {menu}").into()); }
        let menu_receipt=wait_state("text",json!("menu fixture text"))?;
        if call("dispatch",json!({"window_id":u32::MAX,"point":{"x":1,"y":1},"intent":{"kind":"click"}})).await.is_ok(){return Err("stale window identity was accepted".into());}
        let front_after=backend.frontmost_app().await?.map(|a|a.bundle_id);
        let cursor_after=backend.cursor_position().await?;
        let report=json!({"status":"passed","fixtureBundleId":app,"window":window,"capture":capture,"typed":typed,"clicked":clicked,"menu":menu,"textReceipt":text_receipt,"clickReceipt":click_receipt,"menuReceipt":menu_receipt,"frontmostBefore":front_before,"frontmostAfter":front_after,"cursorBefore":cursor_before,"cursorAfter":cursor_after,"frontmostUnchanged":front_before==front_after,"cursorUnchanged":cursor_before==cursor_after,"limits":["Fixture backend process TCC attribution differs from a newly packaged Desktop unless launched through that host","Foreground/cursor comparisons can be affected by human activity","No Space move/raw-input fallback exercised"]});
        std::fs::write(output.join("report.json"),serde_json::to_vec_pretty(&report)?)?;
        println!("{}",output.join("report.json").display());
        Ok(())
    })
}
#[cfg(not(target_os = "macos"))]
fn main() {
    eprintln!("app-scoped fixture acceptance requires macOS");
    std::process::exit(2);
}
