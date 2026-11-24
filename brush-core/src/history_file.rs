//! Private history-file format and transaction helpers.
//!
//! `History` owns the in-memory entries; this module owns the bytes on disk: record boundaries,
//! timestamps, file identity, the read cursor, locking, and atomic replacement.

use std::{
    io::Write as _,
    path::{Path, PathBuf},
};

use crate::{error, history::Item};

/// Identity of a history file, used to detect replacement.
#[cfg(unix)]
pub(crate) type Identity = (u64, u64);
/// Identity of a history file, used to detect replacement.
#[cfg(not(unix))]
pub(crate) type Identity = ();

/// A byte cursor into a history file.
#[derive(Clone)]
pub(crate) struct ReadPosition {
    /// The file this cursor belongs to.
    pub path: PathBuf,
    /// Number of complete bytes already consumed.
    pub offset: u64,
    /// Identity of the file when the cursor was recorded.
    pub identity: Option<Identity>,
    /// Checksum of the bytes before `offset`, used to detect in-place replacement.
    pub checksum: u64,
}

/// File-format and cursor state owned by one shell session.
#[derive(Clone, Default)]
pub(crate) struct State {
    /// Byte cursor into the history file.
    pub read_position: Option<ReadPosition>,
    /// Records this session appended immediately and may need filtering on later reads.
    pub own_appends: Vec<OwnAppend>,
}

/// A record this session appended immediately, remembered until the read cursor passes it.
#[derive(Clone)]
pub(crate) struct OwnAppend {
    /// The history file the record was written to.
    pub path: PathBuf,
    /// The appended entry.
    pub item: Item,
}

/// Returns a checksum of the last (up to) 256 bytes of `bytes`.
pub(crate) fn tail_checksum(bytes: &[u8]) -> u64 {
    let start = bytes.len().saturating_sub(256);
    let mut hash = 0xcbf2_9ce4_8422_2325_u64;
    for byte in &bytes[start..] {
        hash ^= u64::from(*byte);
        hash = hash.wrapping_mul(0x100_0000_01b3);
    }
    hash
}

/// Returns the end of the last complete history record in `bytes`.
///
/// A newline normally completes a record, but in a timestamped file a timestamp without a
/// following command is only half a record. Leave that timestamp for the next read rather than
/// committing the cursor past it.
pub(crate) fn complete_record_end(bytes: &[u8]) -> usize {
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
    let mut trailing_timestamp = None;
    let mut has_command = false;
    for line in lines {
        if is_timestamp(line) {
            if has_command || trailing_timestamp.is_none() {
                trailing_timestamp = Some(offset);
                has_command = false;
            }
        } else {
            has_command = true;
        }
        offset += line.len() + 1;
    }

    if trailing_timestamp.is_some() && !has_command {
        trailing_timestamp.unwrap_or(end)
    } else {
        end
    }
}

/// Returns the identity used to detect a replaced history file.
pub(crate) fn file_identity(metadata: &std::fs::Metadata) -> Identity {
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

/// Keeps the last `max_items` entries in a history file's contents.
pub(crate) fn trim_history_entries(contents: &str, max_items: usize) -> String {
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

/// Locks the history file used at `path`.
pub(crate) fn lock_history_file(
    path: &Path,
) -> Result<crate::sys::fs::HistoryFileLock, error::Error> {
    let lock_path = lock_path(path);
    let file = std::fs::OpenOptions::new()
        .create(true)
        .truncate(false)
        .write(true)
        .open(lock_path)?;
    Ok(crate::sys::fs::lock_history_file(file)?)
}

/// Returns the fixed lock file path for a history path. It is deliberately independent of
/// `TMPDIR`, and symlinks are resolved so every spelling uses one lock.
pub(crate) fn lock_path(path: &Path) -> PathBuf {
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

/// Writes `contents` to an exclusively-created temporary file next to `target`, then renames
/// it into place. A symlinked target is resolved; existing permissions are preserved.
pub(crate) fn atomic_write(path: &Path, contents: &[u8]) -> Result<(), error::Error> {
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
        let _ = std::fs::remove_file(&temp_path);
    }
    result
}
