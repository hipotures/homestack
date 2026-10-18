//! Configuration validation against frozen verdicts from the final Python CLI.

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};

use hs_parity::corpus::Corpus;

type TestResult = Result<(), Box<dyn std::error::Error>>;

fn repo_root() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR")).join("../..")
}

fn rust_verdict(text: &str) -> String {
    match hs_config::legacy::read(text) {
        Ok(_) => "accepted".to_owned(),
        Err(error) => error.field,
    }
}

#[test]
fn frozen_python_and_rust_agree_on_every_configuration() -> TestResult {
    let corpus: Corpus = toml::from_str(&std::fs::read_to_string(repo_root().join("parity/config-corpus.toml"))?)?;
    corpus.validate()?;
    if std::env::var("HOMESTACK_PHASE").is_ok_and(|phase| phase != corpus.phase) {
        return Ok(());
    }
    let base = std::fs::read_to_string(repo_root().join(&corpus.base))?;
    let reference: BTreeMap<String, String> =
        serde_json::from_str(include_str!("../../../parity/fixtures/config-verdicts.json"))?;
    assert_eq!(reference.len(), corpus.cases.len(), "one frozen verdict per case");
    for case in &corpus.cases {
        assert_eq!(rust_verdict(&case.document(&base)?), case.expect, "{}", case.name);
        assert_eq!(
            reference.get(&case.name).map(String::as_str),
            Some(case.python_verdict()),
            "{}",
            case.name
        );
    }
    Ok(())
}
