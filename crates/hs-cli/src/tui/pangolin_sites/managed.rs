//! Managed Site initialization for common service deployment
//! (`services::deploy`): a new connector container gets its own Site, named
//! as the container (`pangolin-1`, …), with its own credential and journal
//! kept per CTID, through the same
//! Step 5 tunnel, Step 7 organization key and Step 8 credential, recovery
//! and installation logic. The trusted context is opened here from the saved
//! Pangolin settings with a tunnel of its own, never borrowed from the
//! wizard's memory; the temporary Root key is never involved.

use std::sync::Arc;
use std::sync::atomic::Ordering;

use super::*;
use crate::services::deploy::{SiteInit, SiteReady};
use crate::system::System;
use crate::touch::Sounds;
use crate::tui::pangolin_tunnel::Session as Tunnel;
use hs_deploy::config::DeployConfig;

/// How long the tunnel may take, a security key's touch included.
const TUNNEL_LIMIT: Duration = Duration::from_secs(180);
/// How long the console may take to install the credential.
const CONSOLE_LIMIT: Duration = Duration::from_secs(60);
/// Precedes the installed credential's digest in the console; the script
/// prints it split in two, so the echoed command never matches.
const MARKER: &str = "HOMESTACK-SITE-DIGEST ";

impl Context {
    /// A managed instance's credential; its journal and lock sit next to it.
    fn managed_key(&self, ctid: u32) -> PathBuf {
        self.key
            .with_file_name(format!("pangolin-managed-site-{}-{ctid}.key", self.identity))
    }
}

/// The journal belongs to this server, organization and container.
fn owned(ctx: &Context, record: &SiteRecord, ctid: u32) -> Result<(), String> {
    if record.target != ctx.target || record.organization != ctx.organization || record.ctid != Some(ctid) {
        return Err(format!(
            "The Site journal of CT {ctid} belongs to another server, organization or container; nothing is changed"
        ));
    }
    Ok(())
}

/// The step for Site `nice` of CT `ctid` from its journal, its saved credential
/// and Pangolin's own Site of that name. `home` is the home volume of an existing
/// container; without one, anything saved for this CTID is an earlier
/// instance's and is refused rather than adopted.
fn reconcile(
    ctx: &Context,
    key: &Credential,
    ctid: u32,
    nice: &str,
    home: Option<&str>,
    record: Option<&SiteRecord>,
    credential: Option<&Credential>,
) -> Result<SiteStep, String> {
    if home.is_none() && (record.is_some() || credential.is_some()) {
        return Err(format!(
            "A Site journal or credential for CT {ctid} remains from an earlier instance; nothing is changed. \
             Inspect Site {nice} and remove it and its journal explicitly before reusing this CTID"
        ));
    }
    if let Some(record) = record {
        owned(ctx, record, ctid)?;
        if record.site != nice {
            return Err(format!(
                "CT {ctid} is now named {nice}, but its Site is {}; nothing is changed",
                record.site
            ));
        }
        if record.home.as_deref() != home {
            return Err(format!(
                "CT {ctid}: its home differs from the one its Site journal recorded; recovery refused"
            ));
        }
        if credential.and_then(|c| c.key_id().ok()) != Some(record.newt_id.as_str()) {
            return Err("The saved Site credential does not match its journal; nothing is changed".to_owned());
        }
    }
    let remote = site(&ctx.endpoint, key, &ctx.organization, nice)?;
    if let (Some(record), Some((id, _))) = (record, &remote)
        && record.site_id.is_some_and(|saved| saved != *id)
    {
        return Err(format!(
            "Site {nice} has another ID than its journal records; nothing is changed"
        ));
    }
    site_step(record, remote.as_ref(), credential.and_then(|c| c.key_id().ok()), nice)
}

/// The verified Step 5 tunnel to the saved Pangolin server and its Step 7
/// organization key; the tunnel closes when this is dropped.
pub struct Pangolin {
    ctx: Context,
    _tunnel: Tunnel,
}

impl Pangolin {
    /// Start and verify a tunnel from the saved settings in `dir` (the
    /// HomeStack configuration directory), then check the key.
    pub fn open(
        dir: &Path,
        system: Arc<dyn System>,
        prefix: Vec<String>,
        sounds: Sounds,
        stop: &AtomicBool,
    ) -> Result<Self, String> {
        let wizard = crate::tui::pangolin::Wizard::open(dir)?;
        let spec = wizard
            .tunnel_spec()
            .ok_or("Pangolin Step 4 has no verified loopback Integration API. Complete Steps 1–4 first.")?;
        let tunnel = Tunnel::start(
            crate::tui::pangolin::next_test(),
            spec.clone(),
            system,
            prefix,
            sounds,
            crate::touch::default_player(),
            Arc::new(|_| {}),
        );
        let started = Instant::now();
        let endpoint = loop {
            if let Some(endpoint) = tunnel.live_endpoint(&spec) {
                break endpoint;
            }
            if tunnel.finished() || stop.load(Ordering::SeqCst) || started.elapsed() >= TUNNEL_LIMIT {
                return Err(
                    "The Step 5 Pangolin API tunnel could not be established and verified. Check Steps 1–5.".to_owned(),
                );
            }
            std::thread::sleep(Duration::from_millis(200));
        };
        let pangolin = Self {
            ctx: wizard.managed_context(endpoint)?,
            _tunnel: tunnel,
        };
        pangolin.key()?;
        Ok(pangolin)
    }

    /// The tunnel is still verified.
    #[must_use]
    pub fn alive(&self) -> bool {
        self.ctx.endpoint.alive()
    }

    /// What the context is, for the review.
    #[must_use]
    pub fn describe(&self) -> String {
        format!("organization {} on {}", self.ctx.organization, self.ctx.target)
    }

    /// The Step 7 key over the live, verified tunnel, reading exactly this
    /// organization; checked again before every step.
    fn key(&self) -> Result<Credential, String> {
        if !self.ctx.endpoint.alive() {
            return Err(
                "The Step 5 Pangolin API tunnel is no longer verified; nothing is changed. Open the operation again."
                    .to_owned(),
            );
        }
        let key = api_key(&self.ctx)?;
        organization(&self.ctx, &key)?;
        Ok(key)
    }

    /// The completed Site of `ctid`: its journal, saved credential and
    /// Pangolin's own Site agree. Its name and ID.
    fn completed(&self, key: &Credential, ctid: u32) -> Result<(String, u64), String> {
        let path = self.ctx.managed_key(ctid);
        let storage = Storage::open(&path)?;
        let record: SiteRecord = storage
            .load()?
            .ok_or_else(|| format!("CT {ctid} has no managed Site journal on this desktop"))?;
        owned(&self.ctx, &record, ctid)?;
        let nice = record.site.clone();
        if record.stage != SiteStage::Online {
            return Err(format!(
                "CT {ctid}: its Site initialization is unfinished; resume the deployment first"
            ));
        }
        let credential = storage.recover_credential(&path)?;
        let site_id = record.site_id.ok_or("Invalid Site journal.")?;
        match site(&self.ctx.endpoint, key, &self.ctx.organization, &nice)? {
            Some((id, newt))
                if id == site_id && newt == record.newt_id && credential.key_id().ok() == Some(newt.as_str()) =>
            {
                Ok((nice, site_id))
            }
            Some(_) => Err(format!("Site {nice} no longer matches its journal; nothing is changed")),
            None => Err(format!("Site {nice} is gone from Pangolin; nothing is changed")),
        }
    }

    /// Poll until Site `site_id` is online, or the limit passes.
    fn wait_online(&self, key: &Credential, nice: &str, site_id: u64, say: &dyn Fn(&str)) -> Result<String, String> {
        say(&format!(
            "  waiting for Site {nice} to be online (up to {} s)",
            HEALTH_LIMIT.as_secs()
        ));
        let started = Instant::now();
        loop {
            if !self.ctx.endpoint.alive() {
                return Err("The Step 5 Pangolin API tunnel closed before the Site was online".to_owned());
            }
            match online(&self.ctx.endpoint, key, &self.ctx.organization, site_id) {
                Ok(true) => {
                    return Ok(format!(
                        "Site {nice} (id {site_id}) of organization {} online",
                        self.ctx.organization
                    ));
                }
                Ok(false) | Err(_) if started.elapsed() < HEALTH_LIMIT => std::thread::sleep(HEALTH_RETRY),
                Ok(false) => {
                    return Err(format!(
                        "Site {nice} (id {site_id}) is not online after {} s",
                        HEALTH_LIMIT.as_secs()
                    ));
                }
                Err(error) => return Err(error),
            }
        }
    }
}

impl SiteReady for Pangolin {
    fn identity(&self, ctid: u32) -> Result<String, String> {
        let key = self.key()?;
        let (nice, id) = self.completed(&key, ctid)?;
        Ok(format!(
            "CT {ctid}: Site {nice} (id {id}) of organization {} matches its journal and saved credential; kept",
            self.ctx.organization
        ))
    }

    fn ready(&self, ctid: u32, say: &dyn Fn(&str)) -> Result<String, String> {
        let key = self.key()?;
        let (nice, id) = self.completed(&key, ctid)?;
        self.wait_online(&key, &nice, id, say)
    }

    fn removal(&self, ctid: u32) -> Result<String, String> {
        let key = self.key()?;
        let storage = Storage::open(&self.ctx.managed_key(ctid))?;
        Ok(match self.owned_site(&key, &storage, ctid)? {
            Some((nice, Some(id))) => format!(
                "CT {ctid}: Site {nice} (id {id}) is deleted from Pangolin, its credential and journal from this desktop"
            ),
            Some((nice, None)) => format!(
                "CT {ctid}: Site {nice} is not in Pangolin; its credential and journal are deleted from this desktop"
            ),
            None => format!("CT {ctid}: no Site journal on this desktop; nothing in Pangolin is changed"),
        })
    }

    fn remove(&self, ctid: u32, say: &dyn Fn(&str)) -> Result<String, String> {
        let key = self.key()?;
        // Held until the files are gone: no other HomeStack run touches this Site.
        let storage = Storage::open(&self.ctx.managed_key(ctid))?;
        let Some((nice, id)) = self.owned_site(&key, &storage, ctid)? else {
            return Ok(format!("CT {ctid}: no Site journal on this desktop"));
        };
        if let Some(id) = id {
            say(&format!("CT {ctid} · deleting Site {nice} from Pangolin"));
            let stop = AtomicBool::new(false);
            let deleted = client(&self.ctx.endpoint, &key, &stop).request(
                "DELETE",
                &format!("/v1/site/{id}"),
                None,
                Probe::Organization,
            );
            if site(&self.ctx.endpoint, &key, &self.ctx.organization, &nice)?.is_some() {
                return Err(match deleted {
                    Ok((403, _)) => {
                        "The HomeStack API key may not delete Sites (deleteSite); refresh Step 7".to_owned()
                    }
                    Ok((status, _)) => {
                        format!("Pangolin refused to delete Site {nice} (HTTP {status}); nothing else is removed")
                    }
                    Err(problem) => format!(
                        "Site {nice} was not deleted ({}); nothing else is removed",
                        explain(problem)
                    ),
                });
            }
        }
        storage.remove()?;
        Ok(format!(
            "CT {ctid}: Site {nice} deleted from Pangolin, its credential and journal from this desktop"
        ))
    }
}

impl Pangolin {
    /// The Site the journal of `ctid` names, if there is a journal: its name
    /// and, when Pangolin still has it with the journal's IDs, its ID.
    fn owned_site(
        &self,
        key: &Credential,
        storage: &Storage,
        ctid: u32,
    ) -> Result<Option<(String, Option<u64>)>, String> {
        let Some(record) = storage.load::<SiteRecord>()? else {
            return Ok(None);
        };
        owned(&self.ctx, &record, ctid)?;
        match site(&self.ctx.endpoint, key, &self.ctx.organization, &record.site)? {
            Some((id, newt)) if Some(id) == record.site_id && newt == record.newt_id => {
                Ok(Some((record.site, Some(id))))
            }
            Some(_) => Err(format!(
                "Site {} in Pangolin is not the one its journal records; nothing is removed",
                record.site
            )),
            None => Ok(Some((record.site, None))),
        }
    }
}

/// The instance under initialization; its journal stays locked from
/// `prepare` until its Site is online.
struct Held {
    ctid: u32,
    storage: Storage,
    record: SiteRecord,
    credential: Credential,
}

/// The Pangolin context and the deploy token: what a new managed Site
/// container needs once, for its credential, which reaches it through the
/// Proxmox console (`hs_deploy::console`), never SSH.
pub struct Initializer {
    pangolin: Pangolin,
    config: DeployConfig,
    pve: hs_pve::PveClient,
    held: Option<Held>,
}

impl Initializer {
    pub fn open(pangolin: Pangolin, config: DeployConfig) -> Result<Self, String> {
        let pve = hs_deploy::deploy::deploy_client(&config)?;
        Ok(Self {
            pangolin,
            config,
            pve,
            held: None,
        })
    }

    /// The tunnel is still verified.
    #[must_use]
    pub fn alive(&self) -> bool {
        self.pangolin.alive()
    }

    #[must_use]
    pub fn describe(&self) -> String {
        format!("Pangolin: {}", self.pangolin.describe())
    }
}

impl SiteInit for Initializer {
    fn review(&mut self, ctid: u32, nice: &str, home: Option<&str>) -> Result<String, String> {
        self.held = None;
        let key = self.pangolin.key()?;
        let ctx = &self.pangolin.ctx;
        let path = ctx.managed_key(ctid);
        let storage = Storage::open(&path)?;
        let record: Option<SiteRecord> = storage.load()?;
        let credential = if storage.has_credential() || storage.has_pending() {
            Some(storage.recover_credential(&path)?)
        } else {
            None
        };
        drop(storage);
        Ok(
            match reconcile(ctx, &key, ctid, nice, home, record.as_ref(), credential.as_ref())? {
                SiteStep::Create if credential.is_some() => {
                    format!("create Site {nice} with the credential the interrupted run saved")
                }
                SiteStep::Create => {
                    format!("create Site {nice} with a new credential")
                }
                SiteStep::Reuse(id) | SiteStep::Adopt(id) => {
                    format!("Site {nice} (id {id}) of the interrupted run, kept")
                }
            },
        )
    }

    fn prepare(&mut self, node: &str, ctid: u32, nice: &str, home: &str, say: &dyn Fn(&str)) -> Result<(), String> {
        self.held = None;
        let key = self.pangolin.key()?;
        let ctx = &self.pangolin.ctx;
        let path = ctx.managed_key(ctid);
        let storage = Storage::open(&path)?;
        let record: Option<SiteRecord> = storage.load()?;
        let credential = if storage.has_credential() || storage.has_pending() {
            Some(storage.recover_credential(&path)?)
        } else {
            None
        };
        let step = reconcile(ctx, &key, ctid, nice, Some(home), record.as_ref(), credential.as_ref())?;
        let (mut record, credential) = journal(
            &storage,
            &path,
            |newt_id| SiteRecord {
                target: ctx.target.clone(),
                organization: ctx.organization.clone(),
                node: node.to_owned(),
                site: nice.to_owned(),
                newt_id,
                site_id: None,
                ctid: Some(ctid),
                stage: SiteStage::Prepared,
                at: stamp(),
                home: Some(home.to_owned()),
            },
            say,
        )?;
        owned(ctx, &record, ctid)?;
        if record.site != nice {
            return Err("The Site journal names another Site; nothing is changed".to_owned());
        }
        let site_id = match step {
            SiteStep::Reuse(id) | SiteStep::Adopt(id) => id,
            SiteStep::Create => create_site(ctx, &key, &storage, &mut record, &credential, say)?,
        };
        if record.site_id != Some(site_id) || record.stage < SiteStage::Created {
            record.site_id = Some(site_id);
            record.stage = record.stage.max(SiteStage::Created);
            record.at = stamp();
            storage.save(&record)?;
        }
        say(&format!(
            "  Site {nice} (id {site_id}) of organization {}",
            ctx.organization
        ));
        self.held = Some(Held {
            ctid,
            storage,
            record,
            credential,
        });
        Ok(())
    }

    fn install(&mut self, node: &str, ctid: u32, say: &dyn Fn(&str)) -> Result<(), String> {
        let held = self
            .held
            .as_mut()
            .filter(|held| held.ctid == ctid)
            .ok_or("Site initialization was not prepared")?;
        let content = configuration(&self.pangolin.ctx.url, &held.credential)?;
        say("  installing the Site credential through the container's console");
        // Echo off first; the receiver reads the configuration from the here-document.
        let script = Zeroizing::new(format!(
            "stty -echo\nenv HOMESTACK_MANAGED_HOME=1 {TOOL} <<'HOMESTACK-SITE'\n{}HOMESTACK-SITE\necho \"HOMESTACK-SITE-\"\"DIGEST $(env HOMESTACK_MANAGED_HOME=1 {TOOL} --digest)\"\nexit\n",
            content.as_str()
        ));
        let output = tokio::runtime::Handle::current().block_on(hs_deploy::console::run(
            &self.pve,
            &self.config,
            node,
            ctid,
            &script,
            MARKER,
            CONSOLE_LIMIT,
        ))?;
        let installed = output
            .lines()
            .find_map(|line| line.split_once(MARKER).map(|(_, digest)| digest))
            .map(str::trim)
            .unwrap_or_default();
        if installed != digest(&content) {
            return Err(format!("CT {ctid}: the installed credential does not match"));
        }
        say("  credential installed in /home/app/pangolin-site and verified by its digest");
        if held.record.stage < SiteStage::Installed {
            held.record.stage = SiteStage::Installed;
            held.record.at = stamp();
            held.storage.save(&held.record)?;
        }
        Ok(())
    }

    fn online(&mut self, ctid: u32, say: &dyn Fn(&str)) -> Result<String, String> {
        let key = self.pangolin.key()?;
        let held = self
            .held
            .as_ref()
            .filter(|held| held.ctid == ctid && held.record.stage >= SiteStage::Installed)
            .ok_or("Site credential was not installed")?;
        let site_id = held.record.site_id.ok_or("Invalid Site journal.")?;
        let shown = self.pangolin.wait_online(&key, &held.record.site, site_id, say)?;
        let ctx = &self.pangolin.ctx;
        match site(&ctx.endpoint, &key, &ctx.organization, &held.record.site)? {
            Some((id, newt)) if id == site_id && newt == held.record.newt_id => {}
            _ => return Err("The online Site no longer matches its journal".to_owned()),
        }
        let mut held = self.held.take().ok_or("Site initialization was not prepared")?;
        held.record.stage = SiteStage::Online;
        held.record.at = stamp();
        held.storage.save(&held.record)?;
        Ok(shown)
    }
}
