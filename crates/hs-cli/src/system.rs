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
    /// Run `argv` with `input` on its stdin; its output captured.
    fn run_input(&self, argv: &[String], _input: &str) -> io::Result<Output> {
        Err(io::Error::other(format!(
            "cannot run {:?} with input here",
            argv.first()
        )))
    }
    /// Run `argv` on the terminal (stdin, stdout and stderr inherited); its
    /// exit code.
    fn run_interactive(&self, argv: &[String]) -> io::Result<i32> {
        Err(io::Error::other(format!(
            "cannot run {:?} on the terminal here",
            argv.first()
        )))
    }
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

    fn run_input(&self, argv: &[String], input: &str) -> io::Result<Output> {
        let (program, args) = argv.split_first().ok_or_else(|| io::Error::other("empty command"))?;
        let mut child = Command::new(program)
            .args(args)
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .spawn()?;
        let mut stdin = child.stdin.take().ok_or_else(|| io::Error::other("no stdin"))?;
        let input = input.to_owned();
        // Written from a thread, so a large input cannot block on a full
        // output pipe.
        let writer = std::thread::spawn(move || stdin.write_all(input.as_bytes()));
        let output = child.wait_with_output()?;
        let _ = writer.join();
        Ok(Output {
            code: code(output.status),
            stdout: String::from_utf8_lossy(&output.stdout).into_owned(),
            stderr: String::from_utf8_lossy(&output.stderr).into_owned(),
        })
    }

    fn run_interactive(&self, argv: &[String]) -> io::Result<i32> {
        let (program, args) = argv.split_first().ok_or_else(|| io::Error::other("empty command"))?;
        Ok(code(Command::new(program).args(args).status()?))
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

    /// Run the installer's wizard: the steps, then the summary `summarize`
    /// makes from the answers. The default asks by number, for a console
    /// that cannot draw.
    fn wizard(
        &mut self,
        wizard: &mut crate::wizard::Wizard,
        summarize: &mut dyn FnMut(&[crate::wizard::Answer]) -> crate::wizard::Summary,
    ) -> crate::wizard::Outcome {
        crate::wizard::run_numbered(self, wizard, summarize)
    }

    /// Run the installer's form; the default asks by number.
    fn form(
        &mut self,
        form: &mut crate::form::Form,
        summarize: &mut dyn FnMut(&[crate::wizard::Answer]) -> crate::wizard::Summary,
    ) -> crate::form::Outcome {
        crate::form::run_numbered(self, form, summarize)
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

    fn wizard(
        &mut self,
        wizard: &mut crate::wizard::Wizard,
        summarize: &mut dyn FnMut(&[crate::wizard::Answer]) -> crate::wizard::Summary,
    ) -> crate::wizard::Outcome {
        if io::stdin().is_terminal()
            && io::stdout().is_terminal()
            && let Ok(outcome) = crate::wizard::run_on_terminal(wizard, summarize)
        {
            return outcome;
        }
        crate::wizard::run_numbered(self, wizard, summarize)
    }

    fn form(
        &mut self,
        form: &mut crate::form::Form,
        summarize: &mut dyn FnMut(&[crate::wizard::Answer]) -> crate::wizard::Summary,
    ) -> crate::form::Outcome {
        if io::stdin().is_terminal()
            && io::stdout().is_terminal()
            && let Ok(outcome) = crate::form::run_on_terminal(form, summarize)
        {
            return outcome;
        }
        crate::form::run_numbered(self, form, summarize)
    }
}
