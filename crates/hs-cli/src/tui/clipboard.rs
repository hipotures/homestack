//! Copy through the terminal, including when the TUI runs over SSH.

use std::io::{self, Write};

use base64::{Engine, engine::general_purpose::STANDARD};

pub(super) fn copy(output: &mut impl Write, text: &str) -> io::Result<()> {
    write!(output, "\x1b]52;c;{}\x07", STANDARD.encode(text))?;
    output.flush()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn clipboard_sequence_contains_the_complete_utf8_json() -> io::Result<()> {
        let text = "{\n  \"name\": \"żółć\"\n}";
        let mut output = Vec::new();
        copy(&mut output, text)?;
        assert!(output.starts_with(b"\x1b]52;c;"));
        assert!(output.ends_with(b"\x07"));
        let decoded = STANDARD
            .decode(&output[7..output.len() - 1])
            .map_err(io::Error::other)?;
        assert_eq!(decoded, text.as_bytes());
        Ok(())
    }
}
