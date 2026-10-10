//! Exact UTF-16 previews shared by native session-agent consumers.
use crate::protocol::text::ClientTextDto;

/// First useful line, capped at 160 Unicode scalars while preserving isolated UTF-16 units.
pub fn summarize_activity_line(units: &[u16]) -> Option<ClientTextDto> {
    for line in units.split(|unit| *unit == u16::from(b'\n')) {
        // `str::lines()` removes the CR from CRLF before trimming each line.
        let line = line.strip_suffix(&[u16::from(b'\r')]).unwrap_or(line);
        let line = trim_utf16_whitespace(line);
        if line.is_empty() {
            continue;
        }
        if ["completed", "cancelled", "failed", "idle", "running"]
            .iter()
            .any(|marker| equals_ascii(line, marker))
        {
            return None;
        }
        return Some(ClientTextDto::from_code_units(
            take_utf16_characters(line, 160).to_vec(),
        ));
    }
    None
}

fn trim_utf16_whitespace(units: &[u16]) -> &[u16] {
    let mut start = 0;
    while start < units.len() {
        let (is_whitespace, width) = utf16_scalar(units, start);
        if !is_whitespace {
            break;
        }
        start += width;
    }

    let mut cursor = start;
    let mut end = start;
    while cursor < units.len() {
        let (is_whitespace, width) = utf16_scalar(units, cursor);
        if !is_whitespace {
            end = cursor + width;
        }
        cursor += width;
    }
    &units[start..end]
}

fn take_utf16_characters(units: &[u16], limit: usize) -> &[u16] {
    let mut cursor = 0;
    let mut count = 0;
    while cursor < units.len() && count < limit {
        cursor += utf16_scalar_width(units, cursor);
        count += 1;
    }
    &units[..cursor]
}

fn utf16_scalar(units: &[u16], index: usize) -> (bool, usize) {
    let width = utf16_scalar_width(units, index);
    let scalar = match width {
        2 => {
            let high = u32::from(units[index]) - 0xd800;
            let low = u32::from(units[index + 1]) - 0xdc00;
            0x10000 + (high << 10) + low
        }
        _ => u32::from(units[index]),
    };
    (
        char::from_u32(scalar).is_some_and(char::is_whitespace),
        width,
    )
}

fn utf16_scalar_width(units: &[u16], index: usize) -> usize {
    usize::from(
        (0xd800..=0xdbff).contains(&units[index])
            && units
                .get(index + 1)
                .is_some_and(|unit| (0xdc00..=0xdfff).contains(unit)),
    ) + 1
}

fn equals_ascii(units: &[u16], value: &str) -> bool {
    units.len() == value.len()
        && units
            .iter()
            .copied()
            .zip(value.bytes())
            .all(|(unit, byte)| unit == u16::from(byte))
}

#[cfg(test)]
mod activity_tests {
    use super::summarize_activity_line;

    #[test]
    fn activity_summary_trims_whitespace_without_replacing_lone_surrogates() {
        let summary = summarize_activity_line(&[
            u16::from(b' '),
            u16::from(b'Q'),
            0xd800,
            u16::from(b'!'),
            u16::from(b' '),
        ])
        .unwrap();

        assert_eq!(
            summary.code_units,
            [u16::from(b'Q'), 0xd800, u16::from(b'!')]
        );
    }

    #[test]
    fn activity_summary_counts_a_surrogate_pair_as_one_character() {
        let mut source = vec![u16::from(b'a'); 159];
        source.extend("🧭".encode_utf16());
        source.push(u16::from(b'b'));

        let summary = summarize_activity_line(&source).unwrap();

        assert_eq!(summary.code_units.len(), 161);
        assert_eq!(
            &summary.code_units[159..],
            "🧭".encode_utf16().collect::<Vec<_>>()
        );
    }

    #[test]
    fn activity_summary_uses_first_nonempty_crlf_line_and_skips_status_markers() {
        let first_line = " \r\n  activity  \r\n later"
            .encode_utf16()
            .collect::<Vec<_>>();
        let marker = "\r\nrunning\r\nactivity".encode_utf16().collect::<Vec<_>>();

        assert_eq!(
            summarize_activity_line(&first_line).unwrap().code_units,
            "activity".encode_utf16().collect::<Vec<_>>()
        );
        assert!(summarize_activity_line(&marker).is_none());
    }
}
