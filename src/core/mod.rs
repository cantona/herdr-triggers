//! The herdr-core host: the desktop app's terminals, reached over
//! herdr-core's unix socket instead of herdr's.
//!
//! One framed connection carries everything. On connect the daemon calls
//! `events.subscribe`, keeps the terminal table from the snapshot and the
//! pushed events, reads screens with `terminal.read`, and types through
//! `terminal.attach` as raw DATA frames - `terminal.send` would append Enter
//! and wait seconds for quiet. A transport failure drops the connection and
//! the next call reconnects; after `patience` of continuous failure the
//! error turns into `Gone` and the engine exits.

pub mod paths;
pub mod rpc;
pub mod terminals;
pub mod wire;

use std::path::PathBuf;
use std::sync::Mutex;
use std::time::{Duration, Instant};

use serde_json::{json, Value};

use crate::client::ClientError;
use crate::server::{desktop_notification_command, Notification, PaneInfo, Server, ServerError};
use rpc::Connection;
use terminals::Table;

const CALL_TIMEOUT: Duration = Duration::from_secs(2);
const DEFAULT_PATIENCE: Duration = Duration::from_secs(30);
/// How often a daemon that follows the app looks for it, and how long the
/// app may be unreachable (an update restarts it) before the daemon leaves.
const APP_CHECK_EVERY: Duration = Duration::from_secs(5);
const APP_PATIENCE: Duration = Duration::from_secs(10);

/// Follows the Herdr desktop app: the daemon it started leaves once the app
/// has quit, without the app having to stop it.
struct AppWatch {
    socket: PathBuf,
    every: Duration,
    patience: Duration,
    state: Mutex<AppState>,
}

#[derive(Default)]
struct AppState {
    /// When the app was last looked for.
    checked: Option<Instant>,
    /// Since when it has been gone.
    gone_since: Option<Instant>,
    /// One connection kept open: a new one every check would fill the
    /// backlog of an app that is frozen and not accepting, and then (on
    /// macOS at once, on Linux once connect stops blocking) read as closed.
    held: Option<std::os::unix::net::UnixStream>,
}

/// Whether the app is still at the other end: it answers end of file or an
/// error once it has quit. Anything it sent is read and dropped.
fn still_connected(stream: &mut std::os::unix::net::UnixStream) -> bool {
    use std::io::Read;
    let mut buffer = [0u8; 512];
    for _ in 0..16 {
        match stream.read(&mut buffer) {
            Ok(0) => return false,
            Ok(_) => {}
            Err(err) if err.kind() == std::io::ErrorKind::WouldBlock => return true,
            Err(err) if err.kind() == std::io::ErrorKind::Interrupted => {}
            Err(_) => return false,
        }
    }
    true
}

impl AppWatch {
    fn check(&self) -> Result<(), ServerError> {
        let mut state = self
            .state
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        if state.checked.is_some_and(|at| at.elapsed() < self.every) {
            return Ok(());
        }
        state.checked = Some(Instant::now());
        let mut open = state.held.as_mut().is_some_and(still_connected);
        if !open {
            state.held = None;
            // A listener that is there but not accepting (a frozen app)
            // still counts as the app being open.
            open = match rpc::connect_unix(&self.socket, Duration::ZERO) {
                Ok(stream) => {
                    state.held = stream.set_nonblocking(true).is_ok().then_some(stream);
                    true
                }
                Err(err) => err.kind() == std::io::ErrorKind::TimedOut,
            };
        }
        if open {
            state.gone_since = None;
            return Ok(());
        }
        let since = *state.gone_since.get_or_insert_with(Instant::now);
        if since.elapsed() >= self.patience {
            return Err(ServerError::Gone(format!(
                "the Herdr app ({}) has been closed for {}s",
                self.socket.display(),
                since.elapsed().as_secs()
            )));
        }
        Ok(())
    }
}
/// How long a helper program may take before the engine gives up on it: it
/// runs on the poll thread, which must keep answering stop and reload.
const HELPER_TIMEOUT: Duration = Duration::from_secs(5);

/// Runs `command` and collects its output, killing it at `deadline`.
fn run_with_deadline(
    command: &mut std::process::Command,
    deadline: Duration,
) -> Result<std::process::Output, ServerError> {
    use std::io::Read;
    let program = command.get_program().to_string_lossy().into_owned();
    let mut child = command
        .stdin(std::process::Stdio::null())
        .stdout(std::process::Stdio::piped())
        .stderr(std::process::Stdio::piped())
        .spawn()
        .map_err(|err| ServerError::Helper(format!("cannot run {program}: {err}")))?;
    let until = Instant::now() + deadline;
    let status = loop {
        match child.try_wait() {
            Ok(Some(status)) => break status,
            Ok(None) if Instant::now() >= until => {
                let _ = child.kill();
                let _ = child.wait();
                return Err(ServerError::Helper(format!(
                    "{program} did not finish within {}s",
                    deadline.as_secs()
                )));
            }
            Ok(None) => std::thread::sleep(Duration::from_millis(20)),
            Err(err) => {
                return Err(ServerError::Helper(format!(
                    "cannot wait for {program}: {err}"
                )))
            }
        }
    };
    let mut stdout = Vec::new();
    let mut stderr = Vec::new();
    if let Some(mut pipe) = child.stdout.take() {
        let _ = pipe.read_to_end(&mut stdout);
    }
    if let Some(mut pipe) = child.stderr.take() {
        let _ = pipe.read_to_end(&mut stderr);
    }
    Ok(std::process::Output {
        status,
        stdout,
        stderr,
    })
}

struct Live {
    conn: Connection,
    /// The channel `events.subscribe` opened.
    events: u32,
    /// The event channel is a byte stream of JSON lines that the core cuts
    /// into frames by size, not by line: the tail of a cut line waits here.
    partial: Vec<u8>,
    table: Table,
}

struct State {
    live: Option<Live>,
    /// When the current run of failures began; `None` while connected.
    failing_since: Option<Instant>,
}

pub struct CoreServer {
    app: Option<AppWatch>,
    socket: PathBuf,
    patience: Duration,
    state: Mutex<State>,
}

impl CoreServer {
    pub fn new(socket: PathBuf) -> Self {
        let patience = std::env::var("HERDR_TRIGGERS_CORE_PATIENCE_MS")
            .ok()
            .and_then(|ms| ms.parse().ok())
            .map(Duration::from_millis)
            .unwrap_or(DEFAULT_PATIENCE);
        Self::with_patience(socket, patience)
    }

    pub fn with_patience(socket: PathBuf, patience: Duration) -> Self {
        Self {
            app: None,
            socket,
            patience,
            state: Mutex::new(State {
                live: None,
                failing_since: None,
            }),
        }
    }

    /// Leave once the Herdr app behind `app_socket` has quit.
    pub fn following_app(mut self, app_socket: PathBuf) -> Self {
        self.app = Some(AppWatch {
            socket: app_socket,
            every: APP_CHECK_EVERY,
            patience: APP_PATIENCE,
            state: Mutex::new(AppState::default()),
        });
        self
    }

    fn check_app(&self) -> Result<(), ServerError> {
        self.app.as_ref().map_or(Ok(()), AppWatch::check)
    }

    fn open(&self) -> Result<Live, ClientError> {
        let mut conn = Connection::connect(&self.socket, CALL_TIMEOUT)?;
        let result = conn.call("events.subscribe", json!({}))?;
        let events = result["channel"]
            .as_u64()
            .ok_or_else(|| ClientError::Malformed("events.subscribe gave no channel".into()))?
            as u32;
        let table = Table::from_snapshot(
            result["seq"].as_u64().unwrap_or(0),
            result["terminals"]
                .as_array()
                .map(Vec::as_slice)
                .unwrap_or(&[]),
        );
        Ok(Live {
            conn,
            events,
            partial: Vec::new(),
            table,
        })
    }

    /// Runs `f` on the live connection, connecting first when needed. A
    /// transport failure drops the connection so the next call reconnects;
    /// an API error (a terminal that vanished) keeps it.
    fn with_live<T>(
        &self,
        f: impl FnOnce(&mut Live) -> Result<T, ClientError>,
    ) -> Result<T, ServerError> {
        let mut state = self
            .state
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        if state.live.is_none() {
            match self.open() {
                Ok(live) => {
                    state.live = Some(live);
                    state.failing_since = None;
                }
                Err(err) => return Err(self.failure(&mut state, err)),
            }
        }
        let Some(live) = state.live.as_mut() else {
            return Err(ServerError::Client(ClientError::Closed));
        };
        match f(live) {
            Ok(value) => Ok(value),
            Err(err) if is_transport(&err) => {
                state.live = None;
                Err(self.failure(&mut state, err))
            }
            Err(err) => Err(ServerError::Client(err)),
        }
    }

    fn failure(&self, state: &mut State, err: ClientError) -> ServerError {
        let since = *state.failing_since.get_or_insert_with(Instant::now);
        if since.elapsed() >= self.patience {
            ServerError::Gone(format!(
                "herdr-core at {} unreachable for {:?}: {err}",
                self.socket.display(),
                since.elapsed()
            ))
        } else {
            ServerError::Client(err)
        }
    }
}

/// The last `n` lines of `text` (all of it when it has fewer).
fn last_lines(text: &str, n: usize) -> &str {
    let mut seen = 0;
    for (at, byte) in text.trim_end_matches('\n').bytes().enumerate().rev() {
        if byte == b'\n' {
            seen += 1;
            if seen == n {
                return &text[at + 1..];
            }
        }
    }
    text
}

fn is_transport(err: &ClientError) -> bool {
    !matches!(err, ClientError::Api { .. })
}

/// Applies every queued event to the table, crediting each frame so the
/// core keeps sending, and drops leftovers of finished attach channels.
fn pump_events(live: &mut Live) -> Result<(), ClientError> {
    live.conn.pump()?;
    for frame in live.conn.take(live.events) {
        match frame.kind {
            wire::DATA => {
                live.partial.extend_from_slice(&frame.payload);
                while let Some(end) = live.partial.iter().position(|b| *b == b'\n') {
                    let line: Vec<u8> = live.partial.drain(..=end).collect();
                    if let Ok(notification) = serde_json::from_slice::<Value>(&line) {
                        live.table.apply(&notification);
                    }
                }
                live.conn.write(
                    live.events,
                    wire::CREDIT,
                    &wire::credit(frame.payload.len()),
                )?;
            }
            wire::CLOSE => return Err(ClientError::Closed),
            _ => {}
        }
    }
    live.conn.clear_side();
    Ok(())
}

impl Server for CoreServer {
    fn describe(&self) -> String {
        self.socket.display().to_string()
    }

    /// Reads what the core already pushed; no request, so it is free.
    fn alive(&self) -> Result<(), ServerError> {
        self.check_app()?;
        self.with_live(pump_events)
    }

    fn panes(&self, _with_labels: bool) -> Result<Vec<PaneInfo>, ServerError> {
        self.check_app()?;
        self.with_live(|live| {
            pump_events(live)?;
            Ok(live.table.panes())
        })
    }

    fn read(
        &self,
        pane_id: &str,
        _source: &str,
        lines: Option<u32>,
    ) -> Result<String, ServerError> {
        // Always the screen alone: asking herdr-core for history makes it
        // scroll a full-screen app to collect it, at poll rate. `lines` keeps
        // its herdr meaning, the last N lines of what is read.
        let text = self.with_live(|live| {
            let result = live
                .conn
                .call("terminal.read", json!({"terminal": pane_id, "lines": 0}))?;
            result["text"]
                .as_str()
                .map(str::to_string)
                .ok_or_else(|| ClientError::Malformed("terminal.read has no text".into()))
        })?;
        Ok(match lines {
            Some(n) => last_lines(&text, n as usize).to_string(),
            None => text,
        })
    }

    fn send_text(&self, pane_id: &str, text: &str) -> Result<(), ServerError> {
        self.with_live(|live| {
            // The terminal's own size, so attaching never resizes it; pumped
            // first so a resize since the last poll is not undone.
            pump_events(live)?;
            let (cols, rows) = live.table.size(pane_id).unwrap_or((80, 24));
            let result = live.conn.call(
                "terminal.attach",
                json!({"terminal": pane_id, "observe": false, "history": 0, "cols": cols, "rows": rows}),
            )?;
            let channel = result["channel"]
                .as_u64()
                .ok_or_else(|| ClientError::Malformed("terminal.attach gave no channel".into()))? as u32;
            live.conn.write(channel, wire::DATA, text.as_bytes())?;
            // Detach at once; the first paint and the core's CLOSE are
            // discarded by the next pump. Not waiting keeps a fire at one
            // round trip.
            live.conn.write(channel, wire::CLOSE, &[])?;
            Ok(())
        })
    }

    fn tab_label(&self, tab_id: &str) -> Result<String, ServerError> {
        let label = self.with_live(|live| {
            pump_events(live)?;
            Ok(live.table.tab_label(tab_id))
        })?;
        label.ok_or_else(|| {
            ServerError::Helper(format!("no terminal of tab {tab_id} carries a where label"))
        })
    }

    fn rename_tab(&self, tab_id: &str, label: &str) -> Result<(), ServerError> {
        // herdr-core has no tab names; the app owns them, and its CLI reaches
        // the open window. With the window closed this fails, and the rule
        // logs it like any other failed action.
        let args = json!({"id": tab_id, "name": label}).to_string();
        let output = run_with_deadline(
            std::process::Command::new("herdr-app").args(["do", "tab.rename", &args]),
            HELPER_TIMEOUT,
        )?;
        if output.status.success() {
            Ok(())
        } else {
            // herdr-app reports a refused command as JSON on stdout.
            let why = [&output.stdout, &output.stderr]
                .iter()
                .map(|bytes| String::from_utf8_lossy(bytes).trim().to_string())
                .filter(|text| !text.is_empty())
                .collect::<Vec<_>>()
                .join(" ");
            Err(ServerError::Helper(format!(
                "herdr-app do tab.rename failed: {why}"
            )))
        }
    }

    /// A transport failure dropped the one connection; the next pane would
    /// reconnect and wait out the same timeout again.
    fn aborts_poll(&self, err: &ServerError) -> bool {
        match err {
            ServerError::Client(err) => is_transport(err),
            ServerError::Gone(_) => true,
            ServerError::Helper(_) => false,
        }
    }

    fn notify_is_desktop(&self) -> bool {
        true
    }

    fn notify(&self, n: &Notification<'_>) -> Result<String, ServerError> {
        // No toast API outside the window: the desktop's notifier is the
        // only channel, so a notifier that fails is an error, not a reason
        // word - nobody saw the notice, and the rule must stay armed.
        let mut command =
            desktop_notification_command(n.desktop.unwrap_or("normal"), n.title, n.body);
        let program = command.get_program().to_string_lossy().into_owned();
        let output = run_with_deadline(&mut command, HELPER_TIMEOUT)?;
        if output.status.success() {
            Ok("shown".to_string())
        } else {
            Err(ServerError::Helper(format!(
                "{program} failed ({}): {}",
                output.status,
                String::from_utf8_lossy(&output.stderr).trim()
            )))
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use rpc::tests::{fake_core, read_request, reply};
    use std::io::Read;

    fn record(address: &str) -> Value {
        json!({"address": address, "state": "live", "title": "sh", "cols": 191, "rows": 42,
               "labels": {"herdr.space": "sp1", "herdr.tab": "tb1", "where": "~ / 1"}})
    }

    fn subscribe(stream: &mut std::os::unix::net::UnixStream, channel: u32, terminals: Vec<Value>) {
        let (id, method, _) = read_request(stream);
        assert_eq!(method, "events.subscribe");
        reply(
            stream,
            id,
            json!({"seq": 1, "terminals": terminals, "channel": channel}),
        );
    }

    #[test]
    fn panes_come_from_the_subscription_and_its_events_never_from_a_list_call() {
        // The event is sent only after the first panes() call returned, so
        // the first result is deterministically the snapshot alone.
        let (go_tx, go_rx) = std::sync::mpsc::channel::<()>();
        let (sent_tx, sent_rx) = std::sync::mpsc::channel::<()>();
        let (socket, core) = fake_core(move |stream| {
            subscribe(stream, 5, vec![record("t1")]);
            go_rx.recv().unwrap();
            let event = json!({"jsonrpc": "2.0", "method": "event", "params": {"type": "created", "terminal": "t2", "data": {"record": record("t2")}, "seq": 2}});
            wire::write_frame(stream, 5, wire::DATA, format!("{event}\n").as_bytes()).unwrap();
            sent_tx.send(()).unwrap();
            // The daemon credits what it consumed.
            let credit = wire::read_frame(stream).unwrap().unwrap();
            assert_eq!((credit.channel, credit.kind), (5, wire::CREDIT));
            // Nothing else may arrive before the daemon hangs up: no
            // terminal.list. (No new read timeout here: macOS refuses
            // SO_RCVTIMEO with EINVAL once the peer has closed.)
            let mut rest = Vec::new();
            let _ = stream.read_to_end(&mut rest);
            assert!(rest.is_empty(), "unexpected frames: {rest:?}");
        });
        let server = CoreServer::new(socket);
        let first = server.panes(true).unwrap();
        assert_eq!(
            first.iter().map(|p| p.pane_id.as_str()).collect::<Vec<_>>(),
            ["t1"]
        );
        go_tx.send(()).unwrap();
        // Written means already in the socket buffer, so the next pump sees it.
        sent_rx.recv().unwrap();
        let second = server.panes(true).unwrap();
        assert_eq!(
            second
                .iter()
                .map(|p| p.pane_id.as_str())
                .collect::<Vec<_>>(),
            ["t1", "t2"]
        );
        drop(server);
        core.join().unwrap();
    }

    #[test]
    fn an_event_line_cut_across_two_frames_is_reassembled() {
        let (go_tx, go_rx) = std::sync::mpsc::channel::<()>();
        let (sent_tx, sent_rx) = std::sync::mpsc::channel::<()>();
        let (socket, core) = fake_core(move |stream| {
            subscribe(stream, 5, vec![record("t1")]);
            go_rx.recv().unwrap();
            let event = json!({"jsonrpc": "2.0", "method": "event", "params": {"type": "created", "terminal": "t2", "data": {"record": record("t2")}, "seq": 2}});
            let line = format!("{event}\n");
            let (head, tail) = line.as_bytes().split_at(line.len() / 2);
            wire::write_frame(stream, 5, wire::DATA, head).unwrap();
            wire::write_frame(stream, 5, wire::DATA, tail).unwrap();
            sent_tx.send(()).unwrap();
            let mut rest = Vec::new();
            let _ = stream.read_to_end(&mut rest);
        });
        let server = CoreServer::new(socket);
        assert_eq!(server.panes(true).unwrap().len(), 1);
        go_tx.send(()).unwrap();
        sent_rx.recv().unwrap();
        let ids: Vec<String> = server
            .panes(true)
            .unwrap()
            .into_iter()
            .map(|p| p.pane_id)
            .collect();
        assert_eq!(ids, ["t1", "t2"]);
        drop(server);
        core.join().unwrap();
    }

    #[test]
    fn a_helper_that_hangs_is_killed_at_its_deadline() {
        let started = Instant::now();
        let err = run_with_deadline(
            std::process::Command::new("sleep").arg("10"),
            Duration::from_millis(200),
        )
        .unwrap_err();
        assert!(
            started.elapsed() < Duration::from_secs(2),
            "{:?}",
            started.elapsed()
        );
        assert!(err.to_string().contains("did not finish"), "{err}");
        let done = run_with_deadline(
            std::process::Command::new("sh").args(["-c", "echo out; echo err >&2"]),
            Duration::from_secs(5),
        )
        .unwrap();
        assert_eq!(
            (done.stdout.as_slice(), done.stderr.as_slice()),
            (&b"out\n"[..], &b"err\n"[..])
        );
    }

    #[test]
    fn a_daemon_following_the_app_leaves_once_the_app_stays_closed() {
        let dir = std::env::temp_dir().join(format!("triggers-app-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        let socket = dir.join("herdr-app.sock");
        let watch = AppWatch {
            socket: socket.clone(),
            every: Duration::ZERO,
            patience: Duration::from_millis(200),
            state: Mutex::new(AppState::default()),
        };
        let app = std::os::unix::net::UnixListener::bind(&socket).unwrap();
        assert!(watch.check().is_ok(), "the app is up");
        // Quit: the socket file stays, connections are refused.
        let closed = Instant::now();
        drop(app);
        assert!(socket.exists());
        assert!(watch.check().is_ok(), "within the patience window");
        // A program another test thread spawns holds a copy of the listener
        // until it execs, so the app can look open for a moment after it
        // closed, and that restarts the clock: wait for Gone, bounded.
        loop {
            std::thread::sleep(Duration::from_millis(50));
            match watch.check() {
                Err(ServerError::Gone(_)) => break,
                Ok(()) => assert!(closed.elapsed() < Duration::from_secs(5), "never gone"),
                Err(other) => panic!("{other}"),
            }
        }
        assert!(closed.elapsed() >= watch.patience);
        // Back within the window resets the clock.
        std::fs::remove_file(&socket).unwrap();
        let _app = std::os::unix::net::UnixListener::bind(&socket).unwrap();
        assert!(watch.check().is_ok());
        std::fs::remove_dir_all(dir).unwrap();
    }

    #[test]
    fn a_frozen_app_is_watched_over_one_held_connection() {
        let dir = std::env::temp_dir().join(format!("triggers-frozen-app-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        let socket = dir.join("herdr-app.sock");
        // Never accepts: what a frozen app looks like from outside.
        let app = std::os::unix::net::UnixListener::bind(&socket).unwrap();
        let watch = AppWatch {
            socket,
            every: Duration::ZERO,
            patience: Duration::ZERO,
            state: Mutex::new(AppState::default()),
        };
        for _ in 0..20 {
            assert!(watch.check().is_ok());
        }
        app.set_nonblocking(true).unwrap();
        let mut queued = 0;
        while app.accept().is_ok() {
            queued += 1;
        }
        assert_eq!(queued, 1, "one connection, not one per check");
        std::fs::remove_dir_all(dir).unwrap();
    }

    #[test]
    fn lines_keeps_the_last_n_lines_of_the_screen() {
        assert_eq!(last_lines("a\nb\nc\n", 2), "b\nc\n");
        assert_eq!(last_lines("a\nb\nc", 2), "b\nc");
        assert_eq!(last_lines("a\nb", 5), "a\nb");
        assert_eq!(last_lines("", 3), "");
    }

    #[test]
    fn send_text_attaches_at_the_terminals_size_types_the_exact_bytes_and_closes() {
        let (socket, core) = fake_core(|stream| {
            subscribe(stream, 5, vec![record("t1")]);
            let (id, method, params) = read_request(stream);
            assert_eq!(method, "terminal.attach");
            assert_eq!(params["terminal"], "t1");
            assert_eq!(params["observe"], false);
            assert_eq!(
                (params["cols"].as_u64(), params["rows"].as_u64()),
                (Some(191), Some(42))
            );
            reply(
                stream,
                id,
                json!({"channel": 9, "terminal": record("t1"), "resize_notifications": true, "automatic_input": true}),
            );
            let typed = wire::read_frame(stream).unwrap().unwrap();
            assert_eq!((typed.channel, typed.kind), (9, wire::DATA));
            assert_eq!(typed.payload, b"hunter2\n");
            let closed = wire::read_frame(stream).unwrap().unwrap();
            assert_eq!(
                (closed.channel, closed.kind, closed.payload.len()),
                (9, wire::CLOSE, 0)
            );
        });
        let server = CoreServer::new(socket);
        server.send_text("t1", "hunter2\n").unwrap();
        drop(server);
        core.join().unwrap();
    }

    #[test]
    fn a_lost_core_is_retried_then_becomes_gone_after_the_patience_window() {
        let (die_tx, die_rx) = std::sync::mpsc::channel::<()>();
        let (socket, core) = fake_core(move |stream| {
            subscribe(stream, 5, vec![record("t1")]);
            // Then the core dies, once the first poll has seen it alive.
            die_rx.recv().unwrap();
        });
        let server = CoreServer::with_patience(socket.clone(), Duration::from_millis(300));
        assert_eq!(server.panes(false).unwrap().len(), 1);
        die_tx.send(()).unwrap();
        core.join().unwrap();
        std::fs::remove_file(&socket).unwrap();
        // The listener is gone: a pump sees EOF, the reconnect is refused. A
        // child another test forks in parallel can hold the fake's socket
        // until it execs, delaying that EOF, so wait for it, bounded.
        let deadline = Instant::now() + Duration::from_secs(2);
        let first = loop {
            match server.panes(false) {
                Err(err) => break err,
                Ok(_) if Instant::now() < deadline => std::thread::sleep(Duration::from_millis(10)),
                Ok(panes) => panic!("still answered after the core died: {panes:?}"),
            }
        };
        assert!(!server.is_fatal(&first), "{first}");
        std::thread::sleep(Duration::from_millis(350));
        let later = server.panes(false).unwrap_err();
        assert!(server.is_fatal(&later), "{later}");
        assert!(matches!(later, ServerError::Gone(_)));
    }
}
