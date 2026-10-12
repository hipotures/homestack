//! Run the simulator by hand: `pve-sim --scenario FILE [--listen ADDR] [--ca-out FILE]`.
//!
//! Prints one JSON line with the base URL, serves until Ctrl-C, then prints
//! the recorded operations to stderr.

use std::net::SocketAddr;
use std::path::PathBuf;
use std::process::ExitCode;

use pve_sim::{ClusterSpec, PveSim};

struct Args {
    scenario: PathBuf,
    listen: SocketAddr,
    ca_out: Option<PathBuf>,
}

fn parse_args() -> Result<Args, String> {
    let mut scenario = None;
    let mut listen = SocketAddr::from(([127, 0, 0, 1], 0));
    let mut ca_out = None;
    let mut args = std::env::args().skip(1);
    while let Some(arg) = args.next() {
        let mut value = || args.next().ok_or_else(|| format!("{arg} needs a value"));
        match arg.as_str() {
            "--scenario" => scenario = Some(PathBuf::from(value()?)),
            "--listen" => {
                listen = value()?
                    .parse()
                    .map_err(|_| "--listen needs an address like 127.0.0.1:8006")?
            }
            "--ca-out" => ca_out = Some(PathBuf::from(value()?)),
            other => return Err(format!("unknown argument {other:?}")),
        }
    }
    Ok(Args {
        scenario: scenario.ok_or("--scenario is required")?,
        listen,
        ca_out,
    })
}

#[tokio::main]
async fn main() -> ExitCode {
    tracing_subscriber::fmt().with_writer(std::io::stderr).init();
    match run().await {
        Ok(()) => ExitCode::SUCCESS,
        Err(error) => {
            eprintln!("pve-sim: {error}");
            ExitCode::from(2)
        }
    }
}

async fn run() -> Result<(), String> {
    let args = parse_args()?;
    let text =
        std::fs::read_to_string(&args.scenario).map_err(|error| format!("{}: {error}", args.scenario.display()))?;
    let spec = ClusterSpec::from_scenario(&text)?;
    let sim = PveSim::start_on(spec, args.listen)
        .await
        .map_err(|error| error.to_string())?;
    if let Some(path) = &args.ca_out {
        std::fs::write(path, sim.ca_pem()).map_err(|error| format!("{}: {error}", path.display()))?;
    }
    println!("{}", serde_json::json!({ "url": sim.base_url() }));
    tokio::signal::ctrl_c().await.map_err(|error| error.to_string())?;
    for op in sim.ops() {
        eprintln!("{op}");
    }
    sim.shutdown().await;
    Ok(())
}
