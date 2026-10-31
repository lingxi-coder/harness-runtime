//! The two authorities never share an answer path: owner approval is a trusted
//! host capability; questions go through the original-human backend transaction.
use super::{http::Client, Error, Reply, Target, TeamBotState};
use crate::desktop::team_process::{
    TeamHumanQuestion, TeamOwnerPermission, TeamProcessBroker, TeamProcessError,
};
use async_trait::async_trait;
use serde::Deserialize;
use serde_json::json;
use sha2::{Digest, Sha256};
use std::collections::BTreeMap;
use std::sync::Arc;
use std::time::Duration;
use tokio_util::sync::CancellationToken;

#[async_trait]
pub trait TeamOwnerApproval: Send + Sync {
    async fn approve(
        &self,
        request: TeamOwnerPermission,
        cancel: CancellationToken,
    ) -> Result<bool, Error>;
}

pub(super) struct Broker {
    client: Client,
    target: Target,
    attempt: String,
    lease: u64,
    owner: Arc<dyn TeamOwnerApproval>,
}
#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct Ack {
    ok: bool,
}
impl Broker {
    pub fn new(
        client: Client,
        target: Target,
        attempt: String,
        lease: u64,
        owner: Arc<dyn TeamOwnerApproval>,
    ) -> Self {
        Self {
            client,
            target,
            attempt,
            lease,
            owner,
        }
    }
    fn identity(&self, kind: &str, id: &str) -> String {
        format!(
            "{:x}",
            Sha256::digest(
                serde_json::to_vec(&(kind, &self.target.trigger_id, &self.attempt, id))
                    .expect("string tuple")
            )
        )
    }
    async fn poll(&self, cancel: &CancellationToken) -> Result<Reply, Error> {
        let reply: Reply = self
            .client
            .call(
                &self.target,
                self.target.command("botPoll", &self.attempt),
                cancel,
            )
            .await?;
        if reply.state != TeamBotState::Running
            || reply.lease_until != Some(self.lease)
            || reply.execute.is_some()
            || reply.execution.is_some()
        {
            return Err(Error::Protocol);
        }
        Ok(reply)
    }
    async fn wait(&self, id: &str, waiting: bool, cancel: &CancellationToken) -> Result<(), Error> {
        let mut command = self.target.command("botWait", &self.attempt);
        command["waitId"] = json!(id);
        command["waiting"] = json!(waiting);
        let ack: Ack = self.client.call(&self.target, command, cancel).await?;
        if !ack.ok {
            return Err(Error::Protocol);
        }
        Ok(())
    }
}
#[async_trait]
impl TeamProcessBroker for Broker {
    async fn approve(
        &self,
        request: TeamOwnerPermission,
        cancel: CancellationToken,
    ) -> Result<bool, TeamProcessError> {
        let operation = async {
            if request.tool_name != "Bash" {
                return Ok(false);
            }
            self.poll(&cancel).await?;
            let id = self.identity("approval", &request.id);
            self.wait(&id, true, &cancel).await?;
            let allowed = self.owner.approve(request, cancel.clone()).await?;
            self.poll(&cancel).await?;
            self.wait(&id, false, &cancel).await?;
            Ok::<_, Error>(allowed)
        };
        tokio::select! { biased; _=cancel.cancelled()=>Err(TeamProcessError::Cancelled),
        result=operation=>result.map_err(|_|TeamProcessError::Cancelled) }
    }
    async fn ask(
        &self,
        request: TeamHumanQuestion,
        cancel: CancellationToken,
    ) -> Result<Option<BTreeMap<String, String>>, TeamProcessError> {
        let operation = async {
            self.poll(&cancel).await?;
            let question_id = self.identity("question", &request.id);
            let mut command = self.target.command("botQuestion", &self.attempt);
            command["questionId"] = json!(question_id);
            command["text"] = json!(request.text);
            command["questions"] = json!(request.questions);
            let ack: Ack = self.client.call(&self.target, command, &cancel).await?;
            if !ack.ok {
                return Err(Error::Protocol);
            }
            loop {
                let reply = self.poll(&cancel).await?;
                if reply.question_id.as_deref() != Some(&question_id) {
                    return Err(Error::Protocol);
                }
                if let Some(answers) = reply.answers {
                    if answers.len() != request.questions.len()
                        || request.questions.iter().any(|q| !answers.contains_key(q))
                        || answers
                            .values()
                            .any(|a| a.is_empty() || a.encode_utf16().count() > 2000)
                    {
                        return Err(Error::Protocol);
                    }
                    return Ok(Some(answers.into_iter().collect()));
                }
                tokio::time::sleep(Duration::from_millis(750)).await;
            }
        };
        tokio::select! { biased; _=cancel.cancelled()=>Err(TeamProcessError::Cancelled),
        result=operation=>result.map_err(|_|TeamProcessError::Cancelled) }
    }
}
