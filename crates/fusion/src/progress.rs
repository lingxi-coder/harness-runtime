//! Progress fan-out helper.

use platform_api::{FusionProgress, FusionStage};
use tokio::sync::mpsc::Sender;

/// Best-effort progress send. A closed OR FULL channel is ignored (F005):
/// progress is a lossy side-channel, never a pipeline the orchestrator's own
/// stage transitions should back-pressure on. `try_send` (not `.send().await`)
/// is what makes that true — a slow/stalled consumer must not delay the
/// panel/analyst stage it is merely observing.
pub fn emit(
    progress: &Option<Sender<FusionProgress>>,
    stage: FusionStage,
    panel_id: Option<String>,
    message: impl Into<String>,
) {
    if let Some(tx) = progress {
        let _ = tx.try_send(FusionProgress {
            stage,
            panel_id,
            message: message.into(),
            realized_output_tokens: None,
            egress_profiles: None,
            panels_allocated: None,
        });
    }
}

/// [Round-12 finding [3]] Same as [`emit`], but publishes how many panels the
/// SPAWNER has provably allocated a child for so far.
///
/// Only the two panel-stage emitters have that number
/// (`panel::PanelDispatch::allocated_count`), and only they may send it: on
/// every other event `panels_allocated` stays `None`, which consumers read as
/// "no figure published" rather than "zero allocated". See
/// [`platform_api::FusionProgress::panels_allocated`] for why the resolved
/// `total` on the stage cannot answer the same question.
pub fn emit_with_allocated(
    progress: &Option<Sender<FusionProgress>>,
    stage: FusionStage,
    panel_id: Option<String>,
    message: impl Into<String>,
    panels_allocated: u8,
) {
    if let Some(tx) = progress {
        let _ = tx.try_send(FusionProgress {
            stage,
            panel_id,
            message: message.into(),
            realized_output_tokens: None,
            egress_profiles: None,
            panels_allocated: Some(panels_allocated),
        });
    }
}

/// [Finding 12; round-3 review B2 extends this with `egress_profiles`] Same
/// as [`emit`], but carries this run's realized output-token spend so far —
/// for a caller that tracks its own token budget and needs to charge
/// already-billed spend even when the overall call ends in
/// `Err` — and, when the orchestrator already knows the resolved egress
/// profile set at the emission point, that list too (see
/// `tasks::handlers::local_fusion`'s failure-path `<egress-profiles>`
/// disclosure).
pub fn emit_with_realized_tokens(
    progress: &Option<Sender<FusionProgress>>,
    stage: FusionStage,
    message: impl Into<String>,
    realized_output_tokens: u64,
    egress_profiles: Option<Vec<String>>,
) {
    if let Some(tx) = progress {
        let _ = tx.try_send(FusionProgress {
            stage,
            panel_id: None,
            message: message.into(),
            realized_output_tokens: Some(realized_output_tokens),
            egress_profiles,
            panels_allocated: None,
        });
    }
}
