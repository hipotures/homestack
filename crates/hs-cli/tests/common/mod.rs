//! Test doubles shared by the client's tests: a scripted [`System`] (the
//! local commands and the clock) and a recorded console.

#![allow(dead_code)]

use std::collections::VecDeque;
use std::io;
use std::sync::{Arc, Mutex, PoisonError};

use hs_cli::system::{Console, Output, System};

pub type TestResult = Result<(), Box<dyn std::error::Error>>;

pub type RunFn = Box<dyn Fn(&[String]) -> io::Result<Output> + Send + Sync>;
pub type WhichFn = Box<dyn Fn(&str) -> Option<String> + Send + Sync>;

/// Local commands answered by closures, every call recorded.
pub struct FakeSystem {
    pub which: WhichFn,
    pub run: RunFn,
    pub master: RunFn,
    pub clock: Mutex<VecDeque<f64>>,
    pub runs: Mutex<Vec<Vec<String>>>,
    pub masters: Mutex<Vec<Vec<String>>>,
}

impl FakeSystem {
    /// Every tool present at `/usr/bin/<name>`, every command exits 0.
    pub fn new() -> Self {
        Self {
            which: Box::new(|name| Some(format!("/usr/bin/{name}"))),
            run: Box::new(|_| Ok(Output::default())),
            master: Box::new(|_| Ok(Output::default())),
            clock: Mutex::new(VecDeque::new()),
            runs: Mutex::new(Vec::new()),
            masters: Mutex::new(Vec::new()),
        }
    }

    pub fn which(mut self, which: impl Fn(&str) -> Option<String> + Send + Sync + 'static) -> Self {
        self.which = Box::new(which);
        self
    }

    pub fn run(mut self, run: impl Fn(&[String]) -> io::Result<Output> + Send + Sync + 'static) -> Self {
        self.run = Box::new(run);
        self
    }

    pub fn master(mut self, master: impl Fn(&[String]) -> io::Result<Output> + Send + Sync + 'static) -> Self {
        self.master = Box::new(master);
        self
    }

    pub fn clock(self, ticks: &[f64]) -> Self {
        *self.clock.lock().unwrap_or_else(PoisonError::into_inner) = ticks.iter().copied().collect();
        self
    }

    pub fn runs(&self) -> Vec<Vec<String>> {
        self.runs.lock().unwrap_or_else(PoisonError::into_inner).clone()
    }

    pub fn masters(&self) -> Vec<Vec<String>> {
        self.masters.lock().unwrap_or_else(PoisonError::into_inner).clone()
    }
}

impl System for FakeSystem {
    fn which(&self, name: &str) -> Option<String> {
        (self.which)(name)
    }

    fn run(&self, argv: &[String]) -> io::Result<Output> {
        self.runs
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .push(argv.to_vec());
        (self.run)(argv)
    }

    fn run_master(&self, argv: &[String]) -> io::Result<Output> {
        self.masters
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .push(argv.to_vec());
        (self.master)(argv)
    }

    fn monotonic(&self) -> f64 {
        self.clock
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .pop_front()
            .unwrap_or(0.0)
    }
}

/// Output kept for the test; answers given in order.
#[derive(Clone, Default)]
pub struct Recorded {
    inner: Arc<Mutex<RecordedInner>>,
}

#[derive(Default)]
struct RecordedInner {
    out: String,
    err: String,
    answers: VecDeque<String>,
    secrets: VecDeque<String>,
    questions: Vec<String>,
}

impl Recorded {
    pub fn answering(answers: &[&str]) -> Self {
        let recorded = Self::default();
        recorded.lock().answers = answers.iter().map(|a| (*a).to_owned()).collect();
        recorded
    }

    pub fn with_secret(self, secret: &str) -> Self {
        self.lock().secrets.push_back(secret.to_owned());
        self
    }

    fn lock(&self) -> std::sync::MutexGuard<'_, RecordedInner> {
        self.inner.lock().unwrap_or_else(PoisonError::into_inner)
    }

    pub fn out(&self) -> String {
        self.lock().out.clone()
    }

    pub fn err(&self) -> String {
        self.lock().err.clone()
    }

    pub fn questions(&self) -> Vec<String> {
        self.lock().questions.clone()
    }
}

impl Console for Recorded {
    fn out(&mut self, text: &str) {
        self.lock().out.push_str(text);
    }

    fn err(&mut self, text: &str) {
        self.lock().err.push_str(text);
    }

    fn ask(&mut self, question: &str) -> Option<String> {
        let mut inner = self.lock();
        inner.questions.push(question.to_owned());
        inner.answers.pop_front()
    }

    fn secret(&mut self, prompt: &str) -> Option<String> {
        let mut inner = self.lock();
        inner.questions.push(prompt.to_owned());
        inner.secrets.pop_front()
    }
}

pub fn strings(items: &[&str]) -> Vec<String> {
    items.iter().map(|item| (*item).to_owned()).collect()
}
