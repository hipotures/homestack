//! One disposable topology: independent PVE disks on a private rootless L2 hub.

use std::path::{Path, PathBuf};
use std::process::{Child, Stdio};
use std::time::Duration;

use serde_json::{Value, json};

use crate::{Result, phases::shell_quote, vm::Vm};

pub fn directory(root: &Path, index: u8) -> PathBuf {
    if index == 1 {
        root.to_owned()
    } else {
        root.join("nodes").join(format!("pve{index}"))
    }
}

pub struct Backbone {
    child: Child,
}

impl Backbone {
    pub async fn start(dir: &Path, artifacts: &Path, nodes: u8) -> Result<Option<Self>> {
        if nodes == 1 {
            return Ok(None);
        }
        let log = std::fs::File::create(artifacts.join("backbone.log"))?;
        let mut command = std::process::Command::new("qemu-system-x86_64");
        command.args([
            "-machine",
            "none",
            "-nodefaults",
            "-display",
            "none",
            "-monitor",
            "none",
            "-serial",
            "none",
        ]);
        for index in 1..=nodes {
            command.args([
                "-netdev",
                &format!(
                    "stream,id=wire{index},addr.type=unix,addr.path={},server=on",
                    dir.join(format!("backbone-{index}.sock")).display()
                ),
                "-netdev",
                &format!("hubport,id=port{index},hubid=0,netdev=wire{index}"),
            ]);
        }
        let mut backbone = Self {
            child: command.stdin(Stdio::null()).stdout(Stdio::null()).stderr(log).spawn()?,
        };
        for _ in 0..100 {
            if backbone.child.try_wait()?.is_some() {
                return Err("private cluster backbone stopped; see backbone.log".into());
            }
            if (1..=nodes).all(|index| dir.join(format!("backbone-{index}.sock")).exists()) {
                return Ok(Some(backbone));
            }
            tokio::time::sleep(Duration::from_millis(50)).await;
        }
        Err("private cluster backbone did not become ready".into())
    }
}

impl Drop for Backbone {
    fn drop(&mut self) {
        if self.child.try_wait().is_ok_and(|state| state.is_none()) {
            let _ = self.child.kill();
        }
        let _ = self.child.wait();
    }
}

pub fn write_ssh_inventory(dir: &Path, report: &Value) -> Result<()> {
    use std::io::Write;
    let primary = std::fs::read_to_string(dir.join("ssh_config"))?.replace(
        "Host pve-e2e\n",
        "Host pve-e2e pve-e2e-pve1 pve1 pve1.e2e.homestack.app pve.e2e.homestack.app 10.77.0.1\n",
    );
    std::fs::write(dir.join("ssh_config"), primary)?;
    let mut config = std::fs::OpenOptions::new().append(true).open(dir.join("ssh_config"))?;
    for node in report["nodes"]
        .as_array()
        .ok_or("missing node inventory")?
        .iter()
        .skip(1)
    {
        let name = node["name"].as_str().ok_or("missing node name")?;
        let address = node["address"].as_str().ok_or("missing node address")?;
        let source = Path::new(node["directory"].as_str().ok_or("missing node directory")?).join("ssh_config");
        let entry = std::fs::read_to_string(source)?.replace(
            "Host pve-e2e\n",
            &format!("Host pve-e2e-{name} {name} {name}.e2e.homestack.app {address}\n"),
        );
        writeln!(config, "\n{entry}")?;
    }
    Ok(())
}

async fn install(vm: &mut Vm, kernel: bool) -> Result<()> {
    let before = vm.wait_ssh(None).await?;
    eprintln!(
        "pve{}: installing {}",
        vm.index,
        if kernel { "Proxmox kernel" } else { "Proxmox services" }
    );
    vm.logged_ssh(
        if kernel {
            crate::KERNEL_INSTALL
        } else {
            crate::PVE_INSTALL
        },
        Duration::from_secs(1500),
        &vm.artifacts.join(if kernel {
            "install-kernel.log"
        } else {
            "install-pve.log"
        }),
    )
    .await?;
    vm.checked_ssh(
        "systemctl reboot --no-block\n",
        Duration::from_secs(15),
        &vm.artifacts.join("reboot.log"),
    )
    .await?;
    vm.wait_ssh(Some(&before)).await?;
    if !kernel {
        vm.checked_ssh(
            crate::WAIT_PVE,
            Duration::from_secs(120),
            &vm.artifacts.join("proxmox-readiness.log"),
        )
        .await?;
    }
    Ok(())
}

pub async fn install_all(vms: &mut [Vm], kernel: bool) -> Result<()> {
    match vms {
        [first] => install(first, kernel).await,
        [first, second, third] => {
            tokio::try_join!(install(first, kernel), install(second, kernel), install(third, kernel))?;
            Ok(())
        }
        _ => Err("E2E requires one or three running nodes".into()),
    }
}

async fn wait_quorum(vm: &Vm) -> Result<()> {
    vm.checked_ssh(
        r#"set -eu
python3 - <<'QUORUM'
import json,subprocess,time
for attempt in range(60):
    result=subprocess.run(['pvesh','get','/cluster/status','--output-format','json'],capture_output=True,text=True)
    if result.returncode == 0:
        status=json.loads(result.stdout)
        if any(entry.get('type')=='cluster' and entry.get('quorate') == 1 for entry in status):
            print(json.dumps(status))
            break
    time.sleep(1)
else:
    raise RuntimeError('Proxmox API did not report cluster quorum before its deadline')
QUORUM
"#,
        Duration::from_secs(90),
        &vm.artifacts.join("cluster.log"),
    )
    .await?;
    Ok(())
}

pub async fn configure(vms: &mut [Vm], run_id: &str, password: &str) -> Result<Value> {
    if vms.len() == 1 {
        return Ok(json!({"nodes":1,"mode":"standalone"}));
    }
    let hosts = "127.0.0.1 localhost\n10.77.0.1 pve1.e2e.homestack.app pve1 pve.e2e.homestack.app\n10.77.0.2 pve2.e2e.homestack.app pve2\n10.77.0.3 pve3.e2e.homestack.app pve3\n";
    for vm in vms.iter() {
        let script = format!(
            "set -eu\npython3 -c 'import json; assert json.load(open(\"/etc/homestack-e2e.json\"))[\"run_id\"] == \"{run_id}\"'\nprintf %s {} > /etc/hosts\n",
            shell_quote(hosts)
        );
        vm.checked_ssh(&script, Duration::from_secs(30), &vm.artifacts.join("cluster.log"))
            .await?;
        if vm.index != 1 {
            crate::network::configure_peer(vm).await?;
        }
    }
    let primary = vms.first().ok_or("missing primary node")?;
    // Cluster joins replace node-local authorized_keys with the shared file.
    // Keep every pinned controller key authorized throughout that replacement.
    let mut keys = String::new();
    for vm in vms.iter() {
        keys.push_str(&std::fs::read_to_string(vm.dir.join("identity.pub"))?);
    }
    primary
        .checked_ssh(
            &format!(
                "set -eu\nprintf %s {} >> /root/.ssh/authorized_keys\npvecm create hse2e-{} --link0 10.77.0.1\n",
                shell_quote(&keys),
                &run_id[..8]
            ),
            Duration::from_secs(120),
            &primary.artifacts.join("cluster.log"),
        )
        .await?;
    wait_quorum(primary).await?;
    let fingerprint = primary
        .checked_ssh(
            "openssl x509 -in /etc/pve/local/pveproxy-ssl.pem -noout -fingerprint -sha256\n",
            Duration::from_secs(15),
            &primary.artifacts.join("cluster.log"),
        )
        .await?;
    let fingerprint = String::from_utf8(fingerprint)?
        .trim()
        .split_once('=')
        .ok_or("missing cluster TLS fingerprint")?
        .1
        .to_owned();
    for vm in vms.iter().skip(1) {
        let arguments = json!({"hostname":"10.77.0.1","password":password,"fingerprint":fingerprint,"link0":format!("10.77.0.{}",vm.index)});
        // A successful CLI invocation does not imply a successful join worker.
        let script = format!(
            "set -eu\npython3 - <<'JOIN'\n{}\njoin(json.loads({}))\nJOIN\n",
            include_str!("cluster-join.py"),
            serde_json::to_string(&arguments.to_string())?
        );
        vm.checked_ssh(&script, Duration::from_secs(150), &vm.artifacts.join("cluster.log"))
            .await?;
        wait_quorum(primary).await?;
        wait_quorum(vm).await?;
    }
    let nodes = verify(vms, run_id).await?;
    Ok(json!({"nodes":vms.len(),"mode":"cluster","members":nodes}))
}

pub async fn verify(vms: &[Vm], run_id: &str) -> Result<Value> {
    let mut observed = Value::Null;
    for vm in vms {
        let script = format!(
            "set -eu\npython3 - <<'VERIFY'\nimport json,subprocess,time\nassert json.load(open('/etc/homestack-e2e.json'))['run_id']=={run_id:?}\nfor attempt in range(60):\n nodes=json.loads(subprocess.check_output(['pvesh','get','/nodes','--output-format','json']))\n if len(nodes)=={} and all(n.get('status')=='online' for n in nodes): break\n time.sleep(1)\nelse: raise RuntimeError('Not every E2E node is online')\nassert sorted(n['node'] for n in nodes)==[f'pve{{i}}' for i in range(1,{}+1)]\nprint(json.dumps(nodes))\nVERIFY\n{}",
            vms.len(),
            vms.len(),
            if vms.len() > 1 {
                "pvecm status | grep -Eq '^Quorate:[[:space:]]+Yes'\n"
            } else {
                ""
            }
        );
        let output = vm
            .checked_ssh(
                &script,
                Duration::from_secs(90),
                &vm.artifacts.join("cluster-verification.log"),
            )
            .await?;
        observed = serde_json::from_slice(&output)?;
    }
    Ok(observed)
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn node_directories_share_a_lifecycle_but_not_disks() {
        assert_eq!(directory(Path::new("/lab"), 1), Path::new("/lab"));
        assert_eq!(directory(Path::new("/lab"), 3), Path::new("/lab/nodes/pve3"));
    }

    #[test]
    fn join_requires_a_successful_worker_even_when_the_cli_succeeds() -> Result<()> {
        let script = format!(
            "{}\n{}",
            include_str!("cluster-join.py"),
            r#"
from types import SimpleNamespace
from unittest.mock import patch
task = 'UPID:pve2:00000001:00000002:00000003:clusterjoin::root@pam:'
for output, worker, expected in [
    (json.dumps(task), 'OK', True),
    (json.dumps(task), "500 Can't connect to 10.77.0.1:8006", False),
    ('', 'OK', False),
]:
    with patch('subprocess.run', return_value=SimpleNamespace(stdout=output, stderr='')), \
         patch('subprocess.check_output', side_effect=['pve2', json.dumps({'status':'stopped','exitstatus':worker})]):
        try:
            join({})
            passed = True
        except RuntimeError:
            passed = False
        assert passed == expected, (output, worker)
"#
        );
        let output = std::process::Command::new("python3").args(["-c", &script]).output()?;
        assert!(output.status.success(), "{}", String::from_utf8_lossy(&output.stderr));
        Ok(())
    }
}
