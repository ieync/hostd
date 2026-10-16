//! Accept loop, per-connection handling and the byte pipe.
//!
//! One thread handles each client connection until the request head is routed. For a
//! proxied connection a second thread is added so both directions copy concurrently.
//! There is no async runtime: on loopback, with a handful of browser connections,
//! blocking threads are simpler and cost almost nothing while idle.

use crate::config::Route;
use crate::http::{Head, HeadError, read_head};
use crate::pages;
use crate::router::{Resolution, Router, resolve};
use std::io::{self, Read, Write};
use std::net::{Ipv4Addr, Ipv6Addr, Shutdown, SocketAddr, TcpListener, TcpStream};
use std::sync::Arc;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::thread;
use std::time::Duration;

/// Default cap on simultaneous client connections.
///
/// Browsers open at most about six connections per host, so 256 covers dozens of tabs
/// and apps. Each proxied connection uses two threads, so the cap bounds the process
/// to roughly 512 threads and stops a runaway local client from exhausting the thread
/// limit.
pub const MAX_CONNECTIONS: usize = 256;

/// Stack size for connection threads.
///
/// These threads only run copy loops and small string formatting, so they need far
/// less than the 2 MiB default. Stack memory is reserved lazily, but a small value
/// keeps the virtual reservation for ~512 threads modest.
const THREAD_STACK: usize = 128 * 1024;

/// How long a client may take to send its request head.
///
/// Browsers send the head immediately after connecting. A connection silent for 5 s is
/// a speculative preconnect or a stuck client, and closing it frees its thread.
const HEAD_TIMEOUT: Duration = Duration::from_secs(5);

/// How long to wait for a backend to accept a connection.
///
/// Loopback connects succeed or are refused in microseconds. Anything slower means a
/// wedged listener (full accept backlog), so fail fast with a 502 instead of leaving
/// the browser tab spinning.
const CONNECT_TIMEOUT: Duration = Duration::from_secs(2);

/// How long to spend writing one of our own pages to a client.
///
/// Stops a client that never reads from pinning a thread on a few hundred bytes.
const ERROR_WRITE_TIMEOUT: Duration = Duration::from_secs(5);

/// How long to keep draining a client's unread request data after an error page.
///
/// Closing a socket that still has unread data makes the kernel send RST, which can
/// make the client discard the response we just wrote. One second is enough for a
/// local client to finish an in-flight upload without holding the thread for long.
const LINGER: Duration = Duration::from_secs(1);

/// Upper bound on bytes drained during [`LINGER`].
///
/// Bounds the work a hostile client can make us do for one rejected request.
const LINGER_MAX_BYTES: usize = 64 * 1024;

/// Pause after a failed `accept`.
///
/// `accept` fails with EMFILE/ENFILE when descriptors run out; retrying immediately
/// would spin a core at 100%. 50 ms keeps that cheap and recovers quickly once
/// descriptors free up.
const ACCEPT_ERROR_BACKOFF: Duration = Duration::from_millis(50);

/// Tunables for [`serve`].
#[derive(Debug, Clone)]
pub struct Options {
    /// Maximum simultaneous client connections; extra ones are closed immediately.
    pub max_connections: usize,
    /// Log every routed connection to stderr.
    pub verbose: bool,
}

impl Default for Options {
    fn default() -> Self {
        Self {
            max_connections: MAX_CONNECTIONS,
            verbose: false,
        }
    }
}

/// Counts a live connection and releases its slot when dropped, on every exit path.
struct Slot(Arc<AtomicUsize>);

impl Drop for Slot {
    fn drop(&mut self) {
        self.0.fetch_sub(1, Ordering::AcqRel);
    }
}

/// Connect to a backend on loopback: IPv4 first, then IPv6.
///
/// Node (and so Vite and friends) often binds only `::1` for "localhost", while most
/// other servers bind only `127.0.0.1`. Trying both means a route works whichever the
/// app picked. Returns the IPv4 error when both fail, as it is the more informative one.
pub fn connect_backend(port: u16, timeout: Duration) -> io::Result<TcpStream> {
    let v4 = SocketAddr::from((Ipv4Addr::LOCALHOST, port));
    match TcpStream::connect_timeout(&v4, timeout) {
        Ok(stream) => Ok(stream),
        Err(first) => {
            let v6 = SocketAddr::from((Ipv6Addr::LOCALHOST, port));
            TcpStream::connect_timeout(&v6, timeout).map_err(|_| first)
        }
    }
}

/// Run the accept loop on `listener` forever, serving routes from `router`.
///
/// Each accepted connection gets its own small-stack thread. If the connection cap is
/// reached, or a thread cannot be spawned, the new connection is dropped.
pub fn serve(listener: TcpListener, router: Arc<Router>, options: Options) {
    let active = Arc::new(AtomicUsize::new(0));
    loop {
        let (stream, _peer) = match listener.accept() {
            Ok(accepted) => accepted,
            Err(e) => {
                eprintln!("hostd: accept failed: {e}");
                thread::sleep(ACCEPT_ERROR_BACKOFF);
                continue;
            }
        };

        if active.fetch_add(1, Ordering::AcqRel) >= options.max_connections {
            active.fetch_sub(1, Ordering::AcqRel);
            eprintln!(
                "hostd: connection limit ({}) reached, dropping a connection",
                options.max_connections
            );
            continue;
        }
        // From here the slot is released by `Slot::drop`, even if the spawn below fails
        // and the closure (which owns the slot) is dropped unrun.
        let slot = Slot(Arc::clone(&active));
        let router = Arc::clone(&router);
        let verbose = options.verbose;
        let spawned = thread::Builder::new()
            .name("conn".into())
            .stack_size(THREAD_STACK)
            .spawn(move || {
                let _slot = slot;
                handle(stream, &router, verbose);
            });
        if let Err(e) = spawned {
            eprintln!("hostd: cannot spawn connection thread: {e}");
        }
    }
}

/// Read one request head, decide where it goes, and either answer it or proxy it.
fn handle(client: TcpStream, router: &Router, verbose: bool) {
    // Requests and responses are small and latency matters more than packing; Nagle
    // would add up to ~40 ms to every dev-server round trip.
    let _ = client.set_nodelay(true);
    let _ = client.set_read_timeout(Some(HEAD_TIMEOUT));

    let head = match read_head(&mut &client) {
        Ok(head) => head,
        Err(HeadError::Closed) => return,
        Err(HeadError::TooLarge) => return respond(&client, &pages::head_too_large()),
        Err(HeadError::Malformed) => return respond(&client, &pages::bad_request()),
        Err(HeadError::Io(e)) => {
            if matches!(
                e.kind(),
                io::ErrorKind::WouldBlock | io::ErrorKind::TimedOut
            ) {
                respond(&client, &pages::request_timeout());
            }
            return;
        }
    };

    let config = router.snapshot();
    match resolve(&config, head.host()) {
        Resolution::Route { name, route } => forward(client, &head, name, route, verbose),
        Resolution::Index => respond(&client, &pages::index(&config, None)),
        Resolution::Missing(name) => respond(&client, &pages::index(&config, Some(&name))),
        Resolution::Foreign => respond(&client, &pages::not_found()),
    }
}

/// Write one of our own responses, then close politely.
fn respond(client: &TcpStream, bytes: &[u8]) {
    let _ = client.set_write_timeout(Some(ERROR_WRITE_TIMEOUT));
    let mut writer = client;
    if writer.write_all(bytes).is_err() {
        return;
    }
    let _ = client.shutdown(Shutdown::Write);
    // Drain whatever the client is still sending so closing does not turn into an RST
    // that destroys the response we just wrote (see `LINGER`).
    let _ = client.set_read_timeout(Some(LINGER));
    let mut reader = client;
    let mut sink = [0u8; 4096];
    let mut drained = 0;
    while drained < LINGER_MAX_BYTES {
        match reader.read(&mut sink) {
            Ok(0) | Err(_) => break,
            Ok(n) => drained += n,
        }
    }
}

/// Connect to the route's backend, replay the request head, then pipe both ways.
fn forward(client: TcpStream, head: &Head, name: &str, route: &Route, verbose: bool) {
    let backend = match connect_backend(route.port, CONNECT_TIMEOUT) {
        Ok(backend) => backend,
        Err(e) => {
            eprintln!("hostd: {name}: cannot reach port {}: {e}", route.port);
            return respond(&client, &pages::bad_gateway(name, route.port));
        }
    };
    if verbose {
        eprintln!("hostd: {name} -> 127.0.0.1:{}", route.port);
    }
    let _ = backend.set_nodelay(true);
    // After the head there is no deadline: WebSockets and SSE legitimately sit idle.
    let _ = client.set_read_timeout(None);

    let rewritten;
    let head_bytes: &[u8] = if route.rewrite_host {
        rewritten = head.with_host(&format!("127.0.0.1:{}", route.port));
        &rewritten
    } else {
        head.raw()
    };
    let mut writer = &backend;
    if let Err(e) = writer
        .write_all(head_bytes)
        .and_then(|()| writer.write_all(head.leftover()))
    {
        eprintln!(
            "hostd: {name}: writing request to port {} failed: {e}",
            route.port
        );
        return respond(&client, &pages::bad_gateway(name, route.port));
    }

    pipe(client, backend);
}

/// Copy bytes both ways between `client` and `backend` until the connection is done.
///
/// Client to backend runs on a helper thread; backend to client runs on this one.
///
/// - If the client finishes sending (EOF), only the backend's write side is shut down:
///   the response may still be on its way.
/// - If the backend finishes (EOF), nothing more can reach the client, so both sockets
///   are closed completely. That also wakes the helper thread out of its blocking read.
fn pipe(client: TcpStream, backend: TcpStream) {
    let (Ok(mut from_client), Ok(mut to_backend)) = (client.try_clone(), backend.try_clone())
    else {
        return;
    };

    let upstream = thread::Builder::new()
        .name("pipe-up".into())
        .stack_size(THREAD_STACK)
        .spawn(move || {
            if io::copy(&mut from_client, &mut to_backend).is_ok() {
                let _ = to_backend.shutdown(Shutdown::Write);
            } else {
                let _ = from_client.shutdown(Shutdown::Both);
                let _ = to_backend.shutdown(Shutdown::Both);
            }
        });
    let upstream = match upstream {
        Ok(handle) => handle,
        Err(e) => {
            eprintln!("hostd: cannot spawn pipe thread: {e}");
            return;
        }
    };

    let mut from_backend = &backend;
    let mut to_client = &client;
    let _ = io::copy(&mut from_backend, &mut to_client);
    let _ = client.shutdown(Shutdown::Both);
    let _ = backend.shutdown(Shutdown::Both);
    let _ = upstream.join();
}
