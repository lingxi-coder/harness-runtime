//! Append-only owner-private records. A pre-start intent is never removed by
//! automatic recovery. Advisory locking releases on crash; execution authority
//! comes from the immutable start record and backend fencing, not the lock.

use super::{Error, Target};
use serde::{de::DeserializeOwned, Deserialize, Serialize};
use sha2::{Digest, Sha256};
use std::fs::{File, OpenOptions};
use std::io::{Read, Write};
use std::os::fd::AsRawFd;
use std::os::unix::fs::{DirBuilderExt, MetadataExt, OpenOptionsExt};
use std::path::{Path, PathBuf};

#[derive(Serialize, Deserialize)]
#[serde(deny_unknown_fields, rename_all = "camelCase")]
pub(super) struct Intent {
    pub version: u32,
    pub target: Target,
    pub attempt_id: String,
    pub invocation_id: uuid::Uuid,
}

pub(super) struct Journal {
    path: PathBuf,
    _lock: File,
    pub intent: Intent,
}

pub(super) fn private_dir(path: &Path) -> Result<(), Error> {
    let m = std::fs::symlink_metadata(path)?;
    if !path.is_absolute()
        || !m.is_dir()
        || m.mode() & 0o777 != 0o700
        || m.uid() != nix::unistd::geteuid().as_raw()
        || path.canonicalize()? != path
    {
        return Err(Error::Storage);
    }
    Ok(())
}

fn regular(file: &File, mode: u32) -> Result<(), Error> {
    let m = file.metadata()?;
    if !m.is_file()
        || m.mode() & 0o777 != mode
        || m.uid() != nix::unistd::geteuid().as_raw()
        || m.nlink() != 1
        || m.len() > 65536
    {
        return Err(Error::Storage);
    }
    Ok(())
}

impl Journal {
    pub fn open(root: &Path, endpoint: &url::Url, target: Target) -> Result<Self, Error> {
        private_dir(root)?;
        // Binding revision/generation are validated below, but deliberately NOT
        // part of the key. Changing them must not mint new authority for a
        // trigger after a crash. Backend origin separates emulator/production.
        let bytes = serde_json::to_vec(&(
            "team-connector-v1",
            endpoint.as_str(),
            &target.owner_uid,
            &target.team_id,
            &target.conversation_id,
            &target.bot_key,
            &target.trigger_id,
        ))
        .map_err(|_| Error::Protocol)?;
        let digest = Sha256::digest(bytes);
        let key = format!("{digest:x}");
        let lock = OpenOptions::new()
            .read(true)
            .write(true)
            .create(true)
            .truncate(false)
            .mode(0o600)
            .custom_flags(nix::libc::O_NOFOLLOW)
            .open(root.join(format!("lock-{key}")))?;
        regular(&lock, 0o600)?;
        match nix::fcntl::flock(
            lock.as_raw_fd(),
            nix::fcntl::FlockArg::LockExclusiveNonblock,
        ) {
            Ok(()) => (),
            Err(nix::errno::Errno::EWOULDBLOCK) => return Err(Error::Busy),
            Err(_) => return Err(Error::Storage),
        }
        private_dir(root)?;
        let path = root.join(format!("run-{key}"));
        let created = match std::fs::DirBuilder::new().mode(0o700).create(&path) {
            Ok(()) => {
                File::open(root)?.sync_all()?;
                true
            }
            Err(e) if e.kind() == std::io::ErrorKind::AlreadyExists => false,
            Err(e) => return Err(e.into()),
        };
        private_dir(&path)?;
        let mut uuid_bytes: [u8; 16] = digest[..16].try_into().map_err(|_| Error::Storage)?;
        uuid_bytes[6] = (uuid_bytes[6] & 0x0f) | 0x80;
        uuid_bytes[8] = (uuid_bytes[8] & 0x3f) | 0x80;
        let intent = Intent {
            version: 1,
            target,
            attempt_id: uuid::Uuid::new_v4().simple().to_string(),
            invocation_id: uuid::Uuid::from_bytes(uuid_bytes),
        };
        let mut journal = Self {
            path,
            _lock: lock,
            intent,
        };
        if created {
            journal.write("intent.json", &journal.intent)?;
        } else {
            let saved: Intent = journal.read("intent.json")?.ok_or(Error::Storage)?;
            if saved.version != 1
                || saved.target != journal.intent.target
                || saved.invocation_id != journal.intent.invocation_id
                || !super::identifier(&saved.attempt_id, 8, 64)
            {
                return Err(Error::Storage);
            }
            journal.intent = saved;
        }
        Ok(journal)
    }

    pub fn read<T: DeserializeOwned>(&self, name: &str) -> Result<Option<T>, Error> {
        private_dir(&self.path)?;
        let mut file = match OpenOptions::new()
            .read(true)
            .custom_flags(nix::libc::O_NOFOLLOW)
            .open(self.path.join(name))
        {
            Ok(file) => file,
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(None),
            Err(e) => return Err(e.into()),
        };
        regular(&file, 0o400)?;
        let mut bytes = Vec::new();
        (&mut file).take(65537).read_to_end(&mut bytes)?;
        if bytes.len() > 65536 {
            return Err(Error::Storage);
        }
        serde_json::from_slice(&bytes)
            .map(Some)
            .map_err(|_| Error::Storage)
    }

    pub fn write<T: Serialize>(&self, name: &str, value: &T) -> Result<(), Error> {
        private_dir(&self.path)?;
        let bytes = serde_json::to_vec(value).map_err(|_| Error::Storage)?;
        if bytes.len() > 65536 {
            return Err(Error::Storage);
        }
        let mut file = OpenOptions::new()
            .write(true)
            .create_new(true)
            .mode(0o400)
            .custom_flags(nix::libc::O_NOFOLLOW)
            .open(self.path.join(name))?;
        file.write_all(&bytes)?;
        file.sync_all()?;
        File::open(&self.path)?.sync_all()?;
        Ok(())
    }
}
