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
    /// The focused button: a bar in the accent colour, as a selected row.
    Chosen,
    /// The current step or stage.
    Raised,
}

/// One title, button or counter in a border.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Control {
    pub text: String,
    /// Character indices highlighted as the control's hotkey.
    pub hotkeys: Vec<usize>,
    /// The key a click on a named key stands for.
    pub code: Option<crossterm::event::KeyCode>,
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

    /// Highlight characters following `&`; the first is the key letter.
    #[must_use]
    pub fn key(markup: &str) -> Self {
        let mut text = String::new();
        let mut hotkeys = Vec::new();
        let mut marked = false;
        for c in markup.chars() {
            if c == '&' && !marked {
                marked = true;
                continue;
            }
            if marked {
                hotkeys.push(text.chars().count());
                marked = false;
            }
            text.push(c);
        }
        Self {
            text,
            hotkeys,
            ..Self::default()
        }
    }

    /// A named key before its action: `TAB group`, the name in the hotkey
    /// colour; a click is `code`.
    #[must_use]
    pub fn named(name: &str, action: &str, code: crossterm::event::KeyCode) -> Self {
        Self {
            text: format!("{name} {action}"),
            hotkeys: (0..name.chars().count()).collect(),
            code: Some(code),
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

    /// The focused button of a box.
    #[must_use]
    pub fn chosen(mut self) -> Self {
        self.look = Look::Chosen;
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
        self.hotkeys
            .first()
            .and_then(|at| self.text.chars().nth(*at))
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

/// Prominent notices retain the row background and remain bold without color.
#[must_use]
pub fn alert_style(palette: &Palette, base: Style) -> Style {
    base.fg(palette.alert)
        .remove_modifier(Modifier::DIM)
        .add_modifier(Modifier::BOLD)
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
        Look::Chosen => palette.background,
    };
    let background = if control.look == Look::Chosen {
        palette.focus
    } else {
        palette.background
    };
    let mut style = Style::default().fg(base).bg(background);
    if control.bold || matches!(control.look, Look::Raised | Look::Chosen) {
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
        let in_key = control.hotkeys.contains(&index);
        let chosen = if in_key { key_style } else { style };
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

/// What a scrollbar scrolls; a press or a drag on it sets that (`App::scroll_to`).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Scrolls {
    /// The workspaces list (the selection moves with it).
    List,
    Log,
    Storage,
    /// The plan in the action box.
    Action,
    /// The setup tree (the highlight moves with it), its details, its review.
    SetupTree,
    SetupDetails,
    SetupReview,
    /// The install's review.
    InstallReview,
}

/// A scrollbar as drawn: what it scrolls and where its parts are.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Bar {
    pub scrolls: Scrolls,
    /// The rows of `↑` and `↓`; the track lies between them.
    pub up: u16,
    pub down: u16,
    /// The thumb's length and its first row in the track.
    pub thumb: u16,
    pub start: u16,
    pub offset: usize,
    pub visible: usize,
    /// The farthest offset: the rows that do not fit.
    pub most: usize,
}

impl Bar {
    fn track(&self) -> u16 {
        self.down - self.up - 1
    }

    /// The offset that puts the thumb's first row at `row` of the track.
    fn offset_at(&self, row: i32) -> usize {
        let room = self.track() - self.thumb;
        if room == 0 {
            return self.offset;
        }
        let row = usize::try_from(row.clamp(0, i32::from(room))).unwrap_or(0);
        let room = usize::from(room);
        (row * self.most + room / 2) / room
    }

    /// A press at row `y`: the offset it puts, and where it holds the thumb
    /// for a drag (`None`: an arrow, a line at a time).
    #[must_use]
    pub fn press(&self, y: u16) -> (usize, Option<u16>) {
        if y <= self.up {
            return (self.offset.saturating_sub(1), None);
        }
        if y >= self.down {
            return ((self.offset + 1).min(self.most), None);
        }
        let row = y - self.up - 1;
        if (self.start..self.start + self.thumb).contains(&row) {
            return (self.offset, Some(row - self.start));
        }
        // Beside the thumb: the thumb's middle jumps there.
        let grip = self.thumb / 2;
        (self.offset_at(i32::from(row) - i32::from(grip)), Some(grip))
    }

    /// The offset of a drag at row `y`, the thumb held at `grip`.
    #[must_use]
    pub fn drag(&self, y: u16, grip: u16) -> usize {
        self.offset_at(i32::from(y) - i32::from(self.up) - 1 - i32::from(grip))
    }

    /// The offset `lines` further (back when negative), as the wheel moves it.
    #[must_use]
    pub fn wheel(&self, lines: isize) -> usize {
        self.offset.saturating_add_signed(lines).min(self.most)
    }
}

/// The scrollbar on the right border of `area` when `total` rows do not fit
/// in `visible`: `↑` under the corner, the thumb `█`, `↓` above the corner.
/// The mouse holds and drags its thumb, presses its arrows and its track.
pub fn scrollbar(
    buf: &mut Buffer,
    area: Rect,
    palette: &Palette,
    scrolls: Scrolls,
    offset: usize,
    visible: usize,
    total: usize,
) {
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
    let most = total - visible;
    let offset = offset.min(most);
    // Shorter than the track while rows do not fit, so it can move.
    let thumb = (track * visible)
        .div_ceil(total)
        .clamp(1, track.saturating_sub(1).max(1));
    let start = (offset * (track - thumb)).div_ceil(most);
    for row in 0..track {
        let y = first + 1 + u16::try_from(row).unwrap_or(0);
        if (start..start + thumb).contains(&row) {
            put(buf, x, y, '█', thumb_style);
        } else {
            put(buf, x, y, '│', border);
        }
    }
    let bar = Bar {
        scrolls,
        up: first,
        down: last,
        thumb: u16::try_from(thumb).unwrap_or(1),
        start: u16::try_from(start).unwrap_or(0),
        offset,
        visible,
        most,
    };
    register(Rect::new(x, first, 1, last - first + 1), Hit::Bar(bar));
}

/// What a click on the screen means.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Hit {
    /// The key of a control in a border.
    Key(crossterm::event::KeyCode),
    /// A field of an action's form, and the key a click on its ‹ or › is.
    Field(usize, Option<crossterm::event::KeyCode>),
    /// A part of an amount in a form: its field, the part, and -1 for its
    /// -, 0 for its number, 1 for its +.
    Amount(usize, super::action::Part, i8),
    /// A row of the setup tree (its index in the rows drawn) and the part
    /// clicked: 0 the arrow, 1 the checkbox, 2 the label.
    SetupRow(usize, u8),
    /// The details of the setup box (the wheel scrolls them).
    SetupDetails,
    /// A group in the setup box's bar: show it.
    SetupGroup(usize),
    /// A row of the install's settings.
    Setting(usize),
    /// A choice in the box of the focused setting.
    Choice(usize),
    /// An item of a list in a box (the menu, the themes).
    Item(usize),
    /// A theme in the Appearance section of the settings.
    Theme(usize),
    Integration(usize),
    PangolinStep(usize),
    PangolinPrerequisite(usize),
    PangolinDiscovery(usize),
    PangolinApi(usize),
    IntegrationField(usize),
    /// The log panel: the farthest it scrolls, the rows it shows and the
    /// line under the pointer (lines back from the newest; `None` below
    /// the last).
    Log {
        most: usize,
        visible: usize,
        line: Option<usize>,
    },
    /// A scrollbar.
    Bar(Bar),
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
    if let Some(code) = control.code {
        return Some(code);
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
