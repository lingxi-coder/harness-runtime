//! Modified-djb2 / SDBM-style hash — 1:1 port of `claude-code/src/utils/hash.ts:7-13`.
//! Initial value 0, step `(h * 31) + c`, signed-i32 wrap on overflow,
//! iterates over UTF-16 code units (NOT UTF-8 bytes).

/// Returns claude-code's `djb2Hash(str)` as a signed i32 (caller chooses encoding).
#[must_use]
pub fn djb2_hash(s: &str) -> i32 {
    let mut hash: i32 = 0;
    for unit in s.encode_utf16() {
        // Promote u16 -> i32, then mirror `((h << 5) - h + c) | 0`.
        // i32::wrapping_* gives us the same wrap-around semantics as `| 0`.
        hash = hash
            .wrapping_shl(5)
            .wrapping_sub(hash)
            .wrapping_add(i32::from(unit));
    }
    hash
}
