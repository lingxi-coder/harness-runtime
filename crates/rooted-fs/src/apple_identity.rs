//! Read the volume UUID through an already validated directory descriptor.

use super::{FsError, RootIdentity};
use sha2::{Digest, Sha256};
use std::os::fd::{AsRawFd, BorrowedFd};

fn from_volume_uuid(identity: RootIdentity, uuid: [u8; 16]) -> RootIdentity {
    let mut digest = Sha256::new();
    digest.update(b"lingxi-source-volume-uuid-v1\0");
    digest.update(uuid);
    let digest = digest.finalize();
    RootIdentity {
        volume: u64::from_le_bytes(digest[..8].try_into().expect("eight digest bytes")),
        file: identity.file,
    }
}

pub(super) fn persistent_identity(
    directory: BorrowedFd<'_>,
    identity: RootIdentity,
) -> Result<RootIdentity, FsError> {
    let mut attributes = libc::attrlist {
        bitmapcount: libc::ATTR_BIT_MAP_COUNT,
        reserved: 0,
        commonattr: 0,
        volattr: libc::ATTR_VOL_INFO | libc::ATTR_VOL_UUID,
        dirattr: 0,
        fileattr: 0,
        forkattr: 0,
    };
    let mut result = [0u8; 20];
    // SAFETY: the descriptor is borrowed from the checked directory open;
    // attrlist is initialized, and the writable buffer has its stated size.
    let status = unsafe {
        libc::fgetattrlist(
            directory.as_raw_fd(),
            std::ptr::from_mut(&mut attributes).cast(),
            result.as_mut_ptr().cast(),
            result.len(),
            0,
        )
    };
    if status != 0 {
        let error = std::io::Error::last_os_error();
        if matches!(error.raw_os_error(), Some(libc::ENOTSUP | libc::EINVAL)) {
            return Ok(identity);
        }
        return Err(FsError::Io(format!(
            "cannot identify source volume: {error}"
        )));
    }
    let length = u32::from_ne_bytes(result[..4].try_into().expect("four length bytes"));
    if length != result.len() as u32 {
        return Err(FsError::Io("invalid source volume UUID response".into()));
    }
    let uuid: [u8; 16] = result[4..].try_into().expect("sixteen UUID bytes");
    if uuid == [0; 16] {
        return Ok(identity);
    }
    Ok(from_volume_uuid(identity, uuid))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn persistent_volume_identity_survives_device_renumbering_but_not_source_replacement() {
        let uuid = [7; 16];
        let first = from_volume_uuid(
            RootIdentity {
                volume: 11,
                file: 42,
            },
            uuid,
        );
        assert_eq!(
            first,
            from_volume_uuid(
                RootIdentity {
                    volume: 13,
                    file: 42
                },
                uuid
            )
        );
        assert_ne!(
            first,
            from_volume_uuid(
                RootIdentity {
                    volume: 11,
                    file: 43
                },
                uuid
            )
        );
        assert_ne!(
            first,
            from_volume_uuid(
                RootIdentity {
                    volume: 11,
                    file: 42
                },
                [8; 16]
            )
        );
    }

    #[test]
    fn real_directory_uuid_identity_is_stable_and_rejects_a_replaced_root() {
        let temp = tempfile::tempdir().unwrap();
        let root = std::fs::canonicalize(temp.path()).unwrap().join("source");
        std::fs::create_dir(&root).unwrap();
        let opened = crate::root_identity(&root).unwrap();
        let persistent = crate::persistent_root_identity_pinned(&root, &opened).unwrap();
        assert_eq!(persistent.file, opened.file);
        assert_eq!(
            persistent,
            crate::persistent_root_identity_pinned(&root, &opened).unwrap()
        );
        assert_ne!(
            persistent.volume, opened.volume,
            "the system temp volume must provide its persistent UUID"
        );
        std::fs::rename(&root, root.with_file_name("previous-source")).unwrap();
        std::fs::create_dir(&root).unwrap();
        assert!(crate::persistent_root_identity_pinned(&root, &opened).is_err());
    }
}
