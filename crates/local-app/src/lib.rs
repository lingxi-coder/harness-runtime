//! Schema-2 Local App source library and host-owned Agent/Chat contracts.
//! Applications, release grants and durable Chat data live independently of
//! their authoring conversation. Native hosts supply process and browser
//! lifecycle adapters; the model-facing workflow is described by [`skill`].

#![forbid(unsafe_code)]

pub mod skill;
/// Schema-2, session-independent source applications and host lifecycle API.
pub mod v2;

/// Tool name the agent calls to publish, list and read apps.
pub const TOOL_NAME: &str = "LocalApp";
