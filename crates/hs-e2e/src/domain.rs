//! Persistent DNS and public TLS fixtures, independent of disposable VM files.

use std::fs::File;
use std::io::Write;
use std::net::Ipv4Addr;
use std::os::unix::fs::{OpenOptionsExt, PermissionsExt};
use std::path::{Path, PathBuf};
use std::process::Stdio;
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use hs_deploy::acme::{self, Stored};
use hs_deploy::dns::DnsApi;
use hs_deploy::spaceship::{SpaceshipClient, SpaceshipCredentials};
use instant_acme::{AuthorizationStatus, ChallengeType, Identifier, NewOrder};
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use tokio::process::Command;

use crate::Result;

pub const HOST: &str = "pve.e2e.homestack.app";
pub const NODE: &str = "pve1";
pub const SEARCH: &str = "e2e.homestack.app";
const ZONE: &str = "homestack.app";
const SUB: &str = "pve.e2e";
const CHALLENGE: &str = "_acme-challenge.pve.e2e";
const DIRECTORY: &str = "https://acme-v02.api.letsencrypt.org/directory";

#[derive(Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct LocalAddress {
    server: Ipv4Addr,
    address: Ipv4Addr,
}

#[derive(Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct Journal {
    hostname: String,
    public_address: Option<Ipv4Addr>,
    local_address: Option<LocalAddress>,
    pending_txt: Option<String>,
}

pub struct Fixture {
    dir: PathBuf,
    _lock: File,
    dns: DnsApi,
    tsig: PathBuf,
    journal: Journal,
}

impl Fixture {
    pub fn open(dir: &Path, credentials: &Path, tsig: &Path) -> Result<Self> {
        let credentials = SpaceshipCredentials::from_file(credentials)?;
        if dir.exists() && std::fs::symlink_metadata(dir)?.file_type().is_symlink() {
            return Err("fixture directory must not be a symlink".into());
        }
        std::fs::create_dir_all(dir)?;
        let dir = dir.canonicalize()?;
        for name in ["fixture.lock", "manifest.json", "manifest.tmp", "acme"] {
            if let Ok(meta) = std::fs::symlink_metadata(dir.join(name))
                && meta.file_type().is_symlink()
            {
                return Err(format!("fixture {name} must not be a symlink").into());
            }
        }
        if !dir.join("manifest.json").exists()
            && std::fs::read_dir(&dir)?.any(|entry| entry.is_ok_and(|entry| entry.file_name() != "fixture.lock"))
        {
            return Err("refusing to adopt a nonempty fixture directory without its manifest".into());
        }
        let lock = File::options()
            .create(true)
            .truncate(false)
            .read(true)
            .write(true)
            .mode(0o600)
            .open(dir.join("fixture.lock"))?;
        lock.try_lock()
            .map_err(|_| "the E2E domain fixture is in use by another controller")?;
        let path = dir.join("manifest.json");
        let journal = if path.exists() {
            let journal: Journal = serde_json::from_slice(&std::fs::read(path)?)?;
            if journal.hostname != HOST {
                return Err("fixture manifest belongs to a different domain".into());
            }
            journal
        } else {
            Journal {
                hostname: HOST.into(),
                public_address: None,
                local_address: None,
                pending_txt: None,
            }
        };
        std::fs::set_permissions(&dir, std::fs::Permissions::from_mode(0o700))?;
        let fixture = Self {
            dir,
            _lock: lock,
            dns: DnsApi::Spaceship(SpaceshipClient::new(credentials)?),
            tsig: tsig.to_owned(),
            journal,
        };
        fixture.save()?;
        Ok(fixture)
    }

    fn spaceship(&self) -> &SpaceshipClient {
        match &self.dns {
            DnsApi::Spaceship(client) => client,
            DnsApi::Ovh(_) => unreachable!("the E2E fixture only uses Spaceship"),
        }
    }

    fn save(&self) -> Result<()> {
        let temporary = self.dir.join("manifest.tmp");
        let mut file = File::options()
            .write(true)
            .create(true)
            .truncate(true)
            .mode(0o600)
            .open(&temporary)?;
        file.write_all(&serde_json::to_vec_pretty(&self.journal)?)?;
        file.sync_all()?;
        std::fs::rename(temporary, self.dir.join("manifest.json"))?;
        File::open(&self.dir)?.sync_all()?;
        Ok(())
    }

    pub async fn prepare(&mut self, address: Ipv4Addr, local_dns: Option<Ipv4Addr>) -> Result<Value> {
        self.cleanup_challenge().await?;
        eprintln!("Checking existing DNS records at Spaceship and the cached certificate for {HOST}...");
        if self.journal.public_address.is_some_and(|owned| owned != address) {
            return Err("fixture address changed; reset it with cleanup --all first".into());
        }
        let records = self.spaceship().records(ZONE).await?;
        let create = address_missing(&records, address)?;
        if create {
            // Persist intent before the request: a lost response must still be recoverable.
            self.journal.public_address = Some(address);
            self.save()?;
            self.spaceship().add_address(ZONE, SUB, address).await?;
            eprintln!("Public DNS [CREATED]: added missing record {HOST} -> {address}");
        } else {
            eprintln!("Public DNS [EXISTING, UNCHANGED]: {HOST} -> {address}");
        }
        if let Some(server) = local_dns {
            self.prepare_local(server, address).await?;
        } else if self.journal.local_address.is_some() {
            return Err(
                "this fixture also owns local DNS; supply the same --local-dns or reset with cleanup --all".into(),
            );
        }
        let acme_dir = self.dir.join("acme");
        for name in ["account.json", "cert.pem", "key.pem"] {
            if let Ok(meta) = std::fs::symlink_metadata(acme_dir.join(name))
                && (!meta.is_file() || meta.file_type().is_symlink())
            {
                return Err(format!("cached {name} must be a regular private file").into());
            }
        }
        let now = SystemTime::now().duration_since(UNIX_EPOCH)?.as_secs() as i64;
        let cached = acme::stored(&acme_dir)?;
        let reused = cached.as_ref().is_some_and(|stored| usable(stored, now));
        let stored = if reused {
            cached.ok_or("cached certificate disappeared")?
        } else {
            eprintln!(
                "Certificate [REQUESTING NEW]: no reusable cached certificate; requesting Let's Encrypt via DNS-01..."
            );
            self.issue(&acme_dir).await?
        };
        let fingerprint = self.validate_certificate().await?;
        if reused {
            eprintln!("Certificate [REUSED]: existing cached certificate verified; no new ACME order or challenge.");
        } else {
            eprintln!("Certificate [CREATED]: newly issued certificate verified and saved in cache.");
        }
        self.verify_resolution(address).await?;
        Ok(
            json!({"hostname": HOST, "address": address.to_string(), "dns_created": create,
            "certificate_reused": reused, "certificate_not_after": stored.not_after,
            "certificate_sha256": fingerprint,
            "directory": self.dir, "retained_after_vm_cleanup": true}),
        )
    }

    async fn issue(&mut self, dir: &Path) -> Result<Stored> {
        let account = acme::account(DIRECTORY, dir).await?;
        let mut order = account
            .new_order(&NewOrder::new(&[Identifier::Dns(HOST.into())]))
            .await?;
        {
            let mut authorizations = order.authorizations();
            while let Some(authz) = authorizations.next().await {
                let mut authz = authz?;
                match authz.status {
                    AuthorizationStatus::Valid => continue,
                    AuthorizationStatus::Pending => {}
                    status => return Err(format!("ACME authorization is {status:?}").into()),
                }
                let mut challenge = authz
                    .challenge(ChallengeType::Dns01)
                    .ok_or("ACME has no DNS-01 challenge")?;
                let value = challenge.key_authorization().dns_value();
                let records = self.spaceship().records(ZONE).await?;
                if records
                    .iter()
                    .any(|r| r["type"] == "TXT" && r["name"] == CHALLENGE && r["value"] == value)
                {
                    return Err("refusing to adopt an unowned ACME challenge record".into());
                }
                self.journal.pending_txt = Some(value.clone());
                self.save()?;
                self.spaceship().add_txt(ZONE, CHALLENGE, &value).await?;
                acme::wait_for_txt(&self.dns, ZONE, &format!("{CHALLENGE}.{ZONE}"), &value, &|line| {
                    eprintln!("{line}")
                })
                .await?;
                challenge.set_ready().await?;
            }
        }
        let stored = acme::finish(&mut order, dir).await?;
        self.cleanup_challenge().await?;
        Ok(stored)
    }

    pub async fn cleanup_challenge(&mut self) -> Result<()> {
        if let Some(value) = &self.journal.pending_txt {
            let records = self.spaceship().records(ZONE).await?;
            if records
                .iter()
                .any(|r| r["type"] == "TXT" && r["name"] == CHALLENGE && r["value"] == *value)
            {
                self.spaceship().remove_txt(ZONE, CHALLENGE, value).await?;
            }
            self.journal.pending_txt = None;
            self.save()?;
        }
        Ok(())
    }

    async fn prepare_local(&mut self, server: Ipv4Addr, address: Ipv4Addr) -> Result<()> {
        if self
            .journal
            .local_address
            .as_ref()
            .is_some_and(|owned| owned.server != server || owned.address != address)
        {
            return Err("local DNS fixture changed; reset with cleanup --all first".into());
        }
        let ipv6 = tool(
            "dig",
            &["+short", "+time=3", "+tries=1", &format!("@{server}"), HOST, "AAAA"],
        )
        .await?;
        if !ipv6.is_empty() {
            return Err("the local E2E hostname has conflicting AAAA or CNAME records".into());
        }
        let addresses = self.local_addresses(server).await?;
        if addresses == vec![address.to_string()] {
            eprintln!("Local DNS [EXISTING, UNCHANGED] at {server}: {HOST} -> {address}");
            return Ok(());
        }
        if !addresses.is_empty() {
            return Err("the local E2E hostname has conflicting A or CNAME records".into());
        }
        self.journal.local_address = Some(LocalAddress { server, address });
        self.save()?;
        self.nsupdate(server, &format!(
            "prereq nxrrset {HOST}. A\nprereq nxrrset {HOST}. AAAA\nprereq nxrrset {HOST}. CNAME\nupdate add {HOST}. 60 A {address}\n"
        )).await?;
        if self.local_addresses(server).await? != vec![address.to_string()] {
            return Err("local E2E DNS update did not become visible".into());
        }
        eprintln!("Local DNS [CREATED] at {server}: added missing record {HOST} -> {address}");
        Ok(())
    }

    async fn local_addresses(&self, server: Ipv4Addr) -> Result<Vec<String>> {
        let output = tool(
            "dig",
            &["+short", "+time=3", "+tries=1", &format!("@{server}"), HOST, "A"],
        )
        .await?;
        Ok(String::from_utf8(output)?.lines().map(str::to_owned).collect())
    }

    async fn nsupdate(&self, server: Ipv4Addr, updates: &str) -> Result<()> {
        use tokio::io::AsyncWriteExt;
        let mut child = Command::new("nsupdate")
            .arg("-v")
            .arg("-k")
            .arg(&self.tsig)
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .kill_on_drop(true)
            .spawn()?;
        let mut stdin = child.stdin.take().ok_or("nsupdate stdin is unavailable")?;
        stdin
            .write_all(format!("server {server}\nzone {ZONE}.\n{updates}send\n").as_bytes())
            .await?;
        drop(stdin);
        let output = tokio::time::timeout(Duration::from_secs(30), child.wait_with_output()).await??;
        if !output.status.success() {
            return Err(format!("local DNS update failed: {}", String::from_utf8_lossy(&output.stderr)).into());
        }
        Ok(())
    }

    pub async fn verify_resolution(&self, address: Ipv4Addr) -> Result<()> {
        let deadline = tokio::time::Instant::now() + Duration::from_secs(60);
        loop {
            // NXDOMAIN is cached under ANY. Refreshing only A can leave that
            // old negative entry behind, hiding the name again after its A TTL.
            let _ = tokio::time::timeout(
                Duration::from_secs(10),
                Command::new("resolvectl")
                    .args(["query", "--cache=no", "--type=ANY", HOST])
                    .stdin(Stdio::null())
                    .stdout(Stdio::null())
                    .stderr(Stdio::null())
                    .kill_on_drop(true)
                    .status(),
            )
            .await;
            let addresses = tokio::net::lookup_host((HOST, 0)).await;
            if addresses.is_ok_and(|mut entries| entries.any(|entry| entry.ip() == address)) {
                return Ok(());
            }
            if tokio::time::Instant::now() >= deadline {
                return Err(format!("system DNS does not resolve {HOST} to {address}; check split DNS, rebinding protection and negative caches (for systemd-resolved: resolvectl query --cache=no --type=ANY {HOST})").into());
            }
            tokio::time::sleep(Duration::from_secs(2)).await;
        }
    }

    async fn validate_certificate(&self) -> Result<String> {
        let dir = self.dir.join("acme");
        let cert = dir.join("cert.pem").display().to_string();
        let key = dir.join("key.pem").display().to_string();
        tool(
            "openssl",
            &[
                "verify",
                "-purpose",
                "sslserver",
                "-verify_hostname",
                HOST,
                "-untrusted",
                &cert,
                &cert,
            ],
        )
        .await?;
        let certificate_key = tool("openssl", &["x509", "-in", &cert, "-noout", "-pubkey"]).await?;
        let private_key = tool("openssl", &["pkey", "-in", &key, "-passin", "pass:", "-pubout"]).await?;
        if certificate_key != private_key {
            return Err("cached certificate does not match its private key".into());
        }
        Ok(
            String::from_utf8(tool("openssl", &["x509", "-in", &cert, "-noout", "-fingerprint", "-sha256"]).await?)?
                .trim()
                .to_owned(),
        )
    }

    pub async fn install(&self, vm: &crate::vm::Vm) -> Result<()> {
        use base64::Engine;
        let encode = |name| -> Result<String> {
            Ok(base64::engine::general_purpose::STANDARD.encode(std::fs::read(self.dir.join("acme").join(name))?))
        };
        let script = format!(
            "umask 077\ndirectory=$(mktemp -d /run/hs-e2e-tls.XXXXXX)\ntrap 'rm -rf -- \"$directory\"' EXIT\nprintf '%s' '{}' | base64 -d > \"$directory/cert.pem\"\nprintf '%s' '{}' | base64 -d > \"$directory/key.pem\"\npvenode cert set \"$directory/cert.pem\" \"$directory/key.pem\" --force\nsystemctl restart pveproxy\n",
            encode("cert.pem")?,
            encode("key.pem")?
        );
        // Do not log the script or SSH output: they may contain private key material.
        let output = vm.ssh(&script, Duration::from_secs(60)).await?;
        if !output.status.success() {
            return Err("cannot install the cached public certificate in PVE".into());
        }
        Ok(())
    }

    pub async fn reset(&mut self) -> Result<()> {
        self.cleanup_challenge().await?;
        if let Some(owned) = &self.journal.local_address {
            self.nsupdate(owned.server, &format!("update delete {HOST}. A {}\n", owned.address))
                .await?;
            self.journal.local_address = None;
            self.save()?;
        }
        if let Some(address) = self.journal.public_address {
            let records = self.spaceship().records(ZONE).await?;
            if records
                .iter()
                .any(|r| r["type"] == "A" && r["name"] == SUB && r["address"] == address.to_string())
            {
                self.spaceship().remove_address(ZONE, SUB, address).await?;
            }
            self.journal.public_address = None;
            self.save()?;
        }
        // Only remove known fixture files; never recursively delete a supplied path.
        for name in [
            "account.json",
            "cert.pem",
            "key.pem",
            "account.tmp",
            "cert.tmp",
            "key.tmp",
        ] {
            let path = self.dir.join("acme").join(name);
            if path.exists() {
                std::fs::remove_file(path)?;
            }
        }
        if self.dir.join("acme").exists() {
            std::fs::remove_dir(self.dir.join("acme"))?;
        }
        eprintln!("Removed owned E2E DNS records and cached ACME account/certificate. Provider credentials retained.");
        Ok(())
    }
}

fn usable(stored: &Stored, now: i64) -> bool {
    stored.names.iter().any(|name| name == HOST) && stored.not_after - now >= acme::RENEW_DAYS * 86_400
}

fn address_missing(records: &[Value], address: Ipv4Addr) -> Result<bool> {
    let relevant: Vec<_> = records
        .iter()
        .filter(|r| {
            r["name"].as_str().is_some_and(|name| name.eq_ignore_ascii_case(SUB))
                && matches!(r["type"].as_str(), Some("A" | "AAAA" | "CNAME"))
        })
        .collect();
    if relevant.is_empty() {
        return Ok(true);
    }
    if relevant.len() == 1 && relevant[0]["type"] == "A" && relevant[0]["address"] == address.to_string() {
        return Ok(false);
    }
    Err(format!("{HOST} already has conflicting DNS records; refusing to overwrite them").into())
}

pub(crate) async fn tool(program: &str, args: &[&str]) -> Result<Vec<u8>> {
    let output = tokio::time::timeout(
        Duration::from_secs(30),
        Command::new(program)
            .args(args)
            .stdin(Stdio::null())
            .kill_on_drop(true)
            .output(),
    )
    .await??;
    if !output.status.success() {
        return Err(format!("{program} failed: {}", String::from_utf8_lossy(&output.stderr)).into());
    }
    Ok(output.stdout)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn public_dns_reuses_matching_records_and_rejects_conflicts() -> Result<()> {
        let address = Ipv4Addr::LOCALHOST;
        assert!(address_missing(&[], address)?);
        assert!(address_missing(
            &[json!({"type":"A", "name":"pve1", "address":"192.168.1.1"})],
            address
        )?);
        assert!(!address_missing(
            &[json!({"type":"A", "name":SUB, "address":"127.0.0.1"})],
            address
        )?);
        for record in [
            json!({"type":"A", "name":SUB, "address":"192.168.1.1"}),
            json!({"type":"AAAA", "name":SUB, "address":"::1"}),
            json!({"type":"CNAME", "name":SUB, "value":"production.example"}),
        ] {
            assert!(address_missing(&[record], address).is_err());
        }
        Ok(())
    }

    #[test]
    fn reuse_requires_the_exact_pve_name_and_renewal_margin() {
        let mut stored = Stored {
            cert: String::new(),
            key: String::new(),
            not_after: 40 * 86_400,
            names: vec![HOST.into()],
        };
        assert!(usable(&stored, 0));
        assert!(!usable(&stored, 11 * 86_400));
        stored.names = vec!["*.e2e.homestack.app".into()];
        assert!(!usable(&stored, 0));
        stored.names = vec!["pve1.homestack.app".into()];
        assert!(!usable(&stored, 0));
    }

    fn credentials(dir: &Path) -> Result<PathBuf> {
        let path = dir.join("credentials");
        std::fs::write(&path, "SPACESHIP_API_KEY=test\nSPACESHIP_API_SECRET=test\n")?;
        Ok(path)
    }

    #[test]
    fn fixture_lock_and_ownership_guard_existing_state() -> Result<()> {
        let root = tempfile::tempdir()?;
        let credentials = credentials(root.path())?;
        let state = root.path().join("fixture");
        let fixture = Fixture::open(&state, &credentials, &credentials)?;
        assert!(Fixture::open(&state, &credentials, &credentials).is_err());
        drop(fixture);
        assert!(Fixture::open(&state, &credentials, &credentials).is_ok());
        let unmanaged = root.path().join("production");
        std::fs::create_dir(&unmanaged)?;
        std::fs::write(unmanaged.join("cert.pem"), "untouched")?;
        assert!(Fixture::open(&unmanaged, &credentials, &credentials).is_err());
        assert!(!unmanaged.join("fixture.lock").exists());
        assert_eq!(std::fs::read_to_string(unmanaged.join("cert.pem"))?, "untouched");
        std::fs::write(
            state.join("manifest.json"),
            r#"{"hostname":"production.example","public_address":null,"local_address":null,"pending_txt":null}"#,
        )?;
        assert!(Fixture::open(&state, &credentials, &credentials).is_err());
        Ok(())
    }

    #[tokio::test]
    async fn reset_removes_only_known_cache_files_and_preserves_credentials() -> Result<()> {
        let root = tempfile::tempdir()?;
        let credentials = credentials(root.path())?;
        let state = root.path().join("fixture");
        let mut fixture = Fixture::open(&state, &credentials, &credentials)?;
        std::fs::create_dir(state.join("acme"))?;
        for name in ["account.json", "cert.pem", "key.pem"] {
            std::fs::write(state.join("acme").join(name), "fixture")?;
        }
        std::fs::write(state.join("unrelated"), "keep")?;
        fixture.reset().await?;
        assert!(!state.join("acme").exists());
        assert_eq!(std::fs::read_to_string(state.join("unrelated"))?, "keep");
        assert!(credentials.exists());
        assert!(state.join("manifest.json").exists());
        assert!(Fixture::open(&state, &credentials, &credentials).is_err());
        Ok(())
    }
}
