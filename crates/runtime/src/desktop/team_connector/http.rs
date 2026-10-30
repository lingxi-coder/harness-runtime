use super::{Error, Target};
use async_trait::async_trait;
use lingxi_llm_client::transport::http_backend as reqwest;
use serde::de::DeserializeOwned;
use serde_json::Value;
use std::sync::Arc;
use std::time::Duration;
use tokio_util::sync::CancellationToken;

/// Account authority supplied by the trusted host, never a renderer or worker.
/// Cancel the session token synchronously on logout/account replacement. Epochs
/// distinguish logout/relogin as the same UID, including during awaited refresh.
#[async_trait]
pub trait TeamConnectorAccount: Send + Sync {
    fn is_current(&self, owner_uid: &str, epoch: u64) -> bool;
    async fn credential(&self) -> Result<TeamConnectorCredential, Error>;
}

/// A Firebase ID token from the current owner account. Never persisted or sent
/// to the model worker. Deliberately has no Debug/Serialize implementation.
pub struct TeamConnectorCredential {
    pub owner_uid: String,
    pub epoch: u64,
    pub id_token: String,
}

pub(super) struct Client {
    pub endpoint: url::Url,
    account: Arc<dyn TeamConnectorAccount>,
    owner_uid: String,
    epoch: u64,
    pub session_cancel: CancellationToken,
    http: reqwest::Client,
}

impl Client {
    pub fn new(
        endpoint: &str,
        account: Arc<dyn TeamConnectorAccount>,
        owner_uid: String,
        epoch: u64,
        session_cancel: CancellationToken,
    ) -> Result<Self, Error> {
        let endpoint = url::Url::parse(endpoint).map_err(|_| Error::Configuration)?;
        let loopback = endpoint
            .host_str()
            .and_then(|s| s.trim_matches(['[', ']']).parse::<std::net::IpAddr>().ok())
            .is_some_and(|ip| ip.is_loopback());
        if !(endpoint.scheme() == "https" || (endpoint.scheme() == "http" && loopback))
            || endpoint.host().is_none()
            || !endpoint.username().is_empty()
            || endpoint.password().is_some()
            || endpoint.query().is_some()
            || endpoint.fragment().is_some()
            || endpoint.path() != "/v1/teams/messages"
        {
            return Err(Error::Configuration);
        }
        let http = reqwest::Client::builder()
            .redirect(reqwest::redirect::Policy::none())
            .retry(reqwest::retry::never())
            .no_proxy()
            .referer(false)
            .no_gzip()
            .no_brotli()
            .no_zstd()
            .no_deflate()
            .timeout(Duration::from_secs(5))
            .build()
            .map_err(|_| Error::Transport)?;
        Ok(Self {
            endpoint,
            account,
            owner_uid,
            epoch,
            session_cancel,
            http,
        })
    }

    pub fn check(&self, target: &Target) -> Result<(), Error> {
        if target.owner_uid != self.owner_uid
            || self.session_cancel.is_cancelled()
            || !self.account.is_current(&self.owner_uid, self.epoch)
        {
            return Err(Error::Account);
        }
        Ok(())
    }

    /// No retry layer: start ACK loss must remain ambiguous. All response bytes,
    /// token refresh and HTTP waits are finite. Server error bodies are withheld.
    pub async fn call<T: DeserializeOwned>(
        &self,
        target: &Target,
        body: Value,
        cancel: &CancellationToken,
    ) -> Result<T, Error> {
        self.check(target)?;
        let operation = async {
            let credential = self.account.credential().await?;
            self.check(target)?;
            if credential.owner_uid != self.owner_uid
                || credential.epoch != self.epoch
                || credential.id_token.is_empty()
                || credential.id_token.len() > 16384
                || credential
                    .id_token
                    .chars()
                    .any(|c| c.is_control() || c.is_whitespace())
            {
                return Err(Error::Account);
            }
            let bytes = serde_json::to_vec(&body).map_err(|_| Error::Protocol)?;
            if bytes.len() > 65536 {
                return Err(Error::Protocol);
            }
            let mut response = self
                .http
                .post(self.endpoint.clone())
                .bearer_auth(credential.id_token)
                .header("content-type", "application/json")
                .header("cache-control", "no-store")
                .body(bytes)
                .send()
                .await
                .map_err(|_| Error::Transport)?;
            self.check(target)?;
            if response.status().as_u16() != 200 {
                return Err(Error::Http(response.status().as_u16()));
            }
            if response.content_length().is_some_and(|len| len > 65536) {
                return Err(Error::Protocol);
            }
            let mut bytes = Vec::new();
            while let Some(chunk) = response.chunk().await.map_err(|_| Error::Transport)? {
                if bytes.len().saturating_add(chunk.len()) > 65536 {
                    return Err(Error::Protocol);
                }
                bytes.extend_from_slice(&chunk);
            }
            self.check(target)?;
            serde_json::from_slice(&bytes).map_err(|_| Error::Protocol)
        };
        tokio::select! {
            biased;
            _ = self.session_cancel.cancelled() => Err(Error::Account),
            _ = cancel.cancelled() => Err(Error::Cancelled),
            result = tokio::time::timeout(Duration::from_secs(5), operation) => {
                result.map_err(|_| Error::Transport)?
            }
        }
    }
}
