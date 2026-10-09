//! Bounded native contact search.

use async_trait::async_trait;
use lingxi_core::host::contacts::{ContactsError, ContactsQuery};
use once_cell::sync::Lazy;
use permission::PermissionResult;
use serde::Deserialize;
use serde_json::{json, Value};
use tool_api::context::ToolUseContext;
use tool_api::progress::ToolProgressSender;
use tool_api::tool_trait::{
    DescriptionOptions, PromptOptions, Tool, ToolCallResult, ToolError, ToolStaticContext,
    ValidationError,
};
use tool_api::BuiltinToolContext;

use crate::device_support::{ask, bound_text, bounded_records, execute, failure, invalid, parse};

/// Canonical model-facing tool name.
pub const TOOL_NAME: &str = "contacts";
const MAX_QUERY_CHARS: usize = 200;
const MAX_LIMIT: u32 = 50;

fn default_limit() -> u32 {
    20
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct Input {
    query: String,
    #[serde(default = "default_limit")]
    limit: u32,
}

fn query(input: &Value) -> Result<ContactsQuery, ValidationError> {
    let input: Input = parse(input)?;
    let query = input.query.trim();
    if query.is_empty() || query.chars().count() > MAX_QUERY_CHARS {
        return Err(ValidationError(
            "query must contain 1..=200 characters after trimming".into(),
        ));
    }
    if !(1..=MAX_LIMIT).contains(&input.limit) {
        return Err(ValidationError(
            "limit must be an integer from 1 through 50".into(),
        ));
    }
    Ok(ContactsQuery {
        query: query.into(),
        limit: input.limit,
    })
}

/// Search contacts by display name without exposing the whole address book.
#[derive(Clone)]
pub struct ContactsTool {
    ctx: BuiltinToolContext,
}

impl ContactsTool {
    /// Construct using the native provider in the builtin context.
    #[must_use]
    pub fn new(ctx: BuiltinToolContext) -> Self {
        Self { ctx }
    }
}

static INPUT_SCHEMA: Lazy<Value> = Lazy::new(|| {
    json!({
        "type": "object", "properties": {
            "query": { "type": "string", "minLength": 1, "maxLength": MAX_QUERY_CHARS, "description": "Non-empty display-name search text; surrounding whitespace is trimmed." },
            "limit": { "type": "integer", "minimum": 1, "maximum": MAX_LIMIT, "default": 20 }
        }, "required": ["query"], "additionalProperties": false
    })
});

#[async_trait]
impl Tool for ContactsTool {
    fn name(&self) -> &str {
        TOOL_NAME
    }
    fn input_schema(&self) -> &Value {
        &INPUT_SCHEMA
    }
    fn is_enabled(&self, _: &ToolStaticContext) -> bool {
        self.ctx.contacts.is_some()
    }
    fn max_result_size_chars(&self) -> usize {
        32768
    }
    fn is_concurrency_safe(&self, _: &Value) -> bool {
        false
    }
    fn is_read_only(&self, _: &Value) -> bool {
        true
    }
    fn search_hint(&self) -> Option<&str> {
        Some("Search contacts, address book, phone numbers and email addresses by name")
    }
    async fn check_permissions(&self, _: &Value, _: &ToolUseContext) -> PermissionResult {
        ask(TOOL_NAME, "Search contacts by the requested name and read matching phone numbers and email addresses. Native contacts permission may also be required.")
    }
    async fn description(&self, _: &Value, _: &DescriptionOptions) -> String {
        "Searching device contacts by name".into()
    }
    async fn prompt(&self, _: &PromptOptions) -> String {
        "Search native contacts by a non-empty display-name query of at most 200 characters. Returns contacts with id, display_name, phones and emails. Limit is 1..50 (default 20). A true truncated flag means the native result reached the requested limit (more contacts may exist), or returned data was shortened or omitted. It does not prove that more contacts exist. Use the specific person requested; this tool cannot modify contacts or list the entire address book.".into()
    }
    async fn validate_input(
        &self,
        input: &Value,
        _: &ToolUseContext,
    ) -> Result<(), ValidationError> {
        query(input).map(|_| ())
    }
    async fn call(
        &self,
        input: Value,
        ctx: ToolUseContext,
        _: ToolProgressSender,
    ) -> Result<ToolCallResult, ToolError> {
        let request = query(&input).map_err(invalid)?;
        let limit = request.limit as usize;
        let Some(provider) = &self.ctx.contacts else {
            return Ok(failure(
                "unavailable",
                "Contacts are unavailable on this device",
            ));
        };
        Ok(match execute(&ctx, provider.search(request)).await? {
            Ok(mut contacts) => {
                // Native providers cap their Vec at limit without reporting has_more.
                // Treat a full page as potentially incomplete, even at an exact match.
                let mut truncated = contacts.len() >= limit;
                contacts.truncate(limit);
                for contact in &mut contacts {
                    truncated |= bound_text(&mut contact.id, 512);
                    truncated |= bound_text(&mut contact.display_name, 500);
                    for values in [&mut contact.phones, &mut contact.emails] {
                        truncated |= values.len() > 10;
                        values.truncate(10);
                        for text in values {
                            truncated |= bound_text(text, 256);
                        }
                    }
                }
                bounded_records(
                    "contacts",
                    contacts.into_iter().map(|contact| json!(contact)),
                    self.max_result_size_chars(),
                    truncated,
                )
            }
            Err(error) => failure(
                match &error {
                    ContactsError::Unavailable => "unavailable",
                    ContactsError::PermissionDenied => "permission_denied",
                    ContactsError::Invalid(_) => "invalid_request",
                    ContactsError::Other(_) => "native_error",
                },
                error,
            ),
        })
    }
}
