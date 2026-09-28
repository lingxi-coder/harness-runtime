/// Oracle `kq` is read by leaf file tools through a process-global probe, so
/// a root that never publishes one silently loses the whole stale-recovery
/// path — `read_auto_allowed` just answers `false` forever. The composition
/// needs a live policy and a built registry, neither unit-constructible
/// here, so pin it against this file's own source. The needle is assembled
/// at runtime so it cannot match the comment that explains it.
///
/// 🚨 Comment lines are stripped first. A publication that is commented out
/// has stopped publishing while every character of it is still in the file,
/// so a bare `contains` over `include_str!` passes on exactly the change it
/// exists to catch — measured, not assumed: commenting the block out left
/// this test green until the strip was added.
#[test]
fn this_root_publishes_the_read_auto_allow_probe() {
    const SRC: &str = include_str!("../mod.rs");
    let code = code_only(SRC);
    let publish = "set_read_auto_allow_prob".to_string() + "e(";
    assert!(
        code.contains(&publish),
        "this root must publish the kq probe, or Edit can never recover a \
         stale-but-clean edit"
    );
    let inputs = "PolicyReadAutoAllow::ne".to_string() + "w(policy, tools.all_names())";
    assert!(
        code.contains(&inputs),
        "the probe must be built from the boot policy AND the final tool \
         list — an unknown tool list answers false for everything"
    );
}

/// The strip itself, pinned: without this the test above cannot tell a live
/// publication from a commented-out one.
#[test]
fn the_strip_removes_comment_lines_and_keeps_code() {
    let stripped = code_only("    // set_x(1);\n    set_x(2);\n/// set_x(3);\n");
    assert!(
        !stripped.contains("set_x(1)"),
        "a `//` line must be stripped"
    );
    assert!(
        !stripped.contains("set_x(3)"),
        "a `///` line must be stripped"
    );
    assert!(stripped.contains("set_x(2)"), "real code must survive");
}

fn code_only(src: &str) -> String {
    src.lines()
        .filter(|line| !line.trim_start().starts_with("//"))
        .collect::<Vec<_>>()
        .join("\n")
}
