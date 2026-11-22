//! Reads and writes the live login (`.credentials.json`, or on macOS Claude
//! Code's Keychain item) and the live identity (`oauthAccount` in
//! `.claude.json`). See spec §2.1, §2.2, §2.3, §6.2.
//!
//! The caller holds `ClaudeLocks` for every write; this module does not lock.
//! Only the keys in [`LOGIN_KEYS`] and the `oauthAccount` key are replaced:
//! every other key keeps its value and its place (serde_json has
//! `preserve_order` on). The file and the Keychain item hold the same JSON and
//! share one merge path; only where the bytes come from and go to differs.

mod keychain;

use std::fs;
use std::io;
use std::path::Path;

use serde_json::{Map, Value};

use crate::atomic::{backup_once, write_atomic, Access};
use crate::paths::{ClaudePaths, KeychainItem};
use crate::types::{Login, OAuthAccount, LOGIN_KEYS};

#[derive(Debug, thiserror::Error)]
pub enum LiveError {
    #[error("the Keychain item {service} is not valid JSON; not touching it")]
    KeychainCorrupt { service: String },
    #[error("the Keychain item {service} has no JSON object at the top level")]
    KeychainNotObject { service: String },
    #[error("Keychain item {service}: {detail}")]
    KeychainName { service: String, detail: String },
    #[error("the keychain {path} does not exist; /usr/bin/security would use the default keychain instead")]
    KeychainFileMissing { path: std::path::PathBuf },
    #[error("reading the Keychain item {service} failed: {stderr}")]
    KeychainRead { service: String, stderr: String },
    #[error("writing the Keychain item {service} failed: {stderr}")]
    KeychainWrite { service: String, stderr: String },
    #[error("Keychain item {service}: {source}")]
    KeychainIo {
        service: String,
        #[source]
        source: std::io::Error,
    },
    #[error(
        "the live login is the Keychain item {service}, and the Keychain exists only on macOS"
    )]
    KeychainUnsupported { service: String },
    #[error("{path} is not valid JSON; not touching it")]
    Corrupt { path: std::path::PathBuf },
    #[error("{path} has no JSON object at the top level")]
    NotObject { path: std::path::PathBuf },
    #[error("{path} does not exist; run Claude Code once first")]
    Missing { path: std::path::PathBuf },
    #[error("login holds a key that is not a login key: {key}")]
    ForeignKey { key: String },
    #[error("{path}: {source}")]
    Io {
        path: std::path::PathBuf,
        #[source]
        source: std::io::Error,
    },
}

/// The live login (the [`LOGIN_KEYS`] of `.credentials.json`, or of the
/// Keychain item when `paths.login_keychain` is Some). Ok(None) when the file
/// or item is missing or has no `claudeAiOauth`.
pub fn read_login(paths: &ClaudePaths) -> Result<Option<Login>, LiveError> {
    let store = LoginStore::of(paths);
    let Some(bytes) = store.read()? else {
        return Ok(None);
    };
    let credentials = store.parse(&bytes)?;
    Ok(Login::from_credentials(&credentials))
}

/// Replace the login in `.credentials.json`: read the whole file (missing → empty
/// object), remove every [`LOGIN_KEYS`] key, insert the login's keys (None = log
/// out: only remove), keep every other key in place and order, write compact JSON
/// without a trailing newline, via [`write_atomic`] (mode 0600 if new). Calls
/// [`backup_once`] first.
///
/// When `paths.login_keychain` is Some, the same JSON is read from and written
/// to that Keychain item instead (updated in place, so it keeps its access
/// list), and the backup is the item `<service>.seatswap-backup` (same account
/// and keychain), stored once before the first write.
pub fn write_login(paths: &ClaudePaths, login: Option<&Login>) -> Result<(), LiveError> {
    if let Some(login) = login {
        for key in login.0.keys() {
            if !LOGIN_KEYS.contains(&key.as_str()) {
                return Err(LiveError::ForeignKey { key: key.clone() });
            }
        }
    }

    let store = LoginStore::of(paths);
    let original = store.read()?;
    let mut credentials = match &original {
        Some(bytes) => store.parse(bytes)?,
        None => Map::new(),
    };

    // `shift_remove` (not `remove`) so the keys we keep do not move.
    for key in LOGIN_KEYS {
        credentials.shift_remove(key);
    }
    if let Some(login) = login {
        for (key, value) in &login.0 {
            credentials.insert(key.clone(), value.clone());
        }
    }

    let bytes = json_bytes(&Value::Object(credentials), false).map_err(|e| store.io_error(e))?;
    store.replace(original.as_deref(), &bytes)
}

/// Where the live login's JSON is kept: `.credentials.json`, or Claude Code's
/// Keychain item on macOS (spec §2.2).
enum LoginStore<'a> {
    File(&'a Path),
    Keychain(&'a KeychainItem),
}

impl<'a> LoginStore<'a> {
    fn of(paths: &'a ClaudePaths) -> LoginStore<'a> {
        match &paths.login_keychain {
            Some(item) => LoginStore::Keychain(item),
            None => LoginStore::File(&paths.credentials_file),
        }
    }

    /// The stored bytes; None when the file or the item does not exist.
    fn read(&self) -> Result<Option<Vec<u8>>, LiveError> {
        match self {
            LoginStore::File(path) => match fs::read(path) {
                Ok(bytes) => Ok(Some(bytes)),
                Err(e) if e.kind() == io::ErrorKind::NotFound => Ok(None),
                Err(source) => Err(io_error(path, source)),
            },
            LoginStore::Keychain(item) => keychain::read(item),
        }
    }

    fn parse(&self, bytes: &[u8]) -> Result<Map<String, Value>, LiveError> {
        match self {
            LoginStore::File(path) => parse_object(path, bytes),
            LoginStore::Keychain(item) => match parse_json(bytes) {
                Ok(object) => Ok(object),
                Err(Shape::Corrupt) => Err(LiveError::KeychainCorrupt {
                    service: item.service.clone(),
                }),
                Err(Shape::NotObject) => Err(LiveError::KeychainNotObject {
                    service: item.service.clone(),
                }),
            },
        }
    }

    /// Back up the original once, then store `bytes`. `original` is what
    /// [`read`](Self::read) returned (None: nothing to back up).
    fn replace(&self, original: Option<&[u8]>, bytes: &[u8]) -> Result<(), LiveError> {
        match self {
            LoginStore::File(path) => {
                backup_once(path).map_err(|source| io_error(path, source))?;
                write_atomic(path, bytes, Access::Private).map_err(|source| io_error(path, source))
            }
            LoginStore::Keychain(item) => {
                if let Some(original) = original {
                    keychain::backup_once(item, original)?;
                }
                keychain::write(item, bytes)
            }
        }
    }

    fn io_error(&self, source: io::Error) -> LiveError {
        match self {
            LoginStore::File(path) => io_error(path, source),
            LoginStore::Keychain(item) => LiveError::KeychainIo {
                service: item.service.clone(),
                source,
            },
        }
    }
}

/// `oauthAccount` of `.claude.json`. Ok(None) when the file or the key is
/// missing (`null` counts as missing, as an `Option` would read it).
pub fn read_identity(paths: &ClaudePaths) -> Result<Option<OAuthAccount>, LiveError> {
    let path = &paths.identity_file;
    let bytes = match fs::read(path) {
        Ok(bytes) => bytes,
        Err(e) if e.kind() == io::ErrorKind::NotFound => return Ok(None),
        Err(source) => return Err(io_error(path, source)),
    };
    let identity = parse_object(path, &bytes)?;
    match identity.get("oauthAccount") {
        None | Some(Value::Null) => Ok(None),
        Some(Value::Object(account)) => Ok(Some(OAuthAccount(account.clone()))),
        Some(_) => Err(LiveError::NotObject { path: path.clone() }),
    }
}

/// Replace `oauthAccount` only; the file must exist (Missing otherwise). Pretty
/// JSON with 2-space indent, no trailing newline, other keys in place and order.
/// Calls [`backup_once`] first.
pub fn write_identity(paths: &ClaudePaths, account: &OAuthAccount) -> Result<(), LiveError> {
    let path = &paths.identity_file;
    let bytes = match fs::read(path) {
        Ok(bytes) => bytes,
        Err(e) if e.kind() == io::ErrorKind::NotFound => {
            return Err(LiveError::Missing { path: path.clone() });
        }
        Err(source) => return Err(io_error(path, source)),
    };
    let mut identity = parse_object(path, &bytes)?;
    // Replacing an existing key keeps its position in the file.
    identity.insert("oauthAccount".to_string(), Value::Object(account.0.clone()));

    let bytes = json_bytes(&Value::Object(identity), true).map_err(|e| io_error(path, e))?;
    backup_once(path).map_err(|source| io_error(path, source))?;
    write_atomic(path, &bytes, Access::Private).map_err(|source| io_error(path, source))
}

/// Why stored bytes are not a JSON object.
enum Shape {
    Corrupt,
    NotObject,
}

fn parse_json(bytes: &[u8]) -> Result<Map<String, Value>, Shape> {
    match serde_json::from_slice::<Value>(bytes) {
        Ok(Value::Object(object)) => Ok(object),
        Ok(_) => Err(Shape::NotObject),
        Err(_) => Err(Shape::Corrupt),
    }
}

fn parse_object(path: &Path, bytes: &[u8]) -> Result<Map<String, Value>, LiveError> {
    parse_json(bytes).map_err(|shape| match shape {
        Shape::Corrupt => LiveError::Corrupt {
            path: path.to_path_buf(),
        },
        Shape::NotObject => LiveError::NotObject {
            path: path.to_path_buf(),
        },
    })
}

/// Serialize like Claude Code writes its files: compact or 2-space pretty, never
/// with a trailing newline. A `Value` always serializes; the error arm exists so
/// no `unwrap` is needed.
fn json_bytes(value: &Value, pretty: bool) -> Result<Vec<u8>, io::Error> {
    let encoded = if pretty {
        serde_json::to_vec_pretty(value)
    } else {
        serde_json::to_vec(value)
    };
    encoded.map_err(|source| io::Error::new(io::ErrorKind::InvalidData, source))
}

fn io_error(path: &Path, source: io::Error) -> LiveError {
    LiveError::Io {
        path: path.to_path_buf(),
        source,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;
    #[cfg(unix)]
    use std::os::unix::fs::PermissionsExt;
    use std::path::PathBuf;

    const ACCOUNT_UUID: &str = "4b7e2f91-3c5a-4d8e-9f1b-6a2c8e4d7b30";
    const ORGANIZATION_UUID: &str = "e2a6c9d1-7f4b-4a3e-8c5d-1b9f0a7e3c62";

    fn paths_in(tmp: &tempfile::TempDir) -> ClaudePaths {
        let config = tmp.path().join("cfg");
        fs::create_dir(&config).unwrap();
        ClaudePaths::for_config_dir(&config, tmp.path())
    }

    /// Write `bytes`; on Unix also set mode 0600, as Claude Code does.
    fn write_private(path: &Path, bytes: &[u8]) {
        fs::write(path, bytes).unwrap();
        #[cfg(unix)]
        fs::set_permissions(path, fs::Permissions::from_mode(0o600)).unwrap();
    }

    #[cfg(unix)]
    fn mode(path: &Path) -> u32 {
        fs::metadata(path).unwrap().permissions().mode() & 0o777
    }

    /// The `.credentials.json` shape from docs/implementation.md, one line, plus
    /// one unknown key that a future Claude Code version might add.
    fn credentials_json() -> String {
        format!(
            concat!(
                r#"{{"mcpOAuth":{{"plugin:engineering:linear|5f1c2a9be0d34c11":{{"serverName":"plugin:engineering:linear","#,
                r#""accessToken":"lin_oauth_8d2f4c1b9e7a6d5c4b3a291807f6e5d4c3b2a1908f7e6d5c4b3a291807f6e5d4","#,
                r#""expiresAt":1794400000000}}}},"futureKey":{{"a":1}},"claudeAiOauth":{{"accessToken":"sk-ant-oat01-{}","#,
                r#""refreshToken":"sk-ant-ort01-{}","expiresAt":1794356040000,"refreshTokenExpiresAt":1796772840000,"#,
                r#""scopes":["user:inference","user:profile","user:mcp_servers"],"subscriptionType":"team","#,
                r#""subscriptionTypeSource":"profile","rateLimitTier":"default_claude_max_5x"}},"#,
                r#""organizationUuid":"{}"}}"#
            ),
            "a".repeat(95),
            "b".repeat(95),
            ORGANIZATION_UUID
        )
    }

    fn login(access: &str, refresh: &str) -> Login {
        let oauth = json!({
            "accessToken": access,
            "refreshToken": refresh,
            "expiresAt": 1794356040000_i64,
            "refreshTokenExpiresAt": 1796772840000_i64,
            "scopes": ["user:inference", "user:profile"],
            "subscriptionType": "max",
            "subscriptionTypeSource": "profile",
            "rateLimitTier": "default_claude_max_20x",
        });
        let mut map = Map::new();
        map.insert("claudeAiOauth".to_string(), oauth);
        Login(map)
    }

    fn other_login() -> Login {
        login(
            &format!("sk-ant-oat01-{}", "c".repeat(95)),
            &format!("sk-ant-ort01-{}", "d".repeat(95)),
        )
    }

    fn identity() -> OAuthAccount {
        let mut map = Map::new();
        map.insert("accountUuid".to_string(), json!(ACCOUNT_UUID));
        map.insert("emailAddress".to_string(), json!("someone@example.com"));
        map.insert("organizationUuid".to_string(), json!(ORGANIZATION_UUID));
        map.insert("organizationName".to_string(), json!("Acme"));
        map.insert("organizationRole".to_string(), json!("admin"));
        map.insert("billingType".to_string(), json!("stripe_subscription"));
        map.insert("hasExtraUsageEnabled".to_string(), json!(false));
        OAuthAccount(map)
    }

    #[test]
    fn write_login_keeps_other_keys_and_replaces_the_login() {
        let tmp = tempfile::tempdir().unwrap();
        let paths = paths_in(&tmp);
        let original = credentials_json();
        write_private(&paths.credentials_file, original.as_bytes());
        let before: Value = serde_json::from_str(&original).unwrap();

        let new_login = other_login();
        write_login(&paths, Some(&new_login)).unwrap();

        let bytes = fs::read(&paths.credentials_file).unwrap();
        assert!(!bytes.contains(&b'\n'), "credentials must stay one line");
        #[cfg(unix)]
        assert_eq!(mode(&paths.credentials_file), 0o600);
        let after: Value = serde_json::from_slice(&bytes).unwrap();
        let map = after.as_object().unwrap();

        // Untouched keys keep their value and their order; the login keys moved
        // to the end, as the contract's remove-then-insert says.
        let untouched: Vec<&str> = map
            .keys()
            .map(String::as_str)
            .filter(|key| !LOGIN_KEYS.contains(key))
            .collect();
        assert_eq!(untouched, ["mcpOAuth", "futureKey"]);
        assert_eq!(map["mcpOAuth"], before["mcpOAuth"]);
        assert_eq!(map["futureKey"], json!({"a": 1}));
        assert_eq!(map["claudeAiOauth"], new_login.0["claudeAiOauth"]);
        assert!(!map.contains_key("organizationUuid"));
        assert_eq!(read_login(&paths).unwrap(), Some(new_login));
    }

    #[test]
    fn write_login_none_removes_only_the_login_keys() {
        let tmp = tempfile::tempdir().unwrap();
        let paths = paths_in(&tmp);
        write_private(&paths.credentials_file, credentials_json().as_bytes());
        write_login(&paths, None).unwrap();

        let bytes = fs::read(&paths.credentials_file).unwrap();
        assert!(!bytes.contains(&b'\n'));
        let map: Map<String, Value> = serde_json::from_slice(&bytes).unwrap();
        assert_eq!(
            map.keys().map(String::as_str).collect::<Vec<_>>(),
            ["mcpOAuth", "futureKey"]
        );
        assert_eq!(read_login(&paths).unwrap(), None);
    }

    #[test]
    fn write_login_creates_a_missing_file_with_only_the_login() {
        let tmp = tempfile::tempdir().unwrap();
        let paths = paths_in(&tmp);
        let new_login = login("sk-ant-oat01-fresh", "sk-ant-ort01-fresh");
        write_login(&paths, Some(&new_login)).unwrap();

        let bytes = fs::read(&paths.credentials_file).unwrap();
        assert!(!bytes.contains(&b'\n'));
        #[cfg(unix)]
        assert_eq!(mode(&paths.credentials_file), 0o600);
        let map: Map<String, Value> = serde_json::from_slice(&bytes).unwrap();
        assert_eq!(
            map.keys().map(String::as_str).collect::<Vec<_>>(),
            ["claudeAiOauth"]
        );
        assert_eq!(read_login(&paths).unwrap(), Some(new_login));
        assert!(
            !backup_path(&paths.credentials_file).exists(),
            "nothing to back up"
        );
    }

    #[test]
    fn corrupt_credentials_are_never_overwritten() {
        let tmp = tempfile::tempdir().unwrap();
        let paths = paths_in(&tmp);
        let corrupt: &[u8] = b"{\"claudeAiOauth\": {";
        write_private(&paths.credentials_file, corrupt);

        match write_login(&paths, Some(&other_login())) {
            Err(LiveError::Corrupt { path }) => assert_eq!(path, paths.credentials_file),
            Err(e) => panic!("expected Corrupt, got {e}"),
            Ok(()) => panic!("a corrupt file must not be overwritten"),
        }
        assert_eq!(fs::read(&paths.credentials_file).unwrap(), corrupt);
        assert!(!backup_path(&paths.credentials_file).exists());
    }

    #[test]
    fn a_foreign_key_in_the_login_is_rejected() {
        let tmp = tempfile::tempdir().unwrap();
        let paths = paths_in(&tmp);
        let original = credentials_json();
        write_private(&paths.credentials_file, original.as_bytes());

        let mut map = other_login().0;
        map.insert("mcpOAuth".to_string(), json!({}));
        match write_login(&paths, Some(&Login(map))) {
            Err(LiveError::ForeignKey { key }) => assert_eq!(key, "mcpOAuth"),
            Err(e) => panic!("expected ForeignKey, got {e}"),
            Ok(()) => panic!("a foreign key must be rejected"),
        }
        assert_eq!(
            fs::read(&paths.credentials_file).unwrap(),
            original.as_bytes()
        );
    }

    #[test]
    fn backup_is_made_once_with_the_original_bytes() {
        let tmp = tempfile::tempdir().unwrap();
        let paths = paths_in(&tmp);
        let original = credentials_json();
        write_private(&paths.credentials_file, original.as_bytes());

        write_login(&paths, Some(&other_login())).unwrap();
        let backup = backup_path(&paths.credentials_file);
        assert_eq!(fs::read(&backup).unwrap(), original.as_bytes());
        #[cfg(unix)]
        assert_eq!(mode(&backup), 0o600);

        write_login(&paths, None).unwrap();
        assert_eq!(
            fs::read(&backup).unwrap(),
            original.as_bytes(),
            "the second write must not replace the backup"
        );
    }

    #[test]
    fn read_login_without_oauth_is_none() {
        let tmp = tempfile::tempdir().unwrap();
        let paths = paths_in(&tmp);
        assert_eq!(read_login(&paths).unwrap(), None, "missing file");
        write_private(&paths.credentials_file, br#"{"mcpOAuth":{}}"#);
        assert_eq!(read_login(&paths).unwrap(), None);
    }

    fn backup_path(path: &Path) -> PathBuf {
        let mut name = path.as_os_str().to_os_string();
        name.push(".seatswap-backup");
        PathBuf::from(name)
    }

    fn identity_json() -> String {
        concat!(
            "{\n",
            "  \"numStartups\": 41,\n",
            "  \"oauthAccount\": {\n",
            "    \"accountUuid\": \"4b7e2f91-3c5a-4d8e-9f1b-6a2c8e4d7b30\",\n",
            "    \"emailAddress\": \"old@example.com\",\n",
            "    \"organizationUuid\": \"e2a6c9d1-7f4b-4a3e-8c5d-1b9f0a7e3c62\",\n",
            "    \"organizationName\": \"Old GmbH\"\n",
            "  },\n",
            "  \"projects\": { \"/home/u/x\": { \"allowedTools\": [] } }\n",
            "}"
        )
        .to_string()
    }

    #[test]
    fn write_identity_replaces_only_oauth_account() {
        let tmp = tempfile::tempdir().unwrap();
        let paths = paths_in(&tmp);
        write_private(&paths.identity_file, identity_json().as_bytes());
        let account = identity();
        write_identity(&paths, &account).unwrap();

        let bytes = fs::read(&paths.identity_file).unwrap();
        assert!(!bytes.ends_with(b"\n"), "no trailing newline");
        let text = String::from_utf8(bytes.clone()).unwrap();
        assert!(
            text.starts_with("{\n  \"numStartups\": 41,"),
            "2-space pretty layout kept"
        );
        let map: Map<String, Value> = serde_json::from_slice(&bytes).unwrap();
        assert_eq!(
            map.keys().map(String::as_str).collect::<Vec<_>>(),
            ["numStartups", "oauthAccount", "projects"]
        );
        assert_eq!(map["numStartups"], json!(41));
        assert_eq!(map["oauthAccount"], Value::Object(account.0.clone()));
        assert_eq!(map["projects"], json!({"/home/u/x": {"allowedTools": []}}));
        #[cfg(unix)]
        assert_eq!(mode(&paths.identity_file), 0o600);
        assert_eq!(read_identity(&paths).unwrap(), Some(account));
    }

    /// Paths whose live login is `item` (in a throw-away keychain); the
    /// credentials file of the same folder must stay untouched.
    #[cfg(target_os = "macos")]
    fn keychain_paths(tmp: &tempfile::TempDir, item: &KeychainItem) -> ClaudePaths {
        assert!(item.keychain.is_some(), "tests always name a keychain");
        let mut paths = paths_in(tmp);
        paths.login_keychain = Some(item.clone());
        paths
    }

    #[cfg(target_os = "macos")]
    fn backup_of(item: &KeychainItem) -> KeychainItem {
        KeychainItem {
            service: format!("{}.seatswap-backup", item.service),
            ..item.clone()
        }
    }

    #[cfg(target_os = "macos")]
    #[test]
    fn keychain_write_login_keeps_other_keys_and_replaces_the_login() {
        let keychain = keychain::tests::ThrowawayKeychain::new();
        let item = keychain.item("seatswap-test-credentials-replace");
        let tmp = tempfile::tempdir().unwrap();
        let paths = keychain_paths(&tmp, &item);
        let original = credentials_json();
        keychain.put(&item, original.as_bytes());
        let before: Value = serde_json::from_str(&original).unwrap();

        let new_login = other_login();
        write_login(&paths, Some(&new_login)).unwrap();

        let bytes = keychain.get(&item).unwrap();
        assert!(!bytes.contains(&b'\n'), "the item stays one line of JSON");
        let map: Map<String, Value> = serde_json::from_slice(&bytes).unwrap();
        let untouched: Vec<&str> = map
            .keys()
            .map(String::as_str)
            .filter(|key| !LOGIN_KEYS.contains(key))
            .collect();
        assert_eq!(untouched, ["mcpOAuth", "futureKey"]);
        assert_eq!(map["mcpOAuth"], before["mcpOAuth"]);
        assert_eq!(map["futureKey"], json!({"a": 1}));
        assert_eq!(map["claudeAiOauth"], new_login.0["claudeAiOauth"]);
        assert!(!map.contains_key("organizationUuid"));
        assert_eq!(read_login(&paths).unwrap(), Some(new_login));
        assert!(
            !paths.credentials_file.exists(),
            "the file is not the live login here"
        );
    }

    #[cfg(target_os = "macos")]
    #[test]
    fn keychain_write_login_none_removes_only_the_login_keys() {
        let keychain = keychain::tests::ThrowawayKeychain::new();
        let item = keychain.item("seatswap-test-credentials-logout");
        let tmp = tempfile::tempdir().unwrap();
        let paths = keychain_paths(&tmp, &item);
        keychain.put(&item, credentials_json().as_bytes());

        write_login(&paths, None).unwrap();
        let map: Map<String, Value> =
            serde_json::from_slice(&keychain.get(&item).unwrap()).unwrap();
        assert_eq!(
            map.keys().map(String::as_str).collect::<Vec<_>>(),
            ["mcpOAuth", "futureKey"]
        );
        assert_eq!(read_login(&paths).unwrap(), None);
    }

    #[cfg(target_os = "macos")]
    #[test]
    fn keychain_missing_item_reads_none_and_is_created_without_a_backup() {
        let keychain = keychain::tests::ThrowawayKeychain::new();
        let item = keychain.item("seatswap-test-credentials-fresh");
        let tmp = tempfile::tempdir().unwrap();
        let paths = keychain_paths(&tmp, &item);
        assert_eq!(read_login(&paths).unwrap(), None);

        let new_login = login(
            &format!("sk-ant-oat01-{}", "e".repeat(95)),
            &format!("sk-ant-ort01-{}", "f".repeat(95)),
        );
        write_login(&paths, Some(&new_login)).unwrap();
        let map: Map<String, Value> =
            serde_json::from_slice(&keychain.get(&item).unwrap()).unwrap();
        assert_eq!(
            map.keys().map(String::as_str).collect::<Vec<_>>(),
            ["claudeAiOauth"]
        );
        assert_eq!(read_login(&paths).unwrap(), Some(new_login));
        assert_eq!(keychain.get(&backup_of(&item)), None, "nothing to back up");
    }

    #[cfg(target_os = "macos")]
    #[test]
    fn keychain_corrupt_item_is_never_overwritten() {
        let keychain = keychain::tests::ThrowawayKeychain::new();
        let item = keychain.item("seatswap-test-credentials-corrupt");
        let tmp = tempfile::tempdir().unwrap();
        let paths = keychain_paths(&tmp, &item);
        let corrupt: &[u8] = b"{\"claudeAiOauth\": {";
        keychain.put(&item, corrupt);

        match write_login(&paths, Some(&other_login())) {
            Err(LiveError::KeychainCorrupt { service }) => assert_eq!(service, item.service),
            Err(e) => panic!("expected KeychainCorrupt, got {e}"),
            Ok(()) => panic!("a corrupt item must not be overwritten"),
        }
        assert!(matches!(
            read_login(&paths),
            Err(LiveError::KeychainCorrupt { .. })
        ));
        assert_eq!(keychain.get(&item).as_deref(), Some(corrupt));
        assert_eq!(keychain.get(&backup_of(&item)), None);
    }

    #[cfg(target_os = "macos")]
    #[test]
    fn keychain_foreign_key_in_the_login_is_rejected() {
        let keychain = keychain::tests::ThrowawayKeychain::new();
        let item = keychain.item("seatswap-test-credentials-foreign");
        let tmp = tempfile::tempdir().unwrap();
        let paths = keychain_paths(&tmp, &item);
        let original = credentials_json();
        keychain.put(&item, original.as_bytes());

        let mut map = other_login().0;
        map.insert("mcpOAuth".to_string(), json!({}));
        match write_login(&paths, Some(&Login(map))) {
            Err(LiveError::ForeignKey { key }) => assert_eq!(key, "mcpOAuth"),
            Err(e) => panic!("expected ForeignKey, got {e}"),
            Ok(()) => panic!("a foreign key must be rejected"),
        }
        assert_eq!(keychain.get(&item), Some(original.into_bytes()));
        assert_eq!(keychain.get(&backup_of(&item)), None);
    }

    #[cfg(target_os = "macos")]
    #[test]
    fn keychain_backup_is_made_once_with_the_original_bytes() {
        let keychain = keychain::tests::ThrowawayKeychain::new();
        let item = keychain.item("seatswap-test-credentials-backup");
        let tmp = tempfile::tempdir().unwrap();
        let paths = keychain_paths(&tmp, &item);
        let original = credentials_json();
        keychain.put(&item, original.as_bytes());

        write_login(&paths, Some(&other_login())).unwrap();
        let backup = backup_of(&item);
        assert_eq!(keychain.get(&backup), Some(original.clone().into_bytes()));

        write_login(&paths, None).unwrap();
        assert_eq!(
            keychain.get(&backup),
            Some(original.into_bytes()),
            "the second write must not replace the backup"
        );
    }

    /// Several MCP logins, a non-ASCII name (which `security -w` prints as
    /// hex) and the login: longer than one `security -i` line can carry.
    #[cfg(target_os = "macos")]
    fn long_credentials_json() -> String {
        let mut mcp = Map::new();
        for (index, server) in ["linear", "notion", "github", "slack", "sentry", "figma"]
            .iter()
            .enumerate()
        {
            let key = format!("plugin:engineering:{server}|5f1c2a9be0d34c1{index}");
            mcp.insert(
                key,
                json!({
                    "serverName": format!("plugin:engineering:{server}"),
                    "serverUrl": format!("https://mcp.{server}.example.com/sse"),
                    "accessToken": format!("{server}_oauth_{}", "8d2f4c1b9e7a6d5c".repeat(4)),
                    "refreshToken": format!("{server}_refresh_{}", "4b3a291807f6e5d4".repeat(4)),
                    "expiresAt": 1794400000000_i64,
                    "scope": "read write",
                }),
            );
        }
        let mut credentials = Map::new();
        credentials.insert("mcpOAuth".to_string(), Value::Object(mcp));
        credentials.insert(
            "futureKey".to_string(),
            json!({"organizationName": "Müller & Söhne GmbH"}),
        );
        let oauth = other_login().0["claudeAiOauth"].clone();
        credentials.insert("claudeAiOauth".to_string(), oauth);
        credentials.insert("organizationUuid".to_string(), json!(ORGANIZATION_UUID));
        let text = serde_json::to_string(&Value::Object(credentials)).unwrap();
        assert!(
            text.len() * 2 > keychain::INTERACTIVE_LINE_LIMIT,
            "the test value must be over the line limit"
        );
        text
    }

    #[cfg(target_os = "macos")]
    #[test]
    fn keychain_long_value_is_written_whole() {
        let keychain = keychain::tests::ThrowawayKeychain::new();
        let item = keychain.item("seatswap-test-credentials-long");
        let tmp = tempfile::tempdir().unwrap();
        let paths = keychain_paths(&tmp, &item);
        let original = long_credentials_json();
        keychain.put(&item, original.as_bytes());
        let before: Map<String, Value> = serde_json::from_str(&original).unwrap();
        assert_eq!(
            read_login(&paths).unwrap(),
            Login::from_credentials(&before),
            "the hex that security prints for non-ASCII data is decoded"
        );

        let new_login = login(
            &format!("sk-ant-oat01-{}", "g".repeat(95)),
            &format!("sk-ant-ort01-{}", "h".repeat(95)),
        );
        write_login(&paths, Some(&new_login)).unwrap();
        let after: Map<String, Value> =
            serde_json::from_slice(&keychain.get(&item).unwrap()).unwrap();
        assert_eq!(
            after.keys().map(String::as_str).collect::<Vec<_>>(),
            ["mcpOAuth", "futureKey", "claudeAiOauth"]
        );
        assert_eq!(after["mcpOAuth"], before["mcpOAuth"]);
        assert_eq!(after["futureKey"], before["futureKey"]);
        assert_eq!(read_login(&paths).unwrap(), Some(new_login));
        assert_eq!(
            keychain.get(&backup_of(&item)),
            Some(original.into_bytes()),
            "the long original is backed up whole"
        );
    }

    #[cfg(not(target_os = "macos"))]
    #[test]
    fn a_keychain_login_is_an_error_off_macos() {
        let tmp = tempfile::tempdir().unwrap();
        let mut paths = paths_in(&tmp);
        paths.login_keychain = Some(KeychainItem {
            service: "seatswap-test-credentials-elsewhere".to_string(),
            account: "sam".to_string(),
            keychain: Some(tmp.path().join("never.keychain-db")),
        });
        for result in [
            read_login(&paths).map(|_| ()),
            write_login(&paths, Some(&other_login())),
        ] {
            match result {
                Err(LiveError::KeychainUnsupported { service }) => {
                    assert_eq!(service, "seatswap-test-credentials-elsewhere")
                }
                other => panic!("expected KeychainUnsupported, got {other:?}"),
            }
        }
        assert!(!paths.credentials_file.exists(), "no fallback to the file");
    }

    #[test]
    fn write_identity_needs_an_existing_file() {
        let tmp = tempfile::tempdir().unwrap();
        let paths = paths_in(&tmp);
        match write_identity(&paths, &identity()) {
            Err(LiveError::Missing { path }) => assert_eq!(path, paths.identity_file),
            Err(e) => panic!("expected Missing, got {e}"),
            Ok(()) => panic!("writing an identity must not create the file"),
        }
        assert!(!paths.identity_file.exists());
        assert_eq!(read_identity(&paths).unwrap(), None);
    }
}
