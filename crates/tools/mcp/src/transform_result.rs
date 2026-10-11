//! MCP tool-result content transform (MCP-5e).
//!
//! 1:1 port of the JSON-reshaping in claude-code's `transformResultContent` /
//! `transformMCPResult` (`services/mcp/client.ts:2478-2706`). For each content
//! block an MCP tool returns, that TS switch converts the raw server block into
//! the model-facing API content block(s):
//!
//! - `text`          → `{type:"text", text}` (passthrough; `client.ts:2483`).
//! - `audio`         → base64-decode + [`persist_blob_to_text_block`] →
//!   a `{type:"text"}` block prefixed `[Audio from <server>] `
//!   (`client.ts:2490-2502`).
//! - `image`         → `{type:"image", source:{type:"base64", data, media_type}}`
//!   with `media_type = "image/<ext>"` where `<ext>` is the segment of `mimeType`
//!   after the `/` (default `png`). The buffer is run through the shared image
//!   resize/downsample ladder first — `client.ts:2503-2523`.
//! - `resource` with `text` → a `{type:"text"}` block prefixed
//!   `[Resource from <server> at <uri>] ` (`client.ts:2528-2534`).
//! - `resource` with `blob` + an image `mimeType` (in [`IMAGE_MIME_TYPES`]) →
//!   a `{type:"text"}` prefix block + an `{type:"image"}` block (same resize
//!   path as `image`) — `client.ts:2538-2563`.
//! - `resource` with `blob` + a non-image `mimeType` → [`persist_blob_to_text_block`]
//!   with the `[Resource from <server> at <uri>] ` prefix (`client.ts:2564-2571`).
//! - `resource_link` → a `{type:"text"}` block `[Resource link: <name>] <uri>`
//!   plus optional ` (<description>)` (`client.ts:2575-2587`).
//!
//! Image processing reuses `tool-file`'s API-budget implementation. Images
//! already within the byte/dimension budget keep their original base64 spelling;
//! oversized images are resized/re-encoded before they reach the model.
//!
//! Persistence (audio + non-image resource blobs) REUSES the MCP-5d
//! [`mcp::mcp_output_storage`] helpers (`decode_base64` + `persist_binary_content`
//! + `binary_blob_saved_message`); no new persistence code lives here.
//!
//! Transform scope mirrors TS exactly: `transformMCPResult` only walks
//! `result.content` when it is an ARRAY (`client.ts:2686`). A non-array
//! `content` (e.g. a bare `toolResult` string) is left untouched here so the
//! caller forwards it verbatim, matching the TS branches that never reach
//! `transformResultContent`.

use std::path::Path;

use lingxi_core::types::utf16_json::Utf16JsonProjection as UJP;
use mcp::normalization::normalize_name_for_mcp;
use mcp::{binary_blob_saved_message, decode_base64, persist_binary_content, PersistBinaryResult};

/// Side data the blob-persisting cases (`audio`, non-image `resource` blob)
/// need: where to write decoded bytes, plus a per-blob `(now_millis, rand_tag)`
/// seed factory. Bundled so [`transform_result_content`] stays a single call
/// and tests can provide deterministic seeds.
#[derive(Clone, Copy)]
pub struct PersistContext<'a> {
    /// Directory decoded blob bytes are written to (a session/tool-results dir
    /// in production; a `tempfile::TempDir` in tests).
    pub output_dir: &'a Path,
    /// Produces a fresh `Date.now()` / random-tag pair for every persisted blob.
    /// Multiple blobs can share a MIME-derived extension, so reusing one seed
    /// for an entire MCP result would overwrite earlier files.
    pub seed_factory: &'a (dyn Fn() -> (u128, String) + Send + Sync),
}

/// MIME types claude-code treats as inline images (binary `Lqd` set). A blob /
/// `image` block with one of these (after [`is_image_mime`] normalization)
/// becomes an inline image block; anything else is persisted to disk.
const IMAGE_MIME_TYPES: &[&str] = &["image/jpeg", "image/png", "image/gif", "image/webp"];

/// `Ara(mimeType)` (binary @198957564): `split(';')[0].trim().toLowerCase()`,
/// normalize `image/jpg` → `image/jpeg`, then membership in [`IMAGE_MIME_TYPES`].
/// A missing/empty mime is not an image. The `;`-strip drops params like
/// `; charset=…`; the jpg→jpeg alias and the lowercase make the gate faithful.
#[must_use]
fn is_image_mime(mime: Option<&str>) -> bool {
    let Some(raw) = mime else {
        return false;
    };
    let base = raw
        .split(';')
        .next()
        .unwrap_or("")
        .trim()
        .to_ascii_lowercase();
    let normalized = if base == "image/jpg" {
        "image/jpeg"
    } else {
        base.as_str()
    };
    IMAGE_MIME_TYPES.contains(&normalized)
}

/// Transform an MCP tool-result content value while retaining exact source
/// strings and keys. Only JSON arrays are reshaped; all other source values are
/// returned unchanged.
#[must_use]
pub fn transform_result_content_source(
    content: &UJP,
    server_name: &str,
    ctx: PersistContext,
) -> UJP {
    let Some(length) = content.array_len("") else {
        return content.clone();
    };
    let mut out = Vec::with_capacity(length);
    for index in 0..length {
        let Ok(index) = u32::try_from(index) else {
            continue;
        };
        let Ok(item) = content.subprojection(&format!("/{index}")) else {
            continue;
        };
        out.extend(transform_block_source(&item, server_name, ctx));
    }
    UJP::array(out).expect("transformed MCP content is a valid JSON array")
}

/// Transform ONE content block, returning the model-facing block(s).
///
/// Faithful to `transformResultContent`'s `switch (resultContent.type)`
/// (`client.ts:2482-2590`):
/// - `text` → `[{type:"text", text}]`
/// - `audio` → `[{type:"text", text: "[Audio from <server>] <saved msg>"}]`
/// - `image` → `[{type:"image", source:{type:"base64", data, media_type}}]`
/// - `resource` with `text` → `[{type:"text", text: "<prefix><text>"}]`
/// - `resource` with `blob` → image sub-case (prefix + image block) or
///   persisted text block
/// - `resource_link` → `[{type:"text", text: "<link form>"}]`
/// - any unrecognized block → the original block verbatim (the TS `default`
///   returns `[]`, but here we preserve the block so unknown shapes survive the
///   round-trip rather than vanishing).
fn transform_block_source(block: &UJP, server_name: &str, ctx: PersistContext) -> Vec<UJP> {
    let kind = block.string_at("/type");
    match kind {
        // case 'text': `let o={type:"text",text}; if(r){if(e._meta)o._meta=e._meta}`
        // — on the tool-result path the binary `Voo`/`Rzr` runs with the 4th arg
        // `r=true` (both real call sites are `Voo(c,n,r,!0)` / `Voo(i,n,r,!0)`),
        // so a source block's per-block `_meta` IS carried onto the emitted text
        // block when present. A block WITHOUT `_meta` stays bare `{type,text}`
        // (no null key). The model-facing STRING reads only `text`, so the
        // preserved `_meta` does not perturb the model-text path.
        Some("text") => {
            let text = source_string_field(block, "text").unwrap_or_else(|| "".into());
            let mut out = text_block_source(text);
            if let Some(meta) = source_field(block, "_meta") {
                out.set_field("_meta", meta)
                    .expect("text block metadata field is unique");
            }
            vec![out]
        }
        // case 'audio': persistBlobToTextBlock(decode(data), mimeType, server,
        //   `[Audio from ${server}] `)  (client.ts:2490-2502)
        Some("audio") => {
            let Some(data) = strict_string_field(block, "data") else {
                return vec![block.clone()];
            };
            let mime = strict_string_field(block, "mimeType");
            let source_description = generated_text(&format!("[Audio from {server_name}] "));
            vec![persist_blob_to_text_block_source(
                &data,
                mime.as_deref(),
                server_name,
                &source_description,
                ctx,
            )]
        }
        // case 'image': binary `Rzr` gates on `Ara(mimeType)` — a recognized
        // image mime → inline image block; otherwise the bytes are persisted to
        // disk as a `[Image from <server>] ` text block (NOT emitted as a broken
        // image the API would reject).
        Some("image") => {
            let Some(data) = strict_string_field(block, "data") else {
                return vec![block.clone()];
            };
            let mime = strict_string_field(block, "mimeType");
            if is_image_mime(mime.as_deref()) {
                vec![image_block_source(&data, mime.as_deref())]
            } else {
                let source_description = generated_text(&format!("[Image from {server_name}] "));
                vec![persist_blob_to_text_block_source(
                    &data,
                    mime.as_deref(),
                    server_name,
                    &source_description,
                    ctx,
                )]
            }
        }
        // case 'resource': text → prefixed text block; blob → image block or
        //   persisted text block (client.ts:2524-2573)
        Some("resource") => transform_resource_source(block, server_name, ctx),
        // case 'resource_link': `[Resource link: ...] ...`
        Some("resource_link") => {
            vec![text_block_source(resource_link_text_source(block))]
        }
        // Any other/unknown block: pass the original block through verbatim (see
        // fn-doc for why TS `default`'s `[]` is intentionally not mirrored).
        _ => vec![block.clone()],
    }
}

/// Transform a `resource` block. Mirrors `client.ts:2524-2573`:
/// - `text` in resource → a single prefixed text block;
/// - `blob` with an image mimeType (in [`IMAGE_MIME_TYPES`]) → a prefix text
///   block + an image block;
/// - `blob` with a non-image mimeType → [`persist_blob_to_text_block`].
fn transform_resource_source(block: &UJP, server_name: &str, ctx: PersistContext) -> Vec<UJP> {
    let Some(resource) = source_field(block, "resource") else {
        // No `resource` field — not a shape TS produces; pass through.
        return vec![block.clone()];
    };
    let uri = source_string_field(&resource, "uri").unwrap_or_else(|| "".into());
    let prefix = concat_text(&[
        &generated_text(&format!("[Resource from {server_name} at ")),
        &uri,
        &generated_text("] "),
    ]);

    // if ('text' in resource) → `${prefix}${resource.text}`
    if let Some(text) = source_string_field(&resource, "text") {
        return vec![text_block_source(concat_text(&[&prefix, &text]))];
    }

    // else if ('blob' in resource) → image sub-case or persist.
    if let Some(blob) = strict_string_field(&resource, "blob") {
        let mime = strict_string_field(&resource, "mimeType");
        let is_image = is_image_mime(mime.as_deref());
        if is_image {
            // content.push(prefix text) then content.push(image block). The TS
            // `if (prefix)` guard is always truthy here — `prefix` is the
            // non-empty `[Resource from ...] ` template — so the prefix block is
            // always emitted, matching `client.ts:2548-2553`.
            return vec![
                text_block_source(prefix),
                image_block_source(&blob, mime.as_deref()),
            ];
        }
        // non-image blob → persistBlobToTextBlock(decode(blob), mimeType,
        //   server, prefix)  (client.ts:2564-2571)
        return vec![persist_blob_to_text_block_source(
            &blob,
            mime.as_deref(),
            server_name,
            &prefix,
            ctx,
        )];
    }

    // Neither text nor blob — TS returns []; preserve the block for safety.
    vec![block.clone()]
}

/// Build a `{type:"image", source:{type:"base64", data, media_type}}` block.
///
/// `media_type` is `"image/<ext>"` where `<ext>` is the part of `mimeType`
/// after the first `/` (default `png`), reproducing
/// `` `image/${resized.mediaType}` `` with `resized.mediaType = ext`
/// (`client.ts:2506,2517-2518`). Within-budget bytes keep the original base64
/// spelling; oversized images use the shared resize/downsample result.
fn image_block_source(data: &str, mime_type: Option<&str>) -> UJP {
    let (resized_data, media_ext) = maybe_resize(data, mime_type);
    source_object(vec![
        ("type".into(), "image".into()),
        (
            "source".into(),
            source_object(vec![
                ("type".into(), "base64".into()),
                ("data".into(), resized_data.into()),
                ("media_type".into(), format!("image/{media_ext}").into()),
            ]),
        ),
    ])
}

/// Analogue of claude-code's `maybeResizeAndDownsampleImageBuffer`
/// (`utils/imageResizer.ts:169`). Invalid image data is left untouched so the
/// normal provider validation remains the source of the eventual error.
fn maybe_resize(data: &str, mime_type: Option<&str>) -> (String, String) {
    let raw_ext = mime_type
        .and_then(|m| m.split(';').next())
        .and_then(|m| m.split('/').nth(1))
        .map(str::trim)
        .filter(|e| !e.is_empty())
        .unwrap_or("png")
        .to_ascii_lowercase();
    // `image/jpg` is not a valid wire media_type — the Anthropic API requires
    // `image/jpeg` (matches the `Ara` jpg→jpeg normalization).
    let ext = if raw_ext == "jpg" {
        "jpeg".to_string()
    } else {
        raw_ext
    };
    let Ok(original_bytes) = decode_base64(data) else {
        return (data.to_string(), ext);
    };
    let Ok(processed) = tool_api::util::image_budget::process_image(original_bytes.clone()) else {
        return (data.to_string(), ext);
    };
    // Preserve the exact original base64 spelling on the no-op path, including
    // harmless whitespace accepted from MCP servers.
    if decode_base64(&processed.base64).ok().as_deref() == Some(original_bytes.as_slice()) {
        return (data.to_string(), ext);
    }
    let processed_ext = processed
        .media_type
        .strip_prefix("image/")
        .unwrap_or("jpeg")
        .to_string();
    (processed.base64, processed_ext)
}

/// Decode base64 `data`, persist the bytes to disk, and return a `{type:"text"}`
/// block describing where they were saved. 1:1 with `persistBlobToTextBlock`
/// (`client.ts:2598-2627`), reusing the MCP-5d [`mcp::mcp_output_storage`]
/// helpers (`decode_base64` + `persist_binary_content` +
/// `binary_blob_saved_message`).
///
/// The `persistId` reproduces the TS template
/// `` `mcp-${normalizeNameForMCP(serverName)}-blob-${Date.now()}-${rand}` `` —
/// note the `-blob-` infix and the normalized server name, distinct from the
/// `mcp-resource-` template used by the 5d resource-read path.
///
/// On a decode or write failure the text block carries
/// `"<sourceDescription>Binary content (<mime>, <n> bytes) could not be saved
/// to disk: <error>"`, mirroring the TS `'error' in result` branch
/// (`client.ts:2607-2613`). NOTE: that error branch reports a RAW BYTE COUNT
/// (`bytes.length`), not `formatFileSize`.
fn persist_blob_to_text_block_source(
    base64_data: &str,
    mime_type: Option<&str>,
    server_name: &str,
    source_description: &UJP,
    ctx: PersistContext,
) -> UJP {
    let mime_label = mime_type
        .filter(|m| !m.is_empty())
        .unwrap_or("unknown type");

    // Buffer.from(data, 'base64') — Node tolerates malformed base64 by decoding
    // what it can; our decoder is stricter. A decode failure surfaces the same
    // "could not be saved to disk" text branch (with the raw byte count of the
    // bytes we managed to decode, i.e. 0), so the model still gets a text block.
    let bytes = match decode_base64(base64_data) {
        Ok(b) => b,
        Err(e) => {
            return text_block_source(concat_text(&[
                source_description,
                &generated_text(&format!(
                    "Binary content ({mime_label}, 0 bytes) could not be saved to disk: {e}"
                )),
            ]));
        }
    };

    // Persist ids are generated per blob because multiple resources in one
    // result can map to the same extension and must not overwrite each other.
    let (now_millis, rand_tag) = (ctx.seed_factory)();
    // persistId = `mcp-${normalizeNameForMCP(serverName)}-blob-${now}-${rand}`
    let persist_id = format!(
        "mcp-{}-blob-{}-{}",
        normalize_name_for_mcp(server_name),
        now_millis,
        rand_tag
    );

    match persist_binary_content(&bytes, mime_type, &persist_id, ctx.output_dir) {
        PersistBinaryResult::Ok { filepath, size, .. } => text_block_source(
            binary_blob_saved_message(&filepath, mime_type, size, source_description),
        ),
        PersistBinaryResult::Err { error } => text_block_source(concat_text(&[
            source_description,
            &generated_text(&format!(
                "Binary content ({mime_label}, {} bytes) could not be saved to disk: {error}",
                bytes.len()
            )),
        ])),
    }
}

/// Build the `resource_link` text form, byte-faithful to `client.ts:2576-2580`:
/// `[Resource link: ${name}] ${uri}` with an optional ` (${description})`
/// suffix when `description` is present.
fn resource_link_text_source(block: &UJP) -> UJP {
    let name = source_string_field(block, "name").unwrap_or_else(|| "".into());
    let uri = source_string_field(block, "uri").unwrap_or_else(|| "".into());
    let mut parts = vec![
        generated_text("[Resource link: "),
        name,
        generated_text("] "),
        uri,
    ];
    if let Some(description) = source_string_field(block, "description") {
        // TS appends only when truthy; an empty string is falsy in JS and is
        // therefore NOT appended.
        if description
            .string_units("")
            .is_some_and(|units| !units.is_empty())
        {
            parts.push(generated_text(" ("));
            parts.push(description);
            parts.push(generated_text(")"));
        }
    }
    concat_text(&parts.iter().collect::<Vec<_>>())
}

fn generated_text(text: &str) -> UJP {
    text.into()
}

fn source_field(value: &UJP, field: &str) -> Option<UJP> {
    value.subprojection(&format!("/{field}")).ok()
}

fn source_string_field(value: &UJP, field: &str) -> Option<UJP> {
    source_field(value, field).filter(|field| field.string_units("").is_some())
}

fn strict_string_field(value: &UJP, field: &str) -> Option<String> {
    String::from_utf16(&source_string_field(value, field)?.string_units("")?).ok()
}

fn concat_text(parts: &[&UJP]) -> UJP {
    let units = parts
        .iter()
        .flat_map(|part| part.string_units("").unwrap_or_default())
        .collect::<Vec<_>>();
    UJP::root_string(String::from_utf16_lossy(&units), units)
        .expect("concatenated UTF-16 strings form a source projection")
}

fn source_object(fields: Vec<(String, UJP)>) -> UJP {
    UJP::object(fields).expect("generated MCP result fields are unique")
}

fn text_block_source(text: UJP) -> UJP {
    source_object(vec![("type".into(), "text".into()), ("text".into(), text)])
}

#[cfg(test)]
fn transform_result_content(
    content: &serde_json::Value,
    server_name: &str,
    ctx: PersistContext,
) -> serde_json::Value {
    let source = UJP::parse(&content.to_string()).expect("test JSON fixture is valid");
    let transformed = transform_result_content_source(&source, server_name, ctx);
    serde_json::from_str(
        &transformed
            .to_json_string()
            .expect("JSON source is serializable"),
    )
    .expect("transformed source is valid JSON")
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    /// A fixed [`PersistContext`] writing into `dir` with a deterministic seed,
    /// so persisted filenames are predictable in assertions.
    fn ctx(dir: &Path) -> PersistContext<'_> {
        fn fixed_seed() -> (u128, String) {
            (1700, "abc123".into())
        }
        PersistContext {
            output_dir: dir,
            seed_factory: &fixed_seed,
        }
    }

    /// Minimal standard base64 encoder for test fixtures only.
    fn b64(bytes: &[u8]) -> String {
        const ALPHABET: &[u8; 64] =
            b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789+/";
        let mut out = String::new();
        for chunk in bytes.chunks(3) {
            let b0 = chunk[0] as usize;
            let b1 = chunk.get(1).copied().unwrap_or(0) as usize;
            let b2 = chunk.get(2).copied().unwrap_or(0) as usize;
            out.push(ALPHABET[b0 >> 2] as char);
            out.push(ALPHABET[((b0 & 0x03) << 4) | (b1 >> 4)] as char);
            if chunk.len() > 1 {
                out.push(ALPHABET[((b1 & 0x0f) << 2) | (b2 >> 6)] as char);
            } else {
                out.push('=');
            }
            if chunk.len() > 2 {
                out.push(ALPHABET[b2 & 0x3f] as char);
            } else {
                out.push('=');
            }
        }
        out
    }

    #[test]
    fn text_block_passthrough() {
        let dir = tempfile::tempdir().unwrap();
        let content = json!([{ "type": "text", "text": "hello world" }]);
        let got = transform_result_content(&content, "srv", ctx(dir.path()));
        assert_eq!(got, json!([{ "type": "text", "text": "hello world" }]));
    }

    #[test]
    fn text_block_missing_text_defaults_empty() {
        let dir = tempfile::tempdir().unwrap();
        let content = json!([{ "type": "text" }]);
        let got = transform_result_content(&content, "srv", ctx(dir.path()));
        assert_eq!(got, json!([{ "type": "text", "text": "" }]));
    }

    #[test]
    fn image_block_passthrough_base64_unchanged() {
        // maybeResize is a passthrough: the base64 data is forwarded UNCHANGED,
        // wrapped in an image block with media_type "image/<ext>".
        let dir = tempfile::tempdir().unwrap();
        let data = b64(b"\x89PNG\r\n\x1a\nfake-png");
        let content = json!([{ "type": "image", "data": data, "mimeType": "image/png" }]);
        let got = transform_result_content(&content, "srv", ctx(dir.path()));
        assert_eq!(
            got,
            json!([{
                "type": "image",
                "source": { "type": "base64", "data": data, "media_type": "image/png" }
            }])
        );
    }

    #[test]
    fn image_block_media_type_from_mime_ext() {
        // media_type ext is the segment after '/'. jpeg → "image/jpeg".
        let dir = tempfile::tempdir().unwrap();
        let content = json!([{ "type": "image", "data": "QUJD", "mimeType": "image/jpeg" }]);
        let got = transform_result_content(&content, "srv", ctx(dir.path()));
        assert_eq!(got[0]["source"]["media_type"], json!("image/jpeg"));
        assert_eq!(got[0]["source"]["data"], json!("QUJD"));
    }

    #[test]
    fn image_block_missing_mime_persisted_not_emitted() {
        // binary `Ara(undefined)` → false (`if(!e)return!1`): a missing mimeType
        // is NOT a recognized image, so the bytes are persisted to disk as a
        // `[Image from <server>] ` text block — NOT emitted as a png image.
        let dir = tempfile::tempdir().unwrap();
        let content = json!([{ "type": "image", "data": b64(b"PNGBYTES") }]);
        let got = transform_result_content(&content, "srv", ctx(dir.path()));
        let blocks = got.as_array().unwrap();
        assert_eq!(blocks.len(), 1);
        assert_eq!(blocks[0]["type"], json!("text"));
        assert!(
            blocks[0]["text"]
                .as_str()
                .unwrap()
                .starts_with("[Image from srv] "),
            "got: {}",
            blocks[0]["text"]
        );
    }

    #[test]
    fn text_block_preserves_per_block_meta() {
        // On the tool-result path (binary `Voo`/`Rzr` with `r=true`) a source
        // text block's per-block `_meta` IS carried onto the emitted text block.
        let dir = tempfile::tempdir().unwrap();
        let content = json!([{ "type": "text", "text": "hi", "_meta": { "k": "v" } }]);
        let got = transform_result_content(&content, "srv", ctx(dir.path()));
        assert_eq!(got[0]["type"], json!("text"));
        assert_eq!(got[0]["text"], json!("hi"));
        assert_eq!(got[0]["_meta"], json!({ "k": "v" }));
        // A text block WITHOUT _meta stays bare (no null _meta key).
        let bare = transform_result_content(
            &json!([{ "type": "text", "text": "x" }]),
            "srv",
            ctx(dir.path()),
        );
        assert!(bare[0].get("_meta").is_none());
        assert_eq!(bare, json!([{ "type": "text", "text": "x" }]));
    }

    #[test]
    fn text_block_preserves_lone_utf16_surrogate_source() {
        let dir = tempfile::tempdir().unwrap();
        let content = UJP::parse(r#"[{"type":"text","text":"\ud800"}]"#)
            .expect("escaped lone surrogate is valid JSON source");
        let got = transform_result_content_source(&content, "srv", ctx(dir.path()));
        assert_eq!(got.string_units("/0/text"), Some(vec![0xd800]));
    }

    #[test]
    fn image_block_jpg_alias_normalized_to_jpeg() {
        // `image/jpg` passes the Ara gate (jpg→jpeg) and the emitted media_type
        // is the wire-valid `image/jpeg`, not `image/jpg`.
        let dir = tempfile::tempdir().unwrap();
        let content = json!([{ "type": "image", "data": "QUJD", "mimeType": "image/jpg" }]);
        let got = transform_result_content(&content, "srv", ctx(dir.path()));
        assert_eq!(got[0]["type"], json!("image"));
        assert_eq!(got[0]["source"]["media_type"], json!("image/jpeg"));
    }

    #[test]
    fn image_block_mime_with_params_stripped() {
        // `image/png; charset=binary` → params stripped, recognized as png.
        let dir = tempfile::tempdir().unwrap();
        let content =
            json!([{ "type": "image", "data": "QUJD", "mimeType": "image/png; charset=binary" }]);
        let got = transform_result_content(&content, "srv", ctx(dir.path()));
        assert_eq!(got[0]["type"], json!("image"));
        assert_eq!(got[0]["source"]["media_type"], json!("image/png"));
    }

    #[test]
    fn audio_block_persisted_to_text() {
        let dir = tempfile::tempdir().unwrap();
        let payload = b"ID3\x03fake-mp3-bytes";
        let data = b64(payload);
        let content = json!([{ "type": "audio", "data": data, "mimeType": "audio/mpeg" }]);
        let got = transform_result_content(&content, "mysrv", ctx(dir.path()));
        // One text block, prefixed `[Audio from mysrv] `.
        assert_eq!(got.as_array().unwrap().len(), 1);
        assert_eq!(got[0]["type"], json!("text"));
        let text = got[0]["text"].as_str().unwrap();
        // persistId template: mcp-<normalizedServer>-blob-<now>-<rand>.<ext>
        let saved_path = dir.path().join("mcp-mysrv-blob-1700-abc123.mp3");
        let saved = saved_path.to_string_lossy();
        assert_eq!(
            text,
            format!(
                "[Audio from mysrv] Binary content (audio/mpeg, {} bytes) saved to {saved}",
                payload.len()
            )
        );
        // Bytes on disk are the RAW decoded payload, not base64.
        assert_eq!(std::fs::read(&saved_path).unwrap(), payload);
    }

    #[test]
    fn audio_block_normalizes_server_name_in_persist_id() {
        // A server name with invalid chars is normalized for the persistId.
        let dir = tempfile::tempdir().unwrap();
        let data = b64(b"x");
        let content = json!([{ "type": "audio", "data": data, "mimeType": "audio/wav" }]);
        let got = transform_result_content(&content, "my.server", ctx(dir.path()));
        let text = got[0]["text"].as_str().unwrap();
        assert!(
            text.contains("mcp-my_server-blob-1700-abc123.wav"),
            "got {text}"
        );
    }

    #[test]
    fn resource_with_text_is_prefixed() {
        let dir = tempfile::tempdir().unwrap();
        let content = json!([{
            "type": "resource",
            "resource": { "uri": "file:///a.txt", "text": "BODY" }
        }]);
        let got = transform_result_content(&content, "myserver", ctx(dir.path()));
        assert_eq!(
            got,
            json!([{
                "type": "text",
                "text": "[Resource from myserver at file:///a.txt] BODY"
            }])
        );
    }

    #[test]
    fn resource_with_text_empty_uri() {
        let dir = tempfile::tempdir().unwrap();
        let content = json!([{
            "type": "resource",
            "resource": { "text": "X" }
        }]);
        let got = transform_result_content(&content, "s", ctx(dir.path()));
        assert_eq!(
            got,
            json!([{ "type": "text", "text": "[Resource from s at ] X" }])
        );
    }

    #[test]
    fn resource_blob_image_emits_prefix_then_image() {
        // An image-mimeType resource blob → a prefix text block + an image block
        // (maybeResize passthrough; base64 unchanged).
        let dir = tempfile::tempdir().unwrap();
        let data = b64(b"\x89PNGblob");
        let content = json!([{
            "type": "resource",
            "resource": { "uri": "mock://img", "blob": data, "mimeType": "image/png" }
        }]);
        let got = transform_result_content(&content, "imgsrv", ctx(dir.path()));
        assert_eq!(
            got,
            json!([
                { "type": "text", "text": "[Resource from imgsrv at mock://img] " },
                {
                    "type": "image",
                    "source": { "type": "base64", "data": data, "media_type": "image/png" }
                }
            ])
        );
        // No file was written for the inline-image sub-case.
        assert!(std::fs::read_dir(dir.path()).unwrap().next().is_none());
    }

    #[test]
    fn resource_blob_webp_is_image() {
        // image/webp is in IMAGE_MIME_TYPES → image block path.
        let dir = tempfile::tempdir().unwrap();
        let content = json!([{
            "type": "resource",
            "resource": { "uri": "u", "blob": "QUJD", "mimeType": "image/webp" }
        }]);
        let got = transform_result_content(&content, "s", ctx(dir.path()));
        assert_eq!(got.as_array().unwrap().len(), 2);
        assert_eq!(got[1]["source"]["media_type"], json!("image/webp"));
    }

    #[test]
    fn oversized_image_is_resized_before_model_delivery() {
        // Valid 3000x1 RGB PNG. Its dimensions exceed the 2000px API cap even
        // though the compressed fixture is tiny.
        const WIDE_PNG: &str = "iVBORw0KGgoAAAANSUhEUgAAC7gAAAABCAIAAADBtXRpAAAAH0lEQVR42u3BAQEAAACCIP+vbkhAAQAAAAAAAADAgQEjKQABp2QvZgAAAABJRU5ErkJggg==";
        let dir = tempfile::tempdir().unwrap();
        let content = json!([{
            "type": "image",
            "data": WIDE_PNG,
            "mimeType": "image/png"
        }]);

        let got = transform_result_content(&content, "images", ctx(dir.path()));

        assert_eq!(got[0]["source"]["media_type"], json!("image/jpeg"));
        let resized = got[0]["source"]["data"].as_str().unwrap();
        assert_ne!(resized, WIDE_PNG);
        assert_eq!(&decode_base64(resized).unwrap()[..2], &[0xff, 0xd8]);
    }

    #[test]
    fn resource_blob_non_image_is_persisted() {
        // A non-image blob (PDF) → persistBlobToTextBlock with the resource
        // prefix.
        let dir = tempfile::tempdir().unwrap();
        let payload = b"%PDF-1.4 body";
        let data = b64(payload);
        let content = json!([{
            "type": "resource",
            "resource": { "uri": "mock://doc", "blob": data, "mimeType": "application/pdf" }
        }]);
        let got = transform_result_content(&content, "fs", ctx(dir.path()));
        assert_eq!(got.as_array().unwrap().len(), 1);
        assert_eq!(got[0]["type"], json!("text"));
        let saved_path = dir.path().join("mcp-fs-blob-1700-abc123.pdf");
        let saved = saved_path.to_string_lossy();
        assert_eq!(
            got[0]["text"].as_str().unwrap(),
            format!(
                "[Resource from fs at mock://doc] Binary content (application/pdf, 13 bytes) saved to {saved}"
            )
        );
        assert_eq!(std::fs::read(&saved_path).unwrap(), payload);
    }

    #[test]
    fn same_extension_blobs_get_distinct_persist_ids_and_keep_both_payloads() {
        let dir = tempfile::tempdir().unwrap();
        let next_seed = std::sync::atomic::AtomicUsize::new(0);
        let seed_factory = || {
            let index = next_seed.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
            let tag = match index {
                0 => "abc123",
                1 => "def456",
                _ => panic!("unexpected extra persisted blob"),
            };
            (1700, tag.to_owned())
        };
        let content = json!([
            {
                "type": "resource",
                "resource": {
                    "uri": "mock://first",
                    "blob": b64(b"first javascript payload"),
                    "mimeType": "application/javascript"
                }
            },
            {
                "type": "resource",
                "resource": {
                    "uri": "mock://second",
                    "blob": b64(b"second javascript payload"),
                    "mimeType": "text/javascript"
                }
            }
        ]);
        let transformed = transform_result_content(
            &content,
            "srv",
            PersistContext {
                output_dir: dir.path(),
                seed_factory: &seed_factory,
            },
        );

        let first_path = dir.path().join("mcp-srv-blob-1700-abc123.js");
        let second_path = dir.path().join("mcp-srv-blob-1700-def456.js");
        assert_ne!(first_path, second_path);
        assert!(transformed[0]["text"]
            .as_str()
            .unwrap()
            .contains(first_path.to_str().unwrap()));
        assert!(transformed[1]["text"]
            .as_str()
            .unwrap()
            .contains(second_path.to_str().unwrap()));
        assert_eq!(
            std::fs::read(first_path).unwrap(),
            b"first javascript payload"
        );
        assert_eq!(
            std::fs::read(second_path).unwrap(),
            b"second javascript payload"
        );
    }

    #[test]
    fn resource_blob_non_image_no_mime_is_persisted_as_bin() {
        // No mimeType → not an image → persisted with `bin` ext and "unknown
        // type" label.
        let dir = tempfile::tempdir().unwrap();
        let data = b64(b"raw");
        let content = json!([{
            "type": "resource",
            "resource": { "uri": "u", "blob": data }
        }]);
        let got = transform_result_content(&content, "s", ctx(dir.path()));
        let text = got[0]["text"].as_str().unwrap();
        assert!(text.contains("unknown type"), "got {text}");
        assert!(text.contains("mcp-s-blob-1700-abc123.bin"), "got {text}");
    }

    #[test]
    fn persist_decode_failure_yields_error_text() {
        // Invalid base64 → the "could not be saved to disk" text branch.
        let dir = tempfile::tempdir().unwrap();
        let content = json!([{ "type": "audio", "data": "!!notb64!!", "mimeType": "audio/mpeg" }]);
        let got = transform_result_content(&content, "s", ctx(dir.path()));
        let text = got[0]["text"].as_str().unwrap();
        assert!(
            text.starts_with(
                "[Audio from s] Binary content (audio/mpeg, 0 bytes) could not be saved to disk:"
            ),
            "got {text}"
        );
    }

    #[test]
    fn resource_link_without_description() {
        let dir = tempfile::tempdir().unwrap();
        let content = json!([{
            "type": "resource_link",
            "name": "Docs",
            "uri": "https://example.com/docs"
        }]);
        let got = transform_result_content(&content, "srv", ctx(dir.path()));
        assert_eq!(
            got,
            json!([{
                "type": "text",
                "text": "[Resource link: Docs] https://example.com/docs"
            }])
        );
    }

    #[test]
    fn resource_link_with_description() {
        let dir = tempfile::tempdir().unwrap();
        let content = json!([{
            "type": "resource_link",
            "name": "Docs",
            "uri": "https://example.com/docs",
            "description": "the docs"
        }]);
        let got = transform_result_content(&content, "srv", ctx(dir.path()));
        assert_eq!(
            got,
            json!([{
                "type": "text",
                "text": "[Resource link: Docs] https://example.com/docs (the docs)"
            }])
        );
    }

    #[test]
    fn resource_link_empty_description_not_appended() {
        let dir = tempfile::tempdir().unwrap();
        let content = json!([{
            "type": "resource_link",
            "name": "N",
            "uri": "u",
            "description": ""
        }]);
        let got = transform_result_content(&content, "srv", ctx(dir.path()));
        assert_eq!(
            got,
            json!([{ "type": "text", "text": "[Resource link: N] u" }])
        );
    }

    #[test]
    fn multiple_blocks_flattened_in_order() {
        let dir = tempfile::tempdir().unwrap();
        let content = json!([
            { "type": "text", "text": "one" },
            { "type": "resource_link", "name": "L", "uri": "u" },
            { "type": "resource", "resource": { "uri": "r", "text": "two" } }
        ]);
        let got = transform_result_content(&content, "srv", ctx(dir.path()));
        assert_eq!(
            got,
            json!([
                { "type": "text", "text": "one" },
                { "type": "text", "text": "[Resource link: L] u" },
                { "type": "text", "text": "[Resource from srv at r] two" }
            ])
        );
    }

    #[test]
    fn unknown_block_passthrough() {
        let dir = tempfile::tempdir().unwrap();
        let block = json!({ "type": "future_thing", "x": 1 });
        let content = json!([block.clone()]);
        let got = transform_result_content(&content, "srv", ctx(dir.path()));
        assert_eq!(got, json!([block]));
    }

    #[test]
    fn non_array_content_passthrough() {
        // TS only reshapes when `result.content` is an array; a bare string
        // (the locked `parity_mcp_invocation` fixture's mock returns "ok")
        // must survive untouched.
        let dir = tempfile::tempdir().unwrap();
        let content = json!("ok");
        let got = transform_result_content(&content, "srv", ctx(dir.path()));
        assert_eq!(got, json!("ok"));

        let obj = json!({ "structuredContent": { "a": 1 } });
        assert_eq!(transform_result_content(&obj, "srv", ctx(dir.path())), obj);
    }
}
