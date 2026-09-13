from __future__ import annotations

from dataclasses import replace
from pathlib import Path
import tempfile

from homestack import config, models


EXAMPLE_CONFIG_PATH = Path(__file__).resolve().parents[1] / "config.example.toml"


def test_config() -> config.Config:
    return config.Config(
        path=Path("/tmp/homestack-test.toml"),
        transport_type="herdr",
        node="example-node-1",
        control_node="example-node-1",
        gold_vmid=101,
        storage_layouts={
            "homestack-storage-1": ("example-storage-a",),
            "homestack-storage-2": ("example-storage-b",),
            "homestack-storage-3": ("example-storage-b",),
        },
        root_storage="example-storage-a",
        root_disk="scsi0",
        home_storage="example-storage-a",
        home_disk="scsi1",
        default_home_size="20G",
        network_prefix="192.0.2",
        network_cidr=24,
        gateway="192.0.2.1",
        dns_servers=("192.0.2.1", "1.1.1.1"),
        snippet_storage="local",
        snippet_dir=Path("/var/lib/vz/snippets"),
        user_name="user",
        user_uid=1000,
        user_gid=1000,
        workspace_ssh=config.WorkspaceSSHConfig(
            user="user",
            identity_files=("~/.ssh/example-hardware-key",),
            identities_only=True,
            log_level="FATAL",
        ),
        herdr_workspace="example-workspace",
        herdr_tab="example-node-1",
        herdr_debug=True,
        storage_display_unit="GiB",
        storage_display_decimals=0,
    )


def example_setup():
    """Load setup declarations explicitly from the checked-in TOML example."""
    return config.load_config(EXAMPLE_CONFIG_PATH).setup


def example_config() -> config.Config:
    return replace(test_config(), setup=example_setup())


def example_config_from_text(text: str) -> config.Config:
    """Load a modified example document through the normal runtime loader."""
    with tempfile.TemporaryDirectory(prefix="homestack-example-") as directory:
        path = Path(directory) / "config.toml"
        path.write_text(text, encoding="utf-8")
        loaded = config.load_config(path)
    return replace(test_config(), setup=loaded.setup)


def runtime_example_config() -> config.Config:
    """Load the example with arbitrary newer Codex values through TOML."""
    text = EXAMPLE_CONFIG_PATH.read_text(encoding="utf-8")
    marker = 'sandbox_mode = "danger-full-access"\n\n'
    additions = (
        marker
        + "[setup.items.config_files.values.agents]\n"
        + "enabled = true\n"
        + "max_concurrent_threads_per_session = 12\n\n"
    )
    return example_config_from_text(text.replace(marker, additions, 1))


class FakeSession:
    def __init__(self) -> None:
        self.commands: list[str] = []

    def run(self, command: str, **_: object) -> models.RemoteResult:
        self.commands.append(command)
        if command.startswith("test -e "):
            return models.RemoteResult(1, "")
        return models.RemoteResult(0, "")

    def execution_info(self) -> dict[str, object]:
        return {"type": "fake"}
