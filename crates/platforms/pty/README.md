# platform-pty

Cross-platform pseudo-terminal process primitives used by LingXi Code's
interactive background sessions.

The lifecycle and I/O design is adapted from OpenAI Codex's
`codex-rs/utils/pty` at commit
`b8c2d29cc23b41fa7c7f5f5483e92fc71099635a` (Apache-2.0): bounded byte
channels, a blocking PTY reader and waiter, an asynchronous writer, retained
PTY handles, resize support, and whole-process-tree termination.

The PTY implementation is supplied by `portable-pty` 0.9.0. On Windows that
crate uses ConPTY; this wrapper additionally assigns the child to a
kill-on-close Job Object so descendants cannot outlive the PTY session.

## API

- `spawn_pty_process` starts a command under a controlling PTY.
- `ProcessHandle::write` sends raw, unmodified bytes.
- `ProcessHandle::resize` updates the terminal dimensions.
- `ProcessHandle::signal`, `request_terminate`, and `terminate` control the
  complete child process group/tree.
- `ProcessHandle::wait` and `SpawnedProcess::exit_rx` observe exit without
  losing final PTY output.

Callers must keep draining `stdout_rx`: it is deliberately bounded so a slow
consumer applies backpressure instead of allowing unbounded memory growth.
