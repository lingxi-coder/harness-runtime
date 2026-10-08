//! Remote UI operations share the current Mod session and attached roster.
use super::*;
use futures::{stream::FuturesUnordered, StreamExt};
use lingxi_core::host::{ModRemoteUiAnswer, ModRemoteUiClient, ModRemoteUiHost};
use lingxi_core::types::utf16_json::Utf16JsonProjection;
use serde_json::{json, Value};

impl ConversationOrchestrator {
    pub fn set_remote_ui_host(&self, host: Arc<dyn ModRemoteUiHost>) {
        *self
            .prompt_runtime
            .remote_ui_host
            .write()
            .unwrap_or_else(std::sync::PoisonError::into_inner) = Some(host);
    }
    pub(crate) fn remote_ui_host(&self) -> Option<Arc<dyn ModRemoteUiHost>> {
        self.prompt_runtime
            .remote_ui_host
            .read()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .clone()
    }
    pub fn detach_sdk_ui_client(&self, client: &str) {
        if let Some(host) = self.remote_ui_host() {
            host.detach(client);
        }
    }
    pub async fn remote_mod_ui_operation(
        &self,
        event: &str,
        input: Utf16JsonProjection,
        plugin: &str,
    ) -> Result<Utf16JsonProjection, hooks::mods::ModError> {
        input
            .validate()
            .map_err(|error| hooks::mods::ModError::Protocol(error.to_string()))?;
        let host = self.remote_ui_host();
        let answer = match event {
            "ui.copy" => ModRemoteUiAnswer::Copy,
            "prompt.read" => ModRemoteUiAnswer::PromptRead,
            "prompt.fill" => ModRemoteUiAnswer::PromptFill,
            "prompt.suggest" => ModRemoteUiAnswer::PromptSuggest,
            "ui.selection" => ModRemoteUiAnswer::ReadSelection,
            _ => {
                return Err(hooks::mods::ModError::Protocol(format!(
                    "Unknown remote UI operation: {event}"
                )))
            }
        };
        if answer == ModRemoteUiAnswer::ReadSelection {
            if let Some(host) = host {
                let mut replies = FuturesUnordered::new();
                for client in host.clients(answer) {
                    let host = host.clone();
                    let request = remote_ui_request(answer, &client, &input, plugin)?;
                    replies.push(async move { host.request(request).await });
                }
                while let Some(reply) = replies.next().await {
                    let Some(reply) = reply else {
                        continue;
                    };
                    if reply
                        .value
                        .get("text")
                        .and_then(Value::as_str)
                        .is_some_and(|text| !text.is_empty())
                    {
                        let mut selection = Utf16JsonProjection::plain(json!({}));
                        selection
                            .set_field(
                                "text",
                                reply.subprojection("/text").map_err(projection_error)?,
                            )
                            .map_err(projection_error)?;
                        if reply
                            .value
                            .get("instance_id")
                            .and_then(Value::as_str)
                            .is_some_and(|id| !id.is_empty())
                        {
                            selection
                                .set_field(
                                    "requestId",
                                    reply
                                        .subprojection("/instance_id")
                                        .map_err(projection_error)?,
                                )
                                .map_err(projection_error)?;
                        }
                        return Ok(selection);
                    }
                }
            }
            let selection = self
                .mod_ui_selection
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner)
                .as_ref()
                .map(|selection| {
                    let mut value = json!({"text":selection.text});
                    if let Some(id) = &selection.request_id {
                        value["requestId"] = json!(id);
                    }
                    value
                })
                .unwrap_or(Value::Null);
            return Ok(Utf16JsonProjection::plain(selection));
        }
        let surfaces = hooks::mods::ModSessionContext::surfaces(self);
        let surface = input
            .value
            .get("surface")
            .and_then(Value::as_str)
            .or_else(|| surfaces.first().map(String::as_str));
        if answer == ModRemoteUiAnswer::Copy
            && !surface.is_some_and(|surface| surfaces.iter().any(|entry| entry == surface))
        {
            return Ok(Utf16JsonProjection::plain(
                json!({"isCopied":false,"reason":"no-surface"}),
            ));
        }
        let text_length = input
            .string_units("/text")
            .map(|text| text.len())
            .unwrap_or_default();
        let mut clients = host
            .as_ref()
            .map(|host| host.clients(answer))
            .unwrap_or_default();
        if answer == ModRemoteUiAnswer::Copy {
            clients.retain(|client| Some(client.surface.as_str()) == surface);
        }
        // Native local stdio reaches the first matching client. Selection
        // uses its separate fan-out above; remote-host reach is not this lane.
        clients.truncate(1);
        let no_composer = answer == ModRemoteUiAnswer::PromptFill && clients.is_empty();
        let mut reply = None;
        if text_length <= 1_000_000 {
            if let Some(host) = &host {
                for client in clients {
                    let request = remote_ui_request(answer, &client, &input, plugin)?;
                    if let Some(answer) = host.request(request).await {
                        reply = Some(answer);
                        break;
                    }
                }
            }
        }
        match answer {
            ModRemoteUiAnswer::Copy => Ok(Utf16JsonProjection::plain(
                if reply
                    .as_ref()
                    .and_then(|reply| reply.value.get("copied"))
                    .and_then(Value::as_bool)
                    == Some(true)
                {
                    json!({"isCopied":true})
                } else {
                    json!({"isCopied":false,"reason":"no-clipboard"})
                },
            )),
            ModRemoteUiAnswer::PromptRead => prompt_box(reply),
            ModRemoteUiAnswer::PromptFill => {
                let filled = reply
                    .as_ref()
                    .and_then(|reply| reply.value.get("filled"))
                    .and_then(Value::as_bool)
                    == Some(true);
                let mut response = Utf16JsonProjection::plain(json!({"isFilled":filled}));
                if no_composer {
                    response.value["refusal"] = json!("no_composer");
                }
                Ok(response)
            }
            ModRemoteUiAnswer::PromptSuggest => Ok(Utf16JsonProjection::plain(
                json!({"isShown":reply.as_ref().and_then(|reply| reply.value.get("shown")).and_then(Value::as_bool)==Some(true)}),
            )),
            ModRemoteUiAnswer::ReadSelection => unreachable!(),
        }
    }
}

fn projection_error(error: impl std::fmt::Display) -> hooks::mods::ModError {
    hooks::mods::ModError::Protocol(error.to_string())
}
fn prompt_box(
    reply: Option<Utf16JsonProjection>,
) -> Result<Utf16JsonProjection, hooks::mods::ModError> {
    let Some(reply) = reply else {
        return Ok(Utf16JsonProjection::plain(json!({"text":"","cursor":0})));
    };
    let text = reply.subprojection("/text").map_err(projection_error)?;
    let length = text
        .string_units("")
        .map(|text| text.len())
        .unwrap_or_default();
    let cursor = reply
        .value
        .get("cursor")
        .and_then(Value::as_f64)
        .unwrap_or_default()
        .clamp(0.0, length as f64) as u64;
    let mut response = Utf16JsonProjection::plain(json!({}));
    response.set_field("text", text).map_err(projection_error)?;
    response.value["cursor"] = json!(cursor);
    Ok(response)
}
fn remote_ui_request(
    answer: ModRemoteUiAnswer,
    client: &ModRemoteUiClient,
    input: &Utf16JsonProjection,
    plugin: &str,
) -> Result<Utf16JsonProjection, hooks::mods::ModError> {
    let mut request = Utf16JsonProjection::plain(
        json!({"subtype":answer.subtype(),"surface":client.surface,"client_id":client.client_id}),
    );
    if answer == ModRemoteUiAnswer::Copy {
        request.value["plugin"] = json!(plugin);
    }
    if matches!(
        answer,
        ModRemoteUiAnswer::Copy | ModRemoteUiAnswer::PromptFill | ModRemoteUiAnswer::PromptSuggest
    ) {
        request
            .set_field(
                "text",
                input.subprojection("/text").map_err(projection_error)?,
            )
            .map_err(projection_error)?;
    }
    if answer == ModRemoteUiAnswer::PromptFill {
        request.value["mode"] = input
            .value
            .get("mode")
            .cloned()
            .unwrap_or_else(|| json!("replace"));
        if input
            .value
            .get("decorations")
            .and_then(Value::as_array)
            .is_some_and(|items| !items.is_empty())
        {
            request
                .set_field(
                    "decorations",
                    input
                        .subprojection("/decorations")
                        .map_err(projection_error)?,
                )
                .map_err(projection_error)?;
        }
    }
    Ok(request)
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn callback_requests_preserve_text_units_and_native_copy_field_order() {
        let input = Utf16JsonProjection::parse(r#"{"text":"\ud800"}"#).unwrap();
        let client = ModRemoteUiClient {
            client_id: "phone".into(),
            surface: "mobile".into(),
            answers: vec![ModRemoteUiAnswer::Copy],
        };
        let request =
            remote_ui_request(ModRemoteUiAnswer::Copy, &client, &input, "fixture").unwrap();
        assert_eq!(
            request.to_json_string().unwrap(),
            r#"{"subtype":"ui_copy","surface":"mobile","client_id":"phone","plugin":"fixture","text":"\ud800"}"#
        );
    }
    #[test]
    fn prompt_cursor_clamps_to_exact_utf16_length_and_accepts_integer_float() {
        let reply = Utf16JsonProjection::parse(r#"{"text":"\ud800😀","cursor":99.0}"#).unwrap();
        let response = prompt_box(Some(reply)).unwrap();
        assert_eq!(
            response.string_units("/text"),
            Some(vec![0xd800, 0xd83d, 0xde00])
        );
        assert_eq!(response.value["cursor"], 3);
        assert_eq!(
            prompt_box(None).unwrap().value,
            json!({"text":"","cursor":0})
        );
    }
}
