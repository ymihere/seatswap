//! The sessions thread: Claude Code's running sessions and the transcript
//! scanner (spec §1.6).
//!
//! It runs only when `readSessions` is on. Every wake it asks which sessions are
//! alive, reads the new parts of their transcripts, writes the tokens and the
//! session records to the history, then hands a [`SessionsSummary`] to the main
//! thread and the models in use to the engine thread.

use std::cell::RefCell;
use std::collections::{BTreeMap, BTreeSet, HashMap, HashSet};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::mpsc::{self, Receiver, RecvTimeoutError, Sender};
use std::sync::{Arc, Mutex};
use std::thread::{self, JoinHandle};
use std::time::{Duration, Instant};

use chrono::{DateTime, Duration as ChronoDuration, Utc};

use seatswap_core::codex_sessions::{self, CodexSession};
use seatswap_core::feed;
use seatswap_core::harness::{self, HarnessPaths, HarnessSession};
use seatswap_core::history::{EventKind, HistoryDir, Range, Recorder, SessionRecord, TokenRecord};
use seatswap_core::live_sessions::{
    self, LiveSession, LiveStatus, OtherHost, RecordedTokens, Rows,
};
use seatswap_core::money::{is_plan_model, Fx, Prices, Valuer};
use seatswap_core::paths::{AppPaths, ClaudePaths, CodexPaths};
use seatswap_core::project::{self, ProjectKey};
use seatswap_core::projects;
use seatswap_core::sessions::{self, Liveness, SessionFile, SessionStatus, SystemProbe};
use seatswap_core::store::{self, Settings};
use seatswap_core::transcripts::{self, HarnessRecorded, RecordedSession, ScanState, SessionSeen};
use seatswap_core::watcher::{watch_folder, FolderWatch, WatchError};

use super::lookup_rt::LookupClient;
use super::window::{view_of, ClosedSince, SessionsOptions, SessionsView};
use super::worker::{EngineCommand, SessionsSummary, UiEvent, Waiting};
use crate::ops;

/// What the three sessions commands say when `readSessions` is off. The label
/// is the one the Settings page gives the toggle; a restart is needed because
/// the thread starts only with the app.
pub(crate) const SESSIONS_OFF: &str =
    "reading sessions is off: turn on \"Read Claude Code's session files\" in Settings → General and restart seatswap";

/// The regular wake, so a session folder that cannot be watched still works.
const WAKE: Duration = Duration::from_secs(60);
/// After a scan that did not finish, read the next piece soon.
const AFTER_INCOMPLETE: Duration = Duration::from_secs(1);
/// One transcript scan reads at most this much.
const SCAN_BUDGET: u64 = 64 * 1024 * 1024;
/// At most one pushed view in this window; one inside it follows when it is
/// over, so the last view always holds the final numbers (spec §3.8).
const VIEW_EVERY_MS: i64 = 500;
/// The closed rows are read again at least this often (the minute pass).
const CLOSED_EVERY_SECS: i64 = 60;
/// A closed choice only a command asked for is dropped this long after its last
/// ask, so a page that changes its mind often does not grow the kept rows.
const CLOSED_KEEP_SECS: i64 = 300;
/// A session record is rewritten at most this often while its summary changes.
const RECORD_EVERY_SECS: i64 = 10 * 60;
/// The projects, values and settings the rows are built with are at most this
/// old.
const ROWS_CONTEXT_MAX_AGE_SECS: i64 = 300;
/// A part of the rows context that could not be built is tried again after
/// this many seconds, not on every pass.
const ROWS_CONTEXT_RETRY_SECS: i64 = 60;
/// This host reports even when nothing changed after this many seconds, so
/// another host does not take a quiet host for a stale one.
const REPORT_HEARTBEAT_SECS: i64 = 300;
/// A Harness or Codex session whose file was not written for this long is not
/// part of the live rows: a session that works writes its file while it works,
/// so one that has been quiet for a day is no longer shown. This window does
/// not drop a recorded Harness token total (a total goes only when its session
/// file leaves the store), and a closed Claude session's job links are kept this
/// long after that session's last answer. One window for both stores, so the
/// two reads never drift apart.
const HARNESS_SINCE_HOURS: i64 = 24;

/// What one pass found, for the thread and the tests.
#[derive(Debug, Clone)]
pub(crate) struct PassOutcome {
    pub summary: SessionsSummary,
    pub models: Option<BTreeSet<String>>,
    pub tied_busy: u32,
    /// The running tied sessions' answers with thinking, each session's list one
    /// after the other.
    pub thinking: Vec<(DateTime<Utc>, u32)>,
    pub wake_soon: bool,
}

/// What the window sends to the sessions thread. `Wake` is the folder
/// watchers' plain wake; the other commands are the three Tauri commands.
enum SessionsCommand {
    View {
        options: SessionsOptions,
        reply: Sender<Result<SessionsView, String>>,
    },
    Subscribe(SessionsOptions),
    Unsubscribe,
    Stop,
    Wake,
}

/// The commands share with the thread, like the look-up's client.
#[derive(Clone)]
pub(crate) struct SessionsClient {
    commands: Sender<SessionsCommand>,
}

impl SessionsClient {
    pub(crate) fn view(&self, options: SessionsOptions) -> Result<SessionsView, String> {
        let (reply, received) = mpsc::channel();
        super::send_to_worker(self.commands.send(SessionsCommand::View { options, reply }))?;
        super::worker_reply(received.recv())?
    }

    pub(crate) fn subscribe(&self, options: SessionsOptions) -> Result<(), String> {
        super::send_to_worker(self.commands.send(SessionsCommand::Subscribe(options)))
    }

    pub(crate) fn unsubscribe(&self) -> Result<(), String> {
        super::send_to_worker(self.commands.send(SessionsCommand::Unsubscribe))
    }
}

/// At most one view per [`VIEW_EVERY_MS`], with a trailing one. The time is an
/// argument, so a test drives it without sleeping.
#[derive(Debug, Default)]
struct ViewThrottle {
    /// The last view sent.
    last_sent: Option<DateTime<Utc>>,
    /// A view was wanted inside the window and has to follow when it is over.
    pending: bool,
}

impl ViewThrottle {
    /// Whether a view is due now: a change or the minute pass, but never within
    /// [`VIEW_EVERY_MS`] of the last one. A wanted view inside the window is
    /// remembered and sent by the next pass after it.
    fn due(&mut self, now: DateTime<Utc>, changed: bool) -> bool {
        let minute = is_due(self.last_sent, now, 60);
        let wanted = changed || minute || self.pending;
        if !wanted {
            return false;
        }
        if self.last_sent.is_some_and(|last| {
            now >= last && now.signed_duration_since(last).num_milliseconds() < VIEW_EVERY_MS
        }) {
            self.pending = true;
            return false;
        }
        self.pending = false;
        self.last_sent = Some(now);
        true
    }

    /// How long until a pending view may be sent; `None` when none is pending
    /// (a clock that moved back makes it due now).
    fn wait(&self, now: DateTime<Utc>) -> Option<Duration> {
        if !self.pending {
            return None;
        }
        let last = self.last_sent?;
        let until = last + ChronoDuration::milliseconds(VIEW_EVERY_MS);
        let wait = until.signed_duration_since(now);
        if wait <= ChronoDuration::zero() {
            return Some(Duration::ZERO);
        }
        u64::try_from(wait.num_milliseconds())
            .ok()
            .map(Duration::from_millis)
    }
}

/// The projects, the pricing and the running sessions' token records the rows
/// are built with. They are built and rebuilt apart from each other: a failing
/// one does not keep the others from being built, and the rows are built with
/// what is there.
#[derive(Default)]
struct RowsContext {
    projects: Option<ProjectsBuilt>,
    /// Why the projects could not be built, and when that was tried.
    projects_failure: Option<ContextFailure>,
    pricing: Option<PricingBuilt>,
    /// Why the pricing could not be built, and when that was tried.
    pricing_failure: Option<ContextFailure>,
    recorded: Option<RecordedBuilt>,
    /// Why the token records could not be read, and when that was tried.
    recorded_failure: Option<ContextFailure>,
    /// The settings' currency from the accounts file, read whether or not the
    /// prices could be read.
    currency: Option<String>,
}

/// The closed rows read for one `ClosedSince`, with when they were read, when
/// that choice was last asked for and why their values are missing.
struct ClosedRows {
    rows: Rows,
    read_at: DateTime<Utc>,
    asked_at: DateTime<Utc>,
    failure: Option<String>,
}

struct ProjectsBuilt {
    projects: projects::Projects,
    /// Warnings from the project and session reads.
    warnings: Vec<String>,
    built: DateTime<Utc>,
}

struct PricingBuilt {
    /// The start of the range the valuer was built for: it values a record
    /// of that range or later with its whole week.
    from: DateTime<Utc>,
    valuer: Valuer,
    settings: Settings,
    /// The prices as they were read, so a model without one can be asked for
    /// ([`want_missing_prices`]).
    prices: Prices,
    /// The dollar rates the provider cost is converted with.
    fx: Fx,
    built: DateTime<Utc>,
}

/// Ask the look-up thread for a model's price (see
/// [`LookupClient::want_price`]). A type of its own so the memory's field stays
/// readable.
type WantPrice = Box<dyn Fn(&str)>;

/// This host's token records of the running sessions: read from the history
/// when built, then kept current with every line this host writes
/// ([`RecordedTokens::add`]), so a pass does not read the history again.
struct RecordedBuilt {
    tokens: RecordedTokens,
    /// The history read's warnings, shown on every pass like the projects'.
    warnings: Vec<String>,
    built: DateTime<Utc>,
}

/// What the rows of one pass are built with ([`prepare_rows_context`]).
struct RowsInputs<'a> {
    projects: Option<&'a projects::Projects>,
    pricing: live_sessions::Pricing<'a>,
    /// The settings' currency from the accounts file.
    currency: Option<String>,
    /// The running sessions' token records, or why they cannot be read.
    recorded: Result<&'a RecordedTokens, String>,
}

struct ContextFailure {
    text: String,
    tried: DateTime<Utc>,
}

/// What the thread remembers between two passes.
pub(crate) struct SessionMemory {
    home: std::path::PathBuf,
    projects: HashMap<String, ProjectKey>,
    waiting: HashMap<String, WaitingMemory>,
    /// The last session record written for each session.
    records: HashMap<String, SessionRecord>,
    /// Sessions whose one closing record has been written and whose entry is
    /// already pruned from the [`ScanState`], so `SessionSeen.recorded` cannot
    /// remember them. A session still in the state uses `recorded`, never this.
    closed: HashSet<String>,
    rows_context: Option<RowsContext>,
    /// The rows of the last report written successfully.
    last_rows: Option<Vec<LiveSession>>,
    /// When this host's report was last written successfully.
    last_report_at: Option<DateTime<Utc>>,
    /// The running sessions of the last pass that could read every file, so a
    /// session that left the list is reported at once.
    running_ids: BTreeSet<String>,
    /// The other hosts' reports of the last read, used on every pass until the
    /// next read so no wait or warning flickers.
    other_hosts: Option<live_sessions::OtherHosts>,
    /// When the other hosts' reports were last read.
    last_other_hosts_at: Option<DateTime<Utc>>,
    /// Why there is no history, when the thread started without one: the rows
    /// then say why their values are missing.
    history_error: Option<String>,
    /// DeepSeek Harness and flash-agents' folders; `None` when they cannot be
    /// found, or in a test that does not build a store.
    harness_paths: Option<HarnessPaths>,
    /// Why the Harness folders cannot be found, said in the view's warnings.
    harness_error: Option<String>,
    /// flash-agents' job reports read so far: a job's Harness id and label.
    /// The labels are task texts and live only here.
    job_reports: harness::JobReports,
    /// Codex's home; `None` when it cannot be found, or in a test that does not
    /// build a store.
    codex_paths: Option<CodexPaths>,
    /// Why Codex's home cannot be found, said in the view's warnings.
    codex_error: Option<String>,
    /// The rows of the last pass, which the view is built from.
    rows: Option<Rows>,
    /// Each running session's answers with thinking since the user's last turn.
    thinking: BTreeMap<String, u32>,
    /// The last summary's `unavailable`, shown by the view.
    unavailable: Option<String>,
    /// The settings' currency the rows were valued with.
    currency: Option<String>,
    /// The history's host; `None` when the thread runs without a history.
    this_host: Option<String>,
    /// The last pass's time, the view's `at`.
    at: Option<DateTime<Utc>>,
    /// The closed rows the view may ask for, each with the choice it was read
    /// for; a choice is kept for [`CLOSED_KEEP_SECS`] after its last ask.
    closed_rows: BTreeMap<ClosedSince, ClosedRows>,
    /// The subscription's options; `None` while nothing is subscribed. A
    /// command's options are never kept here.
    subscription: Option<SessionsOptions>,
    /// Where pushed views go.
    ui: Option<Sender<UiEvent>>,
    /// The last pushed view, so a pass that changed nothing is not pushed.
    last_pushed: Option<SessionsView>,
    throttle: ViewThrottle,
    /// The transcript folders of the running sessions and their agents, from
    /// the keys the `ScanState` knows.
    transcript_folders: BTreeSet<PathBuf>,
    /// The agents' transcript ends, remembered per path with the file's length
    /// and modified time, so an unchanged transcript is not read again.
    agent_ends: transcripts::AgentEnds,
    /// Claude Code's own cost of each session, in US dollars, as the status
    /// line's feed last said it (item 6). A cost is kept while its session is a
    /// row; the feed holds the last writer, so it can be some seconds old.
    feed_costs: BTreeMap<String, f64>,
    /// Ask the look-up thread for a model's price ([`LookupClient::want_price`]);
    /// `None` when no look-up runs (the CLI's tests, or a thread started
    /// without one).
    want_price: Option<WantPrice>,
    /// The models already asked for in this run, so an unpriced model is asked
    /// for once while that stays so.
    wanted_models: BTreeSet<String>,
    /// The most transcript bytes one pass reads: [`SCAN_BUDGET`]. A test
    /// lowers it to reach the limit with small transcripts.
    scan_budget: u64,
}

impl SessionMemory {
    pub(crate) fn new(home: std::path::PathBuf) -> SessionMemory {
        SessionMemory {
            home,
            projects: HashMap::new(),
            waiting: HashMap::new(),
            records: HashMap::new(),
            closed: HashSet::new(),
            rows_context: None,
            last_rows: None,
            last_report_at: None,
            running_ids: BTreeSet::new(),
            other_hosts: None,
            last_other_hosts_at: None,
            history_error: None,
            harness_paths: None,
            harness_error: None,
            job_reports: harness::JobReports::default(),
            codex_paths: None,
            codex_error: None,
            rows: None,
            thinking: BTreeMap::new(),
            unavailable: None,
            currency: None,
            this_host: None,
            at: None,
            closed_rows: BTreeMap::new(),
            subscription: None,
            ui: None,
            last_pushed: None,
            throttle: ViewThrottle::default(),
            transcript_folders: BTreeSet::new(),
            agent_ends: transcripts::AgentEnds::default(),
            feed_costs: BTreeMap::new(),
            want_price: None,
            wanted_models: BTreeSet::new(),
            scan_budget: SCAN_BUDGET,
        }
    }

    /// Start pushing views for `options`: the next pass sends the first one.
    fn subscribe(&mut self, options: SessionsOptions) {
        self.subscription = Some(options);
        self.last_pushed = None;
        self.throttle = ViewThrottle::default();
    }

    /// Stop pushing views. The caller drops the transcript folder watches.
    fn unsubscribe(&mut self) {
        self.subscription = None;
        self.last_pushed = None;
        self.throttle = ViewThrottle::default();
        self.transcript_folders.clear();
    }

    /// The view the subscription pushes, or `None` when nothing is subscribed
    /// or before the first pass.
    fn view(&self) -> Option<SessionsView> {
        let options = self.subscription.as_ref()?;
        self.view_for(options)
    }

    /// The view for `options`, from what the thread holds. The command's answer
    /// and the pushed view both come through here; a command's options never
    /// replace the subscription's.
    fn view_for(&self, options: &SessionsOptions) -> Option<SessionsView> {
        let at = self.at?;
        let rows = self.rows.as_ref()?;
        let other: &[OtherHost] = self
            .other_hosts
            .as_ref()
            .map(|hosts| hosts.hosts.as_slice())
            .unwrap_or(&[]);
        let closed = self.closed_rows.get(&options.closed);
        let closed_sessions = closed
            .map(|read| read.rows.sessions.as_slice())
            .unwrap_or(&[]);
        let closed_failure = closed.and_then(|read| read.failure.as_deref());
        let unavailable = match (&self.unavailable, closed_failure) {
            // A broken accounts file fails both reads with the same text: say it once.
            (Some(first), Some(second)) if first.contains(second) => Some(first.clone()),
            (Some(first), Some(second)) => Some(format!("{first}; {second}")),
            (Some(first), None) => Some(first.clone()),
            (None, Some(second)) => Some(second.to_string()),
            (None, None) => None,
        };
        Some(view_of(
            at,
            self.this_host.clone(),
            &rows.sessions,
            closed_sessions,
            other,
            &self.thinking,
            unavailable.as_deref(),
            self.currency.as_deref(),
            options,
        ))
    }

    fn project_for(&mut self, cwd: &str) -> ProjectKey {
        if let Some(project) = self.projects.get(cwd) {
            return project.clone();
        }
        let project = project::identify(Path::new(cwd), &self.home);
        self.projects.insert(cwd.to_string(), project.clone());
        project
    }
}

/// Whether a time is due for the next round: `now` is at least `secs` after
/// `at`, or `at` is in the future (a clock that moved back must not silence
/// the thread). `None` is due at once. Every "is it time again" rule of the
/// thread goes through this one helper.
fn is_due(at: Option<DateTime<Utc>>, now: DateTime<Utc>, secs: i64) -> bool {
    match at {
        None => true,
        Some(at) => at > now || now.signed_duration_since(at).num_seconds() >= secs,
    }
}

/// Whether one part of the rows context has to be built now: never built,
/// older than [`ROWS_CONTEXT_MAX_AGE_SECS`], or a failed build whose
/// [`ROWS_CONTEXT_RETRY_SECS`] have passed.
fn part_needs_build(
    built: Option<DateTime<Utc>>,
    failure: Option<&ContextFailure>,
    now: DateTime<Utc>,
) -> bool {
    if !is_due(built, now, ROWS_CONTEXT_MAX_AGE_SECS) {
        return false;
    }
    failure.is_none_or(|failure| is_due(Some(failure.tried), now, ROWS_CONTEXT_RETRY_SECS))
}

/// [`part_needs_build`] for a part built for a range: also when the part
/// built does not reach back far enough (`short`). A failed build is tried
/// again only after [`ROWS_CONTEXT_RETRY_SECS`] either way, so a range that
/// cannot be built is not tried on every pass.
fn ranged_part_needs_build(
    built: Option<DateTime<Utc>>,
    failure: Option<&ContextFailure>,
    short: bool,
    now: DateTime<Utc>,
) -> bool {
    let retry =
        failure.is_none_or(|failure| is_due(Some(failure.tried), now, ROWS_CONTEXT_RETRY_SECS));
    part_needs_build(built, failure, now) || (short && retry)
}

/// Build the parts of the rows context that are due, show every failure that
/// is still current once, and hand out what the rows are built with.
///
/// `needed_from` is the first hour of the running sessions' token records
/// ([`live_sessions::records_from`]); `None` when no session runs. The token records
/// are read when they are due or do not reach back that far, and the valuer is
/// built for a range that starts there too (or a day ago, whichever is
/// earlier), so every record of a running session is valued with its whole
/// week.
fn prepare_rows_context<'a>(
    context: &'a mut RowsContext,
    app: &AppPaths,
    history: &HistoryDir,
    needed_from: Option<DateTime<Utc>>,
    now: DateTime<Utc>,
    summary: &mut SessionsSummary,
) -> RowsInputs<'a> {
    let projects_due = part_needs_build(
        context.projects.as_ref().map(|built| built.built),
        context.projects_failure.as_ref(),
        now,
    );
    let day_ago = now - ChronoDuration::days(1);
    let pricing_from = needed_from.map_or(day_ago, |from| from.min(day_ago));
    let pricing_due = ranged_part_needs_build(
        context.pricing.as_ref().map(|built| built.built),
        context.pricing_failure.as_ref(),
        context
            .pricing
            .as_ref()
            .is_some_and(|built| built.from > pricing_from),
        now,
    );
    if projects_due || pricing_due {
        let accounts = store::load_accounts(app)
            .map_err(|error| format!("the accounts cannot be read: {error}"));
        // The view's currency comes from the settings with the accounts file,
        // whether or not the prices and the dollar rate can be read.
        context.currency = accounts
            .as_ref()
            .ok()
            .and_then(|file| file.settings.currency.clone());
        if projects_due {
            match &accounts {
                Ok(file) => match projects::load(history, &file.accounts) {
                    Ok((projects, warnings)) => {
                        context.projects = Some(ProjectsBuilt {
                            projects,
                            warnings,
                            built: now,
                        });
                        context.projects_failure = None;
                    }
                    Err(error) => {
                        context.projects_failure = Some(ContextFailure {
                            text: format!("the projects cannot be read: {error}"),
                            tried: now,
                        })
                    }
                },
                Err(error) => {
                    context.projects_failure = Some(ContextFailure {
                        text: error.clone(),
                        tried: now,
                    })
                }
            }
        }
        if pricing_due {
            match &accounts {
                Ok(file) => match build_pricing(
                    app,
                    history,
                    &Range {
                        from: pricing_from,
                        to: now,
                    },
                    file,
                ) {
                    Ok((valuer, settings, prices, fx)) => {
                        context.pricing = Some(PricingBuilt {
                            from: pricing_from,
                            valuer,
                            settings,
                            prices,
                            fx,
                            built: now,
                        });
                        context.pricing_failure = None;
                    }
                    Err(text) => {
                        context.pricing_failure = Some(ContextFailure { text, tried: now })
                    }
                },
                Err(error) => {
                    context.pricing_failure = Some(ContextFailure {
                        text: error.clone(),
                        tried: now,
                    })
                }
            }
        }
    }
    prepare_recorded(context, history, needed_from, now);

    let projects_failure = context
        .projects_failure
        .as_ref()
        .map(|failure| failure.text.clone());
    let pricing_failure = context
        .pricing_failure
        .as_ref()
        .map(|failure| failure.text.clone());
    if let Some(text) = &projects_failure {
        add_unavailable(summary, text.clone());
    }
    if let Some(text) = &pricing_failure {
        // The accounts are one failure for both parts: show it once.
        if projects_failure.as_ref() != Some(text) {
            add_unavailable(summary, text.clone());
        }
    }
    if let Some(failure) = &context.recorded_failure {
        add_unavailable(summary, failure.text.clone());
    }
    if let Some(built) = &context.projects {
        for warning in &built.warnings {
            add_warning(summary, warning.clone());
        }
    }
    if let Some(built) = &context.recorded {
        for warning in &built.warnings {
            add_warning(summary, warning.clone());
        }
    }
    let projects = context.projects.as_ref().map(|built| &built.projects);
    // A valuer built for a later start would value the older records without
    // their weeks: it is not used then, as the records below.
    let pricing = match &context.pricing {
        Some(built) if built.from <= pricing_from => live_sessions::Pricing::Known {
            valuer: &built.valuer,
            settings: &built.settings,
        },
        _ => live_sessions::Pricing::Unknown {
            reason: pricing_failure.unwrap_or_else(|| "the values cannot be read".to_string()),
        },
    };
    // Records that do not reach back to every running session's first hour
    // would count only part of a row: they are not used then.
    let recorded = match (&context.recorded, &context.recorded_failure) {
        (Some(built), _) if needed_from.is_none_or(|from| built.tokens.covers(from)) => {
            Ok(&built.tokens)
        }
        (_, Some(failure)) => Err(failure.text.clone()),
        (_, None) => Err("the sessions' token records are not read yet".to_string()),
    };
    RowsInputs {
        projects,
        pricing,
        currency: context.currency.clone(),
        recorded,
    }
}

/// Read the running sessions' token records when they are due: never read,
/// older than [`ROWS_CONTEXT_MAX_AGE_SECS`], or not reaching back to
/// `needed_from`. A failed read is tried again after
/// [`ROWS_CONTEXT_RETRY_SECS`]; the records kept until then stay current
/// through the lines this host writes. Without a running session nothing is
/// read; an empty set is kept, so the first session that runs reads its
/// records.
fn prepare_recorded(
    context: &mut RowsContext,
    history: &HistoryDir,
    needed_from: Option<DateTime<Utc>>,
    now: DateTime<Utc>,
) {
    let Some(from) = needed_from else {
        if context.recorded.is_none() {
            context.recorded = Some(RecordedBuilt {
                tokens: RecordedTokens::default(),
                warnings: Vec::new(),
                built: now,
            });
        }
        return;
    };
    let due = ranged_part_needs_build(
        context.recorded.as_ref().map(|built| built.built),
        context.recorded_failure.as_ref(),
        context
            .recorded
            .as_ref()
            .is_none_or(|built| !built.tokens.covers(from)),
        now,
    );
    if !due {
        return;
    }
    match RecordedTokens::read(history, from) {
        Ok((tokens, warnings)) => {
            context.recorded = Some(RecordedBuilt {
                tokens,
                warnings,
                built: now,
            });
            context.recorded_failure = None;
        }
        Err(error) => {
            context.recorded_failure = Some(ContextFailure {
                text: format!("the sessions' token records cannot be read: {error}"),
                tried: now,
            });
        }
    }
}

/// Read the prices, the dollar rate and the accounts and build the valuer for
/// `range`. Any failure is the error text the sessions area shows.
fn build_pricing(
    app: &AppPaths,
    history: &HistoryDir,
    range: &Range,
    file: &store::AccountsFile,
) -> Result<(Valuer, Settings, Prices, Fx), String> {
    let (prices, fx) = super::window::load_prices_and_fx(app)?;
    let valuer = Valuer::build(
        history,
        range,
        &prices,
        &fx,
        &file.settings,
        &file.accounts,
        &file.provider_subscriptions,
        &RefCell::new(Vec::new()),
    )
    .map_err(|error| format!("the values cannot be read: {error}"))?;
    Ok((valuer, file.settings.clone(), prices, fx))
}

struct WaitingMemory {
    project: ProjectKey,
    since: DateTime<Utc>,
}

/// The handle the tray keeps. Dropping it, or calling [`stop`](Self::stop),
/// stops and joins the thread.
pub(crate) struct SessionsHandle {
    stop: Arc<AtomicBool>,
    commands: Sender<SessionsCommand>,
    client: SessionsClient,
    thread: Option<JoinHandle<()>>,
    latest: Arc<Mutex<Option<SessionsSummary>>>,
}

impl SessionsHandle {
    /// The summary of the last pass, shared with the action thread so a switch
    /// can record how many sessions it interrupted.
    pub(crate) fn latest(&self) -> Arc<Mutex<Option<SessionsSummary>>> {
        Arc::clone(&self.latest)
    }

    /// The client the three sessions commands use.
    pub(crate) fn client(&self) -> SessionsClient {
        self.client.clone()
    }

    pub(crate) fn stop(&mut self) {
        self.stop.store(true, Ordering::SeqCst);
        let _ = self.commands.send(SessionsCommand::Stop);
        if let Some(thread) = self.thread.take() {
            let _ = thread.join();
        }
    }
}

impl Drop for SessionsHandle {
    fn drop(&mut self) {
        self.stop();
    }
}

/// Start the sessions thread. `settings.read_sessions` must already be on; the
/// caller only starts it then. `history` is the folder the worker opened at
/// start, shared with the engine and action runtimes.
#[allow(clippy::too_many_arguments)]
pub(crate) fn spawn(
    app: AppPaths,
    claude: ClaudePaths,
    history: Result<HistoryDir, String>,
    harness: Result<HarnessPaths, String>,
    codex: Result<CodexPaths, String>,
    lookup: Option<LookupClient>,
    ui: Sender<UiEvent>,
    engine: Sender<EngineCommand>,
) -> Result<SessionsHandle, String> {
    let stop = Arc::new(AtomicBool::new(false));
    let (commands, command_rx) = mpsc::channel::<SessionsCommand>();
    let latest = Arc::new(Mutex::new(None));
    let thread_stop = Arc::clone(&stop);
    let thread_latest = Arc::clone(&latest);
    let watch_wake = commands.clone();
    let client = SessionsClient {
        commands: commands.clone(),
    };
    let thread = thread::Builder::new()
        .name("seatswap-sessions".to_string())
        .spawn(move || {
            run(
                app,
                claude,
                history,
                harness,
                codex,
                lookup,
                ui,
                engine,
                thread_stop,
                command_rx,
                watch_wake,
                thread_latest,
            )
        })
        .map_err(|error| format!("cannot start the sessions thread: {error}"))?;
    Ok(SessionsHandle {
        stop,
        commands,
        client,
        thread: Some(thread),
        latest,
    })
}

#[allow(clippy::too_many_arguments)]
fn run(
    app: AppPaths,
    claude: ClaudePaths,
    history: Result<HistoryDir, String>,
    harness: Result<HarnessPaths, String>,
    codex: Result<CodexPaths, String>,
    lookup: Option<LookupClient>,
    ui: Sender<UiEvent>,
    engine: Sender<EngineCommand>,
    stop: Arc<AtomicBool>,
    commands: Receiver<SessionsCommand>,
    watch_wake: Sender<SessionsCommand>,
    latest: Arc<Mutex<Option<SessionsSummary>>>,
) {
    let (recorder, history_error) = match history {
        Ok(history) => (Some(Recorder::new(history)), None),
        Err(error) => (None, Some(error)),
    };
    let (mut state, state_error) = load_state(&app);
    let mut memory = SessionMemory::new(claude.home().to_path_buf());
    memory.history_error = history_error.clone();
    match harness {
        Ok(paths) => memory.harness_paths = Some(paths),
        Err(error) => memory.harness_error = Some(error),
    }
    match codex {
        Ok(paths) => memory.codex_paths = Some(paths),
        Err(error) => memory.codex_error = Some(error),
    }
    memory.ui = Some(ui.clone());
    // A model without a price is asked for through the look-up thread, which
    // limits itself to one ask an hour per model (G-1).
    memory.want_price =
        lookup.map(|lookup| Box::new(move |model: &str| lookup.want_price(model)) as WantPrice);
    let mut watch: Option<FolderWatch> = None;
    let mut last_watch_error: Option<String> = None;
    let mut transcript_watches: BTreeMap<PathBuf, FolderWatch> = BTreeMap::new();
    let mut last_transcript_watch_error: Option<String> = None;

    loop {
        if stop.load(Ordering::SeqCst) {
            return;
        }
        if watch.is_none() {
            let wake = watch_wake.clone();
            match watch_folder(&claude.sessions_dir(), move || {
                let _ = wake.send(SessionsCommand::Wake);
            }) {
                Ok(found) => watch = Some(found),
                // The folder is made when the first session starts; the 60 s
                // wake tries again.
                Err(WatchError::Io { source, .. })
                    if source.kind() == std::io::ErrorKind::NotFound => {}
                Err(error) => {
                    let text = error.to_string();
                    if last_watch_error.as_deref() != Some(text.as_str()) {
                        eprintln!("seatswap: sessions watcher: {text}");
                        last_watch_error = Some(text);
                    }
                }
            }
        }

        let now = Utc::now();
        let outcome = match SystemProbe::new() {
            Ok(probe) => one_pass(
                &app,
                &claude,
                &probe,
                &harness::RealProc,
                recorder.as_ref(),
                &mut state,
                &mut memory,
                now,
                state_error.as_deref(),
                history_error.as_deref(),
            ),
            Err(error) => {
                let mut summary = SessionsSummary {
                    running: 0,
                    busy: 0,
                    tied: 0,
                    tied_busy: 0,
                    waiting: Vec::new(),
                    unavailable: None,
                    warning: None,
                };
                add_unavailable(
                    &mut summary,
                    format!("running sessions cannot be read: {error}"),
                );
                if let Some(error) = &history_error {
                    // The order the pop-up shows: the history error first, then the state.
                    add_warning(&mut summary, error.clone());
                }
                if let Some(error) = &state_error {
                    add_warning(&mut summary, error.clone());
                }
                save_state(
                    &app,
                    &claude,
                    &mut state,
                    now,
                    state_error.as_deref(),
                    &mut summary,
                );
                pass_without_running(&app, recorder.as_ref(), &mut memory, now, summary)
            }
        };

        update_transcript_watches(
            &mut transcript_watches,
            &memory.transcript_folders,
            &watch_wake,
            &mut last_transcript_watch_error,
        );

        {
            let mut shared = match latest.lock() {
                Ok(shared) => shared,
                Err(poisoned) => poisoned.into_inner(),
            };
            *shared = Some(outcome.summary.clone());
        }
        if ui.send(UiEvent::Sessions(outcome.summary.clone())).is_err() {
            return;
        }
        if engine
            .send(EngineCommand::ModelsInUse {
                models: outcome.models,
                tied_busy: outcome.tied_busy,
                thinking: outcome.thinking,
            })
            .is_err()
        {
            return;
        }

        let mut wait = if outcome.wake_soon {
            AFTER_INCOMPLETE
        } else {
            WAKE
        };
        if let Some(pending) = memory.throttle.wait(now) {
            if pending < wait {
                wait = pending;
            }
        }
        // One deadline for the next pass. A command handled below waits only
        // for the rest of it; a `View` or `Unsubscribe` never moves it later.
        let deadline = Instant::now() + wait;
        loop {
            let remaining = deadline.saturating_duration_since(Instant::now());
            match commands.recv_timeout(remaining) {
                Ok(SessionsCommand::Wake) => break,
                Ok(SessionsCommand::View { options, reply }) => {
                    let answer = answer_view(&mut memory, options, &app, recorder.as_ref());
                    let _ = reply.send(answer);
                }
                Ok(SessionsCommand::Subscribe(options)) => {
                    memory.subscribe(options);
                    break;
                }
                Ok(SessionsCommand::Unsubscribe) => {
                    memory.unsubscribe();
                    transcript_watches.clear();
                }
                Ok(SessionsCommand::Stop) | Err(RecvTimeoutError::Disconnected) => return,
                Err(RecvTimeoutError::Timeout) => break,
            }
            // A command handled past the deadline never keeps the next pass
            // waiting: the deadline is the pass's, not the command's.
            if Instant::now() >= deadline {
                break;
            }
        }
    }
}

/// Keep exactly the transcript folders of the running sessions and their
/// agents: a folder that left the list is dropped, a new one is watched. The
/// path comes from the `ScanState` key, never from a second naming rule.
fn update_transcript_watches(
    watches: &mut BTreeMap<PathBuf, FolderWatch>,
    folders: &BTreeSet<PathBuf>,
    wake: &Sender<SessionsCommand>,
    last_error: &mut Option<String>,
) {
    watches.retain(|folder, _| folders.contains(folder));
    for folder in folders {
        if watches.contains_key(folder) {
            continue;
        }
        let wake = wake.clone();
        match watch_folder(folder, move || {
            let _ = wake.send(SessionsCommand::Wake);
        }) {
            Ok(found) => {
                watches.insert(folder.clone(), found);
            }
            Err(WatchError::Io { source, .. }) if source.kind() == std::io::ErrorKind::NotFound => {
            }
            Err(error) => {
                let text = format!("{}: {error}", folder.display());
                if last_error.as_deref() != Some(text.as_str()) {
                    eprintln!("seatswap: sessions watcher: {text}");
                    *last_error = Some(text);
                }
            }
        }
    }
}

/// The transcript state at start: the files saved by the last run, or the
/// default with the error that keeps this pass from writing it back.
///
/// A version-1 state is migrated by `ScanState::load`; a state of any other
/// version is an error. This pass then reads no transcript and writes no
/// record, because rebuilding the state is what would write the history twice.
fn load_state(app: &AppPaths) -> (ScanState, Option<String>) {
    match ScanState::load(app) {
        Ok(state) => (state, None),
        Err(error) => (
            ScanState::default(),
            Some(format!(
                "the transcript state cannot be read: {error}; no transcript is read until it can be. \
                 Deleting {} reads every transcript again and writes its tokens to the history a second time",
                app.transcripts_state_dir().display()
            )),
        ),
    }
}

/// Write what changed; a failure is a warning on the summary, never a pass
/// that stops.
fn save_state(
    app: &AppPaths,
    claude: &ClaudePaths,
    state: &mut ScanState,
    now: DateTime<Utc>,
    state_error: Option<&str>,
    summary: &mut SessionsSummary,
) {
    if state_error.is_some() {
        return;
    }
    if let Err(error) = state.save(claude, app, now) {
        add_warning(
            summary,
            format!("the transcript state cannot be written: {error}"),
        );
    }
}

/// Item 6: Claude Code's own cost from the status line's feed becomes the API
/// value of its session's row, and a model without a price is asked for. The
/// feed is read on every pass: it holds the last session that wrote it, so a
/// cost can be some seconds old, and the last cost read for a session is kept
/// while that session is a row.
fn apply_provider_costs(
    app: &AppPaths,
    rows: &mut Rows,
    memory: &mut SessionMemory,
    now: DateTime<Utc>,
    summary: &mut SessionsSummary,
) {
    // A broken feed or a cost it cannot use is said on every pass while it
    // lasts, not once and then gone: the warning is on the pass's own summary,
    // so a user who opens the window later still sees it.
    let problem = match feed::read(app) {
        Ok(Some(feed)) => {
            if let Some(problem) = feed.cost_problem {
                Some(problem)
            } else {
                match (feed.session, feed.cost_usd) {
                    (Some(session), Some(cost)) if cost.is_finite() && cost >= 0.0 => {
                        memory.feed_costs.insert(session, cost);
                        None
                    }
                    (Some(session), Some(cost)) => Some(format!(
                        "the status line's feed holds a cost for session {session} that is not a finite cost of 0 or more ({cost}); it is not used"
                    )),
                    _ => None,
                }
            }
        }
        Ok(None) => None,
        Err(error) => Some(format!(
            "the status line's feed cannot be read, so Claude Code's own costs are not used: {error}"
        )),
    };
    if let Some(text) = problem {
        add_warning(summary, text);
    }
    let listed: BTreeSet<&str> = rows
        .sessions
        .iter()
        .map(|row| row.session.as_str())
        .collect();
    memory
        .feed_costs
        .retain(|session, _| listed.contains(session.as_str()));
    let Some(built) = memory
        .rows_context
        .as_ref()
        .and_then(|context| context.pricing.as_ref())
    else {
        return;
    };
    for row in &mut rows.sessions {
        if row.provider != live_sessions::CLAUDE_PROVIDER {
            continue;
        }
        let cost = memory.feed_costs.get(&row.session).copied();
        if let Some(problem) = live_sessions::take_provider_cost(
            &mut row.values,
            cost,
            &built.settings,
            &built.fx,
            now,
        ) {
            add_warning(summary, format!("session {}: {problem}", row.session));
        }
    }
    want_missing_prices(memory, &rows.sessions, now);
}

/// Ask the look-up thread for a price for every plan model that a listed row or
/// one of its agents used and that has none (item 4, spec §1.5). A model outside
/// the Claude plan (a Codex model, a Harness model) never gets a price from
/// models.dev's `anthropic` list, and asking would be a useless look-up. The
/// thread limits itself to one ask an hour per model; the set here keeps a pass
/// every second from sending the same ask again, and a model that gets a price
/// is dropped from it so a later loss of the price asks again.
fn want_missing_prices(memory: &mut SessionMemory, rows: &[LiveSession], now: DateTime<Utc>) {
    let unpriced: BTreeSet<String> = {
        let Some(prices) = memory
            .rows_context
            .as_ref()
            .and_then(|context| context.pricing.as_ref())
            .map(|built| &built.prices)
        else {
            return;
        };
        let mut models = BTreeSet::new();
        for row in rows {
            models.extend(row.model.iter().cloned());
            models.extend(row.agents.iter().filter_map(|agent| agent.model.clone()));
        }
        models
            .into_iter()
            .filter(|model| is_plan_model(model))
            .filter(|model| prices.of_model(model, now).is_none())
            .collect()
    };
    let mut ask = Vec::new();
    for model in &unpriced {
        if memory.wanted_models.insert(model.clone()) {
            ask.push(model.clone());
        }
    }
    memory
        .wanted_models
        .retain(|model| unpriced.contains(model));
    if let Some(want) = &memory.want_price {
        for model in &ask {
            want(model);
        }
    }
}

/// One wake of the sessions thread: read what is new, write the history, save
/// the state. The thread calls it with [`SystemProbe`]; the tests with a fake
/// probe.
#[allow(clippy::too_many_arguments)]
fn one_pass(
    app: &AppPaths,
    claude: &ClaudePaths,
    probe: &dyn Liveness,
    procs: &dyn harness::ProcSource,
    recorder: Option<&Recorder>,
    state: &mut ScanState,
    memory: &mut SessionMemory,
    now: DateTime<Utc>,
    state_error: Option<&str>,
    history_error: Option<&str>,
) -> PassOutcome {
    let mut outcome = run_pass(
        app,
        claude,
        probe,
        procs,
        recorder,
        state,
        memory,
        now,
        state_error,
    );
    memory.unavailable = outcome.summary.unavailable.clone();
    if let Some(error) = history_error {
        // The order the pop-up shows: the history error first, then the state.
        prepend_warning(&mut outcome.summary, error.to_string());
    }
    save_state(app, claude, state, now, state_error, &mut outcome.summary);
    outcome
}

/// One pass: running sessions, transcripts, tokens, session records, waits.
/// The thread calls it with [`SystemProbe`]; the tests call it with a fake probe.
#[allow(clippy::too_many_arguments)]
pub(crate) fn run_pass(
    app: &AppPaths,
    claude: &ClaudePaths,
    probe: &dyn Liveness,
    procs: &dyn harness::ProcSource,
    recorder: Option<&Recorder>,
    state: &mut ScanState,
    memory: &mut SessionMemory,
    now: DateTime<Utc>,
    state_error: Option<&str>,
) -> PassOutcome {
    let mut summary = SessionsSummary {
        running: 0,
        busy: 0,
        tied: 0,
        tied_busy: 0,
        waiting: Vec::new(),
        unavailable: None,
        warning: state_error.map(str::to_string),
    };
    let running = match sessions::running(claude, probe) {
        Ok(running) => running,
        Err(error) => {
            add_unavailable(
                &mut summary,
                format!("running sessions cannot be read: {error}"),
            );
            return pass_without_running(app, recorder, memory, now, summary);
        }
    };

    summary.running = match count_u32(running.sessions.len(), "the running session count") {
        Ok(count) => count,
        Err(error) => {
            add_warning(&mut summary, error);
            return PassOutcome {
                summary,
                models: None,
                tied_busy: 0,
                thinking: Vec::new(),
                wake_soon: false,
            };
        }
    };
    let busy = running
        .sessions
        .iter()
        .filter(|session| session.status == SessionStatus::Busy)
        .count();
    summary.busy = match count_u32(busy, "the busy session count") {
        Ok(count) => count,
        Err(error) => {
            add_warning(&mut summary, error);
            return PassOutcome {
                summary,
                models: None,
                tied_busy: 0,
                thinking: Vec::new(),
                wake_soon: false,
            };
        }
    };
    for session in &running.sessions {
        if let Some(version) = &session.version {
            if sessions::newer_than_tested(version) {
                add_warning(
                    &mut summary,
                    format!(
                        "Claude Code {version} is newer than the version seatswap was tested with ({}). Session data may be incomplete.",
                        sessions::TESTED_CLAUDE_CODE
                    ),
                );
                break;
            }
        }
    }
    if !running.skipped.is_empty() {
        let texts: Vec<String> = running
            .skipped
            .iter()
            .map(|name| format!("session file {name} cannot be read"))
            .collect();
        add_warning(
            &mut summary,
            format!("{}; no session was closed in this pass", texts.join("; ")),
        );
    }

    // Waiting sessions, and the waits that just ended. The rows are the one
    // source of a waiting session's text; a file with an invalid time is their
    // warning, not a second one here.
    let mut still_waiting: HashSet<String> = HashSet::new();
    for session in &running.sessions {
        if session.status != SessionStatus::Waiting {
            continue;
        }
        let Some(since) = DateTime::<Utc>::from_timestamp_millis(session.status_updated_at) else {
            continue;
        };
        let project = memory.project_for(&session.cwd);
        memory
            .waiting
            .insert(session.session_id.clone(), WaitingMemory { project, since });
        still_waiting.insert(session.session_id.clone());
    }
    let ended: Vec<String> = memory
        .waiting
        .keys()
        .filter(|session| !still_waiting.contains(*session))
        .cloned()
        .collect();
    for session in ended {
        if let Some(wait) = memory.waiting.remove(&session) {
            let secs = match u64::try_from((now - wait.since).num_seconds()) {
                Ok(secs) => secs,
                Err(_) => {
                    add_warning(
                        &mut summary,
                        format!(
                            "a wait of session {session} ended before it began (the clock moved back); it is not recorded"
                        ),
                    );
                    continue;
                }
            };
            if let Some(recorder) = recorder {
                if let Err(error) = recorder.event(
                    now,
                    EventKind::Waited {
                        session,
                        project: wait.project,
                        secs,
                    },
                ) {
                    add_history_error(&mut summary, error);
                }
            }
        }
    }

    // Transcripts. A state that could not be read keeps the session counts but
    // stops every transcript read, as the settings say. The token lines this
    // pass reads but cannot write stay `unwritten`: their answers are read again
    // next pass, and until then the running rows still count them.
    let mut scan_complete = true;
    let mut unwritten: Vec<TokenRecord> = Vec::new();
    if state_error.is_none() {
        match transcripts::scan(claude, state, memory.scan_budget) {
            Ok(mut scan) => {
                scan_complete = scan.complete;
                let tokens = transcripts::token_records(&scan.answers, scan.state(), &memory.home);
                if tokens.is_empty() {
                    // No token line can be lost, so the offsets move at once;
                    // the caller's save writes the state and its pruning.
                    scan.commit(state);
                } else if recorder.is_none() {
                    // Without a recorder the token lines are not written. The
                    // offsets stay where they were, so a later pass with a
                    // working history reads these answers again.
                    scan_complete = true;
                    unwritten = tokens;
                } else {
                    // Put the scanned offsets on disk before the token lines: a
                    // crash between the two steps loses those answers (the disk
                    // says they were read, but their lines were never written),
                    // never counts them twice. The other order counted them
                    // twice when the save failed.
                    let folders = scan.changed_folders();
                    match scan.save(claude, app, now) {
                        Ok(_) => {
                            let tokens_written = match recorder {
                                Some(recorder) => match recorder.tokens(&tokens) {
                                    Ok(()) => true,
                                    Err(error) => {
                                        add_history_error(&mut summary, error);
                                        false
                                    }
                                },
                                None => true,
                            };
                            if tokens_written {
                                scan.commit(state);
                                recorded_lines_written(memory, &tokens);
                            } else {
                                unwritten = tokens.clone();
                                // The token lines are not written: put the old
                                // offsets back on disk, so a restart reads the
                                // same answers again and nothing is counted
                                // twice.
                                for folder in &folders {
                                    state.mark_changed(folder);
                                }
                                if let Err(error) = state.save(claude, app, now) {
                                    add_warning(
                                        &mut summary,
                                        format!(
                                            "the transcript state cannot be written back: {error}"
                                        ),
                                    );
                                }
                                // Retrying at once would read the same bytes
                                // again and make no progress until the history
                                // works: wait for the regular wake instead.
                                scan_complete = true;
                            }
                        }
                        Err(error) => {
                            add_warning(
                                &mut summary,
                                format!("the transcript state cannot be written: {error}"),
                            );
                            scan_complete = true;
                            unwritten = tokens;
                        }
                    }
                }
                // A scan that is not committed starts at the same offsets every
                // pass, so a backlog past the budget is never read until its
                // lines can be written: the running rows count only the part
                // read, never a part twice. Say so, never show it as all.
                if !unwritten.is_empty() && !scan.complete {
                    add_warning(
                        &mut summary,
                        "the transcripts hold more new answers than one pass reads, and they cannot be recorded: the running sessions count only the part read until the history can be written".to_string(),
                    );
                }
            }
            Err(error) => {
                scan_complete = false;
                add_warning(
                    &mut summary,
                    format!("the transcripts cannot be read: {error}"),
                );
            }
        }
    }

    let running_ids: BTreeSet<String> = running
        .sessions
        .iter()
        .map(|session| session.session_id.clone())
        .collect();

    // The transcript ends no one asked for during the last pass are forgotten.
    memory.agent_ends.start_pass();
    // One `agents_of` per running session per pass: the session record and the
    // rows use the same list, so the meta files are read once. Its warnings are
    // the pass's, whether or not the window is open.
    let mut agents_by_session: HashMap<String, Vec<transcripts::AgentMeta>> = HashMap::new();
    for session in &running.sessions {
        let Some(seen) = state.sessions.get(&session.session_id) else {
            continue;
        };
        let agents = transcripts::agents_of(
            claude,
            &session.session_id,
            &seen.spawns,
            &mut memory.agent_ends,
        );
        for warning in agents.warnings {
            add_warning(&mut summary, warning);
        }
        agents_by_session.insert(session.session_id.clone(), agents.metas);
    }

    // The sessions that left the running list, kept until the rows give the
    // agents linked to them: `forget_closed` removes them below, and their
    // closed records then count the same flash/Harness workers the running row
    // counted. Their `recorded` line says which already have their closing
    // record.
    let stopped: Vec<(String, SessionSeen)> = if running.skipped.is_empty() {
        state
            .sessions
            .iter()
            .filter(|(session, _)| !running_ids.contains(*session))
            .filter(|(_, seen)| {
                !seen
                    .recorded
                    .as_ref()
                    .is_some_and(|recorded| recorded.closed && recorded.last == seen.last)
            })
            .map(|(session, seen)| (session.clone(), seen.clone()))
            .collect()
    } else {
        Vec::new()
    };

    // A session that is not running and whose last answer is more than an hour
    // old leaves the state; its files' offsets stay, so its transcript is not
    // read again. As with the closed records: when a session file could not be
    // read this pass, the thread cannot tell which session ended, so it forgets
    // none.
    if running.skipped.is_empty() {
        state.forget_closed(&running_ids, now);
    }
    // The links of a closed session are kept for the Harness window, then go.
    state.prune_closed_flash_jobs(now, HARNESS_SINCE_HOURS);

    // A session that left the list is reported at once and its closed rows
    // are read again; otherwise both follow their own "is it time" rules.
    let closed_now = running.skipped.is_empty()
        && memory
            .running_ids
            .iter()
            .any(|session| !running_ids.contains(session));

    // The Codex sessions of the same live window. The process table is walked
    // for them together with the Harness sessions below, never twice.
    let since = now - ChronoDuration::hours(HARNESS_SINCE_HOURS);
    if let Some(error) = &memory.codex_error {
        add_warning(&mut summary, error.clone());
    }
    let codex_sessions = match &memory.codex_paths {
        Some(paths) => {
            let sessions = paths.sessions_dir();
            let read = codex_sessions::sessions(&paths.home, since, now);
            for warning in &read.warnings {
                add_warning(&mut summary, warning.clone());
            }
            // A recorded total goes only when its rollout file has left the
            // store and no rollout of its session was read in this pass; the
            // window and a failed read keep it.
            if sessions.exists() {
                let read_ids: BTreeSet<String> = read
                    .sessions
                    .iter()
                    .map(|session| session.id.clone())
                    .collect();
                state.prune_codex_recorded(&sessions, &read_ids);
            }
            read.sessions
        }
        None => Vec::new(),
    };

    // The Harness sessions of the range and the Claude Code sessions they hang
    // under: named in a transcript, or (on Linux) through a running job's lock.
    if let Some(error) = &memory.harness_error {
        add_warning(&mut summary, error.clone());
    }
    let (harness_sessions, mut harness_owners, claude_cwd, harness_recorded, harness_walk) =
        match &memory.harness_paths {
            Some(paths) => {
                let read = harness::sessions(paths, since);
                for warning in &read.warnings {
                    add_warning(&mut summary, warning.clone());
                }
                for warning in memory.job_reports.refresh(paths) {
                    add_warning(&mut summary, warning);
                }
                let links = harness::links(
                    &memory.job_reports,
                    &state.sessions,
                    &state.closed_flash_jobs,
                );
                for warning in &links.warnings {
                    add_warning(&mut summary, warning.clone());
                }
                let read_by_id: BTreeMap<&str, &HarnessSession> = read
                    .sessions
                    .iter()
                    .map(|session| (session.id.as_str(), session))
                    .collect();
                // The process table is walked only when a running rule needs
                // it: an unfinished Harness session is unlinked (to find its
                // owner) or quiet for the whole quiet window (to find its
                // lock). A finished session needs neither.
                let unlinked = |session: &HarnessSession| {
                    let mut seen = BTreeSet::new();
                    let mut current = Some(session.id.as_str());
                    while let Some(id) = current {
                        if !seen.insert(id) {
                            break;
                        }
                        if links.owners.contains_key(id) {
                            return false;
                        }
                        current = match read_by_id.get(id) {
                            Some(session) => session.parent.as_deref(),
                            None => break,
                        };
                    }
                    true
                };
                let needs_walk = read.sessions.iter().any(|session| {
                    !session.finished
                        && (now.signed_duration_since(session.last)
                            >= ChronoDuration::minutes(harness::HARNESS_QUIET_MINUTES)
                            || unlinked(session))
                });
                if paths.sessions.exists() {
                    // A recorded total goes only when its session's file has
                    // left the store; the window and a failed read keep it.
                    state.prune_harness_recorded(&paths.sessions);
                }
                let recorded = state.harness_recorded.clone();
                let mut claude_cwd: BTreeMap<String, String> = state
                    .sessions
                    .iter()
                    .map(|(session, seen)| (session.clone(), seen.cwd.clone()))
                    .collect();
                for session in &running.sessions {
                    claude_cwd.insert(session.session_id.clone(), session.cwd.clone());
                }
                (
                    read.sessions,
                    links.owners,
                    claude_cwd,
                    recorded,
                    needs_walk,
                )
            }
            None => (
                Vec::new(),
                BTreeMap::new(),
                BTreeMap::new(),
                BTreeMap::new(),
                false,
            ),
        };

    // One walk of the process table serves both stores: the Harness locks and
    // owners, and the files a process holds open. A Codex session needs it only
    // when its last event is old enough that a holder is what makes it running.
    let codex_walk = live_sessions::codex_needs_process_walk(&codex_sessions, now);
    let mut harness_locks = BTreeSet::new();
    let mut codex_open_files = BTreeSet::new();
    if harness_walk || codex_walk {
        let claude_pids: BTreeMap<u32, String> = running
            .sessions
            .iter()
            .map(|session| (session.pid, session.session_id.clone()))
            .collect();
        // Without a Harness store only the open files are collected; Codex
        // still needs them.
        let found = match &memory.harness_paths {
            Some(paths) => harness::proc_links(paths, procs, &claude_pids),
            None => harness::proc_links_in(None, procs, &claude_pids),
        };
        for warning in &found.warnings {
            add_warning(&mut summary, warning.clone());
        }
        // A walk that Codex alone needed does not change a Harness row: its
        // links are used only when its own running rule asked for the walk.
        if harness_walk {
            for (id, owner) in found.owners {
                // A transcript's owner wins over the process link.
                harness_owners.entry(id).or_insert(owner);
            }
            harness_locks = found.locks;
        }
        codex_open_files = found.files;
    }

    // The rows of this host, and the report the other hosts read. The session
    // files alone are enough to build them: a history that cannot be read gives
    // the projects' default names and says why the values are missing, so a
    // waiting session still notifies.
    let mut currency = None;
    let harness = HarnessInputs {
        sessions: &harness_sessions,
        owners: &harness_owners,
        claude_cwd: &claude_cwd,
        locks: &harness_locks,
        recorded: &harness_recorded,
    };
    let (mut rows, live) = match recorder {
        Some(recorder) => {
            let mut context = memory.rows_context.take().unwrap_or_default();
            let (rows, live) = {
                let inputs = prepare_rows_context(
                    &mut context,
                    app,
                    recorder.dir(),
                    live_sessions::records_from(&running.sessions, state),
                    now,
                    &mut summary,
                );
                currency = inputs.currency.clone();
                live_rows(
                    &running.sessions,
                    state,
                    memory,
                    &agents_by_session,
                    &inputs,
                    &unwritten,
                    recorder.dir().host(),
                    now,
                    &harness,
                    &codex_sessions,
                    &codex_open_files,
                )
            };
            memory.rows_context = Some(context);
            (Some(rows), live)
        }
        None => {
            // Without a history nothing is recorded: a running row counts its
            // transcripts, and its values are unknown with the reason.
            let reason = memory.history_error.clone().unwrap_or_else(|| {
                "the history is not open, so the values are not known".to_string()
            });
            let inputs = RowsInputs {
                projects: None,
                pricing: live_sessions::Pricing::Unknown {
                    reason: reason.clone(),
                },
                currency: None,
                recorded: Err(reason),
            };
            let (rows, live) = live_rows(
                &running.sessions,
                state,
                memory,
                &agents_by_session,
                &inputs,
                &unwritten,
                "",
                now,
                &harness,
                &codex_sessions,
                &codex_open_files,
            );
            (Some(rows), live)
        }
    };
    let LiveParts {
        tokens: harness_tokens,
        recorded: harness_recorded,
        linked: harness_agent_counts,
    } = live;
    // The Codex lines are the growth of each rollout's token_count lines since
    // the recorded totals, per 5-minute bucket and model. They are written
    // below in the Harness order, through the same sequence.
    let codex_recorded = {
        let mut recorded = state.codex_recorded.clone();
        let built = if codex_sessions.is_empty() {
            codex_sessions::CodexLines::default()
        } else {
            codex_sessions::codex_token_lines(&codex_sessions, &mut recorded, &memory.home, since)
        };
        for warning in built.warnings {
            add_warning(&mut summary, warning);
        }
        (built.lines, recorded)
    };
    // Item 6: the status line's cost of a Claude Code session is its API value,
    // and a model without a price is asked for (items 4 and 6).
    if let Some(rows) = rows.as_mut() {
        apply_provider_costs(app, rows, memory, now, &mut summary);
    }

    // One record per running session whose summary changed and whose last
    // record is old enough (or which has none yet). The linked flash/Harness
    // agents are the ones `live_rows` just added to the running row.
    for session in &running.sessions {
        let Some(seen) = state.sessions.get(&session.session_id) else {
            continue;
        };
        let agents = agents_by_session
            .get(&session.session_id)
            .map(Vec::as_slice)
            .unwrap_or(&[]);
        let linked = harness_agent_counts
            .get(&session.session_id)
            .copied()
            .unwrap_or_default();
        let record = match session_record(
            SessionAgents {
                metas: agents,
                linked,
            },
            &session.session_id,
            seen,
            session.entrypoint.clone(),
            false,
            memory,
            now,
        ) {
            Ok(record) => record,
            Err(error) => {
                add_warning(&mut summary, error);
                continue;
            }
        };
        if !record_is_due(memory, &record, now) {
            continue;
        }
        if let Some(recorder) = recorder {
            match recorder.session(&record) {
                Ok(()) => {
                    let last = record.last;
                    memory.records.insert(record.session.clone(), record);
                    memory.closed.remove(&session.session_id);
                    state.set_recorded(
                        &session.session_id,
                        RecordedSession {
                            last,
                            closed: false,
                        },
                    );
                }
                Err(error) => add_history_error(&mut summary, error),
            }
        }
    }

    // Every session seen in a transcript whose session file is not running gets
    // one closed record: a `claude -p` session that started and ended between
    // two wakes, and every session the backfill read. `recorded` remembers what
    // was written, so a restart writes no second record for an unchanged
    // session. The `stopped` list was taken before `forget_closed` removed the
    // sessions from the state.
    for (session, seen) in &stopped {
        let last = seen.last;
        let entrypoint = memory
            .records
            .get(session)
            .and_then(|record| record.entrypoint.clone());
        let agents = transcripts::agents_of(claude, session, &seen.spawns, &mut memory.agent_ends);
        for warning in agents.warnings {
            add_warning(&mut summary, warning);
        }
        let linked = harness_agent_counts
            .get(session)
            .copied()
            .unwrap_or_default();
        let record = match session_record(
            SessionAgents {
                metas: &agents.metas,
                linked,
            },
            session,
            seen,
            entrypoint,
            true,
            memory,
            now,
        ) {
            Ok(record) => record,
            Err(error) => {
                add_warning(&mut summary, error);
                continue;
            }
        };
        if let Some(recorder) = recorder {
            match recorder.session(&record) {
                Ok(()) => {
                    memory.records.insert(session.clone(), record);
                    state.set_recorded(session, RecordedSession { last, closed: true });
                }
                Err(error) => add_history_error(&mut summary, error),
            }
        }
    }

    // A session that left the running list whose transcript entry is already
    // pruned still gets one closing record, with the values of its last record.
    if running.skipped.is_empty() {
        let gone: Vec<String> = memory
            .records
            .keys()
            .filter(|session| {
                !running_ids.contains(*session)
                    && !state.sessions.contains_key(*session)
                    && !memory.closed.contains(*session)
            })
            .cloned()
            .collect();
        for session in gone {
            let Some(last) = memory.records.get(&session).cloned() else {
                continue;
            };
            if last.closed {
                memory.closed.insert(session);
                continue;
            }
            let mut closed = last;
            closed.t = now;
            closed.closed = true;
            if let Some(recorder) = recorder {
                match recorder.session(&closed) {
                    Ok(()) => {
                        memory.records.insert(session.clone(), closed);
                        memory.closed.insert(session);
                    }
                    Err(error) => add_history_error(&mut summary, error),
                }
            }
        }
    }
    // The Harness and Codex tokens reach the history like an agent's: only what
    // grew. The totals are saved before the lines are written, as S-1 saves its
    // scan state before writing: a crash between the two loses a delta, never
    // counts it twice. A state that could not be loaded keeps every line back,
    // so nothing is counted and forgotten.
    if state_error.is_none() {
        write_token_lines(
            recorder,
            state,
            claude,
            app,
            now,
            &harness_tokens,
            &harness_recorded,
            |state| state.harness_recorded.clone(),
            |state, recorded| state.set_harness_recorded(recorded),
            "Harness",
            memory,
            &mut summary,
        );
        write_token_lines(
            recorder,
            state,
            claude,
            app,
            now,
            &codex_recorded.0,
            &codex_recorded.1,
            |state| state.codex_recorded.clone(),
            |state, recorded| state.set_codex_recorded(recorded),
            "Codex",
            memory,
            &mut summary,
        );
    }
    if let Some(rows) = &rows {
        for warning in &rows.warnings {
            add_warning(&mut summary, warning.clone());
        }
        for session in rows
            .sessions
            .iter()
            .filter(|session| session.status == LiveStatus::Waiting)
        {
            summary.waiting.push(Waiting {
                session: session.session.clone(),
                label: session.label.clone(),
                title: session.title.clone(),
                project: session.project.clone(),
                since: session.since,
                host: None,
            });
        }
    }

    // The report follows the rows at most every
    // [`live_sessions::REPORT_EVERY_SECS`], with a heartbeat so another host
    // does not see a quiet host as stale.
    if running.skipped.is_empty() {
        if let (Some(recorder), Some(rows)) = (recorder, rows.as_ref()) {
            write_report_if_due(
                recorder,
                &rows.sessions,
                now,
                closed_now,
                memory,
                &mut summary,
            );
            memory.running_ids = running_ids.clone();
        }
    }

    read_other_hosts(recorder, memory, now, &mut summary);

    // What the view is made of, kept for the commands and the pushes.
    memory.rows = rows;
    memory.this_host = recorder.map(|recorder| recorder.dir().host().to_string());
    memory.at = Some(now);
    memory.currency = currency;
    memory.thinking = running
        .sessions
        .iter()
        .filter_map(|session| {
            state
                .sessions
                .get(&session.session_id)
                .map(|seen| (session.session_id.clone(), seen.thinking_since_user()))
        })
        .collect();
    refresh_closed_rows(app, recorder, memory, now, closed_now, &mut summary);
    memory.unavailable = summary.unavailable.clone();
    memory.transcript_folders = if memory.subscription.is_some() {
        transcript_folders(state, &running.sessions, &claude.projects_dir())
    } else {
        BTreeSet::new()
    };
    publish_view(memory, now);

    let models = models_in_use(&running.sessions, state);
    let (tied, tied_busy, thinking) = tied_sessions(&running.sessions, state);
    summary.tied = tied;
    summary.tied_busy = tied_busy;
    PassOutcome {
        summary,
        models,
        tied_busy,
        thinking,
        wake_soon: !scan_complete,
    }
}

/// Read the other hosts' reports when due, and add their warnings and waiting
/// sessions to the summary. Their reports are read at most every
/// [`live_sessions::REPORT_EVERY_SECS`], like this host writes its own; the
/// last read is used on every pass in between, so a wait and a warning stay put
/// until the next read. A read that fails replaces it with an empty one and its
/// error.
fn read_other_hosts(
    recorder: Option<&Recorder>,
    memory: &mut SessionMemory,
    now: DateTime<Utc>,
    summary: &mut SessionsSummary,
) {
    let Some(recorder) = recorder else {
        return;
    };
    if is_due(
        memory.last_other_hosts_at,
        now,
        live_sessions::REPORT_EVERY_SECS,
    ) {
        memory.last_other_hosts_at = Some(now);
        memory.other_hosts = Some(match live_sessions::other_hosts(recorder.dir(), now) {
            Ok(other) => other,
            Err(error) => live_sessions::OtherHosts {
                hosts: Vec::new(),
                warnings: vec![format!("the other hosts' reports cannot be read: {error}")],
            },
        });
    }
    let Some(other) = &memory.other_hosts else {
        return;
    };
    for warning in &other.warnings {
        add_warning(summary, warning.clone());
    }
    for host in &other.hosts {
        if host.stale {
            continue;
        }
        for session in &host.sessions {
            if session.status != LiveStatus::Waiting {
                continue;
            }
            summary.waiting.push(Waiting {
                session: session.session.clone(),
                label: session.label.clone(),
                title: session.title.clone(),
                project: session.project.clone(),
                since: session.since,
                host: Some(host.host.clone()),
            });
        }
    }
}

/// A pass that cannot read the running sessions still leaves a view, not "no
/// pass yet": this host's running rows are empty (they are not known, never the
/// old ones), while the other hosts' reports and the closed rows are kept. The
/// summary's `unavailable` text is the view's, and the pass time is recorded.
fn pass_without_running(
    app: &AppPaths,
    recorder: Option<&Recorder>,
    memory: &mut SessionMemory,
    now: DateTime<Utc>,
    mut summary: SessionsSummary,
) -> PassOutcome {
    memory.rows = Some(no_rows());
    memory.thinking.clear();
    memory.this_host = recorder.map(|recorder| recorder.dir().host().to_string());
    memory.at = Some(now);
    memory.currency = match recorder {
        Some(recorder) => {
            let mut context = memory.rows_context.take().unwrap_or_default();
            // The settings' currency, the projects and the pricing the closed
            // rows need; the running rows are not built.
            let currency =
                prepare_rows_context(&mut context, app, recorder.dir(), None, now, &mut summary)
                    .currency;
            memory.rows_context = Some(context);
            currency
        }
        None => None,
    };
    read_other_hosts(recorder, memory, now, &mut summary);
    refresh_closed_rows(app, recorder, memory, now, false, &mut summary);
    memory.unavailable = summary.unavailable.clone();
    // No running session is known, so there is no transcript folder to watch.
    memory.transcript_folders.clear();
    publish_view(memory, now);
    PassOutcome {
        summary,
        models: None,
        tied_busy: 0,
        thinking: Vec::new(),
        wake_soon: false,
    }
}

/// The view a `View` command answers: from what the thread holds, without a
/// pass. Its options are the command's own; the subscription's are not touched.
/// Another closed choice reads that choice's closed rows once and keeps them.
fn answer_view(
    memory: &mut SessionMemory,
    options: SessionsOptions,
    app: &AppPaths,
    recorder: Option<&Recorder>,
) -> Result<SessionsView, String> {
    let at = memory
        .at
        .ok_or_else(|| "the sessions thread has not finished a pass yet".to_string())?;
    if let Some(recorder) = recorder {
        read_closed_rows(memory, app, recorder, options.closed, at, false);
    }
    memory
        .view_for(&options)
        .ok_or_else(|| "the sessions thread has not finished a pass yet".to_string())
}

/// Refresh the closed rows of the subscription's choice after a pass, and drop
/// a choice no command has asked for in [`CLOSED_KEEP_SECS`]. `ClosedSince::None`
/// reads nothing.
fn refresh_closed_rows(
    app: &AppPaths,
    recorder: Option<&Recorder>,
    memory: &mut SessionMemory,
    now: DateTime<Utc>,
    closed_now: bool,
    summary: &mut SessionsSummary,
) {
    let choice = memory.subscription.as_ref().map(|options| options.closed);
    let Some(recorder) = recorder else {
        memory.closed_rows.clear();
        return;
    };
    if let Some(choice) = choice {
        read_closed_rows(memory, app, recorder, choice, now, closed_now);
    }
    memory.closed_rows.retain(|_, read| {
        now.signed_duration_since(read.asked_at).num_seconds() < CLOSED_KEEP_SECS
    });
    // The closed rows belong to the window: their failure is the view's
    // `unavailable` (see `SessionMemory::view_for`), not the pop-up's. The
    // history read's warnings are passed on like every other history warning.
    if let Some(read) = choice.and_then(|choice| memory.closed_rows.get(&choice)) {
        for warning in &read.rows.warnings {
            add_warning(summary, warning.clone());
        }
    }
}

/// Read `choice`'s closed rows when they are not held, a running session of this
/// host left the list (`closed_now`), or [`CLOSED_EVERY_SECS`] passed. The
/// result is kept with that choice until its next read or its last ask is
/// [`CLOSED_KEEP_SECS`] old.
fn read_closed_rows(
    memory: &mut SessionMemory,
    app: &AppPaths,
    recorder: &Recorder,
    choice: ClosedSince,
    now: DateTime<Utc>,
    closed_now: bool,
) {
    if choice == ClosedSince::None {
        memory.closed_rows.remove(&ClosedSince::None);
        return;
    }
    let due = match memory.closed_rows.get(&choice) {
        None => true,
        Some(read) => closed_now || is_due(Some(read.read_at), now, CLOSED_EVERY_SECS),
    };
    if !due {
        if let Some(read) = memory.closed_rows.get_mut(&choice) {
            read.asked_at = now;
        }
        return;
    }
    let Some(since) = choice.since(now) else {
        return;
    };
    let projects = memory
        .rows_context
        .as_ref()
        .and_then(|context| context.projects.as_ref())
        .map(|built| built.projects.clone());
    let (rows, failure) = closed_rows_for(app, recorder.dir(), since, now, projects.as_ref());
    memory.closed_rows.insert(
        choice,
        ClosedRows {
            rows,
            read_at: now,
            asked_at: now,
            failure,
        },
    );
}

/// The closed rows over `[since, now]`, with a valuer built for exactly that
/// range. A missing part is `None` / `Pricing::Unknown`, as for the running
/// rows; the error text is the summary's `unavailable`.
fn closed_rows_for(
    app: &AppPaths,
    history: &HistoryDir,
    since: DateTime<Utc>,
    now: DateTime<Utc>,
    projects: Option<&projects::Projects>,
) -> (Rows, Option<String>) {
    let range = Range {
        from: since,
        to: now,
    };
    let file = match store::load_accounts(app) {
        Ok(file) => file,
        Err(error) => {
            let text = format!("the accounts cannot be read: {error}");
            return (no_rows(), Some(text));
        }
    };
    let (failure, rows) = match build_pricing(app, history, &range, &file) {
        Ok((valuer, settings, _, _)) => (
            None,
            live_sessions::closed(
                history,
                since,
                now,
                projects,
                &live_sessions::Pricing::Known {
                    valuer: &valuer,
                    settings: &settings,
                },
            ),
        ),
        Err(text) => (
            Some(text.clone()),
            live_sessions::closed(
                history,
                since,
                now,
                projects,
                &live_sessions::Pricing::Unknown { reason: text },
            ),
        ),
    };
    match rows {
        Ok(rows) => (rows, failure),
        Err(error) => {
            let text = format!("the closed sessions cannot be read: {error}");
            (no_rows(), Some(text))
        }
    }
}

/// No closed rows: the reason is returned beside them, never as a warning.
fn no_rows() -> Rows {
    Rows {
        sessions: Vec::new(),
        warnings: Vec::new(),
    }
}

/// The transcript folders of the running sessions and their agents, from the
/// keys the `ScanState` already knows: `<folder>/<session>.jsonl` and
/// `<folder>/<session>/subagents/agent-<id>.jsonl`.
fn transcript_folders(
    state: &ScanState,
    running: &[SessionFile],
    projects_dir: &Path,
) -> BTreeSet<PathBuf> {
    let runs: HashMap<&str, &str> = running
        .iter()
        .filter_map(|session| {
            state
                .sessions
                .get(&session.session_id)
                .map(|seen| (session.session_id.as_str(), seen.folder.as_str()))
        })
        .collect();
    let mut folders = BTreeSet::new();
    // The main transcript's folder is the project folder the session sees.
    for folder in runs.values() {
        if !folder.is_empty() {
            folders.insert(projects_dir.join(folder));
        }
    }
    // The agents' folders are the deeper keys of the same session.
    for key in state.files.keys() {
        let Some((folder, rest)) = key.split_once('/') else {
            continue;
        };
        let session = rest.split(['/', '.']).next().unwrap_or_default();
        if runs
            .get(session)
            .is_some_and(|expected| *expected == folder)
        {
            if let Some(parent) = Path::new(key).parent() {
                if !parent.as_os_str().is_empty() {
                    folders.insert(projects_dir.join(parent));
                }
            }
        }
    }
    folders
}

/// Push the view after a pass while subscribed: at most one per
/// [`VIEW_EVERY_MS`], with a trailing one, and the minute pass even when
/// nothing changed. Without a subscription nothing is sent.
fn publish_view(memory: &mut SessionMemory, now: DateTime<Utc>) {
    if memory.subscription.is_none() {
        return;
    }
    let Some(view) = memory.view() else {
        return;
    };
    let changed = memory
        .last_pushed
        .as_ref()
        .is_none_or(|last| !same_pushed(last, &view));
    if !memory.throttle.due(now, changed) {
        return;
    }
    memory.last_pushed = Some(view.clone());
    if let Some(ui) = &memory.ui {
        let _ = ui.send(UiEvent::SessionsView(view));
    }
}

/// Whether two pushed views hold the same facts. The times move with every
/// pass and are not a change (the minute pass covers them).
fn same_pushed(first: &SessionsView, second: &SessionsView) -> bool {
    first.this_host == second.this_host
        && first.sessions == second.sessions
        && first.switch_effect == second.switch_effect
        && first.currency == second.currency
        && first.unavailable == second.unavailable
        && first.hosts.len() == second.hosts.len()
        && first
            .hosts
            .iter()
            .zip(&second.hosts)
            .all(|(a, b)| a.host == b.host && a.stale == b.stale && a.this == b.this)
}

/// The linked agents of one session: what `add_agents` counts on a running row.
#[derive(Debug, Clone, Copy, Default)]
struct AgentCounts {
    started: usize,
    finished: usize,
}

/// The parts of a pass's rows that are not the rows themselves.
struct LiveParts {
    tokens: Vec<TokenRecord>,
    recorded: BTreeMap<String, HarnessRecorded>,
    linked: BTreeMap<String, AgentCounts>,
}

/// What one session record counts: Claude Code's agent metas and the linked
/// flash/Harness agents.
struct SessionAgents<'a> {
    metas: &'a [transcripts::AgentMeta],
    linked: AgentCounts,
}

/// The Harness store's part of a pass that the rows are built from.
struct HarnessInputs<'a> {
    sessions: &'a [HarnessSession],
    owners: &'a BTreeMap<String, String>,
    claude_cwd: &'a BTreeMap<String, String>,
    locks: &'a BTreeSet<String>,
    recorded: &'a BTreeMap<String, HarnessRecorded>,
}

/// Lines this host just wrote to the history: the running sessions' token
/// records keep them, so the next pass counts them without reading the history
/// again.
fn recorded_lines_written(memory: &mut SessionMemory, lines: &[TokenRecord]) {
    if let Some(built) = memory
        .rows_context
        .as_mut()
        .and_then(|context| context.recorded.as_mut())
    {
        built.tokens.add(lines);
    }
}

/// Write one store's token lines in the one order both stores use: save the
/// recorded totals, write the lines, and put the old totals back when either
/// step fails, so the next pass writes the same delta and none is lost. A pass
/// with no line saves the totals at once: there is no line to lose. Without a
/// recorder nothing is written and no total advances.
#[allow(clippy::too_many_arguments)]
fn write_token_lines<T: Clone + PartialEq>(
    recorder: Option<&Recorder>,
    state: &mut ScanState,
    claude: &ClaudePaths,
    app: &AppPaths,
    now: DateTime<Utc>,
    tokens: &[TokenRecord],
    recorded: &BTreeMap<String, T>,
    get: impl Fn(&ScanState) -> BTreeMap<String, T>,
    set: impl Fn(&mut ScanState, &BTreeMap<String, T>),
    what: &str,
    memory: &mut SessionMemory,
    summary: &mut SessionsSummary,
) {
    if tokens.is_empty() {
        set(state, recorded);
        return;
    }
    let Some(recorder) = recorder else {
        return;
    };
    let previous = get(state);
    set(state, recorded);
    match state.save(claude, app, now) {
        Ok(_) => {
            if let Err(error) = recorder.tokens(tokens) {
                add_history_error(summary, error);
                // The lines are not written: put the old totals back on disk,
                // as the transcript scan puts its offsets back, so the next
                // pass writes this delta and none is lost.
                set(state, &previous);
                if let Err(error) = state.save(claude, app, now) {
                    add_warning(
                        summary,
                        format!("the {what} token totals cannot be written back: {error}"),
                    );
                }
            } else {
                recorded_lines_written(memory, tokens);
            }
        }
        Err(error) => {
            // Nothing was written: put the old totals back, so this pass
            // writes no line and the next one tries again.
            set(state, &previous);
            add_warning(
                summary,
                format!("the {what} token totals cannot be written: {error}"),
            );
        }
    }
}

/// The rows of this host from the session files, with the project look-up the
/// thread caches and what [`prepare_rows_context`] built.
///
/// The Harness rows come first: their token lines are written after the rows,
/// so a running Claude Code row counts its linked workers' growth of this pass
/// from them, with `unwritten` (the transcript lines this pass could not
/// write), beside the records the history holds; `running` puts its workers
/// under it without adding their tokens again. Without records the rows count
/// their transcripts ([`live_sessions::RowTokens::Transcripts`]).
#[allow(clippy::too_many_arguments)]
fn live_rows(
    files: &[SessionFile],
    state: &ScanState,
    memory: &mut SessionMemory,
    agents: &HashMap<String, Vec<transcripts::AgentMeta>>,
    inputs: &RowsInputs,
    unwritten: &[TokenRecord],
    host: &str,
    now: DateTime<Utc>,
    harness: &HarnessInputs,
    codex_sessions: &[CodexSession],
    codex_open_files: &BTreeSet<PathBuf>,
) -> (live_sessions::Rows, LiveParts) {
    let metas = |session: &str| agents.get(session).cloned().unwrap_or_default();
    let reports = std::mem::take(&mut memory.job_reports);
    let project_of = {
        let memory = RefCell::new(&mut *memory);
        move |cwd: &str| memory.borrow_mut().project_for(cwd)
    };
    let projects = inputs.projects;
    let pricing = &inputs.pricing;
    let harness_rows = if harness.sessions.is_empty() {
        None
    } else {
        Some(live_sessions::harness_rows(
            harness.sessions,
            harness.owners,
            harness.claude_cwd,
            harness.recorded,
            harness.locks,
            &reports,
            projects,
            pricing,
            host,
            now,
            &project_of,
        ))
    };
    let mut pending = unwritten.to_vec();
    let no_agents = BTreeMap::new();
    let linked = match &harness_rows {
        Some(out) => {
            pending.extend(out.tokens.iter().cloned());
            &out.agents
        }
        None => &no_agents,
    };
    // Records that cannot be had leave the running rows' values unknown with
    // that reason, never valued from part of their records.
    let row_tokens = match &inputs.recorded {
        Ok(recorded) => live_sessions::RowTokens::Recorded {
            recorded,
            pending: &pending,
        },
        Err(reason) => live_sessions::RowTokens::Transcripts { unwritten, reason },
    };
    let mut rows = live_sessions::running(
        files,
        state,
        &metas,
        linked,
        row_tokens,
        projects,
        pricing,
        host,
        now,
        &project_of,
    );
    let parts = match harness_rows {
        None => LiveParts {
            tokens: Vec::new(),
            recorded: harness.recorded.clone(),
            linked: BTreeMap::new(),
        },
        Some(out) => {
            // The same agents `running` puts on the running row: a session
            // record counts them too, so its agent counts do not change when it
            // closes.
            let linked = out
                .agents
                .iter()
                .map(|(session, agents)| {
                    let started = agents.len();
                    let finished = agents.iter().filter(|agent| !agent.running).count();
                    (session.clone(), AgentCounts { started, finished })
                })
                .collect();
            rows.warnings.extend(out.rows.warnings);
            rows.sessions.extend(out.rows.sessions);
            LiveParts {
                tokens: out.tokens,
                recorded: out.recorded,
                linked,
            }
        }
    };
    if !codex_sessions.is_empty() {
        let codex = live_sessions::codex_rows(
            codex_sessions,
            codex_open_files,
            projects,
            pricing,
            host,
            now,
            &project_of,
        );
        rows.warnings.extend(codex.warnings);
        rows.sessions.extend(codex.sessions);
    }
    memory.job_reports = reports;
    (rows, parts)
}

fn session_record(
    agents: SessionAgents<'_>,
    session: &str,
    seen: &SessionSeen,
    entrypoint: Option<String>,
    closed: bool,
    memory: &mut SessionMemory,
    now: DateTime<Utc>,
) -> Result<SessionRecord, String> {
    let started = count_u32(
        agents.metas.len() + agents.linked.started,
        &format!("the number of agents started in session {session}"),
    )?;
    let finished = count_u32(
        agents.metas.iter().filter(|agent| agent.is_done()).count() + agents.linked.finished,
        &format!("the number of agents finished in session {session}"),
    )?;
    Ok(SessionRecord {
        t: now,
        session: session.to_string(),
        cwd: seen.cwd.clone(),
        project: memory.project_for(&seen.cwd),
        entrypoint,
        title: seen.shown_title().cloned(),
        first: seen.first,
        last: seen.last,
        agents_started: started,
        agents_finished: finished,
        closed,
    })
}

/// A count that does not fit in 32 bits is an error, never `u32::MAX`.
fn count_u32(count: usize, what: &str) -> Result<u32, String> {
    u32::try_from(count)
        .map_err(|_| format!("{what} ({count}) does not fit in 32 bits; it is not written"))
}

/// A record is written when none exists yet, or when the summary differs from
/// the last one written and that line is at least 10 minutes old. `t` is not
/// part of the comparison.
fn record_is_due(memory: &SessionMemory, record: &SessionRecord, now: DateTime<Utc>) -> bool {
    match memory.records.get(&record.session) {
        None => true,
        Some(last) => {
            !same_summary(last, record)
                && now.signed_duration_since(last.t).num_seconds() >= RECORD_EVERY_SECS
        }
    }
}

fn same_summary(first: &SessionRecord, second: &SessionRecord) -> bool {
    first.session == second.session
        && first.cwd == second.cwd
        && first.project == second.project
        && first.entrypoint == second.entrypoint
        && first.title == second.title
        && first.first == second.first
        && first.last == second.last
        && first.agents_started == second.agents_started
        && first.agents_finished == second.agents_finished
        && first.closed == second.closed
}

/// The models of the running sessions that have one (lower-case ids). A session
/// without an answer yet adds nothing; only a pass that cannot read the session
/// files has no set at all.
fn models_in_use(running: &[SessionFile], state: &ScanState) -> Option<BTreeSet<String>> {
    let mut models = BTreeSet::new();
    for session in running {
        let Some(model) = state
            .sessions
            .get(&session.session_id)
            .and_then(|seen| seen.last_model.as_deref())
        else {
            continue;
        };
        models.insert(model.to_lowercase());
    }
    Some(models)
}

/// The sessions whose model ties its thinking to the account, and of those the
/// ones busy now (spec §2.11), and every tied session's answers with thinking
/// since its user turn, one session's list after the other (spec §2.12).
fn tied_sessions(
    running: &[SessionFile],
    state: &ScanState,
) -> (u32, u32, Vec<(DateTime<Utc>, u32)>) {
    let mut tied = 0u32;
    let mut busy = 0u32;
    let mut thinking = Vec::new();
    for session in running {
        let Some(seen) = state.sessions.get(&session.session_id) else {
            continue;
        };
        if !seen
            .last_model
            .as_deref()
            .is_some_and(seatswap_core::burn::tied::thinking_tied)
        {
            continue;
        }
        tied += 1;
        thinking.extend(seen.thinking.iter().copied());
        if session.status == SessionStatus::Busy && seen.thinking_since_user() > 0 {
            busy += 1;
        }
    }
    (tied, busy, thinking)
}

/// Write this host's report when the rows changed and the last one is at least
/// [`live_sessions::REPORT_EVERY_SECS`] old, when a session just closed, or when
/// the last report is older than [`REPORT_HEARTBEAT_SECS`] even though nothing
/// changed. The time is passed in, so tests drive it without sleeping.
fn write_report_if_due(
    recorder: &Recorder,
    sessions: &[LiveSession],
    now: DateTime<Utc>,
    closed_now: bool,
    memory: &mut SessionMemory,
    summary: &mut SessionsSummary,
) {
    let changed = memory.last_rows.as_deref() != Some(sessions);
    let due = closed_now
        || is_due(memory.last_report_at, now, REPORT_HEARTBEAT_SECS)
        || (changed && is_due(memory.last_report_at, now, live_sessions::REPORT_EVERY_SECS));
    if !due {
        return;
    }
    match live_sessions::write_report(recorder.dir(), sessions, now) {
        Ok(_) => {
            memory.last_report_at = Some(now);
            memory.last_rows = Some(sessions.to_vec());
        }
        Err(error) => add_history_error(summary, error),
    }
}

fn add_unavailable(summary: &mut SessionsSummary, text: String) {
    summary.unavailable = Some(match summary.unavailable.take() {
        Some(previous) => format!("{previous}; {text}"),
        None => text,
    });
}

fn add_warning(summary: &mut SessionsSummary, text: String) {
    summary.warning = Some(match summary.warning.take() {
        Some(previous) => format!("{previous}; {text}"),
        None => text,
    });
}

/// Put a warning before the ones already on the summary: the history error is
/// shown before the state error.
fn prepend_warning(summary: &mut SessionsSummary, text: String) {
    summary.warning = Some(match summary.warning.take() {
        Some(next) => format!("{text}; {next}"),
        None => text,
    });
}

fn add_history_error(summary: &mut SessionsSummary, error: seatswap_core::history::HistoryError) {
    add_warning(summary, ops::history_unwritable(&error));
}

#[cfg(test)]
mod tests {
    use super::*;

    use std::cell::Cell;
    use std::fs;
    use std::time::UNIX_EPOCH;

    use seatswap_core::history::{Event, Hosts, Range};
    use seatswap_core::store::Settings;
    use tempfile::TempDir;

    const SESSION: &str = "7c1e4b2a-9d3f-4e6a-b8c5-2f0a1d7e9b34";

    struct FakeProbe(HashMap<u32, String>);

    impl Liveness for FakeProbe {
        fn alive(&self, file: &SessionFile) -> bool {
            self.0.get(&file.pid) == Some(&file.proc_start)
        }
    }

    /// A process table with no processes: the tests that do not link a Harness
    /// job keep production code free of a test-only branch.
    struct NoProc;

    impl harness::ProcSource for NoProc {
        fn pids(&self) -> std::io::Result<Vec<u32>> {
            Ok(Vec::new())
        }

        fn open_files(&self, _pid: u32) -> std::io::Result<Vec<PathBuf>> {
            Ok(Vec::new())
        }

        fn parent(&self, _pid: u32) -> Option<u32> {
            None
        }
    }

    fn time(text: &str) -> DateTime<Utc> {
        DateTime::parse_from_rfc3339(text)
            .unwrap()
            .with_timezone(&Utc)
    }

    fn fixture() -> (TempDir, ClaudePaths, AppPaths) {
        let tmp = tempfile::tempdir().unwrap();
        let config = tmp.path().join("claude");
        fs::create_dir_all(&config).unwrap();
        let claude = ClaudePaths::for_config_dir(&config, tmp.path());
        let app = AppPaths::at(&tmp.path().join("data"));
        fs::create_dir_all(&app.data_dir).unwrap();
        fs::create_dir_all(claude.sessions_dir()).unwrap();
        (tmp, claude, app)
    }

    fn session_json(
        pid: u32,
        session_id: &str,
        cwd: &str,
        name: Option<&str>,
        status: &str,
        status_updated_at: i64,
    ) -> String {
        let mut value = serde_json::json!({
            "pid": pid,
            "sessionId": session_id,
            "cwd": cwd,
            "startedAt": 1_794_612_960_232_i64,
            "procStart": format!("proc-{pid}"),
            "version": "2.1.322",
            "kind": "interactive",
            "entrypoint": "cli",
            "updatedAt": status_updated_at,
            "status": status,
            "statusUpdatedAt": status_updated_at,
        });
        if let Some(name) = name {
            value["name"] = serde_json::json!(name);
        }
        value.to_string()
    }

    fn transcript_line(
        session: &str,
        id: &str,
        at: DateTime<Utc>,
        cwd: &str,
        model: &str,
        usage: (u64, u64, u64, u64),
    ) -> String {
        serde_json::json!({
            "type": "assistant",
            "uuid": "0b6f4c1a-2d3e-4f50-8a9b-0c1d2e3f4a5b",
            "parentUuid": "91c2a7b3-4c5d-4e6f-8a90-1b2c3d4e5f60",
            "sessionId": session,
            "timestamp": at.to_rfc3339_opts(chrono::SecondsFormat::Millis, true),
            "cwd": cwd,
            "gitBranch": "main",
            "isSidechain": false,
            "requestId": format!("req_{id}"),
            "version": "2.1.322",
            "message": {
                "id": id,
                "type": "message",
                "role": "assistant",
                "model": model,
                "content": [{"type": "text", "text": "fix the failing tests"}],
                "stop_reason": "end_turn",
                "usage": {
                    "input_tokens": usage.0,
                    "output_tokens": usage.1,
                    "cache_creation_input_tokens": usage.2,
                    "cache_read_input_tokens": usage.3,
                    "service_tier": "standard",
                }
            }
        })
        .to_string()
    }

    fn write_session(claude: &ClaudePaths, pid: u32, text: &str) {
        fs::write(claude.sessions_dir().join(format!("{pid}.json")), text).unwrap();
    }

    fn probe(pids: &[u32]) -> FakeProbe {
        FakeProbe(
            pids.iter()
                .map(|pid| (*pid, format!("proc-{pid}")))
                .collect(),
        )
    }

    fn history(root: &Path) -> seatswap_core::history::HistoryDir {
        seatswap_core::history::HistoryDir::at(root, "atlas")
    }

    fn repo(home: &Path, name: &str) -> std::path::PathBuf {
        let root = home.join("projects").join(name);
        fs::create_dir_all(root.join(".git")).unwrap();
        fs::write(
            root.join(".git").join("config"),
            format!("[remote \"origin\"]\n\turl = https://github.com/sam/{name}.git\n"),
        )
        .unwrap();
        root
    }

    #[test]
    fn summary_counts_running_busy_and_waiting() {
        let (tmp, claude, app) = fixture();
        let history = history(&tmp.path().join("history"));
        let recorder = Recorder::new(history);
        let waiting_since = time("2026-11-14T09:56:00Z");
        write_session(
            &claude,
            100,
            &session_json(
                100,
                "11111111-1111-4111-8111-111111111111",
                "/home/sam/projects/atlas",
                Some("atlas-busy"),
                "busy",
                waiting_since.timestamp_millis(),
            ),
        );
        write_session(
            &claude,
            101,
            &session_json(
                101,
                "22222222-2222-4222-8222-222222222222",
                "/home/sam/projects/ledger",
                Some("ledger-idle"),
                "idle",
                waiting_since.timestamp_millis(),
            ),
        );
        write_session(
            &claude,
            102,
            &session_json(
                102,
                "33333333-3333-4333-8333-333333333333",
                "/home/sam/projects/atlas",
                Some("atlas-waiting"),
                "waiting",
                waiting_since.timestamp_millis(),
            ),
        );
        let mut state = ScanState::default();
        let mut memory = SessionMemory::new(tmp.path().to_path_buf());
        let outcome = run_pass(
            &app,
            &claude,
            &probe(&[100, 101, 102]),
            &NoProc,
            Some(&recorder),
            &mut state,
            &mut memory,
            time("2026-11-14T10:00:00Z"),
            None,
        );
        assert_eq!(outcome.summary.running, 3);
        assert_eq!(outcome.summary.busy, 1);
        assert_eq!(outcome.summary.waiting.len(), 1);
        assert_eq!(outcome.summary.waiting[0].since, waiting_since);
    }

    #[test]
    fn a_session_without_a_name_gets_project_and_two_hex() {
        let (tmp, claude, app) = fixture();
        let history = history(&tmp.path().join("history"));
        let recorder = Recorder::new(history);
        let cwd = repo(tmp.path(), "atlas");
        let status_at = time("2026-11-14T09:56:00Z");
        write_session(
            &claude,
            200,
            &session_json(
                200,
                SESSION,
                cwd.to_str().unwrap(),
                None,
                "waiting",
                status_at.timestamp_millis(),
            ),
        );
        let mut state = ScanState::default();
        let mut memory = SessionMemory::new(tmp.path().to_path_buf());
        let outcome = run_pass(
            &app,
            &claude,
            &probe(&[200]),
            &NoProc,
            Some(&recorder),
            &mut state,
            &mut memory,
            time("2026-11-14T10:00:00Z"),
            None,
        );
        assert_eq!(outcome.summary.waiting.len(), 1);
        assert_eq!(outcome.summary.waiting[0].label, "atlas-7c");
        assert_eq!(outcome.summary.waiting[0].project, "atlas");
    }

    #[test]
    fn a_wait_that_ends_is_recorded() {
        let (tmp, claude, _app) = fixture();
        let history = history(&tmp.path().join("history"));
        let recorder = Recorder::new(seatswap_core::history::HistoryDir::at(
            history.root(),
            "atlas",
        ));
        let since = time("2026-11-14T09:56:00Z");
        write_session(
            &claude,
            300,
            &session_json(
                300,
                SESSION,
                "/home/sam/projects/atlas",
                Some("atlas-a6"),
                "waiting",
                since.timestamp_millis(),
            ),
        );
        let mut state = ScanState::default();
        let mut memory = SessionMemory::new(tmp.path().to_path_buf());
        run_pass(
            &_app,
            &claude,
            &probe(&[300]),
            &NoProc,
            Some(&recorder),
            &mut state,
            &mut memory,
            since,
            None,
        );

        let later = time("2026-11-14T10:06:00Z");
        write_session(
            &claude,
            300,
            &session_json(
                300,
                SESSION,
                "/home/sam/projects/atlas",
                Some("atlas-a6"),
                "busy",
                later.timestamp_millis(),
            ),
        );
        run_pass(
            &_app,
            &claude,
            &probe(&[300]),
            &NoProc,
            Some(&recorder),
            &mut state,
            &mut memory,
            later,
            None,
        );

        let read = history
            .read::<Event>(
                &Range {
                    from: time("2026-11-01T00:00:00Z"),
                    to: time("2026-12-01T00:00:00Z"),
                },
                &Hosts::One("atlas".to_string()),
            )
            .unwrap();
        let waited: Vec<&EventKind> = read
            .records
            .iter()
            .map(|hosted| &hosted.record.kind)
            .filter(|kind| matches!(kind, EventKind::Waited { .. }))
            .collect();
        assert_eq!(waited.len(), 1);
        match waited[0] {
            EventKind::Waited { secs, .. } => assert_eq!(*secs, 600),
            other => panic!("expected a waited event, got {other:?}"),
        }
    }

    /// A session file that cannot be parsed (Claude Code rewrote it and the
    /// thread read it half-written) must not look like a session that ended:
    /// the thread cannot tell, so it must not write a closing record.
    #[test]
    fn a_session_file_that_cannot_be_read_does_not_close_the_session() {
        let (tmp, claude, _app) = fixture();
        let history = history(&tmp.path().join("history"));
        let recorder = Recorder::new(seatswap_core::history::HistoryDir::at(
            history.root(),
            "atlas",
        ));
        let first = time("2026-11-14T09:56:00Z");
        write_session(
            &claude,
            500,
            &session_json(
                500,
                SESSION,
                "/home/sam/projects/atlas",
                Some("atlas-a6"),
                "busy",
                first.timestamp_millis(),
            ),
        );
        let mut state = ScanState::default();
        state.sessions.insert(
            SESSION.to_string(),
            SessionSeen {
                folder: "-home-sam-projects-atlas".to_string(),
                cwd: "/home/sam/projects/atlas".to_string(),
                first,
                last: first,
                last_model: Some("claude-opus-5-5".to_string()),
                last_user_turn: None,
                thinking: Vec::new(),
                totals: Default::default(),
                by_model: Default::default(),
                agents: Default::default(),
                spawns: Default::default(),
                recent: Default::default(),
                recorded: None,
                flash_jobs: BTreeSet::new(),
                title: None,
                custom_title: None,
            },
        );
        let mut memory = SessionMemory::new(tmp.path().to_path_buf());
        run_pass(
            &_app,
            &claude,
            &probe(&[500]),
            &NoProc,
            Some(&recorder),
            &mut state,
            &mut memory,
            first,
            None,
        );

        // The file is half-written: it does not parse any more.
        fs::write(claude.sessions_dir().join("500.json"), "{\"pid\":500,").unwrap();
        let second = time("2026-11-14T10:00:00Z");
        run_pass(
            &_app,
            &claude,
            &probe(&[500]),
            &NoProc,
            Some(&recorder),
            &mut state,
            &mut memory,
            second,
            None,
        );

        let file = tmp
            .path()
            .join("history")
            .join("atlas")
            .join("sessions-2026-11.jsonl");
        let text = fs::read_to_string(&file).unwrap();
        let lines: Vec<&str> = text.lines().collect();
        assert_eq!(
            lines.len(),
            1,
            "a file that cannot be read must not close the session: {text}"
        );
    }

    /// The wall clock moved backwards between the start of the wait and its
    /// end. The elapsed time is negative and does not fit the event's `secs`;
    /// the thread must not quietly write "0 seconds".
    #[test]
    fn a_wait_that_ends_after_the_clock_moved_backwards_is_not_written_as_zero() {
        let (tmp, claude, _app) = fixture();
        let history = history(&tmp.path().join("history"));
        let recorder = Recorder::new(seatswap_core::history::HistoryDir::at(
            history.root(),
            "atlas",
        ));
        let since = time("2026-11-14T09:56:00Z");
        write_session(
            &claude,
            600,
            &session_json(
                600,
                SESSION,
                "/home/sam/projects/atlas",
                Some("atlas-a6"),
                "waiting",
                since.timestamp_millis(),
            ),
        );
        let mut state = ScanState::default();
        let mut memory = SessionMemory::new(tmp.path().to_path_buf());
        run_pass(
            &_app,
            &claude,
            &probe(&[600]),
            &NoProc,
            Some(&recorder),
            &mut state,
            &mut memory,
            since,
            None,
        );

        // The clock jumps six minutes back; the session is busy again.
        let earlier = time("2026-11-14T09:50:00Z");
        write_session(
            &claude,
            600,
            &session_json(
                600,
                SESSION,
                "/home/sam/projects/atlas",
                Some("atlas-a6"),
                "busy",
                earlier.timestamp_millis(),
            ),
        );
        run_pass(
            &_app,
            &claude,
            &probe(&[600]),
            &NoProc,
            Some(&recorder),
            &mut state,
            &mut memory,
            earlier,
            None,
        );

        let read = history
            .read::<Event>(
                &Range {
                    from: time("2026-11-01T00:00:00Z"),
                    to: time("2026-12-01T00:00:00Z"),
                },
                &Hosts::One("atlas".to_string()),
            )
            .unwrap();
        for hosted in &read.records {
            if let EventKind::Waited { secs, .. } = &hosted.record.kind {
                assert_ne!(
                    *secs, 0,
                    "a wait that ended after the clock moved backwards must not be written as 0 seconds"
                );
            }
        }
    }

    #[test]
    fn a_closed_session_gets_one_closing_record() {
        let (tmp, claude, _app) = fixture();
        let history = history(&tmp.path().join("history"));
        let recorder = Recorder::new(seatswap_core::history::HistoryDir::at(
            history.root(),
            "atlas",
        ));
        let first = time("2026-11-14T09:56:00Z");
        write_session(
            &claude,
            400,
            &session_json(
                400,
                SESSION,
                "/home/sam/projects/atlas",
                Some("atlas-a6"),
                "busy",
                first.timestamp_millis(),
            ),
        );
        let mut state = ScanState::default();
        state.sessions.insert(
            SESSION.to_string(),
            SessionSeen {
                folder: "-home-sam-projects-atlas".to_string(),
                cwd: "/home/sam/projects/atlas".to_string(),
                first,
                last: first,
                last_model: Some("claude-opus-5-5".to_string()),
                last_user_turn: None,
                thinking: Vec::new(),
                totals: Default::default(),
                by_model: Default::default(),
                agents: Default::default(),
                spawns: Default::default(),
                recent: Default::default(),
                recorded: None,
                flash_jobs: BTreeSet::new(),
                title: None,
                custom_title: None,
            },
        );
        let mut memory = SessionMemory::new(tmp.path().to_path_buf());
        run_pass(
            &_app,
            &claude,
            &probe(&[400]),
            &NoProc,
            Some(&recorder),
            &mut state,
            &mut memory,
            first,
            None,
        );

        fs::remove_file(claude.sessions_dir().join("400.json")).unwrap();
        let second = time("2026-11-14T10:00:00Z");
        run_pass(
            &_app,
            &claude,
            &probe(&[400]),
            &NoProc,
            Some(&recorder),
            &mut state,
            &mut memory,
            second,
            None,
        );
        let third = time("2026-11-14T10:02:00Z");
        run_pass(
            &_app,
            &claude,
            &probe(&[400]),
            &NoProc,
            Some(&recorder),
            &mut state,
            &mut memory,
            third,
            None,
        );

        let file = tmp
            .path()
            .join("history")
            .join("atlas")
            .join("sessions-2026-11.jsonl");
        let text = fs::read_to_string(&file).unwrap();
        let lines: Vec<&str> = text.lines().collect();
        assert_eq!(lines.len(), 2, "{text}");
        let last: SessionRecord = serde_json::from_str(lines[1]).unwrap();
        assert!(last.closed);
    }

    /// The order of the pop-up's messages: the history error comes before the
    /// state that cannot be written.
    #[test]
    fn a_history_error_comes_before_a_state_that_cannot_be_written() {
        let (tmp, claude, app) = fixture();
        let folder = claude.projects_dir().join("-home-sam-projects-atlas");
        fs::create_dir_all(&folder).unwrap();
        fs::write(
            folder.join(format!("{SESSION}.jsonl")),
            format!(
                "{}\n",
                serde_json::json!({
                    "type": "user",
                    "timestamp": "2026-11-14T09:56:00Z",
                    "sessionId": SESSION,
                    "cwd": "/home/sam/projects/atlas",
                    "message": {"role": "user", "content": "fix the failing tests"},
                })
            ),
        )
        .unwrap();
        // The state loads while its folder is still a folder. The user line
        // above changes the state and holds no tokens, so the save tries to
        // write that folder without a recorder.
        let (mut state, state_error) = load_state(&app);
        assert_eq!(state_error, None);
        fs::write(app.transcripts_state_dir(), "not a folder").unwrap();

        // A history folder that is a file cannot be opened.
        let history_file = tmp.path().join("history-is-a-file");
        fs::write(&history_file, "").unwrap();
        let settings = Settings {
            host_name: Some("atlas".to_string()),
            history_dir: Some(history_file),
            ..Settings::default()
        };
        let (recorder, history_error) = match ops::open_history(&app, &settings) {
            Ok(recorder) => (Some(recorder), None),
            Err(error) => (None, Some(error)),
        };
        assert!(recorder.is_none(), "the history folder is a file");
        let history_error = history_error.expect("a history folder that is a file is an error");

        let mut memory = SessionMemory::new(claude.home().to_path_buf());
        let outcome = one_pass(
            &app,
            &claude,
            &probe(&[]),
            &NoProc,
            recorder.as_ref(),
            &mut state,
            &mut memory,
            time("2026-11-14T10:00:00Z"),
            state_error.as_deref(),
            Some(history_error.as_str()),
        );
        let warning = outcome.summary.warning.expect("both errors are reported");
        assert!(
            warning.starts_with(&format!(
                "{history_error}; the transcript state cannot be written: "
            )),
            "{warning}"
        );
        assert_eq!(
            outcome.summary.unavailable, None,
            "the counts stay visible: only unreadable session files are unavailable"
        );
    }

    /// The first start reads a projects folder it has never seen and writes one
    /// token line per session, 5-minute bucket and model; a second start, from
    /// the saved state, adds none. The data is made here: three sessions, two
    /// days, about forty answers, two models, several answers in one bucket.
    #[test]
    fn a_backfill_writes_each_bucket_once() {
        use std::collections::BTreeMap;

        const LEDGER: &str = "2b7d9c1e-5a4f-4c3b-9e8d-7f6a5b4c3d2e";
        const ACME_KB: &str = "4f8a1c3e-6b2d-4a5c-9e7f-1a3b5c7d9e20";
        const OPUS: &str = "claude-opus-5-5";
        const SONNET: &str = "claude-sonnet-5-5";

        #[derive(Debug, Default, Clone, PartialEq, Eq)]
        struct Totals {
            input: u64,
            output: u64,
            cache_write: u64,
            cache_read: u64,
            answers: u32,
        }

        impl Totals {
            fn add(&mut self, usage: (u64, u64, u64, u64)) {
                self.input += usage.0;
                self.output += usage.1;
                self.cache_write += usage.2;
                self.cache_read += usage.3;
                self.answers += 1;
            }
        }

        fn start_of_bucket(at: DateTime<Utc>) -> DateTime<Utc> {
            seatswap_core::transcripts::bucket_start(at)
        }

        fn project_folder(cwd: &Path) -> String {
            cwd.to_string_lossy()
                .chars()
                .map(|c| if c.is_ascii_alphanumeric() { c } else { '-' })
                .collect()
        }

        fn files_under(root: &Path) -> BTreeMap<std::path::PathBuf, Vec<u8>> {
            let mut files = BTreeMap::new();
            let mut folders = vec![root.to_path_buf()];
            while let Some(folder) = folders.pop() {
                let entries = match fs::read_dir(&folder) {
                    Ok(entries) => entries,
                    Err(error) if error.kind() == std::io::ErrorKind::NotFound => continue,
                    Err(error) => panic!("{}: {error}", folder.display()),
                };
                for entry in entries {
                    let path = entry.unwrap().path();
                    if path.is_dir() {
                        folders.push(path);
                    } else if path.file_name().and_then(|name| name.to_str())
                        != Some("sessions-now.json")
                    {
                        // The live report is not a history line: it is rewritten
                        // each time this host's rows are reported.
                        files.insert(path.clone(), fs::read(&path).unwrap());
                    }
                }
            }
            files
        }

        fn token_lines(root: &Path) -> usize {
            files_under(root)
                .iter()
                .filter(|(path, _)| {
                    path.file_name()
                        .and_then(|name| name.to_str())
                        .is_some_and(|name| name.starts_with("tokens-") && name.ends_with(".jsonl"))
                })
                .map(|(_, bytes)| bytes.iter().filter(|byte| **byte == b'\n').count())
                .sum()
        }

        let (tmp, claude, app) = fixture();
        let mut cwds = HashMap::new();
        for name in ["atlas", "ledger", "acme-kb"] {
            cwds.insert(name, repo(tmp.path(), name));
        }

        // (project, session, first answer, model, answers from that time)
        let groups: [(&str, &str, &str, &str, u32); 15] = [
            ("atlas", SESSION, "2026-11-12T09:10:00Z", OPUS, 3),
            ("atlas", SESSION, "2026-11-12T09:30:00Z", SONNET, 2),
            ("atlas", SESSION, "2026-11-12T10:05:00Z", OPUS, 2),
            ("atlas", SESSION, "2026-11-12T14:20:00Z", SONNET, 3),
            ("atlas", SESSION, "2026-11-13T08:10:00Z", OPUS, 4),
            ("atlas", SESSION, "2026-11-13T09:15:00Z", SONNET, 3),
            ("atlas", SESSION, "2026-11-13T12:30:00Z", OPUS, 3),
            ("ledger", LEDGER, "2026-11-12T11:05:00Z", OPUS, 4),
            ("ledger", LEDGER, "2026-11-12T13:20:00Z", SONNET, 3),
            ("ledger", LEDGER, "2026-11-13T10:10:00Z", SONNET, 3),
            ("ledger", LEDGER, "2026-11-13T11:40:00Z", OPUS, 4),
            ("acme-kb", ACME_KB, "2026-11-12T09:05:00Z", SONNET, 2),
            ("acme-kb", ACME_KB, "2026-11-12T16:20:00Z", OPUS, 3),
            ("acme-kb", ACME_KB, "2026-11-13T08:40:00Z", OPUS, 4),
            ("acme-kb", ACME_KB, "2026-11-13T09:50:00Z", SONNET, 3),
        ];

        let mut lines: HashMap<&str, Vec<String>> = HashMap::new();
        let mut expected: BTreeMap<(String, DateTime<Utc>, String), Totals> = BTreeMap::new();
        let mut per_session: BTreeMap<String, Totals> = BTreeMap::new();
        let mut sequence = 0u32;
        for (project, session, start, model, count) in groups {
            let cwd = cwds[project].to_str().unwrap().to_string();
            for answer in 0..count {
                let at = time(start) + chrono::Duration::minutes(i64::from(answer) * 3);
                // Input in the thousands, cache reads in the tens of thousands,
                // output in the hundreds.
                let usage = (
                    2_400 + u64::from(sequence) * 91,
                    260 + u64::from(sequence) * 7,
                    17_000 + u64::from(sequence) * 173,
                    31_000 + u64::from(sequence) * 257,
                );
                expected
                    .entry((session.to_string(), start_of_bucket(at), model.to_string()))
                    .or_default()
                    .add(usage);
                per_session
                    .entry(session.to_string())
                    .or_default()
                    .add(usage);
                lines.entry(session).or_default().push(transcript_line(
                    session,
                    &format!("msg_{sequence:04}"),
                    at,
                    &cwd,
                    model,
                    usage,
                ));
                sequence += 1;
            }
        }
        assert_eq!(sequence, 46, "the backfill has about forty answers");

        for (session, session_lines) in &lines {
            let cwd = groups
                .iter()
                .find(|group| group.1 == *session)
                .map(|group| &cwds[group.0])
                .unwrap();
            let folder = claude.projects_dir().join(project_folder(cwd));
            fs::create_dir_all(&folder).unwrap();
            fs::write(
                folder.join(format!("{session}.jsonl")),
                format!("{}\n", session_lines.join("\n")),
            )
            .unwrap();
        }

        let history_root = tmp.path().join("history");
        let history_dir = history(&history_root);
        let first_now = time("2026-11-13T13:00:00Z");
        let (mut state, state_error) = load_state(&app);
        let mut memory = SessionMemory::new(claude.home().to_path_buf());
        let recorder = Recorder::new(history(&history_root));
        let outcome = one_pass(
            &app,
            &claude,
            &probe(&[]),
            &NoProc,
            Some(&recorder),
            &mut state,
            &mut memory,
            first_now,
            state_error.as_deref(),
            None,
        );
        assert_eq!(outcome.summary.unavailable, None);
        assert_eq!(outcome.summary.warning, None);
        assert_eq!(outcome.summary.running, 0);
        assert_eq!(
            token_lines(&history_root),
            expected.len(),
            "one token line per session, 5-minute bucket and model"
        );

        let read = history_dir
            .read::<seatswap_core::history::TokenRecord>(
                &Range {
                    from: time("2026-11-01T00:00:00Z"),
                    to: time("2026-12-01T00:00:00Z"),
                },
                &Hosts::One("atlas".to_string()),
            )
            .unwrap();
        assert_eq!(read.warnings, Vec::<String>::new());
        assert_eq!(read.records.len(), expected.len());
        let mut seen = HashSet::new();
        let mut got_per_session: BTreeMap<String, Totals> = BTreeMap::new();
        for hosted in &read.records {
            let record = &hosted.record;
            let key = (record.session.clone(), record.t, record.model.clone());
            let want = expected
                .get(&key)
                .unwrap_or_else(|| panic!("a token line for {key:?} that has no answers"));
            assert!(seen.insert(key.clone()), "two token lines for {key:?}");
            assert_eq!(record.input, want.input, "{key:?} input");
            assert_eq!(record.out, want.output, "{key:?} output");
            assert_eq!(record.cw, want.cache_write, "{key:?} cache write");
            assert_eq!(record.cr, want.cache_read, "{key:?} cache read");
            assert_eq!(record.answers, want.answers, "{key:?} answers");
            assert_eq!(record.agent_answers, 0);
            let total = got_per_session.entry(record.session.clone()).or_default();
            total.input += record.input;
            total.output += record.out;
            total.cache_write += record.cw;
            total.cache_read += record.cr;
            total.answers += record.answers;
        }
        assert_eq!(seen.len(), expected.len());
        assert_eq!(got_per_session, per_session);

        let before = files_under(&history_root);
        let (mut state, state_error) = load_state(&app);
        let mut memory = SessionMemory::new(claude.home().to_path_buf());
        let recorder = Recorder::new(history(&history_root));
        let outcome = one_pass(
            &app,
            &claude,
            &probe(&[]),
            &NoProc,
            Some(&recorder),
            &mut state,
            &mut memory,
            time("2026-11-13T14:00:00Z"),
            state_error.as_deref(),
            None,
        );
        assert_eq!(outcome.summary.unavailable, None);
        assert_eq!(
            files_under(&history_root),
            before,
            "a second start adds no line to any history file"
        );
    }

    #[test]
    fn tokens_read_while_the_history_is_closed_are_not_lost() {
        let (tmp, claude, app) = fixture();
        let folder = claude.projects_dir().join("-home-sam-projects-atlas");
        fs::create_dir_all(&folder).unwrap();
        fs::write(
            folder.join(format!("{SESSION}.jsonl")),
            format!(
                "{}\n",
                transcript_line(
                    SESSION,
                    "msg_01QmB7xT",
                    time("2026-11-14T09:41:07Z"),
                    "/home/sam/projects/atlas",
                    "claude-opus-5-5",
                    (2, 166, 26_173, 28_126),
                )
            ),
        )
        .unwrap();

        // First start: the history cannot be opened (say HostTaken), so there
        // is no recorder; the pass still runs and saves the scan state.
        let (mut state, _) = load_state(&app);
        let mut memory = SessionMemory::new(claude.home().to_path_buf());
        one_pass(
            &app,
            &claude,
            &probe(&[]),
            &NoProc,
            None,
            &mut state,
            &mut memory,
            time("2026-11-14T10:00:00Z"),
            None,
            Some("the history cannot be written: host atlas is taken"),
        );

        // The owner fixes hostName and restarts: now there is a recorder.
        let recorder = Recorder::new(history(&tmp.path().join("history")));
        let (mut state, _) = load_state(&app);
        let mut memory = SessionMemory::new(claude.home().to_path_buf());
        one_pass(
            &app,
            &claude,
            &probe(&[]),
            &NoProc,
            Some(&recorder),
            &mut state,
            &mut memory,
            time("2026-11-14T10:05:00Z"),
            None,
            None,
        );

        let read = recorder
            .dir()
            .read::<seatswap_core::history::TokenRecord>(
                &Range {
                    from: time("2026-11-01T00:00:00Z"),
                    to: time("2026-12-01T00:00:00Z"),
                },
                &Hosts::One("atlas".to_string()),
            )
            .unwrap();
        assert_eq!(
            read.records.len(),
            1,
            "the answer read while the history was closed must still reach tokens-…"
        );
    }

    #[test]
    fn a_history_error_keeps_the_counts_visible() {
        let (tmp, claude, app) = fixture();
        write_session(
            &claude,
            100,
            &session_json(
                100,
                SESSION,
                "/home/sam/projects/atlas",
                Some("atlas-a6"),
                "busy",
                time("2026-11-14T09:56:00Z").timestamp_millis(),
            ),
        );
        let recorder = Recorder::new(history(&tmp.path().join("history")));
        let (mut state, _) = load_state(&app);
        let mut memory = SessionMemory::new(claude.home().to_path_buf());
        let outcome = one_pass(
            &app,
            &claude,
            &probe(&[100]),
            &NoProc,
            Some(&recorder),
            &mut state,
            &mut memory,
            time("2026-11-14T10:00:00Z"),
            None,
            Some("the history cannot be written: host atlas is taken"),
        );
        assert_eq!(outcome.summary.running, 1);
        // panel.js renderFooter shows `unavailable` *instead of* the counts.
        assert!(
            outcome.summary.unavailable.is_none(),
            "unavailable hides `1 sessions · 1 busy` in the footer: {:?}",
            outcome.summary.unavailable
        );
        assert_eq!(
            outcome.summary.warning.as_deref(),
            Some("the history cannot be written: host atlas is taken"),
            "the history error is a warning beside the counts"
        );
    }

    #[test]
    fn a_state_that_cannot_be_read_is_a_warning_beside_the_counts() {
        let (_tmp, claude, app) = fixture();
        write_session(
            &claude,
            100,
            &session_json(
                100,
                SESSION,
                "/home/sam/projects/atlas",
                Some("atlas-a6"),
                "busy",
                time("2026-11-14T09:56:00Z").timestamp_millis(),
            ),
        );
        let (mut state, _) = load_state(&app);
        let mut memory = SessionMemory::new(claude.home().to_path_buf());
        let state_error = "the transcript state cannot be read: not a state file";
        let outcome = one_pass(
            &app,
            &claude,
            &probe(&[100]),
            &NoProc,
            None,
            &mut state,
            &mut memory,
            time("2026-11-14T10:00:00Z"),
            Some(state_error),
            None,
        );
        assert_eq!(outcome.summary.running, 1);
        assert_eq!(
            outcome.summary.unavailable, None,
            "the state is not the session files; the counts stay visible"
        );
        assert_eq!(outcome.summary.warning.as_deref(), Some(state_error));
    }

    /// A changed title alone makes the session record due again, like a
    /// changed `last`.
    #[test]
    fn a_new_title_makes_the_session_record_due() {
        let (_tmp, claude, _app) = fixture();
        let now = time("2026-11-16T10:00:00Z");
        let at = time("2026-11-16T09:00:00Z");
        let record = |title: Option<&str>| SessionRecord {
            t: at,
            session: SESSION.to_string(),
            cwd: "/home/sam/projects/atlas".to_string(),
            project: ProjectKey::Folder("atlas".to_string()),
            entrypoint: Some("cli".to_string()),
            title: title.map(str::to_string),
            first: at,
            last: at,
            agents_started: 0,
            agents_finished: 0,
            closed: false,
        };
        let mut memory = SessionMemory::new(claude.home().to_path_buf());
        memory.records.insert(SESSION.to_string(), record(None));

        assert!(
            !record_is_due(&memory, &record(None), now),
            "an unchanged summary is not written again"
        );
        let mut titled = record(Some("Fix the invoice export"));
        titled.t = now;
        assert!(
            record_is_due(&memory, &titled, now),
            "a changed title is a changed summary"
        );
    }

    /// The title a scan reads from a transcript reaches the session record the
    /// app writes.
    #[test]
    fn a_session_record_carries_the_name_claude_code_shows() {
        let (_tmp, claude, _app) = fixture();
        let now = time("2026-11-16T10:00:00Z");
        let folder = claude.projects_dir().join("-home-sam-projects-atlas");
        fs::create_dir_all(&folder).unwrap();
        let transcript = folder.join(format!("{SESSION}.jsonl"));
        let first = transcript_line(
            SESSION,
            "msg_01QmB7xT",
            now - ChronoDuration::minutes(5),
            "/home/sam/projects/atlas",
            "claude-opus-5-5",
            (2, 166, 26_173, 28_126),
        );
        let title = serde_json::json!({
            "type": "ai-title",
            "aiTitle": "Fix the invoice export",
            "sessionId": SESSION,
        })
        .to_string();
        // `/rename` in Claude Code (1.4.4): the record carries the user's name.
        let rename = serde_json::json!({
            "type": "custom-title",
            "customTitle": "tripwire",
            "sessionId": SESSION,
        })
        .to_string();
        fs::write(&transcript, format!("{first}\n{title}\n{rename}\n")).unwrap();
        let mut state = ScanState::default();
        let mut scan = transcripts::scan(&claude, &state, 64 * 1024 * 1024).unwrap();
        assert_eq!(scan.answers.len(), 1);
        scan.commit(&mut state);

        let seen = &state.sessions[SESSION];
        let mut memory = SessionMemory::new(claude.home().to_path_buf());
        let record = session_record(
            SessionAgents {
                metas: &[],
                linked: Default::default(),
            },
            SESSION,
            seen,
            Some("cli".to_string()),
            false,
            &mut memory,
            now,
        )
        .unwrap();

        assert_eq!(record.title.as_deref(), Some("tripwire"));
    }

    /// Review finding 1: upgrading from 1.1.0 must migrate its version-1 scan
    /// state, not rebuild it. The test keeps its old name because
    /// `specs/plan-1.2/PLAN.md` and `specs/plan/tests_audit.py` still list it;
    /// it now proves the offsets are kept and no record is written twice.
    #[test]
    fn an_old_scan_state_is_rebuilt() {
        let (tmp, claude, app) = fixture();
        let now = time("2026-11-16T10:00:00Z");
        let first_at = time("2026-11-16T09:41:07.412Z");
        let folder = claude.projects_dir().join("-home-sam-projects-atlas");
        fs::create_dir_all(&folder).unwrap();
        let transcript = folder.join(format!("{SESSION}.jsonl"));
        let first = transcript_line(
            SESSION,
            "msg_01QmB7xT",
            first_at,
            "/home/sam/projects/atlas",
            "claude-opus-5-5",
            (2, 166, 26_173, 28_126),
        );
        fs::write(&transcript, format!("{first}\n")).unwrap();
        let key = format!("-home-sam-projects-atlas/{SESSION}.jsonl");
        let length = fs::metadata(&transcript).unwrap().len();

        // The history 1.1.0 left for this transcript: its token line and the
        // closed SessionRecord it wrote when the session ended.
        let history_root = tmp.path().join("history");
        let recorder = Recorder::new(history(&history_root));
        let project = ProjectKey::Folder("atlas".to_string());
        let token = TokenRecord {
            t: time("2026-11-16T09:00:00Z"),
            minutes: 60,
            session: SESSION.to_string(),
            cwd: "/home/sam/projects/atlas".to_string(),
            project: project.clone(),
            model: "claude-opus-5-5".to_string(),
            input: 2,
            out: 166,
            cw: 26_173,
            cr: 28_126,
            answers: 1,
            agent_answers: 0,
        };
        recorder.tokens(std::slice::from_ref(&token)).unwrap();
        let first_record = SessionRecord {
            t: first_at,
            session: SESSION.to_string(),
            cwd: "/home/sam/projects/atlas".to_string(),
            project,
            entrypoint: Some("cli".to_string()),
            title: None,
            first: first_at,
            last: first_at,
            agents_started: 0,
            agents_finished: 0,
            closed: true,
        };
        recorder.session(&first_record).unwrap();

        // A state file exactly as 1.1.0 wrote it: the whole transcript was
        // read, the session's sums and its written closing record are kept.
        let mut files = serde_json::Map::new();
        files.insert(
            key.clone(),
            serde_json::json!({
                "offset": length,
                "grew": "2026-11-16T09:41:07.412Z",
                "recent": ["msg_01QmB7xT"],
                "lastAnswer": "2026-11-16T09:41:07.412Z"
            }),
        );
        let old = serde_json::json!({
            "version": 1,
            "files": files,
            "sessions": {
                SESSION: {
                    "folder": "-home-sam-projects-atlas",
                    "cwd": "/home/sam/projects/atlas",
                    "first": "2026-11-16T09:41:07.412Z",
                    "last": "2026-11-16T09:41:07.412Z",
                    "lastModel": "claude-opus-5-5",
                    "lastUserTurn": null,
                    "thinking": [],
                    "totals": {"input": 2, "out": 166, "cw": 26173, "cr": 28126},
                    "byModel": {"claude-opus-5-5": {"input": 2, "out": 166, "cw": 26173, "cr": 28126}},
                    "agents": {},
                    "spawns": {},
                    // Version 1 kept one count per minute, without a model.
                    "recent": [["2026-11-16T09:41:00Z", {"input": 2, "out": 166, "cw": 26173, "cr": 28126}]],
                    "recorded": {"last": "2026-11-16T09:41:07.412Z", "closed": true}
                }
            }
        });
        let state_dir = app.transcripts_state_dir();
        fs::create_dir_all(&state_dir).unwrap();
        let state_file = state_dir.join("-home-sam-projects-atlas.json");
        fs::write(&state_file, old.to_string()).unwrap();

        // One 1.2.0 pass through the thread's own entry point. Nothing already
        // read by 1.1.0 may produce a record again.
        let (mut state, state_error) = load_state(&app);
        assert_eq!(state_error, None, "version 1 is migrated, not an error");
        let mut memory = SessionMemory::new(claude.home().to_path_buf());
        one_pass(
            &app,
            &claude,
            &probe(&[]),
            &NoProc,
            Some(&recorder),
            &mut state,
            &mut memory,
            now,
            None,
            None,
        );
        assert_eq!(
            state.sessions[SESSION].totals.out, 166,
            "the already-read answer is not counted into the state again"
        );
        let tokens = recorder
            .dir()
            .read::<TokenRecord>(
                &Range {
                    from: time("2026-11-01T00:00:00Z"),
                    to: time("2026-12-01T00:00:00Z"),
                },
                &Hosts::One("atlas".to_string()),
            )
            .unwrap();
        assert_eq!(tokens.records.len(), 1, "no token line is written again");
        assert_eq!(tokens.records[0].record.out, 166);
        assert_eq!(
            state.files[&key].offset, length,
            "the state keeps 1.1.0's read offset"
        );
        let sessions = recorder
            .dir()
            .read::<SessionRecord>(
                &Range {
                    from: time("2026-11-01T00:00:00Z"),
                    to: time("2026-12-01T00:00:00Z"),
                },
                &Hosts::One("atlas".to_string()),
            )
            .unwrap();
        assert_eq!(
            sessions.records.len(),
            1,
            "no session record is written again"
        );
        let saved: serde_json::Value =
            serde_json::from_slice(&fs::read(&state_file).unwrap()).unwrap();
        assert_eq!(saved["version"], 2, "the migrated state is written back");
        assert_eq!(saved["files"][&key]["offset"].as_u64(), Some(length));

        // An answer appended after the 1.1.0 state is read once.
        let second_at = time("2026-11-16T09:50:00Z");
        let second = transcript_line(
            SESSION,
            "msg_02QmB7xT",
            second_at,
            "/home/sam/projects/atlas",
            "claude-opus-5-5",
            (3, 200, 0, 0),
        );
        let mut text = fs::read_to_string(&transcript).unwrap();
        text.push_str(&second);
        text.push('\n');
        fs::write(&transcript, text).unwrap();
        let later = now + ChronoDuration::minutes(1);
        let (mut state, state_error) = load_state(&app);
        assert_eq!(state_error, None);
        let mut memory = SessionMemory::new(claude.home().to_path_buf());
        one_pass(
            &app,
            &claude,
            &probe(&[]),
            &NoProc,
            Some(&recorder),
            &mut state,
            &mut memory,
            later,
            None,
            None,
        );
        let tokens = recorder
            .dir()
            .read::<TokenRecord>(
                &Range {
                    from: time("2026-11-01T00:00:00Z"),
                    to: time("2026-12-01T00:00:00Z"),
                },
                &Hosts::One("atlas".to_string()),
            )
            .unwrap();
        assert_eq!(tokens.records.len(), 2, "the new answer is one new line");
        assert_eq!(
            tokens
                .records
                .iter()
                .map(|hosted| hosted.record.out)
                .sum::<u64>(),
            366,
            "the old 166 is not written again"
        );
        assert_eq!(
            tokens
                .records
                .iter()
                .filter(|hosted| hosted.record.out == 200)
                .count(),
            1,
            "the appended answer is recorded once"
        );
        let sessions = recorder
            .dir()
            .read::<SessionRecord>(
                &Range {
                    from: time("2026-11-01T00:00:00Z"),
                    to: time("2026-12-01T00:00:00Z"),
                },
                &Hosts::One("atlas".to_string()),
            )
            .unwrap();
        assert_eq!(
            sessions
                .records
                .iter()
                .filter(|hosted| hosted.record.last == first_at)
                .count(),
            1,
            "the 1.1.0 closing record is not written again"
        );
        assert!(
            sessions
                .records
                .iter()
                .any(|hosted| hosted.record.closed && hosted.record.last == second_at),
            "the appended answer gets its one closing record"
        );
    }

    /// A newer state file is an error naming the file and both versions; the
    /// pass writes nothing, because a rebuild is what doubles the history.
    #[test]
    fn a_state_file_of_a_newer_version_is_an_error_and_writes_no_record() {
        let (tmp, claude, app) = fixture();
        let now = time("2026-11-16T10:00:00Z");
        let folder = claude.projects_dir().join("-home-sam-projects-atlas");
        fs::create_dir_all(&folder).unwrap();
        fs::write(
            folder.join(format!("{SESSION}.jsonl")),
            format!(
                "{}\n",
                transcript_line(
                    SESSION,
                    "msg_01QmB7xT",
                    time("2026-11-16T09:41:07Z"),
                    "/home/sam/projects/atlas",
                    "claude-opus-5-5",
                    (2, 166, 26_173, 28_126)
                )
            ),
        )
        .unwrap();
        let state_dir = app.transcripts_state_dir();
        fs::create_dir_all(&state_dir).unwrap();
        let state_file = state_dir.join("-home-sam-projects-atlas.json");
        let newer = serde_json::json!({
            "version": 3,
            "files": {},
            "sessions": {}
        });
        fs::write(&state_file, newer.to_string()).unwrap();

        let (mut state, error) = load_state(&app);
        let error = error.expect("a version this build does not read is an error");
        assert!(
            error.contains(state_file.to_str().unwrap()),
            "the file is named: {error}"
        );
        assert!(error.contains("version 3"), "the found version: {error}");
        assert!(error.contains("version 2"), "this build's version: {error}");
        assert!(
            error.contains("newer seatswap"),
            "a higher version says why: {error}"
        );
        assert!(state.files.is_empty(), "nothing is read from version 3");

        let history_root = tmp.path().join("history");
        let recorder = Recorder::new(history(&history_root));
        let mut memory = SessionMemory::new(claude.home().to_path_buf());
        one_pass(
            &app,
            &claude,
            &probe(&[]),
            &NoProc,
            Some(&recorder),
            &mut state,
            &mut memory,
            now,
            Some(&error),
            None,
        );

        assert!(
            !history_root
                .join("atlas")
                .join("tokens-2026-11.jsonl")
                .exists(),
            "a state error writes no token line"
        );
        assert!(
            !history_root
                .join("atlas")
                .join("sessions-2026-11.jsonl")
                .exists(),
            "a state error writes no session record"
        );
        assert_eq!(
            fs::read_to_string(&state_file).unwrap(),
            newer.to_string(),
            "the state file is left as it was"
        );
    }

    #[test]
    fn a_short_session_gets_a_session_record() {
        let (tmp, claude, app) = fixture();
        let recorder = Recorder::new(history(&tmp.path().join("history")));
        let (mut state, _) = load_state(&app);
        let mut memory = SessionMemory::new(claude.home().to_path_buf());
        // 10:00:00.5 a `claude -p` session has just started: no answer yet.
        write_session(
            &claude,
            100,
            &session_json(
                100,
                SESSION,
                "/home/sam/projects/atlas",
                None,
                "busy",
                time("2026-11-14T10:00:00Z").timestamp_millis(),
            ),
        );
        one_pass(
            &app,
            &claude,
            &probe(&[100]),
            &NoProc,
            Some(&recorder),
            &mut state,
            &mut memory,
            time("2026-11-14T10:00:01Z"),
            None,
            None,
        );
        // It answers and ends cleanly (its file is removed) before the next wake.
        let folder = claude.projects_dir().join("-home-sam-projects-atlas");
        fs::create_dir_all(&folder).unwrap();
        fs::write(
            folder.join(format!("{SESSION}.jsonl")),
            format!(
                "{}\n",
                transcript_line(
                    SESSION,
                    "msg_01QmB7xT",
                    time("2026-11-14T10:00:09Z"),
                    "/home/sam/projects/atlas",
                    "claude-opus-5-5",
                    (2, 166, 26_173, 28_126)
                )
            ),
        )
        .unwrap();
        fs::remove_file(claude.sessions_dir().join("100.json")).unwrap();
        one_pass(
            &app,
            &claude,
            &probe(&[]),
            &NoProc,
            Some(&recorder),
            &mut state,
            &mut memory,
            time("2026-11-14T10:00:20Z"),
            None,
            None,
        );
        let read = recorder
            .dir()
            .read::<SessionRecord>(
                &Range {
                    from: time("2026-11-01T00:00:00Z"),
                    to: time("2026-12-01T00:00:00Z"),
                },
                &Hosts::One("atlas".to_string()),
            )
            .unwrap();
        assert!(
            !read.records.is_empty(),
            "the session used tokens but has no sessions-… line"
        );
    }

    /// A session without an answer yet must not turn the whole set of models
    /// into "not known": that would switch a per-model limit to the server's
    /// `is_active` view for one wake.
    #[test]
    fn models_in_use_keeps_the_models_of_the_sessions_that_have_one() {
        const SECOND: &str = "2b7d9c1e-5a4f-4c3b-9e8d-7f6a5b4c3d2e";
        let (_tmp, claude, _app) = fixture();
        let at = time("2026-11-14T10:00:00Z");
        write_session(
            &claude,
            700,
            &session_json(
                700,
                SESSION,
                "/home/sam/projects/atlas",
                None,
                "busy",
                at.timestamp_millis(),
            ),
        );
        write_session(
            &claude,
            701,
            &session_json(
                701,
                SECOND,
                "/home/sam/projects/ledger",
                None,
                "busy",
                at.timestamp_millis(),
            ),
        );
        let mut state = ScanState::default();
        state.sessions.insert(
            SESSION.to_string(),
            SessionSeen {
                folder: "-home-sam-projects-atlas".to_string(),
                cwd: "/home/sam/projects/atlas".to_string(),
                first: at,
                last: at,
                last_model: Some("claude-opus-5-5".to_string()),
                last_user_turn: None,
                thinking: Vec::new(),
                totals: Default::default(),
                by_model: Default::default(),
                agents: Default::default(),
                spawns: Default::default(),
                recent: Default::default(),
                recorded: None,
                flash_jobs: BTreeSet::new(),
                title: None,
                custom_title: None,
            },
        );
        let mut memory = SessionMemory::new(claude.home().to_path_buf());
        let outcome = run_pass(
            &_app,
            &claude,
            &probe(&[700, 701]),
            &NoProc,
            None,
            &mut state,
            &mut memory,
            at,
            None,
        );
        assert_eq!(
            outcome.models,
            Some(BTreeSet::from(["claude-opus-5-5".to_string()])),
            "the session without an answer adds nothing and does not clear the set"
        );
    }

    #[test]
    fn the_pass_hands_over_the_thinking_of_tied_sessions_only() {
        const OTHER: &str = "2b7d9c1e-5a4f-4c3b-9e8d-7f6a5b4c3d2e";
        let (_tmp, claude, app) = fixture();
        let at = time("2026-11-14T10:00:00Z");
        write_session(
            &claude,
            800,
            &session_json(
                800,
                SESSION,
                "/home/sam/projects/atlas",
                None,
                "busy",
                at.timestamp_millis(),
            ),
        );
        write_session(
            &claude,
            801,
            &session_json(
                801,
                OTHER,
                "/home/sam/projects/ledger",
                None,
                "idle",
                at.timestamp_millis(),
            ),
        );
        let thinking = vec![
            (time("2026-11-14T09:58:00Z"), 2),
            (time("2026-11-14T09:59:00Z"), 1),
        ];
        let mut state = ScanState::default();
        state.sessions.insert(
            SESSION.to_string(),
            SessionSeen {
                folder: "-home-sam-projects-atlas".to_string(),
                cwd: "/home/sam/projects/atlas".to_string(),
                first: at,
                last: at,
                last_model: Some("claude-fable-5-2".to_string()),
                last_user_turn: None,
                thinking: thinking.clone(),
                totals: Default::default(),
                by_model: Default::default(),
                agents: Default::default(),
                spawns: Default::default(),
                recent: Default::default(),
                recorded: None,
                flash_jobs: BTreeSet::new(),
                title: None,
                custom_title: None,
            },
        );
        state.sessions.insert(
            OTHER.to_string(),
            SessionSeen {
                folder: "-home-sam-projects-ledger".to_string(),
                cwd: "/home/sam/projects/ledger".to_string(),
                first: at,
                last: at,
                last_model: Some("claude-opus-5-5".to_string()),
                last_user_turn: None,
                thinking: vec![(time("2026-11-14T09:57:00Z"), 3)],
                totals: Default::default(),
                by_model: Default::default(),
                agents: Default::default(),
                spawns: Default::default(),
                recent: Default::default(),
                recorded: None,
                flash_jobs: BTreeSet::new(),
                title: None,
                custom_title: None,
            },
        );
        let mut memory = SessionMemory::new(claude.home().to_path_buf());

        let outcome = run_pass(
            &app,
            &claude,
            &probe(&[800, 801]),
            &NoProc,
            None,
            &mut state,
            &mut memory,
            at,
            None,
        );

        assert_eq!(outcome.thinking, thinking);
    }

    #[test]
    fn the_agents_are_read_once_per_session_per_pass() {
        let (_tmp, claude, app) = fixture();
        let at = time("2026-11-14T10:00:00Z");
        write_session(
            &claude,
            900,
            &session_json(
                900,
                SESSION,
                "/home/sam/projects/atlas",
                None,
                "busy",
                at.timestamp_millis(),
            ),
        );
        // The unreadable meta is a warning from `agents_of`; the session record
        // and the rows must share one read, so it is said once.
        let subagents = claude
            .projects_dir()
            .join("-home-sam-projects-atlas")
            .join(SESSION)
            .join("subagents");
        fs::create_dir_all(&subagents).unwrap();
        fs::write(subagents.join("agent-a875.meta.json"), "{\"agentType\": ").unwrap();
        let mut state = ScanState::default();
        state.sessions.insert(
            SESSION.to_string(),
            SessionSeen {
                folder: "-home-sam-projects-atlas".to_string(),
                cwd: "/home/sam/projects/atlas".to_string(),
                first: at,
                last: at,
                last_model: Some("claude-opus-5-5".to_string()),
                last_user_turn: None,
                thinking: Vec::new(),
                totals: Default::default(),
                by_model: Default::default(),
                agents: Default::default(),
                spawns: Default::default(),
                recent: Default::default(),
                recorded: None,
                flash_jobs: BTreeSet::new(),
                title: None,
                custom_title: None,
            },
        );

        let mut memory = SessionMemory::new(claude.home().to_path_buf());
        let outcome = run_pass(
            &app,
            &claude,
            &probe(&[900]),
            &NoProc,
            None,
            &mut state,
            &mut memory,
            at,
            None,
        );

        let warning = outcome.summary.warning.unwrap_or_default();
        assert_eq!(
            warning.matches("agent meta").count(),
            1,
            "the meta file is read once per pass: {warning:?}"
        );
    }

    #[test]
    fn the_summary_counts_tied_sessions_and_the_busy_ones() {
        const OTHER: &str = "2b7d9c1e-5a4f-4c3b-9e8d-7f6a5b4c3d2e";
        let (_tmp, claude, app) = fixture();
        let at = time("2026-11-14T10:00:00Z");
        write_session(
            &claude,
            910,
            &session_json(
                910,
                SESSION,
                "/home/sam/projects/atlas",
                None,
                "busy",
                at.timestamp_millis(),
            ),
        );
        write_session(
            &claude,
            911,
            &session_json(
                911,
                OTHER,
                "/home/sam/projects/ledger",
                None,
                "idle",
                at.timestamp_millis(),
            ),
        );
        let mut state = ScanState::default();
        state.sessions.insert(
            SESSION.to_string(),
            SessionSeen {
                folder: "-home-sam-projects-atlas".to_string(),
                cwd: "/home/sam/projects/atlas".to_string(),
                first: at,
                last: at,
                last_model: Some("claude-fable-5-2".to_string()),
                last_user_turn: None,
                thinking: vec![(time("2026-11-14T09:58:00Z"), 2)],
                totals: Default::default(),
                by_model: Default::default(),
                agents: Default::default(),
                spawns: Default::default(),
                recent: Default::default(),
                recorded: None,
                flash_jobs: BTreeSet::new(),
                title: None,
                custom_title: None,
            },
        );
        state.sessions.insert(
            OTHER.to_string(),
            SessionSeen {
                folder: "-home-sam-projects-ledger".to_string(),
                cwd: "/home/sam/projects/ledger".to_string(),
                first: at,
                last: at,
                last_model: Some("claude-fable-5-2".to_string()),
                last_user_turn: None,
                thinking: Vec::new(),
                totals: Default::default(),
                by_model: Default::default(),
                agents: Default::default(),
                spawns: Default::default(),
                recent: Default::default(),
                recorded: None,
                flash_jobs: BTreeSet::new(),
                title: None,
                custom_title: None,
            },
        );

        let mut memory = SessionMemory::new(claude.home().to_path_buf());
        let outcome = run_pass(
            &app,
            &claude,
            &probe(&[910, 911]),
            &NoProc,
            None,
            &mut state,
            &mut memory,
            at,
            None,
        );

        assert_eq!(outcome.summary.tied, 2);
        assert_eq!(outcome.summary.tied_busy, 1);
    }

    /// A closed session that answers again gets one new closed record; an
    /// unchanged one gets none.
    #[test]
    fn a_closed_session_that_answers_again_gets_a_new_closed_record() {
        let (tmp, claude, app) = fixture();
        let recorder = Recorder::new(history(&tmp.path().join("history")));
        let folder = claude.projects_dir().join("-home-sam-projects-atlas");
        fs::create_dir_all(&folder).unwrap();
        let path = folder.join(format!("{SESSION}.jsonl"));
        fs::write(
            &path,
            format!(
                "{}\n",
                transcript_line(
                    SESSION,
                    "msg_01",
                    time("2026-11-14T09:41:00Z"),
                    "/home/sam/projects/atlas",
                    "claude-opus-5-5",
                    (2, 166, 26_173, 28_126),
                )
            ),
        )
        .unwrap();
        let (mut state, state_error) = load_state(&app);
        let mut memory = SessionMemory::new(claude.home().to_path_buf());
        one_pass(
            &app,
            &claude,
            &probe(&[]),
            &NoProc,
            Some(&recorder),
            &mut state,
            &mut memory,
            time("2026-11-14T10:00:00Z"),
            state_error.as_deref(),
            None,
        );

        let mut text = fs::read_to_string(&path).unwrap();
        text.push_str(&format!(
            "{}\n",
            transcript_line(
                SESSION,
                "msg_02",
                time("2026-11-14T09:42:00Z"),
                "/home/sam/projects/atlas",
                "claude-opus-5-5",
                (2, 170, 26_201, 28_150),
            )
        ));
        fs::write(&path, text).unwrap();
        let (mut state, state_error) = load_state(&app);
        let mut memory = SessionMemory::new(claude.home().to_path_buf());
        let outcome = one_pass(
            &app,
            &claude,
            &probe(&[]),
            &NoProc,
            Some(&recorder),
            &mut state,
            &mut memory,
            time("2026-11-14T10:01:00Z"),
            state_error.as_deref(),
            None,
        );
        assert_eq!(outcome.summary.unavailable, None);
        // No new answer: no third record.
        let (mut state, state_error) = load_state(&app);
        let mut memory = SessionMemory::new(claude.home().to_path_buf());
        one_pass(
            &app,
            &claude,
            &probe(&[]),
            &NoProc,
            Some(&recorder),
            &mut state,
            &mut memory,
            time("2026-11-14T10:02:00Z"),
            state_error.as_deref(),
            None,
        );

        let read = recorder
            .dir()
            .read::<SessionRecord>(
                &Range {
                    from: time("2026-11-01T00:00:00Z"),
                    to: time("2026-12-01T00:00:00Z"),
                },
                &Hosts::One("atlas".to_string()),
            )
            .unwrap();
        assert_eq!(
            read.records.len(),
            2,
            "one closed record per batch of answers"
        );
        assert!(read.records.iter().all(|hosted| hosted.record.closed));
        assert_eq!(read.records[0].record.last, time("2026-11-14T09:41:00Z"));
        assert_eq!(read.records[1].record.last, time("2026-11-14T09:42:00Z"));
    }

    /// A session that closed gets one closing record, and no second one after
    /// a restart. `SessionSeen.recorded` is what remembers the closing record,
    /// so it has to reach the state file: the project folder of a session that
    /// only ends (its transcript does not grow) is not written by the scan.
    #[test]
    fn a_closed_session_gets_no_second_record_after_a_restart() {
        let (tmp, claude, app) = fixture();
        let recorder = Recorder::new(history(&tmp.path().join("history")));
        let folder = claude.projects_dir().join("-home-sam-projects-atlas");
        fs::create_dir_all(&folder).unwrap();
        fs::write(
            folder.join(format!("{SESSION}.jsonl")),
            format!(
                "{}\n",
                transcript_line(
                    SESSION,
                    "msg_01QmB7xT",
                    time("2026-11-14T09:41:07Z"),
                    "/home/sam/projects/atlas",
                    "claude-opus-5-5",
                    (2, 166, 26_173, 28_126),
                )
            ),
        )
        .unwrap();

        // 10:00 the session runs and has answered.
        write_session(
            &claude,
            100,
            &session_json(
                100,
                SESSION,
                "/home/sam/projects/atlas",
                Some("atlas-a6"),
                "busy",
                time("2026-11-14T10:00:00Z").timestamp_millis(),
            ),
        );
        let (mut state, _) = load_state(&app);
        let mut memory = SessionMemory::new(claude.home().to_path_buf());
        one_pass(
            &app,
            &claude,
            &probe(&[100]),
            &NoProc,
            Some(&recorder),
            &mut state,
            &mut memory,
            time("2026-11-14T10:00:00Z"),
            None,
            None,
        );

        // 10:05 it ends: its session file is gone and its transcript did not
        // grow, so the scan writes nothing and the project folder is not
        // touched.
        fs::remove_file(claude.sessions_dir().join("100.json")).unwrap();
        one_pass(
            &app,
            &claude,
            &probe(&[]),
            &NoProc,
            Some(&recorder),
            &mut state,
            &mut memory,
            time("2026-11-14T10:05:00Z"),
            None,
            None,
        );

        // The app restarts: the state comes from the file, the memory is empty.
        let (mut state, state_error) = load_state(&app);
        assert_eq!(state_error, None);
        let mut memory = SessionMemory::new(claude.home().to_path_buf());
        one_pass(
            &app,
            &claude,
            &probe(&[]),
            &NoProc,
            Some(&recorder),
            &mut state,
            &mut memory,
            time("2026-11-14T10:10:00Z"),
            None,
            None,
        );

        let file = tmp
            .path()
            .join("history")
            .join("atlas")
            .join("sessions-2026-11.jsonl");
        let text = fs::read_to_string(&file).unwrap();
        let lines: Vec<&str> = text.lines().collect();
        assert_eq!(
            lines.len(),
            2,
            "one open and one closing record, not a second closing one: {text}"
        );
    }

    /// One answer is counted once. The state that remembers it is written
    /// after the token lines, so a state that cannot be written leaves the
    /// offsets on the disk where they were: the next start reads the same
    /// answer again and counts it a second time.
    #[test]
    #[cfg(unix)]
    fn tokens_written_while_the_state_cannot_be_saved_are_not_counted_twice() {
        use std::os::unix::fs::PermissionsExt;

        let (tmp, claude, app) = fixture();
        let recorder = Recorder::new(history(&tmp.path().join("history")));
        let folder = claude.projects_dir().join("-home-sam-projects-atlas");
        fs::create_dir_all(&folder).unwrap();
        fs::write(
            folder.join(format!("{SESSION}.jsonl")),
            format!(
                "{}\n",
                transcript_line(
                    SESSION,
                    "msg_01QmB7xT",
                    time("2026-11-14T09:41:07Z"),
                    "/home/sam/projects/atlas",
                    "claude-opus-5-5",
                    (2, 166, 26_173, 28_126),
                )
            ),
        )
        .unwrap();

        // The state folder cannot be written: the offsets stay on the disk.
        let state_dir = app.transcripts_state_dir();
        fs::create_dir_all(&state_dir).unwrap();
        fs::set_permissions(&state_dir, fs::Permissions::from_mode(0o555)).unwrap();

        let (mut state, _) = load_state(&app);
        let mut memory = SessionMemory::new(claude.home().to_path_buf());
        one_pass(
            &app,
            &claude,
            &probe(&[]),
            &NoProc,
            Some(&recorder),
            &mut state,
            &mut memory,
            time("2026-11-14T10:00:00Z"),
            None,
            None,
        );

        // The state folder works again, and the app restarts: the state on the
        // disk knows nothing about the answer that was read while the save
        // failed.
        fs::set_permissions(&state_dir, fs::Permissions::from_mode(0o700)).unwrap();
        let (mut state, state_error) = load_state(&app);
        assert_eq!(state_error, None);
        let mut memory = SessionMemory::new(claude.home().to_path_buf());
        one_pass(
            &app,
            &claude,
            &probe(&[]),
            &NoProc,
            Some(&recorder),
            &mut state,
            &mut memory,
            time("2026-11-14T10:05:00Z"),
            None,
            None,
        );

        let read = recorder
            .dir()
            .read::<seatswap_core::history::TokenRecord>(
                &Range {
                    from: time("2026-11-01T00:00:00Z"),
                    to: time("2026-12-01T00:00:00Z"),
                },
                &Hosts::One("atlas".to_string()),
            )
            .unwrap();
        assert_eq!(
            read.records.len(),
            1,
            "one answer is counted once, not once per start"
        );
    }

    /// A session file that cannot be parsed leaves the thread unable to tell
    /// which sessions are still running: it must not forget one of them, the
    /// same way it must not close one of them.
    #[test]
    fn a_session_file_that_cannot_be_read_does_not_forget_the_session() {
        let (tmp, claude, _app) = fixture();
        let first = time("2026-11-14T09:56:00Z");
        // The file is half-written: it does not parse any more, so the thread
        // cannot tell which session is still running.
        fs::write(claude.sessions_dir().join("500.json"), "{\"pid\":500,").unwrap();
        let mut state = ScanState::default();
        state.sessions.insert(
            SESSION.to_string(),
            SessionSeen {
                folder: "-home-sam-projects-atlas".to_string(),
                cwd: "/home/sam/projects/atlas".to_string(),
                first,
                last: first,
                last_model: Some("claude-opus-5-5".to_string()),
                last_user_turn: None,
                thinking: Vec::new(),
                totals: Default::default(),
                by_model: Default::default(),
                agents: Default::default(),
                spawns: Default::default(),
                recent: Default::default(),
                recorded: None,
                flash_jobs: BTreeSet::new(),
                title: None,
                custom_title: None,
            },
        );
        let mut memory = SessionMemory::new(tmp.path().to_path_buf());
        run_pass(
            &_app,
            &claude,
            &probe(&[]),
            &NoProc,
            None,
            &mut state,
            &mut memory,
            first + chrono::Duration::hours(2),
            None,
        );

        assert!(
            state.sessions.contains_key(SESSION),
            "a session file that cannot be read must not make the thread forget a session"
        );
    }

    // ---- this host's report and the other hosts' reports (spec §3.8) ----

    fn report_of(root: &Path, host: &str) -> live_sessions::HostReport {
        let path = history(root).sessions_now_file(host);
        let text =
            fs::read_to_string(&path).unwrap_or_else(|error| panic!("{}: {error}", path.display()));
        serde_json::from_str(&text).expect("a report as write_report writes it")
    }

    #[test]
    fn the_report_is_written_at_most_every_thirty_seconds() {
        let (tmp, claude, app) = fixture();
        let root = tmp.path().join("history");
        let recorder = Recorder::new(history(&root));
        let mut state = ScanState::default();
        let mut memory = SessionMemory::new(tmp.path().to_path_buf());
        let first = time("2026-11-14T10:00:00Z");
        write_session(
            &claude,
            400,
            &session_json(
                400,
                SESSION,
                "/home/sam/projects/atlas",
                Some("atlas-a6"),
                "busy",
                first.timestamp_millis(),
            ),
        );

        run_pass(
            &app,
            &claude,
            &probe(&[400]),
            &NoProc,
            Some(&recorder),
            &mut state,
            &mut memory,
            first,
            None,
        );
        let report = report_of(&root, "atlas");
        assert_eq!(report.at, first);
        assert_eq!(report.sessions.len(), 1);

        // The rows changed 10 s later: the report waits for the 30 s.
        let changed = first + chrono::Duration::seconds(10);
        write_session(
            &claude,
            400,
            &session_json(
                400,
                SESSION,
                "/home/sam/projects/atlas",
                Some("atlas-a6"),
                "idle",
                changed.timestamp_millis(),
            ),
        );
        run_pass(
            &app,
            &claude,
            &probe(&[400]),
            &NoProc,
            Some(&recorder),
            &mut state,
            &mut memory,
            changed,
            None,
        );
        assert_eq!(report_of(&root, "atlas").at, first);

        // At 30 s the change is reported.
        let due = first + chrono::Duration::seconds(30);
        run_pass(
            &app,
            &claude,
            &probe(&[400]),
            &NoProc,
            Some(&recorder),
            &mut state,
            &mut memory,
            due,
            None,
        );
        let report = report_of(&root, "atlas");
        assert_eq!(report.at, due);
        assert_eq!(report.sessions[0].status, live_sessions::LiveStatus::Idle);
    }

    #[test]
    fn a_quiet_host_still_reports_every_five_minutes() {
        let (tmp, claude, app) = fixture();
        let root = tmp.path().join("history");
        let recorder = Recorder::new(history(&root));
        let mut state = ScanState::default();
        let mut memory = SessionMemory::new(tmp.path().to_path_buf());
        let first = time("2026-11-14T10:00:00Z");

        run_pass(
            &app,
            &claude,
            &probe(&[]),
            &NoProc,
            Some(&recorder),
            &mut state,
            &mut memory,
            first,
            None,
        );
        assert!(report_of(&root, "atlas").sessions.is_empty());
        assert_eq!(report_of(&root, "atlas").at, first);

        // Nothing changed and four minutes have not passed: no new report.
        run_pass(
            &app,
            &claude,
            &probe(&[]),
            &NoProc,
            Some(&recorder),
            &mut state,
            &mut memory,
            first + chrono::Duration::seconds(299),
            None,
        );
        assert_eq!(report_of(&root, "atlas").at, first);

        // The heartbeat writes the same empty rows again after five minutes.
        let heartbeat = first + chrono::Duration::seconds(301);
        run_pass(
            &app,
            &claude,
            &probe(&[]),
            &NoProc,
            Some(&recorder),
            &mut state,
            &mut memory,
            heartbeat,
            None,
        );
        let report = report_of(&root, "atlas");
        assert_eq!(report.at, heartbeat);
        assert!(report.sessions.is_empty(), "an empty list is a report too");
    }

    #[test]
    fn a_closed_session_is_reported_at_once() {
        let (tmp, claude, app) = fixture();
        let root = tmp.path().join("history");
        let recorder = Recorder::new(history(&root));
        let mut state = ScanState::default();
        let mut memory = SessionMemory::new(tmp.path().to_path_buf());
        let first = time("2026-11-14T10:00:00Z");
        write_session(
            &claude,
            500,
            &session_json(
                500,
                SESSION,
                "/home/sam/projects/atlas",
                Some("atlas-a6"),
                "busy",
                first.timestamp_millis(),
            ),
        );
        run_pass(
            &app,
            &claude,
            &probe(&[500]),
            &NoProc,
            Some(&recorder),
            &mut state,
            &mut memory,
            first,
            None,
        );
        assert_eq!(report_of(&root, "atlas").sessions.len(), 1);

        fs::remove_file(claude.sessions_dir().join("500.json")).unwrap();
        let closed = first + chrono::Duration::seconds(10);
        run_pass(
            &app,
            &claude,
            &probe(&[]),
            &NoProc,
            Some(&recorder),
            &mut state,
            &mut memory,
            closed,
            None,
        );
        let report = report_of(&root, "atlas");
        assert_eq!(report.at, closed, "a close is reported at once");
        assert!(report.sessions.is_empty());
    }

    // ---- proof tests of the C2-3 review ----

    fn vega_waiting_row(since: DateTime<Utc>) -> LiveSession {
        LiveSession {
            session: "dd5e0c1a-3b7f-4c2d-9e8a-6f1b2c3d4e5f".to_string(),
            host: "vega".to_string(),
            label: "acme-kb-dd".to_string(),
            title: None,
            project_id: seatswap_core::projects::ProjectId::of(
                &seatswap_core::project::ProjectKey::Repo("github.com/acme/acme-kb".to_string()),
                "vega",
            ),
            project: "acme-kb".to_string(),
            org: None,
            cwd: "/home/sam/projects/acme-kb".to_string(),
            provider: "Claude Code".to_string(),
            model: Some("claude-opus-5-5".to_string()),
            tied: false,
            status: LiveStatus::Waiting,
            since,
            started: since,
            agents_running: 0,
            agents_done: 0,
            agents: Vec::new(),
            tokens: Default::default(),
            last_hour: Default::default(),
            last_hour_values: live_sessions::Values::sum(&[]),
            agents_done_values: live_sessions::Values::sum(&[]),
            values: live_sessions::Values {
                api: None,
                api_currency: None,
                api_missing: None,
                api_partial: false,
                subscription: None,
                currency: None,
                subscription_missing: None,
                subscription_partial: false,
            },
        }
    }

    #[test]
    fn review_other_hosts_waiting_session_stays_on_the_passes_between_two_reads() {
        let (tmp, claude, app) = fixture();
        let root = tmp.path().join("history");
        let recorder = Recorder::new(history(&root));
        let first = time("2026-11-14T10:00:00Z");
        let vega = seatswap_core::history::HistoryDir::at(&root, "vega");
        fs::create_dir_all(root.join("vega")).unwrap();
        fs::write(
            root.join("vega").join("host.json"),
            "{\"machineId\":\"made-up\"}",
        )
        .unwrap();
        let since = first - chrono::Duration::minutes(2);
        live_sessions::write_report(&vega, &[vega_waiting_row(since)], first).unwrap();
        let mut state = ScanState::default();
        let mut memory = SessionMemory::new(tmp.path().to_path_buf());

        let one = run_pass(
            &app,
            &claude,
            &probe(&[]),
            &NoProc,
            Some(&recorder),
            &mut state,
            &mut memory,
            first,
            None,
        );
        assert_eq!(
            one.summary.waiting.len(),
            1,
            "the read pass lists vega's wait"
        );
        let two = run_pass(
            &app,
            &claude,
            &probe(&[]),
            &NoProc,
            Some(&recorder),
            &mut state,
            &mut memory,
            first + chrono::Duration::seconds(5),
            None,
        );
        assert_eq!(
            two.summary.waiting.len(),
            1,
            "a pass between two reads drops vega's wait, so notify.rs withdraws the notification and sends it again at the next read"
        );
    }

    #[test]
    fn review_this_hosts_waiting_session_does_not_need_the_history() {
        let (tmp, claude, app) = fixture();
        let status_at = time("2026-11-14T09:56:00Z");
        write_session(
            &claude,
            600,
            &session_json(
                600,
                SESSION,
                "/home/sam/projects/acme-kb",
                Some("acme-kb-dd"),
                "waiting",
                status_at.timestamp_millis(),
            ),
        );
        let mut state = ScanState::default();
        let mut memory = SessionMemory::new(tmp.path().to_path_buf());
        let outcome = run_pass(
            &app,
            &claude,
            &probe(&[600]),
            &NoProc,
            None,
            &mut state,
            &mut memory,
            time("2026-11-14T10:00:00Z"),
            None,
        );
        assert_eq!(
            outcome.summary.waiting.len(),
            1,
            "before C2-3 a waiting session notified without a history folder"
        );
    }

    #[test]
    fn review_this_hosts_waiting_session_survives_a_broken_prices_file() {
        let (tmp, claude, app) = fixture();
        let root = tmp.path().join("history");
        let recorder = Recorder::new(history(&root));
        fs::write(app.prices_file(), "{not json").unwrap();
        let status_at = time("2026-11-14T09:56:00Z");
        write_session(
            &claude,
            601,
            &session_json(
                601,
                SESSION,
                "/home/sam/projects/acme-kb",
                Some("acme-kb-dd"),
                "waiting",
                status_at.timestamp_millis(),
            ),
        );
        let mut state = ScanState::default();
        let mut memory = SessionMemory::new(tmp.path().to_path_buf());
        let outcome = run_pass(
            &app,
            &claude,
            &probe(&[601]),
            &NoProc,
            Some(&recorder),
            &mut state,
            &mut memory,
            time("2026-11-14T10:00:00Z"),
            None,
        );
        assert!(outcome.summary.unavailable.is_some(), "the context fails");
        assert_eq!(
            outcome.summary.waiting.len(),
            1,
            "a broken prices file must not switch off this host's waiting notifications"
        );
    }

    #[test]
    fn review_a_clock_that_moved_back_does_not_silence_the_report() {
        let (tmp, claude, app) = fixture();
        let root = tmp.path().join("history");
        let recorder = Recorder::new(history(&root));
        let mut state = ScanState::default();
        let mut memory = SessionMemory::new(tmp.path().to_path_buf());
        let first = time("2026-11-14T10:00:00Z");
        let file = |status: &str, at: DateTime<Utc>| {
            session_json(
                700,
                SESSION,
                "/home/sam/projects/atlas",
                Some("atlas-a6"),
                status,
                at.timestamp_millis(),
            )
        };
        write_session(&claude, 700, &file("busy", first));
        run_pass(
            &app,
            &claude,
            &probe(&[700]),
            &NoProc,
            Some(&recorder),
            &mut state,
            &mut memory,
            first,
            None,
        );

        let back = first - chrono::Duration::hours(1);
        write_session(&claude, 700, &file("idle", back));
        run_pass(
            &app,
            &claude,
            &probe(&[700]),
            &NoProc,
            Some(&recorder),
            &mut state,
            &mut memory,
            back,
            None,
        );
        assert_eq!(
            report_of(&root, "atlas").at,
            back,
            "the rows changed and the last report is far away from now: it is due"
        );
    }

    fn transcript_folder(claude: &ClaudePaths, folder: &str) -> PathBuf {
        let path = claude.projects_dir().join(folder);
        fs::create_dir_all(&path).unwrap();
        path
    }

    fn append_transcript(claude: &ClaudePaths, folder: &str, session: &str, line: &str) {
        use std::io::Write;
        let dir = transcript_folder(claude, folder);
        let mut file = fs::OpenOptions::new()
            .create(true)
            .append(true)
            .open(dir.join(format!("{session}.jsonl")))
            .unwrap();
        writeln!(file, "{line}").unwrap();
    }

    fn views(received: &Receiver<UiEvent>) -> Vec<SessionsView> {
        let mut out = Vec::new();
        while let Ok(event) = received.try_recv() {
            if let UiEvent::SessionsView(view) = event {
                out.push(view);
            }
        }
        out
    }

    #[test]
    fn nothing_is_emitted_without_a_subscription() {
        let (tmp, claude, app) = fixture();
        let recorder = Recorder::new(history(&tmp.path().join("history")));
        let now = time("2026-11-14T10:00:00Z");
        write_session(
            &claude,
            800,
            &session_json(
                800,
                SESSION,
                "/home/sam/projects/acme-kb",
                Some("acme-kb-7c"),
                "busy",
                now.timestamp_millis(),
            ),
        );
        append_transcript(
            &claude,
            "-home-sam-projects-acme-kb",
            SESSION,
            &transcript_line(
                SESSION,
                "msg_01",
                now,
                "/home/sam/projects/acme-kb",
                "claude-opus-5-5",
                (2, 100, 26_173, 28_126),
            ),
        );
        let (sent, received) = mpsc::channel();
        let mut state = ScanState::default();
        let mut memory = SessionMemory::new(tmp.path().to_path_buf());
        memory.ui = Some(sent);
        run_pass(
            &app,
            &claude,
            &probe(&[800]),
            &NoProc,
            Some(&recorder),
            &mut state,
            &mut memory,
            now,
            None,
        );
        assert!(
            views(&received).is_empty(),
            "a pass without a subscription sends no view"
        );
    }

    #[test]
    fn a_transcript_change_emits_at_most_two_views_a_second() {
        let (tmp, claude, app) = fixture();
        let recorder = Recorder::new(history(&tmp.path().join("history")));
        let now = time("2026-11-14T10:00:00Z");
        write_session(
            &claude,
            801,
            &session_json(
                801,
                SESSION,
                "/home/sam/projects/acme-kb",
                Some("acme-kb-7c"),
                "busy",
                now.timestamp_millis(),
            ),
        );
        append_transcript(
            &claude,
            "-home-sam-projects-acme-kb",
            SESSION,
            &transcript_line(
                SESSION,
                "msg_01",
                now,
                "/home/sam/projects/acme-kb",
                "claude-opus-5-5",
                (2, 1, 26_173, 28_126),
            ),
        );
        let (sent, received) = mpsc::channel();
        let mut state = ScanState::default();
        let mut memory = SessionMemory::new(tmp.path().to_path_buf());
        memory.ui = Some(sent);
        memory.subscribe(SessionsOptions::default());
        run_pass(
            &app,
            &claude,
            &probe(&[801]),
            &NoProc,
            Some(&recorder),
            &mut state,
            &mut memory,
            now,
            None,
        );
        let mut emitted = views(&received);
        assert_eq!(emitted.len(), 1, "the first pass sends one view");

        for index in 1..=10 {
            let at = now + chrono::Duration::milliseconds(index * 90);
            append_transcript(
                &claude,
                "-home-sam-projects-acme-kb",
                SESSION,
                &transcript_line(
                    SESSION,
                    &format!("msg_{:02}", index + 1),
                    at,
                    "/home/sam/projects/acme-kb",
                    "claude-opus-5-5",
                    (2, 100, 26_173, 28_126),
                ),
            );
            run_pass(
                &app,
                &claude,
                &probe(&[801]),
                &NoProc,
                Some(&recorder),
                &mut state,
                &mut memory,
                at,
                None,
            );
            emitted.extend(views(&received));
        }
        let within_one_second = emitted
            .iter()
            .filter(|view| view.at < now + chrono::Duration::seconds(1))
            .count();
        assert!(
            within_one_second <= 2,
            "{within_one_second} views within one second: {emitted:?}"
        );

        let trailing = now + chrono::Duration::milliseconds(1_040);
        run_pass(
            &app,
            &claude,
            &probe(&[801]),
            &NoProc,
            Some(&recorder),
            &mut state,
            &mut memory,
            trailing,
            None,
        );
        emitted.extend(views(&received));
        let last = emitted.last().expect("the trailing view");
        assert_eq!(
            last.sessions[0].tokens.out, 1_001,
            "the last view holds the final numbers"
        );
        assert_eq!(last.at, trailing);
    }

    #[test]
    fn a_broken_prices_file_is_said_once_in_the_view_and_in_the_pop_up() {
        let (tmp, claude, app) = fixture();
        let recorder = Recorder::new(history(&tmp.path().join("history")));
        fs::write(app.prices_file(), "{not json").unwrap();
        let now = time("2026-11-14T10:00:00Z");
        let mut state = ScanState::default();
        let mut memory = SessionMemory::new(tmp.path().to_path_buf());
        memory.subscription = Some(SessionsOptions {
            closed: ClosedSince::Day,
            host: None,
        });

        let outcome = run_pass(
            &app,
            &claude,
            &probe(&[]),
            &NoProc,
            Some(&recorder),
            &mut state,
            &mut memory,
            now,
            None,
        );
        let pop_up = outcome.summary.unavailable.expect("the pop-up says why");
        assert_eq!(
            pop_up.matches("the prices cannot be read").count(),
            1,
            "{pop_up}"
        );
        let view = memory.view().expect("a view after a pass");
        let window = view.unavailable.expect("the window says why");
        assert_eq!(
            window.matches("the prices cannot be read").count(),
            1,
            "{window}"
        );
    }

    #[test]
    fn closed_rows_are_read_once_per_choice() {
        let (tmp, claude, app) = fixture();
        let history = history(&tmp.path().join("history"));
        let recorder = Recorder::new(history.clone());
        let now = time("2026-11-14T10:00:00Z");
        let closed_at = now - chrono::Duration::hours(3);
        history
            .append(&SessionRecord {
                t: closed_at,
                session: "11a2b3c4-d5e6-4f70-8a9b-0c1d2e3f4a5b".to_string(),
                cwd: "/home/sam/projects/acme-kb".to_string(),
                project: ProjectKey::Repo("github.com/sam/acme-kb".to_string()),
                entrypoint: Some("cli".to_string()),
                title: None,
                first: closed_at - chrono::Duration::hours(1),
                last: closed_at,
                agents_started: 0,
                agents_finished: 0,
                closed: true,
            })
            .unwrap();
        history
            .append(&seatswap_core::history::TokenRecord {
                t: closed_at - chrono::Duration::minutes(10),
                minutes: 60,
                session: "11a2b3c4-d5e6-4f70-8a9b-0c1d2e3f4a5b".to_string(),
                cwd: "/home/sam/projects/acme-kb".to_string(),
                project: ProjectKey::Repo("github.com/sam/acme-kb".to_string()),
                model: "claude-opus-5-5".to_string(),
                input: 0,
                out: 4_000_000,
                cw: 0,
                cr: 0,
                answers: 1,
                agent_answers: 0,
            })
            .unwrap();
        let (sent, _received) = mpsc::channel();
        let mut state = ScanState::default();
        let mut memory = SessionMemory::new(tmp.path().to_path_buf());
        memory.ui = Some(sent);
        memory.subscription = Some(SessionsOptions {
            closed: ClosedSince::Day,
            host: None,
        });

        run_pass(
            &app,
            &claude,
            &probe(&[]),
            &NoProc,
            Some(&recorder),
            &mut state,
            &mut memory,
            now,
            None,
        );
        assert_eq!(
            memory
                .closed_rows
                .get(&ClosedSince::Day)
                .map(|read| read.read_at),
            Some(now)
        );
        assert_eq!(
            memory
                .closed_rows
                .get(&ClosedSince::Day)
                .map(|read| read.rows.sessions.len()),
            Some(1)
        );

        run_pass(
            &app,
            &claude,
            &probe(&[]),
            &NoProc,
            Some(&recorder),
            &mut state,
            &mut memory,
            now + chrono::Duration::seconds(10),
            None,
        );
        assert_eq!(
            memory
                .closed_rows
                .get(&ClosedSince::Day)
                .map(|read| read.read_at),
            Some(now),
            "a later pass with the same choice does not read the closed rows again"
        );

        memory.subscription = Some(SessionsOptions {
            closed: ClosedSince::Week,
            host: None,
        });
        run_pass(
            &app,
            &claude,
            &probe(&[]),
            &NoProc,
            Some(&recorder),
            &mut state,
            &mut memory,
            now + chrono::Duration::seconds(20),
            None,
        );
        assert_eq!(
            memory
                .closed_rows
                .get(&ClosedSince::Week)
                .map(|read| read.read_at),
            Some(now + chrono::Duration::seconds(20)),
            "a changed choice reads again"
        );
        assert_eq!(
            memory
                .closed_rows
                .get(&ClosedSince::Week)
                .map(|read| read.rows.sessions.len()),
            Some(1)
        );
    }

    /// The pushed view is the view of the *subscription* (the plan: the event
    /// is "the view for the subscribed options"). A `View` command with other
    /// options answers its own question; it must not take the subscription's
    /// place.
    #[test]
    fn a_view_command_with_other_options_does_not_change_the_subscription() {
        let (tmp, claude, app) = fixture();
        let history = history(&tmp.path().join("history"));
        let recorder = Recorder::new(history.clone());
        let now = time("2026-11-14T10:00:00Z");
        let closed_at = now - chrono::Duration::hours(3);
        history
            .append(&SessionRecord {
                t: closed_at,
                session: "11a2b3c4-d5e6-4f70-8a9b-0c1d2e3f4a5b".to_string(),
                cwd: "/home/sam/projects/acme-kb".to_string(),
                project: ProjectKey::Repo("github.com/sam/acme-kb".to_string()),
                entrypoint: Some("cli".to_string()),
                title: None,
                first: closed_at - chrono::Duration::hours(1),
                last: closed_at,
                agents_started: 0,
                agents_finished: 0,
                closed: true,
            })
            .unwrap();
        write_session(
            &claude,
            900,
            &session_json(
                900,
                SESSION,
                "/home/sam/projects/acme-kb",
                Some("acme-kb-7c"),
                "busy",
                now.timestamp_millis(),
            ),
        );
        let (sent, received) = mpsc::channel();
        let mut state = ScanState::default();
        let mut memory = SessionMemory::new(tmp.path().to_path_buf());
        memory.ui = Some(sent);
        // The subscription wants no closed rows.
        memory.subscribe(SessionsOptions::default());
        run_pass(
            &app,
            &claude,
            &probe(&[900]),
            &NoProc,
            Some(&recorder),
            &mut state,
            &mut memory,
            now,
            None,
        );

        // The window asks once for the closed rows.
        let answer = answer_view(
            &mut memory,
            SessionsOptions {
                closed: ClosedSince::Day,
                host: None,
            },
            &app,
            Some(&recorder),
        )
        .expect("the command's answer");
        assert!(
            answer
                .sessions
                .iter()
                .any(|row| row.status == LiveStatus::Closed),
            "the command's answer holds the closed rows"
        );

        // The next pushed view is still the subscription's view.
        run_pass(
            &app,
            &claude,
            &probe(&[900]),
            &NoProc,
            Some(&recorder),
            &mut state,
            &mut memory,
            now + chrono::Duration::seconds(61),
            None,
        );
        let pushed = views(&received);
        let last = pushed.last().expect("a pushed view");
        assert!(
            last.sessions
                .iter()
                .all(|row| row.status != LiveStatus::Closed),
            "the pushed view is the subscription's view (closed: none), but it holds {} rows",
            last.sessions.len()
        );
    }

    /// A pass that cannot read the session files must say so in the window's
    /// view, not only in the pop-up: the window would otherwise keep showing
    /// the old rows and never learn why they are stale.
    #[test]
    fn a_pass_that_cannot_read_the_session_files_says_why_in_the_view() {
        let (tmp, claude, app) = fixture();
        let recorder = Recorder::new(history(&tmp.path().join("history")));
        let now = time("2026-11-14T10:00:00Z");
        // The sessions folder is a file: it cannot be read.
        fs::remove_dir_all(claude.sessions_dir()).unwrap();
        fs::write(claude.sessions_dir(), "not a folder").unwrap();
        let (sent, _received) = mpsc::channel();
        let mut state = ScanState::default();
        let mut memory = SessionMemory::new(tmp.path().to_path_buf());
        memory.ui = Some(sent);
        memory.subscribe(SessionsOptions::default());

        let outcome = run_pass(
            &app,
            &claude,
            &probe(&[]),
            &NoProc,
            Some(&recorder),
            &mut state,
            &mut memory,
            now,
            None,
        );
        assert!(
            outcome
                .summary
                .unavailable
                .as_deref()
                .unwrap_or_default()
                .contains("running sessions cannot be read"),
            "the pop-up says why: {:?}",
            outcome.summary.unavailable
        );
        let view = memory
            .view()
            .expect("the window's view says why too, not 'no pass yet'");
        assert!(
            view.unavailable
                .as_deref()
                .unwrap_or_default()
                .contains("running sessions cannot be read"),
            "the window says why: {:?}",
            view.unavailable
        );
    }

    /// The view carries the settings' currency, not the currency of a valuer
    /// that could be built: the settings are read even when the prices cannot
    /// be read.
    #[test]
    fn the_view_has_the_settings_currency_when_the_prices_cannot_be_read() {
        let (tmp, claude, app) = fixture();
        let recorder = Recorder::new(history(&tmp.path().join("history")));
        fs::write(
            app.accounts_file(),
            serde_json::json!({
                "version": 1,
                "settings": { "currency": "EUR" },
            })
            .to_string(),
        )
        .unwrap();
        fs::write(app.prices_file(), "{not json").unwrap();
        let now = time("2026-11-14T10:00:00Z");
        let mut state = ScanState::default();
        let mut memory = SessionMemory::new(tmp.path().to_path_buf());
        memory.subscribe(SessionsOptions::default());

        run_pass(
            &app,
            &claude,
            &probe(&[]),
            &NoProc,
            Some(&recorder),
            &mut state,
            &mut memory,
            now,
            None,
        );
        let view = memory.view().expect("a view after the pass");
        assert_eq!(
            view.currency.as_deref(),
            Some("EUR"),
            "the view carries the settings' currency"
        );
    }

    /// A settings file with the euro as its currency, and a dollar rate.
    fn write_eur_settings(app: &AppPaths) {
        fs::write(
            app.accounts_file(),
            serde_json::json!({
                "version": 1,
                "settings": { "currency": "EUR" },
            })
            .to_string(),
        )
        .unwrap();
    }

    /// A `prices.json` with one price for each of `models`, under the key the
    /// valuer looks it up by, and nothing else.
    fn write_prices(app: &AppPaths, models: &[&str]) {
        let mut prices = Prices::default();
        let list = models
            .iter()
            .map(|model| {
                (
                    seatswap_core::money::model_price_key(model),
                    seatswap_core::money::ModelPrice {
                        input: 4.0,
                        output: 20.0,
                        cache_write: 5.0,
                        cache_read: 0.2,
                        tiered: false,
                    },
                )
            })
            .collect();
        prices.take(list, time("2026-01-01T00:00:00Z"));
        prices.save(app).unwrap();
    }

    /// An `fx.json` for the euro with `rate` dollars to the euro.
    fn write_fx(app: &AppPaths, rate: f64) {
        let mut fx = Fx {
            currency: "EUR".to_string(),
            ..Fx::default()
        };
        fx.take(time("2026-01-01T00:00:00Z").date_naive(), rate);
        fx.save(app).unwrap();
    }

    /// A running session of the atlas project with one answer of `model`.
    fn write_running_answer(claude: &ClaudePaths, pid: u32, session: &str, model: &str) {
        let folder = claude.projects_dir().join("-home-sam-projects-atlas");
        fs::create_dir_all(&folder).unwrap();
        fs::write(
            folder.join(format!("{session}.jsonl")),
            format!(
                "{}\n",
                transcript_line(
                    session,
                    "msg_01",
                    time("2026-11-14T09:41:00Z"),
                    "/home/sam/projects/atlas",
                    model,
                    (2, 166, 26_173, 28_126),
                )
            ),
        )
        .unwrap();
        write_session(
            claude,
            pid,
            &session_json(
                pid,
                session,
                "/home/sam/projects/atlas",
                Some("atlas-a6"),
                "busy",
                time("2026-11-14T09:41:00Z").timestamp_millis(),
            ),
        );
    }

    /// A `feed.json` as the status line writes it: a session and its cost.
    fn write_feed(app: &AppPaths, session: &str, cost: f64) {
        fs::write(
            app.feed_file(),
            serde_json::json!({
                "at": "2026-11-14T09:59:59Z",
                "fiveHour": null,
                "sevenDay": null,
                "model": "claude-opus-5-5",
                "session": session,
                "costUsd": cost,
            })
            .to_string(),
        )
        .unwrap();
    }

    /// A `feed.json` whose cost the status line could not store: it wrote the
    /// reason instead.
    fn write_feed_problem(app: &AppPaths, problem: &str) {
        fs::write(
            app.feed_file(),
            serde_json::json!({
                "at": "2026-11-14T09:59:59Z",
                "fiveHour": null,
                "sevenDay": null,
                "model": "claude-opus-5-5",
                "costProblem": problem,
            })
            .to_string(),
        )
        .unwrap();
    }

    /// Item 6: Claude Code's own cost of a session is the row's API value, in the
    /// API column's currency, and the last cost read is kept while the session is
    /// a row — the feed holds only the last writer.
    #[test]
    fn the_status_lines_cost_is_the_claude_rows_api_value() {
        let (tmp, claude, app) = fixture();
        write_eur_settings(&app);
        write_prices(&app, &["claude-opus-5-5"]);
        write_fx(&app, 2.0);
        let recorder = Recorder::new(history(&tmp.path().join("history")));
        write_running_answer(&claude, 400, SESSION, "claude-opus-5-5");
        write_feed(&app, SESSION, 1.5);
        let mut state = ScanState::default();
        let mut memory = SessionMemory::new(tmp.path().to_path_buf());

        run_pass(
            &app,
            &claude,
            &probe(&[400]),
            &NoProc,
            Some(&recorder),
            &mut state,
            &mut memory,
            time("2026-11-14T10:00:00Z"),
            None,
        );
        let api_of = |memory: &SessionMemory| {
            memory
                .rows
                .as_ref()
                .expect("rows after a pass")
                .sessions
                .iter()
                .find(|row| row.session == SESSION)
                .expect("the session's row")
                .values
                .api
        };
        assert_eq!(
            api_of(&memory),
            Some(3.0),
            "1.50 US dollars at the stored rate of 2.0"
        );

        // The status line wrote no new feed: the cost read before is kept while
        // the session is a row, never dropped to models.dev's value.
        fs::remove_file(app.feed_file()).unwrap();
        run_pass(
            &app,
            &claude,
            &probe(&[400]),
            &NoProc,
            Some(&recorder),
            &mut state,
            &mut memory,
            time("2026-11-14T10:00:05Z"),
            None,
        );
        assert_eq!(api_of(&memory), Some(3.0), "the remembered cost stays");
        assert_eq!(memory.feed_costs.len(), 1);

        // The session ended: its cost is forgotten.
        fs::remove_file(claude.sessions_dir().join("400.json")).unwrap();
        run_pass(
            &app,
            &claude,
            &probe(&[]),
            &NoProc,
            Some(&recorder),
            &mut state,
            &mut memory,
            time("2026-11-14T10:00:10Z"),
            None,
        );
        assert!(
            memory.feed_costs.is_empty(),
            "a cost is kept only while its session is a row"
        );
    }

    /// A cost the feed cannot mean is said once and never used, not even as 0.
    #[test]
    fn a_cost_that_is_not_a_finite_cost_is_not_used() {
        let (tmp, claude, app) = fixture();
        write_eur_settings(&app);
        write_prices(&app, &["claude-opus-5-5"]);
        write_fx(&app, 2.0);
        let recorder = Recorder::new(history(&tmp.path().join("history")));
        write_session(
            &claude,
            401,
            &session_json(
                401,
                SESSION,
                "/home/sam/projects/atlas",
                None,
                "busy",
                time("2026-11-14T09:41:00Z").timestamp_millis(),
            ),
        );
        write_feed(&app, SESSION, -1.5);
        let mut state = ScanState::default();
        let mut memory = SessionMemory::new(tmp.path().to_path_buf());

        let outcome = run_pass(
            &app,
            &claude,
            &probe(&[401]),
            &NoProc,
            Some(&recorder),
            &mut state,
            &mut memory,
            time("2026-11-14T10:00:00Z"),
            None,
        );
        let warning = outcome.summary.warning.expect("a warning says why");
        assert!(
            warning.contains("not a finite cost of 0 or more"),
            "{warning}"
        );
        let row = memory
            .rows
            .as_ref()
            .expect("rows")
            .sessions
            .iter()
            .find(|row| row.session == SESSION)
            .expect("the row");
        assert_eq!(
            row.values.api,
            Some(0.0),
            "the bad cost is not shown; the row's own empty value is the known 0"
        );
        assert_eq!(row.values.api_missing, None);
        assert!(memory.feed_costs.is_empty(), "the bad cost is not kept");

        // A pass every second over the same bad feed says it on every pass while
        // it lasts, not once and then gone.
        let again = run_pass(
            &app,
            &claude,
            &probe(&[401]),
            &NoProc,
            Some(&recorder),
            &mut state,
            &mut memory,
            time("2026-11-14T10:00:01Z"),
            None,
        );
        let warning = again.summary.warning.expect("the warning still says why");
        assert!(
            warning.contains("not a finite cost of 0 or more"),
            "{warning}"
        );
        assert!(memory.feed_costs.is_empty());
    }

    /// A `costProblem` the status line wrote is a warning on every pass too.
    #[test]
    fn a_feeds_cost_problem_is_said_on_every_pass() {
        let (tmp, claude, app) = fixture();
        write_eur_settings(&app);
        write_prices(&app, &["claude-opus-5-5"]);
        write_fx(&app, 2.0);
        let recorder = Recorder::new(history(&tmp.path().join("history")));
        write_session(
            &claude,
            403,
            &session_json(
                403,
                SESSION,
                "/home/sam/projects/atlas",
                None,
                "busy",
                time("2026-11-14T09:41:00Z").timestamp_millis(),
            ),
        );
        let problem = format!("Claude Code's cost for session {SESSION} is not a number: null");
        write_feed_problem(&app, &problem);
        let mut state = ScanState::default();
        let mut memory = SessionMemory::new(tmp.path().to_path_buf());

        for offset in [0, 1, 2] {
            let outcome = run_pass(
                &app,
                &claude,
                &probe(&[403]),
                &NoProc,
                Some(&recorder),
                &mut state,
                &mut memory,
                time("2026-11-14T10:00:00Z") + chrono::Duration::seconds(offset),
                None,
            );
            assert_eq!(
                outcome.summary.warning.as_deref(),
                Some(problem.as_str()),
                "the feed's cost problem is said on every pass"
            );
        }
    }

    /// A converted cost that is not finite is not used: the row keeps the
    /// valuer's value and the warning names the session (item 2).
    #[test]
    fn a_converted_cost_that_is_not_finite_keeps_the_valuer_value() {
        let (tmp, claude, app) = fixture();
        write_eur_settings(&app);
        write_prices(&app, &["claude-opus-5-5"]);
        write_fx(&app, 2.0);
        let recorder = Recorder::new(history(&tmp.path().join("history")));
        write_running_answer(&claude, 404, SESSION, "claude-opus-5-5");
        let now = time("2026-11-14T10:00:00Z");
        // The huge rate makes the valuer's own conversion huge (but finite);
        // the cost's conversion overflows, and that is the one not used.
        write_fx(&app, f64::MAX);
        let mut state = ScanState::default();
        let mut plain = SessionMemory::new(tmp.path().to_path_buf());
        run_pass(
            &app,
            &claude,
            &probe(&[404]),
            &NoProc,
            Some(&recorder),
            &mut state,
            &mut plain,
            now,
            None,
        );
        let valued = plain
            .rows
            .as_ref()
            .expect("rows")
            .sessions
            .iter()
            .find(|row| row.session == SESSION)
            .expect("the row")
            .values
            .api;
        assert!(
            valued.is_some_and(f64::is_finite),
            "the valuer prices the session's tokens: {valued:?}"
        );

        write_feed(&app, SESSION, f64::MAX);
        let mut state = ScanState::default();
        let mut memory = SessionMemory::new(tmp.path().to_path_buf());
        // A fresh state reads the transcript again: a history of its own keeps
        // its token lines from being counted beside the first pass's.
        let recorder = Recorder::new(history(&tmp.path().join("history-again")));
        let outcome = run_pass(
            &app,
            &claude,
            &probe(&[404]),
            &NoProc,
            Some(&recorder),
            &mut state,
            &mut memory,
            now,
            None,
        );
        let warning = outcome.summary.warning.expect("a warning says why");
        assert!(
            warning.contains(SESSION) && warning.contains("not used"),
            "the warning names the session: {warning}"
        );
        let row = memory
            .rows
            .as_ref()
            .expect("rows")
            .sessions
            .iter()
            .find(|row| row.session == SESSION)
            .expect("the row");
        assert_eq!(
            row.values.api, valued,
            "the non-finite conversion is not used; the valuer's value stays"
        );
    }

    /// Item 4: a model without a price is asked for once, and not at all when it
    /// has a price.
    #[test]
    fn a_model_without_a_price_is_asked_for_once() {
        let (tmp, claude, app) = fixture();
        write_eur_settings(&app);
        write_prices(&app, &["claude-opus-5-5"]);
        write_fx(&app, 1.0);
        let recorder = Recorder::new(history(&tmp.path().join("history")));
        write_running_answer(&claude, 402, SESSION, "claude-fable-5-2");
        let now = time("2026-11-14T10:00:00Z");
        let mut state = ScanState::default();
        let mut memory = SessionMemory::new(tmp.path().to_path_buf());
        let asked: std::rc::Rc<RefCell<Vec<String>>> = std::rc::Rc::new(RefCell::new(Vec::new()));
        let seen = std::rc::Rc::clone(&asked);
        memory.want_price = Some(Box::new(move |model: &str| {
            seen.borrow_mut().push(model.to_string());
        }));

        run_pass(
            &app,
            &claude,
            &probe(&[402]),
            &NoProc,
            Some(&recorder),
            &mut state,
            &mut memory,
            now,
            None,
        );
        assert_eq!(
            *asked.borrow(),
            vec!["claude-fable-5-2".to_string()],
            "the unpriced model is asked for; the priced one is not"
        );

        run_pass(
            &app,
            &claude,
            &probe(&[402]),
            &NoProc,
            Some(&recorder),
            &mut state,
            &mut memory,
            now + chrono::Duration::seconds(1),
            None,
        );
        assert_eq!(
            asked.borrow().len(),
            1,
            "a pass every second does not ask again: {:?}",
            asked.borrow()
        );

        // A store where the model has a price asks for nothing.
        let mut priced = SessionMemory::new(tmp.path().to_path_buf());
        let none: std::rc::Rc<RefCell<Vec<String>>> = std::rc::Rc::new(RefCell::new(Vec::new()));
        let seen = std::rc::Rc::clone(&none);
        priced.want_price = Some(Box::new(move |model: &str| {
            seen.borrow_mut().push(model.to_string());
        }));
        write_prices(&app, &["claude-opus-5-5", "claude-fable-5-2"]);
        let mut fresh = ScanState::default();
        run_pass(
            &app,
            &claude,
            &probe(&[402]),
            &NoProc,
            Some(&recorder),
            &mut fresh,
            &mut priced,
            now,
            None,
        );
        assert!(
            none.borrow().is_empty(),
            "a priced model is not asked for: {:?}",
            none.borrow()
        );
    }

    /// Item 5: only a Claude Code row's model can get a price from models.dev's
    /// `anthropic` list, so an unpriced Harness model is not asked for.
    #[test]
    fn a_harness_model_without_a_price_is_not_asked_for() {
        let (tmp, claude, app) = fixture();
        write_eur_settings(&app);
        write_prices(&app, &[]);
        write_fx(&app, 1.0);
        let recorder = Recorder::new(history(&tmp.path().join("history")));
        let paths = harness_paths(&tmp);
        let now = time("2026-11-14T10:00:00Z");
        write_harness(&paths, HARNESS_JOB, None, 0, 1, now, false);
        let mut state = ScanState::default();
        let mut memory = SessionMemory::new(tmp.path().to_path_buf());
        memory.harness_paths = Some(paths);
        let asked: std::rc::Rc<RefCell<Vec<String>>> = std::rc::Rc::new(RefCell::new(Vec::new()));
        let seen = std::rc::Rc::clone(&asked);
        memory.want_price = Some(Box::new(move |model: &str| {
            seen.borrow_mut().push(model.to_string());
        }));

        run_pass(
            &app,
            &claude,
            &probe(&[]),
            &NoProc,
            Some(&recorder),
            &mut state,
            &mut memory,
            now,
            None,
        );
        let row = harness_row(&memory, HARNESS_JOB);
        assert_eq!(row.provider, "DeepSeek Harness");
        assert_eq!(row.model.as_deref(), Some(HARNESS_MODEL));
        assert!(
            asked.borrow().is_empty(),
            "a Harness model is never asked for through the anthropic list: {:?}",
            asked.borrow()
        );
    }

    /// Item 5: a plan model one of a row's agents used is asked for too, not
    /// only the row's own last model.
    #[test]
    fn an_agents_plan_model_without_a_price_is_asked_for() {
        let (tmp, claude, app) = fixture();
        write_eur_settings(&app);
        write_prices(&app, &["claude-opus-5-5"]);
        write_fx(&app, 1.0);
        let recorder = Recorder::new(history(&tmp.path().join("history")));
        write_running_answer(&claude, 402, SESSION, "claude-opus-5-5");
        let subagents = claude
            .projects_dir()
            .join("-home-sam-projects-atlas")
            .join(SESSION)
            .join("subagents");
        fs::create_dir_all(&subagents).unwrap();
        fs::write(
            subagents.join("agent-a875.jsonl"),
            format!(
                "{}\n",
                transcript_line(
                    SESSION,
                    "msg_agent",
                    time("2026-11-14T09:42:00Z"),
                    "/home/sam/projects/atlas",
                    "claude-haiku-5-5",
                    (2, 10, 0, 0),
                )
            ),
        )
        .unwrap();
        let now = time("2026-11-14T10:00:00Z");
        let mut state = ScanState::default();
        let mut memory = SessionMemory::new(tmp.path().to_path_buf());
        let asked: std::rc::Rc<RefCell<Vec<String>>> = std::rc::Rc::new(RefCell::new(Vec::new()));
        let seen = std::rc::Rc::clone(&asked);
        memory.want_price = Some(Box::new(move |model: &str| {
            seen.borrow_mut().push(model.to_string());
        }));

        run_pass(
            &app,
            &claude,
            &probe(&[402]),
            &NoProc,
            Some(&recorder),
            &mut state,
            &mut memory,
            now,
            None,
        );

        let row = harness_row(&memory, SESSION);
        assert_eq!(row.model.as_deref(), Some("claude-opus-5-5"));
        assert_eq!(
            row.agents
                .iter()
                .filter_map(|agent| agent.model.as_deref())
                .collect::<Vec<_>>(),
            vec!["claude-haiku-5-5"],
            "the row carries its agent's model"
        );
        assert_eq!(
            *asked.borrow(),
            vec!["claude-haiku-5-5".to_string()],
            "the unpriced plan model of the agent is asked for; the priced row model is not"
        );
    }

    /// The commands must not starve the thread: after a command the thread has
    /// to wait out the *rest* of its window, not a whole window again. With a
    /// window that is over and over again, the passes (and with them the
    /// pending view) would never come.
    #[test]
    fn the_commands_do_not_starve_the_passes() {
        use std::time::Instant;
        let (tmp, claude, app) = fixture();
        // The projects folder cannot be read: the scan fails, so the thread
        // asks for the next pass in a second.
        fs::write(claude.projects_dir(), "not a folder").unwrap();
        let (ui, received) = mpsc::channel();
        let (engine, _engine_rx) = mpsc::channel();
        let mut handle = spawn(
            app,
            claude,
            Ok(history(&tmp.path().join("history"))),
            Ok(harness::HarnessPaths::at(
                tmp.path().join("sessions"),
                tmp.path().join("reports"),
            )),
            Ok(CodexPaths::at(&tmp.path().join("codex"))),
            None,
            ui,
            engine,
        )
        .expect("the sessions thread");
        let client = handle.client();
        let first = received
            .recv_timeout(std::time::Duration::from_secs(10))
            .expect("the first pass");
        assert!(
            matches!(first, UiEvent::Sessions(_)),
            "the first pass sends its summary"
        );

        // Now the window asks for a view again and again, as a page that polls
        // would.
        let start = Instant::now();
        let mut passes = 0;
        while start.elapsed() < std::time::Duration::from_secs(3) {
            let _ = client.view(SessionsOptions::default());
            std::thread::sleep(std::time::Duration::from_millis(10));
            while let Ok(event) = received.try_recv() {
                if matches!(event, UiEvent::Sessions(_)) {
                    passes += 1;
                }
            }
        }
        handle.stop();
        assert!(
            passes >= 2,
            "the thread must not starve its passes: {passes} passes in 3 s"
        );
    }
    /// REVIEW PROOF (S-1): the session record counts an agent finished by the
    /// same rule as the Sessions row (`finished` or an ended transcript).
    #[test]
    fn review_the_record_counts_an_ended_agent_as_finished() {
        let tmp = tempfile::tempdir().unwrap();
        let seen: SessionSeen = serde_json::from_value(serde_json::json!({
            "folder": "-home-sam-projects-atlas",
            "cwd": "/home/sam/projects/atlas",
            "first": "2026-11-14T09:41:07.412Z",
            "last": "2026-11-14T09:41:07.412Z",
            "lastModel": "claude-opus-5-5",
            "lastUserTurn": null
        }))
        .unwrap();
        let agent = transcripts::AgentMeta {
            session: SESSION.to_string(),
            agent: "a8751b20660a1de6".to_string(),
            agent_type: None,
            description: None,
            finished: false,
            spawn_depth: None,
            tool_use_id: None,
            parent: None,
            transcript_ended: true,
        };
        let mut memory = SessionMemory::new(tmp.path().to_path_buf());
        let record = session_record(
            SessionAgents {
                metas: &[agent],
                linked: AgentCounts::default(),
            },
            SESSION,
            &seen,
            None,
            true,
            &mut memory,
            time("2026-11-14T10:00:00Z"),
        )
        .unwrap();
        assert_eq!(record.agents_finished, 1);
    }

    const HARNESS_JOB: &str = "flash-task-5c974a53-178a-4254-93b2-b804a111414a";
    // Used only by the tests of the Linux process walk.
    #[cfg(target_os = "linux")]
    const HARNESS_UNLINKED: &str = "flash-task-8d2f0e7c-6b1a-4c3d-9e0f-1a2b3c4d5e6f";
    #[cfg(target_os = "linux")]
    const HARNESS_OTHER: &str = "8d2f0e7c-6b1a-4c3d-9e0f-1a2b3c4d5e6f";
    const HARNESS_KEY: &str = "--home-sam-projects-atlas--";
    const HARNESS_MODEL: &str = "deepseek-v4.1-flash:cloud";

    fn harness_paths(tmp: &TempDir) -> seatswap_core::harness::HarnessPaths {
        seatswap_core::harness::HarnessPaths::at(
            tmp.path().join("harness-sessions"),
            tmp.path().join("flash-agents"),
        )
    }

    /// A zstd frame with one raw block. The app does not depend on the zstd
    /// crate, so its tests build the frame themselves; the bytes are a normal
    /// frame that `zstd::stream::read::Decoder` reads.
    fn zstd_frame(text: &[u8]) -> Vec<u8> {
        let size = u32::try_from(text.len()).expect("a short test file");
        let mut frame = vec![0x28, 0xb5, 0x2f, 0xfd];
        if size <= 255 {
            frame.push(0x20);
            frame.push(u8::try_from(size).expect("one byte"));
        } else {
            frame.push(0x60);
            let stored = u16::try_from(size - 256).expect("two bytes");
            frame.extend_from_slice(&stored.to_le_bytes());
        }
        let block = (size << 3) | 1;
        frame.extend_from_slice(&block.to_le_bytes()[..3]);
        frame.extend_from_slice(text);
        frame
    }

    /// Write one Harness session with `messages` assistant events; each message
    /// is 100 input and 50 output. The last event is at `last`.
    fn write_harness(
        paths: &seatswap_core::harness::HarnessPaths,
        id: &str,
        parent: Option<&str>,
        depth: u32,
        messages: u32,
        last: DateTime<Utc>,
        finished: bool,
    ) -> std::path::PathBuf {
        let dir = paths.sessions.join(HARNESS_KEY).join(id);
        fs::create_dir_all(&dir).expect("a session folder");
        let millis = last.timestamp_millis();
        let mut header = serde_json::json!({
            "type": "session",
            "version": 4,
            "id": id,
            "createdAt": millis - 3_600_000,
            "cwd": "/home/sam/projects/atlas",
            "isSeeded": false,
            "delegationDepth": depth,
        });
        if let Some(parent) = parent {
            header["parentSession"] = serde_json::json!(parent);
            header["origin"] = serde_json::json!("subagent");
        }
        let mut text = format!("{header}\n");
        text.push_str(&format!(
            "{}\n",
            serde_json::json!({"type": "turn/start", "seq": 1, "time": millis - 60_000, "data": {"turn": 1}})
        ));
        for number in 0..messages {
            text.push_str(&format!(
                "{}\n",
                serde_json::json!({
                    "type": "assistant/message",
                    "seq": number + 2,
                    "time": millis - 2_000 + i64::from(number) * 1_000,
                    "data": {
                        "turn": 1,
                        "step": 1,
                        "message": {
                            "role": "assistant",
                            "source": {"kind": "model", "provider": "ollama", "model": HARNESS_MODEL},
                            "id": format!("m{number}")
                        },
                        "usage": {
                            "inputTokens": 100,
                            "outputTokens": 50,
                            "totalTokens": 150
                        }
                    }
                })
            ));
        }
        if finished {
            text.push_str(&format!(
                "{}\n",
                serde_json::json!({"type": "turn/end", "seq": 20, "time": millis, "data": {"turn": 1, "reason": {"kind": "completed"}}})
            ));
        }
        let file = dir.join("session.v4.jsonl.zstd");
        fs::write(&file, zstd_frame(text.as_bytes())).expect("a session file");
        fs::write(dir.join("session.lock"), []).expect("a lock");
        file
    }

    fn set_mtime(path: &Path, at: DateTime<Utc>) {
        let when = UNIX_EPOCH
            + std::time::Duration::from_secs(u64::try_from(at.timestamp()).expect("a time"));
        fs::OpenOptions::new()
            .write(true)
            .open(path)
            .expect("the session file")
            .set_modified(when)
            .expect("a modification time");
    }

    #[derive(Default)]
    struct FakeProc {
        processes: BTreeMap<u32, (Vec<PathBuf>, Option<u32>)>,
        walks: Cell<usize>,
    }

    impl harness::ProcSource for FakeProc {
        fn pids(&self) -> std::io::Result<Vec<u32>> {
            self.walks.set(self.walks.get() + 1);
            Ok(self.processes.keys().copied().collect())
        }

        fn open_files(&self, pid: u32) -> std::io::Result<Vec<PathBuf>> {
            Ok(self
                .processes
                .get(&pid)
                .map(|(files, _)| files.clone())
                .unwrap_or_default())
        }

        fn parent(&self, pid: u32) -> Option<u32> {
            self.processes.get(&pid).and_then(|(_, parent)| *parent)
        }
    }

    fn seen_with_jobs(
        _session: &str,
        cwd: &str,
        last: DateTime<Utc>,
        jobs: BTreeSet<String>,
    ) -> SessionSeen {
        SessionSeen {
            folder: "-home-sam-projects-atlas".to_string(),
            cwd: cwd.to_string(),
            first: last,
            last,
            last_model: Some("claude-opus-5-5".to_string()),
            last_user_turn: None,
            thinking: Vec::new(),
            totals: Default::default(),
            by_model: Default::default(),
            agents: Default::default(),
            spawns: Default::default(),
            recent: Default::default(),
            recorded: None,
            flash_jobs: jobs,
            title: None,
            custom_title: None,
        }
    }

    fn harness_row<'a>(memory: &'a SessionMemory, session: &str) -> &'a LiveSession {
        memory
            .rows
            .as_ref()
            .expect("rows after the pass")
            .sessions
            .iter()
            .find(|row| row.session == session)
            .expect("the session's row")
    }

    // The process walk exists on Linux only (docs/windows.md, "The process
    // link does not exist on Windows").
    #[test]
    #[cfg(target_os = "linux")]
    fn run_pass_links_a_running_job_through_its_lock_holder() {
        let (tmp, claude, app) = fixture();
        let recorder = Recorder::new(history(&tmp.path().join("history")));
        let now = time("2026-11-16T14:00:00Z");
        write_session(
            &claude,
            400,
            &session_json(
                400,
                SESSION,
                "/home/sam/projects/atlas",
                Some("atlas-7c"),
                "busy",
                now.timestamp_millis(),
            ),
        );
        let paths = harness_paths(&tmp);
        let file = write_harness(&paths, HARNESS_JOB, None, 0, 1, now, false);
        let lock = file
            .parent()
            .expect("a session folder")
            .join("session.lock");
        let procs = FakeProc {
            processes: BTreeMap::from([
                (500, (vec![lock], Some(400))),
                (400, (Vec::new(), Some(1))),
                (1, (Vec::new(), None)),
            ]),
            walks: Cell::new(0),
        };
        let mut state = ScanState::default();
        let mut memory = SessionMemory::new(tmp.path().to_path_buf());
        memory.harness_paths = Some(paths.clone());

        let outcome = run_pass(
            &app,
            &claude,
            &probe(&[400]),
            &procs,
            Some(&recorder),
            &mut state,
            &mut memory,
            now,
            None,
        );

        assert_eq!(outcome.summary.running, 1);
        assert!(procs.walks.get() > 0, "the process table is walked");
        let row = harness_row(&memory, SESSION);
        let job = row
            .agents
            .iter()
            .find(|agent| agent.agent == HARNESS_JOB)
            .expect("the linked job is a flash agent");
        assert_eq!(job.kind.as_deref(), Some("flash"));
        assert!(job.running);
        assert_eq!(job.model.as_deref(), Some(HARNESS_MODEL));
    }

    /// Z-2 finding 20: a closed session used to count only Claude Code's agent
    /// metas, so its "N done" dropped when it closed. The linked flash/Harness
    /// agents are counted by the same rule as the running row.
    #[test]
    fn a_closed_session_keeps_its_flash_agents() {
        let (tmp, claude, app) = fixture();
        let recorder = Recorder::new(history(&tmp.path().join("history")));
        let now = time("2026-11-16T14:00:00Z");
        write_session(
            &claude,
            400,
            &session_json(
                400,
                SESSION,
                "/home/sam/projects/atlas",
                None,
                "busy",
                now.timestamp_millis(),
            ),
        );
        let subagents = claude
            .projects_dir()
            .join("-home-sam-projects-atlas")
            .join(SESSION)
            .join("subagents");
        fs::create_dir_all(&subagents).unwrap();
        for agent in ["a111", "b222"] {
            fs::write(
                subagents.join(format!("agent-{agent}.meta.json")),
                serde_json::json!({
                    "agentType": "general-purpose",
                    "description": "fix the failing tests",
                    "isBuiltIn": true,
                    "finished": true,
                    "spawnDepth": 1,
                    "toolUseId": format!("toolu_{agent}")
                })
                .to_string(),
            )
            .unwrap();
        }
        let paths = harness_paths(&tmp);
        write_harness(&paths, HARNESS_JOB, None, 0, 1, now, true);
        let mut state = ScanState::default();
        state.sessions.insert(
            SESSION.to_string(),
            seen_with_jobs(
                SESSION,
                "/home/sam/projects/atlas",
                now,
                BTreeSet::from([HARNESS_JOB.to_string()]),
            ),
        );
        let mut memory = SessionMemory::new(claude.home().to_path_buf());
        memory.harness_paths = Some(paths);

        run_pass(
            &app,
            &claude,
            &probe(&[400]),
            &NoProc,
            Some(&recorder),
            &mut state,
            &mut memory,
            now,
            None,
        );
        let running_done = harness_row(&memory, SESSION).agents_done;
        assert_eq!(
            running_done, 3,
            "two Claude agents and the linked flash job are done"
        );

        fs::remove_file(claude.sessions_dir().join("400.json")).unwrap();
        let later = now + ChronoDuration::minutes(1);
        run_pass(
            &app,
            &claude,
            &probe(&[]),
            &NoProc,
            Some(&recorder),
            &mut state,
            &mut memory,
            later,
            None,
        );

        let read = recorder
            .dir()
            .read::<SessionRecord>(
                &Range {
                    from: now - ChronoDuration::hours(1),
                    to: later + ChronoDuration::hours(1),
                },
                &Hosts::One("atlas".to_string()),
            )
            .unwrap();
        let closed = read
            .records
            .iter()
            .filter(|hosted| hosted.record.session == SESSION && hosted.record.closed)
            .max_by_key(|hosted| hosted.record.t)
            .expect("the closing record");
        assert_eq!(
            closed.record.agents_finished, running_done,
            "the closed record counts the flash agent too"
        );
    }

    // The process walk exists on Linux only (docs/windows.md, "The process
    // link does not exist on Windows").
    #[test]
    #[cfg(target_os = "linux")]
    fn run_pass_reads_codex_sessions_from_their_folder() {
        let (tmp, claude, app) = fixture();
        let now = time("2026-11-16T14:00:00Z");
        let codex = CodexPaths::at(&tmp.path().join("codex"));
        let day = codex.sessions_dir().join("2026").join("11").join("16");
        fs::create_dir_all(&day).unwrap();
        let id = "3f7b2c1e-9a4d-4e8f-8b6a-2c5d9e1f7a30";
        let event = |timestamp: &str, ordinal: u32, kind: &str, payload: serde_json::Value| {
            serde_json::json!({
                "timestamp": timestamp,
                "ordinal": ordinal,
                "type": kind,
                "payload": payload,
            })
            .to_string()
        };
        let text = [
            event(
                "2026-11-16T11:50:00.000Z",
                0,
                "session_meta",
                serde_json::json!({
                    "id": id,
                    "session_id": id,
                    "cwd": "/home/sam/projects/atlas",
                    "originator": "codex-tui",
                    "source": "cli",
                    "cli_version": "0.51.0",
                    "model_provider": "openai",
                }),
            ),
            event(
                "2026-11-16T11:50:01.000Z",
                1,
                "turn_context",
                serde_json::json!({
                    "turn_id": "9b8a7c6d-5e4f-4a3b-2c1d-0e9f8a7b6c5d",
                    "cwd": "/home/sam/projects/atlas",
                    "model": "gpt-6.1-sol",
                    "effort": "high",
                }),
            ),
            event(
                "2026-11-16T11:55:00.000Z",
                2,
                "event_msg",
                serde_json::json!({
                    "type": "token_count",
                    "info": {
                        "total_token_usage": {
                            "input_tokens": 12000,
                            "cached_input_tokens": 8000,
                            "cache_write_input_tokens": 300,
                            "output_tokens": 700,
                            "reasoning_output_tokens": 100,
                            "total_tokens": 12700,
                        },
                    },
                }),
            ),
        ]
        .map(|line| format!("{line}\n"))
        .join("");
        let file = day.join(format!("rollout-2026-11-16T11-50-00-{id}.jsonl"));
        fs::write(&file, text).unwrap();
        set_mtime(&file, now - ChronoDuration::hours(3));
        // The file is quiet for longer than the running rule's window; only the
        // process that holds it open makes it running.
        let procs = FakeProc {
            processes: BTreeMap::from([(700, (vec![file.clone()], None))]),
            walks: Cell::new(0),
        };
        let mut state = ScanState::default();
        let mut memory = SessionMemory::new(tmp.path().to_path_buf());
        memory.codex_paths = Some(codex);

        run_pass(
            &app,
            &claude,
            &probe(&[]),
            &procs,
            None,
            &mut state,
            &mut memory,
            now,
            None,
        );

        assert!(procs.walks.get() > 0, "the quiet file needs the walk");
        let row = memory
            .rows
            .as_ref()
            .expect("rows after the pass")
            .sessions
            .iter()
            .find(|row| row.session == id)
            .expect("the Codex row");
        assert_eq!(row.provider, "Codex");
        assert_eq!(row.status, LiveStatus::Idle);
        assert_eq!(row.model.as_deref(), Some("gpt-6.1-sol"));
        assert_eq!(row.tokens.out, 700);
    }

    // The process walk exists on Linux only (docs/windows.md, "The process
    // link does not exist on Windows").
    #[test]
    #[cfg(target_os = "linux")]
    fn a_transcript_owner_wins_over_a_process_owner() {
        let (tmp, claude, app) = fixture();
        let now = time("2026-11-16T14:00:00Z");
        for (pid, session) in [(400, SESSION), (401, HARNESS_OTHER)] {
            write_session(
                &claude,
                pid,
                &session_json(
                    pid,
                    session,
                    "/home/sam/projects/atlas",
                    None,
                    "busy",
                    now.timestamp_millis(),
                ),
            );
        }
        let paths = harness_paths(&tmp);
        // Both jobs are still working, so the process table is walked and the
        // two owners can meet.
        let linked = write_harness(&paths, HARNESS_JOB, None, 0, 1, now, false);
        let unlinked = write_harness(&paths, HARNESS_UNLINKED, None, 0, 1, now, false);
        let lock = |file: &std::path::Path| file.parent().expect("a folder").join("session.lock");
        let procs = FakeProc {
            processes: BTreeMap::from([
                (500, (vec![lock(&linked)], Some(401))),
                (501, (vec![lock(&unlinked)], Some(401))),
                (401, (Vec::new(), Some(1))),
                (1, (Vec::new(), None)),
            ]),
            walks: Cell::new(0),
        };
        let mut state = ScanState::default();
        state.sessions.insert(
            SESSION.to_string(),
            seen_with_jobs(
                SESSION,
                "/home/sam/projects/atlas",
                now,
                BTreeSet::from([HARNESS_JOB.to_string()]),
            ),
        );
        state.sessions.insert(
            HARNESS_OTHER.to_string(),
            seen_with_jobs(
                HARNESS_OTHER,
                "/home/sam/projects/atlas",
                now,
                BTreeSet::new(),
            ),
        );
        let mut memory = SessionMemory::new(tmp.path().to_path_buf());
        memory.harness_paths = Some(paths.clone());

        run_pass(
            &app,
            &claude,
            &probe(&[400, 401]),
            &procs,
            None,
            &mut state,
            &mut memory,
            now,
            None,
        );

        assert!(procs.walks.get() > 0, "the unlinked job needs the walk");
        let transcript_row = harness_row(&memory, SESSION);
        assert!(transcript_row
            .agents
            .iter()
            .any(|agent| agent.agent == HARNESS_JOB));
        let process_row = harness_row(&memory, HARNESS_OTHER);
        assert!(
            !process_row
                .agents
                .iter()
                .any(|agent| agent.agent == HARNESS_JOB),
            "the process link must not take the job from the transcript"
        );
        assert!(process_row
            .agents
            .iter()
            .any(|agent| agent.agent == HARNESS_UNLINKED));
    }

    #[test]
    fn an_already_linked_job_does_not_walk_the_process_table() {
        let (tmp, claude, app) = fixture();
        let now = time("2026-11-16T14:00:00Z");
        let paths = harness_paths(&tmp);
        write_harness(&paths, HARNESS_JOB, None, 0, 1, now, true);
        let procs = FakeProc::default();
        let mut state = ScanState::default();
        state.sessions.insert(
            SESSION.to_string(),
            seen_with_jobs(
                SESSION,
                "/home/sam/projects/atlas",
                now,
                BTreeSet::from([HARNESS_JOB.to_string()]),
            ),
        );
        let mut memory = SessionMemory::new(tmp.path().to_path_buf());
        memory.harness_paths = Some(paths.clone());

        // The process table is not needed to link a job its transcript names.
        // The walk is only for a Harness session still unlinked.
        run_pass(
            &app,
            &claude,
            &probe(&[]),
            &procs,
            None,
            &mut state,
            &mut memory,
            now,
            None,
        );

        assert_eq!(procs.walks.get(), 0, "no walk when every job is linked");
    }

    // The process walk exists on Linux only (docs/windows.md, "The process
    // link does not exist on Windows").
    #[test]
    #[cfg(target_os = "linux")]
    fn a_quiet_unfinished_job_is_done_unless_its_lock_is_held() {
        let (tmp, claude, app) = fixture();
        let now = time("2026-11-16T14:00:00Z");
        let last = now - ChronoDuration::minutes(31);
        let paths = harness_paths(&tmp);
        let file = write_harness(&paths, HARNESS_JOB, None, 0, 1, last, false);
        let lock = file
            .parent()
            .expect("a session folder")
            .join("session.lock");
        let mut state = ScanState::default();
        let mut memory = SessionMemory::new(tmp.path().to_path_buf());
        memory.harness_paths = Some(paths.clone());

        let no_lock = FakeProc::default();
        run_pass(
            &app,
            &claude,
            &probe(&[]),
            &no_lock,
            None,
            &mut state,
            &mut memory,
            now,
            None,
        );
        assert!(
            memory
                .rows
                .as_ref()
                .expect("rows after the pass")
                .sessions
                .iter()
                .all(|row| row.session != HARNESS_JOB),
            "a quiet job without a lock holder is done: no row"
        );

        let held = FakeProc {
            processes: BTreeMap::from([(500, (vec![lock], Some(1))), (1, (Vec::new(), None))]),
            walks: Cell::new(0),
        };
        run_pass(
            &app,
            &claude,
            &probe(&[]),
            &held,
            None,
            &mut state,
            &mut memory,
            now,
            None,
        );
        assert_eq!(harness_row(&memory, HARNESS_JOB).status, LiveStatus::Busy);
    }

    /// The Harness totals follow the transcript scan's rule: saved before the
    /// lines, put back when the lines cannot be written, so a delta is written
    /// once — never twice, and not lost when the history was briefly blocked.
    #[test]
    fn a_harness_delta_that_cannot_be_written_is_written_once_later() {
        let (tmp, claude, app) = fixture();
        let now = time("2026-11-16T14:00:00Z");
        let paths = harness_paths(&tmp);
        write_harness(&paths, HARNESS_JOB, None, 0, 2, now, true);
        let history_root = tmp.path().join("history");
        // A file where the history folder belongs: the token line cannot be
        // written, but the state save can.
        fs::write(&history_root, "not a folder").expect("a blocking file");
        let recorder = Recorder::new(history(&history_root));
        let mut state = ScanState::default();
        let mut memory = SessionMemory::new(tmp.path().to_path_buf());
        memory.harness_paths = Some(paths.clone());
        let pass = |state: &mut ScanState, memory: &mut SessionMemory, recorder: &Recorder| {
            run_pass(
                &app,
                &claude,
                &probe(&[]),
                &NoProc,
                Some(recorder),
                state,
                memory,
                now,
                None,
            );
        };

        pass(&mut state, &mut memory, &recorder);
        assert!(
            !state.harness_recorded.contains_key(HARNESS_JOB),
            "a delta whose line was not written is not recorded"
        );
        let on_disk = ScanState::load(&app).expect("the saved state");
        assert!(
            !on_disk.harness_recorded.contains_key(HARNESS_JOB),
            "the old totals were put back on disk"
        );

        fs::remove_file(&history_root).expect("the blocking file");
        fs::create_dir_all(&history_root).expect("the history folder");
        let recorder = Recorder::new(history(&history_root));
        pass(&mut state, &mut memory, &recorder);
        pass(&mut state, &mut memory, &recorder);
        let lines = fs::read_to_string(history_root.join("atlas").join("tokens-2026-11.jsonl"))
            .expect("the token file");
        assert_eq!(lines.lines().count(), 1, "written once: {lines}");
        let recorded = state.harness_recorded.get(HARNESS_JOB).expect("the totals");
        assert_eq!(recorded.tokens.input, 200);
        assert_eq!(recorded.messages, 2);
        assert_eq!(
            ScanState::load(&app)
                .expect("the saved state")
                .harness_recorded
                .get(HARNESS_JOB)
                .map(|recorded| recorded.messages),
            Some(2),
            "the totals are on disk before the next pass"
        );
    }

    /// A Codex write failure rolls back only the Codex totals: the Harness
    /// totals of the same pass stay on disk.
    #[test]
    fn a_codex_write_failure_puts_back_only_the_codex_totals() {
        let (tmp, claude, app) = fixture();
        let now = time("2026-11-16T14:00:00Z");
        let paths = harness_paths(&tmp);
        write_harness(&paths, HARNESS_JOB, None, 0, 2, now, true);
        let codex = CodexPaths::at(&tmp.path().join("codex"));
        let id = "3f7b2c1e-9a4d-4e8f-8b6a-2c5d9e1f7a30";
        let day = codex.sessions_dir().join("2026").join("11").join("16");
        fs::create_dir_all(&day).unwrap();
        let event = |timestamp: &str, ordinal: u32, kind: &str, payload: serde_json::Value| {
            serde_json::json!({
                "timestamp": timestamp,
                "ordinal": ordinal,
                "type": kind,
                "payload": payload,
            })
            .to_string()
        };
        let text = [
            event(
                "2026-11-16T13:50:00.000Z",
                0,
                "session_meta",
                serde_json::json!({
                    "id": id,
                    "session_id": id,
                    "cwd": "/home/sam/projects/atlas",
                    "originator": "codex-tui",
                    "source": "cli",
                    "cli_version": "0.51.0",
                    "model_provider": "openai",
                }),
            ),
            event(
                "2026-11-16T13:59:00.000Z",
                1,
                "turn_context",
                serde_json::json!({
                    "turn_id": "9b8a7c6d-5e4f-4a3b-2c1d-0e9f8a7b6c5d",
                    "cwd": "/home/sam/projects/atlas",
                    "model": "gpt-6.1-sol",
                    "effort": "high",
                }),
            ),
            event(
                "2026-11-16T14:00:00.000Z",
                2,
                "event_msg",
                serde_json::json!({
                    "type": "token_count",
                    "info": {
                        "total_token_usage": {
                            "input_tokens": 12000,
                            "cached_input_tokens": 8000,
                            "output_tokens": 700,
                            "reasoning_output_tokens": 100,
                            "total_tokens": 12700,
                        },
                    },
                }),
            ),
        ]
        .map(|line| format!("{line}\n"))
        .join("");
        let file = day.join(format!("rollout-2026-11-16T13-50-00-{id}.jsonl"));
        fs::write(&file, text).unwrap();
        set_mtime(&file, now - ChronoDuration::hours(1));

        // A file where the history folder belongs: the token line cannot be
        // written, but the state save can.
        let history_root = tmp.path().join("history");
        fs::write(&history_root, "not a folder").expect("a blocking file");
        let recorder = Recorder::new(history(&history_root));
        let mut state = ScanState::default();
        state.set_harness_recorded(&BTreeMap::from([(
            HARNESS_JOB.to_string(),
            HarnessRecorded {
                tokens: seatswap_core::transcripts::Tokens {
                    input: 200,
                    out: 100,
                    cw: 0,
                    cr: 0,
                },
                messages: 2,
                file: Some(PathBuf::from(format!(
                    "{HARNESS_KEY}/{HARNESS_JOB}/session.v4.jsonl.zstd"
                ))),
            },
        )]));
        let mut memory = SessionMemory::new(tmp.path().to_path_buf());
        memory.harness_paths = Some(paths);
        memory.codex_paths = Some(codex);

        run_pass(
            &app,
            &claude,
            &probe(&[]),
            &NoProc,
            Some(&recorder),
            &mut state,
            &mut memory,
            now,
            None,
        );

        assert!(
            !state.codex_recorded.contains_key(id),
            "a Codex total whose line was not written is not recorded"
        );
        assert_eq!(
            state.harness_recorded.get(HARNESS_JOB).map(|r| r.messages),
            Some(2),
            "the Harness totals are not rolled back with the Codex ones"
        );
        let on_disk = ScanState::load(&app).expect("the saved state");
        assert!(
            !on_disk.codex_recorded.contains_key(id),
            "the old Codex totals were put back on disk"
        );
        assert_eq!(
            on_disk
                .harness_recorded
                .get(HARNESS_JOB)
                .map(|r| r.messages),
            Some(2),
            "the Harness totals stay on disk"
        );
    }

    #[test]
    fn a_state_error_writes_no_harness_token_line() {
        let (tmp, claude, app) = fixture();
        let now = time("2026-11-16T14:00:00Z");
        let paths = harness_paths(&tmp);
        write_harness(&paths, HARNESS_JOB, None, 0, 2, now, true);
        let history_root = tmp.path().join("history");
        let recorder = Recorder::new(history(&history_root));
        let mut state = ScanState::default();
        let mut memory = SessionMemory::new(tmp.path().to_path_buf());
        memory.harness_paths = Some(paths);

        run_pass(
            &app,
            &claude,
            &probe(&[]),
            &NoProc,
            Some(&recorder),
            &mut state,
            &mut memory,
            now,
            Some("the transcript state cannot be read"),
        );

        assert!(state.harness_recorded.is_empty());
        assert!(
            !history_root
                .join("atlas")
                .join("tokens-2026-11.jsonl")
                .exists(),
            "a state that could not be loaded writes no Harness line"
        );
    }

    /// The 24 h window only decides which sessions are read: a total whose
    /// file still exists is kept, even when the file is older than the window.
    #[test]
    fn an_old_harness_total_is_kept_while_its_file_exists() {
        let (tmp, claude, app) = fixture();
        let now = time("2026-11-16T14:00:00Z");
        let paths = harness_paths(&tmp);
        let file = write_harness(&paths, HARNESS_JOB, None, 0, 2, now, true);
        set_mtime(&file, now - ChronoDuration::hours(25));
        let mut state = ScanState::default();
        state.harness_recorded.insert(
            HARNESS_JOB.to_string(),
            HarnessRecorded {
                tokens: transcripts::Tokens {
                    input: 200,
                    out: 100,
                    cw: 0,
                    cr: 0,
                },
                messages: 2,
                file: Some(
                    file.strip_prefix(&paths.sessions)
                        .expect("a file below the store")
                        .to_path_buf(),
                ),
            },
        );
        let mut memory = SessionMemory::new(tmp.path().to_path_buf());
        memory.harness_paths = Some(paths);

        one_pass(
            &app,
            &claude,
            &probe(&[]),
            &NoProc,
            None,
            &mut state,
            &mut memory,
            now,
            None,
            None,
        );

        assert!(
            state.harness_recorded.contains_key(HARNESS_JOB),
            "a total outside the 24h window is kept while its file exists"
        );
    }

    /// A total goes only when its session's file has left the store; a deleted
    /// file drops it at the next save.
    #[test]
    fn a_recorded_total_is_dropped_when_its_file_is_deleted() {
        let (tmp, claude, app) = fixture();
        let now = time("2026-11-16T14:00:00Z");
        let paths = harness_paths(&tmp);
        let file = write_harness(&paths, HARNESS_JOB, None, 0, 2, now, true);
        let recorder = Recorder::new(history(&tmp.path().join("history")));
        let mut state = ScanState::default();
        let mut memory = SessionMemory::new(tmp.path().to_path_buf());
        memory.harness_paths = Some(paths);

        one_pass(
            &app,
            &claude,
            &probe(&[]),
            &NoProc,
            Some(&recorder),
            &mut state,
            &mut memory,
            now,
            None,
            None,
        );
        assert!(state.harness_recorded.contains_key(HARNESS_JOB));
        assert!(
            ScanState::load(&app)
                .expect("the saved state")
                .harness_recorded
                .contains_key(HARNESS_JOB),
            "the total is on disk while its file exists"
        );

        fs::remove_file(&file).expect("the session file is gone");
        one_pass(
            &app,
            &claude,
            &probe(&[]),
            &NoProc,
            Some(&recorder),
            &mut state,
            &mut memory,
            now,
            None,
            None,
        );

        assert!(
            !state.harness_recorded.contains_key(HARNESS_JOB),
            "a deleted session file drops its recorded total"
        );
        assert!(
            !ScanState::load(&app)
                .expect("the saved state")
                .harness_recorded
                .contains_key(HARNESS_JOB),
            "the deleted total is not on disk either"
        );
    }

    /// A session file that cannot be read in one pass must not lose its totals:
    /// when it reads again, its tokens would be written a second time.
    #[test]
    fn proof_a_session_file_that_fails_once_keeps_its_totals() {
        let (tmp, claude, app) = fixture();
        let now = time("2026-11-16T14:00:00Z");
        let paths = harness_paths(&tmp);
        let file = write_harness(&paths, HARNESS_JOB, None, 0, 2, now, true);
        let history_root = tmp.path().join("history");
        let recorder = Recorder::new(history(&history_root));
        let mut state = ScanState::default();
        let mut memory = SessionMemory::new(tmp.path().to_path_buf());
        memory.harness_paths = Some(paths.clone());
        let pass = |state: &mut ScanState, memory: &mut SessionMemory| {
            run_pass(
                &app,
                &claude,
                &probe(&[]),
                &NoProc,
                Some(&recorder),
                state,
                memory,
                now,
                None,
            );
        };

        pass(&mut state, &mut memory);
        assert!(state.harness_recorded.contains_key(HARNESS_JOB));
        fs::write(&file, b"not a zstd stream").expect("a broken file");
        pass(&mut state, &mut memory);
        assert!(
            state.harness_recorded.contains_key(HARNESS_JOB),
            "a session whose file failed to read is not out of range: its totals stay"
        );
        write_harness(&paths, HARNESS_JOB, None, 0, 2, now, true);
        pass(&mut state, &mut memory);
        let lines = fs::read_to_string(history_root.join("atlas").join("tokens-2026-11.jsonl"))
            .expect("the token file");
        assert_eq!(lines.lines().count(), 1, "written once: {lines}");
    }

    /// A job a transcript names, quiet for 45 minutes, whose lock a process
    /// holds, is running; the lock set must not depend on another job being
    /// unlinked.
    // The process walk exists on Linux only (docs/windows.md, "The process
    // link does not exist on Windows").
    #[test]
    #[cfg(target_os = "linux")]
    fn proof_a_linked_quiet_job_with_a_held_lock_is_running() {
        let (tmp, claude, app) = fixture();
        let now = time("2026-11-16T14:00:00Z");
        write_session(
            &claude,
            400,
            &session_json(
                400,
                SESSION,
                "/home/sam/projects/atlas",
                None,
                "busy",
                now.timestamp_millis(),
            ),
        );
        let paths = harness_paths(&tmp);
        let file = write_harness(
            &paths,
            HARNESS_JOB,
            None,
            0,
            1,
            now - ChronoDuration::minutes(45),
            false,
        );
        let lock = file.parent().expect("a folder").join("session.lock");
        let procs = FakeProc {
            processes: BTreeMap::from([(500, (vec![lock], Some(400))), (400, (Vec::new(), None))]),
            walks: Cell::new(0),
        };
        let mut state = ScanState::default();
        state.sessions.insert(
            SESSION.to_string(),
            seen_with_jobs(
                SESSION,
                "/home/sam/projects/atlas",
                now,
                BTreeSet::from([HARNESS_JOB.to_string()]),
            ),
        );
        let mut memory = SessionMemory::new(tmp.path().to_path_buf());
        memory.harness_paths = Some(paths);

        run_pass(
            &app,
            &claude,
            &probe(&[400]),
            &procs,
            None,
            &mut state,
            &mut memory,
            now,
            None,
        );

        let job = harness_row(&memory, SESSION)
            .agents
            .iter()
            .find(|agent| agent.agent == HARNESS_JOB)
            .expect("the job");
        assert!(job.running, "its lock is held");
    }

    /// A Claude Code session that closed more than an hour ago leaves the scan
    /// state (`forget_closed`), but its `flash_jobs` move into the kept closed
    /// links. A job of that session, still inside the 24 h Harness window, keeps
    /// its owner instead of becoming its own "DeepSeek Harness" row.
    #[test]
    fn proof_a_job_of_a_closed_session_does_not_become_its_own_row() {
        let (tmp, claude, app) = fixture();
        let now = time("2026-11-16T14:00:00Z");
        let paths = harness_paths(&tmp);
        write_harness(
            &paths,
            HARNESS_JOB,
            None,
            0,
            1,
            now - ChronoDuration::hours(3),
            true,
        );
        let mut state = ScanState::default();
        state.sessions.insert(
            SESSION.to_string(),
            seen_with_jobs(
                SESSION,
                "/home/sam/projects/atlas",
                now - ChronoDuration::hours(2),
                BTreeSet::from([HARNESS_JOB.to_string()]),
            ),
        );
        let mut memory = SessionMemory::new(tmp.path().to_path_buf());
        memory.harness_paths = Some(paths);

        run_pass(
            &app,
            &claude,
            &probe(&[]),
            &NoProc,
            None,
            &mut state,
            &mut memory,
            now,
            None,
        );

        let rows = memory.rows.as_ref().expect("rows");
        assert!(
            !rows.sessions.iter().any(|row| row.session == HARNESS_JOB),
            "the job's owner was forgotten after an hour, so the job is its own row"
        );
    }

    /// The output tokens of `session`'s token lines in this host's history.
    fn recorded_out(root: &Path, session: &str) -> u64 {
        history(root)
            .read::<TokenRecord>(&Range::ALL, &seatswap_core::history::Hosts::All)
            .expect("the token lines")
            .records
            .iter()
            .filter(|hosted| hosted.record.session == session)
            .map(|hosted| hosted.record.out)
            .sum()
    }

    /// Fault B: a running row's tokens are its token lines in the history. A
    /// line written in a later pass is counted at once, without the history
    /// being read again, and never twice.
    #[test]
    fn a_running_row_counts_its_token_lines_across_passes() {
        let (tmp, claude, app) = fixture();
        write_eur_settings(&app);
        write_prices(&app, &["claude-opus-5-5"]);
        write_fx(&app, 1.0);
        let root = tmp.path().join("history");
        let recorder = Recorder::new(history(&root));
        write_running_answer(&claude, 404, SESSION, "claude-opus-5-5");
        let now = time("2026-11-14T10:00:00Z");
        let mut state = ScanState::default();
        let mut memory = SessionMemory::new(tmp.path().to_path_buf());
        let pass = |state: &mut ScanState, memory: &mut SessionMemory, at: DateTime<Utc>| {
            run_pass(
                &app,
                &claude,
                &probe(&[404]),
                &NoProc,
                Some(&recorder),
                state,
                memory,
                at,
                None,
            );
            harness_row(memory, SESSION).tokens.out
        };

        assert_eq!(pass(&mut state, &mut memory, now), 166);
        append_transcript(
            &claude,
            "-home-sam-projects-atlas",
            SESSION,
            &transcript_line(
                SESSION,
                "msg_02",
                time("2026-11-14T09:58:00Z"),
                "/home/sam/projects/atlas",
                "claude-opus-5-5",
                (3, 200, 0, 0),
            ),
        );
        let later = now + ChronoDuration::seconds(1);
        assert_eq!(
            pass(&mut state, &mut memory, later),
            366,
            "the line this pass wrote counts at once"
        );
        assert_eq!(recorded_out(&root, SESSION), 366, "each line once");
        assert_eq!(
            pass(&mut state, &mut memory, later + ChronoDuration::seconds(1)),
            366,
            "nothing new, nothing added"
        );
        let row = harness_row(&memory, SESSION);
        assert!(
            row.values.api.is_some(),
            "the lines are valued: {:?}",
            row.values
        );
    }

    /// A linked flash job's tokens count once in its Claude Code row: in the
    /// pass that reads them, from the lines written after the rows, and in
    /// every later pass from the history, never added again by the job put
    /// under the row (fault B, question b).
    #[test]
    fn a_linked_jobs_tokens_count_once_in_its_running_row() {
        let (tmp, claude, app) = fixture();
        let root = tmp.path().join("history");
        let recorder = Recorder::new(history(&root));
        let now = time("2026-11-16T14:00:00Z");
        write_session(
            &claude,
            400,
            &session_json(
                400,
                SESSION,
                "/home/sam/projects/atlas",
                None,
                "busy",
                now.timestamp_millis(),
            ),
        );
        let paths = harness_paths(&tmp);
        write_harness(&paths, HARNESS_JOB, None, 0, 2, now, false);
        let mut state = ScanState::default();
        let mut memory = SessionMemory::new(claude.home().to_path_buf());
        memory.harness_paths = Some(paths.clone());
        let pass = |state: &mut ScanState, memory: &mut SessionMemory, at: DateTime<Utc>| {
            // The session has no transcript here, so the scan forgets it each
            // pass: put back what its transcript would give, the job's link.
            state.sessions.insert(
                SESSION.to_string(),
                seen_with_jobs(
                    SESSION,
                    "/home/sam/projects/atlas",
                    at,
                    BTreeSet::from([HARNESS_JOB.to_string()]),
                ),
            );
            run_pass(
                &app,
                &claude,
                &probe(&[400]),
                &NoProc,
                Some(&recorder),
                state,
                memory,
                at,
                None,
            );
            let row = harness_row(memory, SESSION);
            let job = row
                .agents
                .iter()
                .find(|agent| agent.agent == HARNESS_JOB)
                .expect("the linked job");
            (row.tokens.out, job.tokens.out)
        };

        assert_eq!(
            pass(&mut state, &mut memory, now),
            (100, 100),
            "the job's growth of this pass, once"
        );
        assert_eq!(recorded_out(&root, SESSION), 100, "written after the rows");
        assert_eq!(
            pass(&mut state, &mut memory, now + ChronoDuration::seconds(1)),
            (100, 100),
            "now from the history, still once"
        );
        write_harness(
            &paths,
            HARNESS_JOB,
            None,
            0,
            3,
            now + ChronoDuration::seconds(2),
            false,
        );
        assert_eq!(
            pass(&mut state, &mut memory, now + ChronoDuration::seconds(2)),
            (150, 150)
        );
        assert_eq!(recorded_out(&root, SESSION), 150);
    }

    /// A flash job is named by its report's label; the label is shown live and
    /// never reaches the report other hosts read.
    #[test]
    fn a_flash_jobs_label_is_its_task_and_never_written() {
        let (tmp, claude, app) = fixture();
        let root = tmp.path().join("history");
        let recorder = Recorder::new(history(&root));
        let now = time("2026-11-16T14:00:00Z");
        write_session(
            &claude,
            400,
            &session_json(
                400,
                SESSION,
                "/home/sam/projects/atlas",
                None,
                "busy",
                now.timestamp_millis(),
            ),
        );
        let paths = harness_paths(&tmp);
        write_harness(&paths, HARNESS_JOB, None, 0, 1, now, true);
        let label = "Fix the invoice export";
        let patches = paths.reports.join("8279d42859b9").join("patches");
        fs::create_dir_all(&patches).expect("the reports folder");
        fs::write(
            patches.join("job-3e32c2fc.report.json"),
            serde_json::json!({
                "key": "8279d42859b9", "kind": "task", "sessionId": HARNESS_JOB,
                "label": label, "status": "done", "stopReason": "end_turn",
                "timing": {}, "createdAt": "2026-11-16T13:59:00.000Z", "report": {}
            })
            .to_string(),
        )
        .expect("the job's report");
        let mut state = ScanState::default();
        state.sessions.insert(
            SESSION.to_string(),
            seen_with_jobs(
                SESSION,
                "/home/sam/projects/atlas",
                now,
                BTreeSet::from(["job-3e32c2fc".to_string()]),
            ),
        );
        let mut memory = SessionMemory::new(claude.home().to_path_buf());
        memory.harness_paths = Some(paths);

        let outcome = run_pass(
            &app,
            &claude,
            &probe(&[400]),
            &NoProc,
            Some(&recorder),
            &mut state,
            &mut memory,
            now,
            None,
        );

        assert_eq!(outcome.summary.warning, None);
        let job = harness_row(&memory, SESSION)
            .agents
            .iter()
            .find(|agent| agent.agent == HARNESS_JOB)
            .expect("the job, linked through its report")
            .clone();
        assert_eq!(job.task.as_deref(), Some(label));
        assert_eq!(job.provider, live_sessions::HARNESS_PROVIDER);
        let report = fs::read_to_string(history(&root).sessions_now_file("atlas"))
            .expect("this host's report");
        assert!(report.contains(HARNESS_JOB), "the job is in the report");
        assert!(!report.contains(label), "the label is never written");
    }

    /// A running row whose records cannot be had (here: no history is open)
    /// counts its transcripts as before the token records: the totals the
    /// state holds, the answers read past it and its linked job's tokens. Its
    /// values are unknown with the reason, and a pass that reads the same
    /// unwritten answers again does not count them twice.
    #[test]
    fn a_running_row_without_records_counts_its_transcripts() {
        let (tmp, claude, app) = fixture();
        let recorder = Recorder::new(history(&tmp.path().join("history")));
        write_running_answer(&claude, 404, SESSION, "claude-opus-5-5");
        let now = time("2026-11-14T10:00:00Z");
        let mut state = ScanState::default();
        let mut memory = SessionMemory::new(tmp.path().to_path_buf());
        let paths = harness_paths(&tmp);
        memory.harness_paths = Some(paths.clone());
        let pass = |recorder: Option<&Recorder>,
                    state: &mut ScanState,
                    memory: &mut SessionMemory,
                    at: DateTime<Utc>| {
            run_pass(
                &app,
                &claude,
                &probe(&[404]),
                &NoProc,
                recorder,
                state,
                memory,
                at,
                None,
            );
            harness_row(memory, SESSION).clone()
        };

        assert_eq!(
            pass(Some(&recorder), &mut state, &mut memory, now)
                .tokens
                .out,
            166
        );
        // The history breaks; the session answers again and starts a flash job.
        let history_error = "the history folder cannot be opened: another machine's host.json";
        memory.history_error = Some(history_error.to_string());
        append_transcript(
            &claude,
            "-home-sam-projects-atlas",
            SESSION,
            &transcript_line(
                SESSION,
                "msg_02",
                time("2026-11-14T09:58:00Z"),
                "/home/sam/projects/atlas",
                "claude-opus-5-5",
                (3, 200, 0, 0),
            ),
        );
        write_harness(&paths, HARNESS_JOB, None, 0, 2, now, false);
        state
            .sessions
            .get_mut(SESSION)
            .expect("the session in the state")
            .flash_jobs
            .insert(HARNESS_JOB.to_string());

        for second in [1, 2] {
            let row = pass(
                None,
                &mut state,
                &mut memory,
                now + ChronoDuration::seconds(second),
            );
            assert_eq!(
                row.tokens.out,
                166 + 200 + 100,
                "the state's totals, the unwritten answer and the job, once (pass {second})"
            );
            let job = row
                .agents
                .iter()
                .find(|agent| agent.agent == HARNESS_JOB)
                .expect("the linked job");
            assert_eq!(job.tokens.out, 100);
            assert_eq!(row.values.api, None);
            assert_eq!(row.values.api_missing.as_deref(), Some(history_error));
            assert_eq!(
                row.values.subscription_missing.as_deref(),
                Some(history_error)
            );
        }
    }

    /// Without a history the scan's offsets do not move, so a backlog past
    /// the budget is read from the same place every pass: the row counts the
    /// part read, the same each pass and never twice, and a warning says the
    /// rest is not counted. Once the history works, every answer counts once.
    #[test]
    fn a_backlog_past_the_budget_without_a_history_is_counted_in_part_and_said() {
        let (tmp, claude, app) = fixture();
        let root = tmp.path().join("history");
        let recorder = Recorder::new(history(&root));
        write_running_answer(&claude, 404, SESSION, "claude-opus-5-5");
        let now = time("2026-11-14T10:00:00Z");
        let mut state = ScanState::default();
        let mut memory = SessionMemory::new(tmp.path().to_path_buf());
        let pass = |recorder: Option<&Recorder>,
                    state: &mut ScanState,
                    memory: &mut SessionMemory,
                    at: DateTime<Utc>| {
            let outcome = run_pass(
                &app,
                &claude,
                &probe(&[404]),
                &NoProc,
                recorder,
                state,
                memory,
                at,
                None,
            );
            (
                harness_row(memory, SESSION).tokens.out,
                outcome.summary.warning.unwrap_or_default(),
            )
        };

        assert_eq!(pass(Some(&recorder), &mut state, &mut memory, now).0, 166);
        memory.history_error = Some("the history folder cannot be opened".to_string());
        // One line per pass: the budget is reached after the first whole line.
        memory.scan_budget = 1;
        for (number, out) in [(2, 200), (3, 300), (4, 400)] {
            append_transcript(
                &claude,
                "-home-sam-projects-atlas",
                SESSION,
                &transcript_line(
                    SESSION,
                    &format!("msg_0{number}"),
                    time("2026-11-14T09:50:00Z") + ChronoDuration::minutes(number),
                    "/home/sam/projects/atlas",
                    "claude-opus-5-5",
                    (3, out, 0, 0),
                ),
            );
        }

        for second in [1, 2] {
            let (out, warning) = pass(
                None,
                &mut state,
                &mut memory,
                now + ChronoDuration::seconds(second),
            );
            assert_eq!(
                out,
                166 + 200,
                "the first unread line, once (pass {second})"
            );
            assert!(
                warning.contains("the running sessions count only the part read"),
                "{warning}"
            );
        }

        memory.history_error = None;
        memory.scan_budget = SCAN_BUDGET;
        let (out, warning) = pass(
            Some(&recorder),
            &mut state,
            &mut memory,
            now + ChronoDuration::seconds(3),
        );
        assert_eq!(out, 166 + 200 + 300 + 400);
        assert!(!warning.contains("count only the part read"), "{warning}");
        assert_eq!(recorded_out(&root, SESSION), 166 + 200 + 300 + 400);
    }

    /// A valuer that has to reach further back for an older running session,
    /// and cannot be built, is tried again only after the retry time, not on
    /// every pass; until then the values are unknown with the reason, never
    /// valued by the valuer of the shorter range.
    #[test]
    fn a_pricing_that_cannot_reach_back_is_retried_after_the_retry_time() {
        let (tmp, _claude, app) = fixture();
        write_eur_settings(&app);
        write_prices(&app, &["claude-opus-5-5"]);
        write_fx(&app, 1.0);
        let dir = history(&tmp.path().join("history"));
        let mut context = RowsContext::default();
        let mut summary = SessionsSummary {
            running: 0,
            busy: 0,
            tied: 0,
            tied_busy: 0,
            waiting: Vec::new(),
            unavailable: None,
            warning: None,
        };
        let now = time("2026-11-14T10:00:00Z");
        let older = Some(now - ChronoDuration::days(3));
        let mut known = |context: &mut RowsContext,
                         needed_from: Option<DateTime<Utc>>,
                         at: DateTime<Utc>| {
            matches!(
                prepare_rows_context(context, &app, &dir, needed_from, at, &mut summary).pricing,
                live_sessions::Pricing::Known { .. }
            )
        };
        let tried = |context: &RowsContext| context.pricing_failure.as_ref().map(|f| f.tried);

        assert!(known(&mut context, None, now), "built for the last day");
        fs::write(app.prices_file(), "{not json").unwrap();
        let first = now + ChronoDuration::seconds(10);
        assert!(
            !known(&mut context, older, first),
            "the old valuer is too short"
        );
        assert_eq!(tried(&context), Some(first));
        let soon = first + ChronoDuration::seconds(ROWS_CONTEXT_RETRY_SECS - 1);
        assert!(!known(&mut context, older, soon));
        assert_eq!(
            tried(&context),
            Some(first),
            "not tried again before the retry time"
        );
        let later = first + ChronoDuration::seconds(ROWS_CONTEXT_RETRY_SECS);
        assert!(!known(&mut context, older, later));
        assert_eq!(tried(&context), Some(later), "tried again after it");

        write_prices(&app, &["claude-opus-5-5"]);
        let fixed = later + ChronoDuration::seconds(ROWS_CONTEXT_RETRY_SECS);
        assert!(known(&mut context, older, fixed));
        assert_eq!(tried(&context), None);
        assert_eq!(
            context.pricing.as_ref().map(|built| built.from),
            older.map(|from| from.min(fixed - ChronoDuration::days(1)))
        );
    }
}
