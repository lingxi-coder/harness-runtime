//! Native MCP eventsource-parser framing, after TextDecoderStream decoding.
//! Retry fields are observable before a data event, and ids are local to events.

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct Event {
    pub event: String,
    pub data: String,
    pub id: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum Item {
    Event(Event),
    Retry(u64),
}

#[derive(Default)]
pub(crate) struct Decoder {
    line: Vec<u8>,
    pending_cr: bool,
    started: bool,
    event: String,
    data: Vec<String>,
    id: Option<String>,
}

impl Decoder {
    pub(crate) fn push(&mut self, bytes: &[u8]) -> Vec<Item> {
        let mut output = Vec::new();
        for &byte in bytes {
            if self.pending_cr {
                self.pending_cr = false;
                self.line(&mut output);
                if byte == b'\n' {
                    continue;
                }
            }
            match byte {
                b'\r' => self.pending_cr = true,
                b'\n' => self.line(&mut output),
                byte => self.line.push(byte),
            }
        }
        output
    }

    fn line(&mut self, output: &mut Vec<Item>) {
        // Keeping partial lines as bytes also preserves a UTF-8 character split
        // across network chunks. TextDecoder replaces malformed sequences.
        let bytes = std::mem::take(&mut self.line);
        let text = String::from_utf8_lossy(&bytes);
        let text = if self.started {
            text.as_ref()
        } else {
            self.started = true;
            text.strip_prefix('\u{feff}').unwrap_or(&text)
        };
        if text.is_empty() {
            if !self.data.is_empty() {
                output.push(Item::Event(Event {
                    event: std::mem::take(&mut self.event),
                    data: self.data.join("\n"),
                    id: self.id.take(),
                }));
            }
            self.event.clear();
            self.data.clear();
            self.id = None;
            return;
        }
        if text.starts_with(':') {
            return;
        }
        let (field, value) = text.split_once(':').unwrap_or((text, ""));
        let value = value.strip_prefix(' ').unwrap_or(value);
        match field {
            "data" => self.data.push(value.to_owned()),
            "event" => self.event = value.to_owned(),
            "id" => self.id = (!value.contains('\0')).then(|| value.to_owned()),
            "retry" if !value.is_empty() && value.bytes().all(|byte| byte.is_ascii_digit()) => {
                if let Ok(value) = value.parse() {
                    output.push(Item::Retry(value));
                }
            }
            _ => {}
        }
    }
}

#[cfg(test)]
mod tests {
    #[test]
    fn native_322_sse_parser_matches_retry_id_and_chunk_boundaries() {
        let fixture: serde_json::Value = serde_json::from_str(include_str!(
            "../tests/fixtures/mcp_remote_gap_2_1_322.json"
        ))
        .unwrap();
        for case in fixture["sseCases"].as_array().unwrap() {
            let text = case["chunks"]
                .as_array()
                .unwrap()
                .iter()
                .map(|chunk| chunk.as_str().unwrap())
                .collect::<String>();
            for width in 1..=8 {
                let mut decoder = super::Decoder::default();
                let actual = text
                    .as_bytes()
                    .chunks(width)
                    .flat_map(|chunk| decoder.push(chunk))
                    .map(|item| match item {
                        super::Item::Retry(value) => serde_json::json!({"retry":value}),
                        super::Item::Event(event) => {
                            serde_json::json!({"event":event.event,"data":event.data,"id":event.id})
                        }
                    })
                    .collect::<Vec<_>>();
                assert_eq!(
                    actual,
                    case["items"].as_array().unwrap().clone(),
                    "width {width}: {text:?}"
                );
            }
        }
    }
}
