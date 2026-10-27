//! Concrete, sequential scenarios against public executables and HTTPS endpoints.

use std::path::{Path, PathBuf};
use std::process::Stdio;
use std::time::Duration;

use serde_json::{Value, json};
use sha2::{Digest, Sha256};
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::process::Command;

use crate::{Options, Result, phases, save_report, service};

/// Workers start their own process group. Cancellation must also stop the
/// fixture's local server, SSH tunnel and terminal child before lab removal.
struct WorkerGroup(Option<u32>);

impl Drop for WorkerGroup {
    fn drop(&mut self) {
        if let Some(pid) = self.0 {
            let _ = std::process::Command::new("kill")
                .args(["-KILL", "--", &format!("-{pid}")])
                .status();
        }
    }
}

#[derive(Clone, Copy)]
enum Action {
    Confirmation,
    Install,
    Permissions,
    Idempotency,
    Reads,
    Refusals,
    Workspace(&'static str),
    Legacy(&'static str),
    Cluster,
    InstanceRecovery,
    Reinstall,
    Tui,
    NodeRecovery,
    Gap(&'static str),
}

struct Scenario {
    id: String,
    description: &'static str,
    nodes: u8, // 0 means both topologies.
    baseline: &'static str,
    fixtures: &'static str,
    timeout: u64,
    smoke: bool,
    action: Action,
    verification: &'static str,
    cleanup: &'static str,
}

fn registry() -> Vec<Scenario> {
    let mut cases = Vec::new();
    let mut add = |id: &str, description, nodes, baseline, fixtures, timeout, smoke, action, verification, cleanup| {
        cases.push(Scenario {
            id: id.into(),
            description,
            nodes,
            baseline,
            fixtures,
            timeout,
            smoke,
            action,
            verification,
            cleanup,
        });
    };
    add(
        "installation.confirmation",
        "Refuse an unconfirmed noninteractive installation without mutation",
        0,
        "infrastructure",
        "prepared installer inputs",
        180,
        true,
        Action::Confirmation,
        "exit 3 and confirmation_required; unchanged PVE users, roles, ACLs and guests",
        "none",
    );
    add(
        "installation.complete",
        "Install the real API containers and client",
        0,
        "infrastructure",
        "Gold 100; explicit image and expected digest; cached wildcard TLS",
        2400,
        true,
        Action::Install,
        "HTTPS readiness; PVE CT configuration, pool and placement; installed client status",
        "suite uninstall",
    );
    add(
        "authorization.effective",
        "Audit user and separated runtime/deploy token rights",
        0,
        "installed",
        "actual Gold, workspace and API CT paths; foreign VM",
        300,
        true,
        Action::Permissions,
        "pveum effective permissions and real forbidden PVE operation",
        "owned foreign VM",
    );
    add(
        "installation.idempotency",
        "Repeat the Proxmox installation without changing ACLs or tokens",
        0,
        "installed",
        "installed roles, ACLs, token IDs and snippets",
        300,
        false,
        Action::Idempotency,
        "independent PVE snapshots; zero installer permission steps",
        "none",
    );
    add(
        "api.reads",
        "Read configuration, status, storage, discovery and Gold readiness",
        0,
        "installed",
        "Gold 100 and all API instances",
        180,
        true,
        Action::Reads,
        "compare API and PVE node inventories; Gold configuration and readiness",
        "none",
    );
    add(
        "api.refusals",
        "Reject unauthenticated, invalid and foreign-resource operations",
        0,
        "installed",
        "foreign VM; PVE and job snapshots",
        180,
        true,
        Action::Refusals,
        "exact refusal classes; unchanged PVE config, storage and jobs",
        "owned foreign VM",
    );
    for (id, description, kind) in [
        (
            "workspace.lifecycle",
            "Create, boot, observe, power off/on and destroy",
            "lifecycle",
        ),
        (
            "workspace.refresh",
            "Replace root while retaining persistent home identity and content",
            "refresh",
        ),
        (
            "workspace.addkey",
            "Add a synthetic SSH key and verify guest authorization",
            "addkey",
        ),
        (
            "workspace.stale-plan",
            "Reject a changed plan without modifying disks",
            "stale",
        ),
        (
            "workspace.setup-files",
            "Provision a synthetic desktop configuration file through the public setup CLI",
            "setup",
        ),
    ] {
        add(
            id,
            description,
            0,
            "installed",
            "new run-owned workspace 242; synthetic guest SSH key",
            1200,
            kind == "lifecycle",
            Action::Workspace(kind),
            "PVE config/volume/power state and guest filesystem/SSH observations",
            "API destroy with PVE absence and volume checks",
        );
    }
    add(
        "workspace.capacity",
        "Refuse allocation before and after storage capacity changes",
        0,
        "gold",
        "existing capacity fixture, owned tmpfs storage",
        1200,
        false,
        Action::Legacy("capacity"),
        "existing independent VM, storage and no-job assertions",
        "existing guarded capacity cleanup",
    );
    add(
        "workspace.migrate",
        "Running/stopped migrations, persistence and target capacity refusal",
        3,
        "gold",
        "existing migration fixture; owned tiny target storage",
        2400,
        false,
        Action::Legacy("migration"),
        "PVE node/power state, guest markers, target storage and task results",
        "existing guarded migration cleanup",
    );
    add(
        "cluster.inventory",
        "Compare three-node inventory, membership and quorum",
        3,
        "infrastructure",
        "owned three-node lab",
        120,
        true,
        Action::Cluster,
        "pvesh cluster/status and per-node pveversion",
        "none",
    );
    add(
        "service.instance-recovery",
        "Stop one API instance and verify peer availability and recovery",
        3,
        "installed",
        "three installed API containers",
        300,
        false,
        Action::InstanceRecovery,
        "PVE stopped/running state and authenticated HTTPS on surviving peers",
        "restart the same owned CT even after failure",
    );
    add(
        "installation.cancel",
        "Open and cancel the real TUI installer without mutation",
        0,
        "installed",
        "PTY and isolated client profile",
        180,
        false,
        Action::Tui,
        "actual terminal output and unchanged PVE authorization, guests and jobs",
        "terminate only the owned TUI process",
    );
    add(
        "cluster.node-recovery",
        "Suspend one owned Proxmox node, wait for surviving quorum, then verify recovery",
        3,
        "installed",
        "owned pve3 QEMU process",
        600,
        false,
        Action::NodeRecovery,
        "PVE reports pve3 offline with remaining quorum, then all nodes online",
        "resume the guarded QEMU process; verify stable three-node quorum and API readiness even on failure",
    );
    add(
        "installation.reinstall",
        "Remove API containers and authorization, then install again",
        0,
        "installed",
        "installed baseline with no scenario guests",
        3000,
        false,
        Action::Reinstall,
        "PVE absence of CTs/user/tokens; fresh HTTPS and client readiness",
        "suite uninstall",
    );
    for (id, reason, nodes) in [
        (
            "service.schema3",
            "Schema-3 service deployment/reconciliation and image replacement require the TUI service driver; follow docs/e2e-schema3.md in an isolated lab",
            0,
        ),
        (
            "service.manifest",
            "Manifest deployment/reconciliation is currently TUI-only; automated TUI driver is not implemented",
            0,
        ),
        (
            "service.resources",
            "Manifest CPU/memory/root/home resource changes require the missing TUI driver",
            0,
        ),
        (
            "service.image-persistence",
            "Manifest image replacement and persistent /home/app observations require a purpose-built service image and TUI driver",
            0,
        ),
        (
            "service.migrate",
            "Service migration is currently a TUI workflow; automated TUI driver is not implemented",
            3,
        ),
    ] {
        add(
            id,
            reason,
            nodes,
            "installed",
            "missing functional fixture/driver",
            60,
            false,
            Action::Gap(reason),
            "not implemented; never reported as passed",
            "none",
        );
    }
    for role in ["home", "root"] {
        for layout in [
            "ext4-whole",
            "ext4-partition",
            "lvm",
            "luks-key-lvm",
            "luks-pass-lvm",
            "btrfs-subvolume",
            "btrfs-luks",
            "btrfs-multi",
            "lvm-multi",
            "partition-not-last",
        ] {
            cases.push(Scenario {
                id: format!("storage.resize.{role}.{layout}"), description: "Existing resize matrix: growth/refusal, same-size recovery and persistence", nodes: 0,
                baseline: "gold", fixtures: "Gold 100 and existing run-owned resize base; independently scoped local server", timeout: 2400, smoke: false,
                action: Action::Legacy("resize"), verification: "existing guest filesystem size/UUID/marker/ownership assertions; PVE volumes; interruption barrier", cleanup: "existing guarded resize cleanup",
            });
        }
    }
    cases
}

fn selected(options: &Options) -> Vec<Scenario> {
    registry()
        .into_iter()
        .filter(|case| {
            if let Some(id) = &options.scenario {
                case.id == *id
                    || matches!(
                        case.action,
                        Action::Confirmation | Action::Install | Action::Permissions
                    )
            } else {
                options
                    .suite
                    .as_deref()
                    .is_some_and(|suite| suite == "full" || case.smoke)
            }
        })
        .collect()
}

pub fn validate(options: &Options) -> Result<()> {
    if options.suite.as_deref().is_some_and(|s| !matches!(s, "smoke" | "full")) {
        return Err("--suite must be smoke or full".into());
    }
    if options.suite.is_some() && options.scenario.is_some() {
        return Err("choose --suite or --scenario".into());
    }
    if options
        .scenario
        .as_ref()
        .is_some_and(|id| !registry().iter().any(|case| case.id == *id))
    {
        return Err("unknown scenario; use list-scenarios".into());
    }
    if let Some(digest) = &options.server_image_digest
        && !digest
            .strip_prefix("sha256:")
            .is_some_and(|hex| hex.len() == 64 && hex.bytes().all(|b| b.is_ascii_hexdigit()))
    {
        return Err("--server-image-digest must be sha256 followed by 64 hexadecimal digits".into());
    }
    Ok(())
}

pub fn list() -> Result<()> {
    for case in registry() {
        println!(
            "{}\t{}\t{}\t{}s\t{}",
            case.id,
            if case.nodes == 3 { "three" } else { "both" },
            if case.smoke { "smoke,full" } else { "full" },
            case.timeout,
            case.description
        );
    }
    Ok(())
}

pub fn initialize(options: &Options, report: &mut Value) -> Result<()> {
    if options.suite.is_none() && options.scenario.is_none() {
        return Ok(());
    }
    let source = Path::new(env!("CARGO_MANIFEST_DIR")).join("../..");
    let git = std::process::Command::new("git")
        .arg("-C")
        .arg(&source)
        .args(["rev-parse", "HEAD"])
        .output()?;
    if !git.status.success() {
        return Err("cannot identify tested Git revision".into());
    }
    report["tested_commit"] = json!(String::from_utf8(git.stdout)?.trim());
    report["working_tree_dirty"] = json!(
        !std::process::Command::new("git")
            .arg("-C")
            .arg(&source)
            .args(["status", "--porcelain"])
            .output()?
            .stdout
            .is_empty()
    );
    let mut versions = serde_json::Map::new();
    for (program, flag) in [
        ("rustc", "--version"),
        ("python3", "--version"),
        ("ssh", "-V"),
        ("qemu-system-x86_64", "--version"),
        ("uname", "-sr"),
    ] {
        let version = match std::process::Command::new(program).arg(flag).output() {
            Ok(output) => String::from_utf8_lossy(if output.stdout.is_empty() {
                &output.stderr
            } else {
                &output.stdout
            })
            .lines()
            .next()
            .unwrap_or("")
            .to_owned(),
            Err(error) => format!("unavailable: {}", error.kind()),
        };
        versions.insert(program.into(), json!(version));
    }
    report["environment_versions"] = json!(versions);
    report["suite"] = json!(options.suite.as_deref().unwrap_or("selected"));
    report["server_image"] = json!({"reference": options.server_image, "expected_digest": options.server_image_digest});
    report["scenarios"] = json!(
        selected(options)
            .iter()
            .map(|case| json!({
                "id": case.id, "description": case.description, "topology": options.nodes,
                "status": if case.nodes != 0 && case.nodes != options.nodes { "OUT_OF_SCOPE" } else { "PENDING" },
                "duration_seconds": 0, "preconditions": case.baseline, "fixtures": case.fixtures,
                "timeout_seconds": case.timeout, "expected": case.verification, "cleanup_contract": case.cleanup,
                "observed": [], "failed_step": null, "error_category": null, "error": null,
                "cleanup": {"status": "NOT_REQUIRED"},
            }))
            .collect::<Vec<_>>()
    );
    report["product_installation"] = json!({"status":"PENDING"});
    report["uninstall"] = json!({"status":"PENDING"});
    Ok(())
}

pub fn finish_pending(report: &mut Value, reason: &str) {
    if let Some(cases) = report["scenarios"].as_array_mut() {
        for case in cases {
            if case["status"] == "RUNNING" {
                if let Some(started) = case["started_at_ms"].as_u64() {
                    case["duration_seconds"] =
                        json!(phases::now_ms().saturating_sub(u128::from(started)) as f64 / 1000.0);
                }
                case["status"] = json!("FAILED");
                case["error_category"] = json!("interrupted");
                case["error"] = json!("execution interrupted before result finalization; see suite error");
                case["cleanup"] = json!({"status":"UNCONFIRMED"});
            } else if case["status"] == "PENDING" {
                case["status"] = json!("BLOCKED");
                case["error_category"] = json!("baseline_unavailable");
                case["error"] = json!(reason);
            }
        }
    }
    for phase in ["product_installation", "uninstall"] {
        if report[phase]["status"] == "PENDING" {
            report[phase] = json!({"status":"BLOCKED", "error":reason});
        }
    }
}

pub fn write_summary(dir: &Path, report: &Value) -> Result<()> {
    let Some(cases) = report["scenarios"].as_array() else {
        return Ok(());
    };
    let counts: serde_json::Map<String, Value> = ["PASSED", "FAILED", "BLOCKED", "OUT_OF_SCOPE"]
        .into_iter()
        .map(|status| {
            (
                status.into(),
                json!(cases.iter().filter(|c| c["status"] == status).count()),
            )
        })
        .collect();
    let mut machine = report.clone();
    machine["scenario_summary"] = json!(counts);
    let temp = dir.join("scenarios.json.tmp");
    std::fs::write(&temp, serde_json::to_vec_pretty(&machine)?)?;
    std::fs::rename(temp, dir.join("scenarios.json"))?;
    let mut summary = format!(
        "HomeStack E2E: {} node(s), commit {}\nSuite: {}\n",
        report["resources"]["nodes"], report["tested_commit"], report["suite"]
    );
    for (status, count) in counts {
        summary.push_str(&format!("{status}: {count}\n"));
    }
    for case in cases {
        summary.push_str(&format!(
            "{}  {} ({}) cleanup={} {}\n",
            case["status"].as_str().unwrap_or("?"),
            case["id"].as_str().unwrap_or("?"),
            case["error"].as_str().unwrap_or(""),
            case["cleanup"]["status"].as_str().unwrap_or("?"),
            case["cleanup"]["error"].as_str().unwrap_or("")
        ));
    }
    summary.push_str(&format!(
        "Installation: {}\nUninstall: {}\nInfrastructure cleanup: {}\n",
        report["product_installation"]["status"], report["uninstall"]["status"], report["cleanup"]
    ));
    std::fs::write(dir.join("summary.txt"), summary)?;
    Ok(())
}

/// Preflight may fail before DNS credentials or a VM directory can be opened.
pub fn early_failure(error: &str) -> Result<()> {
    let args: Vec<String> = std::env::args().skip(1).collect();
    if args.first().map(String::as_str) != Some("run") {
        return Ok(());
    }
    let run_id = uuid::Uuid::new_v4().to_string();
    let Ok(options) = Options::parse(&args, &run_id) else {
        return Ok(());
    };
    if options.suite.is_none() && options.scenario.is_none() {
        return Ok(());
    }
    // Never overwrite a previous run's evidence, including a live paused lab.
    if options.artifacts.exists() {
        return Ok(());
    }
    std::fs::create_dir_all(&options.artifacts)?;
    std::fs::set_permissions(&options.artifacts, std::os::unix::fs::PermissionsExt::from_mode(0o700))?;
    let mut report = json!({"run_id":run_id, "status":"failed", "resources":{"nodes":options.nodes},
        "phases":{"preflight":{"status":"failed", "error_category":"host_prerequisite", "error":error}}, "error":error,
        "cleanup":{"passed":true,"resources_allocated":false,"temporary_directory_removed":true}});
    initialize(&options, &mut report)?;
    finish_pending(&mut report, error);
    save_report(&options.artifacts, &report)
}

struct Context {
    dir: PathBuf,
    artifacts: PathBuf,
    report: Value,
    client: reqwest::Client,
    token: String,
    version: String,
    evidence: Vec<Value>,
    step: String,
    workspace: Option<Value>,
    foreign: bool,
    permission_vm: bool,
    restart_ct: Option<(String, u64)>,
    node_recovery_pending: bool,
}

impl Context {
    fn profile(&self) -> PathBuf {
        self.dir.join("client/config/homestack")
    }

    async fn command(&mut self, program: &str, args: &[String]) -> Result<std::process::Output> {
        self.step = format!("CLI {program} {}", args.first().map_or("", String::as_str));
        let mut command = crate::client::automation(&self.dir, program, args)?;
        let proxy = self.report["scenario_proxy"].as_str().ok_or("scenario proxy missing")?;
        for key in [
            "HTTPS_PROXY",
            "https_proxy",
            "HTTP_PROXY",
            "http_proxy",
            "ALL_PROXY",
            "all_proxy",
        ] {
            command.env(key, proxy);
        }
        command
            .env("NO_PROXY", "ghcr.io,pkg-containers.githubusercontent.com")
            .env("no_proxy", "ghcr.io,pkg-containers.githubusercontent.com");
        let output = command.stdin(Stdio::null()).output().await?;
        self.evidence.push(json!({"cli":program,"exit_code":output.status.code(),"stdout":self.redact(&String::from_utf8_lossy(&output.stdout)),"stderr":self.redact(&String::from_utf8_lossy(&output.stderr))}));
        self.flush()?;
        Ok(output)
    }

    fn redact(&self, text: &str) -> String {
        let mut text = text.to_owned();
        for file in ["api-token", "pve-token", "pve-token-deploy"] {
            if let Ok(secret) = std::fs::read_to_string(self.profile().join(file)) {
                let secret = secret.trim();
                if !secret.is_empty() {
                    text = text.replace(secret, "[REDACTED]");
                    if let Some((_, value)) = secret.split_once('=') {
                        text = text.replace(value, "[REDACTED]");
                    }
                }
            }
        }
        text
    }

    fn flush(&self) -> Result<()> {
        service::write_private(
            &self.artifacts.join("evidence.json"),
            &serde_json::to_vec_pretty(&self.evidence)?,
        )
    }

    async fn cli(&mut self, program: &str, args: &[String]) -> Result<()> {
        let output = self.command(program, args).await?;
        ensure(output.status.success(), "public CLI failed; see scenario evidence")
    }

    async fn node(&mut self, node: &str, args: &[&str]) -> Result<Vec<u8>> {
        self.step = format!("PVE observation {}", args.first().unwrap_or(&""));
        let output = self.node_command(node, args)?.output().await?;
        ensure(
            output.status.success(),
            &format!(
                "guarded PVE observation failed: {}",
                self.redact(&String::from_utf8_lossy(&output.stderr))
            ),
        )?;
        Ok(output.stdout)
    }

    fn node_command(&self, node: &str, args: &[&str]) -> Result<Command> {
        ensure(
            self.report["nodes"]
                .as_array()
                .is_some_and(|nodes| nodes.iter().any(|n| n["name"] == node)),
            "unknown E2E node",
        )?;
        let run_id = uuid::Uuid::parse_str(self.report["run_id"].as_str().ok_or("missing run identity")?)?;
        let guard = format!(
            "import json,subprocess,sys; assert json.load(open('/etc/homestack-e2e.json'))['run_id']=='{run_id}'; sys.exit(subprocess.call(sys.argv[1:]))"
        );
        let remote = [vec!["python3", "-c", &guard], args.to_vec()]
            .concat()
            .iter()
            .map(|a| phases::shell_quote(a))
            .collect::<Vec<_>>()
            .join(" ");
        let mut command = Command::new("ssh");
        command
            .arg("-F")
            .arg(self.dir.join("ssh_config"))
            .args(["-o", "BatchMode=yes", &format!("pve-e2e-{node}"), &remote])
            .stdin(Stdio::null())
            .kill_on_drop(true);
        Ok(command)
    }

    async fn pve(&mut self, args: &[&str]) -> Result<Value> {
        let args = [vec!["pvesh"], args.to_vec(), vec!["--output-format", "json"]].concat();
        let bytes = self.node("pve1", &args).await?;
        let value: Value = if bytes.is_empty() {
            Value::Null
        } else {
            serde_json::from_slice(&bytes)?
        };
        let redacted: Value = serde_json::from_str(&self.redact(&value.to_string()))?;
        self.evidence
            .push(json!({"interface":"pve-root","command":args,"response":sanitize(redacted)}));
        self.flush()?;
        Ok(value)
    }

    async fn inventory(&mut self) -> Result<Value> {
        let mut rows = self.pve(&["get", "/cluster/resources", "--type", "vm"]).await?;
        for row in rows.as_array_mut().ok_or("invalid VM inventory")? {
            row.as_object_mut().ok_or("invalid VM inventory row")?.retain(|key, _| {
                matches!(
                    key.as_str(),
                    "vmid"
                        | "name"
                        | "node"
                        | "type"
                        | "pool"
                        | "status"
                        | "template"
                        | "tags"
                        | "maxmem"
                        | "maxcpu"
                        | "maxdisk"
                )
            });
        }
        ordered_rows(rows)
    }

    async fn volume_inventory(&mut self) -> Result<Value> {
        let rows = self.pve(&["get", "/nodes/pve1/storage/local/content"]).await?;
        let mut identities = Vec::new();
        for row in rows.as_array().ok_or("invalid volume inventory")? {
            ensure(
                row["volid"].as_str().is_some_and(|id| !id.is_empty()),
                "volume identity missing",
            )?;
            identities.push(json!({"volid":row["volid"],"vmid":row["vmid"],"format":row["format"],"content":row["content"],"size":row["size"]}));
        }
        ordered_rows(json!(identities))
    }

    async fn config(&mut self, node: &str, kind: &str, vmid: u64) -> Result<Value> {
        self.pve(&["get", &format!("/nodes/{node}/{kind}/{vmid}/config")]).await
    }

    async fn api_at(
        &mut self,
        host: &str,
        method: &str,
        path: &str,
        payload: Option<Value>,
        authenticated: bool,
    ) -> Result<(u16, Value)> {
        self.step = format!("HTTPS {method} {path} on {host}");
        let mut request = self
            .client
            .request(method.parse()?, format!("https://{host}.e2e.homestack.app{path}"));
        if authenticated {
            request = request.bearer_auth(&self.token);
        }
        if !self.version.is_empty() {
            request = request.header("homestack-config-version", &self.version);
        }
        if let Some(payload) = payload {
            request = request.json(&payload);
        }
        let response = request.send().await?;
        let status = response.status().as_u16();
        let bytes = response.bytes().await?;
        let value: Value = if bytes.is_empty() {
            Value::Null
        } else {
            serde_json::from_slice(&bytes)?
        };
        self.evidence.push(
            json!({"interface":host,"method":method,"path":path,"status":status,"response":sanitize(value.clone())}),
        );
        self.flush()?;
        Ok((status, value))
    }

    async fn api(&mut self, method: &str, path: &str, payload: Option<Value>) -> Result<Value> {
        let (status, value) = self.api_at("api1", method, path, payload, true).await?;
        ensure(
            (200..300).contains(&status),
            &format!(
                "HTTPS {method} {path} returned {status}: {}",
                self.redact(&value.to_string())
            ),
        )?;
        Ok(value)
    }

    async fn plan(&mut self, operation: &str, parameters: Value) -> Result<Value> {
        self.api(
            "POST",
            "/v1/plans",
            Some(json!({"operation":operation,"parameters":parameters})),
        )
        .await
    }

    async fn apply(&mut self, plan: &Value) -> Result<Value> {
        let job = self
            .api(
                "POST",
                &format!("/v1/plans/{}/apply", plan["id"].as_str().ok_or("plan ID missing")?),
                Some(json!({"digest":plan["digest"]})),
            )
            .await?;
        let id = job["id"].as_str().ok_or("job ID missing")?;
        loop {
            let state = self.api("GET", &format!("/v1/jobs/{id}"), None).await?;
            match state["state"].as_str() {
                Some("succeeded") => {
                    for task in state["tasks"].as_array().into_iter().flatten() {
                        if let Some(upid) = task["upid"].as_str() {
                            let node = upid.split(':').nth(1).ok_or("invalid PVE UPID")?;
                            let result = self
                                .pve(&["get", &format!("/nodes/{node}/tasks/{upid}/status")])
                                .await?;
                            ensure(result["exitstatus"] == "OK", "PVE task did not finish successfully")?;
                        }
                    }
                    return Ok(state);
                }
                Some("failed") => return Err(format!("job {id} failed: {}", self.redact(&state.to_string())).into()),
                _ => tokio::time::sleep(Duration::from_millis(500)).await,
            }
        }
    }

    async fn operation(&mut self, operation: &str, parameters: Value) -> Result<Value> {
        let plan = self.plan(operation, parameters).await?;
        self.apply(&plan).await
    }

    fn install_args(&self, servers: bool, yes: bool) -> Result<Vec<String>> {
        let mut args: Vec<String> = vec![
            "--json",
            "install",
            "--ssh",
            "root@pve-e2e",
            "--storages",
            "local",
            "--snippet-storage",
            "local",
            "--service-storages",
            "local",
            "--template-storage",
            "local",
            "--gold",
            "100",
            "--bridge",
            "vmbr0",
        ]
        .into_iter()
        .map(str::to_owned)
        .collect();
        if servers {
            args.extend([
                "--server-domain".into(),
                crate::domain::SEARCH.into(),
                "--server-image".into(),
                self.report["service"]["image"]
                    .as_str()
                    .ok_or("missing selected image")?
                    .into(),
            ]);
        } else {
            args.push("--no-servers".into());
        }
        if yes {
            args.push("--yes".into());
        }
        Ok(args)
    }

    async fn ready(&mut self) -> Result<()> {
        self.token = std::fs::read_to_string(self.profile().join("api-token"))?.trim().into();
        let config: toml::Table = std::fs::read_to_string(self.profile().join("client.toml"))?.parse()?;
        let urls = config["server"]["urls"].as_array().ok_or("client has no server URLs")?;
        let nodes = self.report["nodes"].as_array().ok_or("missing node inventory")?.clone();
        ensure(urls.len() == nodes.len(), "installed client does not cover every node")?;
        for (index, node) in nodes.iter().enumerate() {
            let host = format!("api{}", index + 1);
            ensure(
                urls.iter()
                    .any(|url| url.as_str() == Some(&format!("https://{host}.e2e.homestack.app"))),
                "unexpected client endpoint",
            )?;
            let deadline = tokio::time::Instant::now() + Duration::from_secs(120);
            loop {
                match self.api_at(&host, "GET", "/health/ready", None, true).await {
                    Ok((200, _)) => break,
                    _ if tokio::time::Instant::now() < deadline => tokio::time::sleep(Duration::from_secs(2)).await,
                    _ => return Err(format!("{host} did not become ready over authenticated HTTPS").into()),
                }
            }
            let node_name = node["name"].as_str().ok_or("missing node name")?;
            for (name, content) in [
                (
                    "homestack-vendor-create.yaml",
                    include_str!("../../../deploy/cloud-init/homestack-vendor-create.yaml"),
                ),
                (
                    "homestack-vendor-keep.yaml",
                    include_str!("../../../deploy/cloud-init/homestack-vendor-keep.yaml"),
                ),
            ] {
                let actual = self
                    .node(node_name, &["sha256sum", &format!("/var/lib/vz/snippets/{name}")])
                    .await?;
                let expected: String = Sha256::digest(content.as_bytes())
                    .iter()
                    .map(|byte| format!("{byte:02x}"))
                    .collect();
                ensure(
                    String::from_utf8_lossy(&actual).split_whitespace().next() == Some(&expected),
                    "installed vendor snippet differs from tested source",
                )?;
            }
            let ctid = 101 + index as u64;
            let cfg = self
                .config(node["name"].as_str().ok_or("missing node name")?, "lxc", ctid)
                .await?;
            let deadline = tokio::time::Instant::now() + Duration::from_secs(120);
            loop {
                let current = self
                    .pve(&["get", &format!("/nodes/{node_name}/lxc/{ctid}/status/current")])
                    .await?;
                let resources = self.inventory().await?;
                let matches = resources.as_array().is_some_and(|vms| {
                    vms.iter().any(|vm| {
                        vm["vmid"] == ctid
                            && vm["pool"] == "homestack-service"
                            && vm["node"] == node["name"]
                            && vm["status"] == "running"
                    })
                });
                if current["status"] == "running" && matches {
                    break;
                }
                ensure(
                    tokio::time::Instant::now() < deadline,
                    "API CT pool, placement or state did not converge within 120 seconds",
                )?;
                tokio::time::sleep(Duration::from_secs(2)).await;
            }
            self.evidence
                .push(json!({"ctid":ctid,"node":node["name"],"config":sanitize(cfg)}));
        }
        self.version = self.api("GET", "/v1/config/version", None).await?["config_version"]
            .as_str()
            .ok_or("config version missing")?
            .into();
        self.cli("homestack", &["--json".into(), "status".into()]).await
    }
}

fn ensure(condition: bool, message: &str) -> Result<()> {
    if condition {
        Ok(())
    } else {
        Err(message.to_owned().into())
    }
}

fn ordered_rows(mut value: Value) -> Result<Value> {
    value
        .as_array_mut()
        .ok_or("invalid inventory collection")?
        .sort_by_cached_key(Value::to_string);
    Ok(value)
}

fn sanitize(mut value: Value) -> Value {
    match &mut value {
        Value::Object(map) => {
            for (key, value) in map {
                if [
                    "token",
                    "value",
                    "secret",
                    "password",
                    "authorization",
                    "env",
                    "lxc.environment",
                    "secrets",
                ]
                .iter()
                .any(|word| key.eq_ignore_ascii_case(word))
                    || key.to_ascii_lowercase().contains("password")
                    || key.contains("passphrase")
                    || key.contains("private_key")
                {
                    *value = json!("[REDACTED]");
                } else {
                    *value = sanitize(value.take());
                }
            }
        }
        Value::Array(values) => {
            for value in values {
                *value = sanitize(value.take());
            }
        }
        _ => {}
    }
    value
}

async fn install(ctx: &mut Context) -> Result<()> {
    let image = ctx.report["service"]["image"].as_str().ok_or("missing image")?;
    let resolved = hs_core::registry::resolve(image).await?;
    ensure(
        resolved.digest == ctx.report["service"]["digest"],
        "selected image tag changed before installation",
    )?;
    let args = ctx.install_args(true, true)?;
    ctx.cli("homestack", &args).await?;
    ctx.ready().await?;
    let image = ctx.report["service"]["image"].as_str().ok_or("missing image")?;
    ensure(
        hs_core::registry::resolve(image).await?.digest == ctx.report["service"]["digest"],
        "selected image tag changed during installation",
    )
}

async fn authorization_snapshot(ctx: &mut Context) -> Result<Value> {
    let roles = ordered_rows(ctx.pve(&["get", "/access/roles"]).await?)?;
    let acl = ordered_rows(ctx.pve(&["get", "/access/acl"]).await?)?;
    let tokens = ordered_rows(ctx.pve(&["get", "/access/users/homestack@pve/token"]).await?)?;
    Ok(json!({"roles":roles,"acl":acl,"tokens":tokens}))
}

async fn foreign(ctx: &mut Context) -> Result<()> {
    ensure(
        !ctx.inventory()
            .await?
            .as_array()
            .is_some_and(|vms| vms.iter().any(|v| v["vmid"] == 243)),
        "foreign fixture VMID is occupied",
    )?;
    let owner = format!(
        "HomeStack E2E foreign {}",
        ctx.report["run_id"].as_str().ok_or("missing ID")?
    );
    ctx.foreign = true;
    ctx.node(
        "pve1",
        &[
            "qm",
            "create",
            "243",
            "--name",
            "hst-foreign",
            "--description",
            &owner,
            "--memory",
            "128",
        ],
    )
    .await?;
    Ok(())
}

async fn permissions(ctx: &mut Context) -> Result<()> {
    foreign(ctx).await?;
    ensure(
        !ctx.inventory()
            .await?
            .as_array()
            .is_some_and(|vms| vms.iter().any(|v| v["vmid"] == 244)),
        "permission fixture VMID occupied",
    )?;
    let owner = format!(
        "HomeStack E2E permissions {}",
        ctx.report["run_id"].as_str().ok_or("missing ID")?
    );
    ctx.permission_vm = true;
    ctx.node(
        "pve1",
        &[
            "qm",
            "create",
            "244",
            "--pool",
            "homestack",
            "--name",
            "hst-permissions",
            "--description",
            &owner,
            "--memory",
            "128",
        ],
    )
    .await?;
    let state = authorization_snapshot(ctx).await?;
    let tokens = state["tokens"].as_array().ok_or("missing token list")?;
    for token in ["server", "deploy"] {
        ensure(
            tokens
                .iter()
                .any(|t| t["tokenid"] == token && (t["privsep"] == 1 || t["privsep"] == true)),
            "required token is missing or has no privilege separation",
        )?;
    }
    // Exact paths matter: pool permissions alone are not proof of VM authority.
    for token in [None, Some("server"), Some("deploy")] {
        let mut checks: Vec<(String, Vec<&str>)> = [
            (
                "/",
                if token == Some("deploy") {
                    vec![]
                } else {
                    vec!["Sys.Audit"]
                },
            ),
            (
                "/vms/100",
                if token == Some("deploy") {
                    vec![]
                } else {
                    vec!["VM.Audit", "VM.Clone"]
                },
            ),
            ("/vms/101", vec!["VM.Audit", "VM.Config.Disk"]),
            (
                "/vms/244",
                if token == Some("deploy") {
                    vec![]
                } else {
                    vec!["VM.Audit", "VM.Allocate", "VM.Config.Disk", "VM.Migrate"]
                },
            ),
            ("/storage/local", vec!["Datastore.Audit", "Datastore.AllocateSpace"]),
            ("/nodes/pve1", vec!["Sys.Audit"]),
            ("/vms/243", vec![]),
        ]
        .into_iter()
        .map(|(path, required)| (path.to_owned(), required))
        .collect();
        for (index, node) in ctx.report["nodes"]
            .as_array()
            .ok_or("missing node inventory")?
            .iter()
            .enumerate()
            .skip(1)
        {
            checks.push((
                format!("/nodes/{}", node["name"].as_str().ok_or("missing node name")?),
                vec!["Sys.Audit"],
            ));
            checks.push((
                format!("/vms/{}", 101 + index),
                vec![
                    "VM.Audit",
                    "VM.Config.Disk",
                    "VM.Config.CPU",
                    "VM.Config.Memory",
                    "VM.Config.Network",
                    "VM.Migrate",
                ],
            ));
        }
        for (path, required) in checks {
            let mut args = vec!["pveum", "user"];
            if token.is_some() {
                args.push("token");
            }
            args.extend(["permissions", "homestack@pve"]);
            if let Some(token) = token {
                args.push(token);
            }
            args.extend(["--path", path.as_str(), "--output-format", "json"]);
            let permissions: Value = serde_json::from_slice(&ctx.node("pve1", &args).await?)?;
            ensure(permissions.is_object(), "invalid effective permissions response")?;
            let empty = serde_json::Map::new();
            let grants = match permissions.get(&path) {
                Some(grants) => grants.as_object().ok_or("invalid privilege map")?,
                None => &empty, // Proxmox omits paths at which the principal has no grants.
            };
            for privilege in required {
                ensure(
                    grants.contains_key(privilege),
                    &format!("{token:?} lacks {privilege} at {path}"),
                )?;
            }
            for privilege in [
                "Permissions.Modify",
                "Sys.Modify",
                "User.Modify",
                "Realm.Allocate",
                "Sys.Console",
            ] {
                ensure(
                    !grants.contains_key(privilege),
                    &format!("unexpected elevated privilege {privilege} at {path}"),
                )?;
            }
            if path == "/vms/243" || path == "/vms/244" && token == Some("deploy") {
                ensure(
                    !grants.keys().any(|key| key.starts_with("VM.")),
                    "foreign VM has HomeStack privileges",
                )?;
            }
            ctx.evidence
                .push(json!({"principal":token.unwrap_or("user"),"path":path,"effective_permissions":permissions}));
        }
    }
    let before = ctx.config("pve1", "qemu", 243).await?;
    // Use each real token for an actual forbidden PVE config write.
    for file in ["pve-token", "pve-token-deploy"] {
        let secret = std::fs::read_to_string(ctx.profile().join(file))?;
        let response = ctx
            .client
            .put("https://pve1.e2e.homestack.app:8006/api2/json/nodes/pve1/qemu/243/config")
            .header("Authorization", format!("PVEAPIToken={}", secret.trim()))
            .form(&[("description", "forbidden mutation")])
            .send()
            .await?;
        ensure(
            response.status().as_u16() == 403,
            "foreign PVE operation did not return 403",
        )?;
        ensure(
            ctx.config("pve1", "qemu", 243).await? == before,
            "forbidden PVE operation changed foreign VM",
        )?;
    }
    ctx.evidence.push(state);
    Ok(())
}

async fn reads(ctx: &mut Context) -> Result<()> {
    let telemetry = ctx.api("GET", "/.well-known/homestack/status", None).await?;
    ensure(
        telemetry["schema"] == 3 && telemetry["status"] == "pass" && telemetry["process_rss_bytes"].as_u64().is_some(),
        "API service telemetry schema, health or RSS is incorrect",
    )?;
    for path in [
        "/v1/config/version",
        "/v1/capabilities",
        "/v1/storage",
        "/v1/storage/overview",
        "/v1/orphans",
        "/v1/discovery",
        "/v1/networks",
        "/v1/gold/readiness",
    ] {
        ctx.api("GET", path, None).await?;
    }
    let status = ctx.api("GET", "/v1/status", None).await?;
    let api_nodes = status["summary"]["nodes"]
        .as_array()
        .ok_or("API node inventory missing")?;
    let pve_nodes = ctx.pve(&["get", "/nodes"]).await?;
    let pve_nodes = pve_nodes.as_array().ok_or("PVE node inventory missing")?;
    ensure(
        api_nodes.len() == pve_nodes.len()
            && pve_nodes
                .iter()
                .all(|p| api_nodes.iter().any(|a| a["node"] == p["node"] && a["online"] == true)),
        "API and PVE inventory differ",
    )?;
    let gold = ctx.config("pve1", "qemu", 100).await?;
    ensure(
        gold["tags"]
            .as_str()
            .is_some_and(|s| s.split(';').any(|tag| tag == "homestack-gold")),
        "Gold tag missing",
    )?;
    ctx.evidence.push(json!({"pve_nodes":pve_nodes,"gold":gold}));
    Ok(())
}

async fn refusals(ctx: &mut Context) -> Result<()> {
    foreign(ctx).await?;
    let before = ctx.config("pve1", "qemu", 243).await?;
    let resources = ctx.inventory().await?;
    let volumes = ctx.volume_inventory().await?;
    let jobs = ctx.api("GET", "/v1/jobs", None).await?;
    let (status, _) = ctx
        .api_at(
            "api1",
            "POST",
            "/v1/plans",
            Some(json!({"operation":"workspace.destroy","parameters":{"vmid":243}})),
            false,
        )
        .await?;
    ensure(status == 401, "missing bearer token must return 401")?;
    for (operation, parameters, expected) in [
        ("workspace.destroy", json!({"vmid":243}), 404),
        (
            "workspace.create",
            json!({"vmid":242,"name":"hst-invalid","pool":"foreign"}),
            422,
        ),
        ("workspace.resize", json!({"vmid":"invalid"}), 422),
    ] {
        let (status, body) = ctx
            .api_at(
                "api1",
                "POST",
                "/v1/plans",
                Some(json!({"operation":operation,"parameters":parameters})),
                true,
            )
            .await?;
        ensure(
            status == expected,
            &format!("{operation} refusal expected {expected}, got {status}"),
        )?;
        if operation == "workspace.destroy" {
            ensure(
                body["error"]["code"] == "not_found",
                "foreign VM refusal must not disclose its identity",
            )?;
        }
    }
    ensure(
        ctx.config("pve1", "qemu", 243).await? == before && ctx.inventory().await? == resources,
        "refusal changed PVE configuration",
    )?;
    ensure(
        ctx.volume_inventory().await? == volumes && ctx.api("GET", "/v1/jobs", None).await? == jobs,
        "refusal allocated a volume or job",
    )?;
    Ok(())
}

async fn guest(ctx: &mut Context, command: &str) -> Result<String> {
    ctx.step = "guest QGA observation".into();
    let inventory = ctx.inventory().await?;
    let vm = inventory
        .as_array()
        .and_then(|vms| vms.iter().find(|v| v["vmid"] == 242))
        .ok_or("workspace not visible")?;
    let node = vm["node"].as_str().ok_or("workspace node missing")?;
    let cfg = ctx.config(node, "qemu", 242).await?;
    ensure(
        ctx.workspace
            .as_ref()
            .is_some_and(|identity| cfg["smbios1"] == identity["smbios1"] && cfg["name"] == identity["name"]),
        "workspace identity changed",
    )?;
    let result: Value = serde_json::from_slice(
        &ctx.node(
            node,
            &[
                "qm",
                "guest",
                "exec",
                "242",
                "--timeout",
                "30",
                "--",
                "nsenter",
                "--mount=/proc/1/ns/mnt",
                "--",
                "/bin/sh",
                "-lec",
                command,
            ],
        )
        .await?,
    )?;
    ensure(result["exitcode"] == 0, "guest command failed")?;
    Ok(result["out-data"].as_str().unwrap_or_default().into())
}

async fn guest_ready(ctx: &mut Context) -> Result<()> {
    let deadline = tokio::time::Instant::now() + Duration::from_secs(240);
    loop {
        if guest(
            ctx,
            "test -f /run/cloud-init/result.json && findmnt -rn -M /home/user && id user",
        )
        .await
        .is_ok()
        {
            return Ok(());
        }
        ensure(
            tokio::time::Instant::now() < deadline,
            "Cloud-Init/QGA/home did not become ready",
        )?;
        tokio::time::sleep(Duration::from_secs(2)).await;
    }
}

async fn workspace(ctx: &mut Context, kind: &str) -> Result<()> {
    ensure(
        !ctx.inventory()
            .await?
            .as_array()
            .is_some_and(|vms| vms.iter().any(|v| v["vmid"] == 242)),
        "workspace fixture VMID occupied",
    )?;
    let name = format!("hst-{}", &ctx.report["run_id"].as_str().ok_or("missing run ID")?[..8]);
    // Register the submitted plan before applying: a timeout is not proof of absence.
    let plan = ctx
        .plan("workspace.create", json!({"vmid":242,"name":name,"home_size":"512M"}))
        .await?;
    ctx.workspace = Some(json!({"name":name,"pending":true,"plan":plan}));
    if matches!(kind, "setup" | "lifecycle") {
        ctx.cli(
            "homestack",
            &[
                "--json".into(),
                "create".into(),
                "242".into(),
                name.clone(),
                "--home-size".into(),
                "512M".into(),
                "--yes".into(),
            ],
        )
        .await?;
    } else {
        ctx.apply(&plan).await?;
    }
    let cfg = ctx.config("pve1", "qemu", 242).await?;
    ensure(
        cfg["name"] == name && cfg["scsi1"].as_str().is_some(),
        "workspace config missing",
    )?;
    ctx.workspace = Some(cfg.clone());
    ctx.operation("vm.start", json!({"vmid":242})).await?;
    guest_ready(ctx).await?;
    let marker = format!("home-marker-{}", ctx.report["run_id"].as_str().ok_or("missing run ID")?);
    guest(ctx, &format!("printf %s {} > /home/user/hst-marker; chown 1000:1000 /home/user/hst-marker; chmod 640 /home/user/hst-marker; printf root-marker > /root/hst-root-marker; sync", phases::shell_quote(&marker))).await?;
    let home_before = guest(
        ctx,
        "findmnt -n -o UUID -M /home/user; stat -c '%u:%g:%a' /home/user/hst-marker; cat /home/user/hst-marker",
    )
    .await?;
    match kind {
        "setup" => setup_files(ctx).await?,
        "refresh" => {
            ctx.operation("workspace.refresh", json!({"vmid":242})).await?;
            guest_ready(ctx).await?;
            let after = ctx.config("pve1", "qemu", 242).await?;
            ensure(after["scsi1"] == cfg["scsi1"], "refresh replaced home volume")?;
            ensure(
                guest(ctx, "test ! -e /root/hst-root-marker && echo root-replaced")
                    .await?
                    .contains("root-replaced"),
                "disposable root was retained",
            )?;
        }
        "addkey" => {
            let key_dir = tempfile::Builder::new()
                .prefix("scenario-addkey-")
                .tempdir_in(&ctx.dir)?;
            let identity = key_dir.path().join("identity");
            let status = Command::new("ssh-keygen")
                .args(["-q", "-t", "ed25519", "-N", "", "-C", "homestack-e2e-addkey", "-f"])
                .arg(&identity)
                .stdin(Stdio::null())
                .status()
                .await?;
            ensure(status.success(), "could not generate disposable add-key identity")?;
            let key = std::fs::read_to_string(identity.with_extension("pub"))?;
            let blob = key.split_whitespace().nth(1).ok_or("invalid synthetic key")?;
            ensure(
                !guest(ctx, "cat /home/user/.ssh/authorized_keys").await?.contains(blob),
                "new add-key identity was already authorized",
            )?;
            ctx.operation("workspace.addkey", json!({"vmid":242,"key":key.trim(),"restart":false}))
                .await?;
            ensure(
                guest(ctx, "cat /home/user/.ssh/authorized_keys")
                    .await?
                    .contains(key.split_whitespace().nth(1).ok_or("invalid synthetic key")?),
                "added SSH key absent in guest",
            )?;
            verify_ssh(ctx, &identity).await?;
            key_dir.close()?;
        }
        "stale" => {
            let plan = ctx.plan("workspace.destroy", json!({"vmid":242})).await?;
            ctx.node("pve1", &["qm", "set", "242", "--name", &(name.clone() + "-changed")])
                .await?;
            let (status, _) = ctx
                .api_at(
                    "api1",
                    "POST",
                    &format!("/v1/plans/{}/apply", plan["id"].as_str().ok_or("missing plan ID")?),
                    Some(json!({"digest":plan["digest"]})),
                    true,
                )
                .await?;
            ensure(status == 409, "stale plan was not refused with 409")?;
            let after = ctx.config("pve1", "qemu", 242).await?;
            ensure(
                after["scsi0"] == cfg["scsi0"] && after["scsi1"] == cfg["scsi1"],
                "stale apply changed disks",
            )?;
            ctx.node("pve1", &["qm", "set", "242", "--name", &name]).await?;
        }
        _ => {
            ctx.cli("homestack", &["--json".into(), "status".into(), "242".into()])
                .await?;
            authorize_ssh(ctx).await?;
            ctx.operation("vm.stop", json!({"vmid":242})).await?;
            ensure(
                ctx.pve(&["get", "/nodes/pve1/qemu/242/status/current"]).await?["status"] == "stopped",
                "workspace did not stop",
            )?;
            ctx.operation("vm.start", json!({"vmid":242})).await?;
            guest_ready(ctx).await?;
        }
    }
    ensure(
        guest(
            ctx,
            "findmnt -n -o UUID -M /home/user; stat -c '%u:%g:%a' /home/user/hst-marker; cat /home/user/hst-marker",
        )
        .await?
            == home_before,
        "home filesystem identity, permissions, owner or contents changed",
    )?;
    ctx.evidence
        .push(json!({"workspace":242,"config":cfg,"home_observation":home_before}));
    Ok(())
}

async fn cluster(ctx: &mut Context) -> Result<()> {
    let state = ctx.pve(&["get", "/cluster/status"]).await?;
    let entries = state.as_array().ok_or("invalid cluster status")?;
    ensure(
        entries.iter().any(|e| e["type"] == "cluster" && e["quorate"] == 1)
            && entries
                .iter()
                .filter(|e| e["type"] == "node" && e["online"] == 1)
                .count()
                == 3,
        "cluster is not quorate with three online nodes",
    )?;
    for node in ["pve1", "pve2", "pve3"] {
        let version = ctx.node(node, &["pveversion", "--verbose"]).await?;
        let storage = ctx.pve(&["get", &format!("/nodes/{node}/storage")]).await?;
        ctx.evidence
            .push(json!({"node":node,"version":String::from_utf8(version)?,"storage":storage}));
    }
    ctx.evidence.push(json!({"cluster_status":state}));
    Ok(())
}

async fn recovery(ctx: &mut Context) -> Result<()> {
    let cfg = ctx.config("pve1", "lxc", 101).await?;
    ensure(cfg["hostname"] == "api1", "unexpected API instance identity")?;
    ctx.restart_ct = Some(("pve1".into(), 101));
    ctx.node("pve1", &["pct", "stop", "101"]).await?;
    ensure(
        ctx.pve(&["get", "/nodes/pve1/lxc/101/status/current"]).await?["status"] == "stopped",
        "API instance did not stop",
    )?;
    for host in ["api2", "api3"] {
        ensure(
            ctx.api_at(host, "GET", "/v1/status", None, true).await?.0 == 200,
            "surviving API peer unavailable",
        )?;
    }
    ctx.node("pve1", &["pct", "start", "101"]).await?;
    ctx.ready().await?;
    ctx.restart_ct = None;
    Ok(())
}

async fn uninstall(ctx: &mut Context) -> Result<()> {
    let deploy = ctx.profile().join("deploy.toml").display().to_string();
    if Path::new(&deploy).is_file() {
        ctx.cli(
            "homestack-deploy",
            &["--config".into(), deploy, "remove".into(), "--yes".into()],
        )
        .await?;
    }
    ctx.cli(
        "homestack",
        &[
            "--json".into(),
            "uninstall".into(),
            "--ssh".into(),
            "root@pve-e2e".into(),
            "--snippet-storage".into(),
            "local".into(),
            "--yes".into(),
        ],
    )
    .await?;
    let resources = ctx.inventory().await?;
    ensure(
        !resources.as_array().is_some_and(|vms| {
            vms.iter()
                .any(|v| v["pool"] == "homestack-service" || v["pool"] == "homestack")
        }),
        "HomeStack guests remain after uninstall",
    )?;
    ensure(
        !ctx.pve(&["get", "/access/users"])
            .await?
            .as_array()
            .is_some_and(|users| users.iter().any(|u| u["userid"] == "homestack@pve")),
        "HomeStack user remains after uninstall",
    )?;
    let acl = ctx.pve(&["get", "/access/acl"]).await?;
    ensure(
        !acl.as_array().is_some_and(|entries| {
            entries
                .iter()
                .any(|e| e["ugid"].as_str().is_some_and(|id| id.starts_with("homestack@pve")))
        }),
        "HomeStack ACLs remain after uninstall",
    )?;
    Ok(())
}

async fn cleanup_case(ctx: &mut Context) -> Result<()> {
    let mut errors = Vec::new();
    if ctx.node_recovery_pending {
        match restore_node(ctx).await {
            Ok(()) => {}
            Err(error) => errors.push(error.to_string()),
        }
    }
    if let Some((node, ctid)) = ctx.restart_ct.clone() {
        let result: Result<()> = async {
            let state = ctx
                .pve(&["get", &format!("/nodes/{node}/lxc/{ctid}/status/current")])
                .await?;
            if state["status"] != "running" {
                ensure(state["status"] == "stopped", "API container power state is uncertain")?;
                ctx.node(&node, &["pct", "start", &ctid.to_string()]).await?;
            }
            ctx.ready().await
        }
        .await;
        match result {
            Ok(()) => ctx.restart_ct = None,
            Err(error) => errors.push(error.to_string()),
        }
    }
    if let Some(identity) = ctx.workspace.clone() {
        let result: Result<()> = async {
            ensure(
                identity["pending"] != true,
                "create result is ambiguous; fixture cleanup is unconfirmed",
            )?;
            let mut reviewed = identity.clone();
            let inventory = ctx.inventory().await?;
            if let Some(vm) = inventory
                .as_array()
                .and_then(|vms| vms.iter().find(|v| v["vmid"] == 242))
            {
                let config = ctx
                    .config(vm["node"].as_str().ok_or("missing node")?, "qemu", 242)
                    .await?;
                ensure(
                    config["smbios1"] == identity["smbios1"]
                        && config["name"] == identity["name"]
                        && vm["pool"] == "homestack",
                    "workspace cleanup ownership changed",
                )?;
                reviewed = config;
                ctx.operation("workspace.destroy", json!({"vmid":242})).await?;
            }
            ensure(
                !ctx.inventory()
                    .await?
                    .as_array()
                    .is_some_and(|vms| vms.iter().any(|v| v["vmid"] == 242)),
                "workspace remains after destroy",
            )?;
            for entry in ctx.report["nodes"].as_array().ok_or("missing nodes")?.clone() {
                let node = entry["name"].as_str().ok_or("missing node")?;
                // --vmid filters images, but still includes unrelated snippets
                // and templates unless content is restricted explicitly.
                let volumes = ctx
                    .pve(&[
                        "get",
                        &format!("/nodes/{node}/storage/local/content"),
                        "--content",
                        "images",
                    ])
                    .await?;
                let known: Vec<_> = ["scsi0", "scsi1", "ide2"]
                    .iter()
                    .flat_map(|slot| [&identity[*slot], &reviewed[*slot]])
                    .filter_map(|value| value.as_str().and_then(|spec| spec.split(',').next()))
                    .collect();
                ensure(
                    volumes.as_array().is_some_and(|rows| {
                        rows.iter().all(|volume| {
                            volume["volid"].as_str().is_some_and(|id| !id.is_empty())
                                && volume["vmid"].as_u64().is_some()
                        })
                    }),
                    "incomplete image identity or native owner in storage inventory",
                )?;
                let leftovers: Vec<_> = volumes
                    .as_array()
                    .ok_or("invalid storage content inventory")?
                    .iter()
                    .filter(|volume| {
                        volume["vmid"] == 242 || volume["volid"].as_str().is_some_and(|id| known.contains(&id))
                    })
                    .collect();
                ensure(
                    leftovers.is_empty(),
                    &format!("workspace volumes remain after destroy on {node}: {}", json!(leftovers)),
                )?;
            }
            Ok(())
        }
        .await;
        match result {
            Ok(()) => ctx.workspace = None,
            Err(error) => errors.push(crate::describe(error.as_ref())),
        }
    }
    if ctx.permission_vm
        && !ctx
            .inventory()
            .await?
            .as_array()
            .is_some_and(|vms| vms.iter().any(|vm| vm["vmid"] == 244))
    {
        ctx.permission_vm = false;
    }
    if ctx.permission_vm {
        let result: Result<()> = async {
            let cfg = ctx.config("pve1", "qemu", 244).await?;
            let expected = format!(
                "HomeStack E2E permissions {}",
                ctx.report["run_id"].as_str().ok_or("missing ID")?
            );
            ensure(
                cfg["description"] == expected && cfg["name"] == "hst-permissions",
                "permission fixture ownership changed",
            )?;
            ctx.node("pve1", &["qm", "destroy", "244", "--purge", "1"]).await?;
            ensure(
                !ctx.inventory()
                    .await?
                    .as_array()
                    .is_some_and(|vms| vms.iter().any(|v| v["vmid"] == 244)),
                "permission fixture remains",
            )
        }
        .await;
        match result {
            Ok(()) => ctx.permission_vm = false,
            Err(error) => errors.push(crate::describe(error.as_ref())),
        }
    }
    if ctx.foreign
        && !ctx
            .inventory()
            .await?
            .as_array()
            .is_some_and(|vms| vms.iter().any(|vm| vm["vmid"] == 243))
    {
        ctx.foreign = false;
    }
    if ctx.foreign {
        let result = async {
            let cfg = ctx.config("pve1", "qemu", 243).await?;
            let expected = format!(
                "HomeStack E2E foreign {}",
                ctx.report["run_id"].as_str().ok_or("missing ID")?
            );
            ensure(
                cfg["description"] == expected && cfg["name"] == "hst-foreign",
                "foreign fixture ownership changed",
            )?;
            ctx.node("pve1", &["qm", "destroy", "243", "--purge", "1"]).await?;
            ensure(
                !ctx.inventory()
                    .await?
                    .as_array()
                    .is_some_and(|vms| vms.iter().any(|v| v["vmid"] == 243)),
                "foreign fixture remains",
            )
        }
        .await;
        match result {
            Ok(()) => ctx.foreign = false,
            Err(error) => errors.push(crate::describe(error.as_ref())),
        }
    }
    ensure(errors.is_empty(), &errors.join("; "))
}

async fn legacy(ctx: &mut Context, case: &Scenario, kind: &str) -> Result<()> {
    ctx.step = format!("existing {kind} fixture");
    let private = ctx.dir.join("scenario-workers").join(&case.id);
    std::fs::create_dir_all(private.parent().ok_or("missing worker parent")?)?;
    let result = ctx.artifacts.join("worker-result.json");
    if result.exists() {
        std::fs::remove_file(&result)?;
    }
    let repo = Path::new(env!("CARGO_MANIFEST_DIR")).join("../..").canonicalize()?;
    let suite_report =
        Path::new(ctx.report["artifacts"].as_str().ok_or("missing suite artifacts")?).join("report.json");
    let stdout = private.with_file_name(format!("{}.stdout.log", case.id));
    let stderr = private.with_file_name(format!("{}.stderr.log", case.id));
    service::write_private(&stdout, b"")?;
    service::write_private(&stderr, b"")?;
    let mut child = Command::new("python3")
        .args(["-c", include_str!("scenario-worker.py")])
        .arg(&repo)
        .arg(suite_report)
        .arg(&private)
        .arg(&result)
        .arg(kind)
        .arg(&case.id)
        .stdin(Stdio::null())
        .stdout(Stdio::from(std::fs::File::create(&stdout)?))
        .stderr(Stdio::from(std::fs::File::create(&stderr)?))
        .process_group(0)
        .kill_on_drop(true)
        .spawn()?;
    let pid = child.id().ok_or("worker PID unavailable")?;
    let mut group = WorkerGroup(Some(pid));
    let mut timed_out = false;
    let status = match tokio::time::timeout(Duration::from_secs(case.timeout.saturating_sub(150)), child.wait()).await {
        Ok(result) => result?,
        Err(_) => {
            // Let the Python fixture unwind and perform its original guarded cleanup.
            Command::new("kill")
                .args(["-TERM", "--", &format!("-{pid}")])
                .status()
                .await?;
            timed_out = true;
            match tokio::time::timeout(Duration::from_secs(120), child.wait()).await {
                Ok(result) => result?,
                Err(_) => {
                    Command::new("kill")
                        .args(["-KILL", "--", &format!("-{pid}")])
                        .status()
                        .await?;
                    child.wait().await?
                }
            }
        }
    };
    let bytes = match std::fs::read(&result) {
        Ok(bytes) => bytes,
        Err(error) => {
            let mut diagnostic = ctx.redact(&std::fs::read_to_string(&stderr)?);
            for name in ["api-token", "pve-token"] {
                if let Ok(secret) = std::fs::read_to_string(private.join(name)) {
                    let secret = secret.trim();
                    if !secret.is_empty() {
                        diagnostic = diagnostic.replace(secret, "[REDACTED]");
                        if let Some((_, value)) = secret.split_once('=') {
                            diagnostic = diagnostic.replace(value, "[REDACTED]");
                        }
                    }
                }
            }
            diagnostic = diagnostic.replace("hs-resize-fixture-passphrase", "[REDACTED]");
            ctx.evidence.push(json!({"worker_exit_code":status.code(),"error_category":"worker_initialization","stderr":diagnostic,"result_error":error.to_string()}));
            ctx.flush()?;
            return Err(format!(
                "worker exited without a result ({}); fixture cleanup is unconfirmed: {}",
                status,
                diagnostic.lines().last().unwrap_or("no diagnostic output")
            )
            .into());
        }
    };
    let evidence: Value = serde_json::from_slice(&bytes)?;
    if evidence["cleanup"]["status"] == "PASSED" {
        group.0 = None;
    }
    ctx.evidence.push(sanitize(evidence.clone()));
    ctx.flush()?;
    ensure(
        evidence["cleanup"]["status"] == "PASSED",
        "existing fixture cleanup failed; see worker evidence",
    )?;
    ensure(
        !timed_out && status.success() && evidence["status"] == "PASSED",
        evidence["error"].as_str().unwrap_or("existing scenario failed"),
    )
}

async fn execute_case(ctx: &mut Context, case: &Scenario) -> Result<()> {
    match case.action {
        Action::Confirmation => confirmation(ctx).await,
        Action::Tui => tui(ctx).await,
        Action::NodeRecovery => node_recovery(ctx).await,
        Action::Install => install(ctx).await,
        Action::Permissions => permissions(ctx).await,
        Action::Idempotency => {
            let before = authorization_snapshot(ctx).await?;
            let output = ctx.command("homestack", &ctx.install_args(false, true)?).await?;
            ensure(output.status.success(), "repeat install failed")?;
            let applied: Value = serde_json::from_slice(&output.stdout)?;
            ensure(
                applied["applied_steps"] == 0 && before == authorization_snapshot(ctx).await?,
                "repeat install changed roles, ACLs or tokens",
            )?;
            // Exercise the real noninteractive confirmation boundary too.
            Ok(())
        }
        Action::Reads => reads(ctx).await,
        Action::Refusals => refusals(ctx).await,
        Action::Workspace(kind) => workspace(ctx, kind).await,
        Action::Legacy(kind) => legacy(ctx, case, kind).await,
        Action::Cluster => cluster(ctx).await,
        Action::InstanceRecovery => recovery(ctx).await,
        Action::Reinstall => {
            uninstall(ctx).await?;
            install(ctx).await
        }
        Action::Gap(reason) => Err(reason.into()),
    }
}

pub async fn run(options: &Options, dir: &Path, artifacts: &Path, report: &mut Value) -> Result<()> {
    let suite_started = tokio::time::Instant::now();
    phases::begin(report, "scenarios");
    save_report(artifacts, report)?;
    // The listener proves the active controller owns the worker report; it is
    // deliberately separate from the manual inspection/resume socket.
    let socket = dir.join("scenario.sock");
    let listener = tokio::net::UnixListener::bind(&socket)?;
    let controller = tokio::spawn(async move {
        while let Ok((mut stream, _)) = listener.accept().await {
            let _ = stream.write_all(b"E2E\n").await;
        }
    });
    std::fs::create_dir_all(dir.join("client"))?;
    let registry = crate::registry_proxy::start().await?;
    let mut tunnel = crate::tunnel::Tunnel::start(dir, registry.port).await?;
    let proxy = format!("socks5h://127.0.0.1:{}", tunnel.port);
    report["scenario_proxy"] = json!(proxy);
    report["scenario_socket"] = json!(socket);
    save_report(artifacts, report)?;
    let client = reqwest::Client::builder()
        .proxy(reqwest::Proxy::all(&proxy)?)
        .timeout(Duration::from_secs(45))
        .build()?;
    let mut ctx = Context {
        dir: dir.into(),
        artifacts: artifacts.join("scenarios"),
        report: report.clone(),
        client,
        token: String::new(),
        version: String::new(),
        evidence: Vec::new(),
        step: String::new(),
        workspace: None,
        foreign: false,
        permission_vm: false,
        restart_ct: None,
        node_recovery_pending: false,
    };
    let mut installed = false;
    let mut safe = true;
    let mut installation_attempted = false;
    for (index, case) in selected(options).iter().enumerate() {
        if report["scenarios"][index]["status"] == "OUT_OF_SCOPE" {
            continue;
        }
        if matches!(case.action, Action::Gap(_))
            || (report["service_preparation_error"].is_string() && !matches!(case.action, Action::Cluster))
            || (!safe && case.baseline != "infrastructure")
            || (case.baseline == "installed" && !installed)
        {
            report["scenarios"][index]["status"] = json!("BLOCKED");
            report["scenarios"][index]["error_category"] = json!(if matches!(case.action, Action::Gap(_)) {
                "coverage_gap"
            } else {
                "baseline_unavailable"
            });
            report["scenarios"][index]["error"] = json!(if let Action::Gap(reason) = case.action {
                reason
            } else {
                report["service_preparation_error"]
                    .as_str()
                    .unwrap_or("installed baseline unavailable or unsafe after an earlier failure")
            });
            save_report(artifacts, report)?;
            continue;
        }
        ctx.artifacts = artifacts.join("scenarios").join(&case.id);
        std::fs::create_dir_all(&ctx.artifacts)?;
        ctx.evidence.clear();
        ctx.step = "preconditions".into();
        report["scenarios"][index]["status"] = json!("RUNNING");
        report["scenarios"][index]["started_at_ms"] = json!(phases::now_ms());
        report["scenarios"][index]["log_directory"] = json!(ctx.artifacts);
        save_report(artifacts, report)?;
        eprintln!("Scenario: {}", case.id);
        let started = tokio::time::Instant::now();
        if matches!(case.action, Action::Install) {
            installation_attempted = true;
        }
        if matches!(case.action, Action::NodeRecovery) {
            node_recovery_diagnostics(&mut ctx, "before-stop").await;
        }
        let functional_started = tokio::time::Instant::now();
        let (mut result, mut timed_out) =
            match tokio::time::timeout(Duration::from_secs(case.timeout), execute_case(&mut ctx, case)).await {
                Ok(result) => (result, false),
                Err(_) => (
                    Err("scenario deadline exceeded; outstanding job state is unconfirmed".into()),
                    true,
                ),
            };
        if matches!(case.action, Action::NodeRecovery) {
            let remaining = Duration::from_secs(case.timeout).saturating_sub(functional_started.elapsed());
            // Save the functional result before diagnostics, including on timeout.
            node_recovery_diagnostics(
                &mut ctx,
                if result.is_ok() {
                    "outage-passed"
                } else {
                    "outage-failed"
                },
            )
            .await;
            if result.is_ok() {
                match tokio::time::timeout(remaining, restore_node(&mut ctx)).await {
                    Ok(recovery) => result = recovery,
                    Err(_) => {
                        result = Err("scenario deadline exceeded; node recovery is unconfirmed".into());
                        timed_out = true;
                    }
                }
            }
        }
        let failed_step = ctx.step.clone();
        // Evidence persistence must not bypass fault-injection recovery.
        if !matches!(case.action, Action::NodeRecovery) {
            ctx.flush()?;
        }
        let cleanup = tokio::time::timeout(Duration::from_secs(180), cleanup_case(&mut ctx))
            .await
            .unwrap_or_else(|_| Err("scenario cleanup deadline exceeded".into()));
        if matches!(case.action, Action::NodeRecovery) {
            // Post-recovery diagnostics run outside the functional/cleanup deadlines.
            if !ctx.node_recovery_pending && ctx.evidence.iter().any(|e| e["node_recovery_seconds"].is_number()) {
                node_recovery_diagnostics(&mut ctx, "recovered").await;
            }
            persist_node_diagnostics(&mut ctx);
        }
        let worker_cleanup_unsafe = matches!(case.action, Action::Legacy(_))
            && result.is_err()
            && ctx.evidence.last().is_none_or(|e| e["cleanup"]["status"] != "PASSED");
        if cleanup.is_err() || worker_cleanup_unsafe || timed_out {
            safe = false;
        }
        if matches!(case.action, Action::Install) {
            installed = result.is_ok();
        }
        if matches!(case.action, Action::Reinstall) && result.is_err() {
            installed = false;
        }
        if matches!(case.action, Action::Permissions) && result.is_err() {
            installed = false;
        }
        let status = if result.is_ok() && cleanup.is_ok() {
            "PASSED"
        } else {
            "FAILED"
        };
        let entry = &mut report["scenarios"][index];
        entry["status"] = json!(status);
        entry["duration_seconds"] = json!(started.elapsed().as_secs_f64());
        entry["observed"] = json!(ctx.evidence);
        entry["failed_step"] = json!(result.as_ref().err().map(|_| failed_step));
        entry["error_category"] = json!(if timed_out {
            Some("timeout")
        } else if result.is_err() {
            Some("operation_or_assertion")
        } else if cleanup.is_err() {
            Some("cleanup_failed")
        } else {
            None
        });
        entry["error"] = json!(result.as_ref().err().map(|e| ctx.redact(&crate::describe(e.as_ref()))));
        entry["cleanup"] = json!({"status":if cleanup.is_ok() && !worker_cleanup_unsafe {"PASSED"} else {"FAILED"},"error":cleanup.as_ref().err().map(|e|ctx.redact(&crate::describe(e.as_ref())))});
        if matches!(case.action, Action::Install) {
            report["product_installation"] =
                json!({"status":status,"image":report["service"],"error":report["scenarios"][index]["error"]});
        }
        save_report(artifacts, report)?;
        eprintln!("{}: {status}", case.id);
    }
    let functional_result = ensure(
        report["scenarios"].as_array().is_some_and(|cases| {
            cases
                .iter()
                .all(|case| case["status"] == "PASSED" || case["status"] == "OUT_OF_SCOPE")
        }),
        "required functional scenarios failed or were blocked",
    );
    phases::finish(report, "scenarios", &functional_result, suite_started.elapsed());
    phases::begin(report, "uninstall");
    let uninstall_started = tokio::time::Instant::now();
    save_report(artifacts, report)?;
    ctx.artifacts = artifacts.join("uninstall");
    ctx.evidence.clear();
    std::fs::create_dir_all(&ctx.artifacts)?;
    let uninstall_result = if installation_attempted && safe {
        tokio::time::timeout(Duration::from_secs(900), uninstall(&mut ctx))
            .await
            .unwrap_or_else(|_| Err("uninstall deadline exceeded".into()))
    } else {
        Err("normal uninstall blocked by unavailable or unsafe baseline; owned QEMU lab cleanup still runs".into())
    };
    ctx.flush()?;
    report["uninstall"] = json!({"status":if uninstall_result.is_ok(){"PASSED"}else if !installation_attempted {"BLOCKED"}else{"FAILED"},"error":uninstall_result.as_ref().err().map(|e|ctx.redact(&crate::describe(e.as_ref()))),"evidence":ctx.evidence});
    phases::finish(report, "uninstall", &uninstall_result, uninstall_started.elapsed());
    let stopped = tunnel.stop().await;
    controller.abort();
    let _ = controller.await;
    std::fs::remove_file(socket)?;
    report["scenario_proxy"] = Value::Null;
    finish_pending(report, "scenario runner could not establish the baseline");
    save_report(artifacts, report)?;
    stopped?;
    ensure(
        uninstall_result.is_ok()
            && report["scenarios"].as_array().is_some_and(|cases| {
                cases
                    .iter()
                    .all(|c| c["status"] == "PASSED" || c["status"] == "OUT_OF_SCOPE")
            }),
        "required scenario failed/blocked or uninstall incomplete; see scenarios.json and summary.txt",
    )
}

async fn preinstall_snapshot(ctx: &mut Context) -> Result<Value> {
    Ok(
        json!({"roles":ordered_rows(ctx.pve(&["get","/access/roles"]).await?)?, "acl":ordered_rows(ctx.pve(&["get","/access/acl"]).await?)?,
        "users":ordered_rows(ctx.pve(&["get","/access/users"]).await?)?, "guests":ctx.inventory().await?, "gold":ctx.config("pve1","qemu",100).await?}),
    )
}

async fn confirmation(ctx: &mut Context) -> Result<()> {
    let before = preinstall_snapshot(ctx).await?;
    let output = ctx.command("homestack", &ctx.install_args(true, false)?).await?;
    ensure(
        output.status.code() == Some(3),
        "unconfirmed installation did not exit with code 3",
    )?;
    let refusal: Value = serde_json::from_slice(&output.stdout)?;
    ensure(
        refusal["confirmation_required"] == true,
        "installer did not require confirmation",
    )?;
    ensure(
        preinstall_snapshot(ctx).await? == before,
        "unconfirmed installation mutated Proxmox",
    )
}

async fn tui(ctx: &mut Context) -> Result<()> {
    let before = authorization_snapshot(ctx).await?;
    let jobs = ctx.api("GET", "/v1/jobs", None).await?;
    let resources = ctx.inventory().await?;
    let template = crate::client::automation(&ctx.dir, "homestack", &["tui".into()])?;
    let mut command = Command::new("python3");
    command
        .args(["-c", include_str!("scenario-tui.py")])
        .arg(template.as_std().get_program());
    for (key, value) in template.as_std().get_envs() {
        if let Some(value) = value {
            command.env(key, value);
        } else {
            command.env_remove(key);
        }
    }
    let proxy = ctx.report["scenario_proxy"].as_str().ok_or("missing proxy")?;
    for key in ["HTTPS_PROXY", "https_proxy", "ALL_PROXY", "all_proxy"] {
        command.env(key, proxy);
    }
    let child = command
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .process_group(0)
        .kill_on_drop(true)
        .spawn()?;
    let mut group = WorkerGroup(Some(child.id().ok_or("TUI driver PID missing")?));
    let output = child.wait_with_output().await?;
    group.0 = None;
    ctx.evidence.push(json!({"terminal":ctx.redact(&String::from_utf8_lossy(&output.stdout)),"errors":ctx.redact(&String::from_utf8_lossy(&output.stderr))}));
    ensure(
        output.status.success(),
        "real TUI cancellation journey failed; see terminal evidence",
    )?;
    ensure(
        authorization_snapshot(ctx).await? == before
            && ctx.inventory().await? == resources
            && ctx.api("GET", "/v1/jobs", None).await? == jobs,
        "TUI cancellation mutated product state",
    )
}

async fn node_signal(ctx: &mut Context, signal: &str) -> Result<()> {
    let node = ctx.report["nodes"]
        .as_array()
        .and_then(|nodes| nodes.iter().find(|n| n["name"] == "pve3"))
        .ok_or("pve3 absent")?;
    let pid = node["qemu_pid"].as_u64().ok_or("missing QEMU PID")?;
    let directory = Path::new(node["directory"].as_str().ok_or("missing node directory")?);
    ensure(
        directory.starts_with(&ctx.dir) && directory != ctx.dir,
        "node process directory outside owned lab",
    )?;
    let cmdline = std::fs::read(format!("/proc/{pid}/cmdline"))?;
    let arguments: Vec<_> = cmdline.split(|b| *b == 0).collect();
    let disk = directory.join("disk.qcow2");
    ensure(
        arguments
            .iter()
            .any(|arg| String::from_utf8_lossy(arg).contains(&disk.display().to_string())),
        "QEMU process identity no longer matches owned node disk",
    )?;
    ensure(
        arguments
            .first()
            .is_some_and(|arg| String::from_utf8_lossy(arg).contains("qemu-system-x86_64")),
        "node process is not QEMU",
    )?;
    ensure(
        Command::new("kill")
            .args([signal, "--", &pid.to_string()])
            .status()
            .await?
            .success(),
        "could not signal owned QEMU process",
    )
}

// Diagnostics never authorize a mutation or replace quorum assertions.
async fn node_recovery_diagnostics(ctx: &mut Context, phase: &str) {
    let mut processes = Vec::new();
    let mut pids = Vec::new();
    let mut targets = vec![(
        "backbone".to_owned(),
        ctx.report["backbone_pid"].as_u64(),
        ctx.dir.join("backbone-1.sock"),
    )];
    if let Some(nodes) = ctx.report["nodes"].as_array() {
        for node in nodes {
            if let (Some(name), Some(directory)) = (node["name"].as_str(), node["directory"].as_str()) {
                targets.push((
                    name.to_owned(),
                    node["qemu_pid"].as_u64(),
                    Path::new(directory).join("disk.qcow2"),
                ));
            }
        }
    }
    for (name, pid, identity) in targets {
        let observation = (|| -> Result<Value> {
            let pid = pid.ok_or("missing process PID")?;
            ensure(identity.starts_with(&ctx.dir), "process identity outside owned lab")?;
            let cmdline = std::fs::read(format!("/proc/{pid}/cmdline"))?;
            ensure(
                cmdline
                    .split(|b| *b == 0)
                    .any(|arg| String::from_utf8_lossy(arg).contains(&identity.display().to_string())),
                "process no longer matches owned lab",
            )?;
            pids.push(pid);
            let mut state = json!({"name":name,"pid":pid});
            for field in ["status", "stat", "wchan"] {
                state[field] = match std::fs::read_to_string(format!("/proc/{pid}/{field}")) {
                    Ok(value) => json!(value),
                    Err(error) => json!({"error":error.to_string()}),
                };
            }
            Ok(state)
        })();
        processes.push(observation.unwrap_or_else(|error| json!({"name":name,"error":error.to_string()})));
    }
    let sockets = match tokio::time::timeout(
        Duration::from_secs(3),
        Command::new("ss").args(["-xapn"]).kill_on_drop(true).output(),
    )
    .await
    {
        Ok(Ok(output)) => {
            json!({"exit_code":output.status.code(),"owned_socket_queues":String::from_utf8_lossy(&output.stdout).lines().filter(|line| {
            line.contains(&ctx.dir.display().to_string()) || pids.iter().any(|pid| line.contains(&format!("pid={pid},")))
        }).collect::<Vec<_>>(),"stderr":ctx.redact(&String::from_utf8_lossy(&output.stderr))})
        }
        other => json!({"error":format!("socket observation unavailable: {other:?}")}),
    };
    ctx.evidence.push(json!({"node_recovery_diagnostics":phase,"observed_at_ms":phases::now_ms(),"host_processes":processes,"host_unix_sockets":sockets}));
    persist_node_diagnostics(ctx);
    // Immutable access avoids changing ctx.step, including on cancellation.
    // Independent SSH forwards do not depend on the shared L2 backbone.
    let (first, second) = tokio::join!(survivor_diagnostics(ctx, "pve1"), survivor_diagnostics(ctx, "pve2"));
    for (node, observation) in [("pve1", first), ("pve2", second)] {
        let mut observation = sanitize(observation);
        redact_diagnostic(ctx, &mut observation);
        ctx.evidence
            .push(json!({"node_recovery_diagnostics":phase,"node":node,"observation":observation}));
        persist_node_diagnostics(ctx);
    }
}

fn redact_diagnostic(ctx: &Context, value: &mut Value) {
    match value {
        Value::String(text) => *text = ctx.redact(text),
        Value::Array(values) => values.iter_mut().for_each(|v| redact_diagnostic(ctx, v)),
        Value::Object(values) => values.values_mut().for_each(|v| redact_diagnostic(ctx, v)),
        _ => {}
    }
}

fn persist_node_diagnostics(ctx: &mut Context) {
    if let Err(error) = ctx.flush() {
        let error = ctx.redact(&crate::describe(error.as_ref()));
        eprintln!("Node recovery evidence persistence failed: {error}");
        // Retain in memory for the scenario report or a later successful flush.
        ctx.evidence
            .push(json!({"node_recovery_diagnostics_error":"evidence persistence failed","error":error}));
    }
}

async fn survivor_diagnostics(ctx: &Context, node: &str) -> Value {
    let result: Result<Value> = async {
        let command = ctx.node_command(
            node,
            &["python3", "-c", include_str!("node-recovery-diagnostics.py"), node],
        )?;
        diagnostic_output(command, Duration::from_secs(25)).await
    }
    .await;
    result.unwrap_or_else(|error| json!({"error":error.to_string()}))
}

async fn diagnostic_output(mut command: Command, limit: Duration) -> Result<Value> {
    let mut child = command
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .kill_on_drop(true)
        .spawn()?;
    let mut stdout = child.stdout.take().ok_or("diagnostic stdout unavailable")?;
    let mut stderr = child.stderr.take().ok_or("diagnostic stderr unavailable")?;
    let mut out = Vec::new();
    let mut err = Vec::new();
    let result = tokio::time::timeout(limit, async {
        tokio::try_join!(stdout.read_to_end(&mut out), stderr.read_to_end(&mut err), child.wait())
    })
    .await;
    let mut observation = json!({"stderr":String::from_utf8_lossy(&err),"observations":[]});
    match result {
        Ok(Ok((_, _, status))) => {
            observation["exit_code"] = json!(status.code());
            if !status.success() {
                observation["error"] = json!("diagnostic SSH command failed");
            }
        }
        Ok(Err(error)) => observation["error"] = json!(error.to_string()),
        Err(_) => observation["error"] = json!("independent SSH diagnostic deadline exceeded"),
    }
    // JSON lines preserve completed commands even on SSH failure or timeout.
    let mut records = Vec::new();
    for line in out.split(|byte| *byte == b'\n').filter(|line| !line.is_empty()) {
        records.push(
            serde_json::from_slice::<Value>(line)
                .unwrap_or_else(|error| json!({"error":error.to_string(),"raw_stdout":String::from_utf8_lossy(line)})),
        );
    }
    if records.is_empty() {
        observation["output_error"] = json!("no diagnostic observations received");
    }
    observation["observations"] = json!(records);
    Ok(observation)
}

async fn restore_node(ctx: &mut Context) -> Result<()> {
    // Keep recovery pending after CONT until the shared baseline is usable.
    // Cleanup must retry/verify recovery when the outage assertion itself fails.
    let resumed_at = tokio::time::Instant::now();
    node_signal(ctx, "-CONT").await?;
    let deadline = resumed_at + Duration::from_secs(120);
    let mut stable = 0;
    while stable < 2 {
        stable = if cluster(ctx).await.is_ok() { stable + 1 } else { 0 };
        ensure(
            tokio::time::Instant::now() < deadline,
            "resumed nodes did not recover stable three-node quorum",
        )?;
        if stable < 2 {
            tokio::time::sleep(Duration::from_secs(2)).await;
        }
    }
    ctx.ready().await?;
    ctx.evidence
        .push(json!({"node_recovery_seconds":resumed_at.elapsed().as_secs_f64(),"stable_quorum_samples":2}));
    ctx.node_recovery_pending = false;
    Ok(())
}

async fn node_recovery(ctx: &mut Context) -> Result<()> {
    // Infrastructure fault injection, not a HomeStack cluster-membership operation.
    ctx.node_recovery_pending = true;
    let stopped_at = tokio::time::Instant::now();
    node_signal(ctx, "-STOP").await?;
    let deadline = stopped_at + Duration::from_secs(100);
    let mut stable = 0;
    while stable < 2 {
        let status = ctx.pve(&["get", "/cluster/status"]).await?;
        let entries = status.as_array().ok_or("invalid cluster status")?;
        let down = entries.iter().any(|e| e["name"] == "pve3" && e["online"] == 0);
        let survivors = ["pve1", "pve2"]
            .iter()
            .all(|name| entries.iter().any(|e| e["name"] == *name && e["online"] == 1));
        let quorate = entries.iter().any(|e| e["type"] == "cluster" && e["quorate"] == 1);
        stable = if down && survivors && quorate { stable + 1 } else { 0 };
        ensure(
            tokio::time::Instant::now() < deadline,
            "node outage did not converge to two surviving members with quorum",
        )?;
        if stable == 2 {
            ctx.evidence.push(json!({"unavailable_node_cluster_status":status,"surviving_quorum_convergence_seconds":stopped_at.elapsed().as_secs_f64()}));
        } else {
            tokio::time::sleep(Duration::from_secs(2)).await;
        }
    }
    ensure(
        ctx.api_at("api1", "GET", "/v1/status", None, true).await?.0 == 200,
        "HomeStack API unavailable during node outage",
    )?;
    Ok(())
}

async fn authorize_ssh(ctx: &mut Context) -> Result<()> {
    let key = std::fs::read_to_string(ctx.dir.join("client/home/.ssh/e2e-workspace.pub"))?;
    ctx.operation("workspace.addkey", json!({"vmid":242,"key":key.trim(),"restart":false}))
        .await?;
    let identity = ctx.dir.join("client/home/.ssh/e2e-workspace");
    verify_ssh(ctx, &identity).await
}

async fn verify_ssh(ctx: &mut Context, identity: &Path) -> Result<()> {
    let key = guest(ctx, "cat /etc/ssh/ssh_host_ed25519_key.pub").await?;
    let hosts = ctx.dir.join("scenario-guest-hosts");
    service::write_private(&hosts, format!("10.77.0.242 {}\n", key.trim()).as_bytes())?;
    let jump = format!(
        "/usr/bin/ssh -F {} -o BatchMode=yes -W %h:%p pve-e2e",
        phases::shell_quote(&ctx.dir.join("ssh_config").display().to_string())
    );
    let output = Command::new("ssh")
        .args([
            "-F",
            "/dev/null",
            "-o",
            "BatchMode=yes",
            "-o",
            "IdentityAgent=none",
            "-o",
            "IdentitiesOnly=yes",
            "-o",
            "StrictHostKeyChecking=yes",
            "-o",
            "GlobalKnownHostsFile=/dev/null",
            "-o",
            "ConnectTimeout=10",
            "-o",
            &format!("UserKnownHostsFile={}", hosts.display()),
            "-o",
            &format!("ProxyCommand={jump}"),
            "-i",
            &identity.display().to_string(),
            "user@10.77.0.242",
            "cat /home/user/hst-marker",
        ])
        .stdin(Stdio::null())
        .kill_on_drop(true)
        .output()
        .await?;
    ctx.evidence.push(json!({"guest_ssh_attempt":{"exit_code":output.status.code(),"stderr":ctx.redact(&String::from_utf8_lossy(&output.stderr))}}));
    ctx.flush()?;
    ensure(output.status.success(), "isolated guest SSH authentication failed")?;
    ensure(
        String::from_utf8(output.stdout)?.trim()
            == format!("home-marker-{}", ctx.report["run_id"].as_str().ok_or("missing ID")?),
        "SSH read did not match guest persistence marker",
    )?;
    ctx.evidence
        .push(json!({"guest_ssh":"pinned host key via guarded lab jump","marker_verified":true}));
    Ok(())
}

async fn setup_files(ctx: &mut Context) -> Result<()> {
    authorize_ssh(ctx).await?;
    let source = ctx.dir.join("client/home/.gitconfig");
    let content = format!(
        "[user]\n    name = HomeStack E2E {}\n    email = e2e@example.invalid\n",
        ctx.report["run_id"].as_str().ok_or("missing ID")?
    );
    service::write_private(&source, content.as_bytes())?;
    let result = ctx
        .cli(
            "homestack",
            &[
                "--json".into(),
                "setup".into(),
                "242".into(),
                "app=git:file:~/.gitconfig".into(),
                "--non-interactive".into(),
                "--yes".into(),
            ],
        )
        .await;
    std::fs::remove_file(source)?;
    result?;
    ensure(
        guest(ctx, "cat /home/user/.gitconfig").await? == content,
        "setup file differs from the synthetic desktop source",
    )?;
    ensure(
        guest(ctx, "stat -c '%u:%g:%a' /home/user/.gitconfig").await?.trim() == "1000:1000:600",
        "setup file ownership or permissions changed",
    )
}

#[cfg(test)]
mod node_diagnostic_tests {
    use super::*;

    #[tokio::test]
    async fn node_diagnostics_preserve_partial_output_on_failure_and_timeout() -> Result<()> {
        for (script, limit, exit_code) in [
            (
                "import sys; print('{\"uptime\":\"42\"}', flush=True); print('broken JSON', flush=True); print('SSH failed', file=sys.stderr); sys.exit(255)",
                Duration::from_secs(5),
                Some(255),
            ),
            (
                "import time; print('{\"uptime\":\"42\"}', flush=True); time.sleep(60)",
                Duration::from_secs(1),
                None,
            ),
        ] {
            let mut command = Command::new("python3");
            command.args(["-c", script]);
            let observation = diagnostic_output(command, limit).await?;
            assert!(observation["error"].is_string());
            assert_eq!(observation["exit_code"].as_i64(), exit_code);
            assert_eq!(observation["observations"][0]["uptime"], "42");
            if exit_code.is_some() {
                assert_eq!(observation["observations"][1]["raw_stdout"], "broken JSON");
                assert!(observation["observations"][1]["error"].is_string());
                assert!(
                    observation["stderr"]
                        .as_str()
                        .ok_or("missing stderr")?
                        .contains("SSH failed")
                );
            }
        }
        Ok(())
    }

    #[tokio::test]
    async fn node_diagnostics_failures_do_not_skip_survivors_or_recovery() -> Result<()> {
        let dir = tempfile::tempdir()?;
        let artifacts = dir.path().join("unwritable-artifacts");
        std::fs::write(&artifacts, b"not a directory")?;
        let mut ctx = Context {
            dir: dir.path().into(),
            artifacts: artifacts.clone(),
            // pve1 fails inventory validation; pve2 fails run identity validation.
            report: json!({"run_id":"invalid","nodes":[{"name":"pve2"},{"name":"pve3"}]}),
            client: reqwest::Client::new(),
            token: String::new(),
            version: String::new(),
            evidence: Vec::new(),
            step: "original quorum failure".into(),
            workspace: None,
            foreign: false,
            permission_vm: false,
            restart_ct: None,
            node_recovery_pending: true,
        };
        node_recovery_diagnostics(&mut ctx, "outage-failed").await;
        assert_eq!(ctx.step, "original quorum failure");
        assert!(ctx.node_recovery_pending);
        for node in ["pve1", "pve2"] {
            let record = ctx
                .evidence
                .iter()
                .find(|e| e["node"] == node)
                .ok_or("survivor observation missing")?;
            assert!(record["observation"]["error"].is_string());
        }
        assert!(
            ctx.evidence
                .iter()
                .any(|e| e["node_recovery_diagnostics_error"].is_string())
        );
        // Cleanup still reaches the guarded CONT path; no process is signaled.
        let error = cleanup_case(&mut ctx)
            .await
            .err()
            .ok_or("cleanup did not attempt node recovery")?;
        assert!(error.to_string().contains("missing QEMU PID"));
        assert!(ctx.node_recovery_pending);
        assert_eq!(ctx.step, "original quorum failure");
        // A later successful write preserves earlier errors and partial evidence.
        std::fs::remove_file(&artifacts)?;
        persist_node_diagnostics(&mut ctx);
        let persisted: Value = serde_json::from_slice(&std::fs::read(artifacts.join("evidence.json"))?)?;
        assert_eq!(persisted, json!(ctx.evidence));
        // String redaction cannot invalidate the surrounding JSON.
        service::write_private(&ctx.profile().join("api-token"), b"\\")?;
        let mut observation = json!({"stdout":"a\\b", "password":"secret"});
        observation = sanitize(observation);
        redact_diagnostic(&ctx, &mut observation);
        assert_eq!(observation, json!({"stdout":"a[REDACTED]b", "password":"[REDACTED]"}));
        Ok(())
    }

    #[test]
    fn node_diagnostics_guest_commands_keep_errors_and_partial_evidence() -> Result<()> {
        let script = format!(
            r#"
import contextlib, io, json, subprocess, sys
from types import SimpleNamespace
from unittest.mock import patch
source = {source}
outputs = [
    FileNotFoundError('missing ping'),
    subprocess.TimeoutExpired(['pvesh'], 2, output=b'partial status', stderr=b'partial error'),
    SimpleNamespace(returncode=1, stdout='no quorum', stderr='command failed'),
] + [SimpleNamespace(returncode=0, stdout='observed', stderr='')] * 8
stream = io.StringIO()
with patch.object(sys, 'argv', ['diagnostics', 'pve1']), \
     patch('pathlib.Path.read_text', side_effect=OSError('unavailable proc')), \
     patch('subprocess.run', side_effect=outputs), contextlib.redirect_stdout(stream):
    exec(source)
records = [json.loads(line) for line in stream.getvalue().splitlines()]
assert len(records) == 12, records
assert records[0]['uptime']['error'] == 'unavailable proc'
assert 'missing ping' in records[1]['commands'][0]['error']
assert records[2]['commands'][0]['stdout'] == 'partial status'
assert records[2]['commands'][0]['stderr'] == 'partial error'
assert records[3]['commands'][0]['exit_code'] == 1
assert records[-1]['commands'][0]['stdout'] == 'observed'
"#,
            source = serde_json::to_string(include_str!("node-recovery-diagnostics.py"))?,
        );
        let output = std::process::Command::new("python3").args(["-c", &script]).output()?;
        assert!(output.status.success(), "{}", String::from_utf8_lossy(&output.stderr));
        Ok(())
    }
}
