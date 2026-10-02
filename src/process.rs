use crate::{
    backend::{cancelled, Error},
    config::Config,
};
use std::{
    fs,
    io::{self, Read},
    os::{
        fd::{AsRawFd, FromRawFd, OwnedFd},
        unix::{fs::PermissionsExt, process::CommandExt},
    },
    path::{Path, PathBuf},
    process::{Child, Command, ExitStatus, Stdio},
    sync::atomic::AtomicBool,
    thread,
    time::{Duration, Instant},
};

const OUTPUT_LIMIT: usize = 64 * 1024;

fn guarded_command(path: &Path) -> Command {
    let supervisor = unsafe { libc::getpid() };
    let mut command = Command::new(path);
    command.process_group(0);
    // After fork, this callback and its guardian use no allocation, locks, or Rust
    // unwinding. The guardian never returns to the copied multithreaded runtime.
    unsafe {
        command.pre_exec(move || parent_death_guard(supervisor));
    }
    command
}

fn parent_death_guard(supervisor: libc::pid_t) -> io::Result<()> {
    unsafe {
        if libc::prctl(
            libc::PR_SET_PDEATHSIG,
            libc::SIGKILL as libc::c_ulong,
            0,
            0,
            0,
        ) < 0
        {
            return Err(io::Error::last_os_error());
        }
        // Close the race between fork and installing the direct leader's death signal.
        if libc::getppid() != supervisor {
            libc::_exit(1);
        }
        if libc::getpgrp() != libc::getpid() {
            return Err(io::Error::from_raw_os_error(libc::EINVAL));
        }
        let supervisor_fd = libc::syscall(libc::SYS_pidfd_open, supervisor, 0u32) as libc::c_int;
        if supervisor_fd < 0 {
            return Err(io::Error::last_os_error());
        }
        let supervisor_fd = OwnedFd::from_raw_fd(supervisor_fd);
        let leader_fd = libc::syscall(libc::SYS_pidfd_open, libc::getpid(), 0u32) as libc::c_int;
        if leader_fd < 0 {
            return Err(io::Error::last_os_error());
        }
        let leader_fd = OwnedFd::from_raw_fd(leader_fd);
        let mut pipe = [-1; 2];
        if libc::pipe2(pipe.as_mut_ptr(), libc::O_CLOEXEC) < 0 {
            return Err(io::Error::last_os_error());
        }
        let ready_read = OwnedFd::from_raw_fd(pipe[0]);
        let ready_write = OwnedFd::from_raw_fd(pipe[1]);
        // Every caller configures all three standard descriptors before pre_exec.
        if supervisor_fd.as_raw_fd() < 3 || leader_fd.as_raw_fd() < 3 || ready_write.as_raw_fd() < 3
        {
            return Err(io::Error::from_raw_os_error(libc::EBADF));
        }
        let mut blocked = std::mem::zeroed::<libc::sigset_t>();
        let mut previous = std::mem::zeroed::<libc::sigset_t>();
        libc::sigfillset(&mut blocked);
        if libc::sigprocmask(libc::SIG_SETMASK, &blocked, &mut previous) < 0 {
            return Err(io::Error::last_os_error());
        }
        // Do not run library pthread_atfork handlers inside the already-forked child.
        let guardian = libc::syscall(libc::SYS_fork) as libc::pid_t;
        if guardian == 0 {
            // Preserve only the two identity-stable watches and the readiness pipe.
            // In particular, do not keep the operation lock or spawn's error pipe open.
            if libc::dup2(supervisor_fd.as_raw_fd(), 0) < 0
                || libc::dup2(leader_fd.as_raw_fd(), 1) < 0
                || libc::dup2(ready_write.as_raw_fd(), 2) < 0
                || libc::syscall(libc::SYS_close_range, 3u32, u32::MAX, 0u32) < 0
            {
                libc::_exit(1);
            }
            let ready = 1u8;
            if libc::write(2, (&ready as *const u8).cast(), 1) != 1 {
                libc::_exit(1);
            }
            libc::close(2);
            let mut watches = [
                libc::pollfd {
                    fd: 0,
                    events: libc::POLLIN,
                    revents: 0,
                },
                libc::pollfd {
                    fd: 1,
                    events: libc::POLLIN,
                    revents: 0,
                },
            ];
            loop {
                if libc::poll(watches.as_mut_ptr(), 2, -1) >= 0 {
                    break;
                }
                if *libc::__errno_location() != libc::EINTR {
                    break;
                }
            }
            // This guardian is itself a member: the PGID cannot have been recycled.
            // Also watch the supervisor directly in case exec clears PDEATHSIG.
            libc::kill(-libc::getpgrp(), libc::SIGKILL);
            libc::_exit(1);
        }
        let fork_error = if guardian < 0 {
            Some(io::Error::last_os_error())
        } else {
            None
        };
        drop(ready_write);
        let mut ready = 0u8;
        if guardian > 0 {
            loop {
                let result = libc::read(ready_read.as_raw_fd(), (&mut ready as *mut u8).cast(), 1);
                if result >= 0 || *libc::__errno_location() != libc::EINTR {
                    break;
                }
            }
        }
        if libc::sigprocmask(libc::SIG_SETMASK, &previous, std::ptr::null_mut()) < 0 {
            return Err(io::Error::last_os_error());
        }
        if let Some(error) = fork_error {
            return Err(error);
        }
        if ready != 1 {
            // EOF means failed guardian initialization. Reap it before refusing exec.
            while libc::waitpid(guardian, std::ptr::null_mut(), 0) < 0 {
                if *libc::__errno_location() != libc::EINTR {
                    break;
                }
            }
            return Err(io::Error::from_raw_os_error(libc::EIO));
        }
        Ok(())
    }
}

pub(crate) fn executable(name: &str) -> Result<PathBuf, Error> {
    let candidates: Vec<PathBuf> = if name.contains('/') {
        vec![PathBuf::from(name)]
    } else {
        std::env::var_os("PATH")
            .map(|p| {
                std::env::split_paths(&p)
                    .filter(|p| p.is_absolute())
                    .map(|p| p.join(name))
                    .collect()
            })
            .unwrap_or_default()
    };
    candidates.into_iter().find(|p| fs::metadata(p).is_ok_and(|m| m.is_file() && m.permissions().mode() & 0o111 != 0))
        .ok_or_else(|| Error::Dependency("Moonlight executable not found or not executable; install Moonlight Qt and check PATH/config".into()))
}

struct Group(Child, Duration, bool);
impl Group {
    fn poll(&mut self) -> Result<Option<ExitStatus>, Error> {
        let mut info = std::mem::MaybeUninit::<libc::siginfo_t>::zeroed();
        // Observe without reaping so the process-group ID cannot be reused before cleanup.
        let result = unsafe {
            libc::waitid(
                libc::P_PID,
                self.0.id(),
                info.as_mut_ptr(),
                libc::WEXITED | libc::WNOHANG | libc::WNOWAIT,
            )
        };
        if result < 0 {
            if io::Error::last_os_error().kind() == io::ErrorKind::Interrupted {
                return Ok(None);
            }
            return Err(Error::Stream(
                "Cannot supervise Moonlight child process".into(),
            ));
        }
        if unsafe { info.assume_init().si_pid() } == 0 {
            return Ok(None);
        }
        unsafe {
            libc::kill(-(self.0.id() as i32), libc::SIGKILL);
        }
        let status = self
            .0
            .wait()
            .map_err(|_| Error::Stream("Cannot reap Moonlight child process".into()))?;
        self.2 = false;
        Ok(Some(status))
    }

    fn terminate(&mut self) {
        if !self.2 {
            return;
        }
        let pid = self.0.id() as i32;
        // The child owns a fresh process group; kill descendants as well as the leader.
        unsafe {
            libc::kill(-pid, libc::SIGTERM);
        }
        let deadline = Instant::now() + self.1;
        while Instant::now() < deadline {
            thread::sleep(Duration::from_millis(10));
        }
        unsafe {
            libc::kill(-pid, libc::SIGKILL);
        }
        let _ = self.0.wait();
        self.2 = false;
    }
}
impl Drop for Group {
    fn drop(&mut self) {
        self.terminate();
    }
}

pub(crate) struct Output {
    pub success: bool,
    pub stdout: Vec<u8>,
    pub timed_out: bool,
}

fn nonblocking(pipe: &impl AsRawFd) -> Result<(), Error> {
    let fd = pipe.as_raw_fd();
    let flags = unsafe { libc::fcntl(fd, libc::F_GETFL) };
    if flags < 0 || unsafe { libc::fcntl(fd, libc::F_SETFL, flags | libc::O_NONBLOCK) } < 0 {
        return Err(Error::Stream("Cannot supervise Moonlight output".into()));
    }
    Ok(())
}

fn drain(pipe: &mut impl Read, bytes: &mut Vec<u8>) -> Result<(), Error> {
    let mut buffer = [0; 4096];
    // A continuously writing child must not prevent checking the deadline.
    for _ in 0..32 {
        match pipe.read(&mut buffer) {
            Ok(0) => break,
            Ok(n) => {
                if bytes.len() + n > OUTPUT_LIMIT {
                    return Err(Error::Stream(
                        "Moonlight list output exceeded the safe limit".into(),
                    ));
                }
                bytes.extend_from_slice(&buffer[..n]);
            }
            Err(e) if e.kind() == io::ErrorKind::WouldBlock => break,
            Err(e) if e.kind() == io::ErrorKind::Interrupted => continue,
            Err(_) => return Err(Error::Stream("Cannot read Moonlight list output".into())),
        }
    }
    Ok(())
}

pub(crate) fn probe(
    path: &Path,
    host: &str,
    timeout: Duration,
    cancel: &AtomicBool,
) -> Result<Output, Error> {
    capture(path, &["list", host], timeout, cancel)
}

pub(crate) fn preflight(path: &Path, timeout: Duration, cancel: &AtomicBool) -> Result<(), Error> {
    let output = capture(path, &["--version"], timeout, cancel)?;
    if !output.success || output.timed_out {
        return Err(Error::Dependency("Moonlight --version failed or timed out; check installation and graphical session before changing plug power".into()));
    }
    Ok(())
}

fn capture(
    path: &Path,
    args: &[&str],
    timeout: Duration,
    cancel: &AtomicBool,
) -> Result<Output, Error> {
    cancelled(cancel)?;
    let deadline = Instant::now() + timeout;
    let child = guarded_command(path)
        .args(args)
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::null())
        .spawn()
        .map_err(|_| Error::Dependency("Cannot launch supervised Moonlight; check executable and Linux pidfd/close_range/fork support".into()))?;
    let mut child = Group(child, Duration::ZERO, true);
    let mut pipe = child.0.stdout.take().expect("piped stdout");
    nonblocking(&pipe)?;
    let mut bytes = Vec::new();
    loop {
        cancelled(cancel)?;
        if Instant::now() >= deadline {
            return Ok(Output {
                success: false,
                stdout: bytes,
                timed_out: true,
            });
        }
        drain(&mut pipe, &mut bytes)?;
        if let Some(status) = child.poll()? {
            drain(&mut pipe, &mut bytes)?;
            return Ok(Output {
                success: status.success(),
                stdout: bytes,
                timed_out: false,
            });
        }
        thread::sleep(
            Duration::from_millis(20).min(deadline.saturating_duration_since(Instant::now())),
        );
    }
}

/// Upstream app/cli/listapps.cpp printApps prints exactly one name per line.
pub(crate) fn has_app(output: &[u8], app: &str) -> Result<bool, Error> {
    let text = std::str::from_utf8(output).map_err(|_| {
        Error::Stream("Moonlight list output is not UTF-8; verify client version/locale".into())
    })?;
    Ok(text.lines().any(|line| line == app))
}

pub(crate) fn stream(
    path: &Path,
    config: &Config,
    cancel: &AtomicBool,
    launched: impl FnOnce(),
) -> Result<(), Error> {
    cancelled(cancel)?;
    let child = guarded_command(path)
        .args(["stream", &config.moonlight.host, &config.moonlight.app])
        .args(&config.moonlight.stream_args)
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .spawn()
        .map_err(|_| Error::Stream("Cannot launch supervised Moonlight; check executable and Linux pidfd/close_range/fork support".into()))?;
    let mut child = Group(child, Duration::from_millis(200), true);
    launched();
    loop {
        cancelled(cancel)?;
        if let Some(status) = child.poll()? {
            return stream_result(status);
        }
        thread::sleep(Duration::from_millis(20));
    }
}

fn stream_result(status: ExitStatus) -> Result<(), Error> {
    if status.success() {
        Ok(())
    } else {
        Err(Error::Stream("Moonlight stream failed; inspect Moonlight directly for diagnostics (plug left unchanged)".into()))
    }
}

pub(crate) fn notify(message: &str) {
    notify_program(Path::new("notify-send"), message);
}

fn notify_program(path: &Path, message: &str) {
    let Ok(child) = guarded_command(path)
        .args(["--", "Moonboot", message])
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .spawn()
    else {
        return;
    };
    let mut child = Group(child, Duration::ZERO, true);
    let deadline = Instant::now() + Duration::from_millis(500);
    while Instant::now() < deadline {
        if matches!(child.poll(), Ok(Some(_)) | Err(_)) {
            break;
        }
        thread::sleep(Duration::from_millis(20));
    }
}

#[cfg(test)]
pub(crate) fn isolated_test(name: &str) -> bool {
    if std::env::var("MOONBOOT_BACKEND_ISOLATED_TEST")
        .ok()
        .as_deref()
        == Some(name)
    {
        return false;
    }
    // Parallel fork/exec tests can briefly inherit another test's write/lock FDs
    // until CLOEXEC runs. Isolate fixtures so ETXTBSY/flock assertions are deterministic.
    let output = Command::new(std::env::current_exe().unwrap())
        .args(["--exact", name, "--nocapture"])
        .env("MOONBOOT_BACKEND_ISOLATED_TEST", name)
        .output()
        .unwrap();
    assert!(
        output.status.success(),
        "isolated test {name} failed: {} {}",
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    );
    true
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::{atomic::Ordering, Arc};

    fn script(contents: &str) -> (tempfile::TempDir, PathBuf) {
        let directory = tempfile::tempdir().unwrap();
        let path = directory.path().join("fake-moonlight");
        let shell = executable("sh").expect("backend process tests require a POSIX shell on PATH");
        fs::write(&path, format!("#!{}\n{contents}\n", shell.display())).unwrap();
        fs::set_permissions(&path, fs::Permissions::from_mode(0o700)).unwrap();
        (directory, path)
    }

    #[test]
    fn fixtures_match_exact_lines_not_substrings_or_csv() {
        assert!(has_app(b"Steam\nDesktop With Spaces\n", "Desktop With Spaces").unwrap());
        assert!(has_app(b"Desktop\r\n", "Desktop").unwrap());
        assert!(!has_app(b"Other Desktop\nDesktop Extra\n", "Desktop").unwrap());
        assert!(!has_app(b"\"Desktop\",1,true\n", "Desktop").unwrap());
        assert!(has_app(b"\xff", "Desktop").is_err());
    }

    #[test]
    fn hung_probe_kills_group_reaps_leader_and_is_bounded() {
        if isolated_test("process::tests::hung_probe_kills_group_reaps_leader_and_is_bounded") {
            return;
        }
        let (_directory, path) = script("sleep 30 &\nprintf '%s\\n%s\\n' \"$$\" \"$!\"\nwait");
        let begin = Instant::now();
        // Allow cold fork/exec under parallel builds before asserting emitted PIDs.
        let output = probe(
            &path,
            "fake",
            Duration::from_secs(1),
            &AtomicBool::new(false),
        )
        .unwrap();
        assert!(output.timed_out);
        assert!(!output.success);
        assert!(begin.elapsed() < Duration::from_secs(2));
        let ids: Vec<u32> = String::from_utf8(output.stdout)
            .unwrap()
            .lines()
            .map(|s| s.parse().unwrap())
            .collect();
        assert_eq!(ids.len(), 2);
        assert!(
            !PathBuf::from(format!("/proc/{}", ids[0])).exists(),
            "leader must be reaped"
        );
        let deadline = Instant::now() + Duration::from_secs(1);
        loop {
            let stat = fs::read_to_string(format!("/proc/{}/stat", ids[1]));
            if stat.is_err()
                || stat.as_ref().is_ok_and(|s| {
                    s.split_once(") ")
                        .is_some_and(|(_, fields)| fields.starts_with('Z'))
                })
            {
                break;
            }
            assert!(Instant::now() < deadline, "descendant still executing");
            thread::sleep(Duration::from_millis(10));
        }
    }

    #[test]
    fn dependency_preflight_is_bounded_and_must_succeed() {
        if isolated_test("process::tests::dependency_preflight_is_bounded_and_must_succeed") {
            return;
        }
        let (_directory, path) =
            script("[ \"$1\" = '--version' ] || exit 1; printf 'Moonlight fake\\n'");
        preflight(&path, Duration::from_secs(1), &AtomicBool::new(false)).unwrap();
        let (_directory, path) = script("exit 1");
        assert!(matches!(
            preflight(&path, Duration::from_secs(1), &AtomicBool::new(false)),
            Err(Error::Dependency(_))
        ));
        let (_directory, path) = script("sleep 30");
        assert!(matches!(
            preflight(&path, Duration::from_millis(30), &AtomicBool::new(false)),
            Err(Error::Dependency(_))
        ));
    }

    #[test]
    fn successful_notifications_deliver_arguments_from_worker_and_path() {
        if isolated_test(
            "process::tests::successful_notifications_deliver_arguments_from_worker_and_path",
        ) {
            return;
        }
        let (directory, path) = script("printf '%s\\n' \"$@\" > \"$0.delivered\"");
        let notifier = directory.path().join("notify-send");
        fs::rename(path, &notifier).unwrap();
        let delivered = directory.path().join("notify-send.delivered");
        let message = "DEMO: simulated cloud failure; $(printf injected)";
        let expected = format!("--\nMoonboot\n{message}\n");

        notify_program(&notifier, message);
        assert_eq!(fs::read_to_string(&delivered).unwrap(), expected);
        fs::remove_file(&delivered).unwrap();

        // Only this isolated test process changes PATH; the shebang is absolute.
        let previous_path = std::env::var_os("PATH");
        std::env::set_var("PATH", directory.path());
        thread::spawn(move || notify(message)).join().unwrap();
        match previous_path {
            Some(path) => std::env::set_var("PATH", path),
            None => std::env::remove_var("PATH"),
        }
        assert_eq!(fs::read_to_string(&delivered).unwrap(), expected);
    }

    #[test]
    fn missing_failing_and_hung_notifications_are_best_effort() {
        if isolated_test("process::tests::missing_failing_and_hung_notifications_are_best_effort") {
            return;
        }
        let directory = tempfile::tempdir().unwrap();
        notify_program(
            &directory.path().join("missing-notifier"),
            "synthetic notification",
        );
        let (_directory, path) = script("exit 1");
        notify_program(&path, "synthetic notification");
        let (_directory, path) = script("sleep 30");
        let begin = Instant::now();
        notify_program(&path, "synthetic notification");
        assert!(begin.elapsed() < Duration::from_secs(2));
    }

    #[test]
    fn cancellation_stops_probe_and_supervised_stream() {
        if isolated_test("process::tests::cancellation_stops_probe_and_supervised_stream") {
            return;
        }
        for streaming in [false, true] {
            let (_directory, path) = script("sleep 30 &\nwait");
            let cancel = Arc::new(AtomicBool::new(false));
            let trigger = cancel.clone();
            let thread = thread::spawn(move || {
                thread::sleep(Duration::from_millis(80));
                trigger.store(true, Ordering::Relaxed);
            });
            let begin = Instant::now();
            let result = if streaming {
                stream(&path, &test_config(), &cancel, || {})
            } else {
                probe(&path, "fake", Duration::from_secs(30), &cancel).map(|_| ())
            };
            assert!(matches!(result, Err(Error::Cancelled)));
            assert!(begin.elapsed() < Duration::from_secs(2));
            thread.join().unwrap();
        }
    }

    fn test_config() -> Config {
        use crate::config::{Moonlight, Notifications, Startup, Tuya};
        Config {
            tuya: Tuya {
                endpoint: "https://example.invalid".into(),
                device_id: "fake".into(),
                switch_code: "switch".into(),
                credentials_file: "/never-read".into(),
            },
            moonlight: Moonlight {
                executable: "unused".into(),
                host: "fake host".into(),
                app: "Desktop With Spaces".into(),
                stream_args: vec!["--example".into(), "$(touch sentinel); argument".into()],
            },
            startup: Startup {
                timeout_seconds: 5,
                poll_interval_seconds: 1,
                probe_timeout_seconds: 1,
                http_timeout_seconds: 1,
            },
            notifications: Notifications { enabled: false },
        }
    }

    #[test]
    fn streaming_preserves_arguments_without_shell_interpretation() {
        if isolated_test(
            "process::tests::streaming_preserves_arguments_without_shell_interpretation",
        ) {
            return;
        }
        let (_directory, path) = script("[ \"$#\" = 5 ] && [ \"$1\" = stream ] && [ \"$2\" = 'fake host' ] && [ \"$3\" = 'Desktop With Spaces' ] && [ \"$4\" = '--example' ] && [ \"$5\" = '$(touch sentinel); argument' ]");
        stream(&path, &test_config(), &AtomicBool::new(false), || {}).unwrap();
    }

    #[test]
    fn bounded_output_nonzero_and_stream_failure_are_not_leaked() {
        if isolated_test("process::tests::bounded_output_nonzero_and_stream_failure_are_not_leaked")
        {
            return;
        }
        let (_directory, path) = script("while :; do printf 'SECRET SECRET SECRET SECRET SECRET SECRET SECRET SECRET SECRET\\n'; done");
        let error = probe(
            &path,
            "fake",
            Duration::from_secs(2),
            &AtomicBool::new(false),
        )
        .err()
        .unwrap();
        assert!(!error.to_string().contains("SECRET"));
        let (_directory, path) = script("printf 'SECRET\\n' >&2; exit 1");
        let output = probe(
            &path,
            "fake",
            Duration::from_secs(1),
            &AtomicBool::new(false),
        )
        .unwrap();
        assert!(!output.success);
        assert!(output.stdout.is_empty());
        let error = stream(&path, &test_config(), &AtomicBool::new(false), || {}).unwrap_err();
        assert!(!error.to_string().contains("SECRET"));
    }
}
