use crate::{
    backend::{self, Error, Event, Operation, Phase},
    config::Config,
    demo::{Demo, Scenario},
    ui::{Action, Model},
};
use std::{
    fs::{self, DirBuilder, File, OpenOptions},
    io::{Read, Write},
    os::{
        fd::{AsRawFd, FromRawFd},
        unix::{
            fs::{DirBuilderExt, FileTypeExt, MetadataExt, OpenOptionsExt, PermissionsExt},
            net::{UnixListener, UnixStream},
        },
    },
    path::{Path, PathBuf},
    process::{Child, Command, Stdio},
    sync::{
        atomic::{AtomicBool, Ordering},
        mpsc, Arc, Mutex,
    },
    thread::{self, JoinHandle},
    time::{Duration, Instant},
};

pub fn signal_flag() -> Result<Arc<AtomicBool>, Error> {
    let flag = Arc::new(AtomicBool::new(false));
    for signal in [
        signal_hook::consts::SIGINT,
        signal_hook::consts::SIGTERM,
        signal_hook::consts::SIGHUP,
    ] {
        signal_hook::flag::register(signal, flag.clone())
            .map_err(|_| Error::Dependency("Cannot install cancellation signal handler".into()))?;
    }
    Ok(flag)
}

/// Frontend failures can occur before the backend has a configuration to log with.
pub fn report_failure(error: &Error, demo: Option<Scenario>) {
    log_frontend(&format!("{} {error}", error.code()), demo);
    crate::process::notify(if demo.is_some() {
        "DEMO: simulated operation or controls failed. Open DEMO Controls for details; no real power or network actions occurred."
    } else {
        "Controls failed. Open Moonboot or run moonboot gui in a terminal for details. Power is never automatically reversed."
    });
}

fn log_frontend(message: &str, demo: Option<Scenario>) {
    let base = std::env::var_os("XDG_STATE_HOME")
        .filter(|value| !value.is_empty())
        .map(PathBuf::from)
        .or_else(|| std::env::var_os("HOME").map(|home| PathBuf::from(home).join(".local/state")));
    if let Some(base) = base.filter(|path| path.is_absolute()) {
        let _ = (|| -> std::io::Result<()> {
            fs::create_dir_all(&base)?;
            let path = base.join(namespace(demo, "moonboot"));
            match DirBuilder::new().mode(0o700).create(&path) {
                Ok(()) => {}
                Err(error) if error.kind() == std::io::ErrorKind::AlreadyExists => {}
                Err(error) => return Err(error),
            }
            let directory = OpenOptions::new()
                .read(true)
                .custom_flags(libc::O_DIRECTORY | libc::O_NOFOLLOW | libc::O_CLOEXEC)
                .open(path)?;
            let metadata = directory.metadata()?;
            if metadata.uid() != unsafe { libc::geteuid() } || metadata.mode() & 0o777 != 0o700 {
                return Ok(());
            }
            let fd = unsafe {
                libc::openat(
                    directory.as_raw_fd(),
                    c"frontend.log".as_ptr(),
                    libc::O_WRONLY
                        | libc::O_APPEND
                        | libc::O_CREAT
                        | libc::O_NOFOLLOW
                        | libc::O_CLOEXEC
                        | libc::O_NONBLOCK,
                    0o600,
                )
            };
            if fd < 0 {
                return Err(std::io::Error::last_os_error());
            }
            let mut file = unsafe { File::from_raw_fd(fd) };
            let metadata = file.metadata()?;
            if !metadata.is_file()
                || metadata.uid() != unsafe { libc::geteuid() }
                || metadata.mode() & 0o777 != 0o600
                || metadata.nlink() != 1
            {
                return Ok(());
            }
            if fs2::FileExt::try_lock_exclusive(&file).is_err() {
                return Ok(());
            }
            let timestamp = std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap_or_default()
                .as_secs();
            let message = format!("{timestamp} {message}\n");
            if message.len() > 4096 {
                return Ok(());
            }
            if metadata.len() + message.len() as u64 > 16384 {
                file.set_len(0)?;
            }
            file.write_all(message.as_bytes())
        })();
    }
}

pub fn namespace(demo: Option<Scenario>, suffix: &str) -> String {
    match demo {
        Some(scenario) => format!("demo-{}-{suffix}", scenario.name()),
        None => suffix.into(),
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Request {
    Open,
    Quit,
}

impl Request {
    fn byte(self) -> u8 {
        match self {
            Self::Open => b'O',
            Self::Quit => b'Q',
        }
    }
    fn parse(byte: u8) -> Option<Self> {
        match byte {
            b'O' => Some(Self::Open),
            b'Q' => Some(Self::Quit),
            _ => None,
        }
    }
}

/// The advisory lock is separate from the socket and is never unlinked.
pub struct Instance {
    listener: UnixListener,
    path: PathBuf,
    _lock: File,
}

fn socket_path(demo: Option<Scenario>) -> Result<PathBuf, Error> {
    let runtime = std::env::var_os("XDG_RUNTIME_DIR")
        .map(PathBuf::from)
        .filter(|path| path.is_absolute())
        .ok_or_else(|| {
            Error::Config("XDG_RUNTIME_DIR must be a private absolute runtime directory".into())
        })?;
    let metadata = fs::symlink_metadata(&runtime)
        .map_err(|_| Error::Config("Cannot inspect UI runtime directory".into()))?;
    if !metadata.is_dir()
        || metadata.uid() != unsafe { libc::geteuid() }
        || metadata.mode() & 0o777 != 0o700
    {
        return Err(Error::Config(
            "UI runtime directory must be owned by this user with mode 0700, not a symlink".into(),
        ));
    }
    Ok(runtime.join(format!("moonboot-{}.sock", namespace(demo, "ui"))))
}

fn owned_socket(path: &Path) -> Result<bool, Error> {
    match fs::symlink_metadata(path) {
        Ok(metadata)
            if metadata.file_type().is_socket()
                && metadata.uid() == unsafe { libc::geteuid() }
                && metadata.mode() & 0o777 == 0o600 =>
        {
            Ok(true)
        }
        Ok(_) => Err(Error::Config(
            "UI socket must be a same-user mode-0600 socket; refusing unsafe path".into(),
        )),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(false),
        Err(_) => Err(Error::Config("Cannot inspect UI socket".into())),
    }
}

fn same_user(stream: &UnixStream) -> bool {
    let mut credentials: libc::ucred = unsafe { std::mem::zeroed() };
    let mut length = std::mem::size_of::<libc::ucred>() as libc::socklen_t;
    let result = unsafe {
        libc::getsockopt(
            stream.as_raw_fd(),
            libc::SOL_SOCKET,
            libc::SO_PEERCRED,
            (&mut credentials as *mut libc::ucred).cast(),
            &mut length,
        )
    };
    result == 0
        && length as usize == std::mem::size_of::<libc::ucred>()
        && credentials.uid == unsafe { libc::geteuid() }
}

pub fn request(demo: Option<Scenario>, request: Request) -> Result<bool, Error> {
    let path = socket_path(demo)?;
    if !owned_socket(&path)? {
        return Ok(false);
    }
    let mut stream = match UnixStream::connect(path) {
        Ok(stream) => stream,
        Err(error)
            if matches!(
                error.kind(),
                std::io::ErrorKind::ConnectionRefused | std::io::ErrorKind::NotFound
            ) =>
        {
            return Ok(false)
        }
        Err(_) => return Err(Error::Dependency("Cannot contact existing panel".into())),
    };
    if !same_user(&stream) {
        return Err(Error::Config("UI peer is not the current user".into()));
    }
    stream
        .set_read_timeout(Some(Duration::from_millis(300)))
        .ok();
    stream
        .set_write_timeout(Some(Duration::from_millis(300)))
        .ok();
    let mut ack = [0];
    Ok(stream.write_all(&[request.byte()]).is_ok()
        && stream.read_exact(&mut ack).is_ok()
        && ack == *b"K")
}

impl Instance {
    pub fn claim(demo: Option<Scenario>) -> Result<Option<Self>, Error> {
        let lock = match backend::operation_lock(&namespace(demo, "ui")) {
            Ok(lock) => lock,
            Err(Error::Busy) => {
                let deadline = Instant::now() + Duration::from_secs(3);
                while Instant::now() < deadline {
                    if request(demo, Request::Open)? {
                        return Ok(None);
                    }
                    thread::sleep(Duration::from_millis(30));
                }
                return Err(Error::Busy);
            }
            Err(error) => return Err(error),
        };
        let path = socket_path(demo)?;
        // Only the lock owner may remove a stale socket; never unlink arbitrary files.
        if owned_socket(&path)? {
            fs::remove_file(&path)
                .map_err(|_| Error::Config("Cannot remove stale UI socket".into()))?;
        }
        let listener = UnixListener::bind(&path)
            .map_err(|_| Error::Dependency("Cannot bind private UI socket".into()))?;
        fs::set_permissions(&path, fs::Permissions::from_mode(0o600))
            .map_err(|_| Error::Config("Cannot secure UI socket".into()))?;
        listener
            .set_nonblocking(true)
            .map_err(|_| Error::Dependency("Cannot configure UI socket".into()))?;
        Ok(Some(Self {
            listener,
            path,
            _lock: lock,
        }))
    }

    pub fn receive(&self) -> Option<Request> {
        let (mut stream, _) = self.listener.accept().ok()?;
        if !same_user(&stream) {
            return None;
        }
        stream
            .set_read_timeout(Some(Duration::from_millis(100)))
            .ok();
        stream
            .set_write_timeout(Some(Duration::from_millis(100)))
            .ok();
        let mut byte = [0];
        stream.read_exact(&mut byte).ok()?;
        let request = Request::parse(byte[0])?;
        stream.write_all(b"K").ok()?;
        Some(request)
    }
}

impl Drop for Instance {
    fn drop(&mut self) {
        let _ = fs::remove_file(&self.path);
    }
}

enum Source {
    Production(Arc<Mutex<Option<Config>>>),
    Demo(Arc<Mutex<Demo>>),
}

pub struct Session {
    pub model: Arc<Mutex<Model>>,
    source: Option<Source>,
    config_path: Option<PathBuf>,
    cancel: Arc<AtomicBool>,
    end_stream: Arc<AtomicBool>,
    quit: Arc<AtomicBool>,
    worker: Option<JoinHandle<()>>,
    ctx: egui::Context,
    window_ctx: Option<egui::Context>,
    desired_visible: Arc<AtomicBool>,
}

impl Session {
    /// Demo selection precedes Config::load, including when an explicit config is supplied.
    pub fn new(
        path: Option<&Path>,
        demo: Option<Scenario>,
        ctx: egui::Context,
        quit: Arc<AtomicBool>,
    ) -> Self {
        let (model, source) = if let Some(scenario) = demo {
            (
                Model::new(
                    "demo-host.invalid".into(),
                    "Demo Desktop".into(),
                    "demo-plug".into(),
                    demo,
                ),
                Some(Source::Demo(Arc::new(Mutex::new(Demo::new(scenario))))),
            )
        } else {
            match Config::load(path) {
                Ok(config) => (
                    Model::new(
                        config.moonlight.host.clone(),
                        config.moonlight.app.clone(),
                        config.tuya.device_id.clone(),
                        None,
                    ),
                    Some(Source::Production(Arc::new(Mutex::new(Some(config))))),
                ),
                Err(error) => {
                    report_failure(&error, None);
                    let mut model = Model::new(
                        "Not configured".into(),
                        "Unknown".into(),
                        "Unknown".into(),
                        None,
                    );
                    model.error = Some(error.to_string());
                    model.configured = false;
                    let baseline = match Config::draft(path) {
                        Ok(config) => Some(config),
                        Err(error) => {
                            model.error = Some(format!("Cannot safely inspect configuration: {error}. Saving is blocked; repair the selected file and Reload from disk."));
                            None
                        }
                    };
                    (
                        model,
                        Some(Source::Production(Arc::new(Mutex::new(baseline)))),
                    )
                }
            }
        };
        Self {
            model: Arc::new(Mutex::new(model)),
            source,
            config_path: if demo.is_none() {
                Config::path(path).ok()
            } else {
                None
            },
            cancel: Arc::new(AtomicBool::new(false)),
            end_stream: Arc::new(AtomicBool::new(false)),
            quit,
            worker: None,
            ctx,
            window_ctx: None,
            desired_visible: Arc::new(AtomicBool::new(true)),
        }
    }

    pub fn action(&mut self, action: Action) {
        match action {
            Action::OpenSettings => self.open_settings(),
            Action::SaveSettings => self.save_settings(),
            Action::ReloadSettings => self.reload_settings(),
            Action::CancelSettings => self.model.lock().unwrap().discard_settings(&self.ctx),
            Action::Run(operation) => self.start(operation),
            Action::Cancel => {
                self.cancel.store(true, Ordering::Relaxed);
                self.model.lock().unwrap().decline();
            }
            Action::EndDemo => {
                self.end_stream.store(true, Ordering::Relaxed);
            }
            Action::Hide => {
                let mut model = self.model.lock().unwrap();
                model.discard_settings(&self.ctx);
                model.set_visible(false);
                drop(model);
                self.desired_visible.store(false, Ordering::Relaxed);
                if let Some(ctx) = &self.window_ctx {
                    ctx.send_viewport_cmd(egui::ViewportCommand::Close);
                    ctx.request_repaint();
                }
            }
            Action::Quit => {
                self.quit.store(true, Ordering::Relaxed);
                self.cancel.store(true, Ordering::Relaxed);
                let mut model = self.model.lock().unwrap();
                model.discard_settings(&self.ctx);
                model.set_visible(false);
                drop(model);
                self.desired_visible.store(false, Ordering::Relaxed);
                if let Some(ctx) = &self.window_ctx {
                    ctx.send_viewport_cmd(egui::ViewportCommand::Close);
                    ctx.request_repaint();
                }
            }
        }
    }

    fn open_settings(&mut self) {
        if self.quit.load(Ordering::Relaxed) {
            return;
        }
        let Some(Source::Production(active)) = &self.source else {
            return;
        };
        let config = active.lock().unwrap().clone();
        let mut model = self.model.lock().unwrap();
        if !model.settings_allowed() {
            return;
        }
        let Some(path) = &self.config_path else {
            model.error = Some(
                "Cannot resolve configuration location; supply an absolute --config path.".into(),
            );
            return;
        };
        model.discard_settings(&self.ctx);
        let unknown = config.is_none();
        let mut draft = crate::ui::settings::Draft::new(config.unwrap_or_default(), path.clone());
        if unknown {
            draft.save_blocked = true;
            draft.warning = Some("No safely observed configuration is available. Saving is blocked; repair the selected file and Reload from disk before saving or running operations.".into());
        }
        model.settings = Some(draft);
    }

    fn save_settings(&mut self) {
        if self.quit.load(Ordering::Relaxed) {
            return;
        }
        // Reject forged demo actions before resolving paths or touching the filesystem.
        let Some(Source::Production(active)) = &self.source else {
            return;
        };
        if active.lock().unwrap().is_none() {
            return;
        }
        let Some(path) = self.config_path.clone() else {
            return;
        };
        let mut config = {
            let mut model = self.model.lock().unwrap();
            if !model.settings_allowed() {
                return;
            }
            let Some(draft) = &model.settings else {
                return;
            };
            if draft.save_blocked {
                return;
            }
            let config = draft.config.clone();
            model.busy = true;
            model.message = "Saving configuration locally...".into();
            config
        };
        if let Some(worker) = self.worker.take() {
            let _ = worker.join();
        }
        let active = active.clone();
        let model = self.model.clone();
        let ctx = self.ctx.clone();
        self.worker = Some(thread::spawn(move || {
            let result = (|| {
                let _lock = backend::operation_lock("operation")?;
                let outcome = config.save(Some(&path))?;
                *active.lock().unwrap() = if outcome.requires_reload {
                    None
                } else {
                    Some(config.clone())
                };
                Ok::<_, Error>(outcome)
            })();
            let mut model = model.lock().unwrap();
            match result {
                Ok(outcome) => model.settings_saved(config, outcome, path, &ctx),
                Err(error) => {
                    let message = format!("Settings were not saved: {error}. Reload from disk after external edits or retargeting. For read-only agenix/Nix targets, edit the encrypted/declarative source and redeploy; for a busy operation, wait and retry.");
                    if let Some(draft) = &mut model.settings {
                        draft.error = Some(message.clone());
                    }
                    model.error = Some(message);
                    model.message = "Configuration unchanged; correct Settings and retry, or Cancel to discard the draft.".into();
                }
            }
            model.busy = false;
            drop(model);
            ctx.request_repaint();
        }));
    }

    fn reload_settings(&mut self) {
        if self.quit.load(Ordering::Relaxed) {
            return;
        }
        // Demo actions must return before even looking up a production path.
        let Some(Source::Production(active)) = &self.source else {
            return;
        };
        let Some(path) = self.config_path.clone() else {
            return;
        };
        {
            let mut model = self.model.lock().unwrap();
            if !model.settings_allowed() || model.settings.is_none() {
                return;
            }
            model.busy = true;
            model.message = "Reloading configuration locally...".into();
        }
        if let Some(worker) = self.worker.take() {
            let _ = worker.join();
        }
        let active = active.clone();
        let model = self.model.clone();
        let ctx = self.ctx.clone();
        self.worker = Some(thread::spawn(move || {
            let result = (|| {
                let _lock = backend::operation_lock("operation")?;
                let config = Config::load(Some(&path))?;
                *active.lock().unwrap() = Some(config.clone());
                Ok::<_, Error>(config)
            })();
            let mut model = model.lock().unwrap();
            match result {
                Ok(config) => {
                    model.host = config.moonlight.host.clone();
                    model.app = config.moonlight.app.clone();
                    model.device = config.tuya.device_id.clone();
                    model.status = None;
                    model.configured = true;
                    model.error = None;
                    model.message =
                        "Configuration reloaded. Status is Unknown until explicitly refreshed."
                            .into();
                    if model.settings.is_some() {
                        model.discard_settings(&ctx);
                        model.settings = Some(crate::ui::settings::Draft::new(config, path));
                    }
                }
                Err(error) => {
                    let message = format!("Settings were not reloaded: {error}. Active configuration and unsaved edits are unchanged; check the deployed configuration or wait for the current operation and retry.");
                    if let Some(draft) = &mut model.settings {
                        draft.error = Some(message.clone());
                    }
                    model.error = Some(message);
                    model.message = "Configuration unchanged; reload failed.".into();
                }
            }
            model.busy = false;
            drop(model);
            ctx.request_repaint();
        }));
    }

    pub fn open(&mut self) {
        if self.quit.load(Ordering::Relaxed) {
            return;
        }
        self.model.lock().unwrap().set_visible(true);
        self.desired_visible.store(true, Ordering::Relaxed);
        if let Some(ctx) = &self.window_ctx {
            ctx.send_viewport_cmd(egui::ViewportCommand::Focus);
            ctx.request_repaint();
        }
    }

    fn attach_panel(&mut self, ctx: egui::Context) {
        self.ctx = ctx.clone();
        self.window_ctx = Some(ctx);
    }

    fn detach_panel(&mut self) {
        self.window_ctx = None;
        self.ctx = egui::Context::default();
        // Do not clear desired_visible: Open may have arrived while Close completed.
    }

    fn start(&mut self, operation: Operation) {
        if self.quit.load(Ordering::Relaxed) || self.source.is_none() {
            return;
        }
        {
            let mut model = self.model.lock().unwrap();
            if model.busy
                || !model.configured
                || model.phase != Phase::Idle
                || model.settings.is_some()
            {
                return;
            }
            model.busy = true;
            model.error = None;
            model.message = "Validating requested operation...".into();
            model.decline();
            model.discard_settings(&self.ctx);
        }
        if let Some(worker) = self.worker.take() {
            let _ = worker.join();
        }
        self.cancel.store(false, Ordering::Relaxed);
        if self.quit.load(Ordering::Relaxed) {
            self.cancel.store(true, Ordering::Relaxed);
        }
        self.end_stream.store(false, Ordering::Relaxed);
        let model = self.model.clone();
        let cancel = self.cancel.clone();
        let quit = self.quit.clone();
        let end = self.end_stream.clone();
        let ctx = self.ctx.clone();
        let source = match self.source.as_ref().unwrap() {
            Source::Production(config) => Source::Production(config.clone()),
            Source::Demo(demo) => Source::Demo(demo.clone()),
        };
        self.worker = Some(thread::spawn(move || {
            let (approval_tx, approval_rx) = mpsc::channel();
            let emit = |event| {
                let confirming = event == Event::Phase(Phase::ConfirmingOff);
                let mut state = model.lock().unwrap();
                state.event(event);
                if confirming {
                    state.begin_confirmation(approval_tx.clone());
                }
                drop(state);
                ctx.request_repaint();
            };
            let approve = || loop {
                if cancel.load(Ordering::Relaxed) || quit.load(Ordering::Relaxed) {
                    return false;
                }
                match approval_rx.recv_timeout(Duration::from_millis(20)) {
                    Ok(approved) => return approved,
                    Err(mpsc::RecvTimeoutError::Disconnected) => return false,
                    Err(mpsc::RecvTimeoutError::Timeout) => {}
                }
            };
            let result = match source {
                Source::Production(config) => {
                    let config = config.lock().unwrap().clone();
                    match config {
                        Some(config) => {
                            backend::run(&config, operation, cancel.clone(), emit, approve)
                        }
                        None => Err(Error::Config(
                            "No active configuration; open Settings.".into(),
                        )),
                    }
                }
                Source::Demo(demo) => demo
                    .lock()
                    .unwrap()
                    .run(operation, &cancel, &end, emit, approve),
            };
            let demo = model.lock().unwrap().demo;
            if let (Some(scenario), Err(error)) = (demo, &result) {
                report_failure(error, Some(scenario));
            }
            model.lock().unwrap().finish(result);
            ctx.request_repaint();
        }));
    }
}

impl Drop for Session {
    fn drop(&mut self) {
        self.cancel.store(true, Ordering::Relaxed);
        self.model.lock().unwrap().decline();
        self.model.lock().unwrap().discard_settings(&self.ctx);
        if let Some(worker) = self.worker.take() {
            let _ = worker.join();
        }
    }
}

struct Panel {
    session: Arc<Mutex<Session>>,
    closing: bool,
}
impl Panel {
    fn draw(&mut self, ctx: &egui::Context) {
        let mut session = self.session.lock().unwrap();
        // A queued Close still belongs to this panel, even if Open requested its replacement.
        if self.closing
            || session.quit.load(Ordering::Relaxed)
            || !session.desired_visible.load(Ordering::Relaxed)
        {
            self.closing = true;
            ctx.send_viewport_cmd(egui::ViewportCommand::Close);
            return;
        }
        if ctx.input(|input| input.viewport().close_requested()) {
            self.closing = true;
            session.action(Action::Hide);
            return;
        }
        let action = session.model.lock().unwrap().draw(ctx);
        if let Some(action) = action {
            if matches!(action, Action::Hide | Action::Quit) {
                self.closing = true;
            }
            session.action(action);
        }
        // A surviving worker may still repaint the context of a destroyed window.
        if session.model.lock().unwrap().busy {
            ctx.request_repaint_after(Duration::from_millis(100));
        }
    }
}
impl eframe::App for Panel {
    fn persist_egui_memory(&self) -> bool {
        false
    }
    fn update(&mut self, ctx: &egui::Context, _frame: &mut eframe::Frame) {
        self.draw(ctx);
    }
}

pub fn gui(
    path: Option<PathBuf>,
    demo: Option<Scenario>,
    quit: Arc<AtomicBool>,
) -> Result<(), Error> {
    let Some(instance) = Instance::claim(demo)? else {
        return Ok(());
    };
    let session = Arc::new(Mutex::new(Session::new(
        path.as_deref(),
        demo,
        egui::Context::default(),
        quit.clone(),
    )));
    let desired_visible = session.lock().unwrap().desired_visible.clone();
    let monitor_session = session.clone();
    let monitor_quit = quit.clone();
    let monitor = thread::spawn(move || {
        while !monitor_quit.load(Ordering::Relaxed) {
            match instance.receive() {
                Some(Request::Open) => monitor_session.lock().unwrap().open(),
                Some(Request::Quit) => monitor_session.lock().unwrap().action(Action::Quit),
                None => {}
            }
            thread::sleep(Duration::from_millis(20));
        }
        monitor_session.lock().unwrap().action(Action::Quit);
    });
    let result = (|| {
        while !quit.load(Ordering::Relaxed) {
            if !desired_visible.load(Ordering::Relaxed) {
                thread::sleep(Duration::from_millis(20));
                continue;
            }
            let panel_session = session.clone();
            let options = eframe::NativeOptions {
                run_and_return: true,
                viewport: egui::ViewportBuilder::default()
                    .with_app_id(namespace(demo, "moonboot"))
                    .with_title(if demo.is_some() {
                        "Moonboot DEMO"
                    } else {
                        "Moonboot"
                    })
                    .with_inner_size([520.0, 440.0])
                    .with_min_inner_size([360.0, 320.0]),
                ..Default::default()
            };
            // All runs occur on this main thread, reusing eframe's cached event loop.
            let window_result = eframe::run_native(
                "Moonboot",
                options,
                Box::new(move |creation| {
                    creation.egui_ctx.set_visuals(egui::Visuals::dark());
                    panel_session
                        .lock()
                        .unwrap()
                        .attach_panel(creation.egui_ctx.clone());
                    Ok(Box::new(Panel {
                        session: panel_session,
                        closing: false,
                    }))
                }),
            );
            session.lock().unwrap().detach_panel();
            window_result.map_err(|_| Error::Dependency("Cannot open graphical panel; run in a Wayland desktop session with graphics libraries available".into()))?;
        }
        Ok(())
    })();
    quit.store(true, Ordering::Relaxed);
    let _ = monitor.join();
    drop(session);
    result
}

struct Tray {
    demo: Option<Scenario>,
    actions: mpsc::Sender<Request>,
}
impl ksni::Tray for Tray {
    fn id(&self) -> String {
        namespace(self.demo, "moonboot")
    }
    fn title(&self) -> String {
        if self.demo.is_some() {
            "Moonboot DEMO".into()
        } else {
            "Moonboot".into()
        }
    }
    fn tool_tip(&self) -> ksni::ToolTip {
        ksni::ToolTip {
            title: self.title(),
            description: "Open Controls; no power actions in tray".into(),
            ..Default::default()
        }
    }
    fn icon_pixmap(&self) -> Vec<ksni::Icon> {
        let mut data = Vec::with_capacity(32 * 32 * 4);
        for y in 0_i32..32 {
            for x in 0_i32..32 {
                let moon = (x - 15).pow(2) + (y - 15).pow(2) < 169
                    && (x - 22).pow(2) + (y - 10).pow(2) > 100;
                data.extend_from_slice(if moon {
                    &[255, 246, 194, 87]
                } else {
                    &[0, 0, 0, 0]
                });
            }
        }
        vec![ksni::Icon {
            width: 32,
            height: 32,
            data,
        }]
    }
    fn activate(&mut self, _: i32, _: i32) {
        let _ = self.actions.send(Request::Open);
    }
    fn menu(&self) -> Vec<ksni::MenuItem<Self>> {
        vec![
            ksni::menu::StandardItem {
                label: if self.demo.is_some() {
                    "Open DEMO Controls".into()
                } else {
                    "Open Controls".into()
                },
                activate: Box::new(|tray: &mut Self| {
                    let _ = tray.actions.send(Request::Open);
                }),
                ..Default::default()
            }
            .into(),
            ksni::menu::StandardItem {
                label: "Quit".into(),
                activate: Box::new(|tray: &mut Self| {
                    let _ = tray.actions.send(Request::Quit);
                }),
                ..Default::default()
            }
            .into(),
        ]
    }
    fn watcher_offline(&self, _: ksni::OfflineReason) -> bool {
        let message = "Tray host unavailable. Enable Waybar's tray module or use moonboot gui; waiting for tray host recovery.";
        eprintln!("{message}");
        // Waybar can start after the tray, or restart while the controller stays alive.
        log_frontend(&format!("waiting {message}"), self.demo);
        true
    }
    fn watcher_online(&self) {
        let message = "Tray host available; resuming Moonboot icon registration.";
        eprintln!("{message}");
        log_frontend(&format!("info {message}"), self.demo);
    }
}

struct PanelChild(Child);
impl Drop for PanelChild {
    fn drop(&mut self) {
        if !matches!(self.0.try_wait(), Ok(None)) {
            return;
        }
        // An owned, unreaped child pins its PID until try_wait reports exit.
        unsafe {
            libc::kill(self.0.id() as libc::pid_t, libc::SIGTERM);
        }
        let deadline = Instant::now() + Duration::from_secs(2);
        while Instant::now() < deadline {
            if !matches!(self.0.try_wait(), Ok(None)) {
                return;
            }
            thread::sleep(Duration::from_millis(20));
        }
        if matches!(self.0.try_wait(), Ok(None)) {
            let _ = self.0.kill();
            let _ = self.0.wait();
        }
    }
}

pub fn tray(
    path: Option<PathBuf>,
    demo: Option<Scenario>,
    quit: Arc<AtomicBool>,
) -> Result<(), Error> {
    use ksni::blocking::TrayMethods;
    let _lock = backend::operation_lock(&namespace(demo, "tray"))?;
    let (actions, receiver) = mpsc::channel();
    let handle = Tray { demo, actions }
        .assume_sni_available(true)
        .spawn()
        .map_err(|_| {
            Error::Dependency(
                "Cannot register tray on session D-Bus; use moonboot gui in your desktop session"
                    .into(),
            )
        })?;
    let mut child: Option<PanelChild> = None;
    while !quit.load(Ordering::Relaxed) {
        if handle.is_closed() {
            return Err(Error::Dependency(
                "Tray D-Bus service stopped; use moonboot gui".into(),
            ));
        }
        if let Some(process) = child.as_mut() {
            if process
                .0
                .try_wait()
                .map_err(|_| Error::Dependency("Cannot reap panel process".into()))?
                .is_some()
            {
                child = None;
            }
        }
        match receiver.recv_timeout(Duration::from_millis(50)) {
            Ok(Request::Quit) => {
                quit.store(true, Ordering::Relaxed);
            }
            Ok(Request::Open) => {
                if request(demo, Request::Open)? {
                    continue;
                }
                if child.is_some() {
                    continue;
                }
                let executable = std::env::current_exe()
                    .map_err(|_| Error::Dependency("Cannot locate panel executable".into()))?;
                let mut command = Command::new(executable);
                command
                    .arg("gui")
                    .stdin(Stdio::null())
                    .stdout(Stdio::null());
                if let Some(path) = &path {
                    command.arg("--config").arg(path);
                }
                if let Some(scenario) = demo {
                    command.arg("--demo").arg(scenario.name());
                }
                child = Some(PanelChild(command.spawn().map_err(|_| {
                    Error::Dependency("Cannot launch controls; try moonboot gui".into())
                })?));
            }
            Err(mpsc::RecvTimeoutError::Timeout) => {}
            Err(mpsc::RecvTimeoutError::Disconnected) => {
                quit.store(true, Ordering::Relaxed);
            }
        }
    }
    // Also stop standalone controls even when this tray never opened or spawned them.
    let _ = request(demo, Request::Quit);
    // Signal the owned child as well: it might still be initializing its socket.
    drop(child);
    handle.shutdown().wait();
    Ok(())
}

/// Reads canonical terminal input without a blocking read_line, so signals cancel prompts.
pub fn terminal_approval(host: &str, device: &str, cancel: &AtomicBool) -> bool {
    if unsafe { libc::isatty(libc::STDIN_FILENO) } != 1 {
        return false;
    }
    eprintln!("Host: {host}\nPlug device: {device}\nThis cuts AC power, not Linux shutdown. Data loss may occur if Linux is running.\nVerify shutdown has fully completed independently. Other automation can change power outside this lock.\nType exactly POWER OFF and press Enter to confirm:");
    let mut input = Vec::new();
    loop {
        if cancel.load(Ordering::Relaxed) {
            return false;
        }
        let mut fd = libc::pollfd {
            fd: libc::STDIN_FILENO,
            events: libc::POLLIN,
            revents: 0,
        };
        let ready = unsafe { libc::poll(&mut fd, 1, 50) };
        if ready < 0 {
            if std::io::Error::last_os_error().kind() == std::io::ErrorKind::Interrupted {
                continue;
            }
            return false;
        }
        if ready == 0 {
            continue;
        }
        let mut byte = 0_u8;
        if unsafe { libc::read(libc::STDIN_FILENO, (&mut byte as *mut u8).cast(), 1) } != 1 {
            return false;
        }
        if byte == b'\n' {
            return input == b"POWER OFF" && !cancel.load(Ordering::Relaxed);
        }
        input.push(byte);
        if input.len() > 128 {
            return false;
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn panel_worker_fixture() {
        if std::env::var_os("MOONBOOT_WINDOW_WORKER_FIXTURE").is_none() {
            return;
        }
        let session = Arc::new(Mutex::new(Session::new(
            None,
            Some(Scenario::Success),
            egui::Context::default(),
            Arc::new(AtomicBool::new(false)),
        )));
        session
            .lock()
            .unwrap()
            .attach_panel(egui::Context::default());
        let panel = Panel {
            session: session.clone(),
            closing: false,
        };
        session
            .lock()
            .unwrap()
            .action(Action::Run(Operation::Start));
        session.lock().unwrap().action(Action::Hide);
        drop(panel);
        session.lock().unwrap().detach_panel();
        let deadline = Instant::now() + Duration::from_secs(6);
        while session.lock().unwrap().model.lock().unwrap().phase != Phase::Streaming {
            assert!(Instant::now() < deadline);
            thread::sleep(Duration::from_millis(10));
        }
        for _ in 0..3 {
            session.lock().unwrap().open();
            let ctx = egui::Context::default();
            session.lock().unwrap().attach_panel(ctx.clone());
            let mut panel = Panel {
                session: session.clone(),
                closing: false,
            };
            let output = ctx.run(Default::default(), |ctx| panel.draw(ctx));
            assert!(
                output.viewport_output[&egui::ViewportId::ROOT].repaint_delay
                    <= Duration::from_millis(100)
            );
            session.lock().unwrap().action(Action::Hide);
            drop(panel);
            session.lock().unwrap().detach_panel();
            assert_eq!(
                session.lock().unwrap().model.lock().unwrap().phase,
                Phase::Streaming
            );
            assert!(!session.lock().unwrap().cancel.load(Ordering::Relaxed));
            assert!(matches!(
                backend::operation_lock("demo-success-operation"),
                Err(Error::Busy)
            ));
        }
        session.lock().unwrap().action(Action::EndDemo);
        while session.lock().unwrap().model.lock().unwrap().busy {
            assert!(Instant::now() < deadline);
            thread::sleep(Duration::from_millis(10));
        }
        if let Some(Source::Demo(demo)) = &session.lock().unwrap().source {
            let demo = demo.lock().unwrap();
            assert_eq!(demo.streams, 1);
            assert_eq!(demo.commands, [true]);
        } else {
            panic!("demo source was replaced");
        }
        session.lock().unwrap().action(Action::Quit);
        drop(session);
        assert!(backend::operation_lock("demo-success-operation").is_ok());
    }

    #[test]
    fn worker_survives_panel_destruction_and_recreation_in_isolated_process() {
        let directory = tempfile::tempdir().unwrap();
        fs::set_permissions(directory.path(), fs::Permissions::from_mode(0o700)).unwrap();
        let mut child = Command::new(std::env::current_exe().unwrap())
            .args([
                "--exact",
                "controller::tests::panel_worker_fixture",
                "--nocapture",
                "--test-threads=1",
            ])
            .env("MOONBOOT_WINDOW_WORKER_FIXTURE", "1")
            .env("XDG_RUNTIME_DIR", directory.path())
            .env("XDG_STATE_HOME", directory.path())
            .env("HOME", directory.path())
            .env("PATH", "/nonexistent-test-path")
            .spawn()
            .unwrap();
        let deadline = Instant::now() + Duration::from_secs(10);
        loop {
            if let Some(status) = child.try_wait().unwrap() {
                assert!(status.success());
                break;
            }
            if Instant::now() >= deadline {
                let _ = child.kill();
                let _ = child.wait();
                panic!("panel lifecycle fixture did not exit");
            }
            thread::sleep(Duration::from_millis(10));
        }
    }

    #[test]
    fn closing_panel_preserves_session_and_open_survives_window_teardown() {
        let session = Arc::new(Mutex::new(Session::new(
            None,
            Some(Scenario::Success),
            egui::Context::default(),
            Arc::new(AtomicBool::new(false)),
        )));
        let ctx = egui::Context::default();
        session.lock().unwrap().attach_panel(ctx.clone());
        {
            let session = session.lock().unwrap();
            let mut model = session.model.lock().unwrap();
            model.busy = true;
            model.event(Event::Phase(Phase::Waiting));
        }
        let mut panel = Panel {
            session: session.clone(),
            closing: false,
        };
        let mut input = egui::RawInput::default();
        input
            .viewports
            .get_mut(&egui::ViewportId::ROOT)
            .unwrap()
            .events
            .push(egui::ViewportEvent::Close);
        let output = ctx.run(input, |ctx| panel.draw(ctx));
        let commands = &output.viewport_output[&egui::ViewportId::ROOT].commands;
        assert!(commands
            .iter()
            .any(|command| matches!(command, egui::ViewportCommand::Close)));
        assert!(!commands.iter().any(|command| matches!(
            command,
            egui::ViewportCommand::CancelClose | egui::ViewportCommand::Visible(_)
        )));
        assert!(!session
            .lock()
            .unwrap()
            .desired_visible
            .load(Ordering::Relaxed));
        assert!(!session.lock().unwrap().cancel.load(Ordering::Relaxed));
        // IPC Open received during Close must not be erased when run_native returns.
        session.lock().unwrap().open();
        let mut input = egui::RawInput::default();
        input
            .viewports
            .get_mut(&egui::ViewportId::ROOT)
            .unwrap()
            .events
            .push(egui::ViewportEvent::Close);
        let output = ctx.run(input, |ctx| panel.draw(ctx));
        assert!(output.viewport_output[&egui::ViewportId::ROOT]
            .commands
            .iter()
            .any(|command| matches!(command, egui::ViewportCommand::Close)));
        session.lock().unwrap().detach_panel();
        drop(panel);
        assert!(session.lock().unwrap().window_ctx.is_none());
        assert!(session
            .lock()
            .unwrap()
            .desired_visible
            .load(Ordering::Relaxed));
        assert!(!session.lock().unwrap().cancel.load(Ordering::Relaxed));
        assert_eq!(
            session.lock().unwrap().model.lock().unwrap().phase,
            Phase::Waiting
        );
        let ctx = egui::Context::default();
        session.lock().unwrap().attach_panel(ctx.clone());
        let mut panel = Panel {
            session: session.clone(),
            closing: false,
        };
        let output = ctx.run(Default::default(), |ctx| panel.draw(ctx));
        assert!(
            output.viewport_output[&egui::ViewportId::ROOT].repaint_delay
                <= Duration::from_millis(100)
        );
        assert!(session.lock().unwrap().model.lock().unwrap().busy);
        session.lock().unwrap().action(Action::Quit);
    }

    #[test]
    fn quit_during_panel_initialization_closes_late_window_and_declines_approval() {
        let session = Arc::new(Mutex::new(Session::new(
            None,
            Some(Scenario::Success),
            egui::Context::default(),
            Arc::new(AtomicBool::new(false)),
        )));
        let (tx, rx) = mpsc::channel();
        session
            .lock()
            .unwrap()
            .model
            .lock()
            .unwrap()
            .begin_confirmation(tx);
        session.lock().unwrap().action(Action::Quit);
        assert!(!rx.recv_timeout(Duration::from_secs(1)).unwrap());
        session.lock().unwrap().open();
        let ctx = egui::Context::default();
        session.lock().unwrap().attach_panel(ctx.clone());
        let mut panel = Panel {
            session: session.clone(),
            closing: false,
        };
        let output = ctx.run(Default::default(), |ctx| panel.draw(ctx));
        assert!(output.viewport_output[&egui::ViewportId::ROOT]
            .commands
            .iter()
            .any(|command| matches!(command, egui::ViewportCommand::Close)));
        assert!(!session
            .lock()
            .unwrap()
            .desired_visible
            .load(Ordering::Relaxed));
        assert!(session.lock().unwrap().cancel.load(Ordering::Relaxed));
        session.lock().unwrap().detach_panel();
    }

    #[test]
    fn hide_open_close_event_preserves_reopen_without_reviving_old_panel() {
        use egui_kittest::{kittest::Queryable, Harness};
        let session = Arc::new(Mutex::new(Session::new(
            None,
            Some(Scenario::Success),
            egui::Context::default(),
            Arc::new(AtomicBool::new(false)),
        )));
        let mut harness = Harness::builder()
            .with_size(egui::vec2(700.0, 620.0))
            .build_state(
                |ctx, panel: &mut Panel| panel.draw(ctx),
                Panel {
                    session: session.clone(),
                    closing: false,
                },
            );
        session.lock().unwrap().attach_panel(harness.ctx.clone());
        harness.get_by_label("Hide").click();
        // A native Close ends run_native; the headless harness cannot consume it.
        harness.step();
        assert!(harness.output().viewport_output[&egui::ViewportId::ROOT]
            .commands
            .iter()
            .any(|command| matches!(command, egui::ViewportCommand::Close)));
        assert!(!session.lock().unwrap().quit.load(Ordering::Relaxed));
        assert!(!session.lock().unwrap().cancel.load(Ordering::Relaxed));
        assert!(harness.state().closing);
        // egui-winit delivers the queued native Close on a later frame, after IPC Open.
        session.lock().unwrap().open();
        harness
            .input_mut()
            .viewports
            .get_mut(&egui::ViewportId::ROOT)
            .unwrap()
            .events
            .push(egui::ViewportEvent::Close);
        harness.step();
        let commands = &harness.output().viewport_output[&egui::ViewportId::ROOT].commands;
        assert!(commands
            .iter()
            .any(|command| matches!(command, egui::ViewportCommand::Close)));
        assert!(!commands.iter().any(|command| matches!(
            command,
            egui::ViewportCommand::CancelClose | egui::ViewportCommand::Visible(_)
        )));
        assert!(harness.query_by_label("Start Session").is_none());
        assert!(session
            .lock()
            .unwrap()
            .desired_visible
            .load(Ordering::Relaxed));
        session.lock().unwrap().detach_panel();
        drop(harness);
        assert!(session
            .lock()
            .unwrap()
            .desired_visible
            .load(Ordering::Relaxed));
        assert!(!session.lock().unwrap().cancel.load(Ordering::Relaxed));
        let ctx = egui::Context::default();
        session.lock().unwrap().attach_panel(ctx.clone());
        let mut replacement = Panel {
            session: session.clone(),
            closing: false,
        };
        let output = ctx.run(Default::default(), |ctx| replacement.draw(ctx));
        assert!(!replacement.closing);
        assert!(!output.viewport_output[&egui::ViewportId::ROOT]
            .commands
            .iter()
            .any(|command| matches!(command, egui::ViewportCommand::Close)));
        session.lock().unwrap().detach_panel();
    }

    #[test]
    fn panel_child_fixture() {
        let Some(ready) = std::env::var_os("MOONBOOT_PANEL_CHILD_READY") else {
            return;
        };
        let cancel = signal_flag().unwrap();
        let ignore = std::env::var_os("MOONBOOT_PANEL_CHILD_IGNORE_TERM").is_some();
        fs::write(&ready, "ready").unwrap();
        while ignore || !cancel.load(Ordering::Relaxed) {
            thread::sleep(Duration::from_millis(10));
        }
        fs::write(PathBuf::from(ready).with_extension("finished"), "finished").unwrap();
    }

    #[test]
    fn panel_child_shutdown_is_bounded_and_reaps_cooperative_or_stuck_children() {
        for ignore in [false, true] {
            let directory = tempfile::tempdir().unwrap();
            let ready = directory.path().join("ready");
            let mut command = Command::new(std::env::current_exe().unwrap());
            command
                .args([
                    "--exact",
                    "controller::tests::panel_child_fixture",
                    "--nocapture",
                ])
                .env("MOONBOOT_PANEL_CHILD_READY", &ready)
                .stdin(Stdio::null())
                .stdout(Stdio::null())
                .stderr(Stdio::null());
            if ignore {
                command.env("MOONBOOT_PANEL_CHILD_IGNORE_TERM", "1");
            }
            let mut child = PanelChild(command.spawn().unwrap());
            let pid = child.0.id() as libc::pid_t;
            let deadline = Instant::now() + Duration::from_secs(3);
            while !ready.exists() {
                assert!(child.0.try_wait().unwrap().is_none());
                assert!(Instant::now() < deadline);
                thread::sleep(Duration::from_millis(10));
            }
            let start = Instant::now();
            drop(child);
            assert!(start.elapsed() < Duration::from_secs(4));
            assert_eq!(ready.with_extension("finished").exists(), !ignore);
            let mut status = 0;
            // ECHILD proves the owned process was reaped, not left alive or as a zombie.
            assert_eq!(
                unsafe { libc::waitpid(pid, &mut status, libc::WNOHANG) },
                -1
            );
            assert_eq!(
                std::io::Error::last_os_error().raw_os_error(),
                Some(libc::ECHILD)
            );
        }
        let mut child = PanelChild(
            Command::new(std::env::current_exe().unwrap())
                .args(["--exact", "controller::tests::panel_child_fixture"])
                .env_remove("MOONBOOT_PANEL_CHILD_READY")
                .stdout(Stdio::null())
                .stderr(Stdio::null())
                .spawn()
                .unwrap(),
        );
        assert!(child.0.wait().unwrap().success());
        let start = Instant::now();
        drop(child);
        assert!(start.elapsed() < Duration::from_secs(1));
    }
}
