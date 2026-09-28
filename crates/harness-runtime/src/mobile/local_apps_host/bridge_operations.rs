use super::public_ip;
use super::read_limited_stream;
use super::required_string;
use super::BridgeFailure;
use super::LocalAppsHostBroker;
use super::LOCAL_APP_BRIDGE_CONTROL_BYTES;
use super::LOCAL_APP_BRIDGE_FILE_BYTES;
use super::LOCAL_APP_BRIDGE_LLM_BYTES;
use super::MAX_NETWORK_RESPONSE_BYTES;
use super::UI_TIMEOUT;
use crate::mobile::local_apps_mcp::LocalAppsMcpHost;
use client_protocol::events::ClientEvent;
use client_protocol::local_apps::AppAuthorizationDecisionDto;
use client_protocol::local_apps::AppBridgeOperationDto;
use client_protocol::local_apps::AppBridgeRequestDto;
use client_protocol::local_apps::AppBridgeResponseDto;
use client_protocol::local_apps::AppEventDto;
use client_protocol::local_apps::AppUiRequestDto;
use local_apps::load_manifest;
use local_apps::load_permissions;
use serde_json::json;
use serde_json::Map;
use serde_json::Value;
use std::net::IpAddr;
use std::net::SocketAddr;
use tokio::sync::oneshot;
use tokio::time::timeout;
use tokio::time::Duration;

impl LocalAppsHostBroker {
    pub(super) async fn request_ui(&self, request: AppUiRequestDto) -> Result<Value, String> {
        let request_id = request.request_id.clone();
        let is_qa_request = request.request_id.starts_with("qa-ui-");
        let (sender, receiver) = oneshot::channel();
        self.pending_ui
            .lock()
            .await
            .insert(request_id.clone(), sender);
        self.event_sink
            .emit(ClientEvent::AppEvent {
                event: AppEventDto::AppUiRequest { request },
            })
            .await;
        let resolution = match timeout(UI_TIMEOUT, receiver).await {
            Ok(Ok(resolution)) => resolution,
            Ok(Err(_)) => return Err("WebView action was cancelled".into()),
            Err(_) => {
                self.pending_ui.lock().await.remove(&request_id);
                return Err("WebView action timed out".into());
            }
        };
        if matches!(resolution.decision, AppAuthorizationDecisionDto::Deny) {
            return Err("user denied the WebView action".into());
        }
        if let Some(error) = resolution.error {
            // Native QA clients preserve an authenticated envelope even when
            // the page action itself fails. Keep that envelope on the normal
            // result channel and localize the failure inside `result`; the
            // Host can then validate `lingxi_qa`, persist failed UiAction
            // evidence, and return its Host-issued evidence IDs. A transport
            // or identity failure has no such envelope and must remain a
            // top-level error.
            if is_qa_request {
                if let Some(result_json) = resolution.result_json {
                    if let Ok(Value::Object(mut envelope)) = serde_json::from_str(&result_json) {
                        if envelope.get("lingxi_qa").is_some() {
                            let failed_result = match envelope.remove("result") {
                                Some(Value::Object(mut result)) => {
                                    result.insert("ok".into(), Value::Bool(false));
                                    result.insert("error".into(), Value::String(error));
                                    Value::Object(result)
                                }
                                _ => json!({"ok": false, "error": error}),
                            };
                            envelope.insert("result".into(), failed_result);
                            return Ok(Value::Object(envelope));
                        }
                    }
                }
            }
            return Err(error);
        }
        let result = resolution.result_json.unwrap_or_else(|| "{}".into());
        serde_json::from_str(&result)
            .map_err(|error| format!("invalid WebView result JSON: {error}"))
    }
    pub(crate) async fn execute_bridge(&self, request: AppBridgeRequestDto) {
        let result = self.execute_bridge_inner(&request).await;
        let response = match result {
            Ok(value) => AppBridgeResponseDto {
                request_id: request.request_id,
                app_id: request.app_id,
                ok: true,
                result_json: Some(value.to_string()),
                error: None,
                error_code: None,
            },
            Err(failure) => AppBridgeResponseDto {
                request_id: request.request_id,
                app_id: request.app_id,
                ok: false,
                result_json: None,
                error: Some(failure.message),
                error_code: failure.code.map(str::to_string),
            },
        };
        self.event_sink
            .emit(ClientEvent::AppEvent {
                event: AppEventDto::AppBridgeResponse { response },
            })
            .await;
    }
    pub(super) async fn execute_bridge_inner(
        &self,
        request: &AppBridgeRequestDto,
    ) -> Result<Value, BridgeFailure> {
        // Track every page request, not just the eventual mutation: an async
        // click handler may await a query/device bridge before issuing its
        // persisted write. Only MutateData receives the scoped event id.
        let qa_bridge_guard = self.begin_qa_bridge_request(&request.app_id).await;
        let qa_action_event_id = qa_bridge_guard
            .as_ref()
            .map(|guard| guard.event_id().to_string());
        let qa_bridge_event_id = matches!(request.operation, AppBridgeOperationDto::MutateData)
            .then_some(qa_action_event_id.as_deref())
            .flatten();
        let result = self
            .execute_bridge_inner_scoped(request, qa_bridge_event_id)
            .await;
        // Dropping the guard synchronously settles the in-flight counter even
        // when the bridge future is cancelled before this point.
        drop(qa_bridge_guard);
        result
    }
    pub(super) async fn execute_bridge_inner_scoped(
        &self,
        request: &AppBridgeRequestDto,
        qa_bridge_event_id: Option<&str>,
    ) -> Result<Value, BridgeFailure> {
        let payload_json = request.payload_json.as_deref().unwrap_or("{}");
        let payload_limit = if matches!(
            request.operation,
            AppBridgeOperationDto::LlmChat | AppBridgeOperationDto::LlmStream
        ) {
            LOCAL_APP_BRIDGE_LLM_BYTES
        } else if matches!(
            request.operation,
            AppBridgeOperationDto::FileRead | AppBridgeOperationDto::FileWrite
        ) {
            LOCAL_APP_BRIDGE_FILE_BYTES
        } else {
            LOCAL_APP_BRIDGE_CONTROL_BYTES
        };
        if payload_json.len() > payload_limit {
            return Err(BridgeFailure::coded(
                "payload_too_large",
                format!(
                    "bridge payload is {} bytes; the limit for this operation is {payload_limit}",
                    payload_json.len()
                ),
            ));
        }
        let payload: Value = serde_json::from_str(payload_json).map_err(|error| {
            BridgeFailure::coded(
                "payload_invalid",
                format!("invalid bridge payload JSON: {error}"),
            )
        })?;
        // The page-facing DTO is intentionally small and legacy-compatible;
        // the v2 attribution context is created here, inside the trusted host,
        // before any capability handler runs. A page cannot manufacture its
        // origin, app instance, or grant epoch.
        let invocation_context = self
            .build_bridge_invocation_context(request)
            .map_err(BridgeFailure::from)?;
        let mut input = payload.as_object().cloned().ok_or_else(|| {
            BridgeFailure::coded("payload_invalid", "bridge payload must be a JSON object")
        })?;
        input.insert("app_id".into(), Value::String(request.app_id.clone()));
        match request.operation {
            AppBridgeOperationDto::QueryData => self
                .query_data_value(Value::Object(input))
                .await
                .map_err(BridgeFailure::from),
            // The page is acting for the foreground user, not an agent.  Its
            // app id is host-bound and the manifest still constrains writes.
            AppBridgeOperationDto::MutateData => self
                .mutate_data_value(Value::Object(input), false, qa_bridge_event_id)
                .await
                .map_err(BridgeFailure::from),
            AppBridgeOperationDto::RuntimeStatus => {
                let runtime = self
                    .service()?
                    .runtime_record(&request.app_id)
                    .await
                    .map_err(|error| error.to_string())?;
                Ok(json!(runtime))
            }
            AppBridgeOperationDto::NetworkRequest => self
                .network_request(&request.app_id, Value::Object(input))
                .await
                .map_err(BridgeFailure::from),
            AppBridgeOperationDto::CapturePhoto => {
                self.capture_photo_value(&request.app_id, &payload).await
            }
            AppBridgeOperationDto::PickImage => {
                self.pick_image_value(&request.app_id, &payload).await
            }
            AppBridgeOperationDto::RecordAudioStart => {
                let runtime_generation = self.audio_runtime_generation(&invocation_context).await?;
                self.record_audio_start_value(&invocation_context, runtime_generation, &payload)
                    .await
            }
            AppBridgeOperationDto::RecordAudioStop => {
                let runtime_generation = self.audio_runtime_generation(&invocation_context).await?;
                self.record_audio_stop_value(&invocation_context, runtime_generation)
                    .await
            }
            AppBridgeOperationDto::GetLocation => self.get_location_value(&request.app_id).await,
            AppBridgeOperationDto::PostNotification => {
                self.post_notification_value(&request.app_id, &payload)
                    .await
            }
            AppBridgeOperationDto::TranscribeSpeech => {
                let runtime_generation = self.audio_runtime_generation(&invocation_context).await?;
                self.transcribe_speech_value(&invocation_context, runtime_generation, &payload)
                    .await
            }
            AppBridgeOperationDto::ClipboardGetText => {
                self.clipboard_get_text_value(&request.app_id).await
            }
            AppBridgeOperationDto::ClipboardSetText => {
                self.clipboard_set_text_value(&request.app_id, &payload)
                    .await
            }
            AppBridgeOperationDto::Share => self.share_value(&request.app_id, &payload).await,
            AppBridgeOperationDto::SynthesizeSpeech => {
                let runtime_generation = self.audio_runtime_generation(&invocation_context).await?;
                self.synthesize_speech_value(&invocation_context, runtime_generation, &payload)
                    .await
            }
            AppBridgeOperationDto::FileRead => {
                self.file_read_value(&request.app_id, &payload).await
            }
            AppBridgeOperationDto::FileWrite => {
                self.file_write_value(&request.app_id, &payload).await
            }
            AppBridgeOperationDto::DeviceStatus => self.device_status_value(&request.app_id).await,
            AppBridgeOperationDto::Haptics => self.haptics_value(&request.app_id, &payload).await,
            AppBridgeOperationDto::DeepLink => {
                self.deep_link_value(&request.app_id, &payload).await
            }
            AppBridgeOperationDto::CalendarListEvents => {
                self.calendar_list_events_value(&request.app_id, &payload)
                    .await
            }
            AppBridgeOperationDto::ContactsSearch => {
                self.contacts_search_value(&request.app_id, &payload).await
            }
            AppBridgeOperationDto::MediaGet => self.media_value(&request.app_id, &payload).await,
            AppBridgeOperationDto::LlmChat => self.llm_chat_value(&request.app_id, &payload).await,
            AppBridgeOperationDto::LlmStream => {
                self.llm_stream_value(&request.app_id, &request.request_id, &payload)
                    .await
            }
            AppBridgeOperationDto::AgentPost => {
                self.agent_post_value(&request.app_id, &payload).await
            }
            AppBridgeOperationDto::AgentSessionCreate => self
                .agent_session_create_value(Value::Object(input))
                .await
                .map_err(BridgeFailure::from),
            AppBridgeOperationDto::AgentSessionList => self
                .agent_session_list_value(Value::Object(input))
                .await
                .map_err(BridgeFailure::from),
            AppBridgeOperationDto::AgentSessionResume => {
                input.insert("action".into(), Value::String("resume".into()));
                self.agent_session_update_value(Value::Object(input))
                    .await
                    .map_err(BridgeFailure::from)
            }
            AppBridgeOperationDto::AgentSessionClose => {
                input.insert("action".into(), Value::String("close".into()));
                self.agent_session_update_value(Value::Object(input))
                    .await
                    .map_err(BridgeFailure::from)
            }
            AppBridgeOperationDto::AgentSend => {
                self.agent_send_value(&request.app_id, &request.request_id, &payload)
                    .await
            }
            AppBridgeOperationDto::AgentStream => {
                self.agent_stream_value(&request.app_id, &request.request_id, &payload)
                    .await
            }
            AppBridgeOperationDto::AgentCancel => {
                self.agent_cancel_value(&request.app_id, &payload).await
            }
            AppBridgeOperationDto::AgentProfileProposeUpdate => self
                .agent_profile_propose_value(Value::Object(input))
                .await
                .map_err(BridgeFailure::from),
            AppBridgeOperationDto::BackgroundSchedule => self
                .background_schedule_value(Value::Object(input))
                .await
                .map_err(BridgeFailure::from),
            AppBridgeOperationDto::BackgroundList => self
                .background_list_value(Value::Object(input))
                .await
                .map_err(BridgeFailure::from),
            AppBridgeOperationDto::BackgroundStatus => self
                .background_status_value(Value::Object(input))
                .await
                .map_err(BridgeFailure::from),
            AppBridgeOperationDto::BackgroundCancel => self
                .background_cancel_value(Value::Object(input))
                .await
                .map_err(BridgeFailure::from),
            AppBridgeOperationDto::BackgroundRetry => self
                .background_retry_value(Value::Object(input))
                .await
                .map_err(BridgeFailure::from),
            _ => Err("unsupported bridge operation for this engine version".into()),
        }
    }
    pub(super) fn build_bridge_invocation_context(
        &self,
        request: &AppBridgeRequestDto,
    ) -> Result<local_apps::InvocationContext, String> {
        let capability = match request.operation {
            AppBridgeOperationDto::QueryData => local_apps::CapabilityId::DataQuery,
            AppBridgeOperationDto::MutateData => local_apps::CapabilityId::DataMutate,
            AppBridgeOperationDto::NetworkRequest => local_apps::CapabilityId::NetworkRequest,
            AppBridgeOperationDto::RuntimeStatus => local_apps::CapabilityId::RuntimeStatus,
            AppBridgeOperationDto::CapturePhoto => local_apps::CapabilityId::Camera,
            AppBridgeOperationDto::PickImage => local_apps::CapabilityId::PhotoLibrary,
            AppBridgeOperationDto::RecordAudioStart | AppBridgeOperationDto::RecordAudioStop => {
                local_apps::CapabilityId::Microphone
            }
            AppBridgeOperationDto::GetLocation => local_apps::CapabilityId::Location,
            AppBridgeOperationDto::TranscribeSpeech => local_apps::CapabilityId::SpeechToText,
            AppBridgeOperationDto::PostNotification => local_apps::CapabilityId::Notifications,
            AppBridgeOperationDto::ClipboardGetText | AppBridgeOperationDto::ClipboardSetText => {
                local_apps::CapabilityId::Clipboard
            }
            AppBridgeOperationDto::Share => local_apps::CapabilityId::Share,
            AppBridgeOperationDto::SynthesizeSpeech => local_apps::CapabilityId::TextToSpeech,
            AppBridgeOperationDto::FileRead => local_apps::CapabilityId::FilesRead,
            AppBridgeOperationDto::FileWrite => local_apps::CapabilityId::FilesWrite,
            AppBridgeOperationDto::DeviceStatus => local_apps::CapabilityId::DeviceStatus,
            AppBridgeOperationDto::Haptics => local_apps::CapabilityId::Haptics,
            AppBridgeOperationDto::DeepLink => local_apps::CapabilityId::DeepLink,
            AppBridgeOperationDto::CalendarListEvents => local_apps::CapabilityId::Calendar,
            AppBridgeOperationDto::ContactsSearch => local_apps::CapabilityId::Contacts,
            AppBridgeOperationDto::MediaGet => local_apps::CapabilityId::Media,
            AppBridgeOperationDto::LlmChat => local_apps::CapabilityId::LlmComplete,
            AppBridgeOperationDto::LlmStream => local_apps::CapabilityId::LlmStream,
            AppBridgeOperationDto::AgentPost => local_apps::CapabilityId::AgentEmit,
            AppBridgeOperationDto::AgentSessionCreate => {
                local_apps::CapabilityId::AgentSessionCreate
            }
            AppBridgeOperationDto::AgentSessionList => local_apps::CapabilityId::AgentSessionList,
            AppBridgeOperationDto::AgentSessionResume => {
                local_apps::CapabilityId::AgentSessionResume
            }
            AppBridgeOperationDto::AgentSessionClose => local_apps::CapabilityId::AgentSessionClose,
            AppBridgeOperationDto::AgentSend => local_apps::CapabilityId::AgentSend,
            AppBridgeOperationDto::AgentStream => local_apps::CapabilityId::AgentStream,
            AppBridgeOperationDto::AgentCancel => local_apps::CapabilityId::AgentCancel,
            AppBridgeOperationDto::AgentProfileProposeUpdate => {
                local_apps::CapabilityId::AgentProfilePropose
            }
            AppBridgeOperationDto::BackgroundSchedule => {
                local_apps::CapabilityId::BackgroundSchedule
            }
            AppBridgeOperationDto::BackgroundList
            | AppBridgeOperationDto::BackgroundStatus
            | AppBridgeOperationDto::BackgroundCancel
            | AppBridgeOperationDto::BackgroundRetry => {
                local_apps::CapabilityId::BackgroundSchedule
            }
            _ => return Err("unsupported bridge operation for runtime v2 context".into()),
        };
        let layout = self.layout(&request.app_id)?;
        let manifest = load_manifest(&layout).map_err(|error| error.to_string())?;
        if !manifest.runtime_api_compatible() {
            return Err(format!(
                "runtime_api_incompatible: app manifest targets runtime API v{}",
                manifest.runtime_api_version
            ));
        }
        let permissions = load_permissions(&layout).map_err(|error| error.to_string())?;
        let context = local_apps::InvocationContext {
            app_id: request.app_id.clone(),
            app_instance_id: format!("page-{}", request.app_id),
            request_id: request.request_id.clone(),
            turn_id: None,
            origin: local_apps::InvocationOrigin::PageForeground,
            grant_epoch: permissions.grant_epoch,
            capability_instance: Some(format!("{}:{}", request.app_id, capability.as_str())),
            call_chain: Vec::new(),
        };
        context.validate().map_err(|error| error.to_string())?;
        Ok(context)
    }
    pub(super) async fn network_request(
        &self,
        app_id: &str,
        input: Value,
    ) -> Result<Value, String> {
        let url_text = required_string(&input, "url")?;
        let url = reqwest::Url::parse(url_text).map_err(|error| format!("invalid URL: {error}"))?;
        if url.scheme() != "https" || url.username() != "" || url.password().is_some() {
            return Err(
                "network bridge accepts plain HTTPS URLs without embedded credentials".into(),
            );
        }
        let domain = url
            .host_str()
            .ok_or_else(|| "network URL has no hostname".to_string())?;
        if domain == "localhost" || !domain.contains('.') || domain.parse::<IpAddr>().is_ok() {
            return Err("network bridge requires a public DNS hostname".into());
        }
        self.authorize_domain(app_id, domain).await?;
        let port = url.port_or_known_default().unwrap_or(443);
        let resolved: Vec<SocketAddr> = tokio::net::lookup_host((domain, port))
            .await
            .map_err(|error| format!("resolve network domain: {error}"))?
            .collect();
        if resolved.is_empty() || resolved.iter().any(|address| !public_ip(address.ip())) {
            return Err("network domain resolved to a private, local, or invalid address".into());
        }
        let method = input
            .get("method")
            .and_then(Value::as_str)
            .unwrap_or("GET")
            .to_ascii_uppercase();
        if !matches!(method.as_str(), "GET" | "POST" | "PUT" | "PATCH" | "DELETE") {
            return Err("network method must be GET, POST, PUT, PATCH, or DELETE".into());
        }
        let client = reqwest::Client::builder()
            .redirect(reqwest::redirect::Policy::none())
            .timeout(Duration::from_secs(30))
            .resolve_to_addrs(domain, &resolved)
            .build()
            .map_err(|error| format!("create network client: {error}"))?;
        let mut builder = client.request(method.parse().map_err(|_| "invalid HTTP method")?, url);
        if let Some(headers) = input.get("headers").and_then(Value::as_object) {
            if headers.len() > 32 {
                return Err("network request has more than 32 headers".into());
            }
            for (name, value) in headers {
                let Some(value) = value.as_str() else {
                    return Err("network header values must be strings".into());
                };
                let lower = name.to_ascii_lowercase();
                if matches!(
                    lower.as_str(),
                    "host" | "cookie" | "authorization" | "proxy-authorization"
                ) {
                    return Err(format!("network header {name:?} is reserved"));
                }
                builder = builder.header(name, value);
            }
        }
        if let Some(body) = input.get("body") {
            let encoded = if let Some(text) = body.as_str() {
                text.as_bytes().to_vec()
            } else {
                serde_json::to_vec(body).map_err(|error| error.to_string())?
            };
            if encoded.len() > 1024 * 1024 {
                return Err("network request body exceeds 1 MiB".into());
            }
            builder = builder.body(encoded);
        }
        let response = builder
            .send()
            .await
            .map_err(|error| format!("network request failed: {error}"))?;
        let status = response.status().as_u16();
        let headers: Map<String, Value> = response
            .headers()
            .iter()
            .filter_map(|(name, value)| {
                value
                    .to_str()
                    .ok()
                    .map(|value| (name.to_string(), Value::String(value.to_string())))
            })
            .collect();
        let bytes = read_limited_stream(
            response.bytes_stream(),
            MAX_NETWORK_RESPONSE_BYTES,
            "read network response",
            "network response exceeds 2 MiB",
        )
        .await?;
        Ok(json!({
            "status": status,
            "headers": headers,
            "body": String::from_utf8_lossy(&bytes),
        }))
    }
}
