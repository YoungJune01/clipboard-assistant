use std::{
    error::Error,
    fmt, fs,
    io::{Read, Write},
    path::{Path, PathBuf},
    sync::Arc,
    time::Duration,
};

use chrono::{DateTime, Utc};
use reqwest::{
    Method, StatusCode, Url,
    blocking::{Client, Response},
    header::{CONTENT_LENGTH, ETAG},
};
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};

use crate::{
    domain::{SyncInterval, WebDavConfig},
    services::{
        backup::{self, MAX_ARCHIVE_BYTES},
        persistence::PersistenceWorker,
    },
};

const ARCHIVE_NAME: &str = "clipboard-assistant-latest.clipbackup";
const STATE_NAME: &str = "clipboard-assistant-state.json";
const MAX_STATE_BYTES: u64 = 64 * 1024;

#[derive(Clone)]
pub struct WebDavCredential {
    username: String,
    password: String,
}

impl WebDavCredential {
    pub fn new(username: String, password: String) -> Self {
        Self { username, password }
    }
}

impl fmt::Debug for WebDavCredential {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("WebDavCredential")
            .field("username", &"[redacted]")
            .field("password", &"[redacted]")
            .finish()
    }
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum SyncOutcome {
    Disabled,
    Uploaded,
    Unchanged,
    Conflict { path: PathBuf },
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct RemoteObject {
    pub etag: Option<String>,
    pub bytes: Vec<u8>,
}

pub trait WebDavClient: Send + Sync {
    fn get(&self, path: &str, max_bytes: u64) -> Result<Option<RemoteObject>, SyncError>;
    fn put(&self, path: &str, bytes: &[u8]) -> Result<Option<String>, SyncError>;
    fn move_object(&self, source: &str, destination: &str) -> Result<(), SyncError>;
}

pub trait BackupSource: Send + Sync {
    fn create_backup(&self, destination: &Path) -> Result<(), SyncError>;
}

impl BackupSource for PersistenceWorker {
    fn create_backup(&self, destination: &Path) -> Result<(), SyncError> {
        self.backup(destination.to_path_buf())
            .map_err(|_| SyncError::LocalStorage)
    }
}

#[derive(Debug)]
pub enum SyncError {
    InvalidConfiguration,
    Authentication,
    Network,
    RemoteRejected,
    InvalidRemoteData,
    LocalStorage,
    Io(std::io::Error),
}

impl fmt::Display for SyncError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::InvalidConfiguration => formatter.write_str("WebDAV configuration is invalid"),
            Self::Authentication => formatter.write_str("WebDAV authentication failed"),
            Self::Network => formatter.write_str("WebDAV server is unavailable"),
            Self::RemoteRejected => formatter.write_str("WebDAV server rejected the request"),
            Self::InvalidRemoteData => formatter.write_str("remote clipboard backup is invalid"),
            Self::LocalStorage | Self::Io(_) => {
                formatter.write_str("local clipboard sync storage is unavailable")
            }
        }
    }
}

impl Error for SyncError {
    fn source(&self) -> Option<&(dyn Error + 'static)> {
        match self {
            Self::Io(error) => Some(error),
            _ => None,
        }
    }
}

impl From<std::io::Error> for SyncError {
    fn from(error: std::io::Error) -> Self {
        Self::Io(error)
    }
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
struct RemoteState {
    format_version: u32,
    sha256: String,
    device_id: String,
    updated_at: DateTime<Utc>,
}

pub struct ReqwestWebDavClient {
    client: Client,
    base_url: Url,
    credential: WebDavCredential,
}

impl ReqwestWebDavClient {
    pub fn new(config: &WebDavConfig, credential: WebDavCredential) -> Result<Self, SyncError> {
        let base_url = validated_base_url(config)?;
        let client = Client::builder()
            .connect_timeout(Duration::from_secs(15))
            .timeout(Duration::from_secs(120))
            .build()
            .map_err(|_| SyncError::Network)?;
        Ok(Self {
            client,
            base_url,
            credential,
        })
    }

    pub fn test_connection(&self) -> Result<(), SyncError> {
        let response = self
            .request(
                Method::from_bytes(b"PROPFIND").map_err(|_| SyncError::Network)?,
                "",
            )?
            .header("Depth", "0")
            .send()
            .map_err(|_| SyncError::Network)?;
        checked_response(response).map(|_| ())
    }

    fn request(
        &self,
        method: Method,
        path: &str,
    ) -> Result<reqwest::blocking::RequestBuilder, SyncError> {
        let url = self
            .base_url
            .join(path)
            .map_err(|_| SyncError::InvalidConfiguration)?;
        Ok(self
            .client
            .request(method, url)
            .basic_auth(&self.credential.username, Some(&self.credential.password)))
    }
}

impl WebDavClient for ReqwestWebDavClient {
    fn get(&self, path: &str, max_bytes: u64) -> Result<Option<RemoteObject>, SyncError> {
        let response = self
            .request(Method::GET, path)?
            .send()
            .map_err(|_| SyncError::Network)?;
        if response.status() == StatusCode::NOT_FOUND {
            return Ok(None);
        }
        let response = checked_response(response)?;
        if response
            .headers()
            .get(CONTENT_LENGTH)
            .and_then(|value| value.to_str().ok())
            .and_then(|value| value.parse::<u64>().ok())
            .is_some_and(|length| length > max_bytes)
        {
            return Err(SyncError::InvalidRemoteData);
        }
        let etag = response
            .headers()
            .get(ETAG)
            .and_then(|value| value.to_str().ok())
            .map(str::to_owned);
        let mut reader = response.take(max_bytes + 1);
        let mut bytes = Vec::new();
        reader
            .read_to_end(&mut bytes)
            .map_err(|_| SyncError::Network)?;
        if bytes.len() as u64 > max_bytes {
            return Err(SyncError::InvalidRemoteData);
        }
        Ok(Some(RemoteObject { etag, bytes }))
    }

    fn put(&self, path: &str, bytes: &[u8]) -> Result<Option<String>, SyncError> {
        let response = self
            .request(Method::PUT, path)?
            .body(bytes.to_vec())
            .send()
            .map_err(|_| SyncError::Network)?;
        let response = checked_response(response)?;
        Ok(response
            .headers()
            .get(ETAG)
            .and_then(|value| value.to_str().ok())
            .map(str::to_owned))
    }

    fn move_object(&self, source: &str, destination: &str) -> Result<(), SyncError> {
        let destination = self
            .base_url
            .join(destination)
            .map_err(|_| SyncError::InvalidConfiguration)?;
        let response = self
            .request(
                Method::from_bytes(b"MOVE").map_err(|_| SyncError::Network)?,
                source,
            )?
            .header("Destination", destination.as_str())
            .header("Overwrite", "T")
            .send()
            .map_err(|_| SyncError::Network)?;
        checked_response(response).map(|_| ())
    }
}

pub fn synchronize(
    config: &mut WebDavConfig,
    client: &dyn WebDavClient,
    backup_source: &dyn BackupSource,
    staging_root: &Path,
    now: DateTime<Utc>,
) -> Result<SyncOutcome, SyncError> {
    if !config.enabled {
        return Ok(SyncOutcome::Disabled);
    }
    validated_base_url(config)?;
    fs::create_dir_all(staging_root)?;
    let local_path = staging_root.join(format!("sync-{}.clipbackup", uuid::Uuid::new_v4()));
    let result = (|| {
        backup_source.create_backup(&local_path)?;
        backup::verify_archive(&local_path).map_err(|_| SyncError::LocalStorage)?;
        let local_hash = hash_file(&local_path)?;
        let remote = client.get(STATE_NAME, MAX_STATE_BYTES)?;
        let remote_etag = remote.as_ref().and_then(|object| object.etag.clone());
        let remote_state = remote
            .map(|object| serde_json::from_slice::<RemoteState>(&object.bytes))
            .transpose()
            .map_err(|_| SyncError::InvalidRemoteData)?;

        if remote_state
            .as_ref()
            .is_some_and(|state| state.sha256 == local_hash)
        {
            config.last_local_sha256 = Some(local_hash.clone());
            config.last_remote_sha256 = Some(local_hash);
            config.last_etag = remote_etag;
            config.last_success_at = Some(now);
            config.last_result = Some("unchanged".to_owned());
            return Ok(SyncOutcome::Unchanged);
        }

        let local_changed = config
            .last_local_sha256
            .as_ref()
            .is_some_and(|previous| previous != &local_hash);
        let remote_changed = match (&config.last_remote_sha256, &remote_state) {
            (Some(previous), Some(remote)) => previous != &remote.sha256,
            (None, Some(_)) => true,
            _ => false,
        };
        if local_changed && remote_changed {
            let remote_archive = client
                .get(ARCHIVE_NAME, MAX_ARCHIVE_BYTES)?
                .ok_or(SyncError::InvalidRemoteData)?;
            let conflicts = staging_root.join("conflicts");
            fs::create_dir_all(&conflicts)?;
            let conflict_path = conflicts.join(format!(
                "{}-{}.clipbackup",
                now.format("%Y%m%d-%H%M%S"),
                safe_device_id(
                    remote_state
                        .as_ref()
                        .map_or("remote", |state| &state.device_id)
                )
            ));
            write_verified_download(&conflict_path, &remote_archive.bytes)?;
            config.last_result = Some("conflict".to_owned());
            return Ok(SyncOutcome::Conflict {
                path: conflict_path,
            });
        }

        let archive_bytes = fs::read(&local_path)?;
        let suffix = uuid::Uuid::new_v4();
        let temporary_archive = format!("{ARCHIVE_NAME}.{suffix}.uploading");
        client.put(&temporary_archive, &archive_bytes)?;
        client.move_object(&temporary_archive, ARCHIVE_NAME)?;
        let state = RemoteState {
            format_version: 1,
            sha256: local_hash.clone(),
            device_id: config.device_id.clone(),
            updated_at: now,
        };
        let state_bytes = serde_json::to_vec(&state).map_err(|_| SyncError::LocalStorage)?;
        let temporary_state = format!("{STATE_NAME}.{suffix}.uploading");
        let state_etag = client.put(&temporary_state, &state_bytes)?;
        client.move_object(&temporary_state, STATE_NAME)?;
        config.last_local_sha256 = Some(local_hash.clone());
        config.last_remote_sha256 = Some(local_hash);
        config.last_etag = state_etag;
        config.last_success_at = Some(now);
        config.last_result = Some("uploaded".to_owned());
        Ok(SyncOutcome::Uploaded)
    })();
    let _ = fs::remove_file(local_path);
    result
}

pub fn next_run(now: DateTime<Utc>, interval: SyncInterval) -> Option<DateTime<Utc>> {
    let duration = interval.duration()?;
    chrono::Duration::from_std(duration)
        .ok()
        .map(|duration| now + duration)
}

fn validated_base_url(config: &WebDavConfig) -> Result<Url, SyncError> {
    let mut url =
        Url::parse(config.endpoint.trim()).map_err(|_| SyncError::InvalidConfiguration)?;
    if url.host_str().is_none()
        || (url.scheme() != "https" && !(url.scheme() == "http" && config.allow_insecure_http))
    {
        return Err(SyncError::InvalidConfiguration);
    }
    if !url.path().ends_with('/') {
        url.set_path(&format!("{}/", url.path()));
    }
    let folder = config.remote_folder.trim().trim_matches('/');
    if !folder.is_empty() {
        url = url
            .join(&format!("{folder}/"))
            .map_err(|_| SyncError::InvalidConfiguration)?;
    }
    Ok(url)
}

fn checked_response(response: Response) -> Result<Response, SyncError> {
    match response.status() {
        StatusCode::UNAUTHORIZED | StatusCode::FORBIDDEN => Err(SyncError::Authentication),
        status if status.is_success() => Ok(response),
        _ => Err(SyncError::RemoteRejected),
    }
}

fn hash_file(path: &Path) -> Result<String, SyncError> {
    let mut file = fs::File::open(path)?;
    let mut digest = Sha256::new();
    std::io::copy(&mut file, &mut digest).map_err(SyncError::Io)?;
    Ok(format!("{:x}", digest.finalize()))
}

fn write_verified_download(path: &Path, bytes: &[u8]) -> Result<(), SyncError> {
    if bytes.len() as u64 > MAX_ARCHIVE_BYTES {
        return Err(SyncError::InvalidRemoteData);
    }
    let temporary = path.with_extension("downloading");
    if let Some(parent) = path.parent() {
        fs::create_dir_all(parent)?;
    }
    let result = (|| {
        let mut file = fs::File::create(&temporary)?;
        file.write_all(bytes)?;
        file.sync_all()?;
        backup::verify_archive(&temporary).map_err(|_| SyncError::InvalidRemoteData)?;
        fs::rename(&temporary, path)?;
        Ok(())
    })();
    if result.is_err() {
        let _ = fs::remove_file(&temporary);
    }
    result
}

fn safe_device_id(value: &str) -> String {
    let value = value
        .chars()
        .filter(|character| character.is_ascii_alphanumeric() || matches!(character, '-' | '_'))
        .take(64)
        .collect::<String>();
    if value.is_empty() {
        "remote".to_owned()
    } else {
        value
    }
}

pub struct SyncScheduler {
    stop: Option<std::sync::mpsc::Sender<()>>,
    thread: Option<std::thread::JoinHandle<()>>,
}

impl SyncScheduler {
    pub fn start(interval: SyncInterval, on_tick: Arc<dyn Fn() + Send + Sync>) -> Option<Self> {
        let duration = interval.duration()?;
        let (stop, stopped) = std::sync::mpsc::channel();
        let thread = std::thread::Builder::new()
            .name("clipboard-webdav-sync".to_owned())
            .spawn(move || {
                loop {
                    match stopped.recv_timeout(duration) {
                        Ok(()) | Err(std::sync::mpsc::RecvTimeoutError::Disconnected) => break,
                        Err(std::sync::mpsc::RecvTimeoutError::Timeout) => on_tick(),
                    }
                }
            })
            .ok()?;
        Some(Self {
            stop: Some(stop),
            thread: Some(thread),
        })
    }
}

impl Drop for SyncScheduler {
    fn drop(&mut self) {
        if let Some(stop) = self.stop.take() {
            let _ = stop.send(());
        }
        if let Some(thread) = self.thread.take() {
            let _ = thread.join();
        }
    }
}

#[cfg(test)]
mod tests {
    use std::{collections::HashMap, sync::Mutex};

    use tempfile::TempDir;

    use super::*;

    #[derive(Default)]
    struct FakeClient {
        objects: Mutex<HashMap<String, RemoteObject>>,
        calls: Mutex<Vec<String>>,
        fail_auth: bool,
    }

    impl WebDavClient for FakeClient {
        fn get(&self, path: &str, _max_bytes: u64) -> Result<Option<RemoteObject>, SyncError> {
            self.calls.lock().unwrap().push(format!("GET {path}"));
            if self.fail_auth {
                return Err(SyncError::Authentication);
            }
            Ok(self.objects.lock().unwrap().get(path).cloned())
        }

        fn put(&self, path: &str, bytes: &[u8]) -> Result<Option<String>, SyncError> {
            self.calls.lock().unwrap().push(format!("PUT {path}"));
            self.objects.lock().unwrap().insert(
                path.to_owned(),
                RemoteObject {
                    etag: Some("etag".to_owned()),
                    bytes: bytes.to_vec(),
                },
            );
            Ok(Some("etag".to_owned()))
        }

        fn move_object(&self, source: &str, destination: &str) -> Result<(), SyncError> {
            self.calls
                .lock()
                .unwrap()
                .push(format!("MOVE {source} {destination}"));
            let object = self
                .objects
                .lock()
                .unwrap()
                .remove(source)
                .ok_or(SyncError::RemoteRejected)?;
            self.objects
                .lock()
                .unwrap()
                .insert(destination.to_owned(), object);
            Ok(())
        }
    }

    struct TestBackup {
        source: PathBuf,
    }

    impl BackupSource for TestBackup {
        fn create_backup(&self, destination: &Path) -> Result<(), SyncError> {
            fs::copy(&self.source, destination)?;
            Ok(())
        }
    }

    fn valid_backup(directory: &TempDir) -> PathBuf {
        let database = directory.path().join("source.sqlite3");
        rusqlite::Connection::open(&database).unwrap();
        let backup = directory.path().join("source.clipbackup");
        backup::create_archive(&database, None, &backup, "test").unwrap();
        backup
    }

    fn config() -> WebDavConfig {
        WebDavConfig {
            enabled: true,
            endpoint: "https://dav.example.test/".to_owned(),
            ..WebDavConfig::default()
        }
    }

    #[test]
    fn disabled_sync_never_contacts_webdav_or_creates_a_backup() {
        let directory = TempDir::new().unwrap();
        let client = FakeClient::default();
        let source = TestBackup {
            source: directory.path().join("missing"),
        };
        let mut config = WebDavConfig::default();
        assert_eq!(
            synchronize(&mut config, &client, &source, directory.path(), Utc::now()).unwrap(),
            SyncOutcome::Disabled
        );
        assert!(client.calls.lock().unwrap().is_empty());
    }

    #[test]
    fn manual_sync_uploads_a_verified_backup_with_atomic_moves() {
        let directory = TempDir::new().unwrap();
        let source = TestBackup {
            source: valid_backup(&directory),
        };
        let client = FakeClient::default();
        let mut config = config();
        assert_eq!(
            synchronize(&mut config, &client, &source, directory.path(), Utc::now()).unwrap(),
            SyncOutcome::Uploaded
        );
        let objects = client.objects.lock().unwrap();
        let archive = objects.get(ARCHIVE_NAME).unwrap();
        let downloaded = directory.path().join("downloaded.clipbackup");
        fs::write(&downloaded, &archive.bytes).unwrap();
        backup::verify_archive(&downloaded).unwrap();
        let calls = client.calls.lock().unwrap();
        assert!(
            calls
                .iter()
                .any(|call| call.starts_with("MOVE ") && call.ends_with(ARCHIVE_NAME))
        );
        assert!(
            calls
                .iter()
                .any(|call| call.starts_with("MOVE ") && call.ends_with(STATE_NAME))
        );
    }

    #[test]
    fn unchanged_remote_hash_skips_archive_transfer() {
        let directory = TempDir::new().unwrap();
        let source_path = valid_backup(&directory);
        let hash = hash_file(&source_path).unwrap();
        let source = TestBackup {
            source: source_path,
        };
        let client = FakeClient::default();
        client.objects.lock().unwrap().insert(
            STATE_NAME.to_owned(),
            RemoteObject {
                etag: Some("same".to_owned()),
                bytes: serde_json::to_vec(&RemoteState {
                    format_version: 1,
                    sha256: hash,
                    device_id: "other".to_owned(),
                    updated_at: Utc::now(),
                })
                .unwrap(),
            },
        );
        let mut config = config();
        assert_eq!(
            synchronize(&mut config, &client, &source, directory.path(), Utc::now()).unwrap(),
            SyncOutcome::Unchanged
        );
        assert_eq!(
            client.calls.lock().unwrap().as_slice(),
            [format!("GET {STATE_NAME}")]
        );
    }

    #[test]
    fn authentication_failure_does_not_modify_remote_or_local_state() {
        let directory = TempDir::new().unwrap();
        let source = TestBackup {
            source: valid_backup(&directory),
        };
        let client = FakeClient {
            fail_auth: true,
            ..FakeClient::default()
        };
        let mut config = config();
        let original = config.clone();
        assert!(matches!(
            synchronize(&mut config, &client, &source, directory.path(), Utc::now()),
            Err(SyncError::Authentication)
        ));
        assert_eq!(config, original);
        assert!(client.objects.lock().unwrap().is_empty());
    }

    #[test]
    fn divergent_local_and_remote_state_creates_a_verified_conflict_copy() {
        let directory = TempDir::new().unwrap();
        let local = valid_backup(&directory);
        let remote_directory = TempDir::new().unwrap();
        let remote = valid_backup(&remote_directory);
        let source = TestBackup { source: local };
        let client = FakeClient::default();
        client.objects.lock().unwrap().insert(
            STATE_NAME.to_owned(),
            RemoteObject {
                etag: None,
                bytes: serde_json::to_vec(&RemoteState {
                    format_version: 1,
                    sha256: hash_file(&remote).unwrap(),
                    device_id: "remote-device".to_owned(),
                    updated_at: Utc::now(),
                })
                .unwrap(),
            },
        );
        client.objects.lock().unwrap().insert(
            ARCHIVE_NAME.to_owned(),
            RemoteObject {
                etag: None,
                bytes: fs::read(remote).unwrap(),
            },
        );
        let mut config = config();
        config.last_local_sha256 = Some("old-local".to_owned());
        config.last_remote_sha256 = Some("old-remote".to_owned());
        let outcome =
            synchronize(&mut config, &client, &source, directory.path(), Utc::now()).unwrap();
        let SyncOutcome::Conflict { path } = outcome else {
            panic!("expected conflict")
        };
        backup::verify_archive(&path).unwrap();
        assert!(
            !client
                .calls
                .lock()
                .unwrap()
                .iter()
                .any(|call| call.starts_with("PUT "))
        );
    }

    #[test]
    fn tampered_remote_conflict_archive_is_rejected_and_removed() {
        let directory = TempDir::new().unwrap();
        let source = TestBackup {
            source: valid_backup(&directory),
        };
        let client = FakeClient::default();
        client.objects.lock().unwrap().insert(
            STATE_NAME.to_owned(),
            RemoteObject {
                etag: None,
                bytes: serde_json::to_vec(&RemoteState {
                    format_version: 1,
                    sha256: "remote-hash".to_owned(),
                    device_id: "remote-device".to_owned(),
                    updated_at: Utc::now(),
                })
                .unwrap(),
            },
        );
        client.objects.lock().unwrap().insert(
            ARCHIVE_NAME.to_owned(),
            RemoteObject {
                etag: None,
                bytes: b"not a clipboard backup".to_vec(),
            },
        );
        let mut config = config();
        config.last_local_sha256 = Some("old-local".to_owned());
        config.last_remote_sha256 = Some("old-remote".to_owned());

        assert!(matches!(
            synchronize(&mut config, &client, &source, directory.path(), Utc::now()),
            Err(SyncError::InvalidRemoteData)
        ));
        let conflicts = directory.path().join("conflicts");
        assert!(!conflicts.exists() || fs::read_dir(conflicts).unwrap().next().is_none());
    }

    #[test]
    fn scheduler_intervals_and_credentials_are_safe() {
        let now = Utc::now();
        assert_eq!(next_run(now, SyncInterval::Manual), None);
        assert_eq!(
            next_run(now, SyncInterval::FifteenMinutes),
            Some(now + chrono::Duration::minutes(15))
        );
        assert_eq!(
            next_run(now, SyncInterval::OneHour),
            Some(now + chrono::Duration::hours(1))
        );
        assert_eq!(
            next_run(now, SyncInterval::SixHours),
            Some(now + chrono::Duration::hours(6))
        );
        assert_eq!(
            next_run(now, SyncInterval::Daily),
            Some(now + chrono::Duration::days(1))
        );
        let credential =
            WebDavCredential::new("secret-user".to_owned(), "secret-password".to_owned());
        let debug = format!("{credential:?}");
        assert!(!debug.contains("secret-user"));
        assert!(!debug.contains("secret-password"));
        assert!(
            !serde_json::to_string(&config())
                .unwrap()
                .contains("password")
        );
    }
}
