"""Validated setup catalog definitions, defaults, and legacy adaptation."""
from __future__ import annotations

from dataclasses import asdict, dataclass, field
import hashlib
import json
import re
from typing import Any

from .models import AppError


@dataclass(frozen=True)
class Group:
    id: str
    label: str
    description: str = ""


@dataclass(frozen=True)
class FileParams:
    path: str


@dataclass(frozen=True)
class EnvironmentParams:
    profile: str


@dataclass(frozen=True)
class ApplicationParams:
    command: str
    interpreter: str = "bash"
    interaction: str = "interactive"
    prerequisites: tuple[str, ...] = ()
    check: str = ""
    prerequisite_checks: tuple[str, ...] = ()
    non_interactive: str = ""
    bin_dirs: tuple[str, ...] = ("~/.local/bin",)
    requires_absent: tuple[str, ...] = ()


@dataclass(frozen=True)
class RepositoryParams:
    repository: str


@dataclass(frozen=True)
class Entry:
    id: str
    group: str
    handler: str
    label: str
    description: str
    params: FileParams | EnvironmentParams | ApplicationParams | RepositoryParams
    depends_on: tuple[str, ...] = ()

    def definition(self) -> dict[str, Any]:
        data = asdict(self)
        data.update(data.pop("params"))
        return data


DEFAULT_GROUPS = (
    Group("files", "Files", "Desktop files copied into persistent home; no deletes."),
    Group("env", "Environment", "User shell profiles; login shell stays unchanged."),
    Group("app", "Applications", "Explicit user-space installers; onboarding is separate."),
    Group("repo", "Repositories", "GitHub checkouts with workspace-local deploy keys."),
)
CODEX_RECIPE = "curl -fsSL https://chatgpt.com/codex/install.sh | CODEX_NON_INTERACTIVE=1 sh"
# Download separately so installer stdin remains the real terminal when required.
HERMES_RECIPE = '''installer=$(mktemp)
trap 'rm -f -- "$installer"' EXIT
curl -fsSL https://hermes-agent.nousresearch.com/install.sh -o "$installer"
for stage in repository venv python-deps node-deps path config complete; do
    bash "$installer" --stage "$stage" --non-interactive --skip-browser --skip-computer-use
done'''


def defaults() -> tuple[Entry, ...]:
    profiles = [("bash", "Bash"), ("zsh", "Zsh"), ("fish", "Fish"), ("nu", "Nushell")]
    items = [Entry(i, "env", "environment", label,
                   f"Prepare {label} startup files and user executable paths.", EnvironmentParams(i))
             for i, label in profiles]
    items.extend([
        Entry("codex", "app", "application", "Codex", "Install Codex CLI. Sign-in remains separate.",
              ApplicationParams(CODEX_RECIPE, interaction="non-interactive", prerequisites=("curl", "tar"), check="codex --version")),
        Entry("opencode", "app", "application", "OpenCode", "Install OpenCode. Provider configuration remains separate.",
              ApplicationParams("curl -fsSL https://opencode.ai/install | bash", interaction="non-interactive",
                                prerequisites=("curl", "tar"), check="opencode --version", bin_dirs=("~/.opencode/bin", "~/.local/bin"))),
        Entry("hermes", "app", "application", "Hermes Agent",
              "Install the Hermes CLI through user-space bootstrap stages. No browser, desktop, gateway or provider setup. Run hermes setup separately.",
              ApplicationParams(HERMES_RECIPE, interaction="non-interactive", prerequisites=("git", "curl", "tar", "xz", "node", "npm", "g++", "make"),
                                prerequisite_checks=(
                                    "node -e \"const v=process.versions.node; const [a,b]=v.split('.').map(Number); process.exit(!v.includes('-') && ((a===22 && b>=22)||(a===24 && b>=11)||a>=26) ? 0 : 1)\"",
                                    "npm --version | node -e \"let v=\'\';process.stdin.on(\'data\',x=>v+=x).on(\'end\',()=>{const [a,b]=v.trim().split(\'.\').map(Number);process.exit(a===11 && b>=10 && b<=16 ? 1 : 0)})\"",
                                ),
                                check="hermes --version", bin_dirs=("~/.local/bin", "~/.hermes/bin", "~/.hermes/node/bin"),
                                requires_absent=("~/.hermes/hermes-agent",))),
    ])
    return tuple(items)


@dataclass(frozen=True)
class SetupConfig:
    groups: tuple[Group, ...] = DEFAULT_GROUPS
    items: tuple[Entry, ...] = field(default_factory=defaults)


def _strings(value: Any, key: str) -> tuple[str, ...]:
    if not isinstance(value, (list, tuple)) or any(not isinstance(x, str) or not x.strip() or "\0" in x for x in value):
        raise AppError(f"[setup] {key} must be an array of non-empty strings")
    return tuple(value)


def parse_setup(raw: Any) -> SetupConfig:
    from .config import validate_sync_path_spec, validate_repository_spec
    if not isinstance(raw, dict) or set(raw) - {"groups", "items"}:
        raise AppError("[setup] accepts only groups and items")
    groups = {g.id: g for g in DEFAULT_GROUPS}
    seen: set[str] = set()
    if not isinstance(raw.get("groups", []), list):
        raise AppError("[setup] groups must be an array of tables")
    for data in raw.get("groups", []):
        if not isinstance(data, dict) or set(data) - {"id", "label", "description"}:
            raise AppError("Invalid setup group definition")
        group_id = data.get("id", "")
        if not isinstance(group_id, str) or not re.fullmatch(r"[a-z][a-z0-9_-]*", group_id) or group_id in seen or group_id in {"root", "f", "e", "a", "r", *(e.id for e in defaults())}:
            raise AppError("Invalid or duplicate setup group ID")
        seen.add(group_id)
        base = asdict(groups[group_id]) if group_id in groups else {}
        base.update(data)
        if not isinstance(base.get("label"), str) or not base["label"].strip():
            raise AppError("Setup group requires a label")
        if not isinstance(base.get("description", ""), str):
            raise AppError("Setup group description must be text")
        groups[group_id] = Group(**base)
    entries = {e.id: e.definition() for e in defaults()}
    seen.clear()
    raw_items = raw.get("items", [])
    if not isinstance(raw_items, list):
        raise AppError("[setup] items must be an array of tables")
    for data in raw_items:
        if not isinstance(data, dict):
            raise AppError("Setup item must be a table")
        item_id = data.get("id", "")
        if not isinstance(item_id, str) or not re.fullmatch(r"[a-z][a-z0-9_-]*", item_id) or item_id in seen or item_id in groups or item_id in {"root", "all"}:
            raise AppError("Invalid or duplicate setup item ID")
        seen.add(item_id)
        base = entries.get(item_id, {}).copy()
        if "command" in data and data["command"] != base.get("command"):
            # A customized payload cannot inherit a safety claim about another recipe.
            base.update(interaction="interactive", non_interactive="", check="", requires_absent=())
        base.update(data)
        entries[item_id] = base
    parsed = []
    types = {"file": FileParams, "environment": EnvironmentParams, "application": ApplicationParams, "repository": RepositoryParams}
    common = {"id", "group", "handler", "label", "description", "depends_on"}
    for data in entries.values():
        handler = data.get("handler")
        if not isinstance(handler, str) or not isinstance(data.get("group"), str) or handler not in types or data.get("group") not in groups:
            raise AppError("Setup item has an unknown handler or group")
        for key in ("label", "description"):
            if not isinstance(data.get(key), str) or not data[key].strip():
                raise AppError(f"Setup item {data['id']} needs {key}")
        cls = types[handler]
        params = {k: v for k, v in data.items() if k not in common}
        if set(params) - set(cls.__dataclass_fields__):
            raise AppError(f"Unknown parameters for setup item {data['id']}")
        try:
            for key in ("prerequisites", "bin_dirs", "requires_absent", "prerequisite_checks"):
                if key in params:
                    params[key] = _strings(params[key], key)
            p = cls(**params)
        except TypeError as exc:
            raise AppError(f"Invalid parameters for setup item {data['id']}") from exc
        if isinstance(p, FileParams):
            if not isinstance(p.path, str):
                raise AppError("File path must be a string")
            validate_sync_path_spec(p.path)
        if isinstance(p, EnvironmentParams) and (not isinstance(p.profile, str) or p.profile not in {"bash", "zsh", "fish", "nu"}):
            raise AppError("Environment profile must be bash, zsh, fish or nu")
        if isinstance(p, RepositoryParams):
            validate_repository_spec(p.repository)
        if isinstance(p, ApplicationParams):
            if not isinstance(p.interpreter, str) or p.interpreter not in {"bash", "zsh"}:
                raise AppError("Application interpreter must be bash or zsh (pipeline failure handling required)")
            if not isinstance(p.interaction, str) or p.interaction not in {"interactive", "non-interactive"}:
                raise AppError("Application interaction must be interactive or non-interactive")
            for key in ("command", "check", "non_interactive"):
                value = getattr(p, key)
                if not isinstance(value, str) or "\0" in value or (key == "command" and not value.strip()):
                    raise AppError(f"Application {key} must be valid text")
            for tool in p.prerequisites:
                if not re.fullmatch(r"[A-Za-z0-9_.+-]+", tool):
                    raise AppError("Application prerequisites must be executable names")
            if any(":" in path for path in p.bin_dirs):
                raise AppError("Application bin_dirs must not contain PATH separators")
            for path in (*p.bin_dirs, *p.requires_absent):
                validate_sync_path_spec(path)
        parsed.append(Entry(data["id"], data["group"], handler, data["label"], data["description"], p,
                            _strings(data.get("depends_on", ()), "depends_on")))
    return SetupConfig(tuple(groups.values()), tuple(parsed))


def legacy_id(kind: str, payload: str) -> str:
    return f"legacy-{kind}-" + hashlib.sha256(payload.encode()).hexdigest()[:16]


def effective_entries(cfg: Any) -> tuple[Entry, ...]:
    items = list(cfg.setup.items)
    paths = {e.params.path for e in items if isinstance(e.params, FileParams)}
    for path in cfg.sync_paths:
        if path not in paths:
            items.append(Entry(legacy_id("file", path), "files", "file", path, "Legacy sync path. Move to [[setup.items]] to customize its label.", FileParams(path)))
            paths.add(path)
    commands = {e.params.command for e in items if isinstance(e.params, ApplicationParams)}
    for command in cfg.sync_commands:
        if command in commands:
            continue
        items.append(Entry(legacy_id("app", command), "app", "application", "Legacy application " + str(len([e for e in items if e.id.startswith('legacy-app-')]) + 1),
                           "Preserved legacy sync command. Review the payload in configuration and explicitly declare its interaction mode before unattended use.", ApplicationParams(command)))
        commands.add(command)
    if len({e.id for e in items}) != len(items):
        raise AppError("A configured stable ID conflicts with a legacy setup entry; reconcile the duplicate ID")
    return tuple(items)


def setup_to_toml(setup: SetupConfig) -> str:
    lines = []
    for group in setup.groups:
        lines += ["", "[[setup.groups]]"]
        lines.extend(f"{k} = {json.dumps(v)}" for k, v in asdict(group).items())
    for entry in setup.items:
        lines += ["", "[[setup.items]]"]
        lines.extend(f"{k} = {json.dumps(v)}" for k, v in entry.definition().items())
    return "\n".join(lines) + "\n"
