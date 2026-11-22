//! The Linux menu entry and its icon (spec R7, docs/implementation.md).
//!
//! At every start the tray app writes
//! `$XDG_DATA_HOME/applications/seatswap.desktop` and
//! `$XDG_DATA_HOME/icons/hicolor/256x256/apps/seatswap.png` through
//! [`seatswap_core::atomic::write_atomic`] when they are missing or differ. An
//! entry another program or the owner made — a file without a line exactly
//! `X-seatswap-managed=true` — is never overwritten, and neither is a symbolic
//! link at either path: such a file is not seatswap's own. [`Outcome`] names
//! what was written and what was left alone, so the caller prints one line per
//! left-alone path. The icon is seatswap's own file and is still written when
//! the entry is left alone.
//!
//! Windows and macOS have no such entry: there [`apply_at_start`] touches
//! nothing and returns an empty [`Outcome`] (a Mac finds the app by its bundle
//! in `/Applications`, and `~/.local/share` means nothing to it).

use std::path::PathBuf;

#[cfg(not(any(windows, target_os = "macos")))]
use std::fs;
#[cfg(not(any(windows, target_os = "macos")))]
use std::path::Path;

#[cfg(not(any(windows, target_os = "macos")))]
use seatswap_core::atomic::{self, Access};
#[cfg(not(any(windows, target_os = "macos")))]
use seatswap_core::paths::{app_icon, applications_entry};

#[cfg(not(any(windows, target_os = "macos")))]
use super::autostart::{exec_line, has_line, symlink_at, AutostartError};
#[cfg(not(any(windows, target_os = "macos")))]
use super::APP_ICON_PNG;

/// The line that marks an entry as seatswap's own. A file without it is left
/// alone.
#[cfg(not(any(windows, target_os = "macos")))]
const MANAGED_LINE: &[u8] = b"X-seatswap-managed=true";

/// What [`apply`] did with the entry and its icon: the paths it wrote and the
/// paths it left alone, each at most once. Both lists empty means there was
/// nothing to do. A left-alone icon and a written entry (or the reverse) are
/// both named.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct Outcome {
    pub written: Vec<PathBuf>,
    pub left_alone: Vec<PathBuf>,
}

#[cfg(not(any(windows, target_os = "macos")))]
#[derive(Debug, thiserror::Error)]
pub(crate) enum DesktopEntryError {
    /// A path the entry must hold as text is not valid Unicode. The entry is
    /// text, so an odd path cannot be written there unchanged; it is an error,
    /// never a path with its odd characters replaced.
    #[error("the menu entry names {path}, which is not valid Unicode")]
    NotUnicode { path: PathBuf },
    /// A path the entry must hold as text holds a line break, which would end
    /// the entry's line and add a key.
    #[error("the menu entry cannot hold a line break in {path:?}")]
    LineBreak { path: PathBuf },
    /// `exec_line` refused the command. It refuses a path that is not valid
    /// Unicode or that holds a line break; the type is broader.
    #[error("{0}")]
    Command(#[source] AutostartError),
    #[error("{path}: {source}")]
    Io {
        path: PathBuf,
        #[source]
        source: std::io::Error,
    },
}

/// The start's menu-entry work: [`apply`] under `$XDG_DATA_HOME` with this
/// run's flags. On Windows and macOS there is no menu entry: the outcome is
/// empty and nothing is touched.
pub(crate) fn apply_at_start(arguments: &[String]) -> Result<Outcome, String> {
    #[cfg(not(any(windows, target_os = "macos")))]
    {
        let data_home = super::autostart::data_home().map_err(|error| error.to_string())?;
        let exe = super::autostart::running_program()?;
        apply(&data_home, &exe, arguments).map_err(|error| error.to_string())
    }
    #[cfg(any(windows, target_os = "macos"))]
    {
        let _ = arguments;
        Ok(Outcome {
            written: Vec::new(),
            left_alone: Vec::new(),
        })
    }
}

/// Write the menu entry and its icon under `xdg_data_home` (the caller's
/// `$XDG_DATA_HOME`) when they are missing or different. `Exec` is
/// `exec_line(exe, arguments)`, the same command the autostart entry holds.
/// The icon is written first; a failure names the file and stops the other
/// write. A symbolic link at the icon or the entry path is left alone, and so
/// is an entry without the managed line.
#[cfg(not(any(windows, target_os = "macos")))]
pub(crate) fn apply(
    xdg_data_home: &Path,
    exe: &Path,
    arguments: &[String],
) -> Result<Outcome, DesktopEntryError> {
    let entry_path = applications_entry(xdg_data_home);
    let icon_path = app_icon(xdg_data_home);
    // Before anything is written: a path the entry cannot hold is an error.
    let exec = exec_line(exe, arguments).map_err(|error| match error {
        AutostartError::NotUnicode { path } => DesktopEntryError::NotUnicode { path },
        other => DesktopEntryError::Command(other),
    })?;
    let icon = icon_path
        .to_str()
        .ok_or_else(|| DesktopEntryError::NotUnicode {
            path: icon_path.clone(),
        })?;
    if icon.contains(['\n', '\r']) {
        return Err(DesktopEntryError::LineBreak {
            path: icon_path.clone(),
        });
    }
    // A string value in the entry doubles a backslash (Desktop Entry spec).
    let icon = icon.replace('\\', "\\\\");
    let entry = format!(
        "[Desktop Entry]\nType=Application\nName=seatswap\nComment=Switch Claude Code between Claude accounts\n{exec}\nIcon={icon}\nTerminal=false\nCategories=Utility;\nStartupWMClass=seatswap\nX-seatswap-managed=true\n"
    );

    let mut outcome = Outcome {
        written: Vec::new(),
        left_alone: Vec::new(),
    };

    if is_symlink(&icon_path)? {
        outcome.left_alone.push(icon_path);
    } else if write_if_different(&icon_path, APP_ICON_PNG)? {
        outcome.written.push(icon_path);
    }

    if is_symlink(&entry_path)? {
        outcome.left_alone.push(entry_path);
        return Ok(outcome);
    }
    let existing = match fs::read(&entry_path) {
        Ok(bytes) => Some(bytes),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => None,
        Err(source) => {
            return Err(DesktopEntryError::Io {
                path: entry_path,
                source,
            })
        }
    };
    match existing {
        None => {
            write_file(&entry_path, entry.as_bytes())?;
            outcome.written.push(entry_path);
        }
        Some(existing) if !has_managed_line(&existing) => outcome.left_alone.push(entry_path),
        Some(existing) if existing == entry.as_bytes() => {}
        Some(_) => {
            write_file(&entry_path, entry.as_bytes())?;
            outcome.written.push(entry_path);
        }
    }
    Ok(outcome)
}

/// Whether `path` is a symbolic link, including one whose target is missing
/// ([`symlink_at`]), with the path named in an error.
#[cfg(not(any(windows, target_os = "macos")))]
fn is_symlink(path: &Path) -> Result<bool, DesktopEntryError> {
    symlink_at(path).map_err(|source| DesktopEntryError::Io {
        path: path.to_path_buf(),
        source,
    })
}

/// Whether `bytes` hold a line that is exactly [`MANAGED_LINE`].
#[cfg(not(any(windows, target_os = "macos")))]
fn has_managed_line(bytes: &[u8]) -> bool {
    has_line(bytes, MANAGED_LINE)
}

/// Write `bytes` to `path` when it is missing or holds another content.
/// `Ok(true)` when the file was written.
#[cfg(not(any(windows, target_os = "macos")))]
fn write_if_different(path: &Path, bytes: &[u8]) -> Result<bool, DesktopEntryError> {
    match fs::read(path) {
        Ok(existing) if existing == bytes => return Ok(false),
        Ok(_) => {}
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
        Err(source) => {
            return Err(DesktopEntryError::Io {
                path: path.to_path_buf(),
                source,
            })
        }
    }
    write_file(path, bytes)?;
    Ok(true)
}

/// Create `path`'s folder when it is missing and replace `path` with `bytes`.
#[cfg(not(any(windows, target_os = "macos")))]
fn write_file(path: &Path, bytes: &[u8]) -> Result<(), DesktopEntryError> {
    if let Some(folder) = path.parent() {
        fs::create_dir_all(folder).map_err(|source| DesktopEntryError::Io {
            path: folder.to_path_buf(),
            source,
        })?;
    }
    atomic::write_atomic(path, bytes, Access::Shared).map_err(|source| DesktopEntryError::Io {
        path: path.to_path_buf(),
        source,
    })
}

#[cfg(all(test, not(any(windows, target_os = "macos"))))]
mod tests {
    use super::*;

    use std::time::{Duration, SystemTime};

    fn hand_made_entry() -> &'static str {
        "[Desktop Entry]\nType=Application\nName=seatswap\nExec=/usr/bin/seatswap-tray\n"
    }

    /// An absolute path that is not valid Unicode.
    #[cfg(unix)]
    fn not_unicode_path() -> PathBuf {
        use std::os::unix::ffi::OsStrExt;
        PathBuf::from(std::ffi::OsStr::from_bytes(b"/opt/seatswap-\xff/data"))
    }

    #[test]
    fn a_missing_entry_and_icon_are_written() {
        let tmp = tempfile::tempdir().expect("tempdir");
        let xdg = tmp.path().join("data");
        let exe = Path::new("/opt/seat swap/seatswap-tray");
        let arguments = vec![
            "--data-dir".to_string(),
            "/home/u/my data".to_string(),
            "--claude-config-dir".to_string(),
            "/home/u/.claude".to_string(),
        ];

        assert_eq!(
            apply(&xdg, exe, &arguments).expect("write the entry"),
            Outcome {
                written: vec![app_icon(&xdg), applications_entry(&xdg)],
                left_alone: Vec::new(),
            }
        );

        let icon = app_icon(&xdg);
        let icon_text = icon.to_str().expect("a Unicode temp path");
        let text = fs::read_to_string(applications_entry(&xdg)).expect("read the entry");
        assert_eq!(
            text,
            format!(
                "[Desktop Entry]\nType=Application\nName=seatswap\nComment=Switch Claude Code between Claude accounts\nExec=\"/opt/seat swap/seatswap-tray\" \"--data-dir\" \"/home/u/my data\" \"--claude-config-dir\" \"/home/u/.claude\"\nIcon={icon_text}\nTerminal=false\nCategories=Utility;\nStartupWMClass=seatswap\nX-seatswap-managed=true\n"
            )
        );
        assert_eq!(fs::read(&icon).expect("read the icon"), APP_ICON_PNG);
    }

    #[test]
    fn an_entry_that_differs_is_rewritten_and_an_equal_one_is_not_touched() {
        let tmp = tempfile::tempdir().expect("tempdir");
        let xdg = tmp.path().join("data");
        let exe = Path::new("/opt/seatswap");
        let entry = applications_entry(&xdg);
        apply(&xdg, exe, &[]).expect("write the entry");
        let written = fs::read_to_string(&entry).expect("read the entry");

        fs::write(
            &entry,
            written.replace("Name=seatswap", "Name=someone else"),
        )
        .expect("change the entry");
        assert_eq!(
            apply(&xdg, exe, &[]).expect("rewrite the entry"),
            Outcome {
                written: vec![entry.clone()],
                left_alone: Vec::new(),
            }
        );
        assert_eq!(
            fs::read_to_string(&entry).expect("read the entry"),
            written,
            "the differing entry holds our text again"
        );

        let old = SystemTime::UNIX_EPOCH + Duration::from_secs(1_600_000_000);
        fs::File::options()
            .write(true)
            .open(&entry)
            .expect("open the entry")
            .set_times(fs::FileTimes::new().set_modified(old))
            .expect("set the mtime");
        assert_eq!(
            apply(&xdg, exe, &[]).expect("an equal entry"),
            Outcome {
                written: Vec::new(),
                left_alone: Vec::new(),
            }
        );
        assert_eq!(
            fs::metadata(&entry)
                .expect("the entry's metadata")
                .modified()
                .expect("the entry's mtime"),
            old,
            "an equal entry is not touched"
        );
    }

    #[test]
    fn a_hand_made_entry_is_left_alone() {
        let tmp = tempfile::tempdir().expect("tempdir");
        let xdg = tmp.path().join("data");
        let entry = applications_entry(&xdg);
        fs::create_dir_all(entry.parent().expect("the applications folder"))
            .expect("create the applications folder");
        fs::write(&entry, hand_made_entry()).expect("a hand-made entry");

        assert_eq!(
            apply(&xdg, Path::new("/opt/seatswap"), &[]).expect("apply"),
            Outcome {
                written: vec![app_icon(&xdg)],
                left_alone: vec![entry.clone()],
            }
        );
        assert_eq!(
            fs::read_to_string(&entry).expect("read the entry"),
            hand_made_entry(),
            "an entry without the managed line is never overwritten"
        );
        assert_eq!(
            fs::read(app_icon(&xdg)).expect("the icon is seatswap's own file"),
            APP_ICON_PNG
        );
    }

    #[test]
    fn an_icon_that_differs_is_written_while_an_equal_entry_is_not() {
        let tmp = tempfile::tempdir().expect("tempdir");
        let xdg = tmp.path().join("data");
        let icon = app_icon(&xdg);
        let entry = applications_entry(&xdg);
        apply(&xdg, Path::new("/opt/seatswap"), &[]).expect("write both");
        let written = fs::read_to_string(&entry).expect("read the entry");

        fs::write(&icon, b"an older picture").expect("an older icon");
        assert_eq!(
            apply(&xdg, Path::new("/opt/seatswap"), &[]).expect("apply"),
            Outcome {
                written: vec![icon.clone()],
                left_alone: Vec::new(),
            }
        );
        assert_eq!(fs::read(&icon).expect("read the icon"), APP_ICON_PNG);
        assert_eq!(
            fs::read_to_string(&entry).expect("read the entry"),
            written,
            "the equal entry keeps its text"
        );
    }

    #[test]
    fn an_icon_that_differs_is_written_while_the_entry_is_left_alone() {
        let tmp = tempfile::tempdir().expect("tempdir");
        let xdg = tmp.path().join("data");
        let icon = app_icon(&xdg);
        let entry = applications_entry(&xdg);
        apply(&xdg, Path::new("/opt/seatswap"), &[]).expect("write both");
        fs::write(&entry, hand_made_entry()).expect("a hand-made entry");
        fs::write(&icon, b"an older picture").expect("an older icon");

        assert_eq!(
            apply(&xdg, Path::new("/opt/seatswap"), &[]).expect("apply"),
            Outcome {
                written: vec![icon.clone()],
                left_alone: vec![entry.clone()],
            }
        );
        assert_eq!(fs::read(&icon).expect("read the icon"), APP_ICON_PNG);
        assert_eq!(
            fs::read_to_string(&entry).expect("read the entry"),
            hand_made_entry(),
            "the hand-made entry is still untouched"
        );
    }

    #[test]
    fn equal_icon_and_entry_are_not_touched() {
        let tmp = tempfile::tempdir().expect("tempdir");
        let xdg = tmp.path().join("data");
        let icon = app_icon(&xdg);
        apply(&xdg, Path::new("/opt/seatswap"), &[]).expect("write both");

        let old = SystemTime::UNIX_EPOCH + Duration::from_secs(1_600_000_000);
        fs::File::options()
            .write(true)
            .open(&icon)
            .expect("open the icon")
            .set_times(fs::FileTimes::new().set_modified(old))
            .expect("set the mtime");
        assert_eq!(
            apply(&xdg, Path::new("/opt/seatswap"), &[]).expect("apply"),
            Outcome {
                written: Vec::new(),
                left_alone: Vec::new(),
            }
        );
        assert_eq!(
            fs::metadata(&icon)
                .expect("the icon's metadata")
                .modified()
                .expect("the icon's mtime"),
            old,
            "an equal icon is not touched"
        );
    }

    #[test]
    fn an_entry_with_crlf_line_endings_is_left_alone() {
        let tmp = tempfile::tempdir().expect("tempdir");
        let xdg = tmp.path().join("data");
        let entry = applications_entry(&xdg);
        fs::create_dir_all(entry.parent().expect("the applications folder"))
            .expect("create the applications folder");
        let crlf = "[Desktop Entry]\r\nType=Application\r\nName=seatswap\r\nExec=/usr/bin/seatswap-tray\r\nX-seatswap-managed=true\r\n";
        fs::write(&entry, crlf).expect("a CRLF entry");

        assert_eq!(
            apply(&xdg, Path::new("/opt/seatswap"), &[]).expect("apply"),
            Outcome {
                written: vec![app_icon(&xdg)],
                left_alone: vec![entry.clone()],
            }
        );
        assert_eq!(
            fs::read_to_string(&entry).expect("read the entry"),
            crlf,
            "a CRLF entry has no line exactly X-seatswap-managed=true"
        );
    }

    #[cfg(unix)]
    #[test]
    fn a_symlinked_icon_is_left_alone_and_its_target_is_unchanged() {
        use std::os::unix::fs::symlink;

        let tmp = tempfile::tempdir().expect("tempdir");
        let xdg = tmp.path().join("data");
        let icon = app_icon(&xdg);
        fs::create_dir_all(icon.parent().expect("the icon folder")).expect("create the folder");
        let target = tmp.path().join("owner.png");
        fs::write(&target, b"the owner's picture").expect("the owner's icon");
        symlink(&target, &icon).expect("a symlinked icon");

        assert_eq!(
            apply(&xdg, Path::new("/opt/seatswap"), &[]).expect("apply"),
            Outcome {
                written: vec![applications_entry(&xdg)],
                left_alone: vec![icon.clone()],
            }
        );
        assert_eq!(
            fs::read(&target).expect("the target"),
            b"the owner's picture",
            "the link's target is not overwritten"
        );
        assert!(
            fs::symlink_metadata(&icon)
                .expect("the link")
                .file_type()
                .is_symlink(),
            "the link stays a link"
        );
    }

    #[cfg(unix)]
    #[test]
    fn a_symlinked_entry_is_left_alone() {
        use std::os::unix::fs::symlink;

        let tmp = tempfile::tempdir().expect("tempdir");
        let xdg = tmp.path().join("data");
        let entry = applications_entry(&xdg);
        fs::create_dir_all(entry.parent().expect("the applications folder"))
            .expect("create the folder");
        let target = tmp.path().join("owner.desktop");
        fs::write(&target, hand_made_entry()).expect("the owner's entry");
        symlink(&target, &entry).expect("a symlinked entry");

        assert_eq!(
            apply(&xdg, Path::new("/opt/seatswap"), &[]).expect("apply"),
            Outcome {
                written: vec![app_icon(&xdg)],
                left_alone: vec![entry.clone()],
            }
        );
        assert_eq!(
            fs::read_to_string(&target).expect("the target"),
            hand_made_entry(),
            "the link's target is not overwritten"
        );
        assert!(
            fs::symlink_metadata(&entry)
                .expect("the link")
                .file_type()
                .is_symlink(),
            "the link stays a link"
        );
    }

    #[cfg(unix)]
    #[test]
    fn a_dangling_link_is_left_alone_not_an_error() {
        use std::os::unix::fs::symlink;

        let tmp = tempfile::tempdir().expect("tempdir");
        let xdg = tmp.path().join("data");
        let icon = app_icon(&xdg);
        let entry = applications_entry(&xdg);
        fs::create_dir_all(icon.parent().expect("the icon folder")).expect("create the folder");
        symlink(tmp.path().join("missing.png"), &icon).expect("a dangling icon link");

        assert_eq!(
            apply(&xdg, Path::new("/opt/seatswap"), &[]).expect("apply"),
            Outcome {
                written: vec![entry.clone()],
                left_alone: vec![icon.clone()],
            }
        );
        assert!(
            fs::symlink_metadata(&icon)
                .expect("the link")
                .file_type()
                .is_symlink(),
            "the dangling link is not replaced"
        );

        fs::remove_file(&icon).expect("remove the icon link");
        fs::remove_file(&entry).expect("remove the entry the first apply wrote");
        symlink(tmp.path().join("missing.desktop"), &entry).expect("a dangling entry link");
        assert_eq!(
            apply(&xdg, Path::new("/opt/seatswap"), &[]).expect("apply"),
            Outcome {
                written: vec![icon.clone()],
                left_alone: vec![entry.clone()],
            }
        );
        assert!(
            fs::symlink_metadata(&entry)
                .expect("the link")
                .file_type()
                .is_symlink(),
            "the dangling entry link is not replaced"
        );
    }

    #[test]
    fn a_backslash_in_the_icon_path_is_escaped() {
        let tmp = tempfile::tempdir().expect("tempdir");
        let xdg = tmp.path().join("data\\with\\backslashes");
        let icon = app_icon(&xdg);
        let icon_text = icon
            .to_str()
            .expect("a Unicode temp path")
            .replace('\\', "\\\\");

        apply(&xdg, Path::new("/opt/seatswap"), &[]).expect("apply");
        let text = fs::read_to_string(applications_entry(&xdg)).expect("read the entry");
        assert!(
            text.contains(&format!("\nIcon={icon_text}\n")),
            "a backslash is doubled in the string value: {text:?}"
        );
        assert_eq!(fs::read(&icon).expect("read the icon"), APP_ICON_PNG);
    }

    #[test]
    fn an_icon_path_with_a_line_break_is_refused() {
        let tmp = tempfile::tempdir().expect("tempdir");
        let xdg = tmp.path().join("data\nwith\na break");
        match apply(&xdg, Path::new("/opt/seatswap"), &[]) {
            Err(DesktopEntryError::LineBreak { path }) => assert_eq!(path, app_icon(&xdg)),
            other => panic!("expected LineBreak, got {other:?}"),
        }
        assert!(
            !applications_entry(&xdg).exists(),
            "nothing may be created for an entry that cannot be written"
        );
        assert!(
            !app_icon(&xdg).exists(),
            "the icon is not written before the entry is refused"
        );
    }

    #[test]
    fn an_exe_path_with_a_line_break_is_refused() {
        let tmp = tempfile::tempdir().expect("tempdir");
        let exe = tmp.path().join("seat\nswap");
        match apply(tmp.path(), &exe, &[]) {
            Err(DesktopEntryError::Command(AutostartError::LineBreak { value })) => {
                assert_eq!(value, exe.to_str().expect("a Unicode temp path"))
            }
            other => panic!("expected a LineBreak command error, got {other:?}"),
        }
        assert!(
            !applications_entry(tmp.path()).exists(),
            "nothing may be created for an entry that cannot be written"
        );
        assert!(
            !app_icon(tmp.path()).exists(),
            "the icon is not written before the command is refused"
        );
    }

    #[test]
    fn an_argument_with_a_line_break_is_refused() {
        let tmp = tempfile::tempdir().expect("tempdir");
        let argument = "/home/u/my\rdata".to_string();
        match apply(
            tmp.path(),
            Path::new("/opt/seatswap"),
            std::slice::from_ref(&argument),
        ) {
            Err(DesktopEntryError::Command(AutostartError::LineBreak { value })) => {
                assert_eq!(value, argument)
            }
            other => panic!("expected a LineBreak command error, got {other:?}"),
        }
        assert!(!applications_entry(tmp.path()).exists());
        assert!(!app_icon(tmp.path()).exists());
    }

    #[cfg(unix)]
    #[test]
    fn a_path_that_is_not_unicode_is_refused() {
        let tmp = tempfile::tempdir().expect("tempdir");
        let exe = not_unicode_path();
        assert!(exe.is_absolute());
        assert!(exe.to_str().is_none());
        match apply(tmp.path(), &exe, &[]) {
            Err(DesktopEntryError::NotUnicode { path }) => assert_eq!(path, exe),
            other => panic!("expected NotUnicode, got {other:?}"),
        }
        assert!(
            !applications_entry(tmp.path()).exists(),
            "nothing may be created for an entry that cannot be written"
        );
        assert!(
            !app_icon(tmp.path()).exists(),
            "the icon is not written before the entry is refused"
        );
    }
}
