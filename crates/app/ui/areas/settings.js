// The Settings area of the large window (spec §3.7): eight groups on the left,
// the chosen group's card on the right. Every control saves one field at once
// through `save_setting`; a refused save shows its error under the row and puts
// the control back to the saved value. A snapshot updates the open group in
// place from the stored views; it never re-reads them. Plain ES2020, no
// framework.

import {
  abbrLetterBoxes,
  applyTheme,
  clear,
  colorSwatches,
  contrastText,
  dateText,
  durationText,
  errorMessage,
  focusInside,
  h,
  hexField,
  iconPreview,
  isColor,
  isObject,
  loginDaysLeft,
  message,
  panel,
  providerName,
  selectInput,
  shortPercent,
  showTokens,
  switchControl,
  textInput,
} from "../common.js";

const GROUPS = [
  { id: "general", label: "General" },
  { id: "accounts", label: "Accounts" },
  { id: "auto-burn", label: "Auto-burn" },
  { id: "notifications", label: "Notifications" },
  { id: "projects", label: "Projects" },
  { id: "agents", label: "Agents" },
  { id: "providers", label: "Providers" },
  { id: "history", label: "History and hosts" },
];

const ALIASES = {
  general: "general",
  gen: "general",
  accounts: "accounts",
  acc: "accounts",
  "auto-burn": "auto-burn",
  autoburn: "auto-burn",
  burn: "auto-burn",
  notifications: "notifications",
  noti: "notifications",
  projects: "projects",
  proj: "projects",
  agents: "agents",
  providers: "providers",
  prov: "providers",
  history: "history",
  hist: "history",
};

const MISSING = "not measured yet: it needs a few days of work";

const state = {
  snapshot: null,
  settings: null,
  context: null,
  container: null,
  group: "general",
  // The group the last `renderGroup` drew: a redraw of that group keeps the
  // body's scroll position; another group starts at the top.
  drawnGroup: null,
  sub: null,
  // The item an Accounts address named (a stored account's id or a provider
  // subscription's `account.acc`), or null outside Accounts. The card looks it
  // up again in every snapshot it draws, so a snapshot that drops the account
  // says "No account {id}." and selects nothing.
  addressItem: null,
  // The account whose chip editor is open, and the row the target marks.
  openEditorId: null,
  selectedId: null,
  confirmRemoveId: null,
  // The Codex login whose "Remove saved login" waits for a second click (R8):
  // `saved:<org>:<acc>`, or null.
  confirmSavedLoginId: null,
  confirmRemoveOrgId: null,
  // The organization edit waiting for its confirmation: `{ kind: "orgJoin",
  // org, orgName, into, intoName }` or `{ kind: "orgUnjoin", org, orgName }`.
  // One field for both kinds, so no confirm outlives the one under it.
  confirmOrgEdit: null,
  right: null,
  errors: Object.create(null),
  // "seatswap <version>" above the Money section: the command is asked once
  // when the area mounts, never again on a snapshot or a group change.
  version: null,
  versionError: null,
  versionLoaded: false,
  // The account ids the last snapshot held: only a change in this list makes
  // the Accounts group read the projects again.
  accountIds: null,
  // The agents' skill and calls, read when the Agents group opens.
  skill: null,
  skillError: null,
  skillLoaded: false,
  skillToken: 0,
  calls: null,
  callsError: null,
  callsLoaded: false,
  callsToken: 0,
  // The money view from the look-up thread, the price list under Providers,
  // and the request tokens that keep a slow reply from overwriting a newer one.
  money: null,
  moneyError: null,
  moneyLoaded: false,
  moneyToken: 0,
  moneyListening: false,
  // The money rows and the price block are built again only when the view
  // they show changed (decision 25: no flicker on a snapshot): the key is the
  // JSON of that part of the view, and the node is the one kept while it holds.
  moneyKey: null,
  moneyNode: null,
  priceRows: null,
  priceError: null,
  priceLoaded: false,
  priceToken: 0,
  priceKey: null,
  priceNode: null,
  // Whether the API price list is open; it starts closed and the same
  // control opens and closes it (decision 27).
  priceOpen: false,
  // Saves run one after another, each built from the settings as the one
  // before left them, so a quick second change never carries a stale copy.
  saving: Promise.resolve(),
  pendingSaves: 0,
  // The resolved projects and organizations (spec §3.6), and the request token
  // that keeps a slow reply from overwriting a newer one.
  projects: null,
  projectsError: null,
  projectsLoaded: false,
  projectsToken: 0,
  // The host folders of the history (spec §1.5), the view loaded when the
  // group is opened, and what the export and import rows are holding.
  history: null,
  historyError: null,
  historyLoaded: false,
  historyToken: 0,
  exportPath: "",
  exportResult: "",
  importFolder: "",
  importResult: "",
  importConflicts: [],
};

// A snapshot read before a save landed must not undo it: while saves are
// pending, the page keeps its own settings.
function takeSettings(snapshot) {
  if (state.pendingSaves > 0) return;
  state.settings = isObject(snapshot.settings) ? snapshot.settings : null;
}

function groupFrom(sub) {
  if (typeof sub !== "string" || sub === "") return { group: "general", item: null };
  const mark = sub.indexOf("?");
  const name = (mark < 0 ? sub : sub.slice(0, mark)).toLowerCase();
  const item = mark < 0 ? null : sub.slice(mark + 1);
  return { group: ALIASES[name] || null, item };
}

// ---------------------------------------------------------------------------
// the page and its eight groups
// ---------------------------------------------------------------------------

// The `accounts` list of the real snapshot, which always has it. A snapshot
// without it is an error the page shows; it is never read as an empty list.
function accountsOf(snapshot) {
  if (!Array.isArray(snapshot.accounts)) {
    throw new Error("the snapshot has no accounts list");
  }
  return snapshot.accounts;
}

// The account ids a snapshot holds, as one comparable text: only a changed
// list makes the Accounts group read the projects again.
function accountIdsOf(snapshot) {
  return accountsOf(snapshot)
    .map((row) => (isObject(row) && typeof row.id === "string" ? row.id : ""))
    .filter((id) => id !== "")
    .sort()
    .join("\u0000");
}

// The provider subscriptions the snapshot carries (spec R4), listed under the
// Claude accounts in the same table. A snapshot without the list is an error
// the page shows; it is never read as an empty list.
function providerSubscriptionsOf(snapshot) {
  if (!Array.isArray(snapshot.providerSubscriptions)) {
    throw new Error("the snapshot has no provider subscriptions list");
  }
  return snapshot.providerSubscriptions;
}

function providerSubscriptionRows() {
  return providerSubscriptionsOf(state.snapshot || {});
}

// The crate's version, asked once when the Settings area mounts. A refused
// command keeps the line and puts its reason in the tooltip; it is never a
// silent blank.
async function loadVersion(context) {
  if (state.versionLoaded || !context) return;
  state.versionLoaded = true;
  try {
    const version = await context.invoke("app_version", {});
    if (typeof version !== "string" || version === "") {
      throw new Error("app_version returned an unexpected reply");
    }
    state.version = version;
    state.versionError = null;
  } catch (error) {
    state.version = null;
    state.versionError = errorMessage(error);
  }
}

// The card's first body line, above the Money section (R7).
function versionLine() {
  if (typeof state.version === "string" && state.version !== "") {
    return h("div", { class: "lr mf", text: `seatswap ${state.version}` });
  }
  return h("div", {
    class: "lr mf",
    title: state.versionError || "the app did not report its version",
    text: "seatswap — version not known",
  });
}

// Open a group: replace the old content only once the views it needs are
// stored, so the old group stays visible while it loads.
export async function render(container, context) {
  const snapshot = isObject(context.snapshot) ? context.snapshot : {};
  const parsed = groupFrom(context.sub);
  const group = parsed.group;
  if (!state.versionLoaded) await loadVersion(context);
  const groupChanged = group !== state.group;
  const sameTarget = state.sub === context.sub;
  const openRequest = context.fromOpen === true;
  // One address form: `settings/accounts?<id>` names a stored account or a
  // provider subscription. `groupFrom` is the only parser; this looks the item
  // up in the snapshot the address was opened with.
  const item =
    group === "accounts" && typeof parsed.item === "string" && parsed.item !== ""
      ? parsed.item
      : null;
  const target = item === null ? null : accountTarget(snapshot, item);
  // An address change under Accounts is navigation, not a snapshot: it applies
  // even while a field has focus, because it names the row to show.
  const addressChange = group === "accounts" && (!sameTarget || openRequest);
  if (groupChanged) {
    state.confirmRemoveOrgId = null;
    state.confirmOrgEdit = null;
  }
  state.sub = context.sub;
  // The address's item is kept for every Accounts redraw; `accountsCard` looks
  // it up again in the snapshot it draws.
  state.addressItem = item;
  // A Claude account opens its chip editor when the address changed or an
  // `open-area` asked for it; a plain re-render keeps the editor as it is. A
  // provider subscription is a row of its own and has no chip editor.
  if (item !== null && (!sameTarget || openRequest)) {
    state.selectedId = target && target.account ? target.account.id : null;
    state.openEditorId = target && target.account ? target.account.id : null;
  }
  if (
    focusInside(state.container) &&
    group === state.group &&
    state.container === container &&
    !openRequest &&
    !addressChange
  ) {
    // Keep the control (and what was typed) until it is saved on change.
    container.classList.add("settings-fixed");
    state.snapshot = snapshot;
    takeSettings(snapshot);
    state.context = context;
    return;
  }
  container.classList.add("settings-fixed");
  state.snapshot = snapshot;
  takeSettings(snapshot);
  state.context = context;
  state.container = container;
  state.group = group;
  state.accountIds = accountIdsOf(snapshot);
  listenMoney(context);
  if ((group === "general" || group === "providers") && (groupChanged || !state.moneyLoaded)) {
    await loadMoney(context);
  }
  if (group === "providers" && (groupChanged || !state.priceLoaded)) {
    await loadPriceList();
  }
  if ((group === "accounts" || group === "projects") && (groupChanged || !state.projectsLoaded)) {
    await loadProjects();
  }
  // The hosts table is read when the group opens; a snapshot in between
  // redraws it from the stored view.
  if (group === "history" && (groupChanged || !state.historyLoaded)) await loadHistory();
  if (group === "agents") {
    if (groupChanged || !state.skillLoaded) await loadSkillView();
    if (groupChanged || !state.callsLoaded) await loadCallsView();
  }

  // `clear` below builds a new group body, so a full redraw of the group drawn
  // before reads the old body's scroll here and hands it to `renderGroup`. A
  // changed group, an address change or an `open-area` request starts at the
  // top as before.
  const keptScroll =
    state.drawnGroup === state.group && !addressChange && !openRequest
      ? groupScroll(container)
      : undefined;
  clear(container);
  const right = h("div", { class: "col settings-col" });
  const grid = h("div", { class: "g settings-area" }, [
    h("div", { class: "hcard" }, [
      h(
        "div",
        { class: "snav", role: "tablist", "aria-label": "Setting group" },
        GROUPS.map((group) => groupButton(group, right))
      ),
    ]),
    right,
  ]);
  container.appendChild(grid);
  renderGroup(right, state.group, keptScroll);
  if (addressChange && target) {
    // A later render can change the group or the address while this one waits
    // for `loadProjects`: reveal only when the page this render drew is
    // Accounts and the row's field is still the one the current address names.
    // A missing row while Accounts is drawn stays an error in
    // `revealAccountRow`, as before.
    let currentTarget = null;
    if (state.group === "accounts") {
      const current = groupFrom(state.sub);
      currentTarget =
        current.group === "accounts" && typeof current.item === "string" && current.item !== ""
          ? accountTarget(state.snapshot || {}, current.item)
          : null;
    }
    if (currentTarget !== null && currentTarget.field === target.field) {
      revealAccountRow(target.field);
    }
  }
}

// A snapshot: store it, then redraw the open group from the stored views. No
// command is sent for a redraw. While a field has focus nothing is redrawn;
// the next snapshot after it lost focus draws. The only view read here is the
// projects view, and only when the snapshot's account list changed.
export async function update(snapshot) {
  const next = isObject(snapshot) ? snapshot : {};
  state.snapshot = next;
  takeSettings(next);
  if (!state.container || !state.right) return;
  if (focusInside(state.container)) return;
  const group = state.group;
  const right = state.right;
  const ids = accountIdsOf(next);
  const accountsChanged = state.accountIds !== null && ids !== state.accountIds;
  state.accountIds = ids;
  if (accountsChanged && group === "accounts") await loadProjects();
  if (state.right !== right || state.group !== group || focusInside(state.container)) return;
  renderGroup(right, group);
}

function groupButton(group, right) {
  const current = group.id === state.group;
  return h(
    "button",
    {
      type: "button",
      class: `snb${current ? " ison" : ""}`,
      role: "tab",
      "aria-selected": current ? "true" : "false",
      onclick: () => selectGroup(group.id, right),
    },
    group.label
  );
}

function selectGroup(id, right) {
  state.openEditorId = null;
  state.selectedId = null;
  state.confirmRemoveId = null;
  state.confirmRemoveOrgId = null;
  state.confirmOrgEdit = null;
  state.confirmSavedLoginId = null;
  const target = `settings/${id}`;
  if (location.hash.slice(1) === target) {
    state.group = id;
    renderGroup(right, id);
    return;
  }
  location.hash = target;
}

// The element whose scrollTop changes when a Settings group's content scrolls:
// the card's `.settings-body` (the area itself has `overflow: hidden` while
// `.settings-fixed` is on). Without `top` it reads the position; with one it
// sets it. One helper for every `renderGroup` caller.
function groupScroll(right, top) {
  const body = right.querySelector(".settings-body");
  if (!body) return 0;
  if (top === undefined) return body.scrollTop;
  body.scrollTop = top;
  return body.scrollTop;
}

function renderGroup(right, id, keptScroll) {
  // Read the old body before `clear`: a redraw of the group drawn before keeps
  // its position, another group starts at the top. `render` hands the position
  // on because it builds a new body before calling this.
  let scroll = keptScroll;
  if (scroll === undefined) {
    scroll = state.drawnGroup === id ? groupScroll(right) : 0;
  }
  clear(right);
  state.group = id;
  state.drawnGroup = id;
  state.right = right;
  if (!GROUPS.some((group) => group.id === id)) {
    right.appendChild(panel("Settings", "", [message("Unknown setting group.")]));
    groupScroll(right, scroll);
    return;
  }
  if (id === "general") right.appendChild(generalCard());
  else if (id === "accounts") right.appendChild(accountsCard());
  else if (id === "auto-burn") right.appendChild(autoBurnCard());
  else if (id === "notifications") right.appendChild(notificationsCard());
  else if (id === "projects") right.appendChild(projectsCard());
  else if (id === "providers") right.appendChild(providersCard());
  else if (id === "history") right.appendChild(historyCard());
  else right.appendChild(agentsCard());
  groupScroll(right, scroll);
}

function section(text) {
  return h("div", { class: "sh", text });
}

function setting(label, control, note, field) {
  const text = field ? state.errors[field] || "" : "";
  const error = text ? h("div", { class: "row-error", text }) : null;
  const noteNode = note
    ? typeof note === "string"
      ? h("span", { class: "mf" }, note)
      : note
    : null;
  const children = [
    h("div", { class: "set" }, [
      h("span", {}, label),
      h("span", {}, [control, noteNode]),
    ]),
  ];
  if (error) children.push(error);
  return h("div", { class: "set-row", dataset: field ? { field } : {} }, children);
}

function note(children) {
  return h("div", { class: "note" }, children);
}

// ---------------------------------------------------------------------------
// saving: one control, one field, no Save button
// ---------------------------------------------------------------------------

function rowErrorNode(row) {
  let node = row.querySelector(".row-error");
  if (!node) {
    node = h("div", { class: "row-error" });
    row.appendChild(node);
  }
  return node;
}

// A row's stored error, added only when there is one: an empty error element
// would still be a node the page's error check reports.
function paintRowError(row, field) {
  const text = state.errors[field];
  if (!text) return;
  const node = rowErrorNode(row);
  node.textContent = text;
  node.hidden = false;
}

function fieldError(field, text) {
  state.errors[field] = text || null;
  if (!state.container) return;
  const rows = state.container.querySelectorAll(`[data-field="${field}"]`);
  for (const row of rows) {
    const existing = row.querySelector(".row-error");
    if (!text) {
      if (existing) existing.remove();
      continue;
    }
    const node = rowErrorNode(row);
    node.textContent = text;
    node.hidden = false;
  }
}

// Every save (a setting or an account field) runs through one queue, one after
// another, so a quick second change keeps the first.
function queuedSave(run) {
  state.pendingSaves += 1;
  const task = state.saving.then(run);
  state.saving = task.catch(() => false).finally(() => {
    state.pendingSaves -= 1;
  });
  return task;
}

function saveField(field, value) {
  return queuedSave(() => saveNow(field, value));
}

// An account field saves through the same `edit_account` the CLI uses. The
// refused save's error stays under that account's row and the control goes
// back to the stored value at the caller.
function saveAccount(account, patch, field) {
  return queuedSave(async () => {
    try {
      await state.context.invoke("edit_account", { key: account.id, ...patch });
      fieldError(field, null);
      return true;
    } catch (error) {
      fieldError(field, errorMessage(error));
      return false;
    }
  });
}

// A provider subscription's field saves through `edit_subscription`. The
// refusals are the same as an account edit's: the error stays under the row and
// the caller puts the control back.
function saveSubscription(subscription, patch, field) {
  return queuedSave(async () => {
    try {
      await state.context.invoke("edit_subscription", {
        account: subscription.account,
        ...patch,
      });
      fieldError(field, null);
      return true;
    } catch (error) {
      fieldError(field, errorMessage(error));
      return false;
    }
  });
}

async function saveNow(field, value) {
  if (!state.settings) {
    fieldError(field, "The settings could not be read; nothing is saved.");
    return false;
  }
  try {
    const saved = await state.context.invoke("save_setting", { field, value });
    if (!isObject(saved) || !isObject(saved.settings)) {
      throw new Error("save_setting returned an unexpected reply");
    }
    state.settings = saved.settings;
    if (state.snapshot) state.snapshot.settings = saved.settings;
    // A warning is not a refused save: the settings are saved. It stays under
    // the row like an error, and the next successful save of the field clears
    // it.
    const warning =
      typeof saved.warning === "string" && saved.warning !== "" ? saved.warning : null;
    fieldError(field, warning);
    return true;
  } catch (error) {
    fieldError(field, errorMessage(error));
    return false;
  }
}

function savedValue(field) {
  return state.settings ? state.settings[field] : undefined;
}

// The mock-up gives every select the same 170 px width; the two organization
// selects and "For sessions on" are 130 px, and the currency is 110 px.
function selectWidth(select, width = 170) {
  select.style.width = `${width}px`;
  select.style.flex = "0 0 auto";
  return select;
}

function themeControl(value) {
  const select = selectInput(
    [
      ["system", "System"],
      ["light", "Light"],
      ["dark", "Dark"],
    ],
    value,
    async (next) => {
      applyTheme(next);
      select.disabled = true;
      const ok = await saveField("theme", next);
      if (!ok) applyTheme(savedValue("theme"));
      select.value = String(savedValue("theme"));
      select.disabled = false;
    }
  );
  select.setAttribute("aria-label", "Theme");
  return selectWidth(select);
}

function selectSetting(field, options, label, width) {
  const select = selectInput(options, savedValue(field), async (next) => {
    select.disabled = true;
    await saveField(field, next);
    select.value = String(savedValue(field));
    select.disabled = false;
  });
  if (label) select.setAttribute("aria-label", label);
  return selectWidth(select, width);
}

function toggle(field, label, words = true) {
  const control = switchControl(
    Boolean(savedValue(field)),
    label,
    async (next) => {
      control.button.disabled = true;
      const ok = await saveField(field, next);
      if (!ok) control.set(Boolean(savedValue(field)));
      control.button.disabled = false;
    },
    { words }
  );
  return control.node;
}

// What a number field holds, for the Rust side to check: its number when the
// text is a number, the text itself when it is not, and null when it is empty.
function typedNumber(raw) {
  const text = String(raw).trim();
  if (text === "") return null;
  const value = Number(text);
  return Number.isFinite(value) ? value : text;
}

function numberField(field, label, options) {
  const value = savedValue(field);
  const input = h("input", {
    type: "number",
    class: "inp num",
    value: value === null || value === undefined ? "" : String(value),
    step: String(options.step || 1),
    "aria-label": label,
  });
  const save = async () => {
    await saveField(field, typedNumber(input.value));
    const saved = savedValue(field);
    input.value = saved === null || saved === undefined ? "" : String(saved);
  };
  input.addEventListener("change", save);
  input.addEventListener("keydown", (event) => {
    if (event.key === "Enter") {
      event.preventDefault();
      input.blur();
    }
  });
  return h("span", { class: "number-control" }, [
    input,
    h("span", { class: "mf", text: options.unit || "" }),
  ]);
}

const RESTART_NOTE = "takes effect after the tray app restarts";

// The one text-setting field. `options` holds `placeholder`, `note` (shown
// under the row), `restart` (true when the saved value differs from the one in
// effect: the restart note then replaces `note`) and `width`. Every text
// setting saves the same way; a history field reads its view again afterwards.
function textField(field, label, options) {
  const settings = options || {};
  const value = savedValue(field);
  const input = textInput({
    value: value === null || value === undefined ? "" : String(value),
    placeholder: settings.placeholder,
    "aria-label": label,
  });
  input.className = "inp tall";
  if (settings.width) input.style.width = `${settings.width}px`;
  const noteNode = h("span", { class: "mf" });
  const showNote = () => {
    noteNode.textContent = settings.restart === true ? RESTART_NOTE : settings.note || "";
  };
  input.addEventListener("change", async () => {
    const raw = input.value.trim();
    const next = raw === "" ? null : raw;
    const ok = await saveField(field, next);
    if (!ok) {
      input.value = savedValue(field) || "";
      showNote();
      return;
    }
    input.value = next || "";
    if (settings.restart !== undefined) {
      // The saved value now differs from the one in effect; read the view again
      // so the note and the placeholders come from the command, not the page.
      await loadHistory();
      if (state.group === "history" && state.right) renderGroup(state.right, "history");
    }
  });
  showNote();
  return setting(label, input, noteNode, field);
}

// ---------------------------------------------------------------------------
// Money (spec §1.4, §1.4.1; lead decisions 3, 4 and 6)
// ---------------------------------------------------------------------------

async function loadMoney(context) {
  const token = ++state.moneyToken;
  try {
    const view = await context.invoke("money_view", {});
    if (token !== state.moneyToken) return;
    if (isObject(view)) {
      state.money = view;
      state.moneyError = null;
    } else {
      state.moneyError = "money_view returned an unexpected reply";
    }
  } catch (error) {
    if (token !== state.moneyToken) return;
    state.moneyError = errorMessage(error);
  }
  if (token === state.moneyToken) state.moneyLoaded = true;
}

function listenMoney(context) {
  if (state.moneyListening) return;
  state.moneyListening = true;
  Promise.resolve(
    context.listen("money", (view) => {
      if (!isObject(view)) return;
      state.money = view;
      state.moneyError = null;
      state.moneyLoaded = true;
      const draw = async () => {
        // Providers shows the model list: a new view may carry a new list.
        if (state.group === "providers") {
          state.priceLoaded = false;
          await loadPriceList();
        }
        renderMoney();
      };
      draw().catch((error) => console.error(`money event: ${errorMessage(error)}`));
    })
  ).catch((error) => {
    state.moneyListening = false;
    console.error(`money event: ${errorMessage(error)}`);
  });
}

// A change of the look-up thread's view replaces only the rows it feeds; the
// rest of the group keeps its controls and their focus (decision 25).
function renderMoney() {
  if (!state.right) return;
  if (focusInside(state.container)) return;
  if (state.group === "general") {
    const current = state.right.querySelector(".money-rows");
    if (!current) {
      renderGroup(state.right, "general");
      return;
    }
    current.replaceWith(moneyRows());
    return;
  }
  if (state.group === "providers") {
    const current = state.right.querySelector(".price-block");
    if (current) current.replaceWith(priceBlock());
  }
}

// After a save of currency or lookUpPrices: one round now, so the rate and the
// list are fresh at once.
async function refreshMoney() {
  if (!state.context) return;
  const token = ++state.moneyToken;
  try {
    const view = await state.context.invoke("lookup_now", {});
    if (token !== state.moneyToken) return;
    if (isObject(view)) {
      state.money = view;
      state.moneyError = null;
    } else {
      state.moneyError = "lookup_now returned an unexpected reply";
    }
  } catch (error) {
    if (token !== state.moneyToken) return;
    state.moneyError = errorMessage(error);
  }
  if (token !== state.moneyToken) return;
  state.moneyLoaded = true;
  if (state.group === "providers") {
    state.priceLoaded = false;
    await loadPriceList();
  }
  renderMoney();
}

// The page's times all come from `time_texts`; the Rust side formats them.
// A failed `time_texts` is shown once in the open group, not once for every
// node that got its "?".
function showTimeError(error) {
  const text = `The times could not be read: ${errorMessage(error)}`;
  console.warn(`[settings] ${text}`);
  const right = state.right;
  if (!right || right.querySelector("[data-time-error]")) return;
  const node = message(text, "error");
  node.dataset.timeError = "true";
  right.appendChild(node);
}

// The page's times all come from `time_texts`; the Rust side formats them. A
// failed request puts "?" into each node and says why once in the group.
async function fillTimeNodes(requests) {
  if (requests.length === 0 || !state.context) return;
  let texts = null;
  let error = null;
  try {
    texts = await state.context.invoke("time_texts", {
      times: requests.map((request) => ({ at: request.at, style: request.style })),
    });
    if (!Array.isArray(texts) || texts.length !== requests.length) {
      throw new Error("time_texts returned an unexpected reply");
    }
  } catch (problem) {
    error = problem;
  }
  for (let index = 0; index < requests.length; index += 1) {
    const request = requests[index];
    const value = error ? "?" : texts[index] || "";
    // Most rows write `prefix + time + suffix`; a row whose words depend on the
    // form the Rust side chose ("today 10:10", "Thu 10:10") shapes it itself.
    const text = error
      ? `${request.prefix || ""}${value}${request.suffix || ""}`
      : request.text
        ? request.text(value)
        : `${request.prefix || ""}${value}${request.suffix || ""}`;
    if (request.attribute) request.node.setAttribute(request.attribute, text);
    else request.node.textContent = text;
  }
  if (error) showTimeError(error);
}

// The note rows lay their parts out as flex items; a bare " · " text node loses
// its outer spaces there. A span with `white-space: pre` keeps them.
function middotSeparator() {
  return h("span", { class: "middot-sep", text: " · " });
}

function addNotePart(host, node) {
  if (host.childNodes.length > 0) host.appendChild(middotSeparator());
  host.appendChild(node);
}

const STANDARD_CURRENCIES = ["EUR", "USD", "CHF", "GBP"];

function currencyRow() {
  const stored = savedValue("currency");
  const code = typeof stored === "string" && stored !== "" ? stored : null;
  const known = code !== null && STANDARD_CURRENCIES.includes(code);
  let custom;
  let customWrap;

  const reset = () => {
    const current = savedValue("currency");
    const currentCode =
      typeof current === "string" && current !== "" ? current : null;
    select.value = STANDARD_CURRENCIES.includes(currentCode)
      ? currentCode
      : currentCode
        ? "other"
        : "";
    customWrap.hidden = Boolean(currentCode) && STANDARD_CURRENCIES.includes(currentCode);
    custom.value = currentCode && !STANDARD_CURRENCIES.includes(currentCode) ? currentCode : "";
  };

  const select = selectInput(
    [
      ["", "choose…"],
      ...STANDARD_CURRENCIES.map((currency) => [currency, currency]),
      ["other", "other…"],
    ],
    known ? code : code ? "other" : "",
    async (next) => {
      if (next === "other") {
        customWrap.hidden = false;
        custom.focus();
        return;
      }
      select.disabled = true;
      const ok = await saveField("currency", next === "" ? null : next);
      select.disabled = false;
      if (ok) {
        await refreshMoney();
      } else {
        reset();
      }
    }
  );
  select.setAttribute("aria-label", "Currency");
  selectWidth(select, 110);

  custom = textInput({
    value: code && !known ? code : "",
    placeholder: "EUR",
    maxlength: "3",
    class: "inp tall money-other-input",
    "aria-label": "Currency code",
  });
  custom.addEventListener("change", async () => {
    const raw = custom.value.trim();
    const next = raw === "" ? null : raw;
    const ok = await saveField("currency", next);
    if (!ok) {
      custom.value = code || "";
      return;
    }
    await refreshMoney();
  });
  customWrap = h("span", { class: "money-other", hidden: known || !code }, [custom]);
  return setting(
    "Currency",
    h("span", { class: "money-currency" }, [select, customWrap]),
    "for prices and for the value of tokens",
    "currency"
  );
}

function lookUpRow() {
  const control = switchControl(
    Boolean(savedValue("lookUpPrices")),
    "Look up prices and the dollar rate",
    async (next) => {
      control.button.disabled = true;
      const ok = await saveField("lookUpPrices", next);
      if (!ok) {
        control.set(Boolean(savedValue("lookUpPrices")));
        control.button.disabled = false;
        return;
      }
      await refreshMoney();
      control.button.disabled = false;
    }
  );
  return setting(
    "Look up prices and the dollar rate",
    control.node,
    "prices hourly from models.dev · rate daily from the European Central Bank · nothing about you is sent",
    "lookUpPrices"
  );
}

// The row's own value only: the reason it is missing is said once, in the note
// under it. A missing value is "—", never a sentence in the value's place
// (review finding 15).
function rateText(view, code) {
  if (!code || !isObject(view) || !isObject(view.rate)) return "—";
  const value = Number(view.rate.value);
  if (!Number.isFinite(value)) return "—";
  return `${value.toFixed(4)} ${code}`;
}

// "fetched today 10:10": the Rust side's `dayClock` is "10:10" today and names
// the weekday or the date otherwise, so the word "today" follows that form.
function fetchedPart(times, fetchedAt) {
  const node = h("span");
  if (typeof fetchedAt === "string" && fetchedAt !== "") {
    times.push({
      node,
      style: "dayClock",
      at: fetchedAt,
      text: (value) => `fetched ${value.includes(" ") ? value : `today ${value}`}`,
    });
  } else {
    // An old fx.json keeps no fetch time (the documented rule, decision 4).
    node.textContent = "fetched: not recorded";
  }
  return node;
}

// "ECB rate of Fri 13 Nov · European Central Bank via Frankfurter · fetched
// today 10:10" for every currency (decision 4). USD itself needs no look-up;
// with the look-ups off the switch is the reason and the API value is in US
// dollars (decisions 3 and 5, review finding 15).
function rateRow(times) {
  const code = currencyCode();
  const view = isObject(state.money) ? state.money : null;
  const control = h("span", { class: "money-rate", text: rateText(view, code) });
  const note = h("span", { class: "mf money-rate-note" });
  const rate = view && isObject(view.rate) ? view.rate : null;
  const hasRate = rate !== null && Number.isFinite(Number(rate.value));
  if (!code) {
    note.textContent = "set a currency first";
  } else if (code === "USD") {
    note.textContent = "USD itself";
  } else if (view && view.lookUp !== true) {
    note.textContent = "look-ups are off: API values are shown in USD";
  } else if (!hasRate) {
    note.textContent = "no rate yet: API values are shown in USD";
  } else {
    if (typeof rate.date === "string" && rate.date !== "") {
      addNotePart(
        note,
        h("span", { text: `ECB rate of ${dateText(`${rate.date}T00:00:00`)}` })
      );
    }
    if (typeof view.rateSource === "string" && view.rateSource !== "") {
      addNotePart(note, h("span", { text: view.rateSource }));
    }
    addNotePart(note, fetchedPart(times, view.rateFetchedAt));
  }
  const row = setting("1 US dollar is", control, note, "rate");
  const problem =
    view && typeof view.rateError === "string" && view.rateError !== ""
      ? view.rateError
      : null;
  if (problem) {
    const error = rowErrorNode(row);
    error.hidden = false;
    error.textContent = problem;
  }
  return row;
}

function countText(count, singular, plural) {
  return `${count} ${count === 1 ? singular : plural}`;
}

// The stored list's last change, read once: a malformed record is an error the
// page shows, never two zero counts hidden in a sentence (review finding 15).
function lastChangeOf(view) {
  if (!isObject(view) || view.lastChange === null || view.lastChange === undefined) {
    return null;
  }
  const change = view.lastChange;
  if (
    !isObject(change) ||
    typeof change.at !== "string" ||
    change.at === "" ||
    !Array.isArray(change.added) ||
    !Array.isArray(change.changed)
  ) {
    return { error: `the last change could not be read: ${JSON.stringify(change)}` };
  }
  return { at: change.at, added: change.added.length, changed: change.changed.length };
}

// Everything the price look-up's view cannot say: the look-up's failure, a
// missing source or count, an unreadable last change or left-out list. The one
// builder both the API prices row and the Providers block use (finding 15).
function priceProblems(view) {
  const problems = [];
  if (!isObject(view)) {
    if (state.moneyError) problems.push(state.moneyError);
    return problems;
  }
  if (typeof view.lastError === "string" && view.lastError !== "") {
    problems.push(view.lastError);
  }
  if (state.moneyError) problems.push(state.moneyError);
  if (typeof view.pricesSource !== "string" || view.pricesSource === "") {
    problems.push(`the price list's source is missing: ${JSON.stringify(view.pricesSource)}`);
  }
  if (!Number.isInteger(view.modelsWithPrice) || view.modelsWithPrice < 0) {
    problems.push(
      `the API price count is not a whole number of 0 or more: ${JSON.stringify(view.modelsWithPrice)}`
    );
  }
  const change = lastChangeOf(view);
  if (change && change.error) problems.push(change.error);
  if (view.leftOut !== undefined && view.leftOut !== null) {
    if (
      !Array.isArray(view.leftOut) ||
      view.leftOut.some((entry) => typeof entry !== "string" || entry === "")
    ) {
      problems.push(
        `the list of left-out models could not be read: ${JSON.stringify(view.leftOut)}`
      );
    }
  }
  return problems;
}

function priceProblemNode(view, times) {
  const problems = priceProblems(view);
  if (problems.length === 0) return null;
  const node = h("div", { class: "row-error price-list-error" });
  for (const text of problems) node.appendChild(h("div", { text }));
  const stored = h("div");
  if (!isObject(view)) {
    stored.textContent = "the stored list is not known";
  } else if (typeof view.pricesCheckedAt === "string" && view.pricesCheckedAt !== "") {
    stored.appendChild(document.createTextNode("the stored list is from "));
    const day = h("span");
    stored.appendChild(day);
    times.push({ node: day, style: "day", at: view.pricesCheckedAt });
  } else {
    stored.textContent = "nothing is stored yet";
  }
  node.appendChild(stored);
  return node;
}

function leftOutLine(view) {
  const leftOut =
    isObject(view) && Array.isArray(view.leftOut)
      ? view.leftOut.filter((entry) => typeof entry === "string" && entry !== "")
      : [];
  if (leftOut.length === 0) return null;
  return h("div", {
    class: "lr mf price-left-out",
    text: `${countText(leftOut.length, "model", "models")} of models.dev's list ${leftOut.length === 1 ? "was" : "were"} left out`,
    title: leftOut.join("\n"),
  });
}

function apiPricesRow(times) {
  const view = isObject(state.money) ? state.money : null;
  const count =
    isObject(view) && Number.isInteger(view.modelsWithPrice) && view.modelsWithPrice >= 0
      ? view.modelsWithPrice
      : null;
  const text =
    count === null
      ? state.moneyError || isObject(view)
        ? "prices: not read"
        : "no prices yet"
      : `${count} models with a price`;
  // The model list itself is read-only and lives under Providers (decision 7);
  // this row is the look-up's state only.
  const control = h("span", { class: "money-prices-head" }, [h("span", { text })]);

  const note = h("span", { class: "mf money-api-note" });
  if (view) {
    if (typeof view.pricesCheckedAt === "string" && view.pricesCheckedAt !== "") {
      const node = h("span");
      addNotePart(note, node);
      times.push({
        node,
        style: "dayClock",
        at: view.pricesCheckedAt,
        prefix: "checked ",
      });
    }
    const change = lastChangeOf(view);
    if (change && !change.error) {
      const parts = [];
      if (change.added > 0) {
        parts.push(countText(change.added, "new model", "new models"));
      }
      if (change.changed > 0) parts.push(countText(change.changed, "price", "prices"));
      const node = h("span");
      addNotePart(note, node);
      times.push({
        node,
        style: "day",
        at: change.at,
        prefix: "last change ",
        suffix: parts.length > 0 ? `: ${parts.join(", ")}` : "",
      });
    }
  }
  if (note.childNodes.length === 0) note.textContent = "not looked up yet";

  const row = setting("API prices", control, note, "prices");
  const problem = priceProblemNode(view, times);
  if (problem) row.appendChild(problem);
  const leftOut = leftOutLine(view);
  if (leftOut) row.appendChild(leftOut);
  return row;
}

// The General group's money rows, kept while the view they show is unchanged:
// the key is the JSON of the money part, so a snapshot that changes nothing
// else never refills their times (decision 25). The cached node is moved into
// the new card by its caller.
function moneyRows() {
  const key = JSON.stringify({
    view: isObject(state.money) ? state.money : null,
    error: state.moneyError,
    loaded: state.moneyLoaded,
    currency: currencyCode(),
  });
  if (state.moneyNode && state.moneyKey === key) return state.moneyNode;
  const times = [];
  const node = h("div", { class: "col money-rows" }, [
    section("Money"),
    currencyRow(),
    lookUpRow(),
    rateRow(times),
    apiPricesRow(times),
  ]);
  state.moneyNode = node;
  state.moneyKey = key;
  fillTimeNodes(times);
  return node;
}

// Read the price list once per opening or change; the card draws it from the
// stored rows, so a snapshot never asks for it again.
async function loadPriceList() {
  if (!state.context) return;
  const token = ++state.priceToken;
  try {
    const rows = await state.context.invoke("price_list", {});
    if (token !== state.priceToken) return;
    if (!Array.isArray(rows)) throw new Error("price_list returned an unexpected reply");
    state.priceRows = rows;
    state.priceError = null;
  } catch (error) {
    if (token !== state.priceToken) return;
    state.priceRows = null;
    state.priceError = errorMessage(error);
  }
  if (token === state.priceToken) state.priceLoaded = true;
}

// models.dev is the list's only source; the view names it (`pricesSource`).
const MODELS_DEV_URL = "https://models.dev";

// "API prices from models.dev · checked 11:11" (decision 7): the list's source
// as a link, and when the look-up last answered. A missing source is an error
// below the line, never the words "models.dev" put there by the page (finding
// 15).
function pricesLine(times) {
  const view = isObject(state.money) ? state.money : null;
  const source =
    view && typeof view.pricesSource === "string" && view.pricesSource !== ""
      ? view.pricesSource
      : null;
  const line = h("div", { class: "price-line" });
  if (source !== null) {
    line.appendChild(h("span", { class: "price-from", text: "API prices from " }));
    line.appendChild(
      h("a", {
        class: "link",
        href: MODELS_DEV_URL,
        target: "_blank",
        rel: "noreferrer noopener",
        text: source,
      })
    );
  } else {
    line.appendChild(h("span", { class: "price-from", text: "API prices" }));
  }
  const checked = h("span", { class: "mf" });
  if (view && typeof view.pricesCheckedAt === "string" && view.pricesCheckedAt !== "") {
    line.appendChild(middotSeparator());
    addNotePart(checked, h("span", { text: "checked " }));
    const node = h("span");
    checked.appendChild(node);
    times.push({ node, style: "dayClock", at: view.pricesCheckedAt });
  } else {
    line.appendChild(middotSeparator());
    checked.textContent = "not looked up yet";
  }
  line.appendChild(checked);
  return line;
}

// The price block under Providers: the source line, the read-only list and the
// look-up's state. Built again only when the part of the view it shows changed
// (decision 25), so a snapshot never refills its times.
function priceBlock() {
  const view = isObject(state.money) ? state.money : null;
  const key = JSON.stringify({
    source: view && view.pricesSource,
    checkedAt: view && view.pricesCheckedAt,
    pricesError: view && view.lastError,
    leftOut: view && view.leftOut,
    moneyError: state.moneyError,
    rows: state.priceRows,
    listError: state.priceError,
    loaded: state.priceLoaded,
  });
  if (state.priceNode && state.priceKey === key) return state.priceNode;
  const times = [];
  const body = h("div", { class: "price-body", hidden: !state.priceOpen }, [priceList()]);
  const toggle = h("button", {
    type: "button",
    class: "hbtn xs price-toggle",
  });
  const paintToggle = () => {
    toggle.textContent = state.priceOpen ? "Hide" : "Show…";
    toggle.setAttribute("aria-expanded", state.priceOpen ? "true" : "false");
    toggle.setAttribute(
      "aria-label",
      state.priceOpen ? "Hide the API price list" : "Show the API price list"
    );
    body.hidden = !state.priceOpen;
  };
  toggle.addEventListener("click", () => {
    state.priceOpen = !state.priceOpen;
    paintToggle();
  });
  paintToggle();
  const line = pricesLine(times);
  line.appendChild(toggle);
  // The look-up's error and the models it left out are shown once, under
  // the General group's API prices row (spec §11.5), not again here.
  const node = h("div", { class: "price-block" }, [line, body]);
  state.priceNode = node;
  state.priceKey = key;
  fillTimeNodes(times);
  return node;
}

// The rows themselves, read-only: every price is a value, never a field.
function priceList() {
  if (state.priceError) {
    return h("div", { class: "row-error price-list-error", text: state.priceError });
  }
  if (!state.priceLoaded || state.priceRows === null) {
    return h("div", { class: "lr mf", text: "loading the price list…" });
  }
  const rows = state.priceRows;
  if (rows.length === 0) {
    return h("div", {
      class: "lr mf",
      text: "no prices yet: run a look-up with the switch on",
    });
  }
  const list = h("div", { class: "price-list" });
  list.appendChild(
    h("div", { class: "lr hd price-grid" }, [
      h("span", { text: "Model" }),
      h("span", { text: "Input" }),
      h("span", { text: "Output" }),
      h("span", { text: "Cache write" }),
      h("span", { text: "Cache read" }),
      h("span", { text: "" }),
    ])
  );
  const times = [];
  for (const row of rows) list.appendChild(priceRow(row, times));
  list.appendChild(
    h("div", {
      class: "note price-list-note",
      text: "USD per million tokens. A tiered model uses its base price.",
    })
  );
  fillTimeNodes(times);
  return list;
}

// One price as written; a value that is not a number is an error the page
// shows, never a made-up 0.
function priceText(value) {
  const number = Number(value);
  if (!Number.isFinite(number)) {
    throw new Error(`a price is not a number: ${JSON.stringify(value)}`);
  }
  return String(number);
}

// One model's four looked-up prices; `since` says when the price in effect
// started, as a tip on the row.
function priceRow(row, times) {
  const wrap = h("div", { class: "price-wrap" });
  if (typeof row.since === "string" && row.since !== "") {
    times.push({
      node: wrap,
      style: "day",
      at: row.since,
      prefix: "since ",
      attribute: "title",
    });
  }
  const grid = h("div", { class: "lr price-grid" });
  grid.appendChild(h("span", { class: "mono x11", title: row.key, text: row.key }));
  for (const property of ["input", "output", "cacheWrite", "cacheRead"]) {
    grid.appendChild(
      h("span", {
        class: "price-value mono x11",
        text: priceText(row.price ? row.price[property] : undefined),
      })
    );
  }
  const marks = h("span", { class: "price-marks" });
  if (row.tiered) marks.appendChild(h("span", { class: "sw warn", text: "tiered" }));
  grid.appendChild(marks);
  wrap.appendChild(grid);
  return wrap;
}

// ---------------------------------------------------------------------------
// General
// ---------------------------------------------------------------------------

// ---------------------------------------------------------------------------
// Accounts (spec §3.7): a row per account, a chip editor, and the price.
// ---------------------------------------------------------------------------

function accountRows() {
  return accountsOf(state.snapshot || {});
}

// The row an Accounts address names: a stored account's `id` or a provider
// subscription's `account.acc`. The one address form stays `?<item>`
// (`groupFrom` parses it); null means the snapshot holds neither, so the card
// says so instead of quietly opening another row.
function accountTarget(snapshot, item) {
  const account =
    accountsOf(snapshot).find((row) => isObject(row) && row.id === item) || null;
  if (account) return { account, field: `account:${account.id}` };
  const subscription =
    providerSubscriptionsOf(snapshot).find(
      (row) => isObject(row) && isObject(row.account) && row.account.acc === item
    ) || null;
  if (subscription) {
    return {
      subscription,
      field: `subscription:${subscription.account.org}:${subscription.account.acc}`,
    };
  }
  return null;
}

// Bring an address's row into view and put the cursor in its price field. It
// runs after `renderGroup`, once per address change; a later snapshot redraw
// does not call it, and `update` keeps out while a field has focus.
function revealAccountRow(field) {
  if (!state.right) return;
  const row = Array.from(state.right.querySelectorAll(".acc-wrap")).find(
    (node) => node.dataset.field === field
  );
  if (!row) throw new Error(`the ${field} row is not on the page`);
  row.scrollIntoView({ block: "nearest", inline: "nearest" });
  const price = row.querySelector(".acc-price");
  if (!price) throw new Error(`the ${field} row has no price field`);
  price.focus();
}

// The currency code saved under General: three capital letters, or null when
// none is set. Without one, a price says its currency is unknown and
// `amountText` writes "—" instead of a made-up code.
function currencyCode() {
  const value = state.settings && state.settings.currency;
  return typeof value === "string" && /^[A-Z]{3}$/.test(value) ? value : null;
}

function accountPlan(row) {
  return typeof row.plan === "string" && row.plan !== "" ? row.plan : "—";
}

function accountEmail(row) {
  return typeof row.email === "string" && row.email !== "" ? row.email : "—";
}

// "in 4d", "today" or "ended": the same days-left rule and duration spelling
// as the pop-up's "login ends in 4d" (review finding 20, fix point 3).
function loginEndsText(row) {
  const days = loginDaysLeft(row);
  if (days === null) return "—";
  if (days < 0) return "ended";
  if (days === 0) return "today";
  return `in ${durationText(days * 86400)}`;
}

function accountsCard() {
  const card = panel(
    "Accounts",
    "the coloured field holds the abbreviation: click it to change the letters and the colour",
    [],
    [h("span", { class: "mf", text: "The ✎ in the pop-up opens this group." })]
  );
  const body = card.querySelector(".settings-body");
  // The address's item is looked up again in the snapshot this card draws: a
  // snapshot that dropped it says so above the list; it never falls back to
  // the first row.
  const addressTarget =
    state.addressItem === null ? null : accountTarget(state.snapshot || {}, state.addressItem);
  if (state.addressItem !== null && addressTarget === null) {
    body.appendChild(
      h("div", {
        class: "row-error",
        text: `No account ${state.addressItem}.`,
      })
    );
  }
  body.appendChild(
    h("div", { class: "lr hd acc-grid" }, [
      h("span", { text: "Abbr." }),
      h("span", { text: "Label" }),
      h("span", { text: "Account" }),
      h("span", { text: "Plan" }),
      h("span", { text: "Price / month" }),
      h("span", { text: "Auto-burn may use it" }),
      h("span", { text: "Organization" }),
      h("span", { text: "Login ends" }),
    ])
  );
  const rows = accountRows();
  if (rows.length === 0) {
    body.appendChild(h("div", { class: "lr", text: "No accounts yet." }));
  }
  for (const row of rows) body.appendChild(accountRow(row));
  for (const subscription of providerSubscriptionRows()) {
    body.appendChild(subscriptionRow(subscription));
  }
  body.appendChild(
    h("div", { class: "lr acc-add" }, [
      h("span", {
        class: "mf",
        text: "+ Add account: log in with /login in Claude Code, then save it here.",
      }),
    ])
  );
  body.appendChild(
    note([
      h("b", { text: "Account and plan" }),
      " come from the login and cannot be changed here. ",
      h("b", { text: "Price" }),
      " is what you pay per month, in the currency set under General. It turns percent into money, so accounts can be added up. ",
      h("b", { text: "Auto-burn may use it:" }),
      " switch it off for an account that should be left alone. ",
      h("b", { text: "OpenAI" }),
      " subscriptions are the Codex logins seatswap has seen; ",
      h("b", { text: "Ollama" }),
      " has one. Their plan, when the provider names one, is read-only.",
    ])
  );
  return card;
}

function accountRow(row) {
  const field = `account:${row.id}`;
  let accent = isColor(row.color) ? row.color.toLowerCase() : "#555b66";
  let abbr = row.abbr || "?";

  const wrap = h("div", {
    class: `acc-wrap${state.selectedId === row.id ? " selected" : ""}`,
    dataset: { field },
  });
  const chip = h("button", {
    type: "button",
    class: `acc-chip${state.openEditorId === row.id ? " open" : ""}`,
    title: "Change the abbreviation and the colour",
    "aria-label": `Edit account ${abbr}`,
    "aria-expanded": state.openEditorId === row.id ? "true" : "false",
  });
  chip.appendChild(h("span", { text: abbr }));
  chip.appendChild(h("i", { "aria-hidden": "true", text: "\u270E" }));
  const paintChip = () => {
    chip.firstChild.textContent = abbr;
    chip.style.background = accent;
    chip.style.color = contrastText(accent);
    chip.setAttribute("aria-label", `Edit account ${abbr}`);
  };
  paintChip();
  chip.addEventListener("click", () => {
    state.openEditorId = state.openEditorId === row.id ? null : row.id;
    state.selectedId = row.id;
    state.confirmRemoveId = null;
    renderGroup(state.right, "accounts");
  });

  const labelInput = textInput({
    class: "inp tall acc-label",
    value: row.label || "",
    maxlength: "40",
    "aria-label": "Label",
  });
  labelInput.addEventListener("change", async () => {
    const next = labelInput.value.trim();
    if (next === (row.label || "")) {
      labelInput.value = row.label || "";
      return;
    }
    const ok = await saveAccount(row, { label: next }, field);
    if (ok) row.label = next;
    else labelInput.value = row.label || "";
  });

  const code = currencyCode();
  const priceCurrency = () =>
    typeof row.priceCurrency === "string" && row.priceCurrency !== ""
      ? row.priceCurrency
      : null;
  const currencyLabel = h("span", { class: "x11 mf" });
  const currencyMark = h("span", {
    class: "sw warn price-currency-mark",
    hidden: true,
  });
  const paintPriceCurrency = () => {
    currencyMark.hidden = true;
    currencyMark.title = "";
    currencyMark.setAttribute("aria-label", "");
    if (row.price === null || row.price === undefined) {
      if (code) {
        currencyLabel.textContent = code;
        currencyLabel.className = "x11 mf";
      } else {
        currencyLabel.textContent = "currency: set it under General";
        currencyLabel.className = "x11 mf currency-hint";
      }
      return;
    }
    const own = priceCurrency();
    if (own === null) {
      currencyLabel.textContent = "no currency";
      currencyLabel.className = "x11 mf";
      currencyMark.hidden = false;
      currencyMark.title =
        "the price was entered before a currency was set; enter it again";
      currencyMark.setAttribute("aria-label", currencyMark.title);
      return;
    }
    currencyLabel.textContent = own;
    currencyLabel.className = "x11 mf";
    if (code && own !== code) {
      currencyMark.hidden = false;
      currencyMark.title = `in ${own}: enter it again for ${code}`;
      currencyMark.setAttribute("aria-label", currencyMark.title);
    }
  };

  const priceInput = textInput({
    class: "inp tall acc-price",
    value: row.price === null || row.price === undefined ? "" : String(row.price),
    inputmode: "decimal",
    "aria-label": "Price per month",
  });
  const storedPrice = () =>
    row.price === null || row.price === undefined ? "" : String(row.price);
  priceInput.addEventListener("change", async () => {
    const amount = typedNumber(priceInput.value);
    const sameCurrency =
      amount === null || (code !== null && priceCurrency() === code);
    if (amount === row.price && sameCurrency) {
      priceInput.value = storedPrice();
      return;
    }
    const ok = await saveAccount(row, { price: amount }, field);
    if (ok) {
      row.price = amount;
      row.priceCurrency = amount === null ? null : code;
      priceInput.value = storedPrice();
      paintPriceCurrency();
    } else {
      priceInput.value = storedPrice();
    }
  });

  paintPriceCurrency();
  const priceCell = h("div", { class: "price-cell" }, [
    priceInput,
    currencyLabel,
    currencyMark,
  ]);

  const auto = switchControl(
    row.autoBurn === true,
    `Auto-burn may use ${abbr}`,
    async (next) => {
      const ok = await saveAccount(row, { autoBurn: next }, field);
      if (ok) row.autoBurn = next;
      else auto.set(row.autoBurn === true);
    },
    { words: false }
  );

  wrap.appendChild(
    h("div", { class: "lr acc-grid" }, [
      chip,
      labelInput,
      h("span", { class: "acc-text", title: accountEmail(row), text: accountEmail(row) }),
      h("span", { class: "acc-text", title: accountPlan(row), text: accountPlan(row) }),
      priceCell,
      h("div", { class: "acc-switch" }, [auto.node]),
      accountOrgCell(row, field),
      h("span", { class: "acc-text", text: loginEndsText(row) }),
    ])
  );
  paintRowError(wrap, field);

  if (state.openEditorId === row.id) {
    wrap.appendChild(
      chipEditor(row, field, (next) => {
        if (typeof next.abbr === "string") abbr = next.abbr;
        if (typeof next.color === "string") accent = next.color;
        paintChip();
      })
    );
  }
  return wrap;
}

function subscriptionPlan(subscription) {
  if (typeof subscription.plan === "string" && subscription.plan !== "") {
    return subscription.plan;
  }
  return "—";
}

// One provider subscription under the Claude accounts: the same table, the same
// abbreviation/colour editor and price cell, but no auto-burn, organization or
// login of its own. OpenAI's plan comes from the provider (read-only); Ollama's
// is typed here.
function subscriptionRow(subscription) {
  const field = `subscription:${subscription.account.org}:${subscription.account.acc}`;
  let accent = isColor(subscription.color) ? subscription.color.toLowerCase() : "#555b66";
  let abbr = subscription.abbr || "?";

  const wrap = h("div", {
    class: "acc-wrap subscription",
    dataset: { field },
  });
  const chip = h("button", {
    type: "button",
    class: `acc-chip${state.openEditorId === field ? " open" : ""}`,
    title: "Change the abbreviation and the colour",
    "aria-label": `Edit subscription ${abbr}`,
    "aria-expanded": state.openEditorId === field ? "true" : "false",
  });
  chip.appendChild(h("span", { text: abbr }));
  chip.appendChild(h("i", { "aria-hidden": "true", text: "\u270E" }));
  const paintChip = () => {
    chip.firstChild.textContent = abbr;
    chip.style.background = accent;
    chip.style.color = contrastText(accent);
    chip.setAttribute("aria-label", `Edit subscription ${abbr}`);
  };
  paintChip();
  chip.addEventListener("click", () => {
    state.openEditorId = state.openEditorId === field ? null : field;
    state.selectedId = null;
    state.confirmSavedLoginId = null;
    renderGroup(state.right, "accounts");
  });

  const labelInput = textInput({
    class: "inp tall acc-label",
    value: subscription.label || "",
    maxlength: "40",
    "aria-label": "Label",
  });
  labelInput.addEventListener("change", async () => {
    const next = labelInput.value.trim();
    if (next === (subscription.label || "")) {
      labelInput.value = subscription.label || "";
      return;
    }
    const ok = await saveSubscription(subscription, { label: next }, field);
    if (ok) subscription.label = next;
    else labelInput.value = subscription.label || "";
  });

  const code = currencyCode();
  const currencyLabel = h("span", { class: "x11 mf" });
  const paintPriceCurrency = () => {
    if (subscription.price === null || subscription.price === undefined) {
      currencyLabel.textContent = code || "currency: set it under General";
      currencyLabel.className = code ? "x11 mf" : "x11 mf currency-hint";
      return;
    }
    const own =
      typeof subscription.priceCurrency === "string" && subscription.priceCurrency !== ""
        ? subscription.priceCurrency
        : null;
    currencyLabel.textContent = own || "no currency";
    currencyLabel.className = "x11 mf";
    if (code && own !== code) {
      currencyLabel.title = own
        ? `in ${own}: enter it again for ${code}`
        : "the price was entered before a currency was set; enter it again";
    } else {
      currencyLabel.title = "";
    }
  };

  const priceInput = textInput({
    class: "inp tall acc-price",
    value:
      subscription.price === null || subscription.price === undefined
        ? ""
        : String(subscription.price),
    inputmode: "decimal",
    "aria-label": "Price per month",
  });
  const storedPrice = () =>
    subscription.price === null || subscription.price === undefined
      ? ""
      : String(subscription.price);
  priceInput.addEventListener("change", async () => {
    const amount = typedNumber(priceInput.value);
    if (amount === subscription.price) {
      priceInput.value = storedPrice();
      return;
    }
    const ok = await saveSubscription(subscription, { price: amount }, field);
    if (ok) {
      subscription.price = amount;
      subscription.priceCurrency = amount === null ? null : code;
      priceInput.value = storedPrice();
      paintPriceCurrency();
    } else {
      priceInput.value = storedPrice();
    }
  });
  paintPriceCurrency();

  let planControl;
  if (subscription.company === "ollama") {
    const planInput = textInput({
      class: "inp tall acc-label",
      value: typeof subscription.plan === "string" ? subscription.plan : "",
      maxlength: "40",
      "aria-label": "Plan",
    });
    planInput.addEventListener("change", async () => {
      const typed = planInput.value.trim();
      // An empty or blank field clears the plan: the command takes `null`,
      // never an empty text.
      const next = typed === "" ? null : typed;
      const stored = typeof subscription.plan === "string" ? subscription.plan : null;
      if (next === stored) {
        planInput.value = stored || "";
        return;
      }
      const ok = await saveSubscription(subscription, { plan: next }, field);
      if (ok) subscription.plan = next;
      else planInput.value = stored || "";
    });
    planControl = planInput;
  } else {
    planControl = h("span", {
      class: "acc-text",
      title: "the plan comes from the provider",
      text: subscriptionPlan(subscription),
    });
  }

  // The one company-name map lives in common.js; `providerName` throws on a
  // company key it does not know instead of quietly writing "—".
  const company = providerName(subscription.company, undefined);
  company.className = "acc-text acc-company";
  company.title = company.textContent;
  wrap.appendChild(
    h("div", { class: "lr acc-grid" }, [
      chip,
      labelInput,
      company,
      planControl,
      h("div", { class: "price-cell" }, [priceInput, currencyLabel]),
      h("span", { class: "acc-text", text: "—" }),
      h("span", { class: "acc-text", text: "—" }),
      h("span", { class: "acc-text", text: "—" }),
    ])
  );
  paintRowError(wrap, field);

  if (state.openEditorId === field) {
    const editor = chipEditor(
      subscription,
      field,
      (next) => {
        if (typeof next.abbr === "string") abbr = next.abbr;
        if (typeof next.color === "string") accent = next.color;
        paintChip();
      },
      saveSubscription,
      false
    );
    // R8: an OpenAI subscription whose login seatswap has a saved copy of gets
    // the one action that deletes that copy; the row, its plan, price and
    // history stay, and Codex's own login file is never touched. The live login
    // is not offered: the next poll would save its copy again.
    const login = codexLogin(subscription.account);
    if (login) {
      if (login.live === true) editor.appendChild(liveLoginNote(login));
      else editor.appendChild(removeSavedLogin(subscription, field));
    }
    wrap.appendChild(editor);
  }
  return wrap;
}

// The Codex login the snapshot knows for an account, or null (the OpenAI card
// is off, or its copy is gone). Only a saved, non-live login may be removed.
function codexLogin(account) {
  if (!isObject(account) || account.org !== "codex") return null;
  const snapshot = state.snapshot || {};
  const logins = Array.isArray(snapshot.codexLogins) ? snapshot.codexLogins : [];
  return (
    logins.find((entry) => isObject(entry.account) && sameRef(entry.account, account)) ||
    null
  );
}

// The sentence that takes the place of Remove on the live login, muted. It
// comes from the snapshot (the worker's own refusal), never from a second copy
// in this page.
function liveLoginNote(login) {
  const note =
    typeof login.removeNote === "string" && login.removeNote !== ""
      ? login.removeNote
      : null;
  return h("div", { class: "editor-actions" }, [
    h("span", {
      class: "mf x11",
      text: note || "—",
      title: note || "the snapshot carries no removal note for this login",
    }),
  ]);
}

function removeSavedLogin(subscription, field) {
  const account = { acc: subscription.account.acc, org: subscription.account.org };
  const id = `saved:${account.org}:${account.acc}`;
  const abbr = subscription.abbr || "?";
  if (state.confirmSavedLoginId === id) {
    return h("div", { class: "confirm" }, [
      h("div", {
        class: "confirm-text",
        text: `Delete seatswap's copy of ${abbr}? Codex's own login file is not touched.`,
      }),
      h("div", { class: "confirm-actions" }, [
        h("button", {
          type: "button",
          class: "hbtn xs danger",
          text: "Remove saved login",
          onclick: () => forgetCodexLogin(account, field),
        }),
        h("button", {
          type: "button",
          class: "hbtn xs ghost",
          text: "Keep it",
          onclick: () => {
            state.confirmSavedLoginId = null;
            renderGroup(state.right, "accounts");
          },
        }),
      ]),
    ]);
  }
  return h("div", { class: "editor-actions" }, [
    h("button", {
      type: "button",
      class: "hbtn xs danger",
      text: "Remove saved login",
      title:
        "Deletes seatswap's copy of this Codex login. Codex's own login file is not touched.",
      onclick: () => {
        state.confirmSavedLoginId = id;
        renderGroup(state.right, "accounts");
      },
    }),
  ]);
}

async function forgetCodexLogin(account, field) {
  try {
    await state.context.invoke("remove_codex_login", { account });
    state.confirmSavedLoginId = null;
    delete state.errors[field];
    const snapshot = state.snapshot;
    if (snapshot && Array.isArray(snapshot.codexLogins)) {
      snapshot.codexLogins = snapshot.codexLogins.filter(
        (login) => !(isObject(login.account) && sameRef(login.account, account))
      );
    }
    renderGroup(state.right, "accounts");
  } catch (error) {
    fieldError(field, errorMessage(error));
  }
}

function chipEditor(row, field, changed, save = saveAccount, removable = true) {
  let color = isColor(row.color) ? row.color.toLowerCase() : "";

  const preview = iconPreview();
  const schedule = () => {
    preview.schedule(letters.value(), color, (error) => fieldError(field, error));
  };

  const letters = abbrLetterBoxes(row.abbr || "", {
    onInput: schedule,
    onChange: async (abbr) => {
      if (abbr === row.abbr) {
        letters.setValue(row.abbr || "");
        return;
      }
      const ok = await save(row, { abbr }, field);
      if (ok) {
        row.abbr = abbr;
        changed({ abbr });
      } else {
        letters.setValue(row.abbr || "");
      }
    },
  });

  const swatches = colorSwatches(color, async (next) => {
    hex.setValue(next);
    color = next;
    schedule();
    await saveColor(next);
  });

  async function saveColor(next) {
    if (next === row.color) return;
    const ok = await save(row, { color: next }, field);
    if (ok) {
      row.color = next;
      swatches.select(next);
      changed({ color: next });
    } else {
      hex.setValue(row.color || "");
      swatches.select(row.color);
      color = row.color || "";
      schedule();
    }
  }

  const hex = hexField(color, {
    onInput: (next) => {
      color = next;
      schedule();
    },
    onChange: saveColor,
  });

  const remove = removable
    ? h("button", {
        type: "button",
        class: "hbtn xs danger",
        text: "Remove",
        "aria-label": `Remove account ${row.abbr || "?"}`,
        onclick: () => {
          state.confirmRemoveId = row.id;
          renderGroup(state.right, "accounts");
        },
      })
    : null;
  const editor = h("div", { class: "acc-editor" }, [
    h("div", { class: "editor-row" }, [
      h("div", { class: "field" }, [
        h("span", { class: "field-label", text: "Letters" }),
        letters.node,
      ]),
      h("div", { class: "field" }, [
        h("span", { class: "field-label", text: "Colour" }),
        h("div", { class: "color-line" }, [swatches.node, hex.node]),
      ]),
      h("div", { class: "field" }, [
        h("span", { class: "field-label", text: "Tray icon" }),
        preview.node,
      ]),
    ]),
    h("div", { class: "editor-actions" }, remove ? [remove] : []),
  ]);
  if (removable && state.confirmRemoveId === row.id) editor.appendChild(confirmRemove(row, field));
  preview.draw(letters.value(), color).then((error) => {
    if (error) fieldError(field, error);
  });
  return editor;
}

function confirmRemove(row, field) {
  const name = `${row.abbr || "?"} — ${row.label || "this account"}`;
  return h("div", { class: "confirm" }, [
    h("div", {
      class: "confirm-text",
      text: `Remove ${name}? The live login is not touched.`,
    }),
    h("div", { class: "confirm-actions" }, [
      h("button", {
        type: "button",
        class: "hbtn xs danger",
        text: "Remove",
        onclick: () => removeAccount(row, field),
      }),
      h("button", {
        type: "button",
        class: "hbtn xs ghost",
        text: "Keep it",
        onclick: () => {
          state.confirmRemoveId = null;
          renderGroup(state.right, "accounts");
        },
      }),
    ]),
  ]);
}

async function removeAccount(row, field) {
  try {
    await state.context.invoke("remove_account", { key: row.id });
    state.confirmRemoveId = null;
    state.openEditorId = null;
    state.selectedId = null;
    delete state.errors[field];
    const snapshot = state.snapshot;
    if (snapshot) {
      if (Array.isArray(snapshot.accounts)) {
        snapshot.accounts = snapshot.accounts.filter((entry) => entry.id !== row.id);
      }
      if (Array.isArray(snapshot.views)) {
        snapshot.views = snapshot.views.filter((entry) => entry.id !== row.id);
      }
    }
    renderGroup(state.right, "accounts");
  } catch (error) {
    fieldError(field, errorMessage(error));
  }
}

// ---------------------------------------------------------------------------
// Projects and organizations (spec §3.6): names, joins, organizations — the
// same on every host. A change appends one line to the shared project log.
// ---------------------------------------------------------------------------

// Every replacement of state.projects bumps the token, so a `loadProjects`
// reply that started before it cannot overwrite the newer view.
function setProjects(view, error) {
  state.projectsToken += 1;
  state.projects = view;
  state.projectsError = error;
}

async function loadProjects() {
  if (!state.context) return;
  const token = ++state.projectsToken;
  try {
    const view = await state.context.invoke("projects_view", {});
    if (token !== state.projectsToken) return;
    if (!isObject(view)) throw new Error("projects_view returned an unexpected reply");
    setProjects(view, null);
  } catch (error) {
    if (token !== state.projectsToken) return;
    setProjects(null, errorMessage(error));
  }
  if (token === state.projectsToken) state.projectsLoaded = true;
}

function projectKeyText(key) {
  if (!isObject(key) || typeof key.id !== "string") return "?";
  return typeof key.host === "string" && key.host !== ""
    ? `${key.kind}:${key.id}@${key.host}`
    : `${key.kind}:${key.id}`;
}

// A project's name in a list. A folder project's id has a host, so the same
// folder on two hosts can be told apart: "~ (home folder) · atlas".
function projectLabel(name, key) {
  const text = typeof name === "string" && name !== "" ? name : "?";
  const host =
    isObject(key) && typeof key.host === "string" && key.host !== "" ? key.host : null;
  return host ? `${text} · ${host}` : text;
}

function sameRef(first, second) {
  return Boolean(first && second && first.acc === second.acc && first.org === second.org);
}

function accountRefOf(projects, id) {
  if (!projects || !Array.isArray(projects.accountRefs)) return null;
  const entry = projects.accountRefs.find((item) => item.id === id);
  return entry ? entry.account : null;
}

function orgOfAccount(projects, id) {
  const ref = accountRefOf(projects, id);
  if (!ref) return null;
  const orgs = Array.isArray(projects.orgs) ? projects.orgs : [];
  return orgs.find((org) => (org.accounts || []).some((account) => sameRef(account, ref))) || null;
}

// One project edit through the window's command, queued with the settings
// saves so two quick changes keep their order. The reply is the whole new view.
function saveProject(edit, field) {
  return queuedSave(async () => {
    try {
      const view = await state.context.invoke("projects_edit", { edit });
      if (!isObject(view)) throw new Error("projects_edit returned an unexpected reply");
      setProjects(view, null);
      fieldError(field, null);
      if (state.right && (state.group === "projects" || state.group === "accounts")) {
        renderGroup(state.right, state.group);
      }
      return true;
    } catch (error) {
      fieldError(field, errorMessage(error));
      return false;
    }
  });
}

// The Accounts group's organization column: the organizations plus "none".
// One change is one edit for this account: it moves only this account.
function accountOrgCell(row, field) {
  const projects = isObject(state.projects) ? state.projects : null;
  if (!projects) {
    return h("span", {
      class: "acc-text",
      text: "not read",
      title: state.projectsError || "The organizations are read from the history.",
    });
  }
  const current = orgOfAccount(projects, row.id);
  const options = [["", "none"], ...projects.orgs.map((org) => [org.id, org.name])];
  const select = selectInput(options, current ? current.id : "", async (next) => {
    select.disabled = true;
    const ok = await saveAccountOrg(row, next, field);
    if (ok) {
      renderGroup(state.right, "accounts");
      return;
    }
    select.disabled = false;
    const back = orgOfAccount(state.projects, row.id);
    select.value = back ? back.id : "";
  });
  select.setAttribute("aria-label", `Organization of ${row.abbr || "the account"}`);
  return selectWidth(select, 130);
}

function saveAccountOrg(row, orgId, field) {
  return queuedSave(async () => {
    const projects = isObject(state.projects) ? state.projects : null;
    if (!projects) {
      fieldError(field, "The organizations could not be read; nothing is saved.");
      return false;
    }
    const ref = accountRefOf(projects, row.id);
    if (!ref) {
      fieldError(field, "The account is not in the stored list; nothing is saved.");
      return false;
    }
    const current = orgOfAccount(projects, row.id);
    const to = orgId === "" ? null : projects.orgs.find((org) => org.id === orgId) || null;
    if (orgId !== "" && !to) {
      fieldError(field, `unknown organization ${orgId}`);
      return false;
    }
    if ((current ? current.id : "") === (to ? to.id : "")) {
      fieldError(field, null);
      return true;
    }
    // One edit names this one account, so no other account moves.
    const edit = { edit: "accountOrg", account: ref, org: to ? to.id : null };
    try {
      const view = await state.context.invoke("projects_edit", { edit });
      if (!isObject(view)) throw new Error("projects_edit returned an unexpected reply");
      setProjects(view, null);
      fieldError(field, null);
      return true;
    } catch (error) {
      fieldError(field, errorMessage(error));
      await loadProjects();
      return false;
    }
  });
}

function foldersText(folders) {
  if (!isObject(folders)) return "—";
  const parts = [];
  for (const host of Object.keys(folders).sort()) {
    const list = Array.isArray(folders[host]) ? folders[host] : [];
    parts.push(`${host}: ${list.join(", ")}`);
  }
  return parts.length > 0 ? parts.join(" · ") : "—";
}

// Where an organization's identity came from (spec §11.6): the words the
// mock-up writes in an organization's heading and under "merged in".
const ORG_ORIGIN = {
  owner: "from the owner of the repositories on GitHub",
  login: "from an account's login",
  added: "made here",
};

const MERGED_ORIGIN = {
  owner: "owner on GitHub, also its new repositories",
  login: "from an account's login",
  added: "made here",
};

function orgOrigin(org) {
  if (!Object.prototype.hasOwnProperty.call(ORG_ORIGIN, org.source)) {
    throw new Error(
      `the organization ${org.id} has no origin: ${JSON.stringify(org.source)}`
    );
  }
  return org.source;
}

function orgEmpty(org) {
  if (typeof org.empty !== "boolean") {
    throw new Error(
      `the organization ${org.id} has no empty flag: ${JSON.stringify(org.empty)}`
    );
  }
  return org.empty;
}

// A malformed account list is an error the page shows, never an empty list.
function orgAccounts(org) {
  if (!Array.isArray(org.accounts)) {
    throw new Error(
      `the organization ${org.id} has no account list: ${JSON.stringify(org.accounts)}`
    );
  }
  return org.accounts;
}

// The organizations merged into this one, as the core sends them.
function orgMerged(org) {
  if (!Array.isArray(org.merged)) {
    throw new Error(
      `the organization ${org.id} has no merged list: ${JSON.stringify(org.merged)}`
    );
  }
  return org.merged.map((merged) => {
    if (!isObject(merged)) {
      throw new Error(
        `the organization ${org.id} has a merged entry that is not an object: ${JSON.stringify(merged)}`
      );
    }
    for (const field of ["id", "name"]) {
      if (typeof merged[field] !== "string" || merged[field] === "") {
        throw new Error(
          `the organization ${org.id} has a merged entry without a ${field}: ${JSON.stringify(merged)}`
        );
      }
    }
    if (!Object.prototype.hasOwnProperty.call(MERGED_ORIGIN, merged.source)) {
      throw new Error(
        `the organization ${merged.id} has no origin: ${JSON.stringify(merged.source)}`
      );
    }
    return { id: merged.id, name: merged.name, source: merged.source };
  });
}

function tokenValue(what, value) {
  if (typeof value !== "number" || !Number.isFinite(value) || value < 0) {
    throw new Error(`the ${what} has no token count: ${JSON.stringify(value)}`);
  }
  return value;
}

function sessionValue(what, value) {
  if (!Number.isInteger(value) || value < 0) {
    throw new Error(`the ${what} has no session count: ${JSON.stringify(value)}`);
  }
  return value;
}

// A token cell in the one spelling (`showTokens`): below a million the
// `tokens-small` colour shows, as in the mock-up.
function tokenCell(what, value) {
  return showTokens(h("span", { class: "n" }), tokenValue(what, value));
}

function sessionCell(what, value) {
  return h("span", { class: "n", text: String(sessionValue(what, value)) });
}

function projectsCard() {
  const card = panel(
    "Projects",
    "a project is a repository; an organization is several projects · the same on all your hosts · tokens and sessions: all time",
    [],
    [
      h("span", {
        class: "mf",
        text: "Changes apply to the past too: the history stores the repository, not the name.",
      }),
    ]
  );
  const body = card.querySelector(".settings-body");
  const projects = isObject(state.projects) ? state.projects : null;
  if (!projects) {
    body.appendChild(message(state.projectsError || "The projects could not be read."));
    return card;
  }
  if (!Array.isArray(projects.projects) || !Array.isArray(projects.orgs)) {
    throw new Error(
      "the projects view has no project or organization list: " +
        JSON.stringify({ projects: projects.projects, orgs: projects.orgs })
    );
  }
  body.appendChild(
    h("div", { class: "lr hd proj-grid" }, [
      h("span", { text: "Project" }),
      h("span", { text: "Repository" }),
      h("span", { text: "Folders, per host" }),
      h("span", { class: "n", text: "Tokens" }),
      h("span", { class: "n", text: "Sessions" }),
      h("span"),
    ])
  );
  if (projects.projects.length === 0 && projects.orgs.length === 0) {
    body.appendChild(
      h("div", { class: "lr mf", text: "No sessions recorded yet, so no project is known." })
    );
  }

  // One heading per organization that is not merged into another; a project
  // whose organization is merged in is drawn under its target's heading.
  const index = organizationIndex(projects);
  const groups = new Map(projects.orgs.map((org) => [org.id, []]));
  const unassigned = [];
  for (const project of projects.projects) {
    const heading = projectHeadingId(project, index);
    if (heading === null) unassigned.push(project);
    else groups.get(heading).push(project);
  }
  for (const org of projects.orgs) {
    body.appendChild(orgRow(org, projects));
    for (const project of groups.get(org.id)) {
      body.appendChild(projectRow(project, projects, org.id));
    }
  }
  body.appendChild(noOrganizationRow(unassigned));
  for (const project of unassigned) body.appendChild(projectRow(project, projects, null));

  body.appendChild(section("Join projects"));
  body.appendChild(joinRow(projects));

  body.appendChild(section("New organization"));
  body.appendChild(orgAddRow(projects));
  body.appendChild(
    note([
      h("b", { text: "Found by the repository." }),
      " The owner of a repository on GitHub becomes its organization, on every host. ",
      h("b", { text: "Move to" }),
      " moves one project. ",
      h("b", { text: "Merge into" }),
      " moves a whole organization, also the repositories of that owner you start later. Everything here is shared with your other hosts through the history folder, and applies to the past too.",
    ])
  );
  return card;
}

// The direct organizations and the merged ones they name, by lower-case
// identity: a project's organization is drawn under the heading it resolves to.
function organizationIndex(projects) {
  const direct = new Map();
  const merged = new Map();
  for (const org of projects.orgs) {
    direct.set(org.id.toLowerCase(), org.id);
    for (const entry of orgMerged(org)) {
      const key = entry.id.toLowerCase();
      if (merged.has(key)) {
        throw new Error(
          `the organization ${entry.id} is merged into more than one organization`
        );
      }
      merged.set(key, org.id);
    }
  }
  return { direct, merged };
}

function projectHeadingId(project, index) {
  if (project.org === null) return null;
  if (typeof project.org !== "string" || project.org === "") {
    throw new Error(
      `the project ${projectKeyText(project.key)} has no organization: ${JSON.stringify(project.org)}`
    );
  }
  const key = project.org.toLowerCase();
  const heading = index.direct.get(key) ?? index.merged.get(key);
  if (heading === undefined) {
    throw new Error(
      `the project ${projectKeyText(project.key)} names the organization ${project.org}, which is not in the view`
    );
  }
  return heading;
}

// The last heading: the projects no organization owns, with the sums the page
// computes from those projects' own figures.
function noOrganizationRow(projects) {
  let tokens = 0;
  let sessions = 0;
  for (const project of projects) {
    tokens += tokenValue(`project ${projectKeyText(project.key)}`, project.tokens);
    sessions += sessionValue(`project ${projectKeyText(project.key)}`, project.sessions);
  }
  return h("div", { class: "lr ogh proj-grid" }, [
    h("span", {}, [h("b", { text: "No organization" })]),
    h("span", { class: "mf ogl", text: "counted as Unassigned in Statistics" }),
    tokenCell("No organization", tokens),
    h("span", { class: "n", text: String(sessions) }),
    h("span"),
  ]);
}

function projectRow(project, projects, heading) {
  const field = `project:${projectKeyText(project.key)}`;
  const wrap = h("div", {
    class: "proj-wrap",
    dataset: { field, org: heading === null ? "" : heading },
  });

  const nameInput = textInput({
    class: "inp tall proj-name",
    value: project.name || "",
    "aria-label": "Project name",
  });
  nameInput.addEventListener("change", async () => {
    const next = nameInput.value.trim();
    if (next === project.name) {
      nameInput.value = project.name || "";
      return;
    }
    const ok = await saveProject({ edit: "rename", key: project.key, name: next }, field);
    if (!ok) nameInput.value = project.name || "";
  });

  const repo = project.repo
    ? h("span", { class: "mono x11", title: project.repo, text: project.repo })
    : h("span", { class: "mono x11 mf", text: "no repository" });
  const folders = foldersText(project.folders);
  wrap.appendChild(
    h("div", { class: "lr tall proj-grid" }, [
      nameInput,
      repo,
      h("span", { class: "mono x11 mf", title: folders, text: folders }),
      tokenCell(`project ${projectKeyText(project.key)}`, project.tokens),
      sessionCell(`project ${projectKeyText(project.key)}`, project.sessions),
      projectMoveCell(project, projects, heading, field),
    ])
  );
  if (Array.isArray(project.joined) && project.joined.length > 0) {
    wrap.appendChild(joinedLine(project, field));
  }
  paintRowError(wrap, field);
  return wrap;
}

// "Move to…" replaces the organization dropdown: the other organizations
// (the merged ones are not headings), then "No organization". A choice sends
// the `assign` edit at once; the menu goes back to its placeholder.
function projectMoveCell(project, projects, heading, field) {
  const NO_ORGANIZATION = "\u0000no-organization";
  const options = [["", "Move to…"]];
  for (const org of projects.orgs) {
    if (heading !== null && organizationNamesMatch(org.id, heading)) continue;
    options.push([org.id, org.name]);
  }
  options.push([NO_ORGANIZATION, "No organization"]);
  const select = selectInput(options, "", async (next) => {
    if (next === "") return;
    select.value = "";
    const ok = await saveProject(
      { edit: "assign", key: project.key, org: next === NO_ORGANIZATION ? null : next },
      field
    );
    if (!ok) select.value = "";
  });
  select.options[0].disabled = true;
  select.setAttribute(
    "aria-label",
    `Move ${project.name || "the project"} to another organization`
  );
  return selectWidth(select, 136);
}

// A joined project has no row of its own: the core sends its key and the
// default name of that key, shown under the project it was joined into, with
// an undo that writes `Unjoin`.
function joinedLine(project, field) {
  const line = h("div", { class: "proj-joined" }, [h("span", { class: "mf", text: "joined: " })]);
  project.joined.forEach((joined, index) => {
    if (index > 0) line.appendChild(middotSeparator());
    line.appendChild(
      h("span", { class: "mono x11", text: projectLabel(joined.name, joined.key) })
    );
    const undo = h("button", {
      type: "button",
      class: "hbtn xs ghost",
      text: "undo",
      "aria-label": `Unjoin ${projectLabel(joined.name, joined.key)}`,
    });
    undo.addEventListener("click", async () => {
      undo.disabled = true;
      const ok = await saveProject({ edit: "unjoin", key: joined.key }, field);
      if (!ok) undo.disabled = false;
    });
    line.appendChild(document.createTextNode(" "));
    line.appendChild(undo);
  });
  return line;
}

function joinRow(projects) {
  const field = "project:join";
  const rows = projects.projects;
  const options = rows.map((project, index) => [
    String(index),
    projectLabel(project.name, project.key),
  ]);
  const first = selectInput(options, "0", () => {});
  const second = selectInput(options, rows.length > 1 ? "1" : "0", () => {});
  first.setAttribute("aria-label", "Join this project");
  second.setAttribute("aria-label", "Into this project");
  selectWidth(first);
  selectWidth(second);
  const join = h("button", { type: "button", class: "hbtn", text: "Join projects" });
  join.disabled = rows.length < 2;
  join.addEventListener("click", async () => {
    const from = rows[Number(first.value)];
    const into = rows[Number(second.value)];
    if (!from || !into) return;
    join.disabled = true;
    const ok = await saveProject({ edit: "join", key: from.key, into: into.key }, field);
    if (!ok) join.disabled = false;
  });
  const control = h("span", { class: "proj-join" }, [
    h("span", { class: "mf", text: "join" }),
    first,
    h("span", { class: "mf", text: "into" }),
    second,
    join,
  ]);
  return setting(
    "Join projects",
    control,
    rows.length < 2
      ? "there must be two projects"
      : "the first project's sessions count for the second",
    field
  );
}

function accountChip(ref, projects) {
  const entry = (projects.accountRefs || []).find((item) => sameRef(item.account, ref));
  const row = entry ? accountRows().find((account) => account.id === entry.id) : null;
  const abbr = row && row.abbr ? row.abbr : "?";
  const color = row && isColor(row.color) ? row.color : "#555b66";
  return h("span", {
    class: "acc-chip org-chip",
    style: { background: color, color: contrastText(color) },
    title: row ? `${row.label || abbr}${row.email ? ` · ${row.email}` : ""}` : ref.acc,
    text: abbr,
  });
}

// The repositories whose owner is this organization, as core sends them. A
// malformed entry is an error the page shows, never a repository quietly left
// out.
function orgOwnerRepositories(org) {
  if (!Array.isArray(org.ownerRepositories)) {
    throw new Error(
      `the organization ${org.id} has no repository list: ${JSON.stringify(org.ownerRepositories)}`
    );
  }
  return org.ownerRepositories.map((repo) => {
    if (typeof repo !== "string" || repo === "") {
      throw new Error(
        `the organization ${org.id} has a repository entry that is not a name: ${JSON.stringify(repo)}`
      );
    }
    return repo;
  });
}

// One organization's heading: its name field, its accounts and origin, the
// organizations merged into it with a split-off link each, its sums and its
// "Merge into…" menu. The merged organizations are not headings of their own.
function orgRow(org, projects) {
  const field = `org:${org.id}`;
  const owners = orgOwnerRepositories(org);
  const accounts = orgAccounts(org);
  const merged = orgMerged(org);
  const empty = orgEmpty(org);
  const source = orgOrigin(org);
  const orgTokens = tokenValue(`organization ${org.id}`, org.tokens);
  const orgSessions = sessionValue(`organization ${org.id}`, org.sessions);
  const wrap = h("div", {
    class: `org-wrap${empty ? " empty" : ""}`,
    dataset: { field, org: org.id },
  });
  const nameInput = textInput({
    class: "inp tall org-name",
    value: org.name || "",
    "aria-label": "Organization name",
  });
  nameInput.title = `The shown name; the projects stay linked to ${org.id}.`;
  nameInput.addEventListener("change", async () => {
    const next = nameInput.value.trim();
    if (next === org.name) {
      nameInput.value = org.name || "";
      return;
    }
    const ok = await saveProject({ edit: "orgRename", org: org.id, name: next }, field);
    if (!ok) nameInput.value = org.name || "";
  });

  const line = h("span", { class: "row ogl" });
  if (accounts.length > 0) {
    line.appendChild(
      h("span", { class: "org-chips" }, accounts.map((ref) => accountChip(ref, projects)))
    );
  }
  line.appendChild(h("span", { class: "mf org-source", text: ORG_ORIGIN[source] }));
  if (empty) {
    line.appendChild(
      h("span", { class: "mf org-empty" }, [
        " · ",
        h("b", { text: "empty" }),
        ": its projects were moved; not shown in Statistics",
      ])
    );
  }
  if (merged.length > 0) {
    const mergedLine = h("span", { class: "mf org-merged" }, [
      " · ",
      h("b", { text: "merged in:" }),
      " ",
    ]);
    merged.forEach((entry, index) => {
      if (index > 0) mergedLine.appendChild(document.createTextNode(", "));
      mergedLine.appendChild(h("span", { text: entry.name }));
      mergedLine.appendChild(
        h("span", { class: "mf", text: ` (${MERGED_ORIGIN[entry.source]})` })
      );
      const split = h("button", {
        type: "button",
        class: "lnk",
        text: "split off…",
        "aria-label": `Split off ${entry.name}`,
      });
      split.addEventListener("click", () => {
        state.confirmRemoveOrgId = null;
        state.confirmOrgEdit = {
          kind: "orgUnjoin",
          org: entry.id,
          orgName: entry.name,
        };
        renderGroup(state.right, "projects");
      });
      mergedLine.appendChild(document.createTextNode(" "));
      mergedLine.appendChild(split);
    });
    line.appendChild(mergedLine);
  }
  if (owners.length === 0) {
    // The rule of decision 30: only an organization that is not a repository
    // owner has a Remove button; an owner would come back with its repository.
    const remove = h("button", {
      type: "button",
      class: "hbtn xs danger org-remove",
      text: "Remove",
      "aria-label": `Remove organization ${org.name}`,
    });
    remove.addEventListener("click", () => {
      state.confirmOrgEdit = null;
      state.confirmRemoveOrgId = org.id;
      renderGroup(state.right, "projects");
    });
    line.appendChild(document.createTextNode(" · "));
    line.appendChild(remove);
  }

  const options = [["", "Merge into…"]];
  for (const other of projects.orgs) {
    if (organizationNamesMatch(other.id, org.id)) continue;
    options.push([other.id, other.name]);
  }
  const merge = selectInput(options, "", (next) => {
    if (next === "") return;
    const into = projects.orgs.find((other) => other.id === next);
    if (!into) throw new Error(`unknown organization ${next}`);
    merge.value = "";
    state.confirmRemoveOrgId = null;
    state.confirmOrgEdit = {
      kind: "orgJoin",
      org: org.id,
      orgName: org.name,
      into: into.id,
      intoName: into.name,
    };
    renderGroup(state.right, "projects");
  });
  merge.options[0].disabled = true;
  merge.setAttribute("aria-label", `Merge ${org.name} into another organization`);
  selectWidth(merge, 136);

  wrap.appendChild(
    h("div", { class: `lr ogh proj-grid${empty ? " empty" : ""}` }, [
      nameInput,
      line,
      empty ? h("span") : showTokens(h("span", { class: "n" }), orgTokens),
      empty ? h("span") : h("span", { class: "n", text: String(orgSessions) }),
      merge,
    ])
  );
  const confirm = state.confirmOrgEdit;
  if (isObject(confirm) && confirm.kind === "orgJoin" && organizationNamesMatch(confirm.org, org.id)) {
    wrap.appendChild(orgMergeConfirm(confirm, field));
  }
  if (
    isObject(confirm) &&
    confirm.kind === "orgUnjoin" &&
    merged.some((entry) => organizationNamesMatch(entry.id, confirm.org))
  ) {
    wrap.appendChild(orgSplitConfirm(confirm, field));
  }
  if (state.confirmRemoveOrgId === org.id) wrap.appendChild(orgRemoveConfirm(org, field));
  paintRowError(wrap, field);
  return wrap;
}

// A merge waits for its confirmation under the source heading. The text says
// what the merge does to later repositories; nothing is sent before Merge.
function orgMergeConfirm(confirm, field) {
  return h("div", { class: "confirm org-confirm org-head-confirm" }, [
    h("div", {
      class: "confirm-text",
      text: `Merge ${confirm.orgName} into ${confirm.intoName}? Its projects count for ${confirm.intoName}, also repositories of its owner started later.`,
    }),
    h("div", { class: "confirm-actions" }, [
      h("button", {
        type: "button",
        class: "hbtn xs",
        text: "Merge",
        onclick: async () => {
          const ok = await saveProject(
            { edit: "orgJoin", org: confirm.org, into: confirm.into },
            field
          );
          if (ok) {
            state.confirmOrgEdit = null;
            renderGroup(state.right, "projects");
          }
        },
      }),
      h("button", {
        type: "button",
        class: "hbtn xs ghost",
        text: "Cancel",
        onclick: () => {
          state.confirmOrgEdit = null;
          renderGroup(state.right, "projects");
        },
      }),
    ]),
  ]);
}

// A split-off waits for its confirmation under the target heading, where the
// merged organization is named.
function orgSplitConfirm(confirm, field) {
  return h("div", { class: "confirm org-confirm org-head-confirm" }, [
    h("div", {
      class: "confirm-text",
      text: `Split off ${confirm.orgName}? It gets its projects back; projects moved by hand since stay where they are.`,
    }),
    h("div", { class: "confirm-actions" }, [
      h("button", {
        type: "button",
        class: "hbtn xs",
        text: "Split off",
        onclick: async () => {
          const ok = await saveProject({ edit: "orgUnjoin", org: confirm.org }, field);
          if (ok) {
            state.confirmOrgEdit = null;
            renderGroup(state.right, "projects");
          }
        },
      }),
      h("button", {
        type: "button",
        class: "hbtn xs ghost",
        text: "Cancel",
        onclick: () => {
          state.confirmOrgEdit = null;
          renderGroup(state.right, "projects");
        },
      }),
    ]),
  ]);
}

// The confirmation first says what happens: the organization's projects and
// accounts go back to none (decision 30). A malformed view is an error the page
// shows, never a made-up "0 projects".
function orgRemoveConfirm(org, field) {
  if (!Number.isFinite(org.projects)) {
    throw new Error(
      `the organization ${org.id} has no project count: ${JSON.stringify(org.projects)}`
    );
  }
  if (!Array.isArray(org.accounts)) {
    throw new Error(
      `the organization ${org.id} has no account list: ${JSON.stringify(org.accounts)}`
    );
  }
  const projectCount = org.projects;
  const accountCount = org.accounts.length;
  return h("div", { class: "confirm org-confirm" }, [
    h("div", {
      class: "confirm-text",
      text: `Remove ${org.name}? Its ${countText(projectCount, "project", "projects")} and ${countText(accountCount, "account", "accounts")} go back to none.`,
    }),
    h("div", { class: "confirm-actions" }, [
      h("button", {
        type: "button",
        class: "hbtn xs danger",
        text: "Remove",
        onclick: async () => {
          const ok = await saveProject(
            { edit: "orgRemove", org: org.id },
            field
          );
          if (ok) {
            state.confirmRemoveOrgId = null;
            renderGroup(state.right, "projects");
          }
        },
      }),
      h("button", {
        type: "button",
        class: "hbtn xs ghost",
        text: "Keep it",
        onclick: () => {
          state.confirmRemoveOrgId = null;
          renderGroup(state.right, "projects");
        },
      }),
    ]),
  ]);
}

// The same rule as core::projects::same_organization_name: compare the names
// after `toLowerCase()`, Unicode case included.
function organizationNamesMatch(first, second) {
  return first.toLowerCase() === second.toLowerCase();
}

// The "+ Add organization" line under the list: a name and one button, so a
// new organization starts empty. A name that exists is refused here, before
// an edit is sent (decision 30).
function orgAddRow(projects) {
  const field = "org:add";
  const input = textInput({
    class: "inp tall org-add-name",
    placeholder: "organization name",
    "aria-label": "New organization name",
  });
  const add = h("button", {
    type: "button",
    class: "hbtn xs",
    text: "Add organization",
  });
  const submit = async () => {
    // core::projects refuses a name with outer white space; the page trims
    // what was typed before it sends, so that refusal is never seen here.
    const name = input.value.trim();
    if (name === "") {
      fieldError(field, "An organization name must not be empty.");
      input.focus();
      return;
    }
    const exists =
      projects.orgs.some(
        (org) =>
          organizationNamesMatch(org.id, name) ||
          organizationNamesMatch(org.name, name)
      ) ||
      projects.orgs.some((org) =>
        orgMerged(org).some(
          (merged) =>
            organizationNamesMatch(merged.id, name) ||
            organizationNamesMatch(merged.name, name)
        )
      );
    if (exists) {
      fieldError(field, `an organization named ${name} already exists`);
      input.focus();
      return;
    }
    add.disabled = true;
    const ok = await saveProject({ edit: "orgAdd", name }, field);
    add.disabled = false;
    if (!ok) {
      input.focus();
      return;
    }
    input.value = "";
  };
  add.addEventListener("click", submit);
  input.addEventListener("keydown", (event) => {
    if (event.key === "Enter") {
      event.preventDefault();
      submit();
    }
  });
  return setting(
    "+ Add organization",
    h("span", { class: "org-add" }, [input, add]),
    "a new organization starts empty",
    field
  );
}

// The mock-up shows the folder in effect as the empty field's placeholder. The
// snapshot fills it from the ClaudePaths the app runs with; when the snapshot
// has no folder, the field says so instead of inventing one.
function configFolderPlaceholder() {
  const folder =
    state.snapshot && typeof state.snapshot.claudeConfigDir === "string"
      ? state.snapshot.claudeConfigDir
      : "";
  return folder !== "" ? folder : "the config folder in effect is not known";
}

function generalCard() {
  const card = panel("General", "", [], [
    h("span", { class: "mf" }, [
      "To open the pop-up with a key, bind ",
      h("code", { text: "seatswap panel" }),
      " to a key in your desktop's keyboard settings.",
    ]),
  ]);
  const body = card.querySelector(".settings-body");

  // The card's first line, above the Money section (R7).
  body.appendChild(versionLine());

  // Money is the first section, as in the mock-up. Its rows are built (and
  // their times filled) only when the money view changed (decision 25).
  body.appendChild(moneyRows());

  body.appendChild(section("Look"));
  body.appendChild(setting("Theme", themeControl(savedValue("theme") || "system"), "", "theme"));
  body.appendChild(
    setting(
      "Tray text",
      selectSetting(
        "trayText",
        [
          ["percent", "Percent"],
          ["percentReset", "Percent and reset"],
          ["off", "Off"],
        ],
        "Tray text"
      ),
      "",
      "trayText"
    )
  );
  body.appendChild(
    setting(
      "The tray ring shows",
      selectSetting(
        "iconLimit",
        [
          ["binding", "The binding limit"],
          ["session", "The 5h session"],
          ["week", "The week"],
        ],
        "The tray ring shows"
      ),
      "",
      "iconLimit"
    )
  );
  body.appendChild(
    setting(
      "The tray icon shows",
      selectSetting(
        "iconText",
        [
          ["letters", "The account's letters"],
          ["percent", "The ring's percent (100 as 99)"],
        ],
        "The tray icon shows"
      ),
      "",
      "iconText"
    )
  );
  body.appendChild(section("Start and polling"));
  body.appendChild(
    setting("Start at login", toggle("startAtLogin", "Start at login"), "", "startAtLogin")
  );
  body.appendChild(
    setting(
      "Read the active account every",
      numberField("pollActiveSecs", "Read the active account every", {
        step: 30,
        unit: "s",
      }),
      "every 30s from 90 %",
      "pollActiveSecs"
    )
  );
  body.appendChild(
    setting(
      "Read inactive accounts every",
      numberField("pollInactiveSecs", "Read inactive accounts every", {
        step: 30,
        unit: "s",
      }),
      "",
      "pollInactiveSecs"
    )
  );
  body.appendChild(section("Claude Code"));
  body.appendChild(
    textField("claudeConfigDir", "Claude Code's config folder", {
      placeholder: configFolderPlaceholder(),
      note: "empty: $CLAUDE_CONFIG_DIR or Claude Code's default folder · takes effect after restarting seatswap",
    })
  );
  body.appendChild(
    setting(
      "Read Claude Code's session files",
      toggle("readSessions", "Read Claude Code's session files"),
      "model, busy or idle, tokens per folder · never message text · takes effect after restarting seatswap",
      "readSessions"
    )
  );
  body.appendChild(
    setting(
      "Copy the live login at once",
      toggle("mirrorLive", "Copy the live login at once"),
      "",
      "mirrorLive"
    )
  );
  body.appendChild(
    setting(
      "Refresh inactive logins",
      toggle("refreshInactive", "Refresh inactive logins"),
      "needed to read their usage",
      "refreshInactive"
    )
  );

  return card;
}

// ---------------------------------------------------------------------------
// Auto-burn
// ---------------------------------------------------------------------------

function burn() {
  return isObject(state.snapshot.burn) ? state.snapshot.burn : null;
}

// The window share is the range over the accounts auto-burn may use; one value
// when they agree.
function measuredText(measured) {
  if (!isObject(measured)) return MISSING;
  const values = Object.values(measured.windowPercentOfWeek || {}).filter((value) =>
    Number.isFinite(value)
  );
  const parts = [];
  if (values.length === 0) {
    parts.push(`a full 5h window is ${MISSING}`);
  } else {
    const low = Math.min(...values);
    const high = Math.max(...values);
    const share =
      low === high ? shortPercent(low) : `${shortPercent(low)}–${shortPercent(high)}`;
    parts.push(`a full 5h window is about ${share} % of a week`);
  }
  const cost = measured.switchCostPercentOfWindow;
  if (Number.isFinite(cost)) {
    parts.push(`a switch costs about ${shortPercent(cost)} % of a window`);
  } else {
    parts.push(`a switch cost is ${MISSING}`);
  }
  return parts.join(" · ");
}

function switchesNote(view) {
  if (!view) return "";
  if (!view.early) return "no other early switches";
  const value = view.measured ? view.measured.switchesPerDay : null;
  if (!Number.isFinite(value)) return MISSING;
  return `about ${shortPercent(value)} switches a day`;
}

function autoBurnCard() {
  const view = burn();
  return panel(
    "Auto-burn",
    "switches accounts so that you are blocked as little as possible",
    [
      section("Auto-burn"),
      setting(
        "Auto-burn",
        toggle("autoBurn", "Auto-burn"),
        "switches by itself, as well as it can",
        "autoBurn"
      ),
      setting(
        "Suggest switches",
        toggle("suggestSwitches", "Suggest switches"),
        "while auto-burn is off: what it would do now, and why",
        "suggestSwitches"
      ),
      section("What it decides by itself"),
      setting(
        "How early it switches",
        h("span", { text: "from what it has measured" }),
        "the size of your 5h windows and what a switch costs"
      ),
      setting("Measured so far", h("span", { text: measuredText(view && view.measured) })),
      setting(
        "So right now",
        h("span", {
          class: view && view.early ? "ok" : "",
          text: view && typeof view.why === "string" ? view.why : MISSING,
        }),
        switchesNote(view)
      ),
      setting(
        "While a session would lose its thinking",
        h("span", { text: "a planned switch waits" }),
        "never longer than the wanted account can spare"
      ),
      note([
        h("b", { text: "There is nothing to tune." }),
        " A session on Fable 5.2, Mythos 5.2, Sonnet 5.5 or Haiku 5.5 cannot read its earlier thinking on another account, so a planned switch waits while such a session is busy. Sessions on Opus 5.5 keep their thinking. For long runs that should survive switches, Opus 5.5 is the better choice.",
      ]),
    ]
  );
}

// ---------------------------------------------------------------------------
// Notifications
// ---------------------------------------------------------------------------

function notificationsCard() {
  // The mock-up draws "All accounts are full" as a switch, but the settings
  // description says it is always on: there is no setting to save. The switch
  // shows that state and says so, instead of offering a click that would do
  // nothing.
  const allFull = switchControl(true, "All accounts are full", () => {});
  allFull.button.disabled = true;
  allFull.button.title = "always on: it is sent whenever every account is full";
  return panel(
    "Notifications",
    "",
    [
      section("Sessions"),
      setting(
        "A session is waiting for you",
        toggle("notifyWaiting", "A session is waiting for you"),
        "when it has waited 15 seconds · stays until you click it · a click opens Sessions",
        "notifyWaiting"
      ),
      setting(
        "For sessions on",
        selectSetting(
          "notifyWaitingHosts",
          [
            ["thisHost", "this host"],
            ["allHosts", "all hosts"],
          ],
          "For sessions on",
          130
        ),
        "other hosts report with the delay of the sync",
        "notifyWaitingHosts"
      ),
      section("Accounts"),
      setting(
        "An account is usable again",
        toggle("notifyUsableAgain", "An account is usable again"),
        "",
        "notifyUsableAgain"
      ),
      setting(
        "All accounts are full",
        allFull.node,
        "with the time the next one is free"
      ),
      setting(
        "Every automatic switch",
        toggle("notifyOnSwitch", "Every automatic switch"),
        "",
        "notifyOnSwitch"
      ),
      setting("A switch by an agent", h("span", { class: "mf", text: "always" })),
      setting(
        "Suggested switches",
        h(
          "button",
          {
            type: "button",
            class: "link",
            onclick: () => {
              const right = state.container.querySelector(".settings-col");
              if (right) selectGroup("auto-burn", right);
            },
          },
          "see Auto-burn"
        )
      ),
      setting(
        "A login ends",
        toggle("notifyLoginEnds", "A login ends"),
        "5 days and 1 day before",
        "notifyLoginEnds"
      ),
    ]
  );
}

// ---------------------------------------------------------------------------
// Agents
// ---------------------------------------------------------------------------

function agentsCard() {
  const skillControl = h("span", { class: "row" });
  const skillNote = h("span", { class: "mf" });
  if (isObject(state.skill)) {
    showSkill(skillControl, skillNote, state.skill);
  } else if (state.skillError) {
    skillControl.appendChild(
      h("span", { class: "sw bad", text: "could not read" })
    );
    skillNote.textContent = state.skillError;
  } else {
    skillControl.appendChild(h("span", { class: "mf", text: "loading…" }));
  }
  const skillRow = setting("Agent skill", skillControl, skillNote, "skill");
  const callRows = h("div", { class: "col calls" });
  const calls = h("div", { class: "col" }, [
    h("div", {
      class: "lr hd",
      style: { gridTemplateColumns: "96px minmax(0, 1fr) 170px 110px" },
    }, [
      h("span", { text: "When" }),
      h("span", { text: "Command" }),
      h("span", { text: "From" }),
      h("span", { text: "Result" }),
    ]),
    callRows,
  ]);

  const card = panel(
    "Agents",
    "what agents may do with seatswap",
    [
      section("What agents may do"),
      setting(
        "Agents may read usage",
        toggle("agentsRead", "Agents may read usage"),
        "status, list",
        "agentsRead"
      ),
      setting(
        "Agents may switch accounts",
        toggle("agentsSwitch", "Agents may switch accounts"),
        "use, next · every switch is notified",
        "agentsSwitch"
      ),
      note([
        h("b", { text: "These switches are not a lock." }),
        " They work through a marker that Claude Code puts on every command it runs. An agent runs as you: it can remove the marker, or change seatswap's files directly. The switches stop agents that follow the rules. To block the command itself, add a deny rule for ",
        h("span", { class: "mono", text: "seatswap" }),
        " to Claude Code's permissions.",
      ]),
      section("Telling agents about seatswap"),
      skillRow,
      section("Last calls by agents"),
      calls,
    ],
    h("span", { class: "mf" }, [
      "A call counts as an agent's when ",
      h("span", { class: "mono", text: "CLAUDECODE=1" }),
      " or ",
      h("span", { class: "mono", text: "AI_AGENT" }),
      " is set.",
    ])
  );

  // Draw the two rows that need the app from the stored views.
  renderCalls(callRows);
  return card;
}

function skillButton(label, force, control, noteNode) {
  const button = h("button", {
    type: "button",
    class: "hbtn xs pri",
    onclick: async () => {
      button.disabled = true;
      try {
        const status = await state.context.invoke("skill_install", { force });
        fieldError("skill", null);
        state.skill = status;
        state.skillError = null;
        showSkill(control, noteNode, status);
      } catch (error) {
        fieldError("skill", errorMessage(error));
      }
      button.disabled = false;
    },
  }, label);
  return button;
}

function showSkill(control, noteNode, status) {
  clear(control);
  if (!status.installed) {
    control.appendChild(skillButton("Install", false, control, noteNode));
    noteNode.textContent = `not installed · goes to ${status.path}`;
    return;
  }
  control.appendChild(
    h("span", { class: status.differs ? "sw warn" : "sw ok", text: status.differs ? "Differs" : "Installed" })
  );
  if (status.differs) {
    control.appendChild(skillButton("Replace", true, control, noteNode));
    noteNode.textContent = `differs from the bundled skill · ${status.path}`;
  } else {
    noteNode.textContent = status.path;
  }
}

async function loadSkillView() {
  if (!state.context) return;
  const token = ++state.skillToken;
  try {
    const status = await state.context.invoke("skill_status", {});
    if (token !== state.skillToken) return;
    if (!isObject(status)) throw new Error("skill_status returned an unexpected reply");
    state.skill = status;
    state.skillError = null;
  } catch (error) {
    if (token !== state.skillToken) return;
    state.skill = null;
    state.skillError = errorMessage(error);
  }
  if (token === state.skillToken) state.skillLoaded = true;
}

async function loadCallsView() {
  if (!state.context) return;
  const token = ++state.callsToken;
  try {
    const rows = await state.context.invoke("agent_calls", { limit: 20 });
    if (token !== state.callsToken) return;
    if (!Array.isArray(rows)) throw new Error("agent_calls returned an unexpected reply");
    // The clock text is part of the stored row, so a redraw needs no command.
    let whens = rows.map(() => "?");
    try {
      whens = await state.context.invoke("time_texts", {
        times: rows.map((row) => ({ at: row.at, style: "dayClock" })),
      });
      if (!Array.isArray(whens) || whens.length !== rows.length) {
        throw new Error("time_texts returned an unexpected reply");
      }
    } catch (error) {
      console.error(`agent call times: ${errorMessage(error)}`);
      whens = rows.map(() => "?");
    }
    if (token !== state.callsToken) return;
    state.calls = rows.map((row, index) => Object.assign({}, row, { when: whens[index] }));
    state.callsError = null;
  } catch (error) {
    if (token !== state.callsToken) return;
    state.calls = null;
    state.callsError = errorMessage(error);
  }
  if (token === state.callsToken) state.callsLoaded = true;
}

function renderCalls(container) {
  clear(container);
  if (state.callsError) {
    container.appendChild(message(state.callsError));
    return;
  }
  const rows = Array.isArray(state.calls) ? state.calls : null;
  if (rows === null) {
    container.appendChild(h("div", { class: "lr mf", text: "loading…" }));
    return;
  }
  if (rows.length === 0) {
    container.appendChild(h("div", { class: "lr" }, [h("span", { text: "No calls yet." })]));
    return;
  }
  for (const row of rows) {
    container.appendChild(
      h(
        "div",
        {
          class: "lr",
          style: { gridTemplateColumns: "96px minmax(0, 1fr) 170px 110px" },
        },
        [
          h("span", { class: "mf", text: row.when || "?" }),
          h("span", { class: "mono x11", text: `seatswap ${row.command}` }),
          h("span", { class: "mf", text: row.cwd || "—" }),
          h("span", {}, [
            h("span", {
              class: row.allowed ? "sw ok" : "sw bad",
              text: row.allowed ? "answered" : "refused",
            }),
          ]),
        ]
      )
    );
  }
}

// ---------------------------------------------------------------------------
// Providers (spec §7, step D task 5)
// ---------------------------------------------------------------------------

function providersCard() {
  return panel(
    "Providers",
    "read-only cards for OpenAI (Codex), Ollama's cloud plan and the API prices",
    [
      section("OpenAI (Codex)"),
      setting(
        "Show an OpenAI (Codex) card",
        toggle("codexCard", "Show an OpenAI (Codex) card"),
        "read only · every 5 minutes",
        "codexCard"
      ),
      note(
        "seatswap reads Codex's login file and asks chatgpt.com for your usage every 5 minutes. It never changes Codex's files."
      ),
      textField("codexHome", "Codex folder", {
        note: "empty: $CODEX_HOME or ~/.codex",
      }),
      section("Ollama"),
      setting(
        "Show an Ollama card",
        toggle("ollamaCard", "Show an Ollama card"),
        "asks the Ollama server on this machine for your cloud balance every 5 minutes",
        "ollamaCard"
      ),
      textField("ollamaUrl", "Ollama server"),
      // The model list moved here from General (decision 7): read-only.
      section("API prices"),
      priceBlock(),
    ]
  );
}

// ---------------------------------------------------------------------------
// History and hosts (spec §1.5, §3.7; step C1 task 7)
// ---------------------------------------------------------------------------

// The mock-up's line: a host whose last record is older than this is amber.
const HOST_STALE_MS = 15 * 60000;

function historyCard() {
  const view = isObject(state.history) ? state.history : null;
  const card = panel("History and hosts", "what seatswap has recorded, and where", []);
  const body = card.querySelector(".settings-body");

  body.appendChild(section("This host"));
  body.appendChild(
    textField("hostName", "Name of this host", {
      placeholder: view && typeof view.host === "string" ? view.host : "",
      note: "must be different on every host",
      restart: Boolean(view && typeof view.savedHost === "string"),
    })
  );
  body.appendChild(
    textField("historyDir", "History folder", {
      placeholder: view && typeof view.dir === "string" ? view.dir : "",
      note: "a synced folder shares it with your other hosts · takes effect after restarting seatswap, which brings this host's history along",
      restart: Boolean(view && typeof view.savedDir === "string"),
      width: 280,
    })
  );
  const suggestion = dropboxSuggestion(view);
  if (suggestion) body.appendChild(suggestion);

  body.appendChild(section("Hosts found in the folder"));
  const hosts = h("div", { class: "col hist-hosts" });
  body.appendChild(hosts);
  renderHosts(
    hosts,
    view,
    [state.historyError, dropboxSuggestionError(view)].filter(Boolean).join(" · ") || null
  );

  body.appendChild(
    note([
      h("b", { text: "Each host writes only its own subfolder," }),
      " so two hosts can never overwrite each other. ",
      h("b", { text: "Logins are never put here:" }),
      " a login copied to another host would log this one out.",
    ])
  );
  body.appendChild(
    note([
      h("b", { text: "Shared with your other hosts through this folder:" }),
      " each account's label, abbreviation, colour, price and whether auto-burn may use it," +
        " and the currency, the notifications and the status line. The last edit wins." +
        " An account appears on a host only after you log in to it there; it then takes" +
        " these from the other hosts. Logins are never shared.",
    ])
  );

  body.appendChild(section("Export and import"));
  body.appendChild(exportRow());
  body.appendChild(importRow());
  return card;
}

// Decision 3: Dropbox's own info.json named a folder and no history folder is
// set, so the view suggests `<Dropbox>/seatswap`. It is only a suggestion:
// nothing is created until Use is pressed, and Use saves through the same
// `save_setting` command the field uses.
function dropboxSuggestion(view) {
  if (!isObject(view) || view.suggestedFolder === null) return null;
  const folder = view.suggestedFolder;
  if (typeof folder !== "string" || folder === "") {
    throw new Error(
      `the history view's suggestedFolder is not a folder: ${JSON.stringify(folder)}`
    );
  }
  const use = h("button", { type: "button", class: "hbtn xs", text: "Use" });
  use.setAttribute("aria-label", `Use ${folder} as the history folder`);
  use.addEventListener("click", async () => {
    use.disabled = true;
    const ok = await saveField("historyDir", folder);
    use.disabled = false;
    if (!ok) return;
    // Read the view again: with a folder set the suggestion is gone.
    await loadHistory();
    if (state.group === "history" && state.right) renderGroup(state.right, "history");
  });
  return h("div", { class: "set" }, [
    h("span"),
    h("span", {}, [
      h("span", { class: "mf" }, [
        "Dropbox found: ",
        h("span", { class: "mono x11", text: folder }),
      ]),
      use,
    ]),
  ]);
}

// A broken Dropbox info.json is shown in the group's existing error place,
// beside the host folders; it is never silently dropped.
function dropboxSuggestionError(view) {
  if (!isObject(view) || view.suggestedError === null) return null;
  if (typeof view.suggestedError !== "string" || view.suggestedError === "") {
    throw new Error(
      `the history view's suggestedError is not a text: ${JSON.stringify(view.suggestedError)}`
    );
  }
  return view.suggestedError;
}

// Read the host folders when the group opens, after an import and after a
// saved host name or folder. A snapshot render in between uses the stored view.
async function loadHistory() {
  if (!state.context) return;
  const token = ++state.historyToken;
  try {
    const view = await state.context.invoke("history_view", {});
    if (token !== state.historyToken) return;
    if (!isObject(view)) throw new Error("history_view returned an unexpected reply");
    state.history = view;
    state.historyError = null;
  } catch (error) {
    if (token !== state.historyToken) return;
    state.historyError = errorMessage(error);
  }
  if (token === state.historyToken) state.historyLoaded = true;
}

// The hosts table: Host · Last record · Since · Size, 28 px rows as the mock-up.
function renderHosts(container, view, error) {
  clear(container);
  const rows = isObject(view) && Array.isArray(view.hosts) ? view.hosts : [];
  if (error && rows.length === 0) {
    container.appendChild(message(error));
    return;
  }
  container.appendChild(
    h("div", { class: "lr hd hist-grid" }, [
      h("span", { text: "Host" }),
      h("span", { text: "Last record" }),
      h("span", { text: "Since" }),
      h("span", { class: "hist-size", text: "Size" }),
    ])
  );
  const times = [];
  const now = Date.now();
  for (const info of rows) {
    let last = null;
    if (typeof info.lastRecord === "string" && info.lastRecord !== "") {
      const age = Date.parse(info.lastRecord);
      const stale = !Number.isFinite(age) || now - age > HOST_STALE_MS;
      const text = h("span");
      last = h("span", { class: stale ? "sw warn" : "sw ok" }, [text]);
      times.push({ node: text, style: "age", at: info.lastRecord });
    } else {
      last = h("span", { class: "mf", text: "no record yet" });
    }

    let since;
    if (typeof info.firstRecord === "string" && info.firstRecord !== "") {
      const date = h("span");
      times.push({
        node: date,
        style: "dayYear",
        at: info.firstRecord,
      });
      since = h("span", {}, [date]);
      if (info.thisHost) since.appendChild(document.createTextNode(" · this host"));
    } else {
      since = h("span", { class: "mf", text: "—" });
    }

    container.appendChild(
      h("div", { class: "lr hist-grid" }, [
        h("b", { text: info.host }),
        h("span", {}, [last]),
        since,
        h("span", { class: "n hist-size", text: bytesText(info.bytes) }),
      ])
    );
  }
  if (error) container.appendChild(message(error));
  fillTimeNodes(times);
}

// A folder's size as the mock-up shows it: "2.9 MB".
function bytesText(bytes) {
  const count = Number(bytes);
  if (!Number.isFinite(count) || count < 0) return "—";
  if (count >= 1024 * 1024) return `${(count / (1024 * 1024)).toFixed(1)} MB`;
  if (count >= 1024) return `${(count / 1024).toFixed(1)} kB`;
  return `${Math.round(count)} B`;
}

function exportRow() {
  const input = textInput({
    value: state.exportPath,
    placeholder: "~/seatswap-history.csv",
    "aria-label": "save to",
  });
  input.className = "inp";
  input.addEventListener("input", () => {
    state.exportPath = input.value;
  });
  const result = h("span", { class: "mf" });
  const written = (text) => {
    state.exportResult = text;
    result.textContent = text;
  };
  const run = (format) => async () => {
    const to = String(state.exportPath || "").trim();
    if (to === "") {
      written("");
      fieldError("historyExport", "enter a file path");
      return;
    }
    fieldError("historyExport", null);
    try {
      await state.context.invoke("history_export", {
        format,
        to,
        from: null,
        until: null,
      });
      written(`written: ${to}`);
    } catch (error) {
      written("");
      fieldError("historyExport", errorMessage(error));
    }
  };
  const buttons = h("span", { class: "hist-buttons" }, [
    h("button", { type: "button", class: "hbtn xs", onclick: run("csv") }, "CSV"),
    h("button", { type: "button", class: "hbtn xs", onclick: run("json") }, "JSON"),
  ]);
  written(state.exportResult);
  return setting(
    "Export",
    h("span", { class: "hist-export" }, [input, buttons]),
    result,
    "historyExport"
  );
}

function importRow() {
  const input = textInput({
    value: state.importFolder,
    placeholder: "~/Sync/seatswap-history",
    "aria-label": "import from",
  });
  input.className = "inp";
  input.addEventListener("input", () => {
    state.importFolder = input.value;
  });
  const noteLines = [];
  if (state.importResult !== "") noteLines.push(state.importResult);
  noteLines.push(...state.importConflicts);
  const noteChildren = [];
  for (const line of noteLines) {
    if (noteChildren.length > 0) noteChildren.push(h("br"));
    noteChildren.push(line);
  }
  const result = h("span", { class: "mf" }, noteChildren);
  const sub = h("span");
  const note = h("span", { class: "mf" }, [result, sub]);
  const paintSub = () => {
    sub.textContent = result.hasChildNodes()
      ? " · for hosts without a synced folder"
      : "for hosts without a synced folder";
  };
  paintSub();
  const run = async () => {
    const from = String(state.importFolder || "").trim();
    if (from === "") {
      state.importResult = "";
      state.importConflicts = [];
      result.textContent = "";
      paintSub();
      fieldError("historyImport", "enter a folder path");
      return;
    }
    fieldError("historyImport", null);
    try {
      const reply = await state.context.invoke("history_import", { from });
      if (
        !isObject(reply) ||
        !Array.isArray(reply.copied) ||
        !Array.isArray(reply.conflicts) ||
        !Array.isArray(reply.warnings) ||
        reply.warnings.some((warning) => typeof warning !== "string") ||
        !isObject(reply.view)
      ) {
        throw new Error("history_import returned an unexpected reply");
      }
      for (const conflict of reply.conflicts) {
        if (
          !isObject(conflict) ||
          typeof conflict.host !== "string" ||
          typeof conflict.reason !== "string"
        ) {
          throw new Error("history_import returned an unexpected reply");
        }
      }
      state.history = reply.view;
      state.importResult =
        reply.copied.length === 0 ? "nothing new" : `copied: ${reply.copied.join(", ")}`;
      state.importConflicts = [
        ...reply.conflicts.map((conflict) => `conflict ${conflict.host}: ${conflict.reason}`),
        ...reply.warnings,
      ];
    } catch (error) {
      state.importResult = "";
      state.importConflicts = [];
      result.textContent = "";
      paintSub();
      fieldError("historyImport", errorMessage(error));
      return;
    }
    // The table shows the folder as it is now; the field keeps what was typed.
    if (state.group === "history" && state.right) renderGroup(state.right, "history");
  };
  return setting(
    "Import another host's folder",
    h("span", { class: "hist-import" }, [
      input,
      h("button", { type: "button", class: "hbtn xs", onclick: run }, "Import"),
    ]),
    note,
    "historyImport"
  );
}

// Leaving Settings gives the window's area its own scrolling back; the group
// menu stayed fixed only while this page showed (item 27).
export function unmount() {
  if (state.container) state.container.classList.remove("settings-fixed");
  state.drawnGroup = null;
  // The next mount asks the version again, once (R7).
  state.version = null;
  state.versionError = null;
  state.versionLoaded = false;
}
