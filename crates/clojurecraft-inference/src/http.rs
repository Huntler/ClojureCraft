//! A small HTTP/1.1 client for the local llama.cpp server.
//!
//! The server is reached over a Unix-domain socket in a private directory (or, as a fallback,
//! TCP on the loopback interface). Reads poll the socket with a short timeout so that
//! cancellation, an overall deadline, and an idle limit take effect even while the server sends
//! nothing, for example during long prompt processing.

use std::io::{self, BufRead, BufReader, Read, Write};
use std::net::{Ipv4Addr, SocketAddr, TcpStream};
use std::os::unix::net::UnixStream;
use std::path::PathBuf;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::{Duration, Instant};

const POLL: Duration = Duration::from_millis(100);
const MAX_HEAD: usize = 64 * 1024;

/// A cancellation flag shared between the requester and the worker doing the request.
#[derive(Clone, Debug, Default)]
pub struct Cancel(Arc<AtomicBool>);

impl Cancel {
    pub fn cancel(&self) {
        self.0.store(true, Ordering::SeqCst);
    }
    pub fn is_cancelled(&self) -> bool {
        self.0.load(Ordering::SeqCst)
    }
}

/// Where the server listens.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Endpoint {
    Unix(PathBuf),
    Tcp(u16),
}

/// Limits applied while waiting for the server.
#[derive(Clone, Debug, Default)]
pub struct Control {
    pub cancel: Cancel,
    pub deadline: Option<Instant>,
    /// The limit `deadline` was derived from.
    pub timeout: Option<Duration>,
    /// Longest silence tolerated between received bytes.
    pub idle: Option<Duration>,
}

impl Control {
    pub fn new(cancel: Cancel, timeout: Option<Duration>, idle: Option<Duration>) -> Self {
        Self {
            cancel,
            deadline: timeout.map(|t| Instant::now() + t),
            timeout,
            idle,
        }
    }

    fn check(&self, last: Instant) -> Result<(), HttpError> {
        if self.cancel.is_cancelled() {
            return Err(HttpError::Cancelled);
        }
        if self.deadline.is_some_and(|d| Instant::now() >= d) {
            return Err(HttpError::Deadline);
        }
        if let Some(idle) = self.idle
            && last.elapsed() >= idle
        {
            return Err(HttpError::Idle(idle));
        }
        Ok(())
    }
}

#[derive(Clone, Debug, PartialEq, Eq, thiserror::Error)]
pub enum HttpError {
    #[error("cancelled")]
    Cancelled,
    #[error("deadline reached")]
    Deadline,
    #[error("no data for {0:?}")]
    Idle(Duration),
    #[error("could not connect: {0}")]
    Connect(String),
    #[error("connection failed: {0}")]
    Io(String),
    #[error("malformed HTTP response: {0}")]
    Protocol(String),
}

/// Carries an [`HttpError`] through `io::Read` so it can be recovered intact.
#[derive(Debug)]
struct Abort(HttpError);

impl std::fmt::Display for Abort {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        self.0.fmt(f)
    }
}
impl std::error::Error for Abort {}

fn from_io(e: io::Error) -> HttpError {
    if e.get_ref().is_some_and(|inner| inner.is::<Abort>())
        && let Some(inner) = e.into_inner()
        && let Ok(abort) = inner.downcast::<Abort>()
    {
        return abort.0;
    }
    HttpError::Io("connection closed or failed".into())
}

fn io_message(e: &io::Error) -> HttpError {
    HttpError::Io(e.to_string())
}

enum Stream {
    Unix(UnixStream),
    Tcp(TcpStream),
}

impl Stream {
    fn connect(endpoint: &Endpoint) -> io::Result<Self> {
        let s = match endpoint {
            Endpoint::Unix(path) => Self::Unix(UnixStream::connect(path)?),
            Endpoint::Tcp(port) => Self::Tcp(TcpStream::connect_timeout(
                &SocketAddr::from((Ipv4Addr::LOCALHOST, *port)),
                Duration::from_secs(5),
            )?),
        };
        match &s {
            Self::Unix(u) => {
                u.set_read_timeout(Some(POLL))?;
                u.set_write_timeout(Some(Duration::from_secs(10)))?;
            }
            Self::Tcp(t) => {
                t.set_read_timeout(Some(POLL))?;
                t.set_write_timeout(Some(Duration::from_secs(10)))?;
                t.set_nodelay(true)?;
            }
        }
        Ok(s)
    }
}

impl Read for Stream {
    fn read(&mut self, buf: &mut [u8]) -> io::Result<usize> {
        match self {
            Self::Unix(s) => s.read(buf),
            Self::Tcp(s) => s.read(buf),
        }
    }
}

impl Write for Stream {
    fn write(&mut self, buf: &[u8]) -> io::Result<usize> {
        match self {
            Self::Unix(s) => s.write(buf),
            Self::Tcp(s) => s.write(buf),
        }
    }
    fn flush(&mut self) -> io::Result<()> {
        match self {
            Self::Unix(s) => s.flush(),
            Self::Tcp(s) => s.flush(),
        }
    }
}

/// A socket whose reads keep polling until data arrives or the control says stop.
struct Polled {
    stream: Stream,
    control: Control,
    last: Instant,
}

impl Read for Polled {
    fn read(&mut self, buf: &mut [u8]) -> io::Result<usize> {
        loop {
            if let Err(e) = self.control.check(self.last) {
                return Err(io::Error::other(Abort(e)));
            }
            match self.stream.read(buf) {
                Ok(n) => {
                    self.last = Instant::now();
                    return Ok(n);
                }
                Err(e)
                    if matches!(
                        e.kind(),
                        io::ErrorKind::WouldBlock | io::ErrorKind::TimedOut | io::ErrorKind::Interrupted
                    ) => {}
                Err(e) => return Err(e),
            }
        }
    }
}

enum Framing {
    Length(u64),
    Chunked { left: u64, done: bool },
    UntilClose,
}

/// A response body, decoded from its transfer framing.
pub struct Body {
    inner: BufReader<Polled>,
    framing: Framing,
}

impl Body {
    fn chunk_size(&mut self) -> io::Result<u64> {
        let mut line = String::new();
        self.inner.read_line(&mut line)?;
        let hex = line.split(';').next().unwrap_or("").trim();
        u64::from_str_radix(hex, 16)
            .map_err(|_| io::Error::new(io::ErrorKind::InvalidData, format!("bad chunk size {hex:?}")))
    }
}

impl Read for Body {
    fn read(&mut self, buf: &mut [u8]) -> io::Result<usize> {
        match &mut self.framing {
            Framing::UntilClose => self.inner.read(buf),
            Framing::Length(left) => {
                if *left == 0 || buf.is_empty() {
                    return Ok(0);
                }
                let max = usize::try_from(*left).unwrap_or(usize::MAX).min(buf.len());
                let n = self.inner.read(&mut buf[..max])?;
                if n == 0 {
                    return Err(io::ErrorKind::UnexpectedEof.into());
                }
                if let Framing::Length(left) = &mut self.framing {
                    *left -= n as u64;
                }
                Ok(n)
            }
            Framing::Chunked { done: true, .. } => Ok(0),
            Framing::Chunked { left, .. } => {
                if *left == 0 {
                    let size = self.chunk_size()?;
                    if size == 0 {
                        // Trailer section ends with an empty line.
                        let mut line = String::new();
                        while self.inner.read_line(&mut line)? > 0 && !line.trim().is_empty() {
                            line.clear();
                        }
                        self.framing = Framing::Chunked { left: 0, done: true };
                        return Ok(0);
                    }
                    self.framing = Framing::Chunked {
                        left: size,
                        done: false,
                    };
                }
                let Framing::Chunked { left, .. } = &mut self.framing else {
                    return Ok(0);
                };
                let max = usize::try_from(*left).unwrap_or(usize::MAX).min(buf.len());
                let n = self.inner.read(&mut buf[..max])?;
                if n == 0 {
                    return Err(io::ErrorKind::UnexpectedEof.into());
                }
                *left -= n as u64;
                if *left == 0 {
                    let mut crlf = String::new();
                    self.inner.read_line(&mut crlf)?;
                }
                Ok(n)
            }
        }
    }
}

pub struct Response {
    pub status: u16,
    body: BufReader<Body>,
}

impl Response {
    /// Reads the whole body, refusing more than `max` bytes.
    pub fn text(mut self, max: usize) -> Result<String, HttpError> {
        let mut out = Vec::new();
        (&mut self.body)
            .take(max as u64 + 1)
            .read_to_end(&mut out)
            .map_err(from_io)?;
        if out.len() > max {
            return Err(HttpError::Protocol(format!("response larger than {max} bytes")));
        }
        Ok(String::from_utf8_lossy(&out).into_owned())
    }

    /// Reads one line of the body (without the line ending); `None` at the end. Lines longer than
    /// `max` bytes are refused.
    pub fn line(&mut self, max: usize) -> Result<Option<String>, HttpError> {
        let mut out = Vec::new();
        let n = (&mut self.body)
            .take(max as u64 + 1)
            .read_until(b'\n', &mut out)
            .map_err(from_io)?;
        if n == 0 {
            return Ok(None);
        }
        if out.len() > max && !out.ends_with(b"\n") {
            return Err(HttpError::Protocol(format!("line longer than {max} bytes")));
        }
        while out.last().is_some_and(|b| *b == b'\n' || *b == b'\r') {
            out.pop();
        }
        Ok(Some(String::from_utf8_lossy(&out).into_owned()))
    }
}

/// Sends one request on a fresh connection and returns once the response head is read.
pub fn request(
    endpoint: &Endpoint,
    api_key: Option<&str>,
    method: &str,
    path: &str,
    body: Option<&[u8]>,
    control: &Control,
) -> Result<Response, HttpError> {
    control.check(Instant::now())?;
    let mut stream = Stream::connect(endpoint).map_err(|e| HttpError::Connect(e.to_string()))?;
    let mut head =
        format!("{method} {path} HTTP/1.1\r\nHost: localhost\r\nConnection: close\r\nAccept: */*\r\n");
    if let Some(key) = api_key {
        head.push_str(&format!("Authorization: Bearer {key}\r\n"));
    }
    if let Some(b) = body {
        head.push_str(&format!(
            "Content-Type: application/json\r\nContent-Length: {}\r\n",
            b.len()
        ));
    }
    head.push_str("\r\n");
    stream.write_all(head.as_bytes()).map_err(|e| io_message(&e))?;
    if let Some(b) = body {
        stream.write_all(b).map_err(|e| io_message(&e))?;
    }
    stream.flush().map_err(|e| io_message(&e))?;

    let mut reader = BufReader::new(Polled {
        stream,
        control: control.clone(),
        last: Instant::now(),
    });
    let mut read_head = 0usize;
    let mut status_line = String::new();
    read_head += reader.read_line(&mut status_line).map_err(from_io)?;
    let status = parse_status(&status_line)?;
    let mut length = None;
    let mut chunked = false;
    loop {
        let mut line = String::new();
        let n = reader.read_line(&mut line).map_err(from_io)?;
        if n == 0 {
            return Err(HttpError::Protocol(
                "connection closed inside the response head".into(),
            ));
        }
        read_head += n;
        if read_head > MAX_HEAD {
            return Err(HttpError::Protocol("response head too large".into()));
        }
        let line = line.trim_end();
        if line.is_empty() {
            break;
        }
        let Some((name, value)) = line.split_once(':') else {
            return Err(HttpError::Protocol(format!("bad header line {line:?}")));
        };
        let value = value.trim();
        if name.eq_ignore_ascii_case("content-length") {
            length = Some(
                value
                    .parse::<u64>()
                    .map_err(|_| HttpError::Protocol(format!("bad content length {value:?}")))?,
            );
        } else if name.eq_ignore_ascii_case("transfer-encoding")
            && value.to_ascii_lowercase().contains("chunked")
        {
            chunked = true;
        }
    }
    let framing = if chunked {
        Framing::Chunked { left: 0, done: false }
    } else if let Some(n) = length {
        Framing::Length(n)
    } else {
        Framing::UntilClose
    };
    Ok(Response {
        status,
        body: BufReader::new(Body {
            inner: reader,
            framing,
        }),
    })
}

fn parse_status(line: &str) -> Result<u16, HttpError> {
    let mut parts = line.split_whitespace();
    match (parts.next(), parts.next()) {
        (Some(v), Some(code)) if v.starts_with("HTTP/1.") => code
            .parse()
            .map_err(|_| HttpError::Protocol(format!("bad status line {:?}", line.trim()))),
        _ => Err(HttpError::Protocol(format!("bad status line {:?}", line.trim()))),
    }
}

#[cfg(test)]
pub(crate) mod test_server {
    //! A scripted HTTP server on a Unix socket for protocol tests.

    use std::io::{Read, Write};
    use std::os::unix::net::UnixListener;
    use std::path::PathBuf;
    use std::thread;
    use std::time::Duration;

    /// What the server writes after reading a request; `Pause` waits before the next write.
    #[derive(Clone)]
    pub enum Part {
        Bytes(Vec<u8>),
        Pause(Duration),
    }

    pub fn chunked_sse(events: &[&str]) -> Vec<Part> {
        let mut parts = vec![Part::Bytes(
            b"HTTP/1.1 200 OK\r\nContent-Type: text/event-stream\r\nTransfer-Encoding: chunked\r\n\r\n"
                .to_vec(),
        )];
        for e in events {
            let payload = format!("data: {e}\n\n");
            parts.push(Part::Bytes(
                format!("{:x}\r\n{payload}\r\n", payload.len()).into_bytes(),
            ));
        }
        parts.push(Part::Bytes(b"0\r\n\r\n".to_vec()));
        parts
    }

    pub fn json(status: u16, body: &str) -> Vec<Part> {
        vec![Part::Bytes(
            format!(
                "HTTP/1.1 {status} X\r\nContent-Type: application/json\r\nContent-Length: {}\r\n\r\n{body}",
                body.len()
            )
            .into_bytes(),
        )]
    }

    /// Serves one scripted response per connection, in order. Returns the socket path; the
    /// requests received are sent back through the returned receiver.
    pub fn serve(
        dir: &std::path::Path,
        responses: Vec<Vec<Part>>,
    ) -> (PathBuf, std::sync::mpsc::Receiver<String>) {
        serve_at(dir.join("test.sock"), responses)
    }

    pub fn serve_at(
        path: PathBuf,
        responses: Vec<Vec<Part>>,
    ) -> (PathBuf, std::sync::mpsc::Receiver<String>) {
        let listener = UnixListener::bind(&path).expect("bind");
        let (tx, rx) = std::sync::mpsc::channel();
        thread::spawn(move || {
            for parts in responses {
                let Ok((mut conn, _)) = listener.accept() else {
                    return;
                };
                let mut req = Vec::new();
                let mut buf = [0u8; 4096];
                // Read the head and any declared body.
                loop {
                    let Ok(n) = conn.read(&mut buf) else { return };
                    if n == 0 {
                        break;
                    }
                    req.extend_from_slice(&buf[..n]);
                    let text = String::from_utf8_lossy(&req).into_owned();
                    if let Some(end) = text.find("\r\n\r\n") {
                        let len = text
                            .lines()
                            .find_map(|l| l.strip_prefix("Content-Length: "))
                            .and_then(|v| v.trim().parse::<usize>().ok())
                            .unwrap_or(0);
                        if req.len() >= end + 4 + len {
                            break;
                        }
                    }
                }
                let _ = tx.send(String::from_utf8_lossy(&req).into_owned());
                for p in parts {
                    match p {
                        Part::Bytes(b) => {
                            if conn.write_all(&b).is_err() {
                                break;
                            }
                            let _ = conn.flush();
                        }
                        Part::Pause(d) => thread::sleep(d),
                    }
                }
            }
        });
        (path, rx)
    }
}

#[cfg(test)]
mod tests {
    use super::test_server::{Part, chunked_sse, json, serve};
    use super::*;

    #[test]
    fn reads_chunked_event_streams_line_by_line() {
        let dir = tempfile::tempdir().unwrap();
        let (path, requests) = serve(dir.path(), vec![chunked_sse(&["{\"a\":1}", "[DONE]"])]);
        let mut r = request(
            &Endpoint::Unix(path),
            Some("key"),
            "POST",
            "/x",
            Some(b"{}"),
            &Control::default(),
        )
        .unwrap();
        assert_eq!(r.status, 200);
        let mut lines = Vec::new();
        while let Some(l) = r.line(1024).unwrap() {
            lines.push(l);
        }
        assert_eq!(lines, vec!["data: {\"a\":1}", "", "data: [DONE]", ""]);
        let req = requests.recv().unwrap();
        assert!(req.starts_with("POST /x HTTP/1.1\r\n"));
        assert!(req.contains("Authorization: Bearer key\r\n"));
        assert!(req.ends_with("\r\n\r\n{}"));
    }

    #[test]
    fn reads_length_delimited_bodies_and_refuses_oversized_ones() {
        let dir = tempfile::tempdir().unwrap();
        let (path, _) = serve(dir.path(), vec![json(400, "{\"e\":1}"), json(200, "0123456789")]);
        let ep = Endpoint::Unix(path);
        let r = request(&ep, None, "GET", "/", None, &Control::default()).unwrap();
        assert_eq!(r.status, 400);
        assert_eq!(r.text(100).unwrap(), "{\"e\":1}");
        let r = request(&ep, None, "GET", "/", None, &Control::default()).unwrap();
        assert!(matches!(r.text(5), Err(HttpError::Protocol(_))));
    }

    #[test]
    fn cancellation_interrupts_a_silent_server_promptly() {
        let dir = tempfile::tempdir().unwrap();
        let mut parts = chunked_sse(&["{}"]);
        parts.insert(1, Part::Pause(Duration::from_secs(5)));
        let (path, _) = serve(dir.path(), vec![parts]);
        let cancel = Cancel::default();
        let control = Control::new(cancel.clone(), None, None);
        let mut r = request(&Endpoint::Unix(path), None, "GET", "/", None, &control).unwrap();
        let c = cancel.clone();
        std::thread::spawn(move || {
            std::thread::sleep(Duration::from_millis(150));
            c.cancel();
        });
        let start = Instant::now();
        assert_eq!(r.line(1024), Err(HttpError::Cancelled));
        assert!(start.elapsed() < Duration::from_secs(1), "{:?}", start.elapsed());
    }

    #[test]
    fn idle_limit_and_deadline_end_waiting() {
        let dir = tempfile::tempdir().unwrap();
        let slow = vec![Part::Pause(Duration::from_secs(3))];
        let (path, _) = serve(dir.path(), vec![slow.clone(), slow]);
        let ep = Endpoint::Unix(path);
        let c = Control::new(Cancel::default(), None, Some(Duration::from_millis(300)));
        assert!(matches!(
            request(&ep, None, "GET", "/", None, &c),
            Err(HttpError::Idle(_))
        ));
        let c = Control::new(Cancel::default(), Some(Duration::from_millis(300)), None);
        assert!(matches!(
            request(&ep, None, "GET", "/", None, &c),
            Err(HttpError::Deadline)
        ));
    }

    #[test]
    fn truncated_stream_is_an_error_not_a_clean_end() {
        let dir = tempfile::tempdir().unwrap();
        let mut parts = chunked_sse(&["{\"a\":1}"]);
        parts.pop(); // no terminating chunk: the connection just closes
        let mut first = parts[1].clone();
        if let Part::Bytes(b) = &mut first {
            b.truncate(b.len() - 6);
        }
        parts[1] = first;
        let (path, _) = serve(dir.path(), vec![parts]);
        let mut r = request(&Endpoint::Unix(path), None, "GET", "/", None, &Control::default()).unwrap();
        let mut result = Ok(Some(String::new()));
        while let Ok(Some(_)) = result {
            result = r.line(1024);
        }
        assert!(matches!(result, Err(HttpError::Io(_))), "{result:?}");
    }

    #[test]
    fn missing_server_is_a_connect_error() {
        let dir = tempfile::tempdir().unwrap();
        let r = request(
            &Endpoint::Unix(dir.path().join("none.sock")),
            None,
            "GET",
            "/",
            None,
            &Control::default(),
        );
        assert!(matches!(r, Err(HttpError::Connect(_))));
    }
}
