//! Cloud-Init (mapping C1–C3) under decision 1 = A.
//!
//! Python writes four files per workspace on its node
//! (`cloudinit.write_snippets`); the API cannot write files. Here Proxmox
//! generates user-data, network-config and meta-data from the VM's own
//! options ([`cloud_init_settings`]), and `cicustom` names only a static
//! vendor snippet, installed once per node from `deploy/cloud-init/`. The
//! owner chose two files over a flag in `smbios1` (decisions.md, decision 1):
//! [`HomeFormat::Create`] formats a blank new home, [`HomeFormat::Keep`]
//! refuses to. Without per-VM files there is nothing stale to find or remove
//! (C2) and nothing to copy to another node (C3).

use hs_model::config::Config;

use crate::error::CoreError;

/// The vendor snippet, with `@ALLOW_FORMAT@` for the flag.
const VENDOR_TEMPLATE: &str = include_str!("cloudinit_vendor.yaml");

/// Whether the vendor script may format a blank home (Python's
/// `allow_format`, `preserve_home=False`).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum HomeFormat {
    /// `create`: a blank new home is formatted and gets the keys.
    Create,
    /// `refresh` and `addkey`: a blank home stops the script.
    Keep,
}

impl HomeFormat {
    /// The snippet's file name on the snippet storage.
    #[must_use]
    pub const fn snippet_file(self) -> &'static str {
        match self {
            Self::Create => "homestack-vendor-create.yaml",
            Self::Keep => "homestack-vendor-keep.yaml",
        }
    }

    const fn flag(self) -> &'static str {
        match self {
            Self::Create => "1",
            Self::Keep => "0",
        }
    }
}

/// The vendor snippet for `format`, as installed on every node.
#[must_use]
pub fn vendor_snippet(format: HomeFormat) -> String {
    VENDOR_TEMPLATE.replace("@ALLOW_FORMAT@", format.flag())
}

/// `cicustom_value`: only `vendor=`; Proxmox generates the rest.
#[must_use]
pub fn cicustom_value(cfg: &Config, format: HomeFormat) -> String {
    format!(
        "vendor={}:snippets/{}",
        cfg.cloud_init.snippet_storage,
        format.snippet_file()
    )
}

/// Perl's `URI::Escape::uri_escape`, which Proxmox applies to `sshkeys`:
/// every byte but `A-Za-z0-9-._~` as `%XX`.
#[must_use]
pub fn quote_sshkeys(keys: &str) -> String {
    keys.bytes()
        .map(|byte| {
            if byte.is_ascii_alphanumeric() || b"-._~".contains(&byte) {
                char::from(byte).to_string()
            } else {
                format!("%{byte:02X}")
            }
        })
        .collect()
}

/// The VM options that replace Python's four snippet files, in the order
/// they are set:
///
/// - `ipconfig0`: the address and gateway of the network snippet; Proxmox
///   matches the interface by the MAC of `net0` and names it `eth0`;
/// - `nameserver`: `[network] dns`;
/// - `ciuser=root`: Proxmox then writes `disable_root: False` and no
///   `users: - default`, so no account is created and the keys go to root;
///   the create snippet copies them into the new home;
/// - `ciupgrade=0`: no `package_upgrade: true`;
/// - `sshkeys` (create only): the workspace keys, which Python wrote into
///   the vendor script; refresh leaves the VM's keys as they are;
/// - `cicustom`: the vendor snippet for `format`.
///
/// The hostname is the VM's `name`; the instance id is Proxmox's hash of
/// user-data and network-config, so changing keys or network starts a new
/// instance, as Python's new random `instance-id` did on every write.
pub fn cloud_init_settings(
    cfg: &Config,
    format: HomeFormat,
    ip: &str,
    cidr: u8,
    gateway: &str,
    authorized_keys: Option<&str>,
) -> Result<Vec<(&'static str, String)>, CoreError> {
    let keys = authorized_keys.filter(|keys| !keys.is_empty());
    if format == HomeFormat::Create && keys.is_none() {
        return Err(CoreError::Invalid(
            "Workspace create requires at least one user SSH public key".to_owned(),
        ));
    }
    let mut settings = vec![
        ("ipconfig0", format!("ip={ip}/{cidr},gw={gateway}")),
        ("nameserver", cfg.network.dns.join(" ")),
        ("ciuser", "root".to_owned()),
        ("ciupgrade", "0".to_owned()),
    ];
    if let (HomeFormat::Create, Some(keys)) = (format, keys) {
        settings.push(("sshkeys", quote_sshkeys(keys)));
    }
    settings.push(("cicustom", cicustom_value(cfg, format)));
    Ok(settings)
}
