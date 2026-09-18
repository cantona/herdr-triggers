//! herdr-triggers: regex-driven terminal triggers as a resident herdr plugin.
//!
//! herdr runs plugin hook commands and `wait()`s on them, with 32 in flight at
//! once, so `start` must detach and return immediately - a daemon holding a
//! hook slot forever would starve every other plugin.

mod actions;
mod client;
mod config;
mod engine;
mod log;
mod rules;

use std::path::{Path, PathBuf};
use std::sync::atomic::Ordering;

use client::Client;

const PLUGIN_ID: &str = "cantona.herdr-triggers";
const USAGE: &str = "\
usage: herdr-triggersd <command> [--session <name> | --socket <path> | --all]

A daemon runs per herdr server. With no target, the command acts on the server
the calling shell belongs to - which is whichever pane you happen to be in, so
be explicit when it matters:

  --session <name>   the named session ('default' = the default server; a
                     session actually named 'default' needs --socket)
  --socket <path>    a server's socket directly
  --all              every server found on this machine

  start    detach and run the trigger daemon (idempotent)
  restart  stop a running daemon, then start a fresh one
  run      run the daemon in the foreground
  stop     stop a running daemon
  status   report daemon state, rule count and recent log lines
  reload   re-read triggers.toml
  reset    re-arm every `once` rule
";

fn main() {
    let args: Vec<String> = std::env::args().skip(1).collect();
    let command = args.first().cloned().unwrap_or_default();

    // Resolve the target before anything reads the socket or state paths.
    // `get(1..)` not `[1..]`: a bare invocation has no element 1 and slicing
    // panicked, replacing the usage message with a stack trace.
    match parse_target(args.get(1..).unwrap_or(&[])) {
        Ok(Target::Inherit) => {}
        Ok(Target::One(path)) => client::set_socket_override(path),
        Ok(Target::All) => std::process::exit(run_for_every_server(&command)),
        Err(message) => {
            eprintln!("{message}\n\n{USAGE}");
            std::process::exit(2);
        }
    }
    let code = match command.as_str() {
        "start" => start(),
        "restart" => restart(),
        "run" => run_foreground(),
        "stop" => signal_daemon(libc::SIGTERM, "stop"),
        "reload" => signal_daemon(libc::SIGHUP, "reload"),
        "reset" => signal_daemon(libc::SIGUSR1, "reset"),
        "status" => status(),
        "-h" | "--help" | "help" => {
            print!("{USAGE}");
            0
        }
        other => {
            eprintln!("unknown command {other:?}\n\n{USAGE}");
            2
        }
    };
    std::process::exit(code);
}

enum Target {
    Inherit,
    One(PathBuf),
    All,
}

/// Reads the single optional target flag. One flag is the whole grammar, so
/// this is a match rather than a loop.
fn parse_target(rest: &[String]) -> Result<Target, String> {
    let Some(flag) = rest.first() else {
        return Ok(Target::Inherit);
    };
    let value = rest.get(1);
    // Reject an unknown flag before counting arguments, or the arity check
    // reports the wrong thing for a typo like `--sesion com`.
    if !matches!(flag.as_str(), "--all" | "--socket" | "--session") {
        return Err(format!("unknown option {flag:?}"));
    }
    // One target, or none. Silently honouring the first of two would act on a
    // server the reader did not name.
    let consumed = if flag == "--all" { 1 } else { 2 };
    if rest.len() > consumed {
        return Err(format!("unexpected argument {:?}", rest[consumed]));
    }
    match flag.as_str() {
        "--all" => Ok(Target::All),
        "--socket" => value
            .map(|path| Target::One(PathBuf::from(path)))
            .ok_or_else(|| "--socket needs a path".to_string()),
        "--session" => {
            let name = value.ok_or("--session needs a name")?;
            let sock = if name == "default" {
                herdr_config_dir().join("herdr.sock")
            } else {
                herdr_config_dir()
                    .join("sessions")
                    .join(name)
                    .join("herdr.sock")
            };
            if sock.exists() {
                Ok(Target::One(sock))
            } else {
                Err(format!(
                    "no herdr server for session {name:?} ({})",
                    sock.display()
                ))
            }
        }
        // The guard above already rejected anything else, so this exists only
        // to satisfy exhaustiveness on a &str match.
        _ => unreachable!("unknown flag {flag:?} passed the guard above"),
    }
}

/// Runs the command once per server, in a child so each gets its own resolved
/// socket and state dir. Returns the worst exit code.
fn run_for_every_server(command: &str) -> i32 {
    if command == "run" {
        // `run` is a foreground daemon: the first child would never return and
        // the rest would never start.
        eprintln!("run cannot be used with --all; it stays in the foreground");
        return 2;
    }
    let servers = client::discover_servers();
    if servers.is_empty() {
        eprintln!("no herdr server found");
        return 1;
    }
    let Ok(exe) = std::env::current_exe() else {
        eprintln!("cannot locate own binary");
        return 1;
    };
    let mut worst = 0;
    for (name, socket) in servers {
        println!("== {name} ==");
        let status = std::process::Command::new(&exe)
            .arg(command)
            .arg("--socket")
            .arg(&socket)
            .status();
        match status {
            Ok(status) => worst = worst.max(status.code().unwrap_or(1)),
            Err(err) => {
                eprintln!("cannot run for {name}: {err}");
                worst = worst.max(1);
            }
        }
    }
    worst
}

fn config_dir() -> PathBuf {
    env_dir("HERDR_PLUGIN_CONFIG_DIR").unwrap_or_else(|| {
        herdr_config_dir()
            .join("plugins")
            .join("config")
            .join(PLUGIN_ID)
    })
}

/// State lives per herdr SERVER, not per plugin.
///
/// A machine can run several herdr servers at once (a named session plus the
/// default one), each with its own panes, and one daemon can only attach to one
/// socket. Sharing a state dir would mean their pidfiles, locks and ledgers
/// collide, and the flock would let only the first daemon run - so the session
/// identity is part of the path.
fn state_dir() -> PathBuf {
    let base = env_dir("HERDR_PLUGIN_STATE_DIR")
        .unwrap_or_else(|| herdr_state_dir().join("plugins").join(PLUGIN_ID));
    match session_tag() {
        Some(tag) => base.join(tag),
        None => base,
    }
}

/// A filesystem-safe name for the server this daemon talks to.
///
/// A named session is prefixed, so a session that happens to be called
/// "default" cannot land on the same state dir as the actual default server -
/// two servers sharing a pidfile and lock is what lets only one daemon run.
fn session_tag() -> Option<String> {
    let socket = client::socket_path();
    // .../sessions/<name>/herdr.sock -> "session-<name>"; the default server
    // -> "default".
    let named = socket
        .parent()
        .filter(|dir| {
            dir.parent()
                .and_then(|p| p.file_name())
                .is_some_and(|n| n == "sessions")
        })
        .and_then(|dir| dir.file_name())
        .map(|name| format!("session-{}", name.to_string_lossy()))
        .unwrap_or_else(|| "default".to_string());
    let safe: String = named
        .chars()
        .map(|c| {
            if c.is_alphanumeric() || c == '-' || c == '_' {
                c
            } else {
                '_'
            }
        })
        .collect();
    (!safe.is_empty()).then_some(safe)
}

fn env_dir(key: &str) -> Option<PathBuf> {
    std::env::var(key)
        .ok()
        .filter(|value| !value.is_empty())
        .map(PathBuf::from)
}

fn herdr_config_dir() -> PathBuf {
    match std::env::var("XDG_CONFIG_HOME") {
        Ok(dir) if !dir.is_empty() => PathBuf::from(dir).join("herdr"),
        _ => home().join(".config").join("herdr"),
    }
}

fn herdr_state_dir() -> PathBuf {
    match std::env::var("XDG_STATE_HOME") {
        Ok(dir) if !dir.is_empty() => PathBuf::from(dir).join("herdr"),
        _ => home().join(".local").join("state").join("herdr"),
    }
}

fn home() -> PathBuf {
    std::env::var("HOME").map(PathBuf::from).unwrap_or_default()
}

fn pid_path() -> PathBuf {
    state_dir().join("triggersd.pid")
}

fn log_path() -> PathBuf {
    state_dir().join("triggersd.log")
}

fn start() -> i32 {
    if let Some(pid) = running_pid() {
        // herdr re-runs startup hooks; a second daemon would double every fire.
        println!("herdr-triggersd already running (pid {pid})");
        return 0;
    }
    // Detach before anything spawns a thread: fork() in a threaded process only
    // carries the calling thread into the child.
    match detach() {
        Detached::Parent => 0,
        Detached::Child => {
            daemon_main();
            0
        }
        Detached::Failed(err) => {
            eprintln!("cannot detach: {err}");
            1
        }
    }
}

/// Stops a running daemon and starts a fresh one.
///
/// Waits for the old daemon to actually release its lock rather than sleeping a
/// guessed interval: `start` refuses while the lock is held, so returning too
/// early would leave nothing running at all.
fn restart() -> i32 {
    if running_pid().is_some() {
        let code = signal_daemon(libc::SIGTERM, "stop");
        if code != 0 {
            return code;
        }
        let deadline = std::time::Instant::now() + std::time::Duration::from_secs(5);
        while !lock_is_free() {
            if std::time::Instant::now() >= deadline {
                eprintln!("old daemon still holds the lock after 5s; not starting a new one");
                return 1;
            }
            std::thread::sleep(std::time::Duration::from_millis(20));
        }
    } else {
        println!("nothing running; starting");
    }
    start()
}

fn run_foreground() -> i32 {
    daemon_main();
    0
}

fn daemon_main() {
    let state_dir = state_dir();
    let _ = std::fs::create_dir_all(&state_dir);
    log::init(&log_path());
    // Read the level BEFORE the first line is written: the engine applies it on
    // config load, which happens after the startup line, so `log = "off"` would
    // otherwise still leave a file behind with a couple of lines in it.
    if let Some(level) = config::load_triggers(&config_dir().join("triggers.toml"))
        .ok()
        .and_then(|loaded| log::Level::parse(&loaded.settings.log))
    {
        log::set_level(level);
    }
    // A single flock is the real mutual exclusion: the pidfile check races, and
    // two daemons would each answer every prompt - a password typed twice. The
    // lock is held for the process's life; the fd is leaked on purpose.
    if !acquire_singleton_lock(&state_dir) {
        log_line!("another herdr-triggersd already holds the lock; exiting");
        return;
    }
    write_pidfile();
    install_signal_handlers();

    let client = Client::from_env();
    log_line!(
        "herdr-triggersd {} starting (socket {})",
        env!("CARGO_PKG_VERSION"),
        client.path().display()
    );
    let mut engine = engine::Engine::new(client, &config_dir(), &state_dir);
    engine.run();
    let _ = std::fs::remove_file(pid_path());
}

enum Detached {
    Parent,
    Child,
    Failed(std::io::Error),
}

/// Standard double fork: the first fork lets the hook command return, `setsid`
/// leaves herdr's process group and session, and the second fork makes the
/// daemon a non-session-leader so it can never acquire a controlling terminal.
fn detach() -> Detached {
    match unsafe { libc::fork() } {
        -1 => return Detached::Failed(std::io::Error::last_os_error()),
        0 => {}
        _ => return Detached::Parent,
    }

    if unsafe { libc::setsid() } == -1 {
        return Detached::Failed(std::io::Error::last_os_error());
    }

    match unsafe { libc::fork() } {
        -1 => return Detached::Failed(std::io::Error::last_os_error()),
        0 => {}
        // herdr caps plugin command output at 64 KiB and reads it to EOF, so
        // the intermediate process exits immediately and closes the pipes.
        _ => unsafe { libc::_exit(0) },
    }

    redirect_standard_streams();
    Detached::Child
}

fn redirect_standard_streams() {
    unsafe {
        let null = libc::open(c"/dev/null".as_ptr(), libc::O_RDWR);
        if null >= 0 {
            libc::dup2(null, libc::STDIN_FILENO);
            libc::dup2(null, libc::STDOUT_FILENO);
            libc::dup2(null, libc::STDERR_FILENO);
            if null > libc::STDERR_FILENO {
                libc::close(null);
            }
        }
    }
}

extern "C" fn handle_signal(signal: libc::c_int) {
    // Async-signal-safe: only atomic stores.
    match signal {
        libc::SIGHUP => engine::RELOAD.store(true, Ordering::Relaxed),
        libc::SIGUSR1 => engine::RESET.store(true, Ordering::Relaxed),
        _ => engine::STOP.store(true, Ordering::Relaxed),
    }
}

/// The handlers only flip an atomic; the poll loop acts on it between
/// intervals. `SA_RESTART` is irrelevant here - the daemon sleeps between polls
/// rather than blocking in a read, so nothing needs interrupting.
fn install_signal_handlers() {
    for signal in [libc::SIGTERM, libc::SIGINT, libc::SIGHUP, libc::SIGUSR1] {
        unsafe {
            let mut action: libc::sigaction = std::mem::zeroed();
            action.sa_sigaction = handle_signal as *const () as usize;
            action.sa_flags = 0;
            libc::sigemptyset(&mut action.sa_mask);
            libc::sigaction(signal, &action, std::ptr::null_mut());
        }
    }
}

/// Takes an exclusive, non-blocking flock on a lock file and leaks the fd so
/// the lock lives as long as the process. Returns false if another daemon holds
/// it. The kernel drops the lock automatically when this process dies, so a
/// crashed daemon never wedges the next start.
fn acquire_singleton_lock(state_dir: &Path) -> bool {
    use std::os::fd::IntoRawFd;
    let path = state_dir.join("triggersd.lock");
    let mut options = std::fs::OpenOptions::new();
    options.create(true).write(true).truncate(false);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt;
        options.mode(0o600);
    }
    let Ok(file) = options.open(&path) else {
        return false;
    };
    // A mode on create does nothing to a lock file that already exists.
    crate::log::tighten(&path);
    // into_raw_fd releases the fd from File's ownership, so nothing closes it:
    // it stays open, and the lock held, until the process exits.
    let fd = file.into_raw_fd();
    if unsafe { libc::flock(fd, libc::LOCK_EX | libc::LOCK_NB) } != 0 {
        unsafe { libc::close(fd) };
        return false;
    }
    true
}

fn write_pidfile() {
    let path = pid_path();
    if let Some(parent) = path.parent() {
        let _ = std::fs::create_dir_all(parent);
    }
    crate::log::tighten(&path);
    let _ = std::fs::write(&path, format!("{}\n", std::process::id()));
    crate::log::tighten(&path);
}

/// The pid of a live daemon for THIS server, or `None`.
///
/// Liveness comes from the lock, not from the pid: pids are recycled, and
/// `kill(pid, 0)` succeeds for any process this user owns - so a stale pidfile
/// naming some unrelated process would make `start` refuse and, worse, make
/// `stop` signal that process. Only a daemon holds the flock, so if the lock
/// can be taken there is no daemon, whatever the pidfile says.
fn running_pid() -> Option<i32> {
    if lock_is_free() {
        return None;
    }
    let text = std::fs::read_to_string(pid_path()).ok()?;
    text.trim().parse().ok()
}

/// Tries the lock without blocking, releasing it immediately. `true` means no
/// daemon is running for this server.
fn lock_is_free() -> bool {
    use std::os::fd::IntoRawFd;
    let path = state_dir().join("triggersd.lock");
    if !path.exists() {
        return true;
    }
    let Ok(file) = std::fs::OpenOptions::new().write(true).open(&path) else {
        // Cannot tell; assume a daemon holds it rather than start a second one.
        return false;
    };
    let fd = file.into_raw_fd();
    let free = unsafe { libc::flock(fd, libc::LOCK_EX | libc::LOCK_NB) } == 0;
    if free {
        unsafe { libc::flock(fd, libc::LOCK_UN) };
    }
    unsafe { libc::close(fd) };
    free
}

fn signal_daemon(signal: libc::c_int, label: &str) -> i32 {
    let Some(pid) = running_pid() else {
        eprintln!("herdr-triggersd is not running");
        return 1;
    };
    if unsafe { libc::kill(pid, signal) } != 0 {
        eprintln!(
            "cannot {label} pid {pid}: {}",
            std::io::Error::last_os_error()
        );
        return 1;
    }
    println!("{label} sent to herdr-triggersd (pid {pid})");
    0
}

fn status() -> i32 {
    // Which server this command resolved to: a daemon runs per herdr server,
    // and the answer depends on ambient env, so print it rather than let the
    // reader assume.
    println!("herdr socket: {}", client::socket_path().display());
    println!("state dir:    {}", state_dir().display());
    match running_pid() {
        Some(pid) => println!("running (pid {pid})"),
        None => println!("not running"),
    }
    let triggers = config_dir().join("triggers.toml");
    match config::load_triggers(&triggers) {
        Ok(loaded) => println!("{} rules in {}", loaded.rules.len(), triggers.display()),
        Err(err) => println!("config: {err}"),
    }
    let ledger = state_dir().join("once-ledger.json");
    let fired = std::fs::read_to_string(&ledger)
        .ok()
        .and_then(|text| serde_json::from_str::<Vec<String>>(&text).ok())
        .map(|entries| entries.len())
        .unwrap_or(0);
    println!("{fired} once rules already fired ({})", ledger.display());
    match config::load_triggers(&config_dir().join("triggers.toml"))
        .ok()
        .and_then(|loaded| log::Level::parse(&loaded.settings.log))
    {
        Some(log::Level::Off) => println!("logging is off ([settings] log = \"off\")"),
        _ => print_log_tail(&log_path(), 20),
    }
    0
}

fn print_log_tail(path: &Path, lines: usize) {
    let Ok(text) = std::fs::read_to_string(path) else {
        println!("no log at {}", path.display());
        return;
    };
    println!("--- {} (last {lines} lines) ---", path.display());
    let all: Vec<&str> = text.lines().collect();
    for line in all.iter().skip(all.len().saturating_sub(lines)) {
        println!("{line}");
    }
}
