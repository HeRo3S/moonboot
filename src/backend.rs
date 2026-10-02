use crate::{
    cloud::{Cloud, Plug},
    config::Config,
    process,
};
use fs2::FileExt;
use std::{
    ffi::CString,
    fs::{DirBuilder, File, OpenOptions},
    io::Write,
    os::{
        fd::{AsRawFd, FromRawFd},
        unix::fs::{DirBuilderExt, MetadataExt, OpenOptionsExt},
    },
    path::{Path, PathBuf},
    sync::{
        atomic::{AtomicBool, Ordering},
        Arc,
    },
    thread,
    time::{Duration, Instant},
};

#[derive(Debug, thiserror::Error)]
pub enum Error {
    #[error("Configuration error: {0}")]
    Config(String),
    #[error("Dependency error: {0}")]
    Dependency(String),
    #[error("Cloud error: {0}")]
    Cloud(String),
    #[error("Readiness timed out; verify pairing, Sunshine, host/app and network. If the plug was already on, AC-restoration boot cannot start an off PC. Power was left unchanged.")]
    ReadinessTimeout,
    #[error("Another operation or supervised stream is active; wait for it to finish")]
    Busy,
    #[error("Moonlight error: {0}")]
    Stream(String),
    #[error("Operation cancelled; any power command already sent is not reversed")]
    Cancelled,
    #[error("Power-off approval was declined; no off command was sent")]
    ApprovalDeclined,
}
impl Error {
    pub fn code(&self) -> i32 {
        match self {
            Self::Config(_) | Self::Dependency(_) => 2,
            Self::Cloud(_) => 3,
            Self::ReadinessTimeout => 4,
            Self::Busy => 5,
            Self::Stream(_) => 6,
            Self::Cancelled => 130,
            Self::ApprovalDeclined => 7,
        }
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Operation {
    Start,
    Check,
    PlugOff,
    Refresh,
}
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Phase {
    Idle,
    SwitchingOn,
    Waiting,
    Launching,
    Streaming,
    ConfirmingOff,
    SwitchingOff,
}
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Event {
    Phase(Phase),
    Status(bool),
    Message(String),
}

pub(crate) fn cancelled(cancel: &AtomicBool) -> Result<(), Error> {
    if cancel.load(Ordering::Relaxed) {
        Err(Error::Cancelled)
    } else {
        Ok(())
    }
}

/// All production operations use namespace "operation"; demo uses a distinct name.
pub fn operation_lock(namespace: &str) -> Result<File, Error> {
    let runtime = std::env::var_os("XDG_RUNTIME_DIR")
        .map(PathBuf::from)
        .filter(|p| p.is_absolute())
        .ok_or_else(|| {
            Error::Config(
                "XDG_RUNTIME_DIR is absent or not absolute; run in your logged-in user session"
                    .into(),
            )
        })?;
    lock_at(&runtime, namespace)
}

pub(crate) fn lock_at(runtime: &Path, namespace: &str) -> Result<File, Error> {
    if namespace.is_empty()
        || namespace.len() > 64
        || !namespace
            .bytes()
            .all(|b| b.is_ascii_alphanumeric() || b == b'-' || b == b'_')
    {
        return Err(Error::Config("Invalid operation lock namespace".into()));
    }
    let directory = OpenOptions::new()
        .read(true)
        .custom_flags(libc::O_DIRECTORY | libc::O_NOFOLLOW | libc::O_CLOEXEC)
        .open(runtime)
        .map_err(|_| {
            Error::Config(
                "Cannot open XDG_RUNTIME_DIR securely; use your private session runtime directory"
                    .into(),
            )
        })?;
    let m = directory
        .metadata()
        .map_err(|_| Error::Config("Cannot inspect runtime directory".into()))?;
    if m.uid() != unsafe { libc::geteuid() } || m.mode() & 0o777 != 0o700 {
        return Err(Error::Config(
            "XDG_RUNTIME_DIR must be owned by this user with mode 0700".into(),
        ));
    }
    let name = CString::new(format!("moonboot-{namespace}.lock")).expect("validated namespace");
    let fd = unsafe {
        libc::openat(
            directory.as_raw_fd(),
            name.as_ptr(),
            libc::O_RDWR | libc::O_CREAT | libc::O_NOFOLLOW | libc::O_CLOEXEC | libc::O_NONBLOCK,
            0o600,
        )
    };
    if fd < 0 {
        return Err(Error::Config(
            "Cannot open secure operation lock; inspect the runtime directory".into(),
        ));
    }
    let file = unsafe { File::from_raw_fd(fd) };
    let m = file
        .metadata()
        .map_err(|_| Error::Config("Cannot inspect operation lock".into()))?;
    if !m.is_file()
        || m.uid() != unsafe { libc::geteuid() }
        || m.mode() & 0o777 != 0o600
        || m.nlink() != 1
    {
        return Err(Error::Config(
            "Operation lock must be a private mode-0600 regular file".into(),
        ));
    }
    file.try_lock_exclusive().map_err(|e| {
        if e.kind() == std::io::ErrorKind::WouldBlock {
            Error::Busy
        } else {
            Error::Config(
                "Cannot acquire operation lock; runtime filesystem must support advisory locking"
                    .into(),
            )
        }
    })?;
    // Never unlink: unlinking a locked inode permits a second lock on its replacement.
    Ok(file)
}

pub fn run(
    config: &Config,
    operation: Operation,
    cancel: Arc<AtomicBool>,
    emit: impl Fn(Event),
    approve: impl FnOnce() -> bool,
) -> Result<(), Error> {
    let result = (|| {
        config.validate()?;
        let _lock = operation_lock("operation")?;
        cancelled(&cancel)?;
        let executable = if matches!(operation, Operation::Start | Operation::Check) {
            let executable = process::executable(&config.moonlight.executable)?;
            process::preflight(
                &executable,
                Duration::from_secs(config.startup.probe_timeout_seconds),
                &cancel,
            )?;
            Some(executable)
        } else {
            None
        };
        let mut cloud = Cloud::new(
            &config.tuya,
            Duration::from_secs(config.startup.http_timeout_seconds),
        )?;
        let mut runtime = RealRuntime {
            epoch: Instant::now(),
            executable,
            config,
        };
        workflow(
            config,
            operation,
            &cancel,
            &emit,
            approve,
            &mut cloud,
            &mut runtime,
        )
    })();
    if let Err(error) = &result {
        private_log(error);
        if config.notifications.enabled {
            process::notify("Operation failed or cancelled. Open Moonboot for details; power commands are never automatically reversed.");
        }
    }
    emit(Event::Phase(Phase::Idle));
    result
}

fn private_log(error: &Error) {
    let base = std::env::var_os("XDG_STATE_HOME")
        .filter(|v| !v.is_empty())
        .map(PathBuf::from)
        .or_else(|| std::env::var_os("HOME").map(|p| PathBuf::from(p).join(".local/state")));
    let Some(base) = base.filter(|p| p.is_absolute()) else {
        return;
    };
    // Parent creation uses normal XDG permissions; only our private leaf contains logs.
    if std::fs::create_dir_all(&base).is_err() {
        return;
    }
    let _ = write_private_log(&base.join("moonboot"), error);
}

fn write_private_log(path: &Path, error: &Error) -> std::io::Result<()> {
    match DirBuilder::new().mode(0o700).create(path) {
        Ok(()) => {}
        Err(e) if e.kind() == std::io::ErrorKind::AlreadyExists => {}
        Err(e) => return Err(e),
    }
    let directory = OpenOptions::new()
        .read(true)
        .custom_flags(libc::O_DIRECTORY | libc::O_NOFOLLOW | libc::O_CLOEXEC)
        .open(path)?;
    let m = directory.metadata()?;
    if m.uid() != unsafe { libc::geteuid() } || m.mode() & 0o777 != 0o700 {
        return Ok(());
    }
    let name = c"backend.log";
    let fd = unsafe {
        libc::openat(
            directory.as_raw_fd(),
            name.as_ptr(),
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
    let m = file.metadata()?;
    if !m.is_file()
        || m.uid() != unsafe { libc::geteuid() }
        || m.mode() & 0o777 != 0o600
        || m.nlink() != 1
    {
        return Ok(());
    }
    // A separate log lock also handles concurrent Busy/preflight failures.
    if FileExt::try_lock_exclusive(&file).is_err() {
        return Ok(());
    }
    let timestamp = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap_or_default()
        .as_secs();
    let message = format!("{timestamp} {} {error}\n", error.code());
    if message.len() > 4096 {
        return Ok(());
    }
    if file.metadata()?.len() + message.len() as u64 > 16384 {
        file.set_len(0)?;
    }
    file.write_all(message.as_bytes())
}

pub(crate) trait Runtime {
    fn now(&self) -> Duration;
    fn sleep(&mut self, duration: Duration, cancel: &AtomicBool) -> Result<(), Error>;
    fn probe(&mut self, timeout: Duration, cancel: &AtomicBool) -> Result<process::Output, Error>;
    fn stream(&mut self, cancel: &AtomicBool, emit: &dyn Fn(Event)) -> Result<(), Error>;
    fn notify(&mut self, message: &str);
}

struct RealRuntime<'a> {
    epoch: Instant,
    executable: Option<PathBuf>,
    config: &'a Config,
}
impl Runtime for RealRuntime<'_> {
    fn now(&self) -> Duration {
        self.epoch.elapsed()
    }
    fn sleep(&mut self, duration: Duration, cancel: &AtomicBool) -> Result<(), Error> {
        let deadline = Instant::now() + duration;
        loop {
            cancelled(cancel)?;
            let remaining = deadline.saturating_duration_since(Instant::now());
            if remaining.is_zero() {
                return Ok(());
            }
            thread::sleep(remaining.min(Duration::from_millis(20)));
        }
    }
    fn probe(&mut self, timeout: Duration, cancel: &AtomicBool) -> Result<process::Output, Error> {
        process::probe(
            self.executable.as_ref().expect("Start preflight"),
            &self.config.moonlight.host,
            timeout,
            cancel,
        )
    }
    fn stream(&mut self, cancel: &AtomicBool, emit: &dyn Fn(Event)) -> Result<(), Error> {
        process::stream(
            self.executable.as_ref().expect("Start preflight"),
            self.config,
            cancel,
            || emit(Event::Phase(Phase::Streaming)),
        )
    }
    fn notify(&mut self, message: &str) {
        if self.config.notifications.enabled {
            process::notify(message);
        }
    }
}

pub(crate) fn workflow(
    config: &Config,
    operation: Operation,
    cancel: &AtomicBool,
    emit: &dyn Fn(Event),
    approve: impl FnOnce() -> bool,
    plug: &mut impl Plug,
    runtime: &mut impl Runtime,
) -> Result<(), Error> {
    let http = Duration::from_secs(config.startup.http_timeout_seconds);
    cancelled(cancel)?;
    plug.validate(http, cancel)?;
    cancelled(cancel)?;
    let on = plug.status(http, cancel)?;
    emit(Event::Status(on));
    cancelled(cancel)?;
    match operation {
        Operation::Check | Operation::Refresh => {
            emit(Event::Message("Plug status and supported boolean control verified; plug-on is not proof the PC is running".into()));
            return Ok(());
        }
        Operation::PlugOff => {
            if !on {
                emit(Event::Message(
                    "Plug already reports off; no command sent".into(),
                ));
                return Ok(());
            }
            emit(Event::Phase(Phase::ConfirmingOff));
            cancelled(cancel)?;
            if !approve() {
                return Err(Error::ApprovalDeclined);
            }
            cancelled(cancel)?;
            emit(Event::Phase(Phase::SwitchingOff));
            return change_power(false, config, cancel, emit, plug, runtime);
        }
        Operation::Start => {}
    }
    runtime.notify("Starting session; plug power will never be automatically switched off.");
    if !on {
        emit(Event::Phase(Phase::SwitchingOn));
        change_power(true, config, cancel, emit, plug, runtime)?;
    }
    emit(Event::Phase(Phase::Waiting));
    let deadline = runtime.now() + Duration::from_secs(config.startup.timeout_seconds);
    loop {
        cancelled(cancel)?;
        let remaining = deadline.saturating_sub(runtime.now());
        if remaining.is_zero() {
            return Err(Error::ReadinessTimeout);
        }
        let output = runtime.probe(
            remaining.min(Duration::from_secs(config.startup.probe_timeout_seconds)),
            cancel,
        )?;
        cancelled(cancel)?;
        if runtime.now() >= deadline {
            return Err(Error::ReadinessTimeout);
        }
        if output.success && !output.timed_out {
            if !process::has_app(&output.stdout, &config.moonlight.app)? {
                return Err(Error::Stream("Moonlight returned an app list without the configured app; verify exact app name and client list format".into()));
            }
            break;
        }
        runtime.sleep(
            Duration::from_secs(config.startup.poll_interval_seconds)
                .min(deadline.saturating_sub(runtime.now())),
            cancel,
        )?;
    }
    cancelled(cancel)?;
    emit(Event::Phase(Phase::Launching));
    runtime.notify("Connecting Moonlight; stream exit leaves plug power unchanged.");
    runtime.stream(cancel, emit)
}

fn change_power(
    on: bool,
    config: &Config,
    cancel: &AtomicBool,
    emit: &dyn Fn(Event),
    plug: &mut impl Plug,
    runtime: &mut impl Runtime,
) -> Result<(), Error> {
    cancelled(cancel)?;
    let deadline = runtime.now() + Duration::from_secs(20);
    let http = Duration::from_secs(config.startup.http_timeout_seconds);
    let command = plug.command(on, http.min(deadline.saturating_sub(runtime.now())), cancel);
    cancelled(cancel)?;
    if matches!(command, Err(Error::Cancelled)) {
        return Err(Error::Cancelled);
    }
    // Even an ambiguous command is followed by reads only, never a resend/toggle.
    if command.is_err() {
        emit(Event::Message(
            "Command result was uncertain; checking reported power without resending".into(),
        ));
    }
    loop {
        cancelled(cancel)?;
        let remaining = deadline.saturating_sub(runtime.now());
        if remaining.is_zero() {
            return Err(Error::Cloud("Power state is unconfirmed; inspect the plug app. No command was resent and no toggle was attempted".into()));
        }
        let status = plug.status(http.min(remaining), cancel);
        cancelled(cancel)?;
        match status {
            Ok(actual) => {
                emit(Event::Status(actual));
                if actual == on && runtime.now() < deadline {
                    return Ok(());
                }
            }
            Err(Error::Cloud(message)) => {
                return Err(Error::Cloud(format!(
                    "Power state is unconfirmed; no command was resent. {message}"
                )))
            }
            Err(error) => return Err(error),
        }
        runtime.sleep(
            Duration::from_secs(config.startup.poll_interval_seconds)
                .min(deadline.saturating_sub(runtime.now())),
            cancel,
        )?;
    }
}

#[cfg(test)]
#[path = "backend_tests.rs"]
mod tests;
