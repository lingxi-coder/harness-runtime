//! Exact Acorn syntax parsing in the workflow crate's existing QuickJS runtime.
//!
//! This module loads the pinned upstream Acorn UMD build, then passes workflow
//! source as a JSON-escaped string value to `acorn.parse`. It never evaluates
//! the source being parsed. The metadata AST walk and qS-specific diagnostic
//! formatting remain at their current integration boundary.

use rquickjs::{Context, Runtime};
use serde::Deserialize;

const ACORN_SOURCE: &str = include_str!("../assets/acorn-8.15.0/acorn.js");
const ACORN_VERSION: &str = "8.15.0";

/// Acorn's syntax error location, with JavaScript's UTF-16 column and offset.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct AcornSyntaxError {
    pub(crate) message: String,
    pub(crate) line: Option<usize>,
    pub(crate) column_utf16: Option<usize>,
    pub(crate) position_utf16: Option<usize>,
}

/// Failure while setting up the embedded parser, or invalid workflow syntax.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum AcornParserError {
    /// A QuickJS, pinned-version, or reply-decoding failure.
    Runtime(String),
    /// Acorn rejected the source. The location is expressed in UTF-16 units.
    Syntax(AcornSyntaxError),
}

impl std::fmt::Display for AcornParserError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Runtime(message) => write!(f, "Acorn runtime: {message}"),
            Self::Syntax(error) => std::fmt::Display::fmt(error, f),
        }
    }
}

impl std::error::Error for AcornParserError {}

impl std::fmt::Display for AcornSyntaxError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(&self.message)
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) struct AcornMetaObjectSpan {
    pub(crate) object_start_utf16: usize,
    pub(crate) object_end_utf16: usize,
    pub(crate) declaration_end_utf16: usize,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub(crate) struct AcornParseResult {
    pub(crate) meta_object: Option<AcornMetaObjectSpan>,
}

#[derive(Debug, Deserialize)]
struct MetaObjectSpanReply {
    object_start_utf16: usize,
    object_end_utf16: usize,
    declaration_end_utf16: usize,
}

#[derive(Debug, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
enum ParseReply {
    Valid {
        #[serde(default)]
        meta_object: Option<MetaObjectSpanReply>,
    },
    SyntaxError {
        message: String,
        line: Option<usize>,
        column_utf16: Option<usize>,
        position_utf16: Option<usize>,
    },
}

/// Parse a workflow as Native's qS fallback does, returning syntax validity,
/// the first exported const-meta object span (when present), and structured
/// syntax errors. No workflow body is executed here.
pub(crate) fn parse(source: &str) -> Result<AcornParseResult, AcornParserError> {
    let source_literal = serde_json::to_string(source)
        .map_err(|error| AcornParserError::Runtime(format!("encode source: {error}")))?;
    let request = format!(
        r#"(() => {{
            const source = {source_literal};
            try {{
                const program = globalThis.acorn.parse(source, {{
                    ecmaVersion: "latest",
                    sourceType: "module",
                    allowAwaitOutsideFunction: true,
                    allowReturnOutsideFunction: true
                }});
                const statement = program.body[0];
                let metaObject = null;
                if (statement && statement.type === "ExportNamedDeclaration") {{
                    const declaration = statement.declaration;
                    if (declaration && declaration.type === "VariableDeclaration" &&
                        declaration.kind === "const" && declaration.declarations.length === 1) {{
                        const declarator = declaration.declarations[0];
                        if (declarator.id && declarator.id.type === "Identifier" &&
                            declarator.id.name === "meta" && declarator.init &&
                            declarator.init.type === "ObjectExpression") {{
                            metaObject = {{
                                object_start_utf16: declarator.init.start,
                                object_end_utf16: declarator.init.end,
                                declaration_end_utf16: statement.end
                            }};
                        }}
                    }}
                }}
                return JSON.stringify({{ kind: "valid", meta_object: metaObject }});
            }} catch (error) {{
                const location = error && typeof error === "object" ? error.loc : null;
                return JSON.stringify({{
                    kind: "syntax_error",
                    message: error instanceof Error ? error.message : String(error),
                    line: location && typeof location.line === "number" ? location.line : null,
                    column_utf16: location && typeof location.column === "number" ? location.column : null,
                    position_utf16: error && typeof error.pos === "number" ? error.pos : null
                }});
            }}
        }})()"#
    );

    let runtime = Runtime::new().map_err(|error| AcornParserError::Runtime(error.to_string()))?;
    let context =
        Context::full(&runtime).map_err(|error| AcornParserError::Runtime(error.to_string()))?;
    let reply = context.with(|context| -> Result<ParseReply, AcornParserError> {
        context
            .eval::<rquickjs::Value, _>(ACORN_SOURCE.as_bytes())
            .map_err(|error| AcornParserError::Runtime(error.to_string()))?;

        let version = context
            .eval::<String, _>(b"globalThis.acorn.version")
            .map_err(|error| AcornParserError::Runtime(error.to_string()))?;
        if version != ACORN_VERSION {
            return Err(AcornParserError::Runtime(format!(
                "pinned parser version mismatch: expected {ACORN_VERSION}, found {version}"
            )));
        }

        let json = context
            .eval::<String, _>(request.as_bytes())
            .map_err(|error| AcornParserError::Runtime(error.to_string()))?;
        serde_json::from_str(&json)
            .map_err(|error| AcornParserError::Runtime(format!("decode parser reply: {error}")))
    })?;

    match reply {
        ParseReply::Valid { meta_object } => Ok(AcornParseResult {
            meta_object: meta_object.map(|span| AcornMetaObjectSpan {
                object_start_utf16: span.object_start_utf16,
                object_end_utf16: span.object_end_utf16,
                declaration_end_utf16: span.declaration_end_utf16,
            }),
        }),
        ParseReply::SyntaxError {
            message,
            line,
            column_utf16,
            position_utf16,
        } => Err(AcornParserError::Syntax(AcornSyntaxError {
            message,
            line,
            column_utf16,
            position_utf16,
        })),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn pinned_acorn_accepts_native_module_options_without_running_body() {
        parse(
            "export const meta = { name: 'x', description: 'd' }; await Promise.resolve(); return 7;",
        )
        .expect("qS parser options admit top-level await and return");
        parse("throw new Error('workflow body must not execute');")
            .expect("parser accepts syntax without executing the body");
    }

    #[test]
    fn syntax_diagnostics_keep_acorn_message_and_utf16_location() {
        let error = parse("const emoji='🚀'; const = 1;")
            .expect_err("invalid declaration should be rejected");
        let AcornParserError::Syntax(error) = error else {
            panic!("syntax errors should retain Acorn's location: {error:?}");
        };
        assert_eq!(error.message, "Unexpected token (1:24)");
        assert_eq!(error.line, Some(1));
        assert_eq!(error.column_utf16, Some(24));
        assert_eq!(error.position_utf16, Some(24));
    }
}
