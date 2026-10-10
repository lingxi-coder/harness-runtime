//! Derive the content runtime from the byte-identical upstream
//! `vendor/codex/visualize.html`.
//!
//! The upstream file loads Floating UI and Lucide from unpkg. LingXi serves
//! the same releases from its own asset origin, so the three script URLs are
//! rewritten to `{{ASSET_BASE}}` paths that the document wrapper fills per
//! host. Each URL must occur exactly once: a moved or re-pinned upstream URL
//! fails the build instead of silently leaving a CDN load in the output.

use std::path::PathBuf;

const SOURCE: &str = "vendor/codex/visualize.html";

const REWRITES: [(&str, &str); 3] = [
    (
        "https://unpkg.com/@floating-ui/core@1.7.3/dist/floating-ui.core.umd.min.js",
        "{{ASSET_BASE}}floating-ui.core.umd.min.js",
    ),
    (
        "https://unpkg.com/@floating-ui/dom@1.7.4/dist/floating-ui.dom.umd.min.js",
        "{{ASSET_BASE}}floating-ui.dom.umd.min.js",
    ),
    (
        "https://unpkg.com/lucide@1.17.0/dist/umd/lucide.js",
        "{{ASSET_BASE}}lucide.min.js",
    ),
];

// Apache-2.0 section 4(b): a modified file carries a prominent notice.
const MODIFICATION_NOTICE: &str = "<!-- Modified by LingXi from openai/codex \
codex-rs/tui/assets/inline_visualization/visualize.html at \
ea27864f99f0b086cec2f9f0251b7190fb9844f1 (Apache-2.0): CDN script URLs \
replaced with local asset paths. -->\n";

fn main() {
    println!("cargo:rerun-if-changed={SOURCE}");
    println!("cargo:rerun-if-changed=build.rs");
    let source = std::fs::read_to_string(SOURCE).expect("read vendored visualize.html");
    let mut runtime = source;
    for (from, to) in REWRITES {
        let count = runtime.matches(from).count();
        assert!(
            count == 1,
            "{SOURCE}: expected exactly one `{from}`, found {count}; re-vendor and update build.rs"
        );
        runtime = runtime.replacen(from, to, 1);
    }
    assert!(
        !runtime.contains("https://") && !runtime.contains("http://"),
        "{SOURCE}: an external URL survived the rewrite"
    );
    let out =
        PathBuf::from(std::env::var_os("OUT_DIR").expect("OUT_DIR")).join("visualize.runtime.html");
    std::fs::write(out, format!("{MODIFICATION_NOTICE}{runtime}")).expect("write derived runtime");
}
