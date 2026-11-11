//! Upgrade a still-proven legacy source before using persistent volume IDs.
//! A receipt from a different device/inode is never adopted automatically.

use super::*;

pub(super) fn upgrade_current_sources(library: &AppLibrary) -> Result<(), AppError> {
    let _lock = library.lock()?;
    let root = library.root.join("pending-authoring");
    let pin = rooted_fs::root_identity(&root)
        .map_err(|error| AppError::Invalid(format!("invalid authoring receipt root: {error}")))?;
    let entries = fs::read_dir(&root)?.collect::<Result<Vec<_>, _>>()?;
    for entry in &entries {
        let name = entry.file_name();
        let Some(name) = name
            .to_str()
            .filter(|name| name.starts_with("source-") && name.ends_with(".json"))
        else {
            continue;
        };
        let Some(content) = read(&root, &pin, name)? else {
            continue;
        };
        let mut proof: NativeAuthoringSource = serde_json::from_str(&content)?;
        if !valid_uuid(&proof.source_id)
            || name != AppLibrary::native_authoring_source_name(&proof.source_directory)
        {
            return Err(AppError::Invalid(
                "authoring source receipt identity mismatch".into(),
            ));
        }
        let Ok(persistent) = native_source_identity(&proof.source_directory) else {
            continue;
        };
        if proof.source_identity == persistent {
            continue;
        }
        let legacy = rooted_fs::root_identity(Path::new(&proof.source_directory))
            .map_err(|error| AppError::Invalid(format!("invalid authoring source: {error}")))?;
        if proof.source_identity != legacy {
            // A remounted old receipt needs explicit native recovery. Inode
            // equality alone cannot authorize a different physical volume.
            continue;
        }
        let mut updates = Vec::new();
        for related in &entries {
            let related_name = related.file_name();
            let Some(related_name) = related_name.to_str().filter(|name| name.ends_with(".json"))
            else {
                continue;
            };
            if !related_name.starts_with("owner-") && !related_name.starts_with("creation-") {
                continue;
            }
            let Some(before) = read(&root, &pin, related_name)? else {
                continue;
            };
            let after = if related_name.starts_with("owner-") {
                let mut receipt: NativeAuthoringReceipt = serde_json::from_str(&before)?;
                if receipt.source_id != proof.source_id
                    || receipt.authoring.source_directory != proof.source_directory
                {
                    continue;
                }
                if receipt.source_identity == persistent {
                    continue;
                }
                if receipt.source_identity != legacy {
                    return Err(AppError::Invalid("authoring source proof mismatch".into()));
                }
                receipt.source_identity = persistent;
                serde_json::to_vec_pretty(&receipt)?
            } else {
                let mut receipt: NativeSourceCreation = serde_json::from_str(&before)?;
                if receipt.source_id != proof.source_id
                    || receipt.source_directory != proof.source_directory
                {
                    continue;
                }
                if receipt.source_identity == persistent {
                    continue;
                }
                if receipt.source_identity != legacy {
                    return Err(AppError::Invalid("authoring source proof mismatch".into()));
                }
                receipt.source_identity = persistent;
                serde_json::to_vec_pretty(&receipt)?
            };
            updates.push((related_name.to_owned(), before, after));
        }
        proof.source_identity = persistent;
        // The source proof commits last. An interrupted upgrade is safely
        // retried while that proof still matches the legacy directory, with
        // already-upgraded related receipts left intact.
        updates.push((name.to_owned(), content, serde_json::to_vec_pretty(&proof)?));
        for (name, before, after) in updates {
            if native_source_identity(&proof.source_directory)? != persistent
                || read(&root, &pin, &name)?.as_deref() != Some(before.as_str())
            {
                return Err(AppError::Invalid(
                    "authoring source changed during identity upgrade".into(),
                ));
            }
            rooted_fs::atomic_write_pinned(
                &root,
                Path::new(&name),
                &after,
                rooted_fs::AtomicWriteOptions {
                    overwrite: true,
                    create_parents: false,
                    ..Default::default()
                },
                &pin,
            )
            .map_err(|error| {
                AppError::Invalid(format!("cannot upgrade authoring source identity: {error}"))
            })?;
        }
    }
    Ok(())
}

fn read(
    root: &Path,
    pin: &rooted_fs::RootIdentity,
    name: &str,
) -> Result<Option<String>, AppError> {
    let captured = rooted_fs::read_file_single_link_checked(root, Path::new(name))
        .map_err(|error| AppError::Invalid(format!("invalid authoring receipt: {error}")))?;
    if !captured.matches_root(Some(pin)) {
        return Err(AppError::Invalid("authoring receipt root changed".into()));
    }
    Ok(captured.content)
}
