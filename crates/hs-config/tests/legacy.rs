//! The Python `config.toml` read like `config.load_config`, and split into
//! `server.toml` and `client.toml`. Ports of `python/tests/test_config.py`.

use std::path::{Path, PathBuf};

use hs_config::client::ClientConfig;
use hs_config::legacy::{self, Legacy};
use hs_config::paths::default_config_path;
use hs_config::split::split;
use hs_model::config::{Config, ConfigError};

type TestResult = Result<(), Box<dyn std::error::Error>>;

fn example_path() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR")).join("../../python/config.example.toml")
}

fn example() -> String {
    match std::fs::read_to_string(example_path()) {
        Ok(text) => text,
        Err(error) => panic!("{}: {error}", example_path().display()),
    }
}

fn edited(from: &str, to: &str) -> String {
    let text = example();
    assert!(text.contains(from), "example lacks {from:?}");
    text.replacen(from, to, 1)
}

fn rejected(text: &str) -> ConfigError {
    match legacy::read(text) {
        Ok(read) => panic!("accepted: {:?}", read.server),
        Err(error) => error,
    }
}

/// Port of `ConfigTests::test_default_config_uses_xdg_config_home`.
#[test]
fn default_config_uses_xdg_config_home() {
    let env = |name: &str| match name {
        "XDG_CONFIG_HOME" => Some("/tmp/example-xdg".to_owned()),
        "HOME" => Some("/tmp/example-home".to_owned()),
        _ => None,
    };
    assert_eq!(
        default_config_path(env),
        Some(PathBuf::from("/tmp/example-xdg/homestack/config.toml"))
    );
}

/// Port of `ConfigTests::test_default_config_falls_back_to_home_config`.
#[test]
fn default_config_falls_back_to_home_config() {
    let env = |name: &str| (name == "HOME").then(|| "/tmp/example-home".to_owned());
    assert_eq!(
        default_config_path(env),
        Some(PathBuf::from("/tmp/example-home/.config/homestack/config.toml"))
    );
    let empty_xdg = |name: &str| match name {
        "XDG_CONFIG_HOME" => Some(String::new()),
        "HOME" => Some("/tmp/example-home".to_owned()),
        _ => None,
    };
    assert_eq!(
        default_config_path(empty_xdg),
        Some(PathBuf::from("/tmp/example-home/.config/homestack/config.toml"))
    );
    let tilde_xdg = |name: &str| match name {
        "XDG_CONFIG_HOME" => Some("~/xdg".to_owned()),
        "HOME" => Some("/tmp/example-home".to_owned()),
        _ => None,
    };
    assert_eq!(
        default_config_path(tilde_xdg),
        Some(PathBuf::from("/tmp/example-home/xdg/homestack/config.toml"))
    );
}

/// Port of `ConfigTests::test_repository_config_loads_workspace_ssh_section`.
#[test]
fn repository_config_loads_workspace_ssh_section() -> TestResult {
    let Legacy { server, client, .. } = legacy::read(&example())?;
    assert_eq!(client.workspace_ssh.user, "developer");
    assert_eq!(client.workspace_ssh.identity_files, ["~/.ssh/example-hardware-key"]);
    assert!(client.workspace_ssh.identities_only);
    assert_eq!(client.workspace_ssh.log_level, "FATAL");
    assert_eq!(server.repo_owner.as_deref(), Some("example-owner"));
    assert_eq!(client.repo.owner.as_deref(), Some("example-owner"));
    assert_eq!(client.repo.checkout_root, "~/DEV");
    Ok(())
}

/// Port of `ConfigTests::test_workspace_ssh_validation_matches_ssh_config_contract`.
#[test]
fn workspace_ssh_validation_matches_ssh_config_contract() {
    let cases = [
        (
            "missing table",
            edited("[workspace_ssh]", "[removed_workspace_ssh]"),
            "workspace_ssh",
        ),
        (
            "empty user",
            edited("user = \"developer\"", "user = \"\""),
            "workspace_ssh.user",
        ),
        (
            "mismatched user",
            edited("user = \"developer\"", "user = \"operator\""),
            "workspace_ssh.user",
        ),
        (
            "whitespace identity",
            edited("~/.ssh/example-hardware-key", "~/.ssh/example hardware key"),
            "workspace_ssh.identity_files",
        ),
        (
            "duplicate identity",
            edited(
                "\"~/.ssh/example-hardware-key\"",
                "\"~/.ssh/example-hardware-key\", \"~/.ssh/example-hardware-key\"",
            ),
            "workspace_ssh.identity_files",
        ),
        (
            "nonboolean identities only",
            edited("identities_only = true", "identities_only = \"yes\""),
            "workspace_ssh.identities_only",
        ),
        (
            "unsupported log level",
            edited("log_level = \"FATAL\"", "log_level = \"TRACE\""),
            "workspace_ssh.log_level",
        ),
    ];
    for (label, text, field) in cases {
        assert_eq!(rejected(&text).field, field, "{label}");
    }
}

/// Port of `ConfigTests::test_repo_config_validation`.
#[test]
fn repo_config_validation() {
    let cases = [
        (
            edited(
                "owner = \"example-owner\"",
                "owner = \"https://github.com/example-owner\"",
            ),
            "repo.owner",
        ),
        (
            edited("checkout_root = \"~/DEV\"", "checkout_root = \"/tmp/DEV\""),
            "repo.checkout_root",
        ),
        (
            edited("checkout_root = \"~/DEV\"", "checkout_root = \"~/DEV/../other\""),
            "repo.checkout_root",
        ),
    ];
    for (text, field) in cases {
        assert_eq!(rejected(&text).field, field);
    }
}

/// Port of `ConfigTests::test_unsupported_transport_fails_clearly`.
#[test]
fn unsupported_transport_fails_clearly() {
    let error = rejected(&edited("type = \"herdr\"", "type = \"ssh\""));
    assert_eq!(error.field, "transport.type");
    assert!(error.message.contains("Unsupported transport type"), "{error}");
}

/// Port of `ConfigTests::test_root_and_home_must_use_different_disk_slots`.
#[test]
fn root_and_home_must_use_different_disk_slots() {
    let error = rejected(&edited("[home]\ndisk = \"scsi1\"", "[home]\ndisk = \"scsi0\""));
    assert_eq!(error.field, "root.disk");
    assert!(error.message.contains("different slots"), "{error}");
}

/// Port of `ConfigTests::test_obsolete_sync_section_is_rejected`.
#[test]
fn obsolete_sync_section_is_rejected() {
    let error = rejected(&format!("{}\n[sync]\npaths = [\"~/obsolete\"]\n", example()));
    assert_eq!(error.field, "sync");
    assert!(
        error.message.contains("[sync]") && error.message.contains("not supported"),
        "{error}"
    );
}

#[test]
fn an_install_draft_is_rejected_with_the_resume_hint() {
    let error = rejected("version = 1\ninstall_draft = true\n\n[install]\ncompleted = [\"transport\"]\n");
    assert_eq!(error.field, "install_draft");
    assert!(error.message.contains("configuration is incomplete"), "{error}");
}

#[test]
fn unknown_sections_are_ignored_like_python_and_reported_as_dropped() -> TestResult {
    let read = legacy::read(&format!("{}\n[runtime]\nroot = \"/root/.homestack\"\n", example()))?;
    assert_eq!(read.dropped, ["control", "runtime", "transport"]);
    Ok(())
}

/// Port of `ConfigTests::test_generated_toml_round_trips_through_normal_loader`:
/// the files `split` writes load through the normal loaders and keep every
/// value, including the setup catalog.
#[test]
fn generated_toml_round_trips_through_normal_loader() -> TestResult {
    let text = format!(
        "{}\n[sounds]\nenabled = false\nrepeat_seconds = 4\n",
        edited(
            "fido_devices = [\"yubikey\", \"trezor\"]",
            "fido_devices = [\"YubiKey\", \"1050:0407\", \"yubikey\"]"
        )
        .replace("# fido_devices", "fido_devices")
    );
    let read = legacy::read(&text)?;
    let files = split(&text)?;
    assert_eq!(Config::parse(&files.server_toml)?, read.server);
    let client = ClientConfig::parse(&files.client_toml)?;
    assert_eq!(client, read.client);
    assert_eq!(client.workspace_ssh.fido_devices, ["yubikey", "1050:0407"]);
    assert!(
        client
            .setup
            .is_some_and(|setup| setup.to_string().contains("opencode-v2"))
    );
    assert!(client.sounds.is_some());
    for key in ["install_draft", "[transport", "[control", "[sync"] {
        assert!(
            !files.server_toml.contains(key) && !files.client_toml.contains(key),
            "{key}"
        );
    }
    assert!(!files.server_toml.contains("[workspace_ssh]"));
    assert!(!files.client_toml.contains("[node]"));
    Ok(())
}

#[test]
fn the_client_file_is_strict() -> TestResult {
    let files = split(&example())?;
    let error = match ClientConfig::parse(&format!("{}\n[node]\nname = \"x\"\n", files.client_toml)) {
        Ok(_) => return Err("accepted a server section in client.toml".into()),
        Err(error) => error,
    };
    assert_eq!(error.field, "node");
    Ok(())
}

/// `[server]` of the Rust client (phase 13, decision 8): HTTPS only.
#[test]
fn the_server_section_takes_only_https_urls() -> TestResult {
    let urls = vec![
        "https://api1.hs.example:443".to_owned(),
        "https://api2.hs.example".to_owned(),
    ];
    let files = hs_config::split::split_with_server(&example(), &urls)?;
    let client = ClientConfig::parse(&files.client_toml)?;
    let server = client.server.ok_or("no [server]")?;
    assert_eq!(server.urls, urls);
    assert_eq!(server.token_file, hs_config::client::DEFAULT_TOKEN_FILE);
    assert_eq!(server.ca_file, None);
    assert!(split(&example())?.client_toml.find("[server]").is_none());

    let plain = hs_config::split::split_with_server(&example(), &["http://api1.hs.example".to_owned()]);
    let error = plain.err().ok_or("accepted an http:// URL")?;
    assert_eq!(error.field, "server.urls");
    assert!(error.to_string().contains("https://"), "{error}");

    let base = split(&example())?.client_toml;
    for (section, field) in [
        ("[server]\nurls = []\n", "server.urls"),
        ("[server]\nurls = [\"https://\"]\n", "server.urls"),
        ("[server]\nurls = [\"https://a\", \"https://a\"]\n", "server.urls"),
        (
            "[server]\nurls = [\"https://a\"]\ntoken_file = \"\"\n",
            "server.token_file",
        ),
        ("[server]\nurls = [\"https://a\"]\nport = 1\n", "server.port"),
    ] {
        let error = match ClientConfig::parse(&format!("{base}{section}")) {
            Ok(_) => return Err(format!("accepted {section:?}").into()),
            Err(error) => error,
        };
        assert_eq!(error.field, field, "{section:?}");
    }
    let with_ca = ClientConfig::parse(&format!(
        "{base}[server]\nurls = [\"https://a\"]\ntoken_file = \"~/t\"\nca_file = \"/c.pem\"\n"
    ))?;
    let server = with_ca.server.clone().ok_or("no [server]")?;
    assert_eq!(
        (server.token_file.as_str(), server.ca_file.as_deref()),
        ("~/t", Some("/c.pem"))
    );
    assert_eq!(ClientConfig::parse(&with_ca.to_toml()?)?, with_ca);
    Ok(())
}

/// Port of `test_fido.py::ConfigTests::test_fido_devices_are_parsed_deduplicated_and_round_trip`.
#[test]
fn fido_devices_are_parsed_deduplicated_and_round_trip() -> TestResult {
    let text = example();
    let section = text.find("[workspace_ssh]").ok_or("no [workspace_ssh]")?;
    let marker = section + text[section..].find("log_level =").ok_or("no log_level")?;
    let end = marker + text[marker..].find('\n').ok_or("no line end")? + 1;
    let patched = format!(
        "{}fido_devices = [\"YubiKey\", \"trezor\", \"yubikey\"]\n{}",
        &text[..end],
        &text[end..]
    );
    let loaded = legacy::read(&patched)?;
    assert_eq!(loaded.client.workspace_ssh.fido_devices, ["yubikey", "trezor"]);
    let again = ClientConfig::parse(&split(&patched)?.client_toml)?;
    assert_eq!(again.workspace_ssh.fido_devices, ["yubikey", "trezor"]);
    assert!(!split(&example())?.client_toml.contains("fido_devices"));
    let bad = format!("{}fido_devices = \"yubikey\"\n{}", &text[..end], &text[end..]);
    assert!(legacy::read(&bad).is_err());
    Ok(())
}
