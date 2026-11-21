//! Interactive behavior tests for the reedline input backend, run over a
//! real pseudo-terminal.
//!
//! `pty_startup_tests.rs` covers *startup* on the default backend and
//! `interactive_tests.rs` covers job control on the `basic` backend; this file
//! is for behavior specific to reedline's line editing (key bindings, its
//! interaction with the terminal).
//!
//! Unlike the basic backend, reedline queries the terminal for the cursor
//! position (DSR, `ESC [ 6 n`) before it paints a prompt. A pty with nothing
//! on the other end never answers, so these tests play the role of the
//! terminal emulator and answer each query themselves (or, for the timeout
//! test, deliberately withhold an answer).

// Only compile this for platforms supported by expectrl's pty backend.
#![cfg(any(
    target_os = "linux",
    target_os = "macos",
    target_os = "freebsd",
    target_os = "netbsd",
    target_os = "openbsd"
))]
#![cfg(test)]
#![allow(clippy::panic_in_result_fn)]

use std::time::Duration;

use anyhow::Context;
use expectrl::{
    Expect, Session,
    process::unix::{PtyStream, UnixProcess},
    stream::log::LogStream,
};

const PROMPT: &str = "brush> ";
const DSR_QUERY: &str = "\x1b[6n";
const DSR_REPLY: &str = "\x1b[1;1R";

/// OSC 633 markers bracketing a command: emitted before it runs, and after.
const COMMAND_STARTED: &str = "\x1b]633;C";
const COMMAND_FINISHED: &str = "\x1b]633;D";

/// A key bound with `bind -x` must run its command and leave the shell
/// alive. Regression test for reedline >= 0.48 delivering such keys as
/// `Signal::HostCommand`, which previously fell through to a fatal
/// "unexpected error occurred reading input".
#[test]
fn bound_key_runs_command_and_shell_survives() -> anyhow::Result<()> {
    let mut session = start_reedline_session()?;
    expect_next_prompt(&mut session, 0)?;

    // The bound command's output is split so the echoed keystrokes of the
    // `bind` line itself can't satisfy the expectation below.
    session.send_line(r#"bind -x '"\C-t": echo BOUND_""FIRED'"#)?;
    expect_next_prompt(&mut session, 0)?;

    // Ctrl+T.
    session.send("\x14")?;
    session
        .expect("BOUND_FIRED")
        .context("bound command did not run")?;
    expect_next_prompt(&mut session, 0).context("no prompt after bound command")?;

    // The shell must still be interactive afterwards.
    session.send_line("echo STILL_$((40+2))")?;
    session
        .expect("STILL_42")
        .context("shell did not survive the bound command")?;

    Ok(())
}

/// Like readline, completion sees the whole line, not just the text before the cursor:
/// e.g. `COMP_WORDS` includes words after it.
#[test]
fn completion_sees_text_after_cursor() -> anyhow::Result<()> {
    let mut session = start_reedline_session()?;
    expect_next_prompt(&mut session, 0)?;

    session.send_line(
        r#"_f() { echo "WORDS""=<${COMP_WORDS[*]}> CWORD=$COMP_CWORD" >&2; }; complete -F _f mycmd"#,
    )?;
    expect_next_prompt(&mut session, 0)?;

    // Type the line, move the cursor back before `b`, and press Tab.
    session.send("mycmd a b")?;
    session.send("\x1b[D\t")?;
    session
        .expect("WORDS=<mycmd a b> CWORD=2")
        .context("completion did not see the whole line")?;

    Ok(())
}

/// `preexec` fires for lines the user typed, not for a command a key binding ran. The
/// hooks exist to observe what the user is about to run; bash-preexec has the same split,
/// because a `bind -x` command runs from readline rather than from the command line.
#[test]
fn bound_key_command_does_not_fire_preexec() -> anyhow::Result<()> {
    let mut session = start_reedline_session_with(|cmd| {
        cmd.arg("--enable-zsh-hooks");
    })?;
    expect_next_prompt(&mut session, 0)?;

    // The hook counts its dispatches as well as reporting its argument, so a stray dispatch
    // for the bound command can't hide between the markers below. Markers are split so the
    // echoed keystrokes of the setup lines can't satisfy the expectations.
    session.send_line(r#"preexec() { n=$((n+1)); echo "PRE_""EXEC[$1]"; }"#)?;
    expect_next_prompt(&mut session, 0)?;

    // Dispatch 1, for this line.
    session.send_line(r#"bind -x '"\C-t": echo BOUND_""FIRED'"#)?;
    expect_next_prompt(&mut session, 0)?;

    // Ctrl+T. The bound command runs, but nothing was typed, so no dispatch.
    session.send("\x14")?;
    session
        .expect("BOUND_FIRED")
        .context("bound command did not run")?;
    expect_next_prompt(&mut session, 0).context("no prompt after bound command")?;

    // Dispatch 2, for this line -- `$n` expands after the hook has already run, so a third
    // dispatch anywhere would show up here.
    session.send_line("echo COUNT_$n")?;
    session
        .expect("PRE_EXEC[echo COUNT_$n]")
        .context("preexec did not fire for a typed line")?;
    session
        .expect("COUNT_2")
        .context("preexec fired for something other than the two typed lines")?;

    Ok(())
}

/// A command a key binding runs is bracketed by the same OSC 633 marker pair as a typed one.
/// Terminals track commands by that pair, so emitting only the closing marker -- as this path
/// used to -- leaves one counting a command that never started.
#[test]
fn osc_command_markers_stay_paired_for_a_bound_command() -> anyhow::Result<()> {
    let mut session = start_reedline_session_with(|cmd| {
        cmd.arg("--enable-terminal-integration");
        // OSC 633 is emitted only for terminals known to understand it.
        cmd.env("TERM_PROGRAM", "vscode");
    })?;
    expect_next_prompt(&mut session, 0)?;

    session.send_line(r#"bind -x '"\C-t": echo BOUND_""FIRED'"#)?;
    expect_next_prompt(&mut session, 0)?;

    // Ctrl+T. The bound command runs without the user typing a line, but it is still a
    // command, so it gets both markers around it.
    session.send("\x14")?;
    session
        .expect(COMMAND_STARTED)
        .context("no command-started marker for the bound command")?;
    session
        .expect("BOUND_FIRED")
        .context("bound command did not run")?;
    session
        .expect(COMMAND_FINISHED)
        .context("no command-finished marker for the bound command")?;

    Ok(())
}

/// A single unanswered cursor-position query (a transient terminal hiccup,
/// e.g. right after a full-screen program hands the terminal back) must not
/// terminate the shell; reedline asks again and uses the next answer.
#[test]
fn transient_cursor_query_timeout_is_tolerated() -> anyhow::Result<()> {
    let mut session = start_reedline_session()?;
    expect_next_prompt(&mut session, 0)?;

    // Withhold the answer to the query preceding the next prompt; crossterm
    // gives up on it after ~2s and the shell must carry on rather than exit.
    session.send_line("echo BEFORE_$((7*6))")?;
    session.expect("BEFORE_42")?;
    expect_next_prompt(&mut session, 1)
        .context("shell did not survive one unanswered cursor query")?;

    session.send_line("echo AFTER_$((6*7))")?;
    session
        .expect("AFTER_42")
        .context("shell not interactive after the unanswered query")?;

    Ok(())
}

/// A terminal that never answers cursor-position queries still gets a
/// prompt and a working shell; reedline (>= 0.52) falls back to anchoring
/// the prompt at the bottom of the screen instead of failing the read.
#[test]
fn unanswered_cursor_queries_still_yield_a_prompt() -> anyhow::Result<()> {
    let mut session = start_reedline_session()?;
    expect_next_prompt(&mut session, 0)?;

    session.send_line("echo BEFORE_$((7*6))")?;
    session.expect("BEFORE_42")?;

    // Never answer from here on.
    session
        .expect(PROMPT)
        .context("no prompt without cursor-query answers")?;
    session.send_line("echo AFTER_$((6*7))")?;
    session
        .expect("AFTER_42")
        .context("shell not interactive without cursor-query answers")?;

    Ok(())
}

/// Exercise editing and completion with real input bytes. Setting `BASH_PATH`
/// also runs the same locally generated fixtures through the Bash oracle.
#[cfg(target_os = "linux")]
#[test]
fn readline_file_behavior_matches_bash() -> anyhow::Result<()> {
    let temp = tempfile::tempdir()?;
    create_readline_fixtures(temp.path())?;

    let bash = std::env::var_os("BASH_PATH").map(Some);
    for bash in std::iter::once(None).chain(bash) {
        let oracle = bash.is_some();
        let configured = |cmd: &mut std::process::Command| {
            cmd.env("INPUTRC", temp.path().join("startup"))
                .env("HOME", temp.path())
                .env("HISTFILE", "")
                .env("PROMPT_COMMAND", "")
                .env("LC_ALL", "C.UTF-8")
                .env_remove("BASH_ENV")
                .current_dir(temp.path());
        };
        let mut session = if let Some(bash) = bash {
            let mut cmd = std::process::Command::new(bash);
            cmd.args(["--norc", "--noprofile", "-i"])
                .env("PS1", PROMPT)
                .env("TERM", "xterm-256color");
            configured(&mut cmd);
            let session = expectrl::Session::spawn(cmd)?;
            let mut session = expectrl::session::log(session, std::io::stdout())?;
            session.set_expect_timeout(Some(Duration::from_secs(15)));
            session
        } else {
            start_reedline_session_with(configured)?
        };
        let prompt = |session: &mut ShellSession| -> anyhow::Result<()> {
            if oracle {
                session.expect(PROMPT)?;
            } else {
                expect_next_prompt(session, 0)?;
            }
            Ok(())
        };
        let submit = |session: &mut ShellSession, keys: &str, output: &str| -> anyhow::Result<()> {
            session.send(keys)?;
            let output = format!("{output}\r\n");
            expect_with_terminal_answers(session, &output)
                .with_context(|| format!("oracle={oracle}, input={keys:?}"))?;
            prompt(session)
        };

        prompt(&mut session)?;
        submit(
            &mut session,
            "bind -v | grep '^set completion-query-items '\r",
            "set completion-query-items 97",
        )?;
        check_readline_reload(&mut session, temp.path(), &prompt, &submit, oracle)?;
        submit(
            &mut session,
            "bind -f keys; printf 'load:%s\\n' $?\r",
            "load:0",
        )?;
        session.send_line(r#"observe(){ printf 'OBS:%s\n' "$*"; }"#)?;
        prompt(&mut session)?;
        submit(&mut session, "observe hist-older\r", "OBS:hist-older")?;
        submit(&mut session, "observe hist-newer\r", "OBS:hist-newer")?;
        submit(&mut session, "observe unrelated\r", "OBS:unrelated")?;

        // Set the search prefix with both Left and Right while the cursor is mid-line.
        let history = "observe hist-junk\x1b[D\x1b[D\x1b[D\x1b[D\x1b[D\x1b[C\x1b[A\x1b[A\x1b[B\r";
        submit(&mut session, history, "OBS:hist-newer")?;

        submit(&mut session, "observe sam\t\t\r", "OBS:sample-alpha")?;
        submit(&mut session, "observe sam\x1b[Z\x1b[Z\r", "OBS:sample-beta")?;
        submit(&mut session, "observe sam\t\t\t\t\r", "OBS:sample-")?;
        submit(&mut session, "observe merc\t\r", "OBS:Mercury")?;
        submit(&mut session, "observe mozil\x1b[D\x1b[D\t\r", "OBS:mozilla")?;
        submit(&mut session, "observe li\t\r", "OBS:linked/")?;
        submit(&mut session, "observe .hi\t\r", "OBS:.hidden")?;
        submit(
            &mut session,
            "observe public-only/\t\r",
            "OBS:public-only/public",
        )?;
        submit(&mut session, "observe hé中\r", "OBS:hé中")?;

        session.send("observe sam\t")?;
        expect_with_terminal_answers(&mut session, "...alpha")?;
        expect_with_terminal_answers(&mut session, "...beta")?;
        submit(&mut session, "\r", "OBS:sample-")?;

        session.send_line("bind 'set completion-query-items 2'")?;
        prompt(&mut session)?;
        session.send("observe q\t")?;
        expect_with_terminal_answers(&mut session, "Display all 3 possibilities? (y or n)")?;
        session.send("n")?;
        submit(&mut session, "\r", "OBS:q")?;

        session.send_line("bind 'set menu-complete-display-prefix off'")?;
        prompt(&mut session)?;
        session.send("observe sam\t")?;
        expect_with_terminal_answers(&mut session, "Display all 2 possibilities? (y or n)")?;
        session.send("n")?;
        submit(&mut session, "\r", "OBS:sample-")?;

        session.send_line("bind 'set completion-query-items 20'")?;
        prompt(&mut session)?;
        submit(&mut session, "observe sam\t\r", "OBS:sample-alpha")?;

        check_readline_keymaps(&mut session, &prompt, &submit, oracle)?;
    }
    Ok(())
}

#[cfg(target_os = "linux")]
fn check_readline_reload(
    session: &mut ShellSession,
    root: &std::path::Path,
    prompt: &impl Fn(&mut ShellSession) -> anyhow::Result<()>,
    submit: &impl Fn(&mut ShellSession, &str, &str) -> anyhow::Result<()>,
    oracle: bool,
) -> anyhow::Result<()> {
    let reload = root.join("reload keys");
    std::fs::write(&reload, "set completion-query-items 41\n")?;
    session.send_line("bind -f 'reload keys'")?;
    prompt(session)?;
    std::fs::write(&reload, "set completion-query-items 57\n")?;
    session.send("\x18\x12")?;
    if !oracle {
        prompt(session)?;
    }
    submit(
        session,
        "bind -v | grep '^set completion-query-items '\r",
        "set completion-query-items 57",
    )?;
    Ok(())
}

#[cfg(target_os = "linux")]
fn check_readline_keymaps(
    session: &mut ShellSession,
    prompt: &impl Fn(&mut ShellSession) -> anyhow::Result<()>,
    submit: &impl Fn(&mut ShellSession, &str, &str) -> anyhow::Result<()>,
    oracle: bool,
) -> anyhow::Result<()> {
    session.send_line(r#"bind -m emacs-ctlx 'q: "inserted text"'"#)?;
    prompt(session)?;
    submit(session, "observe \x18q\r", "OBS:inserted text")?;
    session.send_line("bind 'set completion-map-case on'")?;
    prompt(session)?;
    submit(session, "observe kebab-\t\r", "OBS:kebab_pair")?;
    session.send_line("bind 'set mark-directories off'")?;
    prompt(session)?;
    submit(session, "observe lin\t\r", "OBS:linked")?;
    session.send_line("bind 'set mark-directories on'")?;
    prompt(session)?;
    session.send_line("bind 'TAB: complete' 'set disable-completion on'")?;
    prompt(session)?;
    submit(session, "observe sam\textra\r", "OBS:sam extra")?;
    session.send_line("bind 'set disable-completion off'")?;
    prompt(session)?;

    session.send_line(
        r#"bind 'set editing-mode vi'; bind 'set show-mode-in-prompt on'; bind -m vi-command 'q: beginning-of-line'; bind -m vi-insert 'Control-t: "insert mode"'"#,
    )?;
    prompt(session)?;
    submit(session, "observe \x14\r", "OBS:insert mode")?;
    if oracle {
        return Ok(());
    }
    session.send("bserve value")?;
    session.send("\x1b")?;
    expect_with_terminal_answers(session, "(cmd)")?;
    submit(session, "qio\r", "OBS:value")?;
    Ok(())
}

#[cfg(target_os = "linux")]
fn create_readline_fixtures(root: &std::path::Path) -> std::io::Result<()> {
    std::fs::write(root.join("startup"), "set completion-query-items 97\n")?;
    let settings = r#"
set input-meta on
set output-meta on
set convert-meta off
set completion-ignore-case on
set completion-prefix-display-length 3
set show-all-if-unmodified on
set show-all-if-ambiguous on
set mark-symlinked-directories on
set match-hidden-files off
set page-completions off
set completion-query-items 20
set visible-stats on
set skip-completed-text on
set colored-stats on
set menu-complete-display-prefix on
"\e[D": backward-char
"\e[C": forward-char
"\e[A": history-search-backward
"\e[B": history-search-forward
TAB: menu-complete
"\e[Z": menu-complete-backward
"#;
    std::fs::write(root.join("keys"), settings)?;
    for name in [
        "sample-alpha",
        "sample-beta",
        "Mercury",
        "kebab_pair",
        "mozilla",
        "q0",
        "q1",
        "q2",
        ".hidden",
    ] {
        std::fs::File::create(root.join(name))?;
    }
    std::fs::create_dir(root.join("target-directory"))?;
    std::os::unix::fs::symlink("target-directory", root.join("linked"))?;
    std::fs::create_dir(root.join("public-only"))?;
    std::fs::File::create(root.join("public-only/.private"))?;
    std::fs::File::create(root.join("public-only/public"))?;
    Ok(())
}

/// Exercise recording, nested playback, and playback across two prompt reads
/// in an isolated terminal, with Bash as the optional second implementation.
#[cfg(target_os = "linux")]
#[test]
fn readline_macros_match_bash() -> anyhow::Result<()> {
    for_readline_shell(
        r#"
"\C-]": print-last-kbd-macro
"\C-g": "nested"
"\C-t": "\C-g!"
"\C-y": "ABC\C-?"
"\C-xq": "printf 'FIRST %s\\n' 1\rprintf 'SECOND %s\\n' 2\r"
"#,
        |shell| {
            shell.session.send("\x18(")?;
            for key in ["A", "B", "\x02", "C", "\x05", "\x18)"] {
                shell.session.send(key)?;
                // Deliver a subsequent key after Readline has had a chance to
                // process the previous key as typing, rather than as a paste.
                std::thread::sleep(Duration::from_millis(30));
            }
            shell.session.send("\x1d")?;
            shell.expect("AB\\C-bC\\C-e\r\n")?;
            shell.clear_line()?;

            // Apply the recorded edits to the argument of a new command.
            shell.session.send("printf '%s\\n' ")?;
            shell.expect("printf '%s\\n' ")?;
            shell.session.send("\x1b2\x18e\r")?;
            shell.expect("\x1b[?2004l")?;
            shell.expect("ACBACB\r\n")?;
            shell.prompt()?;

            shell.session.send("printf 'NEST=%s\\n' ")?;
            shell.expect("printf 'NEST=%s\\n' ")?;
            shell.session.send("\x14\r")?;
            shell.expect("NEST=nested!\r\n")?;
            shell.prompt()?;

            shell.run_and_expect("printf '%s\\n' \x19", "AB")?;

            shell.session.send("\x18q")?;
            shell.expect("FIRST 1\r\n")?;
            shell.expect("SECOND 2\r\n")?;
            shell.prompt()?;
            shell.session.send("\x18(")?;
            shell
                .session
                .send("\x1b[200~printf 'RECORDED %s\\n' 3\x1b[201~")?;
            shell.expect("RECORDED %s")?;
            shell.session.send("\r")?;
            shell.expect("\x1b[?2004l")?;
            shell.expect("RECORDED 3\r\n")?;
            shell.prompt()?;
            shell.session.send("\x18)")?;

            shell.session.send("\x18e")?;
            shell.expect("\x1b[?2004l")?;
            shell.expect("RECORDED 3\r\n")?;
            shell.prompt()?;
            Ok(())
        },
    )
}

#[cfg(target_os = "linux")]
#[test]
fn readline_cyclic_macros_return_control_to_the_terminal() -> anyhow::Result<()> {
    for_readline_shell(
        r#"
"\C-g": "\C-t"
"\C-t": "\C-g"
"#,
        |shell| {
            if !shell.bash {
                // Bash is only used as an oracle for terminating macros.
                shell.session.send("\x07")?;
                shell.expect("\x07")?;
                shell.run_and_expect("echo recovered", "recovered")?;
            }
            Ok(())
        },
    )
}

/// Check the two encodings for inputrc's Meta notation and the difference
/// between character and byte cursor motion in a UTF-8 session.
#[cfg(target_os = "linux")]
#[test]
fn readline_raw_keys_match_bash() -> anyhow::Result<()> {
    for_readline_shell(
        r#"
set convert-meta off
set force-meta-prefix off
"\M-j": "HIGH"
set force-meta-prefix on
"\M-k": "ESC"
"#,
        |shell| {
            shell.command(
                r#"bind -x '"\C-g":printf "POINT""=%s LINE=%s\n" "$READLINE_POINT" "$READLINE_LINE"'"#,
            )?;
            shell.session.send(b"echo \xea\x07")?;
            shell.expect("POINT=9 LINE=echo HIGH\r\n")?;
            shell.resume_host_callback()?;
            shell.clear_line()?;

            shell.session.send("echo \x1bk\x07")?;
            shell.expect("POINT=8 LINE=echo ESC\r\n")?;
            shell.resume_host_callback()?;
            shell.clear_line()?;

            for (setting, cursor) in [("on", 7), ("off", 6)] {
                shell.command(&format!("bind 'set byte-oriented {setting}'"))?;
                shell.session.send("echo aéZ\x02\x02\x07")?;
                shell.expect(&format!("POINT={cursor} LINE=echo aéZ\r\n"))?;
                shell.resume_host_callback()?;
                shell.clear_line()?;
            }
            for (setting, remaining) in [("on", "aZ"), ("off", "Z")] {
                shell.command(&format!("bind 'set byte-oriented {setting}'"))?;
                shell.run_and_expect("printf '%s\\n' aéZ\x02\x7f\x7f\x05", remaining)?;
            }
            shell.command(r#"bind 'set convert-meta on' '"\M-j": "CONVERTED"'"#)?;
            shell.session.send(b"echo \xea\x07")?;
            shell.expect("POINT=14 LINE=echo CONVERTED\r\n")?;
            shell.resume_host_callback()?;
            shell.clear_line()?;
            Ok(())
        },
    )
}

#[cfg(target_os = "linux")]
#[test]
fn readline_byte_input_preserves_typeahead_during_cursor_query() -> anyhow::Result<()> {
    for_readline_shell("set byte-oriented on\n", |shell| {
        if !shell.bash {
            shell.session.send(":\r")?;
            shell.expect("\x1b[?2004l")?;
            shell.session.expect(DSR_QUERY)?;
            shell.session.send("printf 'TYPEAHEAD %s\\n' 7\r")?;
            shell.session.send(DSR_REPLY)?;
            shell.expect("\x1b[?2004l")?;
            shell.expect("TYPEAHEAD 7\r\n")?;
            shell.prompt()?;
        }
        Ok(())
    })
}

#[cfg(target_os = "linux")]
#[test]
fn readline_terminal_display_matches_bash() -> anyhow::Result<()> {
    for_readline_shell(
        r"
set enable-active-region on
set active-region-start-color \e[35m
set active-region-end-color \e[39m
set enable-keypad on
set enable-meta-key on
",
        |shell| {
            shell.session.send("echo ")?;
            shell.expect("echo ")?;
            shell.session.send("\x1b[200~REGION\x1b[201~")?;
            shell.expect("\x1b[35mREGION\x1b[39m")?;
            shell.session.send("X\r")?;
            shell.expect("REGIONX\r\n")?;
            shell.prompt()?;

            shell.run_and_expect("echo 123NEEDLE456", "123NEEDLE456")?;
            shell.session.send("\x12NEEDLE")?;
            shell.expect("\x1b[35mNEEDLE\x1b[39m")?;
            shell.session.send("\r")?;
            shell.expect("\x1b[?2004l")?;
            shell.expect("123NEEDLE456\r\n")?;
            shell.prompt()?;

            shell.command("bind 'set enable-active-region off'")?;
            shell.session.send("echo ")?;
            shell.expect("echo ")?;
            shell.session.send("\x1b[200~PLAIN\x1b[201~")?;
            let output = shell.capture("PLAIN")?;
            assert!(!output.contains("\x1b[35m"));
            shell.session.send("X\r")?;
            let output = shell.capture("PLAINX\r\n")?;
            assert!(!output.contains("\x1b[35m"));
            shell.prompt()?;

            shell.command("bind 'set echo-control-characters on'")?;
            shell.session.send("\x03")?;
            shell.expect("^C")?;
            shell.prompt()?;

            shell.command("bind 'set echo-control-characters off'")?;
            shell.session.send("\x03")?;
            let output = shell.capture(PROMPT)?;
            assert!(!output.contains("^C"));

            // For xterm the application keypad exit and Meta exit are distinct.
            // The following read enters keypad mode, with Meta disabled.
            shell.session.send("bind 'set enable-meta-key off'\r")?;
            shell.expect("\x1b[?1l\x1b>")?;
            let next = shell.capture(PROMPT)?;
            assert!(next.contains("\x1b[?1h\x1b="));
            assert!(!next.contains("\x1b[?1034h"));
            shell.session.send("bind 'set enable-meta-key on'\r")?;
            shell.expect("\x1b[?2004l")?;
            let next = shell.capture(PROMPT)?;
            if !shell.bash {
                assert!(next.contains("\x1b[?1034h"));
            }
            Ok(())
        },
    )
}

#[cfg(target_os = "linux")]
#[test]
fn readline_history_edits_match_bash() -> anyhow::Result<()> {
    for_readline_shell(
        "set mark-modified-lines on\nset revert-all-at-newline off\n",
        |shell| {
            shell.run_and_expect("echo PRIOR", "PRIOR")?;
            shell.run_and_expect("echo BASE", "BASE")?;

            shell.session.send("\x10X")?;
            shell.expect("*brush> echo BASEX")?;
            shell.session.send("\x10\x0e\r")?;
            shell.expect("\x1b[?2004l")?;
            shell.expect("BASEX\r\n")?;
            shell.prompt()?;

            shell.command("bind 'set revert-all-at-newline on'")?;
            shell.run_and_expect("echo ORIGINAL", "ORIGINAL")?;
            shell.session.send("\x10X")?;
            shell.expect("*brush> echo ORIGINALX")?;
            shell.session.send("\x01#\r")?;
            shell.expect("\x1b[?2004l")?;
            shell.prompt()?;

            // The newest entry is the comment. The entry before it is the
            // earlier command, which must have regained its original body.
            shell.session.send("\x10\x10\r")?;
            shell.expect("\x1b[?2004l")?;
            shell.expect("ORIGINAL\r\n")?;
            shell.prompt()?;
            Ok(())
        },
    )
}

#[cfg(target_os = "linux")]
#[test]
fn readline_history_arrows_keep_suggestions_separate() -> anyhow::Result<()> {
    const FRAME_END: &str = "\x1b[?2026l";
    const HINT: &str = "\x1b[2;3m";
    for_readline_shell_with_config(
        r#"
"\e[A": history-search-backward
"\e[B": history-search-forward
"\e[C": "\C-f"
"#,
        Some("[ui]\nsyntax-highlighting = true\n"),
        |shell| {
            shell.run_and_expect("echo sample", "sample")?;
            shell.run_and_expect("echo sample-extended", "sample-extended")?;
            if !shell.bash {
                shell.expect(FRAME_END)?;
            }
            for (key, text) in [("\x1b[A", "sample-extended"), ("\x1b[A", "sample")] {
                shell.session.send(key)?;
                if !shell.bash {
                    let frame = shell.capture(FRAME_END)?;
                    assert!(frame.contains("\x1b[1;32mecho"), "{frame:?}");
                    assert!(frame.contains(text), "{frame:?}");
                    assert!(!frame.contains(HINT), "{frame:?}");
                }
            }
            shell.session.send("\x1b[B\r")?;
            shell.expect("\x1b[?2004l")?;
            shell.expect("sample-extended\r\n")?;
            shell.prompt()?;
            if shell.bash {
                return Ok(());
            }
            shell.expect(FRAME_END)?;

            // Recall with the cursor still inside the command, then cancel.
            shell.session.send("echo sa\x1b[A")?;
            shell.expect("mple-extended")?;
            shell.expect(FRAME_END)?;
            shell.session.send("\x03")?;
            let output = shell.capture("^C\r\n")?;
            assert!(output.contains("mple-extended"), "{output:?}");
            assert!(!shell.capture(PROMPT)?.contains("mple-extended"));
            shell.expect(FRAME_END)?;

            // The same suffix is a suggestion when typing at the end of a draft.
            shell.session.send("echo sample")?;
            shell.expect(&format!("{HINT}-extended"))?;
            shell.expect(FRAME_END)?;
            shell.session.send("\x1b[D")?;
            let frame = shell.capture(FRAME_END)?;
            assert!(!frame.contains("-extended"), "{frame:?}");
            shell.session.send("\x1b[C")?;
            shell.expect(&format!("{HINT}-extended"))?;
            shell.expect(FRAME_END)?;
            shell.session.send("\x1b[C\r")?;
            shell.expect("\x1b[?2004l")?;
            shell.expect("sample-extended\r\n")?;
            shell.prompt()?;

            shell.command(
                r"printf '#!/bin/sh\nexit 0\n' > ./history-command; chmod u+x ./history-command",
            )?;
            shell.command("./history-command")?;
            shell.expect(FRAME_END)?;
            shell.session.send("./hi\x1b[A")?;
            shell.expect("\x1b[1;32mstory-command")?;
            shell.expect(FRAME_END)?;
            shell.session.send("\x03")?;
            shell.expect("^C\r\n")?;
            shell.prompt()
        },
    )
}

#[cfg(target_os = "linux")]
#[test]
fn history_suggestion_navigation_accepts_only_on_right() -> anyhow::Result<()> {
    const FRAME_END: &str = "\x1b[?2026l";
    const HINT: &str = "\x1b[2;3m";
    let omarchy_keys = r#"
"\e[A": history-search-backward
"\e[B": history-search-forward
"\e[C": "\C-f"
TAB: menu-complete
"#;
    for inputrc in [omarchy_keys, "TAB: menu-complete\n"] {
        for_readline_shell_with_config(
            inputrc,
            Some("[ui]\nsyntax-highlighting = true\nhistory-suggestion-navigation = true\n"),
            |shell| {
                if shell.bash {
                    return Ok(());
                }
                shell.run_and_expect("echo draft-older", "draft-older")?;
                shell.run_and_expect("echo draft-older-tail", "draft-older-tail")?;
                shell.run_and_expect("echo draft-newer", "draft-newer")?;
                shell.expect(FRAME_END)?;

                shell.session.send("echo dr")?;
                shell.expect(&format!("{HINT}aft-newer"))?;
                shell.expect(FRAME_END)?;
                for (key, suffix) in [("\x1b[A", "aft-older-tail"), ("\x1b[B", "aft-newer")] {
                    shell.session.send(key)?;
                    let frame = shell.capture(FRAME_END)?;
                    assert!(frame.contains(&format!("{HINT}{suffix}")), "{frame:?}");
                }
                shell.run_and_expect("", "dr")?;
                shell.expect(FRAME_END)?;

                shell.session.send("echo dra\x1b[A\x1b[A")?;
                shell.expect(&format!("{HINT}ft-older\x1b[0m"))?;
                shell.expect(FRAME_END)?;
                shell.session.send("\x1b[C")?;
                let frame = shell.capture(FRAME_END)?;
                assert!(frame.contains("draft-older"), "{frame:?}");
                assert!(!frame.contains("-tail"), "{frame:?}");
                shell.run_and_expect("", "draft-older")?;
                shell.expect(FRAME_END)?;

                // With an empty draft, Up still recalls and submits a history entry.
                shell.run_and_expect("\x1b[A", "draft-older")?;

                shell.command(": > uniquely-matching")?;
                shell.run_and_expect("echo uniquely-m\t", "uniquely-matching")
            },
        )?;
    }
    Ok(())
}

#[cfg(target_os = "linux")]
#[test]
fn history_suggestions_accept_and_highlight_through_the_next_word_boundary() -> anyhow::Result<()> {
    const FRAME_END: &str = "\x1b[?2026l";
    for_readline_shell_with_config(
        r#"
"\e[A": history-search-backward
"\e[C": "\C-f"
"#,
        Some("[ui]\nsyntax-highlighting = true\nhistory-suggestion-navigation = true\n"),
        |shell| {
            if shell.bash {
                return Ok(());
            }
            shell.run_and_expect("echo 'alpha beta' /tmp/older", "alpha beta /tmp/older")?;
            shell.run_and_expect("echo 'alpha beta' /tmp/newer", "alpha beta /tmp/newer")?;
            shell.expect(FRAME_END)?;

            shell.session.send("echo\x1b[A")?;
            shell.expect("\x1b[2;3m 'alpha beta' /tmp/older")?;
            shell.expect(FRAME_END)?;

            for (quoted, remaining) in [
                ("'", "alpha beta' /tmp/older"),
                ("'alpha ", "beta' /tmp/older"),
                ("'alpha beta'", "tmp/older"),
            ] {
                shell.session.send("\x1b[1;5C")?;
                let frame = shell.capture(FRAME_END)?;
                assert!(frame.contains("\x1b[1;32mecho\x1b[0m"), "{frame:?}");
                assert!(
                    frame.contains(&format!("\x1b[33m{quoted}\x1b[0m")),
                    "{frame:?}"
                );
                assert!(
                    frame.contains(&format!("\x1b[2;3m{remaining}\x1b[0m")),
                    "{frame:?}"
                );
            }

            shell.run_and_expect("\x1b[C", "alpha beta /tmp/older")?;
            shell.run_and_expect("echo sample\x01\x1b[1;5C\x1b[1;2CX", "Xample")
        },
    )
}

#[cfg(target_os = "linux")]
#[test]
fn history_suggestion_highlights_only_the_typed_prefix() -> anyhow::Result<()> {
    const FRAME_END: &str = "\x1b[?2026l";
    const BUILTIN_PREFIX: &str = "\x1b[1;32mec\x1b[0m";
    const HINT: &str = "\x1b[2;3m";
    for_readline_shell_with_config(
        r#"
"\e[A": history-search-backward
"\e[B": history-search-forward
"#,
        Some("[ui]\nsyntax-highlighting = true\nhistory-suggestion-navigation = true\n"),
        |shell| {
            if shell.bash {
                return Ok(());
            }
            shell.command(
                "PATH=./missing; history -s 'echo saved'; history -s 'ec-missing other'",
            )?;
            shell.expect(FRAME_END)?;

            for (keys, suffix, highlighted) in [
                ("ec", "-missing other", false),
                ("\x1b[A", "ho saved", true),
                ("\x1b[B", "-missing other", false),
                ("\x1b[A", "ho saved", true),
            ] {
                shell.session.send(keys)?;
                let frame = shell.capture(&format!("{HINT}{suffix}"))?;
                assert_eq!(frame.contains(BUILTIN_PREFIX), highlighted, "{frame:?}");
                shell.expect(FRAME_END)?;
            }

            shell.session.send("\r")?;
            let submitted = shell.capture("\x1b[?2004l")?;
            assert!(!submitted.contains(BUILTIN_PREFIX), "{submitted:?}");
            assert!(!submitted.contains(HINT), "{submitted:?}");
            shell.expect("command not found: ec")?;
            shell.prompt()
        },
    )
}

#[cfg(target_os = "linux")]
#[test]
fn forward_word_binding_partially_accepts_a_visible_suggestion() -> anyhow::Result<()> {
    const FRAME_END: &str = "\x1b[?2026l";
    for_readline_shell_with_config(
        r#"
$if mode=emacs
"\e[1;5C": forward-word
$endif
"#,
        Some("[ui]\nsyntax-highlighting = true\nhistory-suggestion-navigation = true\n"),
        |shell| {
            if shell.bash {
                return Ok(());
            }
            shell.run_and_expect("echo target alpha", "target alpha")?;
            shell.expect(FRAME_END)?;

            shell.session.send("echo tar")?;
            shell.expect("\x1b[2;3mget alpha")?;
            shell.expect(FRAME_END)?;
            shell.session.send("\x1b[1;5C")?;
            shell.expect("\x1b[2;3malpha")?;
            shell.expect(FRAME_END)?;
            shell.run_and_expect("", "target")?;

            // An ordinary forward-word binding still moves through typed text.
            shell.run_and_expect("echo one two\x01\x1b[1;5C X", "X one two")
        },
    )
}

#[cfg(target_os = "linux")]
#[test]
fn readline_terminal_special_keys_match_bash() -> anyhow::Result<()> {
    for (setting, result) in [("on", "A"), ("off", "BA")] {
        for_readline_shell(
            &format!("set bind-tty-special-chars {setting}\n"),
            |shell| {
                shell.command("stty erase '^T'")?;
                shell.run_and_expect("printf '%s\\n' AB\x14", result)
            },
        )?;
    }
    Ok(())
}

#[cfg(target_os = "linux")]
#[test]
fn readline_horizontal_scroll_and_bell_match_bash() -> anyhow::Result<()> {
    for_readline_shell("set horizontal-scroll-mode on\n", |shell| {
        let long = format!("{}ABCDEFGHIJKLMNOPQRSTUVWXYZ", "0123456789".repeat(12));
        shell.session.send(format!("echo {long}"))?;
        // A left overflow marker and the tail are both visible. The
        // submitted command still receives the complete input.
        shell.expect("<")?;
        shell.expect("UVWXYZ")?;
        shell.session.send("\r")?;
        shell.expect("\x1b[?2004l")?;
        shell.expect(&format!("{long}\r\n"))?;
        shell.prompt()?;

        shell.command("bind 'set horizontal-scroll-mode off' 'set blink-matching-paren on'")?;
        shell.session.send("echo (123")?;
        shell.expect("echo (123")?;
        shell.session.send(")")?;
        let cursor_moved = if shell.bash {
            ")\x08"
        } else {
            "echo \x1b7(123)"
        };
        shell.expect(cursor_moved)?;
        shell.clear_line()?;

        shell.command("bind 'set prefer-visible-bell on'")?;
        shell.session.send("echo no-such-file\t")?;
        shell.expect("\x1b[?5h")?;
        shell.expect("\x1b[?5l")?;
        shell.clear_line()?;

        shell.command("bind 'set prefer-visible-bell off'")?;
        shell.session.send("echo no-such-file\t")?;
        shell.expect("\x07")?;
        shell.clear_line()
    })
}

#[cfg(target_os = "linux")]
#[test]
fn readline_tab_lists_candidates_below_the_live_input() -> anyhow::Result<()> {
    const FRAME_END: &str = "\x1b[?2026l";
    for_readline_shell(
        "set show-all-if-ambiguous on\n\
         set show-all-if-unmodified on\n\
         set page-completions off\n\
         set completion-prefix-display-length 2\n\
         set menu-complete-display-prefix on\n\
         TAB: menu-complete\n",
        |shell| {
            shell.command("touch sample-alpha sample-beta")?;
            if shell.bash {
                shell.session.send("echo sam\t")?;
                shell.expect("...alpha")?;
                shell.session.send("\t")?;
            } else {
                shell.session.send("echo sam")?;
                shell.expect("echo sam")?;
                shell.expect(FRAME_END)?;

                shell.session.send("\t")?;
                let frame = shell.capture(FRAME_END)?;
                let buffer = frame.find("echo sample-").context("completed prefix")?;
                let below = frame.find("\x1b[2;1H").context("move below the input")?;
                let first = frame
                    .find("...alpha")
                    .context("candidate in the same redraw")?;
                let second = frame.find("...beta").context("second candidate")?;
                assert!(
                    buffer < below && below < first && first < second,
                    "{frame:?}"
                );

                shell.session.send("\t")?;
                let frame = shell.capture(FRAME_END)?;
                assert!(frame.contains("echo sample-alpha "), "{frame:?}");
                assert!(frame.contains("...alpha  ...beta"), "{frame:?}");
            }
            shell.run_and_expect("", "sample-alpha")
        },
    )
}

#[cfg(target_os = "linux")]
#[test]
fn readline_completion_list_overflow_is_still_visible() -> anyhow::Result<()> {
    for_readline_shell(
        "set completion-display-width 0\n\
         set page-completions off\n\
         set show-all-if-unmodified on\n",
        |shell| {
            shell.command("touch item-{00..29}")?;
            shell.session.send("echo item-\t")?;
            shell.expect("item-00\r\nitem-01")?;
            shell.expect("item-29")?;
            shell.run_and_expect("", "item-")
        },
    )
}

#[cfg(target_os = "linux")]
#[test]
fn readline_completion_display_width_affects_default_tab() -> anyhow::Result<()> {
    for_readline_shell("set completion-display-width 0\n", |shell| {
        shell.command("touch sample-a sample-b sample-c")?;
        shell.session.send("echo sample-\t\t")?;
        shell.expect("sample-a\r\nsample-b\r\nsample-c")?;
        shell.clear_line()
    })
}

#[cfg(target_os = "linux")]
#[test]
fn readline_shell_bindings_execute_after_round_trip() -> anyhow::Result<()> {
    for_readline_shell("", |shell| {
        shell.command(r#"bind -x '"\C-t": printf "\"%s\"\n" café'"#)?;
        shell.command(r#"definition=$(bind -X); bind -r '\C-t'; bind -x "$definition""#)?;
        shell.session.send("\x14")?;
        shell.expect("\"café\"\r\n")?;
        shell.resume_host_callback()
    })
}

#[cfg(target_os = "linux")]
struct ReadlinePty {
    session: ShellSession,
    bash: bool,
}

#[cfg(target_os = "linux")]
impl ReadlinePty {
    fn capture(&mut self, expected: &str) -> anyhow::Result<String> {
        let mut output = Vec::new();
        loop {
            let found = self
                .session
                .expect(expectrl::Any([expected, DSR_QUERY]))
                .with_context(|| format!("bash={}, waiting for {expected:?}", self.bash))?;
            output.extend_from_slice(found.before());
            if found.get(0) == Some(DSR_QUERY.as_bytes()) {
                self.session.send(DSR_REPLY)?;
            } else {
                return Ok(String::from_utf8_lossy(&output).into_owned());
            }
        }
    }

    fn expect(&mut self, expected: &str) -> anyhow::Result<()> {
        self.capture(expected).map(drop)
    }

    fn prompt(&mut self) -> anyhow::Result<()> {
        self.expect(PROMPT)
    }

    fn run_and_expect(&mut self, command: &str, output: &str) -> anyhow::Result<()> {
        self.session.send(command)?;
        self.session.send("\r")?;
        self.expect("\x1b[?2004l")?;
        self.expect(&format!("{output}\r\n"))?;
        self.prompt()
    }

    fn resume_host_callback(&mut self) -> anyhow::Result<()> {
        if !self.bash {
            self.prompt()?;
        }
        Ok(())
    }

    fn command(&mut self, command: &str) -> anyhow::Result<()> {
        self.session.send(command)?;
        self.session.send("\r")?;
        // A redraw can still paint PS1 while the command is being typed.
        // Bracketed-paste is disabled on leaving the editor for execution.
        self.expect("\x1b[?2004l")?;
        self.prompt()
    }

    fn clear_line(&mut self) -> anyhow::Result<()> {
        self.session.send("\x01\x0b\r")?;
        self.expect("\x1b[?2004l")?;
        self.prompt()
    }
}

#[cfg(target_os = "linux")]
fn for_readline_shell(
    inputrc: &str,
    check: impl FnMut(&mut ReadlinePty) -> anyhow::Result<()>,
) -> anyhow::Result<()> {
    for_readline_shell_with_config(inputrc, None, check)
}

#[cfg(target_os = "linux")]
fn for_readline_shell_with_config(
    inputrc: &str,
    brush_config: Option<&str>,
    mut check: impl FnMut(&mut ReadlinePty) -> anyhow::Result<()>,
) -> anyhow::Result<()> {
    let temp = tempfile::tempdir()?;
    let startup = temp.path().join("inputrc");
    std::fs::write(&startup, inputrc)?;
    let config = temp.path().join("brush.toml");
    if let Some(contents) = brush_config {
        std::fs::write(&config, contents)?;
    }
    for bash in std::iter::once(None).chain(std::env::var_os("BASH_PATH").map(Some)) {
        let using_bash = bash.is_some();
        let mut cmd = if let Some(bash) = bash {
            // expectrl starts the slave PTY with ECHO disabled. Bash reads
            // that setting before Readline decides whether to redisplay.
            let mut cmd = std::process::Command::new("sh");
            cmd.args([
                "-c",
                "stty echo; export PS1=\"$1\"; shift; exec \"$@\"",
                "readline-oracle",
            ])
            .arg(PROMPT)
            .arg(bash)
            .args(["--norc", "--noprofile", "-i"]);
            cmd
        } else {
            let mut cmd = std::process::Command::new(assert_cmd::cargo::cargo_bin!("brush"));
            cmd.args(["--norc", "--noprofile", "--input-backend=reedline"]);
            if brush_config.is_some() {
                cmd.arg("--config").arg(&config);
            } else {
                cmd.args(["--no-config", "--disable-color"]);
            }
            cmd
        };
        cmd.env("INPUTRC", &startup)
            .env("HOME", temp.path())
            .env("HISTFILE", "")
            .env("PROMPT_COMMAND", "")
            .env("LC_ALL", "C.UTF-8")
            .env("PS1", PROMPT)
            .env("TERM", "xterm-256color")
            .env_remove("NO_COLOR")
            .env_remove("BASH_ENV")
            .current_dir(temp.path());
        let session = expectrl::Session::spawn(cmd)?;
        let mut session = expectrl::session::log(session, std::io::stdout())?;
        session.set_expect_timeout(Some(Duration::from_secs(5)));
        session.get_process_mut().set_window_size(80, 24)?;
        let mut shell = ReadlinePty {
            session,
            bash: using_bash,
        };
        shell.prompt()?;
        check(&mut shell).with_context(|| format!("terminal scenario, bash={using_bash}"))?;
        shell.session.send("exit\r")?;
        shell.session.expect(expectrl::Eof)?;
    }
    Ok(())
}

//
// Helpers
//

type ShellSession = Session<UnixProcess, LogStream<PtyStream, std::io::Stdout>>;

#[cfg(target_os = "linux")]
fn expect_with_terminal_answers(session: &mut ShellSession, expected: &str) -> anyhow::Result<()> {
    loop {
        let found = session.expect(expectrl::Any([expected, DSR_QUERY]))?;
        if found.get(0) == Some(DSR_QUERY.as_bytes()) {
            session.send(DSR_REPLY)?;
        } else {
            return Ok(());
        }
    }
}

/// Waits for the cursor-position query that precedes a prompt paint, answers
/// it, and then waits for the prompt. The first `withhold` queries are left
/// unanswered instead, so the shell has to re-issue them.
fn expect_next_prompt(session: &mut ShellSession, mut withhold: usize) -> anyhow::Result<()> {
    loop {
        session
            .expect(DSR_QUERY)
            .context("no cursor-position query before prompt")?;
        if withhold > 0 {
            withhold -= 1;
            continue;
        }
        session.send(DSR_REPLY)?;
        session
            .expect(PROMPT)
            .context("no prompt after answered query")?;
        return Ok(());
    }
}

fn start_reedline_session() -> anyhow::Result<ShellSession> {
    start_reedline_session_with(|_| {})
}

fn start_reedline_session_with(
    configure: impl FnOnce(&mut std::process::Command),
) -> anyhow::Result<ShellSession> {
    let shell_path = assert_cmd::cargo::cargo_bin!("brush");

    let mut cmd = std::process::Command::new(shell_path);
    cmd.args([
        "--norc",
        "--noprofile",
        "--no-config",
        "--disable-bracketed-paste",
        "--disable-color",
        "--input-backend=reedline",
    ]);
    cmd.env("PS1", PROMPT);
    cmd.env("TERM", "xterm-256color");
    configure(&mut cmd);

    let session = expectrl::session::Session::spawn(cmd)?;

    // N.B. Replace with `session` directly to disable logging of the session.
    let mut session = expectrl::session::log(session, std::io::stdout())?;

    // The timeout tests deliberately let ~2s crossterm timeouts elapse,
    // several in a row when the terminal never answers.
    session.set_expect_timeout(Some(Duration::from_secs(15)));

    Ok(session)
}
