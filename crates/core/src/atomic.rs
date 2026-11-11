//! The one way seatswap writes a file: a temp file in the same folder,
//! fsync, rename over the target, fsync the folder. An existing file keeps its
//! mode; a new one gets `mode_if_new`.

use std::fs::{self, File, OpenOptions};
use std::io::{self, Write};
use std::os::unix::fs::{OpenOptionsExt, PermissionsExt};
use std::path::{Path, PathBuf};

/// Replace `path` with `bytes` atomically. A reader sees the old or the new
/// content, never a mix. The parent folder must exist.
pub fn write_atomic(path: &Path, bytes: &[u8], mode_if_new: u32) -> io::Result<()> {
    let dir = path
        .parent()
        .filter(|d| !d.as_os_str().is_empty())
        .unwrap_or(Path::new("."));
    let mode = match fs::metadata(path) {
        Ok(m) => m.permissions().mode() & 0o7777,
        Err(e) if e.kind() == io::ErrorKind::NotFound => mode_if_new,
        Err(e) => return Err(e),
    };
    let tmp = temp_path(path);
    let result = (|| {
        let mut f = OpenOptions::new()
            .write(true)
            .create_new(true)
            .mode(mode)
            .open(&tmp)?;
        // create_new honours the umask; set the mode explicitly.
        f.set_permissions(fs::Permissions::from_mode(mode))?;
        f.write_all(bytes)?;
        f.sync_all()?;
        drop(f);
        fs::rename(&tmp, path)?;
        File::open(dir)?.sync_all()
    })();
    if result.is_err() {
        let _ = fs::remove_file(&tmp);
    }
    result
}

/// Copy `path` to `<path>.seatswap-backup` (mode 0600) unless that backup
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
    write_atomic(&backup, &bytes, 0o600)?;
    fs::set_permissions(&backup, fs::Permissions::from_mode(0o600))?;
    Ok(true)
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

    #[test]
    fn keeps_mode_of_existing_file_and_sets_new_mode() {
        let tmp = tempfile::tempdir().unwrap();
        let p = tmp.path().join("f.json");
        write_atomic(&p, b"one", 0o600).unwrap();
        assert_eq!(
            fs::metadata(&p).unwrap().permissions().mode() & 0o777,
            0o600
        );
        fs::set_permissions(&p, fs::Permissions::from_mode(0o640)).unwrap();
        write_atomic(&p, b"two", 0o600).unwrap();
        assert_eq!(fs::read(&p).unwrap(), b"two");
        assert_eq!(
            fs::metadata(&p).unwrap().permissions().mode() & 0o777,
            0o640
        );
        // no temp files left behind
        assert_eq!(fs::read_dir(tmp.path()).unwrap().count(), 1);
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
        assert_eq!(
            fs::metadata(&b).unwrap().permissions().mode() & 0o777,
            0o600
        );
    }
}
