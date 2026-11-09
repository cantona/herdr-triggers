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

    let mut daemon = Command::new(BIN)
        .args(["--host", "core", "run", "--socket"])
        .arg(&socket)
        .env("HERDR_TRIGGERS_CONFIG_DIR", &config)
        .env("HERDR_TRIGGERS_STATE_DIR", &state)
        .env("HERDR_TRIGGERS_CORE_PATIENCE_MS", "2000")
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .spawn()
        .unwrap();

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
    cmd.env("HOME", root.join("home"))
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

fn spawn_service(root: &Path, socket: &Path) -> std::process::Child {
    isolated(root)
        .args(["--host", "core", "service", "--socket"])
        .arg(socket)
        .env("HERDR_TRIGGERS_CORE_PATIENCE_MS", "1000")
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .spawn()
        .unwrap()
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
fn the_service_watches_the_default_session_whatever_hc_session_says() {
    let (root, _, _) = service_root("service-default");
    let run = root.join("run");
    std::fs::create_dir_all(run.join("herdr-core")).unwrap();
    let mut daemon = isolated(&root)
        .args(["--host", "core", "service"])
        .env("XDG_RUNTIME_DIR", &run)
        .env("HC_SESSION", "work")
        .env("HC_SOCKET", run.join("herdr-core/work.sock"))
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .spawn()
        .unwrap();
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
