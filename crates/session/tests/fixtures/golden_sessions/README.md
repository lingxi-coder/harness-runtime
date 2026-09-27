# Golden session fixtures

Captured (or synthetically authored per the M5-07 byte-locks) from claude-code.

## Token substitution

Fixture lines use four token classes:

- `<UUID-N>` — placeholder UUIDs. Tests substitute fixed test UUIDs:
  - `<UUID-1>` -> `aaaaaaaa-aaaa-aaaa-aaaa-aaaaaaaaaaaa`
  - `<UUID-2>` -> `bbbbbbbb-bbbb-bbbb-bbbb-bbbbbbbbbbbb`
  - `<UUID-3>` -> `cccccccc-cccc-cccc-cccc-cccccccccccc`
  - `<UUID-4>` -> `dddddddd-dddd-dddd-dddd-dddddddddddd`
  - `<UUID-5>` -> `eeeeeeee-eeee-eeee-eeee-eeeeeeeeeeee`
- `<SESSION-N>` — session UUID. Tests substitute:
  - `<SESSION-1>` -> `11111111-2222-3333-4444-555555555555`
- `<TS-N>` — ISO-8601 timestamp. Tests substitute:
  - `<TS-1>` -> `2026-05-25T14:30:00.000Z`
  - `<TS-2>` -> `2026-05-25T14:30:01.000Z`
  - `<TS-3>` -> `2026-05-25T14:30:02.000Z`
  - `<TS-4>` -> `2026-05-25T14:30:03.000Z`
  - `<TS-5>` -> `2026-05-25T14:30:04.000Z`

## Files

| File | Source | Description |
|---|---|---|
| `single_turn_no_tools.jsonl` | Synthetic, schema per `claude-code/src/types/logs.ts:8-17, 221-231` and `types/message.ts:72-89, 95-100` | 1 user prompt + 1 assistant end_turn response. |
| `multi_turn_with_tools.jsonl` | Synthetic, schema-locked | 2 user prompts + 1 assistant `tool_use` (Read tool) + 1 user `tool_result` + 1 assistant `end_turn`. |
| `compacted_session.jsonl` | Synthetic, schema-locked | `system` `compact_boundary` line between two user/assistant pairs (M5-08 surface). |

## RE-CAPTURE schedule

Once `claude` is runnable in this worktree, RE-CAPTURE all three from a real
session via:

```bash
mkdir -p /tmp/golden-capture && cd /tmp/golden-capture
claude -p "say hi"                                           # single_turn_no_tools
claude -p "read /etc/hostname"                               # multi_turn_with_tools
# compacted_session: long convo or `/compact` command        # compacted_session
cp ~/.claude/projects/-tmp-golden-capture/*.jsonl ./capture/
```

Then run `tools/golden-sanitize.sh` (TBD M5-08 helper) to redact PII and
tokenize. Until then, the synthetic fixtures are byte-locked to the
T0 reverse-engineered schema and the round-trip tests in T10-T12 prove the
writer matches them.
