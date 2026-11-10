//! Decompressed body and SSE event budgets from the shipped MCP fetch wrapper.
pub(crate) const MESSAGE_BYTES: usize = 16 * 1024 * 1024;

#[derive(Debug)]
pub(crate) struct Overflow {
    pub limit: usize,
    pub event: bool,
}
impl Overflow {
    pub(crate) fn diagnostic(&self) -> String {
        let mb = (self.limit as f64 / 1024.0 / 1024.0).round() as u64;
        if self.event {
            format!("sent more than {mb}MB in one event of an event stream (no blank line ended it), so Claude Code stopped reading it.")
        } else {
            format!("sent a response larger than {mb}MB (counted after decompression), so Claude Code stopped reading it.")
        }
    }
}

pub(crate) struct Budget {
    event_limit: usize,
    total_limit: Option<usize>,
    total: usize,
    event_bytes: usize,
    line_bytes: usize,
    cr: bool,
}
impl Budget {
    pub(crate) fn new(event_limit: usize, total_limit: Option<usize>) -> Self {
        Self {
            event_limit,
            total_limit,
            total: 0,
            event_bytes: 0,
            line_bytes: 0,
            cr: false,
        }
    }
    pub(crate) fn push(&mut self, chunk: &[u8]) -> Result<(), Overflow> {
        self.total = self.total.saturating_add(chunk.len());
        if self.total_limit.is_some_and(|limit| self.total > limit) {
            return Err(Overflow {
                limit: self.total_limit.unwrap(),
                event: false,
            });
        }
        if self
            .total_limit
            .is_some_and(|limit| limit <= self.event_limit)
        {
            return Ok(());
        }
        let mut blank = None;
        for (index, &byte) in chunk.iter().enumerate() {
            if self.cr && byte == b'\n' {
                self.cr = false;
                continue;
            }
            self.cr = false;
            if byte == b'\r' || byte == b'\n' {
                if self.line_bytes == 0 {
                    blank = Some(index);
                }
                self.line_bytes = 0;
                self.cr = byte == b'\r';
            } else {
                self.line_bytes += 1;
            }
        }
        self.event_bytes = blank.map_or_else(
            || self.event_bytes.saturating_add(chunk.len()),
            |index| chunk.len() - 1 - index,
        );
        if self.event_bytes > self.event_limit {
            return Err(Overflow {
                limit: self.event_limit,
                event: true,
            });
        }
        Ok(())
    }
}

pub(crate) fn is_event_stream(value: &str) -> bool {
    let value = value.trim_start().to_ascii_lowercase();
    value.strip_prefix("text/event-stream").is_some_and(|rest| {
        let rest = rest.trim_start();
        rest.is_empty() || rest.starts_with(';')
    })
}

#[cfg(test)]
mod tests {
    #[test]
    fn native_322_body_and_event_budgets_match_shipped_transform() {
        let fixture: serde_json::Value = serde_json::from_str(include_str!(
            "../tests/fixtures/mcp_remote_gap_2_1_322.json"
        ))
        .unwrap();
        for case in fixture["bodyBudgetCases"].as_array().unwrap() {
            let mut budget = super::Budget::new(
                case["eventLimit"].as_u64().unwrap() as usize,
                case["totalLimit"].as_u64().map(|value| value as usize),
            );
            let mut failure = None;
            for chunk in case["chunks"].as_array().unwrap() {
                if let Err(error) = budget.push(chunk.as_str().unwrap().as_bytes()) {
                    failure = Some(error.diagnostic());
                    break;
                }
            }
            assert_eq!(failure.as_deref(), case["error"].as_str(), "{case}");
        }
    }
}
