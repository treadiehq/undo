//! Persistent daemon logging.
//!
//! Writes a rolling log to `~/.undo/undo.log` (mode `0o600`, consistent with the
//! rest of the data dir) so daemon errors and crashes survive the terminal that
//! launched `undo start`. Warnings and errors are *teed* to stderr as before, so
//! the interactive experience is unchanged.
//!
//! Multiple daemons (one per watched project) share a single log file. Writes use
//! `O_APPEND` and every line carries a `[pid]` prefix so concurrent daemons stay
//! attributable. Size-based rotation is best-effort: with several daemons writing
//! at once the rename can race, which at worst drops a rotation cycle — acceptable
//! for a debug log and far simpler than per-project files.

use std::fs::{File, OpenOptions};
use std::io::Write;
use std::path::{Path, PathBuf};
use std::sync::{Mutex, OnceLock};

/// Rotate once the active log passes this size, keeping one prior file.
const MAX_LOG_BYTES: u64 = 5 * 1024 * 1024;

/// Log file name inside the data directory.
const LOG_FILE_NAME: &str = "undo.log";

#[derive(Clone, Copy)]
pub enum Level {
    Info,
    Notice,
    Warn,
    Error,
}

impl Level {
    fn as_str(self) -> &'static str {
        match self {
            Level::Info => "INFO",
            Level::Notice => "INFO",
            Level::Warn => "WARN",
            Level::Error => "ERROR",
        }
    }
}

/// Owns the open log file and performs size-based rotation. Kept separate from
/// the global handle so it can be unit-tested directly with a small cap.
pub struct Logger {
    path: PathBuf,
    rotated: PathBuf,
    file: File,
    max_bytes: u64,
}

impl Logger {
    pub fn open(path: PathBuf, max_bytes: u64) -> std::io::Result<Self> {
        let file = open_append_0600(&path)?;
        let rotated = rotated_path(&path);
        Ok(Self {
            path,
            rotated,
            file,
            max_bytes,
        })
    }

    /// Rename the active log to `<name>.1` and reopen a fresh file once the cap
    /// is exceeded. Best-effort: a failed rename leaves the current file in place.
    fn rotate_if_needed(&mut self) {
        let len = self.file.metadata().map(|m| m.len()).unwrap_or(0);
        if len < self.max_bytes {
            return;
        }
        if std::fs::rename(&self.path, &self.rotated).is_ok()
            && let Ok(file) = open_append_0600(&self.path)
        {
            self.file = file;
        }
    }

    fn write_line(&mut self, level: Level, msg: &str) {
        self.rotate_if_needed();
        let ts = chrono::Local::now().format("%Y-%m-%dT%H:%M:%S%.3f");
        // One write per line; O_APPEND keeps concurrent daemons' lines intact.
        let _ = writeln!(
            self.file,
            "{} [{}] {} {}",
            ts,
            std::process::id(),
            level.as_str(),
            msg
        );
        let _ = self.file.flush();
    }
}

fn rotated_path(path: &Path) -> PathBuf {
    let name = path
        .file_name()
        .map(|n| n.to_string_lossy().into_owned())
        .unwrap_or_else(|| LOG_FILE_NAME.to_string());
    path.with_file_name(format!("{}.1", name))
}

fn open_append_0600(path: &Path) -> std::io::Result<File> {
    use std::os::unix::fs::OpenOptionsExt;
    let file = OpenOptions::new()
        .create(true)
        .append(true)
        .mode(0o600)
        .open(path)?;
    // Enforce 0o600 on pre-existing files too (create+mode only applies on create).
    use std::os::unix::fs::PermissionsExt;
    let _ = std::fs::set_permissions(path, std::fs::Permissions::from_mode(0o600));
    Ok(file)
}

static LOGGER: OnceLock<Mutex<Logger>> = OnceLock::new();

/// Full path to the daemon log inside `bt_dir`.
pub fn log_path(bt_dir: &Path) -> PathBuf {
    bt_dir.join(LOG_FILE_NAME)
}

/// Initialise the global daemon logger. Idempotent and infallible from the
/// caller's perspective: if the file can't be opened, logging silently degrades
/// to stderr-only rather than failing daemon startup.
pub fn init(bt_dir: &Path) {
    if LOGGER.get().is_some() {
        return;
    }
    if let Ok(logger) = Logger::open(log_path(bt_dir), MAX_LOG_BYTES) {
        let _ = LOGGER.set(Mutex::new(logger));
    }
}

/// Install a panic hook that records the panic (with backtrace) to the log before
/// delegating to the previous hook, so a crash in the watch loop leaves a trace.
pub fn install_panic_hook() {
    let previous = std::panic::take_hook();
    std::panic::set_hook(Box::new(move |info| {
        let backtrace = std::backtrace::Backtrace::force_capture();
        let message = format!("PANIC: {}\n{}", info, backtrace);
        if !try_to_file(Level::Error, &message) {
            // A panic may have occurred while this thread held LOGGER. Never wait
            // for that lock from the panic hook because unwinding cannot release
            // the guard until the hook returns.
            let _ = writeln!(std::io::stderr().lock(), "{}", message);
        }
        previous(info);
    }));
}

fn try_to_file(level: Level, msg: &str) -> bool {
    LOGGER
        .get()
        .is_some_and(|handle| try_write_line(handle, level, msg))
}

fn try_write_line(handle: &Mutex<Logger>, level: Level, msg: &str) -> bool {
    let Ok(mut logger) = handle.try_lock() else {
        return false;
    };
    logger.write_line(level, msg);
    true
}

fn to_file(level: Level, msg: &str) {
    if let Some(handle) = LOGGER.get()
        && let Ok(mut logger) = handle.lock()
    {
        logger.write_line(level, msg);
    }
}

/// File-only: lifecycle breadcrumbs that don't need to interrupt the terminal.
pub fn info(msg: &str) {
    to_file(Level::Info, msg);
}

/// Plain stderr line + log: visible operational notices (e.g. auto-prune summary).
pub fn notice(msg: &str) {
    eprintln!("{}", msg);
    to_file(Level::Notice, msg);
}

/// Yellow `warning:` on stderr + log.
pub fn warn(msg: &str) {
    eprintln!("{}warning:{} {}", crate::YELLOW, crate::RESET, msg);
    to_file(Level::Warn, msg);
}

/// Red `error:` on stderr + log.
pub fn error(msg: &str) {
    eprintln!("{}error:{} {}", crate::RED, crate::RESET, msg);
    to_file(Level::Error, msg);
}

/// `crate::log_info!(...)` — file-only formatted breadcrumb.
#[macro_export]
macro_rules! log_info {
    ($($arg:tt)*) => { $crate::logging::info(&std::format!($($arg)*)) };
}

/// `crate::log_notice!(...)` — stderr + file.
#[macro_export]
macro_rules! log_notice {
    ($($arg:tt)*) => { $crate::logging::notice(&std::format!($($arg)*)) };
}

/// `crate::log_warn!(...)` — yellow stderr warning + file.
#[macro_export]
macro_rules! log_warn {
    ($($arg:tt)*) => { $crate::logging::warn(&std::format!($($arg)*)) };
}

/// `crate::log_error!(...)` — red stderr error + file.
#[macro_export]
macro_rules! log_error {
    ($($arg:tt)*) => { $crate::logging::error(&std::format!($($arg)*)) };
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::os::unix::fs::PermissionsExt;

    /// The log file is created mode 0o600 so it never leaks contents to other users.
    #[test]
    fn open_creates_log_file_with_owner_only_perms() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join(LOG_FILE_NAME);
        let _logger = Logger::open(path.clone(), MAX_LOG_BYTES).unwrap();
        assert!(path.exists(), "log file must be created on open");
        let mode = std::fs::metadata(&path).unwrap().permissions().mode() & 0o777;
        assert_eq!(mode, 0o600, "log file must be owner-only");
    }

    /// Each line carries the process id and level so concurrent daemons stay attributable.
    #[test]
    fn write_line_includes_pid_and_level() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join(LOG_FILE_NAME);
        let mut logger = Logger::open(path.clone(), MAX_LOG_BYTES).unwrap();
        logger.write_line(Level::Warn, "disk gremlins detected");
        let contents = std::fs::read_to_string(&path).unwrap();
        assert!(contents.contains("WARN"), "level missing: {contents}");
        assert!(
            contents.contains("disk gremlins detected"),
            "message missing: {contents}"
        );
        assert!(
            contents.contains(&format!("[{}]", std::process::id())),
            "pid prefix missing: {contents}"
        );
    }

    /// Writes accumulate (append mode) rather than truncating prior content.
    #[test]
    fn writes_are_appended() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join(LOG_FILE_NAME);
        let mut logger = Logger::open(path.clone(), MAX_LOG_BYTES).unwrap();
        logger.write_line(Level::Info, "first");
        logger.write_line(Level::Info, "second");
        let contents = std::fs::read_to_string(&path).unwrap();
        assert!(contents.contains("first") && contents.contains("second"));
        assert_eq!(contents.lines().count(), 2);
    }

    /// Panic-path logging must return immediately when a log write already owns
    /// the mutex; blocking here would deadlock before unwinding can drop the guard.
    #[test]
    fn panic_log_write_skips_locked_logger() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join(LOG_FILE_NAME);
        let handle =
            Mutex::new(Logger::open(path.clone(), MAX_LOG_BYTES).expect("open test logger"));
        let guard = handle.lock().unwrap();

        assert!(!try_write_line(
            &handle,
            Level::Error,
            "panic while logging"
        ));

        drop(guard);
        assert!(std::fs::read_to_string(path).unwrap().is_empty());
    }

    /// Once the active log passes the cap it is rotated to `<name>.1` and a fresh
    /// file takes over, so the log can never grow without bound.
    #[test]
    fn rotation_triggers_past_cap_and_keeps_one_prior_file() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join(LOG_FILE_NAME);
        let rotated = dir.path().join("undo.log.1");
        // Tiny cap so a handful of lines triggers rotation.
        let mut logger = Logger::open(path.clone(), 128).unwrap();
        for _ in 0..50 {
            logger.write_line(
                Level::Info,
                "filler line long enough to exceed the tiny cap quickly",
            );
        }
        assert!(
            rotated.exists(),
            "a rotated .1 file must exist after exceeding the cap"
        );
        assert!(
            path.exists(),
            "the active log must be reopened after rotation"
        );
        let mode = std::fs::metadata(&path).unwrap().permissions().mode() & 0o777;
        assert_eq!(mode, 0o600, "the reopened log must remain owner-only");
    }
}
