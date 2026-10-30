#!/usr/bin/env python3
"""Install native agent telemetry settings and one auto-detected boot script."""
import argparse
from datetime import datetime
import json
import os
from pathlib import Path
import shutil
import stat
import subprocess
import sys
import tempfile
from urllib.parse import urlsplit


def atomic_write(path, data):
    target = path.resolve()
    target.parent.mkdir(parents=True, exist_ok=True)
    mode = stat.S_IMODE(target.stat().st_mode) if target.exists() else 0o600
    fd, name = tempfile.mkstemp(prefix='.' + target.name + '.', dir=target.parent)
    try:
        with os.fdopen(fd, 'w') as stream:
            os.fchmod(stream.fileno(), mode)
            stream.write(data)
            stream.flush()
            os.fsync(stream.fileno())
        os.replace(name, target)
    finally:
        Path(name).unlink(missing_ok=True)


def backup(path, stamp):
    if not path.exists():
        return None
    destination = path.with_name(path.name + '.bak.' + stamp)
    fd = os.open(destination, os.O_WRONLY | os.O_CREAT | os.O_EXCL, 0o600)
    with os.fdopen(fd, 'wb') as stream, path.open('rb') as source:
        shutil.copyfileobj(source, stream)
    return destination


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument('--collector-url', default='http://otel.homestack.app:4318')
    parser.add_argument('--dry-run', action='store_true', help='validate and describe changes without writing or sudo')
    args = parser.parse_args()
    if sys.version_info < (3, 11):
        parser.error('Python 3.11 or later is required')
    if not shutil.which('git'):
        parser.error('Install Git first')
    if not shutil.which('systemctl'):
        parser.error('This installer requires systemd')
    rc_support = any(Path(p).is_file() for p in (
        '/etc/systemd/system/rc-local.service', '/usr/lib/systemd/system/rc-local.service',
        '/lib/systemd/system/rc-local.service'))
    rc_path = Path('/etc/rc.local')
    generator_path = Path('/etc/systemd/system-generators/homestack-agents')
    installed = rc_path if rc_support else generator_path
    alternative = generator_path if rc_support else rc_path
    root = Path(__file__).resolve().parent
    sys.path.insert(0, str(root / 'vendor'))
    try:
        import tomlkit
    except ImportError:
        parser.error('Run install from the extracted tar.gz package (it bundles tomlkit).')
    endpoint = args.collector_url.rstrip('/')
    url = urlsplit(endpoint)
    if url.scheme not in ('http', 'https') or not url.netloc or url.query or url.fragment or url.username:
        parser.error('--collector-url must be an HTTP(S) base URL without credentials, query or fragment')
    home = Path.home()
    codex_path = home / '.codex/config.toml'
    claude_path = home / '.claude/settings.json'
    boot = root / 'rc.local'
    if not boot.is_file():
        parser.error('Missing packaged rc.local')
    compile(boot.read_text(), str(boot), 'exec')
    # Parse and construct BOTH results before changing either configuration.
    old_codex = codex_path.read_text() if codex_path.exists() else ''
    codex = tomlkit.parse(old_codex)
    otel = codex.setdefault('otel', tomlkit.table())
    if not hasattr(otel, 'get'):
        parser.error('Codex [otel] must be a TOML table')
    otel['log_user_prompt'] = False
    for key, signal in [('exporter', 'logs'), ('metrics_exporter', 'metrics')]:
        existing = otel.get(key, {})
        current = existing.get('otlp-http', {}) if hasattr(existing, 'get') else {}
        original = current.get('headers', {}).get('X-Homestack-Upstream', current.get('endpoint', ''))
        # A running local relay uses the metrics exporter as its upstream source.
        if urlsplit(original).hostname == '127.0.0.1':
            metrics = otel.get('metrics_exporter', {})
            if hasattr(metrics, 'get'):
                current_metrics = metrics.get('otlp-http', {})
                original = current_metrics.get('headers', {}).get('X-Homestack-Upstream', current_metrics.get('endpoint', ''))
        sensitive_headers = {k: v for k, v in current.get('headers', {}).items() if k.lower() != 'x-homestack-upstream'}
        if (sensitive_headers or current.get('tls')) and (urlsplit(original).scheme, urlsplit(original).netloc) != (url.scheme, url.netloc):
            parser.error(f'Codex {key} has headers/TLS for another Collector; review them before changing its destination')
        replacement = dict(current)
        replacement.update(endpoint=endpoint + '/v1/' + signal, protocol='binary')
        otel[key] = {'otlp-http': replacement}
    new_codex = tomlkit.dumps(codex)
    tomlkit.parse(new_codex)
    old_claude = claude_path.read_text() if claude_path.exists() else None
    claude = json.loads(old_claude) if old_claude is not None else {}
    if not isinstance(claude, dict) or not isinstance(claude.get('env', {}), dict):
        parser.error('Claude settings and env must be JSON objects')
    env = claude.setdefault('env', {})
    for signal in ('METRICS', 'LOGS'):
        original = env.get(f'HOMESTACK_OTEL_{signal}_UPSTREAM', env.get(f'OTEL_EXPORTER_OTLP_{signal}_ENDPOINT', env.get('OTEL_EXPORTER_OTLP_ENDPOINT', '')))
        if (env.get('OTEL_EXPORTER_OTLP_HEADERS') or env.get(f'OTEL_EXPORTER_OTLP_{signal}_HEADERS')) and (urlsplit(original).scheme, urlsplit(original).netloc) != (url.scheme, url.netloc):
            parser.error(f'Claude {signal.lower()} headers belong to another Collector; review them before changing its destination')
    env.update({
        'CLAUDE_CODE_ENABLE_TELEMETRY': '1',
        'OTEL_METRICS_EXPORTER': 'otlp',
        'OTEL_LOGS_EXPORTER': 'otlp',
        'OTEL_EXPORTER_OTLP_PROTOCOL': 'http/protobuf',
        'OTEL_EXPORTER_OTLP_METRICS_ENDPOINT': endpoint + '/v1/metrics',
        'OTEL_EXPORTER_OTLP_LOGS_ENDPOINT': endpoint + '/v1/logs',
        'OTEL_EXPORTER_OTLP_METRICS_TEMPORALITY_PREFERENCE': 'cumulative',
        'OTEL_METRICS_INCLUDE_REPOSITORY': '1',
    })
    new_claude = json.dumps(claude, indent=2, ensure_ascii=False) + '\n'
    if installed.exists() and installed.read_bytes() != boot.read_bytes():
        if b'Boot configured agents using native accounts, configs and workspace trust.' not in installed.read_bytes():
            parser.error(f'{installed} already contains a different startup script; refusing to overwrite it')
    remove_alternative = (alternative.exists() and
        b'Boot configured agents using native accounts, configs and workspace trust.' in alternative.read_bytes())
    for path in (codex_path, claude_path):
        if path.exists() and path.stat().st_uid != os.getuid():
            parser.error(f'{path} is owned by another account; run as its owner')
    print(f'Agent account: {__import__("pwd").getpwuid(os.getuid()).pw_name} ({home})')
    print(f'Collector: {endpoint}')
    print(f'Autostart: {"rc.local" if rc_support else "systemd generator"} ({installed})')
    print(f'Will merge telemetry into {codex_path} and {claude_path}; install {installed}.')
    if args.dry_run:
        print('Dry run: configuration syntax valid; no files changed.')
        return
    sudo = [] if os.geteuid() == 0 else ['sudo']
    if sudo:
        subprocess.run(sudo + ['-v'], check=True)
    stamp = datetime.now().strftime('%Y%m%d-%H%M%S-%f')
    backups = {p: backup(p, stamp) for p in (codex_path, claude_path)}
    for path, saved in backups.items():
        print(f'Backup: {saved}' if saved else f'New configuration: {path}')
    # Only this explicitly privileged stage writes outside the caller's home.
    for original in [installed] + ([alternative] if remove_alternative else []):
        if original.exists():
            saved = str(original) + '.bak.' + stamp
            subprocess.run(sudo + ['install', '-o', 'root', '-g', 'root', '-m', '0600', str(original), saved], check=True)
            print(f'Boot script backup: {saved}')
    try:
        atomic_write(codex_path, new_codex)
        atomic_write(claude_path, new_claude)
        subprocess.run(sudo + ['install', '-D', '-o', 'root', '-g', 'root', '-m', '0755', str(boot), str(installed)], check=True)
        if remove_alternative:
            subprocess.run(sudo + ['rm', '--', str(alternative)], check=True)
    except Exception:
        for path, saved in backups.items():
            if saved:
                atomic_write(path, saved.read_text())
            else:
                path.unlink(missing_ok=True)
        raise
    print('Installed. Running agents were not restarted. Reboot when ready.')
    print('Existing agent logins, workspace trust and unrelated settings were preserved.')


if __name__ == '__main__':
    try:
        main()
    except (OSError, ValueError, subprocess.CalledProcessError) as error:
        sys.exit(f'Installation failed: {type(error).__name__}: {error}')
