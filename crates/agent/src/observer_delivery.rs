//! The observer delivery loop, ported from claude-code 2.1.270
//! (`cBn` enqueue → `mKo` loop → `gKo` batch → `hKo` spawn-or-deliver).
//!
//! An observed agent's turn produces activity; the activity is rendered into a
//! digest and queued on the pairing; a single loop drains the queue into the
//! observer. The shape matters more than it looks:
//!
//! * **One drainer.** `delivering` is the oracle's `if (n.delivering) return`.
//!   Two turns finishing at once must not both spawn the observer's first run.
//! * **The batch is taken whole.** `buffer.splice(0, buffer.length)` — a
//!   delivery carries everything queued since the last one, so a slow observer
//!   coalesces rather than falling behind one digest at a time.
//! * **A failed delivery drops its batch and stops the loop.** The oracle logs
//!   `(batch dropped)` and returns. Retrying would re-send the same digest to
//!   an observer that may already have it.
//! * **Losing the observer's state is not losing the pairing.** A resume
//!   failure restarts the observer under a NEW id with the framing prompt plus
//!   a note saying its context was lost; only a user stop is terminal.

use crate::observer_text::{ObservedActivity, ObserverFraming, build_digest, framing_prompt};
use lingxi_core::host::observer_pairing::{
    ObserverPairing, ObserverPairings, PairingState, QueuedDigest,
};
use lingxi_core::types::AgentId;
use std::sync::Arc;

/// Oracle's `[Note: …]`, appended to the framing prompt when an observer is
/// restarted because its previous state could not be resumed.
pub const FRESH_START_NOTE: &str =
    "[Note: your previous observation context was lost; this is a fresh start mid-task.]";

/// Why a delivery attempt failed, as far as the loop needs to care.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum DeliveryError {
    /// The user stopped the observer. Terminal for the pairing.
    StoppedByUser,
    /// The observer's conversation state could not be resumed. Recoverable by
    /// restarting it fresh under a new id.
    ResumeStateLost,
    /// Anything else. The batch is dropped and the loop stops.
    Other(String),
}

/// What the host must provide to actually run an observer.
#[async_trait::async_trait]
pub trait ObserverSpawner: Send + Sync {
    /// Start the observer for the first time, seeded with its framing prompt
    /// and the first digest.
    async fn spawn_first_run(
        &self,
        pairing: &ObserverPairing,
        framing_prompt: String,
        digest: String,
    ) -> Result<(), DeliveryError>;

    /// Deliver a digest into an already-running observer.
    async fn deliver(
        &self,
        observer_task_id: &AgentId,
        digest: String,
    ) -> Result<(), DeliveryError>;
}

/// Whether a delivery may proceed at all (oracle `zmt`, the arm/delivery-time
/// permission gate). A host with no gate wired allows.
pub trait DeliveryGate: Send + Sync {
    /// `Ok(true)` to proceed, `Ok(false)` to deny the pairing permanently,
    /// `Err` for a gate failure that drops the batch without denying.
    fn allows(&self, pairing: &ObserverPairing) -> Result<bool, String>;
}

/// Oracle `gKo`: assemble one delivery's text from a batch of queued digests.
///
/// A queued entry that carried a trigger gets that trigger rendered as its own
/// envelope ahead of the digest, so the observer sees what prompted the turn.
#[must_use]
pub fn assemble_batch(pairing: &ObserverPairing, batch: &[QueuedDigest]) -> String {
    let mut parts: Vec<String> = Vec::new();
    for entry in batch {
        if let Some(trigger) = entry.trigger.as_deref() {
            parts.push(build_digest(
                &pairing.observed_envelope_name,
                Some(trigger),
                &[],
                pairing.observer_message.as_deref(),
                false,
            ));
        }
        parts.push(entry.digest.clone());
    }
    format!(
        "{}\n\n{}",
        parts.join("\n\n"),
        crate::observer_text::digest_postamble(pairing.observer_message.as_deref())
    )
}

/// The framing an observer is briefed with, derived from its pairing.
#[must_use]
pub fn framing_for(pairing: &ObserverPairing) -> ObserverFraming {
    match pairing.via_worker_name.as_deref() {
        Some(worker) => ObserverFraming::ViaWorker {
            observed_envelope_name: pairing.observed_envelope_name.clone(),
            via_worker_name: worker.to_string(),
            report_target_name: pairing.report_target_name.clone(),
            coordinator_task: None,
        },
        None => ObserverFraming::Solo {
            observed_envelope_name: pairing.observed_envelope_name.clone(),
            report_target_name: pairing.report_target_name.clone(),
        },
    }
}

/// Oracle `cBn`: render a turn's activity into a digest and queue it.
///
/// Returns whether anything was queued. An empty turn with no trigger queues
/// nothing — the oracle's `if (activity.length === 0 && trigger === undefined)
/// return`, which is what keeps an idle observed agent from waking its
/// observer with an empty envelope.
pub fn enqueue_activity(
    pairings: &ObserverPairings,
    observed_key: &str,
    activity: &[ObservedActivity],
    trigger: Option<&str>,
) -> bool {
    if activity.is_empty() && trigger.is_none() {
        return false;
    }
    let Some(pairing) = pairings.get(observed_key) else {
        return false;
    };
    if !pairing.state.is_armed() {
        return false;
    }
    let digest = build_digest(
        &pairing.observed_envelope_name,
        None,
        activity,
        pairing.observer_message.as_deref(),
        false,
    );
    pairings.enqueue(
        observed_key,
        QueuedDigest {
            digest,
            trigger: trigger.map(str::to_string),
        },
    )
}

/// Oracle `mKo`: drain the pairing's queue into the observer.
///
/// Returns the number of batches delivered. Safe to call concurrently — the
/// second caller returns 0 because the first owns the buffer.
pub async fn run_delivery_loop(
    pairings: &Arc<ObserverPairings>,
    observed_key: &str,
    spawner: &Arc<dyn ObserverSpawner>,
    gate: Option<&Arc<dyn DeliveryGate>>,
) -> usize {
    let mut delivered = 0usize;
    loop {
        let Some(batch) = pairings.take_batch(observed_key) else {
            return delivered;
        };
        let Some(mut pairing) = pairings.get(observed_key) else {
            pairings.finish_delivery(observed_key);
            return delivered;
        };

        // Oracle `zmt`, consulted per batch rather than once at arm time: a
        // deny is permanent for the pairing, a gate ERROR only drops the batch.
        if let Some(gate) = gate {
            match gate.allows(&pairing) {
                Ok(true) => {}
                Ok(false) => {
                    pairings.set_state(observed_key, PairingState::Denied);
                    pairings.finish_delivery(observed_key);
                    return delivered;
                }
                Err(_) => {
                    pairings.finish_delivery(observed_key);
                    return delivered;
                }
            }
        }

        let text = assemble_batch(&pairing, &batch);
        let outcome = if pairing.first_run_done {
            spawner
                .deliver(&pairing.observer_task_id, text.clone())
                .await
        } else {
            spawner
                .spawn_first_run(
                    &pairing,
                    framing_prompt(&framing_for(&pairing)),
                    text.clone(),
                )
                .await
        };

        match outcome {
            Ok(()) => {
                if !pairing.first_run_done {
                    pairing.first_run_done = true;
                    pairings.insert(observed_key, pairing);
                }
                delivered += 1;
            }
            Err(DeliveryError::StoppedByUser) => {
                pairings.set_state(observed_key, PairingState::Stopped);
                pairings.finish_delivery(observed_key);
                return delivered;
            }
            Err(DeliveryError::ResumeStateLost) => {
                // The observer's state is gone, not the pairing. Restart it
                // under a NEW id, briefed that it lost its context.
                pairing.observer_task_id = AgentId::new();
                let framing = format!(
                    "{}\n\n{FRESH_START_NOTE}",
                    framing_prompt(&framing_for(&pairing))
                );
                let restarted = spawner.spawn_first_run(&pairing, framing, text).await;
                if restarted.is_ok() {
                    pairing.first_run_done = true;
                    pairings.insert(observed_key, pairing);
                    delivered += 1;
                } else {
                    pairings.finish_delivery(observed_key);
                    return delivered;
                }
            }
            Err(DeliveryError::Other(_)) => {
                // Oracle: the batch is dropped and the loop stops. Re-sending
                // would hand the observer a digest it may already have.
                pairings.finish_delivery(observed_key);
                return delivered;
            }
        }
        pairings.finish_delivery(observed_key);
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use lingxi_core::host::subagent_spawn::ObserverSpec;
    use std::sync::Mutex;

    #[derive(Default)]
    struct Recorder {
        first_runs: Mutex<Vec<(AgentId, String, String)>>,
        deliveries: Mutex<Vec<(AgentId, String)>>,
        next_error: Mutex<Option<DeliveryError>>,
    }

    #[async_trait::async_trait]
    impl ObserverSpawner for Recorder {
        async fn spawn_first_run(
            &self,
            pairing: &ObserverPairing,
            framing_prompt: String,
            digest: String,
        ) -> Result<(), DeliveryError> {
            self.first_runs.lock().unwrap().push((
                pairing.observer_task_id,
                framing_prompt,
                digest,
            ));
            Ok(())
        }
        async fn deliver(&self, id: &AgentId, digest: String) -> Result<(), DeliveryError> {
            if let Some(e) = self.next_error.lock().unwrap().take() {
                return Err(e);
            }
            self.deliveries.lock().unwrap().push((*id, digest));
            Ok(())
        }
    }

    fn table(observer: AgentId) -> Arc<ObserverPairings> {
        let t = Arc::new(ObserverPairings::new());
        t.insert(
            "k",
            ObserverPairing::armed(
                observer,
                &ObserverSpec::new("reviewer"),
                "worker-1".into(),
                "main".into(),
            ),
        );
        t
    }

    fn text() -> Vec<ObservedActivity> {
        vec![ObservedActivity::AssistantText {
            text: "did a thing".into(),
        }]
    }

    #[test]
    fn an_empty_turn_with_no_trigger_queues_nothing() {
        let t = table(AgentId::new());
        assert!(!enqueue_activity(&t, "k", &[], None));
        // …but a bare trigger is worth waking for
        assert!(enqueue_activity(&t, "k", &[], Some("go")));
    }

    #[test]
    fn a_terminal_pairing_queues_nothing() {
        let t = table(AgentId::new());
        t.set_state("k", PairingState::Retired);
        assert!(!enqueue_activity(&t, "k", &text(), None));
    }

    #[tokio::test]
    async fn the_first_delivery_spawns_and_later_ones_deliver() {
        let observer = AgentId::new();
        let t = table(observer);
        let rec = Arc::new(Recorder::default());
        let spawner: Arc<dyn ObserverSpawner> = rec.clone();

        enqueue_activity(&t, "k", &text(), None);
        assert_eq!(run_delivery_loop(&t, "k", &spawner, None).await, 1);
        assert_eq!(rec.first_runs.lock().unwrap().len(), 1);
        assert_eq!(rec.deliveries.lock().unwrap().len(), 0);

        enqueue_activity(&t, "k", &text(), None);
        assert_eq!(run_delivery_loop(&t, "k", &spawner, None).await, 1);
        assert_eq!(
            rec.first_runs.lock().unwrap().len(),
            1,
            "the observer is only started once"
        );
        assert_eq!(rec.deliveries.lock().unwrap().len(), 1);
    }

    /// Everything queued since the last delivery goes in one batch, so a slow
    /// observer coalesces instead of falling a digest behind per turn.
    #[tokio::test]
    async fn a_batch_carries_every_queued_digest() {
        let t = table(AgentId::new());
        let rec = Arc::new(Recorder::default());
        let spawner: Arc<dyn ObserverSpawner> = rec.clone();
        enqueue_activity(&t, "k", &text(), None);
        enqueue_activity(&t, "k", &text(), Some("second turn"));
        assert_eq!(run_delivery_loop(&t, "k", &spawner, None).await, 1);
        let runs = rec.first_runs.lock().unwrap();
        let digest = &runs[0].2;
        assert_eq!(digest.matches("did a thing").count(), 2);
        assert!(digest.contains("second turn"));
    }

    /// A stop is terminal; a lost resume is not. Both are pinned because
    /// treating them alike in either direction is a silent bug: one would
    /// resurrect an observer the user killed, the other would abandon a
    /// pairing over a recoverable fault.
    #[tokio::test]
    async fn a_user_stop_is_terminal_but_a_lost_resume_restarts_fresh() {
        let observer = AgentId::new();
        let t = table(observer);
        let rec = Arc::new(Recorder::default());
        let spawner: Arc<dyn ObserverSpawner> = rec.clone();

        enqueue_activity(&t, "k", &text(), None);
        run_delivery_loop(&t, "k", &spawner, None).await; // first run

        *rec.next_error.lock().unwrap() = Some(DeliveryError::ResumeStateLost);
        enqueue_activity(&t, "k", &text(), None);
        run_delivery_loop(&t, "k", &spawner, None).await;
        let runs = rec.first_runs.lock().unwrap();
        assert_eq!(runs.len(), 2, "a lost resume restarts the observer");
        assert!(runs[1].1.ends_with(FRESH_START_NOTE));
        assert_ne!(runs[1].0, observer, "restart gets a NEW observer id");
        drop(runs);
        assert!(t.get("k").unwrap().state.is_armed());

        *rec.next_error.lock().unwrap() = Some(DeliveryError::StoppedByUser);
        enqueue_activity(&t, "k", &text(), None);
        run_delivery_loop(&t, "k", &spawner, None).await;
        assert_eq!(t.get("k").unwrap().state, PairingState::Stopped);
    }

    struct Deny;
    impl DeliveryGate for Deny {
        fn allows(&self, _: &ObserverPairing) -> Result<bool, String> {
            Ok(false)
        }
    }
    struct Broken;
    impl DeliveryGate for Broken {
        fn allows(&self, _: &ObserverPairing) -> Result<bool, String> {
            Err("gate exploded".into())
        }
    }

    /// A DENY is permanent for the pairing; a gate ERROR only drops the batch.
    /// Collapsing them would either kill a pairing over a transient fault or
    /// keep retrying one that policy has refused.
    #[tokio::test]
    async fn a_gate_deny_is_permanent_but_a_gate_error_is_not() {
        let rec = Arc::new(Recorder::default());
        let spawner: Arc<dyn ObserverSpawner> = rec.clone();

        let t = table(AgentId::new());
        enqueue_activity(&t, "k", &text(), None);
        let deny: Arc<dyn DeliveryGate> = Arc::new(Deny);
        assert_eq!(run_delivery_loop(&t, "k", &spawner, Some(&deny)).await, 0);
        assert_eq!(t.get("k").unwrap().state, PairingState::Denied);

        let t2 = table(AgentId::new());
        enqueue_activity(&t2, "k", &text(), None);
        let broken: Arc<dyn DeliveryGate> = Arc::new(Broken);
        assert_eq!(
            run_delivery_loop(&t2, "k", &spawner, Some(&broken)).await,
            0
        );
        assert!(
            t2.get("k").unwrap().state.is_armed(),
            "a gate fault must not deny the pairing"
        );
    }

    #[test]
    fn the_fresh_start_note_is_byte_identical_to_2_1_270() {
        let f: serde_json::Value = serde_json::from_str(include_str!(
            "../../test-harness/src/parity/fixtures/cc_2_1_270_observer_agent.json"
        ))
        .unwrap();
        assert_eq!(FRESH_START_NOTE, f["fresh_start_note"].as_str().unwrap());
    }
}

/// Project a runner message onto the oracle's activity kinds (`UIo`'s arms).
///
/// The oracle digests what the observed agent DID, so a message becomes zero
/// or more entries: an assistant turn contributes its text and each tool call,
/// a user turn contributes its text and each tool result. A `System` message
/// contributes nothing — the observer is briefed separately and has no business
/// seeing the observed agent's system prompt.
#[must_use]
pub fn activity_from_message(
    message: &lingxi_core::types::ConversationMessage,
) -> Vec<ObservedActivity> {
    use lingxi_core::types::{ContentBlock, ConversationMessage};
    let mut out = Vec::new();
    match message {
        ConversationMessage::Assistant { content, .. } => {
            for block in content {
                if let Some(text) = block.visible_text() {
                    if !text.trim().is_empty() {
                        out.push(ObservedActivity::AssistantText {
                            text: text.to_owned(),
                        });
                    }
                } else if let ContentBlock::ToolUse { name, input, .. } = block {
                    out.push(ObservedActivity::ToolCall {
                        name: name.clone(),
                        input: input.to_string(),
                    });
                }
            }
        }
        ConversationMessage::User { content, .. } => {
            for block in content {
                if let Some(text) = block.visible_text() {
                    if !text.trim().is_empty() {
                        out.push(ObservedActivity::UserMessage {
                            text: text.to_owned(),
                        });
                    }
                } else if let ContentBlock::ToolResult { content, .. } = block {
                    out.push(ObservedActivity::ToolResult {
                        content: content.clone(),
                    });
                }
            }
        }
        ConversationMessage::System { .. } => {}
    }
    out
}

#[cfg(test)]
mod projection_tests {
    use super::*;
    use lingxi_core::types::{ContentBlock, ConversationMessage, MessageId, ToolUseId};

    #[test]
    fn an_assistant_turn_contributes_its_text_and_each_tool_call() {
        let msg = ConversationMessage::Assistant { per_turn_effort: None,
            id: MessageId::new(),
            content: vec![
                ContentBlock::Text {
                    text: "thinking".into(),
                    citations: None,
                },
                ContentBlock::ToolUse { input_projection: None,
                    id: ToolUseId::new(),
                    name: "Bash".into(),
                    input: serde_json::json!({"command": "ls"}),
                    provider_id: None,
                },
            ],
            stop_reason: None,
        };
        let got = activity_from_message(&msg);
        assert_eq!(got.len(), 2);
        assert!(matches!(got[0], ObservedActivity::AssistantText { .. }));
        assert!(matches!(got[1], ObservedActivity::ToolCall { .. }));
    }

    #[test]
    fn opaque_anthropic_text_is_observed_once_as_assistant_text() {
        let msg = ConversationMessage::Assistant { per_turn_effort: None,
            id: MessageId::new(),
            content: vec![ContentBlock::ProviderContent {
                protocol: "anthropic_messages".into(),
                value: serde_json::json!({
                    "type":"text",
                    "text":"visible answer",
                    "future_annotation":{"keep":true}
                }),
            }],
            stop_reason: None,
        };

        assert_eq!(
            activity_from_message(&msg),
            vec![ObservedActivity::AssistantText {
                text: "visible answer".into(),
            }]
        );
    }

    /// A user turn's tool results are what the observed agent SAW, so they are
    /// digested as results, not as user text.
    #[test]
    fn a_user_turn_contributes_text_and_tool_results_separately() {
        let msg = ConversationMessage::User { api_message_override: None,
            id: MessageId::new(),
            content: vec![
                ContentBlock::ToolResult { content_projection: None,
                    tool_use_id: ToolUseId::new(),
                    content: "file listing".into(),
                    is_error: Some(false),
                    provider_tool_use_id: None,
                    content_blocks: None,
                },
                ContentBlock::Text {
                    text: "now do X".into(),
                    citations: None,
                },
            ],
            is_meta: false,
            is_compact_summary: false,
            is_visible_in_transcript_only: false,
        };
        let got = activity_from_message(&msg);
        assert_eq!(got.len(), 2);
        assert!(matches!(got[0], ObservedActivity::ToolResult { .. }));
        assert!(matches!(got[1], ObservedActivity::UserMessage { .. }));
    }

    /// Whitespace-only text is not activity. Without this an assistant turn
    /// that only called a tool would digest an empty `AssistantText` alongside
    /// the call.
    #[test]
    fn empty_text_is_not_activity() {
        let msg = ConversationMessage::Assistant { per_turn_effort: None,
            id: MessageId::new(),
            content: vec![ContentBlock::Text {
                text: "  \n".into(),
                citations: None,
            }],
            stop_reason: None,
        };
        assert!(activity_from_message(&msg).is_empty());
    }

    /// The observer is briefed separately; it has no business seeing the
    /// observed agent's system prompt.
    #[test]
    fn a_system_message_contributes_nothing() {
        let msg = ConversationMessage::System { api_system: None,
            id: MessageId::new(),
            content: "you are…".into(),
            subtype: None,
            compact_metadata: None,
            model_fallback: None,
            refusal_fallback: None,
        };
        assert!(activity_from_message(&msg).is_empty());
    }
}
