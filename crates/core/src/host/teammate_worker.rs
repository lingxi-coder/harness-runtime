//! Private host transport for a teammate running in a terminal pane.
//! Prompt/context travel over a private manifest, never shell arguments.
use crate::types::utf16_json::Utf16JsonProjection;
use async_trait::async_trait;
use serde::de::{DeserializeOwned, MapAccess, Visitor};
use serde::{Deserialize, Deserializer, Serialize};
use serde_json::value::RawValue;
use std::collections::{HashMap, HashSet};
use std::fmt;
use std::path::PathBuf;

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct PaneTeammateManifest {
    pub socket_path: PathBuf,
    pub token: String,
    pub agent_id: crate::types::AgentId,
    pub name: String,
    pub team_name: String,
    pub parent_session_id: crate::types::SessionId,
    pub request: crate::host::subagent_spawn::SubagentSpawnRequest,
}

#[derive(Debug, Clone, Serialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum WorkerToParent {
    Hello {
        token: String,
    },
    Ready {
        task_id: String,
        selection: crate::host::team_spawn::TeammateModelSelection,
    },
    SendMessage {
        id: u64,
        #[serde(with = "crate::types::utf16_json::raw_json_serde")]
        input: Utf16JsonProjection,
    },
    State {
        status: String,
        error: Option<String>,
        awaiting_plan_approval: bool,
    },
    Output {
        #[serde(with = "crate::types::utf16_json::raw_json_serde")]
        text: Utf16JsonProjection,
    },
    CoordinatorMessage {
        #[serde(with = "crate::types::utf16_json::raw_json_serde")]
        message: Utf16JsonProjection,
    },
}

#[derive(Debug, Clone, Serialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum ParentToWorker {
    Message {
        #[serde(with = "crate::types::utf16_json::raw_json_serde")]
        text: Utf16JsonProjection,
    },
    PlanApprovalResponse {
        response: crate::host::teammate_plan::PlanApprovalResponse,
    },
    SendMessageResult {
        id: u64,
        #[serde(with = "crate::types::utf16_json::raw_json_serde")]
        result: Utf16JsonProjection,
        is_error: bool,
    },
    Shutdown,
}

#[derive(Debug, Clone)]
pub struct PaneMessageResult {
    pub result: Utf16JsonProjection,
    pub is_error: bool,
}

/// Forward the complete SendMessage operation to its owning session.
#[async_trait]
pub trait PaneMessageForwarder: Send + Sync {
    async fn send_message(&self, input: Utf16JsonProjection) -> Result<PaneMessageResult, String>;
}

/// Capture each frame member as JSON source before selecting its tagged
/// variant. Serde's internally tagged enum deserializer buffers members into
/// `Content`, which cannot preserve `RawValue` payloads containing lone UTF-16
/// surrogates for `Utf16JsonProjection`.
struct RawFrame {
    fields: HashMap<String, Box<RawValue>>,
    duplicates: HashSet<String>,
}

impl<'de> Deserialize<'de> for RawFrame {
    fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
    where
        D: Deserializer<'de>,
    {
        struct RawFrameVisitor;

        impl<'de> Visitor<'de> for RawFrameVisitor {
            type Value = RawFrame;

            fn expecting(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
                formatter.write_str("a tagged teammate frame object")
            }

            fn visit_map<A>(self, mut map: A) -> Result<Self::Value, A::Error>
            where
                A: MapAccess<'de>,
            {
                let mut fields = HashMap::new();
                let mut duplicates = HashSet::new();
                while let Some(key) = map.next_key::<String>()? {
                    // Keep the exact JSON spelling of the value until a
                    // variant is known. JSON object duplicate keys use the
                    // same last-member-wins behavior as the source parser;
                    // known DTO fields are checked after variant selection.
                    if fields
                        .insert(key.clone(), map.next_value::<Box<RawValue>>()?)
                        .is_some()
                    {
                        duplicates.insert(key);
                    }
                }
                Ok(RawFrame { fields, duplicates })
            }
        }

        deserializer.deserialize_map(RawFrameVisitor)
    }
}

fn raw_required<T, E>(
    fields: &mut HashMap<String, Box<RawValue>>,
    name: &'static str,
) -> Result<T, E>
where
    T: DeserializeOwned,
    E: serde::de::Error,
{
    let raw = fields.remove(name).ok_or_else(|| E::missing_field(name))?;
    serde_json::from_str(raw.get()).map_err(E::custom)
}

fn raw_optional<T, E>(
    fields: &mut HashMap<String, Box<RawValue>>,
    name: &'static str,
) -> Result<Option<T>, E>
where
    T: DeserializeOwned,
    E: serde::de::Error,
{
    fields
        .remove(name)
        .map(|raw| serde_json::from_str::<Option<T>>(raw.get()).map_err(E::custom))
        .transpose()
        .map(Option::flatten)
}

fn raw_projection<E>(
    fields: &mut HashMap<String, Box<RawValue>>,
    name: &'static str,
) -> Result<Utf16JsonProjection, E>
where
    E: serde::de::Error,
{
    let raw = fields.remove(name).ok_or_else(|| E::missing_field(name))?;
    Utf16JsonProjection::parse(raw.get()).map_err(E::custom)
}

fn reject_duplicate_fields<E>(duplicates: &HashSet<String>, known: &[&str]) -> Result<(), E>
where
    E: serde::de::Error,
{
    if let Some(field) = duplicates
        .iter()
        .find(|field| known.contains(&field.as_str()))
    {
        return Err(E::custom(format!("duplicate field `{field}`")));
    }
    Ok(())
}

impl<'de> Deserialize<'de> for WorkerToParent {
    fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
    where
        D: Deserializer<'de>,
    {
        let RawFrame {
            mut fields,
            duplicates,
        } = RawFrame::deserialize(deserializer)?;
        let tag: String = raw_required(&mut fields, "type")?;
        let known_fields: &[&str] = match tag.as_str() {
            "hello" => &["type", "token"],
            "ready" => &["type", "task_id", "selection"],
            "send_message" => &["type", "id", "input"],
            "state" => &["type", "status", "error", "awaiting_plan_approval"],
            "output" => &["type", "text"],
            "coordinator_message" => &["type", "message"],
            _ => &["type"],
        };
        reject_duplicate_fields::<D::Error>(&duplicates, known_fields)?;
        match tag.as_str() {
            "hello" => Ok(Self::Hello {
                token: raw_required(&mut fields, "token")?,
            }),
            "ready" => Ok(Self::Ready {
                task_id: raw_required(&mut fields, "task_id")?,
                selection: raw_required(&mut fields, "selection")?,
            }),
            "send_message" => Ok(Self::SendMessage {
                id: raw_required(&mut fields, "id")?,
                input: raw_projection(&mut fields, "input")?,
            }),
            "state" => Ok(Self::State {
                status: raw_required(&mut fields, "status")?,
                error: raw_optional(&mut fields, "error")?,
                awaiting_plan_approval: raw_required(&mut fields, "awaiting_plan_approval")?,
            }),
            "output" => Ok(Self::Output {
                text: raw_projection(&mut fields, "text")?,
            }),
            "coordinator_message" => Ok(Self::CoordinatorMessage {
                message: raw_projection(&mut fields, "message")?,
            }),
            _ => Err(serde::de::Error::unknown_variant(
                &tag,
                &[
                    "hello",
                    "ready",
                    "send_message",
                    "state",
                    "output",
                    "coordinator_message",
                ],
            )),
        }
    }
}

impl<'de> Deserialize<'de> for ParentToWorker {
    fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
    where
        D: Deserializer<'de>,
    {
        let RawFrame {
            mut fields,
            duplicates,
        } = RawFrame::deserialize(deserializer)?;
        let tag: String = raw_required(&mut fields, "type")?;
        let known_fields: &[&str] = match tag.as_str() {
            "message" => &["type", "text"],
            "plan_approval_response" => &["type", "response"],
            "send_message_result" => &["type", "id", "result", "is_error"],
            "shutdown" => &["type"],
            _ => &["type"],
        };
        reject_duplicate_fields::<D::Error>(&duplicates, known_fields)?;
        match tag.as_str() {
            "message" => Ok(Self::Message {
                text: raw_projection(&mut fields, "text")?,
            }),
            "plan_approval_response" => Ok(Self::PlanApprovalResponse {
                response: raw_required(&mut fields, "response")?,
            }),
            "send_message_result" => Ok(Self::SendMessageResult {
                id: raw_required(&mut fields, "id")?,
                result: raw_projection(&mut fields, "result")?,
                is_error: raw_required(&mut fields, "is_error")?,
            }),
            "shutdown" => Ok(Self::Shutdown),
            _ => Err(serde::de::Error::unknown_variant(
                &tag,
                &[
                    "message",
                    "plan_approval_response",
                    "send_message_result",
                    "shutdown",
                ],
            )),
        }
    }
}

#[cfg(test)]
mod serialization_tests {
    use super::*;

    fn frame(source: &str) -> Vec<u8> {
        let mut bytes = source.as_bytes().to_vec();
        bytes.push(b'\n');
        bytes
    }

    fn projection(source: &str) -> Utf16JsonProjection {
        Utf16JsonProjection::parse(source).expect("projection source is valid JSON")
    }

    fn assert_worker_projection_frame(source: &str, expected: Utf16JsonProjection) {
        let parsed: WorkerToParent = serde_json::from_slice(&frame(source)).unwrap();
        match &parsed {
            WorkerToParent::SendMessage { input, .. } => assert_eq!(input, &expected),
            WorkerToParent::Output { text } => assert_eq!(text, &expected),
            WorkerToParent::CoordinatorMessage { message } => assert_eq!(message, &expected),
            other => panic!("unexpected worker frame: {other:?}"),
        }
        let serialized = serde_json::to_vec(&parsed).unwrap();
        let reparsed: WorkerToParent = serde_json::from_slice(&serialized).unwrap();
        match reparsed {
            WorkerToParent::SendMessage { input, .. } => assert_eq!(input, expected),
            WorkerToParent::Output { text } => assert_eq!(text, expected),
            WorkerToParent::CoordinatorMessage { message } => assert_eq!(message, expected),
            other => panic!("unexpected round-tripped worker frame: {other:?}"),
        }
    }

    fn assert_parent_projection_frame(source: &str, expected: Utf16JsonProjection) {
        let parsed: ParentToWorker = serde_json::from_slice(&frame(source)).unwrap();
        match &parsed {
            ParentToWorker::Message { text } => assert_eq!(text, &expected),
            ParentToWorker::SendMessageResult { result, .. } => assert_eq!(result, &expected),
            other => panic!("unexpected parent frame: {other:?}"),
        }
        let serialized = serde_json::to_vec(&parsed).unwrap();
        let reparsed: ParentToWorker = serde_json::from_slice(&serialized).unwrap();
        match reparsed {
            ParentToWorker::Message { text } => assert_eq!(text, expected),
            ParentToWorker::SendMessageResult { result, .. } => assert_eq!(result, expected),
            other => panic!("unexpected round-tripped parent frame: {other:?}"),
        }
    }

    #[test]
    fn worker_state_error_accepts_absent_null_and_string_values() {
        for source in [
            r#"{"type":"state","status":"running","awaiting_plan_approval":false}"#,
            r#"{"type":"state","status":"running","error":null,"awaiting_plan_approval":false}"#,
        ] {
            let parsed: WorkerToParent = serde_json::from_slice(&frame(source)).unwrap();
            assert!(matches!(
                parsed,
                WorkerToParent::State {
                    status,
                    error: None,
                    awaiting_plan_approval: false,
                } if status == "running"
            ));
        }

        let parsed: WorkerToParent = serde_json::from_slice(&frame(
            r#"{"type":"state","status":"failed","error":"startup failed","awaiting_plan_approval":false}"#,
        ))
        .unwrap();
        assert!(matches!(
            parsed,
            WorkerToParent::State {
                status,
                error: Some(error),
                awaiting_plan_approval: false,
            } if status == "failed" && error == "startup failed"
        ));
    }

    #[test]
    fn worker_request_and_output_frames_round_trip_ordinary_and_lone_surrogate_text() {
        assert_worker_projection_frame(
            r#"{"type":"send_message","id":4,"input":{"to":"main","message":"ordinary"}}"#,
            projection(r#"{"to":"main","message":"ordinary"}"#),
        );
        let request = projection(r#"{"to":"main","message":"\ud800"}"#);
        assert_eq!(request.string_units("/message"), Some(vec![0xd800]));
        assert_worker_projection_frame(
            r#"{"type":"send_message","id":5,"input":{"to":"main","message":"\ud800"}}"#,
            request,
        );

        assert_worker_projection_frame(
            r#"{"type":"output","text":"ordinary output"}"#,
            projection(r#""ordinary output""#),
        );
        let output = projection(r#""\ud800""#);
        assert_eq!(output.string_units(""), Some(vec![0xd800]));
        assert_worker_projection_frame(r#"{"type":"output","text":"\ud800"}"#, output);
    }

    #[test]
    fn parent_result_and_permission_frames_round_trip_ordinary_and_lone_surrogate_text() {
        assert_parent_projection_frame(
            r#"{"type":"send_message_result","id":4,"result":{"ok":true,"message":"ordinary"},"is_error":false}"#,
            projection(r#"{"ok":true,"message":"ordinary"}"#),
        );
        let result = projection(r#"{"ok":true,"message":"\ud800"}"#);
        assert_eq!(result.string_units("/message"), Some(vec![0xd800]));
        assert_parent_projection_frame(
            r#"{"type":"send_message_result","id":5,"result":{"ok":true,"message":"\ud800"},"is_error":false}"#,
            result,
        );

        for (feedback, expected_units) in [
            ("ordinary", "ordinary".encode_utf16().collect::<Vec<_>>()),
            (r#"\ud800"#, vec![0xd800]),
        ] {
            let source = format!(
                r#"{{"type":"plan_approval_response","response":{{"requestId":"request-1","approved":true,"feedback":"{feedback}","permissionMode":"acceptEdits"}}}}"#
            );
            let parsed: ParentToWorker = serde_json::from_slice(&frame(&source)).unwrap();
            let ParentToWorker::PlanApprovalResponse { response } = parsed else {
                panic!("expected plan approval response frame");
            };
            let feedback = response.feedback.as_ref().expect("feedback is present");
            assert_eq!(feedback.string_units(""), Some(expected_units));
            assert_eq!(response.permission_mode.as_deref(), Some("acceptEdits"));

            let serialized = serde_json::to_vec(&ParentToWorker::PlanApprovalResponse {
                response: response.clone(),
            })
            .unwrap();
            let reparsed: ParentToWorker = serde_json::from_slice(&serialized).unwrap();
            let ParentToWorker::PlanApprovalResponse { response: reparsed } = reparsed else {
                panic!("expected round-tripped plan approval response frame");
            };
            assert_eq!(reparsed.feedback.as_ref(), Some(feedback));
            assert_eq!(reparsed.permission_mode.as_deref(), Some("acceptEdits"));
        }
    }

    #[test]
    fn parent_message_frame_round_trips_ordinary_and_lone_surrogate_text() {
        assert_parent_projection_frame(
            r#"{"type":"message","text":"ordinary message"}"#,
            projection(r#""ordinary message""#),
        );
        let message = projection(r#""\ud800""#);
        assert_eq!(message.string_units(""), Some(vec![0xd800]));
        assert_parent_projection_frame(r#"{"type":"message","text":"\ud800"}"#, message);
    }
}
