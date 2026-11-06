//! A minimal HTTP/1.1 server for tests: fixed routes, `Range: bytes=N-M` support, optional
//! bearer-token requirements, and a log of what was requested.

use std::collections::HashMap;
use std::io::{BufRead, BufReader, Write};
use std::net::TcpListener;
use std::sync::{Arc, Mutex};
use std::thread;

#[derive(Clone)]
pub struct Route {
    pub status: u16,
    pub body: Vec<u8>,
    /// Requests without `Authorization: Bearer <token>` get 401; with another token, 403.
    pub token: Option<String>,
}

impl Route {
    pub fn ok(body: impl Into<Vec<u8>>) -> Self {
        Self {
            status: 200,
            body: body.into(),
            token: None,
        }
    }

    pub fn status(status: u16) -> Self {
        Self {
            status,
            body: Vec::new(),
            token: None,
        }
    }

    pub fn gated(mut self, token: &str) -> Self {
        self.token = Some(token.to_owned());
        self
    }
}

#[derive(Clone, Debug, Default)]
pub struct Request {
    pub path: String,
    pub range: Option<(u64, Option<u64>)>,
    pub authorization: Option<String>,
}

pub struct Server {
    pub base: String,
    pub log: Arc<Mutex<Vec<Request>>>,
}

impl Server {
    pub fn requests(&self) -> Vec<Request> {
        self.log.lock().map(|l| l.clone()).unwrap_or_default()
    }

    /// Total body bytes the server was asked for on `path`.
    pub fn bytes_requested(&self, path: &str, full_len: u64) -> u64 {
        self.requests()
            .iter()
            .filter(|r| r.path == path)
            .map(|r| match r.range {
                Some((start, Some(end))) => end + 1 - start,
                Some((start, None)) => full_len - start,
                None => full_len,
            })
            .sum()
    }
}

pub fn serve(routes: HashMap<String, Route>) -> Server {
    let listener = TcpListener::bind("127.0.0.1:0").expect("bind");
    let addr = listener.local_addr().expect("addr");
    let log = Arc::new(Mutex::new(Vec::new()));
    let log2 = log.clone();
    thread::spawn(move || {
        for stream in listener.incoming() {
            let Ok(mut stream) = stream else { continue };
            let mut reader = BufReader::new(stream.try_clone().expect("clone"));
            let mut first = String::new();
            if reader.read_line(&mut first).unwrap_or(0) == 0 {
                continue;
            }
            let target = first.split_whitespace().nth(1).unwrap_or("/").to_owned();
            let path = target.split('?').next().unwrap_or("/").to_owned();
            let mut req = Request {
                path: path.clone(),
                ..Request::default()
            };
            loop {
                let mut line = String::new();
                if reader.read_line(&mut line).unwrap_or(0) == 0 || line == "\r\n" {
                    break;
                }
                let lower = line.to_ascii_lowercase();
                if let Some(r) = lower.strip_prefix("range: bytes=") {
                    let (a, b) = r.trim().split_once('-').unwrap_or((r.trim(), ""));
                    req.range = Some((a.parse().unwrap_or(0), b.parse().ok()));
                }
                if lower.starts_with("authorization:") {
                    req.authorization = Some(line["authorization:".len()..].trim().to_owned());
                }
            }
            if let Ok(mut l) = log2.lock() {
                l.push(req.clone());
            }
            let route = routes.get(&path).cloned().unwrap_or_else(|| Route::status(404));
            let mut status = route.status;
            if let Some(token) = &route.token {
                match &req.authorization {
                    None => status = 401,
                    Some(a) if *a != format!("Bearer {token}") => status = 403,
                    Some(_) => {}
                }
            }
            let body: &[u8] = if status == 200 { &route.body } else { b"" };
            let (status, slice) = match req.range {
                Some((start, end)) if status == 200 && (start as usize) < body.len() => {
                    let end = end.map_or(body.len(), |e| (e as usize + 1).min(body.len()));
                    (206, &body[start as usize..end])
                }
                _ => (status, body),
            };
            let head = format!(
                "HTTP/1.1 {status} X\r\nContent-Length: {}\r\nConnection: close\r\n\r\n",
                slice.len()
            );
            let _ = stream.write_all(head.as_bytes());
            let _ = stream.write_all(slice);
        }
    });
    Server {
        base: format!("http://{addr}"),
        log,
    }
}
