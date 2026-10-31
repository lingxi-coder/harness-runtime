//! Structural checks for a document published as a schema-2 static source.
//! Project publication validates its own manifest and source tree separately.

const MAX_INLINE_DOCUMENT_BYTES: usize = 25 * 1024 * 1024;

/// Validate the bounded inline document before a host stages index.html and
/// its schema-2 manifest. This does not grant browser or Agent capabilities.
pub fn validate_inline_document(document: &str) -> Result<(), String> {
    if document.len() > MAX_INLINE_DOCUMENT_BYTES {
        return Err(format!(
            "The document is {} bytes; the limit is {MAX_INLINE_DOCUMENT_BYTES} bytes (25 MiB).",
            document.len()
        ));
    }
    if document.trim().is_empty() {
        return Err("The document is empty.".into());
    }
    if document.contains('\0') {
        return Err("The document contains a NUL character.".into());
    }
    let lower = document.to_ascii_lowercase();
    if !["!doctype html", "html", "head", "body"]
        .iter()
        .any(|tag| tag_present(&lower, tag))
    {
        return Err("Write a complete HTML document, including its document elements.".into());
    }
    Ok(())
}

fn tag_present(lower: &str, tag: &str) -> bool {
    let open = format!("<{tag}");
    let bytes = lower.as_bytes();
    lower.match_indices(&open).any(|(at, _)| {
        bytes
            .get(at + open.len())
            .is_some_and(|&next| next == b'>' || next == b'/' || next.is_ascii_whitespace())
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn static_documents_keep_current_browser_and_agent_scripts() {
        for document in [
            "<!doctype html><p>hello</p>",
            "<HTML lang='zh'><body>聊天</body></HTML>",
            "<body><script type='module'>fetch('/agent'); new Worker('worker.js')</script></body>",
        ] {
            validate_inline_document(document).unwrap();
        }
    }

    #[test]
    fn fragments_and_invalid_document_bytes_are_rejected() {
        for document in [
            "",
            " \n",
            "<!doctype html>\0",
            "<div>fragment</div>",
            "<header>fragment</header><bodyguard>",
        ] {
            assert!(validate_inline_document(document).is_err(), "{document:?}");
        }
    }

    #[test]
    fn inline_limit_is_enforced_before_staging() {
        let mut document = "<!doctype html>".to_owned();
        document.extend(std::iter::repeat_n(
            ' ',
            MAX_INLINE_DOCUMENT_BYTES - document.len(),
        ));
        validate_inline_document(&document).unwrap();
        document.push(' ');
        assert!(validate_inline_document(&document).is_err());
    }
}
