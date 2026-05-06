//! Zellij logging utility functions.

use std::{
    fs,
    io::{self, prelude::*},
    os::unix::io::RawFd,
    path::{Path, PathBuf},
};

use log::LevelFilter;

use log4rs::append::rolling_file::{
    policy::compound::{
        roll::fixed_window::FixedWindowRoller, trigger::size::SizeTrigger, CompoundPolicy,
    },
    RollingFileAppender,
};
use log4rs::config::{Appender, Config, Logger, Root};
use log4rs::encode::pattern::PatternEncoder;

use crate::consts::{ZELLIJ_TMP_DIR, ZELLIJ_TMP_LOG_DIR, ZELLIJ_TMP_LOG_FILE};
use crate::envs;
use crate::shared::set_permissions;

const LOG_MAX_BYTES: u64 = 1024 * 1024 * 16; // 16 MiB per log

/// Replace any character that isn't safe in a filename (alphanumeric, dash,
/// underscore, dot) with an underscore. Session names from users can include
/// spaces, slashes, and other characters that don't belong in a path.
fn sanitize_for_filename(name: &str) -> String {
    name.chars()
        .map(|c| match c {
            'a'..='z' | 'A'..='Z' | '0'..='9' | '-' | '_' | '.' => c,
            _ => '_',
        })
        .collect()
}

/// The log file path for the current process. If ZELLIJ_SESSION_NAME is set
/// (server processes, CLI commands invoked from inside a session) the log
/// goes to a per-session file; otherwise we fall back to the shared zellij.log
/// for early-CLI / pre-session work (list-sessions, --help, etc.).
pub fn current_log_file() -> PathBuf {
    match envs::get_session_name() {
        Ok(name) if !name.is_empty() => {
            ZELLIJ_TMP_LOG_DIR.join(format!("zellij-{}.log", sanitize_for_filename(&name)))
        },
        _ => ZELLIJ_TMP_LOG_FILE.clone(),
    }
}

/// Roll-over filename pattern for the current log file (used by the size
/// trigger to rotate `zellij-<session>.log` to `zellij-<session>.log.old.1`).
fn current_log_roll_pattern(log_file: &Path) -> String {
    let stem = log_file
        .file_stem()
        .and_then(|s| s.to_str())
        .unwrap_or("zellij");
    ZELLIJ_TMP_LOG_DIR
        .join(format!("{}.log.old.{{}}", stem))
        .to_str()
        .unwrap()
        .to_string()
}

pub fn configure_logger() {
    atomic_create_dir(&*ZELLIJ_TMP_DIR).unwrap();
    atomic_create_dir(&*ZELLIJ_TMP_LOG_DIR).unwrap();
    let log_file = current_log_file();
    atomic_create_file(&log_file).unwrap();

    let trigger = SizeTrigger::new(LOG_MAX_BYTES);
    let roll_pattern = current_log_roll_pattern(&log_file);
    let roller = FixedWindowRoller::builder()
        .build(&roll_pattern, 1)
        .unwrap();

    // {n} means platform dependent newline
    // module is padded to exactly 25 bytes and thread is padded to be between 10 and 15 bytes.
    let file_pattern = "{highlight({level:<6})} |{module:<25.25}| {date(%Y-%m-%d %H:%M:%S.%3f)} [{thread:<10.15}] [{file}:{line}]: {message} {n}";

    // default zellij appender, should be used across most of the codebase.
    let log_file_appender = RollingFileAppender::builder()
        .encoder(Box::new(PatternEncoder::new(file_pattern)))
        .build(
            &log_file,
            Box::new(CompoundPolicy::new(
                Box::new(trigger),
                Box::new(roller.clone()),
            )),
        )
        .unwrap();

    // plugin appender. To be used in logging_pipe to forward stderr output from plugins. We do some formatting
    // in logging_pipe to print plugin name as 'module' and plugin_id instead of thread.
    let log_plugin = RollingFileAppender::builder()
        .encoder(Box::new(PatternEncoder::new(
            "{highlight({level:<6})} {message} {n}",
        )))
        .build(
            &log_file,
            Box::new(CompoundPolicy::new(Box::new(trigger), Box::new(roller))),
        )
        .unwrap();

    // Set the default logging level to "info" and log it to zellij.log file
    // Decrease verbosity for `wasmtime_wasi` module because it has a lot of useless info logs
    // For `zellij_server::logging_pipe`, we use custom format as we use logging macros to forward stderr output from plugins
    let config = Config::builder()
        .appender(Appender::builder().build("logFile", Box::new(log_file_appender)))
        .appender(Appender::builder().build("logPlugin", Box::new(log_plugin)))
        // reduce the verbosity of isahc, otherwise it logs on every failed web request
        .logger(
            Logger::builder()
                .appender("logFile")
                .build("isahc", LevelFilter::Error),
        )
        .logger(
            Logger::builder()
                .appender("logPlugin")
                .build("wasmtime_wasi", LevelFilter::Warn),
        )
        .logger(
            Logger::builder()
                .appender("logPlugin")
                .additive(false)
                .build("zellij_server::logging_pipe", LevelFilter::Trace),
        )
        .build(Root::builder().appender("logFile").build(LevelFilter::Info))
        .unwrap();

    let _ = log4rs::init_config(config).unwrap();
}

pub fn atomic_create_file(file_name: &Path) -> io::Result<()> {
    let _ = fs::OpenOptions::new()
        .append(true)
        .create(true)
        .open(file_name)?;
    set_permissions(file_name, 0o600)
}

pub fn atomic_create_dir(dir_name: &Path) -> io::Result<()> {
    let result = if let Err(e) = fs::create_dir(dir_name) {
        if e.kind() == std::io::ErrorKind::AlreadyExists {
            Ok(())
        } else {
            Err(e)
        }
    } else {
        Ok(())
    };
    if result.is_ok() {
        set_permissions(dir_name, 0o700)?;
    }
    result
}

pub fn debug_to_file(message: &[u8], pid: RawFd) -> io::Result<()> {
    let mut path = PathBuf::new();
    path.push(&*ZELLIJ_TMP_LOG_DIR);
    path.push(format!("zellij-{}.log", pid));

    let mut file = fs::OpenOptions::new()
        .append(true)
        .create(true)
        .open(&path)?;
    set_permissions(&path, 0o600)?;
    file.write_all(message)
}
