//! The real daemon against a fake herdr-core: one framed connection, a
//! scripted screen, and the bytes a rule types captured on the attach
//! channel. Also proves the daemon leaves when the core does.

use std::io::{BufRead, BufReader, Read, Write};
use std::os::unix::net::UnixListener;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use serde_json::{json, Value};

const BIN: &str = env!("CARGO_BIN_EXE_herdr-triggersd");
const HEADER: usize = 9;

fn encode(channel: u32, kind: u8, payload: &[u8]) -> Vec<u8> {
    let mut out = Vec::with_capacity(HEADER + payload.len());
    out.extend_from_slice(&channel.to_le_bytes());
    out.push(kind);
    out.extend_from_slice(&(payload.len() as u32).to_le_bytes());
    out.extend_from_slice(payload);
    out
}

/// `Ok(None)` on a clean close, `Err` on a timeout (the caller re-checks
/// its stop flag). The fake's reader is a BufReader over a blocking stream,
/// and a timeout only ever hits while waiting for the first header byte of
/// the next frame, because the daemon writes each frame in one `write_all`.
fn read_frame(r: &mut impl Read) -> std::io::Result<Option<(u32, u8, Vec<u8>)>> {
    let mut header = [0u8; HEADER];
    match r.read_exact(&mut header) {
        Ok(()) => {}
        Err(e) if e.kind() == std::io::ErrorKind::UnexpectedEof => return Ok(None),
        Err(e) => return Err(e),
    }
    let channel = u32::from_le_bytes([header[0], header[1], header[2], header[3]]);
    let len = u32::from_le_bytes([header[5], header[6], header[7], header[8]]) as usize;
    let mut payload = vec![0; len];
    r.read_exact(&mut payload)?;
    Ok(Some((channel, header[4], payload)))
}

#[derive(Default)]
struct Seen {
    typed: Vec<Vec<u8>>,
    methods: Vec<String>,
}

/// A herdr-core that owns one terminal `t1` showing `screen`, until `stop`.
fn fake_core(
    socket: &Path,
    screen: &str,
    seen: Arc<Mutex<Seen>>,
    stop: Arc<AtomicBool>,
) -> std::thread::JoinHandle<()> {
    let listener = UnixListener::bind(socket).unwrap();
    let screen = screen.to_string();
    std::thread::spawn(move || {
        let Ok((mut stream, _)) = listener.accept() else {
            return;
        };
        // Short read timeouts so the stop flag is checked between frames.
        stream
            .set_read_timeout(Some(Duration::from_millis(200)))
            .unwrap();
        let mut reader = BufReader::new(stream.try_clone().unwrap());
        let mut line = String::new();
        while reader.read_line(&mut line).is_err() {
            if stop.load(Ordering::SeqCst) {
                return;
            }
        }
        assert!(line.contains("\"hello\""), "{line}");
        let hello = json!({"jsonrpc": "2.0", "id": 0, "result": {"framed": true, "methods": ["terminal.list", "terminal.read", "terminal.attach", "events.subscribe"]}});
        stream.write_all(format!("{hello}\n").as_bytes()).unwrap();
        let record = json!({"address": "t1", "state": "live", "title": "login", "cols": 100, "rows": 30,
                            "labels": {"herdr.space": "sp1", "herdr.tab": "tb1", "where": "probe / 1"}});
        loop {
            if stop.load(Ordering::SeqCst) {
                return;
            }
            let (channel, kind, payload) = match read_frame(&mut reader) {
                Ok(Some(frame)) => frame,
                Ok(None) => return,
                Err(_) => continue, // timeout: check the flag again
            };
            match (channel, kind) {
                (0, 0) => {
                    let request: Value = serde_json::from_slice(&payload).unwrap();
                    let id = request["id"].clone();
                    let method = request["method"].as_str().unwrap_or_default().to_string();
                    seen.lock().unwrap().methods.push(method.clone());
                    let result = match method.as_str() {
                        "events.subscribe" => {
                            json!({"seq": 1, "terminals": [record], "channel": 5})
                        }
                        "terminal.read" => {
                            json!({"text": screen, "complete": true, "how": "screen"})
                        }
                        "terminal.attach" => {
                            json!({"channel": 9, "terminal": record, "resize_notifications": true, "automatic_input": true})
                        }
                        other => panic!("unexpected method {other}"),
                    };
                    let reply = json!({"jsonrpc": "2.0", "id": id, "result": result});
                    stream
                        .write_all(&encode(0, 0, reply.to_string().as_bytes()))
                        .unwrap();
                }
                (9, 0) => seen.lock().unwrap().typed.push(payload),
                (9, 2) => stream.write_all(&encode(9, 2, b"")).unwrap(),
                _ => {}
            }
        }
    })
}

fn wait_until(deadline: Duration, mut condition: impl FnMut() -> bool) -> bool {
    let until = Instant::now() + deadline;
    while Instant::now() < until {
        if condition() {
            return true;
        }
        std::thread::sleep(Duration::from_millis(20));
    }
    condition()
}

fn fresh_dir(tag: &str) -> PathBuf {
    let dir = std::env::temp_dir().join(format!("triggers-it-{tag}-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).unwrap();
    dir
}

#[test]
fn the_daemon_fires_once_through_a_fake_core_and_leaves_when_it_does() {
    let root = fresh_dir("core");
    let (config, state) = (root.join("config"), root.join("state"));
    std::fs::create_dir_all(&config).unwrap();
    std::fs::create_dir_all(&state).unwrap();
    std::fs::write(
        config.join("triggers.toml"),
        "[settings]\npoll_ms = 50\nfire_on_existing_text = true\n\n\
         [[rules]]\nregex = \"Password:\"\nonce = true\ntail_within = 1\n\
         action = { type = \"send_text\", text = \"hunter2\\n\" }\n",
    )
    .unwrap();
    let socket = root.join("default.sock");
    let seen = Arc::new(Mutex::new(Seen::default()));
    let stop = Arc::new(AtomicBool::new(false));
    let core = fake_core(&socket, "login:\nPassword:", seen.clone(), stop.clone());

    let mut daemon = Spawned(
        Command::new(BIN)
            .args(["--host", "core", "run", "--socket"])
            .arg(&socket)
            .env("HERDR_TRIGGERS_CONFIG_DIR", &config)
            .env("HERDR_TRIGGERS_STATE_DIR", &state)
            .env("HERDR_TRIGGERS_CORE_PATIENCE_MS", "2000")
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .spawn()
            .unwrap(),
    );

    assert!(
        wait_until(Duration::from_secs(5), || !seen
            .lock()
            .unwrap()
            .typed
            .is_empty()),
        "nothing was typed within 5 s; log: {}",
        std::fs::read_to_string(state.join("core-default").join("triggersd.log"))
            .unwrap_or_default()
    );
    assert_eq!(seen.lock().unwrap().typed, vec![b"hunter2\n".to_vec()]);

    // The prompt stays on screen: `once` and the latch keep it to one fire.
    std::thread::sleep(Duration::from_millis(500));
    assert_eq!(seen.lock().unwrap().typed.len(), 1);
    assert!(
        !seen
            .lock()
            .unwrap()
            .methods
            .iter()
            .any(|m| m == "terminal.list"),
        "the poll loop listed terminals"
    );

    let status = Command::new(BIN)
        .args(["--host", "core", "status", "--json", "--socket"])
        .arg(&socket)
        .env("HERDR_TRIGGERS_CONFIG_DIR", &config)
        .env("HERDR_TRIGGERS_STATE_DIR", &state)
        .output()
        .unwrap();
    let status: Value = serde_json::from_slice(&status.stdout).unwrap();
    assert_eq!(status["host"], "core");
    assert_eq!(status["running"], true);
    assert_eq!(status["rules"], 1);
    assert_eq!(status["last_fire"]["pane"], "t1");
    assert!(status["log"].as_array().is_some_and(|log| !log.is_empty()));

    // The core goes away: its thread ends (closing the connection), then
    // the socket file is removed so a reconnect is refused.
    stop.store(true, Ordering::SeqCst);
    core.join().unwrap();
    std::fs::remove_file(&socket).unwrap();
    let exited = wait_until(Duration::from_secs(10), || {
        daemon.try_wait().unwrap().is_some()
    });
    if !exited {
        let _ = daemon.kill();
    }
    assert!(
        exited,
        "the daemon did not exit within 10 s of the core leaving"
    );
    let log = std::fs::read_to_string(state.join("core-default").join("triggersd.log")).unwrap();
    assert!(log.contains("server is gone"), "{log}");
    std::fs::remove_dir_all(root).unwrap();
}

/// The binary with every path it could touch pointed into `root`, and no
/// ambient core or app.
fn isolated(root: &Path) -> Command {
    let mut cmd = Command::new(BIN);
    cmd.env("PATH", stand_in_managers(root))
        // Absent unless a test makes it: "systemd does not run here".
        .env("HERDR_TRIGGERS_SYSTEMD_DIR", root.join("run-systemd"))
        .env("HOME", root.join("home"))
        .env("XDG_CONFIG_HOME", root.join("home/.config"))
        .env("XDG_STATE_HOME", root.join("home/.local/state"))
        .env("HERDR_APP_HOME", root.join("home/.config/herdr-app"))
        .env("HERDR_TRIGGERS_CONFIG_DIR", root.join("config"))
        .env("HERDR_TRIGGERS_STATE_DIR", root.join("state"))
        .env_remove("HC_SOCKET")
        .env_remove("HC_SESSION")
        .env_remove("HERDR_PLUGIN_CONFIG_DIR")
        .env_remove("XDG_RUNTIME_DIR");
    cmd
}

/// PATH with `systemctl`, `busctl` and `launchctl` replaced by scripts that
/// log their arguments to `<root>/managers.log` and fail, so no test reaches
/// the real service managers. A script already in `<root>/stand-in-bin` is
/// kept.
fn stand_in_managers(root: &Path) -> std::ffi::OsString {
    use std::os::unix::fs::PermissionsExt;
    let bin = root.join("stand-in-bin");
    std::fs::create_dir_all(&bin).unwrap();
    for name in ["systemctl", "busctl", "launchctl"] {
        let script = bin.join(name);
        // A test may plant its own stand-in first.
        if script.exists() {
            continue;
        }
        std::fs::write(
            &script,
            format!(
                "#!/bin/sh\necho \"{name} $*\" >> '{}'\nexit 1\n",
                root.join("managers.log").display()
            ),
        )
        .unwrap();
        std::fs::set_permissions(&script, std::fs::Permissions::from_mode(0o755)).unwrap();
    }
    let mut path = std::ffi::OsString::from(&bin);
    path.push(":");
    path.push(std::env::var_os("PATH").unwrap_or_default());
    path
}

/// A stand-in `busctl` that answers the `UnitPath` query with
/// `~/.config/systemd/user` (and its `.control` sibling), as a reachable
/// systemd user manager does. Without it the stand-in fails, which reads as
/// no manager at all.
fn stand_in_busctl_answering(root: &Path) {
    use std::os::unix::fs::PermissionsExt;
    let bin = root.join("stand-in-bin");
    std::fs::create_dir_all(&bin).unwrap();
    let dir = root.join("home/.config/systemd/user");
    let answer = serde_json::json!({
        "type": "as",
        "data": [format!("{}.control", dir.display()), dir.display().to_string()]
    });
    let script = bin.join("busctl");
    std::fs::write(
        &script,
        format!(
            "#!/bin/sh\necho \"busctl $*\" >> '{}'\necho '{answer}'\n",
            root.join("managers.log").display()
        ),
    )
    .unwrap();
    std::fs::set_permissions(&script, std::fs::Permissions::from_mode(0o755)).unwrap();
}

fn status_json(root: &Path, socket: &Path) -> Value {
    let out = isolated(root)
        .args(["--host", "core", "status", "--json", "--socket"])
        .arg(socket)
        .output()
        .unwrap();
    serde_json::from_slice(&out.stdout).unwrap()
}

fn service_root(tag: &str) -> (PathBuf, PathBuf, PathBuf) {
    let root = fresh_dir(tag);
    let config = root.join("config");
    std::fs::create_dir_all(&config).unwrap();
    std::fs::write(
        config.join("triggers.toml"),
        "[settings]\npoll_ms = 50\nfire_on_existing_text = true\n\n\
         [[rules]]\nregex = \"Password:\"\nonce = true\ntail_within = 1\n\
         action = { type = \"send_text\", text = \"hunter2\\n\" }\n",
    )
    .unwrap();
    let state = root.join("state").join("core-default");
    let socket = root.join("default.sock");
    (root, state, socket)
}

/// A daemon a test started, killed when dropped: a failed assertion must
/// not leave it running, waiting on a socket that is already deleted.
struct Spawned(std::process::Child);

impl Drop for Spawned {
    fn drop(&mut self) {
        if matches!(self.0.try_wait(), Ok(None)) {
            let _ = self.0.kill();
            let _ = self.0.wait();
        }
    }
}

impl std::ops::Deref for Spawned {
    type Target = std::process::Child;
    fn deref(&self) -> &Self::Target {
        &self.0
    }
}

impl std::ops::DerefMut for Spawned {
    fn deref_mut(&mut self) -> &mut Self::Target {
        &mut self.0
    }
}

fn spawn_service(root: &Path, socket: &Path) -> Spawned {
    Spawned(
        isolated(root)
            .args(["--host", "core", "service", "--socket"])
            .arg(socket)
            .env("HERDR_TRIGGERS_CORE_PATIENCE_MS", "1000")
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .spawn()
            .unwrap(),
    )
}

fn stop_service(root: &Path, socket: &Path, daemon: &mut std::process::Child) {
    let stop = isolated(root)
        .args(["--host", "core", "stop", "--socket"])
        .arg(socket)
        .output()
        .unwrap();
    assert!(stop.status.success(), "{stop:?}");
    let exited = wait_until(Duration::from_secs(5), || {
        daemon.try_wait().unwrap().is_some()
    });
    if !exited {
        let _ = daemon.kill();
    }
    assert!(exited, "the service did not exit after stop");
    assert_eq!(daemon.wait().unwrap().code(), Some(0));
}

#[test]
fn the_service_waits_for_core_attaches_and_reattaches_after_it_leaves() {
    let (root, state, socket) = service_root("service");
    let log = || std::fs::read_to_string(state.join("triggersd.log")).unwrap_or_default();
    let marker = state.join("triggersd.waiting");
    let mut daemon = spawn_service(&root, &socket);

    assert!(
        wait_until(Duration::from_secs(5), || marker.exists()),
        "no waiting marker; log: {}",
        log()
    );
    let waiting = status_json(&root, &socket);
    assert_eq!(waiting["running"], true);
    assert_eq!(waiting["waiting"], true);
    assert_eq!(waiting["service"]["installed"], false);
    let tui = isolated(&root)
        .args(["--host", "herdr", "status", "--json"])
        .env("HERDR_PLUGIN_STATE_DIR", root.join("tui-state"))
        .env_remove("HERDR_SOCKET_PATH")
        .env_remove("HERDR_SESSION")
        .output()
        .unwrap();
    let tui: Value = serde_json::from_slice(&tui.stdout).unwrap();
    assert_eq!(tui["host"], "herdr");
    assert!(tui["service"].is_null(), "{tui}");
    assert_eq!(
        std::fs::read_to_string(state.join("triggersd.mode")).unwrap(),
        "service\n"
    );

    let seen = Arc::new(Mutex::new(Seen::default()));
    let stop = Arc::new(AtomicBool::new(false));
    let core = fake_core(&socket, "login:\nPassword:", seen.clone(), stop.clone());
    assert!(
        wait_until(Duration::from_secs(6), || !seen
            .lock()
            .unwrap()
            .typed
            .is_empty()),
        "nothing typed after core appeared; log: {}",
        log()
    );
    assert!(!marker.exists());
    assert_eq!(status_json(&root, &socket)["waiting"], false);

    // Core leaves: the service goes back to waiting instead of exiting.
    stop.store(true, Ordering::SeqCst);
    core.join().unwrap();
    std::fs::remove_file(&socket).unwrap();
    assert!(
        wait_until(Duration::from_secs(10), || marker.exists()),
        "no waiting marker after core left; log: {}",
        log()
    );
    assert!(daemon.try_wait().unwrap().is_none(), "the service exited");
    assert!(
        log().contains("herdr-core gone; waiting for it"),
        "{}",
        log()
    );

    let again = Arc::new(Mutex::new(Seen::default()));
    let stop_again = Arc::new(AtomicBool::new(false));
    let core = fake_core(
        &socket,
        "login:\nPassword:",
        again.clone(),
        stop_again.clone(),
    );
    assert!(
        wait_until(Duration::from_secs(6), || again
            .lock()
            .unwrap()
            .methods
            .iter()
            .any(|m| m == "events.subscribe")),
        "no re-attach to the new core; log: {}",
        log()
    );
    assert!(wait_until(Duration::from_secs(2), || !marker.exists()));

    stop_service(&root, &socket, &mut daemon);
    stop_again.store(true, Ordering::SeqCst);
    core.join().unwrap();
    assert!(!state.join("triggersd.pid").exists());
    assert!(!marker.exists());
    std::fs::remove_dir_all(root).unwrap();
}

#[test]
fn a_stop_while_the_service_waits_exits_promptly() {
    let (root, state, socket) = service_root("service-stop");
    let marker = state.join("triggersd.waiting");
    let mut daemon = spawn_service(&root, &socket);
    assert!(wait_until(Duration::from_secs(5), || marker.exists()));

    // A second service for the same core waits behind the lock, and a stop
    // ends that wait promptly too.
    let mut second = spawn_service(&root, &socket);
    let log = || std::fs::read_to_string(state.join("triggersd.log")).unwrap_or_default();
    assert!(
        wait_until(Duration::from_secs(5), || log()
            .contains("another herdr-triggersd holds the lock; waiting for it")),
        "{}",
        log()
    );
    assert!(
        second.try_wait().unwrap().is_none(),
        "the second service exited"
    );
    let at = Instant::now();
    assert_eq!(unsafe { libc::kill(second.id() as i32, libc::SIGTERM) }, 0);
    assert!(wait_until(Duration::from_secs(2), || second
        .try_wait()
        .unwrap()
        .is_some()));
    assert!(at.elapsed() < Duration::from_secs(1), "{:?}", at.elapsed());
    assert_eq!(second.wait().unwrap().code(), Some(0));

    let at = Instant::now();
    stop_service(&root, &socket, &mut daemon);
    assert!(at.elapsed() < Duration::from_secs(2), "{:?}", at.elapsed());
    assert!(!marker.exists());
    assert!(!state.join("triggersd.pid").exists());
    std::fs::remove_dir_all(root).unwrap();
}

#[test]
fn a_service_behind_a_held_lock_waits_and_takes_over_when_it_is_released() {
    use std::os::fd::AsRawFd;
    let (root, state, socket) = service_root("service-lock");
    std::fs::create_dir_all(&state).unwrap();
    let held = std::fs::OpenOptions::new()
        .create(true)
        .truncate(false)
        .write(true)
        .open(state.join("triggersd.lock"))
        .unwrap();
    assert_eq!(
        unsafe { libc::flock(held.as_raw_fd(), libc::LOCK_EX | libc::LOCK_NB) },
        0
    );
    let log = || std::fs::read_to_string(state.join("triggersd.log")).unwrap_or_default();
    let mut daemon = spawn_service(&root, &socket);
    assert!(
        wait_until(Duration::from_secs(5), || log()
            .contains("another herdr-triggersd holds the lock; waiting for it")),
        "{}",
        log()
    );
    std::thread::sleep(Duration::from_millis(300));
    assert!(daemon.try_wait().unwrap().is_none(), "the service exited");
    assert!(!state.join("triggersd.mode").exists());
    assert!(!state.join("triggersd.pid").exists());
    assert_eq!(log().matches("holds the lock").count(), 1, "{}", log());

    drop(held);
    assert!(
        wait_until(Duration::from_secs(5), || std::fs::read_to_string(
            state.join("triggersd.mode")
        )
        .is_ok_and(|mode| mode == "service\n")),
        "the service did not take the lock; log: {}",
        log()
    );
    assert_eq!(
        std::fs::read_to_string(state.join("triggersd.pid")).unwrap(),
        format!("{}\n", daemon.id())
    );
    assert_eq!(status_json(&root, &socket)["pid"], daemon.id());
    stop_service(&root, &socket, &mut daemon);
    std::fs::remove_dir_all(root).unwrap();
}

#[test]
fn install_without_a_terminal_prints_the_plan_and_changes_nothing() {
    let root = fresh_dir("install");
    let home = root.join("home");
    std::fs::create_dir_all(&home).unwrap();

    let dry = isolated(&root)
        .args(["install", "--dry-run"])
        .stdin(Stdio::null())
        .output()
        .unwrap();
    assert_eq!(dry.status.code(), Some(0), "{dry:?}");
    let plan = String::from_utf8_lossy(&dry.stdout);
    assert!(
        plan.contains("create") && plan.contains("mod main.js"),
        "{plan}"
    );
    assert!(plan.contains("key ctrl-alt-g"), "{plan}");
    assert!(
        plan.contains("keys.json does not exist yet: open the Herdr app once"),
        "{plan}"
    );

    let asked = isolated(&root)
        .arg("install")
        .stdin(Stdio::null())
        .output()
        .unwrap();
    assert_eq!(asked.status.code(), Some(2), "{asked:?}");
    assert_eq!(
        String::from_utf8_lossy(&asked.stderr),
        "not a terminal: pass --yes to apply\n"
    );

    let tui = isolated(&root)
        .args(["--host", "herdr", "install"])
        .stdin(Stdio::null())
        .output()
        .unwrap();
    assert_eq!(tui.status.code(), Some(2));

    for name in [
        "XDG_CONFIG_HOME",
        "XDG_STATE_HOME",
        "HERDR_TRIGGERS_CONFIG_DIR",
        "HERDR_TRIGGERS_STATE_DIR",
        "HERDR_APP_HOME",
    ] {
        let relative = isolated(&root)
            .args(["install", "--dry-run"])
            .env(name, "rel/dir")
            .stdin(Stdio::null())
            .output()
            .unwrap();
        assert_eq!(relative.status.code(), Some(1), "{name}: {relative:?}");
        assert_eq!(
            String::from_utf8_lossy(&relative.stderr),
            format!("error: {name} must be absolute, not rel/dir\n")
        );
        assert!(relative.stdout.is_empty(), "{name}: planned anyway");
    }

    let empty: Vec<_> = walk(&home);
    assert!(empty.is_empty(), "install wrote {empty:?}");
    assert!(!root.join("state").exists());
    std::fs::remove_dir_all(root).unwrap();
}

/// Holds `<state>/core-<tag>/triggersd.lock` and names `pid` in the pidfile,
/// as a running daemon for that core would.
fn fake_daemon(root: &Path, tag: &str, pid: i32) -> std::fs::File {
    use std::os::fd::AsRawFd;
    let state = root.join("state").join(format!("core-{tag}"));
    std::fs::create_dir_all(&state).unwrap();
    std::fs::write(state.join("triggersd.pid"), format!("{pid}\n")).unwrap();
    std::fs::write(state.join("triggersd.mode"), "standalone\n").unwrap();
    let lock = std::fs::OpenOptions::new()
        .create(true)
        .truncate(false)
        .write(true)
        .open(state.join("triggersd.lock"))
        .unwrap();
    assert_eq!(
        unsafe { libc::flock(lock.as_raw_fd(), libc::LOCK_EX | libc::LOCK_NB) },
        0
    );
    lock
}

#[test]
fn install_looks_for_the_service_cores_daemon_from_any_core_terminal() {
    let root = fresh_dir("install-core");
    std::fs::create_dir_all(root.join("home")).unwrap();
    // A daemon to stop matters only when there is a service to replace it.
    stand_in_busctl_answering(&root);
    let dry_run = |session: Option<&str>| {
        let mut cmd = isolated(&root);
        cmd.args(["install", "--dry-run"]).stdin(Stdio::null());
        if let Some(session) = session {
            cmd.env("HC_SESSION", session);
        }
        let out = cmd.output().unwrap();
        assert_eq!(out.status.code(), Some(0), "{out:?}");
        String::from_utf8_lossy(&out.stdout).to_string()
    };

    let default = fake_daemon(&root, "default", 4242);
    let plan = dry_run(Some("work"));
    assert!(
        plan.contains("running daemon") && plan.contains("pid 4242"),
        "{plan}"
    );
    drop(default);
    std::fs::remove_dir_all(root.join("state")).unwrap();

    let work = fake_daemon(&root, "work", 4343);
    for session in [Some("work"), None] {
        let plan = dry_run(session);
        assert!(!plan.contains("running daemon"), "{plan}");
    }
    drop(work);

    // status says whether the service watches the core it resolved.
    let this_core = |session: Option<&str>| {
        let mut cmd = isolated(&root);
        cmd.args(["--host", "core", "status", "--json"]);
        if let Some(session) = session {
            cmd.env("HC_SESSION", session);
        }
        let out: Value = serde_json::from_slice(&cmd.output().unwrap().stdout).unwrap();
        out["service"]["this_core"].clone()
    };
    assert_eq!(this_core(None), true);
    assert_eq!(this_core(Some("work")), false);
    std::fs::remove_dir_all(root).unwrap();
}

#[test]
fn stopping_a_stray_daemon_lets_a_waiting_service_take_over() {
    let (root, state, socket) = service_root("service-takeover");
    let mut stray = Spawned(
        isolated(&root)
            .args(["--host", "core", "run", "--socket"])
            .arg(&socket)
            .env("HERDR_TRIGGERS_CORE_PATIENCE_MS", "60000")
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .spawn()
            .unwrap(),
    );
    let pid_is = |pid: u32| {
        std::fs::read_to_string(state.join("triggersd.pid")).is_ok_and(|p| p == format!("{pid}\n"))
    };
    assert!(wait_until(Duration::from_secs(5), || pid_is(stray.id())));
    let log = || std::fs::read_to_string(state.join("triggersd.log")).unwrap_or_default();
    let mut service = spawn_service(&root, &socket);
    assert!(
        wait_until(Duration::from_secs(5), || log()
            .contains("another herdr-triggersd holds the lock; waiting for it")),
        "{}",
        log()
    );

    let stop = isolated(&root)
        .args(["--host", "core", "stop", "--socket"])
        .arg(&socket)
        .output()
        .unwrap();
    assert_eq!(stop.status.code(), Some(0), "{stop:?}");
    assert!(wait_until(Duration::from_secs(5), || stray
        .try_wait()
        .unwrap()
        .is_some()));
    assert!(
        wait_until(Duration::from_secs(5), || pid_is(service.id())),
        "the service did not take over; log: {}",
        log()
    );
    assert_eq!(
        std::fs::read_to_string(state.join("triggersd.mode")).unwrap(),
        "service\n"
    );
    stop_service(&root, &socket, &mut service);
    std::fs::remove_dir_all(root).unwrap();
}

#[test]
fn the_service_watches_the_default_session_whatever_hc_session_says() {
    let (root, _, _) = service_root("service-default");
    let run = root.join("run");
    std::fs::create_dir_all(run.join("herdr-core")).unwrap();
    let mut daemon = Spawned(
        isolated(&root)
            .args(["--host", "core", "service"])
            .env("XDG_RUNTIME_DIR", &run)
            .env("HC_SESSION", "work")
            .env("HC_SOCKET", run.join("herdr-core/work.sock"))
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .spawn()
            .unwrap(),
    );
    let default = root.join("state/core-default");
    assert!(
        wait_until(Duration::from_secs(5), || std::fs::read_to_string(
            default.join("triggersd.mode")
        )
        .is_ok_and(|mode| mode == "service\n")),
        "no service state in core-default"
    );
    assert!(!root.join("state/core-work").exists());
    let log = || std::fs::read_to_string(default.join("triggersd.log")).unwrap_or_default();
    assert!(
        wait_until(Duration::from_secs(5), || log()
            .contains("herdr-core/default.sock does not answer")),
        "{}",
        log()
    );
    stop_service(&root, &run.join("herdr-core/default.sock"), &mut daemon);
    std::fs::remove_dir_all(root).unwrap();
}

/// A detached daemon a test started through `start`: not a child, so it is
/// found by its lock and pidfile and stopped when this is dropped.
#[cfg(target_os = "linux")]
struct DetachedDaemon(PathBuf);

#[cfg(target_os = "linux")]
impl Drop for DetachedDaemon {
    fn drop(&mut self) {
        use std::os::fd::AsRawFd;
        let Ok(lock) = std::fs::OpenOptions::new()
            .write(true)
            .open(self.0.join("triggersd.lock"))
        else {
            return;
        };
        // The lock taken means no daemon holds it.
        if unsafe { libc::flock(lock.as_raw_fd(), libc::LOCK_EX | libc::LOCK_NB) } == 0 {
            return;
        }
        let pid = std::fs::read_to_string(self.0.join("triggersd.pid"))
            .ok()
            .and_then(|text| text.trim().parse::<i32>().ok());
        if let Some(pid) = pid {
            unsafe { libc::kill(pid, libc::SIGKILL) };
        }
    }
}

// The service file it plants is a systemd unit; on macOS `start` would look
// for the launchd plist and never delegate.
#[cfg(target_os = "linux")]
#[test]
fn start_falls_back_to_a_detached_daemon_when_the_manager_refuses() {
    let (root, state, _) = service_root("start-fallback");
    let run = root.join("run");
    std::fs::create_dir_all(run.join("herdr-core")).unwrap();
    let unit = root.join("home/.config/systemd/user/herdr-triggers.service");
    std::fs::create_dir_all(unit.parent().unwrap()).unwrap();
    std::fs::write(&unit, "[Service]\n").unwrap();
    stand_in_busctl_answering(&root);

    let _detached = DetachedDaemon(state.clone());
    let start = isolated(&root)
        .args(["--host", "core", "start"])
        .env("XDG_RUNTIME_DIR", &run)
        .env("HERDR_TRIGGERS_CORE_PATIENCE_MS", "60000")
        .stdin(Stdio::null())
        .output()
        .unwrap();
    assert_eq!(start.status.code(), Some(0), "{start:?}");
    let err = String::from_utf8_lossy(&start.stderr);
    assert!(
        err.contains("systemctl --user start herdr-triggers.service failed")
            && err.contains("starting a detached daemon instead"),
        "{err}"
    );
    assert!(wait_until(Duration::from_secs(5), || {
        std::fs::read_to_string(state.join("triggersd.mode"))
            .is_ok_and(|mode| mode == "standalone\n")
    }));
    let managers = std::fs::read_to_string(root.join("managers.log")).unwrap();
    assert_eq!(
        managers,
        "busctl --user --json=short get-property org.freedesktop.systemd1 \
         /org/freedesktop/systemd1 org.freedesktop.systemd1.Manager UnitPath\n\
         systemctl --user start herdr-triggers.service\n"
    );

    // Restart: the manager's stop fails (a warning), the detached daemon is
    // stopped all the same, and with nothing left running the start goes
    // ahead and falls back again.
    let first = std::fs::read_to_string(state.join("triggersd.pid")).unwrap();
    let restart = isolated(&root)
        .args(["--host", "core", "restart"])
        .env("XDG_RUNTIME_DIR", &run)
        .env("HERDR_TRIGGERS_CORE_PATIENCE_MS", "60000")
        .stdin(Stdio::null())
        .output()
        .unwrap();
    assert_eq!(restart.status.code(), Some(0), "{restart:?}");
    let err = String::from_utf8_lossy(&restart.stderr);
    assert!(
        err.contains("warning: systemctl --user stop herdr-triggers.service failed")
            && err.contains("starting a detached daemon instead"),
        "{err}"
    );
    assert!(
        wait_until(Duration::from_secs(5), || {
            std::fs::read_to_string(state.join("triggersd.pid")).is_ok_and(|pid| pid != first)
                && std::fs::read_to_string(state.join("triggersd.mode"))
                    .is_ok_and(|mode| mode == "standalone\n")
        }),
        "no fresh daemon after restart"
    );

    // Stop asks the manager first, which fails here. The detached daemon is
    // not the service's, so the manager cannot bring it back: it is stopped
    // anyway, and the exit reports the manager's failure.
    let stop = isolated(&root)
        .args(["--host", "core", "stop"])
        .env("XDG_RUNTIME_DIR", &run)
        .output()
        .unwrap();
    assert_eq!(stop.status.code(), Some(1), "{stop:?}");
    assert!(String::from_utf8_lossy(&stop.stderr)
        .contains("systemctl --user stop herdr-triggers.service failed"));
    assert!(!state.join("triggersd.pid").exists(), "{stop:?}");
    std::fs::remove_dir_all(root).unwrap();
}

/// A stand-in `systemctl` whose `stop` of the unit ends the daemon whose pid
/// is in `state`, as the real manager would, and fails everything else.
#[cfg(target_os = "linux")]
fn stand_in_systemctl_that_stops(root: &Path, state: &Path) {
    use std::os::unix::fs::PermissionsExt;
    let bin = root.join("stand-in-bin");
    std::fs::create_dir_all(&bin).unwrap();
    let script = bin.join("systemctl");
    std::fs::write(
        &script,
        format!(
            "#!/bin/sh\necho \"systemctl $*\" >> '{log}'\n\
             if [ \"$*\" = \"--user stop herdr-triggers.service\" ]; then\n\
             kill \"$(cat '{pid}')\"\nexit 0\nfi\nexit 1\n",
            log = root.join("managers.log").display(),
            pid = state.join("triggersd.pid").display()
        ),
    )
    .unwrap();
    std::fs::set_permissions(&script, std::fs::Permissions::from_mode(0o755)).unwrap();
}

// The planted service file is a systemd unit.
#[cfg(target_os = "linux")]
#[test]
fn stop_goes_through_the_manager_when_the_service_is_installed() {
    let (root, state, _) = service_root("stop-manager");
    let run = root.join("run");
    std::fs::create_dir_all(run.join("herdr-core")).unwrap();
    let unit = root.join("home/.config/systemd/user/herdr-triggers.service");
    std::fs::create_dir_all(unit.parent().unwrap()).unwrap();
    std::fs::write(&unit, "[Service]\n").unwrap();
    stand_in_systemctl_that_stops(&root, &state);
    stand_in_busctl_answering(&root);
    let service = || {
        Spawned(
            isolated(&root)
                .args(["--host", "core", "service"])
                .env("XDG_RUNTIME_DIR", &run)
                .stdin(Stdio::null())
                .stdout(Stdio::null())
                .stderr(Stdio::null())
                .spawn()
                .unwrap(),
        )
    };
    let running_as_service = |pid: u32| {
        wait_until(Duration::from_secs(5), || {
            std::fs::read_to_string(state.join("triggersd.pid"))
                .is_ok_and(|p| p == format!("{pid}\n"))
                && std::fs::read_to_string(state.join("triggersd.mode"))
                    .is_ok_and(|mode| mode == "service\n")
        })
    };
    let stop = || {
        isolated(&root)
            .args(["--host", "core", "stop"])
            .env("XDG_RUNTIME_DIR", &run)
            .output()
            .unwrap()
    };

    let mut daemon = service();
    assert!(running_as_service(daemon.id()));
    let stopped = stop();
    assert_eq!(stopped.status.code(), Some(0), "{stopped:?}");
    assert!(String::from_utf8_lossy(&stopped.stdout)
        .contains("stopped the systemd service (systemctl --user stop herdr-triggers.service)"));
    assert_eq!(daemon.wait().unwrap().code(), Some(0));
    let managers = std::fs::read_to_string(root.join("managers.log")).unwrap();
    assert!(
        managers.ends_with("systemctl --user stop herdr-triggers.service\n"),
        "{managers}"
    );

    // Nothing running: the manager is still asked, so a service in its
    // restart delay stays stopped.
    let idle = stop();
    assert_eq!(idle.status.code(), Some(0), "{idle:?}");
    assert!(String::from_utf8_lossy(&idle.stdout)
        .contains("herdr-triggersd is not running; the systemd service stays stopped"));

    // The manager fails: its daemon is not signalled, since it would only be
    // started again.
    std::fs::remove_file(root.join("stand-in-bin/systemctl")).unwrap();
    let mut daemon = service();
    assert!(running_as_service(daemon.id()));
    let refused = stop();
    assert_eq!(refused.status.code(), Some(1), "{refused:?}");
    assert!(String::from_utf8_lossy(&refused.stderr).contains(&format!(
        "not signalling pid {}: the systemd service would start it again",
        daemon.id()
    )));
    std::thread::sleep(Duration::from_millis(300));
    assert!(
        daemon.try_wait().unwrap().is_none(),
        "the service was stopped"
    );
    drop(daemon);
    std::fs::remove_dir_all(root).unwrap();
}

// The planted service file is a systemd unit.
#[cfg(target_os = "linux")]
#[test]
fn without_a_systemd_user_manager_the_unit_file_is_no_service() {
    let root = fresh_dir("no-manager");
    let unit = root.join("home/.config/systemd/user/herdr-triggers.service");
    std::fs::create_dir_all(unit.parent().unwrap()).unwrap();
    std::fs::write(&unit, "[Service]\n").unwrap();
    let run = root.join("run");
    std::fs::create_dir_all(run.join("herdr-core")).unwrap();

    let status = isolated(&root)
        .args(["--host", "core", "status"])
        .env("XDG_RUNTIME_DIR", &run)
        .output()
        .unwrap();
    assert!(
        String::from_utf8_lossy(&status.stdout).contains("service: not installed"),
        "{status:?}"
    );

    let plan = isolated(&root)
        .args(["install", "--dry-run"])
        .stdin(Stdio::null())
        .output()
        .unwrap();
    assert_eq!(plan.status.code(), Some(0), "{plan:?}");
    let plan = String::from_utf8_lossy(&plan.stdout);
    assert!(
        plan.contains("skip") && plan.contains("no systemd user manager answers ("),
        "{plan}"
    );
    let undo = isolated(&root)
        .args(["uninstall", "--dry-run"])
        .stdin(Stdio::null())
        .output()
        .unwrap();
    let undo = String::from_utf8_lossy(&undo.stdout);
    assert!(undo.contains("so the file is only removed"), "{undo}");
    let managers = std::fs::read_to_string(root.join("managers.log")).unwrap();
    assert!(
        managers.lines().all(|line| line.starts_with("busctl ")),
        "{managers}"
    );
    std::fs::remove_dir_all(root).unwrap();
}

// The planted service file is a systemd unit.
#[cfg(target_os = "linux")]
#[test]
fn a_service_out_of_reach_is_neither_signalled_nor_replaced() {
    let (root, state, _) = service_root("unreachable");
    std::fs::create_dir_all(root.join("run-systemd")).unwrap();
    let run = root.join("run");
    std::fs::create_dir_all(run.join("herdr-core")).unwrap();
    let unit = root.join("home/.config/systemd/user/herdr-triggers.service");
    std::fs::create_dir_all(unit.parent().unwrap()).unwrap();
    std::fs::write(&unit, "[Service]\n").unwrap();
    let cli = |args: &[&str]| {
        isolated(&root)
            .args(["--host", "core"])
            .args(args)
            .env("XDG_RUNTIME_DIR", &run)
            .stdin(Stdio::null())
            .output()
            .unwrap()
    };
    let unreachable = "cannot reach the systemd user manager from this session (";

    // Nothing running: start refuses rather than detach beside the service.
    let start = cli(&["start"]);
    assert_eq!(start.status.code(), Some(1), "{start:?}");
    assert!(String::from_utf8_lossy(&start.stderr).contains(unreachable));
    assert!(!state.join("triggersd.pid").exists());

    let mut daemon = Spawned(
        isolated(&root)
            .args(["--host", "core", "service"])
            .env("XDG_RUNTIME_DIR", &run)
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .spawn()
            .unwrap(),
    );
    assert!(wait_until(Duration::from_secs(5), || {
        std::fs::read_to_string(state.join("triggersd.mode")).is_ok_and(|m| m == "service\n")
    }));
    for command in ["stop", "restart"] {
        let out = cli(&[command]);
        assert_eq!(out.status.code(), Some(1), "{command}: {out:?}");
        let err = String::from_utf8_lossy(&out.stderr);
        assert!(
            err.contains(&format!(
                "not signalling pid {}: the systemd service would start it again",
                daemon.id()
            )) && err.contains(unreachable),
            "{command}: {err}"
        );
    }
    std::thread::sleep(Duration::from_millis(300));
    assert!(
        daemon.try_wait().unwrap().is_none(),
        "the service was signalled"
    );

    let status = cli(&["status", "--json"]);
    let status: Value = serde_json::from_slice(&status.stdout).unwrap();
    assert_eq!(status["service"]["installed"], true);
    assert_eq!(status["service"]["manager"], "systemd");
    assert_eq!(status["service"]["reachable"], false);
    let human = String::from_utf8_lossy(&cli(&["status"]).stdout).to_string();
    assert!(
        human.contains("service: systemd installed (manager not reachable from this session: "),
        "{human}"
    );

    let install = isolated(&root)
        .args(["install", "--dry-run"])
        .stdin(Stdio::null())
        .output()
        .unwrap();
    let install = String::from_utf8_lossy(&install.stdout);
    assert!(
        install.contains(unreachable) && install.contains("run install from your desktop session"),
        "{install}"
    );
    let undo = isolated(&root)
        .args(["uninstall", "--yes"])
        .stdin(Stdio::null())
        .output()
        .unwrap();
    assert_eq!(undo.status.code(), Some(0), "{undo:?}");
    assert!(String::from_utf8_lossy(&undo.stdout)
        .contains("run uninstall from your desktop session to stop and remove the service"));
    assert!(unit.exists(), "uninstall removed the unit");
    let managers = std::fs::read_to_string(root.join("managers.log")).unwrap();
    assert!(
        managers.lines().all(|line| line.starts_with("busctl ")),
        "{managers}"
    );
    drop(daemon);
    std::fs::remove_dir_all(root).unwrap();
}

fn walk(dir: &Path) -> Vec<PathBuf> {
    let mut found = Vec::new();
    for entry in std::fs::read_dir(dir).into_iter().flatten().flatten() {
        let path = entry.path();
        if path.is_dir() {
            found.extend(walk(&path));
        } else {
            found.push(path);
        }
    }
    found
}
