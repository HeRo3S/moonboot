use crate::backend::Error;
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use std::{
    env,
    ffi::CString,
    fmt, fs,
    io::{Read, Seek, SeekFrom, Write},
    os::fd::AsRawFd,
    os::unix::fs::{DirBuilderExt, MetadataExt, OpenOptionsExt, PermissionsExt},
    path::{Path, PathBuf},
    sync::atomic::{AtomicU64, Ordering},
};

#[derive(Clone, Debug, Default, Serialize, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct Config {
    pub tuya: Tuya,
    pub moonlight: Moonlight,
    #[serde(skip_serializing_if = "is_default")]
    pub startup: Startup,
    #[serde(skip_serializing_if = "is_default")]
    pub notifications: Notifications,
    #[serde(skip)]
    pub(crate) source: Option<SourceStamp>,
}

#[derive(Clone, PartialEq, Eq)]
pub(crate) struct SourceStamp {
    requested: PathBuf,
    target: PathBuf,
    version: Option<FileVersion>,
}

impl fmt::Debug for SourceStamp {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str("SourceStamp([REDACTED])")
    }
}

#[derive(Clone, PartialEq, Eq)]
struct FileVersion {
    identity: [u64; 10],
    fingerprint: [u8; 32],
}

#[derive(Debug, Default)]
pub struct SaveOutcome {
    /// The write committed; display this warning without claiming the file was unchanged.
    pub warning: Option<String>,
    /// Disable operations and further edits until an explicit reload observes the selected file.
    pub requires_reload: bool,
}

#[derive(Clone, Default, Serialize, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct Tuya {
    pub endpoint: String,
    pub device_id: String,
    pub switch_code: String,
    #[serde(skip_serializing_if = "String::is_empty")]
    pub client_id: String,
    #[serde(skip_serializing_if = "String::is_empty")]
    pub client_secret: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub credentials_file: Option<PathBuf>,
}

impl fmt::Debug for Tuya {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("Tuya")
            .field("endpoint", &self.endpoint)
            .field("device_id", &self.device_id)
            .field("switch_code", &self.switch_code)
            .field("client_id", &"[REDACTED]")
            .field("client_secret", &"[REDACTED]")
            .field("credentials_file", &self.credentials_file)
            .finish()
    }
}

impl Tuya {
    pub(crate) fn validate_credentials(&self) -> Result<(), Error> {
        match (&self.credentials_file, self.client_id.is_empty(), self.client_secret.is_empty()) {
            (Some(path), true, true) if expand_home(path)?.is_absolute() => Ok(()),
            (None, false, false) if [&self.client_id, &self.client_secret].iter()
                .all(|s| s.bytes().all(|b| b.is_ascii_graphic())) => Ok(()),
            _ => Err(Error::Config("Provide either a complete nonempty ASCII client_id/client_secret pair without whitespace or an absolute credentials_file (~/ supported), never both".into())),
        }
    }
}

#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct Moonlight {
    #[serde(skip_serializing_if = "is_default_executable")]
    pub executable: String,
    pub host: String,
    #[serde(skip_serializing_if = "is_default_app")]
    pub app: String,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub stream_args: Vec<String>,
}

fn is_default<T: Default + PartialEq>(value: &T) -> bool {
    value == &T::default()
}

fn is_default_executable(value: &str) -> bool {
    value == "moonlight"
}

fn is_default_app(value: &str) -> bool {
    value == "Desktop"
}

impl Default for Moonlight {
    fn default() -> Self {
        Self {
            executable: "moonlight".into(),
            host: String::new(),
            app: "Desktop".into(),
            stream_args: vec![],
        }
    }
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct Startup {
    pub timeout_seconds: u64,
    pub poll_interval_seconds: u64,
    pub probe_timeout_seconds: u64,
    pub http_timeout_seconds: u64,
}

impl Default for Startup {
    fn default() -> Self {
        Self {
            timeout_seconds: 180,
            poll_interval_seconds: 3,
            probe_timeout_seconds: 10,
            http_timeout_seconds: 10,
        }
    }
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct Notifications {
    pub enabled: bool,
}

impl Default for Notifications {
    fn default() -> Self {
        Self { enabled: true }
    }
}

impl Config {
    pub fn path(path: Option<&Path>) -> Result<PathBuf, Error> {
        Ok(match path {
            Some(path) => expand_home(path)?,
            None => {
                let base = match env::var_os("XDG_CONFIG_HOME").filter(|v| !v.is_empty()) {
                    Some(base) => PathBuf::from(base),
                    None => home()?.join(".config"),
                };
                if !base.is_absolute() {
                    return Err(Error::Config("XDG_CONFIG_HOME must be absolute".into()));
                }
                base.join("moonboot/config.toml")
            }
        })
    }

    /// Reads inline credentials, but never opens external credentials or dependencies.
    pub fn load(path: Option<&Path>) -> Result<Self, Error> {
        let requested = absolute_path(Self::path(path)?)?;
        let target = fs::canonicalize(&requested).map_err(|_| source_error())?;
        let file = open_source(&target)?;
        let (version, bytes) = file_version(&file)?;
        let text = std::str::from_utf8(&bytes).map_err(|_| source_error())?;
        // TOML errors may include source lines, so never expose their Display text.
        let mut config: Self = toml::from_str(text)
            .map_err(|_| Error::Config("Invalid configuration TOML or unknown fields".into()))?;
        if !config.tuya.client_id.is_empty() || !config.tuya.client_secret.is_empty() {
            check_private(&file, &target)?;
        }
        if let Some(path) = &mut config.tuya.credentials_file {
            *path = expand_home(path)?;
        }
        config.validate()?;
        config.source = Some(SourceStamp {
            requested,
            target,
            version: Some(version),
        });
        Ok(config)
    }

    /// Starts setup with an observed baseline, without parsing or opening external credentials.
    pub fn draft(path: Option<&Path>) -> Result<Self, Error> {
        let requested = absolute_path(Self::path(path)?)?;
        Ok(Self {
            source: Some(observe_source(&requested)?),
            ..Self::default()
        })
    }

    /// The caller holds the shared operation lock while saving.
    pub fn save(&mut self, path: Option<&Path>) -> Result<SaveOutcome, Error> {
        self.validate()?;
        let requested = absolute_path(Self::path(path)?)?;
        let failure = || {
            Error::Config(
                "Cannot safely save configuration; target changed or path is unsafe".into(),
            )
        };
        let managed = || {
            Error::Config("Configuration is encrypted, read-only or managed by agenix/Nix; edit the encrypted or declarative source and redeploy".into())
        };
        if in_store(&requested) || requested.extension().is_some_and(|s| s == "age") {
            return Err(managed());
        }
        let observed = observe_source(&requested)?;
        if self.source.as_ref().is_some_and(|s| s != &observed)
            || (self.source.is_none() && observed.version.is_some())
        {
            return Err(conflict());
        }
        let target = match fs::canonicalize(&requested) {
            Ok(target) => target,
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => {
                // A dangling secret symlink must not be replaced by a runtime copy.
                if fs::symlink_metadata(&requested).is_ok() {
                    return Err(managed());
                }
                let parent = requested
                    .parent()
                    .filter(|p| !p.as_os_str().is_empty())
                    .unwrap_or(Path::new("."));
                fs::DirBuilder::new()
                    .recursive(true)
                    .mode(0o700)
                    .create(parent)
                    .map_err(|_| failure())?;
                fs::canonicalize(parent)
                    .map_err(|_| failure())?
                    .join(requested.file_name().ok_or_else(failure)?)
            }
            Err(_) => return Err(failure()),
        };
        if in_store(&target)
            || target.extension().is_some_and(|s| s == "age")
            || target.components().any(|c| {
                c.as_os_str()
                    .to_str()
                    .is_some_and(|s| s == "agenix" || s.starts_with("agenix."))
            })
        {
            return Err(managed());
        }
        let parent = target.parent().ok_or_else(failure)?;
        let directory = fs::OpenOptions::new()
            .read(true)
            .custom_flags(libc::O_DIRECTORY | libc::O_NOFOLLOW | libc::O_CLOEXEC)
            .open(parent)
            .map_err(|_| failure())?;
        let dm = directory.metadata().map_err(|_| failure())?;
        if dm.uid() != unsafe { libc::geteuid() } || dm.mode() & 0o022 != 0 {
            return Err(failure());
        }
        fs2::FileExt::try_lock_exclusive(&directory).map_err(|_| Error::Busy)?;
        if observe_source(&requested)? != observed {
            return Err(conflict());
        }
        // Anchor every operation to the opened directory, not a mutable directory symlink.
        let anchored = PathBuf::from(format!("/proc/self/fd/{}", directory.as_raw_fd()));
        let destination = anchored.join(target.file_name().ok_or_else(failure)?);
        match fs::symlink_metadata(&destination) {
            Ok(m) => {
                if !m.is_file()
                    || m.uid() != unsafe { libc::geteuid() }
                    || m.nlink() != 1
                    || m.mode() & 0o022 != 0
                {
                    return Err(failure());
                }
                if m.mode() & 0o200 == 0 {
                    return Err(managed());
                }
            }
            Err(e) if e.kind() == std::io::ErrorKind::NotFound && observed.version.is_none() => {}
            Err(_) => return Err(failure()),
        }
        let text = toml::to_string_pretty(self)
            .map_err(|_| Error::Config("Cannot serialize configuration".into()))?;
        if text.len() > 65536 {
            return Err(Error::Config("Configuration exceeded 64 KiB".into()));
        }
        static NEXT: AtomicU64 = AtomicU64::new(0);
        let mut temporary = None;
        for _ in 0..100 {
            let name = anchored.join(format!(
                ".moonboot-{}-{}.tmp",
                std::process::id(),
                NEXT.fetch_add(1, Ordering::Relaxed)
            ));
            match fs::OpenOptions::new()
                .read(true)
                .write(true)
                .create_new(true)
                .mode(0o600)
                .custom_flags(libc::O_NOFOLLOW | libc::O_CLOEXEC)
                .open(&name)
            {
                Ok(file) => {
                    temporary = Some((name, file));
                    break;
                }
                Err(e) if e.kind() == std::io::ErrorKind::AlreadyExists => continue,
                Err(_) => return Err(failure()),
            }
        }
        let (name, mut file) = temporary.ok_or_else(failure)?;
        let result = (|| {
            file.set_permissions(fs::Permissions::from_mode(0o600))
                .map_err(|_| failure())?;
            file.write_all(text.as_bytes()).map_err(|_| failure())?;
            file.sync_all().map_err(|_| failure())?;
            let written = file_version(&file)?.0;
            let same_parent =
                fs::metadata(parent).is_ok_and(|m| m.dev() == dm.dev() && m.ino() == dm.ino());
            if observe_source(&requested)? != observed || !same_parent {
                return Err(conflict());
            }
            let from = CString::new(name.as_os_str().as_encoded_bytes()).map_err(|_| failure())?;
            let to =
                CString::new(destination.as_os_str().as_encoded_bytes()).map_err(|_| failure())?;
            #[cfg(test)]
            save_hook(SaveStage::BeforeCommit);
            let flags = if observed.version.is_some() {
                libc::RENAME_EXCHANGE
            } else {
                libc::RENAME_NOREPLACE
            };
            if unsafe {
                libc::renameat2(
                    libc::AT_FDCWD,
                    from.as_ptr(),
                    libc::AT_FDCWD,
                    to.as_ptr(),
                    flags,
                )
            } != 0
            {
                return Err(failure());
            }
            // From here onward the write committed: failures are warnings, not false rollback claims.
            let mut outcome = SaveOutcome::default();
            if let Some(expected) = &observed.version {
                let displaced = open_source(&name).and_then(|f| file_version(&f).map(|v| v.0));
                if displaced
                    .as_ref()
                    .is_ok_and(|v| v.matches_displaced(expected))
                {
                    if fs::remove_file(&name).is_err() {
                        outcome.warning = Some("Configuration committed; the displaced-file backup could not be removed.".into());
                    }
                } else {
                    // The unique temporary name now holds the competing version. Never delete it.
                    // chmod only an opened regular inode, never a potentially displaced symlink.
                    if let Ok(backup) = fs::OpenOptions::new()
                        .read(true)
                        .custom_flags(libc::O_NOFOLLOW | libc::O_NONBLOCK)
                        .open(&name)
                    {
                        if backup.metadata().is_ok_and(|m| {
                            m.is_file() && m.nlink() == 1 && m.uid() == unsafe { libc::geteuid() }
                        }) {
                            let _ = backup.set_permissions(fs::Permissions::from_mode(0o600));
                            let _ = backup.sync_all();
                        }
                    }
                    outcome.warning = Some("Configuration committed, but a concurrent version was displaced and retained in a unique .moonboot-*.tmp backup beside the target. Reload and review the backup before further edits.".into());
                    outcome.requires_reload = true;
                }
            }
            #[cfg(test)]
            save_hook(SaveStage::AfterCommit);
            if sync_directory(&directory).is_err() {
                let warning = "Configuration committed, but directory sync failed; durability after a crash is uncertain.";
                outcome.warning = Some(match outcome.warning {
                    Some(previous) => format!("{previous} {warning}"),
                    None => warning.into(),
                });
            }
            let committed = file_version(&file).map(|v| v.0);
            let selected = observe_source(&requested);
            if let (Ok(version), Ok(selected)) = (committed, selected) {
                if version.matches_displaced(&written)
                    && selected.target == target
                    && selected.version.as_ref() == Some(&version)
                {
                    self.source = Some(selected);
                } else {
                    outcome.requires_reload = true;
                }
            } else {
                outcome.requires_reload = true;
            }
            if outcome.requires_reload {
                let warning = "The save committed, but the selected configuration changed or could not be verified. Reload before running operations or saving again.";
                outcome.warning = Some(match outcome.warning {
                    Some(previous) => format!("{previous} {warning}"),
                    None => warning.into(),
                });
                // An unverifiable committed save must not authorize another overwrite.
                self.source = Some(observed.clone());
            }
            Ok(outcome)
        })();
        if result.is_err() {
            let _ = fs::remove_file(&name);
        }
        result
    }

    pub fn validate(&self) -> Result<(), Error> {
        let url = reqwest::Url::parse(&self.tuya.endpoint)
            .map_err(|_| Error::Config("Tuya endpoint must be an HTTPS origin".into()))?;
        if url.scheme() != "https"
            || url.host_str().is_none()
            || !url.username().is_empty()
            || url.password().is_some()
            || url.query().is_some()
            || url.fragment().is_some()
            || url.path() != "/"
        {
            return Err(Error::Config("Tuya endpoint must be an HTTPS origin without credentials, path, query, or fragment".into()));
        }
        for value in [&self.tuya.device_id, &self.tuya.switch_code] {
            if value.is_empty()
                || !value
                    .bytes()
                    .all(|b| b.is_ascii_alphanumeric() || b == b'_' || b == b'-')
            {
                return Err(Error::Config("Device ID and switch code must contain only letters, digits, underscore or hyphen".into()));
            }
        }
        self.tuya.validate_credentials()?;
        for value in [
            &self.moonlight.executable,
            &self.moonlight.host,
            &self.moonlight.app,
        ] {
            if value.is_empty() || value.chars().any(char::is_control) || value.starts_with('-') {
                return Err(Error::Config("Moonlight executable, host, and app must be nonempty, non-option values without control characters".into()));
            }
        }
        if self.moonlight.stream_args.iter().any(|s| s.contains('\0')) {
            return Err(Error::Config(
                "Moonlight arguments cannot contain NUL".into(),
            ));
        }
        for value in [
            self.startup.timeout_seconds,
            self.startup.poll_interval_seconds,
            self.startup.probe_timeout_seconds,
            self.startup.http_timeout_seconds,
        ] {
            if value == 0 || value > 86400 {
                return Err(Error::Config(
                    "Timeout and poll values must be between 1 and 86400 seconds".into(),
                ));
            }
        }
        Ok(())
    }
}

fn home() -> Result<PathBuf, Error> {
    env::var_os("HOME")
        .map(PathBuf::from)
        .filter(|p| p.is_absolute())
        .ok_or_else(|| Error::Config("HOME must be an absolute path".into()))
}

fn conflict() -> Error {
    Error::Config("Configuration changed since this draft was opened. Reload Settings before saving; no save was applied.".into())
}

fn source_error() -> Error {
    Error::Config(
        "Cannot safely observe configuration; use a readable regular file smaller than 64 KiB"
            .into(),
    )
}

fn absolute_path(path: PathBuf) -> Result<PathBuf, Error> {
    if path.is_absolute() {
        Ok(path)
    } else {
        Ok(env::current_dir().map_err(|_| source_error())?.join(path))
    }
}

// Resolve existing directory symlinks even when the destination or its parents are absent.
fn resolve_target(path: &Path) -> Result<PathBuf, Error> {
    match fs::canonicalize(path) {
        Ok(target) => Ok(target),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => {
            if fs::symlink_metadata(path).is_ok() {
                return Err(source_error());
            }
            let parent = path.parent().ok_or_else(source_error)?;
            Ok(resolve_target(parent)?.join(path.file_name().ok_or_else(source_error)?))
        }
        Err(_) => Err(source_error()),
    }
}

fn open_source(path: &Path) -> Result<fs::File, Error> {
    fs::OpenOptions::new()
        .read(true)
        .custom_flags(libc::O_NOFOLLOW | libc::O_NONBLOCK | libc::O_CLOEXEC)
        .open(path)
        .map_err(|_| source_error())
}

fn metadata_identity(m: &fs::Metadata) -> [u64; 10] {
    [
        m.dev(),
        m.ino(),
        m.uid() as u64,
        m.mode() as u64,
        m.nlink(),
        m.len(),
        m.mtime() as u64,
        m.mtime_nsec() as u64,
        m.ctime() as u64,
        m.ctime_nsec() as u64,
    ]
}

fn file_version(mut file: &fs::File) -> Result<(FileVersion, Vec<u8>), Error> {
    let before = file.metadata().map_err(|_| source_error())?;
    if !before.is_file() || before.len() > 65536 {
        return Err(source_error());
    }
    file.seek(SeekFrom::Start(0)).map_err(|_| source_error())?;
    let mut bytes = Vec::new();
    file.take(65537)
        .read_to_end(&mut bytes)
        .map_err(|_| source_error())?;
    let identity = metadata_identity(&file.metadata().map_err(|_| source_error())?);
    if bytes.len() > 65536 || identity != metadata_identity(&before) {
        return Err(source_error());
    }
    Ok((
        FileVersion {
            identity,
            fingerprint: Sha256::digest(&bytes).into(),
        },
        bytes,
    ))
}

impl FileVersion {
    fn matches_displaced(&self, expected: &Self) -> bool {
        // renameat2 changes ctime; all other identity/content fields must still match.
        self.identity[..8] == expected.identity[..8] && self.fingerprint == expected.fingerprint
    }
}

fn observe_source(requested: &Path) -> Result<SourceStamp, Error> {
    let target = resolve_target(requested)?;
    let version = match fs::symlink_metadata(&target) {
        Ok(_) => {
            let file = open_source(&target)?;
            let actual = fs::read_link(format!("/proc/self/fd/{}", file.as_raw_fd()))
                .map_err(|_| source_error())?;
            if actual != target {
                return Err(source_error());
            }
            Some(file_version(&file)?.0)
        }
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => None,
        Err(_) => return Err(source_error()),
    };
    Ok(SourceStamp {
        requested: requested.into(),
        target,
        version,
    })
}

fn sync_directory(directory: &fs::File) -> std::io::Result<()> {
    #[cfg(test)]
    if FAIL_DIRECTORY_SYNC.with(|fail| fail.replace(false)) {
        return Err(std::io::Error::other("synthetic directory sync failure"));
    }
    directory.sync_all()
}

#[cfg(test)]
#[derive(Clone, Copy, PartialEq, Eq)]
enum SaveStage {
    BeforeCommit,
    AfterCommit,
}

#[cfg(test)]
type SaveHook = Box<dyn FnOnce()>;
#[cfg(test)]
thread_local! {
    static SAVE_HOOK: std::cell::RefCell<Option<(SaveStage, SaveHook)>> = const { std::cell::RefCell::new(None) };
    static FAIL_DIRECTORY_SYNC: std::cell::Cell<bool> = const { std::cell::Cell::new(false) };
}

#[cfg(test)]
fn save_hook(stage: SaveStage) {
    let hook = SAVE_HOOK.with(|slot| {
        let mut slot = slot.borrow_mut();
        if slot
            .as_ref()
            .is_some_and(|(expected, _)| *expected == stage)
        {
            slot.take()
        } else {
            None
        }
    });
    if let Some((_, hook)) = hook {
        hook();
    }
}

pub(crate) fn expand_home(path: &Path) -> Result<PathBuf, Error> {
    if path == Path::new("~") {
        return home();
    }
    if let Ok(rest) = path.strip_prefix("~/") {
        return Ok(home()?.join(rest));
    }
    Ok(path.to_owned())
}

fn in_store(path: &Path) -> bool {
    path.starts_with("/nix/store")
}

fn check_private(file: &fs::File, target: &Path) -> Result<(), Error> {
    let m = file
        .metadata()
        .map_err(|_| Error::Config("Cannot inspect private file".into()))?;
    let actual = fs::read_link(format!("/proc/self/fd/{}", file.as_raw_fd()))
        .map_err(|_| Error::Config("Cannot resolve opened private file".into()))?;
    if in_store(target)
        || in_store(&actual)
        || actual != target
        || !m.is_file()
        || m.uid() != unsafe { libc::geteuid() }
        || !matches!(m.mode() & 0o7777, 0o400 | 0o600)
        || m.nlink() != 1
    {
        return Err(Error::Config("Secret-bearing files must be regular, owned by this user, mode 0400 or 0600, not hard-linked, and outside the Nix store".into()));
    }
    Ok(())
}

pub(crate) fn read_private_file(path: &Path, limit: u64) -> Result<String, Error> {
    let target = fs::canonicalize(expand_home(path)?)
        .map_err(|_| Error::Config("Cannot resolve private file".into()))?;
    let file = fs::OpenOptions::new()
        .read(true)
        .custom_flags(libc::O_NOFOLLOW | libc::O_NONBLOCK | libc::O_CLOEXEC)
        .open(&target)
        .map_err(|_| Error::Config("Cannot open private file".into()))?;
    check_private(&file, &target)?;
    let mut text = String::new();
    file.take(limit + 1)
        .read_to_string(&mut text)
        .map_err(|_| Error::Config("Cannot read private file".into()))?;
    if text.len() as u64 > limit {
        return Err(Error::Config(
            "Private file exceeded safe size limit".into(),
        ));
    }
    Ok(text)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::os::unix::fs::{symlink, PermissionsExt};
    const TEXT: &str = "[tuya]\nendpoint='https://example.invalid'\ndevice_id='fake-device'\nswitch_code='switch_1'\ncredentials_file='/nonexistent-test-secret'\n[moonlight]\nexecutable='fake-moonlight'\nhost='fake-host'\napp='Desktop'\nstream_args=[]\n[startup]\ntimeout_seconds=180\npoll_interval_seconds=3\nprobe_timeout_seconds=10\nhttp_timeout_seconds=10\n[notifications]\nenabled=false\n";

    #[test]
    fn load_is_inert_and_validates_without_credentials_or_executable() {
        let directory = tempfile::tempdir().unwrap();
        let path = directory.path().join("config.toml");
        fs::write(&path, TEXT).unwrap();
        let config = Config::load(Some(&path)).unwrap();
        assert_eq!(config.moonlight.app, "Desktop");
        assert!(!config.notifications.enabled);
        assert!(Config::load(Some(&directory.path().join("missing"))).is_err());
    }

    #[test]
    fn endpoints_timing_path_and_argument_injection_validation() {
        let config: Config = toml::from_str(TEXT).unwrap();
        for endpoint in [
            "http://example.invalid",
            "https://user:secret@example.invalid",
            "https://example.invalid/path",
            "https://example.invalid?x=1",
            "https://example.invalid#frag",
        ] {
            let mut invalid = config.clone();
            invalid.tuya.endpoint = endpoint.into();
            assert!(invalid.validate().is_err());
        }
        let mut invalid = config.clone();
        invalid.startup.timeout_seconds = 0;
        assert!(invalid.validate().is_err());
        invalid.startup.timeout_seconds = u64::MAX;
        assert!(invalid.validate().is_err());
        let mut invalid = config.clone();
        invalid.tuya.device_id = "../../another-device".into();
        assert!(invalid.validate().is_err());
        let mut invalid = config.clone();
        invalid.moonlight.host = "--help".into();
        assert!(invalid.validate().is_err());
        let mut invalid = config.clone();
        invalid.moonlight.app = "Desktop\nOther App".into();
        assert!(invalid.validate().is_err());
        let mut valid = config;
        valid.moonlight.stream_args = vec!["literal; $(shell)".into()];
        assert!(valid.validate().is_ok());
    }

    #[test]
    fn config_parser_errors_do_not_echo_source() {
        let directory = tempfile::tempdir().unwrap();
        let path = directory.path().join("config.toml");
        fs::write(&path, "SECRET SOURCE = invalid").unwrap();
        assert!(!Config::load(Some(&path))
            .unwrap_err()
            .to_string()
            .contains("SECRET SOURCE"));
    }

    fn inline() -> Config {
        let mut config: Config = toml::from_str(TEXT).unwrap();
        config.tuya.credentials_file = None;
        config.tuya.client_id = "synthetic-private-id".into();
        config.tuya.client_secret = "synthetic-private-secret".into();
        config
    }

    #[test]
    fn defaults_roundtrip_and_required_values() {
        let text = "[tuya]\nendpoint='https://example.invalid'\ndevice_id='fake'\nswitch_code='switch_1'\nclient_id='synthetic-id'\nclient_secret='synthetic-secret'\n[moonlight]\nhost='fake-host'\n";
        let config: Config = toml::from_str(text).unwrap();
        config.validate().unwrap();
        assert_eq!(config.moonlight.executable, "moonlight");
        assert_eq!(config.moonlight.app, "Desktop");
        assert!(config.moonlight.stream_args.is_empty());
        assert_eq!(config.startup.timeout_seconds, 180);
        assert_eq!(config.startup.poll_interval_seconds, 3);
        assert_eq!(config.startup.probe_timeout_seconds, 10);
        assert_eq!(config.startup.http_timeout_seconds, 10);
        assert!(config.notifications.enabled);
        assert!(config.tuya.credentials_file.is_none());
        let serialized = toml::to_string(&config).unwrap();
        assert!(!serialized.contains("credentials_file"));
        for default_field in [
            "startup",
            "notifications",
            "executable",
            "app =",
            "stream_args",
        ] {
            assert!(!serialized.contains(default_field));
        }
        let again: Config = toml::from_str(&serialized).unwrap();
        assert_eq!(again.tuya.client_secret, config.tuya.client_secret);
        assert!(Config::default().validate().is_err());
        assert!(Tuya::default().client_id.is_empty());
        let mut missing = config;
        missing.moonlight.host.clear();
        assert!(missing.validate().is_err());
    }

    #[test]
    fn credential_selection_and_diagnostics_are_redacted() {
        let config = inline();
        let debug = format!("{config:?}");
        for secret in [&config.tuya.client_id, &config.tuya.client_secret] {
            assert!(!debug.contains(secret));
        }
        for variant in 0..4 {
            let mut invalid = config.clone();
            match variant {
                0 => invalid.tuya.client_id.clear(),
                1 => invalid.tuya.client_secret.clear(),
                2 => invalid.tuya.credentials_file = Some("/synthetic-secret-file".into()),
                _ => invalid.tuya.client_secret.push('\n'),
            }
            let error = invalid.validate().unwrap_err();
            assert!(!error.to_string().contains("synthetic-private"));
            assert!(!format!("{error:?}").contains("synthetic-private"));
        }
        let legacy: Config = toml::from_str(TEXT).unwrap();
        legacy.validate().unwrap();
        assert!(legacy.tuya.client_id.is_empty());
        let serialized = toml::to_string(&legacy).unwrap();
        let again: Config = toml::from_str(&serialized).unwrap();
        assert_eq!(again.tuya.credentials_file, legacy.tuya.credentials_file);
    }

    #[test]
    fn inline_config_requires_private_target_even_through_symlinks() {
        let dir = tempfile::tempdir().unwrap();
        let target = dir.path().join("target");
        fs::write(&target, toml::to_string(&inline()).unwrap()).unwrap();
        symlink("target", dir.path().join("first")).unwrap();
        symlink("first", dir.path().join("second")).unwrap();
        for mode in [0o400, 0o600] {
            fs::set_permissions(&target, fs::Permissions::from_mode(mode)).unwrap();
            assert!(Config::load(Some(&dir.path().join("second"))).is_ok());
            assert!(read_private_file(&dir.path().join("second"), 65536).is_ok());
        }
        for mode in [0o644, 0o640, 0o660, 0o700, 0o4600] {
            match fs::set_permissions(&target, fs::Permissions::from_mode(mode)) {
                Ok(()) => {}
                Err(error) if mode == 0o4600 && error.raw_os_error() == Some(libc::EPERM) => {
                    // Nix's build sandbox can prohibit creating setuid-mode fixtures.
                    continue;
                }
                Err(error) => panic!("Cannot set fixture permissions: {error}"),
            }
            assert!(Config::load(Some(&target)).is_err());
            assert!(read_private_file(&target, 65536).is_err());
        }
        fs::set_permissions(&target, fs::Permissions::from_mode(0o600)).unwrap();
        fs::hard_link(&target, dir.path().join("hard")).unwrap();
        assert!(Config::load(Some(&target)).is_err());
        assert!(read_private_file(&target, 65536).is_err());
        assert!(inline().save(Some(&target)).is_err());
        assert!(in_store(Path::new("/nix/store/synthetic/config")));
        assert!(!in_store(Path::new("/nix/storehouse/config")));
    }

    #[test]
    fn atomic_save_preserves_file_and_directory_symlink_chains() {
        if crate::process::isolated_test(
            "config::tests::atomic_save_preserves_file_and_directory_symlink_chains",
        ) {
            return;
        }
        let dir = tempfile::tempdir().unwrap();
        let real = dir.path().join("real");
        fs::create_dir(&real).unwrap();
        symlink("real", dir.path().join("directory-link")).unwrap();
        let target = real.join("config.toml");
        fs::write(&target, TEXT).unwrap();
        fs::set_permissions(&target, fs::Permissions::from_mode(0o644)).unwrap();
        symlink("real/config.toml", dir.path().join("first")).unwrap();
        symlink("first", dir.path().join("second")).unwrap();
        let selected = dir.path().join("second");
        let mut config = Config::load(Some(&selected)).unwrap();
        config.tuya = inline().tuya;
        config.save(Some(&selected)).unwrap();
        assert_eq!(
            fs::read_link(dir.path().join("second")).unwrap(),
            Path::new("first")
        );
        assert_eq!(
            fs::read_link(dir.path().join("first")).unwrap(),
            Path::new("real/config.toml")
        );
        assert_eq!(fs::metadata(&target).unwrap().mode() & 0o777, 0o600);
        assert_eq!(
            Config::load(Some(&target)).unwrap().tuya.client_id,
            config.tuya.client_id
        );
        let mut config =
            Config::load(Some(&dir.path().join("directory-link/config.toml"))).unwrap();
        config
            .save(Some(&dir.path().join("directory-link/config.toml")))
            .unwrap();
        assert!(fs::symlink_metadata(dir.path().join("directory-link"))
            .unwrap()
            .file_type()
            .is_symlink());
        assert_eq!(fs::read_dir(&real).unwrap().count(), 1);
    }

    #[test]
    fn save_creates_private_files_and_refuses_managed_or_unsafe_targets() {
        if crate::process::isolated_test(
            "config::tests::save_creates_private_files_and_refuses_managed_or_unsafe_targets",
        ) {
            return;
        }
        let dir = tempfile::tempdir().unwrap();
        let mut config = inline();
        let new = dir.path().join("new/nested/config.toml");
        config.save(Some(&new)).unwrap();
        assert_eq!(fs::metadata(&new).unwrap().mode() & 0o777, 0o600);
        assert_eq!(
            fs::metadata(new.parent().unwrap()).unwrap().mode() & 0o777,
            0o700
        );
        fs::set_permissions(&new, fs::Permissions::from_mode(0o400)).unwrap();
        let before = fs::read(&new).unwrap();
        symlink(&new, dir.path().join("readonly-link")).unwrap();
        let mut config = Config::load(Some(&dir.path().join("readonly-link"))).unwrap();
        let error = config
            .save(Some(&dir.path().join("readonly-link")))
            .unwrap_err();
        assert!(error.to_string().contains("redeploy"));
        assert_eq!(fs::read(&new).unwrap(), before);
        fs::set_permissions(&new, fs::Permissions::from_mode(0o600)).unwrap();
        fs::write(&new, TEXT).unwrap();
        fs::set_permissions(&new, fs::Permissions::from_mode(0o400)).unwrap();
        assert!(Config::load(Some(&new)).is_ok());
        assert!(config.save(Some(&new)).is_err());
        assert!(config
            .save(Some(Path::new("/nix/store/moonboot-synthetic-config")))
            .is_err());
        let managed = dir.path().join("agenix");
        fs::create_dir(&managed).unwrap();
        assert!(config.save(Some(&managed.join("config"))).is_err());
        symlink("nonexistent", dir.path().join("dangling")).unwrap();
        assert!(config.save(Some(&dir.path().join("dangling"))).is_err());
        assert!(fs::symlink_metadata(dir.path().join("dangling"))
            .unwrap()
            .file_type()
            .is_symlink());
        assert!(Config::default()
            .save(Some(&dir.path().join("invalid")))
            .is_err());
        assert!(!dir.path().join("invalid").exists());
    }

    #[test]
    fn nonregular_and_oversized_files_fail_without_blocking() {
        if crate::process::isolated_test(
            "config::tests::nonregular_and_oversized_files_fail_without_blocking",
        ) {
            return;
        }
        use std::{ffi::CString, os::unix::ffi::OsStrExt};
        let dir = tempfile::tempdir().unwrap();
        let fifo = dir.path().join("fifo");
        let name = CString::new(fifo.as_os_str().as_bytes()).unwrap();
        assert_eq!(unsafe { libc::mkfifo(name.as_ptr(), 0o600) }, 0);
        assert!(read_private_file(&fifo, 16384).is_err());
        assert!(Config::load(Some(&fifo)).is_err());
        assert!(inline().save(Some(&fifo)).is_err());
        assert!(read_private_file(dir.path(), 16384).is_err());
        let big = dir.path().join("oversized");
        fs::write(&big, vec![b'x'; 65537]).unwrap();
        fs::set_permissions(&big, fs::Permissions::from_mode(0o600)).unwrap();
        assert!(Config::load(Some(&big)).is_err());
        assert!(read_private_file(&big, 16384).is_err());
    }

    #[test]
    fn save_refuses_publicly_writable_parent_and_directory_lock_contention() {
        if crate::process::isolated_test(
            "config::tests::save_refuses_publicly_writable_parent_and_directory_lock_contention",
        ) {
            return;
        }
        let dir = tempfile::tempdir().unwrap();
        let parent = dir.path().join("parent");
        fs::create_dir(&parent).unwrap();
        fs::set_permissions(&parent, fs::Permissions::from_mode(0o777)).unwrap();
        let path = parent.join("config");
        assert!(inline().save(Some(&path)).is_err());
        assert!(!path.exists());
        fs::set_permissions(&parent, fs::Permissions::from_mode(0o700)).unwrap();
        let directory = fs::File::open(&parent).unwrap();
        fs2::FileExt::try_lock_exclusive(&directory).unwrap();
        assert!(matches!(inline().save(Some(&path)), Err(Error::Busy)));
        drop(directory);
        inline().save(Some(&path)).unwrap();
    }

    #[test]
    fn config_path_expansion_in_isolated_process() {
        if crate::process::isolated_test("config::tests::config_path_expansion_in_isolated_process")
        {
            return;
        }
        env::set_var("HOME", "/synthetic-home");
        env::remove_var("XDG_CONFIG_HOME");
        assert_eq!(
            Config::path(None).unwrap(),
            Path::new("/synthetic-home/.config/moonboot/config.toml")
        );
        assert_eq!(
            Config::path(Some(Path::new("~/settings/config.toml"))).unwrap(),
            Path::new("/synthetic-home/settings/config.toml")
        );
        env::set_var("XDG_CONFIG_HOME", "/synthetic-xdg");
        assert_eq!(
            Config::path(None).unwrap(),
            Path::new("/synthetic-xdg/moonboot/config.toml")
        );
        env::set_var("XDG_CONFIG_HOME", "relative-path");
        assert!(Config::path(None).is_err());
        env::set_var("HOME", "relative-home");
        assert!(Config::path(Some(Path::new("~/config"))).is_err());
    }

    fn create_config(path: &Path) -> Config {
        let mut config = inline();
        let outcome = config.save(Some(path)).unwrap();
        assert!(outcome.warning.is_none());
        assert!(!outcome.requires_reload);
        config
    }

    #[test]
    fn loaded_baseline_rejects_stale_content_and_retargeted_symlink() {
        if crate::process::isolated_test(
            "config::tests::loaded_baseline_rejects_stale_content_and_retargeted_symlink",
        ) {
            return;
        }
        let dir = tempfile::tempdir().unwrap();
        let first = dir.path().join("first");
        let second = dir.path().join("second");
        create_config(&first);
        create_config(&second);
        let link = dir.path().join("selected");
        symlink("first", &link).unwrap();
        let mut stale = Config::load(Some(&link)).unwrap();
        let mut writer = Config::load(Some(&first)).unwrap();
        writer.tuya.client_secret = "changed-synthetic-secret".into();
        writer.save(Some(&first)).unwrap();
        let competing = fs::read(&first).unwrap();
        let error = stale.save(Some(&link)).unwrap_err();
        assert!(error.to_string().contains("Reload"));
        assert!(!error.to_string().contains("synthetic-secret"));
        assert_eq!(fs::read(&first).unwrap(), competing);
        let mut stale = Config::load(Some(&link)).unwrap();
        fs::remove_file(&link).unwrap();
        symlink("second", &link).unwrap();
        let second_before = fs::read(&second).unwrap();
        assert!(stale.save(Some(&link)).is_err());
        assert_eq!(fs::read(&second).unwrap(), second_before);
        assert_eq!(fs::read(&first).unwrap(), competing);
    }

    #[test]
    fn drafts_observe_absence_or_invalid_contents_and_unknown_sources_cannot_overwrite() {
        if crate::process::isolated_test("config::tests::drafts_observe_absence_or_invalid_contents_and_unknown_sources_cannot_overwrite") { return; }
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("missing/initial-config");
        let mut draft = Config::draft(Some(&path)).unwrap();
        assert!(draft.source.as_ref().unwrap().version.is_none());
        draft.tuya = inline().tuya;
        draft.moonlight = inline().moonlight;
        create_config(&path);
        let before = fs::read(&path).unwrap();
        assert!(draft.save(Some(&path)).is_err());
        assert_eq!(fs::read(&path).unwrap(), before);
        assert!(inline().save(Some(&path)).is_err());
        fs::write(&path, b"invalid-synthetic-secret = [\xff").unwrap();
        let mut draft = Config::draft(Some(&path)).unwrap();
        assert!(draft.tuya.client_secret.is_empty());
        draft.tuya = inline().tuya;
        draft.moonlight = inline().moonlight;
        let observed = draft.source.clone();
        fs::write(&path, b"different-synthetic-secret = invalid").unwrap();
        let another = Config::draft(Some(&path)).unwrap();
        assert!(observed != another.source);
        assert!(!format!("{draft:?}").contains("invalid-synthetic-secret"));
        assert!(!format!("{another:?}").contains("different-synthetic-secret"));
        let before = fs::read(&path).unwrap();
        assert!(draft.save(Some(&path)).is_err());
        assert_eq!(fs::read(&path).unwrap(), before);
        let mut fresh = Config::draft(Some(&path)).unwrap();
        fresh.tuya = inline().tuya;
        fresh.moonlight = inline().moonlight;
        fresh.save(Some(&path)).unwrap();
        assert!(Config::load(Some(&path)).is_ok());
    }

    #[test]
    fn refreshed_baseline_allows_second_save_and_is_not_serialized() {
        if crate::process::isolated_test(
            "config::tests::refreshed_baseline_allows_second_save_and_is_not_serialized",
        ) {
            return;
        }
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("config");
        let mut config = create_config(&path);
        let first = config.source.clone();
        config.moonlight.host = "second-host".into();
        config.save(Some(&path)).unwrap();
        assert!(first != config.source);
        config.moonlight.host = "third-host".into();
        config.save(Some(&path)).unwrap();
        assert_eq!(
            Config::load(Some(&path)).unwrap().moonlight.host,
            "third-host"
        );
        let text = toml::to_string(&config).unwrap();
        assert!(!text.contains("source"));
        assert!(!text.contains("fingerprint"));
        let decoded: Config = toml::from_str(&text).unwrap();
        assert!(decoded.source.is_none());
        assert!(!format!("{config:?}").contains("synthetic-private-secret"));
    }

    #[test]
    fn directory_sync_failure_reports_committed_create_and_update() {
        if crate::process::isolated_test(
            "config::tests::directory_sync_failure_reports_committed_create_and_update",
        ) {
            return;
        }
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("config");
        let mut config = inline();
        for host in ["created-host", "updated-host"] {
            config.moonlight.host = host.into();
            FAIL_DIRECTORY_SYNC.with(|fail| fail.set(true));
            let outcome = config.save(Some(&path)).unwrap();
            assert!(outcome.warning.unwrap().contains("durability"));
            assert!(!outcome.requires_reload);
            assert_eq!(Config::load(Some(&path)).unwrap().moonlight.host, host);
        }
        assert_eq!(fs::read_dir(dir.path()).unwrap().count(), 1);
    }

    #[test]
    fn writer_between_precheck_and_exchange_is_preserved_in_private_backup() {
        if crate::process::isolated_test(
            "config::tests::writer_between_precheck_and_exchange_is_preserved_in_private_backup",
        ) {
            return;
        }
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("config");
        let mut draft = create_config(&path);
        let competing = dir.path().join("competing");
        let mut other = create_config(&competing);
        other.moonlight.host = "competing-host".into();
        other.save(Some(&competing)).unwrap();
        let competing_bytes = fs::read(&competing).unwrap();
        let destination = path.clone();
        SAVE_HOOK.with(|hook| {
            *hook.borrow_mut() = Some((
                SaveStage::BeforeCommit,
                Box::new(move || {
                    fs::rename(&competing, &destination).unwrap();
                }),
            ))
        });
        draft.moonlight.host = "draft-host".into();
        let outcome = draft.save(Some(&path)).unwrap();
        assert!(outcome.requires_reload);
        assert!(outcome.warning.unwrap().contains("backup"));
        assert_eq!(
            Config::load(Some(&path)).unwrap().moonlight.host,
            "draft-host"
        );
        let backup = fs::read_dir(dir.path())
            .unwrap()
            .map(|entry| entry.unwrap().path())
            .find(|p| {
                p.file_name()
                    .unwrap()
                    .to_string_lossy()
                    .starts_with(".moonboot-")
            })
            .unwrap();
        assert_eq!(fs::read(&backup).unwrap(), competing_bytes);
        assert_eq!(fs::metadata(&backup).unwrap().mode() & 0o777, 0o600);
        assert!(draft.save(Some(&path)).is_err());
    }

    #[test]
    fn symlink_retarget_at_commit_requires_reload_without_writing_new_target() {
        if crate::process::isolated_test(
            "config::tests::symlink_retarget_at_commit_requires_reload_without_writing_new_target",
        ) {
            return;
        }
        let dir = tempfile::tempdir().unwrap();
        let first = dir.path().join("first");
        let second = dir.path().join("second");
        create_config(&first);
        create_config(&second);
        let link = dir.path().join("selected");
        symlink("first", &link).unwrap();
        let mut draft = Config::load(Some(&link)).unwrap();
        let before = fs::read(&second).unwrap();
        let selected = link.clone();
        SAVE_HOOK.with(|hook| {
            *hook.borrow_mut() = Some((
                SaveStage::BeforeCommit,
                Box::new(move || {
                    fs::remove_file(&selected).unwrap();
                    symlink("second", &selected).unwrap();
                }),
            ))
        });
        draft.moonlight.host = "committed-original-host".into();
        let outcome = draft.save(Some(&link)).unwrap();
        assert!(outcome.requires_reload);
        assert!(outcome.warning.unwrap().contains("Reload"));
        assert_eq!(
            Config::load(Some(&first)).unwrap().moonlight.host,
            "committed-original-host"
        );
        assert_eq!(fs::read(&second).unwrap(), before);
        assert_eq!(fs::read_link(&link).unwrap(), Path::new("second"));
    }

    #[test]
    fn postcommit_destination_replacement_requires_reload_and_preserves_writer() {
        if crate::process::isolated_test("config::tests::postcommit_destination_replacement_requires_reload_and_preserves_writer") { return; }
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("config");
        let mut draft = create_config(&path);
        let competing = dir.path().join("competing");
        let mut writer = create_config(&competing);
        writer.moonlight.host = "postcommit-writer".into();
        writer.save(Some(&competing)).unwrap();
        let before = fs::read(&competing).unwrap();
        let destination = path.clone();
        SAVE_HOOK.with(|hook| {
            *hook.borrow_mut() = Some((
                SaveStage::AfterCommit,
                Box::new(move || {
                    fs::rename(&competing, &destination).unwrap();
                }),
            ))
        });
        let outcome = draft.save(Some(&path)).unwrap();
        assert!(outcome.requires_reload);
        assert!(outcome.warning.unwrap().contains("committed"));
        assert_eq!(fs::read(&path).unwrap(), before);
    }

    #[test]
    fn encrypted_age_targets_are_never_created_or_overwritten() {
        if crate::process::isolated_test(
            "config::tests::encrypted_age_targets_are_never_created_or_overwritten",
        ) {
            return;
        }
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("secret.age");
        let mut config = inline();
        assert!(config
            .save(Some(&path))
            .unwrap_err()
            .to_string()
            .contains("encrypted"));
        assert!(!path.exists());
        let ciphertext = b"age-encryption.org/v1\nsynthetic-ciphertext";
        fs::write(&path, ciphertext).unwrap();
        fs::set_permissions(&path, fs::Permissions::from_mode(0o600)).unwrap();
        let mut draft = Config::draft(Some(&path)).unwrap();
        draft.tuya = inline().tuya;
        draft.moonlight = inline().moonlight;
        assert!(draft.save(Some(&path)).is_err());
        let link = dir.path().join("config.toml");
        symlink("secret.age", &link).unwrap();
        let mut draft = Config::draft(Some(&link)).unwrap();
        draft.tuya = inline().tuya;
        draft.moonlight = inline().moonlight;
        assert!(draft
            .save(Some(&link))
            .unwrap_err()
            .to_string()
            .contains("encrypted"));
        assert_eq!(fs::read(&path).unwrap(), ciphertext);
        assert_eq!(fs::read_link(&link).unwrap(), Path::new("secret.age"));
    }

    #[test]
    fn creator_after_prechecks_is_not_overwritten() {
        if crate::process::isolated_test(
            "config::tests::creator_after_prechecks_is_not_overwritten",
        ) {
            return;
        }
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("config");
        let mut draft = inline();
        let destination = path.clone();
        SAVE_HOOK.with(|hook| {
            *hook.borrow_mut() = Some((
                SaveStage::BeforeCommit,
                Box::new(move || {
                    fs::write(&destination, b"synthetic-competing-creator").unwrap();
                }),
            ))
        });
        assert!(draft.save(Some(&path)).is_err());
        assert_eq!(fs::read(&path).unwrap(), b"synthetic-competing-creator");
        assert_eq!(fs::read_dir(dir.path()).unwrap().count(), 1);
    }

    #[test]
    fn unexpected_displaced_symlink_does_not_modify_its_referent() {
        if crate::process::isolated_test(
            "config::tests::unexpected_displaced_symlink_does_not_modify_its_referent",
        ) {
            return;
        }
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("config");
        let mut draft = create_config(&path);
        let unrelated = dir.path().join("unrelated");
        fs::write(&unrelated, b"unrelated-synthetic-data").unwrap();
        fs::set_permissions(&unrelated, fs::Permissions::from_mode(0o644)).unwrap();
        let selected = path.clone();
        let referent = unrelated.clone();
        SAVE_HOOK.with(|hook| {
            *hook.borrow_mut() = Some((
                SaveStage::BeforeCommit,
                Box::new(move || {
                    fs::remove_file(&selected).unwrap();
                    symlink(&referent, &selected).unwrap();
                }),
            ))
        });
        let outcome = draft.save(Some(&path)).unwrap();
        assert!(outcome.requires_reload);
        assert!(outcome.warning.unwrap().contains("backup"));
        assert_eq!(fs::read(&unrelated).unwrap(), b"unrelated-synthetic-data");
        assert_eq!(fs::metadata(&unrelated).unwrap().mode() & 0o777, 0o644);
        assert!(Config::load(Some(&path)).is_ok());
        let backup = fs::read_dir(dir.path())
            .unwrap()
            .map(|entry| entry.unwrap().path())
            .find(|p| {
                p.file_name()
                    .unwrap()
                    .to_string_lossy()
                    .starts_with(".moonboot-")
            })
            .unwrap();
        assert_eq!(fs::read_link(backup).unwrap(), unrelated);
    }

    #[test]
    fn postcommit_in_place_content_change_requires_reload() {
        if crate::process::isolated_test(
            "config::tests::postcommit_in_place_content_change_requires_reload",
        ) {
            return;
        }
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("config");
        let mut draft = create_config(&path);
        let selected = path.clone();
        SAVE_HOOK.with(|hook| {
            *hook.borrow_mut() = Some((
                SaveStage::AfterCommit,
                Box::new(move || {
                    fs::write(&selected, b"synthetic-in-place-writer").unwrap();
                }),
            ))
        });
        let outcome = draft.save(Some(&path)).unwrap();
        assert!(outcome.requires_reload);
        assert!(outcome.warning.unwrap().contains("committed"));
        assert_eq!(fs::read(&path).unwrap(), b"synthetic-in-place-writer");
        assert!(draft.save(Some(&path)).is_err());
    }
}
