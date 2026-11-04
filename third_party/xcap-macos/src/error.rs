use std::sync::PoisonError;

use thiserror::Error;

#[derive(Debug, Error)]
pub enum XCapError {
    #[error("Not supported")]
    NotSupported,
    #[error("{0}")]
    Error(String),
    #[error("StdSyncPoisonError {0}")]
    StdSyncPoisonError(String),
    #[error("Invalid capture region: {0}")]
    InvalidCaptureRegion(String),

    #[cfg(target_os = "macos")]
    #[error("Objc2CoreGraphicsCGError {:?}", 0)]
    Objc2CoreGraphicsCGError(objc2_core_graphics::CGError),
}

impl XCapError {
    pub fn new<S: ToString>(err: S) -> Self {
        XCapError::Error(err.to_string())
    }
}

pub type XCapResult<T> = Result<T, XCapError>;

impl<T> From<PoisonError<T>> for XCapError {
    fn from(value: PoisonError<T>) -> Self {
        XCapError::StdSyncPoisonError(value.to_string())
    }
}
