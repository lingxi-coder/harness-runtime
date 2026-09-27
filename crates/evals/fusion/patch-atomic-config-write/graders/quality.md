---
type: llm
name: quality
weight: 2.0
---
A strong proposal writes a temporary file in the SAME directory, writes and fsyncs it,
renames it over the target, and then fsyncs the directory. Look for all four parts: the
same-directory requirement so the rename is atomic, the fsync of the file before the
rename, the rename itself, and the directory fsync without which the rename may not
survive. Look also for cleanup of the temporary file on the error path, and for the
observation that `write_all` returning `Ok` does not mean the bytes reached the disk.
Penalise a proposal that omits either fsync.
