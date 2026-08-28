use std::{
    fs::{self, OpenOptions},
    io::Write,
    path::{Path, PathBuf},
};

use serde::{Deserialize, Serialize};

use crate::services::persistence::{DATABASE_FILE, PersistenceError};

const BOOTSTRAP_FILE: &str = "storage-location.json";

#[derive(Clone, Debug, PartialEq, Eq, Serialize)]
#[serde(rename_all = "camelCase")]
pub(crate) struct StorageLocation {
    pub(crate) directory: PathBuf,
    pub(crate) database_path: PathBuf,
    pub(crate) data_size_bytes: u64,
    pub(crate) is_default: bool,
}

#[derive(Debug, Deserialize, Serialize)]
#[serde(rename_all = "camelCase")]
struct StorageBootstrap {
    storage_directory: PathBuf,
}

pub(crate) fn resolve_database_path(app_data: &Path) -> Result<PathBuf, PersistenceError> {
    let bootstrap_path = app_data.join(BOOTSTRAP_FILE);
    if !bootstrap_path.is_file() {
        return Ok(app_data.join(DATABASE_FILE));
    }
    let bytes = fs::read(&bootstrap_path).map_err(PersistenceError::FileOperation)?;
    let bootstrap: StorageBootstrap =
        serde_json::from_slice(&bytes).map_err(|_| PersistenceError::InvalidData)?;
    let database_path = bootstrap.storage_directory.join(DATABASE_FILE);
    if !database_path.is_file() {
        return Err(PersistenceError::InvalidData);
    }
    Ok(database_path)
}

pub(crate) fn describe(database_path: &Path, app_data: &Path) -> StorageLocation {
    let directory = database_path.parent().unwrap_or(app_data).to_path_buf();
    StorageLocation {
        data_size_bytes: database_size(database_path),
        is_default: same_path(&directory, app_data),
        directory,
        database_path: database_path.to_path_buf(),
    }
}

pub(crate) fn validate_destination(
    destination: &Path,
    current_directory: &Path,
) -> Result<(), PersistenceError> {
    if same_path(destination, current_directory) {
        return Err(PersistenceError::InvalidData);
    }
    if destination.exists() {
        if !destination.is_dir() {
            return Err(PersistenceError::InvalidData);
        }
        let mut entries = fs::read_dir(destination).map_err(PersistenceError::FileOperation)?;
        if entries
            .next()
            .transpose()
            .map_err(PersistenceError::FileOperation)?
            .is_some()
        {
            return Err(PersistenceError::InvalidData);
        }
    }
    Ok(())
}

pub(crate) fn write_bootstrap(
    app_data: &Path,
    storage_directory: &Path,
) -> Result<(), PersistenceError> {
    fs::create_dir_all(app_data).map_err(PersistenceError::CreateDirectory)?;
    let path = app_data.join(BOOTSTRAP_FILE);
    let temporary = app_data.join(format!("{BOOTSTRAP_FILE}.writing"));
    let bytes = serde_json::to_vec_pretty(&StorageBootstrap {
        storage_directory: storage_directory.to_path_buf(),
    })
    .map_err(|_| PersistenceError::InvalidData)?;
    let result = (|| {
        let mut file = OpenOptions::new()
            .create(true)
            .truncate(true)
            .write(true)
            .open(&temporary)
            .map_err(PersistenceError::FileOperation)?;
        file.write_all(&bytes)
            .and_then(|()| file.sync_all())
            .map_err(PersistenceError::FileOperation)?;
        drop(file);
        if path.exists() {
            fs::remove_file(&path).map_err(PersistenceError::FileOperation)?;
        }
        fs::rename(&temporary, &path).map_err(PersistenceError::FileOperation)
    })();
    if result.is_err() {
        let _ = fs::remove_file(temporary);
    }
    result
}

fn database_size(path: &Path) -> u64 {
    ["", "-wal", "-shm", "-journal"]
        .iter()
        .map(|suffix| {
            let mut value = path.as_os_str().to_os_string();
            value.push(suffix);
            fs::metadata(PathBuf::from(value)).map_or(0, |metadata| metadata.len())
        })
        .sum()
}

fn same_path(left: &Path, right: &Path) -> bool {
    let normalize = |path: &Path| {
        fs::canonicalize(path).unwrap_or_else(|_| {
            let parent = path.parent().unwrap_or_else(|| Path::new("."));
            let parent = fs::canonicalize(parent).unwrap_or_else(|_| parent.to_path_buf());
            path.file_name()
                .map_or(parent.clone(), |name| parent.join(name))
        })
    };
    normalize(left) == normalize(right)
}

#[cfg(test)]
mod tests {
    use std::fs::File;

    use tempfile::tempdir;

    use super::*;

    #[test]
    fn missing_bootstrap_uses_default_database() {
        let app_data = tempdir().unwrap();
        assert_eq!(
            resolve_database_path(app_data.path()).unwrap(),
            app_data.path().join(DATABASE_FILE)
        );
    }

    #[test]
    fn bootstrap_reopens_existing_custom_database() {
        let app_data = tempdir().unwrap();
        let storage = tempdir().unwrap();
        File::create(storage.path().join(DATABASE_FILE)).unwrap();
        write_bootstrap(app_data.path(), storage.path()).unwrap();
        assert_eq!(
            resolve_database_path(app_data.path()).unwrap(),
            storage.path().join(DATABASE_FILE)
        );
    }

    #[test]
    fn missing_custom_database_is_not_silently_replaced() {
        let app_data = tempdir().unwrap();
        let storage = tempdir().unwrap();
        write_bootstrap(app_data.path(), storage.path()).unwrap();
        assert!(matches!(
            resolve_database_path(app_data.path()),
            Err(PersistenceError::InvalidData)
        ));
    }

    #[test]
    fn destination_must_be_empty_and_different() {
        let current = tempdir().unwrap();
        let destination = tempdir().unwrap();
        assert!(validate_destination(destination.path(), current.path()).is_ok());
        File::create(destination.path().join("existing.txt")).unwrap();
        assert!(validate_destination(destination.path(), current.path()).is_err());
        assert!(validate_destination(current.path(), current.path()).is_err());
    }
}
