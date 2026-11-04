pub mod claude;
pub mod codex;
use crate::{app::Context, model::*};
use serde_json::{Value, json};
use std::{future::Future, path::Path, pin::Pin, sync::Arc};
pub type ProbeFuture<'a, T> = Pin<Box<dyn Future<Output = Result<T>> + Send + 'a>>;
pub trait Provider: Send + Sync {
    fn id(&self) -> &'static str;
    fn capabilities(&self) -> Vec<&'static str> {
        vec![
            "authentication",
            "account_information",
            "subscription_limits",
            "runtime_version",
        ]
    }
    fn probe<'a>(&'a self, ctx: &'a Context, p: &'a Profile, binary: &'a Path) -> ProbeFuture<'a, Snapshot>;
    fn authenticate<'a>(
        &'a self,
        ctx: Arc<Context>,
        p: Profile,
        op: String,
        codes: tokio::sync::mpsc::Receiver<String>,
    ) -> ProbeFuture<'a, Account>;
    fn statistics<'a>(&'a self, _ctx: &'a Context, _p: &'a Profile) -> ProbeFuture<'a, Value> {
        Box::pin(async { Err(Failure::unsupported()) })
    }
    fn description(&self) -> Value {
        json!({"id":self.id(),"capabilities":self.capabilities(),"scopes":["account"]})
    }
}
pub fn registered() -> Vec<Arc<dyn Provider>> {
    vec![Arc::new(codex::Codex), Arc::new(claude::Claude)]
}
