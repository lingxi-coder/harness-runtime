# Native 2.1.293 tool-registration request oracle

Set the pinned executable explicitly, then run this script from any directory:

    CLAUDE_NATIVE_BINARY=/path/to/claude python3 /path/to/harness-runtime/docs/parity/current-alignment/fixtures/mod-tool-register/run-native.py

The shared input for Native and Harness replay is cases.json. It fixes the
plugin tool schema, session ID, model, prompt, and the omitted/false/true
isDeferred cases. The runner verifies the executable SHA-256 before use,
isolates HOME and CLAUDE_CONFIG_DIR, limits settings to the project, and routes
model traffic to a loopback stub with a dummy key. Generated raw request bodies,
exact tools slices, stdout, stderr, and receipt go to run-output/; the checked-in
native-baseline/ is immutable evidence. Nothing is relayed upstream.

These observations cover only the Native public plugin path. They do not
constitute a Harness pass.
