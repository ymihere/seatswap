//! The two HTTP endpoints seatswap uses: an account's usage and the OAuth
//! token refresh (spec §3, §6.3, §7.2, §7.3). Both endpoints are undocumented,
//! so their URLs, headers and reply shapes live in this file and nowhere else.

use std::time::Duration;

use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};
use serde_json::{Map, Value};

use crate::types::Login;

pub const USAGE_URL: &str = "https://api.anthropic.com/api/oauth/usage";
pub const TOKEN_URL: &str = "https://platform.claude.com/v1/oauth/token";
pub const CLIENT_ID: &str = "9d1c250a-e61b-44d9-88ed-5944d1962f5e";
pub const OAUTH_BETA: &str = "oauth-2025-04-20";

/// The server rejects a library default User-Agent with 403 "error code: 1010".
const USER_AGENT: &str = concat!("seatswap/", env!("CARGO_PKG_VERSION"));
const TIMEOUT: Duration = Duration::from_secs(20);
const MAX_ERROR_BODY_CHARS: usize = 300;

#[derive(Debug, thiserror::Error)]
pub enum ApiError {
    #[error("the access token is not authorized")]
    Unauthorized,
    #[error("the login cannot be refreshed: {0}")]
    InvalidGrant(String),
    #[error("the refresh reply belongs to another account: expected {expected:?}, got {got:?}")]
    OwnerMismatch {
        expected: (String, String),
        got: (String, String),
    },
    #[error("rate limited; retry after {retry_after_secs:?} seconds")]
    RateLimited { retry_after_secs: Option<u64> },
    #[error("server error {0}")]
    Server(u16),
    #[error("HTTP {status}: {body}")]
    Http { status: u16, body: String },
    #[error("network error: {0}")]
    Network(String),
    #[error("cannot decode the reply: {0}")]
    Decode(String),
}

impl ApiError {
    /// Errors worth retrying with the back-off from §7.1. A dead access token or
    /// an invalid refresh token stays broken until someone logs in again.
    pub fn is_transient(&self) -> bool {
        matches!(
            self,
            ApiError::RateLimited { .. } | ApiError::Server(_) | ApiError::Network(_)
        )
    }
}

pub struct Api {
    agent: ureq::Agent,
    usage_url: String,
    token_url: String,
}

impl Api {
    pub fn new() -> Api {
        Api {
            agent: build_agent(),
            usage_url: USAGE_URL.to_string(),
            token_url: TOKEN_URL.to_string(),
        }
    }

    /// Same agent, but pointed at a local fake server (tests).
    pub fn with_urls(usage_url: &str, token_url: &str) -> Api {
        Api {
            agent: build_agent(),
            usage_url: usage_url.to_string(),
            token_url: token_url.to_string(),
        }
    }

    /// GET the account's usage with its access token.
    pub fn usage(&self, access_token: &str) -> Result<Usage, ApiError> {
        let response = self
            .agent
            .get(&self.usage_url)
            .set("User-Agent", USER_AGENT)
            .set("Authorization", &format!("Bearer {access_token}"))
            .set("anthropic-beta", OAUTH_BETA)
            .call();
        let response = match response {
            Ok(response) => response,
            Err(ureq::Error::Status(status, response)) => {
                return Err(status_error(status, response))
            }
            Err(ureq::Error::Transport(error)) => return Err(ApiError::Network(error.to_string())),
        };
        let body = read_body(response)?;
        let value: Value =
            serde_json::from_str(&body).map_err(|error| ApiError::Decode(error.to_string()))?;
        Usage::from_json(&value)
    }

    /// Exchange a login's refresh token for a new access token. `expected` is the
    /// account identity (accountUuid, organizationUuid) the login belongs to.
    pub fn refresh(
        &self,
        login: &Login,
        expected: (&str, &str),
        now_ms: i64,
    ) -> Result<Login, ApiError> {
        let refresh_token = login
            .refresh_token()
            .ok_or_else(|| ApiError::Decode("login has no refresh token".to_string()))?
            .to_string();
        let request = serde_json::json!({
            "grant_type": "refresh_token",
            "refresh_token": refresh_token,
            "client_id": CLIENT_ID,
            "scope": login.scopes().join(" "),
        });
        let response = self
            .agent
            .post(&self.token_url)
            .set("User-Agent", USER_AGENT)
            .set("Content-Type", "application/json")
            .send_json(request);
        let response = match response {
            Ok(response) => response,
            Err(ureq::Error::Status(status, response)) => {
                return Err(status_error(status, response))
            }
            Err(ureq::Error::Transport(error)) => return Err(ApiError::Network(error.to_string())),
        };
        let body = read_body(response)?;
        let reply: Value =
            serde_json::from_str(&body).map_err(|error| ApiError::Decode(error.to_string()))?;
        refreshed_login(login, &reply, expected, now_ms)
    }
}

impl Default for Api {
    fn default() -> Self {
        Api::new()
    }
}

fn build_agent() -> ureq::Agent {
    ureq::AgentBuilder::new().timeout(TIMEOUT).build()
}

fn read_body(response: ureq::Response) -> Result<String, ApiError> {
    response
        .into_string()
        .map_err(|error| ApiError::Network(error.to_string()))
}

/// Map a 4xx/5xx reply (ureq's `Error::Status`) to the contract's variants.
fn status_error(status: u16, response: ureq::Response) -> ApiError {
    match status {
        401 => ApiError::Unauthorized,
        429 => ApiError::RateLimited {
            retry_after_secs: retry_after_secs(&response),
        },
        500..=599 => ApiError::Server(status),
        400 => {
            let body = read_body(response).unwrap_or_default();
            match invalid_grant_description(&body) {
                Some(description) => ApiError::InvalidGrant(description),
                None => ApiError::Http {
                    status,
                    body: cut_body(&body),
                },
            }
        }
        _ => {
            let body = read_body(response).unwrap_or_default();
            ApiError::Http {
                status,
                body: cut_body(&body),
            }
        }
    }
}

fn retry_after_secs(response: &ureq::Response) -> Option<u64> {
    response
        .header("Retry-After")
        .and_then(|value| value.trim().parse().ok())
}

/// `400 {"error":"invalid_grant","error_description":"…"}`.
fn invalid_grant_description(body: &str) -> Option<String> {
    let value: Value = serde_json::from_str(body).ok()?;
    if value.get("error").and_then(Value::as_str) != Some("invalid_grant") {
        return None;
    }
    Some(
        value
            .get("error_description")
            .and_then(Value::as_str)
            .unwrap_or("invalid_grant")
            .to_string(),
    )
}

fn cut_body(body: &str) -> String {
    body.chars().take(MAX_ERROR_BODY_CHARS).collect()
}

/// Update the login from a 200 refresh reply. The old login is only cloned, so
/// every field and key the reply does not mention stays exactly as it was.
fn refreshed_login(
    login: &Login,
    reply: &Value,
    expected: (&str, &str),
    now_ms: i64,
) -> Result<Login, ApiError> {
    let access_token = reply
        .get("access_token")
        .and_then(Value::as_str)
        .ok_or_else(|| ApiError::Decode("refresh reply has no access_token".to_string()))?;
    let expires_in = reply
        .get("expires_in")
        .and_then(Value::as_i64)
        .ok_or_else(|| ApiError::Decode("refresh reply has no expires_in".to_string()))?;
    let expires_at = add_seconds(now_ms, expires_in).ok_or_else(|| {
        ApiError::Decode("refresh reply's expires_in is out of range".to_string())
    })?;

    let new_refresh_token = reply
        .get("refresh_token")
        .and_then(Value::as_str)
        .map(str::to_string);
    let new_scopes = match reply.get("scope") {
        None | Some(Value::Null) => None,
        Some(Value::String(scope)) => Some(
            scope
                .split_whitespace()
                .map(str::to_string)
                .collect::<Vec<_>>(),
        ),
        Some(_) => {
            return Err(ApiError::Decode(
                "refresh reply's scope is not a string".to_string(),
            ))
        }
    };
    let new_end = match reply.get("refresh_token_expires_in") {
        None | Some(Value::Null) => None,
        Some(_) => {
            let seconds = reply
                .get("refresh_token_expires_in")
                .and_then(Value::as_i64)
                .ok_or_else(|| {
                    ApiError::Decode(
                        "refresh reply's refresh_token_expires_in is not a number".to_string(),
                    )
                })?;
            Some(add_seconds(now_ms, seconds).ok_or_else(|| {
                ApiError::Decode(
                    "refresh reply's refresh_token_expires_in is out of range".to_string(),
                )
            })?)
        }
    };

    let got = (
        owner_uuid(reply, "account")?,
        owner_uuid(reply, "organization")?,
    );
    let expected_owner = (expected.0.to_string(), expected.1.to_string());
    if got != expected_owner {
        return Err(ApiError::OwnerMismatch {
            expected: expected_owner,
            got,
        });
    }

    let mut updated = login.clone();
    let oauth = updated
        .0
        .get_mut("claudeAiOauth")
        .and_then(Value::as_object_mut)
        .ok_or_else(|| ApiError::Decode("login has no claudeAiOauth object".to_string()))?;
    oauth.insert(
        "accessToken".to_string(),
        Value::String(access_token.to_string()),
    );
    oauth.insert("expiresAt".to_string(), Value::from(expires_at));
    if let Some(refresh_token) = new_refresh_token {
        oauth.insert("refreshToken".to_string(), Value::String(refresh_token));
    }
    if let Some(new_end) = new_end {
        // A login ends on a fixed date; a refresh must never move it later.
        let end = match login.refresh_token_expires_at_ms() {
            Some(old_end) => old_end.min(new_end),
            None => new_end,
        };
        oauth.insert("refreshTokenExpiresAt".to_string(), Value::from(end));
    }
    if let Some(scopes) = new_scopes {
        oauth.insert(
            "scopes".to_string(),
            Value::Array(scopes.into_iter().map(Value::String).collect()),
        );
    }
    Ok(updated)
}

fn add_seconds(now_ms: i64, seconds: i64) -> Option<i64> {
    seconds
        .checked_mul(1000)
        .and_then(|ms| now_ms.checked_add(ms))
}

fn owner_uuid(reply: &Value, object: &str) -> Result<String, ApiError> {
    reply
        .get(object)
        .and_then(Value::as_object)
        .and_then(|value| value.get("uuid"))
        .and_then(Value::as_str)
        .map(str::to_string)
        .ok_or_else(|| ApiError::Decode(format!("refresh reply has no {object}.uuid")))
}

/// One usage limit from the `limits[]` array (or from the `five_hour` /
/// `seven_day` fallback objects).
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct Limit {
    pub kind: LimitKind,
    pub percent: f64,
    pub severity: Severity,
    pub resets_at: Option<DateTime<Utc>>,
    pub model: Option<String>,
    pub is_active: bool,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum LimitKind {
    Session,
    WeeklyAll,
    WeeklyScoped,
    Other(String),
}

impl LimitKind {
    fn from_api(text: &str) -> LimitKind {
        match text {
            "session" => LimitKind::Session,
            "weekly_all" => LimitKind::WeeklyAll,
            "weekly_scoped" => LimitKind::WeeklyScoped,
            other => LimitKind::Other(other.to_string()),
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Severity {
    Normal,
    Warning,
    Critical,
    Other(String),
}

impl Severity {
    fn from_api(text: &str) -> Severity {
        match text {
            "normal" => Severity::Normal,
            "warning" => Severity::Warning,
            "critical" => Severity::Critical,
            other => Severity::Other(other.to_string()),
        }
    }
}

/// §7.3: < 70 Fine, 70–89 Close, ≥ 90 or Critical severity → Critical,
/// ≥ 100 → Locked.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum LoadLevel {
    Fine,
    Close,
    Critical,
    Locked,
}

impl LoadLevel {
    pub fn color(&self) -> &'static str {
        match self {
            LoadLevel::Fine => "#22c55e",
            LoadLevel::Close => "#f59e0b",
            LoadLevel::Critical | LoadLevel::Locked => "#ef4444",
        }
    }
}

impl Limit {
    pub fn level(&self) -> LoadLevel {
        if self.percent >= 100.0 {
            LoadLevel::Locked
        } else if self.percent >= 90.0 || self.severity == Severity::Critical {
            LoadLevel::Critical
        } else if self.percent >= 70.0 {
            LoadLevel::Close
        } else {
            LoadLevel::Fine
        }
    }
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct Usage {
    pub limits: Vec<Limit>,
    pub extra_usage_enabled: bool,
}

impl Usage {
    /// Read a usage reply. `limits[]` wins when it is an array (even empty);
    /// only a missing or null `limits` falls back to `five_hour` / `seven_day`.
    pub fn from_json(value: &Value) -> Result<Usage, ApiError> {
        let object = value
            .as_object()
            .ok_or_else(|| ApiError::Decode("usage reply is not a JSON object".to_string()))?;
        let extra_usage_enabled = object
            .get("extra_usage")
            .and_then(Value::as_object)
            .and_then(|extra| extra.get("is_enabled"))
            .and_then(Value::as_bool)
            .unwrap_or(false);
        let limits = match object.get("limits") {
            Some(Value::Array(items)) => items
                .iter()
                .map(limit_from_json)
                .collect::<Result<Vec<_>, ApiError>>()?,
            None | Some(Value::Null) => fallback_limits(object)?,
            Some(_) => {
                return Err(ApiError::Decode(
                    "usage reply's limits is not an array".to_string(),
                ))
            }
        };
        Ok(Usage {
            limits,
            extra_usage_enabled,
        })
    }

    pub fn session(&self) -> Option<&Limit> {
        self.limits
            .iter()
            .find(|limit| limit.kind == LimitKind::Session)
    }

    pub fn weekly_all(&self) -> Option<&Limit> {
        self.limits
            .iter()
            .find(|limit| limit.kind == LimitKind::WeeklyAll)
    }

    pub fn weekly_scoped(&self) -> impl Iterator<Item = &Limit> {
        self.limits
            .iter()
            .filter(|limit| limit.kind == LimitKind::WeeklyScoped)
    }

    /// The active limit, else the one with the highest percentage, else none.
    pub fn binding(&self) -> Option<&Limit> {
        self.limits
            .iter()
            .find(|limit| limit.is_active)
            .or_else(|| {
                self.limits
                    .iter()
                    .max_by(|left, right| left.percent.total_cmp(&right.percent))
            })
    }
}

fn limit_from_json(value: &Value) -> Result<Limit, ApiError> {
    let object = value
        .as_object()
        .ok_or_else(|| ApiError::Decode("usage limit is not a JSON object".to_string()))?;
    let kind = object
        .get("kind")
        .and_then(Value::as_str)
        .ok_or_else(|| ApiError::Decode("usage limit has no kind".to_string()))?;
    let percent = object
        .get("percent")
        .and_then(Value::as_f64)
        .ok_or_else(|| ApiError::Decode("usage limit has no numeric percent".to_string()))?;
    let severity = object
        .get("severity")
        .and_then(Value::as_str)
        .map(Severity::from_api)
        .unwrap_or(Severity::Normal);
    Ok(Limit {
        kind: LimitKind::from_api(kind),
        percent,
        severity,
        resets_at: resets_at(object.get("resets_at")),
        model: model_name(object.get("scope")),
        is_active: object
            .get("is_active")
            .and_then(Value::as_bool)
            .unwrap_or(false),
    })
}

fn fallback_limits(object: &Map<String, Value>) -> Result<Vec<Limit>, ApiError> {
    let mut limits = Vec::new();
    if let Some(window) = window_object(object, "five_hour")? {
        limits.push(fallback_limit(LimitKind::Session, window)?);
    }
    if let Some(window) = window_object(object, "seven_day")? {
        limits.push(fallback_limit(LimitKind::WeeklyAll, window)?);
    }
    Ok(limits)
}

fn window_object<'a>(
    object: &'a Map<String, Value>,
    name: &str,
) -> Result<Option<&'a Map<String, Value>>, ApiError> {
    match object.get(name) {
        None | Some(Value::Null) => Ok(None),
        Some(Value::Object(window)) => Ok(Some(window)),
        Some(_) => Err(ApiError::Decode(format!(
            "usage reply's {name} is not an object"
        ))),
    }
}

fn fallback_limit(kind: LimitKind, window: &Map<String, Value>) -> Result<Limit, ApiError> {
    let percent = window
        .get("utilization")
        .and_then(Value::as_f64)
        .ok_or_else(|| ApiError::Decode("usage window has no numeric utilization".to_string()))?;
    Ok(Limit {
        kind,
        percent,
        severity: Severity::Normal,
        resets_at: resets_at(window.get("resets_at")),
        model: None,
        is_active: false,
    })
}

fn resets_at(value: Option<&Value>) -> Option<DateTime<Utc>> {
    value
        .and_then(Value::as_str)
        .and_then(|text| DateTime::parse_from_rfc3339(text).ok())
        .map(|time| time.with_timezone(&Utc))
}

fn model_name(scope: Option<&Value>) -> Option<String> {
    scope
        .and_then(Value::as_object)
        .and_then(|scope| scope.get("model"))
        .and_then(Value::as_object)
        .and_then(|model| model.get("display_name"))
        .and_then(Value::as_str)
        .map(str::to_string)
}

#[cfg(test)]
mod tests {
    use super::*;

    use std::net::TcpListener;
    use std::sync::mpsc::{self, Receiver};
    use std::thread::{self, JoinHandle};

    use tiny_http::{Header, Response, Server, StatusCode};

    const ACCOUNT_UUID: &str = "4b7e2f91-3c5a-4d8e-9f1b-6a2c8e4d7b30";
    const OTHER_ACCOUNT_UUID: &str = "0f5f8b7e-6f0a-4d6e-9b0a-8c1d2e3f4a5b";
    const ORG_UUID: &str = "e2a6c9d1-7f4b-4a3e-8c5d-1b9f0a7e3c62";
    const SCOPES: [&str; 6] = [
        "user:inference",
        "user:profile",
        "user:file_upload",
        "user:mcp_servers",
        "user:plugins",
        "user:sessions:claude_code",
    ];

    const USAGE_SAMPLE: &str = r#"{
      "five_hour":  { "utilization": 97.0, "resets_at": "2026-11-10T13:40:00.102645+00:00" },
      "seven_day":  { "utilization": 48.0, "resets_at": "2026-11-16T01:00:00+00:00" },
      "limits": [
        { "kind": "session", "group": "session", "percent": 97, "severity": "critical",
          "resets_at": "2026-11-10T13:40:00+00:00", "scope": null, "is_active": true },
        { "kind": "weekly_all", "group": "weekly", "percent": 48, "severity": "normal",
          "resets_at": "2026-11-16T01:00:00+00:00", "scope": null, "is_active": false },
        { "kind": "weekly_scoped", "group": "weekly", "percent": 43, "severity": "normal",
          "resets_at": "2026-11-16T01:00:00+00:00",
          "scope": { "model": { "display_name": "Fable" } }, "is_active": false }
      ],
      "extra_usage": { "is_enabled": false },
      "some_code_named_field": null
    }"#;

    fn shaped_token(prefix: &str, fill: char) -> String {
        format!("{prefix}{}", fill.to_string().repeat(95))
    }

    fn usage_token() -> String {
        shaped_token("sk-ant-oat01-", 'a')
    }

    fn new_access_token() -> String {
        shaped_token("sk-ant-oat01-", 'c')
    }

    fn old_refresh_token() -> String {
        shaped_token("sk-ant-ort01-", 'b')
    }

    fn new_refresh_token() -> String {
        shaped_token("sk-ant-ort01-", 'd')
    }

    fn login(access: &str, refresh: &str, end_ms: i64) -> Login {
        let mut oauth = Map::new();
        oauth.insert("accessToken".into(), Value::String(access.into()));
        oauth.insert("refreshToken".into(), Value::String(refresh.into()));
        oauth.insert("expiresAt".into(), Value::from(1_794_356_040_000_i64));
        oauth.insert("refreshTokenExpiresAt".into(), Value::from(end_ms));
        oauth.insert(
            "scopes".into(),
            Value::Array(SCOPES.iter().map(|s| Value::String((*s).into())).collect()),
        );
        oauth.insert("subscriptionType".into(), Value::String("team".into()));
        oauth.insert(
            "subscriptionTypeSource".into(),
            Value::String("profile".into()),
        );
        oauth.insert(
            "rateLimitTier".into(),
            Value::String("default_claude_max_5x".into()),
        );
        let mut top = Map::new();
        top.insert("claudeAiOauth".into(), Value::Object(oauth));
        top.insert("organizationUuid".into(), Value::String(ORG_UUID.into()));
        Login(top)
    }

    fn refresh_reply(
        access: &str,
        refresh: Option<&str>,
        refresh_expires_in: Option<i64>,
        scope: Option<&str>,
    ) -> String {
        let mut reply = serde_json::json!({
            "token_type": "Bearer",
            "access_token": access,
            "expires_in": 28_800,
            "token_uuid": "7d5b8d5e-4a4a-4c4a-9f0e-2b2f8c3a1d6e",
            "account": {
                "uuid": ACCOUNT_UUID,
                "email_address": "someone@example.com"
            },
            "organization": { "uuid": ORG_UUID }
        });
        if let Some(refresh) = refresh {
            reply["refresh_token"] = Value::String(refresh.into());
        }
        if let Some(seconds) = refresh_expires_in {
            reply["refresh_token_expires_in"] = Value::from(seconds);
        }
        if let Some(scope) = scope {
            reply["scope"] = Value::String(scope.into());
        }
        reply.to_string()
    }

    struct Reply {
        status: u16,
        headers: Vec<(String, String)>,
        body: String,
    }

    impl Reply {
        fn ok(body: impl Into<String>) -> Reply {
            Reply {
                status: 200,
                headers: Vec::new(),
                body: body.into(),
            }
        }

        fn status(status: u16, body: impl Into<String>) -> Reply {
            Reply {
                status,
                headers: Vec::new(),
                body: body.into(),
            }
        }

        fn header(mut self, name: &str, value: &str) -> Reply {
            self.headers.push((name.to_string(), value.to_string()));
            self
        }
    }

    #[derive(Debug)]
    struct Captured {
        method: String,
        path: String,
        headers: Vec<(String, String)>,
        body: String,
    }

    impl Captured {
        fn header(&self, name: &str) -> Option<&str> {
            self.headers
                .iter()
                .find(|(field, _)| field.eq_ignore_ascii_case(name))
                .map(|(_, value)| value.as_str())
        }
    }

    fn start_server(reply: Reply) -> (String, Receiver<Captured>, JoinHandle<()>) {
        let server = Server::http("127.0.0.1:0").unwrap();
        let address = server.server_addr().to_ip().unwrap();
        let base = format!("http://{address}");
        let (sender, receiver) = mpsc::channel();
        let handle = thread::spawn(move || {
            let mut request = server.recv().unwrap();
            let mut body = String::new();
            request.as_reader().read_to_string(&mut body).unwrap();
            let captured = Captured {
                method: request.method().as_str().to_string(),
                path: request.url().to_string(),
                headers: request
                    .headers()
                    .iter()
                    .map(|header| {
                        (
                            header.field.as_str().as_str().to_string(),
                            header.value.as_str().to_string(),
                        )
                    })
                    .collect(),
                body,
            };
            let mut response =
                Response::from_string(reply.body).with_status_code(StatusCode(reply.status));
            for (name, value) in reply.headers {
                response = response
                    .with_header(Header::from_bytes(name.as_bytes(), value.as_bytes()).unwrap());
            }
            request.respond(response).unwrap();
            sender.send(captured).unwrap();
        });
        (base, receiver, handle)
    }

    fn api_for(base: &str) -> Api {
        Api::with_urls(
            &format!("{base}/api/oauth/usage"),
            &format!("{base}/v1/oauth/token"),
        )
    }

    fn captured(receiver: &Receiver<Captured>, handle: JoinHandle<()>) -> Captured {
        let request = receiver
            .recv_timeout(Duration::from_secs(10))
            .expect("the fake server received no request");
        handle.join().unwrap();
        request
    }

    fn closed_port_api() -> Api {
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let address = listener.local_addr().unwrap();
        drop(listener);
        Api::with_urls(
            &format!("http://{address}/api/oauth/usage"),
            &format!("http://{address}/v1/oauth/token"),
        )
    }

    #[test]
    fn usage_sends_the_contract_headers_and_reads_limits() {
        let token = usage_token();
        let (base, receiver, handle) = start_server(Reply::ok(USAGE_SAMPLE));
        let usage = api_for(&base).usage(&token).unwrap();
        let request = captured(&receiver, handle);

        assert_eq!(request.method, "GET");
        assert_eq!(request.path, "/api/oauth/usage");
        assert_eq!(request.header("User-Agent"), Some(USER_AGENT));
        let authorization = format!("Bearer {token}");
        assert_eq!(
            request.header("Authorization"),
            Some(authorization.as_str())
        );
        assert_eq!(request.header("anthropic-beta"), Some(OAUTH_BETA));

        assert_eq!(usage.limits.len(), 3);
        let session = usage.session().unwrap();
        assert_eq!(session.kind, LimitKind::Session);
        assert_eq!(session.percent, 97.0);
        assert_eq!(session.severity, Severity::Critical);
        assert!(session.is_active);
        assert_eq!(
            session.resets_at,
            Some("2026-11-10T13:40:00Z".parse::<DateTime<Utc>>().unwrap())
        );
        let weekly = usage.weekly_all().unwrap();
        assert_eq!(weekly.percent, 48.0);
        assert_eq!(weekly.severity, Severity::Normal);
        assert!(!weekly.is_active);
        let scoped: Vec<_> = usage.weekly_scoped().collect();
        assert_eq!(scoped.len(), 1);
        assert_eq!(scoped[0].percent, 43.0);
        assert_eq!(scoped[0].model.as_deref(), Some("Fable"));
        assert_eq!(usage.binding().unwrap().kind, LimitKind::Session);
        assert!(!usage.extra_usage_enabled);
    }

    #[test]
    fn personal_reply_has_no_session_and_binds_the_active_weekly_limit() {
        let body = r#"{
          "five_hour": null,
          "seven_day": { "utilization": 48.0, "resets_at": "2026-11-16T01:00:00+00:00" },
          "limits": [
            { "kind": "weekly_all", "group": "weekly", "percent": 48, "severity": "normal",
              "resets_at": "2026-11-16T01:00:00+00:00", "scope": null, "is_active": true },
            { "kind": "weekly_scoped", "group": "weekly", "percent": 43, "severity": "normal",
              "resets_at": "2026-11-16T01:00:00+00:00",
              "scope": { "model": { "display_name": "Fable" } }, "is_active": false }
          ],
          "extra_usage": { "is_enabled": false }
        }"#;
        let (base, receiver, handle) = start_server(Reply::ok(body));
        let usage = api_for(&base).usage(&usage_token()).unwrap();
        let _ = captured(&receiver, handle);

        assert!(usage.session().is_none());
        let binding = usage.binding().unwrap();
        assert_eq!(binding.kind, LimitKind::WeeklyAll);
        assert!(binding.is_active);
    }

    #[test]
    fn missing_limits_falls_back_to_five_hour_and_seven_day() {
        let body = r#"{
          "five_hour": { "utilization": 97.0, "resets_at": "2026-11-10T13:40:00.102645+00:00" },
          "seven_day": { "utilization": 48.0, "resets_at": "2026-11-16T01:00:00+00:00" }
        }"#;
        let (base, receiver, handle) = start_server(Reply::ok(body));
        let usage = api_for(&base).usage(&usage_token()).unwrap();
        let _ = captured(&receiver, handle);

        assert_eq!(usage.limits.len(), 2);
        let session = usage.session().unwrap();
        assert_eq!(session.percent, 97.0);
        assert_eq!(session.severity, Severity::Normal);
        assert_eq!(
            session.resets_at,
            Some(
                "2026-11-10T13:40:00.102645Z"
                    .parse::<DateTime<Utc>>()
                    .unwrap()
            )
        );
        assert_eq!(usage.weekly_all().unwrap().percent, 48.0);
        assert!(!usage.extra_usage_enabled);
        assert_eq!(usage.binding().unwrap().kind, LimitKind::Session);
    }

    #[test]
    fn binding_without_an_active_limit_is_the_highest_percent() {
        let body = r#"{
          "limits": [
            { "kind": "weekly_scoped", "percent": 43, "severity": "normal",
              "scope": { "model": { "display_name": "Fable" } }, "is_active": false },
            { "kind": "weekly_all", "percent": 48, "severity": "normal", "is_active": false }
          ]
        }"#;
        let (base, receiver, handle) = start_server(Reply::ok(body));
        let usage = api_for(&base).usage(&usage_token()).unwrap();
        let _ = captured(&receiver, handle);

        let binding = usage.binding().unwrap();
        assert_eq!(binding.kind, LimitKind::WeeklyAll);
        assert_eq!(binding.percent, 48.0);
    }

    #[test]
    fn http_statuses_map_to_the_contract_variants() {
        let token = usage_token();

        let (base, receiver, handle) = start_server(Reply::status(401, ""));
        let error = api_for(&base).usage(&token).unwrap_err();
        let _ = captured(&receiver, handle);
        assert!(matches!(error, ApiError::Unauthorized));

        let (base, receiver, handle) = start_server(Reply::status(403, "error code: 1010"));
        let error = api_for(&base).usage(&token).unwrap_err();
        let _ = captured(&receiver, handle);
        match error {
            ApiError::Http { status, body } => {
                assert_eq!(status, 403);
                assert_eq!(body, "error code: 1010");
            }
            other => panic!("expected Http, got {other:?}"),
        }

        let (base, receiver, handle) =
            start_server(Reply::status(429, "").header("Retry-After", "30"));
        let error = api_for(&base).usage(&token).unwrap_err();
        let _ = captured(&receiver, handle);
        match error {
            ApiError::RateLimited { retry_after_secs } => {
                assert_eq!(retry_after_secs, Some(30))
            }
            other => panic!("expected RateLimited, got {other:?}"),
        }

        let (base, receiver, handle) = start_server(Reply::status(503, "unavailable"));
        let error = api_for(&base).usage(&token).unwrap_err();
        let _ = captured(&receiver, handle);
        assert!(matches!(error, ApiError::Server(503)));
    }

    #[test]
    fn closed_port_is_a_network_error() {
        let error = closed_port_api().usage(&usage_token()).unwrap_err();
        assert!(matches!(error, ApiError::Network(_)), "got {error:?}");
    }

    #[test]
    fn refresh_rotates_tokens_but_never_moves_the_end_date_later() {
        let old_access = usage_token();
        let old_refresh = old_refresh_token();
        let stored_end = 1_796_700_000_000_i64;
        let now_ms = 1_794_356_040_000_i64;
        // now + refresh_token_expires_in is 1796730600000, later than stored_end.
        let reply = refresh_reply(
            &new_access_token(),
            Some(&new_refresh_token()),
            Some(2_374_560),
            Some(&SCOPES.join(" ")),
        );
        let (base, receiver, handle) = start_server(Reply::ok(reply));
        let login = login(&old_access, &old_refresh, stored_end);
        let refreshed = api_for(&base)
            .refresh(&login, (ACCOUNT_UUID, ORG_UUID), now_ms)
            .unwrap();
        let request = captured(&receiver, handle);

        assert_eq!(request.method, "POST");
        assert_eq!(request.path, "/v1/oauth/token");
        assert_eq!(request.header("User-Agent"), Some(USER_AGENT));
        assert_eq!(request.header("Content-Type"), Some("application/json"));
        let sent: Value = serde_json::from_str(&request.body).unwrap();
        let sent = sent.as_object().unwrap();
        assert_eq!(sent.len(), 4);
        assert_eq!(sent["grant_type"], "refresh_token");
        assert_eq!(sent["refresh_token"], old_refresh);
        assert_eq!(sent["client_id"], CLIENT_ID);
        assert_eq!(sent["scope"], SCOPES.join(" "));

        assert_eq!(refreshed.access_token(), Some(new_access_token().as_str()));
        assert_eq!(
            refreshed.refresh_token(),
            Some(new_refresh_token().as_str())
        );
        assert_eq!(refreshed.expires_at_ms(), Some(now_ms + 28_800 * 1000));
        assert_eq!(refreshed.refresh_token_expires_at_ms(), Some(stored_end));
        assert_eq!(refreshed.scopes(), SCOPES);
        assert_eq!(refreshed.subscription_type(), Some("team"));
        assert_eq!(refreshed.rate_limit_tier(), Some("default_claude_max_5x"));
        assert_eq!(
            refreshed.0.get("organizationUuid").and_then(Value::as_str),
            Some(ORG_UUID)
        );
    }

    #[test]
    fn refresh_without_a_new_refresh_token_keeps_the_old_one() {
        let old_refresh = old_refresh_token();
        let stored_end = 1_796_700_000_000_i64;
        let reply = refresh_reply(
            &new_access_token(),
            None,
            Some(2_374_560),
            Some(&SCOPES.join(" ")),
        );
        let (base, receiver, handle) = start_server(Reply::ok(reply));
        let login = login(&usage_token(), &old_refresh, stored_end);
        let refreshed = api_for(&base)
            .refresh(&login, (ACCOUNT_UUID, ORG_UUID), 1_794_356_040_000_i64)
            .unwrap();
        let _ = captured(&receiver, handle);

        assert_eq!(refreshed.refresh_token(), Some(old_refresh.as_str()));
        assert_eq!(refreshed.refresh_token_expires_at_ms(), Some(stored_end));
    }

    #[test]
    fn refresh_rejects_another_owner() {
        let reply = refresh_reply(
            &new_access_token(),
            Some(&new_refresh_token()),
            Some(2_374_560),
            None,
        )
        .replace(ACCOUNT_UUID, OTHER_ACCOUNT_UUID);
        let (base, receiver, handle) = start_server(Reply::ok(reply));
        let login = login(&usage_token(), &old_refresh_token(), 1_796_700_000_000);
        let error = api_for(&base)
            .refresh(&login, (ACCOUNT_UUID, ORG_UUID), 1_794_356_040_000)
            .unwrap_err();
        let _ = captured(&receiver, handle);

        match error {
            ApiError::OwnerMismatch { expected, got } => {
                assert_eq!(expected, (ACCOUNT_UUID.to_string(), ORG_UUID.to_string()));
                assert_eq!(got, (OTHER_ACCOUNT_UUID.to_string(), ORG_UUID.to_string()));
            }
            other => panic!("expected OwnerMismatch, got {other:?}"),
        }
    }

    #[test]
    fn invalid_grant_is_its_own_error() {
        let reply =
            r#"{"error":"invalid_grant","error_description":"Refresh token not found or invalid"}"#;
        let (base, receiver, handle) = start_server(Reply::status(400, reply));
        let login = login(&usage_token(), &old_refresh_token(), 1_796_700_000_000);
        let error = api_for(&base)
            .refresh(&login, (ACCOUNT_UUID, ORG_UUID), 1_794_356_040_000)
            .unwrap_err();
        let _ = captured(&receiver, handle);

        match error {
            ApiError::InvalidGrant(description) => {
                assert_eq!(description, "Refresh token not found or invalid")
            }
            other => panic!("expected InvalidGrant, got {other:?}"),
        }
    }

    #[test]
    fn a_200_reply_that_is_not_json_is_a_decode_error() {
        let (base, receiver, handle) = start_server(Reply::ok("this is not json"));
        let login = login(&usage_token(), &old_refresh_token(), 1_796_700_000_000);
        let error = api_for(&base)
            .refresh(&login, (ACCOUNT_UUID, ORG_UUID), 1_794_356_040_000)
            .unwrap_err();
        let _ = captured(&receiver, handle);

        assert!(matches!(error, ApiError::Decode(_)), "got {error:?}");
    }

    #[test]
    fn refresh_without_a_refresh_token_does_not_call_the_server() {
        let mut login = login(&usage_token(), &old_refresh_token(), 1_796_700_000_000);
        login
            .0
            .get_mut("claudeAiOauth")
            .and_then(Value::as_object_mut)
            .unwrap()
            .remove("refreshToken");
        let error = closed_port_api()
            .refresh(&login, (ACCOUNT_UUID, ORG_UUID), 1_794_356_040_000)
            .unwrap_err();

        match error {
            ApiError::Decode(message) => assert_eq!(message, "login has no refresh token"),
            other => panic!("expected Decode, got {other:?}"),
        }
    }

    #[test]
    fn a_200_refresh_reply_without_access_token_or_expires_in_is_a_decode_error() {
        let missing_access = serde_json::json!({
            "expires_in": 28_800,
            "account": { "uuid": ACCOUNT_UUID },
            "organization": { "uuid": ORG_UUID }
        })
        .to_string();
        let (base, receiver, handle) = start_server(Reply::ok(missing_access));
        let login = login(&usage_token(), &old_refresh_token(), 1_796_700_000_000);
        let error = api_for(&base)
            .refresh(&login, (ACCOUNT_UUID, ORG_UUID), 1_794_356_040_000)
            .unwrap_err();
        let _ = captured(&receiver, handle);
        assert!(matches!(error, ApiError::Decode(_)), "got {error:?}");

        let missing_expires = serde_json::json!({
            "access_token": new_access_token(),
            "account": { "uuid": ACCOUNT_UUID },
            "organization": { "uuid": ORG_UUID }
        })
        .to_string();
        let (base, receiver, handle) = start_server(Reply::ok(missing_expires));
        let error = api_for(&base)
            .refresh(&login, (ACCOUNT_UUID, ORG_UUID), 1_794_356_040_000)
            .unwrap_err();
        let _ = captured(&receiver, handle);
        assert!(matches!(error, ApiError::Decode(_)), "got {error:?}");
    }

    #[test]
    fn error_texts_never_contain_the_token() {
        let access = usage_token();
        let (base, receiver, handle) = start_server(Reply::status(403, "error code: 1010"));
        let error = api_for(&base).usage(&access).unwrap_err();
        let _ = captured(&receiver, handle);
        assert!(!error.to_string().contains(&access));
        assert!(!error.is_transient());

        let refresh = old_refresh_token();
        let (base, receiver, handle) = start_server(Reply::status(
            400,
            r#"{"error":"invalid_grant","error_description":"Refresh token not found or invalid"}"#,
        ));
        let login = login(&usage_token(), &refresh, 1_796_700_000_000);
        let error = api_for(&base)
            .refresh(&login, (ACCOUNT_UUID, ORG_UUID), 1_794_356_040_000)
            .unwrap_err();
        let _ = captured(&receiver, handle);
        assert!(!error.to_string().contains(&refresh));
        assert!(!error.to_string().contains(&usage_token()));

        let (base, receiver, handle) = start_server(Reply::status(503, "down"));
        let error = api_for(&base).usage(&access).unwrap_err();
        let _ = captured(&receiver, handle);
        assert!(!error.to_string().contains(&access));
        assert!(error.is_transient());
    }

    fn limit_at(percent: f64, severity: Severity) -> Limit {
        Limit {
            kind: LimitKind::Session,
            percent,
            severity,
            resets_at: None,
            model: None,
            is_active: false,
        }
    }

    #[test]
    fn load_level_boundaries() {
        assert_eq!(limit_at(69.9, Severity::Normal).level(), LoadLevel::Fine);
        assert_eq!(limit_at(70.0, Severity::Normal).level(), LoadLevel::Close);
        assert_eq!(limit_at(89.9, Severity::Normal).level(), LoadLevel::Close);
        assert_eq!(
            limit_at(90.0, Severity::Normal).level(),
            LoadLevel::Critical
        );
        assert_eq!(
            limit_at(99.9, Severity::Normal).level(),
            LoadLevel::Critical
        );
        assert_eq!(limit_at(100.0, Severity::Normal).level(), LoadLevel::Locked);
        assert_eq!(
            limit_at(50.0, Severity::Critical).level(),
            LoadLevel::Critical
        );
        assert_eq!(
            limit_at(100.0, Severity::Critical).level(),
            LoadLevel::Locked
        );
        assert_eq!(LoadLevel::Fine.color(), "#22c55e");
        assert_eq!(LoadLevel::Close.color(), "#f59e0b");
        assert_eq!(LoadLevel::Critical.color(), "#ef4444");
        assert_eq!(LoadLevel::Locked.color(), "#ef4444");
    }

    #[test]
    fn usage_serde_round_trip() {
        let usage = Usage {
            limits: vec![
                Limit {
                    kind: LimitKind::Session,
                    percent: 97.0,
                    severity: Severity::Critical,
                    resets_at: Some(
                        "2026-11-10T13:40:00.102645+00:00"
                            .parse::<DateTime<Utc>>()
                            .unwrap(),
                    ),
                    model: None,
                    is_active: true,
                },
                Limit {
                    kind: LimitKind::WeeklyAll,
                    percent: 48.0,
                    severity: Severity::Normal,
                    resets_at: None,
                    model: None,
                    is_active: false,
                },
                Limit {
                    kind: LimitKind::WeeklyScoped,
                    percent: 43.0,
                    severity: Severity::Warning,
                    resets_at: None,
                    model: Some("Fable".to_string()),
                    is_active: false,
                },
                Limit {
                    kind: LimitKind::Other("mystery_kind".to_string()),
                    percent: 1.0,
                    severity: Severity::Other("mystery_severity".to_string()),
                    resets_at: None,
                    model: None,
                    is_active: false,
                },
            ],
            extra_usage_enabled: true,
        };
        let encoded = serde_json::to_string(&usage).unwrap();
        assert!(encoded.contains("\"weekly_all\""));
        assert!(encoded.contains("\"weekly_scoped\""));
        assert!(encoded.contains(r#"{"other":"mystery_kind"}"#));
        assert!(encoded.contains(r#"{"other":"mystery_severity"}"#));
        let decoded: Usage = serde_json::from_str(&encoded).unwrap();
        assert_eq!(decoded, usage);
    }

    #[test]
    fn unknown_kind_and_severity_become_other() {
        let body = r#"{
          "limits": [
            { "kind": "monthly_extra", "percent": 12, "severity": "elevated" }
          ]
        }"#;
        let (base, receiver, handle) = start_server(Reply::ok(body));
        let usage = api_for(&base).usage(&usage_token()).unwrap();
        let _ = captured(&receiver, handle);

        let limit = &usage.limits[0];
        assert_eq!(limit.kind, LimitKind::Other("monthly_extra".to_string()));
        assert_eq!(limit.severity, Severity::Other("elevated".to_string()));
        assert_eq!(limit.resets_at, None);
        assert_eq!(limit.model, None);
        assert!(!limit.is_active);
    }

    #[test]
    fn a_usage_reply_that_is_not_an_object_is_a_decode_error() {
        let error = Usage::from_json(&serde_json::json!([1, 2, 3])).unwrap_err();
        assert!(matches!(error, ApiError::Decode(_)), "got {error:?}");
    }

    #[test]
    fn an_unparsable_reset_date_is_none() {
        let body = r#"{
          "limits": [
            { "kind": "session", "percent": 10, "severity": "normal",
              "resets_at": "not a date", "is_active": true }
          ]
        }"#;
        let (base, receiver, handle) = start_server(Reply::ok(body));
        let usage = api_for(&base).usage(&usage_token()).unwrap();
        let _ = captured(&receiver, handle);
        assert_eq!(usage.limits[0].resets_at, None);
    }
}
