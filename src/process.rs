use std::path::{Path, PathBuf};
use std::process::{Child, Command};

pub(crate) fn find_executable(name: &str) -> Option<PathBuf> {
    let path = std::env::var_os("PATH")?;
    std::env::split_paths(&path)
        .map(|directory| directory.join(name))
        .find(|candidate| candidate.is_file())
}

/// Pick an X display that has neither a socket nor a lock file.
pub(crate) fn pick_free_display(socket_dir: &Path) -> Option<u32> {
    let used: Vec<u32> = std::fs::read_dir(socket_dir)
        .into_iter()
        .flatten()
        .flatten()
        .filter_map(|entry| entry.file_name().into_string().ok())
        .filter_map(|name| name.strip_prefix('X').and_then(|value| value.parse().ok()))
        .collect();

    (10..99).find(|number| {
        !used.contains(number) && !Path::new("/tmp").join(format!(".X{number}-lock")).exists()
    })
}

/// Spawn a child that the kernel kills if its owning process disappears.
pub(crate) fn spawn_owned_child(command: &mut Command) -> std::io::Result<Child> {
    use std::os::unix::process::CommandExt;

    let expected_parent = std::process::id() as libc::pid_t;
    // SAFETY: pre_exec runs after fork. The closure calls only async-signal-
    // safe libc functions and constructs errno-backed errors.
    unsafe {
        command.pre_exec(move || {
            if libc::prctl(libc::PR_SET_PDEATHSIG, libc::SIGKILL, 0, 0, 0) == -1 {
                return Err(std::io::Error::last_os_error());
            }
            if libc::getppid() != expected_parent {
                return Err(std::io::Error::from_raw_os_error(libc::ESRCH));
            }
            Ok(())
        });
    }
    command.spawn()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn display_picker_avoids_sockets_and_locks() {
        let directory =
            std::env::temp_dir().join(format!("agent-seat-display-test-{}", uuid::Uuid::new_v4()));
        std::fs::create_dir(&directory).unwrap();
        std::fs::write(directory.join("X10"), b"").unwrap();
        assert_eq!(pick_free_display(&directory), Some(11));
        std::fs::remove_dir_all(directory).unwrap();
    }
}
