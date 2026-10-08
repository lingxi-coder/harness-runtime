//! Host-owned JSON Schema validation over source-preserving JSON projections.
//!
//! A profile names the Native feature whose dialect and Ajv options apply.
//! Compiling and validating take Core's flat projection directly so object
//! keys, string code units, and binary64 payloads are not normalized through a
//! recursive `serde_json::Value` or a JSON-text round trip.

use crate::types::utf16_json::Utf16JsonProjection;
use async_trait::async_trait;
use std::fmt;
use std::sync::Arc;

/// Native JSON Schema behavior to use for one validation site.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SchemaProfile {
    /// StructuredOutput's Draft-07 Ajv instance: all errors, formats disabled,
    /// `multipleOfPrecision: 6`, with Ajv's default strict/schema-validation
    /// settings.
    StructuredOutputDraft7,
    /// MCP tool output schemas: Ajv 2020-12 with `strict: false`, formats
    /// enabled through `ajv-formats`, schema validation disabled, and all
    /// errors collected.
    McpOutputSchema2020,
}

/// Failure while compiling a schema before it is used for instance checks.
#[derive(Debug)]
pub enum SchemaCompileError {
    /// The source schema is invalid under its selected Native profile. The
    /// diagnostic is an exact root-string projection and may contain lone
    /// UTF-16 units.
    InvalidSchema(Utf16JsonProjection),
    /// The host validator service could not complete compilation.
    Worker(String),
}

impl fmt::Display for SchemaCompileError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::InvalidSchema(detail) => {
                f.write_str("invalid JSON Schema: ")?;
                f.write_str(detail.string_at("").unwrap_or("<invalid exact text>"))
            }
            Self::Worker(detail) => write!(f, "JSON Schema validator worker: {detail}"),
        }
    }
}

impl std::error::Error for SchemaCompileError {}

/// Failure while sending an instance to a compiled schema worker.
#[derive(Debug)]
pub enum SchemaWorkerError {
    /// The persistent host service is unavailable or timed out.
    Unavailable(String),
    /// The worker protocol or its response was malformed.
    Protocol(String),
}

impl fmt::Display for SchemaWorkerError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Unavailable(detail) => {
                write!(f, "JSON Schema validator unavailable: {detail}")
            }
            Self::Protocol(detail) => write!(f, "JSON Schema validator protocol: {detail}"),
        }
    }
}

impl std::error::Error for SchemaWorkerError {}

/// Result of checking one arbitrary source JSON projection.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum JsonSchemaValidation {
    /// The input satisfied the compiled schema.
    Valid,
    /// The input did not satisfy the schema. `detail` is an exact root-string
    /// projection; `keywords` preserves the validator's ordered keyword list
    /// for callers whose Native error code includes it.
    Invalid {
        /// Exact diagnostic text, including any unpaired UTF-16 units.
        detail: Utf16JsonProjection,
        /// Ordered comma-separated Ajv keyword names as individual strings.
        keywords: Vec<String>,
    },
}

/// Long-lived host service that creates isolated Native validator sessions.
pub trait JsonSchemaValidator: Send + Sync {
    /// Create one Native validator instance scope.
    ///
    /// MCP clients retain one session across all of their tools/list schemas so
    /// Ajv's `$id` cache stays client-local. StructuredOutput callers create a
    /// fresh session for each schema, matching Native's per-schema Ajv
    /// constructor.
    fn new_session(&self, profile: SchemaProfile) -> Arc<dyn JsonSchemaValidatorSession>;
}

/// One Native validator instance, scoped to a caller or MCP client.
#[async_trait]
pub trait JsonSchemaValidatorSession: Send + Sync {
    /// Compile a source schema once in this session for subsequent checks.
    async fn compile(
        &self,
        schema: &Utf16JsonProjection,
    ) -> Result<Arc<dyn CompiledJsonSchema>, SchemaCompileError>;
}

/// An immutable schema compiled by a [`JsonSchemaValidator`].
#[async_trait]
pub trait CompiledJsonSchema: Send + Sync {
    /// Validate an exact source projection without changing it.
    async fn validate(
        &self,
        input: &Utf16JsonProjection,
    ) -> Result<JsonSchemaValidation, SchemaWorkerError>;
}
