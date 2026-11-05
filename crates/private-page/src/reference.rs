//! The reference an assistant writes on its own line to place a private page.
//!
//! ```text
//! ::lingxi-page{id="<id>" rev="<n>"}
//! ```

use std::fmt;

/// Start of a reference line; owned by `branding`.
pub const REFERENCE_PREFIX: &str = branding::PRIVATE_PAGE_REFERENCE_PREFIX;

/// Longest accepted page id.
pub const MAX_ID_LEN: usize = 64;

/// A page id: `[A-Za-z0-9_-]{1,64}`.
#[derive(Debug, Clone, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub struct PageId(String);

impl PageId {
    /// Validate `value` as an id.
    #[must_use]
    pub fn parse(value: &str) -> Option<Self> {
        let valid = !value.is_empty()
            && value.len() <= MAX_ID_LEN
            && value
                .bytes()
                .all(|byte| byte.is_ascii_alphanumeric() || byte == b'_' || byte == b'-');
        valid.then(|| Self(value.to_string()))
    }

    /// The id text.
    #[must_use]
    pub fn as_str(&self) -> &str {
        &self.0
    }
}

impl fmt::Display for PageId {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.0)
    }
}

/// One immutable revision of a private page.
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub struct PageRef {
    /// Page id, stable across revisions.
    pub id: PageId,
    /// 1-based revision number.
    pub revision: u32,
}

impl PageRef {
    /// The exact line an assistant writes to place this revision.
    #[must_use]
    pub fn reference_line(&self) -> String {
        format!(
            "{REFERENCE_PREFIX}id=\"{}\" rev=\"{}\"}}",
            self.id, self.revision
        )
    }
}

/// Parse a trimmed directive such as `::lingxi-page{id="a" rev="1"}`.
#[must_use]
pub fn parse_directive(directive: &str) -> Option<PageRef> {
    let body = directive
        .strip_prefix(REFERENCE_PREFIX)?
        .strip_suffix('}')?;
    let mut id = None;
    let mut revision = None;
    let mut rest = body.trim_matches(' ');
    while !rest.is_empty() {
        let (key, after) = rest.split_once("=\"")?;
        let (value, after) = after.split_once('"')?;
        match key {
            "id" if id.is_none() => id = Some(PageId::parse(value)?),
            "rev" if revision.is_none() => revision = Some(parse_revision(value)?),
            _ => return None,
        }
        rest = match after.strip_prefix(' ') {
            Some(next) => next.trim_start_matches(' '),
            None if after.is_empty() => after,
            None => return None,
        };
    }
    Some(PageRef {
        id: id?,
        revision: revision?,
    })
}

fn parse_revision(value: &str) -> Option<u32> {
    if value.is_empty() || value.starts_with('0') || !value.bytes().all(|b| b.is_ascii_digit()) {
        return None;
    }
    value.parse().ok()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn round_trips_and_rejects_a_public_url_shape() {
        let reference = PageRef {
            id: PageId::parse("notes").unwrap(),
            revision: 2,
        };
        let line = reference.reference_line();
        assert_eq!(line, "::lingxi-page{id=\"notes\" rev=\"2\"}");
        assert_eq!(parse_directive(&line), Some(reference));
        assert!(!line.contains("http"));
        assert!(parse_directive("::lingxi-page{id=\"../x\" rev=\"1\"}").is_none());
        assert!(parse_directive("::lingxi-page{id=\"a\" rev=\"0\"}").is_none());
    }
}
