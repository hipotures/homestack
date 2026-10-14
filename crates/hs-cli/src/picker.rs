//! The choice lists of `homestack install` (ratatui): a list with the
//! choices one to a line, moved with the arrow keys, ticked with space when
//! several may be chosen, confirmed with enter. The state and the drawing
//! are separate from the terminal, so they are tested on a buffer.

use ratatui::Frame;
use ratatui::backend::TestBackend;
use ratatui::layout::{Constraint, Layout};
use ratatui::style::{Modifier, Style};
use ratatui::text::Line;
use ratatui::widgets::{Block, Borders, List, ListItem, ListState, Paragraph, Wrap};

/// One choice: what the option takes and what tells it apart.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Item {
    pub value: String,
    pub detail: String,
}

/// A key the picker understands, whatever produced it.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Key {
    Up,
    Down,
    PageUp,
    PageDown,
    Home,
    End,
    Space,
    Enter,
    Cancel,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Outcome {
    /// The indexes of the chosen items, in list order.
    Chosen(Vec<usize>),
    Cancelled,
}

pub struct Picker {
    title: String,
    description: String,
    multiple: bool,
    items: Vec<Item>,
    cursor: usize,
    ticked: Vec<bool>,
    outcome: Option<Outcome>,
    list: ListState,
}

const PAGE: usize = 5;

impl Picker {
    #[must_use]
    pub fn new(title: &str, multiple: bool, items: Vec<Item>) -> Self {
        let ticked = vec![false; items.len()];
        Self {
            title: title.to_owned(),
            description: String::new(),
            multiple,
            items,
            cursor: 0,
            ticked,
            outcome: None,
            list: ListState::default(),
        }
    }

    /// A few words on what the choice is for, drawn wrapped inside the window.
    #[must_use]
    pub fn with_description(mut self, description: &str) -> Self {
        self.description = description.to_owned();
        self
    }

    /// Set once the user has chosen or cancelled.
    #[must_use]
    pub fn outcome(&self) -> Option<Outcome> {
        self.outcome.clone()
    }

    pub fn handle(&mut self, key: Key) {
        if self.outcome.is_some() {
            return;
        }
        let last = self.items.len().saturating_sub(1);
        match key {
            Key::Up => self.cursor = self.cursor.saturating_sub(1),
            Key::Down => self.cursor = (self.cursor + 1).min(last),
            Key::PageUp => self.cursor = self.cursor.saturating_sub(PAGE),
            Key::PageDown => self.cursor = (self.cursor + PAGE).min(last),
            Key::Home => self.cursor = 0,
            Key::End => self.cursor = last,
            Key::Space => {
                if self.multiple
                    && let Some(mark) = self.ticked.get_mut(self.cursor)
                {
                    *mark = !*mark;
                }
            }
            Key::Enter => {
                let mut chosen: Vec<usize> = (0..self.items.len()).filter(|i| self.ticked[*i]).collect();
                if !self.multiple || chosen.is_empty() {
                    chosen = vec![self.cursor];
                }
                if !self.items.is_empty() {
                    self.outcome = Some(Outcome::Chosen(chosen));
                }
            }
            Key::Cancel => self.outcome = Some(Outcome::Cancelled),
        }
    }

    pub fn render(&mut self, frame: &mut Frame) {
        let [window, hint] = Layout::vertical([Constraint::Min(5), Constraint::Length(1)]).areas(frame.area());
        let block = Block::default()
            .borders(Borders::ALL)
            .title(format!(" {} ", self.title));
        let inside = block.inner(window);
        frame.render_widget(block, window);
        let width = usize::from(inside.width).max(1);
        let rows_needed = if self.description.is_empty() {
            0
        } else {
            u16::try_from(wrapped_lines(&self.description, width) + 1).unwrap_or(u16::MAX)
        };
        let [text, body] = Layout::vertical([
            Constraint::Length(rows_needed.min(inside.height.saturating_sub(3))),
            Constraint::Min(1),
        ])
        .areas(inside);
        if !self.description.is_empty() {
            frame.render_widget(
                Paragraph::new(self.description.as_str()).wrap(Wrap { trim: true }),
                text,
            );
        }
        let value_width = self
            .items
            .iter()
            .map(|item| item.value.chars().count())
            .max()
            .unwrap_or(0);
        let rows: Vec<ListItem> = self
            .items
            .iter()
            .enumerate()
            .map(|(index, item)| {
                let mark = match (self.multiple, self.ticked[index], index == self.cursor) {
                    (true, true, _) => "[x]",
                    (true, false, _) => "[ ]",
                    (false, _, true) => "(\u{2022})",
                    (false, _, false) => "( )",
                };
                ListItem::new(format!("{mark} {:<value_width$}  {}", item.value, item.detail))
            })
            .collect();
        self.list.select(Some(self.cursor));
        let list = List::new(rows)
            .highlight_symbol("> ")
            .highlight_style(Style::default().add_modifier(Modifier::BOLD | Modifier::REVERSED));
        frame.render_stateful_widget(list, body, &mut self.list);
        let keys = if self.multiple {
            "up/down move   space tick   enter confirm   esc cancel"
        } else {
            "up/down move   enter choose   esc cancel"
        };
        frame.render_widget(Paragraph::new(Line::from(keys)), hint);
    }

    /// The picker drawn on a `width` x `height` buffer, as text.
    #[must_use]
    pub fn render_to_string(&mut self, width: u16, height: u16) -> String {
        let mut terminal = match ratatui::Terminal::new(TestBackend::new(width, height)) {
            Ok(terminal) => terminal,
            Err(_) => return String::new(),
        };
        if terminal.draw(|frame| self.render(frame)).is_err() {
            return String::new();
        }
        let buffer = terminal.backend().buffer().clone();
        let width = usize::from(width);
        buffer
            .content()
            .chunks(width.max(1))
            .map(|row| {
                row.iter()
                    .map(|cell| cell.symbol())
                    .collect::<String>()
                    .trim_end()
                    .to_owned()
            })
            .collect::<Vec<_>>()
            .join("\n")
    }
}

/// Map a terminal key to the picker's.
fn key_of(event: &crossterm::event::KeyEvent) -> Option<Key> {
    use crossterm::event::{KeyCode, KeyEventKind, KeyModifiers};
    if event.kind == KeyEventKind::Release {
        return None;
    }
    if event.modifiers.contains(KeyModifiers::CONTROL) {
        return matches!(event.code, KeyCode::Char('c' | 'd')).then_some(Key::Cancel);
    }
    Some(match event.code {
        KeyCode::Up | KeyCode::Char('k') => Key::Up,
        KeyCode::Down | KeyCode::Char('j') => Key::Down,
        KeyCode::PageUp => Key::PageUp,
        KeyCode::PageDown => Key::PageDown,
        KeyCode::Home => Key::Home,
        KeyCode::End => Key::End,
        KeyCode::Char(' ') => Key::Space,
        KeyCode::Enter => Key::Enter,
        KeyCode::Esc | KeyCode::Char('q') => Key::Cancel,
        _ => return None,
    })
}

/// Run the picker on the terminal; the terminal is given back whatever
/// happens.
pub fn run_on_terminal(picker: &mut Picker) -> std::io::Result<Outcome> {
    let mut terminal = ratatui::try_init()?;
    let result = (|| loop {
        terminal.draw(|frame| picker.render(frame))?;
        if let Some(outcome) = picker.outcome() {
            return Ok(outcome);
        }
        if let crossterm::event::Event::Key(event) = crossterm::event::read()?
            && let Some(key) = key_of(&event)
        {
            picker.handle(key);
        }
    })();
    ratatui::restore();
    result
}

/// How many lines `text` takes when wrapped at whole words to `width`.
fn wrapped_lines(text: &str, width: usize) -> usize {
    let mut lines = 1;
    let mut used = 0;
    for word in text.split_whitespace() {
        let length = word.chars().count();
        if used == 0 {
            used = length;
        } else if used + 1 + length <= width {
            used += 1 + length;
        } else {
            lines += 1;
            used = length;
        }
        while used > width {
            lines += 1;
            used -= width;
        }
    }
    lines
}
