//! Service names in the local DNS zone: `dig` reads and `nsupdate` changes
//! A records, signed with the TSIG key set up under Integrations (Local
//! DNS). Nothing else in the zone is touched.

use std::collections::BTreeSet;
use std::path::{Path, PathBuf};
use std::process::Stdio;
use std::time::Duration;

use tokio::io::AsyncWriteExt;
use tokio::process::Command;

/// How long a record is cached, in seconds.
const TTL: u32 = 300;

/// The local DNS zone and the server that takes its updates.
#[derive(Debug, Clone)]
pub struct Dns {
    pub zone: String,
    pub server: String,
    key: PathBuf,
}

impl Dns {
    /// The configured local DNS; `None` when its key is not set up.
    pub fn load(token_dir: &Path) -> Result<Option<Self>, String> {
        let dir = token_dir.join("integrations");
        let key = dir.join("dns-tsig.key");
        if !key.is_file() {
            return Ok(None);
        }
        let text = std::fs::read_to_string(dir.join("settings.toml")).unwrap_or_default();
        let table: toml::Table =
            toml::from_str(&text).map_err(|_| "integrations/settings.toml is not valid TOML".to_owned())?;
        let entry = table.get("dns-tsig.key").and_then(toml::Value::as_table);
        let get = |key: &str| {
            entry
                .and_then(|entry| entry.get(key))
                .and_then(toml::Value::as_str)
                .map(str::to_owned)
        };
        match (get("zone"), get("server")) {
            (Some(zone), Some(server)) => Ok(Some(Self { zone, server, key })),
            _ => Err("Local DNS (TSIG) has no zone or server; set them under Integrations".to_owned()),
        }
    }

    /// Whether `fqdn` lies in the zone.
    #[must_use]
    pub fn covers(&self, fqdn: &str) -> bool {
        fqdn == self.zone || fqdn.ends_with(&format!(".{}", self.zone))
    }

    async fn dig(&self, name: &str, kind: &str) -> Result<String, String> {
        let output = Command::new("dig")
            .arg("-k")
            .arg(&self.key)
            .arg(format!("@{}", self.server))
            .args([name, kind, "+short", "+norecurse", "+time=5", "+tries=1"])
            .stdin(Stdio::null())
            .output()
            .await
            .map_err(|error| {
                if error.kind() == std::io::ErrorKind::NotFound {
                    "Install dig (bind tools) to use the local DNS".to_owned()
                } else {
                    format!("Cannot run dig: {error}")
                }
            })?;
        if !output.status.success() {
            return Err(format!("Local DNS {} did not answer for {name}", self.server));
        }
        Ok(String::from_utf8_lossy(&output.stdout).into_owned())
    }

    /// The zone answers, signed by the key.
    pub async fn check(&self) -> Result<(), String> {
        if self.dig(&self.zone, "SOA").await?.trim().is_empty() {
            return Err(format!("Local DNS {} does not serve zone {}", self.server, self.zone));
        }
        Ok(())
    }

    /// The A records of `fqdn`.
    pub async fn addresses(&self, fqdn: &str) -> Result<Vec<String>, String> {
        Ok(self
            .dig(fqdn, "A")
            .await?
            .lines()
            .map(str::trim)
            .filter(|line| !line.is_empty())
            .map(str::to_owned)
            .collect())
    }

    async fn update(&self, commands: &str) -> Result<(), String> {
        let mut child = Command::new("nsupdate")
            .arg("-k")
            .arg(&self.key)
            .stdin(Stdio::piped())
            .stdout(Stdio::null())
            .stderr(Stdio::piped())
            .kill_on_drop(true)
            .spawn()
            .map_err(|error| {
                if error.kind() == std::io::ErrorKind::NotFound {
                    "Install nsupdate (bind tools) to use the local DNS".to_owned()
                } else {
                    format!("Cannot run nsupdate: {error}")
                }
            })?;
        let script = format!("server {}\nzone {}\n{commands}send\n", self.server, self.zone);
        let mut stdin = child.stdin.take().ok_or("nsupdate has no input")?;
        stdin
            .write_all(script.as_bytes())
            .await
            .map_err(|_| "Cannot write to nsupdate".to_owned())?;
        drop(stdin);
        let output = child
            .wait_with_output()
            .await
            .map_err(|_| "nsupdate did not finish".to_owned())?;
        if output.status.success() {
            Ok(())
        } else {
            let reason = String::from_utf8_lossy(&output.stderr);
            Err(format!(
                "Local DNS update refused: {}",
                reason.lines().next().unwrap_or("no details")
            ))
        }
    }

    /// Point `fqdn` at `address` alone, and see it answered.
    pub async fn set(&self, fqdn: &str, address: &str) -> Result<(), String> {
        self.update(&format!(
            "update delete {fqdn}. A\nupdate add {fqdn}. {TTL} A {address}\n"
        ))
        .await?;
        if self.addresses(fqdn).await? == [address] {
            Self::verify_local(fqdn, address).await
        } else {
            Err(format!("{fqdn} does not answer {address} after the update"))
        }
    }

    /// Refresh this name when resolvectl is installed, then verify the desktop's
    /// normal resolver sees exactly the planned IPv4 address.
    pub async fn verify_local(fqdn: &str, address: &str) -> Result<(), String> {
        tokio::time::timeout(Duration::from_secs(15), async {
            let refresh = Command::new("resolvectl")
                .args(["query", "--cache=no", "--type=ANY", fqdn])
                .stdin(Stdio::null())
                .stdout(Stdio::null())
                .stderr(Stdio::piped())
                .kill_on_drop(true)
                .output()
                .await;
            match refresh {
                Ok(output) if output.status.success() => {}
                Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
                Ok(output) => {
                    let reason = String::from_utf8_lossy(&output.stderr);
                    return Err(format!(
                        "DNS record updated, but refreshing {fqdn} on this desktop failed: {}",
                        reason.lines().next().unwrap_or("no details")
                    ));
                }
                Err(error) => return Err(format!("Cannot refresh system DNS for {fqdn}: {error}")),
            }
            let found: BTreeSet<String> = tokio::net::lookup_host((fqdn, 0))
                .await
                .map_err(|error| format!("System DNS cannot resolve {fqdn}: {error}"))?
                .filter(|socket| socket.is_ipv4())
                .map(|socket| socket.ip().to_string())
                .collect();
            if found.len() != 1 || !found.contains(address) {
                return Err(format!(
                    "System DNS for {fqdn} returns [{}]; expected {address}",
                    found.into_iter().collect::<Vec<_>>().join(", ")
                ));
            }
            Ok(())
        })
        .await
        .map_err(|_| format!("System DNS verification for {fqdn} timed out after 15 s"))?
    }

    /// Remove the A records of `fqdn`.
    pub async fn remove(&self, fqdn: &str) -> Result<(), String> {
        self.update(&format!("update delete {fqdn}. A\n")).await
    }
}
