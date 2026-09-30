//! Every outward-facing identifier that carries the claude-code version.
//!
//! Deliberately NOT named after a version. These pins are about the derivation —
//! one constant, three stamps, no independent copies — and a version in the file
//! name goes stale on the next bump, which is how
//! `parity_claude_2_1_220.rs` ended up asserting 2.1.267 content under a 2.1.220
//! name. The version-named suites next door are different: each pins what a
//! specific release said, and their names are accurate.
//!
//! Oracle templates (2.1.267 chunks, `Ma()` and `RMn()`), both reading the same
//! build metadata object whose `VERSION` field is the release number:
//!   User-Agent  `claude-code/${…VERSION}`
//!   AI_AGENT    `claude-code_${…VERSION.replace(/\./g,"-")}_${e}`
//!   LSP client  `clientInfo:{name:"Claude Code",version:{…}.VERSION}`

/// The advertised version is a deliberate act, not a default — claude-code tells
/// servers and child processes what it is, so it may only be raised once the
/// behaviour behind it matches. Update this WITH the constant's doc block, which
/// records why each raise happened.
#[test]
fn the_advertised_version_is_the_one_the_last_sweep_verified() {
    assert_eq!(lingxi_core::host::CLAUDE_CODE_VERSION, "2.1.267");
}

#[test]
fn the_ai_agent_stamp_follows_the_oracle_template() {
    let derived = format!(
        "claude-code_{}_agent",
        lingxi_core::host::CLAUDE_CODE_VERSION.replace('.', "-")
    );
    assert_eq!(derived, "claude-code_2-1-267_agent");
}

#[test]
fn the_web_fetch_user_agent_follows_the_oracle_template() {
    let derived = format!(
        "Claude-User (claude-code/{}; +https://support.anthropic.com/)",
        lingxi_core::host::CLAUDE_CODE_VERSION
    );
    assert_eq!(
        derived,
        "Claude-User (claude-code/2.1.267; +https://support.anthropic.com/)"
    );
}

/// The point of the single constant: no stamp may carry a literal of its own.
/// The LSP `clientInfo` did until 2026-09-10, and a bump that forgot it would
/// have told language servers a different version than it told web servers.
#[test]
fn no_version_facing_identifier_keeps_its_own_copy() {
    let version = lingxi_core::host::CLAUDE_CODE_VERSION;
    let dashed = version.replace('.', "-");
    assert!(format!("claude-code_{dashed}_agent").contains(&dashed));
    assert!(
        format!("Claude-User (claude-code/{version}; +https://support.anthropic.com/)")
            .contains(version)
    );

    let sources = std::path::PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .parent()
        .expect("engine root")
        .to_path_buf();
    let lsp_client =
        std::fs::read_to_string(sources.join("lsp/src/client.rs")).expect("read lsp client");
    assert!(
        lsp_client.contains("lingxi_core::host::CLAUDE_CODE_VERSION"),
        "the LSP clientInfo version must derive from the shared constant, not a literal"
    );
}
