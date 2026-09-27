//! Process-wide color assignment for concurrent agents.
//!
//! [`AgentColorManager`] hands out [`AgentColor`] values such that an agent
//! never gets the same color as one of the most-recent 5 spawns. The state
//! is internally guarded by [`std::sync::Mutex`] so the manager can be shared
//! via [`std::sync::Arc`].

#![allow(clippy::unwrap_used)]

use crate::display::AgentColor;
use protocol::AgentId;
use std::collections::{HashMap, VecDeque};
use std::sync::Mutex;

/// Hands out [`AgentColor`] values for new agent spawns.
///
/// The manager remembers the assignment per agent so that repeated lookups
/// for the same id return the same color, and tracks the 5 most-recent
/// assignments so consecutive spawns get visually distinct colors.
pub struct AgentColorManager {
    /// All colors the manager may hand out, in palette order.
    available: Vec<AgentColor>,
    /// Color currently in use for each known agent.
    assigned: Mutex<HashMap<AgentId, AgentColor>>,
    /// Sliding window of the most-recent assignments (cap = 5).
    recent: Mutex<VecDeque<AgentColor>>,
}

impl AgentColorManager {
    /// Construct a fresh manager with the full 10-color palette.
    #[must_use]
    pub fn new() -> Self {
        Self {
            available: vec![
                AgentColor::Cyan,
                AgentColor::Magenta,
                AgentColor::Yellow,
                AgentColor::Green,
                AgentColor::Blue,
                AgentColor::Red,
                AgentColor::Orange,
                AgentColor::Purple,
                AgentColor::Pink,
                AgentColor::Teal,
            ],
            assigned: Mutex::new(HashMap::new()),
            recent: Mutex::new(VecDeque::new()),
        }
    }

    /// Assign (or look up) the color for `agent`.
    ///
    /// The first time `agent` is seen, the next palette entry that isn't in
    /// the recent-5 window is selected; subsequent calls return the same
    /// color. If the entire palette is in the recent window (only possible
    /// for very small palettes), the function falls back to [`AgentColor::Cyan`].
    pub fn assign(&self, agent: &AgentId) -> AgentColor {
        let mut assigned = self.assigned.lock().unwrap();
        if let Some(c) = assigned.get(agent) {
            return *c;
        }
        let recent = self.recent.lock().unwrap();
        let color = self
            .available
            .iter()
            .find(|c| !recent.contains(c))
            .copied()
            .unwrap_or(AgentColor::Cyan);
        assigned.insert(*agent, color);
        drop(recent);
        let mut recent = self.recent.lock().unwrap();
        recent.push_back(color);
        if recent.len() > 5 {
            recent.pop_front();
        }
        color
    }

    /// Release the color held by `agent`. Subsequent calls to
    /// [`Self::assign`] for the same id will pick a fresh color.
    pub fn release(&self, agent: &AgentId) {
        self.assigned.lock().unwrap().remove(agent);
    }
}

impl Default for AgentColorManager {
    fn default() -> Self {
        Self::new()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn distinct_agents_get_distinct_colors() {
        let m = AgentColorManager::new();
        let a = AgentId::new();
        let b = AgentId::new();
        assert_ne!(m.assign(&a), m.assign(&b));
    }
}
