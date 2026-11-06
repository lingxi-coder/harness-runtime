//! Immutable private transport references. These are transport spools, not a Chat cache.
use super::TeamProcessError;
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use std::io::{Read, Write};
use std::path::{Path, PathBuf};

pub const MAX_DATA_BYTES: u64 = 64 * 1024 * 1024;
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields, rename_all = "camelCase")]
pub struct TeamDataRef {
    pub path: PathBuf,
    pub sha256: String,
    pub bytes: u64,
}
impl TeamDataRef {
    pub fn read<T: serde::de::DeserializeOwned>(&self, root: &Path) -> Result<T, TeamProcessError> {
        if self.bytes > MAX_DATA_BYTES {
            return Err(TeamProcessError::ResourceLimit);
        }
        if self.bytes == 0
            || self.sha256.len() != 64
            || !self
                .sha256
                .bytes()
                .all(|b| b.is_ascii_hexdigit() && !b.is_ascii_uppercase())
        {
            return Err(TeamProcessError::Protocol);
        }
        let root = root.canonicalize()?;
        if self.path.canonicalize()? != self.path || !self.path.starts_with(&root) {
            return Err(TeamProcessError::Protocol);
        }
        let relative = self
            .path
            .strip_prefix(&root)
            .map_err(|_| TeamProcessError::Protocol)?;
        let identity = lingxi_core::host::rooted_fs::root_identity(&root)
            .map_err(|_| TeamProcessError::Protocol)?;
        let mut file =
            lingxi_core::host::rooted_fs::open_read_file_pinned(&root, relative, Some(&identity))
                .map_err(|_| TeamProcessError::Protocol)?;
        let meta = file.metadata()?;
        if !meta.is_file() || meta.len() != self.bytes {
            return Err(TeamProcessError::Protocol);
        }
        #[cfg(unix)]
        {
            use std::os::unix::fs::MetadataExt;
            if meta.uid() != nix::unistd::geteuid().as_raw()
                || meta.mode() & 0o077 != 0
                || meta.nlink() != 1
            {
                return Err(TeamProcessError::Protocol);
            }
        }
        let mut bytes = Vec::new();
        std::io::Read::by_ref(&mut file)
            .take(MAX_DATA_BYTES + 1)
            .read_to_end(&mut bytes)?;
        if bytes.len() as u64 != self.bytes
            || format!("{:x}", Sha256::digest(&bytes)) != self.sha256
        {
            return Err(TeamProcessError::Protocol);
        }
        serde_json::from_slice(&bytes).map_err(|_| TeamProcessError::Protocol)
    }
    pub(crate) fn write<T: Serialize>(root: &Path, value: &T) -> Result<Self, TeamProcessError> {
        let bytes = serde_json::to_vec(value).map_err(|_| TeamProcessError::Protocol)?;
        if bytes.len() as u64 > MAX_DATA_BYTES {
            return Err(TeamProcessError::ResourceLimit);
        }
        if bytes.is_empty() {
            return Err(TeamProcessError::Protocol);
        }
        let sha256 = format!("{:x}", Sha256::digest(&bytes));
        let path = root.join(format!("{sha256}.json"));
        let mut options = std::fs::OpenOptions::new();
        options.write(true).create_new(true);
        #[cfg(unix)]
        {
            use std::os::unix::fs::OpenOptionsExt;
            options.mode(0o600).custom_flags(nix::libc::O_NOFOLLOW);
        }
        match options.open(&path) {
            Ok(mut file) => {
                file.write_all(&bytes)?;
                file.sync_all()?;
                std::fs::File::open(root)?.sync_all()?;
            }
            Err(e) if e.kind() == std::io::ErrorKind::AlreadyExists => {}
            Err(e) => return Err(e.into()),
        }
        let reference = Self {
            path,
            sha256,
            bytes: bytes.len() as u64,
        };
        let _: serde_json::Value = reference.read(root)?;
        Ok(reference)
    }
}
