// The statistics Weeks page's fixture: `stats_weeks`, `stats_week_summary`
// and `stats_window_detail`.

import { hoursText } from "../common.js";
import {
  ACCOUNTS,
  API_PER_M,
  BLOCKED,
  DAY,
  HOUR,
  NOW_MS,
  WEEKS,
  SW_AUTO,
  accountOut,
  chosenAccounts,
  fixtureMoney,
  fixtureValues,
  fixtureWholePercent,
  mockWeek,
  resetEnd,
  round1,
  sum,
  switchesOf,
  tokenCounts,
  topProjects,
  usedMoney,
  weekAccounts,
  weekExpiredMoney,
  weekExpiredPercent,
  weekIso,
  weekTime,
  weeklyPrice,
  windowsOfWeek,
} from "./stats-frame.js";

function recentCurve(account) {
  const start = NOW_MS - 14 * DAY;
  const points = new Map();
  for (let index = 0; index <= WEEKS.length; index += 1) {
    if (account.used[index] === undefined && index < ACCOUNTS[0].used.length) continue;
    if (account.used[index] === null) {
      // A week before the account existed: no window, so no point.
      const end = Date.parse(resetEnd(account, index));
      const windowStart = end - 7 * DAY;
      if (windowStart > NOW_MS || end < start) continue;
      continue;
    }
    const end = Date.parse(resetEnd(account, index));
    const windowStart = end - 7 * DAY;
    if (windowStart > NOW_MS || end < start) continue;
    const running = account.used[index] === undefined;
    const final = running ? account.now : account.used[index];
    const span = running ? NOW_MS - windowStart : 7 * DAY;
    const from = Math.max(windowStart, start);
    const to = Math.min(end, NOW_MS);
    for (let time = from; time <= to; time += 4 * HOUR) {
      const value = Math.min(100, final * ((time - windowStart) / span));
      points.set(time, round1(value));
    }
    if (to === NOW_MS) {
      const value = Math.min(100, final * ((to - windowStart) / span));
      points.set(to, round1(value));
    }
  }
  return [...points.entries()]
    .sort((a, b) => a[0] - b[0])
    .map(([time, value]) => [new Date(time).toISOString(), value]);
}

// The calendar week that holds `time`, as an index of the fixture's weeks.
function weekIndexOf(time) {
  let index = 0;
  while (weekTime(index + 1) <= time) index += 1;
  return index;
}

// The week the running window counts in: the week of its middle, half a week
// before its reset.
function nowWeek(account) {
  const running = weekIndexOf(NOW_MS);
  const end = Date.parse(resetEnd(account, running));
  return new Date(weekTime(weekIndexOf(end - 3.5 * DAY))).toISOString();
}

// Whether the fixture's week `index` holds its now: the running week.
export function holdsNow(index) {
  return weekTime(index) <= NOW_MS && NOW_MS < weekTime(index + 1);
}

function weekRow(account, indexes) {
  return {
    id: account.id,
    plan: account.plan,
    cells: indexes.map((index) => {
      const windows = windowsOfWeek(account, index);
      if (windows.length === 0) return null;
      const used = sum(windows.map((window) => window.used));
      return {
        end: windows[windows.length - 1].end,
        used,
        parts: windows.map((window) => window.used),
        windowEnds: windows.map((window) => window.end),
        expired: weekExpiredPercent(account, index),
        expiredMoney: weekExpiredMoney(account, index),
        full: used >= 100,
        uncertain: false,
      };
    }),
    nowPercent: account.now,
    nowWeek: nowWeek(account),
  };
}

// A provider subscription's weekly windows: OpenAI's and Google's `weekly_all`
// week. Ollama counts per month, so it has no week row of this grid.
function providerWeekRows(chosenSubscriptions, indexes) {
  return chosenSubscriptions
    .filter((subscription) => subscription.company !== "ollama")
    .map((subscription) => weekRow(subscription, indexes));
}

export function weeks(query) {
  const { indexes, chosen, chosenSubscriptions } = chosenAccounts(query);
  const rows = chosen.map((account) => weekRow(account, indexes));
  rows.push(...providerWeekRows(chosenSubscriptions, indexes));
  const recent = chosen.map((account) => ({ id: account.id, points: recentCurve(account) }));
  for (const subscription of chosenSubscriptions) {
    if (subscription.company === "ollama") continue;
    recent.push({ id: subscription.id, points: recentCurve(subscription) });
  }
  return {
    weekStarts: indexes.map(weekIso),
    // The column that holds the fixture's now; its week has no finished window.
    running: indexes.map(holdsNow),
    rows,
    blocked: indexes.map((index) => BLOCKED[index]),
    recent,
    now: new Date(NOW_MS).toISOString(),
  };
}

export function usedPercentTotal(account, indexes) {
  return sum(indexes.map((index) => account.used[index] ?? 0));
}

// The summary of the week that holds the fixture's now, after its last
// finished week: the running windows' use so far, nothing expired yet.
function runningSummary(chosen, week) {
  const used = sum(chosen.map((account) => (account.now / 100) * weeklyPrice(account)));
  const offered = sum(chosen.map((account) => weeklyPrice(account)));
  return {
    week,
    usedPercentOfPaid: offered > 0 ? (used / offered) * 100 : null,
    used: offered > 0 ? used : null,
    expired: offered > 0 ? 0 : null,
    moneyMissing: offered > 0 ? null : "no priced weekly window in this week",
    running: true,
    blockedHours: 0,
    switches: { user: 0, cli: 0, auto: 0, agent: 0, external: 0 },
    switchesTotal: 0,
    perAccountPercent: chosen.map((account) => [account.id, account.now]),
    expiredPercentOfPaid: null,
    topProjects: [],
    tokens: tokenCounts(0),
    values: fixtureValues(0, null),
    usedChangePercent: null,
    conclusion: ["No hour blocked."],
  };
}

export function weekSummary(query, week) {
  const { indexes, chosen } = chosenAccounts(query);
  const index = indexes.find((candidate) => weekIso(candidate) === week);
  if (index === undefined && holdsNow(weekIndexOf(Date.parse(week)))) {
    return runningSummary(chosen, week);
  }
  if (index === undefined) throw new Error(`week ${JSON.stringify(week)} is not in the chosen range`);
  const accounts = weekAccounts(index, chosen);
  const priced = chosen.filter((account) => account.used[index] !== null);
  const used = sum(priced.map((account) => usedMoney(account, index)));
  const expired = sum(priced.map((account) => weekExpiredMoney(account, index)));
  // Every window the week offered offers one week's price, as Overview counts.
  const offered = sum(
    priced.map((account) => weeklyPrice(account) * windowsOfWeek(account, index).length)
  );
  const switches = switchesOf([index]);
  const out = sum(chosen.map((account) => accountOut(account, index)));
  const tokens = tokenCounts(out);
  const running = holdsNow(index);
  const previous = index - 1;
  let usedChangePercent = null;
  if (!running && previous >= 0 && chosen.length > 0) {
    const before = sum(
      chosen
        .filter((account) => account.used[previous] !== null)
        .map((account) => usedMoney(account, previous))
    );
    if (before > 0) usedChangePercent = ((used - before) / before) * 100;
  }
  const conclusion = [];
  conclusion.push(
    switchesOf([index]) && BLOCKED[index] > 0
      ? `${hoursText(BLOCKED[index])} blocked.`
      : "No hour blocked."
  );
  let closest = null;
  for (const account of chosen) {
    if (account.used[index] === null) continue;
    const expiredPercent = weekExpiredPercent(account, index);
    if (closest === null || expiredPercent > closest.expiredPercent) {
      closest = { account, expiredPercent };
    }
  }
  if (closest !== null) {
    if (closest.expiredPercent < 10) {
      conclusion.push(
        `${closest.account.abbr} came closest to expiring unused: ${fixtureWholePercent(
          closest.expiredPercent
        )} left at its reset.`
      );
    } else {
      conclusion.push(
        `${fixtureWholePercent(closest.expiredPercent)} of ${closest.account.abbr}'s week expired, ${fixtureMoney(
          weekExpiredMoney(closest.account, index),
          "EUR"
        )}.`
      );
    }
  }
  if (SW_AUTO[index] > 0) {
    const minutes = index === mockWeek(11) ? 38 : index === mockWeek(10) ? 19 : SW_AUTO[index];
    const count = SW_AUTO[index];
    conclusion.push(
      `Auto-burn held back ${count} planned ${count === 1 ? "switch" : "switches"}, ${minutes} ${
        minutes === 1 ? "minute" : "minutes"
      } in total.`
    );
  }
  return {
    week: weekIso(index),
    usedPercentOfPaid: offered > 0 ? (used / offered) * 100 : null,
    used: offered > 0 ? used : null,
    expired: offered > 0 ? expired : null,
    moneyMissing: offered > 0 ? null : "no priced weekly window in this week",
    running,
    blockedHours: BLOCKED[index],
    switches,
    switchesTotal:
      switches.user + switches.cli + switches.auto + switches.agent + switches.external,
    perAccountPercent: accounts.map((account) => [account.id, account.usedPercent]),
    expiredPercentOfPaid: !running && offered > 0 ? (expired / offered) * 100 : null,
    topProjects: topProjects(index, used, true),
    tokens,
    values: fixtureValues(out > 0 ? (out / 1000000) * API_PER_M : null, null),
    usedChangePercent,
    conclusion,
  };
}

function windowCurve(window, reachedAt) {
  const start = Date.parse(window.start);
  const end = Date.parse(window.end);
  const points = [];
  for (let time = start; time <= end; time += 4 * HOUR) {
    let value;
    if (reachedAt !== null && time >= reachedAt) value = 100;
    else value = Math.min(100, window.used * ((time - start) / (end - start)));
    points.push([new Date(time).toISOString(), round1(value)]);
  }
  if (reachedAt !== null) points.push([new Date(reachedAt).toISOString(), 100]);
  points.sort((a, b) => Date.parse(a[0]) - Date.parse(b[0]));
  return points;
}

// The mock-up's notes for the weeks its detail card shows; every other week
// gets the note its numbers state.
const NOTES = {
  [`LB:${mockWeek(11)}`]: "Second week with auto-burn. 65 % expired at the reset on Monday 17:00.",
  [`AC:${mockWeek(8)}`]:
    "51 % expired. All work that week went to PR and LB; AC was opened on two days only.",
  [`PR:${mockWeek(5)}`]: "First week of this subscription.",
  [`LB:${mockWeek(1)}`]: "AC was full that week too: 14h with no usable account.",
};

function noteFor(account, index, window) {
  if (window.reset) {
    return "The week's limit was given back before its reset, so the use before it stays counted.";
  }
  const note = NOTES[`${account.abbr}:${index}`];
  if (note !== undefined) return note;
  return window.expired > 0
    ? `${fixtureWholePercent(window.expired)} expired at its reset.`
    : "Nothing expired at its reset.";
}

export function windowDetail(query, accountId, end) {
  const { indexes, chosen, chosenSubscriptions } = chosenAccounts(query);
  const account =
    chosen.find((candidate) => candidate.id === accountId) ||
    chosenSubscriptions.find((candidate) => candidate.id === accountId);
  if (!account) throw new Error(`unknown account ${JSON.stringify(accountId)}`);
  let found = null;
  for (const index of indexes) {
    for (const window of windowsOfWeek(account, index)) {
      if (Date.parse(window.end) === Date.parse(end)) found = { index, window };
    }
  }
  if (found === null) {
    throw new Error(`no window of ${account.abbr} ends at ${JSON.stringify(end)}`);
  }
  const { index, window } = found;
  const price = weeklyPrice(account);
  const used = window.used;
  let reached100At = null;
  if (used >= 100) {
    reached100At = new Date(
      Math.min(Date.parse(window.end), Date.parse(window.start) + 4 * DAY + 10 * HOUR)
    ).toISOString();
  }
  const switches = switchesOf([index]);
  // A typical week holds 24 windows; LB's 26 Oct week is the demo of a week
  // that used more windows (12) than its usual working hours hold (8).
  const possibleSessionWindows =
    account.abbr === "PR" ? null : account.abbr === "LB" && index === mockWeek(10) ? 8 : 24;
  const sessionWindowsUsed =
    possibleSessionWindows === null
      ? null
      : account.abbr === "LB" && index === mockWeek(10)
        ? 12
        : used > 0
          ? Math.round(used / (account.abbr === "AC" ? 8 : 10))
          : 0;
  return {
    id: account.id,
    start: window.start,
    end: window.end,
    plan: account.plan,
    weeklyPrice: price,
    used,
    usedMoney: (used / 100) * price,
    expired: window.expired,
    expiredMoney: (window.expired / 100) * price,
    reached100At,
    blockedHoursAfter: reached100At !== null ? BLOCKED[index] : 0,
    sessionWindowsUsed,
    possibleSessionWindows,
    usedMoreThanPossible:
      possibleSessionWindows !== null && sessionWindowsUsed > possibleSessionWindows,
    note: noteFor(account, index, window),
    switchesInOut: switches.user + switches.cli + switches.auto + switches.agent + switches.external,
    switchesAuto: switches.auto,
    lastSampleBeforeEndSecs: 120,
    uncertain: false,
    curve: windowCurve(window, reached100At === null ? null : Date.parse(reached100At)),
  };
}
