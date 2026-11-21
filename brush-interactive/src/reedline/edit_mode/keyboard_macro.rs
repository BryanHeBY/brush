use brush_core::interfaces::{InputFunction, KeySequence, KeyStroke};
use crossterm::event::Event;
use reedline::{ReedlineEvent, ReedlineRawEvent};

use super::super::terminal_input::{
    PASTE_BEGIN, PASTE_END, event_for_stroke, raw_stroke, stroke_bytes,
};
use crate::terminal_keys;

#[derive(Clone, Copy)]
pub(super) enum Command {
    Start,
    End,
    Call,
    Print,
}

impl Command {
    const ALL: [Self; 4] = [Self::Start, Self::End, Self::Call, Self::Print];

    pub(super) const fn function(self) -> InputFunction {
        match self {
            Self::Start => InputFunction::StartKbdMacro,
            Self::End => InputFunction::EndKbdMacro,
            Self::Call => InputFunction::CallLastKbdMacro,
            Self::Print => InputFunction::PrintLastKbdMacro,
        }
    }

    pub(super) const fn marker(self) -> &'static str {
        match self {
            Self::Start => "\0brush-start-kbd-macro",
            Self::End => "\0brush-end-kbd-macro",
            Self::Call => "\0brush-call-last-kbd-macro",
            Self::Print => "\0brush-print-last-kbd-macro",
        }
    }

    pub(super) fn from_event(event: &ReedlineEvent) -> Option<Self> {
        let ReedlineEvent::ExecuteHostCommand(marker) = event else {
            return None;
        };
        Self::ALL
            .into_iter()
            .find(|command| command.marker() == marker)
    }

    pub(super) fn for_function(function: &InputFunction) -> Option<Self> {
        Self::ALL
            .into_iter()
            .find(|command| command.function() == *function)
    }
}

#[derive(Default)]
pub(super) struct KeyboardMacro {
    recording: Option<Vec<Input>>,
    last: Option<Vec<Input>>,
    stack: Vec<Replay>,
    remaining: usize,
    pub(super) output: Option<String>,
    pub(super) bell_requested: bool,
}

#[derive(Clone)]
enum Input {
    Key(KeyStroke),
    Paste(String),
}

struct Replay {
    input: Vec<Input>,
    position: usize,
    repeats: usize,
}

impl KeyboardMacro {
    pub(super) fn record(&mut self, stroke: &KeyStroke) {
        if let Some(recording) = &mut self.recording {
            recording.push(Input::Key(stroke.clone()));
        }
    }

    pub(super) fn record_paste(&mut self, text: &str) {
        if let Some(recording) = &mut self.recording {
            recording.push(Input::Paste(text.to_owned()));
        }
    }

    pub(super) fn execute(&mut self, command: Command, trigger_len: usize, repeat: usize) {
        match command {
            Command::Start => {
                if self.recording.is_none() {
                    self.recording = Some(Vec::new());
                } else {
                    self.bell_requested = true;
                }
            }
            Command::End => {
                if let Some(mut recording) = self.recording.take() {
                    recording.truncate(recording.len().saturating_sub(trigger_len));
                    if repeat > 1 {
                        self.replay(recording.clone(), repeat - 1);
                    }
                    self.last = Some(recording);
                } else {
                    self.bell_requested = true;
                }
            }
            Command::Call => {
                if let Some(last) = &self.last {
                    self.replay(last.clone(), repeat);
                } else {
                    self.bell_requested = true;
                }
            }
            Command::Print => {
                if let Some(last) = &self.last {
                    self.output = Some(format_macro(last));
                } else {
                    self.bell_requested = true;
                }
            }
        }
    }

    pub(super) fn replay_sequence(&mut self, sequence: &KeySequence) {
        let input = match sequence {
            KeySequence::Strokes(strokes) => strokes.iter().cloned().map(Input::Key).collect(),
            KeySequence::Bytes(bytes) => {
                let bytes = bytes.iter().flatten().copied().collect::<Vec<_>>();
                let mut remaining = bytes.as_slice();
                let mut input = Vec::new();
                while !remaining.is_empty() {
                    if let Some(body) = remaining.strip_prefix(PASTE_BEGIN)
                        && let Some(end) = body
                            .windows(PASTE_END.len())
                            .position(|bytes| bytes == PASTE_END)
                    {
                        input.push(Input::Paste(
                            String::from_utf8_lossy(&body[..end]).into_owned(),
                        ));
                        remaining = &body[end + PASTE_END.len()..];
                        continue;
                    }
                    let (stroke, len) =
                        terminal_keys::decode(remaining, false).unwrap_or((None, 1));
                    let (prefix, rest) = remaining.split_at(len);
                    match (prefix, stroke) {
                        ([byte], _) if !byte.is_ascii() => {
                            input.push(Input::Key(raw_stroke(*byte)));
                        }
                        (_, Some(stroke)) => input.push(Input::Key(stroke)),
                        _ => input.extend(prefix.iter().copied().map(raw_stroke).map(Input::Key)),
                    }
                    remaining = rest;
                }
                input
            }
        };
        self.replay(input, 1);
    }

    pub(super) fn interrupt(&mut self) {
        self.recording = None;
        self.stack.clear();
    }

    fn replay(&mut self, input: Vec<Input>, repeats: usize) {
        // Bound both recursive cycles and finite expansions that produce
        // excessive input through repeated nested invocations.
        if self.stack.len() == 64 {
            self.stack.clear();
            self.bell_requested = true;
        } else if !input.is_empty() && repeats > 0 {
            if self.stack.is_empty() {
                self.remaining = 65_536;
            }
            self.stack.push(Replay {
                input,
                position: 0,
                repeats,
            });
        }
    }

    pub(super) fn next_event(&mut self) -> Option<ReedlineRawEvent> {
        if self.remaining == 0 {
            self.stack.clear();
        }
        while let Some(frame) = self.stack.last_mut() {
            if frame.position == frame.input.len() {
                frame.repeats -= 1;
                if frame.repeats == 0 {
                    self.stack.pop();
                    continue;
                }
                frame.position = 0;
            }
            let event = match &frame.input[frame.position] {
                Input::Key(stroke) => event_for_stroke(stroke),
                Input::Paste(text) => Event::Paste(text.clone()),
            };
            frame.position += 1;
            self.remaining -= 1;
            return event.try_into().ok();
        }
        None
    }
}

fn format_macro(input: &[Input]) -> String {
    use std::fmt::Write as _;

    let mut result = String::new();
    for byte in input.iter().flat_map(|event| match event {
        Input::Key(stroke) => stroke_bytes(stroke),
        Input::Paste(text) => [PASTE_BEGIN, text.as_bytes(), PASTE_END].concat(),
    }) {
        match byte {
            0x07 => result.push_str(r"\a"),
            0x08 => result.push_str(r"\b"),
            b'\t' => result.push_str(r"\t"),
            b'\n' => result.push_str(r"\n"),
            0x0b => result.push_str(r"\v"),
            0x0c => result.push_str(r"\f"),
            b'\r' => result.push_str(r"\r"),
            0x1b => result.push_str(r"\e"),
            0x7f => result.push_str(r"\C-?"),
            b'"' | b'\\' => {
                result.push('\\');
                result.push(char::from(byte));
            }
            0x00..=0x1f => {
                let control = char::from(byte | b'@').to_ascii_lowercase();
                let _ = write!(result, r"\C-{control}");
            }
            b' '..=b'~' => result.push(char::from(byte)),
            _ => {
                let _ = write!(result, r"\{byte:03o}");
            }
        }
    }
    result
}
