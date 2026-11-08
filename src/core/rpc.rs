//! One framed connection to herdr-core.
//!
//! The handshake is a plain JSON line: `hello {framed: true}` answered by a
//! line, after which every message is a frame (`wire.rs`). Calls are DATA
//! frames on channel 0 and are answered on channel 0; the event subscription
//! and attach streams each get a channel of their own, and frames for those
//! are kept in a side queue while a reply is awaited.
//!
//! Bytes are read into one buffer and frames parsed out of it, so a read
//! that stops mid-frame (a timeout, a non-blocking drain) never loses the
//! half it already has.

use std::collections::VecDeque;
use std::io::{Read, Write};
use std::os::unix::net::UnixStream;
use std::path::Path;
use std::time::{Duration, Instant};

use serde_json::{json, Value};

use super::wire::{self, Frame};
use crate::client::ClientError;

/// Methods the core host cannot work without; refused at hello rather than
/// discovered one call at a time.
const REQUIRED: [&str; 4] = [
    "terminal.list",
    "terminal.read",
    "terminal.attach",
    "events.subscribe",
];
const CHUNK: usize = 64 * 1024;

pub struct Connection {
    stream: UnixStream,
    /// Bytes received and not yet parsed into frames.
    buffer: Vec<u8>,
    next_id: u64,
    /// Frames for channels other than 0, in arrival order.
    side: VecDeque<Frame>,
    timeout: Duration,
}

/// Connects to a Unix socket without ever blocking past `timeout`.
///
/// A blocking connect to a listener that has stopped accepting (a frozen
/// process) waits forever on Linux once the backlog is full, and that would
/// stall the poll thread. Non-blocking, a full backlog is `EAGAIN`, retried
/// until `timeout` and then `TimedOut`: the listener is there, just busy.
pub fn connect_unix(path: &Path, timeout: Duration) -> std::io::Result<UnixStream> {
    use std::os::fd::AsRawFd;
    use std::os::unix::ffi::OsStrExt;

    let bytes = path.as_os_str().as_bytes();
    // SAFETY: sockaddr_un is plain data; all zeroes is a valid value.
    let mut addr: libc::sockaddr_un = unsafe { std::mem::zeroed() };
    if bytes.len() >= addr.sun_path.len() || bytes.contains(&0) {
        return Err(std::io::Error::new(
            std::io::ErrorKind::InvalidInput,
            format!("socket path cannot be used: {}", path.display()),
        ));
    }
    addr.sun_family = libc::AF_UNIX as libc::sa_family_t;
    for (dst, src) in addr.sun_path.iter_mut().zip(bytes) {
        *dst = *src as libc::c_char;
    }
    let fd = cloexec_socket()?;
    set_flags(&fd, libc::O_NONBLOCK, true)?;
    let deadline = Instant::now() + timeout;
    loop {
        // SAFETY: `addr` is a valid, NUL-terminated sockaddr_un of that size.
        let rc = unsafe {
            libc::connect(
                fd.as_raw_fd(),
                (&addr as *const libc::sockaddr_un).cast::<libc::sockaddr>(),
                std::mem::size_of::<libc::sockaddr_un>() as libc::socklen_t,
            )
        };
        if rc == 0 {
            break;
        }
        let err = std::io::Error::last_os_error();
        match err.raw_os_error() {
            Some(libc::EINTR) => {}
            Some(libc::EAGAIN) => {
                let now = Instant::now();
                if now >= deadline {
                    return Err(std::io::Error::new(
                        std::io::ErrorKind::TimedOut,
                        format!("{} is not accepting connections", path.display()),
                    ));
                }
                std::thread::sleep((deadline - now).min(Duration::from_millis(20)));
            }
            _ => return Err(err),
        }
    }
    set_flags(&fd, libc::O_NONBLOCK, false)?;
    Ok(UnixStream::from(fd))
}

/// A Unix stream socket that is close-on-exec from birth where the platform
/// allows it: set afterwards, a program spawned on another thread in between
/// inherits it.
#[cfg(target_os = "linux")]
fn cloexec_socket() -> std::io::Result<std::os::fd::OwnedFd> {
    use std::os::fd::FromRawFd;
    // SAFETY: plain socket(2); the result is checked before use.
    let raw = unsafe { libc::socket(libc::AF_UNIX, libc::SOCK_STREAM | libc::SOCK_CLOEXEC, 0) };
    if raw < 0 {
        return Err(std::io::Error::last_os_error());
    }
    // SAFETY: `raw` is a fresh descriptor nothing else owns.
    Ok(unsafe { std::os::fd::OwnedFd::from_raw_fd(raw) })
}

/// macOS has no SOCK_CLOEXEC, so the flag is set right after; std does the
/// same there.
#[cfg(not(target_os = "linux"))]
fn cloexec_socket() -> std::io::Result<std::os::fd::OwnedFd> {
    use std::os::fd::{AsRawFd, FromRawFd};
    // SAFETY: plain socket(2); the result is checked before use.
    let raw = unsafe { libc::socket(libc::AF_UNIX, libc::SOCK_STREAM, 0) };
    if raw < 0 {
        return Err(std::io::Error::last_os_error());
    }
    // SAFETY: `raw` is a fresh descriptor nothing else owns.
    let fd = unsafe { std::os::fd::OwnedFd::from_raw_fd(raw) };
    // SAFETY: F_SETFD on a descriptor this function owns.
    if unsafe { libc::fcntl(fd.as_raw_fd(), libc::F_SETFD, libc::FD_CLOEXEC) } < 0 {
        return Err(std::io::Error::last_os_error());
    }
    Ok(fd)
}

fn set_flags(fd: &std::os::fd::OwnedFd, flag: libc::c_int, on: bool) -> std::io::Result<()> {
    use std::os::fd::AsRawFd;
    // SAFETY: F_GETFL/F_SETFL on a descriptor the caller owns.
    let flags = unsafe { libc::fcntl(fd.as_raw_fd(), libc::F_GETFL) };
    if flags < 0 {
        return Err(std::io::Error::last_os_error());
    }
    let flags = if on { flags | flag } else { flags & !flag };
    // SAFETY: as above.
    if unsafe { libc::fcntl(fd.as_raw_fd(), libc::F_SETFL, flags) } < 0 {
        return Err(std::io::Error::last_os_error());
    }
    Ok(())
}

impl Connection {
    pub fn connect(socket: &Path, timeout: Duration) -> Result<Self, ClientError> {
        let stream = connect_unix(socket, timeout)?;
        stream.set_write_timeout(Some(timeout))?;
        let mut this = Self {
            stream,
            buffer: Vec::with_capacity(CHUNK),
            next_id: 0,
            side: VecDeque::new(),
            timeout,
        };
        let hello =
            json!({"jsonrpc": "2.0", "id": 0, "method": "hello", "params": {"framed": true}});
        let mut bytes = hello.to_string().into_bytes();
        bytes.push(b'\n');
        this.stream.write_all(&bytes)?;
        let line = this.read_line(Instant::now() + timeout)?;
        let reply: Value =
            serde_json::from_str(line.trim()).map_err(|_| ClientError::Malformed(line.clone()))?;
        if let Some(error) = reply.get("error") {
            return Err(api_error(error));
        }
        let result = &reply["result"];
        if result["framed"] != Value::Bool(true) {
            return Err(ClientError::Malformed(
                "hello did not switch to framed mode".into(),
            ));
        }
        let methods: Vec<String> = result["methods"]
            .as_array()
            .map(|list| {
                list.iter()
                    .filter_map(Value::as_str)
                    .map(str::to_string)
                    .collect()
            })
            .unwrap_or_default();
        if let Some(missing) = REQUIRED
            .iter()
            .find(|m| !methods.iter().any(|have| have == *m))
        {
            return Err(ClientError::Malformed(format!(
                "herdr-core lacks {missing}; upgrade hc"
            )));
        }
        Ok(this)
    }

    pub fn write(&mut self, channel: u32, kind: u8, payload: &[u8]) -> Result<(), ClientError> {
        wire::write_frame(&mut self.stream, channel, kind, payload)?;
        Ok(())
    }

    /// One JSON-RPC call on channel 0, answered within the timeout. Frames
    /// for other channels seen meanwhile go to the side queue.
    pub fn call(&mut self, method: &str, params: Value) -> Result<Value, ClientError> {
        self.next_id += 1;
        let id = self.next_id;
        let request = json!({"jsonrpc": "2.0", "id": id, "method": method, "params": params});
        self.write(0, wire::DATA, request.to_string().as_bytes())?;
        let deadline = Instant::now() + self.timeout;
        loop {
            let Some(frame) = self.read_one(deadline)? else {
                return Err(ClientError::Closed);
            };
            if frame.channel != 0 {
                self.side.push_back(frame);
                continue;
            }
            if frame.kind != wire::DATA {
                continue;
            }
            let reply: Value = serde_json::from_slice(&frame.payload).map_err(|_| {
                ClientError::Malformed(String::from_utf8_lossy(&frame.payload).into_owned())
            })?;
            if reply.get("id").and_then(Value::as_u64) != Some(id) {
                // A notification, or a reply to something else: not ours.
                continue;
            }
            if let Some(error) = reply.get("error") {
                return Err(api_error(error));
            }
            return Ok(reply.get("result").cloned().unwrap_or(Value::Null));
        }
    }

    /// Reads whatever bytes have already arrived, without waiting, and
    /// queues the complete frames among them.
    pub fn pump(&mut self) -> Result<(), ClientError> {
        self.stream.set_nonblocking(true)?;
        let mut chunk = [0u8; CHUNK];
        let drained = loop {
            match self.stream.read(&mut chunk) {
                Ok(0) => break Err(ClientError::Closed),
                Ok(n) => self.buffer.extend_from_slice(&chunk[..n]),
                Err(e) if e.kind() == std::io::ErrorKind::WouldBlock => break Ok(()),
                Err(e) if e.kind() == std::io::ErrorKind::Interrupted => continue,
                Err(e) => break Err(ClientError::Io(e)),
            }
        };
        self.stream.set_nonblocking(false)?;
        while let Some(frame) = wire::parse_frame(&mut self.buffer)? {
            self.side.push_back(frame);
        }
        drained
    }

    /// Every queued frame for `channel`, in order.
    pub fn take(&mut self, channel: u32) -> Vec<Frame> {
        let mut out = Vec::new();
        let mut rest = VecDeque::with_capacity(self.side.len());
        for frame in self.side.drain(..) {
            if frame.channel == channel {
                out.push(frame);
            } else {
                rest.push_back(frame);
            }
        }
        self.side = rest;
        out
    }

    /// Drops every queued frame: what is left after the event channel was
    /// taken belongs to attach channels already closed.
    pub fn clear_side(&mut self) {
        self.side.clear();
    }

    /// The hello reply: the one plain line. Bytes after its newline are
    /// frames already and stay in the buffer.
    fn read_line(&mut self, deadline: Instant) -> Result<String, ClientError> {
        loop {
            if let Some(at) = self.buffer.iter().position(|b| *b == b'\n') {
                let line: Vec<u8> = self.buffer.drain(..=at).collect();
                return Ok(String::from_utf8_lossy(&line).into_owned());
            }
            if !self.fill(deadline)? {
                return Err(ClientError::Closed);
            }
        }
    }

    /// One frame from the wire, or `None` on a clean close, by `deadline`.
    fn read_one(&mut self, deadline: Instant) -> Result<Option<Frame>, ClientError> {
        loop {
            if let Some(frame) = wire::parse_frame(&mut self.buffer)? {
                return Ok(Some(frame));
            }
            if !self.fill(deadline)? {
                return Ok(None);
            }
        }
    }

    /// Blocks for more bytes until `deadline`. `Ok(false)` on a clean close;
    /// a deadline that passes is a `TimedOut` error. The read timeout is
    /// re-armed to what is left so other channels' frames cannot extend a call.
    fn fill(&mut self, deadline: Instant) -> Result<bool, ClientError> {
        let left = deadline.saturating_duration_since(Instant::now());
        if left.is_zero() {
            return Err(timed_out());
        }
        self.stream.set_read_timeout(Some(left))?;
        let mut chunk = [0u8; CHUNK];
        match self.stream.read(&mut chunk) {
            Ok(0) => Ok(false),
            Ok(n) => {
                self.buffer.extend_from_slice(&chunk[..n]);
                Ok(true)
            }
            // A signal (reload, reset, stop) landing mid-read: try again
            // within the same deadline rather than drop the connection.
            Err(e) if e.kind() == std::io::ErrorKind::Interrupted => Ok(true),
            Err(e)
                if matches!(
                    e.kind(),
                    std::io::ErrorKind::WouldBlock | std::io::ErrorKind::TimedOut
                ) =>
            {
                Err(timed_out())
            }
            Err(e) => Err(ClientError::Io(e)),
        }
    }
}

fn timed_out() -> ClientError {
    ClientError::Io(std::io::Error::new(
        std::io::ErrorKind::TimedOut,
        "herdr-core did not answer in time",
    ))
}

/// herdr-core puts its stable error kind (`not_found`, `blocked`, ...) in
/// `error.data.kind`; `error.code` is the JSON-RPC integer.
fn api_error(error: &Value) -> ClientError {
    let code = error["data"]["kind"]
        .as_str()
        .map(str::to_string)
        .unwrap_or_else(|| error["code"].to_string());
    ClientError::Api {
        code,
        message: error
            .get("message")
            .and_then(Value::as_str)
            .unwrap_or_default()
            .to_string(),
    }
}

#[cfg(test)]
pub(crate) mod tests {
    use super::*;
    use std::io::{BufRead, BufReader, Read};
    use std::os::unix::net::UnixListener;

    /// A fake herdr-core for one connection: answers hello as a line, then
    /// runs `script` over the framed stream.
    pub(crate) fn fake_core(
        script: impl FnOnce(&mut UnixStream) + Send + 'static,
    ) -> (std::path::PathBuf, std::thread::JoinHandle<()>) {
        static NEXT: std::sync::atomic::AtomicUsize = std::sync::atomic::AtomicUsize::new(0);
        let dir = std::env::temp_dir().join(format!(
            "triggers-rpc-{}-{}",
            std::process::id(),
            NEXT.fetch_add(1, std::sync::atomic::Ordering::Relaxed)
        ));
        std::fs::create_dir_all(&dir).unwrap();
        let socket = dir.join("default.sock");
        let listener = UnixListener::bind(&socket).unwrap();
        let handle = std::thread::spawn(move || {
            let (mut stream, _) = listener.accept().unwrap();
            stream
                .set_read_timeout(Some(Duration::from_secs(5)))
                .unwrap();
            let mut line = String::new();
            BufReader::new(stream.try_clone().unwrap())
                .read_line(&mut line)
                .unwrap();
            assert!(
                line.contains("\"hello\""),
                "first message must be hello: {line}"
            );
            let reply =
                json!({"jsonrpc": "2.0", "id": 0, "result": {"framed": true, "methods": REQUIRED}});
            stream.write_all(format!("{reply}\n").as_bytes()).unwrap();
            script(&mut stream);
        });
        (socket, handle)
    }

    pub(crate) fn read_request(stream: &mut UnixStream) -> (u64, String, Value) {
        let frame = wire::read_frame(stream).unwrap().unwrap();
        assert_eq!(frame.channel, 0);
        let request: Value = serde_json::from_slice(&frame.payload).unwrap();
        (
            request["id"].as_u64().unwrap(),
            request["method"].as_str().unwrap().to_string(),
            request["params"].clone(),
        )
    }

    pub(crate) fn reply(stream: &mut UnixStream, id: u64, result: Value) {
        let body = json!({"jsonrpc": "2.0", "id": id, "result": result});
        wire::write_frame(stream, 0, wire::DATA, body.to_string().as_bytes()).unwrap();
    }

    #[test]
    fn a_call_is_answered_and_other_channels_frames_are_kept_aside() {
        let (socket, core) = fake_core(|stream| {
            let (id, method, params) = read_request(stream);
            assert_eq!(method, "terminal.read");
            assert_eq!(params["terminal"], "t1");
            // An event frame lands before the reply: it must not be lost.
            wire::write_frame(stream, 5, wire::DATA, b"{\"method\":\"event\"}\n").unwrap();
            reply(stream, id, json!({"text": "hello"}));
            let mut rest = Vec::new();
            let _ = stream.read_to_end(&mut rest);
        });
        let mut conn = Connection::connect(&socket, Duration::from_secs(2)).unwrap();
        let result = conn
            .call("terminal.read", json!({"terminal": "t1"}))
            .unwrap();
        assert_eq!(result["text"], "hello");
        let kept = conn.take(5);
        assert_eq!(kept.len(), 1);
        assert_eq!(kept[0].payload, b"{\"method\":\"event\"}\n");
        drop(conn);
        core.join().unwrap();
    }

    #[test]
    fn an_api_error_carries_the_kind_and_a_missing_method_is_refused_at_hello() {
        let (socket, core) = fake_core(|stream| {
            let (id, _, _) = read_request(stream);
            let body = json!({"jsonrpc": "2.0", "id": id, "error": {"code": -32000, "message": "no t9", "data": {"kind": "not_found"}}});
            wire::write_frame(stream, 0, wire::DATA, body.to_string().as_bytes()).unwrap();
        });
        let mut conn = Connection::connect(&socket, Duration::from_secs(2)).unwrap();
        let err = conn
            .call("terminal.read", json!({"terminal": "t9"}))
            .unwrap_err();
        assert!(matches!(err, ClientError::Api { code, .. } if code == "not_found"));
        drop(conn);
        core.join().unwrap();

        let dir = std::env::temp_dir().join(format!("triggers-rpc-old-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let socket = dir.join("default.sock");
        let listener = UnixListener::bind(&socket).unwrap();
        let old = std::thread::spawn(move || {
            let (mut stream, _) = listener.accept().unwrap();
            let mut line = String::new();
            BufReader::new(stream.try_clone().unwrap())
                .read_line(&mut line)
                .unwrap();
            let reply = json!({"jsonrpc": "2.0", "id": 0, "result": {"framed": true, "methods": ["terminal.list", "terminal.read"]}});
            stream.write_all(format!("{reply}\n").as_bytes()).unwrap();
        });
        let err = match Connection::connect(&socket, Duration::from_secs(2)) {
            Ok(_) => panic!("a core without terminal.attach was accepted"),
            Err(err) => err,
        };
        assert!(err.to_string().contains("lacks terminal.attach"), "{err}");
        old.join().unwrap();
    }

    #[test]
    fn a_reply_that_never_comes_is_a_timeout_not_a_hang() {
        let (socket, core) = fake_core(|stream| {
            let _ = read_request(stream);
            std::thread::sleep(Duration::from_millis(600));
        });
        let mut conn = Connection::connect(&socket, Duration::from_millis(200)).unwrap();
        let started = Instant::now();
        let err = conn.call("terminal.list", json!({})).unwrap_err();
        assert!(
            started.elapsed() < Duration::from_millis(500),
            "waited {:?}",
            started.elapsed()
        );
        assert!(matches!(err, ClientError::Io(_)), "{err}");
        drop(conn);
        core.join().unwrap();
    }

    #[cfg(target_os = "linux")]
    #[test]
    fn a_listener_that_stops_accepting_times_out_instead_of_blocking() {
        use std::os::fd::AsRawFd;
        let dir = std::env::temp_dir().join(format!("triggers-frozen-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        let socket = dir.join("app.sock");
        let frozen = UnixListener::bind(&socket).unwrap();
        // Linux lets listen() be called again to shrink the backlog.
        assert_eq!(unsafe { libc::listen(frozen.as_raw_fd(), 0) }, 0);
        let started = Instant::now();
        let mut kept = Vec::new();
        let mut timed_out = 0;
        for _ in 0..5 {
            match connect_unix(&socket, Duration::ZERO) {
                Ok(stream) => kept.push(stream),
                Err(err) if err.kind() == std::io::ErrorKind::TimedOut => timed_out += 1,
                Err(err) => panic!("{err}"),
            }
        }
        assert!(timed_out > 0, "the backlog never filled");
        assert!(
            started.elapsed() < Duration::from_secs(2),
            "{:?}",
            started.elapsed()
        );
        std::fs::remove_dir_all(dir).unwrap();
    }
}
