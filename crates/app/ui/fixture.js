// The built-in fixture both pages fall back to in a browser without Tauri:
// what the pop-up and the large window show without the tray app.
// Moved out of panel.js unchanged; its state lives here, not in either page.

import { PALETTE, clone, contrastText, hexToRgb, isColor, isObject } from "./common.js";
import { fixtureStats } from "./fixture-stats.js";
import {
  fixtureEditProject,
  fixtureExpand,
  fixtureHistoryView,
  fixtureMoneyView,
  fixtureProjectView,
  fixtureSaveSetting,
  validateFixtureSettings,
} from "./fixture/settings.js";
import { fixtureSessionsView } from "./fixture/sessions.js";
import {
  FIXTURE_KINDS,
  fixtureFields,
  fixtureInstallState,
  fixturePresets,
  fixtureRecordedKeys,
  fixtureStatuslinePreview,
  sameFixtureValue,
} from "./fixture/statusline.js";

// The bench's one-shot refusal. The pages reach `fixtureInvoke` directly, so a
// browser step arms one command to be refused before it changes a control; it
// is test data for the bench, not a second behaviour of the app.
let pendingRefusal = null;

// The bench's time-text failure: `time_texts` throws this text until the page
// is loaded again, so the page's own "?" and its error message can be checked.
let timeTextsError = null;

export function refuseNextFixtureCall(command, text) {
  pendingRefusal = {
    command: String(command),
    text: typeof text === "string" && text !== "" ? text : null,
  };
}

export const DEFAULT_SETTINGS = {
  trayText: "percent",
  iconLimit: "binding",
  iconText: "letters",
  pollActiveSecs: 120,
  pollInactiveSecs: 900,
  autoBurn: false,
  suggestSwitches: true,
  notifyOnSwitch: true,
  mirrorLive: true,
  refreshInactive: true,
  hotkey: "Ctrl+Alt+C",
  startAtLogin: true,
  theme: "system",
  claudeConfigDir: null,
  historyDir: null,
  codexCard: false,
  codexHome: null,
  ollamaCard: false,
  googleCard: true,
  ollamaUrl: "http://127.0.0.1:11434",
  hostName: null,
  readSessions: true,
  notifyUsableAgain: true,
  notifyWaiting: true,
  notifyWaitingHosts: "thisHost",
  notifyLoginEnds: true,
  agentsRead: true,
  agentsSwitch: false,
  statuslineFeed: true,
  statuslineRecordUntil: null,
  statuslineInstall: null,
  currency: "EUR",
  lookUpPrices: true,
  statusline: {
    left: [
      { kind: "userHost", color: { rule: "fixed", color: "#22c55e" } },
      { kind: "text", color: { rule: "none" }, text: ":" },
      { kind: "folder", color: { rule: "fixed", color: "#3b82f6" } },
      { kind: "model", color: { rule: "byModelFamily" }, prefix: " " },
      { kind: "effort", color: { rule: "byEffort" }, prefix: " " },
    ],
    right: [
      { kind: "account", color: { rule: "account" }, suffix: " " },
      {
        kind: "fiveHourPercent",
        color: { rule: "byLoad", warn: 50, bad: 80 },
        suffix: "%",
        drop: 2,
      },
      {
        kind: "fiveHourReset",
        color: { rule: "none" },
        prefix: " ",
        drop: 1,
        timeFormat: "compactLeft",
      },
      {
        kind: "weekPercent",
        color: { rule: "byLoad", warn: 50, bad: 80 },
        prefix: "  ",
        suffix: "%",
      },
      {
        kind: "weekReset",
        color: { rule: "none" },
        prefix: " ",
        drop: 1,
        timeFormat: "compactLeft",
      },
    ],
    margin: 4,
  },
};

let fixture = null;

// Made once, on the first call, and shared by every module that asks.
export function ensureFixture() {
  if (!fixture) {
    fixture = makeFixture();
    fixture.providers = fixtureOpenProviders(fixture);
    exposeFixtureHooks();
    console.info("[fixture] no window.__TAURI__; using the built-in snapshot");
  }
  return fixture;
}

// The provider views whose card the fixture's settings switch on, as
// gui/providers_rt.rs sends only the switched-on providers, in its order.
export function fixtureOpenProviders(fixture) {
  const settings = fixture.settings || {};
  const open = [];
  for (const view of fixture.providerCardViews) {
    let on;
    if (view.provider === "codex") on = settings.codexCard;
    else if (view.provider === "ollama") on = settings.ollamaCard;
    else if (view.provider === "google") on = settings.googleCard;
    else throw new Error(`unknown provider ${JSON.stringify(view.provider)}`);
    if (typeof on !== "boolean") {
      throw new Error(`the ${view.provider} card setting must be true or false, got ${on}`);
    }
    if (on) open.push(view);
  }
  return open;
}

function fixtureProviderView(fixture, provider) {
  const name = String(provider);
  const view = fixture.providerCardViews.find((entry) => entry.provider === name);
  if (!view) throw new Error(`no provider ${name}`);
  return view;
}

// The bench flips these from the page: a failed provider poll that keeps the
// last numbers, a 24 h spend that could not be read, and both provider cards
// switched off through the settings save. The page announces the change so the
// open view is drawn again.
function exposeFixtureHooks() {
  if (typeof window === "undefined") return;
  window.__fixture = {
    providerError(provider, text) {
      if (!fixture) throw new Error("fixture is not ready");
      if (typeof text !== "string" || text === "") {
        throw new Error(`an error text is needed, not ${JSON.stringify(text)}`);
      }
      const view = fixtureProviderView(fixture, provider);
      view.error = text;
      if (view.usage || view.credits) {
        view.fetchedAt = isoIn(-6 * 60000);
      }
      publishFixtureChange();
      return view.error;
    },
    spendError(text) {
      if (!fixture) throw new Error("fixture is not ready");
      if (typeof text !== "string" || text === "") {
        throw new Error(`a spend error text is needed, not ${JSON.stringify(text)}`);
      }
      const view = fixtureProviderView(fixture, "ollama");
      view.spend24h = null;
      view.spend24hError = text;
      publishFixtureChange();
      return view.spend24hError;
    },
    timeTextsFail(text) {
      if (!fixture) throw new Error("fixture is not ready");
      if (typeof text !== "string" || text === "") {
        throw new Error(`a time-text error is needed, not ${JSON.stringify(text)}`);
      }
      timeTextsError = text;
      return timeTextsError;
    },
    providersOff() {
      if (!fixture) throw new Error("fixture is not ready");
      fixtureSaveSetting(fixture, "codexCard", false);
      fixtureSaveSetting(fixture, "ollamaCard", false);
      fixtureSaveSetting(fixture, "googleCard", false);
      publishFixtureChange();
      return "off";
    },
    // The bench's money view override: fields the real look-up thread could
    // send but `fixtureMoneyView` would otherwise fill in from the settings or
    // the price list. `null` puts the fixture back.
    moneyView(patch) {
      if (!fixture) throw new Error("fixture is not ready");
      if (patch === null) {
        delete fixture.moneyOverride;
        return null;
      }
      if (!isObject(patch)) {
        throw new Error(`a money view patch is needed, not ${JSON.stringify(patch)}`);
      }
      fixture.moneyOverride = clone(patch);
      return clone(fixture.moneyOverride);
    },
  };
}

function publishFixtureChange() {
  if (typeof window === "undefined") return;
  window.dispatchEvent(new CustomEvent("seatswap:fixture"));
}

// The snapshot the fixture commands change, or null before `ensureFixture`.
export function currentFixture() {
  return fixture;
}

const GLYPHS = {
  A: ["010", "101", "111", "101", "101"],
  B: ["110", "101", "110", "101", "110"],
  C: ["011", "100", "100", "100", "011"],
  D: ["110", "101", "101", "101", "110"],
  E: ["111", "100", "110", "100", "111"],
  F: ["111", "100", "110", "100", "100"],
  G: ["011", "100", "101", "101", "011"],
  H: ["101", "101", "111", "101", "101"],
  I: ["111", "010", "010", "010", "111"],
  J: ["001", "001", "001", "101", "010"],
  K: ["101", "101", "110", "101", "101"],
  L: ["100", "100", "100", "100", "111"],
  M: ["101", "111", "111", "101", "101"],
  N: ["110", "101", "101", "101", "101"],
  O: ["010", "101", "101", "101", "010"],
  P: ["110", "101", "110", "100", "100"],
  Q: ["010", "101", "101", "110", "011"],
  R: ["110", "101", "110", "101", "101"],
  S: ["011", "100", "010", "001", "110"],
  T: ["111", "010", "010", "010", "010"],
  U: ["101", "101", "101", "101", "111"],
  V: ["101", "101", "101", "101", "010"],
  W: ["101", "101", "111", "111", "101"],
  X: ["101", "101", "010", "101", "101"],
  Y: ["101", "101", "010", "010", "010"],
  Z: ["111", "001", "010", "100", "111"],
  0: ["111", "101", "101", "101", "111"],
  1: ["010", "110", "010", "010", "111"],
  2: ["110", "001", "010", "100", "111"],
  3: ["110", "001", "010", "001", "110"],
  4: ["101", "101", "111", "001", "001"],
  5: ["111", "100", "110", "001", "110"],
  6: ["011", "100", "110", "101", "010"],
  7: ["111", "001", "010", "010", "010"],
  8: ["010", "101", "010", "101", "010"],
  9: ["010", "101", "011", "001", "110"],
};

function fixtureId() {
  if (typeof crypto !== "undefined" && typeof crypto.randomUUID === "function") {
    return crypto.randomUUID();
  }
  return "xxxxxxxx-xxxx-4xxx-yxxx-xxxxxxxxxxxx".replace(/[xy]/g, (character) => {
    const random = Math.floor(Math.random() * 16);
    const value = character === "x" ? random : (random & 0x3) | 0x8;
    return value.toString(16);
  });
}

export function isoIn(milliseconds) {
  return new Date(Date.now() + milliseconds).toISOString();
}

function makeFixture() {
  const day = 86400000;
  const hour = 3600000;
  // Ollama's monthly allowance resets on the third of the coming month.
  const nextMonth = new Date();
  nextMonth.setMonth(nextMonth.getMonth() + 1, 3);
  nextMonth.setHours(0, 0, 0, 0);
  const untilNextMonth = nextMonth.toISOString();
  const acmeId = "5d1c2a9b-e0d3-4c11-9e21-7c8f1a2b3c4d";
  const labId = "8b4e7f20-6a1d-4d33-b8c5-2f9a0e6d7c81";
  const privateId = "c0a1b2d3-e4f5-4a6b-8c7d-9e0f1a2b3c4d";
  const codexAccountId = "0f3a6c2e-7b1d-4e9a-8c5f-2d4b6a8e0c13";
  const codexSavedId = "7d2b9e4c-1a3f-4c5e-8b6d-9f0a1c2e3d4b";
  return {
    // The folder the app runs with (the Snapshot's field); the General group
    // shows it as the empty config-folder field's placeholder.
    claudeConfigDir: "~/.claude",
    views: [
      {
        id: privateId,
        label: "Private",
        abbr: "PR",
        color: "#8b5cf6",
        email: "sam.private@mail.example",
        plan: "Ultra",
        autoBurn: true,
        isActive: false,
        loginState: "ok",
        loginEndsAtMs: Date.now() + 4 * day,
        loginDaysLeft: 4,
        usage: {
          limits: [
            {
              kind: "weekly_all",
              percent: 70,
              severity: "normal",
              resetsAt: isoIn(3 * day + 5 * hour),
              model: null,
              isActive: true,
            },
            {
              kind: "weekly_scoped",
              percent: 22,
              severity: "normal",
              resetsAt: isoIn(3 * day),
              model: "Opus",
              isActive: false,
            },
            // An Other limit keeps the card's generic row in the fixture too.
            {
              kind: { other: "daily_tokens" },
              percent: 42,
              severity: "normal",
              resetsAt: isoIn(6 * hour),
              model: null,
              isActive: false,
            },
          ],
          extraUsageEnabled: false,
        },
        usable: true,
        blockedUntil: null,
        fetchedAt: isoIn(-8 * 60000),
        lastError: null,
      },
      {
        id: labId,
        label: "Lab",
        abbr: "LB",
        color: "#10b981",
        email: "sam@lab.example",
        plan: "Max",
        autoBurn: true,
        isActive: false,
        loginState: "ok",
        loginEndsAtMs: Date.now() + 12 * day,
        loginDaysLeft: 12,
        usage: {
          limits: [
            {
              kind: "weekly_all",
              percent: 40,
              severity: "normal",
              resetsAt: isoIn(5 * day + 3 * hour),
              model: null,
              isActive: true,
            },
            {
              kind: "weekly_scoped",
              percent: 21,
              severity: "normal",
              resetsAt: isoIn(5 * day),
              model: "Fable",
              isActive: false,
            },
          ],
          extraUsageEnabled: false,
        },
        usable: true,
        blockedUntil: null,
        fetchedAt: isoIn(-60000),
        lastError: null,
      },
      {
        id: acmeId,
        label: "Acme",
        abbr: "AC",
        color: "#3b82f6",
        email: "sam@acme.example",
        plan: "Team Premium",
        autoBurn: true,
        isActive: true,
        loginState: "ok",
        loginEndsAtMs: Date.now() + 41 * day,
        loginDaysLeft: 41,
        usage: {
          limits: [
            {
              kind: "session",
              percent: 97,
              severity: "critical",
              resetsAt: isoIn(72 * 60000),
              model: null,
              isActive: true,
            },
            {
              kind: "weekly_all",
              percent: 48,
              severity: "normal",
              resetsAt: isoIn(5 * day + 3 * hour),
              model: null,
              isActive: false,
            },
            {
              kind: "weekly_scoped",
              percent: 43,
              severity: "normal",
              resetsAt: isoIn(5 * day),
              model: "Fable",
              isActive: false,
            },
          ],
          extraUsageEnabled: false,
        },
        usable: true,
        blockedUntil: null,
        fetchedAt: isoIn(-12000),
        lastError: null,
      },
    ],
    // The stored accounts behind the views, as `gui/worker.rs`'s `AccountRow`:
    // the Accounts group needs `orgName`, `price` and `priceCurrency`, which an
    // `AccountView` does not hold.
    accounts: [
      {
        id: privateId,
        label: "Private",
        abbr: "PR",
        color: "#8b5cf6",
        email: "sam.private@mail.example",
        orgName: "sam",
        plan: "Ultra",
        price: 180.0,
        priceCurrency: "USD",
        autoBurn: true,
        loginEndsAtMs: Date.now() + 4 * day,
        loginDaysLeft: 4,
        isActive: false,
      },
      {
        id: labId,
        label: "Lab",
        abbr: "LB",
        color: "#10b981",
        email: "sam@lab.example",
        orgName: "sam",
        plan: "Max",
        price: null,
        priceCurrency: null,
        autoBurn: true,
        loginEndsAtMs: Date.now() + 12 * day,
        loginDaysLeft: 12,
        isActive: false,
      },
      {
        id: acmeId,
        label: "Acme",
        abbr: "AC",
        color: "#3b82f6",
        email: "sam@acme.example",
        orgName: "acme",
        plan: "Team Premium",
        price: 320.0,
        priceCurrency: "EUR",
        autoBurn: true,
        loginEndsAtMs: Date.now() + 41 * day,
        loginDaysLeft: 41,
        isActive: true,
      },
    ],
    // The provider subscriptions under the Claude accounts (spec R4): OpenAI
    // and Google with the provider's plan and a monthly price, Ollama without
    // either.
    providerSubscriptions: [
      {
        account: { acc: codexAccountId, org: "codex" },
        company: "openai",
        label: "OpenAI",
        abbr: "OA",
        color: "#db2777",
        plan: "Plus",
        price: 23.0,
        priceCurrency: "EUR",
      },
      {
        account: { acc: codexSavedId, org: "codex" },
        company: "openai",
        label: "OpenAI",
        abbr: "O2",
        color: "#db2777",
        plan: "Pro",
        price: null,
        priceCurrency: null,
      },
      {
        account: { acc: "ollama", org: "ollama" },
        company: "ollama",
        label: "Ollama",
        abbr: "OL",
        color: "#ea580c",
        plan: null,
        price: null,
        priceCurrency: null,
      },
      {
        account: { acc: "sam@mail.example", org: "google" },
        company: "google",
        label: "Google",
        abbr: "GO",
        color: "#0d9488",
        plan: "Google AI Pro",
        price: 21.99,
        priceCurrency: "EUR",
      },
    ],
    activeId: acmeId,
    live: { kind: "known", accountId: acmeId },
    burn: {
      on: true,
      pausedUntil: null,
      early: true,
      why: "switches early whenever it pays",
      line: [
        { text: "On " },
        { account: acmeId, abbr: "AC" },
        { text: " until its 5h window is full, about 2h. Then " },
        { account: labId, abbr: "LB" },
        { text: "." },
      ],
      planned: { [labId]: isoIn(2 * hour) },
      waiting: { to: privateId, latest: isoIn(40 * 60000), tiedBusy: 1 },
      suggestion: null,
      measured: {
        windowPercentOfWeek: {
          [privateId]: 8.6,
          [labId]: 9.4,
          [acmeId]: 10.2,
        },
        switchCostPercentOfWindow: 0.8,
        switchesPerDay: 5.6,
      },
    },
    skill: {
      path: "~/.claude/skills/seatswap/SKILL.md",
      installed: false,
      differs: false,
    },
    agentCalls: [
      {
        at: isoIn(-30 * 60000),
        command: "status --json",
        cwd: "~/projects/ledger",
        allowed: true,
      },
      {
        at: isoIn(-26 * hour),
        command: "use PR",
        cwd: "~/projects/ledger",
        allowed: false,
      },
      {
        at: isoIn(-50 * hour),
        command: "status --json --refresh",
        cwd: "~/projects/acme-kb",
        allowed: true,
      },
    ],
    warnings: [{ kind: "soon", id: privateId, daysLeft: 4 }],
    pending: null,
    secretsWarning: null,
    lastPollAt: isoIn(-12000),
    error: null,
    sessions: {
      running: 9,
      busy: 2,
      tied: 1,
      tiedBusy: 1,
      waiting: [
        {
          session: "7c1e4b2a-9d3f-4e6a-b8c5-2f0a1d7e9b34",
          label: "acme-kb-dd",
          project: "acme-kb",
          since: isoIn(-4 * 60000),
          host: null,
        },
      ],
      unavailable: null,
      warning: null,
    },
    // The saved Codex logins (R8): one entry per login, live first, each with
    // its own usage. `codexLogins` is what the snapshot carries while the OpenAI
    // card is on; the live row takes its numbers from that card, since one
    // login's poll fills both.
    codexLoginRows: [
      {
        account: { acc: codexAccountId, org: "codex" },
        abbr: "OA",
        label: "OpenAI",
        color: "#db2777",
        plan: "Plus",
        live: true,
        usage: null,
        state: "ok",
        message: null,
        removeNote: "the live login: switch Codex to another login first to remove it",
      },
      {
        account: { acc: codexSavedId, org: "codex" },
        abbr: "O2",
        label: "OpenAI",
        color: "#db2777",
        plan: "Pro",
        live: false,
        usage: {
          limits: [
            {
              kind: "session",
              percent: 3,
              severity: "normal",
              resetsAt: isoIn(4 * hour + 45 * 60000),
              model: null,
              isActive: false,
            },
            {
              kind: "weekly_all",
              percent: 20,
              severity: "normal",
              resetsAt: isoIn(3 * day + 16 * hour),
              model: null,
              isActive: true,
            },
          ],
          extraUsageEnabled: false,
        },
        state: "ok",
        message: null,
      },
    ],
    get codexLogins() {
      const settings = this.settings || {};
      if (!settings.codexCard) return [];
      const card = this.providerCardViews.find((view) => view.provider === "codex");
      if (!card) throw new Error("the fixture has no Codex card view");
      return this.codexLoginRows.map((row) => {
        const login = clone(row);
        if (login.live) {
          login.plan = card.plan;
          login.usage = card.usage ? clone(card.usage) : null;
        }
        return login;
      });
    },
    // Every provider view the fixture keeps (spec §7): a Codex Plus login and
    // an Ollama cloud plan, as gui/providers_rt.rs can send them. `providers`
    // is the list its settings open, as Rust sends only the switched-on ones.
    providerCardViews: [
      {
        provider: "codex",
        title: "OpenAI (Codex)",
        plan: "Plus",
        usage: {
          limits: [
            {
              kind: "session",
              percent: 12,
              severity: "normal",
              resetsAt: isoIn(2 * hour + 15 * 60000),
              model: null,
              isActive: false,
            },
            {
              kind: "weekly_all",
              percent: 48,
              severity: "normal",
              resetsAt: isoIn(5 * day + 3 * hour),
              model: null,
              isActive: true,
            },
          ],
          extraUsageEnabled: false,
        },
        credits: null,
        spend24h: null,
        spend24hError: null,
        fetchedAt: isoIn(0),
        error: null,
      },
      {
        provider: "ollama",
        title: "Ollama",
        plan: null,
        usage: {
          limits: [
            {
              kind: { other: "monthly_credits" },
              percent: 29.2,
              severity: "normal",
              resetsAt: untilNextMonth,
              model: null,
              isActive: true,
            },
          ],
          extraUsageEnabled: false,
        },
        credits: {
          left: 212.4,
          allowance: 300,
          until: untilNextMonth,
          purchased: 0,
        },
        spend24h: 3.18,
        spend24hError: null,
        fetchedAt: isoIn(0),
        error: null,
      },
      {
        provider: "google",
        title: "Google (Antigravity)",
        plan: "Google AI Pro",
        email: "sam@mail.example",
        usage: {
          limits: [
            {
              kind: { other: "5h Claude/GPT" },
              percent: 75,
              severity: "normal",
              resetsAt: isoIn(3 * hour),
              model: null,
              isActive: true,
            },
            {
              kind: { other: "week Claude/GPT" },
              percent: 12,
              severity: "normal",
              resetsAt: isoIn(6 * day + 4 * hour),
              model: null,
              isActive: false,
            },
            {
              kind: "weekly_all",
              percent: 25,
              severity: "normal",
              resetsAt: isoIn(4 * day + 20 * hour),
              model: null,
              isActive: false,
            },
          ],
          extraUsageEnabled: false,
        },
        credits: null,
        spend24h: null,
        spend24hError: null,
        fetchedAt: isoIn(-2 * hour),
        error: null,
        notRunning: false,
        notes: ["5h was reset since the last reading"],
        canConnect: false,
      },
    ],
    providers: [],
    money: {
      currency: "EUR",
      rate: { value: 0.9048, date: "2026-11-13" },
      lookUp: true,
      modelsWithPrice: 4,
      pricesCheckedAt: isoIn(-2 * 3600000),
      lastChange: {
        at: isoIn(-6 * 86400000),
        added: ["openai/gpt-6"],
        changed: ["anthropic/claude-haiku-5-5"],
        removed: [],
      },
      leftOut: ["anthropic/claude-x-9: the input cost is not a number"],
      rateError: null,
      lastError: null,
    },
    priceList: [
      {
        key: "anthropic/claude-opus-5-5",
        price: { input: 4, output: 20, cacheWrite: 5, cacheRead: 0.2, tiered: false },
        tiered: false,
        since: isoIn(-30 * 86400000),
      },
      {
        key: "anthropic/claude-haiku-5-5",
        price: { input: 0.1, output: 0.5, cacheWrite: 0.125, cacheRead: 0.01, tiered: true },
        tiered: true,
        since: isoIn(-30 * 86400000),
      },
      {
        key: "openai/gpt-6",
        price: { input: 2, output: 10, cacheWrite: 2.5, cacheRead: 0.2, tiered: false },
        tiered: false,
        since: isoIn(-12 * 86400000),
      },
      {
        key: "ollama-cloud/deepseek-v4.1-flash",
        price: { input: 0.3, output: 1.2, cacheWrite: 0, cacheRead: 0, tiered: false },
        tiered: false,
        since: isoIn(-5 * 86400000),
      },
    ],
    // The Projects group (spec §3.6): a fixed view the two project commands
    // read and update. The real app resolves the shared log in Rust.
    projectView: {
      projects: [
        {
          key: { kind: "repo", id: "github.com/acme/acme-kb" },
          name: "acme-kb",
          repo: "github.com/acme/acme-kb",
          org: "acme",
          joined: [],
          folders: { atlas: ["~/projects/acme-kb"] },
          sessions: 212,
        },
        {
          key: { kind: "repo", id: "github.com/acme/acme-notes" },
          name: "acme-notes",
          repo: "github.com/acme/acme-notes",
          org: "acme",
          joined: [],
          folders: { atlas: ["~/projects/acme-notes"] },
          sessions: 41,
        },
        {
          key: { kind: "repo", id: "github.com/sam/atlas" },
          name: "atlas",
          repo: "github.com/sam/atlas",
          org: "sam",
          joined: [],
          folders: {
            atlas: ["~/projects/atlas"],
            vega: ["~/src/atlas"],
          },
          sessions: 268,
        },
        {
          key: { kind: "repo", id: "github.com/sam/ledger" },
          name: "ledger",
          repo: "github.com/sam/ledger",
          org: "sam",
          joined: [],
          folders: {
            atlas: ["~/projects/ledger"],
            vega: ["~/ledger"],
          },
          sessions: 97,
        },
        {
          key: { kind: "repo", id: "github.com/sam/seatswap" },
          name: "seatswap",
          repo: "github.com/sam/seatswap",
          org: "sam",
          joined: [],
          folders: { atlas: ["~/projects/seatswap"] },
          sessions: 63,
        },
        {
          key: { kind: "folder", id: "~", host: "atlas" },
          name: "~ (home folder)",
          repo: null,
          org: null,
          joined: [],
          folders: { atlas: ["~"] },
          sessions: 12,
        },
        {
          key: { kind: "folder", id: "~", host: "vega" },
          name: "~ (home folder)",
          repo: null,
          org: null,
          joined: [],
          folders: { vega: ["~"] },
          sessions: 10,
        },
      ],
      orgs: [
        {
          id: "acme",
          name: "acme",
          accounts: [{ acc: acmeId, org: "org:acme" }],
          projects: 2,
        },
        {
          id: "sam",
          name: "sam",
          accounts: [
            { acc: privateId, org: "org:sam" },
            { acc: labId, org: "org:sam" },
          ],
          projects: 3,
        },
      ],
    },
    // The History and hosts group (spec §1.5, §3.7): the host folders of the
    // history, and the files an export in this browser has "written".
    history: {
      host: "atlas",
      dir: "~/Sync/seatswap-history",
      hosts: [
        {
          host: "atlas",
          thisHost: true,
          firstRecord: isoIn(-100 * day),
          lastRecord: isoIn(-2 * 60000),
          bytes: 3040870,
        },
        {
          host: "vega",
          thisHost: false,
          firstRecord: isoIn(-72 * day),
          lastRecord: isoIn(-3 * hour),
          bytes: 1258291,
        },
      ],
      bytes: 4299161,
    },
    exports: [],
    // The status line builder's browser state: what Claude Code's settings
    // would hold, and the field names a recording found. The fixture starts
    // with another line installed, as the task's bench does.
    statuslineKeys: null,
    statuslineClaude: {
      current: { type: "command", command: "bash ~/x.sh", refreshInterval: 2 },
      ours: {
        type: "command",
        command: '"/home/sam/apps/seatswap" statusline',
        refreshInterval: 2,
      },
      settingsFile: "~/.claude/settings.json",
    },
    settings: Object.assign({}, DEFAULT_SETTINGS, {
      autoBurn: true,
      suggestSwitches: true,
      codexCard: true,
      ollamaCard: true,
    }),
  };
}

function fixtureUniqueAbbr(fixture, label) {
  const used = new Set(fixture.views.map((view) => view.abbr));
  const clean = String(label || "").toUpperCase().replace(/[^A-Z0-9]/g, "");
  const candidates = [];
  if (clean.length >= 2) candidates.push(clean.slice(0, 2));
  if (clean.length >= 1) candidates.push(clean[0]);
  if (clean.length >= 2) candidates.push(clean[0] + clean[clean.length - 1]);
  for (const candidate of candidates) {
    if (candidate && candidate.length <= 2 && !used.has(candidate)) return candidate;
  }
  for (const character of "ABCDEFGHIJKLMNOPQRSTUVWXYZ0123456789") {
    if (!used.has(character)) return character;
  }
  return "X";
}

function fixtureCreateFromLive(fixture, labelValue) {
  const label = String(labelValue || "").trim().slice(0, 40);
  if (!label) throw new Error("invalid label: use 1 to 40 characters");
  const color = PALETTE.find((candidate) => !fixture.views.some((view) => view.color === candidate));
  const id = fixtureId();
  const abbr = fixtureUniqueAbbr(fixture, label);
  const loginEndsAtMs = Date.now() + 30 * 86400000;
  const account = {
    id,
    label,
    abbr,
    color: color || PALETTE[0],
    email: fixture.live.email || null,
    orgName: fixture.live.orgName || null,
    plan: "Ultra",
    price: null,
    priceCurrency: null,
    autoBurn: true,
    loginEndsAtMs,
    loginDaysLeft: 30,
    isActive: false,
  };
  const view = {
    id,
    label,
    abbr,
    color: account.color,
    email: account.email,
    plan: account.plan,
    autoBurn: account.autoBurn,
    isActive: false,
    loginState: "ok",
    loginEndsAtMs,
    loginDaysLeft: 30,
    usage: null,
    usable: true,
    blockedUntil: null,
    fetchedAt: null,
    lastError: null,
  };
  fixture.accounts.push(account);
  fixture.views.push(view);
  for (const entry of fixture.views) entry.isActive = entry.id === id;
  for (const entry of fixture.accounts) entry.isActive = entry.id === id;
  fixture.activeId = id;
  fixture.live = { kind: "known", accountId: id };
  if (fixture.burn) fixture.burn.suggestion = null;
  fixture.lastPollAt = new Date().toISOString();
  return account;
}

function fixtureIcon(abbr, color, size) {
  const pixels = new Uint8ClampedArray(size * size * 4);
  const clean = String(abbr || "?")
    .toUpperCase()
    .replace(/[^A-Z0-9]/g, "")
    .slice(0, 2) || "?";
  const disc = hexToRgb(color) || { r: 59, g: 130, b: 246 };
  const textColor = contrastText(color) === "#0f1115"
    ? { r: 15, g: 17, b: 21 }
    : { r: 255, g: 255, b: 255 };
  const centre = (size - 1) / 2;
  const radius = size * 0.46;
  const gap = Math.max(1, Math.round(size / 22));
  let unit = Math.max(1, Math.floor(size / 9));
  const textWidth = (count) => count * 3 * unit + (count - 1) * gap;
  while (unit > 1 && textWidth(clean.length) > radius * 1.35) {
    unit -= 1;
  }
  const totalWidth = textWidth(clean.length);
  const totalHeight = 5 * unit;
  const startX = centre - (totalWidth - 1) / 2;
  const startY = centre - (totalHeight - 1) / 2;

  const glyphHit = (x, y) => {
    for (let index = 0; index < clean.length; index += 1) {
      const offsetX = index * (3 * unit + gap);
      const localX = x - offsetX;
      if (localX >= 0 && localX < 3 * unit) {
        const column = Math.floor(localX / unit);
        const row = Math.floor(y / unit);
        if (row >= 0 && row < 5) {
          const rows = GLYPHS[clean[index]];
          if (rows && rows[row] && rows[row][column] === "1") return true;
        }
      }
    }
    return false;
  };

  for (let y = 0; y < size; y += 1) {
    for (let x = 0; x < size; x += 1) {
      let hits = 0;
      let red = 0;
      let green = 0;
      let blue = 0;
      for (let sy = 0; sy < 2; sy += 1) {
        for (let sx = 0; sx < 2; sx += 1) {
          const px = x + (sx + 0.5) / 2;
          const py = y + (sy + 0.5) / 2;
          const dx = px - centre;
          const dy = py - centre;
          if (dx * dx + dy * dy > radius * radius) continue;
          const localX = px - startX;
          const localY = py - startY;
          const onGlyph =
            localX >= 0 &&
            localY >= 0 &&
            localX < totalWidth &&
            localY < totalHeight &&
            glyphHit(localX, localY);
          const colorAt = onGlyph ? textColor : disc;
          hits += 1;
          red += colorAt.r;
          green += colorAt.g;
          blue += colorAt.b;
        }
      }
      if (hits === 0) continue;
      const offset = (y * size + x) * 4;
      pixels[offset] = Math.round(red / hits);
      pixels[offset + 1] = Math.round(green / hits);
      pixels[offset + 2] = Math.round(blue / hits);
      pixels[offset + 3] = Math.round((hits / 4) * 255);
    }
  }
  return pixels;
}

// The fixture's money view with the bench's last override on top, so a step can
// show what the page does with a malformed view.
function fixtureMoneyWithOverride(fixture) {
  const view = fixtureMoneyView(fixture);
  return isObject(fixture.moneyOverride)
    ? Object.assign(view, clone(fixture.moneyOverride))
    : view;
}

export async function fixtureInvoke(command, args) {
  if (pendingRefusal && pendingRefusal.command === command) {
    const refusal = pendingRefusal;
    pendingRefusal = null;
    throw new Error(refusal.text || `${command} refused by the bench step`);
  }
  console.log(`[fixture] ${command}`, args);
  if (String(command).startsWith("stats_")) return fixtureStats(command, args);
  const fixture = currentFixture();
  if (!fixture && command !== "fit_panel" && command !== "hide_panel") {
    throw new Error("fixture is not ready");
  }

  switch (command) {
    case "get_snapshot": {
      const snapshot = clone(fixture);
      delete snapshot.projectView;
      // The stored views and accounts stay in the fixture; the snapshot carries
      // the lists the Rust one does.
      delete snapshot.providerCardViews;
      return snapshot;
    }

    case "app_version":
      // A made-up version for the page pictures; the app answers with its crate version.
      return "1.3.0";

    case "refresh_now":
      fixture.lastPollAt = new Date().toISOString();
      for (const view of fixture.views) {
        if (view.usage) view.fetchedAt = fixture.lastPollAt;
      }
      return null;

    case "switch_account": {
      const key = String(args.key || "");
      const unsaved = args.unsaved || "refuse";
      const label = args.label === undefined || args.label === null ? null : String(args.label);
      const target = fixture.views.find(
        (view) => view.id === key || String(view.abbr).toLowerCase() === key.toLowerCase()
      );
      if (!target) throw new Error(`unknown account ${key}`);
      if (target.loginState === "loggedOut") {
        throw new Error(`${target.label} is logged out; log in again first`);
      }
      if (fixture.live.kind === "unknown" && unsaved === "refuse") {
        fixture.pending = {
          targetId: target.id,
          targetLabel: target.label,
          email: fixture.live.email || null,
          orgName: fixture.live.orgName || null,
        };
        throw new Error("not saved");
      }
      if (fixture.live.kind === "unknown" && unsaved === "save") {
        fixtureCreateFromLive(fixture, label);
      }
      fixture.pending = null;
      for (const view of fixture.views) view.isActive = view.id === target.id;
      for (const account of fixture.accounts) account.isActive = account.id === target.id;
      fixture.activeId = target.id;
      fixture.live = { kind: "known", accountId: target.id };
      if (fixture.burn) fixture.burn.suggestion = null;
      fixture.lastPollAt = new Date().toISOString();
      return `Switched to ${target.abbr} — ${target.label}`;
    }

    case "cancel_pending":
      fixture.pending = null;
      return null;

    case "save_current": {
      if (fixture.live.kind === "unknown") {
        fixtureCreateFromLive(fixture, args.label);
        fixture.pending = null;
        return null;
      }
      if (fixture.live.kind === "known") {
        const account = fixture.views.find((view) => view.id === fixture.live.accountId);
        if (!account) throw new Error("the live account is not in the list");
        account.loginState = "ok";
        fixture.lastPollAt = new Date().toISOString();
        return null;
      }
      throw new Error("there is no live login to save");
    }

    case "edit_account": {
      const key = String(args.key || "");
      const view = fixture.views.find(
        (entry) => entry.id === key || String(entry.abbr).toLowerCase() === key.toLowerCase()
      );
      if (!view) throw new Error(`unknown account ${key}`);
      const account = fixture.accounts.find((entry) => entry.id === view.id);
      if (!account) throw new Error(`the account ${key} has no stored row`);
      const label = args.label === undefined || args.label === null
        ? account.label
        : String(args.label).trim();
      if (!label || label.length > 40) {
        throw new Error(`invalid label ${JSON.stringify(label)}: use 1 to 40 characters`);
      }
      const abbr = (args.abbr === undefined || args.abbr === null
        ? account.abbr
        : String(args.abbr)
      )
        .trim()
        .toUpperCase();
      if (!/^[A-Z0-9]{1,2}$/.test(abbr)) {
        throw new Error(
          `invalid abbreviation ${JSON.stringify(abbr)}: use 1-2 letters A-Z or digits`
        );
      }
      if (
        fixture.views.some((entry) => entry.id !== view.id && entry.abbr === abbr) ||
        fixture.providerSubscriptions.some((entry) => entry.abbr === abbr)
      ) {
        throw new Error(`abbreviation ${JSON.stringify(abbr)} is already used`);
      }
      const color = (args.color === undefined || args.color === null
        ? account.color
        : String(args.color)
      )
        .trim()
        .toLowerCase();
      if (!/^#[0-9a-f]{6}$/.test(color)) {
        throw new Error(`invalid colour ${JSON.stringify(color)}: use #rrggbb`);
      }
      const autoBurn =
        args.autoBurn === undefined || args.autoBurn === null
          ? account.autoBurn
          : args.autoBurn;
      if (typeof autoBurn !== "boolean") {
        throw new Error("autoBurn must be true or false");
      }
      let price = account.price === undefined ? null : account.price;
      let priceCurrency = account.priceCurrency || null;
      if (Object.prototype.hasOwnProperty.call(args, "price")) {
        if (args.price === null || args.price === "") {
          price = null;
          priceCurrency = null;
        } else {
          const code =
            typeof fixture.settings.currency === "string"
              ? fixture.settings.currency
              : null;
          if (!code) {
            throw new Error("set a currency in Settings → General before a price");
          }
          const amount = Number(args.price);
          if (!Number.isFinite(amount) || amount <= 0) {
            throw new Error(
              `invalid price ${String(args.price)}: use a number above 0 per month`
            );
          }
          price = amount;
          priceCurrency = code;
        }
      }
      for (const entry of [view, account]) {
        entry.label = label;
        entry.abbr = abbr;
        entry.color = color;
        entry.autoBurn = autoBurn;
      }
      account.price = price;
      account.priceCurrency = priceCurrency;
      return clone(account);
    }

    // The saved Codex logins (R8): a switch moves the live mark and answers with
    // the worker's note; the saved login's own numbers become the card's. The
    // old login keeps its usage until the app's next poll, as it does in Rust.
    case "switch_codex": {
      const account = isObject(args.account) ? args.account : null;
      if (!account || typeof account.acc !== "string" || typeof account.org !== "string") {
        throw new Error("switch_codex needs an account {acc, org}");
      }
      const target = fixture.codexLoginRows.find(
        (row) => row.account.acc === account.acc && row.account.org === account.org
      );
      if (!target || account.org !== "codex") {
        throw new Error(`${account.acc} is not a saved Codex login`);
      }
      if (target.live) return "";
      // A switch takes a moment in the app too; the delay lets the pending state
      // be seen in a page shot.
      await new Promise((resolve) => setTimeout(resolve, 400));
      const previous = fixture.codexLoginRows.find((row) => row.live) || null;
      const card = fixture.providerCardViews.find((view) => view.provider === "codex");
      if (previous && card) {
        previous.plan = card.plan;
        previous.usage = card.usage ? clone(card.usage) : null;
      }
      for (const row of fixture.codexLoginRows) row.live = row === target;
      if (card) {
        card.plan = target.plan;
        card.usage = target.usage ? clone(target.usage) : null;
        card.fetchedAt = isoIn(0);
        card.error = null;
      }
      return previous
        ? `Codex now uses ${target.abbr}. Codex sessions already running keep ${previous.abbr} until they restart.`
        : `Codex now uses ${target.abbr}.`;
    }

    // The Google card's Connect (spec §7.9): the card then says agy is
    // connected and waits for its first reading, as the provider thread's poll does.
    case "connect_agy": {
      const card = fixture.providerCardViews.find((view) => view.provider === "google");
      if (!card || card.canConnect !== true) {
        throw new Error("agy is not waiting to be connected");
      }
      card.canConnect = false;
      card.notes = [
        "agy is connected. Its usage arrives while agy runs; restart agy sessions that were open before.",
      ];
      return null;
    }

    // Settings → Accounts: deletes seatswap's copy only; the subscription row,
    // its plan, price and history stay, and Codex's own file is not touched.
    // The live login is refused with the worker's words.
    case "remove_codex_login": {
      const account = isObject(args.account) ? args.account : null;
      if (!account || typeof account.acc !== "string" || typeof account.org !== "string") {
        throw new Error("remove_codex_login needs an account {acc, org}");
      }
      const target = fixture.codexLoginRows.find(
        (row) => row.account.acc === account.acc && row.account.org === account.org
      );
      if (!target) {
        throw new Error(`${account.acc} is not a saved Codex login`);
      }
      if (target.live) {
        throw new Error("the live login: switch Codex to another login first to remove it");
      }
      fixture.codexLoginRows = fixture.codexLoginRows.filter((row) => row !== target);
      return null;
    }

    case "edit_subscription": {
      const account = isObject(args.account) ? args.account : null;
      if (!account || typeof account.acc !== "string" || typeof account.org !== "string") {
        throw new Error("edit_subscription needs an account {acc, org}");
      }
      const subscription = fixture.providerSubscriptions.find(
        (entry) =>
          isObject(entry.account) &&
          entry.account.acc === account.acc &&
          entry.account.org === account.org
      );
      if (!subscription) throw new Error(`unknown provider subscription ${account.org}/${account.acc}`);
      if (args.label !== undefined && args.label !== null) {
        const label = String(args.label).trim();
        if (!label || label.length > 40) {
          throw new Error(`invalid label ${JSON.stringify(label)}: use 1 to 40 characters`);
        }
        subscription.label = label;
      }
      if (args.abbr !== undefined && args.abbr !== null) {
        const abbr = String(args.abbr).trim().toUpperCase();
        if (!/^[A-Z0-9]{1,2}$/.test(abbr)) {
          throw new Error(
            `invalid abbreviation ${JSON.stringify(abbr)}: use 1-2 letters A-Z or digits`
          );
        }
        const taken = [
          ...fixture.accounts.map((entry) => entry.abbr),
          ...fixture.providerSubscriptions
            .filter((entry) => entry !== subscription)
            .map((entry) => entry.abbr),
        ];
        if (taken.some((entry) => String(entry).toUpperCase() === abbr)) {
          throw new Error(`abbreviation ${JSON.stringify(abbr)} is already used`);
        }
        subscription.abbr = abbr;
      }
      if (args.color !== undefined && args.color !== null) {
        const color = String(args.color).trim().toLowerCase();
        if (!/^#[0-9a-f]{6}$/.test(color)) {
          throw new Error(`invalid colour ${JSON.stringify(color)}: use #rrggbb`);
        }
        subscription.color = color;
      }
      if (Object.prototype.hasOwnProperty.call(args, "plan")) {
        if (subscription.company !== "ollama") {
          throw new Error("OpenAI's plan comes from the provider and cannot be edited here");
        }
        const plan = args.plan === null ? "" : String(args.plan).trim();
        if (!plan) {
          subscription.plan = null;
        } else if (plan.length > 40) {
          throw new Error(`invalid plan ${JSON.stringify(plan)}: use 1 to 40 characters`);
        } else {
          subscription.plan = plan;
        }
      }
      if (Object.prototype.hasOwnProperty.call(args, "price")) {
        if (args.price === null || args.price === "") {
          subscription.price = null;
          subscription.priceCurrency = null;
        } else {
          const code =
            typeof fixture.settings.currency === "string"
              ? fixture.settings.currency
              : null;
          if (!code) {
            throw new Error("set a currency in Settings → General before a price");
          }
          const amount = Number(args.price);
          if (!Number.isFinite(amount) || amount <= 0) {
            throw new Error(
              `invalid price ${String(args.price)}: use a number above 0 per month`
            );
          }
          subscription.price = amount;
          subscription.priceCurrency = code;
        }
      }
      return clone(subscription);
    }

    case "remove_account": {
      const key = String(args.key || "");
      const index = fixture.views.findIndex(
        (view) => view.id === key || String(view.abbr).toLowerCase() === key.toLowerCase()
      );
      if (index < 0) throw new Error(`unknown account ${key}`);
      const removed = fixture.views[index];
      fixture.views.splice(index, 1);
      fixture.accounts = fixture.accounts.filter((account) => account.id !== removed.id);
      if (fixture.live.kind === "known" && fixture.live.accountId === removed.id) {
        fixture.live = {
          kind: "unknown",
          email: removed.email || null,
          orgName: null,
        };
      }
      if (fixture.activeId === removed.id) fixture.activeId = null;
      for (const view of fixture.views) view.isActive = view.id === fixture.activeId;
      for (const account of fixture.accounts) account.isActive = account.id === fixture.activeId;
      if (fixture.burn) fixture.burn.suggestion = null;
      if (Array.isArray(fixture.warnings)) {
        fixture.warnings = fixture.warnings.filter((warning) => warning.id !== removed.id);
      }
      return clone(removed);
    }

    case "set_auto_burn": {
      if (typeof args.on !== "boolean") {
        throw new Error("set_auto_burn needs a boolean on");
      }
      if (!fixture.burn) throw new Error("auto-burn is not loaded");
      fixtureSaveSetting(fixture, "autoBurn", args.on);
      fixture.lastPollAt = new Date().toISOString();
      return null;
    }

    case "pause_burn": {
      if (!fixture.burn) throw new Error("auto-burn is not loaded");
      const hours = args.hours;
      if (hours === null || hours === undefined) {
        fixture.burn.pausedUntil = null;
      } else {
        if (!Number.isInteger(hours) || hours <= 0) {
          throw new Error(`pause_burn needs a positive whole number of hours, got ${hours}`);
        }
        fixture.burn.pausedUntil = isoIn(hours * 3600000);
      }
      fixture.lastPollAt = new Date().toISOString();
      return null;
    }

    case "save_setting":
      return fixtureSaveSetting(fixture, String(args.field || ""), args.value);

    // The status line builder (spec §5.4). The preview is fixture data, not a
    // second renderer; the commands' answers have the Rust types' shapes.
    case "statusline_presets":
      return clone(fixturePresets());

    case "statusline_kinds":
      return clone(FIXTURE_KINDS);

    case "statusline_preview":
      return fixtureStatuslinePreview(args.config, args.widths, args.scenario);

    case "statusline_record": {
      const minutes = Number(args.minutes);
      if (!Number.isInteger(minutes) || minutes < 1 || minutes > 60) {
        throw new Error(`a recording lasts 1 to 60 minutes, got ${args.minutes}`);
      }
      fixtureSaveSetting(fixture, "statuslineRecordUntil", isoIn(minutes * 60000));
      // The real recorder fills the file when Claude Code next calls the
      // status line; the browser gets a finished list at once.
      fixture.statuslineKeys = fixtureRecordedKeys();
      return null;
    }

    case "statusline_keys":
      return fixture.statuslineKeys === null ? null : clone(fixture.statuslineKeys);

    case "statusline_fields":
      return clone(fixtureFields());

    case "statusline_install_state":
      return clone(fixtureInstallState(fixture));

    case "statusline_install": {
      const state = fixtureInstallState(fixture);
      if (!state.installed) {
        const record = isObject(fixture.settings.statuslineInstall)
          ? fixture.settings.statuslineInstall
          : null;
        const previous =
          record !== null && sameFixtureValue(state.current, record.installed)
            ? clone(record.previous)
            : clone(state.current);
        fixture.statuslineClaude.current = clone(state.ours);
        fixture.settings = validateFixtureSettings(
          Object.assign({}, fixture.settings, {
            statuslineInstall: {
              installed: clone(state.ours),
              previous,
            },
          })
        );
      }
      return clone(fixtureInstallState(fixture));
    }

    case "statusline_restore": {
      const state = fixtureInstallState(fixture);
      if (!isObject(fixture.settings.statuslineInstall)) {
        throw new Error(
          "seatswap has no record of the line before its own: nothing to restore"
        );
      }
      if (!state.restorable) {
        throw new Error("the status line in Claude Code is not seatswap's: nothing to restore");
      }
      const restored = state.previous;
      fixture.statuslineClaude.current = restored;
      fixture.settings = validateFixtureSettings(
        Object.assign({}, fixture.settings, { statuslineInstall: null })
      );
      return clone(fixtureInstallState(fixture));
    }

    case "sessions_view":
      return clone(fixtureSessionsView(args));

    case "sessions_subscribe":
      return null;

    case "sessions_unsubscribe":
      return null;

    case "open_panel":
      return null;

    case "open_window":
      return null;

    case "agent_calls": {
      const limit = Number(args.limit);
      if (!Number.isInteger(limit) || limit < 0) {
        throw new Error(`agent_calls needs a limit of 0 or more, got ${args.limit}`);
      }
      return clone(
        fixture.agentCalls
          .slice()
          .sort((a, b) => Date.parse(b.at) - Date.parse(a.at))
          .slice(0, limit)
      );
    }

    case "money_view":
      return fixtureMoneyWithOverride(fixture);

    case "price_list":
      return clone(fixture.priceList);

    case "lookup_now":
      fixture.money.pricesCheckedAt = new Date().toISOString();
      return fixtureMoneyWithOverride(fixture);

    case "skill_status":
      return clone(fixture.skill);

    case "skill_install": {
      if (typeof args.force !== "boolean") {
        throw new Error("skill_install needs force true or false");
      }
      if (fixture.skill.installed && fixture.skill.differs && !args.force) {
        throw new Error(
          `${fixture.skill.path} already exists and differs from seatswap's bundled skill; use --force to replace it`
        );
      }
      fixture.skill = Object.assign({}, fixture.skill, {
        installed: true,
        differs: false,
      });
      return clone(fixture.skill);
    }

    case "icon_preview": {
      const abbr = String(args.abbr || "").toUpperCase();
      const color = String(args.color || "").toLowerCase();
      if (!/^[A-Z0-9]{1,2}$/.test(abbr)) throw new Error("invalid abbreviation");
      if (!/^#[0-9a-f]{6}$/.test(color)) throw new Error("invalid colour");
      return [16, 22, 32].map((size) => ({
        size,
        rgba: Array.from(fixtureIcon(abbr, color, size)),
      }));
    }

    // Only a stand-in so the page can be looked at in a browser: the real
    // texts come from `format.rs`, which a browser cannot run.
    case "time_texts": {
      if (timeTextsError) throw new Error(timeTextsError);
      const months = [
        "Jan",
        "Feb",
        "Mar",
        "Apr",
        "May",
        "Jun",
        "Jul",
        "Aug",
        "Sep",
        "Oct",
        "Nov",
        "Dec",
      ];
      const weekdays = ["Sun", "Mon", "Tue", "Wed", "Thu", "Fri", "Sat"];
      const clock = (date) =>
        `${String(date.getHours()).padStart(2, "0")}:${String(date.getMinutes()).padStart(2, "0")}`;
      return (args.times || []).map((time) => {
        if (time.style === "age") return "a moment ago";
        const date = new Date(time.at);
        if (time.style === "day" || time.style === "date") {
          // `date` is a calendar date: the day as written, without moving it
          // into the browser's time zone. `day` is a moment in local time.
          if (time.style === "date") {
            return `${date.getUTCDate()} ${months[date.getUTCMonth()]}`;
          }
          return `${date.getDate()} ${months[date.getMonth()]}`;
        }
        if (time.style === "left") {
          // The time left as `format::time_left`: days and hours from a day on,
          // else hours and minutes, rounded up.
          const seconds = (date - new Date()) / 1000;
          if (seconds <= 0) return "0s";
          if (seconds >= 86400) {
            const hours = Math.ceil(seconds / 3600);
            const days = Math.floor(hours / 24);
            return hours % 24 === 0 ? `${days}d` : `${days}d ${hours % 24}h`;
          }
          const minutes = Math.ceil(seconds / 60);
          if (minutes < 60) return `${minutes}min`;
          const rest = minutes % 60;
          return rest === 0 ? `${Math.floor(minutes / 60)}h` : `${Math.floor(minutes / 60)}h ${rest}min`;
        }
        if (time.style === "dayYear") {
          // The local calendar day and year, as `TimeStyle::DayYear`.
          return `${date.getDate()} ${months[date.getMonth()]} ${date.getFullYear()}`;
        }
        if (time.style === "dayClock") {
          const now = new Date();
          const today = new Date(now.getFullYear(), now.getMonth(), now.getDate());
          const day = new Date(date.getFullYear(), date.getMonth(), date.getDate());
          const days = Math.round((today - day) / 86400000);
          // Today keeps the clock; the last six days name the weekday; older
          // calls keep the day and month in front, as `TimeStyle::DayClock`.
          if (days === 0) return clock(date);
          if (days > 0 && days <= 6) return `${weekdays[date.getDay()]} ${clock(date)}`;
          return `${date.getDate()} ${months[date.getMonth()]} ${clock(date)}`;
        }
        return clock(date);
      });
    }

    case "projects_view":
      return clone(fixtureProjectView(fixture));

    case "projects_edit": {
      fixtureEditProject(fixture, args.edit);
      return clone(fixtureProjectView(fixture));
    }

    case "history_view":
      return fixtureHistoryView(fixture);

    case "history_import": {
      const from = fixtureExpand(String(args.from || "").trim());
      if (from === "") throw new Error("enter a folder path");
      if (!from.startsWith("/")) {
        throw new Error(`the folder must be an absolute path, got ${from}`);
      }
      const folders = from.replace(/\/+$/, "").split("/");
      const name = String(folders[folders.length - 1] || "").toLowerCase();
      // The history folder itself, as the group shows it ("~" is the home folder).
      const own = fixtureHistoryView(fixture).dir.replace(/^~/, "/home/sam");
      if (from.replace(/\/+$/, "") === own || name === fixture.history.host) {
        throw new Error(`${from}: that is this history folder`);
      }
      if (!/^[a-z0-9][a-z0-9._-]*$/.test(name)) {
        throw new Error(`no host folder found in ${from}`);
      }
      const known = fixture.history.hosts.find((host) => host.host === name);
      const copied = [];
      if (!known) {
        fixture.history.hosts.push({
          host: name,
          thisHost: false,
          firstRecord: isoIn(-30 * 86400000),
          lastRecord: isoIn(-60000),
          bytes: 734003,
        });
        copied.push(name);
      } else {
        // One more file arrived from that host.
        known.bytes += 262144;
        known.lastRecord = isoIn(-30000);
        copied.push(name);
      }
      fixture.history.hosts.sort((one, other) => (one.host < other.host ? -1 : 1));
      return { copied, conflicts: [], warnings: [], view: fixtureHistoryView(fixture) };
    }

    case "history_export": {
      const format = String(args.format || "");
      if (format !== "csv" && format !== "json") {
        throw new Error(`unknown export format ${JSON.stringify(format)}: use "csv" or "json"`);
      }
      const to = fixtureExpand(String(args.to || "").trim());
      if (to === "") throw new Error("enter a file path");
      if (!to.startsWith("/")) throw new Error(`the export path must be an absolute path, got ${to}`);
      const from = args.from ? Date.parse(String(args.from)) : null;
      const until = args.until ? Date.parse(String(args.until)) : null;
      if (from !== null && until !== null && !(from < until)) {
        throw new Error(`from ${args.from} is not before until ${args.until}`);
      }
      if (fixture.exports.includes(to)) throw new Error(`${to} exists; choose another name`);
      fixture.exports.push(to);
      return null;
    }

    case "fit_panel":
      return null;

    case "hide_panel":
      return null;

    default:
      throw new Error(`unknown command ${command}`);
  }
}
