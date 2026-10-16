//! Reading and rewriting the HTTP request head.
//!
//! hostd only needs the first request of a connection: the request line and headers,
//! up to the blank line. [`read_head`] collects exactly that (plus any body bytes that
//! arrived in the same read, kept as [`Head::leftover`]) and parses it with `httparse`.
//! Everything after the head is forwarded as opaque bytes.

use std::io::{self, Read};

/// Largest request head accepted, in bytes.
///
/// 16 KiB is far above what browsers send to a dev server (a few KiB with cookies) and
/// matches the order of magnitude of nginx/Apache defaults, while bounding per-connection
/// memory so a runaway client cannot grow the buffer without limit.
pub const MAX_HEAD: usize = 16 * 1024;

/// Header slots handed to `httparse`; more than this is treated as an oversized head.
///
/// 64 is double what a typical browser request carries and keeps the parse array on the
/// stack at 2 KiB, which matters because connection threads run with small stacks.
const MAX_HEADERS: usize = 64;

/// Why a request head could not be read.
#[derive(Debug)]
pub enum HeadError {
    /// The client closed the connection before sending anything.
    Closed,
    /// The head exceeded [`MAX_HEAD`] bytes or the header-count limit.
    TooLarge,
    /// Not valid HTTP/1.x, or ambiguous (for example two `Host` headers).
    Malformed,
    /// A socket error, including read timeouts.
    Io(io::Error),
}

/// A parsed request head plus the raw bytes it came from.
#[derive(Debug)]
pub struct Head {
    raw: Vec<u8>,
    leftover: Vec<u8>,
    method: String,
    target: String,
    minor: u8,
    headers: Vec<(String, Vec<u8>)>,
}

impl Head {
    /// The head exactly as received, including the terminating blank line.
    pub fn raw(&self) -> &[u8] {
        &self.raw
    }

    /// Bytes that were read past the head (start of the body, or pipelined data).
    pub fn leftover(&self) -> &[u8] {
        &self.leftover
    }

    /// The value of the `Host` header, if present and valid UTF-8.
    pub fn host(&self) -> Option<&str> {
        self.headers
            .iter()
            .find(|(name, _)| name.eq_ignore_ascii_case("host"))
            .and_then(|(_, value)| std::str::from_utf8(value).ok())
            .map(str::trim)
    }

    /// Re-serialise the head with `Host` replaced by `new_host`.
    ///
    /// The original host is preserved in `X-Forwarded-Host` (any client-supplied value
    /// of that header is dropped so it cannot be spoofed). All other headers, including
    /// `Content-Length` and `Transfer-Encoding`, are copied untouched so body framing is
    /// unaffected.
    pub fn with_host(&self, new_host: &str) -> Vec<u8> {
        let mut out = Vec::with_capacity(self.raw.len() + 64);
        out.extend_from_slice(self.method.as_bytes());
        out.push(b' ');
        out.extend_from_slice(self.target.as_bytes());
        out.extend_from_slice(format!(" HTTP/1.{}\r\n", self.minor).as_bytes());

        let mut wrote_host = false;
        for (name, value) in &self.headers {
            if name.eq_ignore_ascii_case("x-forwarded-host") {
                continue;
            }
            if name.eq_ignore_ascii_case("host") {
                out.extend_from_slice(format!("Host: {new_host}\r\n").as_bytes());
                wrote_host = true;
                continue;
            }
            out.extend_from_slice(name.as_bytes());
            out.extend_from_slice(b": ");
            out.extend_from_slice(value);
            out.extend_from_slice(b"\r\n");
        }
        if !wrote_host {
            out.extend_from_slice(format!("Host: {new_host}\r\n").as_bytes());
        }
        if let Some(original) = self.host() {
            out.extend_from_slice(format!("X-Forwarded-Host: {original}\r\n").as_bytes());
        }
        out.extend_from_slice(b"\r\n");
        out
    }
}

/// Read from `reader` until a complete request head has arrived, then parse it.
pub fn read_head<R: Read>(reader: &mut R) -> Result<Head, HeadError> {
    let mut buf: Vec<u8> = Vec::with_capacity(2048);
    let mut chunk = [0u8; 2048];

    loop {
        let n = match reader.read(&mut chunk) {
            Ok(n) => n,
            Err(e) if e.kind() == io::ErrorKind::Interrupted => continue,
            Err(e) => return Err(HeadError::Io(e)),
        };
        if n == 0 {
            // EOF: a clean close before any byte is routine (browser preconnects),
            // EOF mid-head is a broken request.
            return Err(if buf.is_empty() {
                HeadError::Closed
            } else {
                HeadError::Malformed
            });
        }
        buf.extend_from_slice(&chunk[..n]);

        let mut slots = [httparse::EMPTY_HEADER; MAX_HEADERS];
        let mut req = httparse::Request::new(&mut slots);
        match req.parse(&buf) {
            Ok(httparse::Status::Complete(head_len)) => {
                if head_len > MAX_HEAD {
                    return Err(HeadError::TooLarge);
                }
                let method = req.method.unwrap_or_default().to_owned();
                let target = req.path.unwrap_or_default().to_owned();
                let minor = req.version.unwrap_or(1);
                let headers: Vec<(String, Vec<u8>)> = req
                    .headers
                    .iter()
                    .map(|h| (h.name.to_owned(), h.value.to_vec()))
                    .collect();

                // Two Host headers are a classic request-smuggling/host-confusion
                // vector: we would route on one and the backend might trust the other.
                let hosts = headers
                    .iter()
                    .filter(|(name, _)| name.eq_ignore_ascii_case("host"))
                    .count();
                if hosts > 1 {
                    return Err(HeadError::Malformed);
                }

                let leftover = buf.split_off(head_len);
                return Ok(Head {
                    raw: buf,
                    leftover,
                    method,
                    target,
                    minor,
                    headers,
                });
            }
            Ok(httparse::Status::Partial) => {
                if buf.len() >= MAX_HEAD {
                    return Err(HeadError::TooLarge);
                }
            }
            Err(httparse::Error::TooManyHeaders) => return Err(HeadError::TooLarge),
            Err(_) => return Err(HeadError::Malformed),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Reader that hands out one byte per call, to exercise partial reads.
    struct Dribble<'a> {
        data: &'a [u8],
        pos: usize,
    }

    impl Read for Dribble<'_> {
        fn read(&mut self, buf: &mut [u8]) -> io::Result<usize> {
            if self.pos >= self.data.len() {
                return Ok(0);
            }
            buf[0] = self.data[self.pos];
            self.pos += 1;
            Ok(1)
        }
    }

    const GET: &[u8] = b"GET /x?y=1 HTTP/1.1\r\nHost: Health.localhost:80\r\nAccept: */*\r\n\r\n";

    #[test]
    fn parses_head_split_across_many_reads() {
        let mut r = Dribble { data: GET, pos: 0 };
        let head = read_head(&mut r).unwrap();
        assert_eq!(head.host(), Some("Health.localhost:80"));
        assert_eq!(head.raw(), GET);
        assert!(head.leftover().is_empty());
    }

    #[test]
    fn keeps_bytes_after_the_head_as_leftover() {
        let mut data =
            b"POST /up HTTP/1.1\r\nHost: a.localhost\r\nContent-Length: 4\r\n\r\n".to_vec();
        let head_len = data.len();
        data.extend_from_slice(b"BODY");
        let head = read_head(&mut data.as_slice()).unwrap();
        assert_eq!(head.raw().len(), head_len);
        assert_eq!(head.leftover(), b"BODY");
    }

    #[test]
    fn missing_host_is_none() {
        let head = read_head(&mut &b"GET / HTTP/1.0\r\n\r\n"[..]).unwrap();
        assert_eq!(head.host(), None);
    }

    #[test]
    fn two_host_headers_are_rejected() {
        let data = b"GET / HTTP/1.1\r\nHost: a.localhost\r\nhost: b.localhost\r\n\r\n";
        assert!(matches!(
            read_head(&mut &data[..]),
            Err(HeadError::Malformed)
        ));
    }

    #[test]
    fn oversized_head_is_rejected() {
        let mut data = b"GET / HTTP/1.1\r\nX-Pad: ".to_vec();
        data.extend(std::iter::repeat_n(b'a', MAX_HEAD * 2));
        assert!(matches!(
            read_head(&mut data.as_slice()),
            Err(HeadError::TooLarge)
        ));
    }

    #[test]
    fn too_many_headers_is_rejected() {
        let mut data = b"GET / HTTP/1.1\r\n".to_vec();
        for i in 0..(MAX_HEADERS + 5) {
            data.extend_from_slice(format!("X-{i}: v\r\n").as_bytes());
        }
        data.extend_from_slice(b"\r\n");
        assert!(matches!(
            read_head(&mut data.as_slice()),
            Err(HeadError::TooLarge)
        ));
    }

    #[test]
    fn eof_before_anything_is_closed_and_mid_head_is_malformed() {
        assert!(matches!(read_head(&mut &b""[..]), Err(HeadError::Closed)));
        assert!(matches!(
            read_head(&mut &b"GET / HT"[..]),
            Err(HeadError::Malformed)
        ));
    }

    #[test]
    fn garbage_is_malformed() {
        assert!(matches!(
            read_head(&mut &b"\x00\x01\x02\r\n\r\n"[..]),
            Err(HeadError::Malformed)
        ));
    }

    #[test]
    fn with_host_replaces_host_and_records_original() {
        let data = b"GET /a?b=1 HTTP/1.1\r\nHost: app.localhost\r\nX-Forwarded-Host: evil\r\nAccept: */*\r\n\r\n";
        let head = read_head(&mut &data[..]).unwrap();
        let out = head.with_host("127.0.0.1:5173");
        assert_eq!(
            String::from_utf8(out).unwrap(),
            "GET /a?b=1 HTTP/1.1\r\nHost: 127.0.0.1:5173\r\nAccept: */*\r\nX-Forwarded-Host: app.localhost\r\n\r\n"
        );
    }

    #[test]
    fn with_host_adds_host_when_absent() {
        let head = read_head(&mut &b"GET / HTTP/1.0\r\nAccept: */*\r\n\r\n"[..]).unwrap();
        let out = String::from_utf8(head.with_host("127.0.0.1:1")).unwrap();
        assert_eq!(
            out,
            "GET / HTTP/1.0\r\nAccept: */*\r\nHost: 127.0.0.1:1\r\n\r\n"
        );
    }
}
