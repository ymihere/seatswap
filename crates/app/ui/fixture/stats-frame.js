// The statistics fixture's shared frame: weeks, accounts, ids, money and
// range helpers, the findings, `statsEmpty` and `answer`.

import { hoursText } from "../common.js";
import { ensureFixture } from "../fixture.js";

export const DAY = 86400000;
export const HOUR = 3600000;

// The snapshot's view order; the app's Rust commands walk Ctx::accounts, so
// the pages see this order too.
const VIEW_ABBRS = ["PR", "LB", "AC"];

// The frame's made-up answer: the first week of the fixture's data (17 Aug) and
// its history size in bytes (4.6 MB, one decimal).
export const HISTORY_START = "2026-08-17T08:14:00Z";
export const HISTORY_BYTES = 4_812_288;

// `?fixture=stats-fresh`: a new install's frame, a history with no reading and
// no switch yet. The Statistics area shows its one "No statistics yet" note.
function statsFresh() {
  return (
    typeof location !== "undefined" &&
    new URLSearchParams(location.search).get("fixture") === "stats-fresh"
  );
}

// `?fixture=stats-year`: a year of history. 41 made-up weeks come before the
// mock-up's twelve, so the ranges "12 mo" and "All" show 52 and 53 weeks (the
// charts' check at many columns). The mock-up's weeks keep their numbers:
// `mockWeek(n)` is the index of its week n (0 = 17 Aug, 11 = 2 Nov).
function statsYear() {
  return (
    typeof location !== "undefined" &&
    new URLSearchParams(location.search).get("fixture") === "stats-year"
  );
}

const YEAR_WEEKS = statsYear() ? 41 : 0;

export function mockWeek(n) {
  return YEAR_WEEKS + n;
}

// The made-up weeks' values, oldest first: `valueOf(k)` for k = 0..40.
function yearValues(valueOf) {
  return Array.from({ length: YEAR_WEEKS }, (_, k) => valueOf(k));
}

export function statsFrame() {
  if (statsFresh()) return { hosts: ["atlas"], historyStart: null, historyBytes: 0 };
  return {
    hosts: ["atlas", "vega"],
    historyStart: YEAR_WEEKS > 0 ? `${WEEKS[0]}T08:14:00Z` : HISTORY_START,
    historyBytes: HISTORY_BYTES,
  };
}

const MOCK_WEEKS = [
  "2026-08-17",
  "2026-08-24",
  "2026-08-31",
  "2026-09-07",
  "2026-09-14",
  "2026-09-21",
  "2026-09-28",
  "2026-10-05",
  "2026-10-12",
  "2026-10-19",
  "2026-10-26",
  "2026-11-02",
];

export const WEEKS = [
  ...yearValues((k) =>
    new Date(Date.parse(`${MOCK_WEEKS[0]}T00:00:00.000Z`) - (YEAR_WEEKS - k) * 7 * DAY)
      .toISOString()
      .slice(0, 10)
  ),
  ...MOCK_WEEKS,
];

// data.py's ACC, in the mock-up's ORDER (LB, AC, PR). The id is the built-in
// snapshot's view id for the abbreviation, filled in before an answer is built.
export const ACCOUNTS = [
  {
    id: null,
    abbr: "LB",
    label: "Lab",
    plan: "Max",
    price: 180,
    used: [
      ...yearValues((k) => Math.min(100, 64 + ((k * 37) % 43))),
      100, 100, 96, 100, 100, 88, 74, 81, 69, 92, 94, 97,
    ],
    now: 40,
    resetDay: 7,
    resetHour: 17,
  },
  {
    id: null,
    abbr: "AC",
    label: "Acme",
    plan: "Team Premium",
    price: 140,
    used: [
      ...yearValues((k) => Math.min(100, 52 + ((k * 23) % 53))),
      91, 100, 100, 97, 100, 72, 58, 64, 49, 71, 91, 93,
    ],
    now: 48,
    resetDay: 7,
    resetHour: 2,
  },
  {
    id: null,
    abbr: "PR",
    label: "Private",
    plan: "Ultra",
    price: 360,
    used: [
      ...yearValues(() => null),
      null, null, null, null, null, 61, 83, 77, 90, 68, 88, 90,
    ],
    now: 70,
    resetDay: 5,
    resetHour: 19,
  },
];

// The weeks of a subscription that has no row in a given week (one per entry
// of WEEKS), so a provider's array stays as long as the accounts'.
const EMPTY_WEEKS = [...yearValues(() => null), ...Array(12).fill(null)];

// The provider subscriptions the built-in snapshot stores: OA's weekly window
// and OL's monthly one are the page's made-up rows (OA one finished week and a
// running window; O2, the second Codex login of the pop-up's fixture, only a
// running window and no price; OL has no weekly window, only its monthly credits;
// GO one finished week and a running window, whose work is not recorded).
export const SUBSCRIPTIONS = [
  {
    id: null,
    abbr: "OA",
    label: "OpenAI",
    plan: "Plus",
    color: "#db2777",
    price: 23,
    company: "openai",
    used: [...EMPTY_WEEKS.slice(0, -1), 46],
    now: 61,
    resetDay: 5,
    resetHour: 19,
    monthEnd: null,
  },
  {
    id: null,
    abbr: "O2",
    label: "OpenAI",
    plan: "Pro",
    color: "#db2777",
    price: null,
    company: "openai",
    used: EMPTY_WEEKS,
    now: 20,
    resetDay: 2,
    resetHour: 16,
    monthEnd: null,
  },
  {
    id: null,
    abbr: "OL",
    label: "Ollama",
    plan: null,
    color: "#ea580c",
    price: null,
    company: "ollama",
    used: EMPTY_WEEKS,
    now: 34,
    resetDay: null,
    resetHour: null,
    monthEnd: "2026-12-03T00:00:00.000Z",
  },
  {
    id: null,
    abbr: "GO",
    label: "Google",
    plan: "Google AI Pro",
    color: "#0d9488",
    price: 21.99,
    company: "google",
    used: [...EMPTY_WEEKS.slice(0, -1), 40],
    now: 25,
    resetDay: 1,
    resetHour: 8,
    monthEnd: null,
  },
];

// Each answer names the built-in snapshot's accounts: their ids come from its
// stored rows by abbreviation, never from ids of the fixture's own. A provider
// subscription's id is its `account.acc`, as the snapshot's rows carry it.
export function resolveIds() {
  const fixture = ensureFixture();
  if (!fixture || !Array.isArray(fixture.accounts)) {
    throw new Error("the built-in snapshot has no account rows");
  }
  if (!Array.isArray(fixture.providerSubscriptions)) {
    throw new Error("the built-in snapshot has no provider subscriptions");
  }
  for (const account of ACCOUNTS) {
    const row = fixture.accounts.find(
      (candidate) => candidate && candidate.abbr === account.abbr
    );
    if (!row || typeof row.id !== "string" || row.id === "") {
      throw new Error(`the built-in snapshot has no account ${JSON.stringify(account.abbr)}`);
    }
    account.id = row.id;
  }
  for (const subscription of SUBSCRIPTIONS) {
    const row = fixture.providerSubscriptions.find(
      (candidate) => candidate && candidate.abbr === subscription.abbr
    );
    if (
      !row ||
      !row.account ||
      typeof row.account.acc !== "string" ||
      row.account.acc === ""
    ) {
      throw new Error(
        `the built-in snapshot has no provider subscription ${JSON.stringify(subscription.abbr)}`
      );
    }
    subscription.id = row.account.acc;
  }
}

export function viewOrder() {
  return VIEW_ABBRS.map((abbr) => {
    const account = byAbbr(abbr);
    if (!account) throw new Error(`unknown account abbreviation ${JSON.stringify(abbr)}`);
    return account.id;
  });
}

export const BLOCKED = [
  ...yearValues((k) => [0, 3, 0, 9, 14, 1, 0, 27, 6, 0, 0.5][k % 11]),
  11, 14, 8, 12, 9, 1, 0, 0.5, 0, 1, 0.5, 0,
];
const SW_YOU = [...yearValues((k) => 5 + (k % 6)), 9, 11, 8, 10, 9, 6, 5, 6, 4, 6, 2, 1];
export const SW_AUTO = [...yearValues(() => 0), 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 19, 22];
const SW_AGENT = [...yearValues(() => 0), 0, 0, 0, 0, 0, 0, 1, 0, 2, 0, 0, 0];

// Output tokens (millions) of all accounts together, data.py's 68.9 in week 11
// and 44.8 in week 8.
export const OUT_M = [
  ...yearValues((k) => 40 + ((k * 13) % 29)),
  58.0, 61.5, 55.2, 66.4, 64.1, 70.8, 72.6, 79.4, 44.8, 88.0, 110.3, 68.9,
];
export const TOK_TOTAL = 840.0;
export const API_TOTAL = 17100;
export const API_PER_M = API_TOTAL / TOK_TOTAL;
export const NOW_MS = Date.parse("2026-11-12T10:00:00Z");

// The mock-up's tokens per percent of a finished account week (data.py: LB 1.9 M,
// AC 1.4 M, PR 3.8 M).
export const TOKENS_PER_PERCENT = { LB: 1900000, AC: 1400000, PR: 3800000 };

export const ORG_ACC = {
  acme: { LB: 70, AC: 258, PR: 141 },
  sam: { LB: 369, AC: 55, PR: 304 },
  Unassigned: { LB: 14, AC: 6, PR: 17 },
};

export const TREE = [
  { level: 0, name: "acme", share: 38, split: [15, 55, 30], sessions: 253, key: null },
  { level: 1, name: "acme-kb", share: 31, split: [14, 58, 28], sessions: 212, key: ["repo", "github.com/acme/acme-kb"] },
  { level: 2, name: "ingest", share: 19, split: [14, 58, 28], sessions: 131, key: null },
  { level: 2, name: "web", share: 9, split: [14, 58, 28], sessions: 64, key: null },
  { level: 2, name: "(top folder)", share: 3, split: [14, 58, 28], sessions: 17, key: null },
  { level: 1, name: "acme-notes", share: 7, split: [19, 42, 39], sessions: 41, key: ["repo", "github.com/acme/acme-notes"] },
  { level: 0, name: "sam", share: 59, split: [51, 7, 42], sessions: 428, key: null },
  { level: 1, name: "atlas", share: 34, split: [55, 5, 40], sessions: 268, key: ["repo", "github.com/sam/atlas"] },
  { level: 2, name: "engine", share: 21, split: [55, 5, 40], sessions: 160, key: null },
  { level: 2, name: "frontend", share: 11, split: [55, 5, 40], sessions: 91, key: null },
  { level: 2, name: "(top folder)", share: 2, split: [55, 5, 40], sessions: 17, key: null },
  { level: 1, name: "ledger", share: 17, split: [44, 10, 46], sessions: 97, key: ["repo", "github.com/sam/ledger"] },
  { level: 1, name: "notes", share: 8, split: [48, 10, 42], sessions: 63, key: ["repo", "github.com/sam/notes"] },
  { level: 0, name: "Unassigned", share: 3, split: [38, 16, 46], sessions: 22, key: null },
  { level: 1, name: "~ (home folder)", share: 3, split: [38, 16, 46], sessions: 22, key: ["folder", "~"] },
];

export const ORG_ACCOUNTS = { acme: ["AC"], sam: ["LB", "PR"] };

export function accountById(id) {
  return ACCOUNTS.find((account) => account.id === id) || null;
}

export function byAbbr(abbr) {
  return ACCOUNTS.find((account) => account.abbr === abbr) || null;
}

export function sum(values) {
  let total = 0;
  for (const value of values) {
    if (typeof value === "number" && Number.isFinite(value)) total += value;
  }
  return total;
}

export function round1(value) {
  return Math.round(value * 10) / 10;
}

export function fixtureMoney(value, currency) {
  const rounded = Math.round(value * 100) / 100;
  if (rounded < 10) return `${rounded.toFixed(2)} ${currency}`;
  return `${Math.round(rounded).toLocaleString("en-US")} ${currency}`;
}

export function fixtureWholePercent(value) {
  return `${Math.round(value)} %`;
}

export function joinNames(names) {
  if (names.length === 0) return "";
  if (names.length === 1) return names[0];
  if (names.length === 2) return `${names[0]} and ${names[1]}`;
  return `${names.slice(0, -1).join(", ")} and ${names[names.length - 1]}`;
}

export function weekIso(index) {
  return `${WEEKS[index]}T00:00:00.000Z`;
}

export function weekTime(index) {
  if (index < WEEKS.length) return Date.parse(weekIso(index));
  // The running week after the last finished one.
  return weekTime(WEEKS.length - 1) + (index - (WEEKS.length - 1)) * 7 * DAY;
}

export function weeklyPrice(account) {
  return (account.price * 12) / 52;
}

export function usedMoney(account, index) {
  const used = account.used[index];
  if (used === null) return 0;
  return (used / 100) * weeklyPrice(account);
}

// The windows of one account week, oldest first, in the shape the history
// writes them: a reset window ends at the lower reading and counts nothing as
// expired; every other window's `lost` is its expired share. One made-up week
// has a reset: LB's 2 Nov week is 62 % then 35 % from 0, 97 % in total and 65 %
// expired (the reset window's own share is nothing). LB's 26 Oct week holds a
// window that used nothing, so the pages must not draw a 1 px part for it while
// its total and tooltip still count it.
const WEEK_WINDOWS = {
  [`LB:${mockWeek(11)}`]: [
    {
      used: 62,
      start: "2026-11-02T17:00:00.000Z",
      end: "2026-11-06T17:00:00.000Z",
      expired: 0,
      reset: true,
    },
    {
      used: 35,
      start: "2026-11-06T17:00:00.000Z",
      end: "2026-11-09T17:00:00.000Z",
      expired: 65,
      reset: false,
    },
  ],
  [`LB:${mockWeek(10)}`]: [
    {
      used: 94,
      start: "2026-10-26T17:00:00.000Z",
      end: "2026-10-30T17:00:00.000Z",
      expired: 6,
      reset: false,
    },
    {
      used: 0,
      start: "2026-10-30T17:00:00.000Z",
      end: "2026-11-02T17:00:00.000Z",
      expired: 0,
      reset: false,
    },
  ],
};

export function windowsOfWeek(account, index) {
  const used = account.used[index];
  if (used === null) return [];
  const end = resetEnd(account, index);
  const special = WEEK_WINDOWS[`${account.abbr}:${index}`];
  if (special !== undefined) return special;
  return [
    {
      used,
      start: new Date(Date.parse(end) - 7 * DAY).toISOString(),
      end,
      expired: 100 - used,
      reset: false,
    },
  ];
}

export function weekExpiredPercent(account, index) {
  return sum(windowsOfWeek(account, index).map((window) => window.expired));
}

export function weekExpiredMoney(account, index) {
  return sum(
    windowsOfWeek(account, index).map((window) => (window.expired / 100) * weeklyPrice(account))
  );
}

export function expiredMoney(account, index) {
  return weekExpiredMoney(account, index);
}

export function resetEnd(account, index) {
  return new Date(
    weekTime(index) + account.resetDay * DAY + account.resetHour * HOUR
  ).toISOString();
}

function rangeIndexes(range) {
  const all = WEEKS.map((_, index) => index);
  switch (range) {
    case "thisWeek":
      return [mockWeek(11)];
    case "lastWeek":
      return [mockWeek(10)];
    case "days30":
      return [7, 8, 9, 10, 11].map(mockWeek);
    case "days90":
      return all.slice(-13);
    case "months12":
      return all.slice(-52);
    case "all":
      return all;
    default:
      throw new Error(`unknown range ${JSON.stringify(range)}`);
  }
}

export function subscriptionById(id) {
  return SUBSCRIPTIONS.find((subscription) => subscription.id === id) || null;
}

// The chosen Claude accounts and provider subscriptions. An empty
// `query.accounts` is every one of both lists; an id names either an account
// or a provider subscription (a provider week's id is its `account.acc`).
export function chosenAccounts(query) {
  if (!query || typeof query !== "object") throw new Error("a statistics query is missing");
  const indexes = rangeIndexes(query.range);
  if (!Array.isArray(query.accounts)) throw new Error("query.accounts must be a list");
  const chosen = [];
  const subscriptions = [];
  if (query.accounts.length === 0) {
    chosen.push(...viewOrder().map(accountById));
    subscriptions.push(...SUBSCRIPTIONS);
  } else {
    for (const id of query.accounts) {
      const account = accountById(id);
      if (account) {
        if (!chosen.includes(account)) chosen.push(account);
        continue;
      }
      const subscription = subscriptionById(id);
      if (!subscription) throw new Error(`unknown account ${JSON.stringify(id)}`);
      if (!subscriptions.includes(subscription)) subscriptions.push(subscription);
    }
  }
  return { indexes, chosen, chosenSubscriptions: subscriptions };
}

function totalUsedMoney(index) {
  return sum(
    ACCOUNTS.filter((account) => account.used[index] !== null).map((account) =>
      usedMoney(account, index)
    )
  );
}

export function accountOut(account, index) {
  const used = account.used[index];
  if (used === null) return 0;
  const total = totalUsedMoney(index);
  if (total <= 0) return 0;
  return OUT_M[index] * 1000000 * (usedMoney(account, index) / total);
}

export function tokenCounts(out) {
  return {
    input: Math.round(out * 0.15),
    out: Math.round(out),
    cw: Math.round(out * 0.2),
    cr: Math.round(out * 3.2),
  };
}

export function switchesOf(indexes) {
  return {
    user: Math.round(sum(indexes.map((index) => SW_YOU[index]))),
    cli: 0,
    auto: Math.round(sum(indexes.map((index) => SW_AUTO[index]))),
    agent: Math.round(sum(indexes.map((index) => SW_AGENT[index]))),
    external: 0,
  };
}

function projectShares(index) {
  const atlas = 30 + ((index * 7) % 11);
  const acmeKb = index === 8 ? 21 : 34 - ((index * 5) % 9);
  const ledger = 13 + ((index * 3) % 8);
  const other = 100 - atlas - acmeKb - ledger;
  return [
    ["atlas", atlas],
    ["acme-kb", acmeKb],
    ["ledger", ledger],
    ["other", other],
  ];
}

export function topProjects(index, usedTotal, asPercent) {
  const shares = projectShares(index);
  const named = shares.slice(0, 3);
  named.sort((a, b) => b[1] - a[1]);
  const other = shares[3][1];
  const rows = named.map(([name, share]) => [
    name,
    asPercent ? share : (share / 100) * usedTotal,
  ]);
  rows.push(["other", asPercent ? other : (other / 100) * usedTotal]);
  return rows;
}

export function weekAccounts(index, chosen) {
  return chosen
    .filter((account) => account.used[index] !== null)
    .map((account) => ({
      id: account.id,
      usedPercent: account.used[index],
      used: usedMoney(account, index),
      expiredPercent: weekExpiredPercent(account, index),
      expired: weekExpiredMoney(account, index),
      uncertain: false,
    }));
}

export function expiredByAccount(indexes, chosen) {
  const rows = chosen
    .map((account) => [
      account.id,
      sum(indexes.map((index) => expiredMoney(account, index))),
    ])
    .filter(([, value]) => value > 0);
  rows.sort((a, b) => b[1] - a[1] || a[0].localeCompare(b[0]));
  return rows;
}

export function marksOf(indexes) {
  const first = weekTime(indexes[0]);
  const end = weekTime(indexes[indexes.length - 1]) + 7 * DAY;
  // The year adds marks in neighbouring weeks, whose labels would touch.
  const year =
    YEAR_WEEKS > 0
      ? [
          { at: `${WEEKS[0]}T00:00:00.000Z`, label: "LB, AC added" },
          { at: "2026-03-02T00:00:00.000Z", label: "auto-burn on" },
          { at: "2026-03-09T00:00:00.000Z", label: "auto-burn off" },
          { at: "2026-03-16T00:00:00.000Z", label: "auto-burn on" },
          { at: "2026-09-14T00:00:00.000Z", label: "auto-burn off" },
        ]
      : [];
  return [
    ...year,
    { at: "2026-09-21T00:00:00.000Z", label: "PR added" },
    { at: "2026-10-26T00:00:00.000Z", label: "auto-burn on" },
  ].filter((mark) => {
    const at = Date.parse(mark.at);
    return at >= first && at < end;
  });
}

export function findingExpired(indexes, chosen, currency) {
  const rows = expiredByAccount(indexes, chosen);
  if (rows.length === 0) return null;
  const [id, amount] = rows[0];
  if (amount <= 0) return null;
  const account = accountById(id);
  if (!account) return null;
  const weeks = indexes.filter((index) => account.used[index] !== null).length;
  const noun = weeks === 1 ? "week" : "weeks";
  return `${account.abbr} is where most money expired: ${fixtureMoney(amount, currency)} in ${weeks} ${noun}.`;
}

function weekMoney(week, chosen) {
  let used = 0;
  let expired = 0;
  let paid = 0;
  for (const account of chosen) {
    if (account.used[week] === null) continue;
    used += usedMoney(account, week);
    expired += weekExpiredMoney(account, week);
    paid += weeklyPrice(account) * windowsOfWeek(account, week).length;
  }
  return { used, expired, paid };
}

export function findingBlocked(indexes, blockedHours, avoidableHours) {
  if (blockedHours <= 0 || indexes.length === 0) return null;
  const noun = indexes.length === 1 ? "week" : "weeks";
  return `You were blocked ${hoursText(blockedHours)} in ${indexes.length} ${noun}; ${hoursText(avoidableHours)} of them while other quota expired.`;
}

export function findingAutoBurn(indexes, chosen) {
  const at = Date.parse("2026-10-26T00:00:00.000Z");
  const first = weekTime(indexes[0]);
  const end = weekTime(indexes[indexes.length - 1]) + 7 * DAY;
  if (at < first || at >= end) return null;
  const before = indexes.filter((index) => weekTime(index) + 7 * DAY <= at);
  const after = indexes.filter((index) => weekTime(index) >= at);
  if (before.length === 0 || after.length === 0) return null;
  let paidBefore = 0;
  let expiredBefore = 0;
  for (const index of before) {
    const money = weekMoney(index, chosen);
    paidBefore += money.paid;
    expiredBefore += money.expired;
  }
  let paidAfter = 0;
  let expiredAfter = 0;
  for (const index of after) {
    const money = weekMoney(index, chosen);
    paidAfter += money.paid;
    expiredAfter += money.expired;
  }
  if (paidBefore <= 0 || paidAfter <= 0) return null;
  const blockedBefore = sum(before.map((index) => BLOCKED[index])) / before.length;
  const blockedAfter = sum(after.map((index) => BLOCKED[index])) / after.length;
  const date = new Date(at).toLocaleDateString("en-GB", { day: "numeric", month: "short" });
  return `Since auto-burn (${date}): ${hoursText(blockedAfter)} blocked a week, ${fixtureWholePercent(
    (expiredAfter / paidAfter) * 100
  )} expired. Before: ${hoursText(blockedBefore)}, ${fixtureWholePercent((expiredBefore / paidBefore) * 100)}.`;
}

export function findingFull(indexes, chosen) {
  let best = null;
  for (const account of chosen) {
    const weeks = indexes.filter((index) => account.used[index] !== null).length;
    if (weeks === 0) continue;
    const full = indexes.filter(
      (index) => account.used[index] !== null && account.used[index] >= 100
    ).length;
    if (full === 0) continue;
    if (best === null || full > best.full) best = { account, full, weeks };
  }
  if (best === null) return null;
  const noun = best.weeks === 1 ? "week" : "weeks";
  return `${best.account.abbr} was full in ${best.full} of ${best.weeks} ${noun}.`;
}

export function round2(value) {
  return Math.round(value * 100) / 100;
}

export function allUsedMoney() {
  let total = 0;
  for (const account of ACCOUNTS) {
    for (let index = 0; index < WEEKS.length; index += 1) {
      total += usedMoney(account, index);
    }
  }
  return total;
}

// A `Values` object for an answer whose two halves are already in money: the
// fixture has no missing half and no second currency.
export function fixtureValues(api, subscription) {
  return {
    api,
    apiCurrency: api === null ? null : "EUR",
    apiMissing: null,
    subscription,
    currency: "EUR",
    subscriptionMissing:
      subscription === null ? "the fixture has no subscription value here" : null,
  };
}

export function chosenHosts(query) {
  const all = ["atlas", "vega"];
  if (!query || typeof query !== "object") throw new Error("a statistics query is missing");
  if (query.host === null || query.host === undefined) return all;
  if (typeof query.host !== "string" || !all.includes(query.host)) {
    throw new Error(`unknown host ${JSON.stringify(query.host)}`);
  }
  return [query.host];
}

export function statsEmpty() {
  return (
    typeof location !== "undefined" &&
    new URLSearchParams(location.search).get("fixture") === "stats-empty"
  );
}

export function answer(query, build) {
  if (statsEmpty()) {
    return { empty: { recordingSince: "2026-11-09T08:00:00Z" } };
  }
  return { data: build(query) };
}
