//! The live login as Claude Code keeps it on macOS: one generic-password item
//! in the Keychain (spec §2.2), read and written with `/usr/bin/security`.
//!
//! Claude Code itself uses `/usr/bin/security`, so the item's access list
//! trusts that program. Using the same program (by absolute path) means no
//! access prompt, and `add-generic-password -U` updates the item in place, so
//! it keeps its access list and Claude Code keeps its access.
//!
//! The secret never goes on the command line when it fits: it is sent as one
//! line to `security -i` on stdin. A longer line would be split by `security`
//! (see [`INTERACTIVE_LINE_LIMIT`]), so a value that does not fit is passed on
//! the command line instead, as Claude Code does.
//!
//! The names, the command line and the hex coding are compiled on every OS
//! and tested on every OS, except the tests that need a Unix path for the
//! keychain file (a Windows path holds backslashes, which are refused); only
//! macOS runs `security`. Elsewhere every use is
//! [`LiveError::KeychainUnsupported`].
#![cfg_attr(not(target_os = "macos"), allow(dead_code))]

use std::ffi::OsString;
use std::fs;

use super::LiveError;
use crate::paths::KeychainItem;

/// The one program that reads and writes the item (see the module doc).
const SECURITY: &str = "/usr/bin/security";

/// `security`'s exit status for "the specified item could not be found".
const NOT_FOUND: i32 = 44;

/// The longest line, `"\n"` included, that is sent to `security -i`.
///
/// Measured on macOS 26 (Darwin 25.6): `security -i` reads lines of up to 4096
/// bytes including the `"\n"` whole and splits longer ones. The cut-off part
/// runs as a command of its own, so a split `add-generic-password` line wrote
/// its first part, without the keychain argument, into the default keychain.
/// 4032 leaves 64 bytes of margin.
pub(super) const INTERACTIVE_LINE_LIMIT: usize = 4032;

/// Appended to the service name for the backup item.
const BACKUP_SUFFIX: &str = ".seatswap-backup";

/// The item's names, checked so they can be put between double quotes on a
/// `security -i` line.
struct Names<'a> {
    service: &'a str,
    account: &'a str,
    keychain: Option<&'a str>,
}

/// How a value is written.
#[derive(Debug, PartialEq, Eq)]
enum WriteCommand {
    /// One line for `security -i` on stdin; the value is not on any command line.
    Interactive(String),
    /// The arguments after `/usr/bin/security`, for a value whose line would be
    /// longer than [`INTERACTIVE_LINE_LIMIT`].
    ///
    /// The one accepted trade-off: the value's hex is visible in this user's
    /// process list for the moment the command runs, exactly as when Claude
    /// Code itself writes the item.
    Arguments(Vec<OsString>),
}

/// The item seatswap backs `item` up into: `<service>.seatswap-backup`, same
/// account and keychain.
fn backup_item(item: &KeychainItem) -> KeychainItem {
    KeychainItem {
        service: format!("{}{BACKUP_SUFFIX}", item.service),
        account: item.account.clone(),
        keychain: item.keychain.clone(),
    }
}

/// Store `original` in the backup item unless that item exists already.
/// Mirrors [`crate::atomic::backup_once`]: the first original is kept.
pub(super) fn backup_once(item: &KeychainItem, original: &[u8]) -> Result<(), LiveError> {
    let backup = backup_item(item);
    if exists(&backup)? {
        return Ok(());
    }
    write(&backup, original)
}

/// Check the names: none may be empty or hold a double quote, a backslash or a
/// line break (they are not escaped, they are refused), and a named keychain
/// must be an absolute path to an existing file: for a keychain path that does
/// not exist, `security` quietly uses the default keychain instead (observed),
/// and a relative one is looked up by name in the search list.
fn names(item: &KeychainItem) -> Result<Names<'_>, LiveError> {
    let refuse = |detail: String| LiveError::KeychainName {
        service: item.service.clone(),
        detail,
    };
    for (what, value) in [("service", &item.service), ("account", &item.account)] {
        check_text(what, value).map_err(refuse)?;
    }
    let keychain = match &item.keychain {
        None => None,
        Some(path) => {
            let Some(text) = path.to_str() else {
                return Err(refuse(format!(
                    "the keychain path {} is not valid UTF-8",
                    path.display()
                )));
            };
            check_text("keychain path", text).map_err(refuse)?;
            if !path.is_absolute() {
                return Err(refuse(format!(
                    "the keychain path {text:?} must be absolute"
                )));
            }
            match fs::metadata(path) {
                Ok(metadata) if metadata.is_file() => {}
                _ => return Err(LiveError::KeychainFileMissing { path: path.clone() }),
            }
            Some(text)
        }
    };
    Ok(Names {
        service: &item.service,
        account: &item.account,
        keychain,
    })
}

fn check_text(what: &str, value: &str) -> Result<(), String> {
    if value.is_empty() {
        return Err(format!("the {what} is empty"));
    }
    if value.contains(['"', '\\', '\n', '\r']) {
        return Err(format!(
            "the {what} {value:?} holds a double quote, a backslash or a line break, \
             which seatswap does not pass to /usr/bin/security"
        ));
    }
    Ok(())
}

/// The command that stores `data` in `item` (created if missing, updated in
/// place otherwise): a `security -i` line when it fits in
/// [`INTERACTIVE_LINE_LIMIT`], the command line otherwise.
fn write_command(item: &KeychainItem, data: &[u8]) -> Result<WriteCommand, LiveError> {
    let names = names(item)?;
    let hex = hex_encode(data);
    let mut line = format!(
        "add-generic-password -U -a \"{}\" -s \"{}\" -X {hex}",
        names.account, names.service
    );
    if let Some(keychain) = names.keychain {
        line.push_str(&format!(" \"{keychain}\""));
    }
    line.push('\n');
    if fits_interactive(&line) {
        return Ok(WriteCommand::Interactive(line));
    }
    let mut arguments: Vec<OsString> = [
        "add-generic-password",
        "-U",
        "-a",
        names.account,
        "-s",
        names.service,
        "-X",
    ]
    .into_iter()
    .map(OsString::from)
    .collect();
    arguments.push(hex.into());
    if let Some(keychain) = names.keychain {
        arguments.push(keychain.into());
    }
    Ok(WriteCommand::Arguments(arguments))
}

/// Whether `security -i` reads `line` (with its `"\n"`) whole.
fn fits_interactive(line: &str) -> bool {
    line.len() <= INTERACTIVE_LINE_LIMIT
}

fn hex_encode(data: &[u8]) -> String {
    data.iter().map(|byte| format!("{byte:02x}")).collect()
}

/// The data from `find-generic-password -w`'s stdout: without the `"\n"`
/// `security` adds, and decoded when `security` printed it as hex (it does so
/// for data that is not printable). The login is a JSON object, which starts
/// with `{` and so is never all hex digits.
fn password_from_stdout(mut stdout: Vec<u8>) -> Vec<u8> {
    if stdout.last() == Some(&b'\n') {
        stdout.pop();
    }
    match hex_decode(&stdout) {
        Some(decoded) => decoded,
        None => stdout,
    }
}

/// Some when `text` is a non-empty, even-length run of hex digits.
fn hex_decode(text: &[u8]) -> Option<Vec<u8>> {
    if text.is_empty() || !text.len().is_multiple_of(2) {
        return None;
    }
    text.chunks(2)
        .map(|pair| {
            let digits = std::str::from_utf8(pair).ok()?;
            if !digits.bytes().all(|b| b.is_ascii_hexdigit()) {
                return None;
            }
            u8::from_str_radix(digits, 16).ok()
        })
        .collect()
}

/// `security`'s stderr for an error message. Any run of 16 or more hex digits
/// is replaced, so no value (which `security` might echo, e.g. as an unknown
/// command after a split line) reaches a message or a log.
fn stderr_text(stderr: &[u8]) -> String {
    let text = String::from_utf8_lossy(stderr);
    let mut out = String::with_capacity(text.len());
    let mut run = String::new();
    for c in text.chars() {
        if c.is_ascii_hexdigit() {
            run.push(c);
            continue;
        }
        flush_hex_run(&mut out, &mut run);
        out.push(c);
    }
    flush_hex_run(&mut out, &mut run);
    let trimmed = out.trim();
    if trimmed.is_empty() {
        "no message from /usr/bin/security".to_string()
    } else {
        trimmed.to_string()
    }
}

fn flush_hex_run(out: &mut String, run: &mut String) {
    if run.len() >= 16 {
        out.push_str("<hidden>");
    } else {
        out.push_str(run);
    }
    run.clear();
}

#[cfg(target_os = "macos")]
fn io_error(item: &KeychainItem, source: std::io::Error) -> LiveError {
    LiveError::KeychainIo {
        service: item.service.clone(),
        source: std::io::Error::new(source.kind(), format!("running {SECURITY}: {source}")),
    }
}

/// The item's data; None when there is no such item.
#[cfg(target_os = "macos")]
pub(super) fn read(item: &KeychainItem) -> Result<Option<Vec<u8>>, LiveError> {
    use std::process::{Command, Stdio};
    let names = names(item)?;
    let mut command = Command::new(SECURITY);
    command.args([
        "find-generic-password",
        "-a",
        names.account,
        "-s",
        names.service,
        "-w",
    ]);
    if let Some(keychain) = names.keychain {
        command.arg(keychain);
    }
    let output = command
        .stdin(Stdio::null())
        .output()
        .map_err(|source| io_error(item, source))?;
    match output.status.code() {
        Some(0) => Ok(Some(password_from_stdout(output.stdout))),
        Some(NOT_FOUND) => Ok(None),
        _ => Err(LiveError::KeychainRead {
            service: item.service.clone(),
            stderr: stderr_text(&output.stderr),
        }),
    }
}

/// Whether the item exists. Asks for its attributes only, never its data.
#[cfg(target_os = "macos")]
fn exists(item: &KeychainItem) -> Result<bool, LiveError> {
    use std::process::{Command, Stdio};
    let names = names(item)?;
    let mut command = Command::new(SECURITY);
    command.args([
        "find-generic-password",
        "-a",
        names.account,
        "-s",
        names.service,
    ]);
    if let Some(keychain) = names.keychain {
        command.arg(keychain);
    }
    let output = command
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .output()
        .map_err(|source| io_error(item, source))?;
    match output.status.code() {
        Some(0) => Ok(true),
        Some(NOT_FOUND) => Ok(false),
        _ => Err(LiveError::KeychainRead {
            service: item.service.clone(),
            stderr: stderr_text(&output.stderr),
        }),
    }
}

/// Store `data` in the item: created if missing, otherwise updated in place
/// (same service and account), which keeps its access list.
#[cfg(target_os = "macos")]
pub(super) fn write(item: &KeychainItem, data: &[u8]) -> Result<(), LiveError> {
    use std::io::Write;
    use std::process::{Command, Stdio};
    let output = match write_command(item, data)? {
        WriteCommand::Interactive(line) => {
            let mut child = Command::new(SECURITY)
                .arg("-i")
                .stdin(Stdio::piped())
                .stdout(Stdio::null())
                .stderr(Stdio::piped())
                .spawn()
                .map_err(|source| io_error(item, source))?;
            // `security -i` runs the line at its end and exits at the end of
            // stdin, with a status that says whether the command failed.
            let sent = match child.stdin.take() {
                Some(mut stdin) => stdin.write_all(line.as_bytes()),
                None => Err(std::io::Error::other("no stdin to write to")),
            };
            let output = child
                .wait_with_output()
                .map_err(|source| io_error(item, source))?;
            if output.status.success() {
                sent.map_err(|source| io_error(item, source))?;
            }
            output
        }
        WriteCommand::Arguments(arguments) => Command::new(SECURITY)
            .args(arguments)
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .output()
            .map_err(|source| io_error(item, source))?,
    };
    if output.status.success() {
        Ok(())
    } else {
        Err(LiveError::KeychainWrite {
            service: item.service.clone(),
            stderr: stderr_text(&output.stderr),
        })
    }
}

#[cfg(not(target_os = "macos"))]
fn unsupported(item: &KeychainItem) -> LiveError {
    LiveError::KeychainUnsupported {
        service: item.service.clone(),
    }
}

#[cfg(not(target_os = "macos"))]
pub(super) fn read(item: &KeychainItem) -> Result<Option<Vec<u8>>, LiveError> {
    Err(unsupported(item))
}

#[cfg(not(target_os = "macos"))]
fn exists(item: &KeychainItem) -> Result<bool, LiveError> {
    Err(unsupported(item))
}

#[cfg(not(target_os = "macos"))]
pub(super) fn write(item: &KeychainItem, _data: &[u8]) -> Result<(), LiveError> {
    Err(unsupported(item))
}

#[cfg(test)]
pub(super) mod tests {
    use super::*;
    use std::path::PathBuf;

    /// A test item's names; never Claude Code's own service name, so that no
    /// mistake can reach a real login.
    fn test_item(keychain: Option<PathBuf>) -> KeychainItem {
        KeychainItem {
            service: "seatswap-test-credentials-3f9a1c7e".to_string(),
            account: "sam".to_string(),
            keychain,
        }
    }

    /// An existing file that stands for a keychain where nothing runs `security`.
    /// Unix only: a Windows temp path holds backslashes, which `names` refuses.
    #[cfg(unix)]
    fn fake_keychain(tmp: &tempfile::TempDir) -> PathBuf {
        let path = tmp.path().join("fake.keychain-db");
        fs::write(&path, b"").unwrap();
        path
    }

    fn arguments(command: &WriteCommand) -> Vec<String> {
        match command {
            WriteCommand::Arguments(arguments) => arguments
                .iter()
                .map(|a| a.to_str().unwrap().to_string())
                .collect(),
            WriteCommand::Interactive(line) => panic!("expected arguments, got line {line:?}"),
        }
    }

    // Unix only: a Windows temp path holds backslashes, and `"` is not
    // allowed in a Windows file name.
    #[cfg(unix)]
    #[test]
    fn a_short_value_goes_on_stdin_as_one_quoted_line() {
        let tmp = tempfile::tempdir().unwrap();
        let keychain = fake_keychain(&tmp);
        let item = test_item(Some(keychain.clone()));
        let command = write_command(&item, br#"{"a":1}"#).unwrap();
        assert_eq!(
            command,
            WriteCommand::Interactive(format!(
                "add-generic-password -U -a \"sam\" -s \"seatswap-test-credentials-3f9a1c7e\" \
                 -X 7b2261223a317d \"{}\"\n",
                keychain.display()
            ))
        );
    }

    // Unix only: a Windows temp path holds backslashes, and `"` is not
    // allowed in a Windows file name.
    #[cfg(unix)]
    #[test]
    fn the_line_limit_decides_between_stdin_and_arguments() {
        let tmp = tempfile::tempdir().unwrap();
        let item = test_item(Some(fake_keychain(&tmp)));
        // The largest value that still fits on one line.
        let mut size = 0;
        while matches!(
            write_command(&item, &vec![b'a'; size + 1]).unwrap(),
            WriteCommand::Interactive(_)
        ) {
            size += 1;
        }
        let WriteCommand::Interactive(line) = write_command(&item, &vec![b'a'; size]).unwrap()
        else {
            panic!("the largest fitting value must go on stdin");
        };
        assert!(line.len() <= INTERACTIVE_LINE_LIMIT);
        assert!(line.len() + 2 > INTERACTIVE_LINE_LIMIT, "{}", line.len());
        assert!(line.ends_with('\n'));

        let over = vec![b'a'; size + 1];
        let command = write_command(&item, &over).unwrap();
        let keychain = item.keychain.as_ref().unwrap().to_str().unwrap();
        assert_eq!(
            arguments(&command),
            [
                "add-generic-password",
                "-U",
                "-a",
                "sam",
                "-s",
                "seatswap-test-credentials-3f9a1c7e",
                "-X",
                &"61".repeat(size + 1),
                keychain,
            ]
        );
    }

    #[test]
    fn the_default_keychain_has_no_keychain_argument() {
        let item = test_item(None);
        let WriteCommand::Interactive(line) = write_command(&item, b"{}").unwrap() else {
            panic!("a short value goes on stdin");
        };
        assert_eq!(
            line,
            "add-generic-password -U -a \"sam\" -s \"seatswap-test-credentials-3f9a1c7e\" -X 7b7d\n"
        );
        let command = write_command(&item, &vec![b'{'; INTERACTIVE_LINE_LIMIT]).unwrap();
        assert_eq!(arguments(&command).len(), 8, "no keychain argument");
    }

    #[test]
    fn names_that_cannot_be_quoted_are_refused() {
        for (service, account) in [
            ("svc\"x", "sam"),
            ("svc", "s\\am"),
            ("svc\nadd-generic-password", "sam"),
            ("svc", "sam\r"),
            ("", "sam"),
            ("svc", ""),
        ] {
            let item = KeychainItem {
                service: service.to_string(),
                account: account.to_string(),
                keychain: None,
            };
            match write_command(&item, b"{}") {
                Err(LiveError::KeychainName {
                    service: named,
                    detail,
                }) => {
                    assert_eq!(named, service);
                    assert!(
                        detail.contains("double quote, a backslash or a line break")
                            || detail.contains("is empty"),
                        "{detail}"
                    );
                }
                other => panic!("{service:?}/{account:?} must be refused, got {other:?}"),
            }
        }
    }

    // Unix only: a Windows temp path holds backslashes, and `"` is not
    // allowed in a Windows file name.
    #[cfg(unix)]
    #[test]
    fn a_keychain_path_must_be_absolute_quotable_and_exist() {
        let tmp = tempfile::tempdir().unwrap();
        let quoted = tmp.path().join("a\"b.keychain-db");
        fs::write(&quoted, b"").unwrap();
        for path in [quoted, PathBuf::from("login.keychain-db")] {
            match write_command(&test_item(Some(path.clone())), b"{}") {
                Err(LiveError::KeychainName { detail, .. }) => {
                    assert!(detail.contains("keychain path"), "{detail}")
                }
                other => panic!("{} must be refused, got {other:?}", path.display()),
            }
        }

        // security would quietly fall back to the default keychain.
        let missing = tmp.path().join("gone.keychain-db");
        match write_command(&test_item(Some(missing.clone())), b"{}") {
            Err(LiveError::KeychainFileMissing { path }) => assert_eq!(path, missing),
            other => panic!("a missing keychain must be refused, got {other:?}"),
        }
        let folder = tmp.path().to_path_buf();
        assert!(matches!(
            write_command(&test_item(Some(folder)), b"{}"),
            Err(LiveError::KeychainFileMissing { .. })
        ));
    }

    #[test]
    fn hex_output_is_decoded_and_text_is_kept() {
        assert_eq!(password_from_stdout(b"{\"a\":1}\n".to_vec()), b"{\"a\":1}");
        assert_eq!(
            password_from_stdout(b"7b22c3a4223a317d\n".to_vec()),
            "{\"ä\":1}".as_bytes()
        );
        assert_eq!(
            password_from_stdout(b"7b7\n".to_vec()),
            b"7b7",
            "odd length"
        );
        assert_eq!(password_from_stdout(b"\n".to_vec()), b"");
        assert_eq!(hex_encode("{\"ä\":1}".as_bytes()), "7b22c3a4223a317d");
    }

    #[test]
    fn stderr_never_carries_a_value() {
        let hex = hex_encode(format!("sk-ant-ort01-{}", "b".repeat(95)).as_bytes());
        let stderr = format!(
            "security: unknown command \"{hex}\"\n{}: returned 1\n",
            &hex[..40]
        );
        let text = stderr_text(stderr.as_bytes());
        assert_eq!(
            text,
            "security: unknown command \"<hidden>\"\n<hidden>: returned 1"
        );
        assert_eq!(
            stderr_text(b"security: SecKeychainItemCreateFromContent (Claude Code-credentials-45574e61): fail\n"),
            "security: SecKeychainItemCreateFromContent (Claude Code-credentials-45574e61): fail"
        );
        assert_eq!(stderr_text(b""), "no message from /usr/bin/security");
    }

    /// A keychain file of its own in a temporary folder, unlocked and without
    /// auto-lock, deleted on drop. Creating it does not change the user's
    /// keychain search list.
    #[cfg(target_os = "macos")]
    pub(in crate::live) struct ThrowawayKeychain {
        pub(in crate::live) path: PathBuf,
        _dir: tempfile::TempDir,
    }

    #[cfg(target_os = "macos")]
    impl ThrowawayKeychain {
        pub(in crate::live) fn new() -> ThrowawayKeychain {
            let dir = tempfile::tempdir().unwrap();
            let path = dir.path().join("seatswap-test.keychain-db");
            security(&[
                OsString::from("create-keychain"),
                "-p".into(),
                "".into(),
                path.clone().into(),
            ]);
            let keychain = ThrowawayKeychain { path, _dir: dir };
            security(&["set-keychain-settings".into(), keychain.path.clone().into()]);
            assert!(
                keychain.path.is_file(),
                "the throw-away keychain must exist"
            );
            keychain
        }

        /// A test item in this keychain (never Claude Code's service name).
        pub(in crate::live) fn item(&self, service: &str) -> KeychainItem {
            assert!(service.starts_with("seatswap-test-"), "{service}");
            KeychainItem {
                service: service.to_string(),
                account: "sam".to_string(),
                keychain: Some(self.path.clone()),
            }
        }

        /// Store `data` the way Claude Code does (on the command line), not
        /// through the code under test.
        pub(in crate::live) fn put(&self, item: &KeychainItem, data: &[u8]) {
            assert_eq!(item.keychain.as_deref(), Some(self.path.as_path()));
            security(&[
                "add-generic-password".into(),
                "-U".into(),
                "-a".into(),
                item.account.clone().into(),
                "-s".into(),
                item.service.clone().into(),
                "-X".into(),
                hex_encode(data).into(),
                self.path.clone().into(),
            ]);
        }

        /// The item's data as `security` prints it, not through the code under test.
        pub(in crate::live) fn get(&self, item: &KeychainItem) -> Option<Vec<u8>> {
            assert_eq!(item.keychain.as_deref(), Some(self.path.as_path()));
            let output = std::process::Command::new(SECURITY)
                .args([
                    "find-generic-password",
                    "-a",
                    &item.account,
                    "-s",
                    &item.service,
                    "-w",
                ])
                .arg(&self.path)
                .output()
                .unwrap();
            match output.status.code() {
                Some(0) => {
                    let mut data = output.stdout;
                    assert_eq!(data.pop(), Some(b'\n'));
                    Some(hex_decode(&data).unwrap_or(data))
                }
                Some(NOT_FOUND) => None,
                other => panic!("find-generic-password: {other:?}"),
            }
        }
    }

    #[cfg(target_os = "macos")]
    impl Drop for ThrowawayKeychain {
        fn drop(&mut self) {
            let _ = std::process::Command::new(SECURITY)
                .arg("delete-keychain")
                .arg(&self.path)
                .output();
        }
    }

    /// Run `security` with `arguments`, whose last one is always the
    /// throw-away keychain.
    #[cfg(target_os = "macos")]
    fn security(arguments: &[OsString]) {
        let last = std::path::Path::new(arguments.last().unwrap());
        assert!(
            last.ends_with("seatswap-test.keychain-db"),
            "{}",
            last.display()
        );
        let output = std::process::Command::new(SECURITY)
            .args(arguments)
            .stdin(std::process::Stdio::null())
            .output()
            .unwrap();
        assert!(
            output.status.success(),
            "security {:?}: {}",
            arguments.first(),
            String::from_utf8_lossy(&output.stderr)
        );
    }

    #[cfg(target_os = "macos")]
    #[test]
    fn the_longest_line_on_stdin_is_stored_whole() {
        let keychain = ThrowawayKeychain::new();
        let item = keychain.item("seatswap-test-credentials-longest-line");
        let mut size = 2;
        while matches!(
            write_command(&item, &vec![b'x'; size + 1]).unwrap(),
            WriteCommand::Interactive(_)
        ) {
            size += 1;
        }
        let mut data = vec![b'x'; size];
        data[0] = b'"';
        data[size - 1] = b'"';
        assert!(matches!(
            write_command(&item, &data).unwrap(),
            WriteCommand::Interactive(_)
        ));
        write(&item, &data).unwrap();
        assert_eq!(keychain.get(&item), Some(data.clone()));
        assert_eq!(read(&item).unwrap(), Some(data));
        assert!(exists(&item).unwrap());
    }

    #[cfg(target_os = "macos")]
    #[test]
    fn a_value_over_the_line_limit_is_stored_whole_through_arguments() {
        let keychain = ThrowawayKeychain::new();
        let item = keychain.item("seatswap-test-credentials-arguments");
        let data = format!("{{\"k\":\"{}\"}}", "z".repeat(INTERACTIVE_LINE_LIMIT));
        assert!(matches!(
            write_command(&item, data.as_bytes()).unwrap(),
            WriteCommand::Arguments(_)
        ));
        write(&item, data.as_bytes()).unwrap();
        assert_eq!(keychain.get(&item), Some(data.clone().into_bytes()));
        assert_eq!(read(&item).unwrap(), Some(data.into_bytes()));
    }

    #[cfg(target_os = "macos")]
    #[test]
    fn a_missing_item_reads_as_none() {
        let keychain = ThrowawayKeychain::new();
        let item = keychain.item("seatswap-test-credentials-missing");
        assert_eq!(read(&item).unwrap(), None);
        assert!(!exists(&item).unwrap());
    }
}
