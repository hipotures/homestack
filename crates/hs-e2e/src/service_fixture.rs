//! Owned DNS and public wildcard TLS for the E2E HomeStack API only.

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
use serde_json::Value;
use tokio::io::AsyncWriteExt;
use tokio::process::Command;

use crate::{
    Result,
    domain::{SEARCH, tool},
};

pub const HOST: &str = "api1.e2e.homestack.app";
pub const ADDRESS: Ipv4Addr = Ipv4Addr::new(10, 77, 0, 101);
const ZONE: &str = "homestack.app";
const SUB: &str = "api1.e2e";
const CHALLENGE: &str = "_acme-challenge.e2e";
const DIRECTORY: &str = "https://acme-v02.api.letsencrypt.org/directory";

fn named(record: &Value, name: &str) -> bool {
    record["name"]
        .as_str()
        .is_some_and(|found| found.eq_ignore_ascii_case(name))
}

#[derive(Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct Journal {
    hostname: String,
    public_owned: bool,
    local_owned: Option<Ipv4Addr>,
    pending_txt: Option<String>,
}

pub struct Fixture {
    pub dir: PathBuf,
    _lock: File,
    dns: DnsApi,
    tsig: PathBuf,
    journal: Journal,
}

fn private_dir(path: &Path) -> Result<()> {
    if path.is_symlink() {
        return Err("service fixture must not contain symlinks".into());
    }
    std::fs::create_dir_all(path)?;
    std::fs::set_permissions(path, std::fs::Permissions::from_mode(0o700))?;
    Ok(())
}

impl Fixture {
    pub fn open(parent: &Path, credentials: &Path, tsig: &Path) -> Result<Self> {
        let dir = parent.join("service");
        private_dir(&dir)?;
        for name in [
            "fixture.lock",
            "manifest.json",
            "manifest.tmp",
            "acme",
            "acme/account.json",
            "acme/cert.pem",
            "acme/key.pem",
        ] {
            if dir.join(name).is_symlink() {
                return Err("service fixture must not contain symlinks".into());
            }
        }
        if !dir.join("manifest.json").exists()
            && std::fs::read_dir(&dir)?.any(|entry| entry.is_ok_and(|entry| entry.file_name() != "fixture.lock"))
        {
            return Err("refusing to adopt an unowned service fixture directory".into());
        }
        let lock = File::options()
            .read(true)
            .write(true)
            .create(true)
            .truncate(false)
            .mode(0o600)
            .open(dir.join("fixture.lock"))?;
        lock.try_lock()
            .map_err(|_| "the HomeStack E2E API fixture is in use by another client")?;
        let journal = if dir.join("manifest.json").exists() {
            let journal: Journal = serde_json::from_slice(&std::fs::read(dir.join("manifest.json"))?)?;
            if journal.hostname != HOST {
                return Err("service fixture belongs to a different domain".into());
            }
            journal
        } else {
            Journal {
                hostname: HOST.into(),
                public_owned: false,
                local_owned: None,
                pending_txt: None,
            }
        };
        let fixture = Self {
            dir,
            _lock: lock,
            dns: DnsApi::Spaceship(SpaceshipClient::new(SpaceshipCredentials::from_file(credentials)?)?),
            tsig: tsig.to_owned(),
            journal,
        };
        fixture.save()?;
        Ok(fixture)
    }

    fn spaceship(&self) -> &SpaceshipClient {
        match &self.dns {
            DnsApi::Spaceship(client) => client,
            DnsApi::Ovh(_) => unreachable!(),
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

    pub async fn prepare(&mut self, local_dns: Option<Ipv4Addr>) -> Result<Stored> {
        self.cleanup_challenge().await?;
        let records = self.spaceship().records(ZONE).await?;
        let existing: Vec<_> = records
            .iter()
            .filter(|record| named(record, SUB) && matches!(record["type"].as_str(), Some("A" | "AAAA" | "CNAME")))
            .collect();
        if existing.is_empty() {
            self.journal.public_owned = true;
            self.save()?;
            self.spaceship().add_address(ZONE, SUB, ADDRESS).await?;
            eprintln!("HomeStack public DNS [CREATED]: {HOST} -> {ADDRESS}");
        } else if existing.len() == 1 && existing[0]["type"] == "A" && existing[0]["address"] == ADDRESS.to_string() {
            eprintln!("HomeStack public DNS [EXISTING, UNCHANGED]: {HOST} -> {ADDRESS}");
        } else {
            return Err("HomeStack E2E API DNS has conflicting records; refusing to replace them".into());
        }
        if let Some(server) = local_dns {
            self.prepare_local(server).await?;
        } else if self.journal.local_owned.is_some() {
            return Err("this API fixture also owns local DNS; supply --local-dns".into());
        }
        let dir = self.dir.join("acme");
        private_dir(&dir)?;
        for name in ["cert.pem", "key.pem", "account.json"] {
            if dir.join(name).is_symlink() {
                return Err("service TLS cache must not contain symlinks".into());
            }
        }
        let now = SystemTime::now().duration_since(UNIX_EPOCH)?.as_secs() as i64;
        let stored = match acme::stored(&dir)? {
            Some(stored) if acme::current(&stored, SEARCH, now) => {
                eprintln!("HomeStack certificate [REUSED]: *.{SEARCH}; no new ACME order.");
                stored
            }
            _ => {
                eprintln!("HomeStack certificate [REQUESTING NEW]: *.{SEARCH} via DNS-01...");
                self.issue(&dir).await?
            }
        };
        tool(
            "openssl",
            &[
                "verify",
                "-purpose",
                "sslserver",
                "-verify_hostname",
                HOST,
                "-untrusted",
                &dir.join("cert.pem").display().to_string(),
                &dir.join("cert.pem").display().to_string(),
            ],
        )
        .await?;
        let cert_key = tool(
            "openssl",
            &[
                "x509",
                "-in",
                &dir.join("cert.pem").display().to_string(),
                "-noout",
                "-pubkey",
            ],
        )
        .await?;
        let private_key = tool(
            "openssl",
            &[
                "pkey",
                "-in",
                &dir.join("key.pem").display().to_string(),
                "-passin",
                "pass:",
                "-pubout",
            ],
        )
        .await?;
        if cert_key != private_key {
            return Err("service certificate does not match its private key".into());
        }
        let _ = Command::new("resolvectl")
            .args(["query", "--cache=no", "--type=ANY", HOST])
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .status()
            .await;
        let addresses = tokio::net::lookup_host((HOST, 443)).await?;
        if !addresses.into_iter().any(|address| address.ip() == ADDRESS) {
            return Err("system DNS does not resolve the E2E API to its guest address".into());
        }
        Ok(stored)
    }

    async fn issue(&mut self, dir: &Path) -> Result<Stored> {
        let account = acme::account(DIRECTORY, dir).await?;
        let mut order = account
            .new_order(&NewOrder::new(&[Identifier::Dns(format!("*.{SEARCH}"))]))
            .await?;
        {
            let mut authorizations = order.authorizations();
            while let Some(authz) = authorizations.next().await {
                let mut authz = authz?;
                match authz.status {
                    AuthorizationStatus::Valid => continue,
                    AuthorizationStatus::Pending => {}
                    status => return Err(format!("API ACME authorization is {status:?}").into()),
                }
                let mut challenge = authz
                    .challenge(ChallengeType::Dns01)
                    .ok_or("ACME has no DNS-01 challenge")?;
                let value = challenge.key_authorization().dns_value();
                if self
                    .spaceship()
                    .records(ZONE)
                    .await?
                    .iter()
                    .any(|record| named(record, CHALLENGE))
                {
                    return Err("refusing to replace an existing unowned API ACME challenge".into());
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
            if self
                .spaceship()
                .records(ZONE)
                .await?
                .iter()
                .any(|record| record["type"] == "TXT" && named(record, CHALLENGE) && record["value"] == *value)
            {
                self.spaceship().remove_txt(ZONE, CHALLENGE, value).await?;
            }
            self.journal.pending_txt = None;
            self.save()?;
        }
        Ok(())
    }

    async fn prepare_local(&mut self, server: Ipv4Addr) -> Result<()> {
        if self.journal.local_owned.is_some_and(|owned| owned != server) {
            return Err("API local DNS server changed; clean the owned fixture first".into());
        }
        for kind in ["AAAA", "CNAME"] {
            if !tool(
                "dig",
                &["+short", "+time=3", "+tries=1", &format!("@{server}"), HOST, kind],
            )
            .await?
            .is_empty()
            {
                return Err("local API DNS has conflicting AAAA or CNAME records".into());
            }
        }
        let addresses = tool(
            "dig",
            &["+short", "+time=3", "+tries=1", &format!("@{server}"), HOST, "A"],
        )
        .await?;
        if String::from_utf8_lossy(&addresses).trim() == ADDRESS.to_string() {
            eprintln!("HomeStack local DNS [EXISTING, UNCHANGED] at {server}: {HOST} -> {ADDRESS}");
            return Ok(());
        }
        if !addresses.is_empty() {
            return Err("local API DNS has conflicting A records".into());
        }
        self.journal.local_owned = Some(server);
        self.save()?;
        self.nsupdate(server, &format!("prereq nxrrset {HOST}. A\nprereq nxrrset {HOST}. AAAA\nprereq nxrrset {HOST}. CNAME\nupdate add {HOST}. 60 A {ADDRESS}\n")).await?;
        eprintln!("HomeStack local DNS [CREATED] at {server}: {HOST} -> {ADDRESS}");
        Ok(())
    }

    async fn nsupdate(&self, server: Ipv4Addr, updates: &str) -> Result<()> {
        let mut child = Command::new("nsupdate")
            .arg("-v")
            .arg("-k")
            .arg(&self.tsig)
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .kill_on_drop(true)
            .spawn()?;
        let mut stdin = child.stdin.take().ok_or("nsupdate stdin unavailable")?;
        stdin
            .write_all(format!("server {server}\nzone {ZONE}.\n{updates}send\n").as_bytes())
            .await?;
        drop(stdin);
        let output = tokio::time::timeout(Duration::from_secs(30), child.wait_with_output()).await??;
        if !output.status.success() {
            return Err(format!(
                "API local DNS update failed: {}",
                String::from_utf8_lossy(&output.stderr)
            )
            .into());
        }
        Ok(())
    }

    pub async fn reset(&mut self) -> Result<()> {
        self.cleanup_challenge().await?;
        if let Some(server) = self.journal.local_owned {
            self.nsupdate(server, &format!("update delete {HOST}. A {ADDRESS}\n"))
                .await?;
            self.journal.local_owned = None;
            self.save()?;
        }
        if self.journal.public_owned {
            self.spaceship().remove_address(ZONE, SUB, ADDRESS).await?;
            self.journal.public_owned = false;
            self.save()?;
        }
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
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn credentials(dir: &Path) -> Result<PathBuf> {
        let path = dir.join("credentials");
        std::fs::write(&path, "SPACESHIP_API_KEY=test\nSPACESHIP_API_SECRET=test\n")?;
        Ok(path)
    }

    #[tokio::test]
    async fn service_fixture_lock_reset_and_files_are_scoped_to_the_api() -> Result<()> {
        let parent = tempfile::tempdir()?;
        let key = credentials(parent.path())?;
        let tsig = parent.path().join("tsig");
        let pve_cache = parent.path().join("acme");
        std::fs::create_dir(&pve_cache)?;
        std::fs::write(pve_cache.join("cert.pem"), "PVE certificate must remain")?;
        let mut fixture = Fixture::open(parent.path(), &key, &tsig)?;
        assert!(Fixture::open(parent.path(), &key, &tsig).is_err());
        assert_eq!(std::fs::metadata(&fixture.dir)?.permissions().mode() & 0o777, 0o700);
        std::fs::create_dir(fixture.dir.join("acme"))?;
        for name in ["account.json", "cert.pem", "key.pem"] {
            std::fs::write(fixture.dir.join("acme").join(name), "test")?;
        }
        fixture.reset().await?;
        assert!(!fixture.dir.join("acme").exists());
        assert_eq!(
            std::fs::read_to_string(pve_cache.join("cert.pem"))?,
            "PVE certificate must remain"
        );
        assert!(key.is_file());
        drop(fixture);
        Fixture::open(parent.path(), &key, &tsig)?;
        Ok(())
    }

    #[test]
    fn foreign_cache_and_symlinks_are_never_adopted() -> Result<()> {
        let parent = tempfile::tempdir()?;
        let key = credentials(parent.path())?;
        let tsig = parent.path().join("tsig");
        let dir = parent.path().join("service");
        std::fs::create_dir(&dir)?;
        std::fs::write(dir.join("foreign"), "keep")?;
        assert!(Fixture::open(parent.path(), &key, &tsig).is_err());
        std::fs::remove_file(dir.join("foreign"))?;
        let fixture = Fixture::open(parent.path(), &key, &tsig)?;
        drop(fixture);
        std::fs::create_dir(dir.join("acme"))?;
        std::os::unix::fs::symlink(&key, dir.join("acme/key.pem"))?;
        assert!(Fixture::open(parent.path(), &key, &tsig).is_err());
        assert!(key.is_file());
        Ok(())
    }
}
