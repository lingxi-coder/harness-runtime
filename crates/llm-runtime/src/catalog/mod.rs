//! Product projections of the independent client's model catalog.
mod upstream;
pub use upstream::{builtin_presets, catalog_from_profiles, BuiltinCatalog};

pub(crate) use upstream::enrich_context_metadata;
