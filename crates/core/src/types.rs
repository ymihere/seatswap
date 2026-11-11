//! Value types shared by every module. This file is the contract between modules:
//! change it only on purpose, and never add a second representation of a login.

use serde::{Deserialize, Serialize};
use serde_json::{Map, Value};

/// The keys of `.credentials.json` that together make up one account's login.
/// Claude Code's logout deletes exactly these (2.1.322). A switch saves and
/// restores them as one set; every other key (`mcpOAuth`, unknown keys) is kept.
pub const LOGIN_KEYS: [&str; 7] = [
    "claudeAiOauth",
    "organizationUuid",
    "trustedDeviceToken",
    "trustedDeviceEnrolledAfterRefusalAtMs",
    "enterpriseGateway",
    "designOauth",
    "cloudSessionsOauth",
];

/// One account's login: the subset of `.credentials.json` whose keys are in
/// [`LOGIN_KEYS`], stored and restored whole. Field values are kept exactly as
/// Claude Code wrote them (unknown fields inside them included).
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(transparent)]
pub struct Login(pub Map<String, Value>);

impl Login {
    /// Take the login keys out of a whole `.credentials.json` object.
    /// Returns `None` when the object has no `claudeAiOauth` key.
    pub fn from_credentials(credentials: &Map<String, Value>) -> Option<Login> {
        if !credentials.contains_key("claudeAiOauth") {
            return None;
        }
        let map = LOGIN_KEYS
            .iter()
            .filter_map(|k| credentials.get(*k).map(|v| (k.to_string(), v.clone())))
            .collect();
        Some(Login(map))
    }

    /// The `claudeAiOauth` object. Every `Login` has one (see `from_credentials`).
    pub fn oauth(&self) -> Option<&Map<String, Value>> {
        self.0.get("claudeAiOauth").and_then(Value::as_object)
    }

    fn oauth_str(&self, key: &str) -> Option<&str> {
        self.oauth()?.get(key)?.as_str()
    }

    fn oauth_i64(&self, key: &str) -> Option<i64> {
        self.oauth()?.get(key)?.as_i64()
    }

    pub fn access_token(&self) -> Option<&str> {
        self.oauth_str("accessToken")
    }

    pub fn refresh_token(&self) -> Option<&str> {
        self.oauth_str("refreshToken")
    }

    /// Access token expiry, ms since the epoch.
    pub fn expires_at_ms(&self) -> Option<i64> {
        self.oauth_i64("expiresAt")
    }

    /// The login's fixed end date, ms since the epoch (spec §2.1).
    pub fn refresh_token_expires_at_ms(&self) -> Option<i64> {
        self.oauth_i64("refreshTokenExpiresAt")
    }

    pub fn scopes(&self) -> Vec<String> {
        self.oauth()
            .and_then(|o| o.get("scopes"))
            .and_then(Value::as_array)
            .map(|a| {
                a.iter()
                    .filter_map(|v| v.as_str().map(str::to_string))
                    .collect()
            })
            .unwrap_or_default()
    }

    pub fn subscription_type(&self) -> Option<&str> {
        self.oauth_str("subscriptionType")
    }

    pub fn rate_limit_tier(&self) -> Option<&str> {
        self.oauth_str("rateLimitTier")
    }

    /// True when there is nothing usable to save: an empty refresh token, or
    /// Claude Code marked the login as ended (`loginEnded`).
    pub fn is_ended(&self) -> bool {
        self.refresh_token().is_none_or(str::is_empty)
            || self.oauth().is_some_and(|o| o.contains_key("loginEnded"))
    }
}

/// The `oauthAccount` object of `.claude.json`: the account identity.
/// Kept whole as Claude Code wrote it; accessors read the fields we need.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(transparent)]
pub struct OAuthAccount(pub Map<String, Value>);

impl OAuthAccount {
    fn s(&self, key: &str) -> Option<&str> {
        self.0.get(key)?.as_str()
    }
    pub fn account_uuid(&self) -> Option<&str> {
        self.s("accountUuid")
    }
    pub fn organization_uuid(&self) -> Option<&str> {
        self.s("organizationUuid")
    }
    pub fn email(&self) -> Option<&str> {
        self.s("emailAddress")
    }
    pub fn organization_name(&self) -> Option<&str> {
        self.s("organizationName")
    }
    /// Claude's own name for a personal plan: "Free", "Pro", "Max", "Ultra".
    /// Absent for Team and Enterprise seats.
    pub fn plan_display_name(&self) -> Option<&str> {
        self.s("planDisplayName")
    }
    /// `(accountUuid, organizationUuid)`: what identifies an account (spec §5).
    pub fn identity(&self) -> Option<(&str, &str)> {
        Some((self.account_uuid()?, self.organization_uuid()?))
    }
}
