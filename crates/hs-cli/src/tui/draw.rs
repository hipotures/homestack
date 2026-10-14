//! Drawing the main screen (mockups 09–11, 15–17) from an [`App`].

use hs_model::status::{LayoutCapacity, StatusEntry};
use ratatui::Frame;
use ratatui::buffer::Buffer;
use ratatui::layout::{Position, Rect};
use ratatui::style::{Color, Modifier, Style};

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
fn write(buf: &mut Buffer, x: u16, y: u16, width: u16, text: &str, style: Style) -> u16 {
    let mut used = 0;
    for c in text.chars() {
        if used >= width {
            break;
        }
        let at = Position { x: x + used, y };
        if buf.area.contains(at) {
            let cell = &mut buf[(at.x, at.y)];
            cell.set_char(c);
            cell.set_style(style);
        }
        used += 1;
    }
    used
}

/// `text` cut to `width` characters, with "…" when cut.
fn cut(text: &str, width: usize) -> String {
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

fn bar(width: usize, percent: f64) -> String {
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
            parts.push((format!("   gold {} ", gold.vmid), text));
            parts.push(dot_state(palette, &gold.status));
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
                (format!("{}, {running} running", here.len()), text),
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
        lines.push(vec![
            (
                format!(
                    " gold {} {}  {}  ",
                    gold.vmid,
                    gold.name.as_deref().unwrap_or(""),
                    gold.node
                ),
                text,
            ),
            dot_state(palette, &gold.status),
            (format!(" {}", gold.status), muted),
        ]);
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
            width: 4,
            right: false,
            value: |entry| entry.vmid.to_string(),
        },
        Column {
            heading: "NAME",
            width: 13,
            right: false,
            value: |entry| entry.name.clone().unwrap_or_default(),
        },
        Column {
            heading: "NODE",
            width: 5,
            right: false,
            value: |entry| entry.node.clone(),
        },
        Column {
            heading: "STATE",
            width: 9,
            right: false,
            value: |entry| {
                let symbol = if entry.status == "running" { "●" } else { "○" };
                format!("{symbol} {}", entry.status)
            },
        },
        Column {
            heading: "ADDRESS",
            width: 11,
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
    // Step 1 is read-only: every action but info waits for step 2.
    vec![
        Control::key("info ↵").enabled(selected.is_some()),
        Control::key("&ssh").muted(),
        Control::key(if running { "s&top" } else { "s&tart" }).muted(),
        Control::key("s&etup").muted(),
        Control::key("&refresh").muted(),
        Control::key("resi&ze").muted(),
        Control::key("add&key").muted(),
        Control::key("mi&grate").muted(),
        Control::key("&destroy").muted(),
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
            Control::key("&new").muted(),
        ],
        bottom_left: workspace_actions(app),
        bottom_right: vec![Control::plain(&format!("{position}/{}", rows.len()))],
        focused: false,
    };
    let inner = frame(buf, area, palette, &sides);
    let text = text_style(palette);
    let muted = text.fg(palette.muted);
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
                    (reason.clone(), muted),
                ],
            );
            y += 1;
        }
        write(buf, inner.x + 2, y + 1, inner.width, "It tries again by itself.", muted);
        return;
    }
    if app.status.is_none() {
        write(buf, inner.x + 1, inner.y, inner.width, "Reading the status…", muted);
        return;
    }
    let mut list = inner;
    let selected = app.selected_entry();
    if let Some(entry) = selected {
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
    for (row, entry) in rows.iter().enumerate().skip(offset).take(visible) {
        let y = list.y + 1 + u16::try_from(row - offset).unwrap_or(0);
        let chosen = Some(row) == index;
        let mut style = text;
        if chosen {
            style = style.bg(palette.selection).add_modifier(Modifier::BOLD);
            if palette.no_color {
                style = style.add_modifier(Modifier::REVERSED);
            }
        }
        let line = list_line(&columns, |column| (column.value)(entry));
        let line = format!("{line:<width$}", width = usize::from(list.width));
        write(buf, list.x, y, list.width, &line, style);
        // The state dot in its colour.
        if let Some(at) = line.find(['●', '○']) {
            let x = list.x + u16::try_from(line[..at].chars().count()).unwrap_or(0);
            let color: Color = if entry.status == "running" {
                palette.ok
            } else {
                palette.muted
            };
            write(
                buf,
                x,
                y,
                1,
                if entry.status == "running" { "●" } else { "○" },
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

fn log(buf: &mut Buffer, area: Rect, app: &App, mut controls: Vec<Control>) {
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

fn centered(area: Rect, width: u16, height: u16) -> Rect {
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
            lines.iter().map(|line| (line.clone(), text)).collect(),
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

const HELP: [&str; 15] = [
    "1 2 3 4     show or hide workspaces, cluster, storage, log",
    "0           the standard view: every panel and node, no filter",
    "p           next preset (auto, 1, 2)",
    "m  esc      menu (esc first clears the filter)",
    "↑ ↓         select a workspace; PgUp PgDn Home End",
    "↵           details of the selected workspace",
    "← →         one node or all",
    "f           filter by name or VMID; ↵ keeps it, esc clears it",
    "q  ctrl-c   quit",
    "",
    "The actions in the bottom border of the workspaces panel",
    "(ssh, start/stop, setup, refresh, resize, add key, migrate,",
    "destroy, new) come in the next steps of phase 13b; until",
    "then they are grey. The CLI has them all:",
    "homestack --help",
];
