//! The Windows registry, under `HKEY_CURRENT_USER` only (docs/windows.md §6.4).
//! The only module that names `winreg`: the autostart entry (`Run` key) and the
//! name notifications are shown under (`AppUserModelId`).
#![cfg(windows)]

use std::io;

use winreg::enums::{HKEY_CURRENT_USER, KEY_SET_VALUE};
use winreg::RegKey;

/// Programs started at login.
pub(crate) const RUN_KEY: &str = r"Software\Microsoft\Windows\CurrentVersion\Run";
/// The identifier in tauri.conf.json; notifications are sent under it.
pub(crate) const APP_ID: &str = "dev.seatswap.app";
/// Where Windows looks up the display name of [`APP_ID`].
pub(crate) const APP_ID_KEY: &str = r"Software\Classes\AppUserModelId\dev.seatswap.app";

/// Set the string value `name` under `HKCU\<key>`, creating the key when missing.
pub(crate) fn set_string(key: &str, name: &str, value: &str) -> io::Result<()> {
    let (opened, _) = current_user().create_subkey(key)?;
    opened.set_value(name, &value)
}

/// The string value `name` under `HKCU\<key>`; a missing key or value is `None`.
/// Only tests read values back; the program only writes and deletes them.
#[cfg(test)]
pub(crate) fn get_string(key: &str, name: &str) -> io::Result<Option<String>> {
    let opened = match current_user().open_subkey_with_flags(key, winreg::enums::KEY_READ) {
        Ok(opened) => opened,
        Err(error) if error.kind() == io::ErrorKind::NotFound => return Ok(None),
        Err(error) => return Err(error),
    };
    match opened.get_value::<String, _>(name) {
        Ok(value) => Ok(Some(value)),
        Err(error) if error.kind() == io::ErrorKind::NotFound => Ok(None),
        Err(error) => Err(error),
    }
}

/// Delete the value `name` under `HKCU\<key>`; a missing key or value is fine.
pub(crate) fn delete_value(key: &str, name: &str) -> io::Result<()> {
    let opened = match current_user().open_subkey_with_flags(key, KEY_SET_VALUE) {
        Ok(opened) => opened,
        Err(error) if error.kind() == io::ErrorKind::NotFound => return Ok(()),
        Err(error) => return Err(error),
    };
    match opened.delete_value(name) {
        Ok(()) => Ok(()),
        Err(error) if error.kind() == io::ErrorKind::NotFound => Ok(()),
        Err(error) => Err(error),
    }
}

fn current_user() -> RegKey {
    RegKey::predef(HKEY_CURRENT_USER)
}

/// A throw-away key `HKCU\Software\seatswap-test-<unique>` for tests, deleted
/// with everything below it on drop (docs/windows.md rule 2.4). Tests never
/// touch any other key.
#[cfg(test)]
pub(crate) struct TestKey {
    pub(crate) path: String,
}

#[cfg(test)]
impl TestKey {
    pub(crate) fn new() -> TestKey {
        use std::sync::atomic::{AtomicU32, Ordering};
        use std::time::{SystemTime, UNIX_EPOCH};
        // The app crate has no uuid dependency; process id, time and a counter
        // make the name unique across parallel tests and runs.
        static COUNTER: AtomicU32 = AtomicU32::new(0);
        let nanos = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .expect("clock after 1970")
            .as_nanos();
        let unique = format!(
            "{:08x}-{nanos:032x}-{:04x}",
            std::process::id(),
            COUNTER.fetch_add(1, Ordering::SeqCst)
        );
        TestKey {
            path: format!(r"Software\seatswap-test-{unique}"),
        }
    }
}

#[cfg(test)]
impl Drop for TestKey {
    fn drop(&mut self) {
        assert!(self.path.starts_with(r"Software\seatswap-test-"));
        match current_user().delete_subkey_all(&self.path) {
            Ok(()) => {}
            Err(error) if error.kind() == io::ErrorKind::NotFound => {}
            Err(error) => panic!("cannot delete the test key {}: {error}", self.path),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn set_then_get() {
        let key = TestKey::new();
        set_string(&key.path, "seatswap", r#""C:\x\seatswap-tray.exe""#).expect("set");
        assert_eq!(
            get_string(&key.path, "seatswap").expect("get").as_deref(),
            Some(r#""C:\x\seatswap-tray.exe""#)
        );
    }

    #[test]
    fn set_overwrites() {
        let key = TestKey::new();
        set_string(&key.path, "DisplayName", "first").expect("first");
        set_string(&key.path, "DisplayName", "seatswap").expect("second");
        assert_eq!(
            get_string(&key.path, "DisplayName")
                .expect("get")
                .as_deref(),
            Some("seatswap")
        );
    }

    #[test]
    fn delete_removes_the_value_and_keeps_the_others() {
        let key = TestKey::new();
        set_string(&key.path, "seatswap", "one").expect("set");
        set_string(&key.path, "other", "two").expect("set the other");
        delete_value(&key.path, "seatswap").expect("delete");
        assert_eq!(get_string(&key.path, "seatswap").expect("get"), None);
        assert_eq!(
            get_string(&key.path, "other")
                .expect("get the other")
                .as_deref(),
            Some("two")
        );
    }

    #[test]
    fn delete_of_a_missing_value_or_key_is_fine() {
        let key = TestKey::new();
        delete_value(&key.path, "seatswap").expect("missing key");
        set_string(&key.path, "other", "two").expect("set");
        delete_value(&key.path, "seatswap").expect("missing value");
    }

    #[test]
    fn get_from_a_missing_key_is_none() {
        let key = TestKey::new();
        assert_eq!(get_string(&key.path, "seatswap").expect("get"), None);
    }
}
