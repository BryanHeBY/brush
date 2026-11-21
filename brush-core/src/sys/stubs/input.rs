//! Terminal input utilities

use crate::{error, interfaces};

/// Translates a key code (byte sequence) into a `Key` enum value. Returns `None`
/// if the key code is not recognized.
///
/// This is a stub implementation that recognizes single-byte non-control
/// characters but does not support terminal-specific key sequences.
pub fn try_get_key_from_key_code(key_code: &[u8]) -> Option<interfaces::Key> {
    if key_code.len() == 1 && !key_code[0].is_ascii_control() {
        Some(interfaces::Key::Character(key_code[0] as char))
    } else {
        None
    }
}

/// Returns empty terminal mode sequences on platforms without terminfo.
pub fn readline_terminal_modes(_keypad: bool, _meta: bool) -> (Vec<u8>, Vec<u8>) {
    (Vec::new(), Vec::new())
}

/// Returns no terminal editing character overrides on this platform.
pub fn readline_terminal_keys() -> Vec<(u8, Option<interfaces::InputFunction>)> {
    Vec::new()
}

/// Returns an unsupported error on platforms without a byte-oriented terminal.
pub fn read_stdin_bytes(_buffer: &mut [u8]) -> std::io::Result<usize> {
    Err(std::io::Error::from(std::io::ErrorKind::Unsupported))
}

/// Returns ANSI reverse video and its matching reset for this platform.
pub fn readline_terminal_standout() -> (String, String) {
    ("\x1b[7m".to_owned(), "\x1b[27m".to_owned())
}

/// Requests the audible bell on platforms without a terminal flash capability.
pub fn flash_readline_terminal(output: &mut impl std::io::Write) -> std::io::Result<()> {
    output.write_all(b"\x07")?;
    output.flush()
}
