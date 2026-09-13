"""Desktop-side parsing and patching for application configuration documents.

The guest only transports bytes.  Parsers live here so a fresh workspace does
not need any additional TOML or YAML packages installed.
"""
from __future__ import annotations

from collections.abc import Mapping, MutableMapping
from copy import deepcopy
import io
import json
from typing import Any

from ruamel.yaml import YAML

import tomlkit

from .models import AppError


_FORMATS = {"toml", "json", "yaml"}


def _same_value(left: Any, right: Any) -> bool:
    """Compare values without treating bools as integers."""
    if isinstance(left, bool) or isinstance(right, bool):
        return type(left) is type(right) and left == right
    if left is None or right is None:
        return left is None and right is None
    if type(left) is not type(right):
        # Parser scalar subclasses (for example tomlkit.Integer) are compared
        # by their unwrapped Python values below.
        left = _plain_value(left)
        right = _plain_value(right)
    if isinstance(left, Mapping) and isinstance(right, Mapping):
        return (len(left) == len(right)
                and all(key in right and _same_value(value, right[key])
                        for key, value in left.items()))
    if isinstance(left, (list, tuple)) and isinstance(right, (list, tuple)):
        return len(left) == len(right) and all(_same_value(a, b) for a, b in zip(left, right))
    return type(left) is type(right) and left == right


def _plain_value(value: Any) -> Any:
    unwrap = getattr(value, "unwrap", None)
    if callable(unwrap):
        try:
            value = unwrap()
        except Exception:
            pass
    if isinstance(value, Mapping):
        return {key: _plain_value(child) for key, child in value.items()}
    if isinstance(value, (list, tuple)):
        return [_plain_value(child) for child in value]
    # ruamel preserves quote/style through scalar subclasses.  Compare their
    # values rather than their presentation classes.
    if isinstance(value, str):
        return str(value)
    if isinstance(value, bool):
        return bool(value)
    if isinstance(value, int):
        return int(value)
    if isinstance(value, float):
        return float(value)
    return value


def _display_path(path: tuple[str, ...]) -> str:
    return ".".join(path) or "<root>"


def _parse_toml(text: str | None) -> Any:
    if text is None or not text.strip():
        return tomlkit.document()
    try:
        return tomlkit.parse(text)
    except Exception as exc:
        # Do not include parser details because they can contain document text.
        raise AppError("TOML configuration could not be parsed") from exc


def _yaml_parser() -> YAML:
    parser = YAML(typ="rt")
    parser.preserve_quotes = True
    parser.width = 4096
    return parser


def _parse_yaml(text: str | None) -> Any:
    if text is None or not text.strip():
        return {}
    try:
        value = _yaml_parser().load(text)
    except Exception as exc:
        raise AppError("YAML configuration could not be parsed") from exc
    if value is None:
        # A comment-only YAML file is an empty mapping.  An explicit null is a
        # scalar document and is rejected by merge_document's root check.
        content_lines = (
            line.strip() for line in text.splitlines()
            if line.strip() and line.strip() not in {"---", "..."}
        )
        if not any(not line.startswith("#") for line in content_lines):
            return {}
    return value


def _parse_json(text: str | None) -> Any:
    if text is None:
        return {}

    def reject_constant(_value: str) -> Any:
        raise ValueError("non-standard JSON number")

    def reject_duplicate(pairs: list[tuple[str, Any]]) -> dict[str, Any]:
        result: dict[str, Any] = {}
        for key, value in pairs:
            if key in result:
                raise ValueError("duplicate JSON key")
            result[key] = value
        return result

    try:
        return json.loads(
            text,
            parse_constant=reject_constant,
            object_pairs_hook=reject_duplicate,
        )
    except (TypeError, ValueError) as exc:
        raise AppError("JSON configuration could not be parsed") from exc


def _parse_document(format: str, text: str | None) -> Any:
    format = format.lower()
    if format == "toml":
        return _parse_toml(text)
    if format == "json":
        return _parse_json(text)
    if format == "yaml":
        return _parse_yaml(text)
    raise AppError("Structured configuration format must be TOML, JSON or YAML")


def _mapping(value: Any, path: tuple[str, ...]) -> MutableMapping[str, Any]:
    if not isinstance(value, MutableMapping):
        raise AppError(f"Configuration path {_display_path(path)} requires a mapping")
    return value


def _yaml_anchor_name(value: Any) -> str | None:
    anchor = getattr(value, "anchor", None)
    name = getattr(anchor, "value", None)
    return name if isinstance(name, str) and name else None


def _yaml_shared_mapping_ids(document: Any) -> set[int]:
    """Find mapping objects referenced by more than one YAML path."""
    counts: dict[int, int] = {}
    active: set[int] = set()

    def visit(value: Any) -> None:
        if isinstance(value, Mapping):
            identity = id(value)
            counts[identity] = counts.get(identity, 0) + 1
            if identity in active:
                return
            active.add(identity)
            for child in value.values():
                visit(child)
            active.remove(identity)
        elif isinstance(value, (list, tuple)):
            identity = id(value)
            if identity in active:
                return
            active.add(identity)
            for child in value:
                visit(child)
            active.remove(identity)

    visit(document)
    return {identity for identity, count in counts.items() if count > 1}


def _detach_yaml_alias(
    parent: MutableMapping[str, Any],
    key: str,
    child: Any,
    shared_ids: set[int] | None = None,
) -> Any:
    """Copy an anchored mapping before changing a descendant below it.

    ruamel represents an alias and its anchor as the same Python object.  A
    direct assignment below that object would therefore change every alias,
    including keys HomeStack did not declare.  Merge keys can also expose a
    shared descendant without giving that descendant its own anchor, so the
    caller supplies all shared mapping identities. Detaching only the path
    being edited keeps round-trip comments/order and leaves other aliases
    intact.
    """
    if not isinstance(child, MutableMapping):
        return child
    if _yaml_anchor_name(child) is None and (shared_ids is None or id(child) not in shared_ids):
        return child
    detached = deepcopy(child)
    clear_anchor = getattr(detached, "yaml_set_anchor", None)
    if callable(clear_anchor):
        clear_anchor(None, always_dump=False)
    parent[key] = detached
    return detached


def _lookup(document: Any, path: tuple[str, ...]) -> tuple[Any, Any, bool]:
    """Return (parent, leaf key, exists), checking all structural parents."""
    current = _mapping(document, ())
    traversed: list[str] = []
    for segment in path[:-1]:
        traversed.append(segment)
        if segment not in current:
            return current, path[-1], False
        child = current[segment]
        if not isinstance(child, Mapping):
            raise AppError(f"Configuration path {_display_path(path)} requires a mapping at {_display_path(path[:len(traversed)])}")
        current = child
    return current, path[-1], path[-1] in current


def _lookup_and_detach_yaml(
    document: Any,
    path: tuple[str, ...],
    shared_ids: set[int] | None = None,
) -> tuple[Any, Any, bool]:
    current = _mapping(document, ())
    traversed: list[str] = []
    for segment in path[:-1]:
        traversed.append(segment)
        if segment not in current:
            return current, path[-1], False
        child = current[segment]
        if not isinstance(child, Mapping):
            raise AppError(f"Configuration path {_display_path(path)} requires a mapping at {_display_path(tuple(traversed))}")
        child = _detach_yaml_alias(current, segment, child, shared_ids)
        current = child
    return current, path[-1], path[-1] in current


def _create_parent(
    document: Any,
    path: tuple[str, ...],
    *,
    yaml_mode: bool,
    yaml_shared_ids: set[int] | None = None,
) -> MutableMapping[str, Any]:
    current = _mapping(document, ())
    traversed: list[str] = []
    for segment in path[:-1]:
        traversed.append(segment)
        if segment in current:
            child = current[segment]
            if not isinstance(child, Mapping):
                raise AppError(f"Configuration path {_display_path(path)} requires a mapping at {_display_path(tuple(traversed))}")
            if yaml_mode:
                child = _detach_yaml_alias(current, segment, child, yaml_shared_ids)
            elif isinstance(child, tomlkit.items.InlineTable):
                # Inline tables cannot safely host a nested standard table;
                # promote the edited branch while retaining all its keys.
                promoted = tomlkit.table()
                for key, value in child.items():
                    promoted[key] = value
                current[segment] = child = promoted
            current = child
            continue
        child = {}
        if yaml_mode:
            # A plain mapping is accepted by ruamel and gains normal
            # round-trip behavior once attached to the CommentedMap root.
            from ruamel.yaml.comments import CommentedMap
            child = CommentedMap()
        elif isinstance(current, tomlkit.items.Table) or isinstance(current, tomlkit.toml_document.TOMLDocument):
            child = tomlkit.table()
        current[segment] = child
        current = child
    return current


def _set_value_existing(
    document: Any,
    path: tuple[str, ...],
    value: Any,
    *,
    yaml_mode: bool,
    yaml_shared_ids: set[int] | None = None,
) -> str:
    if not path or any(not isinstance(segment, str) for segment in path):
        raise AppError("Structured configuration paths must contain one or more text keys")
    # The lookup is deliberately performed before mutation.  It catches a
    # scalar/list parent and returns a precise, sanitized AppError.
    if yaml_mode:
        parent, key, exists = _lookup_and_detach_yaml(document, path, yaml_shared_ids)
    else:
        parent, key, exists = _lookup(document, path)
    if exists:
        state = "matching" if _same_value(parent[key], value) else "different"
        if state == "different":
            parent[key] = value
        return state
    parent = _create_parent(document, path, yaml_mode=yaml_mode, yaml_shared_ids=yaml_shared_ids)
    parent[path[-1]] = value
    return "missing"


def _dump_document(format: str, document: Any) -> str:
    if format == "toml":
        try:
            return tomlkit.dumps(document)
        except Exception as exc:
            raise AppError("TOML configuration could not be serialized") from exc
    if format == "json":
        try:
            return json.dumps(document, ensure_ascii=False, indent=2) + "\n"
        except (TypeError, ValueError, OverflowError) as exc:
            raise AppError("JSON configuration could not be serialized") from exc
    output = io.StringIO()
    try:
        _yaml_parser().dump(document, output)
    except Exception as exc:
        raise AppError("YAML configuration could not be serialized") from exc
    return output.getvalue()


def inspect_document(
    format: str,
    text: str | None,
    leaves: list[tuple[tuple[str, ...], Any]],
) -> list[str]:
    """Return live states without serializing or mutating the document.

    A malformed document is a file-wide error.  A structurally incompatible
    parent is reported as ``unavailable`` for only the affected leaf, so live
    inspection can still show matching/missing/different siblings.
    """
    if not isinstance(format, str):
        raise AppError("Structured configuration format must be TOML, JSON or YAML")
    format = format.lower()
    if format not in _FORMATS:
        raise AppError("Structured configuration format must be TOML, JSON or YAML")
    normalized: list[tuple[tuple[str, ...], Any]] = []
    seen: set[tuple[str, ...]] = set()
    for path, value in leaves:
        path = tuple(path)
        if not path or any(not isinstance(segment, str) for segment in path):
            raise AppError("Structured configuration paths must contain one or more text keys")
        if path in seen:
            raise AppError("Structured configuration contains duplicate managed paths")
        seen.add(path)
        normalized.append((path, value))
    document = _parse_document(format, text)
    if not isinstance(document, Mapping):
        return ["unavailable"] * len(normalized)
    result: list[str] = []
    for path, desired in normalized:
        try:
            parent, key, exists = _lookup(document, path)
        except AppError:
            result.append("unavailable")
            continue
        if not exists:
            result.append("missing")
        else:
            result.append("matching" if _same_value(parent[key], desired) else "different")
    return result


def merge_document(
    format: str,
    text: str | None,
    leaves: list[tuple[tuple[str, ...], Any]],
) -> tuple[str, list[str]]:
    """Patch selected leaves into a parsed document and return states.

    ``leaves`` contains only HomeStack-declared paths.  An absent target file
    starts as an empty mapping.  The original text is returned for a complete
    no-op so comments, aliases and formatting remain byte-for-byte unchanged.
    """
    if not isinstance(format, str):
        raise AppError("Structured configuration format must be TOML, JSON or YAML")
    format = format.lower()
    if format not in _FORMATS:
        raise AppError("Structured configuration format must be TOML, JSON or YAML")
    normalized: list[tuple[tuple[str, ...], Any]] = []
    seen: set[tuple[str, ...]] = set()
    for path, value in leaves:
        path = tuple(path)
        if not path or any(not isinstance(segment, str) for segment in path):
            raise AppError("Structured configuration paths must contain one or more text keys")
        if path in seen:
            raise AppError("Structured configuration contains duplicate managed paths")
        seen.add(path)
        normalized.append((path, value))
    document = _parse_document(format, text)
    if not isinstance(document, Mapping):
        raise AppError("Structured configuration document root must be a mapping")
    yaml_shared_ids = _yaml_shared_mapping_ids(document) if format == "yaml" else None
    states: list[str] = []
    try:
        # First inspect every path.  This ensures a structural conflict blocks
        # the whole candidate before any caller can consider it writable.
        for path, value in normalized:
            if format == "yaml":
                _lookup_and_detach_yaml(document, path, yaml_shared_ids)
            else:
                _lookup(document, path)
        for path, value in normalized:
            states.append(_set_value_existing(document, path, value, yaml_mode=format == "yaml", yaml_shared_ids=yaml_shared_ids))
    except AppError:
        raise
    except (KeyError, TypeError, ValueError) as exc:
        raise AppError("Structured configuration path could not be updated") from exc
    candidate = text if text is not None and all(state == "matching" for state in states) else _dump_document(format, document)
    # Always parse the generated candidate and verify every selected leaf.  A
    # parser may accept a syntactically valid construct while dropping an
    # attempted nested update (notably TOML inline-table promotion).
    checked = _parse_document(format, candidate)
    if not isinstance(checked, Mapping):
        raise AppError("Structured configuration candidate root is not a mapping")
    for path, value in normalized:
        current: Any = checked
        for segment in path:
            if not isinstance(current, Mapping) or segment not in current:
                raise AppError("Structured configuration candidate lost a managed value")
            current = current[segment]
        if not _same_value(current, value):
            raise AppError("Structured configuration candidate does not match managed values")
    return candidate, states
