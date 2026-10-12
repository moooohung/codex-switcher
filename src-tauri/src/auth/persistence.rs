//! Locked transactions and durable replacement for credential files.
use anyhow::{Context, Result};
use serde::de::DeserializeOwned;
use std::{
    fs::{self, File, OpenOptions},
    io::Write,
    path::{Path, PathBuf},
};

pub fn lock_file(path: &Path) -> Result<File> {
    if let Some(parent) = path.parent() {
        fs::create_dir_all(parent)?;
    }
    let file = OpenOptions::new()
        .read(true)
        .write(true)
        .create(true)
        .truncate(false)
        .open(path)?;
    file.lock().context("Could not lock account storage")?;
    Ok(file)
}

pub fn backup_path(path: &Path) -> PathBuf {
    let mut name = path.as_os_str().to_os_string();
    name.push(".bak");
    PathBuf::from(name)
}

pub fn read_json<T: DeserializeOwned>(path: &Path) -> Result<Option<T>> {
    // Deletion is intentional logout; only repair a damaged existing primary.
    if !path.exists() {
        return Ok(None);
    }
    let primary = fs::read(path)
        .context("Could not read credential file")
        .and_then(|bytes| serde_json::from_slice::<T>(&bytes).context("Invalid credential JSON"));
    match primary {
        Ok(value) => Ok(Some(value)),
        Err(primary_error) => {
            let bytes = fs::read(backup_path(path))
                .with_context(|| format!("{primary_error}; no readable recovery copy"))?;
            let value = serde_json::from_slice::<T>(&bytes).context("Recovery copy is invalid")?;
            replace_bytes(path, &bytes)?;
            Ok(Some(value))
        }
    }
}

pub fn atomic_write(path: &Path, bytes: &[u8]) -> Result<()> {
    // Never replace a valid recovery copy with a damaged primary file.
    if let Ok(previous) = fs::read(path) {
        if serde_json::from_slice::<serde_json::Value>(&previous).is_ok() {
            replace_bytes(&backup_path(path), &previous)?;
        }
    }
    replace_bytes(path, bytes)
}

fn replace_bytes(path: &Path, bytes: &[u8]) -> Result<()> {
    let parent = path.parent().context("Storage path has no parent")?;
    fs::create_dir_all(parent)?;
    let temporary = parent.join(format!(".switcher-{}.tmp", uuid::Uuid::new_v4()));
    let result = (|| -> Result<()> {
        let mut options = OpenOptions::new();
        options.write(true).create_new(true);
        #[cfg(unix)]
        {
            use std::os::unix::fs::OpenOptionsExt;
            options.mode(0o600);
        }
        let mut file = options.open(&temporary)?;
        file.write_all(bytes)?;
        file.sync_all()?;
        drop(file);
        #[cfg(windows)]
        {
            use std::os::windows::ffi::OsStrExt;
            use windows_sys::Win32::Storage::FileSystem::{
                MoveFileExW, MOVEFILE_REPLACE_EXISTING, MOVEFILE_WRITE_THROUGH,
            };
            // Rust file I/O supports long paths, but this direct Win32 call
            // needs the canonical extended prefix for the same directory.
            let canonical = parent.canonicalize()?;
            let from_path =
                canonical.join(temporary.file_name().context("Missing temporary name")?);
            let to_path = canonical.join(path.file_name().context("Missing storage filename")?);
            let from: Vec<u16> = from_path.as_os_str().encode_wide().chain(Some(0)).collect();
            let to: Vec<u16> = to_path.as_os_str().encode_wide().chain(Some(0)).collect();
            if unsafe {
                MoveFileExW(
                    from.as_ptr(),
                    to.as_ptr(),
                    MOVEFILE_REPLACE_EXISTING | MOVEFILE_WRITE_THROUGH,
                )
            } == 0
            {
                return Err(std::io::Error::last_os_error().into());
            }
        }
        #[cfg(not(windows))]
        {
            fs::rename(&temporary, path)?;
            File::open(parent)?.sync_all()?;
        }
        Ok(())
    })();
    if result.is_err() {
        let _ = fs::remove_file(&temporary);
    }
    result.with_context(|| format!("Could not replace {}", path.display()))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[cfg(windows)]
    #[test]
    fn atomic_replace_supports_long_hook_profile_paths() {
        let root = std::env::temp_dir().join(format!("switcher-long-{}", uuid::Uuid::new_v4()));
        let dir = root
            .join("a".repeat(90))
            .join("b".repeat(90))
            .join("cache-state");
        fs::create_dir_all(&dir).unwrap();
        let path = dir.join("ledger.json");
        assert!(path.as_os_str().len() > 260);
        atomic_write(&path, br#"{"generation":1}"#).unwrap();
        atomic_write(&path, br#"{"generation":2}"#).unwrap();
        assert_eq!(fs::read(&path).unwrap(), br#"{"generation":2}"#);
        assert_eq!(
            fs::read(backup_path(&path)).unwrap(),
            br#"{"generation":1}"#
        );
        fs::remove_dir_all(root.canonicalize().unwrap()).unwrap();
    }
    #[test]
    fn deleted_primary_does_not_restore_logged_out_credentials() {
        let dir = std::env::temp_dir().join(format!("switcher-logout-{}", uuid::Uuid::new_v4()));
        let path = dir.join("auth.json");
        atomic_write(&path, br#"{"generation":1}"#).unwrap();
        atomic_write(&path, br#"{"generation":2}"#).unwrap();
        fs::remove_file(&path).unwrap();
        assert!(read_json::<serde_json::Value>(&path).unwrap().is_none());
        fs::remove_dir_all(dir).unwrap();
    }
    #[test]
    fn interrupted_primary_recovers_previous_valid_json() {
        let dir = std::env::temp_dir().join(format!("switcher-atomic-{}", uuid::Uuid::new_v4()));
        let path = dir.join("accounts.json");
        atomic_write(&path, br#"{"generation":1}"#).unwrap();
        atomic_write(&path, br#"{"generation":2}"#).unwrap();
        fs::write(&path, b"\0\0").unwrap();
        let recovered = read_json::<serde_json::Value>(&path).unwrap().unwrap();
        assert_eq!(recovered["generation"], 1);
        assert_eq!(
            read_json::<serde_json::Value>(&path).unwrap().unwrap(),
            recovered
        );
        fs::remove_dir_all(dir).unwrap();
    }
}
