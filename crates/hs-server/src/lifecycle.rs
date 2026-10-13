//! `workspace.create` and `workspace.destroy` (phase 5): the operations of
//! `hs_core::lifecycle` in the plan and job model.
//!
//! `POST /v1/plans` with `{"operation": "workspace.create", "parameters":
//! {"vmid": 240, "name": "hst-a", "home_size": "20G", "storage": "..."}}`
//! (`home_size` and `storage` optional), or `{"operation":
//! "workspace.destroy", "parameters": {"vmid": 240}}`. The job's result is
//! the plan with what the client needs for its local steps and the event,
//! `workspace.created` or `workspace.destroyed`.

use std::sync::Arc;

use hs_core::CoreError;
use hs_core::lifecycle::{
    CreateRequest, LifecycleTiming, create_workspace, destroy_workspace, plan_create, plan_destroy,
};
use serde::Deserialize;
use serde_json::Value;

use crate::operations::{BoxFuture, JobHandle, OpContext, OpFailure, Operation, Planned};

/// The lifecycle operations with `timing`.
#[must_use]
pub fn operations(timing: LifecycleTiming) -> Vec<Arc<dyn Operation>> {
    vec![Arc::new(Create { timing }), Arc::new(Destroy { timing })]
}

fn config(ctx: &OpContext) -> Result<&hs_model::config::Config, OpFailure> {
    ctx.config
        .as_ref()
        .map(|stored| &stored.config)
        .ok_or(OpFailure::invalid(
            "The server has no configuration yet; upload one with PUT /v1/config.",
        ))
}

fn vmid_of(summary: &Value) -> Result<u32, OpFailure> {
    summary
        .get("vmid")
        .and_then(Value::as_u64)
        .and_then(|vmid| u32::try_from(vmid).ok())
        .ok_or(OpFailure::invalid("The plan has no VMID."))
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct CreateParameters {
    vmid: u32,
    name: String,
    home_size: Option<String>,
    storage: Option<String>,
    node: Option<String>,
}

struct Create {
    timing: LifecycleTiming,
}

impl Operation for Create {
    fn name(&self) -> &'static str {
        "workspace.create"
    }

    fn plan<'a>(&'a self, ctx: &'a OpContext, parameters: &'a Value) -> BoxFuture<'a, Result<Planned, OpFailure>> {
        Box::pin(async move {
            let parameters: CreateParameters = serde_json::from_value(parameters.clone()).map_err(|_| {
                OpFailure::invalid("workspace.create takes vmid and name, and optionally home_size, storage and node.")
            })?;
            let request = CreateRequest {
                vmid: parameters.vmid,
                name: parameters.name,
                home_size: parameters.home_size,
                storage: parameters.storage,
                node: parameters.node,
            };
            let summary = plan_create(ctx.pve.as_ref(), config(ctx)?, &ctx.pools.workspaces, &request).await?;
            Ok(Planned {
                targets: vec![request.vmid],
                summary,
            })
        })
    }

    fn execute<'a>(
        &'a self,
        ctx: &'a OpContext,
        planned: &'a Planned,
        job: &'a JobHandle,
    ) -> BoxFuture<'a, Result<Value, OpFailure>> {
        Box::pin(async move {
            vmid_of(&planned.summary)?;
            Ok(create_workspace(ctx.pve.as_ref(), config(ctx)?, &planned.summary, &self.timing, job).await?)
        })
    }
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct DestroyParameters {
    vmid: u32,
}

struct Destroy {
    timing: LifecycleTiming,
}

impl Operation for Destroy {
    fn name(&self) -> &'static str {
        "workspace.destroy"
    }

    fn plan<'a>(&'a self, ctx: &'a OpContext, parameters: &'a Value) -> BoxFuture<'a, Result<Planned, OpFailure>> {
        Box::pin(async move {
            let parameters: DestroyParameters = serde_json::from_value(parameters.clone())
                .map_err(|_| OpFailure::invalid("workspace.destroy takes vmid."))?;
            let summary = plan_destroy(ctx.pve.as_ref(), config(ctx)?, &ctx.pools.workspaces, parameters.vmid)
                .await
                .map_err(|error| match error {
                    CoreError::NotFound(_) => OpFailure::not_found("No such VM."),
                    other => OpFailure::Core(other),
                })?;
            Ok(Planned {
                targets: vec![parameters.vmid],
                summary,
            })
        })
    }

    fn execute<'a>(
        &'a self,
        ctx: &'a OpContext,
        planned: &'a Planned,
        job: &'a JobHandle,
    ) -> BoxFuture<'a, Result<Value, OpFailure>> {
        Box::pin(async move {
            vmid_of(&planned.summary)?;
            Ok(destroy_workspace(
                ctx.pve.as_ref(),
                config(ctx)?,
                &ctx.pools.workspaces,
                &planned.summary,
                &self.timing,
                job,
            )
            .await?)
        })
    }
}
