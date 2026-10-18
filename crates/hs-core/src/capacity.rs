//! Shared storage admission check before operations allocate VM disks.

use serde_json::{Value, json};

use crate::error::CoreError;
use crate::pve::Pve;

pub(crate) async fn require_space(
    pve: &impl Pve,
    target: &str,
    storage: &str,
    required: u64,
) -> Result<Value, CoreError> {
    let status = pve
        .get(&format!("/nodes/{target}/storage/{storage}/status"), &[])
        .await?;
    let enabled = |key: &str| status[key] == true || status[key] == 1;
    if !enabled("active") || !enabled("enabled") {
        return Err(CoreError::Refused(format!(
            "Target storage {storage} on {target} is disabled or inactive"
        )));
    }
    if !status["content"]
        .as_str()
        .is_some_and(|content| content.split(',').any(|kind| kind == "images"))
    {
        return Err(CoreError::Refused(format!(
            "Target storage {storage} on {target} does not support VM images"
        )));
    }
    let available = status["avail"].as_u64().ok_or_else(|| {
        CoreError::Refused(format!(
            "Cannot determine available space on target storage {storage} on {target}"
        ))
    })?;
    if required > available {
        return Err(CoreError::Refused(format!(
            "Not enough free space on target storage {storage} on {target}: requires {required} bytes, available {available} bytes"
        )));
    }
    Ok(json!({"required_bytes": required, "available_bytes": available}))
}
