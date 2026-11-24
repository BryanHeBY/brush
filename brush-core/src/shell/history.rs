//! History management for shells.

use std::{collections::BTreeSet, path::PathBuf};

use crate::{error, history::HistoryControl, openfiles, patterns};

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
