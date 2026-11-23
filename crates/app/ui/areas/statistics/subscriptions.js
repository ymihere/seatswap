// The Subscriptions page (spec §3.3, R6 of specs/plan-1.3/PLAN.md): what each
// plan gives, whether it is used, what 1 % of a limit buys over the weeks, and
// the tokens a full week holds per subscription and model. The page formats
// and draws the struct; every sum, share and sentence comes from the Rust page.

import {
  accountName,
  accountNameText,
  accountPlan,
  accountView,
  amountNode,
  amountTip,
  clear,
  durationText,
  hoursText,
  emptyLine,
  h,
  isColor,
  modelName,
  percentText,
  providerName,
  shortDate,
  tokens,
} from "../../common.js";
import { lines } from "../../charts.js";

// The three limits the JSON may name, and the one wording of each.
const LIMITS = {
  week: { tab: "Week", phrase: "week" },
  "5h": { tab: "5 h", phrase: "5 h window" },
  month: { tab: "Month", phrase: "month" },
};

// Why a figure in hours of work is missing: no hour of the last 4 weeks holds
// priced work of the company's models (the Rust page's worked hour).
const NO_WORKED_HOUR = "no hour with priced work of this company in the last 4 weeks";

function requireObject(value, what) {
  if (value === null || typeof value !== "object" || Array.isArray(value)) {
    throw new Error(`${what} must be an object, not ${JSON.stringify(value)}`);
  }
  return value;
}

function requireArray(value, what) {
  if (!Array.isArray(value)) {
    throw new Error(`${what} must be a list, not ${JSON.stringify(value)}`);
  }
  return value;
}

function requireString(value, what) {
  if (typeof value !== "string" || value === "") {
    throw new Error(`${what} must be a non-empty string, not ${JSON.stringify(value)}`);
  }
  return value;
}

function optionalString(value, what) {
  if (value === null || value === undefined) return null;
  return requireString(value, what);
}

function requireNumber(value, what) {
  if (typeof value !== "number" || !Number.isFinite(value)) {
    throw new Error(`${what} must be a finite number, not ${JSON.stringify(value)}`);
  }
  return value;
}

function optionalNumber(value, what) {
  if (value === null || value === undefined) return null;
  return requireNumber(value, what);
}

function requireWholeNumber(value, what) {
  const number = requireNumber(value, what);
  if (!Number.isInteger(number) || number < 0) {
    throw new Error(`${what} must be a whole number of 0 or more, not ${JSON.stringify(value)}`);
  }
  return number;
}

function requireBoolean(value, what) {
  if (typeof value !== "boolean") {
    throw new Error(`${what} must be true or false, not ${JSON.stringify(value)}`);
  }
  return value;
}

function requireTime(value, what) {
  requireString(value, what);
  if (Number.isNaN(Date.parse(value))) {
    throw new Error(`${what} has no readable time: ${JSON.stringify(value)}`);
  }
  return value;
}

function limitInfo(value) {
  requireString(value, "a limit");
  const info = LIMITS[value];
  if (info === undefined) {
    throw new Error(`unknown limit ${JSON.stringify(value)}: use week, 5h or month`);
  }
  return info;
}

// The company name as plain text, through common.js's one provider tag: an
// unknown company is an error there, never a tag of its own.
function companyText(company) {
  return providerName(company).textContent;
}

function colorOf(env, id) {
  const view = accountView(env.accounts, id);
  return view && isColor(view.color) ? view.color : "var(--muted)";
}

function planNode(plan, id, env) {
  const text = optionalString(plan, "a plan");
  if (text !== null) return h("span", { class: "stats-plan", text });
  return accountPlan(env.accounts, id);
}

function amountCell(value, currency, reason) {
  const node = amountNode(value, currency);
  if (value === null || value === undefined) node.title = reason;
  return node;
}

function shareCell(value, reason) {
  if (value === null || value === undefined) {
    return h("span", { class: "n mf", text: "—", title: reason });
  }
  return h("span", { class: "n", text: percentText(requireNumber(value, "a used share")) });
}

// A subscription row whose price was never entered: the row's id is the stored
// account's id or the provider subscription's `account.acc`, the item the
// Settings address `#settings/accounts?<id>` accepts. A row whose entered price
// cannot be shown in the display currency keeps its "—" with its reason.
function noPriceCell(id) {
  return h("span", { class: "mf stats-sub-no-price" }, [
    "no price yet · ",
    h("a", {
      href: `#settings/accounts?${id}`,
      class: "stats-sub-set-price",
      text: "set it",
      title: "Enter the monthly price in Settings → Accounts",
    }),
  ]);
}

// A subscribed price in the display currency; the price the user entered stays
// in the tooltip when the row carries one.
function priceCell(row, currency, reason, env) {
  const price = optionalNumber(row.priceMonth, "a usage row's priceMonth");
  // The "all" total row is a sum, not a subscription: it never carries a link.
  if (row.id !== "all" && price === null) {
    const id = requireString(row.id, "a usage row's id");
    // Settings can only open a row its account or provider subscription gives:
    // a Claude account no longer stored has no such row, so its cell keeps
    // "—" with the row's reason.
    if (
      (row.entered === null || row.entered === undefined) &&
      accountView(env.accounts, id) !== null
    ) {
      return noPriceCell(id);
    }
  }
  const node = amountCell(price, currency, reason);
  if (row.entered === null || row.entered === undefined) return node;
  requireObject(row.entered, "a usage row's entered price");
  const amount = requireNumber(row.entered.amount, "an entered price's amount");
  const enteredCurrency = optionalString(
    row.entered.currency,
    "an entered price's currency"
  );
  node.title = `entered as ${amount}${enteredCurrency === null ? "" : ` ${enteredCurrency}`}${
    typeof currency === "string" ? `, shown in ${currency}` : ""
  }`;
  return node;
}

// A bar's width is a percent; the text beside it carries the number itself,
// which may pass 100. An estimated bar is striped by its class.
function barNode(percent, color, title, estimated) {
  const bar = h("span", {
    class: `stats-ubar${estimated ? " stats-ubar-est" : ""}`,
    style: { "--c": color },
  });
  if (percent !== null) {
    const value = requireNumber(percent, "a bar width");
    if (value < 0) throw new Error(`a bar width ${String(value)} is not a percent`);
    const fill = h("i", { style: { width: `${Math.min(value, 100)}%` } });
    if (!estimated) fill.style.background = color;
    bar.appendChild(fill);
  }
  if (title) bar.title = title;
  return bar;
}

function ratioText(value, what) {
  const fixed = requireNumber(value, what).toFixed(2);
  return fixed.endsWith("0") ? fixed.slice(0, -1) : fixed;
}

// An hours figure of work: whole hours from 1h up, minutes below ("30h",
// "40min"), hours not rolled into days.
function workHoursText(hours) {
  const value = requireNumber(hours, "a number of hours");
  if (value < 0) throw new Error(`a number of hours ${String(value)} is negative`);
  return hoursText(value >= 1 ? Math.round(value) : Math.round(value * 60) / 60, { upTo: "h" });
}

function signedPercentText(value) {
  const number = requireNumber(value, "a change's percent");
  if (number === 0) return percentText(0);
  const sign = number < 0 ? "−" : "+";
  return `${sign}${percentText(Math.abs(number))}`;
}

function cardHead(title, sub) {
  return h("div", { class: "ch" }, [
    h("span", { class: "t", text: title }),
    typeof sub === "string" ? h("span", { class: "s", text: sub }) : null,
  ]);
}

// ---------------------------------------------------------------------------
// the totals line
// ---------------------------------------------------------------------------

function totalsCard(totals, currency, env) {
  requireObject(totals, "totals");
  const count = requireWholeNumber(totals.count, "totals.count");
  const price = optionalNumber(totals.priceMonth, "totals.priceMonth");
  const api = optionalNumber(totals.apiMonth, "totals.apiMonth");
  const times = optionalNumber(totals.timesPaid, "totals.timesPaid");
  const used = optionalNumber(totals.tokensUsedMonth, "totals.tokensUsedMonth");
  const weeks = requireWholeNumber(totals.weeks, "totals.weeks");
  const partial = requireArray(totals.partial, "totals.partial");

  const card = h("div", { class: "hcard stats-sub-totals" });
  // A sum over no counted subscription has no figure at all: the line says
  // what is missing instead of writing 0 for each sum.
  if (count === 0) {
    card.appendChild(
      h("span", {
        class: "mf",
        text: "No subscription has a finished week in this range yet.",
        title: "the totals count only subscriptions with a finished week in this range",
      })
    );
    return card;
  }
  card.appendChild(
    h("span", {
      class: "x13 stats-sub-totals-title",
      text: `All ${count} subscription${count === 1 ? "" : "s"}, a month`,
    })
  );
  card.appendChild(
    h("span", {}, ["cost ", h("b", {}, [amountCell(price, currency, "no price in this range")])])
  );
  const worth = h("span", {}, [
    "worth ",
    h("b", {}, [amountCell(api, currency, "no API value in this range")]),
    " at API prices ",
  ]);
  if (times === null) {
    worth.appendChild(h("span", { class: "mf", text: "(× —)", title: "no price to compare with" }));
  } else {
    worth.appendChild(h("span", { class: "mf", text: `(× ${times.toFixed(1)})` }));
  }
  card.appendChild(worth);
  if (used === null) {
    card.appendChild(
      h("span", { class: "mf", text: "tokens used —", title: "no tokens measured in this range" })
    );
  } else {
    card.appendChild(h("span", {}, ["tokens used ", h("b", { text: tokens(used) })]));
  }
  card.appendChild(h("span", { class: "sp" }));
  const weeksNode = h("span", {
    class: "xs mf",
    text: `from ${rangeWeeksText(weeks)}`,
  });
  for (const entry of partial) {
    requireObject(entry, "an entry of totals.partial");
    const id = requireString(entry.id, "totals.partial[].id");
    const ownWeeks = requireWholeNumber(entry.weeks, "totals.partial[].weeks");
    weeksNode.appendChild(
      document.createTextNode(`, ${accountNameText(env.accounts, id)} from its ${ownWeeks}`)
    );
  }
  card.appendChild(weeksNode);
  return card;
}

// ---------------------------------------------------------------------------
// card 1: what does each plan give me?
// ---------------------------------------------------------------------------

// The finished calendar weeks a figure covers: "the last 3 weeks", or, in a
// range whose only week still runs, the windows of it that have finished.
function rangeWeeksText(weeks) {
  if (weeks === 0) return "the windows of this week that have finished";
  return `the last ${weeks} week${weeks === 1 ? "" : "s"}`;
}

function trendNode(change) {
  requireObject(change, "a change");
  const model = requireString(change.model, "change.model");
  const percent = requireNumber(change.percent, "change.percent");
  const since = requireTime(change.since, "change.since");
  const before = requireNumber(change.before, "change.before");
  const after = requireNumber(change.after, "change.after");
  return h("span", {
    class: "stats-warn",
    text: `${modelName(model)} ${signedPercentText(percent)} since ${shortDate(since)}`,
    title: `1 % bought ${tokens(before)} tokens before, ${tokens(after)} since`,
  });
}

// The reference plan's name and id. `sizes.reference` is the reference
// subscription's id; its name comes from its size row, else the account view.
function referencePlan(sizes, env) {
  if (sizes.reference === null || sizes.reference === undefined) {
    return { id: null, text: "—", title: "no measured plan in this range" };
  }
  const reference = requireString(sizes.reference, "sizes.reference");
  const rows = requireArray(sizes.rows, "sizes.rows");
  const row = rows.find((candidate) => candidate && candidate.id === reference);
  if (row) {
    const plan = optionalString(row.plan, "sizes.rows[].plan");
    if (plan !== null) return { id: reference, text: plan, title: `measured against ${plan}` };
    const view = accountView(env.accounts, reference);
    if (view && typeof view.plan === "string" && view.plan !== "") {
      return {
        id: reference,
        text: view.plan,
        title: `measured against ${view.plan}`,
      };
    }
  }
  throw new Error(`sizes.reference ${JSON.stringify(reference)} names no plan row`);
}

function planRowNode(row, env, unmeasured) {
  requireObject(row, "a plan row");
  const id = requireString(row.id, "a plan row's id");
  companyText(row.company);
  const company = requireString(row.company, "a plan row's company");
  const fullHours = optionalNumber(row.fullHours, "a plan row's fullHours");
  const fullTokens = optionalNumber(row.fullTokens, "a plan row's fullTokens");
  const basis = optionalNumber(row.basisHours, "a plan row's basisHours");
  const thin = requireBoolean(row.thin, "a plan row's thin");
  const missing = optionalString(row.missing, "a plan row's missing reason");

  const line = h("div", { class: "lr stats-sub-plans-row" });
  const account = h("span", { class: "stats-sub-account" });
  account.appendChild(accountName(env.accounts, id));
  account.appendChild(planNode(row.plan, id, env));
  line.appendChild(account);

  const work = h("span", { class: "stats-sub-work" });
  if (missing !== null) {
    work.appendChild(h("span", { class: "mf", text: "—", title: missing }));
  } else if ((fullHours === null) !== (fullTokens === null)) {
    throw new Error(`a plan row ${JSON.stringify(id)} has only one of fullHours and fullTokens`);
  } else if (fullHours === null) {
    work.appendChild(h("span", { class: "mf", text: "—", title: NO_WORKED_HOUR }));
  } else {
    if (fullHours < 0) throw new Error(`a plan row's fullHours are negative: ${fullHours}`);
    if (fullTokens < 0) throw new Error(`a plan row's fullTokens are negative: ${fullTokens}`);
    work.appendChild(h("b", { text: workHoursText(fullHours) }));
    work.appendChild(document.createTextNode(" of work · "));
    work.appendChild(h("b", { text: tokens(fullTokens) }));
    work.appendChild(document.createTextNode(" tokens"));
    if (company === "ollama") work.appendChild(h("span", { class: "mf", text: " a month" }));
    work.title =
      "a full limit in hours you actually worked with this company's models over the last 4 weeks, at their average cost, and in tokens at your mix of models";
  }
  line.appendChild(work);

  if (missing !== null) {
    line.appendChild(h("span", { class: "mf", text: "—", title: missing }));
  } else if (unmeasured.has(id)) {
    // Its factor still leans on the plan's name (Plan sizes says "not
    // measured yet"), so there is no measured course over the weeks either.
    line.appendChild(
      h("span", {
        class: "stats-warn",
        text: "not measured yet",
        title: "too little priced work in this range: the hours lean on the plan's name",
      })
    );
  } else {
    // Too few points tell no change, so such a line is not called steady.
    const trendMissing = optionalString(row.trendMissing, "a plan row's trendMissing");
    if (row.trend !== null && row.trend !== undefined) {
      line.appendChild(trendNode(row.trend));
    } else if (trendMissing !== null) {
      line.appendChild(h("span", { class: "mf", text: "—", title: trendMissing }));
    } else {
      line.appendChild(h("span", { class: "mf", text: "steady" }));
    }
  }

  const basisNode = h("span", { class: "n stats-sub-basis" });
  if (missing !== null) {
    basisNode.appendChild(h("span", { class: "mf", text: "—", title: missing }));
  } else if (basis === null) {
    basisNode.appendChild(h("span", { class: "mf", text: "—", title: "no work in this range" }));
  } else {
    if (basis < 0) throw new Error(`a plan row's basisHours is negative: ${basis}`);
    basisNode.appendChild(document.createTextNode(workHoursText(basis)));
    if (thin && !unmeasured.has(id)) {
      basisNode.appendChild(
        h("span", {
          class: "stats-warn",
          text: " · thin",
          title: "the range's half-width is more than 20 % of the value",
        })
      );
    }
  }
  line.appendChild(basisNode);
  return line;
}

function planSizes(sizes, env, body) {
  requireObject(sizes, "sizes");
  const rows = requireArray(sizes.rows, "sizes.rows");
  const reference = referencePlan(sizes, env);

  body.appendChild(
    h("div", { class: "lr stats-sub-sizes-head" }, [
      h("span", {
        text:
          reference.id === null
            ? "Plan sizes: no plan measured in this range yet"
            : `Plan sizes, measured against ${reference.text}`,
        title: reference.title,
      }),
    ])
  );
  if (rows.length === 0) {
    body.appendChild(emptyLine("no measured plan in this range"));
    return;
  }
  for (const row of rows) {
    requireObject(row, "a plan size row");
    const id = optionalString(row.id, "a plan size row's id");
    if (id !== null && id === reference.id) continue;
    const plan = optionalString(row.plan, "a plan size row's plan");
    const ofReference = optionalNumber(row.ofReference, "a plan size row's ofReference");
    const low = optionalNumber(row.low, "a plan size row's low");
    const high = optionalNumber(row.high, "a plan size row's high");
    const byName = optionalNumber(row.byName, "a plan size row's byName");
    const measured = requireBoolean(row.measured, "a plan size row's measured");

    const line = h("div", { class: "lr stats-sub-size-row" });
    line.appendChild(id === null ? h("span") : accountName(env.accounts, id));
    const text = h("span", { class: "stats-sub-size-text" });

    if (ofReference === null) {
      if (measured) {
        throw new Error(`a plan size row with no ratio of the reference is measured`);
      }
      if (low !== null || high !== null) {
        throw new Error(`a plan size row with no ratio of the reference has a range`);
      }
      text.classList.add("mf");
      text.appendChild(planNode(plan, id, env));
      text.appendChild(document.createTextNode(" · not measured yet"));
      if (byName !== null) {
        text.appendChild(
          document.createTextNode(`, counts as ${ratioText(byName, "byName")}× by its name`)
        );
      }
      line.appendChild(text);
      line.appendChild(h("span"));
      line.appendChild(h("span"));
      body.appendChild(line);
      continue;
    }

    if (low === null || high === null) {
      throw new Error(`a plan size row with a ratio of the reference has no range`);
    }
    if (high < low) throw new Error(`a plan size row's high ${high} is below its low ${low}`);
    text.appendChild(h("b", { text: `${ratioText(ofReference, "ofReference")}× ${reference.text}` }));
    if (plan !== null && plan !== reference.text) {
      text.appendChild(h("span", { class: "stats-plan", text: ` · ${plan}` }));
    }
    if (!measured) text.classList.add("mf");
    line.appendChild(text);
    if (byName === null) {
      line.appendChild(
        h("span", {
          class: "mf stats-sub-size-name",
          text: "by name —",
          title: "this plan's name gives no capacity",
        })
      );
    } else {
      line.appendChild(
        h("span", {
          class: "mf stats-sub-size-name",
          text: `by name ${ratioText(byName, "byName")}×`,
          title: "the size the plan's name gives, against the reference plan",
        })
      );
    }
    line.appendChild(
      measured
        ? h("span", {
            class: "n mf",
            text: `± ${ratioText((high - low) / 2, "a plan size row's range")}`,
            title: "the range of the ratio",
          })
        : h("span", {
            class: "n mf",
            text: "not measured",
            title: "only a few hours of work rest on this figure",
          })
    );
    body.appendChild(line);
  }
}

function plansCard(plans, sizes, env) {
  const rows = requireArray(plans, "plans");
  const card = h("div", { class: "hcard stats-sub-plans" });
  card.appendChild(cardHead("What does each plan give me?", "your actual work, measured"));
  const body = h("div", { class: "stats-sub-plans-body" });
  if (rows.length === 0) {
    body.appendChild(emptyLine("no subscription in this range"));
  } else {
    body.appendChild(
      h("div", { class: "lr hd stats-sub-plans-row" }, [
        h("span", { text: "Subscription" }),
        h("span", {
          text: "A full limit holds",
          title:
            "an hour of work: an hour of the last 4 weeks with work of this company's models, at their average cost and tokens; Ollama: a month's limit",
        }),
        h("span", { text: "Over the weeks" }),
        h("span", {
          class: "n",
          text: "Based on",
          title: "the hours of your work these figures were measured on",
        }),
      ])
    );
    // The plans whose factor is not measured, as Plan sizes reports them.
    const unmeasured = new Set(
      requireArray(sizes.rows, "sizes.rows")
        .filter((size) => size && size.measured === false)
        .map((size) => size.id)
    );
    for (const row of rows) body.appendChild(planRowNode(row, env, unmeasured));
  }
  planSizes(sizes, env, body);
  card.appendChild(body);
  return card;
}

// ---------------------------------------------------------------------------
// card 2: am I using what I pay for?
// ---------------------------------------------------------------------------

function usageLabel(row, env) {
  const id = requireString(row.id, "a usage row's id");
  const label = h("span", { class: "stats-sub-account" });
  label.appendChild(accountName(env.accounts, id));
  label.appendChild(planNode(row.plan, id, env));
  const since = optionalString(row.since, "a usage row's since");
  if (since !== null) {
    requireTime(since, "a usage row's since");
    label.appendChild(
      h("span", {
        class: "stats-plan",
        text: `since ${shortDate(since)}`,
        title: `this subscription started ${shortDate(since)}`,
      })
    );
  }
  return label;
}

function usageCells(row, currency, env) {
  const missing = optionalString(row.missing, "a usage row's missing reason");
  const used = optionalNumber(row.usedPercent, "a usage row's usedPercent");
  const expired = optionalNumber(row.expired, "a usage row's expired");
  const api = optionalNumber(row.api, "a usage row's api");
  const times = optionalNumber(row.timesPaid, "a usage row's timesPaid");
  const color = colorOf(env, row.id);

  const priceReason = missing || "no price entered for this subscription";
  const usedReason = missing || "no finished week in this range";
  const apiReason = missing || "no API value for this subscription";
  const timesReason = missing || "no paid money to compare with";
  const expiredReason = missing || "no finished week in this range";

  const cells = [priceCell(row, currency, priceReason, env)];
  const plan = optionalString(row.plan, "a usage row's plan");
  const barTitle =
    used === null
      ? usedReason
      : `${
          row.id === "all"
            ? "all subscriptions"
            : plan === null
              ? accountNameText(env.accounts, row.id)
              : plan
        } · used ${percentText(used)}`;
  cells.push(barNode(used, color, barTitle, false));
  cells.push(shareCell(used, usedReason));
  cells.push(amountCell(expired, currency, expiredReason));

  const apiCell = h("span", { class: "n stats-sub-api" });
  if (api === null) {
    apiCell.appendChild(h("span", { class: "mf", text: "—", title: apiReason }));
  } else {
    apiCell.appendChild(amountNode(api, currency));
    apiCell.appendChild(document.createTextNode(" · × "));
    if (times === null) {
      apiCell.appendChild(h("span", { class: "mf", text: "—", title: timesReason }));
    } else {
      apiCell.appendChild(document.createTextNode(times.toFixed(1)));
    }
    apiCell.title = `${amountTip(api, currency)} at API prices`;
  }
  cells.push(apiCell);
  return cells;
}

function usageRowNode(row, currency, env) {
  requireObject(row, "a usage row");
  const line = h("div", { class: "lr stats-sub-usage-row" });
  line.appendChild(usageLabel(row, env));
  for (const cell of usageCells(row, currency, env)) line.appendChild(cell);
  return line;
}

function usageTotalNode(row, count, currency, env) {
  requireObject(row, "usageTotal");
  if (row.id !== "all") {
    throw new Error(`usageTotal.id must be "all", not ${JSON.stringify(row.id)}`);
  }
  const line = h("div", { class: "lr stats-sub-usage-row stats-sub-usage-total" });
  line.appendChild(
    h("span", { class: "stats-sub-total-label", text: `All ${count}` })
  );
  for (const cell of usageCells(row, currency, env)) line.appendChild(cell);
  return line;
}

function usageCard(groups, total, totalCount, weeks, currency, env) {
  const list = requireArray(groups, "usage");
  const card = h("div", { class: "hcard stats-sub-usage" });
  card.appendChild(
    cardHead(
      "Am I using what I pay for?",
      totalCount === 0 ? "no finished week in this range" : rangeWeeksText(weeks)
    )
  );
  const body = h("div", { class: "stats-sub-usage-body" });
  body.appendChild(
    h("div", { class: "lr hd stats-sub-usage-row" }, [
      h("span", { text: "Subscription" }),
      h("span", { class: "n", text: "Price / month" }),
      h("span", { text: "Used" }),
      h("span"),
      h("span", { class: "n", text: "Expired" }),
      h("span", { class: "n", text: "At API prices · × paid" }),
    ])
  );
  for (const group of list) {
    requireObject(group, "a usage group");
    companyText(group.company);
    const groupCount = requireWholeNumber(group.count, "a usage group's count");
    const price = optionalNumber(group.priceMonth, "a usage group's priceMonth");
    const rows = requireArray(group.rows, "a usage group's rows");
    // A company with no subscription in the range gets no subtitle row: its
    // count and price would describe nothing below it.
    if (rows.length === 0) continue;
    const line = h("div", { class: "lr stats-sub-usage-group" });
    line.appendChild(
      h("span", { class: "stats-sub-group-company" }, [
        providerName(group.company, "the company of these subscriptions"),
        h("span", {
          class: "mf",
          text: `${groupCount} subscription${groupCount === 1 ? "" : "s"} · `,
        }),
        amountCell(price, currency, "no price entered for these subscriptions"),
        h("span", { class: "mf", text: " a month" }),
      ])
    );
    body.appendChild(line);
    for (const row of rows) body.appendChild(usageRowNode(row, currency, env));
  }
  // With nothing counted the total row would be a row of sums over no
  // subscription; the card's subtitle already says why there is no figure.
  if (totalCount > 0) body.appendChild(usageTotalNode(total, totalCount, currency, env));
  card.appendChild(body);
  return card;
}

// ---------------------------------------------------------------------------
// card 3: what 1 % of a limit buys, over the weeks
// ---------------------------------------------------------------------------

function changeSentence(change, id, limit, env) {
  requireObject(change, "a change");
  const model = requireString(change.model, "change.model");
  const since = requireTime(change.since, "change.since");
  const percent = requireNumber(change.percent, "change.percent");
  const before = requireNumber(change.before, "change.before");
  const after = requireNumber(change.after, "change.after");
  const direction = percent < 0 ? "fewer" : "more";
  const abbr = accountNameText(env.accounts, id);
  return `Since ${shortDate(since)}, 1 % of ${abbr}'s ${limitInfo(limit).phrase} buys about ${percentText(
    Math.abs(percent)
  )} ${direction} ${modelName(model)} tokens: ${tokens(after)} instead of ${tokens(before)}.`;
}

function nowLines(entry, limit, env, panel) {
  const now = requireArray(entry.now, "a per-percent entry's now");
  for (const item of now) {
    requireObject(item, "a per-percent entry's now row");
    const model = requireString(item.model, "a now row's model");
    const value = requireNumber(item.tokens, "a now row's tokens");
    panel.appendChild(
      h("p", { class: "stats-sub-now", text: `1 % buys ${modelName(model)} ${tokens(value)} tokens` })
    );
  }
  const minutes = optionalNumber(entry.minutes, "a per-percent entry's minutes");
  if (minutes !== null && minutes < 0) throw new Error("a per-percent entry's minutes are negative");
  const worth = h("p", { class: "stats-sub-now" });
  worth.appendChild(document.createTextNode("1 % ≈ "));
  worth.appendChild(
    minutes === null
      ? h("span", { class: "mf", text: "—", title: NO_WORKED_HOUR })
      : document.createTextNode(durationText(Math.round(minutes * 60)))
  );
  worth.appendChild(document.createTextNode(" of your work"));
  panel.appendChild(worth);

  const holds = optionalNumber(entry.windowHoldsMinutes, "windowHoldsMinutes");
  if (limit === "5h") {
    const line = h("p", { class: "stats-sub-now" });
    line.appendChild(document.createTextNode("A full 5 h window holds "));
    if (holds === null) {
      line.appendChild(
        h("span", {
          class: "mf",
          text: "—",
          title: NO_WORKED_HOUR,
        })
      );
    } else {
      if (holds < 0) throw new Error("windowHoldsMinutes is negative");
      line.appendChild(document.createTextNode(durationText(Math.round(holds * 60))));
    }
    line.appendChild(document.createTextNode(" of your work."));
    panel.appendChild(line);
  } else if (holds !== null) {
    throw new Error(`windowHoldsMinutes is set on a ${limit} line; it is only set on 5h lines`);
  }
}

function limitPanel(entry, env) {
  const id = requireString(entry.id, "a per-percent entry's id");
  const limit = requireString(entry.limit, "a per-percent entry's limit");
  limitInfo(limit);
  const model = requireString(entry.model, "a per-percent entry's model");
  const points = requireArray(entry.points, "a per-percent entry's points");
  const change = entry.change === null || entry.change === undefined ? null : entry.change;

  const panel = h("div", { class: "stats-sub-limit" });

  if (points.length === 0) {
    panel.appendChild(emptyLine("no finished week in this range"));
  } else {
    // The steady note is a reading of the points; a line without points says
    // only that there is nothing to read yet.
    const note = h("p", {
      class: `stats-sub-note ${change === null ? "stats-sub-note-ok" : "stats-sub-note-warn"}`,
    });
    const changeMissing = optionalString(entry.changeMissing, "a per-percent entry's changeMissing");
    if (change === null && changeMissing !== null) {
      note.className = "stats-sub-note mf";
      note.textContent = `${changeMissing.charAt(0).toUpperCase()}${changeMissing.slice(1)}.`;
    } else if (change === null) {
      note.textContent = "Steady. No week left its usual range.";
    } else {
      note.textContent = changeSentence(change, id, limit, env);
    }
    panel.appendChild(note);

    const primary = [];
    const likelyUpTo = [];
    const likelyFrom = [];
    for (const point of points) {
      requireObject(point, "a per-percent point");
      const week = requireTime(point.week, "a per-percent point's week");
      const value = requireNumber(point.tokens, "a per-percent point's tokens");
      const low = requireNumber(point.low, "a per-percent point's low");
      const high = optionalNumber(point.high, "a per-percent point's high");
      primary.push([week, value]);
      likelyFrom.push([week, low]);
      if (high !== null) likelyUpTo.push([week, high]);
    }
    const modelText = modelName(model);
    const series = [
      {
        name: `${modelText} per 1 %`,
        color: "var(--text)",
        points: primary,
        tip: (time, value) =>
          `${shortDate(new Date(time))}: ${tokens(value)} ${modelText} tokens per 1 %`,
      },
    ];
    if (likelyUpTo.length > 0) {
      series.push({
        name: "likely up to",
        color: "var(--muted)",
        dash: true,
        dots: false,
        points: likelyUpTo,
        tip: (time, value) =>
          `likely up to | ${shortDate(new Date(time))}: ${tokens(value)} ${modelText} tokens per 1 %`,
      });
    }
    series.push({
      name: "likely from",
      color: "var(--muted)",
      dash: true,
      dots: false,
      points: likelyFrom,
      tip: (time, value) =>
        `likely from | ${shortDate(new Date(time))}: ${tokens(value)} ${modelText} tokens per 1 %`,
    });
    const marks =
      change === null
        ? []
        : [
            {
              at: requireTime(change.since, "change.since"),
              label: `${percentText(Math.abs(requireNumber(change.percent, "change.percent")))} ${
                requireNumber(change.percent, "change.percent") < 0 ? "fewer" : "more"
              }`,
            },
          ];
    panel.appendChild(
      lines({
        series,
        marks,
        yFormat: (value) => tokens(value),
      })
    );
  }

  nowLines(entry, limit, env, panel);
  return panel;
}

function groupPerPercent(perPercent) {
  const rows = requireArray(perPercent, "perPercent");
  const groups = [];
  const byId = new Map();
  for (const entry of rows) {
    requireObject(entry, "a per-percent entry");
    const id = requireString(entry.id, "a per-percent entry's id");
    const limit = requireString(entry.limit, "a per-percent entry's limit");
    limitInfo(limit);
    requireString(entry.model, "a per-percent entry's model");
    requireArray(entry.points, "a per-percent entry's points");
    requireArray(entry.now, "a per-percent entry's now");
    let group = byId.get(id);
    if (group === undefined) {
      group = { id, entries: [] };
      byId.set(id, group);
      groups.push(group);
    }
    if (group.entries.some((candidate) => candidate.limit === limit)) {
      throw new Error(`a per-percent entry repeats the ${limit} limit of ${id}`);
    }
    group.entries.push(entry);
  }
  return groups;
}

function onePercentCard(perPercent, env) {
  const card = h("div", { class: "hcard stats-sub-onepct-card" });
  const groups = groupPerPercent(perPercent);
  const tabs = h("div", { class: "stats-sub-acc-tabs", role: "tablist" });
  card.appendChild(
    h("div", { class: "ch" }, [
      h("span", { class: "t", text: "What 1 % of a limit buys, over the weeks" }),
      h("span", { class: "sp" }),
      tabs,
    ])
  );
  const body = h("div", { class: "stats-sub-onepct-body" });
  card.appendChild(body);

  if (groups.length === 0) {
    body.appendChild(emptyLine("no subscription in this range"));
    return card;
  }

  // The mock-up opens on the subscription whose percentage changed: that is
  // the one line of the card that needs explaining. With no change anywhere,
  // the first subscription in the JSON is the natural first tab.
  const changed = groups.find((group) =>
    group.entries.some((entry) => entry.change !== null && entry.change !== undefined)
  );
  const state = { id: (changed || groups[0]).id, limits: new Map() };
  const tabNodes = new Map();

  function updateTabs() {
    for (const [id, node] of tabNodes) {
      const on = id === state.id;
      node.classList.toggle("stats-sub-acc-tab-on", on);
      node.setAttribute("aria-selected", on ? "true" : "false");
    }
  }

  function renderBody() {
    clear(body);
    const group = groups.find((candidate) => candidate.id === state.id);
    if (group === undefined) throw new Error(`unknown per-percent subscription ${state.id}`);
    const wanted = state.limits.get(group.id);
    const limit = wanted === undefined ? group.entries[0].limit : wanted;
    const current =
      group.entries.find((candidate) => candidate.limit === limit) || group.entries[0];

    const panel = h("div", { class: "stats-sub-onepct" });
    const top = h("div", { class: "stats-sub-onepct-top" });
    const limitTabs = h("div", { class: "stats-tabs", role: "tablist" });
    for (const entry of group.entries) {
      const info = limitInfo(entry.limit);
      const button = h("button", {
        type: "button",
        role: "tab",
        class: `stats-tab${entry.limit === current.limit ? " stats-tab-on" : ""}`,
        text: info.tab,
        "aria-selected": entry.limit === current.limit ? "true" : "false",
        onclick: () => {
          state.limits.set(group.id, entry.limit);
          renderBody();
        },
      });
      limitTabs.appendChild(button);
    }
    top.appendChild(limitTabs);
    top.appendChild(h("span", { class: "sp" }));
    top.appendChild(
      h("span", {
        class: "stats-sub-models",
        text: `${modelName(requireString(current.model, "a per-percent entry's model"))} tokens per 1 % · dashed: likely range`,
      })
    );
    panel.appendChild(top);

    const panels = h("div", { class: "stats-sub-onepct-panels" });
    for (const entry of group.entries) {
      const limitPanelNode = limitPanel(entry, env);
      limitPanelNode.hidden = entry.limit !== current.limit;
      panels.appendChild(limitPanelNode);
    }
    panel.appendChild(panels);
    body.appendChild(panel);
  }

  for (const group of groups) {
    const button = h(
      "button",
      {
        type: "button",
        role: "tab",
        class: "stats-sub-acc-tab",
        onclick: () => {
          state.id = group.id;
          updateTabs();
          renderBody();
        },
      },
      [accountName(env.accounts, group.id)]
    );
    tabNodes.set(group.id, button);
    tabs.appendChild(button);
  }
  updateTabs();
  renderBody();
  return card;
}

// ---------------------------------------------------------------------------
// card 4: tokens a week, per subscription and model
// ---------------------------------------------------------------------------

function parseTokenCell(cell, model, index) {
  requireObject(cell, "a tokens-week cell");
  const has = (field) => Object.prototype.hasOwnProperty.call(cell, field);
  const figureFields = ["tokens", "low", "high", "estimated", "thin", "measured"];
  if (has("missing")) {
    if (figureFields.some(has)) {
      throw new Error(
        `a tokens-week cell is both a figure and missing: ${JSON.stringify(cell)}`
      );
    }
    return { missing: requireString(cell.missing, "a tokens-week cell's missing reason") };
  }
  if (!has("tokens")) {
    throw new Error(
      `a tokens-week cell must be a figure or { missing }: ${JSON.stringify(cell)}`
    );
  }
  const value = requireNumber(cell.tokens, "a tokens-week cell's tokens");
  const low = requireNumber(cell.low, "a tokens-week cell's low");
  const high = optionalNumber(cell.high, "a tokens-week cell's high");
  const estimated = requireBoolean(cell.estimated, "a tokens-week cell's estimated");
  const thin = requireBoolean(cell.thin, "a tokens-week cell's thin");
  const measured = requireBoolean(cell.measured, "a tokens-week cell's measured");
  if (value < 0 || low < 0) throw new Error("a tokens-week cell has a negative token count");
  if (high !== null && high < low) {
    throw new Error(`a tokens-week cell's high ${high} is below its low ${low}`);
  }
  if (typeof model.model !== "string") {
    throw new Error(`a tokens-week model at ${index} has no id`);
  }
  return { value, low, high, estimated, thin, measured };
}

function tokenCell(parsed, model, max, color, env, id) {
  if (parsed.missing !== undefined) {
    return h("span", {
      class: "mf stats-sub-token-null",
      text: "—",
      title: parsed.missing,
    });
  }
  const cell = h("span", { class: "stats-sub-token-cell" });
  const width = max > 0 ? (parsed.value / max) * 100 : 0;
  cell.appendChild(barNode(width, color, null, parsed.estimated));
  const text = h("span", { class: "stats-sub-token-text" });
  text.appendChild(document.createTextNode(tokens(parsed.value)));
  const modelText = modelName(requireString(model.model, "a tokens-week model id"));
  let title = `${accountNameText(env.accounts, id)} · ${modelText} | ${tokens(
    parsed.value
  )} tokens a full weekly limit`;
  // One mark after the figure, the first that applies. A factor that leans on
  // its plan's size comes first: its range says nothing measured (with no pair
  // it is the prior itself and would read `± 0`). "assumed" is the Forecast's
  // word for the same factor ("measured or assumed from its plan").
  const mark = (cls, markText) => {
    text.appendChild(document.createTextNode(" "));
    text.appendChild(h("span", { class: cls, text: markText }));
  };
  if (!parsed.measured) {
    mark("stats-plan", "assumed");
    title += " | assumed from the plan's size: too little work measured yet";
  } else if (parsed.estimated) {
    mark("stats-plan", "est.");
  } else if (parsed.thin) {
    mark("stats-warn", "thin");
    title += " | thin: measured on only a little work in the range";
  } else if (parsed.high !== null) {
    const range = tokens((parsed.high - parsed.low) / 2);
    mark("stats-plan", `± ${range}`);
    title += ` ± ${range} (range)`;
  }
  if (parsed.estimated) {
    title += ` | estimated: priced at ${companyText(
      model.company
    )}'s main token mix, not measured`;
  }
  cell.appendChild(text);
  cell.title = title;
  return cell;
}

function tokensWeekCard(tokensWeek, plans, env) {
  requireObject(tokensWeek, "tokensWeek");
  const models = requireArray(tokensWeek.models, "tokensWeek.models");
  const rows = requireArray(tokensWeek.rows, "tokensWeek.rows");
  const plansById = new Map();
  for (const plan of requireArray(plans, "plans")) {
    if (plan && typeof plan.id === "string") plansById.set(plan.id, plan.plan);
  }

  const card = h("div", { class: "hcard stats-sub-tokens" });
  card.appendChild(
    cardHead(
      "Tokens a week, per subscription and model",
      "a full limit · bars compare within a model"
    )
  );

  for (const model of models) {
    requireObject(model, "a tokensWeek model");
    requireString(model.model, "a tokensWeek model's id");
    companyText(model.company);
  }

  const parsedRows = rows.map((row) => {
    requireObject(row, "a tokensWeek row");
    const id = requireString(row.id, "a tokensWeek row's id");
    const cells = requireArray(row.cells, "a tokensWeek row's cells");
    if (cells.length !== models.length) {
      throw new Error(
        `the tokensWeek row ${id} has ${cells.length} cells for ${models.length} models`
      );
    }
    return {
      id,
      cells: cells.map((cell, index) => parseTokenCell(cell, models[index], index)),
      missing: optionalString(row.missing, "a tokensWeek row's missing reason"),
    };
  });

  const maxes = models.map((_, index) => {
    let max = 0;
    for (const row of parsedRows) {
      if (row.missing !== null) continue;
      const cell = row.cells[index];
      if (cell.missing === undefined && cell.value > max) max = cell.value;
    }
    return max;
  });

  const estimatedColumns = models.map((_, index) => {
    let any = false;
    let allEstimated = true;
    for (const row of parsedRows) {
      if (row.missing !== null) continue;
      const cell = row.cells[index];
      if (cell.missing !== undefined) continue;
      any = true;
      if (!cell.estimated) allEstimated = false;
    }
    return any && allEstimated;
  });

  const columns = `118px repeat(${models.length}, minmax(0, 1fr))`;
  const body = h("div", { class: "stats-sub-tokens-body" });
  const head = h("div", { class: "lr hd stats-sub-model-row stats-sub-model-head" });
  head.style.gridTemplateColumns = columns;
  head.appendChild(h("span", { text: "Subscription" }));
  for (const [index, model] of models.entries()) {
    head.appendChild(
      h("span", {
        text: `${modelName(model.model)}${estimatedColumns[index] ? ", estimated" : ""}`,
        title: estimatedColumns[index]
          ? "this model is not measured: priced at the company's main token mix"
          : "a full weekly limit's tokens",
      })
    );
  }
  body.appendChild(head);

  for (const row of parsedRows) {
    const line = h("div", { class: "lr stats-sub-model-row stats-sub-token-row" });
    line.style.gridTemplateColumns = columns;
    const label = h("span", { class: "stats-sub-account" });
    label.appendChild(accountName(env.accounts, row.id));
    const plan = plansById.get(row.id);
    label.appendChild(
      typeof plan === "string" && plan !== ""
        ? h("span", { class: "stats-plan", text: plan })
        : accountPlan(env.accounts, row.id)
    );
    line.appendChild(label);
    const color = colorOf(env, row.id);
    for (const [index, model] of models.entries()) {
      line.appendChild(
        row.missing !== null
          ? h("span", {
              class: "mf stats-sub-token-null",
              text: "—",
              title: row.missing,
            })
          : tokenCell(row.cells[index], model, maxes[index], color, env, row.id)
      );
    }
    body.appendChild(line);
  }

  body.appendChild(
    h("p", {
      class: "stats-sub-token-note",
      text: "Fitted from all work up to now each time the page opens; the range is how far a figure may still be off.",
    })
  );
  card.appendChild(body);
  return card;
}

// ---------------------------------------------------------------------------
// the page
// ---------------------------------------------------------------------------

export async function show(container, env) {
  const data = await env.call("stats_subscriptions");
  requireObject(data, "stats_subscriptions");
  if (
    data.currency !== null &&
    data.currency !== undefined &&
    typeof data.currency !== "string"
  ) {
    throw new Error(`the page's currency must be a string, not ${JSON.stringify(data.currency)}`);
  }
  const currency =
    typeof data.currency === "string" ? data.currency : env.currency;

  const root = h("div", { class: "g stats-subscriptions" });
  root.appendChild(totalsCard(data.totals, currency, env));
  const totalCount = requireWholeNumber(data.totals.count, "totals.count");
  const weeks = requireWholeNumber(data.totals.weeks, "totals.weeks");
  root.appendChild(plansCard(data.plans, data.sizes, env));
  root.appendChild(usageCard(data.usage, data.usageTotal, totalCount, weeks, currency, env));
  root.appendChild(onePercentCard(data.perPercent, env));
  root.appendChild(tokensWeekCard(data.tokensWeek, data.plans, env));
  clear(container);
  container.appendChild(root);
}
