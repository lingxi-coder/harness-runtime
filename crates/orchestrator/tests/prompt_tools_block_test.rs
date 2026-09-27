use orchestrator::prompt::tools_block;

#[test]
fn empty_input_returns_empty_string() {
    let out = tools_block::format(&[]);
    assert_eq!(out, "");
}

#[test]
fn single_tool_shape() {
    let out = tools_block::format(&["Read".to_string()]);
    assert_eq!(out, "<tools>\n- Read\n</tools>\n");
}

#[test]
fn multiple_tools_emitted_alphabetic_regardless_of_input_order() {
    let out = tools_block::format(&["Write".to_string(), "Bash".to_string(), "Read".to_string()]);
    assert_eq!(out, "<tools>\n- Bash\n- Read\n- Write\n</tools>\n");
}

#[test]
fn names_with_dots_and_uppercase_sort_byte_lex() {
    let out = tools_block::format(&[
        "mcp__server__tool".to_string(),
        "Read".to_string(),
        "BashOutput".to_string(),
    ]);
    // Byte-lex: uppercase < underscore-prefix-lowercase ; "Bash" < "Read" < "mcp__"
    // because 'B'(66) < 'R'(82) < 'm'(109).
    assert_eq!(
        out,
        "<tools>\n- BashOutput\n- Read\n- mcp__server__tool\n</tools>\n"
    );
}
