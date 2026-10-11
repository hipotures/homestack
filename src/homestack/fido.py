"""Choose which FIDO security keys an ssh session can see.

OpenSSH asks every attached FIDO device in USB order, so a locked second key
(for example a Trezor) can ask for a PIN before the key that owns the login
handle is tried. HomeStack runs the workspace ssh in a bubblewrap mount
namespace that exposes only the preferred kind of device.
"""
from __future__ import annotations

from dataclasses import dataclass
import re
import shutil
import subprocess
from typing import Callable

from .models import AppError

# Names accepted in [workspace_ssh] fido_devices, mapped to USB vendor IDs.
NAMES: dict[str, tuple[str, ...]] = {"yubikey": ("1050",), "trezor": ("1209",)}
LABELS = {"yubikey": "YubiKey", "trezor": "Trezor"}
_ID_RE = re.compile(r"[0-9a-f]{4}(:[0-9a-f]{4})?")
_LISTING_RE = re.compile(
    r"^(/dev/hidraw[0-9]+): vendor=0x([0-9a-fA-F]+), product=0x([0-9a-fA-F]+)", re.MULTILINE
)


@dataclass(frozen=True)
class Device:
    path: str
    vendor: str
    product: str


def validate_entry(value: object) -> str:
    """Normalize one fido_devices entry: a known name, ``vvvv`` or ``vvvv:pppp`` (hex)."""
    if not isinstance(value, str):
        raise AppError("[workspace_ssh] fido_devices entries must be strings")
    entry = value.strip().lower()
    if entry in NAMES or _ID_RE.fullmatch(entry):
        return entry
    raise AppError(
        "[workspace_ssh] fido_devices entries must be "
        + ", ".join(NAMES) + " or a USB id such as 1050 or 1050:0407"
    )


def label(entry: str) -> str:
    return LABELS.get(entry, entry)


def parse_listing(text: str) -> list[Device]:
    """Parse ``fido2-token -L`` output into devices with 4-digit lowercase IDs."""
    return [
        Device(path, vendor.lower().zfill(4), product.lower().zfill(4))
        for path, vendor, product in _LISTING_RE.findall(text or "")
    ]


def list_devices(run: Callable[..., subprocess.CompletedProcess] = subprocess.run) -> list[Device]:
    if shutil.which("fido2-token") is None:
        raise AppError("fido2-token (libfido2 tools) is required for [workspace_ssh] fido_devices")
    try:
        result = run(["fido2-token", "-L"], capture_output=True, text=True, timeout=10)
    except (OSError, subprocess.TimeoutExpired) as exc:
        raise AppError("Could not list security keys with fido2-token") from exc
    return parse_listing(result.stdout)


def _matches(entry: str, device: Device) -> bool:
    if entry in NAMES:
        return device.vendor in NAMES[entry]
    if ":" in entry:
        return f"{device.vendor}:{device.product}" == entry
    return device.vendor == entry


def select(preference: tuple[str, ...], devices: list[Device]) -> tuple[str, list[Device]]:
    """Return the first preferred kind that has an attached device, with all its devices."""
    for entry in preference:
        found = [device for device in devices if _matches(entry, device)]
        if found:
            return entry, found
    raise AppError(
        "No configured security key is connected ("
        + ", ".join(label(entry) for entry in preference)
        + "); plug one in and retry."
    )


def missing_key_message(entry: str, identity_files: list[str], alias: str) -> str | None:
    """Explain why the chosen kind of key cannot log in, from the host's IdentityFile names.

    Returns None when the host lists a key for that kind, or names no kind at all.
    """
    if entry not in NAMES:
        return None
    named = {name for path in identity_files for name in NAMES if name in path.lower()}
    if not named or entry in named:
        return None
    others = " or ".join(label(name) for name in sorted(named))
    return (
        f"{alias} has no {label(entry)} key configured; only {others} keys are set up there. "
        f"Connect a {others} or add a {label(entry)} key to the host."
    )


def sandbox_prefix(devices: list[Device]) -> list[str]:
    """argv prefix that runs a command with only ``devices`` visible below /dev."""
    bwrap = shutil.which("bwrap")
    if bwrap is None:
        raise AppError(
            "bubblewrap (bwrap) is required for [workspace_ssh] fido_devices; "
            "install it or remove the setting."
        )
    prefix = [bwrap, "--bind", "/", "/", "--dev", "/dev"]
    for device in devices:
        prefix += ["--dev-bind", device.path, device.path]
    return prefix


def advice(devices: list[Device]) -> str | None:
    """A hint for when several kinds of security key are attached and no preference is set."""
    kinds = {device.vendor for device in devices}
    if len(kinds) < 2:
        return None
    names = ", ".join(
        sorted({next((LABELS[n] for n, ids in NAMES.items() if device.vendor in ids), device.vendor)
                for device in devices})
    )
    return (
        f"Several security keys are connected ({names}). A key that is not used for this login may ask "
        "for a PIN: reject it on that device, unplug it, or set [workspace_ssh] fido_devices to choose one."
    )
