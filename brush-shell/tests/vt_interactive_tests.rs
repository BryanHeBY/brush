//! Syntax highlighting options on the VT backend, over a real pseudo-terminal.
//!
//! Like the other pty tests, these run from a cargo target directory rather than
//! as prebuilt compatibility-test artifacts.

#![cfg(test)]
#![cfg(vt_backend)]
#![cfg(any(target_os = "linux", target_os = "macos", target_os = "freebsd"))]
#![allow(clippy::panic_in_result_fn)]

use std::time::Duration;

use anyhow::Context;
use expectrl::Expect;

#[test]
fn vt_highlighting_respects_cli_and_config() -> anyhow::Result<()> {
    for (config_enabled, italics, flags, expected) in [
        (Some(false), true, vec![], false),
        (None, false, vec!["--enable-highlighting"], true),
        (Some(true), false, vec![], true),
        (Some(true), true, vec![], true),
        (Some(true), true, vec!["--disable-color"], false),
        (
            None,
            false,
            vec!["--enable-highlighting", "--disable-color"],
            false,
        ),
    ] {
        let config_dir = tempfile::tempdir()?;
        let config_path = config_dir.path().join("config.toml");
        let mut config = format!("[ui]\nitalics = {italics}\n");
        if let Some(enabled) = config_enabled {
            use std::fmt::Write as _;
            writeln!(config, "syntax-highlighting = {enabled}")?;
        }
        std::fs::write(&config_path, config)?;
        let mut cmd = std::process::Command::new(assert_cmd::cargo::cargo_bin!("brush"));
        cmd.args(["--norc", "--noprofile", "--input-backend=vt", "--config"])
            .arg(&config_path)
            .args(&flags)
            .env("PS1", "brush> ")
            .env("TERM", "xterm-256color");
        let mut session = expectrl::Session::spawn(cmd)?;
        session.set_expect_timeout(Some(Duration::from_secs(8)));
        session.expect("brush> ").context("VT prompt missing")?;
        // Interrupt after typing so the captured bytes contain the redraws but
        // no command output that could supply its own color sequences.
        session.send("echo --color 'value' $HOME | cat")?;
        session.send("\x03")?;
        let captured = session.expect("^C")?;
        let output = String::from_utf8_lossy(captured.before());
        assert_eq!(
            output.contains("\x1b[33m"),
            expected,
            "config={config_enabled:?}, flags={flags:?}: {output:?}"
        );
        let plain_option = output.contains("\x1b[38;2;255;177;120m--color");
        // ANSI parameters may be padded when nu-ansi-term's gnu_legacy feature is on.
        let italic_option = output.contains("\x1b[03;38;2;255;177;120m--color")
            || output.contains("\x1b[3;38;2;255;177;120m--color");
        let italic_operator = output.contains("\x1b[03;39m|") || output.contains("\x1b[3;39m|");
        assert_eq!(plain_option, expected && !italics);
        assert_eq!(italic_option, expected && italics);
        assert_eq!(italic_operator, expected && italics);
        session.expect("brush> ").context("VT prompt missing")?;
        session.send("\x04")?;
        session.expect(expectrl::Eof)?;
    }
    Ok(())
}
