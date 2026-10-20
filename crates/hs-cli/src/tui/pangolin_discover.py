# Read-only remote probe. Only allowlisted inventory leaves this process.
# Python 3 is required remotely; no packages are installed.
import json
import os
import re
import subprocess

facts = {}
issues = []


def run(args):
    try:
        p = subprocess.run(args, stdout=subprocess.PIPE, stderr=subprocess.PIPE,
                           timeout=6, check=False)
        # Raw output, especially inspect/configuration data, is never printed.
        code = 13 if p.returncode and b'permission denied' in p.stderr.lower() else p.returncode
        return code, p.stdout[:1048576].decode('utf-8', 'replace')
    except FileNotFoundError:
        return 127, ''
    except (OSError, subprocess.TimeoutExpired):
        return 126, ''


def safe(value):
    value = str(value)
    return value if len(value) <= 512 and not any(ord(c) < 32 for c in value) else 'Unavailable'


def scalar_config(path):
    # Parse only unambiguous literal scalar values. Never interpret YAML tags,
    # anchors, expressions, substitutions or secret values. Unsupported YAML
    # remains unknown instead of importing an optional YAML implementation.
    try:
        with open(path, encoding='utf-8') as stream:
            text = stream.read(262145)
        if len(text) > 262144:
            return False
    except (OSError, UnicodeError):
        return False
    # Refuse YAML merge/anchor/tag semantics instead of guessing their effect.
    if any(re.search(r'(^|\s)(?:<<:|[&*!][A-Za-z_])', line) for line in text.splitlines()):
        return False
    sections = []
    seen = set()
    roots = set()
    for line in text.splitlines():
        match = re.fullmatch(r'( *)([a-z_]+):\s*(.*?)\s*', line)
        if not match:
            continue
        indent, key, value = len(match[1]), match[2], match[3].split(' #')[0].strip()
        while sections and sections[-1][0] >= indent:
            sections.pop()
        parents = [item[1] for item in sections]
        if value.startswith(('|', '>')):
            return False
        if indent == 0 and key in ('flags', 'server'):
            if key in roots or value:
                return False
            roots.add(key)
        if not value:
            sections.append((indent, key))
        elif parents == ['flags'] and sections[0][0] == 0 and key == 'enable_integration_api':
            if key in seen:
                facts.pop('api_enabled', None)
                return False
            seen.add(key)
            if value in ('true', 'false'):
                facts['api_enabled'] = value
        elif parents == ['server'] and sections[0][0] == 0 and key in ('integration_port', 'internal_port', 'external_port'):
            if key in seen:
                return False
            seen.add(key)
            if value.isdecimal() and 0 < int(value) < 65536:
                facts['api_port' if key == 'integration_port' else key] = value
    facts['config'] = safe(path)
    return True


code, listing = run(['docker', 'ps', '-a', '--no-trunc', '--format', '{{json .}}'])
containers = []
if code == 0:
    for line in listing.splitlines()[:256]:
        try:
            item = json.loads(line)
            image = item.get('Image', '')
            # Official image identities; Docker itself is not installation evidence.
            if re.fullmatch(r'(?:(?:docker.io|ghcr.io)/)?fosrl/pangolin(?::[^\s]+|@sha256:[a-f0-9]+)?', image):
                containers.append(item)
        except (ValueError, TypeError):
            issues.append('docker_listing_invalid')
elif code == 13:
    issues.append('docker_permission')
elif code != 127:
    issues.append('docker_unavailable')

native = []
unknown_units = []
rc, units = run(['systemctl', 'list-unit-files', '--type=service', '--no-legend', '--no-pager'])
if rc == 0:
    for line in units.splitlines()[:1024]:
        unit = line.split()[0] if line.split() else ''
        if re.fullmatch(r'[a-zA-Z0-9_.@-]*pangolin[a-zA-Z0-9_.@-]*\.service', unit):
            rc, text = run(['systemctl', 'show', unit, '--no-pager',
                            '--property=ExecStart,ActiveState,SubState,MainPID,FragmentPath,WorkingDirectory'])
            data = dict(line.split('=', 1) for line in text.splitlines() if '=' in line)
            # A matching unit name alone is insufficient evidence.
            executable = re.search(r'path=([^ ;]+)', data.get('ExecStart', ''))
            if rc == 0 and executable and os.path.basename(executable[1]) == 'pangolin':
                native.append((unit, data))
            else:
                unknown_units.append(unit)

if len(containers) + len(native) > 1 or (unknown_units and (containers or native)):
    outcome = 'Unsupported installation'
    issues.append('ambiguous')
elif containers:
    item = containers[0]
    identity = item.get('ID', '')
    if not re.fullmatch(r'[a-f0-9]{12,64}', identity):
        raise ValueError('Invalid container identity')
    # Formatting prevents environments and unrelated labels leaving Docker.
    template = '{{json .State.Running}}\n{{json .Config.Image}}\n{{json .Mounts}}\n{{json .NetworkSettings.Ports}}\n{{json (index .Config.Labels "com.docker.compose.project")}}\n{{json (index .Config.Labels "com.docker.compose.project.config_files")}}\n{{json .State.Pid}}'
    rc, text = run(['docker', 'inspect', '--format', template, identity])
    if rc:
        outcome = 'Permission denied' if rc == 13 else 'Failed'
        issues.append('docker_permission' if rc == 13 else 'docker_unavailable')
    else:
        running, image, mounts, ports, project, compose, pid = map(json.loads, text.splitlines())
        facts.update(installation='Docker', identity=identity, state='Running' if running else 'Stopped',
                     image=safe(image), version_evidence='Image reference only; running software version unverified')
        if project:
            if not compose or not all(os.path.isabs(path) for path in str(compose).split(',')):
                issues.append('compose_unknown')
            facts['installation'] = 'Docker Compose'
            facts['compose_project'] = safe(project)
            if compose:
                facts['compose'] = safe(compose)
        mount_lines = []
        candidates = []
        for mount in mounts[:32]:
            source, destination = mount.get('Source', ''), mount.get('Destination', '')
            if not os.path.isabs(source) or not os.path.isabs(destination):
                continue
            mount_lines.append(safe(source + ' -> ' + destination))
            # Bounded filenames within actual mount metadata, never a recursive search.
            if os.path.basename(destination) in ('config.yml', 'config.yaml'):
                candidates.append(source)
            elif os.path.basename(destination) == 'config':
                candidates.extend(os.path.join(source, name) for name in ('config.yml', 'config.yaml'))
        facts['mounts'] = safe('; '.join(mount_lines))
        readable = [path for path in candidates if os.path.isfile(path)]
        if len(readable) == 1:
            facts['config'] = safe(readable[0])
            if not scalar_config(readable[0]):
                for key in ('api_enabled', 'api_port', 'internal_port', 'external_port'):
                    facts.pop(key, None)
                facts['config_status'] = 'Unreadable or unsupported YAML'
                issues.append('config_unreadable')
            else:
                facts['config_status'] = 'Literal settings parsed'
        else:
            issues.append('config_unknown')
        mappings = []
        for port, bindings in (ports or {}).items():
            if re.fullmatch(r'[0-9]+/(?:tcp|udp)', port):
                for binding in bindings or []:
                    mappings.append(safe(port + ' -> ' + binding.get('HostIp', '') + ':' + binding.get('HostPort', '')))
        facts['mappings'] = safe('; '.join(mappings)) or 'None'
        api_port = facts.get('api_port')
        if api_port:
            bindings = (ports or {}).get(api_port + '/tcp') or []
            facts['api_publication'] = ('Not published' if not bindings else
                'Loopback only' if all(b.get('HostIp') in ('127.0.0.1', '::1') for b in bindings) else 'Wider interface')
        # Read kernel socket tables using the inspected container PID; no exec or nsenter.
        listeners = set()
        if running and isinstance(pid, int) and pid > 0:
            try:
                for name in ('tcp', 'tcp6'):
                    with open('/proc/' + str(pid) + '/net/' + name, encoding='ascii') as stream:
                        for line in stream.read(65536).splitlines()[1:]:
                            columns = line.split()
                            if len(columns) > 3 and columns[3] == '0A':
                                listeners.add(str(int(columns[1].split(':')[1], 16)))
                facts['listening_ports'] = ', '.join(sorted(listeners, key=int))
                if api_port:
                    facts['api_listening'] = 'Yes' if api_port in listeners else 'No'
            except (OSError, ValueError):
                issues.append('sockets_unreadable')
        if not running:
            issues.append('stopped')
        if 'api_enabled' not in facts or (facts.get('api_enabled') == 'true' and 'api_port' not in facts):
            issues.append('api_unknown')
        outcome = 'Discovered' if running and not issues else 'Partially discovered'
elif native:
    unit, data = native[0]
    facts.update(installation='Native/systemd', identity=safe(unit),
                 state='Running' if data.get('ActiveState') == 'active' and data.get('SubState') == 'running' and data.get('MainPID', '').isdecimal() and int(data['MainPID']) > 0 else 'Stopped',
                 working_directory=safe(data.get('WorkingDirectory', '')),
                 service_file=safe(data.get('FragmentPath', '')))
    issues.append('native_incomplete')
    outcome = 'Partially discovered'
elif unknown_units:
    outcome = 'Unsupported installation'
    issues.append('unsupported')
else:
    outcome = 'Permission denied' if code == 13 else 'Failed' if code not in (0, 127) else 'Not found'
    issues.append('not_found' if outcome == 'Not found' else 'docker_permission' if code == 13 else 'docker_unavailable')

print(json.dumps(dict(outcome=outcome, facts=facts, issues=issues), separators=(',', ':')))
