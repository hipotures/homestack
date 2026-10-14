//! What the client asks of the desktop: local commands, the clock, and the
//! terminal. Tests replace both traits.

use std::io::{self, BufRead, IsTerminal, Write};
use std::os::unix::fs::PermissionsExt;
use std::path::Path;
use std::process::{Command, Stdio};
use std::time::Instant;

/// A finished local command.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Output {
    pub code: i32,
    pub stdout: String,
    pub stderr: String,
}

impl Output {
    /// Exit 0 with `stdout`.
    #[must_use]
    pub fn stdout(text: &str) -> Self {
        Self {
            code: 0,
            stdout: text.to_owned(),
            stderr: String::new(),
        }
    }

    /// Exit `code` with `stderr`.
    #[must_use]
    pub fn failed(code: i32, stderr: &str) -> Self {
        Self {
            code,
            stdout: String::new(),
            stderr: stderr.to_owned(),
        }
    }
}

/// Local commands and the monotonic clock.
pub trait System: Send + Sync {
    /// `shutil.which`: the program's path, if it can be run.
    fn which(&self, name: &str) -> Option<String>;
    /// Run `argv` with no input; its output captured.
    fn run(&self, argv: &[String]) -> io::Result<Output>;
    /// Run an SSH master (`ssh -fN`): its stdout dropped, its stderr kept in
    /// a file rather than a pipe, since the detached master keeps the
    /// descriptor open.
    fn run_master(&self, argv: &[String]) -> io::Result<Output>;
    /// Seconds on a monotonic clock.
    fn monotonic(&self) -> f64;
}

/// The real desktop.
pub struct RealSystem {
    started: Instant,
}

impl Default for RealSystem {
    fn default() -> Self {
        Self {
            started: Instant::now(),
        }
    }
}

fn executable(path: &Path) -> bool {
    path.metadata()
        .is_ok_and(|meta| meta.is_file() && meta.permissions().mode() & 0o111 != 0)
}

/// `shutil.which` on `PATH`; a name with a slash is checked as it is.
#[must_use]
pub fn which(name: &str) -> Option<String> {
    if name.contains('/') {
        return executable(Path::new(name)).then(|| name.to_owned());
    }
    let path = std::env::var_os("PATH")?;
    std::env::split_paths(&path)
        .map(|dir| dir.join(name))
        .find(|candidate| executable(candidate))
        .map(|found| found.display().to_string())
}

fn code(status: std::process::ExitStatus) -> i32 {
    status.code().unwrap_or(-1)
}

impl System for RealSystem {
    fn which(&self, name: &str) -> Option<String> {
        which(name)
    }

    fn run(&self, argv: &[String]) -> io::Result<Output> {
        let (program, args) = argv.split_first().ok_or_else(|| io::Error::other("empty command"))?;
        let output = Command::new(program).args(args).stdin(Stdio::null()).output()?;
        Ok(Output {
            code: code(output.status),
            stdout: String::from_utf8_lossy(&output.stdout).into_owned(),
            stderr: String::from_utf8_lossy(&output.stderr).into_owned(),
        })
    }

    fn run_master(&self, argv: &[String]) -> io::Result<Output> {
        let (program, args) = argv.split_first().ok_or_else(|| io::Error::other("empty command"))?;
        let mut errors = tempfile::tempfile()?;
        let status = Command::new(program)
            .args(args)
            .stdout(Stdio::null())
            .stderr(errors.try_clone()?)
            .status()?;
        let mut stderr = String::new();
        io::Seek::rewind(&mut errors)?;
        io::Read::read_to_string(&mut errors, &mut stderr)?;
        Ok(Output {
            code: code(status),
            stdout: String::new(),
            stderr,
        })
    }

    fn monotonic(&self) -> f64 {
        self.started.elapsed().as_secs_f64()
    }
}

/// One entry of a list to choose from.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Entry {
    pub value: String,
    pub detail: String,
    /// Listed only to show it exists: it cannot be chosen.
    pub blocked: bool,
}

impl Entry {
    #[must_use]
    pub fn new(value: &str, detail: &str) -> Self {
        Self {
            value: value.to_owned(),
            detail: detail.to_owned(),
            blocked: false,
        }
    }

    #[must_use]
    pub fn blocked(value: &str, detail: &str) -> Self {
        Self {
            blocked: true,
            ..Self::new(value, detail)
        }
    }
}

/// The terminal: output, the questions of a confirmation and a hidden
/// passphrase.
pub trait Console: Send {
    fn out(&mut self, text: &str);
    fn err(&mut self, text: &str);
    /// Show `question` and read one line; `None` at end of input.
    fn ask(&mut self, question: &str) -> Option<String>;
    /// Read a line without echo; `None` when that is not possible.
    fn secret(&mut self, prompt: &str) -> Option<String>;

    /// Choose among `items` (a value and what tells it apart): the indexes
    /// of the chosen ones, `None` when the user gives up. The default asks
    /// for numbers, for a console that cannot draw a list.
    fn pick(&mut self, title: &str, description: &str, multiple: bool, items: &[Entry]) -> Option<Vec<usize>> {
        pick_numbered(self, title, description, multiple, items)
    }
}

/// stdout, stderr and stdin.
#[derive(Default)]
pub struct Terminal;

impl Console for Terminal {
    fn out(&mut self, text: &str) {
        let mut stdout = io::stdout().lock();
        let _ = stdout.write_all(text.as_bytes());
        let _ = stdout.flush();
    }

    fn err(&mut self, text: &str) {
        let _ = io::stderr().lock().write_all(text.as_bytes());
    }

    fn ask(&mut self, question: &str) -> Option<String> {
        self.out(question);
        let mut line = String::new();
        match io::stdin().lock().read_line(&mut line) {
            Ok(0) | Err(_) => None,
            Ok(_) => Some(line.trim_end_matches(['\n', '\r']).to_owned()),
        }
    }

    fn secret(&mut self, prompt: &str) -> Option<String> {
        if !io::stdin().is_terminal() {
            return None;
        }
        let _ = io::stderr().lock().write_all(prompt.as_bytes());
        rpassword::read_password().ok()
    }

    fn pick(&mut self, title: &str, description: &str, multiple: bool, items: &[Entry]) -> Option<Vec<usize>> {
        use crate::picker::{Item, Outcome, Picker, run_on_terminal};
        if io::stdin().is_terminal() && io::stdout().is_terminal() {
            let rows = items
                .iter()
                .map(|entry| Item {
                    value: entry.value.clone(),
                    detail: entry.detail.clone(),
                    blocked: entry.blocked,
                })
                .collect();
            let mut picker = Picker::new(title, multiple, rows).with_description(description);
            if let Ok(outcome) = run_on_terminal(&mut picker) {
                return match outcome {
                    Outcome::Chosen(chosen) => Some(chosen),
                    Outcome::Cancelled => None,
                };
            }
        }
        pick_numbered(self, title, description, multiple, items)
    }
}

/// [`Console::pick`] with numbers: the list, one choice a line, then
/// "Choose ...:" until the answer is valid; end of input gives up. Only
/// what can be chosen is numbered; the rest is listed with its reason.
pub fn pick_numbered<C: Console + ?Sized>(
    console: &mut C,
    title: &str,
    description: &str,
    multiple: bool,
    items: &[Entry],
) -> Option<Vec<usize>> {
    let width = items.iter().map(|item| item.value.chars().count()).max().unwrap_or(0);
    let mut text = format!("\n{title}\n{description}\n");
    // The index in `items` of each number the user can type.
    let mut numbered: Vec<usize> = Vec::new();
    for (index, item) in items.iter().enumerate() {
        if item.blocked {
            continue;
        }
        numbered.push(index);
        text.push_str(&format!(
            "  {:>2}) {:<width$}  {}\n",
            numbered.len(),
            item.value,
            item.detail
        ));
    }
    for item in items.iter().filter(|item| item.blocked) {
        text.push_str(&format!("   -  {:<width$}  {}\n", item.value, item.detail));
    }
    console.out(&text);
    let count = numbered.len();
    let prompt = if multiple {
        format!("Choose one or more (numbers, e.g. 1,3) [1-{count}]: ")
    } else {
        format!("Choose one [1-{count}]: ")
    };
    loop {
        let answer = console.ask(&prompt)?;
        let numbers: Option<Vec<usize>> = answer
            .split(',')
            .map(|part| part.trim().parse::<usize>().ok().filter(|n| (1..=count).contains(n)))
            .collect();
        match numbers {
            Some(mut numbers) if !numbers.is_empty() && (multiple || numbers.len() == 1) => {
                numbers.sort_unstable();
                numbers.dedup();
                return Some(numbers.into_iter().map(|n| numbered[n - 1]).collect());
            }
            _ => console.out("Please enter a number from the list.\n"),
        }
    }
}
