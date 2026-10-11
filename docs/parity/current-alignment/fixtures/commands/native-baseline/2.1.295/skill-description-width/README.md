# Native 2.1.295 Skill description-width fixtures

These four minimal fixtures replay the existing 2.1.293 `skill-description-width` inputs through pinned Native Claude Code 2.1.295. Each case contains only the original `SKILL.md` bytes and the exact raw byte slice of the target model-facing listing line. The full Native request, response, stdout/stderr, environment, and receipts remain in `acceptance/skill-description-295/capture/` and are not duplicated here.

The captured Native line is from `$.messages[0].content[3].text`. For every case, the original raw JSON-string token was parsed as a string, the unique `- native-width-probe: ` line was UTF-8 encoded, and its bytes matched the raw line fragment exactly; the fragment also appeared once in the token. Thus the Native line has no JSON escaping difference at that captured string boundary.

`manifest.json` pins each copied file's byte length and SHA-256, the request hash, and the native binary and runner fingerprints. The runtime test captures the model-facing `ConversationMessage` immediately before the API client, strictly decodes only the target line's UTF-16 units, then byte-compares its UTF-8 bytes with the raw Native line. This does **not** compare or claim full HTTP-wire/request parity. It does not assert reminder wrappers, other skill entries, system messages, tool schemas, or TUI geometry.
