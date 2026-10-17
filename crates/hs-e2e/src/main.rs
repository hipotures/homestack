//! Minimal disposable PVE bootstrap. This does not run the HomeStack catalogue.

mod access;
mod client;
mod domain;
mod image;
mod install;
mod internet;
mod network;
mod phases;
mod proxy;
mod registry_proxy;
mod seed;
mod service;
mod service_fixture;
mod tunnel;
mod vm;

use std::net::{Ipv4Addr, SocketAddr};
use std::os::unix::fs::{OpenOptionsExt, PermissionsExt};
use std::path::{Path, PathBuf};
use std::process::ExitCode;
use std::time::Duration;

use serde_json::{Value, json};

type Result<T> = std::result::Result<T, Box<dyn std::error::Error + Send + Sync>>;

const USAGE: &str = "homestack-e2e run [--memory-mib N] [--cpus N] [--disk-gib N]\n                  [--timeout-seconds N] [--message TEXT] [--artifacts DIRECTORY]\n                  [--pause-after PHASE[,PHASE...]] [--pause-on-failure]\n                  [--web-bind IPV4] [--web-port PORT]\n                  [--fixtures DIRECTORY] [--spaceship-file FILE]\n                  [--local-dns IPV4] [--tsig-file FILE]\n                  [--runner-ssh DESTINATION] [--runner-ssh-port PORT]\nhomestack-e2e resume --artifacts DIRECTORY\nhomestack-e2e profile --artifacts DIRECTORY (register an already paused lab as profile e2e)\nhomestack --profil e2e [COMMAND]\nhomestack-e2e prepare --artifacts DIRECTORY [--local-dns IPV4] [--spaceship-file FILE] [--tsig-file FILE]\nhomestack-e2e cleanup --all [--fixtures DIRECTORY] [--spaceship-file FILE] [--tsig-file FILE]\n\nPhases: preflight, domain, image, boot, kernel, proxmox, network, homestack, verify, shutdown; cleanup always runs.\nDefaults: 1 vCPU, 2048 MiB RAM, 12 GiB sparse disk, 1800 active seconds.\nInspection pauses have no time limit and require an explicit resume.\nThe panel uses pve.e2e.homestack.app with a public Let's Encrypt certificate.\nDNS and certificate fixtures persist until cleanup --all.\nThe web forward binds to 127.0.0.1 on a free port unless explicitly configured.\nDownloads Debian 13 qcow2, installs Proxmox, verifies SSH, HTTPS and web login,\npowers off, and deletes all temporary VM data. No host network changes.\nRequires Linux x86_64, /dev/kvm, qemu-system-x86_64, qemu-img,\nssh, ssh-keygen, mkfs.vfat, mcopy, openssl, python3 and ip; dig/nsupdate for local DNS.\nNo host root privileges required.";

struct Options {
    memory: u32,
    cpus: u32,
    disk: u32,
    timeout: u64,
    message: String,
    artifacts: PathBuf,
    pause_after: Vec<String>,
    pause_on_failure: bool,
    web_bind: Ipv4Addr,
    web_port: u16,
    fixtures: PathBuf,
    spaceship_file: PathBuf,
    local_dns: Option<Ipv4Addr>,
    tsig_file: PathBuf,
    runner_ssh: Option<String>,
    runner_ssh_port: Option<u16>,
}

impl Options {
    fn parse(args: &[String], run_id: &str) -> Result<Self> {
        let cleanup = args.first().map(String::as_str) == Some("cleanup");
        if args.first().map(String::as_str) != Some("run") && !cleanup {
            return Err("expected 'run'; use --help".into());
        }
        let home = PathBuf::from(std::env::var_os("HOME").ok_or("HOME is not set")?);
        let config = std::env::var_os("XDG_CONFIG_HOME")
            .map_or_else(|| home.join(".config"), PathBuf::from)
            .join("homestack");
        let mut options = Self {
            memory: 2048,
            cpus: 1,
            disk: 12,
            timeout: 1800,
            message: "HomeStack disposable PVE bootstrap".into(),
            artifacts: PathBuf::from("target/e2e").join(run_id),
            pause_after: Vec::new(),
            pause_on_failure: false,
            web_bind: Ipv4Addr::LOCALHOST,
            web_port: 0,
            fixtures: std::env::var_os("XDG_STATE_HOME")
                .map_or_else(|| home.join(".local/state"), PathBuf::from)
                .join("homestack-e2e"),
            spaceship_file: config.join("spaceship-dns"),
            local_dns: None,
            tsig_file: config.join("dns-tsig.key"),
            runner_ssh: None,
            runner_ssh_port: None,
        };
        let mut all = false;
        let mut rest = args[1..].iter();
        while let Some(flag) = rest.next() {
            if cleanup && flag == "--all" {
                all = true;
                continue;
            }
            if cleanup && !matches!(flag.as_str(), "--fixtures" | "--spaceship-file" | "--tsig-file") {
                return Err(format!("unknown cleanup argument {flag}").into());
            }
            if flag == "--pause-on-failure" {
                options.pause_on_failure = true;
                continue;
            }
            let value = rest.next().ok_or_else(|| format!("{flag} needs a value"))?;
            match flag.as_str() {
                "--memory-mib" => options.memory = value.parse()?,
                "--cpus" => options.cpus = value.parse()?,
                "--disk-gib" => options.disk = value.parse()?,
                "--timeout-seconds" => options.timeout = value.parse()?,
                "--message" => options.message.clone_from(value),
                "--artifacts" => options.artifacts = PathBuf::from(value),
                "--pause-after" => {
                    for phase in value.split(',') {
                        if !phases::NAMES.contains(&phase) {
                            return Err(
                                format!("unknown phase {phase:?}; expected {}", phases::NAMES.join(", ")).into(),
                            );
                        }
                        if !options.pause_after.iter().any(|name| name == phase) {
                            options.pause_after.push(phase.to_owned());
                        }
                    }
                }
                "--web-bind" => options.web_bind = value.parse()?,
                "--web-port" => options.web_port = value.parse()?,
                "--fixtures" => options.fixtures = PathBuf::from(value),
                "--spaceship-file" => options.spaceship_file = PathBuf::from(value),
                "--local-dns" => options.local_dns = Some(value.parse()?),
                "--tsig-file" => options.tsig_file = PathBuf::from(value),
                "--runner-ssh" => {
                    access::validate_destination(value)?;
                    options.runner_ssh = Some(value.clone());
                }
                "--runner-ssh-port" => {
                    let port: u16 = value.parse()?;
                    if port == 0 {
                        return Err("--runner-ssh-port must be nonzero".into());
                    }
                    options.runner_ssh_port = Some(port);
                }
                _ => return Err(format!("unknown argument {flag}").into()),
            }
        }
        if cleanup && !all {
            return Err("cleanup requires --all; normal run cleanup retains DNS and certificates".into());
        }
        if !(2048..=16384).contains(&options.memory)
            || !(1..=8).contains(&options.cpus)
            || !(12..=64).contains(&options.disk)
            || !(60..=7200).contains(&options.timeout)
        {
            return Err(
                "resource limits: memory 2048–16384 MiB, CPUs 1–8, disk 12–64 GiB, installation 60–7200 s".into(),
            );
        }
        if options.web_bind.is_unspecified() || options.web_bind.is_multicast() || options.web_bind.is_broadcast() {
            return Err("--web-bind must name a specific local unicast IPv4 address".into());
        }
        Ok(options)
    }
}

#[tokio::main(worker_threads = 2)]
async fn main() -> ExitCode {
    match execute().await {
        Ok(()) => ExitCode::SUCCESS,
        Err(error) => {
            eprintln!("homestack-e2e: {}", describe(error.as_ref()));
            ExitCode::FAILURE
        }
    }
}

async fn execute() -> Result<()> {
    let args: Vec<String> = std::env::args().skip(1).collect();
    if args.first().is_some_and(|arg| arg == "profile") {
        if args.len() != 3 || args[1] != "--artifacts" {
            return Err("usage: homestack-e2e profile --artifacts DIRECTORY".into());
        }
        client::register(Path::new(&args[2]))?;
        eprintln!("Profile e2e registered. Open it with homestack --profil e2e.");
        return Ok(());
    }
    if args.first().map(String::as_str) == Some("client") {
        if args.len() < 4 || args[1] != "--profile" || args[2] != "e2e" || args[3] != "--" {
            return Err("open the client with homestack --profil e2e [COMMAND]".into());
        }
        return tokio::select! {
            result = client::launch_profile(&args[4..]) => result,
            _ = interrupted() => Err("E2E client interrupted; the VM remains running".into()),
        };
    }
    if args.first().is_some_and(|arg| arg == "prepare") {
        for pair in args[1..].chunks(2) {
            if !matches!(
                pair[0].as_str(),
                "--artifacts" | "--local-dns" | "--spaceship-file" | "--tsig-file"
            ) {
                return Err(format!("unknown prepare argument {}", pair[0]).into());
            }
        }
        if !args.iter().any(|arg| arg == "--artifacts") {
            return Err("prepare requires --artifacts DIRECTORY".into());
        }
        let mut preparation = args.clone();
        preparation[0] = "run".into();
        let mut options = Options::parse(&preparation, "prepare")?;
        let report: Value = serde_json::from_slice(&std::fs::read(options.artifacts.join("report.json"))?)?;
        if report["status"] != "paused" {
            return Err("prepare requires a live paused run".into());
        }
        let dir = Path::new(
            report["temporary_directory"]
                .as_str()
                .ok_or("run has no private directory")?,
        );
        drop(std::os::unix::net::UnixStream::connect(dir.join("inspection.sock"))?);
        options.fixtures = PathBuf::from(
            report["domain"]["directory"]
                .as_str()
                .ok_or("run has no fixture directory")?,
        );
        service::prepare(&options, dir, &options.artifacts, &report).await?;
        return Ok(());
    }
    if args.first().map(String::as_str) == Some("resume") {
        if args.len() != 3 || args[1] != "--artifacts" {
            return Err("usage: homestack-e2e resume --artifacts DIRECTORY".into());
        }
        return phases::resume(Path::new(&args[2])).await;
    }
    if args.first().map(String::as_str) == Some("proxy-connect") {
        use tokio::io::AsyncWriteExt;
        let port: u16 = args.get(1).ok_or("missing local proxy port")?.parse()?;
        let connection = tokio::net::TcpStream::connect((Ipv4Addr::LOCALHOST, port)).await?;
        let (mut read, mut write) = connection.into_split();
        tokio::try_join!(
            async {
                tokio::io::copy(&mut tokio::io::stdin(), &mut write).await?;
                write.shutdown().await
            },
            async { tokio::io::copy(&mut read, &mut tokio::io::stdout()).await.map(|_| ()) }
        )?;
        return Ok(());
    }
    if args.first().is_some_and(|arg| arg == "--help" || arg == "-h") {
        println!("{USAGE}");
        return Ok(());
    }
    let run_id = uuid::Uuid::new_v4().to_string();
    let mut options = Options::parse(&args, &run_id)?;
    let mut fixture = domain::Fixture::open(&options.fixtures, &options.spaceship_file, &options.tsig_file)?;
    if args[0] == "cleanup" {
        if options.fixtures.join("service/manifest.json").is_file() {
            service_fixture::Fixture::open(&options.fixtures, &options.spaceship_file, &options.tsig_file)?
                .reset()
                .await?;
        }
        return fixture.reset().await;
    }
    access::configure(&mut options);
    if options.runner_ssh.is_none() && options.runner_ssh_port.is_some() {
        return Err("--runner-ssh-port requires --runner-ssh or an SSH session".into());
    }
    if let Some(parent) = options.artifacts.parent().filter(|p| !p.as_os_str().is_empty()) {
        std::fs::create_dir_all(parent)?;
    }
    std::fs::create_dir(&options.artifacts)?;
    std::fs::set_permissions(&options.artifacts, std::fs::Permissions::from_mode(0o700))?;
    let artifacts = options.artifacts.canonicalize()?;
    if artifacts.to_string_lossy().contains(',') {
        return Err("artifact directory must not contain commas (QEMU argument syntax)".into());
    }
    // Keep disk images off /tmp, which is often RAM-backed on small hosts.
    let work_root = PathBuf::from("target/e2e-work");
    std::fs::create_dir_all(&work_root)?;
    let private = tempfile::Builder::new()
        .prefix("hs-e2e-")
        .tempdir_in(work_root.canonicalize()?)?;
    let private_path = private.path().to_owned();
    let mut running = None;
    let mut report = json!({
        "run_id": run_id, "status": "running", "scope": "debian-qcow2-to-proxmox",
        "resources": {"cpus": options.cpus, "memory_mib": options.memory, "disk_gib": options.disk},
        "artifacts": artifacts, "temporary_directory": private_path,
        "phases": {}, "web_bind": options.web_bind.to_string(),
    });
    save_report(&artifacts, &report)?;
    eprintln!("Run {run_id}; artifacts: {}", artifacts.display());
    let outcome = tokio::select! {
        result = run(&options, &run_id, &private_path, &artifacts, &mut running, &mut report, &mut fixture) => result,
        _ = interrupted() => Err("run interrupted; removing the VM".into()),
    };
    if let Some(phase) = report["phase"].as_str().map(str::to_owned)
        && report["phases"][&phase]["status"] == "running"
    {
        report["phases"][&phase]["status"] = json!("failed");
        report["phases"][&phase]["error"] = json!("run interrupted");
    }
    eprintln!("Removing temporary VM resources...");
    phases::begin(&mut report, "cleanup");
    report["status"] = json!("running");
    report["control_socket"] = Value::Null;
    save_report(&artifacts, &report)?;
    let cleanup_started = tokio::time::Instant::now();
    let graceful = running.as_ref().is_some_and(|vm: &vm::Vm| vm.shutdown_confirmed);
    let termination = match running.as_mut() {
        Some(vm) => vm.terminate(),
        None => Ok(()),
    };
    drop(running);
    // Reap QEMU before unlinking its disks or the seed containing private keys.
    let mut cleanup = match termination {
        Ok(()) => private.close().map_err(Into::into),
        Err(error) => {
            // Do not unlink a running process's disks after an unconfirmed kill.
            let _ = private.keep();
            Err(error)
        }
    };
    let removed = !private_path.exists();
    if cleanup.is_ok() && !removed {
        cleanup = Err("temporary VM directory remains after cleanup".into());
    }
    if let Err(error) = fixture.cleanup_challenge().await {
        eprintln!("ACME challenge cleanup failed; the persistent journal retains it for retry: {error}");
        if cleanup.is_ok() {
            cleanup = Err(error);
        }
    }
    report["graceful_shutdown"] = json!(graceful);
    report["cleanup"] = json!({"passed": cleanup.is_ok() && removed, "temporary_directory_removed": removed,
        "error": cleanup.as_ref().err().map(|error| describe(error.as_ref()))});
    report["status"] = json!(if outcome.is_ok() && cleanup.is_ok() && removed {
        "passed"
    } else {
        "failed"
    });
    report["error"] = json!(outcome.as_ref().err().map(|error| describe(error.as_ref())));
    report["retained_fixture_directory"] = json!(options.fixtures);
    phases::finish(&mut report, "cleanup", &cleanup, cleanup_started.elapsed());
    save_report(&artifacts, &report)?;
    outcome?;
    cleanup?;
    eprintln!("Passed: PVE verified, powered off, QEMU reaped and VM files removed.");
    Ok(())
}

fn save_report(dir: &Path, value: &Value) -> Result<()> {
    let temporary = dir.join("report.json.tmp");
    std::fs::write(&temporary, serde_json::to_vec_pretty(value)?)?;
    std::fs::rename(temporary, dir.join("report.json"))?;
    Ok(())
}

fn describe(error: &(dyn std::error::Error + 'static)) -> String {
    let mut description = error.to_string();
    let mut cause = error.source();
    while let Some(error) = cause {
        description.push_str(": ");
        description.push_str(&error.to_string());
        cause = error.source();
    }
    description
}

async fn interrupted() {
    let termination = tokio::signal::unix::signal(tokio::signal::unix::SignalKind::terminate());
    match termination {
        Ok(mut signal) => {
            tokio::select! { _ = tokio::signal::ctrl_c() => {}, _ = signal.recv() => {} }
        }
        Err(_) => {
            let _ = tokio::signal::ctrl_c().await;
        }
    }
}

#[allow(clippy::too_many_arguments)]
async fn run(
    options: &Options,
    run_id: &str,
    dir: &Path,
    artifacts: &Path,
    slot: &mut Option<vm::Vm>,
    report: &mut Value,
    fixture: &mut domain::Fixture,
) -> Result<()> {
    let mut progress = phases::Progress::new(options, dir, artifacts);
    let hostname = domain::HOST;
    let data = json!({"run_id": run_id, "hostname": hostname, "message": options.message});
    let password = uuid::Uuid::new_v4().simple().to_string();
    progress
        .execute("preflight", report, async |_| {
            eprintln!("Checking KVM and host tools...");
            std::fs::OpenOptions::new()
                .read(true)
                .write(true)
                .open("/dev/kvm")
                .map_err(|error| format!("/dev/kvm is unavailable: {error}; no software-emulation fallback is used"))?;
            let path = std::env::var_os("PATH").ok_or("PATH is not set")?;
            for program in [
                "qemu-system-x86_64",
                "qemu-img",
                "ssh",
                "ssh-keygen",
                "mkfs.vfat",
                "mcopy",
                "openssl",
                "python3",
                "ip",
            ] {
                if !std::env::split_paths(&path).any(|dir| dir.join(program).is_file()) {
                    return Err(format!("missing host tool {program}; see docs/e2e-bootstrap.md").into());
                }
            }
            if options.local_dns.is_some() {
                for program in ["dig", "nsupdate"] {
                    if !std::env::split_paths(&path).any(|dir| dir.join(program).is_file()) {
                        return Err(format!("missing local DNS tool {program}").into());
                    }
                }
            }
            for (program, flag) in [
                ("qemu-system-x86_64", "--version"),
                ("qemu-img", "--version"),
                ("ssh", "-V"),
                ("mcopy", "-V"),
            ] {
                vm::command(program, &[flag], artifacts, "host preflight").await?;
            }
            vm::command(
                "python3",
                &[
                    "-c",
                    "import os; assert hasattr(os, 'pidfd_open'), 'Python 3.9+ with os.pidfd_open is required'",
                ],
                artifacts,
                "host preflight",
            )
            .await?;
            std::net::TcpListener::bind((options.web_bind, options.web_port))
                .map_err(|error| format!("PVE web forwarding address is unavailable: {error}"))?;
            Ok(())
        })
        .await?;
    progress
        .execute("domain", report, async |report| {
            report["domain"] = fixture.prepare(options.web_bind, options.local_dns).await?;
            Ok(())
        })
        .await?;
    progress
        .execute("image", report, async |report| {
            eprintln!("Checking latest Debian 13 qcow2 and verifying SHA-512...");
            report["image"] = serde_json::to_value(image::download(&dir.join("base.qcow2")).await?)?;
            save_report(artifacts, report)?;
            let info = vm::command(
                "qemu-img",
                &["info", "--output=json", &dir.join("base.qcow2").display().to_string()],
                artifacts,
                "image inspection",
            )
            .await?;
            let info: Value = serde_json::from_slice(&info)?;
            if info["format"] != "qcow2"
                || info.get("backing-filename").is_some()
                || info["virtual-size"]
                    .as_u64()
                    .is_none_or(|size| size > u64::from(options.disk) * 1024 * 1024 * 1024)
            {
                return Err("download is not a standalone qcow2 fitting the requested disk".into());
            }
            std::fs::set_permissions(dir.join("base.qcow2"), std::fs::Permissions::from_mode(0o400))?;
            vm::command(
                "qemu-img",
                &[
                    "create",
                    "-f",
                    "qcow2",
                    "-F",
                    "qcow2",
                    "-b",
                    &dir.join("base.qcow2").display().to_string(),
                    &dir.join("disk.qcow2").display().to_string(),
                    &format!("{}G", options.disk),
                ],
                artifacts,
                "overlay creation",
            )
            .await?;
            Ok(())
        })
        .await?;
    let proxy = progress.execute("boot", report, async |report| {
            seed::create(dir, &data, hostname, artifacts).await?;
            let proxy = proxy::start().await?;
            eprintln!(
                "Starting KVM VM: {} vCPU, {} MiB RAM, {} GiB sparse disk...",
                options.cpus, options.memory, options.disk
            );
            *slot = Some(vm::Vm::start(dir, artifacts, options, proxy.port)?);
            let vm = slot.as_mut().ok_or("VM handle is unavailable")?;
            report["ssh_port"] = json!(vm.ssh_port);
            report["api_port"] = json!(vm.api_port);
            save_access(dir, vm, options, hostname, &password, false)?;
            report["access_file"] = json!(dir.join("access.json"));
            report["ssh_config"] = json!(dir.join("ssh_config"));
            show_access(dir, vm, options, hostname, &password, false);
            save_report(artifacts, report)?;
            vm.wait_ssh(None).await?;
            vm.checked_ssh(
                "cloud-init status --wait --long\n",
                Duration::from_secs(240),
                &artifacts.join("cloud-init.log"),
            )
            .await?;
            let observed = vm
                .checked_ssh(
                    "cat /etc/homestack-e2e.json\n",
                    Duration::from_secs(15),
                    &artifacts.join("guest-data.log"),
                )
                .await?;
            if serde_json::from_slice::<Value>(&observed)? != data {
                return Err("guest initialization data differs from the requested data".into());
            }
            report["initialization_verified"] = json!(true);
            vm.checked_ssh(
                "hostname --fqdn\ncat /etc/resolv.conf\ngrep -Eq '^search[[:space:]]+e2e\\.homestack\\.app([[:space:]]|$)' /etc/resolv.conf\n",
                Duration::from_secs(15),
                &artifacts.join("guest-network.log"),
            )
            .await?;
            Ok(proxy)
        }).await?;
    progress
        .execute("kernel", report, async |_| {
            let vm = slot.as_mut().ok_or("VM handle is unavailable")?;
            let boot = vm.wait_ssh(None).await?;
            eprintln!("SSH and initialization data verified. Installing the Proxmox kernel...");
            vm.logged_ssh(
                KERNEL_INSTALL,
                Duration::from_secs(900),
                &artifacts.join("install-kernel.log"),
            )
            .await?;
            eprintln!("Proxmox kernel installed. Rebooting the VM...");
            vm.checked_ssh(
                "systemctl reboot --no-block\n",
                Duration::from_secs(15),
                &artifacts.join("reboot.log"),
            )
            .await?;
            vm.wait_ssh(Some(&boot)).await?;
            Ok(())
        })
        .await?;
    progress
        .execute("proxmox", report, async |report| {
            let vm = slot.as_mut().ok_or("VM handle is unavailable")?;
            eprintln!("Installing Proxmox VE services...");
            vm.logged_ssh(
                PVE_INSTALL,
                Duration::from_secs(900),
                &artifacts.join("install-pve.log"),
            )
            .await?;
            eprintln!("PVE packages installed. Rebooting the VM...");
            let before = vm.wait_ssh(None).await?;
            vm.checked_ssh(
                "systemctl reboot --no-block\n",
                Duration::from_secs(15),
                &artifacts.join("reboot.log"),
            )
            .await?;
            report["boot_id"] = json!(vm.wait_ssh(Some(&before)).await?);
            eprintln!("Waiting for Proxmox services...");
            vm.checked_ssh(
                WAIT_PVE,
                Duration::from_secs(120),
                &artifacts.join("proxmox-readiness.log"),
            )
            .await?;
            // SSH remains key-only; this password is for the temporary PAM web login.
            let result = vm
                .ssh(
                    &format!("printf '%s\\n' 'root:{password}' | chpasswd\n"),
                    Duration::from_secs(15),
                )
                .await?;
            if !result.status.success() {
                return Err("cannot configure the temporary PVE web login".into());
            }
            eprintln!("Installing the cached public TLS certificate...");
            fixture.install(vm).await?;
            fixture.verify_resolution(options.web_bind).await?;
            report["web_url"] = json!(web_url(vm, hostname));
            report["web_trust"] = json!("public CA; normal browser/system roots");
            save_access(dir, vm, options, hostname, &password, true)?;
            show_access(dir, vm, options, hostname, &password, true);
            Ok(())
        })
        .await?;
    let mut internet = None;
    progress
        .execute("network", report, async |report| {
            let vm = slot.as_mut().ok_or("VM handle is unavailable")?;
            eprintln!("Preparing isolated vmbr0 for containers and VMs (10.77.0.1/24)...");
            let (network, gateway) = network::configure(vm, artifacts, dir).await?;
            internet = Some(gateway);
            report["network"] = network;
            eprintln!("Guest network ready: vmbr0, 10.77.0.1/24; DHCP 10.77.0.50-99, DNS 10.77.0.1.");
            eprintln!(
                "Guest Internet ready: all public IPv4 destinations; host, private networks and metadata endpoints are blocked outside PVE."
            );
            Ok(())
        })
        .await?;
    progress
        .execute("homestack", report, async |report| {
            report["service"] = service::prepare(options, dir, artifacts, report).await?;
            Ok(())
        })
        .await?;
    progress.execute("verify", report, async |report| {
            let vm = slot.as_mut().ok_or("VM handle is unavailable")?;
            report["network"] = network::verify(vm, artifacts).await?;
            eprintln!("Verifying the PVE kernel, nested KVM and HTTPS API...");
            let kernel = vm
                .checked_ssh(
                    "uname -r\nif grep -qw vmx /proc/cpuinfo; then modprobe kvm_intel; else modprobe kvm_amd; fi\ntest -c /dev/kvm\nsshd -T | grep -qx 'passwordauthentication no'\nsshd -T | grep -qx 'kbdinteractiveauthentication no'\n",
                    Duration::from_secs(120),
                    &artifacts.join("verification.log"),
                )
                .await?;
            let kernel = String::from_utf8(kernel)?;
            if !kernel.lines().next().is_some_and(|line| line.ends_with("-pve")) {
                return Err("guest did not boot the Proxmox kernel".into());
            }
            report["kernel"] = json!(kernel.lines().next());
            let observed = vm
                .checked_ssh(
                    "cat /etc/homestack-e2e.json\n",
                    Duration::from_secs(15),
                    &artifacts.join("guest-data.log"),
                )
                .await?;
            if serde_json::from_slice::<Value>(&observed)? != data {
                return Err("initialization data changed after installing PVE".into());
            }
            vm.checked_ssh(
                "cat /etc/pve/local/pveproxy-ssl.pem\n",
                Duration::from_secs(15),
                &artifacts.join("pve-certificate.pem"),
            )
            .await?;
            // The token is read through pinned SSH and never written to artifacts.
            let token = vm
                .ssh(
                    "pveum user token add root@pam hs-e2e --privsep 0 --output-format json\n",
                    Duration::from_secs(30),
                )
                .await?;
            if !token.status.success() {
                return Err("cannot create the temporary PVE verification token".into());
            }
            let token: Value = serde_json::from_slice(&token.stdout)?;
            let secret = token["value"].as_str().ok_or("PVE token response has no secret")?;
            let client = reqwest::Client::builder()
                .no_proxy()
                .resolve(hostname, SocketAddr::from((options.web_bind, vm.api_port)))
                .connect_timeout(Duration::from_secs(10))
                .timeout(Duration::from_secs(20))
                .build()?;
            let deadline = tokio::time::Instant::now() + Duration::from_secs(60);
            let mut attempt = 0;
            let version: Value = loop {
                attempt += 1;
                match client
                    .get(format!("https://{hostname}:{}/api2/json/version", vm.api_port))
                    .header("Authorization", format!("PVEAPIToken=root@pam!hs-e2e={secret}"))
                    .send()
                    .await
                {
                    Ok(response) => break response.error_for_status()?.json().await?,
                    Err(error) => {
                        let details = describe(&error);
                        use std::io::Write;
                        let mut log = std::fs::File::options()
                            .create(true)
                            .append(true)
                            .open(artifacts.join("api-readiness.log"))?;
                        writeln!(log, "{details}")?;
                        if tokio::time::Instant::now() >= deadline {
                            return Err(details.into());
                        }
                        if attempt == 1 {
                            eprintln!("Waiting for PVE HTTPS: {details}");
                        }
                        tokio::time::sleep(Duration::from_secs(2)).await;
                    }
                }
            };
            if version["data"]["version"].as_str().is_none() {
                return Err("PVE API response has no version".into());
            }
            report["proxmox"] = version["data"].clone();
            save_report(artifacts, report)?;
            let base = format!("https://{hostname}:{}", vm.api_port);
            // Check the same DNS path used by the browser, without sending
            // credentials. Administrative requests remain pinned to this VM.
            fixture.verify_resolution(options.web_bind).await?;
            let browser = reqwest::Client::builder().no_proxy()
                .redirect(reqwest::redirect::Policy::none())
                .connect_timeout(Duration::from_secs(10)).timeout(Duration::from_secs(20)).build()?;
            let page = browser.get(format!("{base}/")).send().await?.error_for_status()?.text().await?;
            if !page.contains("Proxmox") || !page.contains("pve2") {
                return Err("PVE web panel HTML is missing".into());
            }
            report["web_dns_verified"] = json!(true);
            let login: Value = client.post(format!("{base}/api2/json/access/ticket"))
                .form(&[("username", "root@pam"), ("password", password.as_str())])
                .send().await?.error_for_status()?.json().await?;
            let ticket = login["data"]["ticket"].as_str().filter(|value| !value.is_empty()).ok_or("PVE web login has no ticket")?;
            let nodes: Value = client.get(format!("{base}/api2/json/nodes"))
                .header("Cookie", format!("PVEAuthCookie={ticket}"))
                .send().await?.error_for_status()?.json().await?;
            if !nodes["data"].as_array().is_some_and(|nodes| nodes.len() == 1) {
                return Err("web login cannot read the disposable node inventory".into());
            }
            report["web_login_verified"] = json!(true);
            report["web_url"] = json!(web_url(vm, hostname));
            save_report(artifacts, report)?;
            Ok(())
        }).await?;
    progress
        .execute("shutdown", report, async |_| {
            eprintln!("PVE verified. Requesting graceful poweroff...");
            slot.as_mut().ok_or("VM handle is unavailable")?.poweroff().await
        })
        .await?;
    if let Some(mut internet) = internet {
        internet.stop().await?;
    }
    drop(proxy);
    Ok(())
}

fn web_url(vm: &vm::Vm, hostname: &str) -> String {
    format!("https://{hostname}:{}", vm.api_port)
}

fn save_access(
    dir: &Path,
    vm: &vm::Vm,
    options: &Options,
    hostname: &str,
    password: &str,
    web_ready: bool,
) -> Result<()> {
    let path = dir.join("access.json");
    let tunnel = access::tunnel(options, vm.api_port);
    let access = json!({
        "web_url": web_url(vm, hostname), "web_bind": options.web_bind.to_string(),
        "web_username": "root@pam", "web_password": password, "web_ready": web_ready,
        "ssh_config": dir.join("ssh_config"), "ssh_port": vm.ssh_port,
        "web_trust": "public CA",
        "remote_tunnel_command": tunnel,
        "remote_web_url": tunnel.as_ref().map(|_| format!("https://{hostname}:18006")),
    });
    use std::io::Write;
    let mut file = std::fs::OpenOptions::new()
        .create(true)
        .truncate(true)
        .write(true)
        .mode(0o600)
        .open(path)?;
    file.write_all(&serde_json::to_vec_pretty(&access)?)?;
    Ok(())
}

fn show_access(dir: &Path, vm: &vm::Vm, options: &Options, hostname: &str, password: &str, web_ready: bool) {
    eprintln!(
        "SSH on the runner host: ssh -F {} pve-e2e",
        phases::shell_quote(&dir.join("ssh_config").display().to_string())
    );
    eprintln!("Private access details: {}", dir.join("access.json").display());
    if web_ready {
        if options.web_bind.is_loopback() {
            eprintln!("PVE web panel on this computer: {}", web_url(vm, hostname));
            eprintln!("Open this URL directly when the browser runs on the runner computer.");
        } else {
            eprintln!("PVE web panel: {}", web_url(vm, hostname));
        }
        eprintln!("Login: root");
        eprintln!("Realm: Linux PAM");
        eprintln!("Password: {password}");
        eprintln!("Browser trust: public Let's Encrypt certificate; use the domain in the URL.");
        if let Some(tunnel) = access::tunnel(options, vm.api_port) {
            eprintln!("From another computer, run this tunnel on the browser's computer:");
            eprintln!("  {tunnel}");
            eprintln!("Then open https://{hostname}:18006 in that computer's browser.");
        }
    }
}

const KERNEL_INSTALL: &str = r#"
export DEBIAN_FRONTEND=noninteractive
export LC_ALL=C
test "$(id -u)" = 0
test "$(hostname --ip-address)" = 10.0.2.15
apt-get -o APT::Status-Fd=3 update
apt-get -o APT::Status-Fd=3 -o Dpkg::Use-Pty=0 -y --no-install-recommends install proxmox-default-kernel
"#;

const WAIT_PVE: &str = r#"
for attempt in $(seq 1 50); do
    if systemctl is-active --quiet pve-cluster && systemctl is-active --quiet pvedaemon && systemctl is-active --quiet pveproxy; then
        exit 0
    fi
    sleep 2
done
exit 1
"#;

const PVE_INSTALL: &str = r#"
export DEBIAN_FRONTEND=noninteractive
export LC_ALL=C
case "$(uname -r)" in *-pve) ;; *) echo 'Not running a PVE kernel' >&2; exit 1 ;; esac
echo 'postfix postfix/main_mailer_type select Local only' | debconf-set-selections
apt-get -o APT::Status-Fd=3 -o Dpkg::Use-Pty=0 -y --no-install-recommends install proxmox-ve postfix open-iscsi chrony
rm -f /etc/apt/sources.list.d/pve-enterprise.list /etc/apt/sources.list.d/pve-enterprise.sources /etc/apt/sources.list.d/ceph.sources
apt-get clean
"#;

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn default_resources_are_small_and_pve_suitable() -> Result<()> {
        let options = Options::parse(&["run".into()], "test")?;
        assert_eq!((options.cpus, options.memory, options.disk), (1, 2048, 12));
        assert!(Options::parse(&["run".into(), "--memory-mib".into(), "512".into()], "test").is_err());
        assert!(Options::parse(&["run".into(), "--endpoint".into(), "production".into()], "test").is_err());
        Ok(())
    }

    #[test]
    fn inspection_options_are_explicit_and_reject_unknown_phases() -> Result<()> {
        let args = |args: &[&str]| args.iter().map(|value| (*value).to_owned()).collect::<Vec<_>>();
        let options = Options::parse(
            &args(&[
                "run",
                "--pause-after",
                "boot,verify",
                "--pause-on-failure",
                "--web-bind",
                "192.168.1.20",
                "--web-port",
                "8006",
            ]),
            "test",
        )?;
        assert_eq!(options.pause_after, ["boot", "verify"]);
        assert!(options.pause_on_failure);
        assert_eq!(options.web_bind.to_string(), "192.168.1.20");
        assert_eq!(options.web_port, 8006);
        assert!(Options::parse(&args(&["run", "--pause-after", "cleanup"]), "test").is_err());
        assert!(Options::parse(&args(&["run", "--web-bind", "0.0.0.0"]), "test").is_err());
        Ok(())
    }

    #[test]
    fn fixture_reset_requires_an_explicit_cleanup_command() -> Result<()> {
        let args = |args: &[&str]| args.iter().map(|value| (*value).to_owned()).collect::<Vec<_>>();
        assert!(Options::parse(&args(&["cleanup"]), "test").is_err());
        assert!(Options::parse(&args(&["run", "--all"]), "test").is_err());
        assert!(Options::parse(&args(&["cleanup", "--all", "--web-bind", "127.0.0.1"]), "test").is_err());
        let options = Options::parse(&args(&["cleanup", "--all", "--fixtures", "/tmp/e2e-fixture"]), "test")?;
        assert_eq!(options.fixtures, PathBuf::from("/tmp/e2e-fixture"));
        Ok(())
    }
}
