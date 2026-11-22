//! Claude Code's session files and which of them are alive (spec §1.6).
//!
//! A session file alone does not say whether its session still runs: a killed
//! session leaves its file behind, still `busy`. Only [`Liveness`] decides, and
//! [`SystemProbe`] is the answer for this system.

use std::fs;
use std::io;
use std::path::{Path, PathBuf};

use serde::Deserialize;

use crate::paths::ClaudePaths;
use crate::project;

/// The Claude Code version whose session files were checked (README §5).
pub const TESTED_CLAUDE_CODE: &str = "2.1.322";

/// The prefix of a Windows named pipe path.
const PIPE_PREFIX: &str = r"\\.\pipe\";

#[derive(Debug, thiserror::Error)]
pub enum SessionsError {
    #[error("{path}: {source}")]
    Io {
        path: PathBuf,
        #[source]
        source: io::Error,
    },
}

/// What the session says it is doing, as written in `status`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum SessionStatus {
    Busy,
    Idle,
    Waiting,
    Other(String),
}

impl<'de> Deserialize<'de> for SessionStatus {
    fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
    where
        D: serde::Deserializer<'de>,
    {
        let text = String::deserialize(deserializer)?;
        Ok(match text.as_str() {
            "busy" => SessionStatus::Busy,
            "idle" => SessionStatus::Idle,
            "waiting" => SessionStatus::Waiting,
            _ => SessionStatus::Other(text),
        })
    }
}

/// One `<pid>.json` file, with its `cwd` made canonical.
#[derive(Debug, Clone, PartialEq, Eq, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct SessionFile {
    pub pid: u32,
    pub session_id: String,
    pub cwd: String,
    pub started_at: i64,
    pub proc_start: String,
    pub version: Option<String>,
    pub kind: Option<String>,
    pub entrypoint: Option<String>,
    pub name: Option<String>,
    pub messaging_socket_path: Option<String>,
    pub status: SessionStatus,
    pub status_updated_at: i64,
}

/// Is the process that wrote this file still running?
pub trait Liveness {
    fn alive(&self, file: &SessionFile) -> bool;
}

/// The session files that were read, and the names of the ones that did not parse.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Running {
    pub sessions: Vec<SessionFile>,
    pub skipped: Vec<String>,
}

/// Every session file whose process is alive, sorted by `started_at`, then `pid`.
/// A missing folder is an empty list.
pub fn running(claude: &ClaudePaths, probe: &dyn Liveness) -> Result<Running, SessionsError> {
    let dir = claude.sessions_dir();
    let entries = match fs::read_dir(&dir) {
        Ok(entries) => entries,
        Err(source) if source.kind() == io::ErrorKind::NotFound => {
            return Ok(Running {
                sessions: Vec::new(),
                skipped: Vec::new(),
            })
        }
        Err(source) => return Err(SessionsError::Io { path: dir, source }),
    };
    let mut sessions = Vec::new();
    let mut skipped = Vec::new();
    for entry in entries {
        let entry = match entry {
            Ok(entry) => entry,
            Err(source) => {
                return Err(SessionsError::Io {
                    path: dir.clone(),
                    source,
                })
            }
        };
        let file_name = entry.file_name();
        let Some(name) = file_name.to_str() else {
            continue;
        };
        if !is_session_file_name(name) {
            continue;
        }
        match read_session_file(&entry.path()) {
            Read::Listed(file) => {
                if probe.alive(&file) {
                    sessions.push(*file);
                }
            }
            Read::Quiet => {}
            Read::Broken => skipped.push(name.to_string()),
        }
    }
    sessions.sort_by(|a, b| {
        a.started_at
            .cmp(&b.started_at)
            .then_with(|| a.pid.cmp(&b.pid))
    });
    skipped.sort();
    Ok(Running { sessions, skipped })
}

/// Only `<digits>.json` is a session file: the `.key` files beside it hold a
/// secret and are never opened.
fn is_session_file_name(name: &str) -> bool {
    match name.strip_suffix(".json") {
        Some(pid) => !pid.is_empty() && pid.bytes().all(|byte| byte.is_ascii_digit()),
        None => false,
    }
}

enum Read {
    /// A parsed file; the probe decides whether its session is listed.
    Listed(Box<SessionFile>),
    /// An empty file or a session that is just starting: nothing to list or name.
    Quiet,
    /// A file of the right name that cannot be read or parsed.
    Broken,
}

fn read_session_file(path: &Path) -> Read {
    let text = match fs::read_to_string(path) {
        Ok(text) => text,
        // The session ended cleanly between listing the folder and reading its file.
        Err(error) if error.kind() == io::ErrorKind::NotFound => return Read::Quiet,
        Err(_) => return Read::Broken,
    };
    if text.is_empty() {
        return Read::Quiet;
    }
    let value: serde_json::Value = match serde_json::from_str(&text) {
        Ok(value) => value,
        Err(_) => return Read::Broken,
    };
    let Some(object) = value.as_object() else {
        return Read::Broken;
    };
    // About 250 ms pass between writing the file and adding these keys.
    if !object.contains_key("status") || !object.contains_key("statusUpdatedAt") {
        return Read::Quiet;
    }
    let mut file: SessionFile = match serde_json::from_value(value) {
        Ok(file) => file,
        Err(_) => return Read::Broken,
    };
    file.cwd = project::canonical_cwd(&file.cwd);
    Read::Listed(Box::new(file))
}

/// `\\.\pipe\<name>` → `Some("<name>")`, inner backslashes kept; anything else → `None`.
pub fn pipe_name(messaging_socket_path: &str) -> Option<&str> {
    let name = messaging_socket_path.strip_prefix(PIPE_PREFIX)?;
    if name.is_empty() {
        None
    } else {
        Some(name)
    }
}

/// Windows compares named pipes without case, so the listed names are kept in
/// lower case and the looked-up name is lower-cased too.
#[cfg(any(windows, test))]
fn pipe_is_listed(pipes: &std::collections::HashSet<String>, name: &str) -> bool {
    pipes.contains(&name.to_lowercase())
}

/// Compares the dot-separated numbers with [`TESTED_CLAUDE_CODE`]; a missing
/// part is 0, and a version with a part that is not a number counts as newer.
pub fn newer_than_tested(version: &str) -> bool {
    let given: Vec<&str> = version.split('.').collect();
    let tested: Vec<&str> = TESTED_CLAUDE_CODE.split('.').collect();
    let count = given.len().max(tested.len());
    for index in 0..count {
        let a = match given.get(index) {
            Some(text) => match text.parse::<u64>() {
                Ok(number) => number,
                Err(_) => return true,
            },
            None => 0,
        };
        let b = tested
            .get(index)
            .and_then(|text| text.parse::<u64>().ok())
            .unwrap_or_default();
        if a != b {
            return a > b;
        }
    }
    false
}

#[cfg(target_os = "linux")]
pub struct SystemProbe;

#[cfg(target_os = "linux")]
impl SystemProbe {
    pub fn new() -> io::Result<SystemProbe> {
        Ok(SystemProbe)
    }
}

#[cfg(target_os = "linux")]
impl Liveness for SystemProbe {
    fn alive(&self, file: &SessionFile) -> bool {
        let path = format!("/proc/{}/stat", file.pid);
        let stat = match fs::read_to_string(path) {
            Ok(stat) => stat,
            Err(_) => return false,
        };
        stat_is_alive(&stat, &file.proc_start)
    }
}

/// Fields 3 (state), 4 (parent pid) and 22 (start time) of `/proc/<pid>/stat`.
/// The text after the last `)` starts at field 3, so the start time is the 19th
/// field after the state; the command name in `(…)` may itself contain spaces
/// and `)`.
#[cfg(target_os = "linux")]
fn state_start_and_parent(stat: &str) -> Option<(&str, &str, u32)> {
    let mut fields = stat.rsplit_once(')')?.1.split_whitespace();
    let state = fields.next()?;
    let parent = fields.next()?.parse().ok()?;
    let start_time = fields.nth(17)?;
    Some((state, start_time, parent))
}

/// Field 3 (state) and field 22 (start time) of `/proc/<pid>/stat`, through
/// [`state_start_and_parent`].
#[cfg(target_os = "linux")]
fn state_and_start_time(stat: &str) -> Option<(&str, &str)> {
    state_start_and_parent(stat).map(|(state, start_time, _)| (state, start_time))
}

/// The parent pid of a live process, from the same `/proc/<pid>/stat` reader
/// [`SystemProbe`] uses. `None` when the file cannot be read or has no parent
/// field.
#[cfg(target_os = "linux")]
pub(crate) fn proc_parent(pid: u32) -> Option<u32> {
    let stat = fs::read_to_string(format!("/proc/{pid}/stat")).ok()?;
    state_start_and_parent(&stat).map(|(_, _, parent)| parent)
}

/// A `Z` (zombie) is a killed process its parent has not reaped yet; `X` is
/// dead. Neither is running, even while field 22 still matches.
#[cfg(target_os = "linux")]
fn stat_is_alive(stat: &str, proc_start: &str) -> bool {
    match state_and_start_time(stat) {
        Some((state, start_time)) => state != "Z" && state != "X" && start_time == proc_start,
        None => false,
    }
}

/// The start-time half alone, for the test that reads `/proc/self/stat`.
#[cfg(all(target_os = "linux", test))]
fn start_time(stat: &str) -> Option<&str> {
    state_and_start_time(stat).map(|(_, start_time)| start_time)
}

#[cfg(windows)]
pub struct SystemProbe {
    pipes: std::collections::HashSet<String>,
}

#[cfg(windows)]
impl SystemProbe {
    /// Lists `\\.\pipe\` once; nothing is opened or connected to.
    pub fn new() -> io::Result<SystemProbe> {
        let mut pipes = std::collections::HashSet::new();
        for entry in fs::read_dir(PIPE_PREFIX)? {
            pipes.insert(entry?.file_name().to_string_lossy().to_lowercase());
        }
        Ok(SystemProbe { pipes })
    }
}

#[cfg(windows)]
impl Liveness for SystemProbe {
    fn alive(&self, file: &SessionFile) -> bool {
        match file.messaging_socket_path.as_deref().and_then(pipe_name) {
            Some(name) => pipe_is_listed(&self.pipes, name),
            None => false,
        }
    }
}

/// macOS: Claude Code writes `procStart` as `LC_ALL=C TZ=UTC ps -o lstart= -p
/// <pid>` prints it, trimmed (e.g. `Sat Nov 21 17:48:08 2026`; checked in
/// Claude Code 2.1.330). The probe lists every process once the same way and
/// compares.
#[cfg(target_os = "macos")]
pub struct SystemProbe {
    processes: std::collections::HashMap<u32, PsProcess>,
}

#[cfg(target_os = "macos")]
impl SystemProbe {
    /// Runs `/bin/ps` once for every process: its pid, state and start time,
    /// in the C locale and UTC as Claude Code reads them. Nothing else is
    /// read; no process is touched.
    pub fn new() -> io::Result<SystemProbe> {
        let output = std::process::Command::new("/bin/ps")
            .args(["-A", "-o", "pid=,stat=,lstart="])
            .env("LC_ALL", "C")
            .env("TZ", "UTC")
            .stdin(std::process::Stdio::null())
            .output()?;
        if !output.status.success() {
            return Err(io::Error::other(format!(
                "/bin/ps failed: {}",
                String::from_utf8_lossy(&output.stderr).trim()
            )));
        }
        Ok(SystemProbe {
            processes: ps_processes(&String::from_utf8_lossy(&output.stdout)),
        })
    }
}

#[cfg(target_os = "macos")]
impl Liveness for SystemProbe {
    fn alive(&self, file: &SessionFile) -> bool {
        self.processes
            .get(&file.pid)
            .is_some_and(|process| ps_is_alive(process, &file.proc_start))
    }
}

/// One line of `ps -o pid=,stat=,lstart=`.
#[cfg(any(target_os = "macos", test))]
#[derive(Debug, PartialEq, Eq)]
struct PsProcess {
    state: String,
    /// The start time with its spaces made single (`ps` pads a one-digit day).
    start: String,
}

/// The processes of `ps -o pid=,stat=,lstart=` output, by pid. A line that
/// does not have a pid, a state and a start time is left out.
#[cfg(any(target_os = "macos", test))]
fn ps_processes(text: &str) -> std::collections::HashMap<u32, PsProcess> {
    let mut processes = std::collections::HashMap::new();
    for line in text.lines() {
        let mut fields = line.split_whitespace();
        let (Some(pid), Some(state)) = (fields.next(), fields.next()) else {
            continue;
        };
        let Ok(pid) = pid.parse::<u32>() else {
            continue;
        };
        let start = fields.collect::<Vec<_>>().join(" ");
        if start.is_empty() {
            continue;
        }
        processes.insert(
            pid,
            PsProcess {
                state: state.to_string(),
                start,
            },
        );
    }
    processes
}

/// Running when the start times match (spaces made single on both sides) and
/// the state is not `Z`: a killed process its parent has not reaped yet.
#[cfg(any(target_os = "macos", test))]
fn ps_is_alive(process: &PsProcess, proc_start: &str) -> bool {
    let proc_start = proc_start.split_whitespace().collect::<Vec<_>>().join(" ");
    !process.state.starts_with('Z') && !proc_start.is_empty() && process.start == proc_start
}

#[cfg(not(any(target_os = "linux", target_os = "macos", windows)))]
pub struct SystemProbe;

#[cfg(not(any(target_os = "linux", target_os = "macos", windows)))]
impl SystemProbe {
    pub fn new() -> io::Result<SystemProbe> {
        Err(io::Error::new(
            io::ErrorKind::Unsupported,
            "seatswap cannot tell running Claude Code sessions on this system",
        ))
    }
}

#[cfg(not(any(target_os = "linux", target_os = "macos", windows)))]
impl Liveness for SystemProbe {
    fn alive(&self, _file: &SessionFile) -> bool {
        false
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::HashMap;
    use std::fs;
    use std::path::{Path, PathBuf};
    use tempfile::TempDir;

    /// README §5, the Linux session file, exactly as given.
    const LINUX_FILE: &str = r#"{"pid":41822,"sessionId":"7c1e4b2a-9d3f-4e6a-b8c5-2f0a1d7e9b34","cwd":"/home/sam/projects/atlas","startedAt":1794612960232,"procStart":"19664719","version":"2.1.322","peerProtocol":1,"peerFeatures":["ack"],"kind":"interactive","entrypoint":"cli","pidDomain":"host","asksForConvertedSkills":false,"messagingSocketPath":"/run/user/1000/cc-socks/41822.sock","name":"atlas-a6","nameSource":"derived","nameSince":1794612960232,"updatedAt":1794681295320,"status":"busy","statusUpdatedAt":1794681295320}"#;

    /// README §5, the Windows session file, exactly as given.
    const WINDOWS_FILE: &str = r#"{"pid":20312,"sessionId":"2b7d9c1e-5a4f-4c3b-9e8d-7f6a5b4c3d2e","cwd":"c:\\Users\\sam\\projects\\atlas","startedAt":1794696616830,"procStart":"134391688204417352","version":"2.1.322","peerProtocol":1,"peerFeatures":["notify_idle","artifact_yield"],"kind":"interactive","entrypoint":"claude-vscode","pidDomain":"win32:vega","asksForConvertedSkills":false,"messagingSocketPath":"\\\\.\\pipe\\clsvc\\cc-msg-0f3a9c2e7b1d4f6a8e5c3b7d9a1f2e4c","name":"atlas-2b","nameSource":"derived","nameSince":1794696616830,"updatedAt":1794697457635,"status":"busy","statusUpdatedAt":1794697457635}"#;

    struct FakeProbe(HashMap<u32, String>);

    impl Liveness for FakeProbe {
        fn alive(&self, file: &SessionFile) -> bool {
            self.0.get(&file.pid) == Some(&file.proc_start)
        }
    }

    fn folder() -> (TempDir, ClaudePaths, PathBuf) {
        let tmp = tempfile::tempdir().unwrap();
        let config = tmp.path().join("cfg");
        fs::create_dir(&config).unwrap();
        let claude = ClaudePaths::for_config_dir(&config, tmp.path());
        let sessions = claude.sessions_dir();
        fs::create_dir(&sessions).unwrap();
        (tmp, claude, sessions)
    }

    fn write(dir: &Path, name: &str, text: &str) {
        fs::write(dir.join(name), text).unwrap();
    }

    fn probe(pid: u32, proc_start: &str) -> FakeProbe {
        FakeProbe(HashMap::from([(pid, proc_start.to_string())]))
    }

    fn empty_probe() -> FakeProbe {
        FakeProbe(HashMap::new())
    }

    fn edited(
        text: &str,
        edit: impl FnOnce(&mut serde_json::Map<String, serde_json::Value>),
    ) -> String {
        let mut value: serde_json::Value = serde_json::from_str(text).unwrap();
        edit(value.as_object_mut().unwrap());
        value.to_string()
    }

    fn session_file(pid: u32, proc_start: &str) -> SessionFile {
        SessionFile {
            pid,
            session_id: "7c1e4b2a-9d3f-4e6a-b8c5-2f0a1d7e9b34".to_string(),
            cwd: "/home/sam/projects/atlas".to_string(),
            started_at: 1794612960232,
            proc_start: proc_start.to_string(),
            version: Some("2.1.322".to_string()),
            kind: Some("interactive".to_string()),
            entrypoint: Some("cli".to_string()),
            name: Some("atlas-a6".to_string()),
            messaging_socket_path: None,
            status: SessionStatus::Busy,
            status_updated_at: 1794681295320,
        }
    }

    #[test]
    fn a_live_session_is_listed() {
        let (_tmp, claude, dir) = folder();
        write(&dir, "41822.json", LINUX_FILE);
        let running = running(&claude, &probe(41822, "19664719")).unwrap();
        assert_eq!(running.sessions.len(), 1);
        assert_eq!(running.sessions[0].status, SessionStatus::Busy);
        assert_eq!(running.sessions[0].name.as_deref(), Some("atlas-a6"));
        assert!(running.skipped.is_empty());
    }

    #[test]
    fn a_dead_session_is_not() {
        let (_tmp, claude, dir) = folder();
        write(&dir, "41822.json", LINUX_FILE);
        let running = running(&claude, &empty_probe()).unwrap();
        assert!(running.sessions.is_empty());
        assert!(running.skipped.is_empty());
    }

    #[test]
    fn a_reused_process_id_is_not() {
        let (_tmp, claude, dir) = folder();
        write(&dir, "41822.json", LINUX_FILE);
        let running = running(&claude, &probe(41822, "99999999")).unwrap();
        assert!(running.sessions.is_empty());
    }

    #[test]
    fn an_unknown_status_is_kept_as_text() {
        let (_tmp, claude, dir) = folder();
        let text = edited(LINUX_FILE, |object| {
            object.insert("status".to_string(), "compacting".into());
        });
        write(&dir, "41822.json", &text);
        let running = running(&claude, &probe(41822, "19664719")).unwrap();
        assert_eq!(
            running.sessions[0].status,
            SessionStatus::Other("compacting".to_string())
        );
    }

    #[test]
    fn a_broken_file_is_skipped_and_named() {
        let (_tmp, claude, dir) = folder();
        write(&dir, "41822.json", LINUX_FILE);
        write(&dir, "41823.json", "{");
        let running = running(&claude, &probe(41822, "19664719")).unwrap();
        assert_eq!(running.sessions.len(), 1);
        assert_eq!(running.skipped, vec!["41823.json".to_string()]);
    }

    #[test]
    fn a_missing_folder_is_empty() {
        let tmp = tempfile::tempdir().unwrap();
        let config = tmp.path().join("cfg");
        fs::create_dir(&config).unwrap();
        let claude = ClaudePaths::for_config_dir(&config, tmp.path());
        let running = running(&claude, &empty_probe()).unwrap();
        assert!(running.sessions.is_empty());
        assert!(running.skipped.is_empty());
    }

    #[test]
    fn a_newer_claude_code_is_noticed() {
        assert!(newer_than_tested("2.1.323"));
        assert!(newer_than_tested("2.2.0"));
        assert!(newer_than_tested("next"));
        assert!(!newer_than_tested("2.1.322"));
        assert!(!newer_than_tested("2.1.99"));
    }

    #[test]
    fn only_pid_json_files_are_read() {
        let (_tmp, claude, dir) = folder();
        write(&dir, "41822.json", LINUX_FILE);
        let key = format!("41822.{}.key", "9f2c".repeat(16));
        write(&dir, &key, "{");
        write(&dir, "notes.json", "{");
        let running = running(&claude, &probe(41822, "19664719")).unwrap();
        assert_eq!(running.sessions.len(), 1);
        assert!(running.skipped.is_empty());
    }

    #[test]
    fn an_empty_file_is_ignored_quietly() {
        let (_tmp, claude, dir) = folder();
        write(&dir, "41824.json", "");
        let running = running(&claude, &empty_probe()).unwrap();
        assert!(running.sessions.is_empty());
        assert!(running.skipped.is_empty());
    }

    #[test]
    fn a_file_without_status_is_not_listed_yet() {
        let (_tmp, claude, dir) = folder();
        let text = edited(LINUX_FILE, |object| {
            object.remove("status");
            object.remove("updatedAt");
            object.remove("statusUpdatedAt");
        });
        write(&dir, "41822.json", &text);
        let running = running(&claude, &probe(41822, "19664719")).unwrap();
        assert!(running.sessions.is_empty());
        assert!(running.skipped.is_empty());
    }

    #[test]
    fn a_windows_file_parses() {
        let (_tmp, claude, dir) = folder();
        write(&dir, "20312.json", WINDOWS_FILE);
        let running = running(&claude, &probe(20312, "134391688204417352")).unwrap();
        assert_eq!(running.sessions.len(), 1);
        let file = &running.sessions[0];
        assert_eq!(file.proc_start, "134391688204417352");
        assert!(file.cwd.starts_with(r"C:\Users"));
        assert_eq!(file.entrypoint.as_deref(), Some("claude-vscode"));
        let socket = file.messaging_socket_path.as_deref();
        assert!(socket.is_some());
        assert_eq!(
            socket.and_then(pipe_name),
            Some(r"clsvc\cc-msg-0f3a9c2e7b1d4f6a8e5c3b7d9a1f2e4c")
        );
    }

    #[test]
    fn pipe_name_takes_what_follows_the_pipe_folder() {
        assert_eq!(pipe_name("/run/user/1000/cc-socks/41822.sock"), None);
        assert_eq!(pipe_name(r"\\.\pipe\"), None);
    }

    #[test]
    fn ps_lines_are_read_by_pid() {
        let text = concat!(
            "    1 Ss   Sat Nov  7 16:58:59 2026\n",
            " 1892 S+   Sat Nov 21 17:48:08 2026\n",
            "41822 Z    Fri Nov 20 08:03:19 2026\n",
            "garbage\n",
            "  77 R\n",
        );
        let processes = ps_processes(text);
        assert_eq!(processes.len(), 3, "{processes:?}");
        assert_eq!(
            processes[&1],
            PsProcess {
                state: "Ss".to_string(),
                start: "Sat Nov 7 16:58:59 2026".to_string(),
            }
        );
        assert!(ps_is_alive(&processes[&1892], "Sat Nov 21 17:48:08 2026"));
        // Claude Code trims, so a one-digit day keeps its two spaces.
        assert!(ps_is_alive(&processes[&1], "Sat Nov  7 16:58:59 2026"));
        assert!(!ps_is_alive(&processes[&1892], "Sat Nov 21 17:48:09 2026"));
        assert!(!ps_is_alive(&processes[&1892], ""));
        assert!(
            !ps_is_alive(&processes[&41822], "Fri Nov 20 08:03:19 2026"),
            "a zombie is not running"
        );
    }

    #[test]
    #[cfg(target_os = "macos")]
    fn the_probe_knows_its_own_process() {
        let output = std::process::Command::new("/bin/ps")
            .args(["-o", "lstart=", "-p", &std::process::id().to_string()])
            .env("LC_ALL", "C")
            .env("TZ", "UTC")
            .output()
            .unwrap();
        let own_start = String::from_utf8(output.stdout).unwrap().trim().to_string();
        let probe = SystemProbe::new().unwrap();
        assert!(probe.alive(&session_file(std::process::id(), &own_start)));
        assert!(!probe.alive(&session_file(
            std::process::id(),
            "Thu Jan  1 00:00:00 1970"
        )));
        assert!(!probe.alive(&session_file(u32::MAX, &own_start)));
    }

    #[test]
    #[cfg(target_os = "linux")]
    fn the_probe_knows_its_own_process() {
        let stat = fs::read_to_string("/proc/self/stat").unwrap();
        let own_start = start_time(&stat).unwrap().to_string();
        let probe = SystemProbe::new().unwrap();
        assert!(probe.alive(&session_file(std::process::id(), &own_start)));
        assert!(!probe.alive(&session_file(std::process::id(), "1")));
        assert!(!probe.alive(&session_file(u32::MAX, &own_start)));
    }

    #[test]
    #[cfg(target_os = "linux")]
    fn the_stat_line_is_split_after_the_last_paren() {
        // Fields 4..=21 hold 1..=18; field 22 is the start time.
        let before_start: Vec<String> = (1..=18).map(|value| value.to_string()).collect();
        let stat = format!(
            "41822 (claude (x) y) S {} 19664719 1 1 0 -1 123 0 0 0 0 0 0 0 0 0 0 0 0 0",
            before_start.join(" ")
        );
        assert_eq!(state_and_start_time(&stat), Some(("S", "19664719")));
        assert!(stat_is_alive(&stat, "19664719"));

        let zombie = stat.replacen(") S ", ") Z ", 1);
        assert_eq!(state_and_start_time(&zombie), Some(("Z", "19664719")));
        assert!(!stat_is_alive(&zombie, "19664719"));

        let dead = stat.replacen(") S ", ") X ", 1);
        assert_eq!(state_and_start_time(&dead), Some(("X", "19664719")));
        assert!(!stat_is_alive(&dead, "19664719"));
    }

    #[test]
    #[cfg(target_os = "linux")]
    fn the_stat_line_gives_the_parent_too() {
        // Fields 4..=21 hold 1..=18; field 4 is the parent pid and field 22 the
        // start time.
        let before_start: Vec<String> = (1..=18).map(|value| value.to_string()).collect();
        let stat = format!(
            "41822 (claude (x) y) S {} 19664719 1 1 0 -1 123 0 0 0 0 0 0 0 0 0 0 0 0 0",
            before_start.join(" ")
        );
        assert_eq!(state_start_and_parent(&stat), Some(("S", "19664719", 1)));
    }

    #[test]
    fn sessions_are_sorted_by_start_then_pid() {
        let (_tmp, claude, dir) = folder();
        let session_with = |pid: u32, started_at: i64| {
            edited(LINUX_FILE, |object| {
                object.insert("pid".to_string(), pid.into());
                object.insert("startedAt".to_string(), started_at.into());
            })
        };
        write(&dir, "41830.json", &session_with(41830, 1794612960000));
        write(&dir, "41822.json", &session_with(41822, 1794612970000));
        write(&dir, "41825.json", &session_with(41825, 1794612960000));
        write(&dir, "41826.json", "{");
        write(&dir, "41824.json", "{");
        let probe = FakeProbe(HashMap::from([
            (41822, "19664719".to_string()),
            (41825, "19664719".to_string()),
            (41830, "19664719".to_string()),
        ]));
        let running = running(&claude, &probe).unwrap();
        let pids: Vec<u32> = running.sessions.iter().map(|file| file.pid).collect();
        assert_eq!(pids, vec![41825, 41830, 41822]);
        assert_eq!(
            running.skipped,
            vec!["41824.json".to_string(), "41826.json".to_string()]
        );
    }

    #[test]
    fn one_missing_status_key_is_enough() {
        let (_tmp, claude, dir) = folder();
        let without_updated = edited(LINUX_FILE, |object| {
            object.remove("statusUpdatedAt");
        });
        write(&dir, "41822.json", &without_updated);
        let without_status = edited(LINUX_FILE, |object| {
            object.remove("status");
        });
        write(&dir, "41823.json", &without_status);
        let probe = FakeProbe(HashMap::from([
            (41822, "19664719".to_string()),
            (41823, "19664719".to_string()),
        ]));
        let running = running(&claude, &probe).unwrap();
        assert!(running.sessions.is_empty());
        assert!(running.skipped.is_empty());
    }

    #[test]
    fn the_tested_version_is_all_numbers() {
        for part in TESTED_CLAUDE_CODE.split('.') {
            assert!(part.parse::<u64>().is_ok(), "{part} is not a number");
        }
    }

    #[test]
    fn pipe_names_are_compared_without_case() {
        let pipes = std::collections::HashSet::from([
            r"clsvc\cc-msg-0f3a9c2e7b1d4f6a8e5c3b7d9a1f2e4c".to_string(),
        ]);
        assert!(pipe_is_listed(
            &pipes,
            r"clsvc\CC-MSG-0F3A9C2E7B1D4F6A8E5C3B7D9A1F2E4C"
        ));
        assert!(!pipe_is_listed(
            &pipes,
            r"clsvc\cc-msg-00000000000000000000000000000000"
        ));
    }

    #[test]
    #[cfg(windows)]
    fn a_pipe_that_is_not_listed_is_not_alive() {
        let probe = SystemProbe::new().unwrap();
        let mut file = session_file(20312, "134391688204417352");
        file.messaging_socket_path =
            Some(r"\\.\pipe\clsvc\cc-msg-00000000000000000000000000000000".to_string());
        assert!(!probe.alive(&file));
        file.messaging_socket_path = None;
        assert!(!probe.alive(&file));
    }
}
