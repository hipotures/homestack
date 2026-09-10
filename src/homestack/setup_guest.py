"""Self-contained guest operations. Executed as the workspace user using Python stdlib.

No module imports from HomeStack: this file is also the packaged remote program.
"""
from __future__ import annotations

import json
import os
import re
from pathlib import Path
import stat
import subprocess
import tempfile
import time


class GuestError(Exception):
    pass


def safe_path(home: Path, relative: str, *, directory: bool = False, recursive: bool = False) -> Path:
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


def atomic_write(path: Path, content: str) -> bool:
    payload = content.encode()
    old = path.read_bytes() if path.exists() else None
    if old == payload:
        return False
    path.parent.mkdir(mode=0o700, parents=True, exist_ok=True)
    if old is not None:
        backup = path.with_name(path.name + f".homestack-backup-{time.time_ns()}")
        with backup.open("xb") as handle:
            os.chmod(backup, 0o600)
            handle.write(old)
            handle.flush()
            os.fsync(handle.fileno())
    fd, temporary = tempfile.mkstemp(prefix=".homestack-", dir=path.parent)
    try:
        with os.fdopen(fd, "wb") as handle:
            os.fchmod(handle.fileno(), stat.S_IMODE(path.stat().st_mode) & 0o700 if old is not None else 0o600)
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
    return True


def run(data: dict) -> dict:
    home = Path(data["home"])
    op = data["operation"]
    if op == "identity":
        verify_identity(data)
        return {"ok": True}
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
                   if not (home / relative).exists() or (home / relative).read_text() != content]
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
