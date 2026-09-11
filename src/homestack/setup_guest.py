"""Self-contained guest operations. Executed as the workspace user using Python stdlib.

No module imports from HomeStack: this file is also the packaged remote program.
"""
from __future__ import annotations

import json
import os
import re
import hashlib
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


def managed(text: str, identity: str, body: str) -> str:
    begin, end = f"# >>> HomeStack {identity} >>>", f"# <<< HomeStack {identity} <<<"
    block = begin + "\n" + body.rstrip() + "\n" + end + "\n"
    if begin in text or end in text:
        if text.count(begin) != 1 or text.count(end) != 1 or text.index(end) < text.index(begin):
            raise GuestError("Malformed HomeStack managed block; reconcile it manually")
        start = text.index(begin)
        finish = text.index(end) + len(end)
        if text[finish:finish + 1] == "\n":
            finish += 1
        return text[:start] + block + text[finish:]
    return text + ("\n" if text and not text.endswith("\n") else "") + block


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
        body = paths + '''
case $- in *i*)
    if ! declare -F _completion_loader >/dev/null && [ -r /usr/share/bash-completion/bash_completion ]; then
        . /usr/share/bash-completion/bash_completion
    fi
;; esac'''
        login = next((p for p in (".bash_profile", ".bash_login", ".profile") if (home / p).exists() or (home / p).is_symlink()), ".profile")
        safe_path(home, login)
        safe_path(home, ".bashrc")
        login_text = (home / login).read_text() if (home / login).exists() else ""
        bash_text = (home / ".bashrc").read_text() if (home / ".bashrc").exists() else ""
        # Do not add an edge to startup files that already source a login file.
        if any(p in bash_text for p in (".bash_profile", ".bash_login", ".profile")):
            raise GuestError("Bash startup source-cycle risk; reconcile login-file references in .bashrc")
        login_body = paths
        outside_managed = re.sub(r"(?ms)^# >>> HomeStack bash-login >>>\n.*?^# <<< HomeStack bash-login <<<\n?", "", login_text)
        if ".bashrc" not in "\n".join(line for line in outside_managed.splitlines() if not line.lstrip().startswith("#")):
            login_body += '\nif [ -n "${BASH_VERSION:-}" ] && [ -r "$HOME/.bashrc" ]; then\n    . "$HOME/.bashrc"\nfi'
        return {".bashrc": managed(bash_text, "bash", body), login: managed(login_text, "bash-login", login_body)}
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
        path = safe_path(home, relative)
        result[relative] = managed(path.read_text() if path.exists() else "", profile, body)
    return result


def atomic_write(path: Path, content: str, *, force: bool = False) -> bool:
    payload = content.encode()
    old = path.read_bytes() if path.exists() else None
    if old == payload and not force:
        return False
    mode = stat.S_IMODE(path.stat().st_mode) & 0o700 if old is not None else 0o600
    _atomic_bytes(path, payload, mode=mode)
    return True


def run(data: dict) -> dict:
    home = Path(data["home"])
    op = data["operation"]
    if op == "identity":
        verify_identity(data)
        return {"ok": True}
    if op == "state-read":
        registry = load_registry(home, vmid=int(data["vmid"]), name=str(data["name"]))
        return {"ok": True, "registry": registry, "state_path": "~/" + STATE_FILE}
    if op == "metadata":
        return {"ok": True, "items": [path_metadata(home, relative) for relative in data.get("paths", [])]}
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
        overwrite_paths = set(data.get("overwrite_paths", ()))
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
                   if relative in overwrite_paths
                   or not (home / relative).exists()
                   or (home / relative).read_text() != content]
        if data.get("apply"):
            for relative in changed:
                atomic_write(safe_path(home, relative), updates[relative], force=relative in overwrite_paths)
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
