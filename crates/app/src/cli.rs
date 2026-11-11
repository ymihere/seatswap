//! The `seatswap` command line (spec §4, §9.1; docs/implementation.md
//! "app: CLI").
//!
//! This module only parses arguments, calls the core library and prints. Every
//! decision about an account, a usage percentage or which login is live is made
//! in `seatswap_core`.

use std::io::{self, IsTerminal, Write};
use std::path::{Path, PathBuf};

use chrono::{DateTime, Local, Utc};
use clap::{CommandFactory, Parser, Subcommand, ValueEnum};
use serde::Serialize;

use seatswap_core::api::{Api, Usage};
use seatswap_core::engine::{suggestion, AccountView, Engine, PollReason, Suggestion};
use seatswap_core::paths::{AppPaths, ClaudePaths, PathsError};
use seatswap_core::store::{self, Account, AccountsFile, LoginState, Settings, Store, StoreError};
use seatswap_core::switcher::{self, LiveState, SwitchError, UnsavedPolicy};

use crate::format::{class_name, format_age, format_reset_at, integer_percent, percent, tooltip};
use crate::gui::ipc;
use crate::ops;

/// Colour used when there is no live account or no usage to judge (the grey of
/// the unknown tray ring, docs/implementation.md icon.rs).
const NO_LOAD_COLOR: &str = "#555b66";

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

#[derive(Debug, Subcommand)]
enum Command {
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

    /// Ask the running tray app to open its pop-up.
    Panel {
        /// Which view the pop-up opens on.
        #[arg(value_enum, default_value_t = PanelView::Main)]
        view: PanelView,
    },

    /// Print the live account and its binding limit.
    Status {
        /// Print the account views, the active id and the suggestion as JSON.
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
}

/// The view `seatswap panel` asks the running tray app to open.
#[derive(Debug, Clone, Copy, PartialEq, Eq, ValueEnum)]
enum PanelView {
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
            CliError::UnsavedCurrent(_) => 2,
            _ => 1,
        }
    }
}

/// Parse the command line, run the command and turn the result into a process
/// exit code. Errors are printed as `seatswap: <message>` on stderr.
pub fn run() -> std::process::ExitCode {
    let mut cli = Cli::parse();
    #[cfg(feature = "gui")]
    if cli.command.is_none() {
        return match run_gui(&cli) {
            Ok(code) => code,
            Err(error) => {
                eprintln!("seatswap: {error}");
                std::process::ExitCode::from(error.exit_code())
            }
        };
    }
    let code = match execute(&mut cli) {
        Ok(()) => 0,
        Err(error) => {
            eprintln!("seatswap: {error}");
            error.exit_code()
        }
    };
    std::process::ExitCode::from(code)
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

fn execute(cli: &mut Cli) -> Result<(), CliError> {
    let Some(command) = cli.command.take() else {
        // No subcommand without the `gui` feature: print help. With `gui`,
        // run() started the tray app above.
        Cli::command().print_help().map_err(CliError::Io)?;
        println!();
        return Ok(());
    };
    // `panel` only needs the data folder: it talks to the running tray app.
    let panel_view = match &command {
        Command::Panel { view } => Some(*view),
        _ => None,
    };
    if let Some(view) = panel_view {
        return panel(cli, view);
    }
    let context = open_context(cli)?;
    match command {
        Command::List { refresh } => list(context, refresh),
        Command::Use {
            key,
            save_current,
            discard_current,
        } => use_account(context, &key, save_current, discard_current),
        Command::Next {
            save_current,
            discard_current,
        } => next_account(context, save_current, discard_current),
        Command::Add { label } => add(context, label),
        Command::Rm { key, yes } => remove(context, &key, yes),
        Command::Edit {
            key,
            label,
            abbr,
            color,
        } => edit(context, &key, label, abbr, color),
        // Handled before the store was opened; kept for exhaustiveness.
        Command::Panel { .. } => Ok(()),
        Command::Status {
            json,
            waybar,
            i3blocks,
            refresh,
        } => status(context, json, waybar, i3blocks, refresh),
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
    Ok(Paths { app, claude })
}

fn open_context(cli: &Cli) -> Result<Context, CliError> {
    let app = AppPaths::resolve(cli.data_dir.as_deref())?;
    let (store, warning) = store::open_store(app)?;
    if let Some(warning) = warning {
        eprintln!("seatswap: warning: {warning}");
    }
    let file = store.load()?;
    let paths = resolve_paths(
        store.app_paths().clone(),
        cli.claude_config_dir.as_deref(),
        file.settings.claude_config_dir.as_deref(),
    )?;
    let (_, claude) = paths.into_parts();
    Ok(Context {
        store,
        claude,
        settings: file.settings,
    })
}

/// `seatswap panel`: ask the running tray app to open a view.
fn panel(cli: &Cli, view: PanelView) -> Result<(), CliError> {
    let app = AppPaths::resolve(cli.data_dir.as_deref())?;
    ipc::send_open_panel(&app, view.as_str())?;
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
) -> Result<(), CliError> {
    let policy = unsaved_policy(save_current, discard_current);
    let outcome = switcher::switch_to(&context.claude, &context.store, key, policy, Utc::now())
        .map_err(switch_error)?;
    print_switch(&context.store, &outcome.to);
    Ok(())
}

/// The switch already happened, so an account list that cannot name it is a
/// sentence, not a failed command.
fn print_switch(store: &Store, account_id: &str) {
    match ops::switch_message(store, account_id) {
        Ok(message) => println!("{message}"),
        Err(error) => println!("{}", ops::switched_without_list(&error)),
    }
}

fn next_account(
    context: Context,
    save_current: Option<Option<String>>,
    discard_current: bool,
) -> Result<(), CliError> {
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
                ops::save_live_login(&context.claude, &context.store, label.clone(), now)
                    .map_err(CliError::Message)?;
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
    print_switch(&context.store, &outcome.to);
    Ok(())
}

fn add(context: Context, label: Option<String>) -> Result<(), CliError> {
    let account = ops::save_live_login(&context.claude, &context.store, label, Utc::now())
        .map_err(CliError::Message)?;
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
    let removed = switcher::remove_account(&context.store, key)?;
    println!("Removed {} — {}", removed.abbr, removed.label);
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
    let account = switcher::edit_account(
        &context.store,
        key,
        label.as_deref(),
        abbr.as_deref(),
        color.as_deref(),
    )?;
    println!("Updated {} — {}", account.abbr, account.label);
    Ok(())
}

fn status(
    context: Context,
    json: bool,
    waybar: bool,
    i3blocks: bool,
    refresh: bool,
) -> Result<(), CliError> {
    let now = Utc::now();
    let mut engine = Engine::new(context.claude, context.store, Api::new())?;
    let views = account_views(&mut engine, refresh, PollReason::Timer, now)?;

    if json {
        let active = views
            .iter()
            .find(|view| view.is_active)
            .map(|view| view.id.clone());
        let suggestion = suggestion(&views, &context.settings).map(SuggestionJson::from);
        let output = StatusJson {
            views: &views,
            active,
            suggestion,
        };
        println!("{}", serde_json::to_string(&output)?);
    } else if waybar {
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
    Ok(())
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
        login_ends_text(view.login_days_left).unwrap_or_else(|| "-".to_string()),
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

fn login_ends_text(days: Option<i64>) -> Option<String> {
    match days {
        Some(days) if days < 0 => Some("login ended".to_string()),
        Some(days) => Some(format!("login ends in {days} d")),
        None => None,
    }
}

// ---------------------------------------------------------------------------
// Machine-readable output

#[derive(Debug, Serialize)]
#[serde(rename_all = "camelCase")]
struct StatusJson<'a> {
    views: &'a [AccountView],
    active: Option<String>,
    suggestion: Option<SuggestionJson>,
}

#[derive(Debug, Serialize)]
#[serde(rename_all = "camelCase")]
struct SuggestionJson {
    from_id: String,
    to_id: String,
    from_percent: f64,
    to_percent: f64,
    from_resets_at: Option<DateTime<Utc>>,
}

impl From<Suggestion> for SuggestionJson {
    fn from(suggestion: Suggestion) -> SuggestionJson {
        SuggestionJson {
            from_id: suggestion.from_id,
            to_id: suggestion.to_id,
            from_percent: suggestion.from_percent,
            to_percent: suggestion.to_percent,
            from_resets_at: suggestion.from_resets_at,
        }
    }
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
