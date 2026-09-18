//! JSON-lines client for herdr's unix socket.
//!
//! Every call is one request on its own short-lived connection. The daemon does
//! not use `events.subscribe`: it reads pane screens and matches rules itself,
//! so it never needs a long-lived event stream.

use std::io::{BufRead, BufReader, Write};
use std::os::unix::net::UnixStream;
use std::path::PathBuf;
use std::time::Duration;

use serde_json::{json, Value};

/// Set by `--session`/`--socket` so a control command can target a server
/// explicitly instead of inheriting whichever one the calling pane belongs to.
static OVERRIDE: std::sync::OnceLock<PathBuf> = std::sync::OnceLock::new();

/// Set once, before anything resolves a path. A second call would mean two
/// different targets in one process, which is a bug rather than a preference,
/// so it is refused loudly instead of silently keeping the first.
pub fn set_socket_override(path: PathBuf) {
    if let Err(ignored) = OVERRIDE.set(path) {
        panic!("socket override already set; refused {}", ignored.display());
    }
}

/// Every herdr server on this machine: the default one plus each named
/// session, keyed by the session's own name ("default" for the default
/// server). Note this is NOT the state-dir tag, which prefixes a named session
/// with `session-` so it cannot collide with the default server.
pub fn discover_servers() -> Vec<(String, PathBuf)> {
    let config = match std::env::var("XDG_CONFIG_HOME") {
        Ok(dir) if !dir.is_empty() => PathBuf::from(dir).join("herdr"),
        _ => home_dir().join(".config").join("herdr"),
    };
    let mut found = Vec::new();
    let default = config.join("herdr.sock");
    if default.exists() {
        found.push(("default".to_string(), default));
    }
    if let Ok(entries) = std::fs::read_dir(config.join("sessions")) {
        for entry in entries.flatten() {
            let sock = entry.path().join("herdr.sock");
            if sock.exists() {
                found.push((entry.file_name().to_string_lossy().into_owned(), sock));
            }
        }
    }
    found
}

/// Where herdr's API socket lives, in precedence order:
///
/// 1. an explicit `--session`/`--socket` override,
/// 2. `HERDR_SESSION`, when that session's socket exists,
/// 3. `HERDR_SOCKET_PATH`,
/// 4. the default server's socket.
///
/// The session name beats the path because herdr exports BOTH to plugin
/// commands and a shell can carry a stale or default `HERDR_SOCKET_PATH`
/// alongside a `HERDR_SESSION` naming the server actually in use. Trusting the
/// path first attaches the daemon to a different server, where it watches the
/// wrong panes and every rule appears to have stopped working.
pub fn socket_path() -> PathBuf {
    if let Some(path) = OVERRIDE.get() {
        return path.clone();
    }
    let config_dir = match std::env::var("XDG_CONFIG_HOME") {
        Ok(dir) if !dir.is_empty() => PathBuf::from(dir).join("herdr"),
        _ => home_dir().join(".config").join("herdr"),
    };
    if let Ok(name) = std::env::var("HERDR_SESSION") {
        if !name.is_empty() {
            let by_session = config_dir.join("sessions").join(&name).join("herdr.sock");
            if by_session.exists() {
                return by_session;
            }
        }
    }
    if let Ok(path) = std::env::var("HERDR_SOCKET_PATH") {
        if !path.is_empty() {
            return PathBuf::from(path);
        }
    }
    config_dir.join("herdr.sock")
}

fn home_dir() -> PathBuf {
    std::env::var("HOME").map(PathBuf::from).unwrap_or_default()
}

#[derive(Debug)]
pub enum ClientError {
    Io(std::io::Error),
    Closed,
    Api { code: String, message: String },
    Malformed(String),
}

impl std::fmt::Display for ClientError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            ClientError::Io(err) => write!(f, "socket error: {err}"),
            ClientError::Closed => write!(f, "herdr closed the connection"),
            ClientError::Api { code, message } => {
                write!(f, "herdr rejected the call: {code}: {message}")
            }
            ClientError::Malformed(text) => write!(f, "unparsable frame from herdr: {text}"),
        }
    }
}

impl std::error::Error for ClientError {}

impl From<std::io::Error> for ClientError {
    fn from(err: std::io::Error) -> Self {
        ClientError::Io(err)
    }
}

#[derive(Debug, Clone)]
pub struct Client {
    path: PathBuf,
}

impl Client {
    pub fn new(path: PathBuf) -> Self {
        Self { path }
    }

    pub fn from_env() -> Self {
        Self::new(socket_path())
    }

    pub fn path(&self) -> &PathBuf {
        &self.path
    }

    /// One request, one response, one connection.
    pub fn request(&self, method: &str, params: Value) -> Result<Value, ClientError> {
        let mut stream = UnixStream::connect(&self.path)?;
        stream.set_read_timeout(Some(Duration::from_secs(10)))?;
        stream.set_write_timeout(Some(Duration::from_secs(10)))?;

        let frame = json!({"id": "req", "method": method, "params": params});
        let mut bytes = frame.to_string().into_bytes();
        bytes.push(b'\n');
        stream.write_all(&bytes)?;
        let mut reader = BufReader::new(stream);

        let mut line = String::new();
        if reader.read_line(&mut line)? == 0 {
            return Err(ClientError::Closed);
        }
        let mut value: Value =
            serde_json::from_str(line.trim()).map_err(|_| ClientError::Malformed(line.clone()))?;
        if let Some(error) = value.get("error") {
            return Err(ClientError::Api {
                code: error
                    .get("code")
                    .and_then(Value::as_str)
                    .unwrap_or("unknown")
                    .to_string(),
                message: error
                    .get("message")
                    .and_then(Value::as_str)
                    .unwrap_or_default()
                    .to_string(),
            });
        }
        Ok(value
            .get_mut("result")
            .map(Value::take)
            .unwrap_or(Value::Null))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::{Mutex, MutexGuard, OnceLock};

    /// These tests mutate process-wide environment variables, so they must not
    /// run concurrently with each other.
    fn env_lock() -> MutexGuard<'static, ()> {
        static LOCK: OnceLock<Mutex<()>> = OnceLock::new();
        LOCK.get_or_init(|| Mutex::new(()))
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
    }

    struct EnvGuard {
        key: &'static str,
        previous: Option<String>,
    }

    impl EnvGuard {
        fn set(key: &'static str, value: &str) -> Self {
            let previous = std::env::var(key).ok();
            std::env::set_var(key, value);
            Self { key, previous }
        }

        fn unset(key: &'static str) -> Self {
            let previous = std::env::var(key).ok();
            std::env::remove_var(key);
            Self { key, previous }
        }
    }

    impl Drop for EnvGuard {
        fn drop(&mut self) {
            match self.previous.take() {
                Some(value) => std::env::set_var(self.key, value),
                None => std::env::remove_var(self.key),
            }
        }
    }

    /// A session dir with a real socket file, so `exists()` is true.
    fn seed_session(root: &str, name: &str) -> std::path::PathBuf {
        let dir = std::path::PathBuf::from(root)
            .join("herdr")
            .join("sessions")
            .join(name);
        std::fs::create_dir_all(&dir).unwrap();
        let sock = dir.join("herdr.sock");
        std::fs::write(&sock, b"").unwrap();
        sock
    }

    fn temp_root(tag: &str) -> String {
        let root = std::env::temp_dir().join(format!(
            "herdr-triggers-sock-{}-{tag}-{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        std::fs::create_dir_all(&root).unwrap();
        root.to_string_lossy().into_owned()
    }

    #[test]
    fn a_named_session_beats_a_stale_socket_path() {
        // The regression this guards: a shell can hold HERDR_SESSION for the
        // session in use AND a HERDR_SOCKET_PATH pointing at a different herdr
        // server. Trusting the path attaches the daemon to the wrong server,
        // where every rule silently watches the wrong panes.
        let _guard = env_lock();
        let root = temp_root("both");
        let expected = seed_session(&root, "com");
        let _config = EnvGuard::set("XDG_CONFIG_HOME", &root);
        let _session = EnvGuard::set("HERDR_SESSION", "com");
        let _socket = EnvGuard::set("HERDR_SOCKET_PATH", "/tmp/some-other-herdr.sock");
        assert_eq!(socket_path(), expected);
    }

    #[test]
    fn socket_path_is_used_when_the_named_session_has_no_socket() {
        // A name with nothing behind it must not shadow an explicit path.
        let _guard = env_lock();
        let root = temp_root("nosock");
        let _config = EnvGuard::set("XDG_CONFIG_HOME", &root);
        let _session = EnvGuard::set("HERDR_SESSION", "ghost");
        let _socket = EnvGuard::set("HERDR_SOCKET_PATH", "/tmp/explicit-herdr.sock");
        assert_eq!(socket_path(), PathBuf::from("/tmp/explicit-herdr.sock"));
    }

    #[test]
    fn named_session_resolves_to_the_session_socket() {
        let _guard = env_lock();
        let root = temp_root("named");
        let expected = seed_session(&root, "com");
        let _socket = EnvGuard::unset("HERDR_SOCKET_PATH");
        let _config = EnvGuard::set("XDG_CONFIG_HOME", &root);
        let _session = EnvGuard::set("HERDR_SESSION", "com");
        assert_eq!(socket_path(), expected);
    }

    #[test]
    fn default_session_resolves_to_the_config_dir_socket() {
        let _guard = env_lock();
        let _socket = EnvGuard::unset("HERDR_SOCKET_PATH");
        let _config = EnvGuard::set("XDG_CONFIG_HOME", "/tmp/xdg");
        let _session = EnvGuard::unset("HERDR_SESSION");
        assert_eq!(socket_path(), PathBuf::from("/tmp/xdg/herdr/herdr.sock"));
    }
}
