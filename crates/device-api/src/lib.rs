//! `device-api` — what a host's device can do, as traits and plain data.
//!
//! The camera, the location fix, notifications, the clipboard, sharing, device
//! status, haptics, deep links, the calendar, contacts and the audio service
//! (recording, transcription, speech). A platform implements the ones it has;
//! the engine's tools and the Local App service both call through them, which
//! is why they sit in a crate of their own with no workspace dependencies:
//! neither of those should have to compile the other to name a camera.
//!
//! The engine's `core::host` re-exports every module here under its old path.

#![forbid(unsafe_code)]
// Documentation debt, not a decision that docs do not matter: these modules
// moved here from the engine's `core::host` with their public items as they
// were. The lint stays `warn` at the workspace level so a NEW crate still
// inherits the requirement; this allow is scoped here so the debt is visible
// per crate and can be repaid by deleting this line.
#![allow(missing_docs)]

pub mod audio;
pub mod calendar;
pub mod camera;
pub mod clipboard;
pub mod contacts;
pub mod deep_link;
pub mod device_status;
pub mod haptics;
pub mod location;
pub mod notification;
pub mod share;
pub mod stt;
pub mod tts;
pub mod voice;

pub use audio::{
    AudioCapabilitySnapshot, AudioError, AudioErrorKind, AudioInitiator, AudioOperation,
    AudioOperationContext, AudioOperationId, AudioOperationKind, AudioOperationReadiness,
    AudioOperationSuccess, AudioOwner, AudioReadinessState, AudioRecordingHandle, AudioService,
    AudioStatus,
};
pub use calendar::{CalendarError, CalendarEvent, CalendarProvider, CalendarQuery};
pub use camera::{CameraControl, CameraError, CameraPosition, CapturePhotoOpts, CapturedImage};
pub use clipboard::{Clipboard, ClipboardError};
pub use contacts::{Contact, ContactsError, ContactsProvider, ContactsQuery};
pub use deep_link::{DeepLinkError, DeepLinkOpener};
pub use device_status::{DeviceStatus, DeviceStatusError, DeviceStatusProvider};
pub use haptics::{HapticError, HapticService, HapticStyle};
pub use location::{LocationError, LocationFix, LocationProvider};
pub use notification::{NotificationError, NotificationRequest, NotificationService};
pub use share::{ShareError, SharePayload, ShareResult, SharingService};
pub use stt::SttTranscript;
pub use tts::TtsAudio;
pub use voice::VoiceRecording;
