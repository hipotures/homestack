//! L3 parity for configuration validation: every case of
//! `parity/config-corpus.toml` through Python's `config.load_config` and the
//! Rust reader of the same file (`hs_config::legacy`). Both must accept the
//! same documents and reject the others for the same field, except where a
//! case records the difference and its reason.
//!
//! Needs `uv`. Set `HOMESTACK_PHASE` to skip it outside its phase.

use std::path::{Path, PathBuf};

use hs_parity::corpus::Corpus;
use serde::Deserialize;

type TestResult = Result<(), Box<dyn std::error::Error>>;

fn repo_root() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR")).join("../..")
}

#[derive(Deserialize)]
struct PythonVerdict {
    file: PathBuf,
    verdict: String,
    message: String,
}

async fn python_verdicts(files: &[PathBuf]) -> Result<Vec<PythonVerdict>, Box<dyn std::error::Error>> {
    let root = repo_root();
    let output = tokio::process::Command::new("uv")
        .current_dir(root.join("python"))
        .env("PYTHONPATH", root.join("parity/python"))
        .env_remove("VIRTUAL_ENV")
        .args(["run", "--quiet", "python", "-m", "homestack_parity.config_check"])
        .args(files)
        .output()
        .await?;
    if !output.status.success() {
        return Err(format!(
            "Python run failed ({}):\n{}",
            output.status,
            String::from_utf8_lossy(&output.stderr)
        )
        .into());
    }
    String::from_utf8(output.stdout)?
        .lines()
        .map(|line| serde_json::from_str(line).map_err(Into::into))
        .collect()
}

fn rust_verdict(text: &str) -> String {
    match hs_config::legacy::read(text) {
        Ok(_) => "accepted".to_owned(),
        Err(error) => error.field,
    }
}

#[tokio::test]
async fn python_and_rust_agree_on_every_configuration() -> TestResult {
    let corpus: Corpus = toml::from_str(&std::fs::read_to_string(repo_root().join("parity/config-corpus.toml"))?)?;
    corpus.validate()?;
    if std::env::var("HOMESTACK_PHASE").is_ok_and(|phase| phase != corpus.phase) {
        return Ok(());
    }
    let base = std::fs::read_to_string(repo_root().join(&corpus.base))?;
    let dir = tempfile::tempdir()?;
    let mut files = Vec::new();
    let mut texts = Vec::new();
    for (index, case) in corpus.cases.iter().enumerate() {
        let text = case.document(&base)?;
        let file = dir.path().join(format!("case-{index:03}.toml"));
        std::fs::write(&file, &text)?;
        files.push(file);
        texts.push(text);
    }

    let python = python_verdicts(&files).await?;
    assert_eq!(python.len(), corpus.cases.len(), "one Python verdict per case");
    let mut failures = Vec::new();
    for ((case, text), (file, python)) in corpus.cases.iter().zip(&texts).zip(files.iter().zip(&python)) {
        assert_eq!(&python.file, file);
        let rust = rust_verdict(text);
        if rust != case.expect {
            failures.push(format!("{}: Rust says {rust}, expected {}", case.name, case.expect));
        }
        if python.verdict != case.python_verdict() {
            failures.push(format!(
                "{}: Python says {} ({}), expected {}",
                case.name,
                python.verdict,
                python.message,
                case.python_verdict()
            ));
        }
    }
    assert!(
        failures.is_empty(),
        "{} of {} cases differ:\n{}",
        failures.len(),
        corpus.cases.len(),
        failures.join("\n")
    );
    eprintln!("config corpus: {} cases agree", corpus.cases.len());
    Ok(())
}
