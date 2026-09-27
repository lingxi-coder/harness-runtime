//! TeammateIdle JSON bytes from executing the real 2.1.263 Sa + E_n helpers.
//!
//! Binary SHA-256: ef5d2909c8af49f31ab6d5487e90316777bc2fac170adfe8160716caa8aaf4f9.
//! Extracted src_160988549.js SHA-256:
//! 8a46eeba9de2b74fa4af25cf31418abdfd68bf491a0e9cc1e675b68f5143eb1c.
//! UTF-8 byte slices: E_n [5138434,5138655), Sa [5142853,5143499).
//! Capture: evaluate those verbatim slices in Node; stub Q=/workspace,
//! Tp=/workspace/session.jsonl, H_/fZ/GH as the optional case below,
//! eA=!!scratchpad, Ya=undefined, My yields hookInput. Invoke
//! E_n('scout','session-team',permission,undefined,1000,{session:{id:ZERO_UUID}}).
//! JSON.stringify outputs below are independent of Rust's serializer.
//!
//! This tests the payload serializer, not ambient propagation. The runtime
//! currently has no scratchpad allocator or ambient prompt-id source in its
//! teammate-idle firing scope; those optional values remain absent there.

use hooks::hook_payload::{HookEventNameTeammateIdle, TeammateIdlePayload};

const WITHOUT_OPTIONALS: &str = r#"{"session_id":"00000000-0000-0000-0000-000000000000","transcript_path":"/workspace/session.jsonl","cwd":"/workspace","hook_event_name":"TeammateIdle","teammate_name":"scout","team_name":"session-team"}"#;
const WITH_OPTIONALS: &str = r#"{"session_id":"00000000-0000-0000-0000-000000000000","transcript_path":"/workspace/session.jsonl","cwd":"/workspace","scratchpad_dir":"/workspace/scratch","prompt_id":"prompt-1","permission_mode":"plan","agent_type":"general-purpose","hook_event_name":"TeammateIdle","teammate_name":"scout","team_name":"session-team"}"#;

#[test]
fn teammate_idle_field_order_and_omission_match_executed_oracle() {
    for (optional, expected) in [(false, WITHOUT_OPTIONALS), (true, WITH_OPTIONALS)] {
        let payload = TeammateIdlePayload {
            session_id: "00000000-0000-0000-0000-000000000000".into(),
            transcript_path: "/workspace/session.jsonl".into(),
            cwd: "/workspace".into(),
            scratchpad_dir: optional.then(|| "/workspace/scratch".into()),
            prompt_id: optional.then(|| "prompt-1".into()),
            permission_mode: optional.then(|| "plan".into()),
            agent_type: optional.then(|| "general-purpose".into()),
            hook_event_name: HookEventNameTeammateIdle,
            teammate_name: "scout".into(),
            team_name: "session-team".into(),
        };
        assert_eq!(
            serde_json::to_string(&payload).unwrap().as_bytes(),
            expected.as_bytes()
        );
    }
}
