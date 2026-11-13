---
name: seatswap — specification
status: Draft
created: 2026-11-10
updated: 2026-11-11
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

**Non-goals (v1)**
- Switching the **Claude Desktop** app. Its login is a Chromium cookie, stored encrypted in `~/.config/Claude` (Linux) or the matching folder elsewhere, so swapping files is not reliable there. Revisit later (see §12).
- Switching claude.ai in the browser. Browser profiles already do that.
- Choosing the account automatically. v1 only *suggests* a switch (see §7.4).
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
- Not tried on a Mac yet *(Phase 0)*.

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
      "email": "…", "orgName": "…", "plan": "Team",
      "oauthAccount": { "...": "copy of ~/.claude.json oauthAccount" },
      "addedAt": "2026-11-10T12:00:00Z", "lastUsedAt": "…" },
    { "id": "a92d…", "label": "Private", "abbr": "P", "color": "#f59e0b", "...": "…" }
  ],
  "settings": { "...": "see §10" }
}
```

**Secrets.** Each account's login keys (§2.1) go into the OS keyring as one entry:
- Linux: Secret Service (GNOME Keyring or KWallet).
- macOS: Keychain.
- Windows: Credential Manager.

They use the `keyring` crate, with service `seatswap` and the account `id` as the user. If no keyring is available (a headless Linux box, or a minimal window manager without Secret Service), the fallback is `secrets/<id>.json` with mode `0600` in a `0700` folder. The UI then shows a warning that the logins are stored unencrypted.

**Identity.** An account is identified by `accountUuid` + `organizationUuid`. The same person in two organizations counts as two accounts, which is exactly the two-licence case.

**Usage cache.** `usage-cache.json` holds the last response per account and when it was fetched. The pop-up opens at once from the cache and refreshes in the background.

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
  update tray icon, notify "Switched to P — Private (12 % · 5 h)"
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
- The pop-up shows "login ends in N days" for every account.
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

---

## 7. Usage

### 7.1 Polling

| Account | Interval | Also |
|---|---|---|
| Active | 2 min | right after a switch; when the pop-up opens (if older than 30 s) |
| Inactive | 15 min | when the pop-up opens (if older than 5 min) |

- On an HTTP 429 or 5xx, back off exponentially up to 30 min and show the time of the last fetch.
- On a 401: for the active account, read the live file again (Claude Code may have renewed it). For an inactive account, refresh (§6.3).
- No polling while the machine sleeps. Poll once on wake.

### 7.2 What the app derives from the response

For each account it shows:
- **Session (5 h):** percentage and time until reset (`1h12m`, `35m`). Only if the reply has a session limit (§3).
- **Week, all models:** percentage and the reset: more than 48 h away the weekday and clock time (`Sun 02:00`); otherwise the time until it, `1d18h` from 24 h on, `5h40m` below.
- One rule for every reset shown (pop-up, tray menu, CLI): under 24 h hours and minutes, 24–48 h days and hours, beyond 48 h weekday and clock time. Minutes round up, and a reset time is shown rounded up to the full minute: the server sends a moment just before the boundary (`00:59:59.59`), which is meant as `01:00`.
- **Week, scoped:** one row per scoped weekly limit (e.g. Fable), percentage only.
- **Binding limit:** the one with `is_active: true`, or else the highest percentage. This drives the tray icon.
- **Extra usage / spend:** only if enabled.

### 7.3 Colours by load

| Load | Colour | Meaning |
|---|---|---|
| < 70 % | green `#22c55e` | fine |
| 70–89 % | amber `#f59e0b` | getting close |
| ≥ 90 % or `severity: critical` | red `#ef4444` | about to hit the limit |
| 100 % / locked | red + lock glyph | limit reached, shows the time until reset |

### 7.4 Switch suggestion

- When the active account's binding limit crosses **90 %** (setting) and another account is under **50 %**, send a desktop notification: "AC is at 97 % (resets 14:40). Switch to P (12 %)?" with a [Switch] action.
- If the platform's notifications don't support actions, clicking the notification opens the pop-up.
- A click on the notification's text opens the pop-up. A notification that times out or is dismissed does nothing.
- One notification per account per reset window.

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

- The icon is drawn at run time with `tiny-skia` and a bundled font, at each size the platform asks for (16, 20, 22, 24 and 32 px, and @2x/@3x). It is redrawn when the usage or the account changes. On Cinnamon a new icon every 0.5 s showed at once, at 1–2 ms per update *(tested)*.
- **Text next to the icon, where the platform shows it:**
  - macOS: the menu bar title.
  - Linux: the AppIndicator label. **Cinnamon does not show it** *(tested, Cinnamon 6.6.9)*: the app publishes the label, but Cinnamon's tray bridge (`xapp-sn-watcher`) hands an empty one to the panel. There the ring carries the number, and the tooltip once the app publishes one (below).
  - The text reads `97%`. A setting `trayText` offers `percent`, `percent+reset` (`97% · 1h12m`) or `off`.
- Tooltip: "AC · Acme — session 97 % (resets 14:40) · week 48 %".
- **High-contrast / monochrome option:** macOS "template" style, or a plain white icon for dark panels. The ring stays, the letters become cut-outs.

### 8.2 Abbreviations

- **Up to 2 characters**, so the label stays legible at 16 px (the Windows tray at 100 % scaling). Three characters don't fit in a 16–22 px disc at a readable size. Allowed: letters A–Z and digits, stored in upper case. One character is drawn larger.
- Default: the first letters of the first two words of the label ("Acme Team" → `AT`), or the first two letters of a one-word label ("Acme" → `AC`, "Private" → `PR`). The examples in this spec show `P` for the private account; that one was shortened by hand in the editor (§11.3).
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
  ● AC  Acme                97 % · 5 h   ✓
  ● P   Private             12 % · 5 h
  ───────────────────────────────────────
  Open panel…                       ⌃⌥C
  Add account…
  Settings…
  Quit
```

- Clicking an account switches to it.
- The usage text in the menu updates live.
- "Open panel…" opens the pop-up.
- An optional global shortcut (default Ctrl+Alt+C, configurable) opens the pop-up on every platform.
  - **Linux v1:** the app registers no shortcut itself. The person binds `seatswap panel` to a key in the desktop's keyboard settings; that command asks the running tray app to open its pop-up. It works the same on X11 and Wayland. A built-in shortcut would need Tauri's global-shortcut plugin, which is not in the project yet (and cannot grab keys on Wayland).

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
| `abbr`, `label`, `color` per account | from the account | edited in the pop-up |
| `trayText` | `percent` | `percent` · `percent+reset` · `off` |
| `iconLimit` | `binding` | which limit the ring shows: `binding` · `session` · `week` |
| `pollActive` / `pollInactive` | 120 s / 900 s | |
| `suggestAt` / `suggestTargetBelow` | 90 % / 50 % | switch suggestion, §7.4 |
| `notifyOnSwitch` | on | |
| `mirrorLive` | on | §6.4 |
| `refreshInactive` | on | refresh the logins of inactive accounts (§6.3). Off: inactive accounts show their last known usage with its age |
| `hotkey` | Ctrl+Alt+C | Linux v1: not used; bind `seatswap panel` in the desktop instead (§8.3) |
| `startAtLogin` | on | Linux: `~/.config/autostart/seatswap.desktop`; Windows: the `Run` key value `seatswap` → `seatswap-tray.exe` (§9.4) |
| `theme` | system | system · light · dark |
| `claudeConfigDir` | `$CLAUDE_CONFIG_DIR` or `~/.claude` | |

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
- **Gauges:** two concentric rings per account. The outer ring is the week, the inner the 5 h session; the week's percent is the large figure in the middle. The bars beside the gauge follow the same order: week, then session, then the per-model limits. On a plan without a session limit the inner ring is left out (no grey track) and the week's figure sits centred.
  - Rings fill with an ease-out animation (400 ms) when the pop-up opens and when values change.
  - Colours follow §7.3, drawn as a soft gradient along the arc, with a faint glow on the red state.
  - Clicking the gauge of an account that is not in use switches to it, like its Switch button. The pointer becomes a hand over it.
- **Account colour:** a 3 px accent bar on the card's left edge and the abbreviation badge.
- **Active account:** at the bottom. The card is raised (slightly lighter), with a subtle ring in the account colour and an "Active" pill.
- **Closing:** Esc, the × in the header, a click anywhere else (the pop-up loses focus), or a second left click on the tray icon. The pop-up must actually get the keyboard focus when it opens, or none of the first three work (on Cinnamon it did not at first, §17).
- **Motion:** after a switch, the cards trade places with a 250 ms spring animation. The ticking reset countdowns ("1h12m") update every minute without flicker. All animation is off when the system asks for reduced motion.
- **Keyboard:** ↑/↓ moves between accounts, Enter switches, 1–9 switch directly, `,` opens settings, Esc closes.

### 11.2 Mock-up

```
╭──────────────────────────────────────────────────────╮
│  seatswap                           ⟳ 12 s ago  ⚙  ✕ │
│                                                      │
│ ▌╭────╮  P   Private                       [ Switch ]│
│ ▌│ 31 │  …@example.com · Ultra                       │
│ ▌│    │  Week      ██████▏                31 %  Thu  │
│ ▌╰────╯  Opus      ██▍                    12 %       │
│                                                      │
│ ▌╭────╮  AC  Acme ✎                       ● Active   │
│ ▌│ 48 │  someone@… · Team                            │
│ ▌│ 97 │  Week      █████████▋             48 %  Sun  │
│ ▌╰────╯  Session   ████████████████████▉  97 % 1h12m │
│ ▌        Fable     ████████▋              43 %       │
│                                                      │
│  ⚠ AC is almost out. P has room — switch?   [Switch] │
│                                                      │
│  + Add account                       3 sessions open │
╰──────────────────────────────────────────────────────╯
```

### 11.3 Account editor (inline, opens on clicking the badge or the ✎ next to the label)

The ✎ shows while the pointer is on the card or the card has the keyboard focus. The ⚙ in the header is hidden while settings are open.

- Label (free text).
- Abbreviation: two boxes, live preview of the tray icon at 16, 22 and 32 px.
- Colour: 10 preset swatches plus a custom hex value.
- "Remove account", which asks first. It deletes the account from the keyring and list. The live login is never touched.

---

## 12. Later (v2+)

- In-app login for new accounts (OAuth PKCE), so `/login` in Claude Code is no longer needed.
- Claude Desktop: research whether its login can be switched safely (separate `--user-data-dir` per account and swapping which one the launcher uses is the likely route).
- Auto-switch, opt-in: switch by itself when the active account hits 100 % and no session is mid-turn.
- Usage history: a small sparkline per account across the last 7 days, from the poll cache.
- `seatswap statusline`: a short segment for the Claude Code status line (`AC 97%`).
- Native Cinnamon, GNOME and KDE front ends (§9.3).

---

## 13. Security and privacy

- Logins go only into the OS keyring, or a `0600` file if there is none. They are never logged; log lines print the account ID and the token's last 4 characters at most.
- Network traffic goes only to `api.anthropic.com` and `platform.claude.com`. No telemetry, no update pings unless enabled.
- Every write to Claude Code's files is atomic (temp file + rename) and keeps the original file mode.
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
| 6 | macOS | Open: the Keychain names from §2.2, and whether updating the item in place keeps Claude Code's access to it without a new password prompt. |
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
| Linux tray limits (§8.3, §9.2) | A desktop without a StatusNotifierItem host shows no icon at all; on Wayland the pop-up cannot sit at the icon. | The menu and `seatswap panel` reach the pop-up too. Native applets later (§9.3). |

## 17. Known issues

From daily use on Linux Mint 22 Cinnamon (X11) and the headless test runs, as of 2026-11-11.

**Fixed, kept here because the cause is easy to bring back:**
- *The pop-up did not close* (Esc, click elsewhere). Muffin, Cinnamon's window manager, refuses a focus request without a timestamp and ignores the first one while the window is still being mapped, so the pop-up never had the keyboard focus. Now it asks with the X server's time and repeats the request after mapping until it has the focus (`panel::focus_when_mapped`).
- *The pop-up flashed white at its old place before appearing.* It was shown first and moved and resized afterwards. Now it is built hidden at startup and only shown once the page has measured its height and it sits at its final place (`pending_show`, see the panel section of `docs/implementation.md`).
- *A second start did not find the first* when `--data-dir` named the same folder differently (relative and absolute): the socket's name was a hash of the spelling. Now it is a hash of the folder's real path (`AppPaths::gui_socket`).
- *The reset-time rule existed twice*, in Rust for the tray and the CLI and in the pop-up's JavaScript, and the suggestion's clock time already differed (no weekday beyond 24 h). The pop-up now asks the Rust side for every reset text, and since 2026-11-11 also for its "12 s ago" ages (`panel::time_texts`, `format::format_age`).
- *Picking a value in a settings drop-down closed the pop-up.* WebKitGTK shows the list as its own window that holds the input, so the pop-up lost the focus for as long as the list was open; the 150 ms recheck could not cover that. Now focus loss is ignored while a pop-up of seatswap holds the input (`panel::own_popup_open`). Reproduced and checked headless with Muffin, 2026-11-11.
- *Without a tray on the desktop the app crashed at startup* (no StatusNotifierWatcher on the session bus: GNOME without the AppIndicator extension, or the desktop's panel starting after seatswap at login). Now it keeps running without an icon, says so, and shows the icon as soon as a tray appears (`tray::start`); `seatswap panel` works meanwhile.
- *At every login on Linux Mint a "no system tray" notification appeared*, although the icon came a moment later. Cinnamon starts its tray (`xapp-sn-watcher`) and seatswap's autostart entry in the same second, so seatswap's first try often comes before the tray. Now it waits 30 s for a tray before it notifies (`tray::TRAY_GRACE`); the icon still appears the moment the tray does. Checked headless with a tray that comes 3 s late (no notification) and with none (notification after 30 s), 2026-11-11.
- *Inactive accounts kept an old plan label.* The plan was only recomputed when an account was live; the refresh of inactive logins skipped it. All three places that store a login now go through `Account::note_stored_login`.

**Not reproducible any more** (headless runs of 2026-11-11 with Muffin, with Metacity and with no window manager; both were seen before the focus fix above and were most likely the same cause):
- *Opening the pop-up while it is open closed it* (`seatswap panel settings` on an open pop-up). Now it stays open, switches the view and moves to the mouse; `seatswap panel` twice does the same.
- *The pop-up closed as soon as the mouse left it.* Now it stays open until a click elsewhere, Esc or ×.

**Open:**
- No built-in global shortcut (§8.3).
