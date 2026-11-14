//! Checkpoints belong to the host process, never the parent transcript or disk.
use super::*;
use lingxi_core::types::{ContentBlock, ConversationMessage, MessageId};
use serde::{Deserialize, Serialize};

#[derive(Serialize, Deserialize)]
struct Checkpoint {
    version: u32,
    host_session_id: String,
    side_id: String,
    context: SideChatParentSnapshot,
    submission_ids: Vec<String>,
    cancelled_turn_ids: Vec<u64>,
    creation_ids: Vec<String>,
    interrupted: bool,
    turn_id: Option<u64>,
    slash: bool,
    terminal: Option<ClientEvent>,
}

impl SideChatManager {
    pub(super) async fn checkpoint(
        &self,
        id: &str,
        command: Option<&ClientCommand>,
        turn_complete: bool,
    ) -> Result<(), String> {
        if !self.recoverable {
            return Ok(());
        }
        let started = Instant::now();
        let side = self.side(id).await?;
        let mut context = side.session.orchestrator.side_chat_parent_snapshot().await;
        let mut events = side.replay.events.lock().await.clone();
        let turn_id = match command {
            Some(
                ClientCommand::SendPrompt { turn_id, .. }
                | ClientCommand::RunSlashCommand { turn_id, .. },
            ) => *turn_id,
            _ => None,
        };
        if let Some(ClientCommand::SendPrompt { text, images, .. }) = command {
            let source = text.clone().into_projection();
            let mut message = ConversationMessage::user(MessageId::new(), text.display());
            if let ConversationMessage::User { content, .. } = &mut message {
                *content = vec![ContentBlock::TextJsUtf16 {
                    text: text.display(),
                    utf16_code_units: source.string_units("").unwrap_or_default(),
                    citations: None,
                }];
                for image in images {
                    content.push(ContentBlock::Image {
                        source: lingxi_core::types::ImageSource::Base64 {
                            media_type: image.media_type.clone(),
                            data: image.base64.clone(),
                        },
                    });
                }
            }
            context.history.push(message);
            events.push(ClientEvent::MessageComplete {
                stop_reason: None,
                message: Some(client::protocol::message::MessageDto {
                    transcript_uuid: None,
                    skill_name: None,
                    role: "user".into(),
                    blocks: vec![client::protocol::message::MessageBlockDto::Text {
                        text: text.clone(),
                    }],
                    images: images
                        .iter()
                        .map(|image| client::protocol::message::MessageImageDto {
                            media_type: image.media_type.clone(),
                            url: format!("data:{};base64,{}", image.media_type, image.base64),
                        })
                        .collect(),
                    loop_wakeup: None,
                    visualization_context: None,
                    sent_at_ms: None,
                }),
            });
        }
        let state = side.state.lock().await;
        let interrupted = !turn_complete && state.running.is_some();
        let active_id = state.running.as_ref().and_then(|running| running.id);
        let submission_ids = state.submissions.keys().cloned().collect();
        let cancelled_turn_ids = state.cancelled_turn_ids.iter().copied().collect();
        drop(state);
        let creation_ids = self
            .state
            .lock()
            .await
            .requests
            .iter()
            .filter_map(|(request, result)| {
                (result.as_ref().ok().is_some_and(|side| side == id)).then(|| request.clone())
            })
            .collect();
        let checkpoint = Checkpoint {
            version: 1,
            host_session_id: self.parent.current_session_id().await.to_string(),
            side_id: id.to_owned(),
            context,
            submission_ids,
            cancelled_turn_ids,
            creation_ids,
            interrupted,
            turn_id: turn_id.or(active_id),
            slash: matches!(command, Some(ClientCommand::RunSlashCommand { .. })),
            terminal: if turn_complete {
                side.replay.terminal.lock().await.clone()
            } else {
                None
            },
        };
        let checkpoint_json = serde_json::to_string(&checkpoint)
            .map_err(|_| "Side Chat context could not be checkpointed")?;
        let events_json = events
            .iter()
            .map(serde_json::to_string)
            .collect::<Result<Vec<_>, _>>()
            .map_err(|_| "Side Chat replay could not be checkpointed")?;
        tracing::debug!(target: "harness_runtime::side_chat::recovery", side_id = id,
            history_messages = checkpoint.context.history.len(), checkpoint_bytes = checkpoint_json.len(),
            replay_events = events_json.len(), elapsed_us = started.elapsed().as_micros(), "side checkpoint");
        self.owner
            .emit(ClientEvent::SideChatCheckpoint {
                side_id: id.to_owned(),
                checkpoint_json,
                events_json,
            })
            .await;
        Ok(())
    }

    /// Rebuild independent primitives and history, without dispatching any saved command.
    pub async fn restore(
        self: &Arc<Self>,
        id: &str,
        checkpoint_json: &str,
        events_json: Vec<String>,
    ) -> Result<(), String> {
        let _creation = self.create_gate.lock().await;
        if !self.recoverable || self.state.lock().await.closed {
            return Err("Side Chat recovery is unavailable".into());
        }
        let checkpoint: Checkpoint = serde_json::from_str(checkpoint_json)
            .map_err(|_| "Invalid Side Chat recovery checkpoint")?;
        if checkpoint.version != 1
            || checkpoint.side_id != id
            || uuid::Uuid::parse_str(id).is_err()
            || checkpoint.host_session_id != self.parent.current_session_id().await.to_string()
        {
            return Err("Side Chat recovery checkpoint belongs to another conversation".into());
        }
        let state = self.state.lock().await;
        if state.ended_sides.contains(id) {
            return Err("Side Chat has ended".into());
        }
        let exists = state.sides.contains_key(id);
        drop(state);
        if exists {
            return self.emit_snapshot(id).await;
        }
        let mut events = events_json
            .iter()
            .map(|json| serde_json::from_str::<ClientEvent>(json))
            .collect::<Result<Vec<_>, _>>()
            .map_err(|_| "Invalid Side Chat recovery replay")?;
        // A dead process cannot own a pending permission or questionnaire. Never
        // reinstate grants, execute tools, or replay the interrupted admission.
        events.retain(|event| {
            !matches!(
                event,
                ClientEvent::SideChatPermission { .. } | ClientEvent::AskUserQuestion { .. }
            )
        });
        let last_started = events
            .iter()
            .rposition(|event| matches!(event, ClientEvent::TurnStarted { .. }));
        let last_ended = events
            .iter()
            .rposition(|event| matches!(event, ClientEvent::TurnEnded { .. }));
        let terminal_missing =
            last_started.is_some_and(|start| last_ended.is_none_or(|end| end < start));
        let replay = Arc::new(ReplaySink {
            side_id: id.to_owned(),
            owner: self.owner.clone(),
            child: None,
            events: Mutex::new(events),
            order: Mutex::new(()),
            activity: std::sync::Mutex::new(Activity::new()),
            recoverable: true,
            terminal: Mutex::new(None),
        });
        let permission = Arc::new(SidePermissionSink {
            replay: replay.clone(),
            child: None,
        });
        let session = self
            .factory
            .build(checkpoint.context, replay.clone(), permission)
            .await?;
        let mut cancelled_turn_ids = checkpoint
            .cancelled_turn_ids
            .into_iter()
            .collect::<HashSet<_>>();
        if checkpoint.interrupted {
            cancelled_turn_ids.extend(checkpoint.turn_id);
        }
        let side = Arc::new(Side {
            session,
            replay,
            state: Mutex::new(SideState {
                closing: false,
                submissions: checkpoint
                    .submission_ids
                    .into_iter()
                    .map(|id| (id, ()))
                    .collect(),
                running: None,
                cancelled_turn_ids,
            }),
        });
        {
            let mut state = self.state.lock().await;
            for request in checkpoint.creation_ids {
                state.requests.insert(request, Ok(id.to_owned()));
            }
            state.sides.insert(id.to_owned(), side.clone());
        }
        if checkpoint.interrupted {
            if checkpoint.slash {
                side.replay.publish(ClientEvent::SlashCommandResult { turn_id: checkpoint.turn_id, display: "The engine exited. This Side Chat request was interrupted and will not be replayed.".into(), is_error: true }).await;
            }
            side.replay
                .publish(ClientEvent::TurnEnded {
                    outcome: client::protocol::events::TurnOutcomeDto::Cancelled,
                    stop_reason: None,
                    cost: client::adapter::lowering::lower_cost_snapshot(
                        &side.session.orchestrator.snapshot_cost_real().await,
                    ),
                })
                .await;
        } else if terminal_missing {
            if let Some(terminal) = checkpoint.terminal {
                side.replay.publish(terminal).await;
            }
        }
        side.replay.publish(ClientEvent::SystemNotice { message: "This Side Chat was restored after the engine exited. Interrupted requests are not replayed.".into(), is_error: false }).await;
        self.checkpoint(id, None, false).await?;
        self.emit_snapshot(id).await
    }
}
