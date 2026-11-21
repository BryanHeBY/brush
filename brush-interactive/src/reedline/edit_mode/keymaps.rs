use brush_core::interfaces::{
    InputFunction, Key, KeyAction, KeyBindings, KeySequence, KeyStroke, ReadlineKeymap,
};
use reedline::{
    EditMode, EventStatus, PromptEditMode, PromptViMode, ReedlineEvent, ReedlineRawEvent,
};

use radix_trie::TrieCommon as _;

use super::super::terminal_input::{byte_key, raw_byte, raw_stroke, stroke_bytes};

use super::keyboard_macro::{Command, KeyboardMacro};
use super::{UpdatableBindings, translate_action_to_reedline_event, translate_reedline_keycode};

use std::{
    collections::HashMap,
    time::{Duration, Instant},
};

pub(super) const QUOTED_INSERT: &str = "\0brush-quoted-insert";

pub(crate) struct BindingMaps {
    emacs: UpdatableBindings,
    vi_insert: UpdatableBindings,
    vi_command: UpdatableBindings,
    vi: reedline::Vi,
    selected: ReadlineKeymap,
    vi_enabled: bool,
    reported_mode: PromptEditMode,
    last_shell_vi: bool,
    rebuild_vi: bool,
    chord: Vec<KeyStroke>,
    chord_updated_at: Option<Instant>,
    chord_timeout: Duration,
    keyboard_macro: KeyboardMacro,
    trigger_len: usize,
    argument_count: Option<usize>,
    terminal_keys: Vec<(u8, Option<InputFunction>)>,
    quote_next: bool,
    byte_oriented: bool,
}

impl BindingMaps {
    pub(super) fn new(emacs: reedline::Keybindings) -> Self {
        let mut emacs = UpdatableBindings::new(emacs);
        emacs.sequences.insert(
            vec![
                control_x(),
                KeyStroke {
                    control: true,
                    ..KeyStroke::from(Key::Character('r'))
                },
            ],
            KeyAction::DoInputFunction(InputFunction::ReReadInitFile),
        );
        for (character, command) in [
            ('(', Command::Start),
            (')', Command::End),
            ('e', Command::Call),
        ] {
            emacs.sequences.insert(
                vec![control_x(), KeyStroke::from(Key::Character(character))],
                KeyAction::DoInputFunction(command.function()),
            );
        }
        Self {
            emacs,
            vi_insert: UpdatableBindings::new(reedline::default_vi_insert_keybindings()),
            vi_command: UpdatableBindings::new(reedline::default_vi_normal_keybindings()),
            vi: reedline::Vi::default(),
            selected: ReadlineKeymap::Emacs,
            vi_enabled: false,
            reported_mode: PromptEditMode::Emacs,
            last_shell_vi: false,
            rebuild_vi: false,
            chord: Vec::new(),
            chord_updated_at: None,
            chord_timeout: Duration::from_millis(500),
            keyboard_macro: KeyboardMacro::default(),
            trigger_len: 1,
            argument_count: None,
            terminal_keys: Vec::new(),
            quote_next: false,
            byte_oriented: false,
        }
    }

    pub(super) fn sync_vi_editing(&mut self, enabled: bool) {
        if self.vi_enabled != enabled {
            self.change_mode(enabled);
        }
    }

    pub(crate) fn sync_shell_mode(&mut self, enabled: bool) {
        if self.last_shell_vi != enabled {
            self.last_shell_vi = enabled;
            self.sync_vi_editing(enabled);
        }
    }

    pub(crate) const fn set_chord_timeout(&mut self, millis: u64) {
        self.chord_timeout = Duration::from_millis(millis);
    }

    fn change_mode(&mut self, enabled: bool) {
        if self.vi_enabled != enabled {
            self.vi_enabled = enabled;
            self.rebuild_vi = enabled;
            self.chord.clear();
            self.reported_mode = if enabled {
                PromptEditMode::Vi(PromptViMode::Insert)
            } else {
                PromptEditMode::Emacs
            };
        }
        self.selected = if enabled {
            ReadlineKeymap::ViInsert
        } else {
            ReadlineKeymap::Emacs
        };
    }

    const fn selected(&self) -> &UpdatableBindings {
        match self.selected {
            ReadlineKeymap::ViCommand => &self.vi_command,
            ReadlineKeymap::ViInsert => &self.vi_insert,
            _ => &self.emacs,
        }
    }

    const fn selected_mut(&mut self) -> &mut UpdatableBindings {
        match self.selected {
            ReadlineKeymap::ViCommand => {
                self.rebuild_vi = true;
                &mut self.vi_command
            }
            ReadlineKeymap::ViInsert => {
                self.rebuild_vi = true;
                &mut self.vi_insert
            }
            _ => &mut self.emacs,
        }
    }

    pub(crate) fn set_terminal_options(&mut self, options: &brush_core::ReadlineOptions) -> bool {
        self.terminal_keys = brush_core::sys::input::readline_terminal_keys();
        if !options.bind_tty_special_chars {
            self.terminal_keys
                .retain(|(_, function)| function.is_none());
        }
        self.byte_oriented = options.byte_oriented;
        self.byte_oriented
            || [&self.emacs, &self.vi_insert, &self.vi_command]
                .into_iter()
                .any(|bindings| bindings.has_byte_chords())
    }

    pub(super) const fn take_macro_output(&mut self) -> Option<String> {
        self.keyboard_macro.output.take()
    }

    pub(super) fn take_macro_bell(&mut self) -> bool {
        std::mem::take(&mut self.keyboard_macro.bell_requested)
    }

    fn chord_event(&mut self, event: &crossterm::event::Event) -> Option<ReedlineEvent> {
        let crossterm::event::Event::Key(key) = event else {
            return None;
        };
        let mapped = translate_reedline_keycode(key.code)?;
        let raw = raw_byte(*key);
        let stroke = raw.map_or_else(
            || KeyStroke {
                alt: key.modifiers.contains(reedline::KeyModifiers::ALT),
                control: key.modifiers.contains(reedline::KeyModifiers::CONTROL),
                shift: !matches!(mapped, Key::Character(_) | Key::BackTab)
                    && key.modifiers.contains(reedline::KeyModifiers::SHIFT),
                key: mapped,
            },
            raw_stroke,
        );
        self.keyboard_macro.record(&stroke);
        let bindings = if self.vi_enabled {
            if self.vi.edit_mode() == reedline::PromptEditMode::Vi(reedline::PromptViMode::Insert) {
                &self.vi_insert
            } else {
                &self.vi_command
            }
        } else {
            &self.emacs
        };
        let bytes = stroke_bytes(&stroke);
        if std::mem::take(&mut self.quote_next) {
            return Some(ReedlineEvent::Edit(vec![
                reedline::EditCommand::InsertString(String::from_utf8_lossy(&bytes).into_owned()),
            ]));
        }
        let stroke_is_unconfigured = !bindings
            .configured
            .contains(&KeySequence::from(stroke.clone()));
        if let Some((_, function)) = self.terminal_keys.iter().find(|(byte, _)| bytes == [*byte]) {
            if function.is_none() {
                self.chord.clear();
                return Some(ReedlineEvent::CtrlC);
            }
            if stroke_is_unconfigured
                && self.chord.is_empty()
                && let Some(function) = function
            {
                return translate_action_to_reedline_event(&KeyAction::DoInputFunction(
                    function.clone(),
                ));
            }
        }
        if !self.vi_enabled
            && self.chord.is_empty()
            && stroke_is_unconfigured
            && !stroke.control
            && let Key::Character(character) = stroke.key
            && let Some(digit) = character.to_digit(10)
            && (stroke.alt || self.argument_count.is_some())
        {
            let count = self.argument_count.unwrap_or_default();
            self.argument_count = Some((count * 10 + digit as usize).min(65_536));
            return Some(ReedlineEvent::None);
        }
        let now = Instant::now();
        if !self.chord_timeout.is_zero()
            && self
                .chord_updated_at
                .is_some_and(|then| now.duration_since(then) > self.chord_timeout)
        {
            self.chord.clear();
        }
        self.chord_updated_at = Some(now);
        self.chord.push(stroke);
        self.trigger_len = self.chord.len();
        match bindings.match_chord(&self.chord) {
            ChordMatch::Macro(sequence) => {
                self.keyboard_macro.replay_sequence(sequence);
                self.argument_count = None;
                self.chord.clear();
                return Some(ReedlineEvent::None);
            }
            ChordMatch::Action(action) => {
                self.chord.clear();
                return translate_action_to_reedline_event(action);
            }
            ChordMatch::Prefix => return Some(ReedlineEvent::None),
            ChordMatch::Unbound => {}
        }
        self.chord.clear();
        self.trigger_len = 1;
        raw.map(|byte| self.unbound_terminal_byte(byte))
    }

    fn unbound_terminal_byte(&self, byte: u8) -> ReedlineEvent {
        if byte.is_ascii() {
            return ReedlineEvent::None;
        }
        let character = if self.byte_oriented {
            byte_key(byte)
        } else {
            char::REPLACEMENT_CHARACTER
        };
        ReedlineEvent::Edit(vec![reedline::EditCommand::InsertChar(character)])
    }
}

enum ChordMatch<'a> {
    Macro(&'a KeySequence),
    Action(&'a KeyAction),
    Prefix,
    Unbound,
}

impl UpdatableBindings {
    fn has_byte_chords(&self) -> bool {
        !self.raw_mappings.is_empty()
            || self
                .macros
                .keys()
                .any(|seq| matches!(seq, KeySequence::Bytes(_)))
    }

    fn match_chord(&self, strokes: &[KeyStroke]) -> ChordMatch<'_> {
        let bytes = if self.has_byte_chords() {
            strokes.iter().flat_map(stroke_bytes).collect::<Vec<_>>()
        } else {
            Vec::new()
        };
        let mut found_prefix = false;
        for (sequence, target) in &self.macros {
            match sequence {
                KeySequence::Strokes(keys) => {
                    if keys == strokes {
                        return ChordMatch::Macro(target);
                    }
                    found_prefix |= keys.starts_with(strokes);
                }
                KeySequence::Bytes(items) => {
                    let mut keys = items.iter().flatten().copied();
                    if !bytes.is_empty()
                        && keys.by_ref().take(bytes.len()).eq(bytes.iter().copied())
                    {
                        if keys.next().is_none() {
                            return ChordMatch::Macro(target);
                        }
                        found_prefix = true;
                    }
                }
            }
        }
        if let Some(action) = self
            .sequences
            .get(strokes)
            .or_else(|| self.raw_mappings.get(&bytes))
        {
            return ChordMatch::Action(action);
        }
        if found_prefix
            || self.sequences.keys().any(|seq| seq.starts_with(strokes))
            || (!bytes.is_empty() && self.raw_mappings.get_raw_descendant(&bytes).is_some())
        {
            ChordMatch::Prefix
        } else {
            ChordMatch::Unbound
        }
    }
}

impl EditMode for BindingMaps {
    fn next_replay_event(&mut self) -> Option<ReedlineRawEvent> {
        self.keyboard_macro.next_event()
    }

    fn parse_event(&mut self, event: ReedlineRawEvent) -> ReedlineEvent {
        self.trigger_len = 1;
        if self.vi_enabled && self.rebuild_vi {
            self.vi = reedline::Vi::new(
                self.vi_insert.bindings.clone(),
                self.vi_command.bindings.clone(),
                reedline::default_vi_visual_keybindings(),
            );
            self.rebuild_vi = false;
        }

        let before = if self.vi_enabled {
            self.vi.edit_mode()
        } else {
            PromptEditMode::Emacs
        };
        let mut event: crossterm::event::Event = event.into();
        if let crossterm::event::Event::Paste(text) = &event {
            self.keyboard_macro.record_paste(text);
        }
        // Crossterm folds C-\, C-], C-^ and C-_ into C-4 through C-7.
        // Normalize those bytes before both chord matching and editor dispatch.
        if let crossterm::event::Event::Key(key) = &mut event
            && key.modifiers.contains(reedline::KeyModifiers::CONTROL)
            && let reedline::KeyCode::Char(c @ '4'..='7') = key.code
        {
            key.code = reedline::KeyCode::Char(char::from(b'\\' + (c as u8 - b'4')));
        }
        let result = self.chord_event(&event).unwrap_or_else(|| {
            event.try_into().ok().map_or(ReedlineEvent::None, |event| {
                if self.vi_enabled {
                    self.vi.parse_event(event)
                } else {
                    self.emacs.parse_event(event)
                }
            })
        });
        if matches!(&result, ReedlineEvent::ExecuteHostCommand(marker) if marker == QUOTED_INSERT) {
            self.quote_next = true;
            return ReedlineEvent::None;
        }
        if let Some(command) = Command::from_event(&result) {
            let repeat = self.argument_count.take().unwrap_or(1);
            self.keyboard_macro
                .execute(command, self.trigger_len, repeat);
            return ReedlineEvent::Repaint;
        }
        if result == ReedlineEvent::CtrlC {
            self.keyboard_macro.interrupt();
        }
        if result != ReedlineEvent::None {
            self.argument_count = None;
        }
        if let ReedlineEvent::SwitchMode(mode @ (PromptEditMode::Emacs | PromptEditMode::Vi(_))) =
            &result
        {
            let reported_mode = self.reported_mode.clone();
            self.sync_vi_editing(matches!(mode, PromptEditMode::Vi(_)));
            self.reported_mode = reported_mode;
            if self.vi_enabled && !self.rebuild_vi {
                self.vi.handle_mode_specific_event(result.clone());
            }
            return result;
        }
        if self.vi_enabled {
            let after = self.vi.edit_mode();
            if before != after {
                // Apply the Vi mode change at the same point as the key's edit.
                return ReedlineEvent::Multiple(vec![ReedlineEvent::SwitchMode(after), result]);
            }
        }
        result
    }

    fn edit_mode(&self) -> PromptEditMode {
        self.reported_mode.clone()
    }

    fn handle_mode_specific_event(&mut self, event: ReedlineEvent) -> EventStatus {
        if let ReedlineEvent::SwitchMode(mode @ (PromptEditMode::Emacs | PromptEditMode::Vi(_))) =
            event
        {
            self.reported_mode = mode;
            EventStatus::Handled
        } else {
            EventStatus::Inapplicable
        }
    }
}

impl KeyBindings for BindingMaps {
    fn vi_editing(&self) -> Option<bool> {
        Some(self.vi_enabled)
    }

    fn keymap(&self) -> ReadlineKeymap {
        self.selected
    }

    fn select_keymap(&mut self, map: ReadlineKeymap) -> Result<(), std::io::Error> {
        self.selected = map;
        Ok(())
    }

    fn set_vi_editing(&mut self, enabled: bool) -> Result<(), std::io::Error> {
        self.change_mode(enabled);
        Ok(())
    }

    fn get_current(&self) -> HashMap<KeySequence, KeyAction> {
        self.selected()
            .get_current()
            .into_iter()
            .filter_map(|(key, action)| {
                from_backend_key(self.selected, key).map(|key| (key, action))
            })
            .collect()
    }

    fn get_untranslated(&self, bytes: &[u8]) -> Option<&KeyAction> {
        self.selected().get_untranslated(bytes)
    }

    fn bind(&mut self, seq: KeySequence, action: KeyAction) -> Result<(), std::io::Error> {
        let seq = to_backend_key(self.selected, seq);
        self.selected_mut().bind(seq, action)
    }

    fn try_unbind(&mut self, seq: KeySequence) -> bool {
        let seq = to_backend_key(self.selected, seq);
        self.selected_mut().try_unbind(seq)
    }

    fn define_macro(
        &mut self,
        seq: KeySequence,
        target: KeySequence,
    ) -> Result<(), std::io::Error> {
        let seq = to_backend_key(self.selected, seq);
        self.selected_mut().define_macro(seq, target)
    }

    fn get_macros(&self) -> HashMap<KeySequence, KeySequence> {
        self.selected()
            .get_macros()
            .into_iter()
            .filter_map(|(key, action)| {
                from_backend_key(self.selected, key).map(|key| (key, action))
            })
            .collect()
    }
}

fn control_x() -> KeyStroke {
    KeyStroke {
        control: true,
        ..KeyStroke::from(Key::Character('x'))
    }
}

fn to_backend_key(map: ReadlineKeymap, key: KeySequence) -> KeySequence {
    let KeySequence::Strokes(mut strokes) = key else {
        return key;
    };
    match map {
        ReadlineKeymap::EmacsMeta => {
            if let Some(first) = strokes.first_mut() {
                first.alt = true;
            }
        }
        ReadlineKeymap::EmacsCtlx => strokes.insert(0, control_x()),
        _ => {}
    }
    KeySequence::Strokes(strokes)
}

fn from_backend_key(map: ReadlineKeymap, key: KeySequence) -> Option<KeySequence> {
    let KeySequence::Strokes(mut strokes) = key else {
        return (!matches!(map, ReadlineKeymap::EmacsMeta | ReadlineKeymap::EmacsCtlx))
            .then_some(key);
    };
    match map {
        ReadlineKeymap::EmacsMeta => {
            let first = strokes.first_mut().filter(|stroke| stroke.alt)?;
            first.alt = false;
        }
        ReadlineKeymap::EmacsCtlx => {
            strokes = strokes.strip_prefix(&[control_x()])?.to_vec();
            if strokes.is_empty() {
                return None;
            }
        }
        _ => {}
    }
    Some(KeySequence::Strokes(strokes))
}
