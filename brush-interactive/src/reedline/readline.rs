//! Readline actions that need access to the live editing buffer.

mod listings;

use std::sync::{Arc, Mutex, MutexGuard, PoisonError};

use brush_core::{ReadlineBellStyle, ReadlineOptions, history, interfaces::InputFunction};
use crossterm::event::{Event, KeyEvent};
use reedline::{KeyCode, KeyModifiers, ReedlineEvent};

use crate::{completion, refs};

#[derive(Clone, Default)]
pub(super) struct Session(Arc<Mutex<ReadlineSession>>);

impl Session {
    pub(super) fn lock(&self) -> MutexGuard<'_, ReadlineSession> {
        self.0.lock().unwrap_or_else(PoisonError::into_inner)
    }
}

pub(super) struct HistoryHinter {
    inner: reedline::DefaultHinter,
    session: Session,
    enabled: bool,
}

impl HistoryHinter {
    pub(super) fn new(session: Session, style: nu_ansi_term::Style) -> Self {
        Self {
            inner: reedline::DefaultHinter::default().with_style(style),
            session,
            enabled: false,
        }
    }
}

impl reedline::Hinter for HistoryHinter {
    fn handle(
        &mut self,
        line: &str,
        pos: usize,
        history: &dyn reedline::History,
        use_ansi_coloring: bool,
        cwd: &str,
    ) -> String {
        // Readline history actions do not enter Reedline's history mode.
        self.enabled = pos == line.len()
            && self
                .session
                .lock()
                .history
                .as_ref()
                .is_none_or(|search| search.last.text != line);
        if self.enabled {
            self.inner
                .handle(line, pos, history, use_ansi_coloring, cwd)
        } else {
            String::new()
        }
    }

    fn complete_hint(&self) -> String {
        if self.enabled {
            self.inner.complete_hint()
        } else {
            String::new()
        }
    }

    fn next_hint_token(&self) -> String {
        if self.enabled {
            self.inner.next_hint_token()
        } else {
            String::new()
        }
    }
}

#[derive(Clone, Copy)]
pub(super) enum Action {
    HistoryBackward,
    HistoryForward,
    PreviousHistory,
    NextHistory,
    Complete,
    ListCompletions,
    MenuForward,
    MenuBackward,
}

impl Action {
    const ALL: [Self; 8] = [
        Self::HistoryBackward,
        Self::HistoryForward,
        Self::PreviousHistory,
        Self::NextHistory,
        Self::Complete,
        Self::ListCompletions,
        Self::MenuForward,
        Self::MenuBackward,
    ];

    const fn name(self) -> &'static str {
        match self {
            Self::HistoryBackward => "brush_history_search_backward",
            Self::HistoryForward => "brush_history_search_forward",
            Self::PreviousHistory => "brush_previous_history",
            Self::NextHistory => "brush_next_history",
            Self::Complete => "brush_complete",
            Self::ListCompletions => "brush_list_completions",
            Self::MenuForward => "brush_menu_complete",
            Self::MenuBackward => "brush_menu_complete_backward",
        }
    }

    const fn function(self) -> InputFunction {
        match self {
            Self::HistoryBackward => InputFunction::HistorySearchBackward,
            Self::HistoryForward => InputFunction::HistorySearchForward,
            Self::PreviousHistory => InputFunction::PreviousHistory,
            Self::NextHistory => InputFunction::NextHistory,
            Self::Complete => InputFunction::Complete,
            Self::ListCompletions => InputFunction::PossibleCompletions,
            Self::MenuForward => InputFunction::MenuComplete,
            Self::MenuBackward => InputFunction::MenuCompleteBackward,
        }
    }

    pub(super) fn for_function(function: &InputFunction) -> Option<Self> {
        Self::ALL
            .into_iter()
            .find(|action| action.function() == *function)
    }

    pub(super) fn event(self) -> reedline::ReedlineEvent {
        use reedline::ReedlineEvent;
        // Close any displayed menu before invoking the action on the editing buffer.
        ReedlineEvent::Multiple(vec![
            ReedlineEvent::Esc,
            ReedlineEvent::Menu(self.name().into()),
        ])
    }

    pub(super) fn from_events(events: &[reedline::ReedlineEvent]) -> Option<InputFunction> {
        let [
            reedline::ReedlineEvent::Esc,
            reedline::ReedlineEvent::Menu(name),
        ] = events
        else {
            return None;
        };
        Self::ALL
            .into_iter()
            .find(|action| action.name() == name)
            .map(Self::function)
    }
}

#[derive(Clone, Eq, PartialEq)]
struct Buffer {
    text: String,
    cursor: usize,
}

impl Buffer {
    fn read(editor: &reedline::Editor) -> Self {
        Self {
            text: editor.get_buffer().to_owned(),
            cursor: editor.line_buffer().insertion_point(),
        }
    }

    fn replace(&self, start: usize, count: usize, text: &str) -> Self {
        let mut result = self.text.clone();
        result.replace_range(start..start + count, text);
        Self {
            text: result,
            cursor: start + text.len(),
        }
    }

    fn apply(self, editor: &mut reedline::Editor, undo: reedline::UndoBehavior) {
        if self.text != editor.get_buffer() || self.cursor != editor.line_buffer().insertion_point()
        {
            editor.edit_buffer(
                |buffer| {
                    buffer.set_buffer(self.text);
                    buffer.set_insertion_point(self.cursor);
                },
                undo,
            );
        }
    }
}

struct HistorySearch {
    prefix: String,
    start_cursor: usize,
    id: i64,
    last: Buffer,
}

fn history_matches<'a>(
    history: &'a history::History,
    prefix: &str,
    id: Option<i64>,
    backward: bool,
    ignore_case: bool,
) -> Option<impl Iterator<Item = &'a history::Item>> {
    let query = history::Query {
        direction: if backward {
            history::Direction::Backward
        } else {
            history::Direction::Forward
        },
        not_at_or_before_id: if backward { None } else { id },
        not_at_or_after_id: if backward { id } else { None },
        command_line_filter: (!ignore_case)
            .then(|| history::CommandLineFilter::Prefix(prefix.to_owned())),
        ..Default::default()
    };
    let lowercase_prefix = ignore_case.then(|| prefix.to_lowercase());
    Some(history.search(query).ok()?.filter(move |item| {
        lowercase_prefix
            .as_ref()
            .is_none_or(|prefix| item.command_line.to_lowercase().starts_with(prefix))
    }))
}

#[derive(Default)]
pub(super) struct ReadlineSession {
    history: Option<HistorySearch>,
    cycle: Option<CompletionCycle>,
    last_completion: Option<Buffer>,
    options: ReadlineOptions,
    utf8_locale: bool,
    listing: listings::Listings,
}

impl ReadlineSession {
    pub(super) fn print_message(&mut self, message: String) {
        self.listing.print_message(message);
    }

    pub(super) fn begin(&mut self, options: ReadlineOptions, utf8_locale: bool) {
        self.history = None;
        self.cycle = None;
        self.last_completion = None;
        self.options = options;
        self.utf8_locale = utf8_locale;
        self.listing.cancel();
    }

    pub(super) fn parse_event(
        &mut self,
        event: Event,
        mut parse: impl FnMut(Event) -> ReedlineEvent,
    ) -> ReedlineEvent {
        let Event::Key(key) = event else {
            return parse(event);
        };
        if self.listing.handle_key(key) {
            return ReedlineEvent::None;
        }
        let raw = super::terminal_input::raw_byte(key);
        if let KeyCode::Char(c) = key.code
            && raw.map_or_else(|| !c.is_ascii(), |byte| !byte.is_ascii())
            && (self.options.convert_meta || !self.options.input_meta && !self.utf8_locale)
        {
            let mut storage = [0; 4];
            let mut single = [0];
            let bytes = if let Some(byte) = raw {
                single[0] = byte;
                single.as_slice()
            } else {
                c.encode_utf8(&mut storage).as_bytes()
            };
            let events = bytes
                .iter()
                .copied()
                .map(|byte| {
                    let (code, extra) = match byte & 0x7f {
                        b'\t' => (KeyCode::Tab, KeyModifiers::NONE),
                        b'\r' => (KeyCode::Enter, KeyModifiers::NONE),
                        b'\x1b' => (KeyCode::Esc, KeyModifiers::NONE),
                        1..=26 => (
                            KeyCode::Char(char::from((byte & 0x7f) + b'a' - 1)),
                            KeyModifiers::CONTROL,
                        ),
                        value => (KeyCode::Char(char::from(value)), KeyModifiers::NONE),
                    };
                    let original = if raw.is_some() {
                        KeyModifiers::NONE
                    } else {
                        key.modifiers
                    };
                    let modifiers = original
                        | extra
                        | if self.options.convert_meta {
                            KeyModifiers::ALT
                        } else {
                            KeyModifiers::NONE
                        };
                    parse(Event::Key(KeyEvent::new(code, modifiers)))
                })
                .collect();
            return ReedlineEvent::Multiple(events);
        }
        parse(event)
    }

    pub(super) const fn use_readline_completion(&self) -> bool {
        self.options.show_all_if_ambiguous
            || self.options.show_all_if_unmodified
            || self.options.bell_style.is_some()
            || self.options.completion_display_width.is_some()
    }

    pub(super) fn completion_event(
        &self,
        event: &ReedlineEvent,
        default_tab: bool,
    ) -> Option<ReedlineEvent> {
        let explicit = match event {
            ReedlineEvent::Multiple(events) => {
                Action::from_events(events) == Some(InputFunction::Complete)
            }
            _ => false,
        };
        if self.options.disable_completion && (default_tab || explicit) {
            return Some(ReedlineEvent::Edit(vec![
                reedline::EditCommand::InsertChar('\t'),
            ]));
        }
        (default_tab && self.use_readline_completion()).then(|| Action::Complete.event())
    }

    pub(super) fn comment_event(&self, event: &ReedlineEvent) -> Option<ReedlineEvent> {
        use reedline::EditCommand;
        if self.options.comment_begin == "#" {
            return None;
        }
        if let ReedlineEvent::Multiple(actions) = event
            && let [ReedlineEvent::Edit(commands), ReedlineEvent::Enter] = actions.as_slice()
            && matches!(
                commands.as_slice(),
                [
                    EditCommand::MoveToStart { select: false },
                    EditCommand::InsertChar('#')
                ]
            )
        {
            return Some(ReedlineEvent::Multiple(vec![
                ReedlineEvent::Edit(vec![
                    EditCommand::MoveToStart { select: false },
                    EditCommand::InsertString(self.options.comment_begin.clone()),
                ]),
                ReedlineEvent::Enter,
            ]));
        }
        None
    }

    pub(super) fn history_event(&self, event: &ReedlineEvent) -> Option<ReedlineEvent> {
        if !self.options.history_preserve_point {
            return None;
        }
        match event {
            ReedlineEvent::Up | ReedlineEvent::PreviousHistory => {
                Some(Action::PreviousHistory.event())
            }
            ReedlineEvent::Down | ReedlineEvent::NextHistory => Some(Action::NextHistory.event()),
            _ => None,
        }
    }

    fn search(
        &mut self,
        current: &Buffer,
        history: &history::History,
        action: Action,
    ) -> Option<Buffer> {
        self.cycle = None;
        self.last_completion = None;
        let previous = self.history.take().filter(|search| search.last == *current);
        let backward = matches!(action, Action::HistoryBackward | Action::PreviousHistory);
        let plain = matches!(action, Action::PreviousHistory | Action::NextHistory);
        let start_cursor = previous
            .as_ref()
            .map_or(current.cursor, |search| search.start_cursor);
        let prefix = match &previous {
            Some(search) => search.prefix.as_str(),
            None if plain => "",
            None => current.text.get(..current.cursor)?,
        };
        if !backward && previous.is_none() {
            self.bell();
            return None;
        }

        let id = previous.as_ref().map(|search| search.id);
        if let Some(item) = history_matches(
            history,
            prefix,
            id,
            backward,
            self.options.search_ignore_case,
        )?
        .find(|item| previous.is_none() || item.command_line != current.text)
        {
            let next = Buffer {
                text: item.command_line.clone(),
                cursor: if self.options.history_preserve_point {
                    item.command_line
                        .floor_char_boundary(start_cursor.min(item.command_line.len()))
                } else if prefix.is_empty() {
                    item.command_line.len()
                } else {
                    prefix.len()
                },
            };
            self.history = Some(HistorySearch {
                prefix: prefix.to_owned(),
                start_cursor,
                id: item.id,
                last: next.clone(),
            });
            return Some(next);
        }

        if !backward && prefix.is_empty() {
            return Some(Buffer {
                text: String::new(),
                cursor: 0,
            });
        }
        self.history = previous;
        self.bell();
        None
    }

    async fn complete(
        &mut self,
        shell: &mut brush_core::Shell<impl brush_core::ShellExtensions>,
        current: &Buffer,
        action: Action,
        use_color: bool,
    ) -> Option<Buffer> {
        self.history = None;
        let menu = matches!(action, Action::MenuForward | Action::MenuBackward);
        if menu && let Some(cycle) = self.cycle.as_mut().filter(|cycle| cycle.last == *current) {
            return Some(cycle.advance(action));
        }
        self.cycle = None;

        let values = completion::complete_async(shell, &current.text, current.cursor).await;
        if matches!(action, Action::ListCompletions) {
            if !values.candidates.is_empty() {
                self.listing
                    .show(&values, shell, use_color, self.utf8_locale);
            }
            return None;
        }
        let start = values.insertion_index;
        let count = values.delete_count;
        let options = shell.readline_options();
        let candidates = values.candidates.as_slice();
        let word = current.text.get(start..start + count)?;
        let prefix_len = completion::common_prefix_len(candidates, options.completion_ignore_case);
        let result = match candidates {
            [] => {
                self.bell();
                None
            }
            [single] => Some(current.replace(start, count, single)),
            [first, ..] => {
                let prefix = first.get(..prefix_len).unwrap_or_default();
                let extends = prefix_len > word.len()
                    && (prefix.len() - prefix.trim_end_matches('\\').len()) % 2 == 0;
                let partial = if extends {
                    current.replace(start, count, prefix)
                } else {
                    current.clone()
                };
                let show_list = options.show_all_if_ambiguous
                    || options.show_all_if_unmodified && !extends
                    || !menu && !extends && self.last_completion.as_ref() == Some(current);
                let query = if show_list {
                    self.listing
                        .show(&values, shell, use_color, self.utf8_locale)
                } else {
                    false
                };
                if menu && !query {
                    let base = if options.menu_complete_display_prefix {
                        partial
                    } else {
                        current.clone()
                    };
                    let mut cycle = CompletionCycle {
                        base: base.clone(),
                        last: base.clone(),
                        start,
                        count: base.cursor - start,
                        candidates: values.candidates,
                        position: None,
                    };
                    let next = if options.menu_complete_display_prefix {
                        base
                    } else {
                        cycle.advance(action)
                    };
                    self.cycle = Some(cycle);
                    Some(next)
                } else {
                    Some(partial)
                }
            }
        };
        self.last_completion = if menu { None } else { result.clone() };
        result
    }

    pub(super) fn bell(&self) {
        use std::io::Write as _;
        let mut terminal = std::io::stderr();
        match self.options.bell_style.unwrap_or_default() {
            ReadlineBellStyle::None => {}
            ReadlineBellStyle::Audible => {
                let _ = terminal.write_all(b"\x07").and_then(|()| terminal.flush());
            }
            ReadlineBellStyle::Visible => {
                let _ = brush_core::sys::input::flash_readline_terminal(&mut terminal);
            }
        }
    }
}

struct CompletionCycle {
    base: Buffer,
    last: Buffer,
    start: usize,
    count: usize,
    candidates: Vec<String>,
    position: Option<usize>,
}

impl CompletionCycle {
    fn advance(&mut self, action: Action) -> Buffer {
        let last = self.candidates.len() - 1;
        self.position = if matches!(action, Action::MenuBackward) {
            match self.position {
                None => Some(last),
                Some(index) => index.checked_sub(1),
            }
        } else {
            match self.position {
                None => Some(0),
                Some(index) => (index < last).then_some(index + 1),
            }
        };
        self.last = match self.position {
            Some(index) => self
                .base
                .replace(self.start, self.count, &self.candidates[index]),
            None => self.base.clone(),
        };
        self.last.clone()
    }
}

pub(super) fn install<SE: brush_core::ShellExtensions>(
    mut editor: reedline::Reedline,
    shell: &refs::ShellRef<SE>,
    shared: &Session,
    use_color: bool,
) -> reedline::Reedline {
    for action in Action::ALL {
        let menu = ActionMenu {
            settings: reedline::MenuSettings::default().with_name(action.name()),
            active: false,
            shared: shared.clone(),
            shell: shell.clone(),
            action,
            use_color,
        };
        let menu = if matches!(
            action,
            Action::HistoryBackward
                | Action::HistoryForward
                | Action::PreviousHistory
                | Action::NextHistory
        ) {
            reedline::ReedlineMenu::HistoryMenu(Box::new(menu))
        } else {
            reedline::ReedlineMenu::EngineCompleter(Box::new(menu))
        };
        editor = editor.with_menu(menu);
    }
    editor.with_external_printer(ListingOutput(shared.clone()))
}

struct ListingOutput(Session);

impl reedline::ExternalOutput for ListingOutput {
    fn drain(&mut self) -> std::io::Result<Vec<String>> {
        Ok(self.0.lock().listing.drain())
    }
}

// Reedline invokes an opening menu synchronously with both the editor and the
// completer available, before processing the next key in the same input burst.
// These one-shot menus apply the action and immediately return to normal editing.
struct ActionMenu<SE: brush_core::ShellExtensions> {
    settings: reedline::MenuSettings,
    active: bool,
    shared: Session,
    shell: refs::ShellRef<SE>,
    action: Action,
    use_color: bool,
}

impl<SE: brush_core::ShellExtensions> reedline::Menu for ActionMenu<SE> {
    fn settings(&self) -> &reedline::MenuSettings {
        &self.settings
    }
    fn is_active(&self) -> bool {
        self.active
    }
    fn set_active(&mut self, active: bool) {
        self.active = active;
    }
    fn clear_input(&mut self) {}
    fn menu_event(&mut self, event: reedline::MenuEvent) {
        self.handle_menu_event(&event);
    }
    fn can_quick_complete(&self) -> bool {
        true
    }

    fn update_values(&mut self, editor: &mut reedline::Editor, _: &mut dyn reedline::Completer) {
        let current = Buffer::read(editor);
        let mut shared = self.shared.lock();
        if let Some((next, undo)) = tokio::task::block_in_place(|| {
            tokio::runtime::Handle::current().block_on(async {
                let mut shell = self.shell.lock().await;
                let result = match self.action {
                    Action::HistoryBackward
                    | Action::HistoryForward
                    | Action::PreviousHistory
                    | Action::NextHistory => {
                        let history = shell.history()?;
                        let next = shared.search(&current, history, self.action)?;
                        Some((next, reedline::UndoBehavior::HistoryNavigation))
                    }
                    _ => {
                        let next = shared
                            .complete(&mut shell, &current, self.action, self.use_color)
                            .await?;
                        Some((next, reedline::UndoBehavior::CreateUndoPoint))
                    }
                };
                drop(shell);
                result
            })
        }) {
            next.apply(editor, undo);
        }
        self.active = false;
    }

    fn can_partially_complete(
        &mut self,
        _: bool,
        _: &mut reedline::Editor,
        _: &mut dyn reedline::Completer,
    ) -> bool {
        false
    }
    fn update_working_details(
        &mut self,
        _: &mut reedline::Editor,
        _: &mut dyn reedline::Completer,
        _: &reedline::Painter,
    ) {
    }
    fn reset_position(&mut self) {}
    fn replace_in_buffer(&self, _: &mut reedline::Editor) {}
    fn menu_required_lines(&self, _: u16) -> u16 {
        0
    }
    fn menu_string(&self, _: u16, _: bool) -> String {
        String::new()
    }
    fn min_rows(&self) -> u16 {
        0
    }
    fn get_values(&self) -> &[reedline::Suggestion] {
        &[]
    }
}
