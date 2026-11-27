//! History management for shells.

use std::{
    collections::BTreeSet,
    io::{Read, Seek, SeekFrom},
    path::{Path, PathBuf},
};

use crate::{
    error,
    history::{History, HistoryControl, HistoryWriteMode},
    history_file::{
        OwnAppend, ReadPosition, complete_record_end, lock_history_file, tail_checksum,
        trim_history_bytes, write_startup_trim,
    },
    openfiles, patterns,
    sys::fs::{HistoryFileIdentity, history_file_metadata},
};

/// Joins the lines of a multi-line command the way bash's `cmdhist` does.
///
/// The tokenizer identifies continuations and the newline tokens eligible for joining. The
/// parser then checks whether each eligible newline can become `"; "` or a space. This keeps
/// separate commands in a block while allowing a space after `then`, `do`, or a trailing pipe.
fn join_multiline_command(command: &str, parser_options: &brush_parser::ParserOptions) -> String {
    if !command.contains('\n') {
        return command.to_owned();
    }

    let Ok(layout) = brush_parser::multiline_layout(command, parser_options) else {
        return command.to_owned();
    };
    if layout.has_here_document {
        // Keep here-document lines together as entered, including their terminal newline.
        return if command.ends_with('\n') {
            command.to_owned()
        } else {
            format!("{command}\n")
        };
    }
    if layout.line_breaks.is_empty() {
        return command.to_owned();
    }
    let mut lines = Vec::with_capacity(layout.line_breaks.len() + 1);
    let mut separators = Vec::with_capacity(layout.line_breaks.len());
    let mut remaining = command;
    let mut start = 0;
    for line_break in layout.line_breaks {
        let (offset, length, separator) = match line_break {
            brush_parser::SourceLineBreak::Newline(offset) => (offset, 1, "\n"),
            brush_parser::SourceLineBreak::Continuation(offset) => (offset - 1, 2, ""),
        };
        if offset + length == command.len() && separator == "\n" {
            break;
        }
        let (line, tail) = remaining.split_at(offset - start);
        lines.push(line);
        separators.push(separator);
        remaining = tail.split_at(length).1;
        start = offset + length;
    }
    lines.push(remaining);

    if let Some(replacements) = layout.newline_replacements {
        for (separator, (_, replacement)) in separators.iter_mut().zip(replacements) {
            *separator = replacement;
        }
        return render_history_lines(&lines, &separators);
    }

    let parse = |source: &str| {
        brush_parser::Parser::new(source.as_bytes(), parser_options)
            .parse_program()
            .map(|_| ())
    };
    for boundary in 0..separators.len() {
        if separators[boundary].is_empty() {
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

/// Renders source spans with the chosen history separators.
fn render_history_lines(lines: &[&str], separators: &[&str]) -> String {
    let mut rendered = String::new();
    for (index, line) in lines.iter().enumerate() {
        if index > 0 {
            rendered.push_str(separators[index - 1]);
        }
        rendered.push_str(line);
    }
    rendered
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

/// The startup bytes and the metadata of the handle that supplied them.
struct StartupHistorySnapshot {
    bytes: Vec<u8>,
    file: Option<(std::fs::Metadata, Option<HistoryFileIdentity>)>,
}

const MAX_FILE_SIZE_FOR_HISTORY_IMPORT: u64 = 1024 * 1024 * 1024; // 1 GiB

impl StartupHistorySnapshot {
    fn read(mut file: openfiles::OpenFile) -> Result<Option<Self>, error::Error> {
        let mut bytes = Vec::new();
        let file = if let openfiles::OpenFile::File(handle) = &file {
            let (metadata, identity) = history_file_metadata(handle)?;
            let file_size = metadata.len();
            // Also skip non-regular files that report size zero despite having contents.
            if file_size == 0 {
                return Ok(None);
            }
            if file_size > MAX_FILE_SIZE_FOR_HISTORY_IMPORT {
                return Err(error::ErrorKind::HistoryFileTooLargeToImport.into());
            }
            // Keep appends after this size observation for the next incremental read.
            file.take(file_size).read_to_end(&mut bytes)?;
            Some((metadata, identity))
        } else {
            file.read_to_end(&mut bytes)?;
            None
        };
        Ok(Some(Self { bytes, file }))
    }

    fn trim(
        &mut self,
        path: &Path,
        limit: usize,
        mode: HistoryWriteMode,
    ) -> Result<(), error::Error> {
        let Some((original, identity)) = &mut self.file else {
            return Ok(());
        };
        let trimmed = trim_history_bytes(&self.bytes, limit);
        if trimmed.len() == self.bytes.len() {
            return Ok(());
        }

        // The portable lock covers cooperating writers. Leave a changed path or file for
        // the next incremental read if a writer outside that lock has already modified it.
        let (current, current_identity) = history_file_metadata(&std::fs::File::open(path)?)?;
        if current_identity != *identity
            || current.len() != self.bytes.len() as u64
            || current.modified().ok() != original.modified().ok()
        {
            return Ok(());
        }
        *identity = write_startup_trim(path, &trimmed, mode)?;
        self.bytes = trimmed;
        Ok(())
    }
}

impl<SE: crate::extensions::ShellExtensions> crate::Shell<SE> {
    pub(super) fn load_startup_history(&mut self) -> Result<(), error::Error> {
        let Some(path) = self.history_file_path() else {
            return Ok(());
        };

        let mut file_limit = self.history_file_size_limit();
        // Take the writer lock before the snapshot if startup may trim the file. A session
        // that cannot lock the file can still import it and track future appends.
        let _lock = if file_limit.is_some() && self.history_write_mode == HistoryWriteMode::Portable
        {
            if path.metadata().is_ok_and(|metadata| {
                metadata.is_file()
                    && (1..=MAX_FILE_SIZE_FOR_HISTORY_IMPORT).contains(&metadata.len())
            }) {
                match lock_history_file(&path) {
                    Ok(lock) => Some(lock),
                    Err(error) => {
                        tracing::warn!("could not lock history file for startup trim: {error}");
                        file_limit = None;
                        None
                    }
                }
            } else {
                file_limit = None;
                None
            }
        } else {
            None
        };

        let file = self.open_file(
            std::fs::File::options().read(true),
            &path,
            &self.default_exec_params(),
        )?;
        if let Some(snapshot) = StartupHistorySnapshot::read(file)? {
            self.install_startup_history(path, snapshot, file_limit)?;
        }
        Ok(())
    }

    fn install_startup_history(
        &mut self,
        path: PathBuf,
        mut snapshot: StartupHistorySnapshot,
        file_limit: Option<usize>,
    ) -> Result<(), error::Error> {
        let original_len = snapshot.bytes.len();
        let complete_end = complete_record_end(&snapshot.bytes);
        let mut history = History::import(&snapshot.bytes[..complete_end])?;
        if let Some(limit) = self.history_size_limit() {
            history.retain_last(limit);
        }
        history.set_write_mode(self.history_write_mode);
        if let Some(limit) = file_limit
            && let Err(error) = snapshot.trim(&path, limit, self.history_write_mode)
        {
            tracing::warn!("could not trim history file at startup: {error}");
        }

        // Trimming retains a suffix. Translate the consumed boundary to that suffix, including
        // when the rewrite kept only an unfinished record or left the file empty.
        let end = complete_end.saturating_sub(original_len - snapshot.bytes.len());
        self.history_file_state.read_position = snapshot.file.map(|(_, identity)| ReadPosition {
            path,
            offset: end as u64,
            identity,
            checksum: tail_checksum(&snapshot.bytes[..end]),
        });
        self.history = Some(history);
        Ok(())
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
        reason = "only the checksum prefix length, at most 256 bytes, is converted to usize"
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
        let (metadata, file_id) = history_file_metadata(&file)?;
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

        let mut offset = candidate.unwrap_or(0);
        let mut prefix_len = (offset - read_from) as usize;
        if offset > 0
            && stored.as_ref().is_some_and(|position| {
                bytes
                    .get(..prefix_len)
                    .is_none_or(|prefix| tail_checksum(prefix) != position.checksum)
            })
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
                offset,
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
            offset: offset + complete_end as u64,
            identity: file_id,
            checksum: tail_checksum(&bytes[..prefix_len + complete_end]),
        });

        Ok(())
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
    use crate::{ProfileLoadBehavior, RcLoadBehavior, variables::ShellVariable};
    use std::io::Write as _;

    #[test]
    fn cmdhist_joins_ordinary_lines_around_a_quoted_newline() {
        let options = brush_parser::ParserOptions::default();
        let source = "if (( 1 << 2 )); then\n  : '<<\n' é#word\n  : next\nfi\n";
        let expected = "if (( 1 << 2 )); then   : '<<\n' é#word;   : next; fi\n";
        assert_eq!(join_multiline_command(source, &options), expected);
    }

    #[test]
    fn cmdhist_joins_each_non_comment_boundary() {
        let options = brush_parser::ParserOptions::default();
        let source = "if true; then\n: foo#bar\n: safe # comment\\\n: next\nfi";
        let expected = "if true; then : foo#bar; : safe # comment\\\n: next; fi";
        assert_eq!(join_multiline_command(source, &options), expected);
    }

    #[test]
    fn cmdhist_keeps_the_source_of_a_nested_here_document() {
        let options = brush_parser::ParserOptions::default();
        let source = ": \"$(cat <<-'END' # comment\n\tbody\n\tEND\n)\" <<<word\n: next";
        assert_eq!(
            join_multiline_command(source, &options),
            format!("{source}\n"),
        );
    }

    async fn startup_shell(path: &Path) -> Result<crate::Shell, error::Error> {
        crate::Shell::builder()
            .interactive(true)
            .do_not_inherit_env(true)
            .skip_well_known_vars(true)
            .profile(ProfileLoadBehavior::Skip)
            .rc(RcLoadBehavior::Skip)
            .var(
                "HISTFILE",
                ShellVariable::new(path.to_string_lossy().into_owned()),
            )
            .build()
            .await
    }

    fn history_lines(shell: &crate::Shell) -> Vec<&str> {
        shell
            .history()
            .expect("history")
            .iter()
            .map(|item| item.command_line.as_str())
            .collect()
    }

    #[tokio::test]
    async fn startup_cursor_tracks_appends_on_both_sides_of_the_snapshot()
    -> Result<(), error::Error> {
        let dir = tempfile::tempdir()?;
        let path = dir.path().join("history");
        std::fs::write(&path, b"original\n")?;
        let mut shell = startup_shell(&path).await?;

        let opened = std::fs::File::open(&path)?;
        let mut writer = std::fs::OpenOptions::new().append(true).open(&path)?;
        writer.write_all(b"before-snapshot\n")?;
        let snapshot = StartupHistorySnapshot::read(opened.into())?.expect("snapshot");
        writer.write_all(b"after-snapshot\n")?;
        // The startup trim must also leave this uncoordinated append in place.
        shell.install_startup_history(path, snapshot, Some(1))?;
        assert_eq!(history_lines(&shell), ["original", "before-snapshot"]);
        writer.write_all(b"after-import\n")?;

        shell.read_new_history()?;
        shell.read_new_history()?;
        assert_eq!(
            history_lines(&shell),
            [
                "original",
                "before-snapshot",
                "after-snapshot",
                "after-import"
            ]
        );
        Ok(())
    }

    #[tokio::test]
    async fn startup_uses_the_identity_of_the_handle_it_reads() -> Result<(), error::Error> {
        let dir = tempfile::tempdir()?;
        let path = dir.path().join("history");
        std::fs::write(&path, b"old-one\nold-two\n")?;
        let mut shell = startup_shell(&path).await?;

        let opened = std::fs::File::open(&path)?;
        let (_, identity) = history_file_metadata(&opened)?;
        let replacement = dir.path().join("replacement");
        std::fs::write(&replacement, b"rotated-one\nrotated-two\n")?;
        std::fs::rename(&replacement, &path)?;
        let snapshot = StartupHistorySnapshot::read(opened.into())?.expect("snapshot");
        shell.install_startup_history(path.clone(), snapshot, Some(1))?;
        let cursor = shell
            .history_file_state
            .read_position
            .as_ref()
            .expect("cursor");
        assert_eq!(cursor.identity, identity);
        assert_eq!(cursor.offset, 16);
        assert_eq!(history_lines(&shell), ["old-one", "old-two"]);
        assert_eq!(std::fs::read(&path)?, b"rotated-one\nrotated-two\n");

        shell.read_new_history()?;
        shell.read_new_history()?;
        assert_eq!(
            history_lines(&shell),
            ["old-one", "old-two", "rotated-one", "rotated-two"]
        );
        Ok(())
    }

    #[tokio::test]
    async fn startup_cursor_recovers_after_in_place_truncation() -> Result<(), error::Error> {
        let dir = tempfile::tempdir()?;
        let path = dir.path().join("history");
        std::fs::write(&path, b"long-before-truncation\n")?;
        let mut shell = startup_shell(&path).await?;
        let snapshot =
            StartupHistorySnapshot::read(std::fs::File::open(&path)?.into())?.expect("snapshot");
        std::fs::write(&path, b"short\n")?;
        shell.install_startup_history(path.clone(), snapshot, None)?;

        shell.read_new_history()?;
        std::fs::write(&path, b"regrown-past-that-cursor\n")?;
        shell.read_new_history()?;
        shell.read_new_history()?;
        assert_eq!(
            history_lines(&shell),
            [
                "long-before-truncation",
                "short",
                "regrown-past-that-cursor"
            ]
        );
        Ok(())
    }

    #[tokio::test]
    async fn startup_trim_tracks_the_written_suffix_and_leaves_partial_records_pending()
    -> Result<(), error::Error> {
        for mode in [HistoryWriteMode::Portable, HistoryWriteMode::Bash] {
            let dir = tempfile::tempdir()?;
            let path = dir.path().join("history");
            let retained = b"#1700000002\nkept\n";
            let partial = b"#1700000003\nincomplete";
            std::fs::write(
                &path,
                [&b"#1700000001\nremoved\n"[..], retained, partial].concat(),
            )?;
            let mut shell = startup_shell(&path).await?;
            shell.set_env_global("HISTFILESIZE", ShellVariable::new("2"))?;
            shell.set_history_write_mode(mode);
            shell
                .load_config(&ProfileLoadBehavior::Skip, &RcLoadBehavior::Skip)
                .await?;

            assert_eq!(history_lines(&shell), ["removed", "kept"]);
            assert_eq!(std::fs::read(&path)?, [&retained[..], partial].concat());
            let cursor = shell
                .history_file_state
                .read_position
                .as_ref()
                .expect("cursor");
            let (_, identity) = history_file_metadata(&std::fs::File::open(&path)?)?;
            assert_eq!(cursor.identity, identity);
            assert_eq!(cursor.offset, retained.len() as u64);
            shell.read_new_history()?;
            assert_eq!(history_lines(&shell), ["removed", "kept"]);

            std::fs::OpenOptions::new()
                .append(true)
                .open(&path)?
                .write_all(b"-done\n")?;
            shell.read_new_history()?;
            shell.read_new_history()?;
            assert_eq!(
                history_lines(&shell),
                ["removed", "kept", "incomplete-done"]
            );
        }
        Ok(())
    }

    #[tokio::test]
    async fn startup_consumes_invalid_complete_lines_but_leaves_partial_lines_pending()
    -> Result<(), error::Error> {
        let dir = tempfile::tempdir()?;
        let path = dir.path().join("history");
        let complete = b"invalid-\xff\nvalid\n";
        std::fs::write(&path, [&complete[..], b"partial"].concat())?;
        let mut shell = startup_shell(&path).await?;
        shell.load_startup_history()?;
        assert_eq!(history_lines(&shell), ["valid"]);
        let cursor = shell
            .history_file_state
            .read_position
            .as_ref()
            .expect("cursor");
        assert_eq!(cursor.offset, complete.len() as u64);
        assert_eq!(cursor.checksum, tail_checksum(complete));

        std::fs::OpenOptions::new()
            .append(true)
            .open(&path)?
            .write_all(b"-done\n")?;
        shell.read_new_history()?;
        assert_eq!(history_lines(&shell), ["valid", "partial-done"]);
        Ok(())
    }

    #[tokio::test]
    async fn startup_zero_file_limit_still_reads_the_first_append() -> Result<(), error::Error> {
        let dir = tempfile::tempdir()?;
        let path = dir.path().join("history");
        std::fs::write(&path, b"before-trim\n")?;
        let mut shell = startup_shell(&path).await?;
        shell.set_env_global("HISTFILESIZE", ShellVariable::new("0"))?;
        shell.load_startup_history()?;

        assert_eq!(history_lines(&shell), ["before-trim"]);
        assert_eq!(std::fs::metadata(&path)?.len(), 0);
        assert_eq!(
            shell
                .history_file_state
                .read_position
                .as_ref()
                .expect("cursor")
                .offset,
            0
        );

        std::fs::OpenOptions::new()
            .append(true)
            .open(&path)?
            .write_all(b"after-trim\n")?;
        shell.read_new_history()?;
        assert_eq!(history_lines(&shell), ["before-trim", "after-trim"]);
        Ok(())
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn startup_rejects_files_above_the_import_limit() -> Result<(), error::Error> {
        let dir = tempfile::tempdir()?;
        let path = dir.path().join("history");
        std::fs::File::create(&path)?.set_len(MAX_FILE_SIZE_FOR_HISTORY_IMPORT + 1)?;
        let mut shell = startup_shell(&path).await?;
        assert!(matches!(
            shell.load_startup_history(),
            Err(error) if matches!(error.kind(), error::ErrorKind::HistoryFileTooLargeToImport)
        ));
        assert!(shell.history_file_state.read_position.is_none());
        Ok(())
    }

    #[tokio::test]
    async fn share_reads_new_entries_after_an_atomic_rewrite() -> Result<(), error::Error> {
        let dir = tempfile::tempdir()?;
        let path = dir.path().join("history");
        std::fs::write(&path, "old-entry\n")?;

        let mut shell = startup_shell(&path).await?;
        shell.load_startup_history()?;

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

        let mut shell = startup_shell(&path).await?;
        shell.load_startup_history()?;

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

        let mut shell = startup_shell(&path).await?;
        shell.set_env_global("HISTSIZE", ShellVariable::new("1"))?;
        shell.set_env_global("HISTFILESIZE", ShellVariable::new("0"))?;

        shell.load_startup_history()?;
        assert_eq!(history_lines(&shell), [": two"]);
        assert_eq!(std::fs::read_to_string(&path)?, ": one\n: two\n");
        let cursor = shell
            .history_file_state
            .read_position
            .as_ref()
            .expect("cursor");
        assert_eq!(cursor.offset, 12);

        std::fs::OpenOptions::new()
            .append(true)
            .open(&path)?
            .write_all(b": three\n")?;
        shell.read_new_history()?;
        assert_eq!(history_lines(&shell), [": three"]);
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

        let mut shell = startup_shell(&path).await?;
        shell.load_startup_history()?;

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

        let mut shell = startup_shell(&path).await?;
        shell.load_startup_history()?;

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

        let mut shell = startup_shell(&path).await?;
        shell.load_startup_history()?;

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
