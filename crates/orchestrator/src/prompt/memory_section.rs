//! Claude Code 2.1.319 file-memory protocols, evaluated from the pinned artifact.
//! LingXi retains its configured directories and instruction-file name.
#![forbid(unsafe_code)]

/// Fixed memory index filename.
pub const MEMORY_INDEX_NAME: &str = "MEMORY.md";
const AUTO: &str = include_str!("templates/claude-code-2.1.319/memory-auto.md");
const STATIC: &str = include_str!("templates/claude-code-2.1.319/memory-auto-static.md");
const LEAN: &str = include_str!("templates/claude-code-2.1.319/memory-lean.md");
const REWRITE: &str = include_str!("templates/claude-code-2.1.319/memory-lean-rewrite.md");

/// Native single-directory lean memory protocol.
#[must_use]
pub fn render(path: &str) -> String {
    render_with_hygiene(path, false)
}

/// Rewritten memory hygiene is independent of the lean layout selection.
#[must_use]
pub fn render_with_hygiene(path: &str, rewrite: bool) -> String {
    (if rewrite { REWRITE } else { LEAN }).replacen("{memory_path}", path, 1)
}

/// Static instructions used when the host splits stable and dynamic context.
#[must_use]
pub fn render_auto_static() -> String {
    STATIC.to_owned()
}

/// Dynamic directory announcement paired with the static instructions.
#[must_use]
pub fn render_auto_directory(path: &str) -> String {
    format!("# auto memory\nMemory directory: `{path}`")
}

/// Complete standard memory protocol.
#[must_use]
pub fn render_auto(path: &str) -> String {
    AUTO.replacen("{memory_path}", path, 1)
}

/// Recognize the source-locked protocol in a resumed snapshot without
/// recapturing other sections when its store still matches current settings.
pub(crate) fn snapshot_matches_directory(
    snapshot: &lingxi_core::host::PromptSnapshot,
    directory: Option<&std::path::Path>,
    exclude_dynamic_sections: bool,
) -> bool {
    let has = |text: &str| {
        snapshot
            .system_prompt
            .iter()
            .any(|section| section.contains(text))
    };
    let Some(directory) = directory else {
        return ![AUTO, LEAN, REWRITE, STATIC].iter().any(|template| {
            has(template
                .split("{memory_path}")
                .next()
                .expect("memory template prefix"))
        });
    };
    let path = memory::memdir::paths::memory_directory_prompt_path(directory);
    has(&render(&path))
        || has(&render_with_hygiene(&path, true))
        || if exclude_dynamic_sections {
            has(STATIC)
        } else {
            has(&render_auto(&path))
        }
}

/// Select the protocol for the active provider's resolved profile.
#[must_use]
pub fn render_for_profile(
    path: &str,
    profile: lingxi_core::host::model_capabilities::PromptProfile,
) -> String {
    render_for_prompt(path, profile, false)
}

/// Native Tji/Aji split only applies to the standard local-file protocol.
#[must_use]
pub fn render_for_prompt(
    path: &str,
    profile: lingxi_core::host::model_capabilities::PromptProfile,
    exclude_dynamic_sections: bool,
) -> String {
    match profile {
        lingxi_core::host::model_capabilities::PromptProfile::ClaudeLean => render(path),
        _ if exclude_dynamic_sections => render_auto_static(),
        _ => render_auto(path),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn excluded_standard_memory_keeps_the_system_protocol_identical_across_directories() {
        use lingxi_core::host::model_capabilities::PromptProfile;
        for profile in [PromptProfile::ClaudeStandard, PromptProfile::FullHarness] {
            let a = render_for_prompt("/first/memory/", profile, true);
            let b = render_for_prompt("/second/memory/", profile, true);
            assert_eq!(a, render_auto_static());
            assert_eq!(a, b);
            assert_ne!(render_for_prompt("/first/memory/", profile, false), a);
        }
        assert_eq!(
            render_for_prompt("/first/memory/", PromptProfile::ClaudeLean, true),
            render("/first/memory/")
        );
    }
    #[test]
    fn every_supported_memory_renderer_matches_the_shipped_artifact() {
        let fixture: serde_json::Value = serde_json::from_str(include_str!(
            "../../../../scripts/tests/fixtures/memory_prompts_319.json"
        ))
        .unwrap();
        for case in fixture["cases"].as_array().unwrap() {
            let path = case["path"].as_str().unwrap();
            let actual = match case["kind"].as_str().unwrap() {
                "lean" => render_with_hygiene(path, case["rewrite"].as_bool().unwrap()),
                "auto" => render_auto(path),
                _ => unreachable!(),
            };
            assert_eq!(actual, case["expected"].as_str().unwrap(), "{case}");
        }
        assert_eq!(
            render_auto_static(),
            fixture["templates"]["auto_static"].as_str().unwrap()
        );
    }
}
