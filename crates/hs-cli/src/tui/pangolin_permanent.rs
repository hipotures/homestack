//! Step 7: explicit organization-key plans and durable, conservative recovery.
use super::pangolin::{Wizard, next_test};
use super::pangolin_key::{self as bootstrap, Client, Credential, Probe};
use super::pangolin_store::Storage;
use super::pangolin_tunnel::Endpoint;
use crossterm::event::{KeyCode, KeyEvent, KeyModifiers};
use serde::{Deserialize, Serialize};
use std::collections::BTreeSet;
use std::path::PathBuf;
use std::sync::atomic::AtomicBool;
use std::time::Instant;
use zeroize::Zeroizing;

pub const ACTIONS: [&str; 25] = [
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
}
#[derive(Debug, Clone, PartialEq)]
pub enum Plan {
    Create,
    Replace { id: String, before: BTreeSet<String> },
    AssignCreated { id: String, before: BTreeSet<String> },
}
#[derive(Debug, Clone, Copy, PartialEq)]
pub enum Operation {
    Inspect,
    Verify,
    Prepare,
    Apply,
}
#[derive(Debug, Clone, PartialEq, Default)]
pub struct View {
    pub running: u64,
    pub cancel_requested: bool,
    pub confirm: bool,
    pub confirm_yes: bool,
    pub pasted: bool,
    pub import: bool,
    pub input: String,
    pub credential: Credential,
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
    pub prepared: bool,
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
    credential: Credential,
    source: Option<PathBuf>,
    import: bool,
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
fn work(endpoint: &Endpoint, input: Input, stop: &AtomicBool, out: &mut Outcome) -> Result<()> {
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
    let mut record = storage.load::<Record>()?.unwrap_or(Record {
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
    if matches!(record.stage, Stage::CreationRequested | Stage::Created) {
        // A crash may follow durable secret persistence but precede its metadata update.
        // Recover only by the actual local token ID, never by a name-only guess.
        let recovered = storage.recover_credential(&input.destination).map_err(|_| "Outcome uncertain. Creation was requested and no safely persisted credential is available. Pangolin cannot return its original secret. Never retry creation; resolve recovery separately.")?;
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
    if matches!(input.operation, Operation::Inspect | Operation::Verify) && out.ids.is_empty() {
        if !matches!(record.stage, Stage::NoMutation | Stage::Prepared) {
            return Err("Unresolved recovery prevents creation.".into());
        }
        out.plan = Some(Plan::Create);
        return Ok(());
    }
    if input.operation == Operation::Prepare && out.ids.is_empty() && input.plan == Some(Plan::Create) {
        if !matches!(record.stage, Stage::NoMutation | Stage::Prepared) {
            return Err("Unresolved recovery prevents creation.".into());
        }
        storage.release()?;
        storage.reserve()?;
        save(&storage, &mut record, Stage::Prepared)?;
        out.record = Some(record);
        out.plan = Some(Plan::Create);
        out.prepared = true;
        return Ok(());
    }
    let creating = input.operation == Operation::Apply && input.plan == Some(Plan::Create);
    let credential;
    if creating {
        if !out.ids.is_empty() {
            return Err("A homestack key appeared before creation. Creation refused.".into());
        }
        if record.stage != Stage::Prepared || !storage.has_pending() {
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
        if storage.persist(&credential).is_err() {
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
        credential = if !input.credential.is_empty() {
            input.credential.duplicate()
        } else if let Some(source) = &input.source {
            super::pangolin_store::read_credential(source)?
        } else if record.credential_saved
            || storage.has_credential()
            || storage.has_pending()
            || matches!(
                record.stage,
                Stage::Created | Stage::CredentialSaved | Stage::ActionsVerified | Stage::Complete
            )
        {
            if record.credential_path == input.destination {
                storage.recover_credential(&input.destination).map_err(|_| "Existing HomeStack credential is not safely available. Pangolin cannot return the original secret. Resolve recovery separately; no duplicate will be created.")?
            } else {
                super::pangolin_store::read_credential(&record.credential_path)?
            }
        } else {
            return Err("An existing HomeStack key was found, but its credential is not available. Pangolin cannot return its original secret. Provide the existing credential to continue without creating a duplicate.".into());
        };
        if credential.key_id().map_err(|_| "Invalid ID.secret credential.")? != id {
            return Err("Credential key ID does not match the existing organization key. Adoption refused.".into());
        }
    }
    let resuming_creation = !creating
        && record.created_here
        && record.credential_saved
        && matches!(record.stage, Stage::Created | Stage::CredentialSaved)
        && record.credential_path == input.destination
        && input.source.is_none()
        && input.credential.is_empty();
    let id = credential.key_id().map_err(|_| "Invalid credential.")?.to_owned();
    let permanent = Client {
        endpoint,
        credential: &credential,
        stop,
        started: Instant::now(),
    };
    // Newly created keys have no actions until the approved assignment below.
    if !creating {
        // Imported keys must authenticate before any replacement plan. An owned creation
        // may still have zero actions: its durable token and creation provenance permit
        // an explicitly reviewed continuation, followed by full independent verification.
        if !resuming_creation {
            org(&permanent, &input.org)?;
        }
        let path = input.source.clone().filter(|_| !input.import).unwrap_or_else(|| {
            if input.credential.is_empty() && input.source.is_none() {
                record.credential_path.clone()
            } else {
                input.destination.clone()
            }
        });
        if path == input.destination {
            if storage.has_credential() || storage.has_pending() {
                let stored = storage.recover_credential(&path)?;
                if stored.value != credential.value {
                    return Err(
                        "Canonical credential already exists with different content; it will never be overwritten."
                            .into(),
                    );
                }
            } else {
                storage.reserve()?;
                storage.persist(&credential)?;
            }
        }
        record.key_id = Some(id.clone());
        record.credential_path = path;
        record.credential_saved = true;
        // Preserve the previous action evidence only until this run reads it again.
        record.actions_verified = false;
        save(&storage, &mut record, Stage::CredentialSaved)?;
        out.record = Some(record.clone());
    }
    let differs = out.assigned != expected();
    let replace = if resuming_creation {
        Plan::AssignCreated {
            id: id.clone(),
            before: out.assigned.clone(),
        }
    } else {
        Plan::Replace {
            id: id.clone(),
            before: out.assigned.clone(),
        }
    };
    if !creating && differs {
        out.plan = Some(replace.clone());
        if input.operation == Operation::Prepare {
            if input.plan != Some(replace.clone()) {
                return Err("Permissions changed. Review the refreshed additions/removals before approving.".into());
            }
            out.prepared = true;
            return Ok(());
        }
        if input.operation != Operation::Apply || input.plan != Some(replace) {
            return Err(if resuming_creation {
                "Created credential retained. Actions are pending; review the continuation plan in F7 and approve with F5. No new key will be created."
            } else { "Existing credential validated. Permissions differ; review missing and extra actions in F7. Replacement requires separate approval." }.into());
        }
    }
    if creating || (differs && input.operation == Operation::Apply) {
        if keys(&root, &input.org)? != vec![id.clone()] {
            return Err("Key conflict changed; permission replacement refused.".into());
        }
        if !creating && actions(&root, &input.org, &id)? != out.assigned {
            return Err("Actions changed immediately before replacement; review again.".into());
        }
        org(&root, &input.org)?;
        let body = serde_json::to_string(&serde_json::json!({"actionIds": ACTIONS}))
            .map_err(|_| "Cannot encode approved actions.")?;
        if api(root.request("POST", &action_path(&input.org, &id), Some(&body), Probe::Actions))?.0 != 200 {
            return Err(
                "Action assignment rejected. Check compatibility with the pinned 25-action profile. Credential is retained; recheck before applying another reviewed plan."
                    .into(),
            );
        }
    }
    out.assigned = actions(&root, &input.org, &id)?;
    if out.assigned != expected() {
        return Err("Assigned actions differ from the exact approved 25-action profile.".into());
    }
    if keys(&root, &input.org)? != vec![id.clone()] {
        return Err("Cannot confirm a unique organization-scoped, non-Root homestack key.".into());
    }
    record.actions_verified = true;
    save(&storage, &mut record, Stage::ActionsVerified)?;
    out.record = Some(record.clone());
    reads(&permanent, &input.org)?;
    out.reads = true;
    negatives(&permanent, &input.org, &id)?;
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
    pub fn permanent_destination(&self) -> PathBuf {
        use sha2::{Digest, Sha256};
        let identity = format!("{}\0{}", self.target, self.bootstrap.organization);
        let digest: String = Sha256::digest(identity.as_bytes())
            .iter()
            .map(|byte| format!("{byte:02x}"))
            .collect();
        self.path
            .parent()
            .unwrap_or(std::path::Path::new("/"))
            .join("secrets/pangolin")
            .join(digest)
            .join("homestack.key")
    }
    pub fn permanent_controls(&self) -> Vec<usize> {
        let mut controls = vec![0];
        if self.permanent.result.as_ref().is_some_and(|r| r.ids.len() == 1) {
            controls.extend([1, 2, 3]);
        }
        controls.extend([4, 5]);
        if self.permanent.result.as_ref().is_some_and(|r| r.ids.len() == 1) {
            controls.push(6);
        }
        if self.permanent_can_apply() {
            controls.push(7);
        }
        controls
    }
    pub fn permanent_can_apply(&self) -> bool {
        self.permanent_available()
            && self.permanent_current()
            && self.permanent.running == 0
            && self.permanent.result.as_ref().is_some_and(|r| r.plan.is_some())
    }
    pub fn begin_permanent(&mut self, operation: Operation) -> Option<u64> {
        if self.permanent.running != 0 || self.selected_step != 6 || self.help_open || self.report_open {
            return None;
        }
        if !self.permanent_available() {
            self.message = ORG_REQUIRED.into();
            return None;
        }
        if matches!(operation, Operation::Prepare | Operation::Apply) && !self.permanent_can_apply() {
            return None;
        }
        if operation == Operation::Apply
            && (!self.permanent.confirm
                || !self.permanent.confirm_yes
                || !self.permanent.result.as_ref().is_some_and(|r| r.prepared))
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
        Some(Input {
            target: self.target.clone(),
            org: self.bootstrap.organization.clone(),
            root: self.bootstrap.key.duplicate(),
            credential: self.permanent.credential.duplicate(),
            source: (!self.permanent.pasted && !self.permanent.input.is_empty())
                .then(|| PathBuf::from(&self.permanent.input)),
            import: self.permanent.import,
            destination: self.permanent_destination(),
            operation: self.permanent.operation?,
            plan: self.permanent.result.as_ref().and_then(|r| r.plan.clone()),
        })
    }
    pub fn complete_permanent(&mut self, id: u64, mut result: Outcome) {
        if id == 0 || self.permanent.running != id {
            return;
        }
        self.permanent.running = 0;
        self.permanent.credential.clear();
        if !self.permanent_current() || !self.permanent_available() {
            result.verified = false;
            result.prepared = false;
            result.plan = None;
            result.error =
                Some("Tunnel or identity changed. Reconnect and inspect; durable recovery state is retained.".into());
        }
        self.permanent.confirm = result.prepared && result.error.is_none();
        self.permanent.confirm_yes = false;
        if self.permanent.confirm {
            self.report_scroll = 0;
        }
        self.permanent.result = Some(result);
    }
    pub fn permanent_paste(&mut self, text: &str) {
        if self.permanent.running != 0 || self.permanent.confirm || self.focus != 2 {
            return;
        }
        if self.permanent.pasted {
            self.permanent.credential.insert(text);
        } else {
            let capacity = 4096usize.saturating_sub(self.permanent.input.len());
            self.permanent
                .input
                .extend(text.chars().filter(|c| !c.is_control()).take(capacity));
        }
        // Editing never preserves a previous mutation approval.
        if let Some(result) = &mut self.permanent.result {
            result.plan = None;
            result.prepared = false;
            result.verified = false;
        }
    }
    pub fn permanent_key(&mut self, key: KeyEvent) -> bool {
        if self.permanent.confirm {
            match key.code {
                KeyCode::Esc => self.permanent.confirm = false,
                KeyCode::Tab | KeyCode::BackTab | KeyCode::Left | KeyCode::Right | KeyCode::Char(' ') => {
                    self.permanent.confirm_yes = !self.permanent.confirm_yes
                }
                KeyCode::Enter if !self.permanent.confirm_yes => self.permanent.confirm = false,
                KeyCode::PageDown | KeyCode::Down => self.report_scroll = self.report_scroll.saturating_add(3),
                KeyCode::PageUp | KeyCode::Up => self.report_scroll = self.report_scroll.saturating_sub(3),
                _ => {}
            }
            return true;
        }
        if self.permanent.running != 0 {
            if key.code == KeyCode::Esc {
                self.permanent.cancel_requested = true;
                self.message = "Cancellation requested. Outcome uncertain if a remote mutation was sent; waiting for durable recovery evidence.".into();
            }
            return true;
        }
        let controls = self.permanent_controls();
        let at = controls.iter().position(|v| *v == self.focus).unwrap_or(0);
        match key.code {
            KeyCode::Down | KeyCode::Tab if key.code == KeyCode::Tab || self.focus > 0 => {
                self.focus = controls[(at + 1) % controls.len()]
            }
            KeyCode::Up | KeyCode::BackTab if key.code == KeyCode::BackTab || self.focus > 0 => {
                self.focus = controls[(at + controls.len() - 1) % controls.len()]
            }
            KeyCode::Enter | KeyCode::Right if self.focus == 0 => self.focus = controls.get(1).copied().unwrap_or(0),
            KeyCode::Enter | KeyCode::Char(' ') if self.focus == 1 => {
                self.permanent.pasted = !self.permanent.pasted;
                self.permanent.credential.clear();
                self.permanent.input.clear();
                if let Some(r) = &mut self.permanent.result {
                    r.verified = false;
                    r.plan = None;
                    r.prepared = false;
                }
            }
            KeyCode::Enter | KeyCode::Char(' ') if self.focus == 3 => {
                self.permanent.import = !self.permanent.import;
                if let Some(r) = &mut self.permanent.result {
                    r.verified = false;
                    r.plan = None;
                    r.prepared = false;
                }
            }
            KeyCode::Enter if self.focus == 4 => {
                self.report_open = true;
                self.report_scroll = 0;
            }
            KeyCode::Backspace if self.focus == 2 => {
                if self.permanent.pasted {
                    self.permanent.credential.pop();
                } else {
                    self.permanent.input.pop();
                }
                if let Some(r) = &mut self.permanent.result {
                    r.verified = false;
                    r.plan = None;
                    r.prepared = false;
                }
            }
            KeyCode::Delete if self.focus == 2 => {
                self.permanent.credential.clear();
                self.permanent.input.clear();
                if let Some(r) = &mut self.permanent.result {
                    r.verified = false;
                    r.plan = None;
                    r.prepared = false;
                }
            }
            KeyCode::Char('u') if self.focus == 2 && key.modifiers.contains(KeyModifiers::CONTROL) => {
                self.permanent.credential.clear();
                self.permanent.input.clear();
                if let Some(r) = &mut self.permanent.result {
                    r.verified = false;
                    r.plan = None;
                    r.prepared = false;
                }
            }
            KeyCode::Char(c)
                if self.focus == 2 && !key.modifiers.intersects(KeyModifiers::CONTROL | KeyModifiers::ALT) =>
            {
                self.permanent_paste(&c.to_string())
            }
            _ => return false,
        }
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
                lines.push(format!("Recovery stage: {:?}\nKnown key ID: {}\nCredential: {}\nDurably saved: {}\nExact actions verified: {}\nAt: {}", record.stage, record.key_id.as_deref().unwrap_or("Not returned"), record.credential_path.display(), record.credential_saved, record.actions_verified, record.at));
            }
            lines.push(format!("Independent Organization/Sites/Resources/domains reads: {}\nAdmin listing + action administration + other-org access: {}", if out.reads { "PASS" } else { "Not verified" }, if out.negatives { "DENIED (expected)" } else { "Not verified" }));
        }
        lines.push("Approved action profile (exact equality required):".into());
        lines.extend(ACTIONS.into_iter().map(|a| format!("  {a}")));
        lines.push("Writes to Sites, Resources, Targets and policies were NOT executed. Write capabilities are confirmed only through assigned action metadata. Non-Root scope follows the pinned API's filtered organization listing and independent negative checks. No Root key is revoked.".into());
        lines.join("\n")
    }
}
