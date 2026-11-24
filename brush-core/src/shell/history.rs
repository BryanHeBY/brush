//! History management for shells.

use std::{
    collections::BTreeSet,
    io::{Seek, SeekFrom},
    path::{Path, PathBuf},
};

use crate::{error, history::HistoryControl, openfiles, patterns};

/// Joins the lines of a multi-line command the way bash's `cmdhist` does: lines that end a
/// simple command are separated by `"; "`, while a line ending at a keyword or a continuation
/// (`do`, `then`, `|`, ...) gets a space instead. Leading whitespace on the next line is kept.
fn join_multiline_command(command: &str) -> String {
    let mut joined = String::new();

    for (index, line) in command.lines().enumerate() {
        if index > 0 {
            let previous = joined.trim_end();
            let last_word = previous.split_whitespace().last().unwrap_or_default();
            let keyword = matches!(
                last_word,
                "do" | "then"
                    | "else"
                    | "elif"
                    | "if"
                    | "while"
                    | "until"
                    | "for"
                    | "case"
                    | "in"
                    | "{"
                    | "("
                    | "!"
                    | "|"
                    | "||"
                    | "&&"
                    | "&"
                    | ";"
            );
            let continuation = previous.ends_with('\\');

            if keyword || continuation || previous.ends_with(';') {
                joined.push(' ');
            } else {
                joined.push_str("; ");
            }
        }
        joined.push_str(line);
    }

    joined
}

impl<SE: crate::extensions::ShellExtensions> crate::Shell<SE> {
    pub(super) fn load_history(&self) -> Result<Option<crate::history::History>, error::Error> {
        const MAX_FILE_SIZE_FOR_HISTORY_IMPORT: u64 = 1024 * 1024 * 1024; // 1 GiB

        let Some(history_path) = self.history_file_path() else {
            return Ok(None);
        };

        let mut options = std::fs::File::options();
        options.read(true);

        let mut history_file =
            self.open_file(&options, history_path, &self.default_exec_params())?;

        // Check on the file's size.
        if let openfiles::OpenFile::File(file) = &mut history_file {
            let file_metadata = file.metadata()?;
            let file_size = file_metadata.len();

            // If the file is empty, no reason to try reading it. Note that this will also
            // end up excluding non-regular files that report a 0 file size but appear
            // to have contents when read.
            if file_size == 0 {
                return Ok(None);
            }

            // Bail if the file is unrealistically large. For now we just refuse to import it.
            if file_size > MAX_FILE_SIZE_FOR_HISTORY_IMPORT {
                return Err(error::ErrorKind::HistoryFileTooLargeToImport.into());
            }
        }

        let mut history = crate::history::History::import(history_file)?;
        if let Some(limit) = self.history_size_limit() {
            history.retain_last(limit);
        }
        Ok(Some(history))
    }

    /// Returns the number of entries the in-memory history may hold, or `None` for no limit.
    /// `HISTSIZE` takes precedence; an inputrc `history-size` still caps a history that the
    /// environment leaves uncapped.
    fn history_size_limit(&self) -> Option<usize> {
        let from_env = Self::env_usize(self.env_str("HISTSIZE"));
        match (from_env, self.readline_options.history_size) {
            (Some(env), Some(readline)) => Some(env.min(readline)),
            (Some(env), None) => Some(env),
            (None, readline) => readline,
        }
    }

    /// Returns the number of entries the history file may hold, or `None` for no limit.
    fn history_file_size_limit(&self) -> Option<usize> {
        Self::env_usize(self.env_str("HISTFILESIZE"))
    }

    /// Parses a size variable. As bash does here, an empty, negative, or otherwise invalid
    /// value means "no limit"; zero is a real limit.
    fn env_usize(value: Option<std::borrow::Cow<'_, str>>) -> Option<usize> {
        value?.trim().parse().ok()
    }

    /// Returns whether `HISTIGNORE` says to leave `command` out. Patterns are shell globs, and
    /// a pattern of `&` means the command just recorded before this one.
    fn history_ignored(&self, command: &str) -> Result<bool, error::Error> {
        let Some(ignore) = self.env_str("HISTIGNORE") else {
            return Ok(false);
        };
        let previous = self
            .history
            .as_ref()
            .and_then(crate::history::History::last)
            .map(|item| item.command_line.as_str());

        for pattern in ignore.split(':') {
            if pattern.is_empty() {
                continue;
            }
            if pattern == "&" {
                if previous == Some(command) {
                    return Ok(true);
                }
                continue;
            }

            let pattern = patterns::Pattern::from(pattern)
                .set_extended_globbing(self.options.extended_globbing);
            if pattern.exactly_matches(command)? {
                return Ok(true);
            }
        }

        Ok(false)
    }

    /// Returns the path to the history file used by the shell, if one is set.
    pub fn history_file_path(&self) -> Option<PathBuf> {
        self.env_str("HISTFILE")
            .filter(|s| !s.is_empty())
            .map(|s| PathBuf::from(s.into_owned()))
    }

    /// Returns the path to the history file used by the shell, if one is set.
    pub fn history_time_format(&self) -> Option<String> {
        self.env_str("HISTTIMEFORMAT").map(|s| s.into_owned())
    }

    /// Reads history from `path` (or the shell's history file) into the current session.
    ///
    /// `new_only` reads from the position reached by the last read or save, so a shared file
    /// can contribute the entries another session wrote without re-importing the whole file.
    /// A file shorter than the recorded position was truncated or replaced; it is read from the
    /// beginning.
    pub fn read_history_file(
        &mut self,
        path: Option<&Path>,
        new_only: bool,
    ) -> Result<(), error::Error> {
        let Some(path) = path
            .map(Path::to_path_buf)
            .or_else(|| self.history_file_path())
        else {
            return Ok(());
        };

        let mut file = std::fs::File::open(&path)?;
        let length = file.metadata()?.len();

        let offset = if new_only {
            match &self.history_read_position {
                Some((position_path, position))
                    if position_path == &path && *position <= length =>
                {
                    *position
                }
                _ => 0,
            }
        } else {
            0
        };
        file.seek(SeekFrom::Start(offset))?;

        let size_limit = self.history_size_limit();
        let Some(history) = &mut self.history else {
            return Err(error::ErrorKind::HistoryNotEnabled.into());
        };
        history.append_from(file)?;
        if let Some(limit) = size_limit {
            history.retain_last(limit);
        }
        self.history_read_position = Some((path, length));

        Ok(())
    }

    /// Reads entries other sessions appended to the shell's history file since this session last
    /// read or saved it.
    pub fn read_new_history(&mut self) -> Result<(), error::Error> {
        // A session that has not saved yet has no file; that is not an error for share mode.
        let Some(path) = self.history_file_path() else {
            return Ok(());
        };
        let Ok(file) = std::fs::File::open(&path) else {
            return Ok(());
        };

        let size_limit = self.history_size_limit();
        let Some(history) = &mut self.history else {
            return Err(error::ErrorKind::HistoryNotEnabled.into());
        };
        history.append_from_unique(file)?;
        if let Some(limit) = size_limit {
            history.retain_last(limit);
        }

        Ok(())
    }

    /// Saves history back to any backing storage.
    pub fn save_history(&mut self) -> Result<(), error::Error> {
        if let Some(history_file_path) = self.history_file_path() {
            // See if there's *any* time format configured. That triggers writing out
            // timestamps.
            let write_timestamps = self.env.is_set("HISTTIMEFORMAT");
            // Brush appends by default; `shopt -u histappend` selects bash's overwrite.
            // Non-interactive shells have no `histappend` setting of their own and keep the
            // historical append behavior when history is enabled explicitly.
            let append = !self.options.interactive || self.options.append_to_history_file;
            let file_limit = self.history_file_size_limit();

            if let Some(history) = &mut self.history {
                history.flush(
                    history_file_path.as_path(),
                    append,
                    append, /* unsaved items only when appending */
                    write_timestamps,
                )?;

                if let Some(limit) = file_limit {
                    history.truncate_file(history_file_path.as_path(), limit)?;
                }
            }

            // Remember where this session's writes end, so share mode doesn't import them
            // again as if another shell had written them.
            if let Ok(metadata) = std::fs::metadata(&history_file_path) {
                self.history_read_position = Some((history_file_path, metadata.len()));
            }
        }

        Ok(())
    }

    /// Adds a command to history. Like bash, an interactive shell leaves it out or makes room
    /// for it as the colon-separated values of `HISTCONTROL` say:
    ///
    /// - `ignorespace`: leave it out if it starts with a space.
    /// - `ignoredups`: leave it out if it's the same as the last entry.
    /// - `ignoreboth`: both of those.
    /// - `erasedups`: first remove any earlier entries the same as it.
    pub fn add_to_history(&mut self, command: &str) -> Result<(), error::Error> {
        // `set +o history` turns off automatic recording; explicit `history -s` still adds.
        if !self.options.enable_command_history {
            return Ok(());
        }

        // Like bash, save the command as typed, surrounding whitespace and all, but leave out
        // blank ones.
        if command.trim().is_empty() {
            return Ok(());
        }

        // `cmdhist` joins the lines of a multi-line command into one entry; `lithist` keeps
        // them. The caller has already trimmed the final newline.
        let command = if self.options.save_multiline_cmds_in_history
            && !self.options.embed_newlines_in_multiline_cmds_in_history
        {
            std::borrow::Cow::Owned(join_multiline_command(command))
        } else {
            std::borrow::Cow::Borrowed(command)
        };
        let command = command.as_ref();

        if self.history_ignored(command)? {
            return Ok(());
        }

        let controls = if self.options.interactive {
            self.env_str("HISTCONTROL")
                .map(|value| HistoryControl::parse_list(&value))
                .unwrap_or_default()
        } else {
            BTreeSet::new()
        };

        let size_limit = self.history_size_limit();

        let Some(history) = &mut self.history else {
            return Ok(());
        };

        if controls.contains(&HistoryControl::IgnoreSpace) && command.starts_with(' ') {
            return Ok(());
        }

        if controls.contains(&HistoryControl::IgnoreDups)
            && history
                .last()
                .is_some_and(|last| last.command_line == command)
        {
            return Ok(());
        }
        if controls.contains(&HistoryControl::EraseDups) {
            history.retain(|item| item.command_line != command);
        }

        history.add(crate::history::Item {
            id: 0,
            command_line: command.to_owned(),
            timestamp: Some(crate::history::ItemTimestamp::now()),
            dirty: true,
        })?;
        if let Some(limit) = size_limit {
            history.retain_last(limit);
        }

        Ok(())
    }
}
