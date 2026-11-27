use lingxi_core::types::utf16_json::Utf16JsonProjection as Json;
use serde::{Deserialize, Serialize};
use serde_json::json;
use sha2::{Digest, Sha256};
use std::{
    io::{self, Write},
    path::{Path, PathBuf},
    sync::Arc,
    time::Duration,
};
use tool_api::tool_trait::*;

pub(super) const NAME: &str = "TeamVisualization";
const MAX_HTML: usize = 50 * 1024 * 1024;
const PART_BYTES: usize = 2 * 1024 * 1024;
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
    html: HtmlFile,
}

#[derive(Debug, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct HtmlFile {
    path: PathBuf,
    bytes: usize,
    sha256: String,
    parts: Vec<String>,
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
            && (1..=MAX_HTML).contains(&bundle.html.bytes)
            && super::checkpoint::valid_digest(&bundle.html.sha256)
            && bundle.html.parts.len() == bundle.html.bytes.div_ceil(PART_BYTES)
            && bundle
                .html
                .parts
                .iter()
                .all(|part| super::checkpoint::valid_digest(part))
            && part.media_type == "application/json"
            && part.data.version == 2
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
                let temporary =
                    suffix.len() == 32 && suffix.bytes().all(|digit| digit.is_ascii_hexdigit());
                let draft = wire_id(id) && (temporary || suffix == "json" && id != task);
                let payload = id.rsplit_once('.').is_some_and(|(id, digest)| {
                    wire_id(id)
                        && super::checkpoint::valid_digest(digest)
                        && (temporary || suffix == "html" && id != task)
                });
                if (draft || payload)
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
        let html = &artifact.parts[0].data.bundle.html;
        let expected = self.path.with_extension(format!("{}.html", html.sha256));
        (artifact.valid() && artifact.artifact_id == self.task && html.path == expected)
            .then_some(artifact)
    }

    fn save(&self, title: String, html: &str, bindings: Vec<Binding>) -> io::Result<()> {
        let bytes = html.as_bytes();
        let sha256 = format!("{:x}", Sha256::digest(bytes));
        let path = self.path.with_extension(format!("{sha256}.html"));
        let previous = self
            .load()
            .map(|artifact| artifact.parts[0].data.bundle.html.path.clone());
        write_atomic(&path, |file| file.write_all(bytes))?;
        let artifact = Artifact {
            artifact_id: self.task.clone(),
            name: title.clone(),
            parts: [Part {
                media_type: "application/json".into(),
                data: Data {
                    version: 2,
                    bindings,
                    bundle: Bundle {
                        title,
                        html: HtmlFile {
                            path: path.clone(),
                            bytes: bytes.len(),
                            sha256,
                            parts: bytes
                                .chunks(PART_BYTES)
                                .map(|part| format!("{:x}", Sha256::digest(part)))
                                .collect(),
                        },
                    },
                },
            }],
        };
        let saved = write_atomic(&self.path, |file| {
            serde_json::to_writer(file, &artifact).map_err(io::Error::from)
        });
        if let Some(obsolete) = previous.filter(|old| saved.is_ok() && *old != path) {
            let _ = std::fs::remove_file(obsolete);
        }
        saved
    }
}

fn write_atomic(
    path: &Path,
    write: impl FnOnce(&mut std::fs::File) -> io::Result<()>,
) -> io::Result<()> {
    let next = path.with_extension(uuid::Uuid::new_v4().simple().to_string());
    let mut options = std::fs::OpenOptions::new();
    options.write(true).create_new(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt;
        options.mode(0o600).custom_flags(nix::libc::O_NOFOLLOW);
    }
    let mut file = options.open(&next)?;
    let result = (|| {
        write(&mut file)?;
        file.sync_all()?;
        drop(file);
        std::fs::rename(&next, path)?;
        #[cfg(unix)]
        std::fs::File::open(path.parent().ok_or(io::ErrorKind::InvalidInput)?)?.sync_all()?;
        Ok(())
    })();
    if result.is_err() {
        let _ = std::fs::remove_file(next);
    }
    result
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
        let title = parsed.title.trim();
        let mut aliases = std::collections::BTreeSet::new();
        if title.is_empty()
            || title.encode_utf16().count() > 80
            || parsed.bindings.len() > 4
            || !parsed.bindings.iter().all(|binding| {
                wire_id(&binding.alias)
                    && aliases.insert(&binding.alias)
                    && wire_id(&binding.message_id)
                    && binding.source_alias.as_deref().is_none_or(wire_id)
            })
            || !wire_id(&self.draft.task)
        {
            return Ok(super::list_tools::done(
                &json!({"error":"Provide a title and up to four unique aliases. For source message IDs and aliases use 1–64 letters, digits, underscores or hyphens."}),
                true,
            ));
        }
        if !visualization::checks::check_fragment_with_limit(&parsed.html, MAX_HTML)
            .is_publishable()
        {
            return Ok(super::list_tools::done(
                &json!({"error":"Use a valid offline HTML fragment up to 50 MiB"}),
                true,
            ));
        }
        let saved = self.draft.save(title.into(), &parsed.html, parsed.bindings);
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

    #[test]
    fn large_draft_survives_with_a_small_result_frame() {
        let root = tempfile::tempdir().unwrap();
        std::fs::create_dir(root.path().join("home")).unwrap();
        let draft = Draft::new(root.path(), "task-large");
        let mut html = "<p>Orders</p>".to_owned();
        html.extend(std::iter::repeat_n(' ', MAX_HTML - html.len()));
        assert!(visualization::checks::check_fragment_with_limit(&html, MAX_HTML).is_publishable());
        draft.save("Orders".into(), &html, vec![]).unwrap();
        let artifact = draft.load().unwrap();
        let file = &artifact.parts[0].data.bundle.html;
        assert_eq!(file.bytes, MAX_HTML);
        assert_eq!(file.parts.len(), 25);
        assert!(super::super::protocol::encode(&artifact).unwrap().len() < 4096);
        draft.remove();
        assert_eq!(
            std::fs::metadata(&file.path).unwrap().len(),
            MAX_HTML as u64
        );
    }
}
