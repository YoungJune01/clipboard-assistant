use std::{
    collections::HashSet,
    error::Error,
    fmt, fs,
    io::{self, Read, Seek, Write},
    path::{Component, Path, PathBuf},
};

use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use zip::{ZipArchive, ZipWriter, write::SimpleFileOptions};

const FORMAT_VERSION: u32 = 1;
const MANIFEST_PATH: &str = "manifest.json";
const DATABASE_PATH: &str = "clipboard-history.sqlite3";
const ASSETS_DIRECTORY: &str = "assets/";
const MAX_MANIFEST_BYTES: u64 = 1024 * 1024;
const MAX_DATABASE_BYTES: u64 = 2 * 1024 * 1024 * 1024;
const MAX_ASSET_BYTES: u64 = 256 * 1024 * 1024;
const MAX_ARCHIVE_BYTES: u64 = 4 * 1024 * 1024 * 1024;
const MAX_ARCHIVE_ENTRIES: usize = 10_000;

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub(crate) struct BackupManifest {
    pub(crate) format_version: u32,
    pub(crate) created_at: DateTime<Utc>,
    pub(crate) app_version: String,
    pub(crate) database_sha256: String,
    pub(crate) assets: Vec<BackupAsset>,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub(crate) struct BackupAsset {
    pub(crate) path: String,
    pub(crate) sha256: String,
    pub(crate) byte_length: u64,
}

#[derive(Debug)]
pub enum BackupError {
    Io(io::Error),
    Zip(zip::result::ZipError),
    Json(serde_json::Error),
    InvalidArchive,
    UnsupportedVersion(u32),
}

impl fmt::Display for BackupError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Io(_) | Self::Zip(_) => {
                formatter.write_str("clipboard backup file is unavailable")
            }
            Self::Json(_) | Self::InvalidArchive => {
                formatter.write_str("clipboard backup is invalid")
            }
            Self::UnsupportedVersion(version) => write!(
                formatter,
                "clipboard backup version {version} is unsupported"
            ),
        }
    }
}

impl Error for BackupError {
    fn source(&self) -> Option<&(dyn Error + 'static)> {
        match self {
            Self::Io(error) => Some(error),
            Self::Zip(error) => Some(error),
            Self::Json(error) => Some(error),
            Self::InvalidArchive | Self::UnsupportedVersion(_) => None,
        }
    }
}

impl From<io::Error> for BackupError {
    fn from(error: io::Error) -> Self {
        Self::Io(error)
    }
}

impl From<zip::result::ZipError> for BackupError {
    fn from(error: zip::result::ZipError) -> Self {
        Self::Zip(error)
    }
}

impl From<serde_json::Error> for BackupError {
    fn from(error: serde_json::Error) -> Self {
        Self::Json(error)
    }
}

pub(crate) enum PreparedBackup {
    Archive { database: PathBuf, staging: PathBuf },
    LegacySqlite(PathBuf),
}

impl PreparedBackup {
    pub(crate) fn database_path(&self) -> &Path {
        match self {
            Self::Archive { database, .. } | Self::LegacySqlite(database) => database,
        }
    }
}

impl Drop for PreparedBackup {
    fn drop(&mut self) {
        if let Self::Archive { staging, .. } = self {
            let _ = fs::remove_dir_all(staging);
        }
    }
}

pub(crate) fn create_archive(
    database: &Path,
    assets_root: Option<&Path>,
    destination: &Path,
    app_version: &str,
) -> Result<(), BackupError> {
    let temporary = work_path(destination, ".exporting");
    remove_file_if_exists(&temporary)?;
    let result = (|| {
        let database_metadata = fs::metadata(database)?;
        if !database_metadata.is_file() || database_metadata.len() > MAX_DATABASE_BYTES {
            return Err(BackupError::InvalidArchive);
        }
        let database_sha256 = hash_file(database, MAX_DATABASE_BYTES)?;
        let assets = collect_assets(assets_root)?;
        let manifest = BackupManifest {
            format_version: FORMAT_VERSION,
            created_at: Utc::now(),
            app_version: app_version.to_owned(),
            database_sha256,
            assets,
        };
        write_archive(database, assets_root, &temporary, &manifest)?;
        verify_archive(&temporary)?;
        replace_file(&temporary, destination)?;
        Ok(())
    })();
    if result.is_err() {
        let _ = remove_file_if_exists(&temporary);
    }
    result
}

pub(crate) fn prepare_restore(
    source: &Path,
    staging_parent: &Path,
) -> Result<PreparedBackup, BackupError> {
    if !source.is_file() {
        return Err(BackupError::InvalidArchive);
    }
    let mut file = fs::File::open(source)?;
    let mut signature = [0_u8; 4];
    let count = file.read(&mut signature)?;
    if count < signature.len() || signature != [0x50, 0x4b, 0x03, 0x04] {
        return Ok(PreparedBackup::LegacySqlite(source.to_path_buf()));
    }

    fs::create_dir_all(staging_parent)?;
    let staging = staging_parent.join(format!(".restore-archive-{}", uuid::Uuid::new_v4()));
    fs::create_dir(&staging)?;
    let result = extract_verified_archive(source, &staging);
    match result {
        Ok(database) => Ok(PreparedBackup::Archive { database, staging }),
        Err(error) => {
            let _ = fs::remove_dir_all(&staging);
            Err(error)
        }
    }
}

fn write_archive(
    database: &Path,
    assets_root: Option<&Path>,
    destination: &Path,
    manifest: &BackupManifest,
) -> Result<(), BackupError> {
    let file = fs::File::create(destination)?;
    let mut writer = ZipWriter::new(file);
    let options = SimpleFileOptions::default().compression_method(zip::CompressionMethod::Deflated);
    writer.start_file(MANIFEST_PATH, options)?;
    writer.write_all(&serde_json::to_vec_pretty(manifest)?)?;
    writer.start_file(DATABASE_PATH, options)?;
    copy_bounded(
        &mut fs::File::open(database)?,
        &mut writer,
        MAX_DATABASE_BYTES,
    )?;
    writer.add_directory(ASSETS_DIRECTORY, SimpleFileOptions::default())?;
    if let Some(root) = assets_root {
        for asset in &manifest.assets {
            writer.start_file(&asset.path, options)?;
            copy_bounded(
                &mut fs::File::open(root.join(asset.path.trim_start_matches(ASSETS_DIRECTORY)))?,
                &mut writer,
                MAX_ASSET_BYTES,
            )?;
        }
    }
    writer.finish()?.sync_all()?;
    Ok(())
}

fn verify_archive(path: &Path) -> Result<BackupManifest, BackupError> {
    if fs::metadata(path)?.len() > MAX_ARCHIVE_BYTES {
        return Err(BackupError::InvalidArchive);
    }
    let mut archive = ZipArchive::new(fs::File::open(path)?)?;
    validate_archive_entries(&mut archive)?;
    let manifest = read_manifest(&mut archive)?;
    validate_manifest(&manifest)?;
    let database_hash = hash_zip_entry(&mut archive, DATABASE_PATH, MAX_DATABASE_BYTES)?;
    if database_hash != manifest.database_sha256 {
        return Err(BackupError::InvalidArchive);
    }
    let declared_assets = manifest
        .assets
        .iter()
        .map(|asset| asset.path.as_str())
        .collect::<HashSet<_>>();
    for index in 0..archive.len() {
        let entry = archive.by_index(index)?;
        if entry.is_file()
            && entry.name().starts_with(ASSETS_DIRECTORY)
            && !declared_assets.contains(entry.name())
        {
            return Err(BackupError::InvalidArchive);
        }
    }
    for asset in &manifest.assets {
        let entry = archive.by_name(&asset.path)?;
        if entry.size() != asset.byte_length {
            return Err(BackupError::InvalidArchive);
        }
        drop(entry);
        let hash = hash_zip_entry(&mut archive, &asset.path, MAX_ASSET_BYTES)?;
        if hash != asset.sha256 {
            return Err(BackupError::InvalidArchive);
        }
    }
    Ok(manifest)
}

fn extract_verified_archive(source: &Path, staging: &Path) -> Result<PathBuf, BackupError> {
    let manifest = verify_archive(source)?;
    let mut archive = ZipArchive::new(fs::File::open(source)?)?;
    let database = staging.join(DATABASE_PATH);
    extract_entry(&mut archive, DATABASE_PATH, &database, MAX_DATABASE_BYTES)?;
    for asset in manifest.assets {
        let relative = asset.path.trim_start_matches(ASSETS_DIRECTORY);
        extract_entry(
            &mut archive,
            &asset.path,
            &staging.join(ASSETS_DIRECTORY).join(relative),
            MAX_ASSET_BYTES,
        )?;
    }
    Ok(database)
}

fn validate_archive_entries<R: Read + Seek>(
    archive: &mut ZipArchive<R>,
) -> Result<(), BackupError> {
    if archive.len() > MAX_ARCHIVE_ENTRIES {
        return Err(BackupError::InvalidArchive);
    }
    let mut names = HashSet::new();
    let mut total = 0_u64;
    for index in 0..archive.len() {
        let entry = archive.by_index(index)?;
        let name = entry.name();
        if !is_safe_archive_path(name) || !names.insert(name.to_owned()) {
            return Err(BackupError::InvalidArchive);
        }
        let limit = if name == MANIFEST_PATH {
            MAX_MANIFEST_BYTES
        } else if name == DATABASE_PATH {
            MAX_DATABASE_BYTES
        } else if name == ASSETS_DIRECTORY || (name.starts_with(ASSETS_DIRECTORY) && entry.is_dir())
        {
            0
        } else if name.starts_with(ASSETS_DIRECTORY) {
            MAX_ASSET_BYTES
        } else {
            return Err(BackupError::InvalidArchive);
        };
        if entry.size() > limit || entry.compressed_size() > MAX_ARCHIVE_BYTES {
            return Err(BackupError::InvalidArchive);
        }
        total = total
            .checked_add(entry.size())
            .ok_or(BackupError::InvalidArchive)?;
        if total > MAX_ARCHIVE_BYTES {
            return Err(BackupError::InvalidArchive);
        }
    }
    if !names.contains(MANIFEST_PATH) || !names.contains(DATABASE_PATH) {
        return Err(BackupError::InvalidArchive);
    }
    Ok(())
}

fn read_manifest<R: Read + Seek>(
    archive: &mut ZipArchive<R>,
) -> Result<BackupManifest, BackupError> {
    let entry = archive.by_name(MANIFEST_PATH)?;
    let mut bytes = Vec::with_capacity(entry.size() as usize);
    entry.take(MAX_MANIFEST_BYTES + 1).read_to_end(&mut bytes)?;
    if bytes.len() as u64 > MAX_MANIFEST_BYTES {
        return Err(BackupError::InvalidArchive);
    }
    Ok(serde_json::from_slice(&bytes)?)
}

fn validate_manifest(manifest: &BackupManifest) -> Result<(), BackupError> {
    if manifest.format_version != FORMAT_VERSION {
        return Err(BackupError::UnsupportedVersion(manifest.format_version));
    }
    if !is_sha256(&manifest.database_sha256) || manifest.assets.len() > MAX_ARCHIVE_ENTRIES - 2 {
        return Err(BackupError::InvalidArchive);
    }
    let mut paths = HashSet::new();
    for asset in &manifest.assets {
        if !asset.path.starts_with(ASSETS_DIRECTORY)
            || asset.path.ends_with('/')
            || !is_safe_archive_path(&asset.path)
            || !is_sha256(&asset.sha256)
            || asset.byte_length > MAX_ASSET_BYTES
            || !paths.insert(asset.path.clone())
        {
            return Err(BackupError::InvalidArchive);
        }
    }
    Ok(())
}

fn collect_assets(root: Option<&Path>) -> Result<Vec<BackupAsset>, BackupError> {
    let Some(root) = root.filter(|path| path.is_dir()) else {
        return Ok(Vec::new());
    };
    let mut pending = vec![root.to_path_buf()];
    let mut assets = Vec::new();
    while let Some(directory) = pending.pop() {
        for entry in fs::read_dir(&directory)? {
            let entry = entry?;
            let path = entry.path();
            let metadata = entry.metadata()?;
            if metadata.is_dir() {
                pending.push(path);
            } else if metadata.is_file() {
                if metadata.len() > MAX_ASSET_BYTES || assets.len() >= MAX_ARCHIVE_ENTRIES - 2 {
                    return Err(BackupError::InvalidArchive);
                }
                let relative = path
                    .strip_prefix(root)
                    .map_err(|_| BackupError::InvalidArchive)?;
                let relative = portable_relative_path(relative)?;
                assets.push(BackupAsset {
                    path: format!("{ASSETS_DIRECTORY}{relative}"),
                    sha256: hash_file(&path, MAX_ASSET_BYTES)?,
                    byte_length: metadata.len(),
                });
            }
        }
    }
    assets.sort_by(|left, right| left.path.cmp(&right.path));
    Ok(assets)
}

fn portable_relative_path(path: &Path) -> Result<String, BackupError> {
    let mut parts = Vec::new();
    for component in path.components() {
        match component {
            Component::Normal(value) => parts.push(value.to_string_lossy().into_owned()),
            _ => return Err(BackupError::InvalidArchive),
        }
    }
    let value = parts.join("/");
    if value.is_empty() || !is_safe_archive_path(&value) {
        return Err(BackupError::InvalidArchive);
    }
    Ok(value)
}

fn is_safe_archive_path(value: &str) -> bool {
    value == ASSETS_DIRECTORY
        || (!value.is_empty()
            && !value.starts_with('/')
            && !value.starts_with('\\')
            && !value.contains('\\')
            && !value.contains(':')
            && value
                .split('/')
                .all(|part| !matches!(part, "" | "." | "..")))
}

fn is_sha256(value: &str) -> bool {
    value.len() == 64 && value.bytes().all(|byte| byte.is_ascii_hexdigit())
}

fn hash_file(path: &Path, limit: u64) -> Result<String, BackupError> {
    let mut file = fs::File::open(path)?;
    hash_reader(&mut file, limit)
}

fn hash_zip_entry<R: Read + Seek>(
    archive: &mut ZipArchive<R>,
    name: &str,
    limit: u64,
) -> Result<String, BackupError> {
    let mut entry = archive.by_name(name)?;
    hash_reader(&mut entry, limit)
}

fn hash_reader(reader: &mut impl Read, limit: u64) -> Result<String, BackupError> {
    let mut hasher = Sha256::new();
    let copied = io::copy(&mut reader.take(limit + 1), &mut hasher)?;
    if copied > limit {
        return Err(BackupError::InvalidArchive);
    }
    Ok(format!("{:x}", hasher.finalize()))
}

fn copy_bounded(
    reader: &mut impl Read,
    writer: &mut impl Write,
    limit: u64,
) -> Result<(), BackupError> {
    let copied = io::copy(&mut reader.take(limit + 1), writer)?;
    if copied > limit {
        return Err(BackupError::InvalidArchive);
    }
    Ok(())
}

fn extract_entry<R: Read + Seek>(
    archive: &mut ZipArchive<R>,
    name: &str,
    destination: &Path,
    limit: u64,
) -> Result<(), BackupError> {
    let mut entry = archive.by_name(name)?;
    if let Some(parent) = destination.parent() {
        fs::create_dir_all(parent)?;
    }
    let mut output = fs::File::create(destination)?;
    copy_bounded(&mut entry, &mut output, limit)?;
    output.sync_all()?;
    Ok(())
}

fn replace_file(source: &Path, destination: &Path) -> Result<(), BackupError> {
    let previous = work_path(destination, ".previous");
    remove_file_if_exists(&previous)?;
    if destination.exists() {
        fs::rename(destination, &previous)?;
    }
    match fs::rename(source, destination) {
        Ok(()) => {
            let _ = remove_file_if_exists(&previous);
            Ok(())
        }
        Err(error) => {
            if previous.exists() {
                let _ = fs::rename(&previous, destination);
            }
            Err(BackupError::Io(error))
        }
    }
}

fn remove_file_if_exists(path: &Path) -> Result<(), BackupError> {
    match fs::remove_file(path) {
        Ok(()) => Ok(()),
        Err(error) if error.kind() == io::ErrorKind::NotFound => Ok(()),
        Err(error) => Err(error.into()),
    }
}

fn work_path(path: &Path, suffix: &str) -> PathBuf {
    let mut value = path.as_os_str().to_os_string();
    value.push(suffix);
    PathBuf::from(value)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Cursor;
    use tempfile::tempdir;

    fn write_archive_entries(path: &Path, entries: &[(&str, &[u8])]) {
        let mut writer = ZipWriter::new(fs::File::create(path).unwrap());
        for (name, bytes) in entries {
            writer
                .start_file(*name, SimpleFileOptions::default())
                .unwrap();
            writer.write_all(bytes).unwrap();
        }
        writer.finish().unwrap();
    }

    fn replace_all_bytes(path: &Path, from: &[u8], to: &[u8]) {
        assert_eq!(from.len(), to.len());
        let mut bytes = fs::read(path).unwrap();
        let mut replacements = 0;
        for index in 0..=bytes.len().saturating_sub(from.len()) {
            if &bytes[index..index + from.len()] == from {
                bytes[index..index + to.len()].copy_from_slice(to);
                replacements += 1;
            }
        }
        assert!(replacements >= 2);
        fs::write(path, bytes).unwrap();
    }

    #[test]
    fn manifest_round_trips_with_required_fields() {
        let manifest = BackupManifest {
            format_version: FORMAT_VERSION,
            created_at: Utc::now(),
            app_version: "1.2.3".to_owned(),
            database_sha256: "a".repeat(64),
            assets: vec![],
        };
        let encoded = serde_json::to_vec(&manifest).unwrap();
        assert_eq!(
            serde_json::from_slice::<BackupManifest>(&encoded).unwrap(),
            manifest
        );
    }

    #[test]
    fn archive_contains_verified_database_and_empty_assets_directory() {
        let directory = tempdir().unwrap();
        let database = directory.path().join("source.sqlite3");
        fs::write(&database, b"database bytes").unwrap();
        let destination = directory.path().join("backup.clipbackup");
        create_archive(&database, None, &destination, "0.1.0").unwrap();

        let manifest = verify_archive(&destination).unwrap();
        assert_eq!(
            manifest.database_sha256,
            hash_file(&database, MAX_DATABASE_BYTES).unwrap()
        );
        assert!(manifest.assets.is_empty());
        let mut archive = ZipArchive::new(fs::File::open(destination).unwrap()).unwrap();
        assert!(archive.by_name(ASSETS_DIRECTORY).is_ok());
    }

    #[test]
    fn asset_hashes_are_verified_and_extracted() {
        let directory = tempdir().unwrap();
        let database = directory.path().join("source.sqlite3");
        fs::write(&database, b"database bytes").unwrap();
        let assets = directory.path().join("source-assets");
        fs::create_dir_all(assets.join("nested")).unwrap();
        fs::write(assets.join("nested/image.png"), b"image bytes").unwrap();
        let destination = directory.path().join("backup.clipbackup");
        create_archive(&database, Some(&assets), &destination, "0.1.0").unwrap();

        let prepared = prepare_restore(&destination, directory.path()).unwrap();
        let PreparedBackup::Archive { staging, .. } = &prepared else {
            panic!("archive expected")
        };
        assert_eq!(
            fs::read(staging.join("assets/nested/image.png")).unwrap(),
            b"image bytes"
        );
    }

    #[test]
    fn tampered_database_is_rejected() {
        let directory = tempdir().unwrap();
        let destination = directory.path().join("backup.clipbackup");
        let manifest = BackupManifest {
            format_version: FORMAT_VERSION,
            created_at: Utc::now(),
            app_version: "0.1.0".to_owned(),
            database_sha256: "0".repeat(64),
            assets: vec![],
        };
        let json = serde_json::to_vec(&manifest).unwrap();
        write_archive_entries(
            &destination,
            &[(MANIFEST_PATH, &json), (DATABASE_PATH, b"tampered")],
        );
        assert!(matches!(
            verify_archive(&destination),
            Err(BackupError::InvalidArchive)
        ));
    }

    #[test]
    fn tampered_or_undeclared_asset_is_rejected() {
        let directory = tempdir().unwrap();
        let destination = directory.path().join("backup.clipbackup");
        let database = b"database";
        let asset = b"asset";
        let manifest = BackupManifest {
            format_version: FORMAT_VERSION,
            created_at: Utc::now(),
            app_version: "0.1.0".to_owned(),
            database_sha256: format!("{:x}", Sha256::digest(database)),
            assets: vec![BackupAsset {
                path: "assets/image.png".to_owned(),
                sha256: "0".repeat(64),
                byte_length: asset.len() as u64,
            }],
        };
        let json = serde_json::to_vec(&manifest).unwrap();
        write_archive_entries(
            &destination,
            &[
                (MANIFEST_PATH, &json),
                (DATABASE_PATH, database),
                ("assets/image.png", asset),
            ],
        );
        assert!(matches!(
            verify_archive(&destination),
            Err(BackupError::InvalidArchive)
        ));

        let manifest = BackupManifest {
            assets: vec![],
            ..manifest
        };
        let json = serde_json::to_vec(&manifest).unwrap();
        write_archive_entries(
            &destination,
            &[
                (MANIFEST_PATH, &json),
                (DATABASE_PATH, database),
                ("assets/image.png", asset),
            ],
        );
        assert!(matches!(
            verify_archive(&destination),
            Err(BackupError::InvalidArchive)
        ));
    }

    #[test]
    fn duplicate_and_traversal_paths_are_rejected() {
        let duplicate_directory = tempdir().unwrap();
        let duplicate_path = duplicate_directory.path().join("duplicate.clipbackup");
        write_archive_entries(
            &duplicate_path,
            &[
                (MANIFEST_PATH, b"{}"),
                ("manifest.jsox", b"{}"),
                (DATABASE_PATH, b"db"),
            ],
        );
        replace_all_bytes(&duplicate_path, b"manifest.jsox", b"manifest.json");
        assert!(verify_archive(&duplicate_path).is_err());

        for entries in [
            vec![
                (MANIFEST_PATH, b"{}".as_slice()),
                (DATABASE_PATH, b"db".as_slice()),
                ("assets/../escape", b"bad".as_slice()),
            ],
            vec![
                (MANIFEST_PATH, b"{}".as_slice()),
                (DATABASE_PATH, b"db".as_slice()),
                ("C:/escape", b"bad".as_slice()),
            ],
            vec![
                (MANIFEST_PATH, b"{}".as_slice()),
                (DATABASE_PATH, b"db".as_slice()),
                ("assets\\escape", b"bad".as_slice()),
            ],
        ] {
            let directory = tempdir().unwrap();
            let path = directory.path().join("bad.clipbackup");
            write_archive_entries(&path, &entries);
            assert!(verify_archive(&path).is_err());
        }
    }

    #[test]
    fn unsupported_format_version_is_rejected() {
        let manifest = BackupManifest {
            format_version: FORMAT_VERSION + 1,
            created_at: Utc::now(),
            app_version: "0.1.0".to_owned(),
            database_sha256: "0".repeat(64),
            assets: vec![],
        };
        assert!(matches!(
            validate_manifest(&manifest),
            Err(BackupError::UnsupportedVersion(_))
        ));
    }

    #[test]
    fn legacy_sqlite_file_is_preserved_for_existing_restore_path() {
        let directory = tempdir().unwrap();
        let source = directory.path().join("legacy.clipbackup");
        fs::write(&source, b"SQLite format 3\0legacy").unwrap();
        let prepared = prepare_restore(&source, directory.path()).unwrap();
        assert_eq!(prepared.database_path(), source);
        assert!(matches!(prepared, PreparedBackup::LegacySqlite(_)));
    }

    #[test]
    fn failed_export_preserves_existing_destination_and_cleans_temporary_file() {
        let directory = tempdir().unwrap();
        let database = directory.path().join("missing.sqlite3");
        let destination = directory.path().join("backup.clipbackup");
        fs::write(&destination, b"previous backup").unwrap();
        assert!(create_archive(&database, None, &destination, "0.1.0").is_err());
        assert_eq!(fs::read(&destination).unwrap(), b"previous backup");
        assert!(!work_path(&destination, ".exporting").exists());
    }

    #[test]
    fn safe_path_validation_rejects_ambiguous_paths() {
        assert!(is_safe_archive_path("assets/folder/file.bin"));
        assert!(!is_safe_archive_path("../file"));
        assert!(!is_safe_archive_path("/file"));
        assert!(!is_safe_archive_path("assets\\file"));
        assert!(!is_safe_archive_path("C:/file"));
        let _ = Cursor::new(Vec::<u8>::new());
    }
}
