//! Fixed local media service failures for diagnostics without native error text.

#[derive(Debug)]
pub(crate) enum ServiceFailure {
    #[cfg(feature = "desktop-devices")]
    Playout,
    Render,
    Capture,
    Device,
    Send,
}

impl std::fmt::Display for ServiceFailure {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(match self {
            #[cfg(feature = "desktop-devices")]
            Self::Playout => "audio playout failed",
            Self::Render => "audio rendering failed",
            Self::Capture => "audio capture processing failed",
            Self::Device => "audio device failed",
            Self::Send => "voice audio send failed",
        })
    }
}

impl std::error::Error for ServiceFailure {}
