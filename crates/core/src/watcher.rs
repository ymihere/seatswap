//! Notices the changes Claude Code makes to the live login (spec §6.4).
//!
//! The watcher looks only at file names and at Claude Code's lock folders; it never
//! reads or writes the login files. After a quiet period it calls `on_change` on its
//! own thread, and only while Claude Code holds none of its locks.

use std::ffi::OsStr;
use std::fs;
use std::io;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::mpsc::{self, Receiver, RecvTimeoutError};
use std::sync::{Arc, Mutex, MutexGuard, PoisonError};
use std::thread::{self, JoinHandle};
use std::time::{Duration, Instant};

use notify::event::{AccessKind, AccessMode, EventKind};
use notify::{Event, RecommendedWatcher, RecursiveMode, Watcher as NotifyWatcher};

use crate::locks;
use crate::paths::ClaudePaths;

/// How long the event stream has to stay quiet before `on_change` is considered.
pub const DEBOUNCE: Duration = Duration::from_millis(500);

/// A burst in a watched folder ([`watch_folder`]) fires at the latest this long
/// after its first change, even while changes keep arriving more often than
/// [`DEBOUNCE`]. The login watch ([`watch`]) has no such bound: a login written
/// many times while Claude Code starts is one change.
pub const DEBOUNCE_MAX: Duration = Duration::from_secs(1);

/// How often a busy Claude Code lock is re-checked.
pub const BUSY_RECHECK: Duration = Duration::from_millis(200);

/// How often the login watch calls `on_change` without an event when the live
/// login is a Keychain item (macOS), which cannot be watched (spec §6.4).
pub const KEYCHAIN_POLL: Duration = Duration::from_secs(30);

#[derive(Debug, thiserror::Error)]
pub enum WatchError {
    #[error("file watch: {0}")]
    Notify(#[from] notify::Error),
    #[error("{path}: {source}")]
    Io {
        path: PathBuf,
        #[source]
        source: std::io::Error,
    },
}

/// Whether the login watch sees Claude Code's folders.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum WatchState {
    /// Every folder that holds a login file exists and is watched.
    Watching,
    /// A folder that holds a login file does not exist yet: Claude Code has not
    /// been set up on this machine. The nearest existing parent is watched for
    /// it, so the folder and a later login are noticed without a restart.
    NotSetUp,
    /// The folders changed in a way the watch cannot follow (a path that is not
    /// a folder, one that cannot be read, a watch the system refused). The text
    /// says which path and why; a later change that fixes it ends this state.
    Failed(String),
}

/// Notices changes to the live login. Dropping it stops the watcher thread and
/// joins it, so no callback runs after `drop` returned.
pub struct Watcher {
    /// Shared with the thread, which moves the watches when a folder appears
    /// or goes away; `None` once dropped.
    backend: Arc<Mutex<Option<RecommendedWatcher>>>,
    state: Arc<Mutex<WatchState>>,
    stop: Arc<AtomicBool>,
    thread: Option<JoinHandle<()>>,
}

impl Watcher {
    /// Whether the login folders exist and are watched (see [`WatchState`]).
    pub fn state(&self) -> WatchState {
        lock(&self.state).clone()
    }
}

impl Drop for Watcher {
    fn drop(&mut self) {
        self.stop.store(true, Ordering::SeqCst);
        // Dropping the backend closes the channel, so an idle thread wakes at once.
        let backend = lock(&self.backend).take();
        drop(backend);
        if let Some(thread) = self.thread.take() {
            let _ = thread.join();
        }
    }
}

fn lock<T>(mutex: &Mutex<T>) -> MutexGuard<'_, T> {
    mutex.lock().unwrap_or_else(PoisonError::into_inner)
}

/// Watch the folders that hold the credentials file and the identity file.
/// Both are watched non-recursively (the files are replaced by rename); a folder
/// that is the same for both files is watched once. Events for other files, and
/// pure read-access events, are ignored.
///
/// A missing folder is not an error: Claude Code has not been set up yet
/// ([`WatchState::NotSetUp`]). Its nearest existing parent is watched instead,
/// and when the folder appears the watch moves to it and `on_change` is called
/// (files written before the new watch took hold are then read by the
/// callback). A folder that goes away moves the watch back up the same way: the
/// parent of every existing folder is watched as well, because on Windows the
/// watch of a removed folder ends without an event. No folder is created. A path that exists but is not a folder, or that
/// cannot be read, is an error.
///
/// When the live login is a Keychain item (`claude.login_keychain` is Some),
/// which no event reports, `on_change` is also called every [`KEYCHAIN_POLL`]
/// since the last call, likewise only while Claude Code's locks are free.
pub fn watch(
    claude: &ClaudePaths,
    on_change: impl Fn() + Send + 'static,
) -> Result<Watcher, WatchError> {
    let names = WatchedNames::of(claude)?;
    let mut folders: Vec<PathBuf> = Vec::new();
    for file in [&claude.credentials_file, &claude.identity_file] {
        let folder = file.parent().ok_or_else(|| WatchError::Io {
            path: file.clone(),
            source: io::Error::new(io::ErrorKind::InvalidInput, "the file has no parent folder"),
        })?;
        if !folders.iter().any(|seen| seen == folder) {
            folders.push(folder.to_path_buf());
        }
    }

    let (events, received) = mpsc::channel();
    let mut backend = RecommendedWatcher::new(events, notify::Config::default())?;
    let mut watched = Vec::new();
    let set_up = follow_folders(&mut backend, &mut watched, &folders)?;
    let state = Arc::new(Mutex::new(if set_up {
        WatchState::Watching
    } else {
        WatchState::NotSetUp
    }));
    let backend = Arc::new(Mutex::new(Some(backend)));

    let stop = Arc::new(AtomicBool::new(false));
    let layout = Layout {
        spellings: spellings_of(&folders),
        folders,
        watched,
        backend: Arc::clone(&backend),
        state: Arc::clone(&state),
    };
    let thread_stop = Arc::clone(&stop);
    let thread_paths = claude.clone();
    let spawned = thread::Builder::new()
        .name("seatswap-watcher".to_string())
        .spawn(move || {
            run(
                received,
                thread_stop,
                thread_paths,
                names,
                layout,
                KEYCHAIN_POLL,
                on_change,
            )
        });
    let thread = match spawned {
        Ok(thread) => thread,
        Err(source) => {
            return Err(WatchError::Io {
                path: claude.config_dir.clone(),
                source,
            });
        }
    };

    Ok(Watcher {
        backend,
        state,
        stop,
        thread: Some(thread),
    })
}

/// Where the login watch looks: the folders that hold the login files, the
/// paths watched for them now (each folder, or its nearest existing parent while
/// it is missing), the spellings an event may use for them ([`spellings_of`]),
/// and the state the GUI reads.
struct Layout {
    folders: Vec<PathBuf>,
    watched: Vec<PathBuf>,
    spellings: Vec<PathBuf>,
    backend: Arc<Mutex<Option<RecommendedWatcher>>>,
    state: Arc<Mutex<WatchState>>,
}

impl Layout {
    /// An event about one of the login folders or a parent of one may have
    /// created, removed or renamed it.
    fn touches(&self, event: &Event) -> bool {
        event.paths.iter().any(|path| {
            self.spellings
                .iter()
                .any(|spelling| spelling.starts_with(path))
        })
    }

    /// Plan the watches again after such an event. Returns true when the
    /// watched paths or the state changed: the login may have changed with them.
    fn follow(&mut self) -> bool {
        let before = self.watched.clone();
        let state = {
            let mut backend = lock(&self.backend);
            let Some(backend) = backend.as_mut() else {
                return false;
            };
            match follow_folders(backend, &mut self.watched, &self.folders) {
                Ok(true) => WatchState::Watching,
                Ok(false) => WatchState::NotSetUp,
                Err(error) => WatchState::Failed(error.to_string()),
            }
        };
        self.spellings = spellings_of(&self.folders);
        let mut current = lock(&self.state);
        let changed = *current != state || !same_paths(&before, &self.watched);
        *current = state;
        changed
    }
}

/// The path to watch for `folder`: the folder itself when it exists, else its
/// nearest existing parent. The flag says whether the folder itself exists. A
/// path on the way that exists but is not a folder, or cannot be read, is an
/// error.
fn watch_point(folder: &Path) -> Result<(PathBuf, bool), WatchError> {
    let mut candidate = folder;
    loop {
        match fs::metadata(candidate) {
            Ok(metadata) if metadata.is_dir() => {
                return Ok((candidate.to_path_buf(), candidate == folder));
            }
            Ok(_) => {
                return Err(WatchError::Io {
                    path: candidate.to_path_buf(),
                    source: io::Error::other("not a folder"),
                });
            }
            // "Not a directory" means a parent is a file: walking up finds it
            // and names it.
            Err(source)
                if matches!(
                    source.kind(),
                    io::ErrorKind::NotFound | io::ErrorKind::NotADirectory
                ) =>
            {
                candidate = match candidate.parent() {
                    Some(parent) if !parent.as_os_str().is_empty() => parent,
                    _ => {
                        return Err(WatchError::Io {
                            path: folder.to_path_buf(),
                            source,
                        })
                    }
                };
            }
            Err(source) => {
                return Err(WatchError::Io {
                    path: candidate.to_path_buf(),
                    source,
                });
            }
        }
    }
}

/// The same paths, in any order (neither list holds a path twice).
fn same_paths(a: &[PathBuf], b: &[PathBuf]) -> bool {
    a.len() == b.len() && a.iter().all(|path| b.contains(path))
}

/// Bring the backend's watches in line with the folders as they are now, and
/// say whether every folder exists. An existing folder is watched together with
/// its parent, where its own removal shows up. New watches are added before old ones are
/// removed, and the plan is checked again afterwards, so a folder that appears
/// in between is still followed.
fn follow_folders(
    backend: &mut RecommendedWatcher,
    watched: &mut Vec<PathBuf>,
    folders: &[PathBuf],
) -> Result<bool, WatchError> {
    loop {
        let mut wanted: Vec<PathBuf> = Vec::new();
        let mut set_up = true;
        for folder in folders {
            let (point, present) = watch_point(folder)?;
            set_up &= present;
            let parent = folder.parent().filter(|_| present);
            for path in std::iter::once(point).chain(parent.map(Path::to_path_buf)) {
                if !wanted.contains(&path) {
                    wanted.push(path);
                }
            }
        }
        if same_paths(&wanted, watched) {
            return Ok(set_up);
        }
        for path in &wanted {
            if watched.contains(path) {
                continue;
            }
            if let Err(error) = backend.watch(path, RecursiveMode::NonRecursive) {
                // The folder went away between the check and the watch: plan
                // again. Any other refusal is the error.
                if path.is_dir() {
                    return Err(error.into());
                }
                continue;
            }
            watched.push(path.clone());
        }
        let gone: Vec<PathBuf> = watched
            .iter()
            .filter(|path| !wanted.contains(path))
            .cloned()
            .collect();
        for path in gone {
            // A folder that was removed has lost its watch already, so the
            // system's answer here says nothing new.
            let _ = backend.unwatch(&path);
            watched.retain(|kept| *kept != path);
        }
    }
}

fn ensure_folder(path: &Path) -> Result<(), WatchError> {
    match fs::metadata(path) {
        Ok(metadata) if metadata.is_dir() => Ok(()),
        Ok(_) => Err(WatchError::Io {
            path: path.to_path_buf(),
            source: io::Error::other("not a folder"),
        }),
        Err(source) => Err(WatchError::Io {
            path: path.to_path_buf(),
            source,
        }),
    }
}

/// The two files an event can be about. A watched parent folder may hold
/// another file with the same name (`~/.claude.json` beside a `CLAUDE_CONFIG_DIR`
/// folder), so the whole path counts, not the name.
struct WatchedNames {
    credentials: PathBuf,
    identity: PathBuf,
}

impl WatchedNames {
    fn of(claude: &ClaudePaths) -> Result<WatchedNames, WatchError> {
        file_name(&claude.credentials_file)?;
        file_name(&claude.identity_file)?;
        Ok(WatchedNames {
            credentials: claude.credentials_file.clone(),
            identity: claude.identity_file.clone(),
        })
    }

    /// An event counts when it may have changed one of the watched files. Read
    /// access is not a change; counting it would make the callback (which reads
    /// the files) trigger itself.
    fn is_change(&self, event: &Event) -> bool {
        if is_read_access(&event.kind) {
            return false;
        }
        event.paths.iter().any(|path| self.is_watched(path))
    }

    fn is_watched(&self, path: &Path) -> bool {
        same_place(path, &self.credentials) || same_place(path, &self.identity)
    }
}

/// Whether the event path `event` names `ours`: the same path, or the same
/// name in the same folder once links are resolved. macOS (FSEvents) reports
/// the real path, `/private/var/…` for a folder under `/var/…` or the target
/// of a linked config folder. Only a path with the same name costs a look at
/// the disk.
fn same_place(event: &Path, ours: &Path) -> bool {
    if event == ours {
        return true;
    }
    if event.file_name().is_none() || event.file_name() != ours.file_name() {
        return false;
    }
    match (event.parent(), ours.parent()) {
        (Some(theirs), Some(mine)) => {
            let theirs = resolved(theirs);
            theirs.is_some() && theirs == resolved(mine)
        }
        _ => false,
    }
}

/// `path` with every link in the part of it that exists resolved: the real
/// path of its nearest existing ancestor (or of itself) with the missing rest
/// appended. `None` when not even an ancestor can be resolved.
fn resolved(path: &Path) -> Option<PathBuf> {
    let mut rest = Vec::new();
    let mut candidate = path;
    loop {
        if let Ok(mut real) = fs::canonicalize(candidate) {
            real.extend(rest.iter().rev());
            return Some(real);
        }
        rest.push(candidate.file_name()?);
        candidate = candidate
            .parent()
            .filter(|parent| !parent.as_os_str().is_empty())?;
    }
}

/// The spellings an event may use for `folders` and the folders above them:
/// each folder as given, its real path, and for the folder and every ancestor
/// the entry itself (a link stays a link) in its real parent. Refreshed when
/// the layout follows the folders, so a folder that has gone since is still
/// known by the path the system reported for it.
fn spellings_of(folders: &[PathBuf]) -> Vec<PathBuf> {
    let mut spellings: Vec<PathBuf> = Vec::new();
    let mut add = |path: PathBuf| {
        if !spellings.contains(&path) {
            spellings.push(path);
        }
    };
    for folder in folders {
        add(folder.clone());
        if let Some(real) = resolved(folder) {
            add(real);
        }
        for entry in folder.ancestors() {
            let (Some(parent), Some(name)) = (entry.parent(), entry.file_name()) else {
                continue;
            };
            if let Some(real_parent) = resolved(parent) {
                add(real_parent.join(name));
            }
        }
    }
    spellings
}

fn file_name(path: &Path) -> Result<&OsStr, WatchError> {
    path.file_name().ok_or_else(|| WatchError::Io {
        path: path.to_path_buf(),
        source: io::Error::new(io::ErrorKind::InvalidInput, "the file has no name"),
    })
}

fn is_read_access(kind: &EventKind) -> bool {
    matches!(
        kind,
        EventKind::Access(AccessKind::Open(_))
            | EventKind::Access(AccessKind::Close(AccessMode::Read))
    )
}

#[derive(Clone, Copy)]
enum Phase {
    /// No relevant event seen yet. With a poll interval, the phase ends when the
    /// next poll is due as if a quiet period had just ended.
    Idle,
    /// A relevant event was seen; wait until it has been quiet for [`DEBOUNCE`],
    /// but never past `deadline` (the burst's first change plus the loop's
    /// maximum wait, when it has one).
    Quiet {
        until: Instant,
        deadline: Option<Instant>,
    },
    /// Quiet period over; wait for Claude Code's locks to be free.
    WaitingForLocks,
}

/// How long the next wait may last. Idle waits until `next_poll` when the loop
/// polls, and otherwise has no timeout: the thread blocks on the event channel
/// until an event arrives or the backend is dropped (which closes the channel
/// and wakes it).
fn wait_timeout(phase: Phase, next_poll: Option<Instant>) -> Option<Duration> {
    match phase {
        Phase::Quiet { until, deadline } => {
            Some(fires_at(until, deadline).saturating_duration_since(Instant::now()))
        }
        Phase::WaitingForLocks => Some(BUSY_RECHECK),
        Phase::Idle => next_poll.map(|at| at.saturating_duration_since(Instant::now())),
    }
}

/// The moment a quiet phase ends: after the quiet time, or at the deadline if
/// that comes first.
fn fires_at(until: Instant, deadline: Option<Instant>) -> Instant {
    deadline.map_or(until, |deadline| until.min(deadline))
}

/// The login watch's loop. `keychain_poll` is the poll interval used when the
/// live login is a Keychain item ([`KEYCHAIN_POLL`]; tests pass a short one).
fn run(
    events: Receiver<notify::Result<Event>>,
    stop: Arc<AtomicBool>,
    claude: ClaudePaths,
    names: WatchedNames,
    mut layout: Layout,
    keychain_poll: Duration,
    on_change: impl Fn() + Send + 'static,
) {
    let poll = claude.login_keychain.as_ref().map(|_| keychain_poll);
    run_debounced(
        events,
        stop,
        |event| {
            // A login folder that appeared or went away counts as a change:
            // login files written before the new watch took hold are read by
            // the callback.
            let moved = !is_read_access(&event.kind) && layout.touches(event) && layout.follow();
            moved || names.is_change(event)
        },
        || !locks::claude_locks_busy(&claude),
        None,
        poll,
        on_change,
    );
}

/// One folder, watched non-recursively. Dropping it stops the watcher thread and
/// joins it, so no callback runs after `drop` returned.
pub struct FolderWatch {
    backend: Option<RecommendedWatcher>,
    stop: Arc<AtomicBool>,
    thread: Option<JoinHandle<()>>,
}

impl Drop for FolderWatch {
    fn drop(&mut self) {
        self.stop.store(true, Ordering::SeqCst);
        drop(self.backend.take());
        if let Some(thread) = self.thread.take() {
            let _ = thread.join();
        }
    }
}

/// Watch one folder non-recursively and call `on_change` on its own thread after
/// the event stream has been quiet for [`DEBOUNCE`], or at the latest
/// [`DEBOUNCE_MAX`] after the first change of a burst. Read access is ignored: the
/// callback reads the folder, which would otherwise make it trigger itself.
pub fn watch_folder(
    folder: &Path,
    on_change: impl Fn() + Send + 'static,
) -> Result<FolderWatch, WatchError> {
    ensure_folder(folder)?;
    let (events, received) = mpsc::channel();
    let mut backend = RecommendedWatcher::new(events, notify::Config::default())?;
    backend.watch(folder, RecursiveMode::NonRecursive)?;
    let stop = Arc::new(AtomicBool::new(false));
    let thread_stop = Arc::clone(&stop);
    let spawned = thread::Builder::new()
        .name("seatswap-folder-watch".to_string())
        .spawn(move || {
            run_debounced(
                received,
                thread_stop,
                |event| !is_read_access(&event.kind),
                || true,
                Some(DEBOUNCE_MAX),
                None,
                on_change,
            );
        });
    let thread = match spawned {
        Ok(thread) => thread,
        Err(source) => {
            return Err(WatchError::Io {
                path: folder.to_path_buf(),
                source,
            });
        }
    };
    Ok(FolderWatch {
        backend: Some(backend),
        stop,
        thread: Some(thread),
    })
}

/// The debounce loop shared by [`watch`] and [`watch_folder`]: wait for an event
/// that `is_change` accepts, then after [`DEBOUNCE`] quiet (and at the latest
/// `max_wait` after the burst's first change, when given) and while `ready`
/// holds, call `on_change` once. With `poll`, `on_change` is also due when
/// that long has passed since the start or the last call without one (and
/// still waits for `ready`).
fn run_debounced(
    events: Receiver<notify::Result<Event>>,
    stop: Arc<AtomicBool>,
    mut is_change: impl FnMut(&Event) -> bool,
    ready: impl Fn() -> bool,
    max_wait: Option<Duration>,
    poll: Option<Duration>,
    on_change: impl Fn(),
) {
    let mut phase = Phase::Idle;
    let mut next_poll = poll.map(|every| Instant::now() + every);
    loop {
        if stop.load(Ordering::SeqCst) {
            return;
        }
        if matches!(phase, Phase::WaitingForLocks) && ready() {
            if stop.load(Ordering::SeqCst) {
                return;
            }
            on_change();
            phase = Phase::Idle;
            next_poll = poll.map(|every| Instant::now() + every);
            continue;
        }
        let received = match wait_timeout(phase, next_poll) {
            Some(timeout) => events.recv_timeout(timeout),
            None => events.recv().map_err(|_| RecvTimeoutError::Disconnected),
        };
        match received {
            Ok(Ok(event)) => {
                if is_change(&event) {
                    let now = Instant::now();
                    let deadline = match phase {
                        Phase::Quiet { deadline, .. } => deadline,
                        Phase::Idle | Phase::WaitingForLocks => {
                            max_wait.map(|max_wait| now + max_wait)
                        }
                    };
                    phase = Phase::Quiet {
                        until: now + DEBOUNCE,
                        deadline,
                    };
                }
            }
            // An error inside the stream is not fatal: the next event still works.
            Ok(Err(_)) => {}
            Err(RecvTimeoutError::Timeout) => match phase {
                Phase::Quiet { until, deadline } => {
                    if Instant::now() >= fires_at(until, deadline) {
                        phase = Phase::WaitingForLocks;
                    }
                }
                Phase::Idle => {
                    if next_poll.is_some_and(|at| Instant::now() >= at) {
                        phase = Phase::WaitingForLocks;
                    }
                }
                Phase::WaitingForLocks => {}
            },
            Err(RecvTimeoutError::Disconnected) => return,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::atomic;
    use std::io::Write;
    use std::sync::atomic::AtomicUsize;
    use tempfile::TempDir;

    fn config_paths(tmp: &TempDir) -> ClaudePaths {
        let config = tmp.path().join("cfg");
        fs::create_dir(&config).unwrap();
        ClaudePaths::for_config_dir(&config, tmp.path())
    }

    fn home_paths(tmp: &TempDir) -> ClaudePaths {
        fs::create_dir(tmp.path().join(".claude")).unwrap();
        ClaudePaths::for_config_dir(&tmp.path().join(".claude"), tmp.path())
    }

    fn watched(paths: &ClaudePaths) -> (Watcher, Arc<AtomicUsize>, Receiver<()>) {
        let count = Arc::new(AtomicUsize::new(0));
        let (calls, received) = mpsc::channel();
        let counted = Arc::clone(&count);
        let watcher = watch(paths, move || {
            counted.fetch_add(1, Ordering::SeqCst);
            let _ = calls.send(());
        })
        .unwrap();
        (watcher, count, received)
    }

    /// Wait for the single expected call, then make sure no second one follows.
    fn expect_one(received: &Receiver<()>, count: &AtomicUsize) {
        received
            .recv_timeout(Duration::from_secs(3))
            .expect("on_change was not called");
        assert!(
            received.recv_timeout(Duration::from_millis(1200)).is_err(),
            "on_change was called more than once"
        );
        assert_eq!(count.load(Ordering::SeqCst), 1);
    }

    #[test]
    fn atomic_credentials_write_calls_once() {
        let tmp = TempDir::new().unwrap();
        let paths = config_paths(&tmp);
        let (watcher, count, received) = watched(&paths);

        atomic::write_atomic(&paths.credentials_file, b"{}", atomic::Access::Private).unwrap();
        expect_one(&received, &count);
        drop(watcher);
    }

    #[test]
    fn burst_of_writes_calls_once() {
        let tmp = TempDir::new().unwrap();
        let paths = config_paths(&tmp);
        let (watcher, count, received) = watched(&paths);

        // Claude Code rewrites the file about that often while it starts.
        for _ in 0..20 {
            atomic::write_atomic(&paths.credentials_file, b"{}", atomic::Access::Private).unwrap();
            thread::sleep(Duration::from_millis(50));
        }
        expect_one(&received, &count);
        drop(watcher);
    }

    #[test]
    fn a_burst_longer_than_the_quiet_time_still_wakes() {
        let tmp = TempDir::new().unwrap();
        let folder = tmp.path().join("subagents");
        fs::create_dir(&folder).unwrap();
        let (calls, received) = mpsc::channel();
        let watch = watch_folder(&folder, move || {
            let _ = calls.send(());
        })
        .unwrap();

        // An agent's transcript grows more often than DEBOUNCE, so a pure
        // trailing debounce would never fire while it writes.
        let transcript = folder.join("agent-a875.jsonl");
        let mut wakes = 0;
        let started = Instant::now();
        let mut line = 0;
        while started.elapsed() < Duration::from_secs(3) {
            line += 1;
            let mut file = fs::OpenOptions::new()
                .create(true)
                .append(true)
                .open(&transcript)
                .unwrap();
            writeln!(file, "{{\"line\":{line}}}").unwrap();
            drop(file);
            thread::sleep(Duration::from_millis(200));
            while received.try_recv().is_ok() {
                wakes += 1;
            }
        }
        assert!(
            wakes >= 2,
            "a 3 s burst every 200 ms woke on_change {wakes} times before it stopped"
        );
        drop(watch);
    }

    #[test]
    fn identity_change_in_the_home_layout_calls_once() {
        let tmp = TempDir::new().unwrap();
        let paths = home_paths(&tmp);
        assert_ne!(
            paths.credentials_file.parent(),
            paths.identity_file.parent(),
            "the home layout has two watched folders"
        );
        let (watcher, count, received) = watched(&paths);

        atomic::write_atomic(&paths.identity_file, b"{}", atomic::Access::Private).unwrap();
        expect_one(&received, &count);
        drop(watcher);
    }

    #[test]
    fn unrelated_files_are_ignored() {
        let tmp = TempDir::new().unwrap();
        let paths = config_paths(&tmp);
        let (watcher, count, received) = watched(&paths);

        atomic::write_atomic(
            &paths.config_dir.join("settings.json"),
            b"{}",
            atomic::Access::Private,
        )
        .unwrap();
        atomic::write_atomic(
            &paths.config_dir.join("history.jsonl"),
            b"{}\n",
            atomic::Access::Private,
        )
        .unwrap();
        assert!(
            received.recv_timeout(Duration::from_millis(1500)).is_err(),
            "an unrelated file must not call on_change"
        );
        assert_eq!(count.load(Ordering::SeqCst), 0);
        drop(watcher);
    }

    #[test]
    fn a_login_file_name_in_the_watched_parent_is_not_the_login() {
        // The config folder's parent is watched for the folder's removal; the
        // default `~/.claude.json` there is not this config folder's identity.
        let tmp = TempDir::new().unwrap();
        let paths = config_paths(&tmp);
        let (watcher, count, received) = watched(&paths);
        for name in [".claude.json", ".credentials.json"] {
            atomic::write_atomic(&tmp.path().join(name), b"{}", atomic::Access::Private).unwrap();
        }
        assert!(
            received.recv_timeout(Duration::from_millis(1500)).is_err(),
            "a file of the same name beside the config folder must not call on_change"
        );
        assert_eq!(count.load(Ordering::SeqCst), 0);
        drop(watcher);
    }

    #[test]
    fn busy_storage_lock_delays_the_call() {
        let tmp = TempDir::new().unwrap();
        let paths = config_paths(&tmp);
        let (watcher, count, received) = watched(&paths);

        let lock = paths.storage_write_lock();
        fs::create_dir(&lock).unwrap();
        let stop = Arc::new(AtomicBool::new(false));
        let heartbeat_stop = Arc::clone(&stop);
        let heartbeat = thread::spawn(move || {
            while !heartbeat_stop.load(Ordering::SeqCst) {
                let _ = filetime::set_file_mtime(&lock, filetime::FileTime::now());
                thread::sleep(Duration::from_millis(100));
            }
        });

        atomic::write_atomic(&paths.credentials_file, b"{}", atomic::Access::Private).unwrap();
        assert!(
            received.recv_timeout(Duration::from_millis(1500)).is_err(),
            "on_change must not run while Claude Code's storage lock is fresh"
        );
        assert_eq!(count.load(Ordering::SeqCst), 0);

        stop.store(true, Ordering::SeqCst);
        heartbeat.join().unwrap();
        fs::remove_dir(paths.storage_write_lock()).unwrap();

        received
            .recv_timeout(Duration::from_secs(2))
            .expect("on_change must run after the lock is gone");
        assert_eq!(count.load(Ordering::SeqCst), 1);
        drop(watcher);
    }

    /// Wait for at least one call, then for the burst to end.
    fn expect_some(received: &Receiver<()>, what: &str) {
        received
            .recv_timeout(Duration::from_secs(3))
            .unwrap_or_else(|_| panic!("on_change was not called after {what}"));
        while received.recv_timeout(Duration::from_millis(1200)).is_ok() {}
    }

    #[test]
    fn a_missing_config_folder_is_not_set_up_and_noticed_when_it_appears() {
        let tmp = TempDir::new().unwrap();
        let config = tmp.path().join("cfg");
        let paths = ClaudePaths::for_config_dir(&config, tmp.path());
        let (watcher, _count, received) = watched(&paths);
        assert_eq!(watcher.state(), WatchState::NotSetUp);

        fs::create_dir(&config).unwrap();
        expect_some(&received, "the folder appeared");
        assert_eq!(watcher.state(), WatchState::Watching);

        atomic::write_atomic(&paths.credentials_file, b"{}", atomic::Access::Private).unwrap();
        expect_some(&received, "the first login was written");
        drop(watcher);
    }

    #[test]
    fn claude_codes_first_start_in_the_home_layout_is_noticed() {
        let tmp = TempDir::new().unwrap();
        let paths = ClaudePaths::for_config_dir(&tmp.path().join(".claude"), tmp.path());
        let (watcher, _count, received) = watched(&paths);
        assert_eq!(watcher.state(), WatchState::NotSetUp);

        // Claude Code creates the folder and writes the login right after:
        // the file may land before the new watch holds, and is still noticed.
        fs::create_dir(&paths.config_dir).unwrap();
        atomic::write_atomic(&paths.credentials_file, b"{}", atomic::Access::Private).unwrap();
        atomic::write_atomic(&paths.identity_file, b"{}", atomic::Access::Private).unwrap();
        expect_some(&received, "the folder and the login appeared");
        assert_eq!(watcher.state(), WatchState::Watching);
        drop(watcher);
    }

    #[test]
    fn a_folder_several_levels_down_is_followed() {
        let tmp = TempDir::new().unwrap();
        let config = tmp.path().join("work").join("claude").join("cfg");
        let paths = ClaudePaths::for_config_dir(&config, tmp.path());
        let (watcher, _count, received) = watched(&paths);
        assert_eq!(watcher.state(), WatchState::NotSetUp);

        fs::create_dir_all(&config).unwrap();
        expect_some(&received, "the folders appeared");
        assert_eq!(watcher.state(), WatchState::Watching);

        atomic::write_atomic(&paths.credentials_file, b"{}", atomic::Access::Private).unwrap();
        expect_some(&received, "the login was written");
        drop(watcher);
    }

    #[test]
    fn a_removed_folder_is_followed_back_up_and_down() {
        let tmp = TempDir::new().unwrap();
        let paths = config_paths(&tmp);
        let (watcher, _count, received) = watched(&paths);
        assert_eq!(watcher.state(), WatchState::Watching);

        fs::remove_dir(&paths.config_dir).unwrap();
        expect_some(&received, "the folder was removed");
        assert_eq!(watcher.state(), WatchState::NotSetUp);

        fs::create_dir(&paths.config_dir).unwrap();
        expect_some(&received, "the folder came back");
        assert_eq!(watcher.state(), WatchState::Watching);
        atomic::write_atomic(&paths.credentials_file, b"{}", atomic::Access::Private).unwrap();
        expect_some(&received, "the login was written again");
        drop(watcher);
    }

    #[test]
    fn a_config_path_that_is_a_file_is_an_error() {
        let tmp = TempDir::new().unwrap();
        let config = tmp.path().join("cfg");
        fs::write(&config, b"").unwrap();
        let paths = ClaudePaths::for_config_dir(&config, tmp.path());
        let error = watch(&paths, || {})
            .err()
            .expect("a file is not a config folder");
        assert_eq!(
            error.to_string(),
            format!("{}: not a folder", config.display())
        );

        // A file on the way to the folder can never become it either.
        let below = ClaudePaths::for_config_dir(&config.join("inner"), tmp.path());
        let error = watch(&below, || {})
            .err()
            .expect("a file is not a parent folder");
        assert_eq!(
            error.to_string(),
            format!("{}: not a folder", config.display())
        );
    }

    #[cfg(unix)]
    #[test]
    fn a_config_folder_that_cannot_be_read_is_an_error() {
        use std::os::unix::fs::PermissionsExt;
        let tmp = TempDir::new().unwrap();
        let closed = tmp.path().join("closed");
        fs::create_dir(&closed).unwrap();
        fs::set_permissions(&closed, fs::Permissions::from_mode(0o000)).unwrap();
        let config = closed.join("cfg");
        let paths = ClaudePaths::for_config_dir(&config, tmp.path());
        let result = watch(&paths, || {});
        fs::set_permissions(&closed, fs::Permissions::from_mode(0o700)).unwrap();
        let error = result
            .err()
            .expect("a folder behind a closed one cannot be checked");
        assert!(
            error
                .to_string()
                .starts_with(&format!("{}: ", config.display())),
            "the error names the path: {error}"
        );
        assert!(error.to_string().contains("ermission denied"), "{error}");
    }

    /// A config folder that is a link to the real one, as with dotfiles kept
    /// elsewhere: `tmp/real` and `tmp/cfg -> real`.
    #[cfg(unix)]
    fn linked_config_paths(tmp: &TempDir) -> (ClaudePaths, PathBuf) {
        let real = tmp.path().join("real");
        fs::create_dir(&real).unwrap();
        let config = tmp.path().join("cfg");
        std::os::unix::fs::symlink(&real, &config).unwrap();
        (ClaudePaths::for_config_dir(&config, tmp.path()), real)
    }

    #[cfg(unix)]
    #[test]
    fn a_login_named_by_its_real_path_is_the_login() {
        let tmp = TempDir::new().unwrap();
        let (paths, real) = linked_config_paths(&tmp);
        let names = WatchedNames::of(&paths).unwrap();
        // macOS reports the real path; tempdirs there sit under the /var link too.
        let real = fs::canonicalize(&real).unwrap();
        assert!(names.is_watched(&paths.credentials_file));
        assert!(names.is_watched(&real.join(".credentials.json")));
        assert!(names.is_watched(&real.join(".claude.json")));
        assert!(!names.is_watched(&real.join("settings.json")));
        let real_tmp = fs::canonicalize(tmp.path()).unwrap();
        assert!(
            !names.is_watched(&real_tmp.join(".claude.json")),
            "the parent's file of the same name is not the login"
        );
    }

    #[cfg(unix)]
    #[test]
    fn a_missing_folder_named_by_its_real_parent_touches_the_layout() {
        let tmp = TempDir::new().unwrap();
        let link = tmp.path().join("home");
        std::os::unix::fs::symlink(tmp.path(), &link).unwrap();
        let folder = link.join("work").join("cfg");
        let layout = Layout {
            spellings: spellings_of(std::slice::from_ref(&folder)),
            folders: vec![folder],
            watched: Vec::new(),
            backend: Arc::new(Mutex::new(None)),
            state: Arc::new(Mutex::new(WatchState::NotSetUp)),
        };
        let real_tmp = fs::canonicalize(tmp.path()).unwrap();
        let about = |path: PathBuf| {
            Event::new(EventKind::Create(notify::event::CreateKind::Folder)).add_path(path)
        };
        assert!(layout.touches(&about(link.join("work"))));
        assert!(layout.touches(&about(real_tmp.join("work"))));
        assert!(layout.touches(&about(real_tmp.join("work").join("cfg"))));
        assert!(!layout.touches(&about(real_tmp.join("other"))));
        assert!(!layout.touches(&about(real_tmp.join("work").join("cfg2"))));
    }

    #[cfg(unix)]
    #[test]
    fn a_linked_config_folder_is_watched_through_the_link() {
        let tmp = TempDir::new().unwrap();
        let (paths, _real) = linked_config_paths(&tmp);
        let (watcher, count, received) = watched(&paths);
        assert_eq!(watcher.state(), WatchState::Watching);

        atomic::write_atomic(&paths.credentials_file, b"{}", atomic::Access::Private).unwrap();
        expect_one(&received, &count);
        drop(watcher);
    }

    #[cfg(unix)]
    #[test]
    fn a_linked_config_folder_whose_target_goes_is_followed() {
        let tmp = TempDir::new().unwrap();
        let (paths, real) = linked_config_paths(&tmp);
        let (watcher, _count, received) = watched(&paths);
        assert_eq!(watcher.state(), WatchState::Watching);

        fs::remove_dir(&real).unwrap();
        expect_some(&received, "the link's target was removed");
        assert_eq!(watcher.state(), WatchState::NotSetUp);
        drop(watcher);
    }

    #[test]
    fn idle_waits_without_a_timeout() {
        assert_eq!(wait_timeout(Phase::Idle, None), None);
        assert_eq!(
            wait_timeout(Phase::WaitingForLocks, None),
            Some(BUSY_RECHECK)
        );
    }

    #[test]
    fn idle_with_a_poll_waits_until_the_poll_is_due() {
        let timeout = wait_timeout(Phase::Idle, Some(Instant::now() + KEYCHAIN_POLL)).unwrap();
        assert!(timeout <= KEYCHAIN_POLL && timeout > KEYCHAIN_POLL - Duration::from_secs(1));
        let overdue = Instant::now()
            .checked_sub(Duration::from_millis(5))
            .unwrap();
        assert_eq!(
            wait_timeout(Phase::Idle, Some(overdue)),
            Some(Duration::ZERO)
        );
    }

    /// The login watch's loop on its own thread, fed by a channel the test
    /// holds, so no file event (and no FSEvents) is involved. `on_change`
    /// sends on the returned receiver.
    struct PolledLoop {
        stop: Arc<AtomicBool>,
        events: Option<mpsc::Sender<notify::Result<Event>>>,
        thread: Option<JoinHandle<()>>,
    }

    impl PolledLoop {
        fn start(paths: &ClaudePaths, every: Duration) -> (PolledLoop, Receiver<()>) {
            let (events, received_events) = mpsc::channel();
            let (calls, received) = mpsc::channel();
            let stop = Arc::new(AtomicBool::new(false));
            let layout = Layout {
                folders: Vec::new(),
                watched: Vec::new(),
                spellings: Vec::new(),
                backend: Arc::new(Mutex::new(None)),
                state: Arc::new(Mutex::new(WatchState::Watching)),
            };
            let names = WatchedNames::of(paths).unwrap();
            let thread_paths = paths.clone();
            let thread_stop = Arc::clone(&stop);
            let thread = thread::spawn(move || {
                run(
                    received_events,
                    thread_stop,
                    thread_paths,
                    names,
                    layout,
                    every,
                    move || {
                        let _ = calls.send(());
                    },
                )
            });
            let polled = PolledLoop {
                stop,
                events: Some(events),
                thread: Some(thread),
            };
            (polled, received)
        }
    }

    impl Drop for PolledLoop {
        fn drop(&mut self) {
            self.stop.store(true, Ordering::SeqCst);
            drop(self.events.take());
            if let Some(thread) = self.thread.take() {
                thread.join().unwrap();
            }
        }
    }

    /// A Keychain login that nothing here reads: the loop only calls back.
    fn with_keychain_login(mut paths: ClaudePaths, tmp: &TempDir) -> ClaudePaths {
        paths.login_keychain = Some(crate::paths::KeychainItem {
            service: "seatswap-test-credentials-poll".to_string(),
            account: "sam".to_string(),
            keychain: Some(tmp.path().join("never-used.keychain-db")),
        });
        paths
    }

    const SHORT_POLL: Duration = Duration::from_millis(30);

    #[test]
    fn a_keychain_login_is_polled() {
        let tmp = TempDir::new().unwrap();
        let paths = with_keychain_login(config_paths(&tmp), &tmp);
        let (polled, received) = PolledLoop::start(&paths, SHORT_POLL);
        for call in 1..=3 {
            received
                .recv_timeout(Duration::from_secs(2))
                .unwrap_or_else(|_| panic!("poll {call} did not call on_change"));
        }
        drop(polled);
    }

    #[test]
    fn a_file_login_is_not_polled() {
        let tmp = TempDir::new().unwrap();
        let paths = config_paths(&tmp);
        assert_eq!(paths.login_keychain, None);
        let (polled, received) = PolledLoop::start(&paths, SHORT_POLL);
        assert!(
            received.recv_timeout(SHORT_POLL * 10).is_err(),
            "a file login is noticed by events only"
        );
        drop(polled);
    }

    #[test]
    fn a_keychain_poll_waits_for_claude_codes_locks() {
        let tmp = TempDir::new().unwrap();
        let paths = with_keychain_login(config_paths(&tmp), &tmp);
        let lock = paths.refresh_lock();
        fs::create_dir(&lock).unwrap();
        let stop = Arc::new(AtomicBool::new(false));
        let heartbeat_stop = Arc::clone(&stop);
        let heartbeat_lock = lock.clone();
        let heartbeat = thread::spawn(move || {
            while !heartbeat_stop.load(Ordering::SeqCst) {
                let _ = filetime::set_file_mtime(&heartbeat_lock, filetime::FileTime::now());
                thread::sleep(Duration::from_millis(50));
            }
        });

        let (polled, received) = PolledLoop::start(&paths, SHORT_POLL);
        assert!(
            received.recv_timeout(SHORT_POLL * 10).is_err(),
            "no poll may call on_change while Claude Code renews the login"
        );

        stop.store(true, Ordering::SeqCst);
        heartbeat.join().unwrap();
        fs::remove_dir(&lock).unwrap();
        received
            .recv_timeout(Duration::from_secs(2))
            .expect("the poll must run once the lock is gone");
        drop(polled);
    }

    #[test]
    fn drop_while_idle_returns_within_a_second() {
        let tmp = TempDir::new().unwrap();
        let paths = config_paths(&tmp);
        let watcher = watch(&paths, || {}).unwrap();

        let started = Instant::now();
        drop(watcher);
        let elapsed = started.elapsed();
        assert!(
            elapsed < Duration::from_secs(1),
            "dropping an idle watcher took {elapsed:?}"
        );
    }

    #[test]
    fn no_call_after_drop() {
        let tmp = TempDir::new().unwrap();
        let paths = config_paths(&tmp);
        let count = Arc::new(AtomicUsize::new(0));
        let (calls, received) = mpsc::channel();
        // Keep the channel connected after the callback (and its sender) is gone.
        let keep = calls.clone();
        let counted = Arc::clone(&count);
        let watcher = watch(&paths, move || {
            counted.fetch_add(1, Ordering::SeqCst);
            let _ = calls.send(());
        })
        .unwrap();
        drop(watcher);

        atomic::write_atomic(&paths.credentials_file, b"{}", atomic::Access::Private).unwrap();
        assert!(
            received.recv_timeout(Duration::from_millis(1500)).is_err(),
            "no callback may run after the watcher is dropped"
        );
        assert_eq!(count.load(Ordering::SeqCst), 0);
        drop(keep);
    }
}
