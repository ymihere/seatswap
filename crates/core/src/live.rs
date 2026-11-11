//! Reads and writes the live login (`.credentials.json`) and the live identity
//! (`oauthAccount` in `.claude.json`). See spec §2.1, §2.3, §6.2.
//!
//! The caller holds `ClaudeLocks` for every write; this module does not lock.
//! Only the keys in [`LOGIN_KEYS`] and the `oauthAccount` key are replaced:
//! every other key keeps its value and its place (serde_json has
//! `preserve_order` on).

use std::fs;
use std::io;
use std::path::Path;

use serde_json::{Map, Value};

use crate::atomic::{backup_once, write_atomic, Access};
use crate::paths::ClaudePaths;
use crate::types::{Login, OAuthAccount, LOGIN_KEYS};

#[derive(Debug, thiserror::Error)]
pub enum LiveError {
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

/// The live login (the [`LOGIN_KEYS`] of `.credentials.json`). Ok(None) when the
/// file is missing or has no `claudeAiOauth`.
pub fn read_login(paths: &ClaudePaths) -> Result<Option<Login>, LiveError> {
    let path = &paths.credentials_file;
    let bytes = match fs::read(path) {
        Ok(bytes) => bytes,
        Err(e) if e.kind() == io::ErrorKind::NotFound => return Ok(None),
        Err(source) => return Err(io_error(path, source)),
    };
    let credentials = parse_object(path, &bytes)?;
    Ok(Login::from_credentials(&credentials))
}

/// Replace the login in `.credentials.json`: read the whole file (missing → empty
/// object), remove every [`LOGIN_KEYS`] key, insert the login's keys (None = log
/// out: only remove), keep every other key in place and order, write compact JSON
/// without a trailing newline, via [`write_atomic`] (mode 0600 if new). Calls
/// [`backup_once`] first.
pub fn write_login(paths: &ClaudePaths, login: Option<&Login>) -> Result<(), LiveError> {
    if let Some(login) = login {
        for key in login.0.keys() {
            if !LOGIN_KEYS.contains(&key.as_str()) {
                return Err(LiveError::ForeignKey { key: key.clone() });
            }
        }
    }

    let path = &paths.credentials_file;
    let mut credentials = match fs::read(path) {
        Ok(bytes) => parse_object(path, &bytes)?,
        Err(e) if e.kind() == io::ErrorKind::NotFound => Map::new(),
        Err(source) => return Err(io_error(path, source)),
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

    let bytes = json_bytes(path, &Value::Object(credentials), false)?;
    backup_once(path).map_err(|source| io_error(path, source))?;
    write_atomic(path, &bytes, Access::Private).map_err(|source| io_error(path, source))
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

    let bytes = json_bytes(path, &Value::Object(identity), true)?;
    backup_once(path).map_err(|source| io_error(path, source))?;
    write_atomic(path, &bytes, Access::Private).map_err(|source| io_error(path, source))
}

fn parse_object(path: &Path, bytes: &[u8]) -> Result<Map<String, Value>, LiveError> {
    match serde_json::from_slice::<Value>(bytes) {
        Ok(Value::Object(object)) => Ok(object),
        Ok(_) => Err(LiveError::NotObject {
            path: path.to_path_buf(),
        }),
        Err(_) => Err(LiveError::Corrupt {
            path: path.to_path_buf(),
        }),
    }
}

/// Serialize like Claude Code writes its files: compact or 2-space pretty, never
/// with a trailing newline. A `Value` always serializes; the error arm exists so
/// no `unwrap` is needed.
fn json_bytes(path: &Path, value: &Value, pretty: bool) -> Result<Vec<u8>, LiveError> {
    let encoded = if pretty {
        serde_json::to_vec_pretty(value)
    } else {
        serde_json::to_vec(value)
    };
    encoded.map_err(|source| io_error(path, io::Error::new(io::ErrorKind::InvalidData, source)))
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
