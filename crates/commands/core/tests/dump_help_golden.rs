//! Helper test: dumps the byte-locked `/help` golden fixture to the
//! test-harness fixtures directory. Run with `--ignored` to refresh.

#[test]
#[ignore = "writes the golden fixture; run manually once"]
fn dump_golden_help_screen() {
    let s = command_api::builtin_support::help_render::render_help_screen();
    let path = std::path::PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .join("../../test-harness/src/parity/fixtures/parity_help_screen.txt");
    std::fs::write(&path, s).expect("write golden");
}
