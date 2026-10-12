//! The configuration corpus in `parity/config-corpus.toml`: edits of a base
//! Python `config.toml` and the verdict both implementations must reach.

use serde::Deserialize;

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Corpus {
    pub phase: String,
    /// Base configuration, relative to the repository root.
    pub base: String,
    #[serde(rename = "case")]
    pub cases: Vec<Case>,
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Case {
    pub name: String,
    /// Values to set, by dotted path (`repo.owner`).
    #[serde(default)]
    pub set: toml::Table,
    /// Dotted paths to delete.
    #[serde(default)]
    pub remove: Vec<String>,
    /// Raw text appended to the edited document.
    #[serde(default)]
    pub append: String,
    /// The whole document, instead of editing the base.
    pub text: Option<String>,
    /// The Rust verdict: `accepted` or the field the error is about.
    pub expect: String,
    /// Python's verdict when it differs from `expect`.
    pub python: Option<String>,
    /// Why Python differs; required with `python`.
    pub reason: Option<String>,
}

#[derive(Debug, thiserror::Error)]
pub enum CorpusError {
    #[error("case {case:?}: {problem}")]
    Case { case: String, problem: String },
    #[error("base configuration: {0}")]
    Base(#[from] toml::de::Error),
    #[error("could not serialize case {case:?}: {problem}")]
    Serialize { case: String, problem: String },
}

impl Corpus {
    /// Every case is well-formed and has a unique name.
    pub fn validate(&self) -> Result<(), CorpusError> {
        let mut names = std::collections::BTreeSet::new();
        for case in &self.cases {
            let fail = |problem: &str| {
                Err(CorpusError::Case {
                    case: case.name.clone(),
                    problem: problem.to_owned(),
                })
            };
            if !names.insert(case.name.as_str()) {
                return fail("duplicate name");
            }
            if case.python.is_some() != case.reason.as_deref().is_some_and(|r| !r.trim().is_empty()) {
                return fail("`python` and a non-empty `reason` go together");
            }
            if case.python.as_ref() == Some(&case.expect) {
                return fail("`python` repeats `expect`");
            }
            if case.text.is_some() && (!case.set.is_empty() || !case.remove.is_empty() || !case.append.is_empty()) {
                return fail("`text` replaces the document; it cannot be combined with edits");
            }
        }
        Ok(())
    }
}

impl Case {
    /// The verdict Python must reach.
    #[must_use]
    pub fn python_verdict(&self) -> &str {
        self.python.as_deref().unwrap_or(&self.expect)
    }

    /// The document for this case.
    pub fn document(&self, base: &str) -> Result<String, CorpusError> {
        if let Some(text) = &self.text {
            return Ok(text.clone());
        }
        let mut table: toml::Table = base.parse()?;
        for (path, value) in &self.set {
            let (parent, key) = self.parent(&mut table, path, true)?;
            parent.insert(key, value.clone());
        }
        for path in &self.remove {
            let (parent, key) = self.parent(&mut table, path, false)?;
            if parent.remove(&key).is_none() {
                return Err(self.error(format!("{path} does not exist")));
            }
        }
        let mut text = toml::to_string(&table).map_err(|error| CorpusError::Serialize {
            case: self.name.clone(),
            problem: error.to_string(),
        })?;
        text.push_str(&self.append);
        Ok(text)
    }

    /// The table holding the last segment of `path`, creating missing tables
    /// when `create`.
    fn parent<'t>(
        &self,
        table: &'t mut toml::Table,
        path: &str,
        create: bool,
    ) -> Result<(&'t mut toml::Table, String), CorpusError> {
        let mut segments: Vec<&str> = path.split('.').collect();
        let key = segments
            .pop()
            .filter(|key| !key.is_empty())
            .ok_or_else(|| self.error(format!("bad path {path}")))?;
        let mut current = table;
        for segment in segments {
            if create && !current.contains_key(segment) {
                current.insert(segment.to_owned(), toml::Value::Table(toml::Table::new()));
            }
            current = match current.get_mut(segment) {
                Some(toml::Value::Table(inner)) => inner,
                _ => return Err(self.error(format!("{path}: {segment} is not a table"))),
            };
        }
        Ok((current, key.to_owned()))
    }

    fn error(&self, problem: String) -> CorpusError {
        CorpusError::Case {
            case: self.name.clone(),
            problem,
        }
    }
}
