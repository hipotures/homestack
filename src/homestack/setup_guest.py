"""Self-contained guest operations. Executed as the workspace user using Python stdlib.

No module imports from HomeStack: this file is also the packaged remote program.
"""
from __future__ import annotations

import json
import os
import hashlib
import base64
import binascii
import errno
import secrets
import shutil
from datetime import datetime, timezone
from pathlib import Path
import stat
import subprocess
import tempfile
import time


class GuestError(Exception):
    pass


STATE_DIR = ".local/state/homestack"
STATE_FILE = STATE_DIR + "/setup.json"


def iso_now() -> str:
    return datetime.now(timezone.utc).isoformat(timespec="seconds").replace("+00:00", "Z")


def _state_root(home: Path, *, create: bool = False) -> Path:
    root = safe_path(home, STATE_DIR, directory=True)
    if create:
        root.mkdir(mode=0o700, parents=True, exist_ok=True)
        os.chmod(root, 0o700)
    return root


def _atomic_bytes(path: Path, payload: bytes, *, mode: int = 0o600) -> None:
    path.parent.mkdir(mode=0o700, parents=True, exist_ok=True)
    fd, temporary = tempfile.mkstemp(prefix=".homestack-", dir=path.parent)
    try:
        with os.fdopen(fd, "wb") as handle:
            os.fchmod(handle.fileno(), mode)
            handle.write(payload)
            handle.flush()
            os.fsync(handle.fileno())
        os.replace(temporary, path)
        directory = os.open(path.parent, os.O_RDONLY)
        try:
            os.fsync(directory)
        finally:
            os.close(directory)
    finally:
        Path(temporary).unlink(missing_ok=True)


def _relative_parts(relative: str) -> list[str]:
    parts = relative.split("/")
    if not relative or any(part in {"", ".", ".."} for part in parts):
        raise GuestError("Unsafe path below persistent home")
    return parts


def _check_directory_fd(descriptor: int, label: str) -> None:
    try:
        info = os.fstat(descriptor)
    except OSError as exc:
        raise GuestError(f"{label} cannot be inspected") from exc
    if not stat.S_ISDIR(info.st_mode):
        raise GuestError(f"{label} is not a directory")
    if info.st_uid != os.getuid() or info.st_gid != os.getgid():
        raise GuestError(f"{label} ownership conflict")
    if not os.access(f"/proc/self/fd/{descriptor}", os.W_OK | os.X_OK):
        raise GuestError(f"{label} is not writable")


def _open_home_fd(home: Path) -> int:
    flags = os.O_RDONLY | os.O_DIRECTORY | os.O_NOFOLLOW
    try:
        descriptor = os.open(home, flags)
    except FileNotFoundError as exc:
        raise GuestError("Persistent home is missing") from exc
    except OSError as exc:
        if getattr(exc, "errno", None) == errno.ELOOP:
            raise GuestError("Persistent home is a symlink") from exc
        raise GuestError("Persistent home is not a directory") from exc
    try:
        _check_directory_fd(descriptor, "Persistent home")
    except Exception:
        os.close(descriptor)
        raise
    return descriptor


def _open_directory_chain(root_fd: int, parts: list[str], *, create: bool = False,
                          label: str = "Destination ancestor") -> int | None:
    current = os.dup(root_fd)
    try:
        for index, part in enumerate(parts):
            flags = os.O_RDONLY | os.O_DIRECTORY | os.O_NOFOLLOW
            try:
                child = os.open(part, flags, dir_fd=current)
            except FileNotFoundError:
                if not create:
                    os.close(current)
                    return None
                try:
                    os.mkdir(part, mode=0o700, dir_fd=current)
                except FileExistsError:
                    pass
                try:
                    child = os.open(part, flags, dir_fd=current)
                except OSError as exc:
                    if getattr(exc, "errno", None) == errno.ELOOP:
                        raise GuestError(f"{label} is a symlink") from exc
                    raise GuestError(f"{label} cannot be opened") from exc
            except OSError as exc:
                if getattr(exc, "errno", None) == errno.ELOOP:
                    raise GuestError(f"{label} is a symlink") from exc
                if getattr(exc, "errno", None) == errno.ENOTDIR:
                    raise GuestError(f"{label} is not a directory") from exc
                raise GuestError(f"{label} cannot be opened") from exc
            try:
                _check_directory_fd(child, f"{label} at component {index + 1}")
            except Exception:
                os.close(child)
                raise
            os.close(current)
            current = child
        return current
    except Exception:
        try:
            os.close(current)
        except OSError:
            pass
        raise


class _PinnedFile:
    def __init__(self, parent_fd: int | None, leaf: str):
        self.parent_fd = parent_fd
        self.leaf = leaf

    def close(self) -> None:
        if self.parent_fd is not None:
            os.close(self.parent_fd)
            self.parent_fd = None

    def __enter__(self) -> "_PinnedFile":
        return self

    def __exit__(self, *_: object) -> None:
        self.close()


def _open_pinned_from_root(root_fd: int, relative: str, *, create_parents: bool = False) -> _PinnedFile:
    parts = _relative_parts(relative)
    parent_fd = _open_directory_chain(root_fd, parts[:-1], create=create_parents)
    return _PinnedFile(parent_fd, parts[-1])


def _open_pinned_file(home: Path, relative: str, *, create_parents: bool = False) -> _PinnedFile:
    root_fd = _open_home_fd(home)
    try:
        return _open_pinned_from_root(root_fd, relative, create_parents=create_parents)
    finally:
        os.close(root_fd)


def _read_pinned_file(target: _PinnedFile, label: str = "Structured configuration") -> tuple[bytes | None, str | None, int | None]:
    if target.parent_fd is None:
        return None, None, None
    flags = os.O_RDONLY | os.O_NOFOLLOW | os.O_NONBLOCK
    try:
        descriptor = os.open(target.leaf, flags, dir_fd=target.parent_fd)
    except FileNotFoundError:
        return None, None, None
    except OSError as exc:
        if getattr(exc, "errno", None) == errno.ELOOP:
            raise GuestError(f"{label} path is a symlink") from exc
        raise GuestError(f"{label} cannot be read") from exc
    handle = None
    try:
        info = os.fstat(descriptor)
        if stat.S_ISLNK(info.st_mode):
            raise GuestError(f"{label} path is a symlink")
        if not stat.S_ISREG(info.st_mode):
            raise GuestError(f"{label} path is not a regular file")
        if info.st_uid != os.getuid() or info.st_gid != os.getgid():
            raise GuestError(f"{label} ownership conflict")
        handle = os.fdopen(descriptor, "rb")
        descriptor = None
        with handle:
            content = handle.read()
    except GuestError:
        raise
    except OSError as exc:
        raise GuestError(f"{label} cannot be read") from exc
    finally:
        if descriptor is not None:
            try:
                os.close(descriptor)
            except OSError:
                pass
    return content, hashlib.sha256(content).hexdigest(), stat.S_IMODE(info.st_mode)


def _assert_pinned_leaf(target: _PinnedFile, label: str) -> None:
    if target.parent_fd is None:
        raise GuestError(f"{label} parent is missing")
    try:
        info = os.stat(target.leaf, dir_fd=target.parent_fd, follow_symlinks=False)
    except FileNotFoundError:
        return
    except OSError as exc:
        raise GuestError(f"{label} cannot be inspected") from exc
    if stat.S_ISLNK(info.st_mode):
        raise GuestError(f"{label} path is a symlink")
    if not stat.S_ISREG(info.st_mode):
        raise GuestError(f"{label} path is not a regular file")
    if info.st_uid != os.getuid() or info.st_gid != os.getgid():
        raise GuestError(f"{label} ownership conflict")


def _atomic_bytes_pinned(target: _PinnedFile, payload: bytes, *, mode: int,
                         before_replace=None, create_only: bool = False) -> bool:
    if target.parent_fd is None:
        raise GuestError("Structured configuration parent is missing")
    flags = os.O_WRONLY | os.O_CREAT | os.O_EXCL | os.O_NOFOLLOW
    descriptor = None
    temporary = None
    for _ in range(32):
        candidate = ".homestack-" + secrets.token_hex(12)
        try:
            descriptor = os.open(candidate, flags, mode=0o600, dir_fd=target.parent_fd)
            temporary = candidate
            break
        except FileExistsError:
            continue
        except OSError as exc:
            raise GuestError("Structured configuration temporary file could not be created") from exc
    if descriptor is None or temporary is None:
        raise GuestError("Structured configuration temporary file could not be created")
    try:
        with os.fdopen(descriptor, "wb") as handle:
            descriptor = None
            os.fchmod(handle.fileno(), mode)
            handle.write(payload)
            handle.flush()
            os.fsync(handle.fileno())
        _assert_pinned_leaf(target, "Structured configuration")
        if before_replace is not None:
            before_replace()
        if create_only:
            try:
                os.link(temporary, target.leaf, src_dir_fd=target.parent_fd,
                        dst_dir_fd=target.parent_fd, follow_symlinks=False)
            except FileExistsError:
                return False
            os.unlink(temporary, dir_fd=target.parent_fd)
        else:
            os.replace(temporary, target.leaf, src_dir_fd=target.parent_fd, dst_dir_fd=target.parent_fd)
        temporary = None
        os.fsync(target.parent_fd)
        return True
    except OSError as exc:
        raise GuestError("Structured configuration could not be written") from exc
    finally:
        if descriptor is not None:
            os.close(descriptor)
        if temporary is not None:
            try:
                os.unlink(temporary, dir_fd=target.parent_fd)
            except FileNotFoundError:
                pass
            except OSError:
                pass


_BACKUP_KEY_LABELS = ("archive", "status")
_BACKUP_KEY_PATHS = {
    "archive": "backup/backup.tgz",
    "status": "backup/status.json",
}


def _validate_backup_vmid(value: object) -> int:
    if not isinstance(value, int) or isinstance(value, bool) or value < 1:
        raise GuestError("Backup VMID must be a positive integer")
    return value


def _backup_home_literal(home: Path) -> str:
    """Return a home path safe to interpolate into an unescaped SSH command.

    The forced command is deliberately a literal, rather than a shell-quoted
    value.  Restricting every component to a small portable set prevents
    whitespace, quoting, and shell expansion syntax from changing its meaning
    when sshd invokes the command through the user's shell.
    """

    literal = os.fspath(home)
    if not isinstance(literal, str) or not literal.startswith("/"):
        raise GuestError("Persistent home is not safe for a backup SSH command")
    if literal == "/" or literal != os.path.normpath(literal):
        raise GuestError("Persistent home is not safe for a backup SSH command")
    components = literal.split("/")[1:]
    if not components or any(
        not component
        or component in {".", ".."}
        or any(character not in "abcdefghijklmnopqrstuvwxyzABCDEFGHIJKLMNOPQRSTUVWXYZ0123456789._-" for character in component)
        for component in components
    ):
        raise GuestError("Persistent home is not safe for a backup SSH command")
    return literal


def _decode_backup_key_blob(blob: object, label: str) -> str:
    if not isinstance(blob, str) or not blob or len(blob) > 8192:
        raise GuestError(f"Backup {label} public key is invalid")
    if any(character not in "ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789+/=" for character in blob):
        raise GuestError(f"Backup {label} public key is invalid")
    if len(blob) % 4 == 1:
        raise GuestError(f"Backup {label} public key is invalid")
    try:
        # OpenSSH accepts unpadded base64 in public-key files.  Decode with
        # validation enabled after supplying only the omitted padding.
        base64.b64decode(blob + "=" * ((4 - len(blob) % 4) % 4), validate=True)
    except (binascii.Error, ValueError) as exc:
        raise GuestError(f"Backup {label} public key is invalid") from exc
    return blob


def _validate_backup_public_key(value: object, label: str) -> str:
    if not isinstance(value, str):
        raise GuestError(f"Backup {label} public key is invalid")
    line = value.strip()
    if not line or any(ord(character) < 0x20 or ord(character) == 0x7F for character in line):
        raise GuestError(f"Backup {label} public key is invalid")
    fields = line.split()
    if len(fields) < 2 or fields[0] != "ssh-ed25519":
        raise GuestError(f"Backup {label} public key is invalid")
    blob = _decode_backup_key_blob(fields[1], label)
    # The supplied comment is intentionally ignored.  ssh-keygen commonly
    # appends a host-specific comment; this operation always writes its own
    # stable label and never copies that comment into the authorized_keys
    # options or payload.
    return blob


def _validate_backup_keys(value: object, *, require: bool) -> dict[str, str]:
    if not isinstance(value, dict):
        raise GuestError("Backup public keys must be a mapping")
    if not value:
        if require:
            raise GuestError("Both backup public keys are required when applying")
        return {}
    if set(value) != set(_BACKUP_KEY_LABELS):
        raise GuestError("Backup public keys must contain archive and status")
    return {
        label: _validate_backup_public_key(value[label], label)
        for label in _BACKUP_KEY_LABELS
    }


def _backup_expected_digest(value: object) -> str | None:
    return _validate_expected_digest(value, allow_none=True)


def _backup_canonical_lines(home: str, vmid: int, keys: dict[str, str]) -> dict[str, bytes]:
    comments = {
        "archive": f"homestack-bk-archive-vm{vmid}",
        "status": f"homestack-bk-status-vm{vmid}",
    }
    lines: dict[str, bytes] = {}
    for label in _BACKUP_KEY_LABELS:
        command = f'/usr/bin/cat -- {home}/{_BACKUP_KEY_PATHS[label]}'
        lines[label] = (
            f'restrict,command="{command}" ssh-ed25519 '
            f'{keys[label]} {comments[label]}'
        ).encode("ascii")
    return lines


def _backup_line_body(line: bytes) -> bytes:
    if line.endswith(b"\r\n"):
        return line[:-2]
    if line.endswith(b"\n") or line.endswith(b"\r"):
        return line[:-1]
    return line


def _backup_next_field(line: bytes, offset: int) -> tuple[bytes, int] | None:
    length = len(line)
    while offset < length and line[offset] in b" \t":
        offset += 1
    if offset == length:
        return None
    start = offset
    quote: int | None = None
    escaped = False
    while offset < length:
        character = line[offset]
        if quote is not None:
            if escaped:
                escaped = False
            elif character == ord("\\"):
                escaped = True
            elif character == quote:
                quote = None
        else:
            if character in b" \t":
                break
            if character in (ord('"'), ord("'")):
                quote = character
            elif character == ord("\\"):
                escaped = True
        offset += 1
    if quote is not None or escaped:
        return None
    return line[start:offset], offset


def _backup_actual_entry(line: bytes) -> tuple[bytes, bytes, bytes] | None:
    """Parse enough authorized_keys syntax to identify a managed entry.

    Return ``(options, blob, comment)`` for an actual ed25519 entry.  The
    options field is empty for an entry without options.  Malformed or
    unrelated lines return ``None`` so their original bytes remain untouched.
    """

    body = _backup_line_body(line)
    if not body.strip(b" \t") or body.lstrip(b" \t").startswith(b"#"):
        return None
    first = _backup_next_field(body, 0)
    if first is None:
        return None
    first_field, cursor = first
    options = b""
    if first_field.startswith((b"ssh-", b"ecdsa-", b"sk-")):
        key_type = first_field
    else:
        options = first_field
        second = _backup_next_field(body, cursor)
        if second is None:
            return None
        key_type, cursor = second
        if not key_type.startswith((b"ssh-", b"ecdsa-", b"sk-")):
            return None
    key = _backup_next_field(body, cursor)
    if key is None:
        return None
    blob, cursor = key
    try:
        blob_text = blob.decode("ascii")
        _decode_backup_key_blob(blob_text, "authorized_keys")
    except (UnicodeDecodeError, GuestError):
        return None
    while cursor < len(body) and body[cursor] in b" \t":
        cursor += 1
    return options, blob, body[cursor:]


def _backup_managed_comment(vmid: int, label: str) -> bytes:
    return f"homestack-bk-{label}-vm{vmid}".encode("ascii")


def _backup_canonical_content(content: bytes | None, lines: dict[str, bytes], vmid: int) -> tuple[bytes, list[tuple[bytes, bytes, bytes]]]:
    """Remove this VM's entries and append one canonical archive/status pair."""

    unrelated: list[bytes] = []
    managed: list[tuple[bytes, bytes, bytes]] = []
    if content:
        for raw_line in content.splitlines(keepends=True):
            parsed = _backup_actual_entry(raw_line)
            if parsed is None:
                unrelated.append(raw_line)
                continue
            options, blob, comment = parsed
            if comment in {
                _backup_managed_comment(vmid, "archive"),
                _backup_managed_comment(vmid, "status"),
            }:
                managed.append(parsed)
            else:
                unrelated.append(raw_line)
    prefix = b"".join(unrelated)
    canonical = lines["archive"] + b"\n" + lines["status"] + b"\n"
    if prefix and not prefix.endswith((b"\n", b"\r")):
        prefix += b"\n"
    return prefix + canonical, managed


def _backup_ready(content: bytes | None, *, ssh_mode: int | None, file_mode: int | None,
                  lines: dict[str, bytes], vmid: int,
                  expected_blobs: dict[str, str] | None = None) -> bool:
    if content is None or ssh_mode is None or file_mode is None:
        return False
    if ssh_mode & 0o077 or file_mode & 0o077:
        return False
    expected_by_comment = {
        _backup_managed_comment(vmid, "archive"): ("archive", lines["archive"]),
        _backup_managed_comment(vmid, "status"): ("status", lines["status"]),
    }
    found: list[bytes] = []
    for raw_line in content.splitlines(keepends=True):
        parsed = _backup_actual_entry(raw_line)
        if parsed is None:
            continue
        options, blob, comment = parsed
        if comment not in expected_by_comment:
            continue
        label, expected = expected_by_comment[comment]
        if expected_blobs is not None and _backup_line_body(raw_line) != expected:
            return False
        # Empty lines mapping means the caller intentionally requested a
        # structural inspection.  In that mode validate the key blob and the
        # exact command/comment, but do not require a local key value.
        expected_parsed = _backup_actual_entry(expected + b"\n")
        if expected_parsed is None:
            return False
        expected_options, expected_blob, expected_comment = expected_parsed
        if expected_comment != comment:
            return False
        if options != expected_options:
            return False
        if expected_blobs is not None:
            if blob != expected_blobs[label].encode("ascii"):
                return False
        found.append(comment)
    return sorted(found) == sorted([
        _backup_managed_comment(vmid, "archive"),
        _backup_managed_comment(vmid, "status"),
    ])


def _backup_stat_matches(left: os.stat_result, right: os.stat_result) -> bool:
    return left.st_dev == right.st_dev and left.st_ino == right.st_ino


def _assert_backup_parent(home: Path, home_fd: int, ssh_fd: int, target: _PinnedFile) -> None:
    """Ensure the names still resolve to the pinned descriptors.

    A pinned descriptor prevents writes from following a replacement symlink,
    but by itself it could write into a directory that has since been detached
    from ``home/.ssh``.  Rechecking the names immediately before publication
    makes that substitution fail closed.
    """

    try:
        named_home = os.stat(home, follow_symlinks=False)
        pinned_home = os.fstat(home_fd)
    except OSError as exc:
        raise GuestError("Backup SSH parent could not be revalidated") from exc
    if not stat.S_ISDIR(named_home.st_mode) or not _backup_stat_matches(named_home, pinned_home):
        raise GuestError("Backup SSH parent changed during preparation")
    try:
        named_ssh = os.stat(".ssh", dir_fd=home_fd, follow_symlinks=False)
        pinned_ssh = os.fstat(ssh_fd)
        target_parent = os.fstat(target.parent_fd) if target.parent_fd is not None else None
    except OSError as exc:
        raise GuestError("Backup SSH directory changed during preparation") from exc
    if (
        not stat.S_ISDIR(named_ssh.st_mode)
        or not _backup_stat_matches(named_ssh, pinned_ssh)
        or target_parent is None
        or not _backup_stat_matches(target_parent, pinned_ssh)
    ):
        raise GuestError("Backup SSH directory changed during preparation")
    if named_ssh.st_uid != os.getuid() or named_ssh.st_gid != os.getgid():
        raise GuestError("Backup SSH directory ownership conflict")
    _assert_pinned_leaf(target, "Backup authorized_keys")


def backup_authorized_keys(home: Path, vmid_value: object, keys_value: object,
                           apply_value: object, expected_value: object) -> dict:
    vmid = _validate_backup_vmid(vmid_value)
    literal_home = _backup_home_literal(home)
    if not isinstance(apply_value, bool):
        raise GuestError("Backup authorized_keys apply flag is invalid")
    keys = _validate_backup_keys(keys_value, require=apply_value)
    expected = _backup_expected_digest(expected_value)

    # For a read-only inspection, empty keys request structural readiness.  A
    # write always has both key payloads, so these placeholders are never used
    # to construct an applied file.
    structural_keys = keys or {"archive": "BLOB", "status": "BLOB"}
    lines = _backup_canonical_lines(literal_home, vmid, structural_keys)

    home_fd = _open_home_fd(home)
    ssh_fd: int | None = None
    target: _PinnedFile | None = None
    try:
        ssh_fd = _open_directory_chain(
            home_fd,
            [".ssh"],
            create=False,
            label="Backup SSH directory",
        )
        current: bytes | None
        current_digest: str | None
        current_file_mode: int | None
        if ssh_fd is None:
            current, current_digest, current_file_mode = None, None, None
            ssh_mode = None
        else:
            target = _PinnedFile(os.dup(ssh_fd), "authorized_keys")
            current, current_digest, current_file_mode = _read_pinned_file(
                target, "Backup authorized_keys"
            )
            ssh_mode = stat.S_IMODE(os.fstat(ssh_fd).st_mode)

        ready = bool(keys) and _backup_ready(
            current,
            ssh_mode=ssh_mode,
            file_mode=current_file_mode,
            lines=lines,
            vmid=vmid,
            expected_blobs=keys or None,
        )
        if not apply_value:
            return {
                "ok": True,
                "exists": current is not None,
                "sha256": current_digest,
                "ready": ready,
                "changed": False,
            }
        if current_digest != expected:
            raise GuestError("Backup authorized_keys CAS conflict: file changed since inspection")
        if ready:
            return {"ok": True, "exists": True, "sha256": current_digest,
                    "ready": True, "changed": False}
        if ssh_fd is None:
            # Only create .ssh after validating a missing-file CAS.  Thus a
            # failed apply does not leave an empty directory behind.
            ssh_fd = _open_directory_chain(
                home_fd,
                [".ssh"],
                create=True,
                label="Backup SSH directory",
            )
            if ssh_fd is None:
                raise GuestError("Backup SSH directory could not be created")
            ssh_mode = 0o700
            target = _PinnedFile(os.dup(ssh_fd), "authorized_keys")
            current, current_digest, current_file_mode = _read_pinned_file(
                target, "Backup authorized_keys"
            )
            if current_digest != expected:
                raise GuestError("Backup authorized_keys CAS conflict: file appeared during preparation")

        # Re-read from the pinned parent before constructing the replacement,
        # matching the existing structured-write CAS behavior.
        if target is None or ssh_fd is None:
            raise GuestError("Backup authorized_keys parent is missing")
        latest, latest_digest, latest_mode = _read_pinned_file(
            target, "Backup authorized_keys"
        )
        if latest_digest != expected:
            raise GuestError("Backup authorized_keys CAS conflict: file changed during preparation")
        current, current_digest, current_file_mode = latest, latest_digest, latest_mode

        desired_lines = _backup_canonical_lines(literal_home, vmid, keys)
        candidate, _managed = _backup_canonical_content(current, desired_lines, vmid)
        current_ssh_mode = stat.S_IMODE(os.fstat(ssh_fd).st_mode)
        desired_ssh_mode = current_ssh_mode & 0o700 if current_ssh_mode & 0o077 else current_ssh_mode
        if desired_ssh_mode == 0:
            # This is still a safer mode than 0700, but cannot support the
            # requested write.  Let the chmod/open operation report the
            # permission failure rather than loosening it.
            desired_ssh_mode = current_ssh_mode
        desired_file_mode = (
            current_file_mode & 0o700
            if current_file_mode is not None and current_file_mode & 0o077
            else current_file_mode
        )
        if desired_file_mode is None:
            desired_file_mode = 0o600
        needs_content = current != candidate
        needs_dir_mode = desired_ssh_mode != current_ssh_mode
        needs_file_mode = current_file_mode != desired_file_mode
        changed = needs_content or needs_dir_mode or needs_file_mode
        if not changed:
            return {
                "ok": True,
                "exists": current is not None,
                "sha256": current_digest,
                "ready": _backup_ready(
                    current,
                    ssh_mode=current_ssh_mode,
                    file_mode=current_file_mode,
                    lines=desired_lines,
                    vmid=vmid,
                    expected_blobs=keys,
                ),
                "changed": False,
            }

        if needs_dir_mode:
            _assert_backup_parent(home, home_fd, ssh_fd, target)
            try:
                os.fchmod(ssh_fd, desired_ssh_mode)
                os.fsync(ssh_fd)
            except OSError as exc:
                raise GuestError("Backup SSH directory permissions could not be tightened") from exc

        if needs_content or needs_file_mode:
            def before_replace() -> None:
                _assert_backup_parent(home, home_fd, ssh_fd, target)
                latest_content, latest_digest, _ = _read_pinned_file(
                    target, "Backup authorized_keys"
                )
                if latest_digest != expected:
                    raise GuestError("Backup authorized_keys CAS conflict: file changed before replacement")
                if expected is None and latest_content is not None:
                    raise GuestError("Backup authorized_keys CAS conflict: file appeared before replacement")

            _atomic_bytes_pinned(
                target,
                candidate,
                mode=desired_file_mode,
                before_replace=before_replace,
            )
        return {
            "ok": True,
            "exists": True,
            "sha256": hashlib.sha256(candidate).hexdigest(),
            "ready": _backup_ready(
                candidate,
                ssh_mode=desired_ssh_mode,
                file_mode=desired_file_mode,
                lines=desired_lines,
                vmid=vmid,
                expected_blobs=keys,
            ),
            "changed": True,
        }
    finally:
        if target is not None:
            target.close()
        if ssh_fd is not None:
            os.close(ssh_fd)
        os.close(home_fd)


def _unlink_pinned_file(target: _PinnedFile) -> None:
    _assert_pinned_leaf(target, "Structured configuration")
    try:
        os.unlink(target.leaf, dir_fd=target.parent_fd)
        os.fsync(target.parent_fd)
    except FileNotFoundError as exc:
        raise GuestError("Structured configuration disappeared during restore") from exc
    except OSError as exc:
        raise GuestError("Structured configuration could not be removed") from exc


def structured_read(home: Path, relative: str) -> dict:
    with _open_pinned_file(home, relative) as target:
        content, digest, mode = _read_pinned_file(target)
    return {
        "ok": True,
        "content": base64.b64encode(content).decode("ascii") if content is not None else None,
        "sha256": digest,
        "mode": mode,
    }


def _decode_structured_content(value: object) -> bytes:
    if not isinstance(value, str):
        raise GuestError("Structured configuration content must be base64 text")
    try:
        return base64.b64decode(value.encode("ascii"), validate=True)
    except (UnicodeEncodeError, binascii.Error, ValueError) as exc:
        raise GuestError("Structured configuration content is not valid base64") from exc


def _validate_expected_digest(value: object, *, allow_none: bool) -> str | None:
    if value is None and allow_none:
        return None
    if not isinstance(value, str) or len(value) != hashlib.sha256().digest_size * 2:
        raise GuestError("Invalid structured configuration SHA-256")
    try:
        int(value, 16)
    except ValueError as exc:
        raise GuestError("Invalid structured configuration SHA-256") from exc
    return value.lower()


def structured_write(home: Path, relative: str, content_value: object, expected_value: object) -> dict:
    payload = _decode_structured_content(content_value)
    expected = _validate_expected_digest(expected_value, allow_none=True)
    with _open_pinned_file(home, relative) as target:
        current, digest, _ = _read_pinned_file(target)
        if digest != expected:
            if expected is None:
                detail = "expected the structured configuration file to be absent"
            else:
                detail = "structured configuration changed since inspection"
            raise GuestError(f"Structured configuration CAS conflict: {detail}")
        if current == payload:
            return {"ok": True, "changed": False}
        if target.parent_fd is not None:
            latest, latest_digest, latest_mode = _read_pinned_file(target)
            if latest_digest != expected:
                raise GuestError("Structured configuration CAS conflict: file changed during preparation")
            _atomic_bytes_pinned(target, payload, mode=latest_mode if latest_mode is not None else 0o600)
            return {"ok": True, "changed": True}

    # Only a previously absent parent chain needs a second path resolution.
    # Existing targets keep their already-pinned parent directory through the
    # final CAS check and atomic replacement.
    with _open_pinned_file(home, relative, create_parents=True) as target:
        latest, latest_digest, latest_mode = _read_pinned_file(target)
        if latest_digest != expected:
            raise GuestError("Structured configuration CAS conflict: file changed during preparation")
        _atomic_bytes_pinned(target, payload, mode=latest_mode if latest_mode is not None else 0o600)
    return {"ok": True, "changed": True}


def inspect_managed_files(home: Path, paths: object) -> dict:
    if not isinstance(paths, list) or any(not isinstance(path, str) for path in paths):
        raise GuestError("Managed setup paths must be a list of text paths")
    items = []
    for relative in dict.fromkeys(paths):
        with _open_pinned_file(home, relative) as target:
            content, digest, mode = _read_pinned_file(target, "Managed setup asset")
        items.append(
            {"path": relative, "exists": False}
            if content is None
            else {
                "path": relative,
                "exists": True,
                "type": "file",
                "sha256": digest,
                "size": len(content),
                "mode": mode,
            }
        )
    return {"ok": True, "items": items}


def install_managed_files(home: Path, directories: object, files: object) -> dict:
    if not isinstance(directories, list) or any(not isinstance(path, str) for path in directories):
        raise GuestError("Managed setup directories must be a list of text paths")
    if not isinstance(files, list):
        raise GuestError("Managed setup files must be a list")

    prepared = []
    for item in files:
        if not isinstance(item, dict) or set(item) != {
            "path", "content", "sha256", "expected_sha256", "mode"
        }:
            raise GuestError("Managed setup file definition is invalid")
        relative = item["path"]
        if not isinstance(relative, str):
            raise GuestError("Managed setup file path must be text")
        _relative_parts(relative)
        payload = _decode_structured_content(item["content"])
        desired = _validate_expected_digest(item["sha256"], allow_none=False)
        if hashlib.sha256(payload).hexdigest() != desired:
            raise GuestError("Managed setup asset does not match its declared SHA-256")
        expected = _validate_expected_digest(item["expected_sha256"], allow_none=True)
        mode = item["mode"]
        if not isinstance(mode, int) or isinstance(mode, bool) or mode < 0 or mode > 0o777:
            raise GuestError("Managed setup file mode is invalid")
        prepared.append((relative, payload, expected, mode))

    home_fd = _open_home_fd(home)
    try:
        for relative in dict.fromkeys(directories):
            descriptor = _open_directory_chain(
                home_fd,
                _relative_parts(relative),
                create=True,
                label="Managed setup directory",
            )
            if descriptor is not None:
                os.close(descriptor)
    finally:
        os.close(home_fd)

    changed = []
    for relative, payload, expected, mode in prepared:
        with _open_pinned_file(home, relative) as target:
            current, digest, current_mode = _read_pinned_file(
                target, "Managed setup asset"
            )
            if digest != expected:
                raise GuestError(f"Managed setup asset changed since preflight: ~/{relative}")
            if current == payload and current_mode == mode:
                continue
            _atomic_bytes_pinned(target, payload, mode=mode)
        changed.append(relative)
    return {"ok": True, "changed": changed}


def _validate_snapshot_id(identifier: object) -> str:
    if not isinstance(identifier, str) or not identifier or "/" in identifier or identifier in {".", ".."}:
        raise GuestError("Invalid operation snapshot")
    return identifier


def _open_snapshot_fd(home: Path, identifier: object) -> int:
    snapshot_id = _validate_snapshot_id(identifier)
    home_fd = _open_home_fd(home)
    state_fd = None
    try:
        state_fd = _open_directory_chain(home_fd, _relative_parts(STATE_DIR), label="Operation snapshot ancestor")
        if state_fd is None:
            raise GuestError("Operation snapshot is missing")
        flags = os.O_RDONLY | os.O_DIRECTORY | os.O_NOFOLLOW
        try:
            snapshot_fd = os.open(snapshot_id, flags, dir_fd=state_fd)
        except FileNotFoundError as exc:
            raise GuestError("Operation snapshot is missing") from exc
        except OSError as exc:
            if getattr(exc, "errno", None) == errno.ELOOP:
                raise GuestError("Operation snapshot is a symlink") from exc
            raise GuestError("Operation snapshot is not a directory") from exc
        try:
            _check_directory_fd(snapshot_fd, "Operation snapshot")
        except Exception:
            os.close(snapshot_fd)
            raise
        return snapshot_fd
    finally:
        if state_fd is not None:
            os.close(state_fd)
        os.close(home_fd)


def _open_snapshot_source(home: Path, identifier: object, relative: str) -> tuple[dict, bytes | None, int | None]:
    snapshot_fd = _open_snapshot_fd(home, identifier)
    try:
        manifest_target = _PinnedFile(os.dup(snapshot_fd), "snapshot.json")
        try:
            manifest_bytes, _, _ = _read_pinned_file(manifest_target, "Operation snapshot manifest")
        finally:
            manifest_target.close()
        if manifest_bytes is None:
            raise GuestError("Operation snapshot manifest is missing")
        try:
            manifest_data = json.loads(manifest_bytes.decode("utf-8"))
        except (UnicodeDecodeError, ValueError, TypeError) as exc:
            raise GuestError("Operation snapshot manifest is invalid") from exc
        if not isinstance(manifest_data, dict) or manifest_data.get("version") != 1:
            raise GuestError("Unsupported operation snapshot")
        files = manifest_data.get("files")
        if not isinstance(files, list):
            raise GuestError("Operation snapshot manifest is invalid")
        entry = next((item for item in files if isinstance(item, dict) and item.get("path") == relative), None)
        if entry is None:
            raise GuestError("Requested path is not part of the operation snapshot")
        if entry.get("exists") is False:
            return entry, None, None
        if entry.get("exists") is not True or entry.get("type") != "file":
            raise GuestError("Operation snapshot manifest is invalid")
        expected_digest = entry.get("sha256")
        if not isinstance(expected_digest, str):
            raise GuestError("Operation snapshot manifest is invalid")
        home_flags = os.O_RDONLY | os.O_DIRECTORY | os.O_NOFOLLOW
        try:
            snapshot_home_fd = os.open("home", home_flags, dir_fd=snapshot_fd)
        except FileNotFoundError as exc:
            raise GuestError("Operation snapshot home is missing") from exc
        except OSError as exc:
            if getattr(exc, "errno", None) == errno.ELOOP:
                raise GuestError("Operation snapshot home is a symlink") from exc
            raise GuestError("Operation snapshot home is not a directory") from exc
        try:
            _check_directory_fd(snapshot_home_fd, "Operation snapshot home")
            source = _open_pinned_from_root(snapshot_home_fd, relative)
        finally:
            os.close(snapshot_home_fd)
        try:
            payload, payload_digest, mode = _read_pinned_file(source, "Operation snapshot payload")
        finally:
            source.close()
        if payload is None:
            raise GuestError("Operation snapshot payload is missing")
        if payload_digest != expected_digest:
            raise GuestError("Operation snapshot payload does not match its manifest")
        return entry, payload, mode
    finally:
        os.close(snapshot_fd)


def structured_restore(home: Path, relative: str, snapshot_value: object,
                       expected_value: object, existed_value: object) -> dict:
    if not isinstance(existed_value, bool):
        raise GuestError("Invalid structured configuration existence flag")
    expected = _validate_expected_digest(expected_value, allow_none=False)
    with _open_pinned_file(home, relative) as target:
        if snapshot_value is None:
            if existed_value:
                raise GuestError("Operation snapshot is required to restore an existing file")
            original = None
            original_mode = None
        else:
            manifest_entry, original, original_mode = _open_snapshot_source(home, snapshot_value, relative)
            if (manifest_entry.get("exists") is True) != existed_value:
                raise GuestError("Operation snapshot presence does not match restore request")

        _, current_digest, _ = _read_pinned_file(target)
        if current_digest != expected:
            raise GuestError("Structured configuration restore CAS conflict")
        if not existed_value:
            _unlink_pinned_file(target)
            return {"ok": True, "changed": True}
        if original is None or original_mode is None:
            raise GuestError("Operation snapshot payload is missing")
        _atomic_bytes_pinned(target, original, mode=original_mode)
        return {"ok": True, "changed": True}


def _digest_path(path: Path) -> tuple[str, int]:
    digest = hashlib.sha256()
    if path.is_file():
        with path.open("rb") as handle:
            for chunk in iter(lambda: handle.read(1024 * 1024), b""):
                digest.update(chunk)
        return digest.hexdigest(), path.stat().st_size
    if not path.is_dir():
        raise GuestError("Managed path is neither a regular file nor a directory")
    total = 0
    for root, directories, files in os.walk(path, followlinks=False):
        directories.sort()
        files.sort()
        base = Path(root)
        for name in directories:
            child = base / name
            if child.is_symlink():
                raise GuestError("Managed directory contains a symlink")
            relative = child.relative_to(path).as_posix().encode()
            digest.update(b"D\0" + relative + b"\0")
        for name in files:
            child = base / name
            info = child.lstat()
            if child.is_symlink() or not stat.S_ISREG(info.st_mode):
                raise GuestError("Managed directory contains a symlink or special file")
            relative = child.relative_to(path).as_posix().encode()
            digest.update(b"F\0" + relative + b"\0")
            with child.open("rb") as handle:
                for chunk in iter(lambda: handle.read(1024 * 1024), b""):
                    digest.update(chunk)
                    total += len(chunk)
    return digest.hexdigest(), total


def _birthtime_ns(path: Path, info: os.stat_result) -> int | None:
    value = getattr(info, "st_birthtime_ns", None)
    if isinstance(value, int) and value > 0:
        return value
    try:
        result = subprocess.run(
            ["stat", "-c", "%W", "--", str(path)],
            capture_output=True,
            text=True,
            check=False,
        )
        seconds = int(result.stdout.strip()) if result.returncode == 0 else 0
    except (OSError, ValueError):
        seconds = 0
    return seconds * 1_000_000_000 if seconds > 0 else None


def path_metadata(home: Path, relative: str) -> dict:
    path = safe_path(home, relative, directory=None)
    if not path.exists():
        return {"path": relative, "exists": False}
    info = path.stat()
    digest, size = _digest_path(path)
    metadata = {
        "path": relative,
        "exists": True,
        "type": "directory" if path.is_dir() else "file",
        "sha256": digest,
        "size": size,
        "mtime_ns": info.st_mtime_ns,
    }
    birthtime_ns = _birthtime_ns(path, info)
    if birthtime_ns is not None:
        metadata["birthtime_ns"] = birthtime_ns
    return metadata


def _registry_path(home: Path) -> Path:
    return safe_path(home, STATE_FILE)


def load_registry(home: Path, *, vmid: int | None = None, name: str | None = None) -> dict | None:
    path = _registry_path(home)
    if path.is_symlink():
        raise GuestError("HomeStack setup state is a symlink")
    if not path.exists():
        return None
    try:
        data = json.loads(path.read_text())
    except (OSError, ValueError) as exc:
        raise GuestError("HomeStack setup state is invalid") from exc
    if not isinstance(data, dict) or data.get("version") != 1 or not isinstance(data.get("items", {}), dict):
        raise GuestError("Unsupported HomeStack setup state")
    workspace = data.get("workspace", {})
    if vmid is not None and workspace and workspace.get("vmid") != vmid:
        raise GuestError("HomeStack setup state belongs to another workspace")
    return data


def write_registry(home: Path, data: dict) -> None:
    root = _state_root(home, create=True)
    payload = (json.dumps(data, indent=2, sort_keys=True) + "\n").encode()
    _atomic_bytes(root / "setup.json", payload)


def create_snapshot(home: Path, paths: list[str], item_ids: list[str], *, vmid: int, name: str) -> dict:
    metadata = []
    existing = []
    for relative in dict.fromkeys(paths):
        item = path_metadata(home, relative)
        metadata.append(item)
        if item["exists"]:
            existing.append(relative)
    registry = load_registry(home, vmid=vmid, name=name)
    if not existing:
        return {"created": False, "path": None, "id": None}
    root = _state_root(home, create=True)
    stem = datetime.now().strftime("%Y%m%d_%H%M%S")
    identifier = stem
    index = 1
    while (root / identifier).exists():
        identifier = f"{stem}-{index:02d}"
        index += 1
    snapshot = root / identifier
    snapshot.mkdir(mode=0o700)
    home_copy = snapshot / "home"
    for relative in existing:
        source = safe_path(home, relative)
        destination = home_copy / relative
        destination.parent.mkdir(mode=0o700, parents=True, exist_ok=True)
        if source.is_dir():
            shutil.copytree(source, destination, symlinks=False)
        else:
            shutil.copy2(source, destination)
    state_path = _registry_path(home)
    if registry is not None:
        shutil.copy2(state_path, snapshot / "setup.before.json")
    manifest = {
        "version": 1,
        "created_at": iso_now(),
        "workspace": {"vmid": vmid, "name": name},
        "items": list(dict.fromkeys(item_ids)),
        "files": metadata,
    }
    _atomic_bytes(snapshot / "snapshot.json", (json.dumps(manifest, indent=2, sort_keys=True) + "\n").encode())
    return {"created": True, "id": identifier, "path": f"~/{STATE_DIR}/{identifier}"}


def record_item(home: Path, data: dict) -> dict:
    vmid, name = int(data["vmid"]), str(data["name"])
    registry = load_registry(home, vmid=vmid, name=name) or {
        "version": 1,
        "workspace": {"vmid": vmid, "name": name, "home": str(home), "home_label": f"HS_HOME_{vmid}"},
        "items": {},
    }
    now = iso_now()
    registry["workspace"] = {
        "vmid": vmid,
        "name": name,
        "home": str(home),
        "home_label": f"HS_HOME_{vmid}",
    }
    if vmid == 0:
        registry["workspace"].pop("home_label")
        registry["workspace"]["kind"] = "local"
    items = registry.setdefault("items", {})
    previous = items.get(data["id"], {}) if isinstance(items.get(data["id"]), dict) else {}
    current = {
        "handler": data["handler"],
        "first_managed_at": previous.get("first_managed_at", now),
        "last_applied_at": now,
    }
    if data.get("snapshot"):
        current["last_snapshot"] = data["snapshot"]
    elif previous.get("last_snapshot"):
        current["last_snapshot"] = previous["last_snapshot"]
    paths = list(dict.fromkeys(data.get("paths", [])))
    if paths:
        current["files"] = [path_metadata(home, relative) for relative in paths]
    if data["handler"] == "application":
        if previous.get("installed_at"):
            current["installed_at"] = previous["installed_at"]
        elif data.get("installed"):
            current["installed_at"] = now
    if data.get("repository"):
        current["repository"] = data["repository"]
    items[data["id"]] = current
    registry["updated_at"] = now
    write_registry(home, registry)
    return current


def safe_path(home: Path, relative: str, *, directory: bool | None = False, recursive: bool = False) -> Path:
    parts = relative.split("/")
    if not relative or any(p in {"", ".", ".."} for p in parts):
        raise GuestError("Unsafe path below persistent home")
    if not os.access(home, os.W_OK | os.X_OK):
        raise GuestError("Persistent home is not writable")
    path = home
    for index, part in enumerate(parts):
        path = path / part
        if path.is_symlink():
            raise GuestError(f"Symlink conflict at ~/{'/'.join(parts[:index + 1])}")
        if path.exists():
            info = path.stat()
            if info.st_uid != os.getuid() or info.st_gid != os.getgid():
                raise GuestError(f"Ownership conflict at ~/{'/'.join(parts[:index + 1])}")
            if path.is_dir() and not os.access(path, os.W_OK | os.X_OK):
                raise GuestError("Destination ancestor is not writable")
            if directory is not None:
                must_dir = index < len(parts) - 1 or directory
                if (must_dir and not path.is_dir()) or (not must_dir and not path.is_file()):
                    raise GuestError(f"File type conflict at ~/{'/'.join(parts[:index + 1])}")
    if recursive and path.is_dir():
        def unreadable(error):
            raise GuestError("Destination tree cannot be inspected") from error
        for root, directories, files in os.walk(path, followlinks=False, onerror=unreadable):
            for name in directories + files:
                child = Path(root) / name
                info = child.lstat()
                if child.is_symlink() or not (stat.S_ISREG(info.st_mode) or stat.S_ISDIR(info.st_mode)):
                    raise GuestError("Destination tree contains a symlink or special file")
                if child.is_dir() and not os.access(child, os.W_OK | os.X_OK):
                    raise GuestError("Destination tree is not writable")
                if info.st_uid != os.getuid() or info.st_gid != os.getgid():
                    raise GuestError("Destination tree contains an ownership conflict")
    return path


def verify_identity(data: dict) -> None:
    home = Path(data["home"])
    if os.getuid() == 0 or os.getuid() != data["uid"] or os.getgid() != data["gid"]:
        raise GuestError("Workspace UID/GID mismatch; refusing writes")
    import pwd
    record = pwd.getpwuid(os.getuid())
    if record.pw_name != data["user"] or record.pw_dir != str(home) or os.environ.get("HOME") != str(home):
        raise GuestError("Workspace account or HOME mismatch; refusing writes")
    hostname = subprocess.run(["hostname", "-s"], capture_output=True, text=True, check=True).stdout.strip()
    if hostname != data["name"]:
        raise GuestError("Workspace hostname mismatch; refusing writes")
    if home.is_symlink() or not home.is_dir() or home.stat().st_uid != data["uid"] or home.stat().st_gid != data["gid"]:
        raise GuestError("Persistent home ownership/type mismatch")
    mount = subprocess.run(["findmnt", "-rn", "-T", str(home), "-o", "TARGET,FSTYPE,LABEL"], capture_output=True, text=True, check=True).stdout.split()
    if mount != [str(home), "ext4", f"HS_HOME_{data['vmid']}"]:
        raise GuestError("Persistent home mount/label mismatch; refusing writes")


def posix_paths(bin_dirs: list[str]) -> str:
    # User-controlled paths are serialized by the caller and quoted as literals.
    import shlex
    lines = []
    for path in reversed(bin_dirs):
        relative = shlex.quote(path[2:])
        lines += [f'_hs_bin="$HOME"/{relative}',
                  'case ":$PATH:" in *":$_hs_bin:"*) ;; *) PATH="$_hs_bin:$PATH" ;; esac']
    return "\n".join(lines + ["export PATH", "unset _hs_bin"])


def environment_updates(home: Path, profile: str, bins: list[str]) -> dict[str, str]:
    import shlex
    paths = posix_paths(bins)
    if profile == "bash":
        bashrc = paths + '''
case $- in *i*)
    if ! declare -F _completion_loader >/dev/null && [ -r /usr/share/bash-completion/bash_completion ]; then
        . /usr/share/bash-completion/bash_completion
    fi
;; esac'''
        login = next((p for p in (".bash_profile", ".bash_login", ".profile") if (home / p).exists() or (home / p).is_symlink()), ".profile")
        safe_path(home, login)
        safe_path(home, ".bashrc")
        login_body = paths + '\nif [ -n "${BASH_VERSION:-}" ] && [ -r "$HOME/.bashrc" ]; then\n    . "$HOME/.bashrc"\nfi'
        return {".bashrc": bashrc.rstrip() + "\n", login: login_body.rstrip() + "\n"}
    if profile == "zsh":
        specs = {".zshenv": paths,
                 ".zshrc": 'if (( ! $+functions[compdef] )); then\n    autoload -Uz compinit\n    compinit -i\nfi'}
    elif profile == "fish":
        body = []
        for path in reversed(bins):
            value = '\"$HOME\"/' + shlex.quote(path[2:])
            body += [f"if not contains -- {value} $PATH", f"    set -gx PATH {value} $PATH", "end"]
        specs = {".config/fish/conf.d/homestack.fish": "\n".join(body)}
    elif profile == "nu":
        dirs = " ".join(f"($env.HOME | path join {json.dumps(p[2:])})" for p in bins)
        specs = {".config/nushell/env.nu": f"$env.PATH = ($env.PATH | prepend [{dirs}] | uniq)",
                 ".config/nushell/config.nu": "$env.config = ($env.config | merge {show_banner: false})"}
    else:
        raise GuestError("Unknown shell profile")
    result = {}
    for relative, body in specs.items():
        safe_path(home, relative)
        result[relative] = body.rstrip() + "\n"
    return result


def atomic_write(path: Path, content: str) -> bool:
    payload = content.encode()
    old = path.read_bytes() if path.exists() else None
    if old == payload:
        return False
    mode = stat.S_IMODE(path.stat().st_mode) & 0o700 if old is not None else 0o600
    _atomic_bytes(path, payload, mode=mode)
    return True


def backup_configuration(home: Path, *, create: bool = False) -> dict:
    """Initialize missing user configuration without replacing an existing file."""
    relative = ".config/bk/backup.yaml"
    with _open_pinned_file(home, relative, create_parents=create) as target:
        current, _, _ = _read_pinned_file(target, "BK configuration")
        if current is not None or not create:
            return {"ok": True, "exists": current is not None, "created": False}
        lines = ["version: 1", f"destination: {json.dumps(str(home / 'backup'))}",
                 "retention: 7", "respect_gitignore: true", "sources:",
                 f"  - {json.dumps(str(home / relative))}"]
        created = _atomic_bytes_pinned(target, ("\n".join(lines) + "\n").encode(),
                                       mode=0o600, create_only=True)
        return {"ok": True, "exists": True, "created": created}


def run(data: dict) -> dict:
    home = Path(data["home"])
    op = data["operation"]
    if op == "backup-config":
        return backup_configuration(home, create=data.get("create") is True)
    if op == "identity":
        verify_identity(data)
        return {"ok": True}
    if op == "state-read":
        registry = load_registry(home, vmid=int(data["vmid"]), name=str(data["name"]))
        return {"ok": True, "registry": registry, "state_path": "~/" + STATE_FILE}
    if op == "metadata":
        return {"ok": True, "items": [path_metadata(home, relative) for relative in data.get("paths", [])]}
    if op == "structured-read":
        return structured_read(home, str(data["relative"]))
    if op == "structured-write":
        return structured_write(home, str(data["relative"]), data.get("content"), data.get("expected_sha256"))
    if op == "structured-restore":
        return structured_restore(home, str(data["relative"]), data.get("snapshot"),
                                  data.get("expected_sha256"), data.get("existed"))
    if op == "managed-files-inspect":
        return inspect_managed_files(home, data.get("paths"))
    if op == "managed-files-install":
        return install_managed_files(
            home, data.get("directories"), data.get("files")
        )
    if op == "backup-authorized-keys":
        return backup_authorized_keys(
            home,
            data.get("vmid"),
            data.get("keys"),
            data.get("apply", False),
            data.get("expected_sha256"),
        )
    if op == "snapshot":
        return {"ok": True, **create_snapshot(home, data.get("paths", []), data.get("items", []),
                                                vmid=int(data["vmid"]), name=str(data["name"]))}
    if op == "state-record":
        return {"ok": True, "item": record_item(home, data), "state_path": "~/" + STATE_FILE}
    if op == "repositories":
        checkout_root = str(data["checkout_root"]).strip("/")
        results = {}
        git = shutil.which("git")
        for repository in data.get("repositories", []):
            owner, repo_name = repository.split("/", 1)
            relative = checkout_root + "/" + repo_name
            try:
                checkout = safe_path(home, relative, directory=True)
                if not checkout.exists():
                    results[repository] = {"state": "absent", "ready": False, "exists": False}
                    continue
                info = checkout.stat()
                meta = {"path": relative, "exists": True, "type": "directory", "mtime_ns": info.st_mtime_ns}
                if not checkout.is_dir() or not (checkout / ".git").is_dir():
                    results[repository] = {"state": "conflict", "ready": False, "exists": True, "metadata": meta}
                    continue
                remote = ""
                core_ssh = ""
                if git:
                    remote = subprocess.run([git, "-C", str(checkout), "remote", "get-url", "origin"], capture_output=True, text=True).stdout.strip()
                    core_ssh = subprocess.run([git, "-C", str(checkout), "config", "--local", "--get", "core.sshCommand"], capture_output=True, text=True).stdout.strip()
                private = home / ".ssh" / "homestack" / "github" / f"{owner}-{repo_name}"
                public = Path(str(private) + ".pub")
                accepted = {
                    f"git@github.com:{repository}.git", f"git@github.com:{repository}",
                    f"https://github.com/{repository}.git", f"https://github.com/{repository}",
                    f"ssh://git@github.com/{repository}.git", f"ssh://git@github.com/{repository}",
                }
                expected_ssh = f"ssh -i {private} -o IdentitiesOnly=yes"
                ready = bool(git and remote in accepted and private.is_file() and public.is_file() and core_ssh == expected_ssh)
                results[repository] = {
                    "state": "configured" if ready else "present",
                    "ready": ready,
                    "exists": True,
                    "metadata": meta,
                    "remote": remote or None,
                    "key_pair": private.is_file() and public.is_file(),
                }
            except GuestError as exc:
                results[repository] = {"state": "conflict", "ready": False, "exists": True, "detail": str(exc)}
        return {"ok": True, "repositories": results}
    if op == "environment":
        updates = environment_updates(home, data["profile"], data["bins"])
        # Check the actual selected shell parser before any persistent write.
        for relative, content in updates.items():
            safe_path(home, relative)
            if data["profile"] != "nu":
                args = [data["profile"], "-n"]
                parsed = subprocess.run(args, input=content, text=True, capture_output=True)
                if parsed.returncode:
                    raise GuestError(f"{data['profile']} startup syntax check failed at ~/{relative}")
            else:
                # Nu's parser is available through nu-check without running user configuration.
                parsed = subprocess.run(["nu", "--no-config-file", "-c", "$in | nu-check"],
                                        input=content, capture_output=True, text=True)
                if parsed.returncode or parsed.stdout.strip() != "true":
                    raise GuestError(f"Nushell startup syntax check failed at ~/{relative}")
        changed = [relative for relative, content in updates.items()
                   if not (home / relative).exists()
                   or (home / relative).read_text() != content]
        if data.get("apply"):
            for relative in changed:
                atomic_write(safe_path(home, relative), updates[relative])
        return {"ok": True, "paths": list(updates), "changed": changed}
    if op == "paths":
        for item in data["paths"]:
            safe_path(home, item["relative"], directory=item.get("directory", False), recursive=item.get("recursive", False))
        return {"ok": True}
    if op == "file":
        path = safe_path(home, data["relative"], directory=data["directory"], recursive=data["directory"])
        if data.get("prepare"):
            (path if data["directory"] else path.parent).mkdir(mode=0o700, parents=True, exist_ok=True)
        if data.get("verify") and not path.exists():
            raise GuestError("Transferred destination is missing")
        return {"ok": True}
    raise GuestError("Unknown guest operation")


def main(payload: str) -> None:
    try:
        result = run(json.loads(payload))
    except GuestError as exc:
        result = {"ok": False, "error": str(exc)}
    except Exception:
        result = {"ok": False, "error": "Guest check or write failed; output withheld"}
    print(json.dumps(result))
