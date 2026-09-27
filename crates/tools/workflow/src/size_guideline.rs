//! Dynamic workflow-size guideline (`workflowSizeGuideline`) — the `/config`
//! setting that biases how many subagents a workflow fans out into, and the
//! byte-exact Workflow-tool prompt appendix it produces (parity 2.1.207).
//!
//! Ported from the 2.1.207 binary (`Iir`/`jAs`/`WAs`/`VAs`/`xat`):
//!
//! ```js
//! zvd = ["unrestricted","small","medium","large"];        // enum order
//! Pyo = { small: 5, medium: 15, large: 50 };               // agent caps
//! function xat(e){                                          // normalize
//!   if (e==="small"||e==="medium"||e==="large") return e;
//!   return "unrestricted";
//! }
//! function jAs(e){                                          // one guideline line
//!   if (!(e in Pyo)) return e;
//!   return `${e} — keep workflows under ${Pyo[e]} agents`;
//! }
//! function WAs(){
//!   return "This is a guideline, not a hard limit — follow it unless the user's prompt calls for a different scale.";
//! }
//! function VAs(e){                                          // prompt appendix
//!   let t = Jvd(e);                                         // session-frozen
//!   if (t!=="unrestricted")
//!     return `\nThe user has configured a workflow size guideline in /config: ${jAs(t)}. ${WAs()}`;
//!   return "";
//! }
//! // tool registration: async prompt(){ return qAs + VAs(St().workflowSizeGuideline) }
//! ```
//!
//! The em-dashes are U+2014. The value is frozen per session in the binary via
//! `Jvd`'s `GAs` cache; in LingXi the composition root reads the persisted
//! setting once and hands the frozen value to [`crate::WorkflowTool`], so the
//! per-session freeze is structural rather than a runtime cache.

/// The four `workflowSizeGuideline` values, in the binary's `zvd` order
/// (`unrestricted` first). `unrestricted` is the CLEARED state and produces no
/// prompt appendix; it is NOT the default.
///
/// The default is [`Self::Medium`] (2.1.219). Oracle @234469666:
/// `var rLs=["unrestricted","small","medium","large"], _Td="medium",
/// oko={small:5,medium:15,large:50}`, with
/// `Oft(e)` returning `{size:_Td,isDefault:!0}` when no source set it. This
/// port previously defaulted to `Unrestricted`, i.e. shipped with NO agent cap
/// where the oracle advises under 15.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum WorkflowSizeGuideline {
    /// No guideline — the CLEARED state. No prompt appendix, no agent cap.
    Unrestricted,
    /// Keep workflows under 5 agents.
    Small,
    /// Keep workflows under 15 agents. The shipped DEFAULT (`_Td`).
    #[default]
    Medium,
    /// Keep workflows under 50 agents.
    Large,
}

impl WorkflowSizeGuideline {
    /// The wire values in `zvd` order (`["unrestricted","small","medium","large"]`).
    /// Used to validate the `/config workflowSizeGuideline=…` shorthand.
    pub const ALL_WIRE: [&'static str; 4] = ["unrestricted", "small", "medium", "large"];

    /// Binary `xat(e)`: normalize an arbitrary string to a guideline. Anything
    /// that is not exactly `small`/`medium`/`large` (including `unrestricted`
    /// and any unknown value) collapses to [`Self::Unrestricted`].
    #[must_use]
    pub fn from_wire(s: &str) -> Self {
        match s {
            "small" => Self::Small,
            "medium" => Self::Medium,
            "large" => Self::Large,
            _ => Self::Unrestricted,
        }
    }

    /// The wire string for this guideline.
    #[must_use]
    pub const fn as_wire(self) -> &'static str {
        match self {
            Self::Unrestricted => "unrestricted",
            Self::Small => "small",
            Self::Medium => "medium",
            Self::Large => "large",
        }
    }

    /// Binary `Pyo[e]` / `Kvd(e)`: the soft agent cap, or `None` for
    /// `unrestricted`. `small` → 5, `medium` → 15, `large` → 50.
    #[must_use]
    pub const fn agent_cap(self) -> Option<u32> {
        match self {
            Self::Unrestricted => None,
            Self::Small => Some(5),
            Self::Medium => Some(15),
            Self::Large => Some(50),
        }
    }

    /// Binary `jAs(e)`: the single guideline line `"{size} — keep workflows
    /// under {cap} agents"` (U+2014 em-dash). `unrestricted` has no cap so it
    /// returns just its own name (matching `jAs`'s `if(!(e in Pyo)) return e`),
    /// though [`Self::prompt_appendix`] never emits it for `unrestricted`.
    #[must_use]
    pub fn guideline_line(self) -> String {
        match self.agent_cap() {
            Some(cap) => format!(
                "{} \u{2014} keep workflows under {cap} agents",
                self.as_wire()
            ),
            None => self.as_wire().to_string(),
        }
    }

    /// Binary `VAs(e)`: the Workflow-tool prompt/description appendix. Empty for
    /// [`Self::Unrestricted`]; otherwise a leading newline followed by the
    /// "The user has configured a workflow size guideline in /config: …"
    /// sentence and the [`ADVISORY`] follow-up. Appended verbatim to the tool's
    /// base description (`qAs + VAs(size)`).
    #[must_use]
    pub fn prompt_appendix(self) -> String {
        if self == Self::Unrestricted {
            return String::new();
        }
        format!(
            "\nThe user has configured a workflow size guideline in /config: {}. {ADVISORY}",
            self.guideline_line()
        )
    }
}

/// Binary `WAs()`: the advisory sentence appended after the guideline line
/// (U+2014 em-dash; ASCII apostrophe in "user's").
pub const ADVISORY: &str = "This is a guideline, not a hard limit \u{2014} follow it unless the user's prompt calls for a different scale.";

/// Convenience: [`WorkflowSizeGuideline::prompt_appendix`] over a raw
/// (already-normalized-or-not) wire string. Unknown / `unrestricted` → empty.
#[must_use]
pub fn prompt_appendix_for(size: &str) -> String {
    WorkflowSizeGuideline::from_wire(size).prompt_appendix()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn from_wire_normalizes_like_xat() {
        assert_eq!(
            WorkflowSizeGuideline::from_wire("small"),
            WorkflowSizeGuideline::Small
        );
        assert_eq!(
            WorkflowSizeGuideline::from_wire("medium"),
            WorkflowSizeGuideline::Medium
        );
        assert_eq!(
            WorkflowSizeGuideline::from_wire("large"),
            WorkflowSizeGuideline::Large
        );
        // unrestricted + any unknown → Unrestricted.
        assert_eq!(
            WorkflowSizeGuideline::from_wire("unrestricted"),
            WorkflowSizeGuideline::Unrestricted
        );
        assert_eq!(
            WorkflowSizeGuideline::from_wire("SMALL"),
            WorkflowSizeGuideline::Unrestricted
        );
        assert_eq!(
            WorkflowSizeGuideline::from_wire(""),
            WorkflowSizeGuideline::Unrestricted
        );
    }

    #[test]
    fn agent_caps_match_pyo() {
        assert_eq!(WorkflowSizeGuideline::Small.agent_cap(), Some(5));
        assert_eq!(WorkflowSizeGuideline::Medium.agent_cap(), Some(15));
        assert_eq!(WorkflowSizeGuideline::Large.agent_cap(), Some(50));
        assert_eq!(WorkflowSizeGuideline::Unrestricted.agent_cap(), None);
    }

    #[test]
    fn all_wire_order_matches_zvd() {
        assert_eq!(
            WorkflowSizeGuideline::ALL_WIRE,
            ["unrestricted", "small", "medium", "large"]
        );
    }

    #[test]
    fn advisory_is_byte_exact() {
        assert_eq!(
            ADVISORY,
            "This is a guideline, not a hard limit \u{2014} follow it unless the user's prompt calls for a different scale."
        );
    }

    #[test]
    fn prompt_appendix_small_is_byte_exact() {
        assert_eq!(
            WorkflowSizeGuideline::Small.prompt_appendix(),
            "\nThe user has configured a workflow size guideline in /config: small \u{2014} keep workflows under 5 agents. This is a guideline, not a hard limit \u{2014} follow it unless the user's prompt calls for a different scale."
        );
    }

    #[test]
    fn prompt_appendix_medium_is_byte_exact() {
        assert_eq!(
            WorkflowSizeGuideline::Medium.prompt_appendix(),
            "\nThe user has configured a workflow size guideline in /config: medium \u{2014} keep workflows under 15 agents. This is a guideline, not a hard limit \u{2014} follow it unless the user's prompt calls for a different scale."
        );
    }

    #[test]
    fn prompt_appendix_large_is_byte_exact() {
        assert_eq!(
            WorkflowSizeGuideline::Large.prompt_appendix(),
            "\nThe user has configured a workflow size guideline in /config: large \u{2014} keep workflows under 50 agents. This is a guideline, not a hard limit \u{2014} follow it unless the user's prompt calls for a different scale."
        );
    }

    #[test]
    fn prompt_appendix_unrestricted_is_empty() {
        assert!(WorkflowSizeGuideline::Unrestricted
            .prompt_appendix()
            .is_empty());
        // The free helper agrees for unrestricted + unknown.
        assert!(prompt_appendix_for("unrestricted").is_empty());
        assert!(prompt_appendix_for("bogus").is_empty());
    }

    #[test]
    fn prompt_appendix_for_delegates() {
        assert_eq!(
            prompt_appendix_for("small"),
            WorkflowSizeGuideline::Small.prompt_appendix()
        );
    }
}
