//! L3 parity: every scenario in `scenarios/` through both
//! implementations against the same simulated cluster.
//!
//! Python runs the real CLI on `FixtureTransport` (needs `uv`); Rust runs the
//! server in-process. For each scenario this checks that
//!
//! - the Python command translation agrees with what the simulator saw,
//! - both implementations issued the same operations, apart from those the
//!   normalization rule drops,
//! - their outputs are equal after the scenario's normalization rule,
//! - both match the scenario's expectations.
//!
//! Set `HOMESTACK_PHASE` to run only the scenarios of one phase.

use std::path::{Path, PathBuf};

use hs_parity::normalize::Rules;
use hs_parity::scenario::{Parity, Scenario};
use hs_pve::{PveClientConfig, PveToken};
use hs_server::{ApiToken, ServerConfig};
use pve_sim::PveSim;
use serde_json::Value;

type TestResult = Result<(), Box<dyn std::error::Error>>;

const API_TOKEN: &str = "parity-api-token-0123456789abcdef01234567";

fn repo_root() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR")).join("../..")
}

fn scenario_files() -> Result<Vec<PathBuf>, std::io::Error> {
    let mut files: Vec<PathBuf> = std::fs::read_dir(repo_root().join("scenarios"))?
        .filter_map(Result::ok)
        .map(|entry| entry.path())
        .filter(|path| path.extension().is_some_and(|ext| ext == "toml"))
        .collect();
    files.sort();
    Ok(files)
}

struct PythonRun {
    output: Value,
    recorded_ops: Vec<String>,
}

async fn run_python(
    scenario: &Scenario,
    sim: &PveSim,
    args: &[String],
) -> Result<PythonRun, Box<dyn std::error::Error>> {
    let root = repo_root();
    let dir = tempfile::tempdir()?;
    let ca = dir.path().join("ca.pem");
    let token = dir.path().join("token");
    let record = dir.path().join("record.json");
    std::fs::write(&ca, sim.ca_pem())?;
    std::fs::write(&token, scenario.token(&scenario.homestack.python_token)?)?;
    let config = root.join(&scenario.homestack.config);
    let function = args.first().and_then(|first| first.strip_prefix('@'));
    let mut command = tokio::process::Command::new("uv");
    command
        .current_dir(root.join("python"))
        .env("PYTHONPATH", root.join("parity/python"))
        .env_remove("VIRTUAL_ENV")
        .args(["run", "--quiet", "python", "-m"])
        .arg(if function.is_some() {
            "homestack_parity.func_driver"
        } else {
            "homestack_parity.cli_driver"
        })
        .arg("--sim-url")
        .arg(sim.base_url())
        .arg("--ca-file")
        .arg(&ca)
        .arg("--token-file")
        .arg(&token)
        .arg("--record-out")
        .arg(&record);
    match function {
        Some(name) => command
            .arg("--config")
            .arg(&config)
            .arg("--")
            .arg(name)
            .args(args.iter().skip(1)),
        None => command.arg("--").arg("--config").arg(&config).args(args),
    };
    let output = command.output().await?;
    if !output.status.success() {
        return Err(format!(
            "Python run failed ({}):\n{}\n{}",
            output.status,
            String::from_utf8_lossy(&output.stdout),
            String::from_utf8_lossy(&output.stderr)
        )
        .into());
    }
    let recorded: Value = serde_json::from_str(&std::fs::read_to_string(&record)?)?;
    let recorded_ops = recorded["ops"]
        .as_array()
        .ok_or("record has no ops")?
        .iter()
        .filter_map(|op| op.as_str().map(str::to_owned))
        .collect();
    Ok(PythonRun {
        output: serde_json::from_slice(&output.stdout)?,
        recorded_ops,
    })
}

async fn run_rust(scenario: &Scenario, sim: &PveSim, request: &str) -> Result<Value, Box<dyn std::error::Error>> {
    let token = PveToken::parse(&scenario.token(&scenario.homestack.server_token)?)?;
    let mut pve = PveClientConfig::new(vec![sim.base_url().parse()?], token);
    pve.extra_ca_pem = Some(sim.ca_pem().to_owned());
    // The server gets the server part of the scenario's Python configuration.
    let python_config = std::fs::read_to_string(repo_root().join(&scenario.homestack.config))?;
    let server_toml = hs_config::split::split(&python_config)?.server_toml;
    if let Some(function) = request.strip_prefix('@') {
        let config = hs_model::config::Config::parse(&server_toml)?;
        return run_rust_function(hs_pve::PveClient::new(pve)?, &config, function).await;
    }
    if let Some(args) = request.strip_prefix("cli ") {
        return run_rust_cli(pve, &python_config, &server_toml, args).await;
    }
    let (method, path) = request.split_once(' ').ok_or("rust request must be `METHOD /path`")?;
    let state = tempfile::tempdir()?;
    std::fs::write(state.path().join("server.toml"), server_toml)?;
    let app = hs_server::app(ServerConfig {
        api_token: ApiToken::parse(API_TOKEN)?.into(),
        pve: pve.into(),
        config_file: state.path().join("server.toml"),
        instance: hs_server::Instance::default(),
        pools: hs_server::Pools::default(),
    })?;
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await?;
    let addr = listener.local_addr()?;
    let server = tokio::spawn(async move { axum::serve(listener, app).await });
    let response = reqwest::Client::new()
        .request(method.parse()?, format!("http://{addr}{path}"))
        .bearer_auth(API_TOKEN)
        .send()
        .await?;
    let status = response.status();
    let body: Value = response.json().await?;
    server.abort();
    drop(state);
    if !status.is_success() {
        return Err(format!("Rust request failed ({status}): {body}").into());
    }
    Ok(body)
}

/// What the Rust client printed.
#[derive(Clone, Default)]
struct Captured(std::sync::Arc<std::sync::Mutex<String>>);

impl hs_cli::system::Console for Captured {
    fn out(&mut self, text: &str) {
        if let Ok(mut out) = self.0.lock() {
            out.push_str(text);
        }
    }

    fn err(&mut self, _text: &str) {}

    fn ask(&mut self, _question: &str) -> Option<String> {
        None
    }

    fn secret(&mut self, _prompt: &str) -> Option<String> {
        None
    }
}

/// `homestack ARGS` of the Rust client (phase 13) against the server behind
/// HTTPS, with the client part of the scenario's Python configuration.
async fn run_rust_cli(
    pve: PveClientConfig,
    python_config: &str,
    server_toml: &str,
    args: &str,
) -> Result<Value, Box<dyn std::error::Error>> {
    let state = tempfile::tempdir()?;
    std::fs::write(state.path().join("server.toml"), server_toml)?;
    let app = hs_server::app(ServerConfig {
        api_token: ApiToken::parse(API_TOKEN)?.into(),
        pve: pve.into(),
        config_file: state.path().join("server.toml"),
        instance: hs_server::Instance::default(),
        pools: hs_server::Pools::default(),
    })?;
    let certs = pve_sim::tls::generate()?;
    let tls = axum_server::tls_rustls::RustlsConfig::from_pem(certs.chain_pem.into_bytes(), certs.key_pem.into_bytes())
        .await?;
    let listener = std::net::TcpListener::bind("127.0.0.1:0")?;
    listener.set_nonblocking(true)?;
    let url = format!("https://localhost:{}", listener.local_addr()?.port());
    let handle = axum_server::Handle::new();
    let server = axum_server::from_tcp_rustls(listener, tls)?
        .handle(handle.clone())
        .serve(app.into_make_service());
    tokio::spawn(server);

    let home = tempfile::tempdir()?;
    let dir = home.path().join(".config/homestack");
    std::fs::create_dir_all(&dir)?;
    let client_toml = hs_config::split::split_with_server(python_config, &[url])?.client_toml;
    let ca = dir.join("ca.pem");
    std::fs::write(&ca, &certs.ca_pem)?;
    std::fs::write(
        dir.join("client.toml"),
        client_toml.replace(
            "token_file = ",
            &format!("ca_file = {:?}\ntoken_file = ", ca.display().to_string()),
        ),
    )?;
    let token = dir.join("api-token");
    std::fs::write(&token, API_TOKEN)?;
    std::fs::set_permissions(&token, std::os::unix::fs::PermissionsExt::from_mode(0o600))?;

    let captured = Captured::default();
    let mut context = hs_cli::app::Context {
        home: home.path().to_owned(),
        config_home: None,
        state_home: None,
        stdin_tty: false,
        style: hs_cli::view::Style {
            color: false,
            width: 120,
        },
        system: std::sync::Arc::new(hs_cli::system::RealSystem::default()),
        console: Box::new(captured.clone()),
        poll: std::time::Duration::from_millis(10),
        servers: std::sync::Arc::new(hs_cli::servers::RealServers),
    };
    let argv: Vec<String> = args.split_whitespace().map(str::to_owned).collect();
    let code = hs_cli::app::run(&argv, &mut context).await;
    handle.shutdown();
    let out = captured.0.lock().map(|out| out.clone()).unwrap_or_default();
    if code != 0 {
        return Err(format!("homestack {args} exited {code}:\n{out}").into());
    }
    Ok(serde_json::from_str(&out)?)
}

/// Server functions that have no route of their own, called in-process the
/// way the operations of later phases call them:
///
/// - `verify-refresh VMID`: `hs_core::verify::verify_refreshed_guest` with
///   the VM's name, its `prefix.<vmid>` address and its home label, as
///   `func_driver.py verify-refresh` calls Python's.
/// - `workspace-keys [VMID]`: `hs_core::keys::workspace_authorized_keys`
///   with Gold VMID instead of the configured one.
async fn run_rust_function(
    pve: hs_pve::PveClient,
    config: &hs_model::config::Config,
    function: &str,
) -> Result<Value, Box<dyn std::error::Error>> {
    let words: Vec<&str> = function.split_whitespace().collect();
    match words.as_slice() {
        ["verify-refresh", vmid] => {
            let vmid: u32 = vmid.parse()?;
            let resources = hs_core::inventory::cluster_vm_resources(&pve).await?;
            let node = resources
                .iter()
                .find(|r| r.vmid == vmid)
                .map(|r| r.node.clone())
                .ok_or("no such VM")?;
            let vm = hs_core::inventory::vm_config(&pve, &node, vmid).await?;
            let expected = hs_core::verify::GuestExpectation {
                name: vm.get("name").cloned().unwrap_or_default(),
                address: format!("{}.{vmid}/{}", config.network.prefix, config.network.cidr),
                user: config.user.clone(),
                home_label: format!("HS_HOME_{vmid}"),
            };
            let timeouts = hs_core::verify::VerifyTimeouts::default();
            Ok(
                match hs_core::verify::verify_refreshed_guest(&pve, &node, vmid, &expected, &timeouts).await {
                    Ok(cloud_id) => serde_json::json!({ "ok": true, "cloud_id": cloud_id }),
                    Err(error) => serde_json::json!({ "ok": false, "error": error.to_string() }),
                },
            )
        }
        ["workspace-keys", rest @ ..] => {
            let mut config = config.clone();
            if let [gold] = rest {
                config.gold_vmid = Some(gold.parse()?);
            }
            Ok(match hs_core::keys::workspace_authorized_keys(&pve, &config).await {
                Ok(found) => serde_json::json!({ "ok": true, "keys": found.keys, "source": found.source }),
                Err(error) => serde_json::json!({ "ok": false, "error": error.to_string() }),
            })
        }
        _ => Err(format!("unknown Rust function @{function}").into()),
    }
}

fn op_strings(sim: &PveSim) -> Vec<String> {
    sim.ops().iter().map(ToString::to_string).collect()
}

async fn check(path: &Path, rules: &Rules) -> Result<usize, Box<dyn std::error::Error>> {
    let scenario = Scenario::parse(&std::fs::read_to_string(path)?)?;
    let sim = PveSim::start(scenario.pve.clone()).await?;
    for (index, parity) in scenario.parity.iter().enumerate() {
        let name = format!("{} #{} ({})", scenario.meta.name, index + 1, parity.rust);
        check_one(&scenario, &sim, parity, rules)
            .await
            .map_err(|error| format!("{name}: {error}"))?;
    }
    sim.shutdown().await;
    Ok(scenario.parity.len())
}

async fn check_one(scenario: &Scenario, sim: &PveSim, parity: &Parity, rules: &Rules) -> TestResult {
    let rule = rules.get(&parity.normalize)?;
    sim.clear_ops();
    let python = run_python(scenario, sim, &parity.python).await?;
    let python_ops = op_strings(sim);
    sim.clear_ops();
    let rust = run_rust(scenario, sim, &parity.rust).await?;
    let rust_ops = op_strings(sim);

    if python.recorded_ops != python_ops {
        return Err(format!(
            "Python command translation disagrees with the simulator:\n  translated {:?}\n  simulator  {:?}",
            python.recorded_ops, python_ops
        )
        .into());
    }
    let (python_kept, rust_kept) = (rule.python_ops(&python_ops), rule.rust_ops(&rust_ops));
    if python_kept != rust_kept {
        return Err(format!("operations differ:\n  Python {python_kept:#?}\n  Rust   {rust_kept:#?}").into());
    }
    let python_body = rule.python(&python.output)?;
    let rust_body = rule.rust(&rust);
    if python_body != rust_body {
        return Err(format!(
            "normalized outputs differ:\n  Python {}\n  Rust   {}",
            serde_json::to_string_pretty(&python_body)?,
            serde_json::to_string_pretty(&rust_body)?
        )
        .into());
    }
    if !parity.expect.ops.is_empty() && rust_ops != parity.expect.ops {
        return Err(format!("unexpected operations: {rust_ops:?}").into());
    }
    if let Some(expected) = &parity.expect.body
        && rust_body != serde_json::to_value(expected)?
    {
        return Err(format!("unexpected output: {rust_body}").into());
    }
    Ok(())
}

#[tokio::test]
#[ignore = "slow (owner, 2026-10-15): run with cargo test -- --ignored"]
async fn python_and_rust_agree_on_every_scenario() -> TestResult {
    let rules: Rules = toml::from_str(&std::fs::read_to_string(repo_root().join("parity/normalize.toml"))?)?;
    rules.validate()?;
    let phase = std::env::var("HOMESTACK_PHASE").ok();
    let mut checked = 0;
    for path in scenario_files()? {
        let scenario = Scenario::parse(&std::fs::read_to_string(&path)?)?;
        if phase.as_ref().is_some_and(|phase| *phase != scenario.meta.phase) {
            continue;
        }
        checked += check(&path, &rules)
            .await
            .map_err(|error| format!("{}: {error}", path.display()))?;
    }
    eprintln!("parity: {checked} comparison(s) checked");
    assert!(checked > 0 || phase.is_some(), "no scenarios found");
    Ok(())
}
