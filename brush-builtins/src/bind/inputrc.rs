//! Evaluation of Readline initialization files.

use std::{
    io::Write,
    path::{Path, PathBuf},
};

use brush_core::{
    ExecutionContext, Shell, ShellExtensions, interfaces::KeyBindings, trace_categories,
};

use super::{BindError, apply_readline_input, readline_variables};

const MAX_INCLUDE_DEPTH: usize = 64;
// The inputrc dialect this reader implements, used by version predicates.
const SYNTAX_VERSION: (u32, u32) = (8, 3);

pub(super) fn load(
    path: &str,
    context: &mut ExecutionContext<'_, impl ShellExtensions>,
    bindings: &mut dyn KeyBindings,
) -> Result<(), BindError> {
    let mut stderr = context.stderr();
    read_top_level(path, context.shell, bindings, &mut stderr)
}

/// Loads the interactive configuration from `INPUTRC`, or the user and system
/// fallback files when it is unset.
pub async fn load_default_inputrc(shell: &mut Shell<impl ShellExtensions>) {
    let Some(bindings) = shell.key_bindings().cloned() else {
        return;
    };
    let mut bindings = bindings.lock().await;
    let mut stderr = std::io::stderr();
    if let Some(path) = shell.env_str("INPUTRC").map(std::borrow::Cow::into_owned) {
        let _ = read_top_level(&path, shell, &mut *bindings, &mut stderr);
    } else if read_top_level("~/.inputrc", shell, &mut *bindings, &mut stderr).is_err() {
        #[cfg(unix)]
        let _ = read_top_level("/etc/inputrc", shell, &mut *bindings, &mut stderr);
    }
}

fn read_top_level(
    path: &str,
    shell: &mut Shell<impl ShellExtensions>,
    bindings: &mut dyn KeyBindings,
    stderr: &mut impl Write,
) -> Result<(), BindError> {
    read_file(path, shell, bindings, stderr, &mut Vec::new())?;
    shell.readline_options_mut().init_file = Some(resolve_path(shell, path));
    Ok(())
}

#[derive(Clone, Copy)]
struct Conditional {
    parent_active: bool,
    matches: bool,
    in_else: bool,
}

impl Conditional {
    const fn active(self) -> bool {
        self.parent_active && self.matches != self.in_else
    }
}

fn read_file(
    raw_path: &str,
    shell: &mut Shell<impl ShellExtensions>,
    bindings: &mut dyn KeyBindings,
    stderr: &mut impl Write,
    stack: &mut Vec<PathBuf>,
) -> Result<(), BindError> {
    let path = resolve_path(shell, raw_path);
    let identity = path.canonicalize().unwrap_or_else(|_| path.clone());
    if stack.len() == MAX_INCLUDE_DEPTH || stack.contains(&identity) {
        return Err(BindError::BindingFile {
            path: raw_path.to_owned(),
            source: std::io::Error::other("recursive inputrc include"),
        });
    }
    let contents = std::fs::read_to_string(&path).map_err(|source| BindError::BindingFile {
        path: raw_path.to_owned(),
        source,
    })?;
    stack.push(identity);
    read_lines(&contents, &path, raw_path, shell, bindings, stderr, stack);
    stack.pop();
    Ok(())
}

fn read_lines(
    contents: &str,
    path: &Path,
    raw_path: &str,
    shell: &mut Shell<impl ShellExtensions>,
    bindings: &mut dyn KeyBindings,
    stderr: &mut impl Write,
    stack: &mut Vec<PathBuf>,
) {
    let mut conditions: Vec<Conditional> = Vec::new();
    let mut diagnostic_file_active = true;
    for (index, line) in contents.lines().enumerate() {
        let line = line.trim();
        if line.is_empty() || line.starts_with('#') {
            continue;
        }
        let active = conditions.last().is_none_or(|frame| frame.active());
        let Some(directive) = line.strip_prefix('$') else {
            if active && let Err(err) = apply_readline_input(shell, bindings, line) {
                if let BindError::UnknownVariable(name) = err {
                    if diagnostic_file_active {
                        let _ = writeln!(
                            stderr,
                            "readline: {raw_path}: line {}: {name}: unknown variable name",
                            index + 1
                        );
                    } else {
                        let _ = writeln!(stderr, "readline: {name}: unknown variable name");
                    }
                } else {
                    tracing::debug!(target: trace_categories::INPUT,
                        "{}:{}: {err}", path.display(), index + 1);
                }
            }
            continue;
        };
        let (command, argument) = directive
            .split_once(char::is_whitespace)
            .map_or((directive, ""), |(name, argument)| (name, argument.trim()));
        let message = match command {
            "if" => {
                conditions.push(Conditional {
                    parent_active: active,
                    matches: active && matches_condition(shell, bindings, argument),
                    in_else: false,
                });
                None
            }
            "else" => match conditions.last_mut() {
                Some(frame) if !frame.in_else => {
                    frame.in_else = true;
                    None
                }
                Some(_) => Some("multiple $else directives"),
                None => Some("$else without matching $if"),
            },
            "endif" => conditions
                .pop()
                .is_none()
                .then_some("$endif without matching $if"),
            "include" => {
                // Readline interprets relative includes from the shell's working directory.
                // As with an unreadable optional user init file, a missing include is skipped.
                if active {
                    match read_file(argument, shell, bindings, stderr, stack) {
                        // Readline stops reporting the parent file after a successful include.
                        Ok(()) => diagnostic_file_active = false,
                        Err(err) => tracing::debug!(target: trace_categories::INPUT,
                            "{}:{}: {err}", path.display(), index + 1),
                    }
                }
                None
            }
            _ => Some("unknown parser directive"),
        };
        if let Some(message) = message {
            let _ = writeln!(
                stderr,
                "readline: {}: line {}: {message}",
                path.display(),
                index + 1
            );
        }
    }
}

fn resolve_path(shell: &Shell<impl ShellExtensions>, path: &str) -> PathBuf {
    if let Some(path) = path.strip_prefix("~/")
        && let Some(home) = shell.env_str("HOME")
    {
        return shell.absolute_path(Path::new(home.as_ref()).join(path));
    }
    shell.absolute_path(path)
}

fn matches_condition(
    shell: &Shell<impl ShellExtensions>,
    bindings: &dyn KeyBindings,
    test: &str,
) -> bool {
    if let Some(mode) = test.strip_prefix("mode=") {
        let actual = if bindings
            .vi_editing()
            .unwrap_or_else(|| shell.options().vi_mode)
        {
            "vi"
        } else {
            "emacs"
        };
        return mode.eq_ignore_ascii_case(actual);
    }
    if let Some(term) = test.strip_prefix("term=") {
        return shell.env_str("TERM").is_some_and(|actual| {
            actual == term || actual.split_once('-').is_some_and(|(name, _)| name == term)
        });
    }
    if let Some(condition) = test.strip_prefix("version")
        && let Some((operator, value)) = comparison(condition)
    {
        let (major, minor) = value.split_once('.').unwrap_or((value, "0"));
        return major
            .parse()
            .ok()
            .zip(minor.parse().ok())
            .is_some_and(|version| {
                let order = SYNTAX_VERSION.cmp(&version);
                match operator {
                    "=" | "==" => order.is_eq(),
                    "!=" => order.is_ne(),
                    "<" => order.is_lt(),
                    ">" => order.is_gt(),
                    "<=" => order.is_le(),
                    ">=" => order.is_ge(),
                    _ => false,
                }
            });
    }
    if let Some((name, condition)) = test.split_once(char::is_whitespace) {
        let Some((operator, value)) = comparison(condition) else {
            return false;
        };
        return readline_variables(shell, bindings)
            .into_iter()
            .find(|(variable, _)| *variable == name)
            .is_some_and(|(_, actual)| match operator {
                "=" | "==" => actual.eq_ignore_ascii_case(value),
                "!=" => !actual.eq_ignore_ascii_case(value),
                _ => false,
            });
    }
    test.eq_ignore_ascii_case("bash") || test.eq_ignore_ascii_case("brush")
}

fn comparison(condition: &str) -> Option<(&str, &str)> {
    let condition = condition.trim_start();
    ["==", "!=", "<=", ">=", "=", "<", ">"]
        .into_iter()
        .find_map(|op| condition.strip_prefix(op).map(|rhs| (op, rhs.trim())))
}
