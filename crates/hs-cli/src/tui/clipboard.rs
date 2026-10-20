//! Copy through the terminal, including when the TUI runs over SSH.

use std::io::{self, Write};

use base64::{Engine, engine::general_purpose::STANDARD};

pub(super) fn copy(output: &mut impl Write, text: &str) -> io::Result<()> {
    write!(output, "\x1b]52;c;{}\x07", STANDARD.encode(text))?;
    output.flush()
}
