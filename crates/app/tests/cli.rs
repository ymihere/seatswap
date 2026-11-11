//! Integration tests for the `seatswap` command line. Every test uses a
//! fake Claude Code config folder and a fake data folder; `HOME` points into the
//! same temporary folder so the binary can never see the real home.

use std::fs;
use std::io::{BufRead, BufReader, Write};
use std::os::unix::fs::PermissionsExt;
use std::os::unix::net::UnixListener;
use std::path::{Path, PathBuf};
use std::process::{Command, Output, Stdio};
use std::thread;

use serde_json::{json, Value};

const FIRST_ACCOUNT_UUID: &str = "4b7e2f91-3c5a-4d8e-9f1b-6a2c8e4d7b30";
const FIRST_ORG_UUID: &str = "e2a6c9d1-7f4b-4a3e-8c5d-1b9f0a7e3c62";
const SECOND_ACCOUNT_UUID: &str = "8e1d5b7a-6c3f-4a2b-9d8e-1f2a3b4c5d6e";
const SECOND_ORG_UUID: &str = "5f2c8a1b-9d4e-4f6a-8b3c-7e2d1a9c4b5e";
const THIRD_ACCOUNT_UUID: &str = "0f5f8b7e-6f0a-4d6e-9b0a-8c1d2e3f4a5b";
const THIRD_ORG_UUID: &str = "7c3a9e1d-2b4f-4a6c-8d0e-3f5b7a9c1d2e";

const ENDS_AT_MS: i64 = 1_796_772_840_000;

/// One real-shaped Claude Code login + identity.
struct Live {
    account_uuid: &'static str,
    organization_uuid: &'static str,
    email: &'static str,
    organization_name: &'static str,
    subscription: &'static str,
    rate_limit_tier: &'static str,
    access_fill: char,
    refresh_fill: char,
}

impl Live {
    fn first() -> Live {
        Live {
            account_uuid: FIRST_ACCOUNT_UUID,
            organization_uuid: FIRST_ORG_UUID,
            email: "one@example.com",
            organization_name: "Acme",
            subscription: "team",
            rate_limit_tier: "default_claude_max_5x",
            access_fill: 'a',
            refresh_fill: 'b',
        }
    }

    fn second() -> Live {
        Live {
            account_uuid: SECOND_ACCOUNT_UUID,
            organization_uuid: SECOND_ORG_UUID,
            email: "two@example.com",
            organization_name: "Private",
            subscription: "max",
            rate_limit_tier: "default_claude_max_20x",
            access_fill: 'c',
            refresh_fill: 'd',
        }
    }

    fn third() -> Live {
        Live {
            account_uuid: THIRD_ACCOUNT_UUID,
            organization_uuid: THIRD_ORG_UUID,
            email: "three@example.com",
            organization_name: "Third",
            subscription: "pro",
            rate_limit_tier: "default_claude_pro",
            access_fill: 'e',
            refresh_fill: 'f',
        }
    }

    fn access_token(&self) -> String {
        format!("sk-ant-oat01-{}", self.access_fill.to_string().repeat(95))
    }

    fn refresh_token(&self) -> String {
        format!("sk-ant-ort01-{}", self.refresh_fill.to_string().repeat(95))
    }

    fn credentials_json(&self) -> String {
        json!({
            "mcpOAuth": {
                "plugin:engineering:linear|5f1c2a9be0d34c11": {
                    "serverName": "plugin:engineering:linear",
                    "accessToken": format!("lin_oauth_{}", "e".repeat(64)),
                    "expiresAt": 1_794_400_000_000_i64
                }
            },
            "claudeAiOauth": {
                "accessToken": self.access_token(),
                "refreshToken": self.refresh_token(),
                "expiresAt": 1_794_356_040_000_i64,
                "refreshTokenExpiresAt": ENDS_AT_MS,
                "scopes": [
                    "user:inference", "user:profile", "user:file_upload",
                    "user:mcp_servers", "user:plugins", "user:sessions:claude_code"
                ],
                "subscriptionType": self.subscription,
                "subscriptionTypeSource": "profile",
                "rateLimitTier": self.rate_limit_tier
            },
            "organizationUuid": self.organization_uuid
        })
        .to_string()
    }

    fn identity_json(&self) -> String {
        serde_json::to_string_pretty(&json!({
            "numStartups": 41,
            "oauthAccount": {
                "accountUuid": self.account_uuid,
                "emailAddress": self.email,
                "organizationUuid": self.organization_uuid,
                "organizationName": self.organization_name,
                "organizationRole": "admin",
                "billingType": "stripe_subscription",
                "hasExtraUsageEnabled": false
            },
            "projects": { "/home/u/x": { "allowedTools": [] } }
        }))
        .expect("test identity")
    }
}

struct Fixture {
    home: tempfile::TempDir,
    config_dir: PathBuf,
    data_dir: PathBuf,
}

impl Fixture {
    fn new() -> Fixture {
        let home = tempfile::tempdir().expect("tempdir");
        let config_dir = home.path().join("claude");
        let data_dir = home.path().join("data");
        fs::create_dir_all(&config_dir).expect("config dir");
        fs::create_dir_all(&data_dir).expect("data dir");
        // Record the file backend so opening the store never probes the real
        // OS keyring (AGENTS.md hard rule 1).
        write_private(
            &data_dir.join("accounts.json"),
            br#"{"version":1,"accounts":[],"secretsBackend":"file"}"#,
        );
        let fixture = Fixture {
            home,
            config_dir,
            data_dir,
        };
        fixture.write_live(&Live::first());
        fixture
    }

    fn path(&self, name: &str) -> PathBuf {
        self.config_dir.join(name)
    }

    fn write_live(&self, live: &Live) {
        write_private(
            &self.path(".credentials.json"),
            live.credentials_json().as_bytes(),
        );
        write_private(&self.path(".claude.json"), live.identity_json().as_bytes());
    }

    fn credentials(&self) -> Value {
        read_json(&self.path(".credentials.json"))
    }

    fn identity(&self) -> Value {
        read_json(&self.path(".claude.json"))
    }

    fn accounts(&self) -> Value {
        read_json(&self.data_dir.join("accounts.json"))
    }

    fn command(&self) -> Command {
        let mut command = Command::new(env!("CARGO_BIN_EXE_seatswap"));
        command
            .arg("--data-dir")
            .arg(&self.data_dir)
            .arg("--claude-config-dir")
            .arg(&self.config_dir)
            .env("HOME", self.home.path())
            .env_remove("CLAUDE_CONFIG_DIR")
            .env_remove("XDG_CONFIG_HOME")
            // With no runtime folder the socket is <data>/gui.sock, which the
            // panel test binds.
            .env_remove("XDG_RUNTIME_DIR")
            .stdin(Stdio::null());
        command
    }

    fn run(&self, args: &[&str]) -> Output {
        self.command().args(args).output().expect("run seatswap")
    }
}

fn write_private(path: &Path, bytes: &[u8]) {
    fs::write(path, bytes).expect("write fake Claude Code file");
    fs::set_permissions(path, fs::Permissions::from_mode(0o600)).expect("mode 0600");
}

fn read_json(path: &Path) -> Value {
    let text = fs::read_to_string(path).expect("read JSON file");
    serde_json::from_str(&text).expect("valid JSON")
}

fn stdout(output: &Output) -> String {
    String::from_utf8(output.stdout.clone()).expect("stdout is UTF-8")
}

fn stderr(output: &Output) -> String {
    String::from_utf8(output.stderr.clone()).expect("stderr is UTF-8")
}

fn assert_ok(output: &Output) {
    assert_eq!(
        output.status.code(),
        Some(0),
        "stdout:\n{}\nstderr:\n{}",
        stdout(output),
        stderr(output)
    );
}

fn assert_code(output: &Output, code: i32) {
    assert_eq!(
        output.status.code(),
        Some(code),
        "stdout:\n{}\nstderr:\n{}",
        stdout(output),
        stderr(output)
    );
}

#[test]
fn add_saves_the_live_login_and_list_marks_it() {
    let fixture = Fixture::new();

    let added = fixture.run(&["add"]);
    assert_ok(&added);
    assert!(stdout(&added).contains("AC — Acme"), "{}", stdout(&added));

    let listed = fixture.run(&["list"]);
    assert_ok(&listed);
    let text = stdout(&listed);
    let line = text
        .lines()
        .find(|line| line.contains("Acme"))
        .unwrap_or_else(|| panic!("no account line in {text:?}"));
    assert!(line.starts_with("* "), "live account not marked: {line:?}");
    assert!(line.contains("one@example.com"), "{line:?}");
    assert!(line.contains("Team"), "{line:?}");

    let accounts = fixture.accounts();
    assert_eq!(accounts["accounts"].as_array().expect("accounts").len(), 1);
    assert_eq!(accounts["secretsBackend"], "file");
    assert_eq!(accounts["accounts"][0]["abbr"], "AC");
}

#[test]
fn add_label_saves_a_second_live_account() {
    let fixture = Fixture::new();
    assert_ok(&fixture.run(&["add"]));

    fixture.write_live(&Live::second());
    let added = fixture.run(&["add", "--label", "Private"]);
    assert_ok(&added);
    assert!(
        stdout(&added).contains("PR — Private"),
        "{}",
        stdout(&added)
    );

    let listed = fixture.run(&["list"]);
    assert_ok(&listed);
    let text = stdout(&listed);
    assert!(text.contains("AC  Acme"), "{text}");
    let private = text
        .lines()
        .find(|line| line.contains("Private"))
        .unwrap_or_else(|| panic!("no Private line in {text:?}"));
    assert!(private.starts_with("* "), "{private:?}");
    assert!(private.contains("two@example.com"), "{private:?}");
}

#[test]
fn use_switches_the_fake_live_files() {
    let fixture = Fixture::new();
    assert_ok(&fixture.run(&["add"]));
    fixture.write_live(&Live::second());
    assert_ok(&fixture.run(&["add", "--label", "Private"]));

    let before = fs::read_to_string(fixture.path(".credentials.json")).expect("credentials");
    let switched = fixture.run(&["use", "AC"]);
    assert_ok(&switched);
    assert!(
        stdout(&switched).contains("Switched to AC — Acme"),
        "{}",
        stdout(&switched)
    );

    let after = fs::read_to_string(fixture.path(".credentials.json")).expect("credentials");
    assert_ne!(before, after, "the live credentials must change");
    assert_eq!(
        fixture.credentials()["claudeAiOauth"]["accessToken"],
        json!(Live::first().access_token())
    );
    assert_eq!(
        fixture.identity()["oauthAccount"]["accountUuid"],
        json!(FIRST_ACCOUNT_UUID)
    );
    // Everything that is not a login key stays.
    assert!(fixture.credentials().get("mcpOAuth").is_some());
}

#[test]
fn use_with_an_unsaved_live_login_exits_2() {
    let fixture = Fixture::new();
    assert_ok(&fixture.run(&["add"]));
    fixture.write_live(&Live::third());
    let before = fs::read_to_string(fixture.path(".credentials.json")).expect("credentials");

    let refused = fixture.run(&["use", "AC"]);
    assert_code(&refused, 2);
    assert!(
        stderr(&refused).contains(
            "the current login (three@example.com) is not saved; rerun with --save-current or --discard-current"
        ),
        "{}",
        stderr(&refused)
    );
    assert_eq!(
        fs::read_to_string(fixture.path(".credentials.json")).expect("credentials"),
        before,
        "a refused switch must not touch the live login"
    );
    assert_eq!(
        fixture.accounts()["accounts"]
            .as_array()
            .expect("accounts")
            .len(),
        1
    );
}

#[test]
fn use_discard_current_switches_without_saving() {
    let fixture = Fixture::new();
    assert_ok(&fixture.run(&["add"]));
    fixture.write_live(&Live::third());

    let switched = fixture.run(&["use", "AC", "--discard-current"]);
    assert_ok(&switched);
    assert_eq!(
        fixture.credentials()["claudeAiOauth"]["accessToken"],
        json!(Live::first().access_token())
    );
    assert_eq!(
        fixture.accounts()["accounts"]
            .as_array()
            .expect("accounts")
            .len(),
        1,
        "the discarded login must not be saved"
    );
}

#[test]
fn use_save_current_keeps_the_unsaved_live_login() {
    let fixture = Fixture::new();
    assert_ok(&fixture.run(&["add"]));
    fixture.write_live(&Live::third());

    let switched = fixture.run(&["use", "AC", "--save-current"]);
    assert_ok(&switched);
    assert_eq!(
        fixture.credentials()["claudeAiOauth"]["accessToken"],
        json!(Live::first().access_token())
    );

    let accounts = fixture.accounts();
    let saved = accounts["accounts"]
        .as_array()
        .expect("accounts")
        .iter()
        .find(|account| account["email"] == "three@example.com")
        .expect("the unsaved live login must be kept as an account");
    // The default label comes from the organization name.
    assert_eq!(saved["label"], "Third");
}

#[test]
fn next_wraps_around_the_account_list() {
    let fixture = Fixture::new();
    assert_ok(&fixture.run(&["add"]));
    fixture.write_live(&Live::second());
    assert_ok(&fixture.run(&["add", "--label", "Private"]));

    // Private is last in the list, so next wraps to Acme.
    let wrapped = fixture.run(&["next"]);
    assert_ok(&wrapped);
    assert!(
        stdout(&wrapped).contains("Switched to AC — Acme"),
        "{}",
        stdout(&wrapped)
    );
    assert_eq!(
        fixture.credentials()["claudeAiOauth"]["accessToken"],
        json!(Live::first().access_token())
    );

    // From Acme it goes on to Private.
    let forward = fixture.run(&["next"]);
    assert_ok(&forward);
    assert!(
        stdout(&forward).contains("Switched to PR — Private"),
        "{}",
        stdout(&forward)
    );
    assert_eq!(
        fixture.credentials()["claudeAiOauth"]["accessToken"],
        json!(Live::second().access_token())
    );
}

#[test]
fn edit_changes_the_abbreviation_and_rejects_a_duplicate() {
    let fixture = Fixture::new();
    assert_ok(&fixture.run(&["add"]));
    fixture.write_live(&Live::second());
    assert_ok(&fixture.run(&["add", "--label", "Private"]));

    let edited = fixture.run(&["edit", "AC", "--abbr", "X"]);
    assert_ok(&edited);
    assert!(
        stdout(&edited).contains("Updated X — Acme"),
        "{}",
        stdout(&edited)
    );
    let listed = fixture.run(&["list"]);
    assert!(stdout(&listed).contains("X  Acme"), "{}", stdout(&listed));

    let rejected = fixture.run(&["edit", "X", "--abbr", "PR"]);
    assert_code(&rejected, 1);
    assert!(
        stderr(&rejected).contains("already used"),
        "{}",
        stderr(&rejected)
    );
}

#[test]
fn rm_yes_removes_the_account_and_its_login() {
    let fixture = Fixture::new();
    assert_ok(&fixture.run(&["add"]));
    fixture.write_live(&Live::second());
    assert_ok(&fixture.run(&["add", "--label", "Private"]));

    let accounts = fixture.accounts();
    let private = accounts["accounts"]
        .as_array()
        .expect("accounts")
        .iter()
        .find(|account| account["abbr"] == "PR")
        .expect("Private account");
    let private_id = private["id"].as_str().expect("id").to_string();
    let secret = fixture
        .data_dir
        .join("secrets")
        .join(format!("{private_id}.json"));
    assert!(secret.is_file(), "the stored login must exist first");

    let removed = fixture.run(&["rm", "PR", "--yes"]);
    assert_ok(&removed);
    assert!(
        stdout(&removed).contains("Removed PR — Private"),
        "{}",
        stdout(&removed)
    );
    assert!(!secret.exists(), "the stored login must be deleted");
    let listed = fixture.run(&["list"]);
    assert!(!stdout(&listed).contains("Private"), "{}", stdout(&listed));
}

#[test]
fn rm_without_yes_refuses_when_stdin_is_not_a_terminal() {
    let fixture = Fixture::new();
    assert_ok(&fixture.run(&["add"]));

    let refused = fixture.run(&["rm", "AC"]);
    assert_code(&refused, 1);
    assert!(
        stderr(&refused).contains("without --yes"),
        "{}",
        stderr(&refused)
    );
    let listed = fixture.run(&["list"]);
    assert!(stdout(&listed).contains("Acme"), "{}", stdout(&listed));
}

#[test]
fn status_prints_text_json_and_waybar() {
    let fixture = Fixture::new();
    assert_ok(&fixture.run(&["add"]));

    let plain = fixture.run(&["status"]);
    assert_ok(&plain);
    assert_eq!(stdout(&plain).trim(), "AC");

    let json_out = fixture.run(&["status", "--json"]);
    assert_ok(&json_out);
    let value: Value =
        serde_json::from_str(stdout(&json_out).trim()).expect("status --json is JSON");
    let views = value["views"].as_array().expect("views array");
    assert_eq!(views.len(), 1);
    assert_eq!(views[0]["abbr"], "AC");
    assert_eq!(views[0]["isActive"], true);
    assert!(value["active"].is_string(), "{value}");
    assert!(value["suggestion"].is_null(), "{value}");

    // No cached usage and no --refresh: the waybar output must be the empty state.
    let waybar = fixture.run(&["status", "--waybar"]);
    assert_ok(&waybar);
    let line: Value =
        serde_json::from_str(stdout(&waybar).trim()).expect("status --waybar is JSON");
    assert_eq!(line["text"], "—");
    assert_eq!(line["class"], "none");
    assert!(line["percentage"].is_null(), "{line}");
}

#[test]
fn status_i3blocks_prints_three_lines() {
    let fixture = Fixture::new();
    assert_ok(&fixture.run(&["add"]));

    let output = fixture.run(&["status", "--i3blocks"]);
    assert_ok(&output);
    let text = stdout(&output);
    let lines: Vec<&str> = text.lines().collect();
    assert_eq!(lines.len(), 3, "{text:?}");
    assert_eq!(lines[1], "AC");
    assert_eq!(lines[2], "#555b66");
}

#[test]
fn status_waybar_and_list_use_the_cached_usage() {
    let fixture = Fixture::new();
    assert_ok(&fixture.run(&["add"]));
    let accounts = fixture.accounts();
    let id = accounts["accounts"][0]["id"]
        .as_str()
        .expect("account id")
        .to_string();

    let fetched_at = chrono::Utc::now();
    let resets_at = (fetched_at + chrono::Duration::minutes(100)).to_rfc3339();
    let cache = json!({
        "version": 1,
        "entries": {
            id: {
                "usage": {
                    "limits": [
                        {
                            "kind": "session",
                            "percent": 97.0,
                            "severity": "critical",
                            "resetsAt": resets_at,
                            "model": null,
                            "isActive": true
                        },
                        {
                            "kind": "weekly_all",
                            "percent": 48.0,
                            "severity": "normal",
                            "resetsAt": null,
                            "model": null,
                            "isActive": false
                        }
                    ],
                    "extraUsageEnabled": false
                },
                "fetchedAt": fetched_at.to_rfc3339(),
                "lastError": null,
                "failures": 0,
                "backoffUntil": null
            }
        }
    });
    write_private(
        &fixture.data_dir.join("usage-cache.json"),
        cache.to_string().as_bytes(),
    );

    let waybar = fixture.run(&["status", "--waybar"]);
    assert_ok(&waybar);
    let line: Value =
        serde_json::from_str(stdout(&waybar).trim()).expect("status --waybar is JSON");
    assert_eq!(line["text"], "AC 97%");
    assert_eq!(line["class"], "critical");
    assert_eq!(line["percentage"], 97);
    let tooltip = line["tooltip"].as_str().expect("tooltip");
    assert!(
        tooltip.starts_with("AC · Acme — session 97 % (resets "),
        "{tooltip}"
    );
    assert!(tooltip.ends_with(" · week 48 %"), "{tooltip}");

    let listed = fixture.run(&["list"]);
    assert_ok(&listed);
    let text = stdout(&listed);
    assert!(text.contains("session 97%"), "{text}");
    assert!(text.contains("week 48%"), "{text}");
    assert!(text.contains("login ends in "), "{text}");
}

#[test]
fn unknown_account_exits_1_with_a_message() {
    let fixture = Fixture::new();
    assert_ok(&fixture.run(&["add"]));

    let result = fixture.run(&["use", "ZZ"]);
    assert_code(&result, 1);
    assert!(
        stderr(&result).contains("unknown account ZZ"),
        "{}",
        stderr(&result)
    );
}

#[test]
fn panel_without_a_running_app_exits_1() {
    let fixture = Fixture::new();
    let result = fixture.run(&["panel"]);
    assert_code(&result, 1);
    assert!(
        stderr(&result).contains("seatswap is not running"),
        "{}",
        stderr(&result)
    );
}

#[test]
fn panel_sends_the_view_to_the_running_app() {
    let fixture = Fixture::new();
    let socket = fixture.data_dir.join("gui.sock");
    let listener = UnixListener::bind(&socket).expect("bind the test socket");
    let received = thread::spawn(move || {
        let (mut stream, _) = listener.accept().expect("accept the panel client");
        let mut line = String::new();
        BufReader::new(stream.try_clone().expect("clone the stream"))
            .read_line(&mut line)
            .expect("read the command");
        stream.write_all(b"ok\n").expect("acknowledge");
        line
    });

    let result = fixture.run(&["panel", "settings"]);
    assert_ok(&result);
    assert_eq!(
        received.join().expect("listener thread").trim(),
        "open-panel settings"
    );
}
