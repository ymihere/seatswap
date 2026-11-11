//! Start the tray app at login (docs/implementation.md "Autostart",
//! docs/windows.md §6.5). One entry point, [`apply_autostart`]; only where the
//! entry is stored differs:
//! - Linux: the XDG autostart entry, one small `.desktop` file written through
//!   `atomic::write_atomic` like every other file seatswap owns;
//! - Windows: the value `seatswap` under the `Run` key, which starts
//!   `seatswap-tray.exe` (the program without a console window).

#[cfg(not(windows))]
use std::ffi::{OsStr, OsString};
#[cfg(not(windows))]
use std::fs;
use std::path::{Path, PathBuf};

#[cfg(not(windows))]
use seatswap_core::atomic::{self, Access};
#[cfg(not(windows))]
use seatswap_core::paths::autostart_file;

#[cfg(windows)]
use super::registry;

#[derive(Debug, thiserror::Error)]
pub(crate) enum AutostartError {
    #[cfg(not(windows))]
    #[error("cannot find the user config folder: $XDG_CONFIG_HOME and $HOME are not set")]
    NoConfigHome,
    #[cfg(not(windows))]
    #[error("$HOME is empty or not an absolute path: {0:?}")]
    HomeNotAbsolute(OsString),
    #[error("autostart argument {0} is not an absolute path")]
    RelativePath(PathBuf),
    #[cfg(not(windows))]
    #[error("{path}: {source}")]
    Io {
        path: PathBuf,
        #[source]
        source: std::io::Error,
    },
    #[cfg(windows)]
    #[error("cannot start at login: {path} is missing")]
    NoTrayProgram { path: PathBuf },
    #[cfg(windows)]
    #[error("cannot start at login: {path} is not valid Unicode")]
    NotUnicode { path: PathBuf },
    #[cfg(windows)]
    #[error(
        "cannot start at login: the command is {length} characters long, Windows allows {RUN_COMMAND_LIMIT}"
    )]
    CommandTooLong { length: usize },
    #[cfg(windows)]
    #[error("registry HKEY_CURRENT_USER\\{key}: {source}")]
    Registry {
        key: String,
        #[source]
        source: std::io::Error,
    },
}

/// `$XDG_CONFIG_HOME`, else `~/.config`. Following the XDG spec, a relative
/// `$XDG_CONFIG_HOME` is ignored; a `$HOME` that is empty or relative is an
/// error, never quietly replaced.
#[cfg(not(windows))]
pub(crate) fn config_home_from(
    xdg_config_home: Option<&OsStr>,
    home: Option<&OsStr>,
) -> Result<PathBuf, AutostartError> {
    if let Some(dir) = xdg_config_home {
        if !dir.is_empty() {
            let path = PathBuf::from(dir);
            if path.is_absolute() {
                return Ok(path);
            }
        }
    }
    let Some(home) = home else {
        return Err(AutostartError::NoConfigHome);
    };
    if home.is_empty() {
        return Err(AutostartError::HomeNotAbsolute(OsString::new()));
    }
    let path = PathBuf::from(home);
    if !path.is_absolute() {
        return Err(AutostartError::HomeNotAbsolute(path.into_os_string()));
    }
    Ok(path.join(".config"))
}

/// `$XDG_CONFIG_HOME`, else `~/.config` from the process environment.
#[cfg(not(windows))]
pub(crate) fn config_home() -> Result<PathBuf, AutostartError> {
    config_home_from(
        std::env::var_os("XDG_CONFIG_HOME").as_deref(),
        std::env::var_os("HOME").as_deref(),
    )
}

/// The command-line flags the tray app was started with, so the autostart entry
/// starts the same data folder and Claude folder. The command line made both
/// absolute (`paths::absolute`); a relative one here is an error.
pub(crate) fn startup_arguments(
    data_dir: Option<&Path>,
    claude_config_dir: Option<&Path>,
) -> Result<Vec<String>, AutostartError> {
    let mut arguments = Vec::new();
    for (flag, path) in [
        ("--data-dir", data_dir),
        ("--claude-config-dir", claude_config_dir),
    ] {
        let Some(path) = path else {
            continue;
        };
        if !path.is_absolute() {
            return Err(AutostartError::RelativePath(path.to_path_buf()));
        }
        arguments.push(flag.to_string());
        arguments.push(path.display().to_string());
    }
    Ok(arguments)
}

/// Quote one `Exec` argument per the Desktop Entry spec: wrap it in double
/// quotes, escape `"`, `` ` ``, `$` and `\` inside the quotes, write `%` as
/// `%%`, and then apply the string escaping (every `\` doubled again, so one
/// literal backslash ends up as four).
#[cfg(not(windows))]
fn quote_argument(argument: &str) -> String {
    let mut quoted = String::with_capacity(argument.len() + 2);
    quoted.push('"');
    for character in argument.chars() {
        match character {
            '\\' => quoted.push_str("\\\\"),
            '"' => quoted.push_str("\\\""),
            '`' => quoted.push_str("\\`"),
            '$' => quoted.push_str("\\$"),
            '%' => quoted.push_str("%%"),
            other => quoted.push(other),
        }
    }
    quoted.push('"');
    quoted.replace('\\', "\\\\")
}

#[cfg(not(windows))]
fn exec_line(exe: &Path, arguments: &[String]) -> String {
    let mut parts = Vec::with_capacity(arguments.len() + 1);
    parts.push(quote_argument(&exe.display().to_string()));
    for argument in arguments {
        parts.push(quote_argument(argument));
    }
    format!("Exec={}", parts.join(" "))
}

/// Write the autostart entry when `enabled`, remove it otherwise. Removing an
/// entry that is not there is fine.
#[cfg(not(windows))]
pub(crate) fn apply(
    enabled: bool,
    exe: &Path,
    config_home: &Path,
    arguments: &[String],
) -> Result<(), AutostartError> {
    let path = autostart_file(config_home);
    if !enabled {
        return match fs::remove_file(&path) {
            Ok(()) => Ok(()),
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(()),
            Err(source) => Err(AutostartError::Io { path, source }),
        };
    }
    if let Some(folder) = path.parent() {
        fs::create_dir_all(folder).map_err(|source| AutostartError::Io {
            path: folder.to_path_buf(),
            source,
        })?;
    }
    let entry = format!(
        "[Desktop Entry]\nType=Application\nName=seatswap\nComment=Switch Claude Code between Claude accounts\n{}\nTerminal=false\nX-GNOME-Autostart-enabled=true\n",
        exec_line(exe, arguments)
    );
    // `write_atomic` already gives a new file the requested mode.
    atomic::write_atomic(&path, entry.as_bytes(), Access::Shared).map_err(|source| {
        AutostartError::Io {
            path: path.clone(),
            source,
        }
    })
}

/// The name of the autostart value under the `Run` key.
#[cfg(windows)]
const RUN_VALUE: &str = "seatswap";
/// Microsoft's documented limit for a `Run` command (not tested here).
#[cfg(windows)]
const RUN_COMMAND_LIMIT: usize = 260;

/// The program the entry starts: `seatswap-tray.exe` in the folder of
/// `current_exe`, the same program without a console window.
#[cfg(windows)]
fn tray_program(current_exe: &Path) -> Result<PathBuf, AutostartError> {
    let path = current_exe.with_file_name("seatswap-tray.exe");
    if path.is_file() {
        Ok(path)
    } else {
        Err(AutostartError::NoTrayProgram { path })
    }
}

/// Quote one part of a Windows command line the way Windows programs split
/// their arguments: wrap it in double quotes, write `"` as `\"`, keep
/// backslashes literal except that a run of them directly before a `"` or at
/// the end of the part is doubled.
#[cfg(windows)]
fn quote_argument(part: &str) -> String {
    let mut quoted = String::with_capacity(part.len() + 2);
    quoted.push('"');
    let mut backslashes = 0;
    for character in part.chars() {
        match character {
            '\\' => backslashes += 1,
            '"' => {
                quoted.extend(std::iter::repeat_n('\\', backslashes * 2 + 1));
                quoted.push('"');
                backslashes = 0;
            }
            other => {
                quoted.extend(std::iter::repeat_n('\\', backslashes));
                quoted.push(other);
                backslashes = 0;
            }
        }
    }
    quoted.extend(std::iter::repeat_n('\\', backslashes * 2));
    quoted.push('"');
    quoted
}

/// `"<exe>" "<arg>" …` with Windows quoting. A program path that is not valid
/// Unicode, or a command longer than [`RUN_COMMAND_LIMIT`] UTF-16 units, is an
/// error; the command is never shortened.
#[cfg(windows)]
fn command_line(exe: &Path, arguments: &[String]) -> Result<String, AutostartError> {
    let program = exe.to_str().ok_or_else(|| AutostartError::NotUnicode {
        path: exe.to_path_buf(),
    })?;
    let mut parts = Vec::with_capacity(arguments.len() + 1);
    parts.push(quote_argument(program));
    for argument in arguments {
        parts.push(quote_argument(argument));
    }
    let command = parts.join(" ");
    let length = command.encode_utf16().count();
    if length > RUN_COMMAND_LIMIT {
        return Err(AutostartError::CommandTooLong { length });
    }
    Ok(command)
}

/// Enabled: set the value [`RUN_VALUE`] under `HKCU\<run_key>` to the command
/// that starts `seatswap-tray.exe` with `arguments`. Not enabled: delete it
/// (a value that is not there is fine).
#[cfg(windows)]
pub(crate) fn apply(
    enabled: bool,
    current_exe: &Path,
    run_key: &str,
    arguments: &[String],
) -> Result<(), AutostartError> {
    let registry_error = |source| AutostartError::Registry {
        key: run_key.to_string(),
        source,
    };
    if !enabled {
        return registry::delete_value(run_key, RUN_VALUE).map_err(registry_error);
    }
    let program = tray_program(current_exe)?;
    let command = command_line(&program, arguments)?;
    registry::set_string(run_key, RUN_VALUE, &command).map_err(registry_error)
}

/// Apply the `startAtLogin` setting with the executable and flags of this run.
pub(crate) fn apply_autostart(enabled: bool, arguments: &[String]) -> Result<(), String> {
    let exe = std::env::current_exe()
        .map_err(|error| format!("cannot find the seatswap executable: {error}"))?;
    #[cfg(not(windows))]
    {
        let config_home = config_home().map_err(|error| error.to_string())?;
        apply(enabled, &exe, &config_home, arguments).map_err(|error| error.to_string())
    }
    #[cfg(windows)]
    apply(enabled, &exe, registry::RUN_KEY, arguments).map_err(|error| error.to_string())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn startup_arguments_carry_only_the_flags_that_were_given() {
        let data = std::env::temp_dir().join("data");
        let claude = std::env::temp_dir().join("claude");
        let data_text = data.display().to_string();
        let claude_text = claude.display().to_string();
        assert_eq!(
            startup_arguments(Some(&data), Some(&claude)).expect("absolute paths"),
            vec![
                "--data-dir".to_string(),
                data_text,
                "--claude-config-dir".to_string(),
                claude_text.clone(),
            ]
        );
        assert_eq!(
            startup_arguments(None, Some(&claude)).expect("one flag"),
            vec!["--claude-config-dir".to_string(), claude_text]
        );
        assert!(startup_arguments(None, None).expect("no flags").is_empty());
    }

    #[test]
    fn startup_arguments_refuse_a_relative_path() {
        for (data, claude) in [
            (Some(Path::new("relative/data")), None),
            (None, Some(Path::new("relative/claude"))),
        ] {
            match startup_arguments(data, claude) {
                Err(AutostartError::RelativePath(path)) => assert!(path.is_relative()),
                other => panic!("expected RelativePath, got {other:?}"),
            }
        }
    }

    #[cfg(not(windows))]
    mod xdg {
        use super::super::*;

        use std::os::unix::fs::PermissionsExt;

        #[test]
        fn on_writes_an_entry_with_the_exe_and_off_removes_it() {
            let tmp = tempfile::tempdir().expect("tempdir");
            let exe = Path::new("/opt/seat swap/seatswap");
            let path = autostart_file(tmp.path());
            assert!(!path.exists());

            apply(true, exe, tmp.path(), &[]).expect("write the entry");
            let text = fs::read_to_string(&path).expect("read the entry");
            assert!(
                text.contains("Exec=\"/opt/seat swap/seatswap\""),
                "{text:?}"
            );
            let mode = fs::metadata(&path).expect("metadata").permissions().mode() & 0o777;
            assert_eq!(mode & 0o002, 0, "the entry must not be world-writable");

            apply(false, exe, tmp.path(), &[]).expect("remove the entry");
            assert!(!path.exists());
        }

        #[test]
        fn exec_quotes_the_argument_per_the_desktop_entry_spec() {
            let tmp = tempfile::tempdir().expect("tempdir");
            let exe = Path::new("/tmp/a b/50%\\back\"slash/seatswap");
            apply(true, exe, tmp.path(), &[]).expect("write the entry");
            let text = fs::read_to_string(autostart_file(tmp.path())).expect("read the entry");
            assert!(
                text.contains("Exec=\"/tmp/a b/50%%\\\\\\\\back\\\\\"slash/seatswap\""),
                "{text:?}"
            );
        }

        #[test]
        fn the_entry_passes_the_startup_flags() {
            let tmp = tempfile::tempdir().expect("tempdir");
            let arguments = vec![
                "--data-dir".to_string(),
                "/home/u/my data".to_string(),
                "--claude-config-dir".to_string(),
                "/home/u/.claude".to_string(),
            ];
            apply(true, Path::new("/opt/seatswap"), tmp.path(), &arguments).expect("write");
            let text = fs::read_to_string(autostart_file(tmp.path())).expect("read the entry");
            assert!(
                text.contains(
                    "Exec=\"/opt/seatswap\" \"--data-dir\" \"/home/u/my data\" \"--claude-config-dir\" \"/home/u/.claude\""
                ),
                "{text:?}"
            );
        }

        #[test]
        fn config_home_follows_the_xdg_rules() {
            // An absolute $XDG_CONFIG_HOME wins.
            assert_eq!(
                config_home_from(Some(OsStr::new("/xdg")), Some(OsStr::new("/home/u")))
                    .expect("absolute xdg"),
                PathBuf::from("/xdg")
            );
            // A relative $XDG_CONFIG_HOME is ignored.
            assert_eq!(
                config_home_from(Some(OsStr::new("relative")), Some(OsStr::new("/home/u")))
                    .expect("relative xdg ignored"),
                PathBuf::from("/home/u/.config")
            );
            // An empty $XDG_CONFIG_HOME is ignored too.
            assert_eq!(
                config_home_from(Some(OsStr::new("")), Some(OsStr::new("/home/u")))
                    .expect("empty xdg ignored"),
                PathBuf::from("/home/u/.config")
            );
            // An empty or relative $HOME is an error.
            assert!(config_home_from(None, Some(OsStr::new(""))).is_err());
            assert!(config_home_from(None, Some(OsStr::new("relative"))).is_err());
            assert!(config_home_from(Some(OsStr::new("relative")), None).is_err());
        }

        #[test]
        fn off_without_a_file_is_not_an_error() {
            let tmp = tempfile::tempdir().expect("tempdir");
            apply(false, Path::new("/x/seatswap"), tmp.path(), &[]).expect("no file, no error");
        }

        #[test]
        fn turning_it_off_then_on_again_rewrites_the_entry() {
            let tmp = tempfile::tempdir().expect("tempdir");
            let path = autostart_file(tmp.path());
            apply(true, Path::new("/one/seatswap"), tmp.path(), &[]).expect("first");
            apply(false, Path::new("/one/seatswap"), tmp.path(), &[]).expect("off");
            apply(true, Path::new("/two/seatswap"), tmp.path(), &[]).expect("second");
            let text = fs::read_to_string(path).expect("read");
            assert!(text.contains("Exec=\"/two/seatswap\""), "{text:?}");
        }
    }

    #[cfg(windows)]
    mod run_key {
        use super::super::*;
        use crate::gui::registry::TestKey;
        use std::fs;

        #[test]
        fn quoting_follows_the_windows_rules() {
            for (part, written) in [
                (
                    r"C:\Program Files\seatswap\seatswap-tray.exe",
                    r#""C:\Program Files\seatswap\seatswap-tray.exe""#,
                ),
                ("--data-dir", r#""--data-dir""#),
                (r"C:\my data", r#""C:\my data""#),
                (r"C:\dir\", r#""C:\dir\\""#),
                (r#"a"b"#, r#""a\"b""#),
                (r#"a\"b"#, r#""a\\\"b""#),
            ] {
                assert_eq!(quote_argument(part), written, "quoting {part}");
            }
        }

        /// A folder with `seatswap.exe` and `seatswap-tray.exe` side by side.
        fn programs(tmp: &tempfile::TempDir) -> PathBuf {
            let folder = tmp.path().join("seat swap");
            fs::create_dir(&folder).expect("program folder");
            fs::write(folder.join("seatswap.exe"), b"MZ").expect("seatswap.exe");
            fs::write(folder.join("seatswap-tray.exe"), b"MZ").expect("seatswap-tray.exe");
            folder.join("seatswap.exe")
        }

        #[test]
        fn tray_program_finds_the_sibling_and_reports_a_missing_one() {
            let tmp = tempfile::tempdir().expect("tempdir");
            let exe = programs(&tmp);
            assert_eq!(
                tray_program(&exe).expect("sibling"),
                exe.with_file_name("seatswap-tray.exe")
            );
            fs::remove_file(exe.with_file_name("seatswap-tray.exe")).expect("remove");
            match tray_program(&exe) {
                Err(error @ AutostartError::NoTrayProgram { .. }) => {
                    let missing = exe.with_file_name("seatswap-tray.exe");
                    assert_eq!(
                        error.to_string(),
                        format!("cannot start at login: {} is missing", missing.display())
                    );
                }
                other => panic!("expected NoTrayProgram, got {other:?}"),
            }
        }

        #[test]
        fn apply_writes_replaces_and_removes_the_value() {
            let tmp = tempfile::tempdir().expect("tempdir");
            let exe = programs(&tmp);
            let tray = exe.with_file_name("seatswap-tray.exe");
            let key = TestKey::new();

            apply(true, &exe, &key.path, &[]).expect("on");
            assert_eq!(
                registry::get_string(&key.path, RUN_VALUE).expect("read"),
                Some(format!("\"{}\"", tray.display()))
            );

            let data = tmp.path().join("my data");
            let arguments = vec!["--data-dir".to_string(), data.display().to_string()];
            apply(true, &exe, &key.path, &arguments).expect("on again");
            assert_eq!(
                registry::get_string(&key.path, RUN_VALUE).expect("read"),
                Some(format!(
                    "\"{}\" \"--data-dir\" \"{}\"",
                    tray.display(),
                    data.display()
                ))
            );

            apply(false, &exe, &key.path, &[]).expect("off");
            assert_eq!(
                registry::get_string(&key.path, RUN_VALUE).expect("read"),
                None
            );
            apply(false, &exe, &key.path, &[]).expect("off again is fine");
        }

        #[test]
        fn a_command_over_the_limit_is_refused_and_nothing_is_written() {
            let tmp = tempfile::tempdir().expect("tempdir");
            let exe = programs(&tmp);
            let program = quote_argument(
                exe.with_file_name("seatswap-tray.exe")
                    .to_str()
                    .expect("unicode temp path"),
            );
            // `<program> "<argument>"`: one space and two quotes around the argument.
            let filler = RUN_COMMAND_LIMIT + 1 - program.encode_utf16().count() - 3;
            let exact = vec!["x".repeat(filler - 1)];
            let over = vec!["x".repeat(filler)];
            assert_eq!(
                command_line(&exe.with_file_name("seatswap-tray.exe"), &exact)
                    .expect("260 units fit")
                    .encode_utf16()
                    .count(),
                RUN_COMMAND_LIMIT
            );

            let key = TestKey::new();
            match apply(true, &exe, &key.path, &over) {
                Err(AutostartError::CommandTooLong { length }) => {
                    assert_eq!(length, RUN_COMMAND_LIMIT + 1)
                }
                other => panic!("expected CommandTooLong, got {other:?}"),
            }
            assert_eq!(
                registry::get_string(&key.path, RUN_VALUE).expect("read"),
                None
            );
        }
    }
}
