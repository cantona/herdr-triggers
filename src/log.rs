//! Daemon log.
//!
//! herdr's plugin log records the hook command that started this daemon - which
//! exits immediately - and knows nothing of the detached process it leaves
//! running, so the daemon keeps its own record here, small enough to read from
//! a `status` action.

use std::io::Write;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU8, Ordering};
use std::sync::{Mutex, OnceLock};

const MAX_LOG_BYTES: u64 = 256 * 1024;
const KEEP_ON_ROTATE_BYTES: usize = 128 * 1024;

/// How much gets written. Set from `[settings] log` on every config load.
///
/// The default is `All` on purpose: this file is the ONLY record of what a
/// resident daemon did - herdr's own plugin log covers commands that finished,
/// which this never does - so silence has to be asked for, not stumbled into.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Level {
    /// Nothing written once the config has loaded, so the file is never
    /// created. A config that fails to parse is still reported - otherwise the
    /// failure would be invisible.
    Off,
    /// What happened: fires, failures, config loads, stop. Everything except
    /// the two lines explaining a rule declining to act.
    Fires,
    /// Also why a rule declined: a match outside `tail_within`, and one held by
    /// the cooldown. Both deduped per occurrence rather than logged per poll.
    All,
}

impl Level {
    pub fn parse(name: &str) -> Option<Self> {
        match name {
            "off" => Some(Self::Off),
            "fires" => Some(Self::Fires),
            "all" => Some(Self::All),
            _ => None,
        }
    }
}

static LEVEL: AtomicU8 = AtomicU8::new(2);

pub fn set_level(level: Level) {
    LEVEL.store(
        match level {
            Level::Off => 0,
            Level::Fires => 1,
            Level::All => 2,
        },
        Ordering::Relaxed,
    );
}

pub fn level() -> Level {
    match LEVEL.load(Ordering::Relaxed) {
        0 => Level::Off,
        1 => Level::Fires,
        _ => Level::All,
    }
}

static LOG_PATH: OnceLock<Mutex<Option<PathBuf>>> = OnceLock::new();

fn slot() -> &'static Mutex<Option<PathBuf>> {
    LOG_PATH.get_or_init(|| Mutex::new(None))
}

pub fn init(path: &Path) {
    // A mode on create does nothing for a file that already exists, and this
    // log predates the tightening.
    tighten(path);
    if let Ok(mut guard) = slot().lock() {
        *guard = Some(path.to_path_buf());
    }
}

/// Makes an existing file owner-only. Silent when it does not exist.
pub fn tighten(path: &Path) {
    #[cfg(unix)]
    if path.exists() {
        use std::os::unix::fs::PermissionsExt;
        let _ = std::fs::set_permissions(path, std::fs::Permissions::from_mode(0o600));
    }
}

/// A reason a rule declined to act on something it matched. Only recorded at
/// `all`: it is commentary on what did NOT happen, which is noise once you
/// trust the rule.
pub fn write_detail(message: &str) {
    if level() == Level::All {
        write(message);
    }
}

pub fn write(message: &str) {
    if level() == Level::Off {
        return;
    }
    let Ok(guard) = slot().lock() else {
        return;
    };
    let Some(path) = guard.as_ref() else {
        // Not a daemon run (CLI subcommand, or tests): stderr is the log.
        eprintln!("{message}");
        return;
    };
    rotate_if_needed(path);
    if let Some(parent) = path.parent() {
        let _ = std::fs::create_dir_all(parent);
    }
    let line = format!("{} {message}\n", timestamp());
    // 0600: the log records prompt text and which rules fired, and
    // `triggers-status` prints it. Owner-only is the right default.
    if let Ok(mut file) = open_owner_only(path) {
        let _ = file.write_all(line.as_bytes());
    }
}

/// Append-opens the log, creating it owner-only.
fn open_owner_only(path: &Path) -> std::io::Result<std::fs::File> {
    let mut options = std::fs::OpenOptions::new();
    options.create(true).append(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt;
        options.mode(0o600);
    }
    options.open(path)
}

fn rotate_if_needed(path: &Path) {
    let Ok(metadata) = std::fs::metadata(path) else {
        return;
    };
    if metadata.len() <= MAX_LOG_BYTES {
        return;
    }
    let Ok(text) = std::fs::read_to_string(path) else {
        return;
    };
    let start = text.len().saturating_sub(KEEP_ON_ROTATE_BYTES);
    // Resume at a line boundary so the rotated log does not start mid-entry.
    let start = text[start..]
        .find('\n')
        .map(|offset| start + offset + 1)
        .unwrap_or(start);
    let _ = std::fs::write(path, &text[start..]);
}

/// Epoch seconds with milliseconds. Deliberately not a formatted date: pulling
/// in a time crate for a log prefix is not worth the dependency. The
/// milliseconds matter - whole seconds cannot tell a 20 ms gap from a 1 s one,
/// which is exactly what you need when asking why a trigger felt slow.
fn timestamp() -> String {
    let now = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap_or_default();
    format!("[{}.{:03}]", now.as_secs(), now.subsec_millis())
}

#[macro_export]
macro_rules! log_line {
    ($($arg:tt)*) => {
        $crate::log::write(&format!($($arg)*))
    };
}

/// For the "declined to act, because" lines; silent below `log = "all"`.
#[macro_export]
macro_rules! log_detail {
    ($($arg:tt)*) => {
        $crate::log::write_detail(&format!($($arg)*))
    };
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn levels_parse_and_only_known_names_are_accepted() {
        assert_eq!(Level::parse("off"), Some(Level::Off));
        assert_eq!(Level::parse("fires"), Some(Level::Fires));
        assert_eq!(Level::parse("all"), Some(Level::All));
        assert_eq!(Level::parse("quiet"), None, "a typo must fail at load");
        assert_eq!(Level::parse("Off"), None, "names are lower case");
    }

    #[test]
    fn the_default_level_records_everything() {
        // Silence has to be asked for: this file is the only record of what a
        // resident daemon did.
        assert_eq!(level(), Level::All);
    }
}
