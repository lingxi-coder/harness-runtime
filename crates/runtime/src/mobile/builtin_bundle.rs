//! P1.4 — mobile atomic materialization with digest verification (§19.2).
//!
//! This module turns the packer's output (`local_apps::packer::pack` — an
//! archive of `[u32 LE path_len][path][u64 LE content_len][content]` records,
//! concatenated and hashed — see `local-apps/src/packer.rs`'s module docs for
//! the exact byte layout this module decodes) into a promoted, on-disk,
//! read-only builtin bundle root, verifying every byte along the way.
//!
//! ## Why "atomic" means staging + rename
//!
//! §19.2 requires that an interrupted materialization never replaces a
//! previous verified root — the on-disk state must always be either the old
//! root (untouched) or the new one (complete), never a mixture. This module
//! gets that property from the oldest trick available: write the new root's
//! files into a *staging* directory that nothing else names yet, and only
//! after every file is written and verified does
//! [`materialize_new_root`] `rename(2)` the staging directory onto its final,
//! digest-named path. A crash (or, in a test, an injected fault) at any point
//! before that rename leaves an orphaned staging directory that no caller
//! ever looks at — the previous verified root, a distinct path that was
//! never touched by this call, is exactly as it was.
//!
//! ## The two failure shapes named in the design doc
//!
//! §19.2 explicitly calls out that "an interrupted materialization does not
//! replace the previous verified root" is vacuously true when there is no
//! previous root (nothing can replace it, so any implementation "passes").
//! [`ensure_verified_builtin_root`] is written to make the two states
//! genuinely different, not merely differently named:
//!
//! - a previous verified root exists → a failed materialization returns
//!   `Ok(previous_root)`, unchanged, still fully readable;
//! - no previous verified root exists → a failed materialization returns
//!   `Err(BuiltinBundleError::BuiltinBundleUnavailable(_))` — never a panic,
//!   never a half-registered path handed back as if it were verified.
//!
//! ## Digest verification is content-sensitive, not size-sensitive
//!
//! Both the whole-archive digest ([`MaterializeError::DigestMismatch`]) and
//! the per-file digest ([`MaterializeError::PerFileDigestMismatch`]) are
//! SHA-256 over actual bytes (via `local_apps::packer::sha256_hex`, the same
//! function the packer itself pins its known-answer vectors against) — never
//! a byte-count stand-in. The tests below tamper a single byte while holding
//! every length constant specifically to prove that.
//!
//! ## P1.5 — the §6.2 idempotent short-circuit
//!
//! A second startup whose digest has not changed must not re-decode
//! `archive`. It does re-hash the promoted files because they remain writable;
//! accepting only their prior byte counts would trust same-length tampering.
//! [`ensure_verified_builtin_root`]
//! gets this from [`short_circuit_candidate`]: a previously-promoted root is
//! trusted on sight ONLY when a sibling manifest ([`RootManifest`], written by
//! [`write_manifest`] the moment a root is verified and promoted) still names
//! this exact digest and every component the caller currently expects. Seven
//! independent conditions can defeat it — a missing directory, a directory
//! that is actually a symlink, a missing manifest, an empty or entry-short
//! component list, a component whose recorded byte count or digest disagrees
//! with what the caller declares, a file whose current bytes no longer hash to
//! that digest, or a manifest whose marker field names a different digest — and
//! each is tested in isolation below specifically so
//! a check that quietly didn't exist could not hide behind a neighbor firing
//! instead. Every defeat falls all the way through to the same full
//! decode-and-verify path a first-ever materialization takes; nothing about
//! the short-circuit weakens what happens when it declines to fire.

use std::path::{Component, Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};

use local_apps::PackedFile;
use thiserror::Error;

/// Failure of one attempt to materialize a *new* root. Every variant names
/// enough detail (a path, an expected/actual pair, a count) for a caller —
/// and a test — to assert on *what* failed, not just that something did.
#[derive(Debug, Error, PartialEq, Eq)]
pub enum MaterializeError {
    /// The whole-archive SHA-256 does not match the caller-supplied expected
    /// digest. This is the single-byte-tamper gate: flipping any content byte
    /// changes `actual` while `expected` (and the archive's length) stay
    /// fixed.
    #[error("archive digest mismatch: expected {expected}, computed {actual}")]
    DigestMismatch {
        /// The digest the caller expected (normally the compiled-in constant
        /// for the shipped bundle).
        expected: String,
        /// The digest actually computed from the archive bytes handed in.
        actual: String,
    },
    /// The archive's byte layout does not decode as
    /// `[u32 path_len][path][u64 content_len][content]` records, or a
    /// decoded record fails path safety (empty, absolute, or containing a
    /// `.`/`..` component — the same rule `local-apps/src/packer.rs`'s
    /// `validate_relative_path` enforces on the *pack* side; this is its
    /// materializer-side mirror, since the bytes here came off an archive
    /// rather than a caller-supplied `InventoryEntry`), or disagrees with the
    /// declared inventory (wrong file count, an archive path the inventory
    /// never declared, or an inventory path the archive never delivered).
    #[error("archive is malformed: {0}")]
    MalformedArchive(String),
    /// A decoded record's own content does not hash to the per-file digest
    /// the caller's inventory declared for that path — defense in depth
    /// beyond the whole-archive digest, catching e.g. a corrupted record
    /// whose length fields still parse.
    #[error("per-file digest mismatch for {path}: expected {expected}, got {actual}")]
    PerFileDigestMismatch {
        /// The offending root-relative path.
        path: String,
        /// The digest the inventory declared for this path.
        expected: String,
        /// The digest actually computed from the decoded record's bytes.
        actual: String,
    },
    /// Materialization was interrupted after writing `files_written` file(s)
    /// into the staging directory but before the atomic promote — in
    /// production this is what a real crash/kill/storage-full looks like; in
    /// tests it is deliberately injected (see
    /// `materialize_new_root_with_fault_injection`) so the interruption tests
    /// exercise a genuine partial write rather than a failure at step zero.
    #[error("materialization interrupted after writing {files_written} file(s)")]
    Interrupted {
        /// How many files had already been written to the staging directory
        /// when the interruption occurred.
        files_written: usize,
    },
    /// A filesystem operation failed (create/write/rename) for a reason
    /// unrelated to the archive's own content.
    #[error("failed to materialize {path}: {detail}")]
    Io {
        /// The path being operated on when the failure occurred.
        path: String,
        /// `to_string()` of the underlying `std::io::Error`.
        detail: String,
    },
}

/// Typed failure the §19.2 client contract names: no verified builtin root is
/// available at all (no previous root to fall back to, and the fresh
/// materialization attempt failed). Callers must render this as a real
/// failure UI, never a silent no-op — the design doc is explicit that this is
/// a **new** failure mode this bundle-materialization design introduces
/// (today's `include_bytes!` constants cannot fail to materialize).
#[derive(Debug, Error, PartialEq, Eq)]
pub enum BuiltinBundleError {
    /// No previous verified root existed, and this materialization attempt
    /// failed. The payload is the underlying [`MaterializeError`]'s message,
    /// for logs — callers key behavior on the *variant*
    /// (`BuiltinBundleUnavailable`), not on this string.
    #[error("builtin_bundle_unavailable: {0}")]
    BuiltinBundleUnavailable(String),
}

/// Where a test-injected fault lands inside one materialization attempt.
/// Two points, because they are genuinely different crash states and only
/// the second one can reach the promote boundary:
///
/// - [`InterruptPoint::AfterFilesWritten`] models a crash *during* the
///   staging write, with some files already on disk and others not — the
///   partial-write state §19.2's interruption clauses name.
/// - [`InterruptPoint::BeforePromote`] models a crash at the instant the
///   staging directory is complete and verified but has not yet been renamed
///   into place. This is the dangerous one: it is the only point at which a
///   naive promote (`remove_dir_all` the old root, then `rename`) has already
///   destroyed the previous verified root.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum InterruptPoint {
    /// Fault the instant the n-th staged file has been written.
    AfterFilesWritten(usize),
    /// Fault after every file is staged and verified, immediately before the
    /// atomic promote.
    BeforePromote,
}

static STAGING_SEQUENCE: AtomicU64 = AtomicU64::new(0);

/// A filesystem-unique staging directory NAME (not yet created) for one
/// materialization attempt of `expected_digest`. Uniqueness (not
/// unguessability) is all that's required — this is a same-process, same-user
/// staging area, never handed to an untrusted party — so a process-id +
/// wall-clock + in-process counter tuple is sufficient and needs no RNG
/// dependency this crate does not otherwise carry.
fn staging_dir_name(expected_digest: &str) -> String {
    let sequence = STAGING_SEQUENCE.fetch_add(1, Ordering::Relaxed);
    let nanos = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_nanos())
        .unwrap_or(0);
    format!(
        ".staging-{expected_digest}-{}-{nanos}-{sequence}",
        std::process::id()
    )
}

/// The final promoted-root path for `expected_digest` under `data_root`.
/// Digest-named so the idempotent-reuse short-circuit (P1.5 —
/// `short_circuit_candidate`) can recognize an already-promoted root by path
/// alone; naming is otherwise this module's own implementation detail, never
/// part of a wire contract.
fn promoted_root_path(data_root: &Path, expected_digest: &str) -> PathBuf {
    data_root.join(format!("root-{expected_digest}"))
}

/// The §6.2 short-circuit manifest path for `expected_digest`'s promoted
/// root under `data_root` — a plain SIBLING file next to
/// [`promoted_root_path`]'s directory, never inside it, so it can never
/// collide with a payload path the archive legitimately contains, and its
/// own presence/absence is independent of whatever is on disk inside the
/// promoted root.
fn manifest_path(data_root: &Path, expected_digest: &str) -> PathBuf {
    data_root.join(format!("root-{expected_digest}.manifest.json"))
}

fn active_manifest_path(data_root: &Path) -> PathBuf {
    data_root.join("active.manifest.json")
}

/// On-disk evidence a promoted root's manifest carries so a LATER call can
/// trust it without re-decoding the archive. Current files are still re-hashed.
/// See
/// [`write_manifest`] (the write side, called only after a root is fully
/// verified and promoted) and [`short_circuit_candidate`] (the read side,
/// and the exhaustive enumeration of every way this evidence can fail to be
/// trusted).
#[derive(Debug, Clone, serde::Serialize, serde::Deserialize)]
struct RootManifest {
    /// The marker field: the whole-archive digest this root was verified
    /// against at promotion time. Must equal the CALLER's `expected_digest`
    /// for the short-circuit to fire — a manifest that is stale, or was
    /// somehow copied from a different digest's root, must never be trusted
    /// just because it happens to sit at the path this call is checking.
    digest: String,
    /// One entry per file the inventory declared at promotion time. An
    /// empty list, or a list missing an entry the caller's CURRENT inventory
    /// requires (or disagreeing with it on byte count / digest), defeats the
    /// short-circuit exactly as if the manifest did not exist at all.
    components: Vec<ManifestComponent>,
}

/// One promoted file's recorded identity: enough to recognize whether the
/// caller's current inventory still names the same content, without
/// re-reading or re-hashing the file itself.
#[derive(Debug, Clone, serde::Serialize, serde::Deserialize)]
struct ManifestComponent {
    path: String,
    bytes: u64,
    sha256: String,
}

/// Persist a §6.2 short-circuit manifest for a just-verified `final_root`
/// (identified by `expected_digest`, under `data_root`): the whole-archive
/// digest (the marker field) plus one component entry per file `inventory`
/// declares. Called only from the two success returns of
/// [`materialize_new_root_with_fault_injection`] — never on an interrupted
/// or rejected attempt — so a manifest existing at all is itself evidence
/// that a full verification once succeeded here.
///
/// Written as a plain sibling file, never inside the promoted root (see
/// [`manifest_path`]) — a crash between promoting the root and writing this
/// file just means the NEXT call finds no manifest and safely re-verifies
/// (§6.2's own missing-manifest defeat case), not a hazard this write itself
/// needs to be atomic against.
fn write_manifest(
    data_root: &Path,
    expected_digest: &str,
    inventory: &[PackedFile],
) -> Result<(), MaterializeError> {
    let manifest = RootManifest {
        digest: expected_digest.to_string(),
        components: inventory
            .iter()
            .map(|entry| ManifestComponent {
                path: entry.path.clone(),
                bytes: entry.bytes,
                sha256: entry.sha256.clone(),
            })
            .collect(),
    };
    let path = manifest_path(data_root, expected_digest);
    let json = serde_json::to_vec(&manifest).map_err(|e| MaterializeError::Io {
        path: path.display().to_string(),
        detail: e.to_string(),
    })?;
    atomic_write(&path, &json)
}

fn atomic_write(path: &Path, bytes: &[u8]) -> Result<(), MaterializeError> {
    static WRITE_SEQUENCE: AtomicU64 = AtomicU64::new(0);
    let parent = path.parent().ok_or_else(|| MaterializeError::Io {
        path: path.display().to_string(),
        detail: "path has no parent".to_string(),
    })?;
    std::fs::create_dir_all(parent).map_err(|error| MaterializeError::Io {
        path: parent.display().to_string(),
        detail: error.to_string(),
    })?;
    let sequence = WRITE_SEQUENCE.fetch_add(1, Ordering::Relaxed);
    let name = path
        .file_name()
        .and_then(|value| value.to_str())
        .ok_or_else(|| MaterializeError::Io {
            path: path.display().to_string(),
            detail: "non-UTF-8 filename".to_string(),
        })?;
    let temp = path.with_file_name(format!(".{name}.tmp-{}-{sequence}", std::process::id()));
    let mut options = std::fs::OpenOptions::new();
    options.write(true).create_new(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt as _;
        options.mode(0o600);
    }
    let result = (|| -> std::io::Result<()> {
        use std::io::Write as _;
        let mut file = options.open(&temp)?;
        file.write_all(bytes)?;
        file.sync_all()?;
        std::fs::rename(&temp, path)?;
        if let Ok(directory) = std::fs::File::open(parent) {
            let _ = directory.sync_all();
        }
        Ok(())
    })();
    if let Err(error) = result {
        let _ = std::fs::remove_file(&temp);
        return Err(MaterializeError::Io {
            path: path.display().to_string(),
            detail: error.to_string(),
        });
    }
    Ok(())
}

fn active_verified_bundle(data_root: &Path) -> Option<(PathBuf, Vec<PackedFile>)> {
    let active_path = active_manifest_path(data_root);
    if !std::fs::symlink_metadata(&active_path)
        .ok()?
        .file_type()
        .is_file()
    {
        return None;
    }
    let manifest: RootManifest = serde_json::from_slice(&std::fs::read(active_path).ok()?).ok()?;
    let digest = manifest.digest;
    let inventory: Vec<PackedFile> = manifest
        .components
        .into_iter()
        .map(|component| PackedFile {
            path: component.path,
            bytes: component.bytes,
            sha256: component.sha256,
        })
        .collect();
    if inventory.is_empty() {
        return None;
    }
    let root = short_circuit_candidate(data_root, &digest, &inventory)?;
    Some((root, inventory))
}

fn write_active_manifest(
    data_root: &Path,
    expected_digest: &str,
    inventory: &[PackedFile],
) -> Result<(), MaterializeError> {
    let manifest = RootManifest {
        digest: expected_digest.to_string(),
        components: inventory
            .iter()
            .map(|entry| ManifestComponent {
                path: entry.path.clone(),
                bytes: entry.bytes,
                sha256: entry.sha256.clone(),
            })
            .collect(),
    };
    let path = active_manifest_path(data_root);
    let bytes = serde_json::to_vec(&manifest).map_err(|error| MaterializeError::Io {
        path: path.display().to_string(),
        detail: error.to_string(),
    })?;
    atomic_write(&path, &bytes)
}

/// A previously-promoted root this call can trust WITHOUT re-decoding
/// `archive` or re-hashing any file — or `None` if any one of the five §6.2
/// defeat conditions holds, in which case the caller must fall through to a
/// full [`materialize_new_root_with_fault_injection`] exactly as if no
/// promoted root existed at all:
///
/// 1. no directory exists at the digest-named path yet;
/// 2. that path is not a genuine directory — in particular a SYMLINK,
///    checked with `symlink_metadata` (which does NOT follow a symlink,
///    unlike `Path::is_dir`/`std::fs::metadata`) so a link is rejected
///    without ever caring what it resolves to;
/// 3. no manifest file sits beside it (never written, or removed);
/// 4. the manifest's `digest` marker field does not name THIS call's
///    `expected_digest`;
/// 5. the manifest's component list is empty (no inventory was ever
///    recorded at all), or is missing an entry for one of the paths
///    `inventory` (the caller's current declared file set) requires, or
///    that entry's recorded byte count / digest disagrees with what
///    `inventory` declares for that path.
///
/// This module's tests corrupt exactly ONE of these at a time, holding
/// everything else genuinely valid, specifically so a check that quietly
/// didn't exist could not hide behind a neighbor firing instead. (5)'s two
/// named scenarios — an entirely empty component list, and a list missing
/// one specific entry — are both proven independently even though they
/// share the same loop: each test corrupts only its own scenario, and a
/// weakened loop that stopped rejecting on a failed lookup would turn BOTH
/// tests red, which is the correct, expected coupling for two states that
/// really are the same failure at different scale.
fn short_circuit_candidate(
    data_root: &Path,
    expected_digest: &str,
    inventory: &[PackedFile],
) -> Option<PathBuf> {
    let candidate = promoted_root_path(data_root, expected_digest);

    // (1) & (2): must exist AND be a real directory, never a symlink —
    // `symlink_metadata` is load-bearing here; `candidate.is_dir()` would
    // follow the link and defeat the whole point of this check.
    let root_meta = std::fs::symlink_metadata(&candidate).ok()?;
    if !root_meta.file_type().is_dir() {
        return None;
    }

    // (3): the manifest sibling must be a real file (never a symlink) and
    // parse as a manifest.
    let manifest_file = manifest_path(data_root, expected_digest);
    if !std::fs::symlink_metadata(&manifest_file)
        .ok()?
        .file_type()
        .is_file()
    {
        return None;
    }
    let manifest_bytes = std::fs::read(manifest_file).ok()?;
    let manifest: RootManifest = serde_json::from_slice(&manifest_bytes).ok()?;

    // (4): the marker field.
    if manifest.digest != expected_digest {
        return None;
    }

    // (5): every currently-declared component must have a matching entry
    // recorded — same path, same byte count, same digest. An entirely EMPTY
    // component list needs no separate branch: it falls out of this same
    // loop, since every one of `inventory`'s (non-empty, in every real
    // caller) entries then fails its lookup against an empty map on the
    // very first iteration.
    let by_path: std::collections::BTreeMap<&str, &ManifestComponent> = manifest
        .components
        .iter()
        .map(|component| (component.path.as_str(), component))
        .collect();
    for entry in inventory {
        match by_path.get(entry.path.as_str()) {
            Some(component)
                if component.bytes == entry.bytes && component.sha256 == entry.sha256 => {}
            _ => return None,
        }
    }

    // The manifest proves what was verified at promotion time, but the promoted
    // root remains writable by the owning process. Re-hash every declared file
    // before trusting a cached root: type/length checks alone accept a
    // same-length replacement of a skill, agent, or workflow as verified code.
    // This still avoids decoding and unpacking the embedded archive.
    if !root_matches_inventory(&candidate, inventory, true) {
        return None;
    }

    Some(candidate)
}

fn root_matches_inventory(root: &Path, inventory: &[PackedFile], verify_hash: bool) -> bool {
    inventory.iter().all(|entry| {
        let relative = Path::new(&entry.path);
        let mut path = root.to_path_buf();
        let components: Vec<_> = relative.components().collect();
        for (index, component) in components.iter().enumerate() {
            let Component::Normal(segment) = component else {
                return false;
            };
            path.push(segment);
            let Ok(metadata) = std::fs::symlink_metadata(&path) else {
                return false;
            };
            if index + 1 == components.len() {
                if !metadata.file_type().is_file() || metadata.len() != entry.bytes {
                    return false;
                }
            } else if !metadata.file_type().is_dir() {
                return false;
            }
        }
        !verify_hash
            || std::fs::read(&path)
                .ok()
                .is_some_and(|bytes| local_apps::sha256_hex(&bytes) == entry.sha256)
    })
}

#[cfg(test)]
thread_local! {
    /// Test-only observable for the §6.2 short-circuit's own tests: how many
    /// times [`materialize_new_root_with_fault_injection`] has actually run the
    /// archive decode + per-file verification path on THIS THREAD — i.e. how
    /// many times the short-circuit did NOT fire. Needed because "the call
    /// returned `Ok`" cannot by itself distinguish a genuine skip from a full
    /// re-verification that happens to reach the same keep-existing-directory
    /// outcome; see `unchanged_digest_skips_unpack` for the full argument.
    ///
    /// THREAD-LOCAL, and that is the load-bearing part. A process-global
    /// counter is not a sound observable here, and a mutex over the *reading*
    /// tests does not make it one: `cargo test` runs tests in parallel, and the
    /// twelve OTHER tests in this module that call into the materializer never
    /// had any reason to take such a lock, so their increments land inside a
    /// reading test's "must not move" window regardless. (Measured, not
    /// assumed: with a global counter and a readers-only mutex, delaying one
    /// non-locking sibling into that window failed
    /// `unchanged_digest_skips_unpack` with `left: 30, right: 28`.) Because
    /// every call into the materializer during a test is synchronous on that
    /// test's own thread, a thread-local count is exactly this test's own — no
    /// lock, and no window a sibling can reach into at all.
    static FULL_MATERIALIZE_ATTEMPTS: std::cell::Cell<u64> = const { std::cell::Cell::new(0) };
}

#[cfg(test)]
fn full_materialize_attempts() -> u64 {
    FULL_MATERIALIZE_ATTEMPTS.with(|c| c.get())
}

/// True when every component of `path` is a plain path segment — no empty
/// path, no absolute path, no `.`/`..`/prefix component. Mirrors
/// `local-apps/src/packer.rs`'s `validate_relative_path` so a decoded archive
/// record can never be joined onto the staging root in a way that escapes it.
fn is_safe_relative_path(path: &str) -> bool {
    if path.is_empty() {
        return false;
    }
    Path::new(path)
        .components()
        .all(|component| matches!(component, Component::Normal(_)))
}

/// Narrow an archive-declared `u64` length to a `usize`, or `None` when the
/// declaring archive names a length this platform cannot address.
///
/// `usize_max` is a parameter rather than a hardcoded `usize::MAX` for one
/// reason, and it is a testability reason worth stating: harness-runtime::mobile ships
/// to a 32-bit target (Android armv7), but its tests run on 64-bit hosts,
/// where `declared as usize` is the identity and the truncation this guards
/// against is INVISIBLE — a test written against `usize::MAX` directly is
/// green on the development host whether the guard is present or not.
/// Threading the bound through lets the 32-bit rejection be exercised, and
/// therefore falsified, on any host.
fn narrow_declared_len(declared: u64, usize_max: u64) -> Option<usize> {
    if declared > usize_max {
        return None;
    }
    usize::try_from(declared).ok()
}

/// Decode `archive` into `(path, content)` records per the packer's
/// documented layout: `[u32 LE path_len][path bytes][u64 LE content_len]
/// [content bytes]`, concatenated with no separators. Rejects a truncated
/// trailing record, a non-UTF-8 path, or a path that fails
/// [`is_safe_relative_path`].
fn decode_records(archive: &[u8]) -> Result<Vec<(String, Vec<u8>)>, MaterializeError> {
    let mut records = Vec::new();
    let mut offset = 0usize;
    let len = archive.len();
    while offset < len {
        if offset + 4 > len {
            return Err(MaterializeError::MalformedArchive(format!(
                "truncated path_len field at byte offset {offset}"
            )));
        }
        let path_len = u32::from_le_bytes(
            archive[offset..offset + 4]
                .try_into()
                .expect("checked length"),
        ) as usize;
        offset += 4;

        if offset.checked_add(path_len).is_none_or(|end| end > len) {
            return Err(MaterializeError::MalformedArchive(format!(
                "truncated path bytes ({path_len} declared) at byte offset {offset}"
            )));
        }
        let path =
            String::from_utf8(archive[offset..offset + path_len].to_vec()).map_err(|_| {
                MaterializeError::MalformedArchive(format!("non-utf8 path at byte offset {offset}"))
            })?;
        offset += path_len;

        if offset + 8 > len {
            return Err(MaterializeError::MalformedArchive(format!(
                "truncated content_len field at byte offset {offset}"
            )));
        }
        let declared_content_len = u64::from_le_bytes(
            archive[offset..offset + 8]
                .try_into()
                .expect("checked length"),
        );
        offset += 8;
        // `as usize` would TRUNCATE here on a 32-bit target — and harness-runtime::mobile
        // ships to one (Android armv7). A declared length of 0x1_0000_0005
        // would silently become 5, so a record the decoder must reject would
        // instead be accepted with 5 bytes of someone else's data. Reject the
        // value the archive actually declared, on every pointer width.
        let Some(content_len) = narrow_declared_len(declared_content_len, usize::MAX as u64) else {
            return Err(MaterializeError::MalformedArchive(format!(
                "declared content length {declared_content_len} for {path} does not fit \
                 in this platform's usize"
            )));
        };

        if offset.checked_add(content_len).is_none_or(|end| end > len) {
            return Err(MaterializeError::MalformedArchive(format!(
                "truncated content bytes ({declared_content_len} declared) for {path} \
                 at byte offset {offset}"
            )));
        }
        let content = archive[offset..offset + content_len].to_vec();
        offset += content_len;

        if !is_safe_relative_path(&path) {
            return Err(MaterializeError::MalformedArchive(format!(
                "archive record path is not a plain relative path: {path}"
            )));
        }
        records.push((path, content));
    }
    Ok(records)
}

/// Cross-check decoded archive records against the caller's declared
/// per-file inventory: same path set (no archive path the inventory never
/// declared, no inventory path the archive never delivered), same byte
/// count, same content SHA-256. This is the "逐文件验证 inventory/digest"
/// step of §6.2, layered on top of the whole-archive digest check.
fn verify_records_against_inventory(
    records: &[(String, Vec<u8>)],
    inventory: &[PackedFile],
) -> Result<(), MaterializeError> {
    if records.len() != inventory.len() {
        return Err(MaterializeError::MalformedArchive(format!(
            "archive holds {} record(s) but the inventory declares {}",
            records.len(),
            inventory.len()
        )));
    }
    let by_path: std::collections::BTreeMap<&str, &PackedFile> = inventory
        .iter()
        .map(|entry| (entry.path.as_str(), entry))
        .collect();

    for (path, content) in records {
        let expected = by_path.get(path.as_str()).ok_or_else(|| {
            MaterializeError::MalformedArchive(format!(
                "archive record not declared in the inventory: {path}"
            ))
        })?;
        // Widen the record's length to `u64` rather than narrowing the
        // inventory's `u64` to `usize`: on a 32-bit target the narrowing
        // direction truncates, and a declared length of 2^32 + n would
        // compare equal to a record of n bytes.
        if expected.bytes != content.len() as u64 {
            return Err(MaterializeError::MalformedArchive(format!(
                "{path}: inventory declares {} byte(s), archive holds {}",
                expected.bytes,
                content.len()
            )));
        }
        let actual = local_apps::sha256_hex(content);
        if actual != expected.sha256 {
            return Err(MaterializeError::PerFileDigestMismatch {
                path: path.clone(),
                expected: expected.sha256.clone(),
                actual,
            });
        }
    }
    Ok(())
}

/// Materialize a NEW verified root from `archive` under `data_root`, with an
/// optional fault-injection point that lands mid-write. Never called with
/// `Some(_)` outside this module's own tests — see
/// [`materialize_new_root`] for the real entry point.
///
/// Algorithm (§6.2 steps 2-4):
/// 1. Verify the whole-archive digest.
/// 2. Decode + cross-check every record against `inventory`.
/// 3. Create a staging directory nothing else names yet and write every
///    record into it. If `interrupt_at == Some(AfterFilesWritten(n))`, return
///    [`MaterializeError::Interrupted`] the instant the n-th file lands,
///    leaving the staging directory (with exactly `n` files in it) orphaned
///    on disk — nothing below ever renames it, so it is never mistaken for a
///    promoted root.
/// 4. If the digest-named final path already contains the fully verified
///    inventory, keep it and discard the staging copy. If it is corrupt or
///    has the wrong type, quarantine it and atomically promote the verified
///    staging copy, restoring the old path if promotion fails.
fn materialize_new_root_with_fault_injection(
    data_root: &Path,
    archive: &[u8],
    expected_digest: &str,
    inventory: &[PackedFile],
    interrupt_at: Option<InterruptPoint>,
) -> Result<PathBuf, MaterializeError> {
    // P1.5's own test observable: every entry into the real decode+verify
    // path, whether or not it ends up taking the keep-existing-root branch.
    // Never used to gate behavior — read-only instrumentation for tests that
    // must distinguish "genuinely skipped" from "ran again and reached the
    // same outcome".
    #[cfg(test)]
    FULL_MATERIALIZE_ATTEMPTS.with(|c| c.set(c.get() + 1));

    let actual_digest = local_apps::sha256_hex(archive);
    if actual_digest != expected_digest {
        return Err(MaterializeError::DigestMismatch {
            expected: expected_digest.to_string(),
            actual: actual_digest,
        });
    }

    let records = decode_records(archive)?;
    verify_records_against_inventory(&records, inventory)?;

    std::fs::create_dir_all(data_root).map_err(|e| MaterializeError::Io {
        path: data_root.display().to_string(),
        detail: e.to_string(),
    })?;
    let staging = data_root.join(staging_dir_name(expected_digest));
    std::fs::create_dir_all(&staging).map_err(|e| MaterializeError::Io {
        path: staging.display().to_string(),
        detail: e.to_string(),
    })?;

    for (index, (path, content)) in records.iter().enumerate() {
        let dest = staging.join(path);
        if let Some(parent) = dest.parent() {
            std::fs::create_dir_all(parent).map_err(|e| MaterializeError::Io {
                path: parent.display().to_string(),
                detail: e.to_string(),
            })?;
        }
        std::fs::write(&dest, content).map_err(|e| MaterializeError::Io {
            path: dest.display().to_string(),
            detail: e.to_string(),
        })?;

        if interrupt_at == Some(InterruptPoint::AfterFilesWritten(index + 1)) {
            // Deliberately no cleanup: a real interruption (crash, kill,
            // storage exhaustion) cannot clean up after itself either, and
            // the whole point of the tests this guards is to prove that an
            // orphaned, partially-written staging directory is never
            // mistaken for — or promoted into — a verified root.
            return Err(MaterializeError::Interrupted {
                files_written: index + 1,
            });
        }
    }

    let final_root = promoted_root_path(data_root, expected_digest);
    if std::fs::symlink_metadata(&final_root).is_ok() {
        // A path already occupies this digest address. Verify it before
        // reuse: a stale manifest, partial external deletion, or a symlink
        // must not make arbitrary bytes trusted. DESTROYING a valid root in
        // order to re-promote (`remove_dir_all` followed by `rename`, the
        // obvious implementation) would open exactly the window §19.2
        // forbids: a crash between the remove and the rename leaves NO root
        // at all — neither the old state nor the new one, which is the
        // "mixture" atomicity is supposed to rule out. A re-materialization
        // of the current bundle is an ordinary event (app relaunch,
        // re-verification), so that window is reachable in production, not
        // hypothetical. The existing root is therefore kept untouched and
        // the now-redundant staging directory is discarded instead.
        //
        // Invalid content takes the separate quarantine/promote/rollback
        // branch below, so repair also never leaves the destination absent.
        if std::fs::symlink_metadata(&final_root)
            .ok()
            .is_some_and(|metadata| metadata.file_type().is_dir())
            && root_matches_inventory(&final_root, inventory, true)
        {
            let _ = std::fs::remove_dir_all(&staging);
            write_manifest(data_root, expected_digest, inventory)?;
            return Ok(final_root);
        }

        // The digest-named path exists but is not the verified content we just
        // staged. Move it aside first; if promote fails, put it back so this
        // repair attempt does not make the on-disk state worse.
        let quarantined = data_root.join(format!(
            ".invalid-{expected_digest}-{}",
            staging_dir_name(expected_digest)
        ));
        std::fs::rename(&final_root, &quarantined).map_err(|e| MaterializeError::Io {
            path: final_root.display().to_string(),
            detail: e.to_string(),
        })?;
        if let Err(error) = std::fs::rename(&staging, &final_root) {
            let _ = std::fs::rename(&quarantined, &final_root);
            return Err(MaterializeError::Io {
                path: final_root.display().to_string(),
                detail: error.to_string(),
            });
        }
        if quarantined.is_dir() {
            let _ = std::fs::remove_dir_all(&quarantined);
        } else {
            let _ = std::fs::remove_file(&quarantined);
        }
        write_manifest(data_root, expected_digest, inventory)?;
        return Ok(final_root);
    }

    if interrupt_at == Some(InterruptPoint::BeforePromote) {
        // Deliberately no cleanup, as above: a real crash here cannot tidy
        // up after itself either.
        return Err(MaterializeError::Interrupted {
            files_written: records.len(),
        });
    }

    std::fs::rename(&staging, &final_root).map_err(|e| MaterializeError::Io {
        path: final_root.display().to_string(),
        detail: e.to_string(),
    })?;
    write_manifest(data_root, expected_digest, inventory)?;

    Ok(final_root)
}

/// Materialize a NEW verified root from `archive` under `data_root`. The real
/// (non-test) entry point — always runs to completion or fails cleanly,
/// never injects an artificial interruption.
pub fn materialize_new_root(
    data_root: &Path,
    archive: &[u8],
    expected_digest: &str,
    inventory: &[PackedFile],
) -> Result<PathBuf, MaterializeError> {
    materialize_new_root_with_fault_injection(data_root, archive, expected_digest, inventory, None)
}

/// §6.2/§19.2's top-level contract: produce the currently-verified builtin
/// root, or the specific `builtin_bundle_unavailable` failure.
///
/// - If materializing a new root from `archive` succeeds, that new root is
///   returned and is now the caller's current verified root.
/// - If it fails and `previous_verified_root` names an existing directory,
///   that previous root is returned UNCHANGED — the failed attempt never
///   touched it, so the caller keeps serving whatever it already had (§6.2:
///   "已 scaffold 的 App 仍可…正常 build/run/restore").
/// - If it fails and there is no previous verified root, this returns
///   [`BuiltinBundleError::BuiltinBundleUnavailable`] — never a panic, and
///   never a path to a half-written staging directory.
pub fn ensure_verified_builtin_root(
    data_root: &Path,
    archive: &[u8],
    expected_digest: &str,
    inventory: &[PackedFile],
    previous_verified_root: Option<&Path>,
) -> Result<PathBuf, BuiltinBundleError> {
    ensure_verified_builtin_root_with_fault_injection(
        data_root,
        archive,
        expected_digest,
        inventory,
        previous_verified_root,
        None,
    )
}

/// Test-only entry point for [`ensure_verified_builtin_root`] that can inject
/// a mid-write interruption via `materialize_new_root_with_fault_injection`.
/// `pub(crate)` rather than exported: nothing outside this module's own tests
/// has a legitimate reason to force a fault.
pub(crate) fn ensure_verified_builtin_root_with_fault_injection(
    data_root: &Path,
    archive: &[u8],
    expected_digest: &str,
    inventory: &[PackedFile],
    previous_verified_root: Option<&Path>,
    interrupt_at: Option<InterruptPoint>,
) -> Result<PathBuf, BuiltinBundleError> {
    // P1.5 (§6.2 short-circuit): a previously-promoted root whose on-disk
    // manifest still names this exact digest and every component the caller
    // currently expects is returned immediately — `archive` is never decoded,
    // while each writable promoted file is re-hashed. See
    // `short_circuit_candidate` for the full
    // set of conditions that must ALL hold for this to fire.
    if let Some(root) = short_circuit_candidate(data_root, expected_digest, inventory) {
        return Ok(root);
    }

    match materialize_new_root_with_fault_injection(
        data_root,
        archive,
        expected_digest,
        inventory,
        interrupt_at,
    ) {
        Ok(root) => Ok(root),
        Err(err) => match previous_verified_root {
            Some(prev) if prev.is_dir() => Ok(prev.to_path_buf()),
            _ => Err(BuiltinBundleError::BuiltinBundleUnavailable(
                err.to_string(),
            )),
        },
    }
}

// P1.10 (§6.1/§6.2): `build.rs` runs `local_apps::pack` against the checked-in
// `lingxi-local-app.inventory.txt` (beside the plugin tree), and Cargo embeds its deterministic archive
// and descriptor here. Runtime boot must consume these constants directly:
// rebuilding a scratch source tree and re-running the packer on every launch
// would make the cached-root fast path pay a full read/sort/hash pass before it
// even reached `short_circuit_candidate`.
include!(concat!(env!("OUT_DIR"), "/lingxi-local-app-descriptor.rs"));
const COMPILED_PLUGIN_ARCHIVE: &[u8] =
    include_bytes!(concat!(env!("OUT_DIR"), "/lingxi-local-app.bundle"));

/// Digest of the exact Plugin archive used by Host template selections.
/// Keeping this accessor beside the generated descriptor prevents callers
/// from re-packing the source tree (and accidentally selecting a different
/// bundle identity than the PluginManager registered).
pub(crate) fn compiled_plugin_bundle_digest() -> &'static str {
    COMPILED_PLUGIN_ARCHIVE_DIGEST
}

/// Return the byte-exact catalog emitted from the verified Plugin inventory.
/// Host consumers must read Plugin data through this bundle seam instead of
/// adding another `include_bytes!`/`include_str!` source tree, otherwise the
/// runtime can accidentally validate bytes that differ from the archive
/// identity it reports to downstream agents.
pub(crate) fn compiled_plugin_catalog_bytes() -> &'static [u8] {
    COMPILED_PLUGIN_CATALOG_BYTES
}

fn compiled_in_plugin_inventory() -> Vec<PackedFile> {
    COMPILED_PLUGIN_INVENTORY
        .iter()
        .map(|(path, bytes, sha256)| PackedFile {
            path: (*path).to_string(),
            bytes: *bytes,
            sha256: (*sha256).to_string(),
        })
        .collect()
}

/// Names of the skills in the build-time verified Plugin inventory. This is
/// the scanner/test seam for Phase 2: it derives names from the exact archive
/// descriptor instead of recreating a mobile bundled skill list.
pub(crate) fn compiled_plugin_skill_names() -> Vec<String> {
    COMPILED_PLUGIN_INVENTORY
        .iter()
        .filter_map(|(path, _, _)| {
            path.strip_prefix("skills/")
                .and_then(|rest| rest.strip_suffix("/SKILL.md"))
                .map(str::to_owned)
        })
        .collect()
}

/// Materialize the build-time packed plugin archive to a verified root. The
/// returned inventory is the descriptor generated from the exact packer
/// output, so component classification cannot drift from the archive bytes.
///
/// # Errors
/// [`BuiltinBundleError::BuiltinBundleUnavailable`] if staging the embedded
/// source fails, if `local_apps::pack` rejects the (fixed, compiled-in) file
/// set, or if materialization itself fails with no usable
/// `previous_verified_root` to fall back to — never a panic.
pub fn materialize_compiled_in_plugin_bundle(
    bundle_root: &Path,
    previous_verified_root: Option<&Path>,
) -> Result<(PathBuf, Vec<PackedFile>), BuiltinBundleError> {
    let inventory = compiled_in_plugin_inventory();
    let materialized_root = bundle_root.join("materialized");
    let active = active_verified_bundle(&materialized_root);
    let fallback_root = previous_verified_root
        .map(Path::to_path_buf)
        .or_else(|| active.as_ref().map(|(root, _)| root.clone()));
    let root = ensure_verified_builtin_root(
        &materialized_root,
        COMPILED_PLUGIN_ARCHIVE,
        COMPILED_PLUGIN_ARCHIVE_DIGEST,
        &inventory,
        fallback_root.as_deref(),
    )?;
    let current_root = promoted_root_path(&materialized_root, COMPILED_PLUGIN_ARCHIVE_DIGEST);
    if root == current_root {
        write_active_manifest(
            &materialized_root,
            COMPILED_PLUGIN_ARCHIVE_DIGEST,
            &inventory,
        )
        .map_err(|error| BuiltinBundleError::BuiltinBundleUnavailable(error.to_string()))?;
        Ok((root, inventory))
    } else if let Some((active_root, active_inventory)) = active {
        if root == active_root {
            return Ok((root, active_inventory));
        }
        Ok((root, inventory))
    } else {
        Ok((root, inventory))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use local_apps::{pack, InventoryEntry};
    use std::fs;

    /// Write `content` to `root/rel`, creating parent directories as needed —
    /// same helper shape as `local-apps/src/packer.rs`'s test fixture builder.
    fn write_file(root: &Path, rel: &str, content: &[u8]) {
        let full = root.join(rel);
        if let Some(parent) = full.parent() {
            fs::create_dir_all(parent).unwrap();
        }
        fs::write(full, content).unwrap();
    }

    /// A single-file fixture whose content and packed archive digest are
    /// IDENTICAL to `local-apps/src/packer.rs`'s
    /// `archive_digest_is_a_pinned_known_answer_vector` — reusing that
    /// already-established known-answer vector pins this module's digest
    /// check to being a content digest (not a length stand-in) without
    /// hand-computing a fresh SHA-256 here.
    const KNOWN_ANSWER_DIGEST: &str =
        "4cf2739e19d5d8f1a0f8ec859cac115fe854d271b53a689b7f105726cebd49f5";

    /// SHA-256 of the 11 bytes `hello world`.
    const HELLO_WORLD_SHA256: &str =
        "b94d27b9934d3e08a52e52d7da7dabfac484efe37a5380ee9088f7ace2efcde9";
    /// SHA-256 of the 11 bytes `hello worle` — the SAME LENGTH as
    /// `hello world`, one byte different. The pair exists so a per-file
    /// digest can be pinned along the CONTENT axis at constant length: any
    /// size- or count-derived stand-in produces the same value for both,
    /// while these two constants differ in every nibble.
    const HELLO_WORLE_SHA256: &str =
        "0fc30e735a0228a31cbbb969988b4f50e02e737f979f091d7d224b765443f5d4";

    fn known_answer_fixture(root: &Path) -> Vec<InventoryEntry> {
        write_file(root, "hello.txt", b"hello world");
        vec![InventoryEntry::new("hello.txt")]
    }

    /// A three-file fixture whose content varies by `variant`, so packing two
    /// different variants produces genuinely different archives/digests —
    /// used to model "materializing a new bundle version" in the
    /// interruption tests below.
    fn variant_fixture(root: &Path, variant: u8) -> Vec<InventoryEntry> {
        write_file(
            root,
            "plugin.json",
            format!("{{\"variant\":{variant}}}").as_bytes(),
        );
        write_file(
            root,
            "skills/router.md",
            format!("# router v{variant}\n").as_bytes(),
        );
        write_file(
            root,
            "skills/reference/deep.md",
            format!("deep reference content, variant {variant}\n").as_bytes(),
        );
        vec![
            InventoryEntry::new("plugin.json"),
            InventoryEntry::new("skills/router.md"),
            InventoryEntry::new("skills/reference/deep.md"),
        ]
    }

    /// Named hazard for this batch: a digest that is (or degenerates into) a
    /// byte-count fingerprint would pass every other test in this module,
    /// because none of them tamper with content while holding length
    /// constant. This flips exactly one byte, keeps the archive the SAME
    /// LENGTH, and pins the result against a known-answer vector so the
    /// check is proven content-sensitive rather than merely "different from
    /// itself".
    #[test]
    fn single_byte_tamper_fails_digest() {
        let source = tempfile::tempdir().unwrap();
        let inventory = known_answer_fixture(source.path());
        let packed = pack(source.path(), &inventory).unwrap();

        // Positive control: confirms this fixture really does reproduce the
        // packer's own pinned known-answer vector before we start tampering
        // with it — if this assertion ever fails, the fixture (not the
        // materializer) drifted.
        assert_eq!(
            packed.archive_digest, KNOWN_ANSWER_DIGEST,
            "fixture bug: this is not the packer's known-answer archive"
        );

        let mut tampered = packed.archive.clone();
        let last = tampered.len() - 1;
        tampered[last] ^= 0x01; // flip one bit of the final content byte
        assert_eq!(
            tampered.len(),
            packed.archive.len(),
            "fixture bug: tamper changed the archive length"
        );

        let data_root = tempfile::tempdir().unwrap();
        let err = materialize_new_root(
            data_root.path(),
            &tampered,
            &packed.archive_digest,
            &packed.inventory,
        )
        .unwrap_err();

        match err {
            MaterializeError::DigestMismatch { expected, actual } => {
                assert_eq!(expected, KNOWN_ANSWER_DIGEST);
                // If the check degenerated to a length fingerprint, `actual`
                // would equal `expected` here (the tampered archive is the
                // exact same length) and this test would never reach a red
                // failure at all — it would silently observe `Ok(_)` instead
                // of even reaching this match arm.
                assert_ne!(actual, expected, "tamper must change the computed digest");
            }
            other => panic!("expected DigestMismatch, got {other:?}"),
        }

        // Nothing was ever staged or promoted — a rejected archive must not
        // half-register a directory tree either.
        assert!(
            fs::read_dir(data_root.path()).unwrap().next().is_none(),
            "a digest failure must not leave anything on disk"
        );

        // Positive control: an UNTAMPERED materialize of the same archive,
        // against the same empty data root, must succeed — proving the
        // failure above is specific to the tamper, not to this test's setup.
        let ok_root = materialize_new_root(
            data_root.path(),
            &packed.archive,
            &packed.archive_digest,
            &packed.inventory,
        )
        .unwrap();
        assert_eq!(fs::read(ok_root.join("hello.txt")).unwrap(), b"hello world");
    }

    /// §19.2's split of "an interrupted materialization does not replace the
    /// previous verified root" into its WITH-previous-root half. A real crash
    /// midway is simulated with `InterruptPoint::AfterFilesWritten`, which
    /// lands only after files are already written to the staging directory
    /// (asserted below) — a failure at step zero would not exercise
    /// atomicity at all.
    ///
    /// What this test does NOT cover, and must not be read as covering: the
    /// previous root here lives at a DIFFERENT digest-named path from the
    /// one being materialized, so "the previous root was not replaced" holds
    /// here no matter how the promote step is written. The state in which
    /// that clause has teeth — a previous root at the SAME digest path, so a
    /// destructive promote really can delete it — is
    /// `promote_never_destroys_an_existing_verified_root_at_the_same_digest`.
    /// Measured: reintroducing the destructive `remove_dir_all` + `rename`
    /// promote leaves THIS test green and that one red.
    #[test]
    fn interrupted_materialization_keeps_the_previous_verified_root() {
        let data_root = tempfile::tempdir().unwrap();

        // Establish a previously-verified root the ordinary way.
        let v1_source = tempfile::tempdir().unwrap();
        let v1_inventory = variant_fixture(v1_source.path(), 1);
        let v1_packed = pack(v1_source.path(), &v1_inventory).unwrap();
        let previous_root = ensure_verified_builtin_root(
            data_root.path(),
            &v1_packed.archive,
            &v1_packed.archive_digest,
            &v1_packed.inventory,
            None,
        )
        .expect("first materialization has no previous root to fall back to and must succeed");
        let router_before = fs::read(previous_root.join("skills/router.md")).unwrap();
        assert_eq!(router_before, b"# router v1\n");

        // Attempt to materialize a genuinely different v2 bundle, but
        // interrupt after the first of three files is written.
        let v2_source = tempfile::tempdir().unwrap();
        let v2_inventory = variant_fixture(v2_source.path(), 2);
        let v2_packed = pack(v2_source.path(), &v2_inventory).unwrap();
        assert_ne!(
            v1_packed.archive_digest, v2_packed.archive_digest,
            "fixture bug: v1 and v2 must be different bundles"
        );

        let result = ensure_verified_builtin_root_with_fault_injection(
            data_root.path(),
            &v2_packed.archive,
            &v2_packed.archive_digest,
            &v2_packed.inventory,
            Some(&previous_root),
            Some(InterruptPoint::AfterFilesWritten(1)),
        )
        .expect("a previous verified root exists, so this must fall back to it, not error");

        // The caller's current verified root is still the OLD one, unchanged.
        assert_eq!(result, previous_root);
        let router_after = fs::read(previous_root.join("skills/router.md")).unwrap();
        assert_eq!(
            router_after, router_before,
            "the previous verified root's content must be untouched by the interrupted attempt"
        );

        // The v2 root must never have been promoted.
        let v2_root = promoted_root_path(data_root.path(), &v2_packed.archive_digest);
        assert!(
            !v2_root.exists(),
            "an interrupted materialization must never promote the new root"
        );

        // Prove this was a genuine midway interruption, not a no-op: exactly
        // one orphaned staging directory exists, holding exactly the one
        // file written before the injected fault fired.
        let staging_dirs: Vec<_> = fs::read_dir(data_root.path())
            .unwrap()
            .flatten()
            .filter(|entry| entry.file_name().to_string_lossy().starts_with(".staging-"))
            .collect();
        assert_eq!(
            staging_dirs.len(),
            1,
            "expected exactly one orphaned staging directory from the interrupted attempt"
        );
        let staged_files: Vec<_> = walk_files(&staging_dirs[0].path());
        assert_eq!(
            staged_files.len(),
            1,
            "the interrupted attempt must have written exactly one file before faulting"
        );
    }

    /// §19.2's split of the same condition into its WITHOUT-previous-root
    /// half — the case the design doc calls out as the one that actually
    /// matters, because it is where a panic or a half-registered root would
    /// do real damage. Uses the identical interruption mechanism as the
    /// previous test (mid-way, files already written) so the only variable
    /// between the two tests is presence/absence of a previous root.
    #[test]
    fn no_previous_root_and_failed_materialization_returns_builtin_bundle_unavailable() {
        let data_root = tempfile::tempdir().unwrap();
        let source = tempfile::tempdir().unwrap();
        let inventory = variant_fixture(source.path(), 1);
        let packed = pack(source.path(), &inventory).unwrap();

        let result = ensure_verified_builtin_root_with_fault_injection(
            data_root.path(),
            &packed.archive,
            &packed.archive_digest,
            &packed.inventory,
            None, // no previous verified root
            Some(InterruptPoint::AfterFilesWritten(1)),
        );

        match result {
            Err(BuiltinBundleError::BuiltinBundleUnavailable(detail)) => {
                assert!(
                    !detail.is_empty(),
                    "the error must carry the underlying reason"
                );
                assert!(
                    detail.contains("interrupted"),
                    "expected the underlying Interrupted cause to be named in the message, got: {detail}"
                );
            }
            other => panic!(
                "expected Err(BuiltinBundleUnavailable) with no previous root, got {other:?}"
            ),
        }

        // Never half-registered: no promoted root exists at all.
        let promoted = promoted_root_path(data_root.path(), &packed.archive_digest);
        assert!(
            !promoted.exists(),
            "a failed materialization with no previous root must not promote anything"
        );

        // Same real-interruption proof as the previous test: a partial
        // staging directory exists (this did not fail at step zero), it is
        // just never treated as verified.
        let staging_dirs: Vec<_> = fs::read_dir(data_root.path())
            .unwrap()
            .flatten()
            .filter(|entry| entry.file_name().to_string_lossy().starts_with(".staging-"))
            .collect();
        assert_eq!(staging_dirs.len(), 1);
        assert_eq!(walk_files(&staging_dirs[0].path()).len(), 1);
    }

    /// Every orphaned staging directory left under `data_root`. A staging
    /// directory that is still present is proof that a materialization was
    /// interrupted before it could promote — and that nothing promoted it.
    fn staging_dirs(data_root: &Path) -> Vec<PathBuf> {
        let mut dirs: Vec<PathBuf> = fs::read_dir(data_root)
            .unwrap()
            .flatten()
            .filter(|entry| entry.file_name().to_string_lossy().starts_with(".staging-"))
            .map(|entry| entry.path())
            .collect();
        dirs.sort();
        dirs
    }

    /// Recursively list every regular file under `dir` (test helper only —
    /// the production walk equivalent lives in the packer, which this module
    /// deliberately does not depend on for anything other than `pack`/
    /// `sha256_hex`/the inventory types).
    fn walk_files(dir: &Path) -> Vec<PathBuf> {
        let mut out = Vec::new();
        for entry in fs::read_dir(dir).unwrap().flatten() {
            let path = entry.path();
            if path.is_dir() {
                out.extend(walk_files(&path));
            } else {
                out.push(path);
            }
        }
        out
    }

    /// The per-file digest must be a CONTENT digest, and this test is
    /// written so that it cannot pass unless it is one.
    ///
    /// The obvious version of this test — declare an absurd hash such as 64
    /// zeroes, watch it be rejected — proves almost nothing: a per-file
    /// "digest" that had degenerated into a byte-count fingerprint also
    /// fails to equal 64 zeroes, so that test stays green while the check it
    /// names is worthless. (Measured, not assumed: replacing the per-file
    /// `sha256_hex` with `format!("{:x}", content.len())` left the
    /// zeroes-only version of this test GREEN.)
    ///
    /// So both directions are pinned to literal known-answer vectors
    /// instead: the ACCEPT direction against a real SHA-256 that a length
    /// fingerprint cannot produce, and the REJECT direction against a
    /// SAME-LENGTH one-byte substitution, with the COMPUTED digest asserted
    /// against its own literal.
    #[test]
    fn inventory_declared_hash_mismatch_is_rejected_even_when_archive_digest_matches() {
        // ACCEPT: correct content, inventory pinned to a literal SHA-256.
        let good_source = tempfile::tempdir().unwrap();
        write_file(good_source.path(), "hello.txt", b"hello world");
        let good = pack(good_source.path(), &[InventoryEntry::new("hello.txt")]).unwrap();
        assert_eq!(
            good.inventory[0].sha256, HELLO_WORLD_SHA256,
            "fixture bug: the packer's per-file digest is not the known answer"
        );
        let accept_root = tempfile::tempdir().unwrap();
        let root = materialize_new_root(
            accept_root.path(),
            &good.archive,
            &good.archive_digest,
            &[PackedFile {
                path: "hello.txt".to_string(),
                bytes: 11,
                sha256: HELLO_WORLD_SHA256.to_string(),
            }],
        )
        .expect("content matching its pinned known-answer digest must be accepted");
        assert_eq!(fs::read(root.join("hello.txt")).unwrap(), b"hello world");

        // REJECT: a one-byte, SAME-LENGTH substitution. The archive is
        // repacked so its OWN whole-archive digest matches (the outer gate
        // passes and the per-file check is genuinely reached), and the
        // inventory still declares the original content's digest.
        let bad_source = tempfile::tempdir().unwrap();
        write_file(bad_source.path(), "hello.txt", b"hello worle");
        let bad = pack(bad_source.path(), &[InventoryEntry::new("hello.txt")]).unwrap();
        assert_eq!(
            bad.archive.len(),
            good.archive.len(),
            "fixture bug: the substitution must hold the archive length constant"
        );
        assert_ne!(bad.archive_digest, good.archive_digest);

        let reject_root = tempfile::tempdir().unwrap();
        let err = materialize_new_root(
            reject_root.path(),
            &bad.archive,
            &bad.archive_digest, // whole-archive digest matches these bytes
            &[PackedFile {
                path: "hello.txt".to_string(),
                bytes: 11, // byte count matches too — only the CONTENT differs
                sha256: HELLO_WORLD_SHA256.to_string(),
            }],
        )
        .unwrap_err();

        match err {
            MaterializeError::PerFileDigestMismatch {
                path,
                expected,
                actual,
            } => {
                assert_eq!(path, "hello.txt");
                assert_eq!(expected, HELLO_WORLD_SHA256);
                // The load-bearing assertion: the digest actually COMPUTED
                // from the record's bytes is the known answer for those
                // bytes. Any size- or count-derived stand-in fails here.
                assert_eq!(actual, HELLO_WORLE_SHA256);
            }
            other => panic!("expected PerFileDigestMismatch, got {other:?}"),
        }
        assert!(
            !promoted_root_path(reject_root.path(), &bad.archive_digest).exists(),
            "a per-file digest failure must not promote a root"
        );
        assert_eq!(
            staging_dirs(reject_root.path()).len(),
            0,
            "a per-file digest failure is caught before anything is staged"
        );
    }

    /// Idempotent-shape sanity: materializing the SAME archive twice with no
    /// previous root supplied the second time still succeeds and produces
    /// the same digest-named root path both times. This calls the RAW
    /// `materialize_new_root` directly, which always fully re-decodes and
    /// re-verifies (it has no short-circuit of its own — P1.5's short-circuit
    /// lives one layer up, in `ensure_verified_builtin_root`, and is covered
    /// by that function's own tests below). What this test pins is narrower
    /// but still real: promotion of an identical archive twice, with full
    /// re-verification both times, must not itself be an error.
    #[test]
    fn materializing_the_same_archive_twice_reaches_the_same_root_path() {
        let source = tempfile::tempdir().unwrap();
        let inventory = variant_fixture(source.path(), 1);
        let packed = pack(source.path(), &inventory).unwrap();
        let data_root = tempfile::tempdir().unwrap();

        let first = materialize_new_root(
            data_root.path(),
            &packed.archive,
            &packed.archive_digest,
            &packed.inventory,
        )
        .unwrap();
        let second = materialize_new_root(
            data_root.path(),
            &packed.archive,
            &packed.archive_digest,
            &packed.inventory,
        )
        .unwrap();

        assert_eq!(first, second);
        assert_eq!(
            fs::read(first.join("plugin.json")).unwrap(),
            br#"{"variant":1}"#
        );
    }

    /// The second materialization of an identical archive must KEEP the
    /// already-promoted root rather than removing and re-creating it. The
    /// sentinel is what makes this observable: a `remove_dir_all` + `rename`
    /// promote reproduces every inventory file byte-for-byte, so comparing
    /// file contents alone cannot tell "kept" from "destroyed and rebuilt".
    /// A file that only ever existed in the OLD directory can.
    #[test]
    fn re_promoting_an_identical_archive_keeps_the_existing_root_directory() {
        let source = tempfile::tempdir().unwrap();
        let inventory = variant_fixture(source.path(), 1);
        let packed = pack(source.path(), &inventory).unwrap();
        let data_root = tempfile::tempdir().unwrap();

        let first = materialize_new_root(
            data_root.path(),
            &packed.archive,
            &packed.archive_digest,
            &packed.inventory,
        )
        .unwrap();
        fs::write(first.join("sentinel.marker"), b"i was here").unwrap();

        let second = materialize_new_root(
            data_root.path(),
            &packed.archive,
            &packed.archive_digest,
            &packed.inventory,
        )
        .unwrap();
        assert_eq!(first, second);
        assert_eq!(
            fs::read(first.join("sentinel.marker")).unwrap(),
            b"i was here",
            "the existing verified root was destroyed and re-created rather than kept"
        );

        // And the redundant staging copy must not be left lying around.
        assert_eq!(
            staging_dirs(data_root.path()).len(),
            0,
            "the discarded staging directory must be cleaned up on the keep path"
        );
    }

    /// §19.2's atomicity clause at the point where it actually has teeth. A
    /// promote implemented as "`remove_dir_all` the old root, then `rename`
    /// the new one in" destroys the previous verified root BEFORE the new one
    /// exists; a crash in that window leaves neither — the mixture state
    /// atomicity forbids. This drives a fault into exactly that window while
    /// a previous verified root sits at the SAME digest-named path (the
    /// re-materialization case: app relaunch, re-verification), which the
    /// two required interruption tests cannot reach because they interrupt
    /// during the staging write, long before any promote.
    ///
    /// The "case under test" section below calls
    /// `materialize_new_root_with_fault_injection` DIRECTLY rather than
    /// through `ensure_verified_builtin_root_with_fault_injection` — as it did
    /// before P1.5. That is deliberate, not incidental: P1.5's short-circuit
    /// (`short_circuit_candidate`) now recognizes this exact scenario (a
    /// manifest-backed root already promoted at this digest) and returns it
    /// WITHOUT ever calling this function at all, which would make the
    /// `BeforePromote` fault below unreachable and this test vacuous — it
    /// would keep passing for a reason that has nothing to do with the
    /// atomicity property it names. Calling the lower-level function directly
    /// keeps this test exercising exactly what it always exercised: the
    /// keep-don't-destroy branch inside the raw materializer, independent of
    /// the higher-level short-circuit that now sits in front of it.
    #[test]
    fn promote_never_destroys_an_existing_verified_root_at_the_same_digest() {
        let source = tempfile::tempdir().unwrap();
        let inventory = variant_fixture(source.path(), 1);
        let packed = pack(source.path(), &inventory).unwrap();

        // POSITIVE CONTROL, first: on a data root with NO existing promoted
        // root, `BeforePromote` must genuinely fire and fail. Without this,
        // the main assertion below could pass simply because the fault point
        // is unreachable dead code.
        let fresh = tempfile::tempdir().unwrap();
        let control = ensure_verified_builtin_root_with_fault_injection(
            fresh.path(),
            &packed.archive,
            &packed.archive_digest,
            &packed.inventory,
            None,
            Some(InterruptPoint::BeforePromote),
        );
        match control {
            Err(BuiltinBundleError::BuiltinBundleUnavailable(detail)) => assert!(
                detail.contains("interrupted after writing 3 file(s)"),
                "the promote-time fault must report a COMPLETE staging set, got: {detail}"
            ),
            other => panic!("BeforePromote fault did not fire: got {other:?}"),
        }
        assert!(
            !promoted_root_path(fresh.path(), &packed.archive_digest).exists(),
            "a fault at the promote boundary must not promote anything"
        );
        assert_eq!(
            walk_files(&staging_dirs(fresh.path())[0]).len(),
            3,
            "BeforePromote must land with the staging set COMPLETE, unlike AfterFilesWritten"
        );

        // The case under test: the same digest is already promoted.
        let data_root = tempfile::tempdir().unwrap();
        let previous_root = materialize_new_root(
            data_root.path(),
            &packed.archive,
            &packed.archive_digest,
            &packed.inventory,
        )
        .unwrap();
        let router_before = fs::read(previous_root.join("skills/router.md")).unwrap();
        fs::write(previous_root.join("sentinel.marker"), b"i was here").unwrap();

        // Direct call to the raw materializer (see the doc comment above for
        // why): this is the seam that actually owns the keep-vs-destroy
        // decision this test is about.
        let result = materialize_new_root_with_fault_injection(
            data_root.path(),
            &packed.archive,
            &packed.archive_digest,
            &packed.inventory,
            Some(InterruptPoint::BeforePromote),
        )
        .expect("re-materializing an already-promoted digest must not fail");

        assert_eq!(result, previous_root);
        assert!(
            previous_root.is_dir(),
            "the previous verified root was destroyed by a re-promote"
        );
        assert_eq!(
            fs::read(previous_root.join("sentinel.marker")).unwrap(),
            b"i was here",
            "the previous verified root was removed and re-created, not kept"
        );
        assert_eq!(
            fs::read(previous_root.join("skills/router.md")).unwrap(),
            router_before
        );
    }

    /// A previous root that no longer exists on disk (deleted by the OS,
    /// wiped by the user, never really promoted) must NOT be handed back as
    /// though it were verified. This is a third distinct state from the two
    /// required tests: `Some(path)` supplied, but the path is not a
    /// directory — the shape in which a caller would receive a dangling
    /// "verified" root and treat it as usable.
    #[test]
    fn a_previous_root_that_no_longer_exists_is_not_returned_as_verified() {
        let source = tempfile::tempdir().unwrap();
        let inventory = variant_fixture(source.path(), 1);
        let packed = pack(source.path(), &inventory).unwrap();
        let data_root = tempfile::tempdir().unwrap();
        let vanished = data_root.path().join("root-that-was-deleted");

        let result = ensure_verified_builtin_root_with_fault_injection(
            data_root.path(),
            &packed.archive,
            &packed.archive_digest,
            &packed.inventory,
            Some(&vanished),
            Some(InterruptPoint::AfterFilesWritten(1)),
        );
        match result {
            Err(BuiltinBundleError::BuiltinBundleUnavailable(_)) => {}
            other => panic!(
                "expected BuiltinBundleUnavailable for a vanished previous root, got {other:?}"
            ),
        }

        // Positive control: the SAME call with that path actually present
        // falls back to it successfully, so the rejection above is caused by
        // the path's absence and nothing else.
        fs::create_dir_all(&vanished).unwrap();
        let fallback = ensure_verified_builtin_root_with_fault_injection(
            data_root.path(),
            &packed.archive,
            &packed.archive_digest,
            &packed.inventory,
            Some(&vanished),
            Some(InterruptPoint::AfterFilesWritten(1)),
        )
        .expect("an existing previous root must be returned as the fallback");
        assert_eq!(fallback, vanished);
    }

    /// Build a one-record archive header naming `hello.txt` with an
    /// arbitrary declared content length and no content bytes behind it, plus
    /// the digest of exactly those bytes — so the whole-archive gate passes
    /// and the DECODER is what the test actually reaches.
    fn header_only_archive(declared_content_len: u64) -> (Vec<u8>, String) {
        let mut archive = Vec::new();
        archive.extend_from_slice(&(9u32).to_le_bytes());
        archive.extend_from_slice(b"hello.txt");
        archive.extend_from_slice(&declared_content_len.to_le_bytes());
        let digest = local_apps::sha256_hex(&archive);
        (archive, digest)
    }

    fn decode_failure(declared_content_len: u64) -> String {
        let (archive, digest) = header_only_archive(declared_content_len);
        let data_root = tempfile::tempdir().unwrap();
        let err = materialize_new_root(
            data_root.path(),
            &archive,
            &digest,
            &[PackedFile {
                path: "hello.txt".to_string(),
                bytes: 11,
                sha256: HELLO_WORLD_SHA256.to_string(),
            }],
        )
        .unwrap_err();
        match err {
            MaterializeError::MalformedArchive(detail) => detail,
            other => panic!("expected MalformedArchive, got {other:?}"),
        }
    }

    /// A malformed archive must be REJECTED with a typed error, never panic.
    /// `content_len` is a `u64` read straight out of the archive:
    ///
    /// - adding it to the running offset without a checked add overflows
    ///   `usize` and panics in debug rather than returning the typed error
    ///   §19.2 requires;
    /// - narrowing it with `as usize` TRUNCATES on a 32-bit target — and
    ///   harness-runtime::mobile ships to one (Android armv7) — turning a record that
    ///   must be rejected into one that is accepted with the wrong bytes.
    #[test]
    fn a_length_field_that_overflows_the_offset_is_rejected_rather_than_panicking() {
        // Pointer-width-independent assertion: on a 64-bit target this trips
        // the checked add, on a 32-bit one the `usize::try_from`. Both name
        // the offending record.
        let detail = decode_failure(u64::MAX);
        assert!(
            detail.contains("hello.txt"),
            "the rejection must name the offending record, got: {detail}"
        );
        assert!(
            detail.contains(&u64::MAX.to_string()),
            "the rejection must report the length the archive DECLARED, not a \
             truncated version of it, got: {detail}"
        );

        // A merely-oversized length behaves identically on every pointer
        // width, so the exact wording can be pinned here.
        let detail = decode_failure(1000);
        assert!(
            detail.contains("truncated content bytes (1000 declared) for hello.txt"),
            "unexpected rejection detail: {detail}"
        );

        // Positive control: the same header shape with a length that DOES
        // match the bytes present decodes fine, so the rejections above are
        // caused by the length field and not by this hand-built archive
        // being unparseable in general.
        let mut archive = Vec::new();
        archive.extend_from_slice(&(9u32).to_le_bytes());
        archive.extend_from_slice(b"hello.txt");
        archive.extend_from_slice(&(11u64).to_le_bytes());
        archive.extend_from_slice(b"hello world");
        let digest = local_apps::sha256_hex(&archive);
        assert_eq!(
            digest, KNOWN_ANSWER_DIGEST,
            "the hand-built archive must be byte-identical to the packer's own"
        );
        let data_root = tempfile::tempdir().unwrap();
        let root = materialize_new_root(
            data_root.path(),
            &archive,
            &digest,
            &[PackedFile {
                path: "hello.txt".to_string(),
                bytes: 11,
                sha256: HELLO_WORLD_SHA256.to_string(),
            }],
        )
        .expect("a well-formed hand-built archive must decode");
        assert_eq!(fs::read(root.join("hello.txt")).unwrap(), b"hello world");
    }

    /// The 32-bit truncation guard, exercised on whatever host runs the
    /// suite. Measured, and the reason this test is written against the
    /// predicate rather than only through `materialize_new_root`: restoring
    /// the unsafe `as usize` narrowing leaves EVERY end-to-end test in this
    /// module green on a 64-bit host, because there the narrowing is the
    /// identity. Passing the 32-bit bound explicitly is what makes the
    /// rejection observable — and falsifiable — here.
    #[test]
    fn a_length_no_32_bit_target_can_address_is_rejected_on_any_host() {
        const U32_MAX: u64 = u32::MAX as u64;

        // Rejected on a 32-bit target: `as usize` would have silently turned
        // this into 5 and accepted a five-byte record.
        assert_eq!(narrow_declared_len((1u64 << 32) + 5, U32_MAX), None);
        assert_eq!(narrow_declared_len(U32_MAX + 1, U32_MAX), None);

        // Positive control: values a 32-bit target CAN address still narrow,
        // so the rejections above are about the bound and not about the
        // function refusing everything.
        assert_eq!(narrow_declared_len(5, U32_MAX), Some(5));
        assert_eq!(
            narrow_declared_len(U32_MAX, U32_MAX),
            Some(u32::MAX as usize)
        );
        assert_eq!(narrow_declared_len(0, U32_MAX), Some(0));

        // And the bound production actually passes accepts a real length.
        assert_eq!(narrow_declared_len(11, usize::MAX as u64), Some(11));
    }

    /// A declared byte count that disagrees with the record's actual length
    /// must be rejected, and the comparison must not narrow the inventory's
    /// `u64` to `usize` — on a 32-bit target `2^32 + 11` would then compare
    /// equal to an 11-byte record and slip straight through to the digest
    /// check with a bogus declared size.
    #[test]
    fn a_declared_byte_count_that_truncates_to_the_real_one_is_still_rejected() {
        let source = tempfile::tempdir().unwrap();
        write_file(source.path(), "hello.txt", b"hello world");
        let packed = pack(source.path(), &[InventoryEntry::new("hello.txt")]).unwrap();
        let data_root = tempfile::tempdir().unwrap();

        let err = materialize_new_root(
            data_root.path(),
            &packed.archive,
            &packed.archive_digest,
            &[PackedFile {
                path: "hello.txt".to_string(),
                bytes: (1u64 << 32) + 11,
                sha256: HELLO_WORLD_SHA256.to_string(),
            }],
        )
        .unwrap_err();
        match err {
            MaterializeError::MalformedArchive(detail) => assert!(
                detail.contains("inventory declares 4294967307 byte(s), archive holds 11"),
                "unexpected rejection detail: {detail}"
            ),
            other => panic!("expected MalformedArchive, got {other:?}"),
        }
    }

    // -----------------------------------------------------------------
    // P1.5 — the §6.2 idempotent short-circuit.
    // -----------------------------------------------------------------

    /// Read back a promoted root's §6.2 manifest as a test-mutable struct —
    /// the same struct, from the same file, `write_manifest` produced — so a
    /// test can flip exactly ONE field and write it back with
    /// `overwrite_manifest`, holding every other piece of evidence genuinely
    /// valid.
    fn read_manifest(data_root: &Path, expected_digest: &str) -> RootManifest {
        let bytes = fs::read(manifest_path(data_root, expected_digest)).unwrap();
        serde_json::from_slice(&bytes).unwrap()
    }

    /// Write a (possibly test-corrupted) manifest back to exactly the path
    /// `write_manifest` would have used.
    fn overwrite_manifest(data_root: &Path, expected_digest: &str, manifest: &RootManifest) {
        let bytes = serde_json::to_vec(manifest).unwrap();
        fs::write(manifest_path(data_root, expected_digest), bytes).unwrap();
    }

    /// The §6.2 short-circuit's core promise: a second `ensure_verified_
    /// builtin_root` call for a digest that has not changed must not decode or
    /// unpack the archive. It does re-hash the promoted files, because cached
    /// executable Plugin content remains writable. "It returned `Ok`"
    /// cannot by itself prove that — a full re-verification that happens to
    /// land on the existing-root keep branch (`materialize_new_root_with_
    /// fault_injection`'s `final_root.is_dir()` check) ALSO returns `Ok`
    /// with the same root path.
    ///
    /// Two observables that are genuinely decisive are used instead:
    ///
    /// 1. instrumentation-free, and the one that pins the actual §6.2 words
    ///    "does not re-decode the archive": the second call is handed an
    ///    archive of GARBAGE bytes while still naming the FIRST archive's
    ///    digest. Anything that so much as hashes `archive` computes a
    ///    mismatch and — with no previous verified root supplied — returns
    ///    `BuiltinBundleUnavailable`. Only an implementation that never
    ///    looks at `archive` can return `Ok`. A positive control on a fresh
    ///    `data_root` proves that probe genuinely fires rather than being
    ///    inert;
    /// 2. `full_materialize_attempts()`, a thread-local counter incremented
    ///    at the very top of the decode/verify function, asserted to be
    ///    EXACTLY unchanged.
    ///
    /// The required inverse pairing: a THIRD phase materializes a genuinely
    /// DIFFERENT archive (a different digest) into the SAME `data_root` and
    /// confirms the counter advances by exactly one and the returned root
    /// holds that archive's own content — proving the short-circuit
    /// recognizes an UNCHANGED digest specifically, not merely "this
    /// `data_root` already has something in it" (an implementation that
    /// always skips once anything is promoted would pass phases 1-2 above
    /// but fail this one).
    #[test]
    fn unchanged_digest_skips_unpack() {
        let source = tempfile::tempdir().unwrap();
        let inventory = variant_fixture(source.path(), 1);
        let packed = pack(source.path(), &inventory).unwrap();
        let data_root = tempfile::tempdir().unwrap();

        let first = ensure_verified_builtin_root(
            data_root.path(),
            &packed.archive,
            &packed.archive_digest,
            &packed.inventory,
            None,
        )
        .unwrap();
        assert_eq!(
            fs::read(first.join("plugin.json")).unwrap(),
            br#"{"variant":1}"#
        );

        // The probe for observable (1): the same length, every byte flipped,
        // so it cannot possibly hash to `packed.archive_digest`.
        let garbage: Vec<u8> = packed.archive.iter().map(|b| b ^ 0xFF).collect();
        assert_eq!(
            garbage.len(),
            packed.archive.len(),
            "fixture bug: the probe must not be distinguishable by length"
        );
        let garbage_digest = local_apps::sha256_hex(&garbage);
        assert_ne!(
            garbage_digest, packed.archive_digest,
            "fixture bug: the probe archive must not hash to the digest it claims"
        );

        // POSITIVE CONTROL for that probe, on a data root holding no
        // promoted root at all: reading the archive is unavoidable there, so
        // the mismatch MUST surface. Without this, the `Ok` asserted below
        // could mean the probe is simply inert.
        let fresh = tempfile::tempdir().unwrap();
        match ensure_verified_builtin_root(
            fresh.path(),
            &garbage,
            &packed.archive_digest,
            &packed.inventory,
            None,
        ) {
            Err(BuiltinBundleError::BuiltinBundleUnavailable(detail)) => assert!(
                detail.contains("archive digest mismatch")
                    && detail.contains(&packed.archive_digest),
                "the probe must be rejected by the DIGEST gate specifically, got: {detail}"
            ),
            other => panic!("probe is inert: a garbage archive was not rejected, got {other:?}"),
        }

        let attempts_before = full_materialize_attempts();
        let second = ensure_verified_builtin_root(
            data_root.path(),
            &garbage, // <- never looked at, if the short-circuit is real
            &packed.archive_digest,
            &packed.inventory,
            None,
        )
        .expect(
            "an unchanged digest must be served from the promoted root without ever \
             decoding — or even hashing — the archive handed in",
        );

        assert_eq!(
            second, first,
            "the same digest must resolve to the same root"
        );
        assert_eq!(
            full_materialize_attempts(),
            attempts_before,
            "an unchanged digest must not re-enter the archive decode/verify path at all"
        );
        assert_eq!(
            fs::read(first.join("plugin.json")).unwrap(),
            br#"{"variant":1}"#,
            "the verified cached root must remain byte-identical"
        );

        // A same-length content replacement must defeat the cache evidence and
        // be repaired from the trusted embedded archive. This is the case a
        // type/length-only cache check silently accepted.
        let same_length_tamper = br#"{"variant":9}"#;
        assert_eq!(same_length_tamper.len(), br#"{"variant":1}"#.len());
        fs::write(first.join("plugin.json"), same_length_tamper).unwrap();
        assert_eq!(
            short_circuit_candidate(data_root.path(), &packed.archive_digest, &packed.inventory),
            None,
            "same-length tampering must invalidate the verified-root cache"
        );
        let attempts_before_repair = full_materialize_attempts();
        let repaired = ensure_verified_builtin_root(
            data_root.path(),
            &packed.archive,
            &packed.archive_digest,
            &packed.inventory,
            None,
        )
        .expect("the embedded archive must repair a same-length replacement");
        assert_eq!(repaired, first);
        assert_eq!(
            fs::read(first.join("plugin.json")).unwrap(),
            br#"{"variant":1}"#
        );
        assert_eq!(full_materialize_attempts(), attempts_before_repair + 1);

        // The required inverse: a DIFFERENT archive must NOT be served from
        // the first archive's cached evidence.
        let v2_source = tempfile::tempdir().unwrap();
        let v2_inventory = variant_fixture(v2_source.path(), 2);
        let v2_packed = pack(v2_source.path(), &v2_inventory).unwrap();
        assert_ne!(
            packed.archive_digest, v2_packed.archive_digest,
            "fixture bug: v1 and v2 must be different bundles"
        );

        let attempts_before_v2 = full_materialize_attempts();
        let third = ensure_verified_builtin_root(
            data_root.path(),
            &v2_packed.archive,
            &v2_packed.archive_digest,
            &v2_packed.inventory,
            None,
        )
        .unwrap();

        assert_ne!(
            third, first,
            "a changed digest must promote a DIFFERENT root"
        );
        assert_eq!(
            full_materialize_attempts(),
            attempts_before_v2 + 1,
            "a changed digest must genuinely re-enter the decode/verify path exactly \
             once, not be served from the first archive's cached evidence"
        );
        assert_eq!(
            fs::read(third.join("plugin.json")).unwrap(),
            br#"{"variant":2}"#,
            "the second archive's own content must actually have been unpacked"
        );
    }

    #[test]
    fn missing_cached_component_is_repaired_from_the_embedded_archive() {
        let source = tempfile::tempdir().unwrap();
        let inventory = variant_fixture(source.path(), 1);
        let packed = pack(source.path(), &inventory).unwrap();
        let data_root = tempfile::tempdir().unwrap();

        let root = ensure_verified_builtin_root(
            data_root.path(),
            &packed.archive,
            &packed.archive_digest,
            &packed.inventory,
            None,
        )
        .unwrap();
        let missing = root.join("skills/router.md");
        let expected = fs::read(&missing).unwrap();
        fs::remove_file(&missing).unwrap();
        assert_eq!(
            short_circuit_candidate(data_root.path(), &packed.archive_digest, &packed.inventory),
            None,
            "a manifest cannot make a missing component trusted"
        );

        let attempts_before = full_materialize_attempts();
        let repaired = ensure_verified_builtin_root(
            data_root.path(),
            &packed.archive,
            &packed.archive_digest,
            &packed.inventory,
            None,
        )
        .expect("the archive must replace a corrupt digest-addressed root");
        assert_eq!(repaired, root);
        assert_eq!(fs::read(missing).unwrap(), expected);
        assert_eq!(full_materialize_attempts(), attempts_before + 1);
    }

    /// One of §6.2's five INDEPENDENT defeat conditions: the manifest's
    /// `digest` marker field does not name this call's `expected_digest`.
    /// Every other piece of evidence — the directory, the component list —
    /// is held genuinely valid, so a check that quietly didn't exist could
    /// not be hiding behind one of its neighbors firing instead. (This test
    /// is not in the task's named list, but the design doc's prose names
    /// the marker field as its own required defeat condition alongside the
    /// four that are named, so it gets the same isolated treatment here.)
    #[test]
    fn a_wrong_marker_field_defeats_the_short_circuit() {
        let source = tempfile::tempdir().unwrap();
        let inventory = variant_fixture(source.path(), 1);
        let packed = pack(source.path(), &inventory).unwrap();
        let data_root = tempfile::tempdir().unwrap();

        let root = materialize_new_root(
            data_root.path(),
            &packed.archive,
            &packed.archive_digest,
            &packed.inventory,
        )
        .unwrap();

        // Positive control: the untouched, genuinely valid manifest DOES
        // short-circuit — proves this fixture would pass if not corrupted.
        assert_eq!(
            short_circuit_candidate(data_root.path(), &packed.archive_digest, &packed.inventory),
            Some(root.clone()),
            "fixture bug: a genuinely valid manifest must short-circuit"
        );

        let mut manifest = read_manifest(data_root.path(), &packed.archive_digest);
        manifest.digest = "0".repeat(64);
        overwrite_manifest(data_root.path(), &packed.archive_digest, &manifest);

        assert_eq!(
            short_circuit_candidate(data_root.path(), &packed.archive_digest, &packed.inventory),
            None,
            "a manifest whose marker field names a different digest must not be trusted"
        );

        // The caller-visible entry point must genuinely re-verify rather
        // than silently trust the corrupted marker.
        let attempts_before = full_materialize_attempts();
        let result = ensure_verified_builtin_root(
            data_root.path(),
            &packed.archive,
            &packed.archive_digest,
            &packed.inventory,
            None,
        )
        .expect("a valid archive must still materialize despite a corrupted cached marker");
        assert_eq!(result, root);
        assert_eq!(
            full_materialize_attempts(),
            attempts_before + 1,
            "defeating the short-circuit must fall through to exactly one full \
             decode-and-verify pass"
        );
        assert_eq!(
            read_manifest(data_root.path(), &packed.archive_digest).digest,
            packed.archive_digest,
            "the corrupted marker must be repaired by the fresh materialization"
        );
    }

    /// One of §6.2's five independent defeat conditions: the manifest
    /// sibling file itself is gone (never written, or removed) while the
    /// promoted root directory is untouched and genuinely valid.
    #[test]
    fn a_missing_manifest_defeats_the_short_circuit() {
        let source = tempfile::tempdir().unwrap();
        let inventory = variant_fixture(source.path(), 1);
        let packed = pack(source.path(), &inventory).unwrap();
        let data_root = tempfile::tempdir().unwrap();

        let root = materialize_new_root(
            data_root.path(),
            &packed.archive,
            &packed.archive_digest,
            &packed.inventory,
        )
        .unwrap();

        assert_eq!(
            short_circuit_candidate(data_root.path(), &packed.archive_digest, &packed.inventory),
            Some(root.clone()),
            "fixture bug: a genuinely valid manifest must short-circuit"
        );

        fs::remove_file(manifest_path(data_root.path(), &packed.archive_digest)).unwrap();

        assert_eq!(
            short_circuit_candidate(data_root.path(), &packed.archive_digest, &packed.inventory),
            None,
            "a promoted root with no manifest at all must not be trusted"
        );

        let attempts_before = full_materialize_attempts();
        let result = ensure_verified_builtin_root(
            data_root.path(),
            &packed.archive,
            &packed.archive_digest,
            &packed.inventory,
            None,
        )
        .expect("a valid archive must still materialize with no cached manifest");
        assert_eq!(result, root);
        assert_eq!(
            full_materialize_attempts(),
            attempts_before + 1,
            "defeating the short-circuit must fall through to exactly one full \
             decode-and-verify pass"
        );
        assert!(
            manifest_path(data_root.path(), &packed.archive_digest).exists(),
            "the missing manifest must be recreated by the fresh materialization"
        );
    }

    /// One of §6.2's five independent defeat conditions: the manifest
    /// exists with a correct marker field, but its component list is
    /// EMPTY — no per-file inventory was ever recorded. This is the
    /// total-loss extreme of the SAME per-entry lookup the next test below
    /// exercises with one entry missing rather than all of them (there is
    /// no separate "is it empty" branch in `short_circuit_candidate` — an
    /// empty component map fails the very first lookup the loop makes) —
    /// still an independently meaningful, independently named scenario:
    /// this test corrupts only "clear every component", holding the
    /// directory, the manifest's presence, and its marker field genuinely
    /// valid, so it stands on its own regardless of what the next test
    /// proves.
    #[test]
    fn a_missing_inventory_defeats_the_short_circuit() {
        let source = tempfile::tempdir().unwrap();
        let inventory = variant_fixture(source.path(), 1);
        let packed = pack(source.path(), &inventory).unwrap();
        let data_root = tempfile::tempdir().unwrap();

        let root = materialize_new_root(
            data_root.path(),
            &packed.archive,
            &packed.archive_digest,
            &packed.inventory,
        )
        .unwrap();

        assert_eq!(
            short_circuit_candidate(data_root.path(), &packed.archive_digest, &packed.inventory),
            Some(root.clone()),
            "fixture bug: a genuinely valid manifest must short-circuit"
        );

        let mut manifest = read_manifest(data_root.path(), &packed.archive_digest);
        assert!(
            !manifest.components.is_empty(),
            "fixture bug: nothing to empty"
        );
        manifest.components.clear();
        overwrite_manifest(data_root.path(), &packed.archive_digest, &manifest);

        assert_eq!(
            short_circuit_candidate(data_root.path(), &packed.archive_digest, &packed.inventory),
            None,
            "a manifest with no recorded inventory at all must not be trusted"
        );

        let attempts_before = full_materialize_attempts();
        let result = ensure_verified_builtin_root(
            data_root.path(),
            &packed.archive,
            &packed.archive_digest,
            &packed.inventory,
            None,
        )
        .expect("a valid archive must still materialize with an emptied cached inventory");
        assert_eq!(result, root);
        assert_eq!(
            full_materialize_attempts(),
            attempts_before + 1,
            "defeating the short-circuit must fall through to exactly one full \
             decode-and-verify pass"
        );
        // Name every restored entry rather than just counting them.
        let repaired = read_manifest(data_root.path(), &packed.archive_digest);
        for entry in &packed.inventory {
            let restored = repaired
                .components
                .iter()
                .find(|c| c.path == entry.path)
                .unwrap_or_else(|| panic!("{} was not repopulated", entry.path));
            assert_eq!(restored.bytes, entry.bytes, "{}", entry.path);
            assert_eq!(restored.sha256, entry.sha256, "{}", entry.path);
        }
        assert_eq!(repaired.components.len(), packed.inventory.len());
    }

    /// One of §6.2's five independent defeat conditions: the manifest
    /// exists, its marker field is correct, and its component list is
    /// non-empty — but it is missing the entry for ONE specific path the
    /// caller's inventory still requires. The other entries stay present
    /// and correct, so this is genuinely distinct from the empty-inventory
    /// case above (a single check covering both would leave one of them
    /// unfalsifiable).
    #[test]
    fn a_missing_component_entry_defeats_the_short_circuit() {
        let source = tempfile::tempdir().unwrap();
        let inventory = variant_fixture(source.path(), 1);
        let packed = pack(source.path(), &inventory).unwrap();
        assert!(
            packed.inventory.len() >= 3,
            "fixture bug: need at least 2 remaining entries after dropping 1"
        );
        let data_root = tempfile::tempdir().unwrap();

        let root = materialize_new_root(
            data_root.path(),
            &packed.archive,
            &packed.archive_digest,
            &packed.inventory,
        )
        .unwrap();

        assert_eq!(
            short_circuit_candidate(data_root.path(), &packed.archive_digest, &packed.inventory),
            Some(root.clone()),
            "fixture bug: a genuinely valid manifest must short-circuit"
        );

        let mut manifest = read_manifest(data_root.path(), &packed.archive_digest);
        let dropped_path = "skills/router.md";
        // Anti-`ordered[0]` guard. A check that inspected only the FIRST
        // inventory entry would be vacuously satisfied by a fixture whose
        // corrupted entry always sorts first — the exact shape that left
        // eighteen packer tests green around a guard reading `ordered[0]`.
        // Pin that the entry this test drops is NOT the one a first-entry-only
        // implementation would look at.
        assert_ne!(
            packed.inventory[0].path, dropped_path,
            "fixture bug: the dropped component must not be the first inventory entry, \
             or a first-entry-only check would pass this test vacuously"
        );
        let dropped = packed
            .inventory
            .iter()
            .find(|e| e.path == dropped_path)
            .expect("fixture bug: the dropped path must be in the inventory")
            .clone();
        let before_len = manifest.components.len();
        manifest.components.retain(|c| c.path != dropped_path);
        assert_eq!(
            manifest.components.len(),
            before_len - 1,
            "fixture bug: the target component was not present to drop"
        );
        overwrite_manifest(data_root.path(), &packed.archive_digest, &manifest);

        assert_eq!(
            short_circuit_candidate(data_root.path(), &packed.archive_digest, &packed.inventory),
            None,
            "a manifest missing ONE component the caller's inventory still requires \
             must not be trusted, even with every other entry intact"
        );

        let attempts_before = full_materialize_attempts();
        let result = ensure_verified_builtin_root(
            data_root.path(),
            &packed.archive,
            &packed.archive_digest,
            &packed.inventory,
            None,
        )
        .expect("a valid archive must still materialize with a partially-truncated inventory");
        assert_eq!(result, root);
        assert_eq!(
            full_materialize_attempts(),
            attempts_before + 1,
            "defeating the short-circuit must fall through to exactly one full \
             decode-and-verify pass"
        );
        // Name what was restored, not merely how many things there are: a
        // length check alone would be satisfied by any entry at all
        // reappearing under any path.
        let repaired = read_manifest(data_root.path(), &packed.archive_digest);
        let restored = repaired
            .components
            .iter()
            .find(|c| c.path == dropped_path)
            .expect("the dropped entry must be restored by the fresh materialization");
        assert_eq!(restored.bytes, dropped.bytes);
        assert_eq!(restored.sha256, dropped.sha256);
        assert_eq!(repaired.components.len(), packed.inventory.len());
    }

    /// The component comparison's CONTENT half, which no other test in this
    /// module reaches. `short_circuit_candidate` accepts a recorded component
    /// only when its `bytes` AND its `sha256` both agree with what the
    /// caller's inventory declares — but every other test here leaves both
    /// fields untouched, so both sub-conditions are vacuously true in every
    /// state they exercise. (Measured, not assumed: deleting
    /// `component.sha256 == entry.sha256` from the guard left all seventeen
    /// of those tests GREEN.)
    ///
    /// This is not a cosmetic gap. The manifest is the ONLY evidence the
    /// short-circuit consults before deciding to skip verification entirely,
    /// so a manifest recording a different digest for a path is precisely the
    /// state in which a tampered root gets served as verified. The two
    /// sub-conditions are corrupted in separate phases, each against an
    /// otherwise genuinely valid fixture, so neither can hide behind the
    /// other firing.
    #[test]
    fn a_component_that_disagrees_on_content_defeats_the_short_circuit() {
        let source = tempfile::tempdir().unwrap();
        let inventory = variant_fixture(source.path(), 1);
        let packed = pack(source.path(), &inventory).unwrap();
        let data_root = tempfile::tempdir().unwrap();

        let root = materialize_new_root(
            data_root.path(),
            &packed.archive,
            &packed.archive_digest,
            &packed.inventory,
        )
        .unwrap();

        let pristine = read_manifest(data_root.path(), &packed.archive_digest);
        // Anti-`ordered[0]` guard, as in the missing-entry test: corrupt an
        // entry a first-entry-only implementation would never inspect.
        let target = "skills/router.md";
        assert_ne!(
            packed.inventory[0].path, target,
            "fixture bug: the corrupted component must not be the first inventory entry"
        );

        // Phase 1 — the DIGEST disagrees, byte count held identical. A
        // length- or count-derived stand-in for the comparison cannot tell
        // these two states apart; a real digest comparison must.
        let mut manifest = pristine.clone();
        let entry = manifest
            .components
            .iter_mut()
            .find(|c| c.path == target)
            .expect("fixture bug: target component missing");
        let real_sha = entry.sha256.clone();
        let real_bytes = entry.bytes;
        entry.sha256 = HELLO_WORLD_SHA256.to_string();
        assert_ne!(
            entry.sha256, real_sha,
            "fixture bug: the substituted digest must differ from the real one"
        );
        assert_eq!(
            entry.bytes, real_bytes,
            "fixture bug: only the DIGEST may change in this phase"
        );
        overwrite_manifest(data_root.path(), &packed.archive_digest, &manifest);

        assert_eq!(
            short_circuit_candidate(data_root.path(), &packed.archive_digest, &packed.inventory),
            None,
            "a manifest recording a DIFFERENT digest for a path must not be trusted"
        );

        // Positive control between the two phases: restoring the pristine
        // manifest short-circuits again, proving the refusal above is caused
        // by the substituted digest and by nothing else in this fixture.
        overwrite_manifest(data_root.path(), &packed.archive_digest, &pristine);
        assert_eq!(
            short_circuit_candidate(data_root.path(), &packed.archive_digest, &packed.inventory),
            Some(root.clone()),
            "fixture bug: the pristine manifest must still short-circuit"
        );

        // Phase 2 — the BYTE COUNT disagrees, digest held identical.
        let mut manifest = pristine.clone();
        let entry = manifest
            .components
            .iter_mut()
            .find(|c| c.path == target)
            .expect("fixture bug: target component missing");
        entry.bytes = real_bytes + 1;
        assert_eq!(
            entry.sha256, real_sha,
            "fixture bug: only the BYTE COUNT may change in this phase"
        );
        overwrite_manifest(data_root.path(), &packed.archive_digest, &manifest);

        assert_eq!(
            short_circuit_candidate(data_root.path(), &packed.archive_digest, &packed.inventory),
            None,
            "a manifest recording a different byte count for a path must not be trusted"
        );

        // And the caller-visible entry point genuinely re-verifies rather
        // than trusting the corrupted record.
        let attempts_before = full_materialize_attempts();
        let result = ensure_verified_builtin_root(
            data_root.path(),
            &packed.archive,
            &packed.archive_digest,
            &packed.inventory,
            None,
        )
        .expect("a valid archive must still materialize despite a corrupted cached component");
        assert_eq!(result, root);
        assert_eq!(
            full_materialize_attempts(),
            attempts_before + 1,
            "defeating the short-circuit must fall through to exactly one full \
             decode-and-verify pass"
        );
        let repaired = read_manifest(data_root.path(), &packed.archive_digest);
        let fixed = repaired
            .components
            .iter()
            .find(|c| c.path == target)
            .expect("the corrupted entry must survive the repair");
        assert_eq!(fixed.sha256, real_sha, "the digest must be repaired");
        assert_eq!(fixed.bytes, real_bytes, "the byte count must be repaired");
    }

    /// One of §6.2's five independent defeat conditions: the digest-named
    /// path is a SYMLINK rather than a genuine directory. Checked directly
    /// against `short_circuit_candidate` — the exact function production
    /// calls — rather than only at the `ensure_verified_builtin_root`
    /// integration level, because a symlink pointing AT a directory would
    /// also satisfy `Path::is_dir` (which follows links), so an
    /// integration-level "did it decode the archive" observable could not
    /// tell "the short-circuit refused the symlink" apart from "the raw
    /// materializer's own `is_dir` keep-check happened to follow it". The
    /// property that must hold at THIS seam, unambiguously, is that a
    /// symlink is never trusted regardless of what it resolves to — proven
    /// by pointing it at a directory holding content that is otherwise
    /// perfectly valid (the very same promoted files, just moved) and
    /// confirming the short-circuit still refuses it.
    #[test]
    fn a_symlink_root_defeats_the_short_circuit() {
        use std::os::unix::fs::symlink;

        let source = tempfile::tempdir().unwrap();
        let inventory = variant_fixture(source.path(), 1);
        let packed = pack(source.path(), &inventory).unwrap();
        let data_root = tempfile::tempdir().unwrap();

        let root = materialize_new_root(
            data_root.path(),
            &packed.archive,
            &packed.archive_digest,
            &packed.inventory,
        )
        .unwrap();

        // Positive control on the untouched, genuine directory.
        assert_eq!(
            short_circuit_candidate(data_root.path(), &packed.archive_digest, &packed.inventory),
            Some(root.clone()),
            "fixture bug: a genuinely valid, non-symlink root must short-circuit"
        );

        // Move the real, verified content aside, then put a symlink to it
        // at the promoted path — so the only thing that changes is the file
        // TYPE at `root`, never the bytes it resolves to.
        let decoy = data_root.path().join("decoy-target");
        fs::rename(&root, &decoy).unwrap();
        symlink(&decoy, &root).unwrap();
        assert!(
            root.is_dir(),
            "fixture bug: the symlink must resolve to a real directory"
        );
        assert!(
            fs::symlink_metadata(&root)
                .unwrap()
                .file_type()
                .is_symlink(),
            "fixture bug: `root` must literally be a symlink for this test to mean anything"
        );

        assert_eq!(
            short_circuit_candidate(data_root.path(), &packed.archive_digest, &packed.inventory),
            None,
            "a symlink at the promoted-root path must never be trusted, even when it \
             resolves to genuinely valid content"
        );
    }

    // -----------------------------------------------------------------
    // P1.10 — wiring the packer + this materializer for the compiled-in
    // plugin bundle.
    // -----------------------------------------------------------------

    /// [`materialize_compiled_in_plugin_bundle`] must produce a root holding
    /// the REAL, build-time packed `plugins/lingxi-local-app/` content — named
    /// files, not merely a non-zero count — and the returned inventory must
    /// equal the generated descriptor exactly.
    #[test]
    fn compiled_in_plugin_bundle_materializes_the_real_plugin_files() {
        let bundle_root = tempfile::tempdir().unwrap();
        let (root, inventory) = materialize_compiled_in_plugin_bundle(bundle_root.path(), None)
            .expect("the compiled-in plugin bundle must materialize");

        assert_eq!(
            inventory.len(),
            COMPILED_PLUGIN_INVENTORY.len(),
            "the resolved inventory must name every compiled file exactly once"
        );
        for (path, _, _) in COMPILED_PLUGIN_INVENTORY {
            assert!(
                inventory.iter().any(|entry| entry.path == *path),
                "{path} is compiled in but missing from the resolved inventory"
            );
        }

        // Name a specific real file's real content — not just "some file
        // exists" — at the VERIFIED root the function returned.
        let device_skill = fs::read_to_string(root.join("skills/device/SKILL.md"))
            .expect("skills/device/SKILL.md must be materialized on disk at the verified root");
        let real_skill_marker = ["window", ".", "lingxi", ".v2"].concat();
        assert!(
            device_skill.contains(&real_skill_marker),
            "the materialized skill file must be the real repository content, got: {device_skill}"
        );
        let verifier_agent = fs::read_to_string(root.join("agents/verifier.md"))
            .expect("agents/verifier.md must be materialized on disk at the verified root");
        assert!(
            verifier_agent.contains("name: verifier"),
            "the materialized agent file must be the real repository content, got: {verifier_agent}"
        );
    }

    /// A second call against the SAME `bundle_root` must be the §6.2
    /// short-circuit — recognizing the unchanged compiled-in digest — not a
    /// second decode + verify. The pack already happened in `build.rs`, so
    /// runtime cannot accidentally restage or repack the source tree first.
    #[test]
    fn compiled_in_plugin_bundle_is_idempotent_across_calls() {
        let bundle_root = tempfile::tempdir().unwrap();
        let (first_root, first_inventory) =
            materialize_compiled_in_plugin_bundle(bundle_root.path(), None)
                .expect("first materialization must succeed");

        let attempts_before = full_materialize_attempts();
        let (second_root, second_inventory) =
            materialize_compiled_in_plugin_bundle(bundle_root.path(), None)
                .expect("second materialization must succeed");

        assert_eq!(
            first_root, second_root,
            "the same compiled-in bundle must resolve to the same verified root"
        );
        assert_eq!(first_inventory, second_inventory);
        assert_eq!(
            full_materialize_attempts(),
            attempts_before,
            "an unchanged compiled-in digest must not re-enter the decode/verify path"
        );
    }

    #[test]
    fn compiled_bundle_records_a_restart_visible_verified_fallback() {
        let bundle_root = tempfile::tempdir().unwrap();
        let (root, inventory) = materialize_compiled_in_plugin_bundle(bundle_root.path(), None)
            .expect("initial compiled bundle materialization");

        let active = active_verified_bundle(&bundle_root.path().join("materialized"))
            .expect("production restart must discover the last verified root without an argument");
        assert_eq!(active.0, root);
        assert_eq!(active.1, inventory);

        fs::remove_file(root.join(&inventory[0].path)).unwrap();
        assert_eq!(
            active_verified_bundle(&bundle_root.path().join("materialized")),
            None,
            "a stale active pointer must not turn a damaged previous root into a fallback"
        );
    }
}
