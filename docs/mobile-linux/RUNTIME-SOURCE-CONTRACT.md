# Mobile runtime source boundary

The runtime Git revision pinned by Cargo owns the rootfs schemas and policy under
`docs/mobile-linux`. The Local App runtime seed's pins, policy, SBOM, templates and plugin
assets, and the scripts that verify and stage them (`scripts/runtime/`), belong to the
`local-app-builder` repository the product pins separately. Runtime checks never require a
LingXi `clients` checkout.

Rootfs and node-module tools accept `--output` / `--output-dir`; rootfs builds
also accept `--cache-dir` (or `LINGXI_NODE_SOURCE_CACHE`). Standalone defaults
are beneath this repository's `build/`. Callers using an immutable Git checkout
must provide external output/cache directories. Staging accepts an explicit
external output directory and rejects writes into source directories.

LingXi owns native launcher/iSH patches, source hashes, Swift launch wiring,
Android mksh/toybox packaging and iSH application wrappers. Its independent
`verify-local-app-host.py` checks `local-app-native-policy.json`; native source
pins live in `mobile-linux-native-pins.json`. These are separate required gates,
not optional checks conditional on the presence of client paths.

The existing release rootfs digest gap remains: synthetic tests demonstrate
verification behavior, while release verification requires committed archive
hashes and the complete pinned APK closure. Native device builds and container
rootfs builds are separate acceptance steps.
