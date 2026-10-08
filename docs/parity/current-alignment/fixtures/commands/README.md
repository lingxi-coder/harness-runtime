# Native command, Skill and session oracle

From the acceptance directory, run:

    CLAUDE_NATIVE_BINARY=/path/to/claude python3 fixtures/commands/run-native.py

cases.json fixes the Native version, prompts/code units, session IDs, two local
Skills, model, cwd/config layout, dummy API key and loopback response. The
runner verifies the pinned SHA-256, isolates HOME and Claude config, uses
project-only settings, and writes original request bodies, session JSONL,
stdout/stderr and a receipt under run-output/.

Raw request/session files from the captured CLI include injected user-global
instruction and account context, so they remain local ignored artifacts. The
checked-in native-baseline retains stdout/stderr and the hash-bearing receipt;
compare Native and Harness raw request/session artifacts during the same local
candidate run. Nothing is relayed upstream. The lone-surrogate case keeps its
UTF-16LE stream-json input fixed and tests persistence separately from Native's
final Claude Messages body normalization.
