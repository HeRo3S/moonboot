use crate::backend::Error;
use serde::Deserialize;
use std::{
    env, fs,
    io::Read,
    os::unix::fs::OpenOptionsExt,
    path::{Path, PathBuf},
};

#[derive(Clone, Debug, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Config {
    pub tuya: Tuya,
    pub moonlight: Moonlight,
    pub startup: Startup,
    pub notifications: Notifications,
}

#[derive(Clone, Debug, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Tuya {
    pub endpoint: String,
    pub device_id: String,
    pub switch_code: String,
    pub credentials_file: PathBuf,
}

#[derive(Clone, Debug, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Moonlight {
    pub executable: String,
    pub host: String,
    pub app: String,
    #[serde(default)]
    pub stream_args: Vec<String>,
}

#[derive(Clone, Debug, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Startup {
    pub timeout_seconds: u64,
    pub poll_interval_seconds: u64,
    pub probe_timeout_seconds: u64,
    pub http_timeout_seconds: u64,
}

#[derive(Clone, Debug, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Notifications {
    pub enabled: bool,
}

impl Config {
    /// Loading configuration is inert: credentials and dependencies are not touched.
    pub fn load(path: Option<&Path>) -> Result<Self, Error> {
        let path = match path {
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
        };
        // Symlinks are allowed for declarative Nix configuration; FIFOs are not.
        let file = fs::OpenOptions::new()
            .read(true)
            .custom_flags(libc::O_NONBLOCK | libc::O_CLOEXEC)
            .open(&path)
            .map_err(|_| {
                Error::Config(
                    "Cannot read configuration; supply --config or create the XDG config file"
                        .into(),
                )
            })?;
        let metadata = file
            .metadata()
            .map_err(|_| Error::Config("Cannot inspect configuration".into()))?;
        if !metadata.is_file() || metadata.len() > 65536 {
            return Err(Error::Config(
                "Configuration must be a regular file smaller than 64 KiB".into(),
            ));
        }
        let mut text = String::new();
        file.take(65537)
            .read_to_string(&mut text)
            .map_err(|_| Error::Config("Cannot read configuration".into()))?;
        if text.len() > 65536 {
            return Err(Error::Config("Configuration exceeded 64 KiB".into()));
        }
        // TOML errors may include source lines, so never expose their Display text.
        let mut config: Self = toml::from_str(&text)
            .map_err(|_| Error::Config("Invalid configuration TOML or unknown fields".into()))?;
        config.tuya.credentials_file = expand_home(&config.tuya.credentials_file)?;
        config.validate()?;
        Ok(config)
    }

    pub(crate) fn validate(&self) -> Result<(), Error> {
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
        if !self.tuya.credentials_file.is_absolute() {
            return Err(Error::Config(
                "credentials_file must be an absolute path (~/ is supported)".into(),
            ));
        }
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

fn expand_home(path: &Path) -> Result<PathBuf, Error> {
    if path == Path::new("~") {
        return home();
    }
    if let Ok(rest) = path.strip_prefix("~/") {
        return Ok(home()?.join(rest));
    }
    Ok(path.to_owned())
}

#[cfg(test)]
mod tests {
    use super::*;
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
}
