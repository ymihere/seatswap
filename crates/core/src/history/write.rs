//! Writing the history (spec §1.1, §1.2): the recorder that turns usage
//! answers into samples, finished windows and blocked events, plus the
//! heartbeat, token and session lines.

use std::collections::{BTreeMap, HashMap, HashSet};
use std::io;
use std::path::PathBuf;

use chrono::{DateTime, Datelike, Duration, Utc};

use crate::api::{LimitKind, Usage};

use super::read::{month_start, months_of_kind};
use super::{
    is_early_reset, reset_minute, same_reset, AccountRef, Event, EventKind, HistoryDir,
    HistoryError, Hosts, Kind, LimitSample, OllamaHourRecord, Range, Sample, SessionRecord, Source,
    TokenRecord, UpRecord, WindowRecord, RESET_DROP_POINTS, RESET_JITTER,
};

/// Turns usage answers into history lines. Per account it keeps the last written
/// line (which decides when a sample is written again) and every limit it has
/// seen with the time of the reading that carried it (which decides finished
/// windows, resets and the 100 % crossings). A new recorder reads both back from
/// this host's own files. Finished windows are remembered by (account, kind,
/// model, end) so one window is written once even while the server still sends
/// its old reset time.
pub struct Recorder {
    dir: HistoryDir,
    state: HashMap<AccountRef, AccountState>,
    windows: HashSet<WindowKey>,
    last_up: Option<DateTime<Utc>>,
    /// Warnings from the lines this recorder read back (spec §1.2) and about
    /// readings that went down without a reset, each returned once by
    /// [`take_warnings`](Self::take_warnings).
    warnings: Vec<String>,
    /// The windows (account, limit, reset time) whose small drop was already
    /// named in a warning: one warning per window.
    small_drops: HashSet<(AccountRef, LimitKey, DateTime<Utc>)>,
    /// The last Ollama line per hour this recorder has read back or written.
    ollama_hours: HashMap<DateTime<Utc>, OllamaHourRecord>,
    /// The months of this host's `ollama` lines already read into
    /// [`ollama_hours`](Self::ollama_hours).
    ollama_months: HashSet<DateTime<Utc>>,
    /// The Ollama hours whose answer was older (a smaller count) and already
    /// named in a warning: one warning per hour.
    ollama_older: HashSet<DateTime<Utc>>,
}

/// What one account's readings have shown so far.
#[derive(Default)]
struct AccountState {
    /// The latest sample line written for the account; None = no line found.
    last_written: Option<Sample>,
    /// Every (kind, model) seen, with its latest reading and that reading's time.
    limits: HashMap<LimitKey, RememberedLimit>,
}

impl AccountState {
    /// Remember every limit of `line` that is not remembered yet, as read at
    /// `line.t`. The seeding walks lines newest first, so the newest reading of
    /// a limit wins; the newest poll reading of a limit is remembered as its
    /// reset baseline even when a status-line line is newer.
    fn remember_newest(&mut self, line: &Sample) {
        for limit in &line.l {
            let key = LimitKey::of(limit);
            if let Some(remembered) = self.limits.get_mut(&key) {
                if remembered.poll.is_none() && line.src == Source::Poll {
                    remembered.poll = Some(RememberedPoll {
                        limit: limit.clone(),
                        t: line.t,
                    });
                }
                continue;
            }
            self.limits.insert(
                key,
                RememberedLimit {
                    limit: limit.clone(),
                    t: line.t,
                    opened: None,
                    poll: (line.src == Source::Poll).then(|| RememberedPoll {
                        limit: limit.clone(),
                        t: line.t,
                    }),
                },
            );
        }
    }
}

/// A limit is matched between readings by its kind and model.
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
struct LimitKey {
    k: LimitKind,
    m: Option<String>,
}

impl LimitKey {
    fn of(limit: &LimitSample) -> LimitKey {
        LimitKey {
            k: limit.k.clone(),
            m: limit.m.clone(),
        }
    }

    /// The limit as a warning names it: `weekly_all`, `weekly_scoped (Fable)`.
    fn name(&self) -> String {
        match &self.m {
            Some(model) => format!("{} ({model})", self.k.as_api()),
            None => self.k.as_api().to_string(),
        }
    }
}

/// A limit's latest reading, and when that reading happened.
#[derive(Debug, Clone)]
struct RememberedLimit {
    limit: LimitSample,
    t: DateTime<Utc>,
    /// Where the open window began when a reset began it; `None` when it began
    /// at its usual start (its reset time minus its length).
    opened: Option<Opened>,
    /// The newest reading of this limit that came from a poll. Only a poll can
    /// show a reset: the status line's merged number never moves a percent
    /// down, so a higher status-line reading followed by a lower poll is not a
    /// drop of the usage.
    poll: Option<RememberedPoll>,
}

/// The newest poll reading of a limit.
#[derive(Debug, Clone, PartialEq)]
struct RememberedPoll {
    limit: LimitSample,
    t: DateTime<Utc>,
}

/// A window that a reset began: the time of the first lower reading and that
/// reading's percent. `from` is `None` only when a restart could not find that
/// reading's sample on this host.
#[derive(Debug, Clone, Copy, PartialEq)]
struct Opened {
    start: DateTime<Utc>,
    from: Option<f64>,
}

impl Opened {
    /// Put the window's start and first percent on its record. A record whose
    /// first percent is unknown is uncertain.
    fn mark(&self, record: &mut WindowRecord) {
        record.start = Some(self.start);
        record.from = self.from;
        if self.from.is_none() {
            record.uncertain = true;
        }
    }
}

/// A finished window, as remembered by the recorder.
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
struct WindowKey {
    account: AccountRef,
    k: LimitKind,
    m: Option<String>,
    end: DateTime<Utc>,
}

impl WindowKey {
    fn of(record: &WindowRecord) -> WindowKey {
        WindowKey {
            account: record.account.clone(),
            k: record.k.clone(),
            m: record.m.clone(),
            end: record.end,
        }
    }
}

impl Recorder {
    pub fn new(dir: HistoryDir) -> Recorder {
        Recorder {
            dir,
            state: HashMap::new(),
            windows: HashSet::new(),
            last_up: None,
            warnings: Vec::new(),
            small_drops: HashSet::new(),
            ollama_hours: HashMap::new(),
            ollama_months: HashSet::new(),
            ollama_older: HashSet::new(),
        }
    }

    pub fn dir(&self) -> &HistoryDir {
        &self.dir
    }

    /// The warnings of the lines the recorder read back since the last call
    /// (spec §1.2): a torn last line, a line of a shape this version does not
    /// know. Each one is returned once, so the app can print it once.
    pub fn take_warnings(&mut self) -> Vec<String> {
        std::mem::take(&mut self.warnings)
    }

    /// One usage answer. Appends a Sample if a number or a reset time differs
    /// from the account's last line, or the last line is at least 1 h old.
    /// Before that, appends a WindowRecord for every window that ended: by a
    /// reset (a reading at least [`RESET_DROP_POINTS`] lower before the reset
    /// time, [`reset_window`]) or as [`closed_windows`] says; and a Blocked /
    /// Unblocked event for every limit that crossed 100 %. A smaller drop
    /// before the reset time is kept as the reading and named once per window
    /// in [`take_warnings`](Self::take_warnings). Returns the events it wrote.
    /// The account's last line and limits are read from the files on first use.
    pub fn reading(
        &mut self,
        account: &AccountRef,
        plan: Option<&str>,
        usage: &Usage,
        at: DateTime<Utc>,
        src: Source,
    ) -> Result<Vec<EventKind>, HistoryError> {
        self.load_once(account)?;
        let next = sample_of(account, usage, at, src);
        let Some(state) = self.state.get_mut(account) else {
            return Err(HistoryError::Io {
                path: PathBuf::from("<recorder>"),
                source: io::Error::new(
                    io::ErrorKind::NotFound,
                    "the account's state was not loaded",
                ),
            });
        };

        // Every remembered limit is measured against this reading on its own:
        // a limit the reading does not carry ends nothing by itself.
        let mut closed: HashMap<LimitKey, DateTime<Utc>> = HashMap::new();
        // The windows a reset began at this reading.
        let mut opened: HashMap<LimitKey, Opened> = HashMap::new();
        let mut windows: Vec<(WindowKey, WindowRecord)> = Vec::new();
        let mut unblocked: Vec<EventKind> = Vec::new();
        for (key, remembered) in &state.limits {
            let prev = Sample {
                t: remembered.t,
                account: account.clone(),
                src: next.src,
                l: vec![remembered.limit.clone()],
            };
            // A reset is measured against the newest poll reading, never a
            // status-line reading: the feed's merged percent only moves up.
            let poll_prev = remembered.poll.as_ref().map(|poll| Sample {
                t: poll.t,
                account: account.clone(),
                src: Source::Poll,
                l: vec![poll.limit.clone()],
            });
            let current = find_limit(&next, &remembered.limit);
            if let Some(mut window) = poll_prev
                .as_ref()
                .and_then(|poll| reset_window(poll, &next, plan))
            {
                if let Some(began) = &remembered.opened {
                    began.mark(&mut window);
                }
                let window_key = WindowKey::of(&window);
                if !self.windows.contains(&window_key) {
                    windows.push((window_key, window));
                }
                if let Some(current) = current {
                    opened.insert(
                        key.clone(),
                        Opened {
                            start: at,
                            from: Some(current.p),
                        },
                    );
                }
                continue;
            }
            let mut ended = closed_windows_against(
                &prev,
                poll_prev.as_ref().and_then(|poll| poll.l.first()),
                &next,
                plan,
            );
            if ended.is_empty() {
                // A drop too small for a reset, before the reset time: kept
                // as the reading, named once per window.
                if let (Some(current), Some(poll)) = (current, remembered.poll.as_ref()) {
                    if let Some(reset_at) = poll.limit.r {
                        let points = poll.limit.p - current.p;
                        let window = (account.clone(), key.clone(), reset_at);
                        if points > 0.0 && self.small_drops.insert(window) {
                            self.warnings.push(format!(
                                "usage of account {} {} went down by {points:.1} points without a reset",
                                account.acc,
                                key.name()
                            ));
                        }
                    }
                }
                continue;
            }
            if let Some(began) = &remembered.opened {
                for window in &mut ended {
                    began.mark(window);
                }
            }
            // A full limit whose window ends while no reading carries it was
            // never seen below 100 %, so nobody else can say it is free again.
            // A window that is already written (a restart seeded the old
            // reading) was announced then.
            let carried = next.l.iter().any(|limit| LimitKey::of(limit) == *key);
            let is_new = ended
                .iter()
                .any(|window| !self.windows.contains(&WindowKey::of(window)));
            if !carried && is_new && remembered.limit.p >= 100.0 {
                unblocked.push(EventKind::Unblocked {
                    account: account.clone(),
                    k: key.k.clone(),
                    m: key.m.clone(),
                });
            }
            for window in ended {
                let window_key = WindowKey::of(&window);
                closed.insert(key.clone(), window.end);
                if !self.windows.contains(&window_key) {
                    windows.push((window_key, window));
                }
            }
        }
        for (key, window) in windows {
            self.dir.append(&window)?;
            self.windows.insert(key);
        }

        let mut events = Vec::new();
        for limit in &next.l {
            let before = state
                .limits
                .get(&LimitKey::of(limit))
                .map(|remembered| remembered.limit.p);
            let now_blocked = limit.p >= 100.0;
            let was_blocked = before.map(|percent| percent >= 100.0);
            match (was_blocked, now_blocked) {
                (None, true) | (Some(false), true) => events.push(EventKind::Blocked {
                    account: next.account.clone(),
                    k: limit.k.clone(),
                    m: limit.m.clone(),
                }),
                (Some(true), false) => events.push(EventKind::Unblocked {
                    account: next.account.clone(),
                    k: limit.k.clone(),
                    m: limit.m.clone(),
                }),
                _ => {}
            }
        }
        events.extend(unblocked);
        for kind in &events {
            self.dir.append(&Event {
                t: at,
                kind: kind.clone(),
            })?;
        }

        // A limit in this reading is remembered as read now; one that is
        // missing stays as it was, unless its window just ended.
        // A reset time that is not later than the end just written (server
        // jitter, a stale answer) is that same window: it keeps the written
        // end, or the window would be written a second time with this
        // reading's numbers.
        // The open window keeps where a reset began it until it ends; a reset
        // at this reading begins a new one here.
        for limit in &next.l {
            let key = LimitKey::of(limit);
            let mut limit = limit.clone();
            if let (Some(end), Some(r)) = (closed.get(&key), limit.r) {
                if r <= *end + Duration::seconds(RESET_JITTER) {
                    limit.r = Some(*end);
                }
            }
            let began = match (opened.get(&key), closed.contains_key(&key)) {
                (Some(began), _) => Some(*began),
                (None, true) => None,
                (None, false) => state
                    .limits
                    .get(&key)
                    .and_then(|remembered| remembered.opened),
            };
            // Only a poll replaces the reset baseline; a status-line reading
            // updates the remembered reading but never becomes the baseline.
            let poll = if next.src == Source::Poll {
                Some(RememberedPoll {
                    limit: limit.clone(),
                    t: at,
                })
            } else {
                state
                    .limits
                    .get(&key)
                    .and_then(|remembered| remembered.poll.clone())
            };
            state.limits.insert(
                key,
                RememberedLimit {
                    limit,
                    t: at,
                    opened: began,
                    poll,
                },
            );
        }
        for key in closed.keys() {
            let present = next.l.iter().any(|limit| LimitKey::of(limit) == *key);
            if !present {
                state.limits.remove(key);
            }
        }

        let write = match &state.last_written {
            None => true,
            Some(prev) => {
                prev.l != next.l || at.signed_duration_since(prev.t) >= chrono::Duration::hours(1)
            }
        };
        if write {
            self.dir.append(&next)?;
            state.last_written = Some(next);
        }
        Ok(events)
    }

    pub fn event(&self, at: DateTime<Utc>, kind: EventKind) -> Result<(), HistoryError> {
        self.dir.append(&Event { t: at, kind })
    }

    /// At most one UpRecord per 5 minutes.
    pub fn up(&mut self, at: DateTime<Utc>) -> Result<(), HistoryError> {
        let due = match self.last_up {
            None => true,
            Some(last) => at.signed_duration_since(last) >= chrono::Duration::minutes(5),
        };
        if due {
            self.dir.append(&UpRecord { t: at })?;
            self.last_up = Some(at);
        }
        Ok(())
    }

    pub fn tokens(&self, records: &[TokenRecord]) -> Result<(), HistoryError> {
        self.dir.append_many(records)
    }

    pub fn session(&self, record: &SessionRecord) -> Result<(), HistoryError> {
        self.dir.append(record)
    }

    /// Ollama's hourly buckets into this host's `ollama` month files: one line
    /// for an hour whose numbers grew, and none for the same answer again or an
    /// older answer. A line is written only when none of the record's counts
    /// (`requests`, `input`, `cached_input`, `output`, `usd`) is smaller than
    /// the hour's remembered line and at least one is larger. A record with any
    /// smaller count is an older answer: it is not written, and it is named
    /// once through [`take_warnings`](Self::take_warnings). Returns how many
    /// lines it appended.
    ///
    /// The first use reads this host's `ollama` lines of the months the buckets
    /// fall in and remembers the last line per hour, so an old bucket already
    /// written is not written again. Each month file is written on its own and
    /// its records are remembered as soon as it is written, so a later month
    /// that fails is retried without writing an earlier month's lines again.
    pub fn ollama_hours(&mut self, records: &[OllamaHourRecord]) -> Result<usize, HistoryError> {
        self.load_ollama_months(records)?;
        let mut latest: BTreeMap<DateTime<Utc>, &OllamaHourRecord> = BTreeMap::new();
        for record in records {
            let later = match latest.get(&record.hour) {
                Some(before) => grows(before, record),
                None => true,
            };
            if later {
                latest.insert(record.hour, record);
            }
        }
        let mut by_month: BTreeMap<(i32, u32), Vec<OllamaHourRecord>> = BTreeMap::new();
        for record in latest.values().copied() {
            let Some(remembered) = self.ollama_hours.get(&record.hour) else {
                by_month
                    .entry((record.hour.year(), record.hour.month()))
                    .or_default()
                    .push((*record).clone());
                continue;
            };
            if grows(remembered, record) {
                by_month
                    .entry((record.hour.year(), record.hour.month()))
                    .or_default()
                    .push((*record).clone());
            } else if is_older(remembered, record) && self.ollama_older.insert(record.hour) {
                self.warnings.push(format!(
                    "Ollama's hour {} came back with smaller numbers than the hour's last line; the newer line is kept",
                    record.hour.to_rfc3339()
                ));
            }
        }
        let mut appended = 0;
        for records in by_month.into_values() {
            self.dir.append_many(&records)?;
            for record in &records {
                self.ollama_hours.insert(record.hour, record.clone());
            }
            appended += records.len();
        }
        Ok(appended)
    }

    /// Read this host's `ollama` lines of the months `records` fall in that no
    /// earlier call read, keeping the last line per hour. A month without a
    /// file yet is remembered too, so its file is read only once.
    fn load_ollama_months(&mut self, records: &[OllamaHourRecord]) -> Result<(), HistoryError> {
        let mut asked = Vec::new();
        for record in records {
            let Some(month) = month_start(record.hour.year(), record.hour.month()) else {
                continue;
            };
            if !asked.contains(&month) {
                asked.push(month);
            }
        }
        if asked.is_empty() {
            return Ok(());
        }
        let host_dir = self.dir.root().join(self.dir.host());
        for range in months_of_kind(&host_dir, Kind::Ollama)? {
            if !asked.contains(&range.from) || self.ollama_months.contains(&range.from) {
                continue;
            }
            let read = self
                .dir
                .read::<OllamaHourRecord>(&range, &Hosts::One(self.dir.host().to_string()))?;
            self.warnings.extend(read.warnings);
            for hosted in read.records {
                self.ollama_hours.insert(hosted.record.hour, hosted.record);
            }
            self.ollama_months.insert(range.from);
        }
        for month in asked {
            self.ollama_months.insert(month);
        }
        Ok(())
    }

    /// On first use of an account: the account's lines on this host, walked
    /// newest first over this host's samples month files that exist, newest
    /// month first. The newest line is the last written one; every other line
    /// adds the limits it carries that no newer line already gave, until the
    /// first `Poll` line has been taken in (it carries every limit) or the files
    /// run out. Then the windows already written for the months of those
    /// limits' ends and of their current windows' starts (every month when a
    /// limit's kind has no known length); per limit, the newest reset record
    /// inside its current window (for a kind of unknown length: after its
    /// newest ordinary window end) says where that window began, and
    /// this host's sample at that moment its first percent (missing: the window
    /// is marked uncertain and one warning names it).
    fn load_once(&mut self, account: &AccountRef) -> Result<(), HistoryError> {
        if self.state.contains_key(account) {
            return Ok(());
        }
        let host_dir = self.dir.root().join(self.dir.host());
        let own = Hosts::One(self.dir.host().to_string());

        let mut state = AccountState::default();
        let mut holds_every_limit = false;
        for range in months_of_kind(&host_dir, Kind::Samples)? {
            let read = self.dir.read::<Sample>(&range, &own)?;
            self.warnings.extend(read.warnings);
            let mut lines: Vec<Sample> = read
                .records
                .into_iter()
                .filter(|hosted| &hosted.record.account == account)
                .map(|hosted| hosted.record)
                .collect();
            lines.sort_by_key(|line| std::cmp::Reverse(line.t));
            for line in lines {
                if state.last_written.is_none() {
                    state.last_written = Some(line.clone());
                }
                state.remember_newest(&line);
                if line.src == Source::Poll {
                    holds_every_limit = true;
                    break;
                }
            }
            if holds_every_limit {
                break;
            }
        }

        // The months that hold a remembered limit's reset time, and the start
        // of its current window, where a reset that began it would lie.
        let mut months: Vec<DateTime<Utc>> = Vec::new();
        for remembered in state.limits.values() {
            let Some(reset_at) = remembered.limit.r else {
                continue;
            };
            let mut times = vec![reset_at];
            if let Some(earliest) = current_window_from(&remembered.limit) {
                times.push(earliest);
            }
            for time in times {
                if let Some(month) = month_start(time.year(), time.month()) {
                    if !months.contains(&month) {
                        months.push(month);
                    }
                }
            }
        }
        // A limit of a kind whose length is unknown has no "current window"
        // to look in: its bound is its newest ordinary window end, which can
        // lie in any month, so then every month file is read.
        let unknown_length = state
            .limits
            .keys()
            .any(|key| window_length(&key.k).is_none());
        let mut written: Vec<WindowRecord> = Vec::new();
        if !months.is_empty() || unknown_length {
            for range in months_of_kind(&host_dir, Kind::Windows)? {
                if !unknown_length && !months.contains(&range.from) {
                    continue;
                }
                let read = self.dir.read::<WindowRecord>(&range, &own)?;
                self.warnings.extend(read.warnings);
                for hosted in read.records {
                    if &hosted.record.account == account {
                        self.windows.insert(WindowKey::of(&hosted.record));
                        written.push(hosted.record);
                    }
                }
            }
        }

        // A window that a reset began: the newest reset record of the limit
        // inside its current window gives the start; this host's sample at
        // that moment gives the first percent. For a kind of unknown length
        // the current window is everything after the limit's newest ordinary
        // window end (every month was read, so that bound is known).
        for (key, remembered) in state.limits.iter_mut() {
            let of_limit = |window: &&WindowRecord| window.k == key.k && window.m == key.m;
            let in_current: Box<dyn Fn(DateTime<Utc>) -> bool> = match (
                window_length(&key.k),
                current_window_from(&remembered.limit),
            ) {
                (Some(_), Some(earliest)) => Box::new(move |end| end >= earliest),
                (Some(_), None) => continue,
                (None, _) => {
                    let last_ordinary = written
                        .iter()
                        .filter(of_limit)
                        .filter(|window| !window.reset)
                        .map(|window| window.end)
                        .max();
                    Box::new(move |end| last_ordinary.is_none_or(|last| end > last))
                }
            };
            let newest = written
                .iter()
                .filter(of_limit)
                .filter(|window| window.reset && window.end <= remembered.t)
                .filter(|window| in_current(window.end))
                .max_by_key(|window| window.end);
            let Some(reset) = newest else {
                continue;
            };
            let from = self.percent_at(account, key, reset.end)?;
            if from.is_none() {
                self.warnings.push(format!(
                    "account {} {}: no sample on this host at {}, the first reading after its reset; \
                     the window's first percent is unknown and its record is marked uncertain",
                    account.acc,
                    key.name(),
                    reset.end.to_rfc3339()
                ));
            }
            remembered.opened = Some(Opened {
                start: reset.end,
                from,
            });
        }

        self.state.insert(account.clone(), state);
        Ok(())
    }

    /// The percent of `key` in this host's sample of `account` taken at `at`,
    /// `None` when there is no such sample or it does not carry the limit.
    fn percent_at(
        &mut self,
        account: &AccountRef,
        key: &LimitKey,
        at: DateTime<Utc>,
    ) -> Result<Option<f64>, HistoryError> {
        let range = Range {
            from: at,
            to: at + Duration::seconds(1),
        };
        let read = self
            .dir
            .read::<Sample>(&range, &Hosts::One(self.dir.host().to_string()))?;
        self.warnings.extend(read.warnings);
        Ok(read
            .records
            .iter()
            .map(|hosted| &hosted.record)
            .filter(|sample| &sample.account == account && sample.t == at)
            .find_map(|sample| {
                sample
                    .l
                    .iter()
                    .find(|limit| LimitKey::of(limit) == *key)
                    .map(|limit| limit.p)
            }))
    }
}

/// The earliest moment a reset that began `limit`'s current window can lie
/// at: its reset time minus its length, less [`RESET_JITTER`] (a reset time
/// set from the reset's moment is rounded up to the minute). `None` without a
/// reset time or for a kind of unknown length.
fn current_window_from(limit: &LimitSample) -> Option<DateTime<Utc>> {
    let reset_at = limit.r?;
    let length = window_length(&limit.k)?;
    Some(reset_at - length - Duration::seconds(RESET_JITTER))
}

/// One usage answer as a history sample, with reset times through
/// [`reset_minute`]. Limits keep the order of `usage.limits`.
pub fn sample_of(account: &AccountRef, usage: &Usage, at: DateTime<Utc>, src: Source) -> Sample {
    Sample {
        t: at,
        account: account.clone(),
        src,
        l: usage
            .limits
            .iter()
            .map(|limit| LimitSample {
                k: limit.kind.clone(),
                m: limit.model.clone(),
                p: limit.percent,
                r: limit.resets_at.map(reset_minute),
                b: limit.is_active,
            })
            .collect(),
    }
}

/// The windows of `prev` that have ended by `next` at their reset time. A limit
/// (matched by `k` and `m`) has ended when `prev.r` is `Some(end)` and:
/// `next.t >= end`, or next has the limit with `next.r > end + 60 s` and at
/// least one window length (7 d, 5 h; less 60 s) after `end`, or next
/// has the limit at least [`RESET_DROP_POINTS`] lower within [`RESET_JITTER`]
/// of `end` (the reset seen a moment early by this computer's clock). A limit
/// missing from `next`, or with `r` None, ends nothing by itself. A drop that
/// comes earlier is a reset ([`reset_window`]), which the caller checks first.
/// `end = prev.r`; `final = prev.p`; `lastSample = prev.t`;
/// `uncertain = end - prev.t > 1 h`. `start = end - 7 d` for weekly_all and
/// weekly_scoped, `end - 5 h` for session, `None` otherwise.
/// `lost = max(0, 100 - final)` for the two weekly kinds, `None` otherwise.
pub fn closed_windows(prev: &Sample, next: &Sample, plan: Option<&str>) -> Vec<WindowRecord> {
    closed_windows_against(prev, None, next, plan)
}

/// [`closed_windows`], with the drop at the reset time measured against
/// `poll` (the last poll reading of `prev`'s limit) when there is one. The
/// recorder passes it so a status-line reading cannot make an ordinary end
/// look different from the poll's own reading.
fn closed_windows_against(
    prev: &Sample,
    poll: Option<&LimitSample>,
    next: &Sample,
    plan: Option<&str>,
) -> Vec<WindowRecord> {
    let mut windows = Vec::new();
    for limit in &prev.l {
        let Some(end) = limit.r else { continue };
        let current = find_limit(next, limit);
        // A later reset is the next window only when it lies a whole window
        // length after the old one (less the jitter): the old window ended at
        // its reset and a new one began. An unused Codex limit names a reset
        // one window length after every reading, so its reset moves later by
        // minutes at each poll while the window goes on. A kind of unknown
        // length has no such bound: any later reset is the next window.
        let rollover = window_length(&limit.k).unwrap_or_else(Duration::zero);
        let same_later = current
            .and_then(|current| current.r)
            .is_some_and(|new_end| {
                new_end > end + Duration::seconds(RESET_JITTER)
                    && new_end >= end + rollover - Duration::seconds(RESET_JITTER)
            });
        let before = poll
            .filter(|poll| poll.k == limit.k && poll.m == limit.m)
            .unwrap_or(limit);
        // The drop at the reset time is the window's ordinary end, not an
        // early reset, so it uses the plain point threshold.
        let dropped_at_end = current.is_some_and(|current| {
            before.p - current.p >= RESET_DROP_POINTS && same_reset(next.t, end)
        });
        if next.t >= end || same_later || dropped_at_end {
            windows.push(window_of(prev, limit, end, end, plan));
        }
    }
    windows
}

/// The window of `prev`'s one limit (the limit's last poll reading) that a
/// reset ended at `next`: the shared [`is_early_reset`] rule says the limit
/// fell at least [`RESET_DROP_POINTS`] while `next.t` is more than
/// [`RESET_JITTER`] before `prev.r`, whether or not the reset time moved.
/// `end = next.t`, `reset = true`, `lost = None` (the limit was given back,
/// nothing expired); `start`, `final`, `lastSample`, `uncertain` and `plan` as
/// [`closed_windows`] has them, `start` from `prev.r`. `None` when the limit
/// did not drop that far, has no reset time, or the drop comes at or after the
/// reset time.
fn reset_window(prev: &Sample, next: &Sample, plan: Option<&str>) -> Option<WindowRecord> {
    let limit = prev.l.first()?;
    let reset_at = limit.r?;
    let current = find_limit(next, limit)?;
    if !dropped(limit, current, Some(reset_at), next.t) {
        return None;
    }
    let mut window = window_of(prev, limit, reset_at, next.t, plan);
    window.lost = None;
    window.reset = true;
    Some(window)
}

/// Whether `current` is an early reset of `previous`: [`is_early_reset`]'s one
/// rule, used by the recorder.
fn dropped(
    previous: &LimitSample,
    current: &LimitSample,
    reset_at: Option<DateTime<Utc>>,
    at: DateTime<Utc>,
) -> bool {
    is_early_reset(previous.p, current.p, reset_at, at)
}

/// Whether `next` is a later answer for its hour: no count smaller than the
/// hour's remembered line and at least one larger.
fn grows(previous: &OllamaHourRecord, next: &OllamaHourRecord) -> bool {
    !is_older(previous, next)
        && (next.requests > previous.requests
            || next.input > previous.input
            || next.cached_input > previous.cached_input
            || next.output > previous.output
            || next.usd > previous.usd)
}

/// Whether any count of `next` is smaller than the hour's remembered line:
/// an older answer, which must not replace a newer one.
fn is_older(previous: &OllamaHourRecord, next: &OllamaHourRecord) -> bool {
    next.requests < previous.requests
        || next.input < previous.input
        || next.cached_input < previous.cached_input
        || next.output < previous.output
        || next.usd < previous.usd
}

fn find_limit<'a>(sample: &'a Sample, limit: &LimitSample) -> Option<&'a LimitSample> {
    sample
        .l
        .iter()
        .find(|other| other.k == limit.k && other.m == limit.m)
}

/// How long a window of `kind` runs: a week for the two weekly kinds, 5 h for
/// the session; `None` for a kind this version does not know.
fn window_length(kind: &LimitKind) -> Option<Duration> {
    match kind {
        LimitKind::WeeklyAll | LimitKind::WeeklyScoped => Some(Duration::days(7)),
        LimitKind::Session => Some(Duration::hours(5)),
        LimitKind::Other(_) => None,
    }
}

/// The record of `limit`'s window, whose reset time is `reset_at`, ending at
/// `end` (the reset time, or the reading of a reset).
fn window_of(
    prev: &Sample,
    limit: &LimitSample,
    reset_at: DateTime<Utc>,
    end: DateTime<Utc>,
    plan: Option<&str>,
) -> WindowRecord {
    let lost = match &limit.k {
        LimitKind::WeeklyAll | LimitKind::WeeklyScoped => Some((100.0 - limit.p).max(0.0)),
        LimitKind::Session | LimitKind::Other(_) => None,
    };
    WindowRecord {
        account: prev.account.clone(),
        k: limit.k.clone(),
        m: limit.m.clone(),
        start: window_length(&limit.k).map(|length| reset_at - length),
        end,
        final_percent: limit.p,
        lost,
        last_sample: prev.t,
        plan: plan.map(str::to_string),
        uncertain: end.signed_duration_since(prev.t) > Duration::hours(1),
        reset: false,
        from: None,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    use std::fs;
    use std::io;
    use std::io::Write;
    use std::path::Path;

    use tempfile::tempdir;

    use crate::api::{Limit, Severity};

    use super::super::Range;

    fn utc(text: &str) -> DateTime<Utc> {
        text.parse::<DateTime<Utc>>().expect("test date")
    }

    fn atlas_account() -> AccountRef {
        AccountRef {
            acc: "6f1c2a9e-1d4b-4c7f-9a3e-8b5d2e6f1c04".to_string(),
            org: "a92d7b31-5c8e-4f2a-9d6b-1e3c5a7f9b20".to_string(),
        }
    }

    fn history(root: &Path) -> HistoryDir {
        HistoryDir::at(root, "atlas")
    }

    fn limit(kind: LimitKind, percent: f64, resets_at: Option<DateTime<Utc>>) -> Limit {
        Limit {
            kind,
            percent,
            severity: Severity::Normal,
            resets_at,
            model: None,
            is_active: false,
        }
    }

    fn weekly(percent: f64, resets_at: Option<DateTime<Utc>>) -> Usage {
        Usage {
            limits: vec![limit(LimitKind::WeeklyAll, percent, resets_at)],
            extra_usage_enabled: false,
            warnings: Vec::new(),
        }
    }

    fn scoped_weekly(model: &str, percent: f64, resets_at: Option<DateTime<Utc>>) -> Usage {
        let mut scoped = limit(LimitKind::WeeklyScoped, percent, resets_at);
        scoped.model = Some(model.to_string());
        Usage {
            limits: vec![scoped],
            extra_usage_enabled: false,
            warnings: Vec::new(),
        }
    }

    fn all_and_scoped(
        all: f64,
        all_reset: Option<DateTime<Utc>>,
        model: &str,
        scoped: f64,
        scoped_reset: Option<DateTime<Utc>>,
    ) -> Usage {
        let mut model_limit = limit(LimitKind::WeeklyScoped, scoped, scoped_reset);
        model_limit.model = Some(model.to_string());
        Usage {
            limits: vec![limit(LimitKind::WeeklyAll, all, all_reset), model_limit],
            extra_usage_enabled: false,
            warnings: Vec::new(),
        }
    }

    fn windows_of(root: &Path) -> Vec<WindowRecord> {
        let text = fs::read_to_string(root.join("atlas/windows-2026-11.jsonl")).unwrap();
        text.lines()
            .map(|line| serde_json::from_str(line).unwrap())
            .collect()
    }

    fn wide_range() -> Range {
        Range {
            from: DateTime::<Utc>::MIN_UTC,
            to: DateTime::<Utc>::MAX_UTC,
        }
    }

    fn limit_sample(
        kind: LimitKind,
        model: Option<&str>,
        percent: f64,
        resets_at: Option<DateTime<Utc>>,
    ) -> LimitSample {
        LimitSample {
            k: kind,
            m: model.map(str::to_string),
            p: percent,
            r: resets_at,
            b: false,
        }
    }

    fn sample(t: DateTime<Utc>, limits: Vec<LimitSample>) -> Sample {
        Sample {
            t,
            account: atlas_account(),
            src: Source::Poll,
            l: limits,
        }
    }

    fn line_count(path: &Path) -> usize {
        match fs::read_to_string(path) {
            Ok(text) => text.lines().count(),
            Err(e) if e.kind() == io::ErrorKind::NotFound => 0,
            Err(e) => panic!("{}: {e}", path.display()),
        }
    }

    fn sample_lines(root: &Path) -> usize {
        line_count(&root.join("atlas/samples-2026-11.jsonl"))
    }

    fn window_lines(root: &Path) -> usize {
        line_count(&root.join("atlas/windows-2026-11.jsonl"))
    }

    #[test]
    fn the_first_reading_writes_a_sample() {
        let dir = tempdir().unwrap();
        let mut recorder = Recorder::new(history(dir.path()));

        recorder
            .reading(
                &atlas_account(),
                None,
                &weekly(75.0, Some(utc("2026-11-16T16:00:00Z"))),
                utc("2026-11-14T14:02:11Z"),
                Source::Poll,
            )
            .unwrap();

        assert_eq!(sample_lines(dir.path()), 1);
    }

    #[test]
    fn an_unchanged_reading_within_the_hour_writes_nothing() {
        let dir = tempdir().unwrap();
        let mut recorder = Recorder::new(history(dir.path()));
        let usage = weekly(75.0, Some(utc("2026-11-16T16:00:00Z")));

        recorder
            .reading(
                &atlas_account(),
                None,
                &usage,
                utc("2026-11-14T14:02:11Z"),
                Source::Poll,
            )
            .unwrap();
        recorder
            .reading(
                &atlas_account(),
                None,
                &usage,
                utc("2026-11-14T14:04:11Z"),
                Source::Poll,
            )
            .unwrap();

        assert_eq!(sample_lines(dir.path()), 1);
    }

    #[test]
    fn an_unchanged_reading_after_an_hour_writes_again() {
        let dir = tempdir().unwrap();
        let mut recorder = Recorder::new(history(dir.path()));
        let usage = weekly(75.0, Some(utc("2026-11-16T16:00:00Z")));

        recorder
            .reading(
                &atlas_account(),
                None,
                &usage,
                utc("2026-11-14T14:02:11Z"),
                Source::Poll,
            )
            .unwrap();
        recorder
            .reading(
                &atlas_account(),
                None,
                &usage,
                utc("2026-11-14T15:02:11Z"),
                Source::Poll,
            )
            .unwrap();

        assert_eq!(sample_lines(dir.path()), 2);
    }

    #[test]
    fn a_changed_percent_writes_at_once() {
        let dir = tempdir().unwrap();
        let mut recorder = Recorder::new(history(dir.path()));
        let reset = Some(utc("2026-11-16T16:00:00Z"));

        recorder
            .reading(
                &atlas_account(),
                None,
                &weekly(75.0, reset),
                utc("2026-11-14T14:02:11Z"),
                Source::Poll,
            )
            .unwrap();
        recorder
            .reading(
                &atlas_account(),
                None,
                &weekly(76.0, reset),
                utc("2026-11-14T14:04:11Z"),
                Source::Poll,
            )
            .unwrap();

        assert_eq!(sample_lines(dir.path()), 2);
    }

    #[test]
    fn a_weekly_reset_closes_the_window() {
        let prev = sample(
            utc("2026-11-16T15:58:10Z"),
            vec![limit_sample(
                LimitKind::WeeklyAll,
                None,
                87.0,
                Some(utc("2026-11-16T16:00:00Z")),
            )],
        );
        let next = sample(
            utc("2026-11-16T16:02:00Z"),
            vec![limit_sample(
                LimitKind::WeeklyAll,
                None,
                1.0,
                Some(utc("2026-11-23T16:00:00Z")),
            )],
        );

        let windows = closed_windows(&prev, &next, None);

        assert_eq!(
            windows,
            vec![WindowRecord {
                account: atlas_account(),
                k: LimitKind::WeeklyAll,
                m: None,
                start: Some(utc("2026-11-09T16:00:00Z")),
                end: utc("2026-11-16T16:00:00Z"),
                final_percent: 87.0,
                lost: Some(13.0),
                last_sample: utc("2026-11-16T15:58:10Z"),
                plan: None,
                uncertain: false,
                reset: false,
                from: None,
            }]
        );
    }

    #[test]
    fn a_reset_seen_late_is_uncertain() {
        let prev = sample(
            utc("2026-11-15T09:00:00Z"),
            vec![limit_sample(
                LimitKind::WeeklyAll,
                None,
                87.0,
                Some(utc("2026-11-16T16:00:00Z")),
            )],
        );
        let next = sample(
            utc("2026-11-17T08:00:00Z"),
            vec![limit_sample(
                LimitKind::WeeklyAll,
                None,
                1.0,
                Some(utc("2026-11-23T16:00:00Z")),
            )],
        );

        let windows = closed_windows(&prev, &next, None);

        assert_eq!(windows.len(), 1);
        assert_eq!(windows[0].final_percent, 87.0);
        assert_eq!(windows[0].end, utc("2026-11-16T16:00:00Z"));
        assert!(windows[0].uncertain);
    }

    #[test]
    fn a_session_window_that_ends_has_no_lost() {
        let prev = sample(
            utc("2026-11-14T19:10:00Z"),
            vec![limit_sample(
                LimitKind::Session,
                None,
                64.0,
                Some(utc("2026-11-14T19:30:00Z")),
            )],
        );
        let next = sample(
            utc("2026-11-14T19:31:00Z"),
            vec![limit_sample(LimitKind::Session, None, 0.0, None)],
        );

        let windows = closed_windows(&prev, &next, None);

        assert_eq!(windows.len(), 1);
        assert_eq!(windows[0].k, LimitKind::Session);
        assert_eq!(windows[0].start, Some(utc("2026-11-14T14:30:00Z")));
        assert_eq!(windows[0].end, utc("2026-11-14T19:30:00Z"));
        assert_eq!(windows[0].final_percent, 64.0);
        assert_eq!(windows[0].lost, None);
    }

    #[test]
    fn a_session_without_a_reset_time_closes_nothing() {
        let prev = sample(
            utc("2026-11-14T19:10:00Z"),
            vec![limit_sample(LimitKind::Session, None, 64.0, None)],
        );
        let next = sample(
            utc("2026-11-14T19:12:00Z"),
            vec![limit_sample(LimitKind::Session, None, 65.0, None)],
        );

        assert!(closed_windows(&prev, &next, None).is_empty());
    }

    #[test]
    fn a_reset_time_that_moves_by_seconds_is_the_same_window() {
        let prev = sample(
            utc("2026-11-16T15:59:00Z"),
            vec![limit_sample(
                LimitKind::WeeklyAll,
                None,
                87.0,
                Some(utc("2026-11-16T16:00:00Z")),
            )],
        );
        let next = sample(
            utc("2026-11-16T15:59:30Z"),
            vec![limit_sample(
                LimitKind::WeeklyAll,
                None,
                87.0,
                Some(utc("2026-11-16T16:00:30Z")),
            )],
        );

        assert!(closed_windows(&prev, &next, None).is_empty());
    }

    #[test]
    fn over_100_percent_loses_nothing() {
        let prev = sample(
            utc("2026-11-16T15:58:10Z"),
            vec![limit_sample(
                LimitKind::WeeklyAll,
                None,
                103.0,
                Some(utc("2026-11-16T16:00:00Z")),
            )],
        );
        let next = sample(
            utc("2026-11-16T16:02:00Z"),
            vec![limit_sample(
                LimitKind::WeeklyAll,
                None,
                0.0,
                Some(utc("2026-11-23T16:00:00Z")),
            )],
        );

        let windows = closed_windows(&prev, &next, None);

        assert_eq!(windows.len(), 1);
        assert_eq!(windows[0].lost, Some(0.0));
    }

    #[test]
    fn crossing_100_writes_blocked_and_back_unblocked() {
        let dir = tempdir().unwrap();
        let mut recorder = Recorder::new(history(dir.path()));
        let account = atlas_account();
        let start = utc("2026-11-14T10:00:00Z");
        let reset = Some(utc("2026-11-16T16:00:00Z"));
        let later = Some(utc("2026-11-23T16:00:00Z"));

        let first = recorder
            .reading(&account, None, &weekly(99.0, reset), start, Source::Poll)
            .unwrap();
        assert!(first.is_empty());

        let blocked = recorder
            .reading(
                &account,
                None,
                &weekly(100.0, reset),
                start + chrono::Duration::minutes(2),
                Source::Poll,
            )
            .unwrap();
        assert_eq!(
            blocked,
            vec![EventKind::Blocked {
                account: account.clone(),
                k: LimitKind::WeeklyAll,
                m: None,
            }]
        );

        let still = recorder
            .reading(
                &account,
                None,
                &weekly(100.0, reset),
                start + chrono::Duration::minutes(3),
                Source::Poll,
            )
            .unwrap();
        assert!(still.is_empty());

        let unblocked = recorder
            .reading(
                &account,
                None,
                &weekly(3.0, later),
                start + chrono::Duration::minutes(4),
                Source::Poll,
            )
            .unwrap();
        assert_eq!(
            unblocked,
            vec![EventKind::Unblocked {
                account,
                k: LimitKind::WeeklyAll,
                m: None,
            }]
        );
    }

    #[test]
    fn the_last_line_is_found_again_after_a_restart() {
        let dir = tempdir().unwrap();
        let usage = weekly(75.0, Some(utc("2026-11-16T16:00:00Z")));

        let mut recorder = Recorder::new(history(dir.path()));
        recorder
            .reading(
                &atlas_account(),
                None,
                &usage,
                utc("2026-11-14T14:02:11Z"),
                Source::Poll,
            )
            .unwrap();
        drop(recorder);

        let mut restarted = Recorder::new(history(dir.path()));
        restarted
            .reading(
                &atlas_account(),
                None,
                &usage,
                utc("2026-11-14T14:04:11Z"),
                Source::Poll,
            )
            .unwrap();

        assert_eq!(sample_lines(dir.path()), 1);
    }

    #[test]
    fn load_once_warnings_reach_the_caller() {
        let dir = tempdir().unwrap();
        let mut first = Recorder::new(history(dir.path()));
        first
            .reading(
                &atlas_account(),
                None,
                &weekly(75.0, Some(utc("2026-11-16T16:00:00Z"))),
                utc("2026-11-14T14:02:11Z"),
                Source::Poll,
            )
            .unwrap();
        drop(first);

        // A crash left a torn last line (spec §1.2): reading names it.
        let path = dir.path().join("atlas/samples-2026-11.jsonl");
        let mut file = fs::OpenOptions::new().append(true).open(&path).unwrap();
        file.write_all(b"{\"t\":\"2026-11-14T15").unwrap();
        drop(file);

        let mut restarted = Recorder::new(history(dir.path()));
        restarted
            .reading(
                &atlas_account(),
                None,
                &weekly(76.0, Some(utc("2026-11-16T16:00:00Z"))),
                utc("2026-11-14T14:04:11Z"),
                Source::Poll,
            )
            .unwrap();

        let warnings = restarted.take_warnings();
        assert_eq!(warnings.len(), 1, "{warnings:?}");
        assert!(
            warnings[0].contains("samples-2026-11.jsonl"),
            "the warning names the file: {warnings:?}"
        );
        assert!(
            restarted.take_warnings().is_empty(),
            "a warning is returned once"
        );
    }

    #[test]
    fn up_writes_once_per_five_minutes() {
        let dir = tempdir().unwrap();
        let mut recorder = Recorder::new(history(dir.path()));

        recorder.up(utc("2026-11-14T10:00:00Z")).unwrap();
        recorder.up(utc("2026-11-14T10:02:00Z")).unwrap();
        recorder.up(utc("2026-11-14T10:05:00Z")).unwrap();

        assert_eq!(line_count(&dir.path().join("atlas/up-2026-11.jsonl")), 2);
    }

    fn an_ollama_hour(hour: &str, requests: u64, usd: f64) -> OllamaHourRecord {
        OllamaHourRecord {
            hour: utc(hour),
            requests,
            input: requests * 10,
            cached_input: requests * 6,
            output: requests * 2,
            usd,
        }
    }

    #[test]
    fn a_running_hour_is_replaced_by_its_later_bucket() {
        let dir = tempdir().unwrap();
        let mut recorder = Recorder::new(history(dir.path()));
        let first = an_ollama_hour("2026-11-14T09:00:00Z", 3, 0.02);
        let later = an_ollama_hour("2026-11-14T09:00:00Z", 9, 0.06);

        assert_eq!(
            recorder.ollama_hours(std::slice::from_ref(&first)).unwrap(),
            1
        );
        assert_eq!(
            recorder.ollama_hours(std::slice::from_ref(&later)).unwrap(),
            1
        );

        let read = history(dir.path())
            .read::<OllamaHourRecord>(&Range::ALL, &Hosts::All)
            .unwrap();
        assert_eq!(read.records.len(), 2, "the hour's first and later line");
        assert_eq!(read.records[0].record, first);
        assert_eq!(
            read.records[1].record, later,
            "the last line in file order is the later bucket"
        );

        // A later answer for the hour adds nothing.
        assert_eq!(
            recorder.ollama_hours(std::slice::from_ref(&later)).unwrap(),
            0
        );
        // A recorder that starts again reads the later line back as the hour's.
        let mut restarted = Recorder::new(history(dir.path()));
        assert_eq!(restarted.ollama_hours(&[later]).unwrap(), 0);
    }

    /// PROOF (review O-1): the plan says an hour is replaced only by a later
    /// answer; an answer with smaller numbers for a written hour (late or from
    /// a stale poll) must not become the hour's last line.
    #[test]
    fn proof_a_smaller_bucket_does_not_replace_a_bigger_one() {
        let dir = tempdir().unwrap();
        let mut recorder = Recorder::new(history(dir.path()));
        let big = an_ollama_hour("2026-11-14T09:00:00Z", 9, 0.06);
        let small = an_ollama_hour("2026-11-14T09:00:00Z", 3, 0.02);
        assert_eq!(
            recorder.ollama_hours(std::slice::from_ref(&big)).unwrap(),
            1
        );
        assert_eq!(
            recorder.ollama_hours(std::slice::from_ref(&small)).unwrap(),
            0,
            "the smaller bucket was appended after the bigger one"
        );
    }

    #[test]
    fn an_older_bucket_is_named_in_one_warning() {
        let dir = tempdir().unwrap();
        let mut recorder = Recorder::new(history(dir.path()));
        let big = an_ollama_hour("2026-11-14T09:00:00Z", 9, 0.06);
        let small = an_ollama_hour("2026-11-14T09:00:00Z", 3, 0.02);

        recorder.ollama_hours(&[big]).unwrap();
        assert!(
            recorder.take_warnings().is_empty(),
            "the first line of an hour warns about nothing"
        );

        recorder.ollama_hours(std::slice::from_ref(&small)).unwrap();
        let warnings = recorder.take_warnings();
        assert_eq!(warnings.len(), 1, "{warnings:?}");
        assert!(
            warnings[0].contains("2026-11-14T09:00:00"),
            "the warning names the hour: {warnings:?}"
        );

        recorder.ollama_hours(&[small]).unwrap();
        assert!(
            recorder.take_warnings().is_empty(),
            "the same older answer is named once"
        );
    }

    #[test]
    fn a_failed_month_file_leaves_the_written_month_remembered() {
        let dir = tempdir().unwrap();
        let mut recorder = Recorder::new(history(dir.path()));
        let november = an_ollama_hour("2026-11-14T09:00:00Z", 3, 0.02);
        let december = an_ollama_hour("2026-12-01T10:00:00Z", 4, 0.03);
        let november_file = dir.path().join("atlas/ollama-2026-11.jsonl");
        let december_file = dir.path().join("atlas/ollama-2026-12.jsonl");
        // A folder where December's file belongs makes that one write fail.
        fs::create_dir_all(&december_file).unwrap();

        recorder
            .ollama_hours(&[november.clone(), december.clone()])
            .expect_err("the December file cannot be written");
        assert_eq!(line_count(&november_file), 1, "November's line was written");

        fs::remove_dir(&december_file).unwrap();
        assert_eq!(
            recorder.ollama_hours(&[november, december]).unwrap(),
            1,
            "only December is still to write"
        );
        assert_eq!(
            line_count(&november_file),
            1,
            "November's line was written a second time"
        );
        assert_eq!(line_count(&december_file), 1);
    }

    #[test]
    fn an_old_bucket_already_written_is_not_duplicated() {
        let dir = tempdir().unwrap();
        let mut recorder = Recorder::new(history(dir.path()));
        let bucket = an_ollama_hour("2026-11-13T22:00:00Z", 17, 0.12);

        assert_eq!(
            recorder
                .ollama_hours(std::slice::from_ref(&bucket))
                .unwrap(),
            1
        );
        assert_eq!(
            recorder
                .ollama_hours(std::slice::from_ref(&bucket))
                .unwrap(),
            0,
            "the same 24h answer came again"
        );

        // A new recorder reads the line from this host's month file: still no
        // second line.
        let mut restarted = Recorder::new(history(dir.path()));
        assert_eq!(restarted.ollama_hours(&[bucket]).unwrap(), 0);
        assert_eq!(
            line_count(&dir.path().join("atlas/ollama-2026-11.jsonl")),
            1
        );
    }

    #[test]
    fn a_reset_that_moves_later_by_minutes_is_the_same_window() {
        let prev = sample(
            utc("2026-11-16T09:08:21Z"),
            vec![limit_sample(
                LimitKind::WeeklyAll,
                None,
                0.0,
                Some(utc("2026-11-23T09:09:00Z")),
            )],
        );
        let moved = sample(
            utc("2026-11-16T09:13:21Z"),
            vec![limit_sample(
                LimitKind::WeeklyAll,
                None,
                0.0,
                Some(utc("2026-11-23T09:14:00Z")),
            )],
        );
        assert!(closed_windows(&prev, &moved, None).is_empty());

        // A whole week later is the next window, also when this computer's
        // clock is behind and the reading still lies before the old reset.
        let next_week = sample(
            utc("2026-11-23T09:08:00Z"),
            vec![limit_sample(
                LimitKind::WeeklyAll,
                None,
                0.0,
                Some(utc("2026-11-30T09:09:00Z")),
            )],
        );
        let windows = closed_windows(&prev, &next_week, None);
        assert_eq!(windows.len(), 1);
        assert_eq!(windows[0].end, utc("2026-11-23T09:09:00Z"));
    }

    #[test]
    fn an_unused_limit_whose_reset_moves_with_each_reading_writes_one_window() {
        let dir = tempdir().unwrap();
        let mut recorder = Recorder::new(history(dir.path()));
        let session = |percent: f64, resets_at: Option<DateTime<Utc>>| Usage {
            limits: vec![limit(LimitKind::Session, percent, resets_at)],
            extra_usage_enabled: false,
            warnings: Vec::new(),
        };
        // Unused, the limit names a reset five hours after every reading.
        let start = utc("2026-11-16T09:08:21Z");
        for poll in 0..12 {
            let at = start + Duration::minutes(5 * poll);
            recorder
                .reading(
                    &atlas_account(),
                    None,
                    &session(0.0, Some(at + Duration::hours(5))),
                    at,
                    Source::Poll,
                )
                .unwrap();
        }
        assert_eq!(window_lines(dir.path()), 0);

        // Used, its reset stays; the window ends there, once.
        let used_at = start + Duration::hours(1);
        let reset = utc("2026-11-16T15:09:00Z");
        for at in [used_at, used_at + Duration::minutes(5)] {
            recorder
                .reading(
                    &atlas_account(),
                    None,
                    &session(40.0, Some(reset)),
                    at,
                    Source::Poll,
                )
                .unwrap();
        }
        recorder
            .reading(
                &atlas_account(),
                None,
                &session(0.0, None),
                reset + Duration::minutes(1),
                Source::Poll,
            )
            .unwrap();
        let windows = windows_of(dir.path());
        assert_eq!(windows.len(), 1, "{windows:?}");
        assert_eq!(windows[0].end, reset);
        assert_eq!(windows[0].final_percent, 40.0);
    }

    #[test]
    fn a_window_is_written_once_while_the_server_still_sends_the_old_reset() {
        let dir = tempdir().unwrap();
        let usage = weekly(87.0, Some(utc("2026-11-16T16:00:00Z")));

        let mut recorder = Recorder::new(history(dir.path()));
        recorder
            .reading(
                &atlas_account(),
                None,
                &usage,
                utc("2026-11-16T15:58:10Z"),
                Source::Poll,
            )
            .unwrap();
        assert_eq!(window_lines(dir.path()), 0);
        recorder
            .reading(
                &atlas_account(),
                None,
                &usage,
                utc("2026-11-16T16:01:00Z"),
                Source::Poll,
            )
            .unwrap();
        assert_eq!(window_lines(dir.path()), 1);
        recorder
            .reading(
                &atlas_account(),
                None,
                &usage,
                utc("2026-11-16T16:03:00Z"),
                Source::Poll,
            )
            .unwrap();
        assert_eq!(window_lines(dir.path()), 1);
        drop(recorder);

        let mut restarted = Recorder::new(history(dir.path()));
        restarted
            .reading(
                &atlas_account(),
                None,
                &usage,
                utc("2026-11-16T16:05:00Z"),
                Source::Poll,
            )
            .unwrap();
        assert_eq!(window_lines(dir.path()), 1);
    }

    #[test]
    fn a_limit_missing_once_does_not_close_its_window() {
        let dir = tempdir().unwrap();
        let mut recorder = Recorder::new(history(dir.path()));
        let account = atlas_account();
        let reset = Some(utc("2026-11-16T16:00:00Z"));

        recorder
            .reading(
                &account,
                None,
                &scoped_weekly("Fable", 50.0, reset),
                utc("2026-11-14T10:00:00Z"),
                Source::Poll,
            )
            .unwrap();
        let session_only = Usage {
            limits: vec![limit(
                LimitKind::Session,
                20.0,
                Some(utc("2026-11-14T14:00:00Z")),
            )],
            extra_usage_enabled: false,
            warnings: Vec::new(),
        };
        recorder
            .reading(
                &account,
                None,
                &session_only,
                utc("2026-11-14T10:02:00Z"),
                Source::Statusline,
            )
            .unwrap();
        recorder
            .reading(
                &account,
                None,
                &scoped_weekly("Fable", 87.0, reset),
                utc("2026-11-14T10:04:00Z"),
                Source::Poll,
            )
            .unwrap();
        recorder
            .reading(
                &account,
                None,
                &scoped_weekly("Fable", 87.0, reset),
                utc("2026-11-16T15:58:00Z"),
                Source::Poll,
            )
            .unwrap();
        recorder
            .reading(
                &account,
                None,
                &scoped_weekly("Fable", 1.0, Some(utc("2026-11-23T16:00:00Z"))),
                utc("2026-11-16T16:02:00Z"),
                Source::Poll,
            )
            .unwrap();

        let read = history(dir.path())
            .read::<WindowRecord>(&wide_range(), &Hosts::All)
            .unwrap();
        let scoped: Vec<&WindowRecord> = read
            .records
            .iter()
            .map(|hosted| &hosted.record)
            .filter(|window| {
                window.k == LimitKind::WeeklyScoped && window.m.as_deref() == Some("Fable")
            })
            .collect();
        assert_eq!(scoped.len(), 1);
        assert_eq!(scoped[0].final_percent, 87.0);
        assert!(!scoped[0].uncertain);
        assert_eq!(scoped[0].last_sample, utc("2026-11-16T15:58:00Z"));
    }

    #[test]
    fn blocked_is_not_repeated_after_a_limit_blinks() {
        let dir = tempdir().unwrap();
        let mut recorder = Recorder::new(history(dir.path()));
        let account = atlas_account();
        let start = utc("2026-11-14T10:00:00Z");
        let usage = weekly(100.0, Some(utc("2026-11-16T16:00:00Z")));

        let blocked = recorder
            .reading(&account, None, &usage, start, Source::Poll)
            .unwrap();
        assert_eq!(
            blocked,
            vec![EventKind::Blocked {
                account: account.clone(),
                k: LimitKind::WeeklyAll,
                m: None,
            }]
        );

        let no_limits = Usage {
            limits: vec![],
            extra_usage_enabled: false,
            warnings: Vec::new(),
        };
        let nothing = recorder
            .reading(
                &account,
                None,
                &no_limits,
                start + chrono::Duration::minutes(2),
                Source::Poll,
            )
            .unwrap();
        assert!(nothing.is_empty());

        let again = recorder
            .reading(
                &account,
                None,
                &usage,
                start + chrono::Duration::minutes(4),
                Source::Poll,
            )
            .unwrap();
        assert!(again.is_empty());
    }

    #[test]
    fn last_sample_is_the_last_reading() {
        let dir = tempdir().unwrap();
        let mut recorder = Recorder::new(history(dir.path()));
        let account = atlas_account();
        let reset = Some(utc("2026-11-16T16:00:00Z"));

        recorder
            .reading(
                &account,
                None,
                &weekly(87.0, reset),
                utc("2026-11-16T14:59:00Z"),
                Source::Poll,
            )
            .unwrap();
        let mut at = utc("2026-11-16T15:00:50Z");
        let last = utc("2026-11-16T15:58:50Z");
        while at <= last {
            recorder
                .reading(&account, None, &weekly(87.0, reset), at, Source::Poll)
                .unwrap();
            at += chrono::Duration::minutes(2);
        }
        recorder
            .reading(
                &account,
                None,
                &weekly(1.0, Some(utc("2026-11-23T16:00:00Z"))),
                utc("2026-11-16T16:00:50Z"),
                Source::Poll,
            )
            .unwrap();

        let read = history(dir.path())
            .read::<WindowRecord>(&wide_range(), &Hosts::All)
            .unwrap();
        assert_eq!(read.records.len(), 1);
        assert!(!read.records[0].record.uncertain);
        assert_eq!(
            read.records[0].record.last_sample,
            utc("2026-11-16T15:58:50Z")
        );
    }

    #[test]
    fn a_window_is_found_when_the_last_line_is_two_months_back() {
        let dir = tempdir().unwrap();
        let account = atlas_account();

        let mut first = Recorder::new(history(dir.path()));
        first
            .reading(
                &account,
                None,
                &weekly(87.0, Some(utc("2026-11-02T16:00:00Z"))),
                utc("2026-10-31T23:00:00Z"),
                Source::Poll,
            )
            .unwrap();
        drop(first);

        let mut second = Recorder::new(history(dir.path()));
        second
            .reading(
                &account,
                None,
                &weekly(1.0, Some(utc("2026-12-07T16:00:00Z"))),
                utc("2026-12-01T00:05:00Z"),
                Source::Poll,
            )
            .unwrap();

        let text = fs::read_to_string(dir.path().join("atlas/windows-2026-11.jsonl")).unwrap();
        assert_eq!(text.lines().count(), 1);
        let window: WindowRecord = serde_json::from_str(text.lines().next().unwrap()).unwrap();
        assert_eq!(window.final_percent, 87.0);
        assert!(window.uncertain);
    }

    #[test]
    fn blocked_events_are_in_the_events_file() {
        let dir = tempdir().unwrap();
        let mut recorder = Recorder::new(history(dir.path()));
        let account = atlas_account();
        let start = utc("2026-11-14T10:00:00Z");
        let reset = Some(utc("2026-11-16T16:00:00Z"));

        recorder
            .reading(&account, None, &weekly(99.0, reset), start, Source::Poll)
            .unwrap();
        recorder
            .reading(
                &account,
                None,
                &weekly(100.0, reset),
                start + chrono::Duration::minutes(2),
                Source::Poll,
            )
            .unwrap();

        let text = fs::read_to_string(dir.path().join("atlas/events-2026-11.jsonl")).unwrap();
        assert_eq!(text.lines().count(), 1);
        assert!(text.contains("\"type\":\"blocked\""));
    }

    #[test]
    fn a_scoped_limit_survives_a_restart_after_a_status_line_sample() {
        let dir = tempdir().unwrap();
        let account = atlas_account();

        let mut recorder = Recorder::new(history(dir.path()));
        recorder
            .reading(
                &account,
                None,
                &all_and_scoped(
                    40.0,
                    Some(utc("2026-11-20T18:00:00Z")),
                    "Fable",
                    87.0,
                    Some(utc("2026-11-16T16:00:00Z")),
                ),
                utc("2026-11-14T10:00:00Z"),
                Source::Poll,
            )
            .unwrap();
        recorder
            .reading(
                &account,
                None,
                &weekly(41.0, None),
                utc("2026-11-14T10:05:00Z"),
                Source::Statusline,
            )
            .unwrap();
        drop(recorder);

        let mut restarted = Recorder::new(history(dir.path()));
        restarted
            .reading(
                &account,
                None,
                &all_and_scoped(50.0, None, "Fable", 1.0, Some(utc("2026-11-23T16:00:00Z"))),
                utc("2026-11-16T16:02:00Z"),
                Source::Poll,
            )
            .unwrap();

        let scoped: Vec<WindowRecord> = windows_of(dir.path())
            .into_iter()
            .filter(|window| {
                window.k == LimitKind::WeeklyScoped && window.m.as_deref() == Some("Fable")
            })
            .collect();
        assert_eq!(scoped.len(), 1);
        assert_eq!(scoped[0].final_percent, 87.0);
        assert_eq!(scoped[0].last_sample, utc("2026-11-14T10:00:00Z"));
    }

    #[test]
    fn a_full_limit_that_ends_unseen_writes_unblocked() {
        let dir = tempdir().unwrap();
        let account = atlas_account();

        let mut recorder = Recorder::new(history(dir.path()));
        let blocked = recorder
            .reading(
                &account,
                None,
                &all_and_scoped(
                    40.0,
                    None,
                    "Fable",
                    100.0,
                    Some(utc("2026-11-16T16:00:00Z")),
                ),
                utc("2026-11-14T10:00:00Z"),
                Source::Poll,
            )
            .unwrap();
        assert_eq!(
            blocked,
            vec![EventKind::Blocked {
                account: account.clone(),
                k: LimitKind::WeeklyScoped,
                m: Some("Fable".to_string()),
            }]
        );

        let quiet = recorder
            .reading(
                &account,
                None,
                &weekly(41.0, None),
                utc("2026-11-15T10:00:00Z"),
                Source::Statusline,
            )
            .unwrap();
        assert!(quiet.is_empty());

        let unblocked = recorder
            .reading(
                &account,
                None,
                &weekly(42.0, None),
                utc("2026-11-16T16:01:00Z"),
                Source::Statusline,
            )
            .unwrap();
        assert_eq!(
            unblocked,
            vec![EventKind::Unblocked {
                account: account.clone(),
                k: LimitKind::WeeklyScoped,
                m: Some("Fable".to_string()),
            }]
        );

        let events = fs::read_to_string(dir.path().join("atlas/events-2026-11.jsonl")).unwrap();
        assert_eq!(
            events
                .lines()
                .filter(|line| line.contains("\"type\":\"unblocked\""))
                .count(),
            1
        );

        let scoped: Vec<WindowRecord> = windows_of(dir.path())
            .into_iter()
            .filter(|window| {
                window.k == LimitKind::WeeklyScoped && window.m.as_deref() == Some("Fable")
            })
            .collect();
        assert_eq!(scoped.len(), 1);
        assert_eq!(scoped[0].final_percent, 100.0);
    }

    #[test]
    fn a_restart_does_not_repeat_unblocked() {
        let dir = tempdir().unwrap();
        let account = atlas_account();
        let fable_reset = Some(utc("2026-11-16T16:00:00Z"));

        let mut recorder = Recorder::new(history(dir.path()));
        recorder
            .reading(
                &account,
                None,
                &all_and_scoped(40.0, None, "Fable", 100.0, fable_reset),
                utc("2026-11-14T10:00:00Z"),
                Source::Poll,
            )
            .unwrap();
        let first = recorder
            .reading(
                &account,
                None,
                &weekly(42.0, None),
                utc("2026-11-16T16:01:00Z"),
                Source::Statusline,
            )
            .unwrap();
        assert_eq!(first.len(), 1);
        drop(recorder);

        let mut restarted = Recorder::new(history(dir.path()));
        let again = restarted
            .reading(
                &account,
                None,
                &weekly(43.0, None),
                utc("2026-11-16T16:03:00Z"),
                Source::Statusline,
            )
            .unwrap();

        assert!(again.is_empty());
        let events = fs::read_to_string(dir.path().join("atlas/events-2026-11.jsonl")).unwrap();
        assert_eq!(
            events
                .lines()
                .filter(|line| line.contains("\"type\":\"unblocked\""))
                .count(),
            1
        );
        assert_eq!(window_lines(dir.path()), 1);
    }

    #[test]
    fn a_reset_time_that_jitters_after_the_end_writes_the_window_once() {
        let dir = tempdir().unwrap();
        let account = atlas_account();
        let mut recorder = Recorder::new(history(dir.path()));
        for (at, percent, reset) in [
            ("2026-11-16T15:58:00Z", 87.0, "2026-11-16T16:00:00Z"),
            ("2026-11-16T16:00:30Z", 88.0, "2026-11-16T16:01:00Z"),
            ("2026-11-16T16:02:00Z", 1.0, "2026-11-23T16:00:00Z"),
        ] {
            recorder
                .reading(
                    &account,
                    None,
                    &weekly(percent, Some(utc(reset))),
                    utc(at),
                    Source::Poll,
                )
                .unwrap();
        }

        let windows = windows_of(dir.path());
        assert_eq!(windows.len(), 1);
        assert_eq!(windows[0].end, utc("2026-11-16T16:00:00Z"));
        assert_eq!(windows[0].final_percent, 87.0);
    }

    /// One reading after another for the atlas account, all polled.
    fn readings(recorder: &mut Recorder, list: &[(&str, Usage)]) {
        for (at, usage) in list {
            recorder
                .reading(&atlas_account(), None, usage, utc(at), Source::Poll)
                .unwrap();
        }
    }

    /// A window's own use: `final − from`, `from` absent = 0.
    fn own_use(window: &WindowRecord) -> f64 {
        window.final_percent - window.from.unwrap_or(0.0)
    }

    #[test]
    fn a_drop_to_zero_before_the_reset_time_ends_the_window() {
        let dir = tempdir().unwrap();
        let mut recorder = Recorder::new(history(dir.path()));
        let friday = Some(utc("2026-11-20T10:00:00Z"));
        readings(
            &mut recorder,
            &[
                ("2026-11-16T09:00:00Z", weekly(40.0, friday)),
                ("2026-11-17T12:00:00Z", weekly(62.0, friday)),
                ("2026-11-18T08:30:00Z", weekly(0.0, friday)),
            ],
        );

        let windows = windows_of(dir.path());
        assert_eq!(
            windows,
            vec![WindowRecord {
                account: atlas_account(),
                k: LimitKind::WeeklyAll,
                m: None,
                start: Some(utc("2026-11-13T10:00:00Z")),
                end: utc("2026-11-18T08:30:00Z"),
                final_percent: 62.0,
                lost: None,
                last_sample: utc("2026-11-17T12:00:00Z"),
                plan: None,
                uncertain: true,
                reset: true,
                from: None,
            }]
        );

        // The open window starts at the 0 % reading: its record says so when
        // it ends at Friday's reset.
        readings(
            &mut recorder,
            &[
                ("2026-11-18T15:00:00Z", weekly(30.0, friday)),
                (
                    "2026-11-20T10:02:00Z",
                    weekly(1.0, Some(utc("2026-11-27T10:00:00Z"))),
                ),
            ],
        );
        let windows = windows_of(dir.path());
        assert_eq!(windows.len(), 2, "{windows:?}");
        let after = &windows[1];
        assert_eq!(after.start, Some(utc("2026-11-18T08:30:00Z")));
        assert_eq!(after.end, utc("2026-11-20T10:00:00Z"));
        assert_eq!(after.final_percent, 30.0);
        assert_eq!(after.from, Some(0.0));
        assert!(!after.reset);
        assert_eq!(after.lost, Some(70.0));
    }

    #[test]
    fn a_model_reset_splits_the_model_and_the_week() {
        let dir = tempdir().unwrap();
        let mut recorder = Recorder::new(history(dir.path()));
        let friday = Some(utc("2026-11-20T10:00:00Z"));
        let next_friday = Some(utc("2026-11-27T10:00:00Z"));
        readings(
            &mut recorder,
            &[
                (
                    "2026-11-16T09:00:00Z",
                    all_and_scoped(70.0, friday, "Fable", 50.0, friday),
                ),
                (
                    "2026-11-18T08:30:00Z",
                    all_and_scoped(45.0, friday, "Fable", 0.0, friday),
                ),
            ],
        );

        let windows = windows_of(dir.path());
        assert_eq!(windows.len(), 2, "{windows:?}");
        for window in &windows {
            assert!(window.reset, "{window:?}");
            assert_eq!(window.end, utc("2026-11-18T08:30:00Z"));
            assert_eq!(window.lost, None);
        }
        let fable = windows
            .iter()
            .find(|window| window.k == LimitKind::WeeklyScoped)
            .unwrap();
        assert_eq!(fable.m.as_deref(), Some("Fable"));
        assert_eq!(fable.final_percent, 50.0);

        let later = 60.0;
        readings(
            &mut recorder,
            &[
                (
                    "2026-11-19T12:00:00Z",
                    all_and_scoped(later, friday, "Fable", 20.0, friday),
                ),
                (
                    "2026-11-20T10:02:00Z",
                    all_and_scoped(1.0, next_friday, "Fable", 0.0, next_friday),
                ),
            ],
        );
        let week: Vec<WindowRecord> = windows_of(dir.path())
            .into_iter()
            .filter(|window| window.k == LimitKind::WeeklyAll)
            .collect();
        assert_eq!(week.len(), 2, "{week:?}");
        assert_eq!(week[1].start, Some(utc("2026-11-18T08:30:00Z")));
        assert_eq!(week[1].from, Some(45.0));
        let sum: f64 = week.iter().map(own_use).sum();
        assert_eq!(sum, 70.0 + (later - 45.0));
    }

    #[test]
    fn a_drop_seen_after_a_restart_is_a_reset() {
        let dir = tempdir().unwrap();
        let mut first = Recorder::new(history(dir.path()));
        readings(
            &mut first,
            &[(
                "2026-11-16T10:00:00Z",
                weekly(80.0, Some(utc("2026-11-20T10:00:00Z"))),
            )],
        );
        drop(first);

        // seatswap was off for 20 h; the reset time moved, still in the future.
        let moved = Some(utc("2026-11-24T06:00:00Z"));
        let mut second = Recorder::new(history(dir.path()));
        readings(&mut second, &[("2026-11-17T06:00:00Z", weekly(5.0, moved))]);

        let windows = windows_of(dir.path());
        assert_eq!(windows.len(), 1, "{windows:?}");
        assert!(windows[0].reset);
        assert_eq!(windows[0].end, utc("2026-11-17T06:00:00Z"));
        assert_eq!(windows[0].final_percent, 80.0);
        assert_eq!(windows[0].last_sample, utc("2026-11-16T10:00:00Z"));
        assert_eq!(windows[0].lost, None);
        assert!(windows[0].uncertain);
        drop(second);

        // A later restart still knows where the window after the reset began
        // and its first percent.
        let mut third = Recorder::new(history(dir.path()));
        readings(
            &mut third,
            &[
                ("2026-11-19T09:00:00Z", weekly(30.0, moved)),
                (
                    "2026-11-24T06:02:00Z",
                    weekly(2.0, Some(utc("2026-12-01T06:00:00Z"))),
                ),
            ],
        );
        let windows = windows_of(dir.path());
        assert_eq!(windows.len(), 2, "{windows:?}");
        assert_eq!(windows[1].start, Some(utc("2026-11-17T06:00:00Z")));
        assert_eq!(windows[1].from, Some(5.0));
        assert!(!windows[1].reset);
        assert!(third.take_warnings().is_empty());
    }

    #[test]
    fn a_small_drop_is_kept_and_logged_not_a_reset() {
        let dir = tempdir().unwrap();
        let mut recorder = Recorder::new(history(dir.path()));
        let friday = Some(utc("2026-11-20T10:00:00Z"));
        readings(
            &mut recorder,
            &[
                ("2026-11-16T09:00:00Z", weekly(50.0, friday)),
                ("2026-11-16T09:02:00Z", weekly(49.6, friday)),
            ],
        );

        assert_eq!(window_lines(dir.path()), 0);
        assert_eq!(sample_lines(dir.path()), 2, "the lower reading is kept");
        let warnings = recorder.take_warnings();
        assert_eq!(warnings.len(), 1, "{warnings:?}");
        assert!(
            warnings[0].contains("went down by 0.4 points without a reset"),
            "{warnings:?}"
        );

        // Once per window: a second small drop says nothing new.
        readings(
            &mut recorder,
            &[("2026-11-16T09:04:00Z", weekly(49.3, friday))],
        );
        assert!(recorder.take_warnings().is_empty());
        assert_eq!(window_lines(dir.path()), 0);
    }

    #[test]
    fn a_statusline_reading_is_not_a_reset_baseline() {
        // The feed merges a status-line percent upward, so 62.0 (poll) → 64.0
        // (status line) → 62.9 (poll) is not a drop of the usage. A reset is
        // measured against the last poll reading, 62.0, not 64.0.
        let dir = tempdir().unwrap();
        let mut recorder = Recorder::new(history(dir.path()));
        let reset = Some(utc("2026-11-20T10:00:00Z"));
        for (at, percent, src) in [
            ("2026-11-16T09:00:00Z", 62.0, Source::Poll),
            ("2026-11-16T09:05:00Z", 64.0, Source::Statusline),
            ("2026-11-16T09:10:00Z", 62.9, Source::Poll),
        ] {
            recorder
                .reading(
                    &atlas_account(),
                    None,
                    &weekly(percent, reset),
                    utc(at),
                    src,
                )
                .unwrap();
        }

        assert_eq!(window_lines(dir.path()), 0, "a status line is no reset");
        assert!(
            recorder.take_warnings().is_empty(),
            "a status line's higher reading is no drop to warn about"
        );
    }

    #[test]
    fn a_poll_drop_after_a_statusline_reading_is_a_reset_from_the_poll() {
        let dir = tempdir().unwrap();
        let mut recorder = Recorder::new(history(dir.path()));
        let reset = Some(utc("2026-11-20T10:00:00Z"));
        for (at, percent, src) in [
            ("2026-11-16T09:00:00Z", 62.0, Source::Poll),
            ("2026-11-16T09:05:00Z", 64.0, Source::Statusline),
            ("2026-11-16T09:10:00Z", 40.0, Source::Poll),
        ] {
            recorder
                .reading(
                    &atlas_account(),
                    None,
                    &weekly(percent, reset),
                    utc(at),
                    src,
                )
                .unwrap();
        }

        let windows = windows_of(dir.path());
        assert_eq!(windows.len(), 1, "{windows:?}");
        assert!(windows[0].reset, "{:?}", windows[0]);
        assert_eq!(
            windows[0].final_percent, 62.0,
            "the reset is measured from the poll's 62.0"
        );
        assert_eq!(windows[0].last_sample, utc("2026-11-16T09:00:00Z"));
    }

    #[test]
    fn a_reset_at_the_reset_time_is_an_ordinary_window_end() {
        let dir = tempdir().unwrap();
        let mut recorder = Recorder::new(history(dir.path()));
        readings(
            &mut recorder,
            &[
                (
                    "2026-11-16T15:58:00Z",
                    weekly(87.0, Some(utc("2026-11-16T16:00:00Z"))),
                ),
                (
                    "2026-11-16T16:02:00Z",
                    weekly(1.0, Some(utc("2026-11-23T16:00:00Z"))),
                ),
            ],
        );

        let windows = windows_of(dir.path());
        assert_eq!(windows.len(), 1, "{windows:?}");
        assert!(!windows[0].reset);
        assert_eq!(windows[0].end, utc("2026-11-16T16:00:00Z"));
        assert_eq!(windows[0].lost, Some(13.0));
        assert_eq!(windows[0].from, None);
    }

    #[test]
    fn a_drop_seconds_before_the_reset_time_is_an_ordinary_window_end() {
        let dir = tempdir().unwrap();
        let mut recorder = Recorder::new(history(dir.path()));
        let reset = Some(utc("2026-11-16T16:00:00Z"));
        readings(
            &mut recorder,
            &[
                ("2026-11-16T15:58:00Z", weekly(87.0, reset)),
                // This computer's clock is 30 s behind the server's.
                ("2026-11-16T15:59:30Z", weekly(0.0, reset)),
                (
                    "2026-11-16T16:02:00Z",
                    weekly(1.0, Some(utc("2026-11-23T16:00:00Z"))),
                ),
            ],
        );

        let windows = windows_of(dir.path());
        assert_eq!(windows.len(), 1, "{windows:?}");
        assert!(!windows[0].reset);
        assert_eq!(windows[0].end, utc("2026-11-16T16:00:00Z"));
        assert_eq!(windows[0].final_percent, 87.0);
        assert_eq!(windows[0].lost, Some(13.0));
    }

    #[test]
    fn a_reset_of_a_limit_of_unknown_length_survives_a_restart() {
        let dir = tempdir().unwrap();
        let credits = |percent: f64, reset: &str| Usage {
            limits: vec![limit(
                LimitKind::Other("monthly_credits".to_string()),
                percent,
                Some(utc(reset)),
            )],
            extra_usage_enabled: false,
            warnings: Vec::new(),
        };
        let mut first = Recorder::new(history(dir.path()));
        readings(
            &mut first,
            &[
                (
                    "2026-11-09T09:00:00Z",
                    credits(50.0, "2026-11-10T00:00:00Z"),
                ),
                // An ordinary end on the 10th, then a reset on the 18th.
                (
                    "2026-11-10T00:02:00Z",
                    credits(10.0, "2026-11-30T00:00:00Z"),
                ),
                (
                    "2026-11-16T09:00:00Z",
                    credits(70.0, "2026-11-30T00:00:00Z"),
                ),
                ("2026-11-18T08:30:00Z", credits(4.0, "2026-11-30T00:00:00Z")),
            ],
        );
        drop(first);

        let mut restarted = Recorder::new(history(dir.path()));
        readings(
            &mut restarted,
            &[
                (
                    "2026-11-19T09:00:00Z",
                    credits(25.0, "2026-11-30T00:00:00Z"),
                ),
                ("2026-11-30T00:02:00Z", credits(2.0, "2026-12-31T00:00:00Z")),
            ],
        );

        let windows = windows_of(dir.path());
        assert_eq!(windows.len(), 3, "{windows:?}");
        assert!(windows[1].reset);
        let after = &windows[2];
        assert_eq!(after.end, utc("2026-11-30T00:00:00Z"));
        assert_eq!(after.start, Some(utc("2026-11-18T08:30:00Z")));
        assert_eq!(after.from, Some(4.0));
        assert!(restarted.take_warnings().is_empty());
    }

    #[test]
    fn a_reset_whose_first_sample_is_missing_is_uncertain_and_named() {
        let dir = tempdir().unwrap();
        let dir_history = history(dir.path());
        let friday = Some(utc("2026-11-20T10:00:00Z"));
        // A reset was recorded on Tuesday, but its sample is not on this host.
        dir_history
            .append(&WindowRecord {
                account: atlas_account(),
                k: LimitKind::WeeklyAll,
                m: None,
                start: Some(utc("2026-11-13T10:00:00Z")),
                end: utc("2026-11-17T06:00:00Z"),
                final_percent: 80.0,
                lost: None,
                last_sample: utc("2026-11-17T05:58:00Z"),
                plan: None,
                uncertain: false,
                reset: true,
                from: None,
            })
            .unwrap();
        dir_history
            .append(&sample(
                utc("2026-11-18T09:00:00Z"),
                vec![limit_sample(LimitKind::WeeklyAll, None, 30.0, friday)],
            ))
            .unwrap();

        let mut recorder = Recorder::new(history(dir.path()));
        readings(
            &mut recorder,
            &[(
                "2026-11-20T10:02:00Z",
                weekly(1.0, Some(utc("2026-11-27T10:00:00Z"))),
            )],
        );

        let windows = windows_of(dir.path());
        assert_eq!(windows.len(), 2, "{windows:?}");
        assert_eq!(windows[1].start, Some(utc("2026-11-17T06:00:00Z")));
        assert_eq!(windows[1].from, None);
        assert!(windows[1].uncertain);
        let warnings = recorder.take_warnings();
        assert_eq!(warnings.len(), 1, "{warnings:?}");
        assert!(warnings[0].contains("weekly_all"), "{warnings:?}");
        assert!(warnings[0].contains(&atlas_account().acc), "{warnings:?}");
    }

    /// Proof (review of R-1): two hosts poll the same account and each records
    /// the one reset at its own reading time (here 90 s apart, more than
    /// `RESET_JITTER`). A reader that merges the hosts sees one reset, not two.
    #[test]
    fn two_hosts_recording_one_reset_read_as_one_window() {
        let dir = tempdir().unwrap();
        let friday = Some(utc("2026-11-20T10:00:00Z"));
        for (host, at) in [
            ("atlas", "2026-11-18T08:30:00Z"),
            ("vega", "2026-11-18T08:31:30Z"),
        ] {
            let mut recorder = Recorder::new(HistoryDir::at(dir.path(), host));
            readings(
                &mut recorder,
                &[
                    ("2026-11-17T12:00:00Z", weekly(62.0, friday)),
                    (at, weekly(0.0, friday)),
                ],
            );
        }
        // The second host is another computer: its folder needs a marker.
        if !dir.path().join("vega/host.json").exists() {
            fs::write(
                dir.path().join("vega/host.json"),
                r#"{"machineId":"vega-machine"}"#,
            )
            .unwrap();
        }
        let read = history(dir.path())
            .finished_windows(&wide_range(), &Hosts::All)
            .unwrap();
        let resets: Vec<_> = read.records.iter().filter(|w| w.record.reset).collect();
        assert_eq!(
            resets.len(),
            1,
            "one reset is read as {} windows: {:?}",
            resets.len(),
            read.records
        );
    }

    /// Proof (second review of R-1): host atlas records a reset and the end of
    /// the window after it; host vega was off over the reset and records only
    /// the ordinary end with its old reading. A reader gets atlas's two windows.
    #[test]
    fn a_host_off_over_a_reset_adds_no_window_to_the_reader() {
        let dir = tempdir().unwrap();
        let friday = Some(utc("2026-11-20T10:00:00Z"));
        let next = Some(utc("2026-11-27T10:00:00Z"));
        let mut atlas = Recorder::new(HistoryDir::at(dir.path(), "atlas"));
        readings(
            &mut atlas,
            &[
                ("2026-11-17T12:00:00Z", weekly(62.0, friday)),
                ("2026-11-18T08:30:00Z", weekly(0.0, friday)),
                ("2026-11-19T12:00:00Z", weekly(30.0, friday)),
                ("2026-11-20T10:02:00Z", weekly(1.0, next)),
            ],
        );
        let mut vega = Recorder::new(HistoryDir::at(dir.path(), "vega"));
        readings(
            &mut vega,
            &[
                ("2026-11-16T09:00:00Z", weekly(40.0, friday)),
                ("2026-11-20T10:05:00Z", weekly(1.0, next)),
            ],
        );
        fs::write(dir.path().join("vega/host.json"), r#"{"machineId":"v"}"#).unwrap();
        let read = history(dir.path())
            .finished_windows(&wide_range(), &Hosts::All)
            .unwrap();
        let shape: Vec<_> = read
            .records
            .iter()
            .map(|w| (w.host.clone(), w.record.reset, w.record.final_percent))
            .collect();
        assert_eq!(
            shape,
            vec![
                ("atlas".to_string(), true, 62.0),
                ("atlas".to_string(), false, 30.0)
            ]
        );
    }
}
