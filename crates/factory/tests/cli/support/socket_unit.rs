//! systemd's part in socket activation, for tests of the installed units.
//!
//! A [`SocketUnit`] listens where the installed socket unit says (`ListenStream=`). When a
//! connection waits and no daemon runs, it starts the service unit's `ExecStart=` with the
//! listening socket as fd 3 and `LISTEN_FDS=1`, as systemd does, and it starts the next one
//! once that one has stopped. The socket outlives every daemon, so calls made while none runs
//! wait in it.

use std::fs;
use std::os::fd::OwnedFd;
use std::os::unix::fs::DirBuilderExt;
use std::os::unix::net::UnixListener;
use std::os::unix::process::CommandExt;
use std::path::Path;
use std::process::{Command, Stdio};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::thread::{self, JoinHandle};

use rustix::event::{PollFd, PollFlags, Timespec, poll};
use rustix::process::Signal;

use super::world::poll_until;
use super::{Sandbox, kill_group};

/// Moves the socket from stdin to fd 3 and names it, as systemd passes it.
const ACTIVATE: &str = "exec 3<&0 0</dev/null; export LISTEN_FDS=1 LISTEN_PID=$$; exec \"$@\"";
/// How long the listener waits for a connection before it looks at whether to stop.
const LOOK: Timespec = Timespec {
    tv_sec: 0,
    tv_nsec: 20_000_000,
};

#[derive(Default)]
struct Seen {
    activations: usize,
    daemon: Option<u32>,
}

pub struct SocketUnit {
    seen: Arc<Mutex<Seen>>,
    stopping: Arc<AtomicBool>,
    thread: Option<JoinHandle<()>>,
}

impl SocketUnit {
    /// Listens as the units installed in the sandbox's systemd user dir say.
    pub fn from_installed(sandbox: &Sandbox) -> SocketUnit {
        let units = sandbox.path("config/systemd/user");
        let socket = setting(&units.join("factory.socket"), "ListenStream")
            .replace("%t", &sandbox.path("run").to_string_lossy());
        let exec = setting(&units.join("factory.service"), "ExecStart")
            .replace("%h", &sandbox.path("home").to_string_lossy());
        let socket = Path::new(&socket);
        fs::DirBuilder::new()
            .recursive(true)
            .mode(0o700)
            .create(socket.parent().unwrap())
            .unwrap();
        // As systemd does, take the path over from a socket nothing listens on.
        let _ = fs::remove_file(socket);
        let listener = UnixListener::bind(socket).unwrap();
        let mut args = vec!["-c", ACTIVATE, "sh"];
        args.extend(exec.split_whitespace());
        let template = sandbox.command("sh", &args);
        let log = sandbox.path("run/socket-unit.log");
        let seen = Arc::new(Mutex::new(Seen::default()));
        let stopping = Arc::new(AtomicBool::new(false));
        let thread = {
            let (seen, stopping) = (Arc::clone(&seen), Arc::clone(&stopping));
            thread::spawn(move || {
                while !stopping.load(Ordering::SeqCst) {
                    let mut fds = [PollFd::new(&listener, PollFlags::IN)];
                    if poll(&mut fds, Some(&LOOK)).unwrap_or(0) == 0 {
                        continue;
                    }
                    let mut cmd = like(&template);
                    let socket = OwnedFd::from(listener.try_clone().unwrap());
                    cmd.stdin(Stdio::from(socket))
                        .stdout(Stdio::null())
                        .stderr(
                            fs::File::options()
                                .create(true)
                                .append(true)
                                .open(&log)
                                .unwrap(),
                        )
                        .process_group(0);
                    let mut daemon = cmd.spawn().unwrap();
                    {
                        let mut seen = seen.lock().unwrap();
                        seen.activations += 1;
                        seen.daemon = Some(daemon.id());
                    }
                    let _ = daemon.wait();
                    seen.lock().unwrap().daemon = None;
                }
            })
        };
        SocketUnit {
            seen,
            stopping,
            thread: Some(thread),
        }
    }

    /// How many daemons the socket has started.
    pub fn activations(&self) -> usize {
        self.seen.lock().unwrap().activations
    }

    /// Stops the running daemon with SIGTERM, as `systemctl stop` does, and waits until it
    /// has exited. The next connection starts another.
    pub fn stop_daemon(&self) {
        let mut daemon = None;
        assert!(
            poll_until(|| {
                daemon = self.seen.lock().unwrap().daemon;
                daemon.is_some()
            }),
            "no daemon ran to stop"
        );
        let daemon = daemon.unwrap();
        kill_group(daemon, Signal::TERM);
        assert!(
            poll_until(|| self.seen.lock().unwrap().daemon != Some(daemon)),
            "daemon {daemon} outlived its SIGTERM"
        );
    }
}

impl Drop for SocketUnit {
    fn drop(&mut self) {
        self.stopping.store(true, Ordering::SeqCst);
        if let Some(daemon) = self.seen.lock().map(|seen| seen.daemon).unwrap_or(None) {
            kill_group(daemon, Signal::KILL);
        }
        if let Some(thread) = self.thread.take() {
            let _ = thread.join();
        }
    }
}

/// The value of the first `key=` line of a unit file.
fn setting(unit: &Path, key: &str) -> String {
    let text = fs::read_to_string(unit).unwrap();
    let prefix = format!("{key}=");
    text.lines()
        .find_map(|line| line.strip_prefix(&prefix))
        .unwrap_or_else(|| panic!("no {key}= in {}", unit.display()))
        .to_owned()
}

/// A command like `template`: its program, arguments, environment and working directory.
fn like(template: &Command) -> Command {
    let mut cmd = Command::new(template.get_program());
    cmd.args(template.get_args()).env_clear();
    for (key, value) in template.get_envs() {
        if let Some(value) = value {
            cmd.env(key, value);
        }
    }
    if let Some(dir) = template.get_current_dir() {
        cmd.current_dir(dir);
    }
    cmd
}
