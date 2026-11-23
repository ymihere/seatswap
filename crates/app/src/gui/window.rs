//! The large window: its label, its areas and the one command that opens it
//! (spec §3; docs/implementation.md "app: GUI", "The large window").
//!
//! The window itself is a second Tauri webview window with its own page
//! (`window.html`). The pop-up keeps `panel.rs`: both windows exist at the same
//! time, and a `snapshot` event reaches each of them.

#[cfg(feature = "gui")]
use std::cell::RefCell;
#[cfg(feature = "gui")]
use std::collections::{BTreeMap, BTreeSet};
#[cfg(feature = "gui")]
use std::future::Future;
#[cfg(feature = "gui")]
use std::path::{Path, PathBuf};

#[cfg(feature = "gui")]
use chrono::{DateTime, Duration, Local, TimeZone, Utc};
#[cfg(feature = "gui")]
use serde::{Deserialize, Serialize};

#[cfg(feature = "gui")]
use seatswap_core::api::Usage;
#[cfg(all(feature = "gui", test))]
use seatswap_core::burn::decide::Regime;
#[cfg(all(feature = "gui", test))]
use seatswap_core::burn::plan::Inputs;
#[cfg(feature = "gui")]
use seatswap_core::claude_settings;
#[cfg(feature = "gui")]
use seatswap_core::feed::Feed;
#[cfg(feature = "gui")]
use seatswap_core::history::HistoryDir;
#[cfg(feature = "gui")]
use seatswap_core::live_sessions::{
    switch_kind, LiveSession, LiveStatus, OtherHost, SwitchKind, Values, CLAUDE_PROVIDER,
};
#[cfg(feature = "gui")]
use seatswap_core::money::{is_plan_model, Fx, Prices};
#[cfg(feature = "gui")]
use seatswap_core::paths::{self, AppPaths, ClaudePaths};
#[cfg(feature = "gui")]
use seatswap_core::project::ProjectKey;
#[cfg(feature = "gui")]
use seatswap_core::projects::{self, ProjectId};
#[cfg(feature = "gui")]
use seatswap_core::stats::{
    self, activity, forecast, overview, plans, projects as stats_projects, weeks, Ctx, Empty, Query,
};
#[cfg(feature = "gui")]
use seatswap_core::store::{
    self, Account, KindSpec, ProviderSubscription, SegmentKind, Settings, StatuslineConfig,
    StatuslineInstall,
};
#[cfg(feature = "gui")]
use seatswap_core::switcher::LiveState;
#[cfg(feature = "gui")]
use seatswap_core::transcripts::Tokens;

#[cfg(feature = "gui")]
use tauri::{AppHandle, Emitter, Manager, State, WebviewUrl, WebviewWindowBuilder};

#[cfg(feature = "gui")]
use crate::statusline::render::{all_field_paths, AccountFacts, FieldPaths};
#[cfg(feature = "gui")]
use crate::statusline::{self, FactsSource, PreviewLine, RecordedKeys, Scenario};

#[cfg(feature = "gui")]
use super::burn_rt::KeptForecast;

/// Window label used by `tauri.conf.json`'s capability and by every command.
#[cfg(feature = "gui")]
pub(crate) const WINDOW_LABEL: &str = "main";

/// The four areas the window can show, in the mock-up's order.
pub(crate) const AREAS: [&str; 4] = ["sessions", "statistics", "statusline", "settings"];

/// Start size of the large window (spec §3).
#[cfg(feature = "gui")]
const WINDOW_WIDTH: f64 = 1280.0;
#[cfg(feature = "gui")]
const WINDOW_HEIGHT: f64 = 800.0;

/// `"settings/accounts"` → `("settings", Some("accounts"))`;
/// `"settings/accounts?<id>"` → `("settings", Some("accounts?<id>"))`;
/// `"sessions?<session id>"` → `("sessions", Some("<session id>"))`;
/// `"settings"` → `("settings", None)`. An item without a `/` is only for the
/// sessions area: `"settings?accounts"` is an error. Anything else is an error
/// with the reason: an unknown area, an empty sub-area, one that is not a plain
/// name (`a-z`, `0-9`, `-`) optionally followed by `?<item>` (the item a plain
/// name too), or more than one `/`.
pub(crate) fn parse_target(target: &str) -> Result<(&str, Option<&str>), String> {
    if !target.contains('/') {
        let (area, item) = match target.split_once('?') {
            Some((area, item)) => (area, Some(item)),
            None => (target, None),
        };
        if !AREAS.contains(&area) {
            return Err(format!(
                "unknown area {area:?}: use one of {}",
                AREAS.join(", ")
            ));
        }
        let Some(item) = item else {
            return Ok((area, None));
        };
        if area != "sessions" {
            return Err(format!(
                "{target:?}: only the sessions area takes an item after \"?\""
            ));
        }
        if !is_slug(item) {
            return Err(format!(
                "the item {item:?} may hold only a-z, 0-9 and \"-\""
            ));
        }
        return Ok((area, Some(item)));
    }
    let mut parts = target.split('/');
    let area = parts.next().unwrap_or_default();
    if !AREAS.contains(&area) {
        return Err(format!(
            "unknown area {area:?}: use one of {}",
            AREAS.join(", ")
        ));
    }
    match (parts.next(), parts.next()) {
        (None, None) => Ok((area, None)),
        (Some(""), None) => Err(format!(
            "the sub-area of {target:?} is empty: use <area>/<sub>"
        )),
        (Some(sub), None) if is_sub_area(sub) => Ok((area, Some(sub))),
        (Some(sub), None) => Err(format!(
            "the sub-area {sub:?} may hold only a-z, 0-9 and \"-\", optionally followed by ?<item>"
        )),
        _ => Err(format!(
            "{target:?} names more than one sub-area: use <area> or <area>/<sub>"
        )),
    }
}

/// A sub-area is a name like `accounts` or `forecast`, optionally followed by
/// `?<item>` (`settings/accounts?<account id>`): lower-case letters, digits and
/// `-`. It travels over the socket and in the page's address unencoded, so
/// nothing else may be in it.
fn is_sub_area(sub: &str) -> bool {
    match sub.split_once('?') {
        Some((name, item)) => is_slug(name) && is_slug(item),
        None => is_slug(sub),
    }
}

fn is_slug(value: &str) -> bool {
    !value.is_empty()
        && value
            .bytes()
            .all(|byte| byte.is_ascii_lowercase() || byte.is_ascii_digit() || byte == b'-')
}

/// Create the window (1280 × 800, resizable, decorated, in the taskbar, not
/// always on top) or focus it, and send it `target`.
///
/// A newly created window loads `window.html#<target>`; an existing one is
/// shown, un-minimized, focused and sent `open-area`. The two windows each have
/// their own event name, because `emit` reaches every window: the pop-up keeps
/// `navigate` (decision 3 of the plan's lead).
///
/// Every caller is on the main thread or in a synchronous command: the tray
/// menu's handler, the socket's `run_on_main_thread` and `open_window`. On
/// Windows, building a web view there deadlocks (Tauri's
/// `WebviewWindowBuilder::new`, "Known issues"): the build waits for the main
/// thread it is blocking, and the window stays white. So the work runs on a
/// thread of its own; only a bad target is returned at once. An error from the
/// thread is reported like the app's other errors (printed and notified once).
#[cfg(feature = "gui")]
pub(crate) fn show(app: &AppHandle, target: &str) -> Result<(), String> {
    parse_target(target)?;
    let app = app.clone();
    let target = target.to_string();
    std::thread::Builder::new()
        .name("seatswap-window".to_string())
        .spawn(move || {
            if let Err(error) = show_or_create(&app, &target) {
                super::report_error(&app, &format!("window: {error}"));
            }
        })
        .map(|_| ())
        .map_err(|error| format!("cannot start the window's thread: {error}"))
}

/// Focus the open window on `target`, or build it. One at a time, so two quick
/// clicks build one window and the second focuses it.
#[cfg(feature = "gui")]
fn show_or_create(app: &AppHandle, target: &str) -> Result<(), String> {
    static OPENING: std::sync::Mutex<()> = std::sync::Mutex::new(());
    let _one = OPENING
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner);
    if let Some(window) = app.get_webview_window(WINDOW_LABEL) {
        window.show().map_err(|error| error.to_string())?;
        window.unminimize().map_err(|error| error.to_string())?;
        window.set_focus().map_err(|error| error.to_string())?;
        window
            .emit("open-area", target)
            .map_err(|error| error.to_string())?;
        return Ok(());
    }
    create(app, target)
}

/// Build the large window on its first opening, with the target in its address.
#[cfg(feature = "gui")]
fn create(app: &AppHandle, target: &str) -> Result<(), String> {
    let window = build(app, target)?;
    let handle = app.clone();
    window.on_window_event(move |event| {
        if matches!(event, tauri::WindowEvent::Destroyed) {
            let Some(state) = handle.try_state::<super::GuiState>() else {
                return;
            };
            let Ok(client) = state.sessions_commands() else {
                return;
            };
            // The sessions thread may be in a pass: never hold the main thread.
            std::thread::spawn(move || {
                let _ = client.unsubscribe();
            });
        }
    });
    Ok(())
}

/// The large window's builder, opening on `target`. The window is built hidden
/// and shown and focused once its page has loaded: a web view drawn before its
/// page is in waits up to 0.5 s for a first frame (WebKit on Linux, measured in
/// the bench 2026-11-17), and that page cannot get its files meanwhile, because
/// the app's own protocol answers on the main thread that is waiting.
#[cfg(feature = "gui")]
fn builder<'a>(
    app: &'a AppHandle,
    target: &str,
) -> WebviewWindowBuilder<'a, tauri::Wry, AppHandle> {
    let shown = std::sync::atomic::AtomicBool::new(false);
    WebviewWindowBuilder::new(app, WINDOW_LABEL, WebviewUrl::App("window.html".into()))
        .title("seatswap")
        .inner_size(WINDOW_WIDTH, WINDOW_HEIGHT)
        .resizable(true)
        .decorations(true)
        .always_on_top(false)
        .skip_taskbar(false)
        .visible(false)
        // Before the page's scripts run, so the window opens on its target
        // instead of drawing Sessions first and jumping.
        .initialization_script(format!("window.location.hash = {target:?};"))
        // A failed load ends with `Finished` too, so the window always comes up;
        // a later reload does not take the focus again.
        .on_page_load(move |window, payload| {
            if payload.event() != tauri::webview::PageLoadEvent::Finished
                || shown.swap(true, std::sync::atomic::Ordering::SeqCst)
            {
                return;
            }
            let revealed = window
                .show()
                .map_err(|error| format!("cannot show the window: {error}"))
                .and_then(|()| {
                    window
                        .set_focus()
                        .map_err(|error| format!("cannot focus the window: {error}"))
                });
            if let Err(error) = revealed {
                super::report_error(window.app_handle(), &format!("window: {error}"));
            }
        })
}

/// Build the window. On Linux its web view shares the pop-up's web process
/// (WebKit's "related view"), so no new WebKit process starts: a new one first
/// sets up the graphics driver, and on a laptop whose NVIDIA card sleeps when
/// idle that waits about 2 s for the card to wake (`/dev/nvidiactl`, measured
/// in the bench 2026-11-17), and about 0.5 s more even when it is awake. The
/// pop-up is created at start (`panel::preload`), and again here if it was
/// closed since. A GTK web view is used only on the main thread, so the build
/// runs there and this thread waits for it.
#[cfg(all(feature = "gui", target_os = "linux"))]
fn build(app: &AppHandle, target: &str) -> Result<tauri::WebviewWindow, String> {
    let (built, result) = std::sync::mpsc::channel();
    let handle = app.clone();
    let target = target.to_string();
    app.run_on_main_thread(move || {
        let sender = built.clone();
        let asked = super::panel::preload(&handle)
            .and_then(|()| {
                handle
                    .get_webview_window(super::panel::PANEL_LABEL)
                    .ok_or_else(|| "the pop-up is missing".to_string())
            })
            .and_then(|panel| {
                panel
                    .with_webview(move |related| {
                        let window = builder(&handle, &target)
                            .with_related_view(related.inner())
                            .build()
                            .map_err(|error| format!("cannot open the window: {error}"));
                        let _ = sender.send(window);
                    })
                    .map_err(|error| error.to_string())
            });
        if let Err(error) = asked {
            let _ = built.send(Err(format!("cannot open the window: {error}")));
        }
    })
    .map_err(|error| format!("cannot open the window: {error}"))?;
    result
        .recv()
        .map_err(|_| "cannot open the window: the build did not finish".to_string())?
}

/// Build the window.
#[cfg(all(feature = "gui", not(target_os = "linux")))]
fn build(app: &AppHandle, target: &str) -> Result<tauri::WebviewWindow, String> {
    builder(app, target)
        .build()
        .map_err(|error| format!("cannot open the window: {error}"))
}

/// `open_window("settings")` from a page: the same as `show`.
#[cfg(feature = "gui")]
#[tauri::command]
pub(crate) fn open_window(app: AppHandle, target: String) -> Result<(), String> {
    show(&app, &target)
}

/// The sessions view. The sessions thread answers from what it holds; the
/// command runs on a blocking thread, so a pass never holds up the window.
#[cfg(feature = "gui")]
#[tauri::command]
pub(crate) async fn sessions_view(
    state: State<'_, super::GuiState>,
    options: SessionsOptions,
) -> Result<SessionsView, String> {
    let client = state.sessions_commands()?;
    let view = tauri::async_runtime::spawn_blocking(move || client.view(options))
        .await
        .map_err(|error| error.to_string())??;
    Ok(view)
}

/// Ask the sessions thread to push a view after every pass, for `options`.
#[cfg(feature = "gui")]
#[tauri::command]
pub(crate) async fn sessions_subscribe(
    state: State<'_, super::GuiState>,
    options: SessionsOptions,
) -> Result<(), String> {
    let client = state.sessions_commands()?;
    tauri::async_runtime::spawn_blocking(move || client.subscribe(options))
        .await
        .map_err(|error| error.to_string())?
}

/// Stop the pushed views; the thread drops the transcript folder watches.
#[cfg(feature = "gui")]
#[tauri::command]
pub(crate) async fn sessions_unsubscribe(state: State<'_, super::GuiState>) -> Result<(), String> {
    let client = state.sessions_commands()?;
    tauri::async_runtime::spawn_blocking(move || client.unsubscribe())
        .await
        .map_err(|error| error.to_string())?
}

/// How far back the closed sessions are listed (spec §3.8).
#[cfg(feature = "gui")]
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, PartialOrd, Ord, Deserialize)]
#[serde(rename_all = "camelCase")]
pub(crate) enum ClosedSince {
    /// The default: closed sessions are not listed at all.
    #[default]
    None,
    Day,
    Week,
    Month,
}

#[cfg(feature = "gui")]
impl ClosedSince {
    /// The first moment a closed session may have closed: 24 h, 7 d or 30 d
    /// before `now`; `None` for [`ClosedSince::None`].
    pub(crate) fn since(self, now: DateTime<Utc>) -> Option<DateTime<Utc>> {
        let days = match self {
            ClosedSince::None => return None,
            ClosedSince::Day => 1,
            ClosedSince::Week => 7,
            ClosedSince::Month => 30,
        };
        Some(now - Duration::days(days))
    }
}

/// What the Sessions area asks for. `host` is `None` for all hosts.
#[cfg(feature = "gui")]
#[derive(Debug, Clone, Default, PartialEq, Eq, Deserialize)]
#[serde(rename_all = "camelCase")]
pub(crate) struct SessionsOptions {
    pub closed: ClosedSince,
    pub host: Option<String>,
}

/// One host the view knows about.
#[cfg(feature = "gui")]
#[derive(Debug, Clone, PartialEq, Serialize)]
#[serde(rename_all = "camelCase")]
pub(crate) struct HostState {
    pub host: String,
    /// When its rows were last known (`this` host: the pass time).
    pub at: DateTime<Utc>,
    /// Another host's report is older than `REPORT_STALE_MIN`; never this host.
    pub stale: bool,
    pub this: bool,
}

/// What a switch now costs this host's running sessions (spec §2.11).
#[cfg(feature = "gui")]
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize)]
#[serde(rename_all = "camelCase")]
pub(crate) struct SwitchEffect {
    pub loses_thinking: u32,
    pub loses_thinking_busy: u32,
    /// The distinct model ids of the sessions counted in `loses_thinking`, in
    /// the order their first session is listed.
    pub loses_thinking_models: Vec<String>,
    pub resend: u32,
    pub free: u32,
}

/// Which group a [`Sum`] belongs to (spec §3.8). The page looks a row's or a
/// card's numbers up by this key; it never adds values itself. The page shows
/// the provider group and the provider's model rows too, so those keys are here
/// beside `All`, `Project` and `Model`. By project lists its sessions directly
/// under the project, with no row per model (1.4.5).
#[cfg(feature = "gui")]
#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord)]
pub(crate) enum SumOf {
    All,
    Provider(String),
    Project(ProjectId),
    /// One provider's model: the total row of "By model" (item 43). `Model`
    /// stays the Models card's total over every provider.
    ProviderModel(String, Option<String>),
    Model(Option<String>),
}

#[cfg(feature = "gui")]
impl Serialize for SumOf {
    fn serialize<S: serde::Serializer>(&self, serializer: S) -> Result<S::Ok, S::Error> {
        use serde::ser::SerializeMap;
        let mut map = serializer.serialize_map(None)?;
        match self {
            SumOf::All => map.serialize_entry("kind", "all")?,
            SumOf::Provider(provider) => {
                map.serialize_entry("kind", "provider")?;
                map.serialize_entry("provider", provider)?;
            }
            SumOf::Project(project) => {
                map.serialize_entry("kind", "project")?;
                map.serialize_entry("project", project)?;
            }
            SumOf::ProviderModel(provider, model) => {
                map.serialize_entry("kind", "providerModel")?;
                map.serialize_entry("provider", provider)?;
                map.serialize_entry("model", model)?;
            }
            SumOf::Model(model) => {
                map.serialize_entry("kind", "model")?;
                map.serialize_entry("model", model)?;
            }
        }
        map.end()
    }
}

/// One group's numbers, built over the listed sessions only (spec §3.8): every
/// total counts exactly what the page lists, and the page adds nothing. Agents
/// are summed like the tokens; `last_hour_values` values exactly the last hour
/// (so a row whose hour is not known names its reason in every sum over it),
/// and `values` the rows' whole tokens.
#[cfg(feature = "gui")]
#[derive(Debug, Clone, PartialEq, Serialize)]
#[serde(rename_all = "camelCase")]
pub(crate) struct Sum {
    pub of: SumOf,
    pub sessions: u32,
    pub agents_running: u32,
    pub agents_done: u32,
    pub tokens: Tokens,
    /// `None` when any listed row's last hour is unknown: a sum never hides an
    /// unknown part.
    pub last_hour: Option<Tokens>,
    pub last_hour_values: Values,
    pub values: Values,
}

/// What the Sessions area shows (spec §3.8). The groups' numbers come from
/// `sums`, so they always agree with the list.
#[cfg(feature = "gui")]
#[derive(Debug, Clone, PartialEq, Serialize)]
#[serde(rename_all = "camelCase")]
pub(crate) struct SessionsView {
    /// The pass time this view was built from.
    pub at: DateTime<Utc>,
    /// The history's host name; `None` when the thread runs without a history.
    pub this_host: Option<String>,
    /// This host first, then the other hosts by name.
    pub hosts: Vec<HostState>,
    /// This host's running rows in their order, then the other hosts' by host,
    /// then the closed ones newest first.
    pub sessions: Vec<LiveSession>,
    /// Every group the page shows, over the listed sessions only.
    pub sums: Vec<Sum>,
    /// This host's running sessions only, whatever the host filter says.
    pub switch_effect: SwitchEffect,
    pub currency: Option<String>,
    pub unavailable: Option<String>,
}

/// The one way the held facts become what the page shows. `at` is the pass
/// time; `other_hosts` is the last other-host read; `thinking` is each running
/// session's answers with thinking since the user's last turn.
#[cfg(feature = "gui")]
#[allow(clippy::too_many_arguments)]
pub(crate) fn view_of(
    at: DateTime<Utc>,
    this_host: Option<String>,
    running: &[LiveSession],
    closed: &[LiveSession],
    other_hosts: &[OtherHost],
    thinking: &BTreeMap<String, u32>,
    unavailable: Option<&str>,
    currency: Option<&str>,
    options: &SessionsOptions,
) -> SessionsView {
    let mut hosts = Vec::new();
    if let Some(host) = &this_host {
        hosts.push(HostState {
            host: host.clone(),
            at,
            stale: false,
            this: true,
        });
    }
    let mut others: Vec<&OtherHost> = other_hosts.iter().collect();
    others.sort_by(|a, b| a.host.cmp(&b.host));
    for other in &others {
        hosts.push(HostState {
            host: other.host.clone(),
            at: other.at,
            stale: other.stale,
            this: false,
        });
    }

    let filter = options.host.as_deref();
    let known = filter.is_none_or(|wanted| hosts.iter().any(|host| host.host == wanted));
    let mut sessions = Vec::new();
    if known {
        let this_host_listed =
            filter.is_none() || (this_host.is_some() && filter == this_host.as_deref());
        if this_host_listed {
            sessions.extend(running.iter().cloned());
        }
        for other in &others {
            if filter.is_none() || filter == Some(other.host.as_str()) {
                sessions.extend(other.sessions.iter().cloned());
            }
        }
        if let Some(since) = options.closed.since(at) {
            let mut closed_rows: Vec<&LiveSession> = closed
                .iter()
                .filter(|row| row.since >= since && row.since < at)
                .filter(|row| filter.is_none() || filter == Some(row.host.as_str()))
                .collect();
            closed_rows.sort_by_key(|row| std::cmp::Reverse(row.since));
            // A session listed as running already is the newer fact: its closed
            // row from the history is not listed a second time.
            let listed: BTreeSet<(String, String)> = sessions
                .iter()
                .map(|row| (row.host.clone(), row.session.clone()))
                .collect();
            sessions.extend(
                closed_rows
                    .into_iter()
                    .filter(|row| !listed.contains(&(row.host.clone(), row.session.clone())))
                    .cloned(),
            );
        }
    }

    let mut switch_effect = SwitchEffect::default();
    // Only a Claude Code row on a Claude plan model (or with no model known
    // yet) is touched by switching the Claude account; Codex, Harness and
    // Claude Code rows on an Ollama model are not.
    for row in running.iter().filter(|row| {
        row.provider == CLAUDE_PROVIDER && row.model.as_deref().is_none_or(is_plan_model)
    }) {
        match switch_kind(row, thinking.get(&row.session).copied().unwrap_or(0), at) {
            SwitchKind::LosesThinking => {
                switch_effect.loses_thinking += 1;
                if let Some(model) = &row.model {
                    if !switch_effect.loses_thinking_models.contains(model) {
                        switch_effect.loses_thinking_models.push(model.clone());
                    }
                }
                if row.status == LiveStatus::Busy {
                    switch_effect.loses_thinking_busy += 1;
                }
            }
            SwitchKind::Resend => switch_effect.resend += 1,
            SwitchKind::Free => switch_effect.free += 1,
        }
    }

    let sums = sums_of(&sessions);
    SessionsView {
        at,
        this_host,
        hosts,
        sessions,
        sums,
        switch_effect,
        currency: currency.map(str::to_string),
        unavailable: unavailable.map(str::to_string),
    }
}

/// Every group's numbers over exactly `sessions`: all listed, each provider,
/// each project, each project's model, each provider's model and each model
/// (item 43: a model row is a total row, so it has a sum of its own). A key
/// exists when it has a session; `All` exists even for an empty list, so the
/// page always has the total to look up.
#[cfg(feature = "gui")]
fn sums_of(sessions: &[LiveSession]) -> Vec<Sum> {
    /// One group's numbers while they are added up.
    #[derive(Default)]
    struct Group<'a> {
        sessions: u32,
        agents_running: u32,
        agents_done: u32,
        tokens: Tokens,
        last_hour: Tokens,
        last_hour_unknown: bool,
        values: Vec<&'a Values>,
        last_hour_values: Vec<&'a Values>,
    }

    let mut groups: BTreeMap<SumOf, Group<'_>> = BTreeMap::new();
    groups.insert(SumOf::All, Group::default());
    for row in sessions {
        let keys = [
            SumOf::All,
            SumOf::Provider(row.provider.clone()),
            SumOf::Project(row.project_id.clone()),
            SumOf::ProviderModel(row.provider.clone(), row.model.clone()),
            SumOf::Model(row.model.clone()),
        ];
        for key in keys {
            let group = groups.entry(key).or_default();
            group.sessions += 1;
            group.agents_running += row.agents_running;
            group.agents_done += row.agents_done;
            group.tokens += row.tokens;
            match row.last_hour {
                Some(last_hour) => group.last_hour += last_hour,
                None => group.last_hour_unknown = true,
            }
            group.values.push(&row.values);
            group.last_hour_values.push(&row.last_hour_values);
        }
    }
    groups
        .into_iter()
        .map(|(of, group)| Sum {
            of,
            sessions: group.sessions,
            agents_running: group.agents_running,
            agents_done: group.agents_done,
            tokens: group.tokens,
            last_hour: if group.last_hour_unknown {
                None
            } else {
                Some(group.last_hour)
            },
            last_hour_values: Values::sum(&group.last_hour_values),
            values: Values::sum(&group.values),
        })
        .collect()
}

// ---- the Statistics area's commands (step-c3.md, Task 8) ----

/// A statistics page: the data, or the "nothing recorded" marker.
#[cfg(feature = "gui")]
#[derive(Debug, Clone, PartialEq, Serialize)]
#[serde(rename_all = "camelCase")]
pub(crate) enum Page<T> {
    Empty(Empty),
    Data(T),
}

/// Everything a page reads besides the history, loaded once per command.
#[cfg(feature = "gui")]
#[derive(Debug)]
pub(crate) struct StatsParts {
    accounts: Vec<Account>,
    /// The provider subscriptions the same `accounts.json` stores (decision
    /// P-2 1): their prices and the OpenAI logins a record may belong to.
    provider_subscriptions: Vec<ProviderSubscription>,
    settings: Settings,
    prices: Prices,
    fx: Fx,
    projects: projects::Projects,
    /// The history read warnings of the command; the shared printer gets them
    /// once after the page ran.
    warnings: RefCell<Vec<String>>,
}

#[cfg(feature = "gui")]
impl StatsParts {
    pub(crate) fn ctx<'a, Tz: TimeZone + Copy>(
        &'a self,
        history: &'a HistoryDir,
        now: DateTime<Utc>,
        zone: Tz,
    ) -> Ctx<'a, Tz> {
        Ctx {
            history,
            accounts: &self.accounts,
            provider_subscriptions: &self.provider_subscriptions,
            settings: &self.settings,
            prices: &self.prices,
            fx: &self.fx,
            projects: &self.projects,
            warnings: &self.warnings,
            now,
            zone,
        }
    }

    /// The warnings the command's reads collected, leaving the list empty.
    pub(crate) fn take_warnings(&self) -> Vec<String> {
        std::mem::take(&mut *self.warnings.borrow_mut())
    }
}

/// Read the prices and the dollar rate the app shows money with. Any failure
/// is the text the page shows.
#[cfg(feature = "gui")]
pub(crate) fn load_prices_and_fx(app: &AppPaths) -> Result<(Prices, Fx), String> {
    let prices =
        Prices::load(app).map_err(|error| format!("the prices cannot be read: {error}"))?;
    let fx = Fx::load(app).map_err(|error| format!("the dollar rate cannot be read: {error}"))?;
    Ok((prices, fx))
}

/// Load the accounts file, the prices, the dollar rate and the projects for
/// one statistics command. Any failure is the text the page shows.
#[cfg(feature = "gui")]
pub(crate) fn stats_ctx_parts(app: &AppPaths, history: &HistoryDir) -> Result<StatsParts, String> {
    let file = store::load_accounts(app)
        .map_err(|error| format!("the accounts cannot be read: {error}"))?;
    let (prices, fx) = load_prices_and_fx(app)?;
    let (projects, warnings) = projects::load(history, &file.accounts)
        .map_err(|error| format!("the projects cannot be read: {error}"))?;
    Ok(StatsParts {
        accounts: file.accounts,
        provider_subscriptions: file.provider_subscriptions,
        settings: file.settings,
        prices,
        fx,
        projects,
        warnings: RefCell::new(warnings),
    })
}

/// The one way a page becomes [`Page`]: "nothing recorded" first, else the
/// page's own numbers.
#[cfg(feature = "gui")]
fn stats_page<T, Tz: TimeZone + Copy>(
    parts: &StatsParts,
    history: &HistoryDir,
    now: DateTime<Utc>,
    zone: Tz,
    query: &Query,
    compute: impl FnOnce(&Ctx<'_, Tz>) -> Result<T, stats::StatsError>,
) -> Result<Page<T>, String> {
    let ctx = parts.ctx(history, now, zone);
    let range = stats::range_of(query.range, &ctx).map_err(|error| error.to_string())?;
    match stats::empty(&ctx, &range).map_err(|error| error.to_string())? {
        Some(empty) => Ok(Page::Empty(empty)),
        None => compute(&ctx)
            .map(Page::Data)
            .map_err(|error| error.to_string()),
    }
}

#[cfg(feature = "gui")]
pub(crate) fn stats_overview_at<Tz: TimeZone + Copy>(
    parts: &StatsParts,
    history: &HistoryDir,
    now: DateTime<Utc>,
    zone: Tz,
    query: &Query,
) -> Result<Page<overview::Overview>, String> {
    stats_page(parts, history, now, zone, query, |ctx| {
        overview::overview(ctx, query)
    })
}

/// The Overview's This-week card. It ignores the query's range, so it never
/// says "nothing recorded" and does not go through [`stats_page`]: without a
/// running window its lists are empty and the page says so itself.
#[cfg(feature = "gui")]
pub(crate) fn stats_this_week_at<Tz: TimeZone + Copy>(
    parts: &StatsParts,
    history: &HistoryDir,
    now: DateTime<Utc>,
    zone: Tz,
    query: &Query,
) -> Result<overview::ThisWeek, String> {
    overview::this_week_card(&parts.ctx(history, now, zone), query)
        .map_err(|error| error.to_string())
}

#[cfg(feature = "gui")]
pub(crate) fn stats_weeks_at<Tz: TimeZone + Copy>(
    parts: &StatsParts,
    history: &HistoryDir,
    now: DateTime<Utc>,
    zone: Tz,
    query: &Query,
) -> Result<Page<weeks::WeeksPage>, String> {
    stats_page(parts, history, now, zone, query, |ctx| {
        weeks::weeks(ctx, query)
    })
}

#[cfg(feature = "gui")]
pub(crate) fn stats_week_summary_at<Tz: TimeZone + Copy>(
    parts: &StatsParts,
    history: &HistoryDir,
    now: DateTime<Utc>,
    zone: Tz,
    query: &Query,
    week: DateTime<Utc>,
) -> Result<Page<weeks::WeekSummary>, String> {
    stats_page(parts, history, now, zone, query, |ctx| {
        weeks::week_summary(ctx, query, week)
    })
}

#[cfg(feature = "gui")]
pub(crate) fn stats_window_detail_at<Tz: TimeZone + Copy>(
    parts: &StatsParts,
    history: &HistoryDir,
    now: DateTime<Utc>,
    zone: Tz,
    query: &Query,
    account_id: &str,
    end: DateTime<Utc>,
) -> Result<Page<weeks::WindowDetail>, String> {
    stats_page(parts, history, now, zone, query, |ctx| {
        weeks::window_detail(ctx, account_id, end)
    })
}

#[cfg(feature = "gui")]
pub(crate) fn stats_projects_at<Tz: TimeZone + Copy>(
    parts: &StatsParts,
    history: &HistoryDir,
    now: DateTime<Utc>,
    zone: Tz,
    query: &Query,
) -> Result<Page<stats_projects::ProjectsPage>, String> {
    stats_page(parts, history, now, zone, query, |ctx| {
        stats_projects::projects_page(ctx, query)
    })
}

#[cfg(feature = "gui")]
pub(crate) fn stats_project_detail_at<Tz: TimeZone + Copy>(
    parts: &StatsParts,
    history: &HistoryDir,
    now: DateTime<Utc>,
    zone: Tz,
    query: &Query,
    key: &ProjectKey,
) -> Result<Page<stats_projects::ProjectDetail>, String> {
    stats_page(parts, history, now, zone, query, |ctx| {
        stats_projects::project_detail(ctx, query, key)
    })
}

#[cfg(feature = "gui")]
pub(crate) fn stats_subscriptions_at<Tz: TimeZone + Copy>(
    parts: &StatsParts,
    history: &HistoryDir,
    now: DateTime<Utc>,
    zone: Tz,
    query: &Query,
) -> Result<Page<plans::SubscriptionsPage>, String> {
    stats_page(parts, history, now, zone, query, |ctx| {
        plans::subscriptions_page(ctx, query)
    })
}

/// The Forecast page's error while there is no plan. The page shows it, and
/// every error that starts with it and a colon, as a plain line.
#[cfg(feature = "gui")]
pub(crate) const NO_PLAN: &str = "the forecast needs a reading of every account first";

/// Why the Forecast page has no plan: [`NO_PLAN`], and while auto-burn waits for
/// a first reading of some accounts, which ones and when it comes. The accounts
/// are the auto-burn view's `unread`, named by their abbreviations.
#[cfg(feature = "gui")]
pub(crate) fn no_plan_text(
    burn: Option<&super::burn_rt::BurnView>,
    views: &[seatswap_core::engine::AccountView],
) -> Result<String, String> {
    let unread = burn.map(|view| view.unread.as_slice()).unwrap_or(&[]);
    if unread.is_empty() {
        return Ok(NO_PLAN.to_string());
    }
    let abbrs = unread
        .iter()
        .map(|id| crate::burn_text::abbr(views, id))
        .collect::<Result<Vec<_>, _>>()
        .map_err(|error| error.to_string())?;
    let (has, comes) = if abbrs.len() == 1 {
        ("has", "it comes")
    } else {
        ("have", "they come")
    };
    Ok(format!(
        "{NO_PLAN}: {} {has} none yet; {comes} with the next usage poll",
        abbrs.join(", ")
    ))
}

/// The forecast has no empty marker: it either has a plan or the error
/// `no_plan` ([`no_plan_text`]). A reading older than the burn runtime's
/// freshness is the same as no reading; the age is the oldest reading the
/// situation kept, not the tick time its plan was built for.
#[cfg(feature = "gui")]
pub(crate) fn stats_forecast_at<Tz: TimeZone + Copy>(
    parts: &StatsParts,
    history: &HistoryDir,
    now: DateTime<Utc>,
    zone: Tz,
    kept: Option<KeptForecast>,
    no_plan: String,
) -> Result<Page<forecast::Forecast>, String> {
    let Some((inputs, regime, oldest_reading)) = kept else {
        return Err(no_plan);
    };
    if now.signed_duration_since(oldest_reading) > Duration::minutes(seatswap_core::burn::FRESH_MIN)
    {
        return Err(no_plan);
    }
    let ctx = parts.ctx(history, now, zone);
    forecast::forecast(&ctx, &inputs, &regime)
        .map(Page::Data)
        .map_err(|error| error.to_string())
}

#[cfg(feature = "gui")]
pub(crate) fn stats_rhythm_at<Tz: TimeZone + Copy>(
    parts: &StatsParts,
    history: &HistoryDir,
    now: DateTime<Utc>,
    zone: Tz,
    query: &Query,
) -> Result<Page<activity::RhythmPage>, String> {
    stats_page(parts, history, now, zone, query, |ctx| {
        activity::rhythm(ctx, query)
    })
}

#[cfg(feature = "gui")]
pub(crate) fn stats_activity_at<Tz: TimeZone + Copy>(
    parts: &StatsParts,
    history: &HistoryDir,
    now: DateTime<Utc>,
    zone: Tz,
    query: &Query,
) -> Result<Page<activity::ActivityPage>, String> {
    stats_page(parts, history, now, zone, query, |ctx| {
        activity::activity(ctx, query)
    })
}

#[cfg(feature = "gui")]
pub(crate) fn stats_frame_at(history: &HistoryDir) -> Result<stats::StatsFrame, String> {
    stats::frame(history).map_err(|error| error.to_string())
}

/// The one body of a statistics command: on the blocking thread it reads the
/// files the page needs, then runs the page's own function with the moment and
/// the local zone it uses there.
#[cfg(feature = "gui")]
async fn stats_command<T, F>(state: State<'_, super::GuiState>, run: F) -> Result<T, String>
where
    T: Send + 'static,
    F: FnOnce(&StatsParts, &HistoryDir, DateTime<Utc>, Local) -> Result<T, String> + Send + 'static,
{
    let app = state.app.clone();
    let history = state
        .history
        .as_ref()
        .map_err(|error| error.clone())?
        .clone();
    let printed = std::sync::Arc::clone(&state.printed_warnings);
    tauri::async_runtime::spawn_blocking(move || {
        let parts = stats_ctx_parts(&app, &history)?;
        let result = run(&parts, &history, Utc::now(), Local);
        let warnings = parts.take_warnings();
        super::lock(&printed).print(Some("history"), &warnings);
        result
    })
    .await
    .map_err(|error| error.to_string())?
}

#[cfg(feature = "gui")]
#[tauri::command]
pub(crate) async fn stats_overview(
    state: State<'_, super::GuiState>,
    query: Query,
) -> Result<Page<overview::Overview>, String> {
    stats_command(state, move |parts, history, now, offset| {
        stats_overview_at(parts, history, now, offset, &query)
    })
    .await
}

#[cfg(feature = "gui")]
#[tauri::command]
pub(crate) async fn stats_this_week(
    state: State<'_, super::GuiState>,
    query: Query,
) -> Result<overview::ThisWeek, String> {
    stats_command(state, move |parts, history, now, offset| {
        stats_this_week_at(parts, history, now, offset, &query)
    })
    .await
}

#[cfg(feature = "gui")]
#[tauri::command]
pub(crate) async fn stats_weeks(
    state: State<'_, super::GuiState>,
    query: Query,
) -> Result<Page<weeks::WeeksPage>, String> {
    stats_command(state, move |parts, history, now, offset| {
        stats_weeks_at(parts, history, now, offset, &query)
    })
    .await
}

#[cfg(feature = "gui")]
#[tauri::command]
pub(crate) async fn stats_week_summary(
    state: State<'_, super::GuiState>,
    query: Query,
    week: DateTime<Utc>,
) -> Result<Page<weeks::WeekSummary>, String> {
    stats_command(state, move |parts, history, now, offset| {
        stats_week_summary_at(parts, history, now, offset, &query, week)
    })
    .await
}

#[cfg(feature = "gui")]
#[tauri::command]
pub(crate) async fn stats_window_detail(
    state: State<'_, super::GuiState>,
    query: Query,
    account_id: String,
    end: DateTime<Utc>,
) -> Result<Page<weeks::WindowDetail>, String> {
    stats_command(state, move |parts, history, now, offset| {
        stats_window_detail_at(parts, history, now, offset, &query, &account_id, end)
    })
    .await
}

#[cfg(feature = "gui")]
#[tauri::command]
pub(crate) async fn stats_projects(
    state: State<'_, super::GuiState>,
    query: Query,
) -> Result<Page<stats_projects::ProjectsPage>, String> {
    stats_command(state, move |parts, history, now, offset| {
        stats_projects_at(parts, history, now, offset, &query)
    })
    .await
}

#[cfg(feature = "gui")]
#[tauri::command]
pub(crate) async fn stats_project_detail(
    state: State<'_, super::GuiState>,
    query: Query,
    key: ProjectKey,
) -> Result<Page<stats_projects::ProjectDetail>, String> {
    stats_command(state, move |parts, history, now, offset| {
        stats_project_detail_at(parts, history, now, offset, &query, &key)
    })
    .await
}

#[cfg(feature = "gui")]
#[tauri::command]
pub(crate) async fn stats_subscriptions(
    state: State<'_, super::GuiState>,
    query: Query,
) -> Result<Page<plans::SubscriptionsPage>, String> {
    stats_command(state, move |parts, history, now, offset| {
        stats_subscriptions_at(parts, history, now, offset, &query)
    })
    .await
}

#[cfg(feature = "gui")]
#[tauri::command]
pub(crate) async fn stats_forecast(
    state: State<'_, super::GuiState>,
) -> Result<Page<forecast::Forecast>, String> {
    let inputs = super::lock(&state.forecast).clone();
    let snapshot = state.snapshot()?;
    let no_plan = no_plan_text(snapshot.burn.as_ref(), &snapshot.views)?;
    stats_command(state, move |parts, history, now, offset| {
        stats_forecast_at(parts, history, now, offset, inputs, no_plan)
    })
    .await
}

#[cfg(feature = "gui")]
#[tauri::command]
pub(crate) async fn stats_rhythm(
    state: State<'_, super::GuiState>,
    query: Query,
) -> Result<Page<activity::RhythmPage>, String> {
    stats_command(state, move |parts, history, now, offset| {
        stats_rhythm_at(parts, history, now, offset, &query)
    })
    .await
}

#[cfg(feature = "gui")]
#[tauri::command]
pub(crate) async fn stats_activity(
    state: State<'_, super::GuiState>,
    query: Query,
) -> Result<Page<activity::ActivityPage>, String> {
    stats_command(state, move |parts, history, now, offset| {
        stats_activity_at(parts, history, now, offset, &query)
    })
    .await
}

#[cfg(feature = "gui")]
#[tauri::command]
pub(crate) async fn stats_frame(
    state: State<'_, super::GuiState>,
) -> Result<stats::StatsFrame, String> {
    // The frame needs only the history, not the parts a page reads: a broken
    // prices file must not hide the hosts, the history's start or its size.
    let history = state
        .history
        .as_ref()
        .map_err(|error| error.clone())?
        .clone();
    tauri::async_runtime::spawn_blocking(move || stats_frame_at(&history))
        .await
        .map_err(|error| error.to_string())?
}

/// The preview's source: the data the page already has, never a file read of
/// its own.
#[cfg(feature = "gui")]
struct SnapshotFacts<'a> {
    snapshot: &'a super::worker::Snapshot,
    app: &'a AppPaths,
}

#[cfg(feature = "gui")]
impl FactsSource for SnapshotFacts<'_> {
    fn host_name(&self) -> Option<&str> {
        self.snapshot.settings.as_ref()?.host_name.as_deref()
    }

    fn account(&self, account_id: &str) -> Option<AccountFacts> {
        self.snapshot
            .views
            .iter()
            .find(|view| view.id == account_id)
            .map(|view| AccountFacts {
                abbr: view.abbr.clone(),
                color: view.color.clone(),
                plan: view.plan.clone(),
            })
    }

    fn usage(&self, account_id: &str, _feed: Option<&Feed>) -> Option<Usage> {
        // The snapshot's numbers are the engine's merged view already.
        self.snapshot
            .views
            .iter()
            .find(|view| view.id == account_id)?
            .usage
            .clone()
    }

    fn model_limits(&self, account_id: &str) -> Vec<(String, f64)> {
        self.snapshot
            .views
            .iter()
            .find(|view| view.id == account_id)
            .and_then(|view| view.usage.as_ref())
            .map(statusline::model_limits_of)
            .unwrap_or_default()
    }

    fn burn_next(&self, now: DateTime<Utc>) -> Result<Option<(String, DateTime<Utc>)>, String> {
        // The same file the command reads: the snapshot's burn plan says what
        // auto-burn would do, whether or not it is on, while the line shows
        // only what the tray app wrote.
        statusline::burn_next_account(self.app, now, |account_id| {
            self.snapshot
                .views
                .iter()
                .find(|view| view.id == account_id)
                .map(|view| view.abbr.clone())
        })
    }
}

/// The snapshot's live login in the shape [`statusline::facts_from_source`]
/// takes.
#[cfg(feature = "gui")]
fn live_state_of(live: &super::worker::LiveView) -> LiveState {
    match live {
        super::worker::LiveView::NoLogin => LiveState::NoLogin,
        super::worker::LiveView::Ended { account_id } => LiveState::Ended {
            account_id: account_id.clone(),
        },
        super::worker::LiveView::Known { account_id } => LiveState::Known {
            account_id: account_id.clone(),
        },
        super::worker::LiveView::Unknown { email, org_name } => LiveState::Unknown {
            email: email.clone(),
            org_name: org_name.clone(),
        },
    }
}

/// Render the builder's preview once per width: the given config, never the
/// stored one, and [`statusline::preview_payload`]'s made-up session — a folder
/// under the live home, the sample branch, context, cost and lines — with the
/// live account's numbers. `preview_lines` is the one place the widths are
/// checked.
#[cfg(feature = "gui")]
#[tauri::command]
pub(crate) fn statusline_preview(
    state: State<'_, super::GuiState>,
    config: StatuslineConfig,
    widths: Vec<u16>,
    scenario: Scenario,
) -> Result<Vec<PreviewLine>, String> {
    let snapshot = state.snapshot()?;
    if snapshot.settings.is_none() {
        return Err("accounts.json cannot be read; the preview has no data".to_string());
    }
    let payload = statusline::preview_payload(state.claude.home());
    let now = Utc::now();
    let facts = statusline::facts_from_source(
        &SnapshotFacts {
            snapshot: &snapshot,
            app: &state.app,
        },
        &payload,
        &live_state_of(&snapshot.live),
        state.claude.home(),
        paths::user_name()?,
        now,
    )?;
    statusline::preview_lines(&config, &payload, &facts, &widths, scenario, &Local)
}

/// The builder's three presets, in the page's order.
#[cfg(feature = "gui")]
#[tauri::command]
pub(crate) fn statusline_presets() -> Vec<(String, StatuslineConfig)> {
    StatuslineConfig::presets()
}

/// The builder's one kind table, in the palette's order: each kind's label,
/// group, colour rules, and whether it takes a time format, text or repeats.
#[cfg(feature = "gui")]
#[tauri::command]
pub(crate) fn statusline_kinds() -> Vec<KindSpec> {
    SegmentKind::ALL.iter().map(|kind| kind.spec()).collect()
}

/// Start a recording for `minutes` by changing the one setting through the
/// settings page's save command, the one writer of `accounts.json`.
#[cfg(feature = "gui")]
#[tauri::command]
pub(crate) async fn statusline_record(
    state: State<'_, super::GuiState>,
    minutes: u32,
) -> Result<(), String> {
    let until = statusline::record_until(minutes, Utc::now())?;
    let value =
        serde_json::to_value(until).map_err(|error| format!("statuslineRecordUntil: {error}"))?;
    super::panel::save_setting(state, "statuslineRecordUntil".to_string(), value)
        .await
        .map(|_saved| ())
}

/// The field names recorded so far, or `None` when no recording has written
/// the keys file.
#[cfg(feature = "gui")]
#[tauri::command]
pub(crate) fn statusline_keys(
    state: State<'_, super::GuiState>,
) -> Result<Option<RecordedKeys>, String> {
    statusline::read_recorded_keys(&state.app)
}

/// The payload field paths of the optional segments, from render.rs's one
/// table; the page marks a segment whose path a recording did not see.
#[cfg(feature = "gui")]
#[tauri::command]
pub(crate) fn statusline_fields() -> Vec<FieldPaths> {
    all_field_paths()
}

/// What Claude Code's status line is now, what seatswap would install, and the
/// record of seatswap's install (spec §5.4). `restorable` follows the restore
/// rule: a record exists and the current line is seatswap's or the record's
/// `installed` line; `previous` is the record's previous value (`null` when
/// there is no record).
#[cfg(feature = "gui")]
#[derive(Debug, Clone, PartialEq, Serialize)]
#[serde(rename_all = "camelCase")]
pub(crate) struct InstallState {
    pub current: Option<serde_json::Value>,
    pub ours: serde_json::Value,
    pub installed: bool,
    pub restorable: bool,
    pub previous: Option<serde_json::Value>,
    /// Claude Code's settings file, as the page shows it.
    pub settings_file: String,
}

/// The install rule's decision, taken from one reading of Claude Code's file.
/// `record` is the new record to save before the write (or `None` when the
/// record stays); `write` is seatswap's line to write (or `None` when it is
/// already there); `state` describes the file after the install.
#[cfg(feature = "gui")]
#[derive(Debug, Clone, PartialEq)]
struct InstallDecision {
    record: Option<StatuslineInstall>,
    write: Option<serde_json::Value>,
    state: InstallState,
}

/// The restore rule's decision: what to put back, the line that was there
/// before the restore (for the rollback if clearing the record fails), and the
/// state after the restore.
#[cfg(feature = "gui")]
#[derive(Debug, Clone, PartialEq)]
struct RestoreDecision {
    restore: Option<serde_json::Value>,
    before: Option<serde_json::Value>,
    state: InstallState,
}

/// The console `seatswap` program beside the running one; on Windows the tray
/// program runs this command too, so `current_exe` may be `seatswap-tray.exe`.
#[cfg(feature = "gui")]
fn console_program() -> Result<PathBuf, String> {
    let current = super::autostart::running_program()?;
    super::autostart::console_program(&current).map_err(|error| error.to_string())
}

/// The settings page's one writer keeps `statuslineInstall`; this command only
/// reads the saved record and sends changes back through `save_setting`.
#[cfg(feature = "gui")]
fn saved_install(state: &super::GuiState) -> Result<Option<StatuslineInstall>, String> {
    let snapshot = state.snapshot()?;
    let settings = snapshot.settings.ok_or_else(|| {
        "accounts.json cannot be read; the status line's install record is unknown".to_string()
    })?;
    Ok(settings.statusline_install)
}

/// One reading of Claude Code's status line: seatswap's line, the line the file
/// holds now, and the file's path as the page shows it.
#[cfg(feature = "gui")]
struct StatusLineReading {
    ours: serde_json::Value,
    current: Option<serde_json::Value>,
    settings_file: String,
}

#[cfg(feature = "gui")]
impl StatusLineReading {
    /// The same reading with `current` set to the line a decision writes. The
    /// install state after a change is built from it, so the state builder stays
    /// the one place an [`InstallState`] is made.
    fn with_current(&self, current: Option<serde_json::Value>) -> Self {
        StatusLineReading {
            ours: self.ours.clone(),
            current,
            settings_file: self.settings_file.clone(),
        }
    }
}

/// The one reading of Claude Code's file for the install and restore rules:
/// seatswap's line for `exe`, the current line and the file name.
#[cfg(feature = "gui")]
fn read_status_lines(claude: &ClaudePaths, exe: &Path) -> Result<StatusLineReading, String> {
    Ok(StatusLineReading {
        ours: claude_settings::seatswap_status_line(exe).map_err(|error| error.to_string())?,
        current: claude_settings::status_line(claude).map_err(|error| error.to_string())?,
        settings_file: claude.settings_file().display().to_string(),
    })
}

/// The one place an [`InstallState`] is built, from a reading and the record.
/// `installed` compares the line with seatswap's; `restorable` is true exactly
/// when the restore rule allows it: a record exists and the current line is
/// seatswap's or the record's `installed` line.
#[cfg(feature = "gui")]
fn install_state_of(
    reading: &StatusLineReading,
    record: Option<&StatuslineInstall>,
) -> InstallState {
    InstallState {
        current: reading.current.clone(),
        ours: reading.ours.clone(),
        installed: reading.current.as_ref() == Some(&reading.ours),
        restorable: restorable(record, reading.current.as_ref(), &reading.ours),
        previous: record.and_then(|record| record.previous.clone()),
        settings_file: reading.settings_file.clone(),
    }
}

/// The restore rule: a record exists and the current line is seatswap's or the
/// record's `installed` line (seatswap's older line).
#[cfg(feature = "gui")]
fn restorable(
    record: Option<&StatuslineInstall>,
    current: Option<&serde_json::Value>,
    ours: &serde_json::Value,
) -> bool {
    record.is_some_and(|record| current == Some(ours) || current == Some(&record.installed))
}

/// The install rule, decided from one reading of Claude Code's file:
/// (a) the current line is seatswap's → write nothing and keep the record;
/// (b) a record exists and the current line is the record's `installed` line
/// (seatswap's older line) → keep the record's `previous` and write ours;
/// (c) otherwise (the user's line, or no record) → the current value becomes
/// the record's `previous` and ours is written. A present `null` previous is
/// kept as `Some(null)`; an absent key is `None`.
#[cfg(feature = "gui")]
fn install_decision(
    claude: &ClaudePaths,
    exe: &Path,
    record: Option<StatuslineInstall>,
) -> Result<InstallDecision, String> {
    let reading = read_status_lines(claude, exe)?;
    if reading.current.as_ref() == Some(&reading.ours) {
        return Ok(InstallDecision {
            record: None,
            write: None,
            state: install_state_of(&reading, record.as_ref()),
        });
    }
    let previous = match &record {
        Some(record) if reading.current.as_ref() == Some(&record.installed) => {
            record.previous.clone()
        }
        _ => reading.current.clone(),
    };
    let new_record = StatuslineInstall {
        installed: reading.ours.clone(),
        previous: previous.clone(),
    };
    let state = install_state_of(
        &reading.with_current(Some(reading.ours.clone())),
        Some(&new_record),
    );
    Ok(InstallDecision {
        record: Some(new_record),
        write: Some(reading.ours),
        state,
    })
}

/// The restore rule from one reading of Claude Code's file. It needs a record
/// and the current line must be seatswap's or the record's `installed` line;
/// without a record it refuses with its own message, and with any other line it
/// keeps today's refusal. `restore` is the record's `previous` exactly (`None`
/// removes the key); the state has the record cleared.
#[cfg(feature = "gui")]
fn restore_decision(
    claude: &ClaudePaths,
    exe: &Path,
    record: Option<StatuslineInstall>,
) -> Result<RestoreDecision, String> {
    let reading = read_status_lines(claude, exe)?;
    let Some(record) = record else {
        return Err(
            "seatswap has no record of the line before its own: nothing to restore".to_string(),
        );
    };
    if !restorable(Some(&record), reading.current.as_ref(), &reading.ours) {
        return Err(
            "the status line in Claude Code is not seatswap's: nothing to restore".to_string(),
        );
    }
    let restored = record.previous;
    let state = install_state_of(&reading.with_current(restored.clone()), None);
    Ok(RestoreDecision {
        restore: restored,
        before: reading.current,
        state,
    })
}

/// The status line's current state in Claude Code, the value seatswap would
/// install, and the install record kept in seatswap's settings.
#[cfg(feature = "gui")]
#[tauri::command]
pub(crate) fn statusline_install_state(
    state: State<'_, super::GuiState>,
) -> Result<InstallState, String> {
    let exe = console_program()?;
    let record = saved_install(&state)?;
    let reading = read_status_lines(&state.claude, &exe)?;
    Ok(install_state_of(&reading, record.as_ref()))
}

/// Turn seatswap's status line on in Claude Code. [`install_decision`] reads the
/// file once and decides the new record and the write; `save` stores the record
/// before seatswap's line is written, so a refused save leaves Claude Code's
/// file untouched. The command passes `save_setting` as `save`; the tests pass a
/// fake. Installing again while ours is in place writes nothing and leaves the
/// record unchanged.
#[cfg(feature = "gui")]
async fn install_status_line<F, Fut>(
    claude: &ClaudePaths,
    exe: &Path,
    record: Option<StatuslineInstall>,
    save: F,
) -> Result<InstallState, String>
where
    F: FnOnce(StatuslineInstall) -> Fut,
    Fut: Future<Output = Result<(), String>>,
{
    let decision = install_decision(claude, exe, record)?;
    if let Some(record) = decision.record {
        save(record).await?;
    }
    if let Some(ours) = &decision.write {
        claude_settings::set_status_line(claude, Some(ours)).map_err(|error| error.to_string())?;
    }
    Ok(decision.state)
}

#[cfg(feature = "gui")]
#[tauri::command]
pub(crate) async fn statusline_install(
    state: State<'_, super::GuiState>,
) -> Result<InstallState, String> {
    let exe = console_program()?;
    let record = saved_install(&state)?;
    let claude = state.claude.clone();
    install_status_line(&claude, &exe, record, |record| async move {
        let value =
            serde_json::to_value(&record).map_err(|error| format!("statuslineInstall: {error}"))?;
        super::panel::save_setting(state, "statuslineInstall".to_string(), value)
            .await
            .map(|_saved| ())
    })
    .await
}

/// Put the record's `previous` value back exactly (a present `null` is written
/// as `null`, an absent one removes the key), then clear the record with
/// `save`. It refuses without a record, and when the line in Claude Code is
/// neither seatswap's nor the record's `installed` line, so a later hand-made
/// line is never overwritten. If clearing the record fails, the line that was
/// there before the restore is written back. The command passes `save_setting`
/// as `save`; the tests pass a fake.
#[cfg(feature = "gui")]
async fn restore_status_line<F, Fut>(
    claude: &ClaudePaths,
    exe: &Path,
    record: Option<StatuslineInstall>,
    save: F,
) -> Result<InstallState, String>
where
    F: FnOnce() -> Fut,
    Fut: Future<Output = Result<(), String>>,
{
    let decision = restore_decision(claude, exe, record)?;
    claude_settings::set_status_line(claude, decision.restore.as_ref())
        .map_err(|error| error.to_string())?;
    if let Err(error) = save().await {
        if let Err(rollback) = claude_settings::set_status_line(claude, decision.before.as_ref()) {
            return Err(format!(
                "{error}; the status line could not be put back: {rollback}"
            ));
        }
        return Err(error);
    }
    Ok(decision.state)
}

#[cfg(feature = "gui")]
#[tauri::command]
pub(crate) async fn statusline_restore(
    state: State<'_, super::GuiState>,
) -> Result<InstallState, String> {
    let exe = console_program()?;
    let record = saved_install(&state)?;
    let claude = state.claude.clone();
    restore_status_line(&claude, &exe, record, || async move {
        super::panel::save_setting(
            state,
            "statuslineInstall".to_string(),
            serde_json::Value::Null,
        )
        .await
        .map(|_saved| ())
    })
    .await
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_target_is_an_area_with_an_optional_sub() {
        assert_eq!(
            parse_target("settings/accounts"),
            Ok(("settings", Some("accounts")))
        );
        assert_eq!(
            parse_target("settings/accounts?8564d960fb9a498daede9211fc09c9f8"),
            Ok((
                "settings",
                Some("accounts?8564d960fb9a498daede9211fc09c9f8")
            ))
        );
        assert!(parse_target("settings/accounts?").is_err());
        assert!(parse_target("settings/?x").is_err());
        assert!(parse_target("settings/accounts?a b").is_err());
        assert!(parse_target("nonsense").is_err());
        assert_eq!(parse_target("settings"), Ok(("settings", None)));
        assert_eq!(
            parse_target("sessions?7c1e4b2a-9d3f-4e6a-b8c5-2f0a1d7e9b34"),
            Ok(("sessions", Some("7c1e4b2a-9d3f-4e6a-b8c5-2f0a1d7e9b34")))
        );
        assert!(parse_target("sessions?").is_err());
        assert!(parse_target("sessions?7c1e4b2a-9d3f-4e6a-b8c5-2f0a1d7e9b34/x").is_err());
        assert!(parse_target("settings/").is_err());
        assert!(parse_target("settings/a/b").is_err());
        assert!(parse_target("settings/a b").is_err());
        assert!(parse_target("settings/a\nb").is_err());
        assert!(parse_target("settings/Übersicht").is_err());
    }

    #[test]
    fn review_an_item_after_another_area_is_not_a_sub_area() {
        assert!(
            parse_target("settings?accounts").is_err(),
            "\"settings?accounts\" is read as the sub-area accounts"
        );
        assert!(parse_target("statistics?x").is_err());
    }

    // ---- the Sessions view (spec §3.8) ----

    #[cfg(feature = "gui")]
    use seatswap_core::history::HistoryDir;
    #[cfg(feature = "gui")]
    use seatswap_core::live_sessions::{other_hosts, write_report, Values};
    #[cfg(feature = "gui")]
    use seatswap_core::store::Settings;
    #[cfg(feature = "gui")]
    use seatswap_core::transcripts::Tokens;

    #[cfg(feature = "gui")]
    const ATLAS: &str = "7c1e4b2a-9d3f-4e6a-b8c5-2f0a1d7e9b34";
    #[cfg(feature = "gui")]
    const VEGA: &str = "dd5e0c1a-3b7f-4c2d-9e8a-6f1b2c3d4e5f";
    #[cfg(feature = "gui")]
    const CLOSED_RECENT: &str = "11a2b3c4-d5e6-4f70-8a9b-0c1d2e3f4a5b";
    #[cfg(feature = "gui")]
    const CLOSED_OLD: &str = "2b7d9c1e-5a4f-4c3b-9e8d-7f6a5b4c3d2e";

    #[cfg(feature = "gui")]
    fn time(text: &str) -> DateTime<Utc> {
        text.parse().expect("a test time")
    }

    #[cfg(feature = "gui")]
    fn row(
        session: &str,
        host: &str,
        status: LiveStatus,
        model: Option<&str>,
        since: DateTime<Utc>,
    ) -> LiveSession {
        LiveSession {
            session: session.to_string(),
            host: host.to_string(),
            label: format!("acme-kb-{}", session.chars().take(2).collect::<String>()),
            title: None,
            project_id: ProjectId::of(
                &seatswap_core::project::ProjectKey::Repo("github.com/acme/acme-kb".to_string()),
                host,
            ),
            project: "acme-kb".to_string(),
            org: None,
            cwd: "/home/sam/projects/acme-kb".to_string(),
            provider: "Claude Code".to_string(),
            model: model.map(str::to_string),
            tied: model
                .map(seatswap_core::burn::tied::thinking_tied)
                .unwrap_or(false),
            status,
            since,
            started: since,
            agents_running: 0,
            agents_done: 0,
            agents: Vec::new(),
            tokens: Tokens::default(),
            last_hour: Some(Tokens::default()),
            last_hour_values: Values::of(&[], &Settings::default(), &Fx::default(), Utc::now()),
            agents_done_values: Values::sum(&[]),
            values: Values::of(&[], &Settings::default(), &Fx::default(), Utc::now()),
        }
    }

    #[cfg(feature = "gui")]
    fn id(index: u32) -> String {
        format!("{index:08x}-1111-4111-8111-111111111111")
    }

    #[cfg(feature = "gui")]
    fn options(closed: ClosedSince, host: Option<&str>) -> SessionsOptions {
        SessionsOptions {
            closed,
            host: host.map(str::to_string),
        }
    }

    /// A vega report as `write_report` writes it, read back by this host.
    #[cfg(feature = "gui")]
    fn vega_report(
        root: &std::path::Path,
        now: DateTime<Utc>,
        sessions: Vec<LiveSession>,
    ) -> Vec<OtherHost> {
        std::fs::create_dir_all(root.join("vega")).expect("the vega folder");
        std::fs::write(
            root.join("vega").join("host.json"),
            "{\"machineId\":\"made-up\"}",
        )
        .expect("the host marker");
        let vega = HistoryDir::at(root, "vega");
        write_report(&vega, &sessions, now).expect("a vega report");
        let atlas = HistoryDir::at(root, "atlas");
        other_hosts(&atlas, now).expect("the other hosts").hosts
    }

    #[cfg(feature = "gui")]
    #[test]
    fn the_view_lists_this_host_and_other_hosts() {
        let tmp = tempfile::tempdir().expect("tempdir");
        let now = time("2026-11-14T10:00:00Z");
        let other = vega_report(
            tmp.path(),
            now,
            vec![row(
                VEGA,
                "vega",
                LiveStatus::Busy,
                Some("claude-opus-5-5"),
                now - Duration::minutes(2),
            )],
        );
        let view = view_of(
            now,
            Some("atlas".to_string()),
            &[row(
                ATLAS,
                "atlas",
                LiveStatus::Busy,
                Some("claude-opus-5-5"),
                now - Duration::minutes(3),
            )],
            &[],
            &other,
            &BTreeMap::new(),
            None,
            Some("EUR"),
            &SessionsOptions::default(),
        );

        assert_eq!(view.sessions.len(), 2, "{:?}", view.sessions);
        assert_eq!(view.sessions[0].session, ATLAS);
        assert_eq!(view.sessions[1].session, VEGA);
        assert_eq!(view.hosts.len(), 2);
        assert_eq!(view.hosts[0].host, "atlas");
        assert!(view.hosts[0].this);
        assert_eq!(view.hosts[1].host, "vega");
        assert!(!view.hosts[1].this);
        assert_eq!(view.this_host.as_deref(), Some("atlas"));
    }

    #[cfg(feature = "gui")]
    #[test]
    fn closed_none_lists_only_running() {
        let now = time("2026-11-14T10:00:00Z");
        let view = view_of(
            now,
            Some("atlas".to_string()),
            &[row(
                ATLAS,
                "atlas",
                LiveStatus::Busy,
                Some("claude-opus-5-5"),
                now - Duration::minutes(3),
            )],
            &[row(
                CLOSED_RECENT,
                "atlas",
                LiveStatus::Closed,
                Some("claude-opus-5-5"),
                now - Duration::hours(3),
            )],
            &[],
            &BTreeMap::new(),
            None,
            None,
            &options(ClosedSince::None, None),
        );

        assert_eq!(view.sessions.len(), 1);
        assert_eq!(view.sessions[0].status, LiveStatus::Busy);
    }

    #[cfg(feature = "gui")]
    #[test]
    fn closed_day_adds_the_closed_ones() {
        let now = time("2026-11-14T10:00:00Z");
        let recent = row(
            CLOSED_RECENT,
            "atlas",
            LiveStatus::Closed,
            Some("claude-opus-5-5"),
            now - Duration::hours(3),
        );
        let old = row(
            CLOSED_OLD,
            "atlas",
            LiveStatus::Closed,
            Some("claude-opus-5-5"),
            now - Duration::hours(30),
        );
        let day = view_of(
            now,
            Some("atlas".to_string()),
            &[],
            &[recent.clone(), old.clone()],
            &[],
            &BTreeMap::new(),
            None,
            None,
            &options(ClosedSince::Day, None),
        );
        assert!(day.sessions.iter().any(|row| row.session == CLOSED_RECENT));
        assert!(
            !day.sessions.iter().any(|row| row.session == CLOSED_OLD),
            "a session closed 30h ago is not in the day"
        );
        assert_eq!(day.sessions[0].status, LiveStatus::Closed);

        let week = view_of(
            now,
            Some("atlas".to_string()),
            &[],
            &[recent, old],
            &[],
            &BTreeMap::new(),
            None,
            None,
            &options(ClosedSince::Week, None),
        );
        assert_eq!(week.sessions.len(), 2);
        assert_eq!(week.sessions[0].session, CLOSED_RECENT, "newest first");
        assert_eq!(week.sessions[1].session, CLOSED_OLD);
    }

    #[cfg(feature = "gui")]
    #[test]
    fn a_session_running_on_another_host_is_not_listed_closed_too() {
        let tmp = tempfile::tempdir().expect("tempdir");
        let now = time("2026-11-14T10:00:00Z");
        let mut running = row(
            VEGA,
            "vega",
            LiveStatus::Busy,
            Some("claude-opus-5-5"),
            now - Duration::minutes(2),
        );
        running.tokens.out = 1_000_000;
        let other = vega_report(tmp.path(), now, vec![running]);
        let mut closed = row(
            VEGA,
            "vega",
            LiveStatus::Closed,
            Some("claude-opus-5-5"),
            now - Duration::hours(3),
        );
        closed.tokens.out = 9_000_000;

        let view = view_of(
            now,
            Some("atlas".to_string()),
            &[],
            &[closed],
            &other,
            &BTreeMap::new(),
            None,
            None,
            &options(ClosedSince::Day, None),
        );

        assert_eq!(view.sessions.len(), 1, "{:?}", view.sessions);
        assert_eq!(view.sessions[0].session, VEGA);
        assert_eq!(view.sessions[0].status, LiveStatus::Busy);
        let all = view
            .sums
            .iter()
            .find(|sum| sum.of == SumOf::All)
            .expect("the total");
        assert_eq!(all.sessions, 1);
        assert_eq!(
            all.tokens.out, 1_000_000,
            "the closed row's tokens are not counted a second time"
        );
    }

    #[cfg(feature = "gui")]
    #[test]
    fn a_host_filter_keeps_only_that_host() {
        let tmp = tempfile::tempdir().expect("tempdir");
        let now = time("2026-11-14T10:00:00Z");
        let other = vega_report(
            tmp.path(),
            now,
            vec![row(
                VEGA,
                "vega",
                LiveStatus::Busy,
                Some("claude-opus-5-5"),
                now - Duration::minutes(2),
            )],
        );
        let running = [row(
            ATLAS,
            "atlas",
            LiveStatus::Busy,
            Some("claude-opus-5-5"),
            now - Duration::minutes(3),
        )];
        let vega = view_of(
            now,
            Some("atlas".to_string()),
            &running,
            &[],
            &other,
            &BTreeMap::new(),
            None,
            None,
            &options(ClosedSince::None, Some("vega")),
        );
        assert_eq!(vega.sessions.len(), 1);
        assert_eq!(vega.sessions[0].host, "vega");
        assert_eq!(vega.hosts.len(), 2, "the list of hosts stays complete");

        let unknown = view_of(
            now,
            Some("atlas".to_string()),
            &running,
            &[],
            &other,
            &BTreeMap::new(),
            None,
            None,
            &options(ClosedSince::None, Some("nope")),
        );
        assert!(unknown.sessions.is_empty());
    }

    #[cfg(feature = "gui")]
    #[test]
    fn the_switch_effect_counts_each_kind() {
        let now = time("2026-11-14T10:00:00Z");
        let mut running = Vec::new();
        let mut thinking = BTreeMap::new();
        // 3 losing: one busy, two idle but only recently.
        running.push(row(
            &id(0),
            "atlas",
            LiveStatus::Busy,
            Some("claude-fable-5-2"),
            now,
        ));
        for index in 1..3 {
            running.push(row(
                &id(index),
                "atlas",
                LiveStatus::Idle,
                Some("claude-fable-5-2"),
                now - Duration::minutes(10),
            ));
        }
        for index in 0..3 {
            thinking.insert(id(index), 1);
        }
        // 2 resend: idle on a model whose thinking survives, no thinking held.
        for index in 3..5 {
            running.push(row(
                &id(index),
                "atlas",
                LiveStatus::Idle,
                Some("claude-opus-5-5"),
                now - Duration::minutes(10),
            ));
        }
        // 4 free: idle for longer than the stored copy lives.
        for index in 5..9 {
            running.push(row(
                &id(index),
                "atlas",
                LiveStatus::Idle,
                Some("claude-opus-5-5"),
                now - Duration::minutes(61),
            ));
        }
        let view = view_of(
            now,
            Some("atlas".to_string()),
            &running,
            &[],
            &[],
            &thinking,
            None,
            None,
            &SessionsOptions::default(),
        );
        assert_eq!(
            view.switch_effect,
            SwitchEffect {
                loses_thinking: 3,
                loses_thinking_busy: 1,
                loses_thinking_models: vec!["claude-fable-5-2".to_string()],
                resend: 2,
                free: 4,
            }
        );
    }

    #[cfg(feature = "gui")]
    #[test]
    fn the_switch_effect_names_the_models_that_lose_thinking() {
        let now = time("2026-11-14T10:00:00Z");
        let running = vec![
            row(
                &id(0),
                "atlas",
                LiveStatus::Busy,
                Some("claude-fable-5-2"),
                now,
            ),
            row(
                &id(1),
                "atlas",
                LiveStatus::Idle,
                Some("claude-sonnet-5-5"),
                now - Duration::minutes(10),
            ),
            row(
                &id(2),
                "atlas",
                LiveStatus::Idle,
                Some("claude-fable-5-2"),
                now - Duration::minutes(10),
            ),
        ];
        let thinking: BTreeMap<String, u32> =
            running.iter().map(|row| (row.session.clone(), 1)).collect();
        let view = view_of(
            now,
            Some("atlas".to_string()),
            &running,
            &[],
            &[],
            &thinking,
            None,
            None,
            &SessionsOptions::default(),
        );

        assert_eq!(view.switch_effect.loses_thinking, 3);
        assert_eq!(
            view.switch_effect.loses_thinking_models,
            vec![
                "claude-fable-5-2".to_string(),
                "claude-sonnet-5-5".to_string()
            ],
            "each model once, in the order its first session is listed"
        );
    }

    #[cfg(feature = "gui")]
    #[test]
    fn the_switch_effect_counts_only_this_host() {
        let tmp = tempfile::tempdir().expect("tempdir");
        let now = time("2026-11-14T10:00:00Z");
        let other = vega_report(
            tmp.path(),
            now,
            vec![row(
                VEGA,
                "vega",
                LiveStatus::Busy,
                Some("claude-fable-5-2"),
                now,
            )],
        );
        let mut thinking = BTreeMap::new();
        thinking.insert(ATLAS.to_string(), 1);
        thinking.insert(VEGA.to_string(), 1);
        let view = view_of(
            now,
            Some("atlas".to_string()),
            &[row(
                ATLAS,
                "atlas",
                LiveStatus::Busy,
                Some("claude-fable-5-2"),
                now,
            )],
            &[row(
                CLOSED_RECENT,
                "atlas",
                LiveStatus::Closed,
                Some("claude-fable-5-2"),
                now - Duration::hours(1),
            )],
            &other,
            &thinking,
            None,
            None,
            &options(ClosedSince::Day, None),
        );
        assert_eq!(
            view.switch_effect,
            SwitchEffect {
                loses_thinking: 1,
                loses_thinking_busy: 1,
                loses_thinking_models: vec!["claude-fable-5-2".to_string()],
                resend: 0,
                free: 0,
            },
            "another host's row and a closed row do not change this host's switch cost"
        );
    }

    #[cfg(feature = "gui")]
    #[test]
    fn codex_rows_do_not_count_in_the_switch_effect() {
        let now = time("2026-11-14T10:00:00Z");
        let mut busy = row(&id(20), "atlas", LiveStatus::Busy, Some("gpt-6.1-sol"), now);
        busy.provider = "Codex".to_string();
        let mut old = row(
            &id(21),
            "atlas",
            LiveStatus::Idle,
            Some("gpt-6.1-sol"),
            now - Duration::minutes(61),
        );
        old.provider = "Codex".to_string();
        let view = view_of(
            now,
            Some("atlas".to_string()),
            &[busy, old],
            &[],
            &[],
            &BTreeMap::new(),
            None,
            None,
            &SessionsOptions::default(),
        );
        assert_eq!(
            view.switch_effect,
            SwitchEffect::default(),
            "a switch of the Claude account touches neither Codex row"
        );
    }

    #[cfg(feature = "gui")]
    #[test]
    fn a_view_without_a_history_lists_this_hosts_sessions() {
        let now = time("2026-11-14T10:00:00Z");
        let view = view_of(
            now,
            None,
            &[row(
                ATLAS,
                "",
                LiveStatus::Busy,
                Some("claude-opus-5-5"),
                now,
            )],
            &[],
            &[],
            &BTreeMap::new(),
            Some("the history cannot be read"),
            None,
            &SessionsOptions::default(),
        );
        assert_eq!(view.this_host, None);
        assert!(view.hosts.is_empty());
        assert_eq!(view.sessions.len(), 1);
        assert_eq!(view.sessions[0].host, "");
        assert_eq!(
            view.unavailable.as_deref(),
            Some("the history cannot be read")
        );
    }

    #[cfg(feature = "gui")]
    #[test]
    fn the_sums_cover_every_group() {
        fn find<'a>(view: &'a SessionsView, of: &SumOf) -> &'a Sum {
            view.sums
                .iter()
                .find(|sum| &sum.of == of)
                .unwrap_or_else(|| panic!("no sum for {of:?}"))
        }
        use seatswap_core::project::ProjectKey;

        let now = time("2026-11-14T10:00:00Z");
        let first = ProjectId::of(
            &ProjectKey::Repo("github.com/acme/acme-kb".to_string()),
            "atlas",
        );
        let second = ProjectId::of(
            &ProjectKey::Repo("github.com/sam/acme-kb".to_string()),
            "atlas",
        );
        let mut a = row(
            ATLAS,
            "atlas",
            LiveStatus::Busy,
            Some("claude-opus-5-5"),
            now,
        );
        a.project_id = first.clone();
        a.tokens.out = 1_000_000;
        let mut b = row(
            "aaaa1111-1111-4111-8111-111111111111",
            "atlas",
            LiveStatus::Idle,
            Some("claude-opus-5-5"),
            now,
        );
        b.project_id = second.clone();
        b.tokens.out = 2_000_000;
        let mut c = row(
            "bbbb1111-1111-4111-8111-111111111111",
            "atlas",
            LiveStatus::Idle,
            Some("claude-fable-5-2"),
            now,
        );
        c.project_id = first.clone();
        c.tokens.out = 4_000_000;
        let mut d = row(
            "cccc1111-1111-4111-8111-111111111111",
            "atlas",
            LiveStatus::Idle,
            None,
            now,
        );
        d.project_id = first.clone();
        d.provider = "Codex".to_string();
        d.tokens.out = 8_000_000;
        let view = view_of(
            now,
            Some("atlas".to_string()),
            &[a, b, c, d],
            &[],
            &[],
            &BTreeMap::new(),
            None,
            Some("EUR"),
            &SessionsOptions::default(),
        );

        assert_eq!(view.sums.len(), 11, "{:?}", view.sums);
        assert_eq!(find(&view, &SumOf::All).sessions, 4);
        assert_eq!(find(&view, &SumOf::All).tokens.out, 15_000_000);
        assert_eq!(
            find(&view, &SumOf::Provider("Claude Code".to_string())).sessions,
            3
        );
        assert_eq!(
            find(&view, &SumOf::Provider("Claude Code".to_string()))
                .tokens
                .out,
            7_000_000
        );
        assert_eq!(
            find(&view, &SumOf::Provider("Codex".to_string())).sessions,
            1
        );
        assert_eq!(find(&view, &SumOf::Project(first.clone())).sessions, 3);
        assert_eq!(
            find(&view, &SumOf::Project(first.clone())).tokens.out,
            13_000_000
        );
        assert_eq!(
            find(&view, &SumOf::Project(second.clone())).sessions,
            1,
            "two projects with the same name are two groups"
        );
        assert_eq!(
            find(&view, &SumOf::Model(Some("claude-opus-5-5".to_string())))
                .tokens
                .out,
            3_000_000
        );
        assert_eq!(
            find(&view, &SumOf::Model(Some("claude-fable-5-2".to_string())))
                .tokens
                .out,
            4_000_000
        );
        assert_eq!(find(&view, &SumOf::Model(None)).tokens.out, 8_000_000);
        assert_eq!(
            find(
                &view,
                &SumOf::ProviderModel(
                    "Claude Code".to_string(),
                    Some("claude-opus-5-5".to_string())
                )
            )
            .sessions,
            2,
            "both projects' opus sessions are one provider model"
        );
        assert_eq!(
            find(
                &view,
                &SumOf::ProviderModel(
                    "Claude Code".to_string(),
                    Some("claude-opus-5-5".to_string())
                )
            )
            .tokens
            .out,
            3_000_000
        );
        assert_eq!(
            find(&view, &SumOf::ProviderModel("Codex".to_string(), None))
                .tokens
                .out,
            8_000_000
        );
    }

    #[cfg(feature = "gui")]
    #[test]
    /// A group's sum carries the values and the last hour of exactly the rows
    /// it covers: the closed row is in the sum of the day, and another host's
    /// row in another currency is not added.
    fn the_sums_carry_the_values_of_their_rows() {
        use seatswap_core::project::ProjectKey;

        fn find<'a>(view: &'a SessionsView, of: &SumOf) -> &'a Sum {
            view.sums
                .iter()
                .find(|sum| &sum.of == of)
                .unwrap_or_else(|| panic!("no sum for {of:?}"))
        }

        fn valued(subscription: f64, api: f64, currency: &str) -> Values {
            Values {
                api: Some(api),
                api_currency: Some(currency.to_string()),
                api_missing: None,
                api_partial: false,
                subscription: Some(subscription),
                currency: Some(currency.to_string()),
                subscription_missing: None,
                subscription_partial: false,
            }
        }

        let now = time("2026-11-14T10:00:00Z");
        let acme = ProjectId::of(
            &ProjectKey::Repo("github.com/acme/acme-kb".to_string()),
            "atlas",
        );
        let ledger = ProjectId::of(
            &ProjectKey::Repo("github.com/sam/ledger".to_string()),
            "atlas",
        );
        let mut a = row(
            ATLAS,
            "atlas",
            LiveStatus::Busy,
            Some("claude-opus-5-5"),
            now,
        );
        a.project_id = acme.clone();
        a.tokens.out = 1_000_000;
        a.last_hour.as_mut().expect("a known hour").out = 100_000;
        a.values = valued(2.0, 20.0, "EUR");
        let mut b = row(
            "aaaa1111-1111-4111-8111-111111111111",
            "vega",
            LiveStatus::Busy,
            Some("claude-opus-5-5"),
            now,
        );
        b.project_id = acme.clone();
        b.tokens.out = 2_000_000;
        b.last_hour.as_mut().expect("a known hour").out = 200_000;
        b.values = valued(4.0, 40.0, "USD");
        let mut c = row(
            "bbbb1111-1111-4111-8111-111111111111",
            "atlas",
            LiveStatus::Idle,
            Some("claude-fable-5-2"),
            now,
        );
        c.project_id = ledger.clone();
        c.tokens.out = 3_000_000;
        c.last_hour.as_mut().expect("a known hour").out = 300_000;
        c.values = valued(6.0, 60.0, "EUR");
        let mut done = row(
            CLOSED_RECENT,
            "atlas",
            LiveStatus::Closed,
            Some("claude-fable-5-2"),
            now - Duration::hours(3),
        );
        done.project_id = ledger.clone();
        done.tokens.out = 500_000;
        done.last_hour.as_mut().expect("a known hour").out = 50_000;
        done.values = valued(1.0, 10.0, "EUR");

        let view = view_of(
            now,
            Some("atlas".to_string()),
            &[a, b, c],
            &[done],
            &[],
            &BTreeMap::new(),
            None,
            Some("EUR"),
            &options(ClosedSince::Day, None),
        );

        // The ledger group holds this host's row and the closed one.
        let ledger_sum = find(&view, &SumOf::Project(ledger.clone()));
        assert_eq!(ledger_sum.sessions, 2, "the closed row is in the day");
        assert_eq!(ledger_sum.tokens.out, 3_500_000);
        assert_eq!(ledger_sum.last_hour.expect("a known hour").out, 350_000);
        assert_eq!(ledger_sum.values.subscription, Some(7.0));
        assert_eq!(ledger_sum.values.api, Some(70.0));
        assert_eq!(ledger_sum.values.currency.as_deref(), Some("EUR"));

        // The group of the other host's row holds two currencies: nothing is added.
        let acme_sum = find(&view, &SumOf::Project(acme.clone()));
        assert_eq!(acme_sum.sessions, 2);
        assert_eq!(acme_sum.tokens.out, 3_000_000);
        assert_eq!(acme_sum.last_hour.expect("a known hour").out, 300_000);
        assert_eq!(acme_sum.values.subscription, None);
        assert_eq!(acme_sum.values.currency, None);
        assert_eq!(
            acme_sum.values.subscription_missing.as_deref(),
            Some("these sessions are valued in two currencies")
        );
        assert_eq!(acme_sum.values.api, None);
        assert_eq!(
            acme_sum.values.api_missing.as_deref(),
            Some(
                "the dollar rate is missing for some of these days, so the values are in two currencies"
            )
        );

        // The total of the day counts every listed row.
        let all = find(&view, &SumOf::All);
        assert_eq!(all.sessions, 4);
        assert_eq!(all.tokens.out, 6_500_000);
        assert_eq!(all.last_hour.expect("a known hour").out, 650_000);
    }

    #[cfg(feature = "gui")]
    #[test]
    fn sums_carry_last_hour_and_agents() {
        use seatswap_core::project::ProjectKey;

        fn find<'a>(view: &'a SessionsView, of: &SumOf) -> &'a Sum {
            view.sums
                .iter()
                .find(|sum| &sum.of == of)
                .unwrap_or_else(|| panic!("no sum for {of:?}"))
        }

        fn valued(subscription: f64, api: f64, currency: &str) -> Values {
            Values {
                api: Some(api),
                api_currency: Some(currency.to_string()),
                api_missing: None,
                api_partial: false,
                subscription: Some(subscription),
                currency: Some(currency.to_string()),
                subscription_missing: None,
                subscription_partial: false,
            }
        }

        let now = time("2026-11-14T10:00:00Z");
        let acme = ProjectId::of(
            &ProjectKey::Repo("github.com/acme/acme-kb".to_string()),
            "atlas",
        );
        let mut first = row(
            ATLAS,
            "atlas",
            LiveStatus::Busy,
            Some("claude-opus-5-5"),
            now,
        );
        first.project_id = acme.clone();
        first.agents_running = 2;
        first.agents_done = 3;
        first.last_hour.as_mut().expect("a known hour").out = 100_000;
        first.last_hour_values = valued(2.0, 20.0, "EUR");
        first.values = valued(7.0, 70.0, "EUR");
        let mut second = row(
            "aaaa1111-1111-4111-8111-111111111111",
            "atlas",
            LiveStatus::Idle,
            Some("claude-opus-5-5"),
            now,
        );
        second.project_id = acme.clone();
        second.agents_running = 1;
        second.agents_done = 0;
        second.last_hour.as_mut().expect("a known hour").out = 200_000;
        second.last_hour_values = valued(4.0, 40.0, "EUR");
        second.values = valued(9.0, 90.0, "EUR");
        let view = view_of(
            now,
            Some("atlas".to_string()),
            &[first, second],
            &[],
            &[],
            &BTreeMap::new(),
            None,
            Some("EUR"),
            &SessionsOptions::default(),
        );

        let project = find(&view, &SumOf::Project(acme.clone()));
        assert_eq!(project.sessions, 2);
        assert_eq!(project.agents_running, 3, "2 running plus 1 running");
        assert_eq!(project.agents_done, 3, "3 done plus none done");
        assert_eq!(
            project.last_hour.expect("a known hour").out,
            300_000,
            "the last hour is the two sessions' hours added"
        );
        assert_eq!(
            project.last_hour_values.subscription,
            Some(6.0),
            "the hour's values are added, not the sessions' totals"
        );
        assert_eq!(project.last_hour_values.api, Some(60.0));
        assert_eq!(
            project.values.subscription,
            Some(16.0),
            "the sessions' own totals are valued separately from their hour"
        );
        assert_eq!(project.values.api, Some(160.0));

        let all = find(&view, &SumOf::All);
        assert_eq!(all.agents_running, 3);
        assert_eq!(all.agents_done, 3);
        assert_eq!(all.last_hour.expect("a known hour").out, 300_000);
    }

    /// Nothing used is a known 0 (fix 1): a project with one session that has no
    /// answers and one with tokens sums to the second session's values, not to a
    /// missing value with "no tokens yet".
    #[cfg(feature = "gui")]
    #[test]
    fn a_project_sum_skips_a_session_without_answers_as_a_known_zero() {
        fn find<'a>(view: &'a SessionsView, of: &SumOf) -> &'a Sum {
            view.sums
                .iter()
                .find(|sum| &sum.of == of)
                .unwrap_or_else(|| panic!("no sum for {of:?}"))
        }

        let now = time("2026-11-14T10:00:00Z");
        let project = ProjectId::of(
            &seatswap_core::project::ProjectKey::Repo("github.com/acme/acme-kb".to_string()),
            "atlas",
        );
        let mut silent = row(
            ATLAS,
            "atlas",
            LiveStatus::Idle,
            None,
            now - Duration::minutes(10),
        );
        silent.project_id = project.clone();
        assert_eq!(silent.values.api, Some(0.0), "no answers is a known 0");
        assert_eq!(silent.values.api_missing, None);
        let mut speaking = row(
            "aaaa1111-1111-4111-8111-111111111111",
            "atlas",
            LiveStatus::Busy,
            Some("claude-opus-5-5"),
            now,
        );
        speaking.project_id = project.clone();
        speaking.tokens.out = 1_000_000;
        speaking.values = Values {
            api: Some(20.357),
            api_currency: Some("EUR".to_string()),
            api_missing: None,
            api_partial: false,
            subscription: Some(1.47),
            currency: Some("EUR".to_string()),
            subscription_missing: None,
            subscription_partial: false,
        };
        let view = view_of(
            now,
            Some("atlas".to_string()),
            &[silent, speaking.clone()],
            &[],
            &[],
            &BTreeMap::new(),
            None,
            Some("EUR"),
            &SessionsOptions::default(),
        );

        let sum = find(&view, &SumOf::Project(project));
        assert_eq!(sum.sessions, 2);
        assert_eq!(
            sum.values, speaking.values,
            "the sum is the speaking session's values; the silent one adds a known 0"
        );
        assert_eq!(sum.values.api_missing, None);
        assert_eq!(sum.values.subscription_missing, None);
    }

    /// A Codex row whose token total went down has no last hour; the sum over
    /// it carries the reason instead of a 0 (item 7), so every total row that
    /// covers the row says why its hour is not known.
    #[cfg(feature = "gui")]
    #[test]
    fn a_sum_carries_an_unknown_last_hour_as_its_reason() {
        const REASON: &str =
            "the Codex session's token total went down, so its last hour is not known";

        fn find<'a>(view: &'a SessionsView, of: &SumOf) -> &'a Sum {
            view.sums
                .iter()
                .find(|sum| &sum.of == of)
                .unwrap_or_else(|| panic!("no sum for {of:?}"))
        }

        let now = time("2026-11-14T10:00:00Z");
        let mut codex = row(
            ATLAS,
            "atlas",
            LiveStatus::Idle,
            Some("gpt-6-astra"),
            now - Duration::minutes(35),
        );
        codex.provider = "Codex".to_string();
        codex.tokens.out = 400_000;
        codex.last_hour = None;
        codex.last_hour_values = Values {
            api: None,
            api_currency: None,
            api_missing: Some(REASON.to_string()),
            api_partial: false,
            subscription: None,
            currency: Some("EUR".to_string()),
            subscription_missing: Some(REASON.to_string()),
            subscription_partial: false,
        };
        let view = view_of(
            now,
            Some("atlas".to_string()),
            &[codex],
            &[],
            &[],
            &BTreeMap::new(),
            None,
            Some("EUR"),
            &SessionsOptions::default(),
        );

        for of in [
            SumOf::All,
            SumOf::Provider("Codex".to_string()),
            SumOf::Project(view.sessions[0].project_id.clone()),
            SumOf::ProviderModel("Codex".to_string(), Some("gpt-6-astra".to_string())),
            SumOf::Model(Some("gpt-6-astra".to_string())),
        ] {
            let sum = find(&view, &of);
            assert_eq!(sum.last_hour, None);
            assert_eq!(
                sum.last_hour_values.api_missing.as_deref(),
                Some(REASON),
                "{of:?} keeps the unknown hour's reason"
            );
            assert_eq!(
                sum.last_hour_values.subscription_missing.as_deref(),
                Some(REASON)
            );
        }
    }

    /// The brief's test line for item 7 has a project with a Codex row of an
    /// unknown last hour **and** a Claude Code row: the sum's hour names the
    /// reason, but the row whose hour is known still counts its tokens.
    #[cfg(feature = "gui")]
    #[test]
    fn a_sum_over_an_unknown_hour_is_unknown() {
        const REASON: &str =
            "the Codex session's token total went down, so its last hour is not known";

        fn find<'a>(view: &'a SessionsView, of: &SumOf) -> &'a Sum {
            view.sums
                .iter()
                .find(|sum| &sum.of == of)
                .unwrap_or_else(|| panic!("no sum for {of:?}"))
        }

        fn valued(subscription: f64, api: f64, currency: &str) -> Values {
            Values {
                api: Some(api),
                api_currency: Some(currency.to_string()),
                api_missing: None,
                api_partial: false,
                subscription: Some(subscription),
                currency: Some(currency.to_string()),
                subscription_missing: None,
                subscription_partial: false,
            }
        }

        let now = time("2026-11-14T10:00:00Z");
        let mut claude = row(
            ATLAS,
            "atlas",
            LiveStatus::Idle,
            Some("claude-opus-5-5"),
            now,
        );
        claude.tokens.out = 1_000_000;
        claude.last_hour.as_mut().expect("a known hour").out = 100_000;
        claude.last_hour_values = valued(2.0, 20.0, "EUR");
        claude.values = valued(7.0, 70.0, "EUR");
        let mut codex = row(
            "aaaa1111-1111-4111-8111-111111111111",
            "atlas",
            LiveStatus::Idle,
            Some("gpt-6-astra"),
            now - Duration::minutes(35),
        );
        codex.provider = "Codex".to_string();
        codex.tokens.out = 400_000;
        codex.last_hour = None;
        codex.last_hour_values = Values {
            api: None,
            api_currency: None,
            api_missing: Some(REASON.to_string()),
            api_partial: false,
            subscription: None,
            currency: Some("EUR".to_string()),
            subscription_missing: Some(REASON.to_string()),
            subscription_partial: false,
        };
        codex.values = valued(1.0, 10.0, "EUR");
        let view = view_of(
            now,
            Some("atlas".to_string()),
            &[claude, codex],
            &[],
            &[],
            &BTreeMap::new(),
            None,
            Some("EUR"),
            &SessionsOptions::default(),
        );

        let project = find(&view, &SumOf::Project(view.sessions[0].project_id.clone()));
        assert_eq!(project.sessions, 2);
        assert_eq!(
            project.last_hour, None,
            "the unknown part makes the sum unknown"
        );
        assert_eq!(
            project.last_hour_values.api_missing.as_deref(),
            Some(REASON),
            "the hour's value names the reason"
        );
        assert_eq!(project.values.subscription, Some(8.0));
        assert_eq!(project.values.api, Some(80.0));
    }

    #[cfg(feature = "gui")]
    #[test]
    fn a_model_under_two_providers_has_one_sum_per_provider() {
        const FLASH: &str = "deepseek-v4.1-flash:cloud";

        fn sums_for<'a>(view: &'a SessionsView, provider: &str, model: &str) -> Vec<&'a Sum> {
            view.sums
                .iter()
                .filter(|sum| {
                    matches!(
                        &sum.of,
                        SumOf::ProviderModel(of_provider, of_model)
                            if of_provider == provider && of_model.as_deref() == Some(model)
                    )
                })
                .collect()
        }

        let now = time("2026-11-14T10:00:00Z");
        let claude = row(
            ATLAS,
            "atlas",
            LiveStatus::Busy,
            Some(FLASH),
            now - Duration::minutes(2),
        );
        let mut harness = row(
            "aaaa1111-1111-4111-8111-111111111111",
            "atlas",
            LiveStatus::Idle,
            Some(FLASH),
            now - Duration::minutes(1),
        );
        harness.provider = "DeepSeek Harness".to_string();
        let project = claude.project_id.clone();
        let view = view_of(
            now,
            Some("atlas".to_string()),
            &[claude, harness],
            &[],
            &[],
            &BTreeMap::new(),
            None,
            Some("EUR"),
            &SessionsOptions::default(),
        );

        let under_claude = sums_for(&view, "Claude Code", FLASH);
        let under_harness = sums_for(&view, "DeepSeek Harness", FLASH);
        assert_eq!(under_claude.len(), 1, "{:?}", view.sums);
        assert_eq!(under_harness.len(), 1, "{:?}", view.sums);
        assert_eq!(under_claude[0].sessions, 1);
        assert_eq!(under_harness[0].sessions, 1);
        let model = view
            .sums
            .iter()
            .find(|sum| sum.of == SumOf::Model(Some(FLASH.to_string())))
            .expect("one sum over both providers");
        assert_eq!(model.sessions, 2);

        assert_eq!(
            serde_json::to_value(SumOf::ProviderModel(
                "DeepSeek Harness".to_string(),
                Some(FLASH.to_string())
            ))
            .expect("a sum key as JSON"),
            serde_json::json!({
                "kind": "providerModel",
                "provider": "DeepSeek Harness",
                "model": FLASH,
            })
        );
        assert_eq!(
            serde_json::to_value(Sum {
                of: SumOf::ProviderModel("Codex".to_string(), None),
                sessions: 1,
                agents_running: 2,
                agents_done: 3,
                tokens: Tokens {
                    input: 1,
                    out: 2,
                    cw: 3,
                    cr: 4
                },
                last_hour: Some(Tokens {
                    input: 5,
                    out: 6,
                    cw: 7,
                    cr: 8
                }),
                last_hour_values: Values::sum(&[]),
                values: Values::sum(&[]),
            })
            .expect("a sum as JSON")["agentsRunning"],
            serde_json::json!(2)
        );
        assert_eq!(
            serde_json::to_value(SumOf::ProviderModel("Codex".to_string(), None))
                .expect("a key as JSON"),
            serde_json::json!({"kind": "providerModel", "provider": "Codex", "model": null})
        );
        assert_eq!(
            serde_json::to_value(SumOf::Project(project)).expect("a project key as JSON"),
            serde_json::json!({
                "kind": "project",
                "project": {
                    "kind": "repo",
                    "id": "github.com/acme/acme-kb",
                },
            })
        );
    }

    #[cfg(feature = "gui")]
    use seatswap_core::burn::decide::RegimeWhy;
    #[cfg(feature = "gui")]
    use seatswap_core::burn::plan::{AccountState, SessionState};
    #[cfg(feature = "gui")]
    use seatswap_core::burn::{Rates, Rhythm};
    #[cfg(feature = "gui")]
    use seatswap_core::stats::fixture;
    #[cfg(feature = "gui")]
    use seatswap_core::stats::RangeKey;

    #[cfg(feature = "gui")]
    #[test]
    fn an_empty_range_serialises_as_empty() {
        let page = Page::<()>::Empty(Empty {
            recording_since: None,
        });
        assert_eq!(
            serde_json::to_string(&page).expect("the page as JSON"),
            r#"{"empty":{"recordingSince":null}}"#
        );
    }

    /// Two statistics reads of one torn line put the same warning into the
    /// page's list twice, and the one app-wide printer prints it once.
    #[cfg(feature = "gui")]
    #[test]
    fn two_statistics_reads_print_a_torn_line_once() {
        use std::io::Write;

        let dir = tempfile::tempdir().expect("a temp folder");
        let tiny = fixture::write(dir.path()).expect("the tiny fixture");
        let events = tiny
            .history
            .root()
            .join("atlas")
            .join("events-2026-11.jsonl");
        let mut file = std::fs::OpenOptions::new()
            .append(true)
            .open(&events)
            .expect("the events file");
        file.write_all(b"{\"t\":").expect("the torn last line");
        drop(file);

        let app = AppPaths::at(dir.path());
        let parts = stats_ctx_parts(&app, &tiny.history).expect("the statistics parts");
        let query = Query {
            range: RangeKey::All,
            accounts: Vec::new(),
            host: None,
        };
        let printed = std::sync::Mutex::new(crate::gui::worker::PrintedWarnings::new());

        for _ in 0..2 {
            stats_overview_at(&parts, &tiny.history, tiny.now, tiny.offset, &query)
                .expect("the overview");
            let warnings = parts.take_warnings();
            assert!(
                warnings
                    .iter()
                    .any(|warning| warning.contains("events-2026-11.jsonl line")),
                "the statistics read kept no warning: {warnings:?}"
            );
            crate::gui::lock(&printed).print(Some("history"), &warnings);
        }

        assert_eq!(
            crate::gui::lock(&printed).len(),
            1,
            "the same warning text is printed once for both reads"
        );
    }

    #[cfg(feature = "gui")]
    #[test]
    fn each_command_answers_for_the_tiny_history() {
        let dir = tempfile::tempdir().expect("a temp folder");
        let tiny = fixture::write(dir.path()).expect("the tiny fixture");
        let app = AppPaths::at(dir.path());
        let parts = stats_ctx_parts(&app, &tiny.history).expect("the statistics parts");
        let query = Query {
            range: RangeKey::All,
            accounts: Vec::new(),
            host: None,
        };
        let now = tiny.now;
        let offset = tiny.offset;

        let frame = stats_frame_at(&tiny.history).expect("the frame");
        assert_eq!(frame.hosts, vec!["atlas".to_string()]);
        assert_eq!(
            frame.history_start,
            Some("2026-11-07T12:00:00Z".parse().expect("the first switch"))
        );
        assert!(frame.history_bytes > 0, "the fixture history has files");

        let overview =
            stats_overview_at(&parts, &tiny.history, now, offset, &query).expect("the overview");
        match overview {
            Page::Data(overview) => {
                // The history counts the two finished weeks' 120 EUR and the
                // running windows' use so far (LB 30 %, AC 40 %, 25.38 EUR);
                // the card still shows the two windows.
                assert!(
                    (overview.kpis.used - 145.3846).abs() < 0.001,
                    "used {}",
                    overview.kpis.used
                );
            }
            Page::Empty(empty) => panic!("the tiny history is not empty: {empty:?}"),
        }
        let card =
            stats_this_week_at(&parts, &tiny.history, now, offset, &query).expect("the card");
        let running: Vec<f64> = card.accounts.iter().map(|account| account.used).collect();
        assert_eq!(running, vec![30.0, 40.0]);

        assert!(stats_weeks_at(&parts, &tiny.history, now, offset, &query).is_ok());
        assert!(stats_week_summary_at(
            &parts,
            &tiny.history,
            now,
            offset,
            &query,
            "2026-11-09T00:00:00Z".parse().expect("a week")
        )
        .is_ok());
        assert!(stats_window_detail_at(
            &parts,
            &tiny.history,
            now,
            offset,
            &query,
            &tiny.accounts[0].id,
            "2026-11-09T17:00:00Z".parse().expect("a window end")
        )
        .is_ok());
        assert!(stats_projects_at(&parts, &tiny.history, now, offset, &query).is_ok());
        assert!(stats_project_detail_at(
            &parts,
            &tiny.history,
            now,
            offset,
            &query,
            &ProjectKey::Repo("github.com/sam/atlas".to_string())
        )
        .is_ok());
        assert!(stats_subscriptions_at(&parts, &tiny.history, now, offset, &query).is_ok());
        assert!(stats_rhythm_at(&parts, &tiny.history, now, offset, &query).is_ok());
        assert!(stats_activity_at(&parts, &tiny.history, now, offset, &query).is_ok());

        let inputs = Inputs {
            now,
            offset,
            accounts: vec![
                AccountState {
                    id: tiny.accounts[0].id.clone(),
                    may_use: true,
                    login_ok: true,
                    week_percent: 30.0,
                    week_resets_at: "2026-11-23T17:00:00Z".parse().expect("a reset"),
                    session: Some(SessionState {
                        percent: 0.0,
                        resets_at: None,
                    }),
                    model_blocked: false,
                    rates: Some(Rates::given(4.0, Some(80.0))),
                    thinking_held: 0,
                },
                AccountState {
                    id: tiny.accounts[1].id.clone(),
                    may_use: true,
                    login_ok: true,
                    week_percent: 40.0,
                    week_resets_at: "2026-11-23T02:00:00Z".parse().expect("a reset"),
                    session: Some(SessionState {
                        percent: 0.0,
                        resets_at: None,
                    }),
                    model_blocked: false,
                    rates: Some(Rates::given(4.0, Some(100.0))),
                    thinking_held: 0,
                },
            ],
            active: Some(tiny.accounts[0].id.clone()),
            rhythm: Rhythm { usd: [1.0; 168] },
            last_switch_at: None,
            paused_until: None,
        };
        let regime = Regime {
            early: false,
            why: RegimeWhy::NoCostYet,
            lazy_blocked_h: 0.0,
            eager_blocked_h: 0.0,
            extra_switches: 0,
            cost_h: None,
        };
        let forecast = stats_forecast_at(
            &parts,
            &tiny.history,
            now,
            offset,
            Some((inputs, regime, now)),
            NO_PLAN.to_string(),
        )
        .expect("the forecast");
        assert!(matches!(forecast, Page::Data(_)), "{forecast:?}");

        let error = stats_forecast_at(
            &parts,
            &tiny.history,
            now,
            offset,
            None,
            NO_PLAN.to_string(),
        )
        .expect_err("no reading, no forecast");
        assert_eq!(error, "the forecast needs a reading of every account first");
    }

    /// Without a plan the Forecast page names the accounts auto-burn waits a
    /// first reading of, by their abbreviations, and says when it comes.
    /// Without such an account it keeps the one plain sentence.
    #[cfg(feature = "gui")]
    #[test]
    fn no_plan_names_the_accounts_without_a_reading() {
        use super::super::burn_rt::BurnView;
        use seatswap_core::engine::AccountView;
        use seatswap_core::store::LoginState;

        let view = |id: &str, abbr: &str| AccountView {
            id: id.to_string(),
            label: abbr.to_string(),
            abbr: abbr.to_string(),
            color: "#3b82f6".to_string(),
            email: None,
            plan: None,
            auto_burn: true,
            is_active: false,
            login_state: LoginState::Ok,
            login_ends_at_ms: None,
            login_days_left: None,
            usage: None,
            usable: true,
            blocked_until: None,
            blocker: None,
            fetched_at: None,
            last_error: None,
        };
        let ac = "3f2b8c1e9d4a4e7b8c6d5e4f3a2b1c0d";
        let pr = "9e8d7c6b5a4f4e3d2c1b0a9f8e7d6c5b";
        let views = vec![view(ac, "AC"), view(pr, "PR")];

        assert_eq!(no_plan_text(None, &views).expect("a text"), NO_PLAN);
        assert_eq!(
            no_plan_text(Some(&BurnView::default()), &views).expect("a text"),
            NO_PLAN
        );

        let one = BurnView {
            unread: vec![pr.to_string()],
            ..BurnView::default()
        };
        assert_eq!(
            no_plan_text(Some(&one), &views).expect("a text"),
            "the forecast needs a reading of every account first: PR has none yet; \
             it comes with the next usage poll"
        );

        let two = BurnView {
            unread: vec![ac.to_string(), pr.to_string()],
            ..BurnView::default()
        };
        assert_eq!(
            no_plan_text(Some(&two), &views).expect("a text"),
            "the forecast needs a reading of every account first: AC, PR have none yet; \
             they come with the next usage poll"
        );

        let gone = BurnView {
            unread: vec!["0a1b2c3d4e5f4a6b8c7d9e0f1a2b3c4d".to_string()],
            ..BurnView::default()
        };
        assert!(
            no_plan_text(Some(&gone), &views).is_err(),
            "an account without a view is an error, never a guessed name"
        );
    }

    #[cfg(feature = "gui")]
    #[test]
    fn the_ctx_names_a_broken_prices_file() {
        let dir = tempfile::tempdir().expect("a temp folder");
        let tiny = fixture::write(dir.path()).expect("the tiny fixture");
        let app = AppPaths::at(dir.path());
        std::fs::write(app.prices_file(), "{not json").expect("the broken prices");

        let error = stats_ctx_parts(&app, &tiny.history).expect_err("the prices cannot be read");
        assert!(error.starts_with("the prices cannot be read"), "{error}");
    }

    // ---- proof tests of the review ----

    /// The fixture must stay the fixture: a second `fixture::write` into the
    /// same folder is refused, because the first write is already there. The
    /// pages then count the history once.
    #[cfg(feature = "gui")]
    #[test]
    fn the_fixture_written_twice_counts_the_history_once() {
        let dir = tempfile::tempdir().expect("a temp folder");
        let tiny = fixture::write(dir.path()).expect("the tiny fixture");
        let app = AppPaths::at(dir.path());
        let parts = stats_ctx_parts(&app, &tiny.history).expect("the statistics parts");
        let query = Query {
            range: RangeKey::All,
            accounts: Vec::new(),
            host: None,
        };

        // The second write into the same folder must not add a second copy of
        // every record: it is refused.
        let error = fixture::write(dir.path())
            .err()
            .expect("the second fixture is refused");
        assert!(error.ends_with("is not empty"), "{error}");

        let overview = stats_overview_at(&parts, &tiny.history, tiny.now, tiny.offset, &query)
            .expect("the overview");
        let Page::Data(overview) = overview else {
            panic!("the tiny history is not empty");
        };
        assert_eq!(
            overview.kpis.switches.user, 1,
            "the switches are counted twice"
        );
    }

    /// The forecast is the plan of the reading it was built from. A reading
    /// older than the freshness the burn runtime itself uses (`FRESH_MIN`) is
    /// refused like no reading at all: its seven days would lie in the past.
    #[cfg(feature = "gui")]
    #[test]
    fn the_forecast_refuses_a_reading_that_is_too_old() {
        let dir = tempfile::tempdir().expect("a temp folder");
        let tiny = fixture::write(dir.path()).expect("the tiny fixture");
        let app = AppPaths::at(dir.path());
        let parts = stats_ctx_parts(&app, &tiny.history).expect("the statistics parts");
        let now = tiny.now;
        let offset = tiny.offset;
        let regime = Regime {
            early: false,
            why: RegimeWhy::NoCostYet,
            lazy_blocked_h: 0.0,
            eager_blocked_h: 0.0,
            extra_switches: 0,
            cost_h: None,
        };
        let state = |id: String, percent: f64, resets_at: &str, s: f64| AccountState {
            id,
            may_use: true,
            login_ok: true,
            week_percent: percent,
            week_resets_at: resets_at.parse().expect("a reset"),
            session: Some(SessionState {
                percent: 0.0,
                resets_at: None,
            }),
            model_blocked: false,
            rates: Some(Rates::given(4.0, Some(s))),
            thinking_held: 0,
        };
        let inputs = |at: DateTime<Utc>| Inputs {
            now: at,
            offset,
            accounts: vec![
                state(
                    tiny.accounts[0].id.clone(),
                    30.0,
                    "2026-11-23T17:00:00Z",
                    80.0,
                ),
                state(
                    tiny.accounts[1].id.clone(),
                    40.0,
                    "2026-11-23T02:00:00Z",
                    100.0,
                ),
            ],
            active: Some(tiny.accounts[0].id.clone()),
            rhythm: Rhythm { usd: [1.0; 168] },
            last_switch_at: None,
            paused_until: None,
        };

        // A reading of this moment is a plan.
        let fresh = stats_forecast_at(
            &parts,
            &tiny.history,
            now,
            offset,
            Some((inputs(now), regime.clone(), now)),
            NO_PLAN.to_string(),
        )
        .expect("the forecast of a fresh reading");
        assert!(matches!(fresh, Page::Data(_)), "{fresh:?}");

        // A reading of half an hour ago is not: it is older than the
        // freshness the burn runtime itself uses.
        let old = now - Duration::minutes(seatswap_core::burn::FRESH_MIN + 10);
        let error = stats_forecast_at(
            &parts,
            &tiny.history,
            now,
            offset,
            Some((inputs(old), regime.clone(), old)),
            NO_PLAN.to_string(),
        )
        .expect_err("a reading that is too old is no plan");
        assert_eq!(
            error, "the forecast needs a reading of every account first",
            "{error}"
        );
    }

    /// The plan's own tick time is not the age of its readings: a tick one
    /// minute ago can still be built on readings two hours old when the usage
    /// fetch failed and only a cached regime kept the plan alive.
    #[cfg(feature = "gui")]
    #[test]
    fn the_forecast_refuses_readings_that_are_too_old() {
        let dir = tempfile::tempdir().expect("a temp folder");
        let tiny = fixture::write(dir.path()).expect("the tiny fixture");
        let app = AppPaths::at(dir.path());
        let parts = stats_ctx_parts(&app, &tiny.history).expect("the statistics parts");
        let now = tiny.now;
        let offset = tiny.offset;
        let regime = Regime {
            early: false,
            why: RegimeWhy::NoCostYet,
            lazy_blocked_h: 0.0,
            eager_blocked_h: 0.0,
            extra_switches: 0,
            cost_h: None,
        };
        let inputs = Inputs {
            now: now - Duration::minutes(1),
            offset,
            accounts: vec![AccountState {
                id: tiny.accounts[0].id.clone(),
                may_use: true,
                login_ok: true,
                week_percent: 30.0,
                week_resets_at: "2026-11-23T17:00:00Z".parse().expect("a reset"),
                session: Some(SessionState {
                    percent: 0.0,
                    resets_at: None,
                }),
                model_blocked: false,
                rates: Some(Rates::given(4.0, Some(80.0))),
                thinking_held: 0,
            }],
            active: Some(tiny.accounts[0].id.clone()),
            rhythm: Rhythm { usd: [1.0; 168] },
            last_switch_at: None,
            paused_until: None,
        };

        let oldest = now - Duration::hours(2);
        let error = stats_forecast_at(
            &parts,
            &tiny.history,
            now,
            offset,
            Some((inputs, regime, oldest)),
            NO_PLAN.to_string(),
        )
        .expect_err("readings two hours old are no plan");
        assert_eq!(
            error, "the forecast needs a reading of every account first",
            "{error}"
        );
    }

    #[cfg(feature = "gui")]
    #[test]
    fn stats_what_if_is_gone() {
        let commands = include_str!("mod.rs");
        assert!(
            !commands.contains("stats_what_if"),
            "the what-if command is gone"
        );
    }

    /// The This-week card ignores the range: on the accounts' first day, last
    /// week holds no record and the Overview says "nothing recorded yet", but
    /// the card still lists the two weekly windows running now.
    #[cfg(feature = "gui")]
    #[test]
    fn an_empty_range_still_has_the_this_week_card() {
        let dir = tempfile::tempdir().expect("a temp folder");
        let tiny = fixture::write(dir.path()).expect("the tiny fixture");
        // A history of today's usage readings only: the fixture's two.
        let readings = tiny
            .history
            .read_claude::<seatswap_core::history::Sample>(
                &seatswap_core::history::Range::ALL,
                &seatswap_core::history::Hosts::All,
            )
            .expect("the fixture's readings");
        let today: Vec<_> = readings
            .records
            .into_iter()
            .map(|hosted| hosted.record)
            .filter(|sample| sample.t > tiny.now - Duration::days(1))
            .collect();
        assert_eq!(today.len(), 2, "LB's and AC's reading of today");
        let first_day = tempfile::tempdir().expect("a second temp folder");
        let history = HistoryDir::at(first_day.path(), "atlas");
        history.append_many(&today).expect("today's readings");

        let app = AppPaths::at(dir.path());
        let parts = stats_ctx_parts(&app, &history).expect("the statistics parts");
        let query = Query {
            range: RangeKey::LastWeek,
            accounts: Vec::new(),
            host: None,
        };
        let overview = stats_overview_at(&parts, &history, tiny.now, tiny.offset, &query)
            .expect("the overview command");
        assert!(
            matches!(overview, Page::Empty(_)),
            "last week holds no record: {overview:?}"
        );
        let card = stats_this_week_at(&parts, &history, tiny.now, tiny.offset, &query)
            .expect("the card command");
        let running: Vec<(&str, f64)> = card
            .accounts
            .iter()
            .map(|account| (account.id.as_str(), account.used))
            .collect();
        assert_eq!(
            running,
            vec![
                (tiny.accounts[0].id.as_str(), 30.0),
                (tiny.accounts[1].id.as_str(), 40.0)
            ]
        );
    }

    /// A range with no record says "nothing recorded yet" on every page, not
    /// only in the core: every command that takes a query builds its range and
    /// asks `stats::empty` before it computes.
    #[cfg(feature = "gui")]
    #[test]
    fn every_page_but_the_forecast_says_nothing_recorded_for_an_empty_range() {
        fn assert_empty<T: std::fmt::Debug>(page: Page<T>) {
            match page {
                Page::Empty(_) => {}
                Page::Data(data) => panic!("the empty range has data: {data:?}"),
            }
        }

        let dir = tempfile::tempdir().expect("a temp folder");
        let tiny = fixture::write(dir.path()).expect("the tiny fixture");
        let app = AppPaths::at(dir.path());
        let parts = stats_ctx_parts(&app, &tiny.history).expect("the statistics parts");
        let now = "2026-12-10T10:00:00Z".parse().expect("a later month");
        let offset = tiny.offset;
        let query = Query {
            range: RangeKey::ThisWeek,
            accounts: Vec::new(),
            host: None,
        };

        assert_empty(
            stats_overview_at(&parts, &tiny.history, now, offset, &query)
                .expect("the overview command"),
        );
        assert_empty(
            stats_weeks_at(&parts, &tiny.history, now, offset, &query).expect("the weeks command"),
        );
        assert_empty(
            stats_week_summary_at(
                &parts,
                &tiny.history,
                now,
                offset,
                &query,
                "2026-12-07T00:00:00Z".parse().expect("this week"),
            )
            .expect("the week summary command"),
        );
        assert_empty(
            stats_window_detail_at(
                &parts,
                &tiny.history,
                now,
                offset,
                &query,
                &tiny.accounts[0].id,
                "2026-11-09T17:00:00Z".parse().expect("a window end"),
            )
            .expect("the window detail command"),
        );
        assert_empty(
            stats_projects_at(&parts, &tiny.history, now, offset, &query)
                .expect("the projects command"),
        );
        assert_empty(
            stats_project_detail_at(
                &parts,
                &tiny.history,
                now,
                offset,
                &query,
                &ProjectKey::Repo("github.com/sam/atlas".to_string()),
            )
            .expect("the project detail command"),
        );
        assert_empty(
            stats_subscriptions_at(&parts, &tiny.history, now, offset, &query)
                .expect("the subscriptions command"),
        );
        assert_empty(
            stats_rhythm_at(&parts, &tiny.history, now, offset, &query)
                .expect("the rhythm command"),
        );
        assert_empty(
            stats_activity_at(&parts, &tiny.history, now, offset, &query)
                .expect("the activity command"),
        );
    }

    #[cfg(feature = "gui")]
    #[test]
    fn the_preview_takes_its_auto_burn_from_burn_next_json() {
        use seatswap_core::burn::burn_next;
        use seatswap_core::engine::AccountView;
        use seatswap_core::store::LoginState;

        use super::super::burn_rt::BurnView;
        use super::super::worker::Snapshot;

        const AC_ID: &str = "0f1e2d3c4b5a69788796a5b4c3d2e1f0";
        let now = DateTime::<Utc>::from_timestamp(1_794_473_100, 0).expect("a test time");
        let switch_at = now + Duration::hours(2);
        let tmp = tempfile::tempdir().expect("tempdir");
        let app = AppPaths::at(&tmp.path().join("data"));

        let mut snapshot = Snapshot::empty(None, None, None);
        snapshot.views.push(AccountView {
            id: AC_ID.to_string(),
            label: "AC team".to_string(),
            abbr: "AC".to_string(),
            color: "#3b82f6".to_string(),
            email: None,
            plan: Some("Team".to_string()),
            auto_burn: true,
            is_active: false,
            login_state: LoginState::Ok,
            login_ends_at_ms: None,
            login_days_left: None,
            usage: None,
            usable: true,
            blocked_until: None,
            blocker: None,
            fetched_at: None,
            last_error: None,
        });
        let mut burn = BurnView::default();
        burn.planned.insert(AC_ID.to_string(), switch_at);
        snapshot.burn = Some(burn);

        let source = SnapshotFacts {
            snapshot: &snapshot,
            app: &app,
        };
        assert_eq!(
            source.burn_next(now),
            Ok(None),
            "no burn-next.json, so no auto-burn value even with a filled plan"
        );

        burn_next::write(
            &app,
            &burn_next::BurnNext {
                account: AC_ID.to_string(),
                at: switch_at,
                written: now,
            },
        )
        .expect("burn-next.json");
        assert_eq!(
            source.burn_next(now),
            Ok(Some(("AC".to_string(), switch_at))),
            "the preview shows the line's next account and time"
        );
    }

    // ---- turning the status line on in Claude Code (spec §5.4) ----

    /// The command sends the store's whole table, in the palette's order.
    #[cfg(feature = "gui")]
    #[test]
    fn statusline_kinds_are_the_table_in_palette_order() {
        let expected: Vec<KindSpec> = SegmentKind::ALL.iter().map(|kind| kind.spec()).collect();
        assert_eq!(statusline_kinds(), expected);
    }

    #[cfg(feature = "gui")]
    fn statusline_paths(tmp: &tempfile::TempDir) -> (ClaudePaths, PathBuf) {
        let config = tmp.path().join("claude");
        std::fs::create_dir(&config).expect("the config folder");
        let exe = tmp.path().join(if cfg!(windows) {
            "seatswap.exe"
        } else {
            "seatswap"
        });
        std::fs::write(&exe, b"MZ").expect("the console program beside the running one");
        (ClaudePaths::for_config_dir(&config, tmp.path()), exe)
    }

    #[cfg(feature = "gui")]
    fn write_claude_settings(claude: &ClaudePaths, value: &serde_json::Value) {
        std::fs::write(
            claude.settings_file(),
            serde_json::to_vec(value).expect("JSON"),
        )
        .expect("Claude Code's settings file");
    }

    /// The record the fake `save_setting` holds in an in-memory `Settings`.
    #[cfg(feature = "gui")]
    type FakeRecord = std::rc::Rc<std::cell::RefCell<Option<StatuslineInstall>>>;

    #[cfg(feature = "gui")]
    fn fake_record() -> FakeRecord {
        std::rc::Rc::new(std::cell::RefCell::new(None))
    }

    /// The install command's fake `save_setting`: the record is stored where
    /// `save_setting` would have stored it.
    #[cfg(feature = "gui")]
    fn save_into(
        record: FakeRecord,
    ) -> impl FnOnce(StatuslineInstall) -> std::future::Ready<Result<(), String>> {
        move |saved| {
            *record.borrow_mut() = Some(saved);
            std::future::ready(Ok::<(), String>(()))
        }
    }

    /// The restore command's fake `save_setting`: clearing the record stores
    /// `None`, as `save_setting` does.
    #[cfg(feature = "gui")]
    fn clear_into(record: FakeRecord) -> impl FnOnce() -> std::future::Ready<Result<(), String>> {
        move || {
            *record.borrow_mut() = None;
            std::future::ready(Ok::<(), String>(()))
        }
    }

    #[cfg(feature = "gui")]
    #[test]
    fn the_install_state_names_claude_codes_settings_file() {
        let tmp = tempfile::tempdir().expect("tempdir");
        let (claude, exe) = statusline_paths(&tmp);
        let reading = read_status_lines(&claude, &exe).expect("the reading");
        let state = install_state_of(&reading, None);
        assert_eq!(
            state.settings_file,
            claude.settings_file().display().to_string()
        );
        assert!(!state.installed);
        assert!(!state.restorable);
    }

    #[cfg(feature = "gui")]
    #[test]
    fn restore_puts_the_previous_value_back() {
        let tmp = tempfile::tempdir().expect("tempdir");
        let (claude, exe) = statusline_paths(&tmp);
        let old = serde_json::json!({"type": "command", "command": "bash ~/x.sh"});
        write_claude_settings(
            &claude,
            &serde_json::json!({"model": "opus", "statusLine": old}),
        );
        let ours = claude_settings::seatswap_status_line(&exe).expect("the status line");
        let mut settings = Settings::default();

        let saved = fake_record();
        let installed = tauri::async_runtime::block_on(install_status_line(
            &claude,
            &exe,
            settings.statusline_install.clone(),
            save_into(saved.clone()),
        ))
        .expect("install seatswap's line");
        settings.statusline_install = saved.borrow().clone();
        assert!(installed.installed);
        assert!(installed.restorable);
        assert_eq!(installed.current.as_ref(), Some(&ours));
        assert_eq!(
            claude_settings::status_line(&claude).expect("read the line"),
            Some(ours.clone())
        );
        assert_eq!(
            settings
                .statusline_install
                .as_ref()
                .and_then(|record| record.previous.clone()),
            Some(old.clone()),
            "the line that was there is kept"
        );

        // A second install keeps the first previous value, not seatswap's own.
        let saved = fake_record();
        *saved.borrow_mut() = settings.statusline_install.clone();
        let again = tauri::async_runtime::block_on(install_status_line(
            &claude,
            &exe,
            settings.statusline_install.clone(),
            save_into(saved.clone()),
        ))
        .expect("install again");
        settings.statusline_install = saved.borrow().clone();
        assert!(again.installed);
        assert_eq!(
            settings
                .statusline_install
                .as_ref()
                .and_then(|record| record.previous.clone()),
            Some(old.clone())
        );

        let cleared = fake_record();
        let restored = tauri::async_runtime::block_on(restore_status_line(
            &claude,
            &exe,
            settings.statusline_install.clone(),
            clear_into(cleared.clone()),
        ))
        .expect("restore");
        settings.statusline_install = cleared.borrow().clone();
        assert!(!restored.installed);
        assert!(!restored.restorable);
        assert_eq!(restored.current, Some(old.clone()));
        assert_eq!(
            claude_settings::status_line(&claude).expect("read the restored line"),
            Some(old.clone())
        );
        assert_eq!(settings.statusline_install, None);
    }

    #[cfg(feature = "gui")]
    #[test]
    fn restore_without_a_previous_value_removes_the_key() {
        let tmp = tempfile::tempdir().expect("tempdir");
        let (claude, exe) = statusline_paths(&tmp);
        write_claude_settings(&claude, &serde_json::json!({"model": "opus"}));
        let mut settings = Settings::default();

        let saved = fake_record();
        let installed = tauri::async_runtime::block_on(install_status_line(
            &claude,
            &exe,
            settings.statusline_install.clone(),
            save_into(saved.clone()),
        ))
        .expect("install over no status line");
        settings.statusline_install = saved.borrow().clone();
        assert!(installed.installed);
        assert_eq!(
            settings
                .statusline_install
                .as_ref()
                .and_then(|record| record.previous.clone()),
            None,
            "the file had no statusLine key"
        );

        let cleared = fake_record();
        let restored = tauri::async_runtime::block_on(restore_status_line(
            &claude,
            &exe,
            settings.statusline_install.clone(),
            clear_into(cleared.clone()),
        ))
        .expect("restore");
        settings.statusline_install = cleared.borrow().clone();
        assert!(!restored.installed);
        assert_eq!(restored.current, None);
        assert_eq!(
            claude_settings::status_line(&claude).expect("read the settings"),
            None
        );
        let file: serde_json::Value =
            serde_json::from_slice(&std::fs::read(claude.settings_file()).expect("read"))
                .expect("JSON");
        assert_eq!(file, serde_json::json!({"model": "opus"}));
        assert_eq!(settings.statusline_install, None);
    }

    #[cfg(feature = "gui")]
    #[test]
    fn restore_refuses_a_line_that_is_not_ours() {
        let tmp = tempfile::tempdir().expect("tempdir");
        let (claude, exe) = statusline_paths(&tmp);
        let hand_made = serde_json::json!({"type": "command", "command": "bash ~/later.sh"});
        write_claude_settings(&claude, &serde_json::json!({"statusLine": hand_made}));
        let original = std::fs::read(claude.settings_file()).expect("the file");
        let record = StatuslineInstall {
            installed: serde_json::json!({
                "type": "command",
                "command": "\"old/seatswap\" statusline",
            }),
            previous: Some(serde_json::json!({"type": "command", "command": "bash ~/x.sh"})),
        };

        let error = tauri::async_runtime::block_on(restore_status_line(
            &claude,
            &exe,
            Some(record),
            || std::future::ready(Ok::<(), String>(())),
        ))
        .expect_err("a line the user set afterwards is not restored over");

        assert_eq!(
            error,
            "the status line in Claude Code is not seatswap's: nothing to restore"
        );
        assert_eq!(
            std::fs::read(claude.settings_file()).expect("the file"),
            original,
            "the refused file is untouched"
        );
        assert_eq!(
            claude_settings::status_line(&claude).expect("read"),
            Some(hand_made)
        );
    }

    #[cfg(feature = "gui")]
    #[test]
    fn install_twice_keeps_the_first_previous() {
        let tmp = tempfile::tempdir().expect("tempdir");
        let (claude, exe) = statusline_paths(&tmp);
        let old = serde_json::json!({"type": "command", "command": "bash ~/x.sh"});
        write_claude_settings(&claude, &serde_json::json!({"statusLine": old}));
        let mut settings = Settings::default();

        let saved = fake_record();
        tauri::async_runtime::block_on(install_status_line(
            &claude,
            &exe,
            settings.statusline_install.clone(),
            save_into(saved.clone()),
        ))
        .expect("first install");
        settings.statusline_install = saved.borrow().clone();
        let saved = fake_record();
        *saved.borrow_mut() = settings.statusline_install.clone();
        tauri::async_runtime::block_on(install_status_line(
            &claude,
            &exe,
            settings.statusline_install.clone(),
            save_into(saved.clone()),
        ))
        .expect("second install");
        settings.statusline_install = saved.borrow().clone();

        assert_eq!(
            settings
                .statusline_install
                .as_ref()
                .and_then(|record| record.previous.clone()),
            Some(old)
        );
    }

    #[cfg(feature = "gui")]
    #[test]
    fn restore_without_a_record_is_refused_and_the_file_is_untouched() {
        let tmp = tempfile::tempdir().expect("tempdir");
        let (claude, exe) = statusline_paths(&tmp);
        let ours = claude_settings::seatswap_status_line(&exe).expect("the status line");
        write_claude_settings(
            &claude,
            &serde_json::json!({"model": "opus", "statusLine": ours}),
        );
        let original = std::fs::read(claude.settings_file()).expect("the file");

        let error =
            tauri::async_runtime::block_on(restore_status_line(&claude, &exe, None, || {
                std::future::ready(Ok::<(), String>(()))
            }))
            .expect_err("without a record there is nothing to restore");

        assert_eq!(
            error,
            "seatswap has no record of the line before its own: nothing to restore"
        );
        assert_eq!(
            std::fs::read(claude.settings_file()).expect("the file"),
            original,
            "the refused file is untouched"
        );
    }

    #[cfg(feature = "gui")]
    #[test]
    fn install_over_a_hand_line_after_an_install_stores_that_line_as_previous() {
        let tmp = tempfile::tempdir().expect("tempdir");
        let (claude, exe) = statusline_paths(&tmp);
        let old = serde_json::json!({"type": "command", "command": "bash ~/x.sh"});
        write_claude_settings(&claude, &serde_json::json!({"statusLine": old.clone()}));
        let mut settings = Settings::default();
        let saved = fake_record();
        tauri::async_runtime::block_on(install_status_line(
            &claude,
            &exe,
            settings.statusline_install.clone(),
            save_into(saved.clone()),
        ))
        .expect("first install");
        settings.statusline_install = saved.borrow().clone();

        // The user sets a line by hand after seatswap's install.
        let hand = serde_json::json!({"type": "command", "command": "bash ~/mine.sh"});
        write_claude_settings(&claude, &serde_json::json!({"statusLine": hand.clone()}));
        let saved = fake_record();
        *saved.borrow_mut() = settings.statusline_install.clone();
        let installed = tauri::async_runtime::block_on(install_status_line(
            &claude,
            &exe,
            settings.statusline_install.clone(),
            save_into(saved.clone()),
        ))
        .expect("install over the hand line");
        settings.statusline_install = saved.borrow().clone();

        assert!(installed.installed);
        assert_eq!(
            settings
                .statusline_install
                .as_ref()
                .and_then(|record| record.previous.clone()),
            Some(hand),
            "the hand line, not seatswap's older line, is the previous value"
        );
        assert_eq!(
            settings
                .statusline_install
                .as_ref()
                .map(|record| record.installed.clone()),
            Some(claude_settings::seatswap_status_line(&exe).expect("the status line"))
        );
    }

    #[cfg(feature = "gui")]
    #[test]
    fn install_saves_the_record_before_it_writes() {
        let tmp = tempfile::tempdir().expect("tempdir");
        let (claude, exe) = statusline_paths(&tmp);
        let old = serde_json::json!({"type": "command", "command": "bash ~/x.sh"});
        write_claude_settings(
            &claude,
            &serde_json::json!({"model": "opus", "statusLine": old.clone()}),
        );
        let original = std::fs::read(claude.settings_file()).expect("the file");

        let error =
            tauri::async_runtime::block_on(install_status_line(&claude, &exe, None, |_record| {
                std::future::ready(Err::<(), String>(
                    "accounts.json: the install record cannot be saved".to_string(),
                ))
            }))
            .expect_err("a refused save stops the install");

        assert_eq!(error, "accounts.json: the install record cannot be saved");
        assert_eq!(
            std::fs::read(claude.settings_file()).expect("the file"),
            original,
            "the record is saved first, so a refused save leaves Claude Code's file alone"
        );
        assert_eq!(
            claude_settings::status_line(&claude).expect("read"),
            Some(old)
        );
    }

    #[cfg(feature = "gui")]
    #[test]
    fn restore_puts_the_line_back_when_the_record_cannot_be_cleared() {
        let tmp = tempfile::tempdir().expect("tempdir");
        let (claude, exe) = statusline_paths(&tmp);
        let old = serde_json::json!({"type": "command", "command": "bash ~/x.sh"});
        write_claude_settings(&claude, &serde_json::json!({"statusLine": old}));
        let saved = fake_record();
        tauri::async_runtime::block_on(install_status_line(
            &claude,
            &exe,
            None,
            save_into(saved.clone()),
        ))
        .expect("install");
        let ours = claude_settings::seatswap_status_line(&exe).expect("the status line");

        let error = tauri::async_runtime::block_on(restore_status_line(
            &claude,
            &exe,
            saved.borrow().clone(),
            || {
                std::future::ready(Err::<(), String>(
                    "accounts.json: the install record cannot be cleared".to_string(),
                ))
            },
        ))
        .expect_err("a refused clear stops the restore");

        assert_eq!(error, "accounts.json: the install record cannot be cleared");
        assert_eq!(
            claude_settings::status_line(&claude).expect("read"),
            Some(ours),
            "the line seatswap had installed is put back after the failed clear"
        );
    }

    // ---- proof tests of the review ----

    #[cfg(feature = "gui")]
    #[test]
    fn proof_install_from_a_moved_binary_keeps_the_first_previous() {
        let tmp = tempfile::tempdir().expect("tempdir");
        let (claude, exe) = statusline_paths(&tmp);
        let moved_folder = tmp.path().join("moved");
        std::fs::create_dir(&moved_folder).expect("folder");
        let moved = moved_folder.join(exe.file_name().expect("name"));
        std::fs::write(&moved, b"MZ").expect("program");
        let old = serde_json::json!({"type": "command", "command": "bash ~/x.sh"});
        write_claude_settings(&claude, &serde_json::json!({"statusLine": old}));

        let saved = fake_record();
        tauri::async_runtime::block_on(install_status_line(
            &claude,
            &exe,
            None,
            save_into(saved.clone()),
        ))
        .expect("first install");
        let record = saved.borrow().clone();
        assert_eq!(
            record.as_ref().and_then(|record| record.previous.clone()),
            Some(old.clone())
        );
        // The program is installed somewhere else; the user presses the button again.
        let saved = fake_record();
        *saved.borrow_mut() = record.clone();
        tauri::async_runtime::block_on(install_status_line(
            &claude,
            &moved,
            record,
            save_into(saved.clone()),
        ))
        .expect("second install");
        let record = saved.borrow().clone();

        assert_eq!(
            record.as_ref().and_then(|record| record.previous.clone()),
            Some(old),
            "the user's own line must not be lost"
        );
    }

    #[cfg(feature = "gui")]
    #[test]
    fn proof_a_null_status_line_survives_install_and_restore() {
        let tmp = tempfile::tempdir().expect("tempdir");
        let (claude, exe) = statusline_paths(&tmp);
        write_claude_settings(&claude, &serde_json::json!({"statusLine": null}));
        let original = std::fs::read(claude.settings_file()).expect("file");
        let saved = fake_record();
        tauri::async_runtime::block_on(install_status_line(
            &claude,
            &exe,
            None,
            save_into(saved.clone()),
        ))
        .expect("install");
        // The record goes through accounts.json (serde) like save_setting does.
        let settings = Settings {
            statusline_install: saved.borrow().clone(),
            ..Settings::default()
        };
        let settings: Settings =
            serde_json::from_value(serde_json::to_value(&settings).expect("to")).expect("from");
        tauri::async_runtime::block_on(restore_status_line(
            &claude,
            &exe,
            settings.statusline_install,
            || std::future::ready(Ok::<(), String>(())),
        ))
        .expect("restore");
        let after: serde_json::Value =
            serde_json::from_slice(&std::fs::read(claude.settings_file()).expect("file"))
                .expect("JSON");
        let before: serde_json::Value = serde_json::from_slice(&original).expect("JSON");
        assert_eq!(after, before, "a null statusLine comes back as it was");
    }
}
