//! Just enough HTTP for one client, edge, which buffers every request and
//! sends it with a known length, one request per connection
//! (`proxy_http_version 1.0` in the engine's locations; never keepalive).
//!
//! A general HTTP library reads a request before the handler can look at who
//! sent it; here (floor 8):
//! - the peer is checked as the connection is accepted, before a byte is
//!   read; anything but edge is closed at once and never gets a thread;
//! - each connection has one deadline for the whole request;
//! - at most [`MAX_CONNECTIONS`] are handled at a time; more are closed;
//! - head and body are capped ([`MAX_HEAD`], [`MAX_HEADERS`], [`MAX_BODY`]);
//!   chunked bodies and a second `Content-Length` are refused;
//! - a thread that can't be started (the container's process cap) closes
//!   that one connection instead of taking the engine down, and a slot is
//!   given back even if its handler panics.

use std::io::{Read, Write};
use std::net::{Shutdown, SocketAddr, TcpListener, TcpStream};
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::Arc;
use std::thread;
use std::time::{Duration, Instant};

pub const MAX_HEAD: usize = 8 * 1024;
pub const MAX_HEADERS: usize = 64;
pub const MAX_BODY: usize = 8 * 1024;
pub const MAX_CONNECTIONS: usize = 16;
pub const DEADLINE: Duration = Duration::from_secs(5);

#[derive(Debug, Clone)]
pub struct Request {
    pub method: String,
    /// The request target as sent, query string included.
    pub url: String,
    pub headers: Vec<(String, String)>,
    pub body: Vec<u8>,
    pub peer: SocketAddr,
}

impl Request {
    pub fn header(&self, name: &str) -> Option<&str> {
        self.headers
            .iter()
            .find(|(k, _)| k.eq_ignore_ascii_case(name))
            .map(|(_, v)| v.as_str())
    }
}

#[derive(Debug)]
pub struct Reply {
    pub status: u16,
    pub body: String,
    pub content_type: &'static str,
    pub headers: Vec<(&'static str, &'static str)>,
}

/// Accept connections on `listener`; `admit` decides on the peer address alone.
pub fn serve<A, H>(listener: TcpListener, admit: A, handle: H)
where
    A: Fn(&SocketAddr) -> bool,
    H: Fn(Request) -> Reply + Send + Sync + 'static,
{
    let handle = Arc::new(handle);
    let busy = Arc::new(AtomicUsize::new(0));
    for stream in listener.incoming() {
        let Ok(stream) = stream else { continue };
        let Ok(peer) = stream.peer_addr() else {
            continue;
        };
        if !admit(&peer) || busy.load(Ordering::SeqCst) >= MAX_CONNECTIONS {
            let _ = stream.shutdown(Shutdown::Both);
            continue;
        }
        let slot = Slot::take(&busy);
        let handle = Arc::clone(&handle);
        let spawned = thread::Builder::new().spawn(move || {
            let _slot = slot;
            connection(stream, peer, &*handle);
        });
        if let Err(e) = spawned {
            // The closure, and with it the stream and the slot, is dropped.
            eprintln!("engine: no thread for a connection: {e}");
        }
    }
}

/// One of the [`MAX_CONNECTIONS`]; given back when dropped, panic or not.
struct Slot(Arc<AtomicUsize>);

impl Slot {
    fn take(busy: &Arc<AtomicUsize>) -> Self {
        busy.fetch_add(1, Ordering::SeqCst);
        Self(Arc::clone(busy))
    }
}

impl Drop for Slot {
    fn drop(&mut self) {
        self.0.fetch_sub(1, Ordering::SeqCst);
    }
}

fn connection(mut s: TcpStream, peer: SocketAddr, handle: &dyn Fn(Request) -> Reply) {
    let _ = s.set_write_timeout(Some(DEADLINE));
    let reply = match read_request(&mut s, peer, Instant::now() + DEADLINE) {
        Ok(req) => handle(req),
        Err((status, msg)) => Reply {
            status,
            body: format!("{{\"error\":\"{msg}\"}}"),
            content_type: "application/json",
            headers: vec![],
        },
    };
    let mut head = format!(
        "HTTP/1.1 {} {}\r\nContent-Type: {}\r\nContent-Length: {}\r\nCache-Control: no-store\r\nX-Content-Type-Options: nosniff\r\nConnection: close\r\n",
        reply.status,
        reason(reply.status),
        reply.content_type,
        reply.body.len()
    );
    for (k, v) in &reply.headers {
        head.push_str(&format!("{k}: {v}\r\n"));
    }
    head.push_str("\r\n");
    let _ = s.write_all(head.as_bytes());
    let _ = s.write_all(reply.body.as_bytes());
}

type Refusal = (u16, &'static str);

/// Read into `buf` until `done(buf)` says enough, or the deadline passes.
fn read_until(
    s: &mut TcpStream,
    buf: &mut Vec<u8>,
    deadline: Instant,
    limit: usize,
    done: impl Fn(&[u8]) -> bool,
) -> Result<(), Refusal> {
    let mut chunk = [0u8; 2048];
    while !done(buf) {
        let left = deadline.saturating_duration_since(Instant::now());
        if left.is_zero() {
            return Err((408, "request timeout"));
        }
        let _ = s.set_read_timeout(Some(left));
        match s.read(&mut chunk) {
            Ok(0) => return Err((400, "connection closed early")),
            Ok(n) => buf.extend_from_slice(&chunk[..n]),
            Err(_) => return Err((408, "request timeout")),
        }
        if buf.len() > limit {
            return Err((413, "too large"));
        }
    }
    Ok(())
}

fn read_request(
    s: &mut TcpStream,
    peer: SocketAddr,
    deadline: Instant,
) -> Result<Request, Refusal> {
    let mut buf = Vec::new();
    let end_of_head = |b: &[u8]| b.windows(4).position(|w| w == b"\r\n\r\n");
    read_until(s, &mut buf, deadline, MAX_HEAD + MAX_BODY, |b| {
        end_of_head(b).is_some()
    })?;
    let split = end_of_head(&buf).expect("read_until stopped on it");
    if split > MAX_HEAD {
        return Err((431, "headers too large"));
    }
    let head = std::str::from_utf8(&buf[..split])
        .map_err(|_| (400, "bad request"))?
        .to_string();
    let mut lines = head.split("\r\n");
    let mut first = lines.next().unwrap_or("").split(' ');
    let (Some(method), Some(url), Some(version), None) =
        (first.next(), first.next(), first.next(), first.next())
    else {
        return Err((400, "bad request line"));
    };
    if !matches!(version, "HTTP/1.0" | "HTTP/1.1") || !url.starts_with('/') {
        return Err((400, "bad request line"));
    }
    let mut headers: Vec<(String, String)> = Vec::new();
    for l in lines {
        if headers.len() == MAX_HEADERS {
            return Err((431, "too many headers"));
        }
        let (k, v) = l.split_once(':').ok_or((400, "bad header"))?;
        if k.is_empty()
            || k.bytes()
                .any(|b| b.is_ascii_whitespace() || b.is_ascii_control())
        {
            return Err((400, "bad header"));
        }
        headers.push((k.to_string(), v.trim().to_string()));
    }
    let named = |name: &str| {
        headers
            .iter()
            .filter(|(k, _)| k.eq_ignore_ascii_case(name))
            .map(|(_, v)| v.as_str())
            .collect::<Vec<_>>()
    };
    if !named("Transfer-Encoding").is_empty() {
        return Err((501, "send a Content-Length"));
    }
    let len: usize = match named("Content-Length").as_slice() {
        [] => 0,
        [v] if !v.is_empty() && v.bytes().all(|b| b.is_ascii_digit()) => {
            v.parse().map_err(|_| (413, "too large"))?
        }
        _ => return Err((400, "bad Content-Length")),
    };
    if len > MAX_BODY {
        return Err((413, "too large"));
    }
    let start = split + 4;
    read_until(s, &mut buf, deadline, start + len, |b| {
        b.len() >= start + len
    })?;
    Ok(Request {
        method: method.to_string(),
        url: url.to_string(),
        headers,
        body: buf[start..start + len].to_vec(),
        peer,
    })
}

fn reason(status: u16) -> &'static str {
    match status {
        200 => "OK",
        201 => "Created",
        202 => "Accepted",
        400 => "Bad Request",
        401 => "Unauthorized",
        403 => "Forbidden",
        404 => "Not Found",
        405 => "Method Not Allowed",
        408 => "Request Timeout",
        409 => "Conflict",
        413 => "Payload Too Large",
        429 => "Too Many Requests",
        431 => "Request Header Fields Too Large",
        500 => "Internal Server Error",
        501 => "Not Implemented",
        503 => "Service Unavailable",
        _ => "Error",
    }
}

#[cfg(test)]
pub(crate) mod tests {
    use super::*;

    /// A server on a free local port, admitting per `admit`.
    pub(crate) fn start<H>(admit: bool, handle: H) -> SocketAddr
    where
        H: Fn(Request) -> Reply + Send + Sync + 'static,
    {
        let l = TcpListener::bind("127.0.0.1:0").unwrap();
        let addr = l.local_addr().unwrap();
        thread::spawn(move || serve(l, move |_| admit, handle));
        addr
    }

    fn echo(admit: bool) -> SocketAddr {
        start(admit, |r| {
            assert!(r.url != "/panic", "handler panicked");
            Reply {
                status: 200,
                body: format!(
                    "{} {} {}",
                    r.method,
                    r.url,
                    String::from_utf8_lossy(&r.body)
                ),
                content_type: "text/plain",
                headers: vec![],
            }
        })
    }

    pub(crate) fn send(addr: SocketAddr, raw: &[u8]) -> String {
        let mut s = TcpStream::connect(addr).unwrap();
        s.set_read_timeout(Some(Duration::from_secs(8))).unwrap();
        let _ = s.write_all(raw);
        let mut out = String::new();
        let _ = s.read_to_string(&mut out);
        out
    }

    #[test]
    fn a_request_with_a_body_is_read_whole() {
        let a = echo(true);
        let out = send(
            a,
            b"POST /v1/pair HTTP/1.0\r\nContent-Length: 5\r\n\r\nhello",
        );
        assert!(out.starts_with("HTTP/1.1 200"), "{out}");
        assert!(out.ends_with("POST /v1/pair hello"), "{out}");
    }

    #[test]
    fn a_peer_that_isnt_admitted_is_closed_without_a_reply() {
        let a = echo(false);
        assert_eq!(send(a, b"GET /v1/version HTTP/1.0\r\n\r\n"), "");
    }

    #[test]
    fn a_slow_body_times_out_and_doesnt_block_others() {
        let a = echo(true);
        let mut slow = TcpStream::connect(a).unwrap();
        slow.write_all(b"POST /v1/pair HTTP/1.0\r\nContent-Length: 100\r\n\r\nab")
            .unwrap();
        let t0 = Instant::now();
        let out = send(a, b"GET /v1/version HTTP/1.0\r\n\r\n");
        assert!(out.starts_with("HTTP/1.1 200"), "{out}");
        assert!(
            t0.elapsed() < Duration::from_secs(1),
            "the slow one blocked"
        );
        let mut rest = String::new();
        slow.set_read_timeout(Some(Duration::from_secs(8))).unwrap();
        let _ = slow.read_to_string(&mut rest);
        assert!(rest.starts_with("HTTP/1.1 408"), "{rest}");
    }

    #[test]
    fn connections_past_the_cap_are_closed_and_slots_come_back() {
        let a = echo(true);
        let idle: Vec<TcpStream> = (0..MAX_CONNECTIONS)
            .map(|_| TcpStream::connect(a).unwrap())
            .collect();
        thread::sleep(Duration::from_millis(200));
        assert_eq!(
            send(a, b"GET /v1/version HTTP/1.0\r\n\r\n"),
            "",
            "over the cap"
        );
        drop(idle);
        // The idle ones end at their deadline (or now, closed), freeing slots.
        let t0 = Instant::now();
        loop {
            let out = send(a, b"GET /v1/version HTTP/1.0\r\n\r\n");
            if out.starts_with("HTTP/1.1 200") {
                break;
            }
            assert!(t0.elapsed() < DEADLINE * 2, "slots never came back");
            thread::sleep(Duration::from_millis(100));
        }
    }

    #[test]
    fn panicking_handlers_give_their_slot_back() {
        let a = echo(true);
        for _ in 0..MAX_CONNECTIONS + 4 {
            send(a, b"GET /panic HTTP/1.0\r\n\r\n");
        }
        let out = send(a, b"GET /v1/version HTTP/1.0\r\n\r\n");
        assert!(out.starts_with("HTTP/1.1 200"), "{out}");
    }

    #[test]
    fn malformed_oversized_and_ambiguous_requests_are_refused() {
        let a = echo(true);
        let status = |raw: &[u8]| send(a, raw)[9..12].to_string();
        assert_eq!(
            status(b"POST /x HTTP/1.0\r\nContent-Length: 99999\r\n\r\n"),
            "413"
        );
        assert_eq!(
            status(b"POST /x HTTP/1.1\r\nTransfer-Encoding: chunked\r\n\r\n0\r\n\r\n"),
            "501"
        );
        assert_eq!(
            status(b"POST /x HTTP/1.0\r\nContent-Length: 1\r\nContent-Length: 2\r\n\r\nab"),
            "400",
            "two lengths"
        );
        assert_eq!(
            status(b"POST /x HTTP/1.0\r\nContent-Length: +1\r\n\r\na"),
            "400"
        );
        assert_eq!(status(b"GET /x HTTP/1.0 extra\r\n\r\n"), "400");
        assert_eq!(status(b"GET /x HTTP/2.0\r\n\r\n"), "400");
        assert_eq!(status(b"GET x HTTP/1.0\r\n\r\n"), "400");
        assert_eq!(status(b"GET /x HTTP/1.0\r\nBad Header: 1\r\n\r\n"), "400");
        let mut many = b"GET /x HTTP/1.0\r\n".to_vec();
        for i in 0..MAX_HEADERS + 1 {
            many.extend_from_slice(format!("X-{i}: 1\r\n").as_bytes());
        }
        many.extend_from_slice(b"\r\n");
        assert_eq!(status(&many), "431");
        let mut big = b"GET /x HTTP/1.0\r\nX: ".to_vec();
        big.extend(std::iter::repeat_n(b'a', MAX_HEAD));
        big.extend_from_slice(b"\r\n\r\n");
        assert_eq!(status(&big), "431");
    }
}
