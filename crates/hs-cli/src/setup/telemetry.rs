//! The standalone telemetry installer, bundled into the setup catalog.

use hs_install::shell;

use super::definitions::Application;
use super::sha256_hex;

const INSTALLER: &str = include_str!("../../../../scripts/install-agent-telemetry.py");
const BOOT: &str = include_str!("../../../../scripts/agent-rc.local");

pub fn application() -> Application {
    let command = format!(
        r#"mkdir -p "$HOME/.cache/homestack"
package=$(mktemp -d "$HOME/.cache/homestack/agent-telemetry.XXXXXXXX")
trap 'rm -rf -- "$package"' EXIT
printf '%s' {installer} > "$package/install"
printf '%s' {boot} > "$package/rc.local"
TMPDIR="$package" uv pip install --target "$package/vendor" --no-compile-bytecode 'tomlkit==0.13.3'
python3 "$package/install" --collector-url http://otel.homestack.app:4318
"#,
        installer = shell::quote(INSTALLER),
        boot = shell::quote(BOOT),
    );
    let check = format!(
        r#"from pathlib import Path
import hashlib, json, os, tomllib
paths = [Path('/etc/rc.local'), Path('/etc/systemd/system-generators/homestack-agents')]
assert any(p.is_file() and os.access(p, os.X_OK) and hashlib.sha256(p.read_bytes()).hexdigest() == '{digest}' for p in paths)
home = Path.home()
env = json.loads((home / '.claude/settings.json').read_text())['env']
assert env['CLAUDE_CODE_ENABLE_TELEMETRY'] == '1'
assert env['OTEL_METRICS_EXPORTER'] == env['OTEL_LOGS_EXPORTER'] == 'otlp'
otel = tomllib.loads((home / '.codex/config.toml').read_text())['otel']
for signal, key in [('metrics', 'metrics_exporter'), ('logs', 'exporter')]:
    assert env['OTEL_EXPORTER_OTLP_' + signal.upper() + '_ENDPOINT'].endswith('/v1/' + signal)
    assert otel[key]['otlp-http']['endpoint'].endswith('/v1/' + signal)
"#,
        digest = sha256_hex(BOOT.as_bytes()),
    );
    let mut application = Application::new(&command);
    application.prerequisites = ["python3", "git", "systemctl", "uv"].map(str::to_owned).to_vec();
    application.check = format!("python3 -c {}", shell::quote(&check));
    // sudo can prompt in the interactive TUI; unattended runs must already
    // have permission to install the boot script, or have that script installed.
    application.non_interactive = format!("export SUDO_ASKPASS=/bin/false\n{command}");
    application
}
