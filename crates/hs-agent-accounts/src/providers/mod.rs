pub mod claude;
pub mod codex;
use crate::{app::Context, model::*};
use serde_json::{Value, json};
use std::{future::Future, path::Path, pin::Pin, sync::Arc};
pub type ProbeFuture<'a, T> = Pin<Box<dyn Future<Output = Result<T>> + Send + 'a>>;
pub trait Provider: Send + Sync {
    fn id(&self) -> &'static str;
    fn bundled_version(&self) -> Option<&'static str> {
        None
    }
    fn capabilities(&self) -> Vec<&'static str> {
        Vec::new()
    }
    fn probe<'a>(&'a self, _ctx: &'a Context, _p: &'a Profile, _binary: Option<&'a Path>) -> ProbeFuture<'a, Snapshot> {
        Box::pin(async { Err(Failure::unsupported()) })
    }
    fn authenticate<'a>(
        &'a self,
        _ctx: Arc<Context>,
        _p: Profile,
        _op: String,
        _codes: tokio::sync::mpsc::Receiver<String>,
    ) -> ProbeFuture<'a, Account> {
        Box::pin(async { Err(Failure::unsupported()) })
    }
    fn statistics<'a>(&'a self, _ctx: &'a Context, _p: &'a Profile) -> ProbeFuture<'a, Value> {
        Box::pin(async { Err(Failure::unsupported()) })
    }
    fn description(&self) -> Value {
        let capabilities = self.capabilities();
        json!({"id":self.id(),"capabilities":capabilities,"scopes":["account"]})
    }
}
pub fn registered() -> Vec<Arc<dyn Provider>> {
    vec![Arc::new(codex::Codex), Arc::new(claude::Claude)]
}
