//! The XDG autostart entry (docs/implementation.md "Autostart").
//!
//! One small `.desktop` file, written through `atomic::write_atomic` like every
//! other file seatswap owns.

use std::ffi::{OsStr, OsString};
use std::fs;
use std::path::{Path, PathBuf};

use seatswap_core::atomic;
use seatswap_core::paths::autostart_file;

#[derive(Debug, thiserror::Error)]
pub(crate) enum AutostartError {
    #[error("cannot find the user config folder: $XDG_CONFIG_HOME and $HOME are not set")]
    NoConfigHome,
    #[error("$HOME is empty or not an absolute path: {0:?}")]
    HomeNotAbsolute(OsString),
    #[error("autostart argument {0} is not an absolute path")]
    RelativePath(PathBuf),
    #[error("{path}: {source}")]
    Io {
        path: PathBuf,
        #[source]
        source: std::io::Error,
    },
}

/// `$XDG_CONFIG_HOME`, else `~/.config`. Following the XDG spec, a relative
/// `$XDG_CONFIG_HOME` is ignored; a `$HOME` that is empty or relative is an
/// error, never quietly replaced.
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
    atomic::write_atomic(&path, entry.as_bytes(), 0o644).map_err(|source| AutostartError::Io {
        path: path.clone(),
        source,
    })
}

/// Apply the `startAtLogin` setting with the executable and flags of this run.
pub(crate) fn apply_autostart(enabled: bool, arguments: &[String]) -> Result<(), String> {
    let exe = std::env::current_exe()
        .map_err(|error| format!("cannot find the seatswap executable: {error}"))?;
    let config_home = config_home().map_err(|error| error.to_string())?;
    apply(enabled, &exe, &config_home, arguments).map_err(|error| error.to_string())
}

#[cfg(test)]
mod tests {
    use super::*;

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
    fn startup_arguments_carry_only_the_flags_that_were_given() {
        assert_eq!(
            startup_arguments(Some(Path::new("/data")), Some(Path::new("/claude")))
                .expect("absolute paths"),
            vec![
                "--data-dir".to_string(),
                "/data".to_string(),
                "--claude-config-dir".to_string(),
                "/claude".to_string(),
            ]
        );
        assert_eq!(
            startup_arguments(None, Some(Path::new("/claude"))).expect("one flag"),
            vec!["--claude-config-dir".to_string(), "/claude".to_string()]
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
