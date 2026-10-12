//! L3 parity: every scenario in `rust/scenarios/` through both
//! implementations against the same simulated cluster.
//!
//! Python runs the real CLI on `FixtureTransport` (needs `uv`); Rust runs the
//! server in-process. For each scenario this checks that
//!
//! - the Python command translation agrees with what the simulator saw,
//! - both implementations issued the same operations,
//! - their outputs are equal after the scenario's normalization rule,
//! - both match the scenario's expectations.
//!
//! Set `HOMESTACK_PHASE` to run only the scenarios of one phase.

use std::path::{Path, PathBuf};

use hs_parity::normalize::Rules;
use hs_parity::scenario::Scenario;
use hs_pve::{PveClientConfig, PveToken};
use hs_server::{ApiToken, ServerConfig};
use pve_sim::PveSim;
use serde_json::Value;

type TestResult = Result<(), Box<dyn std::error::Error>>;

const API_TOKEN: &str = "parity-api-token-0123456789abcdef01234567";

fn repo_root() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR")).join("../../..")
}

fn scenario_files() -> Result<Vec<PathBuf>, std::io::Error> {
    let mut files: Vec<PathBuf> = std::fs::read_dir(repo_root().join("rust/scenarios"))?
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
    let output = tokio::process::Command::new("uv")
        .current_dir(&root)
        .env("PYTHONPATH", root.join("rust/parity/python"))
        .env_remove("VIRTUAL_ENV")
        .args([
            "run",
            "--quiet",
            "python",
            "-m",
            "homestack_parity.cli_driver",
            "--sim-url",
            &sim.base_url(),
        ])
        .arg("--ca-file")
        .arg(&ca)
        .arg("--token-file")
        .arg(&token)
        .arg("--record-out")
        .arg(&record)
        .arg("--")
        .arg("--config")
        .arg(root.join(&scenario.homestack.config))
        .args(args)
        .output()
        .await?;
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
    let (method, path) = request.split_once(' ').ok_or("rust request must be `METHOD /path`")?;
    let token = PveToken::parse(&scenario.token(&scenario.homestack.server_token)?)?;
    let mut pve = PveClientConfig::new(vec![sim.base_url().parse()?], token);
    pve.extra_ca_pem = Some(sim.ca_pem().to_owned());
    let app = hs_server::app(ServerConfig {
        api_token: ApiToken::parse(API_TOKEN)?,
        pve,
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
    if !status.is_success() {
        return Err(format!("Rust request failed ({status}): {body}").into());
    }
    Ok(body)
}

fn op_strings(sim: &PveSim) -> Vec<String> {
    sim.ops().iter().map(ToString::to_string).collect()
}

async fn check(path: &Path, rules: &Rules) -> TestResult {
    let scenario = Scenario::parse(&std::fs::read_to_string(path)?)?;
    let Some(parity) = &scenario.parity else { return Ok(()) };
    let rule = rules.get(&parity.normalize)?;
    let name = &scenario.meta.name;
    let sim = PveSim::start(scenario.pve.clone()).await?;

    let python = run_python(&scenario, &sim, &parity.python).await?;
    let python_ops = op_strings(&sim);
    sim.clear_ops();
    let rust = run_rust(&scenario, &sim, &parity.rust).await?;
    let rust_ops = op_strings(&sim);
    sim.shutdown().await;

    assert_eq!(
        python.recorded_ops, python_ops,
        "{name}: Python command translation disagrees with the simulator"
    );
    assert_eq!(python_ops, rust_ops, "{name}: operations differ");
    let python_body = rule.python(&python.output)?;
    let rust_body = rule.rust(&rust);
    assert_eq!(python_body, rust_body, "{name}: normalized outputs differ");
    if !parity.expect.ops.is_empty() {
        assert_eq!(rust_ops, parity.expect.ops, "{name}: unexpected operations");
    }
    if let Some(expected) = &parity.expect.body {
        assert_eq!(rust_body, serde_json::to_value(expected)?, "{name}: unexpected output");
    }
    Ok(())
}

#[tokio::test]
async fn python_and_rust_agree_on_every_scenario() -> TestResult {
    let rules: Rules = toml::from_str(&std::fs::read_to_string(
        repo_root().join("rust/parity/normalize.toml"),
    )?)?;
    rules.validate()?;
    let phase = std::env::var("HOMESTACK_PHASE").ok();
    let mut checked = 0;
    for path in scenario_files()? {
        let scenario = Scenario::parse(&std::fs::read_to_string(&path)?)?;
        if phase.as_ref().is_some_and(|phase| *phase != scenario.meta.phase) {
            continue;
        }
        check(&path, &rules)
            .await
            .map_err(|error| format!("{}: {error}", path.display()))?;
        checked += 1;
    }
    eprintln!("parity: {checked} scenario(s) checked");
    assert!(checked > 0 || phase.is_some(), "no scenarios found");
    Ok(())
}
