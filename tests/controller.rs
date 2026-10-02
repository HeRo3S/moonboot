use moonboot::{
    backend::{self, Error, Operation, Phase},
    controller::{self, Instance, Request, Session},
    demo::Scenario,
    ui::Action,
};
use std::{
    fs::{self, File},
    io::Write,
    os::{
        fd::{AsRawFd, FromRawFd},
        unix::{
            fs::{MetadataExt, PermissionsExt},
            net::{UnixListener, UnixStream},
            process::CommandExt,
        },
    },
    path::Path,
    process::{Child, Command, Stdio},
    sync::{atomic::AtomicBool, Arc},
    thread,
    time::{Duration, Instant},
};

fn wait(child: &mut Child) -> std::process::ExitStatus {
    let deadline = Instant::now() + Duration::from_secs(12);
    loop {
        if let Some(status) = child.try_wait().unwrap() {
            return status;
        }
        if Instant::now() >= deadline {
            let _ = child.kill();
            let _ = child.wait();
            panic!("test child did not exit");
        }
        thread::sleep(Duration::from_millis(10));
    }
}

fn child(mode: &str, directory: &Path) -> Command {
    let mut command = Command::new(std::env::current_exe().unwrap());
    command
        .args([
            "--exact",
            "isolated_child",
            "--nocapture",
            "--test-threads=1",
        ])
        .env("MOONBOOT_TEST_CHILD", mode)
        .env("XDG_RUNTIME_DIR", directory)
        .env("HOME", directory)
        .env("XDG_CONFIG_HOME", directory)
        .env("XDG_STATE_HOME", directory)
        .env("PATH", "/nonexistent-test-path");
    command
}

#[test]
fn isolated_child() {
    let Ok(mode) = std::env::var("MOONBOOT_TEST_CHILD") else {
        return;
    };
    if mode == "prompt" {
        let cancel = controller::signal_flag().unwrap();
        fs::write(std::env::var_os("MOONBOOT_PROMPT_READY").unwrap(), "ready").unwrap();
        let approved = controller::terminal_approval("synthetic-host", "synthetic-plug", &cancel);
        assert_eq!(
            approved,
            std::env::var("MOONBOOT_EXPECT_APPROVAL").unwrap() == "true"
        );
        return;
    }
    if mode == "tray" {
        tray_child();
        return;
    }
    if mode == "tray-recovery" {
        tray_recovery_child();
        return;
    }
    assert_eq!(mode, "controller");
    let instance = Arc::new(Instance::claim(None).unwrap().unwrap());
    let demo_instance = Instance::claim(Some(Scenario::Success)).unwrap().unwrap();
    assert_ne!(
        controller::namespace(None, "ui"),
        controller::namespace(Some(Scenario::Success), "ui")
    );
    let received = instance.clone();
    let server = thread::spawn(move || {
        let deadline = Instant::now() + Duration::from_secs(5);
        let mut requests = Vec::new();
        while Instant::now() < deadline && requests.len() < 2 {
            if let Some(request) = received.receive() {
                requests.push(request);
            }
            thread::sleep(Duration::from_millis(5));
        }
        requests
    });
    let output = Command::new(env!("CARGO_BIN_EXE_moonboot"))
        .args(["gui", "--config", "/missing-config-that-must-not-be-read"])
        .output()
        .unwrap();
    assert!(output.status.success(), "{:?}", output.stderr);
    let path = Path::new(&std::env::var("XDG_RUNTIME_DIR").unwrap()).join("moonboot-ui.sock");
    for forbidden in *b"SFP\n" {
        let mut stream = UnixStream::connect(&path).unwrap();
        stream.write_all(&[forbidden]).unwrap();
    }
    assert!(controller::request(None, Request::Quit).unwrap());
    assert_eq!(server.join().unwrap(), [Request::Open, Request::Quit]);
    drop(instance);
    drop(demo_instance);
    assert!(!path.exists());
    assert!(Path::new(&std::env::var("XDG_RUNTIME_DIR").unwrap())
        .join("moonboot-ui.lock")
        .exists());
    fs::write(&path, "must-not-unlink").unwrap();
    assert!(Instance::claim(None).is_err());
    assert_eq!(fs::read_to_string(&path).unwrap(), "must-not-unlink");
    fs::remove_file(&path).unwrap();
    let target = path.with_extension("sentinel");
    fs::write(&target, "keep").unwrap();
    std::os::unix::fs::symlink(&target, &path).unwrap();
    assert!(Instance::claim(None).is_err());
    assert_eq!(fs::read_to_string(&target).unwrap(), "keep");
    fs::remove_file(&path).unwrap();

    let stale = UnixListener::bind(&path).unwrap();
    fs::set_permissions(&path, fs::Permissions::from_mode(0o666)).unwrap();
    assert!(Instance::claim(None).is_err());
    fs::set_permissions(&path, fs::Permissions::from_mode(0o600)).unwrap();
    drop(stale);
    drop(Instance::claim(None).unwrap().unwrap());

    let state = Path::new(&std::env::var("XDG_STATE_HOME").unwrap()).join("demo-success-moonboot");
    for _ in 0..200 {
        controller::report_failure(
            &Error::Config("DEMO: synthetic failure repeated to test bounded logging".repeat(5)),
            Some(Scenario::Success),
        );
    }
    let log = state.join("frontend.log");
    let metadata = fs::metadata(&log).unwrap();
    assert!(metadata.len() <= 16384);
    assert_eq!(metadata.mode() & 0o777, 0o600);
    fs::remove_file(&log).unwrap();
    std::os::unix::fs::symlink(&target, &log).unwrap();
    controller::report_failure(
        &Error::Config("DEMO: do not follow log symlinks".into()),
        Some(Scenario::Success),
    );
    assert_eq!(fs::read_to_string(&target).unwrap(), "keep");

    let runtime = std::path::PathBuf::from(std::env::var_os("XDG_RUNTIME_DIR").unwrap());
    let config = runtime.join("synthetic-config.toml");
    let credentials = runtime.join("synthetic-credentials.toml");
    fs::write(
        &credentials,
        "client_id='synthetic-id'\nclient_secret='synthetic-secret'\n",
    )
    .unwrap();
    fs::set_permissions(&credentials, fs::Permissions::from_mode(0o600)).unwrap();
    fs::write(&config, format!("[tuya]\nendpoint='https://example.invalid'\ndevice_id='synthetic-plug'\nswitch_code='switch_1'\ncredentials_file='{}'\n[moonlight]\nexecutable='nonexistent-test-moonlight'\nhost='synthetic-host.invalid'\napp='Desktop'\n[startup]\ntimeout_seconds=1\npoll_interval_seconds=1\nprobe_timeout_seconds=1\nhttp_timeout_seconds=1\n[notifications]\nenabled=false\n", credentials.display())).unwrap();
    let fd = unsafe { libc::inotify_init1(libc::IN_CLOEXEC | libc::IN_NONBLOCK) };
    assert!(fd >= 0);
    let observer = unsafe { File::from_raw_fd(fd) };
    for path in [&config, &credentials] {
        let path = std::ffi::CString::new(path.as_os_str().as_encoded_bytes()).unwrap();
        assert!(
            unsafe { libc::inotify_add_watch(observer.as_raw_fd(), path.as_ptr(), libc::IN_OPEN) }
                >= 0
        );
    }
    let quit = Arc::new(AtomicBool::new(false));
    let mut session = Session::new(
        Some(&config),
        Some(Scenario::Success),
        egui::Context::default(),
        quit,
    );
    assert!(session.model.lock().unwrap().error.is_none());
    assert_eq!(session.model.lock().unwrap().host, "demo-host.invalid");
    // A production operation lock does not block demo, and idle panels do not take it.
    let live_lock = backend::operation_lock("operation").unwrap();
    session.action(Action::Run(Operation::Start));
    session.action(Action::Run(Operation::PlugOff));
    session.action(Action::Hide);
    let deadline = Instant::now() + Duration::from_secs(6);
    loop {
        let model = session.model.lock().unwrap();
        assert!(model.error.is_none(), "{:?}", model.error);
        if model.phase == Phase::Streaming {
            break;
        }
        drop(model);
        assert!(Instant::now() < deadline);
        thread::sleep(Duration::from_millis(10));
    }
    assert!(matches!(
        backend::operation_lock("demo-success-operation"),
        Err(Error::Busy)
    ));
    session.action(Action::EndDemo);
    while session.model.lock().unwrap().busy {
        assert!(Instant::now() < deadline);
        thread::sleep(Duration::from_millis(10));
    }
    assert!(session.model.lock().unwrap().status.unwrap().0);
    // Validation completing after Hide must decline without requiring a redraw.
    session.action(Action::Run(Operation::PlugOff));
    while session.model.lock().unwrap().busy {
        assert!(Instant::now() < deadline);
        thread::sleep(Duration::from_millis(10));
    }
    assert!(session.model.lock().unwrap().status.unwrap().0);
    assert!(session
        .model
        .lock()
        .unwrap()
        .error
        .as_ref()
        .unwrap()
        .contains("approval was declined"));
    assert!(backend::operation_lock("demo-success-operation").is_ok());
    session.open();
    session.action(Action::Run(Operation::PlugOff));
    while session.model.lock().unwrap().phase != Phase::ConfirmingOff {
        assert!(Instant::now() < deadline);
        thread::sleep(Duration::from_millis(10));
    }
    // Destruction also unblocks a worker waiting for approval, with no off command.
    let model = session.model.clone();
    drop(session);
    assert!(model.lock().unwrap().status.unwrap().0);
    assert!(model
        .lock()
        .unwrap()
        .error
        .as_ref()
        .unwrap()
        .contains("approval was declined"));
    drop(live_lock);
    assert!(backend::operation_lock("demo-success-operation").is_ok());
    let mut events = [0_u8; 4096];
    assert_eq!(
        unsafe {
            libc::read(
                observer.as_raw_fd(),
                events.as_mut_ptr().cast(),
                events.len(),
            )
        },
        -1
    );
    assert_eq!(
        std::io::Error::last_os_error().kind(),
        std::io::ErrorKind::WouldBlock,
        "demo opened production config or credentials"
    );
}

#[test]
fn secure_ipc_demo_isolation_and_hidden_worker_lifecycle() {
    let directory = tempfile::tempdir().unwrap();
    fs::set_permissions(directory.path(), fs::Permissions::from_mode(0o700)).unwrap();
    let mut child = child("controller", directory.path()).spawn().unwrap();
    assert!(wait(&mut child).success());
}

#[test]
fn terminal_prompt_exact_phrase_eof_and_signal_cancellation() {
    let directory = tempfile::tempdir().unwrap();
    for (input, signal, expected) in [
        (Some("POWER OFF\n"), None, true),
        (Some("POWER off\n"), None, false),
        (Some(" POWER OFF\n"), None, false),
        (Some("\n"), None, false),
        (Some("\u{4}"), None, false),
        (Some("\u{3}"), None, false),
        (None, Some(libc::SIGTERM), false),
        (None, Some(libc::SIGINT), false),
    ] {
        let mut master = -1;
        let mut slave = -1;
        assert_eq!(
            unsafe {
                libc::openpty(
                    &mut master,
                    &mut slave,
                    std::ptr::null_mut(),
                    std::ptr::null(),
                    std::ptr::null(),
                )
            },
            0
        );
        let mut master = unsafe { File::from_raw_fd(master) };
        let slave = unsafe { File::from_raw_fd(slave) };
        let ready = directory.path().join("prompt-ready");
        let _ = fs::remove_file(&ready);
        let mut command = child("prompt", directory.path());
        command
            .env("MOONBOOT_EXPECT_APPROVAL", expected.to_string())
            .env("MOONBOOT_PROMPT_READY", &ready)
            .stdin(Stdio::from(slave))
            .stdout(Stdio::null())
            .stderr(Stdio::null());
        unsafe {
            command.pre_exec(|| {
                if libc::setsid() < 0
                    || libc::ioctl(0, libc::TIOCSCTTY, 0) < 0
                    || libc::tcsetpgrp(0, libc::getpgrp()) < 0
                {
                    return Err(std::io::Error::last_os_error());
                }
                Ok(())
            });
        }
        let mut process = command.spawn().unwrap();
        let deadline = Instant::now() + Duration::from_secs(3);
        while !ready.exists() {
            assert!(Instant::now() < deadline);
            thread::sleep(Duration::from_millis(10));
        }
        if let Some(input) = input {
            master.write_all(input.as_bytes()).unwrap();
        }
        if let Some(signal) = signal {
            assert_eq!(
                unsafe { libc::kill(process.id() as libc::pid_t, signal) },
                0
            );
        }
        assert!(wait(&mut process).success());
    }
}

#[test]
fn isolated_bus_tray_is_inert_and_terminates() {
    let bus = std::env::var_os("PATH")
        .into_iter()
        .flat_map(|path| std::env::split_paths(&path).collect::<Vec<_>>())
        .map(|directory| directory.join("dbus-run-session"))
        .find(|path| path.is_file());
    let Some(bus) = bus else {
        eprintln!("SKIP isolated tray test: dbus-run-session is not available");
        return;
    };
    for demo in [false, true] {
        let directory = tempfile::tempdir().unwrap();
        fs::set_permissions(directory.path(), fs::Permissions::from_mode(0o700)).unwrap();
        let bus_config = directory.path().join("test-bus.conf");
        fs::write(&bus_config, "<busconfig><type>session</type><listen>unix:tmpdir=/tmp</listen><auth>EXTERNAL</auth><policy context=\"default\"><allow send_destination=\"*\"/><allow receive_sender=\"*\"/><allow own=\"*\"/></policy></busconfig>").unwrap();
        let mut command = Command::new(&bus);
        command
            .arg("--config-file")
            .arg(&bus_config)
            .arg("--")
            .arg(std::env::current_exe().unwrap())
            .args([
                "--exact",
                "isolated_child",
                "--nocapture",
                "--test-threads=1",
            ])
            .env("MOONBOOT_TEST_CHILD", "tray")
            .env("MOONBOOT_TEST_DEMO", demo.to_string())
            .env("XDG_RUNTIME_DIR", directory.path())
            .env("XDG_STATE_HOME", directory.path())
            .env("HOME", directory.path())
            .stdin(Stdio::null())
            .stdout(Stdio::inherit())
            .stderr(Stdio::inherit());
        let mut process = command.spawn().unwrap();
        assert!(wait(&mut process).success());
    }
}

fn tray_child() {
    let directory = std::path::PathBuf::from(std::env::var_os("XDG_RUNTIME_DIR").unwrap());
    let demo = std::env::var("MOONBOOT_TEST_DEMO").unwrap() == "true";
    let mut command = Command::new(env!("CARGO_BIN_EXE_moonboot"));
    command
        .args(["tray", "--config", "/missing-config-that-must-not-be-read"])
        .env("PATH", "/nonexistent-test-path")
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null());
    if demo {
        command.args(["--demo", "success"]);
    }
    let mut process = command.spawn().unwrap();
    let tray_lock = directory.join(if demo {
        "moonboot-demo-success-tray.lock"
    } else {
        "moonboot-tray.lock"
    });
    let deadline = Instant::now() + Duration::from_secs(3);
    while !tray_lock.exists() {
        assert!(process.try_wait().unwrap().is_none());
        assert!(Instant::now() < deadline);
        thread::sleep(Duration::from_millis(10));
    }
    thread::sleep(Duration::from_millis(200));
    assert!(process.try_wait().unwrap().is_none());
    assert!(!directory.join("moonboot-operation.lock").exists());
    assert!(!directory.join("moonboot-ui.sock").exists());
    assert!(!directory.join("moonboot-demo-success-ui.sock").exists());
    assert!(!directory
        .join("moonboot-demo-success-operation.lock")
        .exists());
    // A standalone panel was never attached to this tray via Open Controls.
    let scenario = demo.then_some(Scenario::Success);
    let standalone = Instance::claim(scenario).unwrap().unwrap();
    let other = Instance::claim(if demo { None } else { Some(Scenario::Success) })
        .unwrap()
        .unwrap();
    let standalone_worker = thread::spawn(move || {
        let deadline = Instant::now() + Duration::from_secs(3);
        loop {
            if let Some(request) = standalone.receive() {
                assert_eq!(request, Request::Quit);
                return;
            }
            assert!(
                Instant::now() < deadline,
                "tray never sent Quit to standalone controls"
            );
            thread::sleep(Duration::from_millis(10));
        }
    });
    unsafe {
        libc::kill(process.id() as libc::pid_t, libc::SIGTERM);
    }
    assert!(wait(&mut process).success());
    standalone_worker.join().unwrap();
    assert_eq!(other.receive(), None);
}

struct Watcher(std::sync::mpsc::Sender<String>);

#[zbus::interface(name = "org.kde.StatusNotifierWatcher")]
impl Watcher {
    fn register_status_notifier_item(&self, service: &str) {
        self.0.send(service.to_owned()).unwrap();
    }

    #[zbus(property)]
    fn is_status_notifier_host_registered(&self) -> bool {
        true
    }

    #[zbus(property)]
    fn protocol_version(&self) -> i32 {
        0
    }
}

// Reap subprocesses even when a protocol assertion panics.
struct TestProcess(Child);

impl Drop for TestProcess {
    fn drop(&mut self) {
        let _ = self.0.kill();
        let _ = self.0.wait();
    }
}

#[test]
fn isolated_bus_tray_recovers_watcher_and_preserves_demo_identity() {
    let directory = tempfile::tempdir().unwrap();
    fs::set_permissions(directory.path(), fs::Permissions::from_mode(0o700)).unwrap();
    let bus_config = directory.path().join("test-bus.conf");
    // No service activation directories: this bus cannot launch desktop/cloud services.
    fs::write(&bus_config, format!("<busconfig><type>session</type><listen>unix:tmpdir={}</listen><auth>EXTERNAL</auth><policy context=\"default\"><allow send_destination=\"*\"/><allow receive_sender=\"*\"/><allow own=\"*\"/></policy></busconfig>", directory.path().display())).unwrap();
    let mut process = TestProcess(
        Command::new("dbus-run-session")
            .arg("--config-file")
            .arg(bus_config)
            .arg("--")
            .arg(std::env::current_exe().unwrap())
            .args([
                "--exact",
                "isolated_child",
                "--nocapture",
                "--test-threads=1",
            ])
            .env("MOONBOOT_TEST_CHILD", "tray-recovery")
            .env("HOME", directory.path())
            .env("XDG_RUNTIME_DIR", directory.path())
            .env("XDG_CONFIG_HOME", directory.path())
            .env("XDG_STATE_HOME", directory.path())
            .env_remove("DBUS_SESSION_BUS_ADDRESS")
            .env_remove("DBUS_STARTER_ADDRESS")
            .env_remove("DBUS_STARTER_BUS_TYPE")
            .env("DBUS_SYSTEM_BUS_ADDRESS", "unix:path=/nonexistent-test-bus")
            .env_remove("DISPLAY")
            .env_remove("WAYLAND_DISPLAY")
            .stdin(Stdio::null())
            .spawn()
            .expect("tray recovery regression requires dbus-run-session"),
    );
    assert!(wait(&mut process.0).success());
}

fn tray_recovery_child() {
    use std::collections::HashMap;
    use zbus::{
        blocking::{connection::Builder, Proxy},
        zvariant::{OwnedObjectPath, OwnedValue},
    };

    let directory = std::path::PathBuf::from(std::env::var_os("XDG_RUNTIME_DIR").unwrap());
    let address = std::env::var("DBUS_SESSION_BUS_ADDRESS").unwrap();
    assert!(address.contains(directory.to_str().unwrap()));
    let connection = Builder::address(address.as_str())
        .unwrap()
        .method_timeout(Duration::from_secs(2))
        .build()
        .unwrap();
    let dbus = zbus::blocking::fdo::DBusProxy::new(&connection).unwrap();
    assert!(!dbus
        .name_has_owner("org.kde.StatusNotifierWatcher".try_into().unwrap())
        .unwrap());

    let config = directory.join("invalid-config.toml");
    let credentials = directory.join("credentials.toml");
    fs::write(&config, "invalid TOML: must not be read").unwrap();
    fs::write(&credentials, "synthetic credentials: must not be read").unwrap();
    let fd = unsafe { libc::inotify_init1(libc::IN_CLOEXEC | libc::IN_NONBLOCK) };
    assert!(fd >= 0);
    let observer = unsafe { File::from_raw_fd(fd) };
    for path in [&config, &credentials] {
        let path = std::ffi::CString::new(path.as_os_str().as_encoded_bytes()).unwrap();
        assert!(
            unsafe { libc::inotify_add_watch(observer.as_raw_fd(), path.as_ptr(), libc::IN_OPEN) }
                >= 0
        );
    }
    let stderr = directory.join("tray-stderr");
    let mut process = TestProcess(
        Command::new(env!("CARGO_BIN_EXE_moonboot"))
            .args(["tray", "--demo", "success", "--config"])
            .arg(&config)
            .env("PATH", "/nonexistent-test-path")
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(File::create(&stderr).unwrap())
            .spawn()
            .unwrap(),
    );
    let pid = process.0.id();
    let assert_idle = |process: &mut Child| {
        assert_eq!(process.id(), pid);
        assert!(process.try_wait().unwrap().is_none(), "tray exited");
        // Missing-host diagnostics may briefly attempt the optional notifier.
        // Wait for that failed exec to be reaped, rather than racing its fork.
        let deadline = Instant::now() + Duration::from_secs(1);
        loop {
            if fs::read_to_string(format!("/proc/{pid}/task/{pid}/children"))
                .unwrap()
                .trim()
                .is_empty()
            {
                break;
            }
            assert!(Instant::now() < deadline, "tray retained a child process");
            thread::sleep(Duration::from_millis(10));
        }
        for name in [
            "moonboot-ui.sock",
            "moonboot-demo-success-ui.sock",
            "moonboot-ui.lock",
            "moonboot-demo-success-ui.lock",
            "moonboot-operation.lock",
            "moonboot-demo-success-operation.lock",
        ] {
            assert!(!directory.join(name).exists(), "unexpected {name}");
        }
    };
    let wait_diagnostic = |process: &mut Child, count| {
        let deadline = Instant::now() + Duration::from_secs(3);
        loop {
            assert_idle(process);
            let diagnostic = fs::read_to_string(&stderr).unwrap();
            if diagnostic.matches("Tray host unavailable.").count() >= count {
                assert!(diagnostic.contains("Enable Waybar's tray module"));
                assert!(diagnostic.contains("moonboot gui"));
                assert!(diagnostic.contains("waiting for tray host recovery"));
                break;
            }
            assert!(
                Instant::now() < deadline,
                "missing diagnostic: {diagnostic}"
            );
            thread::sleep(Duration::from_millis(10));
        }
        // Let ksni install its NameOwnerChanged subscription after the callback.
        thread::sleep(Duration::from_millis(200));
        assert_idle(process);
    };
    wait_diagnostic(&mut process.0, 1);

    let mut first_service = None;
    for generation in 0..2 {
        let (tx, rx) = std::sync::mpsc::channel();
        let watcher = Builder::address(address.as_str())
            .unwrap()
            .serve_at("/StatusNotifierWatcher", Watcher(tx))
            .unwrap()
            .name("org.kde.StatusNotifierWatcher")
            .unwrap()
            .build()
            .unwrap();
        let service = rx
            .recv_timeout(Duration::from_secs(3))
            .expect("tray did not register with replacement watcher");
        assert_idle(&mut process.0);
        let owner = dbus
            .get_name_owner(service.as_str().try_into().unwrap())
            .unwrap();
        assert_eq!(
            dbus.get_connection_unix_process_id(owner.into()).unwrap(),
            pid
        );
        if let Some(first) = &first_service {
            assert_eq!(&service, first, "recovery replaced the tray D-Bus identity");
        } else {
            first_service = Some(service.clone());
        }
        let item = Proxy::new(
            &connection,
            service.as_str(),
            "/StatusNotifierItem",
            "org.kde.StatusNotifierItem",
        )
        .unwrap();
        assert_eq!(
            item.get_property::<String>("Id").unwrap(),
            "demo-success-moonboot"
        );
        assert_eq!(
            item.get_property::<String>("Title").unwrap(),
            "Moonboot DEMO"
        );
        let menu_path: OwnedObjectPath = item.get_property("Menu").unwrap();
        let menu = Proxy::new(
            &connection,
            service.as_str(),
            menu_path.as_str(),
            "com.canonical.dbusmenu",
        )
        .unwrap();
        type Layout = (i32, HashMap<String, OwnedValue>, Vec<OwnedValue>);
        let (_revision, (root, _, children)): (u32, Layout) = menu
            .call("GetLayout", &(0_i32, -1_i32, Vec::<String>::new()))
            .unwrap();
        assert_eq!(root, 0);
        assert_eq!(children.len(), 2, "tray must expose no power commands");
        let mut quit_id = None;
        for (child, expected) in children.into_iter().zip(["Open DEMO Controls", "Quit"]) {
            let (id, properties, children): Layout = child.try_into().unwrap();
            assert!(id > 0);
            assert!(children.is_empty(), "unexpected command submenu");
            assert_eq!(<&str>::try_from(&properties["label"]).unwrap(), expected);
            // D-BusMenu omits default-valued properties; both default to true.
            for name in ["enabled", "visible"] {
                assert!(properties
                    .get(name)
                    .map(|value| bool::try_from(value).unwrap())
                    .unwrap_or(true));
            }
            if expected == "Quit" {
                quit_id = Some(id);
            }
        }
        assert_idle(&mut process.0);
        if generation == 0 {
            watcher.close().unwrap();
            wait_diagnostic(&mut process.0, 2);
            assert!(!dbus
                .name_has_owner("org.kde.StatusNotifierWatcher".try_into().unwrap())
                .unwrap());
        } else {
            // Only Quit is activated: Open would launch graphical controls.
            menu.call::<_, _, ()>(
                "Event",
                &(quit_id.unwrap(), "clicked", OwnedValue::from(0_i32), 0_u32),
            )
            .unwrap();
            assert!(wait(&mut process.0).success());
            watcher.close().unwrap();
        }
    }
    let mut events = [0_u8; 4096];
    assert_eq!(
        unsafe {
            libc::read(
                observer.as_raw_fd(),
                events.as_mut_ptr().cast(),
                events.len(),
            )
        },
        -1
    );
    assert_eq!(
        std::io::Error::last_os_error().kind(),
        std::io::ErrorKind::WouldBlock,
        "tray opened config or credentials during watcher recovery"
    );
    assert!(!directory.join("moonboot-demo-success-ui.sock").exists());
}
