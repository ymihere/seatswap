//! The auto-burn runtime of the tray app (spec §2.5, §2.12): when to learn,
//! when to decide, what to send, and the `BurnView` the pop-up shows. All the
//! words come from [`crate::burn_text`]; the reading and the inputs come from
//! `seatswap_core::burn::live`.

use std::collections::BTreeMap;
use std::sync::mpsc::{self, Receiver, Sender, TryRecvError};

use chrono::{DateTime, Duration, FixedOffset, Utc};
use serde::Serialize;

use seatswap_core::burn::burn_next::{self, BurnNext};
use seatswap_core::burn::decide::{Decision, Regime, RegimeWhy, Situation};
use seatswap_core::burn::live::{self, Learned, Readiness};
use seatswap_core::burn::plan::{follow, rates_known, Inputs, Plan, Policy};
use seatswap_core::burn::{
    FRESH_MIN, LEARN_RETRY_MIN, STALE_ASK_SECS, SWITCH_RELEARN_MIN, SWITCH_RETRY_MIN,
};
use seatswap_core::engine::{AccountView, PollReason};
use seatswap_core::history::{AccountRef, By, EventKind, HistoryDir};
use seatswap_core::money::{PriceChange, Prices};
use seatswap_core::paths::AppPaths;
use seatswap_core::store::{Account, Settings};
use seatswap_core::switcher::UnsavedPolicy;

use super::worker::{ActionCommand, ActionReply, EngineCommand};
use crate::burn_text::{self, BurnPart, SwitchSuggestion};

/// A switch that is waiting for a quiet moment; the panel shows its latest
/// time (spec §2.12).
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
#[serde(rename_all = "camelCase")]
pub(crate) struct WaitingSwitch {
    pub to: String,
    pub latest: DateTime<Utc>,
    pub tied_busy: u32,
}

/// What the pop-up shows of auto-burn (spec §2.5).
#[derive(Debug, Clone, Default, Serialize)]
#[serde(rename_all = "camelCase")]
pub(crate) struct BurnView {
    pub on: bool,
    pub paused_until: Option<DateTime<Utc>>,
    pub early: bool,
    pub why: String,
    pub line: Option<Vec<BurnPart>>,
    /// The first future use of each account auto-burn may switch to.
    pub planned: BTreeMap<String, DateTime<Utc>>,
    pub waiting: Option<WaitingSwitch>,
    pub suggestion: Option<SwitchSuggestion>,
    /// The accounts auto-burn waits a first reading of, in account order; empty
    /// otherwise. The Forecast page names them while it has no plan.
    pub unread: Vec<String>,
    /// What auto-burn has measured so far (spec §2.13); `None` while there is
    /// no situation to measure.
    pub measured: Option<Measured>,
}

/// What the last regime was computed from, as the settings page shows it
/// (spec §2.13, §3.7).
#[derive(Debug, Clone, Default, PartialEq, Serialize)]
#[serde(rename_all = "camelCase")]
pub(crate) struct Measured {
    /// A full 5 h window of each account auto-burn may use as a percent of its
    /// week: `100 × c / s`.
    pub window_percent_of_week: BTreeMap<String, f64>,
    /// What one switch costs the active account as a percent of its 5 h
    /// window: `cost.usd × s`.
    pub switch_cost_percent_of_window: Option<f64>,
    /// The eager plan's switches over its horizon, per day; `None` while early
    /// switching is off.
    pub switches_per_day: Option<f64>,
}

/// One snapshot's worth of inputs for [`BurnRuntime::after_snapshot`].
pub(crate) struct BurnTick<'a> {
    pub app: &'a AppPaths,
    pub history: Option<&'a HistoryDir>,
    pub views: &'a [AccountView],
    pub accounts: &'a [Account],
    pub settings: &'a Settings,
    pub active: Option<&'a str>,
    pub now: DateTime<Utc>,
    pub offset: FixedOffset,
    pub tied_busy: u32,
    /// The running tied sessions' answers with thinking, minute by minute.
    pub thinking: &'a [(DateTime<Utc>, u32)],
}

/// The plan inputs, its regime and the oldest reading they were built from;
/// the tray keeps the latest for the Forecast page.
pub(crate) type KeptForecast = (Inputs, Regime, DateTime<Utc>);

/// What one tick produced. The worker adds `error` to the snapshot's error,
/// prints `warnings` through the one app printer, and sends `notification`
/// after the switch landed.
#[derive(Debug, Default)]
pub(crate) struct BurnReport {
    pub view: Option<BurnView>,
    pub error: Option<String>,
    pub warnings: Vec<String>,
    pub notification: Option<String>,
    /// The plan inputs, regime and oldest reading this tick computed; `None`
    /// while there is no situation. The Forecast page reads the latest one and
    /// refuses it by the reading's age.
    pub forecast: Option<KeptForecast>,
}

/// A switch that was sent and whose reply has not been read yet.
struct SentSwitch {
    to: String,
    /// The active account when the switch was sent; a refusal leaves it.
    from: Option<String>,
    sent_at: DateTime<Utc>,
    /// The notification to show once the switch landed.
    notification: Option<String>,
    reply: Option<Receiver<ActionReply>>,
    /// The reply's error while the same decision still stands.
    error: Option<String>,
}

pub(crate) struct BurnRuntime {
    learned: Option<Learned>,
    /// The last learn attempt and whether it failed; a failure is retried after
    /// [`LEARN_RETRY_MIN`] and its error stays in every snapshot until a learn
    /// succeeds.
    learn_attempted_at: Option<DateTime<Utc>>,
    learn_error: Option<String>,
    regime: Option<Regime>,
    regime_at: Option<DateTime<Utc>>,
    /// The accounts auto-burn may use and whether each has rates, when the
    /// regime was computed: a change means the regime is computed again.
    regime_basis: Option<BTreeMap<String, bool>>,
    last_active: Option<String>,
    /// The last learn attempt, for the re-learn after a switch event that
    /// landed after the files it names.
    switch_learn_at: Option<DateTime<Utc>>,
    /// The last switch decision sent, its reply, and when it was sent.
    last_sent: Option<SentSwitch>,
    /// The time of the last stale poll ask (at most one per [`STALE_ASK_SECS`]).
    last_stale_ask: Option<DateTime<Utc>>,
    view: Option<BurnView>,
    /// The `burn-next.json` content this runtime wrote last, or `None` when
    /// it removed the file (or never wrote it in this run).
    next_written: Option<BurnNext>,
    /// The stored price list's `checkedAt` and `lastChange` at the last learn;
    /// a different list means the work is priced differently and learns again.
    prices_learned: Option<(Option<DateTime<Utc>>, Option<PriceChange>)>,
    force_learn: bool,
    action: Sender<ActionCommand>,
    engine: Sender<EngineCommand>,
}

impl BurnRuntime {
    /// The runtime needs both senders from the start: the first snapshot the
    /// engine builds already decides and may switch (spec §2.5).
    pub(crate) fn new(action: Sender<ActionCommand>, engine: Sender<EngineCommand>) -> BurnRuntime {
        BurnRuntime {
            learned: None,
            learn_attempted_at: None,
            learn_error: None,
            regime: None,
            regime_at: None,
            regime_basis: None,
            last_active: None,
            switch_learn_at: None,
            last_sent: None,
            last_stale_ask: None,
            view: None,
            next_written: None,
            prices_learned: None,
            force_learn: false,
            action,
            engine,
        }
    }

    /// Re-learn at the next snapshot, after a switch or a pause was written.
    pub(crate) fn force_learn(&mut self) {
        self.force_learn = true;
    }

    pub(crate) fn after_snapshot(&mut self, tick: &BurnTick<'_>) -> BurnReport {
        let report = self.tick(tick);
        self.last_active = tick.active.map(str::to_string);
        report
    }

    fn tick(&mut self, tick: &BurnTick<'_>) -> BurnReport {
        let mut report = BurnReport::default();
        if !tick.settings.auto_burn {
            if let Some(error) = self.clear_next(tick.app) {
                append_error(&mut report, error);
            }
        }
        self.read_switch_reply(tick.views, &mut report);

        let failed_due = self.learn_error.is_some()
            && self
                .learn_attempted_at
                .is_some_and(|at| tick.now - at >= Duration::minutes(LEARN_RETRY_MIN));
        // The tick reads no other file, so it reads the stored price list here
        // to notice a look-up between the hourly learns.
        let prices = Prices::load(tick.app);
        let prices_changed =
            self.prices_learned
                .as_ref()
                .is_some_and(|(checked_at, last_change)| match &prices {
                    Ok(prices) => {
                        prices.checked_at != *checked_at || prices.last_change != *last_change
                    }
                    Err(_) => false,
                });
        let due = self.force_learn
            || self.learn_attempted_at.is_none()
            || (self.learn_error.is_none()
                && self
                    .learn_attempted_at
                    .is_some_and(|at| tick.now - at >= Duration::hours(1)))
            || failed_due
            || tick.active != self.last_active.as_deref()
            || prices_changed
            || self.switch_relearn_due(tick);
        if due {
            self.force_learn = false;
            self.learn_attempted_at = Some(tick.now);
            self.switch_learn_at = Some(tick.now);
            // The work is priced with the stored list read above.
            match (tick.history, prices) {
                (Some(history), Ok(prices)) => match live::learn(
                    history,
                    tick.now,
                    tick.offset,
                    tick.accounts,
                    tick.settings.read_sessions,
                    &prices,
                ) {
                    Ok(learned) => {
                        report.warnings.extend(learned.warnings.iter().cloned());
                        self.prices_learned = Some((prices.checked_at, prices.last_change.clone()));
                        self.learned = Some(learned);
                        self.learn_error = None;
                    }
                    Err(error) => {
                        self.learn_error =
                            Some(format!("auto-burn: cannot read the history: {error}"));
                    }
                },
                (Some(_), Err(error)) => {
                    self.learn_error =
                        Some(format!("auto-burn: the prices cannot be read: {error}"));
                }
                (None, _) => {
                    self.learn_error = Some("auto-burn: the history is not open".to_string());
                }
            }
        }
        if let Some(error) = self.learn_error.clone() {
            append_error(&mut report, error);
        }

        let Some(learned) = &self.learned else {
            return report;
        };
        let readiness = match live::situation(
            learned,
            tick.views,
            tick.accounts,
            tick.active,
            tick.now,
            tick.offset,
            tick.tied_busy,
            tick.thinking,
        ) {
            Ok(readiness) => readiness,
            Err(error) => {
                report.error = Some(format!("auto-burn: {error}"));
                return report;
            }
        };
        let situation = match readiness {
            Readiness::Ready(situation) => situation,
            Readiness::Unread { accounts } => {
                let mut view = waiting_view(tick.settings);
                match burn_text::unread_line(tick.views, &accounts) {
                    Ok(line) => view.line = Some(line),
                    Err(error) => append_error(&mut report, text_error(error)),
                }
                view.unread = accounts;
                self.view = Some(view.clone());
                report.view = Some(view);
                // The same behaviour as `Decision::Stale`: poll, never switch.
                self.ask_stale(tick.now);
                if let Some(error) = self.clear_next(tick.app) {
                    append_error(&mut report, error);
                }
                return report;
            }
            Readiness::NoAccounts => {
                let mut view = waiting_view(tick.settings);
                view.line = Some(burn_text::no_accounts_line(tick.accounts));
                self.view = Some(view.clone());
                report.view = Some(view);
                if let Some(error) = self.clear_next(tick.app) {
                    append_error(&mut report, error);
                }
                return report;
            }
        };
        let fresh = tick.now.signed_duration_since(situation.oldest_reading)
            <= Duration::minutes(FRESH_MIN);
        let basis = regime_basis(&situation);
        let regime_due = fresh
            && (self.regime.is_none()
                || self
                    .regime_at
                    .is_none_or(|at| tick.now - at >= Duration::hours(1))
                || self.regime_basis.as_ref() != Some(&basis));
        let cached_regime = if regime_due {
            None
        } else {
            self.regime.as_ref()
        };
        if !fresh && cached_regime.is_none() {
            self.ask_stale(tick.now);
            return report;
        }
        let assessment = live::assess(situation, cached_regime);
        if regime_due {
            self.regime = Some(assessment.regime.clone());
            self.regime_at = Some(tick.now);
            self.regime_basis = Some(basis);
        }
        let live::Assessment {
            situation,
            regime,
            decision,
            plan,
            next,
        } = assessment;
        report.forecast = Some((
            situation.inputs.clone(),
            regime.clone(),
            situation.oldest_reading,
        ));
        if let Some(error) = self.publish_next(tick, &situation, &decision, &plan, next.as_deref())
        {
            append_error(&mut report, error);
        }
        let outcome = self.apply_decision(
            decision,
            &situation,
            &plan,
            &regime,
            tick.views,
            tick.settings,
            tick.now,
            tick.offset,
        );
        merge(report, outcome)
    }

    /// Write or remove `<data>/burn-next.json` for the status line (spec §7.6).
    /// `None` is success; `Some` is the error the snapshot shows. The file is
    /// rewritten when the account or time changes, and at least every
    /// [`burn_next::REWRITE_AFTER_MIN`] minutes as the tray app's heartbeat.
    fn publish_next(
        &mut self,
        tick: &BurnTick<'_>,
        situation: &Situation,
        decision: &Decision,
        plan: &Plan,
        next: Option<&str>,
    ) -> Option<String> {
        let paused = situation
            .inputs
            .paused_until
            .is_some_and(|until| until > tick.now);
        let desired = if tick.settings.auto_burn && !paused {
            planned_next(decision, plan, next, tick.now)
        } else {
            None
        };
        let Some((account, at)) = desired else {
            return self.clear_next(tick.app);
        };
        let due = match &self.next_written {
            Some(previous) => {
                previous.account != account
                    || previous.at != at
                    || tick.now - previous.written
                        >= Duration::minutes(burn_next::REWRITE_AFTER_MIN)
            }
            None => true,
        };
        if !due {
            return None;
        }
        let document = BurnNext {
            account,
            at,
            written: tick.now,
        };
        match burn_next::write(tick.app, &document) {
            Ok(()) => {
                self.next_written = Some(document);
                None
            }
            Err(error) => Some(format!(
                "auto-burn: cannot write {}: {error}",
                tick.app.burn_next_file().display()
            )),
        }
    }

    /// Remove the status line's next account and forget it. `None` is success;
    /// `Some` is the error the snapshot shows.
    fn clear_next(&mut self, app: &AppPaths) -> Option<String> {
        self.next_written = None;
        match burn_next::remove(app) {
            Ok(()) => None,
            Err(error) => Some(format!(
                "auto-burn: cannot remove {}: {error}",
                app.burn_next_file().display()
            )),
        }
    }

    /// Read the switch's reply, if one arrived: the notification after it
    /// landed, or the error of a refused or failed switch.
    fn read_switch_reply(&mut self, views: &[AccountView], report: &mut BurnReport) {
        let Some(sent) = self.last_sent.as_mut() else {
            return;
        };
        let received = match sent.reply.as_ref() {
            Some(reply) => reply.try_recv(),
            None => return,
        };
        match received {
            Ok(ActionReply::Switched(_)) => {
                sent.reply = None;
                sent.error = None;
                report.notification = sent.notification.take();
            }
            Ok(ActionReply::NotSaved(_)) => {
                sent.reply = None;
                sent.error = Some(match sent.from.as_deref() {
                    Some(from) => match burn_text::abbr(views, from) {
                        Ok(from) => format!(
                            "auto-burn could not leave {from}: the current login is not saved"
                        ),
                        Err(error) => format!("auto-burn: {error}"),
                    },
                    None => "auto-burn: no active account to name".to_string(),
                });
            }
            Ok(ActionReply::Failed(message)) => {
                sent.reply = None;
                sent.error = Some(match burn_text::abbr(views, &sent.to) {
                    Ok(to) => format!("auto-burn: the switch to {to} failed: {message}"),
                    Err(error) => format!("auto-burn: {error}"),
                });
            }
            Ok(ActionReply::Done) => sent.reply = None,
            Ok(ActionReply::Saved(_)) => {
                sent.reply = None;
                sent.error =
                    Some("auto-burn: the action thread gave an unexpected reply".to_string());
            }
            Err(TryRecvError::Empty) => {}
            Err(TryRecvError::Disconnected) => {
                sent.reply = None;
                sent.error = Some("auto-burn: the switch action is not connected".to_string());
            }
        }
    }

    /// Whether the learned history still names another account than the one in
    /// use: the files can change before the switch event is written (the CLI
    /// writes Claude Code's files first), so learn again, at most once per
    /// [`SWITCH_RELEARN_MIN`] minutes.
    fn switch_relearn_due(&self, tick: &BurnTick<'_>) -> bool {
        let Some(learned) = &self.learned else {
            return false;
        };
        let Some(target) = newest_switch_target(learned, tick.now) else {
            return false;
        };
        let target_id = tick
            .accounts
            .iter()
            .find(|account| AccountRef::of(account) == *target)
            .map(|account| account.id.as_str());
        target_id != tick.active
            && self
                .switch_learn_at
                .is_none_or(|at| tick.now - at >= Duration::minutes(SWITCH_RELEARN_MIN))
    }

    /// Ask for a poll of the old readings, at most once per [`STALE_ASK_SECS`].
    fn ask_stale(&mut self, now: DateTime<Utc>) {
        if self
            .last_stale_ask
            .is_some_and(|at| now - at < Duration::seconds(STALE_ASK_SECS))
        {
            return;
        }
        if self
            .engine
            .send(EngineCommand::Poll(PollReason::Stale))
            .is_ok()
        {
            self.last_stale_ask = Some(now);
        }
    }

    #[cfg(test)]
    fn decision_report(
        &mut self,
        situation: &Situation,
        regime: &Regime,
        views: &[AccountView],
        settings: &Settings,
        now: DateTime<Utc>,
        offset: FixedOffset,
    ) -> BurnReport {
        let live::Assessment {
            situation,
            regime,
            decision,
            plan,
            ..
        } = live::assess(situation.clone(), Some(regime));
        self.apply_decision(
            decision, &situation, &plan, &regime, views, settings, now, offset,
        )
    }

    #[allow(clippy::too_many_arguments)]
    fn apply_decision(
        &mut self,
        decision: Decision,
        situation: &Situation,
        plan: &Plan,
        regime: &Regime,
        views: &[AccountView],
        settings: &Settings,
        now: DateTime<Utc>,
        offset: FixedOffset,
    ) -> BurnReport {
        let active = situation.inputs.active.as_deref();
        let mut report = BurnReport::default();
        let line = match burn_text::line(plan, active, views, now) {
            Ok(line) => line,
            Err(error) => {
                report.error = Some(text_error(error));
                None
            }
        };
        let mut view = BurnView {
            on: settings.auto_burn,
            paused_until: situation.inputs.paused_until.filter(|until| *until > now),
            early: regime.early,
            why: burn_text::why(regime.why).to_string(),
            line,
            planned: burn_text::planned(plan, active),
            waiting: None,
            suggestion: None,
            unread: Vec::new(),
            measured: Some(measured(situation, regime)),
        };

        match &decision {
            Decision::Switch { to, reason } => {
                if settings.auto_burn {
                    let retry_due = match &self.last_sent {
                        Some(sent) if sent.to == *to => {
                            sent.reply.is_none()
                                && now - sent.sent_at >= Duration::minutes(SWITCH_RETRY_MIN)
                        }
                        _ => true,
                    };
                    if retry_due {
                        match (
                            burn_text::reason(reason, to, active, views, now, offset),
                            burn_text::switch_notification(&decision, active, views, now, offset),
                        ) {
                            (Ok(reason_text), Ok(notification)) => {
                                let (reply, reply_rx) = mpsc::channel();
                                let sent = self
                                    .action
                                    .send(ActionCommand::Switch {
                                        key: to.clone(),
                                        unsaved: UnsavedPolicy::Refuse,
                                        label: None,
                                        by: By::Auto,
                                        reason: Some(reason_text),
                                        reply,
                                    })
                                    .is_ok();
                                if sent {
                                    self.last_sent = Some(SentSwitch {
                                        to: to.clone(),
                                        from: active.map(str::to_string),
                                        sent_at: now,
                                        notification,
                                        reply: Some(reply_rx),
                                        error: None,
                                    });
                                    self.force_learn = true;
                                } else {
                                    report.error = Some(
                                        "auto-burn: the switch action is not connected".to_string(),
                                    );
                                }
                            }
                            (Err(error), _) | (_, Err(error)) => {
                                report.error = Some(text_error(error));
                            }
                        }
                    } else if let Some(error) =
                        self.last_sent.as_ref().and_then(|sent| sent.error.as_ref())
                    {
                        report.error = Some(error.clone());
                    }
                } else if settings.suggest_switches {
                    match burn_text::suggestion(
                        &decision,
                        &situation.inputs,
                        active,
                        views,
                        now,
                        offset,
                    ) {
                        Ok(suggestion) => view.suggestion = suggestion,
                        Err(error) => report.error = Some(text_error(error)),
                    }
                }
            }
            Decision::Wait {
                to,
                latest,
                tied_busy,
                ..
            } => {
                if settings.auto_burn {
                    view.waiting = Some(WaitingSwitch {
                        to: to.clone(),
                        latest: *latest,
                        tied_busy: *tied_busy,
                    });
                } else if settings.suggest_switches {
                    match burn_text::suggestion(
                        &decision,
                        &situation.inputs,
                        active,
                        views,
                        now,
                        offset,
                    ) {
                        Ok(suggestion) => view.suggestion = suggestion,
                        Err(error) => report.error = Some(text_error(error)),
                    }
                }
            }
            Decision::Stale => self.ask_stale(now),
            Decision::Stay | Decision::AllFull { .. } => {}
        }

        self.view = Some(view.clone());
        report.view = Some(view);
        report
    }
}

/// What auto-burn has measured: a full 5 h window as a percent of each account's
/// week (`100 × c / s`, for every account auto-burn may use whose `s` is known),
/// what one switch costs the active account as a percent of its window
/// (`cost.usd × s`), and the eager plan's switches per day while early switching
/// is on. Missing values stay `None`; the page says what they wait for.
fn measured(situation: &Situation, regime: &Regime) -> Measured {
    let window_percent_of_week = situation
        .inputs
        .accounts
        .iter()
        .filter(|account| account.may_use)
        .filter_map(|account| {
            let rates = account.rates?;
            let s = rates.s?;
            Some((account.id.clone(), 100.0 * rates.c.value / s.value))
        })
        .collect();
    let switch_cost_percent_of_window = situation.cost.and_then(|cost| {
        let active = situation.inputs.active.as_deref()?;
        let account = situation
            .inputs
            .accounts
            .iter()
            .find(|account| account.id == active)?;
        Some(cost.usd * account.rates?.s?.value)
    });
    let switches_per_day = regime.early.then(|| {
        seatswap_core::burn::switches_per_day(follow(&situation.inputs, Policy::Eager).switches)
    });
    Measured {
        window_percent_of_week,
        switch_cost_percent_of_window,
        switches_per_day,
    }
}

/// The accounts auto-burn may use and whether each has rates: the value the
/// regime was computed from, so a change computes it again.
fn regime_basis(situation: &Situation) -> BTreeMap<String, bool> {
    situation
        .inputs
        .accounts
        .iter()
        .filter(|account| account.may_use)
        .map(|account| (account.id.clone(), rates_known(account)))
        .collect()
}

/// The target of the newest switch event of this host in the learned history.
fn newest_switch_target(learned: &Learned, now: DateTime<Utc>) -> Option<&AccountRef> {
    learned
        .events
        .iter()
        .filter(|hosted| hosted.host == learned.host && hosted.record.t <= now)
        .filter_map(|hosted| match &hosted.record.kind {
            EventKind::Switch { to, .. } => Some((hosted.record.t, to)),
            _ => None,
        })
        .max_by_key(|(at, _)| *at)
        .map(|(_, to)| to)
}

/// A view with no decision behind it: the strip keeps its switch and Pause and
/// shows `line` (spec §7.4.6's unread and no-account cases).
fn waiting_view(settings: &Settings) -> BurnView {
    BurnView {
        on: settings.auto_burn,
        paused_until: None,
        early: false,
        why: burn_text::why(RegimeWhy::Learning).to_string(),
        line: None,
        planned: BTreeMap::new(),
        waiting: None,
        suggestion: None,
        unread: Vec::new(),
        measured: None,
    }
}

/// The next account and when its switch is planned: now for a switch, the
/// waiting switch's latest time, or the first future use of the plan's next
/// account.
fn planned_next(
    decision: &Decision,
    plan: &Plan,
    next: Option<&str>,
    now: DateTime<Utc>,
) -> Option<(String, DateTime<Utc>)> {
    match decision {
        Decision::Switch { to, .. } => Some((to.clone(), now)),
        Decision::Wait { to, latest, .. } => Some((to.clone(), *latest)),
        Decision::Stay | Decision::Stale | Decision::AllFull { .. } => {
            let id = next?;
            let at = plan
                .segments
                .iter()
                .find(|segment| segment.account.as_deref() == Some(id))?
                .from;
            Some((id.to_string(), at))
        }
    }
}

/// Add `error` to the report's error, joined with an earlier one.
fn append_error(report: &mut BurnReport, error: String) {
    report.error = Some(match report.error.take() {
        Some(previous) => format!("{previous}; {error}"),
        None => error,
    });
}

fn text_error(error: burn_text::TextError) -> String {
    format!("auto-burn: {error}")
}

fn merge(mut report: BurnReport, next: BurnReport) -> BurnReport {
    if next.view.is_some() {
        report.view = next.view;
    }
    if let Some(error) = next.error {
        report.error = Some(match report.error {
            Some(previous) => format!("{previous}; {error}"),
            None => error,
        });
    }
    report.warnings.extend(next.warnings);
    if next.notification.is_some() {
        report.notification = next.notification;
    }
    report
}

#[cfg(test)]
mod tests {
    use super::*;

    use chrono::FixedOffset;

    use seatswap_core::api::{Limit, LimitKind, Severity, Usage};
    use seatswap_core::burn::decide::{Reason, RegimeWhy, Situation, SwitchCost};
    use seatswap_core::burn::plan::{AccountState, Inputs, Plan, Segment, SessionState};
    use seatswap_core::burn::{Rates, Rhythm};
    use seatswap_core::history::{AccountRef, Event, EventKind, Hosted, Recorder, TokenRecord};
    use seatswap_core::money::ModelPrice;
    use seatswap_core::project::ProjectKey;
    use seatswap_core::store::LoginState;
    use seatswap_core::types::OAuthAccount;

    use super::super::worker::PendingSwitch;

    const ORG: &str = "e92d7b31-5c8e-4f2a-9d6b-1e3c5a7f9b20";
    const PR_ACC: &str = "4b7e2f91-3c5a-4d8e-9f1b-6a2c8e4d7b30";
    const AC_ACC: &str = "8e1d5b7a-6c3f-4a2b-9d8e-1f2a3b4c5d6e";
    const RESET: &str = "2026-11-16T02:00:00Z";

    fn utc(text: &str) -> DateTime<Utc> {
        DateTime::parse_from_rfc3339(text)
            .expect("a test time")
            .with_timezone(&Utc)
    }

    fn offset() -> FixedOffset {
        FixedOffset::east_opt(0).expect("UTC")
    }

    fn account(id: &str, abbr: &str, acc: &str, auto_burn: bool) -> Account {
        Account {
            id: id.to_string(),
            account_uuid: acc.to_string(),
            organization_uuid: ORG.to_string(),
            label: abbr.to_string(),
            abbr: abbr.to_string(),
            color: "#3b82f6".to_string(),
            email: None,
            org_name: None,
            plan: Some("Team".to_string()),
            oauth_account: OAuthAccount(serde_json::Map::new()),
            added_at: utc("2026-11-01T00:00:00Z"),
            last_used_at: None,
            login_state: LoginState::Ok,
            login_ends_at_ms: None,
            auto_burn,
            price: None,
            price_currency: None,
        }
    }

    fn limit(kind: LimitKind, percent: f64) -> Limit {
        Limit {
            kind,
            percent,
            severity: Severity::Normal,
            resets_at: Some(utc(RESET)),
            model: None,
            is_active: false,
        }
    }

    fn usage(week: f64, session: bool) -> Usage {
        let mut limits = vec![limit(LimitKind::WeeklyAll, week)];
        if session {
            limits.push(limit(LimitKind::Session, 10.0));
        }
        Usage {
            limits,
            extra_usage_enabled: false,
            warnings: Vec::new(),
        }
    }

    fn view(
        id: &str,
        abbr: &str,
        week: f64,
        session: bool,
        fetched_at: DateTime<Utc>,
    ) -> AccountView {
        AccountView {
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
            usage: Some(usage(week, session)),
            usable: true,
            blocked_until: None,
            blocker: None,
            fetched_at: Some(fetched_at),
            last_error: None,
        }
    }

    fn state(id: &str, may_use: bool, week: f64, session: bool) -> AccountState {
        AccountState {
            id: id.to_string(),
            may_use,
            login_ok: true,
            week_percent: week,
            week_resets_at: utc(RESET),
            session: if session {
                Some(SessionState {
                    percent: 10.0,
                    resets_at: Some(utc(RESET)),
                })
            } else {
                None
            },
            model_blocked: false,
            rates: if session {
                Some(Rates::given(4.0, Some(100.0)))
            } else {
                Some(Rates::given(2.0, None))
            },
            thinking_held: 0,
        }
    }

    fn situation(now: DateTime<Utc>, active: Option<&str>) -> Situation {
        Situation {
            inputs: Inputs {
                now,
                offset: offset(),
                accounts: vec![
                    state("pr", true, 20.0, false),
                    state("ac", true, 80.0, true),
                ],
                active: active.map(str::to_string),
                rhythm: Rhythm { usd: [1.0; 168] },
                last_switch_at: None,
                paused_until: None,
            },
            cost: Some(SwitchCost {
                usd: 0.05,
                samples: 3,
            }),
            tied_busy: 0,
            oldest_reading: now - Duration::minutes(1),
            sessions_off: false,
        }
    }

    fn regime(early: bool) -> Regime {
        Regime {
            early,
            why: if early {
                RegimeWhy::Pays
            } else {
                RegimeWhy::NoCostYet
            },
            lazy_blocked_h: 1.0,
            eager_blocked_h: 0.0,
            extra_switches: 1,
            cost_h: Some(0.1),
        }
    }

    fn plan(now: DateTime<Utc>) -> Plan {
        Plan {
            segments: vec![
                Segment {
                    account: Some("pr".to_string()),
                    from: now,
                    to: now + Duration::hours(2),
                },
                Segment {
                    account: Some("ac".to_string()),
                    from: now + Duration::hours(2),
                    to: now + Duration::hours(4),
                },
            ],
            blocked_usd: 0.0,
            switches: 1,
            left_at_reset: BTreeMap::new(),
            // The lazy plan's shape: PR fills when its segment ends.
            full_at: BTreeMap::from([("pr".to_string(), now + Duration::hours(2))]),
            full_by: BTreeMap::from([
                ("pr".to_string(), seatswap_core::burn::plan::Fill::Week),
                ("ac".to_string(), seatswap_core::burn::plan::Fill::Window),
            ]),
        }
    }

    fn window_open() -> Decision {
        Decision::Switch {
            to: "ac".to_string(),
            reason: Reason::WindowOpen,
        }
    }

    fn runtime() -> (
        BurnRuntime,
        mpsc::Receiver<ActionCommand>,
        mpsc::Receiver<EngineCommand>,
    ) {
        let (action_tx, action_rx) = mpsc::channel();
        let (engine_tx, engine_rx) = mpsc::channel();
        (BurnRuntime::new(action_tx, engine_tx), action_rx, engine_rx)
    }

    /// The engine builds its first snapshot before it starts `run`, so the
    /// runtime must own both senders from `new`: that first tick already
    /// decides and may switch (the tray bench showed "auto-burn: the switch
    /// action is not connected" before).
    #[test]
    fn the_first_snapshot_can_switch() {
        let now = utc("2026-11-10T12:00:00Z");
        let tmp = tempfile::tempdir().expect("a temp folder");
        let history = HistoryDir::at(tmp.path(), "atlas");
        let app = AppPaths::at(tmp.path());
        let (action_tx, action_rx) = mpsc::channel();
        let (engine_tx, _engine_rx) = mpsc::channel();
        let mut runtime = BurnRuntime::new(action_tx, engine_tx);
        let accounts = vec![
            account("pr", "PR", PR_ACC, true),
            account("ac", "AC", AC_ACC, true),
        ];
        let views = vec![
            view("pr", "PR", 100.0, false, now - Duration::minutes(1)),
            view("ac", "AC", 10.0, true, now - Duration::minutes(1)),
        ];
        let settings = Settings {
            auto_burn: true,
            ..Settings::default()
        };
        let tick = BurnTick {
            app: &app,
            history: Some(&history),
            views: &views,
            accounts: &accounts,
            settings: &settings,
            active: Some("pr"),
            now,
            offset: offset(),
            tied_busy: 0,
            thinking: &[],
        };
        let report = runtime.after_snapshot(&tick);
        assert!(report.view.is_some(), "the first snapshot has a view");
        match action_rx.try_recv().expect("the first tick sends a switch") {
            ActionCommand::Switch { key, by, .. } => {
                assert_eq!(key, "ac");
                assert_eq!(by, By::Auto);
            }
            _ => panic!("not a switch"),
        }
        assert!(action_rx.try_recv().is_err(), "only one command");
    }

    /// A look-up between the hourly learns must reach the rates at once: the
    /// stored list's `checkedAt`/`lastChange` changed, so the next tick learns
    /// again with it.
    #[test]
    fn a_new_price_list_learns_again() {
        let now = utc("2026-11-10T12:00:00Z");
        let tmp = tempfile::tempdir().expect("a temp folder");
        let history = HistoryDir::at(tmp.path(), "atlas");
        let app = AppPaths::at(tmp.path());
        let recorder = Recorder::new(HistoryDir::at(history.root(), history.host()));
        recorder
            .tokens(&[TokenRecord {
                t: now - Duration::minutes(30),
                minutes: 60,
                session: "7c1e4b2a-9d3f-4e6a-b8c5-2f0a1d7e9b34".to_string(),
                cwd: "/home/sam/projects/atlas".to_string(),
                project: ProjectKey::Repo("github.com/sam/atlas".to_string()),
                model: "claude-opus-5-5".to_string(),
                input: 2_000_000,
                out: 0,
                cw: 0,
                cr: 0,
                answers: 1,
                agent_answers: 0,
            }])
            .expect("the token record is written");

        let (action_tx, _action_rx) = mpsc::channel();
        let (engine_tx, _engine_rx) = mpsc::channel();
        let mut runtime = BurnRuntime::new(action_tx, engine_tx);
        let accounts = vec![account("pr", "PR", PR_ACC, true)];
        let views = vec![view("pr", "PR", 10.0, false, now - Duration::minutes(1))];
        let settings = Settings {
            auto_burn: false,
            ..Settings::default()
        };
        let tick = |at: DateTime<Utc>| BurnTick {
            app: &app,
            history: Some(&history),
            views: &views,
            accounts: &accounts,
            settings: &settings,
            active: Some("pr"),
            now: at,
            offset: offset(),
            tied_busy: 0,
            thinking: &[],
        };

        let first = runtime.after_snapshot(&tick(now));
        assert!(
            first
                .warnings
                .iter()
                .any(|warning| warning.contains("no API prices yet")),
            "{:?}",
            first.warnings
        );
        assert!(
            runtime
                .learned
                .as_ref()
                .expect("the first learn")
                .rhythm
                .usd
                .iter()
                .all(|usd| *usd == 0.0),
            "no priced work yet"
        );

        let mut prices = Prices::default();
        prices.take(
            BTreeMap::from([(
                "anthropic/claude-opus-5-5".to_string(),
                ModelPrice {
                    input: 4.0,
                    output: 20.0,
                    cache_write: 5.0,
                    cache_read: 0.2,
                    tiered: false,
                },
            )]),
            now,
        );
        prices.checked_at = Some(now);
        prices.save(&app).expect("prices.json");

        let second = runtime.after_snapshot(&tick(now + Duration::minutes(1)));
        assert!(
            !second
                .warnings
                .iter()
                .any(|warning| warning.contains("no API prices yet")),
            "{:?}",
            second.warnings
        );
        assert!(
            runtime
                .learned
                .as_ref()
                .expect("the second learn")
                .rhythm
                .usd
                .iter()
                .any(|usd| *usd > 0.0),
            "the new list prices the work, so the runtime learned again"
        );
    }

    #[test]
    fn measured_comes_from_the_rates_and_the_cost() {
        let now = utc("2026-11-10T12:00:00Z");
        let (mut runtime, _action, _engine) = runtime();
        let views = vec![
            view("pr", "PR", 20.0, false, now - Duration::minutes(1)),
            view("ac", "AC", 80.0, true, now - Duration::minutes(1)),
        ];
        let settings = Settings::default();
        let situation = situation(now, Some("ac"));
        let report = runtime.apply_decision(
            Decision::Stay,
            &situation,
            &plan(now),
            &regime(true),
            &views,
            &settings,
            now,
            offset(),
        );
        let measured = report
            .view
            .expect("a view")
            .measured
            .expect("a situation was measured");
        assert_eq!(
            measured.window_percent_of_week,
            BTreeMap::from([("ac".to_string(), 4.0)]),
            "PR has no learned s, so only AC's full window is shown"
        );
        let cost = measured
            .switch_cost_percent_of_window
            .expect("AC has an s and the situation a cost");
        assert!((cost - 5.0).abs() < 1e-9, "cost 0.05 USD × s 100 = {cost}");
        let expected = follow(&situation.inputs, Policy::Eager).switches as f64 / 7.0;
        assert_eq!(measured.switches_per_day, Some(expected));

        let no_early = runtime
            .apply_decision(
                Decision::Stay,
                &situation,
                &plan(now),
                &regime(false),
                &views,
                &settings,
                now,
                offset(),
            )
            .view
            .expect("a view")
            .measured
            .expect("a situation was measured");
        assert_eq!(no_early.switches_per_day, None, "early switching is off");

        let mut no_cost = situation;
        no_cost.cost = None;
        let measured = runtime
            .apply_decision(
                Decision::Stay,
                &no_cost,
                &plan(now),
                &regime(true),
                &views,
                &settings,
                now,
                offset(),
            )
            .view
            .expect("a view")
            .measured
            .expect("a situation was measured");
        assert_eq!(measured.switch_cost_percent_of_window, None);
    }

    #[test]
    fn auto_burn_off_never_sends_a_switch() {
        let now = utc("2026-11-10T12:00:00Z");
        let (mut runtime, action, _engine) = runtime();
        let views = vec![
            view("pr", "PR", 20.0, false, now - Duration::minutes(1)),
            view("ac", "AC", 80.0, true, now - Duration::minutes(1)),
        ];
        let settings = Settings::default();
        let report = runtime.apply_decision(
            window_open(),
            &situation(now, Some("pr")),
            &plan(now),
            &regime(true),
            &views,
            &settings,
            now,
            offset(),
        );
        assert!(action.try_recv().is_err(), "auto-burn is off");
        let suggestion = report
            .view
            .expect("a view")
            .suggestion
            .expect("a suggestion");
        assert_eq!(
            suggestion.text,
            "Switch to AC now: its 5h window is open and its week resets on Mon 02:00. \
             Staying on PR is expected to let 16 % of AC expire.",
            "the suggestion's staying sentence is the lazy plan's, not the followed plan's"
        );
    }

    #[test]
    fn suggestions_off_shows_nothing() {
        let now = utc("2026-11-10T12:00:00Z");
        let (mut runtime, action, _engine) = runtime();
        let views = vec![
            view("pr", "PR", 20.0, false, now - Duration::minutes(1)),
            view("ac", "AC", 80.0, true, now - Duration::minutes(1)),
        ];
        let settings = Settings {
            suggest_switches: false,
            ..Settings::default()
        };
        let report = runtime.apply_decision(
            window_open(),
            &situation(now, Some("pr")),
            &plan(now),
            &regime(true),
            &views,
            &settings,
            now,
            offset(),
        );
        assert!(action.try_recv().is_err());
        assert!(report.view.expect("a view").suggestion.is_none());
    }

    #[test]
    fn auto_burn_on_sends_one_switch_by_auto() {
        let now = utc("2026-11-10T12:00:00Z");
        let (mut runtime, action, _engine) = runtime();
        let views = vec![
            view("pr", "PR", 20.0, false, now - Duration::minutes(1)),
            view("ac", "AC", 80.0, true, now - Duration::minutes(1)),
        ];
        let settings = Settings {
            auto_burn: true,
            ..Settings::default()
        };
        runtime.apply_decision(
            window_open(),
            &situation(now, Some("pr")),
            &plan(now),
            &regime(true),
            &views,
            &settings,
            now,
            offset(),
        );
        match action.try_recv().expect("one switch") {
            ActionCommand::Switch {
                key, by, reason, ..
            } => {
                assert_eq!(key, "ac");
                assert_eq!(by, By::Auto);
                assert_eq!(
                    reason.as_deref(),
                    Some("its 5h window is open and its week resets on Mon 02:00")
                );
            }
            _ => panic!("not a switch"),
        }
        assert!(action.try_recv().is_err(), "only one command");
    }

    #[test]
    fn one_decision_sends_one_switch() {
        let now = utc("2026-11-10T12:00:00Z");
        let (mut runtime, action, _engine) = runtime();
        let views = vec![
            view("pr", "PR", 20.0, false, now - Duration::minutes(1)),
            view("ac", "AC", 80.0, true, now - Duration::minutes(1)),
        ];
        let settings = Settings {
            auto_burn: true,
            ..Settings::default()
        };
        let situation = situation(now, Some("pr"));
        let plan = plan(now);
        let regime = regime(true);
        runtime.apply_decision(
            window_open(),
            &situation,
            &plan,
            &regime,
            &views,
            &settings,
            now,
            offset(),
        );
        runtime.apply_decision(
            window_open(),
            &situation,
            &plan,
            &regime,
            &views,
            &settings,
            now,
            offset(),
        );
        assert!(action.try_recv().is_ok(), "the first decision switches");
        assert!(
            action.try_recv().is_err(),
            "the same decision before the switch landed sends nothing"
        );
    }

    #[test]
    fn a_logged_out_account_cannot_hold_back_leaving_a_full_account() {
        let now = utc("2026-11-10T12:00:00Z");
        let tmp = tempfile::tempdir().expect("a temp folder");
        let history = HistoryDir::at(tmp.path(), "atlas");
        let app = AppPaths::at(tmp.path());
        let (mut runtime, action, _engine) = runtime();
        let accounts = vec![
            account("pr", "PR", PR_ACC, true),
            account("ac", "AC", AC_ACC, true),
            account("lb", "LB", "2b7e9c4d-1a3f-4c5e-8b6d-7f0a1c2e3d4b", true),
        ];
        let mut logged_out = view("ac", "AC", 10.0, false, now - Duration::hours(3));
        logged_out.login_state = LoginState::LoggedOut;
        let views = vec![
            view("pr", "PR", 100.0, false, now - Duration::minutes(1)),
            logged_out,
            view("lb", "LB", 10.0, false, now - Duration::minutes(1)),
        ];
        let settings = Settings {
            auto_burn: true,
            ..Settings::default()
        };
        let tick = BurnTick {
            app: &app,
            history: Some(&history),
            views: &views,
            accounts: &accounts,
            settings: &settings,
            active: Some("pr"),
            now,
            offset: offset(),
            tied_busy: 0,
            thinking: &[],
        };
        runtime.after_snapshot(&tick);
        match action.try_recv().expect("one switch") {
            ActionCommand::Switch { key, by, .. } => {
                assert_eq!(key, "lb");
                assert_eq!(by, By::Auto);
            }
            _ => panic!("not a switch"),
        }
        assert!(action.try_recv().is_err(), "only one command");
    }

    #[test]
    fn a_stale_situation_asks_for_one_poll_a_minute() {
        let now = utc("2026-11-10T12:00:00Z");
        let tmp = tempfile::tempdir().expect("a temp folder");
        let history = HistoryDir::at(tmp.path(), "atlas");
        let app = AppPaths::at(tmp.path());
        let (mut runtime, _action, engine) = runtime();
        let accounts = vec![
            account("pr", "PR", PR_ACC, true),
            account("ac", "AC", AC_ACC, true),
        ];
        let views = vec![
            view("pr", "PR", 20.0, false, now - Duration::minutes(25)),
            view("ac", "AC", 80.0, true, now - Duration::minutes(25)),
        ];
        let settings = Settings {
            auto_burn: true,
            ..Settings::default()
        };
        let tick = |at: DateTime<Utc>| BurnTick {
            app: &app,
            history: Some(&history),
            views: &views,
            accounts: &accounts,
            settings: &settings,
            active: Some("pr"),
            now: at,
            offset: offset(),
            tied_busy: 0,
            thinking: &[],
        };
        runtime.after_snapshot(&tick(now));
        assert!(matches!(
            engine.try_recv().expect("the first poll"),
            EngineCommand::Poll(PollReason::Stale)
        ));
        runtime.after_snapshot(&tick(now + Duration::seconds(10)));
        assert!(
            engine.try_recv().is_err(),
            "ten seconds later is not a new ask"
        );
        runtime.after_snapshot(&tick(now + Duration::seconds(71)));
        assert!(matches!(
            engine.try_recv().expect("the second poll"),
            EngineCommand::Poll(PollReason::Stale)
        ));
    }

    #[test]
    fn a_first_tick_without_readings_does_not_stop_decisions() {
        let now = utc("2026-11-10T12:00:00Z");
        let tmp = tempfile::tempdir().expect("a temp folder");
        let history = HistoryDir::at(tmp.path(), "atlas");
        let app = AppPaths::at(tmp.path());
        let (mut runtime, action, engine) = runtime();
        let accounts = vec![
            account("pr", "PR", PR_ACC, true),
            account("ac", "AC", AC_ACC, true),
        ];
        let settings = Settings {
            auto_burn: true,
            ..Settings::default()
        };
        let mut no_reading = view("ac", "AC", 10.0, false, now - Duration::minutes(1));
        no_reading.usage = None;
        no_reading.fetched_at = None;
        let first = vec![
            view("pr", "PR", 100.0, false, now - Duration::minutes(1)),
            no_reading,
        ];
        let first_tick = BurnTick {
            app: &app,
            history: Some(&history),
            views: &first,
            accounts: &accounts,
            settings: &settings,
            active: Some("pr"),
            now,
            offset: offset(),
            tied_busy: 0,
            thinking: &[],
        };
        let first_report = runtime.after_snapshot(&first_tick);
        let burn_view = first_report.view.expect("unread keeps the strip");
        assert_eq!(
            burn_text::plain(burn_view.line.as_deref().expect("a line")),
            "Waiting for a first reading of AC.",
            "the strip says which account is unread"
        );
        assert!(burn_view.on, "the switch stays usable");
        assert!(matches!(
            engine.try_recv().expect("a poll"),
            EngineCommand::Poll(PollReason::Stale)
        ));

        let second = vec![
            view("pr", "PR", 100.0, false, now - Duration::minutes(1)),
            view("ac", "AC", 10.0, false, now),
        ];
        let second_tick = BurnTick {
            app: &app,
            history: Some(&history),
            views: &second,
            accounts: &accounts,
            settings: &settings,
            active: Some("pr"),
            now: now + Duration::minutes(1),
            offset: offset(),
            tied_busy: 0,
            thinking: &[],
        };
        let second_report = runtime.after_snapshot(&second_tick);
        assert!(second_report.view.is_some(), "the regime is computed now");
        match action.try_recv().expect("one switch") {
            ActionCommand::Switch { key, by, .. } => {
                assert_eq!(key, "ac");
                assert_eq!(by, By::Auto);
            }
            _ => panic!("not a switch"),
        }
    }

    #[test]
    fn a_learn_error_stays_until_a_learn_succeeds() {
        let now = utc("2026-11-10T12:00:00Z");
        let tmp = tempfile::tempdir().expect("a temp folder");
        let root = tmp.path().join("history");
        std::fs::write(&root, b"not a history folder").expect("a file where the root belongs");
        let history = HistoryDir::at(&root, "atlas");
        let app = AppPaths::at(tmp.path());
        let (mut runtime, _action, _engine) = runtime();
        let accounts = vec![
            account("pr", "PR", PR_ACC, true),
            account("ac", "AC", AC_ACC, true),
        ];
        let views = vec![
            view("pr", "PR", 20.0, false, now - Duration::minutes(1)),
            view("ac", "AC", 80.0, true, now - Duration::minutes(1)),
        ];
        let settings = Settings {
            auto_burn: true,
            ..Settings::default()
        };
        let tick = |at: DateTime<Utc>| BurnTick {
            app: &app,
            history: Some(&history),
            views: &views,
            accounts: &accounts,
            settings: &settings,
            active: Some("pr"),
            now: at,
            offset: offset(),
            tied_busy: 0,
            thinking: &[],
        };
        let first = runtime.after_snapshot(&tick(now));
        let error = first.error.expect("the learn error");
        assert!(error.starts_with("auto-burn: cannot read the history:"));

        let second = runtime.after_snapshot(&tick(now + Duration::minutes(1)));
        assert_eq!(
            second.error.as_deref(),
            Some(error.as_str()),
            "no retry before five minutes, and the error stays"
        );

        std::fs::remove_file(&root).expect("the file is ours");
        std::fs::create_dir(&root).expect("a history folder");
        let third = runtime.after_snapshot(&tick(now + Duration::minutes(6)));
        assert_eq!(third.error, None, "the successful learn clears the error");
        assert!(third.view.is_some());
    }

    #[test]
    fn a_switch_that_did_not_land_is_tried_again_after_ten_minutes() {
        let now = utc("2026-11-10T12:00:00Z");
        let tmp = tempfile::tempdir().expect("a temp folder");
        let history = HistoryDir::at(tmp.path(), "atlas");
        let app = AppPaths::at(tmp.path());
        let (mut runtime, action, _engine) = runtime();
        let accounts = vec![
            account("pr", "PR", PR_ACC, true),
            account("ac", "AC", AC_ACC, true),
        ];
        let views = vec![
            view("pr", "PR", 100.0, false, now - Duration::minutes(1)),
            view("ac", "AC", 10.0, false, now - Duration::minutes(1)),
        ];
        let settings = Settings {
            auto_burn: true,
            ..Settings::default()
        };
        let tick = |at: DateTime<Utc>| BurnTick {
            app: &app,
            history: Some(&history),
            views: &views,
            accounts: &accounts,
            settings: &settings,
            active: Some("pr"),
            now: at,
            offset: offset(),
            tied_busy: 0,
            thinking: &[],
        };
        runtime.after_snapshot(&tick(now));
        let reply = match action.try_recv().expect("the first switch") {
            ActionCommand::Switch { reply, .. } => reply,
            _ => panic!("not a switch"),
        };
        reply
            .send(ActionReply::Failed("the login file is busy".to_string()))
            .expect("the runtime listens");

        let failed = runtime.after_snapshot(&tick(now + Duration::minutes(1)));
        assert_eq!(
            failed.error.as_deref(),
            Some("auto-burn: the switch to AC failed: the login file is busy")
        );
        assert!(
            action.try_recv().is_err(),
            "the same decision is not sent again before ten minutes"
        );

        runtime.after_snapshot(&tick(now + Duration::minutes(11)));
        assert!(
            action.try_recv().is_ok(),
            "the same decision is sent again after ten minutes"
        );
    }

    #[test]
    fn a_refused_switch_shows_why() {
        let now = utc("2026-11-10T12:00:00Z");
        let tmp = tempfile::tempdir().expect("a temp folder");
        let history = HistoryDir::at(tmp.path(), "atlas");
        let app = AppPaths::at(tmp.path());
        let (mut runtime, action, _engine) = runtime();
        let accounts = vec![
            account("pr", "PR", PR_ACC, true),
            account("ac", "AC", AC_ACC, true),
        ];
        let views = vec![
            view("pr", "PR", 100.0, false, now - Duration::minutes(1)),
            view("ac", "AC", 10.0, false, now - Duration::minutes(1)),
        ];
        let settings = Settings {
            auto_burn: true,
            ..Settings::default()
        };
        let tick = |at: DateTime<Utc>| BurnTick {
            app: &app,
            history: Some(&history),
            views: &views,
            accounts: &accounts,
            settings: &settings,
            active: Some("pr"),
            now: at,
            offset: offset(),
            tied_busy: 0,
            thinking: &[],
        };
        runtime.after_snapshot(&tick(now));
        let reply = match action.try_recv().expect("the switch") {
            ActionCommand::Switch { reply, .. } => reply,
            _ => panic!("not a switch"),
        };
        reply
            .send(ActionReply::NotSaved(PendingSwitch {
                target_id: "ac".to_string(),
                target_label: "AC".to_string(),
                email: None,
                org_name: None,
            }))
            .expect("the runtime listens");

        let report = runtime.after_snapshot(&tick(now + Duration::minutes(1)));
        assert_eq!(
            report.error.as_deref(),
            Some("auto-burn could not leave PR: the current login is not saved")
        );
    }

    #[test]
    fn a_user_switch_holds_nothing_back() {
        let now = utc("2026-11-10T12:00:00Z");
        let event = Hosted {
            host: "atlas".to_string(),
            record: Event {
                t: now - Duration::hours(1),
                kind: EventKind::Switch {
                    from: None,
                    to: AccountRef {
                        acc: AC_ACC.to_string(),
                        org: ORG.to_string(),
                    },
                    by: By::User,
                    reason: None,
                    cwd: None,
                    sessions: None,
                    waited_secs: None,
                },
            },
        };
        let accounts = vec![
            account("pr", "PR", PR_ACC, true),
            account("ac", "AC", AC_ACC, true),
        ];
        let mut rates = BTreeMap::new();
        // PR fills its week fast and its 5 h windows are small; AC can work far
        // longer, so the early rule switches to PR although AC was chosen by
        // hand an hour ago (since 1.5.2 a switch by hand holds nothing back).
        rates.insert(AccountRef::of(&accounts[0]), Rates::given(1.0, Some(100.0)));
        rates.insert(AccountRef::of(&accounts[1]), Rates::given(1.0, None));
        let learned = Learned {
            rhythm: Rhythm { usd: [1.0; 168] },
            rates,
            cost: None,
            warnings: Vec::new(),
            events: vec![event],
            host: "atlas".to_string(),
            sessions_off: false,
        };
        let views = vec![
            view("pr", "PR", 10.0, true, now - Duration::minutes(1)),
            view("ac", "AC", 10.0, false, now - Duration::minutes(1)),
        ];
        let readiness = live::situation(
            &learned,
            &views,
            &accounts,
            Some("ac"),
            now,
            offset(),
            0,
            &[],
        )
        .expect("the views are valid");
        let Readiness::Ready(situation) = readiness else {
            panic!("every account has a reading");
        };
        let (mut runtime, action, _engine) = runtime();
        let settings = Settings {
            auto_burn: true,
            ..Settings::default()
        };
        let report =
            runtime.decision_report(&situation, &regime(true), &views, &settings, now, offset());
        assert_eq!(report.view.expect("a view").paused_until, None);
        match action.try_recv().expect("a switch to PR") {
            ActionCommand::Switch { key, .. } => assert_eq!(key, "pr"),
            _ => panic!("not a switch"),
        }
    }

    #[test]
    fn stale_readings_ask_for_a_poll_and_do_not_switch() {
        let now = utc("2026-11-10T12:00:00Z");
        let (mut runtime, action, engine) = runtime();
        let views = vec![
            view("pr", "PR", 20.0, false, now - Duration::minutes(25)),
            view("ac", "AC", 80.0, true, now - Duration::minutes(25)),
        ];
        let mut situation = situation(now, Some("pr"));
        situation.oldest_reading = now - Duration::minutes(25);
        let settings = Settings {
            auto_burn: true,
            ..Settings::default()
        };
        runtime.decision_report(&situation, &regime(true), &views, &settings, now, offset());
        assert!(action.try_recv().is_err(), "stale readings do not switch");
        assert!(matches!(
            engine.try_recv().expect("a poll"),
            EngineCommand::Poll(PollReason::Stale)
        ));
    }

    #[test]
    fn the_line_names_the_account_in_use_and_the_next() {
        let now = utc("2026-11-10T12:00:00Z");
        let views = vec![
            view("pr", "PR", 20.0, false, now - Duration::minutes(1)),
            view("ac", "AC", 80.0, true, now - Duration::minutes(1)),
        ];
        let line = burn_text::line(&plan(now), Some("pr"), &views, now)
            .expect("a line")
            .expect("a next account");
        assert_eq!(
            burn_text::plain(&line),
            "On PR until its week is full, about 2h. Then AC."
        );
    }

    #[test]
    fn planned_holds_the_first_use_of_each_other_account() {
        let now = utc("2026-11-10T12:00:00Z");
        let plan = Plan {
            segments: vec![
                Segment {
                    account: Some("pr".to_string()),
                    from: now,
                    to: now + Duration::hours(1),
                },
                Segment {
                    account: Some("ac".to_string()),
                    from: now + Duration::hours(1),
                    to: now + Duration::hours(2),
                },
                Segment {
                    account: Some("lb".to_string()),
                    from: now + Duration::hours(2),
                    to: now + Duration::hours(3),
                },
                Segment {
                    account: Some("pr".to_string()),
                    from: now + Duration::hours(3),
                    to: now + Duration::hours(4),
                },
            ],
            blocked_usd: 0.0,
            switches: 2,
            left_at_reset: BTreeMap::new(),
            full_at: BTreeMap::new(),
            full_by: BTreeMap::new(),
        };
        let planned = burn_text::planned(&plan, Some("pr"));
        assert_eq!(planned.get("ac"), Some(&(now + Duration::hours(1))));
        assert_eq!(planned.get("lb"), Some(&(now + Duration::hours(2))));
        assert!(!planned.contains_key("pr"));
    }

    #[test]
    fn the_reply_is_read_after_the_account_changed() {
        let now = utc("2026-11-10T12:00:00Z");
        let tmp = tempfile::tempdir().expect("a temp folder");
        let history = HistoryDir::at(tmp.path(), "atlas");
        let app = AppPaths::at(tmp.path());
        let (mut runtime, action, _engine) = runtime();
        let accounts = vec![
            account("pr", "PR", PR_ACC, true),
            account("ac", "AC", AC_ACC, true),
        ];
        let views = vec![
            view("pr", "PR", 100.0, false, now - Duration::minutes(1)),
            view("ac", "AC", 10.0, true, now - Duration::minutes(1)),
        ];
        let settings = Settings {
            auto_burn: true,
            ..Settings::default()
        };
        let tick = |at: DateTime<Utc>, active: &'static str| BurnTick {
            app: &app,
            history: Some(&history),
            views: &views,
            accounts: &accounts,
            settings: &settings,
            active: Some(active),
            now: at,
            offset: offset(),
            tied_busy: 0,
            thinking: &[],
        };

        runtime.after_snapshot(&tick(now, "pr"));
        let reply = match action.try_recv().expect("the switch") {
            ActionCommand::Switch { reply, .. } => reply,
            _ => panic!("not a switch"),
        };
        // Claude Code's files already name AC, but the action thread has not
        // answered the switch yet: the tick decides Stay and must keep waiting.
        let changed = runtime.after_snapshot(&tick(now + Duration::seconds(30), "ac"));
        assert_eq!(changed.notification, None, "the switch has not landed yet");

        reply
            .send(ActionReply::Switched("ac".to_string()))
            .expect("the runtime listens");
        let answered = runtime.after_snapshot(&tick(now + Duration::minutes(1), "ac"));
        let notification = answered.notification.expect("the landed switch notifies");
        assert!(notification.contains("now on AC"), "{notification}");
        let again = runtime.after_snapshot(&tick(now + Duration::minutes(2), "ac"));
        assert_eq!(again.notification, None, "the notification is sent once");
    }

    #[test]
    fn a_moved_until_does_not_send_the_switch_again() {
        let now = utc("2026-11-10T12:00:00Z");
        let (mut runtime, action, _engine) = runtime();
        let views = vec![
            view("pr", "PR", 100.0, false, now - Duration::minutes(1)),
            view("ac", "AC", 10.0, true, now - Duration::minutes(1)),
        ];
        let settings = Settings {
            auto_burn: true,
            ..Settings::default()
        };
        let situation = situation(now, Some("pr"));
        let plan = plan(now);
        let regime = regime(true);
        let send = |runtime: &mut BurnRuntime, until: DateTime<Utc>, at: DateTime<Utc>| {
            runtime.apply_decision(
                Decision::Switch {
                    to: "ac".to_string(),
                    reason: Reason::ActiveFull { until: Some(until) },
                },
                &situation,
                &plan,
                &regime,
                &views,
                &settings,
                at,
                offset(),
            );
        };
        send(&mut runtime, now + Duration::hours(1), now);
        send(
            &mut runtime,
            now + Duration::hours(1) + Duration::seconds(10),
            now + Duration::seconds(10),
        );
        assert!(action.try_recv().is_ok(), "the first decision switches");
        assert!(
            action.try_recv().is_err(),
            "a target whose reset moved by ten seconds is the same target"
        );
    }

    #[test]
    fn a_stale_situation_computes_no_regime() {
        let now = utc("2026-11-10T12:00:00Z");
        let tmp = tempfile::tempdir().expect("a temp folder");
        let history = HistoryDir::at(tmp.path(), "atlas");
        let app = AppPaths::at(tmp.path());
        let (mut runtime, _action, engine) = runtime();
        let accounts = vec![
            account("pr", "PR", PR_ACC, true),
            account("ac", "AC", AC_ACC, true),
        ];
        let views = vec![
            view("pr", "PR", 20.0, false, now - Duration::minutes(25)),
            view("ac", "AC", 80.0, true, now - Duration::minutes(25)),
        ];
        let settings = Settings {
            auto_burn: true,
            ..Settings::default()
        };
        let tick = BurnTick {
            app: &app,
            history: Some(&history),
            views: &views,
            accounts: &accounts,
            settings: &settings,
            active: Some("pr"),
            now,
            offset: offset(),
            tied_busy: 0,
            thinking: &[],
        };
        runtime.after_snapshot(&tick);
        assert!(runtime.regime.is_none(), "a stale situation has no regime");
        assert!(runtime.regime_at.is_none());
        assert!(matches!(
            engine.try_recv().expect("a stale poll"),
            EngineCommand::Poll(PollReason::Stale)
        ));
    }

    #[test]
    fn an_account_turned_on_computes_the_regime_again() {
        let now = utc("2026-11-10T12:00:00Z");
        let tmp = tempfile::tempdir().expect("a temp folder");
        let history = HistoryDir::at(tmp.path(), "atlas");
        let app = AppPaths::at(tmp.path());
        let (mut runtime, _action, _engine) = runtime();
        let mut accounts = vec![
            account("pr", "PR", PR_ACC, true),
            account("ac", "AC", AC_ACC, false),
        ];
        let views = vec![
            view("pr", "PR", 20.0, false, now - Duration::minutes(1)),
            view("ac", "AC", 80.0, true, now - Duration::minutes(1)),
        ];
        let settings = Settings {
            auto_burn: true,
            ..Settings::default()
        };
        let first = BurnTick {
            app: &app,
            history: Some(&history),
            views: &views,
            accounts: &accounts,
            settings: &settings,
            active: Some("pr"),
            now,
            offset: offset(),
            tied_busy: 0,
            thinking: &[],
        };
        runtime.after_snapshot(&first);
        assert_eq!(runtime.regime_at, Some(now));

        accounts[1].auto_burn = true;
        let second = BurnTick {
            app: &app,
            history: Some(&history),
            views: &views,
            accounts: &accounts,
            settings: &settings,
            active: Some("pr"),
            now: now + Duration::minutes(1),
            offset: offset(),
            tied_busy: 0,
            thinking: &[],
        };
        runtime.after_snapshot(&second);
        assert_eq!(
            runtime.regime_at,
            Some(now + Duration::minutes(1)),
            "a newly usable account computes the regime again"
        );
    }

    #[test]
    fn a_switch_event_that_lands_late_is_learned() {
        let now = utc("2026-11-10T12:00:00Z");
        let tmp = tempfile::tempdir().expect("a temp folder");
        let history = HistoryDir::at(tmp.path(), "atlas");
        let app = AppPaths::at(tmp.path());
        let recorder = Recorder::new(HistoryDir::at(history.root(), history.host()));
        let accounts = vec![
            account("pr", "PR", PR_ACC, true),
            account("ac", "AC", AC_ACC, true),
        ];
        let switch = |to: AccountRef| EventKind::Switch {
            from: None,
            to,
            by: By::Cli,
            reason: None,
            cwd: None,
            sessions: None,
            waited_secs: None,
        };
        recorder
            .event(
                now - Duration::minutes(10),
                switch(AccountRef::of(&accounts[0])),
            )
            .expect("the old switch event");
        let views = vec![
            view("pr", "PR", 20.0, false, now - Duration::minutes(1)),
            view("ac", "AC", 80.0, true, now - Duration::minutes(1)),
        ];
        let settings = Settings {
            auto_burn: true,
            ..Settings::default()
        };
        let tick = |at: DateTime<Utc>| BurnTick {
            app: &app,
            history: Some(&history),
            views: &views,
            accounts: &accounts,
            settings: &settings,
            active: Some("ac"),
            now: at,
            offset: offset(),
            tied_busy: 0,
            thinking: &[],
        };

        let mut runtime = runtime().0;
        runtime.after_snapshot(&tick(now));
        assert_eq!(
            newest_switch_target(
                runtime.learned.as_ref().expect("the first learn"),
                now + Duration::seconds(30)
            ),
            Some(&AccountRef::of(&accounts[0])),
            "the learned history is still on PR"
        );

        recorder
            .event(
                now + Duration::seconds(30),
                switch(AccountRef::of(&accounts[1])),
            )
            .expect("the late switch event");
        runtime.after_snapshot(&tick(now + Duration::minutes(1)));
        assert_eq!(
            newest_switch_target(
                runtime.learned.as_ref().expect("the second learn"),
                now + Duration::minutes(1)
            ),
            Some(&AccountRef::of(&accounts[1])),
            "the event that landed late is learned at the next tick"
        );
    }

    #[test]
    fn an_unread_account_keeps_the_strip_and_says_why() {
        let now = utc("2026-11-10T12:00:00Z");
        let tmp = tempfile::tempdir().expect("a temp folder");
        let history = HistoryDir::at(tmp.path(), "atlas");
        let app = AppPaths::at(tmp.path());
        let (mut runtime, _action, _engine) = runtime();
        let accounts = vec![
            account("lb", "LB", PR_ACC, true),
            account("ac", "AC", AC_ACC, true),
        ];
        let mut lb = view("lb", "LB", 20.0, false, now);
        lb.usage = None;
        lb.fetched_at = None;
        let mut ac = view("ac", "AC", 10.0, true, now);
        ac.usage = None;
        ac.fetched_at = None;
        let views = vec![lb, ac];
        let settings = Settings {
            auto_burn: true,
            ..Settings::default()
        };
        let tick = BurnTick {
            app: &app,
            history: Some(&history),
            views: &views,
            accounts: &accounts,
            settings: &settings,
            active: None,
            now,
            offset: offset(),
            tied_busy: 0,
            thinking: &[],
        };

        let report = runtime.after_snapshot(&tick);
        let view = report.view.expect("the strip stays for an unread account");
        assert!(view.on, "the auto-burn switch still works");
        assert_eq!(
            burn_text::plain(view.line.as_deref().expect("a line")),
            "Waiting for a first reading of LB, AC.",
            "the strip names the unread accounts in account order"
        );
        assert_eq!(
            view.unread,
            vec!["lb".to_string(), "ac".to_string()],
            "the Forecast page names the same accounts"
        );
        assert!(report.forecast.is_none(), "no plan without every reading");
    }

    /// A new install has no stored account: the strip asks for the first one
    /// instead of saying that no login works. Stored accounts whose logins all
    /// ended keep the old line.
    #[test]
    fn no_stored_account_asks_for_one() {
        let now = utc("2026-11-10T12:00:00Z");
        let tmp = tempfile::tempdir().expect("a temp folder");
        let history = HistoryDir::at(tmp.path(), "atlas");
        let app = AppPaths::at(tmp.path());
        let settings = Settings::default();
        let line_for = |accounts: &[Account], views: &[AccountView]| {
            let (mut runtime, _action, _engine) = runtime();
            let tick = BurnTick {
                app: &app,
                history: Some(&history),
                views,
                accounts,
                settings: &settings,
                active: None,
                now,
                offset: offset(),
                tied_busy: 0,
                thinking: &[],
            };
            let report = runtime.after_snapshot(&tick);
            assert!(report.forecast.is_none(), "no account, no plan");
            let view = report.view.expect("the strip stays");
            assert!(view.unread.is_empty(), "no account waits for a reading");
            burn_text::plain(view.line.as_deref().expect("a line"))
        };

        assert_eq!(line_for(&[], &[]), "Save an account to start.");

        let mut lb = account("lb", "LB", PR_ACC, true);
        lb.login_state = LoginState::LoggedOut;
        let mut lb_view = view("lb", "LB", 20.0, false, now);
        lb_view.login_state = LoginState::LoggedOut;
        lb_view.usage = None;
        lb_view.fetched_at = None;
        assert_eq!(
            line_for(&[lb], &[lb_view]),
            "No account auto-burn may use has a working login."
        );
    }

    #[test]
    fn the_next_account_is_written_for_the_status_line() {
        let now = utc("2026-11-10T12:00:00Z");
        let tmp = tempfile::tempdir().expect("a temp folder");
        let history = HistoryDir::at(tmp.path(), "atlas");
        let app = AppPaths::at(tmp.path());
        let (mut runtime, action, _engine) = runtime();
        let accounts = vec![
            account("pr", "PR", PR_ACC, true),
            account("ac", "AC", AC_ACC, true),
        ];
        let views = vec![
            view("pr", "PR", 100.0, false, now - Duration::minutes(1)),
            view("ac", "AC", 10.0, true, now - Duration::minutes(1)),
        ];
        let settings = Settings {
            auto_burn: true,
            ..Settings::default()
        };
        let tick = BurnTick {
            app: &app,
            history: Some(&history),
            views: &views,
            accounts: &accounts,
            settings: &settings,
            active: Some("pr"),
            now,
            offset: offset(),
            tied_busy: 0,
            thinking: &[],
        };

        runtime.after_snapshot(&tick);
        match action.try_recv().expect("the switch") {
            ActionCommand::Switch { key, .. } => assert_eq!(key, "ac"),
            _ => panic!("not a switch"),
        }
        let written = burn_next::read(&app, now)
            .expect("burn-next.json is readable")
            .expect("the next account is written");
        assert_eq!(written.account, "ac");
        assert_eq!(written.at, now, "the switch is planned now");
        assert_eq!(written.written, now);
    }
}
