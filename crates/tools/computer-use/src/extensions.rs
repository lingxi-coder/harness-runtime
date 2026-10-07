use crate::validate;
use serde_json::Value;
use tool_api::tool_trait::ToolError;

fn invalid(message: &str) -> ToolError {
    ToolError::InvalidInput(message.into())
}
pub fn keys(input: &Value, field: &str) -> Result<Vec<String>, ToolError> {
    if let Some(value) = input.get(field) {
        let array = value
            .as_array()
            .ok_or_else(|| invalid("keys/modifiers must be an array"))?;
        if array.is_empty() && field == "keys" || array.len() > 32 {
            return Err(invalid("key array must contain between 1 and 32 keys"));
        }
        array
            .iter()
            .map(|v| {
                v.as_str()
                    .filter(|s| !s.is_empty())
                    .map(str::to_string)
                    .ok_or_else(|| invalid("each key must be a non-empty string"))
            })
            .collect()
    } else if field == "keys" {
        Ok(validate::require_text(input)?
            .split('+')
            .map(str::to_string)
            .collect())
    } else {
        Ok(vec![])
    }
}
pub fn path(input: &Value) -> Result<Option<Vec<(u32, u32)>>, ToolError> {
    let Some(value) = input.get("path") else {
        return Ok(None);
    };
    if input.get("coordinate").is_some()
        || input.get("start_coordinate").is_some()
        || input.get("x").is_some()
        || input.get("y").is_some()
    {
        return Err(invalid("path conflicts with coordinate/start_coordinate"));
    }
    let array = value
        .as_array()
        .ok_or_else(|| invalid("path must be an array"))?;
    if array.len() < 2 || array.len() > 1000 {
        return Err(invalid("path must contain between 2 and 1000 points"));
    }
    array
        .iter()
        .map(|p| validate::require_coord(&serde_json::json!({"coordinate":p}), "coordinate"))
        .collect::<Result<Vec<_>, _>>()
        .map(Some)
}
pub fn pixel_delta(input: &Value) -> Result<Option<(i32, i32)>, ToolError> {
    let Some(value) = input.get("pixel_delta") else {
        return Ok(None);
    };
    if [
        "scroll_direction",
        "scroll_amount",
        "direction",
        "amount",
        "dx",
        "dy",
    ]
    .iter()
    .any(|k| input.get(k).is_some())
    {
        return Err(invalid("pixel_delta conflicts with tick scroll fields"));
    }
    let array = value
        .as_array()
        .filter(|v| v.len() == 2)
        .ok_or_else(|| invalid("pixel_delta must contain two integers"))?;
    let read = |v: &Value| {
        v.as_i64()
            .and_then(|n| i32::try_from(n).ok())
            .filter(|n| n.unsigned_abs() <= 100_000)
            .ok_or_else(|| invalid("pixel_delta exceeds 100000 pixels"))
    };
    Ok(Some((read(&array[0])?, read(&array[1])?)))
}
pub fn validate(action: &str, input: &Value) -> Result<(), ToolError> {
    for field in ["use_current_cursor", "press_enter"] {
        if input.get(field).is_some_and(|v| !v.is_boolean()) {
            return Err(invalid("use_current_cursor/press_enter must be boolean"));
        }
    }
    if input.get("use_current_cursor").and_then(Value::as_bool) == Some(true)
        && ["coordinate", "x", "y"]
            .iter()
            .any(|k| input.get(k).is_some())
    {
        return Err(invalid("use_current_cursor conflicts with coordinate"));
    }
    if input.get("keys").is_some() && input.get("text").is_some() {
        return Err(invalid("keys conflicts with text"));
    }
    let modifiers = keys(input, "modifiers")?;
    if modifiers.iter().any(|k| {
        !matches!(
            k.to_ascii_lowercase().as_str(),
            "cmd"
                | "command"
                | "meta"
                | "super"
                | "win"
                | "windows"
                | "ctrl"
                | "control"
                | "alt"
                | "option"
                | "opt"
                | "shift"
        )
    }) {
        return Err(invalid("modifiers must contain modifier keys"));
    }
    if !modifiers.is_empty()
        && !matches!(
            action,
            "mouse_click"
                | "left_click"
                | "right_click"
                | "middle_click"
                | "double_click"
                | "triple_click"
                | "mouse_move"
                | "left_click_drag"
                | "scroll"
                | "left_mouse_down"
                | "left_mouse_up"
        )
    {
        return Err(invalid("modifiers are only supported for mouse actions"));
    }
    if input.get("path").is_some() && action != "left_click_drag" {
        return Err(invalid("path is only supported for left_click_drag"));
    }
    if input.get("pixel_delta").is_some() && action != "scroll" {
        return Err(invalid("pixel_delta is only supported for scroll"));
    }
    if input.get("button").is_some() && action != "mouse_click" {
        return Err(invalid("button is only supported for mouse_click"));
    }
    if input.get("keys").is_some() && !matches!(action, "key" | "hold_key") {
        return Err(invalid("keys is only supported for key/hold_key"));
    }
    if input.get("press_enter").is_some() && action != "type" {
        return Err(invalid("press_enter is only supported for type"));
    }
    if input.get("use_current_cursor").is_some()
        && !matches!(
            action,
            "mouse_click"
                | "left_click"
                | "right_click"
                | "middle_click"
                | "double_click"
                | "triple_click"
                | "scroll"
        )
    {
        return Err(invalid(
            "use_current_cursor is only supported for click/scroll",
        ));
    }
    if let Some(v) = input.get("coordinate") {
        validate::require_coord(&serde_json::json!({"coordinate":v}), "coordinate")?;
    }
    if input.get("x").is_some() || input.get("y").is_some() {
        validate::require_coord(input, "coordinate")?;
    }
    if input.get("start_coordinate").is_some() {
        validate::require_coord(input, "start_coordinate")?;
    }
    match action {
        "mouse_move" => {
            validate::require_coord(input, "coordinate")?;
        }
        "left_click" | "right_click" | "middle_click" | "double_click" | "triple_click"
        | "mouse_click" | "scroll" => {
            if input.get("use_current_cursor").and_then(Value::as_bool) != Some(true) {
                validate::require_coord(input, "coordinate")?;
            }
            if action == "mouse_click"
                && !matches!(
                    input.get("button").and_then(Value::as_str),
                    Some("left" | "right" | "middle" | "back" | "forward")
                )
            {
                return Err(invalid(
                    "button must be left, right, middle, back, or forward",
                ));
            }
            if action == "scroll" && pixel_delta(input)?.is_none() {
                crate::scroll_delta(input)?;
            }
        }
        "left_click_drag" => {
            if path(input)?.is_none() {
                validate::require_coord(input, "coordinate")?;
            }
        }
        "key" | "hold_key" => {
            keys(input, "keys")?;
            validate::key_repeat(input)?;
            if action == "hold_key" {
                validate::hold_duration(input)?;
            }
        }
        "key_down" | "key_up" => {
            let text = validate::require_text(input)?;
            if text.is_empty() || text.contains('+') {
                return Err(invalid("key_down/key_up require one key"));
            }
        }
        "type" | "write_clipboard" => {
            validate::require_text(input)?;
        }
        "wait" => {
            validate::wait_duration(input)?;
        }
        "zoom" => {
            validate::require_region(input)?;
        }
        "open_application" => {
            if input.get("bundle_id").and_then(Value::as_str).is_none() {
                return Err(invalid("bundle_id is required"));
            }
        }
        "switch_display" => {
            if input.get("display").and_then(Value::as_str).is_none() {
                return Err(invalid("display is required"));
            }
        }
        "request_access" => {
            if input.get("apps").is_some_and(|v| {
                v.as_array()
                    .is_none_or(|a| a.iter().any(|v| !v.is_string()))
            }) {
                return Err(invalid("apps must be an array of strings"));
            }
            if input
                .get("tier")
                .is_some_and(|v| !matches!(v.as_str(), Some("read" | "click" | "full")))
            {
                return Err(invalid("tier must be read, click, or full"));
            }
        }
        _ => {}
    }
    Ok(())
}
