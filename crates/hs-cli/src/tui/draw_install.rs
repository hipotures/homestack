//! Drawing the install screen (mockups 02–08, 18) and ssh's question box.

use ratatui::buffer::Buffer;
use ratatui::layout::Rect;
use ratatui::style::{Modifier, Style};

use super::chrome::{Control, Sides, frame, scrollbar, text_style};
use super::draw::{bar, centered, cut, log, wrap, write};
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
        bottom_right: vec![Control::plain(&counter)],
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
        install.mode.title(),
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
        Stage::Settings => settings(buf, inner, app, install),
        Stage::Review => review(buf, inner, app, install),
        Stage::Run => run(buf, inner, app, install),
        Stage::Done => done(buf, inner, app, install),
    }
    if let Some(below) = below {
        log(buf, below, app, Vec::new());
    }
}

/// The buttons of the stage in the bottom border.
fn buttons(install: &Install) -> Vec<Control> {
    match install.stage {
        Stage::Access => vec![
            Control::key("field ↑↓"),
            Control::key("Connect Enter"),
            Control::key(if install.first_start { "Quit Esc" } else { "back Esc" }),
        ],
        Stage::Check => vec![
            Control::key(match install.mode {
                Mode::Install => "&Settings Enter",
                Mode::Uninstall => "&Review Enter",
            }),
            Control::key("back Esc"),
        ],
        Stage::Settings => vec![
            Control::key("box Tab"),
            Control::key("&Review Enter"),
            Control::key("back Esc"),
        ],
        Stage::Review => {
            let ready = install.reviewed && install.problems.is_empty();
            match install.mode {
                Mode::Install => vec![
                    Control::key("scroll ↑↓"),
                    Control::key("&Install Enter").enabled(ready),
                    Control::key("back Esc"),
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
        Stage::Done if install.ok => vec![Control::key("&Open HomeStack Enter"), Control::key("&Quit")],
        Stage::Done => vec![Control::key("back to the review Enter"), Control::key("&Quit")],
    }
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
        "↑ then ← →: SSH or a Herdr pane",
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
            write(buf, x, y, 10, "Pane", text);
            let line = install.tabs.get(install.tab).map_or_else(
                || "‹ no Herdr tab yet ›".to_owned(),
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
                y + 2,
                width.saturating_sub(10),
                "A Herdr tab whose shell is root on a node, at its prompt.",
                muted,
            );
        }
    }
    y += 4;
    if let Some(busy) = &install.busy {
        write(buf, x, y, width, busy, text.fg(palette.focus));
    } else if let Some(error) = &install.error {
        paragraph(buf, inner, y, error, text.fg(palette.error));
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
    let left = if inner.width >= 110 { 40 } else { 28 };
    let x = inner.x + 1;
    let mut rows: Vec<(String, String)> = install
        .settings
        .iter()
        .map(|setting| (setting.title.clone(), setting.value()))
        .collect();
    if let Some(domain) = &install.domain {
        rows.push(("Server domain".to_owned(), domain.clone()));
    }
    let label_width = if left >= 40 { 19 } else { 17 };
    for (row, (title, value)) in rows.iter().enumerate() {
        let chosen = row == install.row;
        let mark = if chosen { "▶ " } else { "  " };
        let line = format!("{mark}{:<label_width$}{}", cut(title, label_width - 1), value);
        write(
            buf,
            x,
            at(inner.y, row + 1),
            left - 1,
            &cut(&line, usize::from(left - 1)),
            if chosen && !install.chooser { focus } else { text },
        );
    }
    if let Some(found) = &install.found {
        let how = found.root.split(" · ").next().unwrap_or_default();
        let line = format!("  Root access  {how}");
        write(
            buf,
            x,
            at(inner.y, rows.len() + 2),
            left - 1,
            &cut(&line, usize::from(left - 1)),
            muted,
        );
    }
    // How to change them, always shown under the list, in two columns.
    let help = [
        ("↑ ↓", "a setting"),
        ("Tab", "into its box and out"),
        ("Space", "tick in the box"),
        ("letters", "the server domain"),
        ("Enter", "review"),
    ];
    for (row, (key, what)) in help.iter().enumerate() {
        let line = format!("{key:<9}{what}");
        write(
            buf,
            x + 2,
            at(inner.y, rows.len() + 4 + row),
            left - 3,
            &cut(&line, usize::from(left - 3)),
            muted,
        );
    }
    // The chooser of the focused row, in a box on the right.
    let rect = Rect::new(inner.x + left, inner.y, inner.width.saturating_sub(left), inner.height);
    let on_domain = install.domain.is_some() && install.row == install.settings.len();
    if on_domain {
        let box_inner = frame(
            buf,
            rect,
            palette,
            &Sides {
                top_left: vec![Control::plain("Server domain")],
                focused: true,
                ..Sides::default()
            },
        );
        let lines = paragraph(
            buf,
            box_inner,
            box_inner.y,
            "The servers are named apiN.<domain>, one per node; each name must resolve on this desktop to the \
             server's address. The certificate *.<domain> comes from Let's Encrypt.",
            text,
        );
        let domain = install.domain.clone().unwrap_or_default();
        write(
            buf,
            box_inner.x + 2,
            at(box_inner.y, lines + 1),
            box_inner.width.saturating_sub(4),
            &format!("[ {domain}▏ ]"),
            focus,
        );
        return;
    }
    let Some(setting) = install.settings.get(install.row) else {
        return;
    };
    let box_inner = frame(
        buf,
        rect,
        palette,
        &Sides {
            top_left: vec![Control::plain(&setting.title)],
            bottom_left: vec![Control::key(if setting.multiple {
                "tick Space"
            } else {
                "choose Space"
            })],
            focused: install.chooser,
            ..Sides::default()
        },
    );
    let lines = paragraph(buf, box_inner, box_inner.y, &setting.note, text);
    let width = box_inner.width.saturating_sub(4);
    let name_width = setting
        .choices
        .iter()
        .map(|(value, _)| value.chars().count())
        .max()
        .unwrap_or(0);
    for (index, (value, detail)) in setting.choices.iter().enumerate() {
        let ticked = setting.picked.contains(&index);
        let cursor = install.chooser && index == install.cursor;
        let mark = if cursor { "▶" } else { " " };
        let tick = match (setting.multiple, ticked) {
            (true, true) => "[x]",
            (true, false) => "[ ]",
            (false, true) => "(•)",
            (false, false) => "( )",
        };
        let line = format!("{mark} {tick} {value:<name_width$}  {detail}");
        let style = if cursor { focus } else { text };
        write(
            buf,
            box_inner.x + 1,
            at(box_inner.y, lines + 1 + index),
            width,
            &cut(&line, usize::from(width)),
            style,
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
            "Problems (they keep the install from starting)".to_owned(),
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
    if install.mode == Mode::Install && install.problems.is_empty() {
        lines.push((
            "Then the main screen opens with the status of the cluster.".to_owned(),
            text,
        ));
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
        write(buf, x, inner.y + 1, width, "Starting…", text.fg(palette.focus));
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
    let width: u16 = 60;
    let inner_width = usize::from(width - 6);
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
                    (format!("key   {key}"), text),
                    (String::new(), text),
                    (timer, text.fg(palette.focus)),
                ],
                Vec::new(),
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
    let height = u16::try_from(lines.len() + 3).unwrap_or(u16::MAX);
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
