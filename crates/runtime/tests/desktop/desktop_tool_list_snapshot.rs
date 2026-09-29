//! M8-P6 §14 criterion 8 — lock the desktop tool set assembled by the
//! composition root.
//!
//! This is the regression net guaranteeing `harness_runtime::desktop::desktop_tool_registry`
//! registers exactly the intended tool surface. If a tool crate is added,
//! removed, or its `TOOL_NAME` drifts, this snapshot fails and forces a
//! conscious update. The CLI delegates registry assembly to the same function,
//! so this also pins what the desktop binary ships.

#![allow(clippy::unwrap_used)]

use harness_runtime::desktop::desktop_tool_registry;
use lingxi_core::host::process::ProcessOutput;

#[test]
fn desktop_tool_list_snapshot() {
    // A fully-stubbed context is enough — we only enumerate registered names,
    // never invoke a tool.
    let ctx = tool_api::test_support::shell_test_ctx(ProcessOutput {
        stdout: String::new(),
        stderr: String::new(),
        exit_code: 0,
        timed_out: false,
    });
    // `None` → default (non-coordinator) session: byte-identical to pre-M10.
    // Third `None` → no RemoteTrigger auth provider for the offline snapshot.
    let reg = desktop_tool_registry(ctx, None, None);

    let mut names = reg.all_names();
    names.sort();

    insta::assert_yaml_snapshot!("desktop_tool_list", names);
}
