#!/usr/bin/env python3
"""Build the standalone agent telemetry installer tar.gz."""
import os
from pathlib import Path
import shutil
import subprocess
import tarfile

root = Path(__file__).resolve().parent.parent
work = root / 'tmp/agent-telemetry-package'
work.mkdir(parents=True, exist_ok=True)
package = work / 'homestack-agent-telemetry'
if package.exists():
    shutil.rmtree(package)
package.mkdir()
subprocess.run(['uv', 'pip', 'install', '--target', str(package / 'vendor'), '--no-compile-bytecode', 'tomlkit==0.13.3'],
               check=True, env={**os.environ, 'TMPDIR': str(work)})
shutil.copy2(root / 'scripts/install-agent-telemetry.py', package / 'install')
(package / 'install').chmod(0o755)
shutil.copy2(root / 'scripts/agent-rc.local', package / 'rc.local')
(package / 'README.txt').write_text('''HomeStack agent telemetry installer

Requires Linux, Python 3.11+, Git, systemd and sudo.
Install/sign in to Codex and Claude Code separately; this package contains no credentials.
Run as the account that owns the agent configurations, without sudo:

  ./install --dry-run
  ./install

Optional Collector base URL:
  ./install --collector-url http://otel.homestack.app:4318

The installer detects rc-local support automatically. It installs /etc/rc.local
when supported, otherwise /etc/systemd/system-generators/homestack-agents.
The generator creates service definitions only under /run at boot.
The installer requests sudo only for the system startup script. Existing agent configurations
are backed up beside the originals as .bak.TIMESTAMP, with private permissions.
TOML comments and unrelated settings are preserved. Claude JSON formatting is
normalized; unrelated values are preserved. OTEL HTTP exporter destinations are
replaced with the requested Collector. Existing HTTP exporter headers/TLS are
preserved for the same Collector; changing an authenticated destination requires
reviewing those settings first. Existing unrelated rc.local is never
overwritten. A previous HomeStack rc.local is backed up before replacement.
No agents are restarted during installation. Reboot after installation.
Agent authentication and Claude workspace trust/Remote Control consent must
already be configured. The main trusted repository directory is named after
hostname. This installer does not fix Herdr session restoration.

The package bundles tomlkit 0.13.3 with its license. No network access, pip,
uv, virtual environment or extra dependency installation is required on targets.
Operational configuration remains two native agent configs and one startup script;
timestamped backups are intentionally retained.
''')
output = root / 'tmp/homestack-agent-telemetry.tar.gz'
def filter_member(info):
    if '__pycache__' in Path(info.name).parts or info.name.endswith('.pyc'):
        return None
    return info
with tarfile.open(output, 'w:gz') as archive:
    archive.add(package, arcname=package.name, filter=filter_member)
print(output)
