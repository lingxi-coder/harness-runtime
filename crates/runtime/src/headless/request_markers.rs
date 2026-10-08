//! Native 2.1.293 yv/Lo request markers. This finite owner tracks consumed
//! client UUIDs; generated transcript identities are not request markers.

use lingxi_core::types::utf16_json::Utf16JsonProjection;
use serde_json::{Value, json};

const LIMIT: usize = 64;

#[derive(Clone)]
struct Marker {
    projection: Utf16JsonProjection,
    identity: String,
}

impl Marker {
    fn new(projection: Utf16JsonProjection) -> Result<Self, String> {
        if !projection.value.is_string() {
            return Err("request marker UUID must be a string".into());
        }
        let identity = projection
            .to_json_string()
            .map_err(|error| error.to_string())?;
        Ok(Self {
            projection,
            identity,
        })
    }
}

#[derive(Default)]
pub(super) struct RequestMarkers {
    primary: Option<Marker>,
    primary_is_meta: bool,
    consumed: Vec<Marker>,
    staged: Vec<Marker>,
    last_started: Option<Marker>,
    assistant_stamp: Option<String>,
    stream_stamp: Option<String>,
}

impl RequestMarkers {
    pub(super) fn begin(
        primary: Option<Utf16JsonProjection>,
        consumed: Vec<Utf16JsonProjection>,
        primary_is_meta: bool,
    ) -> Result<Self, String> {
        let mut owner = Self {
            primary: primary.map(Marker::new).transpose()?,
            primary_is_meta,
            ..Default::default()
        };
        for value in consumed {
            owner.consume(Marker::new(value)?);
        }
        if let Some(primary) = owner.primary.clone() {
            if !owner.contains(&primary) {
                if owner.consumed.len() >= LIMIT {
                    owner.consumed[LIMIT - 1] = primary;
                } else {
                    owner.consumed.push(primary);
                }
            }
        }
        Ok(owner)
    }

    fn contains(&self, marker: &Marker) -> bool {
        self.consumed
            .iter()
            .any(|value| value.identity == marker.identity)
    }

    fn consume(&mut self, marker: Marker) {
        if marker.projection.value.as_str() == Some("") || self.contains(&marker) {
            return;
        }
        if self.consumed.len() < LIMIT {
            self.consumed.push(marker);
        } else if self.primary.is_none() || self.primary_is_meta {
            self.consumed[LIMIT - 1] = marker;
        }
    }

    /// Native queued_command stages source_uuid. A lifecycle "started" is
    /// the consumption point; queued/cancelled/discarded do not consume it.
    pub(super) fn note_attachment(
        &mut self,
        attachment: &Utf16JsonProjection,
    ) -> Result<(), String> {
        if attachment.value.get("type").and_then(Value::as_str) != Some("queued_command")
            || attachment.value.get("commandMode").and_then(Value::as_str) != Some("prompt")
            || attachment.value.get("isMeta").and_then(Value::as_bool) == Some(true)
            || self.staged.len() >= LIMIT
        {
            return Ok(());
        }
        let Ok(uuid) = attachment.subprojection("/source_uuid") else {
            return Ok(());
        };
        let marker = Marker::new(uuid)?;
        if !self
            .staged
            .iter()
            .any(|value| value.identity == marker.identity)
        {
            self.staged.push(marker);
        }
        Ok(())
    }

    pub(super) fn note_command_started(
        &mut self,
        uuid: Utf16JsonProjection,
    ) -> Result<bool, String> {
        let marker = Marker::new(uuid)?;
        let Some(index) = self
            .staged
            .iter()
            .position(|value| value.identity == marker.identity)
        else {
            return Ok(false);
        };
        self.staged.remove(index);
        self.consume(marker.clone());
        if self.contains(&marker) {
            self.last_started = Some(marker);
        }
        Ok(true)
    }

    fn primary(&self) -> Option<&Marker> {
        let last = self.consumed.last()?;
        if let Some(primary) = self.primary.as_ref() {
            if !(self.primary_is_meta && self.last_started.is_some()) {
                return Some(primary);
            }
        }
        self.last_started.as_ref().or(Some(last))
    }

    pub(super) fn primary_is_unchanged(&self) -> bool {
        self.primary
            .as_ref()
            .zip(self.primary())
            .is_some_and(|(initial, current)| initial.identity == current.identity)
    }

    pub(super) fn is_empty(&self) -> bool {
        self.primary().is_none()
    }

    pub(super) fn fields(&self) -> Result<Option<Utf16JsonProjection>, String> {
        let Some(primary) = self.primary() else {
            return Ok(None);
        };
        let mut fields = Utf16JsonProjection::plain(json!({
            "user_message_uuid":primary.projection.value,
            "user_message_uuids":self.consumed.iter().map(|value| value.projection.value.clone()).collect::<Vec<_>>()
        }));
        fields
            .set_pointer("/user_message_uuid", primary.projection.clone())
            .map_err(|error| error.to_string())?;
        for (index, marker) in self.consumed.iter().enumerate() {
            fields
                .set_pointer(
                    &format!("/user_message_uuids/{index}"),
                    marker.projection.clone(),
                )
                .map_err(|error| error.to_string())?;
        }
        Ok(Some(fields))
    }

    pub(super) fn stamp(&mut self, frame: &mut Utf16JsonProjection) -> Result<(), String> {
        let Some(primary) = self.primary().cloned() else {
            return Ok(());
        };
        let identity = primary.identity.clone();
        let kind = frame.value.get("type").and_then(Value::as_str);
        let main = frame
            .value
            .get("parent_tool_use_id")
            .is_none_or(Value::is_null);
        let fields = match kind {
            Some("result") => self.fields()?,
            Some("assistant") if main && self.assistant_stamp.as_ref() != Some(&identity) => {
                self.assistant_stamp = Some(identity);
                self.fields()?
            }
            Some("stream_event")
                if main
                    && frame.value.pointer("/event/type").and_then(Value::as_str)
                        != Some("ping")
                    && self.stream_stamp.as_ref() != Some(&identity) =>
            {
                self.stream_stamp = Some(identity);
                self.fields()?
            }
            Some("system")
                if frame.value.get("subtype").and_then(Value::as_str)
                    == Some("thinking_tokens") =>
            {
                let mut fields = Utf16JsonProjection::plain(
                    json!({"user_message_uuid":primary.projection.value}),
                );
                fields
                    .set_pointer("/user_message_uuid", primary.projection.clone())
                    .map_err(|error| error.to_string())?;
                Some(fields)
            }
            Some("system")
                if frame.value.get("subtype").and_then(Value::as_str) == Some("status")
                    && frame.value.get("status").and_then(Value::as_str) == Some("requesting") =>
            {
                self.fields()?
                    .map(|fields| fields.pick_object_fields(&["user_message_uuids"]))
                    .transpose()
                    .map_err(|error| error.to_string())?
            }
            _ => None,
        };
        if let Some(fields) = fields {
            for key in fields
                .value
                .as_object()
                .expect("owned marker object")
                .keys()
            {
                let value = fields
                    .subprojection(&format!("/{key}"))
                    .map_err(|error| error.to_string())?;
                frame
                    .set_field(key, value)
                    .map_err(|error| error.to_string())?;
            }
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn id(value: &str) -> Utf16JsonProjection {
        Utf16JsonProjection::plain(json!(value))
    }
    fn field(owner: &RequestMarkers) -> Value {
        owner.fields().unwrap().unwrap().value
    }
    fn attachment(uuid: &str, meta: bool) -> Utf16JsonProjection {
        Utf16JsonProjection::plain(
            json!({"type":"queued_command","commandMode":"prompt","isMeta":meta,"source_uuid":uuid}),
        )
    }

    #[test]
    fn native_batch_deduplicates_in_order_and_keeps_explicit_primary_at_cap() {
        let mut batch = (0..70)
            .map(|index| id(&format!("u{index}")))
            .collect::<Vec<_>>();
        batch.insert(1, id("u0"));
        batch.insert(2, id(""));
        let owner = RequestMarkers::begin(Some(id("primary")), batch, false).unwrap();
        let fields = field(&owner);
        assert_eq!(fields["user_message_uuid"], "primary");
        let list = fields["user_message_uuids"].as_array().unwrap();
        assert_eq!(list.len(), 64);
        assert_eq!(list[0], "u0");
        assert_eq!(list[62], "u62");
        assert_eq!(list[63], "primary");
        let absent =
            RequestMarkers::begin(None, (0..70).map(|i| id(&format!("u{i}"))).collect(), false)
                .unwrap();
        assert_eq!(field(&absent)["user_message_uuid"], "u69");
        assert_eq!(field(&absent)["user_message_uuids"][63], "u69");
    }

    #[test]
    fn only_started_nonmeta_prompt_attachments_consume_and_typed_primary_stays_stable() {
        let mut owner = RequestMarkers::begin(Some(id("typed")), vec![id("typed")], false).unwrap();
        owner.note_attachment(&attachment("later", false)).unwrap();
        owner.note_attachment(&attachment("meta", true)).unwrap();
        assert_eq!(field(&owner)["user_message_uuids"], json!(["typed"]));
        assert!(!owner.note_command_started(id("meta")).unwrap());
        assert!(owner.note_command_started(id("later")).unwrap());
        assert!(!owner.note_command_started(id("later")).unwrap());
        assert_eq!(
            field(&owner)["user_message_uuids"],
            json!(["typed", "later"])
        );
        assert_eq!(field(&owner)["user_message_uuid"], "typed");
        let mut meta = RequestMarkers::begin(Some(id("notification")), vec![], true).unwrap();
        meta.note_attachment(&attachment("human", false)).unwrap();
        meta.note_command_started(id("human")).unwrap();
        assert_eq!(field(&meta)["user_message_uuid"], "human");
    }

    #[test]
    fn independent_scalar_latches_skip_ping_subagents_and_tool_continuation() {
        let mut owner = RequestMarkers::begin(Some(id("typed")), vec![id("typed")], false).unwrap();
        let stamp = |owner: &mut RequestMarkers, value: Value| {
            let mut frame = Utf16JsonProjection::plain(value);
            owner.stamp(&mut frame).unwrap();
            frame.value
        };
        assert!(
            stamp(
                &mut owner,
                json!({"type":"stream_event","event":{"type":"ping"}})
            )
            .get("user_message_uuid")
            .is_none()
        );
        assert!(
            stamp(
                &mut owner,
                json!({"type":"assistant","parent_tool_use_id":"child"})
            )
            .get("user_message_uuid")
            .is_none()
        );
        assert_eq!(
            stamp(&mut owner, json!({"type":"assistant"}))["user_message_uuid"],
            "typed"
        );
        assert_eq!(
            stamp(
                &mut owner,
                json!({"type":"stream_event","event":{"type":"message_start"}})
            )["user_message_uuid"],
            "typed"
        );
        owner.note_attachment(&attachment("later", false)).unwrap();
        owner.note_command_started(id("later")).unwrap();
        assert!(
            stamp(&mut owner, json!({"type":"assistant"}))
                .get("user_message_uuid")
                .is_none()
        );
        assert!(
            stamp(
                &mut owner,
                json!({"type":"stream_event","event":{"type":"message_start"}})
            )
            .get("user_message_uuid")
            .is_none()
        );
        assert_eq!(
            stamp(&mut owner, json!({"type":"result"}))["user_message_uuids"],
            json!(["typed", "later"])
        );
        let status = stamp(
            &mut owner,
            json!({"type":"system","subtype":"status","status":"requesting"}),
        );
        assert!(status.get("user_message_uuid").is_none());
        assert!(status.get("user_message_uuids").is_some());
        let thinking = stamp(
            &mut owner,
            json!({"type":"system","subtype":"thinking_tokens"}),
        );
        assert!(thinking.get("user_message_uuid").is_some());
        assert!(thinking.get("user_message_uuids").is_none());
    }

    #[test]
    fn meta_consumption_restamps_each_kind_and_marker_carrier_keeps_unpaired_utf16() {
        let exact = Utf16JsonProjection::parse(r#""\ud800""#).unwrap();
        let mut owner = RequestMarkers::begin(Some(exact.clone()), vec![exact], true).unwrap();
        let mut first = Utf16JsonProjection::plain(json!({"type":"assistant"}));
        owner.stamp(&mut first).unwrap();
        assert_eq!(
            first.to_json_string().unwrap(),
            r#"{"type":"assistant","user_message_uuid":"\ud800","user_message_uuids":["\ud800"]}"#
        );
        owner.note_attachment(&attachment("human", false)).unwrap();
        owner.note_command_started(id("human")).unwrap();
        let mut second = Utf16JsonProjection::plain(json!({"type":"assistant"}));
        owner.stamp(&mut second).unwrap();
        assert_eq!(second.value["user_message_uuid"], "human");
        assert_eq!(
            second
                .subprojection("/user_message_uuids/0")
                .unwrap()
                .to_json_string()
                .unwrap(),
            r#""\ud800""#
        );
    }
}
