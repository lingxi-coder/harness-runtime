//! The page bridge: what an app's page may ask the host to do
//! (`window.lingxi.v2`), and how the host answers.
//!
//! The service interprets these and every host that serves a page carries
//! them, so they live here rather than in any one host's wire protocol. A host
//! maps them onto its own encoding at its edge; the JSON these types produce is
//! also what the page itself sees, so the serde attributes are part of the
//! contract and are pinned by the tests below.

use serde::{Deserialize, Serialize};

/// Operations accepted by the versioned `window.lingxi.v2` bridge.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum BridgeOperation {
    /// Read records from the app's own data store.
    QueryData,
    /// Change records in the app's own data store.
    MutateData,
    /// One outbound HTTP request to an authorized domain.
    NetworkRequest,
    /// The app's runtime state and preview address.
    RuntimeStatus,
    /// Capture one photo with the device camera (`device.capturePhoto`).
    CapturePhoto,
    /// Pick one image from the photo library (`device.pickImage`).
    PickImage,
    /// Start a microphone recording (`device.recordAudioStart`).
    RecordAudioStart,
    /// Stop the recording and return its bytes (`device.recordAudioStop`).
    RecordAudioStop,
    /// One-shot current location (`device.getLocation`).
    GetLocation,
    /// Listen once and return the transcript (`device.transcribeSpeech`).
    /// This is how speech reaches the model: no provider on this stack
    /// accepts raw audio in a messages call.
    TranscribeSpeech,
    /// Post a local notification (`device.postNotification`).
    PostNotification,
    /// Read plain text from the system clipboard (`clipboard.getText`).
    ClipboardGetText,
    /// Write plain text to the system clipboard (`clipboard.setText`).
    ClipboardSetText,
    /// Open the native share sheet (`device.share`).
    Share,
    /// Synthesize bounded text to audio (`device.synthesizeSpeech`).
    SynthesizeSpeech,
    /// Read an app-private file (`files.read`).
    FileRead,
    /// Write an app-private file (`files.write`).
    FileWrite,
    /// Read non-sensitive device status (`device.status`).
    DeviceStatus,
    /// Trigger one bounded haptic event (`device.haptics`).
    Haptics,
    /// Open one authorized external URL (`device.deepLink`).
    DeepLink,
    /// One side-query chat completion against the user's model (`llm.chat`).
    LlmChat,
    /// Stream one side-query chat completion through [`BridgeStreamFrame`].
    LlmStream,
    /// Post one event into the app's conversation mailbox (`agent.post`).
    AgentPost,
    /// Create a persistent app-owned Agent session.
    AgentSessionCreate,
    /// List persistent app-owned Agent sessions.
    AgentSessionList,
    /// Resume one persistent app-owned Agent session.
    AgentSessionResume,
    /// Close one persistent app-owned Agent session.
    AgentSessionClose,
    /// Run one non-streaming turn in an app-owned Agent session.
    AgentSend,
    /// Run one streaming turn in an app-owned Agent session.
    AgentStream,
    /// Cancel one in-flight app-owned Agent turn.
    AgentCancel,
    /// Propose a user-approved App Agent Profile revision.
    AgentProfileProposeUpdate,
    /// Register a declarative app flow with the host background scheduler.
    BackgroundSchedule,
    /// List app-owned background task lifecycle records.
    BackgroundList,
    /// Read one app-owned background task lifecycle record.
    BackgroundStatus,
    /// Cancel one app-owned background task.
    BackgroundCancel,
    /// Requeue one failed or cancelled app-owned background task.
    BackgroundRetry,
    /// List bounded calendar events (`calendar.listEvents`).
    CalendarListEvents,
    /// Search bounded contact projections (`contacts.search`).
    ContactsSearch,
    /// Read one media handle retained by this app (`media.get`).
    MediaGet,
}

/// One host-bound bridge request. Payloads are data, never executable script.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct BridgeRequest {
    /// The page's correlation id; echoed on the answer.
    pub request_id: String,
    /// The app the page belongs to. The host binds it, the page cannot choose it.
    pub app_id: String,
    /// What the page is asking for.
    pub operation: BridgeOperation,
    /// The operation's arguments as one JSON document, when it has any.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub payload_json: Option<String>,
}

/// The host's answer to a bridge request. JSON stays an opaque data string.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct BridgeResponse {
    /// The request this answers.
    pub request_id: String,
    /// The app the request came from.
    pub app_id: String,
    /// Whether the operation succeeded.
    pub ok: bool,
    /// The operation's result as one JSON document; present only on success.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub result_json: Option<String>,
    /// Human-readable failure text; present only on failure.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub error: Option<String>,
    /// Stable machine-readable failure code (`capability_not_declared`,
    /// `permission_denied`, `audio_session_busy`, `media_too_large`,
    /// `llm_busy`, …) so page code can branch without parsing `error` prose.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub error_code: Option<String>,
}

/// Ordered v2 stream frame used by LLM and Agent session streams.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(
    tag = "type",
    rename_all = "snake_case",
    rename_all_fields = "camelCase"
)]
pub enum BridgeStreamFrame {
    /// The stream opened.
    Started {
        /// The app the stream belongs to.
        app_id: String,
        /// The request that opened the stream.
        request_id: String,
        /// This stream's id.
        stream_id: String,
    },
    /// One chunk of output.
    Data {
        /// The app the stream belongs to.
        app_id: String,
        /// The request that opened the stream.
        request_id: String,
        /// This stream's id.
        stream_id: String,
        /// Position in the stream; frames are delivered in `seq` order.
        seq: u64,
        /// The chunk as one JSON document.
        data_json: String,
    },
    /// The stream finished normally.
    Completed {
        /// The app the stream belongs to.
        app_id: String,
        /// The request that opened the stream.
        request_id: String,
        /// This stream's id.
        stream_id: String,
        /// Position in the stream.
        seq: u64,
    },
    /// The stream failed.
    Error {
        /// The app the stream belongs to.
        app_id: String,
        /// The request that opened the stream.
        request_id: String,
        /// This stream's id.
        stream_id: String,
        /// Position in the stream.
        seq: u64,
        /// Stable machine-readable failure code.
        code: String,
        /// Human-readable failure text.
        message: String,
    },
    /// The stream was cancelled before it finished.
    Cancelled {
        /// The app the stream belongs to.
        app_id: String,
        /// The request that opened the stream.
        request_id: String,
        /// This stream's id.
        stream_id: String,
        /// Position in the stream.
        seq: u64,
        /// Why it stopped.
        reason: String,
    },
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn operations_travel_as_snake_case_names() {
        assert_eq!(
            serde_json::to_value(BridgeOperation::RecordAudioStart).unwrap(),
            json!("record_audio_start")
        );
        assert_eq!(
            serde_json::from_value::<BridgeOperation>(json!("agent_profile_propose_update"))
                .unwrap(),
            BridgeOperation::AgentProfileProposeUpdate
        );
        assert!(serde_json::from_value::<BridgeOperation>(json!("rm_rf")).is_err());
    }

    #[test]
    fn a_request_omits_an_absent_payload() {
        let request = BridgeRequest {
            request_id: "r1".into(),
            app_id: "notes".into(),
            operation: BridgeOperation::QueryData,
            payload_json: None,
        };
        assert_eq!(
            serde_json::to_value(&request).unwrap(),
            json!({"request_id": "r1", "app_id": "notes", "operation": "query_data"})
        );
        let with_payload = BridgeRequest {
            payload_json: Some("{}".into()),
            ..request
        };
        assert_eq!(
            serde_json::to_value(&with_payload).unwrap()["payload_json"],
            json!("{}")
        );
    }

    #[test]
    fn a_response_carries_a_code_only_when_it_failed() {
        let ok = BridgeResponse {
            request_id: "r1".into(),
            app_id: "notes".into(),
            ok: true,
            result_json: Some("{}".into()),
            error: None,
            error_code: None,
        };
        assert_eq!(
            serde_json::to_value(&ok).unwrap(),
            json!({"request_id": "r1", "app_id": "notes", "ok": true, "result_json": "{}"})
        );
        let failed = BridgeResponse {
            ok: false,
            result_json: None,
            error: Some("denied".into()),
            error_code: Some("permission_denied".into()),
            ..ok
        };
        let value = serde_json::to_value(&failed).unwrap();
        assert_eq!(value["error_code"], json!("permission_denied"));
        assert!(value.get("result_json").is_none());
    }

    #[test]
    fn stream_frames_are_tagged_and_camel_cased() {
        let frame = BridgeStreamFrame::Data {
            app_id: "notes".into(),
            request_id: "r1".into(),
            stream_id: "s1".into(),
            seq: 2,
            data_json: "\"hi\"".into(),
        };
        assert_eq!(
            serde_json::to_value(&frame).unwrap(),
            json!({
                "type": "data",
                "appId": "notes",
                "requestId": "r1",
                "streamId": "s1",
                "seq": 2,
                "dataJson": "\"hi\""
            })
        );
        let back: BridgeStreamFrame =
            serde_json::from_value(serde_json::to_value(&frame).unwrap()).unwrap();
        assert_eq!(back, frame);
    }
}
