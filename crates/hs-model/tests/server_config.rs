//! `server.toml`: the part of the Python `config.toml` the server owns
//! (`docs/migration/api-mapping.md`, 8.1), validated like `config.load_config`.

use hs_model::config::{Config, ConfigError, config_version};

const EXAMPLE: &str = r#"version = 1

[node]
name = "pve-example-1"
gold_vmid = 100

[storage_layouts]
homestack-storage-1 = ["example-storage"]
homestack-storage-2 = ["example-storage", "second-storage"]

[root]
disk = "scsi0"

[home]
disk = "scsi1"
default_size = "20G"

[network]
prefix = "192.0.2"
cidr = 24
gateway = "192.0.2.1"
dns = ["192.0.2.53"]

[cloud_init]
snippet_storage = "local"
snippet_dir = "/var/lib/vz/snippets"

[user]
name = "developer"
uid = 1000
gid = 1000

[repo]
owner = "example-owner"
"#;

fn edited(from: &str, to: &str) -> String {
    assert!(EXAMPLE.contains(from), "fixture lacks {from:?}");
    EXAMPLE.replacen(from, to, 1)
}

fn rejected(text: &str) -> ConfigError {
    match Config::parse(text) {
        Ok(config) => panic!("accepted: {config:?}"),
        Err(error) => error,
    }
}

#[test]
fn the_example_loads_with_the_values_python_derives() -> Result<(), ConfigError> {
    let config = Config::parse(EXAMPLE)?;
    assert_eq!(config.node.as_str(), "pve-example-1");
    assert_eq!(config.gold_vmid, Some(100));
    assert_eq!(config.active_layout(), ["example-storage"]);
    assert_eq!(config.default_storage(), "example-storage");
    assert_eq!(config.root_disk, "scsi0");
    assert_eq!(config.home_disk, "scsi1");
    assert_eq!(config.default_home_size, "20G");
    assert_eq!(config.network.cidr, 24);
    assert_eq!(config.network.dns, ["192.0.2.53"]);
    assert_eq!(config.cloud_init.snippet_dir, "/var/lib/vz/snippets");
    assert_eq!(
        (config.user.name.as_str(), config.user.uid, config.user.gid),
        ("developer", 1000, 1000)
    );
    assert_eq!(config.repo_owner.as_deref(), Some("example-owner"));
    Ok(())
}

#[test]
fn serialized_config_round_trips() -> Result<(), ConfigError> {
    let config = Config::parse(EXAMPLE)?;
    let text = config.to_toml();
    assert_eq!(Config::parse(&text)?, config);
    assert_eq!(text, config.to_toml());
    Ok(())
}

#[test]
fn the_version_is_the_sha256_of_the_text() {
    assert_eq!(
        config_version("abc"),
        "ba7816bf8f01cfea414140de5dae2223b00361a396177a9cb410ff61f20015ad"
    );
    assert_ne!(
        config_version(EXAMPLE),
        config_version(&edited("gold_vmid = 100", "gold_vmid = 101"))
    );
}

/// Port of `test_config.py::ConfigTests::test_root_and_home_must_use_different_disk_slots`.
#[test]
fn root_and_home_must_use_different_disk_slots() {
    let error = rejected(&edited("[home]\ndisk = \"scsi1\"", "[home]\ndisk = \"scsi0\""));
    assert_eq!(error.field, "root.disk");
    assert!(error.message.contains("different slots"), "{error}");
}

/// Port of the `[repo] owner` part of `test_config.py::ConfigTests::test_repo_config_validation`.
#[test]
fn repo_owner_must_be_a_github_owner() -> Result<(), ConfigError> {
    let error = rejected(&edited(
        "owner = \"example-owner\"",
        "owner = \"https://github.com/example-owner\"",
    ));
    assert_eq!(error.field, "repo.owner");
    assert!(!error.message.contains("https://"), "input echoed: {error}");
    let without = Config::parse(&edited("owner = \"example-owner\"", "owner = \"\""))?;
    assert_eq!(without.repo_owner, None);
    assert_eq!(
        Config::parse(&edited("owner = \"example-owner\"", "owner = \" a-b \""))?
            .repo_owner
            .as_deref(),
        Some("a-b")
    );
    Ok(())
}

/// Port of `test_config.py::ConfigTests::test_obsolete_sync_section_is_rejected`.
#[test]
fn obsolete_sync_section_is_rejected() {
    let error = rejected(&format!("{EXAMPLE}\n[sync]\npaths = [\"~/obsolete\"]\n"));
    assert_eq!(error.field, "sync");
    assert!(
        error.message.contains("[sync]") && error.message.contains("not supported"),
        "{error}"
    );
}

#[test]
fn every_problem_names_its_field() {
    let cases = [
        ("version = 1", "version = 2", "version"),
        ("version = 1", "version = 1.0", "version"),
        ("dns = [\"192.0.2.53\"]", "dns = []", "network.dns"),
        ("dns = [\"192.0.2.53\"]", "dns = \"192.0.2.53\"", "network.dns"),
        ("name = \"pve-example-1\"", "name = \"pve-example\"", "node.name"),
        ("name = \"pve-example-1\"", "name = \"pve example 1\"", "node.name"),
        ("name = \"pve-example-1\"", "name = 1", "node.name"),
        (
            "name = \"pve-example-1\"",
            "name = \"pve-example-3\"",
            "storage_layouts",
        ),
        ("homestack-storage-2 =", "homestack-two =", "storage_layouts"),
        (
            "homestack-storage-1 = [\"example-storage\"]",
            "homestack-storage-1 = []",
            "storage_layouts.homestack-storage-1",
        ),
        (
            "homestack-storage-1 = [\"example-storage\"]",
            "homestack-storage-1 = [\"a\", \" a \"]",
            "storage_layouts.homestack-storage-1",
        ),
        (
            "homestack-storage-1 = [\"example-storage\"]",
            "homestack-storage-1 = [\" \"]",
            "storage_layouts.homestack-storage-1",
        ),
        ("gold_vmid = 100", "gold_vmid = \"100\"", "node.gold_vmid"),
        ("gold_vmid = 100", "gold_vmid = 99", "node.gold_vmid"),
        ("[root]\ndisk = \"scsi0\"", "[root]\ndisk = \" \"", "root.disk"),
        ("cidr = 24", "cidr = 33", "network.cidr"),
        ("uid = 1000", "uid = -1", "user.uid"),
        ("name = \"developer\"", "name = \" \"", "user.name"),
        (
            "snippet_storage = \"local\"",
            "snippet_storage = 1",
            "cloud_init.snippet_storage",
        ),
        ("[cloud_init]", "[cloud_init_old]", "cloud_init_old"),
        (
            "default_size = \"20G\"",
            "default_size = \"20G\"\nsize = 1",
            "home.size",
        ),
    ];
    for (from, to, field) in cases {
        let error = rejected(&edited(from, to));
        assert_eq!(error.field, field, "{to}: {error}");
    }
}

#[test]
fn a_missing_value_is_reported_like_python() {
    let error = rejected(&edited("gateway = \"192.0.2.1\"\n", ""));
    assert_eq!(error.field, "network.gateway");
    assert_eq!(error.message, "Missing configuration value [network] gateway");
}

#[test]
fn a_section_that_is_not_a_table_is_rejected() {
    // Top level, before the first table header.
    let without_user = edited("[user]\nname = \"developer\"\nuid = 1000\ngid = 1000\n", "");
    let error = rejected(&format!("user = 5\n{without_user}"));
    assert_eq!(error.field, "user");
}

#[test]
fn invalid_toml_reports_the_line_without_the_text() {
    let error = rejected("version = 1\n[node\nsecret-looking = 1\n");
    assert_eq!(error.field, "toml");
    assert!(error.message.contains("line 2"), "{error}");
    assert!(!error.message.contains("secret"), "input echoed: {error}");
}

#[test]
fn unknown_keys_are_named_only_when_they_look_like_keys() {
    let error = rejected(&format!("{EXAMPLE}\n[runtime]\nroot = \"/root\"\n"));
    assert_eq!(error.field, "runtime");
    let error = rejected(&format!("\"<script>\" = 1\n{EXAMPLE}"));
    assert!(!error.message.contains("<script>"), "{error}");
}

#[test]
fn gold_can_be_configured_after_installation() -> Result<(), Box<dyn std::error::Error>> {
    let config = Config::parse(&edited("gold_vmid = 100\n", ""))?;
    assert_eq!(config.gold_vmid, None);
    let text = config.to_toml();
    assert!(!text.contains("gold_vmid"));
    assert_eq!(Config::parse(&text)?, config);
    Ok(())
}
