# Phase 0 results (2026-11-10, Claude Code 2.1.322, Mint / Cinnamon 6.6.9, X11)

All tests ran in a separate config folder (`CLAUDE_CONFIG_DIR=~/cs-test`). The real `~/.claude` login was checked after every step (`tripwire.py`) and never changed account. Test logins are kept in `~/cs-test-vault` (0700); files named `.rotated-away` hold used-up tokens.

"Tested" means seen in a test here. "Code" means read from the 2.1.322 program only.

## Answered

| # | Question (spec §14) | Result | Evidence |
|---|---|---|---|
| 1 | Does a refresh move a login's end date? | **No.** The end date is fixed. Every refresh returns a new refresh token. | Tested on both accounts: 3 refreshes of the Acme login and 1 of the private login with `refresh.py`; the server's `refresh_token_expires_in` gave the same end date each time (moved 0.0 h). |
| 1b | Is the old refresh token still good after a refresh? | **No.** HTTP 400 `invalid_grant`, "Refresh token not found or invalid", about 20 s after the refresh. The login itself survives: the current token still refreshed afterwards. | Tested once. |
| 1c | Is the old access token still good after a refresh? | **No.** HTTP 401 within seconds. | Tested once. |
| 2 | Round trip with a session running | **Works.** One open session followed private → Acme → private → Acme, the last switch between two tool calls of one turn. No restart, no error, and it never wrote the old login back. | Tested. Proof is the cache pattern in the transcript: the first request after each switch read the other organization's cache (e.g. exactly 54,300 cached tokens, the point where the conversation stood on Acme 13 min earlier). `/status` is no proof: it reads the name from `.claude.json`. |
| 2b | Cost of the first request after a switch | The conversation is written into the new organization's cache: about 20k tokens the first time, about 3.9k when switching back (only what was added meanwhile). The ~35k standard opening was served from cache on both accounts. | Same transcript. |
| 3 | Claude Code's locks | **Three locks, all folders made with `mkdir`:** (a) renewal: `<config>/.oauth_refresh.lock`, then `<config>.lock` next to the folder, plus an owner note `.oauth_refresh.lock.owner`; (b) every write of `.credentials.json`: `<config>/.storage-write.lock`; (c) every write of `.claude.json`: `.claude.json.lock` beside it. Claude Code waits for each of them when someone else holds it. | Tested with `lockwatch.py` (inotify): order of creation seen during a renewal; with (a) held 30 s Claude Code did not renew and answered with the token it had; with (b) held 3 s a renewal waited and saved 0.25 s after release; with (c) held 3 s no `.claude.json` write happened until 0.15 s after release. Timeouts from code: (a) abandoned after 60 s, refreshed every 5 s, 5 tries 1–2 s apart; (b) abandoned after 15 s, 10 tries over about 7.5 s; (c) library default, 10 s. |
| 3b | How often is `.credentials.json` written? | **Often, and not only for renewals.** One `claude -p` start rewrote the whole file about 20 times in 2.5 s (MCP server entries), each time read–change–write under lock (b). | Tested. This is why a switch must hold (b): otherwise such a write can put the old login back or lose an MCP login. |
| 4 | Organization caches (`policy-limits.json`, `remote-settings.json`) | **Leave them alone.** They are signed for one organization and one account. The Team account checks them with the server at every start (HTTP 304) and a running session fetched new ones 12 s after a switch to it. The personal account does not use them at all and leaves the other account's files on disk. | Tested for Team ↔ personal. For two different Team/Enterprise organizations only the code is known: a file signed for another organization or account is rejected (`aud_mismatch`, `sub_mismatch`). |
| 4b | Account caches inside `.claude.json` | Claude Code's logout clears 18 cache keys there (code). The ones present in the test folder carry the account or organization they were fetched for, and Claude Code replaced them by itself: a running session wrote a new `clientDataCacheSlots` entry at its first request after a switch, a new session replaced `cachedArtifactRoster` at start. One untagged key (`additionalModelOptionsCache`) was not followed. | Tested (`cachestate.py`). |
| – | Which keys of `.credentials.json` are the login | Claude Code's logout deletes seven keys together: `claudeAiOauth`, `organizationUuid`, `trustedDeviceToken`, `trustedDeviceEnrolledAfterRefusalAtMs`, `enterpriseGateway`, `designOauth`, `cloudSessionsOauth`. A fresh `/login` and a renewal wrote only `claudeAiOauth`. The real file also has `organizationUuid` (equal to the account's organization); the code that writes it was not found. | Code + tested. |
| – | Lifetime of a login | Fresh `/login`: 27.5 days (Acme, Team) and 29.4 days (private, Max). Not a fixed 30 days; read the date from `refreshTokenExpiresAt`. | Tested. |
| – | Two logins of the same account | Exist side by side: a second private `/login` did not affect the real private login. | Tested. |
| 8 | Tauri 2 tray on Cinnamon | **Icon works:** drawn at run time, replaced every 0.5 s, shown live, 1–2 ms per update. **Label does not show:** the app publishes it (`XAyatanaLabel` = "45%"), but Cinnamon's bridge `xapp-sn-watcher` hands an empty label to the panel. Labels in this panel come only from programs that use XApp's own status icon. | Tested with `tray-spike/`: screenshots and D-Bus properties. |

## Smaller findings

- `platform.claude.com` rejects Python's default program name (User-Agent) with HTTP 403 "error code: 1010". A plain `claude-switch-phase0/0.1` works.
- The usage reply for the private account had `five_hour: null` and no `session` entry in `limits[]`, only weekly limits (seen 4 times over 40 min).
- This machine runs two Claude Code versions at once: the terminal uses 2.1.322, four VS Code sessions use the extension's own 2.1.321. Everything here was tested with 2.1.322 only.
- `switch.py` takes none of Claude Code's locks. That was safe only because nothing else used the test folder; the real switch must take them.

## Not done

- 5: which permission (scope) the usage endpoint needs, its rate limit, the `severity` values near 100 %.
- 6 macOS Keychain, 7 Windows.
- 8: whether an open menu refreshes its text live; whether the tooltip shows on Cinnamon; placing the pop-up at the mouse.
- A switch between two different Team/Enterprise organizations.
- Sessions on 2.1.321 (VS Code extension) following a switch. Daily use suggests they do; the test is postponed until no work sessions are running.

## Scripts

`common.py` shared helpers · `tripwire.py` real-login guard · `snap.py` save the test login · `switch.py` §6.2 by hand on the test folder (no locks) · `midturn.py` switch during a running turn · `refresh.py` refresh a saved, inactive login · `usage.py` read usage · `lockwatch.py` watch or hold Claude Code's locks · `cachestate.py` which account each cache belongs to · `tray-spike/` item 8.
