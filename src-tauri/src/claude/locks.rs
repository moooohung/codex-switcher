//! Claude Code / cswap `proper-lockfile` directory locks.
//!
//! Hold credentials locks across the complete read/refresh/write operation.
//! Usage GET requests do not need these locks. These are blocking APIs: async
//! callers should acquire them in `tokio::task::spawn_blocking`.
//!
//! Credentials: `<dir>/.oauth_refresh.lock`, then the sibling `<dir>.lock`;
//! stale after 60 seconds. Config: `<config>.lock`, stale after 10 seconds.
//! Like cswap, the default nine-second wait budget applies to EACH lock, so
//! acquiring both credentials locks can wait approximately eighteen seconds.
//! An interruptible worker updates each directory's mtime every three seconds.

use anyhow::{Context, Result};
use std::{
    fs::{self, File},
    io,
    path::{Path, PathBuf},
    sync::{mpsc, Arc},
    thread::{self, JoinHandle},
    time::{Duration, Instant, SystemTime},
};

pub const DEFAULT_TIMEOUT: Duration = Duration::from_secs(9);
const CREDENTIALS_STALENESS: Duration = Duration::from_secs(60);
const CONFIG_STALENESS: Duration = Duration::from_secs(10);
const HEARTBEAT_INTERVAL: Duration = Duration::from_secs(3);
const RETRY_INTERVAL: Duration = Duration::from_millis(100);

#[derive(Clone, Copy)]
struct Timing {
    timeout: Duration,
    stale: Duration,
    heartbeat: Duration,
    retry: Duration,
}

impl Timing {
    fn new(timeout: Duration, stale: Duration) -> Self {
        Self {
            timeout,
            stale,
            heartbeat: HEARTBEAT_INTERVAL,
            retry: RETRY_INTERVAL,
        }
    }
}

/// Holds both refresh locks in Claude Code's primary-then-legacy order.
#[derive(Debug)]
pub struct CredentialsGuard {
    // Rust drops fields in declaration order: release legacy before primary.
    _legacy: DirectoryGuard,
    _primary: DirectoryGuard,
}

impl CredentialsGuard {
    pub fn acquire(dir: &Path) -> Result<Self> {
        Self::acquire_with_timeout(dir, DEFAULT_TIMEOUT)
    }

    /// `timeout` is a per-lock budget, matching cswap's dual-lock acquisition.
    pub fn acquire_with_timeout(dir: &Path, timeout: Duration) -> Result<Self> {
        Self::acquire_timed(dir, Timing::new(timeout, CREDENTIALS_STALENESS))
    }

    fn acquire_timed(dir: &Path, timing: Timing) -> Result<Self> {
        let legacy = sibling_lock_path(dir)?;
        let primary = DirectoryGuard::acquire(&dir.join(".oauth_refresh.lock"), timing)?;
        let legacy = DirectoryGuard::acquire(&legacy, timing)?;
        Ok(Self {
            _legacy: legacy,
            _primary: primary,
        })
    }
}

/// Guards a global configuration write; the config file need not exist yet.
#[derive(Debug)]
pub struct ConfigGuard {
    _lock: DirectoryGuard,
}

impl ConfigGuard {
    pub fn acquire(config_path: &Path) -> Result<Self> {
        Self::acquire_with_timeout(config_path, DEFAULT_TIMEOUT)
    }

    pub fn acquire_with_timeout(config_path: &Path, timeout: Duration) -> Result<Self> {
        Self::acquire_timed(config_path, Timing::new(timeout, CONFIG_STALENESS))
    }

    fn acquire_timed(config_path: &Path, timing: Timing) -> Result<Self> {
        Ok(Self {
            _lock: DirectoryGuard::acquire(&sibling_lock_path(config_path)?, timing)?,
        })
    }
}

fn sibling_lock_path(path: &Path) -> Result<PathBuf> {
    let mut name = path
        .file_name()
        .with_context(|| format!("Lock target has no file name: {}", path.display()))?
        .to_os_string();
    name.push(".lock");
    Ok(path.with_file_name(name))
}

#[derive(Debug)]
struct DirectoryGuard {
    directory: Arc<Directory>,
    stop: Option<mpsc::Sender<()>>,
    heartbeat: Option<JoinHandle<()>>,
}

impl DirectoryGuard {
    fn acquire(path: &Path, timing: Timing) -> Result<Self> {
        let parent = path.parent().filter(|p| !p.as_os_str().is_empty());
        if let Some(parent) = parent {
            fs::create_dir_all(parent)
                .with_context(|| format!("Create lock parent {}", parent.display()))?;
        }
        let start = Instant::now();
        loop {
            // A zero budget still permits one immediate mkdir attempt.
            match fs::create_dir(path) {
                Ok(()) => {
                    let directory = Arc::new(
                        Directory::open(path)
                            .with_context(|| format!("Open acquired lock {}", path.display()))?,
                    );
                    let mut guard = Self {
                        directory,
                        stop: None,
                        heartbeat: None,
                    };
                    // Verify timestamp updates work before reporting acquisition.
                    guard.directory.touch()?;
                    let (stop, receiver) = mpsc::channel();
                    let directory = Arc::clone(&guard.directory);
                    let heartbeat = thread::Builder::new()
                        .name("claude-lock-heartbeat".into())
                        .spawn(move || {
                            while matches!(
                                receiver.recv_timeout(timing.heartbeat),
                                Err(mpsc::RecvTimeoutError::Timeout)
                            ) {
                                if directory.touch().is_err() {
                                    break;
                                }
                            }
                        })
                        .with_context(|| format!("Start lock heartbeat {}", path.display()))?;
                    guard.stop = Some(stop);
                    guard.heartbeat = Some(heartbeat);
                    return Ok(guard);
                }
                Err(error) if error.kind() == io::ErrorKind::AlreadyExists => {}
                Err(error) => {
                    return Err(error).with_context(|| format!("Acquire lock {}", path.display()));
                }
            }

            if start.elapsed() >= timing.timeout {
                return Err(timeout_error(path, timing.timeout).into());
            }
            // Only empty ordinary directories are candidates. A file, symlink,
            // unreadable directory or changed mtime is never forcibly removed.
            if let Ok(directory) = Directory::open(path) {
                if let Ok(modified) = directory.file.metadata().and_then(|m| m.modified()) {
                    if is_stale(modified, timing.stale) {
                        let _ = directory.remove_if_unchanged(Some((modified, timing.stale)));
                    }
                }
            }
            let remaining = timing.timeout.saturating_sub(start.elapsed());
            if remaining.is_zero() {
                return Err(timeout_error(path, timing.timeout).into());
            }
            // Always sleep after contention, including removal/metadata races.
            thread::sleep(timing.retry.min(remaining));
            if start.elapsed() >= timing.timeout {
                return Err(timeout_error(path, timing.timeout).into());
            }
        }
    }
}

impl Drop for DirectoryGuard {
    fn drop(&mut self) {
        if let Some(stop) = self.stop.take() {
            let _ = stop.send(());
        }
        if let Some(heartbeat) = self.heartbeat.take() {
            let _ = heartbeat.join();
        }
        // No worker can touch the directory after cleanup starts. Refuse to
        // delete a replacement or any directory containing unexpected data.
        let _ = self.directory.remove_if_unchanged(None);
    }
}

fn timeout_error(path: &Path, timeout: Duration) -> io::Error {
    io::Error::new(
        io::ErrorKind::TimedOut,
        format!(
            "Timed out after {timeout:?} acquiring Claude Code lock {}; retry after its refresh/write completes",
            path.display()
        ),
    )
}

fn is_stale(modified: SystemTime, stale: Duration) -> bool {
    SystemTime::now()
        .duration_since(modified)
        .is_ok_and(|age| age > stale)
}

#[derive(Debug)]
struct Directory {
    path: PathBuf,
    file: File,
    identity: platform::Identity,
}

impl Directory {
    fn open(path: &Path) -> io::Result<Self> {
        let metadata = fs::symlink_metadata(path)?;
        if !metadata.is_dir() || metadata.file_type().is_symlink() {
            return Err(io::Error::new(
                io::ErrorKind::InvalidInput,
                "Claude Code lock must be an ordinary directory",
            ));
        }
        let file = platform::open_directory(path)?;
        let identity = platform::identity(&file)?;
        Ok(Self {
            path: path.to_path_buf(),
            file,
            identity,
        })
    }

    fn still_owned(&self) -> io::Result<bool> {
        let current = Self::open(&self.path)?;
        Ok(current.identity == self.identity)
    }

    fn touch(&self) -> io::Result<()> {
        if !self.still_owned()? {
            return Err(io::Error::new(io::ErrorKind::NotFound, "Lock was replaced"));
        }
        // Update the acquired object, never a newly opened path after the check.
        self.file.set_modified(SystemTime::now())
    }

    fn remove_if_unchanged(&self, stale: Option<(SystemTime, Duration)>) -> io::Result<bool> {
        if !self.still_owned()? || fs::read_dir(&self.path)?.next().is_some() {
            return Ok(false);
        }
        // Reopen after the emptiness check to detect a path replacement, and
        // re-read mtime after it to detect a holder's intervening heartbeat.
        let current = Self::open(&self.path)?;
        if current.identity != self.identity {
            return Ok(false);
        }
        if let Some((observed, threshold)) = stale {
            let modified = current.file.metadata()?.modified()?;
            if modified != observed || !is_stale(modified, threshold) {
                return Ok(false);
            }
        }
        platform::remove_empty(&current.file, &self.path)?;
        Ok(true)
    }
}

#[cfg(windows)]
mod platform {
    use std::{
        fs::{File, OpenOptions},
        io,
        os::windows::{fs::OpenOptionsExt, io::AsRawHandle},
        path::Path,
    };
    use windows_sys::Win32::Storage::FileSystem::{
        FileDispositionInfoEx, GetFileInformationByHandle, SetFileInformationByHandle,
        BY_HANDLE_FILE_INFORMATION, DELETE, FILE_ATTRIBUTE_DIRECTORY, FILE_ATTRIBUTE_REPARSE_POINT,
        FILE_DISPOSITION_FLAG_DELETE, FILE_DISPOSITION_FLAG_POSIX_SEMANTICS,
        FILE_DISPOSITION_INFO_EX, FILE_FLAG_BACKUP_SEMANTICS, FILE_FLAG_OPEN_REPARSE_POINT,
        FILE_READ_ATTRIBUTES, FILE_SHARE_DELETE, FILE_SHARE_READ, FILE_SHARE_WRITE,
        FILE_WRITE_ATTRIBUTES,
    };

    #[derive(Debug, PartialEq, Eq)]
    pub(super) struct Identity {
        volume: u32,
        index: u64,
        created: u64,
    }

    pub(super) fn open_directory(path: &Path) -> io::Result<File> {
        OpenOptions::new()
            .access_mode(FILE_READ_ATTRIBUTES | FILE_WRITE_ATTRIBUTES | DELETE)
            .share_mode(FILE_SHARE_READ | FILE_SHARE_WRITE | FILE_SHARE_DELETE)
            .custom_flags(FILE_FLAG_BACKUP_SEMANTICS | FILE_FLAG_OPEN_REPARSE_POINT)
            .open(path)
    }

    pub(super) fn identity(file: &File) -> io::Result<Identity> {
        let mut info = BY_HANDLE_FILE_INFORMATION::default();
        // SAFETY: the File owns a valid live handle and info is a correctly sized
        // writable buffer. The call neither retains nor closes the handle.
        if unsafe { GetFileInformationByHandle(file.as_raw_handle(), &mut info) } == 0 {
            return Err(io::Error::last_os_error());
        }
        if info.dwFileAttributes & FILE_ATTRIBUTE_DIRECTORY == 0
            || info.dwFileAttributes & FILE_ATTRIBUTE_REPARSE_POINT != 0
        {
            return Err(io::Error::new(
                io::ErrorKind::InvalidInput,
                "Not an ordinary directory",
            ));
        }
        Ok(Identity {
            volume: info.dwVolumeSerialNumber,
            index: (u64::from(info.nFileIndexHigh) << 32) | u64::from(info.nFileIndexLow),
            created: (u64::from(info.ftCreationTime.dwHighDateTime) << 32)
                | u64::from(info.ftCreationTime.dwLowDateTime),
        })
    }

    pub(super) fn remove_empty(file: &File, _path: &Path) -> io::Result<()> {
        // Unlink this exact empty directory by handle. POSIX semantics allow a
        // stale takeover even if the expired holder still has a handle open;
        // a replacement at the same path cannot be deleted by this operation.
        let disposition = FILE_DISPOSITION_INFO_EX {
            Flags: FILE_DISPOSITION_FLAG_DELETE | FILE_DISPOSITION_FLAG_POSIX_SEMANTICS,
        };
        // SAFETY: a valid File handle, the documented info class and a correctly
        // sized immutable buffer; this operation never traverses child paths.
        if unsafe {
            SetFileInformationByHandle(
                file.as_raw_handle(),
                FileDispositionInfoEx,
                (&disposition as *const FILE_DISPOSITION_INFO_EX).cast(),
                std::mem::size_of::<FILE_DISPOSITION_INFO_EX>() as u32,
            )
        } == 0
        {
            return Err(io::Error::last_os_error());
        }
        Ok(())
    }
}

#[cfg(unix)]
mod platform {
    use std::{fs, fs::File, io, os::unix::fs::MetadataExt, path::Path};

    #[derive(Debug, PartialEq, Eq)]
    pub(super) struct Identity {
        device: u64,
        inode: u64,
    }

    pub(super) fn open_directory(path: &Path) -> io::Result<File> {
        File::open(path)
    }

    pub(super) fn identity(file: &File) -> io::Result<Identity> {
        let metadata = file.metadata()?;
        if !metadata.is_dir() {
            return Err(io::Error::new(
                io::ErrorKind::InvalidInput,
                "Not a directory",
            ));
        }
        Ok(Identity {
            device: metadata.dev(),
            inode: metadata.ino(),
        })
    }

    pub(super) fn remove_empty(file: &File, path: &Path) -> io::Result<()> {
        let metadata = fs::symlink_metadata(path)?;
        if !metadata.is_dir()
            || metadata.file_type().is_symlink()
            || identity(file)?
                != (Identity {
                    device: metadata.dev(),
                    inode: metadata.ino(),
                })
        {
            return Err(io::Error::new(io::ErrorKind::NotFound, "Lock was replaced"));
        }
        // rmdir is nonrecursive and atomically refuses a nonempty directory.
        fs::remove_dir(path)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::atomic::{AtomicU64, Ordering};

    static NEXT_TEMP: AtomicU64 = AtomicU64::new(0);

    struct TempDir(PathBuf);

    impl TempDir {
        fn new() -> Self {
            loop {
                let path = std::env::temp_dir().join(format!(
                    "claude-lock-test-{}-{}-{}",
                    std::process::id(),
                    SystemTime::now()
                        .duration_since(SystemTime::UNIX_EPOCH)
                        .unwrap()
                        .as_nanos(),
                    NEXT_TEMP.fetch_add(1, Ordering::Relaxed)
                ));
                match fs::create_dir(&path) {
                    Ok(()) => return Self(path),
                    Err(error) if error.kind() == io::ErrorKind::AlreadyExists => continue,
                    Err(error) => panic!("Create temporary directory: {error}"),
                }
            }
        }
    }

    impl Drop for TempDir {
        fn drop(&mut self) {
            // Tests remove only known files and empty directories, never recurse.
            let _ = fs::remove_dir(&self.0);
        }
    }

    fn timing() -> Timing {
        Timing {
            timeout: Duration::from_millis(180),
            stale: Duration::from_millis(500),
            heartbeat: Duration::from_millis(30),
            retry: Duration::from_millis(20),
        }
    }

    fn expire(path: &Path) {
        Directory::open(path)
            .unwrap()
            .file
            .set_modified(SystemTime::now() - Duration::from_secs(120))
            .unwrap();
    }

    fn assert_timeout(error: anyhow::Error) {
        assert_eq!(
            error.downcast_ref::<io::Error>().unwrap().kind(),
            io::ErrorKind::TimedOut
        );
    }

    #[test]
    fn paths_and_default_policy_match_cswap() {
        assert_eq!(DEFAULT_TIMEOUT, Duration::from_secs(9));
        assert_eq!(CREDENTIALS_STALENESS, Duration::from_secs(60));
        assert_eq!(CONFIG_STALENESS, Duration::from_secs(10));
        assert_eq!(HEARTBEAT_INTERVAL, Duration::from_secs(3));
        assert_eq!(
            sibling_lock_path(Path::new(".claude")).unwrap(),
            PathBuf::from(".claude.lock")
        );
        assert_eq!(
            sibling_lock_path(Path::new(".claude.json")).unwrap(),
            PathBuf::from(".claude.json.lock")
        );
    }

    #[test]
    fn contested_lock_times_out_without_removing_holder() {
        let temp = TempDir::new();
        let path = temp.0.join("held.lock");
        fs::create_dir(&path).unwrap();
        let start = Instant::now();
        assert_timeout(DirectoryGuard::acquire(&path, timing()).unwrap_err());
        assert!(start.elapsed() >= timing().timeout);
        assert!(start.elapsed() < Duration::from_secs(3));
        assert!(path.is_dir());
        fs::remove_dir(path).unwrap();
    }

    #[test]
    fn stale_empty_directory_is_taken_over() {
        let temp = TempDir::new();
        let path = temp.0.join("expired.lock");
        fs::create_dir(&path).unwrap();
        let old = Directory::open(&path).unwrap();
        expire(&path);
        let guard = DirectoryGuard::acquire(&path, timing()).unwrap();
        assert_ne!(old.identity, guard.directory.identity);
        assert!(!is_stale(
            fs::metadata(&path).unwrap().modified().unwrap(),
            timing().stale
        ));
        drop(guard);
        assert!(!path.exists());
    }

    #[test]
    fn stale_nonempty_directory_and_regular_file_are_preserved() {
        let temp = TempDir::new();
        let path = temp.0.join("nonempty.lock");
        fs::create_dir(&path).unwrap();
        let child = path.join("keep");
        fs::write(&child, b"keep").unwrap();
        expire(&path);
        assert_timeout(DirectoryGuard::acquire(&path, timing()).unwrap_err());
        assert_eq!(fs::read(&child).unwrap(), b"keep");
        fs::remove_file(child).unwrap();
        fs::remove_dir(&path).unwrap();
        fs::write(&path, b"keep file").unwrap();
        assert_timeout(DirectoryGuard::acquire(&path, timing()).unwrap_err());
        assert_eq!(fs::read(&path).unwrap(), b"keep file");
        fs::remove_file(path).unwrap();
    }

    #[test]
    fn heartbeat_after_stale_observation_prevents_removal() {
        let temp = TempDir::new();
        let path = temp.0.join("refresh.lock");
        fs::create_dir(&path).unwrap();
        expire(&path);
        let directory = Directory::open(&path).unwrap();
        let observed = directory.file.metadata().unwrap().modified().unwrap();
        directory.touch().unwrap();
        assert!(!directory
            .remove_if_unchanged(Some((observed, timing().stale)))
            .unwrap());
        assert!(path.exists());
        drop(directory);
        fs::remove_dir(path).unwrap();
    }

    #[test]
    fn credentials_acquire_primary_before_legacy_and_unwind_primary_on_timeout() {
        let temp = TempDir::new();
        let dir = temp.0.join("profile");
        let primary = dir.join(".oauth_refresh.lock");
        let legacy = sibling_lock_path(&dir).unwrap();
        fs::create_dir(&legacy).unwrap();
        let worker_dir = dir.clone();
        let worker = thread::spawn(move || CredentialsGuard::acquire_timed(&worker_dir, timing()));
        let start = Instant::now();
        while !primary.is_dir() && start.elapsed() < Duration::from_secs(1) {
            thread::sleep(Duration::from_millis(5));
        }
        assert!(
            primary.is_dir(),
            "primary must be held while legacy is contested"
        );
        assert_timeout(worker.join().unwrap().unwrap_err());
        assert!(!primary.exists());
        assert!(legacy.is_dir());
        fs::remove_dir(legacy).unwrap();
        fs::remove_dir(dir).unwrap();
    }

    #[test]
    fn contested_primary_does_not_create_legacy() {
        let temp = TempDir::new();
        let dir = temp.0.join("profile");
        let primary = dir.join(".oauth_refresh.lock");
        fs::create_dir_all(&primary).unwrap();
        assert_timeout(CredentialsGuard::acquire_timed(&dir, timing()).unwrap_err());
        assert!(!sibling_lock_path(&dir).unwrap().exists());
        fs::remove_dir(primary).unwrap();
        fs::remove_dir(dir).unwrap();
    }

    #[test]
    fn public_credentials_policy_preserves_a_twenty_second_old_primary() {
        let temp = TempDir::new();
        let dir = temp.0.join("profile");
        let primary = dir.join(".oauth_refresh.lock");
        fs::create_dir_all(&primary).unwrap();
        let held = Directory::open(&primary).unwrap();
        held.file
            .set_modified(SystemTime::now() - Duration::from_secs(20))
            .unwrap();
        assert_timeout(CredentialsGuard::acquire_with_timeout(&dir, timing().timeout).unwrap_err());
        assert!(held.still_owned().unwrap());
        assert!(!sibling_lock_path(&dir).unwrap().exists());
        drop(held);
        fs::remove_dir(primary).unwrap();
        fs::remove_dir(dir).unwrap();
    }

    #[test]
    fn public_config_policy_takes_over_a_twenty_second_old_lock() {
        let temp = TempDir::new();
        let config = temp.0.join("config.json");
        let path = sibling_lock_path(&config).unwrap();
        fs::create_dir(&path).unwrap();
        let old = Directory::open(&path).unwrap();
        old.file
            .set_modified(SystemTime::now() - Duration::from_secs(20))
            .unwrap();
        let guard = ConfigGuard::acquire_with_timeout(&config, timing().timeout).unwrap();
        assert_ne!(old.identity, guard._lock.directory.identity);
        drop(guard);
        assert!(!path.exists());
    }

    #[test]
    fn live_heartbeat_prevents_steal_beyond_stale_interval() {
        let temp = TempDir::new();
        let path = temp.0.join("live.lock");
        let guard = DirectoryGuard::acquire(&path, timing()).unwrap();
        let original = fs::metadata(&path).unwrap().modified().unwrap();
        thread::sleep(Duration::from_millis(650));
        assert!(fs::metadata(&path).unwrap().modified().unwrap() > original);
        assert_timeout(DirectoryGuard::acquire(&path, timing()).unwrap_err());
        assert!(guard.directory.still_owned().unwrap());
        drop(guard);
        assert!(!path.exists());
    }

    #[test]
    fn public_guards_create_missing_parents_and_cleanup_promptly() {
        let temp = TempDir::new();
        let parent = temp.0.join("missing");
        let dir = parent.join("profile");
        let credentials = CredentialsGuard::acquire(&dir).unwrap();
        assert!(dir.join(".oauth_refresh.lock").is_dir());
        assert!(sibling_lock_path(&dir).unwrap().is_dir());
        let config = dir.join("config.json");
        let config_guard = ConfigGuard::acquire(&config).unwrap();
        assert!(!config.exists());
        assert!(sibling_lock_path(&config).unwrap().is_dir());
        let start = Instant::now();
        drop(config_guard);
        drop(credentials);
        assert!(start.elapsed() < Duration::from_secs(1));
        assert!(!sibling_lock_path(&config).unwrap().exists());
        assert!(!dir.join(".oauth_refresh.lock").exists());
        assert!(!sibling_lock_path(&dir).unwrap().exists());
        fs::remove_dir(dir).unwrap();
        fs::remove_dir(parent).unwrap();
    }

    #[test]
    fn public_guard_heartbeat_updates_mtime_at_the_default_interval() {
        let temp = TempDir::new();
        let config = temp.0.join("config.json");
        let path = sibling_lock_path(&config).unwrap();
        let guard = ConfigGuard::acquire(&config).unwrap();
        let original = fs::metadata(&path).unwrap().modified().unwrap();
        let start = Instant::now();
        while fs::metadata(&path).unwrap().modified().unwrap() <= original {
            assert!(start.elapsed() < HEARTBEAT_INTERVAL + Duration::from_secs(2));
            thread::sleep(Duration::from_millis(20));
        }
        assert!(start.elapsed() >= HEARTBEAT_INTERVAL);
        drop(guard);
        assert!(!path.exists());
    }

    #[test]
    fn waiter_acquires_after_holder_releases() {
        let temp = TempDir::new();
        let path = temp.0.join("wait.lock");
        let holder = DirectoryGuard::acquire(&path, timing()).unwrap();
        let waiter_path = path.clone();
        let waiter = thread::spawn(move || DirectoryGuard::acquire(&waiter_path, timing()));
        thread::sleep(Duration::from_millis(60));
        drop(holder);
        let next = waiter.join().unwrap().unwrap();
        assert!(next.directory.still_owned().unwrap());
        drop(next);
        assert!(!path.exists());
    }

    #[test]
    fn replacement_is_neither_heartbeated_nor_deleted_by_old_guard() {
        let temp = TempDir::new();
        let path = temp.0.join("owned.lock");
        let moved = temp.0.join("old.lock");
        let guard = DirectoryGuard::acquire(&path, timing()).unwrap();
        fs::rename(&path, &moved).unwrap();
        fs::create_dir(&path).unwrap();
        expire(&path);
        let replacement_mtime = fs::metadata(&path).unwrap().modified().unwrap();
        thread::sleep(Duration::from_millis(100));
        assert_eq!(
            fs::metadata(&path).unwrap().modified().unwrap(),
            replacement_mtime
        );
        drop(guard);
        assert!(path.is_dir());
        assert_eq!(
            fs::metadata(&path).unwrap().modified().unwrap(),
            replacement_mtime
        );
        fs::remove_dir(path).unwrap();
        fs::remove_dir(moved).unwrap();
    }

    #[test]
    fn replaced_stale_candidate_is_not_deleted() {
        let temp = TempDir::new();
        let path = temp.0.join("stale.lock");
        let moved = temp.0.join("old.lock");
        fs::create_dir(&path).unwrap();
        expire(&path);
        let old = Directory::open(&path).unwrap();
        let observed = old.file.metadata().unwrap().modified().unwrap();
        fs::rename(&path, &moved).unwrap();
        fs::create_dir(&path).unwrap();
        expire(&path);
        assert!(!old
            .remove_if_unchanged(Some((observed, timing().stale)))
            .unwrap());
        assert!(path.is_dir());
        drop(old);
        fs::remove_dir(path).unwrap();
        fs::remove_dir(moved).unwrap();
    }

    #[test]
    fn drop_preserves_unexpected_contents() {
        let temp = TempDir::new();
        let path = temp.0.join("owned.lock");
        let guard = DirectoryGuard::acquire(&path, timing()).unwrap();
        let child = path.join("keep");
        fs::write(&child, b"keep").unwrap();
        drop(guard);
        assert_eq!(fs::read(&child).unwrap(), b"keep");
        fs::remove_file(child).unwrap();
        fs::remove_dir(path).unwrap();
    }

    #[test]
    fn zero_timeout_allows_immediate_acquisition_but_never_waits() {
        let temp = TempDir::new();
        let path = temp.0.join("immediate.lock");
        let zero = Timing {
            timeout: Duration::ZERO,
            ..timing()
        };
        let guard = DirectoryGuard::acquire(&path, zero).unwrap();
        assert_timeout(DirectoryGuard::acquire(&path, zero).unwrap_err());
        drop(guard);
    }
}
