"""Targetless catalog discovery and immutable numeric selection snapshots."""
from __future__ import annotations

from dataclasses import dataclass
from datetime import datetime
import hashlib
import json
import os
from pathlib import Path
import re
import tempfile
import uuid

from .config import Config, validate_repository_spec
from .models import AppError
from .repo import _github_json
from .setup_config import Entry, FileParams, EnvironmentParams, ApplicationParams, RepositoryParams, effective_entries

ALIASES = {"f": "files", "e": "env", "a": "app", "r": "repo"}


@dataclass
class Catalog:
    entries: tuple[Entry, ...]
    availability: dict[str, str]
    github: dict | None = None
    repository_error: str = ""
    snapshot_id: str | None = None
    timestamps: dict | None = None

    def rows(self, cfg: Config) -> list[dict]:
        rows = []
        for group in cfg.setup.groups:
            for i, entry in enumerate((e for e in self.entries if e.group == group.id), 1):
                location = getattr(entry.params, "path", getattr(entry.params, "repository", ""))
                if isinstance(entry.params, ApplicationParams):
                    location = ", ".join(entry.params.bin_dirs)
                elif isinstance(entry.params, EnvironmentParams):
                    from .setup import write_paths
                    location = ", ".join("~/" + p for p in write_paths(cfg, entry))
                rows.append({"index": i, "id": entry.id, "group": group.id, "label": entry.label,
                             "path_or_repository": location,
                             "availability": self.availability.get(entry.id, "unknown"), "guest_state": "unknown",
                             **(self.timestamps or {}).get(entry.id, {})})
        return rows


def github_identity(cfg: Config) -> dict:
    user = _github_json(["user"], dict)
    if not isinstance(user.get("login"), str) or not isinstance(user.get("id"), int):
        raise AppError("GitHub account identity unavailable; run gh auth login on the desktop")
    return {"host": "github.com", "account": user["login"], "account_id": user["id"], "owner": cfg.repo_owner}


def discover_repositories(cfg: Config) -> tuple[dict, list[dict]]:
    if not cfg.repo_owner:
        raise AppError("Repository discovery unavailable: configure [repo] owner")
    identity = github_identity(cfg)
    # This authenticated endpoint includes private repositories, collaborators and organizations.
    pages = _github_json(["--paginate", "--slurp", "user/repos?per_page=100&affiliation=owner,collaborator,organization_member&visibility=all"], list)
    repos = {}
    for page in pages:
        if not isinstance(page, list):
            raise AppError("GitHub pagination returned an invalid page")
        for repo in page:
            if not isinstance(repo, dict):
                raise AppError("GitHub returned an invalid repository")
            name = validate_repository_spec(repo.get("full_name", ""))
            if name.split("/")[0].casefold() == cfg.repo_owner.casefold():
                repos[name] = repo
    def timestamp(value):
        if not value:
            return 0.0
        try:
            return datetime.fromisoformat(value.replace("Z", "+00:00")).timestamp()
        except (ValueError, TypeError) as exc:
            raise AppError("GitHub returned an invalid repository timestamp") from exc
    def key(repo):
        name = repo["full_name"]
        created = timestamp(repo.get("created_at"))
        recent = max(created, timestamp(repo.get("pushed_at")))
        return (0 if cfg.repo_sort == "name" else -(created if cfg.repo_sort == "created" else recent), name.casefold(), name)
    return identity, sorted(repos.values(), key=key)


def load_catalog(cfg: Config, *, repositories: bool = False) -> Catalog:
    entries = list(effective_entries(cfg))
    availability = {}
    for entry in entries:
        if isinstance(entry.params, FileParams):
            from .sync import sync_plan_item
            availability[entry.id] = sync_plan_item(cfg, entry.params.path)["status"]
        else:
            availability[entry.id] = "guest state unknown"
    catalog = Catalog(tuple(entries), availability, timestamps={})
    if repositories:
        try:
            catalog.github, repos = discover_repositories(cfg)
            known = {e.params.repository: e.id for e in entries if isinstance(e.params, RepositoryParams)}
            for repo in repos:
                name = repo["full_name"]
                if name not in known:
                    entries.append(Entry(name, "repo", "repository", name, "Provision a Git checkout and repo-scoped deploy key; no project scripts run.", RepositoryParams(name)))
                item_id = known.get(name, name)
                availability[item_id] = "available" if repo.get("permissions", {}).get("admin") and not repo.get("archived") and not repo.get("disabled") else "unavailable: repository administration permission required or repository archived/disabled"
                catalog.timestamps[item_id] = {k: repo.get(k) for k in ("created_at", "pushed_at", "private", "archived")}
            order = {r["full_name"]: i for i, r in enumerate(repos)}
            repository_entries = [e for e in entries if e.group == "repo" and isinstance(e.params, RepositoryParams)]
            entries = [e for e in entries if e not in repository_entries] + sorted(repository_entries, key=lambda e: (order.get(e.params.repository, len(order)), e.params.repository))
        except AppError as exc:
            catalog.repository_error = str(exc)
    catalog.entries = tuple(entries)
    return catalog


def fingerprint(cfg: Config) -> str:
    payload = {"items": [e.definition() for e in effective_entries(cfg)],
               "groups": [(g.id, g.label, g.description) for g in cfg.setup.groups]}
    return hashlib.sha256(json.dumps(payload, sort_keys=True).encode()).hexdigest()


def snapshot_directory(cfg: Config) -> Path:
    base = Path(os.environ.get("XDG_STATE_HOME", str(Path.home() / ".local/state")))
    scope = hashlib.sha256(str(cfg.path.expanduser().resolve()).encode()).hexdigest()[:24]
    return base / "homestack/catalogs" / scope


def save_snapshot(cfg: Config, catalog: Catalog) -> str:
    directory = snapshot_directory(cfg)
    directory.mkdir(parents=True, exist_ok=True, mode=0o700)
    identifier = uuid.uuid4().hex
    data = {"version": 1, "id": identifier, "config": str(cfg.path.expanduser().resolve()),
            "fingerprint": fingerprint(cfg), "github": catalog.github,
            "repo": {"owner": cfg.repo_owner, "checkout_root": cfg.repo_checkout_root, "sort": cfg.repo_sort},
            "mapping": {g.id: [e.id for e in catalog.entries if e.group == g.id] for g in cfg.setup.groups},
            "repositories": {e.id: e.params.repository for e in catalog.entries if isinstance(e.params, RepositoryParams)}}
    # Immutable metadata only: no commands, tokens or desktop file contents.
    with (directory / f"{identifier}.json").open("x", encoding="utf-8") as handle:
        os.chmod(handle.name, 0o600)
        json.dump(data, handle)
        handle.flush()
        os.fsync(handle.fileno())
    fd, name = tempfile.mkstemp(dir=directory, prefix=".latest-")
    try:
        with os.fdopen(fd, "w") as handle:
            handle.write(identifier)
            handle.flush()
            os.fsync(handle.fileno())
        os.replace(name, directory / "latest")
    finally:
        Path(name).unlink(missing_ok=True)
    catalog.snapshot_id = identifier
    return identifier


def read_snapshot(cfg: Config, identifier: str | None, *, repositories: bool) -> dict:
    hint = "Run 'homestack setup list' and reuse its catalog ID or use stable IDs."
    directory = snapshot_directory(cfg)
    try:
        identifier = identifier or (directory / "latest").read_text().strip()
        if not re.fullmatch(r"[a-f0-9]{32}", identifier):
            raise AppError(f"Invalid catalog ID. {hint}")
        data = json.loads((directory / f"{identifier}.json").read_text())
    except (OSError, ValueError) as exc:
        raise AppError(f"Catalog snapshot is missing or invalid. {hint}") from exc
    if not isinstance(data, dict) or not isinstance(data.get("mapping"), dict) or not isinstance(data.get("repositories"), dict):
        raise AppError(f"Catalog snapshot is invalid. {hint}")
    if any(not isinstance(ids, list) or any(not isinstance(i, str) for i in ids) for ids in data["mapping"].values()):
        raise AppError(f"Catalog snapshot mapping is invalid. {hint}")
    if (data.get("version") != 1 or data.get("id") != identifier or
        data.get("config") != str(cfg.path.expanduser().resolve()) or data.get("fingerprint") != fingerprint(cfg)):
        raise AppError(f"Catalog snapshot is incompatible with this configuration. {hint}")
    if repositories:
        repo_scope = {"owner": cfg.repo_owner, "checkout_root": cfg.repo_checkout_root, "sort": cfg.repo_sort}
        if data.get("repo") != repo_scope or not data.get("github") or data["github"] != github_identity(cfg):
            raise AppError(f"Catalog repository identity is incompatible. {hint}")
    return data


def parse_assignments(tokens: list[str], cfg: Config) -> dict[str, tuple[str, ...]]:
    groups = {g.id for g in cfg.setup.groups}
    assignments = {}
    for token in tokens:
        if token.count("=") != 1:
            raise AppError("Selectors must use group=item,item (files/f, env/e, app/a, repo/r)")
        key, value = token.split("=")
        key = ALIASES.get(key, key)
        if key not in groups:
            raise AppError(f"Unknown setup selector {key!r}")
        values = tuple(dict.fromkeys(value.split(",")))
        if any(not x or x.strip() != x for x in values):
            raise AppError("Selector contains an empty or malformed item")
        if any(x in {"0", "all"} for x in values):
            if len(values) != 1:
                raise AppError("0 or all must be used alone within a category")
            values = ("all",)
        if key in assignments and set(assignments[key]) != set(values):
            raise AppError(f"Conflicting repeated assignment for {key}")
        assignments[key] = values
    return assignments


def select_entries(cfg: Config, tokens: list[str], *, catalog_id: str | None = None) -> tuple[tuple[Entry, ...], str | None]:
    assignments = parse_assignments(tokens, cfg)
    numeric_groups = {g for g, values in assignments.items() if any(v.isdecimal() for v in values)}
    entries = {e.id: e for e in effective_entries(cfg)}
    repository_groups = {"repo", *(e.group for e in entries.values() if isinstance(e.params, RepositoryParams))}
    snapshot = read_snapshot(cfg, catalog_id, repositories=bool(repository_groups & assignments.keys())) if numeric_groups or catalog_id else None
    if snapshot:
        for item_id, repository in snapshot["repositories"].items():
            if item_id not in entries:
                entries[item_id] = Entry(item_id, "repo", "repository", repository, "Provision the displayed repository identity.", RepositoryParams(validate_repository_spec(repository)))
    if assignments.get("repo") == ("all",) and not snapshot:
        catalog = load_catalog(cfg, repositories=True)
        if catalog.repository_error:
            raise AppError(catalog.repository_error)
        entries.update({e.id: e for e in catalog.entries})
    selected = {}
    for group, values in assignments.items():
        for value in values:
            if value == "all":
                ids = snapshot["mapping"].get(group, []) if snapshot else [e.id for e in entries.values() if e.group == group]
            elif value.isdecimal():
                mapping = snapshot["mapping"].get(group, [])
                number = int(value)
                if not 1 <= number <= len(mapping):
                    raise AppError(f"Index {value} is out of range for {group}; run setup list")
                ids = [mapping[number - 1]]
            else:
                if group == "repo" and "/" in value:
                    repository = validate_repository_spec(value)
                    existing = next((e for e in entries.values() if e.group == group and isinstance(e.params, RepositoryParams) and e.params.repository.casefold() == repository.casefold()), None)
                    if existing:
                        value = existing.id
                    else:
                        entries[repository] = Entry(repository, "repo", "repository", repository, "Explicit repository checkout.", RepositoryParams(repository))
                ids = [value]
            for item_id in ids:
                entry = entries.get(item_id)
                if not entry or entry.group != group:
                    raise AppError(f"Unknown stable ID {item_id!r} in {group}; run setup list")
                selected[item_id] = entry
    if not selected:
        raise AppError("No actions selected; run setup list and specify at least one item")
    return tuple(selected.values()), snapshot["id"] if snapshot else None
