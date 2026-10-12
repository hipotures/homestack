//! `hs-parity`: manifest checks for `scripts/check.sh`.
//!
//! ```text
//! hs-parity manifest-check MANIFEST PYTHON_IDS RUST_TEST_LIST
//! hs-parity manifest-phase MANIFEST PHASE
//! ```
//!
//! `PYTHON_IDS` is the output of `python -m homestack_parity.list_tests`,
//! `RUST_TEST_LIST` the output of `cargo test -- --list --format terse`.

use std::process::ExitCode;

use hs_parity::manifest::{Manifest, Status, parse_rust_test_list};

fn read(path: &str) -> Result<String, String> {
    std::fs::read_to_string(path).map_err(|error| format!("{path}: {error}"))
}

fn manifest(path: &str) -> Result<Manifest, String> {
    toml::from_str(&read(path)?).map_err(|error| format!("{path}: {error}"))
}

fn run(args: &[String]) -> Result<ExitCode, String> {
    match args {
        [command, manifest_path, python_ids, rust_list] if command == "manifest-check" => {
            let manifest = manifest(manifest_path)?;
            let python: Vec<String> = read(python_ids)?
                .lines()
                .filter(|l| !l.is_empty())
                .map(str::to_owned)
                .collect();
            let rust = parse_rust_test_list(&read(rust_list)?);
            let problems = manifest.check(&python, &rust);
            for problem in &problems {
                eprintln!("manifest: {problem}");
            }
            let count = |status| manifest.tests.iter().filter(|entry| entry.status == status).count();
            println!(
                "manifest: {} Python tests: {} ported, {} todo, {} excluded",
                manifest.tests.len(),
                count(Status::Ported),
                count(Status::Todo),
                count(Status::Excluded)
            );
            Ok(if problems.is_empty() {
                ExitCode::SUCCESS
            } else {
                ExitCode::FAILURE
            })
        }
        [command, manifest_path, phase] if command == "manifest-phase" => {
            for test in manifest(manifest_path)?.rust_tests_for_phase(phase) {
                println!("{test}");
            }
            Ok(ExitCode::SUCCESS)
        }
        _ => Err(
            "usage: hs-parity manifest-check MANIFEST PYTHON_IDS RUST_TEST_LIST | manifest-phase MANIFEST PHASE"
                .to_owned(),
        ),
    }
}

fn main() -> ExitCode {
    let args: Vec<String> = std::env::args().skip(1).collect();
    run(&args).unwrap_or_else(|error| {
        eprintln!("hs-parity: {error}");
        ExitCode::from(2)
    })
}
