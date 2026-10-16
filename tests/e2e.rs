//! End-to-end tests: a real hostd accept loop in front of real TCP backends.
//!
//! Each test binds everything on port 0, so tests run in parallel without clashing and
//! never need privileges.

use hostd::config;
use hostd::proxy::{self, Options};
use hostd::router::Router;
use std::io::{Read, Write};
use std::net::{SocketAddr, TcpListener, TcpStream};
use std::sync::Arc;
use std::thread;
use std::time::Duration;

/// Deadline for every client read, so a hang fails the test instead of stalling it.
const IO_TIMEOUT: Duration = Duration::from_secs(5);

/// Read from `stream` until `\r\n\r\n`; returns the head text and any bytes past it.
///
/// Deliberately independent of hostd's own parser so the tests do not grade it with itself.
fn read_head_raw(stream: &mut TcpStream) -> (String, Vec<u8>) {
    let mut buf = Vec::new();
    let mut chunk = [0u8; 1024];
    loop {
        if let Some(pos) = buf.windows(4).position(|w| w == b"\r\n\r\n") {
            let rest = buf.split_off(pos + 4);
            return (String::from_utf8_lossy(&buf).into_owned(), rest);
        }
        let n = stream.read(&mut chunk).unwrap();
        assert!(n > 0, "connection closed before end of head");
        buf.extend_from_slice(&chunk[..n]);
    }
}

/// Value of header `name` (case-insensitive) in a raw head, if present.
fn header<'a>(head: &'a str, name: &str) -> Option<&'a str> {
    head.lines().find_map(|line| {
        let (key, value) = line.split_once(':')?;
        key.eq_ignore_ascii_case(name).then(|| value.trim())
    })
}

/// Start a backend that answers every request with a description of what it received.
///
/// The body lists the Host and X-Forwarded-Host headers and the request body, so tests
/// can assert exactly what hostd forwarded.
fn spawn_describing_backend() -> u16 {
    spawn_describing_backend_on("127.0.0.1:0").expect("bind loopback backend")
}

/// Like [`spawn_describing_backend`], bound to `addr`; `None` if the bind fails.
///
/// Lets a test ask for an address the machine may not support (IPv6 loopback).
fn spawn_describing_backend_on(addr: &str) -> Option<u16> {
    let listener = TcpListener::bind(addr).ok()?;
    let port = listener.local_addr().unwrap().port();
    thread::spawn(move || {
        for stream in listener.incoming() {
            let Ok(mut stream) = stream else { continue };
            thread::spawn(move || {
                let _ = stream.set_read_timeout(Some(IO_TIMEOUT));
                let (head, mut body) = read_head_raw(&mut stream);
                let want: usize = header(&head, "content-length")
                    .and_then(|v| v.parse().ok())
                    .unwrap_or(0);
                while body.len() < want {
                    let mut chunk = [0u8; 1024];
                    let n = stream.read(&mut chunk).unwrap();
                    assert!(n > 0, "body cut short");
                    body.extend_from_slice(&chunk[..n]);
                }
                let text = format!(
                    "host={}\nxfh={}\nbody={}\n",
                    header(&head, "host").unwrap_or("-"),
                    header(&head, "x-forwarded-host").unwrap_or("-"),
                    String::from_utf8_lossy(&body)
                );
                let reply = format!(
                    "HTTP/1.1 200 OK\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{text}",
                    text.len()
                );
                let _ = stream.write_all(reply.as_bytes());
            });
        }
    });
    Some(port)
}

/// Start a backend that upgrades the connection and then echoes every byte it receives.
///
/// Stands in for a WebSocket or SSE-style long-lived connection.
fn spawn_echo_backend() -> u16 {
    let listener = TcpListener::bind("127.0.0.1:0").unwrap();
    let port = listener.local_addr().unwrap().port();
    thread::spawn(move || {
        for stream in listener.incoming() {
            let Ok(mut stream) = stream else { continue };
            thread::spawn(move || {
                let _ = stream.set_read_timeout(Some(IO_TIMEOUT));
                let (_head, leftover) = read_head_raw(&mut stream);
                let _ = stream.write_all(
                    b"HTTP/1.1 101 Switching Protocols\r\nConnection: Upgrade\r\nUpgrade: echo\r\n\r\n",
                );
                if !leftover.is_empty() {
                    let _ = stream.write_all(&leftover);
                }
                let mut chunk = [0u8; 1024];
                while let Ok(n) = stream.read(&mut chunk) {
                    if n == 0 || stream.write_all(&chunk[..n]).is_err() {
                        break;
                    }
                }
            });
        }
    });
    port
}

/// A port that is guaranteed to have nothing listening on it right now.
fn dead_port() -> u16 {
    let listener = TcpListener::bind("127.0.0.1:0").unwrap();
    listener.local_addr().unwrap().port()
}

/// Start hostd on an ephemeral port with `config_text`; returns its address and router.
fn spawn_hostd(config_text: &str) -> (SocketAddr, Arc<Router>) {
    let listener = TcpListener::bind("127.0.0.1:0").unwrap();
    let addr = listener.local_addr().unwrap();
    let router = Arc::new(Router::new(config::parse(config_text).unwrap()));
    let served = Arc::clone(&router);
    thread::spawn(move || proxy::serve(listener, served, Options::default()));
    (addr, router)
}

/// Connect to `addr` with a read deadline.
fn connect(addr: SocketAddr) -> TcpStream {
    let stream = TcpStream::connect(addr).unwrap();
    stream.set_read_timeout(Some(IO_TIMEOUT)).unwrap();
    stream
}

/// Send `raw`, half-close, and read the whole response.
fn exchange(addr: SocketAddr, raw: &str) -> String {
    let mut stream = connect(addr);
    stream.write_all(raw.as_bytes()).unwrap();
    stream.shutdown(std::net::Shutdown::Write).unwrap();
    let mut out = String::new();
    stream.read_to_string(&mut out).unwrap();
    out
}

/// `GET /` with the given Host header.
fn get(addr: SocketAddr, host: &str) -> String {
    exchange(addr, &format!("GET / HTTP/1.1\r\nHost: {host}\r\n\r\n"))
}

#[test]
fn routes_by_host_header() {
    let a = spawn_describing_backend();
    let b = spawn_describing_backend();
    let (addr, _) = spawn_hostd(&format!("[routes]\nalpha = {a}\nbeta = {b}\n"));

    let resp = get(addr, "alpha.localhost");
    assert!(resp.starts_with("HTTP/1.1 200 OK"), "{resp}");
    assert!(resp.contains("host=alpha.localhost\n"), "{resp}");
    assert!(
        resp.contains("xfh=-\n"),
        "no rewrite means no forwarded header: {resp}"
    );

    let resp = get(addr, "BETA.localhost:80");
    assert!(resp.starts_with("HTTP/1.1 200 OK"), "{resp}");
    assert!(resp.contains("host=BETA.localhost:80\n"), "{resp}");
}

#[test]
fn backend_listening_only_on_ipv6_loopback_is_reached() {
    // Node-based dev servers often bind only ::1 for "localhost".
    let Some(port) = spawn_describing_backend_on("[::1]:0") else {
        eprintln!(
            "SKIPPED: this machine has no IPv6 loopback, so the ::1 fallback was NOT exercised"
        );
        return;
    };
    let (addr, _) = spawn_hostd(&format!("[routes]\nnode = {port}\n"));
    let resp = get(addr, "node.localhost");
    assert!(resp.starts_with("HTTP/1.1 200 OK"), "{resp}");
    assert!(resp.contains("host=node.localhost"), "{resp}");
}

#[test]
fn response_arrives_after_client_half_closes() {
    let port = spawn_describing_backend();
    let (addr, _) = spawn_hostd(&format!("[routes]\napp = {port}\n"));
    // `exchange` shuts down its write side right after sending the request.
    let resp = get(addr, "app.localhost");
    assert!(resp.contains("host=app.localhost"), "{resp}");
}

#[test]
fn rewrite_host_changes_host_and_records_original() {
    let port = spawn_describing_backend();
    let (addr, _) = spawn_hostd(&format!(
        "[routes]\nvite = {{ port = {port}, rewrite_host = true }}\n"
    ));
    let resp = exchange(
        addr,
        "GET / HTTP/1.1\r\nHost: vite.localhost\r\nX-Forwarded-Host: spoofed\r\n\r\n",
    );
    assert!(resp.contains(&format!("host=127.0.0.1:{port}\n")), "{resp}");
    assert!(resp.contains("xfh=vite.localhost\n"), "{resp}");
}

#[test]
fn request_body_is_forwarded_intact() {
    let port = spawn_describing_backend();
    let (addr, _) = spawn_hostd(&format!("[routes]\nup = {port}\n"));

    let mut stream = connect(addr);
    stream
        .write_all(b"POST /x HTTP/1.1\r\nHost: up.localhost\r\nContent-Length: 11\r\n\r\nhello")
        .unwrap();
    // Send the rest of the body in a separate write, after the head was already routed.
    thread::sleep(Duration::from_millis(100));
    stream.write_all(b" world").unwrap();
    let mut out = String::new();
    stream.read_to_string(&mut out).unwrap();
    assert!(out.contains("body=hello world\n"), "{out}");
}

#[test]
fn unknown_name_is_404_listing_routes() {
    let port = spawn_describing_backend();
    let (addr, _) = spawn_hostd(&format!("[routes]\nreal = {port}\n"));
    let resp = get(addr, "typo.localhost");
    assert!(resp.starts_with("HTTP/1.1 404"), "{resp}");
    assert!(resp.contains("typo"), "{resp}");
    assert!(resp.contains("real.localhost"), "{resp}");
}

#[test]
fn bare_suffix_shows_index() {
    let port = spawn_describing_backend();
    let (addr, _) = spawn_hostd(&format!("[routes]\nreal = {port}\n"));
    let resp = get(addr, "localhost");
    assert!(resp.starts_with("HTTP/1.1 200 OK"), "{resp}");
    assert!(resp.contains("real.localhost"), "{resp}");
}

#[test]
fn foreign_host_gets_bare_404_and_never_reaches_a_backend() {
    let port = spawn_describing_backend();
    let (addr, _) = spawn_hostd(&format!("[routes]\nsecret = {port}\n"));
    for host in ["evil.example", "127.0.0.1", "secret.localhost.evil.example"] {
        let resp = get(addr, host);
        assert!(resp.starts_with("HTTP/1.1 404"), "{host}: {resp}");
        assert!(
            !resp.contains("secret"),
            "route names must not leak: {resp}"
        );
        assert!(
            !resp.contains("host="),
            "must not reach the backend: {resp}"
        );
    }
    // No Host header at all (HTTP/1.0 style) is treated the same way.
    let resp = exchange(addr, "GET / HTTP/1.0\r\n\r\n");
    assert!(resp.starts_with("HTTP/1.1 404"), "{resp}");
}

#[test]
fn dead_backend_is_502_naming_route_and_port() {
    let port = dead_port();
    let (addr, _) = spawn_hostd(&format!("[routes]\ngone = {port}\n"));
    let resp = get(addr, "gone.localhost");
    assert!(resp.starts_with("HTTP/1.1 502"), "{resp}");
    assert!(resp.contains("gone"), "{resp}");
    assert!(resp.contains(&port.to_string()), "{resp}");
}

#[test]
fn malformed_and_oversized_heads_are_rejected() {
    let (addr, _) = spawn_hostd("");
    let resp = exchange(addr, "\u{1}\u{2}\u{3}\r\n\r\n");
    assert!(resp.starts_with("HTTP/1.1 400"), "{resp}");

    let big = format!(
        "GET / HTTP/1.1\r\nHost: a.localhost\r\nX-Pad: {}\r\n\r\n",
        "a".repeat(40_000)
    );
    let resp = exchange(addr, &big);
    assert!(resp.starts_with("HTTP/1.1 431"), "{resp}");

    let resp = exchange(
        addr,
        "GET / HTTP/1.1\r\nHost: a.localhost\r\nHost: b.localhost\r\n\r\n",
    );
    assert!(resp.starts_with("HTTP/1.1 400"), "{resp}");
}

#[test]
fn upgraded_connection_streams_both_ways_without_waiting_for_eof() {
    let port = spawn_echo_backend();
    let (addr, _) = spawn_hostd(&format!("[routes]\nws = {port}\n"));

    let mut stream = connect(addr);
    stream
        .write_all(b"GET /sock HTTP/1.1\r\nHost: ws.localhost\r\nConnection: Upgrade\r\nUpgrade: echo\r\n\r\n")
        .unwrap();
    let (head, _) = read_head_raw(&mut stream);
    assert!(head.starts_with("HTTP/1.1 101"), "{head}");

    // Several round trips on the still-open connection prove nothing is buffered until close.
    for word in ["ping", "pong", "again"] {
        stream.write_all(word.as_bytes()).unwrap();
        let mut got = vec![0u8; word.len()];
        stream.read_exact(&mut got).unwrap();
        assert_eq!(got, word.as_bytes());
    }
}

#[test]
fn backend_close_closes_the_client_connection() {
    let port = spawn_describing_backend();
    let (addr, _) = spawn_hostd(&format!("[routes]\napp = {port}\n"));

    // Keep the client's write side open: the proxy must still end the connection once
    // the backend is done, rather than waiting for the client to hang up.
    let mut stream = connect(addr);
    stream
        .write_all(b"GET / HTTP/1.1\r\nHost: app.localhost\r\n\r\n")
        .unwrap();
    let mut out = String::new();
    stream.read_to_string(&mut out).unwrap();
    assert!(out.contains("host=app.localhost"), "{out}");
}

#[test]
fn hot_swapped_routes_apply_to_new_connections() {
    let first = spawn_describing_backend();
    let second = spawn_describing_backend();
    let (addr, router) = spawn_hostd(&format!("[routes]\none = {first}\n"));

    assert!(get(addr, "one.localhost").starts_with("HTTP/1.1 200"));
    assert!(get(addr, "two.localhost").starts_with("HTTP/1.1 404"));

    router.replace(config::parse(&format!("[routes]\ntwo = {second}\n")).unwrap());

    assert!(get(addr, "one.localhost").starts_with("HTTP/1.1 404"));
    assert!(get(addr, "two.localhost").starts_with("HTTP/1.1 200"));
}

#[test]
fn many_concurrent_connections_all_succeed() {
    let port = spawn_describing_backend();
    let (addr, _) = spawn_hostd(&format!("[routes]\napp = {port}\n"));
    let handles: Vec<_> = (0..64)
        .map(|_| thread::spawn(move || get(addr, "app.localhost")))
        .collect();
    for handle in handles {
        let resp = handle.join().unwrap();
        assert!(resp.starts_with("HTTP/1.1 200 OK"), "{resp}");
    }
}

#[test]
fn connection_cap_drops_extra_connections_and_recovers() {
    let listener = TcpListener::bind("127.0.0.1:0").unwrap();
    let addr = listener.local_addr().unwrap();
    let router = Arc::new(Router::new(config::parse("").unwrap()));
    thread::spawn(move || {
        proxy::serve(
            listener,
            router,
            Options {
                max_connections: 2,
                verbose: false,
            },
        );
    });

    // Two idle connections occupy both slots (they are waiting for a request head).
    let idle_a = connect(addr);
    let idle_b = connect(addr);
    thread::sleep(Duration::from_millis(200));

    // A third is accepted by the kernel but dropped by hostd: reading gives EOF, no reply.
    let mut third = connect(addr);
    let mut out = Vec::new();
    let _ = third.read_to_end(&mut out);
    assert!(
        out.is_empty(),
        "expected a silent drop, got {:?}",
        String::from_utf8_lossy(&out)
    );

    // Freeing a slot lets new connections through again.
    drop(idle_a);
    drop(idle_b);
    thread::sleep(Duration::from_millis(300));
    let resp = get(addr, "localhost");
    assert!(resp.starts_with("HTTP/1.1 200"), "{resp}");
}
