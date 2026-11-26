# Changelog

All notable changes to this project are documented here. The format follows
[Keep a Changelog](https://keepachangelog.com/en/1.1.0/), and the project uses
[Semantic Versioning](https://semver.org/).

## [Unreleased]

## [1.6.1] - 2026-11-26

### Changed

- The pop-up orders every account by its weekly reset, the active one too: the week that expires
  next stands on top. The active account no longer stays at the bottom.

## [1.6.0] - 2026-11-25

### Added

- Google is a subscription like the others: the account the Antigravity CLI (`agy`) is signed in to has
  a row in Settings → Accounts (short name, colour, price), its 5h and weekly use is recorded in the
  history under its email, and the statistics show its plan, price, weekly use and unused money. What
  `agy` works is not recorded, so its worth is unknown, and the totals that set work against price leave
  Google out. No switching and no auto-burn; seatswap still asks Google nothing.

### Changed

- *Show a Google card* only hides the card, like the OpenAI and Ollama switches; the numbers are still
  recorded.
- Several open `agy` sessions: an older session's numbers no longer replace newer ones.
- The line `agy` shows names the plan once (`Google AI Pro`, not `Google Google AI Pro`).
- When the history is shared, update seatswap on every computer: an older version takes the Google
  account for an unknown Claude account in its statistics.

## [1.5.5] - 2026-11-25

### Added

- A Google card in the pop-up: plan, 5h and weekly limits of the account the Antigravity CLI (`agy`) is
  signed in to. When `agy` is installed but not connected, the card offers **Connect agy**, which sets
  `agy`'s status line to `seatswap statusline` (backed up first; a status line of your own is never
  replaced). Display only: seatswap asks Google nothing and reads no Google login.

## [1.5.4] - 2026-11-24

### Changed

- The pop-up orders the accounts by their weekly reset: the week that expires next stands on top,
  the active account stays at the bottom. Keys 1–9 follow the new order.
- Each card's header shows the time left until the account's weekly reset after its name, in the
  status line's compact form (`3d 2h`, `14h 3min`).

## [1.5.3] - 2026-11-23

### Changed

- Auto-burn leaves an account at 95 % of its 5h window when another account's window has room,
  instead of waiting for 100 %. The window holds no quota of its own, so this costs nothing, and the
  usage figure trails the real use by a few minutes: Claude Code could hit the wall while seatswap
  still read 96 %. With no other window below 95 % auto-burn stays and the account works to 100 %;
  the week still counts as full only at 100 %. The notification says "A1's 5h window is nearly
  full; it resets at 17:00".
- Auto-burn also sees quota at risk when several accounts reset close together. Each could have
  room alone, but work goes to one at a time: two accounts that reset together and each need 20 of
  the 37 working hours left lose 3 hours of quota between them. Auto-burn now adds up what the
  accounts resetting by the same time need and switches early when their shared room is 8 typical
  working hours or less.

### Fixed

- An unused Codex subscription no longer records a finished week and a finished 5h window at every
  poll. Its reset time moves a few minutes later with each answer, and seatswap took each move for
  a window that had ended. Statistics showed hundreds of fake weeks: on the Subscriptions page
  thousands of euros "expired" on a ChatGPT subscription. The records already written are left out
  when the history is read, and the statistics pages say how many.
- Statistics → Subscriptions with a range whose only week still runs ("This week"): the headers say
  "the windows of this week that have finished" instead of "the last 0 weeks".
- Statistics → Subscriptions no longer calls a plan "steady" from too few weeks. Telling a change
  needs 10 weeks; with fewer, "Over the weeks" shows "—" and the 1 % chart says "Too few weeks to
  tell a change: 7 of the 10 it needs."
- The plan name in a "Plan sizes" line ("Team Premium") is no longer cut off.

## [1.5.2] - 2026-11-23

### Changed

- A switch by hand no longer holds auto-burn back for 4 hours. Auto-burn stays on the account you
  chose until it is full or another account is at risk of losing quota; once such an account's
  5h window resets, it switches to it. To keep your choice, pause auto-burn. Settings → Auto-burn
  no longer has the line "After a switch by you".
- The shortest time between a switch and an early switch after it is 5 minutes instead of 30. It
  only guards against a runaway; no planned switch comes sooner.

## [1.5.1] - 2026-11-23

### Fixed

- Auto-burn now moves to an account at risk of losing quota at its weekly reset: one with 8
  typical working hours of slack or less, so time with the machines off does not cost its quota.
  It does so also while other early switches do not pay, and goes back to it once its 5h window
  has reset. Before, it stayed on the active account until that was full and let the other
  account's quota expire. The 30-minute dwell and a manual hold still apply.
- The Forecast's "only when full" figures and the suggestion's "Staying on … is expected to let
  … expire" sentence keep meaning the plan that switches only at 100 %.

### Changed

- The regime's words `switching before 100 % does not pay` and `switching before 100 % pays` are
  now `switches early only for quota at risk of expiring` and `switches early whenever it pays`;
  Settings → Auto-burn says `no other early switches` while early switching is off.

## [1.5.0] - 2026-11-22

### Added

- macOS support. Claude Code keeps its login in the Keychain there; seatswap reads and updates
  that item (`Claude Code-credentials`, or its hashed name for another config folder) in place
  with `/usr/bin/security`, as Claude Code does, so macOS asks nothing and Claude Code keeps its
  access. The first write keeps a copy in `<service>.seatswap-backup`. The Keychain cannot be
  watched, so the tray app also looks at the login every 30 s.
- `SEATSWAP_CLAUDE_LOGIN=file` keeps the login in `.credentials.json` on macOS too, for a Claude
  Code that falls back to that file; `keychain` (macOS only) names the default there.
- macOS: running Claude Code sessions are told by `ps` (the start time Claude Code writes), the
  host name is the Bonjour name (`scutil --get LocalHostName`), the tray app has no Dock icon, and
  start at login is a LaunchAgent in `~/Library/LaunchAgents`.
- Releases include `seatswap-<version>-aarch64-macos.tar.gz` (Apple silicon, not signed), and CI
  runs on macOS.

### Fixed

- The login watch noticed nothing when it saw a folder by another name: macOS reports the real
  path (`/private/var/…` for `/var/…`, the target of a linked config folder).
- Clippy 1.95 warnings (`drop` of values without `Drop`, a negated `is_some`).

### Known issues

- macOS is new and little tested: the tests and a switch by the command line ran on a Mac, but the
  tray app, its pop-up and notifications, start at login, and a running Claude Code session
  following a switch have not been tried there.
- macOS: seatswap needs `$USER` and uses it as it is. Claude Code falls back to the system's user
  name without it, and uses `claude-code-user` for a name with characters other than letters,
  digits, `.`, `_` and `-`; for such a name seatswap looks at another Keychain item.
- macOS: a login over about 2 KB (several MCP logins, for example) is passed to `security` on its
  command line, so it is visible in the process list while that command runs.
- All systems: `CLAUDE_SECURESTORAGE_CONFIG_DIR`, with which Claude Code moves its login and that
  login's write lock, is not supported. With it set, do not use seatswap to switch.

## [1.4.5] - 2026-11-21

### Changed

- Sessions, By project: a project's sessions are listed directly under it, without the line
  "Claude Code · Opus 5.5 · 2 sessions" per provider and model; each session row already shows its
  model. By model is unchanged.
- Sessions: the name column's heading names the levels of the order shown, "Project · session ·
  agent" or "Provider · model · session · agent", instead of one heading for both.

## [1.4.4] - 2026-11-20

### Fixed

- A session renamed in Claude Code (`/rename`) shows its new name in Sessions, in the waiting
  notification and, from its next session record on, in closed rows and Statistics → Activity.
  seatswap read only Claude Code's automatic title; it now names a session as Claude Code does: the
  name you gave it, else the automatic title. A rename shows within a minute, within seconds
  while the window shows Sessions.

## [1.4.3] - 2026-11-19

### Changed

- Sessions: a session's agents are listed directly under it, without the line "Claude Code · Sonnet
  5.5 · 1 agent" per provider and model; each agent row already shows its model. An agent's own
  agents are shown unless you fold them; the completed agents stay folded.
- A session with a title shows the title alone, without its label (`atlas-a6`) as a grey tag; the
  label is in the hover text. The same on Statistics → Activity's largest sessions.

## [1.4.2] - 2026-11-19

### Fixed

- The tray app no longer grows to several gigabytes of memory over a day. The hidden pop-up drew
  every update, and WebKit kept each drawing until the pop-up was next opened, waiting to run its
  ring and card animations. The hidden pop-up now keeps only the newest figures and draws them when
  it opens.
- Updates are sent to the pop-up and the window only when something changed. While Claude sessions
  work and the window shows Sessions, that was several identical updates a second.

## [1.4.1] - 2026-11-18

### Fixed

- Statistics → Subscriptions, "What does each plan give me?": a full limit is now shown as hours of
  your actual work and as tokens, `52h of work · 3.1B tokens`. An hour of work is an hour of the
  last 4 weeks that holds work with the company's models, at its average cost and tokens, all models
  mixed. The per-model hours are gone: they spread a model's work over all four weeks, so a model
  used in one week only (Ultra: "509h Fable") looked three to four times as long. The split by model
  is the "Tokens a week" card's. "Based on", "1 % ≈ N min of your work" and "A full 5 h window holds"
  use the same hours.

## [1.4.0] - 2026-11-18

### Added

- Settings → Projects shows one heading per organization, with its origin in words, merged
  organizations, Split off, empty organizations dimmed, and tokens and sessions of all time;
  "No organization" comes last. Move to moves a project at once; Merge into and Split off ask first.
- Statistics → Projects splits every row's bar by subscription, with work without a known account
  grey and striped, and shows an organization × subscription table whose row, column and total
  figures keep the part that is known.
- The Overview's This-week card shows tokens, money used and API value, the top eight projects split
  by subscription with Show all, and each subscription's running window.
- The tray app writes its Linux menu entry and icon under `$XDG_DATA_HOME` when they are missing or
  differ, with the same command as autostart. A symbolic link there, and an entry it did not make,
  are left alone and named once.
- The Activity page sums its top row over all hosts, with each host in the tooltip, and draws the
  strip under the hour bars as a heatmap of working sessions.

### Changed

- A sum keeps its known part when another part is missing and is shown with `≥`; a share, ratio or
  percent still needs a complete value. A share of an API value is the exception: it counts priced
  work only, so one model without a price no longer empties every share on Statistics → Projects.
  The page says "shares count priced work only".
- Codex and Ollama are polled when found by themselves: Codex when its login file exists, Ollama when
  the program is on `PATH` or this host has an Ollama subscription. A found Codex login is saved. A
  stopped Ollama says `Ollama is not running.` quietly.
- Auto-burn's factor rests on its own measured work from 100 USD on; below that the assumed prior
  still weighs as 100 USD.
- Rhythm's working and blocked hours are per weekday over the whole local days that ended in the
  range, and a weekday that has not ended shows `—`; the bars share a 0–24 h axis that grows to 25 h
  only when the clocks go back.
- Statistics → Subscriptions: a subscription without a price says `no price yet · set it`, and the
  link opens its price field.
- Settings keeps its scroll position on a redraw; `#settings/accounts?<id>` opens and focuses an
  account's or subscription's price.
- Small token counts are lighter but keep at least 3:1 contrast.
- Sessions: a flash job with one worker and no subagents draws one row, with no worker row or fold.

### Fixed

- Windows: Codex logins are kept as private files in seatswap's `secrets` folder, because a login is
  larger than one Credential Manager entry (2,560 bytes). Codex switching works there now.
- Subscriptions → Tokens a week: the note says the figures are fitted from all work up to now each
  time the page opens; it said "Refit every week", which they never were.

## [1.3.0] - 2026-11-18

### Added

- Auto-burn measures work in API dollars: every token line is priced at its model's models.dev
  price, and each account and limit learns one factor (percent per API dollar) with an 80 % range.
  An account on Max, Team Premium, Ultra or Pro starts from a factor assumed from its plan.
- OpenAI (Codex) logins are saved as they are seen and can be switched in the pop-up, in Settings →
  Accounts and with `seatswap codex list` / `seatswap codex switch`. An inactive saved login renews
  itself once at OpenAI's token endpoint. Codex sessions already running keep the login they started
  with.
- OpenAI and Ollama subscriptions are stored beside the Claude accounts, with their own look, plan
  and monthly price. Statistics → Subscriptions is rebuilt around four questions: what each plan
  gives, what is used of what is paid, what 1 % of a limit buys over the weeks, and the tokens a week
  per subscription and model.
- Codex sessions write token lines too, from the growth of their rollout files' running totals, so
  their work is counted at API prices in the statistics.
- Settings → General shows `seatswap <version>`.
- Settings → General → "The tray icon shows": the account's letters (as before) or the ring's percent,
  with 100 % shown as 99 so it stays two digits.

### Changed

- Token lines are written per 5-minute bucket and model, with `"min": 5`. A line without `min` is
  still an hour, so the history of older versions keeps its meaning.
- Update every machine: older versions read the new 5-minute token lines as hours.
- Statistics names the companies Anthropic, OpenAI and Ollama. Blocked hours are shown per company
  and never added across companies; the This-week card has one row per provider subscription; the
  account chips include the provider subscriptions.
- A session on the Sessions page unfolds while its agents run and folds again when they finish,
  unless the user folded or unfolded it by hand.
- A provider is polled while its card is on or its subscription has a price. The OpenAI card lists
  the saved logins and writes Codex's `auth.json` only when switching.

### Fixed

- Windows: Sessions now shows a flash job's task. flash-agents keeps its job reports in
  `%LOCALAPPDATA%\flash-agents` there, and seatswap looked in `~/.local/state/flash-agents`; it now
  follows flash-agents' own rule on every system, `$FLASH_DATA_DIR` included.
- A price typed at most 15 minutes before a plan change belongs to the new plan's period, so it no
  longer makes a minutes-long period at the new price.
- Codex token growth is counted once: a repeated total records nothing, a total below the highest
  one is a warning, and a missing `cache_write_input_tokens` counts as 0.
- The OpenAI card keeps showing the live login's usage when seatswap cannot save that login (for
  example a keyring that refuses it); the pop-up row says why it is not saved.
- A Codex login changed outside seatswap (`codex login`, or a switch from another seatswap process)
  is recorded as a switch, so OpenAI work is booked to the right subscription.
- `seatswap status`, `burn plan` and `burn simulate` print the history's warnings, such as a model
  without a price.
- The Forecast shows `—` with its reason instead of `0h` blocked when no typical working hour is
  known yet.
- An hour figure with no hours reads `0h`, not `0s` (blocked, working and idle hours).
- Settings: a block's last lines no longer run over the next section (seen with a long API prices
  error over "Look"); the page scrolls instead.
- Forecast: the "All accounts together" sentence wraps instead of being cut off.
- Subscriptions → Tokens a week: a figure whose rate is still assumed from its plan says `assumed`
  instead of `± 0`.

### Removed

- Statistics' "What if without it", its `WhatIf` type and the `stats_what_if` command. A keep/drop
  verdict misses what a subscription saves elsewhere.
- Work units and their constants in auto-burn; every figure is in API dollars now.

### Known issues

- Windows: a Codex login is most likely too large for one Credential Manager entry (2,560 bytes), so
  seatswap cannot save it there and Codex switching does not work yet; the OpenAI card still shows
  the usage and says why the login is not saved. Worked out from the sizes, not tried on Windows.

## [1.2.2] - 2026-11-17

1.2.1 was tagged but not released: its Windows check failed. 1.2.2 holds every change of 1.2.1
(below) and this fix.

### Fixed

- Windows: when the Claude Code folder is removed while seatswap runs, the pop-up now says Claude
  Code is not set up and notices the folder when it comes back. Before, the removal went unnoticed
  (only with `CLAUDE_CONFIG_DIR` set; the default `~/.claude` was already followed). A file named
  `.claude.json` next to a `CLAUDE_CONFIG_DIR` folder is no longer taken for its login.

## [1.2.1] - 2026-11-17

### Changed

- Codex and Ollama work no longer counts under a Claude account. Statistics → Projects shows it
  under its provider (valued at API prices), Overview's "This week" card has a row per provider, and
  the account chips no longer hide it. Auto-burn's rates no longer count it as work on the account
  that happened to be in use.
- Plans carry Claude's current names: "Max" and "Ultra" instead of "Max 5x" and "Max 20x", "Team
  Premium" instead of "Team 5x". The new name replaces the old one at the next login refresh and is
  not recorded as a plan change.
- Auto-burn no longer waits to measure an account: an account on Max, Team Premium or Ultra starts
  from an assumed rate for its plan (scaled from the accounts already measured), and its own
  measurements take over as they come in. The pop-up no longer shows "Measuring how much work fits
  in …".
- A rise of an account's usage while it was in use on no seatswap machine (claude.ai, the phone,
  another computer) no longer counts as work done here, so such use no longer makes the account look
  smaller than it is.
- An account whose 5 h rate is not measured yet is planned by its week alone; it no longer holds
  back early switching.
- Statistics → Subscriptions: an account whose price was typed in a few minutes after it was added,
  or after its plan changed, no longer shows a second, minutes-long period. A row without a finished
  week says so in plain words instead of empty bars and `—`.
- On a machine where Claude Code was never run, the pop-up no longer shows a red "file watcher: … No
  such file or directory" error. It says that Claude Code is not set up yet, and the first `/login` is
  noticed without restarting seatswap. A start-up error is no longer shown joined to itself ("X; X").
- A new install no longer shows empty statistics cards: Statistics shows one "No statistics yet" note
  until the first reading (the history of removed accounts stays on show); the pop-up asks to save an account and hides "see the plan"
  while there is no plan; the Forecast names the accounts still waiting for their first reading; its
  "only when full" column wraps instead of being cut; Activity says when no work was recorded and
  shows no row of zeros.
- Statistics → Overview opens with a "This week · so far" card: each account's current weekly window,
  its use so far and what it went to by project, from the first usage reading on, whatever range is
  picked, also one with no history yet. The weekly history below counts finished windows. The Weeks page shows the running week as
  "so far". Overview's marks of one week form one label ("AC, PR added") instead of printing on top
  of each other.
- The dollar rate is asked at every start, and at once when the currency changes, also when the
  currency arrives from another machine. A new install on a second machine no longer shows no rate
  for up to half an hour.
- Statistics → Activity: the hour chart shows only machines. The "Ollama · other" row and the "Other
  Ollama use" box are gone; a note under the chart says how many tokens Ollama counted beyond the
  sessions seatswap saw.

### Fixed

- Statistics → Subscriptions, "What if without it": the expired money could be negative and the
  card then showed only an error. It is now counted per window with the window rule (a window at or
  above 100 % expires nothing), and the dropped account's own expired money no longer counts.
- Statistics is faster with a long history: on a year of history every page and range answers in
  about 0.25 s or less in the core (Overview over 12 months: 0.57 s → 0.17 s). History files are
  read in parallel, and switch costs and window look-ups no longer scan the whole history.
- Statistics: charts stay readable at any number of weeks. Week labels show only where they fit and
  switch to month names ("Oct", "Nov", "2026") for long ranges; numbers under narrow columns shorten
  ("11h blocked" → "11h") or hide and stay in the tooltips; mark labels move to a second line or
  become a dot whose tooltip names them. On Weeks a year of weeks fits a wide window; in a narrow one
  only the grid scrolls, with the account names kept in view. Overview's "Share that expired" keeps a
  readable height under the This-week card, and the Subscriptions time axis stays on Mondays across a
  clock change.
- Status line: a first run without a Claude login shows an information line instead of an error.
- The large window opens at once. On Linux it shares the pop-up's web process instead of starting a
  new one, which on a laptop with a sleeping NVIDIA card waited about 2 s for the card to wake; the
  page now starts loading within 0.1 s instead of 0.7–2.9 s. The window appears once its page has
  loaded.
- Statistics no longer hangs on a range that holds the night the clocks go back (e.g. 25 Oct in
  Central Europe) with a block or work in the repeated hour. The local hour of the second 02:00–03:00
  was taken for the first one, so the hour loops never ended. Every local time that exists twice is
  now ordered by itself, whatever order the time-zone library gives.

## [1.2.0] - 2026-11-17

### Added

**Pop-up**
- The auto-burn line names each account with its coloured chip; the tray menu and notifications show
  the same line as plain text.
- *N sessions* opens the Sessions area, *N waiting for you* opens that session there, *see the plan*
  opens Statistics → Forecast, and *Statistics ↗* opens Statistics. A waiting session id the window
  cannot read is refused with an error.

**Large window**
- Settings → History and hosts suggests `<Dropbox>/seatswap` while no history folder is set, read
  from Dropbox's own `info.json`; Use saves it as the history folder. A file that cannot be read is
  shown with its path and why.
- The tray menu has *Sessions…* and *Statistics…*, each opening the large window at that area; they
  sit between *Add account…* and *Settings…*.
- The Settings area has no second heading under its tab.
- The Settings menu stays on screen while the content pane scrolls.

**Sessions**
- A Claude Code session is named by its own title, the one VS Code and `/resume` show, with its
  label (`atlas-a6`) as a small grey tag after it; a long title is cut and shown whole on hover. The
  waiting notification and Activity's largest sessions name the session by its title too. The title
  is read from the transcript's `ai-title` lines and stored in the session records, so it travels
  with a shared history folder; a session closed before this version keeps its label until its next
  title line. A DeepSeek Harness session that is a row of its own is named by its last
  `session/title` the same way; a flash job's title stays its task text and names no row. A Codex
  session is named by its thread name, read from Codex's `session_index.jsonl` (the newest line for
  its id, so a `/rename` wins).
- Codex sessions (provider "Codex") beside Claude Code's, read from Codex's rollout files: their
  session id, folder, model, token counts and times. A Codex session counts as running while a
  process holds its file open or its last event is younger than 30 minutes. A token total that went
  down leaves its last hour unknown, shown as "—" with the reason.
- DeepSeek Harness sessions (provider "DeepSeek Harness") and flash workers: a flash job hangs under
  the Claude Code session that started it, as an agent tagged "flash", with its workers below. A job
  named in a transcript is linked there; on Windows a running job appears once its result is
  collected.
- Every total row shows the same numbers as a session row: Agents, Tokens, Last hour, value and API
  value. A model under two providers has one sum per provider; a model total row carries the
  "thinking tied" bubble, and a total's Agents shows "—" when every row it sums has no agent data.
- The "thinking tied to the account" bubble replaces the brown dot in the Models card and the words
  on a tied model's row in the By model tree; it also marks each session row that has it.
- The last hour has its EUR and API EUR values, the "N completed" agents line has its total, and a
  done agent without a price keeps its reason in the sum.
- A total's last hour is "—" when any row it sums has an unknown one. A known 0 is an empty last
  hour, not "—".
- A running session counts its linked flash/Harness workers in Agents, Tokens and both values
  exactly as its closed row does, so nothing jumps when it closes.
- A shown duration under a minute counts its seconds, redrawn once a second.
- A blocking 5 h, week or model limit that drops early sends `PR is usable again: its week was reset
  early.` (or `its 5h window was reset early` / `its {model} week was reset early`) at once, once per
  account and reset.
- Claude Code's own `cost.total_cost_usd` from the status line becomes the session's API value. An
  absent cost is no cost; a cost that is not a finite number of 0 or more is not stored, and the
  window shows the reason as a warning while it lasts.

**Status line**
- A separator's character is chosen from pipe `│`, middle dot `·`, bullet `•`, slash `/`, chevron
  `›` or blank; a new separator starts as ` │ `.
- Spaces in a segment's prefix and suffix are shown as a faint dot and a count, such as "2 spaces".
- A segment is removed by dropping it on the Segments box ("drop here to remove") or with the
  Options card's *Remove*; Delete keeps working.
- The account tag's background covers its letters only.

**History and statistics**
- A limit drop of at least one percentage point before its reset time ends the window as a reset,
  never a reading error; a smaller drop is kept as a reading and logged once. The window before the
  reset keeps its use, the next window records the percent it began at (`from`), and a week may use
  more than 100 %.
- A week keeps every window it offered, the one before an early reset and the one after, each placed
  by its own midpoint; a week can hold more than one ordinary window too. A reset window counts
  nothing as expired, and a week's expired share is the sum of every window's, so a reset week's
  used and expired money can be more than one week's price.
- A week's Used and Expired percentages and its summary divide by the windows the week offered, not
  by one week's price.
- In Weeks, a cell draws the used part as a bar and the expired share as a hatched band from the
  top; a click on a part opens that window's detail, also when it shares its week.
- One window seen by two hosts counts once: the reset record wins over an ordinary record, and of
  two ordinary records the one with the latest last reading.
- Overview's Used and Expired shares are of the windows the week offered; a window above 100 %
  counts its whole use, so both shares together can pass 100 % and nothing is cut. Its Blocked line
  names the added account by its short name, also when the account filter leaves it out.
- Overview shows Used and Expired with their percentages, a Switches total and "auto-burn"; Weeks
  shows plan and weekly price, "of about N possible" (or "N used, more than the about M your usual
  working hours hold"), the 100 % line, the "now" mark and clickable weeks with their detail;
  Projects shows its matrix totals, the Used footer, each project's share of all use, the "API values
  at 1 USD = …" line and the by-account bar in the account colours; a share that cannot be computed
  is "—" with its reason, never "0 %"; Forecast shows its missing labels, "about N a day" and the
  "none usable" lane last.
- Activity shows a total tokens box, the "What it shows" sentence ("night(s) with work on <host>"),
  the sum over hosts, a "running" tag on the current week, a taller hour chart and host names that
  wrap instead of being cut.
- Ollama's hourly usage, with requests, tokens and US dollars, is kept in the history.
- "Other Ollama use" is Ollama's hourly input + output minus the tokens of every known session on an
  Ollama model (`…:cloud`) in that UTC hour, over every host whatever the page's host filter. An
  hour where the known sessions used more has a note and no "other" value. The KPI, the lane and the
  legend entry show only when Ollama reported at least one hour in the range.
- The Statistics frame names the history's start and size ("since … · now", Export as a button) and
  marks where subscription values begin.
- Organizations whose names differ only in case are one organization, also in a 1.1.0 history.
  Organizations can be added and removed by hand; one that comes from a GitHub owner cannot be
  removed, and any project can be assigned to another organization.

**Several hosts**
- Hosts that share a history folder share each account's label, abbreviation, colour, "Auto-burn may
  use it" and monthly price, and the settings currency, notifications and status line. An edit
  writes only what it changed, as an `accountLook` or `settingSet` line (the price stays a
  `priceChanged`); each field takes the newest value over all hosts, so the last edit wins. The tray
  app takes the other hosts' edits at start and every 5 minutes, and the open pages show them without
  a restart. A value that does not pass this host's own checks, such as an abbreviation one of its
  accounts already has, is not applied and is named in a warning with the host that wrote it. At
  start a host shares its own values of every field no host has shared yet, except settings still at
  their default. Logins never travel: an account appears on a host only after its login there, and a
  login of an account the history knows takes its shared look and price at once (the tray app,
  `seatswap add` and a switch that saves the live login). Settings → History and hosts says what is
  shared.

**Money**
- The rate line reads "ECB rate of Fri 13 Nov · European Central Bank via Frankfurter · fetched today
  10:10". The rate is fetched once per Frankfurt day while it is older than today, and on working
  days from 16:00 again every 30 minutes until an answer carries today's date or it is 18:00. An
  `fx.json` from 1.1.0 keeps its rate but has no fetch time, so the page says "fetched: not
  recorded".
- The model list lives in Settings → Providers, under "API prices from models.dev · checked 11:11",
  and is read-only.
- Claude Code's own reported cost beats a models.dev price; a model with neither has no API value. A
  Claude plan model (its id starts with `claude-`) without a price is asked for from models.dev at
  once, at most once per model and hour; a model outside the plan is never asked for.
- With a currency set and no stored dollar rate yet, API values are shown in US dollars labelled USD,
  and the settings row says `no rate yet: API values are shown in USD`; they are not unknown. With
  the look-ups off the same USD label says `look-ups are off: API values are shown in USD`.

**Other**
- The Swap S icon: a lowercase s with arrow ends on a green square, for the launcher, the Windows
  `.exe`, notifications and every window. The tray icon stays the live one. Notifications show
  `<data folder>/notification-icon.png`, written when it is missing or holds another picture; no
  installed icon file is needed.
- Durations are written without a space between number and unit: `2h 31min`, `4h`, `12min`,
  `3d 4h`; from a day on, the minutes are not shown.
- A session, agent or hour without tokens is worth a known 0, not an unknown value. A sum is
  unknown only when a part is unknown, and a missing value shows "—" with its reason as a tooltip.

### Changed
- Token counts read 1.54M, 683K, 845 everywhere in the window, with thousands in a lighter colour so
  the millions stand out.
- With *Look up prices and the dollar rate* off, API values stay in US dollars and the column header
  says so.
- A status line saved by 1.1.0 with a separator that had no character now prints `│` where 1.1.0
  printed ASCII `|`. A segment's prefix and suffix keep the value's colour when no background is
  painted.
- A model outside the Claude plan (its id does not start with `claude-`) never takes part in a Claude
  account's week: its subscription value is a known 0 and it never makes the week unknown. *If you
  switched now* counts only Claude Code rows on plan models (or with no model known yet).
- A reset is measured against the last poll reading only: the status line's merged number can never
  show one, and a pair of readings that is a reset is left out of auto-burn's learned rates.
- The status line feed is rewritten when the session, its cost or a cost problem changes, not only
  when the 5 h or weekly numbers change, so a session's cost is seen within seconds.
- An agent without Claude Code's `finished` mark counts as done once its transcript ends with the
  agent's final answer (`stop_reason` `end_turn`); a resumed agent counts as running again.

### Fixed
- Settings: every row wraps at 1100 px and below (the label takes the first line, the control the
  second); the focus frame in table fields shows all four sides; the Join projects line no longer
  overlaps its button; the money rows no longer flicker; a missing rate shows "—" with its reason in
  the note, and the API prices row says `checked Fri 13 Nov 11:11`; and the Activity page's Per week
  / Largest sessions switch sits inside its frame.
- Sessions: "In use" wraps inside its box instead of overflowing, and a session without agent data
  shows "—", not "0 · 0". A Codex rollout counts as running while any process holds its file open,
  not only a process named `codex`. The Sessions area still reads Codex's rollout files while the
  Codex card is off, as long as *Read Claude Code's session files* is on.
- A transcript state written by 1.1.0 is migrated to version 2, keeping its offsets, and written
  back as version 2 on the next save, so no transcript is read or recorded twice; a state of another
  version is an error and that pass reads nothing, instead of being rebuilt.
- A rate-limited request says when seatswap asks again, such as "Claude limits how often seatswap
  may ask; next try at 10:42", instead of "rate limited; retry after Some(0) seconds".
- Ollama's usage answer ends with the running hour (`"partial": true`, ending at the moment of the
  answer); it is read as that hour so far instead of raising a notification about `buckets[24]`.
- Claude Code on an Ollama model: its answers count under the model the session asked for
  (`deepseek-v4.1-flash:cloud`), not Ollama's shorter spelling, so they match the Harness rows, the
  prices and the "other Ollama use" of Activity.
- Statistics → Overview: "Share that expired" says so when no finished weekly window with a price is
  in the range yet, instead of failing with "cannot place the mark".
- Sessions: a flash job without its Claude Code session takes the project it was copied from
  instead of `fm` (the worker copies' folder), and leaves the list once its task ended instead of
  staying as an idle session.
- The `tied` bubble and the count of sessions whose thinking is tied to the account leave out
  models off the Claude plan (Harness, Codex, Ollama): a switch does not touch them.
- The large window uses half the margin around its content (12 px instead of 24 px).
- Sessions: an agent's row shows its model, also once it is folded into "N completed"; it was
  always empty.
- API values of DeepSeek Harness, flash, Ollama and Codex models: prices are looked up under the
  model's own provider (`ollama-cloud/…`, `openai/…`), not as Anthropic models, so these rows and every
  total above them are no longer "—".
- A running session is valued hour by hour from its token lines, like a closed one, so a session that
  runs across days, a weekly reset or a switch gets each hour's account and week; a linked flash job's
  tokens count once.
- Sessions: the running agents' heading names each agent's own provider ("DeepSeek Harness · …" for a
  flash job under a Claude Code session) and says "no answers yet" instead of "null".
- Sessions: a flash job is named by its label from flash-agents' report instead of flash-agents'
  internal prompt, and its workers show no task; agent rows leave out Claude Code's default kind
  `general-purpose` and show just the task.
- A flash worker's copy is traced back to its project in every flash-agents folder layout, and on
  Windows with either case of the drive letter.
- After changing the history folder (Settings → History and hosts, e.g. to Dropbox) this host's
  history stayed behind in the old folder; at the next start it now comes along, merged line by line
  when this host already wrote into the new folder, and the old folder stays as it was. The import
  merges two copies of a host's file that both changed instead of keeping only the longer one.
- Windows: the large window opened plain white (it was built inside the tray menu's handler, which
  deadlocks there); it is now built on a thread of its own. The tray's right-click menu closed by
  itself about once a second; it is now replaced only when its items change.
- Statistics says "1 host", not "1 hosts".
- A running session whose token records cannot be read (no history, an offline Dropbox) shows its
  transcripts' tokens, with the reason its values are missing; the prices for an older session are
  retried every minute, not every pass; a DeepSeek Harness or Codex answer with an empty model no
  longer breaks the Sessions page.
- The pop-up's learning line says what is measured: "Measuring how much work fits in AS, A1. Until
  then: switch only when full."

### Removed
- *Use my own rate* (`usdRate`) and the own prices in Settings. With the look-ups off, money is
  shown in US dollars. An older settings file still loads; both keys are ignored silently.
- The tray's *Open seatswap…* entry; *Sessions…* and *Statistics…* open the large window instead.
- The brown dot that marked a model whose thinking is tied to the account; the bubble replaces it.

## [1.1.0] - 2026-11-16

### Added

**Pop-up**
- The pop-up shows the running Claude Code sessions: how many run, how many are busy and how many
  are waiting for you.
- Notifications "usable again", "all accounts are full" and "waiting for you"; with *For sessions
  on: all hosts*, a session waiting on another host notifies here too, and a click opens the
  Sessions area at that session.
- The plan level on every account, such as "Team 5x".

**Large window**
- A large window with four areas, Sessions, Statistics, Status line and Settings, opened from the
  pop-up's gear, `,`, an account's badge or ✎, the tray's *Settings…* and *Open seatswap…*, or
  `seatswap window [AREA]`; every control saves at once.
- The Settings area: every setting, the account editor (abbreviation, colour, label, price) and the
  History and hosts view.
- The Sessions area: every running session on this host and on the user's other hosts, live, with
  its agents, tokens and their subscription and API values, the closed sessions of the last 24
  hours, 7 or 30 days, and the account in use.

**Status line and auto-burn**
- `seatswap statusline`: Claude Code's status line with the account and its live limits, which also
  feeds Claude Code's 5 h and weekly numbers into seatswap's usage cache, so the active account is
  seconds old instead of up to 2 minutes.
- A status line builder in the large window: segments in two zones, options per segment and a
  preview at three widths.
- "Use in Claude Code" and "Restore previous": seatswap writes only the `statusLine` key of Claude
  Code's settings and can put the earlier value back.
- Status line segments for context used, session cost and lines changed, shown when Claude Code
  sends those fields.
- Auto-burn: one switch that moves between accounts by itself, with switch suggestions while it is
  off, a quiet moment for sessions on a model that would lose its thinking, a manual hold, a pause,
  and a per-account "Auto-burn may use it".
- `seatswap burn plan` and `seatswap burn simulate` (`--json` for the plan): the early-switching
  regime, the lazy plan and a replay of recorded work for the lazy and eager policies.
- The pop-up, `burn-next.json` and the status line name the same next account as the Forecast while
  early switching is on; the pop-up's line says `Then AC, before PR is full.` when that plan leaves
  PR before its week fills.
- Auto-burn goes back to the account that produced the running sessions' thinking when two
  accounts are about equally good.
- `usable` in `seatswap status --json` (whether an account can be used right now), `slackHours` in
  every view while known, and `autoBurn`, `next` and `burnError` at the top level.

**History and statistics**
- A history of every usage reading, finished window, switch and agent call, plus the tokens each
  session used per project and hour, in an append-only folder that several hosts can share;
  `seatswap history export --format csv|json` and `seatswap history import <folder>` read and copy
  it.
- The Statistics area: Overview, Weeks with a summary per week, Projects and organizations,
  Subscriptions with "what if", Forecast, Rhythm and Activity.
- Money: a currency, each account's monthly price, API prices from models.dev and the dollar rate
  from Frankfurter, each kept with its date, so past days keep that day's price; one switch,
  *Look up prices and the dollar rate*, turns both look-ups off, and `seatswap prices [--refresh]`
  prints the stored API prices.
- Projects and organizations: names, joins for the same project seen in two places, and
  organizations, in a project log that travels with the history folder.

**Agents**
- Settings for agents ("Agents may read usage", "Agents may switch accounts") and
  `seatswap agent-skill install|uninstall`.

**Other providers**
- Read-only cards for Codex (5 h and weekly limits) and Ollama's cloud plan (credits left, reset
  date, spend in the last 24 h), each off by default, in Settings → Providers.

### Changed
- Settings and the account editor moved from the pop-up to the large window; the gear, `,` and an
  account's badge or ✎ open it there.
- A per-model limit blocks an account only while a session on that model is open.
- A file seatswap rewrites that is a symbolic link stays a link, with its target rewritten; a link
  pointing nowhere is now an error that names the link, instead of being replaced by a file.
- `seatswap status --json`: `suggestion` has a new shape, `{toId, text}` instead of the old
  `{fromId, toId, fromPercent, toPercent, fromResetsAt}`, so the target and the whole sentence are
  both there.

### Removed
- Settings `suggestAt` and `suggestTargetBelow`: the fixed 90 % / 50 % suggestion is replaced by
  auto-burn. An older `accounts.json` with them still loads; the keys are ignored.

- A reset time in Claude's usage reply that seatswap cannot read is no longer dropped without a
  word: the limit is shown without it and the account's card says why.
- Once a settings file uses the new status line segments (context used, session cost, lines
  changed), a seatswap older than 1.1.0 cannot read it: going back below 1.1.0 then means removing
  those segments first.

### Fixed
- `seatswap status --help` names the JSON fields `views`, `active` and `suggestion` as they are; it
  said "the active id", which is not a field.
- With early switching on, the pop-up, `burn-next.json` and the status line could name a switch for
  "now" within 30 minutes of the last switch or during a manual hold or pause, although auto-burn
  waits then; the plan now starts from the last switch and the hold.

## [1.0.6] - 2026-11-14

### Fixed
- The pop-up's keyboard cursor (the blue ring) no longer stays on an account from an earlier
  opening or a click. It shows only after ↑/↓, starting from the account in use, and is gone
  each time the pop-up opens; until then Enter does nothing.

## [1.0.5] - 2026-11-13

### Added
- Clicking an account's gauge in the pop-up switches to that account, like its Switch button.

## [1.0.4] - 2026-11-13

### Changed
- The pop-up shows the account in use as the bottom card; the other accounts stay above it in
  list order.

## [1.0.3] - 2026-11-13

### Added
- The settings view shows seatswap's version at the right of its header.

## [1.0.2] - 2026-11-12

### Changed
- Pop-up gauges: the outer ring now shows the week and the inner ring the 5 h session; the
  week's percent is the large figure in the middle. On plans without a session limit (Ultra) the
  inner ring is left out instead of showing an empty grey track.
- The usage bars beside the gauge follow the same order: week, session, then the per-model
  limits.

## [1.0.1] - 2026-11-11

### Fixed
- No more "no system tray" notification at login when the desktop's tray starts a moment after
  seatswap (Linux Mint: same second). seatswap now waits 30 s for a tray before it notifies.

## [1.0.0] - 2026-11-11

### Added
- Windows support (Windows 10 1803+ and 11): the same login files as on Linux, the single-instance
  socket, start at login through the `Run` registry key, notifications under the name
  "seatswap". Windows gets two programs: `seatswap.exe` for the terminal and `seatswap-tray.exe`,
  the same program without a console window, for autostart and the Start menu.

### Changed
- A switch also takes Claude Code's two newer renewal locks, `.design_oauth_refresh.lock` and
  `.cloud_sessions_oauth_refresh.lock`, so it cannot land in the middle of a renewal of the
  `designOauth` or `cloudSessionsOauth` login.

### Fixed
- "Start at login" with a program path or a `--data-dir` / `--claude-config-dir` folder that is
  not valid Unicode is now an error. Before, the autostart entry was written with the odd
  characters replaced, which would have started seatswap with another folder.
- A switch suggestion or "new login found" notification that timed out or was dismissed opened
  the pop-up, as if it had been clicked. Now only a click on its button or on its body does
  anything.

## [0.1.0] - 2026-11-11

### Added
- First version, for Linux: tray icon with the active account's abbreviation and a usage ring,
  pop-up with every account's session and weekly usage, switch suggestion and notification,
  refresh of inactive logins, end-of-login warnings, settings, start at login.
- Command line: `list`, `use`, `next`, `add`, `edit`, `rm`, `status` (`--json`, `--waybar`,
  `--i3blocks`), `panel`.
- Left click on the tray icon opens the pop-up and the tooltip shows the usage, via the
  StatusNotifierItem backend of `tray-icon`.
- Runs without a system tray (GNOME without the AppIndicator extension, or a desktop panel that
  starts later): it says so and shows the icon as soon as a tray appears; `seatswap panel` opens
  the pop-up meanwhile.

[Unreleased]: https://github.com/ymihere/seatswap/compare/v1.6.1...HEAD
[1.6.1]: https://github.com/ymihere/seatswap/compare/v1.6.0...v1.6.1
[1.6.0]: https://github.com/ymihere/seatswap/compare/v1.5.5...v1.6.0
[1.5.5]: https://github.com/ymihere/seatswap/compare/v1.5.4...v1.5.5
[1.5.4]: https://github.com/ymihere/seatswap/compare/v1.5.3...v1.5.4
[1.5.3]: https://github.com/ymihere/seatswap/compare/v1.5.2...v1.5.3
[1.5.2]: https://github.com/ymihere/seatswap/compare/v1.5.1...v1.5.2
[1.5.1]: https://github.com/ymihere/seatswap/compare/v1.5.0...v1.5.1
[1.5.0]: https://github.com/ymihere/seatswap/compare/v1.4.5...v1.5.0
[1.4.5]: https://github.com/ymihere/seatswap/compare/v1.4.4...v1.4.5
[1.4.4]: https://github.com/ymihere/seatswap/compare/v1.4.3...v1.4.4
[1.4.3]: https://github.com/ymihere/seatswap/compare/v1.4.2...v1.4.3
[1.4.2]: https://github.com/ymihere/seatswap/compare/v1.4.1...v1.4.2
[1.4.1]: https://github.com/ymihere/seatswap/compare/v1.4.0...v1.4.1
[1.4.0]: https://github.com/ymihere/seatswap/compare/v1.3.0...v1.4.0
[1.3.0]: https://github.com/ymihere/seatswap/compare/v1.2.2...v1.3.0
[1.2.2]: https://github.com/ymihere/seatswap/compare/v1.2.1...v1.2.2
[1.2.1]: https://github.com/ymihere/seatswap/compare/v1.2.0...v1.2.1
[1.2.0]: https://github.com/ymihere/seatswap/compare/v1.1.0...v1.2.0
[1.1.0]: https://github.com/ymihere/seatswap/releases/tag/v1.1.0
[1.0.6]: https://github.com/ymihere/seatswap/releases/tag/v1.0.6
[1.0.5]: https://github.com/ymihere/seatswap/releases/tag/v1.0.5
[1.0.4]: https://github.com/ymihere/seatswap/releases/tag/v1.0.4
[1.0.3]: https://github.com/ymihere/seatswap/releases/tag/v1.0.3
[1.0.2]: https://github.com/ymihere/seatswap/releases/tag/v1.0.2
[1.0.1]: https://github.com/ymihere/seatswap/releases/tag/v1.0.1
[1.0.0]: https://github.com/ymihere/seatswap/releases/tag/v1.0.0
[0.1.0]: https://github.com/ymihere/seatswap/releases/tag/v0.1.0
