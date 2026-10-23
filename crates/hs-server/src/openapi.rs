//! OpenAPI document, generated from the handlers and checked in as
//! `docs/openapi.json`.

use utoipa::openapi::security::{HttpAuthScheme, HttpBuilder, SecurityScheme};
use utoipa::{Modify, OpenApi};

#[derive(OpenApi)]
#[openapi(
    info(title = "HomeStack server API", description = "Proxmox workspace lifecycle service."),
    paths(
        crate::routes::live,
        crate::routes::service_status,
        crate::routes::pve_version,
        crate::routes::pve_node_status,
        crate::routes::config_get,
        crate::routes::config_put,
        crate::routes::config_version,
        crate::routes::capabilities,
        crate::secrets::put_pve_token,
        crate::secrets::put_tls,
        crate::reads::status,
        crate::image_updates::progress,
        crate::reads::workspace,
        crate::reads::storage,
        crate::reads::storage_overview,
        crate::reads::orphans,
        crate::reads::discovery,
        crate::reads::networks,
        crate::reads::gold_readiness,
        crate::jobs::create_plan,
        crate::jobs::get_plan,
        crate::jobs::apply_plan,
        crate::jobs::list_jobs,
        crate::jobs::get_job,
        crate::jobs::job_events,
    ),
    components(schemas(
        hs_model::ErrorBody,
        hs_model::ErrorDetail,
        hs_model::ErrorCode,
        hs_model::health::Liveness,
        hs_model::health::LivenessStatus,
        hs_model::pve::PveVersion,
        hs_model::pve::NodeStatus,
        hs_model::server_config::ConfigVersion,
        hs_model::server_config::ConfigUpdated,
        hs_model::server_config::TlsCertificate,
        hs_model::capabilities::Capabilities,
        hs_model::capabilities::OperationCapability,
        hs_model::capabilities::Operation,
        hs_model::capabilities::Privilege,
        hs_model::capabilities::Installation,
        hs_model::capabilities::InstallationCheck,
        hs_model::capabilities::InstallationItem,
        hs_model::capabilities::InstallationStatus,
        hs_model::status::GlobalStatus,
        hs_model::status::StatusEntry,
        hs_model::status::HomeUsage,
        hs_model::status::DeployedService,
        hs_model::status::ServiceRates,
        hs_model::status::PoolInfo,
        hs_model::status::StatusSummary,
        hs_model::status::Capacity,
        hs_model::status::LayoutCapacity,
        hs_model::status::NodeState,
        hs_model::status::ClusterQuorum,
        hs_model::status::HomesSummary,
        hs_model::status::OrphanVolume,
        hs_model::status::Orphans,
        hs_model::status::WorkspaceStatus,
        hs_model::status::VolumeEntry,
        hs_model::status::SshCommands,
        hs_model::status::Discovery,
        hs_model::status::NetworkCandidate,
        hs_model::status::GoldReadiness,
        hs_model::status::GoldCheck,
        hs_model::jobs::PlanRequest,
        hs_model::jobs::Plan,
        hs_model::jobs::ApplyRequest,
        hs_model::jobs::ApplySecrets,
        hs_model::jobs::Job,
        hs_model::jobs::JobState,
        hs_model::jobs::JobStep,
        hs_model::jobs::StepState,
        hs_model::jobs::JobTask,
        hs_model::jobs::TaskStatus,
        hs_model::jobs::JobProgress,
        hs_model::jobs::JobError,
        hs_model::jobs::StreamMessage,
        hs_model::jobs::StreamMessageType,
    )),
    modifiers(&BearerAuth),
)]
struct ApiDoc;

struct BearerAuth;

impl Modify for BearerAuth {
    fn modify(&self, openapi: &mut utoipa::openapi::OpenApi) {
        let components = openapi.components.get_or_insert_with(Default::default);
        components.add_security_scheme(
            "bearer",
            SecurityScheme::Http(HttpBuilder::new().scheme(HttpAuthScheme::Bearer).build()),
        );
    }
}

/// The OpenAPI document as pretty JSON with a trailing newline.
pub fn openapi_json() -> Result<String, serde_json::Error> {
    let mut json = ApiDoc::openapi().to_pretty_json()?;
    json.push('\n');
    Ok(json)
}
