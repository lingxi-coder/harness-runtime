//! Private worker-side interaction pump. A single pending exchange is shared
//! across both channels, preventing unsolicited, duplicate or reordered replies.
use super::protocol::{self, ParentFrame, WorkerFrame, MAX_FRAME};
use super::{TeamHumanQuestion, TeamOwnerPermission, TeamProcessError};
use lingxi_core::types::utf16_json::Utf16JsonProjection as Json;
use std::collections::BTreeMap;
use std::io::{Read, Write};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex};
use tokio::sync::{oneshot, Mutex as AsyncMutex};
use tokio_util::sync::CancellationToken;
use tool_api::ask_user_question::AskUserQuestionExchange;

enum Pending {
    Permission {
        id: String,
        reply: oneshot::Sender<bool>,
    },
    Question {
        request: TeamHumanQuestion,
        reply: oneshot::Sender<Option<BTreeMap<String, String>>>,
    },
}

pub(super) struct ChildInteraction {
    next: AtomicU64,
    serial: AsyncMutex<()>,
    pending: Mutex<Option<Pending>>,
    pub cancel: CancellationToken,
}

impl ChildInteraction {
    pub fn start(cancel: CancellationToken) -> Result<Arc<Self>, TeamProcessError> {
        let bridge = Arc::new(Self {
            next: AtomicU64::new(1),
            serial: AsyncMutex::new(()),
            pending: Mutex::new(None),
            cancel,
        });
        let reader = bridge.clone();
        // A dedicated OS thread deliberately does not join Tokio shutdown. EOF
        // or the first invalid reply revokes the whole worker, including model
        // and admitted tools, rather than merely abandoning a parked request.
        std::thread::Builder::new()
            .name("team-parent-lease".into())
            .spawn(move || {
                if reader.read_replies().is_err() {
                    reader.cancel.cancel();
                }
            })?;
        Ok(bridge)
    }

    fn id(&self) -> Result<String, TeamProcessError> {
        let next = self
            .next
            .fetch_update(Ordering::Relaxed, Ordering::Relaxed, |n| n.checked_add(1))
            .map_err(|_| TeamProcessError::Protocol)?;
        Ok(format!("interaction-{next}"))
    }

    fn read_replies(&self) -> Result<(), TeamProcessError> {
        let mut input = std::io::stdin();
        loop {
            let mut prefix = [0; 4];
            input.read_exact(&mut prefix)?;
            let length = u32::from_be_bytes(prefix) as usize;
            if length == 0 || length > MAX_FRAME {
                return Err(TeamProcessError::Protocol);
            }
            let mut body = vec![0; length];
            input.read_exact(&mut body)?;
            let response: ParentFrame =
                serde_json::from_slice(&body).map_err(|_| TeamProcessError::Protocol)?;
            let pending = self
                .pending
                .lock()
                .map_err(|_| TeamProcessError::Protocol)?
                .take()
                .ok_or(TeamProcessError::Protocol)?;
            match (pending, response) {
                (
                    Pending::Permission { id, reply },
                    ParentFrame::OwnerPermission {
                        id: got,
                        allow_once,
                    },
                ) if id == got => {
                    reply
                        .send(allow_once)
                        .map_err(|_| TeamProcessError::Protocol)?;
                }
                (
                    Pending::Question { request, reply },
                    ParentFrame::HumanQuestion { id, answers },
                ) if request.id == id => {
                    protocol::validate_answers(&request, &answers)?;
                    reply
                        .send(answers)
                        .map_err(|_| TeamProcessError::Protocol)?;
                }
                _ => return Err(TeamProcessError::Protocol),
            }
        }
    }

    fn emit(&self, pending: Pending, frame: WorkerFrame) -> Result<(), TeamProcessError> {
        if self.cancel.is_cancelled() {
            return Err(TeamProcessError::Cancelled);
        }
        let bytes = protocol::encode(&frame)?;
        let mut slot = self
            .pending
            .lock()
            .map_err(|_| TeamProcessError::Protocol)?;
        if slot.is_some() {
            return Err(TeamProcessError::Protocol);
        }
        *slot = Some(pending);
        // stdout contains frames only. Never log tool input, broker payloads or
        // provider diagnostics; ordinary private runtime transcripts remain.
        let mut output = std::io::stdout().lock();
        output.write_all(&bytes)?;
        output.flush()?;
        Ok(())
    }

    pub async fn approve(
        &self,
        tool_name: String,
        input: serde_json::Value,
    ) -> Result<bool, TeamProcessError> {
        let _serial = self.serial.lock().await;
        let request = TeamOwnerPermission {
            id: self.id()?,
            tool_name,
            input,
        };
        protocol::validate_permission(&request)?;
        let (reply, response) = oneshot::channel();
        self.emit(
            Pending::Permission {
                id: request.id.clone(),
                reply,
            },
            WorkerFrame::OwnerPermission { request },
        )?;
        tokio::select! {
            biased;
            _ = self.cancel.cancelled() => Err(TeamProcessError::Cancelled),
            response = response => response.map_err(|_| TeamProcessError::Protocol),
        }
    }

    pub async fn ask(&self, exchange: AskUserQuestionExchange) -> Result<(), TeamProcessError> {
        let _serial = self.serial.lock().await;
        let request = question_request(self.id()?, &exchange)?;
        let (reply, response) = oneshot::channel();
        self.emit(
            Pending::Question {
                request: request.clone(),
                reply,
            },
            WorkerFrame::HumanQuestion {
                request: request.clone(),
            },
        )?;
        let answers = tokio::select! {
            biased;
            _ = self.cancel.cancelled() => return Err(TeamProcessError::Cancelled),
            response = response => response.map_err(|_| TeamProcessError::Protocol)?,
        };
        protocol::validate_answers(&request, &answers)?;
        if let Some(answers) = answers {
            let pairs = exchange
                .questions
                .into_iter()
                .map(|q| {
                    let key = q.question.string_at("").ok_or(TeamProcessError::Protocol)?;
                    let value = answers.get(key).ok_or(TeamProcessError::Protocol)?;
                    Ok((q.question.clone(), Json::from(value.clone())))
                })
                .collect::<Result<Vec<_>, TeamProcessError>>()?;
            exchange
                .resp_tx
                .send(pairs)
                .map_err(|_| TeamProcessError::Protocol)?;
        }
        // None drops the dedicated responder: a human refusal never routes to
        // the owner's TUI or synthesizes a default answer.
        Ok(())
    }
}

fn projection_text(value: &Json) -> Result<&str, TeamProcessError> {
    let text = value.string_at("").ok_or(TeamProcessError::Protocol)?;
    // Rust strings cannot carry lone UTF-16 surrogates. Refuse instead of
    // silently changing an original question key through lossy projection.
    if value.string_units("").as_deref() != Some(text.encode_utf16().collect::<Vec<_>>().as_slice())
    {
        return Err(TeamProcessError::Protocol);
    }
    Ok(text)
}

fn question_request(
    id: String,
    exchange: &AskUserQuestionExchange,
) -> Result<TeamHumanQuestion, TeamProcessError> {
    if !(1..=4).contains(&exchange.questions.len()) {
        return Err(TeamProcessError::Protocol);
    }
    let mut questions = Vec::new();
    let mut text = String::new();
    for q in &exchange.questions {
        let question = projection_text(&q.question)?;
        questions.push(question.to_owned());
        text.push_str(projection_text(&q.header)?);
        text.push_str("\n");
        text.push_str(question);
        text.push_str(if q.multi_select {
            "\nChoose one or more:\n"
        } else {
            "\nChoose one:\n"
        });
        if !(2..=4).contains(&q.options.len()) {
            return Err(TeamProcessError::Protocol);
        }
        for option in &q.options {
            text.push_str("- ");
            text.push_str(projection_text(&option.label)?);
            text.push_str(": ");
            text.push_str(projection_text(&option.description)?);
            text.push('\n');
            if text.len() > 8192 {
                return Err(TeamProcessError::Protocol);
            }
        }
    }
    let request = TeamHumanQuestion {
        id,
        questions,
        text,
    };
    protocol::validate_question(&request)?;
    Ok(request)
}
