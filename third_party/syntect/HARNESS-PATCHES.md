# Runtime syntax cache codec

Source: crates.io `syntect` 5.3.0, archive SHA-256 `656b45c05d95a5704399aeef6bd0ddec7b2b3531b7c9e900abbf7c4d2190c925`.
The original MIT license and upstream source files are retained.
Local package version: `5.3.0-harness.1`.

The dump codec in `src/dumps.rs` uses Ciborium 0.2.2 instead of unmaintained
Bincode. The four bundled cache files are re-encoded, including every lazily
loaded syntax context. Grammar definitions, parser behavior, theme names,
colors, and public highlighting types are preserved. There is no legacy
runtime decoder and no advisory ignore.

Regenerate assets with `python3 scripts/syntect/regenerate_assets.py` from the
repository root. The temporary exporter verifies the upstream archive checksum
before using the old decoder on its immutable bundled data. The old decoder is
not a runtime or workspace dependency.

`highlight-oracle.json` records every upstream syntax's spans for an identical
multiline corpus and the complete theme data. The client regression compares
the migrated cache against this upstream-generated oracle.
