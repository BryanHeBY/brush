use itertools::Itertools as _;
use std::{collections::HashMap, io::Write, str::FromStr as _};
use strum::IntoEnumIterator;

use brush_core::{
    ExecutionExitCode, ExecutionResult, ReadlineOptions, builtins,
    interfaces::{self, InputFunction, KeyAction, KeySequence, ReadlineKeymap},
    sys, trace_categories,
};

mod inputrc;
pub use inputrc::load_default_inputrc;

impl From<BindKeyMap> for ReadlineKeymap {
    fn from(value: BindKeyMap) -> Self {
        match value {
            BindKeyMap::EmacsStandard => Self::Emacs,
            BindKeyMap::EmacsMeta => Self::EmacsMeta,
            BindKeyMap::EmacsCtlx => Self::EmacsCtlx,
            BindKeyMap::ViCommand => Self::ViCommand,
            BindKeyMap::ViInsert => Self::ViInsert,
        }
    }
}

/// Inspect and modify key bindings and other input configuration.
#[derive(winnow_args::Args)]
#[arg(disable_help_short, disable_version_flag, disable_help_subcommand)]
pub(crate) struct BindCommand {
    /// Name of key map to use.
    #[arg(short = 'm')]
    keymap: Option<BindKeyMap>,
    /// List functions.
    #[arg(short = 'l')]
    list_funcs: bool,
    /// List functions and bindings.
    #[arg(short = 'P')]
    list_funcs_and_bindings: bool,
    /// List functions and bindings in a format suitable for use as input.
    #[arg(short = 'p')]
    list_funcs_and_bindings_reusable: bool,
    /// List key sequences that invoke macros.
    #[arg(short = 'S')]
    list_key_seqs_that_invoke_macros: bool,
    /// List key sequences that invoke macros in a format suitable for use as input.
    #[arg(short = 's')]
    list_key_seqs_that_invoke_macros_reusable: bool,
    /// List variables.
    #[arg(short = 'V')]
    list_vars: bool,
    /// List variables in a format suitable for use as input.
    #[arg(short = 'v')]
    list_vars_reusable: bool,
    /// Find the keys bound to the given named function.
    #[arg(short = 'q', value_name = "FUNC_NAME")]
    query_func_bindings: Option<String>,
    /// Remove all bindings for the given named function.
    #[arg(short = 'u', value_name = "FUNC_NAME")]
    remove_func_bindings: Option<String>,
    /// Remove the binding for the given key sequence.
    #[arg(short = 'r', value_name = "KEY_SEQ")]
    remove_key_seq_binding: Option<String>,
    /// Import bindings from the given file.
    #[arg(short = 'f', value_name = "PATH")]
    bindings_file: Option<String>,
    /// Bind key sequence to command.
    #[arg(short = 'x', value_name = "BINDING")]
    key_seq_bindings: Vec<String>,
    /// List key sequence bindings.
    #[arg(short = 'X')]
    list_key_seq_bindings: bool,
    /// Key sequence binding to readline function or command.
    #[arg(positional)]
    key_sequences: Vec<String>,
}

/// Identifier for a keymap
#[derive(Clone, Copy, winnow_args::ValueEnum)]
pub(crate) enum BindKeyMap {
    #[arg(name = "emacs-standard", alias = "emacs")]
    EmacsStandard,
    #[arg(name = "emacs-meta")]
    EmacsMeta,
    #[arg(name = "emacs-ctlx")]
    EmacsCtlx,
    #[arg(name = "vi-command", alias("vi", "vi-move"))]
    ViCommand,
    #[arg(name = "vi-insert")]
    ViInsert,
}

brush_winnow_args_adapter::impl_parse_args!(BindCommand);

#[derive(Debug, thiserror::Error)]
pub(crate) enum BindError {
    /// Unknown function specified.
    #[error("unknown function: {0}")]
    UnknownFunction(String),

    /// Unknown key binding function.
    #[error("unknown key binding function: {0}")]
    UnknownKeyBindingFunction(String),

    /// Cannot read the supplied binding file.
    #[error("{path}: cannot read: {source}")]
    BindingFile {
        path: String,
        source: std::io::Error,
    },

    /// Invalid or unsupported readline setting.
    #[error("invalid readline setting: {0}")]
    InvalidSetting(String),

    /// An I/O error occurred.
    #[error("I/O error occurred")]
    IoError(#[from] std::io::Error),

    /// A binding parse error occurred.
    #[error(transparent)]
    BindingParseError(#[from] brush_parser::BindingParseError),
}

impl brush_core::BuiltinError for BindError {}

impl From<&BindError> for brush_core::ExecutionExitCode {
    fn from(_err: &BindError) -> Self {
        Self::GeneralError
    }
}

impl<SE: brush_core::ShellExtensions> builtins::Builtin<SE> for BindCommand {
    type Error = BindError;

    async fn execute(
        self,
        mut context: brush_core::ExecutionContext<'_, SE>,
    ) -> Result<brush_core::ExecutionResult, Self::Error> {
        if let Some(key_bindings) = context.shell.key_bindings().cloned() {
            Ok(self.execute_impl(key_bindings, &mut context).await?)
        } else {
            tracing::debug!(target: trace_categories::INPUT,
                 "bind: key bindings not supported in this config");

            // Silently succeed when key bindings are unavailable (e.g., in
            // non-interactive mode or with an input backend that doesn't
            // yet support them).
            Ok(ExecutionExitCode::Success.into())
        }
    }
}

impl BindCommand {
    #[allow(clippy::too_many_lines)]
    async fn execute_impl(
        &self,
        bindings: std::sync::Arc<tokio::sync::Mutex<dyn interfaces::KeyBindings>>,
        context: &mut brush_core::ExecutionContext<'_, impl brush_core::ShellExtensions>,
    ) -> Result<ExecutionResult, BindError> {
        let mut bindings = bindings.lock().await;
        let previous_map = bindings.keymap();
        if let Some(map) = self.keymap {
            bindings.select_keymap(map.into())?;
        }
        let result = self.execute_on_map(&mut *bindings, context);
        if self.keymap.is_some() {
            bindings.select_keymap(previous_map)?;
        }
        result
    }

    #[allow(clippy::too_many_lines)]
    fn execute_on_map(
        &self,
        bindings: &mut dyn interfaces::KeyBindings,
        context: &mut brush_core::ExecutionContext<'_, impl brush_core::ShellExtensions>,
    ) -> Result<ExecutionResult, BindError> {
        if self.list_funcs {
            for func in interfaces::InputFunction::iter() {
                writeln!(context.stdout(), "{func}")?;
            }
        }

        if self.list_funcs_and_bindings {
            display_funcs_and_bindings(bindings, context, false, &self.key_sequences)?;
        }

        if self.list_funcs_and_bindings_reusable {
            display_funcs_and_bindings(bindings, context, true, &self.key_sequences)?;
        }

        if self.list_key_seqs_that_invoke_macros {
            display_macros(&*bindings, context, false /* reusable? */)?;
        }

        if self.list_key_seqs_that_invoke_macros_reusable {
            display_macros(&*bindings, context, true /* reusable? */)?;
        }

        if self.list_vars || self.list_vars_reusable {
            if self.list_vars {
                write_readline_variables(context.shell, bindings, context.stdout(), false)?;
            }
            if self.list_vars_reusable {
                write_readline_variables(context.shell, bindings, context.stdout(), true)?;
            }
        }

        if let Some(func_str) = &self.query_func_bindings {
            let seqs = find_key_seqs_bound_to_function(&*bindings, func_str)?;

            if !seqs.is_empty() {
                writeln!(
                    context.stdout(),
                    "{func_str} can be invoked via {}.",
                    seqs.iter().map(|seq| std::format!("\"{seq}\"")).join(", ")
                )?;
            } else {
                writeln!(context.stdout(), "{func_str} is not bound to any keys.")?;
                return Ok(ExecutionResult::general_error());
            }
        }

        if let Some(func_str) = &self.remove_func_bindings {
            let found_seqs = find_key_seqs_bound_to_function(&*bindings, func_str)?;

            for seq in found_seqs {
                let _ = bindings.try_unbind(seq);
            }
        }

        if let Some(key_seq_str) = &self.remove_key_seq_binding {
            let key_seq = parse_key_sequence(key_seq_str, context.shell.readline_options())?;
            let _ = bindings.try_unbind(key_seq);
        }

        if let Some(path) = &self.bindings_file {
            inputrc::load(path, context, bindings)?;
        }

        if self.list_key_seq_bindings {
            for (seq, action) in &bindings.get_current() {
                let KeyAction::ShellCommand(cmd) = action else {
                    continue;
                };

                writeln!(
                    context.stdout(),
                    "\"{}\" \"{}\"",
                    reusable_key_sequence(seq),
                    reusable_shell_command(cmd)
                )?;
            }
        }

        if !self.key_seq_bindings.is_empty() {
            for key_seq_and_command in &self.key_seq_bindings {
                let (key_seq, command) = parse_key_sequence_and_shell_command(
                    key_seq_and_command,
                    context.shell.readline_options(),
                )?;
                bind_key_sequence_to_shell_cmd(bindings, key_seq, command)?;
            }
        }

        if !self.list_funcs_and_bindings && !self.list_funcs_and_bindings_reusable {
            for key_sequence in &self.key_sequences {
                apply_readline_input(context.shell, bindings, key_sequence.trim())?;
            }
        }

        Ok(ExecutionResult::success())
    }
}

fn apply_readline_input(
    shell: &mut brush_core::Shell<impl brush_core::ShellExtensions>,
    bindings: &mut dyn interfaces::KeyBindings,
    line: &str,
) -> Result<(), BindError> {
    if let Some(setting) = line
        .strip_prefix("set")
        .filter(|rest| rest.starts_with(char::is_whitespace))
    {
        let Some((name, value)) = setting.trim().split_once(char::is_whitespace) else {
            return Err(BindError::InvalidSetting(setting.trim().to_owned()));
        };
        let value = value.trim();
        let value = if matches!(
            name,
            "comment-begin"
                | "emacs-mode-string"
                | "vi-cmd-mode-string"
                | "vi-ins-mode-string"
                | "active-region-start-color"
                | "active-region-end-color"
        ) {
            value
        } else {
            value.split_whitespace().next().unwrap_or_default()
        };
        if name == "keymap" {
            let map = value
                .parse::<ReadlineKeymap>()
                .map_err(|_| BindError::InvalidSetting(setting.trim().to_owned()))?;
            bindings.select_keymap(map)?;
        } else if name == "editing-mode" {
            let vi = if value.eq_ignore_ascii_case("vi") {
                true
            } else if value.eq_ignore_ascii_case("emacs") {
                false
            } else {
                return Err(BindError::InvalidSetting(setting.trim().to_owned()));
            };
            bindings.set_vi_editing(vi)?;
            shell.options_mut().vi_mode = vi;
            shell.options_mut().emacs_mode = !vi;
        } else if name == "mark-directories" {
            let Some(value) = parse_onoff(value) else {
                return Err(BindError::InvalidSetting(setting.trim().to_owned()));
            };
            shell
                .completion_config_mut()
                .fallback_options
                .mark_directories = value;
        } else if !set_readline_variable(shell.readline_options_mut(), name, value) {
            return Err(BindError::InvalidSetting(setting.trim().to_owned()));
        }
        if name == "history-size"
            && let Some(limit) = shell.readline_options().history_size
            && let Some(history) = shell.history_mut()
        {
            history.retain_last(limit);
        }
        return Ok(());
    }
    let (key, target) = parse_key_sequence_and_readline_target(line, shell.readline_options())?;
    bind_key_sequence_to_readline_target(bindings, key, target)
}

fn parse_onoff(value: &str) -> Option<bool> {
    if value.eq_ignore_ascii_case("on") || value == "1" {
        Some(true)
    } else if value.eq_ignore_ascii_case("off") || value == "0" {
        Some(false)
    } else {
        None
    }
}

#[allow(clippy::too_many_lines, reason = "one match arm per inputrc variable")]
fn set_readline_variable(options: &mut ReadlineOptions, name: &str, value: &str) -> bool {
    let target = match name {
        "bind-tty-special-chars" => &mut options.bind_tty_special_chars,
        "blink-matching-paren" => &mut options.blink_matching_paren,
        "byte-oriented" => &mut options.byte_oriented,
        "echo-control-characters" => &mut options.echo_control_characters,
        "enable-keypad" => &mut options.enable_keypad,
        "enable-meta-key" => &mut options.enable_meta_key,
        "force-meta-prefix" => &mut options.force_meta_prefix,
        "horizontal-scroll-mode" => &mut options.horizontal_scroll_mode,
        "mark-modified-lines" => &mut options.mark_modified_lines,
        "revert-all-at-newline" => &mut options.revert_all_at_newline,
        "enable-active-region" => {
            let Some(enabled) = parse_onoff(value) else {
                return false;
            };
            options.enable_active_region = Some(enabled);
            return true;
        }
        "prefer-visible-bell" => {
            let Some(enabled) = parse_onoff(value) else {
                return false;
            };
            options.prefer_visible_bell = enabled;
            options.bell_style = Some(if enabled {
                brush_core::ReadlineBellStyle::Visible
            } else {
                brush_core::ReadlineBellStyle::Audible
            });
            return true;
        }
        "active-region-start-color" | "active-region-end-color" => {
            let target = if name == "active-region-start-color" {
                &mut options.active_region_start_color
            } else {
                &mut options.active_region_end_color
            };
            *target = Some(value.to_owned());
            return true;
        }

        "input-meta" | "meta-flag" => &mut options.input_meta,
        "output-meta" => &mut options.output_meta,
        "convert-meta" => &mut options.convert_meta,
        "completion-ignore-case" => &mut options.completion_ignore_case,
        "show-all-if-ambiguous" => &mut options.show_all_if_ambiguous,
        "show-all-if-unmodified" => &mut options.show_all_if_unmodified,
        "match-hidden-files" => &mut options.match_hidden_files,
        "page-completions" => &mut options.page_completions,
        "visible-stats" => &mut options.visible_stats,
        "skip-completed-text" => &mut options.skip_completed_text,
        "colored-stats" => &mut options.colored_stats,
        "menu-complete-display-prefix" => &mut options.menu_complete_display_prefix,
        "disable-completion" => &mut options.disable_completion,
        "expand-tilde" => &mut options.expand_tilde,
        "completion-map-case" => &mut options.completion_map_case,
        "print-completions-horizontally" => &mut options.print_completions_horizontally,
        "colored-completion-prefix" => &mut options.colored_completion_prefix,
        "search-ignore-case" => &mut options.search_ignore_case,
        "history-preserve-point" => &mut options.history_preserve_point,
        "show-mode-in-prompt" => &mut options.show_mode_in_prompt,
        "bell-style" => {
            let Ok(style) = value.parse() else {
                return false;
            };
            options.bell_style = Some(style);
            return true;
        }
        "comment-begin" | "emacs-mode-string" | "vi-cmd-mode-string" | "vi-ins-mode-string" => {
            let target = match name {
                "comment-begin" => &mut options.comment_begin,
                "emacs-mode-string" => &mut options.emacs_mode_string,
                "vi-cmd-mode-string" => &mut options.vi_cmd_mode_string,
                _ => &mut options.vi_ins_mode_string,
            };
            value.clone_into(target);
            return true;
        }
        "history-size" | "completion-display-width" => {
            let Ok(value) = value.parse::<isize>() else {
                return false;
            };
            let target = if name == "history-size" {
                &mut options.history_size
            } else {
                &mut options.completion_display_width
            };
            *target = usize::try_from(value).ok();
            return true;
        }
        "keyseq-timeout" => {
            options.keyseq_timeout = u64::from(readline_nonnegative_integer(value));
            return true;
        }
        "enable-bracketed-paste" => {
            let Some(value) = parse_onoff(value) else {
                return false;
            };
            options.enable_bracketed_paste = Some(value);
            return true;
        }
        "mark-symlinked-directories" => {
            let Some(value) = parse_onoff(value) else {
                return false;
            };
            options.mark_symlinked_directories = Some(value);
            return true;
        }
        "completion-prefix-display-length" | "completion-query-items" => {
            let value = readline_nonnegative_integer(value) as usize;
            if name == "completion-query-items" {
                options.completion_query_items = value;
            } else {
                options.completion_prefix_display_length = value;
            }
            return true;
        }
        _ => return false,
    };
    let Some(value) = parse_onoff(value) else {
        return false;
    };
    *target = value;
    true
}

// Readline accepts a decimal prefix in these settings and clamps negative values to zero.
fn readline_nonnegative_integer(value: &str) -> u32 {
    let digits = value.strip_prefix('+').unwrap_or(value);
    let end = digits.bytes().take_while(u8::is_ascii_digit).count();
    digits
        .get(..end)
        .and_then(|prefix| prefix.parse::<i32>().ok())
        .and_then(|n| u32::try_from(n).ok())
        .unwrap_or_default()
}

fn write_readline_variables(
    shell: &brush_core::Shell<impl brush_core::ShellExtensions>,
    bindings: &dyn interfaces::KeyBindings,
    mut out: impl Write,
    reusable: bool,
) -> std::io::Result<()> {
    for (name, value) in readline_variables(shell, bindings) {
        if reusable {
            writeln!(out, "set {name} {value}")?;
        } else {
            writeln!(out, "{name} is set to `{value}'")?;
        }
    }
    Ok(())
}

#[allow(
    clippy::too_many_lines,
    reason = "one display entry per inputrc variable"
)]
fn readline_variables(
    shell: &brush_core::Shell<impl brush_core::ShellExtensions>,
    bindings: &dyn interfaces::KeyBindings,
) -> Vec<(&'static str, String)> {
    let options = shell.readline_options();
    let fallback = &shell.completion_config().fallback_options;
    [
        ("bind-tty-special-chars", options.bind_tty_special_chars),
        ("blink-matching-paren", options.blink_matching_paren),
        ("byte-oriented", options.byte_oriented),
        ("echo-control-characters", options.echo_control_characters),
        ("enable-keypad", options.enable_keypad),
        ("enable-meta-key", options.enable_meta_key),
        ("force-meta-prefix", options.force_meta_prefix),
        ("horizontal-scroll-mode", options.horizontal_scroll_mode),
        ("mark-modified-lines", options.mark_modified_lines),
        ("prefer-visible-bell", options.prefer_visible_bell),
        ("revert-all-at-newline", options.revert_all_at_newline),
        (
            "enable-active-region",
            options
                .enable_active_region
                .unwrap_or_else(|| shell.options().interactive),
        ),
        ("colored-stats", options.colored_stats),
        ("completion-ignore-case", options.completion_ignore_case),
        ("convert-meta", options.convert_meta),
        ("input-meta", options.input_meta),
        ("mark-directories", fallback.mark_directories),
        (
            "mark-symlinked-directories",
            options
                .mark_symlinked_directories
                .unwrap_or(fallback.mark_symlinked_directories),
        ),
        ("match-hidden-files", options.match_hidden_files),
        (
            "menu-complete-display-prefix",
            options.menu_complete_display_prefix,
        ),
        ("meta-flag", options.input_meta),
        ("output-meta", options.output_meta),
        ("page-completions", options.page_completions),
        ("show-all-if-ambiguous", options.show_all_if_ambiguous),
        ("show-all-if-unmodified", options.show_all_if_unmodified),
        ("skip-completed-text", options.skip_completed_text),
        ("visible-stats", options.visible_stats),
        ("disable-completion", options.disable_completion),
        ("expand-tilde", options.expand_tilde),
        ("completion-map-case", options.completion_map_case),
        (
            "print-completions-horizontally",
            options.print_completions_horizontally,
        ),
        (
            "colored-completion-prefix",
            options.colored_completion_prefix,
        ),
        ("search-ignore-case", options.search_ignore_case),
        ("history-preserve-point", options.history_preserve_point),
        ("show-mode-in-prompt", options.show_mode_in_prompt),
        (
            "enable-bracketed-paste",
            options.enable_bracketed_paste.unwrap_or(true),
        ),
    ]
    .into_iter()
    .map(|(name, value)| (name, to_onoff(value).to_owned()))
    .chain([
        (
            "bell-style",
            options.bell_style.unwrap_or_default().to_string(),
        ),
        (
            "completion-display-width",
            format_optional_setting(options.completion_display_width),
        ),
        (
            "history-size",
            format_optional_setting(options.history_size),
        ),
        ("keyseq-timeout", options.keyseq_timeout.to_string()),
        ("comment-begin", options.comment_begin.clone()),
        ("emacs-mode-string", options.emacs_mode_string.clone()),
        ("vi-cmd-mode-string", options.vi_cmd_mode_string.clone()),
        ("vi-ins-mode-string", options.vi_ins_mode_string.clone()),
        (
            "editing-mode",
            if bindings
                .vi_editing()
                .unwrap_or_else(|| shell.options().vi_mode)
            {
                "vi"
            } else {
                "emacs"
            }
            .to_owned(),
        ),
        ("keymap", bindings.keymap().to_string()),
        (
            "completion-prefix-display-length",
            options.completion_prefix_display_length.to_string(),
        ),
        (
            "completion-query-items",
            options.completion_query_items.to_string(),
        ),
    ])
    .chain(
        [
            (
                "active-region-end-color",
                &options.active_region_end_color,
                r"\e[27m",
            ),
            (
                "active-region-start-color",
                &options.active_region_start_color,
                r"\e[7m",
            ),
        ]
        .into_iter()
        .filter_map(|(name, value, default)| {
            value
                .clone()
                .or_else(|| shell.options().interactive.then(|| default.to_owned()))
                .map(|value| (name, value))
        }),
    )
    .collect()
}

fn format_optional_setting(value: Option<usize>) -> String {
    value.map_or_else(|| "-1".into(), |n| n.to_string())
}

fn parse_key_sequence(
    input: &str,
    options: &ReadlineOptions,
) -> Result<interfaces::KeySequence, BindError> {
    // First trim any whitespace.
    let input = input.trim();

    let parsed = brush_parser::readline_binding::parse_key_sequence(input)?;
    let abstract_seq = key_sequence_to_abstract_strokes(&parsed, options)?;

    Ok(abstract_seq)
}

fn parse_key_sequence_and_shell_command(
    input: &str,
    options: &ReadlineOptions,
) -> Result<(interfaces::KeySequence, String), BindError> {
    tracing::debug!(target: trace_categories::INPUT,
        "parsing key binding entry: '{input}'"
    );

    // First trim any whitespace.
    let input = input.trim();

    // This should be something of the form:
    //     "KEY-SEQUENCE": SHELL-COMMAND
    let binding = brush_parser::readline_binding::parse_key_sequence_shell_cmd_binding(input)?;
    let abstract_seq = key_sequence_to_abstract_strokes(&binding.seq, options)?;

    Ok((abstract_seq, binding.shell_cmd))
}

#[derive(Debug)]
#[allow(dead_code, reason = "not all variants implemented yet")]
enum BindableReadlineTarget {
    Function(interfaces::InputFunction),
    Macro(interfaces::KeySequence),
}

fn parse_key_sequence_and_readline_target(
    input: &str,
    options: &ReadlineOptions,
) -> Result<(interfaces::KeySequence, BindableReadlineTarget), BindError> {
    tracing::debug!(target: trace_categories::INPUT,
        "parsing key binding entry: '{input}'"
    );

    // First trim any whitespace.
    let input = input.trim();

    // This should be of one of these forms:
    //     "KEY-SEQUENCE":function-name
    //     "KEY-SEQUENCE":readline-command
    let binding = brush_parser::readline_binding::parse_key_sequence_readline_binding(input)?;
    let abstract_seq = key_sequence_to_abstract_strokes(&binding.seq, options)?;

    match binding.target {
        brush_parser::readline_binding::ReadlineTarget::Function(func_name) => {
            let func = parse_readline_function(func_name.as_str())?;
            Ok((abstract_seq, BindableReadlineTarget::Function(func)))
        }
        brush_parser::readline_binding::ReadlineTarget::Macro(target_seq_str) => {
            let parsed_target =
                brush_parser::readline_binding::parse_key_sequence(&target_seq_str)?;
            let abstract_target = key_sequence_to_abstract_strokes(&parsed_target, options)?;
            Ok((abstract_seq, BindableReadlineTarget::Macro(abstract_target)))
        }
    }
}

fn bind_key_sequence_to_shell_cmd(
    bindings: &mut dyn interfaces::KeyBindings,
    key_sequence: interfaces::KeySequence,
    command: String,
) -> Result<(), BindError> {
    tracing::debug!(target: trace_categories::INPUT,
        "binding key sequence: '{key_sequence}' => command '{command}'"
    );

    bindings.bind(key_sequence, interfaces::KeyAction::ShellCommand(command))?;

    Ok(())
}

fn bind_key_sequence_to_readline_target(
    bindings: &mut dyn interfaces::KeyBindings,
    key_sequence: interfaces::KeySequence,
    target: BindableReadlineTarget,
) -> Result<(), BindError> {
    match target {
        BindableReadlineTarget::Function(func) => {
            tracing::debug!(target: trace_categories::INPUT,
                "binding key sequence: '{key_sequence}' => readline function '{func}'"
            );

            bindings.bind(key_sequence, interfaces::KeyAction::DoInputFunction(func))?;
        }
        BindableReadlineTarget::Macro(cmd_macro) => {
            tracing::debug!(target: trace_categories::INPUT,
                "binding key sequence: '{key_sequence}' => readline macro '{cmd_macro}'"
            );

            bindings.define_macro(key_sequence, cmd_macro)?;
        }
    }

    Ok(())
}

fn key_sequence_to_abstract_strokes(
    seq: &brush_parser::readline_binding::KeySequence,
    options: &ReadlineOptions,
) -> Result<interfaces::KeySequence, BindError> {
    let phys_strokes = brush_parser::readline_binding::key_sequence_to_strokes(seq)?;

    // Lift from key codes to abstract keys.
    let mut abstract_strokes = vec![];
    let mut key_code_bytes = vec![];
    let mut uninterpretable = false;
    for mut phys_stroke in phys_strokes {
        // Readline treats C-? as rubout rather than an ASCII C0 control.
        if phys_stroke.control && phys_stroke.key_code.as_slice() == b"?" {
            phys_stroke.control = false;
            phys_stroke.key_code[0] = 0x7f;
        }
        let legacy_meta = phys_stroke.meta && !options.force_meta_prefix && !options.convert_meta;
        let literal_meta_prefix = phys_stroke.meta && !legacy_meta;
        let mut key = portable_key(phys_stroke.key_code.as_slice());
        if let [byte] = phys_stroke.key_code.as_slice()
            && !byte.is_ascii()
        {
            uninterpretable = true;
        }

        // If we couldn't interpret it directly but we see it starts with the escape character,
        // try to see if we can parse it as an Alt+<key> sequence.
        if key.is_none() && phys_stroke.key_code.len() > 1 && phys_stroke.key_code[0] == b'\x1b' {
            key = portable_key(&phys_stroke.key_code[1..]);
            if key.is_some() {
                phys_stroke.meta = true;
            }
        }

        // When storing as bytes, apply control modifier to the key code.
        let mut raw_bytes = phys_stroke.key_code.clone();
        if phys_stroke.control {
            for byte in &mut raw_bytes {
                // Control characters are computed by ANDing with 0x1F
                *byte &= 0x1F;
            }
        }
        if legacy_meta {
            raw_bytes[0] |= 0x80;
            uninterpretable = true;
        } else if literal_meta_prefix {
            raw_bytes.insert(0, b'\x1b');
        }
        key_code_bytes.push(raw_bytes);

        if let Some(key) = key {
            abstract_strokes.push(interfaces::KeyStroke {
                alt: phys_stroke.meta,
                control: phys_stroke.control,
                shift: false,
                key,
            });
        } else if !phys_stroke.control
            && !phys_stroke.meta
            && !phys_stroke.key_code.starts_with(b"\x1b")
            && let Ok(text) = std::str::from_utf8(&phys_stroke.key_code)
            && text.chars().all(|c| !c.is_control())
        {
            abstract_strokes.extend(
                text.chars()
                    .map(|c| interfaces::KeyStroke::from(interfaces::Key::Character(c))),
            );
        } else {
            uninterpretable = true;
        }
    }

    if uninterpretable {
        Ok(interfaces::KeySequence::Bytes(key_code_bytes))
    } else {
        Ok(interfaces::KeySequence::Strokes(abstract_strokes))
    }
}

fn portable_key(code: &[u8]) -> Option<interfaces::Key> {
    use interfaces::Key;
    let key = match code {
        b"\t" => Key::Tab,
        b"\r" | b"\n" => Key::Enter,
        b"\x7f" => Key::Backspace,
        b"\x1b" => Key::Escape,
        b"\x1b[Z" => Key::BackTab,
        b"\x1b[A" | b"\x1bOA" => Key::Up,
        b"\x1b[B" | b"\x1bOB" => Key::Down,
        b"\x1b[C" | b"\x1bOC" => Key::Right,
        b"\x1b[D" | b"\x1bOD" => Key::Left,
        b"\x1b[H" | b"\x1bOH" | b"\x1b[1~" | b"\x1b[7~" => Key::Home,
        b"\x1b[F" | b"\x1bOF" | b"\x1b[4~" | b"\x1b[8~" => Key::End,
        b"\x1b[2~" => Key::Insert,
        b"\x1b[3~" => Key::Delete,
        b"\x1b[5~" => Key::PageUp,
        b"\x1b[6~" => Key::PageDown,
        b"\x1bOP" | b"\x1b[11~" => Key::F(1),
        b"\x1bOQ" | b"\x1b[12~" => Key::F(2),
        b"\x1bOR" | b"\x1b[13~" => Key::F(3),
        b"\x1bOS" | b"\x1b[14~" => Key::F(4),
        b"\x1b[15~" => Key::F(5),
        b"\x1b[17~" => Key::F(6),
        b"\x1b[18~" => Key::F(7),
        b"\x1b[19~" => Key::F(8),
        b"\x1b[20~" => Key::F(9),
        b"\x1b[21~" => Key::F(10),
        b"\x1b[23~" => Key::F(11),
        b"\x1b[24~" => Key::F(12),
        _ => return sys::input::try_get_key_from_key_code(code),
    };
    Some(key)
}

fn reusable_key_sequence(seq: &KeySequence) -> String {
    use interfaces::Key;
    use std::fmt::Write as _;

    let mut result = String::new();
    match seq {
        KeySequence::Strokes(strokes) => {
            for stroke in strokes {
                if stroke.alt {
                    result.push_str(r"\e");
                }
                if stroke.control {
                    result.push_str(r"\C-");
                }
                let code = match stroke.key {
                    Key::Character(c) => {
                        if c == '"' || c == '\\' {
                            result.push('\\');
                        }
                        result.push(c);
                        continue;
                    }
                    Key::Tab => r"\t",
                    Key::Enter => r"\r",
                    Key::Backspace => r"\x7f",
                    Key::Escape => r"\e",
                    Key::BackTab => r"\e[Z",
                    Key::Up => r"\e[A",
                    Key::Down => r"\e[B",
                    Key::Left => r"\e[D",
                    Key::Right => r"\e[C",
                    Key::Home => r"\e[H",
                    Key::End => r"\e[F",
                    Key::Insert => r"\e[2~",
                    Key::Delete => r"\e[3~",
                    Key::PageUp => r"\e[5~",
                    Key::PageDown => r"\e[6~",
                    Key::F(n) => {
                        const CODES: [u8; 12] = [11, 12, 13, 14, 15, 17, 18, 19, 20, 21, 23, 24];
                        if let Some(code) = n
                            .checked_sub(1)
                            .and_then(|index| CODES.get(usize::from(index)))
                        {
                            let _ = write!(result, r"\e[{code}~");
                        }
                        continue;
                    }
                };
                result.push_str(code);
            }
        }
        KeySequence::Bytes(items) => {
            for &byte in items.iter().flatten() {
                match byte {
                    b'\x1b' => result.push_str(r"\e"),
                    1..=26 => {
                        let _ = write!(result, r"\C-{}", char::from(b'a' + byte - 1));
                    }
                    b' '..=b'~' => {
                        if matches!(byte, b'"' | b'\\') {
                            result.push('\\');
                        }
                        result.push(char::from(byte));
                    }
                    0x80..=0xff => {
                        let _ = write!(result, r"\{byte:03o}");
                    }
                    _ => {
                        let _ = write!(result, r"\x{byte:02x}");
                    }
                }
            }
        }
    }
    result
}

fn reusable_shell_command(command: &str) -> String {
    let mut result = String::new();
    for byte in command.bytes() {
        let ascii = byte & 0x7f;
        if ascii != byte {
            result.push_str(r"\M-");
        }
        match ascii {
            0x1b => result.push_str(r"\e"),
            0x7f => result.push_str(r"\C-?"),
            0..=31 => {
                result.push_str(r"\C-");
                result.push(char::from(ascii | 0x40).to_ascii_lowercase());
            }
            b'"' | b'\\' => {
                result.push('\\');
                result.push(char::from(ascii));
            }
            _ => result.push(char::from(ascii)),
        }
    }
    result
}

fn parse_readline_function(func_name: &str) -> Result<interfaces::InputFunction, BindError> {
    interfaces::InputFunction::from_str(func_name)
        .map_err(|_err| BindError::UnknownKeyBindingFunction(func_name.to_owned()))
}

const fn to_onoff(value: bool) -> &'static str {
    if value { "on" } else { "off" }
}

fn display_funcs_and_bindings(
    bindings: &dyn interfaces::KeyBindings,
    context: &brush_core::ExecutionContext<'_, impl brush_core::ShellExtensions>,
    reusable: bool,
    filter: &[String],
) -> Result<(), BindError> {
    let mut sequences_by_func: HashMap<InputFunction, Vec<KeySequence>> = HashMap::new();
    for (seq, action) in &bindings.get_current() {
        let KeyAction::DoInputFunction(func) = action else {
            continue;
        };

        sequences_by_func
            .entry(func.clone())
            .or_default()
            .push(seq.clone());
    }

    let sorted_funcs = interfaces::InputFunction::iter()
        .map(|function| (function.to_string(), function))
        .sorted_by(|left, right| left.0.cmp(&right.0));

    for (name, func) in sorted_funcs {
        if !filter.is_empty() && !filter.contains(&name) {
            continue;
        }
        match sequences_by_func.get(&func) {
            Some(seqs) if reusable => {
                for seq in seqs {
                    writeln!(
                        context.stdout(),
                        "\"{}\": {func}",
                        reusable_key_sequence(seq)
                    )?;
                }
            }
            Some(seqs) => {
                writeln!(
                    context.stdout(),
                    "{func} can be found on {}.",
                    seqs.iter().map(|seq| std::format!("\"{seq}\"")).join(", ")
                )?;
            }
            None if reusable => {
                writeln!(context.stdout(), "# {func} (not bound)")?;
            }
            None => {
                writeln!(context.stdout(), "{func} is not bound to any keys")?;
            }
        }
    }

    Ok(())
}

fn display_macros(
    bindings: &dyn interfaces::KeyBindings,
    context: &brush_core::ExecutionContext<'_, impl brush_core::ShellExtensions>,
    reusable: bool,
) -> Result<(), BindError> {
    for (left, right) in bindings.get_macros() {
        if reusable {
            writeln!(
                context.stdout(),
                "\"{}\": \"{}\"",
                reusable_key_sequence(&left),
                reusable_key_sequence(&right)
            )?;
        } else {
            writeln!(
                context.stdout(),
                "{} outputs {}",
                reusable_key_sequence(&left),
                reusable_key_sequence(&right)
            )?;
        }
    }

    Ok(())
}

fn find_key_seqs_bound_to_function(
    bindings: &dyn interfaces::KeyBindings,
    func_str: &str,
) -> Result<Vec<interfaces::KeySequence>, BindError> {
    let Ok(func_to_find) = InputFunction::from_str(func_str) else {
        return Err(BindError::UnknownFunction(func_str.to_owned()));
    };

    let mut found_seqs = vec![];

    for (seq, action) in &bindings.get_current() {
        if let KeyAction::DoInputFunction(func) = action
            && *func == func_to_find
        {
            found_seqs.push(seq.clone());
        }
    }

    Ok(found_seqs)
}

#[cfg(test)]
mod tests {
    use super::*;
    use pretty_assertions::{assert_eq, assert_matches};

    #[test]
    fn parse_example_key_sequence_and_readline_func() {
        let (key_seq, target) = parse_key_sequence_and_readline_target(
            r#""\C-a":beginning-of-line"#,
            &ReadlineOptions::default(),
        )
        .unwrap();

        assert_eq!(
            key_seq,
            interfaces::KeySequence::Strokes(vec![interfaces::KeyStroke {
                alt: false,
                control: true,
                shift: false,
                key: interfaces::Key::Character('a'),
            }])
        );

        assert_matches!(
            target,
            BindableReadlineTarget::Function(interfaces::InputFunction::BeginningOfLine)
        );
    }

    #[test]
    fn parse_escape_char_key_binding() {
        let (key_seq, target) = parse_key_sequence_and_readline_target(
            r#""\er":transpose-chars"#,
            &ReadlineOptions::default(),
        )
        .unwrap();

        assert_eq!(
            key_seq,
            interfaces::KeySequence::Strokes(vec![interfaces::KeyStroke {
                alt: true,
                control: false,
                shift: false,
                key: interfaces::Key::Character('r'),
            }])
        );

        assert_matches!(
            target,
            BindableReadlineTarget::Function(interfaces::InputFunction::TransposeChars)
        );
    }
}
