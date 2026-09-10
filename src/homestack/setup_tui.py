"""Mouse/keyboard catalog selection and review using the shared setup engine."""
from __future__ import annotations

import threading

from rich.style import Style
from rich.text import Text
from textual import events, work
from textual.app import App, ComposeResult
from textual.binding import Binding
from textual.containers import Container, Horizontal, VerticalScroll
from textual.screen import ModalScreen
from textual.widgets import Button, Footer, Input, Static, Tree
from textual.worker import get_current_worker

from .models import AppError
from .setup import build_plan, execute_plan, write_paths
from .setup_catalog import Catalog, load_catalog, save_snapshot
from .setup_config import ApplicationParams, FileParams, RepositoryParams, defaults


class SetupTree(Tree[str]):
    auto_expand = False
    BINDINGS = [Binding("space", "check", "Toggle"), Binding("0", "select_branch", "Select branch"),
                Binding("left", "collapse", "Collapse"), Binding("right", "expand", "Expand"),
                Binding("enter", "details", "Details")]

    def render_label(self, node, base_style, style):
        label = super().render_label(node, base_style, style)
        # Tag only the checkbox, not the disclosure arrow or the item label.
        # Rendered metadata follows scrolling and variable-width Unicode text.
        prefix = len(self.ICON_NODE_EXPANDED if node.is_expanded else self.ICON_NODE) if node.allow_expand else 0
        label.stylize(Style(meta={"setup_checkbox": node.data}), prefix, prefix + 3)
        return label

    async def _on_click(self, event: events.Click) -> None:
        # Textual also dispatches inherited handlers unless explicitly prevented.
        # Delegating only non-checkbox clicks avoids double toggles and preserves
        # the Tree's native label/disclosure-arrow behavior.
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

    def action_details(self):
        self.app.action_details()


class Review(ModalScreen[bool]):
    BINDINGS = [("escape", "back", "Back")]
    CSS = "Review { align: center middle; } #review-box { width: 90%; height: 85%; border: solid $accent; background: $surface; padding: 1 2; } #review-actions { height: 3; }"

    def __init__(self, text: str, *, apply: bool):
        super().__init__()
        self.text = text
        self.apply = apply

    def compose(self) -> ComposeResult:
        with VerticalScroll(id="review-box"):
            yield Static(self.text, markup=False)
            with Horizontal(id="review-actions"):
                yield Button("Back", id="back")
                if self.apply:
                    yield Button("Apply this plan", variant="primary", id="apply")

    def on_button_pressed(self, event: Button.Pressed):
        self.dismiss(event.button.id == "apply")

    def action_back(self):
        self.dismiss(False)


class SetupApp(App):
    """Selection identity is independent of rendered rows and filtering."""
    CSS = """
    #target, #counts, #keys { height: auto; padding: 0 1; }
    #counts.has-hidden { color: $warning; text-style: bold; }
    #filter { height: 3; }
    #browser { height: 1fr; layout: vertical; }
    #tree { height: 1fr; width: 1fr; border: solid $panel; }
    #tree:focus { border: solid $accent; }
    #details-pane { height: 8; max-height: 40%; width: 1fr; border: solid $panel; padding: 0 1; }
    #details-pane:focus { border: solid $accent; }
    #details { height: auto; }
    #browser.wide { layout: horizontal; }
    #browser.wide #tree { width: 3fr; height: 1fr; }
    #browser.wide #details-pane { width: 2fr; height: 1fr; max-height: 100%; }
    #controls { height: 3; }
    """
    BINDINGS = [("/", "filter", "Filter"), ("d", "details", "Full details"),
                ("D", "details", "Full details"), ("escape", "cancel", "Cancel"),
                ("ctrl+r", "refresh_catalog", "Refresh catalog"),
                Binding("ctrl+c", "cancel", "Cancel", priority=True)]

    def __init__(self, cfg, target, *, catalog=None, loader=load_catalog, executor=execute_plan):
        super().__init__()
        self.cfg, self.target = cfg, target
        self.catalog = catalog or load_catalog(cfg)
        self.loader, self.executor = loader, executor
        self.selected: set[str] = set()
        self.filter_text = ""
        self.nodes = {}
        self.busy = False
        self.cancel_requested = threading.Event()
        self.result = None
        self.action_states = {}
        self.pending_plan = None
        self.details_identity = None
        self.ssh_status = "not connected"

    def compose(self) -> ComposeResult:
        yield Static(self.target_label(), id="target", markup=False)
        yield Input(placeholder="Filter (/). Bulk selection affects visible matches only.", id="filter")
        with Container(id="browser"):
            yield SetupTree(Text("[ ] Setup"), data="root", id="tree")
            with VerticalScroll(id="details-pane", can_focus=False):
                yield Static("Highlight an item for details. D opens full details. Enter never applies.", id="details", markup=False)
        yield Static("Nothing selected", id="counts", markup=False)
        with Horizontal(id="controls"):
            yield Button("Review & apply", id="review", variant="primary")
            yield Button("Cancel", id="cancel")
        yield Static("↑↓ Move  ←→ Expand/collapse  Space Toggle  Tab Focus  / Filter", id="keys", markup=False)
        yield Footer()

    def target_label(self, progress=""):
        parts = [f"Workspace: {self.target['name']} (VM {self.target['vmid']})"]
        if self.target.get("ip"):
            parts.append(str(self.target["ip"]))
        parts.append(f"SSH: {self.ssh_status}")
        if self.catalog.snapshot_id:
            parts.append(f"Catalog: {self.catalog.snapshot_id[:8]}")
        if progress:
            parts.append(progress)
        return "    ".join(parts)

    def on_mount(self):
        # Content wrapping may settle in a later layout pass than Static.update.
        self.watch(self.query_one("#details-pane"), "virtual_size",
                   lambda: self.call_after_refresh(self.update_details_focus))
        self.update_layout()
        self.rebuild()
        self.query_one(SetupTree).focus()
        self.action_refresh_catalog()

    def on_resize(self, event: events.Resize):
        self.update_layout(event.size.width)

    def update_layout(self, width=None):
        self.query_one("#browser").set_class((self.size.width if width is None else width) >= 110, "wide")
        self.call_after_refresh(self.update_details_focus)

    def update_details_focus(self):
        pane = self.query_one("#details-pane", VerticalScroll)
        pane.can_focus = pane.max_scroll_y > 0
        if not pane.can_focus and pane.has_focus:
            self.query_one(SetupTree).focus()

    def show_details(self, identity):
        pane = self.query_one("#details-pane", VerticalScroll)
        if identity != self.details_identity:
            pane.scroll_to(y=0, animate=False, force=True)
        self.details_identity = identity
        self.query_one("#details", Static).update(self.details(identity))
        self.call_after_refresh(self.update_details_focus)

    def available(self, entry):
        state = self.catalog.availability.get(entry.id, "unknown")
        return not (state in {"missing", "type mismatch", "removed"} or state.startswith("unavailable"))

    def matches(self, entry):
        return self.filter_text in f"{entry.label} {entry.id} {entry.description}".casefold()

    def descendants(self, identity, *, visible=False):
        return [e for e in self.catalog.entries if (identity == "root" or e.group == identity or e.id == identity)
                and self.available(e) and (not visible or self.matches(e))]

    def checkbox(self, identity):
        entries = self.descendants(identity)
        count = sum(e.id in self.selected for e in entries)
        return "[x]" if entries and count == len(entries) else "[-]" if count else "[ ]"

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
            extra = f" — {self.catalog.repository_error}" if group.id == "repo" and self.catalog.repository_error else ""
            entries = [e for e in self.catalog.entries if e.group == group.id]
            count = sum(e.id in self.selected for e in entries)
            node = tree.root.add(Text(f"{self.checkbox(group.id)} {group.label}  {count}/{len(entries)}{extra}"), data=group.id, expand=expanded.get(group.id, group.id != "repo"))
            self.nodes[group.id] = node
            for index, entry in enumerate((e for e in self.catalog.entries if e.group == group.id), 1):
                if not self.matches(entry):
                    continue
                state = self.catalog.availability.get(entry.id, "unknown")
                interaction = " — interactive" if isinstance(entry.params, ApplicationParams) and entry.params.interaction == "interactive" else ""
                suffix = "" if self.available(entry) else f" — {state}"
                if entry.id in self.action_states:
                    suffix += " — " + self.action_states[entry.id]
                label = f"{'[x]' if entry.id in self.selected else '[ ]'} {index}. {self.entry_label(entry)}{interaction}{suffix}"
                self.nodes[entry.id] = node.add_leaf(Text(label), data=entry.id)
        self.call_after_refresh(self.restore_cursor, previous)
        self.update_counts()

    def restore_cursor(self, identity):
        node = self.nodes.get(identity, self.nodes["root"])
        # Never leave the cursor pointing into a collapsed branch.
        ancestor = node.parent
        while ancestor:
            if not ancestor.is_expanded:
                node = ancestor
            ancestor = ancestor.parent
        self.query_one(SetupTree).move_cursor(node)
        self.show_details(node.data)

    def entry_label(self, entry):
        if isinstance(entry.params, RepositoryParams) and entry.label == entry.params.repository:
            owner, name = entry.params.repository.split("/", 1)
            if self.cfg.repo_owner and owner.casefold() == self.cfg.repo_owner.casefold():
                return name
        return entry.label

    def update_counts(self):
        counts = ", ".join(f"{g.label}: {sum(e.id in self.selected and e.group == g.id for e in self.catalog.entries)}" for g in self.cfg.setup.groups)
        hidden = sum(e.id in self.selected and not self.matches(e) for e in self.catalog.entries)
        summary = self.query_one("#counts", Static)
        summary.set_class(hidden > 0, "has-hidden")
        summary.update(f"{counts} | Hidden selected: {hidden}" + (" | Bulk selection: visible matches only" if self.filter_text else ""))

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
        ids = {e.id for e in items}
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

    def details(self, identity, *, full=False):
        entry = next((e for e in self.catalog.entries if e.id == identity), None)
        if not entry:
            group = next((g for g in self.cfg.setup.groups if g.id == identity), None)
            lines = [group.label if group else "Workspace setup"]
            if group:
                lines += [group.description]
            if identity == "repo":
                sort = {"recent": "Recent first", "created": "Newest created first", "name": "Name"}
                lines += [f"Owner: {self.cfg.repo_owner or 'not configured'}", f"Sort: {sort.get(self.cfg.repo_sort, self.cfg.repo_sort)}"]
                if self.catalog.repository_error:
                    lines += [self.catalog.repository_error]
            lines += ["", "Space or checkbox click toggles items.", "Click a name to view details without selecting.",
                      "Click an arrow or use Left/Right to collapse/expand.", "0 selects the branch; a filter scopes bulk actions to visible matches.",
                      "Tab / Shift+Tab moves focus. PageUp/PageDown or the mouse wheel scrolls details.", "D / Enter opens full details; never applies."]
            if full and self.catalog.snapshot_id:
                lines += ["", "Catalog: " + self.catalog.snapshot_id]
            return "\n".join(lines)
        p = entry.params
        lines = [entry.label, entry.description, "", "Availability: " + self.catalog.availability.get(entry.id, "unknown"),
                 "Guest state: unknown until approved preflight", ""]
        if full:
            lines += ["ID: " + entry.id]
        if isinstance(p, ApplicationParams):
            lines += [f"Interpreter: {p.interpreter}", f"Interaction: {p.interaction}", "Prerequisites: " + (", ".join(p.prerequisites) or "none"),
                      "Executable paths:", *p.bin_dirs]
            if full:
                known = {e.params.command for e in defaults() if isinstance(e.params, ApplicationParams)}
                lines += ["Command: " + (p.command if p.command in known else "[Custom payload withheld: review command in the local configuration; it may contain secrets]")]
        elif isinstance(p, RepositoryParams):
            owner, name = p.repository.split("/", 1)
            lines += [f"Owner: {owner}", f"Repository: {name}", "", "Checkout:", f"{self.cfg.repo_checkout_root.rstrip('/')}/{name}"]
            timestamps = (self.catalog.timestamps or {}).get(entry.id, {})
            if timestamps:
                lines += ["", f"Created: {timestamps.get('created_at') or 'unknown'}", f"Last push: {timestamps.get('pushed_at') or 'none'}"]
            if full:
                paths = write_paths(self.cfg, entry)
                lines += ["", "Managed paths:", *("~/" + path for path in paths)]
        elif isinstance(p, FileParams):
            lines += ["Source (desktop):", p.path, "", "Destination (workspace):", *["~/" + path for path in write_paths(self.cfg, entry)]]
        else:
            lines += ["Managed paths:", *["~/" + path for path in write_paths(self.cfg, entry)]]
        if entry.depends_on:
            lines += ["", "Requires explicit selection: " + ", ".join(entry.depends_on)]
        return "\n".join(lines)

    def action_filter(self):
        self.query_one(Input).focus()

    def action_details(self):
        node = self.query_one(SetupTree).cursor_node
        if node:
            self.push_screen(Review(self.details(node.data, full=True), apply=False))

    def action_cancel(self):
        if self.busy:
            self.cancel_requested.set()
            self.notify("Cancellation requested; waiting for the current operation to return. Interactive installers use Ctrl+C in their terminal.")
        elif self.filter_text:
            self.query_one(Input).value = ""
            self.query_one(SetupTree).focus()
        else:
            self.exit(self.result)

    def on_button_pressed(self, event: Button.Pressed):
        if event.button.id == "cancel":
            self.action_cancel()
        elif event.button.id == "review" and not self.busy:
            self.review()

    def review(self):
        selected = tuple(e for e in self.catalog.entries if e.id in self.selected)
        if any(not self.available(e) for e in selected):
            self.notify("A selected entry was removed or is unavailable. Deselect it before applying.", severity="error")
            return
        self.prepare_review(selected, self.catalog)

    @work(thread=True, exclusive=True, group="review")
    def prepare_review(self, selected, catalog):
        # Selected directory validation may scan a large desktop tree.
        try:
            plan = build_plan(self.cfg, self.target, selected, catalog_id=catalog.snapshot_id)
        except AppError as exc:
            if self.is_running and not get_current_worker().is_cancelled:
                self.call_from_thread(self.notify, str(exc), severity="error")
            return
        if self.is_running and not get_current_worker().is_cancelled:
            self.call_from_thread(self.show_review, plan, catalog)

    def show_review(self, plan, catalog):
        if self.busy or self.catalog is not catalog or {e.id for e in plan.entries} != self.selected:
            self.notify("Selection changed while preparing the plan. Review the current selection again.")
            return
        self.pending_plan = plan
        lines = [f"Apply to {self.target['name']} (VM {self.target['vmid']})?", "Guest state is unknown. All selected actions will be preflighted before writes.", ""]
        for e in self.pending_plan.entries:
            lines += [f"{e.group}: {e.label} ({e.id})" + (" [hidden by filter]" if not self.matches(e) else ""),
                      e.description, "Requires selection: " + (", ".join(e.depends_on) or "none"), ""]
        self.push_screen(Review("\n".join(lines), apply=True), self.review_answer)

    def review_answer(self, approved):
        if approved:
            self.busy = True
            self.cancel_requested.clear()
            self.execute()

    def terminal(self, operation):
        # Driver signal handlers require the main thread. While suspended, the
        # operation owns the terminal and the UI event loop intentionally waits.
        return self.call_from_thread(self.terminal_on_main, operation)

    def terminal_on_main(self, operation):
        # Textual 8's suspend context must exit normally to restore the driver.
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
        result = self.executor(self.cfg, self.pending_plan, terminal=self.terminal, progress=progress)
        self.call_from_thread(self.finished, result)

    def update_progress(self, identity, state):
        self.action_states[identity] = state
        self.ssh_status = "connected"
        self.query_one("#target", Static).update(self.target_label(f"{identity}: {state}"))
        self.rebuild()

    def finished(self, result):
        self.result = result
        self.busy = False
        self.action_states = {r["id"]: r["status"] for r in result["results"]}
        self.rebuild()
        self.ssh_status = "disconnected"
        self.query_one("#target", Static).update(self.target_label())
        lines = ["Setup finished" if result["ok"] else "Setup stopped", ""]
        lines += [f"{r['label']}: {r['status']} — {r['detail']}" for r in result["results"]]
        self.push_screen(Review("\n".join(lines), apply=False))

    def action_refresh_catalog(self):
        if not self.busy:
            self.refresh_catalog()

    @work(thread=True, exclusive=True, group="catalog")
    def refresh_catalog(self):
        worker = get_current_worker()
        catalog = self.loader(self.cfg, repositories=True)
        if not worker.is_cancelled and self.is_running:
            self.call_from_thread(self.replace_catalog, catalog)

    def save_displayed_catalog(self, catalog):
        if catalog is not self.catalog or not self.is_running:
            return
        try:
            save_snapshot(self.cfg, catalog)
        except OSError:
            self.notify("Catalog snapshot could not be saved; stable-ID selection remains available", severity="warning")
        else:
            self.query_one("#target", Static).update(self.target_label())

    def replace_catalog(self, catalog: Catalog):
        if self.busy:
            return
        current_ids = {e.id for e in catalog.entries}
        removed = [e for e in self.catalog.entries if e.id in self.selected and e.id not in current_ids]
        catalog.entries += tuple(removed)
        for entry in removed:
            catalog.availability[entry.id] = "removed"
        self.catalog = catalog
        self.rebuild()
        self.call_after_refresh(self.save_displayed_catalog, catalog)
        if removed:
            self.notify("Selected entries were removed. They remain selected and block review until deselected.", severity="warning")
