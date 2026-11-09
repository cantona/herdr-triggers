//! Where herdr-core's socket is, and which session this daemon targets.
//!
//! The order copies `hc` itself: an explicit `HC_SOCKET`, the `socket` key
//! of herdr-core's config, the runtime directory, then the state-home
//! fallback `hc` uses when there is no runtime directory.

use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::OnceLock;

pub const DEFAULT_SESSION: &str = "default";

static SOCKET_OVERRIDE: OnceLock<PathBuf> = OnceLock::new();
static SESSION_OVERRIDE: OnceLock<String> = OnceLock::new();
static SERVICE_MODE: AtomicBool = AtomicBool::new(false);

/// `--socket`: set once, before anything resolves a path. Two targets in one
/// process is a bug, so a second call is refused loudly.
pub fn set_socket_override(path: PathBuf) {
    if let Err(ignored) = SOCKET_OVERRIDE.set(path) {
        panic!(
            "core socket override already set; refused {}",
            ignored.display()
        );
    }
}

/// `service`: resolve as the login service does, whatever the manager's
/// environment holds. HC_SESSION and HC_SOCKET belong to a core terminal; one
/// imported into the manager (`systemctl --user import-environment` run from
/// such a terminal) must not move the service off the default session that
/// install, `start` and `status` take it to watch. `--socket` and `--session`
/// still win: they are explicit.
pub fn set_service_mode() {
    SERVICE_MODE.store(true, Ordering::Relaxed);
}

/// `--session`: same contract as the socket override.
pub fn set_session_override(name: String) {
    if let Err(ignored) = SESSION_OVERRIDE.set(name) {
        panic!("core session override already set; refused {ignored:?}");
    }
}

/// The core session: `--session`, else `HC_SESSION` (not for the service),
/// else `default`.
pub fn session() -> String {
    if let Some(name) = SESSION_OVERRIDE.get() {
        return name.clone();
    }
    if SERVICE_MODE.load(Ordering::Relaxed) {
        return DEFAULT_SESSION.to_string();
    }
    match std::env::var("HC_SESSION") {
        Ok(name) if !name.trim().is_empty() => name.trim().to_string(),
        _ => DEFAULT_SESSION.to_string(),
    }
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct SocketInputs {
    pub session: String,
    pub hc_socket: Option<PathBuf>,
    pub config_socket: Option<PathBuf>,
    pub runtime_dir: Option<PathBuf>,
    pub run_user_dir: Option<PathBuf>,
    pub state_home: PathBuf,
}

impl SocketInputs {
    pub fn from_env() -> Result<Self, String> {
        // SAFETY: getuid has no preconditions and cannot fail.
        let uid = unsafe { libc::getuid() };
        let run_user = PathBuf::from(format!("/run/user/{uid}"));
        Ok(Self {
            session: session(),
            hc_socket: non_empty("HC_SOCKET"),
            config_socket: core_config_socket(
                &config_home().join("herdr-core").join("config.toml"),
            )?,
            runtime_dir: non_empty("XDG_RUNTIME_DIR"),
            run_user_dir: run_user.is_dir().then_some(run_user),
            state_home: state_home(),
        })
    }
}

/// The socket this daemon talks to: the override, else the resolution above.
pub fn socket_path() -> Result<PathBuf, String> {
    if let Some(path) = SOCKET_OVERRIDE.get() {
        return Ok(path.clone());
    }
    let mut inputs = SocketInputs::from_env()?;
    if SESSION_OVERRIDE.get().is_some() {
        // An explicit `--session` names the core to act on. HC_SOCKET (set
        // inside core terminals) and the config's `socket` key describe
        // whatever core is ambient, and must not redirect it silently.
        inputs.hc_socket = None;
        inputs.config_socket = None;
    } else if SERVICE_MODE.load(Ordering::Relaxed) {
        inputs = as_service(inputs);
    }
    Ok(resolve_socket(&inputs))
}

/// The socket the login service resolves. It runs with neither HC_SESSION
/// nor HC_SOCKET (those belong to a core terminal), so it always watches the
/// default session, wherever core's config puts it.
pub fn service_socket() -> Result<PathBuf, String> {
    Ok(resolve_socket(&as_service(SocketInputs::from_env()?)))
}

/// `inputs` as the service sees them.
pub fn as_service(inputs: SocketInputs) -> SocketInputs {
    SocketInputs {
        session: DEFAULT_SESSION.to_string(),
        hc_socket: None,
        ..inputs
    }
}

/// Whether two resolved sockets are the same core. A path given with
/// `--socket` may name it through a symlink or a relative path.
pub fn same_socket(a: &Path, b: &Path) -> bool {
    if a == b {
        return true;
    }
    match (std::fs::canonicalize(a), std::fs::canonicalize(b)) {
        (Ok(a), Ok(b)) => a == b,
        _ => false,
    }
}

pub fn resolve_socket(inputs: &SocketInputs) -> PathBuf {
    if let Some(socket) = &inputs.hc_socket {
        return socket.clone();
    }
    if let Some(socket) = &inputs.config_socket {
        return socket.clone();
    }
    let file = format!("{}.sock", inputs.session);
    if let Some(run) = &inputs.runtime_dir {
        return run.join("herdr-core").join(file);
    }
    if let Some(run) = &inputs.run_user_dir {
        return run.join("herdr-core").join(file);
    }
    inputs
        .state_home
        .join("herdr-core")
        .join(&inputs.session)
        .join(file)
}

/// Every core session with a socket where `resolve_socket` would look: the
/// runtime directory when there is one (Linux), else one folder per session
/// under the state home (macOS). Holder sockets
/// (`default.holder.<id>.sock`) are the core's own, not servers.
pub fn discover_sessions() -> Result<Vec<(String, PathBuf)>, String> {
    Ok(discover_in(&SocketInputs::from_env()?))
}

fn discover_in(inputs: &SocketInputs) -> Vec<(String, PathBuf)> {
    let session_of = |path: &Path| -> Option<String> {
        let session = path.file_name()?.to_str()?.strip_suffix(".sock")?;
        (!session.contains('.')).then(|| session.to_string())
    };
    let mut found: Vec<(String, PathBuf)> =
        match inputs.runtime_dir.as_ref().or(inputs.run_user_dir.as_ref()) {
            Some(run) => std::fs::read_dir(run.join("herdr-core"))
                .into_iter()
                .flatten()
                .flatten()
                .filter_map(|entry| {
                    let path = entry.path();
                    Some((session_of(&path)?, path))
                })
                .collect(),
            None => std::fs::read_dir(inputs.state_home.join("herdr-core"))
                .into_iter()
                .flatten()
                .flatten()
                .filter_map(|entry| {
                    let name = entry.file_name().to_str()?.to_string();
                    let path = entry.path().join(format!("{name}.sock"));
                    path.exists().then_some((name, path))
                })
                .collect(),
        };
    found.sort();
    found
}

/// The Herdr desktop app's home: `HERDR_APP_HOME`, else `herdr-app` under
/// the config home. Its mods and `keys.json` live here.
pub fn app_home() -> PathBuf {
    non_empty("HERDR_APP_HOME").unwrap_or_else(|| config_home().join("herdr-app"))
}

/// The app's control socket, in its home. It stays on disk after the app
/// quits; only a refused connection tells that it is gone.
pub fn app_socket() -> PathBuf {
    app_home().join("herdr-app.sock")
}

/// `XDG_CONFIG_HOME`, else `~/.config`. A relative value is ignored, as the
/// XDG spec says: it would resolve against whatever the working directory
/// is, so a detached daemon and a later command could disagree.
pub fn config_home() -> PathBuf {
    xdg_base(non_empty("XDG_CONFIG_HOME"), &home_dir(), ".config")
}

/// `XDG_STATE_HOME`, else `~/.local/state`, with the same rule.
pub fn state_home() -> PathBuf {
    xdg_base(non_empty("XDG_STATE_HOME"), &home_dir(), ".local/state")
}

fn xdg_base(xdg: Option<PathBuf>, home: &Path, fallback: &str) -> PathBuf {
    xdg.filter(|dir| dir.is_absolute())
        .unwrap_or_else(|| home.join(fallback))
}

/// `~/.config/herdr-triggers`, or `HERDR_TRIGGERS_CONFIG_DIR`.
pub fn config_dir() -> PathBuf {
    non_empty("HERDR_TRIGGERS_CONFIG_DIR").unwrap_or_else(|| config_home().join("herdr-triggers"))
}

/// `~/.local/state/herdr-triggers/core-<server>`, or that tag under
/// `HERDR_TRIGGERS_STATE_DIR`. The session is part of the path for the same
/// reason as on the herdr host: one daemon per server, each with its own lock.
pub fn state_dir() -> PathBuf {
    let base = non_empty("HERDR_TRIGGERS_STATE_DIR")
        .unwrap_or_else(|| state_home().join("herdr-triggers"));
    base.join(format!("core-{}", filesystem_safe(&server_tag())))
}

/// The name of the core this process targets, from the socket it resolved
/// to (`<session>.sock`), so `--socket`, `--all` children and an ambient
/// HC_SOCKET each get the state dir of the core they actually watch. Two
/// daemons sharing one dir would share one lock, and the second would never
/// start.
pub fn server_tag() -> String {
    socket_path()
        .ok()
        .and_then(|path| tag_of(&path))
        .unwrap_or_else(session)
}

/// The server tag a socket path gives (`<tag>.sock`).
pub fn tag_of(socket: &Path) -> Option<String> {
    let stem = socket.file_stem()?.to_str()?;
    (!stem.is_empty()).then(|| stem.to_string())
}

fn filesystem_safe(name: &str) -> String {
    name.chars()
        .map(|c| {
            if c.is_alphanumeric() || c == '-' || c == '_' {
                c
            } else {
                '_'
            }
        })
        .collect()
}

fn core_config_socket(path: &Path) -> Result<Option<PathBuf>, String> {
    let text = match std::fs::read_to_string(path) {
        Ok(text) => text,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(None),
        Err(e) => return Err(format!("cannot read {}: {e}", path.display())),
    };
    let value: toml::Value =
        toml::from_str(&text).map_err(|e| format!("invalid {}: {e}", path.display()))?;
    Ok(value
        .get("socket")
        .and_then(toml::Value::as_str)
        .map(PathBuf::from))
}

fn non_empty(name: &str) -> Option<PathBuf> {
    std::env::var_os(name)
        .filter(|value| !value.is_empty())
        .map(PathBuf::from)
}

pub fn home_dir() -> PathBuf {
    std::env::var("HOME").map(PathBuf::from).unwrap_or_default()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn inputs() -> SocketInputs {
        SocketInputs {
            session: "default".into(),
            hc_socket: None,
            config_socket: None,
            runtime_dir: None,
            run_user_dir: None,
            state_home: PathBuf::from("/home/u/.local/state"),
        }
    }

    #[test]
    fn socket_rules_apply_in_core_order() {
        let mut i = inputs();
        assert_eq!(
            resolve_socket(&i),
            PathBuf::from("/home/u/.local/state/herdr-core/default/default.sock")
        );
        i.run_user_dir = Some("/run/user/1000".into());
        assert_eq!(
            resolve_socket(&i),
            PathBuf::from("/run/user/1000/herdr-core/default.sock")
        );
        i.runtime_dir = Some("/rt".into());
        assert_eq!(
            resolve_socket(&i),
            PathBuf::from("/rt/herdr-core/default.sock")
        );
        i.session = "com".into();
        assert_eq!(resolve_socket(&i), PathBuf::from("/rt/herdr-core/com.sock"));
        i.config_socket = Some("/srv/hc.sock".into());
        assert_eq!(resolve_socket(&i), PathBuf::from("/srv/hc.sock"));
        i.hc_socket = Some("/x/hc.sock".into());
        assert_eq!(resolve_socket(&i), PathBuf::from("/x/hc.sock"));
    }

    #[test]
    fn only_a_command_aimed_at_the_default_core_matches_the_service() {
        let mut ambient = inputs();
        ambient.runtime_dir = Some("/rt".into());
        let service = resolve_socket(&as_service(ambient.clone()));
        assert_eq!(service, PathBuf::from("/rt/herdr-core/default.sock"));
        assert!(same_socket(&resolve_socket(&ambient), &service));

        // A shell inside core session "work", by HC_SESSION or HC_SOCKET.
        let mut work = ambient.clone();
        work.session = "work".into();
        assert!(!same_socket(&resolve_socket(&work), &service));
        let mut work = ambient.clone();
        work.hc_socket = Some("/rt/herdr-core/work.sock".into());
        assert!(!same_socket(&resolve_socket(&work), &service));

        // A terminal of the default core carries HC_SOCKET too.
        let mut default = ambient.clone();
        default.hc_socket = Some("/rt/herdr-core/default.sock".into());
        assert!(same_socket(&resolve_socket(&default), &service));

        // `--socket` from `--all`, or typed: the path itself is compared.
        assert!(same_socket(
            Path::new("/rt/herdr-core/default.sock"),
            &service
        ));
        assert!(!same_socket(
            Path::new("/rt/herdr-core/work.sock"),
            &service
        ));

        // core's config moves the socket for the service as well.
        ambient.config_socket = Some("/srv/hc.sock".into());
        let moved = resolve_socket(&as_service(ambient.clone()));
        assert_eq!(moved, PathBuf::from("/srv/hc.sock"));
        assert!(same_socket(&resolve_socket(&ambient), &moved));
    }

    #[test]
    fn a_socket_named_through_a_symlink_is_the_same_socket() {
        let dir = std::env::temp_dir().join(format!("triggers-same-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        let real = dir.join("default.sock");
        std::fs::write(&real, "").unwrap();
        std::os::unix::fs::symlink(&real, dir.join("link.sock")).unwrap();
        assert!(same_socket(&dir.join("link.sock"), &real));
        assert!(!same_socket(&dir.join("missing.sock"), &real));
        std::fs::remove_dir_all(dir).unwrap();
    }

    #[test]
    fn the_core_config_socket_key_is_read_and_a_missing_file_is_not_an_error() {
        let dir = std::env::temp_dir().join(format!("triggers-paths-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("config.toml");
        assert_eq!(core_config_socket(&path).unwrap(), None);
        std::fs::write(&path, "notes = false\nsocket = \"/srv/hc.sock\"\n").unwrap();
        assert_eq!(
            core_config_socket(&path).unwrap(),
            Some(PathBuf::from("/srv/hc.sock"))
        );
        std::fs::write(&path, "socket = [\n").unwrap();
        assert!(core_config_socket(&path).is_err());
        std::fs::remove_dir_all(dir).unwrap();
    }

    #[test]
    fn sessions_are_discovered_where_sockets_are_resolved() {
        let root = std::env::temp_dir().join(format!("triggers-discover-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&root);
        let state = root.join("state");
        for session in ["default", "work"] {
            std::fs::create_dir_all(state.join("herdr-core").join(session)).unwrap();
            std::fs::write(
                state
                    .join("herdr-core")
                    .join(session)
                    .join(format!("{session}.sock")),
                "",
            )
            .unwrap();
        }
        std::fs::write(
            state.join("herdr-core/default/default.holder.ab12.sock"),
            "",
        )
        .unwrap();
        std::fs::create_dir_all(state.join("herdr-core/stale")).unwrap();
        let mut i = inputs();
        i.state_home = state.clone();
        let names: Vec<String> = discover_in(&i).into_iter().map(|(n, _)| n).collect();
        assert_eq!(names, ["default", "work"]);
        for (name, path) in discover_in(&i) {
            let mut one = i.clone();
            one.session = name;
            assert_eq!(resolve_socket(&one), path);
        }

        let run = root.join("run");
        std::fs::create_dir_all(run.join("herdr-core")).unwrap();
        std::fs::write(run.join("herdr-core/default.sock"), "").unwrap();
        std::fs::write(run.join("herdr-core/default.holder.cd34.sock"), "").unwrap();
        i.runtime_dir = Some(run);
        let names: Vec<String> = discover_in(&i).into_iter().map(|(n, _)| n).collect();
        assert_eq!(names, ["default"]);
        std::fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn a_relative_xdg_base_is_ignored() {
        let home = Path::new("/home/u");
        assert_eq!(
            xdg_base(None, home, ".config"),
            Path::new("/home/u/.config")
        );
        assert_eq!(
            xdg_base(Some("/x/cfg".into()), home, ".config"),
            Path::new("/x/cfg")
        );
        assert_eq!(
            xdg_base(Some("cfg".into()), home, ".config"),
            Path::new("/home/u/.config")
        );
        assert_eq!(
            xdg_base(Some("state".into()), home, ".local/state"),
            Path::new("/home/u/.local/state")
        );
    }

    #[test]
    fn a_session_name_becomes_a_safe_state_tag() {
        assert_eq!(filesystem_safe("default"), "default");
        assert_eq!(filesystem_safe("a/b c"), "a_b_c");
    }
}
