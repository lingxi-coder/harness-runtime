//! Host-owned media execution and routing.
#![forbid(unsafe_code)]

pub mod attachment;
pub mod audio;
pub mod local;
mod service;

pub use attachment::{
    MediaAttachmentCleanupFence, MediaAttachmentHandle, MediaAttachmentHost, MediaAttachmentPermit,
    MediaAttachmentPhase, MediaAttachmentRevocation, MediaOperationCleanup,
};
pub use audio::{
    AudioOperationDefaults, AudioRouteMode, AudioRoutingSettings, CloudAudioCredentialSource,
    CloudAudioHost, CloudBinding, OfflineAudioBackend, UnifiedAudioService,
};
pub use local::LocalMediaConnector;
pub use service::{MediaRuntimeBuilder, UnifiedMediaService};
