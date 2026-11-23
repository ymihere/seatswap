//! The `seatswap` command line (spec §4, §9.1; docs/implementation.md
//! "app: CLI").
//!
//! This module only parses arguments, calls the core library and prints. Every
//! decision about an account, a usage percentage or which login is live is made
//! in `seatswap_core`.

use std::collections::{BTreeMap, BTreeSet};
use std::io::{self, IsTerminal, Read, Write};
use std::path::{Path, PathBuf};

use chrono::{DateTime, FixedOffset, Local, LocalResult, NaiveDate, NaiveTime, TimeZone, Utc};
use clap::{CommandFactory, Parser, Subcommand, ValueEnum};
use serde::Serialize;

use seatswap_core::api::{Api, Usage};
use seatswap_core::burn::decide::Regime;
use seatswap_core::burn::live::{self, Readiness};
use seatswap_core::burn::plan::{follow, slack, Inputs, Policy};
use seatswap_core::burn::replay::replay;
use seatswap_core::burn::usd_hours;
use seatswap_core::engine::{AccountView, Engine, PollReason, UsageCache};
use seatswap_core::history::{
    export_csv, export_json, AccountRef, By, EventKind, HistoryDir, Imported, Range, Recorder,
};
use seatswap_core::money::{self, Prices};
use seatswap_core::paths::{self, AppPaths, ClaudePaths, CodexPaths, PathsError};
use seatswap_core::providers::{self, CODEX_ORG};
use seatswap_core::shared;
use seatswap_core::store::{
    self, Account, AccountsFile, LoginState, ProviderSubscription, Settings, Store, StoreError,
};
use seatswap_core::switcher::{self, LiveState, SwitchError, UnsavedPolicy};

use crate::agent_skill::{self, SkillError};
use crate::burn_text;
use crate::format::{
    class_name, format_age, format_reset_at, integer_percent, login_ends_text, percent, tooltip,
};
use crate::gui::ipc;
use crate::ops;
use crate::statusline;

/// Colour used when there is no live account or no usage to judge (the grey of
/// the unknown tray ring, docs/implementation.md icon.rs).
const NO_LOAD_COLOR: &str = "#555b66";

const AGENTS_MAY_NOT_READ: &str = "agents may not read usage (seatswap settings → Agents)";
const AGENTS_MAY_NOT_SWITCH: &str = "agents may not switch accounts (seatswap settings → Agents)";
const AGENTS_MAY_NOT_CHANGE: &str = "agents may not change seatswap's accounts or settings";

/// What an agent's `next` prints while auto-burn decides (spec §6.2).
const AUTO_BURN_DECIDES: &str = "seatswap: auto-burn is on and decides";

/// Who started this call (spec §6.1).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum Caller {
    Person,
    Agent,
}

/// Agent when `CLAUDECODE` is exactly `1`, or `AI_AGENT` is set and not empty.
/// `env` is the environment to read, never the process's own.
pub(crate) fn caller(env: &dyn Fn(&str) -> Option<String>) -> Caller {
    if env("CLAUDECODE").as_deref() == Some("1") {
        return Caller::Agent;
    }
    if env("AI_AGENT").is_some_and(|value| !value.is_empty()) {
        return Caller::Agent;
    }
    Caller::Person
}

/// What an agent may do with one command (spec §6.2).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum Gate {
    Allowed,
    Refused(&'static str),
}

/// The command line of `seatswap`.
#[derive(Debug, Parser)]
#[command(
    name = "seatswap",
    version,
    about = "Switch Claude Code between Claude accounts",
    long_about = None
)]
struct Cli {
    /// seatswap's own data folder (default: the platform config folder).
    #[arg(long, global = true, value_name = "DIR")]
    data_dir: Option<PathBuf>,

    /// Claude Code's config folder (default: the claudeConfigDir setting, then
    /// $CLAUDE_CONFIG_DIR, then ~/.claude).
    #[arg(long, global = true, value_name = "DIR")]
    claude_config_dir: Option<PathBuf>,

    #[command(subcommand)]
    command: Option<Command>,
}

impl Cli {
    /// Make `--data-dir` and `--claude-config-dir` absolute once, before any
    /// use, so the folders are the same whatever the current folder is later
    /// (the autostart entry starts seatswap from the home folder).
    fn make_folders_absolute(&mut self) -> Result<(), PathsError> {
        for folder in [&mut self.data_dir, &mut self.claude_config_dir] {
            if let Some(path) = folder.as_deref() {
                *folder = Some(paths::absolute(path)?);
            }
        }
        Ok(())
    }
}

#[derive(Debug, Subcommand)]
pub(crate) enum Command {
    /// List the stored accounts and their cached usage.
    List {
        /// Poll the usage before printing.
        #[arg(long)]
        refresh: bool,
    },

    /// Switch Claude Code to an account (by id or abbreviation).
    Use {
        /// Account id or abbreviation.
        key: String,

        /// Save the live login as an account first (optional label).
        #[arg(
            long,
            value_name = "LABEL",
            num_args = 0..=1,
            conflicts_with = "discard_current"
        )]
        save_current: Option<Option<String>>,

        /// Throw the live login away instead of saving it.
        #[arg(long, conflicts_with = "save_current")]
        discard_current: bool,
    },

    /// Switch to the next usable account in list order, wrapping around.
    Next {
        /// Save the live login as an account first (optional label).
        #[arg(
            long,
            value_name = "LABEL",
            num_args = 0..=1,
            conflicts_with = "discard_current"
        )]
        save_current: Option<Option<String>>,

        /// Throw the live login away instead of saving it.
        #[arg(long, conflicts_with = "save_current")]
        discard_current: bool,
    },

    /// Save the live login as a new account (or update the stored copy).
    Add {
        /// Label for a newly created account.
        #[arg(long, value_name = "LABEL")]
        label: Option<String>,
    },

    /// Remove an account and its stored login. Claude Code's files stay as they are.
    Rm {
        /// Account id or abbreviation.
        key: String,

        /// Do not ask for confirmation.
        #[arg(long)]
        yes: bool,
    },

    /// Change an account's label, abbreviation or colour.
    Edit {
        /// Account id or abbreviation.
        key: String,

        #[arg(long, value_name = "LABEL")]
        label: Option<String>,

        #[arg(long, value_name = "ABBR")]
        abbr: Option<String>,

        #[arg(long, value_name = "#RRGGBB")]
        color: Option<String>,
    },

    /// Print the auto-burn plan or replay recorded work.
    Burn {
        #[command(subcommand)]
        action: BurnAction,
    },

    /// Install or remove seatswap's agent skill in Claude Code's config folder.
    AgentSkill {
        #[command(subcommand)]
        action: SkillAction,
    },

    /// Ask the running tray app to open its pop-up.
    Panel {
        /// Which view the pop-up opens on.
        #[arg(value_enum, default_value_t = PanelView::Main)]
        view: PanelView,
    },

    /// Ask the running tray app to open the large window at an area.
    Window {
        /// Area, optionally "/<sub>" (default: sessions).
        #[arg(value_name = "AREA")]
        target: Option<String>,
    },

    /// Print the live account and its binding limit.
    Status {
        /// Print JSON: `views` (every account, each with `usable` and, while known, `slackHours`),
        /// `active` (the id of the account in use, or null), `suggestion` (where to switch, or
        /// null), `autoBurn` (`{on, pausedUntil}`), `next` (the id auto-burn would switch to, or
        /// null) and `burnError` (a history or planner error, or null). `next` is planned without
        /// the sessions' thinking data, as `burn plan` is, so it can name another account than the
        /// pop-up while sessions on a model that would lose their thinking run.
        #[arg(long, conflicts_with_all = ["waybar", "i3blocks"])]
        json: bool,

        /// Print one waybar JSON line.
        #[arg(long, conflicts_with_all = ["json", "i3blocks"])]
        waybar: bool,

        /// Print i3blocks' three lines.
        #[arg(long, conflicts_with_all = ["json", "waybar"])]
        i3blocks: bool,

        /// Poll the usage before printing.
        #[arg(long)]
        refresh: bool,
    },

    /// Print the stored API prices per model, in USD per million tokens.
    Prices {
        /// Look the list up at models.dev before printing.
        #[arg(long)]
        refresh: bool,
    },

    /// Write the history as CSV or JSON, or copy another host's folder in.
    History {
        #[command(subcommand)]
        action: HistoryAction,
    },

    /// List the saved Codex logins or switch Codex to one of them (R8).
    Codex {
        #[command(subcommand)]
        action: CodexAction,
    },

    /// Print Claude Code's status line; reads its JSON from standard input.
    Statusline,
}

/// What `seatswap codex` does (R8).
#[derive(Debug, Subcommand)]
pub(crate) enum CodexAction {
    /// Print one line per saved Codex login.
    List {
        /// Print JSON: one entry per saved login, without its usage.
        #[arg(long)]
        json: bool,
    },

    /// Switch Codex's live login to a saved one (by abbreviation).
    Switch {
        /// Abbreviation of a saved Codex login.
        abbr: String,
    },
}

/// What `seatswap history` does (spec §1.5, §3.7).
#[derive(Debug, Subcommand)]
pub(crate) enum HistoryAction {
    /// Write the history to standard output.
    Export {
        /// `csv` or `json`.
        #[arg(long, value_enum)]
        format: ExportFormat,

        /// First UTC day, `YYYY-MM-DD` (default: the oldest record).
        #[arg(long, value_name = "YYYY-MM-DD")]
        from: Option<String>,

        /// Last UTC day, `YYYY-MM-DD`, inclusive (default: every later day).
        #[arg(long, value_name = "YYYY-MM-DD")]
        to: Option<String>,
    },
    /// Copy another history folder's host folders in by hand.
    Import {
        /// The other history folder, an absolute path.
        folder: String,
    },
}

/// The two formats of `seatswap history export`.
#[derive(Debug, Clone, Copy, PartialEq, Eq, ValueEnum)]
pub(crate) enum ExportFormat {
    Csv,
    Json,
}

impl ExportFormat {
    fn as_str(self) -> &'static str {
        match self {
            ExportFormat::Csv => "csv",
            ExportFormat::Json => "json",
        }
    }
}

/// What `seatswap burn` does (spec §2.4, §2.8).
#[derive(Debug, Subcommand)]
pub(crate) enum BurnAction {
    /// Print the early-switching regime and the plan auto-burn follows (eager while early
    /// switching is on).
    Plan {
        /// Print one JSON object instead of the lines.
        #[arg(long)]
        json: bool,
    },
    /// Replay recorded work against both policies.
    Simulate {
        /// First local day of the replay, `YYYY-MM-DD`.
        #[arg(long, value_name = "YYYY-MM-DD")]
        from: String,

        /// Last local day of the replay, `YYYY-MM-DD`, inclusive.
        #[arg(long, value_name = "YYYY-MM-DD")]
        to: String,

        /// Leave an account out, by abbreviation or id.
        #[arg(long = "without", value_name = "ABBR")]
        without: Vec<String>,
    },
}

/// Install or remove the bundled skill (spec §6.4).
#[derive(Debug, Subcommand)]
pub(crate) enum SkillAction {
    /// Write the skill into Claude Code's config folder.
    Install {
        /// Replace a skill file that differs from the bundled one.
        #[arg(long)]
        force: bool,
    },
    /// Remove the skill and its folder when it is empty.
    Uninstall {
        /// Remove a skill file that differs from the bundled one.
        #[arg(long)]
        force: bool,
    },
}

/// The view `seatswap panel` asks the running tray app to open.
#[derive(Debug, Clone, Copy, PartialEq, Eq, ValueEnum)]
pub(crate) enum PanelView {
    Main,
    Add,
    Settings,
}

impl PanelView {
    fn as_str(self) -> &'static str {
        match self {
            PanelView::Main => "main",
            PanelView::Add => "add",
            PanelView::Settings => "settings",
        }
    }
}

/// What every command needs: the open store, Claude Code's folders and the
/// settings that were in effect when the store was opened.
struct Context {
    store: Store,
    claude: ClaudePaths,
    settings: Settings,
}

/// Everything the command line can fail with. Anything that is not an unsaved
/// live login exits 1.
#[derive(Debug, thiserror::Error)]
pub enum CliError {
    #[error("{0}")]
    Message(String),

    /// The live login belongs to no stored account; `use`/`next` exit 2 for this.
    #[error("{0}")]
    UnsavedCurrent(String),

    /// The command cannot run with the given values or the data on hand (bad
    /// dates, an unknown abbreviation, no reading): exits 2, as a usage error.
    #[error("{0}")]
    Usage(String),

    /// An auto-burn text the values do not allow (a missing view or reset).
    #[error(transparent)]
    Text(#[from] burn_text::TextError),

    /// The agent gate refused this call; exits 3 and is recorded first.
    #[error("{0}")]
    AgentRefused(&'static str),

    #[error(transparent)]
    Store(#[from] StoreError),

    #[error(transparent)]
    Switch(#[from] SwitchError),

    #[error(transparent)]
    Engine(#[from] seatswap_core::engine::EngineError),

    #[error(transparent)]
    Paths(#[from] PathsError),

    #[error(transparent)]
    Ipc(#[from] ipc::IpcError),

    #[error(transparent)]
    Io(#[from] io::Error),

    #[error("cannot encode JSON: {0}")]
    Json(#[from] serde_json::Error),
}

impl CliError {
    fn exit_code(&self) -> u8 {
        match self {
            CliError::UnsavedCurrent(_) | CliError::Usage(_) => 2,
            CliError::AgentRefused(_) => 3,
            _ => 1,
        }
    }
}

/// Parse the command line, run the command and turn the result into a process
/// exit code. Errors are printed as `seatswap: <message>` on stderr.
pub fn run() -> std::process::ExitCode {
    let mut cli = Cli::parse();
    if let Err(error) = cli.make_folders_absolute() {
        let error = CliError::from(error);
        let (line, code) = folders_error(cli.command.as_ref(), &error);
        match line {
            Some(line) => println!("{line}"),
            None => eprintln!("seatswap: {error}"),
        }
        return std::process::ExitCode::from(code);
    }
    let caller = caller(&|name| std::env::var(name).ok());
    #[cfg(feature = "gui")]
    if cli.command.is_none() && no_subcommand(caller) == NoSubcommand::StartApp {
        return match run_gui(&cli) {
            Ok(code) => code,
            Err(error) => {
                eprintln!("seatswap: {error}");
                std::process::ExitCode::from(error.exit_code())
            }
        };
    }
    // Not `lock()`: `rm` asks its question through `io::stdin()` itself, and
    // std's stdin lock is not reentrant, so a lock held here would hang it.
    let mut stdin = io::stdin();
    let code = match execute(&mut cli, caller, &mut stdin) {
        Ok(()) => 0,
        Err(error) => {
            eprintln!("seatswap: {error}");
            error.exit_code()
        }
    };
    std::process::ExitCode::from(code)
}

/// A folder that cannot be made absolute before any command runs: for the
/// status line that error is the same one line `seatswap: <reason>` on standard
/// output and exit 0 as every other status line error; every other command
/// keeps it on standard error and exits 1. `None` is the command without a
/// subcommand.
fn folders_error(command: Option<&Command>, error: &CliError) -> (Option<String>, u8) {
    match command {
        Some(Command::Statusline) => (Some(format!("seatswap: {error}")), 0),
        _ => (None, error.exit_code()),
    }
}

/// What a call without a subcommand does (spec §6.1, §6.2): a person starts the
/// tray app in a `gui` build, or sees the help text without `gui`; an agent may
/// not start the app at all.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum NoSubcommand {
    #[cfg(feature = "gui")]
    StartApp,
    #[cfg(not(feature = "gui"))]
    PrintHelp,
    Refuse,
}

fn no_subcommand(caller: Caller) -> NoSubcommand {
    match caller {
        Caller::Agent => NoSubcommand::Refuse,
        Caller::Person => {
            #[cfg(feature = "gui")]
            {
                NoSubcommand::StartApp
            }
            #[cfg(not(feature = "gui"))]
            {
                NoSubcommand::PrintHelp
            }
        }
    }
}

/// Start the tray app. A store that cannot be opened is not fatal: the tray
/// starts with the error in its tooltip so the person can fix it from settings.
#[cfg(feature = "gui")]
fn run_gui(cli: &Cli) -> Result<std::process::ExitCode, CliError> {
    let app = AppPaths::resolve(cli.data_dir.as_deref())?;
    Ok(crate::gui::run(
        app,
        cli.claude_config_dir.clone(),
        cli.data_dir.clone(),
    ))
}

fn execute(cli: &mut Cli, caller: Caller, stdin: &mut dyn Read) -> Result<(), CliError> {
    let Some(command) = cli.command.take() else {
        return match no_subcommand(caller) {
            // An agent's call without a subcommand opens the store like any
            // other agent call, so the refusal can be recorded with its cwd.
            NoSubcommand::Refuse => refuse_no_subcommand(cli),
            // A person: without `gui` the help text. With `gui`, `run()` starts
            // the tray app before `execute`, so this is only reached by an
            // in-process test, which gets the help text too.
            _person => {
                Cli::command().print_help().map_err(CliError::Io)?;
                println!();
                Ok(())
            }
        };
    };
    // Claude Code calls the status line every few seconds with `CLAUDECODE` set,
    // as an agent. It is never gated and never recorded: it runs before any
    // context is opened, so it cannot touch the history or the store's locks.
    if matches!(command, Command::Statusline) {
        println!("{}", statusline(cli, stdin));
        return Ok(());
    }
    match caller {
        // A person's call is never gated, and `panel` and `window` keep needing
        // only the data folder: they talk to the running tray app.
        Caller::Person => {
            match &command {
                Command::Panel { view } => return panel(cli, *view),
                Command::Window { target } => return window(cli, target.as_deref()),
                _ => {}
            }
            let context = open_context(cli)?;
            run_command(context, command, By::Cli, None)
        }
        // An agent's call needs the settings and a history to record it, so the
        // context is opened before the gate and before any command does anything.
        Caller::Agent => {
            let context = open_context(cli)?;
            let cwd = current_cwd();
            let verdict = gate(&command, caller, &context.settings);
            record_agent_call(
                &context,
                &command_text(&command),
                matches!(verdict, Gate::Allowed),
                cwd.clone(),
            );
            match verdict {
                Gate::Allowed => run_command(context, command, By::Agent, cwd),
                Gate::Refused(text) => Err(CliError::AgentRefused(text)),
            }
        }
    }
}

/// A bare `seatswap` from an agent is a refused agent call: it opens the store
/// like the other agent commands, records the refusal, then exits 3.
fn refuse_no_subcommand(cli: &Cli) -> Result<(), CliError> {
    let context = open_context(cli)?;
    record_agent_call(&context, "(no subcommand)", false, current_cwd());
    Err(CliError::AgentRefused(AGENTS_MAY_NOT_CHANGE))
}

fn run_command(
    context: Context,
    command: Command,
    by: By,
    cwd: Option<String>,
) -> Result<(), CliError> {
    match command {
        Command::List { refresh } => list(context, refresh),
        Command::Use {
            key,
            save_current,
            discard_current,
        } => use_account(context, &key, save_current, discard_current, by, cwd),
        Command::Next {
            save_current,
            discard_current,
        } => {
            let line = next_account(context, save_current, discard_current, by, cwd)?;
            println!("{line}");
            Ok(())
        }
        Command::Add { label } => add(context, label),
        Command::Rm { key, yes } => remove(context, &key, yes),
        Command::Edit {
            key,
            label,
            abbr,
            color,
        } => edit(context, &key, label, abbr, color),
        Command::Burn { action } => burn(context, action),
        Command::History { action } => match action {
            HistoryAction::Export { format, from, to } => {
                history_export(&context, format, from.as_deref(), to.as_deref())
            }
            HistoryAction::Import { folder } => history_import(&context, &folder),
        },
        Command::AgentSkill { action } => agent_skill(&context, action),
        Command::Prices { refresh } => prices(context, refresh),
        Command::Codex { action } => codex(context, action),
        // Handled before the store was opened; kept for exhaustiveness.
        Command::Panel { .. } | Command::Window { .. } | Command::Statusline => Ok(()),
        Command::Status {
            json,
            waybar,
            i3blocks,
            refresh,
        } => status(context, json, waybar, i3blocks, refresh),
    }
}

/// `seatswap statusline`: the one line Claude Code shows. It prints that line
/// on standard output and never fails the command: every error becomes
/// `seatswap: <reason>` there, and the exit code stays 0. `stdin` is the
/// payload's reader: `run` passes the process's standard input, every test
/// passes bytes, so no test can wait on a terminal.
fn statusline(cli: &Cli, stdin: &mut dyn Read) -> String {
    let mut input = Vec::new();
    let now = Utc::now();
    match stdin.read_to_end(&mut input) {
        Ok(_) => statusline_output(cli, &input, now),
        Err(error) => format!("seatswap: cannot read standard input: {error}"),
    }
}

/// The status line built from bytes already read; [`statusline`] without the
/// process's standard input, so a test can pass the payload itself.
fn statusline_output(cli: &Cli, stdin: &[u8], now: DateTime<Utc>) -> String {
    match statusline_line(cli, stdin, now) {
        Ok(line) => line,
        Err(reason) => format!("seatswap: {reason}"),
    }
}

/// Everything the line needs, read without a lock, without the network and
/// without opening a secret store: `accounts.json`, the usage cache and Claude
/// Code's two files. The feed is published and the line built first; the
/// recorder (`statusline-keys.json`) runs last, and its error does not change
/// the printed line: the window's `statusline_keys` command reads the same file
/// and shows that error on the page.
fn statusline_line(cli: &Cli, stdin: &[u8], now: DateTime<Utc>) -> Result<String, String> {
    let app = AppPaths::resolve(cli.data_dir.as_deref()).map_err(|error| error.to_string())?;
    if !app.accounts_file().is_file() {
        return Err(format!("no accounts.json in {}", app.data_dir.display()));
    }
    let file = store::load_accounts(&app).map_err(|error| error.to_string())?;
    file.settings
        .validate()
        .map_err(|error| error.to_string())?;
    let payload = statusline::parse(stdin);
    if let Err(reason) = statusline::publish_feed(&app, &file.settings, &payload, now) {
        return Err(format!("feed: {reason}"));
    }
    let paths = resolve_paths(
        app,
        cli.claude_config_dir.as_deref(),
        file.settings.claude_config_dir.as_deref(),
    )
    .map_err(|error| error.to_string())?;
    let (app, claude) = paths.into_parts();
    let cache = UsageCache::load(&app).map_err(|error| error.to_string())?;
    let live = switcher::live_state(&claude, &file).map_err(|error| error.to_string())?;
    let facts = statusline::facts(
        &app,
        &payload,
        &file,
        &live,
        &cache,
        claude.home(),
        paths::user_name()?,
        now,
    )?;
    let line = statusline::render::ansi(
        &statusline::render::render_spans(
            &file.settings.statusline,
            &payload,
            &facts,
            statusline::width::terminal_width(),
            &Local,
        )
        .0,
    );
    // The recorded field names are a diagnostic list, not the line: a broken
    // keys file must not cost the line or the feed above.
    let _ = statusline::record_keys(&app, &file.settings, &payload, now);
    Ok(line)
}

/// The gate table of spec §6.2. A person is always allowed.
pub(crate) fn gate(command: &Command, caller: Caller, settings: &Settings) -> Gate {
    if caller == Caller::Person {
        return Gate::Allowed;
    }
    match command {
        // Claude Code calls it every two seconds as an agent, for a line and
        // nothing else: no setting and no caller can refuse it.
        Command::Statusline => Gate::Allowed,
        // `prices` reads like `status`; refreshing writes prices.json.
        Command::Prices { refresh } => {
            if *refresh {
                Gate::Refused(AGENTS_MAY_NOT_CHANGE)
            } else if settings.agents_read {
                Gate::Allowed
            } else {
                Gate::Refused(AGENTS_MAY_NOT_READ)
            }
        }
        // `history export` reads like `status`; `history import` writes files.
        Command::History { action } => match action {
            HistoryAction::Export { .. } => {
                if settings.agents_read {
                    Gate::Allowed
                } else {
                    Gate::Refused(AGENTS_MAY_NOT_READ)
                }
            }
            HistoryAction::Import { .. } => Gate::Refused(AGENTS_MAY_NOT_CHANGE),
        },
        Command::List { .. } | Command::Status { .. } | Command::Burn { .. } => {
            if settings.agents_read {
                Gate::Allowed
            } else {
                Gate::Refused(AGENTS_MAY_NOT_READ)
            }
        }
        Command::Use {
            save_current,
            discard_current,
            ..
        }
        | Command::Next {
            save_current,
            discard_current,
            ..
        } => {
            if save_current.is_some() || *discard_current {
                Gate::Refused(AGENTS_MAY_NOT_CHANGE)
            } else if settings.agents_switch {
                Gate::Allowed
            } else {
                Gate::Refused(AGENTS_MAY_NOT_SWITCH)
            }
        }
        // `codex list` reads like `status`; `codex switch` is a switch between
        // logins the person saved, like `use`.
        Command::Codex { action } => match action {
            CodexAction::List { .. } => {
                if settings.agents_read {
                    Gate::Allowed
                } else {
                    Gate::Refused(AGENTS_MAY_NOT_READ)
                }
            }
            CodexAction::Switch { .. } => {
                if settings.agents_switch {
                    Gate::Allowed
                } else {
                    Gate::Refused(AGENTS_MAY_NOT_SWITCH)
                }
            }
        },
        Command::Add { .. }
        | Command::Rm { .. }
        | Command::Edit { .. }
        | Command::AgentSkill { .. }
        | Command::Panel { .. }
        | Command::Window { .. } => Gate::Refused(AGENTS_MAY_NOT_CHANGE),
    }
}

/// The subcommand, its positional values and the names of its flags, without
/// free-text values (`--label`, `--abbr`, `--color`, `--save-current`).
fn command_text(command: &Command) -> String {
    match command {
        Command::List { refresh } => flags("list", &[("--refresh", *refresh)]),
        Command::Use {
            key,
            save_current,
            discard_current,
        } => flags(
            &format!("use {key}"),
            &[
                ("--save-current", save_current.is_some()),
                ("--discard-current", *discard_current),
            ],
        ),
        Command::Next {
            save_current,
            discard_current,
        } => flags(
            "next",
            &[
                ("--save-current", save_current.is_some()),
                ("--discard-current", *discard_current),
            ],
        ),
        Command::Add { label } => flags("add", &[("--label", label.is_some())]),
        Command::Rm { key, yes } => flags(&format!("rm {key}"), &[("--yes", *yes)]),
        Command::Edit {
            key,
            label,
            abbr,
            color,
        } => flags(
            &format!("edit {key}"),
            &[
                ("--label", label.is_some()),
                ("--abbr", abbr.is_some()),
                ("--color", color.is_some()),
            ],
        ),
        Command::AgentSkill { action } => match action {
            SkillAction::Install { force } => flags("agent-skill install", &[("--force", *force)]),
            SkillAction::Uninstall { force } => {
                flags("agent-skill uninstall", &[("--force", *force)])
            }
        },
        Command::Burn { action } => match action {
            BurnAction::Plan { json } => flags("burn plan", &[("--json", *json)]),
            BurnAction::Simulate { without, .. } => {
                flags("burn simulate", &[("--without", !without.is_empty())])
            }
        },
        Command::Panel { view } => format!("panel {}", view.as_str()),
        Command::Window { target } => {
            format!("window {}", target.as_deref().unwrap_or("sessions"))
        }
        Command::History { action } => match action {
            HistoryAction::Export { format, from, to } => flags(
                &format!("history export --format {}", format.as_str()),
                &[("--from", from.is_some()), ("--to", to.is_some())],
            ),
            // The folder is free text; a path stays out of the recorded line.
            HistoryAction::Import { .. } => "history import".to_string(),
        },
        Command::Codex { action } => match action {
            CodexAction::List { json } => flags("codex list", &[("--json", *json)]),
            // The abbreviation is free text and stays out of the recorded line.
            CodexAction::Switch { .. } => "codex switch".to_string(),
        },
        Command::Statusline => "statusline".to_string(),
        Command::Prices { refresh } => flags("prices", &[("--refresh", *refresh)]),
        Command::Status {
            json,
            waybar,
            i3blocks,
            refresh,
        } => flags(
            "status",
            &[
                ("--json", *json),
                ("--waybar", *waybar),
                ("--i3blocks", *i3blocks),
                ("--refresh", *refresh),
            ],
        ),
    }
}

fn flags(base: &str, flags: &[(&str, bool)]) -> String {
    let mut text = base.to_string();
    for (name, set) in flags {
        if *set {
            text.push(' ');
            text.push_str(name);
        }
    }
    text
}

/// The CLI's one warning line: every warning this program prints goes through
/// it, so the prefix is written once.
fn warn(text: &str) {
    eprintln!("seatswap: warning: {text}");
}

/// Every agent call is one `agentCall` event, allowed or refused. A history
/// that cannot be written is one warning and never changes the call's result.
fn record_agent_call(context: &Context, command: &str, allowed: bool, cwd: Option<String>) {
    let result = (|| -> Result<(), String> {
        let recorder = ops::open_history(context.store.app_paths(), &context.settings)?;
        recorder
            .event(
                Utc::now(),
                EventKind::AgentCall {
                    command: command.to_string(),
                    cwd,
                    allowed,
                },
            )
            .map_err(|error| ops::history_unwritable(&error))
    })();
    if let Err(message) = result {
        warn(&message);
    }
}

fn current_cwd() -> Option<String> {
    match record_cwd(std::env::current_dir()) {
        Ok(cwd) => Some(cwd),
        Err(reason) => {
            warn(&format!(
                "{reason}; the agent call is recorded without a folder"
            ));
            None
        }
    }
}

/// The folder to record for an agent call: its text only when `current_dir`
/// could be read and its name is valid UTF-8; otherwise the reason it is left
/// out. A record is never silently given a replaced name or dropped.
fn record_cwd(dir: io::Result<PathBuf>) -> Result<String, String> {
    match dir {
        Ok(path) => match path.to_str() {
            Some(text) => Ok(text.to_string()),
            None => Err("the current folder's name is not UTF-8".to_string()),
        },
        Err(error) => Err(format!("the current folder cannot be read: {error}")),
    }
}

// ---------------------------------------------------------------------------
// Folder resolution and the store

/// The two folders every front end works with.
#[derive(Debug, Clone)]
pub(crate) struct Paths {
    pub app: AppPaths,
    pub claude: ClaudePaths,
}

impl Paths {
    pub(crate) fn into_parts(self) -> (AppPaths, ClaudePaths) {
        (self.app, self.claude)
    }
}

/// Resolve both folders in the order docs/implementation.md fixes:
/// Claude folder: `--claude-config-dir`, else the `claudeConfigDir` setting,
/// else `$CLAUDE_CONFIG_DIR`, else `~/.claude`. The data folder is resolved by
/// [`AppPaths::resolve`] first because the setting lives inside it.
pub(crate) fn resolve_paths(
    app: AppPaths,
    claude_config_dir: Option<&Path>,
    setting_claude_config_dir: Option<&Path>,
) -> Result<Paths, PathsError> {
    let claude = if let Some(dir) = claude_config_dir {
        ClaudePaths::resolve(Some(dir))?
    } else if let Some(dir) = setting_claude_config_dir {
        ClaudePaths::resolve(Some(dir))?
    } else {
        ClaudePaths::resolve(None)?
    };
    // This crate's own tests reach here with temporary folders. On macOS
    // `resolve` names the folder's Keychain item, which lives in the user's
    // real default keychain, so the tests always keep the login in the file.
    #[cfg(test)]
    let claude = ClaudePaths {
        login_keychain: None,
        ..claude
    };
    Ok(Paths { app, claude })
}

fn open_context(cli: &Cli) -> Result<Context, CliError> {
    let app = AppPaths::resolve(cli.data_dir.as_deref())?;
    let (store, warning) = store::open_store(app)?;
    if let Some(warning) = warning {
        warn(&warning);
    }
    let file = store.load()?;
    let paths = resolve_paths(
        store.app_paths().clone(),
        cli.claude_config_dir.as_deref(),
        file.settings.claude_config_dir.as_deref(),
    )?;
    let (_, claude) = paths.into_parts();
    // Before any command reads or writes the history: a changed history folder
    // gets this host's history first. The command runs either way.
    match ops::bring_history_along(store.app_paths(), &file.settings, ops::Bringer::Command) {
        Ok(warnings) => {
            for warning in &warnings {
                warn(warning);
            }
        }
        Err(error) => warn(&error),
    }
    Ok(Context {
        store,
        claude,
        settings: file.settings,
    })
}

/// The line `seatswap panel settings` sends: `settings` has no view of its own
/// in the pop-up any more, it opens the large window at Settings (spec §3.7).
fn panel_settings_line() -> String {
    ipc::window_line("settings")
}

/// `seatswap panel`: ask the running tray app to open a view.
fn panel(cli: &Cli, view: PanelView) -> Result<(), CliError> {
    let app = AppPaths::resolve(cli.data_dir.as_deref())?;
    match view {
        PanelView::Settings => ipc::send_line(&app, &panel_settings_line())?,
        PanelView::Main | PanelView::Add => ipc::send_open_panel(&app, view.as_str())?,
    }
    Ok(())
}

/// `seatswap window`: ask the running tray app to open the large window at an
/// area, `sessions` when none is given.
fn window(cli: &Cli, target: Option<&str>) -> Result<(), CliError> {
    let target = target.unwrap_or("sessions");
    crate::gui::window::parse_target(target)
        .map_err(|error| CliError::Usage(format!("window {target:?}: {error}")))?;
    let app = AppPaths::resolve(cli.data_dir.as_deref())?;
    ipc::send_window(&app, target)?;
    Ok(())
}

// ---------------------------------------------------------------------------
// Commands

fn list(context: Context, refresh: bool) -> Result<(), CliError> {
    let now = Utc::now();
    let mut engine = Engine::new(context.claude, context.store, Api::new())?;
    let views = account_views(&mut engine, refresh, PollReason::PanelOpened, now)?;
    for view in &views {
        println!("{}", list_line(view, now));
    }
    Ok(())
}

fn use_account(
    context: Context,
    key: &str,
    save_current: Option<Option<String>>,
    discard_current: bool,
    by: By,
    cwd: Option<String>,
) -> Result<(), CliError> {
    let policy = unsaved_policy(save_current, discard_current);
    let now = Utc::now();
    let outcome = switcher::switch_to(&context.claude, &context.store, key, policy, now)
        .map_err(switch_error)?;
    println!("{}", switch_line(&context.store, &outcome.to));
    record_cli_switch(&context, &outcome, by, cwd, now);
    Ok(())
}

/// The switch is done; a history that cannot be written is a warning, never a
/// failed command.
fn record_cli_switch(
    context: &Context,
    outcome: &switcher::SwitchOutcome,
    by: By,
    cwd: Option<String>,
    at: DateTime<Utc>,
) {
    let result = (|| -> Result<(), String> {
        let file = context.store.load().map_err(|error| error.to_string())?;
        let target = file
            .find(&outcome.to)
            .ok_or_else(|| format!("the store has no account {}", outcome.to))?;
        let to = AccountRef::of(target);
        let from = outcome
            .from
            .as_deref()
            .and_then(|id| file.find(id))
            .map(AccountRef::of);
        let saved_new = outcome.saved_new.as_deref().and_then(|id| file.find(id));
        let recorder = ops::open_history(context.store.app_paths(), &context.settings)?;
        let added = saved_new.is_some();
        ops::record_switch(&recorder, from, to, saved_new, by, None, cwd, None, at)
            .map_err(|error| error.to_string())?;
        if let Some(change) = &outcome.plan_change {
            ops::record_plan_change(&recorder, change, at).map_err(|error| error.to_string())?;
        }
        if added {
            share_new_account(context, &recorder);
        }
        Ok(())
    })();
    if let Err(error) = result {
        warn(&format!(
            "the switch was not written to the history: {error}"
        ));
    }
}

/// The switch already happened, so an account list that cannot name it is a
/// sentence, not a failed command.
fn switch_line(store: &Store, account_id: &str) -> String {
    match ops::switch_message(store, account_id) {
        Ok(message) => message,
        Err(error) => ops::switched_without_list(&error),
    }
}

fn next_account(
    context: Context,
    save_current: Option<Option<String>>,
    discard_current: bool,
    by: By,
    cwd: Option<String>,
) -> Result<String, CliError> {
    // Auto-burn decides on its own: an agent's `next` must not switch behind
    // it (spec §6.2). Nothing is loaded or written for this answer.
    if by == By::Agent && context.settings.auto_burn {
        return Ok(AUTO_BURN_DECIDES.to_string());
    }
    let policy = unsaved_policy(save_current, discard_current);
    let now = Utc::now();

    let mut file = context.store.load()?;
    let mut live = switcher::live_state(&context.claude, &file)?;
    if let LiveState::Unknown { email, .. } = &live {
        match &policy {
            UnsavedPolicy::Refuse => {
                return Err(CliError::UnsavedCurrent(unsaved_message(email.clone())))
            }
            UnsavedPolicy::Save { label } => {
                // Saving first turns the live login into an account, so "next
                // after the live one" has a position in the list.
                let recorder = cli_history(&context);
                let (_, history_error) = ops::save_live_login(
                    &context.claude,
                    &context.store,
                    recorder.as_ref(),
                    label.clone(),
                    now,
                )
                .map_err(CliError::Message)?;
                if let Some(error) = history_error {
                    warn(&error);
                }
                if let Some(recorder) = &recorder {
                    share_new_account(&context, recorder);
                }
                file = context.store.load()?;
                live = switcher::live_state(&context.claude, &file)?;
            }
            UnsavedPolicy::Discard => {}
        }
    }
    if let LiveState::Unknown { email, .. } = &live {
        return Err(CliError::UnsavedCurrent(unsaved_message(email.clone())));
    }

    let target = next_target(&file, &live)?;
    let outcome = switcher::switch_to(&context.claude, &context.store, &target, policy, now)
        .map_err(switch_error)?;
    let line = switch_line(&context.store, &outcome.to);
    record_cli_switch(&context, &outcome, by, cwd, now);
    Ok(line)
}

/// After a login was saved as an account: a login of an account the other
/// hosts know takes its shared look and price at once (spec §5,
/// [`shared::sync`]). What cannot be shared or applied is a warning, never a
/// failed command.
fn share_new_account(context: &Context, recorder: &Recorder) {
    match shared::sync(&context.store, recorder, Utc::now()) {
        Ok(applied) => {
            for warning in &applied.warnings {
                warn(warning);
            }
        }
        Err(error) => warn(&error.to_string()),
    }
}

/// The CLI's history recorder, or `None` with the reason printed once. The
/// command itself never fails because the history cannot be opened.
fn cli_history(context: &Context) -> Option<Recorder> {
    match ops::open_history(context.store.app_paths(), &context.settings) {
        Ok(recorder) => Some(recorder),
        Err(error) => {
            warn(&error);
            None
        }
    }
}

fn add(context: Context, label: Option<String>) -> Result<(), CliError> {
    let recorder = cli_history(&context);
    let (account, history_error) = ops::save_live_login(
        &context.claude,
        &context.store,
        recorder.as_ref(),
        label,
        Utc::now(),
    )
    .map_err(CliError::Message)?;
    if let Some(error) = history_error {
        warn(&error);
    }
    // A known account's shared look replaces the one just picked, so the line
    // names the account as it is stored now.
    let account = match &recorder {
        Some(recorder) => {
            share_new_account(&context, recorder);
            context
                .store
                .load()?
                .find(&account.id)
                .cloned()
                .ok_or_else(|| {
                    CliError::Message(format!("the store has no account {}", account.id))
                })?
        }
        None => account,
    };
    println!("Saved {} — {}", account.abbr, account.label);
    Ok(())
}

fn remove(context: Context, key: &str, yes: bool) -> Result<(), CliError> {
    let file = context.store.load()?;
    let account = file
        .find_by_key(key)
        .ok_or_else(|| CliError::Message(format!("unknown account {key}")))?;
    if !yes && !confirm_remove(account)? {
        println!("Not removed.");
        return Ok(());
    }
    let recorder = cli_history(&context);
    let (removed, history_error) =
        ops::remove_account(&context.store, key, recorder.as_ref(), Utc::now())
            .map_err(CliError::Message)?;
    println!("Removed {} — {}", removed.abbr, removed.label);
    if let Some(error) = history_error {
        warn(&error);
    }
    Ok(())
}

/// Ask `Remove AC — Acme? [y/N]` on a terminal. stdin that is not a
/// terminal is refused: a script must pass `--yes`.
fn confirm_remove(account: &Account) -> Result<bool, CliError> {
    if !io::stdin().is_terminal() {
        return Err(CliError::Message(format!(
            "refusing to remove {} — {} without --yes (stdin is not a terminal)",
            account.abbr, account.label
        )));
    }
    print!("Remove {} — {}? [y/N] ", account.abbr, account.label);
    io::stdout().flush()?;
    let mut answer = String::new();
    io::stdin().read_line(&mut answer)?;
    let answer = answer.trim();
    Ok(answer.eq_ignore_ascii_case("y") || answer.eq_ignore_ascii_case("yes"))
}

fn edit(
    context: Context,
    key: &str,
    label: Option<String>,
    abbr: Option<String>,
    color: Option<String>,
) -> Result<(), CliError> {
    if label.is_none() && abbr.is_none() && color.is_none() {
        return Err(CliError::Message(
            "nothing to change; give --label, --abbr or --color".to_string(),
        ));
    }
    let edited = switcher::edit_account(
        &context.store,
        key,
        label.as_deref(),
        abbr.as_deref(),
        color.as_deref(),
        None,
        None,
    )?;
    println!("Updated {} — {}", edited.account.abbr, edited.account.label);
    // The edit is done; the other hosts take it from the history (spec §5).
    let recorder = cli_history(&context);
    if let Some(error) = ops::record_edit(recorder.as_ref(), &edited, Utc::now()) {
        warn(&error);
    }
    Ok(())
}

// ---------------------------------------------------------------------------
// Codex logins (R8)

/// One saved Codex login as `seatswap codex list --json` prints it: the
/// snapshot's entry without `usage` (the CLI asks no usage and does not poll),
/// and without `state`/`message`, which only a provider poll can know.
#[derive(Debug, Serialize)]
#[serde(rename_all = "camelCase")]
struct CodexLoginRow {
    account: AccountRef,
    abbr: String,
    label: String,
    color: String,
    plan: Option<String>,
    live: bool,
}

/// `seatswap codex`: Codex's folder is the `codexHome` setting, then
/// `$CODEX_HOME`, then `~/.codex`, as the app resolves it.
fn codex(context: Context, action: CodexAction) -> Result<(), CliError> {
    let paths = CodexPaths::resolve(context.settings.codex_home.as_deref())
        .map_err(|error| CliError::Message(error.to_string()))?;
    codex_at(context, &paths, action)
}

/// [`codex`] with Codex's folder given, so a test passes a fake folder
/// (`CodexPaths::at`) and never resolves the real one.
fn codex_at(context: Context, paths: &CodexPaths, action: CodexAction) -> Result<(), CliError> {
    match action {
        CodexAction::List { json } => {
            if json {
                println!("{}", serde_json::to_string(&codex_rows(&context, paths)?)?);
            } else {
                for line in codex_lines(&context, paths)? {
                    println!("{line}");
                }
            }
            Ok(())
        }
        CodexAction::Switch { abbr } => {
            let subscriptions = codex_subscriptions(&context)?;
            let target = match subscriptions
                .iter()
                .find(|subscription| subscription.abbr.eq_ignore_ascii_case(&abbr))
            {
                Some(target) => target,
                None => {
                    let file = context
                        .store
                        .load()
                        .map_err(|error| CliError::Message(error.to_string()))?;
                    if file
                        .accounts
                        .iter()
                        .any(|account| account.abbr.eq_ignore_ascii_case(&abbr))
                    {
                        return Err(CliError::Usage(format!(
                            "codex switch {abbr:?}: {abbr} is a Claude account, not a Codex login"
                        )));
                    }
                    return Err(CliError::Usage(format!(
                        "codex switch {abbr:?}: no saved Codex login has that abbreviation"
                    )));
                }
            };
            let recorder = cli_history(&context);
            let switched = ops::switch_codex(
                context.store.app_paths(),
                paths,
                &context.store,
                recorder.as_ref(),
                &target.account,
                Utc::now(),
            )
            .map_err(CliError::Message)?;
            for warning in &switched.warnings {
                warn(warning);
            }
            if !switched.note.is_empty() {
                println!("{}", switched.note);
            }
            Ok(())
        }
    }
}

/// The saved logins `seatswap codex list` prints, live first and then by
/// abbreviation. Reading only: no login is saved and no subscription is made.
fn codex_rows(context: &Context, paths: &CodexPaths) -> Result<Vec<CodexLoginRow>, CliError> {
    let subscriptions = codex_subscriptions(context)?;
    let live = codex_live(paths);
    let mut rows: Vec<CodexLoginRow> = subscriptions
        .into_iter()
        .map(|subscription| CodexLoginRow {
            live: live.as_ref() == Some(&subscription.account),
            account: subscription.account,
            abbr: subscription.abbr,
            label: subscription.label,
            color: subscription.color,
            plan: subscription.plan,
        })
        .collect();
    rows.sort_by(|first, second| (!first.live, &first.abbr).cmp(&(!second.live, &second.abbr)));
    Ok(rows)
}

/// The lines `seatswap codex list` prints, one per saved login.
fn codex_lines(context: &Context, paths: &CodexPaths) -> Result<Vec<String>, CliError> {
    Ok(codex_rows(context, paths)?.iter().map(codex_line).collect())
}

/// The subscriptions whose Codex login seatswap has a saved copy of, in
/// `accounts.json` order. A subscription whose copy was forgotten is not a saved
/// login and is left out.
fn codex_subscriptions(context: &Context) -> Result<Vec<ProviderSubscription>, CliError> {
    let file = context
        .store
        .load()
        .map_err(|error| CliError::Message(error.to_string()))?;
    let mut saved = Vec::new();
    for subscription in file
        .provider_subscriptions
        .into_iter()
        .filter(|subscription| subscription.account.org == CODEX_ORG)
    {
        let copy = context
            .store
            .secrets()
            .get_codex(&providers::codex::secret_id(&subscription.account))
            .map_err(|error| CliError::Message(error.to_string()))?;
        if copy.is_some() {
            saved.push(subscription);
        }
    }
    Ok(saved)
}

/// The account Codex's login file holds now, read without saving anything
/// ([`providers::codex::live_account`]); `None` when there is no login. Any
/// other failure is said out loud — the list cannot know which login is live
/// then.
fn codex_live(paths: &CodexPaths) -> Option<AccountRef> {
    match providers::codex::live_account(paths) {
        Ok(live) => live,
        Err(providers::ProviderError::NoLogin { .. }) => None,
        Err(error) => {
            warn(&format!("the live Codex login was not read: {error}"));
            None
        }
    }
}

/// One line of `seatswap codex list`: `* OA  Plus  live`.
fn codex_line(row: &CodexLoginRow) -> String {
    format!(
        "{} {:<3} {:<5} {}",
        if row.live { "*" } else { " " },
        row.abbr,
        row.plan.as_deref().unwrap_or("\u{2014}"),
        if row.live { "live" } else { "" }
    )
    .trim_end()
    .to_string()
}

/// `agent-skill install` / `uninstall`: the file below
/// `<claude config dir>/skills/seatswap/`. Nothing outside these two commands
/// writes to Claude Code's config folder.
fn agent_skill(context: &Context, action: SkillAction) -> Result<(), CliError> {
    match action {
        SkillAction::Install { force } => {
            let before = agent_skill::status(&context.claude).map_err(skill_error)?;
            let status = agent_skill::install(&context.claude, force).map_err(skill_error)?;
            if before.installed && !before.differs {
                println!("already installed: {}", status.path.display());
            } else {
                println!("{}", status.path.display());
            }
        }
        SkillAction::Uninstall { force } => {
            let before = agent_skill::status(&context.claude).map_err(skill_error)?;
            if !before.installed {
                println!("not installed: {}", before.path.display());
                return Ok(());
            }
            let status = agent_skill::uninstall(&context.claude, force).map_err(skill_error)?;
            println!("removed {}", status.path.display());
        }
    }
    Ok(())
}

/// Keep the command line's old texts and exit codes: a differing file was a
/// message (exit 1) and a file error was a store error naming the path.
fn skill_error(error: SkillError) -> CliError {
    match error {
        SkillError::DiffersOnInstall { path } => CliError::Message(format!(
            "{} already exists and differs from seatswap's bundled skill; use --force to replace it",
            path.display()
        )),
        SkillError::DiffersOnUninstall { path } => CliError::Message(format!(
            "{} differs from seatswap's bundled skill; use --force to remove it",
            path.display()
        )),
        SkillError::Io { path, source } => CliError::Store(StoreError::Io { path, source }),
    }
}

fn status(
    context: Context,
    json: bool,
    waybar: bool,
    i3blocks: bool,
    refresh: bool,
) -> Result<(), CliError> {
    let now = Utc::now();
    let offset = *Local::now().offset();
    let mut engine = Engine::new(context.claude, context.store, Api::new())?;
    let views = account_views(&mut engine, refresh, PollReason::Timer, now)?;

    if json {
        // Only the JSON form shows auto-burn, and the history read costs four
        // weeks of files: the printed forms must not read it (waybar runs this
        // every few seconds).
        let accounts = engine.store().load()?.accounts;
        let active = views
            .iter()
            .find(|view| view.is_active)
            .map(|view| view.id.clone());
        let burn = burn_status(
            engine.store().app_paths(),
            &context.settings,
            &views,
            &accounts,
            active.as_deref(),
            now,
            offset,
        );
        // `status --json` has no warnings field: the history read's warnings go
        // to stderr, through the one warning line, and nowhere else.
        for warning in &burn.warnings {
            warn(warning);
        }
        let output = StatusJson::new(&views, active, &burn);
        println!("{}", serde_json::to_string(&output)?);
    } else {
        if waybar {
            println!("{}", serde_json::to_string(&waybar_line(&views, now))?);
        } else if i3blocks {
            let (full, short, color) = i3blocks_lines(&views, now);
            println!("{full}");
            println!("{short}");
            println!("{color}");
        } else if let Some(view) = active_view(&views) {
            println!("{}", status_line(view, now));
        } else {
            println!("no live account");
        }
    }
    Ok(())
}

/// `seatswap prices`: the stored list as a table, or a fresh look-up first.
fn prices(context: Context, refresh: bool) -> Result<(), CliError> {
    let app = context.store.app_paths().clone();
    if refresh {
        if !context.settings.look_up_prices {
            return Err(CliError::Message(
                "look-ups are off (lookUpPrices); turn them on in the settings to refresh"
                    .to_string(),
            ));
        }
        match money::look_up_prices(&Api::new(), &app, Utc::now()) {
            Ok(money::Looked::Taken { left_out, .. }) => {
                for entry in &left_out {
                    println!("left out of models.dev's list: {entry}");
                }
            }
            Ok(money::Looked::NotModified) => {}
            Err(error) => {
                return Err(CliError::Message(format!(
                    "cannot refresh the API prices: {error}"
                )))
            }
        }
    }
    let stored = Prices::load(&app)
        .map_err(|error| CliError::Message(format!("the prices cannot be read: {error}")))?;
    let current = stored.current();
    if current.is_empty() {
        println!("no prices yet: run `seatswap prices --refresh` while online.");
        return Ok(());
    }

    let labelled: Vec<(String, money::ModelPrice)> = current
        .into_iter()
        .map(|row| {
            let label = if row.price.tiered {
                format!("{} *", row.key)
            } else {
                row.key
            };
            (label, row.price)
        })
        .collect();
    let key_width = labelled
        .iter()
        .map(|(key, _)| key.chars().count())
        .max()
        .unwrap_or(3)
        .max("key".len());

    println!("USD per million tokens");
    println!(
        "{:<key_width$}  {:>8}  {:>8}  {:>11}  {:>10}",
        "key", "input", "output", "cache write", "cache read"
    );
    for (key, price) in &labelled {
        println!(
            "{:<key_width$}  {:>8}  {:>8}  {:>11}  {:>10}",
            key,
            price_number(price.input),
            price_number(price.output),
            price_number(price.cache_write),
            price_number(price.cache_read)
        );
    }
    if labelled.iter().any(|(_, price)| price.tiered) {
        println!();
        println!("* the price depends on the prompt size; the base price is shown");
    }
    Ok(())
}

fn price_number(value: f64) -> String {
    if value.fract() == 0.0 {
        format!("{value:.0}")
    } else {
        format!("{value}")
    }
}

/// `seatswap history export --format csv|json [--from …] [--to …]`: the UTC days
/// of the range, written to standard output.
fn history_export(
    context: &Context,
    format: ExportFormat,
    from: Option<&str>,
    to: Option<&str>,
) -> Result<(), CliError> {
    let range = export_day_range(from, to)?;
    let history = history_dir(context.store.app_paths(), &context.settings)?;
    let mut out = io::stdout().lock();
    write_export(&history, format, &range, &mut out)?;
    out.flush()?;
    Ok(())
}

/// The export's rows, written to `out`.
fn write_export(
    history: &HistoryDir,
    format: ExportFormat,
    range: &Range,
    out: &mut dyn Write,
) -> Result<(), CliError> {
    match format {
        ExportFormat::Csv => export_csv(history, range, out),
        ExportFormat::Json => export_json(history, range, out),
    }
    .map_err(|error| CliError::Message(format!("cannot read the history: {error}")))
}

/// The range of two UTC days; `--to` is inclusive, so the range ends at the
/// midnight after it. A day that is not given leaves that end open; `--from`
/// after `--to` is an error.
fn export_day_range(from: Option<&str>, to: Option<&str>) -> Result<Range, CliError> {
    let first = match from {
        Some(text) => Some(parse_date("--from", text)?),
        None => None,
    };
    let last = match to {
        Some(text) => Some(parse_date("--to", text)?),
        None => None,
    };
    if let (Some(first), Some(last)) = (first, last) {
        if first > last {
            return Err(CliError::Usage(format!(
                "--from {first} is after --to {last}"
            )));
        }
    }
    let from = match first {
        Some(day) => Some(utc_midnight(day)?),
        None => None,
    };
    let to = match last {
        Some(day) => {
            let next = day
                .succ_opt()
                .ok_or_else(|| CliError::Usage(format!("--to {day}: there is no day after it")))?;
            Some(utc_midnight(next)?)
        }
        None => None,
    };
    Range::between(from, to).map_err(|error| CliError::Message(error.to_string()))
}

/// Midnight UTC of a day: the export's days are UTC days, not local ones.
fn utc_midnight(date: NaiveDate) -> Result<DateTime<Utc>, CliError> {
    date.and_hms_opt(0, 0, 0)
        .map(|naive| Utc.from_utc_datetime(&naive))
        .ok_or_else(|| CliError::Usage(format!("{date}: the day has no midnight")))
}

/// `seatswap history import <folder>`: one line per host at least one file was
/// copied for (or "nothing new") and one per conflict; a conflict leaves an
/// error after everything that was copied.
fn history_import(context: &Context, folder: &str) -> Result<(), CliError> {
    let imported = import_history(context, folder)?;
    for warning in &imported.warnings {
        warn(warning);
    }
    if imported.copied.is_empty() {
        println!("nothing new");
    } else {
        for host in &imported.copied {
            println!("copied {host}");
        }
    }
    if imported.conflicts.is_empty() {
        return Ok(());
    }
    for conflict in &imported.conflicts {
        println!("conflict {}: {}", conflict.host, conflict.reason);
    }
    Err(CliError::Message(format!(
        "{} host folder(s) were not imported",
        imported.conflicts.len()
    )))
}

/// Copy another history folder's host folders in.
fn import_history(context: &Context, folder: &str) -> Result<Imported, CliError> {
    let path = crate::format::expand_tilde(folder, &context.claude.home);
    if !path.is_absolute() {
        return Err(CliError::Usage(format!("{folder} is not an absolute path")));
    }
    let history = history_dir(context.store.app_paths(), &context.settings)?;
    history
        .import(&path)
        .map_err(|error| CliError::Message(error.to_string()))
}

/// The history folder `status` and `burn` read, without creating it.
fn history_dir(app: &AppPaths, settings: &Settings) -> Result<HistoryDir, CliError> {
    let (root, host) = seatswap_core::history::location(app, settings)
        .map_err(|error| CliError::Message(error.to_string()))?;
    Ok(HistoryDir::at(&root, &host))
}

/// What `status` shows of auto-burn, derived from one history read. A history
/// read or planner error is kept in `error` so `status` can answer anyway;
/// `next` and `slack_hours` are then absent. `warnings` are the history read's
/// own warnings, printed once by `status`.
#[derive(Debug, Default)]
struct BurnStatus {
    on: bool,
    paused_until: Option<DateTime<Utc>>,
    next: Option<String>,
    suggestion: Option<SuggestionJson>,
    slack_hours: BTreeMap<String, f64>,
    error: Option<String>,
    warnings: Vec<String>,
}

/// Read the history once and give the auto-burn facts `status` prints. It never
/// fails: a history or planner error goes into [`BurnStatus::error`] and leaves
/// `next` and `slack_hours` empty (spec §6.2).
#[allow(clippy::too_many_arguments)]
fn burn_status(
    app: &AppPaths,
    settings: &Settings,
    views: &[AccountView],
    accounts: &[Account],
    active: Option<&str>,
    now: DateTime<Utc>,
    offset: FixedOffset,
) -> BurnStatus {
    let mut status = BurnStatus {
        on: settings.auto_burn,
        ..BurnStatus::default()
    };
    let history = match history_dir(app, settings) {
        Ok(history) => history,
        Err(error) => {
            status.error = Some(error.to_string());
            return status;
        }
    };
    let prices = match Prices::load(app) {
        Ok(prices) => prices,
        Err(error) => {
            status.error = Some(format!("the prices cannot be read: {error}"));
            return status;
        }
    };
    let learned = match live::learn(
        &history,
        now,
        offset,
        accounts,
        settings.read_sessions,
        &prices,
    ) {
        Ok(learned) => learned,
        Err(error) => {
            status.error = Some(format!("cannot read the history: {error}"));
            return status;
        }
    };
    status.warnings = learned.warnings.clone();
    let active = active.filter(|id| accounts.iter().any(|account| account.id == *id));
    let readiness = match live::situation(&learned, views, accounts, active, now, offset, 0, &[]) {
        Ok(readiness) => readiness,
        Err(error) => {
            status.error = Some(error.to_string());
            return status;
        }
    };
    let Readiness::Ready(situation) = readiness else {
        return status;
    };
    let assessment = live::assess(situation, None);
    status.paused_until = assessment
        .situation
        .inputs
        .manual_hold_until
        .filter(|until| *until > now);
    status.next = assessment.next.clone();

    let typical = assessment.situation.inputs.rhythm.typical_hour();
    for account in &assessment.situation.inputs.accounts {
        if let Some(slack_usd) = slack(account, &assessment.situation.inputs) {
            if let Some(hours) = usd_hours(slack_usd, typical) {
                status.slack_hours.insert(account.id.clone(), hours);
            }
        }
    }

    if !settings.auto_burn && settings.suggest_switches {
        match burn_text::suggestion(
            &assessment.decision,
            &assessment.situation.inputs,
            active,
            views,
            now,
            offset,
        ) {
            Ok(suggestion) => {
                status.suggestion = suggestion.map(|suggestion| SuggestionJson {
                    to_id: suggestion.to,
                    text: suggestion.text,
                });
            }
            Err(error) => status.error = Some(error.to_string()),
        }
    }
    status
}

/// `seatswap burn`: print the plan or the replay.
fn burn(context: Context, action: BurnAction) -> Result<(), CliError> {
    match action {
        BurnAction::Plan { json } => println!("{}", burn_plan(context, json)?),
        BurnAction::Simulate { from, to, without } => {
            println!("{}", burn_simulate(context, &from, &to, &without)?)
        }
    }
    Ok(())
}

/// One JSON object of `seatswap burn plan --json`.
#[derive(Debug, Serialize)]
#[serde(rename_all = "camelCase")]
struct BurnPlanJson {
    regime: BurnRegimeJson,
    segments: Vec<BurnSegmentJson>,
    left_at_reset: BTreeMap<String, f64>,
}

#[derive(Debug, Serialize)]
#[serde(rename_all = "camelCase")]
struct BurnRegimeJson {
    early: bool,
    why: String,
    blocked_lazy_h: Option<f64>,
    blocked_eager_h: Option<f64>,
    extra_switches: u32,
    cost_h: Option<f64>,
}

#[derive(Debug, Serialize)]
#[serde(rename_all = "camelCase")]
struct BurnSegmentJson {
    account: Option<String>,
    from: DateTime<Utc>,
    to: DateTime<Utc>,
}

/// The regime's numbers as `burn plan --json` prints them. Both blocked-hours
/// fields name a policy on purpose, so both plans are computed explicitly: the
/// command's segments are the plan auto-burn follows, which is eager while
/// early switching is on, and it cannot answer for the lazy plan.
fn burn_regime_json(inputs: &Inputs, regime: &Regime) -> BurnRegimeJson {
    let typical = inputs.rhythm.typical_hour();
    let lazy = follow(inputs, Policy::Lazy);
    let eager = follow(inputs, Policy::Eager);
    BurnRegimeJson {
        early: regime.early,
        why: burn_text::why(regime.why).to_string(),
        blocked_lazy_h: usd_hours(lazy.blocked_usd, typical),
        blocked_eager_h: usd_hours(eager.blocked_usd, typical),
        extra_switches: regime.extra_switches,
        cost_h: regime.cost_h,
    }
}

/// `seatswap burn plan [--json]`: the early-switching regime and the plan
/// auto-burn follows (eager while early switching is on), read exactly as the
/// tray app reads them (`tied_busy` is 0: the CLI has no session count).
fn burn_plan(context: Context, json: bool) -> Result<String, CliError> {
    let now = Utc::now();
    let offset = *Local::now().offset();
    let history = history_dir(context.store.app_paths(), &context.settings)?;
    let engine = Engine::new(context.claude, context.store, Api::new())?;
    let views = engine.views(now)?;
    let accounts = engine.store().load()?.accounts;
    let active = views
        .iter()
        .find(|view| view.is_active)
        .map(|view| view.id.as_str());
    let prices = Prices::load(engine.store().app_paths())
        .map_err(|error| CliError::Message(format!("the prices cannot be read: {error}")))?;
    let learned = live::learn(
        &history,
        now,
        offset,
        &accounts,
        context.settings.read_sessions,
        &prices,
    )
    .map_err(|error| CliError::Message(format!("cannot read the history: {error}")))?;
    for warning in &learned.warnings {
        warn(warning);
    }
    let readiness = live::situation(&learned, &views, &accounts, active, now, offset, 0, &[])
        .map_err(|error| CliError::Message(error.to_string()))?;
    let situation = match readiness {
        Readiness::Ready(situation) => situation,
        Readiness::Unread { accounts } => {
            let names = accounts
                .iter()
                .map(|id| burn_text::abbr(&views, id))
                .collect::<Result<Vec<_>, _>>()?
                .join(", ");
            return Err(CliError::Usage(format!("no reading for {names}")));
        }
        Readiness::NoAccounts => {
            return Err(CliError::Usage(
                "no account has a login with a reading".to_string(),
            ))
        }
    };
    let assessment = live::assess(situation, None);
    let regime = &assessment.regime;
    let plan = &assessment.plan;
    if json {
        let output = BurnPlanJson {
            regime: burn_regime_json(&assessment.situation.inputs, regime),
            segments: plan
                .segments
                .iter()
                .map(|segment| BurnSegmentJson {
                    account: segment.account.clone(),
                    from: segment.from,
                    to: segment.to,
                })
                .collect(),
            left_at_reset: plan.left_at_reset.clone(),
        };
        return Ok(serde_json::to_string(&output)?);
    }

    let mut lines = vec![burn_text::why(regime.why).to_string()];
    for segment in &plan.segments {
        lines.push(burn_text::segment_line(
            segment.account.as_deref(),
            &views,
            segment.from,
            segment.to,
            now,
            offset,
        )?);
    }
    Ok(lines.join("\n"))
}

/// `seatswap burn simulate --from … --to … [--without ABBR]…`: replay the
/// recorded work of the local days `from` … `to` (inclusive) for both policies.
fn burn_simulate(
    context: Context,
    from: &str,
    to: &str,
    without: &[String],
) -> Result<String, CliError> {
    let from_date = parse_date("--from", from)?;
    let to_date = parse_date("--to", to)?;
    if from_date > to_date {
        return Err(CliError::Usage(format!("--from {from} is after --to {to}")));
    }
    // `--to` is inclusive: the range ends at the local midnight of the day
    // after it. A fixed 24 h would drop the last hour of a day with a clock
    // change.
    let day_after = to_date
        .succ_opt()
        .ok_or_else(|| CliError::Usage(format!("--to {to} has no day after it")))?;
    let range = Range {
        from: local_midnight(from_date)?,
        to: local_midnight(day_after)?,
    };
    let now = Utc::now();
    let offset = *Local::now().offset();
    let history = history_dir(context.store.app_paths(), &context.settings)?;
    let file = context.store.load()?;
    let mut excluded = Vec::new();
    for key in without {
        let account = file
            .find_by_key(key)
            .ok_or_else(|| CliError::Usage(format!("unknown account {key}")))?;
        excluded.push(AccountRef::of(account));
    }
    let accounts = file.accounts;
    let prices = Prices::load(context.store.app_paths())
        .map_err(|error| CliError::Message(format!("the prices cannot be read: {error}")))?;
    let learned = live::learn(
        &history,
        now,
        offset,
        &accounts,
        context.settings.read_sessions,
        &prices,
    )
    .map_err(|error| CliError::Message(format!("cannot read the history: {error}")))?;
    // The history read's warnings come once, before either replay's own: a
    // warning the replay repeats is not printed again.
    let mut printed = BTreeSet::new();
    for warning in &learned.warnings {
        if printed.insert(warning.clone()) {
            warn(warning);
        }
    }
    let mut lines = Vec::new();
    for policy in [Policy::Lazy, Policy::Eager] {
        let result = replay(&history, &range, &learned.rates, &excluded, policy, &prices)
            .map_err(|error| CliError::Message(error.to_string()))?;
        // Both replays read the same files; a torn line is one warning.
        for warning in &result.warnings {
            if printed.insert(warning.clone()) {
                warn(warning);
            }
        }
        lines.push(burn_text::replay_report(&result, &accounts)?);
    }
    Ok(lines.join("\n"))
}

fn parse_date(flag: &str, text: &str) -> Result<NaiveDate, CliError> {
    NaiveDate::parse_from_str(text, "%Y-%m-%d").map_err(|error| {
        CliError::Usage(format!("{flag} {text:?} is not a YYYY-MM-DD date: {error}"))
    })
}

/// The UTC instant of a local midnight. An ambiguous or missing local midnight
/// is an error, never one of the two chosen silently.
fn local_midnight(date: NaiveDate) -> Result<DateTime<Utc>, CliError> {
    let midnight = date.and_time(NaiveTime::MIN);
    match Local.from_local_datetime(&midnight) {
        LocalResult::Single(at) => Ok(at.with_timezone(&Utc)),
        LocalResult::Ambiguous(first, second) => Err(CliError::Usage(format!(
            "the local midnight of {date} is ambiguous ({first} or {second})"
        ))),
        LocalResult::None => Err(CliError::Usage(format!(
            "there is no local midnight on {date}"
        ))),
    }
}

fn account_views(
    engine: &mut Engine<Api>,
    refresh: bool,
    reason: PollReason,
    now: DateTime<Utc>,
) -> Result<Vec<AccountView>, CliError> {
    if refresh {
        Ok(engine.poll(reason, now)?)
    } else {
        Ok(engine.views(now)?)
    }
}

fn unsaved_policy(save_current: Option<Option<String>>, discard_current: bool) -> UnsavedPolicy {
    if discard_current {
        UnsavedPolicy::Discard
    } else if let Some(label) = save_current {
        UnsavedPolicy::Save { label }
    } else {
        UnsavedPolicy::Refuse
    }
}

fn switch_error(error: SwitchError) -> CliError {
    match error {
        SwitchError::CurrentNotSaved { email } => CliError::UnsavedCurrent(unsaved_message(email)),
        other => CliError::Switch(other),
    }
}

fn unsaved_message(email: Option<String>) -> String {
    match email {
        Some(email) => format!(
            "the current login ({email}) is not saved; rerun with --save-current or --discard-current"
        ),
        None => "the current login is not saved; rerun with --save-current or --discard-current"
            .to_string(),
    }
}

/// The next Ok account after the live one, wrapping around. With no known live
/// account (no login, an ended login with no identity, or a discarded unknown
/// login) the first Ok account is next.
fn next_target(file: &AccountsFile, live: &LiveState) -> Result<String, CliError> {
    let current = match live {
        LiveState::Known { account_id } => Some(account_id.as_str()),
        LiveState::Ended { account_id } => account_id.as_deref(),
        LiveState::Unknown { .. } | LiveState::NoLogin => None,
    };
    let usable: Vec<&Account> = file
        .accounts
        .iter()
        .filter(|account| account.login_state == LoginState::Ok)
        .collect();
    if usable.is_empty() {
        return Err(CliError::Message(
            "no account with a usable login; add one with seatswap add".to_string(),
        ));
    }
    let index = match current.and_then(|id| usable.iter().position(|account| account.id == id)) {
        Some(position) => (position + 1) % usable.len(),
        None => 0,
    };
    Ok(usable[index].id.clone())
}

// ---------------------------------------------------------------------------
// Plain text

fn list_line(view: &AccountView, now: DateTime<Utc>) -> String {
    let mark = if view.is_active { "* " } else { "  " };
    let session = view
        .usage
        .as_ref()
        .and_then(Usage::session)
        .map(|limit| percent(limit.percent))
        .unwrap_or_else(|| "-".to_string());
    let week = view
        .usage
        .as_ref()
        .and_then(Usage::weekly_all)
        .map(|limit| percent(limit.percent))
        .unwrap_or_else(|| "-".to_string());

    let mut fields = vec![
        format!("{mark}{}", view.abbr),
        view.label.clone(),
        view.email.clone().unwrap_or_else(|| "-".to_string()),
        view.plan.clone().unwrap_or_else(|| "-".to_string()),
        format!("session {session}"),
        format!("week {week}"),
        view.login_days_left
            .map(login_ends_text)
            .unwrap_or_else(|| "-".to_string()),
    ];
    if view.login_state == LoginState::LoggedOut {
        fields.push("LOGGED OUT".to_string());
    }
    if view.usage.is_some() {
        if let Some(fetched_at) = view.fetched_at {
            fields.push(format!("({})", format_age(fetched_at, now)));
        }
    }
    if let Some(error) = &view.last_error {
        fields.push(format!("error: {error}"));
    }
    fields.join("  ")
}

fn status_line(view: &AccountView, now: DateTime<Utc>) -> String {
    match view.usage.as_ref().and_then(Usage::binding) {
        Some(limit) => {
            let mut text = format!("{} {}", view.abbr, percent(limit.percent));
            if let Some(resets_at) = limit.resets_at {
                text.push_str(" · ");
                text.push_str(&format_reset_at(resets_at, now, &Local));
            }
            text
        }
        None => view.abbr.clone(),
    }
}

fn active_view(views: &[AccountView]) -> Option<&AccountView> {
    views.iter().find(|view| view.is_active)
}

// ---------------------------------------------------------------------------
// Machine-readable output

#[derive(Debug, Serialize)]
#[serde(rename_all = "camelCase")]
struct StatusJson<'a> {
    views: Vec<StatusView<'a>>,
    active: Option<String>,
    suggestion: Option<SuggestionJson>,
    auto_burn: AutoBurnJson,
    next: Option<String>,
    burn_error: Option<String>,
}

impl<'a> StatusJson<'a> {
    fn new(views: &'a [AccountView], active: Option<String>, burn: &BurnStatus) -> StatusJson<'a> {
        StatusJson {
            views: views
                .iter()
                .map(|view| StatusView {
                    view,
                    slack_hours: burn.slack_hours.get(&view.id).copied(),
                })
                .collect(),
            active,
            suggestion: burn.suggestion.clone(),
            auto_burn: AutoBurnJson {
                on: burn.on,
                paused_until: burn.paused_until,
            },
            next: burn.next.clone(),
            burn_error: burn.error.clone(),
        }
    }
}

/// One account view plus the auto-burn number `status --json` adds to it. The
/// view's own fields are serialized flat, exactly as `seatswap list --json`
/// writes them.
#[derive(Debug, Serialize)]
#[serde(rename_all = "camelCase")]
struct StatusView<'a> {
    #[serde(flatten)]
    view: &'a AccountView,
    /// Working hours the account can be left alone and still be emptied before
    /// its reset; absent while unknown.
    #[serde(skip_serializing_if = "Option::is_none")]
    slack_hours: Option<f64>,
}

#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
struct AutoBurnJson {
    on: bool,
    paused_until: Option<DateTime<Utc>>,
}

#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
struct SuggestionJson {
    to_id: String,
    text: String,
}

#[derive(Debug, Serialize)]
struct WaybarLine {
    text: String,
    tooltip: String,
    class: &'static str,
    percentage: Option<i64>,
}

fn waybar_line(views: &[AccountView], now: DateTime<Utc>) -> WaybarLine {
    let Some(view) = active_view(views) else {
        return WaybarLine {
            text: "—".to_string(),
            tooltip: "no live account".to_string(),
            class: "none",
            percentage: None,
        };
    };
    let Some(usage) = &view.usage else {
        return waybar_without_usage(view);
    };
    let Some(binding) = usage.binding() else {
        return waybar_without_usage(view);
    };
    WaybarLine {
        text: format!("{} {}", view.abbr, percent(binding.percent)),
        tooltip: tooltip(view, usage, now),
        class: class_name(Some(binding.level())),
        percentage: Some(integer_percent(binding.percent)),
    }
}

fn waybar_without_usage(view: &AccountView) -> WaybarLine {
    WaybarLine {
        text: "—".to_string(),
        tooltip: format!("{} · {} — no usage yet", view.abbr, view.label),
        class: "none",
        percentage: None,
    }
}

fn i3blocks_lines(views: &[AccountView], now: DateTime<Utc>) -> (String, String, String) {
    match active_view(views) {
        Some(view) => {
            let level = view
                .usage
                .as_ref()
                .and_then(Usage::binding)
                .map(|limit| limit.level());
            let short = match view.usage.as_ref().and_then(Usage::binding) {
                Some(limit) => format!("{} {}", view.abbr, percent(limit.percent)),
                None => view.abbr.clone(),
            };
            let color = level.map(|level| level.color()).unwrap_or(NO_LOAD_COLOR);
            (status_line(view, now), short, color.to_string())
        }
        None => ("—".to_string(), "—".to_string(), NO_LOAD_COLOR.to_string()),
    }
}

#[cfg(test)]
pub(crate) mod tests {
    use super::*;

    use std::fs;

    use crate::agent_skill::AGENT_SKILL;

    use chrono::Duration;
    use chrono::Utc;
    use serde_json::{json, Value};

    use chrono::FixedOffset;
    use seatswap_core::api::{Limit, LimitKind, Severity, Usage};
    use seatswap_core::burn::decide::RegimeWhy;
    use seatswap_core::burn::plan::{AccountState, Inputs};
    use seatswap_core::burn::{Rates, Rhythm};
    use seatswap_core::engine::{AccountView, CacheEntry};
    use seatswap_core::history::{LimitSample, Sample, Source, TokenRecord};
    use seatswap_core::paths::ClaudePaths;
    use seatswap_core::project::ProjectKey;
    use seatswap_core::store::{FileSecrets, LoginState, Settings, StatuslineConfig, Store};
    use seatswap_core::types::OAuthAccount;

    /// The status line of an explicit data and Claude config folder, for the
    /// statusline module's tests. The command itself builds its `Cli` from the
    /// process's arguments; this only hands the same bytes through the same path.
    pub(crate) fn statusline_output_at(
        app: &AppPaths,
        claude: &ClaudePaths,
        stdin: &[u8],
        now: DateTime<Utc>,
    ) -> String {
        let cli = Cli {
            data_dir: Some(app.data_dir.clone()),
            claude_config_dir: Some(claude.config_dir.clone()),
            command: Some(Command::Statusline),
        };
        statusline_output(&cli, stdin, now)
    }

    const FIRST_ACCOUNT_UUID: &str = "4b7e2f91-3c5a-4d8e-9f1b-6a2c8e4d7b30";
    const FIRST_ORG_UUID: &str = "e2a6c9d1-7f4b-4a3e-8c5d-1b9f0a7e3c62";
    const SECOND_ACCOUNT_UUID: &str = "8e1d5b7a-6c3f-4a2b-9d8e-1f2a3b4c5d6e";
    const SECOND_ORG_UUID: &str = "5f2c8a1b-9d4e-4f6a-8b3c-7e2d1a9c4b5e";
    const THIRD_ACCOUNT_UUID: &str = "2b7e9c4d-1a3f-4c5e-8b6d-7f0a1c2e3d4b";
    const THIRD_ORG_UUID: &str = "9a1c3e5f-7b9d-4f2a-8c6e-0d1b3a5c7e9f";

    /// Two made-up Codex logins for `seatswap codex` (R8): the one in Codex's
    /// fake `auth.json` and the saved one the command switches to.
    const CODEX_LIVE_ID: &str = "0f3a6c2e-7b1d-4e9a-8c5f-2d4b6a8e0c13";
    const CODEX_OTHER_ID: &str = "7d2b9e4c-1a3f-4c5e-8b6d-9f0a1c2e3d4b";
    const CODEX_LIVE_AUTH: &str = r#"{"OPENAI_API_KEY":null,"auth_mode":"chatgpt","last_refresh":"2026-11-14T16:53:03.750022420Z","tokens":{"id_token":"eyJhbGciOi.id.sig","access_token":"eyJhbGciOi.access.sig","refresh_token":"rt_9f2c","account_id":"0f3a6c2e-7b1d-4e9a-8c5f-2d4b6a8e0c13","login_owner_id":"user-sam","oauth_provenance":{"issuer":"https://auth.openai.com","client_id":"app_4c1d2e3f4a5b6c7d8e9f0a1b2c3d4e5f"}}}"#;
    const CODEX_OTHER_AUTH: &str = r#"{"OPENAI_API_KEY":null,"auth_mode":"chatgpt","last_refresh":"2026-11-15T09:10:00.000000000Z","tokens":{"id_token":"eyJhbGciOi.other.id","access_token":"eyJhbGciOi.other.sig","refresh_token":"rt_51ab","account_id":"7d2b9e4c-1a3f-4c5e-8b6d-9f0a1c2e3d4b","login_owner_id":"user-sam","oauth_provenance":{"issuer":"https://auth.openai.com","client_id":"app_4c1d2e3f4a5b6c7d8e9f0a1b2c3d4e5f"}}}"#;

    struct Fixture {
        _tmp: tempfile::TempDir,
        home: PathBuf,
        config_dir: PathBuf,
        data_dir: PathBuf,
    }

    fn live(index: usize) -> (String, String, String, &'static str, &'static str) {
        match index {
            0 => (
                FIRST_ACCOUNT_UUID.to_string(),
                FIRST_ORG_UUID.to_string(),
                "one@example.com".to_string(),
                "Acme",
                "Acme",
            ),
            _ => (
                SECOND_ACCOUNT_UUID.to_string(),
                SECOND_ORG_UUID.to_string(),
                "two@example.com".to_string(),
                "Private",
                "Private",
            ),
        }
    }

    fn credentials_json(org_uuid: &str) -> String {
        json!({
            "claudeAiOauth": {
                "accessToken": format!("sk-ant-oat01-{}", "a".repeat(95)),
                "refreshToken": format!("sk-ant-ort01-{}", "b".repeat(95)),
                "expiresAt": 1_794_356_040_000_i64,
                "refreshTokenExpiresAt": 1_796_772_840_000_i64,
                "scopes": ["user:inference", "user:profile"],
                "subscriptionType": "team",
                "subscriptionTypeSource": "profile",
                "rateLimitTier": "default_claude_max_5x"
            },
            "organizationUuid": org_uuid
        })
        .to_string()
    }

    fn identity_json(account_uuid: &str, org_uuid: &str, email: &str, org_name: &str) -> String {
        json!({
            "numStartups": 41,
            "oauthAccount": {
                "accountUuid": account_uuid,
                "emailAddress": email,
                "organizationUuid": org_uuid,
                "organizationName": org_name,
                "organizationRole": "admin",
                "billingType": "stripe_subscription",
                "hasExtraUsageEnabled": false
            },
            "projects": {}
        })
        .to_string()
    }

    impl Fixture {
        fn new() -> Fixture {
            let tmp = tempfile::tempdir().expect("tempdir");
            let home = tmp.path().to_path_buf();
            let config_dir = home.join("claude");
            let data_dir = home.join("data");
            fs::create_dir_all(&config_dir).expect("config folder");
            fs::create_dir_all(&data_dir).expect("data folder");
            fs::write(
                data_dir.join("accounts.json"),
                json!({
                    "version": 1,
                    "accounts": [],
                    "settings": {"hostName": "atlas"},
                    "secretsBackend": "file"
                })
                .to_string(),
            )
            .expect("accounts file");
            let fixture = Fixture {
                _tmp: tmp,
                home,
                config_dir,
                data_dir,
            };
            fixture.write_live(0);
            fixture
        }

        fn write_live(&self, index: usize) {
            let (account_uuid, org_uuid, email, org_name, _label) = live(index);
            fs::write(
                self.config_dir.join(".credentials.json"),
                credentials_json(&org_uuid),
            )
            .expect("credentials");
            fs::write(
                self.config_dir.join(".claude.json"),
                identity_json(&account_uuid, &org_uuid, &email, org_name),
            )
            .expect("identity");
        }

        fn claude(&self) -> ClaudePaths {
            ClaudePaths::for_config_dir(&self.config_dir, &self.home)
        }

        fn store(&self) -> Store {
            Store::new(
                AppPaths::at(&self.data_dir),
                Box::new(FileSecrets::new(self.data_dir.join("secrets"))),
            )
        }

        fn add_account(&self, index: usize) {
            let (_, _, _, _, label) = live(index);
            ops::save_live_login(
                &self.claude(),
                &self.store(),
                None,
                Some(label.to_string()),
                Utc::now(),
            )
            .expect("the live login is saved");
        }

        /// A fake Codex folder with `CODEX_LIVE_AUTH` in `auth.json` and both
        /// logins as OpenAI subscriptions, the second one saved.
        fn codex_logins(&self) -> PathBuf {
            let home = self.home.join("codex");
            fs::create_dir_all(&home).expect("the Codex folder");
            fs::write(home.join("auth.json"), CODEX_LIVE_AUTH).expect("auth.json");
            let store = self.store();
            let mut file = store.load().expect("accounts");
            file.settings.codex_home = Some(home.clone());
            for (id, abbr, plan) in [
                (CODEX_LIVE_ID, "OA", Some("Plus")),
                (CODEX_OTHER_ID, "OA2", Some("Pro")),
            ] {
                file.provider_subscriptions.push(ProviderSubscription {
                    account: AccountRef {
                        acc: id.to_string(),
                        org: CODEX_ORG.to_string(),
                    },
                    label: "OpenAI".to_string(),
                    abbr: abbr.to_string(),
                    color: "#db2777".to_string(),
                    plan: plan.map(str::to_string),
                    price: None,
                    price_currency: None,
                });
            }
            store.save(&file).expect("accounts saved");
            let other = AccountRef {
                acc: CODEX_OTHER_ID.to_string(),
                org: CODEX_ORG.to_string(),
            };
            let login = serde_json::from_str(CODEX_OTHER_AUTH).expect("a Codex login");
            store
                .secrets()
                .set_codex(&providers::codex::secret_id(&other), &login)
                .expect("the saved copy");
            home
        }

        fn set_agents(&self, read: bool, switch: bool) {
            let store = self.store();
            let mut file = store.load().expect("accounts");
            file.settings.agents_read = read;
            file.settings.agents_switch = switch;
            store.save(&file).expect("settings saved");
        }

        fn cli(&self, command: Command) -> Cli {
            Cli {
                data_dir: Some(self.data_dir.clone()),
                claude_config_dir: Some(self.config_dir.clone()),
                command: Some(command),
            }
        }

        fn cli_without_command(&self) -> Cli {
            Cli {
                data_dir: Some(self.data_dir.clone()),
                claude_config_dir: Some(self.config_dir.clone()),
                command: None,
            }
        }

        fn run(&self, command: Command, caller: Caller) -> Result<(), CliError> {
            let mut cli = self.cli(command);
            let mut stdin: &[u8] = b"";
            execute(&mut cli, caller, &mut stdin)
        }

        fn events(&self) -> Vec<Value> {
            let folder = self.data_dir.join("history").join("atlas");
            let mut names = Vec::new();
            if let Ok(entries) = fs::read_dir(&folder) {
                for entry in entries {
                    let entry = entry.expect("history entry");
                    let name = entry.file_name().to_string_lossy().into_owned();
                    if name.starts_with("events-") && name.ends_with(".jsonl") {
                        names.push(entry.path());
                    }
                }
            }
            names.sort();
            let mut values = Vec::new();
            for name in names {
                let text = fs::read_to_string(name).expect("events file");
                for line in text.lines() {
                    values.push(serde_json::from_str(line).expect("an event line"));
                }
            }
            values
        }

        fn live_bytes(&self) -> (Vec<u8>, Vec<u8>) {
            (
                fs::read(self.config_dir.join(".credentials.json")).expect("credentials"),
                fs::read(self.config_dir.join(".claude.json")).expect("identity"),
            )
        }

        fn history(&self) -> HistoryDir {
            HistoryDir::at(&self.data_dir.join("history"), "atlas")
        }

        fn set_auto_burn(&self, on: bool) {
            let store = self.store();
            let mut file = store.load().expect("accounts");
            file.settings.auto_burn = on;
            store.save(&file).expect("settings saved");
        }

        fn account(&self, index: usize) -> Account {
            let (account_uuid, org_uuid, ..) = live(index);
            let store = self.store();
            let file = store.load().expect("accounts");
            file.accounts
                .iter()
                .find(|account| {
                    account.account_uuid == account_uuid && account.organization_uuid == org_uuid
                })
                .cloned()
                .expect("the saved account")
        }

        /// Put one usage reading in the cache, so the engine's views have it.
        fn set_reading(&self, account: &Account, week: f64, session: bool) {
            let now = Utc::now();
            let reset = now + Duration::days(3);
            let mut limits = vec![Limit {
                kind: LimitKind::WeeklyAll,
                percent: week,
                severity: Severity::Normal,
                resets_at: Some(reset),
                model: None,
                is_active: false,
            }];
            if session {
                limits.push(Limit {
                    kind: LimitKind::Session,
                    percent: 10.0,
                    severity: Severity::Normal,
                    resets_at: Some(reset),
                    model: None,
                    is_active: false,
                });
            }
            let app = AppPaths::at(&self.data_dir);
            let mut cache = UsageCache::load(&app).expect("the usage cache");
            cache.entries.insert(
                account.id.clone(),
                CacheEntry {
                    usage: Some(Usage {
                        limits,
                        extra_usage_enabled: false,
                        warnings: Vec::new(),
                    }),
                    fetched_at: Some(now),
                    ..CacheEntry::default()
                },
            );
            cache.save(&app).expect("the usage cache is saved");
        }

        /// One weekly reading in the history, for the rates a plan or replay learns.
        fn write_sample(&self, account: &Account, at: DateTime<Utc>, week: f64) {
            self.history()
                .append(&Sample {
                    t: at,
                    account: AccountRef::of(account),
                    src: Source::Poll,
                    l: vec![LimitSample {
                        k: LimitKind::WeeklyAll,
                        m: None,
                        p: week,
                        r: Some(at + Duration::days(3)),
                        b: false,
                    }],
                })
                .expect("the sample is written");
        }

        /// One hour of work of `claude-opus-5-5` in the history, worth
        /// `work_usd` API dollars, and `prices.json` with that model's price
        /// (4 USD per million input tokens) so the work can be valued.
        fn write_work(&self, at: DateTime<Utc>, work_usd: f64) {
            let mut prices = Prices::default();
            prices.take(
                BTreeMap::from([(
                    "anthropic/claude-opus-5-5".to_string(),
                    money::ModelPrice {
                        input: 4.0,
                        output: 20.0,
                        cache_write: 5.0,
                        cache_read: 0.2,
                        tiered: false,
                    },
                )]),
                at,
            );
            prices
                .save(&AppPaths::at(&self.data_dir))
                .expect("prices.json");
            self.history()
                .append(&TokenRecord {
                    t: at,
                    minutes: 60,
                    session: "7c1e4b2a-9d3f-4e6a-b8c5-2f0a1d7e9b34".to_string(),
                    cwd: "/home/sam/projects/atlas".to_string(),
                    project: ProjectKey::Repo("github.com/sam/atlas".to_string()),
                    model: "claude-opus-5-5".to_string(),
                    input: (work_usd * 250_000.0).round() as u64,
                    out: 0,
                    cw: 0,
                    cr: 0,
                    answers: 1,
                    agent_answers: 0,
                })
                .expect("the token record is written");
        }

        /// One hour of `claude-new-9`, a model `prices.json` does not list:
        /// `live::learn` names it once in its warnings.
        fn write_unpriced_work(&self, at: DateTime<Utc>) {
            self.history()
                .append(&TokenRecord {
                    t: at,
                    minutes: 60,
                    session: "b2c3d4e5-f6a7-4b8c-9d0e-1f2a3b4c5d6e".to_string(),
                    cwd: "/home/sam/projects/atlas".to_string(),
                    project: ProjectKey::Repo("github.com/sam/atlas".to_string()),
                    model: "claude-new-9".to_string(),
                    input: 30_000_000,
                    out: 0,
                    cw: 0,
                    cr: 0,
                    answers: 1,
                    agent_answers: 0,
                })
                .expect("the unpriced token record is written");
        }
    }

    fn settings(read: bool, switch: bool) -> Settings {
        Settings {
            agents_read: read,
            agents_switch: switch,
            ..Settings::default()
        }
    }

    fn plain_use(key: &str) -> Command {
        Command::Use {
            key: key.to_string(),
            save_current: None,
            discard_current: false,
        }
    }

    fn plain_next() -> Command {
        Command::Next {
            save_current: None,
            discard_current: false,
        }
    }

    fn all_commands() -> Vec<Command> {
        vec![
            Command::List { refresh: false },
            Command::List { refresh: true },
            plain_use("PR"),
            Command::Use {
                key: "PR".to_string(),
                save_current: Some(None),
                discard_current: false,
            },
            Command::Use {
                key: "PR".to_string(),
                save_current: None,
                discard_current: true,
            },
            plain_next(),
            Command::Add { label: None },
            Command::Rm {
                key: "PR".to_string(),
                yes: true,
            },
            Command::Edit {
                key: "PR".to_string(),
                label: None,
                abbr: None,
                color: None,
            },
            Command::Burn {
                action: BurnAction::Plan { json: false },
            },
            Command::Burn {
                action: BurnAction::Simulate {
                    from: "2026-11-09".to_string(),
                    to: "2026-11-16".to_string(),
                    without: vec!["LB".to_string()],
                },
            },
            Command::AgentSkill {
                action: SkillAction::Install { force: false },
            },
            Command::AgentSkill {
                action: SkillAction::Uninstall { force: false },
            },
            Command::Panel {
                view: PanelView::Main,
            },
            Command::Status {
                json: false,
                waybar: false,
                i3blocks: false,
                refresh: false,
            },
            Command::Prices { refresh: false },
            Command::Prices { refresh: true },
            Command::History {
                action: HistoryAction::Export {
                    format: ExportFormat::Csv,
                    from: None,
                    to: None,
                },
            },
            Command::History {
                action: HistoryAction::Import {
                    folder: "/home/sam/seatswap-history".to_string(),
                },
            },
            Command::Statusline,
        ]
    }

    fn env(pairs: &[(&str, &str)]) -> Caller {
        caller(&|name| {
            pairs
                .iter()
                .find(|(key, _)| *key == name)
                .map(|(_, value)| value.to_string())
        })
    }

    /// R8 from the command line: `seatswap codex switch <abbr>` finds the login
    /// by its abbreviation, switches Codex's fake `auth.json` to it and writes
    /// the `providerSwitch` event; an unknown abbreviation, or a Claude
    /// account's, is an error naming it and changes nothing. `codex list`
    /// prints one line per saved login.
    #[test]
    fn codex_switch_by_abbreviation() {
        let fixture = Fixture::new();
        fixture.add_account(0);
        {
            // A Claude account's abbreviation is known, but it is no Codex login.
            let store = fixture.store();
            let mut file = store.load().expect("accounts");
            file.accounts[0].abbr = "LB".to_string();
            store.save(&file).expect("accounts saved");
        }
        let codex_home = fixture.codex_logins();

        fixture
            .run(
                Command::Codex {
                    action: CodexAction::Switch {
                        abbr: "oa2".to_string(),
                    },
                },
                Caller::Person,
            )
            .expect("the switch runs");

        assert_eq!(
            fs::read(codex_home.join("auth.json")).expect("auth.json"),
            CODEX_OTHER_AUTH.as_bytes(),
            "the switch writes the saved login byte for byte"
        );

        let app = AppPaths::at(&fixture.data_dir);
        let host = "atlas".to_string();
        let history = HistoryDir::at(&app.history_dir(), &host);
        let now = Utc::now();
        let read = history
            .read::<seatswap_core::history::Event>(
                &Range {
                    from: now - Duration::hours(1),
                    to: now + Duration::hours(1),
                },
                &seatswap_core::history::Hosts::One(host),
            )
            .expect("the events are read");
        let switches: Vec<&EventKind> = read
            .records
            .iter()
            .map(|record| &record.record.kind)
            .filter(|kind| matches!(kind, EventKind::ProviderSwitch { .. }))
            .collect();
        assert_eq!(switches.len(), 1, "one providerSwitch event");
        assert_eq!(
            switches[0],
            &EventKind::ProviderSwitch {
                from: Some(AccountRef {
                    acc: CODEX_LIVE_ID.to_string(),
                    org: CODEX_ORG.to_string(),
                }),
                to: AccountRef {
                    acc: CODEX_OTHER_ID.to_string(),
                    org: CODEX_ORG.to_string(),
                },
                by: By::User,
            }
        );

        let before = fs::read(codex_home.join("auth.json")).expect("auth.json");
        // An unknown abbreviation is an error naming it.
        let error = fixture
            .run(
                Command::Codex {
                    action: CodexAction::Switch {
                        abbr: "ZZ".to_string(),
                    },
                },
                Caller::Person,
            )
            .expect_err("an unknown abbreviation is refused");
        assert!(error.to_string().contains("ZZ"), "{error}");
        // A known abbreviation that is not a Codex login is refused too, and
        // changes nothing.
        let error = fixture
            .run(
                Command::Codex {
                    action: CodexAction::Switch {
                        abbr: "LB".to_string(),
                    },
                },
                Caller::Person,
            )
            .expect_err("a Claude account is not a Codex login");
        assert!(
            error.to_string().contains("LB") && error.to_string().contains("Claude account"),
            "{error}"
        );
        assert_eq!(
            fs::read(codex_home.join("auth.json")).expect("auth.json"),
            before,
            "a refused switch writes nothing"
        );

        // `codex list` runs on the same fake folders (no usage is asked) and
        // prints the live login first, then the other by abbreviation.
        let store = fixture.store();
        let settings = store.load().expect("accounts").settings;
        let context = Context {
            store,
            claude: fixture.claude(),
            settings,
        };
        let lines = codex_lines(&context, &CodexPaths::at(&codex_home)).expect("the list lines");
        assert_eq!(
            lines,
            vec!["* OA2 Pro   live".to_string(), "  OA  Plus".to_string()],
            "{lines:?}"
        );
        fixture
            .run(
                Command::Codex {
                    action: CodexAction::List { json: false },
                },
                Caller::Person,
            )
            .expect("the list runs");
    }

    /// B8: `codex list` only reads. A live login Codex has open is not saved by
    /// listing it, and neither its missing subscription nor any other record is
    /// written.
    #[test]
    fn codex_list_writes_nothing() {
        let fixture = Fixture::new();
        let codex_home = fixture.codex_logins();
        // The live login has no subscription and no stored copy: a poll would
        // create both, a list must not.
        let store = fixture.store();
        let mut file = store.load().expect("accounts.json");
        file.provider_subscriptions
            .retain(|subscription| subscription.account.acc != CODEX_LIVE_ID);
        store.save(&file).expect("accounts.json");
        let accounts_before =
            fs::read(fixture.data_dir.join("accounts.json")).expect("accounts.json");
        let secrets_before = read_folder(&fixture.data_dir.join("secrets"));
        let settings = store.load().expect("accounts").settings;
        let context = Context {
            store,
            claude: fixture.claude(),
            settings,
        };
        let rows = codex_rows(&context, &CodexPaths::at(&codex_home)).expect("the rows");

        assert_eq!(rows.len(), 1, "only the saved login is listed: {rows:?}");
        assert_eq!(rows[0].abbr, "OA2");
        assert_eq!(
            fs::read(fixture.data_dir.join("accounts.json")).expect("accounts.json"),
            accounts_before,
            "codex list changed accounts.json"
        );
        assert_eq!(
            read_folder(&fixture.data_dir.join("secrets")),
            secrets_before,
            "codex list saved or changed a login"
        );
        assert!(
            fixture
                .store()
                .secrets()
                .get_codex(&providers::codex::secret_id(&AccountRef {
                    acc: CODEX_LIVE_ID.to_string(),
                    org: CODEX_ORG.to_string(),
                }))
                .expect("the secret store")
                .is_none(),
            "codex list saved the live login"
        );
    }

    /// Every file name and its bytes under one folder, for "nothing changed".
    fn read_folder(folder: &std::path::Path) -> Vec<(std::ffi::OsString, Vec<u8>)> {
        let mut files: Vec<(std::ffi::OsString, Vec<u8>)> = fs::read_dir(folder)
            .expect("the folder")
            .map(|entry| {
                let entry = entry.expect("a folder entry");
                (
                    entry.file_name(),
                    fs::read(entry.path()).expect("a file in the folder"),
                )
            })
            .collect();
        files.sort_by(|first, second| first.0.cmp(&second.0));
        files
    }

    #[test]
    fn a_person_is_never_gated() {
        let settings = settings(false, false);
        for command in all_commands() {
            assert_eq!(
                gate(&command, Caller::Person, &settings),
                Gate::Allowed,
                "{command:?}"
            );
        }
    }

    #[test]
    fn an_agent_is_detected_by_either_marker() {
        assert_eq!(env(&[("CLAUDECODE", "1")]), Caller::Agent);
        assert_eq!(
            env(&[("AI_AGENT", "claude-code_2-1-322_agent")]),
            Caller::Agent
        );
        assert_eq!(env(&[("CLAUDECODE", "0")]), Caller::Person);
        assert_eq!(env(&[("AI_AGENT", "")]), Caller::Person);
        assert_eq!(env(&[]), Caller::Person);
    }

    #[test]
    fn a_cwd_is_recorded_only_when_it_is_utf8() {
        assert_eq!(
            record_cwd(Ok(PathBuf::from("/home/sam/projects/atlas"))),
            Ok("/home/sam/projects/atlas".to_string())
        );
        let reason = record_cwd(Err(io::Error::new(
            io::ErrorKind::PermissionDenied,
            "denied",
        )))
        .expect_err("a folder that cannot be read is not recorded");
        assert!(reason.contains("cannot be read"), "{reason}");

        #[cfg(unix)]
        {
            use std::ffi::OsStr;
            use std::os::unix::ffi::OsStrExt;

            let path = PathBuf::from(OsStr::from_bytes(b"/home/sam/\xff"));
            let reason =
                record_cwd(Ok(path)).expect_err("a name that is not UTF-8 is not recorded");
            assert!(reason.contains("not UTF-8"), "{reason}");
        }
    }

    #[test]
    fn the_gate_table() {
        // list and status: agentsRead decides, whatever agentsSwitch says.
        for read in [false, true] {
            for switch in [false, true] {
                let settings = settings(read, switch);
                let expected = if read {
                    Gate::Allowed
                } else {
                    Gate::Refused(AGENTS_MAY_NOT_READ)
                };
                for command in [
                    Command::List { refresh: false },
                    Command::List { refresh: true },
                    Command::Status {
                        json: true,
                        waybar: false,
                        i3blocks: false,
                        refresh: false,
                    },
                    Command::Status {
                        json: false,
                        waybar: true,
                        i3blocks: false,
                        refresh: true,
                    },
                    Command::Burn {
                        action: BurnAction::Plan { json: true },
                    },
                    Command::Burn {
                        action: BurnAction::Simulate {
                            from: "2026-11-09".to_string(),
                            to: "2026-11-16".to_string(),
                            without: Vec::new(),
                        },
                    },
                ] {
                    assert_eq!(
                        gate(&command, Caller::Agent, &settings),
                        expected,
                        "agentsRead {read}, agentsSwitch {switch}: {command:?}"
                    );
                }
            }
        }

        // use and next without the unsaved-current flags: agentsSwitch decides,
        // whatever agentsRead says.
        for read in [false, true] {
            for switch in [false, true] {
                let settings = settings(read, switch);
                let expected = if switch {
                    Gate::Allowed
                } else {
                    Gate::Refused(AGENTS_MAY_NOT_SWITCH)
                };
                for command in [plain_use("PR"), plain_next()] {
                    assert_eq!(
                        gate(&command, Caller::Agent, &settings),
                        expected,
                        "agentsRead {read}, agentsSwitch {switch}: {command:?}"
                    );
                }
            }
        }

        // Everything that changes accounts or settings is always refused.
        for (read, switch) in [(false, false), (false, true), (true, false), (true, true)] {
            let settings = settings(read, switch);
            for command in [
                Command::Use {
                    key: "PR".to_string(),
                    save_current: Some(None),
                    discard_current: false,
                },
                Command::Use {
                    key: "PR".to_string(),
                    save_current: None,
                    discard_current: true,
                },
                Command::Next {
                    save_current: Some(Some("x".to_string())),
                    discard_current: false,
                },
                Command::Next {
                    save_current: None,
                    discard_current: true,
                },
                Command::Add { label: None },
                Command::Rm {
                    key: "PR".to_string(),
                    yes: true,
                },
                Command::Edit {
                    key: "PR".to_string(),
                    label: Some("Acme".to_string()),
                    abbr: None,
                    color: Some("#3b82f6".to_string()),
                },
                Command::AgentSkill {
                    action: SkillAction::Install { force: true },
                },
                Command::AgentSkill {
                    action: SkillAction::Uninstall { force: false },
                },
                Command::Panel {
                    view: PanelView::Settings,
                },
            ] {
                assert_eq!(
                    gate(&command, Caller::Agent, &settings),
                    Gate::Refused(AGENTS_MAY_NOT_CHANGE)
                );
            }
        }
    }

    #[test]
    fn prices_follow_agents_read_and_a_refresh_is_refused() {
        for read in [false, true] {
            let settings = settings(read, false);
            let expected = if read {
                Gate::Allowed
            } else {
                Gate::Refused(AGENTS_MAY_NOT_READ)
            };
            assert_eq!(
                gate(
                    &Command::Prices { refresh: false },
                    Caller::Agent,
                    &settings
                ),
                expected,
                "agentsRead {read}"
            );
        }
        for read in [false, true] {
            for switch in [false, true] {
                let settings = settings(read, switch);
                assert_eq!(
                    gate(&Command::Prices { refresh: true }, Caller::Agent, &settings),
                    Gate::Refused(AGENTS_MAY_NOT_CHANGE),
                    "agentsRead {read}, agentsSwitch {switch}"
                );
            }
        }
        assert_eq!(
            gate(
                &Command::Prices { refresh: true },
                Caller::Person,
                &settings(false, false)
            ),
            Gate::Allowed
        );
    }

    #[test]
    fn a_refresh_with_look_ups_off_is_refused() {
        let fixture = Fixture::new();
        let store = fixture.store();
        let mut file = store.load().expect("accounts");
        file.settings.look_up_prices = false;
        store.save(&file).expect("settings saved");
        let settings = store.load().expect("accounts").settings;
        let context = Context {
            store,
            claude: fixture.claude(),
            settings,
        };

        let error = prices(context, true).expect_err("a refresh with look-ups off");
        assert_eq!(
            error.to_string(),
            "look-ups are off (lookUpPrices); turn them on in the settings to refresh"
        );
        assert_eq!(error.exit_code(), 1);
    }

    #[test]
    fn panel_settings_opens_the_window() {
        assert_eq!(panel_settings_line(), "window settings");
    }

    #[test]
    fn an_agent_may_not_open_the_window() {
        for allowed in [false, true] {
            let settings = settings(allowed, allowed);
            assert_eq!(
                gate(
                    &Command::Window {
                        target: Some("settings".to_string()),
                    },
                    Caller::Agent,
                    &settings
                ),
                Gate::Refused(AGENTS_MAY_NOT_CHANGE)
            );
        }
    }

    /// Proof (review C1-1): the target of `seatswap window` travels to the
    /// running app as one line (`window <target>`). A target with a space or a
    /// newline cannot travel over that line: the app reads the first word or
    /// the first line, so the window the person asked for never opens and the
    /// app answers "error unknown command". The command has to refuse such a
    /// target itself, with a clear message.
    #[test]
    fn proof_a_target_that_cannot_travel_over_the_socket_line_is_refused() {
        let fixture = Fixture::new();
        for target in ["settings/a b", "settings/a\nb"] {
            let error = fixture
                .run(
                    Command::Window {
                        target: Some(target.to_string()),
                    },
                    Caller::Person,
                )
                .expect_err("a target that cannot be sent must be refused");
            assert!(
                matches!(&error, CliError::Usage(_)),
                "the target {target:?} must be refused with a usage error, got: {error}"
            );
        }
    }

    #[test]
    fn the_statusline_is_never_gated() {
        // Every setting pair allows it to an agent: Claude Code calls it with
        // `CLAUDECODE=1` set, and it must print.
        for read in [false, true] {
            for switch in [false, true] {
                assert_eq!(
                    gate(&Command::Statusline, Caller::Agent, &settings(read, switch)),
                    Gate::Allowed,
                    "agentsRead {read}, agentsSwitch {switch}"
                );
            }
        }
        assert_eq!(
            gate(
                &Command::Statusline,
                Caller::Person,
                &settings(false, false)
            ),
            Gate::Allowed
        );

        // A run as an agent leaves no `agentCall` event behind.
        let fixture = Fixture::new();
        let mut cli = fixture.cli(Command::Statusline);
        let mut stdin: &[u8] = b"";
        execute(&mut cli, Caller::Agent, &mut stdin).expect("the status line is allowed");
        let events = fixture.events();
        assert!(
            !events.iter().any(|event| event["type"] == "agentCall"),
            "{events:?}"
        );
    }

    #[test]
    fn statusline_never_fails() {
        let fixture = Fixture::new();
        fs::remove_file(fixture.data_dir.join("accounts.json")).expect("the accounts file");
        let now = Utc::now();

        let line = statusline_output(&fixture.cli(Command::Statusline), b"", now);
        assert!(line.starts_with("seatswap: "), "{line}");

        // The command itself is Ok, so the process exits 0.
        let mut cli = fixture.cli(Command::Statusline);
        let mut stdin: &[u8] = b"";
        assert!(execute(&mut cli, Caller::Person, &mut stdin).is_ok());
    }

    #[test]
    fn the_statusline_reads_the_bytes_it_is_given() {
        // The input is a reader the test hands over, never the process's
        // standard input: a test run on a terminal or an open pipe would hang.
        let fixture = Fixture::new();
        fixture.add_account(0);
        let mut input: &[u8] = br#"{"model":{"id":"claude-fable-5-2","display_name":"Fable 5.2"}}"#;
        let line = statusline(&fixture.cli(Command::Statusline), &mut input);
        assert!(line.contains("Fable 5.2"), "{line}");
    }

    #[test]
    fn a_broken_keys_file_keeps_the_line_and_the_feed() {
        let fixture = Fixture::new();
        fixture.add_account(0);
        let app = AppPaths::at(&fixture.data_dir);
        let now = Utc::now();
        let payload = statusline::README_PAYLOAD.as_bytes();

        let line_without_recording =
            statusline_output(&fixture.cli(Command::Statusline), payload, now);

        let store = fixture.store();
        let mut file = store.load().expect("accounts");
        file.settings.statusline_record_until = Some(now + Duration::minutes(1));
        store.save(&file).expect("the recording setting");
        if app.feed_file().exists() {
            fs::remove_file(app.feed_file()).expect("the feed file of the first run");
        }
        let broken = b"not a recording";
        fs::write(app.statusline_keys_file(), broken).expect("the broken keys file");

        let line = statusline_output(&fixture.cli(Command::Statusline), payload, now);

        assert_eq!(line, line_without_recording, "the line stays the same");
        assert!(!line.starts_with("seatswap: keys: "), "{line}");
        assert!(app.feed_file().is_file(), "the feed is still written");
        assert_eq!(
            fs::read(app.statusline_keys_file()).expect("the keys file"),
            broken,
            "the broken file is unchanged"
        );
    }

    #[test]
    fn an_empty_layout_prints_an_empty_line() {
        let fixture = Fixture::new();
        fixture.add_account(0);
        let store = fixture.store();
        let mut file = store.load().expect("accounts");
        file.settings.statusline = StatuslineConfig {
            left: Vec::new(),
            right: Vec::new(),
            margin: 4,
        };
        store.save(&file).expect("the empty layout");

        let line = statusline_output(
            &fixture.cli(Command::Statusline),
            statusline::README_PAYLOAD.as_bytes(),
            Utc::now(),
        );

        assert_eq!(line, "");
    }

    #[test]
    fn an_unreadable_statusline_input_is_the_lines_one_line() {
        struct Broken;
        impl Read for Broken {
            fn read(&mut self, _buffer: &mut [u8]) -> io::Result<usize> {
                Err(io::Error::new(io::ErrorKind::BrokenPipe, "the pipe closed"))
            }
        }
        let fixture = Fixture::new();
        let line = statusline(&fixture.cli(Command::Statusline), &mut Broken);
        assert!(
            line.starts_with("seatswap: cannot read standard input: "),
            "{line}"
        );
    }

    #[test]
    fn a_folder_error_is_the_status_lines_one_line_and_exit_zero() {
        let error = CliError::from(PathsError::Absolute {
            path: PathBuf::from("relative/data"),
            source: io::Error::new(io::ErrorKind::NotFound, "the current folder was deleted"),
        });

        // The status line: one line on standard output and exit 0, like every
        // other status line error.
        let (line, code) = folders_error(Some(&Command::Statusline), &error);
        assert_eq!(line, Some(format!("seatswap: {error}")));
        assert_eq!(code, 0);

        // Every other command keeps the error on standard error and exit 1.
        for command in all_commands()
            .into_iter()
            .filter(|command| !matches!(command, Command::Statusline))
        {
            let (line, code) = folders_error(Some(&command), &error);
            assert_eq!(line, None, "{command:?}");
            assert_eq!(code, 1, "{command:?}");
        }
        let (line, code) = folders_error(None, &error);
        assert_eq!(line, None);
        assert_eq!(code, 1);
    }

    #[test]
    fn the_command_text_leaves_out_free_text() {
        assert_eq!(command_text(&plain_use("PR")), "use PR");
        assert_eq!(
            command_text(&Command::Use {
                key: "PR".to_string(),
                save_current: Some(None),
                discard_current: false,
            }),
            "use PR --save-current"
        );
        assert_eq!(
            command_text(&Command::Status {
                json: true,
                waybar: false,
                i3blocks: false,
                refresh: true,
            }),
            "status --json --refresh"
        );
        assert_eq!(command_text(&Command::Statusline), "statusline");
        assert_eq!(
            command_text(&Command::Edit {
                key: "PR".to_string(),
                label: Some("Private".to_string()),
                abbr: None,
                color: Some("#3b82f6".to_string()),
            }),
            "edit PR --label --color"
        );
        assert_eq!(
            command_text(&Command::AgentSkill {
                action: SkillAction::Install { force: true },
            }),
            "agent-skill install --force"
        );
        assert_eq!(
            command_text(&Command::AgentSkill {
                action: SkillAction::Uninstall { force: true },
            }),
            "agent-skill uninstall --force"
        );
        assert_eq!(
            command_text(&Command::Panel {
                view: PanelView::Settings,
            }),
            "panel settings"
        );
    }

    /// The recorded line of an agent's call keeps the folder out of it: a path
    /// is the person's, not the history's.
    #[test]
    fn the_history_command_text_leaves_out_the_path() {
        assert_eq!(
            command_text(&Command::History {
                action: HistoryAction::Import {
                    folder: "/home/sam/Sync/seatswap-history".to_string(),
                },
            }),
            "history import"
        );
        assert_eq!(
            command_text(&Command::History {
                action: HistoryAction::Export {
                    format: ExportFormat::Csv,
                    from: Some("2026-11-14".to_string()),
                    to: None,
                },
            }),
            "history export --format csv --from"
        );
    }

    #[test]
    fn a_refused_call_exits_3_and_is_recorded() {
        let fixture = Fixture::new();
        fixture.add_account(0);
        fixture.write_live(1);
        fixture.add_account(1);
        fixture.set_agents(false, false);
        let before = fixture.live_bytes();

        let error = fixture
            .run(plain_use("PR"), Caller::Agent)
            .expect_err("an agent may not switch here");
        assert_eq!(error.exit_code(), 3);
        match error {
            CliError::AgentRefused(text) => assert_eq!(text, AGENTS_MAY_NOT_SWITCH),
            other => panic!("expected AgentRefused, got {other:?}"),
        }

        let events = fixture.events();
        let call = events
            .iter()
            .find(|event| event["type"] == "agentCall")
            .expect("an agentCall event");
        assert_eq!(call["allowed"], false);
        assert_eq!(call["command"], "use PR");
        assert!(call["cwd"].is_string(), "{call}");
        assert_eq!(fixture.live_bytes(), before);
    }

    #[test]
    fn the_no_subcommand_decision_depends_on_the_caller() {
        assert_eq!(no_subcommand(Caller::Agent), NoSubcommand::Refuse);
        #[cfg(feature = "gui")]
        assert_eq!(no_subcommand(Caller::Person), NoSubcommand::StartApp);
        #[cfg(not(feature = "gui"))]
        assert_eq!(no_subcommand(Caller::Person), NoSubcommand::PrintHelp);
    }

    #[test]
    fn an_agent_without_a_subcommand_is_refused_and_recorded() {
        let fixture = Fixture::new();
        let before = fixture.live_bytes();
        let mut cli = fixture.cli_without_command();
        let mut stdin: &[u8] = b"";

        let error = execute(&mut cli, Caller::Agent, &mut stdin)
            .expect_err("an agent may not start the tray app");
        assert_eq!(error.exit_code(), 3);
        match error {
            CliError::AgentRefused(text) => assert_eq!(text, AGENTS_MAY_NOT_CHANGE),
            other => panic!("expected AgentRefused, got {other:?}"),
        }

        let events = fixture.events();
        let call = events
            .iter()
            .find(|event| event["type"] == "agentCall")
            .expect("an agentCall event");
        assert_eq!(call["allowed"], false);
        assert_eq!(call["command"], "(no subcommand)");
        assert!(call["cwd"].is_string(), "{call}");
        assert_eq!(fixture.live_bytes(), before);
    }

    fn utc_text(text: &str) -> DateTime<Utc> {
        text.parse::<DateTime<Utc>>().expect("a test time")
    }

    fn a_token(t: DateTime<Utc>, input: u64) -> TokenRecord {
        TokenRecord {
            t,
            minutes: 60,
            session: "7c1e4b2a-9d3f-4e6a-b8c5-2f0a1d7e9b34".to_string(),
            cwd: "/home/sam/projects/atlas".to_string(),
            project: ProjectKey::Repo("github.com/sam/atlas".to_string()),
            model: "claude-opus-5-5".to_string(),
            input,
            out: 96_500,
            cw: 0,
            cr: 0,
            answers: 1,
            agent_answers: 0,
        }
    }

    #[test]
    fn history_export_follows_agents_read_and_import_is_refused() {
        let export = Command::History {
            action: HistoryAction::Export {
                format: ExportFormat::Csv,
                from: None,
                to: None,
            },
        };
        for read in [false, true] {
            let expected = if read {
                Gate::Allowed
            } else {
                Gate::Refused(AGENTS_MAY_NOT_READ)
            };
            assert_eq!(
                gate(&export, Caller::Agent, &settings(read, true)),
                expected
            );
        }

        let import = Command::History {
            action: HistoryAction::Import {
                folder: "/home/sam/seatswap-history".to_string(),
            },
        };
        for read in [false, true] {
            for switch in [false, true] {
                assert_eq!(
                    gate(&import, Caller::Agent, &settings(read, switch)),
                    Gate::Refused(AGENTS_MAY_NOT_CHANGE)
                );
            }
        }
    }

    #[test]
    fn the_cli_history_folder_is_the_settings_folder() {
        let tmp = tempfile::tempdir().expect("tempdir");
        let app = AppPaths::at(tmp.path());
        for settings in [history_settings(tmp.path()), Settings::default()] {
            let (root, host) = seatswap_core::history::location(&app, &settings)
                .expect("the settings name a history folder");
            let history = history_dir(&app, &settings).expect("the cli history folder");
            assert_eq!(history.root(), root);
            assert_eq!(history.host(), host);
        }
    }

    #[test]
    fn history_export_writes_the_utc_days_to_the_writer() {
        let fixture = Fixture::new();
        let history = fixture.history();
        history
            .append(&a_token(utc_text("2026-11-14T09:00:00Z"), 1_284_000))
            .expect("the token line");
        history
            .append(&a_token(utc_text("2026-11-18T09:00:00Z"), 405_000))
            .expect("the token line");

        let range = export_day_range(Some("2026-11-09"), Some("2026-11-16")).expect("two UTC days");
        assert_eq!(range.from, utc_text("2026-11-09T00:00:00Z"));
        assert_eq!(
            range.to,
            utc_text("2026-11-17T00:00:00Z"),
            "--to is inclusive"
        );

        let mut out = Vec::new();
        write_export(&history, ExportFormat::Csv, &range, &mut out).expect("the export");
        let text = String::from_utf8(out).expect("CSV is text");
        assert!(
            text.starts_with(
                "kind,account,org,limit,model,start,end,percent,day_utc,project,input,output,cache_write,cache_read\n"
            ),
            "{text}"
        );
        assert!(
            text.contains(",2026-11-14,repo:github.com/sam/atlas,1284000,96500,0,0\n"),
            "{text}"
        );
        assert!(
            !text.contains("2026-11-18"),
            "a day outside the range stays out:\n{text}"
        );

        // Both ends are optional, and a range that ends before it starts is an error.
        let open = export_day_range(None, None).expect("the whole history");
        assert_eq!(open.from, DateTime::<Utc>::MIN_UTC);
        assert_eq!(open.to, DateTime::<Utc>::MAX_UTC);
        assert!(export_day_range(Some("2026-11-17"), Some("2026-11-16")).is_err());
        assert!(export_day_range(Some("2026-11-32"), None).is_err());
    }

    #[test]
    fn history_import_copies_a_host_folder() {
        let fixture = Fixture::new();
        let command = Command::History {
            action: HistoryAction::Import {
                folder: String::new(),
            },
        };
        let context = open_context(&fixture.cli(command)).expect("the context opens");
        let there = tempfile::tempdir().expect("tempdir");
        fs::create_dir_all(there.path().join("vega")).expect("the host folder");
        fs::write(
            there.path().join("vega/host.json"),
            "{\"machineId\":\"0b4e7c22-6a1d-4e3f-8c59-2d7a9e1b4f60\"}",
        )
        .expect("the marker");
        let line = "{\"t\":\"2026-11-14T07:00:00Z\"}\n";
        fs::write(there.path().join("vega/up-2026-11.jsonl"), line).expect("an up line");

        let imported = import_history(&context, &there.path().display().to_string())
            .expect("the folder is imported");
        assert_eq!(imported.copied, vec!["vega"]);
        assert!(imported.conflicts.is_empty());
        assert_eq!(
            fs::read_to_string(fixture.data_dir.join("history/vega/up-2026-11.jsonl"))
                .expect("the copied line"),
            line
        );
        assert!(
            import_history(&context, &there.path().display().to_string())
                .expect("a second import")
                .copied
                .is_empty()
        );

        // This history folder itself, and a folder that is not absolute.
        let own = fixture.data_dir.join("history");
        let error = import_history(&context, &own.display().to_string())
            .expect_err("the same folder is refused");
        assert!(
            error.to_string().contains("that is this history folder"),
            "{error}"
        );
        assert!(import_history(&context, "seatswap-history").is_err());
    }

    #[test]
    fn an_allowed_agent_switch_is_recorded_as_agent() {
        let fixture = Fixture::new();
        fixture.add_account(0);
        fixture.write_live(1);
        fixture.add_account(1);
        fixture.set_agents(true, true);

        fixture
            .run(plain_use("AC"), Caller::Agent)
            .expect("an allowed agent switch");

        let events = fixture.events();
        let switch = events
            .iter()
            .find(|event| event["type"] == "switch")
            .expect("a switch event");
        assert_eq!(switch["by"], "agent");
        assert!(switch["cwd"].is_string(), "{switch}");
        let call = events
            .iter()
            .find(|event| event["type"] == "agentCall")
            .expect("an agentCall event");
        assert_eq!(call["allowed"], true);
        assert_eq!(call["command"], "use AC");
    }

    #[test]
    fn skill_install_writes_the_bundled_text() {
        let fixture = Fixture::new();
        let path = crate::test_support::under(&fixture.claude().skills_dir(), "seatswap/SKILL.md");

        fixture
            .run(
                Command::AgentSkill {
                    action: SkillAction::Install { force: false },
                },
                Caller::Person,
            )
            .expect("the skill is installed");
        assert_eq!(
            fs::read_to_string(&path).expect("the skill file"),
            AGENT_SKILL
        );
        let modified = fs::metadata(&path)
            .expect("skill metadata")
            .modified()
            .expect("modification time");

        // A second install writes nothing.
        fixture
            .run(
                Command::AgentSkill {
                    action: SkillAction::Install { force: false },
                },
                Caller::Person,
            )
            .expect("a second install is a no-op");
        assert_eq!(
            fs::read_to_string(&path).expect("the skill file"),
            AGENT_SKILL
        );
        assert_eq!(
            fs::metadata(&path)
                .expect("skill metadata")
                .modified()
                .expect("modification time"),
            modified
        );

        // A different file is not replaced without --force.
        fs::write(&path, "another skill").expect("a changed skill");
        let error = fixture
            .run(
                Command::AgentSkill {
                    action: SkillAction::Install { force: false },
                },
                Caller::Person,
            )
            .expect_err("the changed file is kept");
        match error {
            CliError::Message(text) => {
                assert!(text.contains(&path.display().to_string()), "{text}");
                assert!(text.contains("--force"), "{text}");
            }
            other => panic!("expected a message, got {other:?}"),
        }
        assert_eq!(
            fs::read_to_string(&path).expect("the changed skill"),
            "another skill"
        );

        // --force replaces it.
        fixture
            .run(
                Command::AgentSkill {
                    action: SkillAction::Install { force: true },
                },
                Caller::Person,
            )
            .expect("--force replaces the changed file");
        assert_eq!(
            fs::read_to_string(&path).expect("the skill file"),
            AGENT_SKILL
        );
    }

    #[test]
    fn skill_uninstall_removes_the_file_and_the_empty_folder() {
        let fixture = Fixture::new();
        let folder = fixture.claude().skills_dir().join("seatswap");
        let path = folder.join("SKILL.md");
        fixture
            .run(
                Command::AgentSkill {
                    action: SkillAction::Install { force: false },
                },
                Caller::Person,
            )
            .expect("the skill is installed");
        assert!(path.is_file());

        fixture
            .run(
                Command::AgentSkill {
                    action: SkillAction::Uninstall { force: false },
                },
                Caller::Person,
            )
            .expect("the skill is removed");
        assert!(!path.exists());
        assert!(!folder.exists());
    }

    #[test]
    fn skill_uninstall_keeps_an_edited_file_unless_forced() {
        let fixture = Fixture::new();
        let folder = fixture.claude().skills_dir().join("seatswap");
        let path = folder.join("SKILL.md");
        fs::create_dir_all(&folder).expect("the skill folder");
        fs::write(&path, "my own skill").expect("an edited skill");
        let edited = fs::read(&path).expect("the edited bytes");

        let error = fixture
            .run(
                Command::AgentSkill {
                    action: SkillAction::Uninstall { force: false },
                },
                Caller::Person,
            )
            .expect_err("an edited skill is kept");
        match error {
            CliError::Message(text) => assert_eq!(
                text,
                format!(
                    "{} differs from seatswap's bundled skill; use --force to remove it",
                    path.display()
                )
            ),
            other => panic!("expected a message, got {other:?}"),
        }
        assert!(path.is_file());
        assert_eq!(fs::read(&path).expect("the kept skill"), edited);

        fixture
            .run(
                Command::AgentSkill {
                    action: SkillAction::Uninstall { force: true },
                },
                Caller::Person,
            )
            .expect("--force removes the edited skill");
        assert!(!path.exists());
        assert!(!folder.exists());
    }

    #[test]
    fn a_skill_file_error_names_its_path() {
        // `skills` is a file, not a folder.
        let fixture = Fixture::new();
        let skills = fixture.claude().skills_dir();
        fs::write(&skills, "not a folder").expect("a file where the folder goes");
        for action in [
            SkillAction::Install { force: false },
            SkillAction::Uninstall { force: false },
        ] {
            let error = fixture
                .run(Command::AgentSkill { action }, Caller::Person)
                .expect_err("a file where the folder goes is an error");
            let text = error.to_string();
            assert!(text.contains(&skills.display().to_string()), "{text}");
        }

        // `SKILL.md` is a folder, not a file.
        let fixture = Fixture::new();
        let path = crate::test_support::under(&fixture.claude().skills_dir(), "seatswap/SKILL.md");
        fs::create_dir_all(&path).expect("a folder where the skill goes");
        for action in [
            SkillAction::Install { force: false },
            SkillAction::Uninstall { force: false },
        ] {
            let error = fixture
                .run(Command::AgentSkill { action }, Caller::Person)
                .expect_err("a folder where the skill goes is an error");
            let text = error.to_string();
            assert!(text.contains(&path.display().to_string()), "{text}");
        }
    }

    #[test]
    fn add_writes_one_account_added_with_the_plan() {
        let fixture = Fixture::new();

        fixture
            .run(
                Command::Add {
                    label: Some("Acme".to_string()),
                },
                Caller::Person,
            )
            .expect("the live login is saved");

        let events = fixture.events();
        let added: Vec<&Value> = events
            .iter()
            .filter(|event| event["type"] == json!("accountAdded"))
            .collect();
        assert_eq!(added.len(), 1, "{events:?}");
        assert_eq!(added[0]["account"]["acc"], json!(FIRST_ACCOUNT_UUID));
        assert_eq!(added[0]["account"]["org"], json!(FIRST_ORG_UUID));
        assert_eq!(added[0]["plan"], json!("Team Premium"));

        // Saving the same login again is an update, not a second account.
        fixture
            .run(Command::Add { label: None }, Caller::Person)
            .expect("the stored copy is updated");
        let events = fixture.events();
        assert_eq!(
            events
                .iter()
                .filter(|event| event["type"] == json!("accountAdded"))
                .count(),
            1,
            "{events:?}"
        );
    }

    /// `seatswap edit` writes the same events as the tray app's edit, through
    /// the one function `ops::record_edit`: `accountLook` with only the
    /// changed fields (spec §5).
    #[test]
    fn edit_writes_account_look_with_only_the_changed_fields() {
        let fixture = Fixture::new();
        fixture.add_account(0);

        fixture
            .run(
                Command::Edit {
                    key: "AC".to_string(),
                    label: Some("Acme Lab".to_string()),
                    abbr: Some("ac".to_string()),
                    color: None,
                },
                Caller::Person,
            )
            .expect("the account is edited");

        let events = fixture.events();
        let looks: Vec<&Value> = events
            .iter()
            .filter(|event| event["type"] == json!("accountLook"))
            .collect();
        assert_eq!(looks.len(), 1, "{events:?}");
        let mut look = looks[0].clone();
        look.as_object_mut().expect("an object").remove("t");
        assert_eq!(
            look,
            json!({"type": "accountLook",
                   "account": {"acc": FIRST_ACCOUNT_UUID, "org": FIRST_ORG_UUID},
                   "label": "Acme Lab"}),
            "the same abbreviation is no change"
        );
    }

    /// A login of an account another host already named takes that host's
    /// look and price when it is added here (spec §5).
    #[test]
    fn add_of_a_known_account_takes_its_shared_look_and_price() {
        let fixture = Fixture::new();
        let vega = fixture.data_dir.join("history").join("vega");
        fs::create_dir_all(&vega).expect("vega's folder");
        fs::write(vega.join("host.json"), r#"{"machineId":"made-up"}"#).expect("marker");
        let account = json!({"acc": FIRST_ACCOUNT_UUID, "org": FIRST_ORG_UUID});
        fs::write(
            vega.join("events-2026-11.jsonl"),
            format!(
                "{}\n{}\n",
                json!({"t": "2026-11-15T10:00:00Z", "type": "accountLook", "account": account,
                       "label": "Lumen Bay", "abbr": "LB", "color": "#10b981", "autoBurn": false}),
                json!({"t": "2026-11-15T10:01:00Z", "type": "priceChanged", "account": account,
                       "amount": 180.0, "currency": "EUR"}),
            ),
        )
        .expect("vega's events");

        fixture
            .run(Command::Add { label: None }, Caller::Person)
            .expect("the live login is saved");

        let file = fixture.store().load().expect("accounts");
        let stored = &file.accounts[0];
        assert_eq!(
            (
                stored.label.as_str(),
                stored.abbr.as_str(),
                stored.color.as_str()
            ),
            ("Lumen Bay", "LB", "#10b981")
        );
        assert!(!stored.auto_burn);
        assert_eq!(stored.price, Some(180.0));
        assert_eq!(stored.price_currency.as_deref(), Some("EUR"));
        assert!(
            fixture
                .events()
                .iter()
                .all(|event| event["type"] != json!("accountLook")),
            "every field is shared already, so this host seeds none"
        );
    }

    #[test]
    fn rm_yes_writes_one_account_removed() {
        let fixture = Fixture::new();
        fixture.add_account(0);

        fixture
            .run(
                Command::Rm {
                    key: "AC".to_string(),
                    yes: true,
                },
                Caller::Person,
            )
            .expect("the account is removed");

        let events = fixture.events();
        let removed: Vec<&Value> = events
            .iter()
            .filter(|event| event["type"] == json!("accountRemoved"))
            .collect();
        assert_eq!(removed.len(), 1, "{events:?}");
        assert_eq!(removed[0]["account"]["acc"], json!(FIRST_ACCOUNT_UUID));
        assert_eq!(removed[0]["account"]["org"], json!(FIRST_ORG_UUID));
        assert!(fixture
            .store()
            .load()
            .expect("accounts")
            .accounts
            .is_empty());
    }

    /// The account that is switched away from had a stale plan and the live
    /// login says "Team Premium": the change belongs in the history, written by the
    /// code that records the events of this path.
    #[test]
    fn a_changed_plan_is_written_to_the_history_when_the_account_is_switched_away() {
        let fixture = Fixture::new();
        fixture.add_account(0);
        fixture.write_live(1);
        fixture.add_account(1);
        // The live account's stored plan is stale; its login says "Team Premium".
        let store = fixture.store();
        let mut file = store.load().expect("accounts");
        let live = file
            .accounts
            .iter_mut()
            .find(|account| account.account_uuid == SECOND_ACCOUNT_UUID)
            .expect("the live account");
        live.plan = Some("Team".to_string());
        store.save(&file).expect("the stale plan is stored");

        fixture
            .run(
                Command::Use {
                    key: "AC".to_string(),
                    save_current: None,
                    discard_current: false,
                },
                Caller::Person,
            )
            .expect("the switch");

        let file = store.load().expect("accounts");
        assert_eq!(
            file.accounts
                .iter()
                .find(|account| account.account_uuid == SECOND_ACCOUNT_UUID)
                .expect("the live account")
                .plan
                .as_deref(),
            Some("Team Premium"),
            "the plan of the account is the login's"
        );

        let events = fixture.events();
        let changed: Vec<&Value> = events
            .iter()
            .filter(|event| event["type"] == json!("planChanged"))
            .collect();
        assert_eq!(
            changed.len(),
            1,
            "one event for one plan change: {events:?}"
        );
        assert_eq!(changed[0]["account"]["acc"], json!(SECOND_ACCOUNT_UUID));
        assert_eq!(changed[0]["account"]["org"], json!(SECOND_ORG_UUID));
        assert_eq!(changed[0]["from"], json!("Team"));
        assert_eq!(changed[0]["to"], json!("Team Premium"));
    }

    #[test]
    fn a_switch_that_saves_the_unsaved_login_writes_account_added_first() {
        let fixture = Fixture::new();
        fixture.add_account(0);
        fixture.write_live(1);
        fixture.add_account(1);
        // A third, unsaved live login is what the switch must save.
        fs::write(
            fixture.config_dir.join(".credentials.json"),
            credentials_json(THIRD_ORG_UUID),
        )
        .expect("third credentials");
        fs::write(
            fixture.config_dir.join(".claude.json"),
            identity_json(
                THIRD_ACCOUNT_UUID,
                THIRD_ORG_UUID,
                "three@example.com",
                "Atlas",
            ),
        )
        .expect("third identity");

        fixture
            .run(
                Command::Use {
                    key: "AC".to_string(),
                    save_current: Some(Some("Third".to_string())),
                    discard_current: false,
                },
                Caller::Person,
            )
            .expect("the unsaved login is saved and the switch is done");

        let events = fixture.events();
        let added = events
            .iter()
            .position(|event| event["type"] == json!("accountAdded"))
            .expect("the saved account's accountAdded line");
        let switched = events
            .iter()
            .position(|event| event["type"] == json!("switch"))
            .expect("the switch line");
        assert!(added < switched, "{events:?}");
        assert_eq!(events[added]["account"]["acc"], json!(THIRD_ACCOUNT_UUID));
        assert_eq!(events[added]["plan"], json!("Team Premium"));
        assert_eq!(events[switched]["to"]["acc"], json!(FIRST_ACCOUNT_UUID));
    }

    /// Settings whose history is `<root>/history`, under the test host `atlas`.
    fn history_settings(root: &Path) -> Settings {
        Settings {
            history_dir: Some(root.join("history")),
            host_name: Some("atlas".to_string()),
            ..Settings::default()
        }
    }

    fn burn_account(id: &str, abbr: &str, acc: &str) -> Account {
        Account {
            id: id.to_string(),
            account_uuid: acc.to_string(),
            organization_uuid: FIRST_ORG_UUID.to_string(),
            label: abbr.to_string(),
            abbr: abbr.to_string(),
            color: "#3b82f6".to_string(),
            email: None,
            org_name: None,
            plan: Some("Team".to_string()),
            oauth_account: OAuthAccount(serde_json::Map::new()),
            added_at: Utc::now(),
            last_used_at: None,
            login_state: LoginState::Ok,
            login_ends_at_ms: None,
            auto_burn: true,
            price: None,
            price_currency: None,
        }
    }

    fn burn_view(
        id: &str,
        abbr: &str,
        week: f64,
        session: bool,
        reset: DateTime<Utc>,
        fetched_at: DateTime<Utc>,
    ) -> AccountView {
        let mut limits = vec![Limit {
            kind: LimitKind::WeeklyAll,
            percent: week,
            severity: Severity::Normal,
            resets_at: Some(reset),
            model: None,
            is_active: false,
        }];
        if session {
            limits.push(Limit {
                kind: LimitKind::Session,
                percent: 10.0,
                severity: Severity::Normal,
                resets_at: Some(reset),
                model: None,
                is_active: false,
            });
        }
        AccountView {
            id: id.to_string(),
            label: abbr.to_string(),
            abbr: abbr.to_string(),
            color: "#3b82f6".to_string(),
            email: None,
            plan: None,
            auto_burn: true,
            is_active: false,
            login_state: LoginState::Ok,
            login_ends_at_ms: None,
            login_days_left: None,
            usage: Some(Usage {
                limits,
                extra_usage_enabled: false,
                warnings: Vec::new(),
            }),
            usable: true,
            blocked_until: None,
            blocker: None,
            fetched_at: Some(fetched_at),
            last_error: None,
        }
    }

    /// PR, AC and LB as the planner needs them: PR at 50 % needs 25 USD; AC
    /// and LB each need 25 of the 48 USD before their common weekly reset
    /// after 48 h. The eager plan starts on them at once; the lazy plan waits
    /// until AC is down to the cushion and cannot empty both, so it blocks 60
    /// USD of work to the eager plan's 45.
    fn burn_plan_inputs() -> Inputs {
        let now = DateTime::parse_from_rfc3339("2026-11-10T12:00:00Z")
            .expect("a test time")
            .with_timezone(&Utc);
        let state = |id: &str, c: f64, week: f64, reset: &str| AccountState {
            id: id.to_string(),
            may_use: true,
            login_ok: true,
            week_percent: week,
            week_resets_at: DateTime::parse_from_rfc3339(reset)
                .expect("a test time")
                .with_timezone(&Utc),
            session: None,
            model_blocked: false,
            rates: Some(Rates::given(c, None)),
            thinking_held: 0,
        };
        Inputs {
            now,
            offset: FixedOffset::east_opt(0).expect("UTC"),
            accounts: vec![
                state("pr", 2.0, 50.0, "2026-11-18T12:00:00Z"),
                state("ac", 4.0, 0.0, "2026-11-12T12:00:00Z"),
                state("lb", 4.0, 0.0, "2026-11-12T12:00:00Z"),
            ],
            active: Some("pr".to_string()),
            rhythm: Rhythm { usd: [1.0; 168] },
            last_switch_at: None,
            manual_hold_until: None,
        }
    }

    #[test]
    fn burn_plan_json_names_the_lazy_and_eager_hours() {
        let inputs = burn_plan_inputs();
        let lazy = follow(&inputs, Policy::Lazy);
        let eager = follow(&inputs, Policy::Eager);
        assert_ne!(
            lazy.blocked_usd, eager.blocked_usd,
            "the two policies block different work"
        );
        let regime = Regime {
            early: true,
            why: RegimeWhy::Pays,
            lazy_blocked_h: 1.0,
            eager_blocked_h: 0.0,
            extra_switches: 1,
            cost_h: Some(0.1),
        };
        let json = burn_regime_json(&inputs, &regime);
        let typical = inputs.rhythm.typical_hour();
        assert_eq!(
            json.blocked_lazy_h,
            usd_hours(lazy.blocked_usd, typical),
            "blockedLazyH is the lazy plan's hours, even while early switching is on"
        );
        assert_eq!(
            json.blocked_eager_h,
            usd_hours(eager.blocked_usd, typical),
            "blockedEagerH is the eager plan's hours"
        );
        assert_ne!(json.blocked_lazy_h, json.blocked_eager_h);
    }

    /// The printed forms of `status` show nothing of auto-burn, so they must
    /// not read the four weeks of history: `--waybar` runs every few seconds.
    /// A history folder that cannot be read is the proof: the JSON form calls
    /// it an error, the waybar line still prints without a warning.
    #[test]
    fn status_waybar_does_not_read_the_history() {
        if std::env::var_os("SEATSWAP_WAYBAR_CHILD").is_some() {
            waybar_with_a_broken_history();
            return;
        }
        let exe = std::env::current_exe().expect("the test binary");
        let output = std::process::Command::new(exe)
            .args([
                "status_waybar_does_not_read_the_history",
                "--nocapture",
                "--test-threads=1",
            ])
            .env("SEATSWAP_WAYBAR_CHILD", "1")
            .output()
            .expect("the child runs");
        let stdout = String::from_utf8_lossy(&output.stdout);
        let stderr = String::from_utf8_lossy(&output.stderr);
        assert!(
            stdout.contains("SEATSWAP-WAYBAR-CHILD-RAN"),
            "the child did not run:\n{stdout}\n{stderr}"
        );
        assert!(
            output.status.success(),
            "the child failed:\n{stdout}\n{stderr}"
        );
        assert!(
            !stderr.contains("warning"),
            "the waybar line must not read the history:\n{stderr}"
        );
    }

    fn waybar_with_a_broken_history() {
        let fixture = Fixture::new();
        fixture.add_account(0);
        // A regular file where the history root belongs: reading it fails.
        fs::write(fixture.data_dir.join("history"), b"not a folder")
            .expect("a file where the history root belongs");
        let store = fixture.store();
        let settings = store.load().expect("accounts").settings;
        let context = Context {
            store,
            claude: fixture.claude(),
            settings,
        };
        let result = status(context, false, true, false, false);
        assert!(result.is_ok(), "the waybar line is printed: {result:?}");
        println!("SEATSWAP-WAYBAR-CHILD-RAN");
    }

    #[test]
    fn status_json_suggestion_is_the_planners() {
        let tmp = tempfile::tempdir().expect("tempdir");
        let now = Utc::now();
        // The reset must lie ahead of the real clock the burn status runs on.
        let reset = now + chrono::Duration::hours(3);
        let accounts = vec![
            burn_account("pr", "PR", FIRST_ACCOUNT_UUID),
            burn_account("ac", "AC", SECOND_ACCOUNT_UUID),
        ];
        let views = vec![
            burn_view("pr", "PR", 100.0, false, reset, now),
            burn_view("ac", "AC", 10.0, true, reset, now),
        ];
        let offset = FixedOffset::east_opt(0).expect("UTC");
        let burn = burn_status(
            &AppPaths::at(tmp.path()),
            &history_settings(tmp.path()),
            &views,
            &accounts,
            Some("pr"),
            now,
            offset,
        );
        assert_eq!(burn.error, None);
        let suggestion = burn.suggestion.expect("the planner suggests a switch");
        assert_eq!(suggestion.to_id, "ac");
        assert!(suggestion.text.starts_with("Switch to AC now:"));
    }

    #[test]
    fn status_json_has_usable() {
        let views = vec![account_view(true), account_view(false)];
        let output = StatusJson::new(&views, None, &BurnStatus::default());
        let value = serde_json::to_value(&output).expect("status JSON");
        let views = value["views"].as_array().expect("views");
        assert_eq!(views.len(), 2);
        for view in views {
            assert!(
                view.get("usable").and_then(Value::as_bool).is_some(),
                "{view}"
            );
        }
        assert_eq!(views[0]["usable"], true);
        assert_eq!(views[1]["usable"], false);
    }

    #[test]
    fn burn_plan_json_has_regime_and_segments() {
        let fixture = Fixture::new();
        fixture.add_account(0);
        let account = fixture.account(0);
        fixture.set_reading(&account, 10.0, false);
        let context = open_context(&fixture.cli(Command::Burn {
            action: BurnAction::Plan { json: true },
        }))
        .expect("the context opens");

        let text = burn_plan(context, true).expect("the plan is written");
        let value: Value = serde_json::from_str(&text).expect("one JSON object");
        assert_eq!(value["regime"]["early"], false);
        assert_eq!(
            value["regime"]["why"], "learning",
            "no rates and no rhythm: the plan says why"
        );
        assert!(value["regime"]["blockedLazyH"].is_null());
        assert!(value["regime"]["blockedEagerH"].is_null());
        assert_eq!(value["regime"]["extraSwitches"], 0);
        assert!(value["regime"]["costH"].is_null());
        let segments = value["segments"].as_array().expect("segments");
        assert_eq!(segments.len(), 1);
        assert_eq!(segments[0]["account"], json!(account.id));
        assert!(segments[0]["from"].is_string());
        assert!(segments[0]["to"].is_string());
        assert!(value["leftAtReset"].is_object());
    }

    #[test]
    fn burn_plan_text_names_sessions_off() {
        let fixture = Fixture::new();
        fixture.add_account(0);
        let account = fixture.account(0);
        fixture.set_reading(&account, 10.0, false);
        let store = fixture.store();
        let mut file = store.load().expect("accounts");
        file.settings.read_sessions = false;
        store.save(&file).expect("settings saved");
        let context = open_context(&fixture.cli(Command::Burn {
            action: BurnAction::Plan { json: false },
        }))
        .expect("the context opens");

        let text = burn_plan(context, false).expect("the plan is written");
        assert!(
            text.starts_with("reading sessions is off: no rates are learned"),
            "{text}"
        );
    }

    #[test]
    fn burn_plan_without_a_reading_is_an_error() {
        let fixture = Fixture::new();
        fixture.add_account(0);
        let context = open_context(&fixture.cli(Command::Burn {
            action: BurnAction::Plan { json: false },
        }))
        .expect("the context opens");

        let error = burn_plan(context, false).expect_err("a missing reading is an error");
        assert_eq!(error.exit_code(), 2, "a usage error");
        match error {
            CliError::Usage(text) => {
                assert!(text.contains("AC"), "{text}");
                assert!(text.contains("no reading"), "{text}");
            }
            other => panic!("expected a usage error, got {other:?}"),
        }
    }

    /// A fixture with one reading and two token lines: one priced, one of
    /// `claude-new-9`, which `prices.json` does not list.
    fn warning_fixture() -> Fixture {
        let fixture = Fixture::new();
        fixture.add_account(0);
        let account = fixture.account(0);
        fixture.set_reading(&account, 10.0, false);
        let now = Utc::now();
        fixture.write_work(now - Duration::minutes(20), 1.0);
        fixture.write_unpriced_work(now - Duration::minutes(10));
        fixture
    }

    fn burn_plan_warning_child() {
        let fixture = warning_fixture();
        let context = open_context(&fixture.cli(Command::Burn {
            action: BurnAction::Plan { json: false },
        }))
        .expect("the context opens");
        let text = burn_plan(context, false).expect("the plan is written");
        println!("SEATSWAP-WARN-PLAN-CHILD-RAN\n{text}");
    }

    fn status_warning_child() {
        let fixture = warning_fixture();
        let context = open_context(&fixture.cli(Command::Status {
            json: true,
            waybar: false,
            i3blocks: false,
            refresh: false,
        }))
        .expect("the context opens");
        status(context, true, false, false, false).expect("the status is written");
        println!("SEATSWAP-WARN-STATUS-CHILD-RAN");
    }

    fn simulate_warning_child() {
        let fixture = warning_fixture();
        let day = Local::now().date_naive().format("%Y-%m-%d").to_string();
        let context = open_context(&fixture.cli(Command::Burn {
            action: BurnAction::Simulate {
                from: day.clone(),
                to: day.clone(),
                without: Vec::new(),
            },
        }))
        .expect("the context opens");
        let text = burn_simulate(context, &day, &day, &[]).expect("the replay is written");
        println!("SEATSWAP-WARN-SIMULATE-CHILD-RAN\n{text}");
    }

    /// Run one test function in a child with `--nocapture` and give its stderr,
    /// where `warn` writes.
    fn warning_child(test_name: &str, variable: &str) -> String {
        let exe = std::env::current_exe().expect("the test binary");
        let output = std::process::Command::new(exe)
            .args([test_name, "--nocapture", "--test-threads=1"])
            .env(variable, "1")
            .output()
            .expect("the child runs");
        let stdout = String::from_utf8_lossy(&output.stdout);
        let stderr = String::from_utf8_lossy(&output.stderr);
        assert!(
            output.status.success(),
            "the child failed:\n{stdout}\n{stderr}"
        );
        stderr.into_owned()
    }

    /// `burn plan` prints the history read's warnings once: "no price for …"
    /// must not stay inside `learned.warnings` (review, 1.3.0).
    #[test]
    fn burn_plan_prints_the_historys_warnings_once() {
        if std::env::var_os("SEATSWAP_WARN_PLAN_CHILD").is_some() {
            burn_plan_warning_child();
            return;
        }
        let stderr = warning_child(
            "burn_plan_prints_the_historys_warnings_once",
            "SEATSWAP_WARN_PLAN_CHILD",
        );
        assert_eq!(
            stderr.matches("no price for claude-new-9").count(),
            1,
            "one warning for the model: {stderr}"
        );
    }

    /// `status --json` has no warnings field, so the history read's warnings go
    /// to stderr once (review, 1.3.0).
    #[test]
    fn status_json_prints_the_historys_warnings_once() {
        if std::env::var_os("SEATSWAP_WARN_STATUS_CHILD").is_some() {
            status_warning_child();
            return;
        }
        let stderr = warning_child(
            "status_json_prints_the_historys_warnings_once",
            "SEATSWAP_WARN_STATUS_CHILD",
        );
        assert_eq!(
            stderr.matches("no price for claude-new-9").count(),
            1,
            "one warning for the model: {stderr}"
        );
    }

    /// `burn simulate` prints the history read's warnings once, before the
    /// replays' own warnings (review, 1.3.0).
    #[test]
    fn burn_simulate_prints_the_historys_warnings_once() {
        if std::env::var_os("SEATSWAP_WARN_SIMULATE_CHILD").is_some() {
            simulate_warning_child();
            return;
        }
        let stderr = warning_child(
            "burn_simulate_prints_the_historys_warnings_once",
            "SEATSWAP_WARN_SIMULATE_CHILD",
        );
        assert_eq!(
            stderr.matches("no price for claude-new-9").count(),
            1,
            "one warning for the model: {stderr}"
        );
    }

    #[test]
    fn status_json_has_auto_burn_and_next() {
        let tmp = tempfile::tempdir().expect("tempdir");
        let history = HistoryDir::at(&tmp.path().join("history"), "atlas");
        let mut settings = history_settings(tmp.path());
        settings.auto_burn = true;
        let now = Utc::now();
        let pause = now + Duration::hours(2);
        history
            .append(&seatswap_core::history::Event {
                t: now - Duration::minutes(1),
                kind: EventKind::AutoBurn {
                    on: true,
                    paused_until: Some(pause),
                },
            })
            .expect("the pause is written");
        let reset = now + Duration::days(3);
        let accounts = vec![
            burn_account("pr", "PR", FIRST_ACCOUNT_UUID),
            burn_account("ac", "AC", SECOND_ACCOUNT_UUID),
        ];
        let views = vec![
            burn_view("pr", "PR", 100.0, false, reset, now),
            burn_view("ac", "AC", 10.0, true, reset, now),
        ];
        let burn = burn_status(
            &AppPaths::at(tmp.path()),
            &settings,
            &views,
            &accounts,
            Some("pr"),
            now,
            FixedOffset::east_opt(0).expect("UTC"),
        );
        let output = StatusJson::new(&views, Some("pr".to_string()), &burn);
        let value = serde_json::to_value(&output).expect("status JSON");
        assert_eq!(value["autoBurn"]["on"], true);
        assert_eq!(value["autoBurn"]["pausedUntil"], json!(pause));
        assert_eq!(value["next"], "ac");
        assert!(value["burnError"].is_null());
        for view in value["views"].as_array().expect("views") {
            assert!(
                view.get("slackHours").is_none(),
                "a missing slack is absent, not null: {view}"
            );
        }
    }

    #[test]
    fn next_by_an_agent_does_nothing_while_auto_burn_is_on() {
        let fixture = Fixture::new();
        fixture.add_account(0);
        fixture.write_live(1);
        fixture.add_account(1);
        fixture.write_live(0);
        fixture.set_agents(true, true);
        fixture.set_auto_burn(true);
        let before = fixture.live_bytes();

        let context = open_context(&fixture.cli(plain_next())).expect("the context opens");
        let line = next_account(context, None, false, By::Agent, None)
            .expect("auto-burn answers instead of switching");
        assert_eq!(line, "seatswap: auto-burn is on and decides");
        assert_eq!(fixture.live_bytes(), before);

        let mut cli = fixture.cli(plain_next());
        let mut stdin: &[u8] = b"";
        execute(&mut cli, Caller::Agent, &mut stdin).expect("exit 0");
        assert_eq!(
            fixture.live_bytes(),
            before,
            "the agent's next touched no login"
        );
    }

    #[test]
    fn simulate_prints_both_policies() {
        let fixture = Fixture::new();
        fixture.add_account(0);
        let account = fixture.account(0);
        let day = DateTime::parse_from_rfc3339("2026-11-10T12:00:00Z")
            .expect("a test time")
            .with_timezone(&Utc);
        fixture.write_sample(&account, day, 0.0);
        fixture.write_work(day, 1.0);
        fixture.write_work(day + Duration::hours(1), 2.0);
        let context = open_context(&fixture.cli(Command::Burn {
            action: BurnAction::Simulate {
                from: "2026-11-09".to_string(),
                to: "2026-11-16".to_string(),
                without: Vec::new(),
            },
        }))
        .expect("the context opens");

        let text =
            burn_simulate(context, "2026-11-09", "2026-11-16", &[]).expect("the replay runs");
        assert!(text.contains("Lazy: worked"), "{text}");
        assert!(text.contains("Eager: worked"), "{text}");
        assert!(text.contains("switches"), "{text}");
        assert!(text.contains("AC: 100 % expired"), "{text}");
    }

    #[test]
    fn simulate_rejects_an_unknown_without() {
        let fixture = Fixture::new();
        fixture.add_account(0);
        let context = open_context(&fixture.cli(Command::Burn {
            action: BurnAction::Simulate {
                from: "2026-11-09".to_string(),
                to: "2026-11-16".to_string(),
                without: Vec::new(),
            },
        }))
        .expect("the context opens");

        let error = burn_simulate(context, "2026-11-09", "2026-11-16", &["ZZ".to_string()])
            .expect_err("an unknown abbreviation is an error");
        assert_eq!(error.exit_code(), 2, "a usage error");
        match error {
            CliError::Usage(text) => assert!(text.contains("ZZ"), "{text}"),
            other => panic!("expected a usage error, got {other:?}"),
        }
    }

    /// A range of one local day is a range: `--to` is the *last* local day of
    /// the replay and it is inclusive, so `--from X --to X` replays the day X.
    #[test]
    fn a_one_day_replay_is_a_range() {
        let fixture = Fixture::new();
        fixture.add_account(0);
        let account = fixture.account(0);
        let day = DateTime::parse_from_rfc3339("2026-11-10T12:00:00Z")
            .expect("a test time")
            .with_timezone(&Utc);
        fixture.write_sample(&account, day, 0.0);
        fixture.write_work(day, 1.0);
        let context = open_context(&fixture.cli(Command::Burn {
            action: BurnAction::Simulate {
                from: "2026-11-10".to_string(),
                to: "2026-11-10".to_string(),
                without: Vec::new(),
            },
        }))
        .expect("the context opens");

        let text = burn_simulate(context, "2026-11-10", "2026-11-10", &[])
            .expect("--from and --to on the same day replay that one day");
        assert!(text.contains("Lazy: worked"), "{text}");
        assert!(text.contains("Eager: worked"), "{text}");
    }

    /// The last hour of a local day belongs to that day, also when the clock
    /// changes inside it: the replay ends at the local midnight of the next
    /// day, not 24 hours after the local midnight of the last day. The work
    /// runs in a child process in Europe/Berlin, where the local day
    /// 2026-10-25 has 25 hours. Only Unix reads the child's `TZ`.
    #[cfg(unix)]
    #[test]
    fn a_day_with_a_clock_change_keeps_its_last_hour() {
        if std::env::var_os("SEATSWAP_TZ").is_some() {
            replay_in_berlin();
            return;
        }
        let exe = std::env::current_exe().expect("the test binary");
        let output = std::process::Command::new(exe)
            .args([
                "a_day_with_a_clock_change_keeps_its_last_hour",
                "--nocapture",
                "--test-threads=1",
            ])
            .env("SEATSWAP_TZ", "Europe/Berlin")
            .env("TZ", "Europe/Berlin")
            .output()
            .expect("the child runs");
        let stdout = String::from_utf8_lossy(&output.stdout);
        let stderr = String::from_utf8_lossy(&output.stderr);
        assert!(
            stdout.contains("SEATSWAP-TZ-CHILD-RAN"),
            "the child did not run:\n{stdout}\n{stderr}"
        );
        assert!(
            output.status.success(),
            "the child failed:\n{stdout}\n{stderr}"
        );
    }

    /// The replay of the local days 2026-10-24 and 2026-10-25 in Berlin: the
    /// hour of work in the last hour of the 25th (23:00-24:00 local time)
    /// belongs to the 25th.
    #[cfg(unix)]
    fn replay_in_berlin() {
        let fixture = Fixture::new();
        fixture.add_account(0);
        let account = fixture.account(0);
        let at = |text: &str| {
            DateTime::parse_from_rfc3339(text)
                .expect("a test time")
                .with_timezone(&Utc)
        };
        // A reading inside the range, so the account can be used.
        fixture.write_sample(&account, at("2026-10-24T12:00:00Z"), 0.0);
        // One hour of work in the last hour of the local day 2026-10-25
        // (23:00-24:00 in Berlin, 22:00-23:00 UTC).
        fixture.write_work(at("2026-10-25T22:30:00Z"), 1.0);
        let context = open_context(&fixture.cli(Command::Burn {
            action: BurnAction::Simulate {
                from: "2026-10-24".to_string(),
                to: "2026-10-25".to_string(),
                without: Vec::new(),
            },
        }))
        .expect("the context opens");

        let text =
            burn_simulate(context, "2026-10-24", "2026-10-25", &[]).expect("the replay runs");
        println!("SEATSWAP-TZ-CHILD-RAN");
        assert!(
            text.contains("Lazy: worked 1h"),
            "the last hour of the local day belongs to the day: {text}"
        );
    }

    #[test]
    fn burn_commands_follow_agents_read() {
        let plan = Command::Burn {
            action: BurnAction::Plan { json: true },
        };
        let simulate = Command::Burn {
            action: BurnAction::Simulate {
                from: "2026-11-09".to_string(),
                to: "2026-11-16".to_string(),
                without: Vec::new(),
            },
        };
        for read in [false, true] {
            let settings = settings(read, true);
            let expected = if read {
                Gate::Allowed
            } else {
                Gate::Refused(AGENTS_MAY_NOT_READ)
            };
            for command in [&plan, &simulate] {
                assert_eq!(
                    gate(command, Caller::Agent, &settings),
                    expected,
                    "agentsRead {read}: {command:?}"
                );
            }
        }
    }

    fn account_view(usable: bool) -> AccountView {
        AccountView {
            id: "id".to_string(),
            label: "Acme".to_string(),
            abbr: "AC".to_string(),
            color: "#3b82f6".to_string(),
            email: Some("one@example.com".to_string()),
            plan: None,
            auto_burn: true,
            is_active: false,
            login_state: LoginState::Ok,
            login_ends_at_ms: None,
            login_days_left: None,
            usage: None,
            usable,
            blocked_until: None,
            blocker: None,
            fetched_at: None,
            last_error: None,
        }
    }
}
