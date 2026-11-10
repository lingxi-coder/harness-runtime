//! Native 2.1.319 `LAr` report framing and `KFn` section attribution.
//!
//! The public artifact enables provenance and the note-only handback branch by
//! default. Source UTF-16 units remain exact; only the section hash uses Node's
//! UTF-8 projection, which replaces unmatched surrogates.

use sha2::{Digest, Sha256};

pub(super) fn section_hash(content: &[Vec<u16>]) -> String {
    let mut hash = Sha256::new();
    hash.update(content.len().to_string().as_bytes());
    for text in content {
        hash.update(format!(":{}:", text.len()).as_bytes());
        hash.update(String::from_utf16_lossy(text).as_bytes());
    }
    format!("{:x}", hash.finalize())[..16].to_owned()
}

fn indent(text: &[u16]) -> Vec<u16> {
    let mut output = Vec::with_capacity(text.len() + 2);
    output.extend([u16::from(b' '), u16::from(b' ')]);
    let mut index = 0;
    while index < text.len() {
        let unit = text[index];
        if matches!(
            unit,
            0x0a..=0x0d | 0x1c..=0x1e | 0x85 | 0x2028 | 0x2029
        ) {
            if unit == 0x0d && text.get(index + 1) == Some(&0x0a) {
                index += 1;
            }
            output.extend([u16::from(b'\n'), u16::from(b' '), u16::from(b' ')]);
        } else {
            output.push(unit);
        }
        index += 1;
    }
    output
}

fn join(blocks: &[Vec<u16>]) -> Vec<u16> {
    let mut output = Vec::new();
    for (index, block) in blocks.iter().enumerate() {
        if index > 0 {
            output.push(u16::from(b'\n'));
        }
        output.extend_from_slice(block);
    }
    output
}

pub(super) fn frame_content(
    content: &[Vec<u16>],
    note_count: usize,
    tail_count: usize,
    trusted_hash: Option<&str>,
) -> Vec<Vec<u16>> {
    let trusted_sections = note_count
        .checked_add(tail_count)
        .filter(|count| *count <= content.len())
        .is_some_and(|count| count == 0 || trusted_hash == Some(section_hash(content).as_str()));
    let (notes, body, tail) = if trusted_sections {
        (
            &content[..note_count],
            &content[note_count..content.len() - tail_count],
            &content[content.len() - tail_count..],
        )
    } else {
        (&content[..0], content, &content[..0])
    };

    let mut parts: Vec<Vec<u16>> = notes.iter().chain(tail).map(|text| indent(text)).collect();
    if !body.is_empty() || parts.is_empty() {
        let mut report = join(body);
        if report.is_empty() {
            report.extend("(no text output)".encode_utf16());
        }
        let mut framed: Vec<u16> = lingxi_core::host::handback::HANDBACK_FRAME
            .encode_utf16()
            .collect();
        framed.push(u16::from(b'\n'));
        framed.extend(indent(&report));
        parts.push(framed);
    }
    vec![join(&parts)]
}

#[cfg(test)]
mod tests {
    use super::*;

    fn units(value: &serde_json::Value) -> Vec<u16> {
        value
            .as_array()
            .unwrap()
            .iter()
            .map(|unit| u16::try_from(unit.as_u64().unwrap()).unwrap())
            .collect()
    }

    #[test]
    fn result_framing_matches_native_319_artifact() {
        let fixture: serde_json::Value = serde_json::from_str(include_str!(
            "../tests/fixtures/agent_result_framing_319.json"
        ))
        .unwrap();
        for case in fixture["cases"].as_array().unwrap() {
            let content: Vec<_> = case["contentUnits"]
                .as_array()
                .unwrap()
                .iter()
                .map(units)
                .collect();
            assert_eq!(
                section_hash(&content),
                case["computedHash"].as_str().unwrap(),
                "native section hash: {}",
                case["name"]
            );
            let output = frame_content(
                &content,
                usize::try_from(case["noteCount"].as_u64().unwrap()).unwrap(),
                usize::try_from(case["tailCount"].as_u64().unwrap()).unwrap(),
                case["sectionHash"].as_str(),
            );
            let expected: Vec<_> = case["outputUnits"]
                .as_array()
                .unwrap()
                .iter()
                .map(units)
                .collect();
            assert_eq!(output, expected, "native framing: {}", case["name"]);
        }
    }

    #[test]
    fn overflowing_section_counts_cannot_remove_report_content() {
        let content = vec!["report".encode_utf16().collect()];
        assert_eq!(
            frame_content(&content, usize::MAX, 1, None),
            frame_content(&content, 0, 0, None)
        );
    }
}
