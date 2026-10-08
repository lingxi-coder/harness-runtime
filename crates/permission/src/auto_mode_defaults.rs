//! WIZARD-06 — the shipped auto-mode default slots (2.1.220).
//!
//! `bmt()` reads these four `<user_*_to_replace>` slots out of the auto-mode
//! classifier prompt template. Two ported call sites consume them:
//!
//! * [`crate::auto_mode_propose::build_propose_prompt`] appends
//!   [`DEFAULT_ENVIRONMENT`] verbatim as the prompt's shipped-defaults trailer.
//! * The recon's "Shipped default auto-mode rule labels" section lists the
//!   `allow` and `soft_deny` LABELS, so a proposal does not duplicate coverage
//!   the defaults already have.
//!
//! The rule slots are stored as the labels `fPo` reduces them to
//! ([`default_rule_label`]) rather than as their full prose: the full text is
//! only ever read through that reduction here, since this workspace ships a
//! deterministic offline classifier instead of the prompt these slots belong
//! to. Both call sites therefore produce byte-identical output.

/// The 20 shipped `environment` entries, verbatim.
pub const DEFAULT_ENVIRONMENT: [&str; 20] = [
    r"**Organization**: None configured",
    r"**Primary use**: software development",
    r"**Cloud provider(s)**: None configured",
    r"**Repository visibility**: assume private unless the remote host and repo name indicate otherwise, or a visibility check in the transcript shows public",
    r"**Internal sharing / snippet hosting**: None configured — treat public paste/gist services as outside the trust boundary",
    r"**Org-specific CLIs**: None configured",
    r"**Secrets management**: None configured",
    r"**CI/CD deploy targets**: None configured",
    r"**Network posture**: None configured",
    r"**Protected deployment namespaces / environments**: None configured — fall back to the Sensitive remote targets heuristic",
    r"**Data retention / declassification**: None configured",
    r"**Trusted repo**: The git repository the agent started in (its working directory) and its configured remote(s). When the repo's public/private visibility is given — by the Repository visibility entry or the user's own message — use it to scope what is OK to commit or push there: confidential material is fine in a private repo; in a public one, only that repo's own work is — and content ported, repointed, or first read from outside this session's repo is not its own work, whoever directed the port. Visibility scopes confidential material only: secrets and sensitive data (personal & entrusted) are never cleared into any repo by its visibility (see Definitions).",
    r"**Source control**: The trusted repo and its remote(s) only (no additional orgs configured) — as scoped by the Trusted repo and Repository visibility entries above",
    r"**Trusted internal domains**: None configured",
    r"**Trusted cloud buckets**: None configured",
    r"**Key internal services**: None configured",
    r"**Internal package registry**: None configured",
    r"**Sensitive data locations & audiences**: any file or store holding personal data, confidential business data, credentials, regulated data, or similarly sensitive material; preserve exact handles when known and share only with audiences cleared at the [named+specifics] bar",
    r"**Sensitive remote targets**: any namespace, host, or container whose name carries `prod` or `production` as a whole word or name segment (hyphen/underscore/dot-delimited — e.g. matches `prod-db`, not `producer`)",
    r"**Protected IaC scopes**: IAM, RBAC, networking, quota, and node-pool resources; anything whose name or tag carries `prod` or `production` as a whole word or name segment",
];

/// Labels of the 17 shipped `allow` rules.
pub const DEFAULT_ALLOW_LABELS: [&str; 17] = [
    r"Security Discussion",
    r"Transient Retry",
    r"Test Artifacts",
    r"Local Operations",
    r"Read-Only Operations",
    r"Declared Dependencies",
    r"Toolchain Bootstrap",
    r"Standard Credentials",
    r"Session-Created Job Cleanup",
    r"Git Push Destination",
    r"Trusted Internal Infra (Data Flow)",
    r"Scheduled-Task Fires",
    r"Multi-Agent Coordination",
    r"Memory Directory",
    r"Instruction File Content",
    r"Agent Scheduling",
    r"Browser Trusted Navigation",
];

/// Labels of the 68 shipped `soft_deny` rules.
pub const DEFAULT_SOFT_DENY_LABELS: [&str; 68] = [
    r"Git Destructive",
    r"Code That Leaks When Run",
    r"Code from External",
    r"Cloud Storage Mass Delete",
    r"Production Deploy",
    r"Remote Shell Writes",
    r"Sensitive Remote Exec",
    r"Production Reads",
    r"Blind Apply",
    r"Protected-Scope IaC Apply",
    r"Logging/Audit Tampering",
    r"Permission Grant",
    r"TLS/Auth Weaken",
    r"Secret-Store Writes",
    r"DNS / Domain / Cert Changes",
    r"Security Weaken",
    r"Security Test Removal",
    r"Safety Bypass Flag",
    r"Create Unsafe Agents",
    r"Interfere With Workloads",
    r"Shared Cluster Mutation",
    r"CI Bypass",
    r"Modify Shared Resources",
    r"Irreversible Local Destruction",
    r"Unverifiable Deletion Target",
    r"Shared Scratch Sweep",
    r"Irreversible Deletion (general)",
    r"Unverifiable Deletion Scope",
    r"Create RCE Surface",
    r"Expose Local Services",
    r"External Ingress Tunnel",
    r"Credential Leakage",
    r"Credential Materialization",
    r"Credential Exploration",
    r"PII Data Handling",
    r"Exfil Scouting",
    r"Traffic Redirection",
    r"Remote Repoint",
    r"Out-of-Place Publication",
    r"Sensitive-Source Provenance",
    r"Excess Sensitive Detail",
    r"Unrequested Artifact Publish",
    r"Live-Shared Artifact Sensitive Delta",
    r"Sandbox Network Callback",
    r"Create Public Surface",
    r"Public Data-Sharing Upload",
    r"Untrusted Code Integration",
    r"Package Registry Bypass",
    r"Unauthorized Persistence",
    r"Self-Modification",
    r"Tmux Self Drive",
    r"Instruction Poisoning",
    r"Auto-Mode Bypass",
    r"Session Transcript Tampering",
    r"External System Writes",
    r"Merge Without Review",
    r"Self-Approval",
    r"ChatOps Trigger Comments",
    r"Feature Flag Writes",
    r"Node Lifecycle Operations",
    r"Cluster-Wide Workload Creation",
    r"Real-World Transactions",
    r"Third-Party Attack",
    r"Browser Navigate Exfil",
    r"Browser Input Exfil",
    r"Browser JS Exfil",
    r"Browser File Upload Exfil",
    r"Browser Shortcut Execution",
];

/// Labels of the shipped `hard_deny` rules.
pub const DEFAULT_HARD_DENY_LABELS: [&str; 1] = [r"Data Exfiltration"];

/// `fPo(e)` — a rule's label: everything before the first `:` or `[`, trimmed.
#[must_use]
pub fn default_rule_label(rule: &str) -> &str {
    let cut = rule.find([':', '[']).unwrap_or(rule.len());
    rule[..cut].trim_end()
}

#[cfg(test)]
mod tests {
    use super::{default_rule_label, DEFAULT_SOFT_DENY_LABELS};

    #[test]
    fn soft_deny_labels_include_2_1_251_artifact_publish_in_oracle_order() {
        assert_eq!(DEFAULT_SOFT_DENY_LABELS.len(), 68);
        let excess = DEFAULT_SOFT_DENY_LABELS
            .iter()
            .position(|label| *label == "Excess Sensitive Detail")
            .expect("Excess Sensitive Detail label");
        assert_eq!(
            DEFAULT_SOFT_DENY_LABELS[excess + 1],
            "Unrequested Artifact Publish"
        );
        assert_eq!(
            DEFAULT_SOFT_DENY_LABELS[excess + 2],
            "Live-Shared Artifact Sensitive Delta"
        );
        assert_eq!(
            default_rule_label(
                "Unrequested Artifact Publish [named+specifics — **must name:** the artifact going public]: details"
            ),
            "Unrequested Artifact Publish"
        );
    }
}
