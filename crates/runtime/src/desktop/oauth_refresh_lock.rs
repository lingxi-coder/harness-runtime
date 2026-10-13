//! The lock file behind which processes sharing an OAuth store refresh it.

use std::path::{Path, PathBuf};

use llm_runtime::auth::token_manager::{SharedRefreshGuard, SharedRefreshLock};

/// `<lingxi_home>/.oauth-refresh.lock`. Every process that stores its OAuth
/// sessions under one home refreshes them behind it, as claude-code locks its
/// config directory around a refresh.
pub(super) struct OAuthRefreshLockFile {
    home: PathBuf,
}

impl OAuthRefreshLockFile {
    pub(super) fn new(home: &Path) -> Self {
        Self {
            home: home.to_path_buf(),
        }
    }
}

#[async_trait::async_trait]
impl SharedRefreshLock for OAuthRefreshLockFile {
    async fn try_acquire(&self) -> Result<Option<SharedRefreshGuard>, String> {
        match lingxi_core::host::rooted_fs::try_lock_exclusive(
            &self.home,
            Path::new(".oauth-refresh.lock"),
            0o700,
            0o600,
        ) {
            Ok(lock) => Ok(Some(Box::new(lock))),
            // Another process holds it.
            Err(lingxi_core::host::FsError::AlreadyExists(_)) => Ok(None),
            Err(error) => Err(error.to_string()),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn one_holder_at_a_time_until_it_lets_go() {
        let home = tempfile::tempdir().unwrap();
        let first = OAuthRefreshLockFile::new(home.path());
        let second = OAuthRefreshLockFile::new(home.path());

        let held = first.try_acquire().await.unwrap().expect("free lock");
        assert!(second.try_acquire().await.unwrap().is_none());
        drop(held);
        assert!(second.try_acquire().await.unwrap().is_some());
    }
}
