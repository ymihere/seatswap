//! The account list (`accounts.json`, no secrets) and each account's login
//! (the OS keyring, or 0600 files when there is none). See spec §5, §8.2, §10.
//!
//! Every path comes from [`AppPaths`] and every file write goes through
//! [`crate::atomic::write_atomic`].

use std::collections::HashSet;
use std::fs;
use std::io;
use std::path::{Path, PathBuf};

use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};
use serde_json::Value;

use crate::atomic::{self, write_atomic, Access};
use crate::history::AccountRef;
use crate::paths::AppPaths;
use crate::types::{Login, OAuthAccount};

/// The colours accounts are assigned from, in order (spec §8.1, §11.3).
pub const PALETTE: [&str; 10] = [
    "#3b82f6", "#f59e0b", "#10b981", "#ef4444", "#8b5cf6", "#ec4899", "#14b8a6", "#f97316",
    "#6366f1", "#84cc16",
];

/// The settings every host shares through the history folder (spec §5,
/// "Shared between hosts"; §10), by their key in the settings file. Every
/// other setting stays on its host. This list is the one place that says which
/// keys are shared.
pub const SHARED_SETTINGS: [&str; 8] = [
    "currency",
    "notifyOnSwitch",
    "notifyUsableAgain",
    "notifyWaiting",
    "notifyWaitingHosts",
    "notifyLoginEnds",
    "statusline",
    "statuslineFeed",
];

/// Keyring service name; the keyring user is the account id (spec §5).
const KEYRING_SERVICE: &str = "seatswap";
/// Throw-away entry used to find out whether a keyring works at all.
const KEYRING_PROBE_USER: &str = "seatswap-probe";
const ACCOUNTS_VERSION: u32 = 1;

/// Everything the store can fail with.
#[derive(Debug, thiserror::Error)]
pub enum StoreError {
    #[error("{path}: {source}")]
    Io {
        path: PathBuf,
        #[source]
        source: io::Error,
    },
    #[error("{path} is not valid JSON; not touching it")]
    Corrupt { path: PathBuf },
    #[error("accounts file version {0} is not supported; this build reads version 1")]
    UnsupportedVersion(u32),
    #[error("invalid abbreviation {0:?}: use 1-2 letters A-Z or digits")]
    InvalidAbbr(String),
    #[error("abbreviation {0:?} is already used by another account or subscription")]
    DuplicateAbbr(String),
    #[error("invalid colour {0:?}: use #rrggbb")]
    InvalidColor(String),
    #[error("invalid label {0:?}: use 1 to 40 characters")]
    InvalidLabel(String),
    #[error("unknown account {0}")]
    UnknownAccount(String),
    #[error("account {email} is already stored")]
    DuplicateAccount { email: String },
    #[error("keyring: {0}")]
    Keyring(String),
    #[error("invalid setting: {0}")]
    InvalidSetting(String),
    #[error("unknown setting {0}")]
    UnknownSetting(String),
    /// A setting's JSON value that is not of the setting's type; `message` is
    /// serde's text.
    #[error("{key}: {message}")]
    SettingValue { key: String, message: String },
    #[error("invalid price {0}: use a number above 0 per month")]
    InvalidPrice(f64),
    #[error("invalid account id {0:?}: expected 32 lower-case hex characters")]
    InvalidId(String),
    /// A stored secret that is valid JSON but not a JSON object, so no login of
    /// any kind can be read from it.
    #[error("{id}: the stored login is not a JSON object")]
    NotLogin { id: String },
    #[error("{0} is not a provider account: its org is neither codex nor ollama")]
    NotProvider(String),
    #[error("Claude Code's account identity (oauthAccount in {path}) is missing or incomplete")]
    MissingIdentity { path: PathBuf },
}

/// One account as stored in `accounts.json`; carries no secret.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct Account {
    /// uuid v4 in simple form: 32 lower-case hex characters.
    pub id: String,
    pub account_uuid: String,
    pub organization_uuid: String,
    /// Free text, 1..=40 characters after trim.
    pub label: String,
    /// 1-2 characters A-Z or 0-9, upper case, unique.
    pub abbr: String,
    /// `#rrggbb`, lower case.
    pub color: String,
    pub email: Option<String>,
    pub org_name: Option<String>,
    /// [`plan_label`], e.g. `"Ultra"` or `"Team Premium"`.
    pub plan: Option<String>,
    /// What the subscription costs per month, in the display currency; `None`
    /// when it has not been entered. An older file without the key loads with
    /// `None`.
    #[serde(default)]
    pub price: Option<f64>,
    /// The currency [`Self::price`] was entered in ([`Settings::currency`] at
    /// that moment); `None` when no price is stored or it was entered before a
    /// currency was set. An older file without the key loads with `None`.
    #[serde(default)]
    pub price_currency: Option<String>,
    /// A copy of `.claude.json`'s `oauthAccount`.
    pub oauth_account: OAuthAccount,
    pub added_at: DateTime<Utc>,
    pub last_used_at: Option<DateTime<Utc>>,
    pub login_state: LoginState,
    /// `refreshTokenExpiresAt` of the stored login, ms since the epoch.
    pub login_ends_at_ms: Option<i64>,
    /// Whether auto-burn may switch to this account (spec §2.6). An older file
    /// without the key loads with `true`.
    #[serde(default = "default_account_auto_burn")]
    pub auto_burn: bool,
}

/// Whether the stored login can still be used (spec §6.2).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub enum LoginState {
    Ok,
    LoggedOut,
}

/// Where an account's login is kept. Chosen once, on first use, and recorded in
/// `accounts.json`, so the logins never end up split between two places.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub enum SecretsBackend {
    Keyring,
    File,
}

/// What the tray text next to the menu accounts shows (spec §10).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub enum TrayText {
    Percent,
    PercentReset,
    Off,
}

/// What the tray icon's disc shows (spec §8.1, §10): the live account's letters,
/// or the percent of the limit the ring shows ([`IconLimit`]), 100 as `99`.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub enum IconText {
    Letters,
    Percent,
}

/// Which limit the tray ring shows (spec §10).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub enum IconLimit {
    Binding,
    Session,
    Week,
}

/// The UI theme (spec §10).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub enum Theme {
    System,
    Light,
    Dark,
}

/// Which hosts' waiting sessions notify (spec §4.1).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub enum WaitingHosts {
    ThisHost,
    AllHosts,
}

/// The status line's layout (spec §5.4): two zones, the gap to the terminal's
/// right edge in columns, and the margin a renderer keeps free.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct StatuslineConfig {
    #[serde(default = "default_statusline_left")]
    pub left: Vec<Segment>,
    #[serde(default = "default_statusline_right")]
    pub right: Vec<Segment>,
    #[serde(default = "default_statusline_margin")]
    pub margin: u16,
}

/// One segment of a status line zone. `prefix` and `suffix` are printed only
/// when the segment has a value; `drop` is the drop order (1 is dropped first,
/// `None` is never dropped); `time_format` is used by the reset segments,
/// `text` by the `Text` segment and `separator` by the `Separator` segment.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct Segment {
    pub kind: SegmentKind,
    #[serde(default = "default_color_rule")]
    pub color: ColorRule,
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub prefix: String,
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub suffix: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub drop: Option<u8>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub time_format: Option<TimeFormat>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub text: Option<String>,
    /// The character a separator segment prints. `None` is a pipe: that is the
    /// documented rule for a layout saved by 1.1.0, which stored no character.
    /// Only a separator segment takes one; saving it on another kind is refused.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub separator: Option<SeparatorChar>,
}

/// The character a separator segment prints (spec §5.4): `│`, `·`, `•`, `/`,
/// `›`, or one space ("blank"). Serialised camelCase (`"pipe"`, `"dot"`, …).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub enum SeparatorChar {
    Pipe,
    Dot,
    Bullet,
    Slash,
    Chevron,
    Blank,
}

impl SeparatorChar {
    /// The character this choice prints.
    pub fn text(self) -> &'static str {
        match self {
            SeparatorChar::Pipe => "│",
            SeparatorChar::Dot => "·",
            SeparatorChar::Bullet => "•",
            SeparatorChar::Slash => "/",
            SeparatorChar::Chevron => "›",
            SeparatorChar::Blank => " ",
        }
    }
}

/// What a status line segment shows (spec §5.4).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub enum SegmentKind {
    Account,
    Plan,
    FiveHourPercent,
    FiveHourReset,
    WeekPercent,
    WeekReset,
    ModelPercent,
    AutoBurn,
    Model,
    Effort,
    Folder,
    GitBranch,
    UserHost,
    Clock,
    Text,
    Separator,
    ContextUsed,
    SessionCost,
    LinesChanged,
}

/// The palette group a kind is shown under (spec §5.4).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "camelCase")]
pub enum PaletteGroup {
    Seatswap,
    ClaudeCode,
    System,
}

/// What the builder's palette knows about one kind (spec §5.4): its page label,
/// its group, the colour rules it takes in the page's order, and whether it
/// takes a time format, takes text, or may be added more than once. This is the
/// one table: Rust's validation and the page both read it.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct KindSpec {
    pub kind: SegmentKind,
    pub label: &'static str,
    pub group: PaletteGroup,
    pub colors: &'static [&'static str],
    pub time: bool,
    pub text: bool,
    pub repeatable: bool,
}

impl SegmentKind {
    /// Every kind in the palette's order, group by group.
    pub const ALL: [SegmentKind; 19] = [
        SegmentKind::Account,
        SegmentKind::Plan,
        SegmentKind::FiveHourPercent,
        SegmentKind::FiveHourReset,
        SegmentKind::WeekPercent,
        SegmentKind::WeekReset,
        SegmentKind::ModelPercent,
        SegmentKind::AutoBurn,
        SegmentKind::Model,
        SegmentKind::Effort,
        SegmentKind::Folder,
        SegmentKind::GitBranch,
        SegmentKind::ContextUsed,
        SegmentKind::SessionCost,
        SegmentKind::LinesChanged,
        SegmentKind::UserHost,
        SegmentKind::Clock,
        SegmentKind::Text,
        SegmentKind::Separator,
    ];

    /// The one table of the builder's kinds.
    pub fn spec(self) -> KindSpec {
        match self {
            SegmentKind::Account => KindSpec {
                kind: self,
                label: "account",
                group: PaletteGroup::Seatswap,
                colors: &[COLOR_NONE, COLOR_ACCOUNT, COLOR_FIXED],
                time: false,
                text: false,
                repeatable: false,
            },
            SegmentKind::Plan => KindSpec {
                kind: self,
                label: "plan",
                group: PaletteGroup::Seatswap,
                colors: &[COLOR_NONE, COLOR_FIXED],
                time: false,
                text: false,
                repeatable: false,
            },
            SegmentKind::FiveHourPercent => KindSpec {
                kind: self,
                label: "5h percent",
                group: PaletteGroup::Seatswap,
                colors: &[COLOR_NONE, COLOR_BY_LOAD, COLOR_FIXED],
                time: false,
                text: false,
                repeatable: false,
            },
            SegmentKind::FiveHourReset => KindSpec {
                kind: self,
                label: "5h reset",
                group: PaletteGroup::Seatswap,
                colors: &[COLOR_NONE, COLOR_FIXED],
                time: true,
                text: false,
                repeatable: false,
            },
            SegmentKind::WeekPercent => KindSpec {
                kind: self,
                label: "week percent",
                group: PaletteGroup::Seatswap,
                colors: &[COLOR_NONE, COLOR_BY_LOAD, COLOR_FIXED],
                time: false,
                text: false,
                repeatable: false,
            },
            SegmentKind::WeekReset => KindSpec {
                kind: self,
                label: "week reset",
                group: PaletteGroup::Seatswap,
                colors: &[COLOR_NONE, COLOR_FIXED],
                time: true,
                text: false,
                repeatable: false,
            },
            SegmentKind::ModelPercent => KindSpec {
                kind: self,
                label: "per-model %",
                group: PaletteGroup::Seatswap,
                colors: &[COLOR_NONE, COLOR_BY_LOAD, COLOR_FIXED],
                time: false,
                text: false,
                repeatable: false,
            },
            SegmentKind::AutoBurn => KindSpec {
                kind: self,
                label: "auto-burn: next",
                group: PaletteGroup::Seatswap,
                colors: &[COLOR_NONE, COLOR_FIXED],
                time: false,
                text: false,
                repeatable: false,
            },
            SegmentKind::Model => KindSpec {
                kind: self,
                label: "model",
                group: PaletteGroup::ClaudeCode,
                colors: &[COLOR_NONE, COLOR_BY_MODEL_FAMILY, COLOR_FIXED],
                time: false,
                text: false,
                repeatable: false,
            },
            SegmentKind::Effort => KindSpec {
                kind: self,
                label: "effort",
                group: PaletteGroup::ClaudeCode,
                colors: &[COLOR_NONE, COLOR_BY_EFFORT, COLOR_FIXED],
                time: false,
                text: false,
                repeatable: false,
            },
            SegmentKind::Folder => KindSpec {
                kind: self,
                label: "folder",
                group: PaletteGroup::ClaudeCode,
                colors: &[COLOR_NONE, COLOR_FIXED],
                time: false,
                text: false,
                repeatable: false,
            },
            SegmentKind::GitBranch => KindSpec {
                kind: self,
                label: "git branch",
                group: PaletteGroup::ClaudeCode,
                colors: &[COLOR_NONE, COLOR_FIXED],
                time: false,
                text: false,
                repeatable: false,
            },
            SegmentKind::ContextUsed => KindSpec {
                kind: self,
                label: "context used",
                group: PaletteGroup::ClaudeCode,
                colors: &[COLOR_NONE, COLOR_FIXED],
                time: false,
                text: false,
                repeatable: false,
            },
            SegmentKind::SessionCost => KindSpec {
                kind: self,
                label: "session cost",
                group: PaletteGroup::ClaudeCode,
                colors: &[COLOR_NONE, COLOR_FIXED],
                time: false,
                text: false,
                repeatable: false,
            },
            SegmentKind::LinesChanged => KindSpec {
                kind: self,
                label: "lines changed",
                group: PaletteGroup::ClaudeCode,
                colors: &[COLOR_NONE, COLOR_FIXED],
                time: false,
                text: false,
                repeatable: false,
            },
            SegmentKind::UserHost => KindSpec {
                kind: self,
                label: "user@host",
                group: PaletteGroup::System,
                colors: &[COLOR_NONE, COLOR_FIXED],
                time: false,
                text: false,
                repeatable: false,
            },
            SegmentKind::Clock => KindSpec {
                kind: self,
                label: "clock",
                group: PaletteGroup::System,
                colors: &[COLOR_NONE, COLOR_FIXED],
                time: false,
                text: false,
                repeatable: false,
            },
            SegmentKind::Text => KindSpec {
                kind: self,
                label: "free text",
                group: PaletteGroup::System,
                colors: &[COLOR_NONE, COLOR_FIXED],
                time: false,
                text: true,
                repeatable: true,
            },
            SegmentKind::Separator => KindSpec {
                kind: self,
                label: "separator",
                group: PaletteGroup::System,
                colors: &[COLOR_NONE, COLOR_FIXED],
                time: false,
                text: false,
                repeatable: true,
            },
        }
    }
}

const COLOR_NONE: &str = "none";
const COLOR_FIXED: &str = "fixed";
const COLOR_BY_LOAD: &str = "byLoad";
const COLOR_BY_MODEL_FAMILY: &str = "byModelFamily";
const COLOR_BY_EFFORT: &str = "byEffort";
const COLOR_ACCOUNT: &str = "account";

/// How a segment is coloured (spec §5.4).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "rule", rename_all = "camelCase")]
pub enum ColorRule {
    None,
    Fixed { color: String },
    ByLoad { warn: u32, bad: u32 },
    ByModelFamily,
    ByEffort,
    Account,
}

impl ColorRule {
    /// The rule's serde tag, which is also the name the table and the page use.
    pub fn name(&self) -> &'static str {
        match self {
            ColorRule::None => COLOR_NONE,
            ColorRule::Fixed { .. } => COLOR_FIXED,
            ColorRule::ByLoad { .. } => COLOR_BY_LOAD,
            ColorRule::ByModelFamily => COLOR_BY_MODEL_FAMILY,
            ColorRule::ByEffort => COLOR_BY_EFFORT,
            ColorRule::Account => COLOR_ACCOUNT,
        }
    }
}

/// How a reset segment shows its time (spec §5.4).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub enum TimeFormat {
    CompactLeft,
    Seatswap,
    Clock,
}

impl StatuslineConfig {
    /// The preset "My current line": what the status line script prints today
    /// (spec §5.1), so turning the builder on is a start, not a loss.
    pub fn current_line() -> StatuslineConfig {
        StatuslineConfig {
            left: vec![
                Segment {
                    kind: SegmentKind::UserHost,
                    color: ColorRule::Fixed {
                        color: "#22c55e".to_string(),
                    },
                    prefix: String::new(),
                    suffix: String::new(),
                    drop: None,
                    time_format: None,
                    text: None,
                    separator: None,
                },
                Segment {
                    kind: SegmentKind::Text,
                    color: ColorRule::None,
                    prefix: String::new(),
                    suffix: String::new(),
                    drop: None,
                    time_format: None,
                    text: Some(":".to_string()),
                    separator: None,
                },
                Segment {
                    kind: SegmentKind::Folder,
                    color: ColorRule::Fixed {
                        color: "#3b82f6".to_string(),
                    },
                    prefix: String::new(),
                    suffix: String::new(),
                    drop: None,
                    time_format: None,
                    text: None,
                    separator: None,
                },
                Segment {
                    kind: SegmentKind::Model,
                    color: ColorRule::ByModelFamily,
                    prefix: " ".to_string(),
                    suffix: String::new(),
                    drop: None,
                    time_format: None,
                    text: None,
                    separator: None,
                },
                Segment {
                    kind: SegmentKind::Effort,
                    color: ColorRule::ByEffort,
                    prefix: " ".to_string(),
                    suffix: String::new(),
                    drop: None,
                    time_format: None,
                    text: None,
                    separator: None,
                },
            ],
            right: vec![
                Segment {
                    kind: SegmentKind::Account,
                    color: ColorRule::Account,
                    prefix: String::new(),
                    suffix: " ".to_string(),
                    drop: None,
                    time_format: None,
                    text: None,
                    separator: None,
                },
                Segment {
                    kind: SegmentKind::FiveHourPercent,
                    color: ColorRule::ByLoad { warn: 50, bad: 80 },
                    prefix: String::new(),
                    suffix: "%".to_string(),
                    drop: Some(2),
                    time_format: None,
                    text: None,
                    separator: None,
                },
                Segment {
                    kind: SegmentKind::FiveHourReset,
                    color: ColorRule::None,
                    prefix: " ".to_string(),
                    suffix: String::new(),
                    drop: Some(1),
                    time_format: Some(TimeFormat::CompactLeft),
                    text: None,
                    separator: None,
                },
                Segment {
                    kind: SegmentKind::WeekPercent,
                    color: ColorRule::ByLoad { warn: 50, bad: 80 },
                    prefix: "  ".to_string(),
                    suffix: "%".to_string(),
                    drop: None,
                    time_format: None,
                    text: None,
                    separator: None,
                },
                Segment {
                    kind: SegmentKind::WeekReset,
                    color: ColorRule::None,
                    prefix: " ".to_string(),
                    suffix: String::new(),
                    drop: Some(1),
                    time_format: Some(TimeFormat::CompactLeft),
                    text: None,
                    separator: None,
                },
            ],
            margin: 4,
        }
    }

    /// The three presets the builder offers, in the page's order.
    pub fn presets() -> Vec<(String, StatuslineConfig)> {
        vec![
            (
                "My current line".to_string(),
                StatuslineConfig::current_line(),
            ),
            ("Limits only".to_string(), StatuslineConfig::limits_only()),
            ("Everything".to_string(), StatuslineConfig::everything()),
        ]
    }

    /// The preset "Limits only": the account and its two limits, without the
    /// folder, model or effort.
    pub fn limits_only() -> StatuslineConfig {
        let line = StatuslineConfig::current_line();
        StatuslineConfig {
            left: Vec::new(),
            right: line.right,
            margin: line.margin,
        }
    }

    /// The preset "Everything": every kind once. The drop numbers are the
    /// mock-up's: the two reset times go first, then the 5 h percent, then the
    /// branch.
    pub fn everything() -> StatuslineConfig {
        fn part(kind: SegmentKind, prefix: &str, suffix: &str, drop: Option<u8>) -> Segment {
            Segment {
                kind,
                color: ColorRule::None,
                prefix: prefix.to_string(),
                suffix: suffix.to_string(),
                drop,
                time_format: None,
                text: None,
                separator: None,
            }
        }

        let mut left = vec![
            part(SegmentKind::UserHost, "", "", None),
            part(SegmentKind::Text, "", "", None),
            part(SegmentKind::Folder, "", "", None),
            part(SegmentKind::GitBranch, " ", "", Some(3)),
            part(SegmentKind::Model, " ", "", None),
            part(SegmentKind::Effort, " ", "", None),
            part(SegmentKind::Plan, " ", "", None),
            part(SegmentKind::ModelPercent, " ", "", None),
            part(SegmentKind::AutoBurn, " ", "", None),
            part(SegmentKind::ContextUsed, " ", "", None),
            part(SegmentKind::SessionCost, " ", "", None),
            part(SegmentKind::LinesChanged, " ", "", None),
            part(SegmentKind::Clock, " ", "", None),
            part(SegmentKind::Separator, " ", " ", None),
        ];
        if let Some(segment) = left.first_mut() {
            segment.color = ColorRule::Fixed {
                color: "#22c55e".to_string(),
            };
        }
        if let Some(segment) = left.get_mut(1) {
            segment.text = Some(":".to_string());
        }
        if let Some(segment) = left.get_mut(2) {
            segment.color = ColorRule::Fixed {
                color: "#3b82f6".to_string(),
            };
        }
        if let Some(segment) = left.get_mut(3) {
            segment.color = ColorRule::Fixed {
                color: "#22c55e".to_string(),
            };
        }
        if let Some(segment) = left.get_mut(4) {
            segment.color = ColorRule::ByModelFamily;
        }
        if let Some(segment) = left.get_mut(5) {
            segment.color = ColorRule::ByEffort;
        }
        let line = StatuslineConfig::current_line();
        StatuslineConfig {
            left,
            right: line.right,
            margin: line.margin,
        }
    }
}

const DEFAULT_POLL_ACTIVE_SECS: u64 = 120;
const DEFAULT_POLL_INACTIVE_SECS: u64 = 900;
const DEFAULT_HOTKEY: &str = "Ctrl+Alt+C";
const DEFAULT_OLLAMA_URL: &str = "http://127.0.0.1:11434";

fn default_tray_text() -> TrayText {
    TrayText::Percent
}

fn default_icon_limit() -> IconLimit {
    IconLimit::Binding
}

fn default_icon_text() -> IconText {
    IconText::Letters
}

fn default_poll_active_secs() -> u64 {
    DEFAULT_POLL_ACTIVE_SECS
}

fn default_poll_inactive_secs() -> u64 {
    DEFAULT_POLL_INACTIVE_SECS
}

fn default_account_auto_burn() -> bool {
    true
}

fn default_auto_burn() -> bool {
    false
}

fn default_suggest_switches() -> bool {
    true
}

fn default_notify_on_switch() -> bool {
    true
}

fn default_mirror_live() -> bool {
    true
}

fn default_refresh_inactive() -> bool {
    true
}

fn default_hotkey() -> String {
    DEFAULT_HOTKEY.to_string()
}

fn default_start_at_login() -> bool {
    true
}

fn default_theme() -> Theme {
    Theme::System
}

fn default_read_sessions() -> bool {
    true
}

fn default_notify_usable_again() -> bool {
    true
}

fn default_notify_waiting() -> bool {
    true
}

fn default_notify_waiting_hosts() -> WaitingHosts {
    WaitingHosts::ThisHost
}

fn default_notify_login_ends() -> bool {
    true
}

fn default_agents_read() -> bool {
    true
}

fn default_agents_switch() -> bool {
    false
}

fn default_statusline_feed() -> bool {
    true
}

fn default_look_up_prices() -> bool {
    true
}

fn default_codex_card() -> bool {
    false
}

fn default_ollama_card() -> bool {
    false
}

fn default_google_card() -> bool {
    true
}

fn default_ollama_url() -> String {
    DEFAULT_OLLAMA_URL.to_string()
}

/// A missing status line field takes the preset's value for that field, read
/// from [`StatuslineConfig::current_line`] so the preset is written once.
fn default_statusline_left() -> Vec<Segment> {
    StatuslineConfig::current_line().left
}

fn default_statusline_right() -> Vec<Segment> {
    StatuslineConfig::current_line().right
}

fn default_statusline_margin() -> u16 {
    StatuslineConfig::current_line().margin
}

/// A segment without a colour rule is printed uncoloured.
fn default_color_rule() -> ColorRule {
    ColorRule::None
}

/// What seatswap remembers about the `statusLine` it wrote in Claude Code's
/// settings (spec §5.4).
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct StatuslineInstall {
    /// The exact value seatswap wrote last.
    pub installed: serde_json::Value,
    /// The exact value the file had before seatswap's first install. `None`
    /// (the key is absent) means the file had no `statusLine` key; a present
    /// `null` is the value `null` and is written back as `null`.
    #[serde(
        default,
        deserialize_with = "deserialize_present_value",
        skip_serializing_if = "Option::is_none"
    )]
    pub previous: Option<serde_json::Value>,
}

/// Read a present JSON value as `Some`, also when it is `null`; a missing field
/// takes the serde default `None` instead, so the two cases stay distinct.
fn deserialize_present_value<'de, D>(deserializer: D) -> Result<Option<serde_json::Value>, D::Error>
where
    D: serde::Deserializer<'de>,
{
    Ok(Some(serde::Deserialize::deserialize(deserializer)?))
}

/// Settings (spec §10). Each field has a serde default, so a file written by an
/// older build still loads with that setting's default; a present but invalid
/// value is rejected by [`Settings::validate`], never silently replaced.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct Settings {
    #[serde(default = "default_tray_text")]
    pub tray_text: TrayText,
    #[serde(default = "default_icon_limit")]
    pub icon_limit: IconLimit,
    #[serde(default = "default_icon_text")]
    pub icon_text: IconText,
    #[serde(default = "default_poll_active_secs")]
    pub poll_active_secs: u64,
    #[serde(default = "default_poll_inactive_secs")]
    pub poll_inactive_secs: u64,
    /// When on, auto-burn switches by itself; default off.
    #[serde(default = "default_auto_burn")]
    pub auto_burn: bool,
    /// When auto-burn is off, whether a switch is suggested; default on.
    #[serde(default = "default_suggest_switches")]
    pub suggest_switches: bool,
    #[serde(default = "default_notify_on_switch")]
    pub notify_on_switch: bool,
    #[serde(default = "default_mirror_live")]
    pub mirror_live: bool,
    #[serde(default = "default_refresh_inactive")]
    pub refresh_inactive: bool,
    #[serde(default = "default_hotkey")]
    pub hotkey: String,
    #[serde(default = "default_start_at_login")]
    pub start_at_login: bool,
    #[serde(default = "default_theme")]
    pub theme: Theme,
    #[serde(default)]
    pub claude_config_dir: Option<PathBuf>,
    /// `None` (absent or `null` in the file) means `<data dir>/history`; a relative
    /// path is refused by [`Settings::validate`].
    #[serde(default)]
    pub history_dir: Option<PathBuf>,
    /// `None` (absent or `null` in the file) means the machine's host name; an empty
    /// text or a name outside [`crate::paths::host_name`]'s rule is refused by
    /// [`Settings::validate`].
    #[serde(default)]
    pub host_name: Option<String>,
    #[serde(default = "default_read_sessions")]
    pub read_sessions: bool,
    #[serde(default = "default_notify_usable_again")]
    pub notify_usable_again: bool,
    #[serde(default = "default_notify_waiting")]
    pub notify_waiting: bool,
    #[serde(default = "default_notify_waiting_hosts")]
    pub notify_waiting_hosts: WaitingHosts,
    /// Send the login warnings (5 days, 1 day, ended) while on (spec §3,
    /// Notifications); default on, like the other notify settings.
    #[serde(default = "default_notify_login_ends")]
    pub notify_login_ends: bool,
    #[serde(default = "default_agents_read")]
    pub agents_read: bool,
    #[serde(default = "default_agents_switch")]
    pub agents_switch: bool,
    /// When on, `seatswap statusline` writes the payload's live numbers to
    /// `feed.json` for the tray app (spec §5.3); default on.
    #[serde(default = "default_statusline_feed")]
    pub statusline_feed: bool,
    /// The status line's layout; absent in an older file, so the preset loads.
    #[serde(default = "StatuslineConfig::current_line")]
    pub statusline: StatuslineConfig,
    /// While this time is in the future, `seatswap statusline` records the
    /// field names Claude Code sends to `statusline-keys.json` (spec §5.4);
    /// `None` (absent or `null`) records nothing.
    #[serde(default)]
    pub statusline_record_until: Option<DateTime<Utc>>,
    /// The `statusLine` seatswap wrote last and the value the file had before
    /// its first install (spec §5.4); `None` (absent or `null`) means seatswap
    /// has no record, so there is nothing to restore.
    #[serde(default)]
    pub statusline_install: Option<StatuslineInstall>,
    /// The currency every amount is shown in, three capital letters (spec
    /// §1.4); `None` until it is set, and never assumed.
    #[serde(default)]
    pub currency: Option<String>,
    /// When on, seatswap looks up API prices and the dollar rate (spec §1.4.1);
    /// default on.
    #[serde(default = "default_look_up_prices")]
    pub look_up_prices: bool,
    /// When on, the pop-up shows a read-only Codex card (spec §7.2); default off.
    #[serde(default = "default_codex_card")]
    pub codex_card: bool,
    /// The Codex folder to read; `None` (absent or `null` in the file) means
    /// `$CODEX_HOME`, else `~/.codex`. A relative path is refused by
    /// [`Settings::validate`].
    #[serde(default)]
    pub codex_home: Option<PathBuf>,
    /// When on, the pop-up shows a read-only Ollama card (spec §7.1); default off.
    #[serde(default = "default_ollama_card")]
    pub ollama_card: bool,
    /// The Ollama server the card asks (spec §7.1); default
    /// `"http://127.0.0.1:11434"`. Anything but http(s) with a host is refused by
    /// [`Settings::validate`].
    #[serde(default = "default_ollama_url")]
    pub ollama_url: String,
    /// When on, the pop-up shows a Google card as soon as `agy` has handed the
    /// status line command an account (spec §7.9); default on.
    #[serde(default = "default_google_card")]
    pub google_card: bool,
}

impl Default for Settings {
    fn default() -> Self {
        Settings {
            tray_text: default_tray_text(),
            icon_limit: default_icon_limit(),
            icon_text: default_icon_text(),
            poll_active_secs: default_poll_active_secs(),
            poll_inactive_secs: default_poll_inactive_secs(),
            auto_burn: default_auto_burn(),
            suggest_switches: default_suggest_switches(),
            notify_on_switch: default_notify_on_switch(),
            mirror_live: default_mirror_live(),
            refresh_inactive: default_refresh_inactive(),
            hotkey: default_hotkey(),
            start_at_login: default_start_at_login(),
            theme: default_theme(),
            claude_config_dir: None,
            history_dir: None,
            host_name: None,
            read_sessions: default_read_sessions(),
            notify_usable_again: default_notify_usable_again(),
            notify_waiting: default_notify_waiting(),
            notify_waiting_hosts: default_notify_waiting_hosts(),
            notify_login_ends: default_notify_login_ends(),
            agents_read: default_agents_read(),
            agents_switch: default_agents_switch(),
            statusline_feed: default_statusline_feed(),
            statusline: StatuslineConfig::current_line(),
            statusline_record_until: None,
            statusline_install: None,
            currency: None,
            look_up_prices: default_look_up_prices(),
            codex_card: default_codex_card(),
            codex_home: None,
            ollama_card: default_ollama_card(),
            ollama_url: default_ollama_url(),
            google_card: default_google_card(),
        }
    }
}

impl Settings {
    /// Check the ranges the spec fixes: poll intervals >= 30s, a currency of
    /// three capital letters, and the status line's rules.
    pub fn validate(&self) -> Result<(), StoreError> {
        if self.poll_active_secs < 30 {
            return Err(StoreError::InvalidSetting(format!(
                "pollActiveSecs must be at least 30s, got {}",
                self.poll_active_secs
            )));
        }
        if self.poll_inactive_secs < 30 {
            return Err(StoreError::InvalidSetting(format!(
                "pollInactiveSecs must be at least 30s, got {}",
                self.poll_inactive_secs
            )));
        }
        if let Some(dir) = &self.claude_config_dir {
            if !dir.is_absolute() {
                return Err(StoreError::InvalidSetting(format!(
                    "claudeConfigDir must be an absolute path, got {}",
                    dir.display()
                )));
            }
        }
        if let Some(dir) = &self.codex_home {
            if !dir.is_absolute() {
                return Err(StoreError::InvalidSetting(format!(
                    "codexHome must be an absolute path, got {}",
                    dir.display()
                )));
            }
        }
        if let Some(dir) = &self.history_dir {
            crate::paths::check_history_dir(dir)
                .map_err(|error| StoreError::InvalidSetting(error.to_string()))?;
        }
        if let Some(name) = &self.host_name {
            crate::paths::host_name(Some(name))
                .map_err(|e| StoreError::InvalidSetting(e.to_string()))?;
        }
        if let Some(currency) = &self.currency {
            validate_currency(currency)?;
        }
        validate_ollama_url(&self.ollama_url)?;
        validate_statusline(&self.statusline)?;
        Ok(())
    }

    /// These settings with the one setting `key` (its camelCase name in the
    /// settings file) replaced by `value`, checked as a whole by
    /// [`Self::validate`]. The one way to change a setting by its key: the
    /// settings page's save and the shared settings from other hosts
    /// ([`crate::shared::apply`]) both come here. A key the settings do not
    /// have is [`StoreError::UnknownSetting`], a value of the wrong type
    /// [`StoreError::SettingValue`] naming the key.
    pub fn with_setting(
        &self,
        key: &str,
        value: serde_json::Value,
    ) -> Result<Settings, StoreError> {
        let mut object = self.as_json()?;
        let entries = object.as_object_mut().ok_or_else(|| {
            StoreError::InvalidSetting("the settings are not a JSON object".to_string())
        })?;
        if !entries.contains_key(key) {
            return Err(StoreError::UnknownSetting(key.to_string()));
        }
        entries.insert(key.to_string(), value);
        let merged: Settings =
            serde_json::from_value(object).map_err(|error| StoreError::SettingValue {
                key: key.to_string(),
                message: error.to_string(),
            })?;
        merged.validate()?;
        Ok(merged)
    }

    /// One setting's value as the settings file holds it, by its camelCase
    /// key; [`StoreError::UnknownSetting`] for a key the settings do not have.
    pub fn value_of(&self, key: &str) -> Result<serde_json::Value, StoreError> {
        self.as_json()?
            .get(key)
            .cloned()
            .ok_or_else(|| StoreError::UnknownSetting(key.to_string()))
    }

    fn as_json(&self) -> Result<serde_json::Value, StoreError> {
        serde_json::to_value(self)
            .map_err(|error| StoreError::InvalidSetting(format!("settings: {error}")))
    }
}

/// Check an `ollamaUrl`: it must start with `http://` or `https://`, name a
/// host — a DNS name, an IPv4 address or a bracketed IPv6 address — and contain
/// no whitespace or control character anywhere: no HTTP client can use such a
/// URL, so it is refused here and not at the request. User info (`user:pass@`)
/// is refused without quoting the URL, which may hold a password. It must have
/// no query string (`?…`), no fragment (`#…`) and no empty path segment (`//`
/// after the host): the reader appends `/api/…` to the base, which such a URL
/// would mangle. A port (`:8443`), a path (`/base`), an IPv6 host (`[::1]`) and
/// a single trailing slash are fine. Anything else is
/// [`StoreError::InvalidSetting`] naming the field and the value, never a
/// fallback.
pub fn validate_ollama_url(url: &str) -> Result<(), StoreError> {
    if url.chars().any(|c| c.is_whitespace() || c.is_control()) {
        return Err(StoreError::InvalidSetting(format!(
            "ollamaUrl must not contain a whitespace or control character, got {url:?}"
        )));
    }
    let rest = match url
        .strip_prefix("http://")
        .or_else(|| url.strip_prefix("https://"))
    {
        Some(rest) => rest,
        None => {
            return Err(StoreError::InvalidSetting(format!(
                "ollamaUrl must start with http:// or https://, got {url:?}"
            )))
        }
    };
    let authority = rest.split(['/', '?', '#']).next().unwrap_or("");
    if authority.contains('@') {
        return Err(StoreError::InvalidSetting(
            "ollamaUrl must not contain a user name or password".to_string(),
        ));
    }
    if !host_is_real(authority) {
        return Err(StoreError::InvalidSetting(format!(
            "ollamaUrl must name a host: a name, an IPv4 address or a bracketed IPv6 address, got {url:?}"
        )));
    }
    if url.contains('?') {
        return Err(StoreError::InvalidSetting(format!(
            "ollamaUrl must not have a query string, got {url:?}"
        )));
    }
    if url.contains('#') {
        return Err(StoreError::InvalidSetting(format!(
            "ollamaUrl must not have a fragment, got {url:?}"
        )));
    }
    let path = &rest[authority.len()..];
    if path.contains("//") {
        return Err(StoreError::InvalidSetting(format!(
            "ollamaUrl must not have an empty path segment, got {url:?}"
        )));
    }
    Ok(())
}

/// Whether a URL's authority part starts with a real host: a DNS name, an IPv4
/// address or a bracketed IPv6 address, followed by nothing or by `:port`.
fn host_is_real(authority: &str) -> bool {
    if let Some(inside) = authority.strip_prefix('[') {
        let Some((address, after)) = inside.split_once(']') else {
            return false;
        };
        return address.parse::<std::net::Ipv6Addr>().is_ok()
            && (after.is_empty() || (after.starts_with(':') && after.len() > 1));
    }
    let host = authority.split(':').next().unwrap_or("");
    is_host_name(host)
}

/// Whether `host` is a DNS host name: dot-separated labels of letters, digits
/// and inner hyphens, each 1 to 63 bytes, at most 253 bytes in all. An IPv4
/// address is a name by this rule too.
fn is_host_name(host: &str) -> bool {
    !host.is_empty()
        && host.len() <= 253
        && host.split('.').all(|label| {
            !label.is_empty()
                && label.len() <= 63
                && label
                    .bytes()
                    .all(|byte| byte.is_ascii_alphanumeric() || byte == b'-')
                && !label.starts_with('-')
                && !label.ends_with('-')
        })
}

/// The one currency rule (spec §1.4): three capital letters. Used by
/// [`Settings::currency`] and by a shared price's currency
/// ([`crate::shared::apply`]).
pub fn validate_currency(currency: &str) -> Result<(), StoreError> {
    let letters = currency.len() == 3 && currency.bytes().all(|byte| byte.is_ascii_uppercase());
    if !letters {
        return Err(StoreError::InvalidSetting(format!(
            "currency must be three capital letters, got {currency:?}"
        )));
    }
    Ok(())
}

/// The status line's rules (spec §5.4): a `Fixed` colour is `#rrggbb`, a
/// `ByLoad` pair is ordered and inside 0..=100, `drop` is never 0, a kind takes
/// only the colour rules, time format and text the kind's spec names, and a
/// `Text` segment has its text. The first bad value wins, named by side and
/// position.
fn validate_statusline(config: &StatuslineConfig) -> Result<(), StoreError> {
    for (side, segments) in [("left", &config.left), ("right", &config.right)] {
        for (index, segment) in segments.iter().enumerate() {
            let at = format!("statusline.{side}[{index}]");
            let spec = segment.kind.spec();
            let rule = segment.color.name();
            if !spec.colors.contains(&rule) {
                return Err(StoreError::InvalidSetting(format!(
                    "{at}.color: {} does not take {rule}",
                    spec.label
                )));
            }
            match &segment.color {
                ColorRule::Fixed { color } => {
                    validate_color(color).map_err(|error| {
                        StoreError::InvalidSetting(format!("{at}.color: {error}"))
                    })?;
                }
                ColorRule::ByLoad { warn, bad } => {
                    if warn >= bad {
                        return Err(StoreError::InvalidSetting(format!(
                            "{at}.color: warn ({warn}) must be below bad ({bad})"
                        )));
                    }
                    if *bad > 100 {
                        return Err(StoreError::InvalidSetting(format!(
                            "{at}.color: bad ({bad}) must be at most 100"
                        )));
                    }
                }
                ColorRule::None
                | ColorRule::ByModelFamily
                | ColorRule::ByEffort
                | ColorRule::Account => {}
            }
            if segment.drop == Some(0) {
                return Err(StoreError::InvalidSetting(format!(
                    "{at}.drop: must be at least 1, got 0"
                )));
            }
            if segment.time_format.is_some() && !spec.time {
                return Err(StoreError::InvalidSetting(format!(
                    "{at}.timeFormat: {} does not take a time format",
                    spec.label
                )));
            }
            if segment.text.is_some() && !spec.text {
                return Err(StoreError::InvalidSetting(format!(
                    "{at}.text: {} does not take text",
                    spec.label
                )));
            }
            if segment.kind == SegmentKind::Text && segment.text.is_none() {
                return Err(StoreError::InvalidSetting(format!(
                    "{at}.text: a Text segment needs text"
                )));
            }
            if segment.separator.is_some() && segment.kind != SegmentKind::Separator {
                return Err(StoreError::InvalidSetting(format!(
                    "{at}.separator: {} does not take a separator character",
                    spec.label
                )));
            }
        }
    }
    Ok(())
}

/// A provider subscription stored beside the Claude accounts (spec R4): Codex's
/// or Ollama's login, with the look and monthly price a Claude account has.
/// The plan comes from the provider (Codex's `plan_type`) or, for Ollama, from
/// the person; `price` and `price_currency` follow an account's rule.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ProviderSubscription {
    pub account: AccountRef,
    pub label: String,
    pub abbr: String,
    pub color: String,
    #[serde(default)]
    pub plan: Option<String>,
    #[serde(default)]
    pub price: Option<f64>,
    #[serde(default)]
    pub price_currency: Option<String>,
}

/// Which row an abbreviation check leaves out: the Claude account or the
/// provider subscription an edit is changing, if any. [`AccountsFile::taken_abbrs`]
/// reads it.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum AbbrOwner {
    Account(usize),
    Subscription(usize),
}

/// `accounts.json` with its version. Only version 1 exists; a different version
/// is [`StoreError::UnsupportedVersion`], never silently upgraded.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct AccountsFile {
    pub version: u32,
    #[serde(default)]
    pub accounts: Vec<Account>,
    /// The provider subscriptions, one per Codex or Ollama login (spec R4). An
    /// older file without the key loads with an empty list, and 1.2 reads a
    /// file that has it because it has no `deny_unknown_fields`.
    #[serde(default)]
    pub provider_subscriptions: Vec<ProviderSubscription>,
    #[serde(default)]
    pub settings: Settings,
    /// The secret backend chosen on first use; `None` means no choice was
    /// recorded yet (an older file, or a fresh install).
    #[serde(default)]
    pub secrets_backend: Option<SecretsBackend>,
}

impl Default for AccountsFile {
    fn default() -> Self {
        AccountsFile {
            version: ACCOUNTS_VERSION,
            accounts: Vec::new(),
            provider_subscriptions: Vec::new(),
            settings: Settings::default(),
            secrets_backend: None,
        }
    }
}

impl AccountsFile {
    pub fn find(&self, id: &str) -> Option<&Account> {
        self.accounts.iter().find(|a| a.id == id)
    }

    pub fn find_by_identity(
        &self,
        account_uuid: &str,
        organization_uuid: &str,
    ) -> Option<&Account> {
        self.accounts
            .iter()
            .find(|a| a.account_uuid == account_uuid && a.organization_uuid == organization_uuid)
    }

    /// Look up by id or by abbreviation (case-insensitive) — what the CLI accepts.
    pub fn find_by_key(&self, key: &str) -> Option<&Account> {
        self.find(key).or_else(|| {
            self.accounts
                .iter()
                .find(|a| a.abbr.eq_ignore_ascii_case(key))
        })
    }

    /// Every abbreviation in use, upper-cased: the Claude accounts' and the
    /// provider subscriptions', leaving out `except` (the row an edit is
    /// changing, which may keep its own). This is the one list every
    /// abbreviation check reads.
    pub fn taken_abbrs(&self, except: Option<AbbrOwner>) -> Vec<String> {
        let skip_account = match except {
            Some(AbbrOwner::Account(index)) => Some(index),
            _ => None,
        };
        let skip_subscription = match except {
            Some(AbbrOwner::Subscription(index)) => Some(index),
            _ => None,
        };
        self.accounts
            .iter()
            .enumerate()
            .filter(|(index, _)| Some(*index) != skip_account)
            .map(|(_, account)| account.abbr.trim().to_ascii_uppercase())
            .chain(
                self.provider_subscriptions
                    .iter()
                    .enumerate()
                    .filter(|(index, _)| Some(*index) != skip_subscription)
                    .map(|(_, subscription)| subscription.abbr.trim().to_ascii_uppercase()),
            )
            .collect()
    }

    /// The provider subscription of `account`, or `None`.
    pub fn provider_subscription(&self, account: &AccountRef) -> Option<&ProviderSubscription> {
        self.provider_subscriptions
            .iter()
            .find(|subscription| &subscription.account == account)
    }

    /// Make sure `account` has a provider subscription, and give it `plan` when
    /// it is new or the provider names another plan. Returns whether it was
    /// created.
    ///
    /// A new subscription's look is the documented rule: OpenAI becomes
    /// "OpenAI", `OA`, `#db2777`; Ollama becomes "Ollama", `OL`, `#ea580c`.
    /// Its abbreviation is unique among the Claude accounts and the other
    /// subscriptions; when `OA` or `OL` is taken, the base's first letter with
    /// the first free digit 2-9 (`O2`, `O3`, …) is used. Every abbreviation
    /// goes through the one [`validate_abbr`] rule. Only a Codex, Ollama
    /// or Google `AccountRef` makes a subscription; a Claude account's org is
    /// refused.
    pub fn ensure_provider_subscription(
        &mut self,
        account: &AccountRef,
        plan: Option<&str>,
    ) -> Result<bool, StoreError> {
        let plan = plan.map(str::trim).filter(|name| !name.is_empty());
        if let Some(existing) = self
            .provider_subscriptions
            .iter_mut()
            .find(|subscription| &subscription.account == account)
        {
            if let Some(plan) = plan {
                if existing.plan.as_deref() != Some(plan) {
                    existing.plan = Some(plan.to_string());
                }
            }
            return Ok(false);
        }
        let company = crate::money::Company::of_account(account);
        let label = company.name();
        let (base_abbr, color) = match company {
            crate::money::Company::OpenAI => ("OA", "#db2777"),
            crate::money::Company::Ollama => ("OL", "#ea580c"),
            crate::money::Company::Google => ("GO", "#0d9488"),
            crate::money::Company::Anthropic => {
                return Err(StoreError::NotProvider(account.acc.clone()))
            }
        };
        let abbr = self.unique_subscription_abbr(base_abbr)?;
        self.provider_subscriptions.push(ProviderSubscription {
            account: account.clone(),
            label: label.to_string(),
            abbr,
            color: color.to_string(),
            plan: plan.map(str::to_string),
            price: None,
            price_currency: None,
        });
        Ok(true)
    }

    /// The first free provider abbreviation: the base, then the base's first
    /// letter with the first free digit 2-9 appended. The rule is the
    /// subscription look's; it is checked against every abbreviation in use,
    /// never replaced.
    fn unique_subscription_abbr(&self, base: &str) -> Result<String, StoreError> {
        let taken: HashSet<String> = self.taken_abbrs(None).into_iter().collect();
        if !taken.contains(base) {
            return Ok(base.to_string());
        }
        let Some(first) = base.chars().next() else {
            return Err(StoreError::DuplicateAbbr(base.to_string()));
        };
        first_free_digit_abbr(first, &taken)
            .ok_or_else(|| StoreError::DuplicateAbbr(base.to_string()))
    }
}

/// Where one account's login lives. `encrypted()` false means the UI warns that
/// the fallback files are not encrypted (spec §5).
///
/// The stored value is one JSON value, kept whole and in its key order: a
/// Claude [`Login`], a Codex [`crate::types::CodexLogin`] (R8), or anything else
/// a later version stores. The value methods are the ones an implementation
/// writes; the typed methods on top are the same for every store, so the text a
/// Claude login is stored as does not depend on which one is used.
pub trait SecretStore: Send + Sync {
    /// The stored value for `id`, exactly as it was stored.
    fn get_value(&self, id: &str) -> Result<Option<Value>, StoreError>;
    fn set_value(&self, id: &str, value: &Value) -> Result<(), StoreError>;
    /// Deleting a login that is not there is fine.
    fn delete(&self, id: &str) -> Result<(), StoreError>;
    fn encrypted(&self) -> bool;

    /// A stored Claude [`Login`].
    fn get(&self, id: &str) -> Result<Option<Login>, StoreError> {
        match self.get_value(id)? {
            None => Ok(None),
            Some(Value::Object(map)) => Ok(Some(Login(map))),
            Some(_) => Err(StoreError::NotLogin { id: id.to_string() }),
        }
    }

    /// Store a Claude [`Login`] as the compact JSON of its keys, in their order.
    fn set(&self, id: &str, login: &Login) -> Result<(), StoreError> {
        self.set_value(id, &Value::Object(login.0.clone()))
    }

    /// A stored Codex [`crate::types::CodexLogin`]: `auth.json` whole.
    fn get_codex(&self, id: &str) -> Result<Option<crate::types::CodexLogin>, StoreError> {
        match self.get_value(id)? {
            None => Ok(None),
            Some(Value::Object(map)) => Ok(Some(crate::types::CodexLogin(map))),
            Some(_) => Err(StoreError::NotLogin { id: id.to_string() }),
        }
    }

    fn set_codex(&self, id: &str, login: &crate::types::CodexLogin) -> Result<(), StoreError> {
        self.set_value(id, &Value::Object(login.0.clone()))
    }
}

/// The OS keyring: service `seatswap`, user = account id, value = the login
/// as JSON text (spec §5).
#[derive(Debug, Clone, Copy, Default)]
pub struct KeyringSecrets;

impl KeyringSecrets {
    fn entry(id: &str) -> Result<keyring::Entry, StoreError> {
        keyring::Entry::new(KEYRING_SERVICE, id).map_err(|e| StoreError::Keyring(e.to_string()))
    }
}

impl SecretStore for KeyringSecrets {
    fn get_value(&self, id: &str) -> Result<Option<Value>, StoreError> {
        match Self::entry(id)?.get_password() {
            Ok(text) => serde_json::from_str(&text)
                .map(Some)
                .map_err(|e| StoreError::Keyring(format!("login for {id} is not valid JSON: {e}"))),
            Err(keyring::Error::NoEntry) => Ok(None),
            Err(e) => Err(StoreError::Keyring(e.to_string())),
        }
    }

    fn set_value(&self, id: &str, value: &Value) -> Result<(), StoreError> {
        let text = serde_json::to_string(value)
            .map_err(|e| StoreError::Keyring(format!("cannot encode login for {id}: {e}")))?;
        Self::entry(id)?
            .set_password(&text)
            .map_err(|e| StoreError::Keyring(e.to_string()))
    }

    fn delete(&self, id: &str) -> Result<(), StoreError> {
        match Self::entry(id)?.delete_credential() {
            Ok(()) | Err(keyring::Error::NoEntry) => Ok(()),
            Err(e) => Err(StoreError::Keyring(e.to_string())),
        }
    }

    fn encrypted(&self) -> bool {
        true
    }
}

/// Fallback secret storage: `<dir>/<id>.json`, files 0600 in a 0700 folder.
#[derive(Debug, Clone)]
pub struct FileSecrets {
    dir: PathBuf,
}

impl FileSecrets {
    pub fn new(dir: PathBuf) -> FileSecrets {
        FileSecrets { dir }
    }

    /// The file path for `id`; ids that are not 32 lower-case hex characters are
    /// rejected so a bad id can never escape the folder.
    fn path_for(&self, id: &str) -> Result<PathBuf, StoreError> {
        if id.len() == 32
            && id
                .bytes()
                .all(|b| b.is_ascii_digit() || (b'a'..=b'f').contains(&b))
        {
            Ok(self.dir.join(format!("{id}.json")))
        } else {
            Err(StoreError::InvalidId(id.to_string()))
        }
    }

    fn ensure_dir(&self) -> Result<(), StoreError> {
        atomic::ensure_private_dir(&self.dir).map_err(|e| io_error(&self.dir, e))?;
        atomic::restrict_dir(&self.dir).map_err(|e| io_error(&self.dir, e))
    }
}

impl SecretStore for FileSecrets {
    fn get_value(&self, id: &str) -> Result<Option<Value>, StoreError> {
        let path = self.path_for(id)?;
        let bytes = match fs::read(&path) {
            Ok(bytes) => bytes,
            Err(e) if e.kind() == io::ErrorKind::NotFound => return Ok(None),
            Err(e) => return Err(io_error(&path, e)),
        };
        serde_json::from_slice(&bytes)
            .map(Some)
            .map_err(|_| StoreError::Corrupt { path })
    }

    fn set_value(&self, id: &str, value: &Value) -> Result<(), StoreError> {
        let path = self.path_for(id)?;
        self.ensure_dir()?;
        let bytes = serde_json::to_vec(value).map_err(|e| StoreError::Io {
            path: path.clone(),
            source: io::Error::new(io::ErrorKind::InvalidData, e),
        })?;
        write_atomic(&path, &bytes, Access::Private).map_err(|e| io_error(&path, e))
    }

    fn delete(&self, id: &str) -> Result<(), StoreError> {
        let path = self.path_for(id)?;
        match fs::remove_file(&path) {
            Ok(()) => Ok(()),
            Err(e) if e.kind() == io::ErrorKind::NotFound => Ok(()),
            Err(e) => Err(io_error(&path, e)),
        }
    }

    fn encrypted(&self) -> bool {
        false
    }
}

/// The store used on Windows when the recorded backend is the keyring: Claude
/// logins stay in Credential Manager (`main`), Codex logins go to private files
/// in the secrets folder (`codex`). A Codex login is the whole `auth.json`,
/// past Credential Manager's 2560 bytes per entry (about 3,900 characters in
/// docs/windows.md §15, R7).
///
/// Built on every platform so its tests run everywhere; only
/// [`open_secrets_with`] chooses it, and only on Windows. The typed methods are
/// the trait's own, so a login is the same JSON whichever store holds it.
pub struct RoutedSecrets {
    /// Claude logins and every other value: the backend the user recorded.
    pub main: Box<dyn SecretStore>,
    /// Codex logins: private files in the secrets folder, as the 0600 fallback.
    pub codex: FileSecrets,
}

impl SecretStore for RoutedSecrets {
    fn get_value(&self, id: &str) -> Result<Option<Value>, StoreError> {
        self.main.get_value(id)
    }

    fn set_value(&self, id: &str, value: &Value) -> Result<(), StoreError> {
        self.main.set_value(id, value)
    }

    /// Deletes in `codex` first, then in `main`; the other is still attempted
    /// when one fails, and the first error is the one returned.
    fn delete(&self, id: &str) -> Result<(), StoreError> {
        let codex = self.codex.delete(id);
        let main = self.main.delete(id);
        codex.and(main)
    }

    fn encrypted(&self) -> bool {
        self.main.encrypted()
    }

    fn get_codex(&self, id: &str) -> Result<Option<crate::types::CodexLogin>, StoreError> {
        self.codex.get_codex(id)
    }

    fn set_codex(&self, id: &str, login: &crate::types::CodexLogin) -> Result<(), StoreError> {
        self.codex.set_codex(id, login)
    }
}

/// Open the secrets for the backend recorded in `accounts.json` (`recorded`) or,
/// on first use (`None`), choose one by probing the keyring and report it so the
/// caller can record it. The second value is the backend to record; the third is
/// the warning text shown when the files are used (spec §5).
///
/// A recorded keyring is never abandoned for files: if it does not answer, the
/// error says the logins are stored in the keyring (they are, and falling back
/// would hide them).
// The tuple shape is the binding contract from docs/implementation.md.
#[allow(clippy::type_complexity)]
pub fn open_secrets(
    app: &AppPaths,
    recorded: Option<SecretsBackend>,
) -> Result<(Box<dyn SecretStore>, SecretsBackend, Option<String>), StoreError> {
    open_secrets_with(app, recorded, probe_keyring)
}

/// [`open_secrets`] with the keyring probe injectable, so tests never touch the
/// real keyring. The probe succeeds when the keyring can store and return a
/// throw-away entry.
#[allow(clippy::type_complexity)]
fn open_secrets_with(
    app: &AppPaths,
    recorded: Option<SecretsBackend>,
    probe: impl FnOnce() -> Result<(), String>,
) -> Result<(Box<dyn SecretStore>, SecretsBackend, Option<String>), StoreError> {
    match recorded {
        Some(SecretsBackend::Keyring) => match probe() {
            Ok(()) => Ok((keyring_store(app), SecretsBackend::Keyring, None)),
            Err(reason) => Err(StoreError::Keyring(format!(
                "the OS keyring is not available: {reason}; the logins are stored in the keyring"
            ))),
        },
        Some(SecretsBackend::File) => {
            let (secrets, warning) = file_secrets(app);
            Ok((secrets, SecretsBackend::File, Some(warning)))
        }
        None => match probe() {
            Ok(()) => Ok((keyring_store(app), SecretsBackend::Keyring, None)),
            Err(reason) => {
                let (secrets, warning) = file_secrets(app);
                Ok((
                    secrets,
                    SecretsBackend::File,
                    Some(format!(
                        "the OS keyring is not available ({reason}); {warning}"
                    )),
                ))
            }
        },
    }
}

/// The keyring-backed store. On Windows a Codex login cannot be saved in
/// Credential Manager — 2560 bytes per entry, a Codex login about 7,800 bytes as
/// UTF-16 (docs/windows.md §15) — so the keyring keeps the Claude logins and
/// Codex logins go to private files in the secrets folder (R7).
fn keyring_store(app: &AppPaths) -> Box<dyn SecretStore> {
    #[cfg(windows)]
    {
        Box::new(RoutedSecrets {
            main: Box::new(KeyringSecrets),
            codex: FileSecrets::new(app.secrets_dir()),
        })
    }
    #[cfg(not(windows))]
    {
        let _ = app;
        Box::new(KeyringSecrets)
    }
}

/// The fallback store under `app.secrets_dir()` and the warning that names it.
fn file_secrets(app: &AppPaths) -> (Box<dyn SecretStore>, String) {
    let dir = app.secrets_dir();
    let secrets: Box<dyn SecretStore> = Box::new(FileSecrets::new(dir.clone()));
    (
        secrets,
        format!("logins are stored unencrypted in {}", dir.display()),
    )
}

/// Set, read back and delete a throw-away entry: the only reliable keyring test.
fn probe_keyring() -> Result<(), String> {
    let entry =
        keyring::Entry::new(KEYRING_SERVICE, KEYRING_PROBE_USER).map_err(|e| e.to_string())?;
    entry.set_password("probe").map_err(|e| e.to_string())?;
    entry.get_password().map_err(|e| e.to_string())?;
    match entry.delete_credential() {
        Ok(()) | Err(keyring::Error::NoEntry) => Ok(()),
        Err(e) => Err(e.to_string()),
    }
}

/// The account store: `accounts.json` plus a [`SecretStore`].
pub struct Store {
    app: AppPaths,
    secrets: Box<dyn SecretStore>,
}

impl Store {
    pub fn new(app: AppPaths, secrets: Box<dyn SecretStore>) -> Store {
        Store { app, secrets }
    }

    pub fn secrets(&self) -> &dyn SecretStore {
        self.secrets.as_ref()
    }

    pub fn app_paths(&self) -> &AppPaths {
        &self.app
    }

    /// A missing file is an empty list with default settings. A file that does
    /// not parse is [`StoreError::Corrupt`] and is left untouched.
    pub fn load(&self) -> Result<AccountsFile, StoreError> {
        load_accounts(&self.app)
    }

    /// Pretty JSON, mode 0600, through [`write_atomic`]; creates the data folder
    /// (0700) when it is missing. [`save_accounts`] is the one writer.
    pub fn save(&self, file: &AccountsFile) -> Result<(), StoreError> {
        save_accounts(&self.app, file)
    }
}

/// Write `accounts.json` without opening a secret store: pretty JSON, mode
/// 0600, through [`write_atomic`]; creates the data folder (0700) when it is
/// missing. The counterpart of [`load_accounts`] and the one writer of the
/// file, used both by [`Store::save`] and by a caller that only keeps the
/// account and subscription list.
pub fn save_accounts(app: &AppPaths, file: &AccountsFile) -> Result<(), StoreError> {
    let path = app.accounts_file();
    if file.version != ACCOUNTS_VERSION {
        return Err(StoreError::UnsupportedVersion(file.version));
    }
    atomic::ensure_private_dir(&app.data_dir).map_err(|e| io_error(&app.data_dir, e))?;
    let bytes = serde_json::to_vec_pretty(file).map_err(|e| StoreError::Io {
        path: path.clone(),
        source: io::Error::new(io::ErrorKind::InvalidData, e),
    })?;
    write_atomic(&path, &bytes, Access::Private).map_err(|e| io_error(&path, e))
}

/// Read `accounts.json` without opening a secret store. A missing file is an
/// empty list with default settings. A file that does not parse is
/// [`StoreError::Corrupt`] and is left untouched.
pub fn load_accounts(app: &AppPaths) -> Result<AccountsFile, StoreError> {
    let path = app.accounts_file();
    let bytes = match fs::read(&path) {
        Ok(bytes) => bytes,
        Err(e) if e.kind() == io::ErrorKind::NotFound => return Ok(AccountsFile::default()),
        Err(e) => return Err(io_error(&path, e)),
    };
    let value: serde_json::Value =
        serde_json::from_slice(&bytes).map_err(|_| StoreError::Corrupt { path: path.clone() })?;
    // Check the version before the rest of the shape: a newer file must be
    // reported as unsupported, not as corrupt.
    let version = value
        .get("version")
        .and_then(serde_json::Value::as_u64)
        .and_then(|v| u32::try_from(v).ok())
        .ok_or_else(|| StoreError::Corrupt { path: path.clone() })?;
    if version != ACCOUNTS_VERSION {
        return Err(StoreError::UnsupportedVersion(version));
    }
    serde_json::from_value::<AccountsFile>(value).map_err(|_| StoreError::Corrupt { path })
}

/// The one way to open the store (CLI and GUI): load `accounts.json`, open the
/// secrets for the recorded backend, record the backend on first use (save), and
/// return the store and the warning from [`open_secrets`].
pub fn open_store(app: AppPaths) -> Result<(Store, Option<String>), StoreError> {
    open_store_with(app, probe_keyring)
}

/// [`open_store`] with the keyring probe injectable for tests; no test reaches
/// the real keyring.
fn open_store_with(
    app: AppPaths,
    probe: impl FnOnce() -> Result<(), String>,
) -> Result<(Store, Option<String>), StoreError> {
    // Loading only reads accounts.json; the fallback secrets are never touched
    // before the backend is chosen.
    let mut file = load_accounts(&app)?;
    let (secrets, backend, warning) = open_secrets_with(&app, file.secrets_backend, probe)?;
    let store = Store::new(app, secrets);
    if file.secrets_backend.is_none() {
        file.secrets_backend = Some(backend);
        store.save(&file)?;
    }
    Ok((store, warning))
}

/// "Acme Team" → "AT", "Acme" → "AC", "Private" → "PR", "x" → "X".
/// Words split on whitespace and punctuation. Letters/digits only; a one-word
/// label contributes its first two, a multi-word label the first letter/digit of
/// each of its first two contributing words. Non-ASCII characters are skipped:
/// each word contributes its first ASCII letter or digit, so "Ünïcode Bank" →
/// "NB" and a label with no ASCII letter or digit at all is [`StoreError::InvalidLabel`].
pub fn default_abbr(label: &str) -> Result<String, StoreError> {
    let words: Vec<&str> = label
        .split(|c: char| !c.is_alphanumeric())
        .filter(|w| !w.is_empty())
        .collect();
    let mut letters: Vec<char> = Vec::new();
    if words.len() >= 2 {
        for word in &words {
            if let Some(c) = word.chars().find(|c| c.is_ascii_alphanumeric()) {
                letters.push(c.to_ascii_uppercase());
                if letters.len() == 2 {
                    break;
                }
            }
        }
    } else if let Some(word) = words.first() {
        letters = word
            .chars()
            .filter(|c| c.is_ascii_alphanumeric())
            .take(2)
            .map(|c| c.to_ascii_uppercase())
            .collect();
    }
    if letters.is_empty() {
        return Err(StoreError::InvalidLabel(label.to_string()));
    }
    Ok(letters.into_iter().collect())
}

/// Trim, upper-case, check 1-2 of A-Z 0-9, and check it is not in `others`
/// (every other abbreviation in use: the other accounts' and subscriptions';
/// compared case-insensitively). The one abbreviation rule for accounts and
/// provider subscriptions.
pub fn validate_abbr(abbr: &str, others: &[&str]) -> Result<String, StoreError> {
    let normalized = abbr.trim().to_ascii_uppercase();
    let valid = (1..=2).contains(&normalized.chars().count())
        && normalized
            .chars()
            .all(|c| c.is_ascii_uppercase() || c.is_ascii_digit());
    if !valid {
        return Err(StoreError::InvalidAbbr(abbr.to_string()));
    }
    if others
        .iter()
        .any(|other| other.trim().eq_ignore_ascii_case(&normalized))
    {
        return Err(StoreError::DuplicateAbbr(normalized));
    }
    Ok(normalized)
}

/// `"#rrggbb"` in any case → lower case; anything else is [`StoreError::InvalidColor`].
pub fn validate_color(color: &str) -> Result<String, StoreError> {
    let valid = color.len() == 7
        && color.starts_with('#')
        && color[1..].bytes().all(|b| b.is_ascii_hexdigit());
    if valid {
        Ok(color.to_ascii_lowercase())
    } else {
        Err(StoreError::InvalidColor(color.to_string()))
    }
}

/// Claude's current plan names for a subscription type and the multiplier of
/// its `rateLimitTier` (spec §3.7.1), with the label seatswap up to 1.2.0 gave
/// the same plan. Claude renamed its plans: what the tier still calls `max_5x`
/// is "Max", `max_20x` is "Ultra", and a Team seat with the 5x tier is "Team
/// Premium".
const PLAN_NAMES: [(&str, &str, &str, &str); 3] = [
    ("max", "5x", "Max", "Max 5x"),
    ("max", "20x", "Ultra", "Max 20x"),
    ("team", "5x", "Team Premium", "Team 5x"),
];

/// Whether `from` → `to` is only seatswap's old label of a plan giving way to
/// Claude's current name ([`PLAN_NAMES`]): the plan itself did not change.
fn renamed_only(from: Option<&str>, to: Option<&str>) -> bool {
    PLAN_NAMES
        .iter()
        .any(|(_, _, name, old)| from == Some(*old) && to == Some(*name))
}

/// The plan as Claude names it: the identity's `planDisplayName` ("Pro",
/// "Max", "Ultra"), else the name [`PLAN_NAMES`] gives the login's
/// `subscriptionType` and tier, else the `subscriptionType` with a capital
/// first letter plus its level from `rateLimitTier`. A tier ending in `_<n>x`
/// adds `" <n>x"` ("Enterprise 20x"); any other tier adds
/// `" (<tier without the prefix default_claude_>)"`; no tier adds nothing.
pub fn plan_label(login: &Login, identity: &OAuthAccount) -> Option<String> {
    let named = identity
        .plan_display_name()
        .map(str::trim)
        .filter(|name| !name.is_empty())
        .map(str::to_string);
    named.or_else(|| {
        let subscription = login.subscription_type()?.trim();
        let mut chars = subscription.chars();
        let first = chars.next()?;
        let mut label: String = first.to_uppercase().chain(chars).collect();
        if let Some(tier) = login.rate_limit_tier().map(str::trim) {
            if tier.is_empty() {
                return Some(label);
            }
            let level = tier_multiplier(tier);
            if let Some((_, _, name, _)) = PLAN_NAMES.iter().find(|(kind, multiplier, _, _)| {
                kind.eq_ignore_ascii_case(subscription) && level == Some(*multiplier)
            }) {
                return Some(name.to_string());
            }
            if let Some(level) = level {
                label.push(' ');
                label.push_str(level);
            } else {
                let name = tier.strip_prefix("default_claude_").unwrap_or(tier);
                label.push_str(" (");
                label.push_str(name);
                label.push(')');
            }
        }
        Some(label)
    })
}

/// The `"<n>x"` at the end of a tier written `_<n>x`, else `None`.
fn tier_multiplier(tier: &str) -> Option<&str> {
    let digits = tier.strip_suffix('x')?;
    let start = digits.rfind('_')? + 1;
    let number = &digits[start..];
    if number.is_empty() || !number.bytes().all(|byte| byte.is_ascii_digit()) {
        return None;
    }
    Some(&tier[start..])
}

/// The capacity a tier names: `n` for a tier ending in `_<n>x` with `n` above
/// 0, else `None` (nothing is guessed for a tier without a multiplier).
pub fn tier_capacity(tier: &str) -> Option<f64> {
    let digits = tier_multiplier(tier)?.strip_suffix('x')?;
    let capacity: u32 = digits.parse().ok()?;
    (capacity > 0).then_some(f64::from(capacity))
}

impl Account {
    /// The capacity of the account's plan from its identity's rate-limit tier
    /// ([`OAuthAccount::rate_limit_tier`], [`tier_capacity`]): 5 for Max and
    /// Team Premium, 20 for Ultra. A tier that names no multiplier is 1 when
    /// the plan's name is exactly "Pro" (documented rule), else `None`: Team
    /// Standard and any other plan stay unknown.
    pub fn capacity(&self) -> Option<f64> {
        match self.oauth_account.rate_limit_tier().and_then(tier_capacity) {
            Some(capacity) => Some(capacity),
            None if self.plan.as_deref() == Some("Pro") => Some(1.0),
            None => None,
        }
    }

    /// Bring the fields that come from the stored login up to date after a
    /// login was stored for this account (switch, live sync, refresh): the plan
    /// ([`plan_label`] with the account's identity), the login's end date and
    /// the login state. An old label replaced by Claude's current name
    /// ([`renamed_only`]) is saved but is no plan change.
    pub fn note_stored_login(&mut self, login: &Login) -> LoginNote {
        let plan = plan_label(login, &self.oauth_account);
        let ends_at_ms = login.refresh_token_expires_at_ms();
        let plan_changed =
            self.plan != plan && !renamed_only(self.plan.as_deref(), plan.as_deref());
        let changed = self.plan != plan
            || self.login_ends_at_ms != ends_at_ms
            || self.login_state != LoginState::Ok;
        let previous_plan = self.plan.clone();
        self.plan = plan;
        self.login_ends_at_ms = ends_at_ms;
        self.login_state = LoginState::Ok;
        LoginNote {
            changed,
            plan_changed,
            previous_plan,
        }
    }
}

/// What [`Account::note_stored_login`] changed.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct LoginNote {
    /// The account must be saved: plan, login end date or state changed.
    pub changed: bool,
    /// The plan changed; the caller that records history writes `PlanChanged`.
    pub plan_changed: bool,
    /// The plan before the note, when [`Self::plan_changed`] is true.
    pub previous_plan: Option<String>,
}

/// A monthly price must be a finite number above 0 (spec §1.4); anything else
/// is [`StoreError::InvalidPrice`], never clamped or dropped.
pub fn validate_price(price: Option<f64>) -> Result<Option<f64>, StoreError> {
    match price {
        Some(value) if !value.is_finite() || value <= 0.0 => Err(StoreError::InvalidPrice(value)),
        other => Ok(other),
    }
}

/// A new [`Account`] for a login and identity. The label is the explicit one,
/// else the organization name, else the email; the abbreviation is the label's
/// [`default_abbr`] made unique; the colour is the first free palette entry, or
/// the least-used one when all ten are taken (ties in palette order).
pub fn new_account(
    login: &Login,
    identity: &OAuthAccount,
    label: Option<&str>,
    file: &AccountsFile,
    now: DateTime<Utc>,
) -> Result<Account, StoreError> {
    // `new_account` has no path argument, so callers that know Claude's
    // identity file (switcher) validate the identity first and name the real
    // path. A direct call still gets a clear message naming the file.
    let (account_uuid, organization_uuid) =
        identity
            .identity()
            .ok_or_else(|| StoreError::MissingIdentity {
                path: PathBuf::from(".claude.json"),
            })?;
    if let Some(existing) = file.find_by_identity(account_uuid, organization_uuid) {
        let email = existing
            .email
            .clone()
            .or_else(|| identity.email().map(str::to_string))
            .unwrap_or_default();
        return Err(StoreError::DuplicateAccount { email });
    }
    let label = match label {
        Some(raw) => validate_label(raw)?,
        None => {
            let raw = identity
                .organization_name()
                .filter(|s| !s.trim().is_empty())
                .or_else(|| identity.email())
                .ok_or_else(|| StoreError::InvalidLabel(String::new()))?;
            validate_label(raw)?
        }
    };
    let taken: HashSet<String> = file.taken_abbrs(None).into_iter().collect();
    let abbr = unique_abbr(&label, &taken)?;
    let color = pick_color(file);
    Ok(Account {
        id: uuid::Uuid::new_v4().simple().to_string(),
        account_uuid: account_uuid.to_string(),
        organization_uuid: organization_uuid.to_string(),
        label,
        abbr,
        color,
        email: identity.email().map(str::to_string),
        org_name: identity.organization_name().map(str::to_string),
        plan: plan_label(login, identity),
        oauth_account: identity.clone(),
        added_at: now,
        last_used_at: None,
        login_state: if login.is_ended() {
            LoginState::LoggedOut
        } else {
            LoginState::Ok
        },
        login_ends_at_ms: login.refresh_token_expires_at_ms(),
        auto_burn: true,
        price: None,
        price_currency: None,
    })
}

/// Trim a label and check it is 1..=40 characters; [`StoreError::InvalidLabel`]
/// otherwise. The one label rule, used by account creation and editing.
pub fn validate_label(label: &str) -> Result<String, StoreError> {
    let trimmed = label.trim();
    if (1..=40).contains(&trimmed.chars().count()) {
        Ok(trimmed.to_string())
    } else {
        Err(StoreError::InvalidLabel(label.to_string()))
    }
}

/// The label's default abbreviation, or the first free one made from the label's
/// first letter/digit and each following one, then that first character plus
/// digits 2-9; [`StoreError::DuplicateAbbr`] when all of those are taken.
fn unique_abbr(label: &str, taken: &HashSet<String>) -> Result<String, StoreError> {
    let default = default_abbr(label)?;
    if !taken.contains(&default) {
        return Ok(default);
    }
    let chars: Vec<char> = label
        .chars()
        .filter(|c| c.is_ascii_alphanumeric())
        .map(|c| c.to_ascii_uppercase())
        .collect();
    let Some(&first) = chars.first() else {
        return Err(StoreError::DuplicateAbbr(default));
    };
    let mut tried: HashSet<String> = HashSet::new();
    for c in &chars[1..] {
        let candidate = format!("{first}{c}");
        if !taken.contains(&candidate) && tried.insert(candidate.clone()) {
            return Ok(candidate);
        }
    }
    first_free_digit_abbr(first, taken).ok_or(StoreError::DuplicateAbbr(default))
}

/// The last step of an abbreviation search: `first` with the first free digit
/// 2-9 appended, or `None` when all of them are taken. [`unique_abbr`] and
/// [`AccountsFile::unique_subscription_abbr`] share it; each keeps its own
/// candidates before this step.
fn first_free_digit_abbr(first: char, taken: &HashSet<String>) -> Option<String> {
    for digit in '2'..='9' {
        let candidate = format!("{first}{digit}");
        if !taken.contains(&candidate) {
            return Some(candidate);
        }
    }
    None
}

fn pick_color(file: &AccountsFile) -> String {
    let mut counts = [0usize; PALETTE.len()];
    for account in &file.accounts {
        for (i, color) in PALETTE.iter().enumerate() {
            if account.color.eq_ignore_ascii_case(color) {
                counts[i] += 1;
            }
        }
    }
    if let Some(i) = counts.iter().position(|&n| n == 0) {
        return PALETTE[i].to_string();
    }
    let mut best = 0;
    for i in 1..counts.len() {
        if counts[i] < counts[best] {
            best = i;
        }
    }
    PALETTE[best].to_string()
}

fn io_error(path: &Path, source: io::Error) -> StoreError {
    StoreError::Io {
        path: path.to_path_buf(),
        source,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::{json, Value};
    #[cfg(unix)]
    use std::os::unix::fs::PermissionsExt;

    const ONE_ID: &str = "6f1c2a9be0d34c118f2d4e5a6b7c8d9e";
    const TWO_ID: &str = "a92d4c1e8b3f4a5d9e6c7b8a1d2e3f40";
    const ONE_ACCOUNT_UUID: &str = "4b7e2f91-3c5a-4d8e-9f1b-6a2c8e4d7b30";
    const ONE_ORG_UUID: &str = "e2a6c9d1-7f4b-4a3e-8c5d-1b9f0a7e3c62";
    const TWO_ACCOUNT_UUID: &str = "8e1d5b7a-6c3f-4a2b-9d8e-1f2a3b4c5d6e";
    const TWO_ORG_UUID: &str = "5f2c8a1b-9d4e-4f6a-8b3c-7e2d1a9c4b5e";

    fn now() -> DateTime<Utc> {
        DateTime::parse_from_rfc3339("2026-11-10T12:00:00Z")
            .unwrap()
            .with_timezone(&Utc)
    }

    fn sample_login() -> Login {
        let credentials = json!({
            "mcpOAuth": {
                "plugin:engineering:linear|5f1c2a9be0d34c11": {
                    "serverName": "plugin:engineering:linear",
                    "accessToken": format!("lin_oauth_{}", "c".repeat(64)),
                    "expiresAt": 1794400000000_i64
                }
            },
            "claudeAiOauth": {
                "accessToken": format!("sk-ant-oat01-{}", "a".repeat(95)),
                "refreshToken": format!("sk-ant-ort01-{}", "b".repeat(95)),
                "expiresAt": 1794356040000_i64,
                "refreshTokenExpiresAt": 1796772840000_i64,
                "scopes": [
                    "user:inference", "user:profile", "user:file_upload",
                    "user:mcp_servers", "user:plugins", "user:sessions:claude_code"
                ],
                "subscriptionType": "team",
                "subscriptionTypeSource": "profile",
                "rateLimitTier": "default_claude_max_5x"
            },
            "organizationUuid": ONE_ORG_UUID
        });
        Login::from_credentials(credentials.as_object().unwrap()).unwrap()
    }

    /// A Codex `auth.json` as Codex writes it: the whole object with the three
    /// tokens, their real lengths, and the account id. It is about 3,800
    /// characters, about 7,600 bytes as UTF-16, past Credential Manager's 2560
    /// bytes per entry (docs/windows.md §15).
    fn sample_codex_login() -> crate::types::CodexLogin {
        let value = json!({
            "OPENAI_API_KEY": null,
            "tokens": {
                "id_token": "x".repeat(1_600),
                "access_token": "x".repeat(1_900),
                "refresh_token": "x".repeat(90),
                "account_id": ONE_ACCOUNT_UUID,
            },
            "last_refresh": "2026-11-18T12:00:00.000000Z",
        });
        crate::types::CodexLogin(
            value
                .as_object()
                .expect("the fixture login is an object")
                .clone(),
        )
    }

    fn identity(
        account_uuid: &str,
        organization_uuid: &str,
        org_name: &str,
        email: &str,
    ) -> OAuthAccount {
        serde_json::from_value(json!({
            "accountUuid": account_uuid,
            "emailAddress": email,
            "organizationUuid": organization_uuid,
            "organizationName": org_name,
            "organizationRole": "admin",
            "billingType": "stripe_subscription",
            "hasExtraUsageEnabled": false
        }))
        .unwrap()
    }

    fn sample_identity() -> OAuthAccount {
        identity(
            ONE_ACCOUNT_UUID,
            ONE_ORG_UUID,
            "Acme",
            "someone@example.com",
        )
    }

    fn login_with(subscription: Option<&str>, tier: Option<&str>) -> Login {
        let mut oauth = serde_json::Map::new();
        oauth.insert(
            "refreshToken".to_string(),
            json!(format!("sk-ant-ort01-{}", "b".repeat(95))),
        );
        if let Some(subscription) = subscription {
            oauth.insert("subscriptionType".to_string(), json!(subscription));
        }
        if let Some(tier) = tier {
            oauth.insert("rateLimitTier".to_string(), json!(tier));
        }
        let mut map = serde_json::Map::new();
        map.insert("claudeAiOauth".to_string(), Value::Object(oauth));
        Login(map)
    }

    #[cfg(unix)]
    fn mode(path: &Path) -> u32 {
        fs::metadata(path).unwrap().permissions().mode() & 0o777
    }

    fn store_at(dir: &Path) -> Store {
        let app = AppPaths::at(dir);
        let secrets = FileSecrets::new(app.secrets_dir());
        Store::new(app, Box::new(secrets))
    }

    #[test]
    fn load_missing_file_gives_empty_list_and_defaults() {
        let tmp = tempfile::tempdir().unwrap();
        let store = store_at(&tmp.path().join("app"));
        let file = store.load().unwrap();
        assert_eq!(file.version, 1);
        assert!(file.accounts.is_empty());
        assert_eq!(file.settings, Settings::default());
        assert!(!store.app_paths().accounts_file().exists());
    }

    #[test]
    fn save_and_load_round_trip_two_real_shaped_accounts() {
        let tmp = tempfile::tempdir().unwrap();
        let store = store_at(&tmp.path().join("app"));
        let now = now();

        let mut file = AccountsFile::default();
        let mut first = new_account(&sample_login(), &sample_identity(), None, &file, now).unwrap();
        first.id = ONE_ID.to_string();
        first.last_used_at = Some(now);
        file.accounts.push(first);
        let second_identity =
            identity(TWO_ACCOUNT_UUID, TWO_ORG_UUID, "Private", "two@example.com");
        let mut second = new_account(&sample_login(), &second_identity, None, &file, now).unwrap();
        second.id = TWO_ID.to_string();
        second.login_state = LoginState::LoggedOut;
        file.accounts.push(second);

        file.settings = Settings {
            tray_text: TrayText::PercentReset,
            icon_limit: IconLimit::Week,
            icon_text: IconText::Percent,
            theme: Theme::Dark,
            claude_config_dir: Some(PathBuf::from("/home/u/.claude")),
            ..Settings::default()
        };

        store.save(&file).unwrap();

        #[cfg(unix)]
        let folder = store.app_paths().data_dir.clone();
        let path = store.app_paths().accounts_file();
        #[cfg(unix)]
        assert_eq!(mode(&folder), 0o700);
        #[cfg(unix)]
        assert_eq!(mode(&path), 0o600);
        assert_eq!(file.accounts[0].id.len(), 32);
        assert!(file.accounts[0].id.bytes().all(|b| b.is_ascii_hexdigit()));
        assert_eq!(file.accounts[1].id, TWO_ID);

        let raw = fs::read_to_string(&path).unwrap();
        for expected in [
            "\"version\": 1",
            "\"accountUuid\"",
            "\"organizationUuid\"",
            "\"addedAt\"",
            "\"lastUsedAt\"",
            "\"loginState\": \"loggedOut\"",
            "\"loginEndsAtMs\"",
            "\"oauthAccount\"",
            "\"emailAddress\"",
            "\"trayText\": \"percentReset\"",
            "\"iconLimit\": \"week\"",
            "\"iconText\": \"percent\"",
            "\"theme\": \"dark\"",
            "\"claudeConfigDir\"",
        ] {
            assert!(raw.contains(expected), "missing {expected} in {raw}");
        }
        assert!(!raw.ends_with('\n'));

        let loaded = store.load().unwrap();
        assert_eq!(loaded, file);
        assert_eq!(loaded.accounts[0].oauth_account, sample_identity());
        assert_eq!(loaded.accounts[0].plan.as_deref(), Some("Team Premium"));
        assert_eq!(loaded.accounts[0].abbr, "AC");
        assert_eq!(loaded.accounts[1].abbr, "PR");
    }

    #[test]
    fn settings_missing_poll_active_secs_loads_the_default() {
        let tmp = tempfile::tempdir().unwrap();
        let store = store_at(&tmp.path().join("app"));
        fs::create_dir_all(store.app_paths().data_dir.clone()).unwrap();
        fs::write(
            store.app_paths().accounts_file(),
            br#"{"version":1,"accounts":[],"settings":{"trayText":"percentReset"}}"#,
        )
        .unwrap();
        let file = store.load().unwrap();
        assert_eq!(file.settings.poll_active_secs, 120);
        assert_eq!(file.settings.poll_inactive_secs, 900);
        assert!(!file.settings.auto_burn);
        assert!(file.settings.suggest_switches);
        assert_eq!(file.settings.tray_text, TrayText::PercentReset);
        assert_eq!(file.settings.icon_limit, IconLimit::Binding);
        assert_eq!(file.settings.icon_text, IconText::Letters);
        assert_eq!(file.settings.theme, Theme::System);
        assert_eq!(file.settings.hotkey, "Ctrl+Alt+C");
        assert!(file.settings.notify_on_switch);
        assert!(file.settings.mirror_live);
        assert!(file.settings.refresh_inactive);
        assert!(file.settings.start_at_login);
        assert_eq!(file.settings.claude_config_dir, None);
    }

    #[test]
    fn settings_without_a_statusline_get_the_current_line() {
        let tmp = tempfile::tempdir().unwrap();
        let store = store_at(&tmp.path().join("app"));
        fs::create_dir_all(store.app_paths().data_dir.clone()).unwrap();
        fs::write(
            store.app_paths().accounts_file(),
            br#"{"version":1,"accounts":[],"settings":{}}"#,
        )
        .unwrap();
        let file = store.load().unwrap();
        assert_eq!(file.settings.statusline, StatuslineConfig::current_line());
        assert_eq!(file.settings.statusline.right.len(), 5);
        assert_eq!(file.settings.statusline.margin, 4);
    }

    #[test]
    fn the_current_line_round_trips_through_json() {
        let preset = StatuslineConfig::current_line();
        let text = serde_json::to_string(&preset).unwrap();
        let loaded: StatuslineConfig = serde_json::from_str(&text).unwrap();
        assert_eq!(loaded, preset);
        // The short file: empty prefix/suffix and absent options are left out.
        for omitted in ["\"prefix\":\"\"", "\"suffix\":\"\"", "\"drop\":null"] {
            assert!(!text.contains(omitted), "unexpected {omitted} in {text}");
        }
        // The names on disk are camelCase, and a colour rule is tagged by "rule".
        for expected in [
            "\"kind\":\"fiveHourPercent\"",
            "\"timeFormat\":\"compactLeft\"",
            "\"rule\":\"byLoad\"",
            "\"rule\":\"fixed\"",
        ] {
            assert!(text.contains(expected), "missing {expected} in {text}");
        }
    }

    /// A file written before `statusline` gained a field must still load with
    /// that field's default, like every other setting (see `Settings`).
    #[test]
    fn a_statusline_without_a_margin_still_loads() {
        let tmp = tempfile::tempdir().unwrap();
        let store = store_at(&tmp.path().join("app"));
        fs::create_dir_all(store.app_paths().data_dir.clone()).unwrap();
        fs::write(
            store.app_paths().accounts_file(),
            br#"{"version":1,"accounts":[],"settings":{"statusline":{"left":[],"right":[]}}}"#,
        )
        .unwrap();
        let file = store
            .load()
            .expect("a statusline without a margin must still load");
        assert_eq!(file.settings.statusline.margin, 4);
    }

    /// A `statuslineInstall` with `"previous": null` keeps that `null`; one
    /// without the field has no previous value, which is a different thing.
    #[test]
    fn statusline_install_keeps_a_null_previous_and_a_missing_one_apart() {
        let installed = json!({
            "type": "command",
            "command": "\"/opt/seatswap\" statusline",
            "refreshInterval": 2,
        });
        let with_null: Settings = serde_json::from_value(json!({
            "statuslineInstall": { "installed": installed, "previous": null }
        }))
        .expect("a record with a null previous");
        let record = with_null.statusline_install.expect("the record must load");
        assert_eq!(record.previous, Some(Value::Null));
        assert_eq!(
            serde_json::to_value(&record).expect("serialize"),
            json!({ "installed": installed, "previous": null }),
            "a present null is written back"
        );

        let without: Settings = serde_json::from_value(json!({
            "statuslineInstall": { "installed": installed }
        }))
        .expect("a record without a previous");
        let record = without.statusline_install.expect("the record must load");
        assert_eq!(record.previous, None);
        assert_eq!(
            serde_json::to_value(&record).expect("serialize"),
            json!({ "installed": installed }),
            "a missing previous leaves the field out"
        );
    }

    /// An `accounts.json` written before Task 3 has no `statuslineInstall`, so
    /// seatswap has no record and nothing to restore.
    #[test]
    fn an_older_accounts_file_has_no_statusline_install_record() {
        let tmp = tempfile::tempdir().unwrap();
        let store = store_at(&tmp.path().join("app"));
        fs::create_dir_all(store.app_paths().data_dir.clone()).unwrap();
        fs::write(
            store.app_paths().accounts_file(),
            br#"{"version":1,"accounts":[],"settings":{"trayText":"percentReset"}}"#,
        )
        .unwrap();
        let file = store.load().unwrap();
        assert_eq!(file.settings.statusline_install, None);
    }

    /// A segment written before `color` had a default still loads: a segment
    /// without a colour rule is printed uncoloured (`ColorRule::None`).
    #[test]
    fn a_segment_without_a_colour_rule_loads_uncoloured() {
        let tmp = tempfile::tempdir().unwrap();
        let store = store_at(&tmp.path().join("app"));
        fs::create_dir_all(store.app_paths().data_dir.clone()).unwrap();
        fs::write(
            store.app_paths().accounts_file(),
            br#"{"version":1,"accounts":[],"settings":{"statusline":{"left":[{"kind":"userHost"}],"right":[],"margin":4}}}"#,
        )
        .unwrap();
        let file = store
            .load()
            .expect("a segment without a colour rule must still load");
        assert_eq!(file.settings.statusline.left[0].color, ColorRule::None);
    }

    /// The preset as the page fixture carries it, in the JSON form serde gives
    /// it (camelCase keys, `rule` tags, empty prefix/suffix and absent options
    /// left out), so `panel.js` and the code cannot drift apart.
    #[test]
    fn the_preset_is_the_page_fixture() {
        let expected = serde_json::json!({
            "left": [
                { "kind": "userHost", "color": { "rule": "fixed", "color": "#22c55e" } },
                { "kind": "text", "color": { "rule": "none" }, "text": ":" },
                { "kind": "folder", "color": { "rule": "fixed", "color": "#3b82f6" } },
                { "kind": "model", "color": { "rule": "byModelFamily" }, "prefix": " " },
                { "kind": "effort", "color": { "rule": "byEffort" }, "prefix": " " }
            ],
            "right": [
                { "kind": "account", "color": { "rule": "account" }, "suffix": " " },
                { "kind": "fiveHourPercent", "color": { "rule": "byLoad", "warn": 50, "bad": 80 }, "suffix": "%", "drop": 2 },
                { "kind": "fiveHourReset", "color": { "rule": "none" }, "prefix": " ", "drop": 1, "timeFormat": "compactLeft" },
                { "kind": "weekPercent", "color": { "rule": "byLoad", "warn": 50, "bad": 80 }, "prefix": "  ", "suffix": "%" },
                { "kind": "weekReset", "color": { "rule": "none" }, "prefix": " ", "drop": 1, "timeFormat": "compactLeft" }
            ],
            "margin": 4
        });
        assert_eq!(
            serde_json::to_value(StatuslineConfig::current_line()).unwrap(),
            expected
        );
    }

    /// The preset of the task, segment by segment: kind, colour rule, prefix,
    /// suffix, drop order, time format and text.
    #[test]
    fn the_current_line_is_the_preset_the_task_names() {
        type Parts = (
            SegmentKind,
            ColorRule,
            String,
            String,
            Option<u8>,
            Option<TimeFormat>,
            Option<String>,
        );
        fn parts(segment: &Segment) -> Parts {
            (
                segment.kind,
                segment.color.clone(),
                segment.prefix.clone(),
                segment.suffix.clone(),
                segment.drop,
                segment.time_format,
                segment.text.clone(),
            )
        }

        let config = StatuslineConfig::current_line();
        assert_eq!(config.margin, 4);
        let left: Vec<Parts> = config.left.iter().map(parts).collect();
        assert_eq!(
            left,
            vec![
                (
                    SegmentKind::UserHost,
                    ColorRule::Fixed {
                        color: "#22c55e".to_string()
                    },
                    String::new(),
                    String::new(),
                    None,
                    None,
                    None,
                ),
                (
                    SegmentKind::Text,
                    ColorRule::None,
                    String::new(),
                    String::new(),
                    None,
                    None,
                    Some(":".to_string()),
                ),
                (
                    SegmentKind::Folder,
                    ColorRule::Fixed {
                        color: "#3b82f6".to_string()
                    },
                    String::new(),
                    String::new(),
                    None,
                    None,
                    None,
                ),
                (
                    SegmentKind::Model,
                    ColorRule::ByModelFamily,
                    " ".to_string(),
                    String::new(),
                    None,
                    None,
                    None,
                ),
                (
                    SegmentKind::Effort,
                    ColorRule::ByEffort,
                    " ".to_string(),
                    String::new(),
                    None,
                    None,
                    None,
                ),
            ]
        );
        let right: Vec<Parts> = config.right.iter().map(parts).collect();
        assert_eq!(
            right,
            vec![
                (
                    SegmentKind::Account,
                    ColorRule::Account,
                    String::new(),
                    " ".to_string(),
                    None,
                    None,
                    None,
                ),
                (
                    SegmentKind::FiveHourPercent,
                    ColorRule::ByLoad { warn: 50, bad: 80 },
                    String::new(),
                    "%".to_string(),
                    Some(2),
                    None,
                    None,
                ),
                (
                    SegmentKind::FiveHourReset,
                    ColorRule::None,
                    " ".to_string(),
                    String::new(),
                    Some(1),
                    Some(TimeFormat::CompactLeft),
                    None,
                ),
                (
                    SegmentKind::WeekPercent,
                    ColorRule::ByLoad { warn: 50, bad: 80 },
                    "  ".to_string(),
                    "%".to_string(),
                    None,
                    None,
                    None,
                ),
                (
                    SegmentKind::WeekReset,
                    ColorRule::None,
                    " ".to_string(),
                    String::new(),
                    Some(1),
                    Some(TimeFormat::CompactLeft),
                    None,
                ),
            ]
        );
    }

    /// The edges of the four rules: `warn == bad` is out of order, `bad == 100`
    /// is the last value inside the range, `drop == 1` is the first drop
    /// order, `#ABCDEF` is a `#rrggbb` and `#abc` is not, and a `Text` segment
    /// needs its text (an empty text is a text).
    #[test]
    fn the_edges_of_the_statusline_rules() {
        let mut config = StatuslineConfig::current_line();
        config.right[1].color = ColorRule::ByLoad { warn: 50, bad: 50 };
        assert!(statusline_error(config).contains("statusline.right[1].color"));

        let mut config = StatuslineConfig::current_line();
        config.right[1].color = ColorRule::ByLoad { warn: 99, bad: 100 };
        assert!(settings_with_statusline(config).validate().is_ok());

        let mut config = StatuslineConfig::current_line();
        config.right[2].drop = Some(1);
        assert!(settings_with_statusline(config).validate().is_ok());

        let mut config = StatuslineConfig::current_line();
        config.left[0].color = ColorRule::Fixed {
            color: "#ABCDEF".to_string(),
        };
        assert!(settings_with_statusline(config).validate().is_ok());

        let mut config = StatuslineConfig::current_line();
        config.left[0].color = ColorRule::Fixed {
            color: "#abc".to_string(),
        };
        assert!(statusline_error(config).contains("statusline.left[0].color"));

        let mut config = StatuslineConfig::current_line();
        config.left[1].text = Some(String::new());
        assert!(settings_with_statusline(config).validate().is_ok());

        let mut config = StatuslineConfig::current_line();
        config.left[1].text = None;
        assert!(statusline_error(config).contains("statusline.left[1].text"));
    }

    /// `Settings` has no `deny_unknown_fields`, so an unknown key is ignored
    /// everywhere; inside `statusline` it is ignored too.
    #[test]
    fn unknown_keys_in_the_statusline_are_ignored() {
        let tmp = tempfile::tempdir().unwrap();
        let store = store_at(&tmp.path().join("app"));
        fs::create_dir_all(store.app_paths().data_dir.clone()).unwrap();
        fs::write(
            store.app_paths().accounts_file(),
            br#"{"version":1,"accounts":[],"settings":{"statusline":{"left":[],"right":[],"margin":4,"futureKey":true}}}"#,
        )
        .unwrap();
        let file = store.load().unwrap();
        assert_eq!(file.settings.statusline.margin, 4);
    }

    /// A value the code does not know is an error, not an ignored key: the
    /// whole file cannot be read any more (like every other enum in
    /// `Settings`).
    #[test]
    fn an_unknown_colour_rule_makes_the_file_unreadable() {
        let tmp = tempfile::tempdir().unwrap();
        let store = store_at(&tmp.path().join("app"));
        fs::create_dir_all(store.app_paths().data_dir.clone()).unwrap();
        fs::write(
            store.app_paths().accounts_file(),
            br#"{"version":1,"accounts":[],"settings":{"statusline":{"left":[{"kind":"userHost","color":{"rule":"byLoad2","warn":50,"bad":80}}],"right":[],"margin":4}}}"#,
        )
        .unwrap();
        assert!(matches!(store.load(), Err(StoreError::Corrupt { .. })));
    }

    #[test]
    fn version_two_is_unsupported() {
        let tmp = tempfile::tempdir().unwrap();
        let store = store_at(&tmp.path().join("app"));
        fs::create_dir_all(store.app_paths().data_dir.clone()).unwrap();
        fs::write(
            store.app_paths().accounts_file(),
            br#"{"version":2,"accounts":[],"settings":{}}"#,
        )
        .unwrap();
        assert!(matches!(
            store.load(),
            Err(StoreError::UnsupportedVersion(2))
        ));
    }

    #[test]
    fn corrupt_file_stays_corrupt_and_unchanged() {
        let tmp = tempfile::tempdir().unwrap();
        let store = store_at(&tmp.path().join("app"));
        fs::create_dir_all(store.app_paths().data_dir.clone()).unwrap();
        let path = store.app_paths().accounts_file();
        let bytes = b"{ this is not json";
        fs::write(&path, bytes).unwrap();
        assert!(matches!(store.load(), Err(StoreError::Corrupt { .. })));
        assert_eq!(fs::read(&path).unwrap(), bytes);

        // A version-1 file of the wrong shape is corrupt too, and still untouched.
        let wrong_shape = br#"{"version":1,"accounts":{}}"#;
        fs::write(&path, wrong_shape).unwrap();
        assert!(matches!(store.load(), Err(StoreError::Corrupt { .. })));
        assert_eq!(fs::read(&path).unwrap(), wrong_shape);
    }

    #[test]
    fn settings_validate_rejects_values_outside_the_contract() {
        assert!(Settings::default().validate().is_ok());

        let mut settings = Settings {
            poll_active_secs: 10,
            ..Settings::default()
        };
        assert!(matches!(
            settings.validate(),
            Err(StoreError::InvalidSetting(_))
        ));
        settings.poll_active_secs = 29;
        assert!(settings.validate().is_err());
        settings.poll_active_secs = 30;
        assert!(settings.validate().is_ok());

        settings = Settings {
            poll_inactive_secs: 29,
            ..Settings::default()
        };
        assert!(settings.validate().is_err());
    }

    #[test]
    fn an_older_file_with_suggest_at_still_loads() {
        let tmp = tempfile::tempdir().unwrap();
        let store = store_at(&tmp.path().join("app"));
        fs::create_dir_all(store.app_paths().data_dir.clone()).unwrap();
        fs::write(
            store.app_paths().accounts_file(),
            br#"{"version":1,"accounts":[],"settings":{"suggestAt":90,"suggestTargetBelow":50}}"#,
        )
        .unwrap();
        let file = store.load().unwrap();
        assert!(!file.settings.auto_burn);
        assert!(file.settings.suggest_switches);
    }

    fn settings_with_statusline(config: StatuslineConfig) -> Settings {
        Settings {
            statusline: config,
            ..Settings::default()
        }
    }

    fn statusline_error(config: StatuslineConfig) -> String {
        settings_with_statusline(config)
            .validate()
            .expect_err("this statusline must be rejected")
            .to_string()
    }

    #[test]
    fn validate_rejects_bad_statusline_values() {
        let mut config = StatuslineConfig::current_line();
        config.left[0].color = ColorRule::Fixed {
            color: "#22c5".to_string(),
        };
        assert!(statusline_error(config).contains("statusline.left[0].color"));

        let mut config = StatuslineConfig::current_line();
        config.right[1].color = ColorRule::ByLoad { warn: 80, bad: 50 };
        assert!(statusline_error(config).contains("statusline.right[1].color"));

        let mut config = StatuslineConfig::current_line();
        config.right[1].drop = Some(0);
        assert!(statusline_error(config).contains("statusline.right[1].drop"));

        let mut config = StatuslineConfig::current_line();
        config.left[1].text = None;
        assert!(statusline_error(config).contains("statusline.left[1].text"));
    }

    #[test]
    fn validate_rejects_a_statusline_fixed_colour_that_is_not_rrggbb() {
        let mut config = StatuslineConfig::current_line();
        config.left[0].color = ColorRule::Fixed {
            color: "#22c5".to_string(),
        };
        assert_eq!(
            statusline_error(config),
            "invalid setting: statusline.left[0].color: invalid colour \"#22c5\": use #rrggbb"
        );
    }

    #[test]
    fn validate_rejects_a_statusline_load_rule_out_of_order_or_above_100() {
        let mut config = StatuslineConfig::current_line();
        config.right[1].color = ColorRule::ByLoad { warn: 80, bad: 50 };
        assert_eq!(
            statusline_error(config),
            "invalid setting: statusline.right[1].color: warn (80) must be below bad (50)"
        );

        let mut config = StatuslineConfig::current_line();
        config.right[1].color = ColorRule::ByLoad { warn: 50, bad: 101 };
        assert_eq!(
            statusline_error(config),
            "invalid setting: statusline.right[1].color: bad (101) must be at most 100"
        );
    }

    #[test]
    fn validate_rejects_a_statusline_drop_order_of_zero() {
        let mut config = StatuslineConfig::current_line();
        config.left[0].drop = Some(0);
        assert_eq!(
            statusline_error(config),
            "invalid setting: statusline.left[0].drop: must be at least 1, got 0"
        );
    }

    #[test]
    fn validate_rejects_a_statusline_text_segment_without_text() {
        let mut config = StatuslineConfig::current_line();
        config.left[1].text = None;
        assert_eq!(
            statusline_error(config),
            "invalid setting: statusline.left[1].text: a Text segment needs text"
        );
    }

    /// A separator character belongs to the separator segment; another kind
    /// refuses it, naming the kind and its place, never quietly ignoring it.
    #[test]
    fn validate_refuses_a_separator_char_on_the_wrong_kind() {
        let mut config = StatuslineConfig::current_line();
        config.left[0].separator = Some(SeparatorChar::Dot);
        assert_eq!(
            statusline_error(config),
            "invalid setting: statusline.left[0].separator: user@host does not take a separator character"
        );

        let mut config = StatuslineConfig::everything();
        for segment in config.left.iter_mut() {
            if segment.kind == SegmentKind::Separator {
                segment.separator = Some(SeparatorChar::Chevron);
            }
        }
        assert!(
            settings_with_statusline(config).validate().is_ok(),
            "a separator segment takes a separator character"
        );
    }

    /// The palette's one table lists every kind once, in the page's order, and
    /// no two kinds share a label.
    #[test]
    fn every_kind_has_one_spec() {
        let all = [
            SegmentKind::Account,
            SegmentKind::Plan,
            SegmentKind::FiveHourPercent,
            SegmentKind::FiveHourReset,
            SegmentKind::WeekPercent,
            SegmentKind::WeekReset,
            SegmentKind::ModelPercent,
            SegmentKind::AutoBurn,
            SegmentKind::Model,
            SegmentKind::Effort,
            SegmentKind::Folder,
            SegmentKind::GitBranch,
            SegmentKind::ContextUsed,
            SegmentKind::SessionCost,
            SegmentKind::LinesChanged,
            SegmentKind::UserHost,
            SegmentKind::Clock,
            SegmentKind::Text,
            SegmentKind::Separator,
        ];
        assert_eq!(SegmentKind::ALL, all);
        let mut labels = HashSet::new();
        for kind in SegmentKind::ALL {
            let spec = kind.spec();
            assert_eq!(spec.kind, kind);
            assert!(
                labels.insert(spec.label),
                "the label {:?} is used by two kinds",
                spec.label
            );
        }
    }

    /// A kind refuses a colour rule its spec does not name, with the place and
    /// the kind in the message; every preset stays valid.
    #[test]
    fn validate_refuses_a_colour_rule_the_kind_does_not_take() {
        let mut config = StatuslineConfig::current_line();
        config.left[2].color = ColorRule::ByLoad { warn: 50, bad: 80 };
        assert_eq!(
            statusline_error(config),
            "invalid setting: statusline.left[2].color: folder does not take byLoad"
        );

        let mut config = StatuslineConfig::current_line();
        config.left[0].kind = SegmentKind::Clock;
        config.left[0].color = ColorRule::ByEffort;
        assert_eq!(
            statusline_error(config),
            "invalid setting: statusline.left[0].color: clock does not take byEffort"
        );

        let mut config = StatuslineConfig::current_line();
        config.left[3].color = ColorRule::Account;
        assert_eq!(
            statusline_error(config),
            "invalid setting: statusline.left[3].color: model does not take account"
        );

        for (name, preset) in StatuslineConfig::presets() {
            assert!(
                settings_with_statusline(preset).validate().is_ok(),
                "the preset {name:?} must pass"
            );
        }
    }

    /// A time format belongs to the two reset segments and text to the Text
    /// segment; another kind refuses both.
    #[test]
    fn validate_refuses_a_time_format_or_text_on_the_wrong_kind() {
        let mut config = StatuslineConfig::current_line();
        config.left[0].time_format = Some(TimeFormat::Seatswap);
        assert_eq!(
            statusline_error(config),
            "invalid setting: statusline.left[0].timeFormat: user@host does not take a time format"
        );

        let mut config = StatuslineConfig::current_line();
        config.left[2].text = Some("atlas".to_string());
        assert_eq!(
            statusline_error(config),
            "invalid setting: statusline.left[2].text: folder does not take text"
        );
    }

    #[test]
    fn provider_settings_default_to_off() {
        let defaults = Settings::default();
        assert!(!defaults.codex_card);
        assert_eq!(defaults.codex_home, None);
        assert!(!defaults.ollama_card);
        assert_eq!(defaults.ollama_url, "http://127.0.0.1:11434");

        // An older file without the four keys loads with the defaults.
        let tmp = tempfile::tempdir().unwrap();
        let store = store_at(&tmp.path().join("app"));
        fs::create_dir_all(store.app_paths().data_dir.clone()).unwrap();
        fs::write(
            store.app_paths().accounts_file(),
            br#"{"version":1,"accounts":[],"settings":{"trayText":"percent"}}"#,
        )
        .unwrap();
        let older = store.load().unwrap();
        assert!(!older.settings.codex_card);
        assert_eq!(older.settings.codex_home, None);
        assert!(!older.settings.ollama_card);
        assert_eq!(older.settings.ollama_url, "http://127.0.0.1:11434");
        // The Google card shows only once an account is found, so it is on.
        assert!(defaults.google_card);
        assert!(older.settings.google_card);

        // A file with them loads back the same values.
        let settings = Settings {
            codex_card: true,
            codex_home: Some(PathBuf::from("/x/codex")),
            ollama_card: true,
            ollama_url: "https://ollama.example:8443/base".to_string(),
            ..Settings::default()
        };
        let file = AccountsFile {
            settings: settings.clone(),
            ..AccountsFile::default()
        };
        let text = serde_json::to_string(&file).unwrap();
        for expected in [
            "\"codexCard\":true",
            "\"codexHome\":\"/x/codex\"",
            "\"ollamaCard\":true",
            "\"ollamaUrl\":\"https://ollama.example:8443/base\"",
        ] {
            assert!(text.contains(expected), "missing {expected} in {text}");
        }
        fs::write(store.app_paths().accounts_file(), &text).unwrap();
        let loaded = store.load().unwrap();
        assert_eq!(loaded.settings, settings);
    }

    #[test]
    fn validate_rejects_a_bad_ollama_url() {
        for bad in ["127.0.0.1:11434", "", "http://", "ftp://x"] {
            let settings = Settings {
                ollama_url: bad.to_string(),
                ..Settings::default()
            };
            match settings.validate() {
                Err(StoreError::InvalidSetting(message)) => {
                    assert!(message.contains("ollamaUrl"), "{bad:?}: {message}");
                    let quoted = format!("{bad:?}");
                    assert!(message.contains(&quoted), "{bad:?}: {message}");
                }
                other => panic!("expected InvalidSetting for {bad:?}, got {other:?}"),
            }
        }
        for good in ["http://127.0.0.1:11434", "https://ollama.example:8443/base"] {
            let settings = Settings {
                ollama_url: good.to_string(),
                ..Settings::default()
            };
            assert!(settings.validate().is_ok(), "{good:?} must be accepted");
        }
    }

    /// The task's rule: the URL "must start with `http://` or `https://` and
    /// have a host". A host with a space in it is no host: no HTTP client can
    /// use such a URL, so the setting has to be refused here and not at the
    /// request.
    #[test]
    fn validate_rejects_an_ollama_url_without_a_real_host() {
        for good in [
            "http://127.0.0.1:11434",
            "http://[::1]:11434",
            "https://ollama.example:8443/base",
            "http://127.0.0.1:11434/",
        ] {
            let settings = Settings {
                ollama_url: good.to_string(),
                ..Settings::default()
            };
            assert!(settings.validate().is_ok(), "{good:?} must be accepted");
        }
        for bad in [
            "http://127.0.0.1 :11434",
            "http://ollama host:11434",
            "http://127.0.0.1:11434/\u{7}",
        ] {
            let settings = Settings {
                ollama_url: bad.to_string(),
                ..Settings::default()
            };
            match settings.validate() {
                Err(StoreError::InvalidSetting(message)) => {
                    assert!(message.contains("ollamaUrl"), "{bad:?}: {message}");
                    let quoted = format!("{bad:?}");
                    assert!(message.contains(&quoted), "{bad:?}: {message}");
                }
                other => panic!("expected InvalidSetting for {bad:?}, got {other:?}"),
            }
        }
    }

    #[test]
    fn validate_rejects_an_ollama_url_with_user_info() {
        let url = "http://sam:secret@vega:11434";
        let settings = Settings {
            ollama_url: url.to_string(),
            ..Settings::default()
        };
        match settings.validate() {
            Err(StoreError::InvalidSetting(message)) => {
                assert_eq!(
                    message,
                    "ollamaUrl must not contain a user name or password"
                );
                assert!(!message.contains("secret"), "{message}");
                assert!(!message.contains(url), "{message}");
            }
            other => panic!("expected InvalidSetting for {url:?}, got {other:?}"),
        }
    }

    #[test]
    fn validate_rejects_an_ollama_url_with_an_unclosed_bracket() {
        let url = "http://[";
        let settings = Settings {
            ollama_url: url.to_string(),
            ..Settings::default()
        };
        match settings.validate() {
            Err(StoreError::InvalidSetting(message)) => {
                assert!(message.contains("ollamaUrl"), "{message}");
            }
            other => panic!("expected InvalidSetting for {url:?}, got {other:?}"),
        }
    }

    #[test]
    fn validate_rejects_an_ollama_url_with_an_at_sign_and_no_host() {
        let url = "http://@/";
        let settings = Settings {
            ollama_url: url.to_string(),
            ..Settings::default()
        };
        match settings.validate() {
            Err(StoreError::InvalidSetting(message)) => {
                assert_eq!(
                    message,
                    "ollamaUrl must not contain a user name or password"
                );
                assert!(!message.contains(url), "{message}");
            }
            other => panic!("expected InvalidSetting for {url:?}, got {other:?}"),
        }
    }

    #[test]
    fn validate_rejects_an_ollama_url_with_a_port_but_no_host() {
        let url = "http://:11434";
        let settings = Settings {
            ollama_url: url.to_string(),
            ..Settings::default()
        };
        match settings.validate() {
            Err(StoreError::InvalidSetting(message)) => {
                assert!(message.contains("ollamaUrl"), "{message}");
            }
            other => panic!("expected InvalidSetting for {url:?}, got {other:?}"),
        }
    }

    #[test]
    fn validate_rejects_an_ollama_url_with_a_bracketed_non_ipv6_host() {
        let url = "http://[abc]:11434";
        let settings = Settings {
            ollama_url: url.to_string(),
            ..Settings::default()
        };
        match settings.validate() {
            Err(StoreError::InvalidSetting(message)) => {
                assert!(message.contains("ollamaUrl"), "{message}");
            }
            other => panic!("expected InvalidSetting for {url:?}, got {other:?}"),
        }
    }

    #[test]
    fn validate_rejects_an_ollama_url_with_a_query() {
        let url = "http://127.0.0.1:11434/?x=1";
        let settings = Settings {
            ollama_url: url.to_string(),
            ..Settings::default()
        };
        match settings.validate() {
            Err(StoreError::InvalidSetting(message)) => {
                assert!(message.contains("ollamaUrl"), "{message}");
                assert!(message.contains(&format!("{url:?}")), "{message}");
            }
            other => panic!("expected InvalidSetting for {url:?}, got {other:?}"),
        }
    }

    #[test]
    fn validate_rejects_an_ollama_url_with_a_fragment() {
        let url = "http://127.0.0.1:11434/#top";
        let settings = Settings {
            ollama_url: url.to_string(),
            ..Settings::default()
        };
        match settings.validate() {
            Err(StoreError::InvalidSetting(message)) => {
                assert!(message.contains("ollamaUrl"), "{message}");
                assert!(message.contains(&format!("{url:?}")), "{message}");
            }
            other => panic!("expected InvalidSetting for {url:?}, got {other:?}"),
        }
    }

    #[test]
    fn validate_rejects_an_ollama_url_with_an_empty_path_segment() {
        for url in ["http://127.0.0.1:11434//", "http://127.0.0.1:11434/a//b"] {
            let settings = Settings {
                ollama_url: url.to_string(),
                ..Settings::default()
            };
            match settings.validate() {
                Err(StoreError::InvalidSetting(message)) => {
                    assert!(message.contains("ollamaUrl"), "{message}");
                    assert!(message.contains(&format!("{url:?}")), "{message}");
                }
                other => panic!("expected InvalidSetting for {url:?}, got {other:?}"),
            }
        }
    }

    #[test]
    fn validate_accepts_an_ollama_url_with_a_path_or_a_single_trailing_slash() {
        for url in [
            "https://ollama.example:8443/base",
            "http://127.0.0.1:11434/",
        ] {
            let settings = Settings {
                ollama_url: url.to_string(),
                ..Settings::default()
            };
            assert!(settings.validate().is_ok(), "{url:?} must be accepted");
        }
    }

    #[test]
    fn validate_rejects_a_relative_codex_home() {
        let settings = Settings {
            codex_home: Some(PathBuf::from("codex")),
            ..Settings::default()
        };
        match settings.validate() {
            Err(StoreError::InvalidSetting(message)) => {
                assert!(message.contains("absolute path"), "got {message}");
                assert!(message.contains("codex"), "got {message}");
                assert!(message.contains("codexHome"), "got {message}");
            }
            other => panic!("expected InvalidSetting for a relative codexHome, got {other:?}"),
        }

        let absolute = Settings {
            codex_home: Some(std::env::temp_dir().join(".codex")),
            ..Settings::default()
        };
        assert!(absolute.validate().is_ok());
    }

    #[test]
    fn settings_validate_rejects_a_relative_claude_config_dir() {
        for relative in ["~/.claude", "claude"] {
            let settings = Settings {
                claude_config_dir: Some(PathBuf::from(relative)),
                ..Settings::default()
            };
            match settings.validate() {
                Err(StoreError::InvalidSetting(message)) => {
                    assert!(message.contains("absolute path"), "got {message}");
                    assert!(message.contains(relative), "got {message}");
                }
                other => panic!("expected InvalidSetting for {relative}, got {other:?}"),
            }
        }

        let absolute = Settings {
            claude_config_dir: Some(std::env::temp_dir().join(".claude")),
            ..Settings::default()
        };
        assert!(absolute.validate().is_ok());
    }

    #[test]
    fn settings_from_an_older_file_get_the_new_defaults() {
        let tmp = tempfile::tempdir().unwrap();
        let store = store_at(&tmp.path().join("app"));
        fs::create_dir_all(store.app_paths().data_dir.clone()).unwrap();
        fs::write(
            store.app_paths().accounts_file(),
            br#"{"version":1,"accounts":[],"settings":{"trayText":"percent"}}"#,
        )
        .unwrap();
        let file = store.load().unwrap();
        assert!(file.settings.read_sessions);
        assert!(!file.settings.agents_switch);
        assert!(file.settings.statusline_feed);
        assert!(
            file.settings.notify_login_ends,
            "an older file without notifyLoginEnds loads with it on"
        );
        assert_eq!(file.settings.notify_waiting_hosts, WaitingHosts::ThisHost);
        assert_eq!(file.settings.history_dir, None);
        assert_eq!(file.settings.currency, None);
        assert!(file.settings.look_up_prices);
    }

    #[test]
    fn validate_rejects_a_bad_currency() {
        for currency in ["eur", "EURO", "EU1", "E U"] {
            let settings = Settings {
                currency: Some(currency.to_string()),
                ..Settings::default()
            };
            assert!(matches!(
                settings.validate(),
                Err(StoreError::InvalidSetting(_))
            ));
        }
        let good = Settings {
            currency: Some("EUR".to_string()),
            look_up_prices: false,
            ..Settings::default()
        };
        assert!(good.validate().is_ok());
    }

    /// The shared list names real keys of the settings file: a typo would
    /// share nothing and say nothing.
    #[test]
    fn every_shared_setting_is_a_key_of_the_settings_file() {
        let settings = Settings::default();
        for key in SHARED_SETTINGS {
            assert!(settings.value_of(key).is_ok(), "{key} is a settings key");
        }
        assert!(matches!(
            settings.value_of("noSuchSetting"),
            Err(StoreError::UnknownSetting(_))
        ));
    }

    #[test]
    fn with_setting_changes_one_key_and_checks_the_whole() {
        let settings = Settings {
            auto_burn: true,
            ..Settings::default()
        };
        let merged = settings
            .with_setting("currency", serde_json::json!("EUR"))
            .expect("the currency is merged");
        assert_eq!(merged.currency.as_deref(), Some("EUR"));
        assert!(merged.auto_burn, "every other key is kept");
        assert!(matches!(
            settings.with_setting("currency", serde_json::json!("eur")),
            Err(StoreError::InvalidSetting(_))
        ));
        let wrong_type = settings
            .with_setting("notifyWaiting", serde_json::json!("yes"))
            .expect_err("a text is not a switch");
        assert!(
            wrong_type.to_string().starts_with("notifyWaiting: "),
            "{wrong_type}"
        );
        assert_eq!(
            settings
                .with_setting("noSuchSetting", serde_json::json!(true))
                .expect_err("an unknown key")
                .to_string(),
            "unknown setting noSuchSetting"
        );
    }

    /// A file written by 1.1.0 has `usdRate`; it is no longer a setting, so it
    /// must not stop the file from loading (Andreas: silently).
    #[test]
    fn an_old_settings_file_with_usd_rate_reads() {
        let tmp = tempfile::tempdir().unwrap();
        let store = store_at(&tmp.path().join("app"));
        fs::create_dir_all(store.app_paths().data_dir.clone()).unwrap();
        fs::write(
            store.app_paths().accounts_file(),
            br#"{"version":1,"accounts":[],"settings":{"currency":"EUR","usdRate":{"currency":"EUR","value":0.95},"lookUpPrices":true}}"#,
        )
        .unwrap();
        let file = store.load().unwrap();
        assert_eq!(file.settings.currency.as_deref(), Some("EUR"));
        assert!(file.settings.look_up_prices);
        let json = serde_json::to_value(&file.settings).expect("settings JSON");
        assert!(
            json.get("usdRate").is_none(),
            "the removed setting is not written back: {json}"
        );
    }

    #[test]
    fn validate_rejects_a_bad_host_name() {
        let bad = Settings {
            host_name: Some("my host".to_string()),
            ..Settings::default()
        };
        match bad.validate() {
            Err(StoreError::InvalidSetting(message)) => {
                assert!(message.contains("hostName"), "got {message}");
            }
            other => panic!("expected InvalidSetting, got {other:?}"),
        }

        let good = Settings {
            host_name: Some("vega".to_string()),
            ..Settings::default()
        };
        assert!(good.validate().is_ok());
    }

    #[test]
    fn validate_rejects_an_empty_host_name() {
        let empty = Settings {
            host_name: Some(String::new()),
            ..Settings::default()
        };
        match empty.validate() {
            Err(StoreError::InvalidSetting(message)) => {
                assert!(message.contains("hostName"), "got {message}");
            }
            other => panic!("expected InvalidSetting, got {other:?}"),
        }

        let absent = Settings {
            host_name: None,
            ..Settings::default()
        };
        assert!(absent.validate().is_ok());
    }

    #[test]
    fn validate_rejects_a_relative_history_dir() {
        let settings = Settings {
            history_dir: Some(PathBuf::from("history")),
            ..Settings::default()
        };
        match settings.validate() {
            Err(StoreError::InvalidSetting(message)) => {
                assert!(message.contains("historyDir"), "got {message}");
                assert!(message.contains("absolute path"), "got {message}");
            }
            other => panic!("expected InvalidSetting, got {other:?}"),
        }
    }

    #[test]
    fn load_accounts_reads_a_saved_file_without_a_secret_store() {
        let tmp = tempfile::tempdir().unwrap();
        let store = store_at(&tmp.path().join("app"));
        let mut file = AccountsFile::default();
        file.settings.claude_config_dir = Some(PathBuf::from("/home/u/.claude"));
        store.save(&file).unwrap();

        let loaded = load_accounts(store.app_paths()).unwrap();
        assert_eq!(loaded, file);

        let path = store.app_paths().accounts_file();
        fs::write(&path, b"{ not json").unwrap();
        match load_accounts(store.app_paths()) {
            Err(StoreError::Corrupt { path: reported }) => assert_eq!(reported, path),
            other => panic!("expected Corrupt, got {other:?}"),
        }
    }

    #[test]
    fn file_secrets_round_trip_overwrite_and_delete() {
        let tmp = tempfile::tempdir().unwrap();
        let dir = tmp.path().join("secrets");
        let secrets: Box<dyn SecretStore> = Box::new(FileSecrets::new(dir.clone()));
        let login = sample_login();
        assert!(!secrets.encrypted());
        assert_eq!(secrets.get(ONE_ID).unwrap(), None);

        secrets.set(ONE_ID, &login).unwrap();
        assert_eq!(secrets.get(ONE_ID).unwrap(), Some(login.clone()));
        let path = dir.join(format!("{ONE_ID}.json"));
        #[cfg(unix)]
        assert_eq!(mode(&path), 0o600);
        #[cfg(unix)]
        assert_eq!(mode(&dir), 0o700);

        let mut changed = login.clone();
        let oauth = changed
            .0
            .get_mut("claudeAiOauth")
            .and_then(Value::as_object_mut)
            .unwrap();
        oauth.insert(
            "accessToken".to_string(),
            json!(format!("sk-ant-oat01-{}", "d".repeat(95))),
        );
        secrets.set(ONE_ID, &changed).unwrap();
        assert_eq!(secrets.get(ONE_ID).unwrap(), Some(changed));

        secrets.delete(ONE_ID).unwrap();
        assert_eq!(secrets.get(ONE_ID).unwrap(), None);
        assert!(!path.exists());
        // Deleting a missing login is fine.
        secrets.delete(ONE_ID).unwrap();
    }

    /// A Claude login is still stored as the compact JSON of its keys in their
    /// order: the bytes 1.2.2 wrote. `get`/`set` became provided methods over
    /// the value methods; the text did not change. The keyring writes the same
    /// text (`serde_json::to_string` of the same value).
    #[test]
    fn a_claude_login_is_stored_as_before() {
        let tmp = tempfile::tempdir().unwrap();
        let dir = tmp.path().join("secrets");
        let secrets = FileSecrets::new(dir.clone());
        let access = format!("sk-ant-oat01-{}", "a".repeat(95));
        let refresh = format!("sk-ant-ort01-{}", "b".repeat(95));
        let mut oauth = serde_json::Map::new();
        oauth.insert("accessToken".to_string(), json!(access));
        oauth.insert("refreshToken".to_string(), json!(refresh));
        oauth.insert("expiresAt".to_string(), json!(1_795_000_000_000i64));
        oauth.insert(
            "scopes".to_string(),
            json!(["user:profile", "user:inference"]),
        );
        oauth.insert("subscriptionType".to_string(), json!("max"));
        let mut map = serde_json::Map::new();
        map.insert("claudeAiOauth".to_string(), Value::Object(oauth));
        map.insert("organizationUuid".to_string(), json!(ONE_ORG_UUID));
        let login = Login(map);

        secrets.set(ONE_ID, &login).unwrap();

        let expected = format!(
            "{{\"claudeAiOauth\":{{\"accessToken\":\"{access}\",\"refreshToken\":\"{refresh}\",\"expiresAt\":1795000000000,\"scopes\":[\"user:profile\",\"user:inference\"],\"subscriptionType\":\"max\"}},\"organizationUuid\":\"{ONE_ORG_UUID}\"}}"
        );
        let path = dir.join(format!("{ONE_ID}.json"));
        assert_eq!(fs::read_to_string(&path).unwrap(), expected);
        assert_eq!(secrets.get(ONE_ID).unwrap(), Some(login));
    }

    /// On Windows with the keyring backend, Claude logins stay in the keyring
    /// (`main`) and Codex logins go to files (`codex`) because a Codex login is
    /// past Credential Manager's 2560 bytes per entry (R7, docs/windows.md §15).
    /// Two `FileSecrets` folders stand in for the two stores.
    #[test]
    fn routed_secrets_keep_codex_logins_apart() {
        let tmp = tempfile::tempdir().unwrap();
        let main_dir = tmp.path().join("keyring");
        let codex_dir = tmp.path().join("codex");
        let routed = RoutedSecrets {
            main: Box::new(FileSecrets::new(main_dir.clone())),
            codex: FileSecrets::new(codex_dir.clone()),
        };

        let claude = sample_login();
        let codex = sample_codex_login();
        routed.set(ONE_ID, &claude).unwrap();
        routed.set_codex(TWO_ID, &codex).unwrap();

        // Each reads back through the routed store.
        assert_eq!(routed.get(ONE_ID).unwrap(), Some(claude.clone()));
        assert_eq!(routed.get_codex(TWO_ID).unwrap(), Some(codex.clone()));

        // Each lives in its own store only.
        let main = FileSecrets::new(main_dir.clone());
        let codex_store = FileSecrets::new(codex_dir.clone());
        assert_eq!(main.get(ONE_ID).unwrap(), Some(claude));
        assert_eq!(main.get(TWO_ID).unwrap(), None);
        assert_eq!(codex_store.get_codex(TWO_ID).unwrap(), Some(codex.clone()));
        assert_eq!(codex_store.get(ONE_ID).unwrap(), None);
        assert!(main_dir.join(format!("{ONE_ID}.json")).exists());
        assert!(!main_dir.join(format!("{TWO_ID}.json")).exists());
        assert!(codex_dir.join(format!("{TWO_ID}.json")).exists());
        assert!(!codex_dir.join(format!("{ONE_ID}.json")).exists());

        // The Codex login really is the size Credential Manager cannot take.
        let text = serde_json::to_string(&Value::Object(codex.0)).unwrap();
        let chars = text.chars().count();
        assert!(
            (3_500..4_000).contains(&chars),
            "the fixture Codex login is {chars} characters, not the real size"
        );
        assert_eq!(routed.encrypted(), main.encrypted());
    }

    #[test]
    fn removing_a_codex_login_deletes_its_file() {
        let tmp = tempfile::tempdir().unwrap();
        let main_dir = tmp.path().join("keyring");
        let codex_dir = tmp.path().join("codex");
        let routed = RoutedSecrets {
            main: Box::new(FileSecrets::new(main_dir.clone())),
            codex: FileSecrets::new(codex_dir.clone()),
        };
        routed.set_codex(TWO_ID, &sample_codex_login()).unwrap();
        // The same id in `main` too: a delete removes it from both stores.
        routed.set(TWO_ID, &sample_login()).unwrap();
        let codex_path = codex_dir.join(format!("{TWO_ID}.json"));
        let main_path = main_dir.join(format!("{TWO_ID}.json"));
        assert!(codex_path.exists());
        assert!(main_path.exists());

        routed.delete(TWO_ID).unwrap();

        assert!(!codex_path.exists());
        assert!(!main_path.exists());
        assert_eq!(routed.get_codex(TWO_ID).unwrap(), None);
        assert_eq!(routed.get(TWO_ID).unwrap(), None);
        // Deleting a login that is not there is fine.
        routed.delete(TWO_ID).unwrap();
    }

    #[test]
    fn file_secrets_rejects_ids_that_are_not_32_lowercase_hex() {
        let tmp = tempfile::tempdir().unwrap();
        let dir = tmp.path().join("secrets");
        let secrets = FileSecrets::new(dir.clone());
        let login = sample_login();
        let bad_ids = [
            "../x".to_string(),
            String::new(),
            "A".repeat(32),
            "a".repeat(31),
            "a".repeat(33),
            "g".repeat(32),
            "../../../../etc/passwd".to_string(),
        ];
        for bad in &bad_ids {
            assert!(matches!(secrets.get(bad), Err(StoreError::InvalidId(_))));
            assert!(matches!(
                secrets.set(bad, &login),
                Err(StoreError::InvalidId(_))
            ));
            assert!(matches!(secrets.delete(bad), Err(StoreError::InvalidId(_))));
        }
        assert!(!dir.exists());
    }

    #[test]
    fn default_abbr_cases() {
        assert_eq!(default_abbr("Acme Team").unwrap(), "AT");
        assert_eq!(default_abbr("Acme").unwrap(), "AC");
        assert_eq!(default_abbr("Private").unwrap(), "PR");
        assert_eq!(default_abbr("x").unwrap(), "X");
        assert_eq!(default_abbr("acme-team").unwrap(), "AT");
        assert_eq!(default_abbr("Ünïcode Bank").unwrap(), "NB");
        assert_eq!(default_abbr("42").unwrap(), "42");
        assert!(matches!(
            default_abbr("  "),
            Err(StoreError::InvalidLabel(_))
        ));
        assert!(matches!(default_abbr(""), Err(StoreError::InvalidLabel(_))));
        assert!(matches!(
            default_abbr("🚀"),
            Err(StoreError::InvalidLabel(_))
        ));
    }

    #[test]
    fn validate_abbr_cases() {
        assert_eq!(validate_abbr("as", &[]).unwrap(), "AS");
        assert_eq!(validate_abbr(" a ", &[]).unwrap(), "A");
        assert_eq!(validate_abbr("9", &[]).unwrap(), "9");
        assert!(matches!(
            validate_abbr("ABC", &[]),
            Err(StoreError::InvalidAbbr(_))
        ));
        assert!(matches!(
            validate_abbr("A-", &[]),
            Err(StoreError::InvalidAbbr(_))
        ));
        assert!(matches!(
            validate_abbr("", &[]),
            Err(StoreError::InvalidAbbr(_))
        ));
        assert!(matches!(
            validate_abbr("Ü", &[]),
            Err(StoreError::InvalidAbbr(_))
        ));
        assert!(matches!(
            validate_abbr("A", &["a"]),
            Err(StoreError::DuplicateAbbr(_))
        ));
        assert_eq!(validate_abbr("b", &["A", "C"]).unwrap(), "B");
    }

    #[test]
    fn validate_color_cases() {
        assert_eq!(validate_color("#3B82F6").unwrap(), "#3b82f6");
        assert_eq!(validate_color("#3b82f6").unwrap(), "#3b82f6");
        assert!(matches!(
            validate_color("3b82f6"),
            Err(StoreError::InvalidColor(_))
        ));
        assert!(matches!(
            validate_color("#3b82f"),
            Err(StoreError::InvalidColor(_))
        ));
        assert!(matches!(
            validate_color("#3b82f60"),
            Err(StoreError::InvalidColor(_))
        ));
        assert!(matches!(
            validate_color("#gggggg"),
            Err(StoreError::InvalidColor(_))
        ));
    }

    #[test]
    fn plan_label_cases() {
        let personal = |name: &str| {
            let mut identity = sample_identity();
            identity
                .0
                .insert("planDisplayName".to_string(), json!(name));
            identity
        };
        // Personal plans: Claude's own name, never the tier multiplier.
        assert_eq!(
            plan_label(
                &login_with(Some("max"), Some("default_claude_max_20x")),
                &personal("Ultra")
            )
            .as_deref(),
            Some("Ultra")
        );
        assert_eq!(
            plan_label(
                &login_with(Some("max"), Some("default_claude_max_5x")),
                &personal("Max")
            )
            .as_deref(),
            Some("Max")
        );
        // A plan with a name of its own keeps it, whatever the tier says.
        assert_eq!(
            plan_label(
                &login_with(Some("max"), Some("default_claude_max_20x")),
                &personal("Max")
            )
            .as_deref(),
            Some("Max")
        );
        assert_eq!(
            plan_label(
                &login_with(Some("pro"), Some("default_claude_pro")),
                &personal("Pro")
            )
            .as_deref(),
            Some("Pro")
        );
        // Without a display name, Claude's current names for the old tiers:
        // a Team seat on the 5x tier is "Team Premium", a personal max_5x is
        // "Max" and max_20x is "Ultra".
        assert_eq!(
            plan_label(
                &login_with(Some("team"), Some("default_claude_max_5x")),
                &sample_identity()
            )
            .as_deref(),
            Some("Team Premium")
        );
        assert_eq!(
            plan_label(
                &login_with(Some("max"), Some("default_claude_max_5x")),
                &sample_identity()
            )
            .as_deref(),
            Some("Max")
        );
        assert_eq!(
            plan_label(
                &login_with(Some("team"), Some("default_claude_team_premium")),
                &sample_identity()
            )
            .as_deref(),
            Some("Team (team_premium)")
        );
        assert_eq!(
            plan_label(&login_with(Some("team"), None), &sample_identity()).as_deref(),
            Some("Team")
        );
        assert_eq!(
            plan_label(
                &login_with(Some("max"), Some("default_claude_max_20x")),
                &sample_identity()
            )
            .as_deref(),
            Some("Ultra")
        );
        // A tier the table does not name keeps its multiplier.
        assert_eq!(
            plan_label(
                &login_with(Some("team"), Some("default_claude_max_20x")),
                &sample_identity()
            )
            .as_deref(),
            Some("Team 20x")
        );
        assert_eq!(
            plan_label(
                &login_with(Some("enterprise"), Some("default_claude_max_20x")),
                &sample_identity()
            )
            .as_deref(),
            Some("Enterprise 20x")
        );
        // A blank display name does not hide the subscription type.
        assert_eq!(
            plan_label(&login_with(Some("pro"), None), &personal("  ")).as_deref(),
            Some("Pro")
        );
        assert_eq!(
            plan_label(&login_with(None, None), &sample_identity()),
            None
        );
    }

    /// The tier shapes of spec §3.7.1: the multiplier at the end of the value
    /// is read as it is, a value that is not a multiplier is shown without the
    /// `default_claude_` part, and nothing is guessed.
    #[test]
    fn plan_label_reads_every_tier_shape() {
        let label = |tier: Option<&str>| {
            plan_label(&login_with(Some("team"), tier), &sample_identity())
                .as_deref()
                .map(str::to_string)
        };
        // The multiplier at the end of the value.
        assert_eq!(
            label(Some("default_claude_max_20x")),
            Some("Team 20x".to_string())
        );
        assert_eq!(
            label(Some("default_claude_max_5x")),
            Some("Team Premium".to_string())
        );
        // A tier without the prefix: the multiplier is still the end of it.
        assert_eq!(label(Some("max_5x")), Some("Team Premium".to_string()));
        // A tier that is not a multiplier is shown as it is, without the prefix.
        assert_eq!(
            label(Some("default_claude_team_premium")),
            Some("Team (team_premium)".to_string())
        );
        assert_eq!(
            label(Some("team_premium")),
            Some("Team (team_premium)".to_string())
        );
        // An empty tier adds nothing.
        assert_eq!(label(Some("")), Some("Team".to_string()));
        // "_x" without digits is not a multiplier: the value is shown as it is.
        assert_eq!(
            label(Some("default_claude_max_x")),
            Some("Team (max_x)".to_string())
        );
        // "_05x" is shown as it is: nothing is guessed.
        assert_eq!(
            label(Some("default_claude_max_05x")),
            Some("Team 05x".to_string())
        );
        // No tier at all adds nothing.
        assert_eq!(label(None), Some("Team".to_string()));
    }

    #[test]
    fn a_price_must_be_positive() {
        assert!(matches!(
            validate_price(Some(0.0)),
            Err(StoreError::InvalidPrice(_))
        ));
        assert!(matches!(
            validate_price(Some(-1.0)),
            Err(StoreError::InvalidPrice(_))
        ));
        assert_eq!(validate_price(Some(180.0)).unwrap(), Some(180.0));
        assert_eq!(validate_price(None).unwrap(), None);
    }

    #[test]
    fn noting_a_stored_login_updates_plan_end_and_state_once() {
        let identity = identity(
            ONE_ACCOUNT_UUID,
            ONE_ORG_UUID,
            "Acme",
            "someone@example.com",
        );
        let login = login_with(Some("team"), Some("default_claude_max_5x"));
        let mut account =
            new_account(&login, &identity, None, &AccountsFile::default(), now()).unwrap();
        account.plan = Some("team · max 5x".to_string());
        account.login_state = LoginState::LoggedOut;

        let note = account.note_stored_login(&login);
        assert!(note.changed);
        assert!(note.plan_changed);
        assert_eq!(note.previous_plan.as_deref(), Some("team · max 5x"));
        assert_eq!(account.plan.as_deref(), Some("Team Premium"));
        assert_eq!(account.login_state, LoginState::Ok);
        assert_eq!(
            account.login_ends_at_ms,
            login.refresh_token_expires_at_ms()
        );
        let again = account.note_stored_login(&login);
        assert!(!again.changed, "nothing changes the second time");
        assert!(!again.plan_changed);
    }

    /// The capacity comes from the identity's tiers: the seat's first, then
    /// the organization's; a tier without a multiplier gives none.
    #[test]
    fn capacity_reads_the_seat_tier_then_the_organization_tier() {
        assert_eq!(tier_capacity("default_claude_max_20x"), Some(20.0));
        assert_eq!(tier_capacity("default_claude_max_5x"), Some(5.0));
        assert_eq!(tier_capacity("default_raven"), None);
        assert_eq!(tier_capacity("default_claude_max_0x"), None);
        let identity = |pairs: &[(&str, &str)]| {
            let mut identity = sample_identity();
            for (key, value) in pairs {
                identity.0.insert(key.to_string(), json!(value));
            }
            identity
        };
        let login = login_with(Some("team"), Some("default_claude_max_5x"));
        let mut account = new_account(
            &login,
            &sample_identity(),
            None,
            &AccountsFile::default(),
            now(),
        )
        .unwrap();
        // A Team Premium seat: the seat's tier wins over the organization's.
        account.oauth_account = identity(&[
            ("userRateLimitTier", "default_claude_max_5x"),
            ("organizationRateLimitTier", "default_raven"),
        ]);
        assert_eq!(account.capacity(), Some(5.0));
        // Ultra: only the organization's tier.
        account.oauth_account =
            identity(&[("organizationRateLimitTier", "default_claude_max_20x")]);
        assert_eq!(account.capacity(), Some(20.0));
        // A blank seat tier counts as absent.
        account.oauth_account = identity(&[
            ("userRateLimitTier", " "),
            ("organizationRateLimitTier", "default_claude_max_5x"),
        ]);
        assert_eq!(account.capacity(), Some(5.0));
        // No tier, or one without a multiplier: no capacity.
        account.oauth_account = identity(&[("organizationRateLimitTier", "default_raven")]);
        assert_eq!(account.capacity(), None);
        account.oauth_account = sample_identity();
        assert_eq!(account.capacity(), None);
    }

    /// Pro's tier names no multiplier: its plan's name makes it capacity 1. A
    /// tier with a multiplier still wins, and another plan without one stays
    /// unknown.
    #[test]
    fn capacity_of_pro_is_one_by_its_plan_name() {
        let login = login_with(Some("pro"), Some("default_claude_ai"));
        let mut account = new_account(
            &login,
            &sample_identity(),
            None,
            &AccountsFile::default(),
            now(),
        )
        .unwrap();
        account.plan = Some("Pro".to_string());
        account.oauth_account = sample_identity();
        assert_eq!(account.capacity(), Some(1.0));
        account.oauth_account.0.insert(
            "organizationRateLimitTier".to_string(),
            json!("default_claude_ai"),
        );
        assert_eq!(account.capacity(), Some(1.0));
        account.oauth_account.0.insert(
            "userRateLimitTier".to_string(),
            json!("default_claude_max_5x"),
        );
        assert_eq!(account.capacity(), Some(5.0), "a tier's multiplier wins");
        account.oauth_account = sample_identity();
        account.plan = Some("Team Standard".to_string());
        assert_eq!(account.capacity(), None);
    }

    /// An old label giving way to Claude's current name is saved, but it is no
    /// plan change: the history gets no `PlanChanged` for a rename.
    #[test]
    fn a_renamed_plan_is_saved_but_is_no_plan_change() {
        let identity = identity(
            ONE_ACCOUNT_UUID,
            ONE_ORG_UUID,
            "Acme",
            "someone@example.com",
        );
        for (subscription, tier, old, new) in [
            ("team", "default_claude_max_5x", "Team 5x", "Team Premium"),
            ("max", "default_claude_max_5x", "Max 5x", "Max"),
            ("max", "default_claude_max_20x", "Max 20x", "Ultra"),
        ] {
            let login = login_with(Some(subscription), Some(tier));
            let mut account =
                new_account(&login, &identity, None, &AccountsFile::default(), now()).unwrap();
            account.plan = Some(old.to_string());
            let note = account.note_stored_login(&login);
            assert!(note.changed, "{old}: the new name is saved");
            assert!(!note.plan_changed, "{old}: a rename is no plan change");
            assert_eq!(account.plan.as_deref(), Some(new));
        }
        // An old label of another plan is still a change.
        let login = login_with(Some("max"), Some("default_claude_max_20x"));
        let mut account =
            new_account(&login, &identity, None, &AccountsFile::default(), now()).unwrap();
        account.plan = Some("Max 5x".to_string());
        assert!(account.note_stored_login(&login).plan_changed);
    }

    #[test]
    fn new_account_defaults_from_the_identity() {
        let identity = identity(
            ONE_ACCOUNT_UUID,
            ONE_ORG_UUID,
            "Acme",
            "someone@example.com",
        );
        let file = AccountsFile::default();
        let now = now();
        let account = new_account(&sample_login(), &identity, None, &file, now).unwrap();
        assert_eq!(account.label, "Acme");
        assert_eq!(account.abbr, "AC");
        assert_eq!(account.color, "#3b82f6");
        assert_eq!(account.email.as_deref(), Some("someone@example.com"));
        assert_eq!(account.org_name.as_deref(), Some("Acme"));
        assert_eq!(account.plan.as_deref(), Some("Team Premium"));
        assert_eq!(account.price, None);
        assert_eq!(account.account_uuid, ONE_ACCOUNT_UUID);
        assert_eq!(account.organization_uuid, ONE_ORG_UUID);
        assert_eq!(account.oauth_account, identity);
        assert_eq!(account.added_at, now);
        assert_eq!(account.last_used_at, None);
        assert_eq!(account.login_state, LoginState::Ok);
        assert_eq!(account.login_ends_at_ms, Some(1796772840000));
        assert_eq!(account.id.len(), 32);
        assert!(account
            .id
            .bytes()
            .all(|b| b.is_ascii_digit() || (b'a'..=b'f').contains(&b)));
    }

    #[test]
    fn new_account_abbr_falls_back_when_the_default_is_taken() {
        let now = now();
        let mut file = AccountsFile::default();
        let first_identity = identity(ONE_ACCOUNT_UUID, ONE_ORG_UUID, "Acme", "one@example.com");
        file.accounts
            .push(new_account(&sample_login(), &first_identity, None, &file, now).unwrap());
        let second_identity = identity(TWO_ACCOUNT_UUID, TWO_ORG_UUID, "Acme", "two@example.com");
        let second = new_account(&sample_login(), &second_identity, None, &file, now).unwrap();
        // "AC" is taken; the label's first letter plus the next free letter is "AM".
        assert_eq!(second.abbr, "AM");
        assert_eq!(second.color, "#f59e0b");
        assert_eq!(
            second.label, "Acme",
            "the second account still gets the org name"
        );
    }

    #[test]
    fn new_account_rejects_a_duplicate_identity() {
        let now = now();
        let mut file = AccountsFile::default();
        let existing_identity = identity(
            ONE_ACCOUNT_UUID,
            ONE_ORG_UUID,
            "Acme",
            "someone@example.com",
        );
        file.accounts
            .push(new_account(&sample_login(), &existing_identity, None, &file, now).unwrap());
        assert!(matches!(
            new_account(&sample_login(), &existing_identity, None, &file, now),
            Err(StoreError::DuplicateAccount { .. })
        ));
    }

    #[test]
    fn new_account_trims_the_explicit_label_and_rejects_a_long_one() {
        let now = now();
        let file = AccountsFile::default();
        let account = new_account(
            &sample_login(),
            &sample_identity(),
            Some("  My Account  "),
            &file,
            now,
        )
        .unwrap();
        assert_eq!(account.label, "My Account");
        assert_eq!(account.abbr, "MA");

        assert!(matches!(
            new_account(&sample_login(), &sample_identity(), Some("   "), &file, now),
            Err(StoreError::InvalidLabel(_))
        ));
        let long = "x".repeat(41);
        assert!(matches!(
            new_account(&sample_login(), &sample_identity(), Some(&long), &file, now),
            Err(StoreError::InvalidLabel(_))
        ));
    }

    #[test]
    fn find_by_key_accepts_the_id_and_the_abbreviation_in_lower_case() {
        let now = now();
        let mut file = AccountsFile::default();
        let account = new_account(
            &sample_login(),
            &sample_identity(),
            Some("Acme"),
            &file,
            now,
        )
        .unwrap();
        let id = account.id.clone();
        file.accounts.push(account);

        assert_eq!(
            file.find_by_key(&id).map(|a| a.id.as_str()),
            Some(id.as_str())
        );
        assert_eq!(
            file.find_by_key("ac").map(|a| a.id.as_str()),
            Some(id.as_str())
        );
        assert_eq!(file.find(&id).map(|a| a.id.as_str()), Some(id.as_str()));
        assert_eq!(
            file.find_by_identity(ONE_ACCOUNT_UUID, ONE_ORG_UUID)
                .map(|a| a.id.as_str()),
            Some(id.as_str())
        );
        assert!(file.find_by_key("zz").is_none());
    }

    #[test]
    fn first_use_with_a_working_probe_records_the_keyring() {
        let tmp = tempfile::tempdir().unwrap();
        let app = AppPaths::at(&tmp.path().join("app"));

        let (store, warning) = open_store_with(app.clone(), || Ok(())).unwrap();
        assert_eq!(warning, None);
        assert!(store.secrets().encrypted());

        let raw = fs::read_to_string(app.accounts_file()).unwrap();
        assert!(
            raw.contains("\"secretsBackend\": \"keyring\""),
            "accounts.json must record the keyring: {raw}"
        );
        assert_eq!(
            store.load().unwrap().secrets_backend,
            Some(SecretsBackend::Keyring)
        );
    }

    #[test]
    fn first_use_with_a_failing_probe_records_files_and_warns() {
        let tmp = tempfile::tempdir().unwrap();
        let app = AppPaths::at(&tmp.path().join("app"));
        let secrets_dir = app.secrets_dir();

        let (store, warning) =
            open_store_with(app.clone(), || Err("D-Bus is slow".to_string())).unwrap();
        let warning = warning.expect("using files always warns");
        assert!(
            warning.contains(&secrets_dir.display().to_string()),
            "the warning must name the secrets folder, got: {warning}"
        );
        assert!(
            warning.contains("unencrypted"),
            "the warning must say the files are not encrypted, got: {warning}"
        );
        assert!(!store.secrets().encrypted());

        let raw = fs::read_to_string(app.accounts_file()).unwrap();
        assert!(
            raw.contains("\"secretsBackend\": \"file\""),
            "accounts.json must record the files: {raw}"
        );
        assert_eq!(
            store.load().unwrap().secrets_backend,
            Some(SecretsBackend::File)
        );
    }

    #[test]
    fn a_recorded_keyring_never_falls_back_to_files() {
        let tmp = tempfile::tempdir().unwrap();
        let app = AppPaths::at(&tmp.path().join("app"));
        let bootstrap = store_at(&app.data_dir);
        let file = AccountsFile {
            secrets_backend: Some(SecretsBackend::Keyring),
            ..AccountsFile::default()
        };
        bootstrap.save(&file).unwrap();
        let before = fs::read(app.accounts_file()).unwrap();

        match open_store_with(app.clone(), || Err("the keyring is locked".to_string())) {
            Err(StoreError::Keyring(message)) => {
                assert!(
                    message.contains("the logins are stored in the keyring"),
                    "the error must say where the logins are, got: {message}"
                );
                assert!(
                    message.contains("the keyring is locked"),
                    "the error must give the probe reason, got: {message}"
                );
            }
            Err(e) => panic!("expected StoreError::Keyring, got {e}"),
            Ok(_) => panic!("a recorded keyring must not fall back to files"),
        }
        assert_eq!(
            fs::read(app.accounts_file()).unwrap(),
            before,
            "accounts.json must be unchanged when opening fails"
        );
        assert!(
            !app.secrets_dir().exists(),
            "no fallback folder may be created for a recorded keyring"
        );
    }

    #[test]
    fn a_recorded_file_backend_never_probes_the_keyring() {
        let tmp = tempfile::tempdir().unwrap();
        let app = AppPaths::at(&tmp.path().join("app"));

        let no_probe = || -> Result<(), String> {
            panic!("the probe must not run when the file backend is recorded")
        };
        let (secrets, backend, warning) =
            open_secrets_with(&app, Some(SecretsBackend::File), no_probe).unwrap();
        assert_eq!(backend, SecretsBackend::File);
        assert!(!secrets.encrypted());
        assert!(warning
            .unwrap()
            .contains(&app.secrets_dir().display().to_string()));

        // Same through open_store, with the choice already recorded on disk.
        let bootstrap = store_at(&app.data_dir);
        let file = AccountsFile {
            secrets_backend: Some(SecretsBackend::File),
            ..AccountsFile::default()
        };
        bootstrap.save(&file).unwrap();
        let no_probe = || -> Result<(), String> {
            panic!("the probe must not run when the file backend is recorded")
        };
        let (store, warning) = open_store_with(app.clone(), no_probe).unwrap();
        assert!(!store.secrets().encrypted());
        assert!(warning.is_some());
        assert_eq!(
            store.load().unwrap().secrets_backend,
            Some(SecretsBackend::File)
        );
    }

    #[test]
    fn an_old_accounts_file_without_the_backend_loads_and_is_recorded_on_first_use() {
        let tmp = tempfile::tempdir().unwrap();
        let app = AppPaths::at(&tmp.path().join("app"));
        fs::create_dir_all(&app.data_dir).unwrap();
        fs::write(
            app.accounts_file(),
            br#"{"version":1,"accounts":[],"settings":{}}"#,
        )
        .unwrap();

        let bootstrap = store_at(&app.data_dir);
        assert_eq!(bootstrap.load().unwrap().secrets_backend, None);

        let (store, warning) = open_store_with(app.clone(), || Ok(())).unwrap();
        assert_eq!(warning, None);
        assert_eq!(
            store.load().unwrap().secrets_backend,
            Some(SecretsBackend::Keyring)
        );
    }

    #[test]
    fn the_recorded_backend_survives_a_save_and_load() {
        let tmp = tempfile::tempdir().unwrap();
        let app = AppPaths::at(&tmp.path().join("app"));
        let store = store_at(&app.data_dir);

        for (backend, text) in [
            (SecretsBackend::Keyring, "\"keyring\""),
            (SecretsBackend::File, "\"file\""),
        ] {
            let file = AccountsFile {
                secrets_backend: Some(backend),
                ..AccountsFile::default()
            };
            store.save(&file).unwrap();
            let raw = fs::read_to_string(app.accounts_file()).unwrap();
            assert!(
                raw.contains(&format!("\"secretsBackend\": {text}")),
                "unexpected accounts.json: {raw}"
            );
            assert_eq!(store.load().unwrap().secrets_backend, Some(backend));
        }

        let mut file = store.load().unwrap();
        file.secrets_backend = None;
        store.save(&file).unwrap();
        assert_eq!(store.load().unwrap().secrets_backend, None);
    }

    #[test]
    fn a_provider_subscription_is_created_once_with_its_look() {
        let mut file = AccountsFile::default();
        let codex = AccountRef {
            acc: "0f3a6c2e-7b1d-4e9a-8c5f-2d4b6a8e0c13".to_string(),
            org: crate::providers::CODEX_ORG.to_string(),
        };
        assert!(file
            .ensure_provider_subscription(&codex, Some("Plus"))
            .unwrap());
        let subscription = file.provider_subscription(&codex).expect("OA");
        assert_eq!(
            (
                subscription.label.as_str(),
                subscription.abbr.as_str(),
                subscription.color.as_str(),
                subscription.plan.as_deref(),
            ),
            ("OpenAI", "OA", "#db2777", Some("Plus"))
        );
        assert_eq!(subscription.price, None);
        assert_eq!(subscription.price_currency, None);

        let ollama = AccountRef {
            acc: crate::providers::OLLAMA_ORG.to_string(),
            org: crate::providers::OLLAMA_ORG.to_string(),
        };
        assert!(file.ensure_provider_subscription(&ollama, None).unwrap());
        let subscription = file.provider_subscription(&ollama).expect("OL");
        assert_eq!(
            (
                subscription.label.as_str(),
                subscription.abbr.as_str(),
                subscription.color.as_str(),
                subscription.plan.as_deref(),
            ),
            ("Ollama", "OL", "#ea580c", None)
        );

        assert!(!file
            .ensure_provider_subscription(&codex, Some("Plus"))
            .unwrap());
        assert_eq!(
            file.provider_subscription(&codex)
                .and_then(|subscription| subscription.plan.as_deref()),
            Some("Plus"),
            "a second call kept the plan"
        );
        assert!(!file.ensure_provider_subscription(&ollama, None).unwrap());

        let google = |email: &str| AccountRef {
            acc: email.to_string(),
            org: crate::providers::GOOGLE_ORG.to_string(),
        };
        for (email, abbr) in [("sam@mail.example", "GO"), ("kim@mail.example", "G2")] {
            assert!(file
                .ensure_provider_subscription(&google(email), Some("Google AI Pro"))
                .unwrap());
            let subscription = file.provider_subscription(&google(email)).expect("GO");
            assert_eq!(
                (
                    subscription.label.as_str(),
                    subscription.abbr.as_str(),
                    subscription.color.as_str(),
                    subscription.plan.as_deref(),
                ),
                ("Google", abbr, "#0d9488", Some("Google AI Pro"))
            );
        }
        assert_eq!(file.provider_subscriptions.len(), 4);

        let json = serde_json::to_value(&file).unwrap();
        assert!(json.get("providerSubscriptions").is_some());
        let older: AccountsFile =
            serde_json::from_value(json!({"version": 1, "accounts": []})).unwrap();
        assert!(
            older.provider_subscriptions.is_empty(),
            "an older file without the field loads with none"
        );
    }

    #[test]
    fn a_subscription_takes_the_next_free_abbreviation() {
        let now = now();
        let mut file = AccountsFile::default();
        let oak = identity(ONE_ACCOUNT_UUID, ONE_ORG_UUID, "Oak", "one@example.com");
        let mut oak = new_account(&sample_login(), &oak, None, &file, now).unwrap();
        assert_eq!(oak.abbr, "OA");
        oak.abbr = "OA".to_string();
        file.accounts.push(oak);
        let o2 = identity(TWO_ACCOUNT_UUID, TWO_ORG_UUID, "O2", "two@example.com");
        let mut o2 = new_account(&sample_login(), &o2, None, &file, now).unwrap();
        o2.abbr = "O2".to_string();
        file.accounts.push(o2);

        let codex = AccountRef {
            acc: "0f3a6c2e-7b1d-4e9a-8c5f-2d4b6a8e0c13".to_string(),
            org: crate::providers::CODEX_ORG.to_string(),
        };
        assert!(file
            .ensure_provider_subscription(&codex, Some("Plus"))
            .unwrap());

        assert_eq!(
            file.provider_subscription(&codex)
                .expect("the OpenAI subscription")
                .abbr,
            "O3",
            "the base and O2 are taken, so the first free digit is 3"
        );
    }

    #[test]
    fn a_new_account_does_not_take_a_subscriptions_abbreviation() {
        let now = now();
        let mut file = AccountsFile::default();
        let codex = AccountRef {
            acc: "0f3a6c2e-7b1d-4e9a-8c5f-2d4b6a8e0c13".to_string(),
            org: crate::providers::CODEX_ORG.to_string(),
        };
        assert!(file
            .ensure_provider_subscription(&codex, Some("Plus"))
            .unwrap());
        assert_eq!(
            file.provider_subscription(&codex)
                .expect("the OpenAI subscription")
                .abbr,
            "OA"
        );

        let oak_area = identity(
            TWO_ACCOUNT_UUID,
            TWO_ORG_UUID,
            "Oak Area",
            "two@example.com",
        );
        let account = new_account(&sample_login(), &oak_area, None, &file, now).unwrap();

        assert_ne!(
            account.abbr, "OA",
            "the new Claude account took the subscription's abbreviation"
        );
        assert_eq!(account.abbr, "OK");
    }
}
