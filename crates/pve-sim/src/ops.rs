//! Canonical operations, the shared vocabulary of the parity tests.
//!
//! An operation is a name and sorted `key=value` parameters, written as
//! `name{k1=v1,k2=v2}`. The Python harness produces the same text from shell
//! commands, so both implementations can be compared call by call. The full
//! vocabulary is documented in `parity/ops.md`.

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
            write!(f, "{key}={value}")?;
        }
        f.write_str("}")
    }
}

#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
#[error("invalid operation {0:?}: expected name{{key=value,...}}")]
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
            if params.insert(key.to_owned(), value.to_owned()).is_some() {
                return Err(error());
            }
        }
        Ok(Self {
            name: name.to_owned(),
            params,
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn display_sorts_parameters_and_round_trips() -> Result<(), OpParseError> {
        let op = Op::get("/cluster/resources", [("type".to_owned(), "vm".to_owned())]);
        assert_eq!(op.to_string(), "get{path=/cluster/resources,type=vm}");
        assert_eq!(op.to_string().parse::<Op>()?, op);
        Ok(())
    }

    #[test]
    fn malformed_text_is_rejected() {
        for text in ["get", "get{path=/x", "Get{path=/x}", "get{path}", "get{a=1,a=2}"] {
            assert!(text.parse::<Op>().is_err(), "{text}");
        }
    }
}
