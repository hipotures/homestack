//! The border grammar of the mockups (`docs/tui/mockups/00-system.txt`,
//! part 2), after btop: a panel's controls sit in its border. Titles in the
//! top border are framed `┐title┌`, buttons in the bottom border `┘label└`;
//! the key of a control is one of its letters in the hotkey colour; the panel
//! digit is a superscript before its name.

use ratatui::buffer::Buffer;
use ratatui::layout::Rect;
use ratatui::style::{Color, Modifier, Style};

use super::theme::Palette;

const SUPERSCRIPT: [char; 10] = ['⁰', '¹', '²', '³', '⁴', '⁵', '⁶', '⁷', '⁸', '⁹'];

/// How a control is drawn.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum Look {
    #[default]
    Normal,
    /// Does nothing now (no row selected): all of it muted.
    Muted,
    /// The current step or stage.
    Raised,
}

/// One title, button or counter in a border.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Control {
    pub text: String,
    /// The character index of the key letter.
    pub key: Option<usize>,
    /// The panel digit, drawn as a superscript before the text.
    pub digit: Option<u8>,
    pub look: Look,
    pub bold: bool,
}

impl Control {
    /// A panel's name with its digit: `¹workspaces`.
    #[must_use]
    pub fn panel(digit: u8, name: &str) -> Self {
        Self {
            text: name.to_owned(),
            digit: Some(digit),
            bold: true,
            ..Self::default()
        }
    }

    /// A control whose key follows `&`: `re&size` is resize with s.
    #[must_use]
    pub fn key(markup: &str) -> Self {
        let mut text = String::new();
        let mut key = None;
        let mut marked = false;
        for c in markup.chars() {
            if c == '&' && !marked && key.is_none() {
                marked = true;
                continue;
            }
            if marked {
                key = Some(text.chars().count());
                marked = false;
            }
            text.push(c);
        }
        Self {
            text,
            key,
            ..Self::default()
        }
    }

    /// Text without a key: a counter, a value.
    #[must_use]
    pub fn plain(text: &str) -> Self {
        Self {
            text: text.to_owned(),
            ..Self::default()
        }
    }

    #[must_use]
    pub fn muted(mut self) -> Self {
        self.look = Look::Muted;
        self
    }

    #[must_use]
    pub fn raised(mut self) -> Self {
        self.look = Look::Raised;
        self
    }

    /// Muted unless `on`.
    #[must_use]
    pub fn enabled(self, on: bool) -> Self {
        if on { self } else { self.muted() }
    }

    /// The key letter, lower case.
    #[must_use]
    pub fn key_char(&self) -> Option<char> {
        self.key
            .and_then(|at| self.text.chars().nth(at))
            .map(|c| c.to_ascii_lowercase())
    }

    /// Cells taken with the frame characters.
    #[must_use]
    pub fn width(&self) -> u16 {
        let cells = self.text.chars().count() + usize::from(self.digit.is_some()) + 2;
        u16::try_from(cells).unwrap_or(u16::MAX)
    }
}

/// What a panel shows in its border.
#[derive(Debug, Clone, Default)]
pub struct Sides {
    pub top_left: Vec<Control>,
    pub top_right: Vec<Control>,
    pub bottom_left: Vec<Control>,
    pub bottom_right: Vec<Control>,
    pub focused: bool,
}

fn sum(controls: &[Control]) -> u16 {
    controls.iter().map(Control::width).fold(0, u16::saturating_add)
}

/// Leave controls out until they fit in `room`: first from the end of the
/// left side (the panel name stays), then from the start of the right.
fn fit(left: &[Control], right: &[Control], room: u16) -> (Vec<Control>, Vec<Control>) {
    let mut left = left.to_vec();
    let mut right = right.to_vec();
    while sum(&left).saturating_add(sum(&right)) > room {
        if left.len() > 1 {
            left.pop();
        } else if !right.is_empty() {
            right.remove(0);
        } else if left.pop().is_none() {
            break;
        }
    }
    (left, right)
}

/// Text style for normal content of the panel.
#[must_use]
pub fn text_style(palette: &Palette) -> Style {
    Style::default().fg(palette.foreground).bg(palette.background)
}

fn put(buf: &mut Buffer, x: u16, y: u16, symbol: char, style: Style) {
    if buf.area.contains(ratatui::layout::Position { x, y }) {
        let cell = &mut buf[(x, y)];
        // A control character (a newline from a server's text) cannot be drawn.
        cell.set_char(if symbol.is_control() { ' ' } else { symbol });
        cell.set_style(style);
    }
}

/// Draw `control` at `x` of row `y` with its frame characters; the cells used.
fn draw_control(
    buf: &mut Buffer,
    x: u16,
    y: u16,
    control: &Control,
    palette: &Palette,
    ends: (char, char),
    border: Style,
) -> u16 {
    let base = match control.look {
        Look::Normal => palette.foreground,
        Look::Muted => palette.muted,
        Look::Raised => palette.focus,
    };
    let mut style = Style::default().fg(base).bg(palette.background);
    if control.bold || control.look == Look::Raised {
        style = style.add_modifier(Modifier::BOLD);
    }
    let key_style = if control.look == Look::Muted {
        style
    } else {
        let mut key = style.fg(palette.hotkey).add_modifier(Modifier::BOLD);
        if palette.no_color {
            key = key.add_modifier(Modifier::UNDERLINED);
        }
        key
    };
    let mut at = x;
    put(buf, at, y, ends.0, border);
    at += 1;
    if let Some(digit) = control.digit {
        let symbol = SUPERSCRIPT.get(usize::from(digit)).copied().unwrap_or('·');
        put(buf, at, y, symbol, key_style);
        at += 1;
    }
    for (index, c) in control.text.chars().enumerate() {
        let chosen = if Some(index) == control.key { key_style } else { style };
        put(buf, at, y, c, chosen);
        at += 1;
    }
    put(buf, at, y, ends.1, border);
    at + 1 - x
}

/// Draw the panel border with its controls and clear the inside; the inside.
pub fn frame(buf: &mut Buffer, area: Rect, palette: &Palette, sides: &Sides) -> Rect {
    if area.width < 2 || area.height < 2 {
        return Rect::new(area.x, area.y, 0, 0);
    }
    let border_color: Color = if sides.focused { palette.focus } else { palette.border };
    let border = Style::default().fg(border_color).bg(palette.background);
    let inside = text_style(palette);
    let (left, top, right, bottom) = (area.x, area.y, area.right() - 1, area.bottom() - 1);
    for y in top..=bottom {
        for x in left..=right {
            let (symbol, style) = match (x, y) {
                (x, y) if x == left && y == top => ('╭', border),
                (x, y) if x == right && y == top => ('╮', border),
                (x, y) if x == left && y == bottom => ('╰', border),
                (x, y) if x == right && y == bottom => ('╯', border),
                (_, y) if y == top || y == bottom => ('─', border),
                (x, _) if x == left || x == right => ('│', border),
                _ => (' ', inside),
            };
            put(buf, x, y, symbol, style);
        }
    }
    let room = area.width.saturating_sub(4);
    for (row, ends, given_left, given_right) in [
        (top, ('┐', '┌'), &sides.top_left, &sides.top_right),
        (bottom, ('┘', '└'), &sides.bottom_left, &sides.bottom_right),
    ] {
        let (shown_left, shown_right) = fit(given_left, given_right, room);
        let mut x = left + 2;
        for control in &shown_left {
            let used = draw_control(buf, x, row, control, palette, ends, border);
            register_control(Rect::new(x, row, used, 1), control);
            x += used;
        }
        let mut x = right - 1 - sum(&shown_right);
        for control in &shown_right {
            let used = draw_control(buf, x, row, control, palette, ends, border);
            register_control(Rect::new(x, row, used, 1), control);
            x += used;
        }
    }
    Rect::new(area.x + 1, area.y + 1, area.width - 2, area.height - 2)
}

/// The scrollbar on the right border of `area` when `total` rows do not fit
/// in `visible`: `↑` under the corner, the thumb `█`, `↓` above the corner.
pub fn scrollbar(buf: &mut Buffer, area: Rect, palette: &Palette, offset: usize, visible: usize, total: usize) {
    if total <= visible || area.height < 5 {
        return;
    }
    let x = area.right() - 1;
    let border = Style::default().fg(palette.border).bg(palette.background);
    let thumb_style = Style::default().fg(palette.muted).bg(palette.background);
    let first = area.y + 1;
    let last = area.bottom() - 2;
    put(buf, x, first, '↑', border);
    put(buf, x, last, '↓', border);
    let track = usize::from(last - first - 1);
    let thumb = (track * visible).div_ceil(total).clamp(1, track);
    let start = (offset.min(total - visible) * (track - thumb)).div_ceil(total - visible);
    for row in 0..track {
        let y = first + 1 + u16::try_from(row).unwrap_or(0);
        if (start..start + thumb).contains(&row) {
            put(buf, x, y, '█', thumb_style);
        } else {
            put(buf, x, y, '│', border);
        }
    }
}

/// What a click on the screen means.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Hit {
    /// The key of a control in a border.
    Key(crossterm::event::KeyCode),
    /// A field of an action's form, and the key a click on its ‹ or › is.
    Field(usize, Option<crossterm::event::KeyCode>),
    /// A row of the install's settings.
    Setting(usize),
    /// A choice in the box of the focused setting.
    Choice(usize),
    /// An item of a list in a box (the menu, the themes).
    Item(usize),
}

thread_local! {
    /// Where the last drawing put what can be clicked, the latest last.
    static HITS: std::cell::RefCell<Vec<(Rect, Hit)>> = const { std::cell::RefCell::new(Vec::new()) };
}

/// Forget the clickable places (a new drawing starts).
pub fn clear_hits() {
    HITS.with(|hits| hits.borrow_mut().clear());
}

/// `area` means `hit` when clicked.
pub fn register(area: Rect, hit: Hit) {
    HITS.with(|hits| hits.borrow_mut().push((area, hit)));
}

/// What was drawn last at `x`, `y`.
#[must_use]
pub fn hit_at(x: u16, y: u16) -> Option<Hit> {
    let position = ratatui::layout::Position { x, y };
    HITS.with(|hits| {
        hits.borrow()
            .iter()
            .rev()
            .find(|(area, _)| area.contains(position))
            .map(|(_, hit)| *hit)
    })
}

/// The key a control stands for: its key letter, its panel digit, or the
/// key its label names (Enter, Esc, Tab, Space).
#[must_use]
pub fn control_key(control: &Control) -> Option<crossterm::event::KeyCode> {
    use crossterm::event::KeyCode;
    if let Some(digit) = control.digit {
        return char::from_digit(u32::from(digit), 10).map(KeyCode::Char);
    }
    if let Some(c) = control.key_char() {
        return Some(KeyCode::Char(c));
    }
    let text = control.text.to_lowercase();
    if text.contains("enter") || text.contains('↵') {
        Some(KeyCode::Enter)
    } else if text.contains("esc") {
        Some(KeyCode::Esc)
    } else if text.contains("tab") || text.contains('⇥') {
        Some(KeyCode::Tab)
    } else if text.contains("space") || text.contains('␣') {
        Some(KeyCode::Char(' '))
    } else {
        None
    }
}

fn register_control(area: Rect, control: &Control) {
    if control.look != Look::Muted
        && let Some(key) = control_key(control)
    {
        register(area, Hit::Key(key));
    }
}
