//! Pangolin setup uses the same frame, tree selection and detail panes as setup.
use crossterm::event::KeyCode;
use ratatui::{
    buffer::Buffer,
    layout::Rect,
    style::{Modifier, Style},
};

use super::chrome::{self, Control, Hit, Sides, frame, text_style};
use super::draw::{bar, wrap, write};
use super::pangolin::{BOOTSTRAP, DESCRIPTIONS, HELP, INSTALLATION, STAGES, Wizard};
use super::state::App;

pub(super) fn screen(buf: &mut Buffer, area: Rect, app: &App, wizard: &Wizard) {
    let palette = &app.palette;
    let text = text_style(palette);
    let muted = text.fg(palette.muted);
    let available = wizard.selected_step == 0
        || (wizard.selected_step == 1 && wizard.prerequisites_available())
        || (wizard.selected_step == 2 && wizard.discovery_available())
        || (wizard.selected_step == 3 && wizard.api_available())
        || (wizard.selected_step == 4 && wizard.tunnel_available())
        || (wizard.selected_step == 5 && wizard.bootstrap_available());
    let prerequisites = wizard.selected_step == 1;
    let inner = frame(
        buf,
        area,
        palette,
        &Sides {
            top_left: vec![Control::plain("settings / integrations / pangolin")],
            bottom_left: if prerequisites {
                vec![
                    Control::named("↑↓", "steps", KeyCode::Down),
                    Control::named("Tab", "focus", KeyCode::Tab).enabled(available),
                    Control::named(
                        "Enter",
                        if wizard.focus == 3 { "next" } else { "open/close" },
                        KeyCode::Enter,
                    )
                    .enabled(available),
                    if wizard.focus == 3 {
                        Control::named("Space", "toggle", KeyCode::Char(' ')).enabled(available)
                    } else {
                        Control::named("Pg↓↑", "scroll", KeyCode::PageDown).enabled(available)
                    },
                    Control::named("F4", "help", KeyCode::F(4)),
                    Control::named("Esc", "back", KeyCode::Esc),
                ]
            } else if wizard.selected_step == 5 && wizard.bootstrap.running != 0 {
                vec![Control::named("Esc", "cancel", KeyCode::Esc)]
            } else if wizard.selected_step == 5 && available {
                let mut controls = vec![
                    Control::named("Tab", "focus", KeyCode::Tab),
                    Control::named("F4", "help", KeyCode::F(4)),
                    Control::named("F5", "clear", KeyCode::F(5)),
                    Control::named("F6", "verify", KeyCode::F(6)).enabled(wizard.tunnel_connected()),
                    Control::named("F7", "details", KeyCode::F(7)),
                    Control::named("Esc", "back", KeyCode::Esc),
                ];
                if area.width >= 100 {
                    controls.insert(0, Control::named("↑↓", "steps", KeyCode::Down));
                }
                controls
            } else if wizard.selected_step == 4 && wizard.tunnel.busy() {
                vec![Control::named("Esc", "cancel", KeyCode::Esc)]
            } else if wizard.selected_step == 4 {
                let connected = wizard.tunnel.current.state == super::pangolin_tunnel::State::Connected;
                let compact = area.width < 100;
                let mut controls = vec![
                    Control::named("Tab", "focus", KeyCode::Tab).enabled(available),
                    Control::named("F4", "help", KeyCode::F(4)),
                    Control::named("F5", if compact { "stop" } else { "disconnect" }, KeyCode::F(5)).enabled(connected),
                    Control::named("F6", if connected { "verify" } else { "connect" }, KeyCode::F(6))
                        .enabled(available && !wizard.testing),
                    Control::named("F7", "details", KeyCode::F(7)),
                    Control::named("Esc", "back", KeyCode::Esc),
                ];
                if !compact {
                    controls.insert(0, Control::named("↑↓", "steps", KeyCode::Down));
                }
                controls
            } else if wizard.selected_step == 3 && wizard.testing {
                vec![Control::named("Esc", "cancel", KeyCode::Esc)]
            } else if wizard.selected_step == 3 {
                vec![
                    Control::named("↑↓", "steps", KeyCode::Down).enabled(!wizard.testing),
                    Control::named("Tab", "focus", KeyCode::Tab).enabled(available && !wizard.testing),
                    Control::named("F4", "help", KeyCode::F(4)),
                    Control::named("F5", "apply", KeyCode::F(5)).enabled(wizard.can_apply_api()),
                    Control::named("F6", "inspect", KeyCode::F(6)).enabled(available && !wizard.testing),
                    Control::named("F7", "details", KeyCode::F(7)).enabled(wizard.api.is_some()),
                    Control::named("Esc", if wizard.testing { "cancel" } else { "back" }, KeyCode::Esc),
                ]
            } else if wizard.selected_step == 2 {
                vec![
                    Control::named("↑↓", "steps", KeyCode::Down),
                    Control::named("Tab", "focus", KeyCode::Tab).enabled(available),
                    Control::named("F4", "help", KeyCode::F(4)),
                    Control::named("F6", "discover", KeyCode::F(6)).enabled(available && !wizard.testing),
                    Control::named("F7", "details", KeyCode::F(7)).enabled(wizard.discovery.is_some()),
                    Control::named("Esc", if wizard.testing { "cancel" } else { "back" }, KeyCode::Esc),
                ]
            } else {
                vec![
                    Control::named("↑↓", "steps", KeyCode::Down),
                    Control::named("Tab", "focus", KeyCode::Tab).enabled(available),
                    Control::named("F4", "help", KeyCode::F(4)),
                    Control::named("F6", "test", KeyCode::F(6)).enabled(wizard.selected_step == 0 && !wizard.testing),
                    Control::named("F7", "report", KeyCode::F(7))
                        .enabled(wizard.selected_step == 0 && wizard.report.is_some()),
                    Control::named("Esc", if wizard.testing { "cancel" } else { "back" }, KeyCode::Esc),
                ]
            },
            focused: true,
            ..Sides::default()
        },
    );
    if inner.width < 8 || inner.height < 18 {
        return;
    }
    let x = inner.x + 1;
    let width = inner.width - 2;
    let state = format!("{}/8 completed", wizard.completed());
    write(
        buf,
        x,
        inner.y,
        width,
        "Pangolin Integration Setup",
        text.add_modifier(Modifier::BOLD),
    );
    write(
        buf,
        x + width.saturating_sub(state.len() as u16),
        inner.y,
        width.min(state.len() as u16),
        &state,
        text.fg(palette.muted),
    );
    write(
        buf,
        x,
        inner.y + 1,
        width,
        &bar(usize::from(width), wizard.completed() as f64 * 12.5),
        text.fg(palette.accent),
    );
    let body = Rect::new(x, inner.y + 3, width, inner.height - 5);
    if width >= 108 {
        let nav_width = 40;
        for y in body.top()..body.bottom() {
            write(buf, body.x + nav_width, y, 1, "│", muted);
        }
        steps(
            buf,
            Rect::new(body.x, body.y, nav_width - 1, body.height),
            app,
            wizard,
            true,
        );
        details(
            buf,
            Rect::new(body.x + nav_width + 3, body.y, body.width - nav_width - 3, body.height),
            app,
            wizard,
            true,
        );
    } else if wizard.selected_step == 5 && wizard.focus > 0 && wizard.bootstrap_available() {
        write(
            buf,
            body.x,
            body.y,
            body.width,
            "6. Bootstrap API key",
            text.add_modifier(Modifier::BOLD),
        );
        bootstrap_details(
            buf,
            Rect::new(body.x, body.y + 2, body.width, body.height.saturating_sub(2)),
            app,
            wizard,
        );
    } else if wizard.selected_step == 4 && wizard.focus > 0 && wizard.tunnel_available() {
        write(
            buf,
            body.x,
            body.y,
            body.width,
            "5. API tunnel",
            text.add_modifier(Modifier::BOLD),
        );
        tunnel_details(
            buf,
            Rect::new(body.x, body.y + 2, body.width, body.height.saturating_sub(2)),
            app,
            wizard,
        );
    } else if wizard.selected_step == 3 && wizard.focus > 0 && wizard.api_available() {
        write(
            buf,
            body.x,
            body.y,
            body.width,
            "4. Integration API",
            text.add_modifier(Modifier::BOLD),
        );
        api_details(
            buf,
            Rect::new(body.x, body.y + 2, body.width, body.height.saturating_sub(2)),
            app,
            wizard,
        );
    } else if wizard.selected_step == 2 && wizard.focus > 0 && wizard.discovery_available() {
        write(
            buf,
            body.x,
            body.y,
            body.width,
            "3. Pangolin discovery",
            text.add_modifier(Modifier::BOLD),
        );
        discovery_details(
            buf,
            Rect::new(body.x, body.y + 2, body.width, body.height.saturating_sub(2)),
            app,
            wizard,
        );
    } else if prerequisites && wizard.focus > 0 && wizard.prerequisites_available() {
        // Compact details page: Tab returns to the complete step navigation.
        prerequisites_details(buf, body, app, wizard);
    } else {
        // At 80x24 all eight navigation rows and the essential form stay on
        // screen. Explanations open in the same scrollable help panel as wide mode.
        steps(buf, Rect::new(body.x, body.y, body.width, 8), app, wizard, false);
        write(
            buf,
            body.x,
            body.y + 8,
            body.width,
            &"─".repeat(usize::from(body.width)),
            muted,
        );
        details(
            buf,
            Rect::new(body.x, body.y + 9, body.width, body.height.saturating_sub(9)),
            app,
            wizard,
            false,
        );
    }
    if wizard.selected_step == 0 && !wizard.testing && !wizard.message.is_empty() {
        write(buf, x, inner.bottom() - 1, width, &wizard.message, muted);
    }
    if wizard.api_confirm {
        approval(buf, area, app, wizard);
    }
    if wizard.help_open || wizard.report_open {
        modal(buf, area, app, wizard);
    }
}

fn steps(buf: &mut Buffer, area: Rect, app: &App, wizard: &Wizard, wide: bool) {
    let text = text_style(&app.palette);
    let top = area.y + u16::from(wide);
    if wide {
        write(
            buf,
            area.x + 1,
            area.y,
            area.width - 1,
            "STEPS",
            text.fg(app.palette.muted),
        );
    }
    for (at, title) in STAGES.iter().enumerate() {
        let y = top + at as u16;
        if y >= area.bottom() {
            break;
        }
        let chosen = at == wizard.selected_step;
        let style = if chosen {
            let style = text.bg(app.palette.selection).add_modifier(Modifier::BOLD);
            if app.palette.no_color {
                style.add_modifier(Modifier::REVERSED)
            } else {
                style
            }
        } else if at > 5
            || (at == 5 && !wizard.bootstrap_available())
            || (at == 4 && !wizard.tunnel_available())
            || (at == 3 && !wizard.api_available())
            || (at == 2 && !wizard.discovery_available())
            || (at == 1 && !wizard.prerequisites_available())
        {
            text.fg(app.palette.muted)
        } else {
            text
        };
        write(buf, area.x, y, area.width, &" ".repeat(usize::from(area.width)), style);
        let passed = (at == 0 && wizard.status() == "Verified")
            || (at == 1 && wizard.prerequisites_confirmed())
            || (at == 2 && wizard.discovery_status() == "Discovered")
            || (at == 3 && wizard.api_status() == "Verified")
            || (at == 4 && wizard.tunnel_connected())
            || (at == 5 && wizard.bootstrap_verified());
        let failed = (at == 5 && wizard.bootstrap_available() && wizard.bootstrap_status() == "Failed")
            || (at == 4
                && wizard.tunnel_available()
                && matches!(
                    wizard.tunnel.current.state,
                    super::pangolin_tunnel::State::Failed | super::pangolin_tunnel::State::Lost
                ))
            || (at == 0 && wizard.status() == "Failed")
            || (at == 3 && matches!(wizard.api_status(), "Failed" | "Permission denied"))
            || (at == 2
                && matches!(
                    wizard.discovery_status(),
                    "Failed" | "Permission denied" | "Not found" | "Unsupported installation" | "Partially discovered"
                ));
        let mark = if passed {
            "✓"
        } else if failed {
            "✗"
        } else if chosen && wizard.focus == 0 {
            "▸"
        } else {
            " "
        };
        write(
            buf,
            area.x,
            y,
            area.width,
            &format!("{mark} {}. {title}", at + 1),
            style,
        );
        if (passed || failed) && !app.palette.no_color {
            write(
                buf,
                area.x,
                y,
                1,
                mark,
                style.fg(if passed { app.palette.ok } else { app.palette.error }),
            );
        }
        if at == 4 && passed {
            write(
                buf,
                area.right() - 1,
                y,
                1,
                "●",
                if app.palette.no_color {
                    style
                } else {
                    style.fg(app.palette.ok)
                },
            );
        }
        if at > 5
            || (at == 5 && !wizard.bootstrap_available())
            || (at == 4 && !wizard.tunnel_available())
            || (at == 3 && !wizard.api_available())
            || (at == 2 && !wizard.discovery_available())
            || (at == 1 && !wizard.prerequisites_available())
        {
            write(buf, area.right() - 8, y, 8, "[Locked]", style.fg(app.palette.muted));
        }
        chrome::register(Rect::new(area.x, y, area.width, 1), Hit::PangolinStep(at));
    }
}

fn details(buf: &mut Buffer, area: Rect, app: &App, wizard: &Wizard, wide: bool) {
    let text = text_style(&app.palette);
    let muted = text.fg(app.palette.muted);
    let mut y = area.y;
    if wide {
        write(
            buf,
            area.x,
            y,
            area.width,
            &format!("{}. {}", wizard.selected_step + 1, STAGES[wizard.selected_step]),
            text.add_modifier(Modifier::BOLD),
        );
        y += 2;
    }
    if wizard.selected_step == 5 && wizard.bootstrap_available() {
        bootstrap_details(
            buf,
            Rect::new(area.x, y, area.width, area.bottom().saturating_sub(y)),
            app,
            wizard,
        );
        return;
    }
    if wizard.selected_step == 4
        && (wizard.tunnel_available() || wizard.tunnel.current.state == super::pangolin_tunnel::State::Outdated)
    {
        tunnel_details(
            buf,
            Rect::new(area.x, y, area.width, area.bottom().saturating_sub(y)),
            app,
            wizard,
        );
        return;
    }
    if wizard.selected_step == 3 && wizard.api_available() {
        api_details(
            buf,
            Rect::new(area.x, y, area.width, area.bottom().saturating_sub(y)),
            app,
            wizard,
        );
        return;
    }
    if wizard.selected_step == 2 && wizard.discovery_available() {
        discovery_details(
            buf,
            Rect::new(area.x, y, area.width, area.bottom().saturating_sub(y)),
            app,
            wizard,
        );
        return;
    }
    if wizard.selected_step == 1 && wizard.prerequisites_available() {
        prerequisites_details(
            buf,
            Rect::new(area.x, y, area.width, area.bottom().saturating_sub(y)),
            app,
            wizard,
        );
        return;
    }
    if wizard.selected_step > 0 {
        if !wide {
            write(
                buf,
                area.x,
                y,
                area.width,
                STAGES[wizard.selected_step],
                text.add_modifier(Modifier::BOLD),
            );
            y += 1;
        }
        let lines = if wizard.selected_step == 5 {
            "Locked: verify the private Integration API in Step 4 and connect the API tunnel in Step 5 first. F4 explains the bootstrap key."
                .to_owned()
        } else if wizard.selected_step == 4 {
            "Locked: verify the private Integration API in Step 4 for the current server first. F4 explains the tunnel."
                .to_owned()
        } else if wizard.selected_step == 3 {
            "Locked: complete discovery for the selected server first. F4 explains Integration API configuration."
                .to_owned()
        } else if wizard.selected_step == 2 {
            "Locked: verify Step 1 and confirm Step 2 for the current server. F4 explains discovery.".to_owned()
        } else if wizard.selected_step == 1 {
            "Locked: verify and save the current SSH target in Step 1 first.\n\nF4 explains the manual prerequisites."
                .to_owned()
        } else {
            format!(
                "Locked - not implemented.\n\n{}\n\nF4 opens detailed help. Enter also opens help; it never runs this step.",
                DESCRIPTIONS[wizard.selected_step]
            )
        };
        lines_into(
            buf,
            Rect::new(area.x, y, area.width, area.bottom().saturating_sub(y)),
            &lines,
            0,
            muted,
        );
        return;
    }
    write(
        buf,
        area.x,
        y,
        area.width,
        "SSH connection alias / server hostname",
        if wizard.focus == 1 {
            text.fg(app.palette.focus)
        } else {
            text
        },
    );
    let input = Rect::new(area.x, y + 1, area.width, 3);
    let field = frame(
        buf,
        input,
        &app.palette,
        &Sides {
            focused: wizard.focus == 1,
            ..Sides::default()
        },
    );
    let capacity = usize::from(field.width.saturating_sub(2)).max(1);
    let start = if wizard.focus == 1 {
        wizard.cursor.saturating_sub(capacity - 1)
    } else {
        0
    };
    let value: String = wizard
        .target
        .chars()
        .chain(std::iter::once(' '))
        .skip(start)
        .take(capacity)
        .collect();
    write(buf, field.x + 1, field.y, field.width.saturating_sub(2), &value, text);
    if wizard.focus == 1 {
        let c = wizard.target.chars().nth(wizard.cursor).unwrap_or(' ');
        write(
            buf,
            field.x + 1 + (wizard.cursor - start) as u16,
            field.y,
            1,
            &c.to_string(),
            text.add_modifier(Modifier::REVERSED),
        );
    }
    chrome::register(Rect::new(area.x, y, area.width, 4), Hit::IntegrationField(1));
    y += 4 + u16::from(wide);
    let connection = if wizard.testing {
        "Testing SSH… (Esc cancels)"
    } else if wizard.status() == "Not configured" {
        "Not tested"
    } else {
        wizard.status()
    };
    write(
        buf,
        area.x,
        y,
        area.width,
        &format!("Connection: {connection}"),
        text.fg(if wizard.status() == "Verified" {
            app.palette.ok
        } else {
            app.palette.muted
        }),
    );
    y += 1;
    let last = wizard
        .report
        .as_ref()
        .map(|r| {
            format!(
                "Last test: {} · {} · {}",
                if r.passed { "PASS" } else { "FAIL" },
                r.local_date(app.utc_offset),
                r.duration()
            )
        })
        .unwrap_or_else(|| "Last test: Never".to_owned());
    write(buf, area.x, y, area.width, &last, muted);
    y += 1 + u16::from(wide);
    let style = if wizard.focus == 2 {
        let style = text
            .bg(app.palette.selection)
            .fg(app.palette.focus)
            .add_modifier(Modifier::BOLD);
        if app.palette.no_color {
            style.add_modifier(Modifier::REVERSED)
        } else {
            style
        }
    } else {
        text
    };
    write(buf, area.x, y, 8, "[ Test ]", style);
    chrome::register(Rect::new(area.x, y, 8, 1), Hit::Key(KeyCode::F(6)));
    y += 1 + u16::from(wide);
    if y < area.bottom() {
        write(
            buf,
            area.x,
            y,
            area.width,
            &format!(
                "{} How does this work?  F4 help",
                if wizard.expanded && wide { "▾" } else { "▸" }
            ),
            text.fg(app.palette.focus),
        );
        chrome::register(Rect::new(area.x, y, area.width, 1), Hit::Key(KeyCode::F(4)));
        y += 1;
    }
    if wide && wizard.expanded && y < area.bottom() {
        lines_into(
            buf,
            Rect::new(area.x, y, area.width, area.bottom() - y),
            "Uses your existing OpenSSH configuration. SSH aliases and FIDO hardware keys are supported.\n\nNo private keys or authentication secrets are stored by HomeStack. The test only executes true.\n\nF4 opens detailed contextual help.",
            0,
            muted,
        );
    }
}

fn discovery_details(buf: &mut Buffer, area: Rect, app: &App, wizard: &Wizard) {
    if area.height < 5 {
        return;
    }
    let text = text_style(&app.palette);
    let mut lines = if let Some(report) = &wizard.discovery {
        report.summary()
    } else {
        format!(
            "Server: {}\nInspect runtime, configuration and ports read-only.\nNo server configuration will be changed.",
            wizard.target
        )
    };
    if !wizard.message.is_empty()
        && !wizard
            .discovery
            .as_ref()
            .is_some_and(|report| report.diagnostic.contains(&wizard.message))
    {
        lines.push_str(&format!("\n{}", wizard.message));
    }
    let current = wizard.discovery_status();
    let category = if matches!(current, "Discovering" | "Outdated") {
        current
    } else {
        wizard
            .discovery
            .as_ref()
            .and_then(|r| r.problem())
            .map_or(current, |p| p.0)
    };
    let (mark, color) = match current {
        "Discovered" => ("✓ ", app.palette.ok),
        "Not discovered" | "Discovering" | "Outdated" | "Cancelled" => ("", app.palette.muted),
        _ => ("✗ ", app.palette.error),
    };
    write(
        buf,
        area.x,
        area.y,
        area.width,
        &format!("{mark}{category}"),
        text.fg(color).add_modifier(Modifier::BOLD),
    );
    let style = if wizard.focus == 1 {
        text.bg(app.palette.selection).fg(app.palette.focus)
    } else {
        text
    };
    let label = if wizard.discovering {
        "[ Discovering… Esc cancels ]"
    } else if wizard.discovery.is_some() {
        "[ Discover again ]"
    } else {
        "[ Discover ]"
    };
    write(buf, area.x, area.y + 2, area.width, label, style);
    if !wizard.testing {
        chrome::register(
            Rect::new(area.x, area.y + 2, label.chars().count() as u16, 1),
            Hit::PangolinDiscovery(1),
        );
    }
    if wizard.discovery.is_some() {
        write(
            buf,
            area.x,
            area.y + 3,
            area.width,
            "[ Detailed report ]",
            if wizard.focus == 2 {
                text.bg(app.palette.selection).fg(app.palette.focus)
            } else {
                text
            },
        );
        chrome::register(Rect::new(area.x, area.y + 3, 19, 1), Hit::PangolinDiscovery(2));
    }
    lines_into(
        buf,
        Rect::new(area.x, area.y + 4, area.width, area.height - 4),
        &lines,
        0,
        text,
    );
}

fn prerequisites_details(buf: &mut Buffer, area: Rect, app: &App, wizard: &Wizard) {
    if area.height < 4 {
        return;
    }
    let text = text_style(&app.palette);
    let muted = text.fg(app.palette.muted);
    let focus = |at| {
        if wizard.focus == at {
            let style = text
                .bg(app.palette.selection)
                .fg(app.palette.focus)
                .add_modifier(Modifier::BOLD);
            if app.palette.no_color {
                style.add_modifier(Modifier::REVERSED)
            } else {
                style
            }
        } else {
            text
        }
    };
    // Headers and confirmation remain visible while the instruction body scrolls.
    for (index, title) in ["Pangolin installation", "Bootstrap API key"].iter().enumerate() {
        let y = area.y + index as u16;
        write(
            buf,
            area.x,
            y,
            area.width,
            &format!("{} {title}", if wizard.instructions[index] { "▾" } else { "▸" }),
            focus(index + 1),
        );
        chrome::register(
            Rect::new(area.x, y, area.width, 1),
            Hit::PangolinPrerequisite(index + 1),
        );
    }
    let checkbox_y = area.bottom() - 2;
    write(
        buf,
        area.x,
        checkbox_y,
        area.width,
        &format!(
            "[{}] I have completed these prerequisites",
            if wizard.prerequisites_confirmed() { "x" } else { " " }
        ),
        focus(3),
    );
    chrome::register(
        Rect::new(area.x, checkbox_y, area.width, 1),
        Hit::PangolinPrerequisite(3),
    );
    if !wizard.message.is_empty() {
        write(
            buf,
            area.x,
            checkbox_y + 1,
            area.width,
            &wizard.message,
            text.fg(app.palette.error),
        );
    }
    if area.height > 4 {
        let mut instructions = Vec::new();
        instructions.push(format!("Selected server: {}", wizard.target));
        instructions.push(if wizard.instructions[0] {
            INSTALLATION.to_owned()
        } else {
            "Pangolin Server must be installed and running on the selected server.".to_owned()
        });
        instructions.push(if wizard.instructions[1] {
            BOOTSTRAP.to_owned()
        } else {
            "Create the temporary homestack-bootstrap Root API key.".to_owned()
        });
        lines_into(
            buf,
            Rect::new(area.x, area.y + 2, area.width, area.height - 4),
            &instructions.join("\n\n"),
            wizard.prerequisites_scroll,
            muted,
        );
    }
}

fn lines_into(buf: &mut Buffer, area: Rect, text: &str, scroll: usize, style: Style) {
    let lines: Vec<String> = text
        .lines()
        .flat_map(|line| wrap(line, usize::from(area.width)))
        .collect();
    let scroll = scroll.min(lines.len().saturating_sub(usize::from(area.height)));
    for (row, line) in lines.iter().skip(scroll).take(usize::from(area.height)).enumerate() {
        write(buf, area.x, area.y + row as u16, area.width, line, style);
    }
}

fn modal(buf: &mut Buffer, area: Rect, app: &App, wizard: &Wizard) {
    // The same centered, framed and scrollable panel convention as reports.
    let width = area.width.saturating_sub(8).min(92);
    let height = area.height.saturating_sub(4).min(28);
    let rect = Rect::new(
        area.x + (area.width - width) / 2,
        area.y + (area.height - height) / 2,
        width,
        height,
    );
    let title = if wizard.help_open {
        format!("help · {}. {}", wizard.selected_step + 1, STAGES[wizard.selected_step])
    } else {
        if wizard.selected_step == 5 {
            "Bootstrap API key report"
        } else if wizard.selected_step == 4 {
            "Private API tunnel report"
        } else if wizard.selected_step == 3 {
            "Integration API plan / report"
        } else if wizard.selected_step == 2 {
            "Pangolin discovery report"
        } else {
            "SSH verification report"
        }
        .to_owned()
    };
    // Keep only the modal frame's own registered controls; clicks through
    // the help panel must never reach the form or step list underneath.
    chrome::clear_hits();
    let inner = frame(
        buf,
        rect,
        &app.palette,
        &Sides {
            top_left: vec![Control::plain(&title)],
            bottom_left: vec![
                Control::named("↑↓", "scroll", KeyCode::Down),
                Control::named("Esc", "close", KeyCode::Esc),
            ],
            focused: true,
            ..Sides::default()
        },
    );
    let (content, scroll) = if wizard.help_open {
        (HELP[wizard.selected_step].to_owned(), wizard.help_scroll)
    } else if wizard.selected_step == 5 {
        (wizard.bootstrap_report(app.utc_offset), wizard.report_scroll)
    } else if wizard.selected_step == 4 {
        (
            format!(
                "{}{}",
                wizard.tunnel.text(app.utc_offset),
                if wizard.message.is_empty() {
                    String::new()
                } else {
                    format!("\n{}", wizard.message)
                }
            ),
            wizard.report_scroll,
        )
    } else if wizard.selected_step == 3 {
        (
            wizard.api.as_ref().map(|r| r.text(app.utc_offset)).unwrap_or_default(),
            wizard.report_scroll,
        )
    } else if wizard.selected_step == 2 {
        (
            wizard
                .discovery
                .as_ref()
                .map(|r| r.text(app.utc_offset))
                .unwrap_or_default(),
            wizard.report_scroll,
        )
    } else if let Some(report) = &wizard.report {
        (
            format!(
                "Target: {}\nDate (local): {}\nResult: {} ({})\n\n{}",
                report.target,
                report.local_date(app.utc_offset),
                if report.passed { "PASS" } else { "FAIL" },
                report.duration(),
                report.detail
            ),
            wizard.report_scroll,
        )
    } else {
        (String::new(), 0)
    };
    lines_into(
        buf,
        Rect::new(
            inner.x + 1,
            inner.y + 1,
            inner.width.saturating_sub(2),
            inner.height.saturating_sub(2),
        ),
        &content,
        scroll,
        text_style(&app.palette),
    );
}

fn api_details(buf: &mut Buffer, area: Rect, app: &App, wizard: &Wizard) {
    if area.height < 4 {
        return;
    }
    let text = text_style(&app.palette);
    let status = wizard.api_status();
    let color = match status {
        "Verified" => app.palette.ok,
        "Failed" | "Permission denied" => app.palette.error,
        _ => app.palette.muted,
    };
    write(
        buf,
        area.x,
        area.y,
        area.width,
        &format!("Status: {status}"),
        text.fg(color).add_modifier(Modifier::BOLD),
    );
    let buttons = [
        (
            1,
            if wizard.api_running {
                "[ Working… Esc cancels ]"
            } else if wizard.api.is_some() {
                "[ Verify again ]"
            } else {
                "[ Inspect API ]"
            },
        ),
        (2, "[ View plan / Details ]"),
        (3, "[ Apply changes ]"),
    ];
    for (row, (control, label)) in buttons.iter().enumerate() {
        if *control == 2 && wizard.api.is_none() || *control == 3 && !wizard.can_apply_api() {
            continue;
        }
        let style = if wizard.focus == *control {
            text.bg(app.palette.selection).fg(app.palette.focus)
        } else {
            text
        };
        write(buf, area.x, area.y + 1 + row as u16, area.width, label, style);
        if !wizard.testing {
            chrome::register(
                Rect::new(area.x, area.y + 1 + row as u16, area.width.min(label.len() as u16), 1),
                Hit::PangolinApi(*control),
            );
        }
    }
    if area.height > 4 {
        let summary = if !wizard.message.is_empty() {
            wizard.message.clone()
        } else if let Some(r) = &wizard.api {
            if status == "Outdated" {
                "Server or discovery changed; inspect again. Previous results cannot authorize changes.".into()
            } else if status == "Verified" {
                format!(
                    "{}\nAPI port: {} · Loopback only\n{}\nLast verification: historical; F7 for date and details.",
                    r.endpoint,
                    r.port,
                    r.explanation()
                )
            } else if let Some(p) = &r.plan {
                format!(
                    "{}\nRestart: {} only\nTemporary Pangolin interruption (Sites, Resources, sessions).\nF7: files, values, backup and verification plan.",
                    p.edits.join("\n"),
                    p.service
                )
            } else {
                format!(
                    "Server: {}\n{}{}\n{}",
                    wizard.target,
                    if r.access == "Externally published" {
                        "WARNING: API is externally published.\n"
                    } else {
                        ""
                    },
                    r.explanation(),
                    r.changed
                )
            }
        } else {
            format!(
                "Server: {}\nRevalidate discovery and inspect private API health.\nNo keys or tunnel are used.",
                wizard.target
            )
        };
        lines_into(
            buf,
            Rect::new(area.x, area.y + 4, area.width, area.height - 4),
            &summary,
            0,
            text,
        );
    }
}

fn approval(buf: &mut Buffer, area: Rect, app: &App, wizard: &Wizard) {
    chrome::clear_hits();
    let width = area.width.saturating_sub(4).min(94);
    let height = area.height.saturating_sub(2).min(26);
    let rect = Rect::new(
        area.x + (area.width - width) / 2,
        area.y + (area.height - height) / 2,
        width,
        height,
    );
    let inner = frame(
        buf,
        rect,
        &app.palette,
        &Sides {
            top_left: vec![Control::plain("approve Integration API changes")],
            bottom_left: vec![
                Control::named("Tab", "select", KeyCode::Tab),
                Control::named("Enter", "confirm", KeyCode::Enter),
                Control::named("Esc", "cancel", KeyCode::Esc),
            ],
            focused: true,
            ..Sides::default()
        },
    );
    let text = text_style(&app.palette);
    let Some(plan) = wizard.api.as_ref().and_then(|a| a.plan.as_ref()) else {
        return;
    };
    let content = format!(
        "Server: {}\nWARNING: temporary Pangolin interruption, including Sites, Resources and active sessions.\n{}\nFiles: {}\nBackup: {}\nRecreate only service: {}{}\nFiles and runtime will be revalidated. Backups stay on the VPS; no automatic rollback.",
        wizard.target,
        plan.edits.join("\n"),
        plan.files
            .iter()
            .filter(|f| f.before != f.after)
            .map(|f| f.path.as_str())
            .collect::<Vec<_>>()
            .join(", "),
        plan.backup,
        plan.service,
        if plan.external {
            " Existing external API connectivity will be removed."
        } else {
            ""
        }
    );
    lines_into(
        buf,
        Rect::new(
            inner.x + 1,
            inner.y,
            inner.width.saturating_sub(2),
            inner.height.saturating_sub(2),
        ),
        &content,
        0,
        text,
    );
    for (yes, x, label, control) in [
        (false, inner.x + 1, "[ Cancel ]", 4),
        (true, inner.x + 16, "[ Apply ]", 5),
    ] {
        let style = if wizard.api_confirm_yes == yes {
            text.bg(app.palette.selection)
                .fg(app.palette.focus)
                .add_modifier(Modifier::BOLD)
        } else {
            text
        };
        write(buf, x, inner.bottom() - 1, label.len() as u16, label, style);
        chrome::register(
            Rect::new(x, inner.bottom() - 1, label.len() as u16, 1),
            Hit::PangolinApi(control),
        );
    }
}

fn tunnel_details(buf: &mut Buffer, area: Rect, app: &App, wizard: &Wizard) {
    if area.height < 3 {
        return;
    }
    let text = text_style(&app.palette);
    let state = wizard.tunnel.current.state;
    let color = match state {
        super::pangolin_tunnel::State::Connected => app.palette.ok,
        super::pangolin_tunnel::State::Failed | super::pangolin_tunnel::State::Lost => app.palette.error,
        _ => app.palette.muted,
    };
    write(
        buf,
        area.x,
        area.y,
        area.width,
        &format!("Status: {}", state.label()),
        text.fg(color).add_modifier(Modifier::BOLD),
    );
    let connected = state == super::pangolin_tunnel::State::Connected;
    if !wizard.tunnel.busy() {
        let label = if connected {
            "[ Verify ]"
        } else if matches!(
            state,
            super::pangolin_tunnel::State::Failed
                | super::pangolin_tunnel::State::Lost
                | super::pangolin_tunnel::State::Cancelled
        ) {
            "[ Retry ]"
        } else {
            "[ Connect ]"
        };
        let style = if wizard.focus == 1 {
            text.bg(app.palette.selection).fg(app.palette.focus)
        } else {
            text
        };
        write(buf, area.x, area.y + 1, area.width, label, style);
        if !wizard.testing && wizard.tunnel_available() {
            chrome::register(
                Rect::new(area.x, area.y + 1, label.len() as u16, 1),
                Hit::PangolinTunnel(1),
            );
        }
        if connected {
            let style = if wizard.focus == 2 {
                text.bg(app.palette.selection).fg(app.palette.focus)
            } else {
                text
            };
            write(
                buf,
                area.x + 13,
                area.y + 1,
                area.width.saturating_sub(13),
                "[ Disconnect ]",
                style,
            );
            chrome::register(Rect::new(area.x + 13, area.y + 1, 14, 1), Hit::PangolinTunnel(2));
        }
    }
    let summary = if let Some(error) = wizard.tunnel.current.error {
        error.message().to_owned()
    } else if wizard.tunnel.busy() {
        "Establishing private SSH forwarding and verifying API health… Esc cancels.".into()
    } else if let Some(spec) = wizard.tunnel_spec() {
        let mut lines = format!("Server: {}\nRemote API: {}", spec.target, spec.remote());
        if connected {
            if let Some(port) = wizard.tunnel.current.port {
                lines.push_str(&format!("\nLocal endpoint: 127.0.0.1:{port}\nAPI health: PASS"));
            }
        } else {
            lines.push_str("\nA private SSH tunnel will provide API access. No API key is used.");
        }
        lines
    } else {
        "Private API configuration changed. Verify Step 4 again.".into()
    };
    lines_into(
        buf,
        Rect::new(area.x, area.y + 2, area.width, area.height - 2),
        &summary,
        0,
        text,
    );
}

fn bootstrap_details(buf: &mut Buffer, area: Rect, app: &App, wizard: &Wizard) {
    if area.height < 6 || area.width < 24 {
        return;
    }
    let text = text_style(&app.palette);
    let muted = text.fg(app.palette.muted);
    let focused = |at: usize| {
        if wizard.focus == at {
            let style = text.bg(app.palette.selection).fg(app.palette.focus);
            if app.palette.no_color {
                style.add_modifier(Modifier::REVERSED)
            } else {
                style
            }
        } else {
            text
        }
    };
    let label = 17;
    let status = wizard.bootstrap_status();
    let connected = wizard.tunnel_connected();
    let mut y = area.y;
    write(
        buf,
        area.x,
        y,
        area.width,
        &format!("{:<17}{status}", "Credential"),
        text.fg(match status {
            "Verified" => app.palette.ok,
            "Failed" => app.palette.error,
            _ => app.palette.muted,
        })
        .add_modifier(Modifier::BOLD),
    );
    y += 1;
    write(
        buf,
        area.x,
        y,
        area.width,
        &format!("{:<17}{}", "API tunnel", wizard.tunnel.current.state.label()),
        if connected { text.fg(app.palette.ok) } else { muted },
    );
    y += 1;
    write(
        buf,
        area.x,
        y,
        area.width,
        &format!("{:<17}{}", "Pangolin server", wizard.target),
        text,
    );
    y += 1;
    // Single-line fields keep both inputs and actions visible at 80x24.
    let capacity = usize::from(area.width.saturating_sub(label + 2)).max(1);
    for (control, title) in [(1, "Organization ID"), (2, "Temporary key")] {
        write(
            buf,
            area.x,
            y,
            label,
            title,
            if wizard.focus == control {
                text.fg(app.palette.focus)
            } else {
                text
            },
        );
        let bracket = if wizard.focus == control {
            text.fg(app.palette.focus)
        } else {
            muted
        };
        write(buf, area.x + label, y, 1, "[", bracket);
        write(buf, area.x + label + 1 + capacity as u16, y, 1, "]", bracket);
        let x = area.x + label + 1;
        if control == 1 {
            let value = &wizard.bootstrap.organization;
            let cursor = wizard.bootstrap.cursor;
            let start = if wizard.focus == 1 {
                cursor.saturating_sub(capacity - 1)
            } else {
                0
            };
            let shown: String = value.chars().skip(start).take(capacity).collect();
            write(buf, x, y, capacity as u16, &shown, text);
            if wizard.focus == 1 {
                let c = value.chars().nth(cursor).unwrap_or(' ');
                write(
                    buf,
                    x + (cursor - start) as u16,
                    y,
                    1,
                    &c.to_string(),
                    text.add_modifier(Modifier::REVERSED),
                );
            }
        } else {
            // A fixed mask: neither characters nor length are revealed.
            let dots = if wizard.bootstrap.key.is_empty() {
                0
            } else {
                32.min(capacity - 1)
            };
            write(buf, x, y, capacity as u16, &"•".repeat(dots), text);
            if wizard.focus == 2 {
                write(buf, x + dots as u16, y, 1, " ", text.add_modifier(Modifier::REVERSED));
            }
        }
        if wizard.bootstrap.running == 0 {
            chrome::register(Rect::new(area.x, y, area.width, 1), Hit::PangolinBootstrap(control));
        }
        y += 1;
    }
    if wizard.bootstrap.running != 0 {
        write(buf, area.x, y, area.width, "[ Verifying… Esc cancels ]", focused(3));
    } else {
        let verify = if status == "Verified" {
            "[ Verify again ]"
        } else {
            "[ Verify key ]"
        };
        let style = if connected { focused(3) } else { muted };
        write(buf, area.x, y, verify.len() as u16, verify, style);
        chrome::register(Rect::new(area.x, y, verify.len() as u16, 1), Hit::PangolinBootstrap(3));
        let clear = area.x + verify.len() as u16 + 3;
        write(buf, clear, y, 9, "[ Clear ]", focused(4));
        chrome::register(Rect::new(clear, y, 9, 1), Hit::PangolinBootstrap(4));
    }
    // At 80x24 the result block needs the spacer row.
    y += if area.height >= 17 { 2 } else { 1 };
    if y >= area.bottom() {
        return;
    }
    let memory = "The temporary key stays in memory only.";
    let (lines, style) = if wizard.bootstrap.running != 0 {
        (
            "Verifying read-only through the SSH tunnel… Esc cancels.\nNo Pangolin objects are created or changed."
                .to_owned(),
            muted,
        )
    } else if status == "Verified" {
        let outcome = &wizard.bootstrap_check().map(|c| c.outcome.clone()).unwrap_or_default();
        let mut lines = format!(
            "Authorization\n  {:<24}PASS\n  {:<24}PASS\n  {:<24}PASS\n  {:<24}Assigned\n  {:<24}Assigned\n  {:<24}{}\nWrite operations were not executed.",
            "Root access",
            "listApiKeys",
            "listApiKeyActions",
            "createApiKey",
            "setApiKeyActions",
            "Organization",
            if outcome.organization == Some(true) {
                "Confirmed"
            } else {
                "Not confirmed yet"
            }
        );
        if !outcome.existing.is_empty() {
            lines.push_str("\nExisting homestack key found; Step 7 will handle it without duplicates.");
        }
        (lines, text)
    } else if !connected {
        (
            format!("Connect the API tunnel in Step 5 before validating the bootstrap key.\n{memory}"),
            muted,
        )
    } else if let Some(problem) = wizard.bootstrap_check().and_then(|c| c.outcome.problem) {
        let action = problem.action();
        (
            format!(
                "{}{}{action}\n{memory}",
                problem.message(),
                if action.is_empty() { "" } else { "\n" }
            ),
            if problem == super::pangolin_key::Problem::Cancelled {
                muted
            } else {
                text.fg(app.palette.error)
            },
        )
    } else {
        (
            format!("Enter the Organization ID and the temporary key, then press F6.\n{memory}"),
            muted,
        )
    };
    lines_into(
        buf,
        Rect::new(area.x, y, area.width, area.bottom() - y),
        &lines,
        0,
        style,
    );
}
