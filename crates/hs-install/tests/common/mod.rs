//! A fake Proxmox cluster on disk (`deploy/pve/tests/fake-pve`, the same fake
//! the shell tests of `homestack-pve.sh` use) and a root shell that sees it.
#![allow(dead_code)]
#![allow(clippy::expect_used, clippy::unwrap_used)]

use std::fs;
use std::path::{Path, PathBuf};

use hs_install::{Applied, Command, InstallError, LocalShell, Plan, Settings, apply, make_plan, probe};

pub const ROLES_TARGET: &str = "HomeStack\tPool.Audit,VM.Allocate,VM.Audit,VM.Config.Cloudinit,VM.Config.Disk,VM.Config.Options,VM.GuestAgent.Audit,VM.GuestAgent.FileRead,VM.GuestAgent.Unrestricted,VM.Migrate,VM.PowerMgmt
HomeStackGold\tPool.Audit,VM.Audit,VM.Clone,VM.GuestAgent.Audit,VM.GuestAgent.FileRead,VM.GuestAgent.Unrestricted
HomeStackStorage\tDatastore.AllocateSpace,Datastore.Audit
HomeStackDeploy\tPool.Audit,VM.Allocate,VM.Audit,VM.Config.CPU,VM.Config.Disk,VM.Config.Memory,VM.Config.Network,VM.Config.Options,VM.PowerMgmt
HomeStackDeployNodes\tSys.AccessNetwork
HomeStackDeployTemplates\tDatastore.AllocateSpace,Datastore.AllocateTemplate,Datastore.Audit";

pub const HS_USER: &str = "homestack@pve";

fn manifest() -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR"))
}

pub fn fake_tool() -> PathBuf {
    manifest().join("../../deploy/pve/tests/fake-pve")
}

pub fn cloud_init(name: &str) -> String {
    fs::read_to_string(manifest().join("../../deploy/cloud-init").join(name)).unwrap_or_default()
}

/// One fake cluster; the first node is the one the root shell is on.
pub struct Fake {
    _work: tempfile::TempDir,
    pub dir: PathBuf,
    pub desktop: PathBuf,
}

/// What a run printed and did.
pub struct Run {
    pub ok: bool,
    pub out: String,
    pub plan: Option<Plan>,
    pub applied: Option<Applied>,
    pub error: Option<InstallError>,
}

/// The arguments most tests use.
pub fn standard(test: bool) -> Settings {
    Settings {
        test,
        storages: vec!["local-lvm".into()],
        snippet_storage: Some("local".into()),
        service_storages: vec!["local-lvm".into()],
        template_storage: Some("local".into()),
        ..Settings::default()
    }
}

impl Fake {
    pub fn new(nodes: &[&str]) -> Self {
        let work = tempfile::tempdir().expect("temporary directory");
        let dir = work.path().join("cluster");
        let desktop = work.path().join("desktop");
        fs::create_dir_all(dir.join("bin")).expect("bin");
        fs::create_dir_all(dir.join("vz/snippets")).expect("vz");
        fs::create_dir_all(&desktop).expect("desktop");
        for tool in ["pvesh", "pveum", "pvesm", "ssh", "openssl", "id", "hostname"] {
            std::os::unix::fs::symlink(
                fake_tool().canonicalize().expect("fake-pve"),
                dir.join("bin").join(tool),
            )
            .expect("symlink");
        }
        let fake = Self {
            _work: work,
            dir,
            desktop,
        };
        for file in ["calls.log", "acl.tsv", "tokens.tsv", "pools.tsv"] {
            fake.write(file, "");
        }
        fake.write("users", "root@pam\n");
        fake.write(
            "roles.tsv",
            "PVEAuditor\tDatastore.Audit,Pool.Audit,Sys.Audit,VM.Audit\t1\nPVESDNUser\tSDN.Audit,SDN.Use\t1\n",
        );
        fake.write("options.json", "{}\n");
        fake.write(
            "storage.json",
            r#"[{"storage":"local","type":"dir","path":"/var/lib/vz","content":"iso,vztmpl,backup,snippets"},
 {"storage":"local-lvm","type":"lvmthin","content":"images,rootdir"},
 {"storage":"worker-roots","type":"lvmthin","content":"images","nodes":"pve1"}]
"#,
        );
        let mut table = String::new();
        let mut trusted = String::new();
        for (index, name) in nodes.iter().enumerate() {
            let digits: String = name.chars().filter(char::is_ascii_digit).collect();
            table.push_str(&format!("{name}\t192.0.2.{digits}\t1\t{}\n", u8::from(index == 0)));
            trusted.push_str(&format!("{name}.example.net\n"));
        }
        fake.write("nodes.tsv", &table);
        fake.write("trusted", &trusted);
        if nodes.len() > 1 {
            fake.write("cluster", "");
        }
        fake.write(
            "vms.tsv",
            &format!(
                "101\t{0}\tgold\thomestack-gold\tvirtio=BC:24:11:00:00:01,bridge=vmbr0\tuser=local:snippets/gold-user.yaml\n\
                 100\t{0}\tdesktop\t\tvirtio=BC:24:11:00:00:02,bridge=vmbr0\t\n",
                nodes[0]
            ),
        );
        fs::write(fake.dir.join("vz/snippets/gold-user.yaml"), "owner\n").expect("owner snippet");
        fake
    }

    /// The hand-made state of phases 0-4 (`docs/migration/phase-4b.md`).
    pub fn today() -> Self {
        let fake = Self::new(&["pve2", "pve1", "pve3"]);
        fake.append(
            "roles.tsv",
            "HomeStackServerDisks\tVM.Config.Disk\t0\n\
             HomeStackServerGoldGuest\tVM.GuestAgent.FileRead,VM.GuestAgent.Unrestricted\t0\n\
             HomeStackTest\tDatastore.AllocateSpace,Datastore.Audit,VM.Allocate,VM.Audit,VM.Clone,VM.Config.Cloudinit,VM.Config.Disk,VM.Config.Options,VM.GuestAgent.Audit,VM.GuestAgent.FileRead,VM.GuestAgent.Unrestricted,VM.Migrate,VM.PowerMgmt\t0\n\
             HomeStackTestGold\tVM.Audit,VM.Clone\t0\n\
             HomeStackTestStorage\tDatastore.AllocateSpace,Datastore.Audit\t0\n",
        );
        fake.append("users", "homestack@pve\n");
        fake.write("tokens.tsv", "homestack@pve\tserver\t1\nhomestack@pve\ttest\t1\n");
        fake.write(
            "pools.tsv",
            "homestack-test\t\tHomeStack migration tests, VMIDs 240-249\n",
        );
        fake.write(
            "acl.tsv",
            "/\tuser\thomestack@pve\tPVEAuditor
/\ttoken\thomestack@pve!server\tPVEAuditor
/\ttoken\thomestack@pve!test\tPVEAuditor
/\tgroup\tautomation\tPVESysAdmin
/pool/homestack-test\tuser\thomestack@pve\tPVEAuditor
/pool/homestack-test\tuser\thomestack@pve\tHomeStackTest
/pool/homestack-test\ttoken\thomestack@pve!test\tHomeStackTest
/sdn/zones/localnetwork/vmbr0\tuser\thomestack@pve\tPVEAuditor
/sdn/zones/localnetwork/vmbr0\tuser\thomestack@pve\tPVESDNUser
/sdn/zones/localnetwork/vmbr0\ttoken\thomestack@pve!test\tPVESDNUser
/storage/local\tuser\thomestack@pve\tPVEAuditor
/storage/local\tuser\thomestack@pve\tHomeStackTestStorage
/storage/local\ttoken\thomestack@pve!test\tHomeStackTestStorage
/storage/local-lvm\tuser\thomestack@pve\tHomeStackTestStorage
/storage/local-lvm\tuser\thomestack@pve\tPVEAuditor
/storage/local-lvm\ttoken\thomestack@pve!test\tHomeStackTestStorage
/vms\tuser\thomestack@pve\tHomeStackServerDisks
/vms\tuser\thomestack@pve\tPVEAuditor
/vms\ttoken\thomestack@pve!server\tPVEAuditor
/vms\ttoken\thomestack@pve!server\tHomeStackServerDisks
/vms/101\tuser\thomestack@pve\tHomeStackServerGoldGuest
/vms/101\tuser\thomestack@pve\tHomeStackTestGold
/vms/101\tuser\thomestack@pve\tPVEAuditor
/vms/101\tuser\thomestack@pve\tHomeStackServerDisks
/vms/101\ttoken\thomestack@pve!server\tHomeStackServerGoldGuest
/vms/101\ttoken\thomestack@pve!server\tHomeStackServerDisks
/vms/101\ttoken\thomestack@pve!server\tPVEAuditor
/vms/101\ttoken\thomestack@pve!test\tHomeStackTestGold
",
        );
        for name in ["homestack-vendor-create.yaml", "homestack-vendor-keep.yaml"] {
            fs::write(fake.snippet_dir("local").join(name), cloud_init(name)).expect("snippet");
        }
        fake
    }

    pub fn path(&self, name: &str) -> PathBuf {
        self.dir.join(name)
    }

    pub fn read(&self, name: &str) -> String {
        fs::read_to_string(self.path(name)).unwrap_or_default()
    }

    pub fn write(&self, name: &str, text: &str) {
        fs::write(self.path(name), text).expect("write state");
    }

    pub fn append(&self, name: &str, text: &str) {
        self.write(name, &(self.read(name) + text));
    }

    /// Replace `from` with `to` in a state file.
    pub fn replace(&self, name: &str, from: &str, to: &str) {
        self.write(name, &self.read(name).replace(from, to));
    }

    pub fn shell(&self) -> LocalShell {
        let path = format!(
            "{}:{}",
            self.dir.join("bin").display(),
            std::env::var("PATH").unwrap_or_default()
        );
        LocalShell::new(vec![
            ("PATH".into(), path),
            ("HS_FAKE".into(), self.dir.display().to_string()),
        ])
    }

    /// `install` or `uninstall` as the command line does it: probe, plan,
    /// and apply when asked.
    pub fn run(&self, command: Command, settings: &Settings, yes: bool) -> Run {
        let mut settings = settings.clone();
        settings.token_dir = self.desktop.clone();
        let mut root = self.shell();
        let mut out = String::new();
        let access = match probe(&mut root) {
            Ok(access) => access,
            Err(error) => {
                return Run {
                    ok: false,
                    out: error.to_string(),
                    plan: None,
                    applied: None,
                    error: Some(error),
                };
            }
        };
        out.push_str(&format!("{}: {}\n", access.host, access.topology.describe()));
        let plan = match make_plan(&mut root, &access, command, &settings) {
            Ok(plan) => plan,
            Err(error) => {
                out.push_str(&error.to_string());
                return Run {
                    ok: false,
                    out,
                    plan: None,
                    applied: None,
                    error: Some(error),
                };
            }
        };
        out.push_str(&plan.text());
        let mut applied = None;
        let mut error = None;
        if yes && !plan.steps.is_empty() {
            let mut progress = |line: &str| {
                out.push_str(line);
                out.push('\n');
            };
            match apply(&mut root, &plan, &settings, &mut progress) {
                Ok(done) => applied = Some(done),
                Err(failure) => error = Some(failure),
            }
        }
        if let Some(failure) = &error {
            out.push_str(&format!("{failure}\n"));
        }
        let ok = error.is_none() && plan.node_errors.is_empty();
        Run {
            ok,
            out,
            plan: Some(plan),
            applied,
            error,
        }
    }

    pub fn install(&self, settings: &Settings) -> Run {
        self.run(Command::Install, settings, true)
    }

    pub fn uninstall(&self, settings: &Settings) -> Run {
        self.run(Command::Uninstall, settings, true)
    }

    /// The `pveum` calls that changed something.
    pub fn changes(&self) -> usize {
        self.read("calls.log")
            .lines()
            .filter(|line| line.starts_with("pveum "))
            .count()
    }

    pub fn clear_calls(&self) {
        self.write("calls.log", "");
    }

    /// The ACL entries of `homestack@pve` and its tokens, sorted.
    pub fn homestack_acl(&self) -> Vec<String> {
        let mut lines: Vec<String> = self
            .read("acl.tsv")
            .lines()
            .filter(|line| {
                let who = line.split('\t').nth(2).unwrap_or_default();
                who == HS_USER || who.starts_with("homestack@pve!")
            })
            .map(str::to_owned)
            .collect();
        lines.sort();
        lines
    }

    pub fn snippet_dir(&self, node: &str) -> PathBuf {
        if node == "local" {
            self.dir.join("vz/snippets")
        } else {
            self.dir.join("nodes").join(node).join("vz/snippets")
        }
    }

    pub fn has_snippets(&self, node: &str) -> bool {
        ["homestack-vendor-create.yaml", "homestack-vendor-keep.yaml"]
            .iter()
            .all(|name| fs::read_to_string(self.snippet_dir(node).join(name)).ok() == Some(cloud_init(name)))
    }

    pub fn has_any_snippet(&self, node: &str) -> bool {
        fs::read_dir(self.snippet_dir(node))
            .map(|entries| {
                entries
                    .flatten()
                    .any(|entry| entry.file_name().to_string_lossy().starts_with("homestack-vendor-"))
            })
            .unwrap_or(false)
    }

    pub fn token_files(&self) -> Vec<String> {
        let mut names: Vec<String> = fs::read_dir(&self.desktop)
            .map(|entries| {
                entries
                    .flatten()
                    .map(|e| e.file_name().to_string_lossy().into_owned())
                    .collect()
            })
            .unwrap_or_default();
        names.sort();
        names
    }
}

pub fn sorted_lines(text: &str) -> Vec<String> {
    let mut lines: Vec<String> = text.lines().filter(|l| !l.is_empty()).map(str::to_owned).collect();
    lines.sort();
    lines
}

pub fn exists(path: &Path) -> bool {
    path.exists()
}
