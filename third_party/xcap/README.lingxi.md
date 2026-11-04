# Vendored xcap

This is xcap 0.5.2 from crates.io, released under the accompanying Apache-2.0
license. Its crate archive has SHA-256
`d8c280bc7ebbb756eb06816efc2b16d63f85d7ef6c10dcc09bb612503cd78964` and
upstream source revision `ec943c954ed37bd8fc2c9035f26fb447c1a6b140`.
The upstream manifest is retained as `Cargo.toml.orig`.

The local changes migrate the Linux portal implementation from zbus 5 to zbus 6
and correct its request and session lifecycles. The other platform sources are
unchanged. This fork is a path dependency so its dependency choice also applies
when Harness is used as a Git dependency by another workspace. Remove it once an
upstream release with the migration and compatible image dependency is available.

The standalone `Cargo.lock` is seeded from the Harness lockfile and committed so
the Linux portal tests also run reproducibly outside the excluded workspace.
`check-lock.py` checks shared versions before CI runs those tests. Cargo also
resolves this crate's upstream optional `image` feature and its development
dependencies for the standalone lock, so it contains additional packages not
used by the production configuration. `quick-error` 2.0.1 is an optional image
dependency; the Harness lock has 1.2.3 for a separate dependency. The exception
matches the exact crates.io source and checksum, and CI checks that version is
absent from both active Linux test graphs. When refreshing the locks, seed this
lock from the Harness lock, resolve the standalone crate's
features and development dependencies, then run the lock check and both portal
test variants before committing.

The portal worker owns and closes its D-Bus session while a recorder is alive.
The upstream PipeWire capture thread still runs its main loop without an explicit
stop/join when the recorder is dropped. The portal lifetime test does not cover
or establish cleanup of that separate capture thread.
