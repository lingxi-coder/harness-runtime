---
name: local-app-data
description: Query and mutate a local app's own declared collections through the host's schema-validated store — bounded pages, filters, and two independent confirmation gates on writes.
---

# Query and mutate a local app's data

Own read/write access to one app's own declared collections: bounded,
schema-validated queries and confirmed record mutations. Never touches the
app's SQLite file directly, never runs raw SQL, and never reaches into a
different app's data.

## The tools

- `LocalAppQueryData` — `app_id`, `collection`, `limit` (1-100), `offset`,
  `filter`/`filters`, `sort`/`sort_key`/`sort_direction`. Results come back
  as `records[].document`; `recordId`, `revision`, `createdAtMs`, and
  `updatedAtMs` ride alongside each record as host metadata, never inside
  `document`. Pass the response's numeric `nextOffset` back as the next
  request's `offset` — there is no string cursor, and one was never accepted.
- `LocalAppMutateData` — `app_id`, `collection`, `operations` (1-50 per
  batch), each exactly `{kind:"upsert", recordId, document, expectedRevision?}`
  or `{kind:"delete", recordId, expectedRevision?}`. An `upsert` always
  **replaces the whole document** — there is no partial-field patch. To
  change one field, query the record first, then resend every field with
  the one you're changing.

## Filters and sorting

Eight operators: `equal`, `not_equal`, `less_than`, `less_than_or_equal`,
`greater_than`, `greater_than_or_equal`, `contains` (string substring), and
`in` (up to 20 scalars). Up to 16 filters per query, combined with AND.
`sort_key` is one of `record_id`, `created_at`, `updated_at`, `revision`, or
`field` (with `field_id`).

## What a document field must look like

Every key in `document` must be a field the collection actually declares —
an undeclared key is rejected outright, not silently dropped — and every
`required` field must be present:

- `text` / `long_text` — a string, at most 64 KB.
- `integer` — a whole number.
- `decimal` — a finite JSON number.
- `boolean` — `true`/`false`.
- `date_time` — an ISO-8601/RFC-3339-shaped string with a `T` separator and
  either a trailing `Z` or a `+`/`-` offset, e.g. `2026-08-30T12:00:00Z`.
- `enum` — must equal one of the field's declared `enumOptions`.
- `image_ref` — an opaque native image reference string, 1-2048 bytes; not a
  raw file path and not a base64 blob.

The whole encoded document is capped at 1 MB.

## Optimistic concurrency

`expectedRevision` is optional on both `upsert` and `delete`. Omit it to
write blind — the record's revision still advances, but nothing is checked
against it. Provide it and the write fails with a revision conflict if the
record's current revision doesn't match, including the case where you
supplied a revision but the record doesn't exist yet (treated as expected
vs. actual `0`).

## "CONFIRMED" is two independent, code-enforced gates — not chat courtesy

1. `LocalAppMutateData` is deny-by-default at the ordinary tool-permission
   layer, exactly like `LocalAppQueryData` — the call itself can raise the
   standard permission prompt before it ever reaches the host.
2. Independently, the **first** conversation-agent mutation against an app
   also runs through the host's own `DataMutation` capability gate: unless
   the user has already granted it for this app (persisted, or for this
   session), the host raises its **own** native prompt — "The agent
   requested permission to modify this app's persisted data" — with Deny /
   Allow once / Allow for session / Always allow. A denial fails the whole
   batch with "user denied the local app capability"; nothing partial lands.
   `LocalAppQueryData` does **not** carry this second gate — only mutation
   does.

So a mutation is confirmed by two independently-enforced host gates, not by
asking "are you sure?" in chat. Never substitute a chat confirmation for
actually calling the tool and letting the host raise its own prompt, and
never retry a denied mutation by rephrasing the request — a denial is the
user's answer, not a wording problem.

## Boundaries

- Never construct or execute SQL, and never open the app's database file —
  every access goes through `collection`/`document`, addressed by the
  manifest-declared schema.
- Never operate on a collection or app the current session isn't scoped to;
  inside an app's own conversation, its `LINGXI.md` id is the only one to
  use.
- Declaring a *new* collection or field, or changing what a field IS, is not
  this skill's job — that goes through the app's manifest, in a different
  operation entirely. This skill only reads and writes records against a
  schema that's already been declared.
- A background-scheduled flow step reaches the data store through its own
  capability grant, not through this skill — see `$local-app-background`.
