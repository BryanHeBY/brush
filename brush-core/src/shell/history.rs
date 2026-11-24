//! History management for shells.

use std::{
    collections::BTreeSet,
    io::{Read, Seek, SeekFrom},
    path::{Path, PathBuf},
};

use crate::{error, history::HistoryControl, openfiles, patterns};

/// Joins the lines of a multi-line command the way bash's `cmdhist` does.
///
/// Newlines inside quotes and here-document bodies are part of the command and are kept as
/// written. Elsewhere, a line that ends a simple command is followed by `"; "`, a line ending
/// at a keyword or a continuation by a space, and a backslash-newline is removed. Leading
/// whitespace on the next line is kept.
fn join_multiline_command(command: &str) -> String {
    if command.contains("<<") && !command.contains("<<<") {
        // Bash keeps a trailing newline as part of a here-document history entry.
        return format!("{command}\n");
    }
    if has_quoted_newline(command) || has_unquoted_hash(command) {
        return command.to_owned();
    }

    let mut joined = String::new();
    let mut continued = false;

    for (index, line) in command.lines().enumerate() {
        if index > 0 && !continued {
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

            if keyword || previous.ends_with(';') {
                joined.push(' ');
            } else {
                joined.push_str("; ");
            }
        }

        if let Some(without_backslash) = line.strip_suffix('\\') {
            joined.push_str(without_backslash);
            continued = true;
        } else {
            joined.push_str(line);
            continued = false;
        }
    }

    joined
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
fn tail_checksum(bytes: &[u8]) -> u64 {
    let start = bytes.len().saturating_sub(256);
    let mut hash = 0xcbf2_9ce4_8422_2325_u64;
    for byte in &bytes[start..] {
        hash ^= u64::from(*byte);
        hash = hash.wrapping_mul(0x100_0000_01b3);
    }
    hash
}

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

/// Returns the end of the last complete history record in `bytes`.
///
/// A newline normally completes a record, but in a timestamped file a timestamp without a
/// following command is only half a record. Leave that timestamp for the next read rather than
/// committing the cursor past it.
fn complete_record_end(bytes: &[u8]) -> usize {
    let Some(last_newline) = bytes.iter().rposition(|byte| *byte == b'\n') else {
        return 0;
    };
    let end = last_newline + 1;
    let lines: Vec<&[u8]> = bytes[..last_newline].split(|byte| *byte == b'\n').collect();
    let is_timestamp = |line: &[u8]| {
        line.strip_prefix(b"#")
            .and_then(|rest| std::str::from_utf8(rest).ok())
            .is_some_and(|rest| rest.trim().parse::<i64>().is_ok())
    };
    if !lines.iter().any(|line| is_timestamp(line)) {
        return end;
    }

    let mut offset = 0;
    let mut timestamp_start = None;
    let mut has_command = false;
    for line in lines {
        if is_timestamp(line) {
            if timestamp_start.is_some() && !has_command {
                return timestamp_start.unwrap_or(offset);
            }
            timestamp_start = Some(offset);
            has_command = false;
        } else if !line.is_empty() || timestamp_start.is_some() {
            has_command = true;
        }
        offset += line.len() + 1;
    }

    if timestamp_start.is_some() && !has_command {
        timestamp_start.unwrap_or(end)
    } else {
        end
    }
}

/// Returns the identity used to detect a replaced history file.
fn history_file_identity(metadata: &std::fs::Metadata) -> super::HistoryFileId {
    #[cfg(unix)]
    {
        use std::os::unix::fs::MetadataExt as _;
        (metadata.dev(), metadata.ino())
    }
    #[cfg(not(unix))]
    {
        let _ = metadata;
        ()
    }
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
    #[expect(
        clippy::cast_possible_truncation,
        reason = "history offsets are file lengths, which fit in usize on supported targets"
    )]
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
        let metadata = file.metadata()?;
        let file_id = Some(history_file_identity(&metadata));
        let mut bytes = Vec::new();
        file.read_to_end(&mut bytes)?;

        let complete_end = complete_record_end(&bytes);
        let checksum_at = |offset: usize| tail_checksum(&bytes[..offset]);

        let stored = if new_only {
            self.history_read_position.clone()
        } else {
            None
        };
        let offset = if new_only {
            match &stored {
                Some((position_path, position, _stored_id, stored_checksum))
                    if position_path == &path
                        && (*position as usize) <= complete_end
                        && checksum_at(*position as usize) == *stored_checksum =>
                {
                    *position as usize
                }
                _ => 0,
            }
        } else {
            0
        };
        // Only import complete lines. A writer may be halfway through the last record; leave
        // the position before it so the next read picks up the completed line.
        if complete_end <= offset {
            self.history_read_position = Some((path, offset as u64, file_id, checksum_at(offset)));
            return Ok(());
        }

        let cursor_reset = new_only
            && stored
                .as_ref()
                .is_some_and(|(stored_path, position, _, _)| {
                    stored_path == &path && *position > 0 && offset == 0
                });
        let size_limit = self.history_size_limit();
        let Some(history) = &mut self.history else {
            return Err(error::ErrorKind::HistoryNotEnabled.into());
        };
        if cursor_reset {
            // A rewrite (normal exit, HISTFILESIZE trim, or rotation) can replace the inode,
            // so the old byte offset no longer applies. Skip leading records already at the
            // tail of memory before importing the rest.
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
            for item in imported.iter().skip(skip).cloned() {
                history.add(item)?;
            }
        } else {
            history.append_from(&bytes[offset..complete_end])?;
        }
        if let Some(limit) = size_limit {
            history.retain_last(limit);
        }
        self.history_read_position = Some((
            path,
            complete_end as u64,
            file_id,
            checksum_at(complete_end),
        ));

        Ok(())
    }

    /// Remembers where a startup import stopped, so a first `history -n` does not import the
    /// entries the shell already loaded.
    pub(super) fn remember_history_file_position(&mut self) {
        let Some(path) = self.history_file_path() else {
            return;
        };
        let Ok(metadata) = std::fs::metadata(&path) else {
            return;
        };
        let file_id = Some(history_file_identity(&metadata));
        let Ok(bytes) = std::fs::read(&path) else {
            return;
        };
        let complete_end = complete_record_end(&bytes);
        self.history_read_position = Some((
            path,
            complete_end as u64,
            file_id,
            tail_checksum(&bytes[..complete_end]),
        ));
    }

    /// Records that the complete history file was just written through `offset`.
    fn remember_history_file_end(&mut self) {
        let Some(path) = self.history_file_path() else {
            return;
        };
        let Ok(metadata) = std::fs::metadata(&path) else {
            return;
        };
        let file_id = Some(history_file_identity(&metadata));
        let offset = metadata.len();
        let checksum = (|| {
            let mut file = std::fs::File::open(&path)?;
            file.seek(SeekFrom::Start(offset.saturating_sub(256)))?;
            let mut tail = Vec::new();
            file.read_to_end(&mut tail)?;
            Ok::<_, std::io::Error>(tail_checksum(&tail))
        })()
        .unwrap_or_default();
        self.history_read_position = Some((path, offset, file_id, checksum));
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

        // Read, append, and advance the cursor under one lock. Otherwise another session can
        // append after our read and before our write, and moving the cursor to the end would
        // make the next read skip that record.
        let _lock = crate::history::lock_history_file(history_file_path.as_path())?;
        if history_file_path.exists() {
            self.read_history_file(Some(history_file_path.as_path()), true)?;
        }
        if let Some(history) = &mut self.history {
            history.append_unsaved(history_file_path.as_path(), write_timestamps)?;
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

    #[test]
    fn complete_record_end_waits_for_a_timestamped_command() {
        assert_eq!(complete_record_end(b"#1700000000\n"), 0);
        assert_eq!(complete_record_end(b"#1700000000\ncmd\n"), 16);
        assert_eq!(complete_record_end(b"#1700000000\ncmd\n#1700000001\n"), 16);
        assert_eq!(complete_record_end(b"cmd\n"), 4);
        assert_eq!(complete_record_end(b"#1700000000\nonly-one\n#1\n"), 21);
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
    async fn append_reads_foreign_entries_before_advancing_the_cursor() -> Result<(), error::Error>
    {
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

        shell.append_history()?;

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
