//! The Workflow tool's two model-facing oracle texts and the register that
//! edits them.
//!
//! This lives in the workflow RUNTIME crate rather than beside the tool because
//! both the tool (`tools/workflow`) and the `workflow-authoring` bundled skill
//! (`commands/core`) must read it, and §8.1 forbids a `command` crate from
//! depending on a `tool` crate. A root-level crate is the only shared floor,
//! and this is the one whose script API the reference documents.

use once_cell::sync::Lazy;

/// The tool description (claude-code v2.1.267 `t`), reproduced byte-for-byte.
/// A trailing newline (should an editor add one to the data file) is stripped so
/// the ORACLE text matches the binary exactly.
///
/// 2.1.267 SPLIT what 2.1.245 shipped as a single 19,200-byte description. This
/// file is now only the head — the part every request pays for. The 17 KB
/// script-writing reference moved to [`ORACLE_AUTHORING_SKILL`], served on
/// demand by the `workflow-authoring` bundled skill. Upstream's own changelog
/// puts the saving at "about 1k tokens instead of 5.7k".
///
/// ⛔ This is the oracle, not the shipped text. Never edit
/// `workflow_description.txt` to change what the model reads — register the
/// change in `workflow_description_divergences.json` instead, so it carries an
/// id and a reason. [`DESCRIPTION`] is what actually ships.
pub static ORACLE_DESCRIPTION: Lazy<String> = Lazy::new(|| {
    include_str!("workflow_description.txt")
        .trim_end_matches('\n')
        .to_string()
});

/// The authoring reference (claude-code v2.1.267 `wpn()`), reproduced
/// byte-for-byte in its DEFAULT rendering — the branch taken when no subagent
/// model is pinned.
///
/// Upstream's `wpn()` is a template literal, not a constant: three fragments are
/// interpolated as `${e?"":"…"}`, where `e` is `CLAUDE_CODE_SUBAGENT_MODEL_FORCE`.
/// A pinned deployment model makes per-agent `model` overrides meaningless, so
/// upstream drops the three passages that document them. Storing the *unforced*
/// text and subtracting those fragments (see [`MODEL_FORCE_OMISSIONS`]) keeps
/// this file a byte-for-byte oracle rather than a template the tests cannot
/// compare against the binary.
///
/// ⛔ Same rule as above: edits belong in the divergence register, targeted at
/// `"skill"`. [`AUTHORING_SKILL`] is what actually ships.
pub static ORACLE_AUTHORING_SKILL: Lazy<String> = Lazy::new(|| {
    include_str!("workflow_authoring_skill.txt")
        .trim_end_matches('\n')
        .to_string()
});

/// One named, reasoned local edit to the oracle description.
///
/// The `anchor` is an exact oracle substring that must occur EXACTLY ONCE;
/// `text` is inserted immediately after it. Anchoring rather than offsetting is
/// deliberate: when an oracle refresh reworders or removes the anchored passage,
/// composition fails by divergence id instead of quietly landing the insert in
/// the wrong paragraph.
#[derive(Debug, serde::Deserialize)]
pub struct DescriptionDivergence {
    pub id: String,
    /// Which of the two oracle texts this entry edits.
    pub target: DivergenceTarget,
    /// The finding this divergence answers, for the audit trail.
    #[allow(dead_code)]
    pub finding: String,
    pub reason: String,
    pub anchor: String,
    pub text: String,
}

/// Which byte-locked oracle text a divergence attaches to.
///
/// 2.1.267 split the description in two, and the two registered divergences
/// went to opposite halves: the local-app opt-in clause belongs with the
/// per-request description, while the `fusion()` hook belongs with the authoring
/// reference that documents every other script-body hook. Naming the target
/// keeps `compose` from searching the wrong document and silently reporting a
/// missing anchor.
#[derive(Debug, Clone, Copy, PartialEq, Eq, serde::Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum DivergenceTarget {
    Description,
    Skill,
}

/// The register of local divergences. See the `_doc` block in the data file for
/// why the old single-number lock was replaced.
pub static DESCRIPTION_DIVERGENCES: Lazy<Vec<DescriptionDivergence>> = Lazy::new(|| {
    #[derive(serde::Deserialize)]
    struct Register {
        divergences: Vec<DescriptionDivergence>,
    }
    let register: Register =
        serde_json::from_str(include_str!("workflow_description_divergences.json"))
            .expect("workflow_description_divergences.json is valid JSON");
    register.divergences
});

/// Apply the register to the oracle text.
///
/// Returns `Err` naming the divergence when its anchor is missing or ambiguous
/// — the loud failure that a byte-count lock could not give, because a count
/// cannot distinguish an intentional edit from accidental drift.
pub fn compose_description(
    oracle: &str,
    divergences: &[DescriptionDivergence],
    target: DivergenceTarget,
) -> Result<String, String> {
    let mut composed = oracle.to_string();
    for divergence in divergences.iter().filter(|d| d.target == target) {
        match composed.matches(divergence.anchor.as_str()).count() {
            1 => {}
            0 => {
                return Err(format!(
                    "divergence `{}`: its anchor is no longer present in the Workflow tool \
                     description. The oracle passage it attaches to was reworded or removed, so \
                     the insert has nowhere to go. Re-anchor it against the current oracle text \
                     (or retire the divergence) — do not delete this check.",
                    divergence.id
                ));
            }
            n => {
                return Err(format!(
                    "divergence `{}`: its anchor occurs {n} times, so where the insert lands is \
                     ambiguous. Lengthen the anchor until it is unique.",
                    divergence.id
                ));
            }
        }
        let at = composed
            .find(divergence.anchor.as_str())
            .expect("occurrence count checked above")
            + divergence.anchor.len();
        composed.insert_str(at, &divergence.text);
    }
    Ok(composed)
}

/// The shipped tool description: the oracle head plus its registered divergences.
pub static DESCRIPTION: Lazy<String> = Lazy::new(|| {
    compose_description(
        &ORACLE_DESCRIPTION,
        &DESCRIPTION_DIVERGENCES,
        DivergenceTarget::Description,
    )
    .unwrap_or_else(|error| panic!("Workflow tool description: {error}"))
});

/// The three passages upstream omits when a subagent model is pinned
/// (`${e?"":"…"}` in `wpn()`, `e` = `CLAUDE_CODE_SUBAGENT_MODEL_FORCE`).
///
/// Each must occur EXACTLY ONCE in the oracle text — the same anchor discipline
/// the divergence register uses, and for the same reason: a fragment that stops
/// matching after an oracle refresh must fail loudly instead of silently
/// subtracting nothing.
pub const MODEL_FORCE_OMISSIONS: [&str; 3] = [
    " Add `model` to a phase entry when that phase uses a specific model override.",
    " model?: string,",
    " opts.model overrides the model for this agent call. Default to omitting it — the agent \
     inherits the main-loop model (the resolved session model), which is almost always correct. \
     Only set it when you're highly confident a different tier fits the task; when unsure, omit.",
];

/// The shipped authoring reference, in its unforced rendering.
pub static AUTHORING_SKILL: Lazy<String> = Lazy::new(|| {
    compose_description(
        &ORACLE_AUTHORING_SKILL,
        &DESCRIPTION_DIVERGENCES,
        DivergenceTarget::Skill,
    )
    .unwrap_or_else(|error| panic!("Workflow authoring reference: {error}"))
});

/// The pointer sentence (claude-code v2.1.267 `i`) that replaces the reference
/// in the tool description when the skill can actually be loaded.
pub const AUTHORING_SKILL_POINTER: &str = "Before writing a script, load the `workflow-authoring` \
     skill — the workflow authoring reference: script API and gotchas, resume, the **Ultracode** \
     section, quality patterns, worked examples.";

/// The authoring reference as the `workflow-authoring` skill serves it.
///
/// `model_forced` mirrors upstream's `e`: when a deployment pins the subagent
/// model, the three `model`-override passages are subtracted.
pub fn authoring_skill_body(model_forced: bool) -> String {
    let mut body = AUTHORING_SKILL.clone();
    if model_forced {
        for fragment in MODEL_FORCE_OMISSIONS {
            body = body.replace(fragment, "");
        }
    }
    body
}

/// `a.CLAUDE_CODE_SUBAGENT_MODEL_FORCE` — the deployment pin that makes
/// per-agent `model` overrides meaningless, so the passages documenting them are
/// dropped. Read here at the edge and passed down as a parameter: a gate that
/// reads the environment deep inside the call graph cannot be tested without
/// `set_var`, which flakes the moment the suite runs in parallel.
#[must_use]
pub fn subagent_model_forced() -> bool {
    platform_api::env::is_env_truthy(
        std::env::var("LINGXI_SUBAGENT_MODEL_FORCE")
            .or_else(|_| std::env::var("CLAUDE_CODE_SUBAGENT_MODEL_FORCE"))
            .ok()
            .as_deref(),
    )
}

/// Upstream `eFt()` — the body the `workflow-authoring` skill serves.
///
/// The environment read sits here, at the skill boundary, exactly where
/// upstream's `wpn()` reads it. [`authoring_skill_body`] stays parameterised so
/// the tests can pin both renderings without `set_var`.
#[must_use]
pub fn authoring_skill_prompt() -> String {
    authoring_skill_body(subagent_model_forced())
}

/// Upstream `Epn(e)` — assemble what the model reads for the Workflow tool.
///
/// `skill_reachable` answers "can this request load the `workflow-authoring`
/// skill?". When it can, the description carries a one-line pointer; when it
/// cannot, the whole reference is inlined exactly as 2.1.245 shipped it.
///
/// ⛔ The fallback is not an optimisation detail — it is a reachability
/// invariant. A script only ever learns which hooks exist from this text, so
/// pointing at a skill the model cannot load would make every hook (including
/// LingXi's `fusion()`) unreachable. Upstream inlines for the same reason.
pub fn assemble_description(skill_reachable: bool, model_forced: bool) -> String {
    if skill_reachable {
        format!("{}\n\n{}", *DESCRIPTION, AUTHORING_SKILL_POINTER)
    } else {
        format!("{}\n\n{}", *DESCRIPTION, authoring_skill_body(model_forced))
    }
}
