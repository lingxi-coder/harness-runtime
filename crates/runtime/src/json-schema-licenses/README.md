# Bundled Ajv license and provenance

`ajv-8.18.0-profiles.bundle.mjs` is built from cached upstream npm packages
with Bun 1.3.11 (`bun build --target=bun --minify`). It contains the Draft-07
Ajv constructor, Ajv2020 constructor, and `ajv-formats` plugin.

| Package | Version used | License |
| --- | ---: | --- |
| `ajv` | 8.18.0 | MIT |
| `ajv-formats` | 3.0.1 | MIT |
| `fast-deep-equal` | 3.1.3 | MIT |
| `fast-uri` | 3.1.0 | BSD-3-Clause |
| `json-schema-traverse` | 1.0.0 | MIT |
| `require-from-string` | 2.0.2 | MIT |

The Native 2.1.292 bundle imports Ajv's 2020-12 constructor for MCP output
schemas and calls `ajv-formats` before compilation. The Native Ajv chunk has
the core initializer `this.formats = {}`. Cached upstream Ajv 8.18.0 has the
same initializer; cached upstream 8.20.0 changes it to
`Object.create(null)`. This rules out the cached 8.20.0 source as an exact
match, but does not prove Native's exact Ajv semver. The Native binary has no
Ajv version string in this chunk. The vendored version is therefore a concrete
upstream candidate, not a verified exact-version identification. The
`ajv-formats` semver is also not embedded in the Native chunk.

Native source windows and their hashes are in the packet proof manifest. The
implementation vendors upstream npm package code under the listed licenses;
it does not copy the Native bundle's proprietary chunk.
