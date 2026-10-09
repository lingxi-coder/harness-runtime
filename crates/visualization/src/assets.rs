//! Every byte a visualization host serves, embedded at compile time so the
//! runtime and its assets can never drift apart across Electron, iOS and
//! Android.

/// Upstream control and theme styles (byte-identical vendor copy).
pub const VISUALIZE_CSS: &str = include_str!("../vendor/codex/visualize.css");
/// LingXi content runtime, inlined into each document ahead of the fragment.
pub const CONTENT_RUNTIME_JS: &str = include_str!("../assets/content-runtime.js");

/// File name of the bundled D3 build under `/asset/`.
pub const D3: &str = "d3.min.js";

/// One servable file.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Asset {
    /// Path under the host origin, e.g. `/asset/d3.min.js`.
    pub path: &'static str,
    /// Response `Content-Type`.
    pub content_type: &'static str,
    /// Body.
    pub bytes: &'static [u8],
}

const JS: &str = "text/javascript; charset=utf-8";

/// The shell page, its script and stylesheet, and the library assets.
pub const ASSETS: &[Asset] = &[
    Asset {
        path: "/shell.html",
        content_type: "text/html; charset=utf-8",
        bytes: include_bytes!("../assets/shell.html"),
    },
    Asset {
        path: "/shell.js",
        content_type: JS,
        bytes: include_bytes!("../assets/shell.js"),
    },
    Asset {
        path: "/shell.css",
        content_type: "text/css; charset=utf-8",
        bytes: include_bytes!("../assets/shell.css"),
    },
    Asset {
        path: "/asset/floating-ui.core.umd.min.js",
        content_type: JS,
        bytes: include_bytes!("../vendor/floating-ui-core/floating-ui.core.umd.min.js"),
    },
    Asset {
        path: "/asset/floating-ui.dom.umd.min.js",
        content_type: JS,
        bytes: include_bytes!("../vendor/floating-ui-dom/floating-ui.dom.umd.min.js"),
    },
    Asset {
        path: "/asset/lucide.min.js",
        content_type: JS,
        bytes: include_bytes!("../vendor/lucide/lucide.min.js"),
    },
    Asset {
        path: "/asset/d3.min.js",
        content_type: JS,
        bytes: include_bytes!("../vendor/d3/d3.min.js"),
    },
];

/// Look up a servable asset by its exact path.
#[must_use]
pub fn find(path: &str) -> Option<&'static Asset> {
    ASSETS.iter().find(|asset| asset.path == path)
}

/// Third-party notices every host must ship with the runtime.
#[must_use]
pub fn third_party_notices() -> String {
    let modified = format!(
        "OpenAI Codex inline visualization assets (Apache-2.0); visualize.html modified by {}",
        branding::PRODUCT_NAME
    );
    let sections: [(&str, &str); 6] = [
        (modified.as_str(), include_str!("../vendor/codex/LICENSE")),
        (
            "OpenAI Codex NOTICE",
            include_str!("../vendor/codex/NOTICE"),
        ),
        (
            "@floating-ui/core 1.7.3 (MIT)",
            include_str!("../vendor/floating-ui-core/LICENSE"),
        ),
        (
            "@floating-ui/dom 1.7.4 (MIT)",
            include_str!("../vendor/floating-ui-dom/LICENSE"),
        ),
        (
            "lucide 1.17.0 (ISC; Feather-derived icons MIT)",
            include_str!("../vendor/lucide/LICENSE"),
        ),
        ("d3 7.9.0 (ISC)", include_str!("../vendor/d3/LICENSE")),
    ];
    let mut notices = String::new();
    for (title, text) in sections {
        notices.push_str("==== ");
        notices.push_str(title);
        notices.push_str(" ====\n\n");
        notices.push_str(text.trim_end());
        notices.push_str("\n\n");
    }
    notices
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn every_asset_resolves_and_paths_are_unique() {
        for asset in ASSETS {
            assert_eq!(find(asset.path), Some(asset));
            assert!(!asset.bytes.is_empty(), "{}", asset.path);
        }
        let mut paths: Vec<_> = ASSETS.iter().map(|asset| asset.path).collect();
        paths.sort_unstable();
        paths.dedup();
        assert_eq!(paths.len(), ASSETS.len());
        assert!(find("/asset/../shell.js").is_none());
        assert!(find(&format!("/asset/{D3}")).is_some());
    }

    #[test]
    fn notices_cover_every_vendored_license() {
        let notices = third_party_notices();
        for needle in [
            "Apache License",
            "Copyright 2025 OpenAI",
            "Floating UI contributors",
            "Lucide Icons",
            "Cole Bemis",
            "Mike Bostock",
        ] {
            assert!(notices.contains(needle), "{needle}");
        }
    }

    #[test]
    fn shell_has_no_inline_script_or_external_url() {
        let shell = std::str::from_utf8(find("/shell.html").unwrap().bytes).unwrap();
        assert!(shell.contains("<script src=\"shell.js\" defer></script>"));
        assert_eq!(shell.matches("<script").count(), 1);
        assert!(shell.contains("sandbox=\"allow-scripts\""));
        assert!(!shell.contains("allow-same-origin"));
        for asset in ["/shell.html", "/shell.js", "/shell.css"] {
            let text = std::str::from_utf8(find(asset).unwrap().bytes).unwrap();
            assert!(
                !text.contains("http://") && !text.contains("https://"),
                "{asset}"
            );
        }
        assert!(
            !CONTENT_RUNTIME_JS.contains("http://") && !CONTENT_RUNTIME_JS.contains("https://")
        );
    }
}
