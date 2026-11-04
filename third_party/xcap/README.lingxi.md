# Local xcap dependency

Based on the published `xcap` 0.5.2 crate (Apache-2.0), SHA-256
`d8c280bc7ebbb756eb06816efc2b16d63f85d7ef6c10dcc09bb612503cd78964`.
The original license and crate manifest are retained.

The Linux implementation uses zbus 6's maintained wire types and async API.
Its synchronous public interface drives zbus futures on scoped worker threads
so it remains callable from an existing async runtime. zbus 6 unifies its wire
and connection error types, so the duplicate conversion has been removed.
The macOS and Windows source is unchanged.

Upstream migration context: https://github.com/z-galaxy/zbus/issues/2043
