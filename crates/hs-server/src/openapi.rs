//! OpenAPI document, generated from the handlers and checked in as
//! `docs/openapi.json`.

use utoipa::openapi::security::{HttpAuthScheme, HttpBuilder, SecurityScheme};
use utoipa::{Modify, OpenApi};

#[derive(OpenApi)]
#[openapi(
    info(title = "HomeStack server API", description = "Proxmox workspace lifecycle service."),
    paths(crate::routes::live, crate::routes::pve_version, crate::routes::pve_node_status),
    components(schemas(
        hs_model::ErrorBody,
        hs_model::ErrorDetail,
        hs_model::ErrorCode,
        hs_model::health::Liveness,
        hs_model::health::LivenessStatus,
        hs_model::pve::PveVersion,
        hs_model::pve::NodeStatus,
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
