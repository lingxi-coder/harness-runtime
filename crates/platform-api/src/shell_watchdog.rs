//! Shared shell stall detection; platform memory-pressure probes stay in adapters.
use std::collections::VecDeque;
use std::time::Duration;
use tokio::time::Instant;
const STALL_INTERVAL: Duration = Duration::from_secs(45);
const TAIL_BYTES: usize = 1024;
pub struct ShellWatchdog {
    total: u64,
    observed_total: u64,
    last_growth: Instant,
    tail: VecDeque<u8>,
    notified: bool,
}

impl ShellWatchdog {
    pub fn new(now: Instant) -> Self {
        Self {
            total: 0,
            observed_total: 0,
            last_growth: now,
            tail: VecDeque::with_capacity(TAIL_BYTES),
            notified: false,
        }
    }

    pub fn observe(&mut self, bytes: &[u8]) {
        self.total = self.total.saturating_add(bytes.len() as u64);
        if bytes.len() >= TAIL_BYTES {
            self.tail.clear();
            self.tail.extend(&bytes[bytes.len() - TAIL_BYTES..]);
        } else {
            let excess = (self.tail.len() + bytes.len()).saturating_sub(TAIL_BYTES);
            self.tail.drain(..excess);
            self.tail.extend(bytes);
        }
    }

    pub fn poll(&mut self, now: Instant) -> Option<String> {
        if self.notified {
            return None;
        }
        if self.total > self.observed_total {
            self.observed_total = self.total;
            self.last_growth = now;
            return None;
        }
        if now.duration_since(self.last_growth) < STALL_INTERVAL {
            return None;
        }
        let bytes: Vec<_> = self.tail.iter().copied().collect();
        let tail = String::from_utf8_lossy(&bytes).into_owned();
        if !interactive_prompt(&tail) {
            self.last_growth = now;
            return None;
        }
        self.notified = true;
        Some(tail)
    }
}

// The oracle only examines the last nonblank output line. ASCII folding is
// sufficient for its seven English, case-insensitive prompt expressions.
pub fn interactive_prompt(tail: &str) -> bool {
    let lower = tail
        .trim_end()
        .rsplit('\n')
        .next()
        .unwrap_or("")
        .to_ascii_lowercase();
    if [
        "(y/n)",
        "[y/n]",
        "(yes/no)",
        "press any key",
        "press enter",
        "continue?",
        "overwrite?",
    ]
    .iter()
    .any(|pattern| lower.contains(pattern))
    {
        return true;
    }
    lower.ends_with('?')
        && ["do you", "would you", "shall i", "are you sure", "ready to"]
            .iter()
            .any(|phrase| {
                lower.match_indices(phrase).any(|(offset, phrase)| {
                    let word = |b: u8| b.is_ascii_alphanumeric() || b == b'_';
                    (offset == 0 || !word(lower.as_bytes()[offset - 1]))
                        && lower
                            .as_bytes()
                            .get(offset + phrase.len())
                            .is_none_or(|b| !word(*b))
                })
            })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn only_last_line_and_oracle_prompt_patterns_match() {
        for prompt in [
            "(Y/n)",
            "[y/N]",
            "(YES/no)",
            "Do you want to run?  ",
            "Would you?",
            "Shall I proceed?",
            "Are you sure?",
            "Ready to start?",
            "Press any key",
            "Press Enter",
            "Continue?",
            "Overwrite?",
        ] {
            assert!(interactive_prompt(prompt), "{prompt}");
        }
        for output in [
            "Continue?\ncompiling",
            "Working...",
            "undo you agree?",
            "Do your job?",
            "Do you run",
        ] {
            assert!(!interactive_prompt(output), "{output}");
        }
    }

    #[test]
    fn growth_resets_clock_and_prompt_notifies_once_after_45_seconds() {
        let now = Instant::now();
        let mut watch = ShellWatchdog::new(now);
        watch.observe(b"Continue?");
        assert!(watch.poll(now + Duration::from_secs(5)).is_none());
        assert!(watch.poll(now + Duration::from_secs(49)).is_none());
        assert_eq!(
            watch.poll(now + Duration::from_secs(50)).as_deref(),
            Some("Continue?")
        );
        assert!(watch.poll(now + Duration::from_secs(500)).is_none());
    }

    #[test]
    fn quiet_non_prompt_is_not_a_stall_and_tail_is_bounded() {
        let now = Instant::now();
        let mut watch = ShellWatchdog::new(now);
        watch.observe(&vec![b'x'; 2000]);
        assert_eq!(watch.tail.len(), TAIL_BYTES);
        assert!(watch.poll(now).is_none());
        assert!(watch.poll(now + STALL_INTERVAL).is_none());
        watch.observe(b"\nPress Enter");
        assert!(watch.poll(now + Duration::from_secs(50)).is_none());
        assert!(watch.poll(now + Duration::from_secs(94)).is_none());
        assert!(watch.poll(now + Duration::from_secs(95)).is_some());
    }
}
