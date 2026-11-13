//! OpenCode-compatible background service configuration, registration, and
//! ownership monitoring.
//!
//! The service module owns only the cold-path discovery contract. HTTP
//! authentication and server orchestration remain in their respective modules.

use std::fs::{self, OpenOptions};
use std::io;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Mutex, OnceLock};
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use base64::Engine as _;
use base64::engine::general_purpose::URL_SAFE_NO_PAD;
use rand::RngExt;
use serde::{Deserialize, Serialize};
use sha1::Digest;

/// The OpenCode protocol version expected by the compatible client.
pub const UPSTREAM_COMPAT_VERSION: &str = "2.2.0";
/// OpenCode's default service port for the latest release channel (`0xc0de`).
pub const DEFAULT_PORT: u16 = 49_374;
const DEFAULT_HOSTNAME: &str = "127.0.0.1";
static TEMP_COUNTER: AtomicU64 = AtomicU64::new(0);
static REGISTRATION_FILES: OnceLock<Mutex<()>> = OnceLock::new();

/// Paths used by the OpenCode service contract.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ServicePaths {
    pub config_dir: PathBuf,
    pub config_path: PathBuf,
    pub registration_path: PathBuf,
}

/// Service connection settings persisted in the OpenCode config directory.
#[derive(Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ServiceConfig {
    pub hostname: String,
    pub port: u16,
    pub password: String,
}

impl std::fmt::Debug for ServiceConfig {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter
            .debug_struct("ServiceConfig")
            .field("hostname", &self.hostname)
            .field("port", &self.port)
            .field("password", &"[REDACTED]")
            .finish()
    }
}

/// The registration file read by OpenCode clients and the ownership watcher.
#[derive(Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Registration {
    pub id: String,
    pub version: String,
    pub url: String,
    pub pid: u32,
    pub password: String,
}

impl std::fmt::Debug for Registration {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter
            .debug_struct("Registration")
            .field("id", &self.id)
            .field("version", &self.version)
            .field("url", &self.url)
            .field("pid", &self.pid)
            .field("password", &"[REDACTED]")
            .finish()
    }
}

/// The result of checking whether a registration is still owned by this server.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum WatchDecision {
    Keep,
    Shutdown,
}

/// A registration that can remove only its still-owned file.
#[derive(Debug)]
pub struct RegistrationGuard {
    path: PathBuf,
    registration: Registration,
}

/// Resolve OpenCode's config and state paths using the process XDG settings.
#[must_use]
pub fn service_paths(
    home: &Path,
    config_root: Option<&Path>,
    state_root: Option<&Path>,
    channel: &str,
) -> ServicePaths {
    let config_env = std::env::var_os("XDG_CONFIG_HOME");
    let state_env = std::env::var_os("XDG_STATE_HOME");
    service_paths_with_env(
        home,
        config_root,
        state_root,
        channel,
        config_env.as_deref().map(Path::new),
        state_env.as_deref().map(Path::new),
    )
}

/// Resolve paths with explicit environment values; useful for isolated callers
/// and tests that must not mutate process-global environment variables.
#[must_use]
pub fn service_paths_with_env(
    home: &Path,
    config_root: Option<&Path>,
    state_root: Option<&Path>,
    channel: &str,
    xdg_config_home: Option<&Path>,
    xdg_state_home: Option<&Path>,
) -> ServicePaths {
    let config_root = config_root
        .or(xdg_config_home)
        .map(Path::to_path_buf)
        .unwrap_or_else(|| home.join(".config"));
    let state_root = state_root
        .or(xdg_state_home)
        .map(Path::to_path_buf)
        .unwrap_or_else(|| home.join(".local/state"));
    let channel = safe_channel(channel);
    let filename = if matches!(channel.as_str(), "latest" | "dev" | "beta" | "next") {
        "service.json".to_owned()
    } else {
        format!("service-{channel}.json")
    };
    ServicePaths {
        config_dir: config_root.join("opencode"),
        config_path: config_root.join("opencode").join(&filename),
        registration_path: state_root.join("opencode").join(filename),
    }
}

/// Resolve paths from the environment names used by OpenCode itself.
/// `OPENCODE_CONFIG_DIR` is already the `opencode` directory, unlike the XDG
/// roots accepted by [`service_paths_with_env`].
#[must_use]
pub fn service_paths_for_process(
    home: &Path,
    opencode_config_dir: Option<&Path>,
    xdg_config_home: Option<&Path>,
    xdg_state_home: Option<&Path>,
    channel: &str,
) -> ServicePaths {
    let config_dir = opencode_config_dir.map_or_else(
        || xdg_config_home.unwrap_or_else(|| Path::new("")).to_owned(),
        Path::to_path_buf,
    );
    let config_dir = if config_dir.as_os_str().is_empty() {
        home.join(".config/opencode")
    } else if opencode_config_dir.is_some() {
        config_dir
    } else {
        config_dir.join("opencode")
    };
    let state_root = xdg_state_home
        .map(Path::to_path_buf)
        .unwrap_or_else(|| home.join(".local/state"));
    let channel = safe_channel(channel);
    let filename = if matches!(channel.as_str(), "latest" | "dev" | "beta" | "next") {
        "service.json".to_owned()
    } else {
        format!("service-{channel}.json")
    };
    ServicePaths {
        config_dir: config_dir.clone(),
        config_path: config_dir.join(&filename),
        registration_path: state_root.join("opencode").join(filename),
    }
}

fn safe_channel(channel: &str) -> String {
    let mut result = String::with_capacity(channel.len());
    for character in channel.chars() {
        if character.is_ascii_alphanumeric() || matches!(character, '.' | '-' | '_') {
            result.push(character);
        } else {
            result.push('-');
        }
    }
    if result.is_empty() {
        "latest".to_owned()
    } else {
        result
    }
}

#[must_use]
pub fn default_port(channel: &str) -> u16 {
    match channel {
        "latest" | "dev" | "beta" | "next" => 49_374,
        "local" => 49_375,
        other => {
            let digest = sha1::Sha1::digest(other.as_bytes());
            let mut prefix = [0_u8; 4];
            prefix.copy_from_slice(&digest[..4]);
            10_000 + (u32::from_be_bytes(prefix) % 50_000) as u16
        }
    }
}

/// Read persisted service settings, creating a private config when absent.
pub fn load_or_create_config(path: &Path, default_port: u16) -> io::Result<ServiceConfig> {
    match fs::read(path) {
        Ok(bytes) => {
            let raw: RawServiceConfig = serde_json::from_slice(&bytes).map_err(io::Error::other)?;
            let password_missing = raw.password.as_deref().is_none_or(str::is_empty);
            let password = raw.password.filter(|password| !password.is_empty());
            let port_missing = raw.port.is_none();
            let port = match raw.port {
                None => default_port,
                Some(value) => value
                    .as_u64()
                    .and_then(|port| u16::try_from(port).ok())
                    .filter(|port| *port != 0)
                    .ok_or_else(|| {
                        io::Error::new(
                            io::ErrorKind::InvalidInput,
                            "port must be an integer from 1 through 65535",
                        )
                    })?,
            };
            let config = ServiceConfig {
                hostname: raw.hostname.unwrap_or_else(|| DEFAULT_HOSTNAME.to_owned()),
                port: port as u16,
                password: password.unwrap_or_else(generate_password),
            };
            if password_missing || port_missing {
                write_private_json(path, &config)?;
            }
            Ok(config)
        }
        Err(error) if error.kind() == io::ErrorKind::NotFound => {
            let config = ServiceConfig {
                hostname: DEFAULT_HOSTNAME.to_owned(),
                port: default_port,
                password: generate_password(),
            };
            write_private_json(path, &config)?;
            Ok(config)
        }
        Err(error) => Err(error),
    }
}

#[derive(Debug, Deserialize)]
struct RawServiceConfig {
    hostname: Option<String>,
    port: Option<serde_json::Value>,
    password: Option<String>,
}

fn generate_password() -> String {
    let mut bytes = [0_u8; 32];
    rand::rng().fill(&mut bytes);
    URL_SAFE_NO_PAD.encode(bytes)
}

/// Generate the per-process id written into a managed-service registration.
#[must_use]
pub fn new_registration_id() -> String {
    let mut bytes = [0_u8; 16];
    rand::rng().fill(&mut bytes);
    URL_SAFE_NO_PAD.encode(bytes)
}

/// Compare the registration currently on disk with the registration owned by a
/// watcher. A missing, malformed, or changed file always shuts down.
#[must_use]
pub fn ownership_decision(expected: &Registration, found: Option<&Registration>) -> WatchDecision {
    if found == Some(expected) {
        WatchDecision::Keep
    } else {
        WatchDecision::Shutdown
    }
}

/// Parse the registration file for a watcher tick.
pub fn read_registration(path: &Path) -> io::Result<Registration> {
    let bytes = fs::read(path)?;
    serde_json::from_slice(&bytes).map_err(io::Error::other)
}

/// Write a registration atomically with private-file permissions.
pub fn write_registration(
    path: &Path,
    registration: Registration,
) -> io::Result<RegistrationGuard> {
    let _lock = REGISTRATION_FILES
        .get_or_init(|| Mutex::new(()))
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner);
    if let Ok(existing) = read_registration(path)
        && existing.pid != registration.pid
        && pid_is_alive(existing.pid)
    {
        return Err(io::Error::new(
            io::ErrorKind::AlreadyExists,
            "managed service registration is owned by a live process",
        ));
    }
    let guard = RegistrationGuard {
        path: path.to_owned(),
        registration,
    };
    write_private_json(&guard.path, &guard.registration)?;
    Ok(guard)
}

fn write_private_json<T: Serialize>(path: &Path, value: &T) -> io::Result<()> {
    let parent = path
        .parent()
        .ok_or_else(|| io::Error::new(io::ErrorKind::InvalidInput, "path has no parent"))?;
    fs::create_dir_all(parent)?;
    let temporary = unique_temp_path(parent, path.file_name().unwrap_or_default());
    let bytes = serde_json::to_vec_pretty(value).map_err(io::Error::other)?;
    let mut options = OpenOptions::new();
    options.create_new(true).write(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt;
        options.mode(0o600);
    }
    let mut file = match options.open(&temporary) {
        Ok(file) => file,
        Err(error) => {
            let _ = fs::remove_file(&temporary);
            return Err(error);
        }
    };
    if let Err(error) = std::io::Write::write_all(&mut file, &bytes) {
        let _ = fs::remove_file(&temporary);
        return Err(error);
    }
    if let Err(error) = file.sync_all() {
        let _ = fs::remove_file(&temporary);
        return Err(error);
    }
    drop(file);
    if let Err(error) = fs::rename(&temporary, path) {
        let _ = fs::remove_file(&temporary);
        return Err(error);
    }
    Ok(())
}

#[must_use]
pub(crate) fn format_host_port(hostname: &str, port: u16) -> String {
    let host = hostname
        .strip_prefix('[')
        .and_then(|host| host.strip_suffix(']'))
        .unwrap_or(hostname);
    if host.contains(':') {
        format!("[{host}]:{port}")
    } else {
        format!("{host}:{port}")
    }
}

fn unique_temp_path(parent: &Path, filename: &std::ffi::OsStr) -> PathBuf {
    let counter = TEMP_COUNTER.fetch_add(1, Ordering::Relaxed);
    parent.join(format!(
        ".{}.tmp-{}-{}-{}",
        filename.to_string_lossy(),
        std::process::id(),
        counter,
        SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .map_or(0, |duration| duration.as_nanos())
    ))
}

impl RegistrationGuard {
    /// Remove the registration only when its complete ownership tuple remains.
    pub fn release(self) -> io::Result<()> {
        let _lock = REGISTRATION_FILES
            .get_or_init(|| Mutex::new(()))
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        let tombstone = unique_temp_path(
            self.path.parent().unwrap_or_else(|| Path::new(".")),
            self.path.file_name().unwrap_or_default(),
        );
        match fs::rename(&self.path, &tombstone) {
            Ok(()) => {}
            Err(error) if error.kind() == io::ErrorKind::NotFound => return Ok(()),
            Err(error) => return Err(error),
        }
        let owned = read_registration(&tombstone)
            .map(|found| found == self.registration)
            .unwrap_or(false);
        if owned {
            remove_file_if_present(&tombstone)
        } else {
            restore_non_owned_registration(&tombstone, &self.path)
        }
    }

    /// Return the registration owned by this guard.
    #[must_use]
    pub fn registration(&self) -> &Registration {
        &self.registration
    }
}

fn restore_non_owned_registration(tombstone: &Path, path: &Path) -> io::Result<()> {
    if path.exists() {
        return remove_file_if_present(tombstone);
    }
    match fs::hard_link(tombstone, path) {
        Ok(()) => remove_file_if_present(tombstone),
        Err(error) if error.kind() == io::ErrorKind::AlreadyExists => {
            remove_file_if_present(tombstone)
        }
        Err(error) => Err(error),
    }
}

fn remove_file_if_present(path: &Path) -> io::Result<()> {
    match fs::remove_file(path) {
        Ok(()) => Ok(()),
        Err(error) if error.kind() == io::ErrorKind::NotFound => Ok(()),
        Err(error) => Err(error),
    }
}

/// Return whether a process id is currently alive on Unix.
#[must_use]
pub fn pid_is_alive(pid: u32) -> bool {
    #[cfg(unix)]
    {
        if pid == 0 {
            return false;
        }
        let Ok(pid) = libc::pid_t::try_from(pid) else {
            return false;
        };
        // SAFETY: `kill(pid, 0)` sends no signal and only probes process
        // existence. The integer conversion is bounded by the platform pid type.
        let result = unsafe { libc::kill(pid, 0) };
        result == 0 || io::Error::last_os_error().raw_os_error() == Some(libc::EPERM)
    }
    #[cfg(not(unix))]
    {
        let _ = pid;
        false
    }
}

/// Spawn a low-frequency ownership monitor. The callback is invoked when the
/// registration disappears or changes; the task exits on cancellation.
pub async fn watch_registration(
    path: PathBuf,
    expected: Registration,
    stop: tokio_util::sync::CancellationToken,
    on_shutdown: impl FnOnce() + Send + 'static,
) {
    watch_registration_with_interval(path, expected, stop, Duration::from_secs(5), on_shutdown)
        .await;
}

async fn watch_registration_with_interval(
    path: PathBuf,
    expected: Registration,
    stop: tokio_util::sync::CancellationToken,
    interval_duration: Duration,
    on_shutdown: impl FnOnce() + Send + 'static,
) {
    if stop.is_cancelled() {
        return;
    }
    let mut interval = tokio::time::interval(interval_duration);
    interval.tick().await;
    loop {
        tokio::select! {
            _ = stop.cancelled() => return,
            _ = interval.tick() => {
                let found = read_registration(&path).ok();
                if ownership_decision(&expected, found.as_ref()) == WatchDecision::Shutdown {
                    on_shutdown();
                    return;
                }
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs;
    use std::path::{Path, PathBuf};
    use std::sync::Arc;
    use std::sync::atomic::{AtomicBool, AtomicUsize};

    fn temp_dir(name: &str) -> PathBuf {
        let path = std::env::temp_dir().join(format!(
            "ox-service-{name}-{}-{}",
            std::process::id(),
            SystemTime::now()
                .duration_since(UNIX_EPOCH)
                .expect("system clock is after unix epoch")
                .as_nanos()
        ));
        fs::create_dir_all(&path).expect("create test directory");
        path
    }

    #[test]
    fn service_paths_use_xdg_environment_when_roots_are_omitted() {
        let home = Path::new("/home/tester");
        let paths = service_paths_with_env(
            home,
            None,
            None,
            "latest",
            Some(Path::new("/xdg/config")),
            Some(Path::new("/xdg/state")),
        );
        assert_eq!(
            paths.config_path,
            PathBuf::from("/xdg/config/opencode/service.json")
        );
        assert_eq!(
            paths.registration_path,
            PathBuf::from("/xdg/state/opencode/service.json")
        );
    }

    #[test]
    fn explicit_roots_override_xdg_environment() {
        let paths = service_paths_with_env(
            Path::new("/home/tester"),
            Some(Path::new("/config")),
            Some(Path::new("/state")),
            "team/dev",
            Some(Path::new("/xdg/config")),
            Some(Path::new("/xdg/state")),
        );
        assert_eq!(paths.config_dir, PathBuf::from("/config/opencode"));
        assert_eq!(
            paths.registration_path,
            PathBuf::from("/state/opencode/service-team-dev.json")
        );
    }

    #[test]
    fn shared_channels_use_service_json_and_custom_channels_are_sanitized() {
        for channel in ["latest", "dev", "beta", "next"] {
            let paths = service_paths_with_env(
                Path::new("/home/tester"),
                Some(Path::new("/config")),
                Some(Path::new("/state")),
                channel,
                None,
                None,
            );
            assert_eq!(
                paths.config_path,
                PathBuf::from("/config/opencode/service.json")
            );
            assert_eq!(
                paths.registration_path,
                PathBuf::from("/state/opencode/service.json")
            );
        }

        let paths = service_paths_with_env(
            Path::new("/home/tester"),
            Some(Path::new("/config")),
            Some(Path::new("/state")),
            "team/release v1",
            None,
            None,
        );
        assert_eq!(
            paths.config_path,
            PathBuf::from("/config/opencode/service-team-release-v1.json")
        );
        assert_eq!(
            paths.registration_path,
            PathBuf::from("/state/opencode/service-team-release-v1.json")
        );
    }

    #[test]
    fn debug_does_not_expose_passwords() {
        let config = ServiceConfig {
            hostname: DEFAULT_HOSTNAME.to_owned(),
            port: DEFAULT_PORT,
            password: "config-secret".to_owned(),
        };
        let registration = registration("id", 7, "registration-secret");
        assert!(!format!("{config:?}").contains("config-secret"));
        assert!(!format!("{registration:?}").contains("registration-secret"));
    }

    #[test]
    fn endpoint_formats_ipv6_without_duplicate_brackets() {
        assert_eq!(format_host_port("::1", 49374), "[::1]:49374");
        assert_eq!(format_host_port("[::1]", 49374), "[::1]:49374");
        assert_eq!(format_host_port("127.0.0.1", 49374), "127.0.0.1:49374");
        assert_eq!(format_host_port("localhost", 49374), "localhost:49374");
    }

    #[cfg(unix)]
    #[test]
    fn pid_is_alive_rejects_zero_and_unrepresentable_pids() {
        assert!(!pid_is_alive(0));
        let unrepresentable = u32::try_from(i64::from(libc::pid_t::MAX) + 1)
            .expect("pid_t max plus one fits in u32 on supported Unix hosts");
        assert!(!pid_is_alive(unrepresentable));
    }

    #[test]
    fn missing_password_is_generated_and_persisted() {
        let root = temp_dir("password");
        let paths = service_paths_with_env(&root, Some(&root), Some(&root), "latest", None, None);
        let config =
            load_or_create_config(&paths.config_path, DEFAULT_PORT).expect("create service config");
        assert_eq!(config.hostname, DEFAULT_HOSTNAME);
        assert_eq!(config.port, DEFAULT_PORT);
        assert_eq!(config.password.len(), 43);
        let reloaded =
            load_or_create_config(&paths.config_path, DEFAULT_PORT).expect("read service config");
        assert_eq!(reloaded.password, config.password);
        let _ = fs::remove_dir_all(root);
    }

    #[test]
    fn empty_password_is_generated_and_existing_fields_are_preserved() {
        let root = temp_dir("empty-password");
        let path = root.join("service.json");
        fs::write(
            &path,
            r#"{"hostname":"localhost","port":12345,"password":""}"#,
        )
        .expect("write service config");

        let config = load_or_create_config(&path, DEFAULT_PORT).expect("read service config");
        assert_eq!(config.hostname, "localhost");
        assert_eq!(config.port, 12345);
        assert_eq!(config.password.len(), 43);
        let persisted: serde_json::Value =
            serde_json::from_slice(&fs::read(&path).expect("read persisted config"))
                .expect("valid persisted config");
        assert_eq!(persisted["hostname"], "localhost");
        assert_eq!(persisted["port"], 12345);
        assert_eq!(persisted["password"], config.password);
        let _ = fs::remove_dir_all(root);
    }

    #[test]
    fn invalid_service_ports_are_rejected_as_invalid_input() {
        let root = temp_dir("invalid-port");
        for port in [0, 65_536] {
            let path = root.join(format!("service-{port}.json"));
            fs::write(
                &path,
                format!(r#"{{"hostname":"localhost","port":{port},"password":"secret"}}"#),
            )
            .expect("write service config");
            let error =
                load_or_create_config(&path, DEFAULT_PORT).expect_err("invalid port must fail");
            assert_eq!(error.kind(), io::ErrorKind::InvalidInput);
        }
        let _ = fs::remove_dir_all(root);
    }

    #[test]
    fn missing_port_uses_and_persists_the_channel_default() {
        let root = temp_dir("channel-port");
        let path = root.join("service-team.json");
        fs::write(&path, r#"{"hostname":"localhost","password":"secret"}"#)
            .expect("write service config");

        let config = load_or_create_config(&path, 18_556).expect("read service config");
        assert_eq!(config.port, 18_556);
        let persisted: serde_json::Value =
            serde_json::from_slice(&fs::read(&path).expect("read persisted config"))
                .expect("valid persisted config");
        assert_eq!(persisted["port"], 18_556);
        assert_eq!(persisted["password"], "secret");
        let _ = fs::remove_dir_all(root);
    }

    #[test]
    fn custom_channel_default_port_matches_upstream_sha1_hash() {
        assert_eq!(default_port("latest"), 49_374);
        assert_eq!(default_port("local"), 49_375);
        assert_eq!(default_port("team"), 18_556);
    }

    #[test]
    fn ownership_decision_shuts_down_on_missing_or_changed_registration() {
        let expected = registration("id", 7, "secret");
        assert_eq!(ownership_decision(&expected, None), WatchDecision::Shutdown);
        assert_eq!(
            ownership_decision(&expected, Some(&registration("other", 7, "secret"))),
            WatchDecision::Shutdown
        );
        assert_eq!(
            ownership_decision(&expected, Some(&expected)),
            WatchDecision::Keep
        );
    }

    #[tokio::test]
    async fn watcher_honors_pre_cancelled_token() {
        let root = temp_dir("watch-cancelled");
        let stop = tokio_util::sync::CancellationToken::new();
        stop.cancel();
        let called = Arc::new(AtomicBool::new(false));
        let called_by_task = Arc::clone(&called);
        watch_registration_with_interval(
            root.join("service.json"),
            registration("id", 7, "secret"),
            stop,
            Duration::from_millis(1),
            move || called_by_task.store(true, Ordering::SeqCst),
        )
        .await;
        assert!(!called.load(Ordering::SeqCst));
        let _ = fs::remove_dir_all(root);
    }

    #[tokio::test]
    async fn watcher_requests_shutdown_when_registration_changes_or_disappears() {
        let root = temp_dir("watch-change");
        let path = root.join("service.json");
        let expected = registration("id", 7, "secret");
        let _guard = write_registration(&path, expected.clone()).expect("write registration");
        let stop = tokio_util::sync::CancellationToken::new();
        let called = Arc::new(AtomicUsize::new(0));
        let called_by_task = Arc::clone(&called);
        let task = tokio::spawn(watch_registration_with_interval(
            path.clone(),
            expected,
            stop,
            Duration::from_millis(1),
            move || {
                called_by_task.fetch_add(1, Ordering::SeqCst);
            },
        ));
        tokio::task::yield_now().await;
        fs::remove_file(&path).expect("remove registration");
        tokio::time::timeout(Duration::from_secs(1), task)
            .await
            .expect("watcher should stop")
            .expect("watcher task");
        assert_eq!(called.load(Ordering::SeqCst), 1);
        let _ = fs::remove_dir_all(root);
    }

    #[test]
    fn registration_is_private_atomic_and_release_is_fail_closed() {
        let root = temp_dir("registration");
        let path = root.join("service.json");
        let expected = registration("id", 7, "secret");
        let guard = write_registration(&path, expected.clone()).expect("write registration");
        let parsed: Registration =
            serde_json::from_slice(&fs::read(&path).expect("read registration"))
                .expect("valid registration json");
        assert_eq!(parsed, expected);
        #[cfg(unix)]
        assert_eq!(
            std::os::unix::fs::PermissionsExt::mode(
                &fs::metadata(&path).expect("metadata").permissions()
            ) & 0o777,
            0o600
        );
        fs::write(
            &path,
            serde_json::to_vec(&registration("other", 8, "other")).expect("json"),
        )
        .expect("replace registration");
        guard.release().expect("release should not fail");
        assert_eq!(
            serde_json::from_slice::<Registration>(&fs::read(&path).expect("read replacement"))
                .expect("replacement remains valid"),
            registration("other", 8, "other")
        );
        let guard = write_registration(&path, expected).expect("rewrite registration");
        guard.release().expect("release own registration");
        assert!(!path.exists());
        let _ = fs::remove_dir_all(root);
    }

    #[test]
    fn release_preserves_replacement_path_when_tombstone_restore_finds_existing_path() {
        let root = temp_dir("release-replacement");
        let path = root.join("service.json");
        let tombstone = root.join(".service.json.tombstone");
        let replacement = registration("replacement", 8, "replacement-secret");
        write_private_json(&tombstone, &registration("old", 7, "old-secret"))
            .expect("write tombstone");
        write_private_json(&path, &replacement).expect("write replacement");

        restore_non_owned_registration(&tombstone, &path).expect("restore should not fail");

        assert_eq!(
            read_registration(&path).expect("read replacement"),
            replacement
        );
        assert!(!tombstone.exists());
        let _ = fs::remove_dir_all(root);
    }

    #[test]
    fn private_json_removes_temporary_file_after_rename_failure() {
        let root = temp_dir("write-cleanup");
        let path = root.join("service.json");
        fs::create_dir(&path).expect("create destination directory");

        let error = write_private_json(&path, &registration("id", 7, "secret"))
            .expect_err("renaming over a directory must fail");
        assert_ne!(error.kind(), io::ErrorKind::NotFound);
        let entries: Vec<PathBuf> = fs::read_dir(&root)
            .expect("read test directory")
            .map(|entry| entry.expect("directory entry").path())
            .collect();
        assert_eq!(entries, vec![path.clone()]);
        let _ = fs::remove_dir_all(root);
    }

    #[test]
    fn live_other_owner_cannot_be_overwritten() {
        let root = temp_dir("live-owner");
        let path = root.join("service.json");
        let mut incumbent_process = std::process::Command::new("sleep")
            .arg("60")
            .spawn()
            .expect("spawn live incumbent");
        let incumbent_pid = incumbent_process.id();
        let incumbent = registration("incumbent", incumbent_pid, "secret");
        write_private_json(&path, &incumbent).expect("write incumbent");
        let result = write_registration(&path, registration("new", std::process::id(), "new"));
        let _ = incumbent_process.kill();
        let _ = incumbent_process.wait();
        let error = result.expect_err("a live other registration must block replacement");
        assert_eq!(error.kind(), io::ErrorKind::AlreadyExists);
        assert_eq!(read_registration(&path).expect("read incumbent"), incumbent);
        let _ = fs::remove_dir_all(root);
    }

    #[test]
    fn concurrent_writes_do_not_share_temporary_files() {
        let root = temp_dir("concurrent");
        let path = Arc::new(root.join("service.json"));
        let mut threads = Vec::new();
        for index in 0..16 {
            let path = Arc::clone(&path);
            threads.push(std::thread::spawn(move || {
                write_registration(&path, registration(&format!("id-{index}"), 7, "secret"))
                    .expect("concurrent registration write")
                    .release()
                    .expect("concurrent registration release");
            }));
        }
        for thread in threads {
            thread.join().expect("writer thread");
        }
        assert!(!path.exists());
        let _ = fs::remove_dir_all(root);
    }

    fn registration(id: &str, pid: u32, password: &str) -> Registration {
        Registration {
            id: id.to_owned(),
            version: UPSTREAM_COMPAT_VERSION.to_owned(),
            url: "http://127.0.0.1:49374".to_owned(),
            pid,
            password: password.to_owned(),
        }
    }
}
