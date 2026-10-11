//! Structured JSON projections that retain JavaScript UTF-16 strings and keys.
//!
//! serde_json::Value is the Rust-safe display tree. Exact JavaScript code
//! units travel beside it in typed pointer projections and are applied when
//! the value is addressed or serialized. Callers must preserve this carrier
//! rather than lowering it to Value when exact strings or keys are present.

use serde::{Deserialize, Deserializer, Serialize};
use serde_json::{Map, Value};
use std::collections::HashMap;
use std::io::Write;
use thiserror::Error;

/// A shared, reversible Rust string namespace for consumers of exact JSON.
///
/// Unpaired UTF-16 units use otherwise absent BMP private characters, so keys,
/// required-property names, enum values, and input values share one identity.
/// This representation is local to the consumer and must never be serialized
/// to a protocol. Use [`Self::restore`] after a structural transformation.
pub struct Utf16JsonConsumer {
    values: Vec<Value>,
    units: HashMap<u16, u16>,
}

impl Utf16JsonConsumer {
    /// Project all inputs into one collision-free namespace.
    pub fn new(inputs: &[&Utf16JsonProjection]) -> Result<Self, Utf16JsonProjectionError> {
        let mut occupied = std::collections::HashSet::new();
        let mut unpaired = std::collections::BTreeSet::new();
        for input in inputs {
            input.validate()?;
            let mut pending = vec![(&input.value, String::new())];
            while let Some((value, pointer)) = pending.pop() {
                let mut inspect = |units: Vec<u16>| {
                    occupied.extend(
                        units
                            .iter()
                            .copied()
                            .filter(|unit| (0xe000..=0xf8ff).contains(unit)),
                    );
                    unpaired.extend(
                        char::decode_utf16(units)
                            .filter_map(Result::err)
                            .map(|error| error.unpaired_surrogate()),
                    );
                };
                match value {
                    Value::String(_) => inspect(input.string_units(&pointer).expect("string leaf")),
                    Value::Array(items) => {
                        pending.extend(items.iter().enumerate().map(|(index, value)| {
                            (value, join_pointer(&pointer, &index.to_string()))
                        }))
                    }
                    Value::Object(object) => {
                        for (key, value) in object {
                            inspect(input.key_units(&pointer, key));
                            pending.push((value, join_pointer(&pointer, key)));
                        }
                    }
                    _ => {}
                }
            }
        }
        let mut available = (0xe000_u16..=0xf8ff).filter(|unit| !occupied.contains(unit));
        let mut units = HashMap::new();
        for unit in unpaired {
            units.insert(
                unit,
                available
                    .next()
                    .ok_or(Utf16JsonProjectionError::InvalidProjection(
                        "consumer UTF-16 namespace exhausted",
                    ))?,
            );
        }
        let encode = |source: Vec<u16>| -> String {
            char::decode_utf16(source)
                .map(|item| match item {
                    Ok(character) => character,
                    Err(error) => char::from_u32(u32::from(units[&error.unpaired_surrogate()]))
                        .expect("BMP private character"),
                })
                .collect()
        };
        let mut values = Vec::with_capacity(inputs.len());
        for input in inputs {
            enum Task<'a> {
                Visit(&'a Value, String),
                Array(usize),
                Object(Vec<String>),
            }
            let mut tasks = vec![Task::Visit(&input.value, String::new())];
            let mut completed = Vec::new();
            while let Some(task) = tasks.pop() {
                match task {
                    Task::Visit(Value::Array(items), pointer) => {
                        tasks.push(Task::Array(items.len()));
                        tasks.extend(items.iter().enumerate().rev().map(|(index, value)| {
                            Task::Visit(value, join_pointer(&pointer, &index.to_string()))
                        }));
                    }
                    Task::Visit(Value::Object(object), pointer) => {
                        tasks.push(Task::Object(
                            object
                                .keys()
                                .map(|key| encode(input.key_units(&pointer, key)))
                                .collect(),
                        ));
                        tasks.extend(
                            object.iter().rev().map(|(key, value)| {
                                Task::Visit(value, join_pointer(&pointer, key))
                            }),
                        );
                    }
                    Task::Visit(Value::String(_), pointer) => completed.push(Value::String(
                        encode(input.string_units(&pointer).expect("string leaf")),
                    )),
                    Task::Visit(value, _) => completed.push(clone_value_iteratively(value)),
                    Task::Array(length) => {
                        let children = completed.split_off(completed.len() - length);
                        completed.push(Value::Array(children));
                    }
                    Task::Object(keys) => {
                        let children = completed.split_off(completed.len() - keys.len());
                        completed.push(Value::Object(keys.into_iter().zip(children).collect()));
                    }
                }
            }
            values.push(completed.pop().expect("one consumer root"));
        }
        Ok(Self { values, units })
    }

    /// Read the consumer values in the same order as the admitted inputs.
    pub fn values(&self) -> &[Value] {
        &self.values
    }

    /// Restore a transformed consumer value to its exact protocol carrier.
    pub fn restore(&self, value: &Value) -> Result<Utf16JsonProjection, Utf16JsonProjectionError> {
        let mut raw = serde_json::to_string(value)?;
        for (original, private) in &self.units {
            raw = raw.replace(
                char::from_u32(u32::from(*private)).expect("BMP private character"),
                &format!("\\u{original:04x}"),
            );
        }
        Utf16JsonProjection::parse(&raw)
    }
}

/// An exact JavaScript string at an RFC 6901 pointer in the display tree.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Utf16JsonString {
    /// RFC 6901 path of the replacement-safe display string.
    pub pointer: String,
    /// Original JavaScript UTF-16 code units.
    pub code_units: Vec<u16>,
}

/// An exact JavaScript object key. pointer addresses its containing object;
/// placeholder is the Rust-safe key in the display tree.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Utf16JsonKey {
    /// RFC 6901 path of the containing object.
    pub pointer: String,
    /// Unique display-tree property standing in for the original key.
    pub placeholder: String,
    /// Original JavaScript UTF-16 property name.
    pub code_units: Vec<u16>,
}

/// JSON value plus the exact JavaScript strings and object keys it projects.
#[derive(Debug, PartialEq, Eq, Serialize)]
pub struct Utf16JsonProjection {
    /// Replacement-safe tree for typed Rust consumers.
    pub value: Value,
    /// Exact units at string leaves that differ from the display tree.
    pub strings: Vec<Utf16JsonString>,
    /// Exact units of property names represented by private placeholders.
    pub keys: Vec<Utf16JsonKey>,
}

impl Clone for Utf16JsonProjection {
    fn clone(&self) -> Self {
        Self {
            value: clone_value_iteratively(&self.value),
            strings: self.strings.clone(),
            keys: self.keys.clone(),
        }
    }
}

impl std::ops::Deref for Utf16JsonProjection {
    type Target = Value;
    fn deref(&self) -> &Self::Target {
        &self.value
    }
}

impl Utf16JsonProjection {
    /// Assemble an array, rebasing each child's exact strings and keys.
    pub fn array(values: Vec<Self>) -> Result<Self, Utf16JsonProjectionError> {
        let mut projection = Self::plain(Value::Array(Vec::with_capacity(values.len())));
        for (index, child) in values.into_iter().enumerate() {
            child.validate()?;
            projection
                .value
                .as_array_mut()
                .expect("array")
                .push(child.value);
            projection
                .strings
                .extend(child.strings.into_iter().map(|mut item| {
                    item.pointer = format!("/{index}{}", item.pointer);
                    item
                }));
            projection
                .keys
                .extend(child.keys.into_iter().map(|mut item| {
                    item.pointer = format!("/{index}{}", item.pointer);
                    item
                }));
        }
        projection.validate()?;
        Ok(projection)
    }
    /// Construct ordinary JSON with no exact-string or property sidecars.
    #[must_use]
    pub fn plain(value: Value) -> Self {
        Self {
            value,
            strings: Vec::new(),
            keys: Vec::new(),
        }
    }

    /// Rebase a display-tree transformation while retaining exact units only
    /// at unchanged string leaves and keys still present in the new tree.
    /// A replacement from a new rich source must replace the whole carrier;
    /// this method is for coercion/schema transformations of the same input.
    pub fn rebase_display_value(&mut self, value: Value) -> Result<(), Utf16JsonProjectionError> {
        self.validate()?;
        let mut projection = self.clone();
        projection
            .strings
            .retain(|string| self.value.pointer(&string.pointer) == value.pointer(&string.pointer));
        projection.keys.retain(|key| {
            value
                .pointer(&key.pointer)
                .and_then(Value::as_object)
                .is_some_and(|object| object.contains_key(&key.placeholder))
        });
        projection.value = value;
        projection.validate()?;
        *self = projection;
        Ok(())
    }

    /// Build an exact projection for one JavaScript string value.
    pub fn root_string(
        display: String,
        code_units: Vec<u16>,
    ) -> Result<Self, Utf16JsonProjectionError> {
        let mut projection = Self::plain(Value::String(display.clone()));
        if display.encode_utf16().ne(code_units.iter().copied()) {
            projection.strings.push(Utf16JsonString {
                pointer: String::new(),
                code_units,
            });
        }
        projection.validate()?;
        Ok(projection)
    }

    /// Parse one JSON value while preserving unpaired UTF-16 units in values
    /// and property names.
    pub fn parse(input: &str) -> Result<Self, Utf16JsonProjectionError> {
        if let Ok(value) = serde_json::from_str(input) {
            return Ok(Self::plain(value));
        }
        let mut parser = ProjectionParser {
            input,
            offset: 0,
            strings: Vec::new(),
            keys: Vec::new(),
            next_placeholder: 0,
        };
        let value = parser.value("")?;
        parser.whitespace();
        if parser.offset != input.len() {
            return Err(parser.error("trailing characters"));
        }
        let result = Self {
            value,
            strings: parser.strings,
            keys: parser.keys,
        };
        result.validate()?;
        Ok(result)
    }

    /// Serialize valid JSON with exact UTF-16 escapes for projected strings
    /// and property names.
    pub fn to_json_string(&self) -> Result<String, Utf16JsonProjectionError> {
        self.validate()?;
        let mut output = Vec::new();
        write_projected_value(&mut output, &self.value, &self.strings, &self.keys, "")?;
        String::from_utf8(output)
            .map_err(|_| Utf16JsonProjectionError::InvalidProjection("non-UTF-8 encoder output"))
    }

    /// Validate sidecar pointers, display text, placeholders, and exact-key
    /// uniqueness before the projection is used or serialized.
    pub fn validate(&self) -> Result<(), Utf16JsonProjectionError> {
        let mut seen_strings = HashMap::new();
        for sidecar in &self.strings {
            let target = value_at_pointer(&self.value, &sidecar.pointer)
                .and_then(Value::as_str)
                .ok_or(Utf16JsonProjectionError::InvalidStringPointer)?;
            if target != String::from_utf16_lossy(&sidecar.code_units)
                || seen_strings.insert(sidecar.pointer.as_str(), ()).is_some()
            {
                return Err(Utf16JsonProjectionError::InvalidStringPointer);
            }
        }
        let mut seen_keys = HashMap::new();
        for sidecar in &self.keys {
            let object = value_at_pointer(&self.value, &sidecar.pointer)
                .and_then(Value::as_object)
                .ok_or(Utf16JsonProjectionError::InvalidKeyPointer)?;
            if String::from_utf16(&sidecar.code_units).is_ok()
                || !object.contains_key(&sidecar.placeholder)
                || seen_keys
                    .insert((sidecar.pointer.as_str(), sidecar.placeholder.as_str()), ())
                    .is_some()
            {
                return Err(Utf16JsonProjectionError::InvalidKeyPointer);
            }
        }
        validate_exact_key_uniqueness(&self.value, &self.keys, "")?;
        Ok(())
    }

    /// Return the string-leaf map used by current JSONL exact-string
    /// persistence. Object-key units stay in this projection's keys field.
    #[must_use]
    pub fn string_overrides(&self) -> std::collections::BTreeMap<String, Vec<u16>> {
        self.strings
            .iter()
            .map(|sidecar| (sidecar.pointer.clone(), sidecar.code_units.clone()))
            .collect()
    }

    /// Read an object's property name as exact JavaScript UTF-16 units.
    #[must_use]
    pub fn key_units(&self, parent_pointer: &str, display_key: &str) -> Vec<u16> {
        self.keys
            .iter()
            .find(|sidecar| sidecar.pointer == parent_pointer && sidecar.placeholder == display_key)
            .map(|sidecar| sidecar.code_units.clone())
            .unwrap_or_else(|| display_key.encode_utf16().collect())
    }

    /// Read a string leaf as exact JavaScript UTF-16 units.
    #[must_use]
    pub fn string_units(&self, pointer: &str) -> Option<Vec<u16>> {
        let value = value_at_pointer(&self.value, pointer)?.as_str()?;
        Some(
            self.strings
                .iter()
                .find(|sidecar| sidecar.pointer == pointer)
                .map(|sidecar| sidecar.code_units.clone())
                .unwrap_or_else(|| value.encode_utf16().collect()),
        )
    }

    /// Extract a subtree and rebase its string/key pointers to that subtree.
    pub fn subprojection(&self, pointer: &str) -> Result<Self, Utf16JsonProjectionError> {
        let value = value_at_pointer(&self.value, pointer)
            .map(clone_value_iteratively)
            .ok_or(Utf16JsonProjectionError::InvalidPointer)?;
        let mut result = Self::plain(value);
        for sidecar in &self.strings {
            if let Some(relative) = strip_pointer_prefix(&sidecar.pointer, pointer) {
                result.strings.push(Utf16JsonString {
                    pointer: relative,
                    code_units: sidecar.code_units.clone(),
                });
            }
        }
        for sidecar in &self.keys {
            if let Some(relative) = strip_pointer_prefix(&sidecar.pointer, pointer) {
                result.keys.push(Utf16JsonKey {
                    pointer: relative,
                    placeholder: sidecar.placeholder.clone(),
                    code_units: sidecar.code_units.clone(),
                });
            }
        }
        result.validate()?;
        Ok(result)
    }

    /// Replace an existing subtree, rebasing its exact strings and keys onto
    /// the destination pointer. The operation is transactional.
    pub fn set_pointer(
        &mut self,
        pointer: &str,
        child: Self,
    ) -> Result<(), Utf16JsonProjectionError> {
        self.validate()?;
        child.validate()?;
        if pointer.is_empty() {
            *self = child;
            return Ok(());
        }
        let mut next = self.clone();
        *next
            .value
            .pointer_mut(pointer)
            .ok_or(Utf16JsonProjectionError::InvalidPointer)? = child.value;
        next.strings
            .retain(|entry| !pointer_contains(pointer, &entry.pointer));
        next.keys
            .retain(|entry| !pointer_contains(pointer, &entry.pointer));
        next.strings
            .extend(child.strings.into_iter().map(|entry| Utf16JsonString {
                pointer: format!("{pointer}{}", entry.pointer),
                code_units: entry.code_units,
            }));
        next.keys
            .extend(child.keys.into_iter().map(|entry| Utf16JsonKey {
                pointer: format!("{pointer}{}", entry.pointer),
                placeholder: entry.placeholder,
                code_units: entry.code_units,
            }));
        next.validate()?;
        *self = next;
        Ok(())
    }

    /// Retain selected top-level fields and all projected descendants.
    pub fn pick_object_fields(&self, fields: &[&str]) -> Result<Self, Utf16JsonProjectionError> {
        let object = self
            .value
            .as_object()
            .ok_or(Utf16JsonProjectionError::ExpectedObject)?;
        let mut result = Self::plain(Value::Object(Map::new()));
        for field in fields {
            if object.contains_key(*field) {
                let child = self.subprojection(&join_pointer("", field))?;
                result.set_field(field, child)?;
            }
        }
        Ok(result)
    }

    /// Replace or insert one field while rebasing the child projection.
    pub fn set_field(&mut self, field: &str, child: Self) -> Result<(), Utf16JsonProjectionError> {
        self.validate()?;
        child.validate()?;

        // A generated safe key may equal a caller's literal field name. It is
        // still an exact unpaired key, so move it aside before inserting the
        // distinct literal property.
        let root_placeholder = self
            .keys
            .iter()
            .find(|sidecar| sidecar.pointer.is_empty() && sidecar.placeholder == field)
            .map(|sidecar| sidecar.placeholder.clone());
        if let Some(old) = root_placeholder {
            let new = {
                let object = self
                    .value
                    .as_object_mut()
                    .ok_or(Utf16JsonProjectionError::ExpectedObject)?;
                let new = fresh_key_placeholder(object, field);
                rename_projection_key(object, &old, &new);
                new
            };
            self.rebase_root_key(&old, &new);
        }

        let pointer = join_pointer("", field);
        {
            let object = self
                .value
                .as_object_mut()
                .ok_or(Utf16JsonProjectionError::ExpectedObject)?;
            object.insert(field.to_owned(), child.value);
        }
        self.strings
            .retain(|sidecar| !pointer_contains(&pointer, &sidecar.pointer));
        self.keys
            .retain(|sidecar| !pointer_contains(&pointer, &sidecar.pointer));
        self.strings
            .extend(child.strings.into_iter().map(|sidecar| Utf16JsonString {
                pointer: format!("{pointer}{}", sidecar.pointer),
                code_units: sidecar.code_units,
            }));
        self.keys
            .extend(child.keys.into_iter().map(|sidecar| Utf16JsonKey {
                pointer: format!("{pointer}{}", sidecar.pointer),
                placeholder: sidecar.placeholder,
                code_units: sidecar.code_units,
            }));
        self.validate()
    }

    /// Select a consumer's private placeholder namespace without changing
    /// exact keys, sibling order, or descendant string/key pointers.
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
        for index in 0..self.keys.len() {
            let parent = self.keys[index].pointer.clone();
            let old = self.keys[index].placeholder.clone();
            let new = {
                let object = self
                    .value
                    .pointer_mut(&parent)
                    .and_then(Value::as_object_mut)
                    .ok_or(Utf16JsonProjectionError::InvalidKeyPointer)?;
                let candidate = loop {
                    let candidate = format!("{prefix}{next}__");
                    next =
                        next.checked_add(1)
                            .ok_or(Utf16JsonProjectionError::InvalidProjection(
                                "key placeholder namespace exhausted",
                            ))?;
                    if candidate == old || !object.contains_key(&candidate) {
                        break candidate;
                    }
                };
                rename_projection_key(object, &old, &candidate);
                candidate
            };
            let old_path = join_pointer(&parent, &old);
            let new_path = join_pointer(&parent, &new);
            self.keys[index].placeholder = new;
            for sidecar in &mut self.strings {
                sidecar.pointer = rebase_pointer(&sidecar.pointer, &old_path, &new_path);
            }
            for sidecar in &mut self.keys {
                sidecar.pointer = rebase_pointer(&sidecar.pointer, &old_path, &new_path);
            }
        }
        self.validate()
    }

    fn rebase_root_key(&mut self, old: &str, new: &str) {
        let old_path = join_pointer("", old);
        let new_path = join_pointer("", new);
        for sidecar in &mut self.strings {
            sidecar.pointer = rebase_pointer(&sidecar.pointer, &old_path, &new_path);
        }
        for sidecar in &mut self.keys {
            if sidecar.pointer.is_empty() && sidecar.placeholder == old {
                sidecar.placeholder = new.to_owned();
            } else {
                sidecar.pointer = rebase_pointer(&sidecar.pointer, &old_path, &new_path);
            }
        }
    }
}

impl<'de> Deserialize<'de> for Utf16JsonProjection {
    fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
    where
        D: Deserializer<'de>,
    {
        #[derive(Deserialize)]
        struct ProjectionFields {
            value: Value,
            #[serde(default)]
            strings: Vec<Utf16JsonString>,
            #[serde(default)]
            keys: Vec<Utf16JsonKey>,
        }

        let fields = ProjectionFields::deserialize(deserializer)?;
        let projection = Self {
            value: fields.value,
            strings: fields.strings,
            keys: fields.keys,
        };
        projection.validate().map_err(serde::de::Error::custom)?;
        Ok(projection)
    }
}

/// Parse, association, and structural failures at a rich JSON boundary.
#[derive(Debug, Error)]
pub enum Utf16JsonProjectionError {
    /// The input is not a complete valid JSON value.
    #[error("invalid exact JSON at byte {offset}: {message}")]
    InvalidJson {
        /// UTF-8 byte position where parsing failed.
        offset: usize,
        /// Short description of the invalid syntax.
        message: &'static str,
    },
    /// A sidecar is stale, ambiguous, or inconsistent with its display tree.
    #[error("invalid UTF-16 JSON projection: {0}")]
    InvalidProjection(&'static str),
    /// An exact string does not match the addressed display string.
    #[error("UTF-16 string projection does not identify its display string")]
    InvalidStringPointer,
    /// An exact property does not match a valid private placeholder.
    #[error("UTF-16 key projection does not identify an unpaired object key")]
    InvalidKeyPointer,
    /// A subtree or mutation path does not address the current tree.
    #[error("JSON projection pointer is invalid")]
    InvalidPointer,
    /// A string-specific operation received another JSON shape.
    #[error("request message projection must contain one root string")]
    RootStringRequired,
    /// An object-specific operation received another JSON shape.
    #[error("JSON projection operation requires an object")]
    ExpectedObject,
    /// Ordinary JSON serialization failed.
    #[error(transparent)]
    Serialize(#[from] serde_json::Error),
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
        pointer: String,
        items: Vec<Value>,
        phase: ContainerPhase,
    },
    Object {
        pointer: String,
        map: Map<String, Value>,
        generated: HashMap<String, Vec<u16>>,
        phase: ContainerPhase,
        pending_key: Option<String>,
    },
}

enum ParserAction {
    Continue,
    Value(String),
    ObjectKey,
    CloseArray,
    CloseObject,
}

struct ProjectionParser<'a> {
    input: &'a str,
    offset: usize,
    strings: Vec<Utf16JsonString>,
    keys: Vec<Utf16JsonKey>,
    next_placeholder: u64,
}

impl ProjectionParser<'_> {
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

    fn value(&mut self, pointer: &str) -> Result<Value, Utf16JsonProjectionError> {
        let mut frames = Vec::<ParserFrame>::new();
        let mut completed = None;
        self.start_value(pointer.to_owned(), &mut frames, &mut completed)?;

        loop {
            if let Some(value) = completed.take() {
                match frames.last_mut() {
                    Some(ParserFrame::Array { items, phase, .. }) => {
                        items.push(value);
                        *phase = ContainerPhase::Separator;
                    }
                    Some(ParserFrame::Object {
                        map,
                        pending_key,
                        phase,
                        ..
                    }) => {
                        let key = pending_key.take().expect("object value has a pending key");
                        map.insert(key, value);
                        *phase = ContainerPhase::Separator;
                    }
                    None => return Ok(value),
                }
                continue;
            }

            let action = match frames.last_mut() {
                Some(ParserFrame::Array {
                    pointer,
                    items,
                    phase,
                }) => match phase {
                    ContainerPhase::Start => {
                        if self.take(b']') {
                            ParserAction::CloseArray
                        } else {
                            *phase = ContainerPhase::NeedValue;
                            ParserAction::Value(join_pointer(pointer, &items.len().to_string()))
                        }
                    }
                    ContainerPhase::NeedValue => {
                        ParserAction::Value(join_pointer(pointer, &items.len().to_string()))
                    }
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
                    pointer,
                    phase,
                    pending_key,
                    ..
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
                        let key = pending_key
                            .as_ref()
                            .expect("object value has a pending key");
                        ParserAction::Value(join_pointer(pointer, key))
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
                ParserAction::Value(child_pointer) => {
                    self.start_value(child_pointer, &mut frames, &mut completed)?;
                }
                ParserAction::ObjectKey => {
                    let Some(ParserFrame::Object {
                        pointer,
                        map,
                        generated,
                        phase,
                        pending_key,
                    }) = frames.last_mut()
                    else {
                        return Err(self.error("invalid object parser state"));
                    };
                    self.object_member(pointer, map, generated, phase, pending_key)?;
                }
                ParserAction::CloseArray => {
                    let Some(ParserFrame::Array { items, .. }) = frames.pop() else {
                        return Err(self.error("invalid array parser state"));
                    };
                    completed = Some(Value::Array(items));
                }
                ParserAction::CloseObject => {
                    let Some(ParserFrame::Object { map, .. }) = frames.pop() else {
                        return Err(self.error("invalid object parser state"));
                    };
                    completed = Some(Value::Object(map));
                }
            }
        }
    }

    fn start_value(
        &mut self,
        pointer: String,
        frames: &mut Vec<ParserFrame>,
        completed: &mut Option<Value>,
    ) -> Result<(), Utf16JsonProjectionError> {
        self.whitespace();
        match self.input.as_bytes().get(self.offset).copied() {
            Some(b'"') => {
                let units = self.string()?;
                let display = String::from_utf16(&units).unwrap_or_else(|_| {
                    let display = String::from_utf16_lossy(&units);
                    self.strings.push(Utf16JsonString {
                        pointer,
                        code_units: units,
                    });
                    display
                });
                *completed = Some(Value::String(display));
            }
            Some(b'[') => {
                self.offset += 1;
                frames.push(ParserFrame::Array {
                    pointer,
                    items: Vec::new(),
                    phase: ContainerPhase::Start,
                });
            }
            Some(b'{') => {
                self.offset += 1;
                frames.push(ParserFrame::Object {
                    pointer,
                    map: Map::new(),
                    generated: HashMap::new(),
                    phase: ContainerPhase::Start,
                    pending_key: None,
                });
            }
            Some(_) => {
                let start = self.offset;
                while self.input.as_bytes().get(self.offset).is_some_and(|byte| {
                    !matches!(byte, b' ' | b'\n' | b'\r' | b'\t' | b',' | b']' | b'}')
                }) {
                    self.offset += 1;
                }
                let value = serde_json::from_str(&self.input[start..self.offset])
                    .map_err(|_| self.error("invalid JSON scalar"))?;
                if matches!(value, Value::Null | Value::Bool(_) | Value::Number(_)) {
                    *completed = Some(value);
                } else {
                    return Err(self.error("invalid JSON scalar"));
                }
            }
            None => return Err(self.error("expected JSON value")),
        }
        Ok(())
    }

    fn object_member(
        &mut self,
        pointer: &str,
        map: &mut Map<String, Value>,
        generated: &mut HashMap<String, Vec<u16>>,
        phase: &mut ContainerPhase,
        pending_key: &mut Option<String>,
    ) -> Result<(), Utf16JsonProjectionError> {
        self.whitespace();
        let units = self.string()?;
        let parsed_key = String::from_utf16(&units);
        let (key, unpaired) = match parsed_key {
            Ok(key) => (key, false),
            Err(_) => (String::new(), true),
        };
        let key = if !unpaired {
            if let Some(old_units) = generated.remove(&key) {
                let moved = self.key_placeholder(map, Some(&key));
                rename_projection_key(map, &key, &moved);
                self.rebase_key(pointer, &key, &moved);
                generated.insert(moved, old_units);
            }
            key
        } else if let Some(existing) = generated
            .iter()
            .find_map(|(placeholder, old_units)| (old_units == &units).then(|| placeholder.clone()))
        {
            existing
        } else {
            let placeholder = self.key_placeholder(map, None);
            generated.insert(placeholder.clone(), units.clone());
            self.keys.push(Utf16JsonKey {
                pointer: pointer.to_owned(),
                placeholder: placeholder.clone(),
                code_units: units,
            });
            placeholder
        };
        if !self.take(b':') {
            return Err(self.error("expected object colon"));
        }
        let child_pointer = join_pointer(pointer, &key);
        if map.contains_key(&key) {
            self.clear_descendants(&child_pointer);
        }
        *pending_key = Some(key);
        *phase = ContainerPhase::NeedValue;
        Ok(())
    }

    fn key_placeholder(&mut self, map: &Map<String, Value>, forbidden: Option<&str>) -> String {
        loop {
            self.next_placeholder = self.next_placeholder.saturating_add(1);
            let candidate = format!("__lingxiUtf16KeyProjectionV1_{}__", self.next_placeholder);
            if forbidden != Some(candidate.as_str()) && !map.contains_key(&candidate) {
                return candidate;
            }
        }
    }

    fn clear_descendants(&mut self, pointer: &str) {
        self.strings
            .retain(|item| !pointer_contains(pointer, &item.pointer));
        // The existing property's exact name belongs to the parent object.
        // Replacing its value only removes projections inside that value.
        self.keys
            .retain(|item| !pointer_contains(pointer, &item.pointer));
    }

    fn rebase_key(&mut self, parent: &str, old: &str, new: &str) {
        let old_path = join_pointer(parent, old);
        let new_path = join_pointer(parent, new);
        for sidecar in &mut self.strings {
            sidecar.pointer = rebase_pointer(&sidecar.pointer, &old_path, &new_path);
        }
        for sidecar in &mut self.keys {
            if sidecar.pointer == parent && sidecar.placeholder == old {
                sidecar.placeholder = new.to_owned();
            } else {
                sidecar.pointer = rebase_pointer(&sidecar.pointer, &old_path, &new_path);
            }
        }
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
}

fn write_projected_value(
    output: &mut Vec<u8>,
    value: &Value,
    strings: &[Utf16JsonString],
    keys: &[Utf16JsonKey],
    pointer: &str,
) -> Result<(), Utf16JsonProjectionError> {
    enum Task<'a> {
        Value(&'a Value, String),
        ObjectKey(&'a str, String),
        Byte(u8),
    }

    let mut pending = vec![Task::Value(value, pointer.to_owned())];
    while let Some(task) = pending.pop() {
        match task {
            Task::Byte(byte) => output.push(byte),
            Task::ObjectKey(key, object_pointer) => {
                if let Some(sidecar) = keys
                    .iter()
                    .find(|sidecar| sidecar.pointer == object_pointer && sidecar.placeholder == key)
                {
                    write_units(output, &sidecar.code_units);
                } else {
                    serde_json::to_writer(&mut *output, key)?;
                }
            }
            Task::Value(value, pointer) => match value {
                Value::String(text) => {
                    if let Some(sidecar) = strings.iter().find(|sidecar| sidecar.pointer == pointer)
                    {
                        write_units(output, &sidecar.code_units);
                    } else {
                        serde_json::to_writer(&mut *output, text)?;
                    }
                }
                Value::Array(items) => {
                    output.push(b'[');
                    pending.push(Task::Byte(b']'));
                    for (index, item) in items.iter().enumerate().rev() {
                        pending.push(Task::Value(item, format!("{pointer}/{index}")));
                        if index > 0 {
                            pending.push(Task::Byte(b','));
                        }
                    }
                }
                Value::Object(object) => {
                    output.push(b'{');
                    pending.push(Task::Byte(b'}'));
                    for (index, (key, child)) in javascript_ordered_object_entries(object)
                        .into_iter()
                        .enumerate()
                        .rev()
                    {
                        pending.push(Task::Value(child, join_pointer(&pointer, key)));
                        pending.push(Task::Byte(b':'));
                        pending.push(Task::ObjectKey(key, pointer.clone()));
                        if index > 0 {
                            pending.push(Task::Byte(b','));
                        }
                    }
                }
                Value::Number(number) => {
                    let number =
                        number
                            .as_f64()
                            .ok_or(Utf16JsonProjectionError::InvalidProjection(
                                "JSON number is outside the finite JavaScript number range",
                            ))?;
                    let mut buffer = ryu_js::Buffer::new();
                    output.extend_from_slice(buffer.format_finite(number).as_bytes());
                }
                _ => serde_json::to_writer(&mut *output, value)?,
            },
        }
    }
    Ok(())
}

fn javascript_array_index(key: &str) -> Option<u32> {
    let index = key.parse::<u32>().ok()?;
    (index != u32::MAX && index.to_string() == key).then_some(index)
}

fn javascript_ordered_object_entries(object: &Map<String, Value>) -> Vec<(&String, &Value)> {
    let entries = object.iter().collect::<Vec<_>>();
    let mut integer_keys = Vec::new();
    let mut other_keys = Vec::new();
    for (position, (key, _)) in entries.iter().enumerate() {
        if let Some(index) = javascript_array_index(key) {
            integer_keys.push((index, position));
        } else {
            other_keys.push(position);
        }
    }
    integer_keys.sort_by_key(|(index, _)| *index);
    integer_keys
        .into_iter()
        .map(|(_, position)| entries[position])
        .chain(other_keys.into_iter().map(|position| entries[position]))
        .collect()
}

fn validate_exact_key_uniqueness(
    value: &Value,
    keys: &[Utf16JsonKey],
    pointer: &str,
) -> Result<(), Utf16JsonProjectionError> {
    let mut pending = vec![(value, pointer.to_owned())];
    while let Some((value, pointer)) = pending.pop() {
        match value {
            Value::Array(items) => {
                pending.extend(
                    items
                        .iter()
                        .enumerate()
                        .map(|(index, item)| (item, format!("{pointer}/{index}"))),
                );
            }
            Value::Object(object) => {
                let mut seen = HashMap::<Vec<u16>, ()>::new();
                for (key, child) in object {
                    let units = keys
                        .iter()
                        .find(|sidecar| sidecar.pointer == pointer && sidecar.placeholder == *key)
                        .map(|sidecar| sidecar.code_units.clone())
                        .unwrap_or_else(|| key.encode_utf16().collect());
                    if seen.insert(units, ()).is_some() {
                        return Err(Utf16JsonProjectionError::InvalidProjection(
                            "object has duplicate exact UTF-16 property names",
                        ));
                    }
                    pending.push((child, join_pointer(&pointer, key)));
                }
            }
            _ => {}
        }
    }
    Ok(())
}

fn clone_value_iteratively(value: &Value) -> Value {
    enum Task<'a> {
        Visit(&'a Value),
        Array(usize),
        Object(Vec<&'a String>),
    }

    let mut tasks = vec![Task::Visit(value)];
    let mut completed = Vec::<Value>::new();
    while let Some(task) = tasks.pop() {
        match task {
            Task::Visit(Value::Array(items)) => {
                tasks.push(Task::Array(items.len()));
                tasks.extend(items.iter().rev().map(Task::Visit));
            }
            Task::Visit(Value::Object(object)) => {
                let keys: Vec<_> = object.keys().collect();
                tasks.push(Task::Object(keys.clone()));
                tasks.extend(keys.into_iter().rev().map(|key| Task::Visit(&object[key])));
            }
            Task::Visit(leaf) => completed.push(leaf.clone()),
            Task::Array(length) => {
                let children = completed.split_off(completed.len() - length);
                completed.push(Value::Array(children));
            }
            Task::Object(keys) => {
                let children = completed.split_off(completed.len() - keys.len());
                let mut object = Map::new();
                for (key, child) in keys.into_iter().zip(children) {
                    object.insert(key.clone(), child);
                }
                completed.push(Value::Object(object));
            }
        }
    }
    completed.pop().expect("one root value is cloned")
}

fn value_at_pointer<'a>(root: &'a Value, pointer: &str) -> Option<&'a Value> {
    if pointer.is_empty() {
        return Some(root);
    }
    let rest = pointer.strip_prefix('/')?;
    let mut value = root;
    for raw in rest.split('/') {
        let token = raw.replace("~1", "/").replace("~0", "~");
        if pointer_token(&token) != raw {
            return None;
        }
        value = match value {
            Value::Object(object) => object.get(&token)?,
            Value::Array(items) => {
                let index = token.parse::<usize>().ok()?;
                if index.to_string() != token {
                    return None;
                }
                items.get(index)?
            }
            _ => return None,
        };
    }
    Some(value)
}

fn rename_projection_key(object: &mut Map<String, Value>, old: &str, new: &str) {
    // A placeholder rename changes only the safe display name, so retain the
    // original exact property's position relative to its siblings.
    debug_assert!(object.contains_key(old));
    for (key, value) in std::mem::take(object) {
        object.insert(if key == old { new.to_owned() } else { key }, value);
    }
}

fn fresh_key_placeholder(object: &Map<String, Value>, forbidden: &str) -> String {
    let mut index = 1_u64;
    loop {
        let candidate = format!("__lingxiUtf16KeyProjectionV1_{index}__");
        if candidate != forbidden && !object.contains_key(&candidate) {
            return candidate;
        }
        index = index.saturating_add(1);
    }
}

fn pointer_token(token: &str) -> String {
    token.replace('~', "~0").replace('/', "~1")
}

fn join_pointer(parent: &str, token: &str) -> String {
    format!("{parent}/{}", pointer_token(token))
}

fn strip_pointer_prefix(path: &str, prefix: &str) -> Option<String> {
    if path == prefix {
        Some(String::new())
    } else {
        path.strip_prefix(prefix)
            .filter(|suffix| suffix.starts_with('/'))
            .map(str::to_owned)
    }
}

fn pointer_contains(parent: &str, path: &str) -> bool {
    path == parent
        || path
            .strip_prefix(parent)
            .is_some_and(|suffix| suffix.starts_with('/'))
}

fn rebase_pointer(pointer: &str, old: &str, new: &str) -> String {
    if pointer == old {
        new.to_owned()
    } else {
        pointer
            .strip_prefix(old)
            .filter(|suffix| suffix.starts_with('/'))
            .map_or_else(|| pointer.to_owned(), |suffix| format!("{new}{suffix}"))
    }
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

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn paired_consumer_uses_exact_identity_avoids_pua_and_restores_transforms() {
        let schema = Utf16JsonProjection::parse(r#"{"properties":{"\ud800":{"enum":["\ud801","\ud802"]},"\ue000":{}},"required":["\ud800"]}"#).unwrap();
        let input = Utf16JsonProjection::parse(r#"{"\ud801":"\ud802","\ue000":"\ud800"}"#).unwrap();
        let consumer = Utf16JsonConsumer::new(&[&schema, &input]).unwrap();
        let schema_key = consumer.values()[0]["required"][0].as_str().unwrap();
        assert!(consumer.values()[0]["properties"].get(schema_key).is_some());
        assert!(consumer.values()[1].get(schema_key).is_none());
        assert_eq!(schema_key.encode_utf16().count(), 1);
        assert_ne!(schema_key, "\u{e000}");
        assert_eq!(
            consumer
                .restore(&consumer.values()[0])
                .unwrap()
                .to_json_string()
                .unwrap(),
            schema.to_json_string().unwrap()
        );
        assert_eq!(
            consumer
                .restore(&consumer.values()[1])
                .unwrap()
                .to_json_string()
                .unwrap(),
            input.to_json_string().unwrap()
        );
        let strict = serde_json::json!({"type":"object","properties":consumer.values()[0]["properties"],"required":consumer.values()[0]["required"],"additionalProperties":false});
        let restored = consumer.restore(&strict).unwrap();
        assert!(restored.to_json_string().unwrap().contains(r#""\ud800""#));
        assert_eq!(restored.value["additionalProperties"], false);
    }

    #[test]
    fn exact_string_and_key_units_round_trip_without_value_collisions() {
        let source = r#"{"client":"\ud800","props":{"\ud800":"\ud800","�":"visible"}}"#;
        let decoded = Utf16JsonProjection::parse(source).unwrap();
        decoded.validate().unwrap();
        assert_eq!(decoded.to_json_string().unwrap(), source);
        let parsed = Utf16JsonProjection::parse(source).unwrap();
        assert_eq!(parsed.string_units("/client"), Some(vec![0xd800]));
        assert_eq!(parsed.value["client"], "�");
        let props = parsed.subprojection("/props").unwrap();
        let object = props.value.as_object().unwrap();
        let exact_key = props.key_units("", object.keys().next().unwrap());
        assert!(exact_key == vec![0xd800] || exact_key == vec![0xfffd]);
    }

    #[test]
    fn placeholder_collision_is_renamed_before_a_real_key_is_inserted() {
        let source = r#"{"\ud800":1,"sibling":4,"__lingxiUtf16KeyProjectionV1_1__":2,"�":3}"#;
        let parsed = Utf16JsonProjection::parse(source).unwrap();
        assert_eq!(parsed.to_json_string().unwrap(), source);
        assert_eq!(parsed.keys.len(), 1);
        parsed.validate().unwrap();
    }

    #[test]
    fn replacing_field_replaces_all_projection_metadata_and_preserves_siblings() {
        let mut projection = Utf16JsonProjection::parse(
            r#"{"field":{"old":"\ud800","nested":{"\ud800":"old"}},"sibling":"\ude00"}"#,
        )
        .unwrap();
        projection
            .set_field(
                "field",
                Utf16JsonProjection::plain(serde_json::json!({"new":true})),
            )
            .unwrap();
        assert_eq!(
            projection.to_json_string().unwrap(),
            r#"{"field":{"new":true},"sibling":"\ude00"}"#
        );
        assert_eq!(projection.string_units("/sibling"), Some(vec![0xde00]));
        assert!(projection
            .strings
            .iter()
            .all(|item| !item.pointer.starts_with("/field")));
        assert!(projection
            .keys
            .iter()
            .all(|item| !item.pointer.starts_with("/field")));

        let replacement = Utf16JsonProjection::parse(r#"{"fresh":"\ud800"}"#).unwrap();
        projection.set_field("field", replacement).unwrap();
        assert_eq!(projection.string_units("/field/fresh"), Some(vec![0xd800]));
        assert_eq!(
            projection.to_json_string().unwrap(),
            r#"{"field":{"fresh":"\ud800"},"sibling":"\ude00"}"#
        );
    }

    #[test]
    fn duplicate_unpaired_property_keeps_exact_key_and_last_value() {
        let source = r#"{"\ud800":"first","\ud800":"last"}"#;
        let projection = Utf16JsonProjection::parse(source).unwrap();
        projection.validate().unwrap();
        assert_eq!(projection.keys.len(), 1);
        assert_eq!(projection.to_json_string().unwrap(), r#"{"\ud800":"last"}"#);
        let key = projection.value.as_object().unwrap().keys().next().unwrap();
        assert_eq!(projection.key_units("", key), vec![0xd800]);

        let nested =
            Utf16JsonProjection::parse(r#"{"\ud800":{"\udc00":"\udfff"},"\ud800":"\ud801"}"#)
                .unwrap();
        assert_eq!(nested.keys.len(), 1);
        assert_eq!(nested.strings.len(), 1);
        assert_eq!(nested.to_json_string().unwrap(), r#"{"\ud800":"\ud801"}"#);
    }

    #[test]
    fn set_field_preserves_a_root_key_when_its_placeholder_is_the_field_name() {
        let mut projection =
            Utf16JsonProjection::parse(r#"{"\ud800":"kept","sibling":"stay"}"#).unwrap();
        let placeholder = projection.keys[0].placeholder.clone();
        assert_eq!(placeholder, "__lingxiUtf16KeyProjectionV1_1__");
        projection
            .set_field(
                &placeholder,
                Utf16JsonProjection::plain(Value::String("literal".into())),
            )
            .unwrap();
        assert_eq!(
            projection.to_json_string().unwrap(),
            r#"{"\ud800":"kept","sibling":"stay","__lingxiUtf16KeyProjectionV1_1__":"literal"}"#
        );
        let exact = projection
            .value
            .as_object()
            .unwrap()
            .keys()
            .find(|key| projection.key_units("", key) == vec![0xd800])
            .unwrap();
        assert_eq!(projection.key_units("", exact), vec![0xd800]);
    }

    #[test]
    fn consumer_key_namespace_preserves_nested_units_collisions_and_order() {
        let raw = r#"{"\ud800":{"x/y~":"\udfff","\udc00":"\ud801"},"__consumerKey_1__":"literal","sibling":true}"#;
        let mut value = Utf16JsonProjection::parse(raw).unwrap();
        value.rename_key_placeholders("__consumerKey_").unwrap();
        assert_eq!(value.to_json_string().unwrap(), raw);
        assert_eq!(value.keys.len(), 2);
        assert!(value
            .keys
            .iter()
            .all(|key| key.placeholder.starts_with("__consumerKey_")));
        assert_ne!(value.keys[0].placeholder, "__consumerKey_1__");
        let outer = value
            .keys
            .iter()
            .find(|key| key.pointer.is_empty())
            .unwrap();
        let child = value
            .subprojection(&join_pointer("", &outer.placeholder))
            .unwrap();
        assert_eq!(child.string_units("/x~1y~0"), Some(vec![0xdfff]));
        assert_eq!(child.keys[0].code_units, vec![0xdc00]);
        value
            .rename_key_placeholders("__secondConsumerKey_")
            .unwrap();
        assert_eq!(value.to_json_string().unwrap(), raw);
    }

    #[test]
    fn malformed_json_and_invalid_projection_pointers_are_rejected() {
        for source in [r#"{"x":"\ud800",}"#, r#"{"x":"\ud800"}false"#] {
            assert!(Utf16JsonProjection::parse(source).is_err());
        }
        let mut projection = Utf16JsonProjection::plain(serde_json::json!({"x":"y"}));
        projection.strings.push(Utf16JsonString {
            pointer: "/missing".into(),
            code_units: vec![0xd800],
        });
        assert!(projection.to_json_string().is_err());
    }

    #[test]
    fn javascript_number_format_and_integer_property_order_match_json_stringify() {
        for (number, expected) in [
            (1.0e-6, "0.000001"),
            (1.0e-7, "1e-7"),
            (1.0e20, "100000000000000000000"),
            (1.0e21, "1e+21"),
            (-0.0, "0"),
        ] {
            let value = Value::Number(serde_json::Number::from_f64(number).unwrap());
            assert_eq!(
                Utf16JsonProjection::plain(value).to_json_string().unwrap(),
                expected
            );
        }
        assert_eq!(
            Utf16JsonProjection::parse("0.000001")
                .unwrap()
                .to_json_string()
                .unwrap(),
            "0.000001"
        );
        assert_eq!(
            Utf16JsonProjection::plain(Value::from(9_007_199_254_740_993_u64))
                .to_json_string()
                .unwrap(),
            "9007199254740992"
        );

        let mut object = Map::new();
        for (key, value) in [
            ("10", "ten"),
            ("2", "two"),
            ("01", "lead"),
            ("4294967295", "max-not-index"),
            ("4294967294", "max-index"),
            ("x", "letter"),
        ] {
            object.insert(key.into(), Value::String(value.into()));
        }
        assert_eq!(
            Utf16JsonProjection::plain(Value::Object(object))
                .to_json_string()
                .unwrap(),
            r#"{"2":"two","10":"ten","4294967294":"max-index","01":"lead","4294967295":"max-not-index","x":"letter"}"#
        );
        let exact =
            Utf16JsonProjection::parse(r#"{"10":"ten","2":"two","\ud800":"\ud801","a":"last"}"#)
                .unwrap();
        assert_eq!(
            exact.to_json_string().unwrap(),
            r#"{"2":"two","10":"ten","\ud800":"\ud801","a":"last"}"#
        );
    }

    #[test]
    fn pick_and_subprojection_rebase_exact_data() {
        let source = r#"{"subtype":"ui_message","client":"\ud800","data":{"key":"\ude00"}}"#;
        let value = Utf16JsonProjection::parse(source).unwrap();
        let selected = value.pick_object_fields(&["client", "data"]).unwrap();
        assert_eq!(selected.string_units("/client"), Some(vec![0xd800]));
        assert_eq!(selected.string_units("/data/key"), Some(vec![0xde00]));
        assert_eq!(
            selected
                .subprojection("/data")
                .unwrap()
                .string_units("/key"),
            Some(vec![0xde00])
        );
    }

    #[test]
    fn exact_json_parse_clone_validate_and_serialize_are_iterative() {
        for depth in [126usize, 384] {
            let source = format!("{}\"\\ud800\"{}", "[".repeat(depth), "]".repeat(depth));
            let pointer = "/0".repeat(depth);
            let projection = Utf16JsonProjection::parse(&source).unwrap();
            assert_eq!(projection.string_units(&pointer), Some(vec![0xd800]));
            assert_eq!(projection.to_json_string().unwrap(), source);

            let cloned = projection.clone();
            assert_eq!(cloned.to_json_string().unwrap(), source);
            let subtree = projection.subprojection("").unwrap();
            assert_eq!(subtree.string_units(&pointer), Some(vec![0xd800]));
            assert_eq!(subtree.to_json_string().unwrap(), source);

            let nested_object = format!(
                "{}\"\\ud800\"{}",
                "{\"k\":".repeat(depth),
                "}".repeat(depth)
            );
            let object_source = format!(r#"{{"\ud800":{nested_object}}}"#);
            let object = Utf16JsonProjection::parse(&object_source).unwrap();
            let exact_key = object.keys[0].placeholder.clone();
            let value_pointer = format!("/{exact_key}{}", "/k".repeat(depth));
            assert_eq!(object.string_units(&value_pointer), Some(vec![0xd800]));
            assert_eq!(object.to_json_string().unwrap(), object_source);
        }

        let plain_source = format!("{}false{}", "[".repeat(384), "]".repeat(384));
        let plain = Utf16JsonProjection::parse(&plain_source).unwrap();
        assert_eq!(plain.to_json_string().unwrap(), plain_source);

        let nested_keys = format!(
            "{}\"\\ud800\"{}",
            r#"{"\ud800":"#.repeat(384),
            "}".repeat(384)
        );
        let mut nested_keys_projection = Utf16JsonProjection::parse(&nested_keys).unwrap();
        assert_eq!(nested_keys_projection.keys.len(), 384);
        nested_keys_projection
            .rename_key_placeholders("__deepKey_")
            .unwrap();
        assert_eq!(
            nested_keys_projection.to_json_string().unwrap(),
            nested_keys
        );

        let malformed = format!("{}\"\\ud800\"{}false", "[".repeat(384), "]".repeat(384));
        assert!(Utf16JsonProjection::parse(&malformed).is_err());
    }
}

impl From<serde_json::Value> for Utf16JsonProjection {
    fn from(value: serde_json::Value) -> Self {
        Self::plain(value)
    }
}
