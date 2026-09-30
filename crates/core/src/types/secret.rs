//! Secret newtype + redactable DTOs.
//!
//! [`Secret<T>`] is a thin wrapper around [`secrecy::SecretBox<T>`]. The inner
//! buffer is zeroized on drop. `Debug`/`Display` always print `<redacted>`.
//! Code review can grep `expose_secret(` to audit every read.

use secrecy::{ExposeSecret, SecretBox};
use serde::{Deserialize, Serialize};
use std::fmt;
use zeroize::Zeroize;

/// Wrap any [`Zeroize`] type so it cannot leak via `Debug`/`Display`/`Clone`.
///
/// The inner value is held in a [`SecretBox`], which zeroes its memory on drop.
/// `Secret<T>` deliberately does NOT implement `Clone` — every copy must be a
/// conscious decision made by exposing the secret and re-wrapping it.
pub struct Secret<T: Zeroize>(SecretBox<T>);

impl<T: Zeroize + Default> Secret<T> {
    /// Wrap `value` in a [`Secret`]. The value is moved into a heap-allocated
    /// [`SecretBox`] and will be zeroized on drop.
    pub fn new(value: T) -> Self {
        Self(SecretBox::new(Box::new(value)))
    }

    /// Borrow the inner secret. Audit every call site — search for
    /// `expose_secret(` to enumerate every read of secret material.
    #[must_use]
    pub fn expose_secret(&self) -> &T {
        self.0.expose_secret()
    }
}

impl<T: Zeroize> fmt::Debug for Secret<T> {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "Secret(<redacted>)")
    }
}

impl<T: Zeroize> fmt::Display for Secret<T> {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "<redacted>")
    }
}

// Explicit serde delegation so `Secret<T>` round-trips when the user opts in.
// We intentionally do NOT derive `Clone` — clones must be deliberate.
impl<T: Serialize + Zeroize> Serialize for Secret<T> {
    fn serialize<S: serde::Serializer>(&self, s: S) -> Result<S::Ok, S::Error> {
        self.0.expose_secret().serialize(s)
    }
}
impl<'de, T: Deserialize<'de> + Zeroize + Default> Deserialize<'de> for Secret<T> {
    fn deserialize<D: serde::Deserializer<'de>>(d: D) -> Result<Self, D::Error> {
        T::deserialize(d).map(Self::new)
    }
}

/// Encrypted byte payload returned from secure storage, plus user-visible
/// metadata describing what the bytes represent.
///
/// The byte payload is held in a [`Secret`] and is zeroized on drop. The
/// metadata is plain (non-secret) data describing the secret's provenance.
///
/// `Serialize`/`Deserialize` are provided so this DTO can cross the engine
/// boundary as part of an [`crate::types::effects::Effect`]. The inner bytes are
/// transmitted as-is via the `Secret<T>` serde delegation — callers are
/// responsible for choosing a secure transport.
#[derive(Serialize, Deserialize)]
pub struct SecureStorageData {
    bytes: Secret<Vec<u8>>,
    /// Non-secret metadata describing the secret payload.
    pub metadata: SecureStorageMetadata,
}

// Manual `Clone` impl: `Secret<T>` intentionally does not implement `Clone`,
// but `SecureStorageData` is a DTO that often needs to be cloned across module
// boundaries. We clone the inner bytes explicitly via `expose_secret_bytes()`
// so the clone is auditable and the original `Secret` lifecycle is unchanged.
impl Clone for SecureStorageData {
    fn clone(&self) -> Self {
        Self {
            bytes: Secret::new(self.bytes.expose_secret().clone()),
            metadata: self.metadata.clone(),
        }
    }
}

impl fmt::Debug for SecureStorageData {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("SecureStorageData")
            .field("bytes", &"<redacted>")
            .field("metadata", &self.metadata)
            .finish()
    }
}

impl SecureStorageData {
    /// Build a new [`SecureStorageData`] from raw bytes and metadata. The bytes
    /// are immediately wrapped in a [`Secret`].
    #[must_use]
    pub fn new(bytes: Vec<u8>, metadata: SecureStorageMetadata) -> Self {
        Self {
            bytes: Secret::new(bytes),
            metadata,
        }
    }

    /// Borrow the inner secret bytes. Audit every call site — search for
    /// `expose_secret_bytes(` to enumerate every read of secret material.
    #[must_use]
    pub fn expose_secret_bytes(&self) -> &[u8] {
        self.bytes.expose_secret()
    }
}

/// Plaintext metadata associated with a stored secret. Contains no secret
/// material and is safe to log or transmit.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SecureStorageMetadata {
    /// Wall-clock timestamp when the secret was created in storage.
    pub created_at: std::time::SystemTime,
    /// Wall-clock timestamp of the last successful access, if any.
    pub last_accessed: Option<std::time::SystemTime>,
    /// Kind of secret (e.g. `anthropic_api_key`). See [`SecretKindDto`].
    pub kind: SecretKindDto,
}

/// String form of `SecretKind` for DTOs (avoids cycle with `lingxi-secret`).
///
/// The wrapped string is a stable identifier (e.g. `anthropic_api_key`) chosen
/// by the secret subsystem; the shared types module does not constrain its value.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct SecretKindDto(
    /// Stable identifier for the secret kind.
    pub String,
);

/// Content that may contain user secrets — pre-redaction.
///
/// Wraps a string that has not yet been scanned for secrets. `Debug` prints
/// only the byte length to avoid accidental disclosure in logs. Callers that
/// need to scan or sanitize the content must call [`RedactableContent::expose_for_scan`].
///
/// `Serialize`/`Deserialize` are provided so this DTO can cross the engine
/// boundary as part of an [`crate::types::effects::Effect`]. The wrapped string is
/// transmitted as-is — `Debug` still hides it from logs.
#[derive(Clone, Serialize, Deserialize)]
pub struct RedactableContent(String);

impl fmt::Debug for RedactableContent {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "RedactableContent(<{}B>)", self.0.len())
    }
}

impl RedactableContent {
    /// Wrap `content` for downstream redaction. The string is held in plain
    /// memory — this type only blocks accidental disclosure via `Debug`.
    #[must_use]
    pub fn new(content: String) -> Self {
        Self(content)
    }

    /// Borrow the inner string for redaction scanning. Use sparingly; the
    /// returned `&str` may contain secrets.
    #[must_use]
    pub fn expose_for_scan(&self) -> &str {
        &self.0
    }

    /// Byte length of the wrapped content.
    #[must_use]
    pub fn len(&self) -> usize {
        self.0.len()
    }

    /// `true` if the wrapped content is empty.
    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.0.is_empty()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn secret_debug_redacts() {
        let s = Secret::new("sk-ant-superhot".to_string());
        let dbg = format!("{s:?}");
        assert!(!dbg.contains("sk-ant"), "leaked: {dbg}");
        assert!(dbg.contains("redacted"));
    }

    #[test]
    fn redactable_content_debug_redacts() {
        let r = RedactableContent::new("very_secret".into());
        assert!(!format!("{r:?}").contains("very_secret"));
    }

    #[test]
    fn secret_storage_data_debug_redacts() {
        let data = SecureStorageData::new(
            b"hot-bytes".to_vec(),
            SecureStorageMetadata {
                created_at: std::time::SystemTime::UNIX_EPOCH,
                last_accessed: None,
                kind: SecretKindDto("anthropic_api_key".into()),
            },
        );
        assert!(!format!("{data:?}").contains("hot-bytes"));
    }
}
