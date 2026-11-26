use std::ffi::OsStr;
use std::io;
use std::process::{Command, Stdio};

pub fn open_target(target: impl AsRef<OsStr>) -> io::Result<()> {
    run_launchers(open::commands(target), false)
}

pub fn open_target_detached(target: impl AsRef<OsStr>) -> io::Result<()> {
    run_launchers(open::commands(target), true)
}

fn run_launchers(commands: Vec<Command>, detached: bool) -> io::Result<()> {
    let mut last_error = io::Error::new(io::ErrorKind::NotFound, "no system launcher");
    for mut command in commands {
        aionui_common::host_env::strip_from_command(&mut command);
        command.stdin(Stdio::null()).stdout(Stdio::null()).stderr(Stdio::null());
        if detached {
            configure_detached(&mut command);
        }
        match command.spawn() {
            Ok(_child) if detached && cfg!(windows) => return Ok(()),
            Ok(mut child) => {
                let status = child.wait()?;
                return if status.success() {
                    Ok(())
                } else {
                    Err(io::Error::other(format!("system launcher exited with {status}")))
                };
            }
            Err(error) => last_error = error,
        }
    }
    Err(last_error)
}

#[cfg(unix)]
fn configure_detached(command: &mut Command) {
    use std::os::unix::process::CommandExt;

    // Match open's Unix detachment. The intermediate child exits immediately
    // and is reaped by the caller while the launcher runs in its own session.
    // SAFETY: the child callback only performs async-signal-safe syscalls.
    unsafe {
        command.pre_exec(|| {
            match libc::fork() {
                -1 => return Err(io::Error::last_os_error()),
                0 => {}
                _ => libc::_exit(0),
            }
            if libc::setsid() == -1 {
                return Err(io::Error::last_os_error());
            }
            Ok(())
        });
    }
}

#[cfg(windows)]
fn configure_detached(command: &mut Command) {
    use std::os::windows::process::CommandExt;

    const CREATE_NEW_PROCESS_GROUP: u32 = 0x0000_0200;
    const CREATE_NO_WINDOW: u32 = 0x0800_0000;
    command.creation_flags(CREATE_NEW_PROCESS_GROUP | CREATE_NO_WINDOW);
}

#[cfg(all(test, unix))]
mod tests {
    use super::*;

    #[test]
    fn system_launcher_filters_host_env() {
        if !crate::test_support::run_in_env_child("system_open::tests::system_launcher_filters_host_env", |command| {
            command.env("JWT_SECRET", "ambient fixture");
        }) {
            return;
        }
        let mut command = Command::new("/bin/sh");
        command
            .args([
                "-c",
                "test -z \"${JWT_SECRET+x}\" && test \"$HOST78_USER_VAR\" = explicit",
            ])
            .env("HOST78_USER_VAR", "explicit");
        run_launchers(vec![command], false).unwrap();
    }
}
