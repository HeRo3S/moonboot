use super::*;
use crate::config::{Moonlight, Notifications, Startup, Tuya};
use std::{
    cell::RefCell,
    collections::VecDeque,
    os::unix::fs::{symlink, PermissionsExt},
    os::unix::process::CommandExt,
};

fn config() -> Config {
    Config {
        tuya: Tuya {
            endpoint: "https://example.invalid".into(),
            device_id: "fake-device".into(),
            switch_code: "switch_1".into(),
            credentials_file: Some("/not-read-in-tests".into()),
            ..Tuya::default()
        },
        moonlight: Moonlight {
            executable: "fake-moonlight".into(),
            host: "fake-host".into(),
            app: "Desktop With Spaces".into(),
            stream_args: vec![],
        },
        startup: Startup {
            timeout_seconds: 5,
            poll_interval_seconds: 3,
            probe_timeout_seconds: 4,
            http_timeout_seconds: 2,
        },
        notifications: Notifications { enabled: false },
        ..Config::default()
    }
}

struct FakePlug {
    on: bool,
    commands: Vec<bool>,
    calls: Vec<&'static str>,
    validation_fails: bool,
    ambiguous: bool,
    apply_command: bool,
}
impl FakePlug {
    fn new(on: bool) -> Self {
        Self {
            on,
            commands: vec![],
            calls: vec![],
            validation_fails: false,
            ambiguous: false,
            apply_command: true,
        }
    }
}
impl Plug for FakePlug {
    fn validate(&mut self, _: Duration, _: &AtomicBool) -> Result<(), Error> {
        self.calls.push("validate");
        if self.validation_fails {
            Err(Error::Cloud("offline or unsupported switch".into()))
        } else {
            Ok(())
        }
    }
    fn status(&mut self, _: Duration, _: &AtomicBool) -> Result<bool, Error> {
        self.calls.push("status");
        Ok(self.on)
    }
    fn command(&mut self, on: bool, _: Duration, _: &AtomicBool) -> Result<(), Error> {
        self.calls.push("command");
        self.commands.push(on);
        if self.apply_command {
            self.on = on;
        }
        if self.ambiguous {
            Err(Error::Cloud("ambiguous response".into()))
        } else {
            Ok(())
        }
    }
}

#[derive(Default)]
struct FakeRuntime {
    time: Duration,
    budgets: Vec<Duration>,
    sleeps: Vec<Duration>,
    probe_results: VecDeque<(bool, &'static [u8])>,
    hang: bool,
    streams: usize,
    cancel_probe: bool,
}
impl Runtime for FakeRuntime {
    fn now(&self) -> Duration {
        self.time
    }
    fn sleep(&mut self, duration: Duration, cancel: &AtomicBool) -> Result<(), Error> {
        cancelled(cancel)?;
        self.sleeps.push(duration);
        self.time += duration;
        Ok(())
    }
    fn probe(&mut self, timeout: Duration, cancel: &AtomicBool) -> Result<process::Output, Error> {
        self.budgets.push(timeout);
        if self.cancel_probe {
            cancel.store(true, Ordering::Relaxed);
        }
        if self.hang {
            self.time += timeout;
        }
        let (success, bytes) = self.probe_results.pop_front().unwrap_or((false, b""));
        Ok(process::Output {
            success,
            stdout: bytes.to_vec(),
            timed_out: self.hang,
        })
    }
    fn stream(&mut self, cancel: &AtomicBool, emit: &dyn Fn(Event)) -> Result<(), Error> {
        cancelled(cancel)?;
        self.streams += 1;
        emit(Event::Phase(Phase::Streaming));
        Ok(())
    }
    fn notify(&mut self, _: &str) { /* Failing/missing delivery is deliberately nonfatal. */
    }
}

fn ready() -> FakeRuntime {
    FakeRuntime {
        probe_results: [(true, b"Steam\nDesktop With Spaces\n".as_slice())].into(),
        ..Default::default()
    }
}

#[test]
fn cold_start_runs_actual_workflow_and_never_off() {
    let mut plug = FakePlug::new(false);
    let mut runtime = ready();
    runtime.probe_results.push_front((false, b"ignored error"));
    let events = RefCell::new(Vec::new());
    workflow(
        &config(),
        Operation::Start,
        &AtomicBool::new(false),
        &|e| events.borrow_mut().push(e),
        || panic!("start cannot request approval"),
        &mut plug,
        &mut runtime,
    )
    .unwrap();
    assert_eq!(plug.commands, [true]);
    assert_eq!(plug.calls, ["validate", "status", "command", "status"]);
    assert_eq!(runtime.streams, 1);
    assert_eq!(
        runtime.budgets,
        [Duration::from_secs(4), Duration::from_secs(2)]
    );
    assert!(events.borrow().contains(&Event::Phase(Phase::Streaming)));
}

#[test]
fn already_on_starts_without_command() {
    let mut plug = FakePlug::new(true);
    let mut runtime = ready();
    workflow(
        &config(),
        Operation::Start,
        &AtomicBool::new(false),
        &|_| {},
        || panic!(),
        &mut plug,
        &mut runtime,
    )
    .unwrap();
    assert!(plug.commands.is_empty());
    assert_eq!(runtime.streams, 1);
}

#[test]
fn reads_never_probe_stream_command_or_approve() {
    for operation in [Operation::Check, Operation::Refresh] {
        for on in [false, true] {
            let mut plug = FakePlug::new(on);
            let mut runtime = ready();
            workflow(
                &config(),
                operation,
                &AtomicBool::new(false),
                &|_| {},
                || panic!(),
                &mut plug,
                &mut runtime,
            )
            .unwrap();
            assert!(plug.commands.is_empty());
            assert!(runtime.budgets.is_empty());
            assert_eq!(runtime.streams, 0);
        }
    }
}

#[test]
fn invalid_or_offline_plug_prevents_approval_and_power() {
    for operation in [Operation::Start, Operation::PlugOff] {
        let mut plug = FakePlug::new(true);
        plug.validation_fails = true;
        let mut runtime = ready();
        assert!(workflow(
            &config(),
            operation,
            &AtomicBool::new(false),
            &|_| {},
            || panic!(),
            &mut plug,
            &mut runtime
        )
        .is_err());
        assert!(plug.commands.is_empty());
        assert_eq!(runtime.streams, 0);
    }
}

#[test]
fn approval_only_after_validation_and_status() {
    let mut plug = FakePlug::new(true);
    let mut runtime = ready();
    let events = RefCell::new(vec![]);
    workflow(
        &config(),
        Operation::PlugOff,
        &AtomicBool::new(false),
        &|e| events.borrow_mut().push(e),
        || {
            assert_eq!(
                *events.borrow(),
                [Event::Status(true), Event::Phase(Phase::ConfirmingOff)]
            );
            true
        },
        &mut plug,
        &mut runtime,
    )
    .unwrap();
    assert_eq!(plug.commands, [false]);
    assert_eq!(plug.calls, ["validate", "status", "command", "status"]);
    assert_eq!(runtime.streams, 0);
}

#[test]
fn refusal_close_eof_and_wrong_phrase_have_no_authorization() {
    // The frontend maps all these outcomes to false; the backend never interprets text.
    for _reason in [
        "refused",
        "dialog closed",
        "EOF",
        "wrong phrase",
        "noninteractive",
    ] {
        let mut plug = FakePlug::new(true);
        let mut runtime = ready();
        assert!(matches!(
            workflow(
                &config(),
                Operation::PlugOff,
                &AtomicBool::new(false),
                &|_| {},
                || false,
                &mut plug,
                &mut runtime
            ),
            Err(Error::ApprovalDeclined)
        ));
        assert!(plug.commands.is_empty());
    }
}

#[test]
fn already_off_does_not_even_ask_for_approval() {
    let mut plug = FakePlug::new(false);
    let mut runtime = ready();
    workflow(
        &config(),
        Operation::PlugOff,
        &AtomicBool::new(false),
        &|_| {},
        || panic!(),
        &mut plug,
        &mut runtime,
    )
    .unwrap();
    assert!(plug.commands.is_empty());
}

#[test]
fn cancellation_during_approval_overrides_true() {
    let cancel = AtomicBool::new(false);
    let mut plug = FakePlug::new(true);
    let mut runtime = ready();
    assert!(matches!(
        workflow(
            &config(),
            Operation::PlugOff,
            &cancel,
            &|_| {},
            || {
                cancel.store(true, Ordering::Relaxed);
                true
            },
            &mut plug,
            &mut runtime
        ),
        Err(Error::Cancelled)
    ));
    assert!(plug.commands.is_empty());
}

#[test]
fn ambiguous_commands_are_queried_not_resent() {
    for operation in [Operation::Start, Operation::PlugOff] {
        let mut plug = FakePlug::new(operation == Operation::PlugOff);
        plug.ambiguous = true;
        let mut runtime = ready();
        workflow(
            &config(),
            operation,
            &AtomicBool::new(false),
            &|_| {},
            || true,
            &mut plug,
            &mut runtime,
        )
        .unwrap();
        assert_eq!(plug.commands, [operation == Operation::Start]);
        assert_eq!(plug.calls, ["validate", "status", "command", "status"]);
    }
}

#[test]
fn unconfirmed_power_has_bounded_wait_and_no_stream_or_retry() {
    for operation in [Operation::Start, Operation::PlugOff] {
        let mut plug = FakePlug::new(operation == Operation::PlugOff);
        plug.apply_command = false;
        plug.ambiguous = true;
        let mut runtime = ready();
        assert!(matches!(
            workflow(
                &config(),
                operation,
                &AtomicBool::new(false),
                &|_| {},
                || true,
                &mut plug,
                &mut runtime
            ),
            Err(Error::Cloud(_))
        ));
        assert_eq!(runtime.time, Duration::from_secs(20));
        assert_eq!(plug.commands.len(), 1);
        assert_eq!(runtime.streams, 0);
    }
}

#[test]
fn hung_probe_deadline_and_sleep_are_clamped() {
    let mut plug = FakePlug::new(true);
    let mut runtime = FakeRuntime {
        hang: true,
        ..Default::default()
    };
    assert!(matches!(
        workflow(
            &config(),
            Operation::Start,
            &AtomicBool::new(false),
            &|_| {},
            || panic!(),
            &mut plug,
            &mut runtime
        ),
        Err(Error::ReadinessTimeout)
    ));
    assert_eq!(runtime.time, Duration::from_secs(5));
    assert_eq!(runtime.budgets, [Duration::from_secs(4)]);
    assert_eq!(runtime.sleeps, [Duration::from_secs(1)]);
    assert!(plug.commands.is_empty());
    assert_eq!(runtime.streams, 0);
}

#[test]
fn app_mismatch_is_permanent_but_nonzero_is_not_text_classified() {
    let mut plug = FakePlug::new(true);
    let mut runtime = FakeRuntime {
        probe_results: [(true, b"Desktop With Spaces Extra\n".as_slice())].into(),
        ..Default::default()
    };
    assert!(matches!(
        workflow(
            &config(),
            Operation::Start,
            &AtomicBool::new(false),
            &|_| {},
            || panic!(),
            &mut plug,
            &mut runtime
        ),
        Err(Error::Stream(_))
    ));
    assert_eq!(runtime.budgets.len(), 1);
    assert_eq!(runtime.streams, 0);
    let mut runtime = FakeRuntime {
        probe_results: [(false, b"not paired: localized/unstable".as_slice())].into(),
        ..Default::default()
    };
    assert!(matches!(
        workflow(
            &config(),
            Operation::Start,
            &AtomicBool::new(false),
            &|_| {},
            || panic!(),
            &mut plug,
            &mut runtime
        ),
        Err(Error::ReadinessTimeout)
    ));
}

#[test]
fn probe_cancellation_never_off_or_stream() {
    let mut plug = FakePlug::new(false);
    let mut runtime = FakeRuntime {
        cancel_probe: true,
        ..ready()
    };
    assert!(matches!(
        workflow(
            &config(),
            Operation::Start,
            &AtomicBool::new(false),
            &|_| {},
            || panic!(),
            &mut plug,
            &mut runtime
        ),
        Err(Error::Cancelled)
    ));
    assert_eq!(plug.commands, [true]);
    assert_eq!(runtime.streams, 0);
}

#[test]
fn lock_exclusion_release_namespace_and_symlink_safety() {
    if process::isolated_test("backend::tests::lock_exclusion_release_namespace_and_symlink_safety")
    {
        return;
    }
    let directory = tempfile::tempdir().unwrap();
    std::fs::set_permissions(directory.path(), std::fs::Permissions::from_mode(0o700)).unwrap();
    let first = lock_at(directory.path(), "operation").unwrap();
    assert!(matches!(
        lock_at(directory.path(), "operation"),
        Err(Error::Busy)
    ));
    let demo = lock_at(directory.path(), "demo").unwrap();
    drop(demo);
    drop(first);
    drop(lock_at(directory.path(), "operation").unwrap());
    symlink(
        "moonboot-operation.lock",
        directory.path().join("moonboot-evil.lock"),
    )
    .unwrap();
    assert!(lock_at(directory.path(), "evil").is_err());
    assert!(lock_at(directory.path(), "../escape").is_err());
}

#[test]
fn public_codes_are_stable() {
    assert_eq!(Error::Config("".into()).code(), 2);
    assert_eq!(Error::Dependency("".into()).code(), 2);
    assert_eq!(Error::Cloud("".into()).code(), 3);
    assert_eq!(Error::ReadinessTimeout.code(), 4);
    assert_eq!(Error::Busy.code(), 5);
    assert_eq!(Error::Stream("".into()).code(), 6);
    assert_eq!(Error::ApprovalDeclined.code(), 7);
    assert_eq!(Error::Cancelled.code(), 130);
}

#[test]
fn private_log_is_bounded_private_and_rejects_symlinks() {
    let directory = tempfile::tempdir().unwrap();
    let log_dir = directory.path().join("state");
    for _ in 0..200 {
        write_private_log(&log_dir, &Error::ReadinessTimeout).unwrap();
    }
    let path = log_dir.join("backend.log");
    let metadata = std::fs::metadata(&path).unwrap();
    assert!(metadata.len() <= 16384);
    assert_eq!(metadata.mode() & 0o777, 0o600);
    assert_eq!(std::fs::metadata(&log_dir).unwrap().mode() & 0o777, 0o700);
    std::fs::remove_file(&path).unwrap();
    let sentinel = directory.path().join("sentinel");
    std::fs::write(&sentinel, "unchanged").unwrap();
    symlink(&sentinel, &path).unwrap();
    assert!(write_private_log(&log_dir, &Error::ReadinessTimeout).is_err());
    assert_eq!(std::fs::read_to_string(sentinel).unwrap(), "unchanged");
}

#[test]
fn public_run_child_boundary() {
    let Ok(case) = std::env::var("MOONBOOT_BACKEND_TEST_CHILD") else {
        return;
    };
    let mut config = config();
    config.moonlight.executable = "/definitely-missing-test-moonlight".into();
    config.tuya.credentials_file = Some(
        PathBuf::from(std::env::var_os("XDG_STATE_HOME").unwrap()).join("missing-test-credentials"),
    );
    let events = RefCell::new(vec![]);
    let operation = if case == "missing-credentials" {
        Operation::PlugOff
    } else {
        Operation::Start
    };
    let error = run(
        &config,
        operation,
        Arc::new(AtomicBool::new(false)),
        |event| events.borrow_mut().push(event),
        || panic!("no validation/status, so approval is forbidden"),
    )
    .unwrap_err();
    match case.as_str() {
        "busy" => assert!(matches!(error, Error::Busy)),
        "missing-runtime" => assert!(matches!(error, Error::Config(_))),
        "preflight" => assert!(matches!(error, Error::Dependency(_))),
        "missing-credentials" => assert!(
            matches!(error, Error::Config(_)),
            "PlugOff must not require Moonlight: {error}"
        ),
        _ => panic!("unknown test case"),
    }
    assert_eq!(*events.borrow(), [Event::Phase(Phase::Idle)]);
}

#[test]
fn public_run_lock_preflight_and_plugoff_boundaries_in_isolated_processes() {
    let directory = tempfile::tempdir().unwrap();
    std::fs::set_permissions(directory.path(), std::fs::Permissions::from_mode(0o700)).unwrap();
    for case in [
        "busy",
        "missing-runtime",
        "preflight",
        "missing-credentials",
    ] {
        let lock = if case == "busy" {
            Some(lock_at(directory.path(), "operation").unwrap())
        } else {
            None
        };
        let mut child = std::process::Command::new(std::env::current_exe().unwrap());
        child
            .args([
                "--exact",
                "backend::tests::public_run_child_boundary",
                "--nocapture",
            ])
            .env("MOONBOOT_BACKEND_TEST_CHILD", case)
            .env("XDG_RUNTIME_DIR", directory.path())
            .env("XDG_STATE_HOME", directory.path().join("state"));
        if case == "missing-runtime" {
            child.env_remove("XDG_RUNTIME_DIR");
        }
        let output = child.output().unwrap();
        assert!(
            output.status.success(),
            "isolated case {case} failed: {} {}",
            String::from_utf8_lossy(&output.stdout),
            String::from_utf8_lossy(&output.stderr)
        );
        drop(lock);
    }
}

#[test]
fn abnormal_death_stream_supervisor_child() {
    let Some(root) = std::env::var_os("MOONBOOT_TEST_DEATH_ROOT").map(PathBuf::from) else {
        return;
    };
    let _lock = lock_at(&root, "operation").unwrap();
    let mut config = config();
    config.moonlight.host = root.to_str().unwrap().into();
    process::stream(
        &root.join("fake-moonlight"),
        &config,
        &AtomicBool::new(false),
        || {},
    )
    .unwrap();
}

#[test]
fn supervisor_sigkill_stops_entire_stream_group_and_releases_lock() {
    if process::isolated_test(
        "backend::tests::supervisor_sigkill_stops_entire_stream_group_and_releases_lock",
    ) {
        return;
    }
    // Adopt/reap the fixture's orphaned processes only in this isolated test process.
    assert_eq!(
        unsafe { libc::prctl(libc::PR_SET_CHILD_SUBREAPER, 1, 0, 0, 0) },
        0
    );
    struct Fixture {
        supervisor: std::process::Child,
        sentinel: std::process::Child,
        group: Option<i32>,
    }
    impl Drop for Fixture {
        fn drop(&mut self) {
            // The test does not reap the leader while this fallback still owns its PGID.
            if let Some(group) = self.group {
                unsafe {
                    libc::kill(-group, libc::SIGKILL);
                }
            }
            let _ = self.supervisor.kill();
            let _ = self.supervisor.wait();
            let _ = self.sentinel.kill();
            let _ = self.sentinel.wait();
        }
    }
    fn stopped(pid: i32) -> bool {
        match std::fs::read_to_string(format!("/proc/{pid}/stat")) {
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => true,
            Ok(stat) => stat
                .rsplit_once(") ")
                .is_some_and(|(_, fields)| fields.starts_with('Z') || fields.starts_with('X')),
            Err(error) => panic!("Cannot inspect fixture: {error}"),
        }
    }
    let directory = tempfile::tempdir().unwrap();
    std::fs::set_permissions(directory.path(), std::fs::Permissions::from_mode(0o700)).unwrap();
    let shell = process::executable("sh").unwrap();
    let script = directory.path().join("fake-moonlight");
    std::fs::write(&script, format!("#!{}\ntrap '' TERM\nroot=\"$2\"\nprintf '%s\\n' \"$$\" > \"$root/leader.pid\"\nsh -c 'trap \"\" TERM; printf \"%s\\n\" \"$$\" > \"$1/descendant.pid\"; count=0; while [ \"$count\" -lt 1000 ]; do printf x >> \"$1/heartbeat\"; sleep 0.01; count=$((count+1)); done' _ \"$root\" &\nwait\n", shell.display())).unwrap();
    std::fs::set_permissions(&script, std::fs::Permissions::from_mode(0o700)).unwrap();
    let sentinel = std::process::Command::new(process::executable("sleep").unwrap())
        .arg("60")
        .stdin(std::process::Stdio::null())
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::null())
        .process_group(0)
        .spawn()
        .unwrap();
    let supervisor = std::process::Command::new(std::env::current_exe().unwrap())
        .args([
            "--exact",
            "backend::tests::abnormal_death_stream_supervisor_child",
            "--nocapture",
        ])
        .env("MOONBOOT_TEST_DEATH_ROOT", directory.path())
        .stdin(std::process::Stdio::null())
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::null())
        .spawn()
        .unwrap();
    let mut fixture = Fixture {
        supervisor,
        sentinel,
        group: None,
    };
    let deadline = Instant::now() + Duration::from_secs(3);
    let (leader, descendant) = loop {
        let leader = std::fs::read_to_string(directory.path().join("leader.pid"))
            .ok()
            .and_then(|text| text.trim().parse::<i32>().ok());
        if let Some(leader) = leader {
            assert!(leader > 0);
            fixture.group = Some(leader);
        }
        let descendant = std::fs::read_to_string(directory.path().join("descendant.pid"))
            .ok()
            .and_then(|text| text.trim().parse::<i32>().ok());
        if let (Some(leader), Some(descendant)) = (leader, descendant) {
            if std::fs::metadata(directory.path().join("heartbeat")).is_ok_and(|m| m.len() > 0) {
                break (leader, descendant);
            }
        }
        assert!(
            fixture.supervisor.try_wait().unwrap().is_none(),
            "fixture supervisor failed to launch"
        );
        assert!(Instant::now() < deadline, "stream fixture did not start");
        thread::sleep(Duration::from_millis(5));
    };
    let children =
        std::fs::read_to_string(format!("/proc/{leader}/task/{leader}/children")).unwrap();
    let guardians: Vec<i32> = children
        .split_whitespace()
        .map(|pid| pid.parse().unwrap())
        .filter(|&pid| pid != descendant)
        .collect();
    assert_eq!(
        guardians.len(),
        1,
        "one guardian, separate from the fake descendant"
    );
    let guardian = guardians[0];
    for pid in [leader, descendant, guardian] {
        assert_eq!(unsafe { libc::getpgid(pid) }, leader);
    }
    let descriptors: Vec<_> = std::fs::read_dir(format!("/proc/{guardian}/fd"))
        .unwrap()
        .map(|entry| std::fs::read_link(entry.unwrap().path()).unwrap())
        .collect();
    assert_eq!(
        descriptors,
        [
            PathBuf::from("anon_inode:[pidfd]"),
            PathBuf::from("anon_inode:[pidfd]")
        ]
    );
    let status = std::fs::read_to_string(format!("/proc/{leader}/status")).unwrap();
    let blocked = u64::from_str_radix(
        status
            .lines()
            .find_map(|line| line.strip_prefix("SigBlk:\t"))
            .unwrap()
            .trim(),
        16,
    )
    .unwrap();
    assert_eq!(
        blocked & ((1 << (libc::SIGTERM - 1)) | (1 << (libc::SIGINT - 1))),
        0,
        "client signal mask must be restored"
    );
    assert!(matches!(
        lock_at(directory.path(), "operation"),
        Err(Error::Busy)
    ));
    let killed = Instant::now();
    fixture.supervisor.kill().unwrap();
    assert!(!fixture.supervisor.wait().unwrap().success());
    while ![leader, descendant, guardian].into_iter().all(stopped) {
        assert!(
            killed.elapsed() < Duration::from_secs(2),
            "owned stream group survived supervisor SIGKILL"
        );
        thread::sleep(Duration::from_millis(5));
    }
    let heartbeat = directory.path().join("heartbeat");
    let length = std::fs::metadata(&heartbeat).unwrap().len();
    thread::sleep(Duration::from_millis(100));
    assert_eq!(
        std::fs::metadata(heartbeat).unwrap().len(),
        length,
        "descendant must not keep executing"
    );
    assert!(
        fixture.sentinel.try_wait().unwrap().is_none(),
        "unrelated process must not be killed"
    );
    drop(lock_at(directory.path(), "operation").unwrap());
    fixture.group = None;
    fixture.sentinel.kill().unwrap();
    fixture.sentinel.wait().unwrap();
    // A failed exec must also release spawn's error pipe and terminate its guardian.
    assert!(matches!(
        process::probe(
            &directory.path().join("missing-executable"),
            "fake",
            Duration::from_millis(100),
            &AtomicBool::new(false)
        ),
        Err(Error::Dependency(_))
    ));
    // Reap the leader, guardian, shell descendant and its transient sleep child.
    loop {
        let result = unsafe { libc::waitpid(-1, std::ptr::null_mut(), libc::WNOHANG) };
        if result > 0 {
            continue;
        }
        if result < 0 {
            let error = std::io::Error::last_os_error();
            if error.raw_os_error() == Some(libc::ECHILD) {
                break;
            }
            assert_eq!(error.raw_os_error(), Some(libc::EINTR));
        }
        assert!(
            killed.elapsed() < Duration::from_secs(2),
            "fixture descendants were not reaped"
        );
        thread::sleep(Duration::from_millis(5));
    }
}
