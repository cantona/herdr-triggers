//! The login service: a systemd user unit on Linux, a launchd agent on
//! macOS. Both renderers compile everywhere so either can be tested on
//! either; only `platform_manager` is gated, and every systemctl or
//! launchctl call goes through `System` so tests can record instead.

use std::path::{Path, PathBuf};

pub const UNIT_NAME: &str = "herdr-triggers.service";
pub const LABEL: &str = "io.github.cantona.herdr-triggers";
const BIN_NAME: &str = "herdr-triggersd";

// Each platform constructs only its own variant outside tests.
#[allow(dead_code)]
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Manager {
    Systemd,
    Launchd,
}

impl Manager {
    pub fn name(self) -> &'static str {
        match self {
            Manager::Systemd => "systemd",
            Manager::Launchd => "launchd",
        }
    }
}

#[cfg(target_os = "linux")]
pub fn platform_manager() -> Option<Manager> {
    Some(Manager::Systemd)
}

#[cfg(target_os = "macos")]
pub fn platform_manager() -> Option<Manager> {
    Some(Manager::Launchd)
}

#[cfg(not(any(target_os = "linux", target_os = "macos")))]
pub fn platform_manager() -> Option<Manager> {
    None
}

/// Where launchd reads the agent from.
pub fn plist_path(home: &Path) -> PathBuf {
    home.join("Library")
        .join("LaunchAgents")
        .join(format!("{LABEL}.plist"))
}

/// Where a manager reads the service file from.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Location {
    pub manager: Manager,
    pub path: PathBuf,
    /// Set when the manager could not tell and the path is a guess.
    pub note: Option<String>,
}

/// The unit or plist path the manager actually reads. systemd builds its
/// unit search path from its own process environment, which neither the
/// shell's nor the block `show-environment` prints (imported variables land
/// there) has to match, so the manager is asked for the path itself. An
/// error says why the manager could not be asked, and whether that is
/// because systemd does not run here at all.
pub fn locate(manager: Manager, home: &Path, sys: &mut dyn System) -> Result<Location, Unmanaged> {
    if manager == Manager::Launchd {
        return Ok(Location {
            manager,
            path: plist_path(home),
            note: None,
        });
    }
    let listed = match sys
        .output(&unit_path_query())
        .and_then(|text| parse_unit_path(&text))
    {
        Ok(listed) => listed,
        Err(why) if sys.systemd_booted() => return Err(Unmanaged::Unreachable(why)),
        Err(why) => return Err(Unmanaged::NoSystemd(why)),
    };
    let (path, note) = match user_unit_dir(&listed, home) {
        Some(dir) => (dir.join(UNIT_NAME), None),
        None => (
            default_unit_path(home),
            Some(
                "systemd's UnitPath lists no user config folder; assuming ~/.config/systemd/user"
                    .to_string(),
            ),
        ),
    };
    Ok(Location {
        manager,
        path,
        note,
    })
}

/// Why the systemd user manager could not be asked.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Unmanaged {
    /// systemd does not run here (a container, WSL without it, a distro
    /// without systemd): there is no service.
    NoSystemd(String),
    /// systemd runs, but this session cannot reach the user manager (`su`,
    /// `sudo -iu`, cron, a shell that lost XDG_RUNTIME_DIR). A service it
    /// runs may well be there and running.
    Unreachable(String),
}

/// `/run/systemd/system` is a directory exactly when systemd booted the
/// machine (sd_booted). `HERDR_TRIGGERS_SYSTEMD_DIR` stands in for it in
/// tests.
pub fn systemd_booted() -> bool {
    std::env::var_os("HERDR_TRIGGERS_SYSTEMD_DIR")
        .filter(|dir| !dir.is_empty())
        .map(PathBuf::from)
        .unwrap_or_else(|| PathBuf::from("/run/systemd/system"))
        .is_dir()
}

/// The service as seen from this session.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Installed {
    /// The manager answers and reads the file at `Location`.
    Reachable(Location),
    /// A unit file is in `~/.config/systemd/user`, but the manager cannot be
    /// reached to start, stop or ask about it.
    Unreachable { path: PathBuf, why: String },
}

impl Installed {
    pub fn manager(&self) -> Manager {
        match self {
            Installed::Reachable(found) => found.manager,
            Installed::Unreachable { .. } => Manager::Systemd,
        }
    }
}

/// `~/.config/systemd/user/herdr-triggers.service`: where a unit goes when
/// systemd names no folder of its own, and where uninstall looks when no
/// manager answers.
pub fn default_unit_path(home: &Path) -> PathBuf {
    home.join(".config")
        .join("systemd")
        .join("user")
        .join(UNIT_NAME)
}

/// The folders `busctl --json=short` printed for the manager's `UnitPath`
/// property: an exact list, unlike `systemctl show`, which joins it with
/// spaces and cannot carry a folder that has one.
pub fn parse_unit_path(text: &str) -> Result<Vec<String>, String> {
    let value: serde_json::Value =
        serde_json::from_str(text.trim()).map_err(|e| format!("not JSON: {e}"))?;
    value["data"]
        .as_array()
        .ok_or("no data array")?
        .iter()
        .map(|entry| {
            entry
                .as_str()
                .map(str::to_string)
                .ok_or_else(|| "a folder that is not a string".to_string())
        })
        .collect()
}

/// The user's own unit folder in systemd's `UnitPath`. systemd lists the
/// persistent config folder first and puts its `<dir>.control` sibling
/// ahead of it, wherever the manager's XDG_CONFIG_HOME points, so the first
/// `.control` entry whose base is also listed names it. Failing that, the
/// first `.../systemd/user` entry under HOME outside the data folder
/// (`~/.local/share`, for units that packages install). The runtime folder
/// under /run has the same pair but is tmpfs, gone at reboot.
pub fn user_unit_dir(listed: &[String], home: &Path) -> Option<PathBuf> {
    let paired = listed.iter().find_map(|entry| {
        let base = entry.strip_suffix(".control")?;
        (base.starts_with('/')
            && base.ends_with("/systemd/user")
            && !base.starts_with("/run/")
            && listed.iter().any(|other| other == base))
        .then(|| PathBuf::from(base))
    });
    if paired.is_some() {
        return paired;
    }
    let data = home.join(".local").join("share");
    listed.iter().map(PathBuf::from).find(|dir| {
        dir.is_absolute()
            && dir.ends_with("systemd/user")
            && dir.starts_with(home)
            && !dir.starts_with(&data)
    })
}

/// The installed service on this platform, if its file is where the
/// manager reads it. Without systemd there is none. With systemd running
/// but out of reach, the unit may sit in any folder of the manager's
/// UnitPath, so a running daemon that the service started counts as well as
/// a unit in the default folder.
pub fn installed(home: &Path, sys: &mut dyn System) -> Option<Installed> {
    match locate(platform_manager()?, home, sys) {
        Ok(found) => found.path.exists().then_some(Installed::Reachable(found)),
        Err(Unmanaged::NoSystemd(_)) => None,
        Err(Unmanaged::Unreachable(why)) => {
            let path = default_unit_path(home);
            (path.exists() || sys.service_daemon_running())
                .then_some(Installed::Unreachable { path, why })
        }
    }
}

/// What start, stop and restart say when the manager is out of reach.
pub fn unreachable_message(why: &str) -> String {
    format!(
        "cannot reach the systemd user manager from this session ({why}); \
         run this from your desktop session, or use `systemctl --user` once \
         XDG_RUNTIME_DIR and DBUS_SESSION_BUS_ADDRESS are set"
    )
}

pub fn uid() -> u32 {
    // SAFETY: getuid has no preconditions and cannot fail.
    unsafe { libc::getuid() }
}

/// `systemctl --user ...` or `launchctl ...`.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Cmd {
    pub program: &'static str,
    pub args: Vec<String>,
}

impl Cmd {
    fn new(program: &'static str, args: &[&str]) -> Self {
        Self {
            program,
            args: args.iter().map(|a| a.to_string()).collect(),
        }
    }
}

impl std::fmt::Display for Cmd {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "{} {}", self.program, self.args.join(" "))
    }
}

fn systemctl(args: &[&str]) -> Cmd {
    let mut all = vec!["--user"];
    all.extend_from_slice(args);
    Cmd::new("systemctl", &all)
}

fn job(uid: u32) -> String {
    format!("gui/{uid}/{LABEL}")
}

/// Starts the installed service, so a daemon `start` asked for stays supervised.
pub fn start_cmd(manager: Manager, uid: u32) -> Cmd {
    match manager {
        Manager::Systemd => systemctl(&["start", UNIT_NAME]),
        Manager::Launchd => Cmd::new("launchctl", &["kickstart", &job(uid)]),
    }
}

/// `start` with the service installed. launchd cannot kickstart a job it
/// has not loaded (a failed bootstrap, a manual bootout), so an unloaded
/// plist is bootstrapped instead; `loaded` answers `loaded_probe`.
pub fn start_installed(
    manager: Manager,
    uid: u32,
    unit: &Path,
    loaded: impl FnOnce(&Cmd) -> bool,
) -> Cmd {
    match manager {
        Manager::Launchd if !loaded(&loaded_probe(uid)) => bootstrap(uid, unit),
        _ => start_cmd(manager, uid),
    }
}

/// `stop` with the service installed: through the manager, which would
/// start again a daemon a signal ended (`Restart=always`, `KeepAlive`). A
/// launchd job is booted out, which holds until Start, a re-install or the
/// next login loads it again; one not loaded needs nothing (`None`).
pub fn stop_installed(
    manager: Manager,
    uid: u32,
    loaded: impl FnOnce(&Cmd) -> bool,
) -> Option<Cmd> {
    match manager {
        Manager::Systemd => Some(systemctl(&["stop", UNIT_NAME])),
        Manager::Launchd => loaded(&loaded_probe(uid)).then(|| bootout(uid)),
    }
}

/// Exits 0 when launchd has the job loaded.
pub fn loaded_probe(uid: u32) -> Cmd {
    Cmd::new("launchctl", &["print", &job(uid)])
}

/// Exits 0 when the unit is enabled.
pub fn enabled_probe() -> Cmd {
    systemctl(&["is-enabled", "--quiet", UNIT_NAME])
}

/// Exits 0 when the unit is running.
pub fn active_probe() -> Cmd {
    systemctl(&["is-active", "--quiet", UNIT_NAME])
}

pub fn bootout(uid: u32) -> Cmd {
    Cmd::new("launchctl", &["bootout", &job(uid)])
}

pub fn bootstrap(uid: u32, plist: &Path) -> Cmd {
    Cmd::new(
        "launchctl",
        &[
            "bootstrap",
            &format!("gui/{uid}"),
            &plist.display().to_string(),
        ],
    )
}

pub fn unit_path_query() -> Cmd {
    Cmd::new(
        "busctl",
        &[
            "--user",
            "--json=short",
            "get-property",
            "org.freedesktop.systemd1",
            "/org/freedesktop/systemd1",
            "org.freedesktop.systemd1.Manager",
            "UnitPath",
        ],
    )
}

pub fn daemon_reload() -> Cmd {
    systemctl(&["daemon-reload"])
}

pub fn enable_now() -> Cmd {
    systemctl(&["enable", "--now", UNIT_NAME])
}

/// `enable --now` leaves a running unit on its old command line.
pub fn restart() -> Cmd {
    systemctl(&["restart", UNIT_NAME])
}

pub fn disable_now() -> Cmd {
    systemctl(&["disable", "--now", UNIT_NAME])
}

/// The service manager and the daemon, as the installer touches them.
pub trait System {
    /// Runs `cmd`; an error carries what it printed.
    fn run(&mut self, cmd: &Cmd) -> Result<(), String>;
    /// Whether `cmd` exits 0. For probes, whose failure is an answer.
    fn succeeds(&mut self, cmd: &Cmd) -> bool;
    /// What `cmd` printed on stdout, when it exits 0.
    fn output(&mut self, cmd: &Cmd) -> Result<String, String>;
    /// Whether systemd runs on this machine (see `systemd_booted`).
    fn systemd_booted(&mut self) -> bool;
    /// The pid of a running daemon for the default core that the service did
    /// not start (a detached `start`, or a sidebar block's `--follow-app`).
    fn foreign_daemon(&mut self) -> Option<i32>;
    /// Whether a daemon the service started runs for the default core.
    fn service_daemon_running(&mut self) -> bool;
    fn stop_daemon(&mut self, pid: i32) -> Result<(), String>;
}

/// Runs `cmd` for real.
pub fn run_cmd(cmd: &Cmd) -> Result<(), String> {
    let output = std::process::Command::new(cmd.program)
        .args(&cmd.args)
        .stdin(std::process::Stdio::null())
        .output()
        .map_err(|e| format!("cannot run {}: {e}", cmd.program))?;
    if output.status.success() {
        return Ok(());
    }
    let said = String::from_utf8_lossy(&output.stderr).trim().to_string();
    Err(format!(
        "{cmd} failed ({}){}",
        output.status,
        if said.is_empty() {
            String::new()
        } else {
            format!(": {said}")
        }
    ))
}

/// Runs `cmd` for real and returns its stdout.
pub fn output_cmd(cmd: &Cmd) -> Result<String, String> {
    let output = std::process::Command::new(cmd.program)
        .args(&cmd.args)
        .stdin(std::process::Stdio::null())
        .output()
        .map_err(|e| format!("cannot run {}: {e}", cmd.program))?;
    if !output.status.success() {
        let said = String::from_utf8_lossy(&output.stderr).trim().to_string();
        return Err(match said.lines().next() {
            Some(first) => first.to_string(),
            None => format!("{} exited with {}", cmd.program, output.status),
        });
    }
    Ok(String::from_utf8_lossy(&output.stdout).into_owned())
}

pub fn probe_cmd(cmd: &Cmd) -> bool {
    std::process::Command::new(cmd.program)
        .args(&cmd.args)
        .stdin(std::process::Stdio::null())
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::null())
        .status()
        .is_ok_and(|status| status.success())
}

/// The binary the service runs: the first `herdr-triggersd` on `path` that
/// is the same file as `exe`, so a `~/.local/bin` symlink survives a
/// reinstall to another target; else `exe` itself.
pub fn choose_bin(path: &str, exe: &Path) -> PathBuf {
    use std::os::unix::fs::MetadataExt;
    let Ok(target) = std::fs::metadata(exe) else {
        return exe.to_path_buf();
    };
    std::env::split_paths(path)
        .filter(|dir| dir.is_absolute())
        .map(|dir| dir.join(BIN_NAME))
        .find(|candidate| {
            std::fs::metadata(candidate)
                .is_ok_and(|m| m.dev() == target.dev() && m.ino() == target.ino())
        })
        .unwrap_or_else(|| exe.to_path_buf())
}

/// A systemd unit-file word: quoted when it has to be, `%` doubled because
/// systemd expands specifiers, and `$` doubled where it expands variables.
fn systemd_word(word: &str, in_exec: bool) -> String {
    let mut out = String::with_capacity(word.len());
    for c in word.chars() {
        match c {
            '%' => out.push_str("%%"),
            '$' if in_exec => out.push_str("$$"),
            '\\' => out.push_str("\\\\"),
            '"' => out.push_str("\\\""),
            _ => out.push(c),
        }
    }
    if word
        .chars()
        .any(|c| c.is_whitespace() || c == '"' || c == '\\' || c == '\'' || c == ';')
    {
        format!("\"{out}\"")
    } else {
        out
    }
}

/// Variables that move a directory the daemon uses (config, state, the
/// app's home that `herdr-app do` talks through). Install refuses a
/// relative one, and the service gets each of them pinned.
pub const PASSED_ENV: [&str; 5] = [
    "XDG_CONFIG_HOME",
    "XDG_STATE_HOME",
    "HERDR_TRIGGERS_CONFIG_DIR",
    "HERDR_TRIGGERS_STATE_DIR",
    "HERDR_APP_HOME",
];

/// The service's environment: `PATH`, then every `PASSED_ENV` variable
/// pinned to what the installing CLI resolves. The XDG bases are written
/// out resolved (defaults included) and the others as their value or empty,
/// which the daemon reads as unset. Leaving one out would let the
/// manager's own value through (environment.d, `launchctl setenv`), and the
/// service would keep its lock and log where `status` does not look.
pub fn service_env(
    path: &str,
    config_home: &Path,
    state_home: &Path,
    get: impl Fn(&str) -> Option<String>,
) -> Vec<(String, String)> {
    let mut env = vec![
        ("PATH".to_string(), path.to_string()),
        (
            "XDG_CONFIG_HOME".to_string(),
            config_home.display().to_string(),
        ),
        (
            "XDG_STATE_HOME".to_string(),
            state_home.display().to_string(),
        ),
    ];
    for name in &PASSED_ENV[2..] {
        let value = get(name)
            .filter(|value| Path::new(value).is_absolute())
            .unwrap_or_default();
        env.push((name.to_string(), value));
    }
    env
}

/// `KillMode=process`: a stop or restart ends the daemon only. Programs a
/// `run` action started are fire and forget, live in the unit's cgroup, and
/// would otherwise be killed with it, as they never were under a detached
/// daemon.
pub fn render_unit(bin: &Path, env: &[(String, String)]) -> String {
    let environment: String = env
        .iter()
        .map(|(name, value)| {
            format!(
                "Environment={}\n",
                systemd_word(&format!("{name}={value}"), false)
            )
        })
        .collect();
    format!(
        "[Unit]\n\
         Description=Herdr Triggers for herdr-core\n\
         \n\
         [Service]\n\
         ExecStart={} --host core service\n\
         Restart=always\n\
         RestartSec=5\n\
         KillMode=process\n\
         {environment}\
         \n\
         [Install]\n\
         WantedBy=default.target\n",
        systemd_word(&bin.display().to_string(), true),
    )
}

fn xml(text: &str) -> String {
    let mut out = String::with_capacity(text.len());
    for c in text.chars() {
        match c {
            '&' => out.push_str("&amp;"),
            '<' => out.push_str("&lt;"),
            '>' => out.push_str("&gt;"),
            '"' => out.push_str("&quot;"),
            '\'' => out.push_str("&apos;"),
            _ => out.push(c),
        }
    }
    out
}

/// stdout and stderr go to /dev/null: the daemon keeps its own log.
/// `AbandonProcessGroup` keeps programs a `run` action started alive when
/// the job stops, as `KillMode=process` does for the unit.
pub fn render_plist(bin: &Path, env: &[(String, String)]) -> String {
    let environment: String = env
        .iter()
        .map(|(name, value)| {
            format!(
                "\t\t<key>{}</key>\n\t\t<string>{}</string>\n",
                xml(name),
                xml(value)
            )
        })
        .collect();
    format!(
        "<?xml version=\"1.0\" encoding=\"UTF-8\"?>\n\
         <!DOCTYPE plist PUBLIC \"-//Apple//DTD PLIST 1.0//EN\" \"http://www.apple.com/DTDs/PropertyList-1.0.dtd\">\n\
         <plist version=\"1.0\">\n\
         <dict>\n\
         \t<key>Label</key>\n\
         \t<string>{LABEL}</string>\n\
         \t<key>ProgramArguments</key>\n\
         \t<array>\n\
         \t\t<string>{}</string>\n\
         \t\t<string>--host</string>\n\
         \t\t<string>core</string>\n\
         \t\t<string>service</string>\n\
         \t</array>\n\
         \t<key>RunAtLoad</key>\n\
         \t<true/>\n\
         \t<key>AbandonProcessGroup</key>\n\
         \t<true/>\n\
         \t<key>KeepAlive</key>\n\
         \t<true/>\n\
         \t<key>EnvironmentVariables</key>\n\
         \t<dict>\n\
         {environment}\
         \t</dict>\n\
         \t<key>StandardOutPath</key>\n\
         \t<string>/dev/null</string>\n\
         \t<key>StandardErrorPath</key>\n\
         \t<string>/dev/null</string>\n\
         </dict>\n\
         </plist>\n",
        xml(&bin.display().to_string()),
    )
}

pub fn render(manager: Manager, bin: &Path, env: &[(String, String)]) -> String {
    match manager {
        Manager::Systemd => render_unit(bin, env),
        Manager::Launchd => render_plist(bin, env),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn path_only(path: &str) -> Vec<(String, String)> {
        vec![("PATH".to_string(), path.to_string())]
    }

    #[test]
    fn the_unit_carries_the_binary_and_path() {
        let unit = render_unit(
            Path::new("/home/u/.local/bin/herdr-triggersd"),
            &path_only("/usr/bin:/bin"),
        );
        assert_eq!(
            unit,
            "[Unit]\nDescription=Herdr Triggers for herdr-core\n\n[Service]\n\
             ExecStart=/home/u/.local/bin/herdr-triggersd --host core service\n\
             Restart=always\nRestartSec=5\nKillMode=process\nEnvironment=PATH=/usr/bin:/bin\n\n\
             [Install]\nWantedBy=default.target\n"
        );
    }

    #[test]
    fn the_unit_quotes_spaces_and_escapes_specifiers() {
        let unit = render_unit(
            Path::new("/home/a b/50%/$x/herdr-triggersd"),
            &path_only("/my bin:/usr/bin"),
        );
        assert!(
            unit.contains("ExecStart=\"/home/a b/50%%/$$x/herdr-triggersd\" --host core service\n"),
            "{unit}"
        );
        assert!(
            unit.contains("Environment=\"PATH=/my bin:/usr/bin\"\n"),
            "{unit}"
        );
    }

    #[test]
    fn the_plist_escapes_xml_and_keeps_spaces() {
        let plist = render_plist(
            Path::new("/Users/a b/<&>/herdr-triggersd"),
            &path_only("/x\"y:/usr/bin"),
        );
        assert!(
            plist.contains("<string>/Users/a b/&lt;&amp;&gt;/herdr-triggersd</string>"),
            "{plist}"
        );
        assert!(
            plist.contains("<string>/x&quot;y:/usr/bin</string>"),
            "{plist}"
        );
        assert!(
            plist.contains("<key>Label</key>\n\t<string>io.github.cantona.herdr-triggers</string>")
        );
        assert!(plist.contains("<key>KeepAlive</key>\n\t<true/>"));
        assert!(!plist.contains("SuccessfulExit"));
        assert!(plist.contains("<key>AbandonProcessGroup</key>\n\t<true/>"));
        assert!(plist.contains("<key>RunAtLoad</key>\n\t<true/>"));
        assert_eq!(plist.matches("<string>/dev/null</string>").count(), 2);
        assert!(
            plist.contains(
                "<key>EnvironmentVariables</key>\n\t<dict>\n\
                 \t\t<key>PATH</key>\n\t\t<string>/x&quot;y:/usr/bin</string>\n\t</dict>\n"
            ),
            "{plist}"
        );
    }

    #[test]
    fn every_directory_variable_is_pinned_and_unset_ones_are_empty() {
        let env = service_env(
            "/usr/bin",
            Path::new("/home/u/.config"),
            Path::new("/srv/50% state"),
            |name| match name {
                "HERDR_TRIGGERS_CONFIG_DIR" => Some("/cfg/<rules>".to_string()),
                "HERDR_APP_HOME" => Some("/apps/herdr app".to_string()),
                "HERDR_TRIGGERS_STATE_DIR" => Some("relative/state".to_string()),
                "HC_SOCKET" => Some("/never".to_string()),
                _ => None,
            },
        );
        let pair = |k: &str, v: &str| (k.to_string(), v.to_string());
        assert_eq!(
            env,
            [
                pair("PATH", "/usr/bin"),
                pair("XDG_CONFIG_HOME", "/home/u/.config"),
                pair("XDG_STATE_HOME", "/srv/50% state"),
                pair("HERDR_TRIGGERS_CONFIG_DIR", "/cfg/<rules>"),
                pair("HERDR_TRIGGERS_STATE_DIR", ""),
                pair("HERDR_APP_HOME", "/apps/herdr app"),
            ]
        );

        // systemd: `NAME=` sets an empty value, which the daemon reads as
        // unset; launchd: an empty <string/> does the same.
        let unit = render_unit(Path::new("/bin/herdr-triggersd"), &env);
        assert!(
            unit.contains(
                "KillMode=process\nEnvironment=PATH=/usr/bin\n\
                 Environment=XDG_CONFIG_HOME=/home/u/.config\n\
                 Environment=\"XDG_STATE_HOME=/srv/50%% state\"\n\
                 Environment=HERDR_TRIGGERS_CONFIG_DIR=/cfg/<rules>\n\
                 Environment=HERDR_TRIGGERS_STATE_DIR=\n\
                 Environment=\"HERDR_APP_HOME=/apps/herdr app\"\n\n[Install]\n"
            ),
            "{unit}"
        );

        let plist = render_plist(Path::new("/bin/herdr-triggersd"), &env);
        assert!(
            plist.contains(
                "\t\t<key>PATH</key>\n\t\t<string>/usr/bin</string>\n\
                 \t\t<key>XDG_CONFIG_HOME</key>\n\t\t<string>/home/u/.config</string>\n\
                 \t\t<key>XDG_STATE_HOME</key>\n\t\t<string>/srv/50% state</string>\n\
                 \t\t<key>HERDR_TRIGGERS_CONFIG_DIR</key>\n\t\t<string>/cfg/&lt;rules&gt;</string>\n\
                 \t\t<key>HERDR_TRIGGERS_STATE_DIR</key>\n\t\t<string></string>\n\
                 \t\t<key>HERDR_APP_HOME</key>\n\t\t<string>/apps/herdr app</string>\n\t</dict>\n"
            ),
            "{plist}"
        );
        assert!(!unit.contains("HC_SOCKET") && !plist.contains("HC_SOCKET"));
    }

    #[test]
    fn stop_goes_through_the_manager() {
        assert_eq!(
            stop_installed(Manager::Systemd, 1000, |_| panic!("no probe")).map(|c| c.to_string()),
            Some("systemctl --user stop herdr-triggers.service".to_string())
        );
        let mut probed = Vec::new();
        assert_eq!(
            stop_installed(Manager::Launchd, 501, |c| {
                probed.push(c.to_string());
                true
            })
            .map(|c| c.to_string()),
            Some("launchctl bootout gui/501/io.github.cantona.herdr-triggers".to_string())
        );
        assert_eq!(
            probed,
            ["launchctl print gui/501/io.github.cantona.herdr-triggers"]
        );
        assert_eq!(stop_installed(Manager::Launchd, 501, |_| false), None);
    }

    #[test]
    fn start_bootstraps_a_launchd_job_that_is_not_loaded() {
        let plist = Path::new("/Users/u/Library/LaunchAgents/x.plist");
        let mut probed = Vec::new();
        let cmd = start_installed(Manager::Launchd, 501, plist, |c| {
            probed.push(c.to_string());
            false
        });
        assert_eq!(
            cmd.to_string(),
            "launchctl bootstrap gui/501 /Users/u/Library/LaunchAgents/x.plist"
        );
        assert_eq!(
            probed,
            ["launchctl print gui/501/io.github.cantona.herdr-triggers"]
        );
        assert_eq!(
            start_installed(Manager::Launchd, 501, plist, |_| true).to_string(),
            "launchctl kickstart gui/501/io.github.cantona.herdr-triggers"
        );
        assert_eq!(
            start_installed(Manager::Systemd, 1000, plist, |_| panic!("no probe")).to_string(),
            "systemctl --user start herdr-triggers.service"
        );
    }

    #[test]
    fn the_binary_on_path_is_used_only_when_it_is_the_same_file() {
        let root = std::env::temp_dir().join(format!("triggers-bin-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&root);
        let (cargo, local, other) = (root.join("cargo"), root.join("local"), root.join("other"));
        for dir in [&cargo, &local, &other] {
            std::fs::create_dir_all(dir).unwrap();
        }
        let exe = cargo.join(BIN_NAME);
        std::fs::write(&exe, "bin").unwrap();
        std::os::unix::fs::symlink(&exe, local.join(BIN_NAME)).unwrap();
        std::fs::write(other.join(BIN_NAME), "another build").unwrap();

        let path = |dirs: &[&PathBuf]| {
            std::env::join_paths(dirs.iter().map(|d| d.as_path()))
                .unwrap()
                .into_string()
                .unwrap()
        };
        assert_eq!(
            choose_bin(&path(&[&local, &cargo]), &exe),
            local.join(BIN_NAME)
        );
        assert_eq!(
            choose_bin(&path(&[&other, &cargo]), &exe),
            cargo.join(BIN_NAME)
        );
        assert_eq!(choose_bin(&path(&[&other]), &exe), exe);
        assert_eq!(choose_bin("relative:", &exe), exe);
        std::fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn the_user_unit_folder_is_read_from_systemds_unit_path() {
        let home = Path::new("/home/u");
        let list = |entries: &[&str]| entries.iter().map(|e| e.to_string()).collect::<Vec<_>>();
        let normal = list(&[
            "/home/u/cfg/systemd/user.control",
            "/run/user/1000/systemd/user.control",
            "/run/user/1000/systemd/transient",
            "/home/u/cfg/systemd/user",
            "/etc/systemd/user",
            "/run/user/1000/systemd/user",
            "/home/u/.local/share/systemd/user",
            "/usr/lib/systemd/user",
        ]);
        assert_eq!(
            user_unit_dir(&normal, home),
            Some(PathBuf::from("/home/u/cfg/systemd/user"))
        );
        // The manager's XDG_CONFIG_HOME outside HOME, with a space in it,
        // and an extra SYSTEMD_UNIT_PATH folder in front.
        let outside = list(&[
            "/home/u/units",
            "/data/my cfg/systemd/user.control",
            "/run/user/1000/systemd/user.control",
            "/data/my cfg/systemd/user",
            "/etc/systemd/user",
            "/run/user/1000/systemd/user",
        ]);
        assert_eq!(
            user_unit_dir(&outside, home),
            Some(PathBuf::from("/data/my cfg/systemd/user"))
        );
        // Only the runtime pair: tmpfs, never chosen.
        assert_eq!(
            user_unit_dir(
                &list(&[
                    "/run/user/1000/systemd/user.control",
                    "/run/user/1000/systemd/user"
                ]),
                home
            ),
            None
        );
        // No .control pair: the HOME rule.
        assert_eq!(
            user_unit_dir(
                &list(&["/etc/systemd/user", "/home/u/.config/systemd/user"]),
                home
            ),
            Some(PathBuf::from("/home/u/.config/systemd/user"))
        );
        // Only the data folder and system folders: nothing to write to.
        assert_eq!(
            user_unit_dir(
                &list(&["/etc/systemd/user", "/home/u/.local/share/systemd/user"]),
                home
            ),
            None
        );
        assert_eq!(user_unit_dir(&[], home), None);

        assert_eq!(
            parse_unit_path(r#"{"type":"as","data":["/a b/systemd/user","/etc/systemd/user"]}"#),
            Ok(list(&["/a b/systemd/user", "/etc/systemd/user"]))
        );
        assert!(parse_unit_path("as 2 \"/a\" \"/b\"").is_err());
        assert!(parse_unit_path(r#"{"type":"as"}"#).is_err());
        assert_eq!(
            unit_path_query().to_string(),
            "busctl --user --json=short get-property org.freedesktop.systemd1 \
             /org/freedesktop/systemd1 org.freedesktop.systemd1.Manager UnitPath"
        );
    }

    #[test]
    fn the_service_files_live_where_the_managers_look() {
        assert_eq!(
            plist_path(Path::new("/home/u")),
            Path::new("/home/u/Library/LaunchAgents/io.github.cantona.herdr-triggers.plist")
        );
        assert_eq!(
            start_cmd(Manager::Launchd, 501).to_string(),
            "launchctl kickstart gui/501/io.github.cantona.herdr-triggers"
        );
        assert_eq!(
            start_cmd(Manager::Systemd, 1000).to_string(),
            "systemctl --user start herdr-triggers.service"
        );
    }
}
