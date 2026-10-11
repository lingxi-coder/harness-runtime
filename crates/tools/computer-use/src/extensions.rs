use crate::validate;
use lingxi_core::types::utf16_json::{JsonValueView, Utf16JsonProjection};
use tool_api::tool_trait::ToolError;

fn invalid(message: &str) -> ToolError {
    ToolError::InvalidInput(message.into())
}

fn exact_string(value: JsonValueView<'_>) -> Option<String> {
    String::from_utf16(value.code_units()?).ok()
}
pub fn keys(input: &Utf16JsonProjection, field: &str) -> Result<Vec<String>, ToolError> {
    if let Some(value) = input.get_property(field) {
        let len = value
            .array_len()
            .ok_or_else(|| invalid("keys/modifiers must be an array"))?;
        if len == 0 && field == "keys" || len > 32 {
            return Err(invalid("key array must contain between 1 and 32 keys"));
        }
        (0..len)
            .map(|index| {
                let value = value
                    .get_index(index)
                    .ok_or_else(|| invalid("each key must be a non-empty string"))?;
                let key = exact_string(value)
                    .ok_or_else(|| invalid("each key must be a non-empty string"))?;
                (!key.is_empty())
                    .then_some(key)
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
pub fn path(input: &Utf16JsonProjection) -> Result<Option<Vec<(u32, u32)>>, ToolError> {
    let Some(value) = input.get_property("path") else {
        return Ok(None);
    };
    if input.get_property("coordinate").is_some()
        || input.get_property("start_coordinate").is_some()
        || input.get_property("x").is_some()
        || input.get_property("y").is_some()
    {
        return Err(invalid("path conflicts with coordinate/start_coordinate"));
    }
    let len = value
        .array_len()
        .ok_or_else(|| invalid("path must be an array"))?;
    if !(2..=1000).contains(&len) {
        return Err(invalid("path must contain between 2 and 1000 points"));
    }
    (0..len)
        .map(|index| {
            value
                .get_index(index)
                .ok_or_else(|| invalid("path must contain between 2 and 1000 points"))
                .and_then(|point| validate::require_coord_view(point, "path point"))
        })
        .collect::<Result<Vec<_>, _>>()
        .map(Some)
}
pub fn pixel_delta(input: &Utf16JsonProjection) -> Result<Option<(i32, i32)>, ToolError> {
    let Some(value) = input.get_property("pixel_delta") else {
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
    .any(|k| input.get_property(k).is_some())
    {
        return Err(invalid("pixel_delta conflicts with tick scroll fields"));
    }
    if value.array_len() != Some(2) {
        return Err(invalid("pixel_delta must contain two integers"));
    }
    let read = |v: JsonValueView<'_>| {
        v.as_number()
            .filter(|number| number.is_finite() && number.fract() == 0.0)
            .and_then(|number| {
                (number >= i64::MIN as f64 && number < i64::MAX as f64).then_some(number as i64)
            })
            .and_then(|n| i32::try_from(n).ok())
            .filter(|n| n.unsigned_abs() <= 100_000)
            .ok_or_else(|| invalid("pixel_delta exceeds 100000 pixels"))
    };
    let first = value
        .get_index(0)
        .ok_or_else(|| invalid("pixel_delta must contain two integers"))?;
    let second = value
        .get_index(1)
        .ok_or_else(|| invalid("pixel_delta must contain two integers"))?;
    Ok(Some((read(first)?, read(second)?)))
}
pub fn validate(action: &str, input: &Utf16JsonProjection) -> Result<(), ToolError> {
    for field in ["use_current_cursor", "press_enter"] {
        if input
            .get_property(field)
            .is_some_and(|value| value.as_bool().is_none())
        {
            return Err(invalid("use_current_cursor/press_enter must be boolean"));
        }
    }
    if input
        .get_property("use_current_cursor")
        .and_then(|value| value.as_bool())
        == Some(true)
        && ["coordinate", "x", "y"]
            .iter()
            .any(|k| input.get_property(k).is_some())
    {
        return Err(invalid("use_current_cursor conflicts with coordinate"));
    }
    if input.get_property("keys").is_some() && input.get_property("text").is_some() {
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
    if input.get_property("path").is_some() && action != "left_click_drag" {
        return Err(invalid("path is only supported for left_click_drag"));
    }
    if input.get_property("pixel_delta").is_some() && action != "scroll" {
        return Err(invalid("pixel_delta is only supported for scroll"));
    }
    if input.get_property("button").is_some() && action != "mouse_click" {
        return Err(invalid("button is only supported for mouse_click"));
    }
    if input.get_property("keys").is_some() && !matches!(action, "key" | "hold_key") {
        return Err(invalid("keys is only supported for key/hold_key"));
    }
    if input.get_property("press_enter").is_some() && action != "type" {
        return Err(invalid("press_enter is only supported for type"));
    }
    if input.get_property("use_current_cursor").is_some()
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
    if let Some(v) = input.get_property("coordinate") {
        validate::require_coord_view(v, "coordinate")?;
    }
    if input.get_property("x").is_some() || input.get_property("y").is_some() {
        validate::require_coord(input, "coordinate")?;
    }
    if input.get_property("start_coordinate").is_some() {
        validate::require_coord(input, "start_coordinate")?;
    }
    match action {
        "mouse_move" => {
            validate::require_coord(input, "coordinate")?;
        }
        "left_click" | "right_click" | "middle_click" | "double_click" | "triple_click"
        | "mouse_click" | "scroll" => {
            if input
                .get_property("use_current_cursor")
                .and_then(|value| value.as_bool())
                != Some(true)
            {
                validate::require_coord(input, "coordinate")?;
            }
            if action == "mouse_click"
                && !matches!(
                    input
                        .get_property("button")
                        .and_then(|value| value.as_str()),
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
            if input
                .get_property("bundle_id")
                .and_then(|value| value.as_str())
                .is_none()
            {
                return Err(invalid("bundle_id is required"));
            }
        }
        "switch_display" => {
            if input
                .get_property("display")
                .and_then(|value| value.as_str())
                .is_none()
            {
                return Err(invalid("display is required"));
            }
        }
        "request_access" => {
            if input.get_property("apps").is_some_and(|value| {
                value.array_len().is_none_or(|len| {
                    (0..len).any(|index| value.get_index(index).and_then(exact_string).is_none())
                })
            }) {
                return Err(invalid("apps must be an array of strings"));
            }
            if input
                .get_property("reason")
                .is_some_and(|value| value.as_str().is_some() && exact_string(value).is_none())
            {
                return Err(invalid("reason must be valid Unicode text"));
            }
            if input
                .get_property("tier")
                .is_some_and(|value| !matches!(value.as_str(), Some("read" | "click" | "full")))
            {
                return Err(invalid("tier must be read, click, or full"));
            }
        }
        _ => {}
    }
    Ok(())
}
