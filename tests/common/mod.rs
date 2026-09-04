//! Helpers shared by the end-to-end suites: the built binary's path and a
//! way to run it attached to a terminal.

use std::path::Path;
use std::process::Command;

pub fn upd_bin() -> &'static str {
    env!("CARGO_BIN_EXE_upd")
}

/// Run `upd` with the given arguments attached to a terminal, feed it
/// `input`, and return the exit code it reported with everything it printed.
///
/// `--interactive` refuses to run without a terminal, so there is no way to
/// reach the interactive code path from a plain `Command`. `script` allocates
/// a pty and ships with both platforms this suite runs on, which is a smaller
/// cost than a pty dependency added to test an exit code. It does not pass
/// the child's exit status back on macOS, so the child writes its own status
/// to a file and that file is the answer. `input` reaches the child through
/// the pty, the way a person's keystrokes would.
#[cfg(unix)]
pub fn run_on_a_terminal(
    args: &[&str],
    dir: &Path,
    env: &[(&str, &str)],
    input: &str,
) -> (i32, String) {
    use std::fs;
    use std::io::Write;
    use std::process::Stdio;

    let code_path = dir.join("exit-code");
    let inner = format!(
        "{} {} > '{}' 2>&1; printf %s $? > '{}'",
        shell_quote(upd_bin()),
        args.iter()
            .map(|a| shell_quote(a))
            .collect::<Vec<_>>()
            .join(" "),
        dir.join("output").display(),
        code_path.display(),
    );

    let mut command = Command::new("script");
    if cfg!(target_os = "macos") {
        command.args(["-q", "/dev/null", "/bin/sh", "-c", &inner]);
    } else {
        command.args(["-q", "-c", &inner, "/dev/null"]);
    }
    for (key, value) in env {
        command.env(key, value);
    }
    let mut child = command
        .current_dir(dir)
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .expect("could not run `script`; a pty is required to exercise --interactive");
    // Closing the pipe after the answers is what `script` reads as the end of
    // the session's input.
    let mut stdin = child.stdin.take().expect("script's stdin was not piped");
    stdin
        .write_all(input.as_bytes())
        .expect("could not feed input to the terminal");
    drop(stdin);
    child
        .wait_with_output()
        .expect("could not wait for `script`");

    let output = fs::read_to_string(dir.join("output")).unwrap_or_default();
    // The guard's own message means `script` handed the binary something that
    // was not a terminal. Without this the test would pass on exit 2 for the
    // wrong reason and stop watching the path it exists to watch.
    assert!(
        !output.contains("--interactive requires a terminal"),
        "`script` did not allocate a terminal, so the interactive path never ran: {output}"
    );
    let code = fs::read_to_string(&code_path)
        .expect("the run under `script` wrote no exit code")
        .trim()
        .parse()
        .expect("exit code was not a number");
    (code, output)
}

#[cfg(unix)]
fn shell_quote(s: &str) -> String {
    format!("'{}'", s.replace('\'', r"'\''"))
}
