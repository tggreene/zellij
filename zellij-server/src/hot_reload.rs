//! Hot reload support via the fd daemon.
//!
//! The fd daemon is a separate process that holds PTY master file descriptors
//! across binary upgrades. During hot reload:
//! 1. Server stores all live PTY master fds with the daemon, keyed by terminal_id
//! 2. Server exits gracefully (layout is already serialized every 5s)
//! 3. New binary starts, retrieves fds from daemon
//! 4. PTY spawn path looks up fds by terminal_id rather than FIFO popping,
//!    so resurrection ordering doesn't have to match storage ordering.
//!
//! Resilience:
//! - fds are fstat'd before being sent to the daemon (skips dead descriptors)
//! - if the new server asks for a terminal_id that has no stored fd, it falls
//!   back to spawning a fresh shell rather than producing a dead pane
//! - unconsumed fds are logged + closed once resurrection has settled

use std::collections::HashMap;
use std::os::unix::io::RawFd;
use std::sync::Mutex;

use zellij_fd_daemon::FdDaemonClient;

lazy_static::lazy_static! {
    /// Fds retrieved from the daemon during hot reload, keyed by their original
    /// terminal_id. The PTY spawn path looks up by id, so reordering during
    /// resurrection (or terminal_id allocation drift) doesn't desync the mapping.
    pub static ref HOT_RELOAD_FDS: Mutex<HashMap<u32, RawFd>> = Mutex::new(HashMap::new());

    /// Set to true when the server is exiting for hot reload.
    /// Prevents Pty::drop from sending SIGHUP to child processes (they should survive).
    pub static ref HOT_RELOAD_IN_PROGRESS: Mutex<bool> = Mutex::new(false);

    /// Session name for the current hot-reload-in-flight, used by cleanup
    /// to tell the daemon to shut down once resurrection has consumed
    /// everything it needs.
    static ref HOT_RELOAD_SESSION: Mutex<Option<String>> = Mutex::new(None);
}

const TERMINAL_KEY_PREFIX: &str = "terminal_";

fn terminal_key(terminal_id: u32) -> String {
    format!("{}{}", TERMINAL_KEY_PREFIX, terminal_id)
}

fn parse_terminal_key(key: &str) -> Option<u32> {
    key.strip_prefix(TERMINAL_KEY_PREFIX)
        .and_then(|s| s.parse::<u32>().ok())
}

/// Mark that a hot reload is in progress (called before server exit).
pub fn set_hot_reload_in_progress() {
    *HOT_RELOAD_IN_PROGRESS.lock().unwrap() = true;
}

/// Check if hot reload is in progress.
pub fn is_hot_reload_in_progress() -> bool {
    *HOT_RELOAD_IN_PROGRESS.lock().unwrap()
}

/// Store all current PTY master fds with the fd daemon for safekeeping.
/// Called before the server exits during hot reload.
///
/// Validates each fd with fstat before sending so we never store a closed
/// descriptor (which would surface as a dead pane after reload).
pub fn store_fds_with_daemon(
    session_name: &str,
    terminal_id_to_raw_fd: &std::collections::BTreeMap<u32, Option<RawFd>>,
) -> Result<(), String> {
    zellij_fd_daemon::ensure_daemon_running(session_name)?;

    let client = FdDaemonClient::connect(session_name)
        .ok_or_else(|| "failed to connect to fd daemon after starting it".to_string())?;

    let mut stored = 0;
    let mut skipped_dead = 0;
    let mut skipped_none = 0;
    for (terminal_id, maybe_fd) in terminal_id_to_raw_fd {
        let fd = match maybe_fd {
            Some(fd) => *fd,
            None => {
                skipped_none += 1;
                continue;
            },
        };
        if let Err(e) = nix::sys::stat::fstat(fd) {
            log::warn!(
                "Hot reload: skipping terminal_id {} — fd {} is dead before store: {}",
                terminal_id, fd, e
            );
            skipped_dead += 1;
            continue;
        }
        let key = terminal_key(*terminal_id);
        client.store(&key, fd)?;
        log::info!("Hot reload: stored fd {} as {}", fd, key);
        stored += 1;
    }
    log::info!(
        "Hot reload: store summary — stored {}, skipped {} dead, skipped {} unset",
        stored, skipped_dead, skipped_none
    );

    Ok(())
}

/// Try to retrieve fds from the daemon (called during resurrection).
/// Populates HOT_RELOAD_FDS keyed by terminal_id. Dead fds are logged + dropped.
pub fn try_retrieve_fds_from_daemon(session_name: &str) {
    let client = match FdDaemonClient::connect(session_name) {
        Some(c) => c,
        None => {
            log::info!("Hot reload: no fd daemon running for session {}", session_name);
            return;
        },
    };

    let fd_map = match client.retrieve_all() {
        Ok(m) => m,
        Err(e) => {
            log::warn!("Hot reload: failed to retrieve fds from daemon: {}", e);
            return;
        },
    };

    if fd_map.is_empty() {
        log::info!("Hot reload: daemon had no fds stored");
        return;
    }

    let mut typed = HashMap::new();
    let mut unparseable = Vec::new();
    let mut dead = Vec::new();
    for (key, fd) in fd_map {
        let terminal_id = match parse_terminal_key(&key) {
            Some(id) => id,
            None => {
                unparseable.push(key);
                let _ = nix::unistd::close(fd);
                continue;
            },
        };
        if let Err(e) = nix::sys::stat::fstat(fd) {
            log::warn!("Hot reload: dropping dead fd {} for {}: {}", fd, key, e);
            dead.push(terminal_id);
            let _ = nix::unistd::close(fd);
            continue;
        }
        typed.insert(terminal_id, fd);
    }

    log::info!(
        "Hot reload: retrieved {} live fds for terminal_ids {:?} (dropped {} unparseable, {} dead)",
        typed.len(),
        {
            let mut ids: Vec<u32> = typed.keys().copied().collect();
            ids.sort();
            ids
        },
        unparseable.len(),
        dead.len()
    );

    *HOT_RELOAD_FDS.lock().unwrap() = typed;
    *HOT_RELOAD_SESSION.lock().unwrap() = Some(session_name.to_string());
}

/// True if any fds remain in the hot-reload pool.
pub fn has_hot_reload_fds() -> bool {
    !HOT_RELOAD_FDS.lock().unwrap().is_empty()
}

/// Look up the stored fd for a specific terminal_id.
/// Returns None if no fd was stored for that id (caller should fall back to
/// spawning a fresh terminal — better a fresh shell than a dead pane).
pub fn pop_hot_reload_fd(terminal_id: u32) -> Option<RawFd> {
    let mut guard = HOT_RELOAD_FDS.lock().unwrap();
    if guard.is_empty() {
        // Fresh session, no hot reload in flight — silent fast path.
        return None;
    }
    let fd = guard.remove(&terminal_id);
    match fd {
        Some(fd) => log::info!(
            "Hot reload: matched fd {} to terminal_id {} ({} fds remaining)",
            fd, terminal_id, guard.len()
        ),
        None => log::warn!(
            "Hot reload: no stored fd for terminal_id {} — caller will spawn fresh ({} fds remaining for other ids)",
            terminal_id, guard.len()
        ),
    }
    fd
}

/// Log status of leftover hot-reload fds, but DO NOT close them.
///
/// Resurrection is layout-driven and lazy: panes in inactive tabs don't
/// spawn until the user activates that tab. Earlier versions of this
/// function closed "orphaned" fds at 60s, which prematurely killed shells
/// in unactivated tabs — when the user finally switched, the pane came
/// up as a fresh shell. We now leave HOT_RELOAD_FDS populated for the
/// server's lifetime so a tab activated 5 minutes after reload still
/// recovers its original PTY.
///
/// The fd-daemon (separate process) still has its own dups and will idle
/// out on its 5-minute timer, which bounds total kernel memory if the
/// resurrection never gets around to claiming things.
pub fn cleanup_unused_hot_reload_fds() {
    let remaining = HOT_RELOAD_FDS.lock().unwrap().len();
    if remaining > 0 {
        let ids: Vec<u32> = {
            let guard = HOT_RELOAD_FDS.lock().unwrap();
            let mut v: Vec<u32> = guard.keys().copied().collect();
            v.sort();
            v
        };
        log::info!(
            "Hot reload: {} fds still unclaimed after settling window for \
             terminal_ids {:?}. Leaving them in the pool — panes in inactive \
             tabs may claim them on activation. Daemon idle-timeout will \
             reclaim if they go truly stale.",
            remaining, ids
        );
    }
    // Clear the recorded session name so we don't accidentally try to talk
    // to the daemon on a future invocation (e.g. via stale Arc).
    let _ = HOT_RELOAD_SESSION.lock().unwrap().take();
}
