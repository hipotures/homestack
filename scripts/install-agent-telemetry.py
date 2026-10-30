#!/usr/bin/env python3
"""Configure the current user's agents and enable Remote Control at next boot."""

import argparse
from datetime import datetime
import json
import os
from pathlib import Path
import re
import shutil
import subprocess
import tomllib
from urllib.parse import urlsplit


def write(path, contents, mode=0o644):
    path.parent.mkdir(parents=True, exist_ok=True)
    temporary = path.with_name(path.name + ".agent-setup")
    temporary.write_text(contents)
    temporary.chmod(mode)
    temporary.replace(path)


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--collector-url", default="http://otel.homestack.app:4318")
    parser.add_argument("--claude-directory", type=Path, required=True)
    parser.add_argument("--autostart", choices=["both", "claude", "codex", "none"], default="both")
    args = parser.parse_args()
    endpoint = args.collector_url.rstrip("/")
    if urlsplit(endpoint).scheme not in ("http", "https") or not urlsplit(endpoint).netloc:
        parser.error("--collector-url must be an HTTP(S) URL")
    directory = args.claude_directory.resolve(strict=True)
    if not directory.is_dir():
        parser.error("--claude-directory must be a directory")
    if any(character in str(directory) for character in ("\n", "\r", "\\")):
        parser.error("--claude-directory cannot contain newlines or backslashes")
    home = Path.home()
    for agent in ("claude", "codex"):
        if not os.access(home / ".local/bin" / agent, os.X_OK):
            parser.error(f"Install {agent} in ~/.local/bin first")
    linger = subprocess.check_output(
        ["loginctl", "show-user", str(os.getuid()), "-p", "Linger", "--value"], text=True
    ).strip()
    if args.autostart != "none" and linger != "yes":
        parser.error("Enable boot-time user services first: loginctl enable-linger USER")

    # Snapshot configuration files only; never copy agent databases or tokens.
    backup = home / ".local/state/homestack/agent-setup" / datetime.now().strftime("%Y%m%d-%H%M%S-%f")
    files = [".bashrc", ".profile", ".claude/settings.json", ".codex/config.toml", ".config/homestack/client.toml"]
    files += [f".config/systemd/user/{agent}-remote-control.service" for agent in ("claude", "codex")]
    backup.mkdir(parents=True, mode=0o700)
    for relative in files:
        source = home / relative
        if source.is_file():
            target = backup / relative
            target.parent.mkdir(parents=True, exist_ok=True)
            shutil.copy2(source, target)
            target.chmod(0o600)

    settings_path = home / ".claude/settings.json"
    settings = json.loads(settings_path.read_text()) if settings_path.exists() else {}
    settings["remoteControlAtStartup"] = True
    env = settings.setdefault("env", {})
    for key in list(env):
        if key.startswith("OTEL_"):
            del env[key]
    env.update({
        "CLAUDE_CODE_ENABLE_TELEMETRY": "1",
        "OTEL_METRICS_EXPORTER": "otlp",
        "OTEL_LOGS_EXPORTER": "otlp",
        "OTEL_EXPORTER_OTLP_PROTOCOL": "http/protobuf",
        "OTEL_EXPORTER_OTLP_METRICS_ENDPOINT": endpoint + "/v1/metrics",
        "OTEL_EXPORTER_OTLP_LOGS_ENDPOINT": endpoint + "/v1/logs",
    })
    write(settings_path, json.dumps(settings, indent=2) + "\n", 0o600)

    config_path = home / ".codex/config.toml"
    config = config_path.read_text() if config_path.exists() else ""
    # Replace the exporter section and its subtables, preserving unrelated TOML.
    sections = re.split(r"(?m)(?=^\[)", config)
    config = "".join(section for section in sections if not re.match(r"^\[otel(?:\.|\])", section))
    otel = '\n[otel]\nlog_user_prompt = false\n'
    for key, signal in (("exporter", "logs"), ("metrics_exporter", "metrics")):
        otel += f'{key} = {{ otlp-http = {{ endpoint = {json.dumps(endpoint + "/v1/" + signal)}, protocol = "binary" }} }}\n'
    config = config.rstrip() + "\n" + otel
    tomllib.loads(config)
    write(config_path, config, 0o600)

    root = home / ".local/share/homestack/agents"
    write(root / "agent-start", Path(__file__).with_name("agent-start").read_text(), 0o755)
    (root / "bin").mkdir(exist_ok=True)
    for agent in ("claude", "codex"):
        link = root / "bin" / agent
        link.unlink(missing_ok=True)
        link.symlink_to("../agent-start")

    # Profiles contain PATH wiring only. Telemetry has exactly one runtime source.
    path_block = '\n# >>> homestack agent entrypoints >>>\nexport PATH="$HOME/.local/share/homestack/agents/bin:$PATH"\n# <<< homestack agent entrypoints <<<\n'
    for relative in (".bashrc", ".profile"):
        path = home / relative
        text = path.read_text() if path.exists() else ""
        text = re.sub(r"(?ms)^# >>> homestack (?:codex telemetry|agent entrypoints) >>>\n.*?^# <<< homestack (?:codex telemetry|agent entrypoints) <<<\n?", "", text)
        text = "\n".join(line for line in text.splitlines() if not re.match(r"\s*export OTEL_", line) and line != "# OTLP metrics settings for Prometheus.")
        write(path, text.rstrip() + "\n" + path_block)

    # Prevent workspace setup from restoring the obsolete shell exports.
    client_path = home / ".config/homestack/client.toml"
    if client_path.exists():
        client = client_path.read_text()
        client = "\n".join(line for line in client.splitlines() if not re.match(r'\s*"OTEL_', line)) + "\n"
        tomllib.loads(client)
        write(client_path, client, 0o600)
    (home / ".config/environment.d/90-codex-metrics.conf").unlink(missing_ok=True)
    (home / ".local/share/homestack/codex-telemetry.bash").unlink(missing_ok=True)

    common = "[Unit]\nDescription={description}\n\n[Service]\nWorkingDirectory={directory}\nEnvironment=PATH=%h/.local/share/homestack/agents/bin:%h/.local/bin:%h/.cargo/bin:/usr/local/bin:/usr/bin:/bin\n{body}\n[Install]\nWantedBy=default.target\n"
    command = "%h/.local/share/homestack/agents/agent-start"
    units = {
        "claude": common.format(description="Claude Code Remote Control", directory=str(directory).replace("%", "%%"), body=f"Type=simple\nExecStart={command} claude remote-control\nRestart=on-failure\nRestartSec=15\nTimeoutStopSec=90\n"),
        # The native daemon and its updater own process recovery and updates.
        "codex": common.replace("\n\n[Service]", "\nBefore=herdr.service\n\n[Service]").format(description="Codex managed Remote Control daemon", directory="%h", body=f"Type=oneshot\nRemainAfterExit=yes\nExecStart={command} codex remote-control start\nExecStop={command} codex app-server daemon stop\nRestart=on-failure\nRestartSec=15\nTimeoutStartSec=5min\nTimeoutStopSec=5min\n"),
    }
    for agent, unit in units.items():
        write(home / f".config/systemd/user/{agent}-remote-control.service", unit)
    subprocess.run(["systemctl", "--user", "daemon-reload"], check=True)
    for agent in units:
        action = "enable" if args.autostart in ("both", agent) else "disable"
        subprocess.run(["systemctl", "--user", action, f"{agent}-remote-control.service"], check=True)
    print(f"Configured agents; existing processes left running. Backups: {backup}")
    print("Open a new SSH login for interactive use. Enabled services start at next boot.")


if __name__ == "__main__":
    main()
