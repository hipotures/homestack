//! Installer answers are separate from runtime configuration and retain user intent.

use std::os::unix::fs::PermissionsExt;

use hs_cli::install_config::Input;
use hs_cli::install_flow::{Answer, answered, apply_draft, load_draft, save_draft};
use hs_install::{Choice, Question, Settings};

type Result = std::result::Result<(), Box<dyn std::error::Error>>;

fn choices(option: &'static str, values: &[&str]) -> Question {
    Question {
        option,
        title: option.to_owned(),
        note: String::new(),
        multiple: true,
        choices: values
            .iter()
            .map(|value| Choice {
                value: (*value).to_owned(),
                detail: String::new(),
            })
            .collect(),
        defaults: vec![0],
    }
}

#[test]
fn install_draft_round_trip_preserves_order_empty_choices_and_typed_values() -> Result {
    let dir = tempfile::tempdir()?;
    let path = dir.path().join("install-draft.toml");
    let active = dir.path().join("client.toml");
    std::fs::write(&active, "active sentinel")?;
    let mut questions = vec![
        choices("--storages", &["slow", "fast"]),
        choices("--workspace-keys", &["key-a", "key-b"]),
        choices("config-network-prefix", &[""]),
    ];
    let answers = vec![
        Answer::Picked(vec![1, 0]),
        Answer::Picked(vec![]),
        Answer::Text("10.20.30".into()),
    ];
    save_draft(&path, &questions, &answers, Some("lab.example.net"));
    let draft_text = std::fs::read_to_string(&path)?;
    assert!(
        hs_config::client::ClientConfig::parse(&draft_text).is_err(),
        "a draft became a runtime configuration"
    );
    assert!(!draft_text.contains("192.0.2"));
    let draft = load_draft(&path);
    apply_draft(&mut questions, &draft);
    assert_eq!(questions[0].defaults, [1, 0]);
    assert!(questions[1].defaults.is_empty());
    assert_eq!(questions[2].choices[0].value, "10.20.30");
    assert_eq!(draft["server-domain"], ["lab.example.net"]);
    assert_eq!(
        answered(&Settings::default(), &questions, &answers).storages,
        ["fast", "slow"]
    );
    assert_eq!(std::fs::metadata(&path)?.permissions().mode() & 0o777, 0o600);
    assert_eq!(std::fs::read_to_string(active)?, "active sentinel");
    assert!(!path.with_extension("toml.backup").exists());
    Ok(())
}

#[test]
fn restoring_a_draft_drops_a_gold_choice_that_is_no_longer_offered() {
    let mut question = choices("--gold", &["101", "117"]);
    question.multiple = false;
    let draft = std::collections::BTreeMap::from([("gold".to_owned(), vec!["222".to_owned()])]);
    apply_draft(std::slice::from_mut(&mut question), &draft);
    assert!(question.defaults.is_empty());
}

#[test]
fn reconfiguration_loads_saved_values_while_fresh_input_has_no_network_placeholders() -> Result {
    let dir = tempfile::tempdir()?;
    let path = dir.path().join("config.toml");
    std::fs::write(
        &path,
        "[user]\nname = 'engineer'\n[network]\nprefix = '10.20.30'\ngateway = '10.20.30.1'\ndns = ['10.20.30.53']\n",
    )?;
    let configured = Input::load(&path)?.questions();
    assert_eq!(
        configured
            .iter()
            .find(|q| q.option == "config-user-name")
            .ok_or("user question")?
            .choices[0]
            .value,
        "engineer"
    );
    assert_eq!(
        configured
            .iter()
            .find(|q| q.option == "config-network-prefix")
            .ok_or("network question")?
            .choices[0]
            .value,
        "10.20.30"
    );
    let fresh = Input::load(&dir.path().join("missing.toml"))?.questions();
    for option in [
        "config-user-name",
        "config-network-prefix",
        "config-network-gateway",
        "config-network-dns",
    ] {
        assert!(
            fresh
                .iter()
                .find(|q| q.option == option)
                .ok_or("fresh question")?
                .choices[0]
                .value
                .is_empty(),
            "{option}"
        );
    }
    Ok(())
}

#[test]
fn workspace_keys_are_choices_and_resume_in_the_selected_order() -> Result {
    let home = tempfile::tempdir()?;
    let ssh = home.path().join(".ssh");
    std::fs::create_dir(&ssh)?;
    for name in ["a", "b", "c"] {
        std::fs::write(ssh.join(name), "private fixture")?;
        std::fs::write(
            ssh.join(format!("{name}.pub")),
            format!("ssh-ed25519 AAAA key-{name}\n"),
        )?;
    }
    let input = Input::default();
    let mut keys = input.key_question(home.path());
    assert_eq!(keys.choices.len(), 3);
    assert!(keys.defaults.is_empty());
    assert_eq!(keys.choices[2].detail, "key-c");
    let path = home.path().join("install-draft.toml");
    save_draft(&path, std::slice::from_ref(&keys), &[Answer::Picked(vec![2, 0])], None);
    apply_draft(std::slice::from_mut(&mut keys), &load_draft(&path));
    assert_eq!(keys.defaults, [2, 0]);
    Ok(())
}

#[test]
fn client_configuration_path_observes_current_xdg_location() {
    let home = std::path::Path::new("/desktop-home");
    for directory in ["/config-one", "/config-two"] {
        assert_eq!(
            hs_cli::config::default_path(home, Some(std::path::Path::new(directory))),
            std::path::Path::new(directory).join("homestack/client.toml")
        );
    }
}
