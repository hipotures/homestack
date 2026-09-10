"""Mouse/keyboard workspace setup selector with persistent SSH state inspection."""
from __future__ import annotations

import threading
from datetime import datetime, timezone

from rich.style import Style
from rich.text import Text
from textual import events, work
from textual.app import App, ComposeResult
from textual.binding import Binding
from textual.containers import Container, Horizontal, VerticalScroll
from textual.message import Message
from textual.screen import ModalScreen
from textual.widgets import Footer, Input, Static, Tree
from textual.worker import get_current_worker

from .models import AppError
from .setup import build_plan, execute_plan, inspect_workspace_state, write_paths
from .setup_catalog import Catalog, load_catalog, save_snapshot
from .setup_config import ApplicationParams, FileParams, RepositoryParams, defaults


def _mtime_text(value):
    if not isinstance(value, int):
        return "unknown"
    return datetime.fromtimestamp(value / 1_000_000_000, timezone.utc).isoformat(timespec="seconds").replace("+00:00", "Z")


class CompactAction(Static, can_focus=True):
    """One-row action with a timer scoped to its pending key badge."""

    DEFAULT_CSS = """
    CompactAction { height: 1; width: auto; }
    CompactAction:focus { background: $boost; text-style: bold underline; }
    CompactAction:disabled { text-style: dim; }
    """
    BINDINGS = [Binding("enter", "activate", show=False)]

    class Pressed(Message):
        def __init__(self, control):
            self.action = control
            super().__init__()

        @property
        def control(self):
            return self.action

    def __init__(self, key, label, *, id):
        super().__init__(id=id, markup=False)
        self.key = key
        self.label = label
        self.pending = False
        self.blink_bright = False
        self._blink_timer = None

    def on_mount(self):
        self._blink_timer = self.set_interval(1.0, self.advance_blink, pause=True)
        if self.pending:
            self._blink_timer.resume()

    def on_unmount(self):
        if self._blink_timer is not None:
            self._blink_timer.stop()
            self._blink_timer = None

    def set_pending(self, pending):
        if pending == self.pending:
            return
        self.pending = pending
        self.blink_bright = pending
        if self._blink_timer is not None:
            if pending:
                self._blink_timer.resume()
            else:
                self._blink_timer.pause()
        self.refresh()

    def advance_blink(self):
        if self.pending and not self.disabled:
            self.blink_bright = not self.blink_bright
            self.refresh()

    def render(self):
        color = "white"
        if self.disabled:
            color = "#6e7681"
        elif self.pending:
            color = "bold #d29922" if self.blink_bright else "#6e7681"
        text = Text(no_wrap=True, overflow="ellipsis")
        text.append(f"[ {self.key} ]", style=f"{color} on #30363d")
        text.append(" " + self.label)
        return text

    def action_activate(self):
        if not self.disabled:
            self.post_message(self.Pressed(self))

    def on_click(self, event: events.Click):
        if event.button == 1:
            event.stop()
            self.focus()
            self.action_activate()


class SetupTree(Tree[str]):
    auto_expand = False
    BINDINGS = [
        Binding("space", "check", "Toggle"),
        Binding("0", "select_branch", "Select branch"),
        Binding("left", "collapse", "Collapse"),
        Binding("right", "expand", "Expand"),
        Binding("enter", "review", "Review & apply"),
    ]

    def render_label(self, node, base_style, style):
        label = super().render_label(node, base_style, style)
        prefix = len(self.ICON_NODE_EXPANDED if node.is_expanded else self.ICON_NODE) if node.allow_expand else 0
        label.stylize(Style(meta={"setup_checkbox": node.data}), prefix, prefix + 3)
        return label

    async def _on_click(self, event: events.Click) -> None:
        event.prevent_default()
        if event.button != 1:
            return
        identity = event.style.meta.get("setup_checkbox")
        if identity is not None:
            event.stop()
            node = self.app.nodes.get(identity)
            if node is not None:
                self.focus()
                self.move_cursor(node)
                self.app.toggle_node(node)
        else:
            await super()._on_click(event)

    def action_check(self):
        self.app.toggle_node(self.cursor_node)

    def action_select_branch(self):
        self.app.toggle_node(self.cursor_node, select=True)

    def action_collapse(self):
        node = self.cursor_node
        if node:
            if node.is_expanded:
                node.collapse()
                self.call_after_refresh(self.move_cursor, node)
            elif node.parent:
                self.select_node(node.parent)

    def action_expand(self):
        if self.cursor_node:
            node = self.cursor_node
            node.expand()
            self.call_after_refresh(self.move_cursor, node)

    def action_review(self):
        self.app.review()


class Review(ModalScreen[bool]):
    BINDINGS = [("escape", "back", "Back"), ("enter", "confirm", "Confirm")]
    CSS = (
        "Review { align: center middle; } "
        "#review-box { width: 90%; height: 85%; border: solid $accent; "
        "background: $surface; padding: 1 2; overflow-x: hidden; } "
        "#review-content { height: 1fr; overflow-x: hidden; } "
        "#review-actions { height: 1; } "
        "#back { width: 13; } #apply { width: 1fr; } "
        "Review.narrow #review-box { width: 100%; padding: 1 0; }"
    )

    def __init__(self, text: str, *, apply: bool):
        super().__init__()
        self.text = text
        self.apply = apply

    def compose(self) -> ComposeResult:
        with Container(id="review-box"):
            with VerticalScroll(id="review-content"):
                yield Static(Text(self.text, overflow="fold", no_wrap=False))
            with Horizontal(id="review-actions"):
                yield CompactAction("Esc", "Back", id="back")
                if self.apply:
                    yield CompactAction("Enter", "Apply this plan", id="apply")

    def on_mount(self):
        self.query_one("#apply" if self.apply else "#back", CompactAction).focus()

    def on_resize(self, event: events.Resize):
        self.set_class(event.size.width < 50, "narrow")

    def on_compact_action_pressed(self, event: CompactAction.Pressed):
        event.stop()
        self.dismiss(event.control.id == "apply")

    def action_back(self):
        self.dismiss(False)

    def action_confirm(self):
        self.dismiss(self.apply)


class SetupApp(App):
    """Selection is session-local; colors reflect the last inspected guest state."""

    CSS = """
    #target, #counts, #keys { height: auto; padding: 0 1; }
    #counts.has-hidden { color: $warning; text-style: bold; }
    #filter { height: 3; }
    #browser { height: 1fr; layout: vertical; }
    #tree { height: 1fr; width: 1fr; border: solid $panel; }
    #tree:focus { border: solid $accent; }
    #details-pane { height: 8; max-height: 40%; width: 1fr; border: solid $panel; padding: 0 1; overflow-x: hidden; }
    #details-pane:focus { border: solid $accent; }
    #details { height: auto; width: 1fr; }
    #browser.wide { layout: horizontal; }
    #browser.wide #tree { width: 3fr; height: 1fr; }
    #browser.wide #details-pane { width: 2fr; height: 1fr; max-height: 100%; }
    #controls { height: 1; }
    #review { width: 2fr; }
    #cancel { width: 1fr; }
    """

    BINDINGS = [
        ("/", "filter", "Filter"),
        ("enter", "review", "Review & apply"),
        ("f5", "refresh_state", "Refresh state"),
        ("escape", "cancel", "Cancel"),
        ("ctrl+r", "refresh_catalog", "Refresh catalog"),
        Binding("ctrl+c", "cancel", "Cancel", priority=True),
    ]

    def __init__(
        self,
        cfg,
        target,
        *,
        catalog=None,
        loader=load_catalog,
        executor=execute_plan,
        workspace=None,
        state=None,
    ):
        super().__init__()
        self.cfg, self.target = cfg, target
        self.catalog = catalog or load_catalog(cfg)
        self.loader, self.executor = loader, executor
        self.workspace = workspace
        self.workspace_state = state or {
            "items": {},
            "checked_at": None,
            "registry_present": False,
            "state_path": "~/.local/state/homestack/setup.json",
        }
        self.selected: set[str] = set()
        self.filter_text = ""
        self.nodes = {}
        self.busy = False
        self.cancel_requested = threading.Event()
        self.result = None
        self.action_states = {}
        self.pending_plan = None
        self.details_identity = None
        self._details_line_count = 0
        self._rebuild_generation = 0
        self.ssh_status = "connected" if workspace is not None else "not connected"

    def compose(self) -> ComposeResult:
        yield Static(self.target_label(), id="target", markup=False)
        yield Input(placeholder="Filter (/). Bulk selection affects visible matches only.", id="filter")
        with Container(id="browser"):
            yield SetupTree(Text("[ ] Setup"), data="root", id="tree")
            with VerticalScroll(id="details-pane", can_focus=False):
                yield Static(
                    "Highlight an item for details. Enter reviews selected actions.",
                    id="details",
                    markup=False,
                )
        yield Static("Nothing selected", id="counts", markup=False)
        with Horizontal(id="controls"):
            yield CompactAction("Enter", "Review & apply", id="review")
            yield CompactAction("Esc", "Cancel", id="cancel")
        yield Static(
            "↑↓ Move  ←→ Expand/collapse  Space Toggle  Enter Review & apply  Tab Focus  / Filter  F5 Refresh state",
            id="keys",
            markup=False,
        )
        yield Footer()

    def target_label(self, progress=""):
        parts = [f"Workspace: {self.target['name']} (VM {self.target['vmid']})"]
        if self.target.get("ip"):
            parts.append(str(self.target["ip"]))
        parts.append(f"SSH: {self.ssh_status}")
        if self.workspace_state.get("checked_at"):
            parts.append(f"State: {self.workspace_state['checked_at']}")
        if self.catalog.snapshot_id:
            parts.append(f"Catalog: {self.catalog.snapshot_id[:8]}")
        if progress:
            parts.append(progress)
        return "    ".join(parts)

    def on_mount(self):
        self.watch(
            self.query_one("#details-pane"),
            "virtual_size",
            lambda: self.call_after_refresh(self.update_details_focus),
        )
        self.update_layout()
        self.rebuild()
        self.query_one(SetupTree).focus()

    def on_resize(self, event: events.Resize):
        self.update_layout(event.size.width)

    def update_layout(self, width=None):
        self.query_one("#browser").set_class(
            (self.size.width if width is None else width) >= 110,
            "wide",
        )
        self.call_after_refresh(self.update_details_focus)

    def update_details_focus(self):
        pane = self.query_one("#details-pane", VerticalScroll)
        visible_lines = max(1, pane.scrollable_content_region.height)
        pane.can_focus = pane.max_scroll_y > 0 or self._details_line_count > visible_lines
        if not pane.can_focus and pane.has_focus:
            self.query_one(SetupTree).focus()

    def show_details(self, identity):
        pane = self.query_one("#details-pane", VerticalScroll)
        if identity != self.details_identity:
            pane.scroll_to(y=0, animate=False, force=True)
        self.details_identity = identity
        content = self.details(identity)
        self._details_line_count = len(content.splitlines())
        rendered = Text(content, overflow="fold", no_wrap=False)
        if content.startswith("Status: "):
            status = content.splitlines()[0].removeprefix("Status: ")
            color = {"UPDATE": "red", "INSTALL": "green"}.get(status, "yellow")
            rendered.stylize(f"bold {color}", 0, len(content.splitlines()[0]))
        self.query_one("#details", Static).update(rendered)
        self.update_details_focus()
        self.call_after_refresh(self.update_details_focus)

    def available(self, entry):
        state = self.catalog.availability.get(entry.id, "unknown")
        return not (
            state in {"missing", "type mismatch", "removed"}
            or state.startswith("unavailable")
        )

    def matches(self, entry):
        return self.filter_text in f"{entry.label} {entry.id} {entry.description}".casefold()

    def descendants(self, identity, *, visible=False):
        return [
            entry
            for entry in self.catalog.entries
            if (identity == "root" or entry.group == identity or entry.id == identity)
            and self.available(entry)
            and (not visible or self.matches(entry))
        ]

    def checkbox(self, identity):
        entries = self.descendants(identity)
        count = sum(entry.id in self.selected for entry in entries)
        return "[x]" if entries and count == len(entries) else "[-]" if count else "[ ]"

    def state_item(self, identity):
        return self.workspace_state.get("items", {}).get(identity, {})

    def entry_style(self, entry):
        state = self.state_item(entry.id)
        if entry.id in self.selected and (state.get("ready") or state.get("will_overwrite")):
            return "bold red"
        if state.get("ready"):
            return "green"
        return ""

    def entry_label(self, entry):
        if isinstance(entry.params, RepositoryParams) and entry.label == entry.params.repository:
            owner, name = entry.params.repository.split("/", 1)
            if self.cfg.repo_owner and owner.casefold() == self.cfg.repo_owner.casefold():
                return name
        return entry.label

    def entry_text(self, entry, index, interaction, suffix):
        text = Text(f"{'[x]' if entry.id in self.selected else '[ ]'} {index}. ")
        text.append(self.entry_label(entry), style=self.entry_style(entry))
        if interaction:
            text.append(interaction, style="dim")
        if suffix:
            text.append(suffix, style="dim")
        return text

    def rebuild(self):
        tree = self.query_one(SetupTree)
        previous = tree.cursor_node.data if tree.cursor_node else "root"
        expanded = {key: node.is_expanded for key, node in self.nodes.items()}
        tree.clear()
        tree.root.set_label(Text(f"{self.checkbox('root')} Setup"))
        if expanded.get("root", True):
            tree.root.expand()
        else:
            tree.root.collapse()
        self.nodes = {"root": tree.root}
        for group in self.cfg.setup.groups:
            extra = (
                f" — {self.catalog.repository_error}"
                if group.id == "repo" and self.catalog.repository_error
                else ""
            )
            entries = [entry for entry in self.catalog.entries if entry.group == group.id]
            count = sum(entry.id in self.selected for entry in entries)
            node = tree.root.add(
                Text(f"{self.checkbox(group.id)} {group.label}  {count}/{len(entries)}{extra}"),
                data=group.id,
                expand=expanded.get(group.id, group.id != "repo"),
            )
            self.nodes[group.id] = node
            for index, entry in enumerate(entries, 1):
                if not self.matches(entry):
                    continue
                availability = self.catalog.availability.get(entry.id, "unknown")
                interaction = (
                    " — interactive"
                    if isinstance(entry.params, ApplicationParams)
                    and entry.params.interaction == "interactive"
                    else ""
                )
                suffix = "" if self.available(entry) else f" — {availability}"
                if entry.id in self.action_states:
                    suffix += " — " + self.action_states[entry.id]
                self.nodes[entry.id] = node.add_leaf(
                    self.entry_text(entry, index, interaction, suffix), data=entry.id
                )
        self._rebuild_generation += 1
        generation = self._rebuild_generation
        expected_cursor = tree.cursor_node.data if tree.cursor_node else None
        self.call_after_refresh(self.restore_cursor, previous, expected_cursor, generation)
        self.update_counts()

    def restore_cursor(self, identity, expected_cursor=None, generation=None):
        if generation is not None and generation != self._rebuild_generation:
            return
        tree = self.query_one(SetupTree)
        current = tree.cursor_node.data if tree.cursor_node else None
        if expected_cursor is not None and current != expected_cursor:
            # A user or test moved the cursor before deferred restoration ran.
            # Never overwrite that newer interaction with stale rebuild state.
            if current is not None:
                self.show_details(current)
            return
        node = self.nodes.get(identity, self.nodes["root"])
        ancestor = node.parent
        while ancestor:
            if not ancestor.is_expanded:
                node = ancestor
            ancestor = ancestor.parent
        tree.move_cursor(node)
        self.show_details(node.data)

    def update_counts(self):
        self.update_review_action()
        counts = ", ".join(
            f"{group.label}: {sum(entry.id in self.selected and entry.group == group.id for entry in self.catalog.entries)}"
            for group in self.cfg.setup.groups
        )
        hidden = sum(
            entry.id in self.selected and not self.matches(entry)
            for entry in self.catalog.entries
        )
        summary = self.query_one("#counts", Static)
        summary.set_class(hidden > 0, "has-hidden")
        summary.update(
            f"{counts} | Hidden selected: {hidden}"
            + (" | Bulk selection: visible matches only" if self.filter_text else "")
        )

    def update_review_action(self):
        action = self.query_one("#review", CompactAction)
        action.disabled = self.busy
        action.set_pending(bool(self.selected) and not self.busy)

    def toggle_node(self, node, *, select=False):
        if not node or self.busy:
            return
        identity = node.data
        if not select and not node.children and identity in self.selected:
            self.selected.remove(identity)
            self.rebuild()
            return
        if select and node.parent and not node.children:
            identity = node.parent.data
        items = self.descendants(identity, visible=bool(self.filter_text))
        ids = {entry.id for entry in items}
        if not select and ids and ids <= self.selected:
            self.selected -= ids
        else:
            self.selected |= ids
        self.rebuild()

    def on_input_changed(self, event: Input.Changed):
        self.filter_text = event.value.casefold()
        self.rebuild()

    def on_tree_node_highlighted(self, event: Tree.NodeHighlighted):
        self.show_details(event.node.data)

    def details(self, identity):
        entry = next((entry for entry in self.catalog.entries if entry.id == identity), None)
        if not entry:
            group = next((group for group in self.cfg.setup.groups if group.id == identity), None)
            lines = [group.label if group else "Workspace setup"]
            if group:
                lines += [group.description]
            if identity == "repo":
                sorts = {"recent": "Recent first", "created": "Newest created first", "name": "Name"}
                lines += [
                    f"Owner: {self.cfg.repo_owner or 'not configured'}",
                    f"Sort: {sorts.get(self.cfg.repo_sort, self.cfg.repo_sort)}",
                ]
                if self.catalog.repository_error:
                    lines += [self.catalog.repository_error]
            lines += [
                "",
                "Green: detected ready. Red: selected update/reapply or overwrite risk.",
                "Space or checkbox click toggles items.",
                "Click a name to view details without selecting.",
                "Click an arrow or use Left/Right to collapse/expand.",
                "0 selects the branch; a filter scopes bulk actions to visible matches.",
                "Tab / Shift+Tab moves focus. PageUp/PageDown or the mouse wheel scrolls details.",
                "F5 refreshes guest state through the existing SSH session.",
                "Ctrl+R refreshes the catalog and guest state.",
                "Enter reviews selected actions; confirmation is required before writes.",
            ]
            lines += ["", f"State registry: {self.workspace_state.get('state_path')}"]
            if self.catalog.snapshot_id:
                lines += ["Catalog: " + self.catalog.snapshot_id]
            return "\n".join(lines)

        p = entry.params
        live = self.state_item(entry.id)
        status = (
            "UPDATE"
            if live.get("ready") or live.get("will_overwrite")
            else "UNKNOWN"
            if not live or live.get("state", "unknown").startswith("unknown")
            else "INSTALL"
        )
        lines = [
            "Status: " + status,
            "",
            entry.label,
            entry.description,
            "",
            "Availability: " + self.catalog.availability.get(entry.id, "unknown"),
            "Workspace state: " + live.get("state", "unknown"),
        ]
        if self.workspace_state.get("checked_at"):
            lines += ["State checked: " + self.workspace_state["checked_at"]]
        if live.get("first_managed_at"):
            lines += ["First managed: " + live["first_managed_at"]]
        if live.get("installed_at"):
            lines += ["Installed by HomeStack: " + live["installed_at"]]
        if live.get("last_applied_at"):
            lines += ["Last applied: " + live["last_applied_at"]]
        if live.get("last_snapshot"):
            lines += ["Last snapshot: " + live["last_snapshot"]]
        if live.get("detail"):
            lines += ["State detail: " + str(live["detail"])]
        for file in live.get("files", []):
            if isinstance(file, dict) and file.get("exists"):
                sha = str(file.get("sha256") or "")
                lines += [
                    f"Managed path: ~/{file.get('path')}",
                    "SHA-256: " + (sha[:16] + "…" if sha else "unknown"),
                ]
                if file.get("birthtime_ns"):
                    lines += ["Created: " + _mtime_text(file.get("birthtime_ns"))]
                if file.get("size") is not None:
                    lines += [f"Size: {file['size']} bytes"]
                lines += ["Modified: " + _mtime_text(file.get("mtime_ns"))]
        lines += [""]
        lines += ["ID: " + entry.id]

        if isinstance(p, ApplicationParams):
            lines += [
                f"Interpreter: {p.interpreter}",
                f"Interaction: {p.interaction}",
                "Prerequisites: " + (", ".join(p.prerequisites) or "none"),
                "Executable paths:",
                *p.bin_dirs,
                "Backup paths:",
                *(p.backup_paths or ("none declared",)),
            ]
            known = {
                item.params.command
                for item in defaults()
                if isinstance(item.params, ApplicationParams)
            }
            lines += [
                "Command: "
                + (
                    p.command
                    if p.command in known
                    else "[Custom payload withheld: review command in the local configuration; it may contain secrets]"
                )
            ]
        elif isinstance(p, RepositoryParams):
            owner, name = p.repository.split("/", 1)
            lines += [
                f"Owner: {owner}",
                f"Repository: {name}",
                "",
                "Checkout:",
                f"{self.cfg.repo_checkout_root.rstrip('/')}/{name}",
            ]
            timestamps = (self.catalog.timestamps or {}).get(entry.id, {})
            if timestamps:
                lines += [
                    "",
                    f"Created: {timestamps.get('created_at') or 'unknown'}",
                    f"Last push: {timestamps.get('pushed_at') or 'none'}",
                ]
            lines += ["", "Repository: " + p.repository]
        elif isinstance(p, FileParams):
            lines += [
                "Source (desktop):",
                p.path,
                "",
                "Destination (workspace):",
                *["~/" + path for path in write_paths(self.cfg, entry)],
            ]
        else:
            lines += ["Managed paths:", *["~/" + path for path in write_paths(self.cfg, entry)]]

        if entry.depends_on:
            lines += ["", "Requires explicit selection: " + ", ".join(entry.depends_on)]
        return "\n".join(lines)

    def action_filter(self):
        self.query_one(Input).focus()

    def action_review(self):
        if not isinstance(self.focused, Input):
            self.review()

    def action_cancel(self):
        if self.busy:
            self.cancel_requested.set()
            self.notify(
                "Cancellation requested; waiting for the current operation to return. "
                "Interactive installers use Ctrl+C in their terminal."
            )
        elif self.filter_text:
            self.query_one(Input).value = ""
            self.query_one(SetupTree).focus()
        else:
            self.exit(self.result)

    def on_compact_action_pressed(self, event: CompactAction.Pressed):
        event.stop()
        if event.control.id == "cancel":
            self.action_cancel()
        elif event.control.id == "review":
            self.review()

    def review(self):
        if self.busy:
            return
        if not self.selected:
            self.notify("No setup actions selected.")
            return
        selected = tuple(entry for entry in self.catalog.entries if entry.id in self.selected)
        if any(not self.available(entry) for entry in selected):
            self.notify(
                "A selected entry was removed or is unavailable. Deselect it before applying.",
                severity="error",
            )
            return
        self.prepare_review(selected, self.catalog)

    @work(thread=True, exclusive=True, group="review")
    def prepare_review(self, selected, catalog):
        try:
            plan = build_plan(
                self.cfg,
                self.target,
                selected,
                catalog_id=catalog.snapshot_id,
            )
        except AppError as exc:
            if self.is_running and not get_current_worker().is_cancelled:
                self.call_from_thread(self.notify, str(exc), severity="error")
            return
        if self.is_running and not get_current_worker().is_cancelled:
            self.call_from_thread(self.show_review, plan, catalog)

    def show_review(self, plan, catalog):
        if self.busy or self.catalog is not catalog or {entry.id for entry in plan.entries} != self.selected:
            self.notify("Selection changed while preparing the plan. Review the current selection again.")
            return
        self.pending_plan = plan
        lines = [
            f"Apply to {self.target['name']} (VM {self.target['vmid']})?",
            "All selected actions will be preflighted before writes. Existing declared configuration is snapshotted before overwrite.",
            "",
        ]
        for entry in self.pending_plan.entries:
            live = self.state_item(entry.id)
            action = (
                "Update / reapply existing state"
                if live.get("ready")
                else "Overwrite existing configuration"
                if live.get("will_overwrite")
                else "Install / configure"
            )
            lines += [
                f"{entry.group}: {entry.label} ({entry.id})"
                + (" [hidden by filter]" if not self.matches(entry) else ""),
                entry.description,
                "Action: " + action,
                "Requires selection: " + (", ".join(entry.depends_on) or "none"),
            ]
            if (
                isinstance(entry.params, ApplicationParams)
                and live.get("ready")
                and not entry.params.backup_paths
            ):
                lines += [
                    "WARNING: no application backup_paths are declared; external installer changes outside HomeStack-managed files cannot be restored."
                ]
            lines += [""]
        self.push_screen(Review("\n".join(lines), apply=True), self.review_answer)

    def review_answer(self, approved):
        if approved:
            self.busy = True
            self.update_review_action()
            self.cancel_requested.clear()
            self.execute()

    def terminal(self, operation):
        return self.call_from_thread(self.terminal_on_main, operation)

    def terminal_on_main(self, operation):
        error = None
        with self.suspend():
            try:
                result = operation()
            except BaseException as exc:
                error = exc
        if isinstance(error, KeyboardInterrupt):
            raise AppError("Cancelled; remaining actions were not run") from error
        if error is not None:
            raise error
        return result

    @work(thread=True, exclusive=True, group="execution")
    def execute(self):
        def progress(identity, state):
            if state in {"checking", "running"} and self.cancel_requested.is_set():
                raise AppError("Cancelled; remaining actions were not run")
            self.call_from_thread(self.update_progress, identity, state)

        kwargs = {"terminal": self.terminal, "progress": progress}
        if self.workspace is not None:
            kwargs["workspace"] = self.workspace
        result = self.executor(self.cfg, self.pending_plan, **kwargs)
        state = self.workspace_state
        if self.workspace is not None:
            try:
                state = inspect_workspace_state(
                    self.workspace, self.cfg, self.target, self.catalog.entries
                )
            except AppError as exc:
                self.call_from_thread(
                    self.notify, f"State refresh failed: {exc}", severity="warning"
                )
        self.call_from_thread(self.finished, result, state)

    def update_progress(self, identity, state):
        self.action_states[identity] = state
        self.ssh_status = "connected"
        self.query_one("#target", Static).update(self.target_label(f"{identity}: {state}"))
        self.rebuild()

    def finished(self, result, state=None):
        self.result = result
        self.busy = False
        if state is not None:
            self.workspace_state = state
        self.action_states = {item["id"]: item["status"] for item in result["results"]}
        completed = {
            item["id"]
            for item in result["results"]
            if item["status"] in {"succeeded", "already-ready"}
        }
        self.selected -= completed
        self.ssh_status = "connected" if self.workspace is not None else "disconnected"
        self.rebuild()
        self.query_one("#target", Static).update(self.target_label())
        lines = ["Setup finished" if result["ok"] else "Setup stopped", ""]
        if result.get("snapshot", {}).get("created"):
            lines += ["Snapshot: " + result["snapshot"]["path"], ""]
        lines += [
            f"{item['label']}: {item['status']} — {item['detail']}"
            for item in result["results"]
        ]
        self.push_screen(Review("\n".join(lines), apply=False))

    def action_refresh_state(self):
        if not self.busy and self.workspace is not None:
            self.refresh_state()

    @work(thread=True, exclusive=True, group="state")
    def refresh_state(self):
        worker = get_current_worker()
        try:
            state = inspect_workspace_state(
                self.workspace, self.cfg, self.target, self.catalog.entries
            )
        except AppError as exc:
            if not worker.is_cancelled and self.is_running:
                self.call_from_thread(
                    self.notify, f"State refresh failed: {exc}", severity="error"
                )
            return
        if not worker.is_cancelled and self.is_running:
            self.call_from_thread(self.replace_state, state)

    def replace_state(self, state):
        self.workspace_state = state
        self.ssh_status = "connected"
        self.rebuild()
        self.query_one("#target", Static).update(self.target_label())

    def action_refresh_catalog(self):
        if not self.busy:
            self.refresh_catalog()

    @work(thread=True, exclusive=True, group="catalog")
    def refresh_catalog(self):
        worker = get_current_worker()
        catalog = self.loader(self.cfg, repositories=True)
        state = self.workspace_state
        if self.workspace is not None:
            state = inspect_workspace_state(
                self.workspace, self.cfg, self.target, catalog.entries
            )
        if not worker.is_cancelled and self.is_running:
            self.call_from_thread(self.replace_catalog, catalog, state)

    def save_displayed_catalog(self, catalog):
        if catalog is not self.catalog or not self.is_running:
            return
        try:
            save_snapshot(self.cfg, catalog)
        except OSError:
            self.notify(
                "Catalog snapshot could not be saved; stable-ID selection remains available",
                severity="warning",
            )
        else:
            self.query_one("#target", Static).update(self.target_label())

    def replace_catalog(self, catalog: Catalog, state=None):
        if self.busy:
            return
        current_ids = {entry.id for entry in catalog.entries}
        removed = [
            entry
            for entry in self.catalog.entries
            if entry.id in self.selected and entry.id not in current_ids
        ]
        catalog.entries += tuple(removed)
        for entry in removed:
            catalog.availability[entry.id] = "removed"
        self.catalog = catalog
        if state is not None:
            self.workspace_state = state
        self.rebuild()
        self.call_after_refresh(self.save_displayed_catalog, catalog)
        if removed:
            self.notify(
                "Selected entries were removed. They remain selected and block review until deselected.",
                severity="warning",
            )
