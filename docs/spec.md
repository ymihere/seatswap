---
name: seatswap — specification
status: Draft
created: 2026-11-10
updated: 2026-11-25
tags: [seatswap, spec, claude-code, tray, linux, macos, windows]
---

# seatswap — specification

A tray app that switches Claude Code between several Claude accounts (licences) with one click. All accounts share one `~/.claude`, so memory, skills, plugins, hooks, settings and history are the same whichever account is on. The tray icon shows which account is active and how much of its limit is used. A pop-up shows the usage of every stored account side by side.

## Provenance

Written 2026-11-10 on the Mint/Cinnamon ThinkPad with Claude Code 2.1.322.

**Checked on this machine:**
- Location and layout of the login files.
- The usage endpoint, called with the live login.
- The token endpoint and client ID, read from the Claude Code program.

**Read from the Claude Code program (second pass, same day):** how Claude Code itself refreshes, locks, re-reads and saves the login. These points are marked *(code)*. They describe version 2.1.322 and are read from the code.

**Tested in Phase 0 (same day, evening):** the points marked *(tested)*. The tests ran with two real accounts in a separate config folder, so the real login was never touched. Results, evidence and scripts are in `phase0/RESULTS.md`.

**Still assumed:** everything marked *(Phase 0)*.

The endpoints are undocumented and belong to Claude Code's own login. Anthropic can change them in any release. The design keeps them in one module so a change is a one-file fix.

---

## 1. Goals and non-goals

**Goals**
1. Switch the Claude Code account in one click from the tray, or with one command (`seatswap use B`).
2. Keep one shared `~/.claude`. Nothing besides the login changes on a switch.
3. Show the active account and its usage on the tray icon itself.
4. Show the usage of all stored accounts in a good-looking pop-up, including accounts that aren't active.
5. Give each account a short label of one or two characters, and a colour.
6. Run on Linux (as many desktops as possible), macOS and Windows.
7. Show when every account's login ends and warn in good time before it does. A login has a fixed end date that nothing can move (see §6.3).
8. Switch by itself, when auto-burn is switched on, so that as little work as possible is blocked, and say what it would do when it is off (see §7.4).

**Non-goals (v1)**
- Switching the **Claude Desktop** app. Its login is a Chromium cookie, stored encrypted in `~/.config/Claude` (Linux) or the matching folder elsewhere, so swapping files is not reliable there. Revisit later (see §12).
- Switching claude.ai in the browser. Browser profiles already do that.
- Choosing the account automatically **without being asked**. Auto-burn is a switch (§7.4); off, seatswap only *suggests* a switch.
- API-key accounts (Console billing). Only subscription logins (Pro, Max, Team, Enterprise) in v1.
- A different account per terminal. A switch is global: every Claude Code session on the machine follows it (§6.5). Claude Code has a built-in way to pin one terminal to one account (`claude setup-token` makes a 1-year token, used through the `CLAUDE_CODE_OAUTH_TOKEN` variable). Those tokens only carry the `user:inference` permission *(code)*, so the usage display would most likely not work with them. Not part of v1.

---

## 2. How Claude Code stores the login (observed)

### 2.1 Linux and Windows: `~/.claude/.credentials.json`

On Windows the file is `%USERPROFILE%\.claude\.credentials.json`. It is plain JSON, mode `0600` on Linux; on Windows it has the permissions it inherits from the profile folder (there are no mode bits). Top-level keys:

| Key | What it is | On a switch |
|---|---|---|
| `claudeAiOauth` | **The account login.** It holds `accessToken`, `refreshToken`, `expiresAt`, `refreshTokenExpiresAt` (both in ms since the epoch), `scopes`, `subscriptionType` (e.g. `team`), `subscriptionTypeSource` and `rateLimitTier` (e.g. `default_claude_max_5x`). | **swap** |
| `organizationUuid`, `trustedDeviceToken`, `trustedDeviceEnrolledAfterRefusalAtMs`, `enterpriseGateway`, `designOauth`, `cloudSessionsOauth` | **Other parts of the login.** Claude Code's logout deletes exactly these six together with `claudeAiOauth` *(code)*. On this machine only `organizationUuid` exists; it holds the account's organization. | **swap** |
| `mcpOAuth` | Logins to MCP servers (Linear, Notion, GitHub, Slack and others), keyed by server. | **keep**: these are shared |
| anything else | Unknown future keys. | **keep** |

The seven **login keys** are saved and restored as one set. A login key that the target account does not have is removed from the file, so nothing of the old account stays behind. A fresh `/login` and a renewal wrote only `claudeAiOauth` *(tested)*. The others appear later; the code that writes `organizationUuid` was not found.

Claude Code can add more fields inside `claudeAiOauth` (seen in the code: `clientId`, `subscriptionTypeVerifiedAt`, `loginEnded`). seatswap always stores and restores each login key whole and never picks single fields.

Lifetimes *(tested)*:
- The access token lives for 8 hours.
- **A login has a fixed end date.** `refreshTokenExpiresAt` is set at `/login` and no refresh moves it: four refreshes on the two accounts returned the same end date each time. Claude Code itself keeps the *earlier* of the old and the new date, and marks a login that reached its end with `loginEnded: "time_limit"` *(code)*.
- A fresh `/login` lasted 27.5 days (Acme, Team) and 29.4 days (private, Max). seatswap reads the date from the login and assumes no fixed length.
- **Every refresh uses up the old tokens.** The server returned a new refresh token each time. The old refresh token was rejected right away (`invalid_grant`), and so was the old access token (HTTP 401). The login itself stayed valid.
- Two logins of the same account exist side by side: a second `/login` in another config folder did not disturb the first.

### 2.2 macOS: Keychain

On macOS Claude Code keeps the same JSON in the login Keychain, not in a file:
- With the default config folder the service name is `Claude Code-credentials`. *(code)*
- With `CLAUDE_CONFIG_DIR` set it is `Claude Code-credentials-<h>`, where `<h>` is the first 8 hex characters of the SHA-256 of the folder path. *(code)*
- The Keychain account name is the user name (`$USER`). *(code)*
- Checked on macOS 26 with Claude Code 2.1.330 (2026-11-21): the item `Claude Code-credentials`,
  account `$USER`, in the login Keychain, holding the same JSON as `.credentials.json`; no
  `.credentials.json` on that Mac. Claude Code reads and writes it with `/usr/bin/security`, so
  seatswap uses that program too and updates the item in place (`add-generic-password -U`).
  *(tested)*
- Claude Code writes `procStart` of a session file on macOS as `LC_ALL=C TZ=UTC ps -o lstart= -p
  <pid>` prints it, trimmed; seatswap's session probe compares with the same form. *(code)*

### 2.3 Account identity: `~/.claude.json` → `oauthAccount`

This is a separate file next to the folder. It is large and Claude Code writes to it often. The `oauthAccount` object holds:
- `accountUuid`, `emailAddress`, `fullName`
- `organizationUuid`, `organizationName`, `organizationType`, `organizationRole`, `workspaceRole`
- `billingType`, `seatTier`, `userRateLimitTier`, `organizationRateLimitTier`
- `hasExtraUsageEnabled` and a few onboarding/trial fields

Claude Code uses it for display (`/status`) and for features tied to the organization. **A switch replaces `oauthAccount` and must leave every other key of the file alone.**

Claude Code itself sometimes updates this object *(code)*: when it renews a login and the profile details are incomplete, it fetches the profile and merges name, billing type, seat tier and similar fields into whatever `oauthAccount` is in the file. It does not touch `accountUuid`, the email or the organization. So the two files can disagree for a moment only if a switch lands in the middle of such a renewal; the locks in §6.2 prevent that.

The file also holds caches that belong to the account. Claude Code's logout clears 18 more keys *(code)*, among them `clientDataCacheSlots`, `cachedArtifactRoster`, `modelAccessCache` and `metricsStatusCacheByPrincipal`. seatswap leaves them alone: the ones seen in the tests carry the account or organization they were fetched for, and Claude Code replaced them by itself, a running session at its first request after a switch and a new session at start *(tested)*.

Claude Code writes this file under a lock of its own (§2.6).

### 2.4 Caches tied to the organization

`~/.claude/remote-settings.json` and `~/.claude/policy-limits.json`, each with `.signature*` / `.stamp*` siblings, are fetched per organization. **seatswap does not touch them.** Claude Code keeps them right by itself:

- Each file is signed for one organization and one account. Claude Code rejects a file signed for another *(code)*.
- A Team account checks them with the server at every start, and a running session fetched new ones 12 s after a switch to that account *(tested)*.
- A personal account (Pro, Max) leaves the other account's files on disk *(tested)*. Its log shows no sign that it reads them.

Not tested: a switch between two different Team or Enterprise organizations. There only the signature check keeps one organization's limits out of the other's session.

### 2.5 `CLAUDE_CONFIG_DIR`

If the environment variable is set, all paths above move into that folder, `.claude.json` included. seatswap honours the variable. A setting can also point it at a folder explicitly.

### 2.6 How Claude Code itself handles the login

seatswap shares the login files with every running Claude Code session, so it has to follow the same rules they use among themselves. Points marked *(code)* are read from the program. The locks were also watched in tests (`phase0/lockwatch.py`).

- **Sessions re-read the file when it changes.** *(code, tested)* Before a session uses its login, it looks at the modification time of `.credentials.json`. If the time changed, it drops its copy and reads the file again. So a running session picks up a switched login at its next request. No restart is needed.
- **A lock for renewals.** *(tested)* Before a session renews the login it takes `.oauth_refresh.lock` in the config folder, then a second lock next to the config folder (`<config folder>.lock`, the one older versions use), and writes an owner note `.oauth_refresh.lock.owner` (process ID and start time). It holds them until the new tokens are saved. A session that finds the lock taken tries 5 times, 1–2 s apart, and then gives up on the renewal for now *(code)*.
- **Two more renewal locks.** *(code, Claude Code 2.1.322)* Two secondary logins in `.credentials.json`, `designOauth` and `cloudSessionsOauth`, are renewed under their own lock folders in the config folder: `.design_oauth_refresh.lock` and `.cloud_sessions_oauth_refresh.lock`, abandoned after 60 s. The renewal follows the same pattern as the main login: read the key under the lock, call the server, save only if the key is unchanged. That its save also takes `.storage-write.lock` was read for the main login and is assumed for these two.
- **A lock for every write of `.credentials.json`.** *(tested)* `.storage-write.lock` in the config folder. Claude Code writes this file often, not only for renewals: one session start rewrote it about 20 times in 2.5 s while it added its MCP server entries. Each write reads the whole file, changes its part and writes the whole file back.
- **A lock for every write of `.claude.json`.** *(tested)* `.claude.json.lock` next to the file.
- **All of these locks are folders** made with `mkdir`. The holder keeps the folder's modification time fresh. Others treat a lock as abandoned when that time is older than 60 s (renewal), 15 s (`.storage-write.lock`) or 10 s (`.claude.json.lock`) *(code)*. The owner note only lets a session take the renewal lock over early, when the holder is proven dead *(code)*.
- **Claude Code waits for all three** (renewal, `.storage-write.lock`, `.claude.json.lock`) when someone else holds them *(tested)*.
- **Renewal uses the file, not memory.** *(code)* Under the lock the session reads the file again and sends the refresh token it finds *there*. A session never sends an old refresh token it remembers.
- **Save only if nothing changed.** *(code)* After a renewal the session writes the new tokens only if the file still holds the refresh token it sent. If something else changed the file meanwhile, it keeps the file as it is and adopts it. The renewed tokens are then lost.

What follows for seatswap:
- It must hold the renewal lock while it swaps the login (§6.2). Otherwise a switch can land in the middle of a renewal: the old account's refresh token was just used up, the new one is thrown away, and seatswap saves a dead login for that account. The same holds for the two secondary renewal locks: `designOauth` and `cloudSessionsOauth` are among the keys a switch swaps.
- It must hold `.storage-write.lock` while it rewrites `.credentials.json`, and `.claude.json.lock` while it rewrites `.claude.json`. Otherwise a write that Claude Code started just before the switch puts the old login back, or the switch wipes out an MCP login that Claude Code just saved.
- With these locks held, a running session cannot write an old account back over a switched login.
- Exceptions: sessions started with `CLAUDE_CODE_OAUTH_TOKEN`, and sessions started by another app that hands them a token (Claude Desktop), do not follow the file.

---

## 3. Endpoints (observed 2026-11-10)

| Purpose | Request | Status |
|---|---|---|
| Usage | `GET https://api.anthropic.com/api/oauth/usage` with headers `Authorization: Bearer <accessToken>` and `anthropic-beta: oauth-2025-04-20` | **Verified**, HTTP 200 |
| Token refresh | `POST https://platform.claude.com/v1/oauth/token`, header `Content-Type: application/json`, JSON body `{"grant_type":"refresh_token", "refresh_token":"…", "client_id":"9d1c250a-e61b-44d9-88ed-5944d1962f5e", "scope":"<the login's scopes, separated by spaces>"}` | **Verified** *(tested)*, HTTP 200. The server rejects Python's default `User-Agent` with HTTP 403 ("error code: 1010"), so seatswap sends its own: `seatswap/<version>` |

The refresh response holds `access_token`, `expires_in` (seconds), `scope`, and optionally `refresh_token` and `refresh_token_expires_in`. If `refresh_token` is missing, the old one stays valid, so the code must handle both a rotated and an unchanged refresh token. In every test call the response carried a new `refresh_token` and a `refresh_token_expires_in` *(tested)*. The response also names the owner: `account.uuid`, `account.email_address` and `organization.uuid`. seatswap compares these with the slot it is refreshing and refuses to save on a mismatch.

Scopes on the live login: `user:inference`, `user:profile`, `user:file_upload`, `user:mcp_servers`, `user:plugins`, `user:sessions:claude_code`. Which of them the usage endpoint needs is not known *(Phase 0)*.

The usage response, trimmed to the parts we use:

```json
{
  "five_hour":  { "utilization": 97.0, "resets_at": "2026-11-10T13:40:00+00:00" },
  "seven_day":  { "utilization": 48.0, "resets_at": "2026-11-16T01:00:00+00:00" },
  "limits": [
    { "kind": "session",       "group": "session", "percent": 97, "severity": "critical",
      "resets_at": "2026-11-10T13:40:00+00:00", "scope": null, "is_active": true },
    { "kind": "weekly_all",    "group": "weekly",  "percent": 48, "severity": "normal",
      "resets_at": "2026-11-16T01:00:00+00:00", "scope": null, "is_active": false },
    { "kind": "weekly_scoped", "group": "weekly",  "percent": 43, "severity": "normal",
      "resets_at": "2026-11-16T01:00:00+00:00",
      "scope": { "model": { "display_name": "Fable" } }, "is_active": false }
  ],
  "extra_usage": { "is_enabled": false, "...": "..." },
  "spend": { "used": { "amount_minor": 0, "currency": "USD", "exponent": 2 }, "enabled": false }
}
```

Rules for reading it:
- **Use `limits[]` first.** It is already normalised, with a kind, a percentage, a severity (`normal` / `warning` / `critical`, the last two seen *(Phase 0)*), a reset time, an optional model scope, and `is_active` for the limit that binds right now.
- Fall back to `five_hour` / `seven_day` when `limits` is missing.
- **A limit can be missing.** The private account's reply had `five_hour: null` and no `session` entry in `limits[]`, only weekly limits *(tested)*. Show only the limits the reply holds.
- Ignore unknown keys. The response carries many code-named null fields that change between releases.
- Show `extra_usage` / `spend` only when `enabled` is true.
- **A reset time seatswap cannot read** (not an RFC 3339 date) leaves that limit without a reset, and the account's card says so: `Claude's usage reply has a reset time seatswap cannot read for the session limit ("…"); it is shown without one.` The reading itself is kept. A missing or `null` reset time is normal (no window runs) and says nothing.

---

## 4. Architecture

```
┌──────────────────────────── seatswap (one Rust binary) ───────────────────────────┐
│                                                                                         │
│  core (library crate)                                                                   │
│   ├─ store      account list (no secrets) + secrets in the OS keyring                   │
│   ├─ live       read/write the live Claude Code login (file or macOS Keychain)          │
│   ├─ switcher   save current → load target, under Claude Code's locks, atomic writes    │
│   ├─ usage      poll /api/oauth/usage per account, cache, back off                      │
│   ├─ refresher  refresh logins of inactive accounts, track when each login ends         │
│   └─ watcher    notice when Claude Code changes the live login (renewal, /login)        │
│                                                                                         │
│  front ends                                                                             │
│   ├─ tray       icon + menu (tray-icon crate via Tauri 2)                               │
│   ├─ panel      pop-up window, HTML/CSS in the system webview (Tauri 2)                 │
│   └─ cli        seatswap list | use | next | add | rm | edit | status | panel           │
└─────────────────────────────────────────────────────────────────────────────────────────┘
```

**Stack: Tauri 2 (Rust core, web UI in the system webview).**
- One code base gives a tray, a pop-up window and a CLI on all three systems.
- Binaries are small, around 10 MB against about 150 MB for Electron.
- The pop-up is HTML/CSS, which is the easiest way to make it look good.
- The core is a plain Rust library, so the CLI works without any GUI. It can run over SSH and from a Claude Code status line.

Alternatives considered:
- **Python + PySide6.** Quicker to start and similar to repotray, but packaging for macOS and Windows is heavier.
- **Electron.** Too heavy for a tray app.
- **A native Cinnamon applet, GNOME extension or KDE plasmoid.** The best integration, but one per desktop. These are kept as optional extras (§9.3).

---

## 5. Data model

Config folder (Rust `dirs` crate):
- Linux: `~/.config/seatswap/`
- macOS: `~/Library/Application Support/seatswap/`
- Windows: `%APPDATA%\seatswap\`

**`accounts.json`** holds no secrets:

```json
{
  "version": 1,
  "accounts": [
    { "id": "6f1c…", "accountUuid": "…", "organizationUuid": "…",
      "label": "Acme", "abbr": "AC", "color": "#3b82f6",
      "email": "…", "orgName": "…", "plan": "Team Premium", "price": 180.0,
      "priceCurrency": "EUR",
      "oauthAccount": { "...": "copy of ~/.claude.json oauthAccount" },
      "addedAt": "2026-11-10T12:00:00Z", "lastUsedAt": "…" },
    { "id": "a92d…", "label": "Private", "abbr": "P", "color": "#f59e0b", "...": "…" }
  ],
  "providerSubscriptions": [
    { "account": { "acc": "…", "org": "codex" }, "label": "OpenAI", "abbr": "OA",
      "color": "#db2777", "plan": "Plus", "price": 23.0, "priceCurrency": "EUR" }
  ],
  "settings": { "...": "see §10" }
}
```

**Secrets.** Each account's login keys (§2.1) go into the OS keyring as one entry:
- Linux: Secret Service (GNOME Keyring or KWallet).
- macOS: Keychain.
- Windows: Credential Manager.

They use the `keyring` crate, with service `seatswap` and the account `id` as the user. If no keyring is available (a headless Linux box, or a minimal window manager without Secret Service), the fallback is `secrets/<id>.json` with mode `0600` in a `0700` folder. The UI then shows a warning that the logins are stored unencrypted.

On Windows, Codex's saved logins are always private files in the same `secrets` folder, even while the keyring is in use and Claude logins stay in Credential Manager (W-1, documented rule). One Codex login is larger than one Credential Manager entry holds (2,560 bytes), so a saved login would not fit; the copies are private files like the fallback, with no warning of their own. Codex itself keeps its live login in `auth.json` (§7.9).

**Identity.** An account is identified by `accountUuid` + `organizationUuid`. The same person in two organizations counts as two accounts, which is exactly the two-licence case. An OpenAI, Ollama or Google login is a **provider subscription** in `providerSubscriptions`: its account reference, label, abbreviation, colour, plan and monthly price with its `priceCurrency`, one per login, created when the provider's first reading arrives and edited like an account's (§7.9).

**Usage cache.** `usage-cache.json` holds the last response per account and when it was fetched. The pop-up opens at once from the cache and refreshes in the background.

**Money.** `prices.json` holds the API prices with their dates; `fx.json` the dollar rate with its dates. Both are written under the app lock, atomically and with mode 0600; their shapes are in §11.5.

**The app's own files.** Besides `accounts.json`, the secrets, `usage-cache.json`, `prices.json` and `fx.json`, the data folder holds the status line's `feed.json` and `burn-next.json`, `machine-id`, `history-root.json` (the history folder this host last wrote, below), the transcript state under `transcripts-state/`, `statusline-keys.json` while a recording runs, and `<data folder>/notification-icon.png`: the picture a notification shows, written when it is missing or holds another picture (a new data folder, one a cleaner emptied, or the icon of an older seatswap). A failed icon write is printed and the notification still goes out without an image.

**History.** seatswap keeps what it saw and did in a folder of its own: `<data folder>/history` (`historyDir` names another one). Every host writes only into its own subfolder `<host>/`, and only appends, so several hosts can share one synced folder without producing a conflict. `<host>/host.json` holds the id of the machine that writes there; when it names another machine, seatswap refuses to write into that subfolder and says why.

**A new history folder.** When `historyDir` names another folder, this host's history comes along at
the next start. `<data folder>/history-root.json` (`{"historyRoot":"…"}`) remembers the folder this
host's history was last in; without that file it is `<data folder>/history`, where earlier versions
kept it. The tray app does this at its start; a command does it only while no tray app answers on its
socket, since a running tray app may still be writing the old folder, and leaves it to that tray app's
next start.
When `<new>/<host>/` is missing, `<old>/<host>/` is copied there. When it is this machine's (its
host.json names this machine's id) or has no host.json (which seatswap claims for this machine
anyway), the two folders are merged file by file: a month file gets the lines it is missing, counted
with their repeats, and no line is ever removed. A copied line that is not valid JSON is kept and
named in a warning. The old folder is never changed. A `<new>/<host>/` folder of another machine is
not touched, and the start says so as before; an old folder whose host.json names another machine is
not brought along; a warning says so once, with both ids and the steps that bring it if it is this
machine's (its id was made anew). A move that fails is notified and tried again at the next
start; the history opens in the new folder either way.

The history has eight kinds of file, one JSON object per line, one file per month and kind: `samples-YYYY-MM.jsonl` (one line per usage answer the tray app receives, per account, when a number or a reset time changed, and at least once an hour while the app runs), `windows-YYYY-MM.jsonl` (a window that ended), `events-YYYY-MM.jsonl` (a switch; a limit crossing 100 %, as `blocked`, `unblocked`, `allBlocked` or `anyUsable`; an `accountAdded` or `accountRemoved` when the CLI or the tray app creates or removes an account or a provider subscription; a `loginEnded` or `loginRenewed` when the tray app's poll sees an account's login end or work again; a `planChanged` when the plan label changes; a `priceChanged` when an account's or a provider subscription's monthly price changes; an `accountLook` when an account's label, abbreviation, colour or "Auto-burn may use it" is edited; a `providerSwitch` when Codex's live login changes to another saved login (§7.9); a `settingSet` when a shared setting is saved; `live`; an `agentCall`; a wait), `tokens-YYYY-MM.jsonl` (what the sessions used, one line per session, 5-minute bucket and model; an older line without `min` is an hour), `sessions-YYYY-MM.jsonl` (a session's summary when it changes and when it closes, also for a session that is seen in a transcript but no longer runs), `projects-YYYY-MM.jsonl` (the project log: one line per rename, join, unjoin, organization assignment or organization edit, §11.6), `ollama-YYYY-MM.jsonl` (one line per hour of Ollama's own usage, from a successful `/api/usage?range=24h` answer, §7.9), and `up-YYYY-MM.jsonl` (at most one line per 5 minutes while seatswap runs on this host). The CLI writes the account events (`accountAdded`, `accountRemoved`); the tray app's engine writes the login events (`loginEnded`, `loginRenewed`) with the poll's time, and the app writes `planChanged` and `priceChanged` when it stores a login or a price edit changes them. The tray app and `seatswap edit` write `accountLook`, and a settings save `settingSet` (below).

**Shared between hosts.** Hosts that share one history folder also share each account's look and price and some settings; logins never travel. Per account, named by `acc` and `org`: `label`, `abbr`, `color`, `autoBurn` ("Auto-burn may use it") and the monthly price. Of the settings (§10): `currency`, `notifyOnSwitch`, `notifyUsableAgain`, `notifyWaiting`, `notifyWaitingHosts`, `notifyLoginEnds`, `statusline` and `statuslineFeed`; every other setting, the logins and the account list stay on their host. An edit writes only what it changed: `{"t":"…","type":"accountLook","account":{"acc":"…","org":"…"},"label":"Lab","autoBurn":false}` with only the changed fields, `{"t":"…","type":"settingSet","key":"currency","value":"EUR"}` with the value as the settings file holds it, and the price as the `priceChanged` it already was. Each field and each setting takes its newest value over all hosts: the latest event time, and on the same time the later host name in sort order, so every host decides the same (the last edit wins). The tray app takes them at start and then every 5 minutes; a value another host wrote passes this host's own checks first (label, abbreviation against this host's other accounts, colour, price, the setting's rule), and one that fails is not applied and is named in a warning with the host that wrote it. At start the tray app also writes this host's value of every account field and every setting no host has written yet, except a setting still at its default, so a fresh host never overrides a set-up one. An account appears on a host only after its login there; a login of an account the history knows takes its shared look and price at once. A status line too long for one history line (4096 bytes) is not shared, and the app says so. A host on an older version skips the two new lines with a warning.

**A window's `reset` and `from`.** One rule, `history::is_early_reset`, decides a reset: the percent
is at least 1 point below the previous poll reading of the same account, limit and model, and the new
reading is more than 60 seconds before the limit's reset time. The status line's merged number never
moves a percent down, so it can never show a reset; only a poll's reading is the baseline. A drop too
small for the rule, or one within 60 seconds of the reset time, is kept as a reading and logged once,
never smoothed. A finished window carries `reset: true` when it ended that way — by hand, for one
model, or by the provider after an incident. Nothing expired then, so its `lost` is absent. The
window that follows a reset carries `from`, the percent of its first reading (`0` after a manual
reset); its own use is `final − from`, and `from` absent means 0. A week can therefore hold two
windows: the one before an early reset and the one after. A reset window's `final` and `reset` keep
what it did; §11.4 counts both windows. The recorder, the "usable again" notification (§7.5) and
auto-burn's learned rates all use that one rule. A pair of readings that the rule calls a reset is
left out of the learned rates, because the drop is not use.

**A reset time that moves later.** A window ends at its reset time, or earlier when the next reading
names a reset time a whole window length later (7 days, 5 hours; less 60 seconds): the computer's
clock was behind and the new window has begun. A reset time that moves later by less is the same
window going on. An unused Codex limit names a reset one window length after every reading, so its
reset time moves later by minutes at each poll; until 1.5.3 the recorder wrote a finished window for
each such move. The reader leaves those records out: an ordinary window whose next window of the
same account, limit and model ends later but was read more than 60 seconds before this one's end
did not end: that reading saw the limit still running. The statistics pages name how many records
were left out in their warnings.

**An Ollama hour.** `ollama-YYYY-MM.jsonl` lines are `{"hour":"2026-11-16T10:00:00Z","requests":17,"input":210000,"cachedInput":160000,"output":26000,"usd":0.12}`. An hour is written once and replaced only by a later answer for the same hour whose counts did not shrink and at least one grew; an answer in which one count of the hour is smaller than that hour's last line is not written, and the hour is named once. The record belongs to no Claude account and never counts in a subscription value; §11.4's Activity page reads it.

**A token line's `min`.** Since 1.3 every new token line carries `"min": 5` and covers the 5-minute
bucket it starts in. A line without `min` is an hour, which is what every version before 1.3 wrote.
`TokenRecord::span()` is the one place that says how long a line lasts; every reader that spreads a
line over time uses it (the rates, the rhythm, the replay). The money readers (the Valuer behind
Overview, Projects, Subscriptions and Sessions) place a line at its start: a 5-minute line can fall
at most 5 minutes on the wrong side of a switch or a reset, an older hourly line up to an hour. An older seatswap reads a 5-minute line as an hour: it shows
no error, but spreads that line's work over too long a time, so every machine should be updated
together.

**`codex.json`.** The Codex scan state in `transcripts-state/codex.json` keeps, per Codex session id,
the token total it last recorded, the time of that line and the path of its newest rollout file (a
resumed session's files share one entry). It is written before the
token lines it covers, so a crash records nothing twice. An entry is pruned when its
newest file is gone and the scan no longer sees the session.

Next to the history files, each host writes one live report into its own host folder, `<host>/sessions-now.json` (§11.4): what is running on that host, for the other hosts' Sessions area. It is not history: the import never copies it and a host folder's size leaves it out.

A line is at most 4096 bytes; a longer record is an error, never cut. Times are UTC, RFC 3339; a reset time is rounded up to the full minute before it is stored. An account is named by `acc` (its `accountUuid`) and `org` (its `organizationUuid`), never by seatswap's local account id. A token line carries the project's identity as it was found at that moment, so it stays right after a folder is moved or its transcript is deleted. The transcript offsets move past an answer only after its token line was written: a pass without a working history reads the same answers again instead of losing them.

Appending is one write of the whole line plus an fsync (`atomic::append_line`). Reading opens only the month files that overlap the asked range and merges the hosts in time order. A torn last line, or a line of a shape this version does not know, is skipped and named in a warning, never guessed; the app prints each warning once as `seatswap: history: <warning>` on standard error.

---

## 6. Behaviour

### 6.1 Adding an account

1. **First run:** the live login is read and the user is offered "Save as account 1".
   - Label is prefilled from `organizationName` or the email.
   - Abbreviation is prefilled from the label's initials (see §8.2).
   - A colour is picked from the palette.
2. **More accounts:** from the menu, "Add account…" explains the steps and waits:
   1. "Your current account (AC) is saved."
   2. "In a terminal run `claude`, then `/login` with the other account." A `/logout` first is usually not needed (experience from daily use; to confirm in Phase 0, item 2). The app does not run the OAuth flow itself in v1.
   3. The watcher sees the live login change to an unknown `accountUuid`/org and offers "Save as new account".
3. Later option *(v2)*: run the OAuth flow in-app (PKCE through the browser, the same flow Claude Code uses). Then adding an account doesn't touch Claude Code at all.

### 6.2 Switching (the core algorithm)

```
switch(target):
  if target is marked "login ended": stop, show the re-login steps (§6.1)
  lock  ~/.config/seatswap/switch.lock           (exclusive file lock, 5 s timeout;
                                                       keeps two seatswap processes apart)
  lock  Claude Code's locks (§2.6), in the order Claude Code takes them:
          1. <config folder>/.oauth_refresh.lock, then <config folder>.lock   (renewal)
          2. <config folder>/.design_oauth_refresh.lock,
             then <config folder>/.cloud_sessions_oauth_refresh.lock          (secondary renewals)
          3. <config folder>/.storage-write.lock                              (.credentials.json)
          4. .claude.json.lock next to .claude.json                           (.claude.json)
        wait up to 10 s for all of them, then stop with
        "Claude Code is busy with its login, try again"
  live  = read the login keys (§2.1) from the live login   (file or macOS Keychain)
  ident = read ~/.claude.json → oauthAccount
  cur   = account matching ident.accountUuid + organizationUuid
  if cur exists:
      save live → keyring[cur]                         (it is fresher than our copy)
      save ident → accounts[cur].oauthAccount
  else:
      unlock all, ask "The current login (email) isn't saved. Save it first?"
      [Save] [Discard] [Cancel], then start again from the top
  tgt = keyring[target]
  write live login:
      file:     read whole JSON, remove all login keys, add the target's, write temp file
                (0600) in the same folder, fsync, rename over the original
      Keychain: update the generic-password item in place (same service and account)
  write ~/.claude.json:
      read → replace oauthAccount only → write temp → rename
  unlock all, in reverse order
  update tray icon, notify "Switched to P — Private"
```

Notes:
- **Order of the two writes.** The login file is written first, the identity second. A session that reads between the two sees the new login with the old name for a moment. That affects the display only.
- **If a step fails,** the switch stops with an error and names the state it left ("login switched, identity not"). It never continues quietly. The next switch or the watcher (§6.4) repairs a half-done state by matching the live login to a known account.
- **If the live login is empty or marked as ended** (`refreshToken` is `""`), there is nothing to save for the current account. Its slot is marked "Logged out" and the switch goes on.
- **How seatswap holds Claude Code's locks.** Each lock is a folder made with `mkdir`. If the folder exists, the lock is taken: seatswap waits and tries again. While it holds the locks it sets their modification time anew every 2 s, so none of them looks abandoned (the shortest limit is 10 s). A switch itself takes milliseconds.
- **Abandoned locks.** A lock folder whose modification time is older than its limit (§2.6) was left behind by a crashed process. seatswap removes it and takes the lock, as Claude Code does.
- **No owner note.** seatswap does not write `.oauth_refresh.lock.owner`. Without one, Claude Code waits the normal way and never takes the lock over early *(code)*.
- **Nobody is asked anything while the locks are held.** The question about an unsaved login comes after the locks are released, because the locks count as abandoned after 10–60 s and Claude Code waits on them.

The access token of the target may have expired. That's fine: Claude Code refreshes it with the refresh token on its next call. **seatswap never refreshes the active account itself.** Claude Code owns the active login. A refresh uses up the old tokens at once (§2.1), so every session still holding them would be cut off.

### 6.3 Refreshing inactive logins

An account is **inactive** when its login is not the one in Claude Code's file. Since every session follows the file (§2.6), no session still uses an inactive account's refresh token, so seatswap can refresh it safely.

- **Why refresh at all:** the access token lasts 8 hours. To show the usage of an inactive account, seatswap needs a valid one. When the token has expired and a usage poll is due, seatswap refreshes it (request in §3).
- **Save first.** The old tokens are dead as soon as the server answers (§2.1). seatswap writes the new ones into the keyring before it does anything else with them. If that write fails, the account is logged out; the app says so and shows the steps from §6.1.
- Like Claude Code, seatswap keeps the *earlier* of the old and new `refreshTokenExpiresAt`.
- Refresh runs under seatswap's own lock (`switch.lock`), and only for accounts that are not live. It checks under the lock that the account is still not live.
- A refresh that fails with `invalid_grant` marks the account **"Logged out — log in again"**. It shows red in the pop-up, with the steps from §6.1. The account's old tokens are not used again.
- A network error or a 5xx is not a logout. The account keeps its state and the refresh is tried again with the back-off from §7.1.

**The end date of a login.** A login ends on a fixed date that no refresh moves *(tested, §2.1)*. seatswap cannot keep a login alive. It makes sure the end is never a surprise:
- The pop-up shows the login's end for every account: "login ends today" on its last day,
  "login ends in 3d" otherwise.
- A notification comes 5 days and 1 day before the end, with the re-login steps from §6.1.

This is accepted (decided 2026-11-10): a `/login` per account about once a month is what is needed today without the app.

### 6.4 Noticing changes made by Claude Code

The watcher watches the live login:
- inotify on Linux, FSEvents on macOS, ReadDirectoryChanges on Windows.
- Keychain items can't be watched, so on macOS the login is polled every 30 s.

Cases:
- **Same account, new tokens** (Claude Code renewed them): nothing to do. The keyring copy is updated at the next switch. Optionally copy right away (setting `mirrorLive`, default on) so a crash never leaves a stale slot.
- **Different known account** (someone ran `/login` by hand): the tray follows it.
- **Unknown account:** the app offers to save it (§6.1).
- **Login gone or ended** (`/logout`, or Claude Code marked it as ended): the tray shows "no login" in grey. The account's slot is marked "Logged out".

The watcher reads the file only when Claude Code's lock is free, so it never mirrors a half-finished renewal.

### 6.5 Sessions that are already running

**A running session follows the switch at its next request** *(tested)*. It notices that the login file changed, reads it again and continues on the new account. It does not write the old account back. So a switch moves every open terminal session at once, including an agent that is in the middle of a long task.

The test: one open session (Claude Code 2.1.322 in a terminal), switched private → Acme → private → Acme. The last switch came between two tool calls of one turn. No restart was needed and no error appeared.

What this means in practice:
- This is what one wants when an account is nearly used up: switch, and the running work continues on the other account without a restart.
- A request that is already under way finishes on the old account. The next one goes to the new account.
- **The first request after a switch costs more** *(tested)*. The server keeps a copy of the conversation so far to make follow-up requests cheap, and that copy belongs to one organization. After a switch the conversation is written again for the new one: about 20k tokens in the test. Switching back to an account used a few minutes earlier only costs what was added meanwhile (3.9k tokens after 13 minutes).
- The pop-up counts the running `claude` processes and says before a switch: "3 sessions running — they move to P with their next request".
- Sessions that do not follow: those started with `CLAUDE_CODE_OAUTH_TOKEN`, and those started by Claude Desktop.

Not tried yet *(Phase 0)*:
- Sessions inside VS Code. On this machine the extension runs its own copy of Claude Code (2.1.321). Daily use suggests they follow a switch too. The test is postponed until no work sessions are running.
- A switch between two Team or Enterprise organizations (§2.4).
- macOS, where the login is in the Keychain and Claude Code keeps its copy for up to 30 s.

There is no special handling for an "old account written back" event. If the live login ever changes to another known account, whatever the cause, the rule in §6.4 covers it: the tray follows.

### 6.6 Agents

Every command Claude Code runs carries `CLAUDECODE=1` in its environment; an agent call also carries `AI_AGENT` (for example `claude-code_2-1-322_agent`). seatswap treats a call as an agent call when `CLAUDECODE` is exactly `1`, or `AI_AGENT` is set and not empty. A person's terminal has neither.

Two settings decide what an agent may do. `agentsRead` (on by default) lets agents run `list` and `status`, also with `--json`, `--refresh`, `--waybar` or `--i3blocks`. `agentsSwitch` (off by default) lets agents run `use <account>` and `next` without `--save-current` or `--discard-current`. `add`, `rm`, `edit`, `agent-skill`, `panel`, `use` or `next` with one of those two flags, and `seatswap` without a subcommand (which starts the tray app) can never be run by an agent, whatever the settings.

| Command | Agent may run it when |
|---|---|
| `list`, `status` | `agentsRead` is on; otherwise refused with `agents may not read usage (seatswap settings → Agents)` |
| `use`, `next` without `--save-current` / `--discard-current` | `agentsSwitch` is on; otherwise refused with `agents may not switch accounts (seatswap settings → Agents)` |
| `use` or `next` with one of those flags; `add`, `rm`, `edit`, `agent-skill`, `panel`; no subcommand | never; refused with `agents may not change seatswap's accounts or settings` (no subcommand is recorded as `(no subcommand)`) |

A refused call prints `seatswap: <text>` to stderr and exits with code 3; nothing is half done. Every call by an agent writes an `agentCall` event with the subcommand and its flags (without free-text values, e.g. `use PR` or `status --json --refresh`), the folder it was called from, and whether it was allowed. A switch by an agent is written with `by: agent` and the folder, and the tray always announces it: `An agent switched to PR (from ~/projects/atlas).` — or `An agent switched to PR.` when the folder is not known — whatever `notifyOnSwitch` says.

`seatswap agent-skill install` writes `<Claude config folder>/skills/seatswap/SKILL.md` from the bundled text; it refuses to overwrite a file that differs from it unless `--force`, and prints the path. `seatswap agent-skill uninstall` removes that file (with `--force` if it differs) and the `seatswap` folder when it is empty. Nothing else writes into Claude Code's config folder.

What the settings page says about the two switches:

> These two switches work through a marker that Claude Code puts on every command it runs. An agent runs as you. It can remove the marker, or change seatswap's files directly. The switches stop agents that follow the rules. They are not a lock. To block the command itself, add a deny rule for `seatswap` to Claude Code's permissions.

---

## 7. Usage

### 7.1 Polling

| Account | Interval | Also |
|---|---|---|
| Active | 2min | right after a switch; when the pop-up opens (if older than 30 s) |
| Inactive | 15min | when the pop-up opens (if older than 5min) |

- On an HTTP 429 or 5xx, back off exponentially up to 30min and show the time of the last fetch. A
  rate limit's message names when seatswap asks again, in local time: `Claude limits how often
  seatswap may ask; next try at 10:42`.
- On a 401: for the active account, read the live file again (Claude Code may have renewed it). For an inactive account, refresh (§6.3).
- An account that a limit blocks is polled once 30 s after that limit's reset time, whatever its interval, so a notification does not wait for the 15-minute poll of inactive accounts.
- While the active account's binding limit is at 90 % or more and below 100 %, it is polled every 30 s; at 100 % the reset poll above applies. The status line's feed (§7.6) can make the numbers newer still, without a request. The feed never moves a timer poll: the poll schedule keeps its times whatever the feed does.
- No polling while the machine sleeps. Poll once on wake.

### 7.2 What the app derives from the response

For each account it shows:
- **Session (5h):** percentage and time until reset (`1h 12min`, `35min`). Only if the reply has a session limit (§3).
- **Week, all models:** percentage and the reset: more than 48h away the weekday and clock time (`Sun 02:00`); otherwise the time until it, `1d 18h` from 24h on, `5h 40min` below.
- One rule for every reset shown (pop-up, tray menu, CLI): under 24h hours and minutes, 24–48h days and hours, beyond 48h weekday and clock time. Minutes round up, and a reset time is shown rounded up to the full minute: the server sends a moment just before the boundary (`00:59:59.59`), which is meant as `01:00`.
- **Week, scoped:** one row per scoped weekly limit (e.g. Fable), percentage only.
- **Binding limit:** the one with `is_active: true`, or else the highest percentage, the first of
  equals. This drives the tray icon.
- **Usable:** the login works, the usage is known, and no limit blocks the account. The week is below 100 %, the 5h session below 100 %, and a per-model weekly limit below 100 % while a running session uses that model. A per-model limit at 100 % blocks the account only while a session on that model is open (busy or idle); with `readSessions` off, it blocks only while the server marks it as the binding one.
- **Which limit blocks:** when several limits are at 100 %, the one with the latest reset time is named; one without a reset time comes before one with a time. On a tie the week comes before a model, and a model before the 5h session.
- **Extra usage / spend:** only if enabled.

### 7.3 Colours by load

| Load | Colour | Meaning |
|---|---|---|
| < 70 % | green `#22c55e` | fine |
| 70–89 % | amber `#f59e0b` | getting close |
| ≥ 90 % or `severity: critical` | red `#ef4444` | about to hit the limit |
| 100 % / locked | red + lock glyph | limit reached, shows the time until reset |

### 7.4 Auto-burn and switch suggestions

Auto-burn is one switch, `autoBurn`, off by default. On, seatswap switches by itself; off, with
`suggestSwitches` on (default), it says what it would do and why, with a [Switch] button. There are
no modes and no tuning settings: every threshold is a constant in the code (see below and
`docs/implementation.md`).

| # | Trigger | What happens |
|---|---|---|
| S1 | The active account is blocked, or there is none; or its 5h window is at 95 % or more and another usable account's window is below 95 % | switch at once to the usable account with the least slack, preferring one whose 5h window is below 95 % |
| S2 | Another usable account has clearly less slack | when it is at risk of losing quota (its shared slack is 8 typical working hours or less): switch to it; otherwise only while early switching is on. At a quiet moment (below), at once when its shared slack is 0 or less |
| S3 | Nothing is usable | no switch; the "All accounts are full" notification of §7.5, and the reset poll of §7.1 |

- **Blocked** means the week or the 5h window is at 100 % or more, the login has ended, or a
  per-model limit at 100 % blocks the account while a session on that model is open (§7.2).
- **A nearly full 5h window is left at 95 %.** The window holds no quota of its own: what the account
  does not use in it stays in its week. So leaving 5 % before the wall costs nothing, while the
  usage figure trails the real use by a few minutes and Claude Code may stop at the wall before
  seatswap reads 100 %. The move needs another usable account whose 5h window is below 95 % (an
  uncapped account always is), no pause and at least 5 minutes since the last switch, like an early
  move. With no such account auto-burn stays and the account works to 100 %; it is not full and
  never gives "All accounts are full" at 95 %. `choose` passes over accounts whose window is at
  95 % or more while another usable account's is below. The week still counts as full only at
  100 %: quota left there is gone until the reset. The icon shows the real figure. The plans
  (§2.4) and the Forecast play the same rule.
- **Least slack** is the work the account can still take before its weekly reset minus the work
  needed to empty it. A capped account can only take a window's worth at a time, so its remaining
  5h windows count, not just its percent. An account whose rates are not learned yet is played
  like one without rates: the fixed order is used instead, capped accounts with an open window
  first, then the others, each by the earlier weekly reset and then the account id.
- **Shared slack:** work goes to one account at a time, so accounts whose weeks reset close
  together can each have room alone and still lose quota together. For each weekly reset at or
  after the account's own, take the work until that reset (at most what the accounts resetting by
  then can take) minus the work needed to empty them; the shared slack is the least of these and
  the account's own slack. Two accounts resetting together, each needing 20 of the 37 working
  hours left, have 17 hours of slack each but a shared slack of −3. Only accounts auto-burn may use,
  with a live login, no per-model block, a week below 100 % and known rates count.
- **S2** needs another usable account whose slack is at least 2 typical working hours below the
  active account's, at least 5 minutes since the last switch of any kind, and no pause (§7.4.3).
  When that account's shared slack is 8 typical working hours or less, it is at risk of losing
  quota at its weekly reset, alone or with the accounts that reset before it: slack counts only the
  usual working hours, and every hour the machines are off beyond them costs quota. So auto-burn
  switches to it whether early switching is on or not, also back to it once its 5h window has
  reset. Keeping quota from expiring comes before saving a switch. With a shared slack above that
  cushion it switches only when the expected saved hours are worth it (below). The 2-hour margin
  compares the accounts' own slacks, so two accounts at risk together do not take turns.
- **Seeing 100 % quickly:** the active account is polled every 30 s while its binding limit is at
  90 % or more and below 100 % (§7.1), and the status line feed (§7.6) brings the live numbers
  without a request.
- An account with `autoBurn` off is never a target and is never suggested. When it is the active
  account and gets blocked, S1 still leaves it (§7.4.4), and so does the 95 % rule for its 5h
  window. While such an account is not blocked, auto-burn stays on it: S3 is reached only when the active account is blocked (or there is none)
  and `choose` finds no usable account. An active account auto-burn may not use is also not a
  reason for S2.
- **Going back restores a session's earlier thinking.** When every usable account has a learned
  slack, the choice is not only the least slack. Every usable account whose slack is at most
  2 typical working hours above the least one is a candidate (two slacks within 1e-9 count as
  equal), and among the candidates the account that produced more of the thinking the running
  sessions still hold wins; with none, or with equal counts, the usual order decides: slack, weekly
  reset, account id. The count is per minute: for every running session on a model whose thinking is
  tied to the account (§7.4.2), each answer with thinking since the user's last message adds one to
  the minute of its start, and that minute belongs to the account that was live on this host then,
  as the switch history says. A minute without a known account is left out. It needs learned rates
  (without a slack there is nothing to compare), `readSessions` on, and the sessions thread's list of
  running sessions; the command line does not know the running sessions and plans without the
  counts. The plan and the decision use the same choice, so the pop-up names the account auto-burn
  will switch to.
- **An early switch still needs its distance.** S2 to the account that holds the thinking needs that
  account's slack to be at least 2 typical working hours below the active account's, as for any S2.
  Until it is, auto-burn stays, even when the account with the least slack would have passed; it
  moves when the account with the thinking is that far below, or when another account is clearly
  more urgent. The "wait for a quiet moment" rule of §2.12 applies as before. With `readSessions`
  off nothing is held back.

While early switching is on, the pop-up's next line, `burn-next.json`, the status line's
auto-burn segment and `status --json`'s `next` follow the plan auto-burn follows — the eager plan —
and otherwise the lazy plan, which leaves an account before 100 % only for one at risk of losing
quota. That is the same plan the Forecast shows. The suggestion's "Staying on …" sentence is the
plan that switches only when the account is full ("only when full": blocked, or its 5h window at
95 % with another window that has room), not the followed plan.

#### 7.4.1 The regime

Once an hour seatswap plans the next 7 days twice: the **lazy** plan stays on an account until it
is blocked and then takes the one with the least slack, and it moves earlier only to an account
at risk of losing quota (S2 with a shared slack of 8 typical working hours or less); the **eager** plan also takes the usable
account with the least slack when it has at least 2 typical working hours less slack. Both make
at most one early move per 5 minutes. Both start from the real last switch and the pause
(§7.4.3): the first early move comes no sooner than 5 minutes after that switch and not before the
pause ends, exactly when S2 may switch. So the pop-up, `burn-next.json` and the status
line never name a switch for "now" that auto-burn will not make yet. Early switching (S2) is on
only when all of these hold:

- every account auto-burn may use has rates (measured or assumed, below), and the rhythm has a
  typical working hour;
- the measured cost of a switch has at least 3 samples;
- the eager plan blocks at least 0.5 typical working hours less than the lazy plan;
- those saved hours are at least twice the extra switches' cost (extra switches × the measured
  cost of one switch, in working hours).

**Rates, measured and assumed.** An account's rates say how many percent of its week (`c`) and of
its 5 h window (`s`) one API dollar of work costs: a factor. Work is the token records of Anthropic
models, each priced at its hour at the model's base models.dev price (`money::work_usd`); the
long-context premium of a tiered price is left out (documented rule). An OpenAI or Ollama record
uses no Anthropic account and counts neither here nor in the rhythm. A record whose model has no price is
left out whole, never valued as 0, and one warning names each such model once. The work of a record
is spread evenly over its own span: its `min` minutes, 5 for a line written since 1.3 and 60 for
every older line (§5).

Rates are measured over the last 14 days from pairs of readings of one weekly window at most 6 h
apart: the percent gained between them over the API dollars of work done in between. A pair counts
only when the account was live on some host for at least 90 % of the time between its readings (the
union over hosts, documented rule): a rise outside that time comes from use seatswap does not see
(claude.ai, the phone, a machine without seatswap). The pairs inside one 5 h window give `s` the
same way.

A factor's estimate is `(Σ gain + prior × 100 USD) / (Σ usd + 100 USD)` while the account's own pairs
are below 100 USD and it has a prior, so the prior's weight fades as the account's own dollars grow;
without a prior it is `Σ gain / Σ usd`. Its 80 % range is 1.2816 standard
errors of all pairs around that line, with the prior counted as one more pair of 100 USD while the
prior counts (documented rule); without pairs the range is the prior itself. Once Σ USD reaches
100 USD the factor is measured and the prior is dropped: value, range and standard error are then
computed from the account's own pairs alone. A measured account whose own gain is ≤ 0 has no factor,
never one lifted by a prior. A factor that is not above 0 is unknown, never a number. Below 100 USD of
the account's own work it leans on its prior.

The prior of an account and a limit comes from the factors of the **other** accounts that are
measured from their own pairs alone, in this order (documented rule):
1. pooled over those with the same plan name, `Σ gain / Σ usd`;
2. else pooled over those with a capacity, by their capacity and divided by this account's;
3. else, for the week only, 0.75 % per API dollar at capacity 1 (measured on the accounts in
   November 2026: Max at 5 ≈ 0.15, Team Premium ≈ 0.087, Ultra at 20 ≈ 0.042 % per dollar) divided
   by this account's capacity;
4. else no prior: the account has a factor only once it is measured.

A plan has a capacity when its rate-limit tier ends in `_<n>x`: Max and Team Premium 5, Ultra 20. A
plan whose name is exactly "Pro" has capacity 1 (documented rule); Team Standard and every other
tier without a multiplier have none. An account without a capacity has rates only once 100 USD of
its own work was measured. `s` is assumed the same way, but only from measured windows (there is no
built-in value), and an account whose `s` is not known yet is planned without its 5 h limit; at 100 %
it is still left. Nothing of this is shown in the pop-up: learning is never a message there.
Statistics → Forecast names an account without any week factor `{abbr}'s rates are not known yet.`
and one whose factor leans on its prior `{abbr}'s rates are assumed from its plan.` (or `their
plans.` for several).

The measured cost of a switch comes from the target's percent gained in the 10 minutes after it,
minus the API dollars worked in that window at `c`. A window that holds an Anthropic record without
a price is left out whole (documented rule), never measured with that work missing.

While any of these is missing, auto-burn switches only when the active account is full or another
account is at risk of losing quota. With `readSessions` off there is no work to price: no rates are learned,
the regime says `reading sessions is off: no rates are learned`, and auto-burn switches only when
the active account is full. The
regime's words are `learning`, `no switch measured yet`, `switches early only for quota at risk of expiring` or
`switches early whenever it pays`. Settings → Auto-burn adds `no other early switches` while early switching is off.

#### 7.4.2 Quiet moments

A switch makes a running session send its conversation again, and on some models it continues
without its earlier thinking (`thinking_tied` in `docs/implementation.md`); a model off the Claude
plan (Harness, Codex, Ollama) is never tied, since a switch does not touch it. S2 therefore waits
while sessions on such a model are busy: the decision is a *waiting switch* to the target, with the
latest time at which the rhythm's work from now reaches the target's shared slack (the end of the 7-day
horizon when it never does). The pop-up
shows `A switch to AC is waiting: 2 sessions on a model that would lose its thinking are busy. At
the latest 18:40.` and the switch happens when those sessions are no longer busy, or at the latest
time. A target with no shared slack left is switched to at once instead of waiting. S1 and S3 never wait.
With `readSessions` off there is no session count and S2 does not wait.

#### 7.4.3 Working by hand

- **Manual switch while auto-burn is on.** A switch auto-burn did not make — by a person
  (`seatswap use`, `next` or the pop-up), the CLI, an agent, or Claude Code's `/login` — holds
  nothing back (since 1.5.2): auto-burn stays on the account you chose until it is full or another
  account is at risk of losing quota, and then switches as usual, at least 5 minutes after your
  switch. While early switching pays it may also move to an account with clearly less slack. To
  keep your choice, pause auto-burn.
- **Pause.** "Pause 4h" in the pop-up writes an `autoBurn` event with `pausedUntil`; "Resume"
  writes one with no end. The pop-up shows `Auto-burn paused until 18:10 · Resume`. A pause holds
  S2 back until it ends or is resumed; S1 still leaves a full account. Turning `autoBurn` on or off in the
  settings writes `autoBurnSet`, which is neither a pause nor a resume and leaves both alone.

#### 7.4.4 Per account

An account carries `autoBurn` ("Auto-burn may use this account"), default on. Off, auto-burn never
switches to it and never suggests it. If it is the active account and gets blocked, S1 still
leaves it; if every other account is also unusable, S3 applies.

#### 7.4.5 The texts

Times are local `HH:MM`, or `Mon HH:MM` when not today; durations are `35min`, `2h`, `2h 5min`,
`1d 3h`, with at most two units and no leading zero (`2h 5min`, not `2h 05min`). From a day on the
minutes are not shown (`3d 4h`); `about 2h` keeps its word.

- **The line under the switch:** `On PR until its week is full, about 2h. Then AC.`, `On AC until
  its 5h window is full, about 1h 30min. Then LB.`, `On PR for about 2h. Then AC, before PR is full.`
  (the plan leaves PR before it fills), `On PR. No switch planned this week.` The line never
  speaks of learning: an account without rates is simply not planned to fill (§7.4.1).
- **An automatic switch** (when `notifyOnSwitch` is on): `Auto-burn: now on PR. AC is full until
  14:40.` or `Auto-burn: now on PR: its week resets on Mon 02:00.`
- **A suggestion** (auto-burn off): `Switch to AC now: its 5h window is open and its week resets
  on Mon 02:00.` When the plan that switches only when full ("only when full"), not the plan auto-burn
  follows, expects more than 1 % of the target's week to be left unused at its reset, it adds
  ` Staying on PR is expected to let 12 % of AC expire.` The pop-up shows the same sentence and a
  [Switch] button; the desktop notification is sent once per target and hour.
- **The reason of a switch:** `AC is full until 14:40`, `its 5h window is open and its week
  resets on Mon 02:00`, `its week resets on Mon 02:00`, or `no account was in use`.
- While auto-burn is on, no suggestions are sent: it would ask for what auto-burn does anyway.

#### 7.4.6 How old a reading may be

A decision needs a reading at most 20 minutes old for every account auto-burn may use, plus the
active account; the oldest of those readings is the one tested. An account in that set with a
login but no reading at all also stops a decision. The reading's age is the later of its
poll and its feed (`fetchedAt` in the cache). When the oldest is older than 20 minutes, auto-burn
does not switch or suggest: it asks for a poll, at most once a minute, and the engine polls only
the accounts whose last reading or attempt is older than 20 minutes. The status line feed (§7.6)
is a reading too, so a running session can keep the numbers fresh without a request.

### 7.5 Notifications about limits and waiting

- **Usable again.** When an account that a limit blocked is usable again and is not the active account, seatswap sends `PR is usable again: its 5h window reset.` The reason is `its 5h window reset`, `its week reset` or `its {model} week reset`. It is sent at most once per account and reset minute, and only while `notifyUsableAgain` is on. The poll that sees the block end is what makes it timely: the poll 30 s after the reset time for an ordinary end, or the next snapshot for an early reset. It is sent when the block ended because its reset time passed (the previous `blocked_until` is at or before now), or when the blocking limit fell early by the one reset rule (`history::is_early_reset` in §5): then the reason is `its 5h window was reset early`, `its week was reset early` or `its {model} week was reset early`. A per-model block whose last session on that model closed, or a block the server lifted without the percent falling, sends nothing and does not use up the once-per-reset.
- **All accounts are full.** When every account whose login still works is blocked by a limit, and the previous snapshot had a usable account, seatswap sends `All accounts are full. Next: AC at 14:40.` once; without a next reset time it is `All accounts are full.` An account whose usage is not known yet is not usable but not blocked either, so it does not count as full. Only accounts whose login still works count.
- **Waiting for you.** A session that is still waiting 15 s after it entered `waiting` sends `acme-kb-dd is waiting for you (acme-kb, on atlas).` The session is named by its title when it has one (`Fix the invoice export is waiting for you (acme-kb, on atlas).`), else by its label: the session's name, or `<project>-<first two hex digits of the session id>` when the session has no name. The host part is left out when this host's name cannot be read: `acme-kb-dd is waiting for you (acme-kb).` It is sent once per session and wait, and only while `notifyWaiting` is on. When the session stops waiting, its notification is withdrawn. With `notifyWaitingHosts` set to all hosts, another host's waiting session notifies here too, with that host's name; it arrives with the delay of the shared history folder (`sessions-now.json`, §11.4), so the 15 s counts from the session's own `since`.
- A waiting notification does not time out where the desktop allows that (Linux). On Windows it follows the system's rules; the pop-up keeps the count either way. A click opens the Sessions area at that session, opened and marked for 3 seconds; a session that is no longer listed says so there.

### 7.6 `seatswap statusline` and the feed

`seatswap statusline` is the command Claude Code runs every 2 seconds instead of a shell script. It
reads Claude Code's JSON from standard input, seatswap's layout from its settings, and the usage
from its cache, and prints one line.

- **What it reads from Claude Code:** `workspace.current_dir`, `model.id`, `model.display_name`,
  `effort.level`, and `rate_limits.five_hour` and `.seven_day`, each with `used_percentage` and
  `resets_at` in seconds since the epoch; for the feed it also reads `session_id` and
  `cost.total_cost_usd`. Unknown keys are ignored. A missing key, or a `null`
  whole object such as `"five_hour": null`, is absent; a `null` inside a limit makes the whole
  payload malformed. Anything that is not a JSON object of this shape is treated as an empty
  payload: a malformed payload must not hide the line.
- **What it prints:** the layout `statusline: {left, right, margin}`. The preset "My current line"
  is `sam@atlas:~/projects/atlas Fable 5.2 high` on the left and `AC 12% 4h 45min  48% 3d 17h` on the
  right. A segment without a value prints nothing, with its prefix and suffix. The right part is
  flush with the terminal's right edge minus the margin; when the line does not fit, segments are
  dropped by their `drop` number (1 first) until it does, and when nothing is left to drop it is
  printed anyway. Without a width the right part follows the left after two spaces while both
  zones print something, and directly when one of them is empty.
- **The width** is the terminal of the nearest ancestor process that has one, else `$COLUMNS`,
  else none.
- **Never slow, never a failure:** it uses no network, takes no lock that can wait, does not build
  the engine or the HTTP client, and always exits 0. Any error becomes the line itself:
  `seatswap: <reason>`, because a non-zero exit makes Claude Code hide the whole line. `CLAUDECODE`
  may be set; the command is never gated and writes no `agentCall` event.
- **The recorder's file is not the line:** a `statusline-keys.json` that cannot be parsed does not
  change the printed line and does not stop the feed; the page shows that file's error (§11.4).
- **The preset and the segments:** the preset reproduces the old script. The layout can also hold
  the account (abbreviation in its colour), plan, 5h percent and reset, week percent and reset,
  per-model percent, auto-burn, model, effort, folder (`~` short), git branch, context used, session
  cost, lines changed, `user@host`, clock, free text and a separator. Each segment has an optional
  colour rule (fixed, by load with two thresholds, by model family, by effort, or the account's
  colour), prefix, suffix and drop order; reset segments can use the compact time left, seatswap's
  own format (§7.2) or a clock time. The `autoBurn` segment shows the next account and the planned
  switch time, like the reset segments show their times; it has no value when the tray app plans no
  switch (§7.6). The three segments Claude Code itself supplies read
  `context_window.used_percentage` (`41%`), `cost.total_cost_usd` (`$3.20`) and
  `cost.total_lines_added` with `cost.total_lines_removed` (`+120 -8`) from the payload; a field that
  is `null` or absent prints nothing, as before a session's first answer, when `used_percentage` is
  `null` and `total_cost_usd` a whole `0` (`$0.00`).
- **Turning it on:** the large window's Status line area builds the layout and writes it into Claude
  Code with *Use in Claude Code*; that is the one key `statusLine` of Claude Code's `settings.json`,
  and *Restore previous* puts the earlier value back (§11.4).

**The feed.** When `statuslineFeed` is on (default) and the payload has `rate_limits` with a
`seven_day`, the command writes the live numbers to `<data folder>/feed.json`, atomic and mode
0600:

```json
{"at":"2026-11-15T09:41:12Z","fiveHour":{"percent":12.0,"resetsAt":"2026-11-15T14:30:00Z"},
 "sevenDay":{"percent":48.0,"resetsAt":"2026-11-16T01:00:00Z"},"model":"claude-fable-5-2",
 "session":"7c1e4b2a-9d3f-4e6a-b8c5-2f0a1d7e9b34","costUsd":3.2}
```

- It rewrites the file only when the numbers changed, the session, the cost or the cost problem
  changed, or the file is at least 10 seconds old, and never writes a partial feed: a payload
  without `seven_day` writes nothing. `session`, `costUsd` and `costProblem` are absent while they
  are `None`; a 1.1.0 feed has none of them. A cost that is `null` or absent is no cost; a cost
  without a `session_id`, one that is not a number, or one that is not a finite number of 0 or more
  is not stored, and `costProblem` names the session when it has one and gives the reason while the
  status line itself stays a line.
- **Which account do the numbers belong to?** The payload does not name the account. A session
  that has not sent a request since a switch still carries the old account's numbers. The numbers
  are therefore accepted only for the one stored account whose cached weekly reset is within 60
  seconds of `sevenDay.resetsAt`; none or more than one match means they are dropped. They are
  never stored for the new account because a session still carries the old one.
- **The tray app takes it.** After a write the command sends `feed` on the existing single-instance
  socket; no tray app running is not an error, the file waits. The tray app reads the file, merges
  it into that account's cache entry only when `at` is later than both the poll's `fetchedAt` and
  the last feed, and writes a usage sample with `src: statusline` like a poll. The 5h percent and
  its reset follow the feed when its window is new or the same window is not lower; a 5h window
  whose reset is at or before the feed's `at` is ignored, so a window that has already ended cannot
  come back after a poll replaced it. The weekly
  percent follows it only when it is not lower, and the weekly reset stays the cache's. Per-model
  limits stay the cache's. The polls stay as they are: they bring the per-model limits and the
  inactive accounts.
- **Effect:** the active account's numbers are seconds old instead of up to 2 minutes, without an
  extra request to Anthropic; auto-burn sees 100 % sooner. With `statuslineFeed` off the command
  writes no feed file and sends nothing.

**The next account.** The tray app writes `<data folder>/burn-next.json`, atomic and mode 0600:

```json
{"account":"<account id>","at":"2026-11-15T14:40:00Z","written":"2026-11-15T09:41:12Z"}
```

`at` is the planned switch in UTC and `written` is when the app wrote the file. The app writes it
when the next account or time changes and at least every 10 minutes as its heartbeat; it removes
the file when auto-burn is off, paused, or nothing is planned. The `autoBurn` segment reads the
file without a lock: a missing file shows nothing, an account id that is no longer stored shows
nothing, and a `written` older than 20 minutes shows nothing because the tray app is not running.
A file that cannot be parsed is the line's `seatswap: <reason>` error. The polls keep their times
whatever this file says.

### 7.7 `seatswap burn plan` and `seatswap burn simulate`

Both commands only read; for an agent they follow `agentsRead` like `status` and `list`.
`seatswap status --json`'s `next` is planned the same way, without the sessions' thinking data, so
it can name another account than the pop-up while sessions on a model that would lose their
thinking run.

- **`seatswap burn plan [--json]`** prints the early-switching regime and the plan auto-burn
  follows (eager while early switching is on), read the same way the tray app reads them (with no
  session count, so `tied_busy` is 0). The text form is
  the regime's words, then one line per segment: `Mon 08:00–Mon 10:00  AC`, or `… nothing usable`.
  `--json` prints one object with the regime (`early`, `why`, `blockedLazyH`, `blockedEagerH`,
  `extraSwitches`, `costH`), the segments (`account`, `from`, `to`; `account` is null when nothing
  is usable) and `leftAtReset` (week percent left at each account's first reset). An account
  auto-burn may use (or the active one) with a login but no reading, or no account with a login
  and a reading, is an error: there is nothing to plan from.
- **`seatswap burn simulate --from YYYY-MM-DD --to YYYY-MM-DD [--without ABBR]…`** replays the
  recorded work of those local days (both inclusive) once for each of the two policies, **lazy**
  and **eager**, and prints for each the hours worked, the hours blocked, the number of switches,
  and the week percent that expired at each account's reset. `--without` leaves an account out.
  The replay starts at the first sample of each account in the range, starts with no active
  account, and uses the recorded work spread over each line's own `min` span; it counts only Anthropic
  work, so an OpenAI or Ollama record is left out, and a record whose model has no price is left
  out whole (documented rule), never replayed as 0. An account without learned rates never fills.
  The rates are learned from the history as the tray app learns them. Warnings from a torn history
  line go to standard error.

### 7.8 `status --json` and agents

`seatswap status --json` prints one object:

- `views[]`: every account, with `id`, `label`, `abbr`, `color`, `email`, `plan`, `autoBurn`,
  `isActive`, `loginState`, `loginEndsAtMs`, `loginDaysLeft`, `usage`, `usable`, `blockedUntil`,
  `fetchedAt` and `lastError`, plus `slackHours` while it is known (the working hours the account
  can be left alone and still be emptied before its reset).
- `active`: the id of the live account, or null.
- `suggestion`: `{toId, text}` or null. It is the same suggestion as §7.4.5, only while auto-burn
  is off and `suggestSwitches` is on.
- `autoBurn`: `{on, pausedUntil}`, where `pausedUntil` is null when there is no pause.
- `next`: the id auto-burn would switch to (the target of a switch or a waiting switch, otherwise
  the first account of the plan auto-burn follows other than the active one), or null.
- `burnError`: a history or planner error as text, or null. When it is set, `next` is null and the
  views have no `slackHours`; the command still answers.

Only the JSON form reads the history; `--waybar` and `--i3blocks` do not, because they run every
few seconds. With auto-burn on, `seatswap next` called by an agent prints
`seatswap: auto-burn is on and decides` and exits 0 without switching or touching a login; the
agent call itself is recorded as usual. A person's `next` still switches.

### 7.9 Other companies: OpenAI, Ollama and Google

seatswap shows three companies: **Anthropic** (Claude's plans), **OpenAI** (Codex) and **Ollama**
(DeepSeek Harness and flash workers on Ollama's cloud). "Codex" is only the tool's name: the card
is "OpenAI (Codex)", and Settings → Providers keeps "Codex folder". The company of a model id is
Anthropic's when it starts with `claude-`; Ollama's when it is `<name>:cloud` or
`<name>:<tag>-cloud`; any other id is OpenAI's. The company of an account is OpenAI for the org
`codex`, Ollama for the org `ollama`, and Anthropic for every other org (a Claude organization id).
The pages write the companies' names; every company field of the statistics JSON is the lowercase
key `anthropic`, `openai`, `ollama`.

**Provider subscriptions.** An OpenAI or Ollama login is a subscription of its own, stored in
`accounts.json` as `providerSubscriptions` beside the Claude accounts: the account reference, label,
abbreviation and colour, the plan and the monthly price with its `priceCurrency`, exactly as an
account's. It is created when that provider's first reading arrives, by the one function that also
writes its `accountAdded`; a later reading that names another plan updates it and writes
`planChanged`. A new subscription starts as "OpenAI", `OA`, `#db2777` or as "Ollama", `OL`,
`#ea580c` (documented rule). Its abbreviation is unique among the Claude accounts and the other
subscriptions; when `OA` or `OL` is taken, the first free `O2` … `O9` is used. Every abbreviation
goes through the same rule as an account's (§8.2). OpenAI's plan comes from the provider's
`plan_type`; Ollama's reply names no plan, so its plan is typed in Settings and shows `—` until
then. A price edit writes `priceChanged` for the subscription's account reference; looks and prices
are shared between hosts like an account's.

A provider is polled while its card is on **or** when it is found by itself, even with the card off
(R6). Codex is found when `<codex folder>/auth.json` exists. Ollama is found when the `ollama`
program is in a folder of `PATH` (`ollama.exe` on Windows) or when this host's `accounts.json` holds
an Ollama subscription; an Ollama subscription that only another host stores does not bring Ollama
in. `PATH` is read once at startup, and that one value decides every poll of the run. A found
provider is polled like a switched-on one: its first reading creates or updates its subscription as
above, its `priceChanged` and `planChanged` travel, and a found Codex login is saved into seatswap's
store and may write a `providerSwitch`. With the card off there is no card to show an error, so a
found provider's error is printed once on standard error, never notified. A provider that is
neither on nor found is never read and no request is sent.

**The cards.** Two more usage sources are shown as cards under the account cards in the pop-up,
under a thin heading "Other providers". Each card has its own switch and is off by default; while
it is off, the pop-up shows no card for it, but a found provider is still polled (above) and its
reading recorded. The Sessions area is separate:
while *Read sessions* is on it reads Codex's rollout files whatever the Codex card says (§11.4).
Neither card signs in or out, refreshes the live login, or sends a request that generates text.
Their readings go into the history like a Claude account's, under an organization of their own.

**Codex.** The card reads `<codex folder>/auth.json`: the `codexHome` setting when set, else
`$CODEX_HOME`, else `~/.codex/auth.json`. The file is read at every provider poll; seatswap writes
it only when switching a saved login, and then through its atomic write, which keeps the file's mode
(Codex writes it `0600`; a file seatswap creates gets `0600`). Of it
seatswap uses `tokens.access_token`, `tokens.account_id` and, for a saved login, the whole JSON
object with every key in its order. `auth_mode` is looked at only to tell a login kept in the OS
keyring or an API key from a ChatGPT login. A missing file says so, and names the keyring too: `No
Codex login found in {path}. If Codex keeps its login in the system keyring, seatswap cannot read
it.` A file without tokens whose `auth_mode` is `chatgpt` means Codex keeps its login in the
keyring and says `Codex keeps its login in the system keyring. To switch Codex logins, set
cli_auth_credentials_store = "file" in Codex's config.toml and sign in again.`; an `auth_mode` of
`apikey` says `Codex is signed in with an API key. An API key is no subscription; seatswap neither
saves nor switches it.` Nothing in the keyring is read.

With the card on or a Codex login found, seatswap sends
`GET https://chatgpt.com/backend-api/wham/usage` with
`Authorization: Bearer <access token>`, `chatgpt-account-id: <account id>` and its own
`User-Agent: seatswap/<version>`. The token goes only to that host, the service that issued it, and
is never logged. The request follows no redirect: a 3xx answer is an error naming where it pointed,
and nothing is asked there.

The answer becomes the same limits a Claude account has:

- `rate_limit.primary_window` and `.secondary_window`: a `limit_window_seconds` of 18000 s is the
  5h session and 604800 s the week; `used_percent` is the percent and `reset_at` (seconds since the
  epoch) the reset time. `reset_after_seconds` is ignored.
- An `additional_rate_limits` entry is a per-model limit: its `limit_name` is the model. A 604800 s
  window is a scoped week; 18000 s and every other length keep their own name instead of becoming the
  account's session or week: `5h limit`, `24h limit`, `30min limit`, `45s limit`.
- The plan is `plan_type` with its first letter capitalised (`plus` → `Plus`); the account is the
  answer's `account_id` under the organization `codex`.
- One limit is marked active: the one with the highest percent, the first of equals. The answer has
  no severity, so the load colour comes from the percent alone (§7.3).

**Codex logins.** Every ChatGPT login seatswap sees in `auth.json` is kept in the secrets store
(keyring, or `0600` files, as Claude logins; on Windows always private files, §5) as the file's whole
JSON object, every key in its order,
under the first 32 hex digits of SHA-256 of `codex:<account id>`. When the live tokens differ from
the stored copy (Codex renewed them, or the person signed in with `codex login`), the stored copy is
replaced; a poll that changed nothing writes nothing. A new `account_id` gets a provider
subscription as soon as its copy is saved, so one ChatGPT account is one subscription (`OA`, then the
next free abbreviation). A login kept in the keyring and an API-key login are not saved, with the two
messages above. A save that fails (the secrets store refuses the login, the app lock times out, the
login has no refresh token) never hides the card: the live login's usage is still read and shown,
and its row in the pop-up says `This login is not saved: {error}`; a switch away from it then fails,
because a switch saves the live login first. When the live login changes outside seatswap (`codex
login` with another account, or a switch by another seatswap process), the next poll writes one
`providerSwitch` from the login it saw before, unless this host's last `providerSwitch` already
names the new login.

A switch to a saved login takes seatswap's own app lock first, then Codex's two locks in Codex's
order (`<codex folder>/auth.lock`, then `auth.json.lock`), with one 10 s deadline for both; it reads
the live file only after all of them, saves the live login, and then writes the target's stored
object to `auth.json`. It works in the pop-up, in `seatswap codex switch <abbr>` and in Settings →
Accounts. A switch writes the history event `{"type":"providerSwitch","from":…,"to":…,"by":"user"}`;
a `switch` event stays Claude-only, so no Claude reader (timelines, rates, switch counts) sees a
Codex switch. Older versions skip the new line with a warning. Codex reads `auth.json` only when a
session starts, so after a switch the pop-up says `Codex now uses O2. Codex sessions already
running keep OA until they restart.`; the session count it names is the previous login when it is
stored, else it says `their previous login`.

Inactive saved logins are polled for usage every 5 minutes with their own access token. A 401 renews
the login once at its issuer (`POST https://auth.openai.com/oauth/token` with the
`client_id` from the login's `tokens.oauth_provenance`, `grant_type=refresh_token`, the login's
`refresh_token` and scope `openid profile email`), stores the new `id_token`,
`access_token`, `refresh_token` and `last_refresh`, and asks once more. A stored login whose issuer
is not `https://auth.openai.com` is not renewed: the token is never sent to another issuer. A failed
renewal marks the login `expired` with `Sign in again with codex login.` and keeps the copy.
seatswap never renews the live login; Codex does. The renewal runs under seatswap's app lock, after
reading the live file again under it, so a login that became live during the round is not renewed.

Settings → Accounts' *Remove saved login* on a saved Codex login that is not live deletes seatswap's
stored copy only; Codex's `auth.json` and the subscription stay. While Codex runs on an API key or a
keyring login, no saved login is live, so every saved copy can be removed. Sign-in and sign-out stay Codex's
(`codex login`).

**Codex token lines.** With *Read sessions* on, seatswap writes `TokenRecord` lines for Codex
sessions from Codex's rollout files: the growth of `info.total_token_usage` between `token_count`
lines, per 5-minute bucket and the model of the turn (`turn_context.payload.model` in force at that
line; growth before the session's first `turn_context` belongs to the first one after it, and waits
until that one is written). `input = input_tokens − cached_input_tokens`, `cr = cached_input_tokens`, `out =
output_tokens` (reasoning included) and `cw = 0`, because OpenAI bills no cache write and Codex's
real lines do not carry the field (documented rule); `answers` is the number of `token_count` lines
that grew. What was recorded per Codex session is kept in the scan state (`codex.json`): its total,
its last time and its file. A total equal to the recorded one records nothing; a resumed session
records only its growth; an entry is pruned only when its newest file is gone and the scan no longer sees the session. A session not seen
within 24 h of its end is not recorded (documented rule). The Sessions area's Codex row keeps the
live total from the rollout file; closed Codex sessions are not rows of their own yet.

**Ollama.** The card asks the Ollama server at `ollamaUrl`, default `http://127.0.0.1:11434`:
`GET <ollamaUrl>/api/balance`, then `GET <ollamaUrl>/api/usage?range=24h`, both with seatswap's
`User-Agent` and no secret: the local server signs them with the machine's own key. A path on the
base URL is kept; one trailing `/` is dropped before the endpoint's path is appended. Both requests
follow no redirect, like the Codex one: a 3xx answer is an error naming where it pointed, and nothing
is asked there.

- A **new plan** (monthly credits) has `included.balance_usd`, `included.allowance_usd`,
  `included.period.until` and `purchased.balance_usd`. The card shows the credits left of the
  allowance (or left alone when there is no allowance), the reset date, the bought credits when above
  zero, and the spend of the last 24h from `totals.usage_usd` of `/api/usage`. When an allowance is
  known and above zero, the card's ring is the share of it used, and the reading is recorded as one
  limit of kind `monthly_credits` that resets at the period's end. Without an allowance there is no
  share and no such limit; the credits are still shown and the reading records no usage. A spend that
  cannot be read keeps its row as `not read: {reason}`; the balance still counts.
- An **old plan** (session and weekly limits) has `included.session` and `included.weekly`, each with
  `remaining_percent` and `resets_at`. They become a session and a week limit with
  `percent = 100 − remaining_percent`, like a Claude account; the card has no credits. Such a
  subscription has no month to value its price over, so its subscription value is unknown with the
  reason `no monthly allowance in Ollama's reading`, as for a new plan without an allowance.
- The account is named `ollama` under the organization `ollama`; the plan is empty.
- Every successful `/api/usage?range=24h` answer's `buckets` also become the history's Ollama hours (§5); the Activity page names in one note how much they counted beyond the known sessions (§11.4).

**Google.** Read only. Google's terms forbid third-party software that uses the Antigravity login,
and its FAQ does not support automated account rotation on consumer plans. So seatswap asks Google
nothing, reads no Google login, and neither saves nor switches one. The Antigravity CLI (`agy`) hands
its own numbers to a status line command; that command is `seatswap statusline`, set in
`~/.gemini/antigravity-cli/settings.json` as
`"statusLine": {"type": "command", "command": "\"<console seatswap program>\" statusline", "stack_with_default": true}`
(the path quoted as Claude Code's status line quotes it, §11.4, "The Status line area"). seatswap writes it only when the person
clicks **Connect agy** on the card (below), through the same writer as Claude Code's status line: the file
is backed up once, every other key keeps its value and place, and a missing file is created holding only
this key. A `statusLine` that runs another command is never replaced: Connect is refused with the
sentence `agy's status line already runs {command}; seatswap does not replace it. To connect, set it to
"seatswap statusline" in {path}.`
A payload whose `product` is `antigravity` is agy's: it touches neither the feed nor the recorded keys,
and the line printed for `agy` is the plan as `agy` names it and each limit,
`Google AI Pro · 5h 60% · week 6%` (`Google` without a plan, empty without an account). With an `email`
and a non-empty `quota` the numbers go to `<data>/google.json` under the feed's write rule. The percent
is `100 × (1 − remaining_fraction)`, the reset is `reset_time`. `gemini-5h` is the session and
`gemini-weekly` the week; `3p-5h` and `3p-weekly` are `5h Claude/GPT` and `week Claude/GPT`; any other
bucket keeps its id as its name. The four ids were seen in a real payload of agy 1.5.17.

An open `agy` session keeps saying the numbers of its own last answer while another one works on, so a
stored number is kept against an older one of the same email and plan: less use of the same window (the
resets within the jitter), no use while a used window still runs, or a window that ends earlier with no
more use or that had already ended. This costs two things, because seatswap has agy's word alone and
never asks Google: when Google takes use back inside a window, the number stays until that window has
ended, and when Google resets a limit early, the new window shows at its first use and the history has
no line for the one it cut short. An unused
limit's `reset_time` moves with the clock, so while nothing is used a different reset is no change of
the file.

**Before the first reading.** While `google.json` does not exist, the card follows `agy` itself, read
at every round and when the pop-up opens: with no folder `~/.gemini/antigravity-cli` there is no card;
installed with no `statusLine`, the card says `agy is installed but does not send its usage to seatswap
yet.` and offers **Connect agy**; with seatswap's status line it says `agy is connected. Its usage
arrives while agy runs; restart agy sessions that were open before.`; with another command it names
that command (`agy's status line runs {command}, and no reading has come from it yet. seatswap gets
agy's usage only when that command is seatswap statusline.`) and offers nothing. A settings file that
cannot be read is the card's error with its path. After a successful Connect the provider thread reads
Google at once; a refusal shows in the card until the next click. `agy` names no variable that moves its
folder, so seatswap looks only in the home folder; where `agy` keeps it on Windows and macOS was not
checked.

The card shows once that file exists: the email, the plan, the limits and the age of the reading. A
window whose reset has passed is no row but the note `{name} was reset since the last reading`; a bucket
that cannot be read or is disabled is the note `{id} not read: {reason}`. The numbers move only while
`agy` runs interactively (headless `agy -p` runs no status line), and the file holds one account, the
last one signed in, so a week can read too low. `Show a Google card` only hides the card.

A Google account is a provider subscription (`GO`, `#0d9488`), created at its first reading and edited
like an account's; its plan comes from `agy`. One reading of the file is recorded once, at the time
`agy` said it and as a status-line reading, so a drop in it is never an early reset: the session and the
week as they were then, whatever has ended since. A limit nobody has used is recorded without a reset:
it has no window that could end unused, like a Claude limit without one. The Claude/GPT limits stay on
the card. seatswap does
not record the work `agy` does: what a Google week was worth is unknown with that reason, never 0, and a
figure that sets recorded work against what was paid (the Subscriptions totals and total row, the
This-week card's paid sum and its share) leaves Google out.

**What the card shows.** One card per switched-on provider, Codex first. A Codex card has the
account card's rings and bars, the plan and a row per limit with its reset and its own name; a
per-model limit of another length is labelled `{model} · {name}`, for example
`gpt-6-astra · 5h limit`. Under the OpenAI card it lists every saved Codex login with its own usage,
the live one first; a click switches to it, with a pending row until the answer and then the note
above. An Ollama card on a new plan has the credit rows, in the page's one money rule (§11.5): whole
dollars from 10 on, two decimals below, so `212 USD of 300 USD` and `3.18 USD`. A first reading
without limits says `No usage limits in the reply.`; a provider that is switched off has no card. An
Ollama server that does not answer says `Ollama is not running.` as one quiet grey line on its card,
not as an error in red; the change from running to not running is reported once, no reading is
recorded, and the next answering poll resumes. A failed poll keeps the last numbers and their time,
greys them and shows the error text under them;
the age is printed once the reading is older than two minutes. The next poll tries again.

**A late answer.** The provider thread reads the settings again after every request. If the card was
switched off, or its folder or URL changed, while the request ran, the answer is neither recorded nor
shown. A `codexHome` or `ollamaUrl` changed by hand in `accounts.json` (not through a save) drops the
old numbers before the next round asks the new place, exactly as a save does.

**Every error text the card shows.** The provider reader's own messages are:

| Case | Text |
|---|---|
| no `auth.json` | `No Codex login found in {path}. If Codex keeps its login in the system keyring, seatswap cannot read it.` |
| a Codex login in the keyring | `Codex keeps its login in the system keyring. To switch Codex logins, set cli_auth_credentials_store = "file" in Codex's config.toml and sign in again.` |
| an API-key login | `Codex is signed in with an API key. An API key is no subscription; seatswap neither saves nor switches it.` |
| a switch to a login never saved | `No saved Codex login for {acc}. Sign in with codex login once while seatswap runs.` |
| HTTP 401 from Codex | `Codex's login has expired. Run codex once to renew it.` |
| a failed renewal | `Sign in again with codex login.` |
| a refused connection to Ollama | `Ollama is not running on {url}.` |
| HTTP 401 or 403 from Ollama's balance request | `Ollama on {url} refused the request (HTTP {status}). Is it signed in?` |
| an unusable setting | the setting's own text, for example `the Codex folder (codexHome or $CODEX_HOME) must be an absolute path, got codex` or `ollamaUrl must start with http:// or https://, got "127.0.0.1:11434"` |
| a file that cannot be read | `cannot read {path}: {reason}` |
| an answer of the wrong shape | `Unexpected answer: {reason}` |
| an HTTP or network failure | the API's own text: `the access token is not authorized` · `{service} limits how often seatswap may ask` · `server error 500` · `HTTP 404: {body}` · `cannot connect: {reason}` (Codex) · `network error: {reason}` · `cannot decode the reply: {reason}` |

For Ollama a refused connection becomes `Ollama is not running on {url}.` in reports and on standard
error; its card writes the quiet line `Ollama is not running.` instead. A balance answer of HTTP
401 or 403 becomes `Ollama on {url} refused the request (HTTP {status}). Is it signed in?` (seatswap
sends no token, so the card speaks of none); every other API failure keeps the API's text. A failed
24h usage request is not the card's error at all: its API text is the `not read:` row. The `Setting`
row is what a hand-edited `codexHome` or `ollamaUrl` produces: the settings page refuses such a value
before it is saved, and the reader checks it again before reading or asking anything.

**History.** A reading that carries a usage is recorded like an account's: a `Sample` with source
`Poll` under `AccountRef {acc, org}`, with `org` `codex` or `ollama`; a window that ends and a 100 %
crossing become a `WindowRecord` and a `Blocked`/`Unblocked` event like a Claude account's. A Claude
organization id is a 36-character UUID, so those names cannot clash. Every reader that serves Claude's
accounts uses `read_claude` (and `finished_claude_windows`), which leaves those records out: auto-burn,
the status line's next account and the Forecast stay Anthropic-only, and a provider reading never makes
a switch. The Activity page is the one statistics page that reads Ollama's own hourly lines; the
Statistics pages read the provider subscriptions' samples, windows and token lines through the
company rules in §11.4 (working and blocked hours, subscription value) and not as a Claude account's.
Codex's rollout files give the Sessions area its Codex rows (§11.4), separately from these readings.
`read` stays for the recorder, the export and the import: an export's windows and daily token rows are
raw history, so a provider's finished window can appear in an export, while its samples do not,
because the export has no sample rows. An import copies a host folder as it is, provider lines
included. A history folder that could not be opened at start is already the snapshot's error and was
notified there; the provider thread records nothing and reports nothing a second time.

**Settings.** The five settings are in the settings table (§10) and in Settings → Providers:
`codexCard` and `ollamaCard` default off, `googleCard` on (its card needs an account first), `codexHome` is empty unless set, and `ollamaUrl` defaults to
`http://127.0.0.1:11434`. `codexHome` must be an absolute folder when set. `ollamaUrl` must start with
`http://` or `https://` and name a host: a DNS name (dot-separated labels of letters, digits and inner
hyphens), an IPv4 address or a bracketed IPv6 address. A whitespace or control character, user info
(`user:pass@`, refused without quoting the URL), a query string, a fragment and an empty path segment
are refused, while a port, a path, an IPv6 host and one trailing `/` are accepted.

**What is not done.** No sign-in or sign-out (of OpenAI or Ollama), no request that generates text,
no keyring read (a Codex login kept in the keyring is only named on the card), and no session rows
for closed Codex sessions. Auto-burn stays Anthropic-only.

---

## 8. Tray icon

### 8.1 What it shows

```
 22 px icon (Linux panel)     what each part is

    ╭──────╮                   ring   = binding limit of the active account, clockwise
   ╱  ▓▓▓▓ ╲                            from 12 o'clock, coloured per §7.3
  │   AC    │                  letters = the active account's abbreviation, white or black,
   ╲       ╱                             whichever contrasts best with the account colour
    ╰──────╯                   disc   = account colour
                               (lock badge bottom right at 100 %, grey ring while unknown)
```

- **Letters or percent.** The setting `iconText` puts the ring's percent in the disc instead of the letters: the whole percent of the limit `iconLimit` names, `0` to `99`, with 100 % (or more) shown as `99` so the disc never holds three digits; the closed ring and the lock badge show a full limit. While that limit has no reading yet, the disc keeps the letters beside the grey ring.

- The icon is drawn at run time with `tiny-skia` and a bundled font, at each size the platform asks for (16, 20, 22, 24 and 32 px, and @2x/@3x). It is redrawn when the usage or the account changes. On Cinnamon a new icon every 0.5 s showed at once, at 1–2 ms per update *(tested)*.
- **Text next to the icon, where the platform shows it:**
  - macOS: the menu bar title.
  - Linux: the AppIndicator label. **Cinnamon does not show it** *(tested, Cinnamon 6.6.9)*: the app publishes the label, but Cinnamon's tray bridge (`xapp-sn-watcher`) hands an empty one to the panel. There the ring carries the number, and the tooltip once the app publishes one (below).
  - The text reads `97%`. A setting `trayText` offers `percent`, `percent+reset` (`97% · 1h 12min`) or `off`.
- Tooltip: "AC · Acme — session 97 % (resets 14:40) · week 48 %".
- **High-contrast / monochrome option:** macOS "template" style, or a plain white icon for dark panels. The ring stays, the letters become cut-outs.

### 8.2 Abbreviations

- **Up to 2 characters**, so the label stays legible at 16 px (the Windows tray at 100 % scaling). Three characters don't fit in a 16–22 px disc at a readable size. Allowed: letters A–Z and digits, stored in upper case. One character is drawn larger.
- Default: the first letters of the first two words of the label ("Acme Team" → `AT`), or the first two letters of a one-word label ("Acme" → `AC`, "Private" → `PR`). The examples in this spec show `P` for the private account; that one was shortened by hand in the editor (§11.4).
- Two accounts can't share an abbreviation (checked when editing).
- **Fallback at 16 px:** if a measured glyph width doesn't fit, the font becomes condensed, never smaller than 9 px. The colour still tells the accounts apart at a glance.

### 8.3 Click behaviour

| Platform | Left click | Right click |
|---|---|---|
| Windows | opens the pop-up | menu |
| macOS | opens the pop-up | menu |
| Linux (StatusNotifierItem, via tray-icon's `ksni` backend) | opens the pop-up (`ItemIsMenu` is false, so the host calls `Activate`); confirmed on Cinnamon | menu |

**Linux menu:**
```
  ● AC  Acme                97 % · 5h   ✓
  ● P   Private             12 % · 5h
  ───────────────────────────────────────
  Open panel…                       ⌃⌥C
  Add account…
  Sessions…
  Statistics…
  Settings…
  Quit seatswap
```

- Clicking an account switches to it.
- The usage text in the menu updates live.
- "Open panel…" opens the pop-up. "Sessions…", "Statistics…" and "Settings…" open the large window at that area.
- An optional global shortcut (default Ctrl+Alt+C, configurable) opens the pop-up on every platform.
  - **Linux v1:** the app registers no shortcut itself. The person binds `seatswap panel` to a key in the desktop's keyboard settings; that command asks the running tray app to open its pop-up. It works the same on X11 and Wayland. A built-in shortcut would need Tauri's global-shortcut plugin, which is not in the project yet (and cannot grab keys on Wayland).

### 8.4 The app icon

The tray icon above is live: it changes with the account and its usage. The app's own icon is fixed:
the *Swap S*, a green rounded square (`#22c55e`) holding a dark S whose two ends are arrow heads. It
is 256 px (`crates/app/icons/icon.png`, drawn from `icon.svg`); `icon.ico` carries 16, 24, 32, 48,
64 and 256 px for Windows. It is the launcher's icon, the Windows `.exe`'s, the icon of every window
(taskbar and Alt+Tab) and of every notification.

**The Linux menu entry and its icon (D-1).** At every start the tray app writes
`$XDG_DATA_HOME/applications/seatswap.desktop` (else under `~/.local/share`) and the icon
`$XDG_DATA_HOME/icons/hicolor/256x256/apps/seatswap.png` beside it, when they are missing or differ.
The entry has a line `X-seatswap-managed=true`, starts the same command and flags as the autostart
entry, and names the icon's full path; a path or argument with a line break is refused. A symbolic
link at either path is left alone, and so is an entry without that managed line; each such path
gets one line on standard error: `seatswap: {path} was not made by seatswap; it is left as it is.`
The icon is seatswap's own file and is still written when the entry is left alone. A
write failure is named on standard error and never stops the start. Nothing is written on Windows.

---

## 9. Platform support

### 9.1 Matrix

| Platform | Tray mechanism | Expected | Notes |
|---|---|---|---|
| **Linux Mint Cinnamon (X11)** — reference | AppIndicator via XApp | full, without the label | icon updates tested; no label (§8.1); the pop-up can be placed at the mouse position |
| KDE Plasma (X11/Wayland) | StatusNotifierItem | full | label goes into the tooltip |
| GNOME (Ubuntu) | AppIndicator extension, preinstalled | full | |
| GNOME (Fedora, Debian, vanilla) | needs the "AppIndicator and KStatusNotifierItem Support" extension | menu and pop-up once installed | first run detects a missing tray and says how to add it; the CLI works either way |
| XFCE, MATE, Budgie, LXQt | SNI / AppIndicator panel plugin | full | |
| Sway, Hyprland, other wlroots | Waybar / other bar with an SNI tray | full | also `seatswap status --waybar` for a native bar module |
| i3 / bars without SNI | none | CLI + `--waybar`/`--i3blocks` output | |
| **macOS 12+** | NSStatusItem | full | Keychain prompt on first access ("Always Allow") |
| **Windows 10/11** | notification area | full | tray icons hide in the overflow by default; first run tells you to pin it |

### 9.2 Placing the pop-up

- **Windows and macOS:** anchored to the icon, using the tray rectangle that Tauri reports.
- **Linux X11:** at the mouse position, kept inside the monitor that has the mouse, and flipped to sit above or below depending on whether the panel is at the top or bottom.
- **Linux Wayland:** windows can't place themselves, so the compositor decides. The pop-up is a centred, borderless card, like a launcher. It closes on Esc or when it loses focus.

### 9.3 Optional native front ends (later)

Each one is only a view onto `seatswap status --json` and `seatswap use`:
- Cinnamon applet (JavaScript, in the Spices format).
- GNOME Shell extension.
- KDE Plasma widget.

They give real click handling and exact placement on those desktops. They're not needed for v1.

### 9.4 Packaging

- **Linux:** `.deb`, `.rpm`, AppImage, and an AUR `PKGBUILD`. Autostart through `~/.config/autostart/seatswap.desktop`.
- **macOS:** signed `.dmg` plus a Homebrew cask. Login item via `SMAppService`.
- **Windows:** two programs from one code base: `seatswap.exe` (terminal, CLI) and `seatswap-tray.exe` (the same program built without a console window, for autostart and the Start menu). The first release is a zip with both; an MSI or NSIS installer and a winget manifest come later. Autostart through the value `seatswap` under `HKEY_CURRENT_USER\Software\Microsoft\Windows\CurrentVersion\Run`, which starts `seatswap-tray.exe` with the same `--data-dir` / `--claude-config-dir` flags.
- **CLI only:** `cargo install seatswap --no-default-features`.

---

## 10. Settings

| Setting | Default | |
|---|---|---|
| `abbr`, `label`, `color`, `autoBurn` per account | from the account | **shared**. `autoBurn`: auto-burn may use this account, §7.4.4 |
| `trayText` | `percent` | `percent` · `percent+reset` · `off` |
| `iconLimit` | `binding` | which limit the ring shows: `binding` · `session` · `week` |
| `iconText` | `letters` | what the disc shows: `letters` (the account's abbreviation) · `percent` (the ring's percent, 100 as `99`), §8.1 |
| `pollActive` / `pollInactive` | 120 s / 900 s | |
| `autoBurn` | off | auto-burn switches by itself, §7.4 |
| `suggestSwitches` | on | when auto-burn is off, say what it would do, §7.4 |
| `statuslineFeed` | on | **shared**. `seatswap statusline` writes the live numbers to `feed.json`, §7.6 |
| `statusline` | "My current line" | **shared**. The status line layout: `left`, `right`, `margin`, §7.6 |
| `statuslineInstall` | none | `null`, or `{ "installed": <value>, "previous": <value> }`: the exact `statusLine` seatswap last wrote in Claude Code's `settings.json` and the exact value the file had before its first install. `previous` absent means the file had no key; a present `null` is the value `null`. Written by *Use in Claude Code* and cleared by *Restore previous* (§11.4) |
| `statuslineRecordUntil` | none | `null` or a time: while it is in the future, `seatswap statusline` records the payload's key paths and JSON types to `<data folder>/statusline-keys.json`. *Record for a minute* sets it to one minute from now (1 to 60 minutes are accepted), §11.4 |
| `notifyOnSwitch` | on | **shared** |
| `mirrorLive` | on | §6.4 |
| `refreshInactive` | on | refresh the logins of inactive accounts (§6.3). Off: inactive accounts show their last known usage with its age |
| `hotkey` | Ctrl+Alt+C | Linux v1: not used; bind `seatswap panel` in the desktop instead (§8.3) |
| `startAtLogin` | on | Linux: `~/.config/autostart/seatswap.desktop`; Windows: the `Run` key value `seatswap` → `seatswap-tray.exe` (§9.4) |
| `theme` | system | system · light · dark |
| `claudeConfigDir` | `$CLAUDE_CONFIG_DIR` or `~/.claude` | |
| `historyDir` | `null` (`<data folder>/history`) | text, an absolute folder; point it at a synced folder to share the history with other hosts |
| `hostName` | `null` (the machine's host name) | text, 1–63 characters of `A–Z a–z 0–9 . _ -` that must start with a letter or a digit, stored in lower case; the name this host writes its history under |
| `readSessions` | on | read the session files and transcripts of Claude Code, DeepSeek Harness and flash agents, and Codex's rollout files and thread names (`session_index.jsonl`): model, busy or idle, waits, tokens per folder, agents and their thinking. The Sessions area needs it; turning it on takes effect after a restart. A session's title is read from its `ai-title` lines and stored in the history; message text and worker task texts are never stored |
| `notifyUsableAgain` | on | **shared**. The "usable again" notification (§7.5) |
| `notifyWaiting` | on | **shared**. The "waiting for you" notification (§7.5); a click opens the Sessions area at that session |
| `notifyWaitingHosts` | `thisHost` | **shared**. `thisHost` · `allHosts`: whose waiting sessions notify. Other hosts report through the shared history folder, so their notifications come with the delay of the sync (§4.1, §11.4) |
| `notifyLoginEnds` | on | **shared**. The login warnings at 5 days, 1 day and when a login ended (§7.5). `bool`; Settings → Notifications, "A login ends" |
| `currency` | none | **shared**. `null` or three capital letters (`"EUR"`); `Option<String>`; the currency every amount is shown in. Until it is set, amounts in money stay missing (§11.5). Settings → General, section Money, "Currency" |
| `lookUpPrices` | on | `bool`; whether seatswap may ask models.dev for API prices and Frankfurter for the dollar rate (§11.5). Off, neither host is contacted and `prices --refresh` refuses; stored values stay and API values are shown in USD. Settings → General, Money, "Look up prices and the dollar rate" |
| `price` per account or provider subscription | none | **shared** (as `priceCurrency` with it). `null` or a number above 0: what the subscription costs per month, in `priceCurrency`; `Option<f64>`. Without one the account is left out of every sum in money and the page says so (§11.5). Settings → Accounts, column "Price / month", for a provider subscription too |
| `priceCurrency` per account | none | `null` or three capital letters: the `currency` in effect when `price` was entered. A price counts only in that currency; a price entered before a currency was set has `null` and does not count (§11.5). Settings → Accounts, the price cell |
| `plan` per account | from the login | read-only `Option<String>`: the identity's `planDisplayName`, else Claude's current name for the login's `subscriptionType` and `rateLimitTier` (Claude renamed its plans: `max` with `_5x` is "Max", `max` with `_20x` is "Ultra", `team` with `_5x` is "Team Premium"), else the capitalised `subscriptionType` with its level from `rateLimitTier` ("Enterprise 20x"; a tier seatswap cannot read as a multiplier in parentheses, "Team (team_premium)"). Team Standard seats are not named yet: their tier value has not been seen. A stored label of seatswap 1.2.0 ("Team 5x", "Max 5x", "Max 20x") that gives way to the current name is saved, but it is no plan change: the history gets no `planChanged`. Shown in the pop-up, Settings → Accounts and `seatswap list` |
| `agentsRead` | on | agents may run `list`, `status`, `burn plan`, `burn simulate`, `prices` and `history export` (§6.6, §7.7) |
| `agentsSwitch` | off | agents may run `use` and `next` (§6.6); an agent's `next` does nothing while auto-burn is on (§7.8) |
| `codexCard` | off | `bool`; show the OpenAI (Codex) card in the pop-up, with every saved Codex login under it. While off, there is no card and its logins are not listed, but a Codex login file found in the Codex folder is still polled and saved (§7.9). The Sessions area still reads Codex's rollout files while *Read sessions* is on. Settings → Providers, "Show a Codex card" |
| `codexHome` | none | `null` or an absolute folder: where Codex's `auth.json` and `sessions/` are; `null` means `$CODEX_HOME`, else `~/.codex`. A relative folder is refused. Settings → Providers, "Codex folder" (§7.9) |
| `ollamaCard` | off | `bool`; show a read-only Ollama card. While off, there is no card, but Ollama is still polled when the `ollama` program is found or this host stores an Ollama subscription (§7.9). Settings → Providers, "Show an Ollama card" |
| `ollamaUrl` | `"http://127.0.0.1:11434"` | the Ollama server the card asks; must start with `http://` or `https://` and name a host: a DNS name, an IPv4 address or a bracketed IPv6 address. Refused: a whitespace or control character, user info (without quoting the URL), no such scheme, a host that is not one of those, a query string, a fragment, an empty path segment. A port, a path, an IPv6 host and one trailing `/` are accepted. Settings → Providers, "Ollama server" (§7.9) |
| `googleCard` | on | `bool`; show a Google card while `agy` is installed: Connect until it hands `seatswap statusline` an account, then its numbers (§7.9). Off only hides the card; the numbers are still recorded. Settings → Providers, "Show a Google card" |

Every setting above is edited in the large window's Settings area (§11.4); the last column names the
group and the row. The pop-up has no settings view any more (§11.3). Settings → General's first line
is `seatswap <version>`, the version of the running program.

A setting marked **shared** travels to the other hosts through the history folder and takes the
newest value any host saved (§5, "Shared between hosts"); `store::SHARED_SETTINGS` is the one list of
the shared settings. Every other setting stays on its host. The per-account values are shared by the
account's `acc` and `org`, never with its login.

The former `suggestAt` and `suggestTargetBelow` (the fixed 90 % / 50 % suggestion) are gone. An
older `accounts.json` that still has them loads: unknown keys are ignored, and the two settings
default to auto-burn off and suggestions on.

An older `accounts.json` that still has `usdRate` loads and ignores it silently, and an older
`prices.json` with own prices or an `overrides` key does the same (§11.5); the next save leaves them
out.

---

## 11. The pop-up ("panel")

### 11.1 Look

The target is somewhere between the macOS Control Centre and Raycast: one floating card, calm until something needs attention.

- **Card:** 380 px wide, height fitted to the content, 16 px corner radius, 1 px hairline border.
  - Translucent blur where the platform has it: vibrancy on macOS, Mica/Acrylic on Windows 11 (the `window-vibrancy` crate).
  - On Linux, a solid surface with 96 % opacity.
- **Themes:** dark by default when the system is dark; light follows the system.
  - Dark: background `#0f1115`, cards `#171a21`, text `#e7e9ee`, muted text `#8b93a7`.
  - Light: background `#f6f7f9`, cards `#ffffff`, text `#0f1115`.
- **Type:** Inter, or the system UI font as a fallback. Numbers use tabular figures so percentages don't jitter.
- **Gauges:** two concentric rings per account. The outer ring is the week, the inner the 5h session; the week's percent is the large figure in the middle. The bars beside the gauge follow the same order: week, then session, then the per-model limits. On a plan without a session limit the inner ring is left out (no grey track) and the week's figure sits centred.
  - Rings fill with an ease-out animation (400 ms) when the pop-up opens and when values change.
  - Colours follow §7.3, drawn as a soft gradient along the arc, with a faint glow on the red state.
  - Clicking the gauge of an account that is not in use switches to it, like its Switch button. The pointer becomes a hand over it.
- **Account colour:** a 3 px accent bar on the card's left edge and the abbreviation badge.
- **Order:** the active account at the bottom. Above it the other accounts stand by their weekly reset, the week that expires next on top; an account without a weekly reset follows them, and equal resets keep the list order. The provider cards keep their own section.
- **Active account:** the card is raised (slightly lighter), with a subtle ring in the account colour and an "Active" pill.
- **Card header:** the badge, the account's name, and after it, muted and small, the time left until the weekly reset in the status line's compact form (`3d 2h`, `14h 3min`, `35min`; tooltip "until the weekly reset"). A logged-out account and one without a weekly reset show none.
- **Closing:** Esc, the × in the header, a click anywhere else (the pop-up loses focus), or a second left click on the tray icon. The pop-up must actually get the keyboard focus when it opens, or none of the first three work (on Cinnamon it did not at first, §17).
- **Motion:** after a switch, the cards trade places with a 250 ms spring animation. The ticking reset countdowns ("1h 12min") update every minute without flicker. All animation is off when the system asks for reduced motion.
- **Keyboard:** ↑/↓ moves between accounts, Enter switches, 1–9 switch directly, `,` opens the settings in the large window, Esc closes. The cursor (a blue ring) appears only with the first ↑/↓, starting from the account in use, and is gone again each time the pop-up opens; until then Enter does nothing. Clicking a card does not move it.

**Sessions line.** In the footer next to "+ Add account" the pop-up reads `9 sessions · 2 busy · see the plan` (the mock-up shows it shorter: `3 sessions · 1 busy`), and while someone waits ` · 1 waiting for you` in the warning colour before the plan link. With `readSessions` off there is no line. When the session files cannot be read, their reason stands there muted instead; a newer Claude Code adds a small ⚠ whose tooltip is the warning text. The numbers do not depend on the pop-up being open. `9 sessions` opens the Sessions area; `1 waiting for you` opens Sessions at that session (`sessions?<id>`), and a waiting id the window cannot parse is refused with an error; `see the plan` opens Statistics at Forecast; it shows only while auto-burn has a plan. With no stored account the auto-burn line is `Save an account to start.`

**The auto-burn line.** The line under the cards names every account it mentions as a chip in the account's colour, so a sentence like `On AC until its 5h window is full, about 1h 30min. Then LB.` shows `AC` and `LB` as chips. Where plain text is needed — the tray menu, a notification, the CLI — the same line is printed with each account's abbreviation and no chips.

### 11.2 Mock-up

```
╭──────────────────────────────────────────────────────╮
│  seatswap                           ⟳ 12s ago  ⚙  ✕ │
│                                                      │
│ ▌╭────╮  P   Private                       [ Switch ]│
│ ▌│ 31 │  …@example.com · Ultra                       │
│ ▌│    │  Week      ██████▏                31 %  Thu  │
│ ▌╰────╯  Opus      ██▍                    12 %       │
│                                                      │
│ ▌╭────╮  AC  Acme ✎                       ● Active   │
│ ▌│ 48 │  someone@… · Team                            │
│ ▌│ 97 │  Week      █████████▋             48 %  Sun  │
│ ▌╰────╯  Session   ████████████████▉   97 % 1h 12min │
│ ▌        Fable     ████████▋              43 %       │
│                                                      │
│  ⚠ AC is almost out. P has room — switch?   [Switch] │
│                                                      │
│  + Add account                   3 sessions · 1 busy │
╰──────────────────────────────────────────────────────╯
```

### 11.3 Account editor and settings have moved

The pop-up has no account editor and no settings view any more. Its gear, and `,` on the keyboard,
open the large window at its Settings area. A click on an account's badge, or on its ✎, opens the
large window at Accounts with that account's editor open. The editor and every setting row are in
§11.4; `docs/implementation.md` has the page and command contracts.

---

## 11.4 The large window

seatswap has a second window next to the pop-up: label `main`, title "seatswap", 1280 × 800 at
start, resizable, decorated, in the taskbar and not always on top. It is one more Tauri webview
window with its own page (`window.html`); both windows can exist at the same time and both receive
the app's `snapshot`. Its content keeps a 24 px margin on every side.

**How it is opened.**
- The pop-up's gear (or `,`): the Settings area.
- An account's badge or ✎ in the pop-up: Accounts, with that account's editor open
  (`settings/accounts?<account id>`).
- The tray menu: "Sessions…" opens Sessions, "Statistics…" opens Statistics, "Settings…" opens
  Settings.
- `seatswap window [AREA]` asks the running tray app; without an area it opens `sessions`.
  `seatswap panel settings` sends the same line as `seatswap window settings`.
- A page's `open_window` command, which takes the same target.

A target is an area, optionally followed by `/<sub>` and, for the sessions area only, optionally by
`?<item>`: `settings`, `settings/accounts`, `settings/accounts?<account id>`, `sessions?<session id>`,
`statistics/forecast`. The areas are `sessions`, `statistics`, `statusline` and `settings`; a
sub-area and an item may hold only `a-z`, `0-9` and `-`, and there is at most one `/`. An item after
`?` without a `/` is refused for every area except sessions. Anything else is refused with the
reason, which `seatswap window` prints as a usage error. A newly created window loads `window.html#<target>`; an
already open one is shown, un-minimized, focused and sent `open-area`. The pop-up keeps its own
`navigate` event, because an event reaches every window.

**The areas.** Four tabs at the top left, in this order: Sessions · Statistics · Status line ·
Settings. An empty address opens Sessions. Each area has a page; the Status line area is the
builder below. Under `statistics` the sub-area is one of the seven page
names (`statistics/forecast`); without one the area keeps the page last chosen. The area's title and
its subtitle stand above the card, and the snapshot's error is a banner above both. The theme
follows the `theme` setting and the system, as in the pop-up.

**Settings.** Eight groups on the left, the chosen group's card on the right:
General · Accounts · Auto-burn · Notifications · Projects · Agents · Providers · History and hosts.
The address names the group, with an alias accepted (`gen`, `acc`, `autoburn`/`burn`, `noti`, `proj`,
`prov`, `hist`), and an optional item that opens a row's editor. A click on a group writes the new
address, so Back returns to the previous group. Settings has no second title: the chosen group's card
follows the tabs directly.

Every control saves at once; there is no Save button. A refused save shows its error under the row
and puts the control back to the saved value. Saves run one after another, each built from the
settings the one before left, so a quick second change never carries a stale copy; while a save is
pending, an incoming snapshot does not overwrite the page's own copy. The left group menu stays on
screen while only the content pane scrolls. A redraw of the open group keeps the content pane at the
same scroll position, and another group opens at the top. The address `settings/accounts?<id>` opens
Accounts, scrolls that account's or subscription's row into view and focuses its price field; an id
no row has says `No account {id}.` and focuses nothing. A text field's focus frame is drawn outside
its box, and
the table or label cell that holds it shows the whole frame instead of cutting it; the money rows
are drawn again only when their view changed, so the checked time and the rate do not flicker while
a snapshot ticks.

- **General.**
  - *Version*: the page's first line is `seatswap <version>`, the version of the running program.
  - *Money*: the currency; "Look up prices and the dollar rate"; "1 US dollar is" with the rate in
    effect and below it the source and the reason it is missing, one reason per row — `ECB rate of
    Fri 13 Nov · European Central Bank via Frankfurter · fetched today 10:10`, `fetched: not
    recorded` for a point saved by 1.1.0, `no rate yet: API values are shown in USD` when a currency
    is set and no rate is stored, `look-ups are off: API values are shown in USD`, or `USD itself`;
    the value itself is `—` when it is missing, never a sentence in its place. A failed rate
    look-up's text is under that row. The API prices row says how many models have a price and
    "API prices from models.dev" as one sentence, `checked Fri 13 Nov 11:11` (today: `checked
    11:11`), what the last look-up changed with its day, and how many models the models.dev list
    left out and which. The price list itself is under Providers.
  - *Look*: theme, tray text, which limit the tray ring shows.
  - *Start and polling*: start at login; how often the active account and the inactive accounts are
    read.
  - *Claude Code*: the config folder; read the session files; copy the live login at once; refresh
    inactive logins.
- **Accounts.** One row per account: the chip (abbreviation and colour), the label (editable), the
  account's e-mail, its plan, its price per month with the currency it was entered in, "Auto-burn may
  use it", its organization, and when its login ends. Under the Claude accounts stand the provider
  subscriptions, one row per OpenAI or Ollama login, with the same chip, label, company, plan and
  price. OpenAI's plan is read-only from the provider; Ollama's plan is typed in a one-line field and
  shows `—` until then. A price entered in another currency than the
  current one is marked "in EUR: enter it again for CHF", and entering the same amount again saves it
  in the current currency. A click on the chip opens the editor: one or two letters,
  ten colour swatches and a hex field, a live preview of the tray icon at 16, 22 and 32 px, and
  Remove with a confirmation that says the live login is not touched. Account and plan come from
  the login and are read-only. The organization is chosen from the organizations of §11.6 or
  created there. A saved Codex login that is not the live one has *Remove saved login*, which deletes
  seatswap's stored copy only and says that Codex's own file and the subscription stay.
- **Auto-burn.** The switch, "Suggest switches", and the read-only lines of the auto-burn view: how
  early it switches, what was measured, what it would do now, what a switch by you costs, and
  whether a session would lose its thinking. There is nothing to tune here.
- **Notifications.** "A session is waiting for you" and whether this host's or all hosts' waiting
  sessions notify; "An account is usable again"; "All accounts are full" (always on); "Every
  automatic switch"; "A switch by an agent" (always on); a link to the suggested switches under
  Auto-burn; "A login ends", the 5-day, 1-day and ended warnings.
- **Projects.** §11.6. One heading per organization, its projects below it; `No organization` comes
  last. A project's row shows its name, its tokens and sessions of all time, its organization in
  words and its menus *Move to* and *Merge into*; an organization's row shows its source, tokens and
  sessions, *Merge into* and *Split off* (when merged), and *Remove* except for one that comes from
  a repository owner or is an owner organization of this or another host, which have none and say
  why. *Move to* lists the other organizations and `No organization` and applies at once. *Merge
  into* and *Split off* ask first. `+ Add organization` stands under the list. The *Join projects*
  row keeps its text and its button apart at every width.
- **Agents.** "Agents may read usage" and "Agents may switch accounts", with the text of §6.6; the
  skill's state, an install and a remove button; the last 20 calls by agents, newest first, with
  their folder and whether they were allowed.
- **Providers.** "Show a Codex card" and "Show an Ollama card", both off by default, "Codex folder"
  (empty: `$CODEX_HOME` or `~/.codex`) and "Ollama server" (default
  `http://127.0.0.1:11434`). The section's two notes say that seatswap reads Codex's login file,
  asks chatgpt.com for the usage every 5 minutes, and writes Codex's `auth.json` only when a saved
  login is switched; and that the Ollama card asks the server on this machine for the cloud balance
  every 5 minutes (§7.9). Below them
  stands "API prices from models.dev · checked 11:11", models.dev a link, and the price list: one
  read-only row per model with its four prices, a model whose price depends on the prompt size
  marked and using its base price, and a Show/Hide control. Nothing in the list can be edited or
  added.
- **History and hosts.** This host's name and its history folder; every host folder with its last
  record, its first record and its size; export and import. A note says what the folder shares with
  the other hosts: each account's label, abbreviation, colour, price and whether auto-burn may use
  it, and the currency, the notifications and the status line; the last edit wins, and logins are
  never shared (§5). Another host's edit shows on the open page when it arrives, without a restart. A changed host name or folder takes
  effect after a restart, and the row says so when the saved value differs from the one in effect.
  Export writes CSV or JSON to a file the person names (an existing file is refused); import copies
  another history folder's host folders in by hand and never writes this host's own folder or a
  same-named folder of another machine. A host folder that is already here is merged the same way as
  a new history folder (§5): a missing file is copied, a month file gets its missing lines, and nothing
  is removed, so two copies that both changed keep every line; a copied line that is not valid JSON is
  named. While the saved settings name no history folder, the section
  reads Dropbox's own `info.json` (macOS and Linux `~/.dropbox/info.json`; Windows
  `%APPDATA%\Dropbox\info.json`, then `%LOCALAPPDATA%\Dropbox\info.json`: `personal.path`, else
  `business.path`) and suggests `Dropbox found: <Dropbox folder>/seatswap` with a Use button that
  saves the folder through the same `save_setting` command the field uses, so there is one way to set
  it. The folder is only suggested: nothing is created or written there until Use is pressed. A
  missing `info.json` is no suggestion; a file that cannot be read, a section of another kind, or a
  path that is not a string or is empty is an error naming the file, shown under the host list
  together with the history error if there is one.

### The Sessions area

The first area, and what an empty address opens: every running session on this host and, through the
shared history folder, on the user's other hosts, live, with its agents, tokens and their two values.
It reads Claude Code's session files and transcripts, DeepSeek Harness's session files and
flash-agents' state, and Codex's rollout files. All of it is read while `readSessions` is on (default
on), whatever the Codex card in the pop-up says; turning `readSessions` on takes effect after a
restart, and with it off the area says so. Every row names its provider: `Claude Code`, `Codex` or
`DeepSeek Harness`.

**Numbers on top.**
- Sessions running, and on how many hosts; busy, with how many are waiting for you.
- Agents running, and how many completed today.
- The total of exactly the sessions in the view: their tokens, subscription value and API value; the
  last hour's tokens with the last hour's subscription and API value.
- The sessions whose thinking is tied to the account (§7.4.2), and how many of them are busy.
- The account in use, with its week; its name wraps inside its box instead of being cut.

**The tree.** Two orders, switched at the top and remembered: by project → session (each session row
shows its model, so there is no provider or model row under a project; 1.4.5), or by provider → model → session with the project after the session's name. A session with
running agents is open, and closes again when they finish, unless the user folded or unfolded it by
hand; that choice holds while the window is open, also when the user visits another area. Every
top-level
group row stands on the mock-up's highlighted background and shows how many running and closed
sessions it holds, how many are busy, how many agents are running, and the totals of its tokens and
their two values; every total counts exactly the sessions in the view — a grey row that has left is
shown but no longer counted. Every total row fills the same columns as a session row — Agents, Tokens, Last hour,
subscription value and API value — and leaves Model and Status empty: in By project a project row,
in By model a provider and its model rows. The model rows under a
provider do not repeat it; a model total row carries the `tied` bubble, and a total's Agents shows
`—` when every row it sums has no agent data. A session's row has:

| Column | Content |
|---|---|
| Name | Claude Code's own title of the session (the one VS Code and `/resume` show); a DeepSeek Harness row likewise by the title of its last `session/title` event, except a flash job's row, whose title is the job's label (below); a Codex row by its thread name, the newest line for its id in Codex's `session_index.jsonl`; the session's label (`atlas-a6`) when it has no title. A long title is cut with `…`; the hover text is its whole text with the label on the next line. Opens to its agents |
| Model | the model of its last answer, or `no answers yet`, with the `tied` bubble when its thinking is tied to the account |
| Status | busy · idle · waiting for you, with how long; for a closed session, when it closed |
| Agents | running · completed, the running number in the busy colour (grey at zero), completed always grey; `—` when a session has no agent data |
| Tokens | output tokens of the session and its agents since it started |
| Last hour | output tokens in the last 60 minutes, empty when there were none and `—` with the reason when a total went down; the *Last hour* KPI adds both values, the *Last hour* card the subscription value |
| Subscription value | the value of those tokens in the account's plan (§11.5); a model outside the Claude plan is a known 0 |
| API value | the same tokens at API prices (§11.5) |
| Host | where it runs; for another host, the age of its last report |

A total's last hour is `—` when any row it sums has an unknown one. The last hour is kept per minute
and model, so its tokens are priced like the rest, and a closed session's token records are
priced with it.

A running session is valued exactly like a closed one: from its token lines in the history, one per
5-minute bucket and model, each priced at its bucket with the account that was live then and that week's share
(§11.5). Lines this pass could not write, and the DeepSeek Harness lines written after the rows, count
once as pending until they are recorded. A linked flash job's and its workers' tokens are recorded
under the Claude Code session, so they count once in its row; the job and its workers shown under the
row carry their own numbers and add nothing more. While the history cannot be read, a running row's
tokens are its transcript totals and its values are unknown with the reason.

**Token counts.** Every token count in the window is written the same way: three significant digits
with the unit and no space — 1.54M, 23.4M, 683K, 1.20K, 1.23B — and below 1,000 the plain number (845,
7; 0 for none). Rounding that carries over moves to the next unit (999,600 is 1.00M, not 1000K).
Thousands and plain numbers are drawn in a lighter colour than millions and billions, so the large
numbers stand out; a bold total stays bold. The lighter colour keeps at least 3:1 contrast against
the card and every tinted row it sits on. A count inside a sentence or a hover text uses the same
spelling.

A session opens to its agents. Its running agents are listed directly under it, each row with its
own model, with no line per provider and model (1.4.3): a flash job under a Claude Code session shows
its DeepSeek model on its own row. An agent that has not answered yet reads `no answers yet` in place of
the model, and an agent without a provider shows an error naming the session in its model cell. An agent in a report from an older seatswap, which has no provider, takes its session's. A
row's kind is left out for Claude Code's default `general-purpose` and for an agent without a kind:
the row then shows only the task, in the normal text colour; every other kind (`Explore`, `Plan`, a
custom agent) is shown in bold before the task in grey, a flash job has the tag `flash`, and an agent
with neither kind nor task reads "agent". A flash job's task is the `label` of its flash-agents report
(`<flash-agents state>/<key>/patches/<id>.report.json`), absent while it runs without a report; its
workers have no task. The label is shown live only and never written to any file. One row per agent
has kind, task, the model of its last answer (`no answers yet` before its first), running or completed with when it finished, tokens and both values, and its own agents as "1 running ·
2 done". An agent that started agents is open, to any depth, unless the user folded it; that choice
holds while the window is open. A flash job is an agent with
the tag `flash`, its workers below it. A flash job with exactly one worker, and that worker has no
agents of its own, draws no worker row and no fold (N-1); the job row's running and completed counts
stay as they were. Completed agents are folded into one "38 completed" row that opens and that
carries the subscription and API value of their tokens — one sum for the session, shown only when the
row holds all of its completed agents. An agent
without Claude Code's `finished` mark counts as done once its transcript ends with the agent's final
answer (an assistant entry with `stop_reason` `end_turn`); a resumed agent writes again and counts as
running again. A task text that a row shows comes from a live transcript; a flash worker's title is
shown as its task in the window, and `write_report` clears every agent's task before the report other
hosts read is written. The task text is never written to the history. A running session counts its
linked flash/Harness workers in its Agents, Tokens and both values exactly as its closed row does, so
nothing jumps when it closes.

**Codex and Harness rows.**
- **Codex.** Rows come from Codex's rollout files, `<codex folder>/sessions/YYYY/MM/DD/rollout-*.jsonl`,
  with folder links followed. A resumed session has several files: its tokens are the newest total
  in time over all of them, and its last hour is that total minus the newest total at or before an
  hour ago. A total that went down makes the last hour `—` with that reason. Only the
  `event_msg`/`token_count` lines are token records; a `token_usage_record` line is not read. A
  rollout has no name: a Codex row's title is the `thread_name` of the newest line (by
  `updated_at`) for its id in `<codex folder>/session_index.jsonl`, which Codex appends to when it
  names a thread after its first answer and on `/rename`; an empty or all-space name is no title. It
  is read with the rollouts, never written; a missing file is no title, a broken line a warning. A
  rollout is listed as running while any process holds the file open, or while its last event is
  younger than 30 minutes. Only a running rollout becomes a row. A `token_count` line without an
  `info` key is a warning; `info: null` is Codex's normal early line.
- **DeepSeek Harness.** A job first seen unlinked is its own `DeepSeek Harness` session under its
  Harness id and folder. A flash worker's folder is a copy, which flash-agents names by the first 12
  hex digits of the SHA-256 of the folder it copied, in one of its layouts: `<base>/<key>/slots/<n>`,
  `<tmp>/flash-mcp/<key>/<pid>/<instance>/slots/<slot>` or the older `<key>/<pid>/slots/<slot>`, a
  slot being `<n>`, `slot-<n>` or `slot-<n>-<m>`. An unlinked job takes the folder of a known Claude
  Code session whose key matches (with the copy's subfolder), else the copy's own folder; a folder is
  matched as recorded and, for a Windows path, with either case of the drive letter. An unlinked session is a row while it is open — a process holds its
  lock or its last event is younger than 30 minutes — Busy during a turn, Idle between turns; a
  flash job does one task, so it is done once its turn ended and leaves the list (its tokens stay
  in the history). Once it is linked to a Claude Code session — through the running process
  tree on Linux, or once its result is in a transcript — its later tokens go under that session. On
  Windows the transcript is the only link, so a running job appears once its result is collected. A
  recorded Harness total is kept while its session file exists, so a resumed old job is never counted
  twice.

**Around the tree.**
- *By model*: sessions, busy ones and the last hour's tokens per model; a model that runs under two
  providers keeps one total per provider in the tree and one entry in this card. A tied model carries
  the `tied` bubble, and the card's legend shows the bubble rather than a dot.
- *If you switched now*: how many sessions would work without their earlier thinking, how many of
  those are busy, how many would send their conversation again and how many pay nothing (§7.4.2),
  with a Switch… button that opens the pop-up. Only Claude Code rows on a Claude plan model count
  (`money::is_plan_model`: the model id starts with `claude-`), or with no model known yet; Codex,
  Harness, flash and other models are left out.
- *Last hour*: output tokens by project, with their subscription value.

**Closed sessions and hosts.**
- **Closed sessions**: none (the default) · 24h · 7d · 30d. They stand in
  the same tree, in their project and model group, in grey, with the time they closed. Every total
  follows the choice: the numbers on top and each group's total count exactly the sessions that are
  listed. They come from the history's session and token records, so they are there after Claude
  Code has deleted the transcript.
- **Hosts**: all, or one. Each host writes what is running on it into its own report in the shared
  history folder (`<host>/sessions-now.json`, below). Another host's row carries the age of its
  report ("vega · 2min ago"); a host whose report is older than 15 minutes says "no report since …"
  instead of running. Without a shared history folder only this host is shown; the report is still
  written whenever this host has a history folder, shared or not.

**How it updates.** While the area is open, the sessions thread watches the session files and the
running sessions' transcripts and pushes its view: a change is on screen within about a second, at
most two redraws a second, with a trailing one so the last numbers always arrive, and once a minute
for the durations and "last hour". A shown duration under one minute counts its seconds, redrawn
once a second, so "12s" becomes "13s" without waiting for the minute. Rows keep their place and open
rows stay open; a new session or agent appears in its group. A session that left turns grey for 60
seconds and is then removed, or, with closed sessions shown, a session that has a closing record is
already there as a closed row. Other hosts' rows change when their
report arrives. The thread reads the session files and transcripts whether or not the window is
open; the transcript folder watches and the pushes exist only while the area is subscribed. The
transcript parser's place is kept in a state file under the data folder. That file is a cache: a
state written by 1.1.0 (version 1) is migrated to version 2, keeping its offsets, so the transcripts
are not read again; a state of any other version is an error and stops that pass before it reads a
transcript or writes a token or session record, so nothing is rebuilt over it. The error says
`<path>: state version <found> is not supported; this seatswap reads version 1 and version 2` (and
adds ` (the file was written by a newer seatswap)` when the version is higher), and
the window shows `the transcript state cannot be read: <error>; no transcript is read until it can
be. Deleting <folder> reads every transcript again and writes its tokens to the history a second
time`. Deleting it does make the next pass read the transcripts from the start, and the tokens the
history already holds are then counted twice; the message says so rather than hiding the cost.

**The link `sessions?<session id>`.** The waiting notification's click (§7.5) opens that session:
the area opens it, marks it for 3 seconds and scrolls to it. A session that is no longer listed says
so.

**The report other hosts read.** `<history root>/<host>/sessions-now.json` is pretty JSON, written
atomically with mode 0600 in this host's own folder: `{ "at": "…", "sessions": [ … ] }`, the rows of
this host's Sessions area at that moment. It holds the session rows, each with its title when it
has one, and their agents' kind, model, running or finished state, times, tokens and values. It never holds an agent's task text (it is set
to null before writing) and no message text ever reaches it: none leaves the transcript parser. It
is written when the rows changed and the last write is at least 30 seconds old, at once when a
running session leaves the list, and as a heartbeat every 5 minutes when nothing changed, so a quiet
host is not taken for a stale one; an empty list is a report too. A report more than 15 minutes
before now is stale, and its sessions say "no report"; a time in the future is fresh. The import
never copies the file and a host folder's size leaves it out (§5).

### The Statistics area

The second area: seven pages over the history. Every number is computed from what the history
holds — usage readings, finished weekly windows, switches and blocks, token and session records,
waits — together with the accounts and settings, the prices and dollar rate of §11.5 and the
project log of §11.6. **The statistics only read:** no function of `core::stats` writes a file. The
history records they count hold only time, folder, model, block kinds, token counts, ids and a
session's title; no message text is read or kept for them.

**The frame.** Above the controls stands one line: `{n} subscriptions · {n} hosts`, and `since {date} ·
now` when the history has a first reading or switch. `since` is the earliest usage reading or switch
of any host — an OpenAI or Ollama reading counts too — while token and session records, which go back
further, do not set it; `now` marks the line's right end. Below the controls, the row of seven tabs
ends with the history's size (`history: 4.6 MB`, one decimal, 1 MB = 1 048 576 bytes) and an Export
button that opens Settings → History and hosts. The tabs are Overview · Weeks · Projects ·
Subscriptions · Forecast · Rhythm · Activity. Under them:

- **The range**: This week · Last week · 30d · 90d · 12 mo · All. Every range starts at Monday
  00:00 local time. *This week* runs from this week's Monday to now, *Last week* is the week before,
  and *30d* / *90d* / *12 mo* start at the Monday of the week that holds now − 30 / 90 / 365 days.
  *All* starts at the Monday of the week that holds the first record, so every page's first
  calendar week holds all its windows; a history with no record uses this week's Monday.
- **The subscriptions**, as chips in their colours: the Claude accounts and the stored provider
  subscriptions. A click takes one out of every page; at least
  one stays chosen. While no chip is taken out the query names no subscription, which means every
  one the history knows, including one no longer stored. As soon as a chip is taken out the
  query names the remaining stored subscriptions, so ones no longer stored fall out too. An
  OpenAI or Ollama token record belongs to its provider subscription (§11.5), so a chip chooses it
  like an account's; a provider record whose subscription cannot be named falls out with the first
  taken-out chip. Work of another company never takes a Claude account out (§11.5).
- **The host**, when the history holds more than one: All hosts, or one. Every page except Forecast
  follows it. A missing host list is a banner, and the pages still show every host.

The range, the excluded chips, the host and the tab are remembered. The address names a page
(`statistics/forecast`); without a page name the area keeps the last one chosen.

**An empty range.** A page whose range holds no record at all shows no chart of zeros. It says
`Nothing recorded yet.` or, when the history has any record, `Nothing recorded yet. seatswap has
been recording since {date}.` with the date as `Mon 9 Nov`. This is one marker from `stats::empty`,
the same on every page except Forecast. The Forecast is a plan for every account and host, not a
range; without a reading of every account it says `the forecast needs a reading of every account
first` instead, and while auto-burn waits for the first reading of some accounts it goes on `: AC has
none yet; it comes with the next usage poll` (`AC, PR have none yet; they come …`).

**A new install.** While the history holds no reading and no switch yet, the area shows one note
instead of any page: `No statistics yet`, then `Save your first account in
the pop-up. Statistics start with its first usage reading.` (with accounts: `Statistics start with
the first usage reading, which comes with the next usage poll.`) and `Then Activity and Rhythm fill
in as you work, the weekly numbers once a first weekly window has ended.` It ends by itself with the
first reading. A history whose accounts were all removed since stays on show.

**Definitions every page uses.**

- **Local time.** Every clock hour, day, week, weekday and curve point is in this computer's local
  time zone: the zone's offset at that instant, not one offset for the whole range, so a record on
  the other side of a daylight-saving change keeps its own hour and week. This holds for records
  from another host too. The Activity lanes and the Rhythm heat map take the hours of the week in
  this host's local time for every host.
- **A calendar week** is Monday 00:00 local to the next Monday 00:00 local. A page shows every
  calendar week its range touches; a partial week counts as one.
- **Every window of a week.** A finished `weekly_all` window belongs to the calendar week of its
  midpoint, `start + (end − start) / 2` (a window without a recorded start uses seven days before its
  end), and a range chooses the windows whose midpoints fall inside it. Two hosts' records are one
  window when they name the same account, kind and model and either end within 60 seconds of each
  other (two ordinary windows), their spans `(last reading, end]` overlap (two resets), or one is a
  reset whose start + 7 days is within 60 seconds of the ordinary window's end while the ordinary
  window began before the reset's end. The reset record wins; of two ordinary records the one with
  the latest last reading is kept. One window seen by two hosts is counted once. A week can hold more than one window — an
  early reset splits one, and two ordinary windows can share a week — and each is placed by its own
  midpoint. Every statistics page reads this one shared list.
- **A running window.** Each account's weekly window that runs now is the one of its newest weekly
  reading whose reset still lies ahead. It starts seven days before that reset; its use so far is
  the newest reading minus `from`, the reading a reset began it at. It is placed by its midpoint
  like a finished window, so it never moves to another week when it ends; that midpoint can lie in
  last week or next week. A page counts it where it shows the range's windows: it offers one week's
  price like any window, it has expired nothing yet, and its week is marked "so far". The Expired
  share, the expired finding and the expired part of auto-burn's averages use finished windows only;
  the This-week card and the other findings count the running ones.
- **The price of a window** is the account's monthly price at the window's midpoint: the last
  `PriceChanged` at or before it, or the first change when the midpoint lies before all of them, or
  the stored price when there is no change. It counts only in the currency it was entered in, as in
  §11.5.
- **Used, paid and expired money.** One percent of a week is worth `pv(price) = price × 12 / 52 /
  100` in the display currency. The money paid for a week is `100 × pv`, one week's share of the
  monthly price; the money the week offered is one week's price per window, so a week a reset split
  offers two. What expired is the sum of every window's `lost` (each `max(0, 100 − final) × pv`);
  a window above 100 % contributes 0, and a reset window's absent `lost` counts as 0. A window's use
  is `final − from` (`from` absent = 0), and a week's used money is the sum of its windows' use at
  one week's price. A window above 100 % therefore expired nothing; in a week with a reset, used and
  expired money together can be more than one week's price, because the reset gave a fresh window.
  The Used and Expired shares divide by the offered money, never by the one-week paid money: the
  Used share by the offered money of every window counted (on Weeks a running one too), the Expired
  share by the finished windows' offered money only, so it does not shrink while a window runs. A week
  without a price is left out of every money sum — the account's other, priced weeks still count —
  and the Overview and Subscriptions pages name that account under "No price for …".
- **Blocked hours.** A block is counted only while its account was the account in use on that host,
  and only in the working hours of the rhythm of the four weeks before the range's end: an hour of
  the week whose learned work is above zero. A block that is still open at the range's end counts
  up to it. Blocks of every limit kind count (weekly, session and per-model). Two hosts blocked at
  the same time each have their own span, so each machine's blocked time is shown.
- **Avoidable hours.** The part of the blocked hours in which another stored account with its
  auto-burn switch on was not blocked on that host and had a finished weekly window running that
  ended with something expired. The rest of the blocked hours is not avoidable: no other
  auto-burn account had expired quota running then, or it was blocked on that host too.
- **Switches** are the `Switch` events of the range and host, by who made them: *you* (the tray or
  the pop-up), *cli* (`seatswap use`), *auto* (auto-burn), *agent* (a call by an agent), and
  *external* (a change seatswap noticed but did not make).

**Overview.** On top stands the card **This week · so far** ("each subscription's current window").
It ignores the range: it always shows the weekly windows running now, so a new install sees its use
from the first reading on. It shows also when the range holds nothing: then "Nothing recorded yet."
stands below it instead of the rest of the page. The card has three totals over everything it counts:

- **Tokens**: the output tokens, with the project count below.
- **Used**: the subscription value used, with `27 % of 14.20 EUR paid for this week` below it. The
  paid sum is one week's price for each running Claude week, one week's price for each running
  OpenAI week, and `month price × 12 / 52` for an Ollama month (documented rule). A running window
  without a price adds nothing, the sum is drawn `≥` with the reason (R3), and the percent is `—`
  while the used value or the paid sum is partial. Google's running week is not
  in the sum; the line then ends with `, without GO`.
- **At API prices**: what the same work would cost without a subscription.

Below them, one row per project the card counts, sorted by the known part of its subscription value,
largest first, ties by tokens then name: the project's name; a bar as long as that known part, split
by subscription in each subscription's colour (work without a known account grey and striped, R2);
its tokens; its subscription value and API value. The top eight rows show; the rest stand behind
`Show all N projects ▾` (`Show fewer ▴` closes them), and the open list scrolls inside the card at
the height of those eight rows. A total row `all N` repeats the same figures. The first eight rows
keep their place when the list is opened.

Beside the list stands **Subscriptions this week**, one line per subscription with a running window:
the chip and plan, the use so far and its money (`27 % · 14.20 EUR`; `—` with the reason without a
price), and `resets Thu 10:00`; a provider row without a price shows its use and `no price`. Its
note says `Percent of each subscription's weekly limit; its window resets at the time shown.` The
account rows stand in the stored accounts' order, then the provider subscriptions in the chips'
order. A Claude account's row counts its token records of its running window from its start to now,
and work without a known account from the start of the calendar week; the host filter applies. A
provider row counts its records from its running window's start to now — OpenAI: its `weekly_all`
week; Ollama: the start of the calendar week, like Claude work without a known account, so its work
and its paid share cover the same week. Its line under **Subscriptions this week** keeps its
month's percent and money. Its subscription's chip chooses it like an account's.
Before the first usage reading the card says `No usage reading
yet: this card fills in with the next usage poll.`

The weekly chart below marks a running week `so far`. The card keeps its eight rows, so the whole
Overview scrolls inside the window under the tabs when the card is tall; the weekly charts keep
300 px.

Everything below counts the range's **weekly windows, the running ones included where a point says
so**: five numbers for the range,

- **Used**: the money used of the priced account weeks, the running ones included, with the money
  paid for them below it; its
  percent is the used money over the money the windows offered, one week's price per window, so a
  reset week offers two and a running window offers one. The figures row counts weekly windows alone:
  Claude accounts' and OpenAI subscriptions' `weekly_all` weeks. A priced week adds its money; a week
  without a price is left out, and a stored Claude account with such a week is named under "No price
  for …". An Ollama month is not a weekly window and is not counted there (documented rule).
- **Expired unused**: the money that expired and its percent of the finished windows' offered money,
  split per
  account, largest first. A window above 100 % counts all its use, so Used and Expired together can
  pass 100 %; nothing is cut.
- **Blocked**: one line per company — Anthropic, OpenAI, Ollama — with the working hours in which a
  subscription of that company was blocked inside that company's own working hours. Anthropic's
  hours count only while its account was the account in use; a company's blocked time counts in its
  own working hours only, never summed across companies. When some hours began
  before an account was added, the line below names that account by its short name, also when the
  account chips leave it out.
- **Avoidable**: the part of those hours in which another account's quota expired.
- **Switches**: the total, by you, cli, auto-burn, agents and external, and what one switch costs
  this host's account in use at the range's end as a percent of its 5h window (the measured cost of
  §2.13); `—` when the cost or the rates are missing.

Below them, every calendar week of the range carries two stacked bars in money: one stacked in the
subscriptions' colours with the expired part hatched on top, and one splitting the projects'
subscription value among the projects (the three largest, the rest as *other*). The hours blocked
stand under each week, and the marks say when an account was added and when auto-burn was switched
on or off. Beside them, "Share that expired, per week" is one line: the expired money over the money
the week's finished windows offered, by the same rule as the Expired share; a week without a priced
finished window has no point. When the range holds no weekly window at all, the chart card says
`No weekly window in this range yet.` instead of a chart; the numbers stay. A range whose only weekly
windows still run has its chart, because the chart counts the running ones (R5). A subscription has
the same colour on the card, in the project rows and in the matrix. "What the numbers say"
holds at most four sentences, in this order, each only when it has data:

1. `{abbr} is where most money expired: {amount} in {n} weeks.` — the account with the largest
   expired money, if above zero; only with a currency.
2. `You were blocked {h} in {n} weeks; {a} of them while other quota expired.`
3. `Since auto-burn ({date}): {h} blocked a week, {e} expired. Before: {h0}, {e0}.` — from the latest
   `AutoBurnSet { on: true }` in the range that stayed on, with a full week before and after it.
4. `{abbr} was full in {n} of {m} weeks.` — the account with the most weeks at 100 %.

The findings count every weekly window of the range, the running ones included, except the expired
finding; it and the Expired share use finished windows only, because a running window has expired
nothing. The Used figure, its paid line and the weekly chart count the running ones too.

**Weeks.** Every window as a grid under the heading "Every weekly window": one row per account, one
column per calendar week, the weeks' dates above, and the chosen week's detail in the card on the
right. The week blocks are built as the mock-up's. A cell draws the week's used part as a bar and its
expired share as a hatched band from the top; a week before the account existed is empty, not zero.
A cell holds one part per window of that account week, so a week with a reset has two parts and its
total may pass 100 %. A dashed line marks 100 %, a `now` mark stands at the grid's right, and next
to the rows stands the account's percent now from its newest reading; a row below the grid holds the
hours blocked per week. A click on a part opens that window's detail, also when it shares its week
with another part. One curve per stored account shows its weekly percent over the last 14 days; a
reset is a drop to 0 at the earlier window's reset when that lies between two readings, else at the
later reading, and the curve carries a `now` mark and, when the history starts after its first point,
`history from 16 Nov · no subscription value before`. The page opens on the summary of the newest
finished week of the range, or the running week's when no week has finished.

The running week: the grid's `now` column holds each account's newest percent; a click on it opens
the summary of the week its running window counts in (the week of its midpoint, which may be last
week or next week). That summary counts the running window's use so far, is tagged `so far` and
names its first number `Used so far`; its change against the week before is `—` while the window
runs. The summary of the week that holds now, when no window counts in it, says in which week the
running windows count: `The running weekly windows of AC and LB count in last week, the week that
holds their middle.` (`AC's running weekly window counts in …` for one).

- **The summary of a week** (a click on a week's date): used as a percent of the money the week
  offered (one week's price per window) and in money (a reset week's use can push the percent past
  100 %), expired in money, the hours blocked,
  the switches by who made them, the money used per
  account, the three projects with the largest share of the week's API value (then *other*), the
  week's tokens and API value, and the change of the used money against the week before (`—` when
  either week has no priced account week). Its conclusion has at most three sentences:
  `No hour blocked.` or `{h} blocked.`; then the account week with the largest expired share — `{abbr}
  came closest to expiring unused: {n} left at its reset.` under 10 %, else `{n} of {abbr}'s week
  expired, {money}.` when a currency is set; then `Auto-burn held back {n} planned
  {switch|switches}, {m} {minute|minutes} in total.` when switches of the week carry a waited time.
- **The detail of a window** (a click on a part): its head names the window's plan and its weekly
  price (`X a week`); then used and expired in percent and money; how many of the account's 5h
  windows fit in its usual working hours and how many were used — `{n} used, more than the about {m}
  your usual working hours hold` when the use passes that count, else `{n} used of about {m} possible
  in your working hours`; the first weekly block of that account inside the window (`reached 100 %`)
  and the working hours blocked after it; how many switches went into or out of the account and how
  many of them were auto-burn's; the age of the last reading before the reset; whether the window is
  marked uncertain (seatswap was off at the reset); a note when a reset ended the window early or the
  last reading was uncertain; and the week's curve, built from the account's weekly readings inside
  the window that name its reset.

**Projects and organizations.** A tree table with three levels: organization → project → folder.
An organization's row comes first, with its source in words — `from the owner of the repositories
on GitHub`, `from an account's login` or `made here` — its tokens in and out and its distinct
sessions over all time,
and its project count; an organization with no project and no account is dimmed. An owner
organization is never removed and stays as a row (R1), marked empty when it has no account either. Its projects follow by API value, and a project's folders
appear under it when it has more than one. A folder is the first path part below the project's root
on that host (the common parent of the project's folders), or the top folder. The second view is the
plain folder tree without organization rows; either view opens and closes its rows, a project's
folders start folded, and the fold lasts while the window is open, as on Sessions (R7). Every row
shows its tokens, its share of the
page's API value, its subscription value and API value, a bar split by subscription in each
subscription's colour, and its distinct sessions. A part whose work has no known account (a Claude
record without an account, a provider record without a named subscription) is grey and striped at
the end of the bar; its tooltip names it `unknown account`. An OpenAI or Ollama part is the colour
of its subscription, like a Claude account's; a row's name carries no company tag. The page's note
says `No price for {subscriptions}: their tokens add no value; shares count priced work only.` when a
model has no price.

A share whose API value or whose page total has no priced part is `—` with the reason, never `0 %`.
A share of an API value counts the priced work only (V-2): it divides a part's known API value by
the whole's known API value, so a part without a price is left out and the others keep their shares.
The row's share of the page's API value and the split by subscription both use this rule; the other
ratios on the page, `× paid` and the Used share, use full halves only (R3). A project's detail gives
its share of all use
(`21.9 % of all use · 18 sessions`), lists its subscriptions with their shares, and gives the line
`API values at 1 USD = 0.9048 EUR (13 Nov)`, or
`API values at each day's rate; latest …` when the rates of the range differed.

- **Organization × subscription** is the subscription value per organization and subscription, in
  the chips' order: the Claude accounts in the stored accounts' order, then the provider
  subscriptions. Every subscription has its own column and its own colour. Each cell is the
  subscription value of that organization's records on that subscription; a cell without one is
  `—` with the reason, and a measured `0` stays `0`. Each row and column total is the `Values` sum
  of its cells, so a missing cell leaves the known part and the total is drawn as partial, never
  unknown (R3). The `Used` footer repeats the column totals, each the sum of the cells above it.
  For an organization linked to its own Claude account it carries the sentence
  `{abbr} is {org}'s subscription. It covered {n} of the work for {org}.`; below 100 % it adds
  ` The rest ran on {others}: {amount} of their value.` when another account carried some, and, when
  that account's subscription also carried work for other organizations,
  `The other way round: {abbr} carried {amount} of work for {orgs}.` The sentences name the Claude
  accounts only.
- **A project's detail** (a click on a project row): its tokens, the share of answers that came from
  agents, each host's share of its API value, its subscription value, each of its subscriptions with
  its share, its API value, its output tokens per
  calendar week and model, and its distinct sessions. A folder key belongs to one host: with All
  hosts chosen, the detail of that key counts every host's folder of that name; with one host chosen,
  only that host's.
- **Tokens per percent** of each account is its output tokens of its finished weeks over the sum of
  those weeks' use (each week's windows summed). A fall means use that left no transcript, such as
  claude.ai, the phone, Claude Desktop or a host without the shared history. The page says that such
  use is in the percent but in no project, and how many tokens of Claude models have no known
  account at all; the note `{tokens} tokens: account taken from the usage readings.` names the work
  booked from the usage readings (G-1).
- The page also names the models whose API value is missing because they have no price, and the
  tokens that have no subscription value with the reasons. It does not merge two projects with the
  same name: their keys differ.

**Subscriptions.** The page fits every stored subscription alike, whatever chips it shows: one
factor per subscription and limit from the one estimator in §7.4.1 over the page's range, with the
provider subscriptions' own pairs. An OpenAI pair counts only when every OpenAI record inside it
belongs to that subscription and at least one record in it does belong to it; an Ollama pair
likewise. A pair that no record of the subscription touches says nothing about its plan, and a
record of another company never counts (documented rules).
A Claude account's factors are the ones auto-burn uses. A subscription with no factor at all says
`not measured yet: no pair with priced work in this range`, a Google one
`seatswap does not record the work agy does`; one whose factor leans on its prior still
shows its hours, but its size against the reference says `not measured` and only the size by name is
shown.

Above the cards stands one totals line. **All {n} subscriptions, a month** counts only subscriptions
with a finished week (or Ollama month) in the range, without Google's (§7.9), and says
`No totals: {reason}.` when none counts; then their monthly cost, their worth at API prices with `× paid`,
their tokens used a month and `from the last {w} weeks`. Each subscription counts with its average
week over its own finished weeks in the range × 52 / 12; Ollama counts its average month. A monthly
total with a missing part has no figure, never 0, and a sum over nothing has no figure either; the
subscription sums on the other pages follow the `Values` rule of §11.5. A removed
account's periods in the range keep their money and tokens. A price typed at most 15 minutes before a
plan change belongs to the new plan's period (documented rule), so it makes no minutes-long period of
the old plan at the new price; a price typed after adding an account or changing its plan counts from
that plan's start.

The four cards:

- **What does each plan give me?** For each subscription, what a full limit (Ollama: a month's)
  holds of the owner's actual work: **hours of work** and **tokens**, `100 / factor` API dollars
  over the **worked hour** of the subscription's company. The worked hour is the average local clock
  hour of the last 4 weeks that holds priced work of the company's models, on any host (an hour
  counts once, however many hosts worked in it): its API dollars and its tokens, all of the
  company's models mixed as the owner used them. An hour without work counts nowhere, so a model or a
  weekday used now and then does not thin it out (1.4.1; the per-model hours of 1.3 and 1.4 spread
  a model's work over all four weeks and showed a model used in one week three to four times too
  long). The split by model is the **Tokens a week** card's. Without a worked hour the cell is `—`
  titled `no hour with priced work of this company in the last 4 weeks`. **Over the weeks** says `steady`, or names a change (`gpt-6-astra 24h → 19h since 19 Oct`)
  when two consecutive finished weeks (one calendar week apart; a week without a point breaks the
  run) lie on the same side outside the range of the 8 weeks before (documented rule). That rule
  needs 10 weeks (Ollama: months); with fewer in the range it is `—`, titled `too few weeks to tell a
  change: 7 of the 10 it needs`: a short line is not steady, it is too short to tell (1.5.3). **Based on** shows the factor's `basis_usd` in worked hours; the figure is
  marked `thin` when half its range's width is more than 20 % of its value (documented rule). Below
  it, **Plan sizes, measured**: each plan's week factor against the reference plan — the measured
  Anthropic plan with the largest `basis_usd`, or no reference when none is measured — as
  `value_ref / value` with its range, beside the size by name (the capacity ratio) or
  `not measured`. A factor that leans on its
  prior is not a measured size and shows no ratio.
- **Am I using what I pay for?** Grouped by company with a subtotal line, one row per subscription:
  the price a month (the entered price and currency in the tooltip when converted), the use (its
  average share of the limit over its finished weeks; Ollama: of the monthly credits over its finished
  months), the money expired, the value at API
  prices and `× paid`. A total row sums them. A subscription without a price says
  `no price yet · set it` in its price cell; `set it` links to that subscription's row in Settings →
  Accounts, where the price field is focused (Q-1).
- **What 1 % of a limit buys, over the weeks.** Tabs per subscription, then per limit: Week, 5h, and
  Month for Ollama. One point per finished week: the tokens per 1 % of the main model (the model
  with the most work), `1 / (factor_week × usd_per_token(model))`, with the range as two dashed
  lines. A change is found by the same two-week rule as above; the sentence above the chart says
  `Since <date>, 1 % of <abbr>'s week buys about N % fewer <model> tokens: A instead of B` and a mark
  shows the week; otherwise it says `Steady. No week left its usual range.`, or with fewer than 10
  points `Too few weeks to tell a change: 7 of the 10 it needs.` Under it stand the
  tokens 1 % buys of each model, then `1 % ≈ N min of your work` (`1 / factor` API dollars over the
  worked hour); for 5h also how much work a full window holds, in the same worked hours.
- **Tokens a week, per subscription and model.** Rows are the subscriptions with a week (Ollama has
  none); columns are the models of those rows' companies seen in the range, priced or not, plus each
  company's other priced models of the same line-up (Opus, Sonnet, Haiku, Fable for Anthropic). A cell is the tokens a full weekly limit holds: `100 / factor` API
  dollars divided by the model's dollars per token at the owner's token mix for it. A model the owner
  never used is priced with the token mix of the same company's most used model and marked
  `estimated` (a striped bar); the main model needs a token mix, not a price of its own. A figure
  whose factor still leans on its plan-size prior says `assumed` instead of a range. Bars
  compare within a column. An empty cell is `—` titled with its reason: `not offered by {company}`,
  `no price for {model}`, or why the estimate could not be made. The note reads `Fitted from all work up to now each time the page opens; the range is how
  far a figure may still be off.` (The fit runs on every load of the page; an open page does not reload
  by itself.)

The what-if replay is gone: no page replays a week without a subscription, and there is no
`stats_what_if` command.

**Forecast.** The latest plan of every account and every host, so it follows neither the range, the
account chips nor the host choice. Each time the page loads it uses the plan reading kept from the
auto-burn runtime; when the oldest reading that plan was built from is older than 20 minutes the
page refuses it and says `the forecast needs a reading of every account first`. It says how many
weeks of history that plan reads. Below it:

- **Four numbers**: `Expected to expire this week`, the money expected to expire with auto-burn's plan
  as it is set now (eager when early switching pays, else lazy); `With “only when full”`, the same
  when switching only when an account is full; `Hours with no usable account`, the
  hours of this plan with no usable account; and `Switches planned`, the switches planned with
  `about N a day · N with “only when full”` below it. One sentence for all accounts together follows
  them.
- **Per account**: the weekly percent now; the percent the plan expects at its reset (`100 − the
  plan's unused percent`); the low of its last 8 finished windows; when the plan first has it full;
  its next reset; and what switching only when full would leave unused, in percent and money.
- **The low** is this week's percent now plus the second-lowest gain of its last 8 finished windows,
  at most 100. A window's gain is its final percent minus the percent of the newest weekly reading
  at or before `end − d` that names the window's reset, where `d` is the time from now to the reset
  the plan plays the account to. A reset window names its reset by its start + 7 days, because it
  ends at the reading that showed the drop; an ordinary window names it by its end. A week with a
  reset holds several windows and every one counts. A window without such a reading is left out, and
  so is a negative gain (a drop after a model reset is not use); with fewer than four gains there is
  no low.
- **The sentence**: while an account auto-burn may use has no week factor it starts
  `{abbrs}'s rates are not known yet. ` and while one's factor leans on its prior
  `{abbrs}'s rates are assumed from its plan. ` (`their plans` for more than one). Then either
  `Enough for the week.`, or
  `All accounts full from {weekday HH:MM} to the end of the forecast.`, or
  `All accounts full from {weekday HH:MM} until {abbr} resets {weekday HH:MM}.` when the gap ends at
  that account's reset, else `All accounts full from {weekday HH:MM} until {abbr} is usable again
  {weekday HH:MM}.` A reopened 5h window is not a reset.
- **The lanes** are the next 7 days, one row per account: the stretches the plan plays it, its
  weekly reset as a mark, and the stretches where no account is usable. The `none usable` row is
  always last. Times are local.

**Rhythm.** A heat map of 7 weekdays × 24 clock hours: the output tokens of each hour of the week,
averaged over the range's calendar weeks (a partial week counts as one), all chosen hosts and
companies together. Below it, per weekday: the working hours on that weekday of the range's whole
local days — the distinct local hours with Anthropic tokens, added up over those days and divided by
their number — and the blocked working hours of
Anthropic accounts in the same whole days, divided by the same number. A weekday with no whole local
day inside the range has not ended there and shows `—`, never 0. A local day on the night the clocks
go back has 25 distinct hours; the bars' axis then grows to 25h, with its labels still at 0, 6, 12,
18 and 24h, and a value above 25 is an error. Both charts write each bar's value above it
(`6h 30min`); its tooltip reads `Mon · 6h 30min on an average Monday`. Auto-burn and the Forecast
plan with
the rhythm of the last 4 weeks before now, not with the range's picture; an hour that usually has
work counts as a working hour.

**Activity.** Every chosen host gets a lane over the last 168 local hours, ending with the hour that
holds the range's end. Each hour is one of four states, decided in this order:

| State | Rule |
|---|---|
| Work | the hour holds token records of that host |
| Idle | no tokens, but an `up` record in that hour |
| Off | no tokens and no `up`, but the hour lies at or after the host's first `up` record: seatswap was not running, the machine was off or asleep |
| Unknown | before the host's first `up` record, or without one |

An hour's tooltip holds its output tokens per model, its sessions and the project that used most;
the strip below each lane is a heatmap of how many sessions worked in that hour: each cell fills the
strip's height, coloured by that number over the largest in any lane, and an hour without a session
leaves the bare track. It uses the one heat colour rule Rhythm's cells use. On top stands one
**Last 7 days** box for the page's span, `{h} working` summed over the hosts with each host's share
in the tooltip, then `on but idle {h} · off {h}` and `not known {h}` when any is not known; then
**Tokens**, the range's output tokens summed over the hosts with each
host's below it; **Their
value**, the subscription value and the API value summed over the hosts; **Per working hour**, with
the highest hour. Then **What it shows**: a sentence about the last 7 days — `2 nights with
work on atlas.` for the first host with work before 05:00, and each other host's evenings and
weekend use; with none of those, `No night work, evening or weekend use stands out in the last 7
days.`; with no hour of work at all, `No work recorded in the last 7 days.` The Per week table lists
no row when the range holds no token record; it says `no work recorded in the range`. The lanes carry no agent data, so it never claims an agent ran. Per host the page also counts
hours working, on but idle, off and not known, and its tokens and two values. A switch picks what
the hour bars measure: tokens (per model) · subscription value · API value (the two values exist per
hour only); in the subscription view the chart marks `history from 16 Nov · no subscription value
before`. The chart is as tall as the mock-up's, its host names wrap instead of being cut, and it has
no row labels. The chart tracks what seatswap sees: one lane per host, and Ollama's account total
is never a lane of its own. When Ollama counted more than the known sessions, one muted line under the
lanes, inside the chart's card, says so: `Ollama counted {tokens} more tokens in this range than the
sessions seatswap saw (other machines, apps or Ollama's website).` The number sums, over the UTC hours
whose start is inside the range, Ollama's input + output of the hour (its largest line across hosts,
never a sum) minus the input + output of the known sessions on Ollama models (`…:cloud`) in that same
UTC hour, counted over every host whatever the host filter; an hour where the sessions used more than
Ollama reported adds nothing, never a negative. With 0, or no Ollama line, there is no note. Below the lanes:
output tokens per day for the last 14 days, one bar per host. The card below has two tabs, *Per week*
and *Largest sessions*, and its switch sits inside the card's frame, which lines up with the card
above. *Per week*: one row per calendar week with its working hours per host, tokens and their two
values, sessions, and the project and folder that used most, the current week tagged `running`.
*Largest sessions*: at most 20, by output tokens, each named as the Sessions area names a session (its
title, else its label, from its newest session record), with project, host, model, hours worked, tokens and
their two values, and the agents it started once the session has ended (a running session has none
yet). The minutes sessions waited for you per local day come from the `Waited` events; they follow
the host choice but no account chip, because a wait names no account.

**What is not built here.** Of the proposals of §3.10, "Tokens per percent, over time" and "Time
sessions waited for you" are on these pages (Subscriptions and Activity). "What auto-burn did" and
"Where tokens are lost" are not built and stay proposals.

### The Status line area

The third area: the layout of `seatswap statusline` is built here, and turned on in Claude Code from
here. The page edits `settings.statusline` and asks the backend for every preview; it draws no line
of its own. Every change is saved through the settings page's one writer at once; a refused save
puts the stored layout back and shows the reason at the top of the page.

**The palette.** The segments that are in neither zone, in three groups: *seatswap* (account, plan,
5h percent, 5h reset, week percent, week reset, per-model %, auto-burn: next), *Claude Code*
(model, effort, folder, git branch, context used, session cost, lines changed) and *System*
(`user@host`, clock, free text, separator). Free text and separator can be added more than once; a
segment that is already on the line is not in the palette. A click adds it to the zone last used. A
separator added from the palette starts as ` │ `, one space before and after the pipe.

**The two zones and the margin.** *Left*, and *Right* which is flush with the right edge of the
terminal; at least two spaces stay between the zones while both print something, and an empty
zone takes no gap. A segment is dragged from the palette into a
zone, between two zones and inside one; the keyboard does the same: ←/→ reorder it, `L`/`R` move it
to that zone, `Delete` sends it back to the palette. A segment dropped onto itself, or a drop
outside a zone, changes nothing. A segment from a zone dropped onto the *Segments* box is removed —
the box lights up `drop here to remove` while a chip is dragged over it — and the Options card's
Remove button removes the chosen segment; `Delete` keeps working. Below the zones: the *margin*, the
columns kept free at the terminal's right edge, and the switch *Pass Claude Code's live numbers on to
seatswap* (`statuslineFeed`, §7.6).

**The options of the chosen segment.** The page offers only what applies to its kind:

| Option | What it offers |
|---|---|
| Colour | *none* · *fixed colour* (`#rrggbb`) for every kind; *by load* (yellow from, red from) only for the 5h percent, the week percent and the per-model percent; *by model family* only for the model; *by effort* only for the effort; *the account's colour* only for the account |
| Time format | Only the 5h reset and the week reset: *Compact time left* · *seatswap's* (the rule of §7.2) · *Clock time*; without one seatswap's own format is used |
| Text | Only free text; a segment whose text is empty prints nothing, with its prefix and suffix |
| Separator | Only the separator: *pipe* `│` · *dot* `·` · *bullet* `•` · *slash* `/` · *chevron* `›` · *blank* (one space). A separator saved by 1.1.0 without a character now prints `│` where 1.1.0 printed ASCII `|`. No powerline glyphs |
| Prefix / suffix | Every kind; printed only when the segment has a value; every space in the field is shown (a faint mark behind it, and a count such as "2 spaces") |
| Drop when narrow | *never* · *1 · first* · *2* · *3* |

A segment is drawn as up to three spans: prefix, value and suffix. Only the value span carries the
colour rule's background, and the account tag's background covers its letters only. With no
background the prefix and the suffix keep the value's colour; when the rule paints a background both
are plain, so the colour stops at the value.

The folder has no style option: it is always `~`-shortened. The settings check refuses a colour
rule, a time format or text that the segment's kind does not offer, naming the kind and its place
in the layout. A value the page cannot read (an unknown kind, a colour rule its kind does not
offer, a drop number above 3) is shown as it was stored, never replaced by a guess.

**The preview.** Three width fields (120, 100 and 80 columns at start, each 1 to 65535 and
editable), a *Dark terminal* and a *Light terminal* tab, and the scenario *Live data* · *Test: 97 %*
· *Test: account blocked* · *Test: no 5h limit*. Every width gets its own line, drawn with the
spans' own colours and bold flags on the terminal background, in a monospace terminal font, in a box
as wide as its width in columns; the line ends the margin before the right edge, as in a terminal. Beside each line is its note: `everything fits`, or the kinds the fitting
removed, as `5h reset, week reset and 5h percent are dropped`. The preview draws a made-up
session with the live account's numbers: the folder `~/projects/atlas` under the user's home,
the branch `main`, context used `41%`, session cost `$3.20`, lines changed `+120 -8`, and the
model and effort the payload names; the account's abbreviation, colour, plan and limits are the
live account's. The text and the colours come from
`statusline_preview`, the command's own renderer (§7.6): the preview and the real line are one
implementation. The preview is requested again with every new reading, and 150 ms after the last
change. Without any login yet (a first run) the page asks for no preview and shows an information
line: `No Claude login yet. Log in to Claude Code once and save the account in the pop-up; the preview
then shows its numbers.` Any other failure, such as an ended login, shows the command's error instead
of a line.

**The presets.** *My current line* (what the script of §5.1 printed; the stored default), *Limits
only* and *Everything*. A layout that equals none of them shows *Custom layout*. Choosing a preset
while the layout was changed asks *Replace the layout?* with Cancel and Replace first; without a
change it applies at once. Every preset is saved like a hand-made layout.

**The three payload segments.** They read Claude Code's own numbers from the payload, and each is a
plain number in the JSON:

| Segment | Payload fields | Prints |
|---|---|---|
| context used | `context_window.used_percentage` | `41%` |
| session cost | `cost.total_cost_usd` | `$3.20` |
| lines changed | `cost.total_lines_added` and `cost.total_lines_removed` | `+120 -8` |

A field that is `null` or absent prints nothing, like every other segment, and so does a value that
is not a number. Lines changed needs both numbers. Before a session's first answer Claude Code sends
`"used_percentage": null`, so the context used segment is empty, and `"total_cost_usd": 0`, a whole
number, which prints as `$0.00`.

**Use in Claude Code.** The button first shows a card: the file (`<config>/settings.json`; on Linux
`~/.claude/settings.json`, on Windows `%USERPROFILE%\.claude\settings.json`), the value of
`statusLine` *now* and *after* as JSON, and the sentence "seatswap changes only this key. The value
it replaces is kept and can be put back."; then Install or Cancel. When seatswap's line is already
in place the button reads *In use in Claude Code*, and *Restore previous* stands beside it whenever
there is something to restore.

The value seatswap writes is

```json
{"type": "command", "command": "\"<path>\" statusline", "refreshInterval": 2}
```

where `<path>` is the console `seatswap` program beside the running one (on Windows
`seatswap.exe`, also when the tray program makes the change). The path is always inside double
quotes, so the shell Claude Code runs the command through — bash, or Git Bash on Windows — reads it
as one word; inside the quotes `"`, `$` and `` ` `` get a leading backslash and a backslash is
doubled where it would otherwise escape or be eaten, so a path with a space, a dollar sign or
Windows backslashes arrives unchanged. A path that is not valid Unicode is refused.

The write itself:
- **One key.** seatswap writes exactly one key of Claude Code's `settings.json`: `statusLine`. Every
  other key keeps its value and its place in the file's order; the file is written again as JSON
  with two spaces per level and no trailing newline.
- **A backup once.** Before the first write of an existing file, a copy is kept beside it as
  `settings.json.seatswap-backup`. It is never updated.
- **A missing file** is created holding only `statusLine`.
- **A file that is not a JSON object** — an empty file, broken JSON, a list, a string — is refused
  with its path and the reason, and left byte for byte as it was.
- **A symbolic link stays a link.** The target the link (or the chain of links) points at is
  rewritten; the link is never replaced by a regular file. A link whose target does not exist is
  refused with the link's path, and no file is created beside it.
- Every write is atomic (`atomic::write_atomic`), so a reader sees the old or the new file.

**The install record.** seatswap keeps `statuslineInstall` in its own settings:
`{installed, previous}` — the exact `statusLine` value it wrote last, and the exact value the file
had before its first install. `previous` absent means the file had no `statusLine` key; `previous:
null` means the key was there with the value `null`, and that is what comes back. Installing again
while seatswap's line is in place writes nothing and keeps the record. Installing from a moved
program (another `<path>`) writes the new command but keeps the first `previous` while the file
still holds the record's `installed` line, so the user's own line is never replaced by seatswap's
older one; a line set by hand after an install becomes the new `previous`.

**Restore previous.** It is offered only while seatswap's line is in Claude Code — the record exists
and the current value is seatswap's line or the record's `installed` line. It then writes the
record's `previous` back exactly, or removes the key when there was none, and clears the record. A
line set by hand afterwards is never overwritten: without a record it refuses with "seatswap has no
record of the line before its own: nothing to restore", and when the file holds any other line "the
status line in Claude Code is not seatswap's: nothing to restore", leaving the file untouched.

**Undoing it by hand.** In Claude Code's `settings.json`, `statusLine` may be deleted (or replaced by
the old value) with an editor; nothing else seatswap wrote there has to be touched, because it
wrote nothing else. The copy `settings.json.seatswap-backup` is the file as it was before seatswap's
first write to it.

---

## 11.5 Money: API prices, the dollar rate and the value of tokens

Usage is a percent of each account's own week. Money is the common unit that adds accounts up and
says what a token or a subscription was worth. Amounts are shown with the currency code after the
number: `1,234 EUR`, and below 10 with two decimals, `6.47 EUR`.

- **One currency.** `currency` is the currency every amount is shown in. There is no default: until
  it is set, amounts in money stay missing and the page asks for it.
- **A price per account or provider subscription.** An account's or provider subscription's `price`
  is what it costs per month, in the currency it was entered in, `priceCurrency`. Without a price it
  is left out of every sum
  in money, and the window says so; it is never assumed. A price whose `priceCurrency` is another
  currency than the current one does not count, and the page marks it so it can be entered again; a
  price entered before a currency was set has no `priceCurrency` and does not count either. A price
  typed at most 15 minutes before a plan change belongs to the new plan's period (documented rule).
- **The dollar rate.** API prices are in dollars; a rate turns them into the chosen currency. No rate
  is entered by hand: `USD` itself is always 1, and every other currency uses the looked-up ECB
  rate. Without a stored rate an API amount is not unknown: it is shown in USD and labelled USD, and
  the value's currency says `USD`; a record with no currency at all has no value in the currency and
  says its currency is missing. The code never assumes a rate other than 1 for USD. The settings row shows the rate in effect and where it came
  from — `ECB rate of Fri 13 Nov · European Central Bank via Frankfurter · fetched today 10:10`, or
  `fetched: not recorded` for a point saved by 1.1.0 — or `no rate yet: API values are shown in USD`,
  or `look-ups are off: API values are shown in USD`, or `USD itself`, and a
  failed rate look-up is shown there. With `lookUpPrices` off the API column stays in USD and its
  header says so.
- **The value of one percent** of an account's week is `monthly price × 12 / 52 / 100`: 180 EUR a
  month is 41.54 EUR a week and 0.415 EUR per percent.

**API prices from models.dev.** The list is read for three companies: `anthropic`, `openai` and
`ollama-cloud`. A model's key is `<provider>/<model id>`; its price has four parts, in USD per
million tokens: `input`, `output`, `cacheWrite` and `cacheRead`. A model whose `cost.tiers` makes
the price depend on the prompt size is marked `tiered` and counted at its base price. A model that
lacks an input or an output price is left out; a missing cache price counts as 0. A cost that is
not a number, negative or not finite leaves that model out of the list and names it in the settings
("2 models of models.dev's list were left out"), while the rest of the list is stored; a provider
without a model list, an answer that is not the expected model list, and an answer with none of the
three providers are errors naming the answer, and nothing is stored. The list is read-only: no price
can be edited or added. A model that shows up without a price is asked for from models.dev only when
it is a Claude plan model (its id starts with `claude-`) and is used by a listed session row or
agent; the request is made at most once per model and per hour.

**A provider's own cost.** Claude Code's status line feed carries `cost.total_cost_usd` of a session
(§7.6). Where a session has such a provider's number, that number is the session's API value and
beats the models.dev price (`money::api_value`); without one the models.dev price is used, and
without either there is no API value. The number is in USD; with a stored rate it is converted, and
without one it stays in USD and is labelled USD, never unknown. A price that cannot be put in the
page's currency is not guessed at: the row says why. The feed holds only the last session that wrote
it, so a session's cost can be seconds old, and the feed's own reason for a cost it did not store is
shown as a warning on every pass while it lasts; a cost that is null, absent, not a finite number of
0 or more, or converts to a non-finite amount is not used. The status line itself is never broken by
any of it.

**Prices are kept with their dates.** Each looked-up price that differs from the stored one starts a
new point at the look-up's time; the tokens of a past day keep the price of that day. `prices.json`
holds the points per key, the last look-up's `checkedAt`, its `etag`, the last change and the models
the last list lacked. The price of a key at a time is the last point at or before that time, else
the first point. A model missing from a later list keeps its points; it is reported as removed once,
not on every list that still lacks it. The settings page's price list and `seatswap prices` show the
price in effect. An older `prices.json` that still holds `overrides` (own prices) loads and ignores
them; the next write leaves them out.

**The dollar rate from Frankfurter.** `fx.json` holds one currency and its dated points, each with
the time it was fetched. The rate of a day is the last point at or before it, else the first point.
A looked-up rate that is not a finite number above 0 is an error and nothing is saved. On the next
look-up after the currency changes, the old currency's points are replaced by a new file for the new
currency. A point written by 1.1.0 keeps no fetch time; the settings row shows `fetched: not
recorded` for it.

**The two look-ups and the switch.** `lookUpPrices` (default on) is the one switch. Both requests go
through `api`, both carry the User-Agent `seatswap/<version>` and nothing about the user, an account
or this machine:

| What | Request | When |
|---|---|---|
| API prices | `GET https://models.dev/api.json`, with `If-None-Match: <etag>` when a list is stored | at the tray app's start, then every hour while `lookUpPrices` is on; on a Money setting's save; and with `seatswap prices --refresh` |
| The dollar rate | `GET https://api.frankfurter.dev/v1/latest?base=USD&symbols=<currency>` | at every start of the tray app (even when today's rate is stored), at once when the settings change (a save here, or a currency shared from another host), and then in the hourly round, once per Frankfurt day; on working days from 16:00 to 18:00 Frankfurt time while the stored rate is older than today, every 30 minutes until the answer carries today's date or it is 18:00, then not again that day. Only when `lookUpPrices` is on and the currency is set and not `USD` |

"Not modified" only moves `checkedAt`; the stored prices stay. The 16:00–18:00 asks are Frankfurt
time, computed from UTC with the EU summer-time rule (the last Sunday of March and of October at
01:00 UTC); they run on Monday to Friday, and an answer that carries today's date ends them for the
day. A look-up that fails saves nothing:
the last values stay with their age, the error is shown under "API prices" or at the rate, and no
number is zeroed or replaced. After a failed rate request the next round tries again; an answer for
today is not asked for again that day, so a weekend with no new ECB rate does not cause an hourly
request. With `lookUpPrices` off neither host is contacted and `seatswap prices --refresh` refuses;
the stored values keep being shown. Nothing else on the machine is contacted for money.

**What a failure means for a value.** A token record whose model has no price gets no API value and
the model's name is recorded as the reason, never a guessed price. A record without a currency set
gets no value in the currency and says its currency is missing; a record with a currency and no
stored rate is shown in USD and labelled USD. The value of a subscription is left out when
the account has no price, or when the week's used part or its API value is unknown.

**A partly valued sum.** A sum of `Values` keeps the known part of a half whose other parts are
missing: the amount is the known part, it is drawn `≥ known part` with the first missing part's
reason in its tooltip, and the half is marked partial. A half with no known part at all is `—` with
its reason, never 0; a sum over nothing is a known 0. A `Values` sum is what the project rows, the
Organization × subscription table and the This-week card add.

A share, ratio or percent comes only from full halves: a partial half is a lower bound, and a
quotient of a lower bound is no figure. It is `—` with the half's reason, never a guessed percent.
There is one exception, the one the owner decided on 18 Nov (V-2): **a share of an API value counts
the priced work only**. It divides a part's known API value by the whole's known API value
(`stats::priced_api`), so one model without a price no longer empties every share; a row with no
priced API value has no share (`—`). The project tree's shares, the host shares, the This-week card's
and Projects' split by subscription all use this rule, and Statistics → Projects says "shares count
priced work only" when a model has no price. Other ratios — times paid, used of paid, the Expired
share — keep full halves.

The monthly totals over subscriptions on the Subscriptions page are one sum per figure: a
subscription in it without a price or a finished week leaves that figure `—`, never 0.

**The account of a record.** A Claude record belongs to the account the history's live and switch
events name for its host at its time. When they name none, it is booked from the usage readings
(G-1) to a stored Anthropic account when exactly one qualifies: that account's last reading at or
before the record's start and first reading at or after its end are both at most 30 minutes away,
both name the same weekly reset, and the later percent is higher; no other stored Anthropic account
may have risen around the span by the same reading rule (a change of session reset with a percent
above 0 is a rise). Without exactly one such account the record's account stays unknown. Such a
record counts under that account everywhere on the Statistics pages; only the Projects page's note
says `{tokens} tokens: account taken from the usage readings.` The rhythm, the rates and the Forecast
still read only the live account.

**How a value is computed.**
- One token record's API value in USD is
  `(input × input price + output × output price + cache write × cacheWrite price + cache read ×
  cacheRead price) / 1,000,000`, at the price of the record's day. A model's price key names its
  provider: `claude-*` → `anthropic/<id>`, `<name>:cloud` → `ollama-cloud/<name>`,
  `<name>:<tag>-cloud` → `ollama-cloud/<name>:<tag>`, any other id (Codex's) → `openai/<id>`. A model
  with no price stays unpriced, with the model named.
  The value in the currency is that USD amount times the rate of the record's day.
- A token record belongs to its model's company: `claude-*` is Anthropic's, an Ollama cloud id
  (`<name>:cloud`, `<name>:<tag>-cloud`) Ollama's, any other id OpenAI's — the same rule as the
  price key. A Claude record belongs to the account named by the history's live and switch events, or
  else by the usage readings, as the paragraph "The account of a record" says. An OpenAI or Ollama
  record belongs to its company and to its
  provider subscription, never to a Claude account, whichever account was live. Its subscription
  value comes from that subscription's own window: OpenAI's `weekly_all` week, Ollama's
  `monthly_credits` month. The OpenAI subscription of a record is the `to` of the newest
  `providerSwitch` at or before it on its host, before the host's first switch that event's `from`,
  and on a host with no switch the only stored OpenAI subscription when there is exactly one; when
  several or none are stored, the subscription cannot be named and the value says so. Ollama has one
  subscription, its credit balance. A provider record's subscription value comes from its own
  window; a record with no price or no window reading has none, with the reason, never 0. Only a
  record with four zero token counts has the known subscription value 0. Auto-burn's work in API
  dollars (the rhythm, the rates and the cost of a switch, §2.3) counts Anthropic records only; so
  does the Forecast.
- The value of a subscription for a week: the week's used money, split among the account's token
  records of that week in proportion to their API value. A record's subscription value is `its API
  USD / the week's API USD × used money`. A record of another company never enters a Claude
  account's week: it adds no API money to the split, takes its own subscription's value instead, and
  never makes the Claude week's subscription value unknown. A finished window's use is `final −
  from` (`from` absent = 0), and a week's used percent is the sum of
  its windows' use, so a reset week's may pass 100 %; a running window's use is its latest reading's
  percent − `from`. A window above 100 % expired nothing, and a reset window expired nothing either.
  The paid and expired money are the Statistics area's "Used, paid and expired money":
  `paid = 100 × percent_value(price)`, `offered = paid × the number of windows` and `expired` is the
  sum of every window's `lost` at `percent_value(price)`, while `used` is the windows' summed use at
  one week's price.
- No record belongs to Google: seatswap has none of the work `agy` does (§7.9).
- A provider subscription's value is the same share of its own window: OpenAI's `weekly_all` week
  and Ollama's `monthly_credits` month. The value is `its API USD / the window's API USD × the
  window's used money`. For OpenAI the used money is the week's use at one week's price
  (`monthly price × 12 / 52 / 100`); for Ollama it is the share of the monthly credits used
  (`monthly price × used percent / 100`), and the month starts one calendar month before its reset.
  A window whose prices are missing, or which holds an OpenAI or Ollama model without a price, leaves
  the value out with the reason; it is never valued as 0. A provider subscription without a price has
  no subscription value, while its API value stays.
- The price used is the Statistics area's "The price of a window": the account's or provider
  subscription's monthly price at
  the window's midpoint, `start + (end − start) / 2`; the last `priceChanged` at or before the
  midpoint, the first change when the midpoint lies before all of them, or, when it has no
  such event, the price stored in `accounts.json` with its `priceCurrency`. A price entered in another
  currency than the current `currency` is not used, and the value is left out; a price without a
  currency does not count either. So is a record whose week has no reading, or whose account's
  records in that week have no API value to split.

`priceChanged` is written to the history when an account's or provider subscription's price changes;
it carries the new
`amount` and the `currency` in effect at that moment. The same amount saved again in a new currency
is a change, so it is recorded with the new currency. A plan change is `planChanged`.

**The files.**

`<data folder>/prices.json`, pretty JSON, atomic, mode 0600:

```json
{
  "version": 1,
  "checkedAt": "2026-11-15T09:41:12Z",
  "etag": "W/\"1a2b3c\"",
  "lastChange": {
    "at": "2026-11-15T09:41:12Z",
    "added": ["anthropic/claude-opus-5-5"],
    "changed": [],
    "removed": []
  },
  "models": {
    "anthropic/claude-opus-5-5": [
      { "from": "2026-11-14T08:00:00Z",
        "price": { "input": 4.0, "output": 20.0, "cacheWrite": 5.0, "cacheRead": 0.2, "tiered": false } }
    ]
  },
  "missing": ["openai/gpt-6"]
}
```

`<data folder>/fx.json`, the same way:

```json
{ "version": 1, "currency": "EUR",
  "points": [ { "date": "2026-11-13", "rate": 0.90481, "fetchedAt": "2026-11-13T10:10:00Z" } ] }
```

`checkedAt`, `etag` and `lastChange` are left out of the file rather than written as null;
`models` and `missing` are always present, empty when nothing is there. A file of another
`version` is refused with its path, never replaced.

---

## 11.6 Projects and organizations

A project is a repository, or a folder that has none. Organizations group projects and the accounts
that pay for them. Both are the same on every host once the hosts' project logs have met.

**What names a project.** seatswap reads only `.git` and the git `config` of the folders Claude Code
reports; it executes nothing. From a folder it walks up to the repository root and reads the
`origin` remote. The project's identity is that address, normalised: host lower-cased, no protocol,
no user or token, no port, no `.git`, no trailing `/` (`github.com/acme/atlas`). Without a remote it
is the repository's folder name; outside a repository, the first folder under the home folder, or
`~` for the home folder itself. The key is `repo:<host/path>` or `folder:<name>`. A repository key
is the same on every host; a folder key also names its host (`{"kind":"folder","id":"notes",
"host":"atlas"}`), because a folder project is per host until it is joined by hand.

**Names.** A project's default name is the repository's last path part (`atlas`), the folder's name,
or `~ (home folder)` for `~`. A folder project's default name stays its folder name; the page shows
the host beside it in the folders column. A rename is 1 to 40 characters and may not be only
spaces. A renamed project keeps its key, so a later rename wins and the past keeps counting under
the same project.

**Organizations.** A project's default organization is the repository's owner (`acme` for
`github.com/acme/atlas`); a folder has none. An account's first organization is its login's
`orgName`; without one it has none. An account sits in exactly one organization. Each change of an
account's organization is one `accountOrg` record for that account; the latest such record, in log
order, puts the account in the root of its organization, or in none. Nothing else moves an account,
so moving one account never drops another — even one only another host stores. Organizations have
an identity, which a rename does not change, and a shown name, which starts as that identity.

The page shows one heading per organization, its projects below it, and `No organization` last. A
project's *Move to* menu lists the other organizations and `No organization`; choosing one writes
an `assign` at once, which moves that project alone, also when it came from a repository. An
organization's *Merge into* menu lists the other organizations; the choice asks first, saying that
the merged organization's projects and accounts count for the target and that the merged
organization stops being a row of its own. It writes `orgJoin`. The merged organization's projects,
accounts and later repositories count for the target; the target's row names it under "merged in"
with a *Split off* link. *Split off* asks first and writes `orgUnjoin`; the latest `orgJoin` stops
applying, an `assign` made after the merge keeps the organization it names, and the split-off
organization gets its old projects and accounts back. *Split off* on an organization that is not
merged now is refused with `the organization {org} is not merged into another`. An organization's
name field saves on Enter or when it loses focus; its tooltip reads
`The shown name; the projects stay linked to {id}.`

An organization that comes from a repository owner is never removed (R1). Its row has no Remove
button and its tooltip says why; the attempt is refused with `the organization {name} comes from
the owner of {repository}; it cannot be removed`. When all its projects were moved away it stays as
a row in Settings, dimmed and marked `empty: its projects were moved; not shown in Statistics` when
it has no account either; Statistics → Projects
leaves an empty organization out. An organization whose name only another host's history names is
shown all the same, as an empty row.
Organizations can be added and removed by hand: an added one is a log record like the others, and a
removed one asks first, saying that its projects and accounts go back to none, and then sends them
back. Names that differ only in case are one organization, also in a
1.1.0 history, because a GitHub owner is case-insensitive. The spelling shown comes from the shared
history — the session records and the project log, oldest first — so every host shows the same name;
a login's own spelling is used only when nothing shared names it. A project's row shows its tokens
and sessions of all time; an organization's row shows the same for all its projects.

**Sessions and folders.** A project's folders per host and its session count come from the session
records of every host, over all time: distinct session ids, and the folders the sessions ran in. A
key that no session ever named is not a row of the Projects group. A project seen on two hosts in
two folders is one project; the folders are listed per host. A folder project seen on two hosts is
two projects until they are joined.

**The shared project log.** Every edit is one line in the host's own
`projects-YYYY-MM.jsonl` under the history root, appended and never rewritten. Every host reads all
hosts' files; the later record wins, by its time, ties broken by host name and then by the file's
line order. A join follows its chain; a cycle keeps the later of the records that form it and drops
the earlier one. Because the log travels with the history folder (§1.5), the same names, joins and
organizations appear on every host once the folder has synced. A refused edit appends nothing; the
row shows the reason.

| `edit` | Fields | Meaning |
|---|---|---|
| `rename` | `key`, `name` | the project's shown name |
| `join` | `key`, `into` | `key`'s tokens count for `into`; `key` gets no row of its own |
| `unjoin` | `key` | undo a join |
| `assign` | `key`, `org` (an identity, or `null` for none) | the project's organization; creating an organization is an `assign` with a new name |
| `orgRename` | `org`, `name` | an organization's shown name; `org` stays its identity |
| `orgJoin` | `org`, `into` | `org`'s accounts and projects count for `into` |
| `orgUnjoin` | `org` | `org`'s latest `orgJoin` stops applying; it is a row of its own again |
| `orgAdd` | `name` | a new organization, named by hand |
| `orgRemove` | `org` | the organization goes; its projects and accounts go back to none |
| `accountOrg` | `account`, `org` (an identity, or `null` for none) | one account's organization |

A shown project or organization name is 1 to 40 characters and not only spaces. An organization
identity is only checked to be non-empty; it comes from a login's `orgName` or a repository owner, or
from a name typed by hand, which is trimmed and must be unique. A project or an organization cannot be
joined into itself. A folder project id
names its host; a repository id does not, because a repository is one project on every host. The
Projects group shows each project with its repository, its folders per host, its organization
(chosen from the list or created in place), its tokens and sessions, and the keys joined into it,
each with an undo. Under it, the organizations with their source, accounts, projects, tokens and
sessions. A 1.3.0 host skips an `orgUnjoin` line with a warning instead of failing on it. Changes
apply to the past too, because the history stores the repository's identity, not the shown name.

**The file.** One line per edit, camelCase, the project id as `{"kind":"repo","id":"…"}` or
`{"kind":"folder","id":"…","host":"…"}`:

```json
{"t":"2026-11-15T12:25:05Z","edit":"join","key":{"kind":"folder","id":"atlas-old","host":"atlas"},"into":{"kind":"repo","id":"github.com/acme/atlas"}}
{"t":"2026-11-15T12:30:00Z","edit":"assign","key":{"kind":"repo","id":"github.com/acme/atlas"},"org":"acme"}
{"t":"2026-11-15T12:31:00Z","edit":"orgRename","org":"acme","name":"Acme"}
{"t":"2026-11-15T12:32:00Z","edit":"accountOrg","account":{"acc":"6f1c2a9e-…","org":"a92d7b31-…"},"org":"acme"}
{"t":"2026-11-15T12:33:00Z","edit":"accountOrg","account":{"acc":"6f1c2a9e-…","org":"a92d7b31-…"},"org":null}
```

---

## 12. Later (v2+)

- In-app login for new accounts (OAuth PKCE), so `/login` in Claude Code is no longer needed.
- Claude Desktop: research whether its login can be switched safely (separate `--user-data-dir` per account and swapping which one the launcher uses is the likely route).
- Usage history: a small sparkline per account across the last 7 days, from the poll cache.
- Native Cinnamon, GNOME and KDE front ends (§9.3).

---

## 13. Security and privacy

- Logins go only into the OS keyring, or a `0600` file if there is none. They are never logged; log lines print the account ID and the token's last 4 characters at most.
- Network traffic goes only to `api.anthropic.com` and `platform.claude.com` for an account's usage
  and login; to `chatgpt.com/backend-api/wham/usage` while the Codex card is on or a Codex login is
  found, and to `auth.openai.com/oauth/token` for a saved login that needs renewal; to the Ollama server
  at `ollamaUrl` for `/api/balance` and `/api/usage?range=24h` while the Ollama card is on, the
  `ollama` program is found on `PATH`, or this host stores an Ollama subscription; and — only
  while `lookUpPrices` is on — to `models.dev` for API prices and `api.frankfurter.dev` for the dollar
  rate (§11.5). The two look-ups send the User-Agent `seatswap/<version>` and no account, token or
  machine data. The provider requests follow no redirect: a 3xx answer is an error naming where it
  pointed, and no other host is asked. No telemetry, no update pings unless enabled.
- **The two provider cards' requests.** The Codex card sends one GET to `chatgpt.com` and carries
  Codex's own access token and account id only to that service; the token is never logged, and
  seatswap never renews the live login (Codex does). A saved inactive login is renewed once at
  `https://auth.openai.com/oauth/token`, its own issuer, with that login's refresh token; a login
  from another issuer is not renewed. The Ollama card sends two GETs to the Ollama server and carries no
  secret: the local server signs them with the machine's own key. Neither follows a redirect: a 3xx
  answer is an error naming where it pointed, and nothing is asked there. Each card is off by
  default; while it is off the pop-up shows no card, but a found provider is still polled and its
  reading recorded, and no request generates text (§7.9). A Codex switch writes
  `auth.json` from seatswap's stored copy, under seatswap's app lock and Codex's two locks; nothing
  else in Codex's files is written. The Sessions area is separate: with *Read sessions* on it reads
  Codex's rollout files whatever the card says, as the next point describes.
- **What seatswap reads for the Sessions area.** Claude Code's session files and transcripts
  (session id, folder, model, times, token counts, agents and states); DeepSeek Harness's session
  files (`$DSH_HOME/sessions` or `~/.dsh/sessions`: job id, folder, model, tokens, state) and
  flash-agents' state (`$FLASH_DATA_DIR`, else `%LOCALAPPDATA%\flash-agents` on Windows, `~/Library/Application Support/flash-agents` on macOS, `$XDG_STATE_HOME/flash-agents` or `~/.local/state/flash-agents` elsewhere (flash-agents' own rule, its `durableBaseDir`): the report
  that links a short job id to its flash task); and Codex's rollout files under the Codex folder
  (`$CODEX_HOME` or `~/.codex`: session id, model, token totals, event times) and its
  `session_index.jsonl` beside them (only each thread's `id`, `thread_name` and `updated_at`). None of these files is
  written or created. A Claude Code session's title (the one VS Code and `/resume` show: the
  `customTitle` of its transcript's last `custom-title` line, the name the user gave it with
  `/rename`, else the `aiTitle` of its last `ai-title` line; a rename shows within a pass) is stored in its session records and the live report, so
  it travels with a shared history folder; a DeepSeek Harness session that is a row of its own, not
  a flash job, carries its title (its last `session/title` event) in the live report too, and a
  Codex row its thread name. A flash worker's task
  text and all message text are never stored; a worker's title, shown in the window as a linked
  agent's task, is cleared before the report other hosts read is written. None of what is read is sent anywhere.
- **The history folder suggestion.** Only while the saved settings name no history folder, seatswap
  reads Dropbox's own `info.json` (macOS and Linux `~/.dropbox/info.json`; Windows
  `%APPDATA%\Dropbox\info.json`, then `%LOCALAPPDATA%\Dropbox\info.json`: `personal.path`, else
  `business.path`) to suggest a history folder. A missing file is no suggestion; a broken one is an
  error naming the file, shown in Settings. It is never written or created, and nothing it holds is
  sent anywhere.
- Every write to Claude Code's files is atomic (temp file + rename) and keeps the original file mode;
  a file that is a symbolic link stays a link and its target is rewritten.
- The status line reads Claude Code's JSON, seatswap's settings and its usage cache; its only write
  is `<data folder>/feed.json` (mode 0600), plus `<data folder>/statusline-keys.json` while a
  recording runs — the key paths and JSON types of the payload, never a value. It prints no token
  and sends none.
- **seatswap's one write in Claude Code's settings:** *Use in Claude Code* writes exactly one key of
  `<config>/settings.json`, `statusLine`, and nothing else in that file changes: every other key
  keeps its value and its order. The file is backed up once (`settings.json.seatswap-backup`), the
  change is shown before it is made, and *Restore previous* puts the exact earlier value back or
  removes the key (§11.4).
- Before seatswap first writes to them, it backs up `.credentials.json` and `.claude.json` once as `*.seatswap-backup`.
- seatswap records the Claude Code version it was tested with. If the installed version is newer, the pop-up shows a warning, because the rules in §2.6 may have changed.
- Each account must be a licence the user holds. The app moves the user's own logins between the user's own slots and shares nothing.

---

## 14. Phase 0: what was verified, what is left

Run on 2026-11-10 with two real accounts in a separate config folder. Evidence and scripts: `phase0/RESULTS.md`.

| # | Item | State |
|---|---|---|
| 1 | End date of a login | **Done.** Fixed; a refresh does not move it (§2.1, §6.3). |
| 2 | Round trip with two accounts and a session running | **Done.** Works, also in the middle of a turn (§6.5). |
| 3 | Claude Code's locks | **Done.** Three locks; Claude Code waits for each (§2.6, §6.2). |
| 4 | Organization caches | **Done** for Team ↔ personal: Claude Code handles them (§2.4). Open: two Team or Enterprise organizations. |
| 5 | Usage endpoint | **Partly.** Works with a refreshed login; a limit can be missing (§3). Open: which scope it needs, its rate limit, the values `severity` takes near and at 100 %. |
| 6 | macOS | **Partly.** The Keychain names from §2.2 are confirmed, and a switch by seatswap's command line updates the item in place without a prompt (2026-11-21, a throw-away config folder). Open: a running Claude Code session following a switch on a Mac (it keeps its copy for up to 30 s). |
| 7 | Windows | **Done** (checked 2026-11-11, Claude Code 2.1.322 on Windows 11): `%USERPROFILE%\.claude\.credentials.json` and `%USERPROFILE%\.claude.json`, the same keys and format as on Linux; nothing in Windows Credential Manager; the same lock folders. Port: `docs/windows.md`. |
| 8 | Tauri tray on Cinnamon | **Done** with the `ksni` backend: the icon updates live, left click opens the pop-up at the mouse, right click shows the menu, the tooltip shows. The label next to the icon does not show (§8.1). Open: whether an open menu refreshes its text. |
| 9 | Sessions inside VS Code | Expected to follow a switch like a terminal session (experience from daily use, §6.5). Test postponed until no work sessions are running. |

## 15. Rough estimate (a guess, not measured)

- Phase 0: done on 2026-11-10, except the open items in §14.
- Linux v1 (core, CLI, tray, pop-up, Mint/KDE/GNOME testing): about 1 week.
- macOS + Windows (Keychain, packaging, signing): about 1 more week.
- One developer with Claude.

## 16. Remaining risks

| Risk | What it would mean | What the design does |
|---|---|---|
| A login has a fixed end date (§2.1, confirmed) | The app cannot keep logins alive; it warns before a login ends. Each account needs `/login` about every 4 weeks. **Accepted 2026-11-10:** this is the same as today without the app. | The warnings in §6.3. The in-app login stays in v2 (§12). |
| Every refresh uses up the old tokens (§2.1) | If seatswap refreshes an inactive login and cannot save the answer (crash, keyring error), that account is logged out and needs `/login`. | Save first (§6.3). A setting can turn the refresher off. |
| Undocumented endpoints, and seatswap calls the token endpoint with Claude Code's client ID | Anthropic can change or block this in any release. Refreshing a login from another program may also be against the terms of use; this is not checked. The worst case is that an account gets flagged. | All endpoint code sits in one module. Only inactive logins are refreshed. A setting can turn the refresher off; then inactive accounts show their last known usage with its age. |
| Everything marked *(code)* or *(tested)* comes from one version of the program (2.1.322) | A later Claude Code version may lock, re-read or save differently, and a switch could then lose a login. Several versions can also run at once: on this machine the VS Code extension brings its own 2.1.321. | seatswap records the Claude Code version it was tested with and warns when the installed version is newer. The backup from §13 is kept. |
| A switch moves all running sessions (§6.5) | A long agent run continues on the other account, with its settings and limits. | The pop-up says so before the switch. Per-terminal accounts are a non-goal (§1). |
| Claude Code can move the Windows login into Windows Credential Manager (`docs/windows.md` §13) | Version 2.1.322 has this store behind a server-side flag (`tengu_windows_credman`, off on the test machine on 2026-11-11). With it on, Claude Code reads the login from Credential Manager and deletes `.credentials.json` after its first save there; a switch on Windows then changes nothing. | seatswap reports "no login" instead of failing silently. Support for that store is not built; it is the Windows counterpart of the macOS Keychain work (§2.2). |
| The Codex usage endpoint is internal and not documented by OpenAI (§7.9) | OpenAI can change or remove it in any release; the card would then stop showing numbers. | The endpoint lives in `api.rs` beside seatswap's other endpoints; it is the only Codex endpoint. A changed answer shows the API error text on that card, and nothing else — no switch, no stored account, no Claude reading — is affected. The card is off by default. |
| A Codex login does not fit one Windows Credential Manager entry (`docs/windows.md` §15) | Credential Manager holds 2560 bytes per entry; a Codex login is about 7,800 bytes as UTF-16, so on Windows the save fails and Codex cannot be switched (worked out from the sizes, not run on Windows). | The OpenAI card still shows the live login's usage, and its row says `This login is not saved: {error}`. How to store it on Windows is not decided. |
| Linux tray limits (§8.3, §9.2) | A desktop without a StatusNotifierItem host shows no icon at all; on Wayland the pop-up cannot sit at the icon. | The menu and `seatswap panel` reach the pop-up too. Native applets later (§9.3). |

## 17. Known issues

From daily use on Linux Mint 22 Cinnamon (X11) and the headless test runs, as of 2026-11-11.

**Fixed, kept here because the cause is easy to bring back:**
- *The pop-up did not close* (Esc, click elsewhere). Muffin, Cinnamon's window manager, refuses a focus request without a timestamp and ignores the first one while the window is still being mapped, so the pop-up never had the keyboard focus. Now it asks with the X server's time and repeats the request after mapping until it has the focus (`panel::focus_when_mapped`).
- *The pop-up flashed white at its old place before appearing.* It was shown first and moved and resized afterwards. Now it is built hidden at startup and only shown once the page has measured its height and it sits at its final place (`pending_show`, see the panel section of `docs/implementation.md`).
- *A second start did not find the first* when `--data-dir` named the same folder differently (relative and absolute): the socket's name was a hash of the spelling. Now it is a hash of the folder's real path (`AppPaths::gui_socket`).
- *The reset-time rule existed twice*, in Rust for the tray and the CLI and in the pop-up's JavaScript, and the suggestion's clock time already differed (no weekday beyond 24h). The pop-up now asks the Rust side for every reset text, and since 2026-11-11 also for its "12s ago" ages (`panel::time_texts`, `format::format_age`).
- *Picking a value in a settings drop-down closed the pop-up.* WebKitGTK shows the list as its own window that holds the input, so the pop-up lost the focus for as long as the list was open; the 150 ms recheck could not cover that. Now focus loss is ignored while a pop-up of seatswap holds the input (`panel::own_popup_open`). Reproduced and checked headless with Muffin, 2026-11-11.
- *Without a tray on the desktop the app crashed at startup* (no StatusNotifierWatcher on the session bus: GNOME without the AppIndicator extension, or the desktop's panel starting after seatswap at login). Now it keeps running without an icon, says so, and shows the icon as soon as a tray appears (`tray::start`); `seatswap panel` works meanwhile.
- *At every login on Linux Mint a "no system tray" notification appeared*, although the icon came a moment later. Cinnamon starts its tray (`xapp-sn-watcher`) and seatswap's autostart entry in the same second, so seatswap's first try often comes before the tray. Now it waits 30 s for a tray before it notifies (`tray::TRAY_GRACE`); the icon still appears the moment the tray does. Checked headless with a tray that comes 3 s late (no notification) and with none (notification after 30 s), 2026-11-11.
- *Inactive accounts kept an old plan label.* The plan was only recomputed when an account was live; the refresh of inactive logins skipped it. All three places that store a login now go through `Account::note_stored_login`.

**Not reproducible any more** (headless runs of 2026-11-11 with Muffin, with Metacity and with no window manager; both were seen before the focus fix above and were most likely the same cause):
- *Opening the pop-up while it is open closed it* (`seatswap panel settings` on an open pop-up). Now it stays open, switches the view and moves to the mouse; `seatswap panel` twice does the same.
- *The pop-up closed as soon as the mouse left it.* Now it stays open until a click elsewhere, Esc or ×.

**Open:**
- No built-in global shortcut (§8.3).
- The separator characters `│`, `·`, `•` and `›` are East Asian "ambiguous" width. A terminal that
  draws them two columns wide makes the line one column wider per separator than seatswap counted
  (§11.4). `ASCII` separators `/` and the blank are unaffected.
