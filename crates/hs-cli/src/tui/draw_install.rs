//! Drawing the install screen (mockups 02–08, 18) and ssh's question box.

use crossterm::event::KeyCode;
use ratatui::buffer::Buffer;
use ratatui::layout::Rect;
use ratatui::style::{Modifier, Style};

use super::chrome::{Control, Scrolls, Sides, frame, scrollbar, text_style};
use super::draw::{SPINNER, bar, centered, connection_rect, cut, log, message_box, wrap, write};
use super::install::{AskView, Install, Method, Mode, Stage};
use super::state::App;
use crate::askpass::Kind;

/// Seconds ssh gives a security key, shown counting down.
const TOUCH_SECONDS: u64 = 30;

/// The rows of the log panel at 120 x 36 and more.
const LOG_ROWS: u16 = 7;

fn at(y: u16, row: usize) -> u16 {
    y + u16::try_from(row).unwrap_or(u16::MAX)
}

/// The install panel (and the log under it when there is room).
pub(super) fn screen(buf: &mut Buffer, area: Rect, app: &App, install: &Install) {
    let (main, below) = if area.width >= 120 && area.height >= 36 {
        let height = area.height - LOG_ROWS;
        (
            Rect::new(area.x, area.y, area.width, height),
            Some(Rect::new(area.x, area.y + height, area.width, LOG_ROWS)),
        )
    } else {
        (area, None)
    };
    let palette = &app.palette;
    let (step, steps) = install.step();
    let all_done = install.stage == Stage::Done && install.ok;
    let counter = if install.stage == Stage::Done {
        if install.ok {
            "done".to_owned()
        } else {
            "not finished".to_owned()
        }
    } else {
        format!("step {} of {steps}", step + 1)
    };
    let sides = Sides {
        bottom_left: buttons(install),
        bottom_right: vec![
            Control::named("F2", "copy", crossterm::event::KeyCode::F(2)),
            Control::plain(&counter),
        ],
        focused: true,
        ..Sides::default()
    };
    let inner = frame(buf, main, palette, &sides);
    // The steps in the top border: done ✓, the current one raised.
    let border = Style::default().fg(palette.focus).bg(palette.background);
    let text = text_style(palette);
    let title_style = text.add_modifier(Modifier::BOLD);
    let mut x = main.x + 1;
    let room = main.right().saturating_sub(2);
    x += write(buf, x, main.y, room.saturating_sub(x), "─ ", border);
    x += write(
        buf,
        x,
        main.y,
        room.saturating_sub(x),
        &if app.profile == hs_config::profiles::DEFAULT {
            install.title()
        } else {
            format!("{} [{}]", install.title(), app.profile)
        },
        title_style,
    );
    x += write(buf, x, main.y, room.saturating_sub(x), " ─", border);
    for (index, name) in install.mode.steps().iter().enumerate() {
        if index == step && !all_done {
            x += write(buf, x, main.y, room.saturating_sub(x), "┌", border);
            x += write(
                buf,
                x,
                main.y,
                room.saturating_sub(x),
                name,
                text.fg(palette.focus).add_modifier(Modifier::BOLD),
            );
            x += write(buf, x, main.y, room.saturating_sub(x), "┐─", border);
        } else {
            let label = if index < step || all_done {
                format!(" ✓{name} ─")
            } else {
                format!(" {name} ─")
            };
            x += write(buf, x, main.y, room.saturating_sub(x), &label, text.fg(palette.muted));
        }
    }
    if let Some(found) = &install.found {
        let label = format!(" {} ", found.root);
        let width = u16::try_from(label.chars().count()).unwrap_or(0);
        let start = room.saturating_sub(width);
        if start > x {
            write(buf, start, main.y, width, &label, text.fg(palette.muted));
        }
    }
    match install.stage {
        Stage::Access => access(buf, inner, app, install),
        Stage::Check => check(buf, inner, app, install),
        Stage::Settings => {
            settings(buf, inner, app, install);
            if install.help {
                settings_help(buf, inner, app, install);
            }
            // F5: the rescan in the connection's box.
            if let Some(busy) = &install.busy {
                let spinner = SPINNER[install.frame % SPINNER.len()];
                let text = text_style(palette);
                let title = if install.found.is_some() {
                    "rescan cluster"
                } else {
                    "reading"
                };
                message_box(buf, app, title, &[(format!("{spinner} {busy}"), text)]);
            }
            if let Some(failed) = &install.failed {
                let text = text_style(palette);
                let style = text.fg(palette.error);
                let mut lines: Vec<(String, Style)> = wrap(failed, 64).into_iter().map(|line| (line, style)).collect();
                lines.push((String::new(), text));
                lines.push(("ENTER or ESC closes.".to_owned(), text.fg(palette.muted)));
                message_box(buf, app, "failed", &lines);
            }
        }
        Stage::Review => review(buf, inner, app, install),
        Stage::Run => run(buf, inner, app, install),
        Stage::Done => done(buf, inner, app, install),
    }
    if let Some(below) = below {
        log(buf, below, app, Vec::new());
    }
}

/// The buttons of the stage in the bottom border.
pub(super) fn buttons(install: &Install) -> Vec<Control> {
    match install.stage {
        Stage::Access if install.failed.is_some() => vec![Control::key("close Esc")],
        Stage::Access if install.connecting() => vec![Control::key("cancel Esc")],
        Stage::Access => vec![
            Control::key("field ↑↓"),
            Control::key("Connect Enter"),
            Control::key(if install.first_start { "Quit Esc" } else { "back Esc" }),
        ],
        Stage::Check => vec![
            Control::key(match install.mode {
                Mode::Install | Mode::Sites | Mode::Service => "&Settings Enter",
                Mode::Uninstall => "&Review Enter",
            }),
            Control::key("back Esc"),
        ],
        Stage::Settings => settings_buttons(install),
        Stage::Review => {
            let ready = install.reviewed && install.problems.is_empty();
            let back = Control::named("ESC", "back", KeyCode::Esc);
            let scroll = Control::named("↑↓", "scroll", KeyCode::Down);
            match install.mode {
                Mode::Install => vec![
                    scroll,
                    Control::named("I", "install", KeyCode::Char('i')).enabled(ready),
                    back,
                ],
                Mode::Service if install.removing => vec![
                    scroll,
                    Control::named("DEL", "remove", KeyCode::Delete).enabled(ready),
                    back,
                ],
                Mode::Service | Mode::Sites => vec![
                    scroll,
                    Control::named("D", "deploy", KeyCode::Char('d')).enabled(ready),
                    back,
                ],
                Mode::Uninstall => {
                    let (remove, cancel) = if install.remove_focused {
                        ("▶Remove Enter", "cancel")
                    } else {
                        ("Remove", "▶cancel Enter")
                    };
                    vec![Control::key(remove).enabled(ready), Control::key(cancel)]
                }
            }
        }
        Stage::Run => vec![Control::key("stop Esc").muted()],
        Stage::Done if install.ok && matches!(install.mode, Mode::Sites | Mode::Service) => {
            vec![Control::key("close Enter")]
        }
        Stage::Done if install.mode == Mode::Service => vec![Control::key("back to the settings Enter or Esc")],
        Stage::Done if install.ok => vec![Control::key("&Open HomeStack Enter"), Control::key("&Quit")],
        Stage::Done => vec![Control::key("&back to the review Enter or Esc"), Control::key("&Quit")],
    }
}

/// The keys of the settings: on the list, or in a setting's box.
fn settings_buttons(install: &Install) -> Vec<Control> {
    let esc = |action: &str| Control::named("ESC", action, KeyCode::Esc);
    if !install.chooser {
        let mut keys = vec![
            Control::named("↑↓", "setting", KeyCode::Down),
            Control::named("ENTER", "edit", KeyCode::Enter),
            Control::named("R", "review", KeyCode::Char('r')),
            Control::named("F5", "rescan", KeyCode::F(5)),
        ];
        if install.removable() {
            keys.push(Control::named("DEL", "remove ticked", KeyCode::Delete));
        }
        keys.extend([Control::named("?", "help", KeyCode::Char('?')), esc("back")]);
        return keys;
    }
    let setting = install
        .settings
        .get(install.row)
        .filter(|_| !(install.domain.is_some() && install.row == install.settings.len()));
    let mut keys = Vec::new();
    match setting {
        Some(setting) if setting.auto_text => keys.push(Control::named("SPACE", "select", KeyCode::Char(' '))),
        Some(setting) if setting.list => keys.extend([
            Control::named("+", "add", KeyCode::Char('+')),
            Control::named("-", "remove", KeyCode::Char('-')),
        ]),
        Some(setting) if setting.text.is_none() && setting.multiple => {
            keys.push(Control::named("SPACE", "tick", KeyCode::Char(' ')));
        }
        _ => {}
    }
    let keep = match setting {
        Some(setting) if setting.text.is_none() && !setting.list && !setting.multiple => "choose",
        _ => "keep",
    };
    keys.extend([
        Control::named("ENTER", keep, KeyCode::Enter),
        Control::named("TAB", "next", KeyCode::Tab),
        esc("cancel"),
    ]);
    keys
}

/// Paragraph lines from `y`, wrapped to the inside; the rows used.
fn paragraph(buf: &mut Buffer, inner: Rect, y: u16, text: &str, style: Style) -> usize {
    let width = usize::from(inner.width.saturating_sub(4));
    let lines = wrap(text, width);
    for (row, line) in lines.iter().enumerate() {
        write(buf, inner.x + 2, at(y, row), inner.width.saturating_sub(4), line, style);
    }
    lines.len()
}

fn access(buf: &mut Buffer, inner: Rect, app: &App, install: &Install) {
    let palette = &app.palette;
    let text = text_style(palette);
    let muted = text.fg(palette.muted);
    let focus = text.fg(palette.focus).add_modifier(Modifier::BOLD);
    let intro = match install.mode {
        Mode::Install => {
            "HomeStack needs root on one node once: it creates its user, roles, pool and token there, then a \
             server on every node. Nothing changes in Proxmox until you press Install in the review step."
        }
        Mode::Uninstall => {
            "Root on one node is needed again to remove what HomeStack made in Proxmox. Nothing changes until \
             you press Remove in the review step; no VM, container or volume is ever removed."
        }
        Mode::Sites => {
            "Root on one node over SSH reads the cluster and hands each connector its own Site credential on \
             standard input. Nothing changes in Pangolin or Proxmox until you press Deploy in the review step."
        }
        Mode::Service => {
            "The deploy token reads the cluster. Nothing changes until you press Deploy in the review step."
        }
    };
    let mut y = inner.y + 1;
    y += u16::try_from(paragraph(buf, inner, y, intro, text)).unwrap_or(0) + 1;
    let x = inner.x + 2;
    let width = inner.width.saturating_sub(4);
    write(buf, x, y, 10, "Method", text);
    let method = match install.method {
        Method::Ssh => "‹ SSH ›",
        Method::Herdr => "‹ Herdr pane ›",
    };
    write(
        buf,
        x + 10,
        y,
        width.saturating_sub(10),
        method,
        if install.field == 2 { focus } else { text },
    );
    write(
        buf,
        x + 28,
        y,
        width.saturating_sub(28),
        if install.mode == Mode::Sites {
            "SSH only: credentials never pass through a pane"
        } else {
            "↑ then ← →: SSH or a Herdr pane"
        },
        muted,
    );
    y += 2;
    match install.method {
        Method::Ssh => {
            write(buf, x, y, 10, "Host", text);
            let cursor = if install.field == 0 { "▏" } else { " " };
            let before: String = install.host.chars().take(install.caret).collect();
            let after: String = install.host.chars().skip(install.caret).collect();
            let field = format!(
                "[ {before}{cursor}{after}{} ]",
                " ".repeat(30usize.saturating_sub(install.host.chars().count()))
            );
            write(
                buf,
                x + 10,
                y,
                width.saturating_sub(10),
                &field,
                if install.field == 0 { focus } else { text },
            );
            write(
                buf,
                x + 10,
                y + 1,
                width.saturating_sub(10),
                "USER@HOST of one node; ssh may ask for a touch, a PIN or the host key here.",
                muted,
            );
        }
        Method::Herdr => {
            write(buf, x, y, 10, "Filter", text);
            let cursor = if install.field == 0 { "▏" } else { " " };
            let before: String = install.tab_filter.chars().take(install.tab_caret).collect();
            let after: String = install.tab_filter.chars().skip(install.tab_caret).collect();
            let filter_box = format!(
                "[ {before}{cursor}{after}{} ]",
                " ".repeat(20usize.saturating_sub(install.tab_filter.chars().count()))
            );
            write(
                buf,
                x + 10,
                y,
                width.saturating_sub(10),
                &filter_box,
                if install.field == 0 { focus } else { text },
            );
            write(
                buf,
                x + 36,
                y,
                width.saturating_sub(36),
                "type to filter WORKSPACE/TAB (empty shows all)",
                muted,
            );
            y += 2;
            write(buf, x, y, 10, "Pane", text);
            let matching = install.matching_tabs();
            let line = matching.get(install.tab).map_or_else(
                || "‹ no matching Herdr tabs ›".to_owned(),
                |(name, title)| format!("‹ {name}  {title} ›"),
            );
            write(
                buf,
                x + 10,
                y,
                width.saturating_sub(10),
                &line,
                if install.field == 0 { focus } else { text },
            );
            write(
                buf,
                x + 10,
                y + 1,
                width.saturating_sub(10),
                "A Herdr tab whose shell is root on a node, at its prompt.",
                muted,
            );
            y += 3;
            let room = inner.bottom().saturating_sub(y + 2) as usize;
            if room > 0 && !matching.is_empty() {
                let start = install
                    .tab
                    .saturating_sub(room / 2)
                    .min(matching.len().saturating_sub(room));
                for (offset, (name, title)) in matching.iter().skip(start).take(room).enumerate() {
                    let index = start + offset;
                    let picked = index == install.tab;
                    let mark = if picked { "›" } else { " " };
                    let row_style = if picked && install.field == 0 {
                        focus
                    } else if picked {
                        text
                    } else {
                        muted
                    };
                    let entry = format!("{mark} {name:<18}  {title}");
                    write(buf, x + 10, y, width.saturating_sub(10), &entry, row_style);
                    y += 1;
                }
            }
        }
    }
    if install.method == Method::Ssh {
        y += 4;
    } else {
        y += 1;
    }
    if let Some(error) = &install.error {
        paragraph(buf, inner, y, error, text.fg(palette.error));
    }
    // The connection in the setup's boxes; the touch box replaces it while
    // ssh asks: one box at a time.
    if let Some(failed) = &install.failed {
        let style = text.fg(palette.error);
        let mut lines: Vec<(String, Style)> = wrap(failed, 64).into_iter().map(|line| (line, style)).collect();
        lines.push((String::new(), text));
        lines.push(("ENTER or ESC closes.".to_owned(), muted));
        message_box(buf, app, "connection failed", &lines);
    } else if let Some(busy) = install.busy.as_ref().filter(|_| app.asking.is_none()) {
        let spinner = SPINNER[install.frame % SPINNER.len()];
        message_box(buf, app, "connecting", &[(format!("{spinner} {busy}"), text)]);
    }
}

fn check(buf: &mut Buffer, inner: Rect, app: &App, install: &Install) {
    let palette = &app.palette;
    let text = text_style(palette);
    let Some(found) = &install.found else { return };
    let x = inner.x + 2;
    let width = inner.width.saturating_sub(4);
    let mut row = 1;
    for (mark, label, line) in &found.check {
        let style = match mark {
            super::install::Mark::Ok => text.fg(palette.ok),
            super::install::Mark::Add => text.fg(palette.muted),
            super::install::Mark::Note => text.fg(palette.warning),
        };
        write(buf, x, at(inner.y, row), 2, &mark.symbol().to_string(), style);
        let label = format!("{label:<10} ");
        write(buf, x + 2, at(inner.y, row), 11, &label, text);
        for (piece, part) in wrap(line, usize::from(width.saturating_sub(13))).iter().enumerate() {
            write(
                buf,
                x + 13,
                at(inner.y, row + piece),
                width.saturating_sub(13),
                part,
                text,
            );
            if piece > 0 {
                row += 1;
            }
        }
        row += 1;
    }
    write(
        buf,
        x,
        at(inner.y, row + 1),
        width,
        "Nothing was changed.",
        text.fg(palette.muted),
    );
}

fn settings(buf: &mut Buffer, inner: Rect, app: &App, install: &Install) {
    let palette = &app.palette;
    let text = text_style(palette);
    let muted = text.fg(palette.muted);
    let focus = text.fg(palette.focus).add_modifier(Modifier::BOLD);
    let x = inner.x + 1;
    // Section headings are visual rows, never editable settings or hit targets.
    let mut rows: Vec<(Option<usize>, String, String)> = Vec::new();
    let mut application_heading = false;
    for (index, setting) in install.settings.iter().enumerate() {
        if setting.application_input.is_some() && !application_heading {
            rows.push((None, "Application configuration".into(), String::new()));
            application_heading = true;
        }
        rows.push((Some(index), setting.display_title().into(), setting.value()));
    }
    if let Some(domain) = &install.domain {
        rows.push((Some(install.settings.len()), "Server domain".into(), domain.clone()));
    }
    let narrow = inner.width < 110;
    let left = if narrow { inner.width } else { (inner.width / 2).min(64) };
    let label_width = if narrow {
        17
    } else {
        (rows
            .iter()
            .filter(|r| r.0.is_some())
            .map(|r| r.1.chars().count())
            .max()
            .unwrap_or(0)
            + 2)
        .min(usize::from(left.saturating_sub(3)) / 2)
        .max(1)
    };
    let value_width = usize::from(left.saturating_sub(3)).saturating_sub(label_width).max(1);
    let rows: Vec<_> = rows
        .into_iter()
        .flat_map(|(index, title, value)| {
            if index.is_none() {
                return vec![(index, title, value)];
            }
            let labels = wrap(&title, label_width.saturating_sub(2).max(1));
            let values = wrap(if value.is_empty() { "not set" } else { &value }, value_width);
            (0..labels.len().max(values.len()))
                .map(|line| {
                    (
                        index,
                        labels.get(line).cloned().unwrap_or_default(),
                        values.get(line).cloned().unwrap_or_default(),
                    )
                })
                .collect()
        })
        .collect();
    write(
        buf,
        x + 2,
        inner.y,
        u16::try_from(label_width).unwrap_or(0),
        "Setting",
        muted,
    );
    write(
        buf,
        x + 2 + u16::try_from(label_width).unwrap_or(0),
        inner.y,
        u16::try_from(value_width).unwrap_or(0),
        "Value",
        muted,
    );
    let visible = if narrow {
        rows.len().min(usize::from(inner.height.saturating_sub(7) / 2).max(1))
    } else {
        usize::from(inner.height.saturating_sub(5)).max(1)
    };
    let cursor = rows.iter().position(|row| row.0 == Some(install.row)).unwrap_or(0);
    let last = rows
        .iter()
        .rposition(|row| row.0 == Some(install.row))
        .unwrap_or(cursor);
    let start = (last + 1).saturating_sub(visible).min(cursor);
    for (row, (index, title, value)) in rows.iter().enumerate().skip(start).take(visible) {
        let y = at(inner.y, row - start + 1);
        let Some(index) = index else {
            let heading = format!("── {title} ");
            let separator = "─".repeat(usize::from(left.saturating_sub(1)).saturating_sub(heading.chars().count()));
            write(
                buf,
                x,
                y,
                left.saturating_sub(1),
                &format!("{heading}{separator}"),
                muted,
            );
            continue;
        };
        let chosen = *index == install.row;
        let first = row == 0 || rows[row - 1].0 != Some(*index);
        let mark = if chosen && !install.chooser && first {
            "▶ "
        } else {
            "  "
        };
        let line = format!("{mark}{title:<label_width$}{value}");
        write(
            buf,
            x,
            y,
            left - 1,
            &cut(&line, usize::from(left - 1)),
            if chosen && !install.chooser { focus } else { text },
        );
        super::chrome::register(Rect::new(x, y, left - 1, 1), super::chrome::Hit::Setting(*index));
    }
    if !narrow
        && install.mode != Mode::Service
        && rows.len() + 10 < usize::from(inner.height)
        && let Some(found) = &install.found
    {
        let how = found.root.split(" · ").next().unwrap_or_default();
        let label = if install.mode == Mode::Service {
            "Access"
        } else {
            "Root access"
        };
        let line = format!("  {label}  {how}");
        write(
            buf,
            x,
            at(inner.y, rows.len() + 2),
            left - 1,
            &cut(&line, usize::from(left - 1)),
            muted,
        );
    }
    let choices_area = if narrow {
        let offset = u16::try_from(visible + 2).unwrap_or(inner.height).min(inner.height);
        Rect::new(
            inner.x,
            inner.y + offset,
            inner.width,
            inner.height.saturating_sub(offset),
        )
    } else {
        Rect::new(inner.x + left, inner.y, inner.width.saturating_sub(left), inner.height)
    };
    if choices_area.width < 8 || choices_area.height < 4 {
        return;
    }
    let fit = |content: usize, lines: usize| {
        let width = if narrow {
            choices_area.width
        } else {
            u16::try_from(content + 6).unwrap_or(u16::MAX).min(choices_area.width)
        };
        let height = u16::try_from(lines + 4).unwrap_or(u16::MAX).min(choices_area.height);
        Rect::new(choices_area.x, choices_area.y, width, height)
    };
    let on_domain = install.domain.is_some() && install.row == install.settings.len();
    if on_domain {
        let domain = install.domain.clone().unwrap_or_default();
        let field = format!("[ {} ]", install.text_with_cursor(&domain, install.chooser));
        // Room to type a longer one.
        let rect = fit(field.chars().count().max(40), 1);
        let box_inner = frame(
            buf,
            rect,
            palette,
            &Sides {
                top_left: vec![Control::plain("Server domain")],
                focused: install.chooser,
                ..Sides::default()
            },
        );
        write(
            buf,
            box_inner.x + 2,
            at(box_inner.y, 1),
            box_inner.width.saturating_sub(4),
            &field,
            if install.chooser { focus } else { text },
        );
        return;
    }
    let Some(setting) = install.settings.get(install.row) else {
        return;
    };
    if setting.auto_text {
        let error = install.invalid.as_deref().map(|e| wrap(e, 44)).unwrap_or_default();
        let rect = fit(48, 3 + error.len());
        let box_inner = frame(
            buf,
            rect,
            palette,
            &Sides {
                top_left: vec![Control::plain(setting.display_title())],
                focused: install.chooser,
                ..Sides::default()
            },
        );
        let manual = setting.picked.first() == Some(&1);
        let auto = format!("({}) Auto", if manual { " " } else { "*" });
        let width = box_inner.width.saturating_sub(4);
        write(
            buf,
            box_inner.x + 2,
            at(box_inner.y, 1),
            width,
            &auto,
            if install.chooser && install.cursor == 0 {
                focus
            } else {
                text
            },
        );
        let value = setting.text.as_deref().unwrap_or_default();
        let typed = install.text_with_cursor(value, manual && install.chooser && install.cursor == 1);
        let room = usize::from(width.saturating_sub(15)).max(1);
        let start = if install.chooser && manual {
            install.text_caret.saturating_sub(room - 1)
        } else {
            0
        };
        let shown: String = typed.chars().skip(start).take(room).collect();
        let line = format!("({}) Manual [ {shown} ]", if manual { "*" } else { " " });
        write(
            buf,
            box_inner.x + 2,
            at(box_inner.y, 2),
            width,
            &line,
            if install.chooser && install.cursor == 1 {
                focus
            } else if manual {
                text
            } else {
                muted
            },
        );
        if let Some(hint) = &setting.hint {
            write(buf, box_inner.x + 2, at(box_inner.y, 3), width, hint, muted);
        }
        for (row, line) in error.iter().enumerate() {
            write(
                buf,
                box_inner.x + 2,
                at(box_inner.y, 4 + row),
                width,
                line,
                text.fg(palette.error),
            );
        }
        return;
    }
    if let Some(value) = &setting.text {
        // The field over as many rows as its text takes, so its end and the
        // cursor stay in sight.
        let typed: Vec<char> = install.text_with_cursor(value, install.chooser).chars().collect();
        let field: Vec<String> = typed.chunks(44).map(|chunk| chunk.iter().collect()).collect();
        // Why Enter did not keep it, below the field.
        let error: Vec<String> = install
            .invalid
            .as_deref()
            .map(|error| wrap(&format!("✗ {error}"), 44))
            .unwrap_or_default();
        let rect = fit(48, field.len() + error.len());
        let box_inner = frame(
            buf,
            rect,
            palette,
            &Sides {
                top_left: vec![Control::plain(setting.display_title())],
                focused: install.chooser,
                ..Sides::default()
            },
        );
        for (row, line) in field.iter().enumerate() {
            write(
                buf,
                box_inner.x + 2,
                at(box_inner.y, 1 + row),
                box_inner.width.saturating_sub(4),
                line,
                if install.chooser { focus } else { text },
            );
        }
        for (row, line) in error.iter().enumerate() {
            write(
                buf,
                box_inner.x + 2,
                at(box_inner.y, 1 + field.len() + row),
                box_inner.width.saturating_sub(4),
                line,
                text.fg(palette.error),
            );
        }
        return;
    }
    // The choices only; what a setting is for is in the help. The value and
    // the words of its detail in columns as wide as their longest entry (a
    // long VM name moves nothing).
    let rows: Vec<Vec<&str>> = setting
        .choices
        .iter()
        .map(|(value, detail)| {
            std::iter::once(value.as_str())
                .chain(detail.split_whitespace())
                .collect()
        })
        .collect();
    let columns = rows.iter().map(Vec::len).max().unwrap_or(0);
    let widths: Vec<usize> = (0..columns)
        .map(|column| {
            rows.iter()
                .filter_map(|row| row.get(column))
                .map(|word| word.chars().count())
                .max()
                .unwrap_or(0)
        })
        .collect();
    // Long entries must not push every other entry's details outside the panel.
    let compact = widths.iter().sum::<usize>() + columns.saturating_sub(1) * 2
        > usize::from(choices_area.width.saturating_sub(8));
    let mut lines: Vec<(String, bool)> = rows
        .iter()
        .enumerate()
        .map(|(index, row)| {
            let ticked = setting.picked.contains(&index);
            let cursor = install.chooser && install.adding.is_none() && index == install.cursor;
            let mark = if cursor { "▶" } else { " " };
            let tick = match (setting.list, setting.multiple, ticked) {
                (true, _, _) => "",
                (false, true, true) => "[x]",
                (false, true, false) => "[ ]",
                (false, false, true) => "(•)",
                (false, false, false) => "( )",
            };
            let cells: Vec<String> = row
                .iter()
                .zip(&widths)
                .map(|(word, width)| {
                    if compact {
                        (*word).to_owned()
                    } else {
                        format!("{word:<width$}")
                    }
                })
                .collect();
            let line = format!(
                "{mark} {tick} {}",
                cells.join(if compact { " " } else { "  " }).trim_end()
            );
            (
                if setting.list {
                    line.replacen("  ", " ", 1)
                } else {
                    line
                },
                cursor,
            )
        })
        .collect();
    if let Some(typed) = &install.adding {
        lines.push((format!("+ {}", install.text_with_cursor(typed, true)), true));
    }
    if let Some(error) = &install.invalid {
        lines.push((format!("✗ {error}"), false));
    }
    let content = lines
        .iter()
        .map(|(line, _)| line.chars().count())
        .chain([setting.display_title().chars().count(), 20])
        .max()
        .unwrap_or(0);
    let box_inner = frame(
        buf,
        fit(content, lines.len()),
        palette,
        &Sides {
            top_left: vec![Control::plain(setting.display_title())],
            focused: install.chooser,
            ..Sides::default()
        },
    );
    let width = box_inner.width.saturating_sub(2);
    let visible = usize::from(box_inner.height.saturating_sub(1));
    let selected = if install.adding.is_some() {
        lines.len() - 1
    } else if install.chooser {
        install.cursor
    } else {
        setting.picked.first().copied().unwrap_or(0)
    };
    let start = selected.saturating_add(1).saturating_sub(visible);
    for (index, (line, cursor)) in lines.iter().enumerate().skip(start).take(visible) {
        write(
            buf,
            box_inner.x + 1,
            at(box_inner.y, 1 + index - start),
            width,
            &cut(line, usize::from(width)),
            if line.starts_with("✗ ") {
                text.fg(palette.error)
            } else if *cursor {
                focus
            } else {
                text
            },
        );
        super::chrome::register(
            Rect::new(box_inner.x, at(box_inner.y, 1 + index - start), box_inner.width, 1),
            super::chrome::Hit::Choice(index),
        );
    }
}

fn review(buf: &mut Buffer, inner: Rect, app: &App, install: &Install) {
    let palette = &app.palette;
    let text = text_style(palette);
    let bold = text.add_modifier(Modifier::BOLD);
    let x = inner.x + 2;
    let width = inner.width.saturating_sub(4);
    if let Some(busy) = &install.busy {
        write(buf, x, inner.y + 1, width, busy, text.fg(palette.focus));
        return;
    }
    let mut lines: Vec<(String, Style)> = Vec::new();
    if !install.problems.is_empty() {
        lines.push((
            match install.mode {
                Mode::Service => "Problems (they prevent applying this operation)",
                Mode::Sites => "Problems (they keep the deployment from starting)",
                Mode::Uninstall => "Problems (they keep the removal from starting)",
                Mode::Install => "Problems (they keep the install from starting)",
            }
            .to_owned(),
            bold.fg(palette.error),
        ));
        for problem in &install.problems {
            for piece in wrap(&format!("  ! {problem}"), usize::from(width)) {
                lines.push((piece, text.fg(palette.error)));
            }
        }
        lines.push((String::new(), text));
    }
    for (heading, body) in &install.sections {
        lines.push((heading.clone(), bold));
        for line in body {
            for piece in wrap(&format!("    {line}"), usize::from(width)) {
                lines.push((piece, text));
            }
        }
        lines.push((String::new(), text));
    }
    let visible = usize::from(inner.height.saturating_sub(1));
    let offset = install.scroll.min(lines.len().saturating_sub(visible));
    for (row, (line, style)) in lines.iter().skip(offset).take(visible).enumerate() {
        write(buf, x, at(inner.y, row + 1), width, line, *style);
    }
    scrollbar(
        buf,
        Rect::new(inner.x - 1, inner.y - 1, inner.width + 2, inner.height + 2),
        palette,
        Scrolls::InstallReview,
        offset,
        visible,
        lines.len(),
    );
}

fn run(buf: &mut Buffer, inner: Rect, app: &App, install: &Install) {
    let palette = &app.palette;
    let text = text_style(palette);
    let x = inner.x + 2;
    let width = inner.width.saturating_sub(4);
    let visible = usize::from(inner.height.saturating_sub(2));
    let skip = install.lines.len().saturating_sub(visible);
    let last = install.lines.len().saturating_sub(1);
    for (row, (index, line)) in install.lines.iter().enumerate().skip(skip).enumerate() {
        let (mark, style) = if index == last {
            ("◐", text.fg(palette.focus))
        } else {
            ("✓", text.fg(palette.ok))
        };
        write(buf, x, at(inner.y, row + 1), 2, mark, style);
        write(
            buf,
            x + 2,
            at(inner.y, row + 1),
            width.saturating_sub(2),
            line.trim_start_matches("- "),
            text,
        );
    }
    if install.lines.is_empty() {
        write(buf, x, inner.y + 1, width, "Starting", text.fg(palette.focus));
    }
}

fn done(buf: &mut Buffer, inner: Rect, app: &App, install: &Install) {
    let palette = &app.palette;
    let text = text_style(palette);
    let x = inner.x + 2;
    let width = inner.width.saturating_sub(4);
    let (heading, style) = match (install.ok, install.mode) {
        (true, Mode::Install) => ("HomeStack is installed.", text.fg(palette.ok)),
        (true, Mode::Uninstall) => ("HomeStack is removed from Proxmox.", text.fg(palette.ok)),
        (false, Mode::Install) => ("The install did not finish.", text.fg(palette.error)),
        (false, Mode::Uninstall) => ("The uninstall did not finish.", text.fg(palette.error)),
        (true, Mode::Sites) => ("Every Site connector runs and is online.", text.fg(palette.ok)),
        (false, Mode::Sites) => ("The Site connectors are not all online.", text.fg(palette.error)),
        (true, Mode::Service) => ("The reviewed service operation completed.", text.fg(palette.ok)),
        (false, Mode::Service) => (
            "The service operation did not finish; inspect the journal.",
            text.fg(palette.error),
        ),
    };
    write(buf, x, inner.y + 1, width, heading, style.add_modifier(Modifier::BOLD));
    let mut row = 3;
    for line in &install.result {
        let mark = if install.ok { "✓ " } else { "  " };
        for piece in wrap(&format!("{mark}{line}"), usize::from(width)) {
            write(buf, x, at(inner.y, row), width, &piece, text);
            row += 1;
        }
    }
    if install.ok && install.mode == Mode::Install {
        write(
            buf,
            x,
            at(inner.y, row + 1),
            width,
            "Root on the node is not needed any more; the ssh session is closed.",
            text.fg(palette.muted),
        );
    }
}

/// ssh's question in a box over the screen (mockup 03).
pub(super) fn asking(buf: &mut Buffer, area: Rect, app: &App, asking: &AskView) {
    let palette = &app.palette;
    let text = text_style(palette);
    let rect = connection_rect(area);
    let inner_width = usize::from(rect.width.saturating_sub(6));
    let (title, mut lines, bottom): (&str, Vec<(String, Style)>, Vec<Control>) = match asking.kind {
        Kind::Touch => {
            let key = asking
                .prompt
                .split_once(" for key ")
                .map_or(asking.prompt.as_str(), |(_, key)| key)
                .to_owned();
            let left = TOUCH_SECONDS.saturating_sub(asking.seconds);
            #[allow(clippy::cast_precision_loss)]
            let percent = 100.0 * left as f64 / TOUCH_SECONDS as f64;
            let timer = if left > 0 {
                format!("{}  {left} s left", bar(26, percent))
            } else {
                format!("still waiting ({} s)", asking.seconds)
            };
            (
                "security key",
                vec![
                    (
                        "Touch your security key now.".to_owned(),
                        text.add_modifier(Modifier::BOLD),
                    ),
                    (String::new(), text),
                    if asking.key_names.is_empty() {
                        (format!("key   {key}"), text)
                    } else {
                        (
                            format!("key   {}", asking.key_names.join(" / ")),
                            text.add_modifier(Modifier::BOLD),
                        )
                    },
                    if asking.key_names.is_empty() {
                        (String::new(), text)
                    } else {
                        (
                            format!("      {}", cut(&key, inner_width.saturating_sub(6))),
                            text.fg(palette.muted),
                        )
                    },
                    (String::new(), text),
                    (timer, text.fg(palette.focus)),
                ],
                vec![Control::key("cancel Esc")],
            )
        }
        Kind::Secret => {
            let mut lines: Vec<(String, Style)> = asking
                .prompt
                .lines()
                .flat_map(|line| wrap(line.trim(), inner_width))
                .map(|line| (line, text))
                .collect();
            lines.push((String::new(), text));
            let dots = "•".repeat(asking.input.chars().count());
            lines.push((
                format!(
                    "[ {dots}▏{} ]",
                    " ".repeat(16usize.saturating_sub(dots.chars().count()))
                ),
                text.fg(palette.focus),
            ));
            ("ssh", lines, vec![Control::key("OK Enter"), Control::key("cancel Esc")])
        }
        Kind::HostKey => {
            let lines = asking
                .prompt
                .lines()
                .flat_map(|line| wrap(line.trim(), inner_width))
                .map(|line| (line, text))
                .collect();
            (
                "host key",
                lines,
                vec![Control::key("&Trust Enter"), Control::key("cancel Esc")],
            )
        }
    };
    lines.insert(0, (String::new(), text));
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
    for (row, (line, style)) in lines.iter().enumerate().take(usize::from(inner.height)) {
        write(
            buf,
            inner.x + 2,
            at(inner.y, row),
            inner.width.saturating_sub(4),
            line,
            *style,
        );
    }
}

/// What the server domain is, for the settings' help.
const DOMAIN_NOTE: &str = "The servers are named apiN.<domain>, one per node; each name must resolve on this desktop to \
the server's address. The certificate *.<domain> comes from Let's Encrypt.";

/// The settings' help (?): what each setting is for, and the keys.
fn settings_help(buf: &mut Buffer, area: Rect, app: &App, install: &Install) {
    let palette = &app.palette;
    let text = text_style(palette);
    let bold = text.add_modifier(Modifier::BOLD);
    let width = area.width.saturating_sub(8).min(100);
    let inner_width = usize::from(width.saturating_sub(4));
    let mut lines: Vec<(String, Style)> = Vec::new();
    let mut add = |title: &str, note: &str| {
        lines.push((title.to_owned(), bold));
        for piece in wrap(note, inner_width) {
            lines.push((piece, text));
        }
        lines.push((String::new(), text));
    };
    for setting in &install.settings {
        add(&setting.title, &setting.note);
    }
    if install.domain.is_some() {
        add("Server domain", DOMAIN_NOTE);
    }
    lines.push((
        "↑ ↓ a setting · Tab into its box and out · Space tick · Enter review".to_owned(),
        text.fg(palette.muted),
    ));
    let height = u16::try_from(lines.len() + 3).unwrap_or(u16::MAX).min(area.height);
    let rect = centered(area, width, height);
    let inner = frame(
        buf,
        rect,
        palette,
        &Sides {
            top_left: vec![Control::plain("help: the settings")],
            bottom_left: vec![Control::key("close Esc")],
            focused: true,
            ..Sides::default()
        },
    );
    for (row, (line, style)) in lines.iter().enumerate().take(usize::from(inner.height)) {
        write(
            buf,
            inner.x + 2,
            at(inner.y, row + 1),
            inner.width.saturating_sub(4),
            line,
            *style,
        );
    }
}
