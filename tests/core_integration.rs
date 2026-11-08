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
