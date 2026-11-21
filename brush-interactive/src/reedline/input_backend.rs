use nu_ansi_term::Style;
use reedline::MenuBuilder;

use super::{completer, edit_mode, highlighter, history, readline, validator};
use crate::{InputBackend, ReadResult, ShellError, input_backend::InteractivePrompt, refs};

/// Represents an interactive shell capable of taking commands from standard input
/// and reporting results to standard output and standard error streams.
pub struct ReedlineInputBackend {
    reedline: Option<reedline::Reedline>,
    readline_session: readline::Session,
    keymaps: std::sync::Arc<tokio::sync::Mutex<edit_mode::BindingMaps>>,
    allow_bracketed_paste: bool,
    terminal_input: std::sync::Arc<std::sync::Mutex<super::terminal_input::TerminalInput>>,
}

pub(super) const COMPLETION_MENU_NAME: &str = "completion_menu";
pub(super) const RELOAD_INPUTRC: &str = "\0brush:reload-inputrc";

fn completion_menu_text_style() -> Style {
    Style::new()
}

fn completion_menu_selected_text_style() -> Style {
    Style::new().bold().reverse()
}

fn completion_menu_match_text_style() -> Style {
    Style::new().underline()
}

fn completion_menu_selected_match_text_style() -> Style {
    completion_menu_selected_text_style().underline()
}

fn history_hint_style() -> Style {
    Style::new().italic().dimmed()
}

struct TerminalModes(Vec<u8>);

impl TerminalModes {
    fn enter(options: &brush_core::ReadlineOptions) -> std::io::Result<Self> {
        use std::io::Write as _;
        let (enter, leave) = brush_core::sys::input::readline_terminal_modes(
            options.enable_keypad,
            options.enable_meta_key,
        );
        let guard = Self(leave);
        let mut terminal = std::io::stderr().lock();
        terminal.write_all(&enter)?;
        terminal.flush()?;
        Ok(guard)
    }
}

impl Drop for TerminalModes {
    fn drop(&mut self) {
        use std::io::Write as _;
        let mut terminal = std::io::stderr().lock();
        let _ = terminal.write_all(&self.0).and_then(|()| terminal.flush());
    }
}

fn active_region_color(setting: Option<&str>, default: &str) -> String {
    use brush_parser::readline_binding::{KeySequenceItem, parse_key_sequence};

    setting
        .and_then(|value| parse_key_sequence(value).ok())
        .map(|seq| {
            seq.0
                .into_iter()
                .filter_map(|item| match item {
                    KeySequenceItem::Byte(byte) => Some(byte),
                    _ => None,
                })
                .collect::<Vec<_>>()
        })
        .and_then(|bytes| String::from_utf8(bytes).ok())
        .unwrap_or_else(|| default.to_owned())
}

impl ReedlineInputBackend {
    /// Returns a new interactive shell instance, created with the provided options.
    ///
    /// # Arguments
    ///
    /// * `options` - Options for creating the input backend.
    /// * `shell_ref` - Shell that the backend will be used with.
    pub fn new(
        options: &crate::UIOptions,
        shell_ref: &refs::ShellRef<impl brush_core::ShellExtensions>,
    ) -> Result<Self, ShellError> {
        // Set up key bindings.
        let key_bindings =
            compose_key_bindings(COMPLETION_MENU_NAME, options.history_suggestion_navigation);

        // Set up mutable edit mode.
        let readline_session = readline::Session::new(options.history_suggestion_navigation);
        let mutable_edit_mode =
            edit_mode::MutableEditMode::new(key_bindings, readline_session.clone());
        let updatable_bindings = mutable_edit_mode.bindings();

        // Create helper objects that implement reedline traits; each will
        // hold a reference to the shell.
        let completer = completer::ReedlineCompleter {
            shell: shell_ref.clone(),
        };
        let validator = validator::ReedlineValidator {
            shell: shell_ref.clone(),
        };
        let syntax_highlighter = highlighter::ReedlineHighlighter {
            shell: shell_ref.clone(),
        };
        let history = history::ReedlineHistory {
            shell: shell_ref.clone(),
        };

        // Set up completion menu. Set an empty marker to avoid the
        // line's text horizontally shifting around during/after completion.
        // We set a max column count of 10 to ensure it's larger than the
        // hard-coded default (4 last we checked); if there's not enough
        // horizontal space in the terminal to fit that many columns, given
        // the actual text to be displayed, it will get effectively dereased
        // anyhow.
        let completion_menu = Box::new(
            reedline::ColumnarMenu::default()
                .with_name(COMPLETION_MENU_NAME)
                .with_marker("")
                .with_columns(10)
                // Like readline, hand completion the whole line, not just the text before
                // the cursor: completion functions see the words after the cursor (e.g. in
                // COMP_WORDS), and candidates completed mid-line don't get a trailing space.
                .with_input_mode(reedline::InputMode::FullBuffer)
                .with_text_style(completion_menu_text_style())
                .with_match_text_style(completion_menu_match_text_style())
                .with_selected_text_style(completion_menu_selected_text_style())
                .with_selected_match_text_style(completion_menu_selected_match_text_style()),
        );

        let hinter = readline::HistoryHinter::new(readline_session.clone(), history_hint_style());

        // Instantiate reedline with some defaults and hand it ownership of
        // the helpers.
        let mut reedline = reedline::Reedline::create()
            .with_ansi_colors(!options.disable_color)
            .use_bracketed_paste(!options.disable_bracketed_paste)
            .with_completer(Box::new(completer))
            .with_quick_completions(true)
            .with_validator(Box::new(validator))
            .with_hinter(Box::new(hinter))
            .with_menu(reedline::ReedlineMenu::EngineCompleter(completion_menu))
            .with_edit_mode(Box::new(mutable_edit_mode))
            .with_ordered_input(true)
            .with_history(Box::new(history));

        // Override Reedline's default example highlighter, which hard-codes white as the
        // neutral input color. When syntax highlighting is disabled we still install a plain
        // highlighter so typed text follows the terminal's default foreground color.
        if !options.disable_color {
            reedline = if options.disable_highlighting {
                reedline.with_highlighter(Box::new(highlighter::PlainTextHighlighter))
            } else {
                reedline.with_highlighter(Box::new(syntax_highlighter))
            };
        }

        reedline = readline::install(
            reedline,
            shell_ref,
            &readline_session,
            !options.disable_color,
        );

        let mut shell = tokio::task::block_in_place(|| {
            tokio::runtime::Handle::current().block_on(shell_ref.lock())
        });

        shell.set_key_bindings(Some(updatable_bindings.clone()));
        drop(shell);

        Ok(Self {
            reedline: Some(reedline),
            readline_session,
            keymaps: updatable_bindings,
            allow_bracketed_paste: !options.disable_bracketed_paste,
            terminal_input: std::sync::Arc::new(std::sync::Mutex::new(
                super::terminal_input::TerminalInput::new(std::time::Duration::from_millis(500)),
            )),
        })
    }
}

impl Drop for ReedlineInputBackend {
    fn drop(&mut self) {
        // It's unpleasant to need to do so, but if we detect a panic in the process of being
        // unwound, then we arrange for our reedline::Reedline instance to *not* get dropped.
        // Without this, then there's a chance that our panic handler emitted important
        // diagnostics to stdout but dropping the Reedline object will end up erasing it
        // when the latter object's internal Painter gets dropped and, in turn, may flush
        // some not-yet-flushed terminal control sequences. This isn't theoretical; we've
        // actively seen this in various cases where a panic occurs with Reedline::read_line()
        // on the stack.
        if std::thread::panicking() {
            let reedline = std::mem::take(&mut self.reedline);
            std::mem::forget(reedline);
        }
    }
}

impl InputBackend for ReedlineInputBackend {
    /// Reads a line of input, using the given prompt.
    ///
    /// # Arguments
    ///
    /// * `prompt` - The prompt to display to the user.
    async fn read_line(
        &mut self,
        shell: &crate::ShellRef<impl brush_core::ShellExtensions>,
        prompt: InteractivePrompt,
    ) -> Result<ReadResult, ShellError> {
        let (options, raw_input) = {
            let shell = shell.lock().await;
            let options = shell.readline_options().clone();
            let raw_input = {
                let mut keymaps = self.keymaps.lock().await;
                keymaps.sync_shell_mode(shell.options().vi_mode);
                keymaps.set_chord_timeout(options.keyseq_timeout);
                keymaps.set_terminal_options(&options)
            };
            let utf8_locale = ["LC_ALL", "LC_CTYPE", "LANG"]
                .into_iter()
                .find_map(|name| shell.env_str(name).filter(|value| !value.is_empty()))
                .is_none_or(|locale| {
                    let locale = locale.to_ascii_lowercase();
                    locale.contains("utf-8") || locale.contains("utf8")
                });
            self.readline_session
                .lock()
                .begin(options.clone(), utf8_locale);
            drop(shell);
            (options, raw_input)
        };
        if let Some(enabled) = options.enable_bracketed_paste {
            self.reedline = self.reedline.take().map(|reedline| {
                reedline.use_bracketed_paste(enabled && self.allow_bracketed_paste)
            });
        }
        let Some(reedline) = &mut self.reedline else {
            return Ok(ReadResult::Eof);
        };
        let interrupt_byte = brush_core::sys::input::readline_terminal_keys()
            .into_iter()
            .find_map(|(byte, function)| function.is_none().then_some(byte));
        super::terminal_input::attach(
            reedline,
            &self.terminal_input,
            raw_input,
            std::time::Duration::from_millis(options.keyseq_timeout),
        );
        reedline.set_editable_history(true);
        reedline.set_byte_oriented(options.byte_oriented);
        reedline.set_line_display(reedline::LineDisplay {
            horizontal_scroll: options.horizontal_scroll_mode,
            echo_interrupt: options
                .echo_control_characters
                .then(|| char::from(interrupt_byte.unwrap_or(3) ^ 0x40)),
            blink_matching_bracket: options.blink_matching_paren,
            active_region: options.enable_active_region.unwrap_or(true).then(|| {
                let (start, end) = brush_core::sys::input::readline_terminal_standout();
                (
                    active_region_color(options.active_region_start_color.as_deref(), &start),
                    active_region_color(options.active_region_end_color.as_deref(), &end),
                )
            }),
            mark_modified_history: options.mark_modified_lines,
            accept_history_search: true,
            revert_history_on_submit: options.revert_all_at_newline,
        });
        let _terminal_modes = TerminalModes::enter(&options).map_err(ShellError::InputError)?;
        let prompt = super::prompt::ReadlinePrompt {
            prompt: &prompt,
            options: &options,
            keymaps: &self.keymaps,
        };
        match reedline.read_line(&prompt) {
            Ok(reedline::Signal::Success(s)) => Ok(ReadResult::Input(s)),
            Ok(reedline::Signal::CtrlC(_)) => Ok(ReadResult::Interrupted),
            Ok(reedline::Signal::CtrlD) => Ok(ReadResult::Eof),
            Ok(reedline::Signal::HostCommand(cmd)) if cmd == RELOAD_INPUTRC => {
                let command = options
                    .init_file
                    .as_ref()
                    .and_then(|path| path.to_str())
                    .map_or_else(String::new, |path| {
                        format!("builtin bind -f {}", brush_core::escape::single_quote(path))
                    });
                Ok(ReadResult::BoundCommand(command))
            }
            Ok(reedline::Signal::HostCommand(cmd)) => Ok(ReadResult::BoundCommand(cmd)),
            Ok(_) => Err(ShellError::UnexpectedInputFailure),
            Err(err) => Err(ShellError::InputError(err)),
        }
    }

    fn get_read_buffer(&self) -> Option<(String, usize)> {
        self.reedline.as_ref().map(reedline::Reedline::host_buffer)
    }

    fn set_read_buffer(&mut self, buffer: String, cursor: usize) {
        if let Some(reedline) = &mut self.reedline {
            reedline.set_host_buffer(buffer, cursor);
        }
    }
}

fn compose_key_bindings(
    completion_menu_name: &str,
    history_suggestion_navigation: bool,
) -> reedline::Keybindings {
    let mut key_bindings = reedline::default_emacs_keybindings();

    // Wire up tab to completion.
    key_bindings.add_binding(
        reedline::KeyModifiers::NONE,
        reedline::KeyCode::Tab,
        reedline::ReedlineEvent::UntilFound(vec![
            reedline::ReedlineEvent::Menu(completion_menu_name.to_string()),
            reedline::ReedlineEvent::MenuNext,
            reedline::ReedlineEvent::Edit(vec![reedline::EditCommand::Complete]),
        ]),
    );
    // Wire up shift-tab for completion.
    key_bindings.add_binding(
        reedline::KeyModifiers::SHIFT,
        reedline::KeyCode::BackTab,
        reedline::ReedlineEvent::MenuPrevious,
    );
    if history_suggestion_navigation {
        key_bindings.add_binding(
            reedline::KeyModifiers::CONTROL,
            reedline::KeyCode::Right,
            reedline::ReedlineEvent::UntilFound(vec![
                reedline::ReedlineEvent::HistoryHintWordComplete,
                reedline::ReedlineEvent::Edit(vec![reedline::EditCommand::MoveWordRightStart {
                    select: false,
                }]),
            ]),
        );
    }

    // Add undo.
    key_bindings.add_binding(
        reedline::KeyModifiers::CONTROL,
        reedline::KeyCode::Char('_'),
        reedline::ReedlineEvent::Edit(vec![reedline::EditCommand::Undo]),
    );

    // Capitalize.
    key_bindings.add_binding(
        reedline::KeyModifiers::ALT,
        reedline::KeyCode::Char('c'),
        reedline::ReedlineEvent::Edit(vec![
            reedline::EditCommand::CapitalizeChar,
            reedline::EditCommand::MoveWordRight { select: false },
        ]),
    );

    // Add comment.
    key_bindings.add_binding(
        reedline::KeyModifiers::ALT,
        reedline::KeyCode::Char('#'),
        reedline::ReedlineEvent::Multiple(vec![
            reedline::ReedlineEvent::Edit(vec![
                reedline::EditCommand::MoveToStart { select: false },
                reedline::EditCommand::InsertChar('#'),
            ]),
            reedline::ReedlineEvent::Enter,
        ]),
    );

    key_bindings
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn history_hint_style_is_theme_adaptive() {
        let style = history_hint_style();

        assert_eq!(style.foreground, None);
        assert_eq!(style.background, None);
        assert!(style.is_italic);
        assert!(style.is_dimmed);
    }

    #[test]
    fn completion_menu_styles_are_theme_adaptive() {
        let text_style = completion_menu_text_style();
        let match_style = completion_menu_match_text_style();
        let selected_text_style = completion_menu_selected_text_style();
        let selected_match_text_style = completion_menu_selected_match_text_style();

        assert_eq!(text_style.foreground, None);
        assert_eq!(text_style.background, None);

        assert_eq!(match_style.foreground, None);
        assert_eq!(match_style.background, None);
        assert!(match_style.is_underline);

        assert_eq!(selected_text_style.foreground, None);
        assert_eq!(selected_text_style.background, None);
        assert!(selected_text_style.is_bold);
        assert!(selected_text_style.is_reverse);

        assert_eq!(selected_match_text_style.foreground, None);
        assert_eq!(selected_match_text_style.background, None);
        assert!(selected_match_text_style.is_bold);
        assert!(selected_match_text_style.is_reverse);
        assert!(selected_match_text_style.is_underline);
    }
}
