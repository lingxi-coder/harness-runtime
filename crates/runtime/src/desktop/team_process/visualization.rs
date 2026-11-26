use lingxi_core::types::utf16_json::Utf16JsonProjection as Json;
use serde::{Deserialize, Serialize};
use serde_json::json;
use std::{
    path::{Path, PathBuf},
    sync::Arc,
    time::Duration,
};
use tool_api::tool_trait::*;

pub(super) const NAME: &str = "TeamVisualization";
const MAX_HTML: usize = 48 * 1024;
const ABANDONED_DRAFT_AGE: Duration = Duration::from_secs(30 * 24 * 3600);

fn wire_id(id: &str) -> bool {
    (1..=64).contains(&id.len())
        && id
            .bytes()
            .all(|c| c.is_ascii_alphanumeric() || c == b'_' || c == b'-')
}

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
        wire_id(&self.artifact_id)
            && self.name == bundle.title
            && !bundle.title.trim().is_empty()
            && bundle.title.encode_utf16().count() <= 80
            && bundle.html.len() <= MAX_HTML
            && !bundle.html.trim().is_empty()
            && part.media_type == "application/json"
            && part.data.version == 1
            && part.data.bindings.len() <= 4
            && part.data.bindings.iter().all(|binding| {
                wire_id(&binding.alias)
                    && aliases.insert(&binding.alias)
                    && wire_id(&binding.message_id)
                    && binding.source_alias.as_deref().is_none_or(wire_id)
            })
    }
}

pub(super) struct Draft {
    task: String,
    path: PathBuf,
}

impl Draft {
    pub(super) fn new(root: &Path, task: &str) -> Self {
        let home = root.join("home");
        let path = home.join(format!("visualization-{task}.json"));
        if let Ok(entries) = std::fs::read_dir(&home) {
            for entry in entries.flatten() {
                let name = entry.file_name();
                let Some((id, suffix)) = name
                    .to_str()
                    .and_then(|name| name.strip_prefix("visualization-"))
                    .and_then(|name| name.rsplit_once('.'))
                else {
                    continue;
                };
                if wire_id(id)
                    && (suffix == "json"
                        || (suffix.len() == 32
                            && suffix.bytes().all(|digit| digit.is_ascii_hexdigit())))
                    && (id != task || suffix != "json")
                    && entry.file_type().is_ok_and(|kind| kind.is_file())
                    && entry
                        .metadata()
                        .and_then(|metadata| metadata.modified())
                        .ok()
                        .and_then(|modified| modified.elapsed().ok())
                        .is_some_and(|age| age > ABANDONED_DRAFT_AGE)
                {
                    let _ = std::fs::remove_file(entry.path());
                }
            }
        }
        Self {
            task: task.into(),
            path,
        }
    }

    pub(super) fn remove(&self) {
        let _ = std::fs::remove_file(&self.path);
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
    let identifier =
        json!({"type":"string","minLength":1,"maxLength":64,"pattern":"^[A-Za-z0-9_-]+$"});
    let schema = json!({"type":"object","additionalProperties":false,"required":["title","html"],"properties":{
        "title":{"type":"string","maxLength":80},
        "html":{"type":"string","description":"Self-contained visualize HTML fragment with inline CSS/JS. Use SVG, Canvas, D3 or form controls."},
        "bindings":{"type":"array","maxItems":4,"items":{"type":"object","additionalProperties":false,
          "required":["alias","messageId"],"properties":{"alias":identifier,"messageId":identifier,"sourceAlias":identifier}}}
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
        if !artifact.valid() {
            return Ok(super::list_tools::done(
                &json!({"error":"Provide a title, offline HTML under 48 KiB, and up to four unique aliases. For source message IDs and aliases use 1–64 letters, digits, underscores or hyphens."}),
                true,
            ));
        }
        if !visualization::checks::check_fragment(&artifact.parts[0].data.bundle.html)
            .errors
            .is_empty()
        {
            return Ok(super::list_tools::done(
                &json!({"error":"Use a valid offline HTML fragment under 48 KiB"}),
                true,
            ));
        }
        let next = self
            .draft
            .path
            .with_extension(uuid::Uuid::new_v4().simple().to_string());
        let saved = std::fs::OpenOptions::new()
            .write(true)
            .create_new(true)
            .open(&next)
            .and_then(|mut file| {
                let result = (|| {
                    serde_json::to_writer(&mut file, &artifact)?;
                    file.sync_all()?;
                    drop(file);
                    std::fs::rename(&next, &self.draft.path)
                })();
                if result.is_err() {
                    let _ = std::fs::remove_file(next);
                }
                result
            });
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

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn binding_identifiers_use_the_channel_wire_format() {
        assert!(wire_id("order_rows-1"));
        assert!(wire_id(&"x".repeat(64)));
        for invalid in ["", "order rows", "库存", &"x".repeat(65)] {
            assert!(!wire_id(invalid));
        }
    }
}
