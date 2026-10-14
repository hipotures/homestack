//! The installer's form (ratatui): one screen with every setting and its
//! value, Gold included, and the buttons Install and Cancel. Enter on a
//! setting opens its list of choices; Install opens the confirmation with
//! every change and the buttons Install and Back; Esc there returns to the
//! form. Nothing is changed before the second Install.
//!
//! The state and the drawing are separate from the terminal, so they are
//! tested on a buffer; where there is no terminal to draw on, the same form is
//! asked by number ([`run_numbered`]).

use ratatui::Frame;
use ratatui::backend::TestBackend;
use ratatui::layout::{Constraint, Flex, Layout, Rect};
use ratatui::style::{Color, Modifier, Style};
use ratatui::text::{Line, Span};
use ratatui::widgets::{
    Block, BorderType, Borders, Clear, List, ListItem, ListState, Paragraph, Scrollbar, ScrollbarOrientation,
    ScrollbarState, Wrap,
};

use crate::system::Console;
use crate::wizard::{Answer, Item, Key, Summary};

/// One setting that can be changed: its choices and what is chosen.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Choice {
    pub note: String,
    pub multiple: bool,
    pub items: Vec<Item>,
    pub ticked: Vec<bool>,
    pub cursor: usize,
}

/// One row of the form.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Field {
    pub label: String,
    /// `None`: shown only (given on the command line, or how root is reached).
    pub choice: Option<Choice>,
    /// The value of a row that cannot be changed.
    pub fixed: String,
}

impl Field {
    #[must_use]
    pub fn fixed(label: &str, value: &str) -> Self {
        Self {
            label: label.to_owned(),
            choice: None,
            fixed: value.to_owned(),
        }
    }

    /// A setting; `ticked` are chosen at first.
    #[must_use]
    pub fn choose(label: &str, note: &str, multiple: bool, items: Vec<Item>, ticked: &[usize]) -> Self {
        let mut marks = vec![false; items.len()];
        for index in ticked.iter().take(if multiple { usize::MAX } else { 1 }) {
            if let Some(mark) = marks.get_mut(*index) {
                *mark = true;
            }
        }
        let cursor = ticked.first().copied().filter(|i| *i < items.len()).unwrap_or(0);
        Self {
            label: label.to_owned(),
            choice: Some(Choice {
                note: note.to_owned(),
                multiple,
                items,
                ticked: marks,
                cursor,
            }),
            fixed: String::new(),
        }
    }

    /// What the row shows as its value.
    #[must_use]
    pub fn value(&self) -> String {
        match &self.choice {
            None => self.fixed.clone(),
            Some(choice) => {
                let chosen: Vec<&Item> = choice
                    .items
                    .iter()
                    .enumerate()
                    .filter(|(i, _)| choice.ticked[*i])
                    .map(|(_, item)| item)
                    .collect();
                match chosen.as_slice() {
                    [] => "(none chosen)".to_owned(),
                    [one] if !choice.multiple => {
                        let name = one.detail.split_whitespace().next().unwrap_or_default();
                        if one.value.chars().all(|c| c.is_ascii_digit()) && !name.is_empty() {
                            format!("{} {name}", one.value)
                        } else {
                            one.value.clone()
                        }
                    }
                    many => many
                        .iter()
                        .map(|item| item.value.as_str())
                        .collect::<Vec<_>>()
                        .join(", "),
                }
            }
        }
    }

    fn answer(&self) -> Option<Answer> {
        self.choice
            .as_ref()
            .map(|choice| Answer::Picked((0..choice.ticked.len()).filter(|i| choice.ticked[*i]).collect()))
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Focus {
    Field(usize),
    Install,
    Cancel,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Outcome {
    /// The answers of the settings that can be changed, in form order.
    Install(Vec<Answer>),
    Cancelled,
}

pub struct Form {
    title: String,
    fields: Vec<Field>,
    focus: Focus,
    /// The list of the field being changed, and the field as it was.
    open: Option<(usize, Field)>,
    /// The confirmation: `Some(None)` while it is being made.
    confirm: Option<Option<Summary>>,
    confirm_install: bool,
    scroll: u16,
    /// How far the confirmation can scroll, as last drawn.
    max_scroll: std::cell::Cell<u16>,
    outcome: Option<Outcome>,
    list: ListState,
}

impl Form {
    #[must_use]
    pub fn new(title: &str, fields: Vec<Field>) -> Self {
        let focus = fields
            .iter()
            .position(|field| field.choice.is_some())
            .map_or(Focus::Install, Focus::Field);
        Self {
            title: title.to_owned(),
            fields,
            focus,
            open: None,
            confirm: None,
            confirm_install: true,
            scroll: 0,
            max_scroll: std::cell::Cell::new(u16::MAX),
            outcome: None,
            list: ListState::default(),
        }
    }

    #[must_use]
    pub fn fields(&self) -> &[Field] {
        &self.fields
    }

    #[must_use]
    pub fn outcome(&self) -> Option<Outcome> {
        self.outcome.clone()
    }

    /// The answers of the settings that can be changed.
    #[must_use]
    pub fn answers(&self) -> Vec<Answer> {
        self.fields.iter().filter_map(Field::answer).collect()
    }

    /// The confirmation is open and has to be made from the answers.
    #[must_use]
    pub fn needs_summary(&self) -> bool {
        self.outcome.is_none() && matches!(self.confirm, Some(None))
    }

    pub fn set_summary(&mut self, summary: Summary) {
        self.confirm_install = summary.problems.is_empty();
        self.scroll = 0;
        self.confirm = Some(Some(summary));
    }

    fn editable(&self) -> Vec<usize> {
        (0..self.fields.len())
            .filter(|i| self.fields[*i].choice.is_some())
            .collect()
    }

    fn move_focus(&mut self, down: bool) {
        let mut stops: Vec<Focus> = self.editable().into_iter().map(Focus::Field).collect();
        stops.push(Focus::Install);
        stops.push(Focus::Cancel);
        let at = stops.iter().position(|stop| *stop == self.focus).unwrap_or(0);
        let next = if down {
            (at + 1).min(stops.len() - 1)
        } else {
            at.saturating_sub(1)
        };
        self.focus = stops[next];
    }

    pub fn handle(&mut self, key: Key) {
        if self.outcome.is_some() {
            return;
        }
        if key == Key::Quit {
            self.outcome = Some(Outcome::Cancelled);
            return;
        }
        if self.confirm.is_some() {
            self.handle_confirm(key);
        } else if self.open.is_some() {
            self.handle_list(key);
        } else {
            match key {
                Key::Up => self.move_focus(false),
                Key::Down | Key::Tab => self.move_focus(true),
                Key::Left if self.focus == Focus::Cancel => self.focus = Focus::Install,
                Key::Right if self.focus == Focus::Install => self.focus = Focus::Cancel,
                Key::Enter | Key::Space => match self.focus {
                    Focus::Field(index) => self.open = Some((index, self.fields[index].clone())),
                    Focus::Install => self.confirm = Some(None),
                    Focus::Cancel => self.outcome = Some(Outcome::Cancelled),
                },
                Key::Back => self.outcome = Some(Outcome::Cancelled),
                _ => {}
            }
        }
    }

    fn handle_list(&mut self, key: Key) {
        let Some((index, before)) = self.open.clone() else {
            return;
        };
        let Some(choice) = self.fields[index].choice.as_mut() else {
            self.open = None;
            return;
        };
        let last = choice.items.len().saturating_sub(1);
        match key {
            Key::Up => choice.cursor = choice.cursor.saturating_sub(1),
            Key::Down => choice.cursor = (choice.cursor + 1).min(last),
            Key::PageUp => choice.cursor = choice.cursor.saturating_sub(10),
            Key::PageDown => choice.cursor = (choice.cursor + 10).min(last),
            Key::Home => choice.cursor = 0,
            Key::End => choice.cursor = last,
            Key::Space if choice.multiple => {
                if let Some(mark) = choice.ticked.get_mut(choice.cursor) {
                    *mark = !*mark;
                }
            }
            Key::Space | Key::Enter if !choice.multiple && !choice.items.is_empty() => {
                choice.ticked.iter_mut().for_each(|mark| *mark = false);
                choice.ticked[choice.cursor] = true;
                self.open = None;
            }
            Key::Enter => {
                if !choice.ticked.iter().any(|mark| *mark)
                    && let Some(mark) = choice.ticked.get_mut(choice.cursor)
                {
                    *mark = true;
                }
                self.open = None;
            }
            Key::Back => {
                self.fields[index] = before;
                self.open = None;
            }
            _ => {}
        }
    }

    fn handle_confirm(&mut self, key: Key) {
        let can_install = matches!(&self.confirm, Some(Some(summary)) if summary.problems.is_empty());
        match key {
            Key::Left | Key::Right | Key::Tab => self.confirm_install = can_install && !self.confirm_install,
            Key::Up => self.scroll = self.scroll.saturating_sub(1),
            Key::Down => self.scroll = self.scroll.saturating_add(1).min(self.max_scroll.get()),
            Key::PageUp => self.scroll = self.scroll.saturating_sub(10),
            Key::PageDown => self.scroll = self.scroll.saturating_add(10).min(self.max_scroll.get()),
            Key::Home => self.scroll = 0,
            Key::End => self.scroll = self.max_scroll.get(),
            Key::Enter if matches!(self.confirm, Some(Some(_))) => {
                if self.confirm_install && can_install {
                    self.outcome = Some(Outcome::Install(self.answers()));
                } else {
                    self.confirm = None;
                }
            }
            Key::Back => self.confirm = None,
            _ => {}
        }
    }

    pub fn render(&mut self, frame: &mut Frame) {
        let area = frame.area();
        let [body, hint] = Layout::vertical([Constraint::Min(8), Constraint::Length(1)]).areas(area);
        let block = Block::default()
            .borders(Borders::ALL)
            .border_type(BorderType::Rounded)
            .border_style(Style::default().fg(Color::Cyan))
            .title(Span::styled(
                format!(" {} ", self.title),
                Style::default().fg(Color::Cyan).add_modifier(Modifier::BOLD),
            ));
        let inside = block.inner(body);
        frame.render_widget(block, body);
        let label_width = self.fields.iter().map(|f| f.label.chars().count()).max().unwrap_or(0);
        let mut lines = vec![Line::from("")];
        for (index, field) in self.fields.iter().enumerate() {
            let focused = self.open.is_none() && self.confirm.is_none() && self.focus == Focus::Field(index);
            let value_style = if field.choice.is_none() {
                Style::default().fg(Color::Gray)
            } else if field.value() == "(none chosen)" {
                Style::default().fg(Color::Red)
            } else {
                Style::default().fg(Color::Green)
            };
            let mut row = vec![
                Span::raw(if focused { " \u{25b6} " } else { "   " }),
                Span::styled(
                    format!("{:<label_width$}", field.label),
                    Style::default().add_modifier(Modifier::BOLD),
                ),
                Span::raw("   "),
                Span::styled(field.value(), value_style),
            ];
            if field.choice.is_some() {
                row.push(Span::styled("  [change]", Style::default().fg(Color::DarkGray)));
            }
            let mut line = Line::from(row);
            if focused {
                line = line.style(Style::default().add_modifier(Modifier::REVERSED));
            }
            lines.push(line);
        }
        lines.push(Line::from(""));
        lines.push(buttons(
            "Install",
            "Cancel",
            self.focus == Focus::Install && self.open.is_none() && self.confirm.is_none(),
            self.focus == Focus::Cancel && self.open.is_none() && self.confirm.is_none(),
            true,
        ));
        frame.render_widget(Paragraph::new(lines), inside);
        let keys = if self.confirm.is_some() {
            "\u{2190}/\u{2192} button   enter press   \u{2191}/\u{2193} scroll   esc back to the settings"
        } else if self
            .open
            .as_ref()
            .is_some_and(|(i, _)| self.fields[*i].choice.as_ref().is_some_and(|c| c.multiple))
        {
            "\u{2191}/\u{2193} move   space tick   enter done   esc undo"
        } else if self.open.is_some() {
            "\u{2191}/\u{2193} move   enter choose   esc undo"
        } else {
            "\u{2191}/\u{2193} move   enter change or press   esc cancel"
        };
        frame.render_widget(
            Paragraph::new(Line::styled(keys, Style::default().fg(Color::DarkGray))),
            hint,
        );
        if let Some((index, _)) = self.open {
            self.render_list(frame, body, index);
        }
        if self.confirm.is_some() {
            self.render_confirm(frame, body);
        }
    }

    fn render_list(&mut self, frame: &mut Frame, area: Rect, index: usize) {
        let field = &self.fields[index];
        let Some(choice) = &field.choice else { return };
        let popup = centered(area, 90, 80);
        frame.render_widget(Clear, popup);
        let block = Block::default()
            .borders(Borders::ALL)
            .border_type(BorderType::Rounded)
            .border_style(Style::default().fg(Color::Yellow))
            .title(Span::styled(
                format!(" {} ", field.label),
                Style::default().fg(Color::Yellow).add_modifier(Modifier::BOLD),
            ));
        let inside = block.inner(popup);
        frame.render_widget(block, popup);
        let note_rows = u16::try_from(wrapped_lines(&choice.note, usize::from(inside.width).max(1)) + 1)
            .unwrap_or(u16::MAX)
            .min(inside.height.saturating_sub(3));
        let [note, body] = Layout::vertical([Constraint::Length(note_rows), Constraint::Min(1)]).areas(inside);
        frame.render_widget(
            Paragraph::new(choice.note.as_str())
                .style(Style::default().fg(Color::Gray))
                .wrap(Wrap { trim: true }),
            note,
        );
        let width = choice
            .items
            .iter()
            .map(|item| item.value.chars().count())
            .max()
            .unwrap_or(0);
        let rows: Vec<ListItem> = choice
            .items
            .iter()
            .enumerate()
            .map(|(i, item)| {
                let mark = match (choice.multiple, choice.ticked[i]) {
                    (true, true) => "[x]",
                    (true, false) => "[ ]",
                    (false, true) => "(\u{2022})",
                    (false, false) => "( )",
                };
                ListItem::new(Line::from(vec![
                    Span::styled(
                        format!("{mark} "),
                        Style::default().fg(if choice.ticked[i] { Color::Green } else { Color::Gray }),
                    ),
                    Span::styled(
                        format!("{:<width$}", item.value),
                        Style::default().add_modifier(Modifier::BOLD),
                    ),
                    Span::raw("  "),
                    Span::styled(item.detail.clone(), Style::default().fg(Color::Gray)),
                ]))
            })
            .collect();
        self.list.select(Some(choice.cursor));
        let list = List::new(rows)
            .highlight_symbol("\u{25b6} ")
            .highlight_style(Style::default().add_modifier(Modifier::REVERSED));
        frame.render_stateful_widget(list, body, &mut self.list);
    }

    fn render_confirm(&self, frame: &mut Frame, area: Rect) {
        let popup = centered(area, 94, 90);
        frame.render_widget(Clear, popup);
        let block = Block::default()
            .borders(Borders::ALL)
            .border_type(BorderType::Rounded)
            .border_style(Style::default().fg(Color::Yellow))
            .title(Span::styled(
                " Confirm the installation ",
                Style::default().fg(Color::Yellow).add_modifier(Modifier::BOLD),
            ));
        let inside = block.inner(popup);
        frame.render_widget(block, popup);
        let [text, gap, row] =
            Layout::vertical([Constraint::Min(1), Constraint::Length(1), Constraint::Length(1)]).areas(inside);
        let _ = gap;
        let Some(Some(summary)) = &self.confirm else {
            frame.render_widget(Paragraph::new("Checking the cluster..."), text);
            return;
        };
        let mut lines: Vec<Line> = Vec::new();
        for problem in &summary.problems {
            lines.push(Line::styled(
                format!("\u{2717} {problem}"),
                Style::default().fg(Color::Red).add_modifier(Modifier::BOLD),
            ));
        }
        if !summary.problems.is_empty() {
            lines.push(Line::from(""));
        }
        for (heading, items) in &summary.sections {
            lines.push(Line::styled(
                heading.clone(),
                Style::default().fg(Color::Cyan).add_modifier(Modifier::BOLD),
            ));
            for item in items {
                let bullet = if item.starts_with(' ') { "    " } else { "  \u{2022} " };
                lines.push(Line::from(format!("{bullet}{item}")));
            }
            lines.push(Line::from(""));
        }
        // The rows the text takes once wrapped, so that scrolling stops at
        // its end and the scrollbar shows where the view is.
        let width = usize::from(text.width.saturating_sub(1)).max(1);
        let rows: usize = lines
            .iter()
            .map(|line| {
                let content: String = line.spans.iter().map(|span| span.content.as_ref()).collect();
                if content.trim().is_empty() {
                    1
                } else {
                    wrapped_lines(&content, width)
                }
            })
            .sum();
        let view = usize::from(text.height);
        let max_scroll = u16::try_from(rows.saturating_sub(view)).unwrap_or(u16::MAX);
        self.max_scroll.set(max_scroll);
        let scroll = self.scroll.min(max_scroll);
        let [words, bar] = Layout::horizontal([Constraint::Min(1), Constraint::Length(1)]).areas(text);
        frame.render_widget(
            Paragraph::new(lines).wrap(Wrap { trim: false }).scroll((scroll, 0)),
            words,
        );
        if rows > view {
            let mut state = ScrollbarState::new(rows.saturating_sub(view)).position(usize::from(scroll));
            frame.render_stateful_widget(Scrollbar::new(ScrollbarOrientation::VerticalRight), bar, &mut state);
        }
        let can_install = summary.problems.is_empty();
        frame.render_widget(
            Paragraph::new(buttons(
                "Install",
                "Back",
                self.confirm_install,
                !self.confirm_install,
                can_install,
            )),
            row,
        );
    }

    /// The form drawn on a `width` x `height` buffer, as text.
    #[must_use]
    pub fn render_to_string(&mut self, width: u16, height: u16) -> String {
        let Ok(mut terminal) = ratatui::Terminal::new(TestBackend::new(width, height));
        if terminal.draw(|frame| self.render(frame)).is_err() {
            return String::new();
        }
        terminal
            .backend()
            .buffer()
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

fn buttons(first: &str, second: &str, first_focused: bool, second_focused: bool, first_enabled: bool) -> Line<'static> {
    let style = |focused: bool, enabled: bool, color: Color| {
        if !enabled {
            Style::default().fg(Color::DarkGray)
        } else if focused {
            Style::default().fg(Color::Black).bg(color).add_modifier(Modifier::BOLD)
        } else {
            Style::default().fg(color)
        }
    };
    Line::from(vec![
        Span::raw("   "),
        Span::styled(
            format!("[ {first} ]"),
            style(first_focused, first_enabled, Color::Green),
        ),
        Span::raw("     "),
        Span::styled(format!("[ {second} ]"), style(second_focused, true, Color::White)),
    ])
}

/// A rectangle of `width`% and `height`% of `area`, in its middle.
fn centered(area: Rect, width: u16, height: u16) -> Rect {
    let [row] = Layout::vertical([Constraint::Percentage(height)])
        .flex(Flex::Center)
        .areas(area);
    let [cell] = Layout::horizontal([Constraint::Percentage(width)])
        .flex(Flex::Center)
        .areas(row);
    cell
}

/// Map a terminal key to the form's.
fn key_of(event: &crossterm::event::KeyEvent) -> Option<Key> {
    use crossterm::event::{KeyCode, KeyEventKind, KeyModifiers};
    if event.kind == KeyEventKind::Release {
        return None;
    }
    if event.modifiers.contains(KeyModifiers::CONTROL) {
        return matches!(event.code, KeyCode::Char('c' | 'd')).then_some(Key::Quit);
    }
    Some(match event.code {
        KeyCode::Up | KeyCode::Char('k') => Key::Up,
        KeyCode::Down | KeyCode::Char('j') => Key::Down,
        KeyCode::PageUp => Key::PageUp,
        KeyCode::PageDown => Key::PageDown,
        KeyCode::Home => Key::Home,
        KeyCode::End => Key::End,
        KeyCode::Left | KeyCode::Char('h') => Key::Left,
        KeyCode::Right | KeyCode::Char('l') => Key::Right,
        KeyCode::Tab | KeyCode::BackTab => Key::Tab,
        KeyCode::Char(' ') => Key::Space,
        KeyCode::Enter => Key::Enter,
        KeyCode::Esc => Key::Back,
        _ => return None,
    })
}

/// Run the form on the terminal; the terminal is given back whatever
/// happens. `summarize` makes the confirmation from the answers.
pub fn run_on_terminal(form: &mut Form, summarize: &mut dyn FnMut(&[Answer]) -> Summary) -> std::io::Result<Outcome> {
    let mut terminal = ratatui::try_init()?;
    let result = (|| loop {
        if form.needs_summary() {
            terminal.draw(|frame| form.render(frame))?;
            let summary = summarize(&form.answers());
            form.set_summary(summary);
        }
        terminal.draw(|frame| form.render(frame))?;
        if let Some(outcome) = form.outcome() {
            return Ok(outcome);
        }
        if let crossterm::event::Event::Key(event) = crossterm::event::read()?
            && let Some(key) = key_of(&event)
        {
            form.handle(key);
        }
    })();
    ratatui::restore();
    result
}

/// The form asked by number, for a console that cannot draw: every setting
/// with its value; a setting's number changes it (Enter installs), then the
/// confirmation and "Install?".
pub fn run_numbered<C: Console + ?Sized>(
    console: &mut C,
    form: &mut Form,
    summarize: &mut dyn FnMut(&[Answer]) -> Summary,
) -> Outcome {
    loop {
        let editable = form.editable();
        let mut text = format!("\n{}\n", form.title);
        for field in &form.fields {
            let number = editable
                .iter()
                .position(|i| form.fields[*i].label == field.label)
                .map_or("  ".to_owned(), |n| format!("{:>2}", n + 1));
            text.push_str(&format!("  {number}  {:<20} {}\n", field.label, field.value()));
        }
        console.out(&text);
        let Some(answer) = console.ask("Number to change, or enter to install: ") else {
            return Outcome::Cancelled;
        };
        let answer = answer.trim();
        if answer.is_empty() {
            break;
        }
        let Some(index) = answer
            .parse::<usize>()
            .ok()
            .and_then(|n| n.checked_sub(1))
            .and_then(|n| editable.get(n).copied())
        else {
            console.out("Please enter a number from the list.\n");
            continue;
        };
        let label = form.fields[index].label.clone();
        let Some(choice) = form.fields[index].choice.as_mut() else {
            continue;
        };
        let mut list = format!("\n{label}\n{}\n", choice.note);
        for (i, item) in choice.items.iter().enumerate() {
            let mark = if choice.ticked[i] { "*" } else { " " };
            list.push_str(&format!("  {mark}{:>2}) {}  {}\n", i + 1, item.value, item.detail));
        }
        console.out(&list);
        let prompt = if choice.multiple {
            "Choose one or more (numbers, e.g. 1,3): "
        } else {
            "Choose one: "
        };
        let Some(picked) = console.ask(prompt) else {
            return Outcome::Cancelled;
        };
        let numbers: Option<Vec<usize>> = picked
            .split(',')
            .map(|part| {
                part.trim()
                    .parse::<usize>()
                    .ok()
                    .filter(|n| (1..=choice.items.len()).contains(n))
            })
            .collect();
        match numbers {
            Some(numbers) if !numbers.is_empty() && (choice.multiple || numbers.len() == 1) => {
                choice.ticked.iter_mut().for_each(|mark| *mark = false);
                for n in &numbers {
                    choice.ticked[n - 1] = true;
                }
                choice.cursor = numbers[0] - 1;
            }
            _ => console.out("Please enter numbers from the list.\n"),
        }
    }
    let summary = summarize(&form.answers());
    let mut text = String::from("\nConfirm the installation\n");
    for problem in &summary.problems {
        text.push_str(&format!("! {problem}\n"));
    }
    for (heading, items) in &summary.sections {
        text.push_str(&format!("{heading}\n"));
        for item in items {
            let bullet = if item.starts_with(' ') { "    " } else { "  - " };
            text.push_str(&format!("{bullet}{item}\n"));
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
            "y" | "yes" => return Outcome::Install(form.answers()),
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
