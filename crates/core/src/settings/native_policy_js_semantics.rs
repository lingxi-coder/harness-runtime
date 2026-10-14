//! Detached Core carrier proposal for fixed Bun 1.4.3 policy semantics.
//!
//! This module contains only typed, exact UTF-16 observations. It executes no
//! JavaScript and has no Runtime dependency. The Runtime Bun worker is the only
//! producer; policy validators consume lookups by the original JS string units.

use std::collections::HashMap;

/// One source string and the source predicates applied to its raw and Zod-parsed
/// forms. The raw refinements cover Zod's continuing invalid-format branch and
/// schemas that do not call .url() before Rdt.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct NativePolicyUrlSemanticRow {
    pub input_units: Vec<u16>,
    pub raw_rdt_valid: bool,
    pub raw_archive_refine_valid: bool,
    /// None means Native XAr short-circuited before calling JQ.
    pub raw_archive_hostname_blocked: Option<bool>,
    pub zod_url: NativePolicyZodUrlOutcome,
}

/// A Zod URL result is a typed schema result, not a worker capability result.
/// An Invalid URL remains an ordinary row and cannot be confused with a failed
/// Bun startup, version check, or protocol exchange.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum NativePolicyZodUrlOutcome {
    Invalid,
    Valid {
        /// Zod returns trim(input) with HT/LF/CR removed when normalize=false.
        zod_value_units: Vec<u16>,
        /// Fields from the URL object constructed from the trimmed input.
        href_units: Vec<u16>,
        protocol_units: Vec<u16>,
        username_units: Vec<u16>,
        password_units: Vec<u16>,
        host_units: Vec<u16>,
        hostname_units: Vec<u16>,
        port_units: Vec<u16>,
        pathname_units: Vec<u16>,
        search_units: Vec<u16>,
        hash_units: Vec<u16>,
        origin_units: Vec<u16>,
        /// Rdt called by .url().refine(Rdt) after the Zod parsed string.
        rdt_zod_value_valid: bool,
        /// XAr called by .url().refine(XAr) after the Zod parsed string.
        archive_zod_refine_valid: bool,
        /// None means XAr short-circuited before calling JQ.
        archive_zod_hostname_blocked: Option<bool>,
    },
}

/// Native df(kV(input)) output, keyed by the original exact JS string.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct NativePolicyDisplayRow {
    pub input_units: Vec<u16>,
    pub output_units: Vec<u16>,
}

/// Ordered semantic observations with exact-code-unit indexes.
///
/// The rows retain first-seen order. Hash maps are lookup-only and never define
/// observable iteration order. Duplicate JS strings are rejected by code-unit
/// identity, including lone-surrogate strings.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct NativePolicyJsSemanticsRegistry {
    url_rows: Vec<NativePolicyUrlSemanticRow>,
    display_rows: Vec<NativePolicyDisplayRow>,
    url_index: HashMap<Vec<u16>, usize>,
    display_index: HashMap<Vec<u16>, usize>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum NativePolicyJsSemanticsRegistryError {
    DuplicateUrlKey {
        first_index: usize,
        duplicate_index: usize,
    },
    DuplicateDisplayKey {
        first_index: usize,
        duplicate_index: usize,
    },
}

impl NativePolicyJsSemanticsRegistry {
    pub fn from_rows(
        url_rows: Vec<NativePolicyUrlSemanticRow>,
        display_rows: Vec<NativePolicyDisplayRow>,
    ) -> Result<Self, NativePolicyJsSemanticsRegistryError> {
        let mut url_index = HashMap::with_capacity(url_rows.len());
        for (index, row) in url_rows.iter().enumerate() {
            if let Some(first_index) = url_index.insert(row.input_units.clone(), index) {
                return Err(NativePolicyJsSemanticsRegistryError::DuplicateUrlKey {
                    first_index,
                    duplicate_index: index,
                });
            }
        }

        let mut display_index = HashMap::with_capacity(display_rows.len());
        for (index, row) in display_rows.iter().enumerate() {
            if let Some(first_index) = display_index.insert(row.input_units.clone(), index) {
                return Err(NativePolicyJsSemanticsRegistryError::DuplicateDisplayKey {
                    first_index,
                    duplicate_index: index,
                });
            }
        }

        Ok(Self {
            url_rows,
            display_rows,
            url_index,
            display_index,
        })
    }

    pub fn url(&self, input_units: &[u16]) -> Option<&NativePolicyUrlSemanticRow> {
        self.url_index
            .get(input_units)
            .and_then(|index| self.url_rows.get(*index))
    }

    pub fn display(&self, input_units: &[u16]) -> Option<&NativePolicyDisplayRow> {
        self.display_index
            .get(input_units)
            .and_then(|index| self.display_rows.get(*index))
    }

    pub fn url_rows(&self) -> &[NativePolicyUrlSemanticRow] {
        &self.url_rows
    }

    pub fn display_rows(&self) -> &[NativePolicyDisplayRow] {
        &self.display_rows
    }
}

#[cfg(test)]
mod tests {
    use super::{
        NativePolicyDisplayRow, NativePolicyJsSemanticsRegistry,
        NativePolicyJsSemanticsRegistryError, NativePolicyUrlSemanticRow,
        NativePolicyZodUrlOutcome,
    };

    fn url_row(input_units: Vec<u16>, valid: bool) -> NativePolicyUrlSemanticRow {
        NativePolicyUrlSemanticRow {
            input_units,
            raw_rdt_valid: false,
            raw_archive_refine_valid: false,
            raw_archive_hostname_blocked: None,
            zod_url: if valid {
                NativePolicyZodUrlOutcome::Valid {
                    zod_value_units: vec![b'h' as u16],
                    href_units: vec![b'h' as u16],
                    protocol_units: Vec::new(),
                    username_units: Vec::new(),
                    password_units: Vec::new(),
                    host_units: Vec::new(),
                    hostname_units: Vec::new(),
                    port_units: Vec::new(),
                    pathname_units: Vec::new(),
                    search_units: Vec::new(),
                    hash_units: Vec::new(),
                    origin_units: Vec::new(),
                    rdt_zod_value_valid: false,
                    archive_zod_refine_valid: false,
                    archive_zod_hostname_blocked: None,
                }
            } else {
                NativePolicyZodUrlOutcome::Invalid
            },
        }
    }

    #[test]
    fn registry_preserves_rows_and_looks_up_exact_utf16_units() {
        let first = vec![b'a' as u16, 0xd800];
        let second = vec![b'a' as u16, 0xd801];
        let displays = vec![b'X' as u16];
        let registry = NativePolicyJsSemanticsRegistry::from_rows(
            vec![url_row(first.clone(), true), url_row(second.clone(), false)],
            vec![NativePolicyDisplayRow {
                input_units: displays.clone(),
                output_units: vec![b'Y' as u16],
            }],
        )
        .unwrap();

        assert_eq!(registry.url_rows()[0].input_units, first);
        assert_eq!(registry.url_rows()[1].input_units, second);
        assert!(matches!(
            registry.url(&[b'a' as u16, 0xd800]).unwrap().zod_url,
            NativePolicyZodUrlOutcome::Valid { .. }
        ));
        assert!(matches!(
            registry.url(&[b'a' as u16, 0xd801]).unwrap().zod_url,
            NativePolicyZodUrlOutcome::Invalid
        ));
        assert_eq!(
            registry.display(&displays).unwrap().output_units,
            vec![b'Y' as u16]
        );
    }

    #[test]
    fn registry_rejects_duplicate_exact_url_or_display_keys() {
        let key = vec![0xd800];
        let error = NativePolicyJsSemanticsRegistry::from_rows(
            vec![url_row(key.clone(), true), url_row(key.clone(), false)],
            Vec::new(),
        )
        .unwrap_err();
        assert_eq!(
            error,
            NativePolicyJsSemanticsRegistryError::DuplicateUrlKey {
                first_index: 0,
                duplicate_index: 1
            }
        );

        let error = NativePolicyJsSemanticsRegistry::from_rows(
            Vec::new(),
            vec![
                NativePolicyDisplayRow {
                    input_units: key.clone(),
                    output_units: Vec::new(),
                },
                NativePolicyDisplayRow {
                    input_units: key,
                    output_units: Vec::new(),
                },
            ],
        )
        .unwrap_err();
        assert_eq!(
            error,
            NativePolicyJsSemanticsRegistryError::DuplicateDisplayKey {
                first_index: 0,
                duplicate_index: 1
            }
        );
    }
}
