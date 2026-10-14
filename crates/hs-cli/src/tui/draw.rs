//! Drawing the main screen (mockups 09–11, 15–17) from an [`App`].

use crossterm::event::KeyCode;
use hs_model::status::{LayoutCapacity, StatusEntry};
use ratatui::Frame;
use ratatui::buffer::Buffer;
use ratatui::layout::{Position, Rect};
use ratatui::style::{Color, Modifier, Style};

use super::action::Action;
use super::chrome::{Control, Sides, frame, scrollbar, text_style};
use super::layout::{MIN_HEIGHT, MIN_WIDTH, arrange};
use super::state::{App, MENU, Overlay, PRESETS};
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

/// `text` cut to `width` characters, with "…" when cut.
pub(super) fn cut(text: &str, width: usize) -> String {
    if text.chars().count() <= width {
        return text.to_owned();
    }
    let mut short: String = text.chars().take(width.saturating_sub(1)).collect();
    short.push('…');
    short
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

fn first_label(url: &str) -> String {
    short_host(url).split('.').next().unwrap_or_default().to_owned()
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
    let menu = vec![
        Control::key("&menu"),
        Control::key(&format!("&preset {}", PRESETS[app.preset].0)),
    ];
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
    let sides = Sides {
        top_left,
        top_right: if app.clock.is_empty() {
            Vec::new()
        } else {
            vec![Control::plain(&app.clock)]
        },
        bottom_right: vec![Control::plain(&format!("{online}/{} nodes", nodes.len()))],
        ..Sides::default()
    };
    let inner = frame(buf, area, palette, &sides);
    let text = text_style(palette);
    let muted = text.fg(palette.muted);
    let gold = app.status.as_ref().and_then(|status| status.gold.as_ref());
    let server_dots: Vec<(String, Style)> = app
        .servers
        .iter()
        .map(|(_, error)| dot(palette, Some(error.is_none())))
        .collect();
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
        parts.push(("  servers ".to_owned(), text));
        parts.extend(server_dots);
        if let Some(gold) = gold {
            parts.push(("   gold ".to_owned(), text));
            parts.push(dot_state(palette, &gold.status));
            parts.push((format!(" {}", gold.vmid), text));
        }
        if let Some(percent) = free_percent(app) {
            parts.push((format!("   storage {percent:.0}% free"), text));
        }
        spans(buf, inner.x, inner.y, inner.width, &parts);
        return;
    }
    // The table of nodes, and a box with the servers and Gold on the right.
    let box_width = if inner.width >= 150 { 44 } else { 40 };
    let table_width = inner.width.saturating_sub(box_width + 1);
    write(
        buf,
        inner.x,
        inner.y,
        table_width,
        " NODE   STATE      WORKSPACES",
        muted,
    );
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
        spans(
            buf,
            inner.x,
            y,
            table_width,
            &[
                (format!(" {}  ", pad(&node.node, 5)), text),
                (symbol, style),
                (format!(" {} ", pad(&node.status, 8)), text),
                // Running and stopped as dots with their counts.
                ("● ".to_owned(), text.fg(palette.ok)),
                (format!("{running:<3}"), text),
                ("○ ".to_owned(), text.fg(palette.muted)),
                ((here.len() - running).to_string(), text),
            ],
        );
    }
    let sub = Rect::new(inner.x + inner.width - box_width, inner.y, box_width, inner.height);
    let sub_inner = frame(
        buf,
        sub,
        palette,
        &Sides {
            top_left: vec![Control::plain("servers & gold")],
            ..Sides::default()
        },
    );
    let mut lines: Vec<Vec<(String, Style)>> = Vec::new();
    let rows = usize::from(sub_inner.height);
    if rows > app.servers.len() {
        for (url, error) in &app.servers {
            let (symbol, style) = dot(palette, Some(error.is_none()));
            lines.push(vec![
                (" ".to_owned(), text),
                (symbol, style),
                (format!(" {}", short_host(url)), text),
            ]);
        }
    } else {
        let mut line = vec![(" servers".to_owned(), text)];
        for (url, error) in &app.servers {
            let (symbol, style) = dot(palette, Some(error.is_none()));
            line.push((" ".to_owned(), text));
            line.push((symbol, style));
            line.push((format!(" {}", first_label(url)), text));
        }
        lines.push(line);
    }
    if let Some(gold) = gold {
        // The state dot before the name, as for the servers; its word only
        // when it fits.
        let label = format!(" {} {}  {}", gold.vmid, gold.name.as_deref().unwrap_or(""), gold.node);
        let room = usize::from(sub_inner.width).saturating_sub(" gold ".len() + 1 + label.chars().count());
        let mut line = vec![
            (" gold ".to_owned(), text),
            dot_state(palette, &gold.status),
            (label, text),
        ];
        if gold.status.chars().count() + 2 <= room {
            line.push((format!("  {}", gold.status), muted));
        }
        lines.push(line);
    }
    for (row, line) in lines.iter().enumerate().take(rows) {
        let y = sub_inner.y + u16::try_from(row).unwrap_or(0);
        spans(buf, sub_inner.x, y, sub_inner.width, line);
    }
}

fn dot_state(palette: &Palette, status: &str) -> (String, Style) {
    let style = Style::default().bg(palette.background);
    if status == "running" {
        ("●".to_owned(), style.fg(palette.ok))
    } else {
        ("○".to_owned(), style.fg(palette.muted))
    }
}

/// A list column: heading, width and how a row fills it.
struct Column {
    heading: &'static str,
    width: usize,
    right: bool,
    value: fn(&StatusEntry) -> String,
}

fn columns(width: usize) -> Vec<Column> {
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
            width: 12,
            right: false,
            value: |entry| entry.name.clone().unwrap_or_default(),
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
                    return entry.status.clone();
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
    for mut column in all {
        if used + column.width < width {
            used += column.width + 1;
            chosen.push(column);
            continue;
        }
        let left = width.saturating_sub(used + 1);
        if left >= 9 {
            column.width = left;
            chosen.push(column);
        }
        break;
    }
    chosen
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
    // ssh and setup come in steps 4 and 5 of phase 13b.
    let free = selected.is_some() && app.action.is_none();
    vec![
        Control::key("&info").enabled(selected.is_some()),
        Control::key("&ssh").muted(),
        Control::key(if running { "s&top" } else { "s&tart" }).enabled(free),
        Control::key("s&etup").muted(),
        Control::key("&refresh").enabled(free),
        Control::key("resi&ze").enabled(free),
        Control::key("add&key").enabled(free),
        Control::key("mi&grate").enabled(free),
        Control::key("&destroy").enabled(free),
    ]
}

/// Label and value pairs of the selected workspace.
fn detail_pairs(app: &App, entry: &StatusEntry) -> Vec<(&'static str, String)> {
    let mut pairs = vec![("", format!("{} on {}", entry.status, entry.node))];
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
            pairs.push(("", "reading the details…".to_owned()));
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

fn workspaces(buf: &mut Buffer, area: Rect, app: &App, mut controls: Vec<Control>) {
    let palette = &app.palette;
    let rows = app.rows();
    let mut top_left = vec![Control::panel(1, "workspaces")];
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
            Control::plain(&format!("‹ node: {node} ›")),
            Control::key("&new").enabled(app.status.is_some() && app.action.is_none()),
        ],
        bottom_left: workspace_actions(app),
        bottom_right: vec![Control::plain(&format!("{position}/{}", rows.len()))],
        focused: false,
    };
    let inner = frame(buf, area, palette, &sides);
    let text = text_style(palette);
    let muted = text.fg(palette.muted);
    if app.status.is_none() && app.unreachable.is_empty() && app.notice.is_none() {
        // A moving bar and the time in the middle of the panel, so the start
        // is never a still screen.
        let seconds = f64::from(app.waiting) / 10.0;
        let words = format!("Reading the status from the servers…  {seconds:.1} s");
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
            "It tries again by itself. Menu › Install or reconfigure deploys the servers again.",
            muted,
        );
        return;
    }
    if app.status.is_none() {
        write(buf, inner.x + 1, inner.y, inner.width, "Reading the status…", muted);
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
    } else if let Some(entry) = selected {
        let wide = inner.width >= 82;
        if wide {
            let box_width = if inner.width >= 108 { 45 } else { 35 };
            let sub = Rect::new(inner.right() - box_width, inner.y, box_width, inner.height.min(10));
            details_box(buf, sub, app, entry, false);
            list.width -= box_width + 1;
        } else if app.info {
            let sub = Rect::new(inner.x, inner.y, inner.width, 7.min(inner.height));
            details_box(buf, sub, app, entry, true);
            list.y += sub.height;
            list.height -= sub.height;
        }
    }
    if list.height == 0 {
        return;
    }
    let columns = columns(usize::from(list.width));
    write(
        buf,
        list.x,
        list.y,
        list.width,
        &list_line(&columns, |column| column.heading.to_owned()),
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
        }
        let mut shown = (*entry).clone();
        if let Some(action) = app
            .action
            .as_ref()
            .filter(|action| action.busy() && action.vmid == Some(entry.vmid))
        {
            shown.status = format!("◐ {}", action.kind.word());
        }
        let line = list_line(&columns, |column| (column.value)(&shown));
        let line = format!("{line:<width$}", width = usize::from(list.width));
        write(buf, list.x, y, list.width, &line, style);
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
    scrollbar(buf, bar_area, palette, offset, visible, rows.len());
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

pub(super) fn log(buf: &mut Buffer, area: Rect, app: &App, mut controls: Vec<Control>) {
    let palette = &app.palette;
    let mut top_left = vec![Control::panel(4, "log")];
    top_left.append(&mut controls);
    let sides = Sides {
        top_left,
        bottom_right: vec![Control::plain(&match app.lines.len() {
            1 => "1 line".to_owned(),
            count => format!("{count} lines"),
        })],
        ..Sides::default()
    };
    let inner = frame(buf, area, palette, &sides);
    let text = text_style(palette);
    let muted = text.fg(palette.muted);
    for (row, line) in app.lines.iter().rev().take(usize::from(inner.height)).enumerate() {
        let y = inner.y + u16::try_from(row).unwrap_or(0);
        spans(
            buf,
            inner.x,
            y,
            inner.width,
            &[(format!(" {}  ", line.time), muted), (line.text.clone(), text)],
        );
    }
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

const HELP: [&str; 18] = [
    "1 2 3 4     show or hide workspaces, cluster, storage, log",
    "0           the standard view: every panel and node, no filter",
    "p           next preset (auto, 1, 2)",
    "m           menu",
    "↑ ↓         select a workspace; PgUp PgDn Home End",
    "↵           details of the selected workspace",
    "← →         one node or all",
    "f           filter by name or VMID; ↵ keeps it, esc clears it",
    "q  esc      quit (esc first clears the filter)",
    "",
    "On the selected workspace (a box opens in the panel):",
    "n new   r refresh   z resize   k add key   g migrate   d destroy",
    "In the box: tab next field, ← → choose, ↵ plan, a apply,",
    "esc cancel (while it runs esc hides the box; ↵ shows it).",
    "",
    "ssh, start/stop and setup come in the next steps of phase 13b;",
    "until then they are grey. The CLI has every command:",
    "homestack --help",
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
fn action_box(buf: &mut Buffer, area: Rect, app: &App, action: &Action) {
    use super::action::{Input, PlanFocus, Stage, StepState};
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
    let raised = |control: Control, on: bool| if on { control.raised() } else { control };
    let bottom_left = match &action.stage {
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
                    Input::Text(value) => {
                        let cursor = if focused { "▏" } else { " " };
                        format!(
                            "[ {value}{cursor}{} ]",
                            " ".repeat(24usize.saturating_sub(value.chars().count()))
                        )
                    }
                    Input::Choice { options, index } => {
                        format!("‹ {} ›", options.get(*index).map_or("—", String::as_str))
                    }
                };
                let mut style = text;
                if focused {
                    style = style.bg(palette.selection).add_modifier(Modifier::BOLD);
                }
                field_rows.push((lines.len(), index));
                lines.push((format!("{:<11}{value}", field.label), style));
            }
            lines.push((String::new(), text));
            if let Some(field) = form.fields.get(form.focus) {
                push(&field.note, muted, &mut lines);
            }
            if let Some(error) = &form.error {
                push(error, text.fg(palette.error), &mut lines);
            }
        }
        Stage::Asking => lines.push(("asking the server for the plan…".to_owned(), muted)),
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
                };

                push(&format!("{mark} {}", step.name), text, &mut lines);
            }
            let done = steps.iter().filter(|step| step.state == StepState::Done).count();
            lines.push((String::new(), text));
            let label = if steps.is_empty() {
                "starting…".to_owned()
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
    let height = usize::from(inner.height);
    let scroll = scroll.min(lines.len().saturating_sub(height));
    for (line, field) in &field_rows {
        if *line < scroll || *line >= scroll + height {
            continue;
        }
        let y = inner.y + u16::try_from(line - scroll).unwrap_or(0);
        super::chrome::register(
            Rect::new(inner.x, y, inner.width, 1),
            super::chrome::Hit::Field(*field, None),
        );
        let text = &lines[*line].0;
        for (mark, key) in [('‹', KeyCode::Left), ('›', KeyCode::Right)] {
            if let Some(at) = text.chars().position(|c| c == mark) {
                let x = inner.x + 1 + u16::try_from(at).unwrap_or(0);
                super::chrome::register(Rect::new(x, y, 1, 1), super::chrome::Hit::Field(*field, Some(key)));
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
    }
    scrollbar(buf, area, palette, scroll, height, lines.len());
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
    lines.push("Menu › Install or reconfigure deploys the servers again.".to_owned());
    lines
}
