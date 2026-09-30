
/// Re-encode immutable 5.3.0 bundled data without changing grammar contents.
pub fn export_cbor_assets(output: &std::path::Path) {
    use std::io::Write;
    use flate2::{write::ZlibEncoder, Compression};
    // Record a behavioral oracle from the unchanged upstream engine first.
    let original = SyntaxSet::load_defaults_newlines();
    let themes = crate::highlighting::ThemeSet::load_defaults();
    let lines = [
        "// a comment with punctuation [] {} ()\n",
        "fn main() { let value = 42; println!(\"hello\"); }\n",
        "def function(value): return value + 3.14\n",
        "class Example { const message = 'text'; }\n",
        "<div class=\"example\">content &amp; text</div>\n",
        "SELECT * FROM records WHERE id = 7;\n",
        "key: [true, false, null, 12]\n",
        "# heading with `inline code` and **bold**\n",
    ];
    let mut cases = Vec::new();
    for syntax in original.syntaxes() {
        let mut highlighter = crate::easy::HighlightLines::new(syntax, &themes.themes["base16-ocean.dark"]);
        let spans: Vec<_> = lines.iter().map(|line| {
            highlighter.highlight_line(line, &original).unwrap().into_iter()
                .map(|(style, text)| (style, text.to_owned())).collect::<Vec<_>>()
        }).collect();
        cases.push(serde_json::json!({"name": syntax.name, "spans": spans}));
    }
    let oracle = serde_json::json!({"lines": lines, "cases": cases, "themes": themes});
    serde_json::to_writer(std::fs::File::create(output.join("highlight-oracle.json")).unwrap(), &oracle).unwrap();
    for (name, bytes) in [
        ("default_newlines.packdump", &include_bytes!("../../assets/default_newlines.packdump")[..]),
        ("default_nonewlines.packdump", &include_bytes!("../../assets/default_nonewlines.packdump")[..]),
    ] {
        let mut syntax_set: SyntaxSet = crate::dumps::from_uncompressed_data(bytes).unwrap();
        for syntax in &mut syntax_set.syntaxes {
            let contexts = LazyContexts::deserialize(&syntax.serialized_lazy_contexts);
            let mut output = ZlibEncoder::new(Vec::new(), Compression::best());
            ciborium::ser::into_writer(&contexts, &mut output).unwrap();
            syntax.serialized_lazy_contexts = output.finish().unwrap();
        }
        ciborium::ser::into_writer(&syntax_set, std::fs::File::create(output.join(name)).unwrap()).unwrap();
    }
    let metadata: crate::parsing::Metadata = crate::dumps::from_binary(include_bytes!("../../assets/default_metadata.packdump"));
    let mut file = ZlibEncoder::new(std::fs::File::create(output.join("default_metadata.packdump")).unwrap(), Compression::best());
    ciborium::ser::into_writer(&metadata, &mut file).unwrap();
    file.finish().unwrap();
    let themes = crate::highlighting::ThemeSet::load_defaults();
    let mut file = ZlibEncoder::new(std::fs::File::create(output.join("default.themedump")).unwrap(), Compression::best());
    ciborium::ser::into_writer(&themes, &mut file).unwrap();
    file.flush().unwrap();
    file.finish().unwrap();
}
