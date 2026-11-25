use lingxi_core::types::utf16_json::Utf16JsonProjection as Json;
use serde::{Deserialize, Serialize};
use serde_json::json;
use std::{
    path::{Path, PathBuf},
    sync::Arc,
};
use tool_api::tool_trait::*;

pub(super) const NAME: &str = "TeamVisualization";
const MAX_HTML: usize = 48 * 1024;

#[derive(Debug, Serialize, Deserialize)]
#[serde(deny_unknown_fields, rename_all = "camelCase")]
pub struct Artifact {
    artifact_id: String,
    name: String,
    parts: [Part; 1],
}

#[derive(Debug, Serialize, Deserialize)]
#[serde(deny_unknown_fields, rename_all = "camelCase")]
struct Part {
    media_type: String,
    data: Data,
}

#[derive(Debug, Serialize, Deserialize)]
#[serde(deny_unknown_fields, rename_all = "camelCase")]
struct Data {
    version: u8,
    bundle: Bundle,
    bindings: Vec<Binding>,
}

#[derive(Debug, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct Bundle {
    title: String,
    html: String,
}

#[derive(Debug, Serialize, Deserialize)]
#[serde(deny_unknown_fields, rename_all = "camelCase")]
struct Binding {
    alias: String,
    message_id: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    source_alias: Option<String>,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct Input {
    title: String,
    html: String,
    #[serde(default)]
    bindings: Vec<Binding>,
}

impl Artifact {
    pub(super) fn valid(&self) -> bool {
        let part = &self.parts[0];
        let bundle = &part.data.bundle;
        let mut aliases = std::collections::BTreeSet::new();
        let valid_alias = |id: &str| {
            !id.is_empty()
                && id.len() <= 64
                && id
                    .bytes()
                    .all(|c| c.is_ascii_alphanumeric() || c == b'_' || c == b'-')
        };
        super::protocol::valid_id(&self.artifact_id)
            && self.name == bundle.title
            && !bundle.title.trim().is_empty()
            && bundle.title.encode_utf16().count() <= 80
            && bundle.html.len() <= MAX_HTML
            && !bundle.html.trim().is_empty()
            && part.media_type == "application/json"
            && part.data.version == 1
            && part.data.bindings.len() <= 4
            && part.data.bindings.iter().all(|binding| {
                valid_alias(&binding.alias)
                    && aliases.insert(&binding.alias)
                    && super::protocol::valid_id(&binding.message_id)
                    && binding.source_alias.as_deref().is_none_or(valid_alias)
            })
    }
}

pub(super) struct Draft {
    task: String,
    path: PathBuf,
}

impl Draft {
    pub(super) fn new(root: &Path, task: &str) -> Self {
        Self {
            task: task.into(),
            path: root.join("home").join(format!("visualization-{task}.json")),
        }
    }

    pub(super) fn load(&self) -> Option<Artifact> {
        let artifact: Artifact = serde_json::from_slice(&std::fs::read(&self.path).ok()?).ok()?;
        (artifact.valid() && artifact.artifact_id == self.task).then_some(artifact)
    }
}

struct PublishTool {
    draft: Arc<Draft>,
    schema: Json,
}

pub(super) fn register(registry: &mut tool_api::ToolRegistry, draft: Arc<Draft>) {
    let schema = json!({"type":"object","additionalProperties":false,"required":["title","html"],"properties":{
        "title":{"type":"string","maxLength":80},
        "html":{"type":"string","description":"Self-contained visualize HTML fragment with inline CSS/JS. Use SVG, Canvas, D3 or form controls."},
        "bindings":{"type":"array","maxItems":4,"items":{"type":"object","additionalProperties":false,
          "required":["alias","messageId"],"properties":{"alias":{"type":"string"},"messageId":{"type":"string"},
          "sourceAlias":{"type":"string","description":"Binding alias when the source message is a visualization."}}}}
    }});
    registry.register_builtin(Arc::new(PublishTool {
        draft,
        schema: generated_builtin_schema(&schema),
    }));
}

#[async_trait::async_trait]
impl Tool for PublishTool {
    fn name(&self) -> &str {
        NAME
    }
    fn input_schema(&self) -> &Json {
        &self.schema
    }
    fn input_repair_metadata(&self) -> ToolInputRepairMetadata {
        ToolInputRepairMetadata::Known {
            schema: ToolInputRepairSchema::Advertised,
            schema_provenance: ToolInputSchemaProvenance::GeneratedBuiltin,
            exempt_top_level_keys: None,
        }
    }
    fn is_enabled(&self, _: &ToolStaticContext) -> bool {
        true
    }
    fn max_result_size_chars(&self) -> usize {
        1024
    }
    fn is_concurrency_safe(&self, _: &Json) -> bool {
        false
    }
    fn is_read_only(&self, _: &Json) -> bool {
        false
    }
    async fn description(&self, _: &Json, _: &DescriptionOptions) -> String {
        "Include an interactive visualization in your completed channel reply.".into()
    }
    async fn prompt(&self, _: &PromptOptions) -> Json {
        "Use TeamVisualization for an interactive chart or form. Create a self-contained HTML fragment with inline CSS and JavaScript and a brief plain-text reply. The renderer provides d3 and window.lingxi. For shared data, use TeamChatHistory to identify a channel data message, declare its messageId and an alias, and use await lingxi.data.read(alias) or lingxi.data.watch(alias, callback). Use schema and synthetic examples in the HTML. Never include actual private rows, access credentials or local files. Existing published data access is narrowed to read-only for this output.".into()
    }
    async fn check_permissions(
        &self,
        _: &Json,
        _: &tool_api::context::ToolUseContext,
    ) -> permission::PermissionResult {
        permission::PermissionResult::Allow {
            reason: permission::PermissionDecisionReason::Other {
                reason: "Prepare a channel visualization for the normal task delivery check".into(),
            },
            updated_input: None,
            update_destination: None,
            metadata: Default::default(),
        }
    }
    async fn call(
        &self,
        input: Json,
        _: tool_api::context::ToolUseContext,
        _: tool_api::progress::ToolProgressSender,
    ) -> Result<ToolCallResult, ToolError> {
        let parsed: Option<Input> = input
            .to_json_string()
            .ok()
            .and_then(|value| serde_json::from_str(&value).ok());
        let Some(parsed) = parsed else {
            return Ok(super::list_tools::done(
                &json!({"error":"Invalid visualization"}),
                true,
            ));
        };
        let artifact = Artifact {
            artifact_id: self.draft.task.clone(),
            name: parsed.title.trim().into(),
            parts: [Part {
                media_type: "application/json".into(),
                data: Data {
                    version: 1,
                    bindings: parsed.bindings,
                    bundle: Bundle {
                        title: parsed.title.trim().into(),
                        html: parsed.html,
                    },
                },
            }],
        };
        if !artifact.valid()
            || !visualization::checks::check_fragment(&artifact.parts[0].data.bundle.html)
                .errors
                .is_empty()
        {
            return Ok(super::list_tools::done(
                &json!({"error":"Use a valid offline HTML fragment under 48 KiB"}),
                true,
            ));
        }
        let saved = (|| -> std::io::Result<()> {
            let next = self
                .draft
                .path
                .with_extension(uuid::Uuid::new_v4().simple().to_string());
            let mut file = std::fs::OpenOptions::new()
                .write(true)
                .create_new(true)
                .open(&next)?;
            serde_json::to_writer(&mut file, &artifact)?;
            file.sync_all()?;
            std::fs::rename(next, &self.draft.path)
        })();
        Ok(super::list_tools::done(
            &if saved.is_ok() {
                json!({"prepared":true})
            } else {
                json!({"error":"Visualization could not be prepared"})
            },
            saved.is_err(),
        ))
    }
}
