//! The other providers (spec §7, R8): one reader per provider into seatswap's
//! own `Usage` and `Limit` types so cards, colours and history need no second
//! implementation. Ollama's own server is only ever asked for its balance and
//! usage. Codex's `auth.json` is read at every poll and the login it holds is
//! kept in the secrets store; seatswap writes `auth.json` only when switching
//! between two saved Codex logins, under Codex's own two locks, and it writes no
//! login it has not saved first. Google is never asked and its login never read:
//! `agy` hands its numbers to seatswap's status line command. No token is printed or put in an error text.

use std::io;
use std::path::PathBuf;

use chrono::{DateTime, Utc};
use serde::Serialize;

use crate::api::{ApiError, Usage};
use crate::history::{AccountRef, OllamaHourRecord};
use crate::locks::LockError;

pub mod codex;
pub mod google;
pub mod ollama;

/// The `org` of a Codex [`AccountRef`]; its `acc` is the account id.
pub const CODEX_ORG: &str = "codex";
/// The `org` of an Ollama [`AccountRef`]; its `acc` is the same name.
pub const OLLAMA_ORG: &str = "ollama";
/// The `org` of a Google [`AccountRef`]; its `acc` is the account's email.
pub const GOOGLE_ORG: &str = "google";

#[derive(Debug, thiserror::Error)]
pub enum ProviderError {
    #[error("No Codex login found in {path}. If Codex keeps its login in the system keyring, seatswap cannot read it.")]
    NoLogin { path: PathBuf },
    /// R8's text: the user's next step is Codex's own `config.toml`.
    #[error("Codex keeps its login in the system keyring. To switch Codex logins, set cli_auth_credentials_store = \"file\" in Codex's config.toml and sign in again.")]
    KeyringLogin,
    /// An API key is no subscription and is neither saved nor switched (R8).
    #[error("Codex is signed in with an API key. An API key is no subscription; seatswap neither saves nor switches it.")]
    ApiKeyLogin,
    /// A switch to a login seatswap has never stored; `acc` is the account id.
    #[error("No saved Codex login for {acc}. Sign in with codex login once while seatswap runs.")]
    NotSaved { acc: String },
    /// A stored Codex login could not be renewed (R8).
    #[error("{0}")]
    RenewFailed(String),
    /// A saved login became Codex's live login while a poll asked for it; it is
    /// not renewed, because Codex renews the live login itself (R8).
    #[error("this saved login became Codex's live login during the poll; seatswap does not renew the live login")]
    BecameLive,
    /// A file lock could not be taken — seatswap's own app lock, or Codex's
    /// `auth.lock` or `auth.json.lock` during a switch (R8); the text names the
    /// file.
    #[error(transparent)]
    Lock(#[from] LockError),
    /// The secrets store failed; the text names which one it was.
    #[error("the Codex login store: {0}")]
    Store(#[from] crate::store::StoreError),
    #[error("Codex's login has expired. Run codex once to renew it.")]
    LoginExpired,
    #[error("Ollama is not running on {url}.")]
    NotRunning { url: String },
    #[error("Ollama on {url} refused the request (HTTP {status}). Is it signed in?")]
    OllamaRefused { url: String, status: u16 },
    #[error("{0}")]
    Setting(String),
    #[error(transparent)]
    Api(ApiError),
    #[error("Unexpected answer: {0}")]
    Shape(String),
    #[error("cannot read {path}: {source}")]
    Io {
        path: PathBuf,
        #[source]
        source: io::Error,
    },
    /// `agy`'s settings file could not be read or written (spec §7.9).
    #[error(transparent)]
    StatusLine(#[from] crate::claude_settings::ClaudeSettingsError),
    /// Connect was asked while `agy` is not installed.
    #[error("agy is not installed here: {dir} does not exist")]
    NoAgy { dir: PathBuf },
    /// Connect was asked while `agy`'s status line runs another command; it is
    /// never replaced.
    #[error("agy's status line already runs {command}; seatswap does not replace it. To connect, set it to \"seatswap statusline\" in {path}.")]
    AgyStatusLineTaken { command: String, path: PathBuf },
}

/// A provider's credit balance, in US dollars.
#[derive(Debug, Clone, PartialEq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct Credits {
    pub left: f64,
    pub allowance: Option<f64>,
    pub until: Option<DateTime<Utc>>,
    pub purchased: Option<f64>,
}

/// One provider's answer, in the types seatswap already uses for an account.
#[derive(Debug, Clone, PartialEq)]
pub struct ProviderReading {
    pub account: AccountRef,
    pub plan: Option<String>,
    pub usage: Option<Usage>,
    pub credits: Option<Credits>,
    /// The spend of the last 24 hours, when the provider has such a reading:
    /// `None` for Codex, `Some(Err)` when Ollama's usage answer could not be
    /// read (the balance is still in the reading).
    pub spend_24h: Option<Result<f64, String>>,
    /// Ollama's hourly buckets of the same usage answer: `None` for a provider
    /// without hourly buckets (Codex), `Some(Err)` when the buckets could not be
    /// read (the balance and the 24 h spend are still in the reading).
    pub hours: Option<Result<Vec<OllamaHourRecord>, String>>,
}
