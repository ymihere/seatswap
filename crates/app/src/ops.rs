//! Actions shared by the CLI and the GUI (docs/implementation.md "app: CLI",
//! "app: GUI"). One place for the decisions both front ends must take the same
//! way.

use chrono::{DateTime, Utc};

use seatswap_core::paths::ClaudePaths;
use seatswap_core::store::{Account, Store};
use seatswap_core::switcher::{self, LiveState};

/// Save the live login as an account (or update the stored copy). A login that
/// is missing, ended or still unknown saved nothing and is an error here, so
/// neither front end can report a success that did not happen.
pub(crate) fn save_live_login(
    claude: &ClaudePaths,
    store: &Store,
    label: Option<String>,
    now: DateTime<Utc>,
) -> Result<Account, String> {
    let (state, _) =
        switcher::sync_live(claude, store, Some(label), now).map_err(|error| error.to_string())?;
    match state {
        LiveState::Known { account_id } => {
            let file = store.load().map_err(|error| error.to_string())?;
            file.find(&account_id)
                .cloned()
                .ok_or_else(|| format!("the store has no account {account_id}"))
        }
        LiveState::Ended { .. } => {
            Err("the live login has ended; log in again in Claude Code".to_string())
        }
        LiveState::NoLogin => {
            Err("no live login to save; run Claude Code once and log in".to_string())
        }
        LiveState::Unknown { .. } => {
            Err("the live login could not be saved as an account".to_string())
        }
    }
}

/// The one sentence the CLI and the GUI print after a switch, e.g.
/// "Switched to AC — Acme". A store that cannot be read, or that has no
/// such account, is an error: the raw id is never shown as if it were a name.
pub(crate) fn switch_message(store: &Store, account_id: &str) -> Result<String, String> {
    let file = store.load().map_err(|error| error.to_string())?;
    let account = file
        .find(account_id)
        .ok_or_else(|| format!("the store has no account {account_id}"))?;
    Ok(format!("Switched to {} — {}", account.abbr, account.label))
}

/// The sentence for a switch that happened but whose account list could not
/// name it. Used by the CLI and the GUI so both say the same thing.
pub(crate) fn switched_without_list(error: &str) -> String {
    format!("Switched, but the account list could not be read: {error}")
}

#[cfg(test)]
mod tests {
    use super::*;

    use std::fs;

    use chrono::Utc;
    use seatswap_core::paths::{AppPaths, ClaudePaths};
    use seatswap_core::store::{FileSecrets, Store};

    const ACCOUNT_UUID: &str = "4b7e2f91-3c5a-4d8e-9f1b-6a2c8e4d7b30";
    const ORG_UUID: &str = "e2a6c9d1-7f4b-4a3e-8c5d-1b9f0a7e3c62";
    const EMAIL: &str = "someone@example.com";

    struct Fixture {
        _tmp: tempfile::TempDir,
        claude: ClaudePaths,
        app: AppPaths,
    }

    fn credentials_json(refresh_token: &str) -> serde_json::Value {
        serde_json::json!({
            "mcpOAuth": {
                "plugin:engineering:linear|5f1c2a9be0d34c11": {
                    "serverName": "plugin:engineering:linear",
                    "accessToken": format!("lin_oauth_{}", "e".repeat(64)),
                    "expiresAt": 1_794_400_000_000_i64
                }
            },
            "claudeAiOauth": {
                "accessToken": format!("sk-ant-oat01-{}", "a".repeat(95)),
                "refreshToken": refresh_token,
                "expiresAt": 1_794_356_040_000_i64,
                "refreshTokenExpiresAt": 1_796_772_840_000_i64,
                "scopes": ["user:inference", "user:profile"],
                "subscriptionType": "team",
                "subscriptionTypeSource": "profile",
                "rateLimitTier": "default_claude_max_5x"
            },
            "organizationUuid": ORG_UUID
        })
    }

    fn identity_json() -> serde_json::Value {
        serde_json::json!({
            "numStartups": 41,
            "oauthAccount": {
                "accountUuid": ACCOUNT_UUID,
                "emailAddress": EMAIL,
                "organizationUuid": ORG_UUID,
                "organizationName": "Acme",
                "organizationRole": "admin",
                "billingType": "stripe_subscription",
                "hasExtraUsageEnabled": false
            },
            "projects": {}
        })
    }

    impl Fixture {
        fn new() -> Fixture {
            let tmp = tempfile::tempdir().expect("tempdir");
            let config = tmp.path().join("claude");
            fs::create_dir_all(&config).expect("config folder");
            let claude = ClaudePaths::for_config_dir(&config, tmp.path());
            let app = AppPaths::at(&tmp.path().join("data"));
            fs::create_dir_all(&app.data_dir).expect("data folder");
            fs::write(
                &claude.credentials_file,
                credentials_json(&format!("sk-ant-ort01-{}", "b".repeat(95))).to_string(),
            )
            .expect("credentials");
            fs::write(&claude.identity_file, identity_json().to_string()).expect("identity");
            Fixture {
                _tmp: tmp,
                claude,
                app,
            }
        }

        fn store(&self) -> Store {
            Store::new(
                self.app.clone(),
                Box::new(FileSecrets::new(self.app.secrets_dir())),
            )
        }
    }

    #[test]
    fn save_live_login_returns_the_stored_account() {
        let fixture = Fixture::new();
        let store = fixture.store();
        let account = save_live_login(
            &fixture.claude,
            &store,
            Some("Acme".to_string()),
            Utc::now(),
        )
        .expect("the live login is saved");
        assert_eq!(account.abbr, "AC");
        assert_eq!(account.label, "Acme");
        assert_eq!(account.email.as_deref(), Some(EMAIL));
        let file = store.load().expect("accounts");
        assert_eq!(file.accounts.len(), 1);
    }

    #[test]
    fn save_live_login_without_a_live_login_is_an_error() {
        let fixture = Fixture::new();
        fs::remove_file(&fixture.claude.credentials_file).expect("remove credentials");
        let store = fixture.store();
        let error = save_live_login(&fixture.claude, &store, None, Utc::now())
            .expect_err("nothing to save");
        assert!(error.contains("no live login"), "{error}");
    }

    #[test]
    fn save_live_login_with_an_ended_login_is_an_error() {
        let fixture = Fixture::new();
        fs::write(
            &fixture.claude.credentials_file,
            credentials_json("").to_string(),
        )
        .expect("ended credentials");
        let store = fixture.store();
        let error = save_live_login(&fixture.claude, &store, None, Utc::now())
            .expect_err("the login ended");
        assert!(error.contains("ended"), "{error}");
    }

    #[test]
    fn a_switch_message_never_uses_a_raw_id() {
        let fixture = Fixture::new();
        let store = fixture.store();
        let account = save_live_login(&fixture.claude, &store, None, Utc::now())
            .expect("the live login is saved");
        assert_eq!(
            switch_message(&store, &account.id),
            Ok("Switched to AC — Acme".to_string())
        );

        let error = switch_message(&store, "not-an-account").expect_err("no such account");
        assert_eq!(
            switched_without_list(&error),
            "Switched, but the account list could not be read: the store has no account not-an-account"
        );

        // A store that cannot be read is the same honest sentence.
        fs::write(fixture.app.accounts_file(), b"{ not json").expect("corrupt accounts");
        let error = switch_message(&store, &account.id).expect_err("accounts.json is corrupt");
        let message = switched_without_list(&error);
        assert!(
            message.starts_with("Switched, but the account list could not be read:"),
            "{message}"
        );
        assert!(
            !message.contains(&account.id),
            "the raw id must not be a name: {message}"
        );
    }

    #[test]
    fn the_switched_sentence_has_one_spelling() {
        let fixture = Fixture::new();
        let store = fixture.store();
        let account = save_live_login(&fixture.claude, &store, None, Utc::now())
            .expect("the live login is saved");
        assert_eq!(
            switch_message(&store, &account.id),
            Ok("Switched to AC — Acme".to_string())
        );
    }
}
