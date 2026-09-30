//! Preserve every upstream syntax's highlighting and every theme across the cache migration.
#![cfg(feature = "presentation")]

use serde_json::Value;
use syntect::{easy::HighlightLines, highlighting::ThemeSet, parsing::SyntaxSet};

#[test]
fn cbor_cache_preserves_upstream_highlighting_and_theme_data() {
    let oracle: Value = serde_json::from_slice(include_bytes!(
        "../../../third_party/syntect/assets/highlight-oracle.json"
    ))
    .unwrap();
    let syntaxes = SyntaxSet::load_defaults_newlines();
    let themes = ThemeSet::load_defaults();
    assert_eq!(serde_json::to_value(&themes).unwrap(), oracle["themes"]);
    let cases = oracle["cases"].as_array().unwrap();
    assert_eq!(syntaxes.syntaxes().len(), cases.len());
    for case in cases {
        let name = case["name"].as_str().unwrap();
        let syntax = syntaxes.find_syntax_by_name(name).unwrap();
        let mut highlighter = HighlightLines::new(syntax, &themes.themes["base16-ocean.dark"]);
        let spans: Vec<_> = oracle["lines"]
            .as_array()
            .unwrap()
            .iter()
            .map(|line| {
                highlighter
                    .highlight_line(line.as_str().unwrap(), &syntaxes)
                    .unwrap()
                    .into_iter()
                    .map(|(style, text)| (style, text.to_owned()))
                    .collect::<Vec<_>>()
            })
            .collect();
        assert_eq!(
            serde_json::to_value(spans).unwrap(),
            case["spans"],
            "syntax {name}"
        );
    }
}
