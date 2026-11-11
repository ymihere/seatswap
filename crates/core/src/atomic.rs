//! The one way seatswap writes a file, and the one module that knows about file
//! modes and how to make private files and folders.
//!
//! A write goes to a temp file in the same folder, is fsynced, renamed over the
//! target, and (Unix) the folder is fsynced. On Unix an existing file keeps its
//! mode and a new one gets the mode of `access_if_new`; on Windows a file gets
//! the permissions of its folder, and the rename is retried briefly while
//! another program holds the target open.

use std::fs::{self, File, OpenOptions};
use std::io::{self, Write};
#[cfg(unix)]
use std::os::unix::fs::{DirBuilderExt, OpenOptionsExt, PermissionsExt};
use std::path::{Path, PathBuf};
#[cfg(windows)]
use std::time::{Duration, Instant};

/// Who may read a file seatswap creates.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Access {
    /// Unix: mode 0600.
    Private,
    /// Unix: mode 0644.
    Shared,
}

#[cfg(unix)]
impl Access {
    fn mode(self) -> u32 {
        match self {
            Access::Private => 0o600,
            Access::Shared => 0o644,
        }
    }
}

/// How long a rename over a file another program holds open is retried (Windows).
#[cfg(windows)]
const RENAME_RETRY: Duration = Duration::from_secs(2);

/// Replace `path` with `bytes` atomically. A reader sees the old or the new
/// content, never a mix. The parent folder must exist. An existing file keeps
/// its mode (Unix); a new one is created with `access_if_new`.
pub fn write_atomic(path: &Path, bytes: &[u8], access_if_new: Access) -> io::Result<()> {
    #[cfg(unix)]
    let dir = path
        .parent()
        .filter(|d| !d.as_os_str().is_empty())
        .unwrap_or(Path::new("."));
    #[cfg(unix)]
    let mode = match fs::metadata(path) {
        Ok(m) => m.permissions().mode() & 0o7777,
        Err(e) if e.kind() == io::ErrorKind::NotFound => access_if_new.mode(),
        Err(e) => return Err(e),
    };
    // Windows has no file modes: the new file gets its folder's permissions.
    #[cfg(windows)]
    let _ = access_if_new;
    let tmp = temp_path(path);
    let result = (|| {
        let mut options = OpenOptions::new();
        options.write(true).create_new(true);
        #[cfg(unix)]
        options.mode(mode);
        let mut f = options.open(&tmp)?;
        // create_new honours the umask; set the mode explicitly.
        #[cfg(unix)]
        f.set_permissions(fs::Permissions::from_mode(mode))?;
        f.write_all(bytes)?;
        f.sync_all()?;
        drop(f);
        #[cfg(unix)]
        {
            fs::rename(&tmp, path)?;
            File::open(dir)?.sync_all()
        }
        // Windows refuses to open a folder as a file, so there is no folder sync.
        #[cfg(windows)]
        rename_with_retry(&tmp, path, RENAME_RETRY)
    })();
    if result.is_err() {
        let _ = fs::remove_file(&tmp);
    }
    result
}

/// `fs::rename`, retried while it fails with access denied (OS error 5) or a
/// sharing violation (OS error 32): another program holds the target open
/// without delete sharing. Waits 10 ms, doubling up to 200 ms, until `budget`
/// has passed since the first attempt; then the last error is returned as it is.
/// Any other error returns at once.
#[cfg(windows)]
fn rename_with_retry(from: &Path, to: &Path, budget: Duration) -> io::Result<()> {
    const FIRST_WAIT: Duration = Duration::from_millis(10);
    const LONGEST_WAIT: Duration = Duration::from_millis(200);
    let started = Instant::now();
    let mut wait = FIRST_WAIT;
    loop {
        match fs::rename(from, to) {
            Ok(()) => return Ok(()),
            Err(e) if matches!(e.raw_os_error(), Some(5 | 32)) => {
                let elapsed = started.elapsed();
                if elapsed >= budget {
                    return Err(e);
                }
                std::thread::sleep(wait.min(budget - elapsed));
                wait = (wait * 2).min(LONGEST_WAIT);
            }
            Err(e) => return Err(e),
        }
    }
}

/// Copy `path` to `<path>.seatswap-backup` (private) unless that backup
/// already exists (spec §13). Returns whether a backup was made. A missing
/// `path` makes no backup.
pub fn backup_once(path: &Path) -> io::Result<bool> {
    let backup = {
        let mut s = path.as_os_str().to_os_string();
        s.push(".seatswap-backup");
        PathBuf::from(s)
    };
    if backup.exists() || !path.exists() {
        return Ok(false);
    }
    let bytes = fs::read(path)?;
    write_atomic(&backup, &bytes, Access::Private)?;
    Ok(true)
}

/// Make sure `dir` exists as a folder only its owner can use. A missing folder
/// is created with its parents (Unix: mode 0700, set explicitly because the
/// umask filters the requested mode). An existing folder is left exactly as it
/// is. Something else in its place is an `AlreadyExists` error.
pub fn ensure_private_dir(dir: &Path) -> io::Result<()> {
    match fs::metadata(dir) {
        Ok(metadata) if metadata.is_dir() => return Ok(()),
        Ok(_) => {
            return Err(io::Error::new(
                io::ErrorKind::AlreadyExists,
                "exists but is not a folder",
            ))
        }
        Err(e) if e.kind() == io::ErrorKind::NotFound => {}
        Err(e) => return Err(e),
    }
    let mut builder = fs::DirBuilder::new();
    builder.recursive(true);
    #[cfg(unix)]
    builder.mode(0o700);
    builder.create(dir)?;
    restrict_dir(dir)
}

/// Tighten an existing folder to its owner. Unix: mode 0700. Windows: a folder
/// has its parent's permissions and there is nothing to set.
pub fn restrict_dir(dir: &Path) -> io::Result<()> {
    #[cfg(unix)]
    return fs::set_permissions(dir, fs::Permissions::from_mode(0o700));
    #[cfg(windows)]
    {
        let _ = dir;
        Ok(())
    }
}

/// Open `path` for reading and writing, creating it when missing, never
/// truncating. A file created here is private (Unix: mode 0600).
pub fn open_private(path: &Path) -> io::Result<File> {
    let mut options = OpenOptions::new();
    options.read(true).write(true).create(true).truncate(false);
    #[cfg(unix)]
    options.mode(0o600);
    options.open(path)
}

fn temp_path(path: &Path) -> PathBuf {
    let name = path
        .file_name()
        .map(|n| n.to_string_lossy().into_owned())
        .unwrap_or_default();
    let unique = uuid::Uuid::new_v4().simple().to_string();
    path.with_file_name(format!(
        ".{name}.seatswap-tmp-{}-{}",
        std::process::id(),
        &unique[..12]
    ))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn names_in(dir: &Path) -> Vec<String> {
        let mut names: Vec<String> = fs::read_dir(dir)
            .unwrap()
            .map(|entry| entry.unwrap().file_name().to_string_lossy().into_owned())
            .collect();
        names.sort();
        names
    }

    #[cfg(unix)]
    fn mode(path: &Path) -> u32 {
        fs::metadata(path).unwrap().permissions().mode() & 0o777
    }

    #[test]
    #[cfg(unix)]
    fn keeps_mode_of_existing_file_and_sets_new_mode() {
        let tmp = tempfile::tempdir().unwrap();
        let p = tmp.path().join("f.json");
        write_atomic(&p, b"one", Access::Private).unwrap();
        assert_eq!(
            fs::metadata(&p).unwrap().permissions().mode() & 0o777,
            0o600
        );
        fs::set_permissions(&p, fs::Permissions::from_mode(0o640)).unwrap();
        write_atomic(&p, b"two", Access::Private).unwrap();
        assert_eq!(fs::read(&p).unwrap(), b"two");
        assert_eq!(
            fs::metadata(&p).unwrap().permissions().mode() & 0o777,
            0o640
        );
        // no temp files left behind
        assert_eq!(fs::read_dir(tmp.path()).unwrap().count(), 1);
    }

    #[test]
    #[cfg(unix)]
    fn shared_access_creates_mode_0644() {
        let tmp = tempfile::tempdir().unwrap();
        let p = tmp.path().join("seatswap.desktop");
        write_atomic(&p, b"[Desktop Entry]\n", Access::Shared).unwrap();
        assert_eq!(mode(&p), 0o644);
    }

    #[test]
    fn backup_is_made_once() {
        let tmp = tempfile::tempdir().unwrap();
        let p = tmp.path().join(".credentials.json");
        assert!(!backup_once(&p).unwrap());
        fs::write(&p, b"first").unwrap();
        assert!(backup_once(&p).unwrap());
        fs::write(&p, b"second").unwrap();
        assert!(!backup_once(&p).unwrap());
        let b = tmp.path().join(".credentials.json.seatswap-backup");
        assert_eq!(fs::read(&b).unwrap(), b"first");
        #[cfg(unix)]
        assert_eq!(
            fs::metadata(&b).unwrap().permissions().mode() & 0o777,
            0o600
        );
    }

    #[test]
    fn new_file_has_the_content() {
        let tmp = tempfile::tempdir().unwrap();
        let p = tmp.path().join("accounts.json");
        write_atomic(&p, b"{\"version\":1}", Access::Private).unwrap();
        assert_eq!(fs::read(&p).unwrap(), b"{\"version\":1}");
        assert_eq!(names_in(tmp.path()), vec!["accounts.json"]);
    }

    #[test]
    fn replacing_keeps_no_temp_file() {
        let tmp = tempfile::tempdir().unwrap();
        let p = tmp.path().join(".credentials.json");
        fs::write(&p, b"old").unwrap();
        write_atomic(&p, b"new", Access::Private).unwrap();
        assert_eq!(fs::read(&p).unwrap(), b"new");
        assert_eq!(names_in(tmp.path()), vec![".credentials.json"]);
    }

    #[test]
    fn failed_write_leaves_no_temp_file() {
        let tmp = tempfile::tempdir().unwrap();
        let p = tmp.path().join("missing").join("accounts.json");
        let err = write_atomic(&p, b"{}", Access::Private).unwrap_err();
        assert_eq!(err.kind(), io::ErrorKind::NotFound);
        assert!(names_in(tmp.path()).is_empty());
    }

    #[test]
    fn ensure_private_dir_creates_nested_folders() {
        let tmp = tempfile::tempdir().unwrap();
        let dir = tmp.path().join("one").join("two").join("seatswap");
        ensure_private_dir(&dir).unwrap();
        assert!(dir.is_dir());
        #[cfg(unix)]
        assert_eq!(mode(&dir), 0o700);
    }

    #[test]
    fn ensure_private_dir_leaves_an_existing_folder_alone() {
        let tmp = tempfile::tempdir().unwrap();
        let dir = tmp.path().join("seatswap");
        fs::create_dir(&dir).unwrap();
        fs::write(dir.join("accounts.json"), b"{}").unwrap();
        #[cfg(unix)]
        fs::set_permissions(&dir, fs::Permissions::from_mode(0o755)).unwrap();
        ensure_private_dir(&dir).unwrap();
        assert_eq!(names_in(&dir), vec!["accounts.json"]);
        #[cfg(unix)]
        assert_eq!(mode(&dir), 0o755);
    }

    #[test]
    fn ensure_private_dir_rejects_a_file_in_its_place() {
        let tmp = tempfile::tempdir().unwrap();
        let dir = tmp.path().join("seatswap");
        fs::write(&dir, b"not a folder").unwrap();
        let err = ensure_private_dir(&dir).unwrap_err();
        assert_eq!(err.kind(), io::ErrorKind::AlreadyExists);
        assert_eq!(err.to_string(), "exists but is not a folder");
        assert!(dir.is_file());
    }

    #[test]
    #[cfg(unix)]
    fn restrict_dir_sets_0700() {
        let tmp = tempfile::tempdir().unwrap();
        let dir = tmp.path().join("secrets");
        fs::create_dir(&dir).unwrap();
        fs::set_permissions(&dir, fs::Permissions::from_mode(0o755)).unwrap();
        restrict_dir(&dir).unwrap();
        assert_eq!(mode(&dir), 0o700);
    }

    #[test]
    fn open_private_creates_and_never_truncates() {
        let tmp = tempfile::tempdir().unwrap();
        let p = tmp.path().join("switch.lock");
        drop(open_private(&p).unwrap());
        assert!(p.is_file());
        #[cfg(unix)]
        assert_eq!(mode(&p), 0o600);
        fs::write(&p, b"kept").unwrap();
        drop(open_private(&p).unwrap());
        assert_eq!(fs::read(&p).unwrap(), b"kept");
    }

    #[cfg(windows)]
    mod windows {
        use super::super::*;
        use super::names_in;
        use std::os::windows::fs::OpenOptionsExt;
        use std::sync::mpsc;
        use std::thread;

        /// FILE_SHARE_READ | FILE_SHARE_WRITE: no delete sharing.
        const SHARE_READ_WRITE: u32 = 1 | 2;
        /// FILE_SHARE_READ | FILE_SHARE_WRITE | FILE_SHARE_DELETE.
        const SHARE_READ_WRITE_DELETE: u32 = 1 | 2 | 4;

        /// Open `path` in another thread with `share_mode`, return once it is
        /// open, and keep it open for `hold` or until the returned sender fires.
        fn hold_open(
            path: &Path,
            share_mode: u32,
            hold: Duration,
        ) -> (thread::JoinHandle<()>, mpsc::Sender<()>) {
            let path = path.to_path_buf();
            let (opened_tx, opened_rx) = mpsc::channel();
            let (release_tx, release_rx) = mpsc::channel::<()>();
            let holder = thread::spawn(move || {
                let file = OpenOptions::new()
                    .read(true)
                    .share_mode(share_mode)
                    .open(&path)
                    .unwrap();
                opened_tx.send(()).unwrap();
                let _ = release_rx.recv_timeout(hold);
                drop(file);
            });
            opened_rx.recv().unwrap();
            (holder, release_tx)
        }

        #[test]
        fn write_waits_for_a_holder_without_delete_sharing() {
            let tmp = tempfile::tempdir().unwrap();
            let p = tmp.path().join(".credentials.json");
            fs::write(&p, b"old").unwrap();
            let (holder, _release) = hold_open(&p, SHARE_READ_WRITE, Duration::from_millis(300));
            let started = Instant::now();
            write_atomic(&p, b"new", Access::Private).unwrap();
            assert!(
                started.elapsed() >= Duration::from_millis(200),
                "the write must have waited for the holder, took {:?}",
                started.elapsed()
            );
            holder.join().unwrap();
            assert_eq!(fs::read(&p).unwrap(), b"new");
            assert_eq!(names_in(tmp.path()), vec![".credentials.json"]);
        }

        #[test]
        fn rename_gives_up_after_its_budget_with_the_os_error() {
            let tmp = tempfile::tempdir().unwrap();
            let p = tmp.path().join(".credentials.json");
            fs::write(&p, b"old").unwrap();
            let (holder, release) = hold_open(&p, SHARE_READ_WRITE, Duration::from_secs(60));
            let tmp_file = temp_path(&p);
            fs::write(&tmp_file, b"new").unwrap();
            let started = Instant::now();
            let err = rename_with_retry(&tmp_file, &p, Duration::from_millis(200)).unwrap_err();
            let took = started.elapsed();
            assert_eq!(err.raw_os_error(), Some(5), "{err}");
            assert!(
                took >= Duration::from_millis(200) && took < RENAME_RETRY,
                "the retry must stop at its budget, took {took:?}"
            );
            release.send(()).unwrap();
            holder.join().unwrap();
            assert_eq!(fs::read(&p).unwrap(), b"old");
        }

        #[test]
        fn failed_replace_keeps_the_old_content_and_no_temp_file() {
            let tmp = tempfile::tempdir().unwrap();
            let p = tmp.path().join(".credentials.json");
            fs::write(&p, b"old").unwrap();
            let (holder, release) = hold_open(&p, SHARE_READ_WRITE, Duration::from_secs(60));
            let err = write_atomic(&p, b"new", Access::Private).unwrap_err();
            assert_eq!(err.raw_os_error(), Some(5), "{err}");
            release.send(()).unwrap();
            holder.join().unwrap();
            assert_eq!(fs::read(&p).unwrap(), b"old");
            assert_eq!(names_in(tmp.path()), vec![".credentials.json"]);
        }

        #[test]
        fn write_over_a_holder_with_delete_sharing_succeeds_at_once() {
            let tmp = tempfile::tempdir().unwrap();
            let p = tmp.path().join(".credentials.json");
            fs::write(&p, b"old").unwrap();
            let (holder, release) = hold_open(&p, SHARE_READ_WRITE_DELETE, Duration::from_secs(60));
            let started = Instant::now();
            write_atomic(&p, b"new", Access::Private).unwrap();
            assert!(
                started.elapsed() < Duration::from_millis(100),
                "no retry expected, took {:?}",
                started.elapsed()
            );
            release.send(()).unwrap();
            holder.join().unwrap();
            assert_eq!(fs::read(&p).unwrap(), b"new");
        }
    }
}
