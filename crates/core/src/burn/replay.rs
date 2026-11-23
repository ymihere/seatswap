//! The replay (spec §2.4): play the work recorded in a range of the history
//! against the accounts' limits as they stood at the range's start, once for
//! each policy.

use std::collections::BTreeMap;

use chrono::{DateTime, Offset, Utc};

use crate::api::LimitKind;
use crate::history::{
    AccountRef, HistoryDir, HistoryError, Hosted, Hosts, Range, Record, Sample, TokenRecord,
};
use crate::money::{api_usd, is_plan_model, Prices};
use crate::transcripts::hour_start;

use super::plan::{play, AccountState, Inputs, PlanError, Policy, SessionState, Walk, Work};
use super::rates::hour_shares;
use super::{mean_over_hours, Rates, Rhythm};

/// What one policy did with the recorded work of a range.
#[derive(Debug, Clone, PartialEq)]
pub struct ReplayResult {
    pub policy: Policy,
    /// The API dollars of work of the steps that had an account.
    pub worked_usd: f64,
    /// The API dollars of work of the steps without a usable account.
    pub blocked_usd: f64,
    pub switches: u32,
    /// The week-% unused at every weekly reset the replay passed, summed per
    /// account.
    pub expired: BTreeMap<AccountRef, f64>,
    /// The mean dollars of the recorded hours that had work: the "typical working
    /// hour" the play's eager rule used. 0.0 when no hour had work.
    pub typical_usd: f64,
    /// The warnings of the sample and token reads, for the caller that shows
    /// them (Task 9's CLI).
    pub warnings: Vec<String>,
}

/// Why a replay cannot be played.
#[derive(Debug, thiserror::Error)]
pub enum ReplayError {
    #[error(transparent)]
    History(#[from] HistoryError),
    #[error(transparent)]
    Plan(#[from] PlanError),
    #[error("account {account}: its first sample at {at} has no {missing}")]
    Unplayable {
        account: String,
        at: DateTime<Utc>,
        missing: &'static str,
    },
}

/// Play the recorded work of `range` (the API dollars of the Claude token
/// records, [`recorded_hours`], all hosts together) against the accounts' limits
/// as they stood at `range.from`: each account starts from its first sample in
/// the range (weekly percent and reset, session percent and its reset; a sample
/// without a session limit is an uncapped account). Ids inside the play are
/// `"<acc>/<org>"`, unique per [`AccountRef`]; [`ReplayResult::expired`] names
/// the full [`AccountRef`] again. `rates` is used as given: an account without
/// an entry never fills, as the plan says. `without` leaves accounts out, and
/// the replay starts with no active account, so its first step chooses.
///
/// A sample that cannot be read is the [`ReplayError::History`] error. A sample
/// that cannot be played — no weekly limit, or a weekly limit without a reset
/// time — is [`ReplayError::Unplayable`]; every input is checked with
/// [`Inputs::check`] before the play. The warnings of the reads are kept in
/// [`ReplayResult::warnings`] for the caller that shows them.
pub fn replay(
    history: &HistoryDir,
    range: &Range,
    rates: &BTreeMap<AccountRef, Rates>,
    without: &[AccountRef],
    policy: Policy,
    prices: &Prices,
) -> Result<ReplayResult, ReplayError> {
    let sample_read = history.read_claude::<Sample>(range, &Hosts::All)?;
    let token_read = history.read::<TokenRecord>(range, &Hosts::All)?;
    let mut warnings = sample_read.warnings;
    warnings.extend(token_read.warnings);
    let mut result = replay_records(
        &sample_read.records,
        &token_read.records,
        range,
        rates,
        without,
        policy,
        prices,
    )?;
    result.warnings = warnings;
    Ok(result)
}

/// [`replay`] over records the caller has read already, so a caller that
/// replays many ranges of one longer range reads the history once: the Claude
/// samples ([`HistoryDir::read_claude`]) and the token records
/// ([`HistoryDir::read`]) of a range that holds `range`, sorted by time as a
/// read returns them. Only the records inside `range` are played. The result
/// has no warnings: the caller's read has them.
pub fn replay_records(
    samples: &[Hosted<Sample>],
    tokens: &[Hosted<TokenRecord>],
    range: &Range,
    rates: &BTreeMap<AccountRef, Rates>,
    without: &[AccountRef],
    policy: Policy,
    prices: &Prices,
) -> Result<ReplayResult, ReplayError> {
    let samples = inside(samples, range);
    let tokens = inside(tokens, range);

    let mut accounts = Vec::new();
    let mut refs: BTreeMap<String, AccountRef> = BTreeMap::new();
    for (account, sample) in first_samples(samples, without) {
        let week = sample
            .limit(&LimitKind::WeeklyAll)
            .ok_or_else(|| unplayable(account, sample.t, "weekly limit"))?;
        let week_resets_at = week
            .r
            .ok_or_else(|| unplayable(account, sample.t, "weekly reset"))?;
        let session = sample.limit(&LimitKind::Session).map(|limit| SessionState {
            percent: limit.p,
            resets_at: limit.r,
        });
        let id = id_of(account);
        accounts.push(AccountState {
            id: id.clone(),
            may_use: true,
            login_ok: true,
            week_percent: week.p,
            week_resets_at,
            session,
            model_blocked: false,
            rates: rates.get(account).copied(),
            thinking_held: 0,
        });
        refs.insert(id, account.clone());
    }

    let hours = recorded_hours(tokens, prices);
    let walk = Walk::new(Work::Recorded(&hours), range.to);
    let inputs = Inputs {
        now: range.from,
        offset: range.from.offset().fix(),
        accounts,
        active: None,
        // The walk carries the recorded work and its typical hour; a replay
        // never reads the learned rhythm.
        rhythm: Rhythm { usd: [0.0; 168] },
        // A replay plays its range from the start, as auto-burn would have
        // from no switch and no hold.
        last_switch_at: None,
        manual_hold_until: None,
    };
    inputs.check()?;
    let typical_usd = mean_over_hours(hours.values().copied());
    let played = play(&inputs, policy, &walk, typical_usd);
    let expired = played
        .expired
        .into_iter()
        .filter_map(|(id, expired)| refs.get(&id).map(|account| (account.clone(), expired)))
        .collect();
    Ok(ReplayResult {
        policy,
        worked_usd: played.worked_usd,
        blocked_usd: played.plan.blocked_usd,
        switches: played.plan.switches,
        expired,
        typical_usd,
        warnings: Vec::new(),
    })
}

/// The records of `records` (sorted by time, as a history read returns them)
/// whose time lies in `range`: what a read of `range` itself returns.
fn inside<'a, R: Record>(records: &'a [Hosted<R>], range: &Range) -> &'a [Hosted<R>] {
    debug_assert!(
        records.is_sorted_by_key(|hosted| hosted.record.at()),
        "a replay needs its records sorted by time"
    );
    let from = records.partition_point(|hosted| hosted.record.at() < range.from);
    let to = records.partition_point(|hosted| hosted.record.at() < range.to);
    &records[from..to.max(from)]
}

/// The id a play uses for `account`: unique per [`AccountRef`], unlike the
/// server's `acc` alone, which two organizations can share.
fn id_of(account: &AccountRef) -> String {
    format!("{}/{}", account.acc, account.org)
}

/// The first sample in the range of every account that is not in `without`.
fn first_samples<'a>(
    samples: &'a [Hosted<Sample>],
    without: &[AccountRef],
) -> Vec<(&'a AccountRef, &'a Sample)> {
    let mut firsts: BTreeMap<&AccountRef, &Sample> = BTreeMap::new();
    for hosted in samples {
        let sample = &hosted.record;
        if without.contains(&sample.account) {
            continue;
        }
        firsts.entry(&sample.account).or_insert(sample);
    }
    firsts.into_iter().collect()
}

/// The API dollars of work recorded in each UTC hour, all hosts together. Only
/// Claude work counts: a Codex or Ollama record uses no Claude account and is
/// left out (R7 of 1.3), and so is a record whose model has no price
/// ([`api_usd`] is `None`, documented rule). A line's work is spread over the
/// hours its [`TokenRecord::span`] covers by [`hour_shares`], so a 5-minute
/// line lands wholly in its hour and an hourly line counts as before.
fn recorded_hours(tokens: &[Hosted<TokenRecord>], prices: &Prices) -> BTreeMap<DateTime<Utc>, f64> {
    let mut hours: BTreeMap<DateTime<Utc>, f64> = BTreeMap::new();
    for hosted in tokens {
        let record = &hosted.record;
        if !is_plan_model(&record.model) {
            continue;
        }
        let Some(usd) = api_usd(record, prices) else {
            continue;
        };
        for (hour, share) in hour_shares(record, hour_start) {
            *hours.entry(hour).or_insert(0.0) += usd * share;
        }
    }
    hours
}

/// The first sample of `account` cannot be played.
fn unplayable(account: &AccountRef, at: DateTime<Utc>, missing: &'static str) -> ReplayError {
    ReplayError::Unplayable {
        account: id_of(account),
        at,
        missing,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    use chrono::{Duration, FixedOffset};

    use crate::burn::plan::testkit::{crowded, inputs, now, offset, EPS};
    use crate::history::{LimitSample, Source, TokenRecord};
    use crate::project::ProjectKey;
    use crate::stats::tests::append_codex_records;

    /// The models.dev fixture's prices: `claude-opus-5-5` costs 4 USD per
    /// million input tokens, `deepseek-v4.1-flash:cloud` has a price too.
    fn prices() -> Prices {
        crate::money::tests::priced_prices(now())
    }

    fn account_ref(id: &str) -> AccountRef {
        AccountRef {
            acc: id.to_string(),
            org: format!("org-{id}"),
        }
    }

    /// One week of token records shaped like Task 5's rhythm: every working hour
    /// is worth the rhythm's 1.0 USD. The range is `now` to `now + 7 d - 1 h`, so
    /// it holds the week's 92 USD before PR's Saturday reset and none of
    /// the next Monday morning.
    fn setup() -> (
        tempfile::TempDir,
        HistoryDir,
        BTreeMap<AccountRef, Rates>,
        Range,
    ) {
        setup_from(inputs())
    }

    /// [`setup`] for other accounts: each account's first sample has its
    /// week-% from `inputs`.
    fn setup_from(
        inputs: crate::burn::plan::Inputs,
    ) -> (
        tempfile::TempDir,
        HistoryDir,
        BTreeMap<AccountRef, Rates>,
        Range,
    ) {
        let dir = tempfile::tempdir().expect("temp dir");
        let history = HistoryDir::at(dir.path(), "atlas");
        let range = Range {
            from: now(),
            to: now() + Duration::days(7) - Duration::hours(1),
        };

        for state in &inputs.accounts {
            let mut limits = vec![LimitSample {
                k: LimitKind::WeeklyAll,
                m: None,
                p: state.week_percent,
                r: Some(state.week_resets_at),
                b: false,
            }];
            if state.session.is_some() {
                limits.push(LimitSample {
                    k: LimitKind::Session,
                    m: None,
                    p: 0.0,
                    r: None,
                    b: false,
                });
            }
            history
                .append(&Sample {
                    t: range.from,
                    account: account_ref(&state.id),
                    src: Source::Poll,
                    l: limits,
                })
                .expect("write the sample");
        }

        history
            .append_many(&tokens(&inputs.rhythm, &range, offset()))
            .expect("write the token records");

        let rates = inputs
            .accounts
            .iter()
            .map(|state| {
                (
                    account_ref(&state.id),
                    state.rates.expect("the setup's rates"),
                )
            })
            .collect();
        (dir, history, rates, range)
    }

    /// Token records of one hour each of `claude-opus-5-5` (4 USD per
    /// million input tokens), worth that hour's rhythm dollars.
    fn tokens(rhythm: &Rhythm, range: &Range, offset: FixedOffset) -> Vec<TokenRecord> {
        let mut records = Vec::new();
        let mut at = range.from;
        while at < range.to {
            let work = rhythm.at(at, offset);
            if work > 0.0 {
                records.push(TokenRecord {
                    t: at,
                    minutes: 60,
                    session: "7c1e4b2a-9d3f-4e6a-b8c5-2f0a1d7e9b34".to_string(),
                    cwd: "/home/sam/projects/atlas".to_string(),
                    project: ProjectKey::Repo("github.com/sam/atlas".to_string()),
                    model: "claude-opus-5-5".to_string(),
                    input: (work * 250_000.0).round() as u64,
                    out: 0,
                    cw: 0,
                    cr: 0,
                    answers: 1,
                    agent_answers: 0,
                });
            }
            at += Duration::hours(1);
        }
        records
    }

    #[test]
    fn replay_lazy_and_eager_differ_as_in_the_model() {
        // The crowded setup: AC and LB reset together on Wednesday, and the
        // eager plan starts on them at once.
        let inputs = crowded();
        let (_dir, history, rates, range) = setup_from(inputs);
        let lazy = replay(&history, &range, &rates, &[], Policy::Lazy, &prices()).expect("replay");
        let eager =
            replay(&history, &range, &rates, &[], Policy::Eager, &prices()).expect("replay");

        // Within the recorded week neither blocks work, but the lazy plan
        // leaves 4 % of PR unused at its Saturday reset, the eager plan 2 %,
        // with many more switches.
        assert!(lazy.blocked_usd.abs() < EPS, "{}", lazy.blocked_usd);
        assert!(eager.blocked_usd.abs() < EPS, "{}", eager.blocked_usd);
        let pr = account_ref("PR");
        assert!((lazy.expired[&pr] - 4.0).abs() < EPS, "{:?}", lazy.expired);
        assert!(
            (eager.expired[&pr] - 2.0).abs() < EPS,
            "{:?}",
            eager.expired
        );
        assert_eq!((lazy.switches, eager.switches), (11, 27));
        for result in [&lazy, &eager] {
            let worked = result.worked_usd + result.blocked_usd;
            assert!((worked - 92.0).abs() < EPS, "{worked}");
        }
        assert_eq!(lazy.policy, Policy::Lazy);
        assert_eq!(eager.policy, Policy::Eager);
    }

    #[test]
    fn replay_without_an_account_blocks_more() {
        let (_dir, history, rates, range) = setup();
        let with = replay(&history, &range, &rates, &[], Policy::Lazy, &prices()).expect("replay");
        let without = replay(
            &history,
            &range,
            &rates,
            &[account_ref("AC")],
            Policy::Lazy,
            &prices(),
        )
        .expect("replay");

        assert!(
            without.blocked_usd > with.blocked_usd,
            "{} vs {}",
            without.blocked_usd,
            with.blocked_usd
        );
    }

    // ---- proof tests of the review ----

    /// A history with one account whose first sample cannot be played.
    fn one_account(sample: Sample) -> (tempfile::TempDir, HistoryDir, Range) {
        let dir = tempfile::tempdir().expect("temp dir");
        let history = HistoryDir::at(dir.path(), "atlas");
        let range = Range {
            from: now(),
            to: now() + Duration::hours(1),
        };
        history.append(&sample).expect("write the sample");
        (dir, history, range)
    }

    #[test]
    fn a_sample_the_plan_cannot_play_is_an_error() {
        // A 5 h window at 60 % without a reset time cannot be played: the
        // plan's own check rejects such an input, so the replay must not plan
        // with it (it would take the account for empty).
        let (_dir, history, range) = one_account(Sample {
            t: now(),
            account: account_ref("AC"),
            src: Source::Poll,
            l: vec![
                LimitSample {
                    k: LimitKind::WeeklyAll,
                    m: None,
                    p: 0.0,
                    r: Some(now() + Duration::days(5)),
                    b: false,
                },
                LimitSample {
                    k: LimitKind::Session,
                    m: None,
                    p: 60.0,
                    r: None,
                    b: false,
                },
            ],
        });
        let account = account_ref("AC");
        let rates = BTreeMap::from([(account, Rates::given(4.0, Some(100.0)))]);

        let err = replay(&history, &range, &rates, &[], Policy::Lazy, &prices())
            .expect_err("a window without a reset time cannot be played");
        assert!(err.to_string().contains("AC"), "{err}");
    }

    #[test]
    fn the_error_of_a_sample_that_cannot_be_played_is_not_an_io_error() {
        // The account's first sample has no weekly limit: no file was read or
        // written wrongly, so the error must not be an I/O error about a path
        // that does not exist.
        let (_dir, history, range) = one_account(Sample {
            t: now(),
            account: account_ref("AC"),
            src: Source::Poll,
            l: vec![LimitSample {
                k: LimitKind::Session,
                m: None,
                p: 10.0,
                r: Some(now() + Duration::hours(2)),
                b: false,
            }],
        });

        let err = replay(
            &history,
            &range,
            &BTreeMap::new(),
            &[],
            Policy::Lazy,
            &prices(),
        )
        .expect_err("a sample without a weekly limit cannot be played");
        assert!(
            !matches!(err, ReplayError::History(HistoryError::Io { .. })),
            "{err}"
        );
    }

    #[test]
    fn two_accounts_that_share_an_acc_get_two_ids() {
        // Two organizations can use the same `acc`; the play's ids must tell
        // them apart, so both accounts keep their own expired percent.
        let dir = tempfile::tempdir().expect("temp dir");
        let history = HistoryDir::at(dir.path(), "atlas");
        let range = Range {
            from: now(),
            to: now() + Duration::days(7),
        };
        let first = AccountRef {
            acc: "6f1c2a9e-1d4b-4c7f-9a3e-8b5d2e6f1c04".to_string(),
            org: "a92d7b31-5c8e-4f2a-9d6b-1e3c5a7f9b20".to_string(),
        };
        let second = AccountRef {
            acc: first.acc.clone(),
            org: "c41e6b82-7d3a-4f95-b2c8-5a1d9e3f7b46".to_string(),
        };
        for account in [&first, &second] {
            history
                .append(&Sample {
                    t: range.from,
                    account: account.clone(),
                    src: Source::Poll,
                    l: vec![LimitSample {
                        k: LimitKind::WeeklyAll,
                        m: None,
                        p: 0.0,
                        r: Some(range.from + Duration::days(1)),
                        b: false,
                    }],
                })
                .expect("write the sample");
        }
        let rates = BTreeMap::from([
            (first.clone(), Rates::given(1.0, None)),
            (second.clone(), Rates::given(1.0, None)),
        ]);

        let result = replay(&history, &range, &rates, &[], Policy::Lazy, &prices())
            .expect("two accounts with one acc do not collapse");
        assert_eq!(result.expired.len(), 2);
        assert!((result.expired[&first] - 100.0).abs() < EPS);
        assert!((result.expired[&second] - 100.0).abs() < EPS);
    }

    #[test]
    fn the_replay_keeps_the_read_warnings() {
        use std::fs::OpenOptions;
        use std::io::Write;

        let (dir, history, rates, range) = setup();
        let path = dir.path().join("atlas/samples-2026-11.jsonl");
        let mut file = OpenOptions::new()
            .append(true)
            .open(&path)
            .expect("open the samples file");
        file.write_all(b"{\"t\":\"2026-11-14T1")
            .expect("append a torn line");
        drop(file);

        let result =
            replay(&history, &range, &rates, &[], Policy::Lazy, &prices()).expect("replay");
        assert_eq!(result.warnings.len(), 1);
        assert!(
            result.warnings[0].contains("samples-2026-11.jsonl"),
            "{:?}",
            result.warnings
        );
    }

    /// A replay over records read once for a longer range plays exactly what a
    /// replay that reads its own range plays: the records before and after the
    /// range are left out.
    #[test]
    fn a_replay_of_records_read_for_a_longer_range_is_the_same() {
        let (_dir, history, rates, range) = setup();
        let before = range.from - Duration::days(3);
        let after = range.to + Duration::days(2);
        for (at, percent) in [(before, 70.0), (after, 90.0)] {
            history
                .append(&Sample {
                    t: at,
                    account: account_ref("AC"),
                    src: Source::Poll,
                    l: vec![LimitSample {
                        k: LimitKind::WeeklyAll,
                        m: None,
                        p: percent,
                        r: Some(at + Duration::days(1)),
                        b: false,
                    }],
                })
                .expect("a sample outside the range");
        }
        history
            .append_many(&tokens(
                &inputs().rhythm,
                &Range {
                    from: before,
                    to: range.from,
                },
                offset(),
            ))
            .expect("token records before the range");
        let longer = Range {
            from: before,
            to: after + Duration::hours(1),
        };
        let samples = history
            .read_claude::<Sample>(&longer, &Hosts::All)
            .expect("the samples");
        let records = history
            .read::<TokenRecord>(&longer, &Hosts::All)
            .expect("the token records");

        for without in [vec![], vec![account_ref("AC")]] {
            let own = replay(&history, &range, &rates, &without, Policy::Lazy, &prices())
                .expect("replay");
            let shared = replay_records(
                &samples.records,
                &records.records,
                &range,
                &rates,
                &without,
                Policy::Lazy,
                &prices(),
            )
            .expect("replay of the shared records");
            assert_eq!(shared, own);
        }
    }

    #[test]
    fn provider_records_do_not_change_the_replay() {
        let (_dir, history, rates, range) = setup();
        let before =
            replay(&history, &range, &rates, &[], Policy::Lazy, &prices()).expect("the replay");

        append_codex_records(&history, range.from + Duration::hours(1));
        let after =
            replay(&history, &range, &rates, &[], Policy::Lazy, &prices()).expect("the replay");

        assert_eq!(after, before, "a Codex sample changed the Claude replay");
    }

    /// R7 of 1.3: Codex and Ollama work uses no Claude account, so the replay
    /// plays only Claude work, even when the other company's model has a
    /// price.
    #[test]
    fn replay_counts_only_anthropic_work() {
        let (_dir, history, rates, range) = setup();
        let before =
            replay(&history, &range, &rates, &[], Policy::Lazy, &prices()).expect("the replay");

        let mut others = Vec::new();
        for (model, hour) in [("deepseek-v4.1-flash:cloud", 3), ("gpt-6.1-sol", 4)] {
            others.push(TokenRecord {
                t: range.from + Duration::hours(hour),
                minutes: 60,
                session: "5d2f8a1c-3e7b-4c9d-a0f6-8b1e4d7c2a95".to_string(),
                cwd: "/home/sam/projects/ledger".to_string(),
                project: ProjectKey::Repo("github.com/sam/ledger".to_string()),
                model: model.to_string(),
                input: 40_000_000,
                out: 2_000_000,
                cw: 0,
                cr: 0,
                answers: 12,
                agent_answers: 0,
            });
        }
        history
            .append_many(&others)
            .expect("write the other companies' records");
        let after =
            replay(&history, &range, &rates, &[], Policy::Lazy, &prices()).expect("the replay");

        assert_eq!(after, before, "another company's work changed the replay");
        assert!(
            (after.worked_usd + after.blocked_usd - 92.0).abs() < EPS,
            "{after:?}"
        );
    }

    /// Work of a Claude model without a price is left out of the replay's
    /// recorded hours: never valued as 0 in an hour, never guessed.
    #[test]
    fn replay_leaves_out_work_without_a_price() {
        let (_dir, history, rates, range) = setup();
        let before =
            replay(&history, &range, &rates, &[], Policy::Lazy, &prices()).expect("the replay");

        history
            .append(&TokenRecord {
                t: range.from + Duration::hours(3),
                minutes: 60,
                session: "5d2f8a1c-3e7b-4c9d-a0f6-8b1e4d7c2a95".to_string(),
                cwd: "/home/sam/projects/ledger".to_string(),
                project: ProjectKey::Repo("github.com/sam/ledger".to_string()),
                model: "claude-new-9".to_string(),
                input: 40_000_000,
                out: 2_000_000,
                cw: 0,
                cr: 0,
                answers: 12,
                agent_answers: 0,
            })
            .expect("write the unpriced record");
        let after =
            replay(&history, &range, &rates, &[], Policy::Lazy, &prices()).expect("the replay");

        assert_eq!(after, before);
    }
}
