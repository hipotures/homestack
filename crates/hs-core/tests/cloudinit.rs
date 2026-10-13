//! Ports of the snippet tests of `python/tests/test_proxmox.py`
//! (`HomeDiskTests`) for decision 1 = A: the vendor script is one static file
//! per format, and the rest of Cloud-Init is the VM's own configuration.

mod common;

use common::test_config;
use hs_core::cloudinit::{HomeFormat, cicustom_value, cloud_init_settings, quote_sshkeys, vendor_snippet};

/// Each `  - |` block of a cloud-config list, without its indentation.
fn block_scalars(yaml: &str, key: &str) -> Vec<String> {
    let mut blocks = Vec::new();
    let mut lines = yaml.lines().skip_while(|line| *line != format!("{key}:")).skip(1);
    let mut current: Option<String> = None;
    for line in lines.by_ref() {
        if line == "  - |" {
            blocks.extend(current.take());
            current = Some(String::new());
        } else if let Some(block) = current.as_mut()
            && (line.is_empty() || line.starts_with("      "))
        {
            block.push_str(line.get(6..).unwrap_or_default());
            block.push('\n');
        } else {
            break;
        }
    }
    blocks.extend(current);
    blocks
}

/// `sh -n` of every script in the snippet.
fn assert_shell_syntax(yaml: &str) {
    for key in ["bootcmd", "runcmd"] {
        let scripts = block_scalars(yaml, key);
        assert_eq!(scripts.len(), 1, "one {key} script");
        let status = std::process::Command::new("sh")
            .arg("-n")
            .arg("-c")
            .arg(&scripts[0])
            .status()
            .map(|status| status.success());
        assert_eq!(status.ok(), Some(true), "{key} is valid shell");
    }
}

/// `HomeDiskTests::test_create_snippet_formats_only_blank_new_home`. The
/// serial is no longer the VM's own (`expected_serial=HS_HOME_200`): the
/// static file takes the one disk whose serial is `HS_HOME_<digits>`.
#[test]
fn create_snippet_formats_only_blank_new_home() {
    let vendor = vendor_snippet(HomeFormat::Create);
    assert!(vendor.contains("allow_format=1"));
    assert!(vendor.contains("mkfs.ext4 -m 0 -L"));
    assert!(vendor.contains("rm -rf -- \"$home_path/lost+found\""));
    assert!(vendor.contains("$2 ~ /^HS_HOME_[0-9]+$/"));
    assert!(vendor.contains("LABEL=%s %s ext4 defaults 0 2"));
    assert!(vendor.contains("systemd-escape --path --suffix=mount \"$home_path\""));
    assert!(!vendor.contains("home-user.mount"));
    assert!(!vendor.to_lowercase().contains("virtiofs"));
    assert_shell_syntax(&vendor);
}

/// `HomeDiskTests::test_refresh_snippet_refuses_blank_home`. Proxmox writes
/// the user-data now, so `ssh_pwauth` and `ssh_deletekeys` are in the vendor
/// data, which fills what user-data leaves unset.
#[test]
fn refresh_snippet_refuses_blank_home() {
    let vendor = vendor_snippet(HomeFormat::Keep);
    assert!(vendor.contains("\nssh_pwauth: false\n"));
    assert!(vendor.contains("\nssh_deletekeys: true\n"));
    assert!(vendor.contains("allow_format=0"));
    assert!(vendor.contains("persistent home is blank during refresh; refusing mkfs"));
    assert_shell_syntax(&vendor);
}

/// The two files differ only in the flag.
#[test]
fn the_two_snippets_differ_only_in_the_flag() {
    let create = vendor_snippet(HomeFormat::Create);
    let keep = vendor_snippet(HomeFormat::Keep);
    assert_eq!(create.replace("allow_format=1", "allow_format=0"), keep);
    assert_eq!(create.matches("allow_format=1").count(), 1);
    assert!(!create.contains('@'), "no placeholder is left");
}

/// The files installed on the nodes are the ones in `deploy/cloud-init/`.
#[test]
fn reference_files_are_the_rendered_snippets() {
    for format in [HomeFormat::Create, HomeFormat::Keep] {
        let path = format!(
            "{}/../../deploy/cloud-init/{}",
            env!("CARGO_MANIFEST_DIR"),
            format.snippet_file()
        );
        let text = std::fs::read_to_string(&path).unwrap_or_else(|error| panic!("{path}: {error}"));
        assert_eq!(text, vendor_snippet(format), "{path} is out of date");
    }
    assert_eq!(HomeFormat::Create.snippet_file(), "homestack-vendor-create.yaml");
    assert_eq!(HomeFormat::Keep.snippet_file(), "homestack-vendor-keep.yaml");
}

/// `deploy/pve/homestack-pve.sh` installs the same snippets: its embedded
/// copies are rewritten by `deploy/pve/embed-snippets.sh`.
#[test]
fn install_script_embeds_the_rendered_snippets() {
    let path = format!("{}/../../deploy/pve/homestack-pve.sh", env!("CARGO_MANIFEST_DIR"));
    let script = std::fs::read_to_string(&path).unwrap_or_else(|error| panic!("{path}: {error}"));
    for format in [HomeFormat::Create, HomeFormat::Keep] {
        let start = format!(
            "        {})\n            cat <<'HOMESTACK_SNIPPET'\n",
            format.snippet_file()
        );
        let begin = script
            .find(&start)
            .unwrap_or_else(|| panic!("{path} embeds no {}", format.snippet_file()))
            + start.len();
        let Some(length) = script[begin..].find("\nHOMESTACK_SNIPPET\n") else {
            panic!("{path}: the heredoc of {} does not end", format.snippet_file());
        };
        let end = begin + length + 1;
        assert_eq!(
            &script[begin..end],
            vendor_snippet(format),
            "{path} embeds an old {}; run deploy/pve/embed-snippets.sh",
            format.snippet_file()
        );
    }
}

#[test]
fn cicustom_names_only_the_vendor_snippet() {
    let cfg = test_config();
    assert_eq!(
        cicustom_value(&cfg, HomeFormat::Create),
        "vendor=local:snippets/homestack-vendor-create.yaml"
    );
    assert_eq!(
        cicustom_value(&cfg, HomeFormat::Keep),
        "vendor=local:snippets/homestack-vendor-keep.yaml"
    );
}

/// `write_snippets` for create: what the four files said becomes VM options.
#[test]
fn create_settings_carry_network_keys_and_the_create_snippet() {
    let cfg = test_config();
    let keys = "ssh-ed25519 AAAATEST test\nsk-ssh-ed25519@openssh.com AAAAB user@host\n";
    let settings = cloud_init_settings(&cfg, HomeFormat::Create, "192.0.2.200", 24, "192.0.2.1", Some(keys));
    let expected: Vec<(&str, String)> = vec![
        ("ipconfig0", "ip=192.0.2.200/24,gw=192.0.2.1".to_owned()),
        ("nameserver", "192.0.2.1 1.1.1.1".to_owned()),
        ("ciuser", "root".to_owned()),
        ("ciupgrade", "0".to_owned()),
        (
            "sshkeys",
            "ssh-ed25519%20AAAATEST%20test%0Ask-ssh-ed25519%40openssh.com%20AAAAB%20user%40host%0A".to_owned(),
        ),
        (
            "cicustom",
            "vendor=local:snippets/homestack-vendor-create.yaml".to_owned(),
        ),
    ];
    assert_eq!(settings.ok(), Some(expected));
}

/// `write_snippets(preserve_home=True)`: no keys needed and none written;
/// the VM keeps its `sshkeys`.
#[test]
fn keep_settings_leave_the_keys_alone() {
    let cfg = test_config();
    let settings =
        cloud_init_settings(&cfg, HomeFormat::Keep, "192.0.2.200", 16, "192.0.2.254", None).unwrap_or_default();
    assert_eq!(
        settings.iter().map(|(key, _)| *key).collect::<Vec<_>>(),
        ["ipconfig0", "nameserver", "ciuser", "ciupgrade", "cicustom"]
    );
    assert_eq!(settings[0].1, "ip=192.0.2.200/16,gw=192.0.2.254");
    assert_eq!(settings[4].1, "vendor=local:snippets/homestack-vendor-keep.yaml");
}

#[test]
fn create_requires_a_key() {
    let cfg = test_config();
    for keys in [None, Some("")] {
        let error = cloud_init_settings(&cfg, HomeFormat::Create, "192.0.2.200", 24, "192.0.2.1", keys)
            .err()
            .map(|e| e.to_string());
        assert_eq!(
            error.as_deref(),
            Some("Workspace create requires at least one user SSH public key")
        );
    }
}

/// Proxmox stores `sshkeys` as Perl's `uri_escape` (every byte but
/// `A-Za-z0-9-._~`), as `qm set --sshkeys FILE` writes it.
#[test]
fn sshkeys_are_quoted_like_proxmox() {
    assert_eq!(quote_sshkeys("a-b_c.d~e f/g+h=\n"), "a-b_c.d~e%20f%2Fg%2Bh%3D%0A");
    assert_eq!(quote_sshkeys("ż"), "%C5%BC");
}
