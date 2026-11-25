// The statistics Overview page's fixture: `stats_overview` and its This-week
// card, `stats_this_week`.
//
// Two variants by the page address: `?fixture=stats-no-running` has no usage
// reading yet, so the This-week card is empty; `?fixture=stats-day1` has the
// running windows only, so no finished week is below them. With
// `?fixture=stats-empty` every range holds nothing, and the card still answers.

import {
  BLOCKED,
  DAY,
  NOW_MS,
  SUBSCRIPTIONS,
  byAbbr,
  chosenAccounts,
  expiredByAccount,
  findingAutoBurn,
  findingBlocked,
  findingExpired,
  findingFull,
  fixtureValues,
  marksOf,
  mockWeek,
  resetEnd,
  round1,
  round2,
  sum,
  switchesOf,
  topProjects,
  weekAccounts,
  weekIso,
  weekTime,
  weeklyPrice,
  windowsOfWeek,
} from "./stats-frame.js";

function variant() {
  if (typeof location === "undefined") return null;
  return new URLSearchParams(location.search).get("fixture");
}

// The accounts with a running weekly window on the card and in the range's
// running week: PR and AC.
const RUNNING_ABBRS = ["PR", "AC"];

// The reason the one partly valued project and the total carry the "≥": OL's
// month has no price, so its share of the value is unknown.
const PARTIAL_REASON = "the Ollama month has no price";

// The card's projects, in the order Rust sends them (subscription value,
// largest first; ties by tokens). `subscription` null is a project whose work
// has no known subscription value.
const PROJECTS = [
  {
    name: "atlas",
    tokens: 8_600_000,
    api: 240,
    subscription: 15.4,
    parts: [
      ["PR", 52],
      ["AC", 30],
      ["OA", 12],
      [null, 6],
    ],
  },
  {
    name: "ledger",
    tokens: 7_200_000,
    api: 210,
    subscription: 13.5,
    partial: true,
    parts: [
      ["PR", 44],
      ["AC", 34],
      ["OL", 16],
      [null, 6],
    ],
  },
  {
    name: "acme-kb",
    tokens: 6_500_000,
    api: 190,
    subscription: 12.1,
    parts: [
      ["AC", 70],
      ["PR", 20],
      ["OA", 10],
    ],
  },
  {
    name: "notes",
    tokens: 5_400_000,
    api: 160,
    subscription: 10.2,
    parts: [
      ["PR", 60],
      ["OL", 40],
    ],
  },
  {
    name: "ops-scripts",
    tokens: 4_800_000,
    api: 135,
    subscription: 8.6,
    parts: [
      ["AC", 55],
      ["OA", 45],
    ],
  },
  {
    name: "site",
    tokens: 4_100_000,
    api: 118,
    subscription: 7.5,
    parts: [
      ["PR", 80],
      ["AC", 20],
    ],
  },
  {
    name: "billing",
    tokens: 3_600_000,
    api: 100,
    subscription: 6.4,
    parts: [["AC", 100]],
  },
  {
    name: "docs",
    tokens: 3_200_000,
    api: 92,
    subscription: 5.9,
    parts: [
      ["PR", 50],
      ["OA", 50],
    ],
  },
  {
    name: "infra",
    tokens: 2_700_000,
    api: 76,
    subscription: 4.9,
    parts: [
      ["AC", 65],
      ["OL", 35],
    ],
  },
  {
    name: "ml-lab",
    tokens: 2_300_000,
    api: 64,
    subscription: 4.1,
    parts: [
      ["PR", 75],
      ["OA", 25],
    ],
  },
  {
    name: "parser",
    tokens: 1_900_000,
    api: 53,
    subscription: 3.4,
    parts: [
      ["AC", 90],
      [null, 10],
    ],
  },
  {
    name: "sandbox",
    tokens: 1_500_000,
    api: 42,
    subscription: 2.7,
    parts: [["PR", 100]],
  },
  {
    name: "toolshed",
    tokens: 1_100_000,
    api: 31,
    subscription: 2.0,
    parts: [["OA", 100]],
  },
  {
    name: "webapp",
    tokens: 780_000,
    api: 22,
    subscription: null,
    parts: [["OL", 100]],
  },
];

// The chip id of an abbreviation: a provider subscription's `account.acc` or a
// stored Claude account's id. The ids come from the built-in snapshot, filled
// in by `resolveIds` before an answer is built.
function chipId(abbr) {
  if (abbr === null) return null;
  const subscription = SUBSCRIPTIONS.find((candidate) => candidate.abbr === abbr);
  if (subscription) return subscription.id;
  const account = byAbbr(abbr);
  if (!account) throw new Error(`unknown subscription abbreviation ${JSON.stringify(abbr)}`);
  return account.id;
}

// The shares of a project's API value, largest first, the unknown part last.
function bySubscription(parts) {
  return parts
    .map(([abbr, percent]) => [chipId(abbr), percent])
    .sort(
      (a, b) =>
        b[1] - a[1] ||
        (a[0] === null ? 1 : 0) - (b[0] === null ? 1 : 0) ||
        String(a[0]).localeCompare(String(b[0]))
    );
}

// A project's two values, in the `Values` shape: `ledger` is partial with a
// reason, `webapp` has no known subscription part at all.
function projectValues(project) {
  if (project.subscription === null) {
    return {
      api: project.api,
      apiCurrency: "EUR",
      apiMissing: null,
      apiPartial: false,
      subscription: null,
      currency: "EUR",
      subscriptionMissing: PARTIAL_REASON,
      subscriptionPartial: false,
    };
  }
  const values = fixtureValues(project.api, project.subscription);
  if (project.partial) {
    values.subscriptionMissing = PARTIAL_REASON;
    values.subscriptionPartial = true;
  }
  return values;
}

// The 14 project rows of the new card shape.
function cardProjects() {
  return PROJECTS.map((project) => ({
    name: project.name,
    key: { kind: "repo", id: `github.com/ledger-co/${project.name}` },
    tokensOut: project.tokens,
    values: projectValues(project),
    bySubscription: bySubscription(project.parts),
  }));
}

function emptyValues() {
  return {
    api: 0,
    apiCurrency: "EUR",
    apiMissing: null,
    apiPartial: false,
    subscription: 0,
    currency: "EUR",
    subscriptionMissing: null,
    subscriptionPartial: false,
  };
}

// The card without a usage reading: no line and no project.
function emptyCard() {
  return {
    accounts: [],
    providers: [],
    totals: {
      tokensOut: 0,
      projects: 0,
      used: emptyValues(),
      paid: null,
      paidPartial: false,
      paidWithout: [],
      usedOfPaid: null,
    },
    projects: [],
  };
}

// The weekly window of `account` that runs at the fixture's now: the first one
// whose reset lies after it.
function runningWindow(account) {
  for (let index = 0; ; index += 1) {
    const end = Date.parse(resetEnd(account, index));
    if (end > NOW_MS) return { start: end - 7 * DAY, end };
  }
}

// The provider subscriptions' rows: OA's running week with its price, O2
// without a running window, OL's running month without a price.
function providerRows(subscriptions) {
  return subscriptions.map((subscription) => {
    const used = subscription.abbr === "O2" ? null : subscription.now;
    const priced = subscription.price !== null;
    const money = used === null ? null : priced ? (used / 100) * weeklyPrice(subscription) : null;
    return {
      name: subscription.abbr,
      company: subscription.company,
      used,
      money,
      moneyMissing:
        used === null
          ? "no running window of this subscription"
          : money === null
            ? "the account has no monthly price"
            : null,
      resetsAt:
        used === null
          ? null
          : subscription.monthEnd !== null
            ? subscription.monthEnd
            : new Date(Date.parse(resetEnd(subscription, 12))).toISOString(),
    };
  });
}

// The card ignores the range, so it never answers "empty".
export function thisWeek(query) {
  const { chosen, chosenSubscriptions } = chosenAccounts(query);
  if (variant() === "stats-no-running") return emptyCard();
  const providers = providerRows(chosenSubscriptions);
  const accounts = chosen
    .filter((account) => RUNNING_ABBRS.includes(account.abbr))
    .map((account) => {
      const window = runningWindow(account);
      return {
        id: account.id,
        used: account.now,
        now: account.now,
        money: (account.now / 100) * weeklyPrice(account),
        moneyMissing: null,
        startedAt: new Date(window.start).toISOString(),
        resetsAt: new Date(window.end).toISOString(),
      };
    });
  // No running window at all: no record the card counts, so no project either.
  if (accounts.length === 0 && !providers.some((provider) => typeof provider.used === "number")) {
    return emptyCard();
  }
  const projects = cardProjects();
  const used = {
    api: sum(PROJECTS.map((project) => project.api)),
    apiCurrency: "EUR",
    apiMissing: null,
    apiPartial: false,
    subscription: sum(
      PROJECTS.map((project) =>
        typeof project.subscription === "number" ? project.subscription : 0
      )
    ),
    currency: "EUR",
    subscriptionMissing: PARTIAL_REASON,
    subscriptionPartial: true,
  };
  const paid = round2(
    sum([
      ...accounts.map((account) => {
        const source = chosen.find((candidate) => candidate.id === account.id);
        if (!source) throw new Error(`no account row for ${JSON.stringify(account.id)}`);
        return weeklyPrice(source);
      }),
      ...chosenSubscriptions
        .filter((subscription) => subscription.abbr === "OA")
        .map((subscription) => weeklyPrice(subscription)),
    ])
  );
  return {
    accounts,
    providers,
    totals: {
      tokensOut: sum(PROJECTS.map((project) => project.tokens)),
      projects: PROJECTS.length,
      used,
      paid,
      // OL's running month has no price: the paid sum is a lower bound.
      paidPartial: chosenSubscriptions.some((subscription) => subscription.abbr === "OL"),
      // Google's work is in no record, so its week is not in the paid sum.
      paidWithout: chosenSubscriptions
        .filter((subscription) => subscription.company === "google")
        .map((subscription) => subscription.abbr),
      usedOfPaid: null,
    },
    projects,
  };
}

// The running week's account rows: PR's and AC's use so far, nothing expired
// yet (a running window has no expired share).
function runningWeekAccounts(chosen) {
  return chosen
    .filter((account) => RUNNING_ABBRS.includes(account.abbr))
    .map((account) => ({
      id: account.id,
      usedPercent: account.now,
      used: (account.now / 100) * weeklyPrice(account),
      expiredPercent: 0,
      expired: null,
      uncertain: false,
    }));
}

export function overview(query) {
  const { indexes, chosen } = chosenAccounts(query);
  const currency = "EUR";
  const runningIndex = mockWeek(11);
  const runningChosen = chosen.filter((account) => RUNNING_ABBRS.includes(account.abbr));
  // The weeks whose windows finished: none on day 1, and the running week is
  // not one of them.
  const finished =
    variant() === "stats-day1" ? [] : indexes.filter((index) => index !== runningIndex);
  const weeks = indexes.map((index) => {
    const running = index === runningIndex;
    const done = finished.includes(index);
    const accounts = done
      ? weekAccounts(index, chosen)
      : running
        ? runningWeekAccounts(runningChosen)
        : [];
    const used = sum(accounts.map((account) => account.used ?? 0));
    const expired = sum(accounts.map((account) => account.expired ?? 0));
    // Every finished window of a priced account week offers one week's price,
    // as Rust counts the offered weeks (a reset week offers each of its
    // windows); the running week offers its running windows' week prices.
    const offered = done
      ? sum(
          chosen
            .filter((account) => account.used[index] !== null)
            .map((account) => weeklyPrice(account) * windowsOfWeek(account, index).length)
        )
      : running
        ? sum(runningChosen.map((account) => weeklyPrice(account)))
        : 0;
    return {
      week: weekIso(index),
      accounts,
      projects: done || running ? topProjects(index, used, false) : [],
      blockedHours: BLOCKED[index],
      // A running week has expired nothing yet, so it has no expired share.
      expiredShare: done && offered > 0 ? (expired / offered) * 100 : null,
      running,
    };
  });
  const used = sum(weeks.flatMap((week) => week.accounts.map((account) => account.used ?? 0)));
  const expired = sum(weeks.flatMap((week) => week.accounts.map((account) => account.expired ?? 0)));
  // The figures row counts every weekly window of the range, the running ones
  // included (R5): each offers its account's week price.
  const paid = sum(
    indexes.flatMap((index) => {
      if (finished.includes(index)) {
        return chosen
          .filter((account) => account.used[index] !== null)
          .map((account) => weeklyPrice(account) * windowsOfWeek(account, index).length);
      }
      if (index === runningIndex) return runningChosen.map((account) => weeklyPrice(account));
      return [];
    })
  );
  // The expired share divides by the finished windows only: a running window
  // has expired nothing.
  const finishedOffered = sum(
    finished.flatMap((index) =>
      chosen
        .filter((account) => account.used[index] !== null)
        .map((account) => weeklyPrice(account) * windowsOfWeek(account, index).length)
    )
  );
  const switches = switchesOf(indexes);
  const switchesTotal =
    switches.user + switches.cli + switches.auto + switches.agent + switches.external;
  const blockedHours = sum(indexes.map((index) => BLOCKED[index]));
  // One figure per company, Anthropic first (the core's `blockedByCompany`):
  // the provider's blocked hours never add to Claude's.
  const blockedByCompany = [
    { company: "anthropic", hours: blockedHours },
    { company: "openai", hours: round1(indexes.length * 0.5) },
  ];
  const avoidableHours = blockedHours > 0 ? (blockedHours / 57) * 6 : 0;
  // The mock-up's percentages: the used money of every counted window over the
  // money those windows offered, the same rule as the Rust page.
  const usedPercent = paid > 0 ? (used / paid) * 100 : null;
  const expiredPercent = finishedOffered > 0 ? (expired / finishedOffered) * 100 : null;
  // The last account added in the range: the blocked hours of the whole weeks
  // before the week it was added in (the Rust page's `blockedBeforeAdded`).
  const added = marksOf(indexes).filter((mark) => mark.label.endsWith(" added"));
  const lastAdded = added[added.length - 1];
  let blockedBeforeAdded = null;
  if (lastAdded) {
    const name = lastAdded.label.slice(0, -" added".length);
    if (!byAbbr(name)) {
      throw new Error(`the added account ${JSON.stringify(name)} is not in the fixture`);
    }
    const at = Date.parse(lastAdded.at);
    const hours = sum(
      indexes.filter((index) => weekTime(index) + 7 * DAY <= at).map((index) => BLOCKED[index])
    );
    blockedBeforeAdded = { name, hours };
  }
  const findings = [
    finished.length > 0 ? findingExpired(finished, chosen, currency) : null,
    findingBlocked(indexes, blockedHours, avoidableHours),
    finished.length > 0 ? findingAutoBurn(finished, chosen) : null,
    // The full weeks finding counts every weekly window of the range, the
    // running ones included (R5).
    findingFull(indexes, chosen),
  ].filter((text) => text !== null);
  return {
    withoutPrice: [],
    kpis: {
      used,
      paid,
      expired,
      usedPercent,
      expiredPercent,
      expiredByAccount: expiredByAccount(finished, chosen),
      blockedByCompany,
      blockedBeforeAdded,
      avoidableHours,
      switches,
      switchesTotal,
      switchCostPercentOfWindow: 0.8,
      weeks: indexes.length,
    },
    weeks,
    marks: marksOf(indexes),
    findings,
  };
}
