//! Session-owned, detached Mod output.

use crate::protocol::text::ClientTextDto;
use serde::{Deserialize, Deserializer, Serialize};
use serde_json::value::RawValue;
use std::collections::HashMap;

/// One detached Mod result. Text retains exact UTF-16 units across the JSON
/// and native boundaries; `Status` with no text clears the plugin's status.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
#[cfg_attr(feature = "uniffi", derive(uniffi::Enum))]
#[serde(tag = "type", rename_all = "snake_case")]
#[non_exhaustive]
pub enum ModSessionEventPayload {
    /// A separate notice produced by `turn.complete`.
    Notice {
        /// Notice text.
        message: ClientTextDto,
        /// Whether this is an error notice.
        is_error: bool,
    },
    /// A plugin transcript log.
    Log {
        /// Plugin identity.
        plugin: String,
        /// Log text.
        text: ClientTextDto,
    },
    /// A transient plugin toast.
    Toast {
        /// Plugin identity.
        plugin: String,
        /// Toast text.
        text: ClientTextDto,
        /// Requested timeout in milliseconds.
        timeout_ms: u64,
    },
    /// A plugin status update, or a clear when `text` is null.
    Status {
        /// Plugin identity.
        plugin: String,
        /// New status text, or none to clear it.
        text: Option<ClientTextDto>,
    },
    /// A transient Mod notification without an expiry field.
    Notify {
        /// Plugin identity.
        plugin: String,
        /// Notification text.
        text: ClientTextDto,
    },
}

// Internally tagged serde derives buffer through `Content` and lose support
// for RawValue-backed ClientTextDto. Capture members as raw JSON before
// decoding each field, exactly as the existing ClientEvent decoder does.
struct RawPayload(HashMap<String, Box<RawValue>>);

impl<'de> Deserialize<'de> for RawPayload {
    fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
    where
        D: Deserializer<'de>,
    {
        struct Visitor;
        impl<'de> serde::de::Visitor<'de> for Visitor {
            type Value = RawPayload;

            fn expecting(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
                formatter.write_str("a Mod session payload object")
            }

            fn visit_map<A>(self, mut map: A) -> Result<Self::Value, A::Error>
            where
                A: serde::de::MapAccess<'de>,
            {
                let mut fields = HashMap::new();
                while let Some(key) = map.next_key::<String>()? {
                    fields.insert(key, map.next_value::<Box<RawValue>>()?);
                }
                Ok(RawPayload(fields))
            }
        }
        deserializer.deserialize_map(Visitor)
    }
}

fn required<T, E>(fields: &mut HashMap<String, Box<RawValue>>, name: &'static str) -> Result<T, E>
where
    T: serde::de::DeserializeOwned,
    E: serde::de::Error,
{
    let raw = fields.remove(name).ok_or_else(|| E::missing_field(name))?;
    serde_json::from_str(raw.get()).map_err(E::custom)
}

fn optional<T, E>(
    fields: &mut HashMap<String, Box<RawValue>>,
    name: &'static str,
) -> Result<Option<T>, E>
where
    T: serde::de::DeserializeOwned,
    E: serde::de::Error,
{
    fields.remove(name).map_or(Ok(None), |raw| {
        serde_json::from_str(raw.get()).map_err(E::custom)
    })
}

impl<'de> Deserialize<'de> for ModSessionEventPayload {
    fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
    where
        D: Deserializer<'de>,
    {
        let RawPayload(mut fields) = RawPayload::deserialize(deserializer)?;
        let tag = required::<String, D::Error>(&mut fields, "type")?;
        match tag.as_str() {
            "notice" => Ok(Self::Notice {
                message: required(&mut fields, "message")?,
                is_error: required(&mut fields, "is_error")?,
            }),
            "log" => Ok(Self::Log {
                plugin: required(&mut fields, "plugin")?,
                text: required(&mut fields, "text")?,
            }),
            "toast" => Ok(Self::Toast {
                plugin: required(&mut fields, "plugin")?,
                text: required(&mut fields, "text")?,
                timeout_ms: required(&mut fields, "timeout_ms")?,
            }),
            "status" => Ok(Self::Status {
                plugin: required(&mut fields, "plugin")?,
                text: optional(&mut fields, "text")?,
            }),
            "notify" => Ok(Self::Notify {
                plugin: required(&mut fields, "plugin")?,
                text: required(&mut fields, "text")?,
            }),
            _ => Err(serde::de::Error::unknown_variant(
                &tag,
                &["notice", "log", "toast", "status", "notify"],
            )),
        }
    }
}
