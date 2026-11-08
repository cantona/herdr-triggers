//! herdr-triggers: regex-driven terminal triggers as a resident herdr plugin.
//!
//! herdr runs plugin hook commands and `wait()`s on them, with 32 in flight at
//! once, so `start` must detach and return immediately - a daemon holding a
//! hook slot forever would starve every other plugin.

mod actions;
mod client;
mod config;
mod core;
mod engine;
mod log;
mod rules;
mod server;

use std::path::{Path, PathBuf};
use std::sync::atomic::Ordering;

use client::Client;

const PLUGIN_ID: &str = "cantona.herdr-triggers";
const USAGE: &str = "\
usage: herdr-triggersd [--host herdr|core] [--follow-app] <command> [--session <name> | --socket <path> | --all]

A daemon runs per server. --host picks the kind of server (default herdr):
  herdr   a herdr TUI server, as the plugin uses it
  core    herdr-core, the Herdr desktop app's terminals
--follow-app: a core daemon this starts exits once the Herdr app has been
closed for 10 s (the app's mod passes it).

With no target, the command acts on the server the calling shell belongs to -
which is whichever pane you happen to be in, so be explicit when it matters:

  --session <name>   the named session ('default' = the default server; a
                     herdr session actually named 'default' needs --socket)
  --socket <path>    a server's socket directly
  --all              every server of that host found on this machine

  start          detach and run the trigger daemon (idempotent)
  restart        stop a running daemon, then start a fresh one
  run            run the daemon in the foreground
  stop           stop a running daemon
  status [--json] report daemon state, rule count and recent log lines
  reload         re-read triggers.toml
  reset          re-arm every `once` rule
";

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Host {
    Herdr,
    Core,
}

impl Host {
    fn name(self) -> &'static str {
        match self {
            Host::Herdr => "herdr",
            Host::Core => "core",
        }
    }
}

static HOST: std::sync::OnceLock<Host> = std::sync::OnceLock::new();
/// `--follow-app`: a core daemon started by the app's mod leaves with it.
static FOLLOW_APP: std::sync::atomic::AtomicBool = std::sync::atomic::AtomicBool::new(false);

fn host() -> Host {
    HOST.get().copied().unwrap_or(Host::Herdr)
}

/// `--host herdr|core` or `--host=core` ahead of the command. Default herdr,
/// so every existing plugin invocation keeps meaning what it meant.
fn take_host(args: &mut Vec<String>) -> Result<Host, String> {
    let (name, consumed) = match args.first().map(String::as_str) {
        Some("--host") => (
            args.get(1)
                .cloned()
                .ok_or_else(|| "--host needs a name: herdr or core".to_string())?,
            2,
        ),
        Some(flag) if flag.starts_with("--host=") => (flag["--host=".len()..].to_string(), 1),
        _ => return Ok(Host::Herdr),
    };
    args.drain(..consumed);
    match name.as_str() {
        "herdr" => Ok(Host::Herdr),
        "core" => Ok(Host::Core),
        other => Err(format!("unknown host {other:?}: use herdr or core")),
    }
}

fn main() {
    let mut args: Vec<String> = std::env::args().skip(1).collect();
    let host = match take_host(&mut args) {
        Ok(host) => host,
        Err(message) => {
            eprintln!("{message}\n\n{USAGE}");
            std::process::exit(2);
        }
    };
    let _ = HOST.set(host);
    if args.first().is_some_and(|arg| arg == "--follow-app") {
        // Only a core daemon watches the app; on herdr the flag would be
        // recorded and reported without anything behind it.
        if host != Host::Core {
            eprintln!("--follow-app needs --host core\n\n{USAGE}");
            std::process::exit(2);
        }
        args.remove(0);
        FOLLOW_APP.store(true, Ordering::Relaxed);
    }
    let command = args.first().cloned().unwrap_or_default();
    // `status --json` is the one command with a flag of its own; it comes
    // before the target so the target grammar stays one flag.
    let json = command == "status" && args.get(1).is_some_and(|flag| flag == "--json");
    let target_from = if json { 2 } else { 1 };

    // Resolve the target before anything reads the socket or state paths.
    match parse_target(args.get(target_from..).unwrap_or(&[])) {
        Ok(Target::Inherit) => {}
        Ok(Target::Socket(path)) => match host {
            Host::Herdr => client::set_socket_override(path),
            Host::Core => core::paths::set_socket_override(path),
        },
        Ok(Target::Session(name)) => {
            if let Err(message) = select_session(host, &name) {
                eprintln!("{message}\n\n{USAGE}");
                std::process::exit(2);
            }
        }
        Ok(Target::All) => std::process::exit(run_for_every_server(host, &command, json)),
        Err(message) => {
            eprintln!("{message}\n\n{USAGE}");
            std::process::exit(2);
        }
    }
    let code = match command.as_str() {
        "start" => start(),
        "restart" => restart(),
        "run" => run_foreground(),
        "stop" => stop(),
        "reload" => signal_daemon(libc::SIGHUP, "reload"),
        "reset" => signal_daemon(libc::SIGUSR1, "reset"),
        "status" if json => status_json(),
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
    Socket(PathBuf),
    Session(String),
    All,
}

/// Reads the single optional target flag. One flag is the whole grammar, so
/// this is a match rather than a loop. Session names are resolved per host
/// afterwards, because the two hosts keep their sockets in different places.
fn parse_target(rest: &[String]) -> Result<Target, String> {
    let Some(flag) = rest.first() else {
        return Ok(Target::Inherit);
    };
    let value = rest.get(1);
    if !matches!(flag.as_str(), "--all" | "--socket" | "--session") {
        return Err(format!("unknown option {flag:?}"));
    }
    let consumed = if flag == "--all" { 1 } else { 2 };
    if rest.len() > consumed {
        return Err(format!("unexpected argument {:?}", rest[consumed]));
    }
    match flag.as_str() {
        "--all" => Ok(Target::All),
        "--socket" => value
            .map(|path| Target::Socket(PathBuf::from(path)))
            .ok_or_else(|| "--socket needs a path".to_string()),
        "--session" => value
            .map(|name| Target::Session(name.clone()))
            .ok_or_else(|| "--session needs a name".to_string()),
        _ => unreachable!("unknown flag {flag:?} passed the guard above"),
    }
}

/// Points this process at one named session of the host, refusing a name
/// that has no live socket: silently falling back to the default server
/// would act on the wrong panes.
fn select_session(host: Host, name: &str) -> Result<(), String> {
    match host {
        Host::Herdr => {
            let sock = if name == "default" {
                herdr_config_dir().join("herdr.sock")
            } else {
                herdr_config_dir()
                    .join("sessions")
                    .join(name)
                    .join("herdr.sock")
            };
            if !sock.exists() {
                return Err(format!(
                    "no herdr server for session {name:?} ({})",
                    sock.display()
                ));
            }
            client::set_socket_override(sock);
        }
        Host::Core => {
            core::paths::set_session_override(name.to_string());
            let sock = core::paths::socket_path()?;
            if !sock.exists() {
                return Err(format!(
                    "no herdr-core for session {name:?} ({})",
                    sock.display()
                ));
            }
        }
    }
    Ok(())
}

/// Runs the command once per server, in a child so each gets its own resolved
/// socket and state dir. Returns the worst exit code.
fn run_for_every_server(host: Host, command: &str, json: bool) -> i32 {
    if command == "run" {
        eprintln!("run cannot be used with --all; it stays in the foreground");
        return 2;
    }
    let servers = match host {
        Host::Herdr => client::discover_servers(),
        Host::Core => match core::paths::discover_sessions() {
            Ok(servers) => servers,
            Err(message) => {
                eprintln!("{message}");
                return 1;
            }
        },
    };
    if servers.is_empty() {
        eprintln!("no {} server found", host.name());
        return 1;
    }
    let Ok(exe) = std::env::current_exe() else {
        eprintln!("cannot locate own binary");
        return 1;
    };
    let mut worst = 0;
    for (name, socket) in servers {
        // With --json each server's object is one line of output (JSON
        // Lines); a header would make it unparsable.
        if !json {
            println!("== {name} ==");
        }
        let mut child = std::process::Command::new(&exe);
        child.arg("--host").arg(host.name());
        if FOLLOW_APP.load(Ordering::Relaxed) {
            child.arg("--follow-app");
        }
        child.arg(command);
        if json {
            child.arg("--json");
        }
        let status = child.arg("--socket").arg(&socket).status();
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
    match host() {
        Host::Herdr => env_dir("HERDR_PLUGIN_CONFIG_DIR").unwrap_or_else(|| {
            herdr_config_dir()
                .join("plugins")
                .join("config")
                .join(PLUGIN_ID)
        }),
        Host::Core => core::paths::config_dir(),
    }
}

/// State lives per SERVER, not per plugin (see `session_tag`).
fn state_dir() -> PathBuf {
    match host() {
        Host::Herdr => {
            let base = env_dir("HERDR_PLUGIN_STATE_DIR")
                .unwrap_or_else(|| herdr_state_dir().join("plugins").join(PLUGIN_ID));
            match session_tag() {
                Some(tag) => base.join(tag),
                None => base,
            }
        }
        Host::Core => core::paths::state_dir(),
    }
}

/// The server this process resolved to, as `status` reports it.
fn session_label() -> String {
    match host() {
        Host::Herdr => session_tag().unwrap_or_else(|| "default".to_string()),
        Host::Core => core::paths::server_tag(),
    }
}

/// The socket this process resolved to, as text; an unresolvable core
/// socket is reported, not hidden.
fn socket_label() -> String {
    match host() {
        Host::Herdr => client::socket_path().display().to_string(),
        Host::Core => match core::paths::socket_path() {
            Ok(path) => path.display().to_string(),
            Err(message) => format!("unresolved: {message}"),
        },
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
        // A daemon told to stop still holds the lock until it exits. A start
        // right behind the stop (the app's sidebar block closing in one
        // layout and opening in the next) must wait for it, not report it as
        // running and leave nothing behind once it is gone.
        if !is_stopping(pid) {
            // herdr re-runs startup hooks; a second daemon would double every fire.
            already_running(pid);
            return 0;
        }
        match wait_for_exit(pid) {
            Exit::Gone => {}
            // Another start won the lock meanwhile.
            Exit::Replaced(now) => {
                already_running(now);
                return 0;
            }
            Exit::Stuck => {
                eprintln!(
                    "stopping daemon (pid {pid}) still holds the lock after {}s; not starting",
                    STOP_WAIT.as_secs()
                );
                return 1;
            }
        }
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

/// Reports a running daemon, and when the caller wanted one that follows the
/// app, that this one does not.
fn already_running(pid: i32) {
    println!("herdr-triggersd already running (pid {pid})");
    if FOLLOW_APP.load(Ordering::Relaxed) && !follows_app() {
        println!("it was started without --follow-app and will outlive the app; restart it with --follow-app to change that");
    }
}

/// Stops the daemon and returns once it has exited, so a status read right
/// after says "stopped".
fn stop() -> i32 {
    let Some(pid) = running_pid() else {
        eprintln!("herdr-triggersd is not running");
        return 1;
    };
    let code = signal_daemon(libc::SIGTERM, "stop");
    if code != 0 {
        return code;
    }
    match wait_for_exit(pid) {
        Exit::Gone => 0,
        Exit::Replaced(now) => {
            eprintln!(
                "herdr-triggersd (pid {pid}) stopped, but a start launched pid {now} meanwhile"
            );
            1
        }
        Exit::Stuck => {
            eprintln!(
                "herdr-triggersd (pid {pid}) is still stopping after {}s",
                STOP_WAIT.as_secs()
            );
            1
        }
    }
}

enum Exit {
    Gone,
    /// Another daemon holds the lock now.
    Replaced(i32),
    Stuck,
}

/// Waits for daemon `pid` to let go of the lock, and notices a racing
/// `start` that took it the moment it was free rather than run out the whole
/// limit. Gone means the lock is free: the daemon removes its pid file
/// before it exits, and a new one takes the lock before writing its own, so
/// a held lock without another pid is still waited on.
fn wait_for_exit(pid: i32) -> Exit {
    let deadline = std::time::Instant::now() + STOP_WAIT;
    loop {
        if lock_is_free() {
            return Exit::Gone;
        }
        if let Some(now) = recorded_pid().filter(|now| *now != pid) {
            return Exit::Replaced(now);
        }
        if std::time::Instant::now() >= deadline {
            return Exit::Stuck;
        }
        std::thread::sleep(std::time::Duration::from_millis(20));
    }
}

/// Stops a running daemon and starts a fresh one.
///
/// Waits for the old daemon to actually release its lock rather than sleeping a
/// guessed interval: `start` refuses while the lock is held, so returning too
/// early would leave nothing running at all.
fn restart() -> i32 {
    if let Some(pid) = running_pid() {
        let code = signal_daemon(libc::SIGTERM, "stop");
        if code != 0 {
            return code;
        }
        match wait_for_exit(pid) {
            Exit::Gone => {}
            // A racing start already put a fresh daemon there.
            Exit::Replaced(now) => {
                already_running(now);
                return 0;
            }
            Exit::Stuck => {
                eprintln!(
                    "old daemon still holds the lock after {}s; not starting a new one",
                    STOP_WAIT.as_secs()
                );
                return 1;
            }
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
    let _ = std::fs::remove_file(stopping_path());
    // Whether this daemon leaves with the app, for `status` and for a start
    // from the app that finds it already running.
    let _ = std::fs::write(
        mode_path(),
        if FOLLOW_APP.load(Ordering::Relaxed) {
            "follow-app\n"
        } else {
            "standalone\n"
        },
    );
    install_signal_handlers();

    let server: std::sync::Arc<dyn server::Server> = match host() {
        Host::Herdr => std::sync::Arc::new(Client::from_env()),
        Host::Core => match core::paths::socket_path() {
            Ok(socket) => {
                let server = core::CoreServer::new(socket);
                if FOLLOW_APP.load(Ordering::Relaxed) {
                    std::sync::Arc::new(server.following_app(core::paths::app_socket()))
                } else {
                    std::sync::Arc::new(server)
                }
            }
            Err(message) => {
                log_line!("cannot resolve the herdr-core socket: {message}");
                return;
            }
        },
    };
    log_line!(
        "herdr-triggersd {} starting (socket {})",
        env!("CARGO_PKG_VERSION"),
        server.describe()
    );
    let mut engine = engine::Engine::new(server, &config_dir(), &state_dir);
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
    recorded_pid()
}

/// The pid the daemon wrote, whether or not it still holds the lock.
fn recorded_pid() -> Option<i32> {
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

fn mode_path() -> PathBuf {
    state_dir().join("triggersd.mode")
}

/// Whether the running daemon was started with `--follow-app`.
fn follows_app() -> bool {
    std::fs::read_to_string(mode_path()).is_ok_and(|mode| mode.trim() == "follow-app")
}

fn stopping_path() -> PathBuf {
    state_dir().join("triggersd.stopping")
}

/// How long `stop`, `start` and `restart` wait for a stopping daemon to
/// exit. A stop lands between rules, so it can sit behind one pane read and
/// one action: on herdr-core a reconnect and read of up to 8s (connect,
/// hello, subscribe and read, 2s each) plus a helper allowed 5s, and before
/// the read a coprocess's typing may hold the connection for up to 8s more
/// (a reconnect, then the attach).
const STOP_WAIT: std::time::Duration = std::time::Duration::from_secs(30);

/// Whether daemon `pid` has been sent a stop it has not finished acting on.
fn is_stopping(pid: i32) -> bool {
    std::fs::read_to_string(stopping_path()).is_ok_and(|text| text.trim().parse::<i32>() == Ok(pid))
}

fn signal_daemon(signal: libc::c_int, label: &str) -> i32 {
    let Some(pid) = running_pid() else {
        eprintln!("herdr-triggersd is not running");
        return 1;
    };
    if signal == libc::SIGTERM {
        // Lets a start that follows tell a dying daemon from a running one.
        let _ = std::fs::write(stopping_path(), format!("{pid}\n"));
    }
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
    println!("{} socket: {}", host().name(), socket_label());
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

/// One JSON object for the mod dialog: the same facts `status` prints, plus
/// the last fire parsed out of the log so the dialog need not.
fn status_json() -> i32 {
    let triggers = config_dir().join("triggers.toml");
    let (rules, config_error) = match config::load_triggers(&triggers) {
        Ok(loaded) => (Some(loaded.rules.len()), None),
        Err(err) => (None, Some(err.to_string())),
    };
    let log = log_tail(&log_path(), 20).unwrap_or_default();
    let pid = running_pid();
    let out = serde_json::json!({
        "host": host().name(),
        "session": session_label(),
        "socket": socket_label(),
        "state_dir": state_dir().display().to_string(),
        "running": pid.is_some(),
        "follows_app": pid.is_some() && follows_app(),
        "pid": pid,
        "config": triggers.display().to_string(),
        "rules": rules,
        "config_error": config_error,
        "last_fire": last_fire(&log),
        "log": log,
    });
    println!("{out}");
    0
}

/// The last `lines` lines of the log, or `None` when it cannot be read.
fn log_tail(path: &Path, lines: usize) -> Option<Vec<String>> {
    let text = std::fs::read_to_string(path).ok()?;
    let all: Vec<&str> = text.lines().collect();
    Some(
        all.iter()
            .skip(all.len().saturating_sub(lines))
            .map(|line| line.to_string())
            .collect(),
    )
}

/// `[1794212049.221] rule ssh-auth started in t13: send_text` is what the
/// engine logs on a fire; the newest such line, if any, as rule, pane and time.
fn last_fire(lines: &[String]) -> Option<serde_json::Value> {
    lines.iter().rev().find_map(|line| {
        let (stamp, rest) = line.split_once("] ")?;
        let (secs, millis) = stamp.strip_prefix('[')?.split_once('.')?;
        let at_ms = secs
            .parse::<u64>()
            .ok()?
            .checked_mul(1000)?
            .checked_add(millis.parse::<u64>().ok()?)?;
        let rest = rest.strip_prefix("rule ")?;
        let (rule, rest) = rest.split_once(" started in ")?;
        // herdr pane ids contain a colon (`w1:p2`); the summary follows ": ".
        let (pane, _) = rest.split_once(": ")?;
        Some(serde_json::json!({"rule": rule, "pane": pane, "at_ms": at_ms}))
    })
}

fn print_log_tail(path: &Path, lines: usize) {
    let Some(tail) = log_tail(path, lines) else {
        println!("no log at {}", path.display());
        return;
    };
    println!("--- {} (last {lines} lines) ---", path.display());
    for line in tail {
        println!("{line}");
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn args(list: &[&str]) -> Vec<String> {
        list.iter().map(|s| s.to_string()).collect()
    }

    #[test]
    fn the_host_flag_is_optional_and_comes_first() {
        let mut plain = args(&["status"]);
        assert_eq!(take_host(&mut plain).unwrap(), Host::Herdr);
        assert_eq!(plain, args(&["status"]));

        let mut spaced = args(&["--host", "core", "start"]);
        assert_eq!(take_host(&mut spaced).unwrap(), Host::Core);
        assert_eq!(spaced, args(&["start"]));

        let mut joined = args(&["--host=herdr", "stop", "--all"]);
        assert_eq!(take_host(&mut joined).unwrap(), Host::Herdr);
        assert_eq!(joined, args(&["stop", "--all"]));

        assert!(take_host(&mut args(&["--host", "tui"])).is_err());
        assert!(take_host(&mut args(&["--host"])).is_err());
    }

    #[test]
    fn the_target_grammar_is_one_flag() {
        assert!(matches!(parse_target(&[]).unwrap(), Target::Inherit));
        assert!(matches!(
            parse_target(&args(&["--all"])).unwrap(),
            Target::All
        ));
        assert!(
            matches!(parse_target(&args(&["--session", "com"])).unwrap(), Target::Session(name) if name == "com")
        );
        assert!(
            matches!(parse_target(&args(&["--socket", "/s"])).unwrap(), Target::Socket(path) if path == Path::new("/s"))
        );
        assert!(parse_target(&args(&["--sesion", "com"])).is_err());
        assert!(parse_target(&args(&["--all", "extra"])).is_err());
        assert!(parse_target(&args(&["--session"])).is_err());
    }

    #[test]
    fn the_last_fire_is_parsed_from_the_newest_started_line() {
        let lines = vec![
            "[1794212049.221] rule ssh-auth started in t13: send_text".to_string(),
            "[1794212050.000] rule ssh-auth not fired in t13: cooldown".to_string(),
            "[1794212051.500] rule login started in w1:p2: coprocess answer.sh".to_string(),
            "[1794212052.000] watching 2 rule/pane pairs across 2 panes every 200ms".to_string(),
        ];
        let fire = last_fire(&lines).unwrap();
        assert_eq!(fire["rule"], "login");
        assert_eq!(fire["pane"], "w1:p2");
        assert_eq!(fire["at_ms"], 1794212051500u64);
        assert!(last_fire(&[]).is_none());
        assert!(last_fire(&["garbage".to_string()]).is_none());
    }
}
