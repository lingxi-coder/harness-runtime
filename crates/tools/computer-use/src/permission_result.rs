//! Model-facing TCC setup replies, replayed against Claude Code 2.1.325.
//! The only text substitution is the application name, Claude → LingXi.

use super::{json, ToolCallResult, Utf16JsonProjection};

pub(super) fn tcc_setup_result((accessibility, screen_recording): (bool, bool)) -> ToolCallResult {
    let granted = accessibility && screen_recording;
    let message = if granted {
        "macOS Accessibility and Screen Recording are now both granted. Call request_access again immediately — the next call will show the app selection list.".to_owned()
    } else {
        let mut missing = Vec::new();
        if !accessibility {
            missing.push("Accessibility");
        }
        if !screen_recording {
            missing.push("Screen Recording");
        }
        format!("The user saw the permission prompt but macOS {} permission(s) are still not granted. Do not retry in this turn. Let the user know these permissions need to be granted in the LingXi desktop app on the computer where it's running. If the user grants them and sends a new request, you may call request_access again.", missing.join(" and "))
    };
    // Upstream deliberately marks even the newly-granted reply as an error:
    // this call performed setup, and application selection still needs a retry.
    let mut data = json!({"content":[{"type":"text","text":message}],"isError":true});
    if !granted {
        data["telemetry"] = json!({"error_kind":"tcc_not_granted"});
    }
    let mut result =
        ToolCallResult::from_data(tool_api::tool_trait::generated_builtin_result(&data));
    result.model_content = Some(Utf16JsonProjection::from(message));
    result.is_error = true;
    result
}
