//! Start the tray app at login (docs/implementation.md "Autostart",
//! docs/windows.md §6.5). One entry point, [`apply_autostart`]; only where the
//! entry is stored differs:
//! - Linux: the XDG autostart entry, one small `.desktop` file written through
//!   `atomic::write_atomic` like every other file seatswap owns;
//! - Windows: the value `seatswap` under the `Run` key, which starts
//!   `seatswap-tray.exe` (the program without a console window);
//! - macOS: the per-user LaunchAgent `~/Library/LaunchAgents/dev.seatswap.app.plist`
//!   (`RunAtLoad`, no `KeepAlive`), written through `atomic::write_atomic` only
//!   when it is missing or differs. launchd reads that folder at the next login,
//!   so `launchctl` is never run. A file there without the line
//!   `<!-- X-seatswap-managed=true -->`, or a symbolic link, is not seatswap's
//!   own: it is never overwritten or removed, and the tray app says so.
//!
//! The XDG folder helpers below also serve the Linux menu entry
//! (`desktop_entry.rs`).

#[cfg(not(any(windows, target_os = "macos")))]
use std::ffi::{OsStr, OsString};
#[cfg(not(windows))]
use std::fs;
use std::path::{Path, PathBuf};

#[cfg(not(windows))]
use seatswap_core::atomic::{self, Access};
#[cfg(not(any(windows, target_os = "macos")))]
use seatswap_core::paths::autostart_file;
#[cfg(target_os = "macos")]
use seatswap_core::paths::LAUNCH_AGENT_LABEL;

#[cfg(windows)]
use super::registry;

#[derive(Debug, thiserror::Error)]
pub(crate) enum AutostartError {
    #[cfg(not(any(windows, target_os = "macos")))]
    #[error("cannot find the user folder: ${variable} and $HOME are not set")]
    NoXdgHome { variable: &'static str },
    #[cfg(not(any(windows, target_os = "macos")))]
    #[error("$HOME is empty or not an absolute path: {0:?}")]
    HomeNotAbsolute(OsString),
    #[cfg(not(any(windows, target_os = "macos")))]
    #[error("the desktop entry cannot hold a line break in {value:?}")]
    LineBreak { value: String },
    #[cfg(target_os = "macos")]
    #[error("the LaunchAgent cannot hold the character {character:?} in {value:?}")]
    NotInPlist { character: char, value: String },
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
    #[error("the seatswap program beside the running one is missing: {path}")]
    NoConsoleProgram { path: PathBuf },
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

/// The XDG folder rule both the autostart entry and the Linux menu entry use:
/// `$XDG_<kind>_HOME`, else `$HOME/<fallback>`. Following the XDG spec, a
/// relative or empty `$XDG_<kind>_HOME` is ignored; a `$HOME` that is empty or
/// relative is an error, never quietly replaced.
#[cfg(not(any(windows, target_os = "macos")))]
fn xdg_home_from(
    xdg_home: Option<&OsStr>,
    home: Option<&OsStr>,
    variable: &'static str,
    fallback: &[&str],
) -> Result<PathBuf, AutostartError> {
    if let Some(dir) = xdg_home {
        if !dir.is_empty() {
            let path = PathBuf::from(dir);
            if path.is_absolute() {
                return Ok(path);
            }
        }
    }
    let Some(home) = home else {
        return Err(AutostartError::NoXdgHome { variable });
    };
    if home.is_empty() {
        return Err(AutostartError::HomeNotAbsolute(OsString::new()));
    }
    let mut path = PathBuf::from(home);
    if !path.is_absolute() {
        return Err(AutostartError::HomeNotAbsolute(path.into_os_string()));
    }
    for component in fallback {
        path.push(component);
    }
    Ok(path)
}

/// `$XDG_CONFIG_HOME`, else `~/.config`.
#[cfg(not(any(windows, target_os = "macos")))]
pub(crate) fn config_home_from(
    xdg_config_home: Option<&OsStr>,
    home: Option<&OsStr>,
) -> Result<PathBuf, AutostartError> {
    xdg_home_from(xdg_config_home, home, "XDG_CONFIG_HOME", &[".config"])
}

/// `$XDG_DATA_HOME`, else `~/.local/share`: the folder of the Linux menu entry
/// and its icon.
#[cfg(not(any(windows, target_os = "macos")))]
pub(crate) fn data_home_from(
    xdg_data_home: Option<&OsStr>,
    home: Option<&OsStr>,
) -> Result<PathBuf, AutostartError> {
    xdg_home_from(xdg_data_home, home, "XDG_DATA_HOME", &[".local", "share"])
}

/// `$XDG_CONFIG_HOME`, else `~/.config` from the process environment.
#[cfg(not(any(windows, target_os = "macos")))]
pub(crate) fn config_home() -> Result<PathBuf, AutostartError> {
    config_home_from(
        std::env::var_os("XDG_CONFIG_HOME").as_deref(),
        std::env::var_os("HOME").as_deref(),
    )
}

/// `$XDG_DATA_HOME`, else `~/.local/share` from the process environment.
#[cfg(not(any(windows, target_os = "macos")))]
pub(crate) fn data_home() -> Result<PathBuf, AutostartError> {
    data_home_from(
        std::env::var_os("XDG_DATA_HOME").as_deref(),
        std::env::var_os("HOME").as_deref(),
    )
}

/// `path` as the text the autostart entry holds. The entry is text, so a path
/// that is not valid Unicode cannot be written there unchanged: it is an error,
/// never a path with its odd characters replaced (that would start seatswap
/// with a different folder or program).
fn path_text(path: &Path) -> Result<&str, AutostartError> {
    path.to_str().ok_or_else(|| AutostartError::NotUnicode {
        path: path.to_path_buf(),
    })
}

/// The command-line flags the tray app was started with, so the autostart entry
/// starts the same data folder and Claude folder. The command line made both
/// absolute (`paths::absolute`); a relative one here is an error, and so is one
/// that is not valid Unicode ([`path_text`]).
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
        arguments.push(path_text(path)?.to_string());
    }
    Ok(arguments)
}

/// Quote one `Exec` argument per the Desktop Entry spec: wrap it in double
/// quotes, escape `"`, `` ` ``, `$` and `\` inside the quotes, write `%` as
/// `%%`, and then apply the string escaping (every `\` doubled again, so one
/// literal backslash ends up as four).
#[cfg(not(any(windows, target_os = "macos")))]
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

/// Refuse text that holds a line break: in a desktop entry it would end the
/// line and add a key. The value is named in the error.
#[cfg(not(any(windows, target_os = "macos")))]
fn no_line_break(text: &str) -> Result<(), AutostartError> {
    if text.contains(['\n', '\r']) {
        return Err(AutostartError::LineBreak {
            value: text.to_string(),
        });
    }
    Ok(())
}

#[cfg(not(any(windows, target_os = "macos")))]
pub(crate) fn exec_line(exe: &Path, arguments: &[String]) -> Result<String, AutostartError> {
    let mut parts = Vec::with_capacity(arguments.len() + 1);
    let exe = path_text(exe)?;
    no_line_break(exe)?;
    parts.push(quote_argument(exe));
    for argument in arguments {
        no_line_break(argument)?;
        parts.push(quote_argument(argument));
    }
    Ok(format!("Exec={}", parts.join(" ")))
}

/// Write the autostart entry when `enabled`, remove it otherwise. Removing an
/// entry that is not there is fine.
#[cfg(not(any(windows, target_os = "macos")))]
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
    // Before anything is created: a program path that cannot be written is an error.
    let exec = exec_line(exe, arguments)?;
    if let Some(folder) = path.parent() {
        fs::create_dir_all(folder).map_err(|source| AutostartError::Io {
            path: folder.to_path_buf(),
            source,
        })?;
    }
    let entry = format!(
        "[Desktop Entry]\nType=Application\nName=seatswap\nComment=Switch Claude Code between Claude accounts\n{exec}\nTerminal=false\nX-GNOME-Autostart-enabled=true\n"
    );
    // `write_atomic` already gives a new file the requested mode.
    atomic::write_atomic(&path, entry.as_bytes(), Access::Shared).map_err(|source| {
        AutostartError::Io {
            path: path.clone(),
            source,
        }
    })
}

/// Whether `path` is a symbolic link, including one whose target is missing.
/// `symlink_metadata` does not follow the link, so this sees the link itself.
/// Used by the Linux menu entry and the macOS LaunchAgent.
#[cfg(not(windows))]
pub(crate) fn symlink_at(path: &Path) -> std::io::Result<bool> {
    match fs::symlink_metadata(path) {
        Ok(metadata) => Ok(metadata.file_type().is_symlink()),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(false),
        Err(error) => Err(error),
    }
}

/// Whether `bytes` hold a line (split at `\n`) that is exactly `line`: the
/// marker by which seatswap recognises a file of its own.
#[cfg(not(windows))]
pub(crate) fn has_line(bytes: &[u8], line: &[u8]) -> bool {
    bytes.split(|byte| *byte == b'\n').any(|part| part == line)
}

/// The line that marks a LaunchAgent as seatswap's own, like the menu entry's
/// `X-seatswap-managed=true`. A file without it is left alone.
#[cfg(target_os = "macos")]
const PLIST_MANAGED_LINE: &str = "<!-- X-seatswap-managed=true -->";

/// What [`apply`] did with the LaunchAgent.
#[cfg(target_os = "macos")]
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum LaunchAgent {
    /// Written: it was missing or held another text.
    Written,
    /// Already held this text; not touched.
    Unchanged,
    /// seatswap's own file, removed.
    Removed,
    /// Not there, nothing to remove.
    Absent,
    /// A symbolic link or a file without [`PLIST_MANAGED_LINE`]: not
    /// seatswap's own, neither written nor removed.
    LeftAlone,
}

/// `value` as XML character data, with `& < > " '` escaped. A character an XML
/// 1.0 file cannot hold unchanged (a control character other than tab and line
/// feed, a carriage return, which a reader turns into a line feed, or U+FFFE
/// and U+FFFF) is an error, never dropped or replaced: launchd would start
/// another program or folder.
#[cfg(target_os = "macos")]
fn xml_text(value: &str) -> Result<String, AutostartError> {
    let mut escaped = String::with_capacity(value.len());
    for character in value.chars() {
        match character {
            '&' => escaped.push_str("&amp;"),
            '<' => escaped.push_str("&lt;"),
            '>' => escaped.push_str("&gt;"),
            '"' => escaped.push_str("&quot;"),
            '\'' => escaped.push_str("&apos;"),
            '\t' | '\n' => escaped.push(character),
            '\u{0}'..='\u{1f}' | '\u{fffe}' | '\u{ffff}' => {
                return Err(AutostartError::NotInPlist {
                    character,
                    value: value.to_string(),
                })
            }
            other => escaped.push(other),
        }
    }
    Ok(escaped)
}

/// The LaunchAgent's text: label [`LAUNCH_AGENT_LABEL`], `ProgramArguments`
/// = `exe` then `arguments`, `RunAtLoad`, and no `KeepAlive`, so quitting the
/// tray app does not start it again.
#[cfg(target_os = "macos")]
pub(crate) fn launch_agent_plist(
    exe: &Path,
    arguments: &[String],
) -> Result<String, AutostartError> {
    let mut program_arguments = String::new();
    for argument in std::iter::once(path_text(exe)?).chain(arguments.iter().map(String::as_str)) {
        program_arguments.push_str("\t\t<string>");
        program_arguments.push_str(&xml_text(argument)?);
        program_arguments.push_str("</string>\n");
    }
    Ok(format!(
        "<?xml version=\"1.0\" encoding=\"UTF-8\"?>\n\
         <!DOCTYPE plist PUBLIC \"-//Apple//DTD PLIST 1.0//EN\" \"http://www.apple.com/DTDs/PropertyList-1.0.dtd\">\n\
         {PLIST_MANAGED_LINE}\n\
         <plist version=\"1.0\">\n\
         <dict>\n\
         \t<key>Label</key>\n\
         \t<string>{LAUNCH_AGENT_LABEL}</string>\n\
         \t<key>ProgramArguments</key>\n\
         \t<array>\n\
         {program_arguments}\
         \t</array>\n\
         \t<key>RunAtLoad</key>\n\
         \t<true/>\n\
         </dict>\n\
         </plist>\n"
    ))
}

/// Enabled: write `agent_file` when it is missing or differs. Not enabled:
/// remove it (a file that is not there is fine). Either way a symbolic link or
/// a file without [`PLIST_MANAGED_LINE`] is left alone.
#[cfg(target_os = "macos")]
pub(crate) fn apply(
    enabled: bool,
    exe: &Path,
    agent_file: &Path,
    arguments: &[String],
) -> Result<LaunchAgent, AutostartError> {
    let io_error = |source| AutostartError::Io {
        path: agent_file.to_path_buf(),
        source,
    };
    // Before anything is touched: a program or flag the file cannot hold is an error.
    let plist = if enabled {
        Some(launch_agent_plist(exe, arguments)?)
    } else {
        None
    };
    if symlink_at(agent_file).map_err(io_error)? {
        return Ok(LaunchAgent::LeftAlone);
    }
    let existing = match fs::read(agent_file) {
        Ok(bytes) => Some(bytes),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => None,
        Err(source) => return Err(io_error(source)),
    };
    match (plist, existing) {
        (None, None) => Ok(LaunchAgent::Absent),
        (_, Some(existing)) if !has_line(&existing, PLIST_MANAGED_LINE.as_bytes()) => {
            Ok(LaunchAgent::LeftAlone)
        }
        (None, Some(_)) => match fs::remove_file(agent_file) {
            Ok(()) => Ok(LaunchAgent::Removed),
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(LaunchAgent::Absent),
            Err(source) => Err(io_error(source)),
        },
        (Some(plist), Some(existing)) if existing == plist.as_bytes() => Ok(LaunchAgent::Unchanged),
        (Some(plist), _) => {
            if let Some(folder) = agent_file.parent() {
                fs::create_dir_all(folder).map_err(|source| AutostartError::Io {
                    path: folder.to_path_buf(),
                    source,
                })?;
            }
            // `write_atomic` gives a new file mode 0644: launchd refuses an
            // agent that others can write.
            atomic::write_atomic(agent_file, plist.as_bytes(), Access::Shared).map_err(io_error)?;
            Ok(LaunchAgent::Written)
        }
    }
}

/// The name of the autostart value under the `Run` key.
#[cfg(windows)]
const RUN_VALUE: &str = "seatswap";
/// Microsoft's documented limit for a `Run` command (not tested here).
#[cfg(windows)]
const RUN_COMMAND_LIMIT: usize = 260;

/// The running program: the tray program's folder also holds the console one,
/// and the console program's folder the tray one.
pub(crate) fn running_program() -> Result<PathBuf, String> {
    std::env::current_exe()
        .map_err(|error| format!("cannot find the running seatswap program: {error}"))
}

/// The two seatswap programs beside the running one: the console program (the
/// status line runs it) and the tray program (autostart runs it). The suffix is
/// [`std::env::consts::EXE_SUFFIX`], so this needs no `cfg`.
fn program_beside(current_exe: &Path, console: bool) -> PathBuf {
    let stem = if console { "seatswap" } else { "seatswap-tray" };
    current_exe.with_file_name(format!("{stem}{}", std::env::consts::EXE_SUFFIX))
}

/// The program the entry starts: `seatswap-tray.exe` in the folder of
/// `current_exe`, the same program without a console window.
#[cfg(windows)]
fn tray_program(current_exe: &Path) -> Result<PathBuf, AutostartError> {
    let path = program_beside(current_exe, false);
    if path.is_file() {
        Ok(path)
    } else {
        Err(AutostartError::NoTrayProgram { path })
    }
}

/// The console program beside the running one (`seatswap.exe` on Windows):
/// Claude Code runs it for the status line, also when the tray program is the
/// one making the change.
pub(crate) fn console_program(current_exe: &Path) -> Result<PathBuf, AutostartError> {
    let path = program_beside(current_exe, true);
    if path.is_file() {
        Ok(path)
    } else {
        Err(AutostartError::NoConsoleProgram { path })
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
    let mut parts = Vec::with_capacity(arguments.len() + 1);
    parts.push(quote_argument(path_text(exe)?));
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
    let exe = running_program()?;
    #[cfg(not(any(windows, target_os = "macos")))]
    {
        let config_home = config_home().map_err(|error| error.to_string())?;
        apply(enabled, &exe, &config_home, arguments).map_err(|error| error.to_string())
    }
    #[cfg(target_os = "macos")]
    {
        let agent_file =
            seatswap_core::paths::user_launch_agent_file().map_err(|error| error.to_string())?;
        let done =
            apply(enabled, &exe, &agent_file, arguments).map_err(|error| error.to_string())?;
        // The same line the Linux menu entry prints for a file that is not its own.
        if done == LaunchAgent::LeftAlone {
            eprintln!(
                "seatswap: {} was not made by seatswap; it is left as it is.",
                agent_file.display()
            );
        }
        Ok(())
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

    /// An absolute path that is not valid Unicode.
    fn not_unicode_path() -> PathBuf {
        #[cfg(unix)]
        {
            use std::os::unix::ffi::OsStrExt;
            PathBuf::from(std::ffi::OsStr::from_bytes(b"/opt/seatswap-\xff/data"))
        }
        #[cfg(windows)]
        {
            use std::os::windows::ffi::OsStringExt;
            // `C:\x` followed by half of a two-unit character.
            PathBuf::from(std::ffi::OsString::from_wide(&[
                0x43, 0x3a, 0x5c, 0x78, 0xd800,
            ]))
        }
    }

    #[test]
    fn startup_arguments_refuse_a_path_that_is_not_unicode() {
        let odd = not_unicode_path();
        assert!(odd.is_absolute());
        assert!(odd.to_str().is_none());
        for (data, claude) in [(Some(odd.as_path()), None), (None, Some(odd.as_path()))] {
            match startup_arguments(data, claude) {
                Err(AutostartError::NotUnicode { path }) => assert_eq!(path, odd),
                other => panic!("expected NotUnicode, got {other:?}"),
            }
        }
    }

    #[test]
    fn console_program_finds_the_sibling_and_reports_a_missing_one() {
        let tmp = tempfile::tempdir().expect("tempdir");
        let folder = tmp.path().join("seat swap");
        std::fs::create_dir(&folder).expect("the program folder");
        let name = if cfg!(windows) {
            "seatswap.exe"
        } else {
            "seatswap"
        };
        let exe = folder.join(name);
        std::fs::write(&exe, b"MZ").expect("the console program");
        assert_eq!(console_program(&exe).expect("the sibling"), exe);

        std::fs::remove_file(&exe).expect("remove");
        match console_program(&exe) {
            Err(AutostartError::NoConsoleProgram { path }) => assert_eq!(path, exe),
            other => panic!("expected NoConsoleProgram, got {other:?}"),
        }
    }

    /// The tray program makes the status line's change too, so its folder's
    /// console program is the one Claude Code must run.
    #[test]
    fn console_program_from_the_tray_program_names_the_console_program() {
        let tmp = tempfile::tempdir().expect("tempdir");
        let (tray_name, console_name) = if cfg!(windows) {
            ("seatswap-tray.exe", "seatswap.exe")
        } else {
            ("seatswap-tray", "seatswap")
        };
        let tray = tmp.path().join(tray_name);
        std::fs::write(&tray, b"MZ").expect("the tray program");
        let console = tmp.path().join(console_name);
        std::fs::write(&console, b"MZ").expect("the console program");

        assert_eq!(
            console_program(&tray).expect("the console sibling"),
            console
        );

        std::fs::remove_file(&console).expect("remove");
        match console_program(&tray) {
            Err(AutostartError::NoConsoleProgram { path }) => assert_eq!(path, console),
            other => panic!("expected NoConsoleProgram, got {other:?}"),
        }
    }

    #[cfg(not(any(windows, target_os = "macos")))]
    mod xdg {
        use super::super::*;

        use std::os::unix::fs::PermissionsExt;

        #[test]
        fn a_program_path_that_is_not_unicode_is_refused_and_nothing_is_written() {
            let tmp = tempfile::tempdir().expect("tempdir");
            let exe = super::not_unicode_path();
            match apply(true, &exe, tmp.path(), &[]) {
                Err(AutostartError::NotUnicode { path }) => assert_eq!(path, exe),
                other => panic!("expected NotUnicode, got {other:?}"),
            }
            assert!(
                !tmp.path().join("autostart").exists(),
                "nothing may be created for an entry that cannot be written"
            );
        }

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
        fn exec_line_refuses_a_line_break_in_the_program_or_an_argument() {
            let exe = Path::new("/opt/seat\nswap");
            match exec_line(exe, &[]) {
                Err(AutostartError::LineBreak { value }) => assert_eq!(value, "/opt/seat\nswap"),
                other => panic!("expected LineBreak for the program, got {other:?}"),
            }
            let argument = "/home/u/my\rdata".to_string();
            match exec_line(Path::new("/opt/seatswap"), std::slice::from_ref(&argument)) {
                Err(AutostartError::LineBreak { value }) => assert_eq!(value, argument),
                other => panic!("expected LineBreak for the argument, got {other:?}"),
            }
            // The autostart entry gets the same refusal, before it is written.
            let tmp = tempfile::tempdir().expect("tempdir");
            assert!(apply(
                true,
                Path::new("/opt/seatswap"),
                tmp.path(),
                std::slice::from_ref(&argument)
            )
            .is_err());
            assert!(
                !autostart_file(tmp.path()).exists(),
                "an entry that cannot hold the argument is not written"
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
        fn data_home_follows_the_xdg_rules() {
            assert_eq!(
                data_home_from(Some(OsStr::new("/xdg")), Some(OsStr::new("/home/u")))
                    .expect("absolute xdg"),
                PathBuf::from("/xdg")
            );
            assert_eq!(
                data_home_from(Some(OsStr::new("relative")), Some(OsStr::new("/home/u")))
                    .expect("relative xdg ignored"),
                PathBuf::from("/home/u/.local/share")
            );
            assert!(data_home_from(None, Some(OsStr::new(""))).is_err());
            assert!(data_home_from(None, Some(OsStr::new("relative"))).is_err());
            assert!(data_home_from(Some(OsStr::new("relative")), None).is_err());
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

    #[cfg(target_os = "macos")]
    mod launch_agent {
        use super::super::*;

        use seatswap_core::paths::launch_agent_file;
        use std::os::unix::fs::{symlink, PermissionsExt};
        use std::time::{Duration, SystemTime};

        const EXE: &str = "/Applications/seatswap.app/Contents/MacOS/seatswap";

        fn startup_flags() -> Vec<String> {
            vec![
                "--data-dir".to_string(),
                "/Users/u/Library/Application Support/seatswap".to_string(),
                "--claude-config-dir".to_string(),
                "/Users/u/.claude".to_string(),
            ]
        }

        fn hand_made_agent() -> &'static str {
            "<?xml version=\"1.0\" encoding=\"UTF-8\"?>\n<plist version=\"1.0\">\n<dict>\n\t<key>Label</key>\n\t<string>dev.seatswap.app</string>\n\t<key>Program</key>\n\t<string>/usr/local/bin/seatswap</string>\n</dict>\n</plist>\n"
        }

        #[test]
        fn the_label_is_the_tauri_identifier() {
            let config: serde_json::Value =
                serde_json::from_str(include_str!("../../tauri.conf.json"))
                    .expect("tauri.conf.json is JSON");
            assert_eq!(config["identifier"], LAUNCH_AGENT_LABEL);
        }

        #[test]
        fn the_plist_runs_the_program_with_the_startup_flags_at_load() {
            let text = launch_agent_plist(Path::new(EXE), &startup_flags()).expect("plist");
            assert_eq!(
                text,
                "<?xml version=\"1.0\" encoding=\"UTF-8\"?>\n\
                 <!DOCTYPE plist PUBLIC \"-//Apple//DTD PLIST 1.0//EN\" \"http://www.apple.com/DTDs/PropertyList-1.0.dtd\">\n\
                 <!-- X-seatswap-managed=true -->\n\
                 <plist version=\"1.0\">\n\
                 <dict>\n\
                 \t<key>Label</key>\n\
                 \t<string>dev.seatswap.app</string>\n\
                 \t<key>ProgramArguments</key>\n\
                 \t<array>\n\
                 \t\t<string>/Applications/seatswap.app/Contents/MacOS/seatswap</string>\n\
                 \t\t<string>--data-dir</string>\n\
                 \t\t<string>/Users/u/Library/Application Support/seatswap</string>\n\
                 \t\t<string>--claude-config-dir</string>\n\
                 \t\t<string>/Users/u/.claude</string>\n\
                 \t</array>\n\
                 \t<key>RunAtLoad</key>\n\
                 \t<true/>\n\
                 </dict>\n\
                 </plist>\n"
            );
            assert!(!text.contains("KeepAlive"), "quitting must not restart it");
        }

        #[test]
        fn the_plist_escapes_the_five_xml_characters() {
            let exe = Path::new("/Users/u/Tom & Jerry's <apps>/\"seatswap\"");
            let arguments = vec!["--data-dir".to_string(), "/Users/u/a&b".to_string()];
            let text = launch_agent_plist(exe, &arguments).expect("plist");
            assert!(
                text.contains(
                    "\t\t<string>/Users/u/Tom &amp; Jerry&apos;s &lt;apps&gt;/&quot;seatswap&quot;</string>\n"
                ),
                "{text}"
            );
            assert!(
                text.contains("\t\t<string>/Users/u/a&amp;b</string>\n"),
                "{text}"
            );
            // A tab and a line feed are kept as they are.
            assert_eq!(xml_text("a\tb\nc").expect("tab and LF"), "a\tb\nc");
        }

        #[test]
        fn a_character_xml_cannot_hold_is_refused_and_nothing_is_written() {
            let tmp = tempfile::tempdir().expect("tempdir");
            let agent = launch_agent_file(tmp.path());
            for (argument, character) in [
                ("/Users/u/my\rdata", '\r'),
                ("/Users/u/my\u{1}data", '\u{1}'),
                ("/Users/u/my\u{fffe}data", '\u{fffe}'),
            ] {
                let arguments = vec!["--data-dir".to_string(), argument.to_string()];
                match apply(true, Path::new(EXE), &agent, &arguments) {
                    Err(AutostartError::NotInPlist {
                        character: refused,
                        value,
                    }) => {
                        assert_eq!(refused, character);
                        assert_eq!(value, argument);
                    }
                    other => panic!("expected NotInPlist, got {other:?}"),
                }
            }
            assert!(
                !tmp.path().join("Library").exists(),
                "nothing may be created for an agent that cannot be written"
            );
        }

        #[test]
        fn a_program_path_that_is_not_unicode_is_refused_and_nothing_is_written() {
            let tmp = tempfile::tempdir().expect("tempdir");
            let exe = super::not_unicode_path();
            match apply(true, &exe, &launch_agent_file(tmp.path()), &[]) {
                Err(AutostartError::NotUnicode { path }) => assert_eq!(path, exe),
                other => panic!("expected NotUnicode, got {other:?}"),
            }
            assert!(!tmp.path().join("Library").exists());
        }

        #[test]
        fn on_writes_the_agent_and_off_removes_it() {
            let tmp = tempfile::tempdir().expect("tempdir");
            let agent = launch_agent_file(tmp.path());
            assert!(!agent.exists());

            assert_eq!(
                apply(true, Path::new(EXE), &agent, &startup_flags()).expect("on"),
                LaunchAgent::Written
            );
            assert_eq!(
                fs::read_to_string(&agent).expect("read the agent"),
                launch_agent_plist(Path::new(EXE), &startup_flags()).expect("plist")
            );
            let mode = fs::metadata(&agent).expect("metadata").permissions().mode() & 0o777;
            assert_eq!(mode, 0o644, "launchd refuses an agent others can write");

            assert_eq!(
                apply(false, Path::new(EXE), &agent, &[]).expect("off"),
                LaunchAgent::Removed
            );
            assert!(!agent.exists());
            assert_eq!(
                apply(false, Path::new(EXE), &agent, &[]).expect("off again"),
                LaunchAgent::Absent
            );
        }

        #[test]
        fn an_equal_agent_is_not_touched_and_a_different_one_is_rewritten() {
            let tmp = tempfile::tempdir().expect("tempdir");
            let agent = launch_agent_file(tmp.path());
            apply(true, Path::new(EXE), &agent, &[]).expect("on");

            let old = SystemTime::UNIX_EPOCH + Duration::from_secs(1_600_000_000);
            fs::File::options()
                .write(true)
                .open(&agent)
                .expect("open the agent")
                .set_times(fs::FileTimes::new().set_modified(old))
                .expect("set the mtime");
            assert_eq!(
                apply(true, Path::new(EXE), &agent, &[]).expect("on again"),
                LaunchAgent::Unchanged
            );
            assert_eq!(
                fs::metadata(&agent)
                    .expect("metadata")
                    .modified()
                    .expect("mtime"),
                old,
                "an equal agent is not touched"
            );

            assert_eq!(
                apply(true, Path::new(EXE), &agent, &startup_flags()).expect("new flags"),
                LaunchAgent::Written
            );
            assert_eq!(
                fs::read_to_string(&agent).expect("read the agent"),
                launch_agent_plist(Path::new(EXE), &startup_flags()).expect("plist")
            );
        }

        #[test]
        fn a_hand_made_agent_is_left_alone_on_and_off() {
            let tmp = tempfile::tempdir().expect("tempdir");
            let agent = launch_agent_file(tmp.path());
            fs::create_dir_all(agent.parent().expect("the LaunchAgents folder"))
                .expect("create the folder");
            fs::write(&agent, hand_made_agent()).expect("a hand-made agent");

            for enabled in [true, false] {
                assert_eq!(
                    apply(enabled, Path::new(EXE), &agent, &[]).expect("apply"),
                    LaunchAgent::LeftAlone
                );
                assert_eq!(
                    fs::read_to_string(&agent).expect("read the agent"),
                    hand_made_agent(),
                    "an agent without the managed line is never overwritten or removed"
                );
            }
        }

        #[test]
        fn a_symlinked_agent_is_left_alone_on_and_off() {
            let tmp = tempfile::tempdir().expect("tempdir");
            let agent = launch_agent_file(tmp.path());
            fs::create_dir_all(agent.parent().expect("the LaunchAgents folder"))
                .expect("create the folder");
            // The link points at a file that even carries seatswap's own text.
            let target = tmp.path().join("dotfiles.plist");
            let ours = launch_agent_plist(Path::new(EXE), &[]).expect("plist");
            fs::write(&target, &ours).expect("the link's target");
            symlink(&target, &agent).expect("a symlinked agent");

            for enabled in [true, false] {
                assert_eq!(
                    apply(enabled, Path::new("/other/seatswap"), &agent, &[]).expect("apply"),
                    LaunchAgent::LeftAlone
                );
                assert!(
                    fs::symlink_metadata(&agent)
                        .expect("the link")
                        .file_type()
                        .is_symlink(),
                    "the link stays a link"
                );
                assert_eq!(fs::read_to_string(&target).expect("the target"), ours);
            }

            fs::remove_file(&agent).expect("remove the link");
            symlink(tmp.path().join("missing.plist"), &agent).expect("a dangling link");
            for enabled in [true, false] {
                assert_eq!(
                    apply(enabled, Path::new(EXE), &agent, &[]).expect("apply"),
                    LaunchAgent::LeftAlone
                );
                assert!(fs::symlink_metadata(&agent).is_ok(), "the link stays");
            }
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
        fn a_program_path_that_is_not_unicode_is_refused() {
            let exe = super::not_unicode_path();
            match command_line(&exe, &[]) {
                Err(AutostartError::NotUnicode { path }) => assert_eq!(path, exe),
                other => panic!("expected NotUnicode, got {other:?}"),
            }
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
