"""HomeStack Setup integration for the guest-side BK backup tool."""
from __future__ import annotations

from contextlib import contextmanager
from dataclasses import dataclass
import base64
import fcntl
import hashlib
from importlib import resources
import os
from pathlib import Path
import secrets
import shutil
import stat
import subprocess
import tempfile
from typing import Iterator

from .models import AppError


@dataclass(frozen=True)
class ManagedAsset:
    resource_name: str
    relative_path: str
    mode: int


MANAGED_ASSETS = (
    ManagedAsset("bk.py", ".local/bin/bk", 0o755),
    ManagedAsset("backup.service", ".config/systemd/user/backup.service", 0o644),
    ManagedAsset("backup.timer", ".config/systemd/user/backup.timer", 0o644),
)
MANAGED_DIRECTORIES = (".local/bin", ".config/systemd/user", "backup")
AUTHORIZED_KEYS_PATH = ".ssh/authorized_keys"
RETRIEVAL_ROLES = ("archive", "status")


def _validate_vmid(vmid: int) -> int:
    if not isinstance(vmid, int) or isinstance(vmid, bool) or vmid < 1:
        raise AppError(f"Invalid VMID for BK retrieval keys: {vmid!r}")
    return vmid


def _retrieval_directory() -> Path:
    return Path.home() / ".ssh" / "homestack" / "backup"


def retrieval_key_paths(vmid: int) -> dict[str, tuple[Path, Path]]:
    """Return the exact trusted-desktop paths for a workspace's BK keys."""
    vmid = _validate_vmid(vmid)
    directory = _retrieval_directory()
    return {
        role: (
            directory / f"vm{vmid}-bk-{role}",
            directory / f"vm{vmid}-bk-{role}.pub",
        )
        for role in RETRIEVAL_ROLES
    }


def _canonical_comment(role: str, vmid: int) -> str:
    if role not in RETRIEVAL_ROLES:
        raise AppError(f"Unknown BK retrieval key role: {role!r}")
    return f"homestack-bk-{role}-vm{_validate_vmid(vmid)}"


def managed_state_paths() -> tuple[str, ...]:
    """Return managed setup paths, including the guest authorization file."""
    return (*managed_paths(), AUTHORIZED_KEYS_PATH)


def managed_paths() -> tuple[str, ...]:
    return tuple(asset.relative_path for asset in MANAGED_ASSETS)


def load_assets() -> dict[str, bytes]:
    root = resources.files("homestack").joinpath("assets", "backup")
    try:
        return {
            asset.relative_path: root.joinpath(asset.resource_name).read_bytes()
            for asset in MANAGED_ASSETS
        }
    except (FileNotFoundError, OSError) as exc:
        raise AppError("Packaged BK setup assets are unavailable") from exc


def desired_hashes() -> dict[str, str]:
    return {
        path: hashlib.sha256(content).hexdigest()
        for path, content in load_assets().items()
    }


def _local_error(label: str, exc: BaseException) -> AppError:
    return AppError(f"Cannot inspect {label}: {exc}")


def _check_directory_info(
    info: os.stat_result, path: Path, *, exact_mode: int | None = None
) -> None:
    if not stat.S_ISDIR(info.st_mode):
        raise AppError(f"Unsafe BK retrieval path: {path} is not a directory")
    if info.st_uid != os.getuid() or info.st_gid != os.getgid():
        raise AppError(f"Unsafe BK retrieval path: {path} has unexpected ownership")
    if info.st_mode & 0o022:
        raise AppError(f"Unsafe BK retrieval path: {path} is group/world-writable")
    if exact_mode is not None and stat.S_IMODE(info.st_mode) != exact_mode:
        raise AppError(
            f"BK retrieval directory has mode {stat.S_IMODE(info.st_mode):04o}; "
            f"expected {exact_mode:04o}"
        )


def _lstat_directory(
    path: Path, *, exact_mode: int | None = None
) -> os.stat_result | None:
    try:
        info = path.lstat()
    except FileNotFoundError:
        return None
    except OSError as exc:
        raise _local_error(str(path), exc) from exc
    if stat.S_ISLNK(info.st_mode):
        raise AppError(f"Unsafe BK retrieval path: {path} is a symlink")
    _check_directory_info(info, path, exact_mode=exact_mode)
    return info


def _safe_key_directory(*, create: bool) -> tuple[Path, os.stat_result | None]:
    """Inspect or create the local key directory without following symlinks."""
    home = Path.home()
    try:
        home_info = home.lstat()
    except OSError as exc:
        raise _local_error("desktop home", exc) from exc
    if stat.S_ISLNK(home_info.st_mode) or not stat.S_ISDIR(home_info.st_mode):
        raise AppError("Unsafe BK retrieval path: desktop home is not a directory")
    if home_info.st_uid != os.getuid() or home_info.st_gid != os.getgid():
        raise AppError("Unsafe BK retrieval path: desktop home has unexpected ownership")

    current = home
    for relative in (".ssh", ".ssh/homestack", ".ssh/homestack/backup"):
        current = home / relative
        info = _lstat_directory(current)
        if info is None:
            if not create:
                return current, None
            try:
                current.mkdir(mode=0o700)
            except FileExistsError:
                pass
            except OSError as exc:
                raise _local_error(str(current), exc) from exc
            info = _lstat_directory(current, exact_mode=None)
            if info is None:
                raise AppError(f"BK retrieval directory disappeared: {current}")
    return current, info


def _directory_fd(path: Path) -> int:
    try:
        descriptor = os.open(path, os.O_RDONLY | os.O_DIRECTORY | os.O_NOFOLLOW)
    except OSError as exc:
        raise _local_error(str(path), exc) from exc
    try:
        info = os.fstat(descriptor)
        _check_directory_info(info, path)
    except BaseException:
        os.close(descriptor)
        raise
    return descriptor


@contextmanager
def _locked_key_directory(
    vmid: int, *, create: bool
) -> Iterator[tuple[Path, int] | None]:
    """Serialize local key reconciliation using an advisory directory lock."""
    _validate_vmid(vmid)
    directory, info = _safe_key_directory(create=create)
    if info is None:
        yield None
        return
    descriptor = _directory_fd(directory)
    try:
        try:
            fcntl.flock(descriptor, fcntl.LOCK_EX)
        except OSError as exc:
            raise AppError(
                f"Could not lock BK retrieval key directory: {directory}"
            ) from exc
        yield directory, descriptor
    finally:
        try:
            fcntl.flock(descriptor, fcntl.LOCK_UN)
        finally:
            os.close(descriptor)


def _owned_file_info(path: Path, *, label: str) -> os.stat_result | None:
    try:
        info = path.lstat()
    except FileNotFoundError:
        return None
    except OSError as exc:
        raise _local_error(label, exc) from exc
    if stat.S_ISLNK(info.st_mode):
        raise AppError(f"Unsafe BK retrieval key: {path} is a symlink")
    if not stat.S_ISREG(info.st_mode):
        raise AppError(f"Unsafe BK retrieval key: {path} is not a regular file")
    if info.st_uid != os.getuid() or info.st_gid != os.getgid():
        raise AppError(f"Unsafe BK retrieval key: {path} has unexpected ownership")
    if info.st_nlink != 1:
        raise AppError(f"Unsafe BK retrieval key: {path} has multiple hard links")
    return info


def _read_owned_file(path: Path, *, label: str) -> tuple[bytes, os.stat_result]:
    """Read one owned key file through an O_NOFOLLOW descriptor."""
    try:
        descriptor = os.open(path, os.O_RDONLY | os.O_NOFOLLOW)
    except OSError as exc:
        raise _local_error(label, exc) from exc
    try:
        info = os.fstat(descriptor)
        if stat.S_ISLNK(info.st_mode) or not stat.S_ISREG(info.st_mode):
            raise AppError(f"Unsafe BK retrieval key: {path} is not a regular file")
        if info.st_uid != os.getuid() or info.st_gid != os.getgid():
            raise AppError(f"Unsafe BK retrieval key: {path} has unexpected ownership")
        if info.st_nlink != 1:
            raise AppError(f"Unsafe BK retrieval key: {path} has multiple hard links")
        with os.fdopen(descriptor, "rb") as handle:
            descriptor = -1
            return handle.read(), info
    except AppError:
        raise
    except OSError as exc:
        raise _local_error(label, exc) from exc
    finally:
        if descriptor >= 0:
            os.close(descriptor)


def _parse_public_identity(payload: bytes, *, label: str) -> tuple[str, str, str]:
    try:
        text = payload.decode("utf-8")
    except UnicodeDecodeError as exc:
        raise AppError(f"{label} is not valid UTF-8") from exc
    lines = text.splitlines()
    if len(lines) != 1:
        raise AppError(f"{label} must contain exactly one public key")
    parts = lines[0].split()
    if len(parts) < 2 or parts[0] != "ssh-ed25519":
        raise AppError(f"{label} is not an ED25519 public key")
    try:
        base64.b64decode(parts[1].encode("ascii"), validate=True)
    except (UnicodeEncodeError, ValueError) as exc:
        raise AppError(f"{label} has an invalid public key blob") from exc
    return parts[0], parts[1], " ".join(parts[2:])


def _run_ssh_keygen(private: Path, *, pass_fds: tuple[int, ...] = ()) -> tuple[str, str, str]:
    result = subprocess.run(
        ["ssh-keygen", "-y", "-P", "", "-f", str(private)],
        stdin=subprocess.DEVNULL,
        stdout=subprocess.PIPE,
        stderr=subprocess.PIPE,
        text=True,
        check=False,
        pass_fds=pass_fds,
    )
    if result.returncode:
        detail = (result.stderr or result.stdout or "").strip()
        raise AppError(
            f"Could not derive BK retrieval public key from {private}"
            + (f": {detail.splitlines()[-1]}" if detail else "")
        )
    return _parse_public_identity(
        result.stdout.encode(), label=f"Derived public key for {private}"
    )


def _derive_identity(
    private: Path, payload: bytes, mode: int
) -> tuple[str, str, str]:
    """Derive from the inspected bytes without reopening a replaceable path."""
    descriptor = os.memfd_create("homestack-bk-key", os.MFD_CLOEXEC)
    try:
        os.fchmod(descriptor, 0o600)
        with os.fdopen(os.dup(descriptor), "wb") as handle:
            handle.write(payload)
            handle.flush()
        return _run_ssh_keygen(Path(f"/proc/self/fd/{descriptor}"), pass_fds=(descriptor,))
    finally:
        os.close(descriptor)


def _canonical_public(identity: tuple[str, str, str], role: str, vmid: int) -> str:
    key_type, blob, _ = identity
    if key_type != "ssh-ed25519":
        raise AppError("BK retrieval key is not ED25519")
    return f"{key_type} {blob} {_canonical_comment(role, vmid)}"


def _inspect_pair(role: str, vmid: int, directory: Path | None) -> dict:
    private, public = retrieval_key_paths(vmid)[role]
    result = {
        "role": role,
        "private_path": str(private),
        "public_path": str(public),
        "private_exists": False,
        "public_exists": False,
        "private_mode": None,
        "public_mode": None,
        "state": "missing",
        "ready": False,
        "public_key": "",
    }
    if directory is None:
        return result
    private_info = _owned_file_info(private, label=f"private BK {role} key")
    public_info = _owned_file_info(public, label=f"public BK {role} key")
    result["private_exists"] = private_info is not None
    result["public_exists"] = public_info is not None
    result["private_mode"] = stat.S_IMODE(private_info.st_mode) if private_info else None
    result["public_mode"] = stat.S_IMODE(public_info.st_mode) if public_info else None

    private_payload = b""
    if private_info is not None:
        private_payload, _ = _read_owned_file(
            private, label=f"private BK {role} key"
        )
    public_payload = b""
    if public_info is not None:
        public_payload, _ = _read_owned_file(public, label=f"public BK {role} key")

    private_identity = None
    if private_info is not None:
        private_identity = _derive_identity(
            private, private_payload, result["private_mode"]
        )
        result["public_key"] = _canonical_public(private_identity, role, vmid)
    if public_info is not None:
        try:
            public_identity = _parse_public_identity(
                public_payload, label=f"Public BK {role} key"
            )
        except AppError:
            if private_identity is not None:
                raise
            result["state"] = "orphan-public"
            return result
        if private_identity is None:
            result["state"] = "orphan-public"
            return result
        if private_identity[:2] != public_identity[:2]:
            raise AppError(
                f"BK retrieval {role} private/public key mismatch; "
                "refusing to rotate credentials"
            )

    if private_info is not None and public_info is not None:
        result["state"] = "valid"
        result["ready"] = (
            result["private_mode"] == 0o600 and result["public_mode"] == 0o644
        )
    elif private_info is not None:
        result["state"] = "missing-public"
    return result


def inspect_retrieval_keys(vmid: int) -> dict:
    """Inspect local BK key pairs without creating or changing desktop files."""
    vmid = _validate_vmid(vmid)
    directory, info = _safe_key_directory(create=False)
    directory_value = directory if info is not None else None
    pairs = {
        role: _inspect_pair(role, vmid, directory_value)
        for role in RETRIEVAL_ROLES
    }
    identities = {}
    for role, pair in pairs.items():
        public = pair.get("public_key") or ""
        if not public:
            continue
        identity = tuple(public.split()[:2])
        previous = identities.get(identity)
        if previous is not None:
            raise AppError(
                f"BK retrieval keys for {previous} and {role} use the same "
                "public identity; refusing to continue"
            )
        identities[identity] = role
    return {
        "directory": str(_retrieval_directory()),
        "key_paths": {
            role: {
                "private": pair["private_path"],
                "public": pair["public_path"],
            }
            for role, pair in pairs.items()
        },
        "pairs": pairs,
        "public_keys": {role: pair["public_key"] for role, pair in pairs.items()},
        "ready": (info is not None and stat.S_IMODE(info.st_mode) == 0o700
                  and all(pair["ready"] for pair in pairs.values())),
        "exists": any(
            pair["private_exists"] or pair["public_exists"]
            for pair in pairs.values()
        ),
    }


def _file_identity(info: os.stat_result | None) -> tuple | None:
    if info is None:
        return None
    return (info.st_dev, info.st_ino, info.st_mtime_ns, info.st_ctime_ns,
            info.st_size, info.st_mode, info.st_uid, info.st_gid, info.st_nlink)


def _atomic_local_write(
    path: Path, payload: bytes, *, mode: int, replace: bool
) -> None:
    parent = path.parent
    descriptor = _directory_fd(parent)
    temporary = None
    temporary_fd = None
    try:
        try:
            current = os.stat(path.name, dir_fd=descriptor, follow_symlinks=False)
        except FileNotFoundError:
            current = None
        except OSError as exc:
            raise _local_error(str(path), exc) from exc
        if current is not None:
            if stat.S_ISLNK(current.st_mode) or not stat.S_ISREG(current.st_mode):
                raise AppError(
                    f"Unsafe BK retrieval key: {path} is not a regular file"
                )
            if (
                current.st_uid != os.getuid()
                or current.st_gid != os.getgid()
                or current.st_nlink != 1
            ):
                raise AppError(
                    f"Unsafe BK retrieval key: {path} has unsafe ownership or links"
                )
            if not replace:
                raise AppError(
                    f"BK retrieval key appeared during reconciliation: {path}"
                )
        elif not replace and path.exists():
            raise AppError(f"BK retrieval key appeared during reconciliation: {path}")

        for _ in range(32):
            name = ".bk-" + secrets.token_hex(12)
            try:
                temporary_fd = os.open(
                    name,
                    os.O_WRONLY | os.O_CREAT | os.O_EXCL | os.O_NOFOLLOW,
                    0o600,
                    dir_fd=descriptor,
                )
                temporary = name
                break
            except FileExistsError:
                continue
        if temporary_fd is None or temporary is None:
            raise AppError(
                f"Could not create a temporary BK retrieval file in {parent}"
            )
        with os.fdopen(temporary_fd, "wb") as handle:
            temporary_fd = None
            os.fchmod(handle.fileno(), mode)
            handle.write(payload)
            handle.flush()
            os.fsync(handle.fileno())
        _, named_parent = _safe_key_directory(create=False)
        pinned_parent = os.fstat(descriptor)
        if named_parent is None or (named_parent.st_dev, named_parent.st_ino) != (
            pinned_parent.st_dev, pinned_parent.st_ino
        ):
            raise AppError("BK retrieval directory changed during reconciliation")
        latest = _owned_file_info(path, label="BK retrieval key")
        if _file_identity(latest) != _file_identity(current):
            raise AppError(f"BK retrieval key changed during reconciliation: {path}")
        os.replace(
            temporary,
            path.name,
            src_dir_fd=descriptor,
            dst_dir_fd=descriptor,
        )
        temporary = None
        os.fsync(descriptor)
    except OSError as exc:
        raise _local_error(str(path), exc) from exc
    finally:
        if temporary_fd is not None:
            os.close(temporary_fd)
        if temporary is not None:
            try:
                os.unlink(temporary, dir_fd=descriptor)
            except OSError:
                pass
        os.close(descriptor)


def _generate_pair(role: str, vmid: int, directory: Path, pair: dict) -> None:
    private, public = retrieval_key_paths(vmid)[role]
    comment = _canonical_comment(role, vmid)
    temporary_directory = Path(tempfile.mkdtemp(prefix=".bk-keygen-", dir=directory))
    try:
        os.chmod(temporary_directory, 0o700)
        temporary_private = temporary_directory / private.name
        result = subprocess.run(
            [
                "ssh-keygen",
                "-q",
                "-t",
                "ed25519",
                "-N",
                "",
                "-C",
                comment,
                "-f",
                str(temporary_private),
            ],
            stdin=subprocess.DEVNULL,
            stdout=subprocess.PIPE,
            stderr=subprocess.PIPE,
            text=True,
            check=False,
        )
        if result.returncode:
            detail = (result.stderr or result.stdout or "").strip()
            raise AppError(
                f"Could not generate BK retrieval {role} key"
                + (f": {detail.splitlines()[-1]}" if detail else "")
            )
        private_payload, private_info = _read_owned_file(
            temporary_private, label=f"generated BK {role} key"
        )
        identity = _run_ssh_keygen(temporary_private)
        if identity[2] != comment:
            raise AppError(f"Generated BK retrieval {role} key has an unexpected comment")
        public_payload = (_canonical_public(identity, role, vmid) + "\n").encode()
        # Publish the public half first. If interrupted, the next reconciliation
        # sees an orphan public key and safely replaces the pair.
        _atomic_local_write(public, public_payload, mode=0o644, replace=True)
        _atomic_local_write(private, private_payload, mode=0o600, replace=False)
        del private_info
    finally:
        try:
            shutil.rmtree(temporary_directory)
        except OSError:
            pass


def _cleanup_pair(role: str, vmid: int, pair: dict) -> None:
    """Verify that existing files are HomeStack-generated before destroying them."""
    private, public = retrieval_key_paths(vmid)[role]
    private_info = _owned_file_info(private, label=f"private BK {role} key")
    public_info = _owned_file_info(public, label=f"public BK {role} key")
    if private_info is None and public_info is None:
        return
    comment = _canonical_comment(role, vmid)
    if private_info is not None:
        private_payload, _ = _read_owned_file(
            private, label=f"private BK {role} key"
        )
        identity = _derive_identity(
            private, private_payload, stat.S_IMODE(private_info.st_mode)
        )
        if identity[2] != comment:
            raise AppError(
                f"Refusing to remove unrecognized BK retrieval {role} private key: {private}"
            )
        if public_info is not None:
            public_payload, _ = _read_owned_file(
                public, label=f"public BK {role} key"
            )
            public_identity = _parse_public_identity(
                public_payload, label=f"Public BK {role} key"
            )
            if identity[:2] != public_identity[:2]:
                raise AppError(
                    f"BK retrieval {role} private/public key mismatch; refusing cleanup"
                )
    elif public_info is not None:
        public_payload, _ = _read_owned_file(
            public, label=f"public BK {role} key"
        )
        public_identity = _parse_public_identity(
            public_payload, label=f"Public BK {role} key"
        )
        if public_identity[2] != comment:
            raise AppError(
                f"Refusing to remove unrecognized BK retrieval {role} public key: {public}"
            )


def cleanup_retrieval_keys(vmid: int) -> list[Path]:
    """Remove exactly this VMID's owned BK retrieval files after destruction."""
    vmid = _validate_vmid(vmid)
    directory, info = _safe_key_directory(create=False)
    if info is None:
        return []
    with _locked_key_directory(vmid, create=False) as locked:
        if locked is None:
            return []
        directory, descriptor = locked
        original = {
            path: _owned_file_info(path, label="BK retrieval cleanup key")
            for paths in retrieval_key_paths(vmid).values() for path in paths
        }
        pairs = {
            role: _inspect_pair(role, vmid, directory)
            for role in RETRIEVAL_ROLES
        }
        for role, pair in pairs.items():
            _cleanup_pair(role, vmid, pair)
        removed: list[Path] = []
        for role in RETRIEVAL_ROLES:
            for path in retrieval_key_paths(vmid)[role]:
                try:
                    current = os.stat(
                        path.name, dir_fd=descriptor, follow_symlinks=False
                    )
                except FileNotFoundError:
                    continue
                except OSError as exc:
                    raise _local_error(str(path), exc) from exc
                if _file_identity(current) != _file_identity(original[path]):
                    raise AppError(f"BK retrieval key changed during cleanup: {path}")
                if (
                    stat.S_ISLNK(current.st_mode)
                    or not stat.S_ISREG(current.st_mode)
                    or current.st_uid != os.getuid()
                    or current.st_gid != os.getgid()
                    or current.st_nlink != 1
                ):
                    raise AppError(
                        f"Unsafe BK retrieval key during cleanup: {path}"
                    )
                try:
                    os.unlink(path.name, dir_fd=descriptor)
                    os.fsync(descriptor)
                except OSError as exc:
                    raise _local_error(str(path), exc) from exc
                removed.append(path)
        return removed


def reconcile_retrieval_keys(vmid: int) -> dict:
    """Create or repair both local BK key pairs, preserving valid key bytes."""
    vmid = _validate_vmid(vmid)
    # This read-only pass ensures a mismatch is rejected before creating a
    # directory, changing a mode, or replacing any other pair.
    inspect_retrieval_keys(vmid)
    with _locked_key_directory(vmid, create=True) as locked:
        if locked is None:
            raise AppError("BK retrieval key directory could not be created")
        directory, descriptor = locked
        current = inspect_retrieval_keys(vmid)
        os.fchmod(descriptor, 0o700)
        for role, pair in current["pairs"].items():
            if pair["state"] == "valid":
                private = Path(pair["private_path"])
                public = Path(pair["public_path"])
                if pair["private_mode"] != 0o600:
                    os.chmod(private, 0o600, follow_symlinks=False)
                if pair["public_mode"] != 0o644:
                    os.chmod(public, 0o644, follow_symlinks=False)
            elif pair["private_exists"] and not pair["public_exists"]:
                private = Path(pair["private_path"])
                payload, info = _read_owned_file(
                    private, label=f"private BK {role} key"
                )
                identity = _derive_identity(private, payload, stat.S_IMODE(info.st_mode))
                if pair["private_mode"] != 0o600:
                    os.chmod(private, 0o600, follow_symlinks=False)
                _atomic_local_write(
                    Path(pair["public_path"]),
                    (_canonical_public(identity, role, vmid) + "\n").encode(),
                    mode=0o644,
                    replace=False,
                )
            else:
                _generate_pair(role, vmid, directory, pair)
        final = inspect_retrieval_keys(vmid)
    if not final["ready"]:
        raise AppError(
            "BK retrieval key reconciliation did not produce two valid key pairs"
        )
    return final


def _guest_authorized_keys(
    ws,
    cfg,
    vmid: int,
    public_keys: dict[str, str],
    *,
    apply: bool,
    expected_sha256: str | None = None,
) -> dict:
    from .setup import guest

    keys = {role: str(public_keys.get(role) or "") for role in RETRIEVAL_ROLES}
    if not all(keys.values()):
        keys = {}
    return guest(
        ws,
        cfg,
        "backup-authorized-keys",
        vmid=_validate_vmid(vmid),
        keys=keys,
        apply=apply,
        expected_sha256=expected_sha256,
    )


def _command(ws, cfg, command: str):
    from .setup import command_environment

    return ws.run(
        command_environment(cfg, command, interpreter="sh", pipefail=False),
        check=False,
    )


def _systemctl(ws, cfg, arguments: str):
    return _command(
        ws,
        cfg,
        f"env XDG_RUNTIME_DIR=/run/user/{cfg.user_uid} systemctl --user {arguments}",
    )


def _require_runtime(ws, cfg) -> None:
    from .setup import require_tool

    for tool in ("python3", "file", "git", "systemctl"):
        require_tool(ws, cfg, tool)
    modules = _command(ws, cfg, "python3 -c 'import curses, rich, sqlite3'")
    if modules.returncode:
        raise AppError(
            "BK requires Python modules curses, rich and sqlite3; prepare Gold and refresh separately"
        )
    user_systemd = _systemctl(ws, cfg, "show-environment")
    if user_systemd.returncode:
        raise AppError(
            "The workspace user systemd manager is unavailable; prepare the guest session separately"
        )


def _timer_state(ws, cfg) -> tuple[bool, bool]:
    enabled = _systemctl(ws, cfg, "is-enabled backup.timer").returncode == 0
    active = _systemctl(ws, cfg, "is-active backup.timer").returncode == 0
    return enabled, active


def inspect(ws, cfg, vmid: int, *, check_requirements: bool = True) -> dict:
    """Inspect managed BK assets and the user timer without reading BK user data."""
    from .setup import guest

    if check_requirements:
        _require_runtime(ws, cfg)
        if shutil.which("ssh-keygen") is None:
            raise AppError("BK retrieval requires desktop ssh-keygen")
        if _command(ws, cfg, "test -x /usr/bin/cat").returncode:
            raise AppError("BK retrieval requires guest /usr/bin/cat; prepare Gold separately")
    retrieval = inspect_retrieval_keys(vmid)
    authorization = _guest_authorized_keys(
        ws, cfg, vmid, retrieval["public_keys"], apply=False,
    )
    guest(
        ws,
        cfg,
        "paths",
        paths=[
            {"relative": path, "directory": True}
            for path in MANAGED_DIRECTORIES
        ],
    )
    actual = guest(ws, cfg, "managed-files-inspect", paths=list(managed_paths()))
    files = actual.get("items", [])
    by_path = {item["path"]: item for item in files}
    desired = desired_hashes()
    modes = {asset.relative_path: asset.mode for asset in MANAGED_ASSETS}
    changed = [
        path
        for path in managed_paths()
        if not by_path.get(path, {}).get("exists")
        or by_path[path].get("sha256") != desired[path]
        or by_path[path].get("mode") != modes[path]
    ]
    snapshot_paths = [
        path for path in changed if by_path.get(path, {}).get("exists")
    ]
    if authorization.get("exists") and not authorization.get("ready"):
        snapshot_paths.append(AUTHORIZED_KEYS_PATH)
    timer_enabled, timer_active = _timer_state(ws, cfg)
    any_installed = any(item.get("exists") for item in files)
    ready = (not changed and timer_enabled and timer_active
             and retrieval["ready"] and authorization.get("ready", False))
    state = (
        "configured"
        if ready
        else "not installed"
        if not any_installed
        else "needs update"
    )
    return {
        "state": state,
        "ready": ready,
        "exists": any_installed,
        "will_overwrite": bool(snapshot_paths),
        "files": files,
        "managed_paths": list(managed_state_paths()),
        "changed_paths": changed,
        "snapshot_paths": snapshot_paths,
        "timer_enabled": timer_enabled,
        "timer_active": timer_active,
        "retrieval": retrieval,
        "authorization": authorization,
    }


def preflight(ws, cfg, vmid: int) -> dict:
    return inspect(ws, cfg, vmid, check_requirements=True)


def apply(ws, cfg, vmid: int, state: dict, *, activity=lambda message: None) -> tuple[str, str]:
    """Atomically reconcile HomeStack-owned assets and enable the user timer."""
    from .setup import guest

    if state.get("ready"):
        return "already-ready", "BK assets, user timer and restricted retrieval keys already match"

    activity("Reconcile desktop BK retrieval keys")
    retrieval = reconcile_retrieval_keys(vmid)
    activity("Install restricted BK public keys")
    _guest_authorized_keys(
        ws, cfg, vmid, retrieval["public_keys"], apply=True,
        expected_sha256=state["authorization"].get("sha256"),
    )

    contents = load_assets()
    by_path = {item["path"]: item for item in state.get("files", [])}
    changed = set(state.get("changed_paths", managed_paths()))
    files = []
    for asset in MANAGED_ASSETS:
        if asset.relative_path not in changed:
            continue
        payload = contents[asset.relative_path]
        files.append(
            {
                "path": asset.relative_path,
                "content": base64.b64encode(payload).decode("ascii"),
                "sha256": hashlib.sha256(payload).hexdigest(),
                "expected_sha256": by_path.get(asset.relative_path, {}).get("sha256"),
                "mode": asset.mode,
            }
        )

    activity("Install or update managed BK assets")
    guest(
        ws,
        cfg,
        "managed-files-install",
        directories=list(MANAGED_DIRECTORIES),
        files=files,
    )
    unit_paths = {
        ".config/systemd/user/backup.service",
        ".config/systemd/user/backup.timer",
    }
    if changed & unit_paths:
        activity("Reload the user systemd manager")
        if _systemctl(ws, cfg, "daemon-reload").returncode:
            raise AppError("User systemd daemon-reload failed")

    activity("Enable and start backup.timer")
    if _systemctl(ws, cfg, "enable --now backup.timer").returncode:
        raise AppError("Could not enable and start backup.timer")
    enabled, active = _timer_state(ws, cfg)
    if not enabled or not active:
        raise AppError("backup.timer did not become enabled and active")

    verified = inspect(ws, cfg, vmid, check_requirements=False)
    if not verified["ready"]:
        raise AppError("BK installation verification failed")
    action = "installed" if state.get("state") == "not installed" else "updated"
    return "succeeded", f"BK {action}; backup.timer is enabled and active; restricted retrieval keys configured"
