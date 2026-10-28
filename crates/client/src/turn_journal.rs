//! Shared decoding of the JSON stored in mobile turn-event envelopes.
//!
//! The JSON is the original `ClientEvent` wire form, including exact UTF-16
//! text. Decode it through the protocol's Rust serde implementation rather
//! than asking every native consumer to reproduce the event schema.

use crate::protocol::error::ClientError;
use crate::protocol::events::ClientEvent;

/// A decoded turn event and the identity carried by its original envelope.
///
/// `sequence` is present only when the host confirmed a retained journal
/// sequence. Its absence does not prove that a failed write left no event on
/// disk. A consumer must not advance a durable cursor for such an event.
#[derive(Debug, Clone, PartialEq)]
#[cfg_attr(feature = "uniffi", derive(uniffi::Record))]
pub struct TurnJournalEventDto {
    /// Session that owns the event.
    pub session_id: String,
    /// Turn within that session that owns the event.
    pub turn_id: u64,
    /// Confirmed retained sequence, absent for unconfirmed live delivery.
    pub sequence: Option<u64>,
    /// Complete original event, decoded without losing its text code units.
    pub event: ClientEvent,
}

/// Decode a turn event's original `ClientEvent` JSON using the client protocol.
///
/// Unknown or invalid events and nested turn envelopes/recovery snapshots are
/// rejected. Known events are decoded without maintaining a second allowlist:
/// the host decides which events belong to the turn journal, while clients
/// decide how a decoded event is projected.
#[cfg_attr(feature = "uniffi", uniffi::export)]
pub fn decode_turn_journal_event(event_json: String) -> Result<ClientEvent, ClientError> {
    let event = serde_json::from_str::<ClientEvent>(&event_json).map_err(|error| {
        ClientError::Protocol {
            message: format!("invalid turn event: {error}"),
        }
    })?;
    match event {
        ClientEvent::TurnRecoveryState { .. }
        | ClientEvent::TurnEventReplay { .. }
        | ClientEvent::TurnEventLiveUnretained { .. }
        | ClientEvent::TurnAttachStarted { .. }
        | ClientEvent::TurnAttachFinished { .. }
        | ClientEvent::QueuedTurnStatus { .. }
        | ClientEvent::ModSessionEvent { .. } => Err(ClientError::Protocol {
            message: "turn event must not contain a recovery, delivery, or session envelope".into(),
        }),
        _ => Ok(event),
    }
}

/// Decode a sequenced replay or unconfirmed live envelope as one atomic value.
///
/// This function takes the original envelope rather than separate identity and
/// JSON arguments, preventing accidental pairing of a payload with another
/// turn's cursor. The caller still checks the identity against its active
/// interest before applying the event or performing any side effects.
#[cfg_attr(feature = "uniffi", uniffi::export)]
pub fn decode_turn_event_envelope(
    envelope: ClientEvent,
) -> Result<TurnJournalEventDto, ClientError> {
    let (session_id, turn_id, sequence, event_json) = match envelope {
        ClientEvent::TurnEventReplay {
            session_id,
            turn_id,
            sequence,
            event_json,
        } => (session_id, turn_id, Some(sequence), event_json),
        ClientEvent::TurnEventLiveUnretained {
            session_id,
            turn_id,
            event_json,
        } => (session_id, turn_id, None, event_json),
        _ => {
            return Err(ClientError::Protocol {
                message: "expected a turn event delivery envelope".into(),
            });
        }
    };
    if session_id.is_empty() || sequence == Some(0) {
        return Err(ClientError::Protocol {
            message: "invalid turn event delivery identity or sequence".into(),
        });
    }
    Ok(TurnJournalEventDto {
        session_id,
        turn_id,
        sequence,
        event: decode_turn_journal_event(event_json)?,
    })
}
