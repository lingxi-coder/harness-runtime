use lingxi_core::types::utf16_json::Utf16JsonProjection;

pub(crate) type Source = Utf16JsonProjection;

pub(crate) fn generated(value: serde_json::Value) -> Source {
    Source::from_serde_value(&value).expect("generated web-tool metadata is valid JSON")
}

pub(crate) fn object<const N: usize>(fields: [(&str, Source); N]) -> Source {
    Source::object(
        fields
            .into_iter()
            .map(|(key, value)| (key.into(), value))
            .collect(),
    )
    .expect("fixed web-tool field names are valid")
}

pub(crate) fn text(units: Vec<u16>) -> Source {
    Source::root_string(String::from_utf16_lossy(&units), units)
        .expect("display derives from source")
}

pub(crate) fn affix(prefix: &str, source: &Source, suffix: &str) -> Source {
    text(
        prefix
            .encode_utf16()
            .chain(source.string_units("").expect("typed source string"))
            .chain(suffix.encode_utf16())
            .collect(),
    )
}

pub(crate) fn string_field(input: &Source, name: &str) -> Result<Source, String> {
    let field = input
        .subprojection(&format!("/{name}"))
        .map_err(|_| format!("missing field `{name}`"))?;
    field
        .string_units("")
        .ok_or_else(|| format!("field `{name}` must be a string"))?;
    Ok(field)
}

pub(crate) fn optional_string(input: &Source, name: &str) -> Result<Option<Source>, String> {
    if input
        .at_pointer(&format!("/{name}"))
        .is_none_or(|value| value.is_null())
    {
        Ok(None)
    } else {
        string_field(input, name).map(Some)
    }
}

pub(crate) fn optional_strings(input: &Source, name: &str) -> Result<Option<Vec<Source>>, String> {
    let pointer = format!("/{name}");
    if input
        .at_pointer(&pointer)
        .is_none_or(|value| value.is_null())
    {
        return Ok(None);
    }
    let len = input
        .array_len(&pointer)
        .ok_or_else(|| format!("field `{name}` must be an array"))?;
    (0..len)
        .map(|index| string_field(input, &format!("{name}/{index}")))
        .collect::<Result<Vec<_>, _>>()
        .map(Some)
}

pub(crate) fn js_whitespace(unit: u16) -> bool {
    matches!(unit, 0x0009..=0x000d | 0x0020 | 0x00a0 | 0x1680 | 0x2000..=0x200a | 0x2028 | 0x2029 | 0x202f | 0x205f | 0x3000 | 0xfeff)
}
