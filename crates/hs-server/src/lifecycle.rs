//! `workspace.create`, `workspace.destroy` (phase 5), `workspace.refresh`
//! (phase 6), `workspace.resize` (phase 7), `workspace.migrate` (phase 8) and
//! `workspace.addkey` (phase 9): the operations of `hs_core::lifecycle`,
//! `hs_core::refresh`, `hs_core::resize`, `hs_core::migrate` and
//! `hs_core::addkey` in the plan and job model.
//!
//! `POST /v1/plans` with `{"operation": "workspace.create", "parameters":
//! {"vmid": 240, "name": "hst-a", "home_size": "20G", "storage": "..."}}`
//! (`home_size` and `storage` optional), or `{"operation":
//! "workspace.destroy", "parameters": {"vmid": 240}}`. The job's result is
//! the plan with what the client needs for its local steps and the event,
//! `workspace.created` or `workspace.destroyed`.
//!
//! `{"operation": "workspace.refresh", "parameters": {"vmid": 240}}` replaces
//! the root with a copy of Gold's, or, when the VM's description holds the
//! journal of an interrupted refresh, recovers it; `"force": true` attaches
//! the new root of such a journal even when the VM disagrees with it, without
//! deleting any volume (decision 4). Events `workspace.refreshed` and
//! `workspace.refresh_recovered`.
//!
//! `{"operation": "workspace.resize", "parameters": {"vmid": 240,
//! "home_size": "21G"}}` (or `root_size`) grows a disk to that size; the
//! same size again finishes an interrupted growth. A plan whose
//! `secrets_required` names `luks_passphrase` is applied with
//! `{"digest": "...", "secrets": {"luks_passphrase": "..."}}` (decision 6).
//! Event `workspace.resized`.
//!
//! `{"operation": "workspace.migrate", "parameters": {"vmid": 240,
//! "target_node": "pve3", "target_storage": "local-lvm"}}`
//! (`target_storage` optional: the first storage of the node's layout)
//! moves the workspace offline with its disks and keeps its power state; a
//! failure after the shutdown starts it again on the source node. Event
//! `workspace.migrated`.
//!
//! `{"operation": "workspace.addkey", "parameters": {"vmid": 240, "key":
//! "ssh-ed25519 AAAA... name", "restart": false}}` (`key` optional: the
//! keys already in the VM's `sshkeys`; `restart` optional, false) adds the
//! key to `sshkeys`, switches the VM to the keep vendor snippet and, on a
//! running VM, appends the keys to root's and the user's `authorized_keys`
//! through the guest agent. A running VM is restarted only with `"restart":
//! true`; `pending_restart` in the result says a restart is still needed.
//! Event `workspace.key_added`.

use std::sync::Arc;

use hs_core::CoreError;
use hs_core::addkey::{AddkeyRequest, add_key, plan_addkey};
use hs_core::lifecycle::{
    CreateRequest, LifecycleTiming, create_workspace, destroy_workspace, plan_create, plan_destroy, plan_power,
    power_workspace,
};
use hs_core::migrate::{MigrateRequest, migrate_workspace, plan_migrate};
use hs_core::refresh::{plan_refresh, refresh_workspace};
use hs_core::resize::{ResizeRequest, plan_resize, resize_workspace};
use hs_model::jobs::Secret;
use serde::Deserialize;
use serde_json::Value;

use crate::operations::{BoxFuture, JobHandle, OpContext, OpFailure, Operation, Planned};

/// The lifecycle operations with `timing`.
#[must_use]
pub fn operations(timing: LifecycleTiming) -> Vec<Arc<dyn Operation>> {
    vec![
        Arc::new(Create { timing }),
        Arc::new(Destroy { timing }),
        Arc::new(Refresh { timing }),
        Arc::new(Resize { timing }),
        Arc::new(Migrate { timing }),
        Arc::new(Addkey { timing }),
        Arc::new(Power { timing, start: true }),
        Arc::new(Power { timing, start: false }),
    ]
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

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct RefreshParameters {
    vmid: u32,
    #[serde(default)]
    force: bool,
}

struct Refresh {
    timing: LifecycleTiming,
}

impl Operation for Refresh {
    fn name(&self) -> &'static str {
        "workspace.refresh"
    }

    fn plan<'a>(&'a self, ctx: &'a OpContext, parameters: &'a Value) -> BoxFuture<'a, Result<Planned, OpFailure>> {
        Box::pin(async move {
            let parameters: RefreshParameters = serde_json::from_value(parameters.clone())
                .map_err(|_| OpFailure::invalid("workspace.refresh takes vmid and optionally force."))?;
            let summary = plan_refresh(
                ctx.pve.as_ref(),
                config(ctx)?,
                &ctx.pools.workspaces,
                parameters.vmid,
                parameters.force,
            )
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
            Ok(refresh_workspace(
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

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct ResizeParameters {
    vmid: u32,
    root_size: Option<String>,
    home_size: Option<String>,
}

struct Resize {
    timing: LifecycleTiming,
}

impl Operation for Resize {
    fn name(&self) -> &'static str {
        "workspace.resize"
    }

    fn plan<'a>(&'a self, ctx: &'a OpContext, parameters: &'a Value) -> BoxFuture<'a, Result<Planned, OpFailure>> {
        Box::pin(async move {
            let parameters: ResizeParameters = serde_json::from_value(parameters.clone())
                .map_err(|_| OpFailure::invalid("workspace.resize takes vmid and one of root_size and home_size."))?;
            let request = ResizeRequest {
                vmid: parameters.vmid,
                root_size: parameters.root_size,
                home_size: parameters.home_size,
            };
            let summary = plan_resize(ctx.pve.as_ref(), config(ctx)?, &ctx.pools.workspaces, &request)
                .await
                .map_err(|error| match error {
                    CoreError::NotFound(_) => OpFailure::not_found("No such VM."),
                    other => OpFailure::Core(other),
                })?;
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
            let passphrase = ctx.secrets.luks_passphrase.as_ref().map(Secret::expose);
            Ok(resize_workspace(ctx.pve.as_ref(), &planned.summary, passphrase, &self.timing, job).await?)
        })
    }
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct MigrateParameters {
    vmid: u32,
    target_node: String,
    target_storage: Option<String>,
}

struct Migrate {
    timing: LifecycleTiming,
}

impl Operation for Migrate {
    fn name(&self) -> &'static str {
        "workspace.migrate"
    }

    fn plan<'a>(&'a self, ctx: &'a OpContext, parameters: &'a Value) -> BoxFuture<'a, Result<Planned, OpFailure>> {
        Box::pin(async move {
            let parameters: MigrateParameters = serde_json::from_value(parameters.clone()).map_err(|_| {
                OpFailure::invalid("workspace.migrate takes vmid and target_node, and optionally target_storage.")
            })?;
            let request = MigrateRequest {
                vmid: parameters.vmid,
                target_node: parameters.target_node,
                target_storage: parameters.target_storage,
            };
            let summary = plan_migrate(ctx.pve.as_ref(), config(ctx)?, &ctx.pools.workspaces, &request)
                .await
                .map_err(|error| match error {
                    CoreError::NotFound(_) => OpFailure::not_found("No such VM."),
                    other => OpFailure::Core(other),
                })?;
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
            Ok(migrate_workspace(
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

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct AddkeyParameters {
    vmid: u32,
    key: Option<String>,
    #[serde(default)]
    restart: bool,
}

struct Addkey {
    timing: LifecycleTiming,
}

impl Operation for Addkey {
    fn name(&self) -> &'static str {
        "workspace.addkey"
    }

    fn plan<'a>(&'a self, ctx: &'a OpContext, parameters: &'a Value) -> BoxFuture<'a, Result<Planned, OpFailure>> {
        Box::pin(async move {
            let parameters: AddkeyParameters = serde_json::from_value(parameters.clone())
                .map_err(|_| OpFailure::invalid("workspace.addkey takes vmid, and optionally key and restart."))?;
            let request = AddkeyRequest {
                vmid: parameters.vmid,
                key: parameters.key,
                restart: parameters.restart,
            };
            let summary = plan_addkey(ctx.pve.as_ref(), config(ctx)?, &ctx.pools.workspaces, &request)
                .await
                .map_err(|error| match error {
                    CoreError::NotFound(_) => OpFailure::not_found("No such VM."),
                    other => OpFailure::Core(other),
                })?;
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
            Ok(add_key(
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

/// `workspace.start` and `workspace.stop`: `{"vmid": 240}` (owner,
/// 2026-10-14). Events `workspace.started` and `workspace.stopped`.
struct Power {
    timing: LifecycleTiming,
    start: bool,
}

impl Operation for Power {
    fn name(&self) -> &'static str {
        if self.start {
            "workspace.start"
        } else {
            "workspace.stop"
        }
    }

    fn plan<'a>(&'a self, ctx: &'a OpContext, parameters: &'a Value) -> BoxFuture<'a, Result<Planned, OpFailure>> {
        Box::pin(async move {
            let parameters: DestroyParameters = serde_json::from_value(parameters.clone())
                .map_err(|_| OpFailure::invalid("workspace.start and workspace.stop take vmid."))?;
            let summary = plan_power(
                ctx.pve.as_ref(),
                config(ctx)?,
                &ctx.pools.workspaces,
                parameters.vmid,
                self.start,
            )
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
            Ok(power_workspace(
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
