//! Interactive integration tests for brush shell

// Only compile this for platforms supported by expectrl's pty backend.
#![cfg(any(
    target_os = "linux",
    target_os = "android",
    target_os = "macos",
    target_os = "freebsd"
))]
#![cfg(test)]
#![allow(clippy::panic_in_result_fn)]

use anyhow::Context;
use std::os::unix::process::CommandExt as _;

use expectrl::{
    Expect, Session,
    process::unix::{PtyStream, Signal, UnixProcess, WaitStatus},
    repl::ReplSession,
    stream::log::LogStream,
};

#[test_with::executable(ping)]
#[test]
fn run_suspend_and_fg() -> anyhow::Result<()> {
    let mut session = start_shell_session()?;

    // Ping localhost in a loop; wait for at least one response.
    session.expect_prompt()?;
    session.send_line("ping -c 1000000 127.0.0.1")?;
    session
        .expect("bytes from")
        .context("Initial ping invocation output")?;

    // Suspend and resume a handful of times to make sure it pauses and
    // resumes reliably.
    for _ in 0..5 {
        // Suspend.
        session.suspend()?;
        session.expect_prompt()?;

        // Run `jobs` to see the suspended job.
        let jobs_output = session.exec_output("jobs")?;
        assert!(jobs_output.contains("ping"));

        // Bring the job to the foreground.
        session.send_line("fg")?;
        session.expect("ping").context("Foregrounded ping")?;
    }

    // Ctrl+C to cancel the ping.
    session.interrupt()?;
    session.expect("loss")?;
    session.expect_prompt()?;

    // Exit the shell.
    session.exit()?;

    Ok(())
}

#[test_with::executable(ping)]
#[test]
fn run_in_bg_then_fg() -> anyhow::Result<()> {
    let mut session = start_shell_session()?;

    // Ping localhost in a loop; wait for at least one response.
    session.expect_prompt()?;
    session.send_line("ping -c 1000000 127.0.0.1")?;
    session.expect("bytes from")?;

    // Suspend and send to background.
    session.suspend()?;
    session.expect_prompt()?;

    // Run `jobs` to see the suspended job.
    let jobs_output = session.exec_output("jobs")?;
    assert!(jobs_output.contains("ping"));

    // Send the job to the background.
    session.send_line("bg")?;
    session.expect_prompt()?;

    // Make sure ping is still running asynchronously.
    session.expect("bytes from")?;

    // Kill the job; make sure it's done.
    session.send_line("kill %1")?;
    session.expect_prompt()?;
    session.send_line("wait")?;
    session.expect_prompt()?;

    // Make sure the jobs are gone.
    let jobs_output = session.exec_output("jobs")?;
    assert_eq!(jobs_output.trim(), "");

    // Exit the shell.
    session.exit()?;

    Ok(())
}

#[test_with::executable(sh, sleep)]
#[test]
fn resume_suspended_pipeline_in_foreground() -> anyhow::Result<()> {
    let mut session = start_shell_session()?;
    session.expect_prompt()?;

    // `fg` must continue every process of the pipeline, and give the terminal to their group,
    // or Ctrl+C can't end the pipeline.
    session.send_line("sleep 30 | sh -c 'echo ready-$((1 + 1)); exec sleep 31'")?;
    session.expect("ready-2")?;
    session.suspend()?;
    session
        .expect_prompt()
        .context("Prompt after suspending pipeline")?;

    // (`fg` shows the job's command once it has continued it.)
    session.send_line("fg")?;
    session.expect("exec sleep 31")?;
    session.interrupt()?;
    session
        .expect_prompt()
        .context("Prompt after interrupting the resumed pipeline")?;

    let jobs_output = session.exec_output("jobs")?;
    assert_eq!(jobs_output.trim(), "", "the pipeline should be gone");

    session.exit()?;

    Ok(())
}

#[test_with::executable(sh, sleep)]
#[test]
fn resume_suspended_pipeline_after_its_leader_exits() -> anyhow::Result<()> {
    let mut session = start_shell_session()?;
    session.expect_prompt()?;

    // The pipeline's first program leads its process group, which outlives it: `fg` must give
    // the terminal to the group, not to the first program left. (The second program waits for
    // the first to exit before it's ready.)
    session.send_line("sh -c 'exit 0' | sh -c 'cat; echo ready-$((1 + 1)); exec sleep 32'")?;
    session.expect("ready-2")?;
    session.suspend()?;
    session
        .expect_prompt()
        .context("Prompt after suspending pipeline")?;

    // (`fg` shows the job's command once it has continued it.)
    session.send_line("fg")?;
    session.expect("exec sleep 32")?;
    session.interrupt()?;
    session
        .expect_prompt()
        .context("Prompt after interrupting the resumed pipeline")?;

    let jobs_output = session.exec_output("jobs")?;
    assert_eq!(jobs_output.trim(), "", "the pipeline should be gone");

    session.exit()?;

    Ok(())
}

#[test_with::executable(sh, sleep, pgrep)]
#[test]
fn kill_suspended_pipeline() -> anyhow::Result<()> {
    let mut session = start_shell_session()?;
    session.expect_prompt()?;

    session.send_line("sleep 1017 | sh -c 'echo ready-$((1 + 1)); exec sleep 1018'")?;
    session.expect("ready-2")?;
    session.suspend()?;
    session
        .expect_prompt()
        .context("Prompt after suspending pipeline")?;

    // Killing the job kills all of its pipeline's processes.
    session.send_line("kill -9 %1; wait")?;
    session.expect_prompt()?;
    let jobs_output = session.exec_output("jobs")?;
    assert_eq!(jobs_output.trim(), "", "the job should be gone");
    assert_no_process_left(&mut session, "^sleep 101[78]$")?;

    session.exit()?;

    Ok(())
}

#[test_with::executable(sh, sleep, pgrep)]
#[test]
fn terminate_suspended_pipeline() -> anyhow::Result<()> {
    let mut session = start_shell_session()?;
    session.expect_prompt()?;

    session.send_line("sleep 1027 | sh -c 'echo ready-$((1 + 1)); exec sleep 1028'")?;
    session.expect("ready-2")?;
    session.suspend()?;
    session
        .expect_prompt()
        .context("Prompt after suspending pipeline")?;

    // A stopped job sent a request to terminate is continued, so that it acts on it.
    session.send_line("kill -TERM %1; wait")?;
    session.expect_prompt()?;
    let jobs_output = session.exec_output("jobs")?;
    assert_eq!(jobs_output.trim(), "", "the job should be gone");
    assert_no_process_left(&mut session, "^sleep 102[78]$")?;

    session.exit()?;

    Ok(())
}

#[test_with::executable(sh, head, sort, sleep, ps)]
#[test]
fn resume_pipeline_stopped_in_background() -> anyhow::Result<()> {
    let mut session = start_shell_session()?;
    session.expect_prompt()?;
    let shell_pid = session.exec_output("echo $$")?.trim().parse()?;

    // `fg` must give a job stopped in the background the terminal before it continues it, or
    // the job would stop again on its first read from the terminal. `sleep` keeps `head` from
    // reading until `fg`: a background read of the terminal on macOS can fail with EIO instead
    // of stopping the reader (Linux stops it with SIGTTIN), so a reader that starts while the
    // job is backgrounded can exit before there is a job left to resume.
    session.send_line("sh -c 'echo ready-$((1 + 1)) >&2; sleep 3; exec head -n 2' | sort")?;
    session.expect("ready-2")?;
    wait_for_state(shell_pid, "sleep", ProcessState::Sleeping)?;
    session.suspend()?;
    session
        .expect_prompt()
        .context("Prompt after suspending pipeline")?;

    // Resume it in the background and stop it again while it sleeps, so `fg` resumes a job
    // stopped in the background. (`jobs` still reports it running until the shell notices.)
    session.send_line("bg; kill -STOP %1")?;
    session.expect_prompt()?;
    wait_for_state(shell_pid, "sleep", ProcessState::Stopped)?;

    // (`fg` shows the job's command once it has continued it.)
    session.send_line("fg")?;
    session.expect("exec head -n 2")?;
    session.send("zebra\napple\n")?;
    session
        .expect(expectrl::Regex("apple\\s+zebra"))
        .context("sorted input after resuming the pipeline")?;
    session.expect_prompt()?;

    session.exit()?;

    Ok(())
}

#[test_with::executable(sh, seq, sed, less, stty, ps)]
#[test]
fn resume_pager_of_suspended_program() -> anyhow::Result<()> {
    let mut session = start_shell_session()?;
    session.expect_prompt()?;
    let shell_pid = session.exec_output("echo $$")?.trim().parse()?;
    let initial_modes = terminal_modes(&mut session)?;

    // A program that runs a pager (as `git log` does) is stopped along with it, and its other
    // processes, which `fg` must all continue, giving them the terminal.
    //
    // (The pager acts on Ctrl+Z only between reads from the terminal, by putting the terminal
    // back and stopping itself, so the test waits for it to be reading before suspending it,
    // and for it to have stopped before typing again.)
    session.send_line("sh -c 'seq 1 100 | sed s/^/line-/ | less'")?;
    session.expect("line-1")?;
    wait_for_state(shell_pid, "less", ProcessState::Sleeping)?;
    session.suspend()?;
    session
        .expect_prompt()
        .context("Prompt after suspending the pager")?;
    wait_for_state(shell_pid, "less", ProcessState::Stopped)?;

    // The pager redraws and reads keys from the terminal once continued.
    session.send_line("fg")?;
    session.expect("line-1").context("pager redrawn")?;
    session.send("G")?;
    session.expect("line-100").context("pager's last line")?;

    // And it can be suspended and continued again.
    wait_for_state(shell_pid, "less", ProcessState::Sleeping)?;
    session.suspend()?;
    session
        .expect_prompt()
        .context("Prompt after suspending the pager again")?;
    wait_for_state(shell_pid, "less", ProcessState::Stopped)?;
    session.send_line("fg")?;
    session.expect("line-100").context("pager redrawn again")?;
    session.send("q")?;
    session
        .expect_prompt()
        .context("Prompt after quitting the pager")?;

    // The pager leaves the terminal as it found it.
    assert_eq!(terminal_modes(&mut session)?, initial_modes);

    session.exit()?;

    Ok(())
}

#[test_with::executable(less)]
#[test]
fn run_pipeline_interactively() -> anyhow::Result<()> {
    let mut session = start_shell_session()?;

    // Run a pipeline interactively.
    session.expect_prompt()?;
    session.send_line("echo hello | TERM=linux less")?;
    session
        .expect("hello")
        .context("Echoed text didn't show up")?;
    session.send("h")?;
    session
        .expect("SUMMARY")
        .context("less help didn't show up")?;
    session.send("q")?;
    session.send("q")?;
    session
        .expect_prompt()
        .context("Final prompt didn't show up")?;

    // Exit the shell.
    session.exit()?;

    Ok(())
}

#[test]
fn login_shell_via_argv0_shows_prompt() -> anyhow::Result<()> {
    let mut session = start_shell_session_with(|cmd| {
        cmd.arg("--norc").arg0("-brush");
    })?;

    session.expect_prompt()?;
    let login_shell_output = session.exec_output("shopt -q login_shell && echo login")?;
    assert!(login_shell_output.contains("login"));

    session.exit()?;

    Ok(())
}

#[test]
fn dash_s_with_positional_args_is_interactive() -> anyhow::Result<()> {
    // With `-s`, trailing words are positional parameters and commands still come from
    // stdin -- so at a terminal the shell is interactive, and `PROMPT_COMMAND` and the
    // zsh-style hooks (which are gated on that) run.
    let mut session = start_shell_session_with(|cmd| {
        cmd.args(["--norc", "-s", "myarg"]);
    })?;

    session.expect_prompt()?;

    // N.B. The markers are computed, so the echoed command line can't satisfy the assertion.
    let output = session.exec_output(r#"echo "FLAGS[${-//[!i]/}] ARG[$1]""#)?;
    assert!(
        output.contains("FLAGS[i] ARG[myarg]"),
        "expected an interactive shell with $1 set; got: {output}"
    );

    session.exec_output("PROMPT_COMMAND='echo PC-RAN'")?;
    let output = session.exec_output("echo done")?;
    assert!(
        output.contains("PC-RAN"),
        "PROMPT_COMMAND didn't run: {output}"
    );

    session.exit()?;

    Ok(())
}

#[test]
fn zsh_style_hook_state_is_visible_to_rc_files() -> anyhow::Result<()> {
    // The generic YAML harness always passes --norc, so this ordering check must use a PTY
    // session that can load a dedicated rc file.
    let rc_file = temp_script(
        r#"echo "RC-GUARD[${bash_preexec_imported:-unset}] RC-ARRAYS[${precmd_functions[*]:-unset}|${preexec_functions[*]:-unset}]"
rc_precmd() { echo RC-PRECMD; }
precmd_functions+=(rc_precmd)
"#,
    )?;

    let mut session = start_shell_session_with(|cmd| {
        cmd.arg("--enable-zsh-hooks")
            .arg("--rcfile")
            .arg(rc_file.path());
    })?;

    session.expect("RC-GUARD[defined] RC-ARRAYS[precmd|preexec]")?;
    session.expect("RC-PRECMD")?;
    session.expect_prompt()?;

    let output = session.exec_output("echo trigger")?;
    assert!(output.contains("RC-PRECMD"));

    session.exit()?;

    Ok(())
}

/// A key bound with `bind -x` runs its command, which sees the line being edited.
#[test]
fn runs_commands_bound_to_keys() -> anyhow::Result<()> {
    let mut session = start_shell_session()?;
    session.expect_prompt()?;

    session.send_line(r#"bind -x '"\C-x": echo "bound:[$READLINE_LINE]"'"#)?;
    session.expect_prompt()?;

    // Type a line, and press Ctrl-X.
    session.send("abc\x18")?;
    session
        .expect("bound:[abc]")
        .context("bound command didn't run with the line")?;

    // The line comes back afterward; clear it with Ctrl-U before exiting.
    session.send("\x15")?;
    session.exit()?;

    Ok(())
}

/// Enter sends Ctrl-M, which the terminal passes on as is, so a key binding can
/// tell it from Ctrl-J (which is what `send_line` sends).
#[test]
fn tells_enter_from_ctrl_j() -> anyhow::Result<()> {
    let mut session = start_shell_session()?;
    session.expect_prompt()?;

    session.send_line(r#"bind -x '"\C-m": echo "enter:[$READLINE_LINE]"'"#)?;
    session.expect_prompt()?;

    session.send("abc\r")?;
    session
        .expect("enter:[abc]")
        .context("Enter didn't run the command bound to Ctrl-M")?;

    // The line comes back afterward; clear it with Ctrl-U before exiting.
    session.send("\x15")?;
    session.exit()?;

    Ok(())
}

/// Terminal shell integration brackets each command with a matched pair of OSC 633 markers:
/// `E`/`C` before it runs, `D` after. A `preexec` hook that exits the shell means the command
/// never runs, so neither marker may be emitted -- a lone `D` desynchronizes a terminal's
/// command tracking.
#[test]
fn osc_command_markers_stay_paired_when_preexec_exits() -> anyhow::Result<()> {
    const COMMAND_STARTED: &str = "\x1b]633;C";
    const COMMAND_FINISHED: &str = "\x1b]633;D";

    let mut session = start_shell_session_with(|cmd| {
        cmd.args([
            "--norc",
            "--enable-zsh-hooks",
            "--enable-terminal-integration",
        ]);
        // OSC 633 is emitted only for terminals known to understand it.
        cmd.env("TERM_PROGRAM", "vscode");
    })?;
    session.expect_prompt()?;

    // A command that does run gets the full pair. (`preexec` isn't defined yet when this line
    // is dispatched, so the hook doesn't fire for it.)
    session.send_line("preexec() { case $1 in stop) exit 5;; esac; }")?;
    session
        .expect(COMMAND_STARTED)
        .context("no command-started marker for a command that ran")?;
    session
        .expect(COMMAND_FINISHED)
        .context("no command-finished marker for a command that ran")?;
    session.expect_prompt()?;

    // The hook exits before this line runs, so it gets neither marker.
    session.send_line("stop")?;
    // `Eof` matches the whole remaining buffer, so the bytes seen since the last prompt are
    // the match itself; `before()` would be empty.
    let captures = session
        .expect(expectrl::Eof)
        .context("shell did not exit")?;
    let tail = String::from_utf8_lossy(captures.as_bytes()).into_owned();

    assert!(
        !tail.contains(COMMAND_STARTED) && !tail.contains(COMMAND_FINISHED),
        "a command that never ran emitted markers: {tail:?}"
    );

    Ok(())
}

//
// Helpers
//

#[test_with::executable(sh, sleep)]
#[test]
fn suspend_program_while_background_jobs_wait() -> anyhow::Result<()> {
    let mut session = start_shell_session()?;
    session.expect_prompt()?;

    // Background jobs wait for programs of their own while the foreground program runs; the
    // foreground program's stop is for its own wait alone. (Which wait hears of it first
    // varies, so more background jobs make for a likelier failure without the fix.)
    session.send_line("for i in {1..16}; do { sleep 5; } & done")?;
    session.expect_prompt()?;
    session.send_line("sh -c 'echo ready-$((1 + 1)); exec sleep 1061'")?;
    session.expect("ready-2")?;
    session.suspend()?;
    session
        .expect_prompt()
        .context("Prompt after suspending the program")?;

    let jobs_output = session.exec_output("jobs")?;
    assert!(
        jobs_output
            .lines()
            .any(|line| line.contains("Stopped") && line.contains("1061")),
        "unexpected jobs: {jobs_output}"
    );

    session.send_line("kill -9 %+")?;
    session.expect_prompt()?;

    session.exit()?;

    Ok(())
}

#[test_with::executable(sleep)]
#[test]
fn stop_signal_to_shell_leaves_background_jobs_alone() -> anyhow::Result<()> {
    let mut session = start_shell_session()?;
    session.expect_prompt()?;

    // A stop signal that reaches the shell stops none of the programs that background jobs are
    // waiting for, so none of them is taken to be stopped.
    session.send_line("{ sleep 3; echo bg-$((1 + 1)); } &")?;
    session.expect_prompt()?;
    // (The background job's program has started by the time the signal is sent.)
    let output =
        session.exec_output("sleep 0.5; kill -TSTP $$; sleep 1; echo marker-$((1 + 2))")?;
    assert!(output.contains("marker-3"), "unexpected output: {output}");
    assert!(
        !output.contains("Stopped") && !output.contains("bg-2"),
        "unexpected output: {output}"
    );

    session.exit()?;

    Ok(())
}

/// Starts a shell with a private `HOME`/`HISTFILE`, optionally using a config file and an
/// explicit input backend. Unlike the shared helpers, this omits `--no-config` when a config is
/// given, so history settings can be exercised end to end.
fn start_history_session(
    backend: Option<&str>,
    config: Option<&std::path::Path>,
    home: &std::path::Path,
) -> anyhow::Result<RawShellSession> {
    let shell_path = assert_cmd::cargo::cargo_bin!("brush");

    let mut cmd = std::process::Command::new(shell_path);
    cmd.args([
        "--noprofile",
        "--norc",
        "--disable-bracketed-paste",
        "--disable-color",
    ]);
    if let Some(config) = config {
        cmd.arg("--config").arg(config);
    } else {
        cmd.arg("--no-config");
    }
    if let Some(backend) = backend {
        cmd.arg(format!("--input-backend={backend}"));
    }
    let inputrc = home.join("inputrc");
    std::fs::write(&inputrc, "")?;
    cmd.env("PS1", "hist> ");
    cmd.env("TERM", "xterm-256color");
    cmd.env("HOME", home);
    cmd.env("HISTFILE", home.join("history"));
    cmd.env("INPUTRC", inputrc);

    let session = expectrl::session::Session::spawn(cmd)?;
    let session = expectrl::repl::ReplSession::new(session, "hist> ");

    Ok(session)
}

fn read_history_file(home: &std::path::Path) -> String {
    std::fs::read_to_string(home.join("history")).unwrap_or_default()
}

/// Closing a terminal hangs the shell up. The commands this session already recorded must not
/// disappear with it, whether the default (reedline) backend or the minimal one is reading.
#[test]
fn history_survives_hangup() -> anyhow::Result<()> {
    for backend in [None, Some("minimal"), Some("basic")] {
        let home = tempfile::tempdir()?;
        let mut session = start_history_session(backend, None, home.path())?;
        session.expect_prompt()?;

        session.send_line("echo needle-in-history")?;
        session.expect("needle-in-history")?;
        session.expect_prompt()?;

        session
            .get_session_mut()
            .get_process_mut()
            .kill(Signal::SIGHUP)
            .context("sending SIGHUP")?;

        // Wait for the process itself; the pty master can stay open while a reader thread
        // drains it, so EOF is not a reliable sign that the shell has finished saving. Poll
        // `status` rather than `is_alive`, which reaps the child and discards its status.
        let deadline = std::time::Instant::now() + std::time::Duration::from_secs(5);
        let mut status = WaitStatus::StillAlive;
        while matches!(status, WaitStatus::StillAlive) && std::time::Instant::now() < deadline {
            status = session.get_session().get_process().status()?;
            if matches!(status, WaitStatus::StillAlive) {
                std::thread::sleep(std::time::Duration::from_millis(50));
            }
        }
        assert!(
            matches!(status, WaitStatus::Exited(_, 129)),
            "unexpected exit status after SIGHUP (backend {backend:?}): {status:?}"
        );

        let contents = read_history_file(home.path());
        assert!(
            contents.contains("echo needle-in-history"),
            "history lost on SIGHUP (backend {backend:?}): {contents:?}"
        );
    }

    Ok(())
}

/// With immediate appends enabled, a command is on disk before the shell can be killed without
/// running its normal exit path.
#[test]
fn append_immediately_persists_without_clean_exit() -> anyhow::Result<()> {
    let home = tempfile::tempdir()?;
    let config = home.path().join("config.toml");
    std::fs::write(&config, "[history]\nappend-immediately = true\n")?;

    let mut session = start_history_session(Some("basic"), Some(&config), home.path())?;
    session.expect_prompt()?;
    session.send_line("echo persisted")?;
    session.expect_prompt()?;

    // No `exit`: the session's normal end-of-loop save never runs.
    session
        .get_session_mut()
        .get_process_mut()
        .kill(Signal::SIGKILL)
        .context("sending SIGKILL")?;
    let deadline = std::time::Instant::now() + std::time::Duration::from_secs(5);
    while session
        .get_session()
        .get_process()
        .is_alive()
        .unwrap_or(false)
        && std::time::Instant::now() < deadline
    {
        std::thread::sleep(std::time::Duration::from_millis(50));
    }

    let contents = read_history_file(home.path());
    assert!(
        contents.contains("echo persisted"),
        "command was not appended before the session was killed: {contents:?}"
    );

    Ok(())
}

/// `set +o history` stops automatic recording; `set -o history` starts it again. Explicit
/// `history -s` keeps working while it is off.
#[test]
fn set_plus_o_history_toggles_recording() -> anyhow::Result<()> {
    let home = tempfile::tempdir()?;
    let mut session = start_history_session(Some("basic"), None, home.path())?;
    session.expect_prompt()?;

    session.send_line(": before")?;
    session.expect_prompt()?;
    session.send_line("set +o history")?;
    session.expect_prompt()?;
    session.send_line(": hidden")?;
    session.expect_prompt()?;
    session.send_line("history -s explicit-entry")?;
    session.expect_prompt()?;
    session.send_line("set -o history")?;
    session.expect_prompt()?;
    session.send_line(": after")?;
    session.expect_prompt()?;

    let output = String::from_utf8(session.execute("history")?)?;
    assert!(output.contains(": before"), "history: {output}");
    assert!(output.contains(": after"), "history: {output}");
    assert!(output.contains("explicit-entry"), "history: {output}");
    assert!(!output.contains(": hidden"), "history: {output}");

    session.exit()?;

    Ok(())
}

/// A foreground program receives the hangup too, so the shell stops waiting for it and can save
/// the command it had already recorded.
#[test]
fn hangup_stops_foreground_job_and_saves_history() -> anyhow::Result<()> {
    let home = tempfile::tempdir()?;
    let mut session = start_history_session(None, None, home.path())?;
    session.expect_prompt()?;

    session.send_line("sleep 30")?;
    std::thread::sleep(std::time::Duration::from_millis(300));

    session
        .get_session_mut()
        .get_process_mut()
        .kill(Signal::SIGHUP)
        .context("sending SIGHUP")?;

    let deadline = std::time::Instant::now() + std::time::Duration::from_secs(5);
    while session
        .get_session()
        .get_process()
        .is_alive()
        .unwrap_or(false)
        && std::time::Instant::now() < deadline
    {
        std::thread::sleep(std::time::Duration::from_millis(50));
    }
    anyhow::ensure!(
        !session
            .get_session()
            .get_process()
            .is_alive()
            .unwrap_or(false),
        "shell kept waiting for a foreground job after SIGHUP"
    );

    let contents = read_history_file(home.path());
    assert!(
        contents.contains("sleep 30"),
        "history lost while a foreground job was running: {contents:?}"
    );

    Ok(())
}

/// The normal on-exit path (an EXIT trap included) still runs after a hangup.
#[test]
fn hangup_runs_exit_trap() -> anyhow::Result<()> {
    let home = tempfile::tempdir()?;
    let mut session = start_history_session(None, None, home.path())?;
    session.expect_prompt()?;

    let trap_output = home.path().join("exit-trap");
    session.send_line(format!(
        "trap 'echo EXIT-TRAP > {}' EXIT; echo TRAP-SET",
        brush_core::escape::single_quote(trap_output.to_str().context("trap path")?)
    ))?;
    session.expect("TRAP-SET")?;
    session.expect_prompt()?;
    session.send_line("echo needle")?;
    session.expect_prompt()?;

    session
        .get_session_mut()
        .get_process_mut()
        .kill(Signal::SIGHUP)
        .context("sending SIGHUP")?;

    let deadline = std::time::Instant::now() + std::time::Duration::from_secs(10);
    while !trap_output.exists() && std::time::Instant::now() < deadline {
        std::thread::sleep(std::time::Duration::from_millis(50));
    }
    assert!(trap_output.exists(), "EXIT trap did not run after SIGHUP");
    assert_eq!(std::fs::read_to_string(&trap_output)?, "EXIT-TRAP\n");

    Ok(())
}

/// Share mode imports entries another session appended before drawing the next prompt.
#[test]
fn share_imports_other_sessions_commands() -> anyhow::Result<()> {
    let home = tempfile::tempdir()?;
    let config = home.path().join("config.toml");
    std::fs::write(&config, "[history]\nshare = true\n")?;

    let mut first =
        start_history_session(Some("basic"), Some(&config), home.path()).context("first shell")?;
    let mut second =
        start_history_session(Some("basic"), Some(&config), home.path()).context("second shell")?;
    first.set_echo(true);
    second.set_echo(true);
    first.expect_prompt().context("first prompt")?;
    second.expect_prompt().context("second prompt")?;

    first.send_line("echo duplicate-entry")?;
    first.expect("duplicate-entry")?;
    first.expect_prompt().context("first command")?;
    first.send_line("echo duplicate-entry")?;
    first.expect("duplicate-entry")?;
    first.expect_prompt().context("first repeat")?;

    // Move the second shell through a prompt so share mode imports both executions.
    second.send_line(": share-sync")?;
    second.expect_prompt().context("second sync")?;

    second.send_line("history > \"$HOME/shared-history.txt\"; echo SHARE-DONE")?;
    second.expect("SHARE-DONE")?;
    second.expect_prompt().context("second command")?;

    let seen = std::fs::read_to_string(home.path().join("shared-history.txt"))
        .context("read shared history output")?;
    assert_eq!(
        seen.matches("echo duplicate-entry").count(),
        2,
        "share collapsed a legitimate duplicate: {seen:?}"
    );

    // Advancing again must not import the same entries a second time.
    second.send_line(": share-sync-again")?;
    second.expect_prompt().context("second sync again")?;
    second.send_line("history > \"$HOME/shared-history-again.txt\"; echo SHARE-DONE")?;
    second.expect("SHARE-DONE")?;
    second.expect_prompt().context("second command again")?;
    let seen_again = std::fs::read_to_string(home.path().join("shared-history-again.txt"))
        .context("read second shared history output")?;
    assert_eq!(
        seen_again.matches("echo duplicate-entry").count(),
        2,
        "share re-imported the same entries: {seen_again:?}"
    );
    assert_eq!(
        seen_again
            .lines()
            .filter(|line| line.ends_with(": share-sync"))
            .count(),
        1,
        "share re-imported its own command: {seen_again:?}"
    );

    Ok(())
}

/// An empty `HISTFILE` keeps the in-memory list but reads and writes no file.
#[test]
fn empty_histfile_keeps_memory_history() -> anyhow::Result<()> {
    let home = tempfile::tempdir()?;
    let mut session = start_history_session(Some("basic"), None, home.path())?;
    session.expect_prompt()?;

    session.send_line("HISTFILE=")?;
    session.expect_prompt()?;
    session.send_line("echo kept-in-memory")?;
    session.expect("kept-in-memory")?;
    session.expect_prompt()?;

    let seen = home.path().join("seen");
    session.send_line(format!(
        "history > {}; echo HISTORY-DONE",
        brush_core::escape::single_quote(seen.to_str().context("seen path")?)
    ))?;
    session.expect("HISTORY-DONE")?;
    session.expect_prompt()?;

    let history = std::fs::read_to_string(&seen)?;
    assert!(history.contains("kept-in-memory"), "history: {history}");
    assert!(
        !home.path().join("history").exists(),
        "HISTFILE='' must not write the history file"
    );

    session.exit()?;
    Ok(())
}

/// A bare PTY whose master descriptor the test owns, so it can be closed to hang the terminal
/// up. `expectrl` keeps its master behind a duplicated handle, which cannot be closed that way.
struct RawPty {
    master: Option<std::fs::File>,
    child: std::process::Child,
    pending: Vec<u8>,
}

impl RawPty {
    #[allow(
        clippy::multiple_unsafe_ops_per_block,
        reason = "the pre-exec hook and controlling-terminal ioctl are one setup step"
    )]
    fn spawn(home: &std::path::Path) -> anyhow::Result<Self> {
        let pty = nix::pty::openpty(None, None)?;
        let slave = std::fs::File::from(pty.slave);

        let shell_path = assert_cmd::cargo::cargo_bin!("brush");
        let mut cmd = std::process::Command::new(shell_path);
        cmd.args([
            "--noprofile",
            "--norc",
            "--no-config",
            "--disable-bracketed-paste",
            "--disable-color",
        ]);
        cmd.stdin(std::process::Stdio::from(slave.try_clone()?));
        cmd.stdout(std::process::Stdio::from(slave.try_clone()?));
        cmd.stderr(std::process::Stdio::from(slave));
        let inputrc = home.join("inputrc");
        std::fs::write(&inputrc, "")?;
        cmd.env("PS1", "hist> ");
        cmd.env("TERM", "xterm-256color");
        cmd.env("HOME", home);
        cmd.env("HISTFILE", home.join("history"));
        cmd.env("INPUTRC", inputrc);
        // SAFETY: the pre-exec closure only calls async-signal-safe libc/nix functions to make
        // the child a session leader with this PTY as its controlling terminal.
        unsafe {
            cmd.pre_exec(|| {
                nix::unistd::setsid()?;
                if libc::ioctl(0, libc::TIOCSCTTY, 0) != 0 {
                    return Err(std::io::Error::last_os_error());
                }
                Ok(())
            });
        }

        // `openpty` does not set close-on-exec; without this the child would inherit a
        // duplicate of the master, so closing ours would not hang the terminal up.
        // SAFETY: the descriptor is owned and valid here.
        unsafe {
            libc::fcntl(
                std::os::fd::AsRawFd::as_raw_fd(&pty.master),
                libc::F_SETFD,
                libc::FD_CLOEXEC,
            );
        }

        let child = cmd.spawn()?;
        let master = std::fs::File::from(pty.master);
        // SAFETY: the descriptor is owned and valid here.
        let flags = unsafe { libc::fcntl(std::os::fd::AsRawFd::as_raw_fd(&master), libc::F_GETFL) };
        // SAFETY: the descriptor is owned and valid here.
        unsafe {
            libc::fcntl(
                std::os::fd::AsRawFd::as_raw_fd(&master),
                libc::F_SETFL,
                flags | libc::O_NONBLOCK,
            );
        }

        Ok(Self {
            master: Some(master),
            child,
            pending: Vec::new(),
        })
    }

    fn send_line(&mut self, line: &str) -> anyhow::Result<()> {
        use std::io::Write as _;
        let master = self.master.as_mut().context("master closed")?;
        master.write_all(line.as_bytes())?;
        master.write_all(b"\n")?;
        master.flush()?;
        Ok(())
    }

    fn expect(&mut self, needle: &str) -> anyhow::Result<()> {
        use std::io::Read as _;
        let deadline = std::time::Instant::now() + std::time::Duration::from_secs(10);
        loop {
            if let Some(position) = self
                .pending
                .windows(needle.len())
                .position(|window| window == needle.as_bytes())
            {
                self.pending.drain(..position + needle.len());
                return Ok(());
            }
            anyhow::ensure!(
                std::time::Instant::now() < deadline,
                "timed out waiting for {needle:?}; pending: {:?}",
                String::from_utf8_lossy(&self.pending)
            );
            let mut buffer = [0u8; 4096];
            let master = self.master.as_mut().context("master closed")?;
            match master.read(&mut buffer) {
                Ok(0) => anyhow::bail!("PTY closed while waiting for {needle:?}"),
                Ok(count) => self.pending.extend_from_slice(&buffer[..count]),
                Err(err) if err.kind() == std::io::ErrorKind::WouldBlock => {
                    std::thread::sleep(std::time::Duration::from_millis(10));
                }
                Err(err) => return Err(err.into()),
            }
        }
    }

    fn close_master(&mut self) {
        self.master.take();
    }

    fn wait_for_exit(&mut self) -> anyhow::Result<std::process::ExitStatus> {
        let deadline = std::time::Instant::now() + std::time::Duration::from_secs(5);
        loop {
            if let Some(status) = self.child.try_wait()? {
                return Ok(status);
            }
            anyhow::ensure!(
                std::time::Instant::now() < deadline,
                "shell did not exit after closing the PTY"
            );
            std::thread::sleep(std::time::Duration::from_millis(50));
        }
    }
}

/// Closing the PTY master must end the session, save the recorded commands, and leave them
/// readable in a later session. This is the real hangup path, not a signal delivered while the
/// master stays open.
#[test]
fn history_survives_closed_pty() -> anyhow::Result<()> {
    let home = tempfile::tempdir()?;
    let mut session = RawPty::spawn(home.path())?;
    session.expect("hist> ")?;
    session.send_line("printf 'confirmed-%s\\n' execution")?;
    session.expect("confirmed-execution")?;
    session.expect("hist> ")?;

    session.close_master();
    let status = session.wait_for_exit()?;
    assert_eq!(
        status.code(),
        Some(129),
        "unexpected exit status: {status:?}"
    );

    let contents = read_history_file(home.path());
    assert!(
        contents.contains("printf 'confirmed-%s\\n' execution"),
        "history lost after closing the PTY: {contents:?}"
    );

    // A later session must be able to read it back.
    let mut reopened = start_history_session(Some("basic"), None, home.path())?;
    reopened.expect_prompt()?;
    let seen = home.path().join("reopened-history.txt");
    reopened.send_line(format!(
        "history > {}; echo REOPEN-DONE",
        brush_core::escape::single_quote(seen.to_str().context("reopened path")?)
    ))?;
    reopened.expect("REOPEN-DONE")?;
    reopened.expect_prompt()?;
    let history = std::fs::read_to_string(&seen)?;
    assert!(
        history.contains("printf 'confirmed-%s\\n' execution"),
        "reopened session did not see the command: {history:?}"
    );

    Ok(())
}

/// Immediate appends and share mode must append during the session even after
/// `shopt -u histappend`; only the final save may rewrite the file.
#[test]
fn immediate_append_does_not_overwrite_other_sessions() -> anyhow::Result<()> {
    use std::io::Write as _;

    for setting in ["append-immediately = true", "share = true"] {
        let home = tempfile::tempdir()?;
        let config = home.path().join("config.toml");
        std::fs::write(&config, format!("[history]\n{setting}\n"))?;

        let mut session = start_history_session(Some("basic"), Some(&config), home.path())?;
        session.expect_prompt()?;

        session.send_line("shopt -u histappend; echo SHOPT-OFF")?;
        session.expect("SHOPT-OFF")?;
        session.expect_prompt()?;

        let history_file = home.path().join("history");
        let mut external = std::fs::OpenOptions::new()
            .create(true)
            .append(true)
            .open(&history_file)?;
        external.write_all(b"external-unique\n")?;
        external.sync_all()?;
        drop(external);

        session.send_line(": after-switch; echo AFTER-SWITCH")?;
        session.expect("AFTER-SWITCH")?;
        session.expect_prompt()?;

        let contents = read_history_file(home.path());
        assert!(
            contents.contains("external-unique"),
            "mid-session append with {setting:?} lost another session's entry: {contents:?}"
        );
        assert!(
            contents.contains(": after-switch"),
            "mid-session append with {setting:?} did not save its own entry: {contents:?}"
        );

        session
            .get_session_mut()
            .get_process_mut()
            .kill(Signal::SIGKILL)?;
    }

    Ok(())
}

/// A timestamped `lithist`-style entry keeps its embedded newlines when a new session
/// reloads it. The first session adds the entry explicitly so the test does not depend on
/// Reedline's multi-line cursor-position handshake.
#[test]
fn lithist_timestamped_entry_round_trips() -> anyhow::Result<()> {
    let home = tempfile::tempdir()?;
    let mut session = start_history_session(Some("basic"), None, home.path())?;
    session.expect_prompt()?;
    session.send_line("export HISTTIMEFORMAT='%s '")?;
    session.expect_prompt()?;
    session.send_line("history -s $'for x in 1; do\\n : inside-$x\\ndone'")?;
    session.expect_prompt()?;
    session.send_line("history -w")?;
    session.expect_prompt()?;
    session.exit()?;

    let contents = read_history_file(home.path());
    assert!(
        contents.contains("for x in 1; do\n : inside-$x\ndone"),
        "multi-line entry was split on write: {contents:?}"
    );

    let mut reopened = start_history_session(Some("basic"), None, home.path())?;
    reopened.expect_prompt()?;
    let seen = home.path().join("lithist-history.txt");
    reopened.send_line(format!(
        "history > {}; echo REOPEN-DONE",
        brush_core::escape::single_quote(seen.to_str().context("lithist path")?)
    ))?;
    reopened.expect("REOPEN-DONE")?;
    reopened.expect_prompt()?;
    let history = std::fs::read_to_string(&seen)?;
    assert!(
        history.contains("for x in 1; do\n : inside-$x\ndone"),
        "reloaded history split the entry: {history:?}"
    );

    Ok(())
}

/// The first `history -n` starts from where the startup import stopped, and later calls only
/// import new complete records, even across replacement, truncation, and a partial last line.
#[test]
fn history_n_starts_after_startup_import() -> anyhow::Result<()> {
    use std::io::Write as _;

    let home = tempfile::tempdir()?;
    let history_path = home.path().join("history");
    std::fs::write(&history_path, "old-entry\n")?;

    let mut session = start_history_session(Some("basic"), None, home.path())?;
    session.expect_prompt()?;

    let snapshot = |session: &mut RawShellSession, name: &str| -> anyhow::Result<String> {
        let path = home.path().join(name);
        session.send_line(format!(
            "history > {}; echo SNAPSHOT-DONE",
            brush_core::escape::single_quote(path.to_str().context("snapshot path")?)
        ))?;
        session.expect("SNAPSHOT-DONE")?;
        session.expect_prompt()?;
        Ok(std::fs::read_to_string(path)?)
    };

    std::fs::OpenOptions::new()
        .append(true)
        .open(&history_path)?
        .write_all(b"new-entry\n")?;
    session.send_line("history -n")?;
    session.expect_prompt()?;
    let first = snapshot(&mut session, "first.txt")?;
    assert_eq!(first.matches("old-entry").count(), 1, "{first:?}");
    assert_eq!(first.matches("new-entry").count(), 1, "{first:?}");

    // A second read imports nothing.
    session.send_line("history -n")?;
    session.expect_prompt()?;
    let second = snapshot(&mut session, "second.txt")?;
    assert_eq!(second.matches("old-entry").count(), 1, "{second:?}");
    assert_eq!(second.matches("new-entry").count(), 1, "{second:?}");

    // Replacing the file, even at the same size, resets the cursor.
    let replacement = home.path().join("replacement");
    std::fs::write(&replacement, "replaced!!\n")?;
    std::fs::rename(&replacement, &history_path)?;
    session.send_line("history -n")?;
    session.expect_prompt()?;
    let replaced = snapshot(&mut session, "replaced.txt")?;
    assert_eq!(replaced.matches("replaced!!").count(), 1, "{replaced:?}");

    // A partial last record waits for its newline.
    std::fs::write(&history_path, "unfinished-record")?;
    session.send_line("history -n")?;
    session.expect_prompt()?;
    let partial = snapshot(&mut session, "partial.txt")?;
    assert!(!partial.contains("unfinished-record"), "{partial:?}");
    std::fs::OpenOptions::new()
        .append(true)
        .open(&history_path)?
        .write_all(b"-done\n")?;
    session.send_line("history -n")?;
    session.expect_prompt()?;
    let completed = snapshot(&mut session, "completed.txt")?;
    assert_eq!(
        completed.matches("unfinished-record-done").count(),
        1,
        "{completed:?}"
    );

    // An explicit path is read independently of the default file's cursor.
    let other = home.path().join("other-history");
    std::fs::write(&other, "other-entry\n")?;
    session.send_line(format!(
        "history -n {}",
        brush_core::escape::single_quote(other.to_str().context("other path")?)
    ))?;
    session.expect_prompt()?;
    let other_seen = snapshot(&mut session, "other.txt")?;
    assert_eq!(
        other_seen.matches("other-entry").count(),
        1,
        "{other_seen:?}"
    );

    Ok(())
}

type ShellSession = ReplSession<Session<UnixProcess, LogStream<PtyStream, std::io::Stdout>>>;
type RawShellSession = ReplSession<Session<UnixProcess, PtyStream>>;
// N.B. Comment out the above line and uncomment out the following line to disable logging of the
// session. type ShellSession = ReplSession<Session<UnixProcess, PtyStream>>;

trait SessionExt {
    fn suspend(&mut self) -> anyhow::Result<()>;
    fn interrupt(&mut self) -> anyhow::Result<()>;
    fn exec_output<S: AsRef<str>>(&mut self, cmd: S) -> anyhow::Result<String>;
}

impl SessionExt for ShellSession {
    fn suspend(&mut self) -> anyhow::Result<()> {
        // Send Ctrl+Z to suspend.
        self.send(expectrl::ControlCode::Substitute)?;
        Ok(())
    }

    fn interrupt(&mut self) -> anyhow::Result<()> {
        // Send Ctrl+C to interrupt.
        self.send(expectrl::ControlCode::EndOfText)?;
        Ok(())
    }

    fn exec_output<S: AsRef<str>>(&mut self, cmd: S) -> anyhow::Result<String> {
        let output = self.execute(cmd)?;
        let output_str = String::from_utf8(output)?;
        Ok(output_str)
    }
}

/// Asserts that no process at the session's terminal has a command line matching `pattern`.
fn assert_no_process_left(session: &mut ShellSession, pattern: &str) -> anyhow::Result<()> {
    // (macOS's `pgrep` has no `-s` to search the session, but has `-t`, which also finds
    // processes that aren't the shell's children. It exits with 1 when it finds nothing, and
    // with 2 or more when it fails.)
    let output = session.exec_output(format!(
        "t=$(tty); pgrep -f -t \"${{t#/dev/}}\" '{pattern}'; echo \"pgrep: $?\""
    ))?;
    assert_eq!(output.trim(), "pgrep: 1", "every process should be gone");
    Ok(())
}

/// A process's state, as `ps` reports it.
#[derive(Clone, Copy, Debug)]
enum ProcessState {
    /// Waiting, as for input (`S`).
    Sleeping,
    /// Stopped by a signal (`T`).
    Stopped,
}

/// Waits for the shell's descendants named `name` to be in the given state.
fn wait_for_state(shell_pid: u32, name: &str, state: ProcessState) -> anyhow::Result<()> {
    let state_code = match state {
        ProcessState::Sleeping => 'S',
        ProcessState::Stopped => 'T',
    };
    let deadline = std::time::Instant::now() + std::time::Duration::from_secs(10);
    loop {
        let ps = std::process::Command::new("ps")
            .args(["-A", "-o", "pid=,ppid=,stat=,comm="])
            .output()?;
        let ps = String::from_utf8(ps.stdout)?;
        let processes: Vec<(u32, u32, &str, &str)> = ps
            .lines()
            .filter_map(|line| {
                let mut fields = line.split_whitespace();
                Some((
                    fields.next()?.parse().ok()?,
                    fields.next()?.parse().ok()?,
                    fields.next()?,
                    fields.next()?,
                ))
            })
            .collect();

        let mut descendants = vec![shell_pid];
        let mut i = 0;
        while let Some(&parent) = descendants.get(i) {
            descendants.extend(processes.iter().filter(|p| p.1 == parent).map(|p| p.0));
            i += 1;
        }

        let named: Vec<_> = processes
            .iter()
            .filter(|p| descendants.contains(&p.0) && p.3.rsplit('/').next() == Some(name))
            .collect();
        if !named.is_empty() && named.iter().all(|p| p.2.starts_with(state_code)) {
            return Ok(());
        }

        anyhow::ensure!(
            std::time::Instant::now() < deadline,
            "{name} isn't {state:?}: {named:?}"
        );
        std::thread::sleep(std::time::Duration::from_millis(20));
    }
}

/// Returns whether the session's terminal reads lines, echoes input, and generates signals.
fn terminal_modes(session: &mut ShellSession) -> anyhow::Result<Vec<String>> {
    let output = session.exec_output("stty -a")?;
    Ok(output
        .split(|c: char| c.is_whitespace() || c == ';')
        .filter(|mode| matches!(mode.trim_start_matches('-'), "icanon" | "echo" | "isig"))
        .map(str::to_owned)
        .collect())
}

/// Writes `contents` to a temporary file that is deleted on drop.
fn temp_script(contents: &str) -> anyhow::Result<tempfile::NamedTempFile> {
    use std::io::Write as _;

    let mut file = tempfile::NamedTempFile::new()?;
    file.write_all(contents.as_bytes())?;
    Ok(file)
}

fn start_shell_session() -> anyhow::Result<ShellSession> {
    start_shell_session_with(|cmd| _ = cmd.arg("--norc"))
}

/// Starts a shell session at a pty. `configure` is the only place arguments and environment
/// come from beyond the fixed set below, so a session says for itself what it wants -- notably
/// `--norc`, or the `--rcfile` that a session wanting an rc file passes instead.
fn start_shell_session_with(
    configure: impl FnOnce(&mut std::process::Command),
) -> anyhow::Result<ShellSession> {
    const DEFAULT_PROMPT: &str = "brush> ";
    let shell_path = assert_cmd::cargo::cargo_bin!("brush");

    let mut cmd = std::process::Command::new(shell_path);
    cmd.args([
        "--noprofile",
        "--no-config",
        "--disable-bracketed-paste",
        "--disable-color",
        "--input-backend=basic",
    ]);
    cmd.env("PS1", DEFAULT_PROMPT);
    cmd.env("TERM", "xterm-256color");
    configure(&mut cmd);

    let session = expectrl::session::Session::spawn(cmd)?;

    // N.B. Comment out this line to disable logging of the session (along with a similar line
    // above).
    let session = expectrl::session::log(session, std::io::stdout())?;

    let mut session = expectrl::repl::ReplSession::new(session, DEFAULT_PROMPT);
    session.set_echo(true);

    Ok(session)
}
