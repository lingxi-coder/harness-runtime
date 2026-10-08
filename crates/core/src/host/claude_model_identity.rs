//! Native 2.1.287 wu/qj shape recognition. This does not resolve served aliases.
use fancy_regex::Regex;
use std::sync::LazyLock;

static CURRENT: LazyLock<Regex> = LazyLock::new(|| {
    Regex::new(r"^claude-([a-z]+)-([0-9]{1,2})(?![0-9])(?:-([0-9]{1,2})(?![0-9]))?")
        .expect("native model regex")
});
static VERSION_FIRST: LazyLock<Regex> = LazyLock::new(|| {
    Regex::new(r"^claude-([0-9]{1,2})(?![0-9])(?:-([0-9]{1,2})(?![0-9]))?-([a-z]+)")
        .expect("native model regex")
});
static TRAILER: LazyLock<Regex> = LazyLock::new(|| {
    Regex::new(r"^(?:-fast|-latest)?(?:-v[0-9]{1,3}@[0-9]{8}|[-@][0-9]{8})?(?:-v[0-9]{1,3}(?::[0-9]{1,3})?)?$")
        .expect("native trailer regex")
});

#[must_use]
pub fn recognized(model: &str) -> bool {
    let mut value = super::effort::trim_js_whitespace(model).to_lowercase();
    if value.is_empty() || value.chars().any(super::effort::javascript_whitespace) {
        return false;
    }
    for marker in ["[1m]", "[2m]"] {
        if value.ends_with(marker) {
            value.truncate(value.len() - marker.len());
            break;
        }
    }
    let mut candidate = value.rsplit('/').next().unwrap_or(&value);
    if let Some((region, tail)) = candidate.split_once(".anthropic.") {
        if !["us", "eu", "apac", "jp", "au", "us-gov", "global"].contains(&region) {
            return false;
        }
        candidate = tail;
    } else if let Some(tail) = candidate.strip_prefix("anthropic.") {
        candidate = tail;
    }
    let base = CURRENT
        .captures(candidate)
        .ok()
        .flatten()
        .or_else(|| VERSION_FIRST.captures(candidate).ok().flatten());
    let Some(base) = base.and_then(|captures| captures.get(0)) else {
        return false;
    };
    let tail = &candidate[base.end()..];
    if !tail.is_empty() && !tail.starts_with(['-', '@']) {
        return false;
    }
    TRAILER.is_match(tail).unwrap_or(false)
}
