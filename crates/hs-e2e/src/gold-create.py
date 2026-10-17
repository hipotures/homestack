"""Create only the reference Gold owned by this disposable PVE run."""
import hashlib
import json
import pathlib
import subprocess


def run(*args):
    return subprocess.check_output(args, text=True).strip()


def create(data):
    assert json.load(open('/etc/homestack-e2e.json'))['run_id'] == data['run_id']
    assert pathlib.Path('/sys/class/net/vmbr0/bridge').is_dir()
    if pathlib.Path('/etc/pve/lxc/100.conf').exists():
        raise RuntimeError('VMID 100 belongs to a container; refusing to replace it')
    node = run('hostname', '-s')
    def current_config():
        return json.loads(run('pvesh', 'get', '/nodes/' + node + '/qemu/100/config', '--output-format', 'json'))
    exists = pathlib.Path('/etc/pve/qemu-server/100.conf').exists()
    if exists:
        config = current_config()
        if config.get('description') != data['owner']:
            raise RuntimeError('VMID 100 is not owned by this fixture; refusing to replace it')
        print("Resuming preparation of this run's existing Gold")
    source = pathlib.Path(data['source'])
    with source.open('rb') as image:
        assert hashlib.file_digest(image, 'sha512').hexdigest() == data['sha512'], 'Gold image SHA-512 mismatch'
    storage = json.loads(run('pvesh', 'get', '/storage/local', '--output-format', 'json'))
    content = set(storage['content'].split(',')) | {'images', 'rootdir', 'snippets'}
    run('pvesm', 'set', 'local', '--content', ','.join(sorted(content)))
    pools = json.loads(run('pvesh', 'get', '/pools', '--output-format', 'json'))
    if not any(pool['poolid'] == 'homestack-gold' for pool in pools):
        run('pvesh', 'create', '/pools', '--poolid', 'homestack-gold')
    snippets = pathlib.Path('/var/lib/vz/snippets')
    snippets.mkdir(parents=True, exist_ok=True)
    seed = snippets / ('hs-e2e-gold-' + data['run_id'] + '.yaml')
    seed.write_text('#cloud-config\n' + json.dumps(data['cloud_config']))
    seed.chmod(0o600)
    metadata = snippets / ('hs-e2e-gold-' + data['run_id'] + '-meta.yaml')
    metadata.write_text(json.dumps({'instance-id': data['instance_id'], 'local-hostname': 'gold-e2e'}))
    metadata.chmod(0o600)
    keyfile = pathlib.Path('/run/homestack-e2e-gold.pub')
    keyfile.write_text(data['workspace_key'])
    if not exists:
        run('qm', 'create', '100', '--name', 'gold-e2e', '--memory', '768', '--cores', '1', '--cpu', 'host',
            '--scsihw', 'virtio-scsi-single', '--net0', 'virtio=' + data['mac'] + ',bridge=vmbr0',
            '--agent', 'enabled=1', '--serial0', 'socket', '--vga', 'serial0', '--ostype', 'l26',
            '--tags', 'homestack-gold', '--pool', 'homestack-gold', '--description', data['owner'])
    config = current_config()
    if not config.get('scsi0') and not config.get('unused0'):
        run('qm', 'disk', 'import', '100', str(source), 'local', '--format', 'qcow2')
        config = current_config()
    disk = config.get('scsi0', config.get('unused0', '')).split(',')[0]
    assert disk, 'Gold disk import did not produce a volume'
    cloud = [] if 'cloudinit' in config.get('ide2', '') else ['--ide2', 'local:cloudinit']
    run('qm', 'set', '100', '--scsi0', disk + ',discard=on', *cloud,
        '--boot', 'order=scsi0', '--ipconfig0', 'ip=10.77.0.100/24,gw=10.77.0.1',
        '--nameserver', '10.77.0.1', '--ciuser', 'root', '--sshkeys', str(keyfile),
        '--cicustom', 'user=local:snippets/' + seed.name + ',meta=local:snippets/' + metadata.name)
    run('qm', 'cloudinit', 'update', '100')
    if ',size=8G' not in current_config()['scsi0']:
        run('qm', 'disk', 'resize', '100', 'scsi0', '8G')
    source.unlink()
    keyfile.unlink()
    if run('qm', 'status', '100') == 'status: stopped':
        run('qm', 'start', '100')
    print('Reference Gold 100 created with its agent channel enabled before the first boot')

if __name__ == "__main__":
    create(data)
