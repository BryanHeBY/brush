//! Readline settings and edit buffer support for shell instances.

use crate::{error, extensions, variables::ShellVariable};

/// Signal to use when a line-editing command cannot proceed.
#[derive(
    Clone, Copy, Debug, Default, Eq, PartialEq, strum_macros::Display, strum_macros::EnumString,
)]
#[strum(serialize_all = "kebab-case")]
#[cfg_attr(feature = "serde", derive(serde::Serialize, serde::Deserialize))]
pub enum ReadlineBellStyle {
    /// Suppress the bell.
    None,
    /// Request the terminal's audible bell.
    #[default]
    Audible,
    /// Flash the terminal display.
    Visible,
}

/// Settings shared by the shell's completion engine and interactive input backends.
#[derive(Clone, Debug)]
#[cfg_attr(
    feature = "serde",
    derive(serde::Serialize, serde::Deserialize),
    serde(default)
)]
pub struct ReadlineOptions {
    /// Most recently loaded top-level initialization file.
    pub init_file: Option<std::path::PathBuf>,
    /// Adapt key bindings to the terminal driver’s editing characters.
    pub bind_tty_special_chars: bool,
    /// Briefly move the cursor to the matching opening delimiter.
    pub blink_matching_paren: bool,
    /// Treat multibyte terminal input as individual bytes.
    pub byte_oriented: bool,
    /// Display terminal control characters that generate signals.
    pub echo_control_characters: bool,
    /// Use the terminal’s application keypad mode while editing.
    pub enable_keypad: bool,
    /// Enable the terminal’s Meta modifier while editing.
    pub enable_meta_key: bool,
    /// Always interpret the inputrc Meta modifier as an Escape prefix.
    pub force_meta_prefix: bool,
    /// Display the edit buffer in a single horizontally scrolling row.
    pub horizontal_scroll_mode: bool,
    /// Mark edited history entries with an asterisk.
    pub mark_modified_lines: bool,
    /// Legacy switch for selecting the visible bell.
    pub prefer_visible_bell: bool,
    /// Discard temporary edits to history entries after accepting a line.
    pub revert_all_at_newline: bool,
    /// Override highlighting of pasted text and incremental history matches.
    pub enable_active_region: Option<bool>,
    /// Terminal escape sequence that opens an active region.
    pub active_region_start_color: Option<String>,
    /// Terminal escape sequence that closes an active region.
    pub active_region_end_color: Option<String>,
    /// Keep the high bit of input bytes. Also named `meta-flag`.
    pub input_meta: bool,
    /// Display non-ASCII input directly.
    pub output_meta: bool,
    /// Interpret legacy high-bit input bytes as Meta key chords.
    pub convert_meta: bool,
    /// Match completion candidates without regard to case.
    pub completion_ignore_case: bool,
    /// Abbreviate common prefixes longer than this many characters in lists.
    pub completion_prefix_display_length: usize,
    /// List ambiguous completions immediately.
    pub show_all_if_ambiguous: bool,
    /// List ambiguous completions immediately if the word cannot be extended.
    pub show_all_if_unmodified: bool,
    /// Override how directory symlinks are marked during interactive completion.
    pub mark_symlinked_directories: Option<bool>,
    /// Include hidden filenames when the word does not begin with a dot.
    pub match_hidden_files: bool,
    /// Pause between pages when printing a completion list.
    pub page_completions: bool,
    /// Ask for confirmation before printing more than this many candidates. Zero disables it.
    pub completion_query_items: usize,
    /// Append file type indicators in completion lists.
    pub visible_stats: bool,
    /// Reuse matching text already present after the cursor.
    pub skip_completed_text: bool,
    /// Use file type colors in completion lists.
    pub colored_stats: bool,
    /// Insert the common prefix before cycling through menu completion candidates.
    pub menu_complete_display_prefix: bool,
    /// Suppress interactive completion.
    pub disable_completion: bool,
    /// Signal to use when an editing operation cannot proceed.
    pub bell_style: Option<ReadlineBellStyle>,
    /// Expand a leading tilde while inserting a filename completion.
    pub expand_tilde: bool,
    /// Match underscores and hyphens interchangeably during case-insensitive file completion.
    pub completion_map_case: bool,
    /// Limit the display width of a completion list, if specified.
    pub completion_display_width: Option<usize>,
    /// List completion candidates in row-major order.
    pub print_completions_horizontally: bool,
    /// Highlight the shared prefix in completion lists.
    pub colored_completion_prefix: bool,
    /// Match history searches irrespective of case.
    pub search_ignore_case: bool,
    /// Keep the cursor position when traversing the history.
    pub history_preserve_point: bool,
    /// Cap the number of commands retained in the history.
    pub history_size: Option<usize>,
    /// Display the editing mode in the primary prompt.
    pub show_mode_in_prompt: bool,
    /// Override the interactive backend's bracketed-paste setting.
    pub enable_bracketed_paste: Option<bool>,
    /// Time to wait between strokes of a key sequence, in milliseconds.
    pub keyseq_timeout: u64,
    /// Text inserted by the insert-comment command.
    pub comment_begin: String,
    /// Prompt indicator used in Emacs mode.
    pub emacs_mode_string: String,
    /// Prompt indicator used in Vi movement mode.
    pub vi_cmd_mode_string: String,
    /// Prompt indicator used in Vi insertion mode.
    pub vi_ins_mode_string: String,
}

impl Default for ReadlineOptions {
    fn default() -> Self {
        Self {
            init_file: None,
            bind_tty_special_chars: true,
            blink_matching_paren: false,
            byte_oriented: false,
            echo_control_characters: true,
            enable_keypad: false,
            enable_meta_key: true,
            force_meta_prefix: false,
            horizontal_scroll_mode: false,
            mark_modified_lines: false,
            prefer_visible_bell: true,
            revert_all_at_newline: false,
            enable_active_region: None,
            active_region_start_color: None,
            active_region_end_color: None,
            input_meta: true,
            output_meta: true,
            convert_meta: false,
            completion_ignore_case: false,
            completion_prefix_display_length: 0,
            show_all_if_ambiguous: false,
            show_all_if_unmodified: false,
            mark_symlinked_directories: None,
            match_hidden_files: true,
            page_completions: true,
            completion_query_items: 100,
            visible_stats: false,
            skip_completed_text: false,
            colored_stats: false,
            menu_complete_display_prefix: false,
            disable_completion: false,
            bell_style: None,
            expand_tilde: false,
            completion_map_case: false,
            completion_display_width: None,
            print_completions_horizontally: false,
            colored_completion_prefix: false,
            search_ignore_case: false,
            history_preserve_point: false,
            history_size: None,
            show_mode_in_prompt: false,
            enable_bracketed_paste: None,
            keyseq_timeout: 500,
            comment_begin: "#".to_owned(),
            emacs_mode_string: "@".to_owned(),
            vi_cmd_mode_string: "(cmd)".to_owned(),
            vi_ins_mode_string: "(ins)".to_owned(),
        }
    }
}

impl<SE: extensions::ShellExtensions> crate::Shell<SE> {
    /// Returns the readline-compatible settings for this shell.
    pub const fn readline_options(&self) -> &ReadlineOptions {
        &self.readline_options
    }

    /// Returns mutable access to the readline-compatible settings for this shell.
    pub const fn readline_options_mut(&mut self) -> &mut ReadlineOptions {
        &mut self.readline_options
    }

    /// Updates the shell state to reflect the given edit buffer contents.
    ///
    /// # Arguments
    ///
    /// * `contents` - The contents of the edit buffer.
    /// * `cursor` - The cursor position in the edit buffer.
    pub fn set_edit_buffer(&mut self, contents: String, cursor: usize) -> Result<(), error::Error> {
        self.env
            .set_global("READLINE_LINE", ShellVariable::new(contents))?;

        self.env
            .set_global("READLINE_POINT", ShellVariable::new(cursor.to_string()))?;

        Ok(())
    }

    /// Returns the contents of the shell's edit buffer, if any. The buffer
    /// state is cleared from the shell.
    pub fn pop_edit_buffer(&mut self) -> Result<Option<(String, usize)>, error::Error> {
        let line = self
            .env
            .unset("READLINE_LINE")?
            .map(|line| line.value().to_cow_str(self).to_string());

        let point = self
            .env
            .unset("READLINE_POINT")?
            .and_then(|point| point.value().to_cow_str(self).parse::<usize>().ok())
            .unwrap_or(0);

        if let Some(line) = line {
            Ok(Some((line, point)))
        } else {
            Ok(None)
        }
    }
}
