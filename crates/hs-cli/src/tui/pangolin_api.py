# Remote Phase 4 program. Full configuration and Compose output stay in memory.
import contextlib
import hashlib
import http.client
import io
import json
import os
import re
import stat
import subprocess
import tempfile
import time
import uuid


class Refuse(Exception):
    pass


result = dict(status='Failed', code='unsupported', changed='No changes applied', backup='',
              endpoint='', access='Not inspected', port=0, enabled=False, running=False, plan=None, inventory=None)


def fail(code):
    raise Refuse(code)


def run_cmd(args, timeout=12):
    try:
        environment = {k: v for k, v in os.environ.items() if not k.startswith('COMPOSE_')}
        p = subprocess.run(args, stdout=subprocess.PIPE, stderr=subprocess.PIPE,
                           timeout=timeout, check=False, env=environment)
    except subprocess.TimeoutExpired:
        fail('timeout')
    except OSError:
        fail('runtime')
    if p.returncode:
        fail('permission' if b'permission denied' in p.stderr.lower() else 'runtime')
    if len(p.stdout) > 2097152:
        fail('unsupported')
    return p.stdout.decode('utf-8', 'strict')


def digest(data):
    return hashlib.sha256(data).hexdigest()


def path_ok(path):
    if not os.path.isabs(path) or os.path.normpath(path) != path:
        fail('path')
    part = '/'
    for name in path.split('/')[1:]:
        part = os.path.join(part, name)
        if stat.S_ISLNK(os.lstat(part).st_mode):
            fail('path')


def parent_fd(path):
    # Resolve each directory through O_NOFOLLOW, not a check-then-follow path.
    if not os.path.isabs(path) or os.path.normpath(path) != path:
        fail('path')
    fd = os.open('/', os.O_RDONLY | os.O_DIRECTORY)
    try:
        for name in path.split('/')[1:-1]:
            child = os.open(name, os.O_RDONLY | os.O_DIRECTORY | os.O_NOFOLLOW, dir_fd=fd)
            os.close(fd)
            fd = child
        return fd
    except BaseException:
        os.close(fd)
        raise


def read_file(path):
    parent = parent_fd(path)
    try:
        fd = os.open(os.path.basename(path), os.O_RDONLY | os.O_NOFOLLOW, dir_fd=parent)
        with os.fdopen(fd, 'rb') as stream:
            meta = os.fstat(stream.fileno())
            if not stat.S_ISREG(meta.st_mode) or meta.st_size > 262144:
                fail('unsupported')
            data = stream.read(262145)
        return data, meta
    finally:
        os.close(parent)


def load_yaml(text):
    # Mutation requires an existing safe parser; no packages are installed.
    try:
        import yaml
    except ImportError:
        fail('yaml_parser')
    # Reject semantics that make minimal textual editing ambiguous.
    try:
        for token in yaml.scan(text):
            if isinstance(token, (yaml.tokens.AnchorToken, yaml.tokens.AliasToken,
                                  yaml.tokens.TagToken, yaml.tokens.DirectiveToken)):
                fail('yaml')
    except Refuse:
        raise
    except Exception:
        fail('yaml')
    class Unique(yaml.SafeLoader):
        pass
    def mapping(loader, node):
        pairs = loader.construct_pairs(node)
        output = {}
        for key, value in pairs:
            if not isinstance(key, (str, int)) or key in output:
                fail('yaml')
            output[key] = value
        return output
    Unique.add_constructor(yaml.resolver.BaseResolver.DEFAULT_MAPPING_TAG, mapping)
    try:
        data = yaml.load(text, Loader=Unique)
    except Refuse:
        raise
    except Exception:
        fail('yaml')
    if not isinstance(data, dict):
        fail('yaml')
    return data


def set_scalar(text, section, key, value):
    lines = text.splitlines(keepends=True)
    roots = [i for i, line in enumerate(lines) if re.fullmatch(section + r':\s*(?:#.*)?\r?\n?', line)]
    if len(roots) != 1:
        fail('yaml')
    start = roots[0]
    end = next((i for i in range(start + 1, len(lines)) if re.match(r'[^\s#]', lines[i])), len(lines))
    matches = [i for i in range(start + 1, end) if re.match(r'  ' + key + r':', lines[i])]
    replacement = '  ' + key + ': ' + value + '\n'
    if len(matches) > 1:
        fail('yaml')
    if matches:
        i = matches[0]
        if not re.fullmatch(r'  ' + key + r':\s*(true|false|[0-9]+)\s*(#.*)?\r?\n?', lines[i]):
            fail('yaml')
        comment = re.search(r'\s+#.*', lines[i].rstrip('\r\n'))
        lines[i] = replacement.rstrip('\n') + (comment[0] if comment else '') + '\n'
    else:
        lines.insert(start + 1, replacement)
    return ''.join(lines)


def health(port):
    # http.client never honors HTTP(S)_PROXY and does not follow redirects.
    try:
        connection = http.client.HTTPConnection('127.0.0.1', port, timeout=2)
        try:
            connection.request('GET', '/v1/', headers={'Accept': 'application/json'})
            response = connection.getresponse()
            body = response.read(1025)
            return response.status == 200 and len(body) <= 1024 and json.loads(body) == {'message': 'Healthy'}
        finally:
            connection.close()
    except (OSError, ValueError, http.client.HTTPException):
        return False


def inspect(expected):
    # Check the previously verified configuration without following symlinks;
    # distinguish read permission failure before the bounded discovery probe.
    read_file(expected['config'])
    namespace = {}
    with contextlib.redirect_stdout(io.StringIO()):
        exec(PROBE, namespace)
    f = namespace['facts']
    if any(f.get(k) != expected.get(k) for k in ('identity', 'installation', 'config')):
        fail('stale')
    if namespace['outcome'] != 'Discovered':
        fail('permission' if namespace['outcome'] == 'Permission denied' else 'discovery')
    identity = f['identity']
    docker_host = os.environ.get('DOCKER_HOST', '')
    if docker_host and not docker_host.startswith('unix://'):
        fail('unsupported')
    context_host = run_cmd(['docker', 'context', 'inspect', '--format', '{{(index .Endpoints "docker").Host}}']).strip()
    if not context_host.startswith('unix://'):
        fail('unsupported')
    if not re.fullmatch(r'[a-f0-9]{12,64}', identity):
        fail('unsupported')
    # Never return environments, labels or full Docker inspect data.
    template = '{{json .NetworkSettings.Ports}}\n{{json .HostConfig.NetworkMode}}\n{{json (index .Config.Labels "com.docker.compose.service")}}\n{{json (index .Config.Labels "com.docker.compose.project.working_dir")}}\n{{json .State.Running}}\n{{json .Mounts}}\n{{json .Config.Image}}\n{{json (index .Config.Labels "com.docker.compose.config-hash")}}\n{{json .Image}}'
    ports, network, service, workdir, running, mounts, image, runtime_hash, image_id = map(json.loads, run_cmd(['docker', 'inspect', '--format', template, identity]).splitlines())
    if network in ('host', 'none') or str(network).startswith('container:'):
        fail('unsupported')
    port = int(f.get('api_port') or 3003)
    if not 0 < port < 65536:
        fail('unsupported')
    bindings = (ports or {}).get(str(port) + '/tcp') or []
    external = any(b.get('HostIp') not in ('127.0.0.1', '::1') for b in bindings)
    ipv4 = [b for b in bindings if b.get('HostIp') == '127.0.0.1']
    host_port = int(ipv4[0]['HostPort']) if len(ipv4) == 1 else port
    result.update(access='Externally published' if external else 'Loopback only' if ipv4 else 'Not published on IPv4 loopback', port=port, enabled=f.get('api_enabled') == 'true', running=bool(running),
                  endpoint='http://127.0.0.1:' + str(host_port) + '/v1/')
    if result['enabled'] and running and ipv4 and not external and f.get('api_listening') == 'Yes' and health(host_port):
        result.update(status='Verified', code='healthy')
        return None, None
    if f['installation'] != 'Docker Compose':
        fail('compose_required')
    compose = f.get('compose', '')
    project = f.get('compose_project', '')
    if ',' in compose or not compose or not workdir or not re.fullmatch(r'[a-zA-Z0-9][a-zA-Z0-9_.-]*', str(service)) or not re.fullmatch(r'[a-z0-9][a-z0-9_-]*', project):
        fail('compose')
    path_ok(workdir)
    config = f['config']
    original, config_meta = read_file(config)
    compose_bytes, compose_meta = read_file(compose)
    # Require ownership to preserve owner/group without privilege escalation.
    for path, meta in ((config, config_meta), (compose, compose_meta)):
        if meta.st_uid != os.geteuid() or not os.access(path, os.W_OK) or not os.access(os.path.dirname(path), os.W_OK):
            fail('write_permission')
        if meta.st_nlink != 1:
            fail('path')
    config_text, compose_text = original.decode('utf-8'), compose_bytes.decode('utf-8')
    c = load_yaml(config_text)
    model = load_yaml(compose_text)
    if '$' in compose_text or any(key in model for key in ('include', 'extends')):
        fail('compose')
    services = model.get('services', {})
    svc = services.get(service, {})
    if not svc or any(k in svc for k in ('extends', 'env_file', 'network_mode', 'post_start', 'pre_stop', 'build', 'profiles', 'pull_policy', 'develop')):
        fail('compose')
    # The real project context must resolve to the inspected runtime settings.
    base = ['docker', 'compose', '--env-file', '/dev/null', '--project-directory', workdir, '-p', project, '-f', compose]
    effective = json.loads(run_cmd(base + ['config', '--format', 'json']))
    hashes = run_cmd(base + ['config', '--hash', service]).split()
    if hashes != [service, runtime_hash]:
        fail('compose')
    resolved = effective.get('services', {}).get(service, {})
    if run_cmd(['docker', 'image', 'inspect', '--format', '{{.Id}}', image]).strip() != image_id:
        fail('stale')
    if resolved.get('image') != image or resolved.get('network_mode') or effective.get('include'):
        fail('compose')
    # Bind-mounted directories survive atomic file replacement. Single-file
    # mounts keep the old inode across restart and are deliberately unsupported.
    matched = False
    for mount in mounts:
        source = mount.get('Source', '')
        if mount.get('Type') == 'bind' and os.path.dirname(config) == source and os.path.basename(mount.get('Destination', '')) == 'config':
            matched = any(v.get('type') == 'bind' and v.get('source') == source and v.get('target') == mount.get('Destination') for v in resolved.get('volumes', []))
    if not matched:
        fail('mount')
    updated_config = config_text
    edits = []
    if not result['enabled']:
        updated_config = set_scalar(updated_config, 'flags', 'enable_integration_api', 'true')
        edits.append('flags.enable_integration_api: false -> true')
    if 'api_port' not in f:
        updated_config = set_scalar(updated_config, 'server', 'integration_port', str(port))
        edits.append('server.integration_port: absent -> ' + str(port) + ' (documented default, explicit after approval)')
    updated_model = load_yaml(updated_config)
    wanted = json.loads(json.dumps(c))
    wanted.setdefault('flags', {})['enable_integration_api'] = True
    if 'api_port' not in f:
        wanted.setdefault('server', {})['integration_port'] = port
    if updated_model != wanted:
        fail('yaml')
    updated_compose = compose_text
    # Only literal short-form port lists are edited, never generated templates.
    needs_mapping = external or not ipv4
    if needs_mapping:
        raw_ports = svc.get('ports', [])
        if not isinstance(raw_ports, list) or any(not isinstance(v, str) for v in raw_ports):
            fail('compose')
        remove = []
        for value in raw_ports:
            m = re.fullmatch(r'(?:(127\.0\.0\.1|0\.0\.0\.0):)?([0-9]+):([0-9]+)(?:/tcp)?', value)
            if not m:
                fail('compose')
            if int(m[3]) == port:
                remove.append(value)
        if len(remove) > 1 or (external and not remove):
            fail('compose')
        if remove:
            m = re.fullmatch(r'(?:(127\.0\.0\.1|0\.0\.0\.0):)?([0-9]+):([0-9]+)(?:/tcp)?', remove[0])
            host_port = int(m[2])
            if any(str(b.get('HostPort')) != str(host_port) for b in bindings):
                fail('compose')
        else:
            # Do not claim availability; refuse if another listener owns it.
            import socket
            with socket.socket() as s:
                try:
                    s.bind(('127.0.0.1', host_port))
                except OSError:
                    fail('port_busy')
        publication = '127.0.0.1:' + str(host_port) + ':' + str(port)
        lines = compose_text.splitlines(keepends=True)
        start = next((i for i, l in enumerate(lines) if re.fullmatch(r'  ' + re.escape(service) + r':\s*(#.*)?\n?', l)), None)
        if start is None:
            fail('compose')
        end = next((i for i in range(start + 1, len(lines)) if re.match(r'\S|  \S', lines[i])), len(lines))
        pstart = [i for i in range(start + 1, end) if re.fullmatch(r'    ports:\s*(#.*)?\n?', lines[i])]
        if len(pstart) > 1 or (raw_ports and not pstart):
            fail('compose')
        if pstart:
            i = pstart[0] + 1
            stop = next((j for j in range(i, end) if re.match(r'    \S', lines[j])), end)
            for value in remove:
                matches = [j for j in range(i, stop) if re.fullmatch(r'      - [\'\"]?' + re.escape(value) + r'[\'\"]?\s*(#.*)?\n?', lines[j])]
                if len(matches) != 1:
                    fail('compose')
                lines[matches[0]] = ''
            lines.insert(i, '      - "' + publication + '"\n')
        else:
            lines.insert(start + 1, '    ports:\n      - "' + publication + '"\n')
        updated_compose = ''.join(lines)
        proposed_model = load_yaml(updated_compose)
        expected_model = load_yaml(compose_text)
        expected_model['services'][service]['ports'] = [publication] + [v for v in raw_ports if v not in remove]
        if proposed_model != expected_model:
            fail('compose')
        edits.append('API port publication: ' + ('externally exposed' if external else 'not host-accessible') + ' -> ' + publication)
    if not edits:
        fail('health')
    backup = os.path.join(os.path.dirname(config), '.homestack-api-backup-' + time.strftime('%Y%m%dT%H%M%SZ', time.gmtime()) + '-' + uuid.uuid4().hex)
    files = []
    for path, old, new, meta in ((config, original, updated_config.encode(), config_meta), (compose, compose_bytes, updated_compose.encode(), compose_meta)):
        # Pin even unchanged inputs. Only changed files are backed up/replaced.
        files.append(dict(path=path, before=digest(old), after=digest(new),
                          device=meta.st_dev, inode=meta.st_ino, mode=stat.S_IMODE(meta.st_mode), uid=meta.st_uid, gid=meta.st_gid))
    plan = dict(identity=identity, image_id=image_id, config=config, compose=compose, service=service,
                project=project, workdir=workdir, files=files, edits=edits, backup=backup,
                effective=digest(json.dumps(effective, sort_keys=True).encode()),
                host_port=host_port, port=port, external=external)
    result.update(status='Changes required', code='plan', plan=plan,
                  endpoint='http://127.0.0.1:' + str(host_port) + '/v1/')
    return plan, (updated_config.encode(), updated_compose.encode())


def atomic_file(path, data, meta):
    parent = parent_fd(path)
    temporary = '.homestack-api-' + uuid.uuid4().hex
    try:
        fd = os.open(temporary, os.O_WRONLY | os.O_CREAT | os.O_EXCL | os.O_NOFOLLOW, 0o600, dir_fd=parent)
        with os.fdopen(fd, 'wb') as stream:
            os.fchown(stream.fileno(), meta['uid'], meta['gid'])
            os.fchmod(stream.fileno(), meta['mode'])
            stream.write(data)
            stream.flush()
            os.fsync(stream.fileno())
        current = os.stat(os.path.basename(path), dir_fd=parent, follow_symlinks=False)
        if (current.st_ino != meta['inode'] or current.st_dev != meta['device'] or
                current.st_uid != meta['uid'] or current.st_gid != meta['gid'] or
                stat.S_IMODE(current.st_mode) != meta['mode']):
            fail('stale')
        original_fd = os.open(os.path.basename(path), os.O_RDONLY | os.O_NOFOLLOW, dir_fd=parent)
        with os.fdopen(original_fd, 'rb') as stream:
            if digest(stream.read(262145)) != meta['before']:
                fail('stale')
        os.replace(temporary, os.path.basename(path), src_dir_fd=parent, dst_dir_fd=parent)
        result['changed'] = 'Configuration changed; service activation not verified'
        os.fsync(parent)
    finally:
        try:
            os.unlink(temporary, dir_fd=parent)
        except FileNotFoundError:
            pass
        os.close(parent)


def apply(expected, approved):
    plan, proposed = inspect(expected)
    if plan is None:
        fail('stale')
    plan['backup'] = approved['backup']
    if plan != approved:
        fail('stale')
    # Back up only files to be changed; no full-directory copies/downloads.
    backup = plan['backup']
    if os.path.dirname(backup) != os.path.dirname(plan['config']) or not re.fullmatch(r'\.homestack-api-backup-[0-9TZ]+-[a-f0-9]{32}', os.path.basename(backup)):
        fail('path')
    parent = parent_fd(backup)
    backup_fd = None
    try:
        os.mkdir(os.path.basename(backup), 0o700, dir_fd=parent)
        backup_fd = os.open(os.path.basename(backup), os.O_RDONLY | os.O_DIRECTORY | os.O_NOFOLLOW, dir_fd=parent)
        result['backup'] = backup
        for i, f in enumerate(plan['files']):
            data, meta = read_file(f['path'])
            if digest(data) != f['before'] or meta.st_ino != f['inode'] or meta.st_dev != f['device']:
                fail('stale')
            if f['before'] != f['after']:
                fd = os.open(str(i) + '.original', os.O_WRONLY | os.O_CREAT | os.O_EXCL | os.O_NOFOLLOW, 0o600, dir_fd=backup_fd)
                with os.fdopen(fd, 'wb') as stream:
                    stream.write(data)
                    stream.flush()
                    os.fsync(stream.fileno())
        fd = os.open('recovery.json', os.O_WRONLY | os.O_CREAT | os.O_EXCL | os.O_NOFOLLOW, 0o600, dir_fd=backup_fd)
        with os.fdopen(fd, 'w') as stream:
            json.dump(dict(files=plan['files'], service=plan['service'], project=plan['project']), stream)
            stream.flush()
            os.fsync(stream.fileno())
        os.fsync(backup_fd)
        os.fsync(parent)
    except Refuse:
        raise
    except OSError:
        fail('backup')
    finally:
        if backup_fd is not None:
            os.close(backup_fd)
        os.close(parent)
    # Validate the complete proposed Compose model before replacing live files.
    fd, temporary = tempfile.mkstemp(prefix='.homestack-api-validation-', suffix='.yml', dir=os.path.dirname(plan['compose']))
    try:
        with os.fdopen(fd, 'wb') as stream:
            stream.write(proposed[1])
        command = ['docker', 'compose', '--env-file', '/dev/null', '--project-directory', plan['workdir'], '-p', plan['project']]
        try:
            validated = json.loads(run_cmd(command + ['-f', temporary, 'config', '--format', 'json']))
        except Refuse:
            fail('validation')
    finally:
        os.unlink(temporary)
    for f, data in zip(plan['files'], proposed):
        now, meta = read_file(f['path'])
        if digest(now) != f['before'] or meta.st_ino != f['inode'] or meta.st_dev != f['device']:
            fail('stale')
    for f, data in zip(plan['files'], proposed):
        if f['before'] != f['after']:
            # Recheck immediately before each replacement; no unapproved rollback.
            now, meta = read_file(f['path'])
            if digest(now) != f['before'] or meta.st_ino != f['inode']:
                fail('stale')
            atomic_file(f['path'], data, f)
            result['changed'] = 'Configuration changed; service activation not verified'
    # Do not activate unapproved edits made after our replacements.
    for f in plan['files']:
        current, _ = read_file(f['path'])
        if digest(current) != f['after']:
            fail('stale')
    effective_now = json.loads(run_cmd(command + ['-f', plan['compose'], 'config', '--format', 'json']))
    if effective_now != validated:
        fail('stale')
    image = effective_now.get('services', {}).get(plan['service'], {}).get('image')
    if not image or run_cmd(['docker', 'image', 'inspect', '--format', '{{.Id}}', image]).strip() != plan['image_id']:
        fail('stale')
    # Restart only this service, never dependencies, images or the whole stack.
    try:
        run_cmd(command + ['-f', plan['compose'], 'up', '-d', '--force-recreate', '--no-deps', '--no-build', '--pull', 'never', plan['service']], 40)
    except Refuse:
        fail('restart')
    result['changed'] = 'Changes applied; service health not verified'
    # Refresh runtime evidence after recreation (container ID legitimately changes).
    for _ in range(12):
        namespace = {}
        with contextlib.redirect_stdout(io.StringIO()):
            exec(PROBE, namespace)
        f = namespace['facts']
        if f.get('config') != plan['config'] or f.get('compose_project') != plan['project']:
            fail('stale')
        bindings_ok = f.get('api_publication') == 'Loopback only'
        if namespace['outcome'] == 'Discovered' and f.get('api_enabled') == 'true' and f.get('api_listening') == 'Yes' and bindings_ok and health(plan['host_port']):
            result.update(status='Verified', code='healthy', plan=None, enabled=True, running=True, access='Loopback only',
                          changed='Changes applied and private health verified', inventory=dict(outcome=namespace['outcome'], facts=f, issues=namespace.get('issues', [])))
            # Preserve the original installation identity association locally;
            # Phase 3 must be rerun if the installation is independently replaced.
            return
        time.sleep(1)
    fail('health')


def main():
    try:
        if APPROVED is None:
            inspect(EXPECTED)
        else:
            apply(EXPECTED, APPROVED)
    except PermissionError:
        result.update(status='Permission denied', code='write_permission' if APPROVED else 'permission', plan=None)
    except Refuse as error:
        result.update(status='Failed', code=str(error), plan=None)
    except Exception:
        # Exception strings can contain YAML secrets, paths or command output.
        result.update(status='Failed', code='unexpected', plan=None)
    print(json.dumps(result, separators=(',', ':')))


main()
