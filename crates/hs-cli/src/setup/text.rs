//! The checks and quoting the setup shares with Python's `config.py`:
//! home paths, GitHub repositories and `repr` of a string in messages.

use crate::AppError;

/// Python's `repr()` of a string, as its messages quote values.
#[must_use]
pub fn py_repr(text: &str) -> String {
    let quote = if text.contains('\'') && !text.contains('"') {
        '"'
    } else {
        '\''
    };
    let mut out = String::with_capacity(text.len() + 2);
    out.push(quote);
    for c in text.chars() {
        match c {
            '\\' => out.push_str("\\\\"),
            '\n' => out.push_str("\\n"),
            '\r' => out.push_str("\\r"),
            '\t' => out.push_str("\\t"),
            c if c == quote => {
                out.push('\\');
                out.push(c);
            }
            c if (c as u32) < 0x20 || c as u32 == 0x7f => out.push_str(&format!("\\x{:02x}", c as u32)),
            c if (0x80..0xa0).contains(&(c as u32)) => out.push_str(&format!("\\x{:02x}", c as u32)),
            c => out.push(c),
        }
    }
    out.push(quote);
    out
}

/// `validate_home_path_spec`: `~/RELATIVE` or `~/RELATIVE/`; the relative
/// path without the slash and whether it names a directory.
pub fn home_path(value: &str) -> Result<(String, bool), AppError> {
    let Some(relative) = value.strip_prefix("~/") else {
        return Err(AppError::new(format!(
            "Home path must start with '~/': {}",
            py_repr(value)
        )));
    };
    if value.contains(['\0', '\n', '\r']) {
        return Err(AppError::new(format!(
            "Home path contains an invalid control character: {}",
            py_repr(value)
        )));
    }
    let is_directory = relative.ends_with('/');
    let core = if is_directory {
        &relative[..relative.len() - 1]
    } else {
        relative
    };
    if core.is_empty() {
        return Err(AppError::new("Using the entire home directory ('~/') is not allowed"));
    }
    if core.split('/').any(|part| matches!(part, "" | "." | "..")) {
        return Err(AppError::new(format!(
            "Home path must be a normalized path below '~/' without '.' or '..': {}",
            py_repr(value)
        )));
    }
    Ok((core.to_owned(), is_directory))
}

/// `GITHUB_OWNER_RE`: a GitHub account name.
#[must_use]
pub fn github_owner(text: &str) -> bool {
    let bytes = text.as_bytes();
    let alnum = |b: &u8| b.is_ascii_alphanumeric();
    match bytes {
        [] => false,
        [only] => alnum(only),
        [first, middle @ .., last] => {
            alnum(first) && alnum(last) && middle.len() <= 38 && middle.iter().all(|b| alnum(b) || *b == b'-')
        }
    }
}

/// `validate_repository_spec`: `OWNER/REPO`, trimmed.
pub fn repository(value: &str) -> Result<String, AppError> {
    let text = value.trim();
    let valid = text.split_once('/').is_some_and(|(owner, name)| {
        github_owner(owner)
            && (1..=100).contains(&name.len())
            && name.bytes().all(|b| b.is_ascii_alphanumeric() || b"_.-".contains(&b))
    });
    if !valid {
        return Err(AppError::new(format!(
            "Invalid GitHub repository {}; expected OWNER/REPO",
            py_repr(value)
        )));
    }
    let (_, name) = text.split_once('/').unwrap_or_default();
    if name == "." || name == ".." {
        return Err(AppError::new(format!(
            "Invalid GitHub repository name: {}",
            py_repr(name)
        )));
    }
    Ok(text.to_owned())
}

/// `[a-z][a-z0-9_-]*`: the ID of a setup group, item or config file.
#[must_use]
pub fn setup_id(text: &str) -> bool {
    let mut chars = text.chars();
    chars.next().is_some_and(|c| c.is_ascii_lowercase())
        && chars.all(|c| c.is_ascii_lowercase() || c.is_ascii_digit() || c == '_' || c == '-')
}

#[cfg(test)]
#[allow(clippy::unwrap_used)]
mod tests {
    use super::*;

    #[test]
    fn repr_quotes_as_python_does() {
        assert_eq!(py_repr("a"), "'a'");
        assert_eq!(py_repr("it's"), "\"it's\"");
        assert_eq!(py_repr("both ' \""), "'both \\' \"'");
        assert_eq!(py_repr("a\nb\\"), "'a\\nb\\\\'");
        assert_eq!(py_repr("\0"), "'\\x00'");
    }

    #[test]
    fn repositories_follow_the_github_grammar() {
        assert_eq!(repository(" owner/project ").unwrap(), "owner/project");
        for bad in ["owner", "-owner/x", "owner/", "owner/a b", "o/..", "owner/x/y"] {
            assert!(repository(bad).is_err(), "{bad}");
        }
    }
}
