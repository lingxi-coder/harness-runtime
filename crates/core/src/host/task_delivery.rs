//! Source-preserving snapshot and producer policy for Native local-shell
//! `taskDelivery` rows.
//!
//! A task notification is not a task-delivery snapshot. This module models the
//! latter's `uge` schema and `jB` producer without adding host or transcript
//! state. Request-history normalization lives in `llm-runtime` because it
//! operates on provider-facing conversation messages.

use crate::types::utf16_json::{Utf16JsonProjection, Utf16JsonProjectionError};
use serde::{Deserialize, Deserializer, Serialize, Serializer};
use thiserror::Error;

/// Whether Native's `m1()` producer gate permits creating a new task-delivery
/// snapshot in this host context.
///
/// This policy is intentionally producer-only. A previously persisted,
/// schema-valid snapshot remains eligible for request projection when the
/// current policy later changes.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct TaskDeliveryPolicy {
    /// Effective `tengu_violin_rosin` value; defaults to `false`.
    pub feature_enabled: bool,
    /// Native `Td()` exclusion for Windows and WSL.
    pub windows_or_wsl: bool,
    /// Native `Oon()` strict tool-result-pairing exclusion.
    pub strict_tool_result_pairing: bool,
    /// Native `ur()` bare/simple-mode exclusion.
    pub bare_mode: bool,
}

impl TaskDeliveryPolicy {
    /// Whether the local-shell producer may create a new snapshot.
    #[must_use]
    pub const fn producer_enabled(self) -> bool {
        self.feature_enabled
            && !self.windows_or_wsl
            && !self.strict_tool_result_pairing
            && !self.bare_mode
    }
}

/// Native `uWr` placement facts for one structured task-delivery carrier.
///
/// User history carriers and projected queued-command attachments use
/// different fixed flags in Native. The remaining two flags are derived from
/// the original pre-attachment-projection history.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct TaskDeliveryPlacement {
    /// Native `startsTurn`; when true the result has the context text block.
    pub starts_turn: bool,
    /// Native `inDrainPosition`; true only for a queued-command attachment
    /// immediately following a tool-result user row after attachment packing.
    pub in_drain_position: bool,
    /// Native `behindEarlierMessages`, computed before `zjr` reorders rows.
    pub behind_earlier_messages: bool,
    /// Native `inHumanTurn`; selects the human-turn wake text.
    pub in_human_turn: bool,
}

/// Request-local structured task-delivery input for one carrier row.
///
/// The snapshot remains a raw source projection so schema failure can follow
/// Native's ordinary-row fallback without losing or rewriting its JSON.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TaskDeliveryRequest {
    /// Raw source-preserving `taskDelivery` JSON.
    pub snapshot: Utf16JsonProjection,
    /// Native insertion context derived from the original history snapshot.
    pub placement: TaskDeliveryPlacement,
}

/// A Native local-shell terminal outcome consumed by `jB`.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum TaskDeliveryOutcome {
    /// A command exited successfully.
    Completed,
    /// A command exited with a failure result.
    Failed,
    /// The command was stopped before completion.
    Killed,
}

/// A recognized Native shell stop cause.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum TaskDeliveryStopCause {
    /// The idle system reaper stopped the command under memory pressure.
    MemoryPressure,
    /// The background task reached its time limit.
    Deadline,
}

/// Exact output spool values supplied to Native's `PBn` formatter.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct TaskDeliveryShellOutput {
    /// Source JavaScript UTF-16 units of the output string.
    pub content_utf16: Vec<u16>,
    /// A positive marker that the spool reader omitted earlier bytes. Native
    /// `PBn` only tests whether this value is nonzero; the current Harness
    /// spool API exposes a truncated flag and rounded header, not an exact byte
    /// count.
    pub omitted_bytes: u64,
}

/// Pure local-shell state values consumed by Native's `jB` producer.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct TaskDeliverySnapshotInput {
    /// Native task identity.
    pub task_id: String,
    /// Terminal outcome used to choose the Native snapshot status.
    pub outcome: TaskDeliveryOutcome,
    /// Process exit code; `None` means it is not known.
    pub exit_code: Option<i32>,
    /// Native start time in Unix epoch milliseconds.
    pub start_time_ms: i64,
    /// Native end time in Unix epoch milliseconds.
    pub end_time_ms: i64,
    /// Path used by Native in the status message and truncation header.
    pub output_file: String,
    /// Current spool slice and omission count, when output is available.
    pub output: Option<TaskDeliveryShellOutput>,
    /// Recognized stop cause, if one was recorded.
    pub stop_cause: Option<TaskDeliveryStopCause>,
}

/// Snapshot parsing or generation failed.
#[derive(Debug, Error)]
pub enum TaskDeliveryError {
    /// The projection is not a valid Native `uge` snapshot.
    #[error("invalid Native taskDelivery snapshot: {0}")]
    InvalidSnapshot(&'static str),
    /// A timestamp lies outside JavaScript Date's supported range.
    #[error("taskDelivery timestamp is outside the JavaScript Date range")]
    InvalidTimestamp,
    /// A projected JSON operation failed.
    #[error(transparent)]
    Projection(#[from] Utf16JsonProjectionError),
}

/// Errors from the local-shell `jB` producer.
pub type TaskDeliveryBuildError = TaskDeliveryError;

/// A Native `uge`-validated task-delivery snapshot.
///
/// The wrapper contains the parsed Native schema projection, not the flat
/// carrier's implementation DTO. Native's Zod object parser strips unknown
/// properties, while recognized string values retain their exact UTF-16 units.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TaskDeliverySnapshot {
    projection: Utf16JsonProjection,
}

impl TaskDeliverySnapshot {
    /// Parse and validate one source-preserving task-delivery projection using
    /// Native's `uge` schema. Unknown object properties are discarded just as
    /// Native's Zod object parser does.
    pub fn parse(projection: Utf16JsonProjection) -> Result<Self, TaskDeliveryError> {
        let canonical = canonicalize_native_snapshot(&projection)?;
        Ok(Self {
            projection: canonical,
        })
    }

    /// Borrow the validated source-preserving Native snapshot projection.
    #[must_use]
    pub fn as_projection(&self) -> &Utf16JsonProjection {
        &self.projection
    }

    /// Borrow the validated snapshot projection before Native's `czt` JSON
    /// stringification. Unknown fields have already been stripped, and the
    /// projection preserves the remaining field order and UTF-16 strings.
    #[must_use]
    pub fn to_tool_output_projection(&self) -> &Utf16JsonProjection {
        &self.projection
    }

    /// Reproduce Native's `czt` output as a source-preserving JSON string.
    pub fn to_tool_output_text(&self) -> Result<Utf16JsonProjection, TaskDeliveryError> {
        let text = self.projection.to_json_string()?;
        let units = text.encode_utf16().collect::<Vec<_>>();
        Utf16JsonProjection::root_string(text, units).map_err(Into::into)
    }

    /// Read the task ID from the validated schema.
    #[must_use]
    pub fn task_id(&self) -> &str {
        self.projection
            .string_at("/taskId")
            .expect("validated taskDelivery taskId is a string")
    }

    /// Return the exact task-ID source units without converting through the
    /// display-safe Rust string.
    #[must_use]
    pub fn task_id_units(&self) -> Vec<u16> {
        self.projection
            .string_units("/taskId")
            .expect("validated taskDelivery taskId is a string")
    }

    /// Extract the exact task-ID projection for a Native TaskOutput input.
    pub fn task_id_projection(&self) -> Result<Utf16JsonProjection, TaskDeliveryError> {
        self.projection.subprojection("/taskId").map_err(Into::into)
    }

    /// Read the Native status from the validated schema.
    #[must_use]
    pub fn status(&self) -> &str {
        self.projection
            .string_at("/status")
            .expect("validated taskDelivery status is a string")
    }

    /// Build a local-shell snapshot under the `m1()` producer policy.
    ///
    /// This method does not gate request normalization of snapshots that were
    /// already persisted.
    pub fn from_local_shell(
        policy: TaskDeliveryPolicy,
        input: TaskDeliverySnapshotInput,
    ) -> Result<Option<Self>, TaskDeliveryBuildError> {
        if !policy.producer_enabled() {
            return Ok(None);
        }

        let created_at = javascript_iso_timestamp(input.start_time_ms)?;
        let last_updated_at = javascript_iso_timestamp(input.end_time_ms)?;
        let (status, status_message, result) = match input.outcome {
            TaskDeliveryOutcome::Killed => {
                let wording = match input.stop_cause {
                    Some(cause) => stop_cause_text(cause),
                    None => "stopped before it completed.".into(),
                };
                (
                    "cancelled",
                    format!(
                        "The command was {wording} Output so far: {}",
                        input.output_file
                    ),
                    None,
                )
            }
            TaskDeliveryOutcome::Completed | TaskDeliveryOutcome::Failed => {
                let status_message = match input.exit_code {
                    Some(code) => format!(
                        "Exited with code {code}. Full output: {}",
                        input.output_file
                    ),
                    None => format!(
                        "Exited; the exit code is not known. Full output: {}",
                        input.output_file
                    ),
                };
                let content = match input.output {
                    Some(output) => render_native_bash_output(&output, &input.output_file),
                    None => Vec::new(),
                };
                let content_projection = if content.is_empty() {
                    Utf16JsonProjection::array(Vec::new())?
                } else {
                    Utf16JsonProjection::array(vec![text_block_projection(&content)?])?
                };
                let result = Utf16JsonProjection::object(vec![
                    ("content".into(), content_projection),
                    (
                        "isError".into(),
                        json_projection(&serde_json::Value::Bool(
                            input.outcome == TaskDeliveryOutcome::Failed,
                        ))?,
                    ),
                ])?;
                ("completed", status_message, Some(result))
            }
        };

        let mut fields = vec![
            ("taskId".into(), string_projection(input.task_id)?),
            ("status".into(), string_projection(status.into())?),
            ("statusMessage".into(), string_projection(status_message)?),
            ("createdAt".into(), string_projection(created_at)?),
            ("lastUpdatedAt".into(), string_projection(last_updated_at)?),
        ];
        if let Some(result) = result {
            fields.push(("result".into(), result));
        }
        Self::parse(Utf16JsonProjection::object(fields)?).map(Some)
    }
}

impl Serialize for TaskDeliverySnapshot {
    fn serialize<S>(&self, serializer: S) -> Result<S::Ok, S::Error>
    where
        S: Serializer,
    {
        crate::types::utf16_json::raw_json_serde::serialize(&self.projection, serializer)
    }
}

impl<'de> Deserialize<'de> for TaskDeliverySnapshot {
    fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
    where
        D: Deserializer<'de>,
    {
        let projection = crate::types::utf16_json::raw_json_serde::deserialize(deserializer)?;
        Self::parse(projection).map_err(serde::de::Error::custom)
    }
}

/// Build a task-delivery snapshot only when the source feature gate is active.
pub fn build_local_shell_task_delivery(
    policy: TaskDeliveryPolicy,
    input: TaskDeliverySnapshotInput,
) -> Result<Option<TaskDeliverySnapshot>, TaskDeliveryBuildError> {
    TaskDeliverySnapshot::from_local_shell(policy, input)
}

const NATIVE_TASK_OUTPUT_MAX_CHARS: usize = 12_000;

fn render_native_bash_output(output: &TaskDeliveryShellOutput, output_file: &str) -> Vec<u16> {
    // Native `Bl(content, 12000)` retains the final units. If that slice starts
    // on a low surrogate, Native drops that unit before its UTF-16 roundtrip.
    let source = output.content_utf16.as_slice();
    let truncated = source.len() > NATIVE_TASK_OUTPUT_MAX_CHARS;
    let mut suffix = if truncated {
        &source[source.len() - NATIVE_TASK_OUTPUT_MAX_CHARS..]
    } else {
        source
    };
    if truncated
        && suffix
            .first()
            .is_some_and(|unit| (0xdc00..=0xdfff).contains(unit))
    {
        suffix = &suffix[1..];
    }
    let omitted = output.omitted_bytes > 0 || suffix.len() < source.len();

    let mut body = suffix.to_vec();
    while body.first() == Some(&u16::from(b'\n')) {
        body.remove(0);
    }
    while body
        .last()
        .is_some_and(|unit| is_javascript_trim_space(*unit))
    {
        body.pop();
    }

    if omitted {
        let mut prefixed = "[… earlier output omitted; full output: "
            .encode_utf16()
            .collect::<Vec<_>>();
        prefixed.extend(output_file.encode_utf16());
        prefixed.extend("]\n".encode_utf16());
        prefixed.extend(body);
        prefixed
    } else {
        body
    }
}

fn is_javascript_trim_space(unit: u16) -> bool {
    matches!(
        unit,
        0x0009..=0x000d
            | 0x0020
            | 0x00a0
            | 0x1680
            | 0x2000..=0x200a
            | 0x2028..=0x2029
            | 0x202f
            | 0x205f
            | 0x3000
            | 0xfeff
    )
}

fn stop_cause_text(cause: TaskDeliveryStopCause) -> String {
    match cause {
        TaskDeliveryStopCause::MemoryPressure => format!(
            "stopped because the system is running low on memory. {}",
            "This is not a failure of the command. Claude Code stopped it because the system was critically low on memory while the session was idle, which says nothing about the command or its own memory use, so there is nothing in it to debug. Do not start it again on your own, even if the work seems to need it: memory may still be short. Report what was stopped and why, and start it again only when asked. The user can turn this behavior off by starting Claude Code with CLAUDE_CODE_DISABLE_BG_SHELL_PRESSURE_REAP=1 in its environment; setting it from a shell command has no effect."
        ),
        TaskDeliveryStopCause::Deadline => format!(
            "stopped after reaching its background time limit. {}",
            "If the work in progress still needs it, start it again with `run_in_background` and a longer `timeout`. If it already had the longest `timeout` allowed, do not restart it. Either way, report that it was stopped."
        ),
    }
}

fn javascript_iso_timestamp(milliseconds: i64) -> Result<String, TaskDeliveryError> {
    const MAX_DATE_MILLISECONDS: i64 = 8_640_000_000_000_000;
    const MILLIS_PER_DAY: i64 = 86_400_000;
    if !(-MAX_DATE_MILLISECONDS..=MAX_DATE_MILLISECONDS).contains(&milliseconds) {
        return Err(TaskDeliveryError::InvalidTimestamp);
    }

    let days = milliseconds.div_euclid(MILLIS_PER_DAY);
    let day_milliseconds = milliseconds.rem_euclid(MILLIS_PER_DAY);
    let (year, month, day) = civil_from_days(days);
    let hour = day_milliseconds / 3_600_000;
    let minute = day_milliseconds / 60_000 % 60;
    let second = day_milliseconds / 1_000 % 60;
    let millisecond = day_milliseconds % 1_000;
    let year = if (0..=9999).contains(&year) {
        format!("{year:04}")
    } else if year < 0 {
        format!("-{:06}", year.unsigned_abs())
    } else {
        format!("+{year:06}")
    };
    Ok(format!(
        "{year}-{month:02}-{day:02}T{hour:02}:{minute:02}:{second:02}.{millisecond:03}Z"
    ))
}

/// Proleptic Gregorian civil date for a day count from 1970-01-01.
fn civil_from_days(days_since_epoch: i64) -> (i64, i64, i64) {
    let z = days_since_epoch + 719_468;
    let era = if z >= 0 { z } else { z - 146_096 } / 146_097;
    let day_of_era = z - era * 146_097;
    let year_of_era =
        (day_of_era - day_of_era / 1_460 + day_of_era / 36_524 - day_of_era / 146_096) / 365;
    let mut year = year_of_era + era * 400;
    let day_of_year = day_of_era - (365 * year_of_era + year_of_era / 4 - year_of_era / 100);
    let month_prime = (5 * day_of_year + 2) / 153;
    let day = day_of_year - (153 * month_prime + 2) / 5 + 1;
    let month = month_prime + if month_prime < 10 { 3 } else { -9 };
    if month <= 2 {
        year += 1;
    }
    (year, month, day)
}

fn canonicalize_native_snapshot(
    projection: &Utf16JsonProjection,
) -> Result<Utf16JsonProjection, TaskDeliveryError> {
    if projection.object_entries().is_none() {
        return Err(TaskDeliveryError::InvalidSnapshot("root is not an object"));
    }
    let task_id = required_string(projection, "/taskId", "taskId")?;
    let status = projection
        .string_at("/status")
        .ok_or(TaskDeliveryError::InvalidSnapshot("status is not a string"))?;
    let status_value = required_string(projection, "/status", "status")?;
    let status_message = required_string(projection, "/statusMessage", "statusMessage")?;
    let created_at = required_string(projection, "/createdAt", "createdAt")?;
    let last_updated_at = required_string(projection, "/lastUpdatedAt", "lastUpdatedAt")?;

    let mut fields = vec![
        ("taskId".into(), task_id),
        ("status".into(), status_value),
        ("statusMessage".into(), status_message),
        ("createdAt".into(), created_at),
        ("lastUpdatedAt".into(), last_updated_at),
    ];
    match status {
        "working" | "cancelled" => {}
        "completed" => {
            let content_len = projection.array_len("/result/content").ok_or(
                TaskDeliveryError::InvalidSnapshot("completed result.content is not an array"),
            )?;
            let mut content = Vec::with_capacity(content_len);
            for index in 0..content_len {
                let path = format!("/result/content/{index}");
                let kind = projection.string_at(&format!("{path}/type")).ok_or(
                    TaskDeliveryError::InvalidSnapshot(
                        "completed content block type is not a string",
                    ),
                )?;
                if kind != "text" {
                    return Err(TaskDeliveryError::InvalidSnapshot(
                        "completed content block type is not text",
                    ));
                }
                let type_value =
                    required_string(projection, &format!("{path}/type"), "result.content[].type")?;
                let text =
                    required_string(projection, &format!("{path}/text"), "result.content[].text")?;
                content.push(Utf16JsonProjection::object(vec![
                    ("type".into(), type_value),
                    ("text".into(), text),
                ])?);
            }
            let content = Utf16JsonProjection::array(content)?;
            let is_error = projection
                .at_pointer("/result/isError")
                .and_then(|value| value.as_bool())
                .ok_or(TaskDeliveryError::InvalidSnapshot(
                    "completed result.isError is not a boolean",
                ))?;
            let result = Utf16JsonProjection::object(vec![
                ("content".into(), content),
                (
                    "isError".into(),
                    json_projection(&serde_json::Value::Bool(is_error))?,
                ),
            ])?;
            fields.push(("result".into(), result));
        }
        "failed" => {
            if projection
                .at_pointer("/error/code")
                .and_then(|value| value.as_number())
                .filter(|value| value.is_finite())
                .is_none()
            {
                return Err(TaskDeliveryError::InvalidSnapshot(
                    "failed error.code is not a finite number",
                ));
            }
            let code = projection.subprojection("/error/code")?;
            let message = required_string(projection, "/error/message", "error.message")?;
            fields.push((
                "error".into(),
                Utf16JsonProjection::object(vec![
                    ("code".into(), code),
                    ("message".into(), message),
                ])?,
            ));
        }
        _ => return Err(TaskDeliveryError::InvalidSnapshot("status is unknown")),
    }

    Utf16JsonProjection::object(fields).map_err(Into::into)
}

fn required_string(
    projection: &Utf16JsonProjection,
    pointer: &str,
    field: &'static str,
) -> Result<Utf16JsonProjection, TaskDeliveryError> {
    if projection
        .at_pointer(pointer)
        .and_then(|value| value.as_str())
        .is_none()
    {
        return Err(TaskDeliveryError::InvalidSnapshot(field));
    }
    projection.subprojection(pointer).map_err(Into::into)
}

fn string_projection(value: String) -> Result<Utf16JsonProjection, Utf16JsonProjectionError> {
    let units = value.encode_utf16().collect::<Vec<_>>();
    Utf16JsonProjection::root_string(value, units)
}

fn text_block_projection(
    text_units: &[u16],
) -> Result<Utf16JsonProjection, Utf16JsonProjectionError> {
    let text = Utf16JsonProjection::root_string(
        String::from_utf16_lossy(text_units),
        text_units.to_vec(),
    )?;
    Utf16JsonProjection::object(vec![
        ("type".into(), string_projection("text".into())?),
        ("text".into(), text),
    ])
}

fn json_projection(
    value: &serde_json::Value,
) -> Result<Utf16JsonProjection, Utf16JsonProjectionError> {
    Utf16JsonProjection::from_serde_value(value)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn valid_completed() -> Utf16JsonProjection {
        Utf16JsonProjection::parse(
            r#"{"taskId":"abc123456","status":"completed","statusMessage":"Exited with code 0. Full output: /tmp/out","createdAt":"2026-10-11T00:00:00.000Z","lastUpdatedAt":"2026-10-11T00:00:01.000Z","result":{"content":[{"type":"text","text":"ok"}],"isError":false},"ignored":true}"#,
        )
        .expect("valid JSON")
    }

    #[test]
    fn schema_parse_strips_unknown_fields_and_keeps_exact_units() {
        let mut raw = r#"{"taskId":"task","status":"completed","statusMessage":"done","createdAt":"now","lastUpdatedAt":"now","result":{"content":[{"type":"text","text":"x"}],"isError":false}}"#
            .encode_utf16()
            .collect::<Vec<_>>();
        let needle = "\"x\"".encode_utf16().collect::<Vec<_>>();
        let index = raw
            .windows(needle.len())
            .position(|window| window == needle)
            .unwrap()
            + 1;
        raw.splice(index..index + 1, [0xd800]);
        let projection = Utf16JsonProjection::parse_utf16_json(&raw).expect("valid UTF-16 JSON");
        let snapshot = TaskDeliverySnapshot::parse(projection).expect("valid Native schema");
        assert_eq!(
            snapshot.projection.string_units("/result/content/0/text"),
            Some(vec![0xd800])
        );
        assert!(!snapshot.projection.contains_property("ignored"));
    }

    #[test]
    fn native_schema_rejects_wrong_status_payload_types() {
        for input in [
            r#"{"taskId":"task","status":"completed","statusMessage":"done","createdAt":"now","lastUpdatedAt":"now","result":{"content":[],"isError":"false"}}"#,
            r#"{"taskId":"task","status":"failed","statusMessage":"failed","createdAt":"now","lastUpdatedAt":"now","error":{"code":"1","message":"x"}}"#,
            r#"{"taskId":"task","status":"other","statusMessage":"x","createdAt":"now","lastUpdatedAt":"now"}"#,
        ] {
            assert!(
                TaskDeliverySnapshot::parse(Utf16JsonProjection::parse(input).unwrap()).is_err()
            );
        }
    }

    #[test]
    fn producer_gate_is_false_by_default_and_only_gates_creation() {
        assert!(!TaskDeliveryPolicy::default().producer_enabled());
        let mut enabled = TaskDeliveryPolicy::default();
        enabled.feature_enabled = true;
        let input = TaskDeliverySnapshotInput {
            task_id: "abc123456".into(),
            outcome: TaskDeliveryOutcome::Failed,
            exit_code: Some(2),
            start_time_ms: 0,
            end_time_ms: 1_000,
            output_file: "/tmp/out".into(),
            output: Some(TaskDeliveryShellOutput {
                content_utf16: "\nfailed\n".encode_utf16().collect(),
                omitted_bytes: 0,
            }),
            stop_cause: None,
        };
        let snapshot = build_local_shell_task_delivery(enabled, input)
            .unwrap()
            .unwrap();
        assert_eq!(snapshot.status(), "completed");
        assert_eq!(
            snapshot.projection.string_at("/result/content/0/text"),
            Some("failed")
        );
        assert_eq!(
            snapshot
                .projection
                .at_pointer("/result/isError")
                .and_then(|v| v.as_bool()),
            Some(true)
        );
        // Producer gating does not invalidate or suppress an already built
        // persisted snapshot at request-normalization time.
        assert!(TaskDeliverySnapshot::parse(snapshot.as_projection().clone()).is_ok());
    }

    #[test]
    fn shell_output_uses_native_tail_slice_and_truncation_header() {
        let mut units = "x"
            .repeat(NATIVE_TASK_OUTPUT_MAX_CHARS + 1)
            .encode_utf16()
            .collect::<Vec<_>>();
        units[1] = 0xdc00;
        let rendered = render_native_bash_output(
            &TaskDeliveryShellOutput {
                content_utf16: units,
                omitted_bytes: 0,
            },
            "/tmp/out",
        );
        let rendered = String::from_utf16_lossy(&rendered);
        assert!(rendered.starts_with("[… earlier output omitted; full output: /tmp/out]\n"));
        assert_eq!(
            rendered.matches('x').count(),
            NATIVE_TASK_OUTPUT_MAX_CHARS - 1
        );
    }

    #[test]
    fn short_output_keeps_a_leading_low_surrogate() {
        let rendered = render_native_bash_output(
            &TaskDeliveryShellOutput {
                content_utf16: vec![0xdc00, b'x' as u16],
                omitted_bytes: 0,
            },
            "/tmp/out",
        );
        assert_eq!(rendered, vec![0xdc00, b'x' as u16]);
    }

    #[test]
    fn timestamps_match_javascript_iso_milliseconds_for_negative_and_epoch_values() {
        assert_eq!(
            javascript_iso_timestamp(0).unwrap(),
            "1970-01-01T00:00:00.000Z"
        );
        assert_eq!(
            javascript_iso_timestamp(-1).unwrap(),
            "1969-12-31T23:59:59.999Z"
        );
        assert_eq!(
            javascript_iso_timestamp(1_783_132_800_007).unwrap(),
            "2026-07-04T02:40:00.007Z"
        );
    }
}
