//! Byte-preserving input for Readline settings that address raw terminal bytes.

use std::{
    collections::VecDeque,
    io,
    time::{Duration, Instant},
};

use brush_core::{
    interfaces::{Key, KeyStroke},
    openfiles::OpenFile,
};
use crossterm::event::{Event, KeyCode, KeyEvent, KeyModifiers};

use crate::terminal_keys;

pub(super) const PASTE_BEGIN: &[u8] = b"\x1b[200~";
pub(super) const PASTE_END: &[u8] = b"\x1b[201~";
const RAW_MODIFIERS: KeyModifiers = KeyModifiers::HYPER
    .union(KeyModifiers::SUPER)
    .union(KeyModifiers::META);

pub(super) struct TerminalInput {
    pending: Vec<u8>,
    unparsed: VecDeque<u8>,
    size: Option<(u16, u16)>,
    reading_paste: bool,
    key_timeout: Duration,
}

pub(super) fn attach(
    reedline: &mut reedline::Reedline,
    reader: &std::sync::Arc<std::sync::Mutex<TerminalInput>>,
    raw_input: bool,
    key_timeout: Duration,
) {
    let raw_input = {
        let mut terminal_input = reader
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        terminal_input.key_timeout = key_timeout;
        cfg!(unix) && (raw_input || terminal_input.has_pending_input())
    };
    // Always read through the byte-preserving source: crossterm's own poll can block
    // forever on a terminal that has been hung up, while this source polls stdin and checks
    // the hangup flag.
    let terminal_input = reader.clone();
    let source: Box<dyn FnMut(_) -> _ + Send> = Box::new(move |timeout| {
        terminal_input
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .read_event(timeout)
    });
    reedline.set_input_source(Some(source));
    let terminal_input = reader.clone();
    reedline.set_cursor_position_query(raw_input.then(|| {
        let query: Box<dyn Fn() -> _ + Send + Sync> = Box::new(move || {
            terminal_input
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner)
                .cursor_position()
        });
        query
    }));
}

impl TerminalInput {
    pub(super) fn new(key_timeout: Duration) -> Self {
        Self {
            pending: Vec::new(),
            unparsed: VecDeque::new(),
            size: crossterm::terminal::size().ok(),
            reading_paste: false,
            key_timeout,
        }
    }

    /// Query the same byte stream used by editing and retain the user's
    /// input while waiting for the terminal's response.
    pub(super) fn cursor_position(&mut self) -> io::Result<(u16, u16)> {
        use std::io::Write as _;

        {
            let mut stdout = io::stdout().lock();
            stdout.write_all(b"\x1b[6n")?;
            stdout.flush()?;
        }
        let deadline = Instant::now() + Duration::from_secs(2);
        let mut report_start = None;
        while let Some(timeout) = deadline.checked_duration_since(Instant::now()) {
            let Some(byte) = read_byte(timeout)? else {
                break;
            };
            if byte == 0x1b {
                report_start = Some(self.pending.len());
            }
            self.pending.push(byte);
            if byte == b'R'
                && let Some(start) = report_start.take()
                && let Some(position) = parse_cursor_report(&self.pending[start..])
            {
                self.pending.truncate(start);
                return Ok(position);
            }
        }
        Err(io::Error::new(
            io::ErrorKind::TimedOut,
            "no terminal cursor report",
        ))
    }

    pub(super) fn has_pending_input(&self) -> bool {
        !self.pending.is_empty() || !self.unparsed.is_empty() || self.reading_paste
    }

    pub(super) fn read_event(&mut self, timeout: Duration) -> io::Result<Option<Event>> {
        if crate::hangup::requested() {
            return Err(io::Error::new(
                io::ErrorKind::Interrupted,
                "terminal hangup",
            ));
        }

        if let Ok(size) = crossterm::terminal::size()
            && self.size.replace(size).is_some_and(|old| old != size)
        {
            return Ok(Some(Event::Resize(size.0, size.1)));
        }

        let mut more_coming = true;
        loop {
            if let Some(byte) = self.unparsed.pop_front() {
                return Ok(Some(event_for_stroke(&raw_stroke(byte))));
            }
            if self.pending.starts_with(PASTE_BEGIN) && !self.reading_paste {
                self.pending.clear();
                self.reading_paste = true;
            }
            if self.reading_paste {
                if self.pending.ends_with(PASTE_END) {
                    self.pending.truncate(self.pending.len() - PASTE_END.len());
                    let data = std::mem::take(&mut self.pending);
                    self.reading_paste = false;
                    return Ok(Some(Event::Paste(
                        String::from_utf8_lossy(&data).into_owned(),
                    )));
                }
            } else {
                let next = terminal_keys::decode(&self.pending, more_coming)
                    .or_else(|| (!more_coming && !self.pending.is_empty()).then_some((None, 1)));
                if let Some((stroke, len)) = next {
                    let bytes = self.pending.drain(..len).collect::<Vec<_>>();
                    let stroke = match (bytes.as_slice(), stroke) {
                        ([byte], _) if !byte.is_ascii() => raw_stroke(*byte),
                        (_, Some(stroke)) => stroke,
                        _ => {
                            self.unparsed.extend(bytes);
                            continue;
                        }
                    };
                    return Ok(Some(event_for_stroke(&stroke)));
                }
            }

            let wait = if self.pending.is_empty() || self.reading_paste {
                timeout
            } else {
                self.key_timeout
            };
            if let Some(byte) = read_byte(wait)? {
                self.pending.push(byte);
            } else if self.pending.is_empty() || self.reading_paste {
                return Ok(None);
            } else {
                more_coming = false;
            }
        }
    }
}

fn parse_cursor_report(bytes: &[u8]) -> Option<(u16, u16)> {
    let text = std::str::from_utf8(bytes.strip_prefix(b"\x1b[")?.strip_suffix(b"R")?).ok()?;
    let (row, column) = text.split_once(';')?;
    let zero_based = |n: &str| n.parse::<u16>().ok()?.checked_sub(1);
    Some((zero_based(column)?, zero_based(row)?))
}

fn read_byte(timeout: Duration) -> io::Result<Option<u8>> {
    if !brush_core::sys::poll::poll_for_input(&OpenFile::from(io::stdin()), timeout)? {
        return Ok(None);
    }
    let mut byte = [0];
    if brush_core::sys::input::read_stdin_bytes(&mut byte)? == 0 {
        // A closed terminal reads as EOF here (unlike Ctrl-D, which arrives as a byte). Treat
        // it as the hangup it is, so the session saves history and exits 129.
        crate::hangup::request();
        return Err(io::Error::new(
            io::ErrorKind::UnexpectedEof,
            "terminal input closed",
        ));
    }
    Ok(Some(byte[0]))
}

pub(super) fn event_for_stroke(stroke: &KeyStroke) -> Event {
    let (modifiers, code) = super::edit_mode::translate_key_stroke(stroke);
    let modifiers = if stroke_raw_byte(stroke).is_some() {
        RAW_MODIFIERS
    } else {
        modifiers
    };
    Event::Key(KeyEvent::new(code, modifiers))
}

pub(super) fn byte_key(byte: u8) -> char {
    char::from_u32(0xe000 + u32::from(byte)).unwrap_or(char::REPLACEMENT_CHARACTER)
}

pub(super) fn raw_stroke(byte: u8) -> KeyStroke {
    KeyStroke {
        shift: true,
        ..KeyStroke::from(Key::Character(byte_key(byte)))
    }
}

pub(super) fn stroke_raw_byte(stroke: &KeyStroke) -> Option<u8> {
    if let Key::Character(c) = stroke.key
        && stroke.shift
        && !stroke.alt
        && !stroke.control
    {
        return u32::from(c)
            .checked_sub(0xe000)
            .and_then(|value| u8::try_from(value).ok());
    }
    None
}

pub(super) fn raw_byte(key: KeyEvent) -> Option<u8> {
    if let KeyCode::Char(c) = key.code
        && key.modifiers == RAW_MODIFIERS
    {
        return stroke_raw_byte(&KeyStroke {
            shift: true,
            ..KeyStroke::from(Key::Character(c))
        });
    }
    None
}

pub(super) fn stroke_bytes(stroke: &KeyStroke) -> Vec<u8> {
    stroke_raw_byte(stroke).map_or_else(|| terminal_keys::encode(stroke), |byte| vec![byte])
}
