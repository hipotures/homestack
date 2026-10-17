"""Start only this run's Gold and obtain its host key through the PVE agent."""
import json
import pathlib
import subprocess
import time

def prepare(data):
    assert json.load(open('/etc/homestack-e2e.json'))['run_id'] == data['run_id']
    if pathlib.Path('/etc/pve/lxc/100.conf').exists():
        raise RuntimeError('VMID 100 belongs to a container; refusing Gold inspection')
    node = subprocess.check_output(['hostname', '-s'], text=True).strip()
    cfg = json.loads(subprocess.check_output(['pvesh', 'get', '/nodes/' + node + '/qemu/100/config', '--output-format', 'json']))
    assert cfg.get('description') == 'homestack-e2e:' + data['run_id'] + ':gold', 'Gold ownership changed'
    status = subprocess.check_output(['qm', 'status', '100'], text=True).strip()
    assert status in ['status: running', 'status: stopped'], 'Gold has an unexpected state'
    started = status == 'status: stopped'
    if started:
        # Use modern cloud-config rather than PVE's deprecated `user: root` seed.
        seed = pathlib.Path('/var/lib/vz/snippets/hs-e2e-gold-' + data['run_id'] + '-inspect.yaml')
        seed.touch(mode=0o600, exist_ok=True)
        seed.chmod(0o600)
        seed.write_text('#cloud-config\n' + json.dumps(data['cloud_config']))
        subprocess.run(['qm', 'set', '100', '--cicustom', 'user=local:snippets/' + seed.name], check=True, stdout=subprocess.DEVNULL)
        subprocess.run(['qm', 'start', '100'], check=True, stdout=subprocess.DEVNULL)
    deadline = time.monotonic() + 45
    while True:
        try:
            result = json.loads(subprocess.check_output(
                ['qm', 'guest', 'exec', '100', '--', '/bin/sh', '-c',
                 'systemctl is-active --quiet ssh && cat /etc/ssh/ssh_host_ed25519_key.pub'],
                stderr=subprocess.DEVNULL, text=True, timeout=12))
            key = result.get('out-data', '').strip()
            if result.get('exitcode') == 0 and key.startswith('ssh-ed25519 ') and len(key.splitlines()) == 1:
                print(json.dumps({'started': started, 'key': key}))
                break
        except (subprocess.SubprocessError, ValueError):
            pass
        if time.monotonic() > deadline:
            raise RuntimeError('Gold agent or host key did not become ready; Gold remains available for inspection')
        time.sleep(1)

if __name__ == "__main__":
    prepare(data)
