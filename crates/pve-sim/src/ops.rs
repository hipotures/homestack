//! Canonical operations, the shared vocabulary of the parity tests.
//!
//! An operation is a name and sorted `key=value` parameters, written as
//! `name{k1=v1,k2=v2}`. The Python harness produces the same text from shell
//! commands, so both implementations can be compared call by call. The full
//! vocabulary is documented in `parity/ops.md`. Values are written with
//! `%`, `,`, `=`, `{` and `}` percent-encoded, so a shell command fits in one
//! parameter.

use std::collections::BTreeMap;
use std::fmt;
use std::str::FromStr;

#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub struct Op {
    pub name: String,
    pub params: BTreeMap<String, String>,
}

impl Op {
    /// A read: `get{path=...}` plus any query parameters. A query parameter
    /// called `path` is written `query_path`, so it cannot replace the API
    /// path.
    #[must_use]
    pub fn get(path: &str, query: impl IntoIterator<Item = (String, String)>) -> Self {
        let mut params: BTreeMap<String, String> = query
            .into_iter()
            .map(|(key, value)| {
                if key == "path" {
                    ("query_path".to_owned(), value)
                } else {
                    (key, value)
                }
            })
            .collect();
        params.insert("path".to_owned(), path.to_owned());
        Self {
            name: "get".to_owned(),
            params,
        }
    }
}

impl fmt::Display for Op {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{}{{", self.name)?;
        for (index, (key, value)) in self.params.iter().enumerate() {
            if index > 0 {
                f.write_str(",")?;
            }
            write!(f, "{key}={}", escape(value))?;
        }
        f.write_str("}")
    }
}

impl Op {
    /// An operation with the given parameters.
    #[must_use]
    pub fn new<'a>(name: &str, params: impl IntoIterator<Item = (&'a str, String)>) -> Self {
        Self {
            name: name.to_owned(),
            params: params.into_iter().map(|(k, v)| (k.to_owned(), v)).collect(),
        }
    }
}

const ESCAPED: [(char, &str); 5] = [('%', "%25"), (',', "%2C"), ('=', "%3D"), ('{', "%7B"), ('}', "%7D")];

fn escape(value: &str) -> String {
    let mut out = String::with_capacity(value.len());
    for c in value.chars() {
        match ESCAPED.iter().find(|(raw, _)| *raw == c) {
            Some((_, code)) => out.push_str(code),
            None => out.push(c),
        }
    }
    out
}

fn unescape(value: &str) -> Option<String> {
    let mut out = String::with_capacity(value.len());
    let mut rest = value;
    while let Some(index) = rest.find('%') {
        out.push_str(&rest[..index]);
        let code = rest.get(index..index + 3)?;
        let (raw, _) = ESCAPED.iter().find(|(_, known)| known.eq_ignore_ascii_case(code))?;
        out.push(*raw);
        rest = &rest[index + 3..];
    }
    out.push_str(rest);
    Some(out)
}

#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
#[error("invalid operation {0:?}: expected name{{key=value}} with comma-separated keys")]
pub struct OpParseError(String);

impl FromStr for Op {
    type Err = OpParseError;

    fn from_str(text: &str) -> Result<Self, Self::Err> {
        let error = || OpParseError(text.to_owned());
        let (name, rest) = text.split_once('{').ok_or_else(error)?;
        let body = rest.strip_suffix('}').ok_or_else(error)?;
        if name.is_empty() || !name.bytes().all(|b| b.is_ascii_lowercase() || b == b'_') {
            return Err(error());
        }
        let mut params = BTreeMap::new();
        for pair in body.split(',').filter(|pair| !pair.is_empty()) {
            let (key, value) = pair.split_once('=').ok_or_else(error)?;
            if params
                .insert(key.to_owned(), unescape(value).ok_or_else(error)?)
                .is_some()
            {
                return Err(error());
            }
        }
        Ok(Self {
            name: name.to_owned(),
            params,
        })
    }
}
