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
                        .string_units("/text")
                        .is_some_and(|text| !text.is_empty())
                    {
                        let mut selection = generated_source(json!({}));
                        selection
                            .set_field(
                                "text",
                                reply.subprojection("/text").map_err(projection_error)?,
                            )
                            .map_err(projection_error)?;
                        if reply
                            .string_units("/instance_id")
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
                    let mut fields = vec![("text".into(), selection.text.clone().into())];
                    if let Some(id) = &selection.request_id {
                        fields.push(("requestId".into(), id.as_str().into()));
                    }
                    Utf16JsonProjection::object(fields)
                        .expect("terminal selection fields form a JSON object")
                })
                .unwrap_or_else(|| Utf16JsonProjection::parse("null").expect("null is valid JSON"));
            return Ok(selection);
        }
        let surfaces = hooks::mods::ModSessionContext::surfaces(self);
        let requested_surface = input.string_units("/surface");
        let surface = requested_surface.or_else(|| {
            surfaces
                .first()
                .map(|surface| surface.encode_utf16().collect::<Vec<_>>())
        });
        if answer == ModRemoteUiAnswer::Copy
            && !surface.as_deref().is_some_and(|surface| {
                surfaces
                    .iter()
                    .any(|entry| entry.encode_utf16().eq(surface.iter().copied()))
            })
        {
            return Ok(generated_source(
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
            clients.retain(|client| {
                Some(client.surface.encode_utf16().collect::<Vec<_>>().as_slice())
                    == surface.as_deref()
            });
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
            ModRemoteUiAnswer::Copy => Ok(generated_source(
                if reply
                    .as_ref()
                    .and_then(|reply| reply.at_pointer("/copied"))
                    .and_then(|value| value.as_bool())
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
                    .and_then(|reply| reply.at_pointer("/filled"))
                    .and_then(|value| value.as_bool())
                    == Some(true);
                let mut fields = vec![("isFilled".into(), bool_source(filled))];
                if no_composer {
                    fields.push(("refusal".into(), "no_composer".into()));
                }
                Utf16JsonProjection::object(fields).map_err(projection_error)
            }
            ModRemoteUiAnswer::PromptSuggest => {
                let shown = reply
                    .as_ref()
                    .and_then(|reply| reply.at_pointer("/shown"))
                    .and_then(|value| value.as_bool())
                    == Some(true);
                Utf16JsonProjection::object(vec![("isShown".into(), bool_source(shown))])
                    .map_err(projection_error)
            }
            ModRemoteUiAnswer::ReadSelection => unreachable!(),
        }
    }
}

fn projection_error(error: impl std::fmt::Display) -> hooks::mods::ModError {
    hooks::mods::ModError::Protocol(error.to_string())
}

fn generated_source(value: Value) -> Utf16JsonProjection {
    Utf16JsonProjection::from_serde_value(&value).expect("generated UI JSON is valid")
}

fn bool_source(value: bool) -> Utf16JsonProjection {
    Utf16JsonProjection::parse(if value { "true" } else { "false" }).expect("boolean is valid JSON")
}
fn prompt_box(
    reply: Option<Utf16JsonProjection>,
) -> Result<Utf16JsonProjection, hooks::mods::ModError> {
    let Some(reply) = reply else {
        return Ok(generated_source(json!({"text":"","cursor":0})));
    };
    let text = reply.subprojection("/text").map_err(projection_error)?;
    let length = text
        .string_units("")
        .map(|text| text.len())
        .unwrap_or_default();
    let cursor = reply
        .number_at("/cursor")
        .unwrap_or_default()
        .clamp(0.0, length as f64) as u64;
    Utf16JsonProjection::object(vec![
        ("text".into(), text),
        ("cursor".into(), generated_source(json!(cursor))),
    ])
    .map_err(projection_error)
}
fn remote_ui_request(
    answer: ModRemoteUiAnswer,
    client: &ModRemoteUiClient,
    input: &Utf16JsonProjection,
    plugin: &str,
) -> Result<Utf16JsonProjection, hooks::mods::ModError> {
    let mut fields = vec![
        ("subtype".into(), answer.subtype().into()),
        ("surface".into(), client.surface.as_str().into()),
        ("client_id".into(), client.client_id.as_str().into()),
    ];
    if answer == ModRemoteUiAnswer::Copy {
        fields.push(("plugin".into(), plugin.into()));
    }
    if matches!(
        answer,
        ModRemoteUiAnswer::Copy | ModRemoteUiAnswer::PromptFill | ModRemoteUiAnswer::PromptSuggest
    ) {
        fields.push((
            "text".into(),
            input.subprojection("/text").map_err(projection_error)?,
        ));
    }
    if answer == ModRemoteUiAnswer::PromptFill {
        fields.push((
            "mode".into(),
            if input.contains_property("mode") {
                input.subprojection("/mode").map_err(projection_error)?
            } else {
                "replace".into()
            },
        ));
        if input.array_len("/decorations").is_some_and(|len| len > 0) {
            fields.push((
                "decorations".into(),
                input
                    .subprojection("/decorations")
                    .map_err(projection_error)?,
            ));
        }
    }
    Utf16JsonProjection::object(fields).map_err(projection_error)
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
    fn prompt_fill_request_preserves_mode_and_decoration_source_units() {
        let input = Utf16JsonProjection::parse(
            r#"{"text":"body","mode":"\ud800","decorations":[{"label":"\ud800"}]}"#,
        )
        .unwrap();
        let client = ModRemoteUiClient {
            client_id: "desktop".into(),
            surface: "desktop".into(),
            answers: vec![ModRemoteUiAnswer::PromptFill],
        };
        let request =
            remote_ui_request(ModRemoteUiAnswer::PromptFill, &client, &input, "fixture").unwrap();
        assert_eq!(
            request.to_json_string().unwrap(),
            r#"{"subtype":"ui_prompt_fill","surface":"desktop","client_id":"desktop","text":"body","mode":"\ud800","decorations":[{"label":"\ud800"}]}"#
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
        assert_eq!(response.number_at("/cursor"), Some(3.0));
        let empty = prompt_box(None).unwrap();
        assert_eq!(empty.string_at("/text"), Some(""));
        assert_eq!(empty.number_at("/cursor"), Some(0.0));
    }
}
