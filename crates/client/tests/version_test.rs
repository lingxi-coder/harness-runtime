//! F1-01 — version constant tests.
//!
//! `CLIENT_PROTOCOL_VERSION` is the distinct, client-facing protocol version
//! (governing decision §0.10), separate from `bridge::BRIDGE_PROTOCOL_VERSION`.
//! It must be a valid 3-component semver string so the F2 handshake and the
//! F1-09 structural version-diff guard can reason about major bumps.

use client::protocol::version::CLIENT_PROTOCOL_VERSION;

/// `CLIENT_PROTOCOL_VERSION` parses as a 3-component `major.minor.patch` semver.
///
/// Self-contained parse (no `semver` crate dep, no `serde_json` — the contract
/// crate stays minimal per §0): split on `.` and assert exactly three numeric
/// components.
#[test]
fn version_is_semver() {
    let parts: Vec<&str> = CLIENT_PROTOCOL_VERSION.split('.').collect();
    assert_eq!(
        parts.len(),
        3,
        "CLIENT_PROTOCOL_VERSION must be major.minor.patch, got {CLIENT_PROTOCOL_VERSION:?}"
    );
    for (label, part) in [
        ("major", parts[0]),
        ("minor", parts[1]),
        ("patch", parts[2]),
    ] {
        assert!(
            !part.is_empty() && part.chars().all(|c| c.is_ascii_digit()),
            "{label} component must be numeric, got {part:?}"
        );
    }
}

/// Major 20 removes the Local App families from the client protocol; major 18
/// added the current Mod UI request/result and targeted invalidation
/// protocol. This is a deliberate second lock beyond the structural guard
/// because generated native bindings change and the host requires protocol
/// version lockstep.
///
/// This test is the deliberate second lock on the version: the structural guard
/// only asks that SOME bump happened, so without this a later edit could ride
/// along on this bump without anyone choosing it.
#[test]
fn version_is_twenty_zero_zero() {
    assert_eq!(CLIENT_PROTOCOL_VERSION, "20.0.0");
}
