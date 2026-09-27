//! UI-visible display configuration for an agent.
//!
//! [`AgentDisplay`] is attached to each [`crate::context::SubagentContext`]
//! and surfaced through the host's UI to differentiate concurrent agents.

use serde::{Deserialize, Serialize};

/// Per-spawn display configuration. The `color` is assigned by
/// [`crate::color_manager::AgentColorManager`] so that concurrent agents
/// receive visually distinct colors; `icon` is taken from the agent
/// definition's optional `icon` field.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct AgentDisplay {
    /// Assigned color — guaranteed distinct from the 5 most-recent agents.
    pub color: AgentColor,
    /// Optional emoji / short icon to render alongside the agent label.
    pub icon: Option<String>,
}

/// Palette of colors used to differentiate concurrent agents in the UI.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub enum AgentColor {
    /// Cyan.
    Cyan,
    /// Magenta.
    Magenta,
    /// Yellow.
    Yellow,
    /// Green.
    Green,
    /// Blue.
    Blue,
    /// Red.
    Red,
    /// Orange.
    Orange,
    /// Purple.
    Purple,
    /// Pink.
    Pink,
    /// Teal.
    Teal,
}
