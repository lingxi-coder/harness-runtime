//! Flat JSON values with exact JavaScript UTF-16 strings and property names.
//!
//! Source values refer to child indices in one flat arena. A replacement-safe
//! `serde_json::Value` mirror supports current Rust consumers, while exact
//! strings, property names, object order, and JavaScript number formatting
//! remain in the flat arena. Flat-carrier clone, parse, and serialization do
//! not recurse with JSON depth; the serde DTO is `{ root, nodes }` and
//! `to_json_string` is the source-authoritative JSON text operation.

use serde::{Deserialize, Deserializer, Serialize};
use serde_json::{Map, Value};
use std::collections::{BTreeSet, HashMap, HashSet};
use std::io::Write;
use thiserror::Error;

#[derive(Debug, PartialEq, Eq, Serialize)]
/// A flat JSON value that retains exact JavaScript UTF-16 string and property-name units.
///
/// Rust-facing display strings remain valid UTF-8 and may use replacement characters for
/// unpaired UTF-16 units. This represents JSON values; it does not retain the original JSON
/// source bytes or lexical spelling.
pub struct Utf16JsonProjection {
    root: usize,
    nodes: Vec<JsonNode>,
    /// Replacement-safe serde display tree for current Rust consumers.
    ///
    /// The flat arena remains the source of truth for exact UTF-16 strings,
    /// object order, and JavaScript number stringification. This mirror uses
    /// U+FFFD for lone surrogates in values and unique placeholders for lone
    /// surrogate keys; callers transforming it must use
    /// [`Self::rebase_display_value`] to retain unchanged exact source text.
    #[serde(skip)]
    pub value: Value,
}

impl Drop for Utf16JsonProjection {
    fn drop(&mut self) {
        let display = std::mem::replace(&mut self.value, Value::Null);
        drop_value_iteratively(display);
    }
}

impl Clone for Utf16JsonProjection {
    fn clone(&self) -> Self {
        Self {
            root: self.root,
            nodes: self.nodes.clone(),
            value: display_value(&self.nodes, self.root)
                .expect("a validated projection has a display value"),
        }
    }
}

/// Raw JSON member adapters for DTOs that carry an arbitrary flat projection.
/// These preserve the canonical JSON value shape without serializing the
/// carrier's internal `{root,nodes}` representation or materializing a
/// recursive `serde_json::Value`.
pub mod raw_json_serde {
    use super::Utf16JsonProjection;
    use serde::{Deserialize, Deserializer, Serialize, Serializer};
    use serde_json::value::RawValue;

    /// Serialize one projection as its actual JSON value.
    pub fn serialize<S>(projection: &Utf16JsonProjection, serializer: S) -> Result<S::Ok, S::Error>
    where
        S: Serializer,
    {
        let source = projection
            .to_json_string()
            .map_err(serde::ser::Error::custom)?;
        let raw = RawValue::from_string(source).map_err(serde::ser::Error::custom)?;
        raw.serialize(serializer)
    }

    /// Parse the current raw JSON member directly into the flat carrier.
    pub fn deserialize<'de, D>(deserializer: D) -> Result<Utf16JsonProjection, D::Error>
    where
        D: Deserializer<'de>,
    {
        let raw = Box::<RawValue>::deserialize(deserializer)?;
        Utf16JsonProjection::parse(raw.get()).map_err(serde::de::Error::custom)
    }

    /// Deserialize a present-or-absent projection field. An explicit JSON
    /// null is retained as `Some(Utf16JsonProjection::Null)`.
    pub fn deserialize_option<'de, D>(
        deserializer: D,
    ) -> Result<Option<Utf16JsonProjection>, D::Error>
    where
        D: Deserializer<'de>,
    {
        let raw = Box::<RawValue>::deserialize(deserializer)?;
        Utf16JsonProjection::parse(raw.get())
            .map(Some)
            .map_err(serde::de::Error::custom)
    }

    /// Serialize an optional raw member. Pair with `skip_serializing_if =
    /// "Option::is_none"` when absence differs from explicit JSON null.
    pub fn serialize_option<S>(
        projection: &Option<Utf16JsonProjection>,
        serializer: S,
    ) -> Result<S::Ok, S::Error>
    where
        S: Serializer,
    {
        match projection {
            Some(projection) => serialize(projection, serializer),
            None => serializer.serialize_none(),
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
enum JsonNode {
    Null,
    Bool(bool),
    /// IEEE-754 binary64 bits. This can represent JavaScript's non-finite
    /// numeric values even though JSON text cannot spell them; stringification
    /// emits `null` for those values, matching `JSON.stringify`.
    Number(u64),
    String {
        display: String,
        code_units: Vec<u16>,
    },
    Array(Vec<usize>),
    Object(Vec<JsonProperty>),
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
struct JsonProperty {
    /// A safe Rust-visible key. For an unpaired UTF-16 name this is a unique
    /// placeholder; `code_units` remains the exact JavaScript property name.
    display: String,
    code_units: Vec<u16>,
    value: usize,
}

#[derive(Debug, Error)]
/// An error while parsing, constructing, validating, or serializing a flat JSON projection.
pub enum Utf16JsonProjectionError {
    #[error("invalid exact JSON at byte {offset}: {message}")]
    /// The UTF-8 JSON input is invalid.
    InvalidJson {
        /// Byte offset reported in the UTF-8 input.
        offset: usize,
        /// Static description of the parse failure.
        message: &'static str,
    },
    #[error("invalid exact JSON at UTF-16 unit {offset}: {message}")]
    /// The JSON document represented as UTF-16 units is invalid.
    InvalidUtf16Json {
        /// UTF-16 code-unit offset reported in the input.
        offset: usize,
        /// Static description of the parse failure.
        message: &'static str,
    },
    #[error("invalid flat JSON projection: {0}")]
    /// The flat node arena violates a projection invariant; the payload describes the issue.
    InvalidProjection(&'static str),
    #[error("UTF-16 string projection does not identify its display string")]
    /// A stored display string does not match the display-safe decoding of its UTF-16 units.
    InvalidStringPointer,
    #[error("UTF-16 key projection does not identify a safe object key")]
    /// A stored display key does not match the safe representation of its UTF-16 key units.
    InvalidKeyPointer,
    #[error("JSON projection pointer is invalid")]
    /// A JSON Pointer is malformed or does not resolve to the requested value.
    InvalidPointer,
    #[error("JSON projection operation requires an object")]
    /// An operation requiring an object root received another JSON value kind.
    ExpectedObject,
    #[error(transparent)]
    /// A serde_json error encountered while converting projection data to JSON.
    Serialize(#[from] serde_json::Error),
}

impl<'de> Deserialize<'de> for Utf16JsonProjection {
    fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
    where
        D: Deserializer<'de>,
    {
        #[derive(Deserialize)]
        struct FlatProjection {
            root: usize,
            nodes: Vec<JsonNode>,
        }

        let flat = FlatProjection::deserialize(deserializer)?;
        let mut projection = Self {
            root: flat.root,
            nodes: flat.nodes,
            value: Value::Null,
        };
        projection.validate().map_err(serde::de::Error::custom)?;
        projection
            .refresh_display_value()
            .map_err(serde::de::Error::custom)?;
        Ok(projection)
    }
}

impl Utf16JsonProjection {
    /// Explicit ingress from the current serde_json interoperability boundary.
    /// The input is copied iteratively into the flat arena; the public carrier
    /// never exposes or retains a recursive `serde_json::Value`.
    pub fn from_serde_value(value: &Value) -> Result<Self, Utf16JsonProjectionError> {
        let mut builder = Utf16JsonBuilder::new();
        let root = builder.import_serde_value(value)?;
        builder.finish(root)
    }

    /// Construct an ordinary JSON projection from a serde display value.
    ///
    /// This is an explicit interoperability boundary. The resulting flat
    /// carrier becomes authoritative; exact source strings cannot be inferred
    /// from a lossy `Value`.
    #[must_use]
    pub fn plain(value: Value) -> Self {
        let projection =
            Self::from_serde_value(&value).expect("serde JSON values fit the projection carrier");
        drop_value_iteratively(value);
        projection
    }

    /// Parse JSON text while retaining every JavaScript UTF-16 string and key.
    /// Numbers use JavaScript binary64 parse semantics, including overflow to
    /// infinity and signed underflow to zero.
    pub fn parse(input: &str) -> Result<Self, Utf16JsonProjectionError> {
        FlatJsonParser::new(input).parse()
    }

    /// Parse a JSON document held in a JavaScript string, including literal
    /// unpaired surrogates inside quoted strings. Error offsets count UTF-16
    /// code units. Escaped characters still obey JSON's escape grammar.
    pub fn parse_utf16_json(input: &[u16]) -> Result<Self, Utf16JsonProjectionError> {
        let mut source = String::new();
        let mut byte_to_unit = Vec::new();
        let mut index = 0;
        let mut in_string = false;
        let mut escaped = false;
        while index < input.len() {
            let unit = input[index];
            let (character, consumed) = if (0xd800..=0xdbff).contains(&unit)
                && input
                    .get(index + 1)
                    .is_some_and(|next| (0xdc00..=0xdfff).contains(next))
            {
                let scalar = 0x10000
                    + ((u32::from(unit) - 0xd800) << 10)
                    + (u32::from(input[index + 1]) - 0xdc00);
                (char::from_u32(scalar), 2)
            } else {
                (char::from_u32(u32::from(unit)), 1)
            };
            let start = source.len();
            if let Some(character) = character {
                source.push(character);
                if in_string {
                    if escaped {
                        escaped = false;
                    } else if character == '\\' {
                        escaped = true;
                    } else if character == '"' {
                        in_string = false;
                    }
                } else if character == '"' {
                    in_string = true;
                }
            } else {
                if !in_string || escaped {
                    return Err(Utf16JsonProjectionError::InvalidUtf16Json {
                        offset: index,
                        message: if escaped {
                            "invalid string escape"
                        } else {
                            "unexpected token"
                        },
                    });
                }
                // This substitution changes only the spelling of an accepted
                // literal string unit. Never turn a bad escaped unit into a
                // valid two-backslash sequence.
                use std::fmt::Write as _;
                write!(&mut source, "\\u{unit:04x}").expect("String writes do not fail");
            }
            byte_to_unit.extend(std::iter::repeat_n(index, source.len() - start));
            index += consumed;
        }
        byte_to_unit.push(input.len());
        Self::parse(&source).map_err(|error| match error {
            Utf16JsonProjectionError::InvalidJson { offset, message } => {
                Utf16JsonProjectionError::InvalidUtf16Json {
                    offset: byte_to_unit.get(offset).copied().unwrap_or(input.len()),
                    message,
                }
            }
            other => other,
        })
    }

    /// Build one exact JavaScript string value. `display` must equal the
    /// display-safe decoding of `code_units`.
    pub fn root_string(
        display: String,
        code_units: Vec<u16>,
    ) -> Result<Self, Utf16JsonProjectionError> {
        if display != String::from_utf16_lossy(&code_units) {
            return Err(Utf16JsonProjectionError::InvalidStringPointer);
        }
        let mut builder = Utf16JsonBuilder::new();
        let root = builder.push_string_units(code_units);
        builder.finish(root)
    }

    /// Rebase a display-tree transformation while retaining exact UTF-16 units
    /// at unchanged string leaves and keys still present in the transformed
    /// tree.
    ///
    /// This method is for typed transformations of the same source value. A
    /// replacement from a new rich source should replace the whole carrier.
    pub fn rebase_display_value(&mut self, value: Value) -> Result<(), Utf16JsonProjectionError> {
        self.validate()?;
        enum Task<'a> {
            Visit(&'a Value, String),
            Array(usize),
            Object(Vec<Vec<u16>>),
        }

        let mut builder = Utf16JsonBuilder::new();
        let mut pending = vec![Task::Visit(&value, String::new())];
        let mut completed = Vec::new();
        while let Some(task) = pending.pop() {
            match task {
                Task::Visit(Value::Array(items), pointer) => {
                    pending.push(Task::Array(items.len()));
                    pending.extend(items.iter().enumerate().rev().map(|(index, child)| {
                        Task::Visit(child, join_pointer(&pointer, &index.to_string()))
                    }));
                }
                Task::Visit(Value::Object(object), pointer) => {
                    let old_entries = value_at_pointer(&self.nodes, self.root, &pointer)
                        .and_then(|id| self.nodes.get(id))
                        .and_then(|node| match node {
                            JsonNode::Object(entries) => Some(entries.as_slice()),
                            _ => None,
                        });
                    let keys = object
                        .keys()
                        .map(|key| {
                            old_entries
                                .and_then(|entries| {
                                    entries.iter().find(|entry| entry.display == *key)
                                })
                                .map(|entry| entry.code_units.clone())
                                .unwrap_or_else(|| key.encode_utf16().collect())
                        })
                        .collect();
                    pending.push(Task::Object(keys));
                    pending.extend(
                        object
                            .iter()
                            .rev()
                            .map(|(key, child)| Task::Visit(child, join_pointer(&pointer, key))),
                    );
                }
                Task::Visit(Value::String(text), pointer) => {
                    let unchanged = value_at_pointer(&self.nodes, self.root, &pointer)
                        .and_then(|id| self.nodes.get(id))
                        .and_then(|node| match node {
                            JsonNode::String {
                                display,
                                code_units,
                            } if display == text => Some(code_units.clone()),
                            _ => None,
                        });
                    completed.push(match unchanged {
                        Some(units) => builder.push_string_units(units),
                        None => builder.push_string(text),
                    });
                }
                Task::Visit(value, _) => completed.push(builder.import_serde_value(value)?),
                Task::Array(length) => {
                    let children = completed.split_off(completed.len() - length);
                    completed.push(builder.push_array(children)?);
                }
                Task::Object(keys) => {
                    let children = completed.split_off(completed.len() - keys.len());
                    completed
                        .push(builder.push_object_units(keys.into_iter().zip(children).collect())?);
                }
            }
        }
        let root = completed
            .pop()
            .ok_or(Utf16JsonProjectionError::InvalidProjection(
                "display-value rebase produced no root node",
            ))?;
        let rebased = builder.finish(root)?;
        drop_value_iteratively(value);
        *self = rebased;
        Ok(())
    }

    /// Replace JSON string values in one iterative traversal. Object keys
    /// remain exact; replacement subtrees are imported once and are not
    /// traversed again. The callback visits values in JavaScript property and
    /// array order, and `None` retains the original string.
    pub fn map_string_values(
        &self,
        mut mapper: impl FnMut(&[u16]) -> Option<Self>,
    ) -> Result<Self, Utf16JsonProjectionError> {
        self.validate()?;
        let mut builder = Utf16JsonBuilder::new();
        let mut mapped = vec![usize::MAX; self.nodes.len()];
        let mut pending = vec![(self.root, false)];
        while let Some((id, complete)) = pending.pop() {
            match &self.nodes[id] {
                JsonNode::Array(items) if !complete => {
                    pending.push((id, true));
                    pending.extend(items.iter().rev().map(|child| (*child, false)));
                }
                JsonNode::Object(entries) if !complete => {
                    pending.push((id, true));
                    let order = javascript_ordered_object_indices(entries);
                    pending.extend(
                        order
                            .into_iter()
                            .rev()
                            .map(|index| (entries[index].value, false)),
                    );
                }
                JsonNode::Array(items) => {
                    mapped[id] = builder.push(JsonNode::Array(
                        items.iter().map(|child| mapped[*child]).collect(),
                    ));
                }
                JsonNode::Object(entries) => {
                    // Original exact keys and display identities are already
                    // validated and never change, so rebuilding their child
                    // indices does not require duplicate-key insertion scans.
                    let entries = entries
                        .iter()
                        .map(|entry| JsonProperty {
                            display: entry.display.clone(),
                            code_units: entry.code_units.clone(),
                            value: mapped[entry.value],
                        })
                        .collect();
                    mapped[id] = builder.push(JsonNode::Object(entries));
                }
                JsonNode::String { code_units, .. } => {
                    mapped[id] = match mapper(code_units) {
                        Some(value) => builder.import_projection(&value)?,
                        None => builder.push_string_units(code_units.clone()),
                    };
                }
                node => mapped[id] = builder.push(node.clone()),
            }
        }
        builder.finish(mapped[self.root])
    }

    /// Construct an array from flat JSON values without building a recursive
    /// intermediate tree.
    pub fn array(values: Vec<Self>) -> Result<Self, Utf16JsonProjectionError> {
        let mut builder = Utf16JsonBuilder::new();
        let mut items = Vec::with_capacity(values.len());
        for value in &values {
            items.push(builder.import_projection(value)?);
        }
        let root = builder.push_array(items)?;
        builder.finish(root)
    }

    /// Construct an object with ordinary Unicode keys from flat JSON values.
    pub fn object(fields: Vec<(String, Self)>) -> Result<Self, Utf16JsonProjectionError> {
        let mut builder = Utf16JsonBuilder::new();
        let mut entries = Vec::with_capacity(fields.len());
        for (key, value) in &fields {
            let node = builder.import_projection(value)?;
            entries.push((key.encode_utf16().collect(), node));
        }
        let root = builder.push_object_units(entries)?;
        builder.finish(root)
    }

    /// Construct an object from exact UTF-16 property names and flat values.
    /// This is required when a current JSON object contains an unpaired
    /// surrogate in a property name.
    pub fn object_units(fields: Vec<(Vec<u16>, Self)>) -> Result<Self, Utf16JsonProjectionError> {
        let mut builder = Utf16JsonBuilder::new();
        let mut entries = Vec::with_capacity(fields.len());
        for (key_units, value) in &fields {
            let node = builder.import_projection(value)?;
            entries.push((key_units.clone(), node));
        }
        let root = builder.push_object_units(entries)?;
        builder.finish(root)
    }

    /// JavaScript-compatible JSON stringification over the flat tree.
    pub fn to_json_string(&self) -> Result<String, Utf16JsonProjectionError> {
        self.validate()?;
        let mut output = Vec::new();
        write_json(&mut output, &self.nodes, self.root)?;
        String::from_utf8(output)
            .map_err(|_| Utf16JsonProjectionError::InvalidProjection("non-UTF-8 JSON output"))
    }

    /// JavaScript-compatible `JSON.stringify(value, null, 2)` over the flat
    /// tree. This is used only for the current tool-result persistence format;
    /// traversal remains iterative at arbitrary JSON depth.
    pub fn to_pretty_json_string(&self) -> Result<String, Utf16JsonProjectionError> {
        self.validate()?;
        let mut output = Vec::new();
        write_pretty_json(&mut output, &self.nodes, self.root)?;
        String::from_utf8(output)
            .map_err(|_| Utf16JsonProjectionError::InvalidProjection("non-UTF-8 JSON output"))
    }

    /// Validate node references, tree ownership, display strings, and exact
    /// key uniqueness. Traversal uses an explicit stack.
    pub fn validate(&self) -> Result<(), Utf16JsonProjectionError> {
        if self.root >= self.nodes.len() {
            return Err(Utf16JsonProjectionError::InvalidProjection(
                "root node is outside the arena",
            ));
        }
        let mut visited = vec![false; self.nodes.len()];
        let mut pending = vec![self.root];
        while let Some(id) = pending.pop() {
            if id >= self.nodes.len() {
                return Err(Utf16JsonProjectionError::InvalidProjection(
                    "child node is outside the arena",
                ));
            }
            if std::mem::replace(&mut visited[id], true) {
                return Err(Utf16JsonProjectionError::InvalidProjection(
                    "flat JSON graph contains a cycle or shared child",
                ));
            }
            match &self.nodes[id] {
                JsonNode::String {
                    display,
                    code_units,
                } => {
                    if display != &String::from_utf16_lossy(code_units) {
                        return Err(Utf16JsonProjectionError::InvalidStringPointer);
                    }
                }
                JsonNode::Array(children) => pending.extend(children.iter().copied()),
                JsonNode::Object(entries) => {
                    let mut seen_units = HashSet::<Vec<u16>>::new();
                    let mut seen_display = HashSet::<&str>::new();
                    for entry in entries {
                        if !seen_units.insert(entry.code_units.clone())
                            || !seen_display.insert(&entry.display)
                        {
                            return Err(Utf16JsonProjectionError::InvalidProjection(
                                "object has duplicate exact or display property names",
                            ));
                        }
                        if let Ok(key) = String::from_utf16(&entry.code_units) {
                            if key != entry.display {
                                return Err(Utf16JsonProjectionError::InvalidKeyPointer);
                            }
                        } else if entry.display.is_empty()
                            || entry.display == String::from_utf16_lossy(&entry.code_units)
                        {
                            return Err(Utf16JsonProjectionError::InvalidKeyPointer);
                        }
                        pending.push(entry.value);
                    }
                }
                JsonNode::Null | JsonNode::Bool(_) | JsonNode::Number(_) => {}
            }
        }
        if visited.iter().any(|seen| !seen) {
            return Err(Utf16JsonProjectionError::InvalidProjection(
                "flat JSON arena contains unreachable nodes",
            ));
        }
        Ok(())
    }

    /// Read a value at an RFC 6901 pointer as a borrowed flat view.
    #[must_use]
    pub fn at_pointer(&self, pointer: &str) -> Option<JsonValueView<'_>> {
        let id = value_at_pointer(&self.nodes, self.root, pointer)?;
        Some(JsonValueView {
            nodes: &self.nodes,
            id,
        })
    }

    /// Borrow a top-level object member without constructing a recursive JSON
    /// value. The returned view remains tied to this flat projection.
    #[must_use]
    pub fn get_property(&self, key: &str) -> Option<JsonValueView<'_>> {
        self.at_pointer("")?.get_property(key)
    }

    /// Whether this root object contains the exact UTF-16 property name.
    #[must_use]
    pub fn contains_property(&self, key: &str) -> bool {
        self.get_property(key).is_some()
    }

    /// Borrow the root object's ordered entries. Exact UTF-16 key units remain
    /// available even when a key has no valid Rust String representation.
    #[must_use]
    pub fn object_entries(&self) -> Option<Vec<JsonObjectEntryView<'_>>> {
        self.at_pointer("")?.object_entries()
    }

    /// Read a string's display form at an RFC 6901 pointer.
    #[must_use]
    pub fn string_at(&self, pointer: &str) -> Option<&str> {
        self.at_pointer(pointer)?.as_str()
    }

    /// Read a string's exact JavaScript UTF-16 units at an RFC 6901 pointer.
    #[must_use]
    pub fn string_units(&self, pointer: &str) -> Option<Vec<u16>> {
        Some(self.at_pointer(pointer)?.code_units()?.to_vec())
    }

    /// Replace one existing string value with exact JavaScript UTF-16 units.
    /// This is used when a raw JSON source is parsed into the flat tree and
    /// current exact-string pointers are applied without materializing a
    /// recursive `serde_json::Value`.
    pub fn set_string_units(
        &mut self,
        pointer: &str,
        code_units: &[u16],
    ) -> Result<(), Utf16JsonProjectionError> {
        let id = value_at_pointer(&self.nodes, self.root, pointer)
            .ok_or(Utf16JsonProjectionError::InvalidPointer)?;
        let Some(JsonNode::String {
            display,
            code_units: target,
        }) = self.nodes.get_mut(id)
        else {
            return Err(Utf16JsonProjectionError::InvalidStringPointer);
        };
        *display = String::from_utf16_lossy(code_units);
        target.clear();
        target.extend_from_slice(code_units);
        self.refresh_display_value()?;
        self.validate()
    }

    /// Read a number at an RFC 6901 pointer as JavaScript binary64.
    #[must_use]
    pub fn number_at(&self, pointer: &str) -> Option<f64> {
        self.at_pointer(pointer)?.as_number()
    }

    /// Read an object's key as exact UTF-16 units, addressed by its safe
    /// display key.
    #[must_use]
    pub fn key_units(&self, parent_pointer: &str, display_key: &str) -> Vec<u16> {
        self.at_pointer(parent_pointer)
            .and_then(|parent| {
                parent
                    .object_entries()?
                    .into_iter()
                    .find(|entry| entry.display_key == display_key)
                    .map(|entry| entry.code_units.to_vec())
            })
            .unwrap_or_else(|| display_key.encode_utf16().collect())
    }

    /// Return the number of array items at a pointer.
    #[must_use]
    pub fn array_len(&self, pointer: &str) -> Option<usize> {
        self.at_pointer(pointer)?.array_len()
    }

    /// Extract a subtree and re-root it as its own flat arena.
    pub fn subprojection(&self, pointer: &str) -> Result<Self, Utf16JsonProjectionError> {
        let id = value_at_pointer(&self.nodes, self.root, pointer)
            .ok_or(Utf16JsonProjectionError::InvalidPointer)?;
        compact_nodes(&self.nodes, id)
    }

    /// Extract a subtree by an RFC 6901 pointer stored as exact UTF-16 units.
    /// This supports object keys containing unpaired surrogates.
    pub fn subprojection_units(&self, pointer: &[u16]) -> Result<Self, Utf16JsonProjectionError> {
        let id = value_at_pointer_units(&self.nodes, self.root, pointer)
            .ok_or(Utf16JsonProjectionError::InvalidPointer)?;
        compact_nodes(&self.nodes, id)
    }

    /// Extract a root-object member by exact UTF-16 key units. This avoids
    /// manufacturing a display pointer for a property name containing an
    /// unpaired surrogate.
    pub fn subprojection_property_units(
        &self,
        key_units: &[u16],
    ) -> Result<Self, Utf16JsonProjectionError> {
        let Some(JsonNode::Object(entries)) = self.nodes.get(self.root) else {
            return Err(Utf16JsonProjectionError::ExpectedObject);
        };
        let id = entries
            .iter()
            .find(|entry| entry.code_units == key_units)
            .map(|entry| entry.value)
            .ok_or(Utf16JsonProjectionError::InvalidPointer)?;
        self.subprojection_for_id(id)
    }

    /// Retain selected top-level fields and their complete flat subtrees.
    pub fn pick_object_fields(&self, fields: &[&str]) -> Result<Self, Utf16JsonProjectionError> {
        let view = self
            .at_pointer("")
            .ok_or(Utf16JsonProjectionError::InvalidPointer)?;
        let entries = view
            .object_entries()
            .ok_or(Utf16JsonProjectionError::ExpectedObject)?;
        let mut selected = Vec::new();
        for field in fields {
            if let Some(entry) = entries.iter().find(|entry| entry.display_key == *field) {
                selected.push((
                    (*field).to_owned(),
                    self.subprojection_for_id(entry.value_id)?,
                ));
            }
        }
        Self::object(selected)
    }

    fn subprojection_for_id(&self, id: usize) -> Result<Self, Utf16JsonProjectionError> {
        compact_nodes(&self.nodes, id)
    }

    /// Replace or insert one top-level object field, consuming the child flat
    /// carrier and preserving exact strings and keys.
    pub fn set_field(&mut self, field: &str, child: Self) -> Result<(), Utf16JsonProjectionError> {
        self.validate()?;
        child.validate()?;
        let field_units: Vec<u16> = field.encode_utf16().collect();
        let root = self.root;
        let entries = match self.nodes.get(root) {
            Some(JsonNode::Object(entries)) => entries,
            _ => return Err(Utf16JsonProjectionError::ExpectedObject),
        };
        let conflicting_placeholder = entries.iter().position(|entry| {
            entry.display == field
                && entry.code_units != field_units
                && String::from_utf16(&entry.code_units).is_err()
        });
        if let Some(index) = conflicting_placeholder {
            let entries = match &self.nodes[root] {
                JsonNode::Object(entries) => entries,
                _ => unreachable!(),
            };
            let replacement = fresh_placeholder(entries, field, "__lingxiUtf16KeyProjectionV2_");
            let Some(JsonNode::Object(entries)) = self.nodes.get_mut(root) else {
                unreachable!();
            };
            entries[index].display = replacement;
        }

        let offset = self.nodes.len();
        let child_root = offset + child.root;
        self.nodes
            .extend(child.nodes.iter().map(|node| shift_node(node, offset)));
        let Some(JsonNode::Object(entries)) = self.nodes.get_mut(root) else {
            unreachable!();
        };
        if let Some(existing) = entries
            .iter_mut()
            .find(|entry| entry.code_units == field_units)
        {
            existing.value = child_root;
        } else {
            entries.push(JsonProperty {
                display: field.to_owned(),
                code_units: field_units,
                value: child_root,
            });
        }
        *self = compact_nodes(&self.nodes, self.root)?;
        Ok(())
    }

    /// Apply object-spread semantics from another flat projection. Existing
    /// properties are replaced in place, new properties keep the source order,
    /// and exact UTF-16 property names and values remain flat throughout.
    pub fn merge_object_projection(
        &mut self,
        source: &Self,
    ) -> Result<(), Utf16JsonProjectionError> {
        self.validate()?;
        source.validate()?;
        if !matches!(self.nodes.get(self.root), Some(JsonNode::Object(_))) {
            return Err(Utf16JsonProjectionError::ExpectedObject);
        }
        let source_entries = match source.nodes.get(source.root) {
            Some(JsonNode::Object(entries)) => entries
                .iter()
                .map(|entry| (entry.display.clone(), entry.code_units.clone(), entry.value))
                .collect::<Vec<_>>(),
            _ => return Err(Utf16JsonProjectionError::ExpectedObject),
        };
        let offset = self.nodes.len();
        self.nodes
            .extend(source.nodes.iter().map(|node| shift_node(node, offset)));
        let Some(JsonNode::Object(entries)) = self.nodes.get_mut(self.root) else {
            unreachable!();
        };
        for (mut display, code_units, value) in source_entries {
            let shifted_value = offset + value;
            if let Some(existing) = entries
                .iter_mut()
                .find(|entry| entry.code_units == code_units)
            {
                existing.value = shifted_value;
            } else {
                if String::from_utf16(&code_units).is_err()
                    && entries.iter().any(|entry| entry.display == display)
                {
                    display = fresh_placeholder(entries, &display, "__lingxiUtf16KeyProjectionV2_");
                }
                entries.push(JsonProperty {
                    display,
                    code_units,
                    value: shifted_value,
                });
            }
        }
        *self = compact_nodes(&self.nodes, self.root)?;
        Ok(())
    }

    /// Replace an existing value selected by an RFC 6901 pointer with another
    /// flat projection. The replacement may be the root, an object member, or
    /// an array item. No recursive JSON value is materialized, and untouched
    /// object keys retain their exact UTF-16 units.
    pub fn replace_pointer_projection(
        &mut self,
        pointer: &str,
        replacement: Self,
    ) -> Result<(), Utf16JsonProjectionError> {
        self.replace_pointer_projection_units(
            &pointer.encode_utf16().collect::<Vec<_>>(),
            replacement,
        )
    }

    /// Replace an existing pointer subtree with another exact projection.
    ///
    /// This current host API shares the transactional source-preserving
    /// implementation used by [`Self::replace_pointer_projection`].
    pub fn set_pointer(
        &mut self,
        pointer: &str,
        child: Self,
    ) -> Result<(), Utf16JsonProjectionError> {
        self.replace_pointer_projection(pointer, child)
    }

    /// UTF-16-unit form of [`Self::replace_pointer_projection`], for property
    /// names that cannot be represented by a Rust `str`.
    pub fn replace_pointer_projection_units(
        &mut self,
        pointer: &[u16],
        replacement: Self,
    ) -> Result<(), Utf16JsonProjectionError> {
        self.validate()?;
        replacement.validate()?;
        if pointer.is_empty() {
            *self = replacement;
            return Ok(());
        }

        if pointer.first() != Some(&u16::from(b'/')) {
            return Err(Utf16JsonProjectionError::InvalidPointer);
        }
        let mut tokens = pointer[1..]
            .split(|unit| *unit == u16::from(b'/'))
            .map(decode_pointer_token_units)
            .collect::<Option<Vec<_>>>()
            .ok_or(Utf16JsonProjectionError::InvalidPointer)?;
        let leaf = tokens
            .pop()
            .ok_or(Utf16JsonProjectionError::InvalidPointer)?;

        let mut parent = self.root;
        for token in &tokens {
            parent = match &self.nodes[parent] {
                JsonNode::Object(entries) => entries
                    .iter()
                    .find(|entry| entry.code_units == *token)
                    .map(|entry| entry.value)
                    .ok_or(Utf16JsonProjectionError::InvalidPointer)?,
                JsonNode::Array(items) => {
                    let index = parse_pointer_index(token)
                        .ok_or(Utf16JsonProjectionError::InvalidPointer)?;
                    *items
                        .get(index)
                        .ok_or(Utf16JsonProjectionError::InvalidPointer)?
                }
                JsonNode::Null
                | JsonNode::Bool(_)
                | JsonNode::Number(_)
                | JsonNode::String { .. } => {
                    return Err(Utf16JsonProjectionError::InvalidPointer);
                }
            };
        }

        let target = match &self.nodes[parent] {
            JsonNode::Object(entries) => entries
                .iter()
                .position(|entry| entry.code_units == leaf)
                .map(|index| (false, index)),
            JsonNode::Array(items) => parse_pointer_index(&leaf)
                .filter(|index| *index < items.len())
                .map(|index| (true, index)),
            JsonNode::Null | JsonNode::Bool(_) | JsonNode::Number(_) | JsonNode::String { .. } => {
                None
            }
        }
        .ok_or(Utf16JsonProjectionError::InvalidPointer)?;

        let offset = self.nodes.len();
        let replacement_root = offset + replacement.root;
        self.nodes.extend(
            replacement
                .nodes
                .iter()
                .map(|node| shift_node(node, offset)),
        );
        match &mut self.nodes[parent] {
            JsonNode::Object(entries) if !target.0 => {
                entries[target.1].value = replacement_root;
            }
            JsonNode::Array(items) if target.0 => {
                items[target.1] = replacement_root;
            }
            _ => return Err(Utf16JsonProjectionError::InvalidPointer),
        }
        *self = compact_nodes(&self.nodes, self.root)?;
        Ok(())
    }

    /// Remove one top-level object member and iteratively compact the arena.
    pub fn remove_field(&mut self, field: &str) -> Result<bool, Utf16JsonProjectionError> {
        self.validate()?;
        let field_units: Vec<u16> = field.encode_utf16().collect();
        let Some(JsonNode::Object(entries)) = self.nodes.get_mut(self.root) else {
            return Err(Utf16JsonProjectionError::ExpectedObject);
        };
        let original_len = entries.len();
        entries.retain(|entry| entry.code_units != field_units);
        if entries.len() == original_len {
            return Ok(false);
        }
        *self = compact_nodes(&self.nodes, self.root)?;
        Ok(true)
    }

    /// Compare two flat JSON trees with JavaScript JSON value equality for
    /// numbers, exact UTF-16 equality for strings/keys, and order-independent
    /// object-member matching.
    #[must_use]
    pub fn semantically_eq(&self, other: &Self) -> bool {
        let mut pending = vec![(self.root, other.root)];
        while let Some((left_id, right_id)) = pending.pop() {
            let (Some(left), Some(right)) = (self.nodes.get(left_id), other.nodes.get(right_id))
            else {
                return false;
            };
            match (left, right) {
                (JsonNode::Null, JsonNode::Null) => {}
                (JsonNode::Bool(left), JsonNode::Bool(right)) if left == right => {}
                (JsonNode::Number(left), JsonNode::Number(right))
                    if f64::from_bits(*left) == f64::from_bits(*right) => {}
                (
                    JsonNode::String {
                        code_units: left, ..
                    },
                    JsonNode::String {
                        code_units: right, ..
                    },
                ) if left == right => {}
                (JsonNode::Array(left), JsonNode::Array(right)) if left.len() == right.len() => {
                    pending.extend(left.iter().copied().zip(right.iter().copied()));
                }
                (JsonNode::Object(left), JsonNode::Object(right)) if left.len() == right.len() => {
                    for entry in left {
                        let Some(other_entry) = right
                            .iter()
                            .find(|other_entry| other_entry.code_units == entry.code_units)
                        else {
                            return false;
                        };
                        pending.push((entry.value, other_entry.value));
                    }
                }
                _ => return false,
            }
        }
        true
    }

    /// Change safe display placeholders for unpaired keys without changing
    /// exact UTF-16 property names or JSON order.
    pub fn rename_key_placeholders(
        &mut self,
        prefix: &str,
    ) -> Result<(), Utf16JsonProjectionError> {
        self.validate()?;
        if prefix.is_empty() || !prefix.is_ascii() {
            return Err(Utf16JsonProjectionError::InvalidProjection(
                "key placeholder namespace must be nonempty ASCII",
            ));
        }
        let mut next = 1_u64;
        for node in &mut self.nodes {
            let JsonNode::Object(entries) = node else {
                continue;
            };
            for index in 0..entries.len() {
                if String::from_utf16(&entries[index].code_units).is_ok() {
                    continue;
                }
                let old = entries[index].display.clone();
                let replacement = loop {
                    let candidate = format!("{prefix}{next}__");
                    next =
                        next.checked_add(1)
                            .ok_or(Utf16JsonProjectionError::InvalidProjection(
                                "key placeholder namespace exhausted",
                            ))?;
                    if candidate == old
                        || entries
                            .iter()
                            .enumerate()
                            .all(|(other, entry)| other == index || entry.display != candidate)
                    {
                        break candidate;
                    }
                };
                entries[index].display = replacement;
            }
        }
        self.refresh_display_value()?;
        self.validate()
    }

    fn refresh_display_value(&mut self) -> Result<(), Utf16JsonProjectionError> {
        let display = display_value(&self.nodes, self.root)?;
        let previous = std::mem::replace(&mut self.value, display);
        drop_value_iteratively(previous);
        Ok(())
    }
}

fn drop_value_iteratively(root: Value) {
    let mut pending = vec![root];
    while let Some(value) = pending.pop() {
        match value {
            Value::Array(mut items) => pending.append(&mut items),
            Value::Object(object) => pending.extend(object.into_iter().map(|(_, value)| value)),
            Value::Null | Value::Bool(_) | Value::Number(_) | Value::String(_) => {}
        }
    }
}

/// Build an exact root string from Rust UTF-8 text. This is useful for
/// locally generated diagnostics whose source is already a Rust `str`; it is
/// not an interchange fallback for arbitrary JSON values.
impl From<String> for Utf16JsonProjection {
    fn from(value: String) -> Self {
        let code_units = value.encode_utf16().collect::<Vec<_>>();
        Self::root_string(value, code_units).expect("UTF-8 text is a valid UTF-16 string")
    }
}

impl From<&str> for Utf16JsonProjection {
    fn from(value: &str) -> Self {
        Self::from(value.to_owned())
    }
}

impl From<&String> for Utf16JsonProjection {
    fn from(value: &String) -> Self {
        Self::from(value.as_str())
    }
}

impl From<&Utf16JsonProjection> for Utf16JsonProjection {
    fn from(value: &Utf16JsonProjection) -> Self {
        value.clone()
    }
}

/// Borrowed, non-recursive view of one flat JSON node.
#[derive(Clone, Copy)]
pub struct JsonValueView<'a> {
    nodes: &'a [JsonNode],
    id: usize,
}

impl<'a> JsonValueView<'a> {
    /// Whether both views identify the same node in the same source arena.
    /// This preserves JavaScript object identity for reference-walk guards.
    #[must_use]
    pub fn same_node_as(self, other: Self) -> bool {
        std::ptr::eq(self.nodes, other.nodes) && self.id == other.id
    }

    /// Return this string's Rust-safe display form, or `None` for non-string values.
    ///
    /// Unpaired UTF-16 units are represented lossily; use [`Self::code_units`] for exact units.
    #[must_use]
    pub fn as_str(self) -> Option<&'a str> {
        match &self.nodes[self.id] {
            JsonNode::String { display, .. } => Some(display),
            _ => None,
        }
    }

    /// Return this string's exact UTF-16 code units, or `None` for non-string values.
    #[must_use]
    pub fn code_units(self) -> Option<&'a [u16]> {
        match &self.nodes[self.id] {
            JsonNode::String { code_units, .. } => Some(code_units),
            _ => None,
        }
    }

    /// Return this number as an `f64`, or `None` for non-number values.
    #[must_use]
    pub fn as_number(self) -> Option<f64> {
        match &self.nodes[self.id] {
            JsonNode::Number(bits) => Some(f64::from_bits(*bits)),
            _ => None,
        }
    }

    /// Return this boolean value, or `None` for non-boolean values.
    #[must_use]
    pub fn as_bool(self) -> Option<bool> {
        match &self.nodes[self.id] {
            JsonNode::Bool(value) => Some(*value),
            _ => None,
        }
    }

    /// Whether this node is the JSON `null` value.
    #[must_use]
    pub fn is_null(self) -> bool {
        matches!(&self.nodes[self.id], JsonNode::Null)
    }

    /// Return the array's number of items, or `None` if this node is not an array.
    #[must_use]
    pub fn array_len(self) -> Option<usize> {
        match &self.nodes[self.id] {
            JsonNode::Array(items) => Some(items.len()),
            _ => None,
        }
    }

    /// Return an in-range array item as a borrowed view.
    #[must_use]
    pub fn get_index(self, index: usize) -> Option<Self> {
        let JsonNode::Array(items) = &self.nodes[self.id] else {
            return None;
        };
        let id = *items.get(index)?;
        Some(Self {
            nodes: self.nodes,
            id,
        })
    }

    /// Look up an object member by a Rust string key, comparing its UTF-16 encoding exactly.
    #[must_use]
    pub fn get_property(self, key: &str) -> Option<Self> {
        self.get_property_units(&key.encode_utf16().collect::<Vec<_>>())
    }

    /// Look up an object member by its exact UTF-16 key units.
    ///
    /// This form can address keys that cannot be represented by a Rust `str`.
    #[must_use]
    pub fn get_property_units(self, units: &[u16]) -> Option<Self> {
        let id = self.property_units(units)?;
        Some(Self {
            nodes: self.nodes,
            id,
        })
    }

    fn property_units(self, units: &[u16]) -> Option<usize> {
        let JsonNode::Object(entries) = &self.nodes[self.id] else {
            return None;
        };
        entries
            .iter()
            .find(|entry| entry.code_units == units)
            .map(|entry| entry.value)
    }

    /// Return the object's members as borrowed views, or `None` for non-object values.
    ///
    /// Each view exposes a safe display key and the separate exact UTF-16 key units.
    pub fn object_entries(self) -> Option<Vec<JsonObjectEntryView<'a>>> {
        let JsonNode::Object(entries) = &self.nodes[self.id] else {
            return None;
        };
        Some(
            entries
                .iter()
                .map(|entry| JsonObjectEntryView {
                    display_key: &entry.display,
                    code_units: &entry.code_units,
                    value: Self {
                        nodes: self.nodes,
                        id: entry.value,
                    },
                    value_id: entry.value,
                })
                .collect(),
        )
    }
}

/// A borrowed object member with a safe key display and its exact key units.
pub struct JsonObjectEntryView<'a> {
    /// Rust-safe display spelling of the key; for an unpaired key this is a placeholder, not its identity.
    pub display_key: &'a str,
    /// Exact JavaScript UTF-16 units used to identify the property name.
    pub code_units: &'a [u16],
    /// Borrowed view of the property's value.
    pub value: JsonValueView<'a>,
    value_id: usize,
}

/// Flat JSON builder used by Core producers to assemble semantic request DTOs.
#[derive(Default)]
pub struct Utf16JsonBuilder {
    nodes: Vec<JsonNode>,
    next_placeholder: u64,
}

impl Utf16JsonBuilder {
    /// Create an empty flat-node builder.
    #[must_use]
    pub fn new() -> Self {
        Self {
            nodes: Vec::new(),
            next_placeholder: 0,
        }
    }

    /// Append a JSON `null` node and return its builder-local node ID.
    pub fn push_null(&mut self) -> usize {
        self.push(JsonNode::Null)
    }

    /// Append a boolean node and return its builder-local node ID.
    pub fn push_bool(&mut self, value: bool) -> usize {
        self.push(JsonNode::Bool(value))
    }

    /// Append an `f64` number node and return its builder-local node ID.
    pub fn push_number(&mut self, value: f64) -> usize {
        self.push(JsonNode::Number(value.to_bits()))
    }

    /// Encode Rust text as UTF-16, append it as a string node, and return its node ID.
    pub fn push_string(&mut self, value: &str) -> usize {
        self.push_string_units(value.encode_utf16().collect())
    }

    /// Append a string node with these exact UTF-16 units and return its node ID.
    ///
    /// The node's Rust display form is derived lossily, so unpaired units remain available through
    /// the exact-unit view.
    pub fn push_string_units(&mut self, code_units: Vec<u16>) -> usize {
        self.push(JsonNode::String {
            display: String::from_utf16_lossy(&code_units),
            code_units,
        })
    }

    /// Append an array whose items refer to earlier nodes in this builder.
    ///
    /// Returns an error if any item ID does not refer to an existing builder node.
    pub fn push_array(&mut self, items: Vec<usize>) -> Result<usize, Utf16JsonProjectionError> {
        self.validate_children(&items)?;
        Ok(self.push(JsonNode::Array(items)))
    }

    /// Append an object from Rust string keys and earlier builder node IDs.
    ///
    /// Returns an error for invalid child IDs or a key/display collision.
    pub fn push_object(
        &mut self,
        fields: Vec<(String, usize)>,
    ) -> Result<usize, Utf16JsonProjectionError> {
        self.push_object_units(
            fields
                .into_iter()
                .map(|(key, value)| (key.encode_utf16().collect(), value))
                .collect(),
        )
    }

    /// Append an object from exact UTF-16 keys and earlier builder node IDs.
    ///
    /// This form supports keys containing unpaired units. Returns an error for invalid child IDs
    /// or a key/display collision.
    pub fn push_object_units(
        &mut self,
        fields: Vec<(Vec<u16>, usize)>,
    ) -> Result<usize, Utf16JsonProjectionError> {
        self.validate_children(&fields.iter().map(|(_, id)| *id).collect::<Vec<_>>())?;
        let mut entries = Vec::<JsonProperty>::new();
        for (code_units, value) in fields {
            if let Some(existing) = entries
                .iter_mut()
                .find(|entry| entry.code_units == code_units)
            {
                existing.value = value;
                continue;
            }
            let display = match String::from_utf16(&code_units) {
                Ok(key) => {
                    if let Some(index) = entries.iter().position(|entry| {
                        entry.display == key
                            && entry.code_units != code_units
                            && String::from_utf16(&entry.code_units).is_err()
                    }) {
                        let replacement = self.allocate_placeholder(&entries);
                        entries[index].display = replacement;
                    }
                    key
                }
                Err(_) => self.allocate_placeholder(&entries),
            };
            if entries.iter().any(|entry| entry.display == display) {
                return Err(Utf16JsonProjectionError::InvalidProjection(
                    "object contains colliding display keys",
                ));
            }
            entries.push(JsonProperty {
                display,
                code_units,
                value,
            });
        }
        Ok(self.push(JsonNode::Object(entries)))
    }

    /// Import a serde_json value by reference using an explicit work stack.
    /// This is the intentional typed-Value interop point; no recursive Value
    /// is retained in the result.
    pub fn import_serde_value(&mut self, value: &Value) -> Result<usize, Utf16JsonProjectionError> {
        enum Task<'a> {
            Visit(&'a Value),
            Array(usize),
            Object(Vec<&'a String>),
        }

        let mut tasks = vec![Task::Visit(value)];
        let mut completed = Vec::<usize>::new();
        while let Some(task) = tasks.pop() {
            match task {
                Task::Visit(Value::Null) => completed.push(self.push_null()),
                Task::Visit(Value::Bool(value)) => completed.push(self.push_bool(*value)),
                Task::Visit(Value::Number(number)) => {
                    let value = number
                        .as_f64()
                        .or_else(|| number.to_string().parse::<f64>().ok())
                        .ok_or(Utf16JsonProjectionError::InvalidProjection(
                            "serde number cannot be converted to JavaScript binary64",
                        ))?;
                    completed.push(self.push_number(value));
                }
                Task::Visit(Value::String(value)) => completed.push(self.push_string(value)),
                Task::Visit(Value::Array(items)) => {
                    tasks.push(Task::Array(items.len()));
                    tasks.extend(items.iter().rev().map(Task::Visit));
                }
                Task::Visit(Value::Object(object)) => {
                    let keys: Vec<_> = object.keys().collect();
                    tasks.push(Task::Object(keys.clone()));
                    tasks.extend(keys.into_iter().rev().map(|key| Task::Visit(&object[key])));
                }
                Task::Array(length) => {
                    let items = completed.split_off(completed.len() - length);
                    let id = self.push_array(items)?;
                    completed.push(id);
                }
                Task::Object(keys) => {
                    let children = completed.split_off(completed.len() - keys.len());
                    let fields = keys
                        .into_iter()
                        .zip(children)
                        .map(|(key, value)| (key.encode_utf16().collect(), value))
                        .collect();
                    let id = self.push_object_units(fields)?;
                    completed.push(id);
                }
            }
        }
        completed
            .pop()
            .ok_or(Utf16JsonProjectionError::InvalidProjection(
                "serde value import produced no root node",
            ))
    }

    /// Copy a validated flat projection into this builder and return its new
    /// root node identifier.
    pub fn import_projection(
        &mut self,
        projection: &Utf16JsonProjection,
    ) -> Result<usize, Utf16JsonProjectionError> {
        projection.validate()?;
        let offset = self.nodes.len();
        let root = offset + projection.root;
        self.nodes
            .extend(projection.nodes.iter().map(|node| shift_node(node, offset)));
        Ok(root)
    }

    /// Validate and compact the node graph rooted at `root` into a projection.
    pub fn finish(self, root: usize) -> Result<Utf16JsonProjection, Utf16JsonProjectionError> {
        compact_nodes(&self.nodes, root)
    }

    fn push(&mut self, node: JsonNode) -> usize {
        let id = self.nodes.len();
        self.nodes.push(node);
        id
    }

    fn validate_children(&self, children: &[usize]) -> Result<(), Utf16JsonProjectionError> {
        if children.iter().any(|id| *id >= self.nodes.len()) {
            return Err(Utf16JsonProjectionError::InvalidProjection(
                "builder child must refer to an earlier node",
            ));
        }
        Ok(())
    }

    fn allocate_placeholder(&mut self, entries: &[JsonProperty]) -> String {
        loop {
            self.next_placeholder = self.next_placeholder.saturating_add(1);
            let candidate = format!("__lingxiUtf16KeyProjectionV2_{}__", self.next_placeholder);
            if entries.iter().all(|entry| entry.display != candidate) {
                return candidate;
            }
        }
    }
}

#[derive(Clone, Copy)]
enum ContainerPhase {
    Start,
    NeedValue,
    NeedKey,
    Separator,
}

enum ParserFrame {
    Array {
        items: Vec<usize>,
        phase: ContainerPhase,
    },
    Object {
        entries: Vec<JsonProperty>,
        pending_key: Option<(Vec<u16>, Option<String>)>,
        phase: ContainerPhase,
    },
}

enum ParserAction {
    Continue,
    Value,
    ObjectKey,
    CloseArray,
    CloseObject,
}

struct FlatJsonParser<'a> {
    input: &'a str,
    offset: usize,
    nodes: Vec<JsonNode>,
    next_placeholder: u64,
}

impl<'a> FlatJsonParser<'a> {
    fn new(input: &'a str) -> Self {
        Self {
            input,
            offset: 0,
            nodes: Vec::new(),
            next_placeholder: 0,
        }
    }

    fn parse(mut self) -> Result<Utf16JsonProjection, Utf16JsonProjectionError> {
        let mut frames = Vec::<ParserFrame>::new();
        let mut completed = None;
        self.start_value(&mut frames, &mut completed)?;

        loop {
            if let Some(id) = completed.take() {
                match frames.last_mut() {
                    Some(ParserFrame::Array { items, phase }) => {
                        items.push(id);
                        *phase = ContainerPhase::Separator;
                    }
                    Some(ParserFrame::Object {
                        entries,
                        pending_key,
                        phase,
                    }) => {
                        let (code_units, display) =
                            pending_key.take().expect("object value has a pending key");
                        if let Some(existing) = entries
                            .iter_mut()
                            .find(|entry| entry.code_units == code_units)
                        {
                            existing.value = id;
                        } else {
                            let display = match display {
                                Some(display) => display,
                                None => self.allocate_placeholder(entries, None),
                            };
                            entries.push(JsonProperty {
                                display,
                                code_units,
                                value: id,
                            });
                        }
                        *phase = ContainerPhase::Separator;
                    }
                    None => {
                        self.whitespace();
                        if self.offset != self.input.len() {
                            return Err(self.error("trailing characters"));
                        }
                        return compact_nodes(&self.nodes, id);
                    }
                }
                continue;
            }

            let action = match frames.last_mut() {
                Some(ParserFrame::Array { phase, .. }) => match phase {
                    ContainerPhase::Start => {
                        if self.take(b']') {
                            ParserAction::CloseArray
                        } else {
                            *phase = ContainerPhase::NeedValue;
                            ParserAction::Value
                        }
                    }
                    ContainerPhase::NeedValue => ParserAction::Value,
                    ContainerPhase::Separator => {
                        if self.take(b']') {
                            ParserAction::CloseArray
                        } else if self.take(b',') {
                            *phase = ContainerPhase::NeedValue;
                            ParserAction::Continue
                        } else {
                            return Err(self.error("expected array comma"));
                        }
                    }
                    ContainerPhase::NeedKey => {
                        return Err(self.error("invalid array parser state"));
                    }
                },
                Some(ParserFrame::Object {
                    phase, pending_key, ..
                }) => match phase {
                    ContainerPhase::Start => {
                        if self.take(b'}') {
                            ParserAction::CloseObject
                        } else {
                            ParserAction::ObjectKey
                        }
                    }
                    ContainerPhase::NeedKey => ParserAction::ObjectKey,
                    ContainerPhase::NeedValue => {
                        if pending_key.is_none() {
                            return Err(self.error("object value has no key"));
                        }
                        ParserAction::Value
                    }
                    ContainerPhase::Separator => {
                        if self.take(b'}') {
                            ParserAction::CloseObject
                        } else if self.take(b',') {
                            *phase = ContainerPhase::NeedKey;
                            ParserAction::Continue
                        } else {
                            return Err(self.error("expected object comma"));
                        }
                    }
                },
                None => return Err(self.error("expected JSON value")),
            };

            match action {
                ParserAction::Continue => {}
                ParserAction::Value => self.start_value(&mut frames, &mut completed)?,
                ParserAction::ObjectKey => {
                    let Some(ParserFrame::Object {
                        entries,
                        pending_key,
                        phase,
                    }) = frames.last_mut()
                    else {
                        return Err(self.error("invalid object parser state"));
                    };
                    self.object_member(entries, pending_key, phase)?;
                }
                ParserAction::CloseArray => {
                    let Some(ParserFrame::Array { items, .. }) = frames.pop() else {
                        return Err(self.error("invalid array parser state"));
                    };
                    completed = Some(self.push(JsonNode::Array(items)));
                }
                ParserAction::CloseObject => {
                    let Some(ParserFrame::Object { entries, .. }) = frames.pop() else {
                        return Err(self.error("invalid object parser state"));
                    };
                    completed = Some(self.push(JsonNode::Object(entries)));
                }
            }
        }
    }

    fn start_value(
        &mut self,
        frames: &mut Vec<ParserFrame>,
        completed: &mut Option<usize>,
    ) -> Result<(), Utf16JsonProjectionError> {
        self.whitespace();
        match self.input.as_bytes().get(self.offset).copied() {
            Some(b'"') => {
                let code_units = self.string()?;
                let display = String::from_utf16_lossy(&code_units);
                *completed = Some(self.push(JsonNode::String {
                    display,
                    code_units,
                }));
            }
            Some(b'[') => {
                self.offset += 1;
                frames.push(ParserFrame::Array {
                    items: Vec::new(),
                    phase: ContainerPhase::Start,
                });
            }
            Some(b'{') => {
                self.offset += 1;
                frames.push(ParserFrame::Object {
                    entries: Vec::new(),
                    pending_key: None,
                    phase: ContainerPhase::Start,
                });
            }
            Some(_) => {
                let token = self.scalar_token();
                let node = match token.as_str() {
                    "null" => JsonNode::Null,
                    "true" => JsonNode::Bool(true),
                    "false" => JsonNode::Bool(false),
                    number if parse_json_number(&number).is_some() => JsonNode::Number(
                        parse_json_number(&number)
                            .expect("validated number")
                            .to_bits(),
                    ),
                    _ => return Err(self.error("invalid JSON scalar")),
                };
                *completed = Some(self.push(node));
            }
            None => return Err(self.error("expected JSON value")),
        }
        Ok(())
    }

    fn object_member(
        &mut self,
        entries: &mut Vec<JsonProperty>,
        pending_key: &mut Option<(Vec<u16>, Option<String>)>,
        phase: &mut ContainerPhase,
    ) -> Result<(), Utf16JsonProjectionError> {
        self.whitespace();
        let code_units = self.string()?;
        let display = if let Ok(display) = String::from_utf16(&code_units) {
            if let Some(index) = entries
                .iter()
                .position(|entry| entry.display == display && entry.code_units != code_units)
            {
                let replacement = self.key_placeholder(entries, Some(&display));
                entries[index].display = replacement;
            }
            Some(display)
        } else {
            None
        };
        if !self.take(b':') {
            return Err(self.error("expected object colon"));
        }
        *pending_key = Some((code_units, display));
        *phase = ContainerPhase::NeedValue;
        Ok(())
    }

    fn scalar_token(&mut self) -> String {
        let start = self.offset;
        while self
            .input
            .as_bytes()
            .get(self.offset)
            .is_some_and(|byte| !matches!(byte, b' ' | b'\n' | b'\r' | b'\t' | b',' | b']' | b'}'))
        {
            self.offset += 1;
        }
        self.input[start..self.offset].to_owned()
    }

    fn string(&mut self) -> Result<Vec<u16>, Utf16JsonProjectionError> {
        if !self.take(b'"') {
            return Err(self.error("expected JSON string"));
        }
        let mut units = Vec::new();
        loop {
            match self.input.as_bytes().get(self.offset).copied() {
                Some(b'"') => {
                    self.offset += 1;
                    return Ok(units);
                }
                Some(b'\\') => {
                    self.offset += 1;
                    let escaped = self.input.as_bytes().get(self.offset).copied();
                    self.offset += usize::from(escaped.is_some());
                    units.push(match escaped {
                        Some(b'"') => u16::from(b'"'),
                        Some(b'\\') => u16::from(b'\\'),
                        Some(b'/') => u16::from(b'/'),
                        Some(b'b') => 8,
                        Some(b'f') => 12,
                        Some(b'n') => 10,
                        Some(b'r') => 13,
                        Some(b't') => 9,
                        Some(b'u') => {
                            let mut unit = 0_u16;
                            for _ in 0..4 {
                                let digit = self
                                    .input
                                    .as_bytes()
                                    .get(self.offset)
                                    .and_then(|byte| char::from(*byte).to_digit(16))
                                    .ok_or_else(|| self.error("invalid unicode escape"))?;
                                unit = (unit << 4) | u16::try_from(digit).expect("hex digit");
                                self.offset += 1;
                            }
                            unit
                        }
                        _ => return Err(self.error("invalid string escape")),
                    });
                }
                Some(0..=0x1f) => return Err(self.error("control character in string")),
                Some(_) => {
                    let ch = self.input[self.offset..]
                        .chars()
                        .next()
                        .expect("nonempty suffix");
                    self.offset += ch.len_utf8();
                    let mut pair = [0; 2];
                    units.extend_from_slice(ch.encode_utf16(&mut pair));
                }
                None => return Err(self.error("unterminated JSON string")),
            }
        }
    }

    fn key_placeholder(&mut self, entries: &[JsonProperty], forbidden: Option<&str>) -> String {
        loop {
            self.next_placeholder = self.next_placeholder.saturating_add(1);
            let candidate = format!("__lingxiUtf16KeyProjectionV2_{}__", self.next_placeholder);
            if forbidden != Some(candidate.as_str())
                && entries.iter().all(|entry| entry.display != candidate)
            {
                return candidate;
            }
        }
    }

    fn allocate_placeholder(
        &mut self,
        entries: &[JsonProperty],
        forbidden: Option<&str>,
    ) -> String {
        self.key_placeholder(entries, forbidden)
    }

    fn error(&self, message: &'static str) -> Utf16JsonProjectionError {
        Utf16JsonProjectionError::InvalidJson {
            offset: self.offset,
            message,
        }
    }

    fn whitespace(&mut self) {
        while self
            .input
            .as_bytes()
            .get(self.offset)
            .is_some_and(|byte| matches!(byte, b' ' | b'\n' | b'\r' | b'\t'))
        {
            self.offset += 1;
        }
    }

    fn take(&mut self, expected: u8) -> bool {
        self.whitespace();
        if self.input.as_bytes().get(self.offset) == Some(&expected) {
            self.offset += 1;
            true
        } else {
            false
        }
    }

    fn push(&mut self, node: JsonNode) -> usize {
        let id = self.nodes.len();
        self.nodes.push(node);
        id
    }
}

fn parse_json_number(source: &str) -> Option<f64> {
    let bytes = source.as_bytes();
    let mut index = 0;
    if bytes.get(index) == Some(&b'-') {
        index += 1;
    }
    match bytes.get(index).copied()? {
        b'0' => index += 1,
        b'1'..=b'9' => {
            index += 1;
            while bytes.get(index).is_some_and(u8::is_ascii_digit) {
                index += 1;
            }
        }
        _ => return None,
    }
    if bytes.get(index) == Some(&b'.') {
        index += 1;
        let fraction_start = index;
        while bytes.get(index).is_some_and(u8::is_ascii_digit) {
            index += 1;
        }
        if index == fraction_start {
            return None;
        }
    }
    if matches!(bytes.get(index), Some(b'e' | b'E')) {
        index += 1;
        if matches!(bytes.get(index), Some(b'+' | b'-')) {
            index += 1;
        }
        let exponent_start = index;
        while bytes.get(index).is_some_and(u8::is_ascii_digit) {
            index += 1;
        }
        if index == exponent_start {
            return None;
        }
    }
    (index == bytes.len())
        .then(|| source.parse::<f64>().ok())
        .flatten()
}

fn shift_node(node: &JsonNode, offset: usize) -> JsonNode {
    match node {
        JsonNode::Null => JsonNode::Null,
        JsonNode::Bool(value) => JsonNode::Bool(*value),
        JsonNode::Number(bits) => JsonNode::Number(*bits),
        JsonNode::String {
            display,
            code_units,
        } => JsonNode::String {
            display: display.clone(),
            code_units: code_units.clone(),
        },
        JsonNode::Array(items) => JsonNode::Array(items.iter().map(|id| id + offset).collect()),
        JsonNode::Object(entries) => JsonNode::Object(
            entries
                .iter()
                .map(|entry| JsonProperty {
                    display: entry.display.clone(),
                    code_units: entry.code_units.clone(),
                    value: entry.value + offset,
                })
                .collect(),
        ),
    }
}

fn compact_nodes(
    nodes: &[JsonNode],
    root: usize,
) -> Result<Utf16JsonProjection, Utf16JsonProjectionError> {
    if root >= nodes.len() {
        return Err(Utf16JsonProjectionError::InvalidProjection(
            "root node is outside the arena",
        ));
    }
    let mut visited = vec![false; nodes.len()];
    let mut postorder = Vec::<usize>::new();
    let mut pending = vec![(root, false)];
    while let Some((id, expanded)) = pending.pop() {
        if id >= nodes.len() {
            return Err(Utf16JsonProjectionError::InvalidProjection(
                "child node is outside the arena",
            ));
        }
        if expanded {
            postorder.push(id);
            continue;
        }
        if std::mem::replace(&mut visited[id], true) {
            return Err(Utf16JsonProjectionError::InvalidProjection(
                "flat JSON graph contains a cycle or shared child",
            ));
        }
        pending.push((id, true));
        match &nodes[id] {
            JsonNode::Array(items) => {
                pending.extend(items.iter().rev().map(|child| (*child, false)));
            }
            JsonNode::Object(entries) => {
                pending.extend(entries.iter().rev().map(|entry| (entry.value, false)));
            }
            JsonNode::Null | JsonNode::Bool(_) | JsonNode::Number(_) | JsonNode::String { .. } => {}
        }
    }

    let mut remap = vec![usize::MAX; nodes.len()];
    let mut compacted = Vec::with_capacity(postorder.len());
    for old_id in postorder {
        let new_id = compacted.len();
        remap[old_id] = new_id;
        let node = match &nodes[old_id] {
            JsonNode::Null => JsonNode::Null,
            JsonNode::Bool(value) => JsonNode::Bool(*value),
            JsonNode::Number(bits) => JsonNode::Number(*bits),
            JsonNode::String {
                display,
                code_units,
            } => JsonNode::String {
                display: display.clone(),
                code_units: code_units.clone(),
            },
            JsonNode::Array(items) => JsonNode::Array(items.iter().map(|id| remap[*id]).collect()),
            JsonNode::Object(entries) => JsonNode::Object(
                entries
                    .iter()
                    .map(|entry| JsonProperty {
                        display: entry.display.clone(),
                        code_units: entry.code_units.clone(),
                        value: remap[entry.value],
                    })
                    .collect(),
            ),
        };
        compacted.push(node);
    }
    let value = display_value(&compacted, remap[root])?;
    let projection = Utf16JsonProjection {
        root: remap[root],
        nodes: compacted,
        value,
    };
    projection.validate()?;
    Ok(projection)
}

fn display_value(nodes: &[JsonNode], root: usize) -> Result<Value, Utf16JsonProjectionError> {
    enum Task {
        Visit(usize),
        Array(usize),
        Object(Vec<String>),
    }

    if root >= nodes.len() {
        return Err(Utf16JsonProjectionError::InvalidProjection(
            "root node is outside the arena",
        ));
    }
    let mut pending = vec![Task::Visit(root)];
    let mut completed = Vec::<Value>::new();
    while let Some(task) = pending.pop() {
        match task {
            Task::Visit(id) => {
                match nodes
                    .get(id)
                    .ok_or(Utf16JsonProjectionError::InvalidProjection(
                        "child node is outside the arena",
                    ))? {
                    JsonNode::Null => completed.push(Value::Null),
                    JsonNode::Bool(value) => completed.push(Value::Bool(*value)),
                    JsonNode::Number(bits) => {
                        let number = f64::from_bits(*bits);
                        completed.push(
                            serde_json::Number::from_f64(number).map_or(Value::Null, Value::Number),
                        );
                    }
                    JsonNode::String { display, .. } => {
                        completed.push(Value::String(display.clone()));
                    }
                    JsonNode::Array(items) => {
                        pending.push(Task::Array(items.len()));
                        pending.extend(items.iter().rev().copied().map(Task::Visit));
                    }
                    JsonNode::Object(entries) => {
                        pending.push(Task::Object(
                            entries.iter().map(|entry| entry.display.clone()).collect(),
                        ));
                        pending.extend(entries.iter().rev().map(|entry| Task::Visit(entry.value)));
                    }
                }
            }
            Task::Array(length) => {
                if completed.len() < length {
                    return Err(Utf16JsonProjectionError::InvalidProjection(
                        "display array stack underflow",
                    ));
                }
                let items = completed.split_off(completed.len() - length);
                completed.push(Value::Array(items));
            }
            Task::Object(keys) => {
                if completed.len() < keys.len() {
                    return Err(Utf16JsonProjectionError::InvalidProjection(
                        "display object stack underflow",
                    ));
                }
                let children = completed.split_off(completed.len() - keys.len());
                let mut object = Map::new();
                for (key, child) in keys.into_iter().zip(children) {
                    object.insert(key, child);
                }
                completed.push(Value::Object(object));
            }
        }
    }
    if completed.len() != 1 {
        return Err(Utf16JsonProjectionError::InvalidProjection(
            "display projection did not produce one root value",
        ));
    }
    Ok(completed.pop().expect("one completed root value"))
}

fn join_pointer(parent: &str, token: &str) -> String {
    format!("{parent}/{}", token.replace('~', "~0").replace('/', "~1"))
}

fn value_at_pointer(nodes: &[JsonNode], root: usize, pointer: &str) -> Option<usize> {
    if pointer.is_empty() {
        return Some(root);
    }
    let rest = pointer.strip_prefix('/')?;
    let mut id = root;
    for raw in rest.split('/') {
        let token = decode_pointer_token(raw)?;
        id = match &nodes[id] {
            JsonNode::Object(entries) => entries
                .iter()
                .find(|entry| entry.display == token)
                .map(|entry| entry.value)?,
            JsonNode::Array(items) => {
                let index = token.parse::<usize>().ok()?;
                if index.to_string() != token {
                    return None;
                }
                *items.get(index)?
            }
            JsonNode::Null | JsonNode::Bool(_) | JsonNode::Number(_) | JsonNode::String { .. } => {
                return None;
            }
        };
    }
    Some(id)
}

fn value_at_pointer_units(nodes: &[JsonNode], root: usize, pointer: &[u16]) -> Option<usize> {
    if pointer.is_empty() {
        return Some(root);
    }
    if pointer.first() != Some(&u16::from(b'/')) {
        return None;
    }
    let mut id = root;
    for raw in pointer[1..].split(|unit| *unit == u16::from(b'/')) {
        let token = decode_pointer_token_units(raw)?;
        id = match &nodes[id] {
            JsonNode::Object(entries) => entries
                .iter()
                .find(|entry| entry.code_units == token)
                .map(|entry| entry.value)?,
            JsonNode::Array(items) => *items.get(parse_pointer_index(&token)?)?,
            JsonNode::Null | JsonNode::Bool(_) | JsonNode::Number(_) | JsonNode::String { .. } => {
                return None;
            }
        };
    }
    Some(id)
}

fn decode_pointer_token(raw: &str) -> Option<String> {
    let mut result = String::with_capacity(raw.len());
    let mut chars = raw.chars();
    while let Some(ch) = chars.next() {
        if ch != '~' {
            result.push(ch);
            continue;
        }
        result.push(match chars.next()? {
            '0' => '~',
            '1' => '/',
            _ => return None,
        });
    }
    Some(result)
}

fn decode_pointer_token_units(raw: &[u16]) -> Option<Vec<u16>> {
    let mut result = Vec::with_capacity(raw.len());
    let mut index = 0;
    while index < raw.len() {
        if raw[index] != u16::from(b'~') {
            result.push(raw[index]);
            index += 1;
            continue;
        }
        index += 1;
        result.push(match *raw.get(index)? {
            unit if unit == u16::from(b'0') => u16::from(b'~'),
            unit if unit == u16::from(b'1') => u16::from(b'/'),
            _ => return None,
        });
        index += 1;
    }
    Some(result)
}

fn parse_pointer_index(token: &[u16]) -> Option<usize> {
    let text = String::from_utf16(token).ok()?;
    let index = text.parse::<usize>().ok()?;
    (index.to_string() == text).then_some(index)
}

fn fresh_placeholder(entries: &[JsonProperty], forbidden: &str, prefix: &str) -> String {
    let mut index = 1_u64;
    loop {
        let candidate = format!("{prefix}{index}__");
        if candidate != forbidden && entries.iter().all(|entry| entry.display != candidate) {
            return candidate;
        }
        index = index.saturating_add(1);
    }
}

fn javascript_array_index(key_units: &[u16]) -> Option<u32> {
    let key = String::from_utf16(key_units).ok()?;
    let index = key.parse::<u32>().ok()?;
    (index != u32::MAX && index.to_string() == key).then_some(index)
}

fn javascript_ordered_object_indices(entries: &[JsonProperty]) -> Vec<usize> {
    let mut integer_keys = Vec::<(u32, usize)>::new();
    let mut other_keys = Vec::<usize>::new();
    for (position, entry) in entries.iter().enumerate() {
        if let Some(index) = javascript_array_index(&entry.code_units) {
            integer_keys.push((index, position));
        } else {
            other_keys.push(position);
        }
    }
    integer_keys.sort_by_key(|(index, _)| *index);
    integer_keys
        .into_iter()
        .map(|(_, position)| position)
        .chain(other_keys)
        .collect()
}

fn write_json(
    output: &mut Vec<u8>,
    nodes: &[JsonNode],
    root: usize,
) -> Result<(), Utf16JsonProjectionError> {
    enum Task<'a> {
        Value(usize),
        ObjectKey(&'a [u16]),
        Byte(u8),
    }

    let mut pending = vec![Task::Value(root)];
    while let Some(task) = pending.pop() {
        match task {
            Task::Byte(byte) => output.push(byte),
            Task::ObjectKey(units) => write_units(output, units),
            Task::Value(id) => match &nodes[id] {
                JsonNode::Null => output.extend_from_slice(b"null"),
                JsonNode::Bool(true) => output.extend_from_slice(b"true"),
                JsonNode::Bool(false) => output.extend_from_slice(b"false"),
                JsonNode::Number(bits) => {
                    let number = f64::from_bits(*bits);
                    if !number.is_finite() {
                        output.extend_from_slice(b"null");
                    } else if number == 0.0 {
                        output.push(b'0');
                    } else {
                        let mut buffer = ryu_js::Buffer::new();
                        output.extend_from_slice(buffer.format_finite(number).as_bytes());
                    }
                }
                JsonNode::String { code_units, .. } => write_units(output, code_units),
                JsonNode::Array(items) => {
                    output.push(b'[');
                    pending.push(Task::Byte(b']'));
                    for (index, child) in items.iter().enumerate().rev() {
                        pending.push(Task::Value(*child));
                        if index > 0 {
                            pending.push(Task::Byte(b','));
                        }
                    }
                }
                JsonNode::Object(entries) => {
                    output.push(b'{');
                    pending.push(Task::Byte(b'}'));
                    for (index, entry_index) in javascript_ordered_object_indices(entries)
                        .into_iter()
                        .enumerate()
                        .rev()
                    {
                        let entry = &entries[entry_index];
                        pending.push(Task::Value(entry.value));
                        pending.push(Task::Byte(b':'));
                        pending.push(Task::ObjectKey(&entry.code_units));
                        if index > 0 {
                            pending.push(Task::Byte(b','));
                        }
                    }
                }
            },
        }
    }
    Ok(())
}

fn write_pretty_json(
    output: &mut Vec<u8>,
    nodes: &[JsonNode],
    root: usize,
) -> Result<(), Utf16JsonProjectionError> {
    enum Task<'a> {
        Value(usize, usize),
        ObjectKey(&'a [u16]),
        Bytes(&'static [u8]),
        Indent(usize),
    }

    let mut pending = vec![Task::Value(root, 0)];
    while let Some(task) = pending.pop() {
        match task {
            Task::Bytes(bytes) => output.extend_from_slice(bytes),
            Task::Indent(depth) => output.extend(std::iter::repeat_n(b' ', depth * 2)),
            Task::ObjectKey(units) => write_units(output, units),
            Task::Value(id, depth) => match &nodes[id] {
                JsonNode::Null => output.extend_from_slice(b"null"),
                JsonNode::Bool(true) => output.extend_from_slice(b"true"),
                JsonNode::Bool(false) => output.extend_from_slice(b"false"),
                JsonNode::Number(bits) => {
                    let number = f64::from_bits(*bits);
                    if !number.is_finite() {
                        output.extend_from_slice(b"null");
                    } else if number == 0.0 {
                        output.push(b'0');
                    } else {
                        let mut buffer = ryu_js::Buffer::new();
                        output.extend_from_slice(buffer.format_finite(number).as_bytes());
                    }
                }
                JsonNode::String { code_units, .. } => write_units(output, code_units),
                JsonNode::Array(items) if items.is_empty() => output.extend_from_slice(b"[]"),
                JsonNode::Array(items) => {
                    output.push(b'[');
                    let mut sequence = Vec::with_capacity(items.len() * 4 + 2);
                    for (index, child) in items.iter().enumerate() {
                        if index > 0 {
                            sequence.push(Task::Bytes(b","));
                        }
                        sequence.push(Task::Bytes(b"\n"));
                        sequence.push(Task::Indent(depth + 1));
                        sequence.push(Task::Value(*child, depth + 1));
                    }
                    sequence.push(Task::Bytes(b"\n"));
                    sequence.push(Task::Indent(depth));
                    sequence.push(Task::Bytes(b"]"));
                    pending.extend(sequence.into_iter().rev());
                }
                JsonNode::Object(entries) if entries.is_empty() => output.extend_from_slice(b"{}"),
                JsonNode::Object(entries) => {
                    output.push(b'{');
                    let order = javascript_ordered_object_indices(entries);
                    let mut sequence = Vec::with_capacity(order.len() * 6 + 2);
                    for (index, entry_index) in order.into_iter().enumerate() {
                        if index > 0 {
                            sequence.push(Task::Bytes(b","));
                        }
                        let entry = &entries[entry_index];
                        sequence.push(Task::Bytes(b"\n"));
                        sequence.push(Task::Indent(depth + 1));
                        sequence.push(Task::ObjectKey(&entry.code_units));
                        sequence.push(Task::Bytes(b": "));
                        sequence.push(Task::Value(entry.value, depth + 1));
                    }
                    sequence.push(Task::Bytes(b"\n"));
                    sequence.push(Task::Indent(depth));
                    sequence.push(Task::Bytes(b"}"));
                    pending.extend(sequence.into_iter().rev());
                }
            },
        }
    }
    Ok(())
}

fn write_units(output: &mut Vec<u8>, units: &[u16]) {
    output.push(b'"');
    for decoded in char::decode_utf16(units.iter().copied()) {
        match decoded {
            Ok(ch) => match ch {
                '"' => output.extend_from_slice(br#"\""#),
                '\\' => output.extend_from_slice(br"\\"),
                '\u{08}' => output.extend_from_slice(br"\b"),
                '\u{0c}' => output.extend_from_slice(br"\f"),
                '\n' => output.extend_from_slice(br"\n"),
                '\r' => output.extend_from_slice(br"\r"),
                '\t' => output.extend_from_slice(br"\t"),
                ch if ch <= '\u{1f}' => {
                    write!(output, "\\u{:04x}", u32::from(ch)).expect("Vec writes cannot fail");
                }
                _ => {
                    let mut buffer = [0; 4];
                    output.extend_from_slice(ch.encode_utf8(&mut buffer).as_bytes());
                }
            },
            Err(error) => {
                write!(output, "\\u{:04x}", error.unpaired_surrogate())
                    .expect("Vec writes cannot fail");
            }
        }
    }
    output.push(b'"');
}

/// A collision-free, reversible `serde_json::Value` namespace for validators
/// and typed consumers that cannot inspect the flat source arena directly.
///
/// Unpaired UTF-16 units are represented by private-use characters only in the
/// temporary values returned by [`Self::values`]. Restore those values through
/// this owner before returning them to a source-bearing boundary.
pub struct Utf16JsonConsumer {
    values: Vec<Value>,
    units: HashMap<u16, u16>,
}

impl Utf16JsonConsumer {
    /// Build one namespace shared by all inputs so cross-field identity checks
    /// continue to distinguish different unpaired units from the same unit.
    pub fn new(inputs: &[&Utf16JsonProjection]) -> Result<Self, Utf16JsonProjectionError> {
        let mut occupied = HashSet::new();
        let mut unpaired = BTreeSet::new();
        for input in inputs {
            input.validate()?;
            let mut pending = vec![input.root];
            while let Some(id) = pending.pop() {
                match &input.nodes[id] {
                    JsonNode::String { code_units, .. } => {
                        collect_consumer_units(code_units, &mut occupied, &mut unpaired);
                    }
                    JsonNode::Array(items) => pending.extend(items.iter().copied()),
                    JsonNode::Object(entries) => {
                        for entry in entries {
                            collect_consumer_units(&entry.code_units, &mut occupied, &mut unpaired);
                            pending.push(entry.value);
                        }
                    }
                    JsonNode::Null | JsonNode::Bool(_) | JsonNode::Number(_) => {}
                }
            }
        }

        let mut available = (0xe000_u16..=0xf8ff).filter(|unit| !occupied.contains(unit));
        let mut units = HashMap::new();
        for unit in unpaired {
            let private = available
                .next()
                .ok_or(Utf16JsonProjectionError::InvalidProjection(
                    "consumer UTF-16 namespace exhausted",
                ))?;
            units.insert(unit, private);
        }

        let values = inputs
            .iter()
            .map(|input| consumer_value(input, &units))
            .collect::<Result<Vec<_>, _>>()?;
        Ok(Self { values, units })
    }

    /// Read consumer values in the same order as the admitted source inputs.
    pub fn values(&self) -> &[Value] {
        &self.values
    }

    /// Restore a transformed value to the canonical flat source carrier.
    pub fn restore(&self, value: &Value) -> Result<Utf16JsonProjection, Utf16JsonProjectionError> {
        let reverse = self
            .units
            .iter()
            .map(|(original, private)| (*private, *original))
            .collect::<HashMap<_, _>>();
        let mut builder = Utf16JsonBuilder::new();
        enum Task<'a> {
            Visit(&'a Value),
            Array(usize),
            Object(Vec<Vec<u16>>),
        }
        let mut tasks = vec![Task::Visit(value)];
        let mut complete = Vec::new();
        while let Some(task) = tasks.pop() {
            match task {
                Task::Visit(Value::Array(items)) => {
                    tasks.push(Task::Array(items.len()));
                    tasks.extend(items.iter().rev().map(Task::Visit));
                }
                Task::Visit(Value::Object(object)) => {
                    let entries = object.iter().collect::<Vec<_>>();
                    tasks.push(Task::Object(
                        entries
                            .iter()
                            .map(|(key, _)| restore_consumer_string(key, &reverse))
                            .collect(),
                    ));
                    tasks.extend(entries.iter().rev().map(|(_, child)| Task::Visit(child)));
                }
                Task::Visit(Value::String(text)) => {
                    complete
                        .push(builder.push_string_units(restore_consumer_string(text, &reverse)));
                }
                Task::Visit(Value::Null) => complete.push(builder.push_null()),
                Task::Visit(Value::Bool(value)) => complete.push(builder.push_bool(*value)),
                Task::Visit(Value::Number(value)) => {
                    let number =
                        value
                            .as_f64()
                            .ok_or(Utf16JsonProjectionError::InvalidProjection(
                                "consumer number is not a JavaScript number",
                            ))?;
                    complete.push(builder.push_number(number));
                }
                Task::Array(length) => {
                    let start = complete.len().checked_sub(length).ok_or(
                        Utf16JsonProjectionError::InvalidProjection(
                            "consumer array stack underflow",
                        ),
                    )?;
                    let children = complete.split_off(start);
                    complete.push(builder.push_array(children)?);
                }
                Task::Object(keys) => {
                    let start = complete.len().checked_sub(keys.len()).ok_or(
                        Utf16JsonProjectionError::InvalidProjection(
                            "consumer object stack underflow",
                        ),
                    )?;
                    let children = complete.split_off(start);
                    complete
                        .push(builder.push_object_units(keys.into_iter().zip(children).collect())?);
                }
            }
        }
        let root = complete
            .pop()
            .ok_or(Utf16JsonProjectionError::InvalidProjection(
                "consumer did not produce a root value",
            ))?;
        builder.finish(root)
    }
}

fn collect_consumer_units(
    source: &[u16],
    occupied: &mut HashSet<u16>,
    unpaired: &mut BTreeSet<u16>,
) {
    occupied.extend(
        source
            .iter()
            .copied()
            .filter(|unit| (0xe000..=0xf8ff).contains(unit)),
    );
    unpaired.extend(
        char::decode_utf16(source.iter().copied())
            .filter_map(Result::err)
            .map(|error| error.unpaired_surrogate()),
    );
}

fn encode_consumer_string(source: &[u16], units: &HashMap<u16, u16>) -> String {
    char::decode_utf16(source.iter().copied())
        .map(|item| match item {
            Ok(character) => character,
            Err(error) => char::from_u32(u32::from(units[&error.unpaired_surrogate()]))
                .expect("consumer namespace is a BMP private character"),
        })
        .collect()
}

fn restore_consumer_string(value: &str, units: &HashMap<u16, u16>) -> Vec<u16> {
    value
        .encode_utf16()
        .map(|unit| units.get(&unit).copied().unwrap_or(unit))
        .collect()
}

fn consumer_value(
    input: &Utf16JsonProjection,
    units: &HashMap<u16, u16>,
) -> Result<Value, Utf16JsonProjectionError> {
    enum Task {
        Visit(usize),
        Array(usize),
        Object(Vec<String>),
    }
    let mut tasks = vec![Task::Visit(input.root)];
    let mut complete = Vec::new();
    while let Some(task) = tasks.pop() {
        match task {
            Task::Visit(id) => match &input.nodes[id] {
                JsonNode::Array(items) => {
                    tasks.push(Task::Array(items.len()));
                    tasks.extend(items.iter().rev().copied().map(Task::Visit));
                }
                JsonNode::Object(entries) => {
                    let order = javascript_ordered_object_indices(entries);
                    let keys = order
                        .iter()
                        .map(|index| encode_consumer_string(&entries[*index].code_units, units))
                        .collect();
                    tasks.push(Task::Object(keys));
                    tasks.extend(
                        order
                            .iter()
                            .rev()
                            .map(|index| Task::Visit(entries[*index].value)),
                    );
                }
                JsonNode::String { code_units, .. } => {
                    complete.push(Value::String(encode_consumer_string(code_units, units)))
                }
                JsonNode::Null => complete.push(Value::Null),
                JsonNode::Bool(value) => complete.push(Value::Bool(*value)),
                JsonNode::Number(bits) => {
                    let number = f64::from_bits(*bits);
                    complete.push(
                        serde_json::Number::from_f64(number)
                            .map(Value::Number)
                            .unwrap_or(Value::Null),
                    );
                }
            },
            Task::Array(length) => {
                let start = complete.len().checked_sub(length).ok_or(
                    Utf16JsonProjectionError::InvalidProjection("consumer array stack underflow"),
                )?;
                let children = complete.split_off(start);
                complete.push(Value::Array(children));
            }
            Task::Object(keys) => {
                let start = complete.len().checked_sub(keys.len()).ok_or(
                    Utf16JsonProjectionError::InvalidProjection("consumer object stack underflow"),
                )?;
                let children = complete.split_off(start);
                let object = keys.into_iter().zip(children).collect();
                complete.push(Value::Object(object));
            }
        }
    }
    complete
        .pop()
        .ok_or(Utf16JsonProjectionError::InvalidProjection(
            "consumer did not produce a root value",
        ))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn pretty_stringifier_matches_js_layout_and_keeps_deep_walk_iterative() {
        let value =
            Utf16JsonProjection::parse(r#"{"2":"two","1":"one","x":[true,{"bad":"\ud800"}]}"#)
                .unwrap();
        assert_eq!(
            value.to_pretty_json_string().unwrap(),
            "{\n  \"1\": \"one\",\n  \"2\": \"two\",\n  \"x\": [\n    true,\n    {\n      \"bad\": \"\\ud800\"\n    }\n  ]\n}"
        );

        let depth = 2_000;
        let source = format!("{}0{}", "[".repeat(depth), "]".repeat(depth));
        let deep = Utf16JsonProjection::parse(&source).unwrap();
        let pretty = deep.to_pretty_json_string().unwrap();
        assert!(pretty.starts_with("[\n  [\n    ["));
        assert!(pretty.contains(&format!("\n{}]", "  ".repeat(depth - 1))));
        assert!(pretty.ends_with("\n]"));
    }

    #[test]
    fn exact_utf16_strings_and_keys_round_trip_without_recursive_values() {
        let source = r#"{"client":"\ud800","props":{"\ud800":"\ud800","�":"visible"}}"#;
        let projection = Utf16JsonProjection::parse(source).unwrap();
        projection.validate().unwrap();
        assert_eq!(projection.to_json_string().unwrap(), source);
        assert_eq!(projection.string_units("/client"), Some(vec![0xd800]));
        let props = projection.at_pointer("/props").unwrap();
        let exact = props
            .object_entries()
            .unwrap()
            .into_iter()
            .find(|entry| entry.code_units == [0xd800])
            .unwrap();
        assert_eq!(exact.value.code_units(), Some([0xd800].as_slice()));
        assert_eq!(
            projection.key_units("/props", exact.display_key),
            vec![0xd800]
        );
    }

    #[test]
    fn pointer_replacement_keeps_flat_exact_subtrees_and_replaces_root() {
        let mut projection =
            Utf16JsonProjection::parse(r#"{"\ud800":"kept","items":[1,2]}"#).unwrap();
        let replacement = Utf16JsonProjection::root_string("�".into(), vec![0xdc00]).unwrap();
        projection
            .replace_pointer_projection("/items/0", replacement)
            .unwrap();
        assert_eq!(
            projection.to_json_string().unwrap(),
            r#"{"\ud800":"kept","items":["\udc00",2]}"#
        );
        assert_eq!(projection.string_units("/items/0"), Some(vec![0xdc00]));

        let root = Utf16JsonProjection::parse(r#"{"nested":[true]}"#).unwrap();
        projection
            .replace_pointer_projection("", root.clone())
            .unwrap();
        assert!(projection.semantically_eq(&root));
        assert!(matches!(
            projection.replace_pointer_projection("/nested/01", root),
            Err(Utf16JsonProjectionError::InvalidPointer)
        ));
    }

    #[test]
    fn pointer_replacement_can_address_an_unpaired_utf16_property_name() {
        let mut projection = Utf16JsonProjection::parse(r#"{"\ud800":[0]}"#).unwrap();
        let pointer = [u16::from(b'/'), 0xd800, u16::from(b'/'), u16::from(b'0')];
        let replacement = Utf16JsonProjection::parse(r#"{"exact":"\udfff"}"#).unwrap();
        assert_eq!(
            projection
                .subprojection_units(&pointer)
                .unwrap()
                .to_json_string()
                .unwrap(),
            "0"
        );
        projection
            .replace_pointer_projection_units(&pointer, replacement)
            .unwrap();
        assert_eq!(
            projection.to_json_string().unwrap(),
            r#"{"\ud800":[{"exact":"\udfff"}]}"#
        );
    }

    #[test]
    fn root_object_projection_preserves_and_extracts_unpaired_property_names() {
        let value = Utf16JsonProjection::object_units(vec![(
            vec![0xd800],
            Utf16JsonProjection::root_string(
                "shown�".into(),
                vec![115, 104, 111, 119, 110, 0xd800],
            )
            .unwrap(),
        )])
        .unwrap();
        assert_eq!(
            value.to_json_string().unwrap(),
            r#"{"\ud800":"shown\ud800"}"#
        );
        let child = value.subprojection_property_units(&[0xd800]).unwrap();
        assert_eq!(
            child.string_units(""),
            Some(vec![115, 104, 111, 119, 110, 0xd800])
        );
        let entry = value.object_entries().unwrap().pop().unwrap();
        assert_eq!(entry.code_units, [0xd800]);
        assert_eq!(
            entry.value.code_units(),
            Some([115, 104, 111, 119, 110, 0xd800].as_slice())
        );
    }

    #[test]
    fn object_projection_merge_keeps_spread_order_and_exact_keys() {
        let mut target = Utf16JsonProjection::parse(r#"{"first":0,"replace":0}"#).unwrap();
        let source = Utf16JsonProjection::parse(r#"{"replace":1,"\ud800":"exact"}"#).unwrap();
        target.merge_object_projection(&source).unwrap();
        assert_eq!(
            target.to_json_string().unwrap(),
            r#"{"first":0,"replace":1,"\ud800":"exact"}"#
        );
    }

    #[test]
    fn duplicate_exact_keys_keep_the_last_value_and_original_order() {
        let source = r#"{"before":1,"\ud800":"first","after":2,"\ud800":"last"}"#;
        let projection = Utf16JsonProjection::parse(source).unwrap();
        assert_eq!(
            projection.to_json_string().unwrap(),
            r#"{"before":1,"\ud800":"last","after":2}"#
        );
        let entries = projection.at_pointer("").unwrap().object_entries().unwrap();
        assert_eq!(entries.len(), 3);
        let exact = entries
            .iter()
            .find(|entry| entry.code_units == [0xd800])
            .unwrap();
        assert_eq!(exact.value.as_str(), Some("last"));
    }

    #[test]
    fn safe_key_placeholders_survive_collisions_and_renaming() {
        let source = r#"{"\ud800":1,"__lingxiUtf16KeyProjectionV2_1__":2,"�":3}"#;
        let mut projection = Utf16JsonProjection::parse(source).unwrap();
        assert_eq!(projection.to_json_string().unwrap(), source);
        projection
            .rename_key_placeholders("__consumerKey_")
            .unwrap();
        assert_eq!(projection.to_json_string().unwrap(), source);
        let entries = projection.at_pointer("").unwrap().object_entries().unwrap();
        assert_eq!(
            entries
                .iter()
                .filter(|entry| entry.code_units == [0xd800])
                .count(),
            1
        );
        assert!(entries
            .iter()
            .any(|entry| entry.display_key.starts_with("__consumerKey_")));
    }

    #[test]
    fn flat_builder_renames_a_placeholder_when_a_real_key_uses_it() {
        let mut builder = Utf16JsonBuilder::new();
        let first = builder.push_number(1.0);
        let second = builder.push_number(2.0);
        let placeholder = "__lingxiUtf16KeyProjectionV2_1__";
        let root = builder
            .push_object_units(vec![
                (vec![0xd800], first),
                (placeholder.encode_utf16().collect(), second),
            ])
            .unwrap();
        let projection = builder.finish(root).unwrap();
        assert_eq!(
            projection.to_json_string().unwrap(),
            r#"{"\ud800":1,"__lingxiUtf16KeyProjectionV2_1__":2}"#
        );
        assert_eq!(
            projection.number_at(&format!("/{}", placeholder)),
            Some(2.0)
        );
        let entries = projection.at_pointer("").unwrap().object_entries().unwrap();
        assert_ne!(entries[0].display_key, placeholder);
        assert_eq!(entries[1].display_key, placeholder);
    }

    #[test]
    fn exact_user_strings_can_be_composed_into_flat_objects_and_arrays() {
        let user = Utf16JsonProjection::root_string(
            "hello�".into(),
            vec![
                u16::from(b'h'),
                u16::from(b'e'),
                u16::from(b'l'),
                u16::from(b'l'),
                u16::from(b'o'),
                0xd800,
            ],
        )
        .unwrap();
        let mut message =
            Utf16JsonProjection::from_serde_value(&serde_json::json!({"role":"user"})).unwrap();
        message.set_field("content", user).unwrap();
        let request = Utf16JsonProjection::object(vec![
            (
                "messages".into(),
                Utf16JsonProjection::array(vec![message]).unwrap(),
            ),
            (
                "max_tokens".into(),
                Utf16JsonProjection::from_serde_value(&serde_json::json!(8192)).unwrap(),
            ),
        ])
        .unwrap();
        assert_eq!(
            request.string_units("/messages/0/content"),
            Some(vec![104, 101, 108, 108, 111, 0xd800])
        );
        assert_eq!(
            request.to_json_string().unwrap(),
            r#"{"messages":[{"role":"user","content":"hello\ud800"}],"max_tokens":8192}"#
        );
    }

    #[test]
    fn numeric_parse_and_stringify_match_javascript_binary64() {
        for (source, expected) in [
            ("1e400", "null"),
            ("-0", "0"),
            ("-0.0", "0"),
            ("1e-4000", "0"),
            ("-1e-4000", "0"),
            ("5e-324", "5e-324"),
            ("1e-324", "0"),
            ("9007199254740993", "9007199254740992"),
            ("18446744073709551615", "18446744073709552000"),
        ] {
            assert_eq!(
                Utf16JsonProjection::parse(source)
                    .unwrap()
                    .to_json_string()
                    .unwrap(),
                expected,
                "{source}"
            );
        }
        assert!(Utf16JsonProjection::parse("NaN").is_err());
        assert!(Utf16JsonProjection::parse("1e+").is_err());
    }

    #[test]
    fn serde_json_import_is_explicit_and_rounds_to_javascript_number() {
        let value = serde_json::json!({"large": 9_007_199_254_740_993_u64, "text":"ok"});
        let projection = Utf16JsonProjection::from_serde_value(&value).unwrap();
        assert_eq!(
            projection.to_json_string().unwrap(),
            r#"{"large":9007199254740992,"text":"ok"}"#
        );
    }

    #[test]
    fn flat_serde_clone_and_drop_are_safe_for_deep_values() {
        let depth = 16_384usize;
        let source = format!("{}\"\\ud800\"{}", "[".repeat(depth), "]".repeat(depth));
        let projection = Utf16JsonProjection::parse(&source).unwrap();
        assert_eq!(projection.to_json_string().unwrap(), source);
        let cloned = projection.clone();
        assert_eq!(cloned.to_json_string().unwrap(), source);
        let encoded = serde_json::to_vec(&projection).unwrap();
        let decoded: Utf16JsonProjection = serde_json::from_slice(&encoded).unwrap();
        assert_eq!(decoded.to_json_string().unwrap(), source);
        drop(decoded);
        drop(cloned);
        drop(projection);
    }

    #[test]
    fn deep_objects_keep_exact_unpaired_keys_and_string_values() {
        let depth = 512usize;
        let source = format!(
            "{}\"\\ud800\"{}",
            r#"{"\ud800":"#.repeat(depth),
            "}".repeat(depth)
        );
        let projection = Utf16JsonProjection::parse(&source).unwrap();
        assert_eq!(projection.to_json_string().unwrap(), source);
        let renamed = {
            let mut clone = projection.clone();
            clone.rename_key_placeholders("__deep_").unwrap();
            clone.to_json_string().unwrap()
        };
        assert_eq!(renamed, source);
    }

    #[test]
    fn malformed_json_is_rejected_at_shallow_and_deep_depths() {
        for source in [
            r#"{"x":"\ud800",}"#,
            r#"{"x":"\ud800"}false"#,
            "[NaN]",
            "[01]",
            "[\"\\u12xz\"]",
        ] {
            assert!(Utf16JsonProjection::parse(source).is_err(), "{source}");
        }
        let malformed = format!("{}\"\\u12xz\"{}", "[".repeat(384), "]".repeat(384));
        assert!(Utf16JsonProjection::parse(&malformed).is_err());
    }

    #[test]
    fn serde_effect_dto_contains_only_flat_child_indices() {
        let projection = Utf16JsonProjection::parse(r#"{"text":"\ud800","number":1e400}"#).unwrap();
        let encoded = serde_json::to_vec(&projection).unwrap();
        let text = String::from_utf8(encoded.clone()).unwrap();
        assert!(text.contains("\"root\""));
        assert!(text.contains("\"nodes\""));
        let decoded: Utf16JsonProjection = serde_json::from_slice(&encoded).unwrap();
        assert_eq!(decoded.number_at("/number"), Some(f64::INFINITY));
        assert_eq!(
            decoded.to_json_string().unwrap(),
            r#"{"text":"\ud800","number":null}"#
        );
    }

    #[test]
    fn flat_dto_deserialization_rejects_invalid_child_graphs() {
        for source in [
            r#"{"root":0,"nodes":[{"Array":[1]}]}"#,
            r#"{"root":0,"nodes":[{"Array":[0]}]}"#,
            r#"{"root":0,"nodes":[{"Array":[1,1]},"Null"]}"#,
        ] {
            assert!(
                serde_json::from_str::<Utf16JsonProjection>(source).is_err(),
                "{source}"
            );
        }
    }
}

#[cfg(test)]
mod consumer_tests {
    use super::*;

    #[test]
    fn paired_consumer_restores_exact_source_after_structural_transform() {
        let schema = Utf16JsonProjection::parse(
            r#"{"required":["\ud800"],"properties":{"\ud800":{"enum":["\ud801"]}}}"#,
        )
        .unwrap();
        let input = Utf16JsonProjection::parse(r#"{"\ud801":"\ud802","\ue000":"\ud800"}"#).unwrap();
        let consumer = Utf16JsonConsumer::new(&[&schema, &input]).unwrap();
        let required_key = consumer.values()[0]["required"][0]
            .as_str()
            .expect("consumer strings are valid Rust strings");
        assert!(consumer.values()[0]["properties"]
            .get(required_key)
            .is_some());
        assert!(consumer.values()[1].get(required_key).is_none());
        assert_ne!(required_key, "\u{e000}");
        assert_eq!(
            consumer
                .restore(&consumer.values()[0])
                .unwrap()
                .to_json_string()
                .unwrap(),
            schema.to_json_string().unwrap()
        );

        let transformed = serde_json::json!({
            "required": consumer.values()[0]["required"],
            "properties": consumer.values()[0]["properties"],
            "additionalProperties": false
        });
        let restored = consumer.restore(&transformed).unwrap();
        assert_eq!(
            restored.to_json_string().unwrap(),
            r#"{"required":["\ud800"],"properties":{"\ud800":{"enum":["\ud801"]}},"additionalProperties":false}"#
        );
        assert_eq!(
            consumer
                .restore(&consumer.values()[1])
                .unwrap()
                .to_json_string()
                .unwrap(),
            input.to_json_string().unwrap()
        );
    }
}

#[cfg(test)]
mod utf16_document_tests {
    use super::{Utf16JsonProjection, Utf16JsonProjectionError};

    #[test]
    fn literal_lone_keys_and_values_survive_utf16_document_parsing() {
        let mut document = "{\"".encode_utf16().collect::<Vec<_>>();
        document.extend([0xd800, 0x03a9, 0xdc00]);
        document.extend("\":\"".encode_utf16());
        document.extend([0xdc00, 0x41, 0xd800]);
        document.extend("\",\"emoji\":\"🧪\"}".encode_utf16());
        let value = Utf16JsonProjection::parse_utf16_json(&document).unwrap();
        let key = [0xd800, 0x03a9, 0xdc00];
        assert_eq!(
            value
                .subprojection_property_units(&key)
                .unwrap()
                .string_units(""),
            Some(vec![0xdc00, 0x41, 0xd800])
        );
        assert_eq!(
            value.string_units("/emoji"),
            Some("🧪".encode_utf16().collect())
        );
        let encoded = value.to_json_string().unwrap();
        assert!(encoded.contains("\\ud800Ω\\udc00"));
        assert!(encoded.contains("\\udc00A\\ud800"));
    }

    #[test]
    fn invalid_escaped_lone_units_stay_invalid_and_report_unit_offsets() {
        for (document, expected_offset) in [(vec![0xd800], 0), (vec![0x22, 0x5c, 0xd800, 0x22], 2)]
        {
            match Utf16JsonProjection::parse_utf16_json(&document).unwrap_err() {
                Utf16JsonProjectionError::InvalidUtf16Json { offset, .. } => {
                    assert_eq!(offset, expected_offset)
                }
                error => panic!("wrong error: {error}"),
            }
        }
        let valid = r#"{"quoted":"\\\"","escaped":"\ud800","wide":1e400}"#;
        let utf16 =
            Utf16JsonProjection::parse_utf16_json(&valid.encode_utf16().collect::<Vec<_>>())
                .unwrap();
        assert!(utf16.semantically_eq(&Utf16JsonProjection::parse(valid).unwrap()));
    }

    #[test]
    fn deep_utf16_document_parser_clones_and_drops_without_recursive_carriers() {
        let depth = 1536;
        let mut document = "[".repeat(depth).encode_utf16().collect::<Vec<_>>();
        document.push(0x22);
        document.push(0xdfff);
        document.push(0x22);
        document.extend("]".repeat(depth).encode_utf16());
        let value = Utf16JsonProjection::parse_utf16_json(&document).unwrap();
        assert_eq!(value.string_units(&"/0".repeat(depth)), Some(vec![0xdfff]));
        let cloned = value.clone();
        drop(value);
        let source = cloned.to_json_string().unwrap();
        assert!(source.contains("\\udfff"));
        drop(cloned);
    }

    #[test]
    fn current_display_apis_do_not_replace_the_exact_flat_source() {
        let mut frame =
            Utf16JsonProjection::parse(r#"{"count":1,"nested":{"text":"source-\ud800"}}"#).unwrap();
        assert_eq!(frame.value["nested"]["text"], "source-�");
        frame
            .set_pointer("/count", Utf16JsonProjection::plain(serde_json::json!(2)))
            .unwrap();
        assert_eq!(frame.value["count"], 2);
        assert_eq!(
            frame.string_units("/nested/text"),
            Some("source-".encode_utf16().chain([0xd800]).collect())
        );
        assert_eq!(
            frame.to_json_string().unwrap(),
            r#"{"count":2,"nested":{"text":"source-\ud800"}}"#
        );
    }

    #[test]
    fn display_rebase_keeps_unchanged_surrogate_keys_and_values() {
        let mut projection =
            Utf16JsonProjection::parse(r#"{"\ud800":"value-\ud801","keep":"same"}"#).unwrap();
        let mut changed = projection.value.clone();
        changed["keep"] = serde_json::json!("updated");
        projection.rebase_display_value(changed).unwrap();

        assert_eq!(
            projection.to_json_string().unwrap(),
            r#"{"\ud800":"value-\ud801","keep":"updated"}"#
        );
        assert_eq!(
            projection.string_units("/keep"),
            Some("updated".encode_utf16().collect())
        );
    }

    #[test]
    fn plain_display_ingress_still_uses_javascript_number_stringification() {
        let projection = Utf16JsonProjection::plain(serde_json::json!(9_007_199_254_740_993_u64));
        assert_eq!(projection.to_json_string().unwrap(), "9007199254740992");
    }
}

#[cfg(test)]
mod flat_string_mapper_tests {
    use super::Utf16JsonProjection;

    #[test]
    fn replaces_values_once_in_native_order_without_changing_exact_keys() {
        let template = Utf16JsonProjection::parse(
            r#"{"10":"ten","2":"two","tail":[{"\ud800":"hole"},"keep"]}"#,
        )
        .unwrap();
        let replacement =
            Utf16JsonProjection::parse(r#"{"value":"\udfff","unexpanded":"hole"}"#).unwrap();
        let mut visited = Vec::new();
        let result = template
            .map_string_values(|units| {
                visited.push(String::from_utf16(units).unwrap());
                (units == "hole".encode_utf16().collect::<Vec<_>>()).then(|| replacement.clone())
            })
            .unwrap();
        assert_eq!(visited, ["two", "ten", "hole", "keep"]);
        let object = result.subprojection("/tail/0").unwrap();
        let value = object.subprojection_property_units(&[0xd800]).unwrap();
        assert_eq!(value.string_units("/value"), Some(vec![0xdfff]));
        assert_eq!(value.string_at("/unexpanded"), Some("hole"));
        assert_eq!(result.string_at("/tail/1"), Some("keep"));
        assert_eq!(
            template
                .subprojection("/tail/0")
                .unwrap()
                .subprojection_property_units(&[0xd800])
                .unwrap()
                .string_at(""),
            Some("hole")
        );
    }

    #[test]
    fn deep_template_replacement_and_drop_are_iterative() {
        let depth = 1536;
        let template = Utf16JsonProjection::parse(&format!(
            "{}\"hole\"{}",
            "[".repeat(depth),
            "]".repeat(depth)
        ))
        .unwrap();
        let replacement = Utf16JsonProjection::parse(r#"{"value":"\ud800"}"#).unwrap();
        let mut calls = 0;
        let result = template
            .map_string_values(|_| {
                calls += 1;
                Some(replacement.clone())
            })
            .unwrap();
        assert_eq!(calls, 1);
        assert_eq!(
            result.string_units(&format!("{}/value", "/0".repeat(depth))),
            Some(vec![0xd800])
        );
        let cloned = result.clone();
        drop(result);
        assert!(cloned.to_json_string().unwrap().contains("\\ud800"));
    }

    #[test]
    fn wide_template_keeps_nonstring_nodes_and_one_callback_per_value() {
        let values = (0..4096)
            .map(|_| Utf16JsonProjection::parse(r#""keep""#).unwrap())
            .collect();
        let template = Utf16JsonProjection::array(values).unwrap();
        let mut calls = 0;
        let result = template
            .map_string_values(|_| {
                calls += 1;
                None
            })
            .unwrap();
        assert_eq!(calls, 4096);
        assert!(result.semantically_eq(&template));
    }
}
