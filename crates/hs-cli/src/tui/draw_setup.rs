//! Drawing the setup box (mockups 23–24): the groups in the top border,
//! the line of the target, the filter, the tree beside its details (below
//! them when narrow), the review and the run.

use ratatui::buffer::Buffer;
use ratatui::layout::Rect;
use ratatui::style::{Modifier, Style};

use super::chrome::{Control, Hit, Sides, frame, register, scrollbar, text_style};
use super::draw::{cut, wrap, write};
use super::setup::{SetupBox, Stage, Tone};
use super::state::App;
use super::theme::Palette;

const SPINNER: [char; 10] = ['⠋', '⠙', '⠹', '⠸', '⠼', '⠴', '⠦', '⠧', '⠇', '⠏'];

fn tone_style(palette: &Palette, base: Style, tone: Tone) -> Style {
    match tone {
        Tone::Plain => base,
        Tone::Dim => base.fg(palette.muted),
        Tone::Green => base.fg(palette.ok),
        Tone::Red => base.fg(palette.error).add_modifier(Modifier::BOLD),
        Tone::Yellow => base.fg(palette.warning),
        Tone::Bold => base.add_modifier(Modifier::BOLD),
    }
}

/// Lines from `lines`, each wrapped to `width`.
fn wrapped(lines: &[String], width: usize) -> Vec<String> {
    lines.iter().flat_map(|line| wrap(line, width)).collect()
}

/// The box over `area`.
pub(super) fn setup_box(buf: &mut Buffer, area: Rect, app: &App, setup: &SetupBox) {
    let palette = &app.palette;
    let text = text_style(palette);
    let title = if setup.target.local {
        "setup this desktop".to_owned()
    } else {
        format!("setup {} {}", setup.target.vmid, setup.target.name)
    };
    let mut top_left = vec![Control::plain(&title)];
    let groups = setup.groups();
    let tree = matches!(setup.stage, Stage::Tree);
    if !groups.is_empty() && !matches!(setup.stage, Stage::Loading { .. } | Stage::Failed { .. }) {
        let label = groups.get(setup.section).cloned().unwrap_or_default();
        let carousel = Control::plain(&format!("‹ {label} ›"));
        top_left.push(if tree { carousel.raised() } else { carousel.muted() });
    }
    for (word, on) in [
        ("review", matches!(setup.stage, Stage::Review { .. })),
        ("run", matches!(setup.stage, Stage::Running { .. })),
    ] {
        let control = Control::plain(word);
        top_left.push(if on { control.raised() } else { control.muted() });
    }
    let pending = !setup.selected.is_empty() && !setup.busy;
    let bottom_left = match &setup.stage {
        Stage::Loading { .. } => vec![Control::key("cancel esc")],
        Stage::Failed { .. } => vec![Control::key("close ↵")],
        Stage::Tree => {
            let review = Control::plain("review & apply ↵").enabled(!setup.busy);
            vec![
                Control::key("toggle ␣"),
                Control::key("group ⇥"),
                if pending { review.chosen() } else { review },
                Control::key("cancel esc"),
            ]
        }
        Stage::Review { apply, .. } => {
            let back = Control::key("back esc");
            let run = Control::key("&apply this plan ↵");
            vec![
                if *apply { back } else { back.chosen() },
                if *apply { run.chosen() } else { run },
            ]
        }
        Stage::Running { finished: true, .. } => vec![Control::key("OK ↵").chosen()],
        Stage::Running { .. } => vec![Control::key("cancel esc")],
    };
    let bottom_right = if tree {
        vec![Control::plain(&format!("{}/{}", setup.section + 1, groups.len()))]
    } else {
        Vec::new()
    };
    let inner = frame(
        buf,
        area,
        palette,
        &Sides {
            top_left,
            bottom_left,
            bottom_right,
            focused: true,
            ..Sides::default()
        },
    );
    if inner.height == 0 || inner.width < 4 {
        return;
    }
    let width = usize::from(inner.width.saturating_sub(2));
    let spinner = SPINNER[setup.frame % SPINNER.len()];
    match &setup.stage {
        Stage::Loading { message } => {
            let y = inner.y + inner.height / 2;
            write(
                buf,
                inner.x + 2,
                y,
                inner.width.saturating_sub(4),
                &format!("{spinner} {message}"),
                text,
            );
        }
        Stage::Failed { lines } => {
            let style = text.fg(palette.error);
            for (row, line) in wrapped(lines, width).iter().take(usize::from(inner.height)).enumerate() {
                let y = inner.y + u16::try_from(row).unwrap_or(0);
                write(buf, inner.x + 1, y, inner.width.saturating_sub(2), line, style);
            }
        }
        Stage::Review { lines, scroll, .. } => {
            let all = wrapped(lines, width);
            let height = usize::from(inner.height);
            let scroll = (*scroll).min(all.len().saturating_sub(height));
            for (row, line) in all.iter().skip(scroll).take(height).enumerate() {
                let y = inner.y + u16::try_from(row).unwrap_or(0);
                let style = if row + scroll == 0 {
                    text.add_modifier(Modifier::BOLD)
                } else {
                    text
                };
                write(buf, inner.x + 1, y, inner.width.saturating_sub(2), line, style);
            }
            scrollbar(buf, area, palette, scroll, height, all.len());
        }
        Stage::Running {
            lines,
            finished,
            title,
            auth_wait,
        } => running(buf, inner, app, (lines, *finished, title, *auth_wait), setup.frame),
        Stage::Tree => tree_stage(buf, inner, app, setup),
    }
    if let Some(notice) = &setup.notice {
        let y = inner.bottom().saturating_sub(1);
        let style = if notice.error {
            text.fg(palette.error)
        } else {
            text.fg(palette.warning)
        };
        let line = cut(&notice.text, width);
        write(
            buf,
            inner.x + 1,
            y,
            inner.width.saturating_sub(2),
            &format!("{line:<width$}"),
            style,
        );
    }
}

/// The run (Python `Execution`): the title, the step with its spinner, the
/// activity, the result at the end.
fn running(
    buf: &mut Buffer,
    inner: Rect,
    app: &App,
    (lines, finished, title, auth_wait): (&[String], bool, &str, bool),
    frame: usize,
) {
    let spinner = SPINNER[frame % SPINNER.len()];
    let palette = &app.palette;
    let text = text_style(palette);
    let width = usize::from(inner.width.saturating_sub(2));
    write(
        buf,
        inner.x + 1,
        inner.y,
        inner.width.saturating_sub(2),
        title,
        text.add_modifier(Modifier::BOLD),
    );
    let mut top = inner.y + 2;
    if !finished && auth_wait {
        // Python bright and dim: the frame of the spinner alternates the colour.
        let bright = frame % 16 < 8;
        let colour = if bright { palette.warning } else { palette.accent };
        let rows = [
            (
                format!("{spinner} YubiKey · SSH authentication"),
                text.fg(colour).add_modifier(Modifier::BOLD),
            ),
            (
                "  Touch your YubiKey/security key if it flashes.".to_owned(),
                text.add_modifier(Modifier::BOLD),
            ),
            ("  Connecting desktop Herdr…".to_owned(), text.fg(palette.muted)),
        ];
        for (line, style) in rows {
            write(buf, inner.x + 1, top, inner.width.saturating_sub(2), &line, style);
            top += 1;
        }
        top += 1;
    } else if !finished {
        let step = lines.last().cloned().unwrap_or_else(|| "Preparing setup…".to_owned());
        write(
            buf,
            inner.x + 1,
            top,
            inner.width.saturating_sub(2),
            &format!("{spinner} {}", cut(&step, width.saturating_sub(2))),
            text,
        );
        top += 2;
    }
    let all = wrapped(lines, width);
    let height = usize::from(inner.bottom().saturating_sub(top));
    // The log stays at its end.
    let scroll = all.len().saturating_sub(height);
    for (row, line) in all.iter().skip(scroll).take(height).enumerate() {
        let y = top + u16::try_from(row).unwrap_or(0);
        let style = if line.contains(": failed —") || line.contains(": blocked —") {
            text.fg(palette.error)
        } else if line.contains(": succeeded —") || line.contains(": already-ready —") {
            text.fg(palette.ok)
        } else {
            text
        };
        write(buf, inner.x + 1, y, inner.width.saturating_sub(2), line, style);
    }
}

/// The line of the target, the filter, the tree and the details.
fn tree_stage(buf: &mut Buffer, inner: Rect, app: &App, setup: &SetupBox) {
    let palette = &app.palette;
    let text = text_style(palette);
    let muted = text.fg(palette.muted);
    let width = usize::from(inner.width.saturating_sub(2));
    let label = wrap(&setup.target_label(&setup.last_progress), width);
    let mut y = inner.y;
    for line in label.iter().take(2) {
        write(buf, inner.x + 1, y, inner.width.saturating_sub(2), line, muted);
        y += 1;
    }
    let filter = if setup.filtering || !setup.filter.is_empty() {
        let cursor = if setup.filtering { "▏" } else { "" };
        (format!("Filter: {}{cursor}", setup.filter), text)
    } else {
        (
            "Filter (/). Bulk selection affects visible matches only.".to_owned(),
            muted,
        )
    };
    write(buf, inner.x + 1, y, inner.width.saturating_sub(2), &filter.0, filter.1);
    y += 1;
    y = group_bar(buf, inner, y, app, setup);
    y += 1;
    let notice_rows = u16::from(setup.notice.is_some());
    let body = Rect::new(
        inner.x,
        y,
        inner.width,
        inner.bottom().saturating_sub(y).saturating_sub(notice_rows),
    );
    if body.height == 0 {
        return;
    }
    // Tree 3 : details 2 side by side (Python's 3fr / 2fr); below each
    // other when narrow.
    let (tree_area, details_area) = if body.width >= 70 {
        let tree_width = body.width * 3 / 5;
        (
            Rect::new(body.x, body.y, tree_width, body.height),
            Rect::new(
                body.x + tree_width + 1,
                body.y,
                body.width - tree_width - 1,
                body.height,
            ),
        )
    } else {
        let tree_height = (body.height * 3 / 5).max(1);
        (
            Rect::new(body.x, body.y, body.width, tree_height),
            Rect::new(body.x, body.y + tree_height, body.width, body.height - tree_height),
        )
    };
    draw_tree(buf, tree_area, app, setup);
    if details_area.height > 0 && details_area.width > 2 {
        draw_details(buf, details_area, app, setup);
    }
}

fn draw_tree(buf: &mut Buffer, area: Rect, app: &App, setup: &SetupBox) {
    let palette = &app.palette;
    let text = text_style(palette);
    let rows = setup.rows();
    let height = usize::from(area.height);
    let at = rows.iter().position(|row| row.id == setup.cursor).unwrap_or(0);
    let offset = (at + 1).saturating_sub(height);
    for (index, row) in rows.iter().enumerate().skip(offset).take(height) {
        let y = area.y + u16::try_from(index - offset).unwrap_or(0);
        let Some(node) = setup.nodes.get(&row.id) else {
            continue;
        };
        let chosen = row.id == setup.cursor;
        let base = if chosen {
            let style = text.bg(palette.selection).add_modifier(Modifier::BOLD);
            if palette.no_color {
                style.add_modifier(Modifier::REVERSED)
            } else {
                style
            }
        } else {
            text
        };
        if chosen {
            write(buf, area.x, y, area.width, &" ".repeat(usize::from(area.width)), base);
        }
        let indent = u16::try_from(row.depth * 2).unwrap_or(0);
        let mut x = area.x + 1 + indent;
        register(Rect::new(area.x, y, area.width, 1), Hit::SetupRow(index, 2));
        if row.depth > 0 {
            let arrow = if !row.branch {
                "  "
            } else if row.expanded {
                "▾ "
            } else {
                "▸ "
            };
            if row.branch {
                register(Rect::new(x, y, 2, 1), Hit::SetupRow(index, 0));
            }
            x += write(buf, x, y, 2, arrow, base.fg(palette.muted));
        }
        register(Rect::new(x, y, 3, 1), Hit::SetupRow(index, 1));
        let right = area.right();
        for (part, tone) in &node.label {
            if x >= right {
                break;
            }
            let mut style = tone_style(palette, base, *tone);
            if row.depth == 0 {
                style = style.add_modifier(Modifier::BOLD);
            }
            x += write(buf, x, y, right - x, part, style);
        }
    }
    let bar = Rect::new(area.x, area.y.saturating_sub(1), area.width + 1, area.height + 2);
    scrollbar(buf, bar, palette, offset, height, rows.len());
}

fn draw_details(buf: &mut Buffer, area: Rect, app: &App, setup: &SetupBox) {
    let palette = &app.palette;
    let text = text_style(palette);
    register(area, Hit::SetupDetails);
    let details = setup.details(&setup.cursor);
    let width = usize::from(area.width.saturating_sub(2));
    let lines: Vec<String> = details.lines().flat_map(|line| wrap(line, width)).collect();
    let height = usize::from(area.height);
    let scroll = setup.details_scroll.min(lines.len().saturating_sub(height));
    let status = SetupBox::status_tone(&details);
    for (row, line) in lines.iter().skip(scroll).take(height).enumerate() {
        let y = area.y + u16::try_from(row).unwrap_or(0);
        let style = match status {
            Some(tone) if row + scroll == 0 => tone_style(palette, text, tone).add_modifier(Modifier::BOLD),
            _ => text,
        };
        write(buf, area.x + 1, y, area.width.saturating_sub(2), line, style);
    }
    let bar = Rect::new(area.x, area.y.saturating_sub(1), area.width, area.height + 2);
    scrollbar(buf, bar, palette, scroll, height, lines.len());
}

/// Every group with its count, the shown one raised; a click shows one.
/// The bar wraps when the groups do not fit on one line.
fn group_bar(buf: &mut Buffer, inner: Rect, mut y: u16, app: &App, setup: &SetupBox) -> u16 {
    let palette = &app.palette;
    let text = text_style(palette);
    let right = inner.right().saturating_sub(1);
    let mut x = inner.x + 1;
    for (index, group) in setup.settings.setup.groups.iter().enumerate() {
        let (count, total) = setup.group_count(&group.id);
        let label = format!(" {} {count}/{total} ", group.label);
        let width = u16::try_from(label.chars().count()).unwrap_or(0);
        if x + width > right && x > inner.x + 1 {
            x = inner.x + 1;
            y += 1;
        }
        if y >= inner.bottom() {
            break;
        }
        let style = if index == setup.section {
            text.fg(palette.background)
                .bg(palette.focus)
                .add_modifier(Modifier::BOLD)
        } else if count > 0 {
            text.add_modifier(Modifier::BOLD)
        } else {
            text.fg(palette.muted)
        };
        let used = write(buf, x, y, right.saturating_sub(x), &label, style);
        register(Rect::new(x, y, used, 1), Hit::SetupGroup(index));
        x += used + 1;
    }
    y + 1
}
