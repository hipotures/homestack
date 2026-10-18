//! Establish the current install input requirements without a Python config.

use hs_config::{client::ClientConfig, legacy, split::split};
use hs_model::config::Config;

const MINIMAL: &str = include_str!("../../../config.minimal.toml");
const LAB: &str = include_str!("../../hs-e2e/src/service-config.toml");

#[test]
fn minimal_install_input_splits_without_gold_or_optional_sections() -> Result<(), Box<dyn std::error::Error>> {
    let files = split(MINIMAL)?;
    let server = Config::parse(&files.server_toml)?;
    let client = ClientConfig::parse(&files.client_toml)?;
    assert_eq!(server.gold_vmid, None);
    assert_eq!(client.repo.owner, None);
    assert_eq!(client.repo.checkout_root, "~/DEV");
    assert_eq!(client.display.storage_unit, "GiB");
    assert!(client.setup.is_none());
    assert!(client.sounds.is_none());
    Ok(())
}

#[test]
fn removing_any_minimal_field_is_rejected() -> Result<(), Box<dyn std::error::Error>> {
    let document: toml::Table = MINIMAL.parse()?;
    for (section, value) in &document {
        let paths: Vec<Vec<&str>> = if let Some(table) = value.as_table() {
            table
                .iter()
                .flat_map(|(key, value)| {
                    if let Some(nested) = value.as_table() {
                        nested
                            .keys()
                            .map(|leaf| vec![section.as_str(), key.as_str(), leaf.as_str()])
                            .collect()
                    } else {
                        vec![vec![section.as_str(), key.as_str()]]
                    }
                })
                .collect()
        } else {
            vec![vec![section.as_str()]]
        };
        for path in paths {
            let mut candidate = document.clone();
            let mut table = &mut candidate;
            for component in &path[..path.len() - 1] {
                table = table
                    .get_mut(*component)
                    .and_then(toml::Value::as_table_mut)
                    .ok_or("missing configuration table")?;
            }
            table.remove(path[path.len() - 1]);
            let text = toml::to_string(&candidate)?;
            assert!(legacy::read(&text).is_err(), "accepted without {}", path.join("."));
        }
    }
    Ok(())
}

#[test]
fn workspace_key_name_is_arbitrary_but_empty_list_is_rejected() -> Result<(), Box<dyn std::error::Error>> {
    let text = MINIMAL.replace("~/.ssh/id_ed25519_sk", "/nonexistent/arbitrary-workspace-key");
    let config = legacy::read(&text)?;
    assert_eq!(
        config.client.workspace_ssh.identity_files,
        ["/nonexistent/arbitrary-workspace-key"]
    );
    let text = MINIMAL.replace("[\"~/.ssh/id_ed25519_sk\"]", "[]");
    assert_eq!(
        legacy::read(&text).err().ok_or("accepted empty identity list")?.field,
        "workspace_ssh.identity_files"
    );
    Ok(())
}

#[test]
fn lab_input_also_parses_without_its_reference_gold() -> Result<(), Box<dyn std::error::Error>> {
    let text = LAB.replace("@NODE@", "pve1").replace("@LAYOUT@", "homestack-storage-1");
    let with_gold = legacy::read(&text)?;
    assert_eq!(with_gold.server.gold_vmid, Some(100));
    let without_gold = legacy::read(&text.replace("gold_vmid = 100\n", ""))?;
    assert_eq!(without_gold.server.gold_vmid, None);
    assert_eq!(without_gold.client, with_gold.client);
    Ok(())
}
