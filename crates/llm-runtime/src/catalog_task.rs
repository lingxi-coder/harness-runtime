//! A running turn keeps its model definitions across cloud publications.
use crate::ModelRuntime;
use std::{
    collections::BTreeMap,
    sync::{Arc, Mutex},
};
tokio::task_local! {
    static CLIENTS: Arc<Mutex<BTreeMap<usize, Arc<ModelRuntime>>>>;
}
/// Scope the complete tool/model loop. Nested model operations share the scope;
/// a subsequent turn starts with the newly published official directory.
pub async fn scope<F: std::future::Future>(future: F) -> F::Output {
    if CLIENTS.try_with(|_| ()).is_ok() {
        future.await
    } else {
        CLIENTS
            .scope(Arc::new(Mutex::new(BTreeMap::new())), future)
            .await
    }
}
/// Resolve one runtime owner once per turn. Credentials remain request-local.
pub fn client(template: &ModelRuntime) -> Arc<ModelRuntime> {
    CLIENTS
        .try_with(|clients| {
            clients
                .lock()
                .unwrap_or_else(|e| e.into_inner())
                .entry(template.catalog_owner_key())
                .or_insert_with(|| template.unscoped_current())
                .clone()
        })
        .unwrap_or_else(|_| template.unscoped_current())
}
