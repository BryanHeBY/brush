//! Facilities for tracking and persisting the shell's command history.

use std::{
    collections::BTreeSet,
    io::{BufRead, Read, Write},
    path::Path,
};

use crate::error;

/// Represents a unique identifier for a history item.
type ItemId = i64;

/// Interface for querying and manipulating the shell's recorded history of commands.
// TODO(history): support maximum item count
#[derive(Clone, Default)]
#[cfg_attr(feature = "serde", derive(serde::Serialize, serde::Deserialize))]
pub struct History {
    /// The items, oldest first. Their IDs only increase, so they're in ID order too, which
    /// finds an item by ID (by binary search) without a separate map from IDs to items.
    items: rpds::VectorSync<Item>,
    next_id: ItemId,
    /// Index of the oldest unsaved item, if any. Kept so appending a command's item costs the
    /// unsaved tail rather than a scan of the whole history.
    #[cfg_attr(feature = "serde", serde(default))]
    first_dirty: Option<usize>,
    /// Number of oldest entries forgotten, so displayed history numbers keep counting up as
    /// they do in bash after `HISTSIZE` drops an entry.
    #[cfg_attr(feature = "serde", serde(default))]
    base: usize,
    /// IDs whose append failed and must survive HISTSIZE until they can be retried.
    #[cfg_attr(feature = "serde", serde(skip))]
    retry: BTreeSet<ItemId>,
}

impl History {
    /// Constructs a new `History` instance, with its contents initialized from the given readable
    /// stream. If errors are encountered reading lines from the stream, unreadable lines will
    /// be skipped but the call will still return successfully, with a warning logged. An error
    /// result will be returned only if an internal error occurs updating the history. Like
    /// bash, an entry the stream gives no timestamp gets the time it's read; so does one whose
    /// timestamp is past the year 9999.
    ///
    /// # Arguments
    ///
    /// * `reader` - The readable stream to import history from.
    pub fn import(reader: impl Read) -> Result<Self, error::Error> {
        let mut history = Self::default();
        let read_time = ItemTimestamp::now();

        let buf_reader = std::io::BufReader::new(reader);
        let mut lines = Vec::new();
        for line_result in buf_reader.lines() {
            match line_result {
                Ok(line) => lines.push(line),
                // If we couldn't decode the line due to invalid data (perhaps it wasn't
                // valid UTF8?), skip it and make a best-effort attempt to proceed on.
                // We'll later warn the user.
                Err(err) if err.kind() == std::io::ErrorKind::InvalidData => {
                    tracing::warn!("unreadable history line; {err}");
                }
                // In the event of other kinds of errors, return an error result. We don't
                // want to get stuck in a failing I/O loop.
                Err(err) => return Err(err.into()),
            }
        }

        let is_timestamp = |line: &str| {
            line.strip_prefix('#')
                .is_some_and(|rest| rest.trim().parse::<i64>().is_ok())
        };
        let timed = lines.iter().any(|line| is_timestamp(line));

        if timed {
            // A timestamped entry runs until the next timestamp; this is how a `lithist`
            // command with embedded newlines is stored.
            let mut current: Option<(Option<ItemTimestamp>, Vec<String>)> = None;
            for line in lines {
                if is_timestamp(&line) {
                    if let Some((timestamp, mut command_lines)) = current.take() {
                        add_imported(&mut history, timestamp, &mut command_lines, read_time)?;
                    }
                    let seconds = line
                        .strip_prefix('#')
                        .and_then(|rest| rest.trim().parse::<i64>().ok())
                        .unwrap_or_default();
                    current = Some((ItemTimestamp::from_unix_seconds(seconds), Vec::new()));
                } else if let Some((_timestamp, command_lines)) = &mut current {
                    command_lines.push(line);
                } else {
                    current = Some((None, vec![line]));
                }
            }
            if let Some((timestamp, mut command_lines)) = current {
                add_imported(&mut history, timestamp, &mut command_lines, read_time)?;
            }
        } else {
            let mut next_timestamp = None;
            for line in lines {
                if is_timestamp(&line) {
                    // An untimed file's `#<digits>` line is still a timestamp for the next
                    // line, exactly as bash's reader treats it.
                    let seconds = line
                        .strip_prefix('#')
                        .and_then(|rest| rest.trim().parse::<i64>().ok())
                        .unwrap_or_default();
                    next_timestamp = ItemTimestamp::from_unix_seconds(seconds);
                    continue;
                }
                history.add(Item {
                    id: history.next_id,
                    command_line: line,
                    timestamp: Some(next_timestamp.take().unwrap_or(read_time)),
                    dirty: false,
                })?;
            }
        }

        Ok(history)
    }
    /// Appends the entries from `reader` to this history, preserving their timestamps. Imported
    /// entries are already on disk, so they are not marked for saving again.
    pub fn append_from(&mut self, reader: impl Read) -> Result<(), error::Error> {
        let imported = Self::import(reader)?;
        for item in imported.items.iter().cloned() {
            self.add(item)?;
        }
        Ok(())
    }

    /// Tries to retrieve a history item by its unique identifier. Returns `None` if no item is
    /// found.
    ///
    /// # Arguments
    ///
    /// * `id` - The unique identifier of the history item to retrieve.
    pub fn get_by_id(&self, id: ItemId) -> Result<Option<&Item>, error::Error> {
        Ok(self.position_of(id).and_then(|index| self.items.get(index)))
    }

    /// Replaces the history item with the given ID with a new item. Returns an error if the item
    /// cannot be updated.
    ///
    /// # Arguments
    ///
    /// * `id` - The unique identifier of the history item to update.
    /// * `item` - The new history item to replace the old one.
    pub fn update_by_id(&mut self, id: ItemId, item: Item) -> Result<(), error::Error> {
        let index = self
            .position_of(id)
            .ok_or(error::ErrorKind::HistoryItemNotFound)?;
        // The item keeps its ID, so the items stay in ID order.
        self.items.set_mut(index, Item { id, ..item });
        self.first_dirty = self.items.iter().position(|item| item.dirty);
        Ok(())
    }

    /// Removes the nth item from the history (counting from 0, the oldest). Returns whether
    /// there was such an item to remove.
    pub fn remove_nth_item(&mut self, n: usize) -> bool {
        let len = self.items.len();
        if n >= len {
            return false;
        }

        // Take off the items from the nth on, and put back those after it: so removing a
        // recent item (as `fc -s` does its own) touches only the few after it.
        let after: Vec<Item> = (n + 1..len)
            .filter_map(|i| self.items.get(i).cloned())
            .collect();
        for _ in n..len {
            self.items.drop_last_mut();
        }
        for item in after {
            self.items.push_back_mut(item);
        }
        self.first_dirty = self.items.iter().position(|item| item.dirty);

        true
    }

    /// Keeps only the items that `keep` returns true for, in order. Their IDs don't change.
    pub fn retain(&mut self, mut keep: impl FnMut(&Item) -> bool) {
        let Some(first_removed) = self.items.iter().position(|item| !keep(item)) else {
            return;
        };

        // As in `remove_nth_item`, rebuild only from the first item removed on.
        let kept: Vec<Item> = self
            .items
            .iter()
            .skip(first_removed + 1)
            .filter(|item| keep(item))
            .cloned()
            .collect();
        for _ in first_removed..self.items.len() {
            self.items.drop_last_mut();
        }
        for item in kept {
            self.items.push_back_mut(item);
        }
        self.first_dirty = self.items.iter().position(|item| item.dirty);
    }

    /// Removes and returns all dirty entries. Share mode holds these aside while importing
    /// foreign records so a HISTSIZE trim cannot discard commands that still need writing.
    pub(crate) fn take_dirty(&mut self) -> Vec<Item> {
        let dirty: Vec<Item> = self
            .items
            .iter()
            .filter(|item| item.dirty)
            .cloned()
            .collect();
        if !dirty.is_empty() {
            self.retain(|item| !item.dirty);
        }
        dirty
    }

    /// Trims the oldest entries toward `limit`, preserving entries whose append failed and
    /// still need a retry.
    ///
    /// Ordinary unsaved entries remain subject to HISTSIZE; only failed writes are protected.
    pub(crate) fn retain_last_preserving_retry(&mut self, limit: usize) {
        if self.retry.is_empty() {
            self.retain_last(limit);
            return;
        }
        let excess = self.count().saturating_sub(limit);
        if excess == 0 {
            return;
        }
        let retry = self.retry.clone();
        let newest = self.items.last().map(|item| item.id);
        let mut remaining = excess;
        self.retain(|item| {
            let protected = retry.contains(&item.id) || Some(item.id) == newest;
            if remaining > 0 && !protected {
                remaining -= 1;
                false
            } else {
                true
            }
        });
        self.base += excess - remaining;
    }

    /// Marks all currently dirty entries as needing a retry after a failed append.
    pub(crate) fn mark_dirty_retry(&mut self) {
        for item in self.items.iter().filter(|item| item.dirty) {
            self.retry.insert(item.id);
        }
    }

    /// Discards the oldest entries until at most `limit` remain.
    pub fn retain_last(&mut self, limit: usize) {
        let removed = self.count().saturating_sub(limit);
        let mut remove = removed;
        if remove != 0 {
            self.retain(|_| {
                let keep = remove == 0;
                remove = remove.saturating_sub(1);
                keep
            });
            self.base += removed;
        }
    }

    /// Adds a new history item. Returns the unique identifier of the newly added item.
    ///
    /// # Arguments
    ///
    /// * `item` - The history item to add.
    pub fn add(&mut self, mut item: Item) -> Result<ItemId, error::Error> {
        let id = self.next_id;

        item.id = id;
        self.next_id += 1;

        let dirty = item.dirty;
        self.items.push_back_mut(item);
        if dirty {
            let index = self.items.len() - 1;
            self.first_dirty = Some(self.first_dirty.map_or(index, |first| first.min(index)));
        }

        Ok(id)
    }

    /// Deletes a history item by its unique identifier. Returns an error if the item cannot be
    /// deleted.
    ///
    /// # Arguments
    ///
    /// * `id` - The unique identifier of the history item to delete.
    pub fn delete_item_by_id(&mut self, id: ItemId) -> Result<(), error::Error> {
        if let Some(index) = self.position_of(id) {
            self.remove_nth_item(index);
        }

        Ok(())
    }

    /// Clears all history items.
    pub fn clear(&mut self) -> Result<(), error::Error> {
        self.items = rpds::VectorSync::new_sync();
        self.first_dirty = None;
        self.base = 0;
        Ok(())
    }

    /// Returns the number of items forgotten from the front of the history. Displayed numbers
    /// are this plus the item's position plus one.
    pub const fn base(&self) -> usize {
        self.base
    }

    /// Returns the first dirty index, repairing it if an older serialized state omitted it.
    fn ensure_dirty_index(&mut self) -> Option<usize> {
        if self.first_dirty.is_none() {
            self.first_dirty = self.items.iter().position(|item| item.dirty);
        }
        self.first_dirty
    }

    /// Appends unsaved items to `path` without taking the history-file lock. The caller must
    /// hold it, so the append and any accompanying reads form one transaction.
    pub(crate) fn append_unsaved(
        &mut self,
        history_file_path: impl AsRef<Path>,
        write_timestamps: bool,
    ) -> Result<(), error::Error> {
        let Some(start) = self.ensure_dirty_index() else {
            return Ok(());
        };
        let mut file = std::fs::OpenOptions::new()
            .create(true)
            .append(true)
            .open(history_file_path.as_ref())?;
        let mut written = Vec::new();
        for (index, item) in self.items.iter().enumerate().skip(start) {
            if !item.dirty {
                continue;
            }
            if write_timestamps && let Some(timestamp) = item.timestamp {
                writeln!(file, "#{}", timestamp.as_unix_seconds())?;
            }
            writeln!(file, "{}", item.command_line)?;
            written.push(index);
        }
        file.flush()?;
        for index in written {
            if let Some(item) = self.items.get_mut(index) {
                item.dirty = false;
            }
        }
        self.first_dirty = None;
        Ok(())
    }

    /// Flushes the history to backing storage (if relevant).
    ///
    /// # Arguments
    ///
    /// * `history_file_path` - The path to the history file.
    /// * `append` - Whether to append to the file or overwrite it.
    /// * `unsaved_items_only` - Whether to only write unsaved items; if true, any items will be
    ///   marked as "saved" once saved.
    /// * `write_timestamps` - Whether to write timestamps for each command line.
    pub fn flush(
        &mut self,
        history_file_path: impl AsRef<Path>,
        append: bool,
        unsaved_items_only: bool,
        write_timestamps: bool,
    ) -> Result<(), error::Error> {
        let _lock = lock_history_file(history_file_path.as_ref())?;

        // Open the file
        let mut file_options = std::fs::File::options();

        if append {
            file_options.append(true);
        } else {
            file_options.write(true).truncate(true);
        }

        let mut file =
            std::io::BufWriter::new(file_options.create(true).open(history_file_path.as_ref())?);

        let first_dirty = unsaved_items_only
            .then(|| self.ensure_dirty_index())
            .flatten();
        // Nothing to append? Don't walk a long history just to find no unsaved items.
        if unsaved_items_only && first_dirty.is_none() {
            return Ok(());
        }

        let start = if unsaved_items_only {
            first_dirty.unwrap_or(0)
        } else {
            0
        };

        let mut written = Vec::new();
        for (index, item) in self.items.iter().enumerate().skip(start) {
            if unsaved_items_only && !item.dirty {
                continue;
            }

            if write_timestamps && let Some(timestamp) = item.timestamp {
                writeln!(file, "#{}", timestamp.as_unix_seconds())?;
            }

            writeln!(file, "{}", item.command_line)?;

            if unsaved_items_only {
                written.push(index);
            }
        }

        // Only once they're all written out, mark the items saved: if writing fails, they
        // stay unsaved, so a later flush retries them.
        file.into_inner()
            .map_err(std::io::IntoInnerError::into_error)?;
        for index in written {
            if let Some(item) = self.items.get_mut(index) {
                item.dirty = false;
            }
        }
        if unsaved_items_only {
            self.first_dirty = None;
        }

        Ok(())
    }

    /// Saves history, applying `HISTFILESIZE`, under the history file's cross-process lock.
    ///
    /// The file is rebuilt in a temporary file and atomically renamed, so a concurrent
    /// appender cannot slip between reading the old entries and replacing them, and a failed
    /// write leaves the previous file intact. Items are marked saved only after the rename.
    pub fn save_with_limit(
        &mut self,
        history_file_path: impl AsRef<Path>,
        append: bool,
        unsaved_items_only: bool,
        write_timestamps: bool,
        max_items: Option<usize>,
    ) -> Result<(), error::Error> {
        use std::fmt::Write as _;

        let path = history_file_path.as_ref();
        let _lock = lock_history_file(path)?;

        let first_dirty = unsaved_items_only
            .then(|| self.ensure_dirty_index())
            .flatten();

        // Appending is the common exit path. Write in place so an unreadable or non-UTF-8
        // history file is preserved byte-for-byte instead of being replaced by an empty
        // rewrite. It also keeps symlinks and permissions intact.
        if append && unsaved_items_only && max_items.is_none() {
            self.append_unsaved(path, write_timestamps)?;
            return Ok(());
        }

        let existing = if append {
            match std::fs::read_to_string(path) {
                Ok(existing) => existing,
                Err(error) if error.kind() == std::io::ErrorKind::NotFound => String::new(),
                Err(error) => return Err(error.into()),
            }
        } else {
            String::new()
        };
        let start = if unsaved_items_only {
            first_dirty.unwrap_or(0)
        } else {
            0
        };

        let mut content = existing;
        let mut written = Vec::new();
        for (index, item) in self.items.iter().enumerate().skip(start) {
            if unsaved_items_only && !item.dirty {
                continue;
            }
            if write_timestamps && let Some(timestamp) = item.timestamp {
                writeln!(content, "#{}", timestamp.as_unix_seconds())?;
            }
            writeln!(content, "{}", item.command_line)?;
            if unsaved_items_only {
                written.push(index);
            }
        }

        if let Some(max_items) = max_items {
            content = trim_history_entries(&content, max_items);
        }

        atomic_write(path, content.as_bytes())?;

        for index in written {
            if let Some(item) = self.items.get_mut(index) {
                item.dirty = false;
            }
        }
        if unsaved_items_only {
            self.first_dirty = None;
        }

        Ok(())
    }

    /// Trims the backing file to its last `max_items` lines.
    ///
    /// `HISTFILESIZE` counts entries, but a history file can also contain timestamp lines; a
    /// later format pass will group them. Until then this keeps the last lines, which matches
    /// bash for files without timestamps and never drops the newest entries.
    pub fn truncate_file(
        &self,
        history_file_path: impl AsRef<Path>,
        max_items: usize,
    ) -> Result<(), error::Error> {
        let path = history_file_path.as_ref();
        let _lock = lock_history_file(path)?;
        let Ok(contents) = std::fs::read_to_string(path) else {
            return Ok(());
        };
        let lines: Vec<&str> = contents.lines().collect();

        // With timestamps, a `#<digits>` line starts an entry and the lines after it belong to
        // that entry (a multi-line command); without them, each line is an entry, as bash reads
        // an untimed file. Keep the newest entries, not merely the newest lines.
        let timed = lines.iter().any(|line| {
            line.strip_prefix('#')
                .is_some_and(|rest| rest.trim().parse::<i64>().is_ok())
        });
        let mut entries: Vec<Vec<&str>> = Vec::new();
        if timed {
            for line in lines {
                let is_timestamp = line
                    .strip_prefix('#')
                    .is_some_and(|rest| rest.trim().parse::<i64>().is_ok());
                if is_timestamp || entries.is_empty() {
                    entries.push(vec![line]);
                } else if let Some(entry) = entries.last_mut() {
                    entry.push(line);
                }
            }
        } else {
            entries.extend(lines.into_iter().map(|line| vec![line]));
        }

        if entries.len() <= max_items {
            return Ok(());
        }

        let mut trimmed = entries[entries.len() - max_items..]
            .iter()
            .flatten()
            .copied()
            .collect::<Vec<_>>()
            .join("\n");
        if !trimmed.is_empty() {
            trimmed.push('\n');
        }
        std::fs::write(path, trimmed)?;
        Ok(())
    }

    /// Appends entries from `reader`, skipping ones already present (matched by timestamp and
    /// command line). Share mode uses this to merge a file it may have partially seen.
    pub fn append_from_unique(&mut self, reader: impl Read) -> Result<(), error::Error> {
        let imported = Self::import(reader)?;
        for item in imported.items.iter().cloned() {
            let present = self
                .items
                .iter()
                .any(|existing| existing.command_line == item.command_line);
            if !present {
                self.add(item)?;
            }
        }
        Ok(())
    }

    /// Searches through history using the given query.
    ///
    /// # Arguments
    ///
    /// * `query` - The query to use.
    pub fn search(&self, query: Query) -> Result<impl Iterator<Item = &self::Item>, error::Error> {
        Ok(Search::new(self, query))
    }

    /// Returns whether the history contains no items.
    pub fn is_empty(&self) -> bool {
        self.items.is_empty()
    }

    /// Returns an iterator over the history items.
    pub fn iter(&self) -> impl Iterator<Item = &self::Item> {
        self.items.iter()
    }

    /// Retrieves the nth history item, if it exists. Returns `None` if no such item exists.
    /// Indexing is zero-based, with an index of 0 referencing the oldest item in the history.
    ///
    /// # Arguments
    ///
    /// * `index` - The index of the history item to retrieve.
    pub fn get(&self, index: usize) -> Option<&Item> {
        self.items.get(index)
    }

    /// Returns the newest item, if there is one.
    pub fn last(&self) -> Option<&Item> {
        self.items.last()
    }

    /// Returns the number of items in the history.
    pub fn count(&self) -> usize {
        self.items.len()
    }

    /// Returns how many items have IDs less than `id`: the position of the item with that ID,
    /// if there is one.
    fn position_before(&self, id: ItemId) -> usize {
        // The items are in ID order, so binary search. (By hand: `slice::partition_point`
        // needs a slice, which rpds's vector, a tree, isn't.)
        let (mut low, mut high) = (0, self.items.len());
        while low < high {
            let mid = low + (high - low) / 2;
            if self.items.get(mid).is_some_and(|item| item.id < id) {
                low = mid + 1;
            } else {
                high = mid;
            }
        }
        low
    }

    /// Returns the position of the item with the given ID, if there is one.
    fn position_of(&self, id: ItemId) -> Option<usize> {
        let index = self.position_before(id);
        self.items
            .get(index)
            .is_some_and(|item| item.id == id)
            .then_some(index)
    }
}

/// A value of `HISTCONTROL`, which controls what an interactive shell saves to history.
#[derive(Clone, Copy, Debug, Eq, Hash, Ord, PartialEq, PartialOrd, strum_macros::EnumString)]
#[strum(serialize_all = "lowercase")]
pub(crate) enum HistoryControl {
    /// Don't save a command that starts with a space.
    IgnoreSpace,
    /// Don't save a command that's the same as the last one saved.
    IgnoreDups,
    /// Both `ignorespace` and `ignoredups`.
    IgnoreBoth,
    /// Before saving a command, remove any earlier ones the same as it.
    EraseDups,
}

impl HistoryControl {
    /// Parses `HISTCONTROL`'s colon-separated values into the set of them, with `ignoreboth`
    /// as the two it stands for. Like bash, values it doesn't know are ignored.
    pub(crate) fn parse_list(value: &str) -> BTreeSet<Self> {
        value
            .split(':')
            .filter_map(|value| value.parse().ok())
            .flat_map(|control| match control {
                Self::IgnoreBoth => [Self::IgnoreSpace, Self::IgnoreDups].as_slice(),
                Self::IgnoreSpace => &[Self::IgnoreSpace],
                Self::IgnoreDups => &[Self::IgnoreDups],
                Self::EraseDups => &[Self::EraseDups],
            })
            .copied()
            .collect()
    }
}

/// Represents a timestamp for a history item: an instant in time, independent of time zone.
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Hash)]
#[cfg_attr(feature = "serde", derive(serde::Serialize, serde::Deserialize))]
#[cfg_attr(feature = "serde", serde(transparent))]
pub struct ItemTimestamp(jiff::Timestamp);

impl ItemTimestamp {
    /// Returns the current time.
    pub fn now() -> Self {
        Self(jiff::Timestamp::now())
    }

    /// Converts from a [`std::time::SystemTime`], keeping its sub-second precision. Returns
    /// `None` outside the years -9999 to 9999.
    ///
    /// # Arguments
    ///
    /// * `time` - The time to convert.
    pub fn from_system_time(time: std::time::SystemTime) -> Option<Self> {
        jiff::Timestamp::try_from(time).ok().map(Self)
    }

    /// Converts to a [`std::time::SystemTime`], keeping its sub-second precision. Returns
    /// `None` if the platform's `SystemTime` can't represent it (some can't represent times
    /// before 1970).
    pub fn to_system_time(self) -> Option<std::time::SystemTime> {
        let since_epoch = self.0.as_duration();
        if since_epoch.is_negative() {
            std::time::UNIX_EPOCH.checked_sub(since_epoch.unsigned_abs())
        } else {
            std::time::UNIX_EPOCH.checked_add(since_epoch.unsigned_abs())
        }
    }

    /// Formats the timestamp in local time (the system time zone), using a `strftime`-style
    /// format string, as the C library does in the POSIX locale. A directive that can't be
    /// rendered is left as written.
    ///
    /// # Arguments
    ///
    /// * `format` - The `strftime`-style format string.
    pub fn format_local(self, format: &str) -> String {
        crate::timing::strftime(&crate::timing::to_local(self.0), format)
    }

    /// Converts from whole seconds since the Unix epoch (as history files record them),
    /// returning `None` outside the years -9999 to 9999.
    pub(crate) fn from_unix_seconds(seconds: i64) -> Option<Self> {
        jiff::Timestamp::from_second(seconds).ok().map(Self)
    }

    /// Returns the whole seconds since the Unix epoch, rounded down (so a time before the
    /// epoch with a fraction of a second goes to the earlier second).
    pub(crate) fn as_unix_seconds(self) -> i64 {
        let seconds = self.0.as_second();
        if self.0.subsec_nanosecond() < 0 {
            seconds - 1
        } else {
            seconds
        }
    }
}

/// Represents an item in the history.
#[derive(Clone, Default)]
#[cfg_attr(feature = "serde", derive(serde::Serialize, serde::Deserialize))]
pub struct Item {
    /// The unique identifier of the history item.
    pub id: ItemId,
    /// The actual command line.
    pub command_line: String,
    /// The timestamp when the command was started, if known (a history file needn't record
    /// one).
    pub timestamp: Option<ItemTimestamp>,
    /// Whether or not the item is dirty, i.e., has not yet been written to backing storage.
    pub dirty: bool,
}

impl Item {
    /// Constructs a new `Item` with the given command line.
    ///
    /// # Arguments
    ///
    /// * `command_line` - The command line of the item.
    pub fn new(command_line: impl Into<String>) -> Self {
        Self {
            id: 0, // NOTE: ID will be assigned when added to the history.
            command_line: command_line.into(),
            timestamp: Some(ItemTimestamp::now()),
            dirty: true,
        }
    }
}

/// Encapsulates query parameters for searching through history.
#[derive(Default)]
pub struct Query {
    /// Whether to search forward or backward
    pub direction: Direction,
    /// Optionally, clamp results to items with a timestamp strictly after this. Items with no
    /// timestamp aren't excluded by it.
    pub not_at_or_before_time: Option<ItemTimestamp>,
    /// Optionally, clamp results to items with a timestamp strictly before this. Items with no
    /// timestamp aren't excluded by it.
    pub not_at_or_after_time: Option<ItemTimestamp>,
    /// Optionally, clamp results to items with an ID equal strictly after this.
    pub not_at_or_before_id: Option<ItemId>,
    /// Optionally, clamp results to items with an ID equal strictly before this.
    pub not_at_or_after_id: Option<ItemId>,
    /// Optionally, maximum number of items to retrieve
    pub max_items: Option<i64>,
    /// Optionally, a string-based filter on command line.
    pub command_line_filter: Option<CommandLineFilter>,
}

impl Query {
    /// Checks if the query includes the given item.
    ///
    /// # Arguments
    ///
    /// * `item` - The item to check.
    pub fn includes(&self, item: &Item) -> bool {
        // Filter based on not_at_or_before_time.
        if let Some(not_at_or_before_time) = &self.not_at_or_before_time {
            if item
                .timestamp
                .is_some_and(|ts| ts <= *not_at_or_before_time)
            {
                return false;
            }
        }

        // Filter based on not_at_or_after_time
        if let Some(not_at_or_after_time) = &self.not_at_or_after_time {
            if item.timestamp.is_some_and(|ts| ts >= *not_at_or_after_time) {
                return false;
            }
        }

        // Filter based on not_at_or_before_id
        if self
            .not_at_or_before_id
            .is_some_and(|query_id| item.id <= query_id)
        {
            return false;
        }

        // Filter based on not_at_or_after_id
        if self
            .not_at_or_after_id
            .is_some_and(|query_id| item.id >= query_id)
        {
            return false;
        }

        // Filter based on command_line_filter
        if let Some(command_line_filter) = &self.command_line_filter {
            match command_line_filter {
                CommandLineFilter::Prefix(prefix) => {
                    if !item.command_line.starts_with(prefix) {
                        return false;
                    }
                }
                CommandLineFilter::Suffix(suffix) => {
                    if !item.command_line.ends_with(suffix) {
                        return false;
                    }
                }
                CommandLineFilter::Contains(contains) => {
                    if !item.command_line.contains(contains) {
                        return false;
                    }
                }
                CommandLineFilter::Exact(exact) => {
                    if item.command_line != *exact {
                        return false;
                    }
                }
            }
        }

        true
    }
}

/// Represents the direction of a search operation.
#[derive(Default)]
pub enum Direction {
    /// Search forward from the oldest part of history.
    #[default]
    Forward,
    /// Search backward from the youngest part of history.
    Backward,
}

/// Filter criteria for command lines.
pub enum CommandLineFilter {
    /// The command line must start with this string.
    Prefix(String),
    /// The command line must end with this string.
    Suffix(String),
    /// The command line must contain this string.
    Contains(String),
    /// The command line must match this string exactly.
    Exact(String),
}

/// Represents a search operation.
pub struct Search<'a> {
    /// The history to search through.
    history: &'a History,
    /// The positions of the items left to search: those within the query's ID bounds.
    positions: std::ops::Range<usize>,
    /// The query to apply.
    query: Query,
    /// Count of items returned so far.
    count: usize,
}

impl<'a> Search<'a> {
    /// Constructs a new search against the provided history, querying *all* items.
    ///
    /// # Arguments
    ///
    /// * `history` - The history to search through.
    pub fn all(history: &'a History) -> Self {
        Self::new(history, Query::default())
    }

    /// Constructs a new search against the provided history, using the given query.
    ///
    /// # Arguments
    ///
    /// * `history` - The history to search through.
    /// * `query` - The query to use.
    pub fn new(history: &'a History, query: Query) -> Self {
        // The items within the query's ID bounds are a run of them, since they're in ID
        // order: search just those positions.
        let start = query.not_at_or_before_id.map_or(0, |id| {
            id.checked_add(1)
                .map_or(history.items.len(), |id| history.position_before(id))
        });
        let end = query
            .not_at_or_after_id
            .map_or(history.items.len(), |id| history.position_before(id));

        Self {
            history,
            positions: start..end.max(start),
            query,
            count: 0,
        }
    }
}

impl<'a> Iterator for Search<'a> {
    type Item = &'a Item;

    fn next(&mut self) -> Option<Self::Item> {
        // Once we hit the limit, we stop searching.
        #[expect(clippy::cast_possible_truncation)]
        #[expect(clippy::cast_sign_loss)]
        if self
            .query
            .max_items
            .is_some_and(|max_items| self.count >= max_items as usize)
        {
            return None;
        }

        loop {
            let index = match self.query.direction {
                Direction::Forward => self.positions.next(),
                Direction::Backward => self.positions.next_back(),
            }?;
            let item = self.history.items.get(index)?;
            if self.query.includes(item) {
                self.count += 1;
                return Some(item);
            }
        }
    }
}

/// Adds one imported entry, joining any embedded command lines.
fn add_imported(
    history: &mut History,
    timestamp: Option<ItemTimestamp>,
    command_lines: &mut Vec<String>,
    read_time: ItemTimestamp,
) -> Result<(), error::Error> {
    if command_lines.is_empty() {
        return Ok(());
    }
    let command_line = command_lines.join("\n");
    command_lines.clear();
    history.add(Item {
        id: history.next_id,
        command_line,
        timestamp: Some(timestamp.unwrap_or(read_time)),
        dirty: false,
    })?;
    Ok(())
}

/// Keeps the last `max_items` entries in a history file's contents.
///
/// With timestamps, a `#<digits>` line starts an entry and the lines after it belong to that
/// entry (a multi-line command); without them, each line is an entry, as bash reads an untimed
/// file.
fn trim_history_entries(contents: &str, max_items: usize) -> String {
    let lines: Vec<&str> = contents.lines().collect();
    let timed = lines.iter().any(|line| {
        line.strip_prefix('#')
            .is_some_and(|rest| rest.trim().parse::<i64>().is_ok())
    });
    let mut entries: Vec<Vec<&str>> = Vec::new();
    if timed {
        for line in lines {
            let is_timestamp = line
                .strip_prefix('#')
                .is_some_and(|rest| rest.trim().parse::<i64>().is_ok());
            if is_timestamp || entries.is_empty() {
                entries.push(vec![line]);
            } else if let Some(entry) = entries.last_mut() {
                entry.push(line);
            }
        }
    } else {
        entries.extend(lines.into_iter().map(|line| vec![line]));
    }

    if entries.len() <= max_items {
        return contents.to_owned();
    }
    if max_items == 0 {
        return String::new();
    }

    let mut trimmed = entries[entries.len() - max_items..]
        .iter()
        .flatten()
        .copied()
        .collect::<Vec<_>>()
        .join("\n");
    trimmed.push('\n');
    trimmed
}

/// Locks a per-history-file lock used to serialize writers to `path`.
///
/// The lock lives in the temporary directory rather than next to the history file so it does
/// not become another file in the user's home (or in tests that compare history directories).
pub(crate) fn lock_history_file(
    path: &Path,
) -> Result<crate::sys::fs::HistoryFileLock, error::Error> {
    let lock_path = history_lock_path(path);
    let file = std::fs::OpenOptions::new()
        .create(true)
        .truncate(false)
        .write(true)
        .open(lock_path)?;
    Ok(crate::sys::fs::lock_history_file(file)?)
}

/// Returns the lock file path for a history path, resolving symlinks so every spelling of the
/// same file uses the same lock.
pub(crate) fn history_lock_path(path: &Path) -> std::path::PathBuf {
    use std::hash::{Hash as _, Hasher as _};

    let lock_identity = path.canonicalize().unwrap_or_else(|_| {
        let absolute = if path.is_absolute() {
            path.to_path_buf()
        } else {
            std::env::current_dir().map_or_else(|_| path.to_path_buf(), |cwd| cwd.join(path))
        };
        let Some(file_name) = absolute.file_name() else {
            return absolute;
        };
        let parent = absolute
            .parent()
            .and_then(|parent| parent.canonicalize().ok())
            .unwrap_or_else(|| {
                absolute
                    .parent()
                    .map_or_else(|| Path::new("").to_path_buf(), Path::to_path_buf)
            });
        parent.join(file_name)
    });
    let mut hasher = std::collections::hash_map::DefaultHasher::new();
    lock_identity.hash(&mut hasher);
    #[cfg(unix)]
    let lock_directory = Path::new("/tmp");
    #[cfg(not(unix))]
    let lock_directory = std::env::temp_dir();
    lock_directory.join(format!("brush-history-{:016x}.lock", hasher.finish()))
}

/// Writes `contents` to a temporary file next to the target, then renames it into place.
///
/// A symlinked history file is resolved and its target is replaced, leaving the link in
/// place. The target's existing permissions are copied to the replacement, so a private
/// history file does not become world-readable after a rewrite.
fn atomic_write(path: &Path, contents: &[u8]) -> Result<(), error::Error> {
    let target = match std::fs::symlink_metadata(path) {
        Ok(metadata) if metadata.file_type().is_symlink() => std::fs::canonicalize(path)?,
        _ => path.to_path_buf(),
    };
    let permissions = std::fs::metadata(&target)
        .ok()
        .map(|metadata| metadata.permissions());

    let file_name = target.file_name().ok_or_else(|| {
        std::io::Error::new(std::io::ErrorKind::InvalidInput, "invalid history file")
    })?;

    let mut created = None;
    for attempt in 0..100_u32 {
        let mut temp_name = std::ffi::OsString::from(".");
        temp_name.push(file_name);
        temp_name.push(format!(".tmp.{}.{attempt}", std::process::id()));
        let candidate = target.with_file_name(temp_name);
        match crate::sys::fs::create_new_private_file(&candidate) {
            Ok(file) => {
                created = Some((file, candidate));
                break;
            }
            Err(error) if error.kind() == std::io::ErrorKind::AlreadyExists => {}
            Err(error) => return Err(error.into()),
        }
    }
    let Some((mut file, temp_path)) = created else {
        return Err(std::io::Error::new(
            std::io::ErrorKind::AlreadyExists,
            "could not create a private history temporary file",
        )
        .into());
    };

    let result = (|| -> Result<(), error::Error> {
        file.write_all(contents)?;
        if let Some(permissions) = permissions {
            std::fs::set_permissions(&temp_path, permissions)?;
        }
        file.sync_all()?;
        drop(file);
        std::fs::rename(&temp_path, &target)?;
        Ok(())
    })();
    if result.is_err() {
        // Only this operation's exclusively-created temporary is removed.
        let _ = std::fs::remove_file(&temp_path);
    }
    result
}

#[cfg(test)]
#[allow(clippy::panic_in_result_fn, reason = "assertions in a fallible test")]
mod tests {
    use super::*;

    fn history(lines: &[&str]) -> Result<History, error::Error> {
        let mut history = History::default();
        for line in lines {
            history.add(Item::new(*line))?;
        }
        Ok(history)
    }

    fn command_lines<'a>(items: impl Iterator<Item = &'a Item>) -> Vec<&'a str> {
        items.map(|item| item.command_line.as_str()).collect()
    }

    /// Removing an item shifts the positions of those after it (as `history -d` shows), but
    /// no item's ID changes: an ID names its item for good.
    #[test]
    fn ids_are_stable_across_removal() -> Result<(), error::Error> {
        let mut history = history(&["a", "b", "c", "d"])?;

        assert!(history.remove_nth_item(1));
        assert_eq!(
            history.get(1).map(|item| item.command_line.as_str()),
            Some("c")
        );
        assert_eq!(
            history.get_by_id(2)?.map(|item| item.command_line.as_str()),
            Some("c")
        );

        history.delete_item_by_id(0)?;
        assert_eq!(command_lines(history.iter()), ["c", "d"]);
        assert!(history.get_by_id(0)?.is_none());
        assert!(history.get_by_id(1)?.is_none());
        assert_eq!(history.get_by_id(3)?.map(|item| item.id), Some(3));

        // New items still get new IDs.
        assert_eq!(history.add(Item::new("e"))?, 4);
        assert_eq!(
            history.get_by_id(4)?.map(|item| item.command_line.as_str()),
            Some("e")
        );

        // Even after clearing history: an old ID never comes to name a new item.
        history.clear()?;
        assert_eq!(history.add(Item::new("f"))?, 5);
        assert!(history.get_by_id(0)?.is_none());

        Ok(())
    }

    /// Updating an item keeps its ID, whatever ID the new item carries.
    #[test]
    fn update_by_id_keeps_the_id() -> Result<(), error::Error> {
        let mut history = history(&["a", "b"])?;

        history.update_by_id(
            1,
            Item {
                id: 99,
                ..Item::new("x")
            },
        )?;
        let updated = history
            .get_by_id(1)?
            .map(|item| (item.id, item.command_line.as_str()));
        assert_eq!(updated, Some((1, "x")));
        assert!(history.get_by_id(99)?.is_none());

        assert!(history.update_by_id(7, Item::new("y")).is_err());

        Ok(())
    }

    #[test]
    fn search_by_direction_bounds_and_prefix() -> Result<(), error::Error> {
        let history = history(&["echo hi there", "ls", "echo hello world", "cd /tmp"])?;
        let search = |query| history.search(query).map(command_lines);
        let prefix = |text: &str| Some(CommandLineFilter::Prefix(text.to_owned()));

        // The newest match, as a hint asks for...
        let newest = Query {
            direction: Direction::Backward,
            max_items: Some(1),
            command_line_filter: prefix("echo h"),
            ..Query::default()
        };
        assert_eq!(search(newest)?, ["echo hello world"]);

        // ...and older than a given ID, as history navigation asks for.
        let older = Query {
            direction: Direction::Backward,
            not_at_or_after_id: Some(2),
            command_line_filter: prefix("echo h"),
            ..Query::default()
        };
        assert_eq!(search(older)?, ["echo hi there"]);

        let newer = Query {
            not_at_or_before_id: Some(0),
            not_at_or_after_id: Some(3),
            ..Query::default()
        };
        assert_eq!(search(newer)?, ["ls", "echo hello world"]);

        // Newer than a given ID, with no upper bound, as moving down through history asks.
        let down = Query {
            not_at_or_before_id: Some(1),
            ..Query::default()
        };
        assert_eq!(search(down)?, ["echo hello world", "cd /tmp"]);

        // By substring (as Ctrl-R searches) or exact match, or with no room for any.
        let substring = Query {
            direction: Direction::Backward,
            command_line_filter: Some(CommandLineFilter::Contains("hel".to_owned())),
            ..Query::default()
        };
        assert_eq!(search(substring)?, ["echo hello world"]);
        let exact = Query {
            command_line_filter: Some(CommandLineFilter::Exact("ls".to_owned())),
            ..Query::default()
        };
        assert_eq!(search(exact)?, ["ls"]);
        let no_room = Query {
            max_items: Some(0),
            ..Query::default()
        };
        assert_eq!(search(no_room)?, Vec::<&str>::new());

        let none = Query {
            direction: Direction::Backward,
            command_line_filter: prefix("echo hix"),
            ..Query::default()
        };
        assert_eq!(search(none)?, Vec::<&str>::new());

        Ok(())
    }

    /// A search finds just what checking each item would, in order, whatever its direction,
    /// ID bounds, limit, and filter -- including bounds at deleted IDs and at the extremes.
    /// And an item is found by its ID just as a scan would find it.
    #[test]
    fn search_and_get_by_id_match_a_scan() -> Result<(), error::Error> {
        for len in 0..6_i64 {
            for deleted in 0..(1 << len) {
                let mut history = History::default();
                for id in 0..len {
                    history.add(Item::new(if id % 2 == 0 { "even" } else { "odd" }))?;
                }
                for id in (0..len).filter(|id| deleted & (1 << id) != 0) {
                    history.delete_item_by_id(id)?;
                }
                let scan = || (0..history.count()).filter_map(|index| history.get(index));

                let bounds = [None, Some(ItemId::MIN), Some(-1), Some(0), Some(2)]
                    .into_iter()
                    .chain([Some(len - 1), Some(len), Some(ItemId::MAX)]);
                for lower in bounds.clone() {
                    for upper in bounds.clone() {
                        for backward in [false, true] {
                            for max_items in [None, Some(0), Some(1), Some(2), Some(-1)] {
                                for filter in [None, Some("odd")] {
                                    let query = || Query {
                                        direction: if backward {
                                            Direction::Backward
                                        } else {
                                            Direction::Forward
                                        },
                                        not_at_or_before_id: lower,
                                        not_at_or_after_id: upper,
                                        max_items,
                                        command_line_filter: filter
                                            .map(|f| CommandLineFilter::Prefix(f.to_owned())),
                                        ..Query::default()
                                    };

                                    let found: Vec<ItemId> =
                                        history.search(query())?.map(|item| item.id).collect();
                                    let mut expected: Vec<ItemId> = scan()
                                        .filter(|item| query().includes(item))
                                        .map(|item| item.id)
                                        .collect();
                                    if backward {
                                        expected.reverse();
                                    }
                                    // A negative limit is no limit.
                                    if let Some(max) =
                                        max_items.and_then(|m| usize::try_from(m).ok())
                                    {
                                        expected.truncate(max);
                                    }
                                    assert_eq!(
                                        found, expected,
                                        "len {len}, deleted {deleted:b}, bounds {lower:?}..{upper:?}, backward {backward}, max {max_items:?}, filter {filter:?}"
                                    );
                                }
                            }
                        }
                    }
                }

                for id in -1..=len {
                    let by_scan = scan().find(|item| item.id == id).map(|item| item.id);
                    assert_eq!(history.get_by_id(id)?.map(|item| item.id), by_scan);
                }
            }
        }

        Ok(())
    }

    /// Appending unsaved items writes just those, once; writing everything writes it all.
    #[test]
    fn flush_writes_unsaved_items_once() -> Result<(), error::Error> {
        let dir = tempfile::tempdir()?;
        let path = dir.path().join("history");
        let read = || std::fs::read_to_string(&path);

        // Imported items are already saved; added ones aren't.
        let mut history = History::import(&b"old\n"[..])?;
        history.add(Item::new("new1"))?;
        history.add(Item::new("new2"))?;

        history.flush(&path, true, true, false)?;
        assert_eq!(read()?, "new1\nnew2\n");
        history.flush(&path, true, true, false)?;
        assert_eq!(read()?, "new1\nnew2\n");

        history.flush(&path, false, false, false)?;
        assert_eq!(read()?, "old\nnew1\nnew2\n");

        Ok(())
    }

    #[test]
    fn relative_lock_path_is_stable_before_and_after_creation() -> Result<(), error::Error> {
        let cwd = std::env::current_dir()?;
        let temp = tempfile::Builder::new()
            .prefix("brush-lock-relative-")
            .tempdir_in(&cwd)?;
        let relative = temp
            .path()
            .strip_prefix(&cwd)
            .map_err(std::io::Error::other)?
            .join("history");
        let before = history_lock_path(&relative);
        std::fs::write(temp.path().join("history"), "")?;
        let after = history_lock_path(&relative);
        assert_eq!(before, after);
        Ok(())
    }

    #[cfg(unix)]
    #[test]
    fn private_temp_creation_is_exclusive_and_private() -> Result<(), error::Error> {
        use std::os::unix::fs::PermissionsExt as _;

        let dir = tempfile::tempdir()?;
        let path = dir.path().join("temp");
        let mut file = crate::sys::fs::create_new_private_file(&path)?;
        file.write_all(b"secret")?;
        file.sync_all()?;
        drop(file);
        assert_eq!(
            std::fs::metadata(&path)?.permissions().mode() & 0o777,
            0o600
        );

        let Err(error) = crate::sys::fs::create_new_private_file(&path) else {
            return Err(
                std::io::Error::other("create_new must not truncate an existing object").into(),
            );
        };
        assert_eq!(error.kind(), std::io::ErrorKind::AlreadyExists);
        assert_eq!(std::fs::read_to_string(&path)?, "secret");
        Ok(())
    }

    #[cfg(unix)]
    #[test]
    fn symlink_and_target_share_a_history_lock() -> Result<(), error::Error> {
        use std::os::unix::fs::symlink;

        let dir = tempfile::tempdir()?;
        let target = dir.path().join("target");
        let link = dir.path().join("link");
        std::fs::write(&target, "")?;
        symlink(&target, &link)?;
        assert_eq!(history_lock_path(&target), history_lock_path(&link));
        Ok(())
    }

    #[cfg(unix)]
    #[test]
    fn rewrite_preserves_permissions_and_follows_symlinks() -> Result<(), error::Error> {
        use std::os::unix::fs::{PermissionsExt as _, symlink};

        let dir = tempfile::tempdir()?;
        let target = dir.path().join("target-history");
        std::fs::write(&target, "old\n")?;
        std::fs::set_permissions(&target, std::fs::Permissions::from_mode(0o600))?;

        let link = dir.path().join("history-link");
        symlink(&target, &link)?;

        let mut history = History::default();
        history.add(Item::new("new-entry"))?;
        history.save_with_limit(&link, false, false, false, None)?;

        assert!(
            std::fs::symlink_metadata(&link)?.file_type().is_symlink(),
            "rewrite replaced the symlink"
        );
        assert_eq!(std::fs::read_to_string(&target)?, "new-entry\n");
        assert_eq!(
            std::fs::metadata(&target)?.permissions().mode() & 0o777,
            0o600
        );
        Ok(())
    }

    #[cfg(feature = "serde")]
    #[test]
    fn old_serialized_state_saves_its_dirty_entries() -> Result<(), error::Error> {
        let mut history = History::default();
        history.add(Item::new("from-old-state"))?;

        let mut value = serde_json::to_value(&history).map_err(std::io::Error::other)?;
        value
            .as_object_mut()
            .ok_or_else(|| std::io::Error::other("history serializes as an object"))?
            .remove("first_dirty");
        let mut restored: History = serde_json::from_value(value).map_err(std::io::Error::other)?;
        assert!(restored.first_dirty.is_none());
        assert!(restored.items.iter().any(|item| item.dirty));

        let dir = tempfile::tempdir()?;
        let path = dir.path().join("history");
        restored.save_with_limit(&path, true, true, false, None)?;
        assert_eq!(std::fs::read_to_string(&path)?, "from-old-state\n");

        // Saving again must not duplicate the recovered item, and a new item must be added.
        restored.save_with_limit(&path, true, true, false, None)?;
        assert_eq!(std::fs::read_to_string(&path)?, "from-old-state\n");
        restored.add(Item::new("new-after-restore"))?;
        restored.save_with_limit(&path, true, true, false, None)?;
        let contents = std::fs::read_to_string(&path)?;
        assert_eq!(
            contents.matches("from-old-state").count(),
            1,
            "{contents:?}"
        );
        assert_eq!(
            contents.matches("new-after-restore").count(),
            1,
            "{contents:?}"
        );

        Ok(())
    }

    #[test]
    fn save_with_limit_keeps_every_concurrent_append() -> Result<(), error::Error> {
        let dir = tempfile::tempdir()?;
        let path = dir.path().join("history");
        let barrier = std::sync::Arc::new(std::sync::Barrier::new(7));

        let mut threads = Vec::new();
        for index in 0..6 {
            let path = path.clone();
            let barrier = barrier.clone();
            threads.push(std::thread::spawn(move || {
                let mut history = History::default();
                history.add(Item::new(format!("command-{index}")))?;
                barrier.wait();
                history.save_with_limit(&path, true, true, false, None)
            }));
        }

        barrier.wait();
        for thread in threads {
            thread
                .join()
                .map_err(|_| std::io::Error::other("writer thread panicked"))??;
        }

        let contents = std::fs::read_to_string(&path)?;
        for index in 0..6 {
            assert!(
                contents.contains(&format!("command-{index}")),
                "lost command-{index}: {contents:?}"
            );
        }
        Ok(())
    }

    #[test]
    fn append_preserves_an_unreadable_existing_file() -> Result<(), error::Error> {
        let dir = tempfile::tempdir()?;
        let path = dir.path().join("history");
        let original = b"kept\n\xff\xfe\n";
        std::fs::write(&path, original)?;

        let mut history = History::default();
        history.add(Item::new("appended"))?;
        history.save_with_limit(&path, true, true, false, None)?;

        let contents = std::fs::read(&path)?;
        assert!(contents.starts_with(original), "{contents:?}");
        assert!(contents.ends_with(b"appended\n"), "{contents:?}");

        // A size-limited save must not treat the read failure as an empty file.
        let original = std::fs::read(&path)?;
        let mut history = History::default();
        history.add(Item::new("must-not-write"))?;
        assert!(
            history
                .save_with_limit(&path, true, true, false, Some(1))
                .is_err()
        );
        assert_eq!(std::fs::read(&path)?, original);
        assert!(history.first_dirty.is_some());
        Ok(())
    }

    #[test]
    fn save_with_limit_keeps_the_newest_entries() -> Result<(), error::Error> {
        let dir = tempfile::tempdir()?;
        let path = dir.path().join("history");
        std::fs::write(&path, "#1\nold-1\n#2\nold-2\n#3\nother-session\n")?;

        let mut history = History::default();
        history.add(Item::new("mine"))?;
        history.save_with_limit(&path, true, true, true, Some(2))?;

        let contents = std::fs::read_to_string(&path)?;
        assert!(contents.contains("other-session"), "{contents:?}");
        assert!(contents.contains("mine"), "{contents:?}");
        assert!(!contents.contains("old-1"), "{contents:?}");
        Ok(())
    }

    #[cfg(unix)]
    #[test]
    fn save_with_limit_failure_keeps_the_file_and_allows_retry() -> Result<(), error::Error> {
        use std::os::unix::fs::PermissionsExt as _;

        let dir = tempfile::tempdir()?;
        let path = dir.path().join("history");
        std::fs::write(&path, "existing\n")?;
        std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o444))?;

        let mut history = History::default();
        history.add(Item::new("retry-me"))?;
        assert!(
            history
                .save_with_limit(&path, true, true, false, None)
                .is_err()
        );
        assert_eq!(std::fs::read_to_string(&path)?, "existing\n");
        assert!(history.first_dirty.is_some());

        std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o644))?;
        history.save_with_limit(&path, true, true, false, None)?;
        let contents = std::fs::read_to_string(&path)?;
        assert!(contents.contains("retry-me"), "{contents:?}");
        assert_eq!(
            std::fs::read_to_string(&path)?.matches("retry-me").count(),
            1
        );
        Ok(())
    }

    #[test]
    fn truncate_file_keeps_the_newest_entries() -> Result<(), error::Error> {
        let dir = tempfile::tempdir()?;

        let untimed = dir.path().join("untimed");
        std::fs::write(&untimed, "a\nb\nc\n")?;
        History::default().truncate_file(&untimed, 2)?;
        assert_eq!(std::fs::read_to_string(&untimed)?, "b\nc\n");

        let timed = dir.path().join("timed");
        std::fs::write(&timed, "#1\na\nb\n#2\nc\n")?;
        History::default().truncate_file(&timed, 1)?;
        assert_eq!(std::fs::read_to_string(&timed)?, "#2\nc\n");

        Ok(())
    }

    /// Each of a query's filters, on its own. ID and time bounds are exclusive, and an item
    /// with no timestamp isn't excluded by time bounds.
    #[test]
    fn query_includes_by_each_filter() {
        let at = |seconds| ItemTimestamp::from_unix_seconds(seconds);
        let item = |id, command_line: &str, timestamp| Item {
            id,
            command_line: command_line.to_owned(),
            timestamp,
            dirty: false,
        };

        assert!(Query::default().includes(&item(5, "echo hi", None)));

        let after_id = Query {
            not_at_or_before_id: Some(2),
            ..Query::default()
        };
        assert!(!after_id.includes(&item(2, "x", None)));
        assert!(after_id.includes(&item(3, "x", None)));
        let before_id = Query {
            not_at_or_after_id: Some(2),
            ..Query::default()
        };
        assert!(!before_id.includes(&item(2, "x", None)));
        assert!(before_id.includes(&item(1, "x", None)));

        let after_time = Query {
            not_at_or_before_time: at(100),
            ..Query::default()
        };
        assert!(!after_time.includes(&item(0, "x", at(100))));
        assert!(after_time.includes(&item(0, "x", at(101))));
        assert!(after_time.includes(&item(0, "x", None)));
        let before_time = Query {
            not_at_or_after_time: at(100),
            ..Query::default()
        };
        assert!(!before_time.includes(&item(0, "x", at(100))));
        assert!(before_time.includes(&item(0, "x", at(99))));
        assert!(before_time.includes(&item(0, "x", None)));

        for (filter, matching, other) in [
            (CommandLineFilter::Prefix("ec".to_owned()), "echo", "sec"),
            (CommandLineFilter::Suffix("ho".to_owned()), "echo", "hot"),
            (CommandLineFilter::Contains("ch".to_owned()), "echo", "cd"),
            (CommandLineFilter::Exact("echo".to_owned()), "echo", "echo "),
        ] {
            let query = Query {
                command_line_filter: Some(filter),
                ..Query::default()
            };
            assert!(query.includes(&item(0, matching, None)), "{matching:?}");
            assert!(!query.includes(&item(0, other, None)), "{other:?}");
        }
    }

    /// An empty history, positions past the end, and IDs no item has.
    #[test]
    fn empty_history_and_missing_items() -> Result<(), error::Error> {
        let mut empty = History::default();
        assert!(empty.is_empty());
        assert_eq!(empty.count(), 0);
        assert!(empty.get(0).is_none());
        assert!(empty.get_by_id(0)?.is_none());
        assert!(!empty.remove_nth_item(0));
        empty.delete_item_by_id(0)?;
        assert!(empty.update_by_id(0, Item::new("x")).is_err());
        assert_eq!(empty.iter().count(), 0);
        assert_eq!(empty.search(Query::default())?.count(), 0);

        let mut history = history(&["a", "b", "c"])?;
        assert!(!history.is_empty());
        assert_eq!(history.count(), 3);
        assert!(history.get(3).is_none());
        assert!(!history.remove_nth_item(3));
        history.delete_item_by_id(7)?;
        assert_eq!(command_lines(history.iter()), ["a", "b", "c"]);

        // Updating an item changes just it, where it is.
        history.update_by_id(1, Item::new("B"))?;
        assert_eq!(command_lines(history.iter()), ["a", "B", "c"]);
        assert_eq!(command_lines(Search::all(&history)), ["a", "B", "c"]);

        Ok(())
    }

    /// A new item is unsaved and stamped now; adding it gives it the next ID.
    #[test]
    fn new_items_are_unsaved_and_stamped() -> Result<(), error::Error> {
        let before = ItemTimestamp::now();
        let item = Item::new("ls");
        assert!(item.dirty);
        assert!(item.timestamp.is_some_and(|ts| ts >= before));

        let mut history = history(&["a"])?;
        assert_eq!(history.add(item)?, 1);
        assert_eq!(history.get(1).map(|item| item.id), Some(1));

        Ok(())
    }

    /// Importing reads command lines and the timestamp comments before them (the last of
    /// several; a comment that isn't one clears it), skips unreadable lines, and leaves the
    /// items saved and numbered from 0.
    #[test]
    fn import_groups_timestamped_multiline_entries() -> Result<(), error::Error> {
        let file = b"#1700000000\nfor x in 1; do\n echo inside-$x\ndone\n#1700000001\necho next\n";
        let history = History::import(&file[..])?;
        let items: Vec<_> = history.iter().collect();

        assert_eq!(items.len(), 2);
        assert_eq!(
            items[0].command_line,
            "for x in 1; do\n echo inside-$x\ndone"
        );
        assert_eq!(
            items[0].timestamp,
            ItemTimestamp::from_unix_seconds(1_700_000_000)
        );
        assert_eq!(items[1].command_line, "echo next");

        // Without timestamps there is no way to know entry boundaries; each line is its own
        // entry, as bash reads such a file.
        let untimed = History::import(&b"for x in 1; do\n echo inside\ndone\n"[..])?;
        assert_eq!(
            command_lines(untimed.iter()),
            ["for x in 1; do", " echo inside", "done"]
        );
        Ok(())
    }

    #[test]
    fn import_reads_history_files() -> Result<(), error::Error> {
        let file = b"#1700000000\na\n#1700000005\n#not a time\nb\n#1700000001\n#1700000002\nc\n\xff\xfe\nd\n";
        let mut history = History::import(&file[..])?;

        assert_eq!(
            command_lines(history.iter()),
            ["a", "#not a time\nb", "c\nd"]
        );
        let timestamps: Vec<_> = history.iter().map(|item| item.timestamp).collect();
        assert_eq!(
            timestamps[0],
            ItemTimestamp::from_unix_seconds(1_700_000_000)
        );
        assert_eq!(
            timestamps[1],
            ItemTimestamp::from_unix_seconds(1_700_000_005)
        );
        assert_eq!(
            timestamps[2],
            ItemTimestamp::from_unix_seconds(1_700_000_002)
        );
        assert!(history.iter().all(|item| !item.dirty));
        assert_eq!(
            history.iter().map(|item| item.id).collect::<Vec<_>>(),
            [0, 1, 2]
        );
        assert_eq!(history.add(Item::new("e"))?, 3);

        assert!(History::import(&b""[..])?.is_empty());

        Ok(())
    }

    /// Flushing appends to what's already in the file, with the timestamps of items that
    /// have them if asked; importing that reads them back.
    #[test]
    fn flush_and_import_round_trip_with_timestamps() -> Result<(), error::Error> {
        let dir = tempfile::tempdir()?;
        let path = dir.path().join("history");
        std::fs::write(&path, "existing\n")?;

        let mut history = History::default();
        history.add(Item {
            timestamp: ItemTimestamp::from_unix_seconds(1_700_000_000),
            ..Item::new("a")
        })?;
        history.add(Item {
            timestamp: ItemTimestamp::from_unix_seconds(1_700_000_001),
            ..Item::new("b")
        })?;
        history.flush(&path, true, true, true)?;
        assert_eq!(
            std::fs::read_to_string(&path)?,
            "existing\n#1700000000\na\n#1700000001\nb\n"
        );

        let reread = History::import(std::fs::File::open(&path)?)?;
        assert_eq!(command_lines(reread.iter()), ["existing", "a", "b"]);
        assert_eq!(
            reread.get(1).and_then(|item| item.timestamp),
            ItemTimestamp::from_unix_seconds(1_700_000_000)
        );

        Ok(())
    }

    #[test]
    fn last_and_retain() -> Result<(), error::Error> {
        assert!(History::default().last().is_none());

        let mut history = history(&["a", "x", "b", "x", "c", "x"])?;
        assert_eq!(
            history.last().map(|item| item.command_line.as_str()),
            Some("x")
        );

        // Keeping everything changes nothing.
        history.retain(|_| true);
        assert_eq!(
            command_lines(history.iter()),
            ["a", "x", "b", "x", "c", "x"]
        );

        // Removing some keeps the rest in order, with their IDs.
        history.retain(|item| item.command_line != "x");
        assert_eq!(command_lines(history.iter()), ["a", "b", "c"]);
        assert_eq!(
            history.iter().map(|item| item.id).collect::<Vec<_>>(),
            [0, 2, 4]
        );
        assert_eq!(
            history.get_by_id(4)?.map(|item| item.command_line.as_str()),
            Some("c")
        );

        history.retain(|item| item.command_line != "a");
        assert_eq!(command_lines(history.iter()), ["b", "c"]);
        history.retain(|_| false);
        assert!(history.is_empty());

        Ok(())
    }

    /// `HISTCONTROL` parses as bash reads it: `ignoreboth` is two values, and unknown values,
    /// empty ones, and other cases are ignored.
    #[test]
    fn history_control_parses_like_bash() {
        use HistoryControl::{EraseDups, IgnoreDups, IgnoreSpace};
        let parse = |value| {
            HistoryControl::parse_list(value)
                .into_iter()
                .collect::<Vec<_>>()
        };

        assert_eq!(parse("ignoredups"), [IgnoreDups]);
        assert_eq!(parse("ignorespace"), [IgnoreSpace]);
        assert_eq!(parse("erasedups"), [EraseDups]);
        assert_eq!(parse("ignoreboth"), [IgnoreSpace, IgnoreDups]);
        assert_eq!(parse("erasedups:ignorespace"), [IgnoreSpace, EraseDups]);
        assert_eq!(parse("bogus::ignoredups:"), [IgnoreDups]);
        assert_eq!(parse("IGNOREDUPS"), []);
        assert_eq!(parse(""), []);
    }

    /// Searches and iterators over history can be held across an await in a spawned task.
    #[test]
    fn searches_are_send_and_sync() -> Result<(), error::Error> {
        const fn assert_send_sync<T: Send + Sync>(_: &T) {}

        let history = history(&["a"])?;
        assert_send_sync(&history.iter());
        assert_send_sync(&history.search(Query::default())?);

        Ok(())
    }

    /// Like bash, an entry the file gives no timestamp gets the time it's read.
    #[test]
    fn import_stamps_entries_without_timestamps() -> Result<(), error::Error> {
        let before = ItemTimestamp::now();
        let history = History::import(&b"ls\necho hi\n"[..])?;

        let timestamps: Vec<_> = history.iter().map(|item| item.timestamp).collect();
        assert!(timestamps[0].is_some_and(|ts| ts >= before));
        assert!(timestamps[1].is_some_and(|ts| ts >= before));

        Ok(())
    }

    /// A timestamp past the year 9999 (here, one in milliseconds) is out of range, so the
    /// entry is stamped with the time it's read, as one with no timestamp is.
    #[test]
    fn import_stamps_entries_with_out_of_range_timestamps() -> Result<(), error::Error> {
        let before = ItemTimestamp::now();
        let history = History::import(&b"#253402207200\na\n#253402207201\nb\n"[..])?;

        let timestamps: Vec<_> = history.iter().map(|item| item.timestamp).collect();
        assert_eq!(
            timestamps[0],
            ItemTimestamp::from_unix_seconds(253_402_207_200)
        );
        assert!(timestamps[1].is_some_and(|ts| ts >= before));

        Ok(())
    }

    /// Converting to and from `SystemTime` keeps sub-second precision, on both sides of the
    /// epoch, and a time past the year 9999 is out of range.
    #[test]
    fn system_time_round_trips() {
        let after = std::time::UNIX_EPOCH + std::time::Duration::new(1_700_000_000, 123_456_789);
        let timestamp = ItemTimestamp::from_system_time(after);
        assert_eq!(
            timestamp.and_then(ItemTimestamp::to_system_time),
            Some(after)
        );
        assert!(timestamp > ItemTimestamp::from_unix_seconds(1_700_000_000));
        assert_eq!(
            timestamp.map(ItemTimestamp::as_unix_seconds),
            Some(1_700_000_000)
        );

        // Not every platform's SystemTime reaches before the epoch (wasm's doesn't).
        if let Some(before) =
            std::time::UNIX_EPOCH.checked_sub(std::time::Duration::new(5, 250_000_000))
        {
            let timestamp = ItemTimestamp::from_system_time(before);
            assert_eq!(
                timestamp.and_then(ItemTimestamp::to_system_time),
                Some(before)
            );
            // Whole seconds round down, as history files record them.
            assert_eq!(timestamp.map(ItemTimestamp::as_unix_seconds), Some(-6));
        }

        let far_future =
            std::time::UNIX_EPOCH.checked_add(std::time::Duration::from_secs(253_402_207_201));
        assert_eq!(far_future.and_then(ItemTimestamp::from_system_time), None);
    }

    /// Formatting renders the given format; seconds since the epoch don't depend on the
    /// local time zone.
    #[test]
    fn format_local_renders_the_format() {
        let timestamp = ItemTimestamp::from_unix_seconds(1_700_000_000);
        assert_eq!(
            timestamp.map(|ts| ts.format_local("at %s.")),
            Some("at 1700000000.".to_owned())
        );
    }
}
