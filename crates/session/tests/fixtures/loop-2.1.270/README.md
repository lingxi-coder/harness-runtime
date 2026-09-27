# Claude Code 2.1.270 scheduled-fire oracle

`generate.mjs` evaluates the extracted official `ARt`, `Te`, `K`, and prompt sanitizer functions. It produces fixed-interval and dynamic fire envelopes, the separate no-op companion, and prompt sanitizer cases. UUIDs, time and host/session wrapper fields are fixed fixture inputs; fire fields are produced by upstream code.

```sh
node generate.mjs /path/to/2.1.270/chunks/src_169588164.js
```

Required sibling chunks: `src_197155721.js` (loop companion) and `src_164782789.js` (prompt normalization). Source binary SHA-256: `a506b6d970a4cf44f6abdb53a81ddcd5d3b0ce042a95c502fe9d1f946bdb8807`.

The fixture deliberately does not contain the former LingXi `loopWakeup` envelope. Dynamic records use `taskKind`, `cronKind`, `noOpStreak`, ISO `streakStartedAt`, and `foldedUuids`; fixed records omit loop-only fields. Companion records are meta user messages with `turnCompanion: true`.
