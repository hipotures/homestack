//! Minimal disposable PVE bootstrap. This does not run the HomeStack catalogue.

mod image;
mod proxy;
mod seed;
mod vm;

use std::net::{Ipv4Addr, SocketAddr};
use std::os::unix::fs::PermissionsExt;
use std::path::{Path, PathBuf};
use std::process::ExitCode;
use std::time::Duration;

use serde_json::{Value, json};

type Result<T> = std::result::Result<T, Box<dyn std::error::Error + Send + Sync>>;

const USAGE: &str = "homestack-e2e run [--memory-mib N] [--cpus N] [--disk-gib N]\n                  [--timeout-seconds N] [--message TEXT] [--artifacts DIRECTORY]\n\nDefaults: 1 vCPU, 2048 MiB RAM, 12 GiB sparse disk, 1800 seconds.\nDownloads the latest Debian 13 qcow2, installs Proxmox, verifies SSH and\nHTTPS with pinned trust, powers off, and deletes all temporary VM data.\nRequires Linux x86_64, /dev/kvm, qemu-system-x86_64, qemu-img,\nssh, ssh-keygen, mkfs.vfat and mcopy. No host root privileges required.";

struct Options {
    memory: u32,
    cpus: u32,
    disk: u32,
    timeout: u64,
    message: String,
    artifacts: PathBuf,
}

impl Options {
    fn parse(args: &[String], run_id: &str) -> Result<Self> {
        if args.first().map(String::as_str) != Some("run") {
            return Err("expected 'run'; use --help".into());
        }
        let mut options = Self {
            memory: 2048,
            cpus: 1,
            disk: 12,
            timeout: 1800,
            message: "HomeStack disposable PVE bootstrap".into(),
            artifacts: PathBuf::from("target/e2e").join(run_id),
        };
        let mut rest = args[1..].iter();
        while let Some(flag) = rest.next() {
            let value = rest.next().ok_or_else(|| format!("{flag} needs a value"))?;
            match flag.as_str() {
                "--memory-mib" => options.memory = value.parse()?,
                "--cpus" => options.cpus = value.parse()?,
                "--disk-gib" => options.disk = value.parse()?,
                "--timeout-seconds" => options.timeout = value.parse()?,
                "--message" => options.message.clone_from(value),
                "--artifacts" => options.artifacts = PathBuf::from(value),
                _ => return Err(format!("unknown argument {flag}").into()),
            }
        }
        if !(2048..=16384).contains(&options.memory)
            || !(1..=8).contains(&options.cpus)
            || !(12..=64).contains(&options.disk)
            || !(60..=7200).contains(&options.timeout)
        {
            return Err("resource limits: memory 2048–16384 MiB, CPUs 1–8, disk 12–64 GiB, timeout 60–7200 s".into());
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
    let options = Options::parse(&args, &run_id)?;
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
    });
    save_report(&artifacts, &report)?;
    eprintln!("Run {run_id}; artifacts: {}", artifacts.display());
    let outcome = tokio::select! {
        result = tokio::time::timeout(Duration::from_secs(options.timeout), run(&options, &run_id, &private_path, &artifacts, &mut running, &mut report)) => {
            match result { Ok(result) => result, Err(_) => Err("run deadline exceeded".into()) }
        }
        _ = interrupted() => Err("run interrupted; removing the VM".into()),
    };
    eprintln!("Removing temporary VM resources...");
    let graceful = running.as_ref().is_some_and(|vm: &vm::Vm| vm.shutdown_confirmed);
    let termination = match running.as_mut() {
        Some(vm) => vm.terminate(),
        None => Ok(()),
    };
    drop(running);
    // Reap QEMU before unlinking its disks or the seed containing private keys.
    let cleanup = match termination {
        Ok(()) => private.close().map_err(Into::into),
        Err(error) => {
            // Do not unlink a running process's disks after an unconfirmed kill.
            let _ = private.keep();
            Err(error)
        }
    };
    let removed = !private_path.exists();
    report["graceful_shutdown"] = json!(graceful);
    report["cleanup"] = json!({"passed": cleanup.is_ok() && removed, "temporary_directory_removed": removed,
        "error": cleanup.as_ref().err().map(|error| describe(error.as_ref()))});
    report["status"] = json!(if outcome.is_ok() && cleanup.is_ok() && removed {
        "passed"
    } else {
        "failed"
    });
    report["error"] = json!(outcome.as_ref().err().map(|error| describe(error.as_ref())));
    save_report(&artifacts, &report)?;
    outcome?;
    cleanup?;
    if !removed {
        return Err("temporary VM directory remains after cleanup".into());
    }
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

async fn run(
    options: &Options,
    run_id: &str,
    dir: &Path,
    artifacts: &Path,
    slot: &mut Option<vm::Vm>,
    report: &mut Value,
) -> Result<()> {
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
    ] {
        if !std::env::split_paths(&path).any(|dir| dir.join(program).is_file()) {
            return Err(format!("missing host tool {program}; see docs/e2e-bootstrap.md").into());
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
    eprintln!("Downloading latest Debian 13 qcow2 and verifying SHA-512...");
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
    let hostname = format!("hse2e-{}.e2e.test", &run_id[..8]);
    let data = json!({"run_id": run_id, "hostname": hostname, "message": options.message});
    seed::create(dir, &data, &hostname, artifacts).await?;
    let proxy = proxy::start().await?;
    eprintln!(
        "Starting KVM VM: {} vCPU, {} MiB RAM, {} GiB sparse disk...",
        options.cpus, options.memory, options.disk
    );
    *slot = Some(vm::Vm::start(dir, artifacts, options.memory, options.cpus, proxy.port)?);
    let vm = slot.as_mut().ok_or("VM handle is unavailable")?;
    report["ssh_port"] = json!(vm.ssh_port);
    report["api_port"] = json!(vm.api_port);
    save_report(artifacts, report)?;
    let boot = vm.wait_ssh(None).await?;
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
        "hostname --fqdn\ncat /etc/resolv.conf\ngrep -Eq '^search[[:space:]]+e2e\\.test([[:space:]]|$)' /etc/resolv.conf\n",
        Duration::from_secs(15),
        &artifacts.join("guest-network.log"),
    )
    .await?;
    eprintln!("SSH and initialization data verified. Installing the Proxmox kernel...");
    vm.logged_ssh(
        KERNEL_INSTALL,
        Duration::from_secs(900),
        &artifacts.join("install-kernel.log"),
    )
    .await?;
    vm.checked_ssh(
        "systemctl reboot --no-block\n",
        Duration::from_secs(15),
        &artifacts.join("reboot.log"),
    )
    .await?;
    vm.wait_ssh(Some(&boot)).await?;
    eprintln!("Installing Proxmox VE services...");
    vm.logged_ssh(
        PVE_INSTALL,
        Duration::from_secs(900),
        &artifacts.join("install-pve.log"),
    )
    .await?;
    let before = vm.wait_ssh(None).await?;
    vm.checked_ssh(
        "systemctl reboot --no-block\n",
        Duration::from_secs(15),
        &artifacts.join("reboot.log"),
    )
    .await?;
    report["boot_id"] = json!(vm.wait_ssh(Some(&before)).await?);
    eprintln!("Verifying the PVE kernel, nested KVM and HTTPS API...");
    let kernel = vm
        .checked_ssh(
            "uname -r\nif grep -qw vmx /proc/cpuinfo; then modprobe kvm_intel; else modprobe kvm_amd; fi\ntest -c /dev/kvm\nfor attempt in $(seq 1 50); do if systemctl is-active --quiet pve-cluster && systemctl is-active --quiet pvedaemon && systemctl is-active --quiet pveproxy; then exit 0; fi; sleep 2; done\nexit 1\n",
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
    let ca = vm
        .checked_ssh(
            "cat /etc/pve/pve-root-ca.pem\n",
            Duration::from_secs(15),
            &artifacts.join("pve-ca.pem"),
        )
        .await?;
    vm.checked_ssh(
        "cat /etc/pve/local/pve-ssl.pem\n",
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
        .add_root_certificate(reqwest::Certificate::from_pem(&ca)?)
        .resolve(&hostname, SocketAddr::from((Ipv4Addr::LOCALHOST, vm.api_port)))
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
    eprintln!(
        "PVE {} verified. Requesting graceful poweroff...",
        version["data"]["version"]
    );
    vm.poweroff().await?;
    drop(proxy);
    Ok(())
}

const KERNEL_INSTALL: &str = r#"
export DEBIAN_FRONTEND=noninteractive
test "$(id -u)" = 0
test "$(hostname --ip-address)" = 10.0.2.15
apt-get update
apt-get -y --no-install-recommends install proxmox-default-kernel
"#;

const PVE_INSTALL: &str = r#"
export DEBIAN_FRONTEND=noninteractive
case "$(uname -r)" in *-pve) ;; *) echo 'Not running a PVE kernel' >&2; exit 1 ;; esac
echo 'postfix postfix/main_mailer_type select Local only' | debconf-set-selections
apt-get -y --no-install-recommends install proxmox-ve postfix open-iscsi chrony
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
}
