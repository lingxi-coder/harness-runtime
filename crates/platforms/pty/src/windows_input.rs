/// Stateful normalizer for bytes written to a Windows pseudoconsole.
///
/// `ConPTY` expects Enter as carriage return and Backspace as DEL. All other
/// bytes, including UTF-8 and terminal control sequences, pass unchanged.
#[derive(Default)]
pub(crate) struct WindowsTtyInputNormalizer {
    previous_was_cr: bool,
}

impl WindowsTtyInputNormalizer {
    pub(crate) fn normalize(&mut self, bytes: &[u8]) -> Vec<u8> {
        let mut normalized = Vec::with_capacity(bytes.len());
        for &byte in bytes {
            match byte {
                b'\x08' => normalized.push(b'\x7f'),
                b'\n' if !self.previous_was_cr => normalized.push(b'\r'),
                b'\n' => {}
                _ => normalized.push(byte),
            }
            self.previous_was_cr = byte == b'\r';
        }
        normalized
    }
}

#[cfg(test)]
mod tests {
    use super::WindowsTtyInputNormalizer;

    #[test]
    fn preserves_raw_control_bytes_except_terminal_key_normalization() {
        let mut normalizer = WindowsTtyInputNormalizer::default();
        assert_eq!(
            normalizer.normalize(b"a\x1b\x03\x04\x08\n"),
            b"a\x1b\x03\x04\x7f\r"
        );
    }

    #[test]
    fn collapses_crlf_across_writes() {
        let mut normalizer = WindowsTtyInputNormalizer::default();
        assert_eq!(normalizer.normalize(b"line\r"), b"line\r");
        assert!(normalizer.normalize(b"\n").is_empty());
    }
}
