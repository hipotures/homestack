//! Drawing the main screen (mockups 09–11, 15–17) from an [`App`].

use crossterm::event::KeyCode;
use hs_model::status::{LayoutCapacity, StatusEntry};
use ratatui::Frame;
use ratatui::buffer::Buffer;
use ratatui::layout::{Position, Rect};
use ratatui::style::{Color, Modifier, Style};

use super::action::Action;
use super::chrome::{Control, Hit, Scrolls, Sides, alert_style, frame, register, scrollbar, text_style};
use super::layout::{MIN_HEIGHT, MIN_WIDTH, arrange};
use super::state::{App, MENU, Overlay, VIEWS};
use super::theme::{Palette, THEMES};

/// The screen at `width` x `height`, for tests and snapshots.
#[must_use]
pub fn render(app: &App, width: u16, height: u16) -> Buffer {
    let area = Rect::new(0, 0, width, height);
    let mut buf = Buffer::empty(area);
    draw_into(&mut buf, area, app);
    buf
}

/// Draw `app` on the terminal frame.
pub fn draw(frame: &mut Frame, app: &App) {
    let area = frame.area();
    draw_into(frame.buffer_mut(), area, app);
}

/// Write `text` from `x`, at most `width` cells; the cells written.
pub(super) fn write(buf: &mut Buffer, x: u16, y: u16, width: u16, text: &str, style: Style) -> u16 {
    let mut used = 0;
    for c in text.chars() {
        if used >= width {
            break;
        }
        let at = Position { x: x + used, y };
        if buf.area.contains(at) {
            let cell = &mut buf[(at.x, at.y)];
            // A control character (a newline from a server's text) cannot be drawn.
            cell.set_char(if c.is_control() { ' ' } else { c });
            cell.set_style(style);
        }
        used += 1;
    }
    used
}

/// `text` cut to the available number of characters.
pub(super) fn cut(text: &str, width: usize) -> String {
    text.chars().take(width).collect()
}

fn pad(text: &str, width: usize) -> String {
    let short = cut(text, width);
    format!("{short:<width$}")
}

fn pad_right(text: &str, width: usize) -> String {
    let short = cut(text, width);
    format!("{short:>width$}")
}

/// Binary size, short: `1.7T`, `546G`, `8G`.
fn size(bytes: u64) -> String {
    const GIB: f64 = 1_073_741_824.0;
    #[allow(clippy::cast_precision_loss)]
    let gib = bytes as f64 / GIB;
    if gib >= 1024.0 {
        format!("{:.1}T", gib / 1024.0)
    } else {
        format!("{gib:.0}G")
    }
}

pub(super) fn bar(width: usize, percent: f64) -> String {
    #[allow(
        clippy::cast_possible_truncation,
        clippy::cast_sign_loss,
        clippy::cast_precision_loss
    )]
    let full = ((percent.clamp(0.0, 100.0) / 100.0) * width as f64).round() as usize;
    format!("{}{}", "█".repeat(full.min(width)), "░".repeat(width - full.min(width)))
}

fn short_host(url: &str) -> String {
    url.split("://")
        .nth(1)
        .unwrap_or(url)
        .split(['/', ':'])
        .next()
        .unwrap_or(url)
        .to_owned()
}

fn draw_into(buf: &mut Buffer, area: Rect, app: &App) {
    super::chrome::clear_hits();
    let palette = &app.palette;
    let base = text_style(palette);
    for y in area.top()..area.bottom() {
        for x in area.left()..area.right() {
            let cell = &mut buf[(x, y)];
            cell.set_char(' ');
            cell.set_style(base);
        }
    }
    let screen = arrange(area, app.shown);
    if screen.too_small {
        too_small(buf, area, palette);
        return;
    }
    if let Some(install) = &app.install {
        super::draw_install::screen(buf, area, app, install);
        if let Some(asking) = &app.asking {
            super::draw_install::asking(buf, area, app, asking);
        }
        return;
    }
    // ┐menu┌ and the preset go to the first panel shown.
    let host = if screen.cluster.is_some() {
        2
    } else if screen.workspaces.is_some() {
        1
    } else if screen.storage.is_some() {
        3
    } else {
        4
    };
    let mut menu = vec![
        Control::plain(&format!("profile: {}", app.profile)),
        Control::key("&menu"),
        Control::key(&format!("&view {}", VIEWS[app.view].0)),
    ];
    if let Some(label) = app.api_update_label() {
        menu.push(Control::plain(&label));
    }
    let host_controls = |digit: u8| if digit == host { menu.clone() } else { Vec::new() };
    if let Some(rect) = screen.cluster {
        cluster(buf, rect, app, host_controls(2));
    }
    if let Some(rect) = screen.workspaces {
        workspaces(buf, rect, app, host_controls(1));
    }
    if let Some(rect) = screen.storage {
        storage(buf, rect, app, host_controls(3));
    }
    if let Some(rect) = screen.log {
        log(buf, rect, app, host_controls(4));
    }
    if let Some(overlay) = &app.overlay {
        draw_overlay(buf, area, app, overlay);
    }
    // ssh's question (the touch of the setup's connection) over the screen.
    if let Some(asking) = &app.asking {
        super::draw_install::asking(buf, area, app, asking);
    }
}

fn too_small(buf: &mut Buffer, area: Rect, palette: &Palette) {
    let inner = frame(
        buf,
        area,
        palette,
        &Sides {
            top_left: vec![Control::plain("HomeStack")],
            ..Sides::default()
        },
    );
    let lines = [
        "Terminal too small".to_owned(),
        String::new(),
        format!("Now      {} columns x {} rows", area.width, area.height),
        format!("Needed   {MIN_WIDTH} columns x {MIN_HEIGHT} rows"),
        String::new(),
        "Make the window larger, or press q to quit.".to_owned(),
    ];
    let top = inner.y + inner.height.saturating_sub(6) / 2;
    for (row, text) in lines.iter().enumerate() {
        let width = u16::try_from(text.chars().count()).unwrap_or(0).min(inner.width);
        let x = inner.x + (inner.width - width) / 2;
        let style = if row == 0 {
            text_style(palette).add_modifier(Modifier::BOLD)
        } else {
            text_style(palette)
        };
        write(buf, x, top + u16::try_from(row).unwrap_or(0), inner.width, text, style);
    }
}

fn dot(palette: &Palette, ok: Option<bool>) -> (String, Style) {
    let style = Style::default().bg(palette.background);
    match ok {
        Some(true) => ("●".to_owned(), style.fg(palette.ok)),
        Some(false) => ("✗".to_owned(), style.fg(palette.error)),
        None => ("?".to_owned(), style.fg(palette.muted)),
    }
}

/// Spans written one after another on one row.
fn spans(buf: &mut Buffer, x: u16, y: u16, width: u16, parts: &[(String, Style)]) {
    let mut at = 0;
    for (text, style) in parts {
        if at >= width {
            break;
        }
        at += write(buf, x + at, y, width - at, text, *style);
    }
}

fn free_percent(app: &App) -> Option<f64> {
    let layouts = &app.status.as_ref()?.summary.storage_layouts;
    let (mut total, mut free) = (0u64, 0u64);
    for layout in layouts {
        total += layout.total_bytes?;
        free += layout.available_bytes?;
    }
    #[allow(clippy::cast_precision_loss)]
    (total > 0).then(|| 100.0 * free as f64 / total as f64)
}

/// `19d 13h`, `5h 12m`, `42m`.
fn uptime(seconds: u64) -> String {
    let (days, hours, minutes) = (seconds / 86_400, seconds / 3_600 % 24, seconds / 60 % 60);
    if days > 0 {
        format!("{days}d {hours}h")
    } else if hours > 0 {
        format!("{hours}h {minutes}m")
    } else {
        format!("{minutes}m")
    }
}

/// `used` of `max` in percent; `None` without both or with `max` 0.
fn ratio(used: Option<u64>, max: Option<u64>) -> Option<f64> {
    let (used, max) = (used?, max?);
    #[allow(clippy::cast_precision_loss)]
    (max > 0).then(|| 100.0 * used as f64 / max as f64)
}

/// The theme's green, yellow from 75 %, red from 90 %.
fn load_color(palette: &Palette, percent: f64) -> Color {
    match percent {
        p if p >= 90.0 => palette.error,
        p if p >= 75.0 => palette.warning,
        _ => palette.ok,
    }
}

/// How a node's load fits the cluster panel: the bar's cells and whether
/// ` of 32` / ` of 125G` follows the percentage.
#[derive(Clone, Copy)]
struct LoadColumns {
    bar: usize,
    total: bool,
}

impl LoadColumns {
    /// Cells of a CPU, RAM or DISK column: `██████░░  94% of 125G`.
    fn width(self) -> usize {
        self.bar + usize::from(self.bar > 0) + 4 + if self.total { 8 } else { 0 }
    }

    /// The widest that fits `room` cells after the node columns: three load
    /// columns and UPTIME, two spaces between them.
    fn fitting(room: usize) -> Self {
        [(10, true), (6, true), (0, true), (0, false)]
            .into_iter()
            .map(|(bar, total)| Self { bar, total })
            .find(|columns| 3 * (columns.width() + 2) + 8 <= room)
            .unwrap_or(Self { bar: 0, total: false })
    }
}

/// One load column for `percent` of `total`; `—` when unknown.
fn load(palette: &Palette, columns: LoadColumns, percent: Option<f64>, total: &str) -> Vec<(String, Style)> {
    let text = text_style(palette);
    let Some(percent) = percent else {
        return vec![(pad("—", columns.width() + 2), text.fg(palette.muted))];
    };
    let mut parts = Vec::new();
    if columns.bar > 0 {
        parts.push((bar(columns.bar, percent), text.fg(load_color(palette, percent))));
        parts.push((" ".to_owned(), text));
    }
    parts.push((format!("{percent:>3.0}%"), text));
    if columns.total {
        parts.push((pad(&format!(" of {total}"), 8), text.fg(palette.muted)));
    }
    parts.push(("  ".to_owned(), text));
    parts
}

fn cluster(buf: &mut Buffer, area: Rect, app: &App, mut controls: Vec<Control>) {
    let palette = &app.palette;
    let mut top_left = vec![Control::panel(2, "cluster")];
    top_left.append(&mut controls);
    let nodes = app
        .status
        .as_ref()
        .map(|status| status.summary.nodes.clone())
        .unwrap_or_default();
    let online = nodes.iter().filter(|node| node.online).count();
    let mut top_right = Vec::new();
    // The quorum, from a server that reads it, on a cluster (not a standalone node).
    if let Some(quorum) = app.status.as_ref().and_then(|status| status.summary.quorum.as_ref()) {
        let name = if quorum.name.is_empty() {
            String::new()
        } else {
            format!("{} · ", quorum.name)
        };
        let count = format!("{online}/{}", quorum.nodes.max(u32::try_from(nodes.len()).unwrap_or(0)));
        top_right.push(if quorum.quorate {
            Control::plain(&format!("{name}quorate ✓ {count}"))
        } else {
            Control::plain(&format!("{name}NO QUORUM ✗ {count}")).raised()
        });
    }
    if !app.clock.is_empty() {
        top_right.push(Control::plain(&app.clock));
    }
    let sides = Sides {
        top_left,
        top_right,
        bottom_right: vec![Control::plain(&format!("{online}/{} nodes", nodes.len()))],
        ..Sides::default()
    };
    let inner = frame(buf, area, palette, &sides);
    let text = text_style(palette);
    let muted = text.fg(palette.muted);
    if inner.height < 4 {
        let mut parts: Vec<(String, Style)> = vec![(" ".to_owned(), text)];
        if app.unreachable.is_empty() {
            for node in &nodes {
                parts.push(dot(palette, Some(node.online)));
                parts.push((format!(" {}  ", node.node), text));
            }
        } else {
            parts.push(("nodes ?".to_owned(), muted));
        }
        if let Some(percent) = free_percent(app) {
            parts.push((format!("   storage {percent:.0}% free"), text));
        }
        spans(buf, inner.x, inner.y, inner.width, &parts);
        return;
    }
    // The table of nodes: who, state, workspaces, then the load as fits.
    let name_width = nodes
        .iter()
        .map(|node| node.node.chars().count())
        .max()
        .unwrap_or(0)
        .clamp(5, 16);
    // " NODE·  ● online   ● 1  ○ 0    "
    let lead = 1 + name_width + 2 + 1 + 10 + 2 + 3 + 2 + 3 + 2;
    let columns = LoadColumns::fitting(usize::from(inner.width).saturating_sub(lead));
    let heading = {
        let cell = columns.width() + 2;
        format!(
            " {}  {}{}{}{}{}UPTIME",
            pad("NODE", name_width),
            pad("STATE", 11),
            pad("WORKSPACES", 12),
            pad("CPU", cell),
            pad("RAM", cell),
            pad("DISK", cell),
        )
    };
    write(buf, inner.x, inner.y, inner.width, &heading, muted);
    let workspaces = app
        .status
        .as_ref()
        .map(|status| status.workspaces.as_slice())
        .unwrap_or_default();
    for (row, node) in nodes.iter().enumerate().take(usize::from(inner.height - 1)) {
        let here: Vec<&StatusEntry> = workspaces.iter().filter(|entry| entry.node == node.node).collect();
        let running = here.iter().filter(|entry| entry.status == "running").count();
        let y = inner.y + 1 + u16::try_from(row).unwrap_or(0);
        let (symbol, style) = dot(palette, Some(node.online));
        let mut parts = vec![
            (format!(" {}  ", pad(&node.node, name_width)), text),
            (symbol, style),
            (format!(" {} ", pad(&node.status, 8)), text),
            // Running and stopped as dots with their counts.
            ("● ".to_owned(), text.fg(palette.ok)),
            (format!("{running:<3}"), text),
            ("○ ".to_owned(), text.fg(palette.muted)),
            (format!("{:<3}  ", here.len() - running), text),
        ];
        let cores = node.maxcpu.map_or_else(String::new, |n| n.to_string());
        parts.extend(load(palette, columns, node.cpu.map(|cpu| 100.0 * cpu), &cores));
        parts.extend(load(
            palette,
            columns,
            ratio(node.mem_bytes, node.maxmem_bytes),
            &node.maxmem_bytes.map_or_else(String::new, size),
        ));
        parts.extend(load(
            palette,
            columns,
            ratio(node.disk_bytes, node.maxdisk_bytes),
            &node.maxdisk_bytes.map_or_else(String::new, size),
        ));
        parts.push(match node.uptime_seconds {
            Some(seconds) => (uptime(seconds), text),
            None => ("—".to_owned(), muted),
        });
        spans(buf, inner.x, y, inner.width, &parts);
    }
}

/// A list column: heading, width and how a row fills it.
struct Column {
    heading: &'static str,
    width: usize,
    right: bool,
    value: fn(&StatusEntry) -> String,
}

/// The list's columns for `width` cells: first, without a heading, a square
/// for each of up to `tag_width` tags; NAME as wide as `name_width`, the
/// longest name, and never narrower than its 12.
fn columns(width: usize, tag_width: usize, name_width: usize) -> Vec<Column> {
    let tags = Column {
        heading: "",
        width: tag_width,
        right: false,
        value: |entry| "■".repeat(entry.tags.len().min(MAX_TAGS)),
    };
    let all = [
        Column {
            heading: "VMID",
            // 101010: the numbering by address (owner, 2026-10-14).
            width: 6,
            right: false,
            value: |entry| entry.vmid.to_string(),
        },
        Column {
            heading: "NAME",
            width: name_width.max(12) + 1,
            right: false,
            value: |entry| format!(" {}", entry.name.as_deref().unwrap_or_default()),
        },
        Column {
            heading: "NODE",
            width: 4,
            right: false,
            value: |entry| entry.node.clone(),
        },
        Column {
            heading: "STATE",
            width: 9,
            right: false,
            value: |entry| {
                if entry.status.starts_with('◐') {
                    // The busy dot under the middle of STATE; the action's
                    // name is in its box.
                    return "  ◐".to_owned();
                }
                let symbol = if entry.status == "running" { "●" } else { "○" };
                // Under the middle of STATE.
                format!("  {symbol}")
            },
        },
        Column {
            heading: "ADDRESS",
            width: 15,
            right: false,
            value: |entry| entry.ip.clone().unwrap_or_else(|| "—".to_owned()),
        },
        Column {
            heading: "HOME",
            width: 5,
            right: true,
            value: |entry| {
                entry
                    .home
                    .as_ref()
                    .and_then(|home| home.size_bytes)
                    .map_or_else(|| "—".to_owned(), size)
            },
        },
        Column {
            heading: "FREE",
            width: 6,
            right: true,
            value: |entry| {
                entry
                    .home
                    .as_ref()
                    .and_then(|home| home.free_percent)
                    .map_or_else(|| "—".to_owned(), |percent| format!("{percent:.0}%"))
            },
        },
        Column {
            heading: "ROOT",
            width: 11,
            right: false,
            value: |entry| format!(" {}", entry.root_storage.clone().unwrap_or_default()),
        },
    ];
    // Columns while they fit; the first that does not fit still comes,
    // narrower, when at least 9 cells are left (an address, cut with "…").
    let mut used = 0;
    let mut chosen = Vec::new();
    if tag_width > 0 {
        used += tag_width + 1;
        chosen.push(tags);
    }
    for mut column in all {
        if used + column.width < width {
            used += column.width + 1;
            chosen.push(column);
            continue;
        }
        let left = width.saturating_sub(used + 1);
        // STATE only draws a dot, so keep it even in the narrow action list.
        let minimum = if column.heading == "STATE" { 3 } else { 9 };
        if left >= minimum {
            column.width = left;
            chosen.push(column);
        }
        break;
    }
    chosen
}

/// The most tags a row shows a square for.
const MAX_TAGS: usize = 5;

/// The colour of a tag's square: HomeStack's own in the theme's colours,
/// any other muted.
fn tag_color(palette: &Palette, tag: &str) -> Color {
    if palette.no_color {
        return Color::Reset;
    }
    match tag {
        "homestack-gold" => palette.warning,
        "homestack-ws" => palette.accent,
        "homestack-alien" => Color::Rgb(187, 154, 247),
        "homestack-server" => palette.value,
        "homestack-lock" => palette.error,
        _ => palette.muted,
    }
}

fn list_line(columns: &[Column], cell: impl Fn(&Column) -> String) -> String {
    let mut line = String::new();
    for column in columns {
        let text = cell(column);
        line.push(' ');
        if column.right {
            line.push_str(&pad_right(&text, column.width));
        } else {
            line.push_str(&pad(&text, column.width));
        }
    }
    line
}

fn workspace_actions(app: &App) -> Vec<Control> {
    let selected = app.selected_entry();
    let running = selected.is_some_and(|entry| entry.status == "running");
    let free = selected.is_some() && app.action.is_none() && app.setup.is_none();
    if selected.is_some_and(super::acting::is_server) {
        return vec![
            Control::key("&info"),
            Control::named("␣", "select", KeyCode::Char(' ')),
            Control::named("↵", "update API", KeyCode::Enter).enabled(free),
        ];
    }
    vec![
        Control::key("&info").enabled(selected.is_some()),
        Control::key(
            if selected.is_some_and(|entry| super::shell::is_gold(app, entry.vmid)) {
                "&ssh root"
            } else {
                "&ssh"
            },
        )
        .enabled(selected.is_some_and(|entry| super::shell::available(app, entry.vmid))),
        Control::key(if running { "s&top" } else { "s&tart" }).enabled(free),
        Control {
            code: Some(KeyCode::Enter),
            ..Control::key("&ent&er")
        }
        .enabled(running && free),
        Control::key("&refresh").enabled(free),
        Control::key("resi&ze").enabled(free),
        Control::key("add&key").enabled(free && (!selected.is_some_and(|entry| entry.role == "ALIEN") || running)),
        Control::key("mi&grate").enabled(free),
        Control::key("&destroy").enabled(free),
    ]
}

/// Label and value pairs of the selected workspace.
fn detail_pairs(app: &App, entry: &StatusEntry) -> Vec<(&'static str, String)> {
    let mut pairs = vec![("", format!("{} on {}", entry.status, entry.node))];
    if let Some(image) = &entry.image {
        let tag = |image: &str| image.rsplit_once(':').map_or(image, |(_, tag)| tag).to_owned();
        pairs.push(("address", entry.ip.clone().unwrap_or_else(|| "—".to_owned())));
        pairs.push(("image", tag(image)));
        if let Some(update) = &entry.image_update {
            let state = match update.state {
                hs_model::status::ImageUpdateState::Checking => "checking",
                hs_model::status::ImageUpdateState::UpToDate => "up to date",
                hs_model::status::ImageUpdateState::Available => "available",
                hs_model::status::ImageUpdateState::Unknown => "unknown",
            };
            pairs.push((
                "update",
                update.available.as_deref().map_or_else(|| state.to_owned(), tag),
            ));
            if let Some(checked) = &update.checked_at {
                let checked = super::reinstall::local_time(checked, 0)
                    .map_or_else(|| checked.clone(), |time| format!("{time} UTC"));
                pairs.push(("checked", checked));
            }
            if let Some(error) = &update.error {
                pairs.push(("error", error.clone()));
            }
        }
        return pairs;
    }
    match app.selected_details() {
        Some(details) => {
            let address = details
                .actual_ip
                .clone()
                .map(|ip| ip.split('/').next().unwrap_or_default().to_owned())
                .or_else(|| details.configured_ip.clone())
                .unwrap_or_else(|| "—".to_owned());
            pairs.push(("address", address));
            pairs.push((
                "agent",
                if details.qga { "✓ answers" } else { "— no answer" }.to_owned(),
            ));
            pairs.push(("root", details.root_storage.clone().unwrap_or_else(|| "—".to_owned())));
            let home = match (details.home_size_gb, &details.home_storage) {
                (Some(gib), Some(storage)) => format!("{gib:.0} GiB  {storage}"),
                (Some(gib), None) => format!("{gib:.0} GiB"),
                _ => "—".to_owned(),
            };
            pairs.push(("home", home));
            if let Some(ssh) = &details.ssh.user {
                pairs.push(("ssh", ssh.clone()));
            }
        }
        None => {
            pairs.push(("address", entry.ip.clone().unwrap_or_else(|| "—".to_owned())));
            pairs.push(("root", entry.root_storage.clone().unwrap_or_else(|| "—".to_owned())));
            let message = app
                .details_unavailable(entry)
                .or_else(|| app.details_error())
                .unwrap_or("reading the details");
            pairs.push(("", message.to_owned()));
        }
    }
    pairs
}

fn details_box(buf: &mut Buffer, area: Rect, app: &App, entry: &StatusEntry, packed: bool) {
    let palette = &app.palette;
    let title = format!("{} {}", entry.vmid, entry.name.as_deref().unwrap_or(""));
    let inner = frame(
        buf,
        area,
        palette,
        &Sides {
            top_left: vec![Control::plain(&title)],
            ..Sides::default()
        },
    );
    let text = text_style(palette);
    let muted = text.fg(palette.muted);
    let pairs = detail_pairs(app, entry);
    let mut lines: Vec<Vec<(String, Style)>> = Vec::new();
    let item = |label: &str, value: &str| -> Vec<(String, Style)> {
        if label.is_empty() {
            vec![(format!(" {value}"), text)]
        } else {
            vec![(format!(" {label:<8} "), muted), (value.to_owned(), text)]
        }
    };
    if packed {
        let mut line: Vec<(String, Style)> = Vec::new();
        let mut used = 0;
        for (label, value) in &pairs {
            let parts = item(label, value);
            let width: usize = parts.iter().map(|(t, _)| t.chars().count()).sum::<usize>() + 3;
            if used + width > usize::from(inner.width) && !line.is_empty() {
                lines.push(std::mem::take(&mut line));
                used = 0;
            }
            used += width;
            line.extend(parts);
            line.push(("   ".to_owned(), text));
        }
        if !line.is_empty() {
            lines.push(line);
        }
    } else {
        lines = pairs.iter().map(|(label, value)| item(label, value)).collect();
    }
    for (row, line) in lines.iter().enumerate().take(usize::from(inner.height)) {
        spans(
            buf,
            inner.x,
            inner.y + u16::try_from(row).unwrap_or(0),
            inner.width,
            line,
        );
    }
}

/// How many servers are selected with Space, in every group and on every
/// node; `None` for none.
fn selected_count(app: &App) -> Option<usize> {
    let count = app
        .status
        .iter()
        .flat_map(|status| status.gold.iter().chain(&status.workspaces).chain(&status.others))
        .filter(|entry| app.marked.contains(&entry.vmid))
        .count();
    (count > 0).then_some(count)
}

fn workspaces(buf: &mut Buffer, area: Rect, app: &App, mut controls: Vec<Control>) {
    let palette = &app.palette;
    // The setup takes the panel itself: one frame, as btop's panels.
    if let Some(setup) = &app.setup {
        super::draw_setup::setup_box(buf, area, app, setup, controls);
        return;
    }
    let rows = app.rows();
    let switch = |control: Control, on: bool| if on { control.raised() } else { control };
    let mut top_left = vec![switch(
        Control::panel(1, &format!("‹ {} ›", app.group_name())),
        app.on_group,
    )];
    top_left.append(&mut controls);
    if app.filtering || !app.filter.is_empty() {
        let cursor = if app.filtering { "▏" } else { "" };
        top_left.push(Control::plain(&format!("filter: {}{cursor}", app.filter)));
    } else {
        top_left.push(Control::key("&filter"));
    }
    let node = app.node_name().unwrap_or_else(|| "all".to_owned());
    let position = app.selected_index().map_or(0, |index| index + 1);
    let sides = Sides {
        top_left,
        top_right: vec![
            switch(Control::plain(&format!("‹ node: {node} ›")), !app.on_group),
            Control::key("&new").enabled(app.status.is_some() && app.action.is_none()),
        ],
        bottom_left: workspace_actions(app),
        bottom_right: selected_count(app)
            .map(|count| Control::plain(&format!("{count} selected")))
            .into_iter()
            .chain([Control::plain(&format!("{position}/{}", rows.len()))])
            .collect(),
        focused: false,
    };
    let inner = frame(buf, area, palette, &sides);
    let text = text_style(palette);
    let muted = text.fg(palette.muted);
    if app.status.is_none() && app.unreachable.is_empty() && app.notice.is_none() {
        // A moving bar and the time in the middle of the panel, so the start
        // is never a still screen.
        let seconds = f64::from(app.waiting) / 10.0;
        let message = app.startup_message.unwrap_or("Reading the status from the servers");
        let words = format!("{message}  {seconds:.1} s");
        let width = 30_usize;
        let block = 6_usize;
        let at = usize::try_from(app.waiting).unwrap_or(0) % (width + block);
        let bar: String = (0..width)
            .map(|cell| if cell + block >= at && cell < at { '█' } else { '░' })
            .collect();
        let middle = |text: &str| {
            let length = u16::try_from(text.chars().count()).unwrap_or(0);
            inner.x + inner.width.saturating_sub(length) / 2
        };
        let y = inner.y + inner.height.saturating_sub(3) / 2;
        write(buf, middle(&words), y, inner.width, &words, muted);
        write(buf, middle(&bar), y + 2, inner.width, &bar, text.fg(palette.focus));
        return;
    }
    if let Some((title, lines)) = &app.notice {
        write(
            buf,
            inner.x + 2,
            inner.y + 1,
            inner.width,
            title,
            text.add_modifier(Modifier::BOLD),
        );
        for (row, line) in lines.iter().enumerate() {
            let y = inner.y + 3 + u16::try_from(row).unwrap_or(0);
            write(buf, inner.x + 2, y, inner.width.saturating_sub(2), line, text);
        }
        return;
    }
    if !app.unreachable.is_empty() {
        let mut y = inner.y + 1;
        write(
            buf,
            inner.x + 2,
            y,
            inner.width,
            "No server answers.",
            text.add_modifier(Modifier::BOLD),
        );
        y += 2;
        for (url, reason) in &app.unreachable {
            spans(
                buf,
                inner.x + 2,
                y,
                inner.width.saturating_sub(2),
                &[
                    ("✗ ".to_owned(), text.fg(palette.error)),
                    (format!("{}   ", short_host(url)), text),
                    (short_reason(reason), muted),
                ],
            );
            y += 1;
        }
        write(
            buf,
            inner.x + 2,
            y + 1,
            inner.width.saturating_sub(4),
            "It tries again by itself. Menu › Update API containers updates the saved deployment.",
            muted,
        );
        return;
    }
    if app.status.is_none() {
        write(buf, inner.x + 1, inner.y, inner.width, "Reading the status", muted);
        return;
    }
    let mut list = inner;
    let selected = app.selected_entry();
    let open = app.action.as_ref().filter(|action| !action.hidden);
    if let Some(action) = open {
        if inner.width >= 82 {
            let sub = Rect::new(inner.x + 36, inner.y, inner.width - 36, inner.height);
            action_box(buf, sub, app, action);
            list.width = 35;
        } else {
            let height = inner.height.saturating_sub(6).max(3);
            let sub = Rect::new(inner.x, inner.y, inner.width, height);
            action_box(buf, sub, app, action);
            list.y += height;
            list.height -= height;
        }
    } else if let Some(entry) = selected.filter(|_| app.info) {
        let wide = inner.width >= 82;
        if wide {
            let box_width = if inner.width >= 108 { 45 } else { 35 };
            let sub = Rect::new(inner.right() - box_width, inner.y, box_width, inner.height.min(10));
            details_box(buf, sub, app, entry, false);
            list.width -= box_width + 1;
        } else {
            let sub = Rect::new(inner.x, inner.y, inner.width, 7.min(inner.height));
            details_box(buf, sub, app, entry, true);
            list.y += sub.height;
            list.height -= sub.height;
        }
    }
    if list.height == 0 {
        return;
    }
    // NAME fits the longest name of every group, so it does not change
    // when the group does.
    let name_width = app
        .status
        .iter()
        .flat_map(|status| status.gold.iter().chain(&status.workspaces).chain(&status.others))
        .filter_map(|entry| entry.name.as_deref())
        .map(|name| name.chars().count())
        .max()
        .unwrap_or(0);
    let tag_width = app
        .status
        .iter()
        .flat_map(|status| status.gold.iter().chain(&status.workspaces).chain(&status.others))
        .map(|entry| entry.tags.len().min(MAX_TAGS))
        .max()
        .unwrap_or(0);
    let columns = columns(usize::from(list.width), tag_width, name_width);
    write(
        buf,
        list.x,
        list.y,
        list.width,
        &list_line(&columns, |column| {
            if column.heading == "NAME" {
                " NAME".to_owned()
            } else {
                column.heading.to_owned()
            }
        }),
        muted,
    );
    if rows.is_empty() {
        let message = if app.filter.is_empty() && app.node.is_none() {
            "No workspaces yet."
        } else {
            "No workspace matches."
        };
        write(buf, list.x + 1, list.y + 1, list.width, message, muted);
        return;
    }
    let visible = usize::from(list.height - 1);
    let index = app.selected_index();
    let offset = index.map_or(0, |at| (at + 1).saturating_sub(visible));
    app.list_rows.set((
        list.x,
        list.width,
        list.y + 1,
        u16::try_from(rows.len().saturating_sub(offset).min(visible)).unwrap_or(0),
        offset,
    ));
    for (row, entry) in rows.iter().enumerate().skip(offset).take(visible) {
        let y = list.y + 1 + u16::try_from(row - offset).unwrap_or(0);
        let chosen = Some(row) == index;
        let mut style = text;
        if chosen {
            // A bar in the accent colour, as btop's, visible in every theme.
            style = style
                .fg(palette.background)
                .bg(palette.focus)
                .add_modifier(Modifier::BOLD);
            if palette.no_color {
                style = style.add_modifier(Modifier::REVERSED);
            }
        } else if app.marked.contains(&entry.vmid) {
            // A marked server: the whole line lighter.
            style = style.bg(palette.selection).add_modifier(Modifier::BOLD);
            if palette.no_color {
                style = style.add_modifier(Modifier::UNDERLINED);
            }
        }
        let mut shown = (*entry).clone();
        if let Some(action) = app.action.as_ref().filter(|action| {
            action.running() && (action.vmid == Some(entry.vmid) || action.targets.contains(&entry.vmid))
        }) {
            shown.status = format!("◐ {}", action.kind.word());
        }
        let line = list_line(&columns, |column| (column.value)(&shown));
        let line = format!("{line:<width$}", width = usize::from(list.width));
        write(buf, list.x, y, list.width, &line, style);
        if entry
            .image_update
            .as_ref()
            .is_some_and(|update| update.available.is_some())
        {
            let name_x = columns
                .iter()
                .take_while(|column| column.heading != "NAME")
                .map(|column| column.width + 1)
                .sum::<usize>();
            if columns.iter().any(|column| column.heading == "NAME") {
                write(
                    buf,
                    list.x + 1 + u16::try_from(name_x).unwrap_or(0),
                    y,
                    1,
                    "↑",
                    alert_style(palette, style),
                );
            }
        }
        // The tags' squares in their colours, from the first column.
        for (at, tag) in entry.tags.iter().take(MAX_TAGS).enumerate() {
            let x = list.x + 1 + u16::try_from(at).unwrap_or(0);
            write(buf, x, y, 1, "■", style.fg(tag_color(palette, tag)));
        }
        // The state dot in its colour.
        if let Some(at) = line.find(['●', '○', '◐']) {
            let x = list.x + u16::try_from(line[..at].chars().count()).unwrap_or(0);
            let busy = shown.status.starts_with('◐');
            let color: Color = if busy {
                palette.info
            } else if entry.status == "running" {
                palette.ok
            } else {
                palette.muted
            };
            write(
                buf,
                x,
                y,
                1,
                if busy {
                    "◐"
                } else if entry.status == "running" {
                    "●"
                } else {
                    "○"
                },
                style.fg(color),
            );
        }
    }
    let bar_area = Rect::new(area.x, list.y, area.width, list.height + 1);
    scrollbar(buf, bar_area, palette, Scrolls::List, offset, visible, rows.len());
}

fn storage_line(layout: &LayoutCapacity, width: usize) -> (String, String) {
    let free = layout.available_bytes.map_or_else(|| "—".to_owned(), size);
    let head = format!("{:<5} {}", layout.node, layout.storage);
    let tail = format!("{free} free");
    let gap = width.saturating_sub(head.chars().count() + tail.chars().count() + 1);
    let first = format!(" {head}{}{tail}", " ".repeat(gap));
    let percent = layout.percent.unwrap_or(0.0);
    let second = format!(" {} {percent:>3.0}%", bar(width.saturating_sub(7), percent));
    (first, second)
}

fn storage(buf: &mut Buffer, area: Rect, app: &App, mut controls: Vec<Control>) {
    if matches!(app.view, 2 | 3) {
        storage_details(buf, area, app, controls);
        return;
    }
    let palette = &app.palette;
    let layouts = app
        .status
        .as_ref()
        .map(|status| status.summary.storage_layouts.clone())
        .unwrap_or_default();
    let mut top_left = vec![Control::panel(3, "storage")];
    top_left.append(&mut controls);
    let sides = Sides {
        top_left,
        bottom_right: vec![Control::plain(&format!("{} nodes", layouts.len()))],
        ..Sides::default()
    };
    let inner = frame(buf, area, palette, &sides);
    let text = text_style(palette);
    let mut y = inner.y;
    for layout in &layouts {
        if y + 1 >= inner.bottom() {
            break;
        }
        let (first, second) = storage_line(layout, usize::from(inner.width));
        write(buf, inner.x, y, inner.width, &first, text);
        let color = match layout.percent.unwrap_or(0.0) {
            p if p >= 90.0 => palette.error,
            p if p >= 75.0 => palette.warning,
            _ => palette.ok,
        };
        write(buf, inner.x, y + 1, inner.width, &second, text.fg(color));
        y += 2;
    }
}

/// Append aligned cells, wrapping only a value that exceeds its column.
fn storage_row(lines: &mut Vec<Vec<(String, Style)>>, cells: &[(String, usize, bool, Style)]) {
    let rows = cells
        .iter()
        .map(|(value, width, _, _)| value.chars().count().div_ceil((*width).max(1)))
        .max()
        .unwrap_or(1)
        .max(1);
    for row in 0..rows {
        let mut line = vec![(" ".to_owned(), Style::default())];
        for (index, (value, width, right, style)) in cells.iter().enumerate() {
            let part: String = value.chars().skip(row * width).take(*width).collect();
            if index > 0 {
                line.push((" ".to_owned(), *style));
            }
            line.push((
                if *right {
                    pad_right(&part, *width)
                } else {
                    pad(&part, *width)
                },
                *style,
            ));
        }
        lines.push(line);
    }
}

/// Scrollable storage inventory used only by the dedicated views.
fn storage_details(buf: &mut Buffer, area: Rect, app: &App, mut controls: Vec<Control>) {
    let palette = &app.palette;
    let text = text_style(palette);
    let muted = text.fg(palette.muted);
    let title = text.fg(palette.accent).add_modifier(Modifier::BOLD);
    let mut heading = vec![Control::panel(3, "storage")];
    heading.append(&mut controls);
    let inner = frame(
        buf,
        area,
        palette,
        &Sides {
            top_left: heading,
            bottom_right: vec![Control::plain("↑↓ scroll · PgUp/PgDn · Home/End")],
            ..Sides::default()
        },
    );
    let width = usize::from(inner.width.saturating_sub(1));
    let mut lines: Vec<Vec<(String, Style)>> = Vec::new();
    if let Some(status) = &app.status {
        let summary = &status.summary;
        let overview = match &app.storage_overview {
            Some(Ok(overview)) => Some(overview),
            Some(Err(error)) => {
                lines.push(vec![(
                    format!(" Storage inventory unavailable: {error}"),
                    text.fg(palette.warning),
                )]);
                lines.push(vec![(" Leave and reopen this view to retry.".to_owned(), muted)]);
                None
            }
            None => {
                lines.push(vec![(" Reading storage inventory…".to_owned(), muted)]);
                None
            }
        };
        if let Some(overview) = overview {
            let shared = overview.storages.iter().filter(|storage| storage.shared).count();
            let critical = overview
                .storages
                .iter()
                .filter(|storage| storage.capacity.percent.is_some_and(|p| p >= 85.0))
                .count();
            lines.push(vec![(
                format!(
                    " STORAGE  {} total · {shared} shared · {critical} critical",
                    overview.storages.len()
                ),
                title,
            )]);
            let wide = width >= 139;
            let name_width = if wide { 20 } else { 19 };
            let type_width = if wide { 8 } else { 7 };
            let shared_width = if wide { 6 } else { 2 };
            let content_width = if wide { width.saturating_sub(77) } else { 6 };
            storage_row(
                &mut lines,
                &[
                    ("STORAGE".to_owned(), name_width, false, muted),
                    ("TYPE".to_owned(), type_width, false, muted),
                    ("NODES".to_owned(), 14, false, muted),
                    (
                        if wide { "SHARED" } else { "SH" }.to_owned(),
                        shared_width,
                        false,
                        muted,
                    ),
                    ("USED/TOTAL".to_owned(), 11, true, muted),
                    ("USE%".to_owned(), 4, true, muted),
                    ("FREE".to_owned(), 6, true, muted),
                    (
                        if wide { "CONTENT" } else { "IRBOST" }.to_owned(),
                        content_width,
                        false,
                        muted,
                    ),
                ],
            );
            for storage in &overview.storages {
                let capacity = &storage.capacity;
                let color = if !storage.available {
                    palette.warning
                } else {
                    match capacity.percent {
                        Some(p) if p >= 85.0 => palette.error,
                        Some(p) if p >= 75.0 => palette.warning,
                        _ => palette.foreground,
                    }
                };
                let mut kinds: Vec<_> = storage
                    .content
                    .split(',')
                    .map(str::trim)
                    .filter(|kind| !kind.is_empty())
                    .collect();
                kinds.sort_unstable();
                kinds.dedup();
                let content = if wide {
                    kinds.join(", ")
                } else {
                    [
                        ("images", 'I'),
                        ("rootdir", 'R'),
                        ("backup", 'B'),
                        ("iso", 'O'),
                        ("snippets", 'S'),
                        ("vztmpl", 'T'),
                    ]
                    .iter()
                    .map(|(kind, mark)| if kinds.contains(kind) { *mark } else { '·' })
                    .collect()
                };
                let used = capacity.used_bytes.map_or_else(|| "—".to_owned(), size);
                let total = capacity.total_bytes.map_or_else(|| "—".to_owned(), size);
                let percent = if !storage.available {
                    "off".to_owned()
                } else {
                    capacity.percent.map_or_else(|| "—".to_owned(), |p| format!("{p:.0}%"))
                };
                storage_row(
                    &mut lines,
                    &[
                        (storage.storage.clone(), name_width, false, text),
                        (storage.storage_type.clone(), type_width, false, muted),
                        (storage.nodes.join(","), 14, false, text),
                        (
                            if storage.shared {
                                if wide { "yes" } else { "*" }
                            } else {
                                "—"
                            }
                            .to_owned(),
                            shared_width,
                            false,
                            muted,
                        ),
                        (format!("{used}/{total}"), 11, true, text),
                        (percent, 4, true, text.fg(color)),
                        (
                            capacity.available_bytes.map_or_else(|| "—".to_owned(), size),
                            6,
                            true,
                            text.fg(color),
                        ),
                        (content, content_width, false, muted),
                    ],
                );
            }
            if !wide {
                lines.push(vec![(
                    " I images · R rootdir · B backup · O iso · S snippets · T vztmpl · * shared".to_owned(),
                    muted,
                )]);
            }
            if overview.storages.iter().any(|storage| !storage.available) {
                lines.push(vec![(
                    " off = storage unavailable".to_owned(),
                    text.fg(palette.warning),
                )]);
            }
            for warning in &overview.warnings {
                lines.push(vec![(format!(" {warning}"), text.fg(palette.warning))]);
            }
        }
        lines.push(Vec::new());
        let homes = &summary.homes;
        lines.push(vec![
            (" /HOME  ".to_owned(), title),
            (
                format!(
                    "used {} / limit {}",
                    homes.used_bytes.map_or_else(|| "—".to_owned(), size),
                    size(homes.quota_bytes)
                ),
                text,
            ),
            (
                format!("   missing {}", homes.missing_count),
                if homes.missing_count > 0 {
                    text.fg(palette.warning)
                } else {
                    muted
                },
            ),
        ]);
        lines.push(Vec::new());
        lines.push(vec![
            (format!(" ORPHAN VOLUMES  {}", summary.orphaned_volumes.len()), title),
            (
                if summary.orphan_scan_complete {
                    String::new()
                } else {
                    " · scan incomplete".to_owned()
                },
                text.fg(palette.warning),
            ),
        ]);
        if summary.orphaned_volumes.is_empty() {
            lines.push(vec![(" No detached volumes found.".to_owned(), muted)]);
        } else {
            let volume_width = summary
                .orphaned_volumes
                .iter()
                .map(|volume| volume.volid.chars().count())
                .max()
                .unwrap_or(20)
                .clamp(20, 60)
                .min(width.saturating_sub(34).max(20));
            storage_row(
                &mut lines,
                &[
                    ("VOLUME".to_owned(), volume_width, false, muted),
                    ("STORAGE".to_owned(), 17, false, muted),
                    ("NODE".to_owned(), 8, false, muted),
                    ("SIZE".to_owned(), 6, true, muted),
                ],
            );
            for volume in &summary.orphaned_volumes {
                storage_row(
                    &mut lines,
                    &[
                        (volume.volid.clone(), volume_width, false, text),
                        (volume.storage.clone(), 17, false, muted),
                        (volume.node.clone(), 8, false, text),
                        (volume.size_bytes.map_or_else(|| "—".to_owned(), size), 6, true, text),
                    ],
                );
            }
        }
        if let Some(overview) = overview {
            let mut guests: Vec<_> = status
                .gold
                .iter()
                .chain(&status.workspaces)
                .chain(&status.others)
                .collect();
            guests.sort_by_key(|guest| guest.vmid);
            guests.dedup_by_key(|guest| guest.vmid);
            let backed_up = guests
                .iter()
                .filter(|guest| overview.latest_backups.iter().any(|backup| backup.vmid == guest.vmid))
                .count();
            lines.push(Vec::new());
            lines.push(vec![
                (format!(" BACKUPS  {backed_up}/{} guests", guests.len()), title),
                (
                    if overview.backup_scan_complete {
                        String::new()
                    } else {
                        " · scan incomplete".to_owned()
                    },
                    text.fg(palette.warning),
                ),
            ]);
            if backed_up > 0 {
                let guest_width = guests
                    .iter()
                    .filter_map(|guest| guest.name.as_deref())
                    .map(|name| name.chars().count())
                    .max()
                    .unwrap_or(12)
                    .clamp(12, 32)
                    .min(width.saturating_sub(38).max(12));
                storage_row(
                    &mut lines,
                    &[
                        ("VMID".to_owned(), 7, true, muted),
                        ("GUEST".to_owned(), guest_width, false, muted),
                        ("AGE".to_owned(), 8, true, muted),
                        ("STORAGE".to_owned(), 17, false, muted),
                    ],
                );
                let now = std::time::SystemTime::now()
                    .duration_since(std::time::UNIX_EPOCH)
                    .unwrap_or_default()
                    .as_secs();
                for guest in &guests {
                    let Some(backup) = overview.latest_backups.iter().find(|backup| backup.vmid == guest.vmid) else {
                        continue;
                    };
                    let age = now.saturating_sub(backup.created_at);
                    let age = if age >= 86400 {
                        format!("{}d", age / 86400)
                    } else if age >= 3600 {
                        format!("{}h", age / 3600)
                    } else {
                        format!("{}m", age / 60)
                    };
                    storage_row(
                        &mut lines,
                        &[
                            (guest.vmid.to_string(), 7, true, text),
                            (
                                guest.name.clone().unwrap_or_else(|| "—".to_owned()),
                                guest_width,
                                false,
                                text,
                            ),
                            (age, 8, true, text),
                            (backup.storage.clone(), 17, false, muted),
                        ],
                    );
                }
            }
            let missing: Vec<_> = guests
                .iter()
                .filter(|guest| !overview.latest_backups.iter().any(|backup| backup.vmid == guest.vmid))
                .map(|guest| guest.vmid.to_string())
                .collect();
            if !missing.is_empty() {
                let message = if !overview.backup_scan_complete {
                    format!(" Backup unknown for: {}", missing.join(", "))
                } else if backed_up == 0 {
                    format!(" No backup found for any of the {} guests.", guests.len())
                } else {
                    format!(" No backup found: {}", missing.join(", "))
                };
                lines.push(vec![(message, muted)]);
            }
        }
    } else {
        lines.push(vec![(" Waiting for storage inventory…".to_owned(), muted)]);
    }
    // Wrap prose without ever wrapping an aligned table row as one long string.
    let mut wrapped = Vec::new();
    for line in lines {
        let mut row = Vec::new();
        let mut used = 0;
        for (value, style) in line {
            for ch in value.chars() {
                if used == usize::from(inner.width) {
                    wrapped.push(row);
                    row = Vec::new();
                    used = 0;
                }
                row.push((ch.to_string(), style));
                used += 1;
            }
        }
        wrapped.push(row);
    }
    let visible = usize::from(inner.height);
    let most = wrapped.len().saturating_sub(visible);
    let offset = app.storage_scroll.get().min(most);
    app.storage_scroll.set(offset);
    app.storage_scroll_max.set(most);
    for (row, parts) in wrapped.iter().skip(offset).take(visible).enumerate() {
        let y = inner.y + u16::try_from(row).unwrap_or(0);
        spans(buf, inner.x, y, inner.width, parts);
    }
    scrollbar(buf, area, palette, Scrolls::Storage, offset, visible, wrapped.len());
}

pub(super) fn log(buf: &mut Buffer, area: Rect, app: &App, mut controls: Vec<Control>) {
    let palette = &app.palette;
    let mut top_left = vec![Control::panel(4, "log")];
    top_left.append(&mut controls);
    // The highlighted line's place (1 the newest), as the list's counter.
    let total = app.lines.len();
    let counter = match (app.log_cursor, total) {
        (Some(at), _) => format!("{}/{total}", at + 1),
        (None, 1) => "1 line".to_owned(),
        (None, _) => format!("{total} lines"),
    };
    // Newest first; scrolled back by app.log_scroll lines.
    let visible = usize::from(area.height.saturating_sub(2));
    let most = total.saturating_sub(visible);
    let offset = app.log_scroll.min(most);
    // ┐*┌ in the top right corner: new lines above the view, unread until
    // the log is scrolled to the newest, as an editor marks a change.
    let top_right = if app.log_unread.min(offset) > 0 {
        vec![Control::plain("*").raised()]
    } else {
        Vec::new()
    };
    let sides = Sides {
        top_left,
        top_right,
        bottom_right: vec![Control::plain(&counter)],
        ..Sides::default()
    };
    let inner = frame(buf, area, palette, &sides);
    let text = text_style(palette);
    let muted = text.fg(palette.muted);
    register(
        inner,
        Hit::Log {
            most,
            visible,
            line: None,
        },
    );
    for (row, line) in app.lines.iter().rev().skip(offset).take(visible).enumerate() {
        let y = inner.y + u16::try_from(row).unwrap_or(0);
        let (text, muted) = if app.log_cursor == Some(offset + row) {
            // The list's bar in the accent colour.
            let mut chosen = text
                .fg(palette.background)
                .bg(palette.focus)
                .add_modifier(Modifier::BOLD);
            if palette.no_color {
                chosen = chosen.add_modifier(Modifier::REVERSED);
            }
            write(
                buf,
                inner.x,
                y,
                inner.width,
                &" ".repeat(usize::from(inner.width)),
                chosen,
            );
            (chosen, chosen)
        } else if line.earlier {
            // From an earlier run: all of it muted.
            (muted, muted)
        } else {
            (text, muted)
        };
        spans(
            buf,
            inner.x,
            y,
            inner.width,
            &[(format!(" {}  ", line.time), muted), (line.text.clone(), text)],
        );
        register(
            Rect::new(inner.x, y, inner.width, 1),
            Hit::Log {
                most,
                visible,
                line: Some(offset + row),
            },
        );
    }
    scrollbar(buf, area, palette, Scrolls::Log, offset, visible, total);
}

pub(super) fn centered(area: Rect, width: u16, height: u16) -> Rect {
    let width = width.min(area.width);
    let height = height.min(area.height);
    Rect::new(
        area.x + (area.width - width) / 2,
        area.y + (area.height - height) / 2,
        width,
        height,
    )
}

fn draw_overlay(buf: &mut Buffer, area: Rect, app: &App, overlay: &Overlay) {
    let palette = &app.palette;
    let text = text_style(palette);
    let muted = text.fg(palette.muted);
    let (title, lines, bottom): (&str, Vec<(String, Style)>, Vec<Control>) = match overlay {
        Overlay::Quit => (
            "quit",
            vec![("Exit HomeStack?".to_owned(), text)],
            vec![Control::key("quit ↵"), Control::key("cancel esc")],
        ),
        Overlay::Menu { index } => (
            "menu",
            MENU.iter()
                .enumerate()
                .map(|(at, item)| list_item(palette, item, at == *index))
                .collect(),
            vec![
                Control::key("select ↑↓"),
                Control::key("open ↵"),
                Control::key("close esc"),
            ],
        ),
        Overlay::Themes { index, .. } => (
            "theme",
            THEMES
                .iter()
                .enumerate()
                .map(|(at, name)| {
                    let label = if *name == app.preferences.theme {
                        format!("{name}  (saved)")
                    } else {
                        (*name).to_owned()
                    };
                    list_item(palette, &label, at == *index)
                })
                .collect(),
            vec![
                Control::key("preview ↑↓"),
                Control::key("save ↵"),
                Control::key("back esc"),
            ],
        ),
        Overlay::Help => (
            "help",
            HELP.iter().map(|line| ((*line).to_owned(), text)).collect(),
            vec![Control::key("close esc")],
        ),
        Overlay::Report { title, lines } => (
            title.as_str(),
            lines
                .iter()
                .flat_map(|line| wrap(line, usize::from(area.width.saturating_sub(8))))
                .map(|line| (line, text))
                .collect(),
            vec![Control::key("close esc")],
        ),
        Overlay::ReadDetails { vmid, action } => (
            action.word(),
            vec![(
                app.selected_entry()
                    .and_then(|entry| app.details_unavailable(entry))
                    .map_or_else(|| format!("Reading details of VM {vmid}"), str::to_owned),
                text,
            )],
            vec![Control::key("cancel esc")],
        ),
    };
    let width = lines
        .iter()
        .map(|(line, _)| line.chars().count() + 4)
        .max()
        .unwrap_or(20)
        .max(40);
    let width = u16::try_from(width).unwrap_or(u16::MAX);
    let height = u16::try_from(lines.len() + 4).unwrap_or(u16::MAX);
    let rect = centered(area, width, height);
    let inner = frame(
        buf,
        rect,
        palette,
        &Sides {
            top_left: vec![Control::plain(title)],
            bottom_left: bottom,
            focused: true,
            ..Sides::default()
        },
    );
    for (row, (line, style)) in lines
        .iter()
        .enumerate()
        .take(usize::from(inner.height.saturating_sub(1)))
    {
        let y = inner.y + 1 + u16::try_from(row).unwrap_or(0);
        write(buf, inner.x + 1, y, inner.width.saturating_sub(2), line, *style);
        // The menu's and the themes' items are clicked.
        if matches!(overlay, Overlay::Menu { .. } | Overlay::Themes { .. }) {
            super::chrome::register(Rect::new(inner.x, y, inner.width, 1), super::chrome::Hit::Item(row));
        }
    }
    let _ = muted;
}

fn list_item(palette: &Palette, label: &str, chosen: bool) -> (String, Style) {
    let text = text_style(palette);
    if chosen {
        (
            format!("▶ {label}"),
            text.fg(palette.focus).add_modifier(Modifier::BOLD),
        )
    } else {
        (format!("  {label}"), text)
    }
}

const HELP: [&str; 25] = [
    "1 2 3 4     view 1 workspaces, 2 cluster+storage, 3 storage, 4 log (again: auto)",
    "0           the standard view: every panel and node, no filter",
    "v           next view (auto, 1, 2, 3, 4)",
    "m           menu",
    "F2          copy mode: select terminal text; F2 or Esc resumes the display",
    "F3          copy loaded JSON data to clipboard (terminal OSC 52 support required)",
    "↑ ↓         select a workspace; PgUp PgDn Home End",
    "i           details of the selected workspace (again: hide them)",
    "← →         one node or all",
    "f           filter by name or VMID; ↵ keeps it, esc clears it",
    "esc         close a box, return to view 0, then confirm quit (↵ yes, esc cancel)",
    "q           quit immediately",
    "",
    "On the selected workspace:",
    "s ssh (log out to come back)   t start or stop   enter or e setup",
    "n new   r refresh   z resize   k add key   g migrate   d destroy",
    "In the box: tab next field, ← → choose, ↵ plan, ↵ or a apply,",
    "esc cancel (while it runs esc hides the box; ↵ shows it).",
    "",
    "On a server (containers): ␣ select or unselect it; ↵ update its image,",
    "or every selected one when it is selected, from a published image.",
    "",
    "Setup: tab next group, ␣ toggle, 0 branch, f filter, ← → close or",
    "open, ↵ review, F5 reread state, Ctrl+R catalog; green ready, red overwrite.",
    "The CLI has every command: homestack --help",
];

/// `line` cut into pieces of at most `width` characters, at spaces when it
/// can; the indentation of the first piece is kept on the others.
pub(super) fn wrap(line: &str, width: usize) -> Vec<String> {
    if width == 0 || line.chars().count() <= width {
        return vec![line.to_owned()];
    }
    // A rule (----) is cut to the width, never wrapped.
    if line.trim().chars().all(|c| c == '-') {
        return vec![line.chars().take(width).collect()];
    }
    let indent: String = line.chars().take_while(|c| *c == ' ').collect();
    let indent = if indent.len() * 2 > width {
        String::new()
    } else {
        indent
    };
    let mut pieces = Vec::new();
    let mut current = String::new();
    for word in line.split(' ') {
        let fits = current.chars().count() + word.chars().count() < width;
        if !current.trim().is_empty() && !fits {
            pieces.push(current.trim_end().to_owned());
            current = indent.clone();
        }
        if !current.is_empty() || pieces.is_empty() {
            current.push_str(word);
            current.push(' ');
        } else {
            current = format!("{indent}{word} ");
        }
        while current.chars().count() > width {
            let head: String = current.chars().take(width).collect();
            pieces.push(head);
            current = format!("{indent}{}", current.chars().skip(width).collect::<String>());
        }
    }
    if !current.trim().is_empty() {
        pieces.push(current.trim_end().to_owned());
    }
    pieces
}

/// The box of an action (mockups 12–14): its stages in the top border, its
/// buttons in the bottom border.
/// The reinstall form under its choice: the image each server was made
/// from, and the images published lately, the chosen one marked.
fn reinstall_lines(app: &App, chosen: usize, text: Style, muted: Style, lines: &mut Vec<(String, Style)>) {
    let images = match &app.images {
        None => return lines.push(("reading the images".to_owned(), muted)),
        Some(Err(error)) => return lines.push((error.clone(), text.fg(app.palette.error))),
        Some(Ok(images)) => images,
    };
    lines.push((String::new(), text));
    lines.push(("Now".to_owned(), muted));
    for server in &images.servers {
        let now = server.image.as_ref().map_or_else(
            || "not made by homestack-deploy".to_owned(),
            |image| format!("{}  {}", image.tag(), image.made),
        );
        lines.push((format!("{:<11}{now}", server.name), text));
    }
    lines.push((String::new(), text));
    lines.push(("Published".to_owned(), muted));
    for (index, image) in images.published.iter().enumerate() {
        let on: Vec<&str> = images
            .servers
            .iter()
            .filter(|server| server.image.as_ref().is_some_and(|now| now.image == image.image))
            .map(|server| server.name.as_str())
            .collect();
        let newer = images.servers.iter().any(|server| images.newer(image, server));
        let note = if !on.is_empty() {
            format!("  now on {}", on.join(", "))
        } else if newer {
            "  new".to_owned()
        } else {
            String::new()
        };
        let (marker, style) = if index == chosen {
            ("› ", text.add_modifier(Modifier::BOLD))
        } else if newer {
            ("  ", text)
        } else {
            ("  ", muted)
        };
        lines.push((format!("{marker}{}  {}{note}", image.tag(), image.made), style));
    }
}

fn action_box(buf: &mut Buffer, area: Rect, app: &App, action: &Action) {
    use super::action::{Input, Kind, Part, PlanFocus, Stage, StepState};
    let palette = &app.palette;
    let text = text_style(palette);
    let muted = text.fg(palette.muted);
    let stages: &[&str] = if action.kind.has_form() {
        &["form", "plan", "run"]
    } else {
        &["plan", "run"]
    };
    let current = match action.stage {
        Stage::Form(_) => "form",
        Stage::Asking | Stage::Plan { .. } => "plan",
        _ => "run",
    };
    let at = stages.iter().position(|stage| *stage == current).unwrap_or(0);
    let mut top_left = vec![Control::plain(&action.title())];
    for (index, stage) in stages.iter().enumerate() {
        top_left.push(match index.cmp(&at) {
            std::cmp::Ordering::Less => Control::plain(&format!("✓{stage}")).muted(),
            std::cmp::Ordering::Equal => Control::plain(stage).raised(),
            std::cmp::Ordering::Greater => Control::plain(stage).muted(),
        });
    }
    let raised = |control: Control, on: bool| if on { control.chosen() } else { control };
    let bottom_left = match &action.stage {
        Stage::Form(_) if action.kind == Kind::Addkey => vec![
            Control::key("select space"),
            Control::key("plan ↵"),
            Control::key("cancel esc"),
        ],
        Stage::Form(_) => vec![
            Control::key("next ⇥"),
            Control::key("plan ↵"),
            Control::key("cancel esc"),
        ],
        Stage::Asking => vec![Control::key("cancel esc")],
        Stage::Plan { blocked, focus, .. } => {
            let mut buttons = Vec::new();
            if blocked.is_none() {
                buttons.push(raised(Control::key("&apply"), *focus == PlanFocus::Apply));
            }
            buttons.push(raised(Control::key("cancel esc"), *focus == PlanFocus::Cancel));
            buttons
        }
        Stage::Running { .. } => vec![Control::key("hide esc")],
        Stage::Question { yes, .. } => vec![
            raised(Control::key("&restart now"), *yes),
            raised(Control::key("later esc"), !*yes),
        ],
        Stage::Done { ok, .. } => vec![
            Control::plain(if *ok { "✓ done" } else { "✗ failed" }),
            Control::key("close ↵"),
        ],
    };
    let inner = frame(
        buf,
        area,
        palette,
        &Sides {
            top_left,
            bottom_left,
            focused: true,
            ..Sides::default()
        },
    );
    let width = usize::from(inner.width.saturating_sub(2));
    let mut lines: Vec<(String, Style)> = Vec::new();
    // Which line is which field of a form, for the mouse.
    let mut field_rows: Vec<(usize, usize)> = Vec::new();
    let mut key_footer = Vec::new();
    // The amounts of a form: where their parts are, for the mouse and
    // the colours.
    let mut amounts: Vec<AmountRow> = Vec::new();
    let push = |line: &str, style: Style, lines: &mut Vec<(String, Style)>| {
        for piece in wrap(line, width) {
            lines.push((piece, style));
        }
    };
    let mut scroll = 0;
    match &action.stage {
        Stage::Form(form) => {
            lines.push((String::new(), text));
            for (index, field) in form.fields.iter().enumerate() {
                let focused = index == form.focus;
                let value = match &field.input {
                    Input::Check { checked, caption, .. } => {
                        format!("[{}] {caption}", if *checked { "x" } else { " " })
                    }
                    Input::Text(value) => {
                        let cursor = if focused { "▏" } else { " " };
                        format!(
                            "[ {value}{cursor}{} ]",
                            " ".repeat(24usize.saturating_sub(value.chars().count()))
                        )
                    }
                    Input::Size { gib, min } => {
                        let grown = if gib > min {
                            format!("  +{}G", gib - min)
                        } else {
                            String::new()
                        };
                        format!("‹ {gib}G ›{grown}")
                    }
                    Input::Choice { options, index } => {
                        format!("‹ {} ›", options.get(*index).map_or("—", String::as_str))
                    }
                    input @ Input::Amount { g, m, part, set } => {
                        let total = if !set && input.mib() == 0 {
                            "default".to_owned()
                        } else {
                            amount_text(input.mib())
                        };
                        let pieces = [(Part::G, format!("-{g}G+")), (Part::M, format!("-{m}M+"))];
                        let at = 11 + total.chars().count();
                        let mut starts = [0; 2];
                        let mut start = at;
                        for (slot, (_, piece)) in starts.iter_mut().zip(&pieces) {
                            start += 2;
                            *slot = start;
                            start += piece.chars().count();
                        }
                        amounts.push(AmountRow {
                            line: lines.len(),
                            field: index,
                            total: (11, total.chars().count()),
                            level: (input.mib() > 0).then(|| app.home_level(form, input.mib())),
                            parts: [
                                (Part::G, starts[0], pieces[0].1.chars().count()),
                                (Part::M, starts[1], pieces[1].1.chars().count()),
                            ],
                            chosen: focused.then_some(*part),
                        });
                        format!("{total}  {}  {}", pieces[0].1, pieces[1].1)
                    }
                };
                let mut style = text;
                if focused {
                    style = style.bg(palette.selection).add_modifier(Modifier::BOLD);
                }
                field_rows.push((lines.len(), index));
                lines.push((
                    if action.kind == Kind::Addkey {
                        value
                    } else {
                        format!("{:<11}{value}", field.label)
                    },
                    style,
                ));
                // What the pool gives, shown and not edited.
                if action.kind == Kind::New && field.label == "Pool" {
                    let vmid = app.free_vmid(&app.chosen_pool(form));
                    let ip = vmid.and_then(|vmid| app.address_of(vmid));
                    let shown = |value: Option<String>| value.unwrap_or_else(|| "—".to_owned());
                    lines.push((
                        format!("{:<11}{}", "VMID", shown(vmid.map(|vmid| vmid.to_string()))),
                        muted,
                    ));
                    lines.push((format!("{:<11}{}", "IP", shown(ip.map(|ip| ip.to_string()))), muted));
                }
                if action.kind == Kind::Reinstall {
                    reinstall_lines(app, field.index(), text, muted, &mut lines);
                }
            }
            if action.kind == Kind::Addkey {
                let selected = form
                    .fields
                    .iter()
                    .filter(
                        |field| matches!(&field.input, Input::Check { checked: true, value, .. } if !value.is_empty()),
                    )
                    .count();
                key_footer.push((format!("{selected} public keys selected"), muted));
                if let Some(field) = form.fields.get(form.focus) {
                    push(&field.note, muted, &mut key_footer);
                }
                if let Some(error) = &form.error {
                    push(error, text.fg(palette.error), &mut key_footer);
                }
                let visible = usize::from(inner.height).saturating_sub(key_footer.len());
                scroll = form.focus.saturating_add(2).saturating_sub(visible);
            } else {
                lines.push((String::new(), text));
                if let Some(field) = form.fields.get(form.focus) {
                    push(&field.note, muted, &mut lines);
                }
                if let Some(error) = &form.error {
                    push(error, text.fg(palette.error), &mut lines);
                }
            }
        }
        Stage::Asking => lines.push(("asking the server for the plan".to_owned(), muted)),
        Stage::Plan {
            lines: plan,
            question,
            blocked,
            secret,
            focus,
            scroll: offset,
        } => {
            scroll = *offset;
            for line in plan {
                let style = if line.ends_with("NO CHANGES MADE YET") {
                    text.add_modifier(Modifier::BOLD)
                } else {
                    text
                };
                push(line, style, &mut lines);
            }
            lines.push((String::new(), text));
            match blocked {
                Some(reason) => push(reason, text.fg(palette.warning), &mut lines),
                None => push(question, text.add_modifier(Modifier::BOLD), &mut lines),
            }
            if let Some((prompt, typed)) = secret {
                let cursor = if *focus == PlanFocus::Secret { "▏" } else { "" };
                let dots = "•".repeat(typed.chars().count());
                push(&format!("{prompt}: [ {dots}{cursor} ]"), text, &mut lines);
            }
        }
        Stage::Running { steps } => {
            lines.push((String::new(), text));
            for step in steps {
                let mark = match step.state {
                    StepState::Done => "✓",
                    StepState::Failed => "✗",
                    StepState::Running => "◐",
                    StepState::Waiting => "○",
                };

                push(&format!("{mark} {}", step.name), text, &mut lines);
            }
            let done = steps.iter().filter(|step| step.state == StepState::Done).count();
            lines.push((String::new(), text));
            let label = if steps.is_empty() {
                "starting".to_owned()
            } else {
                format!("{done} of {} steps done", steps.len())
            };
            lines.push((label, muted));
        }
        Stage::Question { lines: question, .. } => {
            lines.push((String::new(), text));
            for line in question {
                push(line, text, &mut lines);
            }
        }
        Stage::Done { ok, lines: result } => {
            let style = if *ok { text } else { text.fg(palette.error) };
            for line in result {
                push(line, style, &mut lines);
            }
        }
    }
    let height = usize::from(inner.height).saturating_sub(key_footer.len());
    let scroll = scroll.min(lines.len().saturating_sub(height));
    for (line, field) in &field_rows {
        if *line < scroll || *line >= scroll + height {
            continue;
        }
        let y = inner.y + u16::try_from(line - scroll).unwrap_or(0);
        super::chrome::register(
            Rect::new(inner.x, y, inner.width, 1),
            super::chrome::Hit::Field(*field, (action.kind == Kind::Addkey).then_some(KeyCode::Char(' '))),
        );
        let text = &lines[*line].0;
        for (mark, key) in [('‹', KeyCode::Left), ('›', KeyCode::Right)] {
            if let Some(at) = text.chars().position(|c| c == mark) {
                let x = inner.x + 1 + u16::try_from(at).unwrap_or(0);
                super::chrome::register(Rect::new(x, y, 1, 1), super::chrome::Hit::Field(*field, Some(key)));
            }
        }
    }
    for amount in &amounts {
        if amount.line < scroll || amount.line >= scroll + height {
            continue;
        }
        let y = inner.y + u16::try_from(amount.line - scroll).unwrap_or(0);
        let x = |at: usize| inner.x + 1 + u16::try_from(at).unwrap_or(0);
        for (part, start, len) in amount.parts {
            let cells = [
                (start, 1, -1),
                (start + 1, len.saturating_sub(2), 0),
                (start + len - 1, 1, 1),
            ];
            for (at, width, delta) in cells {
                let width = u16::try_from(width).unwrap_or(0);
                super::chrome::register(Rect::new(x(at), y, width, 1), Hit::Amount(amount.field, part, delta));
            }
        }
    }
    for (row, (line, style)) in lines.iter().skip(scroll).take(height).enumerate() {
        let y = inner.y + u16::try_from(row).unwrap_or(0);
        write(buf, inner.x + 1, y, inner.width.saturating_sub(2), line, *style);
        // Step marks in their colours.
        if let Some(mark) = line.chars().next().filter(|c| matches!(c, '✓' | '✗' | '◐')) {
            let color = match mark {
                '✓' => palette.ok,
                '✗' => palette.error,
                _ => palette.info,
            };
            write(buf, inner.x + 1, y, 1, &mark.to_string(), style.fg(color));
        }
        // An amount: its total in the colour of its size, its chosen part
        // marked.
        if let Some(amount) = amounts.iter().find(|amount| amount.line == row + scroll) {
            let x = |at: usize| inner.x + 1 + u16::try_from(at).unwrap_or(0);
            let right = inner.x + inner.width.saturating_sub(1);
            let mut paint = |at: usize, len: usize, paint_style: Style| {
                let (from, len) = (x(at), u16::try_from(len).unwrap_or(0));
                for column in from..from.saturating_add(len).min(right) {
                    buf[(column, y)].set_style(paint_style);
                }
            };
            let color = match amount.level {
                Some(super::acting::Level::Danger) => Some(palette.error),
                Some(super::acting::Level::Warning) => Some(palette.warning),
                _ => None,
            };
            if let Some(color) = color {
                paint(amount.total.0, amount.total.1, style.fg(color));
            }
            for (part, start, len) in amount.parts {
                if amount.chosen == Some(part) {
                    paint(start + 1, len.saturating_sub(2), style.add_modifier(Modifier::REVERSED));
                }
            }
        }
    }
    for (row, (line, style)) in key_footer.iter().enumerate() {
        let y = inner.y + u16::try_from(height + row).unwrap_or(0);
        write(buf, inner.x + 1, y, inner.width.saturating_sub(2), line, *style);
    }
    scrollbar(buf, area, palette, Scrolls::Action, scroll, height, lines.len());
}

/// Where an amount of a form is drawn: its total and its two parts (each
/// `-…+`: start and width in characters from the line's start).
struct AmountRow {
    line: usize,
    field: usize,
    total: (usize, usize),
    level: Option<super::acting::Level>,
    parts: [(super::action::Part, usize, usize); 2],
    /// The part ← → change, when the field has the focus.
    chosen: Option<super::action::Part>,
}

/// `2.3G`, `64G`, `0.5G`: MiB as GiB with a tenth when not whole.
fn amount_text(mib: u64) -> String {
    if mib.is_multiple_of(super::action::MIB_PER_GIB) {
        format!("{}G", mib / super::action::MIB_PER_GIB)
    } else {
        #[allow(clippy::cast_precision_loss, reason = "home sizes are small")]
        let gib = mib as f64 / super::action::MIB_PER_GIB as f64;
        format!("{gib:.1}G")
    }
}

const NONE_ANSWERED: &str = "No HomeStack server instance answered: ";

/// A server's reason without the wrapping the client puts around it
/// (`No HomeStack server instance answered: <url>: <reason>`, nested).
#[must_use]
pub fn short_reason(text: &str) -> String {
    let mut rest = text.trim();
    while let Some(inner) = rest.strip_prefix(NONE_ANSWERED) {
        rest = inner;
        if (rest.starts_with("https://") || rest.starts_with("http://"))
            && let Some((_, after)) = rest.split_once("/: ")
        {
            rest = after;
        }
    }
    rest.to_owned()
}

/// The lines of a check of the servers that none answered.
#[must_use]
pub fn unreachable_lines(text: &str) -> Vec<String> {
    let Some(rest) = text.strip_prefix(NONE_ANSWERED) else {
        return vec![text.to_owned()];
    };
    let mut lines = vec!["No server answered.".to_owned(), String::new()];
    for part in rest.split("; https://") {
        let part = part.strip_prefix("https://").unwrap_or(part);
        let (host, reason) = part.split_once("/: ").unwrap_or((part, ""));
        lines.push(format!("✗ {host}  {}", short_reason(reason)));
    }
    lines.push(String::new());
    lines.push("Menu › Update API containers updates the saved deployment.".to_owned());
    lines
}

/// The spinner of what is being waited for.
pub(super) const SPINNER: [char; 10] = ['⠋', '⠙', '⠹', '⠸', '⠼', '⠴', '⠦', '⠧', '⠇', '⠏'];

/// A small framed box in the middle of `area` with `lines`: the start of
/// a connection and its failure (the setup's, the install's).
pub(super) fn message_box(buf: &mut Buffer, area: Rect, app: &App, title: &str, lines: &[(String, Style)]) {
    let palette = &app.palette;
    let longest = lines.iter().map(|(line, _)| line.chars().count()).max().unwrap_or(0);
    let width = u16::try_from(longest + 6)
        .unwrap_or(u16::MAX)
        .clamp(40, area.width.saturating_sub(4).max(10));
    let height = u16::try_from(lines.len() + 4).unwrap_or(u16::MAX).min(area.height);
    let rect = centered(area, width, height);
    let inner = frame(
        buf,
        rect,
        palette,
        &Sides {
            top_left: vec![Control::plain(title)],
            focused: true,
            ..Sides::default()
        },
    );
    for (row, (line, style)) in lines.iter().enumerate() {
        let y = inner.y + 1 + u16::try_from(row).unwrap_or(0);
        if y >= inner.bottom() {
            break;
        }
        write(buf, inner.x + 2, y, inner.width.saturating_sub(4), line, *style);
    }
}
