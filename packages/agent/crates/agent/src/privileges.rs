//! Root only long enough to read the Docker socket's group, a host property no image can know.

use std::path::Path;

use crate::host_layout::DOCKER_SOCKET;
use crate::logger::Logger;

/// The owner of the files the agent writes under the data directory.
pub(crate) const AGENT_ID: u32 = 1000;

/// The system calls of a privilege drop, behind a trait so tests can record them.
#[allow(clippy::missing_errors_doc, reason = "each method fails as the system call it names")]
pub trait PrivilegeCalls {
    fn getuid(&self) -> u32;
    fn setgroups(&mut self, groups: &[u32]) -> nix::Result<()>;
    fn setgid(&mut self, id: u32) -> nix::Result<()>;
    fn setuid(&mut self, id: u32) -> nix::Result<()>;
    /// The socket's group, or `None` when no socket is at the path.
    fn socket_gid(&self, path: &Path) -> Option<u32>;
}

#[must_use]
pub(crate) fn socket_gid(path: &Path) -> Option<u32> {
    use std::os::unix::fs::{FileTypeExt as _, MetadataExt as _};

    std::fs::metadata(path).ok().filter(|meta| meta.file_type().is_socket()).map(|meta| meta.gid())
}

/// The real process.
pub struct ProcessCalls;

impl PrivilegeCalls for ProcessCalls {
    fn getuid(&self) -> u32 {
        nix::unistd::Uid::current().as_raw()
    }

    fn setgroups(&mut self, groups: &[u32]) -> nix::Result<()> {
        set_groups(groups)
    }

    fn setgid(&mut self, id: u32) -> nix::Result<()> {
        nix::unistd::setgid(nix::unistd::Gid::from_raw(id))
    }

    fn setuid(&mut self, id: u32) -> nix::Result<()> {
        nix::unistd::setuid(nix::unistd::Uid::from_raw(id))
    }

    fn socket_gid(&self, path: &Path) -> Option<u32> {
        socket_gid(path)
    }
}

#[cfg(target_os = "linux")]
fn set_groups(groups: &[u32]) -> nix::Result<()> {
    let groups: Vec<nix::unistd::Gid> = groups.iter().map(|&gid| nix::unistd::Gid::from_raw(gid)).collect();

    nix::unistd::setgroups(&groups)
}

#[cfg(not(target_os = "linux"))]
fn set_groups(_groups: &[u32]) -> nix::Result<()> {
    Err(nix::Error::ENOTSUP)
}

#[derive(Debug, thiserror::Error)]
#[error("could not drop root to uid {AGENT_ID}; the container needs the SETUID and SETGID capabilities")]
pub struct DropError(#[source] pub nix::Error);

/// Switches from root to `AGENT_ID` with the socket's group. The user id goes last, because
/// setting it also clears the process's capabilities.
///
/// # Errors
///
/// Fails rather than carry on as root, as in a container started without SETUID and SETGID.
pub fn drop_privileges(calls: &mut dyn PrivilegeCalls, log: &Logger) -> Result<(), DropError> {
    if calls.getuid() != 0 {
        return Ok(());
    }

    let gid = calls.socket_gid(Path::new(DOCKER_SOCKET));

    match gid {
        // The agent's own error on connecting is the useful one.
        None => log.warn(format!("{DOCKER_SOCKET} is not mounted; starting anyway so the agent reports it")),
        Some(gid) => log.info(format!("{DOCKER_SOCKET} is group {gid}; running as uid {AGENT_ID} with that group")),
    }

    calls.setgroups(&[gid.unwrap_or(AGENT_ID)]).map_err(DropError)?;
    calls.setgid(AGENT_ID).map_err(DropError)?;
    calls.setuid(AGENT_ID).map_err(DropError)
}

#[cfg(test)]
#[allow(clippy::disallowed_methods, reason = "a test locks its fixtures and fails on a poisoned lock")]
mod tests {
    use std::sync::{Arc, Mutex};

    use super::*;
    use crate::logger::{Entry, Level};

    struct Recorded {
        uid: u32,
        gid: Option<u32>,
        made: Vec<String>,
        asked: std::cell::RefCell<Vec<String>>,
        refuse_groups: bool,
    }

    impl PrivilegeCalls for Recorded {
        fn getuid(&self) -> u32 {
            self.uid
        }

        fn setgroups(&mut self, groups: &[u32]) -> nix::Result<()> {
            if self.refuse_groups {
                return Err(nix::Error::EPERM);
            }

            self.made.push(format!("setgroups {}", groups.iter().map(ToString::to_string).collect::<Vec<_>>().join(",")));
            Ok(())
        }

        fn setgid(&mut self, id: u32) -> nix::Result<()> {
            self.made.push(format!("setgid {id}"));
            Ok(())
        }

        fn setuid(&mut self, id: u32) -> nix::Result<()> {
            self.made.push(format!("setuid {id}"));
            Ok(())
        }

        fn socket_gid(&self, path: &Path) -> Option<u32> {
            self.asked.borrow_mut().push(path.display().to_string());
            self.gid
        }
    }

    fn calls(uid: u32, gid: Option<u32>) -> Recorded {
        Recorded { uid, gid, made: Vec::new(), asked: std::cell::RefCell::default(), refuse_groups: false }
    }

    fn recording() -> (Logger, Arc<Mutex<Vec<String>>>) {
        let lines = Arc::new(Mutex::new(Vec::new()));
        let seen = Arc::clone(&lines);
        let sink = Arc::new(move |entry: Entry| {
            let level = if entry.level == Level::Warn { "warn" } else { "info" };

            seen.lock().unwrap().push(format!("{level} {}", entry.message.trim_start_matches("[test] ")));
        });

        (Logger::new("test", Some(sink)), lines)
    }

    #[test]
    fn keeps_the_sockets_group_and_gives_up_root_group_first_user_last() {
        let mut process = calls(0, Some(988));
        let (log, lines) = recording();

        drop_privileges(&mut process, &log).unwrap();

        assert_eq!(*process.asked.borrow(), ["/var/run/docker.sock"]);
        assert_eq!(process.made, ["setgroups 988", "setgid 1000", "setuid 1000"]);
        assert_eq!(*lines.lock().unwrap(), ["info /var/run/docker.sock is group 988; running as uid 1000 with that group"]);
    }

    #[test]
    fn starts_unprivileged_without_the_socket() {
        let mut process = calls(0, None);
        let (log, lines) = recording();

        drop_privileges(&mut process, &log).unwrap();

        assert_eq!(process.made, ["setgroups 1000", "setgid 1000", "setuid 1000"]);
        assert_eq!(*lines.lock().unwrap(), ["warn /var/run/docker.sock is not mounted; starting anyway so the agent reports it"]);
    }

    #[test]
    fn stops_naming_the_missing_capabilities_rather_than_run_on_as_root() {
        let mut process = calls(0, Some(988));

        process.refuse_groups = true;

        let error = drop_privileges(&mut process, &Logger::null()).unwrap_err();

        assert_eq!(error.to_string(), "could not drop root to uid 1000; the container needs the SETUID and SETGID capabilities");
        assert_eq!(error.0, nix::Error::EPERM);
    }

    #[test]
    fn changes_nothing_when_not_started_as_root() {
        let mut process = calls(1000, Some(988));

        drop_privileges(&mut process, &Logger::null()).unwrap();

        assert!(process.made.is_empty());
        assert!(process.asked.borrow().is_empty());
    }

    #[test]
    fn the_group_is_read_off_a_socket_and_is_none_for_a_file_or_nothing() {
        let dir = tempfile::tempdir().unwrap();
        let socket = dir.path().join("docker.sock");
        let _listener = std::os::unix::net::UnixListener::bind(&socket).unwrap();

        std::fs::write(dir.path().join("file"), "").unwrap();

        assert_eq!(socket_gid(&socket), Some(nix::unistd::Gid::current().as_raw()));
        assert_eq!(socket_gid(&dir.path().join("file")), None);
        assert_eq!(socket_gid(&dir.path().join("missing")), None);
    }

    /// A test can make the real calls without changing the test process, because a user other than
    /// root is refused each change.
    #[test]
    fn the_real_process_answers_with_its_own_ids_and_refuses_a_user_the_changes() {
        use std::os::unix::fs::MetadataExt as _;

        let dir = tempfile::tempdir().unwrap();
        let socket = dir.path().join("docker.sock");
        let _listener = std::os::unix::net::UnixListener::bind(&socket).unwrap();
        let owner = std::fs::metadata(&socket).unwrap();
        let mut process = ProcessCalls;

        assert_eq!(process.getuid(), owner.uid());
        assert_eq!(process.socket_gid(&socket), Some(owner.gid()));
        assert_eq!(process.socket_gid(&dir.path().join("missing")), None);

        if owner.uid() == 0 {
            return;
        }

        assert!(process.setgroups(&[0]).is_err());
        assert_eq!(process.setgid(0), Err(nix::Error::EPERM));
        assert_eq!(process.setuid(0), Err(nix::Error::EPERM));
    }
}
