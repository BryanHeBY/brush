//! History regressions that require native Unix filesystem semantics.

#![expect(clippy::panic_in_result_fn)]

use std::io::Write as _;

use crate::{
    error,
    history::{History, HistoryWriteMode, Item},
    history_file::lock_path,
};

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

#[test]
fn symlink_and_target_share_a_history_lock() -> Result<(), error::Error> {
    use std::os::unix::fs::symlink;

    let dir = tempfile::tempdir()?;
    let target = dir.path().join("target");
    let link = dir.path().join("link");
    std::fs::write(&target, "")?;
    symlink(&target, &link)?;
    assert_eq!(lock_path(&target), lock_path(&link));
    Ok(())
}

#[test]
fn portable_history_and_lock_are_private() -> Result<(), error::Error> {
    use std::os::unix::fs::PermissionsExt as _;

    let dir = tempfile::tempdir()?;
    let path = dir.path().join("history");
    let mut history = History::default();
    history.add(Item::new("private-entry"))?;
    history.save_with_limit(&path, true, true, false, None)?;

    assert_eq!(
        std::fs::metadata(&path)?.permissions().mode() & 0o777,
        0o600
    );
    assert_eq!(
        std::fs::metadata(lock_path(&path))?.permissions().mode() & 0o777,
        0o600
    );

    let bash_path = dir.path().join("bash-history");
    let mut bash = History::default();
    bash.set_write_mode(HistoryWriteMode::Bash);
    bash.add(Item::new("private-bash-entry"))?;
    bash.save_with_limit(&bash_path, true, true, false, None)?;
    assert_eq!(
        std::fs::metadata(&bash_path)?.permissions().mode() & 0o777,
        0o600
    );
    assert!(!lock_path(&bash_path).exists());
    Ok(())
}

#[test]
fn portable_flush_and_trim_replace_atomically() -> Result<(), error::Error> {
    use std::os::unix::fs::MetadataExt as _;

    let dir = tempfile::tempdir()?;
    let path = dir.path().join("history");
    std::fs::write(&path, "old\n")?;
    let inode = std::fs::metadata(&path)?.ino();
    let mut history = History::default();
    history.add(Item::new("flushed"))?;
    history.flush(&path, false, false, false)?;
    assert_ne!(std::fs::metadata(&path)?.ino(), inode);
    assert_eq!(std::fs::read_to_string(&path)?, "flushed\n");

    std::fs::write(&path, ": one\n: two\n: three\n")?;
    let inode = std::fs::metadata(&path)?.ino();
    History::default().truncate_file(&path, 1)?;
    assert_ne!(std::fs::metadata(&path)?.ino(), inode);
    assert_eq!(std::fs::read_to_string(&path)?, ": three\n");
    Ok(())
}

#[test]
fn failed_portable_rewrite_preserves_the_original() -> Result<(), error::Error> {
    let dir = tempfile::tempdir()?;
    let path = dir.path().join("history");
    std::fs::write(&path, "original\n")?;
    std::fs::create_dir_all(lock_path(&path))?;

    let mut history = History::default();
    history.add(Item::new("replacement"))?;
    assert!(history.flush(&path, false, false, false).is_err());
    assert_eq!(std::fs::read_to_string(&path)?, "original\n");
    Ok(())
}

#[test]
fn write_modes_choose_between_in_place_and_atomic() -> Result<(), error::Error> {
    use std::os::unix::fs::MetadataExt as _;

    let dir = tempfile::tempdir()?;
    let path = dir.path().join("history");
    std::fs::write(&path, "old\n")?;
    let inode = std::fs::metadata(&path)?.ino();

    let mut history = History::default();
    history.set_write_mode(HistoryWriteMode::Bash);
    history.add(Item::new("bash-entry"))?;
    history.save_with_limit(&path, false, false, false, None)?;
    assert_eq!(std::fs::metadata(&path)?.ino(), inode);
    assert!(!lock_path(&path).exists());

    std::fs::write(&path, "old\n")?;
    let inode = std::fs::metadata(&path)?.ino();
    let mut history = History::default();
    history.set_write_mode(HistoryWriteMode::Portable);
    history.add(Item::new("portable-entry"))?;
    history.save_with_limit(&path, false, false, false, None)?;
    assert_ne!(std::fs::metadata(&path)?.ino(), inode);
    assert!(lock_path(&path).exists());
    Ok(())
}

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
    assert!(history.iter().any(|item| item.dirty));

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
