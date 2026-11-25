// The Overview page (spec §3.1): the weekly windows running now, the five
// numbers, each week by account and by project (the running windows counted
// too), what the numbers say, and the share that expired per week. The page
// formats and draws the struct; it adds no sum, share or sentence of its own.

import {
  accountName,
  accountNameText,
  accountPlan,
  accountView,
  amountNode,
  amountTip,
  clear,
  dateText,
  dateTimeText,
  hoursText,
  emptyLine,
  errorMessage,
  h,
  isColor,
  isObject,
  kpi,
  legendItem,
  percentText,
  providerName,
  showTokens,
  shortDate,
  subscriptionSplit,
  switchText,
  valueNode,
  valueText,
  weekdayTimeText,
} from "../../common.js";
import { lines, stackedColumns } from "../../charts.js";

const PROJECT_TOKENS = [
  "var(--focus)",
  "var(--warn)",
  "var(--pill-text)",
  "var(--danger)",
  "var(--muted)",
];

// One colour per project across the page: the lists of `[name, value]` pairs
// are walked in order, a new name takes the next token, `other` is always the
// muted one.
function projectColours(lists) {
  const colours = new Map();
  let index = 0;
  for (const list of lists) {
    for (const [name] of list) {
      if (colours.has(name)) continue;
      if (name === "other") colours.set(name, "var(--muted)");
      else {
        colours.set(name, PROJECT_TOKENS[index % PROJECT_TOKENS.length]);
        index += 1;
      }
    }
  }
  return colours;
}

// A percent as a bar's width; the text beside the bar carries the number
// itself, which may pass 100.
function barWidth(value) {
  if (typeof value !== "number" || !Number.isFinite(value) || value < 0) {
    throw new Error(`the bar width ${String(value)} is not a percent`);
  }
  return `${Math.min(value, 100)}%`;
}

// The subscription behind a This-week provider row: the frame's view for its
// abbreviation, or null for a company row (work whose subscription is not
// known), which the subscriptions block leaves out.
function providerSubscription(provider, env) {
  if (typeof provider.name !== "string" || provider.name === "") return null;
  return env.accounts.find((view) => view.abbr === provider.name) || null;
}

// The known subscription value of a project's row: the number the bar is as
// long as, or null when the project has no known part (its cell and its empty
// bar carry the reason).
function knownSubscription(project) {
  const value = project.values.subscription;
  if (value === null || value === undefined) return null;
  if (typeof value !== "number" || !Number.isFinite(value) || value < 0) {
    throw new Error(
      `the subscription value of ${JSON.stringify(project.name)} is a finite number of 0 or more, not ${JSON.stringify(value)}`
    );
  }
  return value;
}

// The card's fields the page draws; a shape it does not expect is an error,
// never a zero or a blank.
function checkThisWeek(card) {
  if (!isObject(card)) throw new Error("stats_this_week returned no card");
  if (!Array.isArray(card.accounts)) throw new Error("stats_this_week returned no accounts");
  if (!Array.isArray(card.providers)) throw new Error("stats_this_week returned no providers");
  if (!Array.isArray(card.projects)) throw new Error("stats_this_week returned no projects");
  const totals = card.totals;
  if (!isObject(totals)) throw new Error("stats_this_week returned no totals");
  if (
    typeof totals.tokensOut !== "number" ||
    !Number.isFinite(totals.tokensOut) ||
    totals.tokensOut < 0
  ) {
    throw new Error(
      `stats_this_week returned the token total ${JSON.stringify(totals.tokensOut)}`
    );
  }
  if (!Number.isInteger(totals.projects) || totals.projects < 0) {
    throw new Error(
      `stats_this_week returned the project count ${JSON.stringify(totals.projects)}`
    );
  }
  if (totals.projects !== card.projects.length) {
    throw new Error(
      `stats_this_week counts ${totals.projects} projects but lists ${card.projects.length}`
    );
  }
  if (!isObject(totals.used)) throw new Error("stats_this_week returned no used values");
  if (
    totals.paid !== null &&
    (typeof totals.paid !== "number" || !Number.isFinite(totals.paid))
  ) {
    throw new Error(`stats_this_week returned the paid sum ${JSON.stringify(totals.paid)}`);
  }
  if (typeof totals.paidPartial !== "boolean") {
    throw new Error("stats_this_week did not say whether the paid sum is partial");
  }
  if (
    totals.usedOfPaid !== null &&
    (typeof totals.usedOfPaid !== "number" || !Number.isFinite(totals.usedOfPaid))
  ) {
    throw new Error(
      `stats_this_week returned the used share ${JSON.stringify(totals.usedOfPaid)}`
    );
  }
  if (!Array.isArray(totals.paidWithout)) {
    throw new Error("stats_this_week did not say what the paid sum leaves out");
  }
  if (totals.paid === null && (totals.paidPartial || totals.usedOfPaid !== null)) {
    throw new Error("stats_this_week has no paid sum but a partial sum or a share of it");
  }
  for (const project of card.projects) {
    if (!isObject(project)) throw new Error("stats_this_week returned a project that is not a row");
    if (typeof project.name !== "string" || project.name === "") {
      throw new Error("stats_this_week returned a project without a name");
    }
    if (
      typeof project.tokensOut !== "number" ||
      !Number.isFinite(project.tokensOut) ||
      project.tokensOut < 0
    ) {
      throw new Error(
        `the token count of ${JSON.stringify(project.name)} is a finite number of 0 or more`
      );
    }
    if (!isObject(project.values)) {
      throw new Error(`stats_this_week returned no values for ${JSON.stringify(project.name)}`);
    }
    if (!Array.isArray(project.bySubscription)) {
      throw new Error(
        `stats_this_week returned no subscription split for ${JSON.stringify(project.name)}`
      );
    }
  }
}

// The amount of what the running windows' weeks cost. A partial sum is a lower
// bound (R3): "≥" and the reason, so it never reads as the whole price.
function paidAmount(totals, currency) {
  const amount = amountNode(totals.paid, currency);
  if (!totals.paidPartial) return [amount];
  return [
    h("span", { title: "at least: a running window has no price" }, ["≥ ", amount]),
  ];
}

// The Used total's sub-line: the share and the paid sum, or the paid sum alone
// while the share is not known (R3: a partial used value or a partial paid sum
// has no share), or the reason no window has a price. A subscription whose work
// is in no record is named: neither side of the share counts it.
function usedSubline(totals, currency) {
  const parts = [];
  if (totals.paid === null) {
    parts.push("no price for this week's windows");
  } else {
    if (typeof totals.usedOfPaid === "number") {
      parts.push(`${percentText(totals.usedOfPaid)} of `);
    } else {
      parts.push("of ");
    }
    parts.push(...paidAmount(totals, currency));
    parts.push(" paid for this week");
  }
  if (totals.paidWithout.length > 0) {
    parts.push(
      h("span", {
        text: `, without ${totals.paidWithout.join(", ")}`,
        title: "its work is not recorded, so it counts neither as used nor as paid here",
      })
    );
  }
  return parts;
}

// The three totals on top of the card (R5).
function thisWeekTotals(totals, currency) {
  const boxes = h("div", { class: "stats-tw-totals" });
  boxes.appendChild(
    kpi(
      "Tokens",
      showTokens(h("span"), totals.tokensOut),
      `output, ${totals.projects} projects`,
      "Tokens | output tokens of every project this week's card counts"
    )
  );
  boxes.appendChild(
    kpi(
      "Used",
      valueNode(totals.used, "subscription", currency),
      usedSubline(totals, currency),
      "Used | subscription value of the running windows, of what their weeks cost"
    )
  );
  boxes.appendChild(
    kpi(
      "At API prices",
      valueNode(totals.used, "api", currency),
      "what the same work would cost without a subscription",
      "At API prices | the same work without a subscription"
    )
  );
  return boxes;
}

// One project's bar: as long as its known subscription value over the largest
// known part of the card's projects, split by subscription (R2). A project
// without a known part gets no length and the reason on its bar.
function projectBar(project, maxKnown, currency, env) {
  const length = h("span", { class: "stats-tw-length" });
  const known = knownSubscription(project);
  if (known === null) {
    const missing = valueText(project.values, "subscription", currency);
    length.style.width = "0%";
    length.title = missing.title;
    return h("span", { class: "stats-tw-spent", title: missing.title }, [length]);
  }
  const width = maxKnown > 0 ? (known / maxKnown) * 100 : 0;
  length.style.width = barWidth(width);
  if (project.bySubscription.length === 0) {
    length.appendChild(
      h("span", {
        class: "stats-split",
        title: "no subscription split: this project's API value is unknown",
      })
    );
  } else {
    length.appendChild(
      subscriptionSplit(project.bySubscription, env.accounts, (name, percent) =>
        `${name} · ${percentText(percent)} of this project's API value`
      )
    );
  }
  return h("span", { class: "stats-tw-spent" }, [length]);
}

// One project row, in the struct's order (Rust sorts it).
function projectRow(project, maxKnown, currency, env) {
  return h("div", { class: "stats-tw-grid stats-tw-row" }, [
    h("span", { class: "stats-tw-name", text: project.name, title: project.name }),
    projectBar(project, maxKnown, currency, env),
    showTokens(h("span", { class: "stats-tw-num" }), project.tokensOut),
    h("span", { class: "stats-tw-num stats-tw-money" }, [
      valueNode(project.values, "subscription", currency),
    ]),
    h("span", { class: "stats-tw-num stats-tw-api" }, [
      valueNode(project.values, "api", currency),
    ]),
  ]);
}

// The column headers of the project list.
function projectHead() {
  return h("div", { class: "stats-tw-grid stats-tw-head" }, [
    h("span", { text: "Project" }),
    h("span", { text: "Spent, by subscription" }),
    h("span", { class: "stats-tw-num", text: "Tokens" }),
    h("span", { class: "stats-tw-num", text: "Subscr." }),
    h("span", { class: "stats-tw-num", text: "API value" }),
  ]);
}

// The total row under the projects: the same figures over all of them.
function totalRow(totals, currency) {
  return h("div", { class: "stats-tw-grid stats-tw-total" }, [
    h("span", { class: "stats-tw-name stats-tw-muted", text: `all ${totals.projects}` }),
    h("span"),
    showTokens(h("span", { class: "stats-tw-num" }), totals.tokensOut),
    h("span", { class: "stats-tw-num stats-tw-money" }, [
      valueNode(totals.used, "subscription", currency),
    ]),
    h("span", { class: "stats-tw-num stats-tw-api" }, [
      valueNode(totals.used, "api", currency),
    ]),
  ]);
}

// The first eight projects, then the rest behind "Show all N projects ▾"; the
// open list scrolls inside the card, at the height of those eight rows.
function projectList(card, currency, env) {
  const maxKnown = card.projects.reduce(
    (largest, project) => Math.max(largest, knownSubscription(project) ?? 0),
    0
  );
  const rows = h("div", { class: "stats-tw-rows" });
  const projectRows = card.projects.map((project) =>
    projectRow(project, maxKnown, currency, env)
  );
  for (const [index, row] of projectRows.entries()) {
    row.hidden = index >= 8;
    rows.appendChild(row);
  }
  const main = h("div", { class: "stats-tw-main" }, [projectHead(), rows]);
  if (card.projects.length > 8) {
    const button = h("button", {
      type: "button",
      class: "stats-tw-more",
      "aria-expanded": "false",
    });
    let open = false;
    const paint = () => {
      for (const [index, row] of projectRows.entries()) {
        if (index >= 8) row.hidden = !open;
      }
      rows.classList.toggle("stats-tw-open", open);
      button.textContent = open
        ? "Show fewer ▴"
        : `Show all ${card.projects.length} projects ▾`;
      button.setAttribute("aria-expanded", String(open));
    };
    button.addEventListener("click", () => {
      open = !open;
      paint();
    });
    paint();
    main.appendChild(button);
  }
  main.appendChild(totalRow(card.totals, currency));
  return main;
}

// The subscription lines the card carries: every running account window, then
// every provider subscription the chips know with a running window.
function subscriptionEntries(card, env) {
  const entries = [];
  for (const account of card.accounts) {
    if (typeof account.id !== "string" || account.id === "") {
      throw new Error("stats_this_week returned an account row without an id");
    }
    if (typeof account.used !== "number" || !Number.isFinite(account.used)) {
      throw new Error(
        `the used percent of ${JSON.stringify(account.id)} is a finite number`
      );
    }
    if (typeof account.resetsAt !== "string") {
      throw new Error(`the running window of ${JSON.stringify(account.id)} has no reset`);
    }
    if (
      account.money !== null &&
      (typeof account.money !== "number" || !Number.isFinite(account.money))
    ) {
      throw new Error(`the money of ${JSON.stringify(account.id)} is a number or null`);
    }
    if (account.money === null && typeof account.moneyMissing !== "string") {
      throw new Error(`the missing money of ${JSON.stringify(account.id)} has a reason`);
    }
    entries.push({ source: account, id: account.id, provider: false });
  }
  for (const provider of card.providers) {
    if (typeof provider.name !== "string" || provider.name === "") {
      throw new Error("stats_this_week returned a provider row without a name");
    }
    const view = providerSubscription(provider, env);
    if (view === null) continue;
    if (provider.used === null || provider.used === undefined) continue;
    if (typeof provider.used !== "number" || !Number.isFinite(provider.used)) {
      throw new Error(
        `the used percent of ${JSON.stringify(provider.name)} is a finite number`
      );
    }
    if (typeof provider.resetsAt !== "string") {
      throw new Error(`the running window of ${JSON.stringify(provider.name)} has no reset`);
    }
    if (
      provider.money !== null &&
      (typeof provider.money !== "number" || !Number.isFinite(provider.money))
    ) {
      throw new Error(`the money of ${JSON.stringify(provider.name)} is a number or null`);
    }
    entries.push({ source: provider, id: view.id, provider: true });
  }
  return entries;
}

// One subscription line: its tag, plan, a use bar, the percent and money, and
// the reset. A provider without a price reads "no price"; an account without
// one keeps "—" with the reason.
function subscriptionLine(entry, currency, env) {
  const { source, id, provider } = entry;
  let money;
  if (provider && typeof source.money !== "number") {
    money = h("span", {
      text: "no price",
      title:
        typeof source.moneyMissing === "string"
          ? source.moneyMissing
          : "the subscription has no price",
    });
  } else {
    money = amountNode(source.money, currency);
    if (source.money === null && typeof source.moneyMissing === "string") {
      money.title = source.moneyMissing;
    }
  }
  const view = accountView(env.accounts, id);
  const color = view && isColor(view.color) ? view.color : "var(--focus)";
  return h("div", { class: "stats-tw-sub" }, [
    accountName(env.accounts, id),
    accountPlan(env.accounts, id),
    h("span", { class: "stats-ubar" }, [
      h("i", { style: { width: barWidth(source.used), background: color } }),
    ]),
    h("span", { class: "stats-tw-sub-money" }, [
      `${percentText(source.used)} · `,
      money,
    ]),
    h("span", {
      class: "stats-tw-sub-reset",
      text: `resets ${weekdayTimeText(source.resetsAt)}`,
      title: `resets ${dateTimeText(source.resetsAt)}`,
    }),
  ]);
}

// The right-hand block: one line per subscription with a running window and the
// note that explains the percent (R5).
function subscriptionsBlock(entries, currency, env) {
  const block = h("div", { class: "stats-tw-subs" }, [
    h("div", { class: "stats-tw-subs-title", text: "Subscriptions this week" }),
  ]);
  for (const entry of entries) block.appendChild(subscriptionLine(entry, currency, env));
  block.appendChild(
    h("p", {
      class: "stats-tw-subs-note",
      text: "Percent of each subscription's weekly limit; its window resets at the time shown.",
    })
  );
  return block;
}

// The This-week card: the three totals, the projects by subscription value, and
// the subscriptions' running windows beside them. It ignores the range.
function thisWeekCard(card, currency, env) {
  checkThisWeek(card);
  const node = h("div", { class: "hcard stats-tw-card" }, [
    h("div", { class: "ch" }, [
      h("span", { class: "t", text: "This week · so far" }),
      h("span", { class: "s", text: "each subscription's current window" }),
    ]),
  ]);
  const entries = subscriptionEntries(card, env);
  if (card.projects.length === 0 && entries.length === 0) {
    node.appendChild(
      h("p", {
        class: "stats-tw-empty",
        text: "No usage reading yet: this card fills in with the next usage poll.",
      })
    );
    return node;
  }
  node.appendChild(thisWeekTotals(card.totals, currency));
  const main =
    card.projects.length === 0
      ? h("div", { class: "stats-tw-main" }, [projectHead(), emptyLine("no project in this week's windows")])
      : projectList(card, currency, env);
  node.appendChild(
    h("div", { class: "stats-tw-body" }, [main, subscriptionsBlock(entries, currency, env)])
  );
  return node;
}

// A KPI value with its percent beside the money, as the mock-up writes it.
// The percent is a number the view carries; the page only formats it.
function percentFigure(amount, percent) {
  return h("span", {}, [
    amount,
    ...(typeof percent === "number"
      ? [h("span", { class: "stats-kpi-percent", text: percentText(percent) })]
      : []),
  ]);
}

export async function show(container, env) {
  // The card ignores the range, so a range that holds no record still shows
  // it, with the area's "nothing recorded" line below it instead of the rest.
  const [card, page] = await Promise.all([
    env.ask("stats_this_week"),
    env.call("stats_overview").then(
      (data) => ({ data, empty: null }),
      (error) => {
        if (error && error.name === "EmptyRange") return { data: null, empty: errorMessage(error) };
        throw error;
      }
    ),
  ]);
  if (
    !Array.isArray(card.accounts) ||
    !Array.isArray(card.providers) ||
    !Array.isArray(card.projects)
  ) {
    throw new Error("stats_this_week returned an unexpected card");
  }
  clear(container);
  const currency = env.currency;
  const data = page.data;
  // The card's projects first, so the projects running now take the first
  // colours; the weekly chart uses the same colour for the same project.
  const colours = projectColours([
    card.projects.map((project) => [project.name]),
    ...(data === null ? [] : data.weeks.map((week) => week.projects)),
  ]);

  const root = h("div", { class: "g stats-overview" });
  root.appendChild(thisWeekCard(card, currency, env));
  if (data === null) {
    root.appendChild(h("p", { class: "stats-empty stats-overview-empty", text: page.empty }));
    container.appendChild(root);
    return;
  }
  for (const week of data.weeks) {
    if (!Array.isArray(week.accounts) || !Array.isArray(week.projects)) {
      throw new Error("stats_overview returned a week without its accounts or projects");
    }
    if (typeof week.running !== "boolean") {
      throw new Error("stats_overview returned a week without its running mark");
    }
  }

  const kpis = h("div", { class: "stats-kpis" });
  const expiredRows = [];
  for (const [id, amount] of data.kpis.expiredByAccount) {
    if (expiredRows.length > 0) expiredRows.push(" · ");
    expiredRows.push(accountName(env.accounts, id), ` ${amountTip(amount, currency)}`);
  }
  if (expiredRows.length === 0) expiredRows.push("— · no account expired money in the range");
  const cost =
    typeof data.kpis.switchCostPercentOfWindow === "number"
      ? ` · a switch costs ${percentText(data.kpis.switchCostPercentOfWindow)} of its 5h window`
      : " · switch cost —";
  const weeks = data.kpis.weeks === 1 ? "week" : "weeks";
  kpis.appendChild(
    kpi(
      "Used",
      percentFigure(amountNode(data.kpis.used, currency), data.kpis.usedPercent),
      h("span", {}, [
        "of ",
        amountNode(data.kpis.paid, currency),
        ` paid for ${data.kpis.weeks} ${weeks}`,
      ]),
      "Used | money used of the priced weekly windows in the range, the running ones included; an account without a price is left out"
    )
  );
  kpis.appendChild(
    kpi(
      "Expired unused",
      percentFigure(amountNode(data.kpis.expired, currency), data.kpis.expiredPercent),
      expiredRows,
      "Expired unused | quota left at a weekly reset, in money; per account, largest first"
    )
  );
  if (!Array.isArray(data.kpis.blockedByCompany)) {
    throw new Error("stats_overview returned no blockedByCompany");
  }
  const blockedBefore = data.kpis.blockedBeforeAdded;
  const blockedSub = blockedBefore
    ? [
        hoursText(blockedBefore.hours),
        " of Anthropic's before ",
        blockedBefore.name,
        " was added",
      ]
    : "working hours each company was blocked";
  // One figure per company, each on its own line: a provider's blocked hours
  // are not Anthropic's, so they are never added up, and no figure is cut off.
  const blocked = data.kpis.blockedByCompany;
  const blockedValue =
    blocked.length === 0
      ? hoursText(0)
      : h(
          "span",
          {
            class: `stats-blocked-companies${blocked.length > 1 ? " stats-blocked-many" : ""}`,
          },
          blocked.map((entry) =>
            h("span", { class: "stats-blocked-company" }, [
              providerName(
                entry.company,
                "working hours blocked at 100 % of this company's limit"
              ),
              ` ${hoursText(entry.hours)}`,
            ])
          )
        );
  kpis.appendChild(
    kpi(
      "Blocked",
      blockedValue,
      blockedSub,
      "Blocked | working hours in which each company's account was at 100 % of its limit; one figure per company, never a sum",
      "stats-kpi-blocked"
    )
  );
  kpis.appendChild(
    kpi(
      "Avoidable",
      hoursText(data.kpis.avoidableHours),
      "blocked while other quota expired",
      "Avoidable | blocked hours in a week in which another account's quota expired",
      data.kpis.avoidableHours > 0 ? "stats-kpi-value-bad" : null
    )
  );
  kpis.appendChild(
    kpi(
      "Switches",
      String(data.kpis.switchesTotal),
      `${switchText(data.kpis.switches)}${cost}`,
      "Switches | every change of the live account, by who made it; the total of the five counts"
    )
  );

  const kpiCard = h("div", { class: "hcard stats-kpi-card" }, [kpis]);
  root.appendChild(kpiCard);

  const weeksCard = h("div", { class: "hcard stats-weeks-card" });
  const weeksHead = h("div", { class: "ch" }, [
    h("span", { class: "t", text: "Each week: by account, and by project" }),
    h("span", {
      class: "s",
      text: currency || "—",
      title: currency ? undefined : "no currency set in Settings",
    }),
    h("span", { class: "sp" }),
  ]);
  const legend = h("div", { class: "stats-legend" });
  for (const view of env.accounts) {
    legend.appendChild(accountName(env.accounts, view.id));
  }
  legend.appendChild(legendItem("var(--muted)", "expired", true));
  // The chart's own projects; the card's legend names the ones running now.
  const chartProjects = new Set(data.weeks.flatMap((week) => week.projects.map(([name]) => name)));
  if (chartProjects.size > 0) {
    legend.appendChild(h("span", { class: "stats-legend-sep", text: "│" }));
  }
  for (const [name, colour] of colours) {
    if (chartProjects.has(name)) legend.appendChild(legendItem(colour, name, false));
  }
  weeksHead.appendChild(legend);
  weeksCard.appendChild(weeksHead);

  const columns = data.weeks.map((week) => {
    // The history counts every weekly window of the range, the running ones
    // included; a running week's column and tips say so.
    const day = shortDate(week.week);
    const label = week.running === true ? `${day} · so far` : day;
    // The expired money is carried per account, not as one week total, so each
    // account's hatched expired piece stands above the used pieces of the stack.
    const expiredStack = [];
    const usedStack = [];
    for (const account of week.accounts) {
      const view = accountView(env.accounts, account.id);
      const color = view && isColor(view.color) ? view.color : "var(--muted)";
      const name = accountNameText(env.accounts, account.id);
      if (account.expired !== null && account.expired !== undefined) {
        expiredStack.push({
          value: account.expired,
          color: "var(--muted)",
          hatch: true,
          tip: `${name} · week from ${label} | expired ${percentText(
            account.expiredPercent
          )} = ${amountTip(account.expired, currency)}${
            account.uncertain ? " | uncertain: a reading near a reset is missing" : ""
          }`,
        });
      }
      if (account.used !== null && account.used !== undefined) {
        usedStack.push({
          value: account.used,
          color,
          tip: `${name} · week from ${label} | used ${percentText(
            account.usedPercent
          )} = ${amountTip(account.used, currency)}`,
        });
      }
    }
    const projectStack = week.projects.map(([name, value]) => ({
      value,
      color: colours.get(name) || "var(--muted)",
      tip: `${name} · week from ${label} | ${amountTip(value, currency)}`,
    }));
    const blocked = hoursText(week.blockedHours);
    return {
      label,
      at: week.week,
      // The short form when the long one has no room under a column.
      sub: [`${blocked} blocked`, blocked],
      stacks: [expiredStack.concat(usedStack), projectStack],
    };
  });

  const chartBody = h("div", { class: "stats-chart-body" });
  if (data.weeks.length === 0) {
    chartBody.appendChild(emptyLine("no week in the range"));
  } else if (data.weeks.every((week) => week.accounts.length === 0)) {
    // The history counts the running windows too, so this line only remains
    // for a range in which no weekly window has started.
    chartBody.appendChild(
      h("p", {
        class: "stats-empty",
        text: "No weekly window in this range yet.",
      })
    );
  } else {
    chartBody.appendChild(
      stackedColumns({
        columns,
        marks: data.marks.map((mark) => ({ at: mark.at, label: mark.label })),
      })
    );
  }
  weeksCard.appendChild(chartBody);
  root.appendChild(weeksCard);

  const side = h("div", { class: "stats-side" });
  const findingsCard = h("div", { class: "hcard" });
  findingsCard.appendChild(
    h("div", { class: "ch" }, [h("span", { class: "t", text: "What the numbers say" })])
  );
  const findingsBody = h("div", { class: "stats-body" });
  if (data.findings.length === 0) {
    findingsBody.appendChild(emptyLine("no finding in the range"));
  } else {
    const list = h("ul", { class: "stats-findings" });
    for (const finding of data.findings) {
      list.appendChild(h("li", { text: finding }));
    }
    findingsBody.appendChild(list);
  }
  findingsCard.appendChild(findingsBody);
  side.appendChild(findingsCard);

  const shareCard = h("div", { class: "hcard stats-share-card" });
  shareCard.appendChild(
    h("div", { class: "ch" }, [
      h("span", { class: "t", text: "Share that expired, per week" }),
      h("span", { class: "s", text: "of the value of all accounts" }),
    ])
  );
  const shareBody = h("div", { class: "stats-chart-body" });
  if (data.weeks.length === 0) {
    shareBody.appendChild(emptyLine("no week in the range"));
  } else if (data.weeks.every((week) => week.expiredShare === null)) {
    // A share needs a finished weekly window of an account with a price;
    // without any there is no line and no time axis to place the marks on.
    shareBody.appendChild(emptyLine("no finished weekly window with a price in the range yet"));
  } else {
    shareBody.appendChild(
      lines({
        series: [
          {
            name: "expired",
            color: "var(--warn)",
            area: true,
            points: data.weeks.map((week) => [week.week, week.expiredShare]),
            tip: (time, value) =>
              `expired | week from ${dateText(new Date(time))}: ${percentText(value)} of the value`,
          },
        ],
        yMax: 100,
        ySuffix: " %",
        marks: data.marks.map((mark) => ({ at: mark.at, label: mark.label })),
      })
    );
  }
  shareCard.appendChild(shareBody);
  side.appendChild(shareCard);
  root.appendChild(side);

  if (data.withoutPrice.length > 0) {
    const note = h("p", { class: "stats-note" }, ["No price for "]);
    for (const [index, id] of data.withoutPrice.entries()) {
      if (index > 0) note.appendChild(document.createTextNode(", "));
      note.appendChild(accountName(env.accounts, id));
    }
    note.appendChild(document.createTextNode(": left out of the money sums."));
    root.appendChild(note);
  }

  container.appendChild(root);
}
