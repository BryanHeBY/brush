//! Filesystem utilities

pub use super::platform::fs::*;

/// Extension trait for path-related filesystem operations.
pub trait PathExt {
    /// Returns true if the path exists and is readable by the current user.
    fn readable(&self) -> bool;
    /// Returns true if the path exists and is writable by the current user.
    fn writable(&self) -> bool;
    /// Returns true if the path exists and is executable by the current user.
    ///
    /// On Windows, this returns true if *either* the path itself is a file with
    /// a `PATHEXT` extension *or* appending some `PATHEXT` extension resolves
    /// to an existing file. To recover the actual on-disk path in the
    /// latter case, use [`resolve_executable`] which takes ownership
    /// and avoids copies on platforms where no resolution is needed.
    fn executable(&self) -> bool;

    /// Returns true if the path exists and is a block device.
    fn exists_and_is_block_device(&self) -> bool;
    /// Returns true if the path exists and is a character device.
    fn exists_and_is_char_device(&self) -> bool;
    /// Returns true if the path exists and is a FIFO (named pipe).
    fn exists_and_is_fifo(&self) -> bool;
    /// Returns true if the path exists and is a socket.
    fn exists_and_is_socket(&self) -> bool;
    /// Returns true if the path exists and has the setgid bit set.
    fn exists_and_is_setgid(&self) -> bool;
    /// Returns true if the path exists and has the setuid bit set.
    fn exists_and_is_setuid(&self) -> bool;
    /// Returns true if the path exists and has the sticky bit set.
    fn exists_and_is_sticky_bit(&self) -> bool;

    /// Returns the device ID and inode number for the path.
    fn get_device_and_inode(&self) -> Result<(u64, u64), crate::error::Error>;
}

/// A volume or device identifier paired with an open file's identity.
pub(crate) type HistoryFileIdentity = (u64, u128);

/// Reads file metadata and identity from one handle, falling back to no identity where needed.
pub(crate) fn history_file_metadata(
    file: &std::fs::File,
) -> std::io::Result<(std::fs::Metadata, Option<HistoryFileIdentity>)> {
    let metadata = file.metadata()?;
    #[cfg(unix)]
    let identity = {
        use std::os::unix::fs::MetadataExt as _;
        Some((metadata.dev(), u128::from(metadata.ino())))
    };
    #[cfg(windows)]
    let identity = super::windows::fs::history_file_identity(file).ok();
    #[cfg(not(any(unix, windows)))]
    let identity = None;
    Ok((metadata, identity))
}

/// Creates an empty temporary history file that can receive the original access rules.
pub(crate) fn new_history_replacement(path: &std::path::Path) -> std::io::Result<std::fs::File> {
    #[cfg(windows)]
    {
        create_history_replacement(path)
    }
    #[cfg(not(windows))]
    {
        create_new_private_file(path)
    }
}

/// Copies access rules that must be applied before the first write.
#[cfg_attr(
    not(windows),
    expect(
        clippy::missing_const_for_fn,
        clippy::unnecessary_wraps,
        reason = "the shared filesystem interface is fallible on Windows"
    )
)]
pub(crate) fn prepare_history_replacement(
    file: &std::fs::File,
    target: &std::path::Path,
    target_exists: bool,
) -> std::io::Result<()> {
    #[cfg(windows)]
    if target_exists {
        copy_history_dacl(file, target)?;
    }
    #[cfg(not(windows))]
    let _ = (file, target, target_exists);
    Ok(())
}

/// Copies the original group where the platform has POSIX groups.
#[cfg_attr(
    not(unix),
    expect(
        clippy::missing_const_for_fn,
        clippy::unnecessary_wraps,
        reason = "the shared filesystem interface is fallible on Unix"
    )
)]
pub(crate) fn preserve_history_group(
    file: &std::fs::File,
    metadata: Option<&std::fs::Metadata>,
) -> std::io::Result<()> {
    #[cfg(unix)]
    if let Some(metadata) = metadata {
        use std::os::unix::fs::MetadataExt as _;
        set_file_group(file, metadata.gid())?;
    }
    #[cfg(not(unix))]
    let _ = (file, metadata);
    Ok(())
}

#[cfg(test)]
#[cfg(any(unix, windows))]
mod tests {
    use std::io::Write as _;

    use crate::history_file::{lock_history_file, trim_history_file};

    #[test]
    fn portable_trim_retains_appends_from_the_current_lock_holder() -> anyhow::Result<()> {
        use std::{fs::OpenOptions, sync::mpsc, time::Duration};

        let dir = tempfile::tempdir()?;
        let path = dir.path().join("history");
        std::fs::write(&path, b"old1\nold2\nold3\n")?;
        let lock = lock_history_file(&path)?;
        let (ready_tx, ready_rx) = mpsc::channel();
        let (done_tx, done_rx) = mpsc::channel();
        let trim_path = path.clone();
        let trim = std::thread::spawn(move || {
            let _ = ready_tx.send(());
            let result =
                trim_history_file(&trim_path, 2, crate::history::HistoryWriteMode::Portable);
            let _ = done_tx.send(());
            result
        });

        let ready = ready_rx.recv_timeout(Duration::from_secs(5));
        let waited = done_rx.recv_timeout(Duration::from_millis(100));
        let append = (|| -> std::io::Result<()> {
            let mut file = OpenOptions::new().append(true).open(&path)?;
            file.write_all(b"other-session\n")
        })();
        drop(lock);
        trim.join()
            .map_err(|_| anyhow::anyhow!("trim thread panicked"))??;

        ready?;
        anyhow::ensure!(
            matches!(waited, Err(mpsc::RecvTimeoutError::Timeout)),
            "trim did not wait for the sidecar lock"
        );
        append?;
        anyhow::ensure!(
            std::fs::read(&path)? == b"old3\nother-session\n",
            "trim discarded the concurrent append"
        );
        Ok(())
    }
}
