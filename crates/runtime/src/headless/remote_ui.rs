//! Native client-answer roster and requests on the shared control FIFO.
use super::control_plane::StdioControlPlane;
use lingxi_core::host::{ModRemoteUiAnswer, ModRemoteUiClient, ModRemoteUiHost};
use lingxi_core::types::utf16_json::Utf16JsonProjection;
use std::sync::{Arc, Mutex};
use std::time::Duration;

pub struct HeadlessRemoteUiHost {
    plane: Arc<StdioControlPlane>,
    clients: Mutex<Vec<ModRemoteUiClient>>,
}
impl HeadlessRemoteUiHost {
    pub fn new(plane: Arc<StdioControlPlane>) -> Arc<Self> {
        Arc::new(Self {
            plane,
            clients: Mutex::new(Vec::new()),
        })
    }
}

#[async_trait::async_trait]
impl ModRemoteUiHost for HeadlessRemoteUiHost {
    fn attach(&self, client: &str, surface: &str, answers: Option<Vec<ModRemoteUiAnswer>>) {
        let mut clients = self
            .clients
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        if let Some(existing) = clients.iter_mut().find(|entry| entry.client_id == client) {
            if let Some(answers) = answers {
                existing.answers = answers;
            }
        } else {
            clients.push(ModRemoteUiClient {
                client_id: client.to_owned(),
                surface: surface.to_owned(),
                answers: answers.unwrap_or_default(),
            });
        }
    }
    fn detach(&self, client: &str) {
        self.clients
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .retain(|entry| entry.client_id != client);
    }
    fn clients(&self, answer: ModRemoteUiAnswer) -> Vec<ModRemoteUiClient> {
        self.clients
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .iter()
            .filter(|entry| entry.answers.contains(&answer))
            .cloned()
            .collect()
    }
    async fn request(&self, request: Utf16JsonProjection) -> Option<Utf16JsonProjection> {
        let owner = self.plane.auxiliary_tasks()?;
        let plane = self.plane.clone();
        let (settled, observed) = tokio::sync::oneshot::channel();
        // The observation future can be dropped by Promise.any's winner. The
        // actual callback stays in the print owner until reply or timeout.
        owner.push(tokio::spawn(async move {
            let (id, receiver) = plane.send_request(request, None).await;
            let answer = match tokio::time::timeout(Duration::from_millis(5_000), receiver).await {
                Ok(Ok(Ok(answer))) => Some(answer),
                _ => {
                    let request_id = Utf16JsonProjection::from(id.as_str());
                    plane.cancel_request(&request_id).await;
                    None
                }
            };
            let _ = settled.send(answer);
        }));
        observed.await.ok().flatten()
    }
}

/// Invalid answers and error envelopes do not settle native UI callbacks.
pub(super) fn valid_ui_answer(subtype: &str, response: &Utf16JsonProjection) -> bool {
    if response.string_at("/subtype") == Some("error") {
        return false;
    }
    let Some(payload) = response
        .get_property("response")
        .filter(|value| value.object_entries().is_some())
    else {
        return false;
    };
    match subtype {
        "ui_copy" => payload
            .get_property("copied")
            .is_some_and(|value| value.as_bool().is_some()),
        "ui_prompt_fill" => payload
            .get_property("filled")
            .is_some_and(|value| value.as_bool().is_some()),
        "ui_prompt_suggest" => payload
            .get_property("shown")
            .is_some_and(|value| value.as_bool().is_some()),
        "ui_prompt_read" => {
            payload
                .get_property("text")
                .is_some_and(|value| value.as_str().is_some())
                && payload
                    .get_property("cursor")
                    .and_then(|value| value.as_number())
                    .is_some_and(|cursor| cursor >= 0.0 && cursor.fract() == 0.0)
        }
        "ui_read_selection" => {
            payload
                .get_property("text")
                .is_none_or(|value| value.as_str().is_some())
                && payload
                    .get_property("instance_id")
                    .is_none_or(|value| value.as_str().is_some())
        }
        _ => true,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::headless::stream_json::OutboundMsg;
    use serde_json::{json, Value};

    fn source(value: &Value) -> Utf16JsonProjection {
        Utf16JsonProjection::from_serde_value(value).expect("test value is valid JSON")
    }

    #[test]
    fn ui_answer_integer_validation_matches_javascript_numbers() {
        assert!(valid_ui_answer(
            "ui_prompt_read",
            &source(&json!({"subtype":"success","response":{"text":"text","cursor":1.0}}))
        ));
        for cursor in [json!(-1), json!(1.5), json!("1")] {
            assert!(!valid_ui_answer(
                "ui_prompt_read",
                &source(&json!({"subtype":"success","response":{"text":"text","cursor":cursor}}))
            ));
        }
        assert!(valid_ui_answer(
            "ui_copy",
            &source(&json!({"subtype":"success","response":{"copied":false}}))
        ));
    }

    #[tokio::test]
    async fn ui_callback_error_does_not_settle_and_valid_reply_preserves_utf16() {
        let (tx, mut rx) = tokio::sync::mpsc::unbounded_channel();
        let plane = StdioControlPlane::new(Arc::new(tx));
        let owner = Arc::new(crate::headless::run::PrintAuxTaskGroup::default());
        plane.set_auxiliary_tasks(Arc::downgrade(&owner));
        let host = HeadlessRemoteUiHost::new(plane.clone());
        let sender = host.clone();
        let pending = tokio::spawn(async move {
            sender
                .request(source(
                    &json!({"subtype":"ui_prompt_read","surface":"mobile","client_id":"phone"}),
                ))
                .await
        });
        let OutboundMsg::Line(line) = rx.recv().await.unwrap() else {
            panic!("request");
        };
        let request = Utf16JsonProjection::parse(&line).unwrap();
        let id = request.string_at("/request_id").unwrap();
        plane
            .resolve_response(&source(
                &json!({"type":"control_response","response":{"subtype":"error","request_id":id,"error":"not this window"}}),
            ))
            .await;
        tokio::task::yield_now().await;
        assert!(!pending.is_finished());
        let mut response=Utf16JsonProjection::parse(r#"{"type":"control_response","response":{"subtype":"success","response":{"text":"\ud800","cursor":1}}}"#).unwrap();
        let mut inner = response.subprojection("/response").unwrap();
        inner
            .set_field("request_id", Utf16JsonProjection::from(id))
            .unwrap();
        response.set_field("response", inner).unwrap();
        plane.resolve_response(&response).await;
        let answer = pending.await.unwrap().unwrap();
        assert_eq!(answer.string_units("/text"), Some(vec![0xd800]));
        owner.join().await;
    }

    fn response(id: &str, text: &str) -> Utf16JsonProjection {
        source(&json!({"type":"control_response","response":{
            "subtype":"success","request_id":id,"response":{"text":text}
        }}))
    }

    async fn callback_id(rx: &mut tokio::sync::mpsc::UnboundedReceiver<OutboundMsg>) -> String {
        let OutboundMsg::Line(line) = rx.recv().await.unwrap() else {
            panic!("callback request")
        };
        let frame: Value = serde_json::from_str(&line).unwrap();
        assert_eq!(frame["type"], "control_request");
        frame["request_id"].as_str().unwrap().to_owned()
    }

    fn selection_callbacks(
        host: Arc<HeadlessRemoteUiHost>,
    ) -> tokio::task::JoinHandle<Utf16JsonProjection> {
        tokio::spawn(async move {
            use futures::StreamExt;
            let mut callbacks = futures::stream::FuturesUnordered::new();
            for client in ["one", "two"] {
                let host = host.clone();
                callbacks.push(async move {
                    host.request(source(&json!({"subtype":"ui_read_selection","client_id":client,"surface":"mobile"}))).await
                });
            }
            while let Some(Some(answer)) = callbacks.next().await {
                if answer
                    .string_at("/text")
                    .is_some_and(|text| !text.is_empty())
                {
                    return answer;
                }
            }
            panic!("expected selection winner")
        })
    }

    #[tokio::test]
    async fn selection_winner_does_not_cancel_an_owned_losing_callback() {
        let (tx, mut rx) = tokio::sync::mpsc::unbounded_channel();
        let plane = StdioControlPlane::new(Arc::new(tx));
        let owner = Arc::new(crate::headless::run::PrintAuxTaskGroup::default());
        plane.set_auxiliary_tasks(Arc::downgrade(&owner));
        let selection = selection_callbacks(HeadlessRemoteUiHost::new(plane.clone()));
        let winner = callback_id(&mut rx).await;
        let loser = callback_id(&mut rx).await;
        plane.resolve_response(&response(&winner, "chosen")).await;
        assert_eq!(selection.await.unwrap().string_at("/text"), Some("chosen"));
        let joining = {
            let owner = owner.clone();
            tokio::spawn(async move { owner.join().await })
        };
        tokio::task::yield_now().await;
        assert!(
            !joining.is_finished(),
            "loser remains owned after observation is dropped"
        );
        assert!(matches!(
            rx.try_recv(),
            Err(tokio::sync::mpsc::error::TryRecvError::Empty)
        ));
        plane.resolve_response(&response(&loser, "late")).await;
        joining.await.unwrap();
        assert!(matches!(
            rx.try_recv(),
            Err(tokio::sync::mpsc::error::TryRecvError::Empty)
        ));
    }

    #[tokio::test(start_paused = true)]
    async fn losing_selection_callback_cancels_only_at_its_native_timeout() {
        let (tx, mut rx) = tokio::sync::mpsc::unbounded_channel();
        let plane = StdioControlPlane::new(Arc::new(tx));
        let owner = Arc::new(crate::headless::run::PrintAuxTaskGroup::default());
        plane.set_auxiliary_tasks(Arc::downgrade(&owner));
        let selection = selection_callbacks(HeadlessRemoteUiHost::new(plane.clone()));
        let winner = callback_id(&mut rx).await;
        let loser = callback_id(&mut rx).await;
        plane.resolve_response(&response(&winner, "chosen")).await;
        selection.await.unwrap();
        assert!(matches!(
            rx.try_recv(),
            Err(tokio::sync::mpsc::error::TryRecvError::Empty)
        ));
        tokio::time::advance(Duration::from_millis(4_999)).await;
        assert!(matches!(
            rx.try_recv(),
            Err(tokio::sync::mpsc::error::TryRecvError::Empty)
        ));
        tokio::time::advance(Duration::from_millis(1)).await;
        owner.join().await;
        let OutboundMsg::Line(line) = rx.recv().await.unwrap() else {
            panic!("timeout cancellation")
        };
        let frame: Value = serde_json::from_str(&line).unwrap();
        assert_eq!(
            frame,
            json!({"type":"control_cancel_request","request_id":loser})
        );
    }

    #[test]
    fn repeated_client_attach_keeps_absent_answers_and_replaces_present_answers() {
        let (tx, _rx) = tokio::sync::mpsc::unbounded_channel();
        let host = HeadlessRemoteUiHost::new(StdioControlPlane::new(Arc::new(tx)));
        host.attach("phone", "mobile", Some(vec![ModRemoteUiAnswer::Copy]));
        host.attach("phone", "mobile", None);
        assert_eq!(host.clients(ModRemoteUiAnswer::Copy).len(), 1);
        host.attach("phone", "mobile", Some(vec![]));
        assert!(host.clients(ModRemoteUiAnswer::Copy).is_empty());
        host.detach("phone");
        assert!(host.clients.lock().unwrap().is_empty());
    }
}
