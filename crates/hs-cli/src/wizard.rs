//! The installer's screens (ratatui): one step per thing to choose, then a
//! summary of the choices and of every change, with the buttons Install and
//! Back. Esc goes back a step; nothing is changed before Install.
//!
//! The state and the drawing are separate from the terminal, so they are
//! tested on a buffer; where there is no terminal to draw on, the same steps
//! are asked by number ([`run_numbered`]).

use ratatui::Frame;
use ratatui::backend::TestBackend;
use ratatui::layout::{Constraint, Layout};
use ratatui::style::{Color, Modifier, Style};
use ratatui::text::{Line, Span};
use ratatui::widgets::{Block, Borders, List, ListItem, ListState, Paragraph, Wrap};

use crate::system::Console;

/// One thing that can be chosen.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Item {
    pub value: String,
    /// What tells it apart (type, nodes; name, node, state).
    pub detail: String,
}

impl Item {
    #[must_use]
    pub fn new(value: &str, detail: &str) -> Self {
        Self {
            value: value.to_owned(),
            detail: detail.to_owned(),
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
enum Kind {
    Pick {
        multiple: bool,
        items: Vec<Item>,
        ticked: Vec<bool>,
        cursor: usize,
    },
    Text {
        value: String,
    },
}

/// One screen of choices.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Step {
    pub title: String,
    /// What is chosen and why, in a sentence or two.
    pub note: String,
    kind: Kind,
}

impl Step {
    /// A list; `ticked` are chosen before the user changes anything (for a
    /// single choice, the first of them is where the cursor starts).
    #[must_use]
    pub fn pick(title: &str, note: &str, multiple: bool, items: Vec<Item>, ticked: &[usize]) -> Self {
        let mut marks = vec![false; items.len()];
        if multiple {
            for index in ticked {
                if let Some(mark) = marks.get_mut(*index) {
                    *mark = true;
                }
            }
        }
        let cursor = ticked.first().copied().filter(|i| *i < items.len()).unwrap_or(0);
        Self {
            title: title.to_owned(),
            note: note.to_owned(),
            kind: Kind::Pick {
                multiple,
                items,
                ticked: marks,
                cursor,
            },
        }
    }

    /// A line of text to type.
    #[must_use]
    pub fn text(title: &str, note: &str, value: &str) -> Self {
        Self {
            title: title.to_owned(),
            note: note.to_owned(),
            kind: Kind::Text {
                value: value.to_owned(),
            },
        }
    }

    fn answer(&self) -> Answer {
        match &self.kind {
            Kind::Pick {
                multiple,
                ticked,
                cursor,
                ..
            } => {
                if *multiple {
                    Answer::Picked((0..ticked.len()).filter(|i| ticked[*i]).collect())
                } else {
                    Answer::Picked(vec![*cursor])
                }
            }
            Kind::Text { value } => Answer::Text(value.trim().to_owned()),
        }
    }
}

/// What a step was answered with.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Answer {
    /// The indexes of the chosen items.
    Picked(Vec<usize>),
    Text(String),
}

/// The last screen: what was chosen and what will change.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct Summary {
    /// Headed groups of lines.
    pub sections: Vec<(String, Vec<String>)>,
    /// What stops the install; Install cannot be pressed while there is one.
    pub problems: Vec<String>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Outcome {
    /// Install was pressed (or, without a summary, the last step answered).
    Done(Vec<Answer>),
    Cancelled,
}

/// A key the wizard understands, whatever produced it.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Key {
    Up,
    Down,
    PageUp,
    PageDown,
    Home,
    End,
    Left,
    Right,
    Tab,
    Space,
    Enter,
    /// Esc: one step back.
    Back,
    /// Ctrl+C: leave without changing anything.
    Quit,
    Backspace,
    Char(char),
}

const PAGE: usize = 5;

pub struct Wizard {
    steps: Vec<Step>,
    /// The step shown; `steps.len()` is the summary.
    at: usize,
    with_summary: bool,
    summary: Option<Summary>,
    install_focused: bool,
    scroll: u16,
    outcome: Option<Outcome>,
    list: ListState,
}

impl Wizard {
    /// `with_summary`: after the last step comes the summary with Install;
    /// without it the last step ends the wizard.
    #[must_use]
    pub fn new(steps: Vec<Step>, with_summary: bool) -> Self {
        Self {
            steps,
            at: 0,
            with_summary,
            summary: None,
            install_focused: true,
            scroll: 0,
            outcome: None,
            list: ListState::default(),
        }
    }

    #[must_use]
    pub fn steps(&self) -> &[Step] {
        &self.steps
    }

    #[must_use]
    pub fn outcome(&self) -> Option<Outcome> {
        self.outcome.clone()
    }

    /// The answers of every step as they stand.
    #[must_use]
    pub fn answers(&self) -> Vec<Answer> {
        self.steps.iter().map(Step::answer).collect()
    }

    /// The summary is shown and has to be made from the answers.
    #[must_use]
    pub fn needs_summary(&self) -> bool {
        self.outcome.is_none() && self.with_summary && self.at == self.steps.len() && self.summary.is_none()
    }

    pub fn set_summary(&mut self, summary: Summary) {
        self.install_focused = summary.problems.is_empty();
        self.scroll = 0;
        self.summary = Some(summary);
    }

    fn on_summary(&self) -> bool {
        self.at >= self.steps.len()
    }

    fn back(&mut self) {
        if self.at == 0 {
            self.outcome = Some(Outcome::Cancelled);
        } else {
            self.at -= 1;
            self.summary = None;
        }
    }

    fn next(&mut self) {
        self.at += 1;
        if self.at >= self.steps.len() {
            if self.with_summary {
                self.summary = None;
            } else {
                self.outcome = Some(Outcome::Done(self.answers()));
            }
        }
    }

    pub fn handle(&mut self, key: Key) {
        if self.outcome.is_some() {
            return;
        }
        if key == Key::Quit {
            self.outcome = Some(Outcome::Cancelled);
            return;
        }
        if self.on_summary() {
            self.handle_summary(key);
            return;
        }
        let at = self.at;
        let mut advance = false;
        let mut back = false;
        match &mut self.steps[at].kind {
            Kind::Pick {
                multiple,
                items,
                ticked,
                cursor,
            } => {
                let last = items.len().saturating_sub(1);
                match key {
                    Key::Up => *cursor = cursor.saturating_sub(1),
                    Key::Down => *cursor = (*cursor + 1).min(last),
                    Key::PageUp => *cursor = cursor.saturating_sub(PAGE),
                    Key::PageDown => *cursor = (*cursor + PAGE).min(last),
                    Key::Home => *cursor = 0,
                    Key::End => *cursor = last,
                    Key::Space if *multiple => {
                        if let Some(mark) = ticked.get_mut(*cursor) {
                            *mark = !*mark;
                        }
                    }
                    Key::Enter if !items.is_empty() => {
                        // Enter with nothing ticked takes the item under the cursor.
                        if *multiple
                            && !ticked.iter().any(|mark| *mark)
                            && let Some(mark) = ticked.get_mut(*cursor)
                        {
                            *mark = true;
                        }
                        advance = true;
                    }
                    Key::Back => back = true,
                    _ => {}
                }
            }
            Kind::Text { value } => match key {
                Key::Char(c) => value.push(c),
                Key::Space => value.push(' '),
                Key::Backspace => {
                    value.pop();
                }
                Key::Enter if !value.trim().is_empty() => advance = true,
                Key::Back => back = true,
                _ => {}
            },
        }
        if advance {
            self.next();
        } else if back {
            self.back();
        }
    }

    fn handle_summary(&mut self, key: Key) {
        let can_install = self.summary.as_ref().is_some_and(|s| s.problems.is_empty());
        match key {
            Key::Left | Key::Right | Key::Tab => self.install_focused = can_install && !self.install_focused,
            Key::Up => self.scroll = self.scroll.saturating_sub(1),
            Key::Down => self.scroll = self.scroll.saturating_add(1),
            Key::PageUp => self.scroll = self.scroll.saturating_sub(10),
            Key::PageDown => self.scroll = self.scroll.saturating_add(10),
            Key::Enter if self.summary.is_some() => {
                if self.install_focused && can_install {
                    self.outcome = Some(Outcome::Done(self.answers()));
                } else {
                    self.back();
                }
            }
            Key::Back => self.back(),
            _ => {}
        }
    }

    pub fn render(&mut self, frame: &mut Frame) {
        if self.on_summary() {
            self.render_summary(frame);
        } else {
            self.render_step(frame);
        }
    }

    fn render_step(&mut self, frame: &mut Frame) {
        let total = self.steps.len() + usize::from(self.with_summary);
        let step = &self.steps[self.at];
        let title = if total > 1 {
            format!(" {} ({} of {}) ", step.title, self.at + 1, total)
        } else {
            format!(" {} ", step.title)
        };
        let [window, hint] = Layout::vertical([Constraint::Min(5), Constraint::Length(1)]).areas(frame.area());
        let block = Block::default().borders(Borders::ALL).title(title);
        let inside = block.inner(window);
        frame.render_widget(block, window);
        let note_rows = if step.note.is_empty() {
            0
        } else {
            u16::try_from(wrapped_lines(&step.note, usize::from(inside.width).max(1)) + 1).unwrap_or(u16::MAX)
        };
        let [text, body] = Layout::vertical([
            Constraint::Length(note_rows.min(inside.height.saturating_sub(3))),
            Constraint::Min(1),
        ])
        .areas(inside);
        frame.render_widget(Paragraph::new(step.note.as_str()).wrap(Wrap { trim: true }), text);
        let keys = match &step.kind {
            Kind::Pick {
                multiple,
                items,
                ticked,
                cursor,
            } => {
                let width = items.iter().map(|item| item.value.chars().count()).max().unwrap_or(0);
                let rows: Vec<ListItem> = items
                    .iter()
                    .enumerate()
                    .map(|(index, item)| {
                        let mark = match (*multiple, ticked[index], index == *cursor) {
                            (true, true, _) => "[x]",
                            (true, false, _) => "[ ]",
                            (false, _, true) => "(\u{2022})",
                            (false, _, false) => "( )",
                        };
                        ListItem::new(format!("{mark} {:<width$}  {}", item.value, item.detail))
                    })
                    .collect();
                self.list.select(Some(*cursor));
                let list = List::new(rows)
                    .highlight_symbol("> ")
                    .highlight_style(Style::default().add_modifier(Modifier::BOLD | Modifier::REVERSED));
                frame.render_stateful_widget(list, body, &mut self.list);
                if *multiple {
                    "up/down move   space tick   enter next   esc back"
                } else {
                    "up/down move   enter next   esc back"
                }
            }
            Kind::Text { value } => {
                frame.render_widget(Paragraph::new(format!("> {value}_")), body);
                "type, then enter   esc back"
            }
        };
        frame.render_widget(Paragraph::new(Line::from(keys)), hint);
    }

    fn render_summary(&mut self, frame: &mut Frame) {
        let total = self.steps.len() + 1;
        let title = if total > 1 {
            format!(" Summary ({total} of {total}) ")
        } else {
            " Summary ".to_owned()
        };
        let [window, buttons, hint] =
            Layout::vertical([Constraint::Min(5), Constraint::Length(1), Constraint::Length(1)]).areas(frame.area());
        let block = Block::default().borders(Borders::ALL).title(title);
        let inside = block.inner(window);
        frame.render_widget(block, window);
        let Some(summary) = &self.summary else {
            frame.render_widget(Paragraph::new("Checking the cluster..."), inside);
            return;
        };
        let mut lines: Vec<Line> = Vec::new();
        for problem in &summary.problems {
            lines.push(Line::styled(
                format!("! {problem}"),
                Style::default().fg(Color::Red).add_modifier(Modifier::BOLD),
            ));
        }
        if !summary.problems.is_empty() {
            lines.push(Line::from(""));
        }
        for (heading, items) in &summary.sections {
            lines.push(Line::styled(
                heading.clone(),
                Style::default().add_modifier(Modifier::BOLD),
            ));
            for item in items {
                lines.push(Line::from(format!("  {item}")));
            }
            lines.push(Line::from(""));
        }
        frame.render_widget(
            Paragraph::new(lines)
                .wrap(Wrap { trim: false })
                .scroll((self.scroll, 0)),
            inside,
        );
        let can_install = summary.problems.is_empty();
        let button = |label: &str, focused: bool, enabled: bool| {
            let mut style = Style::default();
            if !enabled {
                style = style.add_modifier(Modifier::DIM);
            } else if focused {
                style = style.add_modifier(Modifier::BOLD | Modifier::REVERSED);
            }
            Span::styled(format!("[ {label} ]"), style)
        };
        frame.render_widget(
            Paragraph::new(Line::from(vec![
                Span::raw(" "),
                button("Install", self.install_focused, can_install),
                Span::raw("   "),
                button("Back", !self.install_focused, true),
            ])),
            buttons,
        );
        frame.render_widget(
            Paragraph::new("left/right choose a button   enter press it   up/down scroll   esc back"),
            hint,
        );
    }

    /// The wizard drawn on a `width` x `height` buffer, as text.
    #[must_use]
    pub fn render_to_string(&mut self, width: u16, height: u16) -> String {
        let Ok(mut terminal) = ratatui::Terminal::new(TestBackend::new(width, height));
        if terminal.draw(|frame| self.render(frame)).is_err() {
            return String::new();
        }
        let buffer = terminal.backend().buffer().clone();
        buffer
            .content()
            .chunks(usize::from(width).max(1))
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

/// Map a terminal key to the wizard's.
fn key_of(event: &crossterm::event::KeyEvent) -> Option<Key> {
    use crossterm::event::{KeyCode, KeyEventKind, KeyModifiers};
    if event.kind == KeyEventKind::Release {
        return None;
    }
    if event.modifiers.contains(KeyModifiers::CONTROL) {
        return matches!(event.code, KeyCode::Char('c' | 'd')).then_some(Key::Quit);
    }
    Some(match event.code {
        KeyCode::Up => Key::Up,
        KeyCode::Down => Key::Down,
        KeyCode::PageUp => Key::PageUp,
        KeyCode::PageDown => Key::PageDown,
        KeyCode::Home => Key::Home,
        KeyCode::End => Key::End,
        KeyCode::Left => Key::Left,
        KeyCode::Right => Key::Right,
        KeyCode::Tab | KeyCode::BackTab => Key::Tab,
        KeyCode::Char(' ') => Key::Space,
        KeyCode::Char(c) => Key::Char(c),
        KeyCode::Backspace => Key::Backspace,
        KeyCode::Enter => Key::Enter,
        KeyCode::Esc => Key::Back,
        _ => return None,
    })
}

/// Run the wizard on the terminal; the terminal is given back whatever
/// happens. `summarize` makes the summary from the answers.
pub fn run_on_terminal(
    wizard: &mut Wizard,
    summarize: &mut dyn FnMut(&[Answer]) -> Summary,
) -> std::io::Result<Outcome> {
    let mut terminal = ratatui::try_init()?;
    let result = (|| loop {
        if wizard.needs_summary() {
            terminal.draw(|frame| wizard.render(frame))?;
            let summary = summarize(&wizard.answers());
            wizard.set_summary(summary);
        }
        terminal.draw(|frame| wizard.render(frame))?;
        if let Some(outcome) = wizard.outcome() {
            return Ok(outcome);
        }
        if let crossterm::event::Event::Key(event) = crossterm::event::read()?
            && let Some(key) = key_of(&event)
        {
            wizard.handle(key);
        }
    })();
    ratatui::restore();
    result
}

/// The wizard asked by number, for a console that cannot draw: each step in
/// turn (Enter alone keeps what is ticked), then the summary and "Install?".
pub fn run_numbered<C: Console + ?Sized>(
    console: &mut C,
    wizard: &mut Wizard,
    summarize: &mut dyn FnMut(&[Answer]) -> Summary,
) -> Outcome {
    for index in 0..wizard.steps.len() {
        let step = &mut wizard.steps[index];
        console.out(&format!("\n{}\n{}\n", step.title, step.note));
        match &mut step.kind {
            Kind::Pick {
                multiple,
                items,
                ticked,
                cursor,
            } => {
                let width = items.iter().map(|item| item.value.chars().count()).max().unwrap_or(0);
                let mut text = String::new();
                for (i, item) in items.iter().enumerate() {
                    let mark = if (*multiple && ticked[i]) || (!*multiple && i == *cursor) {
                        "*"
                    } else {
                        " "
                    };
                    text.push_str(&format!(
                        "  {mark}{:>2}) {:<width$}  {}\n",
                        i + 1,
                        item.value,
                        item.detail
                    ));
                }
                console.out(&text);
                let prompt = if *multiple {
                    "Choose one or more (numbers, e.g. 1,3; enter keeps the ones marked *): "
                } else {
                    "Choose one (enter keeps the one marked *): "
                };
                loop {
                    let Some(answer) = console.ask(prompt) else {
                        return Outcome::Cancelled;
                    };
                    if answer.trim().is_empty() {
                        if !*multiple || ticked.iter().any(|mark| *mark) {
                            break;
                        }
                        console.out("Please choose at least one.\n");
                        continue;
                    }
                    let numbers: Option<Vec<usize>> = answer
                        .split(',')
                        .map(|part| {
                            part.trim()
                                .parse::<usize>()
                                .ok()
                                .filter(|n| (1..=items.len()).contains(n))
                        })
                        .collect();
                    match numbers {
                        Some(numbers) if !numbers.is_empty() && (*multiple || numbers.len() == 1) => {
                            for mark in ticked.iter_mut() {
                                *mark = false;
                            }
                            for n in &numbers {
                                ticked[n - 1] = true;
                            }
                            *cursor = numbers[0] - 1;
                            break;
                        }
                        _ => console.out("Please enter a number from the list.\n"),
                    }
                }
            }
            Kind::Text { value } => loop {
                let Some(answer) = console.ask(&format!("{}: ", step.title)) else {
                    return Outcome::Cancelled;
                };
                if !answer.trim().is_empty() {
                    *value = answer.trim().to_owned();
                    break;
                }
            },
        }
    }
    if !wizard.with_summary {
        return Outcome::Done(wizard.answers());
    }
    let summary = summarize(&wizard.answers());
    let mut text = String::from("\nSummary\n");
    for problem in &summary.problems {
        text.push_str(&format!("! {problem}\n"));
    }
    for (heading, items) in &summary.sections {
        text.push_str(&format!("{heading}\n"));
        for item in items {
            text.push_str(&format!("  {item}\n"));
        }
    }
    console.out(&text);
    if !summary.problems.is_empty() {
        return Outcome::Cancelled;
    }
    loop {
        let Some(answer) = console.ask("Install? [y/n] (n): ") else {
            return Outcome::Cancelled;
        };
        match answer.trim().to_lowercase().as_str() {
            "y" | "yes" => return Outcome::Done(wizard.answers()),
            "" | "n" | "no" => return Outcome::Cancelled,
            _ => console.out("Please enter Y or N\n"),
        }
    }
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
