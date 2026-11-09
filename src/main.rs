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
mod install;
mod log;
mod rules;
mod server;

use std::path::{Path, PathBuf};
use std::sync::atomic::Ordering;

use client::Client;
use install::service::Installed;

const PLUGIN_ID: &str = "cantona.herdr-triggers";
const USAGE: &str = "\
usage: herdr-triggersd [--host herdr|core] [--follow-app] <command> [--session <name> | --socket <path> | --all]
       herdr-triggersd install|uninstall [--yes | --dry-run]

A daemon runs per server. --host picks the kind of server (default herdr):
  herdr   a herdr TUI server, as the plugin uses it
  core    herdr-core, the Herdr desktop app's terminals
--follow-app: a core daemon this starts exits once the Herdr app has been
closed for 10 s (kept for 0.2.0 mods and scripts; nothing in 0.2.1 uses it).

install sets up the Herdr desktop app: the mod, its keys.json bindings, the
~/.config/herdr-triggers link to the TUI plugin's config, and a login service
(systemd user unit or launchd agent) running `--host core service`. It prints
the plan and asks; --yes applies without asking, --dry-run only prints.
Without a terminal and without --yes it exits 2. uninstall reverses it.

With no target, the command acts on the server the calling shell belongs to -
which is whichever pane you happen to be in, so be explicit when it matters:

  --session <name>   the named session ('default' = the default server; a
                     herdr session actually named 'default' needs --socket)
  --socket <path>    a server's socket directly
  --all              every server of that host found on this machine

  start          detach and run the trigger daemon (idempotent); aimed at
                 the default core with the login service installed, starts
                 the service instead
  restart        stop a running daemon, then start a fresh one
  run            run the daemon in the foreground
  service        (--host core) foreground daemon for a service manager: waits
                 for herdr-core, attaches, and waits again when it leaves; if
                 another daemon holds the lock, waits for that one to exit
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
/// `--follow-app`: a core daemon started by a 0.2.0 mod leaves with the app.
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
    let explicit_host = args
        .first()
        .is_some_and(|arg| arg == "--host" || arg.starts_with("--host="));
    let host = match take_host(&mut args) {
        Ok(host) => host,
        Err(message) => {
            eprintln!("{message}\n\n{USAGE}");
            std::process::exit(2);
        }
    };
    let follow_app = args.first().is_some_and(|arg| arg == "--follow-app");
    if follow_app {
        args.remove(0);
    }
    let command = args.first().cloned().unwrap_or_default();
    if command == "install" || command == "uninstall" {
        if (explicit_host && host != Host::Core) || follow_app {
            eprintln!(
                "{command} sets up the Herdr desktop app (--host core) and takes no other flags; \
                 the TUI plugin installs with `herdr plugin install`\n\n{USAGE}"
            );
            std::process::exit(2);
        }
        let _ = HOST.set(Host::Core);
        let direction = if command == "install" {
            install::Direction::Install
        } else {
            install::Direction::Uninstall
        };
        std::process::exit(install_main(direction, &args[1..]));
    }
    let _ = HOST.set(host);
    if follow_app {
        // Only a core daemon watches the app; on herdr the flag would be
        // recorded and reported without anything behind it.
        if host != Host::Core {
            eprintln!("--follow-app needs --host core\n\n{USAGE}");
            std::process::exit(2);
        }
        FOLLOW_APP.store(true, Ordering::Relaxed);
    }
    if command == "service" && (host != Host::Core || follow_app) {
        eprintln!("service needs --host core and runs without --follow-app\n\n{USAGE}");
        std::process::exit(2);
    }
    if command == "service" {
        core::paths::set_service_mode();
    }
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
        "service" => service_main(),
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
    if command == "run" || command == "service" {
        eprintln!("{command} cannot be used with --all; it stays in the foreground");
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
        Host::Herdr => tui_config_dir(),
        Host::Core => core::paths::config_dir(),
    }
}

/// The TUI plugin's config directory, which `install` links the core
/// host's config to.
fn tui_config_dir() -> PathBuf {
    env_dir("HERDR_PLUGIN_CONFIG_DIR").unwrap_or_else(|| {
        herdr_config_dir()
            .join("plugins")
            .join("config")
            .join(PLUGIN_ID)
    })
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
    xdg_dir("XDG_CONFIG_HOME")
        .unwrap_or_else(|| home().join(".config"))
        .join("herdr")
}

fn herdr_state_dir() -> PathBuf {
    xdg_dir("XDG_STATE_HOME")
        .unwrap_or_else(|| home().join(".local").join("state"))
        .join("herdr")
}

/// An XDG base directory variable; a relative value is ignored, as the XDG
/// spec says, so nothing resolves against the working directory.
fn xdg_dir(name: &str) -> Option<PathBuf> {
    env_dir(name).filter(|dir| dir.is_absolute())
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
    match supervising_service() {
        Some(Installed::Unreachable { why, .. }) => return refuse_unreachable(None, &why),
        Some(Installed::Reachable(service)) => match start_service(&service) {
            Ok(()) => return 0,
            // No user bus in this session, a unit the manager cannot find:
            // triggers still run, just not supervised.
            Err(message) => {
                eprintln!("{message}");
                eprintln!("starting a detached daemon instead");
            }
        },
        None => {}
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
    if FOLLOW_APP.load(Ordering::Relaxed) && !follows_app() && !is_service_daemon() {
        println!("it was started without --follow-app and will outlive the app; restart it with --follow-app to change that");
    }
}

/// Stops the daemon and returns once it has exited, so a status read right
/// after says "stopped".
fn stop() -> i32 {
    match supervising_service() {
        Some(Installed::Reachable(service)) => return stop_through_service(&service, false),
        Some(Installed::Unreachable { why, .. }) => return refuse_unreachable(running_pid(), &why),
        None => {}
    }
    let Some(pid) = running_pid() else {
        eprintln!("herdr-triggersd is not running");
        return 1;
    };
    signal_and_wait(pid)
}

/// SIGTERM to daemon `pid`, then waits for it to exit.
fn signal_and_wait(pid: i32) -> i32 {
    let code = signal_daemon(libc::SIGTERM, "stop");
    if code != 0 {
        return code;
    }
    report_exit(pid)
}

/// `stop` aimed at the service's core with the service installed. The
/// manager stops the service, since it would start again a daemon that a
/// signal ended, and Stop is meant to hold until Start. A daemon the
/// service did not start is signalled after that, when no service waits to
/// take its place; the manager cannot restart it, so that also happens when
/// the manager fails. `restarting` drops the "not running" note and reports
/// a manager failure as a warning, since the start that follows may still
/// succeed.
fn stop_through_service(service: &install::service::Location, restarting: bool) -> i32 {
    let pid = running_pid();
    let service_daemon = pid.is_some() && is_service_daemon();
    let name = service.manager.name();
    let managed = match install::service::stop_installed(
        service.manager,
        install::service::uid(),
        install::service::probe_cmd,
    ) {
        Some(cmd) => install::service::run_cmd(&cmd).map(|()| {
            println!("stopped the {name} service ({cmd})");
        }),
        None => Ok(()),
    };
    if let Err(message) = &managed {
        if restarting {
            eprintln!("warning: {message}");
        } else {
            eprintln!("{message}");
        }
    }
    let code = match pid {
        None => {
            if !restarting && managed.is_ok() {
                println!("herdr-triggersd is not running; the {name} service stays stopped");
            }
            0
        }
        Some(pid) if service_daemon && managed.is_err() => {
            eprintln!("not signalling pid {pid}: the {name} service would start it again");
            1
        }
        Some(pid) if service_daemon => report_exit(pid),
        Some(pid) => signal_and_wait(pid),
    };
    if managed.is_err() {
        1
    } else {
        code
    }
}

/// Waits for daemon `pid` to exit after a stop and says how it went.
fn report_exit(pid: i32) -> i32 {
    match wait_for_exit(pid) {
        Exit::Gone => 0,
        // The service was waiting behind this daemon; that is the outcome
        // a stop of a stray daemon is meant to have.
        Exit::Replaced(now) if is_service_daemon() => {
            println!(
                "herdr-triggersd (pid {pid}) stopped; the login service took over (pid {now})"
            );
            0
        }
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
    match supervising_service() {
        Some(Installed::Unreachable { why, .. }) => {
            return refuse_unreachable(running_pid(), &why);
        }
        Some(Installed::Reachable(service)) => {
            let code = stop_through_service(&service, true);
            // A manager that refused the stop has already been reported.
            // With nothing left running the start still goes ahead: it
            // delegates, or falls back to a detached daemon, rather than
            // leave triggers off.
            if code != 0 && running_pid().is_some() {
                return code;
            }
            return start();
        }
        None => {}
    }
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

/// start, stop or restart aimed at a service this session cannot reach:
/// nothing is signalled or started. A signal would be undone 5s later
/// (Restart=always), and a detached daemon would hold the lock against the
/// service the manager restarts.
fn refuse_unreachable(pid: Option<i32>, why: &str) -> i32 {
    if let Some(pid) = pid.filter(|_| is_service_daemon()) {
        eprintln!("not signalling pid {pid}: the systemd service would start it again");
    }
    eprintln!("{}", install::service::unreachable_message(why));
    1
}

/// The login service that would run this daemon, when it is installed and
/// this command resolved the core it watches. Flags are not the test: a
/// shell in core session "work" names it only through HC_SESSION or
/// HC_SOCKET, while `--all` reaches the default core with `--socket`.
fn supervising_service() -> Option<Installed> {
    if host() != Host::Core || service_watches_this_core() != Some(true) {
        return None;
    }
    install::service::installed(&home(), &mut DaemonSystem)
}

/// Whether this command resolved the core the login service watches.
/// `None` when either socket cannot be resolved (a broken core config):
/// then nobody can tell, and nothing delegates.
fn service_watches_this_core() -> Option<bool> {
    match (core::paths::socket_path(), core::paths::service_socket()) {
        (Ok(ours), Ok(watched)) => Some(core::paths::same_socket(&ours, &watched)),
        _ => None,
    }
}

/// `start` with the service installed: the manager starts it, so the daemon
/// stays supervised instead of detaching beside it.
fn start_service(service: &install::service::Location) -> Result<(), String> {
    let cmd = install::service::start_installed(
        service.manager,
        install::service::uid(),
        &service.path,
        install::service::probe_cmd,
    );
    install::service::run_cmd(&cmd)?;
    println!("started the {} service ({cmd})", service.manager.name());
    Ok(())
}

fn install_main(direction: install::Direction, args: &[String]) -> i32 {
    use std::io::IsTerminal;
    use std::os::unix::fs::MetadataExt;
    let mode = match install::parse_mode(args) {
        Ok(mode) => mode,
        Err(message) => {
            eprintln!("{message}\n\n{USAGE}");
            return 2;
        }
    };
    // Before anything resolves or creates a path.
    let home_owner = std::fs::metadata(home()).ok().map(|m| m.uid());
    // SAFETY: geteuid has no preconditions and cannot fail.
    if let Err(message) = install::check_user(unsafe { libc::geteuid() }, home_owner) {
        eprintln!("error: {message}");
        return 1;
    }
    // The daemon install looks for, stops and starts is the service's, the
    // default core's, whichever core terminal install runs in.
    // A broken core config must not block removing the mod, keys or
    // service, none of which depend on it; the daemon checks then use the
    // default session's state directory.
    match core::paths::service_socket() {
        Ok(socket) => core::paths::set_socket_override(socket),
        Err(message) => {
            eprintln!("warning: {message}; checking the default session's daemon");
            core::paths::set_session_override(core::paths::DEFAULT_SESSION.to_string());
        }
    }
    let ctx = match install::Context::from_env(tui_config_dir()) {
        Ok(ctx) => ctx,
        Err(message) => {
            eprintln!("error: {message}");
            return 1;
        }
    };
    let mut sys = DaemonSystem;
    let plan = match install::plan(&ctx, direction, &mut sys) {
        Ok(plan) => plan,
        Err(message) => {
            eprintln!("error: {message}\nnothing was changed");
            return 1;
        }
    };
    let stdin = std::io::stdin();
    let tty = stdin.is_terminal() && std::io::stderr().is_terminal();
    install::execute(
        &plan,
        direction,
        mode,
        install::Terminal {
            tty,
            input: &mut stdin.lock(),
            out: &mut std::io::stdout(),
            err: &mut std::io::stderr(),
        },
        &mut sys,
    )
}

/// The real service manager, and this machine's daemon for the core the
/// service watches (`install_main` points the paths there).
struct DaemonSystem;

impl install::service::System for DaemonSystem {
    fn run(&mut self, cmd: &install::service::Cmd) -> Result<(), String> {
        println!("running {cmd}");
        install::service::run_cmd(cmd)
    }

    fn succeeds(&mut self, cmd: &install::service::Cmd) -> bool {
        install::service::probe_cmd(cmd)
    }

    fn output(&mut self, cmd: &install::service::Cmd) -> Result<String, String> {
        install::service::output_cmd(cmd)
    }

    fn systemd_booted(&mut self) -> bool {
        install::service::systemd_booted()
    }

    fn foreign_daemon(&mut self) -> Option<i32> {
        running_pid().filter(|_| !is_service_daemon())
    }

    fn service_daemon_running(&mut self) -> bool {
        running_pid().is_some() && is_service_daemon()
    }

    /// Done once `pid` lets go of the lock, even if another daemon took it
    /// then: a service waiting behind `pid` does exactly that.
    fn stop_daemon(&mut self, pid: i32) -> Result<(), String> {
        match running_pid() {
            Some(now) if now == pid => {}
            _ => return Ok(()),
        }
        if signal_daemon(libc::SIGTERM, "stop") != 0 {
            return Err(format!("could not stop herdr-triggersd (pid {pid})"));
        }
        match wait_for_exit(pid) {
            Exit::Gone => Ok(()),
            Exit::Replaced(_) if is_service_daemon() => Ok(()),
            Exit::Replaced(now) => Err(format!(
                "herdr-triggersd (pid {pid}) stopped, but another daemon (pid {now}) took its place"
            )),
            Exit::Stuck => Err(format!(
                "herdr-triggersd (pid {pid}) still holds the lock after {}s",
                STOP_WAIT.as_secs()
            )),
        }
    }
}

/// `--host core service`: waits for herdr-core, runs the engine while it is
/// there, and waits again when it leaves. When another daemon holds the lock
/// it waits for that one to exit rather than exit 1: the manager would
/// restart it every few seconds for as long as the other daemon lives, and
/// each attempt's log line would rotate the useful log away. It always
/// watches the default session (see `set_service_mode`), and exits 1 when
/// core's config moves that session's socket to another state dir, so the
/// manager restarts it there.
fn service_main() -> i32 {
    let state_dir = match daemon_prologue("service", true) {
        Prologue::Ready(state_dir) => state_dir,
        Prologue::Stopped => return 0,
        Prologue::Held => return 1,
    };
    log_line!(
        "herdr-triggersd {} service starting",
        env!("CARGO_PKG_VERSION")
    );
    let config = config_dir();
    // The tag whose state dir holds this process's lock and pid.
    let held = core::paths::server_tag();
    let served = core::service::serve(
        &engine::STOP,
        &waiting_path(),
        core::service::WAIT_EVERY,
        core::paths::socket_path,
        |socket| core::paths::tag_of(socket).as_deref() == Some(held.as_str()),
        |server| {
            let mut engine = engine::Engine::new(std::sync::Arc::new(server), &config, &state_dir);
            engine.run();
        },
    );
    // The dir the lock is in: after a move, state_dir() resolves to the new
    // core's, whose pidfile may belong to another daemon.
    let _ = std::fs::remove_file(state_dir.join("triggersd.pid"));
    match served {
        core::service::Served::Stopped => {
            log_line!("service stopping");
            0
        }
        // A failure exit, so the service manager starts it again, now in the
        // new dir, and its status shows a failure rather than a clean stop.
        core::service::Served::Moved(socket) => {
            log_line!(
                "herdr-core's socket moved to {}; restarting to follow it",
                socket.display()
            );
            1
        }
    }
}

enum Prologue {
    Ready(PathBuf),
    /// Another daemon holds the lock.
    Held,
    /// Stopped while waiting for the lock.
    Stopped,
}

/// How often a service waiting behind another daemon tries the lock.
const LOCK_RETRY: std::time::Duration = std::time::Duration::from_secs(2);

/// Opens the log, takes the singleton lock, and records the pid and `mode`.
/// With `wait`, a held lock is retried until it is free or a stop arrives;
/// nothing in the state dir is touched meanwhile, since it is the other
/// daemon's.
fn daemon_prologue(mode: &str, wait: bool) -> Prologue {
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
        if !wait {
            log_line!("another herdr-triggersd already holds the lock; exiting");
            return Prologue::Held;
        }
        // Before the wait, so a stop from the service manager ends it.
        install_signal_handlers();
        log_line!("another herdr-triggersd holds the lock; waiting for it");
        if !wait_for_lock(&state_dir) {
            return Prologue::Stopped;
        }
    }
    // How this daemon was started, for `status`, for a start from the app
    // that finds it already running, and for `install`, which stops any
    // daemon the service did not start. Written before the pidfile: a `stop`
    // that sees the new pid reads the mode to tell a service that took over.
    let _ = std::fs::write(mode_path(), format!("{mode}\n"));
    write_pidfile();
    let _ = std::fs::remove_file(stopping_path());
    // A killed service leaves its marker behind; this daemon is not waiting.
    let _ = std::fs::remove_file(waiting_path());
    install_signal_handlers();
    Prologue::Ready(state_dir)
}

/// Takes the lock once it is free; false when a stop arrives first.
fn wait_for_lock(state_dir: &Path) -> bool {
    loop {
        let mut left = LOCK_RETRY;
        while !left.is_zero() {
            if engine::STOP.load(Ordering::Relaxed) {
                return false;
            }
            let step = left.min(std::time::Duration::from_millis(50));
            std::thread::sleep(step);
            left -= step;
        }
        if engine::STOP.load(Ordering::Relaxed) {
            return false;
        }
        if acquire_singleton_lock(state_dir) {
            return true;
        }
    }
}

fn daemon_main() {
    let mode = if FOLLOW_APP.load(Ordering::Relaxed) {
        "follow-app"
    } else {
        "standalone"
    };
    let Prologue::Ready(state_dir) = daemon_prologue(mode, false) else {
        return;
    };

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
    // Not pid_path(): core's config may have moved the socket meanwhile, and
    // state_dir() would now name another core's dir.
    let _ = std::fs::remove_file(state_dir.join("triggersd.pid"));
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

/// Whether the running daemon is the login service's.
fn is_service_daemon() -> bool {
    std::fs::read_to_string(mode_path()).is_ok_and(|mode| mode.trim() == "service")
}

/// Present while a service daemon has no herdr-core to talk to.
fn waiting_path() -> PathBuf {
    state_dir().join("triggersd.waiting")
}

/// Whether the running daemon is a service waiting for herdr-core. The
/// marker alone could be left by a killed daemon.
fn is_waiting(pid: Option<i32>) -> bool {
    pid.is_some() && waiting_path().exists()
}

fn service_json() -> serde_json::Value {
    let installed = install::service::installed(&home(), &mut DaemonSystem);
    serde_json::json!({
        "installed": installed.is_some(),
        "manager": installed.as_ref().map(|i| i.manager().name()),
        // null when not installed; false when this session cannot reach it.
        "reachable": installed
            .as_ref()
            .map(|i| matches!(i, Installed::Reachable(_))),
        "this_core": service_watches_this_core(),
    })
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
    let pid = running_pid();
    match pid {
        Some(pid) => println!("running (pid {pid})"),
        None => println!("not running"),
    }
    if is_waiting(pid) {
        println!("waiting for herdr-core");
    }
    if host() == Host::Core {
        match install::service::installed(&home(), &mut DaemonSystem) {
            Some(installed) => {
                let mut line = format!("service: {} installed", installed.manager().name());
                if service_watches_this_core() == Some(false) {
                    line.push_str(" (watches the default core, not this one)");
                }
                if let Installed::Unreachable { why, .. } = &installed {
                    line.push_str(&format!(
                        " (manager not reachable from this session: {why})"
                    ));
                }
                println!("{line}");
            }
            None => println!("service: not installed (herdr-triggersd install)"),
        }
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
        "waiting": is_waiting(pid),
        // The service runs a core daemon; a TUI daemon has none.
        "service": (host() == Host::Core).then(service_json),
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
