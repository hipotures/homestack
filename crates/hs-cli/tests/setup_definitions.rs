//! The setup's definitions (Python `test_setup.py::DefinitionTests`,
//! `AdditionalSafetyTests`): the defaults, `[setup]` merged over them, and
//! the definitions kept through `client.toml`.
#![allow(clippy::unwrap_used, clippy::expect_used)]

mod setup_fixture;

use std::path::PathBuf;

use hs_cli::setup::definitions::{ConfigFile, Params, SetupConfig, Validation, config_entries, defaults, parse_setup};
use hs_config::client::ClientConfig;
use setup_fixture::entry;
use toml::{Table, Value};

fn example_text() -> String {
    std::fs::read_to_string(PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("../../parity/fixtures/config.example.toml"))
        .unwrap()
}

/// `[setup]` of a Python `config.toml`, as the client keeps it.
fn setup_of(text: &str) -> SetupConfig {
    let legacy = hs_config::legacy::read(text).unwrap();
    parse_setup(legacy.client.setup.as_ref().unwrap()).unwrap()
}

/// `[setup]` written into `client.toml` and read back (the install keeps
/// it as written, which is the Rust form of Python's draft round trip).
fn through_client_toml(text: &str) -> SetupConfig {
    let legacy = hs_config::legacy::read(text).unwrap();
    let written = legacy.client.to_toml().unwrap();
    let read = ClientConfig::parse(&written).unwrap();
    parse_setup(read.setup.as_ref().unwrap()).unwrap()
}

/// A `[setup]` table written as TOML and read back.
fn round_trip(table: &Table) -> SetupConfig {
    let mut document = Table::new();
    document.insert("setup".into(), Value::Table(table.clone()));
    let text = toml::to_string(&document).unwrap();
    let parsed: Table = text.parse().unwrap();
    parse_setup(parsed["setup"].as_table().unwrap()).unwrap()
}

fn setup(text: &str) -> Table {
    text.parse().unwrap()
}

fn codex(setup: &SetupConfig) -> &hs_cli::setup::definitions::Application {
    setup.item("codex").unwrap().application().unwrap()
}

#[test]
fn builtin_codex_has_no_implicit_configuration_or_validator() {
    for setup_cfg in [
        SetupConfig::default(),
        parse_setup(&Table::new()).unwrap(),
        parse_setup(&setup("[[items]]\nid = \"codex\"\nlabel = \"Local Codex\"\n")).unwrap(),
    ] {
        let application = setup_cfg.item("codex").unwrap();
        assert!(application.application().unwrap().config_files.is_empty());
        assert!(application.application().unwrap().validation.is_none());
        assert!(config_entries(application).is_empty());
    }
}

#[test]
fn runtime_toml_extends_builtin_codex_by_id_only() {
    let setup_cfg = parse_setup(&setup(
        r#"
[[items]]
id = "codex"
[items.validation]
command = "codex doctor --json"
type = "json-path"
path = ["checks", "config.load", "status"]
accepted = ["ok", "warning"]
[[items.config_files]]
id = "config"
path = "~/.codex/config.toml"
format = "toml"
[items.config_files.values.agents]
enabled = true
"#,
    ))
    .unwrap();
    let application = codex(&setup_cfg);
    assert_eq!(application.command, entry("codex").application().unwrap().command);
    assert_eq!(application.config_files[0].values, setup("[agents]\nenabled = true\n"));
    assert_eq!(application.validation.as_ref().unwrap().command, "codex doctor --json");
}

#[test]
fn example_codex_configuration_and_validator_round_trip_normally_and_in_draft() {
    let example = setup_of(&example_text());
    let application = codex(&example);
    let values = setup(
        r#"
approvals_reviewer = "user"
approval_policy = "never"
sandbox_mode = "danger-full-access"
[tui]
status_line = ["model-with-reasoning", "current-dir", "project-name", "hostname", "five-hour-limit", "weekly-limit", "context-used", "git-branch", "branch-changes"]
status_line_use_colors = true
[features]
multi_agent = true
multi_agent_v2 = true
[features.context_management]
experimental_mode = true
"#,
    );
    assert_eq!(
        application.config_files,
        vec![ConfigFile {
            id: "config".into(),
            path: "~/.codex/config.toml".into(),
            format: "toml".into(),
            values,
        }]
    );
    assert_eq!(
        application.validation,
        Some(Validation {
            command: "codex doctor --json".into(),
            kind: "json-path".into(),
            path: vec!["checks".into(), "config.load".into(), "status".into()],
            accepted: vec!["ok".into(), "warning".into()],
        })
    );
    assert_eq!(through_client_toml(&example_text()), example);
}

#[test]
fn removing_an_explicit_option_does_not_restore_it_on_round_trip() {
    let text = example_text().replacen("approval_policy = \"never\"\n", "", 1);
    let cfg = setup_of(&text);
    assert!(!codex(&cfg).config_files[0].values.contains_key("approval_policy"));
    let restored = through_client_toml(&text);
    assert!(!codex(&restored).config_files[0].values.contains_key("approval_policy"));
}

#[test]
fn new_nested_runtime_values_survive_normal_and_draft_round_trips() {
    let marker = "sandbox_mode = \"danger-full-access\"\n\n";
    let additions = format!(
        "{marker}[setup.items.config_files.values.agents]\nenabled = true\nmax_concurrent_threads_per_session = 12\n\n"
    );
    let text = example_text().replacen(marker, &additions, 1);
    let cfg = setup_of(&text);
    let agents = codex(&cfg).config_files[0].values["agents"].as_table().unwrap();
    assert_eq!(agents["enabled"], Value::Boolean(true));
    assert_eq!(agents["max_concurrent_threads_per_session"], Value::Integer(12));
    assert_eq!(through_client_toml(&text), cfg);
}

#[test]
fn structured_definitions_and_optional_validator_round_trip_through_drafts() {
    let table = setup(
        r#"
[[items]]
id = "codex"
validation = false
[[items.config_files]]
id = "json"
path = "~/app.json"
format = "json"
[items.config_files.values."foo.bar"]
array = [1, true, "x"]
[[items.config_files]]
id = "yaml"
path = "~/app.yaml"
format = "yaml"
[items.config_files.values]
enabled = true
empty = []
"#,
    );
    let setup_cfg = parse_setup(&table).unwrap();
    assert_eq!(round_trip(&table), setup_cfg);
    let application = setup_cfg.item("codex").unwrap();
    assert!(application.application().unwrap().validation.is_none());
    let keys: Vec<Vec<String>> = config_entries(application)
        .into_iter()
        .map(|leaf| match leaf.params {
            Params::Structured { key, .. } => key,
            _ => Vec::new(),
        })
        .collect();
    assert!(keys.contains(&vec!["foo.bar".to_owned(), "array".to_owned()]));
}

#[test]
fn defaults_and_override_roundtrip() {
    let table = setup("[[items]]\nid = \"codex\"\nlabel = \"My Codex\"\n");
    let cfg = parse_setup(&table).unwrap();
    assert_eq!(round_trip(&table), cfg);
    assert_eq!(cfg.items.iter().filter(|e| e.id == "codex").count(), 1);
    let mut ids: Vec<&str> = cfg.items.iter().map(|e| e.id.as_str()).collect();
    ids.sort_unstable();
    let mut expected = vec![
        "bash",
        "zsh",
        "fish",
        "nu",
        "codex",
        "claude",
        "herdr",
        "opencode",
        "pi",
        "opencode-v2",
        "hermes",
        "rust",
        "bk",
    ];
    expected.sort_unstable();
    assert_eq!(ids, expected);
    assert_eq!(cfg.item("codex").unwrap().label, "My Codex");
}

#[test]
fn changed_command_loses_inherited_safety_claim() {
    let cfg = parse_setup(&setup("[[items]]\nid = \"codex\"\ncommand = \"custom SECRET\"\n")).unwrap();
    let p = codex(&cfg);
    assert_eq!(p.command, "custom SECRET");
    assert_eq!(p.interaction, "interactive");
    assert!(p.check.is_empty());
    assert!(p.non_interactive.is_empty());
}

#[test]
fn application_backup_paths_round_trip_and_validate_below_home() {
    let table = setup("[[items]]\nid = \"codex\"\nbackup_paths = [\"~/.config/codex/\", \"~/.codex/settings.json\"]\n");
    let cfg = parse_setup(&table).unwrap();
    assert_eq!(
        codex(&cfg).backup_paths,
        vec!["~/.config/codex/", "~/.codex/settings.json"]
    );
    assert_eq!(round_trip(&table), cfg);
    assert!(parse_setup(&setup("[[items]]\nid = \"codex\"\nbackup_paths = [\"/tmp/outside\"]\n")).is_err());
}

#[test]
fn explicit_setup_file_round_trip() {
    let table = setup(
        "[[items]]\nid = \"notes\"\ngroup = \"files\"\nhandler = \"file\"\nlabel = \"Notes\"\ndescription = \"Explicit notes file\"\npath = \"~/notes\"\n",
    );
    let cfg = parse_setup(&table).unwrap();
    assert_eq!(round_trip(&table), cfg);
    assert_eq!(
        cfg.item("notes").unwrap().params,
        Params::File {
            path: "~/notes".into(),
            application: None
        }
    );
}

#[test]
fn draft_preserves_setup_and_repo_without_reconstruction() {
    let text = example_text().replacen(
        "id = \"codex\"\ngroup = \"app\"\nhandler = \"application\"\nlabel = \"Codex\"\n",
        "id = \"codex\"\ngroup = \"app\"\nhandler = \"application\"\nlabel = \"Custom label\"\n",
        1,
    );
    assert!(text.contains("Custom label"));
    let legacy = hs_config::legacy::read(&text).unwrap();
    let written = legacy.client.to_toml().unwrap();
    let read = ClientConfig::parse(&written).unwrap();
    assert_eq!(read.setup, legacy.client.setup);
    assert_eq!(read.repo, legacy.client.repo);
    assert_eq!(parse_setup(read.setup.as_ref().unwrap()).unwrap(), setup_of(&text));
}

#[test]
fn invalid_typed_definitions() {
    for text in [
        "items = \"bad\"",
        "groups = \"bad\"",
        "[[items]]\nid = \"codex\"\ninterpreter = \"sh\"",
        "[[items]]\nid = \"codex\"\nprerequisites = [\"curl; exit 0\"]",
        "[[items]]\nid = \"codex\"\nunknown = true",
        "[[items]]\nid = \"codex\"\n[[items]]\nid = \"codex\"",
        "[[items]]\nid = \"bash\"\nprofile = \"powershell\"",
    ] {
        assert!(parse_setup(&setup(text)).is_err(), "{text}");
    }
}

#[test]
fn reserved_ids_and_typed_arrays_fail_as_application_errors() {
    for text in [
        "[[groups]]\nid = \"bash\"\nlabel = \"Other\"",
        "[[groups]]\nid = \"c\"\nlabel = \"Other\"",
        "[[items]]\nid = \"cred\"\nhandler = \"backup\"\ngroup = \"backup\"\nlabel = \"X\"\ndescription = \"\"",
        "[[items]]\nid = \"codex\"\ninteraction = []",
        "[[items]]\nid = \"bash\"\nprofile = []",
        "[[items]]\nid = \"codex\"\nbin_dirs = [\"~/a:/etc\"]",
        "[[items]]\nid = \"codex\"\nprerequisite_checks = [\"a\\u0000b\"]",
    ] {
        assert!(parse_setup(&setup(text)).is_err(), "{text}");
    }
}

#[test]
fn defaults_are_python_s() {
    let ids: Vec<String> = defaults().into_iter().map(|e| e.id).collect();
    assert_eq!(
        ids,
        [
            "bash",
            "zsh",
            "fish",
            "nu",
            "codex",
            "claude",
            "herdr",
            "opencode",
            "opencode-v2",
            "pi",
            "hermes",
            "rust",
            "bk"
        ]
    );
}
