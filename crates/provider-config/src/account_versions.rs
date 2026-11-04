//! Non-secret credential revision fencing shared by credential and health authorities.
use lingxi_core::host::{provider_accounts::AccountMetadata, rooted_fs};
use serde::{Deserialize, Serialize};
use std::path::{Path, PathBuf};

/// Non-secret revision fence for one account identity and its credentials.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct AccountCredentialVersion {
    /// Account identifier associated with this version record.
    pub account_id: String,
    /// Generation of the account identity to which the revision belongs.
    pub identity_generation: u64,
    /// Published credential revision within the identity generation.
    pub credential_revision: u64,
}
fn relative(account: &AccountMetadata) -> Result<PathBuf, String> {
    let scope = &account.credential_ref.store_scope;
    if uuid::Uuid::parse_str(&account.account_id).is_err()
        || account.credential_ref.secret_ref != format!("acct.{}", account.account_id)
        || scope.is_empty()
        || scope.len() > 128
        || !scope
            .bytes()
            .all(|b| b.is_ascii_alphanumeric() || matches!(b, b'.' | b'-' | b'_'))
    {
        return Err("invalid account credential version scope".into());
    }
    Ok(PathBuf::from("provider-accounts/versions")
        .join(scope)
        .join(format!("{}.json", account.account_id)))
}
fn read(
    root: &Path,
    account: &AccountMetadata,
) -> Result<Option<AccountCredentialVersion>, String> {
    match rooted_fs::read_to_string_limited(root, &relative(account)?, 4096) {
        Ok(text) => {
            let version: AccountCredentialVersion =
                serde_json::from_str(&text).map_err(|_| "account credential version invalid")?;
            if version.account_id != account.account_id
                || version.identity_generation == 0
                || version.credential_revision == 0
            {
                return Err("account credential version invalid".into());
            }
            Ok(Some(version))
        }
        Err(rooted_fs::FsError::NotFound(_)) => Ok(None),
        Err(_) => Err("account credential version unavailable".into()),
    }
}
/// Read the published revision for exactly this principal generation; no secrets or writes.
pub fn read_account_credential_version(
    root: &Path,
    account: &AccountMetadata,
) -> Result<Option<u64>, String> {
    Ok(read(root, account)?
        .filter(|version| version.identity_generation == account.identity_generation)
        .map(|version| version.credential_revision))
}
/// The caller must hold the canonical scope directory lock through secure-store and ledger commit.
pub fn write_account_credential_version_locked(
    root: &Path,
    account: &AccountMetadata,
    revision: u64,
) -> Result<(), String> {
    if revision == 0 || account.identity_generation == 0 {
        return Err("account credential revision required".into());
    }
    if let Some(current) = read(root, account)? {
        if (current.identity_generation, current.credential_revision)
            > (account.identity_generation, revision)
        {
            return Err("stale account credential version".into());
        }
        if current.identity_generation == account.identity_generation
            && current.credential_revision == revision
        {
            return Ok(());
        }
    }
    let version = AccountCredentialVersion {
        account_id: account.account_id.clone(),
        identity_generation: account.identity_generation,
        credential_revision: revision,
    };
    let bytes =
        serde_json::to_vec(&version).map_err(|_| "account credential version encoding failed")?;
    rooted_fs::atomic_write(root, &relative(account)?, &bytes, Default::default())
        .map_err(|_| "account credential version commit failed".into())
}

#[cfg(test)]
mod tests {
    use super::*;
    use lingxi_core::host::provider_accounts::{AccountAuthKind, CredentialRef};
    fn account() -> AccountMetadata {
        let id = uuid::Uuid::new_v4().to_string();
        AccountMetadata {
            account_id: id.clone(),
            provider_id: "openai".into(),
            display_name: "Fixture".into(),
            auth_kind: AccountAuthKind::OAuth,
            execution_binding_id: "openai-chatgpt".into(),
            credential_ref: CredentialRef {
                store_scope: "version_fixture".into(),
                secret_ref: format!("acct.{id}"),
            },
            enabled: true,
            user_priority: 0,
            config_revision: 1,
            identity_generation: 1,
        }
    }
    #[test]
    fn revision_ledger_is_read_only_monotonic_and_principal_scoped() {
        let directory = tempfile::tempdir().unwrap();
        let mut account = account();
        assert_eq!(
            read_account_credential_version(directory.path(), &account).unwrap(),
            None
        );
        assert!(!directory.path().join("provider-accounts").exists());
        write_account_credential_version_locked(directory.path(), &account, 2).unwrap();
        assert_eq!(
            read_account_credential_version(directory.path(), &account).unwrap(),
            Some(2)
        );
        write_account_credential_version_locked(directory.path(), &account, 2).unwrap();
        assert!(write_account_credential_version_locked(directory.path(), &account, 1).is_err());
        account.identity_generation = 2;
        assert_eq!(
            read_account_credential_version(directory.path(), &account).unwrap(),
            None
        );
        write_account_credential_version_locked(directory.path(), &account, 1).unwrap();
        account.identity_generation = 1;
        assert!(write_account_credential_version_locked(directory.path(), &account, 99).is_err());
    }
}
