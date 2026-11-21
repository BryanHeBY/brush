//! Implements a parser for readline binding syntax.

use crate::error;

/// Represents a key-sequence-to-shell-command binding.
#[derive(Debug, Clone, PartialEq, Eq)]
#[cfg_attr(test, derive(serde::Serialize, serde::Deserialize))]
pub struct KeySequenceShellCommandBinding {
    /// Key sequence to bind
    pub seq: KeySequence,
    /// Shell command to bind to the sequence
    pub shell_cmd: String,
}

/// Represents a key-sequence-to-readline-command binding.
#[derive(Debug, Clone, PartialEq, Eq)]
#[cfg_attr(test, derive(serde::Serialize, serde::Deserialize))]
pub struct KeySequenceReadlineBinding {
    /// Key sequence to bind
    pub seq: KeySequence,
    /// Readline target to bind to the sequence
    pub target: ReadlineTarget,
}

/// Represents a readline target.
#[derive(Debug, Clone, PartialEq, Eq)]
#[cfg_attr(test, derive(serde::Serialize, serde::Deserialize))]
pub enum ReadlineTarget {
    /// A named readline function.
    Function(String),
    /// A readline command macro.
    Macro(String),
}

/// Represents a key sequence.
#[derive(Debug, Clone, PartialEq, Eq)]
#[cfg_attr(test, derive(serde::Serialize, serde::Deserialize))]
pub struct KeySequence(pub Vec<KeySequenceItem>);

/// Represents an element of a key sequence.
#[derive(Debug, Clone, PartialEq, Eq)]
#[cfg_attr(test, derive(serde::Serialize, serde::Deserialize))]
pub enum KeySequenceItem {
    /// Control
    Control,
    /// Meta
    Meta,
    /// Regular character
    Byte(u8),
}

/// Represents a single key stroke.
#[derive(Debug, Default, Clone, PartialEq, Eq)]
#[cfg_attr(test, derive(serde::Serialize, serde::Deserialize))]
pub struct KeyStroke {
    /// Meta key is held down
    pub meta: bool,
    /// Control key is held down
    pub control: bool,
    /// Primary key code
    pub key_code: Vec<u8>,
}

/// Parses a key sequence.
///
/// # Arguments
///
/// * `input` - The input string to parse
pub fn parse_key_sequence(input: &str) -> Result<KeySequence, error::BindingParseError> {
    readline_binding::key_sequence(input)
        .map_err(|_err| error::BindingParseError::Unknown(input.to_owned()))
}

/// Parses a binding specification that maps a key sequence
/// to a shell command.
///
/// # Arguments
///
/// * `input` - The input string to parse
pub fn parse_key_sequence_shell_cmd_binding(
    input: &str,
) -> Result<KeySequenceShellCommandBinding, error::BindingParseError> {
    readline_binding::key_sequence_shell_cmd_binding(input)
        .map_err(|_err| error::BindingParseError::Unknown(input.to_owned()))
}

/// Parses a binding specification that maps a key sequence
/// to a readline target.
///
/// # Arguments
///
/// * `input` - The input string to parse
pub fn parse_key_sequence_readline_binding(
    input: &str,
) -> Result<KeySequenceReadlineBinding, error::BindingParseError> {
    readline_binding::key_sequence_readline_binding(input)
        .map_err(|_err| error::BindingParseError::Unknown(input.to_owned()))
}

/// Converts a `KeySequence` to a vector of `KeyStroke`.
///
/// # Arguments
///
/// * `seq` - The key sequence to convert
pub fn key_sequence_to_strokes(
    seq: &KeySequence,
) -> Result<Vec<KeyStroke>, error::BindingParseError> {
    let mut strokes = vec![];
    let mut current_stroke = KeyStroke::default();

    for item in &seq.0 {
        if matches!(
            item,
            KeySequenceItem::Control | KeySequenceItem::Meta | KeySequenceItem::Byte(b'\x1b')
        ) && !current_stroke.key_code.is_empty()
        {
            strokes.push(current_stroke);
            current_stroke = KeyStroke::default();
        }

        match item {
            KeySequenceItem::Control => current_stroke.control = true,
            KeySequenceItem::Meta => current_stroke.meta = true,
            KeySequenceItem::Byte(b) => {
                current_stroke.key_code.push(*b);
                // If this is a control or meta stroke, the modifier only applies to this one byte,
                // so we need to push the stroke and start fresh for subsequent bytes.
                if current_stroke.control || current_stroke.meta {
                    strokes.push(current_stroke);
                    current_stroke = KeyStroke::default();
                }
            }
        }
    }

    if current_stroke.key_code.is_empty() {
        if current_stroke.control || current_stroke.meta {
            return Err(error::BindingParseError::MissingKeyCode);
        }
    } else {
        strokes.push(current_stroke);
    }

    Ok(strokes)
}

fn symbolic_key(input: &str) -> Result<KeySequence, &'static str> {
    let name = input.trim();
    let lower = name.to_ascii_lowercase();
    let mut rest = lower.as_str();
    let mut modifiers = Vec::new();
    loop {
        if let Some(after) = rest
            .strip_prefix("control-")
            .or_else(|| rest.strip_prefix("c-"))
        {
            modifiers.push(KeySequenceItem::Control);
            rest = after;
        } else if let Some(after) = rest
            .strip_prefix("meta-")
            .or_else(|| rest.strip_prefix("m-"))
        {
            modifiers.push(KeySequenceItem::Meta);
            rest = after;
        } else {
            break;
        }
    }
    let byte = match rest {
        "escape" | "esc" => b'\x1b',
        "delete" | "del" | "rubout" => b'\x7f',
        "backspace" | "bs" => b'\x08',
        "space" | "spc" => b' ',
        "return" | "ret" => b'\r',
        "newline" | "linefeed" | "lfd" => b'\n',
        "tab" => b'\t',
        "nul" | "null" => b'\0',
        _ => {
            let original = name
                .get(name.len().saturating_sub(rest.len())..)
                .unwrap_or(rest);
            let mut chars = original.chars();
            let Some(c) = chars.next().filter(|_| chars.next().is_none()) else {
                return Err("unknown symbolic key");
            };
            modifiers.extend(utf8_bytes(c));
            return Ok(KeySequence(modifiers));
        }
    };
    modifiers.push(KeySequenceItem::Byte(byte));
    Ok(KeySequence(modifiers))
}

// In the whitespace-separated form of bind -x the surrounding quotes are
// Readline quoting. Decode those escapes before passing the command to the shell.
fn expand_quoted_shell_command(command: &str) -> Result<String, &'static str> {
    let seq = readline_binding::key_sequence(command).map_err(|_| "invalid command escape")?;
    let strokes = key_sequence_to_strokes(&seq).map_err(|_| "missing command escape byte")?;
    let mut bytes = Vec::new();
    for stroke in strokes {
        for mut byte in stroke.key_code {
            if stroke.control {
                byte = if byte == b'?' { 0x7f } else { byte & 0x1f };
            }
            if stroke.meta {
                byte |= 0x80;
            }
            bytes.push(byte);
        }
    }
    String::from_utf8(bytes).map_err(|_| "shell command is not UTF-8")
}

fn utf8_bytes(character: char) -> Vec<KeySequenceItem> {
    character
        .encode_utf8(&mut [0; 4])
        .bytes()
        .map(KeySequenceItem::Byte)
        .collect()
}

peg::parser! {
    grammar readline_binding() for str {
        rule _() = [' ' | '\t']*
        rule spaced() = [' ' | '\t']+

        pub rule key_sequence_shell_cmd_binding() -> KeySequenceShellCommandBinding =
            _ seq:readline_binding_key() _ ":" _ cmd:$([_]*) {
                KeySequenceShellCommandBinding { seq, shell_cmd: cmd.to_owned() }
            } /
            _ seq:readline_binding_key() spaced() cmd:quoted_macro() _ {?
                let shell_cmd = expand_quoted_shell_command(&cmd)?;
                Ok(KeySequenceShellCommandBinding { seq, shell_cmd })
            }

        pub rule key_sequence_readline_binding() -> KeySequenceReadlineBinding =
            _ seq:readline_binding_key() _ ":" _ cmd:quoted_macro() ending() {
                KeySequenceReadlineBinding { seq, target: ReadlineTarget::Macro(cmd) }
            } /
            _ seq:readline_binding_key() _ ":" _ func:$(['a'..='z' | 'A'..='Z' | '0'..='9' | '_' | '-']+) ending() {
                KeySequenceReadlineBinding { seq, target: ReadlineTarget::Function(func.to_owned()) }
            }

        rule ending() = _ ("#" [_]*)?

        rule readline_binding_key() -> KeySequence =
            "\"" seq:quoted_key_sequence() "\"" { seq } /
            name:$([^':' | '"' | ' ' | '\t']+) {? symbolic_key(name) }

        rule quoted_macro() -> String =
            "\"" body:$(( "\\" [_] / [^'"' | '\\'] )*) "\"" { body.to_owned() } /
            "'" body:$(( "\\" [_] / [^'\'' | '\\'] )*) "'" { body.to_owned() }

        rule quoted_key_sequence() -> KeySequence =
            items:key_sequence_item()* { KeySequence(items.into_iter().flatten().collect()) }

        pub rule key_sequence() -> KeySequence =
            items:(key_sequence_item() / "\"" { vec![KeySequenceItem::Byte(b'"')] })* {
                KeySequence(items.into_iter().flatten().collect())
            }

        rule key_sequence_item() -> Vec<KeySequenceItem> =
            item:escape_item() { vec![item] } /
            "\\" c:[_] { utf8_bytes(c) } /
            c:[^'"' | '\\'] { utf8_bytes(c) }

        rule escape_item() -> KeySequenceItem =
            "\\C-" { KeySequenceItem::Control } /
            "\\M-" { KeySequenceItem::Meta } /
            "\\e" { KeySequenceItem::Byte(b'\x1b') } /
            "\\a" { KeySequenceItem::Byte(b'\x07') } /
            "\\b" { KeySequenceItem::Byte(b'\x08') } /
            "\\d" { KeySequenceItem::Byte(b'\x7f') } /
            "\\f" { KeySequenceItem::Byte(b'\x0c') } /
            "\\n" { KeySequenceItem::Byte(b'\n') } /
            "\\r" { KeySequenceItem::Byte(b'\r') } /
            "\\t" { KeySequenceItem::Byte(b'\t') } /
            "\\v" { KeySequenceItem::Byte(b'\x0b') } /
            "\\x" n:hex_number() { KeySequenceItem::Byte(n) } /
            "\\" n:octal_number() { KeySequenceItem::Byte(n) }

        rule octal_number() -> u8 =
            s:$(['0'..='7']*<1,3>) {? u8::from_str_radix(s, 8).or(Err("invalid octal number")) }

        rule hex_number() -> u8 =
            s:$(['0'..='9' | 'a'..='f' | 'A'..='F']*<1,2>) {? u8::from_str_radix(s, 16).or(Err("invalid hex number")) }
    }
}

#[cfg(test)]
#[expect(clippy::panic_in_result_fn)]
mod tests {
    use super::*;
    use anyhow::Result;

    #[test]
    fn test_basic_shell_cmd_binding_parse() -> Result<()> {
        let binding = parse_key_sequence_shell_cmd_binding(r#""\C-k": xyz"#)?;
        assert_eq!(
            binding.seq.0,
            [KeySequenceItem::Control, KeySequenceItem::Byte(b'k')]
        );
        assert_eq!(binding.shell_cmd, "xyz");

        Ok(())
    }

    #[test]
    fn test_basic_readline_func_binding_parse() -> Result<()> {
        let binding = parse_key_sequence_readline_binding(r#""\M-x": some-function"#)?;
        assert_eq!(
            binding.seq.0,
            [KeySequenceItem::Meta, KeySequenceItem::Byte(b'x')]
        );
        assert_eq!(
            binding.target,
            ReadlineTarget::Function("some-function".to_string())
        );

        Ok(())
    }

    #[test]
    fn test_basic_readline_cmd_binding_parse() -> Result<()> {
        let binding = parse_key_sequence_readline_binding(r#""\C-k": "xyz""#)?;
        assert_eq!(
            binding.seq.0,
            [KeySequenceItem::Control, KeySequenceItem::Byte(b'k')]
        );
        assert_eq!(binding.target, ReadlineTarget::Macro(String::from("xyz")));

        Ok(())
    }
}
