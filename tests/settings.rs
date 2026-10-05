use egui_kittest::{
    kittest::{NodeT, Queryable},
    Harness,
};
use moonboot::{
    backend::{self, Event, Phase},
    config::Config,
    controller::Session,
    demo::Scenario,
    ui::{settings::Draft, Action, Model},
};
use std::{
    fs,
    os::unix::fs::PermissionsExt,
    process::Command,
    sync::{atomic::AtomicBool, Arc},
    thread,
    time::{Duration, Instant},
};

fn config() -> Config {
    let mut config = Config::default();
    configure(&mut config);
    config
}

fn configure(config: &mut Config) {
    config.tuya.endpoint = "https://example.invalid".into();
    config.tuya.device_id = "synthetic-device".into();
    config.tuya.switch_code = "switch_1".into();
    config.tuya.client_id = "synthetic-id".into();
    config.tuya.client_secret = "synthetic-secret".into();
    config.moonlight.host = "synthetic-host.invalid".into();
    config.moonlight.stream_args = vec![
        String::new(),
        "two words; $(not-shell)".into(),
        "literal\nnewline".into(),
    ];
    config.notifications.enabled = false;
}

#[test]
fn settings_draft_ui_masking_cancel_escape_and_fresh_undo() {
    let directory = tempfile::tempdir().unwrap();
    let path = directory.path().join("settings.toml");
    let mut model = Model::new(
        "Not configured".into(),
        "Unknown".into(),
        "Unknown".into(),
        None,
    );
    model.configured = false;
    let mut harness = Harness::builder()
        .with_size(egui::vec2(800.0, 900.0))
        .build_state(
            move |ctx, model: &mut Model| match model.draw(ctx) {
                Some(Action::OpenSettings) => {
                    model.settings = Some(Draft::new(Config::default(), path.clone()))
                }
                Some(Action::CancelSettings) => model.discard_settings(ctx),
                _ => {}
            },
            model,
        );
    assert!(!harness
        .get_by_label("Settings")
        .accesskit_node()
        .is_disabled());
    assert!(harness
        .get_by_label("Start Session")
        .accesskit_node()
        .is_disabled());
    harness.get_by_label("Settings").click();
    harness.run();
    for label in [
        "Start Session",
        "Refresh Status",
        "Plug Off",
        "Settings",
        "Hide",
        "Quit",
    ] {
        assert!(harness.get_by_label(label).accesskit_node().is_disabled());
    }
    harness.get_by_label_contains("Configuration location:");
    harness.get_by_label("Save");
    let secret =
        harness.get_by_role_and_label(egui::accesskit::Role::PasswordInput, "Cloud client secret");
    secret.click();
    harness.run();
    harness
        .get_by_role_and_label(egui::accesskit::Role::PasswordInput, "Cloud client secret")
        .type_text("draft-secret");
    harness.run();
    assert_eq!(
        harness
            .state()
            .settings
            .as_ref()
            .unwrap()
            .config
            .tuya
            .client_secret,
        "draft-secret"
    );
    assert!(!harness
        .get_by_role_and_label(egui::accesskit::Role::PasswordInput, "Cloud client secret")
        .value()
        .unwrap_or_default()
        .contains("draft-secret"));
    harness.get_by_label("Cancel").click();
    harness.run();
    assert!(harness.state().settings.is_none());
    harness.get_by_label("Settings").click();
    harness.run();
    harness
        .get_by_role_and_label(egui::accesskit::Role::PasswordInput, "Cloud client secret")
        .click();
    harness.run();
    harness.key_press_modifiers(egui::Modifiers::COMMAND, egui::Key::Z);
    harness.run();
    assert!(harness
        .state()
        .settings
        .as_ref()
        .unwrap()
        .config
        .tuya
        .client_secret
        .is_empty());
    harness.key_press(egui::Key::Escape);
    harness.run();
    assert!(harness.state().settings.is_none());
    harness.get_by_label("Settings").click();
    harness.run();
    harness.get_by_label("Close window").click();
    harness.run();
    assert!(harness.state().settings.is_none());
    harness.state_mut().busy = true;
    harness.state_mut().phase = Phase::Streaming;
    harness.run();
    assert!(harness
        .get_by_label("Settings")
        .accesskit_node()
        .is_disabled());
    harness.state_mut().busy = false;
    harness.state_mut().phase = Phase::Idle;
    harness.state_mut().demo = Some(Scenario::Success);
    harness.run();
    assert!(harness
        .get_by_label("Settings")
        .accesskit_node()
        .is_disabled());
}

fn wait(session: &Session) {
    let deadline = Instant::now() + Duration::from_secs(4);
    while session.model.lock().unwrap().busy {
        assert!(Instant::now() < deadline, "save did not complete");
        thread::sleep(Duration::from_millis(5));
    }
}

#[test]
fn compact_editor_save_and_literal_arguments() {
    let directory = tempfile::tempdir().unwrap();
    let mut model = Model::new(
        "synthetic".into(),
        "Desktop".into(),
        "synthetic".into(),
        None,
    );
    model.settings = Some(Draft::new(config(), directory.path().join("config.toml")));
    let mut harness = Harness::builder()
        .with_size(egui::vec2(520.0, 440.0))
        .build_state(
            |ctx, state: &mut (Model, Vec<Action>)| {
                if let Some(action) = state.0.draw(ctx) {
                    state.1.push(action);
                }
            },
            (model, Vec::new()),
        );
    harness.get_by_label("Save").click();
    harness.run();
    assert_eq!(harness.state().1, [Action::SaveSettings]);
    harness.get_by_label_contains("Keep it out of Git");
    harness.get_by_label("Reload from disk").click();
    harness.run();
    assert_eq!(harness.state().1.last(), Some(&Action::ReloadSettings));
    for phase in [Phase::Waiting, Phase::Streaming, Phase::ConfirmingOff] {
        harness.state_mut().0.phase = phase;
        harness.run();
        assert!(harness
            .get_by_label("Reload from disk")
            .accesskit_node()
            .is_disabled());
    }
    harness.state_mut().0.phase = Phase::Idle;
    harness.state_mut().0.busy = true;
    harness.run();
    assert!(harness
        .get_by_label("Reload from disk")
        .accesskit_node()
        .is_disabled());
    assert_eq!(
        harness
            .state()
            .0
            .settings
            .as_ref()
            .unwrap()
            .config
            .moonlight
            .stream_args,
        config().moonlight.stream_args
    );
}

#[test]
fn external_credentials_toggle_is_mutually_exclusive() {
    let directory = tempfile::tempdir().unwrap();
    let mut model = Model::new(
        "synthetic".into(),
        "Desktop".into(),
        "synthetic".into(),
        None,
    );
    let mut config = config();
    config.moonlight.stream_args.clear();
    model.settings = Some(Draft::new(config, directory.path().join("config.toml")));
    let mut harness = Harness::builder()
        .with_size(egui::vec2(800.0, 900.0))
        .build_state(
            |ctx, model: &mut Model| {
                model.draw(ctx);
            },
            model,
        );
    harness.get_by_label("Advanced").scroll_to_me();
    harness.run();
    harness.get_by_label("Advanced").click();
    harness.run();
    harness
        .get_by_label("Use external credentials file")
        .scroll_to_me();
    harness.run();
    harness
        .get_by_label("Use external credentials file")
        .click();
    harness.run();
    assert!(harness
        .state()
        .settings
        .as_ref()
        .unwrap()
        .config
        .tuya
        .client_id
        .is_empty());
    assert!(harness
        .state()
        .settings
        .as_ref()
        .unwrap()
        .config
        .tuya
        .client_secret
        .is_empty());
    harness
        .get_by_role_and_label(
            egui::accesskit::Role::TextInput,
            "External credentials file",
        )
        .scroll_to_me();
    harness.run();
    harness
        .get_by_role_and_label(
            egui::accesskit::Role::TextInput,
            "External credentials file",
        )
        .click();
    harness.run();
    harness
        .get_by_role_and_label(
            egui::accesskit::Role::TextInput,
            "External credentials file",
        )
        .type_text("/synthetic-runtime/credentials.toml");
    harness.run();
    harness.get_by_label("Save").click();
    harness.run();
    assert_eq!(
        harness
            .state()
            .settings
            .as_ref()
            .unwrap()
            .config
            .tuya
            .credentials_file
            .as_deref(),
        Some(std::path::Path::new("/synthetic-runtime/credentials.toml"))
    );
    harness
        .get_by_label("Use external credentials file")
        .click();
    harness.run();
    harness.get_by_label("Save").click();
    harness.run();
    assert!(harness
        .state()
        .settings
        .as_ref()
        .unwrap()
        .config
        .tuya
        .credentials_file
        .is_none());
}

#[test]
fn isolated_settings_worker() {
    let Some(directory) = std::env::var_os("MOONBOOT_SETTINGS_FIXTURE") else {
        return;
    };
    let directory = std::path::PathBuf::from(directory);
    let path = directory.join("private/config.toml");
    let mut session = Session::new(
        Some(&path),
        None,
        egui::Context::default(),
        Arc::new(AtomicBool::new(false)),
    );
    assert!(!session.model.lock().unwrap().configured);
    session.action(Action::OpenSettings);
    assert!(session
        .model
        .lock()
        .unwrap()
        .settings
        .as_ref()
        .unwrap()
        .config
        .tuya
        .client_secret
        .is_empty());
    configure(
        &mut session
            .model
            .lock()
            .unwrap()
            .settings
            .as_mut()
            .unwrap()
            .config,
    );
    session.action(Action::SaveSettings);
    session.action(Action::Hide);
    wait(&session);
    assert_eq!(
        Config::load(Some(&path)).unwrap().moonlight.host,
        "synthetic-host.invalid"
    );
    assert_eq!(
        fs::metadata(&path).unwrap().permissions().mode() & 0o777,
        0o600
    );
    assert_eq!(
        fs::metadata(path.parent().unwrap())
            .unwrap()
            .permissions()
            .mode()
            & 0o777,
        0o700
    );
    {
        let model = session.model.lock().unwrap();
        assert!(model.configured);
        assert!(model.error.is_none());
        assert!(model.status.is_none());
        assert_eq!(model.host, "synthetic-host.invalid");
    }
    session.open();
    session.action(Action::OpenSettings);
    assert_eq!(
        session
            .model
            .lock()
            .unwrap()
            .settings
            .as_ref()
            .unwrap()
            .config
            .tuya
            .client_id,
        "synthetic-id"
    );
    let before = fs::read(&path).unwrap();
    session
        .model
        .lock()
        .unwrap()
        .settings
        .as_mut()
        .unwrap()
        .config
        .moonlight
        .host = "changed-host.invalid".into();
    let lock = backend::operation_lock("operation").unwrap();
    session.action(Action::SaveSettings);
    wait(&session);
    assert_eq!(fs::read(&path).unwrap(), before);
    assert_eq!(session.model.lock().unwrap().host, "synthetic-host.invalid");
    drop(lock);
    session.action(Action::CancelSettings);
    session.action(Action::OpenSettings);
    assert_eq!(
        session
            .model
            .lock()
            .unwrap()
            .settings
            .as_ref()
            .unwrap()
            .config
            .moonlight
            .host,
        "synthetic-host.invalid"
    );
    fs::set_permissions(&path, fs::Permissions::from_mode(0o400)).unwrap();
    session.action(Action::SaveSettings);
    wait(&session);
    assert_eq!(fs::read(&path).unwrap(), before);
    assert!(session
        .model
        .lock()
        .unwrap()
        .error
        .as_ref()
        .unwrap()
        .contains("redeploy"));
    fs::set_permissions(&path, fs::Permissions::from_mode(0o600)).unwrap();
    session.action(Action::ReloadSettings);
    wait(&session);
    session.action(Action::CancelSettings);
    session.action(Action::OpenSettings);
    session
        .model
        .lock()
        .unwrap()
        .settings
        .as_mut()
        .unwrap()
        .config
        .tuya
        .endpoint = "invalid".into();
    session.action(Action::SaveSettings);
    wait(&session);
    assert_eq!(fs::read(&path).unwrap(), before);
    session.action(Action::CancelSettings);
    session.action(Action::OpenSettings);
    session.model.lock().unwrap().busy = true;
    session.action(Action::SaveSettings);
    assert_eq!(fs::read(&path).unwrap(), before);
    session.model.lock().unwrap().busy = false;
    session
        .model
        .lock()
        .unwrap()
        .event(Event::Phase(Phase::ConfirmingOff));
    session.action(Action::SaveSettings);
    assert_eq!(fs::read(&path).unwrap(), before);
    session.model.lock().unwrap().phase = Phase::Idle;
    session.action(Action::CancelSettings);
    session.action(Action::OpenSettings);
    session
        .model
        .lock()
        .unwrap()
        .settings
        .as_mut()
        .unwrap()
        .config
        .moonlight
        .host = "updated-host.invalid".into();
    session.model.lock().unwrap().status = Some((true, std::time::SystemTime::now()));
    session.action(Action::SaveSettings);
    wait(&session);
    assert!(session.model.lock().unwrap().status.is_none());
    assert_eq!(session.model.lock().unwrap().host, "updated-host.invalid");
    session.action(Action::OpenSettings);
    assert_eq!(
        session
            .model
            .lock()
            .unwrap()
            .settings
            .as_ref()
            .unwrap()
            .config
            .moonlight
            .host,
        "updated-host.invalid"
    );
    drop(session);

    let link = directory.join("link.toml");
    std::os::unix::fs::symlink(&path, &link).unwrap();
    let mut session = Session::new(
        Some(&link),
        None,
        egui::Context::default(),
        Arc::new(AtomicBool::new(false)),
    );
    session.action(Action::OpenSettings);
    session
        .model
        .lock()
        .unwrap()
        .settings
        .as_mut()
        .unwrap()
        .config
        .moonlight
        .app = "Updated Desktop".into();
    session.action(Action::SaveSettings);
    wait(&session);
    assert!(fs::symlink_metadata(&link)
        .unwrap()
        .file_type()
        .is_symlink());
    assert_eq!(
        Config::load(Some(&path)).unwrap().moonlight.app,
        "Updated Desktop"
    );
    drop(session);

    // Exercise the complete UI -> controller -> worker path in the same window context.
    let mut harness = Harness::builder()
        .with_size(egui::vec2(800.0, 900.0))
        .build_state(
            move |ctx, state: &mut Option<Session>| {
                let session = state.get_or_insert_with(|| {
                    Session::new(
                        Some(&link),
                        None,
                        ctx.clone(),
                        Arc::new(AtomicBool::new(false)),
                    )
                });
                let action = session.model.lock().unwrap().draw(ctx);
                if let Some(action) = action {
                    session.action(action);
                }
            },
            None,
        );
    harness.get_by_label("Settings").click();
    harness.run();
    harness
        .get_by_role_and_label(egui::accesskit::Role::TextInput, "Moonlight host")
        .click();
    harness.run();
    harness.key_press_modifiers(egui::Modifiers::COMMAND, egui::Key::A);
    harness.run();
    harness
        .get_by_role_and_label(egui::accesskit::Role::TextInput, "Moonlight host")
        .type_text("edited-in-ui.invalid");
    harness.run();
    harness.get_by_label("Save").click();
    harness.step();
    wait(harness.state().as_ref().unwrap());
    harness.run();
    harness.get_by_label("Host: edited-in-ui.invalid");
    assert!(harness.query_by_label("Cloud client secret").is_none());
    assert_eq!(
        Config::load(Some(&path)).unwrap().moonlight.host,
        "edited-in-ui.invalid"
    );
    assert_eq!(
        Config::load(Some(&path)).unwrap().moonlight.stream_args,
        config().moonlight.stream_args
    );
    harness.get_by_label("Settings").click();
    harness.run();
    assert_eq!(
        harness
            .state()
            .as_ref()
            .unwrap()
            .model
            .lock()
            .unwrap()
            .settings
            .as_ref()
            .unwrap()
            .config
            .moonlight
            .host,
        "edited-in-ui.invalid"
    );
    drop(harness);
    reload_fixture(&directory);
    stale_settings_fixture(&directory);

    // Observe actual file opens as well as writes, including forged demo actions.
    use std::os::fd::{AsRawFd, FromRawFd};
    let credentials = directory.join("external-credentials.toml");
    fs::write(&credentials, "synthetic-secret").unwrap();
    let before = fs::read(&path).unwrap();
    let fd = unsafe { libc::inotify_init1(libc::IN_CLOEXEC | libc::IN_NONBLOCK) };
    assert!(fd >= 0);
    let observer = unsafe { fs::File::from_raw_fd(fd) };
    for watched in [&path, &credentials] {
        let name = std::ffi::CString::new(watched.as_os_str().as_encoded_bytes()).unwrap();
        assert!(
            unsafe { libc::inotify_add_watch(fd, name.as_ptr(), libc::IN_OPEN | libc::IN_MODIFY) }
                >= 0
        );
    }
    let mut demo = Session::new(
        Some(&path),
        Some(Scenario::Success),
        egui::Context::default(),
        Arc::new(AtomicBool::new(false)),
    );
    demo.action(Action::OpenSettings);
    assert!(demo.model.lock().unwrap().settings.is_none());
    demo.model.lock().unwrap().settings = Some(Draft::new(config(), path.clone()));
    demo.action(Action::SaveSettings);
    demo.action(Action::ReloadSettings);
    assert!(!demo.model.lock().unwrap().busy);
    demo.action(Action::CancelSettings);
    drop(demo);
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
        "demo settings accessed production files"
    );
    assert_eq!(fs::read(&path).unwrap(), before);
}

fn stale_settings_fixture(directory: &std::path::Path) {
    let path = directory.join("private/stale.toml");
    let old = config();
    fs::write(&path, toml::to_string(&old).unwrap()).unwrap();
    fs::set_permissions(&path, fs::Permissions::from_mode(0o600)).unwrap();
    let mut session = Session::new(
        Some(&path),
        None,
        egui::Context::default(),
        Arc::new(AtomicBool::new(false)),
    );
    session.action(Action::OpenSettings);
    session
        .model
        .lock()
        .unwrap()
        .settings
        .as_mut()
        .unwrap()
        .config
        .moonlight
        .host = "unsaved-host.invalid".into();
    let mut external = config();
    external.moonlight.host = "external-edit.invalid".into();
    let external_text = toml::to_string(&external).unwrap();
    fs::write(&path, &external_text).unwrap();
    session.action(Action::SaveSettings);
    wait(&session);
    assert_eq!(fs::read_to_string(&path).unwrap(), external_text);
    assert_eq!(session.model.lock().unwrap().host, old.moonlight.host);
    assert_eq!(
        session
            .model
            .lock()
            .unwrap()
            .settings
            .as_ref()
            .unwrap()
            .config
            .moonlight
            .host,
        "unsaved-host.invalid"
    );
    session.action(Action::CancelSettings);
    session.action(Action::OpenSettings);
    assert_eq!(
        session
            .model
            .lock()
            .unwrap()
            .settings
            .as_ref()
            .unwrap()
            .config
            .moonlight
            .host,
        old.moonlight.host
    );
    session.action(Action::ReloadSettings);
    wait(&session);
    assert_eq!(session.model.lock().unwrap().host, "external-edit.invalid");
    session
        .model
        .lock()
        .unwrap()
        .settings
        .as_mut()
        .unwrap()
        .config
        .moonlight
        .host = "saved-after-reload.invalid".into();
    session.action(Action::SaveSettings);
    wait(&session);
    assert_eq!(
        session.model.lock().unwrap().host,
        "saved-after-reload.invalid"
    );
    assert!(session.model.lock().unwrap().error.is_none());
    session.action(Action::OpenSettings);
    session
        .model
        .lock()
        .unwrap()
        .settings
        .as_mut()
        .unwrap()
        .config
        .moonlight
        .app = "Second saved app".into();
    session.action(Action::SaveSettings);
    wait(&session);
    assert!(session.model.lock().unwrap().error.is_none());
    assert_eq!(session.model.lock().unwrap().app, "Second saved app");
    drop(session);

    let target_a = directory.join("private/target-a.toml");
    let target_b = directory.join("private/target-b.toml");
    let selected = directory.join("retarget.toml");
    let text_a = toml::to_string(&old).unwrap();
    for (target, text) in [(&target_a, &text_a), (&target_b, &external_text)] {
        fs::write(target, text).unwrap();
        fs::set_permissions(target, fs::Permissions::from_mode(0o600)).unwrap();
    }
    std::os::unix::fs::symlink(&target_a, &selected).unwrap();
    let mut session = Session::new(
        Some(&selected),
        None,
        egui::Context::default(),
        Arc::new(AtomicBool::new(false)),
    );
    session.action(Action::OpenSettings);
    session
        .model
        .lock()
        .unwrap()
        .settings
        .as_mut()
        .unwrap()
        .config
        .moonlight
        .host = "must-not-overwrite-b.invalid".into();
    fs::remove_file(&selected).unwrap();
    std::os::unix::fs::symlink(&target_b, &selected).unwrap();
    session.action(Action::SaveSettings);
    wait(&session);
    assert_eq!(fs::read_to_string(&target_a).unwrap(), text_a);
    assert_eq!(fs::read_to_string(&target_b).unwrap(), external_text);
    assert_eq!(session.model.lock().unwrap().host, old.moonlight.host);
    session.action(Action::ReloadSettings);
    wait(&session);
    assert_eq!(session.model.lock().unwrap().host, "external-edit.invalid");
    drop(session);

    let missing = directory.join("private/created-externally.toml");
    let mut session = Session::new(
        Some(&missing),
        None,
        egui::Context::default(),
        Arc::new(AtomicBool::new(false)),
    );
    session.action(Action::OpenSettings);
    configure(
        &mut session
            .model
            .lock()
            .unwrap()
            .settings
            .as_mut()
            .unwrap()
            .config,
    );
    fs::write(&missing, &external_text).unwrap();
    fs::set_permissions(&missing, fs::Permissions::from_mode(0o600)).unwrap();
    session.action(Action::SaveSettings);
    wait(&session);
    assert!(!session.model.lock().unwrap().configured);
    assert_eq!(fs::read_to_string(&missing).unwrap(), external_text);
    session.action(Action::ReloadSettings);
    wait(&session);
    assert!(session.model.lock().unwrap().configured);
    assert_eq!(session.model.lock().unwrap().host, "external-edit.invalid");
    drop(session);

    let invalid = directory.join("private/invalid-draft.toml");
    fs::write(&invalid, "SYNTHETIC INVALID TOML").unwrap();
    fs::set_permissions(&invalid, fs::Permissions::from_mode(0o600)).unwrap();
    let mut session = Session::new(
        Some(&invalid),
        None,
        egui::Context::default(),
        Arc::new(AtomicBool::new(false)),
    );
    session.action(Action::OpenSettings);
    assert!(
        !session
            .model
            .lock()
            .unwrap()
            .settings
            .as_ref()
            .unwrap()
            .save_blocked
    );
    configure(
        &mut session
            .model
            .lock()
            .unwrap()
            .settings
            .as_mut()
            .unwrap()
            .config,
    );
    session.action(Action::SaveSettings);
    wait(&session);
    assert!(session.model.lock().unwrap().configured);
    drop(session);

    let unsafe_path = directory.join("unsafe-config-directory");
    fs::create_dir(&unsafe_path).unwrap();
    let mut session = Session::new(
        Some(&unsafe_path),
        None,
        egui::Context::default(),
        Arc::new(AtomicBool::new(false)),
    );
    session.action(Action::OpenSettings);
    assert!(
        session
            .model
            .lock()
            .unwrap()
            .settings
            .as_ref()
            .unwrap()
            .save_blocked
    );
    configure(
        &mut session
            .model
            .lock()
            .unwrap()
            .settings
            .as_mut()
            .unwrap()
            .config,
    );
    session.action(Action::SaveSettings);
    assert!(!session.model.lock().unwrap().busy);
    assert!(unsafe_path.is_dir());
    fs::remove_dir(&unsafe_path).unwrap();
    fs::write(&unsafe_path, external_text).unwrap();
    fs::set_permissions(&unsafe_path, fs::Permissions::from_mode(0o600)).unwrap();
    session.action(Action::ReloadSettings);
    wait(&session);
    assert!(session.model.lock().unwrap().configured);
    assert!(
        !session
            .model
            .lock()
            .unwrap()
            .settings
            .as_ref()
            .unwrap()
            .save_blocked
    );
}

#[test]
fn committed_save_outcomes_are_not_reported_as_precommit_failures() {
    use moonboot::config::SaveOutcome;
    let directory = tempfile::tempdir().unwrap();
    let path = directory.path().join("synthetic-config.toml");
    let ctx = egui::Context::default();
    let mut model = Model::new(
        "old-host.invalid".into(),
        "Old Desktop".into(),
        "old-device".into(),
        None,
    );
    model.settings = Some(Draft::new(config(), path.clone()));
    model.error = Some("Previous failure".into());
    model.settings_saved(
        config(),
        SaveOutcome {
            warning: Some("Committed, but durability is uncertain.".into()),
            requires_reload: false,
        },
        path.clone(),
        &ctx,
    );
    assert!(model.configured);
    assert_eq!(model.host, "synthetic-host.invalid");
    assert!(model.error.is_none());
    assert!(model.message.starts_with("Configuration saved."));
    assert!(model.message.contains("durability is uncertain"));
    assert!(!model.message.contains("not saved"));
    assert!(model.settings.is_none());
    model.settings = Some(Draft::new(config(), path.clone()));
    model.status = Some((true, std::time::SystemTime::now()));
    let mut written = config();
    written.moonlight.host = "written-but-not-selected.invalid".into();
    model.settings_saved(
        written,
        SaveOutcome {
            warning: Some("Committed target; displaced backup retained.".into()),
            requires_reload: true,
        },
        path,
        &ctx,
    );
    assert!(!model.configured);
    assert_eq!(model.host, "synthetic-host.invalid");
    assert!(model.status.is_none());
    assert!(model.error.is_none());
    assert!(model.message.contains("Reload from disk before operations"));
    assert!(model.message.contains("displaced backup retained"));
    assert!(!model.message.contains("not saved"));
    assert!(model.settings.as_ref().unwrap().save_blocked);
    assert!(model.settings_allowed());
    let mut harness = Harness::builder()
        .with_size(egui::vec2(800.0, 900.0))
        .build_state(
            |ctx, model: &mut Model| {
                model.draw(ctx);
            },
            model,
        );
    assert!(harness.get_by_label("Save").accesskit_node().is_disabled());
    assert!(harness
        .get_by_label("Start Session")
        .accesskit_node()
        .is_disabled());
    assert!(!harness
        .get_by_label("Reload from disk")
        .accesskit_node()
        .is_disabled());
    harness.run();
}

fn reload_fixture(directory: &std::path::Path) {
    use std::os::fd::{AsRawFd, FromRawFd};
    let path = directory.join("private/reload.toml");
    let mut initial = Config::draft(Some(&path)).unwrap();
    configure(&mut initial);
    initial.save(Some(&path)).unwrap();
    let ui_path = path.clone();
    let mut harness = Harness::builder()
        .with_size(egui::vec2(800.0, 900.0))
        .build_state(
            move |ctx, state: &mut Option<Session>| {
                let session = state.get_or_insert_with(|| {
                    Session::new(
                        Some(&ui_path),
                        None,
                        ctx.clone(),
                        Arc::new(AtomicBool::new(false)),
                    )
                });
                let action = session.model.lock().unwrap().draw(ctx);
                if let Some(action) = action {
                    session.action(action);
                }
            },
            None,
        );
    let mut deployed = config();
    deployed.moonlight.host = "redeployed-host.invalid".into();
    deployed.moonlight.app = "Redeployed Desktop".into();
    deployed.tuya.device_id = "redeployed-device".into();
    deployed.tuya.client_secret = "redeployed-secret".into();
    fs::write(&path, toml::to_string(&deployed).unwrap()).unwrap();
    fs::set_permissions(&path, fs::Permissions::from_mode(0o400)).unwrap();
    harness.get_by_label("Settings").click();
    harness.run();
    // Open uses the active snapshot, not the redeployed file.
    assert_eq!(
        harness
            .state()
            .as_ref()
            .unwrap()
            .model
            .lock()
            .unwrap()
            .settings
            .as_ref()
            .unwrap()
            .config
            .moonlight
            .host,
        "synthetic-host.invalid"
    );
    harness
        .get_by_role_and_label(egui::accesskit::Role::PasswordInput, "Cloud client secret")
        .click();
    harness.run();
    harness.key_press_modifiers(egui::Modifiers::COMMAND, egui::Key::A);
    harness.run();
    harness
        .get_by_role_and_label(egui::accesskit::Role::PasswordInput, "Cloud client secret")
        .type_text("discarded-draft-secret");
    harness.run();
    harness.key_press_modifiers(egui::Modifiers::COMMAND, egui::Key::Z);
    harness.run();
    assert_eq!(
        harness
            .state()
            .as_ref()
            .unwrap()
            .model
            .lock()
            .unwrap()
            .settings
            .as_ref()
            .unwrap()
            .config
            .tuya
            .client_secret,
        "discarded-draft-secret"
    );
    harness
        .state()
        .as_ref()
        .unwrap()
        .model
        .lock()
        .unwrap()
        .status = Some((true, std::time::SystemTime::now()));
    harness.get_by_label("Reload from disk").click();
    harness.step();
    wait(harness.state().as_ref().unwrap());
    harness.run();
    harness.get_by_label("Host: redeployed-host.invalid");
    harness.get_by_label("App: Redeployed Desktop");
    harness.get_by_label("Plug: Unknown / not observed");
    {
        let model = harness.state().as_ref().unwrap().model.lock().unwrap();
        assert_eq!(model.device, "redeployed-device");
        assert!(model.error.is_none());
        assert_eq!(
            model.settings.as_ref().unwrap().config.tuya.client_secret,
            "redeployed-secret"
        );
    }
    harness
        .get_by_role_and_label(egui::accesskit::Role::PasswordInput, "Cloud client secret")
        .click();
    harness.run();
    harness.key_press_modifiers(egui::Modifiers::COMMAND, egui::Key::Z);
    harness.run();
    assert_eq!(
        harness
            .state()
            .as_ref()
            .unwrap()
            .model
            .lock()
            .unwrap()
            .settings
            .as_ref()
            .unwrap()
            .config
            .tuya
            .client_secret,
        "redeployed-secret"
    );

    // Missing, invalid, and lock-busy reloads retain both active metadata and edits.
    let session = harness.state_mut().as_mut().unwrap();
    {
        let mut model = session.model.lock().unwrap();
        model.settings.as_mut().unwrap().config.moonlight.host = "keep-draft.invalid".into();
        model.settings.as_mut().unwrap().config.tuya.client_secret = "keep-draft-secret".into();
        model.status = Some((true, std::time::SystemTime::now()));
    }
    fs::remove_file(&path).unwrap();
    session.action(Action::ReloadSettings);
    wait(session);
    fs::write(&path, "DO-NOT-ECHO-SECRET invalid TOML").unwrap();
    fs::set_permissions(&path, fs::Permissions::from_mode(0o600)).unwrap();
    session.action(Action::ReloadSettings);
    wait(session);
    {
        let model = session.model.lock().unwrap();
        assert_eq!(model.host, "redeployed-host.invalid");
        assert_eq!(
            model.settings.as_ref().unwrap().config.moonlight.host,
            "keep-draft.invalid"
        );
        assert_eq!(
            model.settings.as_ref().unwrap().config.tuya.client_secret,
            "keep-draft-secret"
        );
        assert!(model.status.is_some());
        assert!(!model.error.as_ref().unwrap().contains("DO-NOT-ECHO-SECRET"));
    }
    let lock = backend::operation_lock("operation").unwrap();
    session.action(Action::ReloadSettings);
    wait(session);
    assert_eq!(
        session
            .model
            .lock()
            .unwrap()
            .settings
            .as_ref()
            .unwrap()
            .config
            .moonlight
            .host,
        "keep-draft.invalid"
    );
    drop(lock);
    for phase in [Phase::Waiting, Phase::Streaming, Phase::ConfirmingOff] {
        session.model.lock().unwrap().phase = phase;
        session.action(Action::ReloadSettings);
        assert!(!session.model.lock().unwrap().busy);
    }
    session.model.lock().unwrap().phase = Phase::Idle;
    session.model.lock().unwrap().busy = true;
    session.action(Action::ReloadSettings);
    session.model.lock().unwrap().busy = false;
    session.action(Action::CancelSettings);
    session.action(Action::OpenSettings);
    assert_eq!(
        session
            .model
            .lock()
            .unwrap()
            .settings
            .as_ref()
            .unwrap()
            .config
            .moonlight
            .host,
        "redeployed-host.invalid"
    );

    // Legacy runtime credential files must not be opened or authenticated by reload.
    let credentials = directory.join("reload-credentials-sentinel.toml");
    fs::write(&credentials, "INVALID SECRET FILE MUST NOT BE READ").unwrap();
    let fd = unsafe { libc::inotify_init1(libc::IN_CLOEXEC | libc::IN_NONBLOCK) };
    assert!(fd >= 0);
    let observer = unsafe { fs::File::from_raw_fd(fd) };
    let name = std::ffi::CString::new(credentials.as_os_str().as_encoded_bytes()).unwrap();
    assert!(unsafe { libc::inotify_add_watch(fd, name.as_ptr(), libc::IN_OPEN) } >= 0);
    deployed.tuya.client_id.clear();
    deployed.tuya.client_secret.clear();
    deployed.tuya.credentials_file = Some(credentials);
    deployed.moonlight.host = "external-keys-host.invalid".into();
    fs::write(&path, toml::to_string(&deployed).unwrap()).unwrap();
    fs::set_permissions(&path, fs::Permissions::from_mode(0o400)).unwrap();
    session.action(Action::ReloadSettings);
    wait(session);
    assert_eq!(
        session.model.lock().unwrap().host,
        "external-keys-host.invalid"
    );
    assert!(session.model.lock().unwrap().error.is_none());
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
        std::io::ErrorKind::WouldBlock
    );
    session.action(Action::CancelSettings);
    session.action(Action::OpenSettings);
    assert_eq!(
        session
            .model
            .lock()
            .unwrap()
            .settings
            .as_ref()
            .unwrap()
            .config
            .moonlight
            .host,
        "external-keys-host.invalid"
    );
    deployed.moonlight.host = "hidden-reload-host.invalid".into();
    fs::set_permissions(&path, fs::Permissions::from_mode(0o600)).unwrap();
    fs::write(&path, toml::to_string(&deployed).unwrap()).unwrap();
    session.action(Action::ReloadSettings);
    session.action(Action::Hide);
    wait(session);
    assert!(session.model.lock().unwrap().settings.is_none());
    assert_eq!(
        session.model.lock().unwrap().host,
        "hidden-reload-host.invalid"
    );
    session.open();
    session.action(Action::OpenSettings);
    deployed.moonlight.host = "joined-reload-host.invalid".into();
    fs::write(&path, toml::to_string(&deployed).unwrap()).unwrap();
    let model = session.model.clone();
    session.action(Action::ReloadSettings);
    drop(harness);
    assert_eq!(model.lock().unwrap().host, "joined-reload-host.invalid");
    assert!(model.lock().unwrap().settings.is_none());
}

#[test]
fn settings_save_is_inert_atomic_and_updates_existing_session() {
    let directory = tempfile::tempdir().unwrap();
    fs::set_permissions(directory.path(), fs::Permissions::from_mode(0o700)).unwrap();
    let status = Command::new(std::env::current_exe().unwrap())
        .args([
            "--exact",
            "isolated_settings_worker",
            "--nocapture",
            "--test-threads=1",
        ])
        .env("MOONBOOT_SETTINGS_FIXTURE", directory.path())
        .env("HOME", directory.path())
        .env("XDG_RUNTIME_DIR", directory.path())
        .env("XDG_CONFIG_HOME", directory.path())
        .env("XDG_STATE_HOME", directory.path())
        .env("PATH", "/nonexistent-test-path")
        .status()
        .unwrap();
    assert!(status.success());
}
