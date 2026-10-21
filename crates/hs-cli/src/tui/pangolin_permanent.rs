//! Step 7: automatic credential discovery, selected permission actions and durable recovery.
use super::pangolin::{Wizard, next_test};
use super::pangolin_key::{self as bootstrap, Client, Credential, Probe};
use super::pangolin_store::Storage;
use super::pangolin_tunnel::Endpoint;
use crossterm::event::{KeyCode, KeyEvent};
use serde::{Deserialize, Serialize};
use std::collections::BTreeSet;
use std::path::PathBuf;
use std::sync::atomic::AtomicBool;
use std::time::Instant;
use zeroize::Zeroizing;

pub const ACTIONS: [&str; 26] = [
    "applyBlueprint",
    "createSite",
    "getSite",
    "listSites",
    "updateSite",
    "deleteSite",
    "createResource",
    "getResource",
    "listResources",
    "updateResource",
    "deleteResource",
    "createTarget",
    "getTarget",
    "listTargets",
    "updateTarget",
    "deleteTarget",
    "getOrg",
    "listOrgDomains",
    "getDomain",
    "getResourcePolicy",
    "updateResourcePolicy",
    "setResourcePolicyRules",
    "createResourceRule",
    "listResourceRules",
    "updateResourceRule",
    "deleteResourceRule",
];
const ORG_REQUIRED: &str = "Organization not verified. Grant getOrg to the bootstrap key and verify Step 6 again.";
const MULTIPLE: &str = "Multiple HomeStack API keys exist. Automatic selection is unsafe.";
type Result<T> = std::result::Result<T, String>;
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum Stage {
    NoMutation,
    Prepared,
    CreationRequested,
    RotationRequested,
    Created,
    CredentialSaved,
    ActionsVerified,
    Complete,
}
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Record {
    pub target: String,
    pub organization: String,
    pub name: String,
    pub key_id: Option<String>,
    pub created_here: bool,
    pub stage: Stage,
    pub credential_path: PathBuf,
    pub credential_saved: bool,
    pub actions_verified: bool,
    pub at: String,
    #[serde(default)]
    pub rotation_from: Option<String>,
}
#[derive(Debug, Clone, PartialEq)]
pub enum Plan {
    Create,
    Permissions { id: String, before: BTreeSet<String> },
    Rotate { id: String },
}
#[derive(Debug, Clone, Copy, PartialEq)]
pub enum Operation {
    Inspect,
    Apply,
}
#[derive(Debug, Clone, PartialEq, Default)]
pub struct View {
    pub running: u64,
    pub cancel_requested: bool,
    pub confirm: bool,
    pub confirm_yes: bool,
    pub replace: bool,
    pub result: Option<Outcome>,
    pub operation: Option<Operation>,
    pub tunnel: u64,
    pub target: String,
    pub organization: String,
}
#[derive(Debug, Clone, PartialEq, Default)]
pub struct Outcome {
    pub error: Option<String>,
    pub ids: Vec<String>,
    pub assigned: BTreeSet<String>,
    pub record: Option<Record>,
    pub plan: Option<Plan>,
    pub verified: bool,
    pub reads: bool,
    pub negatives: bool,
}
impl Outcome {
    pub fn failed(message: &str) -> Self {
        Self {
            error: Some(message.into()),
            ..Self::default()
        }
    }
    pub fn error_summary(&self) -> Option<&'static str> {
        let error = self.error.as_deref()?;
        Some(if error.contains("deleteApiKey") {
            "Grant deleteApiKey to the bootstrap key, then Refresh."
        } else if error.contains("uncertain")
            || error.contains("Uncertain")
            || error.contains("recovery")
            || error.contains("Recovery")
        {
            "Recovery needs attention. Open F7 Details before retrying."
        } else if error.contains("changed") || error.contains("Changed") {
            "State changed. Review the counts or Refresh before retrying."
        } else if error.contains("Negative authorization") {
            "Security check failed. Review key permissions in F7 Details."
        } else if error.contains("storage")
            || error.contains("destination")
            || error.contains("credential")
            || error.contains("Credential")
        {
            "Check private credential storage in F7 Details, then Refresh."
        } else {
            "Setup failed. Open F7 Details, resolve the cause, then Refresh."
        })
    }
    pub fn missing(&self) -> Vec<&str> {
        ACTIONS
            .iter()
            .copied()
            .filter(|a| !self.assigned.contains(*a))
            .collect()
    }
    pub fn extra(&self) -> Vec<&str> {
        self.assigned
            .iter()
            .map(String::as_str)
            .filter(|a| !ACTIONS.contains(a))
            .collect()
    }
}
/// Worker inputs deliberately have no Debug implementation.
pub(super) struct Input {
    target: String,
    org: String,
    root: Credential,
    trusted: Vec<PathBuf>,
    legacy: PathBuf,
    replace: bool,
    destination: PathBuf,
    operation: Operation,
    plan: Option<Plan>,
}
fn stamp() -> String {
    time::OffsetDateTime::now_utc()
        .format(&time::format_description::well_known::Rfc3339)
        .unwrap_or_default()
}
fn expected() -> BTreeSet<String> {
    ACTIONS.into_iter().map(str::to_owned).collect()
}
fn api<T>(result: std::result::Result<T, bootstrap::Problem>) -> Result<T> {
    result.map_err(|p| match p {
        bootstrap::Problem::Rejected(_) => "API credential rejected. Check the complete ID.secret token.".into(),
        _ => p.message().into(),
    })
}
fn org(client: &Client<'_>, id: &str) -> Result<()> {
    match api(client.get(&format!("/v1/org/{id}"), Probe::Organization))? {
        (200, Some(data)) if data.get("org").and_then(|v| v.get("orgId")).and_then(|v| v.as_str()) == Some(id) => {
            Ok(())
        }
        _ => Err("Organization identity could not be independently verified.".into()),
    }
}
fn keys(client: &Client<'_>, org: &str) -> Result<Vec<String>> {
    let rows = api(client.collection(&format!("/v1/org/{org}/api-keys"), "apiKeys", Probe::Keys))?;
    let mut seen = BTreeSet::new();
    let mut ids = Vec::new();
    for row in rows {
        let id = row
            .get("apiKeyId")
            .and_then(|v| v.as_str())
            .filter(|id| bootstrap::valid_identifier(id))
            .ok_or("Incompatible key identifier; response omitted.")?;
        if !seen.insert(id.to_owned())
            || row.get("orgId").and_then(|v| v.as_str()) != Some(org)
            || row.get("name").and_then(|v| v.as_str()).is_none()
        {
            return Err("Incompatible organization key metadata; response omitted.".into());
        }
        if row.get("name").and_then(|v| v.as_str()) == Some("homestack") {
            // Pinned API filters this listing to organization-scoped, non-Root keys.
            // Explicit Root indicators, if returned by another version, must also agree.
            if ["root", "isRoot"]
                .iter()
                .any(|f| row.get(*f).is_some_and(|v| v != &serde_json::Value::Bool(false)))
            {
                return Err("HomeStack key has incompatible Root metadata.".into());
            }
            ids.push(id.to_owned());
        }
    }
    Ok(ids)
}
fn action_path(org: &str, id: &str) -> String {
    format!("/v1/org/{org}/api-key/{id}/actions")
}
fn actions(client: &Client<'_>, org: &str, id: &str) -> Result<BTreeSet<String>> {
    api(client.collection(&action_path(org, id), "actions", Probe::Actions))?
        .into_iter()
        .map(|row| {
            row.get("actionId")
                .and_then(|v| v.as_str())
                .filter(|id| bootstrap::valid_identifier(id))
                .map(str::to_owned)
                .ok_or_else(|| {
                    "Incompatible action IDs. Check the pinned Pangolin version; no IDs were guessed.".into()
                })
        })
        .collect()
}
fn reads(client: &Client<'_>, organization: &str) -> Result<()> {
    org(client, organization)?;
    for field in ["sites", "resources", "domains"] {
        match api(client.get(&format!("/v1/org/{organization}/{field}"), Probe::Organization))? {
            (200, Some(data)) if data.get(field).is_some_and(|v| v.is_array()) => {}
            _ => return Err("Independent Site, Resource or domain metadata read failed; response omitted.".into()),
        }
    }
    Ok(())
}
fn negatives(client: &Client<'_>, org: &str, id: &str) -> Result<()> {
    let other = format!("bootstrap-scope-probe-{}", uuid::Uuid::new_v4().simple());
    for path in [
        format!("/v1/org/{org}/api-keys"),
        action_path(org, id),
        format!("/v1/org/{other}/sites"),
    ] {
        if api(client.get(&path, Probe::Actions))?.0 != 403 {
            return Err("Negative authorization check failed: HTTP 403 is required for both administration endpoints and other-organization access.".into());
        }
    }
    Ok(())
}
fn save(storage: &Storage, record: &mut Record, stage: Stage) -> Result<()> {
    record.stage = stage;
    record.at = stamp();
    storage.save(record)
}
/// Only the secret field is deserialized into a zeroizing buffer. Never a printable JSON Value.
#[derive(Deserialize)]
struct Created {
    success: bool,
    error: bool,
    data: CreatedData,
}
#[derive(Deserialize)]
struct CreatedData {
    #[serde(rename = "apiKeyId")]
    id: String,
    #[serde(rename = "apiKey", deserialize_with = "secret")]
    secret: Zeroizing<String>,
}
fn secret<'de, D: serde::Deserializer<'de>>(deserializer: D) -> std::result::Result<Zeroizing<String>, D::Error> {
    String::deserialize(deserializer).map(Zeroizing::new)
}
pub(super) fn execute(endpoint: &Endpoint, input: Input, stop: &AtomicBool) -> Outcome {
    let mut out = Outcome::default();
    let applying = input.operation == Operation::Apply;
    if let Err(error) = work(endpoint, input, stop, &mut out) {
        out.error = Some(
            if out.record.as_ref().is_some_and(|r| r.stage == Stage::CreationRequested)
                || out.record.as_ref().is_some_and(|r| r.stage == Stage::RotationRequested)
                || (applying
                    && (stop.load(std::sync::atomic::Ordering::SeqCst) || !endpoint.alive())
                    && out
                        .record
                        .as_ref()
                        .is_some_and(|r| !matches!(r.stage, Stage::NoMutation | Stage::Prepared)))
            {
                format!("Outcome uncertain. {error} Inspect F7 recovery evidence; do not retry creation.")
            } else {
                error
            },
        );
    }
    out
}
fn work(endpoint: &Endpoint, mut input: Input, stop: &AtomicBool, out: &mut Outcome) -> Result<()> {
    let root_result = bootstrap::verify(endpoint, &input.org, &input.root, stop);
    if root_result.problem.is_some() || root_result.organization != Some(true) {
        return Err(ORG_REQUIRED.into());
    }
    let root = Client {
        endpoint,
        credential: &input.root,
        stop,
        started: Instant::now(),
    };
    org(&root, &input.org)?;
    out.ids = keys(&root, &input.org)?;
    if out.ids.len() > 1 {
        return Err(MULTIPLE.into());
    }
    let storage = Storage::open(&input.destination)?;
    let saved = storage.load::<Record>()?;
    let saved = if saved.is_none() && input.legacy.with_file_name("recovery.json").exists() {
        Storage::open(&input.legacy)?.load::<Record>()?
    } else {
        saved
    };
    let mut record = saved.unwrap_or(Record {
        target: input.target.clone(),
        organization: input.org.clone(),
        name: "homestack".into(),
        key_id: None,
        created_here: false,
        stage: Stage::NoMutation,
        credential_path: input.destination.clone(),
        credential_saved: false,
        actions_verified: false,
        at: stamp(),
        rotation_from: None,
    });
    if record.target != input.target
        || record.organization != input.org
        || record.name != "homestack"
        || record
            .key_id
            .as_ref()
            .is_some_and(|id| !bootstrap::valid_identifier(id))
        || !record.credential_path.is_absolute()
    {
        return Err("Recovery identity differs. Resolve the non-secret recovery record explicitly.".into());
    }
    out.record = Some(record.clone());
    if record.stage == Stage::RotationRequested {
        return Err("Rotation outcome uncertain. Inspect F7 recovery evidence before any further operation.".into());
    }
    let rotating = input.operation == Operation::Apply && matches!(input.plan, Some(Plan::Rotate { .. }));
    let state_destination = input.destination.clone();
    if let (true, Some(Plan::Rotate { id })) = (rotating, &input.plan) {
        let stem = input
            .destination
            .file_stem()
            .and_then(|s| s.to_str())
            .ok_or("Invalid credential destination.")?;
        input.destination = input.destination.with_file_name(format!("{stem}-replacement-{id}.key"));
    } else if record.credential_path.parent() == input.destination.parent() {
        input.destination = record.credential_path.clone();
    }
    // Keep the identity lock and recovery journal stable across rotations. A new
    // credential gets a separate filename, preserving any old or rejected secret.
    let alternate_storage = if input.destination != state_destination {
        Some(Storage::open(&input.destination)?)
    } else {
        None
    };
    let credential_storage = alternate_storage.as_ref().unwrap_or(&storage);
    if matches!(record.stage, Stage::CreationRequested | Stage::Created) {
        // A crash may follow durable secret persistence but precede its metadata update.
        // Recover only by the actual local token ID, never by a name-only guess.
        let recovered = if record.credential_path == input.destination {
            credential_storage.recover_credential(&input.destination)
        } else {
            Storage::open(&record.credential_path)?.recover_credential(&record.credential_path)
        }.map_err(|_| "Outcome uncertain. Creation was requested and no safely persisted credential is available. Pangolin cannot return its original secret. Never retry creation; resolve recovery separately.")?;
        let id = recovered.key_id().map_err(|_| "Recovery credential invalid.")?;
        if out.ids != vec![id.to_owned()] || record.key_id.as_ref().is_some_and(|known| known != id) {
            return Err(
                "Uncertain creation cannot be matched to the safely persisted credential. Resolve recovery separately."
                    .into(),
            );
        }
        record.key_id = Some(id.to_owned());
        record.credential_saved = true;
        save(&storage, &mut record, Stage::CredentialSaved)?;
        out.record = Some(record.clone());
    }
    if record.key_id.as_ref().is_some_and(|id| out.ids.first() != Some(id)) {
        return Err("Recovery key ID no longer matches the unique organization key. No mutation performed.".into());
    }
    if let Some(id) = out.ids.first() {
        out.assigned = actions(&root, &input.org, id)?;
    }
    if rotating {
        let Some(Plan::Rotate { id }) = &input.plan else {
            unreachable!()
        };
        if out.ids != vec![id.clone()]
            || record.created_here && !matches!(record.stage, Stage::Complete | Stage::NoMutation)
        {
            return Err("Recovery or key identity changed. Rotation refused; inspect F7.".into());
        }
        let root_id = input.root.key_id().map_err(|_| "Bootstrap credential invalid.")?;
        if id == root_id || !actions(&root, &input.org, root_id)?.contains("deleteApiKey") {
            return Err(
                "Rotation requires deleteApiKey on the bootstrap key. Grant it explicitly, then Refresh.".into(),
            );
        }
        credential_storage.release()?;
        credential_storage.reserve()?;
        if keys(&root, &input.org)? != vec![id.clone()] {
            return Err("Key identity changed. Refresh before replacing the key.".into());
        }
        record.rotation_from = Some(id.clone());
        record.key_id = Some(id.clone());
        save(&storage, &mut record, Stage::RotationRequested)?;
        out.record = Some(record.clone());
        let status = api(root.request(
            "DELETE",
            &format!("/v1/org/{}/api-key/{id}", input.org),
            None,
            Probe::Keys,
        ))?
        .0;
        if status != 200 || !keys(&root, &input.org)?.is_empty() {
            return Err("Rotation outcome uncertain. Inspect F7; deletion will not be retried.".into());
        }
        record.key_id = None;
        record.credential_saved = false;
        record.credential_path = input.destination.clone();
        record.created_here = false;
        save(&storage, &mut record, Stage::Prepared)?;
        out.ids.clear();
    }
    if out.ids.is_empty() && input.operation == Operation::Inspect {
        if !matches!(record.stage, Stage::NoMutation | Stage::Prepared) {
            return Err("Unresolved recovery prevents creation. Inspect F7.".into());
        }
        out.plan = Some(Plan::Create);
        return Ok(());
    }
    let creating = input.operation == Operation::Apply && (input.plan == Some(Plan::Create) || rotating);
    if creating && !rotating {
        if !matches!(record.stage, Stage::NoMutation | Stage::Prepared) {
            return Err("Unresolved recovery prevents creation. Inspect F7.".into());
        }
        credential_storage.release()?;
        credential_storage.reserve()?;
        record.credential_path = input.destination.clone();
        save(&storage, &mut record, Stage::Prepared)?;
    }
    let credential;
    if creating {
        if !out.ids.is_empty() {
            return Err("A homestack key appeared before creation. Creation refused.".into());
        }
        if record.stage != Stage::Prepared || !credential_storage.has_pending() {
            return Err("No reserved and reviewed creation plan.".into());
        }
        // Repeat identity and full name-conflict checks immediately before the single PUT.
        org(&root, &input.org)?;
        if !keys(&root, &input.org)?.is_empty() {
            return Err("A homestack key appeared immediately before creation. Creation refused.".into());
        }
        record.created_here = true;
        save(&storage, &mut record, Stage::CreationRequested)?;
        out.record = Some(record.clone());
        let (status, response) = api(root.request(
            "PUT",
            &format!("/v1/org/{}/api-key", input.org),
            Some(r#"{"name":"homestack"}"#),
            Probe::Keys,
        ))?;
        if status != 200 {
            return Err("Creation requested; outcome uncertain. Do not retry creation automatically.".into());
        }
        let created: Created = serde_json::from_slice(&response)
            .map_err(|_| "Creation response incompatible; secret may be unavailable. Manual recovery required.")?;
        if !created.success || created.error || !bootstrap::valid_identifier(&created.data.id) {
            return Err("Creation outcome uncertain; incompatible response omitted.".into());
        }
        drop(response);
        record.key_id = Some(created.data.id.clone());
        record.stage = Stage::Created;
        out.record = Some(record.clone());
        credential = {
            let mut token = Credential::default();
            token.insert(&created.data.id);
            token.insert(".");
            token.insert(&created.data.secret);
            if token.key_id().is_err() {
                let _ = save(&storage, &mut record, Stage::Created);
                return Err("Created secret incompatible with pinned ID.secret format; recover separately.".into());
            }
            token
        };
        record.key_id = Some(created.data.id);
        record.stage = Stage::Created;
        out.record = Some(record.clone());
        // Save the secret before any metadata/network operation that could fail.
        // CreationRequested remains a conservative crash barrier until persistence succeeds.
        if credential_storage.persist(&credential).is_err() {
            let _ = save(&storage, &mut record, Stage::Created);
            out.record = Some(record.clone());
            return Err("Key created; credential not safely persisted. Recovery is required. If the complete local token is unavailable, its secret cannot be retrieved from Pangolin. Never create a replacement automatically.".into());
        }
        record.credential_saved = true;
        save(&storage, &mut record, Stage::CredentialSaved)?;
        out.record = Some(record.clone());
        out.ids = vec![record.key_id.clone().ok_or("Missing created ID.")?];
    } else {
        let Some(id) = out.ids.first() else {
            return Err("No HomeStack key found. Inspect before planning creation.".into());
        };
        let mut found = if credential_storage.has_pending() {
            let token = credential_storage.recover_credential(&input.destination)?;
            (token.key_id().ok() == Some(id.as_str())).then_some(token)
        } else {
            None
        };
        let mut candidates = vec![record.credential_path.clone(), input.destination.clone()];
        candidates.extend(input.trusted.iter().cloned());
        for path in candidates {
            if found.is_some() {
                break;
            }
            if let Ok(token) = super::pangolin_store::read_credential(&path)
                && token.key_id().ok() == Some(id.as_str())
            {
                found = Some(token);
                break;
            }
        }
        let Some(token) = found else {
            out.plan = Some(Plan::Rotate { id: id.clone() });
            return Ok(());
        };
        credential = token;
        if credential.key_id().map_err(|_| "Invalid ID.secret credential.")? != id {
            return Err("Credential key ID does not match the existing organization key. Adoption refused.".into());
        }
    }
    let id = credential.key_id().map_err(|_| "Invalid credential.")?.to_owned();
    let permanent = Client {
        endpoint,
        credential: &credential,
        stop,
        started: Instant::now(),
    };
    // Newly created keys have no actions until the approved assignment below.
    if !creating {
        // Pangolin authenticates before checking actions. HTTP 403 proves possession
        // even when getOrg is missing; the bootstrap listing independently binds this
        // ID to the organization. Full permanent-token reads remain mandatory below.
        {
            match permanent.get(&format!("/v1/org/{}", input.org), Probe::Organization) {
                Err(bootstrap::Problem::Rejected(_)) => {
                    out.plan = Some(Plan::Rotate { id: id.clone() });
                    return Ok(());
                }
                result => {
                    match api(result)? {
                        (403, _) => {}
                        (200, Some(data))
                            if data.get("org").and_then(|v| v.get("orgId")).and_then(|v| v.as_str())
                                == Some(input.org.as_str()) => {}
                        _ => return Err(
                            "Credential cannot verify this organization. Check its access in Pangolin, then Refresh."
                                .into(),
                        ),
                    }
                }
            }
        }
        let path = input.destination.clone();
        if credential_storage.has_credential() || credential_storage.has_pending() {
            let stored = credential_storage.recover_credential(&path)?;
            if stored.value != credential.value {
                return Err("Saved credential has different content. Resolve the conflict in F7.".into());
            }
        } else {
            credential_storage.reserve()?;
            credential_storage.persist(&credential)?;
        }
        record.key_id = Some(id.clone());
        record.credential_path = path;
        record.credential_saved = true;
        // Preserve the previous action evidence only until this run reads it again.
        record.actions_verified = false;
        save(&storage, &mut record, Stage::CredentialSaved)?;
        out.record = Some(record.clone());
    }
    let required = expected();
    let desired = if creating || input.replace {
        required.clone()
    } else {
        out.assigned.union(&required).cloned().collect()
    };
    let differs = out.assigned != desired;
    let plan = Plan::Permissions {
        id: id.clone(),
        before: out.assigned.clone(),
    };
    if !creating && out.assigned != required {
        out.plan = Some(plan.clone());
        if input.operation == Operation::Inspect {
            return Ok(());
        }
        if input.plan != Some(plan) {
            return Err("Permissions changed. Review the refreshed counts and execute again.".into());
        }
    }
    if creating || (differs && input.operation == Operation::Apply) {
        if keys(&root, &input.org)? != vec![id.clone()] {
            return Err("Key conflict changed; permission replacement refused.".into());
        }
        org(&root, &input.org)?;
        let latest = actions(&root, &input.org, &id)?;
        if !creating && latest != out.assigned {
            out.assigned = latest.clone();
            out.plan = Some(Plan::Permissions {
                id: id.clone(),
                before: latest,
            });
            return Err("Permissions changed. Review the refreshed counts and execute again.".into());
        }
        let body = serde_json::to_string(&serde_json::json!({"actionIds": desired}))
            .map_err(|_| "Cannot encode approved actions.")?;
        // A failed or uncertain setter requires Refresh, never another blind write.
        out.plan = None;
        if api(root.request("POST", &action_path(&input.org, &id), Some(&body), Probe::Actions))?.0 != 200 {
            return Err(
                "Action assignment rejected. Check compatibility with the required 26-action profile. Credential is retained; recheck before applying another reviewed plan."
                    .into(),
            );
        }
    }
    out.assigned = actions(&root, &input.org, &id)?;
    if out.assigned != desired || !required.is_subset(&out.assigned) {
        return Err("Permission readback differs from the selected operation. Refresh before retrying.".into());
    }
    if keys(&root, &input.org)? != vec![id.clone()] {
        return Err("Cannot confirm a unique organization-scoped, non-Root homestack key.".into());
    }
    record.actions_verified = true;
    save(&storage, &mut record, Stage::ActionsVerified)?;
    out.record = Some(record.clone());
    reads(&permanent, &input.org)?;
    out.reads = true;
    if let Err(error) = negatives(&permanent, &input.org, &id) {
        if out.assigned != required {
            out.plan = Some(Plan::Permissions {
                id: id.clone(),
                before: out.assigned.clone(),
            });
        }
        return Err(error);
    }
    out.negatives = true;
    // Verify secure availability again instead of trusting a prior write or referenced file.
    let stored = super::pangolin_store::read_credential(&record.credential_path)?;
    if stored.value != credential.value {
        return Err("Stored credential changed during verification.".into());
    }
    if !endpoint.alive() || stop.load(std::sync::atomic::Ordering::SeqCst) {
        return Err("Tunnel lost or operation cancelled; verification incomplete.".into());
    }
    save(&storage, &mut record, Stage::Complete)?;
    out.record = Some(record);
    out.plan = None;
    out.verified = true;
    Ok(())
}

impl Wizard {
    pub fn permanent_available(&self) -> bool {
        self.bootstrap_verified()
            && self
                .bootstrap_check()
                .is_some_and(|c| c.outcome.organization == Some(true))
    }
    pub fn permanent_current(&self) -> bool {
        self.permanent.target == self.target
            && self.permanent.organization == self.bootstrap.organization
            && self.permanent.tunnel == self.tunnel.id
            && self.tunnel_connected()
    }
    pub fn permanent_verified(&self) -> bool {
        self.permanent_available()
            && self.permanent_current()
            && self.permanent.running == 0
            && self.permanent.result.as_ref().is_some_and(|r| r.verified)
    }
    pub fn permanent_legacy_destination(&self) -> PathBuf {
        self.path
            .parent()
            .unwrap_or(std::path::Path::new("/"))
            .join("secrets/pangolin")
            .join(self.permanent_identity())
            .join("homestack.key")
    }
    pub(super) fn permanent_identity(&self) -> String {
        use sha2::{Digest, Sha256};
        Sha256::digest(format!("{}\0{}", self.target, self.bootstrap.organization).as_bytes())
            .iter()
            .map(|byte| format!("{byte:02x}"))
            .collect()
    }
    pub fn permanent_destination(&self) -> PathBuf {
        let home = std::env::var_os("HOME")
            .map(PathBuf::from)
            .unwrap_or_else(|| PathBuf::from("/"));
        let config = std::env::var_os("XDG_CONFIG_HOME")
            .filter(|s| !s.is_empty())
            .map(PathBuf::from)
            .unwrap_or_else(|| home.join(".config"));
        config
            .join("homestack/secrets")
            .join(format!("pangolin-{}.key", self.permanent_identity()))
    }
    pub fn permanent_controls(&self) -> Vec<usize> {
        if self.permanent.running != 0 || !self.permanent_available() {
            return vec![0];
        }
        match self.permanent.result.as_ref().and_then(|r| r.plan.as_ref()) {
            Some(Plan::Permissions { .. }) => vec![0, 1, 2],
            Some(_) => vec![0, 1],
            None => vec![0, 1],
        }
    }
    pub fn permanent_counts(&self) -> (usize, usize, usize) {
        let Some(out) = &self.permanent.result else {
            return (0, 0, 0);
        };
        let required = expected();
        let replace = self.permanent.replace;
        (
            if replace {
                out.assigned.intersection(&required).count()
            } else {
                out.assigned.len()
            },
            required.difference(&out.assigned).count(),
            if replace {
                out.assigned.difference(&required).count()
            } else {
                0
            },
        )
    }
    pub fn permanent_can_apply(&self) -> bool {
        self.permanent_available()
            && self.permanent_current()
            && self.permanent.running == 0
            && self.permanent.result.as_ref().is_some_and(|r| r.plan.is_some())
    }
    pub fn permanent_action(&mut self, key: KeyCode) -> Option<Operation> {
        if self.permanent.running != 0 {
            return None;
        }
        if key == KeyCode::F(6) && !self.permanent.confirm {
            return Some(Operation::Inspect);
        }
        if key != KeyCode::Enter {
            return None;
        }
        if self.permanent.confirm {
            return self.permanent.confirm_yes.then_some(Operation::Apply);
        }
        if self.focus == 0 {
            return None;
        }
        if !self.permanent_can_apply() {
            return self.permanent_available().then_some(Operation::Inspect);
        }
        self.permanent.replace = self.focus == 2;
        if matches!(
            self.permanent.result.as_ref().and_then(|r| r.plan.as_ref()),
            Some(Plan::Rotate { .. })
        ) {
            self.permanent.confirm = true;
            self.permanent.confirm_yes = false;
            return None;
        }
        Some(Operation::Apply)
    }
    pub fn begin_permanent(&mut self, operation: Operation) -> Option<u64> {
        if self.permanent.running != 0 || self.selected_step != 6 || self.help_open || self.report_open {
            return None;
        }
        if !self.permanent_available() {
            self.message = ORG_REQUIRED.into();
            return None;
        }
        if operation == Operation::Apply && !self.permanent_can_apply() {
            return None;
        }
        if operation == Operation::Apply
            && matches!(
                self.permanent.result.as_ref().and_then(|r| r.plan.as_ref()),
                Some(Plan::Rotate { .. })
            )
            && !(self.permanent.confirm && self.permanent.confirm_yes)
        {
            return None;
        }
        self.permanent.confirm = false;
        self.permanent.running = next_test();
        self.permanent.cancel_requested = false;
        self.permanent.operation = Some(operation);
        self.permanent.target = self.target.clone();
        self.permanent.organization = self.bootstrap.organization.clone();
        self.permanent.tunnel = self.tunnel.id;
        self.message.clear();
        Some(self.permanent.running)
    }
    pub(super) fn permanent_input(&self, id: u64) -> Option<Input> {
        if id == 0 || self.permanent.running != id || !self.permanent_available() {
            return None;
        }
        let destination = self.permanent_destination();
        let legacy = self.permanent_legacy_destination();
        let mut trusted = vec![legacy.clone(), destination.with_file_name("pangolin.json")];
        if let Some(config) = self.path.parent().and_then(|p| p.parent()) {
            trusted.push(config.join("secrets/pangolin.json"));
        }
        Some(Input {
            target: self.target.clone(),
            org: self.bootstrap.organization.clone(),
            root: self.bootstrap.key.duplicate(),
            trusted,
            legacy,
            replace: self.permanent.replace,
            destination,
            operation: self.permanent.operation?,
            plan: self.permanent.result.as_ref().and_then(|r| r.plan.clone()),
        })
    }
    pub fn complete_permanent(&mut self, id: u64, mut result: Outcome) {
        if id == 0 || self.permanent.running != id {
            return;
        }
        self.permanent.running = 0;
        if !self.permanent_current() || !self.permanent_available() {
            result.verified = false;
            result.plan = None;
            result.error = Some("Connection changed. Reconnect and Refresh; recovery evidence is retained.".into());
        }
        self.permanent.confirm = false;
        self.permanent.result = Some(result);
        self.permanent.replace = false;
        if self.focus > 0 {
            self.focus = 1;
        }
    }
    pub fn permanent_key(&mut self, key: KeyEvent) -> bool {
        if self.permanent.confirm {
            match key.code {
                KeyCode::Esc => self.permanent.confirm = false,
                KeyCode::Tab | KeyCode::BackTab | KeyCode::Left | KeyCode::Right | KeyCode::Up | KeyCode::Down => {
                    self.permanent.confirm_yes = !self.permanent.confirm_yes
                }
                KeyCode::Enter if !self.permanent.confirm_yes => self.permanent.confirm = false,
                KeyCode::F(7) => return false,
                _ => {}
            }
            return true;
        }
        if self.permanent.running != 0 {
            match key.code {
                KeyCode::Esc => self.permanent.cancel_requested = true,
                KeyCode::F(4) | KeyCode::F(7) => return false,
                _ => {}
            }
            return true;
        }
        let controls = self.permanent_controls();
        let at = controls.iter().position(|v| *v == self.focus).unwrap_or(0);
        match key.code {
            KeyCode::Tab => self.focus = controls[(at + 1) % controls.len()],
            KeyCode::BackTab => self.focus = controls[(at + controls.len() - 1) % controls.len()],
            KeyCode::Left | KeyCode::Right | KeyCode::Down | KeyCode::Up if self.focus > 0 => {
                self.focus = if controls.contains(&2) && self.focus == 1 { 2 } else { 1 };
            }
            KeyCode::Enter | KeyCode::Right if self.focus == 0 => self.focus = controls.get(1).copied().unwrap_or(1),
            KeyCode::Esc if self.focus > 0 => self.focus = 0,
            KeyCode::F(5) => {}
            _ => return false,
        }
        self.permanent.replace = self.focus == 2;
        true
    }
    pub fn permanent_report(&self) -> String {
        let mut lines = vec![
            format!(
                "Status: {}",
                if self.permanent_verified() {
                    "Verified"
                } else {
                    "Incomplete; previous evidence does not authorize use"
                }
            ),
            format!(
                "Organization: {}\nServer: {}\nTunnel: {}\nKey: homestack\nScope: organization / non-Root\nCanonical destination: {}",
                self.bootstrap.organization,
                self.target,
                self.tunnel.current.state.label(),
                self.permanent_destination().display()
            ),
        ];
        if let Some(out) = &self.permanent.result {
            lines.push(format!("Key IDs: {}", out.ids.join(", ")));
            if let Some(error) = &out.error {
                lines.push(error.clone());
            }
            if !out.ids.is_empty() {
                lines.push(format!(
                    "Missing actions: {}\nExtra actions: {}",
                    out.missing().join(", "),
                    out.extra().join(", ")
                ));
            }
            if let Some(record) = &out.record {
                lines.push(format!("Created by this wizard: {}", record.created_here));
                if let Some(id) = &record.rotation_from {
                    lines.push(format!("Rotation original key ID: {id}"));
                }
                lines.push(format!("Recovery stage: {:?}\nKnown key ID: {}\nCredential: {}\nDurably saved: {}\nRequired actions verified: {}\nAt: {}", record.stage, record.key_id.as_deref().unwrap_or("Not returned"), record.credential_path.display(), record.credential_saved, record.actions_verified, record.at));
            }
            lines.push(format!("Independent Organization/Sites/Resources/domains reads: {}\nAdmin listing + action administration + other-org access: {}", if out.reads { "PASS" } else { "Not verified" }, if out.negatives { "DENIED (expected)" } else { "Not verified" }));
        }
        lines.push("Required action profile (Merge preserves extra actions):".into());
        lines.extend(ACTIONS.into_iter().map(|a| format!("  {a}")));
        lines.push("Writes to Sites, Resources, Targets and policies were NOT executed. Write capabilities are confirmed only through assigned action metadata. Non-Root scope follows the pinned API's filtered organization listing and independent negative checks. No Root key is revoked.".into());
        lines.join("\n")
    }
}
