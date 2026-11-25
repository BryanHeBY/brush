//! History management for shells.

use std::{
    collections::BTreeSet,
    io::{Read, Seek, SeekFrom},
    path::{Path, PathBuf},
};

use crate::{
    error,
    history::HistoryControl,
    history_file::{
        OwnAppend, ReadPosition, complete_record_end, lock_history_file, tail_checksum,
    },
    openfiles, patterns,
};

/// Joins the lines of a multi-line command the way bash's `cmdhist` does.
///
/// Newlines inside quotes and here-document bodies are part of the command and are kept as
/// written. Elsewhere the entire command is parsed after trying `"; "` and then a space at each
/// newline. This is stricter than inspecting the preceding word: it keeps two commands inside a
/// block separate, while still leaving a space after `then`, `do`, or a trailing pipe.
fn join_multiline_command(command: &str, parser_options: &brush_parser::ParserOptions) -> String {
    if command.contains("<<") && !command.contains("<<<") {
        // Bash keeps a trailing newline as part of a here-document history entry.
        return format!("{command}\n");
    }
    if has_quoted_newline(command) || has_unquoted_hash(command) {
        return command.to_owned();
    }

    let lines: Vec<&str> = command.lines().collect();
    if lines.len() <= 1 {
        return command.to_owned();
    }

    let parse = |source: &str| {
        brush_parser::Parser::new(source.as_bytes(), parser_options)
            .parse_program()
            .map(|_| ())
    };
    let mut separators = vec!["\n"; lines.len() - 1];

    for boundary in 0..separators.len() {
        if lines[boundary].ends_with('\\') {
            separators[boundary] = "";
            continue;
        }

        separators[boundary] = "; ";
        if parse(&render_history_lines(&lines, &separators)).is_err() {
            separators[boundary] = " ";
            if parse(&render_history_lines(&lines, &separators)).is_err() {
                separators[boundary] = "\n";
            }
        }
    }

    let joined = render_history_lines(&lines, &separators);
    if parse(&joined).is_ok() {
        joined
    } else {
        command.to_owned()
    }
}

/// Renders source lines with the chosen history separators, removing backslash-newline
/// continuations.
fn render_history_lines(lines: &[&str], separators: &[&str]) -> String {
    let mut rendered = String::new();
    for (index, line) in lines.iter().enumerate() {
        if index > 0 {
            rendered.push_str(separators[index - 1]);
        }
        if let Some(without_backslash) = line.strip_suffix('\\') {
            rendered.push_str(without_backslash);
        } else {
            rendered.push_str(line);
        }
    }
    rendered
}

/// Returns true if `command` contains a `#` outside quotes. A newline after an inline
/// comment is a shell syntax boundary; joining it with `; ` would put the following command
/// inside the comment.
fn has_unquoted_hash(command: &str) -> bool {
    let mut quote = None;
    let mut escaped = false;

    for character in command.chars() {
        if escaped {
            escaped = false;
            continue;
        }
        if let Some(open) = quote {
            if character == '\\' && open == '"' {
                escaped = true;
            } else if character == open {
                quote = None;
            }
        } else {
            if character == '#' {
                return true;
            }
            if character == '\'' || character == '"' {
                quote = Some(character);
            } else if character == '\\' {
                escaped = true;
            }
        }
    }
    false
}

/// Returns true if a newline occurs while a single- or double-quoted string is open.
fn has_quoted_newline(command: &str) -> bool {
    let mut quote = None;
    let mut escaped = false;

    for character in command.chars() {
        if escaped {
            escaped = false;
            continue;
        }

        match quote {
            Some(open) => {
                if character == '\\' && open == '"' {
                    escaped = true;
                } else if character == open {
                    quote = None;
                } else if character == '\n' {
                    return true;
                }
            }
            None => {
                if character == '\'' || character == '"' {
                    quote = Some(character);
                } else if character == '\\' {
                    escaped = true;
                }
            }
        }
    }

    false
}

/// A cheap checksum of the last (up to) 256 bytes of `bytes`, used to notice that a file's
/// prefix was replaced in place even when its size and inode did not change.
/// Splits `HISTIGNORE` on unescaped colons, removing the backslash that escapes a colon.
fn split_ignore_patterns(ignore: &str) -> Vec<String> {
    let mut patterns = Vec::new();
    let mut current = String::new();
    let mut characters = ignore.chars();

    while let Some(character) = characters.next() {
        match character {
            '\\' => {
                if let Some(next) = characters.next() {
                    if next != ':' {
                        current.push('\\');
                    }
                    current.push(next);
                } else {
                    current.push('\\');
                }
            }
            ':' => patterns.push(std::mem::take(&mut current)),
            _ => current.push(character),
        }
    }
    patterns.push(current);
    patterns
}

/// A history size variable's explicit state.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum HistoryLimit {
    /// The variable was not set; a default may apply.
    Unset,
    /// The variable was set to an empty, non-numeric, or negative value.
    Unlimited,
    /// The variable was set to a non-negative integer.
    Limited(usize),
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
            self.open_file(&options, history_path.clone(), &self.default_exec_params())?;

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
        history.set_write_mode(self.history_write_mode);
        if let Some(limit) = self.history_file_size_limit()
            && let Err(error) = history.truncate_file(&history_path, limit)
        {
            // Reading succeeded; a read-only directory or unavailable lock must not discard
            // the in-memory history. The file is left untouched for a later writable session.
            tracing::warn!("could not trim history file at startup: {error}");
        }
        Ok(Some(history))
    }

    /// Returns the number of entries the in-memory history may hold, or `None` for no limit.
    /// `HISTSIZE` takes precedence; an inputrc `history-size` still caps a history that the
    /// environment leaves uncapped.
    fn history_size_limit(&self) -> Option<usize> {
        match self.env_history_limit("HISTSIZE") {
            HistoryLimit::Limited(limit) => Some(
                self.readline_options
                    .history_size
                    .map_or(limit, |readline| limit.min(readline)),
            ),
            HistoryLimit::Unlimited => self.readline_options.history_size,
            HistoryLimit::Unset => self.readline_options.history_size,
        }
    }

    /// Returns the number of entries the history file may hold, or `None` for no limit.
    fn history_file_size_limit(&self) -> Option<usize> {
        match self.env_history_limit("HISTFILESIZE") {
            HistoryLimit::Limited(limit) => Some(limit),
            HistoryLimit::Unlimited => None,
            // Bash defaults HISTFILESIZE to HISTSIZE after startup. Only an unset
            // HISTFILESIZE gets this default; explicit empty, non-numeric, or negative values
            // disable trimming.
            HistoryLimit::Unset => self.history_size_limit(),
        }
    }

    /// Parses a history size variable, preserving the difference between unset, explicitly
    /// unlimited, and a numeric limit.
    fn env_history_limit(&self, name: &str) -> HistoryLimit {
        let Some(value) = self.env_str(name) else {
            return HistoryLimit::Unset;
        };
        match value.trim().parse::<i64>() {
            Ok(value) if value >= 0 => {
                usize::try_from(value).map_or(HistoryLimit::Unlimited, HistoryLimit::Limited)
            }
            _ => HistoryLimit::Unlimited,
        }
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

        for pattern in split_ignore_patterns(&ignore) {
            if pattern.is_empty() {
                continue;
            }
            if pattern == "&" {
                if previous == Some(command) {
                    return Ok(true);
                }
                continue;
            }

            let pattern = patterns::Pattern::from(pattern.as_str())
                .set_extended_globbing(self.options.extended_globbing);
            if pattern.exactly_matches(command)? {
                return Ok(true);
            }
        }

        Ok(false)
    }

    /// Returns the configured history file write policy.
    pub const fn history_write_mode(&self) -> crate::history::HistoryWriteMode {
        self.history_write_mode
    }

    /// Sets the history file write policy, including for an already-loaded history.
    #[expect(
        clippy::missing_const_for_fn,
        reason = "updating the loaded history requires a mutable borrow, which is not const"
    )]
    pub fn set_history_write_mode(&mut self, mode: crate::history::HistoryWriteMode) {
        self.history_write_mode = mode;
        if let Some(history) = &mut self.history {
            history.set_write_mode(mode);
        }
    }

    /// Returns the path to the history file used by the shell, if one is set.
    pub fn history_file_path(&self) -> Option<PathBuf> {
        self.env_str("HISTFILE")
            .filter(|s| !s.is_empty())
            .map(|s| self.absolute_path(s.as_ref()))
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
    #[expect(
        clippy::cast_possible_truncation,
        reason = "history offsets are file lengths, which fit in usize on supported targets"
    )]
    #[allow(
        clippy::too_many_lines,
        reason = "a flat incremental-read state machine"
    )]
    pub fn read_history_file(
        &mut self,
        path: Option<&Path>,
        new_only: bool,
    ) -> Result<(), error::Error> {
        let Some(path) = path
            .map(|path| self.absolute_path(path))
            .or_else(|| self.history_file_path())
        else {
            return Ok(());
        };

        let mut file = std::fs::File::open(&path)?;
        let (metadata, file_id) = crate::sys::fs::history_file_metadata(&file)?;
        let length = metadata.len();

        let stored = if new_only {
            self.history_file_state.read_position.clone()
        } else {
            None
        };
        let candidate = stored.as_ref().and_then(|position| {
            (position.path == path && position.offset <= length && position.identity == file_id)
                .then_some(position.offset)
        });

        let read_from = candidate.map_or(0, |position| position.saturating_sub(256));
        let mut bytes = Vec::new();
        file.seek(SeekFrom::Start(read_from))?;
        file.read_to_end(&mut bytes)?;

        let mut offset = candidate.map_or(0, |position| position as usize);
        let mut prefix_len = offset - read_from as usize;
        if offset > 0
            && stored
                .as_ref()
                .is_some_and(|position| tail_checksum(&bytes[..prefix_len]) != position.checksum)
        {
            offset = 0;
            prefix_len = 0;
            file.seek(SeekFrom::Start(0))?;
            bytes.clear();
            file.read_to_end(&mut bytes)?;
        }

        let new_bytes = &bytes[prefix_len..];
        let complete_end = complete_record_end(new_bytes);
        if complete_end == 0 {
            self.history_file_state.read_position = Some(ReadPosition {
                path,
                offset: offset as u64,
                identity: file_id,
                checksum: tail_checksum(&bytes[..prefix_len]),
            });
            return Ok(());
        }

        let cursor_reset = new_only
            && stored.as_ref().is_some_and(|position| {
                position.path == path && position.offset > 0 && offset == 0
            });
        let size_limit = self.history_size_limit();
        let Some(history) = &mut self.history else {
            return Err(error::ErrorKind::HistoryNotEnabled.into());
        };
        let mut own_appends = std::mem::take(&mut self.history_file_state.own_appends);
        let filter_own = new_only;
        let imported: Vec<_> = if cursor_reset {
            let imported = crate::history::History::import(&bytes[..complete_end])?;
            let imported_lines: Vec<_> = imported
                .iter()
                .map(|item| item.command_line.as_str())
                .collect();
            let all_existing: Vec<_> = history
                .iter()
                .map(|item| item.command_line.as_str())
                .collect();
            let existing_lines: Vec<_> = all_existing
                .iter()
                .rev()
                .take(imported_lines.len())
                .copied()
                .collect::<Vec<_>>()
                .into_iter()
                .rev()
                .collect();
            let mut skip = 0;
            for count in 1..=existing_lines.len().min(imported_lines.len()) {
                if existing_lines[existing_lines.len() - count..] == imported_lines[..count] {
                    skip = count;
                }
            }
            imported.iter().skip(skip).cloned().collect()
        } else {
            crate::history::History::import(&new_bytes[..complete_end])?
                .iter()
                .cloned()
                .collect()
        };
        // A later `history -n` can read over records this session appended immediately.
        // They are already in memory; drop the matching imported copies.
        let mut keep = Vec::new();
        for item in imported {
            let own_index = filter_own
                .then(|| {
                    own_appends.iter().position(|own| {
                        own.path == path && own.item.command_line == item.command_line
                    })
                })
                .flatten();
            if let Some(index) = own_index {
                own_appends.remove(index);
            } else {
                keep.push(item);
            }
        }
        for item in keep {
            history.add(item)?;
        }
        if let Some(limit) = size_limit {
            history.retain_last_preserving_retry(limit);
        }
        self.history_file_state.own_appends = own_appends;
        self.history_file_state.read_position = Some(ReadPosition {
            path,
            offset: (offset + complete_end) as u64,
            identity: file_id,
            checksum: tail_checksum(&bytes[..prefix_len + complete_end]),
        });

        Ok(())
    }

    /// Remembers where a startup import stopped, so a first `history -n` does not import the
    /// entries the shell already loaded.
    pub(super) fn remember_history_file_position(&mut self) {
        let Some(path) = self.history_file_path() else {
            return;
        };
        let Ok(mut file) = std::fs::File::open(&path) else {
            return;
        };
        let Ok((_metadata, file_id)) = crate::sys::fs::history_file_metadata(&file) else {
            return;
        };
        let mut bytes = Vec::new();
        if file.read_to_end(&mut bytes).is_err() {
            return;
        }
        let complete_end = complete_record_end(&bytes);
        self.history_file_state.read_position = Some(ReadPosition {
            path,
            offset: complete_end as u64,
            identity: file_id,
            checksum: tail_checksum(&bytes[..complete_end]),
        });
    }

    /// Records that the complete history file was just written through `offset`.
    fn remember_history_file_end(&mut self) {
        let Some(path) = self.history_file_path() else {
            return;
        };
        let Ok(mut file) = std::fs::File::open(&path) else {
            return;
        };
        let Ok((metadata, file_id)) = crate::sys::fs::history_file_metadata(&file) else {
            return;
        };
        let offset = metadata.len();
        let start = offset.saturating_sub(256);
        let mut tail = Vec::new();
        if file.seek(SeekFrom::Start(start)).is_err()
            || file
                .by_ref()
                .take(offset - start)
                .read_to_end(&mut tail)
                .is_err()
            || tail.len() as u64 != offset - start
        {
            return;
        }
        let checksum = tail_checksum(&tail);
        self.history_file_state.read_position = Some(ReadPosition {
            path,
            offset,
            identity: file_id,
            checksum,
        });
    }

    /// Reads entries other sessions appended to the shell's history file since this session last
    /// read or saved it.
    pub fn read_new_history(&mut self) -> Result<(), error::Error> {
        // A session that has not saved yet has no file; that is not an error for share mode.
        let Some(path) = self.history_file_path() else {
            return Ok(());
        };
        if !path.exists() {
            return Ok(());
        }
        self.read_history_file(Some(&path), true)
    }

    /// Appends the session's unsaved entries to the history file, regardless of `histappend`.
    ///
    /// Immediate appends and share mode must never rewrite the file mid-session: another
    /// session may have written entries this shell has not seen. The final save decides
    /// whether the session appends or overwrites.
    pub fn append_history(&mut self) -> Result<(), error::Error> {
        let Some(history_file_path) = self.history_file_path() else {
            return Ok(());
        };
        let write_timestamps = self.env.is_set("HISTTIMEFORMAT");

        let _lock = if self.history_write_mode == crate::history::HistoryWriteMode::Portable {
            Some(match lock_history_file(history_file_path.as_path()) {
                Ok(lock) => lock,
                Err(error) => {
                    if let Some(history) = &mut self.history {
                        history.mark_dirty_retry();
                    }
                    return Err(error);
                }
            })
        } else {
            None
        };
        let appended: Vec<_> = self
            .history
            .as_ref()
            .map(|history| history.iter().filter(|item| item.dirty).cloned().collect())
            .unwrap_or_default();
        let size_limit = self.history_size_limit();
        if let Some(history) = &mut self.history {
            if let Err(error) =
                history.append_unsaved(history_file_path.as_path(), write_timestamps)
            {
                history.mark_dirty_retry();
                return Err(error);
            }
            if let Some(limit) = size_limit {
                history.retain_last(limit);
            }
        }
        self.history_file_state
            .own_appends
            .extend(appended.into_iter().map(|item| OwnAppend {
                path: history_file_path.clone(),
                item,
            }));

        Ok(())
    }

    /// Imports and appends in one transaction for an explicit share session.
    ///
    /// Pending local entries are held aside while foreign records are imported, so a
    /// HISTSIZE trim cannot discard a command that has not reached disk yet. The new cursor
    /// is recorded before releasing the lock.
    pub fn share_history(&mut self) -> Result<(), error::Error> {
        let Some(history_file_path) = self.history_file_path() else {
            return Ok(());
        };
        let write_timestamps = self.env.is_set("HISTTIMEFORMAT");

        let _lock = if self.history_write_mode == crate::history::HistoryWriteMode::Portable {
            Some(match lock_history_file(history_file_path.as_path()) {
                Ok(lock) => lock,
                Err(error) => {
                    if let Some(history) = &mut self.history {
                        history.mark_dirty_retry();
                    }
                    return Err(error);
                }
            })
        } else {
            None
        };
        let pending = self
            .history
            .as_mut()
            .map(crate::history::History::take_dirty)
            .unwrap_or_default();

        let read_result = if history_file_path.exists() {
            self.read_history_file(Some(history_file_path.as_path()), true)
        } else {
            Ok(())
        };

        if let Some(history) = &mut self.history {
            for item in pending {
                history.add(item)?;
            }
        }
        read_result?;

        let size_limit = self.history_size_limit();
        if let Some(history) = &mut self.history {
            if let Err(error) =
                history.append_unsaved(history_file_path.as_path(), write_timestamps)
            {
                history.mark_dirty_retry();
                return Err(error);
            }
            if let Some(limit) = size_limit {
                history.retain_last(limit);
            }
        }
        self.remember_history_file_end();

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
                history.save_with_limit(
                    history_file_path.as_path(),
                    append,
                    append, /* unsaved items only when appending */
                    write_timestamps,
                    file_limit,
                )?;
            }

            // Remember where this session's writes end, so share mode doesn't import them
            // again as if another shell had written them.
            self.remember_history_file_end();
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
            std::borrow::Cow::Owned(join_multiline_command(command, &self.parser_options()))
        } else {
            std::borrow::Cow::Borrowed(command)
        };
        let command = command.as_ref();

        if self.options.interactive && self.history_ignored(command)? {
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
            history.retain_last_preserving_retry(limit);
        }

        Ok(())
    }
}

#[cfg(test)]
#[allow(
    clippy::panic_in_result_fn,
    clippy::expect_used,
    reason = "assertions and fixture lookups in fallible tests"
)]
mod tests {
    use super::*;
    use crate::{history::History, variables::ShellVariable};
    use std::io::Write as _;

    #[tokio::test]
    async fn share_reads_new_entries_after_an_atomic_rewrite() -> Result<(), error::Error> {
        let dir = tempfile::tempdir()?;
        let path = dir.path().join("history");
        std::fs::write(&path, "old-entry\n")?;

        let loaded = History::import(std::fs::File::open(&path)?)?;
        let mut shell = crate::Shell::builder().build().await?;
        shell.set_env_global(
            "HISTFILE",
            ShellVariable::new(path.to_string_lossy().to_string()),
        )?;
        shell.history = Some(loaded);
        shell.remember_history_file_position();

        // Simulate another session's normal exit: the whole file is moved into a new inode
        // while retaining the old prefix and adding one record.
        let replacement = dir.path().join("replacement");
        std::fs::write(&replacement, "old-entry\nnew-entry\n")?;
        std::fs::rename(&replacement, &path)?;

        shell.read_new_history()?;

        let lines: Vec<_> = shell
            .history()
            .expect("history")
            .iter()
            .map(|item| item.command_line.as_str())
            .collect();
        assert_eq!(
            lines.iter().filter(|line| **line == "old-entry").count(),
            1,
            "{lines:?}"
        );
        assert_eq!(
            lines.iter().filter(|line| **line == "new-entry").count(),
            1,
            "{lines:?}"
        );
        Ok(())
    }

    #[tokio::test]
    async fn share_history_protects_pending_commands_from_histsize() -> Result<(), error::Error> {
        let dir = tempfile::tempdir()?;
        let path = dir.path().join("history");
        std::fs::write(&path, "external\n")?;

        let mut shell = crate::Shell::builder().build().await?;
        shell.set_env_global(
            "HISTFILE",
            ShellVariable::new(path.to_string_lossy().to_string()),
        )?;
        shell.set_env_global("HISTSIZE", ShellVariable::new("1"))?;
        shell.history = Some(History::default());
        shell
            .history_mut()
            .expect("history")
            .add(crate::history::Item::new("local-pending"))?;

        shell.share_history()?;

        let contents = std::fs::read_to_string(&path)?;
        assert!(contents.contains("external"), "{contents:?}");
        assert!(contents.contains("local-pending"), "{contents:?}");
        assert!(
            shell
                .history()
                .expect("history")
                .iter()
                .any(|item| item.command_line == "local-pending")
        );
        Ok(())
    }

    #[tokio::test]
    async fn append_history_does_not_import_other_sessions() -> Result<(), error::Error> {
        let dir = tempfile::tempdir()?;
        let path = dir.path().join("history");
        std::fs::write(&path, "external\n")?;

        let mut shell = crate::Shell::builder().build().await?;
        shell.set_env_global(
            "HISTFILE",
            ShellVariable::new(path.to_string_lossy().to_string()),
        )?;
        shell.history = Some(History::default());
        shell
            .history_mut()
            .expect("history")
            .add(crate::history::Item::new("local-only"))?;

        shell.append_history()?;

        let lines: Vec<_> = shell
            .history()
            .expect("history")
            .iter()
            .map(|item| item.command_line.as_str())
            .collect();
        assert_eq!(lines, ["local-only"]);
        let contents = std::fs::read_to_string(&path)?;
        assert!(contents.contains("external"), "{contents:?}");
        assert!(contents.contains("local-only"), "{contents:?}");
        Ok(())
    }

    #[tokio::test]
    async fn explicit_read_does_not_filter_own_command_from_another_file()
    -> Result<(), error::Error> {
        let dir = tempfile::tempdir()?;
        let path = dir.path().join("history");
        std::fs::write(&path, "")?;
        let other = dir.path().join("other-history");
        std::fs::write(&other, ": same\n")?;

        let mut shell = crate::Shell::builder().build().await?;
        shell.set_env_global(
            "HISTFILE",
            ShellVariable::new(path.to_string_lossy().to_string()),
        )?;
        shell.history = Some(History::default());
        shell
            .history_mut()
            .expect("history")
            .add(crate::history::Item::new(": same"))?;
        shell.append_history()?;

        shell.read_history_file(Some(&other), false)?;
        let count = shell
            .history()
            .expect("history")
            .iter()
            .filter(|item| item.command_line == ": same")
            .count();
        assert_eq!(count, 2, "explicit full read was filtered");
        Ok(())
    }

    #[tokio::test]
    async fn immediate_append_keeps_the_read_cursor_before_external_records()
    -> Result<(), error::Error> {
        let dir = tempfile::tempdir()?;
        let path = dir.path().join("history");
        std::fs::write(&path, "old-entry\n")?;

        let loaded = History::import(std::fs::File::open(&path)?)?;
        let mut shell = crate::Shell::builder().build().await?;
        shell.set_env_global(
            "HISTFILE",
            ShellVariable::new(path.to_string_lossy().to_string()),
        )?;
        shell.history = Some(loaded);
        shell.remember_history_file_position();

        shell
            .history_mut()
            .expect("history")
            .add(crate::history::Item::new("local-entry"))?;
        shell.append_history()?;

        std::fs::OpenOptions::new()
            .append(true)
            .open(&path)?
            .write_all(b"external-entry\n")?;
        shell.read_new_history()?;

        let lines: Vec<_> = shell
            .history()
            .expect("history")
            .iter()
            .map(|item| item.command_line.as_str())
            .collect();
        assert_eq!(
            lines
                .iter()
                .filter(|line| **line == "external-entry")
                .count(),
            1,
            "{lines:?}"
        );
        assert_eq!(
            lines.iter().filter(|line| **line == "local-entry").count(),
            1,
            "{lines:?}"
        );
        Ok(())
    }

    #[tokio::test]
    async fn lock_failure_marks_pending_entries_for_retry() -> Result<(), error::Error> {
        let dir = tempfile::tempdir()?;
        let path = dir.path().join("history");
        std::fs::write(&path, "")?;
        let lock_path = crate::history_file::lock_path(&path);
        std::fs::create_dir_all(&lock_path)?;

        let mut shell = crate::Shell::builder().build().await?;
        shell.set_env_global(
            "HISTFILE",
            ShellVariable::new(path.to_string_lossy().to_string()),
        )?;
        shell.set_env_global("HISTSIZE", ShellVariable::new("1"))?;
        shell.history = Some(History::default());
        shell
            .history_mut()
            .expect("history")
            .add(crate::history::Item::new("first-locked"))?;

        assert!(shell.append_history().is_err());
        let history = shell.history_mut().expect("history");
        history.add(crate::history::Item::new("second-locked"))?;
        history.retain_last_preserving_retry(1);
        assert!(
            history
                .iter()
                .any(|item| item.command_line == "first-locked"),
            "lock failure lost the pending command"
        );

        std::fs::remove_dir(&lock_path)?;
        shell.append_history()?;
        let contents = std::fs::read_to_string(&path)?;
        assert!(contents.contains("first-locked"), "{contents:?}");
        assert!(contents.contains("second-locked"), "{contents:?}");
        Ok(())
    }

    #[tokio::test]
    async fn startup_trim_failure_keeps_loaded_history() -> Result<(), error::Error> {
        let dir = tempfile::tempdir()?;
        let path = dir.path().join("history");
        std::fs::write(&path, ": one\n: two\n")?;
        std::fs::create_dir_all(crate::history_file::lock_path(&path))?;

        let mut shell = crate::Shell::builder().build().await?;
        shell.set_env_global(
            "HISTFILE",
            ShellVariable::new(path.to_string_lossy().to_string()),
        )?;
        shell.set_env_global("HISTSIZE", ShellVariable::new("1"))?;
        shell.set_env_global("HISTFILESIZE", ShellVariable::new("0"))?;

        let history = shell.load_history()?.expect("loaded history");
        assert_eq!(history.iter().count(), 1);
        assert_eq!(std::fs::read_to_string(&path)?, ": one\n: two\n");
        Ok(())
    }

    #[tokio::test]
    async fn failed_append_keeps_pending_entries_under_histsize() -> Result<(), error::Error> {
        let dir = tempfile::tempdir()?;
        let path = dir.path().join("history");
        std::fs::create_dir(&path)?;

        let mut shell = crate::Shell::builder().build().await?;
        shell.set_env_global(
            "HISTFILE",
            ShellVariable::new(path.to_string_lossy().to_string()),
        )?;
        shell.set_env_global("HISTSIZE", ShellVariable::new("1"))?;
        shell.history = Some(History::default());
        shell
            .history_mut()
            .expect("history")
            .add(crate::history::Item::new("delayed-by-write-error"))?;

        assert!(shell.append_history().is_err());

        let history = shell.history_mut().expect("history");
        history.add(crate::history::Item::new("second-command"))?;
        history.retain_last_preserving_retry(1);
        assert!(
            history
                .iter()
                .any(|item| item.command_line == "delayed-by-write-error"),
            "HISTSIZE discarded the pending command"
        );

        std::fs::remove_dir(&path)?;
        shell.append_history()?;
        let contents = std::fs::read_to_string(&path)?;
        assert!(contents.contains("delayed-by-write-error"), "{contents:?}");
        assert!(contents.contains("second-command"), "{contents:?}");
        Ok(())
    }

    #[test]
    fn complete_record_end_waits_for_a_timestamped_command() {
        assert_eq!(complete_record_end(b"#1700000000\n"), 0);
        assert_eq!(complete_record_end(b"#1700000000\ncmd\n"), 16);
        assert_eq!(complete_record_end(b"#1700000000\ncmd\n#1700000001\n"), 16);
        assert_eq!(complete_record_end(b"cmd\n"), 4);
        assert_eq!(complete_record_end(b"#1700000000\nonly-one\n#1\n"), 21);
        assert_eq!(complete_record_end(b"#1\n#2\nbody\n"), 11);
        assert_eq!(complete_record_end(b"#1\n#2\n"), 0);
        assert_eq!(complete_record_end(b"#1\nbody\n#2\n#3\n"), 8);
    }

    #[tokio::test]
    async fn share_rechecks_a_same_length_replacement() -> Result<(), error::Error> {
        let dir = tempfile::tempdir()?;
        let path = dir.path().join("history");
        let filler = "x".repeat(300);
        std::fs::write(&path, format!("OLD-ENTRY\n{filler}\n"))?;
        let old_len = std::fs::metadata(&path)?.len();

        let loaded = History::import(std::fs::File::open(&path)?)?;
        let mut shell = crate::Shell::builder().build().await?;
        shell.set_env_global(
            "HISTFILE",
            ShellVariable::new(path.to_string_lossy().to_string()),
        )?;
        shell.history = Some(loaded);
        shell.remember_history_file_position();

        let replacement = dir.path().join("replacement");
        std::fs::write(&replacement, format!("NEW-ENTRY\n{filler}\n"))?;
        assert_eq!(std::fs::metadata(&replacement)?.len(), old_len);
        std::fs::rename(&replacement, &path)?;

        shell.read_new_history()?;
        assert!(
            shell
                .history()
                .expect("history")
                .iter()
                .any(|item| item.command_line == "NEW-ENTRY"),
            "same-length replacement was skipped"
        );
        Ok(())
    }

    #[tokio::test]
    async fn share_does_not_duplicate_records_dropped_by_a_trim() -> Result<(), error::Error> {
        let dir = tempfile::tempdir()?;
        let path = dir.path().join("history");
        std::fs::write(&path, "one\ntwo\nthree\n")?;

        let loaded = History::import(std::fs::File::open(&path)?)?;
        let mut shell = crate::Shell::builder().build().await?;
        shell.set_env_global(
            "HISTFILE",
            ShellVariable::new(path.to_string_lossy().to_string()),
        )?;
        shell.history = Some(loaded);
        shell.remember_history_file_position();

        let replacement = dir.path().join("replacement");
        std::fs::write(&replacement, "two\nthree\nnew\n")?;
        std::fs::rename(&replacement, &path)?;

        shell.read_new_history()?;

        let lines: Vec<_> = shell
            .history()
            .expect("history")
            .iter()
            .map(|item| item.command_line.as_str())
            .collect();
        assert_eq!(
            lines.iter().filter(|line| **line == "two").count(),
            1,
            "{lines:?}"
        );
        assert_eq!(
            lines.iter().filter(|line| **line == "three").count(),
            1,
            "{lines:?}"
        );
        assert_eq!(
            lines.iter().filter(|line| **line == "new").count(),
            1,
            "{lines:?}"
        );
        Ok(())
    }

    #[tokio::test]
    async fn share_imports_a_rotated_file_once() -> Result<(), error::Error> {
        let dir = tempfile::tempdir()?;
        let path = dir.path().join("history");
        std::fs::write(&path, "old-one\nold-two\n")?;

        let loaded = History::import(std::fs::File::open(&path)?)?;
        let mut shell = crate::Shell::builder().build().await?;
        shell.set_env_global(
            "HISTFILE",
            ShellVariable::new(path.to_string_lossy().to_string()),
        )?;
        shell.history = Some(loaded);
        shell.remember_history_file_position();

        let replacement = dir.path().join("replacement");
        std::fs::write(&replacement, "rotated-one\nrotated-two\n")?;
        std::fs::rename(&replacement, &path)?;
        shell.read_new_history()?;
        shell.read_new_history()?;

        let lines: Vec<_> = shell
            .history()
            .expect("history")
            .iter()
            .map(|item| item.command_line.as_str())
            .collect();
        for expected in ["old-one", "old-two", "rotated-one", "rotated-two"] {
            assert_eq!(
                lines.iter().filter(|line| **line == expected).count(),
                1,
                "{expected} duplicated in {lines:?}"
            );
        }
        Ok(())
    }

    #[tokio::test]
    async fn share_reads_foreign_entries_before_advancing_the_cursor() -> Result<(), error::Error> {
        let dir = tempfile::tempdir()?;
        let path = dir.path().join("history");
        std::fs::write(&path, "foreign-before-append\n")?;

        let mut shell = crate::Shell::builder().build().await?;
        shell.set_env_global(
            "HISTFILE",
            ShellVariable::new(path.to_string_lossy().to_string()),
        )?;
        shell.history = Some(History::default());
        shell
            .history_mut()
            .expect("history")
            .add(crate::history::Item::new("mine"))?;

        shell.share_history()?;

        let lines: Vec<_> = shell
            .history()
            .expect("history")
            .iter()
            .map(|item| item.command_line.as_str())
            .collect();
        assert!(lines.contains(&"foreign-before-append"), "{lines:?}");
        assert!(lines.contains(&"mine"), "{lines:?}");

        std::fs::OpenOptions::new()
            .append(true)
            .open(&path)?
            .write_all(b"foreign-after-append\n")?;
        shell.read_new_history()?;
        assert!(
            shell
                .history()
                .expect("history")
                .iter()
                .any(|item| item.command_line == "foreign-after-append")
        );

        Ok(())
    }
}
