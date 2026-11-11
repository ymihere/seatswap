//! Claude Code's lock folders and seatswap's own `switch.lock` (spec §2.6, §6.2).
//!
//! Claude Code's locks are folders: `mkdir` takes the lock, the holder keeps the
//! folder's mtime fresh, and a folder older than its limit counts as abandoned.
//! This module does the same, plus an advisory file lock that keeps two
//! seatswap processes apart.

use std::fs::{self, File, OpenOptions};
use std::io;
use std::os::unix::fs::{DirBuilderExt, OpenOptionsExt, PermissionsExt};
use std::path::{Path, PathBuf};
use std::sync::mpsc;
use std::thread::{self, JoinHandle};
use std::time::{Duration, Instant};

use filetime::FileTime;
use fs4::fs_std::FileExt;

use crate::paths::{AppPaths, ClaudePaths};

/// How often a held lock folder's mtime is set to now.
pub const HEARTBEAT: Duration = Duration::from_secs(2);

/// How long to wait between two attempts at a busy lock.
const POLL: Duration = Duration::from_millis(50);

/// One Claude Code lock folder: where it is and when it counts as abandoned.
#[derive(Debug, Clone)]
pub struct LockSpec {
    pub path: PathBuf,
    pub stale_after: Duration,
}

/// A held lock folder. A background thread sets its mtime to now every
/// [`HEARTBEAT`]. Drop stops the thread and removes the folder (a folder that
/// is already gone is fine).
#[derive(Debug)]
pub struct DirLock {
    path: PathBuf,
    stop: Option<mpsc::Sender<()>>,
    heartbeat: Option<JoinHandle<()>>,
}

impl DirLock {
    pub fn path(&self) -> &Path {
        &self.path
    }

    /// Start the heartbeat for a folder that was just created with `mkdir`.
    fn start(path: PathBuf) -> io::Result<DirLock> {
        let (stop, receiver) = mpsc::channel();
        let watched = path.clone();
        let heartbeat = thread::Builder::new()
            .name("seatswap-lock-heartbeat".to_string())
            .spawn(move || {
                // Ok(()) (drop asked us to stop) and Disconnected both end the thread;
                // only a timeout means it is time to refresh the folder's mtime.
                while let Err(mpsc::RecvTimeoutError::Timeout) = receiver.recv_timeout(HEARTBEAT) {
                    let _ = filetime::set_file_mtime(&watched, FileTime::now());
                }
            })?;
        Ok(DirLock {
            path,
            stop: Some(stop),
            heartbeat: Some(heartbeat),
        })
    }
}

impl Drop for DirLock {
    fn drop(&mut self) {
        // Wake the thread immediately instead of waiting out a HEARTBEAT.
        if let Some(stop) = self.stop.take() {
            let _ = stop.send(());
        }
        if let Some(heartbeat) = self.heartbeat.take() {
            let _ = heartbeat.join();
        }
        let _ = fs::remove_dir(&self.path);
    }
}

#[derive(Debug, thiserror::Error)]
pub enum LockError {
    #[error("{path} is held by another process")]
    Busy { path: PathBuf },
    #[error("lock {path}: {source}")]
    Io {
        path: PathBuf,
        #[source]
        source: std::io::Error,
    },
}

/// mkdir the folder. If it exists: when its mtime is older than `stale_after`,
/// remove it (rmdir) and try again; otherwise sleep ~50 ms and try again until
/// `deadline`, then [`LockError::Busy`]. The parent folder must exist (no
/// `create_dir_all`: a missing config folder is an [`LockError::Io`] error).
pub fn acquire_dir_lock(spec: &LockSpec, deadline: Instant) -> Result<DirLock, LockError> {
    loop {
        match fs::create_dir(&spec.path) {
            Ok(()) => {
                return DirLock::start(spec.path.clone()).map_err(|source| {
                    // Do not leave a folder behind that nobody refreshes.
                    let _ = fs::remove_dir(&spec.path);
                    LockError::Io {
                        path: spec.path.clone(),
                        source,
                    }
                });
            }
            Err(e) if e.kind() == io::ErrorKind::AlreadyExists => {
                let mut removed = false;
                match fs::metadata(&spec.path) {
                    // Gone between mkdir and stat: just try mkdir again.
                    Err(e) if e.kind() == io::ErrorKind::NotFound => removed = true,
                    Err(source) => {
                        return Err(LockError::Io {
                            path: spec.path.clone(),
                            source,
                        });
                    }
                    Ok(metadata) => {
                        let mtime = FileTime::from_last_modification_time(&metadata);
                        if is_stale(mtime, spec.stale_after) {
                            // An rmdir that fails (not empty, already gone) only means
                            // someone else got there first: fall through and retry.
                            removed = fs::remove_dir(&spec.path).is_ok();
                        }
                    }
                }
                if removed {
                    continue;
                }
            }
            Err(source) => {
                return Err(LockError::Io {
                    path: spec.path.clone(),
                    source,
                })
            }
        }
        if Instant::now() >= deadline {
            return Err(LockError::Busy {
                path: spec.path.clone(),
            });
        }
        thread::sleep(POLL);
    }
}

/// True when `mtime` is further in the past than `stale_after`. An mtime in the
/// future (clock skew) is never stale.
fn is_stale(mtime: FileTime, stale_after: Duration) -> bool {
    let age = file_time_ms(FileTime::now()) - file_time_ms(mtime);
    age > 0 && (age as u128) > stale_after.as_millis()
}

fn file_time_ms(time: FileTime) -> i128 {
    i128::from(time.unix_seconds()) * 1000 + i128::from(time.nanoseconds()) / 1_000_000
}

/// The four specs in the order Claude Code takes them:
/// refresh_lock 60 s, legacy_refresh_lock 60 s, storage_write_lock 15 s,
/// identity_lock 10 s.
pub fn claude_lock_specs(paths: &ClaudePaths) -> [LockSpec; 4] {
    [
        LockSpec {
            path: paths.refresh_lock(),
            stale_after: Duration::from_secs(60),
        },
        LockSpec {
            path: paths.legacy_refresh_lock(),
            stale_after: Duration::from_secs(60),
        },
        LockSpec {
            path: paths.storage_write_lock(),
            stale_after: Duration::from_secs(15),
        },
        LockSpec {
            path: paths.identity_lock(),
            stale_after: Duration::from_secs(10),
        },
    ]
}

/// All four, in that order, sharing one deadline (now + timeout). On failure the
/// ones already taken are released before returning the error. Dropping
/// [`ClaudeLocks`] releases them in reverse order.
pub struct ClaudeLocks {
    locks: Vec<DirLock>,
}

impl Drop for ClaudeLocks {
    fn drop(&mut self) {
        while let Some(lock) = self.locks.pop() {
            drop(lock);
        }
    }
}

pub fn acquire_claude_locks(
    paths: &ClaudePaths,
    timeout: Duration,
) -> Result<ClaudeLocks, LockError> {
    let deadline = Instant::now() + timeout;
    let mut locks = Vec::with_capacity(4);
    for spec in claude_lock_specs(paths) {
        match acquire_dir_lock(&spec, deadline) {
            Ok(lock) => locks.push(lock),
            Err(e) => {
                // Dropping the Vec releases what we already hold.
                drop(locks);
                return Err(e);
            }
        }
    }
    Ok(ClaudeLocks { locks })
}

/// True when any of the four exists and is not stale. The watcher reads only
/// when false.
pub fn claude_locks_busy(paths: &ClaudePaths) -> bool {
    claude_lock_specs(paths)
        .iter()
        .any(|spec| match fs::metadata(&spec.path) {
            Ok(metadata) => !is_stale(
                FileTime::from_last_modification_time(&metadata),
                spec.stale_after,
            ),
            Err(_) => false,
        })
}

/// An exclusive advisory file lock (`fs4`). The file is created when missing;
/// its folder must exist. `try_lock` is polled until `timeout`, then
/// [`LockError::Busy`]. A timeout of zero tries exactly once.
#[derive(Debug)]
pub struct FileLock {
    file: File,
}

impl Drop for FileLock {
    fn drop(&mut self) {
        // Fully qualified: `File::unlock` (std, Rust 1.89+) would shadow the fs4
        // trait method, and the workspace's MSRV is 1.80.
        let _ = FileExt::unlock(&self.file);
    }
}

pub fn acquire_file_lock(path: &Path, timeout: Duration) -> Result<FileLock, LockError> {
    // A lock file in a folder that does not exist yet (the GUI's data folder on
    // first start) needs its folder, like the app lock always did.
    if let Some(dir) = path.parent().filter(|dir| !dir.as_os_str().is_empty()) {
        ensure_data_dir(dir)?;
    }
    let file = OpenOptions::new()
        .read(true)
        .write(true)
        .create(true)
        .truncate(false)
        .mode(0o600)
        .open(path)
        .map_err(|source| LockError::Io {
            path: path.to_path_buf(),
            source,
        })?;
    let deadline = Instant::now() + timeout;
    loop {
        match file.try_lock_exclusive() {
            Ok(true) => return Ok(FileLock { file }),
            Ok(false) => {
                if Instant::now() >= deadline {
                    return Err(LockError::Busy {
                        path: path.to_path_buf(),
                    });
                }
                thread::sleep(POLL);
            }
            Err(source) => {
                return Err(LockError::Io {
                    path: path.to_path_buf(),
                    source,
                })
            }
        }
    }
}

/// seatswap's own exclusive lock on `<data_dir>/switch.lock`: a thin call
/// of [`acquire_file_lock`].
pub fn acquire_app_lock(app: &AppPaths, timeout: Duration) -> Result<FileLock, LockError> {
    acquire_file_lock(&app.switch_lock(), timeout)
}

/// Create the data folder with mode 0700 when it does not exist (and leave an
/// existing one's mode alone).
fn ensure_data_dir(dir: &Path) -> Result<(), LockError> {
    match fs::metadata(dir) {
        Ok(metadata) if metadata.is_dir() => return Ok(()),
        Ok(_) => {
            return Err(LockError::Io {
                path: dir.to_path_buf(),
                source: io::Error::new(io::ErrorKind::AlreadyExists, "not a folder"),
            });
        }
        Err(e) if e.kind() == io::ErrorKind::NotFound => {}
        Err(source) => {
            return Err(LockError::Io {
                path: dir.to_path_buf(),
                source,
            })
        }
    }
    fs::DirBuilder::new()
        .recursive(true)
        .mode(0o700)
        .create(dir)
        .map_err(|source| LockError::Io {
            path: dir.to_path_buf(),
            source,
        })?;
    // DirBuilder's mode is filtered by the umask; the data folder holds secrets
    // when the keyring is unavailable, so set it explicitly.
    fs::set_permissions(dir, fs::Permissions::from_mode(0o700)).map_err(|source| LockError::Io {
        path: dir.to_path_buf(),
        source,
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::os::unix::fs::PermissionsExt;
    use std::time::SystemTime;

    fn lock_spec(path: PathBuf, stale_after_secs: u64) -> LockSpec {
        LockSpec {
            path,
            stale_after: Duration::from_secs(stale_after_secs),
        }
    }

    fn backdate(path: &Path, secs: u64) {
        let when = SystemTime::now() - Duration::from_secs(secs);
        filetime::set_file_mtime(path, FileTime::from_system_time(when)).unwrap();
    }

    fn assert_busy(result: Result<DirLock, LockError>, path: &Path) {
        match result {
            Err(LockError::Busy { path: busy }) => assert_eq!(busy, path.to_path_buf()),
            Err(e) => panic!("expected Busy for {}, got {e}", path.display()),
            Ok(_) => panic!("expected Busy for {}, got a lock", path.display()),
        }
    }

    #[test]
    fn second_acquire_is_busy_and_drop_releases() {
        let tmp = tempfile::tempdir().unwrap();
        let path = tmp.path().join("lock");
        let spec = lock_spec(path.clone(), 60);
        let held = acquire_dir_lock(&spec, Instant::now() + Duration::from_secs(1)).unwrap();
        assert_eq!(held.path(), path);
        assert!(path.is_dir());
        assert_busy(
            acquire_dir_lock(&spec, Instant::now() + Duration::from_millis(150)),
            &path,
        );
        drop(held);
        assert!(!path.exists(), "drop must remove the lock folder");
        let again = acquire_dir_lock(&spec, Instant::now() + Duration::from_millis(500)).unwrap();
        drop(again);
    }

    #[test]
    fn heartbeat_keeps_the_mtime_fresh() {
        let tmp = tempfile::tempdir().unwrap();
        let path = tmp.path().join("lock");
        let spec = lock_spec(path.clone(), 60);
        let held = acquire_dir_lock(&spec, Instant::now() + Duration::from_secs(1)).unwrap();
        let acquired = SystemTime::now();
        thread::sleep(Duration::from_millis(2500));
        let mtime = fs::metadata(held.path()).unwrap().modified().unwrap();
        assert!(
            mtime > acquired,
            "mtime {mtime:?} was not refreshed after {acquired:?}"
        );
        drop(held);
    }

    #[test]
    fn stale_folder_is_taken_over() {
        let tmp = tempfile::tempdir().unwrap();
        let path = tmp.path().join("lock");
        fs::create_dir(&path).unwrap();
        backdate(&path, 120);
        let spec = lock_spec(path.clone(), 60);
        let held = acquire_dir_lock(&spec, Instant::now() + Duration::from_millis(500)).unwrap();
        assert_eq!(held.path(), path);
        drop(held);
        assert!(!path.exists());
    }

    #[test]
    fn fresh_foreign_folder_is_not_taken_over() {
        let tmp = tempfile::tempdir().unwrap();
        let path = tmp.path().join("lock");
        fs::create_dir(&path).unwrap();
        let spec = lock_spec(path.clone(), 60);
        assert_busy(
            acquire_dir_lock(&spec, Instant::now() + Duration::from_millis(150)),
            &path,
        );
        assert!(path.is_dir(), "a fresh foreign lock must survive");
    }

    #[test]
    fn folder_with_mtime_in_the_future_is_not_stale() {
        let tmp = tempfile::tempdir().unwrap();
        let path = tmp.path().join("lock");
        fs::create_dir(&path).unwrap();
        let when = SystemTime::now() + Duration::from_secs(300);
        filetime::set_file_mtime(&path, FileTime::from_system_time(when)).unwrap();
        let spec = lock_spec(path.clone(), 60);
        assert_busy(
            acquire_dir_lock(&spec, Instant::now() + Duration::from_millis(150)),
            &path,
        );
    }

    fn fake_paths(tmp: &tempfile::TempDir) -> ClaudePaths {
        let config = tmp.path().join("cfg");
        fs::create_dir(&config).unwrap();
        ClaudePaths::for_config_dir(&config, tmp.path())
    }

    #[test]
    fn claude_locks_take_all_four_in_order() {
        let tmp = tempfile::tempdir().unwrap();
        let paths = fake_paths(&tmp);
        let specs = claude_lock_specs(&paths);
        assert_eq!(specs[0].path, paths.refresh_lock());
        assert_eq!(specs[1].path, paths.legacy_refresh_lock());
        assert_eq!(specs[2].path, paths.storage_write_lock());
        assert_eq!(specs[3].path, paths.identity_lock());
        assert_eq!(
            specs
                .iter()
                .map(|s| s.stale_after.as_secs())
                .collect::<Vec<_>>(),
            vec![60, 60, 15, 10]
        );

        let held = acquire_claude_locks(&paths, Duration::from_secs(1)).unwrap();
        for spec in &specs {
            assert!(
                spec.path.is_dir(),
                "{} must exist while held",
                spec.path.display()
            );
        }
        drop(held);
        for spec in &specs {
            assert!(
                !spec.path.exists(),
                "{} must be gone after drop",
                spec.path.display()
            );
        }
    }

    #[test]
    fn busy_third_lock_releases_the_first_two() {
        let tmp = tempfile::tempdir().unwrap();
        let paths = fake_paths(&tmp);
        fs::create_dir(paths.storage_write_lock()).unwrap();
        let result = acquire_claude_locks(&paths, Duration::from_millis(150));
        match result {
            Err(LockError::Busy { path }) => assert_eq!(path, paths.storage_write_lock()),
            Err(e) => panic!("expected Busy for the storage lock, got {e}"),
            Ok(_) => panic!("expected Busy, got all locks"),
        }
        assert!(
            !paths.refresh_lock().exists(),
            "first lock must be released"
        );
        assert!(
            !paths.legacy_refresh_lock().exists(),
            "second lock must be released"
        );
        assert!(
            paths.storage_write_lock().is_dir(),
            "the busy lock is not ours to remove"
        );
    }

    #[test]
    fn busy_reports_true_false_and_stale() {
        let tmp = tempfile::tempdir().unwrap();
        let paths = fake_paths(&tmp);
        assert!(!claude_locks_busy(&paths));
        fs::create_dir(paths.refresh_lock()).unwrap();
        assert!(claude_locks_busy(&paths));
        backdate(&paths.refresh_lock(), 120);
        assert!(!claude_locks_busy(&paths));
        fs::create_dir(paths.identity_lock()).unwrap();
        assert!(claude_locks_busy(&paths));
    }

    #[test]
    fn file_lock_is_exclusive_and_frees_on_drop() {
        let tmp = tempfile::tempdir().unwrap();
        let path = tmp.path().join("data").join("gui.lock");
        let first = acquire_file_lock(&path, Duration::ZERO).unwrap();
        assert!(path.is_file());
        assert_eq!(
            fs::metadata(path.parent().unwrap())
                .unwrap()
                .permissions()
                .mode()
                & 0o777,
            0o700
        );
        match acquire_file_lock(&path, Duration::ZERO) {
            Err(LockError::Busy { path: busy }) => assert_eq!(busy, path),
            Err(e) => panic!("expected Busy, got {e}"),
            Ok(_) => panic!("the second FileLock must be Busy"),
        }
        drop(first);
        let again = acquire_file_lock(&path, Duration::ZERO).unwrap();
        drop(again);
    }

    #[test]
    fn file_lock_zero_timeout_tries_once() {
        let tmp = tempfile::tempdir().unwrap();
        let path = tmp.path().join("gui.lock");
        let first = acquire_file_lock(&path, Duration::ZERO).unwrap();
        let started = Instant::now();
        match acquire_file_lock(&path, Duration::ZERO) {
            Err(LockError::Busy { .. }) => {}
            Err(e) => panic!("expected Busy, got {e}"),
            Ok(_) => panic!("the second FileLock must be Busy"),
        }
        assert!(
            started.elapsed() < Duration::from_millis(500),
            "a zero timeout must not wait"
        );
        drop(first);
    }

    #[test]
    fn app_lock_excludes_a_second_acquire_and_frees_on_drop() {
        let tmp = tempfile::tempdir().unwrap();
        let app = AppPaths::at(&tmp.path().join("data"));
        let first = acquire_app_lock(&app, Duration::from_secs(1)).unwrap();
        assert!(app.switch_lock().is_file());
        assert_eq!(
            fs::metadata(&app.data_dir).unwrap().permissions().mode() & 0o777,
            0o700
        );
        match acquire_app_lock(&app, Duration::from_millis(150)) {
            Err(LockError::Busy { path }) => assert_eq!(path, app.switch_lock()),
            Err(e) => panic!("expected Busy, got {e}"),
            Ok(_) => panic!("the second FileLock must be Busy"),
        }
        drop(first);
        let again = acquire_app_lock(&app, Duration::from_millis(500)).unwrap();
        drop(again);
    }
}
