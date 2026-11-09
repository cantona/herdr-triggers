//! `--host core service`: the daemon a login service manager keeps. herdr-core
//! has no stable unit to bind to (it runs as transient, hash-named jobs), so
//! the service starts at login and waits for core's socket instead, attaches
//! when core answers, and goes back to waiting when it leaves.

use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::Duration;

use super::CoreServer;
use crate::server::Server;

/// How often a waiting service looks for herdr-core.
pub const WAIT_EVERY: Duration = Duration::from_secs(2);
/// Granularity of the waits, so a stop is acted on promptly.
const STOP_CHECK: Duration = Duration::from_millis(50);

/// Why `serve` returned.
#[derive(Debug, PartialEq, Eq)]
pub enum Served {
    Stopped,
    /// The socket now resolves to another core's state dir: the lock, pid
    /// and marker this process holds would be in the wrong place, and the
    /// CLI would not see it.
    Moved(PathBuf),
}

/// Waits for herdr-core, hands each attached server to `run` (which returns
/// once core is gone or `stop` is set), and waits again, until `stop`. The
/// socket is resolved again on every attempt: core's config may move it.
/// A move within this process's state dir is followed; one that `ours`
/// rejects ends the serve. `marker` exists while this waits: from when
/// `run` returns, which is the engine's patience (30 s) after core stopped
/// answering, until core answers again.
pub fn serve(
    stop: &AtomicBool,
    marker: &Path,
    every: Duration,
    resolve: impl Fn() -> Result<PathBuf, String>,
    ours: impl Fn(&Path) -> bool,
    mut run: impl FnMut(CoreServer),
) -> Served {
    let served = loop {
        let server = match wait_for_core(stop, marker, every, &resolve, &ours) {
            Waited::Attached(server) => *server,
            Waited::Stopped => break Served::Stopped,
            Waited::Moved(socket) => break Served::Moved(socket),
        };
        let _ = std::fs::remove_file(marker);
        crate::log_line!("attached to herdr-core at {}", server.describe());
        run(server);
        if stop.load(Ordering::Relaxed) {
            break Served::Stopped;
        }
        crate::log_line!("herdr-core gone; waiting for it");
    };
    let _ = std::fs::remove_file(marker);
    served
}

enum Waited {
    Attached(Box<CoreServer>),
    Stopped,
    Moved(PathBuf),
}

/// A server whose first call succeeded, unless `stop` is set or the socket
/// moved out of this state dir first. The probe is the attached server's
/// own first call, so core sees one connection, not a throwaway probe
/// followed by the real one.
fn wait_for_core(
    stop: &AtomicBool,
    marker: &Path,
    every: Duration,
    resolve: &impl Fn() -> Result<PathBuf, String>,
    ours: &impl Fn(&Path) -> bool,
) -> Waited {
    let mut announced: Option<String> = None;
    loop {
        if stop.load(Ordering::Relaxed) {
            return Waited::Stopped;
        }
        let why = match resolve() {
            Ok(socket) if !ours(&socket) => return Waited::Moved(socket),
            Ok(socket) => {
                let server = CoreServer::new(socket.clone());
                match server.alive() {
                    Ok(()) => return Waited::Attached(Box::new(server)),
                    Err(_) => format!("herdr-core at {} does not answer", socket.display()),
                }
            }
            Err(message) => format!("cannot resolve the herdr-core socket: {message}"),
        };
        if !marker.exists() {
            if let Err(err) = std::fs::write(marker, "") {
                crate::log_line!("cannot write {}: {err}", marker.display());
            }
        }
        // Once per distinct reason: a wait that lasts all day would otherwise
        // fill the log every two seconds.
        if announced.as_deref() != Some(why.as_str()) {
            crate::log_line!("waiting: {why}");
            announced = Some(why);
        }
        let mut left = every;
        while !left.is_zero() {
            if stop.load(Ordering::Relaxed) {
                return Waited::Stopped;
            }
            let step = left.min(STOP_CHECK);
            std::thread::sleep(step);
            left -= step;
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::core::rpc::tests::{fake_core, read_request, reply};
    use serde_json::json;
    use std::sync::Arc;
    use std::time::Instant;

    fn subscribe(stream: &mut std::os::unix::net::UnixStream) {
        let (id, method, _) = read_request(stream);
        assert_eq!(method, "events.subscribe");
        reply(stream, id, json!({"seq": 1, "terminals": [], "channel": 5}));
    }

    #[test]
    fn a_stop_during_the_wait_exits_promptly_and_clears_the_marker() {
        let dir = std::env::temp_dir().join(format!("triggers-svc-stop-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let marker = dir.join("triggersd.waiting");
        let socket = dir.join("nobody.sock");
        let stop = Arc::new(AtomicBool::new(false));
        let stopper = {
            let stop = stop.clone();
            let marker = marker.clone();
            std::thread::spawn(move || {
                let until = Instant::now() + Duration::from_secs(5);
                while !marker.exists() && Instant::now() < until {
                    std::thread::sleep(Duration::from_millis(10));
                }
                assert!(marker.exists(), "no waiting marker while core is absent");
                stop.store(true, Ordering::Relaxed);
                Instant::now()
            })
        };
        let mut attached = 0;
        let served = serve(
            &stop,
            &marker,
            Duration::from_secs(60),
            || Ok(socket.clone()),
            |_| true,
            |_| attached += 1,
        );
        assert_eq!(served, Served::Stopped);
        let stopped_at = stopper.join().unwrap();
        assert!(
            stopped_at.elapsed() < Duration::from_millis(500),
            "took {:?} to notice the stop",
            stopped_at.elapsed()
        );
        assert_eq!(attached, 0);
        assert!(!marker.exists());
        std::fs::remove_dir_all(dir).unwrap();
    }

    #[test]
    fn the_service_waits_attaches_waits_again_and_reattaches() {
        let (die_tx, die_rx) = std::sync::mpsc::channel::<()>();
        let (socket, first) = fake_core(move |stream| {
            subscribe(stream);
            die_rx.recv().unwrap();
        });
        let marker = socket.parent().unwrap().join("triggersd.waiting");
        let stop = AtomicBool::new(false);
        let mut runs = 0;
        let mut first = Some(first);
        let mut second: Option<std::thread::JoinHandle<()>> = None;
        let served = serve(
            &stop,
            &marker,
            Duration::from_millis(50),
            || Ok(socket.clone()),
            |_| true,
            |server| {
                runs += 1;
                assert!(!marker.exists(), "marker left while attached");
                if runs == 1 {
                    // Core leaves; the run returns as the engine does on Gone.
                    drop(server);
                    die_tx.send(()).unwrap();
                    first.take().unwrap().join().unwrap();
                    std::fs::remove_file(&socket).unwrap();
                    // A new core arrives once the service is waiting again.
                    let marker = marker.clone();
                    let socket = socket.clone();
                    second = Some(std::thread::spawn(move || {
                        let until = Instant::now() + Duration::from_secs(5);
                        while !marker.exists() && Instant::now() < until {
                            std::thread::sleep(Duration::from_millis(10));
                        }
                        assert!(marker.exists(), "no waiting marker after core left");
                        let listener = std::os::unix::net::UnixListener::bind(&socket).unwrap();
                        serve_one(listener);
                    }));
                } else {
                    stop.store(true, Ordering::Relaxed);
                }
            },
        );
        assert_eq!(served, Served::Stopped);
        assert_eq!(runs, 2);
        assert!(!marker.exists());
        second.unwrap().join().unwrap();
        std::fs::remove_dir_all(socket.parent().unwrap()).unwrap();
    }

    #[test]
    fn a_socket_moved_to_another_state_dir_ends_the_serve() {
        let (die_tx, die_rx) = std::sync::mpsc::channel::<()>();
        let (socket, core) = fake_core(move |stream| {
            subscribe(stream);
            die_rx.recv().unwrap();
        });
        let moved = socket.with_file_name("hc.sock");
        let marker = socket.parent().unwrap().join("triggersd.waiting");
        let resolved = std::cell::RefCell::new(socket.clone());
        let held = crate::core::paths::tag_of(&socket).unwrap();
        let stop = AtomicBool::new(false);
        let mut runs = 0;
        let served = serve(
            &stop,
            &marker,
            Duration::from_millis(50),
            || Ok(resolved.borrow().clone()),
            |s| crate::core::paths::tag_of(s).as_deref() == Some(held.as_str()),
            |server| {
                runs += 1;
                // core's config now names another socket, and this core leaves.
                *resolved.borrow_mut() = moved.clone();
                drop(server);
                die_tx.send(()).unwrap();
            },
        );
        assert_eq!(served, Served::Moved(moved.clone()));
        assert_eq!(runs, 1);
        assert!(!marker.exists());
        core.join().unwrap();
        // A move that keeps the tag (another folder, same name) is followed.
        let same_tag = Path::new("/elsewhere/default.sock");
        assert_eq!(
            crate::core::paths::tag_of(same_tag).as_deref(),
            Some(held.as_str())
        );
        std::fs::remove_dir_all(socket.parent().unwrap()).unwrap();
    }

    /// One more connection on `listener`, answered as `fake_core` does, and
    /// held until the service hangs up.
    fn serve_one(listener: std::os::unix::net::UnixListener) {
        use std::io::{BufRead, BufReader, Read, Write};
        let (mut stream, _) = listener.accept().unwrap();
        stream
            .set_read_timeout(Some(Duration::from_secs(5)))
            .unwrap();
        let mut line = String::new();
        BufReader::new(stream.try_clone().unwrap())
            .read_line(&mut line)
            .unwrap();
        assert!(line.contains("\"hello\""), "{line}");
        let hello = json!({"jsonrpc": "2.0", "id": 0, "result": {"framed": true,
            "methods": ["terminal.list", "terminal.read", "terminal.attach", "events.subscribe"]}});
        stream.write_all(format!("{hello}\n").as_bytes()).unwrap();
        subscribe(&mut stream);
        let _ = stream.read_to_end(&mut Vec::new());
    }
}
