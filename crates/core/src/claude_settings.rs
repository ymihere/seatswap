//! The one key seatswap writes in a `settings.json` of a tool that runs a
//! status line command: `statusLine`, in Claude Code's file and, when the person
//! connects it on the Google card, in the Antigravity CLI's (`agy`) file
//! (spec §7.9).
//!
//! A missing file is created holding only that key. An empty file, one that is
//! not a JSON object, or broken JSON is an error with its path and is left
//! untouched. Every other key keeps its value and its position; the file is
//! backed up once before the first write and every write is atomic.
//!
//! [`seatswap_status_line`] always wraps the program path in double quotes and
//! escapes it for the shell Claude Code runs the command through (bash, or Git
//! Bash on Windows), so a path with spaces, `$`, backticks or Windows
//! backslashes reaches the shell unchanged. A path that is not valid Unicode is
//! refused with [`ClaudeSettingsError::NotUnicode`], never replaced by lossy
//! text.

use std::fs;
use std::path::{Path, PathBuf};

use serde_json::{json, Map, Value};

use crate::atomic::{self, Access};
use crate::paths::ClaudePaths;

#[derive(Debug, thiserror::Error)]
pub enum ClaudeSettingsError {
    #[error("{path}: {source}")]
    Io {
        path: PathBuf,
        #[source]
        source: std::io::Error,
    },
    #[error("{path} is not a JSON object")]
    NotAnObject { path: PathBuf },
    #[error("{path}: {source}")]
    Json {
        path: PathBuf,
        #[source]
        source: serde_json::Error,
    },
    #[error("{path} is not valid Unicode, so it cannot be written into a status line command")]
    NotUnicode { path: PathBuf },
}

impl ClaudeSettingsError {
    fn io(path: &Path, source: std::io::Error) -> Self {
        ClaudeSettingsError::Io {
            path: path.to_path_buf(),
            source,
        }
    }

    fn json(path: &Path, source: serde_json::Error) -> Self {
        ClaudeSettingsError::Json {
            path: path.to_path_buf(),
            source,
        }
    }
}

/// The current value of `statusLine`, or `None` when the key or the file is
/// missing.
pub fn status_line(claude: &ClaudePaths) -> Result<Option<Value>, ClaudeSettingsError> {
    status_line_in(&claude.settings_file())
}

/// The current value of `statusLine` in the settings file at `path` (Claude
/// Code's or `agy`'s), or `None` when the key or the file is missing.
pub fn status_line_in(path: &Path) -> Result<Option<Value>, ClaudeSettingsError> {
    Ok(read_object(path)?.and_then(|object| object.get("statusLine").cloned()))
}

/// Set (`Some`) or remove (`None`) `statusLine`. Every other key and the key
/// order stay; a missing file is created holding only this key. Returns the
/// previous value. `backup_once` runs before the first write of an existing
/// file; every write goes through [`atomic::write_atomic`].
pub fn set_status_line(
    claude: &ClaudePaths,
    value: Option<&Value>,
) -> Result<Option<Value>, ClaudeSettingsError> {
    set_status_line_in(&claude.settings_file(), value)
}

/// [`set_status_line`] for the settings file at `path` (Claude Code's or
/// `agy`'s), with the same rules.
pub fn set_status_line_in(
    path: &Path,
    value: Option<&Value>,
) -> Result<Option<Value>, ClaudeSettingsError> {
    let Some(mut object) = read_object(path)? else {
        let Some(value) = value else {
            return Ok(None);
        };
        let mut object = Map::new();
        object.insert("statusLine".to_string(), value.clone());
        write_object(path, &object)?;
        return Ok(None);
    };
    let previous = match value {
        Some(value) => object.insert("statusLine".to_string(), value.clone()),
        None => object.remove("statusLine"),
    };
    // Removing a key that is not there changes nothing, so it writes nothing.
    if value.is_none() && previous.is_none() {
        return Ok(None);
    }
    atomic::backup_once(path).map_err(|source| ClaudeSettingsError::io(path, source))?;
    write_object(path, &object)?;
    Ok(previous)
}

/// The status line Claude Code runs: `{"type":"command","command":"\"<exe>\"
/// statusline","refreshInterval":2}`. The path is always inside double quotes,
/// so the shell Claude Code uses (bash, or Git Bash on Windows) reads it as one
/// word. Inside the quotes `"`, `$` and `` ` `` get a leading backslash, and a
/// backslash is doubled when it ends the path, directly precedes one of those
/// three or another backslash, or is part of a run of two or more (so a UNC
/// path's leading `\\` becomes `\\\\`). A path that is not valid Unicode is
/// [`ClaudeSettingsError::NotUnicode`], never a lossy replacement.
pub fn seatswap_status_line(exe: &Path) -> Result<Value, ClaudeSettingsError> {
    Ok(json!({
        "type": "command",
        "command": status_line_command(exe)?,
        "refreshInterval": 2,
    }))
}

/// The command both tools run: `"<exe>" statusline`, the path quoted as
/// [`seatswap_status_line`] describes.
pub fn status_line_command(exe: &Path) -> Result<String, ClaudeSettingsError> {
    let path = exe
        .to_str()
        .ok_or_else(|| ClaudeSettingsError::NotUnicode {
            path: exe.to_path_buf(),
        })?;
    Ok(format!("\"{}\" statusline", quote_path(path)))
}

/// `path` as one bash double-quoted word's contents: `"`, `$` and `` ` `` are
/// escaped, a backslash before a special character or at the end is doubled,
/// and a run of backslashes is doubled as a whole so each stays literal.
fn quote_path(path: &str) -> String {
    let characters: Vec<char> = path.chars().collect();
    let mut quoted = String::with_capacity(path.len());
    for (index, &character) in characters.iter().enumerate() {
        match character {
            '"' | '$' | '`' => {
                quoted.push('\\');
                quoted.push(character);
            }
            '\\' => {
                let next = characters.get(index + 1);
                let previous = index
                    .checked_sub(1)
                    .and_then(|before| characters.get(before));
                let doubled = next.is_none()
                    || matches!(next, Some('"' | '$' | '`' | '\\'))
                    || matches!(previous, Some('\\'));
                if doubled {
                    quoted.push_str("\\\\");
                } else {
                    quoted.push('\\');
                }
            }
            other => quoted.push(other),
        }
    }
    quoted
}

/// A settings object. A missing file is `None`; an empty file, a
/// value that is not an object, and broken JSON are errors with the path.
fn read_object(path: &Path) -> Result<Option<Map<String, Value>>, ClaudeSettingsError> {
    let bytes = match fs::read(path) {
        Ok(bytes) => bytes,
        Err(source) if source.kind() == std::io::ErrorKind::NotFound => return Ok(None),
        Err(source) => return Err(ClaudeSettingsError::io(path, source)),
    };
    if bytes.is_empty() {
        return Err(ClaudeSettingsError::NotAnObject {
            path: path.to_path_buf(),
        });
    }
    let value: Value =
        serde_json::from_slice(&bytes).map_err(|source| ClaudeSettingsError::json(path, source))?;
    match value {
        Value::Object(object) => Ok(Some(object)),
        _ => Err(ClaudeSettingsError::NotAnObject {
            path: path.to_path_buf(),
        }),
    }
}

/// Pretty JSON with two spaces, as Claude Code writes its files, with no
/// trailing newline. A new settings file is not secret, so it is shared.
fn write_object(path: &Path, object: &Map<String, Value>) -> Result<(), ClaudeSettingsError> {
    let bytes = serde_json::to_vec_pretty(object)
        .map_err(|source| ClaudeSettingsError::json(path, source))?;
    atomic::write_atomic(path, &bytes, Access::Shared)
        .map_err(|source| ClaudeSettingsError::io(path, source))
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;
    use std::path::PathBuf;

    fn claude_in(tmp: &tempfile::TempDir) -> ClaudePaths {
        let config = tmp.path().join("claude");
        fs::create_dir(&config).expect("the config folder");
        ClaudePaths::for_config_dir(&config, tmp.path())
    }

    fn backup_path(path: &Path) -> PathBuf {
        let mut name = path.as_os_str().to_os_string();
        name.push(".seatswap-backup");
        PathBuf::from(name)
    }

    fn read_value(path: &Path) -> Value {
        serde_json::from_slice(&fs::read(path).expect("the settings file")).expect("JSON")
    }

    fn keys(path: &Path) -> Vec<String> {
        let value = read_value(path);
        value
            .as_object()
            .expect("an object")
            .keys()
            .cloned()
            .collect()
    }

    #[test]
    fn set_keeps_every_other_key_and_the_order() {
        let tmp = tempfile::tempdir().expect("tempdir");
        let claude = claude_in(&tmp);
        let path = claude.settings_file();
        let original =
            br#"{"model":"opus","statusLine":{"type":"command","command":"bash ~/x.sh"},"hooks":{}}"#;
        fs::write(&path, original).expect("the original settings");
        let before = read_value(&path);
        let old_status_line = before["statusLine"].clone();

        let ours = seatswap_status_line(Path::new("/opt/seatswap")).expect("the status line");
        let returned = set_status_line(&claude, Some(&ours)).expect("set the status line");

        assert_eq!(returned, Some(old_status_line), "the old value is returned");
        assert_eq!(keys(&path), ["model", "statusLine", "hooks"]);
        let after = read_value(&path);
        for key in ["model", "hooks"] {
            assert_eq!(
                serde_json::to_vec(&after[key]).expect("a value"),
                serde_json::to_vec(&before[key]).expect("a value"),
                "{key} must stay byte-for-byte equal"
            );
        }
        assert_eq!(after["statusLine"], ours);
    }

    #[test]
    fn a_missing_file_is_created_with_one_key() {
        let tmp = tempfile::tempdir().expect("tempdir");
        let claude = claude_in(&tmp);
        let path = claude.settings_file();
        assert!(!path.exists());
        let ours = seatswap_status_line(Path::new("/opt/seatswap")).expect("the status line");

        assert_eq!(
            set_status_line(&claude, Some(&ours)).expect("create the file"),
            None
        );

        let text = fs::read_to_string(&path).expect("the new settings file");
        assert!(text.starts_with("{\n  \"statusLine\": {"), "{text}");
        let value = read_value(&path);
        assert_eq!(value, json!({ "statusLine": ours }));
        assert_eq!(value.as_object().expect("an object").len(), 1);
        assert!(!backup_path(&path).exists(), "a new file needs no backup");
    }

    #[test]
    fn a_file_that_is_not_an_object_is_refused() {
        let tmp = tempfile::tempdir().expect("tempdir");
        let claude = claude_in(&tmp);
        let path = claude.settings_file();
        let ours = seatswap_status_line(Path::new("/opt/seatswap")).expect("the status line");

        for bytes in [b"[]".as_slice(), b"\"\"".as_slice(), b"3".as_slice()] {
            fs::write(&path, bytes).expect("a file that is not an object");
            match set_status_line(&claude, Some(&ours)) {
                Err(ClaudeSettingsError::NotAnObject { path: refused }) => {
                    assert_eq!(refused, path)
                }
                other => panic!("expected NotAnObject, got {other:?}"),
            }
            assert_eq!(fs::read(&path).expect("the file"), bytes);
            assert!(
                !backup_path(&path).exists(),
                "a refused file is not backed up"
            );
        }

        fs::write(&path, b"{\"model\":").expect("broken JSON");
        match set_status_line(&claude, Some(&ours)) {
            Err(ClaudeSettingsError::Json { path: refused, .. }) => assert_eq!(refused, path),
            other => panic!("expected Json, got {other:?}"),
        }
        assert_eq!(fs::read(&path).expect("the file"), b"{\"model\":");
        assert!(!backup_path(&path).exists());
    }

    #[test]
    fn an_empty_file_is_refused() {
        let tmp = tempfile::tempdir().expect("tempdir");
        let claude = claude_in(&tmp);
        let path = claude.settings_file();
        fs::write(&path, b"").expect("an empty settings file");
        let ours = seatswap_status_line(Path::new("/opt/seatswap")).expect("the status line");

        match set_status_line(&claude, Some(&ours)) {
            Err(ClaudeSettingsError::NotAnObject { path: refused }) => assert_eq!(refused, path),
            other => panic!("expected NotAnObject, got {other:?}"),
        }
        assert_eq!(fs::read(&path).expect("the file"), b"");
        assert!(!backup_path(&path).exists());
        assert!(
            status_line(&claude).is_err(),
            "an empty file is not empty settings"
        );
    }

    #[test]
    fn remove_deletes_only_the_key() {
        let tmp = tempfile::tempdir().expect("tempdir");
        let claude = claude_in(&tmp);
        let path = claude.settings_file();
        let original =
            br#"{"model":"opus","statusLine":{"type":"command","command":"bash ~/x.sh"},"hooks":{}}"#;
        fs::write(&path, original).expect("the original settings");
        let before = read_value(&path);
        let old_status_line = before["statusLine"].clone();

        let returned = set_status_line(&claude, None).expect("remove the status line");

        assert_eq!(returned, Some(old_status_line));
        assert_eq!(keys(&path), ["model", "hooks"]);
        let after = read_value(&path);
        for key in ["model", "hooks"] {
            assert_eq!(
                serde_json::to_vec(&after[key]).expect("a value"),
                serde_json::to_vec(&before[key]).expect("a value"),
                "{key} must stay byte-for-byte equal"
            );
        }
    }

    #[test]
    fn the_backup_is_made_once() {
        let tmp = tempfile::tempdir().expect("tempdir");
        let claude = claude_in(&tmp);
        let path = claude.settings_file();
        let original =
            br#"{"model":"opus","statusLine":{"type":"command","command":"bash ~/x.sh"}}"#;
        fs::write(&path, original).expect("the original settings");
        let ours = seatswap_status_line(Path::new("/opt/seatswap")).expect("the status line");

        set_status_line(&claude, Some(&ours)).expect("first set");
        set_status_line(&claude, None).expect("second set");

        let backup = backup_path(&path);
        assert_eq!(fs::read(&backup).expect("the backup"), original);
        assert_eq!(
            fs::read_dir(tmp.path().join("claude"))
                .expect("the config folder")
                .filter_map(|entry| entry.ok())
                .filter(|entry| entry
                    .file_name()
                    .to_string_lossy()
                    .contains("seatswap-backup"))
                .count(),
            1,
            "one backup, not one per write"
        );
    }

    #[test]
    fn an_exe_path_with_a_space_is_quoted() {
        let line =
            seatswap_status_line(Path::new("/opt/my apps/seatswap")).expect("the status line");
        assert_eq!(
            line,
            json!({
                "type": "command",
                "command": "\"/opt/my apps/seatswap\" statusline",
                "refreshInterval": 2,
            })
        );
        assert_eq!(
            seatswap_status_line(Path::new("/opt/my apps/seatswap")).expect("the status line")
                ["command"],
            "\"/opt/my apps/seatswap\" statusline"
        );
        assert_eq!(
            seatswap_status_line(Path::new("/opt/seatswap")).expect("the status line")["command"],
            "\"/opt/seatswap\" statusline",
            "a plain path is quoted too, so bash reads it as one word"
        );
        assert_eq!(
            seatswap_status_line(Path::new("/opt/a\"b/seatswap")).expect("the status line")
                ["command"],
            "\"/opt/a\\\"b/seatswap\" statusline",
            "an inner quote is escaped"
        );
    }

    #[test]
    fn a_plain_exe_path_is_still_quoted() {
        let line = seatswap_status_line(Path::new("/usr/bin/seatswap")).expect("the status line");
        assert_eq!(
            line,
            json!({
                "type": "command",
                "command": "\"/usr/bin/seatswap\" statusline",
                "refreshInterval": 2,
            })
        );
    }

    #[test]
    fn shell_characters_in_the_path_are_escaped() {
        assert_eq!(
            seatswap_status_line(Path::new("/opt/a$b/seatswap")).expect("the status line")
                ["command"],
            "\"/opt/a\\$b/seatswap\" statusline",
            "a dollar is not expanded by bash"
        );
        assert_eq!(
            seatswap_status_line(Path::new("/opt/a`b/seatswap")).expect("the status line")
                ["command"],
            "\"/opt/a\\`b/seatswap\" statusline",
            "a backtick does not start a command substitution"
        );
    }

    #[test]
    fn a_windows_path_keeps_its_backslashes() {
        let line = seatswap_status_line(Path::new(r"C:\Program Files\seatswap\seatswap.exe"))
            .expect("the status line");
        assert_eq!(
            line["command"],
            r#""C:\Program Files\seatswap\seatswap.exe" statusline"#
        );
    }

    #[test]
    fn a_unc_path_doubles_its_leading_backslashes() {
        let line =
            seatswap_status_line(Path::new(r"\\vega\tools\seatswap.exe")).expect("the status line");
        assert_eq!(
            line["command"],
            r#""\\\\vega\tools\seatswap.exe" statusline"#
        );
    }

    #[cfg(unix)]
    #[test]
    fn a_path_that_is_not_unicode_is_refused() {
        use std::ffi::OsStr;
        use std::os::unix::ffi::OsStrExt;

        let odd = PathBuf::from(OsStr::from_bytes(b"/opt/\xff/seatswap"));
        match seatswap_status_line(&odd) {
            Err(ClaudeSettingsError::NotUnicode { path }) => assert_eq!(path, odd),
            other => panic!("expected NotUnicode, got {other:?}"),
        }
    }

    #[cfg(unix)]
    #[test]
    fn the_quoted_command_reaches_the_program_through_bash() {
        use std::os::unix::fs::PermissionsExt;

        let tmp = tempfile::tempdir().expect("tempdir");
        let folder = tmp.path().join("a space $HOME's folder");
        fs::create_dir(&folder).expect("the folder");
        let exe = folder.join("seatswap");
        fs::write(&exe, b"#!/bin/sh\nprintf '%s' \"$1\"\n").expect("the script");
        fs::set_permissions(&exe, fs::Permissions::from_mode(0o755)).expect("executable");

        let line = seatswap_status_line(&exe).expect("the status line");
        let command = line["command"].as_str().expect("the command text");
        let output = std::process::Command::new("bash")
            .arg("-c")
            .arg(command)
            .output()
            .expect("bash runs the command");

        assert!(
            output.status.success(),
            "bash failed: {}",
            String::from_utf8_lossy(&output.stderr)
        );
        assert_eq!(
            String::from_utf8(output.stdout).expect("UTF-8 output"),
            "statusline"
        );
    }

    // ---- proof tests of the review ----

    #[cfg(unix)]
    #[test]
    fn proof_a_symlinked_settings_file_stays_a_symlink() {
        let tmp = tempfile::tempdir().expect("tempdir");
        let claude = claude_in(&tmp);
        let real = tmp.path().join("dotfiles-settings.json");
        fs::write(&real, br#"{"model":"opus"}"#).expect("target");
        std::os::unix::fs::symlink(&real, claude.settings_file()).expect("symlink");
        let ours = seatswap_status_line(Path::new("/opt/seatswap")).expect("line");
        set_status_line(&claude, Some(&ours)).expect("set");
        assert!(
            fs::symlink_metadata(claude.settings_file())
                .expect("meta")
                .file_type()
                .is_symlink(),
            "the symlink was replaced by a regular file"
        );
    }
}
