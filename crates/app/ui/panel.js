// seatswap pop-up panel.
// Plain ES2020, no build step. Talks to the tray app through the Tauri 2 global API;
// without it, runs on a built-in fixture snapshot so the page can be checked in a browser.

// ---------------------------------------------------------------------------
// constants and helpers
// ---------------------------------------------------------------------------

const PALETTE = [
  "#3b82f6", "#f59e0b", "#10b981", "#ef4444", "#8b5cf6",
  "#ec4899", "#14b8a6", "#f97316", "#6366f1", "#84cc16",
];

// The tray's grey for an unknown or invalid account colour (icon.rs GREY).
const TRAY_GREY = "#555b66";

const DEFAULT_SETTINGS = {
  trayText: "percent",
  iconLimit: "binding",
  pollActiveSecs: 120,
  pollInactiveSecs: 900,
  suggestAt: 90,
  suggestTargetBelow: 50,
  notifyOnSwitch: true,
  mirrorLive: true,
  refreshInactive: true,
  hotkey: "Ctrl+Alt+C",
  startAtLogin: true,
  theme: "system",
  claudeConfigDir: null,
};

const LEVEL_COLORS = {
  fine: "#22c55e",
  close: "#f59e0b",
  critical: "#ef4444",
  locked: "#ef4444",
};

const WEEKDAYS = ["Sun", "Mon", "Tue", "Wed", "Thu", "Fri", "Sat"];

const TAURI_GLOBAL = typeof window !== "undefined" ? window.__TAURI__ : undefined;
const HAS_TAURI = Boolean(
  TAURI_GLOBAL && TAURI_GLOBAL.core && typeof TAURI_GLOBAL.core.invoke === "function"
);

const state = {
  snapshot: null,
  view: "main",
  focusedIndex: 0,
  focusInitialized: false,
  editingId: null,
  editor: null,
  pendingKey: null,
  pendingLabel: "",
  unknownKey: null,
  unknownLabel: "",
  settingsDraft: null,
  refreshing: false,
  lastPercents: new Map(),
  lastCardOrder: [],
  errors: {
    banner: null,
    accounts: {},
    editor: null,
    editorColor: null,
    preview: null,
    add: null,
    settings: null,
    settingsConfigDir: null,
    settingsSaved: false,
  },
  fixture: null,
};

function isObject(value) {
  return value !== null && typeof value === "object" && !Array.isArray(value);
}

function clamp(value, low, high) {
  return Math.min(high, Math.max(low, value));
}

function clone(value) {
  return value === undefined || value === null ? value : JSON.parse(JSON.stringify(value));
}

function errorMessage(error) {
  if (typeof error === "string") return error;
  if (error && typeof error.message === "string") return error.message;
  try {
    return JSON.stringify(error);
  } catch {
    return String(error);
  }
}

function isColor(value) {
  return typeof value === "string" && /^#[0-9a-fA-F]{6}$/.test(value);
}

function hexToRgb(value) {
  if (!isColor(value)) return null;
  const number = parseInt(value.slice(1), 16);
  return {
    r: (number >> 16) & 0xff,
    g: (number >> 8) & 0xff,
    b: number & 0xff,
  };
}

function rgba(value, alpha) {
  const rgb = hexToRgb(value);
  if (!rgb) return `rgba(139, 147, 167, ${alpha})`;
  return `rgba(${rgb.r}, ${rgb.g}, ${rgb.b}, ${alpha})`;
}

function rgbToHex(rgb) {
  const part = (number) => clamp(Math.round(number), 0, 255).toString(16).padStart(2, "0");
  return `#${part(rgb.r)}${part(rgb.g)}${part(rgb.b)}`;
}

function mixHex(first, second, ratio) {
  const a = hexToRgb(first);
  const b = hexToRgb(second);
  if (!a || !b) return first;
  const mix = (x, y) => x + (y - x) * ratio;
  return rgbToHex({ r: mix(a.r, b.r), g: mix(a.g, b.g), b: mix(a.b, b.b) });
}

function relativeLuminance(value) {
  const rgb = hexToRgb(value);
  if (!rgb) return 0;
  const channel = (part) => {
    const scaled = part / 255;
    return scaled <= 0.03928 ? scaled / 12.92 : Math.pow((scaled + 0.055) / 1.055, 2.4);
  };
  return 0.2126 * channel(rgb.r) + 0.7152 * channel(rgb.g) + 0.0722 * channel(rgb.b);
}

function contrastText(background) {
  const luminance = relativeLuminance(background);
  const whiteRatio = 1.05 / (luminance + 0.05);
  const darkRatio = (luminance + 0.05) / (relativeLuminance("#0f1115") + 0.05);
  return darkRatio >= whiteRatio ? "#0f1115" : "#ffffff";
}

function limitLevel(limit) {
  const percent = Number(limit && limit.percent);
  if (!Number.isFinite(percent)) return "fine";
  if (percent >= 100) return "locked";
  if (percent >= 90 || limit.severity === "critical") return "critical";
  if (percent >= 70) return "close";
  return "fine";
}

function shortPercent(value) {
  const number = Number(value);
  if (!Number.isFinite(number)) return "–";
  const rounded = Math.round(number * 10) / 10;
  return Number.isInteger(rounded) ? String(rounded) : rounded.toFixed(1);
}

function fmtPercent(value) {
  return `${shortPercent(value)} %`;
}

// Time to a reset, the same rule as `format::format_reset_at` in Rust (change
// both together): under 24 h `35m` / `1h05m`, up to 48 h `1d18h`, beyond that
// the local weekday and clock time `Sun 02:00`. Minutes round up.
function resetText(iso) {
  const time = resetMillis(iso);
  if (!Number.isFinite(time)) return "";
  const seconds = Math.floor((time - Date.now()) / 1000);
  if (seconds <= 0) return "0m";
  if (seconds > 48 * 3600) {
    return `${WEEKDAYS[new Date(time).getDay()]} ${clockText(iso)}`;
  }
  const minutes = Math.ceil(seconds / 60);
  if (minutes < 60) return `${minutes}m`;
  if (minutes < 24 * 60) {
    return `${Math.floor(minutes / 60)}h${String(minutes % 60).padStart(2, "0")}m`;
  }
  return `${Math.floor(minutes / 1440)}d${Math.floor((minutes % 1440) / 60)}h`;
}

// A reset time rounded up to the full minute: the server sends a moment just
// before the boundary (`00:59:59.59`). Same as `reset_minute` in Rust.
function resetMillis(iso) {
  return Math.ceil(Date.parse(iso || "") / 60000) * 60000;
}

// Local clock time of a reset (rounded up to the full minute).
function clockText(iso) {
  const time = resetMillis(iso);
  if (!Number.isFinite(time)) return "";
  const date = new Date(time);
  return `${String(date.getHours()).padStart(2, "0")}:${String(date.getMinutes()).padStart(2, "0")}`;
}

function ageText(milliseconds) {
  const seconds = Math.max(0, Math.floor(milliseconds / 1000));
  if (seconds < 60) return `${seconds} s ago`;
  const minutes = Math.floor(seconds / 60);
  if (minutes < 60) return `${minutes} min ago`;
  const hours = Math.floor(minutes / 60);
  if (hours < 24) return `${hours} h ago`;
  return `${Math.floor(hours / 24)} d ago`;
}

function prefersReducedMotion() {
  return Boolean(
    typeof window !== "undefined" &&
      window.matchMedia &&
      window.matchMedia("(prefers-reduced-motion: reduce)").matches
  );
}

function currentThemeSetting() {
  if (state.snapshot && isObject(state.snapshot.settings) && state.snapshot.settings.theme) {
    return state.snapshot.settings.theme;
  }
  if (state.settingsDraft && state.settingsDraft.theme) return state.settingsDraft.theme;
  return "system";
}

function applyTheme(setting) {
  const wanted = setting || currentThemeSetting();
  let resolved = wanted;
  if (wanted !== "light" && wanted !== "dark") {
    const light =
      typeof window !== "undefined" &&
      window.matchMedia &&
      window.matchMedia("(prefers-color-scheme: light)").matches;
    resolved = light ? "light" : "dark";
  }
  document.documentElement.dataset.theme = resolved;
}

// ---------------------------------------------------------------------------
// bridge: invoke() and listen() over the Tauri 2 global
// ---------------------------------------------------------------------------

async function invoke(command, args) {
  if (HAS_TAURI) {
    return TAURI_GLOBAL.core.invoke(command, args || {});
  }
  return fixtureInvoke(command, args || {});
}

async function listen(eventName, handler) {
  if (HAS_TAURI && TAURI_GLOBAL.event && typeof TAURI_GLOBAL.event.listen === "function") {
    const unlisten = await TAURI_GLOBAL.event.listen(eventName, (event) => {
      handler(event && Object.prototype.hasOwnProperty.call(event, "payload") ? event.payload : event);
    });
    return unlisten;
  }
  return () => {};
}

// ---------------------------------------------------------------------------
// DOM helpers (all data goes through text nodes or attributes)
// ---------------------------------------------------------------------------

function clear(node) {
  while (node && node.firstChild) node.removeChild(node.firstChild);
}

function setStyles(node, styles) {
  for (const [key, value] of Object.entries(styles)) {
    if (value === undefined || value === null) continue;
    if (key.startsWith("--")) node.style.setProperty(key, String(value));
    else node.style[key] = value;
  }
}

function applyProps(node, props) {
  for (const [key, value] of Object.entries(props || {})) {
    if (key === "hidden") {
      node.hidden = Boolean(value);
      continue;
    }
    if (key === "checked") {
      node.checked = Boolean(value);
      continue;
    }
    if (key === "disabled") {
      node.disabled = Boolean(value);
      continue;
    }
    if (value === undefined || value === null || value === false) continue;
    if (key === "class") node.className = value;
    else if (key === "text") node.textContent = String(value);
    else if (key === "style") setStyles(node, value);
    else if (key === "dataset") {
      for (const [dataKey, dataValue] of Object.entries(value)) {
        if (dataValue !== undefined && dataValue !== null) node.dataset[dataKey] = String(dataValue);
      }
    } else if (key === "value") {
      node.value = value;
    } else if (key.startsWith("on") && typeof value === "function") {
      node.addEventListener(key.slice(2), value);
    } else if (value === true) {
      node.setAttribute(key, "");
    } else {
      node.setAttribute(key, String(value));
    }
  }
}

function appendChildren(node, children) {
  const list = Array.isArray(children) ? children : [children];
  for (const child of list) {
    if (child === undefined || child === null || child === false) continue;
    if (typeof child === "string" || typeof child === "number") {
      node.appendChild(document.createTextNode(String(child)));
    } else {
      node.appendChild(child);
    }
  }
}

function h(tag, props, children) {
  const node = document.createElement(tag);
  applyProps(node, props || {});
  if (children !== undefined) appendChildren(node, children);
  return node;
}

function button(label, variant, onclick, props) {
  const extra = props || {};
  const className = ["btn", variant || "", extra.class || ""].filter(Boolean).join(" ");
  const merged = Object.assign({ type: "button" }, extra, {
    class: className,
    onclick,
  });
  return h("button", merged, label);
}

function textInput(props) {
  return h("input", Object.assign({ type: "text", spellcheck: "false" }, props || {}));
}

function selectInput(options, value, onchange) {
  const select = h("select", {
    onchange: (event) => onchange(event.target.value),
  });
  for (const [optionValue, optionLabel] of options) {
    const option = h("option", { value: optionValue, text: optionLabel });
    if (optionValue === value) option.selected = true;
    select.appendChild(option);
  }
  return select;
}

// ---------------------------------------------------------------------------
// snapshot field readers (the shape is docs/implementation.md, serde camelCase)
// ---------------------------------------------------------------------------

function allViews() {
  if (!state.snapshot || !Array.isArray(state.snapshot.views)) return [];
  return state.snapshot.views;
}

function viewIsActive(view) {
  if (!view) return false;
  if (view.isActive) return true;
  return Boolean(
    state.snapshot && state.snapshot.activeId && view.id === state.snapshot.activeId
  );
}

function orderedViews(views) {
  return (Array.isArray(views) ? views.slice() : []).sort(
    (a, b) => Number(viewIsActive(b)) - Number(viewIsActive(a))
  );
}

function findView(id) {
  if (!id) return null;
  return allViews().find((view) => view.id === id) || null;
}

function accountLimits(view) {
  if (!view || !isObject(view.usage) || !Array.isArray(view.usage.limits)) return [];
  return view.usage.limits;
}

function limitKindName(limit) {
  if (!isObject(limit)) return "";
  const kind = limit.kind;
  if (typeof kind === "string") return kind;
  // serde serializes LimitKind::Other("daily_tokens") as {"other":"daily_tokens"}.
  if (isObject(kind) && typeof kind.other === "string") return kind.other;
  return "";
}

function findLimit(view, kind) {
  return accountLimits(view).find((limit) => limitKindName(limit) === kind) || null;
}

function scopedLimits(view) {
  return accountLimits(view).filter((limit) => limitKindName(limit) === "weekly_scoped");
}

function otherLimits(view) {
  const known = new Set(["session", "weekly_all", "weekly_scoped"]);
  return accountLimits(view)
    .map((limit) => ({ name: limitKindName(limit), limit }))
    .filter((entry) => entry.name && !known.has(entry.name));
}

function usageAge(view) {
  if (!view || !view.usage || !view.fetchedAt) return null;
  const time = Date.parse(view.fetchedAt);
  if (!Number.isFinite(time)) return null;
  const elapsed = Date.now() - time;
  if (elapsed < 120000) return null;
  return `usage ${ageText(elapsed)}`;
}

function accountLabel(view) {
  if (!view) return "?";
  return view.abbr || view.label || "?";
}

function loginWarningText(warning) {
  if (!isObject(warning)) return "";
  const account = findView(warning.id);
  const name = account ? account.label : "An account";
  if (warning.kind === "soon") {
    const days = Number(warning.daysLeft);
    return `${name} — login ends in ${Number.isFinite(days) ? days : "?"} d`;
  }
  if (warning.kind === "tomorrow") return `${name} — login ends tomorrow`;
  if (warning.kind === "ended") return `${name} — login ended, log in again`;
  return "";
}

// ---------------------------------------------------------------------------
// theme
// ---------------------------------------------------------------------------

function wireTheme() {
  if (!window.matchMedia) return;
  const media = window.matchMedia("(prefers-color-scheme: dark)");
  if (typeof media.addEventListener === "function") {
    media.addEventListener("change", () => {
      if (currentThemeSetting() === "system") applyTheme("system");
    });
  }
}

// ---------------------------------------------------------------------------
// rendering: main view
// ---------------------------------------------------------------------------

function renderMain() {
  applyTheme();
  const cards = document.getElementById("cards");
  const banners = document.getElementById("banners");
  if (!cards || !banners) return;

  const activeElement = document.activeElement;
  const focusField =
    activeElement && activeElement.dataset && activeElement.dataset.field
      ? activeElement.dataset.field
      : null;
  const focusSelection =
    focusField && activeElement && typeof activeElement.selectionStart === "number"
      ? activeElement.selectionStart
      : null;

  const views = orderedViews(state.snapshot ? state.snapshot.views : []);
  if (views.length > 0) {
    if (state.focusedIndex >= views.length) state.focusedIndex = views.length - 1;
    if (state.focusedIndex < 0) state.focusedIndex = 0;
  } else {
    state.focusedIndex = 0;
  }
  if (!state.focusInitialized && views.length > 0) {
    const activeIndex = views.findIndex((view) => viewIsActive(view));
    state.focusedIndex = activeIndex >= 0 ? activeIndex : 0;
    state.focusInitialized = true;
  }

  if (state.editingId && !findView(state.editingId)) {
    state.editingId = null;
    state.editor = null;
    state.errors.editor = null;
    state.errors.editorColor = null;
  }

  clear(banners);
  renderBanners(banners);

  const previousPositions = captureCardPositions();
  clear(cards);
  if (state.snapshot) {
    views.forEach((view, index) => {
      cards.appendChild(renderCard(view, index));
    });
  }

  const nextOrder = views.map((view) => view.id);
  const reordered =
    state.lastCardOrder.length === nextOrder.length &&
    state.lastCardOrder.some((id, index) => id !== nextOrder[index]);
  state.lastCardOrder = nextOrder;
  if (reordered) animateCardMoves(cards, previousPositions);

  updateAge();
  updateResetTimes();
  fitPanel();

  if (focusField) {
    const next = document.querySelector(`[data-field="${focusField}"]`);
    if (next && typeof next.focus === "function") {
      next.focus();
      if (focusSelection !== null && typeof next.setSelectionRange === "function") {
        try {
          next.setSelectionRange(focusSelection, focusSelection);
        } catch {
          // not a text field; ignore
        }
      }
    }
  }
}

function captureCardPositions() {
  const positions = new Map();
  const cards = document.getElementById("cards");
  if (!cards) return positions;
  for (const child of cards.children) {
    if (child.dataset && child.dataset.accountId) {
      positions.set(child.dataset.accountId, child.getBoundingClientRect().top);
    }
  }
  return positions;
}

function animateCardMoves(cards, previousPositions) {
  if (prefersReducedMotion()) return;
  for (const child of cards.children) {
    const id = child.dataset ? child.dataset.accountId : null;
    if (!id || !previousPositions.has(id)) continue;
    const delta = previousPositions.get(id) - child.getBoundingClientRect().top;
    if (Math.abs(delta) < 1) continue;
    child.style.transition = "none";
    child.style.transform = `translateY(${delta}px)`;
    void child.getBoundingClientRect();
    requestAnimationFrame(() => {
      child.style.transition = "transform 250ms cubic-bezier(0.34, 1.3, 0.64, 1)";
      child.style.transform = "translateY(0)";
      setTimeout(() => {
        child.style.transition = "";
        child.style.transform = "";
      }, 280);
    });
  }
}

function renderBanners(container) {
  const snapshot = state.snapshot;
  if (!snapshot) return;

  if (state.errors.banner) {
    container.appendChild(
      banner("error", "⚠", state.errors.banner, [
        button("Dismiss", "ghost", () => {
          state.errors.banner = null;
          renderMain();
        }, { class: "small" }),
      ])
    );
  }

  if (snapshot.error) {
    container.appendChild(banner("error", "⚠", String(snapshot.error)));
  }

  if (snapshot.secretsWarning) {
    container.appendChild(banner("warn", "⚠", String(snapshot.secretsWarning)));
  }

  if (snapshot.pending) {
    container.appendChild(renderPendingBanner(snapshot.pending));
  } else {
    state.pendingKey = null;
    if (snapshot.live && snapshot.live.kind === "unknown") {
      container.appendChild(renderUnknownLoginBanner(snapshot.live));
    } else {
      state.unknownKey = null;
    }
  }

  if (snapshot.suggestion) {
    container.appendChild(renderSuggestionBanner(snapshot.suggestion));
  }

  if (Array.isArray(snapshot.warnings)) {
    for (const warning of snapshot.warnings) {
      const text = loginWarningText(warning);
      if (!text) continue;
      const kind = warning.kind === "ended" ? "error" : "warn";
      container.appendChild(banner(kind, "⏳", text));
    }
  }
}

function banner(kind, icon, text, extraChildren) {
  const body = h("div", { class: "banner-body" }, [
    h("div", { class: "banner-text", text: String(text) }),
  ]);
  if (extraChildren) {
    if (Array.isArray(extraChildren)) {
      for (const child of extraChildren) {
        if (child) body.appendChild(child);
      }
    } else {
      body.appendChild(extraChildren);
    }
  }
  return h(
    "div",
    {
      class: `banner ${kind}`,
      role: kind === "error" ? "alert" : "status",
    },
    [h("span", { class: "banner-icon", "aria-hidden": "true", text: icon }), body]
  );
}

function pendingLabel(pending) {
  const key = `${pending.targetId || ""}|${pending.email || ""}|${pending.orgName || ""}`;
  if (state.pendingKey !== key) {
    state.pendingKey = key;
    state.pendingLabel = String(pending.orgName || pending.email || "New account").slice(0, 40);
  }
  return state.pendingLabel;
}

function renderPendingBanner(pending) {
  const label = pendingLabel(pending);
  const input = textInput({
    value: label,
    maxlength: "40",
    "aria-label": "Label for the saved account",
    class: "banner-label-input",
    dataset: { field: "pending-label" },
  });
  input.addEventListener("input", () => {
    state.pendingLabel = input.value;
  });

  const actions = h("div", { class: "banner-actions" }, [
    button("Save as new account", "primary small", () =>
      switchWithPolicy(pending.targetId, "save", state.pendingLabel)
    ),
    button("Discard it", "danger small", () =>
      switchWithPolicy(pending.targetId, "discard", null)
    ),
    button("Cancel", "ghost small", cancelPending),
  ]);

  const email = pending.email || pending.orgName || "unknown login";
  const target = pending.targetLabel ? ` the switch to ${pending.targetLabel}` : " the switch";
  const body = h("div", { class: "banner-body" }, [
    h("div", {
      class: "banner-text",
      text: `The current login (${email}) is not saved. Save it as a new account, discard it, or cancel${target}.`,
    }),
    h("div", { class: "banner-label" }, [
      h("span", { class: "field-label", text: "Label" }),
      input,
    ]),
    actions,
  ]);

  return h(
    "div",
    { class: "banner info", role: "status" },
    [h("span", { class: "banner-icon", "aria-hidden": "true", text: "ⓘ" }), body]
  );
}

function unknownLabel(live) {
  const key = `${live.email || ""}|${live.orgName || ""}`;
  if (state.unknownKey !== key) {
    state.unknownKey = key;
    state.unknownLabel = String(live.orgName || live.email || "New account").slice(0, 40);
  }
  return state.unknownLabel;
}

function renderUnknownLoginBanner(live) {
  const label = unknownLabel(live);
  const input = textInput({
    value: label,
    maxlength: "40",
    "aria-label": "Label for the new account",
    class: "banner-label-input",
    dataset: { field: "unknown-label" },
  });
  input.addEventListener("input", () => {
    state.unknownLabel = input.value;
  });

  const body = h("div", { class: "banner-body" }, [
    h("div", {
      class: "banner-text",
      text: `New login found: ${live.email || live.orgName || "unknown account"}`,
    }),
    h("div", { class: "banner-label" }, [
      h("span", { class: "field-label", text: "Label" }),
      input,
    ]),
    h("div", { class: "banner-actions" }, [
      button("Save as account", "primary small", () => saveCurrent(state.unknownLabel)),
    ]),
  ]);

  return h(
    "div",
    { class: "banner info", role: "status" },
    [h("span", { class: "banner-icon", "aria-hidden": "true", text: "＋" }), body]
  );
}

function renderSuggestionBanner(suggestion) {
  const from = findView(suggestion.fromId);
  const to = findView(suggestion.toId);
  const fromName = accountLabel(from);
  const toName = to ? accountLabel(to) : "another account";
  const reset = suggestion.fromResetsAt
    ? ` (resets ${clockText(suggestion.fromResetsAt)})`
    : "";
  const text = `${fromName} is at ${fmtPercent(
    suggestion.fromPercent
  )}${reset} — switch to ${toName} (${fmtPercent(suggestion.toPercent)})?`;

  return banner("info", "⚠", text, [
    h("div", { class: "banner-actions" }, [
      button("Switch", "primary small", () => switchTo(suggestion.toId)),
    ]),
  ]);
}

function loginDaysLeftOf(view) {
  if (view && typeof view.loginDaysLeft === "number") return view.loginDaysLeft;
  if (view && typeof view.loginEndsAtMs === "number") {
    return Math.floor((view.loginEndsAtMs - Date.now()) / 86400000);
  }
  return null;
}

function renderCard(view, index) {
  // An invalid colour is the tray's grey here too, never a palette stand-in.
  const accent = isColor(view.color) ? view.color : TRAY_GREY;
  const card = h("div", {
    class:
      "card" +
      (viewIsActive(view) ? " active" : "") +
      (index === state.focusedIndex ? " focused" : ""),
    role: "listitem",
    tabindex: "-1",
    dataset: { index: String(index), accountId: view.id },
    style: { "--accent": accent, "--accent-border": rgba(accent, 0.55) },
  });

  card.addEventListener("click", (event) => {
    if (event.target.closest("button, input, select, label, a")) return;
    setFocused(index);
  });

  card.appendChild(renderGauge(view));

  const main = h("div", { class: "card-main" });

  const badge = h(
    "button",
    {
      type: "button",
      class: "badge",
      title: `Edit ${view.label || "account"}`,
      "aria-label": `Edit account ${view.abbr || ""}`,
    },
    [view.abbr || "?"]
  );
  badge.style.background = accent;
  badge.style.color = contrastText(accent);
  const edit = (event) => {
    event.stopPropagation();
    openEditor(view.id);
  };
  badge.addEventListener("click", edit);
  // The badge opens the editor too; the pencil is the visible hint for it.
  const pencil = h(
    "button",
    {
      type: "button",
      class: "icon-btn edit-btn",
      title: `Edit ${view.label || "account"}`,
      "aria-label": `Edit account ${view.abbr || ""}`,
    },
    [h("span", { "aria-hidden": "true", text: "\u270E" })]
  );
  pencil.addEventListener("click", edit);

  const status = h("div", { class: "card-status" });
  if (view.loginState === "loggedOut") {
    status.appendChild(h("span", { class: "card-note", text: "Logged out" }));
  } else if (viewIsActive(view)) {
    status.appendChild(h("span", { class: "pill", text: "Active" }));
  } else {
    status.appendChild(
      button("Switch", "small", (event) => {
        event.stopPropagation();
        switchTo(view.id);
      })
    );
  }

  main.appendChild(
    h("div", { class: "card-top" }, [
      badge,
      h("span", {
        class: "card-label",
        text: view.label || "Unnamed account",
        title: view.label || "",
      }),
      pencil,
      status,
    ])
  );

  const metaParts = [];
  if (view.email) metaParts.push(view.email);
  if (view.plan) metaParts.push(view.plan);
  const usageAgeText = usageAge(view);
  if (usageAgeText) metaParts.push(usageAgeText);
  const metaText = metaParts.length ? metaParts.join(" · ") : "no login details";
  main.appendChild(h("div", { class: "card-meta", text: metaText, title: metaText }));

  if (view.loginState === "loggedOut") {
    main.appendChild(renderLoggedOut());
  } else {
    const rows = [];
    const session = findLimit(view, "session");
    if (session) rows.push(renderUsageRow("Session", session, true));
    const week = findLimit(view, "weekly_all");
    if (week) rows.push(renderUsageRow("Week", week, true));
    for (const scoped of scopedLimits(view)) {
      rows.push(renderUsageRow(scoped.model || "Scoped", scoped, false));
    }
    for (const other of otherLimits(view)) {
      rows.push(renderUsageRow(other.name, other.limit, Boolean(other.limit.resetsAt)));
    }
    if (rows.length > 0) {
      main.appendChild(h("div", { class: "usage-rows" }, rows));
    } else if (!view.usage) {
      main.appendChild(h("div", { class: "card-note", text: "Usage not fetched yet." }));
    } else {
      main.appendChild(h("div", { class: "card-note", text: "No usage limits in the reply." }));
    }
  }

  if (view.usage && view.usage.extraUsageEnabled) {
    main.appendChild(h("div", { class: "card-note", text: "Extra usage is enabled." }));
  }

  const daysLeft = loginDaysLeftOf(view);
  if (typeof daysLeft === "number" && view.loginState !== "loggedOut" && daysLeft <= 5) {
    let text;
    if (daysLeft < 0) text = "login ended — log in again";
    else if (daysLeft === 0) text = "login ends today";
    else text = `login ends in ${daysLeft} d`;
    main.appendChild(h("div", { class: "login-ends", text }));
  }

  if (view.lastError) {
    main.appendChild(
      h("div", { class: "card-note", text: String(view.lastError), title: String(view.lastError) })
    );
  }

  const actionError = state.errors.accounts[view.id];
  if (actionError) {
    main.appendChild(h("div", { class: "card-note error", text: actionError }));
  }

  card.appendChild(main);

  if (state.editingId === view.id && state.editor) {
    card.appendChild(renderEditor(view));
  }

  return card;
}

function renderLoggedOut() {
  return h("div", { class: "logged-out" }, [
    h("div", { class: "logged-out-title", text: "Logged out — log in again" }),
    h("ol", {}, [
      h("li", {}, [
        "In a terminal run ",
        h("code", { text: "claude" }),
        ".",
      ]),
      h("li", {}, [
        "Run ",
        h("code", { text: "/login" }),
        " and sign in with this account.",
      ]),
      h("li", { text: "seatswap sees the new login — save it here." }),
    ]),
  ]);
}

function renderUsageRow(name, limit, showReset) {
  const level = limitLevel(limit);
  const color = LEVEL_COLORS[level];
  const percent = Number(limit.percent);
  const width = Number.isFinite(percent) ? clamp(percent, 0, 100) : 0;

  const fill = h("span", { class: "usage-fill" });
  fill.style.width = `${width}%`;
  fill.style.backgroundColor = color;

  const percentNode = h("span", { class: `usage-percent lv-${level}` }, [
    h("span", { text: fmtPercent(percent) }),
  ]);
  if (level === "locked") {
    percentNode.appendChild(
      h("span", { class: "lock", text: "🔒", title: "Limit reached" })
    );
  }

  const reset = h("span", { class: "reset-time" });
  if (showReset && limit.resetsAt) {
    reset.dataset.resetsAt = String(limit.resetsAt);
    reset.textContent = resetText(limit.resetsAt);
  }

  return h("div", { class: "usage-row" }, [
    h("span", { class: "usage-name", text: name, title: name }),
    h("span", { class: "usage-track" }, [fill]),
    percentNode,
    reset,
  ]);
}

// ---------------------------------------------------------------------------
// ring gauges
// ---------------------------------------------------------------------------

let gaugeSequence = 0;

function renderGauge(view) {
  const template = document.getElementById("gauge-template");
  const gauge = template.content.firstElementChild.cloneNode(true);
  const session = findLimit(view, "session");
  const week = findLimit(view, "weekly_all");

  setupArc(
    gauge,
    gauge.querySelector(".ring-arc.ring-outer"),
    gauge.querySelector(".grad-session"),
    session,
    view,
    "session"
  );
  setupArc(
    gauge,
    gauge.querySelector(".ring-arc.ring-inner"),
    gauge.querySelector(".grad-week"),
    week,
    view,
    "week"
  );

  setGaugeText(gauge.querySelector(".gauge-session"), session);
  setGaugeText(gauge.querySelector(".gauge-week"), week);
  return gauge;
}

function setGaugeText(node, limit) {
  if (!node) return;
  if (!limit) {
    node.textContent = "";
    return;
  }
  node.textContent = shortPercent(limit.percent);
  node.classList.add(`lv-${limitLevel(limit)}`);
}

function setupArc(gauge, arc, gradient, limit, view, slot) {
  if (!arc) return;
  if (!limit) {
    arc.style.display = "none";
    return;
  }

  const radius = Number(arc.getAttribute("r")) || 1;
  const circumference = 2 * Math.PI * radius;
  const level = limitLevel(limit);
  const base = LEVEL_COLORS[level];

  if (gradient) {
    const id = `gauge-gradient-${++gaugeSequence}`;
    gradient.id = id;
    const start = gradient.querySelector(".grad-start");
    const end = gradient.querySelector(".grad-end");
    if (start) start.setAttribute("stop-color", base);
    if (end) {
      end.setAttribute(
        "stop-color",
        mixHex(base, level === "critical" || level === "locked" ? "#fecaca" : "#ffffff", 0.3)
      );
    }
    arc.setAttribute("stroke", `url(#${id})`);
  } else {
    arc.setAttribute("stroke", base);
  }

  if (level === "critical" || level === "locked") arc.classList.add("glow");

  const target = clamp(Number(limit.percent) || 0, 0, 100);
  const key = `${view.id}:${slot}`;
  const startPercent = state.lastPercents.has(key) ? state.lastPercents.get(key) : 0;

  arc.style.strokeDasharray = String(circumference);
  arc.style.strokeDashoffset = String(circumference * (1 - clamp(startPercent, 0, 100) / 100));
  arc.style.transition = "none";
  arc.style.opacity = target <= 0 ? "0" : "1";

  if (prefersReducedMotion()) {
    arc.style.strokeDashoffset = String(circumference * (1 - target / 100));
  } else {
    void arc.getBoundingClientRect();
    requestAnimationFrame(() => {
      arc.style.transition = "stroke-dashoffset 400ms cubic-bezier(0.22, 1, 0.36, 1)";
      arc.style.strokeDashoffset = String(circumference * (1 - target / 100));
    });
  }

  state.lastPercents.set(key, target);
}

// ---------------------------------------------------------------------------
// inline account editor
// ---------------------------------------------------------------------------

function openEditor(id) {
  const account = findView(id);
  if (!account) return;
  const characters = String(account.abbr || "").split("");
  const validColor = isColor(account.color);
  state.editingId = id;
  state.editor = {
    label: account.label || "",
    abbr1: characters[0] || "",
    abbr2: characters[1] || "",
    // An invalid saved colour leaves the field empty; it is never replaced by
    // a palette colour that would silently be saved over the real value.
    color: validColor ? account.color.toLowerCase() : "",
    confirmRemove: false,
  };
  state.errors.editor = null;
  state.errors.editorColor = validColor
    ? null
    : `The saved colour ${JSON.stringify(account.color)} is not #rrggbb. Choose a colour.`;
  state.errors.preview = null;
  renderMain();
}

function closeEditor() {
  state.editingId = null;
  state.editor = null;
  state.errors.editor = null;
  state.errors.editorColor = null;
  state.errors.preview = null;
  renderMain();
}

function sanitizeAbbrChar(value) {
  const cleaned = String(value || "").toUpperCase().replace(/[^A-Z0-9]/g, "");
  return cleaned.slice(0, 1);
}

function renderEditor(account) {
  const editor = state.editor;
  const wrapper = h("div", { class: "editor" });

  const labelInput = textInput({
    value: editor.label,
    maxlength: "40",
    "aria-label": "Label",
    dataset: { field: "label" },
  });
  labelInput.addEventListener("input", () => {
    editor.label = labelInput.value;
  });

  const firstBox = textInput({
    value: editor.abbr1,
    maxlength: "1",
    class: "abbr-box",
    "aria-label": "First abbreviation character",
    autocapitalize: "characters",
    autocomplete: "off",
    dataset: { field: "abbr1" },
  });
  const secondBox = textInput({
    value: editor.abbr2,
    maxlength: "1",
    class: "abbr-box",
    "aria-label": "Second abbreviation character",
    autocapitalize: "characters",
    autocomplete: "off",
    dataset: { field: "abbr2" },
  });

  firstBox.addEventListener("input", () => {
    const value = sanitizeAbbrChar(firstBox.value);
    firstBox.value = value;
    editor.abbr1 = value;
    if (value && !editor.abbr2 && secondBox) {
      secondBox.focus();
      secondBox.select();
    }
    schedulePreview();
  });
  firstBox.addEventListener("keydown", (event) => {
    if (event.key === "ArrowRight" || event.key === "Tab") {
      if (event.key === "Tab" && event.shiftKey) return;
      if (secondBox) {
        event.preventDefault();
        secondBox.focus();
        secondBox.select();
      }
    }
  });

  secondBox.addEventListener("input", () => {
    const value = sanitizeAbbrChar(secondBox.value);
    secondBox.value = value;
    editor.abbr2 = value;
    if (!value) {
      // keep the first box as typed; focus stays for correction
    }
    schedulePreview();
  });
  secondBox.addEventListener("keydown", (event) => {
    if (event.key === "Backspace" && !secondBox.value) {
      event.preventDefault();
      firstBox.focus();
      firstBox.select();
    } else if (event.key === "ArrowLeft") {
      event.preventDefault();
      firstBox.focus();
      firstBox.select();
    }
  });

  const pasteHandler = (event) => {
    const pasted = (event.clipboardData && event.clipboardData.getData("text")) || "";
    const cleaned = String(pasted).toUpperCase().replace(/[^A-Z0-9]/g, "").slice(0, 2);
    if (!cleaned) return;
    event.preventDefault();
    editor.abbr1 = cleaned[0] || "";
    editor.abbr2 = cleaned[1] || "";
    firstBox.value = editor.abbr1;
    secondBox.value = editor.abbr2;
    (editor.abbr2 ? secondBox : firstBox).focus();
    schedulePreview();
  };
  firstBox.addEventListener("paste", pasteHandler);
  secondBox.addEventListener("paste", pasteHandler);

  const abbrBoxes = h("div", { class: "abbr-boxes" }, [firstBox, secondBox]);
  const abbrField = h("label", { class: "field" }, [
    h("span", { class: "field-label", text: "Abbreviation" }),
    abbrBoxes,
  ]);

  const hexInput = textInput({
    value: editor.color,
    maxlength: "7",
    class: "hex-input",
    "aria-label": "Colour hex value",
    spellcheck: "false",
    dataset: { field: "hex" },
  });

  const swatches = h("div", { class: "swatches" });
  const swatchButtons = [];
  for (const color of PALETTE) {
    const swatch = h("button", {
      type: "button",
      class: "swatch" + (sameColor(color, editor.color) ? " selected" : ""),
      title: color,
      "aria-label": `Colour ${color}`,
    });
    swatch.style.backgroundColor = color;
    swatch.addEventListener("click", () => {
      editor.color = color;
      hexInput.value = color;
      editorColorError(null);
      updateSwatchSelection(swatchButtons, editor.color);
      schedulePreview();
    });
    swatchButtons.push({ button: swatch, color });
    swatches.appendChild(swatch);
  }

  hexInput.addEventListener("input", () => {
    let value = hexInput.value.trim().toLowerCase();
    if (value && !value.startsWith("#")) value = `#${value}`;
    editor.color = value;
    editorColorError(null);
    updateSwatchSelection(swatchButtons, editor.color);
    schedulePreview();
  });
  hexInput.addEventListener("blur", () => {
    if (isColor(editor.color)) {
      editor.color = editor.color.toLowerCase();
      hexInput.value = editor.color;
      schedulePreview();
    }
  });

  const colorField = h("div", { class: "field grow" }, [
    h("span", { class: "field-label", text: "Colour" }),
    h("div", { class: "editor-row" }, [swatches, hexInput]),
    h("div", {
      class: "inline-error",
      id: "editor-color-error",
      hidden: !state.errors.editorColor,
      text: state.errors.editorColor || "",
    }),
  ]);

  const previews = h("div", { class: "previews" });
  for (const size of [16, 22, 32]) {
    const canvas = h("canvas", {
      width: String(size),
      height: String(size),
      dataset: { size: String(size) },
      "aria-label": `Tray icon preview at ${size} pixels`,
    });
    previews.appendChild(
      h("div", { class: "preview-cell" }, [
        canvas,
        h("span", { class: "preview-size", text: `${size} px` }),
      ])
    );
  }

  const previewError = h("div", {
    class: "inline-error",
    id: "preview-error",
    hidden: !state.errors.preview,
    text: state.errors.preview || "",
  });

  const actions = h("div", { class: "editor-actions" }, [
    button("Save", "primary", saveEditor),
    button("Cancel", "ghost", closeEditor),
    h("span", { class: "editor-spacer" }),
    button("Remove account", "danger", () => {
      editor.confirmRemove = true;
      state.errors.editor = null;
      renderMain();
    }),
  ]);

  const errorNode = h("div", {
    class: "inline-error",
    id: "editor-error",
    hidden: !state.errors.editor,
    text: state.errors.editor || "",
  });

  wrapper.appendChild(
    h("div", { class: "editor-row" }, [
      h("label", { class: "field grow" }, [
        h("span", { class: "field-label", text: "Label" }),
        labelInput,
      ]),
      abbrField,
    ])
  );
  wrapper.appendChild(colorField);
  wrapper.appendChild(previews);
  wrapper.appendChild(previewError);

  if (editor.confirmRemove) {
    const name = `${account.abbr || "?"} — ${account.label || "this account"}`;
    wrapper.appendChild(
      h("div", { class: "confirm" }, [
        h("div", {
          class: "confirm-text",
          text: `Remove ${name}? The live login is not touched.`,
        }),
        h("div", { class: "confirm-actions" }, [
          button("Remove", "danger", () => removeAccount(account.id)),
          button("Keep it", "ghost", () => {
            editor.confirmRemove = false;
            renderMain();
          }),
        ]),
      ])
    );
  }

  wrapper.appendChild(errorNode);
  wrapper.appendChild(actions);

  schedulePreview();
  return wrapper;
}

function sameColor(first, second) {
  return String(first || "").toLowerCase() === String(second || "").toLowerCase();
}

function updateSwatchSelection(swatchButtons, color) {
  for (const entry of swatchButtons) {
    entry.button.classList.toggle("selected", sameColor(entry.color, color));
  }
}

let previewTimer = null;
let previewToken = 0;

function schedulePreview() {
  if (previewTimer !== null) clearTimeout(previewTimer);
  previewTimer = setTimeout(() => {
    previewTimer = null;
    drawPreviews();
  }, 110);
}

function clearPreviews() {
  for (const canvas of document.querySelectorAll(".previews canvas")) {
    const context = canvas.getContext("2d");
    if (context) context.clearRect(0, 0, canvas.width, canvas.height);
  }
}

function bytesOf(value) {
  if (Array.isArray(value)) return value;
  if (value instanceof Uint8Array || value instanceof Uint8ClampedArray) return value;
  if (typeof value === "string") {
    try {
      const binary = atob(value);
      const bytes = new Uint8Array(binary.length);
      for (let index = 0; index < binary.length; index += 1) {
        bytes[index] = binary.charCodeAt(index);
      }
      return bytes;
    } catch {
      return [];
    }
  }
  if (value && Array.isArray(value.data)) return value.data;
  return [];
}

function applyPreviewReply(reply) {
  if (!Array.isArray(reply)) {
    throw new Error("icon preview returned an unexpected reply");
  }
  for (const item of reply) {
    if (!isObject(item)) continue;
    const size = Number(item.size);
    if (!Number.isFinite(size) || size <= 0) continue;
    const canvas = document.querySelector(`.previews canvas[data-size="${String(size)}"]`);
    if (!canvas) continue;
    canvas.width = size;
    canvas.height = size;
    const context = canvas.getContext("2d");
    if (!context) continue;
    const source = bytesOf(item.rgba);
    const expected = size * size * 4;
    if (source.length < expected) {
      throw new Error(`icon preview for ${size} px is too short`);
    }
    const image = context.createImageData(size, size);
    for (let index = 0; index < expected; index += 1) {
      image.data[index] = source[index] & 0xff;
    }
    context.putImageData(image, 0, 0);
  }
}

async function drawPreviews() {
  const editor = state.editor;
  if (!editor) return;
  const abbr = `${editor.abbr1 || ""}${editor.abbr2 || ""}`.trim().toUpperCase();
  const color = String(editor.color || "").toLowerCase();
  if (!/^[A-Z0-9]{1,2}$/.test(abbr) || !isColor(color)) {
    clearPreviews();
    return;
  }
  const token = ++previewToken;
  try {
    const reply = await invoke("icon_preview", { abbr, color });
    if (token !== previewToken || !state.editor) return;
    applyPreviewReply(reply);
    if (state.errors.preview) {
      state.errors.preview = null;
      const node = document.getElementById("preview-error");
      if (node) {
        node.textContent = "";
        node.hidden = true;
      }
      fitPanel();
    }
  } catch (error) {
    if (token !== previewToken || !state.editor) return;
    state.errors.preview = errorMessage(error);
    const node = document.getElementById("preview-error");
    if (node) {
      node.textContent = state.errors.preview;
      node.hidden = false;
    }
    fitPanel();
  }
}

function editorAbbr() {
  if (!state.editor) return "";
  return `${state.editor.abbr1 || ""}${state.editor.abbr2 || ""}`.trim().toUpperCase();
}

function editorError(text) {
  state.errors.editor = text;
  const node = document.getElementById("editor-error");
  if (node) {
    node.textContent = text || "";
    node.hidden = !text;
  }
  fitPanel();
}

function editorColorError(text) {
  state.errors.editorColor = text;
  const node = document.getElementById("editor-color-error");
  if (node) {
    node.textContent = text || "";
    node.hidden = !text;
  }
  fitPanel();
}

async function saveEditor() {
  const editor = state.editor;
  if (!editor || !state.editingId) return;
  const account = findView(state.editingId);
  if (!account) {
    closeEditor();
    return;
  }
  const label = String(editor.label || "").trim();
  const abbr = editorAbbr();
  const color = String(editor.color || "").trim().toLowerCase();

  if (!label || label.length > 40) {
    editorError("Enter a label of 1 to 40 characters.");
    return;
  }
  if (!/^[A-Z0-9]{1,2}$/.test(abbr)) {
    editorError("Use 1 or 2 letters A–Z or digits for the abbreviation.");
    return;
  }
  if (!isColor(color)) {
    editorColorError("Use a colour of the form #rrggbb.");
    return;
  }
  editorColorError(null);

  try {
    await invoke("edit_account", {
      key: account.id,
      label,
      abbr,
      color,
    });
    state.errors.editor = null;
    closeEditor();
    afterCommand();
  } catch (error) {
    editorError(errorMessage(error));
  }
}

async function removeAccount(id) {
  try {
    await invoke("remove_account", { key: id });
    state.errors.editor = null;
    closeEditor();
    afterCommand();
  } catch (error) {
    editorError(errorMessage(error));
    renderMain();
  }
}

// ---------------------------------------------------------------------------
// add view (spec §6.1)
// ---------------------------------------------------------------------------

function renderAdd() {
  const root = document.getElementById("view-add");
  if (!root) return;
  clear(root);

  root.appendChild(
    h("div", { class: "view-head" }, [
      button("‹ Back", "ghost", () => setView("main")),
      h("span", { class: "view-heading", text: "Add an account" }),
    ])
  );

  const live = state.snapshot && isObject(state.snapshot.live) ? state.snapshot.live : { kind: "noLogin" };
  const steps = h("ol", { class: "steps" });
  steps.appendChild(renderAddStepOne(live));
  steps.appendChild(
    h("li", {}, [
      h("div", { class: "step-title", text: "Log in with the other account" }),
      h("div", { class: "step-body" }, [
        "In a terminal run ",
        h("code", { text: "claude" }),
        ", then ",
        h("code", { text: "/login" }),
        " with the other account. A ",
        h("code", { text: "/logout" }),
        " first is usually not needed.",
      ]),
    ])
  );
  steps.appendChild(
    h("li", {}, [
      h("div", { class: "step-title", text: "Save the new login" }),
      h("div", {
        class: "step-body",
        text: "seatswap notices the new login and offers “Save as new account” here.",
      }),
    ])
  );
  root.appendChild(steps);

  if (state.errors.add) {
    root.appendChild(h("div", { class: "inline-error", text: state.errors.add }));
  }

  root.appendChild(
    h("div", { class: "settings-actions" }, [
      button("Done", "ghost", () => setView("main")),
    ])
  );

  fitPanel();
}

function renderAddStepOne(live) {
  const item = h("li", {});
  item.appendChild(h("div", { class: "step-title", text: "Save the current login" }));

  if (live.kind === "known") {
    const account = findView(live.accountId);
    const name = account ? `${account.abbr} — ${account.label}` : "the current account";
    item.appendChild(
      h("div", { class: "step-body", text: `Your current account (${name}) is saved.` })
    );
    return item;
  }

  if (live.kind === "unknown") {
    const email = live.email || live.orgName || "unknown login";
    item.appendChild(
      h("div", { class: "step-body", text: `The live login (${email}) is not saved yet.` })
    );
    const label = unknownLabel(live);
    const input = textInput({
      value: label,
      maxlength: "40",
      "aria-label": "Label for the new account",
    });
    input.addEventListener("input", () => {
      state.unknownLabel = input.value;
    });
    item.appendChild(
      h("div", { class: "inline-form" }, [
        h("label", { class: "field" }, [
          h("span", { class: "field-label", text: "Label" }),
          input,
        ]),
        h("div", { class: "banner-actions" }, [
          button("Save", "primary small", () => saveCurrent(state.unknownLabel)),
        ]),
      ])
    );
    return item;
  }

  if (live.kind === "ended") {
    item.appendChild(
      h("div", {
        class: "step-body",
        text: "The live login has ended. Log in again in Claude Code first.",
      })
    );
    return item;
  }

  item.appendChild(
    h("div", {
      class: "step-body",
      text: "No live login found. Run claude once and sign in first.",
    })
  );
  return item;
}

async function saveCurrent(labelValue) {
  const label = String(labelValue || "").trim();
  if (!label || label.length > 40) {
    state.errors.add = "Enter a label of 1 to 40 characters.";
    if (state.view === "add") {
      renderAdd();
    } else {
      state.errors.banner = state.errors.add;
      renderMain();
    }
    return;
  }
  try {
    await invoke("save_current", { label });
    state.errors.add = null;
    afterCommand();
  } catch (error) {
    state.errors.add = errorMessage(error);
    if (state.view === "add") renderAdd();
    else {
      state.errors.banner = state.errors.add;
      renderMain();
    }
  }
}

// ---------------------------------------------------------------------------
// settings view (spec §10)
// ---------------------------------------------------------------------------

function renderSettings() {
  const root = document.getElementById("view-settings");
  if (!root) return;
  clear(root);

  root.appendChild(
    h("div", { class: "view-head" }, [
      button("‹ Back", "ghost", () => setView("main")),
      h("span", { class: "view-heading", text: "Settings" }),
    ])
  );

  if (!state.snapshot) {
    root.appendChild(h("div", { class: "setting-note", text: "Loading settings…" }));
    fitPanel();
    return;
  }

  if (!isObject(state.snapshot.settings)) {
    // accounts.json could not be read: never show defaults that Save would
    // write over the real settings.
    const error = h("div", {
      class: "inline-error",
      id: "settings-error",
      text:
        state.snapshot.error ||
        "The settings could not be read from accounts.json; Save is disabled.",
    });
    root.appendChild(error);
    const disabled = button("Save", "primary", saveSettings);
    disabled.disabled = true;
    root.appendChild(h("div", { class: "settings-actions" }, [disabled]));
    fitPanel();
    return;
  }

  if (!state.settingsDraft) {
    state.settingsDraft = Object.assign({}, state.snapshot.settings);
  }
  const draft = state.settingsDraft;
  const settings = h("div", { class: "settings" });

  settings.appendChild(
    settingRow(
      "Tray text",
      selectInput(
        [
          ["percent", "Percent"],
          ["percentReset", "Percent + reset"],
          ["off", "Off"],
        ],
        draft.trayText,
        (value) => {
          draft.trayText = value;
        }
      ),
      "What the tray label shows."
    )
  );

  settings.appendChild(
    settingRow(
      "Tray ring",
      selectInput(
        [
          ["binding", "Binding limit"],
          ["session", "Session"],
          ["week", "Week"],
        ],
        draft.iconLimit,
        (value) => {
          draft.iconLimit = value;
        }
      ),
      "Which limit the tray icon ring shows."
    )
  );

  settings.appendChild(
    settingRow(
      "Poll active account",
      numberControl(draft, "pollActiveSecs", { min: "30", step: "30" }, "s"),
      "Seconds between usage polls of the live account."
    )
  );

  settings.appendChild(
    settingRow(
      "Poll other accounts",
      numberControl(draft, "pollInactiveSecs", { min: "30", step: "30" }, "s"),
      "Seconds between usage polls of accounts that are not live."
    )
  );

  settings.appendChild(
    settingRow(
      "Suggest a switch at",
      numberControl(draft, "suggestAt", { min: "1", max: "100", step: "1" }, "%"),
      "Show the switch suggestion when the live account reaches this percentage."
    )
  );

  settings.appendChild(
    settingRow(
      "Suggest a target below",
      numberControl(draft, "suggestTargetBelow", { min: "1", max: "100", step: "1" }, "%"),
      "Only suggest an account whose binding limit is below this percentage."
    )
  );

  settings.appendChild(settingRow("Notify on switch", checkboxControl(draft, "notifyOnSwitch")));
  settings.appendChild(
    settingRow(
      "Mirror live login",
      checkboxControl(draft, "mirrorLive"),
      "Keep the stored copy in step when Claude Code's login changes."
    )
  );
  settings.appendChild(
    settingRow(
      "Refresh inactive logins",
      checkboxControl(draft, "refreshInactive"),
      "Refresh the saved logins of accounts that are not live. Off: their last known usage is shown with its age."
    )
  );
  settings.appendChild(settingRow("Start at login", checkboxControl(draft, "startAtLogin")));
  settings.appendChild(
    settingRow(
      "Theme",
      selectInput(
        [
          ["system", "System"],
          ["light", "Light"],
          ["dark", "Dark"],
        ],
        draft.theme,
        (value) => {
          draft.theme = value;
          applyTheme(value);
        }
      )
    )
  );

  const hotkeyInput = textInput({
    value: draft.hotkey || "",
    readonly: true,
    "aria-label": "Hotkey (read only)",
  });
  settings.appendChild(settingRow("Hotkey", hotkeyInput));
  settings.appendChild(
    h("div", { class: "hotkey-note" }, [
      "Bind ",
      h("code", { text: "seatswap panel" }),
      " to a key in your desktop's keyboard settings.",
    ])
  );

  const configInput = textInput({
    value: draft.claudeConfigDir || "",
    placeholder: "/home/you/.claude",
    "aria-label": "Claude config directory",
  });
  configInput.addEventListener("input", () => {
    draft.claudeConfigDir = configInput.value;
    settingsConfigDirError(null);
  });
  const configField = h("div", { class: "field" }, [
    configInput,
    h("div", {
      class: "inline-error",
      id: "claude-config-error",
      hidden: !state.errors.settingsConfigDir,
      text: state.errors.settingsConfigDir || "",
    }),
  ]);
  settings.appendChild(
    settingRow(
      "Claude config folder",
      configField,
      "Empty: $CLAUDE_CONFIG_DIR or Claude Code's default folder."
    )
  );
  settings.appendChild(
    h("div", {
      class: "setting-note",
      text: "Takes effect after restarting seatswap.",
    })
  );

  root.appendChild(settings);

  const errorNode = h("div", {
    class: "inline-error",
    id: "settings-error",
    hidden: !state.errors.settings,
    text: state.errors.settings || "",
  });
  root.appendChild(errorNode);

  const saved = h("span", {
    class: "inline-ok",
    id: "settings-saved",
    hidden: !state.errors.settingsSaved,
    text: "Saved.",
  });

  root.appendChild(
    h("div", { class: "settings-actions" }, [
      button("Save", "primary", saveSettings),
      saved,
    ])
  );

  fitPanel();
}

function settingRow(labelText, control, subText) {
  const label = h("div", { class: "setting-label" }, [
    h("span", { text: labelText }),
  ]);
  if (subText) label.appendChild(h("span", { class: "sub", text: subText }));
  if (
    control &&
    typeof control.matches === "function" &&
    control.matches("input, select, textarea") &&
    !control.getAttribute("aria-label")
  ) {
    control.setAttribute("aria-label", labelText);
  }
  return h("div", { class: "setting" }, [
    label,
    h("div", { class: "setting-control" }, [control]),
  ]);
}

function numberControl(draft, key, attrs, unit) {
  const input = h(
    "input",
    Object.assign(
      {
        type: "number",
        value: draft[key] === null || draft[key] === undefined ? "" : String(draft[key]),
        "aria-label": key,
      },
      attrs
    )
  );
  input.addEventListener("input", () => {
    draft[key] = input.value === "" ? "" : Number(input.value);
  });
  return h("span", { class: "number-control" }, [
    input,
    h("span", { class: "unit", text: unit }),
  ]);
}

function checkboxControl(draft, key) {
  const input = h("input", { type: "checkbox", checked: Boolean(draft[key]), "aria-label": key });
  input.addEventListener("change", () => {
    draft[key] = input.checked;
  });
  return input;
}

function settingsError(text) {
  state.errors.settings = text || null;
  const node = document.getElementById("settings-error");
  if (node) {
    node.textContent = text || "";
    node.hidden = !text;
  }
  const saved = document.getElementById("settings-saved");
  if (saved) saved.hidden = Boolean(text) || !state.errors.settingsSaved;
  fitPanel();
}

function settingsConfigDirError(text) {
  state.errors.settingsConfigDir = text || null;
  const node = document.getElementById("claude-config-error");
  if (node) {
    node.textContent = text || "";
    node.hidden = !text;
  }
  fitPanel();
}

async function saveSettings() {
  if (!state.snapshot || !isObject(state.snapshot.settings)) {
    settingsError("The settings could not be read from accounts.json; re-open the panel.");
    return;
  }
  if (!state.settingsDraft) return;
  const draft = state.settingsDraft;

  if (!Number.isInteger(draft.pollActiveSecs) || draft.pollActiveSecs < 30) {
    settingsError("Poll active account must be a whole number of at least 30 seconds.");
    return;
  }
  if (!Number.isInteger(draft.pollInactiveSecs) || draft.pollInactiveSecs < 30) {
    settingsError("Poll other accounts must be a whole number of at least 30 seconds.");
    return;
  }
  if (!Number.isInteger(draft.suggestAt) || draft.suggestAt < 1 || draft.suggestAt > 100) {
    settingsError("Suggest a switch at must be a whole number between 1 and 100 %.");
    return;
  }
  if (
    !Number.isInteger(draft.suggestTargetBelow) ||
    draft.suggestTargetBelow < 1 ||
    draft.suggestTargetBelow > 100
  ) {
    settingsError("Suggest a target below must be between 1 and 100 %.");
    return;
  }

  const configDir = String(draft.claudeConfigDir || "").trim();
  const payload = {
    trayText: draft.trayText,
    iconLimit: draft.iconLimit,
    pollActiveSecs: draft.pollActiveSecs,
    pollInactiveSecs: draft.pollInactiveSecs,
    suggestAt: draft.suggestAt,
    suggestTargetBelow: draft.suggestTargetBelow,
    notifyOnSwitch: Boolean(draft.notifyOnSwitch),
    mirrorLive: Boolean(draft.mirrorLive),
    refreshInactive: Boolean(draft.refreshInactive),
    hotkey: draft.hotkey,
    startAtLogin: Boolean(draft.startAtLogin),
    theme: draft.theme,
    claudeConfigDir: configDir === "" ? null : configDir,
  };

  try {
    await invoke("save_settings", { settings: payload });
    state.errors.settings = null;
    state.errors.settingsConfigDir = null;
    state.errors.settingsSaved = true;
    Object.assign(draft, payload);
    state.settingsDraft = draft;
    if (state.snapshot) state.snapshot.settings = Object.assign({}, payload);
    applyTheme(payload.theme);
    settingsConfigDirError(null);
    settingsError(null);
    afterCommand();
  } catch (error) {
    const message = errorMessage(error);
    state.errors.settingsSaved = false;
    if (message.startsWith("settings saved; start at login could not be set:")) {
      // The settings themselves are saved; drop the draft and show only the
      // autostart failure.
      if (state.snapshot) state.snapshot.settings = Object.assign({}, payload);
      state.settingsDraft = null;
      renderSettings();
    }
    // A relative claudeConfigDir comes back from the core; keep it at the
    // field that caused it instead of in the panel-wide error at the bottom.
    const configProblem =
      message.includes("claudeConfigDir") || /absolute path/i.test(message);
    if (configProblem) {
      settingsConfigDirError(message);
      settingsError(null);
    } else {
      settingsConfigDirError(null);
      settingsError(message);
    }
  }
}

// ---------------------------------------------------------------------------
// commands
// ---------------------------------------------------------------------------

function afterCommand() {
  if (HAS_TAURI || !state.fixture) return;
  state.snapshot = state.fixture;
  applyTheme();
  if (state.view === "main") renderMain();
  else if (state.view === "add") renderAdd();
  updateAge();
}

async function switchTo(key) {
  return switchWithPolicy(key, "refuse", null);
}

async function switchWithPolicy(key, unsaved, label) {
  const cleanLabel = label === undefined || label === null ? null : String(label).trim();
  if (unsaved === "save" && !cleanLabel) {
    state.errors.banner = "Enter a label of 1 to 40 characters for the new account.";
    renderMain();
    return;
  }
  state.errors.banner = null;
  delete state.errors.accounts[key];
  try {
    await invoke("switch_account", {
      key,
      unsaved,
      label: cleanLabel,
    });
    delete state.errors.accounts[key];
    afterCommand();
  } catch (error) {
    const message = errorMessage(error);
    if (message === "not saved" || message.indexOf("not saved") !== -1) {
      // The next snapshot carries `pending`; leave the question to that banner.
      afterCommand();
      return;
    }
    if (findView(key)) state.errors.accounts[key] = message;
    else state.errors.banner = message;
    renderMain();
  }
}

async function cancelPending() {
  try {
    await invoke("cancel_pending", {});
    afterCommand();
  } catch (error) {
    state.errors.banner = errorMessage(error);
    renderMain();
  }
}

async function refreshNow() {
  if (state.refreshing) return;
  state.refreshing = true;
  updateAge();
  try {
    await invoke("refresh_now", {});
  } catch (error) {
    state.errors.banner = errorMessage(error);
    if (state.view === "main") renderMain();
  } finally {
    if (!HAS_TAURI) {
      state.refreshing = false;
      afterCommand();
      updateAge();
    } else {
      setTimeout(() => {
        if (!state.refreshing) return;
        state.refreshing = false;
        updateAge();
      }, 1500);
    }
  }
}

async function hidePanel() {
  try {
    await invoke("hide_panel", {});
  } catch {
    // the window is going away; nothing useful to show
  }
}

let fitTimer = null;

function fitPanel() {
  const height = document.body.scrollHeight;
  invoke("fit_panel", { height }).catch(() => {});
  if (fitTimer !== null) clearTimeout(fitTimer);
  fitTimer = setTimeout(() => {
    fitTimer = null;
    invoke("fit_panel", { height: document.body.scrollHeight }).catch(() => {});
  }, 450);
}

// ---------------------------------------------------------------------------
// header age and live countdowns
// ---------------------------------------------------------------------------

function updateAge() {
  const node = document.getElementById("poll-age");
  if (!node) return;
  const refresh = document.getElementById("refresh-btn");
  if (refresh) refresh.classList.toggle("refreshing", state.refreshing);
  if (state.refreshing) {
    node.textContent = "refreshing…";
    return;
  }
  if (!state.snapshot) {
    node.textContent = "loading…";
    return;
  }
  if (!state.snapshot.lastPollAt) {
    node.textContent = "never";
    return;
  }
  const time = Date.parse(state.snapshot.lastPollAt);
  if (!Number.isFinite(time)) {
    node.textContent = "never";
    return;
  }
  node.textContent = ageText(Date.now() - time);
}

function updateResetTimes() {
  for (const node of document.querySelectorAll("[data-resets-at]")) {
    const text = resetText(node.getAttribute("data-resets-at"));
    if (node.textContent !== text) node.textContent = text;
  }
}

// ---------------------------------------------------------------------------
// views, keyboard
// ---------------------------------------------------------------------------

function viewFromHash() {
  const raw = String(location.hash || "").replace(/^#/, "").toLowerCase();
  if (raw === "add" || raw === "settings") return raw;
  return "main";
}

function setView(view, options) {
  const wanted = view === "add" || view === "settings" ? view : "main";
  state.view = wanted;

  const main = document.getElementById("view-main");
  const add = document.getElementById("view-add");
  const settingsView = document.getElementById("view-settings");
  if (main) main.hidden = wanted !== "main";
  if (add) add.hidden = wanted !== "add";
  if (settingsView) settingsView.hidden = wanted !== "settings";
  // The gear leads to settings; inside settings it would lead nowhere.
  const settingsBtn = document.getElementById("settings-btn");
  if (settingsBtn) settingsBtn.hidden = wanted === "settings";

  if (wanted === "main") {
    renderMain();
  } else if (wanted === "add") {
    state.errors.add = null;
    renderAdd();
  } else {
    state.settingsDraft = null;
    state.errors.settings = null;
    state.errors.settingsConfigDir = null;
    state.errors.settingsSaved = false;
    renderSettings();
  }

  if (!options || !options.noHash) {
    try {
      history.replaceState(null, "", `#${wanted}`);
    } catch {
      // some webviews refuse history changes; the view still switched
    }
  }
  fitPanel();
}

function setFocused(index) {
  const views = orderedViews(state.snapshot ? state.snapshot.views : []);
  if (views.length === 0) return;
  state.focusedIndex = ((index % views.length) + views.length) % views.length;
  const cards = document.getElementById("cards");
  if (!cards) return;
  for (let child = 0; child < cards.children.length; child += 1) {
    cards.children[child].classList.toggle("focused", child === state.focusedIndex);
  }
  const focused = cards.children[state.focusedIndex];
  if (focused) {
    if (typeof focused.focus === "function") {
      try {
        focused.focus({ preventScroll: true });
      } catch {
        focused.focus();
      }
    }
    if (typeof focused.scrollIntoView === "function") {
      focused.scrollIntoView({ block: "nearest" });
    }
  }
}

function typingTarget(target) {
  if (!target || !(target instanceof HTMLElement)) return false;
  if (target.isContentEditable) return true;
  return Boolean(target.closest("input, textarea, select, [contenteditable='true']"));
}

function onKeyDown(event) {
  if (event.defaultPrevented || event.metaKey || event.ctrlKey || event.altKey) return;
  const target = event.target;

  if (event.key === "Escape") {
    event.preventDefault();
    if (state.editingId) {
      closeEditor();
      return;
    }
    if (state.view !== "main") {
      setView("main");
      return;
    }
    hidePanel();
    return;
  }

  if (typingTarget(target)) return;
  if (state.view !== "main") return;

  const views = orderedViews(state.snapshot ? state.snapshot.views : []);
  const onControl =
    target instanceof HTMLElement && target.closest("button, a, [role='button']");

  if (event.key === "ArrowDown" || event.key === "ArrowUp") {
    if (views.length === 0) return;
    event.preventDefault();
    setFocused(state.focusedIndex + (event.key === "ArrowDown" ? 1 : -1));
    return;
  }

  if (event.key === "Enter") {
    if (views.length === 0 || onControl) return;
    event.preventDefault();
    const view = views[state.focusedIndex];
    if (view) switchTo(view.id);
    return;
  }

  if (/^[1-9]$/.test(event.key)) {
    const view = views[Number(event.key) - 1];
    if (!view) return;
    event.preventDefault();
    switchTo(view.id);
    return;
  }

  if (event.key === ",") {
    event.preventDefault();
    setView("settings");
  }
}

// ---------------------------------------------------------------------------
// built-in fixture: what a browser shows without the tray app
// ---------------------------------------------------------------------------

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

function isoIn(milliseconds) {
  return new Date(Date.now() + milliseconds).toISOString();
}

function makeFixture() {
  const day = 86400000;
  const hour = 3600000;
  const acmeId = "5d1c2a9b-e0d3-4c11-9e21-7c8f1a2b3c4d";
  const privateId = "8b4e7f20-6a1d-4d33-b8c5-2f9a0e6d7c81";
  const dailyId = "c0a1b2d3-e4f5-4a6b-8c7d-9e0f1a2b3c4d";
  return {
    views: [
      {
        id: acmeId,
        label: "Acme",
        abbr: "AC",
        color: "#3b82f6",
        email: "someone@acme.example",
        plan: "Team",
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
        fetchedAt: isoIn(-12000),
        lastError: null,
      },
      {
        id: privateId,
        label: "Private",
        abbr: "P",
        color: "#8b5cf6",
        email: "private@example.com",
        plan: "Ultra",
        isActive: false,
        loginState: "ok",
        loginEndsAtMs: Date.now() + 4 * day,
        loginDaysLeft: 4,
        usage: {
          limits: [
            {
              kind: "weekly_all",
              percent: 31,
              severity: "normal",
              resetsAt: isoIn(3 * day + 5 * hour),
              model: null,
              isActive: true,
            },
            {
              kind: "weekly_scoped",
              percent: 12,
              severity: "normal",
              resetsAt: isoIn(3 * day),
              model: "Opus",
              isActive: false,
            },
          ],
          extraUsageEnabled: false,
        },
        fetchedAt: isoIn(-8 * 60000),
        lastError: null,
      },
      {
        id: dailyId,
        label: "Daily tokens",
        abbr: "DT",
        color: "#f59e0b",
        email: "daily@example.com",
        plan: "Pro",
        isActive: false,
        loginState: "ok",
        loginEndsAtMs: Date.now() + 12 * day,
        loginDaysLeft: 12,
        // Only an Other limit: the card must still show a row, not "No usage limits".
        usage: {
          limits: [
            {
              kind: { other: "daily_tokens" },
              percent: 42,
              severity: "normal",
              resetsAt: isoIn(6 * hour),
              model: null,
              isActive: true,
            },
          ],
          extraUsageEnabled: false,
        },
        fetchedAt: isoIn(-60000),
        lastError: null,
      },
    ],
    activeId: acmeId,
    live: { kind: "known", accountId: acmeId },
    suggestion: {
      fromId: acmeId,
      toId: privateId,
      fromPercent: 97,
      toPercent: 12,
      fromResetsAt: isoIn(72 * 60000),
    },
    warnings: [{ kind: "soon", id: privateId, daysLeft: 4 }],
    pending: null,
    secretsWarning: null,
    lastPollAt: isoIn(-12000),
    error: null,
    settings: Object.assign({}, DEFAULT_SETTINGS),
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
  const account = {
    id: fixtureId(),
    label,
    abbr: fixtureUniqueAbbr(fixture, label),
    color: color || PALETTE[0],
    email: fixture.live.email || null,
    plan: "Ultra",
    isActive: false,
    loginState: "ok",
    loginEndsAtMs: Date.now() + 30 * 86400000,
    loginDaysLeft: 30,
    usage: null,
    fetchedAt: null,
    lastError: null,
  };
  fixture.views.push(account);
  for (const view of fixture.views) view.isActive = view.id === account.id;
  fixture.activeId = account.id;
  fixture.live = { kind: "known", accountId: account.id };
  fixture.suggestion = null;
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

function validateFixtureSettings(settings) {
  const merged = Object.assign({}, DEFAULT_SETTINGS, settings);
  if (!["percent", "percentReset", "off"].includes(merged.trayText)) {
    throw new Error(`invalid trayText ${JSON.stringify(merged.trayText)}`);
  }
  if (!["binding", "session", "week"].includes(merged.iconLimit)) {
    throw new Error(`invalid iconLimit ${JSON.stringify(merged.iconLimit)}`);
  }
  if (!["system", "light", "dark"].includes(merged.theme)) {
    throw new Error(`invalid theme ${JSON.stringify(merged.theme)}`);
  }
  if (!Number.isInteger(merged.pollActiveSecs) || merged.pollActiveSecs < 30) {
    throw new Error(`pollActiveSecs must be at least 30 s, got ${merged.pollActiveSecs}`);
  }
  if (!Number.isInteger(merged.pollInactiveSecs) || merged.pollInactiveSecs < 30) {
    throw new Error(`pollInactiveSecs must be at least 30 s, got ${merged.pollInactiveSecs}`);
  }
  if (!Number.isInteger(merged.suggestAt) || merged.suggestAt < 1 || merged.suggestAt > 100) {
    throw new Error(`suggestAt must be between 1 and 100, got ${merged.suggestAt}`);
  }
  if (
    !Number.isInteger(merged.suggestTargetBelow) ||
    merged.suggestTargetBelow < 1 ||
    merged.suggestTargetBelow > 100
  ) {
    throw new Error(
      `suggestTargetBelow must be between 1 and 100, got ${merged.suggestTargetBelow}`
    );
  }
  if (
    merged.claudeConfigDir !== null &&
    merged.claudeConfigDir !== undefined &&
    !String(merged.claudeConfigDir).startsWith("/")
  ) {
    throw new Error(
      `claudeConfigDir must be an absolute path, got ${merged.claudeConfigDir}`
    );
  }
  return merged;
}

async function fixtureInvoke(command, args) {
  console.log(`[fixture] ${command}`, args);
  const fixture = state.fixture;
  if (!fixture && command !== "fit_panel" && command !== "hide_panel") {
    throw new Error("fixture is not ready");
  }

  switch (command) {
    case "get_snapshot":
      return clone(fixture);

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
      fixture.activeId = target.id;
      fixture.live = { kind: "known", accountId: target.id };
      fixture.suggestion = null;
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
      const account = fixture.views.find(
        (view) => view.id === key || String(view.abbr).toLowerCase() === key.toLowerCase()
      );
      if (!account) throw new Error(`unknown account ${key}`);
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
      if (fixture.views.some((view) => view.id !== account.id && view.abbr === abbr)) {
        throw new Error(`abbreviation ${JSON.stringify(abbr)} is already used by another account`);
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
      account.label = label;
      account.abbr = abbr;
      account.color = color;
      return clone(account);
    }

    case "remove_account": {
      const key = String(args.key || "");
      const index = fixture.views.findIndex(
        (view) => view.id === key || String(view.abbr).toLowerCase() === key.toLowerCase()
      );
      if (index < 0) throw new Error(`unknown account ${key}`);
      const removed = fixture.views[index];
      fixture.views.splice(index, 1);
      if (fixture.live.kind === "known" && fixture.live.accountId === removed.id) {
        fixture.live = {
          kind: "unknown",
          email: removed.email || null,
          orgName: null,
        };
      }
      if (fixture.activeId === removed.id) fixture.activeId = null;
      for (const view of fixture.views) view.isActive = view.id === fixture.activeId;
      fixture.suggestion = null;
      if (Array.isArray(fixture.warnings)) {
        fixture.warnings = fixture.warnings.filter((warning) => warning.id !== removed.id);
      }
      return clone(removed);
    }

    case "save_settings":
      fixture.settings = validateFixtureSettings(args.settings || {});
      fixture.lastPollAt = fixture.lastPollAt || new Date().toISOString();
      return null;

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

    case "fit_panel":
      return null;

    case "hide_panel":
      return null;

    default:
      throw new Error(`unknown command ${command}`);
  }
}

// ---------------------------------------------------------------------------
// boot
// ---------------------------------------------------------------------------

function wireStatic() {
  const refresh = document.getElementById("refresh-btn");
  if (refresh) refresh.addEventListener("click", refreshNow);
  const settings = document.getElementById("settings-btn");
  if (settings) settings.addEventListener("click", () => setView("settings"));
  const close = document.getElementById("close-btn");
  if (close) close.addEventListener("click", hidePanel);
  const add = document.getElementById("add-btn");
  if (add) add.addEventListener("click", () => setView("add"));

  document.addEventListener("keydown", onKeyDown);
  window.addEventListener("hashchange", () => {
    const wanted = viewFromHash();
    if (wanted !== state.view) setView(wanted, { noHash: true });
  });
}

function handleSnapshot(snapshot) {
  if (!isObject(snapshot)) return;
  state.snapshot = snapshot;
  state.refreshing = false;
  applyTheme();

  if (state.view === "main") {
    renderMain();
  } else if (state.view === "add") {
    const active = document.activeElement;
    const typing =
      active && typeof active.closest === "function"
        ? active.closest("input, textarea, select")
        : null;
    if (!typing) renderAdd();
  } else if (state.view === "settings") {
    if (!isObject(snapshot.settings)) state.settingsDraft = null;
    if (!state.settingsDraft) renderSettings();
  }
  updateAge();
}

async function boot() {
  wireStatic();
  wireTheme();
  if (!HAS_TAURI) {
    state.fixture = makeFixture();
    console.info("[fixture] no window.__TAURI__; using the built-in snapshot");
  }
  applyTheme();
  setView(viewFromHash(), { noHash: true });

  await listen("snapshot", (payload) => handleSnapshot(payload));
  await listen("navigate", (payload) => {
    const view = typeof payload === "string" ? payload : payload && payload.view;
    if (view === "main" || view === "add" || view === "settings") setView(view);
  });

  try {
    const snapshot = await invoke("get_snapshot", {});
    handleSnapshot(snapshot);
  } catch (error) {
    state.errors.banner = errorMessage(error);
    if (state.view === "main") renderMain();
  }

  setInterval(updateAge, 1000);
  setInterval(updateResetTimes, 20000);
  updateAge();
}

if (document.readyState === "loading") {
  document.addEventListener("DOMContentLoaded", boot, { once: true });
} else {
  boot();
}
