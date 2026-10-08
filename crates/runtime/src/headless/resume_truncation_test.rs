//! Tests for [`crate::resume_truncation`] — CLI-13's truncating resume and its
//! `--resume-drops-turn` attribution guard (oracle `AEy`, @306799802).

use super::*;
use serde_json::json;

fn entry(kind: &str, uuid: &str, extra: serde_json::Value) -> JsonlMessage {
    let mut v = json!({
        "type": kind,
        "uuid": uuid,
        "parentUuid": null,
        "sessionId": "11111111-1111-4111-8111-111111111111",
        "timestamp": "2026-08-21T00:00:00.000Z",
    });
    if let (Some(obj), Some(extra)) = (v.as_object_mut(), extra.as_object()) {
        for (k, val) in extra {
            obj.insert(k.clone(), val.clone());
        }
    }
    serde_json::from_value(v).expect("fixture entry parses")
}

fn user_prompt(uuid: &str, text: &str) -> JsonlMessage {
    entry(
        "user",
        uuid,
        json!({"message": {"role":"user","content": text}}),
    )
}

fn assistant(uuid: &str) -> JsonlMessage {
    entry(
        "assistant",
        uuid,
        json!({"message": {"role":"assistant","content":[{"type":"text","text":"ok"}]}}),
    )
}

const KEEP: &str = "aaaaaaaa-aaaa-4aaa-8aaa-aaaaaaaaaaaa";
const TURN: &str = "bbbbbbbb-bbbb-4bbb-8bbb-bbbbbbbbbbbb";
const OTHER: &str = "cccccccc-cccc-4ccc-8ccc-cccccccccccc";

/// The happy path: truncate at the named chain entry, and the discarded range
/// is exactly the declared turn (prompt + its assistant reply + tool results).
#[test]
fn truncates_at_the_named_entry_and_accepts_a_clean_turn() {
    let messages = vec![
        user_prompt(KEEP, "first"),
        assistant("dddddddd-dddd-4ddd-8ddd-dddddddddddd"),
        user_prompt(TURN, "second"),
        assistant("eeeeeeee-eeee-4eee-8eee-eeeeeeeeeeee"),
        entry(
            "user",
            "ffffffff-ffff-4fff-8fff-ffffffffffff",
            json!({"message":{"role":"user","content":[{"type":"tool_result","tool_use_id":"t1","content":"x"}]}}),
        ),
    ];
    // Truncate at the ASSISTANT entry that closes the kept turn — the 2.1.238
    // help text's "any chain-entry UUID, typically the kept turn's last entry".
    let out = apply_truncating_resume(
        messages,
        Some("dddddddd-dddd-4ddd-8ddd-dddddddddddd"),
        Some(TURN),
    )
    .expect("clean single-turn discard is accepted");
    assert_eq!(out.len(), 2);
    assert_eq!(out[0].uuid, KEEP);
}

/// `d<0` → the byte-exact not-found line.
#[test]
fn unknown_resume_point_reports_the_oracle_line() {
    let err = apply_truncating_resume(vec![user_prompt(KEEP, "x")], Some(OTHER), None)
        .expect_err("unknown uuid must fail");
    assert_eq!(
        err,
        format!("No message found with message.uuid of: {OTHER}")
    );
}

/// With no `--resume-drops-turn`, the guard never runs — the resume truncates
/// unconditionally.
#[test]
fn without_the_guard_flag_any_range_is_discarded() {
    let messages = vec![
        user_prompt(KEEP, "first"),
        user_prompt(OTHER, "an unrelated later turn"),
    ];
    let out = apply_truncating_resume(messages, Some(KEEP), None).expect("no guard, no refusal");
    assert_eq!(out.len(), 1);
}

/// A range that starts with a DIFFERENT turn's prompt is refused, and the
/// message carries the oracle's prefix + `rct()` entry pointer.
#[test]
fn refuses_when_the_range_starts_with_another_turn() {
    let messages = vec![user_prompt(KEEP, "first"), user_prompt(OTHER, "another")];
    let err = apply_truncating_resume(messages, Some(KEEP), Some(TURN))
        .expect_err("the declared turn is not what would be discarded");
    assert!(err.starts_with(DROP_GUARD_REFUSED_PREFIX), "got {err}");
    assert!(
        err.contains(&format!(
            "would discard entries not attributable to turn {TURN}: \
             range does not start with the declared turn prompt; first discarded \
             entry 0 [type=user, uuid={OTHER}]"
        )),
        "got {err}"
    );
}

/// An absorbed queued command inside the range is the headline case the flag
/// exists for.
#[test]
fn refuses_absorbed_queued_content() {
    let messages = vec![
        user_prompt(KEEP, "first"),
        user_prompt(TURN, "second"),
        entry(
            "attachment",
            OTHER,
            json!({"attachment": {"type": "queued_command"}}),
        ),
    ];
    let err = apply_truncating_resume(messages, Some(KEEP), Some(TURN)).expect_err("refused");
    assert!(
        err.contains(
            "range contains absorbed queued content; entry 1 [type=attachment (queued_command), uuid="
        ),
        "got {err}"
    );
}

/// A compaction summary in the range is refused; a furniture attachment is not.
#[test]
fn refuses_a_compaction_summary_but_allows_furniture() {
    let furniture = vec![
        user_prompt(KEEP, "first"),
        user_prompt(TURN, "second"),
        entry(
            "attachment",
            OTHER,
            json!({"attachment": {"type": "todo_reminder"}}),
        ),
    ];
    assert!(apply_truncating_resume(furniture, Some(KEEP), Some(TURN)).is_ok());

    let summary = vec![
        user_prompt(KEEP, "first"),
        user_prompt(TURN, "second"),
        entry(
            "user",
            OTHER,
            json!({"isCompactSummary": true, "message": {"role":"user","content":"summary"}}),
        ),
    ];
    let err = apply_truncating_resume(summary, Some(KEEP), Some(TURN)).expect_err("refused");
    assert!(
        err.contains("range contains a compaction summary;"),
        "got {err}"
    );
}

/// `pnu` — an entry with a non-human, non-auto-continuation `origin` is
/// externally sourced and never attributable.
#[test]
fn refuses_an_externally_sourced_entry() {
    let messages = vec![
        user_prompt(KEEP, "first"),
        entry(
            "user",
            TURN,
            json!({"origin": {"kind": "remote"}, "message": {"role":"user","content":"hi"}}),
        ),
    ];
    let err = apply_truncating_resume(messages, Some(KEEP), Some(TURN)).expect_err("refused");
    assert!(
        err.contains("declared turn id names an externally-sourced entry;"),
        "got {err}"
    );
    // `origin.kind === "human"` is fine.
    let ok = vec![
        user_prompt(KEEP, "first"),
        entry(
            "user",
            TURN,
            json!({"origin": {"kind": "human"}, "message": {"role":"user","content":"hi"}}),
        ),
    ];
    assert!(apply_truncating_resume(ok, Some(KEEP), Some(TURN)).is_ok());
}

/// A non-UUID declared turn id is refused before anything else is inspected.
#[test]
fn refuses_a_non_uuid_turn_id() {
    let err = verify_dropped_turn(&[], "not-a-uuid").expect_err("refused");
    assert_eq!(err, "declared turn id is not a UUID: not-a-uuid");
    assert!(
        verify_dropped_turn(&[], TURN).is_ok(),
        "an empty range is ok"
    );
}

/// `WI0` — trailing furniture of the PREVIOUS turn may lead the range: an
/// interrupt sentinel, a synthetic "No response requested." assistant line and
/// a skippable attachment all precede the declared prompt without refusing.
#[test]
fn leading_previous_turn_furniture_is_skipped() {
    let range = vec![
        entry(
            "user",
            "10101010-1010-4010-8010-101010101010",
            json!({"message":{"role":"user","content":"[Request interrupted by user]"}}),
        ),
        entry(
            "assistant",
            "20202020-2020-4020-8020-202020202020",
            json!({"message":{"model":"<synthetic>","role":"assistant",
                   "content":[{"type":"text","text":"No response requested."}]}}),
        ),
        entry(
            "attachment",
            "30303030-3030-4030-8030-303030303030",
            json!({"attachment": {"type": "date_change"}}),
        ),
        user_prompt(TURN, "the real prompt"),
        assistant("40404040-4040-4040-8040-404040404040"),
    ];
    assert!(verify_dropped_turn(&range, TURN).is_ok());

    // …but `qI0` subtracts two furniture types from the SKIPPABLE set, so an
    // `mcp_resource` leading the range is not skipped and the range then fails
    // to start with the declared prompt.
    let mut blocked = range.clone();
    blocked[2] = entry(
        "attachment",
        "30303030-3030-4030-8030-303030303030",
        json!({"attachment": {"type": "mcp_resource"}}),
    );
    let err = verify_dropped_turn(&blocked, TURN).expect_err("refused");
    assert!(
        err.contains("range does not start with the declared turn prompt"),
        "got {err}"
    );
}

/// A delivered poll-event record anywhere in the range refuses with the one
/// reason that carries no entry pointer.
#[test]
fn refuses_a_delivered_poll_event() {
    let range = vec![
        user_prompt(TURN, "prompt"),
        entry(
            "attachment",
            OTHER,
            json!({"attachment": {"type": "poll_events"}}),
        ),
    ];
    let err = verify_dropped_turn(&range, TURN).expect_err("refused");
    assert_eq!(err, "range contains a delivered poll-event record");
}

/// A system-injected turn prompt (`isMeta` WITH a `promptSource`) is refused,
/// while a plain `isMeta` line is allowed.
#[test]
fn distinguishes_system_injected_prompts_from_plain_meta() {
    let injected = vec![
        user_prompt(TURN, "prompt"),
        entry(
            "user",
            OTHER,
            json!({"isMeta": true, "promptSource": "hook",
                   "message": {"role":"user","content":"injected"}}),
        ),
    ];
    let err = verify_dropped_turn(&injected, TURN).expect_err("refused");
    assert!(
        err.contains("range contains a system-injected turn prompt;"),
        "got {err}"
    );

    let plain = vec![
        user_prompt(TURN, "prompt"),
        entry(
            "user",
            OTHER,
            json!({"isMeta": true, "message": {"role":"user","content":"note"}}),
        ),
    ];
    assert!(verify_dropped_turn(&plain, TURN).is_ok());
}
