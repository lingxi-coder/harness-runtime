//! Embedded Codex-derived WebRTC transport. The host owns the Tokio runtime.
//! No process entrypoint, process exits, device access or global environment changes.
mod audio_track;
pub mod incoming;
mod transport;
mod transport_runtime;
pub use transport::{AnswerOutcome, Transport as RtcSession};

pub use incoming::ReceivedRtp;

mod video_track;
pub use video_track::VideoSource;

mod config;
pub use config::{IceConfig, IceServer, MdnsMode, SessionConfig};
