//! Terminal input utilities

use std::collections::HashMap;
use std::sync::LazyLock;
use terminfo::capability as cap;

use crate::{error, interfaces};

macro_rules! key {
    ( $terminfo:expr , $our_key:expr, $terminfo_key:ty ) => {{
        (
            $our_key,
            $terminfo
                .get::<$terminfo_key>()
                .map(|k| k.expand().to_vec()),
        )
    }};
}

fn build_terminfo_key_map() -> HashMap<Vec<u8>, interfaces::Key> {
    let mut map: HashMap<Vec<u8>, interfaces::Key> = HashMap::new();

    if let Ok(ti) = terminfo::Database::from_env() {
        // Iterate over key capabilities and populate the map
        let key_capabilities = [
            key!(ti, interfaces::Key::F(1), cap::KeyF1<'_>),
            key!(ti, interfaces::Key::F(2), cap::KeyF2<'_>),
            key!(ti, interfaces::Key::F(3), cap::KeyF3<'_>),
            key!(ti, interfaces::Key::F(4), cap::KeyF4<'_>),
            key!(ti, interfaces::Key::F(5), cap::KeyF5<'_>),
            key!(ti, interfaces::Key::F(6), cap::KeyF6<'_>),
            key!(ti, interfaces::Key::F(7), cap::KeyF7<'_>),
            key!(ti, interfaces::Key::F(8), cap::KeyF8<'_>),
            key!(ti, interfaces::Key::F(9), cap::KeyF9<'_>),
            key!(ti, interfaces::Key::F(10), cap::KeyF10<'_>),
            key!(ti, interfaces::Key::F(11), cap::KeyF11<'_>),
            key!(ti, interfaces::Key::F(12), cap::KeyF12<'_>),
            key!(ti, interfaces::Key::Backspace, cap::KeyBackspace<'_>),
            key!(ti, interfaces::Key::Enter, cap::KeyEnter<'_>),
            key!(ti, interfaces::Key::Left, cap::KeyLeft<'_>),
            key!(ti, interfaces::Key::Right, cap::KeyRight<'_>),
            key!(ti, interfaces::Key::Up, cap::KeyUp<'_>),
            key!(ti, interfaces::Key::Down, cap::KeyDown<'_>),
            key!(ti, interfaces::Key::Home, cap::KeyHome<'_>),
            key!(ti, interfaces::Key::End, cap::KeyEnd<'_>),
            key!(ti, interfaces::Key::PageUp, cap::KeyPPage<'_>),
            key!(ti, interfaces::Key::PageDown, cap::KeyNPage<'_>),
            key!(ti, interfaces::Key::BackTab, cap::BackTab<'_>),
            // It's not clear if these belong here, because they're not
            // strictly "key" capabilities.
            key!(ti, interfaces::Key::Up, cap::CursorUp<'_>),
            key!(ti, interfaces::Key::Down, cap::CursorDown<'_>),
            key!(ti, interfaces::Key::Left, cap::CursorLeft<'_>),
            key!(ti, interfaces::Key::Right, cap::CursorRight<'_>),
        ];

        for (key, v) in key_capabilities {
            if let Some(Ok(v)) = v {
                map.insert(v, key);
            }
        }
    }

    map
}

pub(crate) static TERMINFO_KEY_MAP: LazyLock<HashMap<Vec<u8>, interfaces::Key>> =
    LazyLock::new(build_terminfo_key_map);

/// Translates a key code (byte sequence) into a `Key` enum value. Returns `None`
/// if the key code is not recognized.
///
/// # Arguments
///
/// * `key_code`: The byte sequence representing the key code.
pub fn try_get_key_from_key_code(key_code: &[u8]) -> Option<interfaces::Key> {
    if let Some(key) = TERMINFO_KEY_MAP.get(key_code) {
        Some(key.clone())
    } else if key_code.len() == 1 && !key_code[0].is_ascii_control() {
        Some(interfaces::Key::Character(key_code[0] as char))
    } else {
        None
    }
}

/// Returns the terminal sequences for entering and leaving its keypad and Meta modes.
pub fn readline_terminal_modes(keypad: bool, meta: bool) -> (Vec<u8>, Vec<u8>) {
    let Ok(ti) = terminfo::Database::from_env() else {
        return (Vec::new(), Vec::new());
    };
    let mut enter = Vec::new();
    let mut leave = Vec::new();
    if keypad {
        if let Some(cap) = ti.get::<cap::KeypadXmit<'_>>() {
            let _ = cap.expand().to(&mut enter);
        }
        if let Some(cap) = ti.get::<cap::KeypadLocal<'_>>() {
            let _ = cap.expand().to(&mut leave);
        }
    }
    if meta {
        if let Some(cap) = ti.get::<cap::MetaOn<'_>>() {
            let _ = cap.expand().to(&mut enter);
        }
        if let Some(cap) = ti.get::<cap::MetaOff<'_>>() {
            let _ = cap.expand().to(&mut leave);
        }
    }
    (enter, leave)
}

/// Returns the input terminal's configured editing and interrupt characters.
pub fn readline_terminal_keys() -> Vec<(u8, Option<interfaces::InputFunction>)> {
    use interfaces::InputFunction as F;
    use nix::sys::termios::{SpecialCharacterIndices as C, tcgetattr};

    let Ok(settings) = tcgetattr(std::io::stdin()) else {
        return Vec::new();
    };
    let keys = [
        (C::VINTR, None),
        (C::VERASE, Some(F::BackwardDeleteChar)),
        (C::VKILL, Some(F::UnixLineDiscard)),
        #[cfg(not(any(target_os = "aix", target_os = "haiku")))]
        (C::VWERASE, Some(F::UnixWordRubout)),
        #[cfg(not(target_os = "haiku"))]
        (C::VLNEXT, Some(F::QuotedInsert)),
    ];
    keys.into_iter()
        .map(|(index, function)| (settings.control_chars[index as usize], function))
        .filter(|(byte, _)| !matches!(byte, 0 | 255))
        .collect()
}

/// Reads raw terminal bytes without buffering bytes that belong to a later read.
pub fn read_stdin_bytes(buffer: &mut [u8]) -> std::io::Result<usize> {
    Ok(nix::unistd::read(std::io::stdin(), buffer)?)
}

/// Returns the terminal's default active-region start and end sequences.
pub fn readline_terminal_standout() -> (String, String) {
    if let Ok(ti) = terminfo::Database::from_env() {
        let ((), start) = key!(ti, (), cap::EnterStandoutMode<'_>);
        let ((), end) = key!(ti, (), cap::ExitStandoutMode<'_>);
        if let (Some(Ok(start)), Some(Ok(end))) = (start, end) {
            return (
                String::from_utf8_lossy(&start).into_owned(),
                String::from_utf8_lossy(&end).into_owned(),
            );
        }
    }
    (String::new(), String::new())
}

/// Emits the terminal's visual bell, using the audible bell when it has no flash capability.
pub fn flash_readline_terminal(output: &mut impl std::io::Write) -> std::io::Result<()> {
    let flash = terminfo::Database::from_env().ok().and_then(|terminal| {
        terminal
            .get::<cap::FlashScreen<'_>>()?
            .expand()
            .to_vec()
            .ok()
    });
    let Some(flash) = flash else {
        output.write_all(b"\x07")?;
        return output.flush();
    };

    // Terminfo encodes padding as $<milliseconds>. A visible bell may
    // need the delay between the sequences that turn reverse video on and off.
    let mut remaining = flash.as_slice();
    while let Some(start) = remaining.windows(2).position(|bytes| bytes == b"$<") {
        let after = &remaining[start + 2..];
        let Some(end) = after.iter().position(|&byte| byte == b'>') else {
            break;
        };
        output.write_all(&remaining[..start])?;
        output.flush()?;
        if let Some(delay) = std::str::from_utf8(&after[..end])
            .ok()
            .and_then(|value| value.trim_end_matches(['*', '/']).parse::<f64>().ok())
            .and_then(|millis| {
                std::time::Duration::try_from_secs_f64(millis.min(500.0) / 1_000.0).ok()
            })
        {
            std::thread::sleep(delay);
        }
        remaining = &after[end + 1..];
    }
    output.write_all(remaining)?;
    output.flush()
}
