use std::env;
use std::path::PathBuf;
use std::process::{self, Command};

fn locate_upd() -> Option<PathBuf> {
    let exe = env::current_exe().ok()?;
    let dir = exe.parent()?;
    let name = if cfg!(windows) { "upd.exe" } else { "upd" };
    let candidate = dir.join(name);
    candidate.is_file().then_some(candidate)
}

fn main() {
    let Some(upd) = locate_upd() else {
        eprintln!("upd-cli: could not locate the `upd` binary alongside this executable");
        process::exit(127);
    };

    let args: Vec<_> = env::args_os().skip(1).collect();

    #[cfg(unix)]
    {
        use std::os::unix::process::CommandExt;
        let err = Command::new(&upd).args(&args).exec();
        eprintln!("upd-cli: failed to exec {}: {err}", upd.display());
        process::exit(127);
    }

    #[cfg(not(unix))]
    {
        match Command::new(&upd).args(&args).status() {
            Ok(status) => process::exit(status.code().unwrap_or(1)),
            Err(err) => {
                eprintln!("upd-cli: failed to spawn {}: {err}", upd.display());
                process::exit(127);
            }
        }
    }
}
