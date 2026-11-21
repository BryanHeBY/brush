use brush_core::{
    interfaces::{self, InputFunction, Key, KeyAction, KeySequence, KeyStroke},
    trace_categories,
};
use radix_trie::{Trie, TrieCommon as _};
use std::{
    collections::{HashMap, HashSet},
    sync::Arc,
};
use tokio::sync::Mutex;

use super::readline::{Action, Session};

mod keyboard_macro;
mod keymaps;
pub(super) use keymaps::BindingMaps;

#[derive(thiserror::Error, Debug)]
pub enum KeyError {
    /// Unsupported key sequence
    #[error("unsupported key sequence: {0}")]
    UnsupportedKeySequence(KeySequence),

    /// Unsupported key action
    #[error("unsupported key action: {0}")]
    UnsupportedKeyAction(KeyAction),
}

pub(crate) struct MutableEditMode {
    inner: Arc<Mutex<BindingMaps>>,
    readline: Session,
}

impl MutableEditMode {
    pub fn new(bindings: reedline::Keybindings, readline: Session) -> Self {
        Self {
            inner: Arc::new(Mutex::new(BindingMaps::new(bindings))),
            readline,
        }
    }

    pub fn bindings(&self) -> Arc<Mutex<BindingMaps>> {
        self.inner.clone()
    }
}

impl reedline::EditMode for MutableEditMode {
    fn parse_event(&mut self, event: reedline::ReedlineRawEvent) -> reedline::ReedlineEvent {
        let mut inner = tokio::task::block_in_place(|| {
            tokio::runtime::Handle::current().block_on(self.inner.lock())
        });

        let mut readline = self.readline.lock();
        let event = readline.parse_event(event.into(), |event| {
            event
                .try_into()
                .ok()
                .map_or(reedline::ReedlineEvent::None, |event| {
                    inner.parse_event(event)
                })
        });
        if let Some(output) = inner.take_macro_output() {
            readline.print_message(output);
        }
        if inner.take_macro_bell() {
            readline.bell();
        }
        drop(inner);
        let default_tab = matches!(&event, reedline::ReedlineEvent::UntilFound(actions)
            if matches!(actions.first(), Some(reedline::ReedlineEvent::Menu(name)) if name == super::input_backend::COMPLETION_MENU_NAME));
        readline
            .completion_event(&event, default_tab)
            .or_else(|| readline.comment_event(&event))
            .or_else(|| readline.history_event(&event))
            .unwrap_or(event)
    }

    fn next_replay_event(&mut self) -> Option<reedline::ReedlineRawEvent> {
        tokio::task::block_in_place(|| {
            tokio::runtime::Handle::current()
                .block_on(self.inner.lock())
                .next_replay_event()
        })
    }

    fn edit_mode(&self) -> reedline::PromptEditMode {
        let inner = tokio::task::block_in_place(|| {
            tokio::runtime::Handle::current().block_on(self.inner.lock())
        });

        inner.edit_mode()
    }

    fn handle_mode_specific_event(
        &mut self,
        event: reedline::ReedlineEvent,
    ) -> reedline::EventStatus {
        tokio::task::block_in_place(|| {
            tokio::runtime::Handle::current()
                .block_on(self.inner.lock())
                .handle_mode_specific_event(event)
        })
    }
}

pub(crate) struct UpdatableBindings {
    bindings: reedline::Keybindings,
    edit_mode: Box<dyn reedline::EditMode>,
    /// Bindings whose terminal bytes have no portable key representation.
    raw_mappings: Trie<Vec<u8>, interfaces::KeyAction>,
    /// Tracks defined macros.
    macros: HashMap<interfaces::KeySequence, interfaces::KeySequence>,
    sequences: HashMap<Vec<KeyStroke>, KeyAction>,
    configured: HashSet<KeySequence>,
}

impl UpdatableBindings {
    pub fn new(bindings: reedline::Keybindings) -> Self {
        // Clone the bindings so we can keep a copy for later updates.
        let edit_mode = Self::rebuild_edit_mode(&bindings);

        Self {
            bindings,
            edit_mode,
            raw_mappings: Trie::new(),
            macros: HashMap::new(),
            sequences: HashMap::new(),
            configured: HashSet::new(),
        }
    }

    pub fn update(&mut self, f: impl Fn(&mut reedline::Keybindings)) {
        f(&mut self.bindings);
        self.edit_mode = Self::rebuild_edit_mode(&self.bindings);
    }

    fn rebuild_edit_mode(bindings: &reedline::Keybindings) -> Box<dyn reedline::EditMode> {
        Box::new(reedline::Emacs::new(bindings.clone()))
    }
}

impl reedline::EditMode for UpdatableBindings {
    fn parse_event(&mut self, event: reedline::ReedlineRawEvent) -> reedline::ReedlineEvent {
        self.edit_mode.parse_event(event)
    }

    fn edit_mode(&self) -> reedline::PromptEditMode {
        self.edit_mode.edit_mode()
    }
}

impl interfaces::KeyBindings for UpdatableBindings {
    fn get_current(&self) -> HashMap<interfaces::KeySequence, interfaces::KeyAction> {
        let mut results = HashMap::new();

        for (key_combo, event) in self.bindings.get_keybindings() {
            let action = translate_reedline_event_to_action(event);
            if let Some(action) = action {
                if let Some(key) = translate_reedline_keycode(key_combo.key_code) {
                    let mut stroke = KeyStroke::from(key);

                    if key_combo.modifier.contains(reedline::KeyModifiers::CONTROL) {
                        stroke.control = true;
                    }
                    if key_combo.modifier.contains(reedline::KeyModifiers::ALT) {
                        stroke.alt = true;
                    }
                    if key_combo.modifier.contains(reedline::KeyModifiers::SHIFT) {
                        stroke.shift = true;
                    }
                    if key_combo.modifier.contains(reedline::KeyModifiers::HYPER) {
                        // TODO
                    }
                    if key_combo.modifier.contains(reedline::KeyModifiers::META) {
                        // TODO
                    }
                    if key_combo.modifier.contains(reedline::KeyModifiers::SUPER) {
                        // TODO
                    }

                    let seq = KeySequence::from(stroke);
                    results.insert(seq, action);
                }
            }
        }
        results.extend(
            self.raw_mappings
                .iter()
                .map(|(bytes, action)| (KeySequence::Bytes(vec![bytes.clone()]), action.clone())),
        );
        results.extend(
            self.sequences
                .iter()
                .map(|(keys, action)| (KeySequence::Strokes(keys.clone()), action.clone())),
        );

        results
    }

    fn get_untranslated(&self, bytes: &[u8]) -> Option<&KeyAction> {
        self.raw_mappings.get(bytes)
    }

    fn bind(&mut self, seq: KeySequence, action: KeyAction) -> Result<(), std::io::Error> {
        let Some(event) = translate_action_to_reedline_event(&action) else {
            return Err(std::io::Error::other(KeyError::UnsupportedKeyAction(
                action,
            )));
        };
        match &seq {
            KeySequence::Strokes(strokes) => {
                if let Some((modifiers, key_code)) = translate_key_sequence_to_reedline(&seq) {
                    self.update(|bindings| {
                        bindings.add_binding(modifiers, key_code, event.clone());
                    });
                } else if strokes.len() > 1 {
                    self.sequences.insert(strokes.clone(), action);
                } else {
                    return Err(std::io::Error::other(KeyError::UnsupportedKeySequence(seq)));
                }
            }
            KeySequence::Bytes(bytes) => {
                self.raw_mappings
                    .insert(bytes.iter().flatten().copied().collect(), action);
            }
        }
        self.macros.remove(&seq);
        self.configured.insert(seq);
        Ok(())
    }

    fn try_unbind(&mut self, seq: KeySequence) -> bool {
        self.configured.insert(seq.clone());
        let removed_macro = self.macros.remove(&seq).is_some();
        let removed_binding = match &seq {
            KeySequence::Strokes(strokes) => {
                if let Some((modifiers, key_code)) = translate_key_sequence_to_reedline(&seq) {
                    let found = self.bindings.find_binding(modifiers, key_code).is_some();
                    if found {
                        self.update(|bindings| {
                            bindings.remove_binding(modifiers, key_code);
                        });
                    }
                    found
                } else {
                    self.sequences.remove(strokes).is_some()
                }
            }
            KeySequence::Bytes(bytes) => {
                let flat_bytes = bytes.iter().flatten().copied().collect::<Vec<_>>();
                self.raw_mappings.remove(&flat_bytes).is_some()
            }
        };
        removed_macro || removed_binding
    }

    fn define_macro(
        &mut self,
        seq: KeySequence,
        target: KeySequence,
    ) -> Result<(), std::io::Error> {
        self.try_unbind(seq.clone());
        self.macros.insert(seq, target);
        Ok(())
    }

    fn get_macros(&self) -> HashMap<KeySequence, KeySequence> {
        self.macros.clone()
    }
}

fn translate_key_sequence_to_reedline(
    seq: &KeySequence,
) -> Option<(reedline::KeyModifiers, reedline::KeyCode)> {
    let KeySequence::Strokes(strokes) = seq else {
        // TODO(input): handle other kinds of key sequences
        return None;
    };

    let [stroke] = &strokes.as_slice() else {
        // TODO(input): handle multiple strokes
        return None;
    };

    Some(translate_key_stroke(stroke))
}

pub(super) fn translate_key_stroke(
    stroke: &KeyStroke,
) -> (reedline::KeyModifiers, reedline::KeyCode) {
    let mut modifiers = reedline::KeyModifiers::empty();
    modifiers.set(reedline::KeyModifiers::ALT, stroke.alt);
    modifiers.set(reedline::KeyModifiers::CONTROL, stroke.control);
    modifiers.set(
        reedline::KeyModifiers::SHIFT,
        stroke.shift || stroke.key == Key::BackTab,
    );

    let key_code = match stroke.key {
        Key::Character(c) => reedline::KeyCode::Char(c),
        Key::Backspace => reedline::KeyCode::Backspace,
        Key::Enter => reedline::KeyCode::Enter,
        Key::Left => reedline::KeyCode::Left,
        Key::Right => reedline::KeyCode::Right,
        Key::Up => reedline::KeyCode::Up,
        Key::Down => reedline::KeyCode::Down,
        Key::Home => reedline::KeyCode::Home,
        Key::End => reedline::KeyCode::End,
        Key::PageUp => reedline::KeyCode::PageUp,
        Key::PageDown => reedline::KeyCode::PageDown,
        Key::Tab => reedline::KeyCode::Tab,
        Key::BackTab => reedline::KeyCode::BackTab,
        Key::Delete => reedline::KeyCode::Delete,
        Key::Insert => reedline::KeyCode::Insert,
        Key::F(n) => reedline::KeyCode::F(n),
        Key::Escape => reedline::KeyCode::Esc,
    };

    (modifiers, key_code)
}

fn translate_action_to_reedline_event(action: &KeyAction) -> Option<reedline::ReedlineEvent> {
    match action {
        KeyAction::ShellCommand(cmd) => {
            Some(reedline::ReedlineEvent::ExecuteHostCommand(cmd.to_owned()))
        }
        KeyAction::DoInputFunction(func) => translate_input_function_to_reedline_event(func),
        KeyAction::Sequence(actions) => {
            // Convert each action in the sequence to a reedline event.
            let events: Vec<_> = actions
                .iter()
                .filter_map(translate_action_to_reedline_event)
                .collect();

            if events.is_empty() {
                None
            } else if events.len() == 1 {
                events.into_iter().next()
            } else {
                Some(reedline::ReedlineEvent::Multiple(events))
            }
        }
    }
}

fn translate_input_function_to_reedline_event(
    func: &InputFunction,
) -> Option<reedline::ReedlineEvent> {
    use reedline::{EditCommand, ReedlineEvent};

    if let Some(command) = keyboard_macro::Command::for_function(func) {
        return Some(ReedlineEvent::ExecuteHostCommand(command.marker().into()));
    }
    if let Some(action) = Action::for_function(func) {
        return Some(action.event());
    }
    match func {
        InputFunction::BackwardDeleteChar => {
            Some(ReedlineEvent::Edit(vec![EditCommand::Backspace]))
        }
        InputFunction::BackwardKillWord => {
            Some(ReedlineEvent::Edit(vec![EditCommand::CutWordLeft]))
        }
        InputFunction::UnixWordRubout => {
            Some(ReedlineEvent::Edit(vec![EditCommand::CutBigWordLeft]))
        }
        InputFunction::QuotedInsert => Some(ReedlineEvent::ExecuteHostCommand(
            keymaps::QUOTED_INSERT.into(),
        )),
        InputFunction::KillLine => Some(ReedlineEvent::Edit(vec![EditCommand::KillLine])),
        InputFunction::KillWholeLine => Some(ReedlineEvent::Edit(vec![EditCommand::CutFromStart])),
        InputFunction::KillWord => Some(ReedlineEvent::Edit(vec![EditCommand::CutWordRight])),
        InputFunction::DeleteChar => Some(ReedlineEvent::Edit(vec![EditCommand::Delete])),
        InputFunction::DowncaseWord => Some(ReedlineEvent::Edit(vec![EditCommand::LowercaseWord])),
        InputFunction::BackwardChar => Some(ReedlineEvent::Edit(vec![EditCommand::MoveLeft {
            select: false,
        }])),
        InputFunction::ForwardChar => Some(ReedlineEvent::Edit(vec![EditCommand::MoveRight {
            select: false,
        }])),
        InputFunction::EndOfLine => Some(ReedlineEvent::Edit(vec![EditCommand::MoveToLineEnd {
            select: false,
        }])),
        InputFunction::BeginningOfLine => {
            Some(ReedlineEvent::Edit(vec![EditCommand::MoveToLineStart {
                select: false,
            }]))
        }
        InputFunction::BackwardWord => Some(ReedlineEvent::Edit(vec![EditCommand::MoveWordLeft {
            select: false,
        }])),
        InputFunction::ForwardWord => Some(ReedlineEvent::Edit(vec![EditCommand::MoveWordRight {
            select: false,
        }])),
        InputFunction::Yank => Some(ReedlineEvent::Edit(vec![EditCommand::PasteCutBufferAfter])),
        InputFunction::ViRedo => Some(ReedlineEvent::Edit(vec![EditCommand::Redo])),
        InputFunction::TransposeChars => {
            Some(ReedlineEvent::Edit(vec![EditCommand::SwapGraphemes]))
        }
        InputFunction::UpcaseWord => Some(ReedlineEvent::Edit(vec![EditCommand::UppercaseWord])),
        InputFunction::Undo => Some(ReedlineEvent::Edit(vec![EditCommand::Undo])),
        InputFunction::ClearScreen => Some(ReedlineEvent::ClearScreen),
        InputFunction::AcceptLine => Some(ReedlineEvent::Enter),
        InputFunction::ReReadInitFile => Some(ReedlineEvent::ExecuteHostCommand(
            super::input_backend::RELOAD_INPUTRC.into(),
        )),
        InputFunction::Abort => Some(ReedlineEvent::Esc),
        InputFunction::TabInsert => Some(ReedlineEvent::Edit(vec![EditCommand::InsertChar('\t')])),
        InputFunction::PreviousHistory => Some(ReedlineEvent::PreviousHistory),
        InputFunction::NextHistory => Some(ReedlineEvent::NextHistory),
        InputFunction::BeginningOfHistory => Some(ReedlineEvent::FirstHistory),
        InputFunction::EndOfHistory => Some(ReedlineEvent::LeaveHistory),
        InputFunction::PreviousScreenLine => Some(ReedlineEvent::Up),
        InputFunction::NextScreenLine => Some(ReedlineEvent::Down),
        InputFunction::BackwardKillLine | InputFunction::UnixLineDiscard => {
            Some(ReedlineEvent::Edit(vec![EditCommand::CutFromLineStart]))
        }
        InputFunction::ViEditingMode => Some(ReedlineEvent::SwitchMode(
            reedline::PromptEditMode::Vi(reedline::PromptViMode::Insert),
        )),
        InputFunction::EmacsEditingMode => {
            Some(ReedlineEvent::SwitchMode(reedline::PromptEditMode::Emacs))
        }
        InputFunction::ReverseSearchHistory => Some(ReedlineEvent::SearchHistory),
        InputFunction::RedrawCurrentLine => Some(ReedlineEvent::Repaint),
        InputFunction::BrushAcceptHint => Some(ReedlineEvent::HistoryHintComplete),
        InputFunction::BrushAcceptHintWord => Some(ReedlineEvent::HistoryHintWordComplete),
        _ => None,
    }
}

const fn translate_reedline_keycode(keycode: reedline::KeyCode) -> Option<Key> {
    match keycode {
        reedline::KeyCode::Backspace => Some(Key::Backspace),
        reedline::KeyCode::Enter => Some(Key::Enter),
        reedline::KeyCode::Left => Some(Key::Left),
        reedline::KeyCode::Right => Some(Key::Right),
        reedline::KeyCode::Up => Some(Key::Up),
        reedline::KeyCode::Down => Some(Key::Down),
        reedline::KeyCode::Home => Some(Key::Home),
        reedline::KeyCode::End => Some(Key::End),
        reedline::KeyCode::PageUp => Some(Key::PageUp),
        reedline::KeyCode::PageDown => Some(Key::PageDown),
        reedline::KeyCode::Tab => Some(Key::Tab),
        reedline::KeyCode::BackTab => Some(Key::BackTab),
        reedline::KeyCode::Delete => Some(Key::Delete),
        reedline::KeyCode::Insert => Some(Key::Insert),
        reedline::KeyCode::F(n) => Some(Key::F(n)),
        reedline::KeyCode::Char(c) => Some(Key::Character(c)),
        reedline::KeyCode::Null => None,
        reedline::KeyCode::Esc => Some(Key::Escape),
        reedline::KeyCode::CapsLock => None,
        reedline::KeyCode::ScrollLock => None,
        reedline::KeyCode::NumLock => None,
        reedline::KeyCode::PrintScreen => None,
        reedline::KeyCode::Pause => None,
        reedline::KeyCode::Menu => None,
        reedline::KeyCode::KeypadBegin => None,
        reedline::KeyCode::Media(_media_key_code) => None,
        reedline::KeyCode::Modifier(_modifier_key_code) => None,
    }
}

#[expect(clippy::too_many_lines)]
fn translate_reedline_event_to_action(event: &reedline::ReedlineEvent) -> Option<KeyAction> {
    if matches!(event, reedline::ReedlineEvent::ExecuteHostCommand(marker) if marker == keymaps::QUOTED_INSERT)
    {
        return Some(KeyAction::DoInputFunction(InputFunction::QuotedInsert));
    }
    if let Some(command) = keyboard_macro::Command::from_event(event) {
        return Some(KeyAction::DoInputFunction(command.function()));
    }
    match event {
        reedline::ReedlineEvent::Edit(cmds) => {
            match cmds.as_slice() {
                [reedline::EditCommand::Backspace] => Some(KeyAction::DoInputFunction(
                    InputFunction::BackwardDeleteChar,
                )),
                [reedline::EditCommand::BackspaceWord] => {
                    // Not quite accurate, because it doesn't save the deleted text.
                    Some(KeyAction::DoInputFunction(InputFunction::BackwardKillWord))
                }
                [reedline::EditCommand::CapitalizeChar] => None,
                [reedline::EditCommand::ClearToLineEnd] => {
                    // Not quite accurate, because it doesn't save the deleted text.
                    Some(KeyAction::DoInputFunction(InputFunction::KillLine))
                }
                [reedline::EditCommand::Complete] => {
                    Some(KeyAction::DoInputFunction(InputFunction::Complete))
                }
                [reedline::EditCommand::CutFromStart] => {
                    Some(KeyAction::DoInputFunction(InputFunction::KillWholeLine))
                }
                [reedline::EditCommand::KillLine] => {
                    Some(KeyAction::DoInputFunction(InputFunction::KillLine))
                }
                [reedline::EditCommand::CutWordLeft] => {
                    Some(KeyAction::DoInputFunction(InputFunction::BackwardKillWord))
                }
                [reedline::EditCommand::CutWordRight] => {
                    Some(KeyAction::DoInputFunction(InputFunction::KillWord))
                }
                [reedline::EditCommand::Delete] => {
                    Some(KeyAction::DoInputFunction(InputFunction::DeleteChar))
                }
                [reedline::EditCommand::DeleteWord] => {
                    Some(KeyAction::DoInputFunction(InputFunction::KillWord))
                }
                [reedline::EditCommand::InsertNewline] => None,
                [reedline::EditCommand::LowercaseWord] => {
                    Some(KeyAction::DoInputFunction(InputFunction::DowncaseWord))
                }
                [reedline::EditCommand::MoveLeft { select: false }] => {
                    Some(KeyAction::DoInputFunction(InputFunction::BackwardChar))
                }
                [reedline::EditCommand::MoveLeft { select: true }] => None,
                [reedline::EditCommand::MoveRight { select: false }] => {
                    Some(KeyAction::DoInputFunction(InputFunction::ForwardChar))
                }
                [reedline::EditCommand::MoveRight { select: true }] => None,
                [reedline::EditCommand::MoveToEnd { select: false }] => {
                    // TODO(input): Not quite accurate, because it doesn't just go to end of line.
                    Some(KeyAction::DoInputFunction(InputFunction::EndOfLine))
                }
                [reedline::EditCommand::MoveToEnd { select: true }] => None,
                [reedline::EditCommand::MoveToLineEnd { select: false }] => {
                    Some(KeyAction::DoInputFunction(InputFunction::EndOfLine))
                }
                [reedline::EditCommand::MoveToLineEnd { select: true }] => None,
                [reedline::EditCommand::MoveToLineStart { select: false }] => {
                    Some(KeyAction::DoInputFunction(InputFunction::BeginningOfLine))
                }
                [reedline::EditCommand::MoveToLineStart { select: true }] => None,
                [reedline::EditCommand::MoveToStart { select: false }] => {
                    // TODO(input): Not quite accurate, because it doesn't just go to beginning of
                    // line.
                    Some(KeyAction::DoInputFunction(InputFunction::BeginningOfLine))
                }
                [reedline::EditCommand::MoveToStart { select: true }] => None,
                [reedline::EditCommand::MoveWordLeft { select: false }] => {
                    Some(KeyAction::DoInputFunction(InputFunction::BackwardWord))
                }
                [reedline::EditCommand::MoveWordLeft { select: true }] => None,
                [reedline::EditCommand::MoveWordRight { select: false }] => {
                    Some(KeyAction::DoInputFunction(InputFunction::ForwardWord))
                }
                [reedline::EditCommand::MoveWordRight { select: true }] => None,
                [reedline::EditCommand::PasteCutBufferAfter] => {
                    Some(KeyAction::DoInputFunction(InputFunction::Yank))
                }
                [reedline::EditCommand::PasteCutBufferBefore] => None,
                [reedline::EditCommand::Redo] => {
                    Some(KeyAction::DoInputFunction(InputFunction::ViRedo))
                }
                [reedline::EditCommand::SelectAll] => None,
                [reedline::EditCommand::SwapGraphemes] => {
                    Some(KeyAction::DoInputFunction(InputFunction::TransposeChars))
                }
                [reedline::EditCommand::UppercaseWord] => {
                    Some(KeyAction::DoInputFunction(InputFunction::UpcaseWord))
                }
                [reedline::EditCommand::Undo] => {
                    Some(KeyAction::DoInputFunction(InputFunction::Undo))
                }
                _ => {
                    // TODO(input): Handle more?
                    tracing::debug!(target: trace_categories::INPUT, "unhandled edit commands: {cmds:?}");
                    None
                }
            }
        }
        reedline::ReedlineEvent::ClearScreen => {
            Some(KeyAction::DoInputFunction(InputFunction::ClearScreen))
        }
        reedline::ReedlineEvent::CtrlC => None,
        reedline::ReedlineEvent::CtrlD => None,
        reedline::ReedlineEvent::Enter => {
            Some(KeyAction::DoInputFunction(InputFunction::AcceptLine))
        }
        reedline::ReedlineEvent::Esc => None,
        reedline::ReedlineEvent::MenuPrevious => None,
        reedline::ReedlineEvent::OpenEditor => None,
        reedline::ReedlineEvent::Left => {
            Some(KeyAction::DoInputFunction(InputFunction::BackwardChar))
        }
        reedline::ReedlineEvent::Right => {
            Some(KeyAction::DoInputFunction(InputFunction::ForwardChar))
        }
        reedline::ReedlineEvent::Up => Some(KeyAction::DoInputFunction(
            InputFunction::PreviousScreenLine,
        )),
        reedline::ReedlineEvent::Down => {
            Some(KeyAction::DoInputFunction(InputFunction::NextScreenLine))
        }
        reedline::ReedlineEvent::SearchHistory => Some(KeyAction::DoInputFunction(
            InputFunction::ReverseSearchHistory,
        )),
        reedline::ReedlineEvent::Repaint => {
            Some(KeyAction::DoInputFunction(InputFunction::RedrawCurrentLine))
        }
        reedline::ReedlineEvent::HistoryHintComplete => {
            Some(KeyAction::DoInputFunction(InputFunction::BrushAcceptHint))
        }
        reedline::ReedlineEvent::HistoryHintWordComplete => Some(KeyAction::DoInputFunction(
            InputFunction::BrushAcceptHintWord,
        )),
        reedline::ReedlineEvent::Multiple(evts) => {
            if let Some(function) = Action::from_events(evts) {
                return Some(KeyAction::DoInputFunction(function));
            }
            if let &[
                reedline::ReedlineEvent::Edit(ref edit_cmds),
                reedline::ReedlineEvent::Enter,
            ] = evts.as_slice()
            {
                if let &[
                    reedline::EditCommand::MoveToStart { select: false },
                    reedline::EditCommand::InsertChar('#'),
                ] = edit_cmds.as_slice()
                {
                    return Some(KeyAction::DoInputFunction(InputFunction::InsertComment));
                }
            }

            // TODO(input): Try to extract something from these?
            tracing::debug!(target: trace_categories::INPUT, "unhandled composite event: {evts:?}");
            None
        }
        reedline::ReedlineEvent::UntilFound(uf_events) => {
            let mut i = 0;

            if uf_events.is_empty() {
                return None;
            }

            while i < uf_events.len() {
                match &uf_events[i] {
                    reedline::ReedlineEvent::HistoryHintComplete
                    | reedline::ReedlineEvent::HistoryHintWordComplete
                    | reedline::ReedlineEvent::Menu(_)
                    | reedline::ReedlineEvent::MenuDown
                    | reedline::ReedlineEvent::MenuUp
                    | reedline::ReedlineEvent::MenuLeft
                    | reedline::ReedlineEvent::MenuRight
                    | reedline::ReedlineEvent::MenuNext
                    | reedline::ReedlineEvent::MenuPrevious
                    | reedline::ReedlineEvent::MenuPageNext
                    | reedline::ReedlineEvent::MenuPagePrevious => {
                        i += 1;
                    }
                    _ => {
                        break;
                    }
                }
            }

            if i == uf_events.len() - 1 {
                translate_reedline_event_to_action(&uf_events[i])
            } else {
                // TODO(input): Try to extract something from these?
                tracing::debug!(target: trace_categories::INPUT, "unhandled until-found event: {uf_events:?}");
                None
            }
        }
        reedline::ReedlineEvent::ExecuteHostCommand(cmd) => {
            Some(if cmd == super::input_backend::RELOAD_INPUTRC {
                KeyAction::DoInputFunction(InputFunction::ReReadInitFile)
            } else {
                KeyAction::ShellCommand(cmd.to_owned())
            })
        }
        evt => {
            // TODO(input): Handle more?
            tracing::debug!(target: trace_categories::INPUT, "unhandled event: {evt:?}");
            None
        }
    }
}
