//! The application side: a Unix socket, readable and writable only by the user, on which each
//! connection speaks MCP. Tool calls are handed to the application's frame loop as [`Request`]s
//! (the application owns every function and service) and the connection waits for the reply.

use std::io::{BufRead, BufReader, Write};
use std::os::unix::fs::PermissionsExt;
use std::os::unix::net::{UnixListener, UnixStream};
use std::path::{Path, PathBuf};
use std::sync::mpsc::{self, Sender};
use std::sync::{Arc, Mutex};
use std::thread;
use std::time::Duration;

use crate::protocol::{self, Handler};
use crate::{Call, Reply};

/// How long a call may wait for the application before the agent is told it is not responding.
const REPLY_TIMEOUT: Duration = Duration::from_secs(60);

/// A tool call for the application to carry out.
pub struct Request {
    pub call: Call,
    reply: Sender<Reply>,
}

impl Request {
    /// Sends the outcome back to the waiting connection.
    pub fn respond(self, reply: Reply) {
        // The agent may have disconnected meanwhile; nothing is waiting then.
        let _ = self.reply.send(reply);
    }
}

/// The listening socket; removed when dropped.
pub struct Server {
    path: PathBuf,
}

impl Drop for Server {
    fn drop(&mut self) {
        let _ = std::fs::remove_file(&self.path);
    }
}

impl Server {
    /// Listens on `path`. Requests are sent on `requests`, followed by `wake` so the application
    /// handles them without waiting for input. Fails when another running instance already
    /// serves this socket.
    pub fn start(
        path: &Path,
        requests: Sender<Request>,
        wake: impl Fn() + Send + Sync + 'static,
    ) -> std::io::Result<Self> {
        if UnixStream::connect(path).is_ok() {
            return Err(std::io::Error::new(
                std::io::ErrorKind::AddrInUse,
                "another ClojureCraft window already serves MCP clients",
            ));
        }
        // Left behind by an application that did not exit cleanly.
        let _ = std::fs::remove_file(path);
        if let Some(parent) = path.parent() {
            std::fs::create_dir_all(parent)?;
        }
        let listener = UnixListener::bind(path)?;
        std::fs::set_permissions(path, std::fs::Permissions::from_mode(0o600))?;
        let handler = Arc::new(Channel {
            requests,
            wake: Box::new(wake),
        });
        thread::Builder::new().name("mcp".into()).spawn(move || {
            for stream in listener.incoming() {
                match stream {
                    Ok(stream) => {
                        let handler = handler.clone();
                        let spawned = thread::Builder::new()
                            .name("mcp-client".into())
                            .spawn(move || serve(stream, handler));
                        if let Err(e) = spawned {
                            tracing::warn!(error = %e, "could not serve an MCP client");
                        }
                    }
                    Err(e) => tracing::warn!(error = %e, "MCP connection failed"),
                }
            }
        })?;
        tracing::info!(socket = %path.display(), "serving MCP clients");
        Ok(Self {
            path: path.to_owned(),
        })
    }
}

struct Channel {
    requests: Sender<Request>,
    wake: Box<dyn Fn() + Send + Sync>,
}

impl Handler for Channel {
    fn call(&self, call: Call) -> Reply {
        let (tx, rx) = mpsc::channel();
        self.requests
            .send(Request { call, reply: tx })
            .map_err(|_| "ClojureCraft is shutting down.".to_owned())?;
        (self.wake)();
        rx.recv_timeout(REPLY_TIMEOUT).unwrap_or_else(|_| {
            Err(
                "ClojureCraft did not respond in time; it may be busy or no longer running. Try \
                 again."
                    .into(),
            )
        })
    }
}

/// Answers each message on its own thread, so a `status` that waits for work does not hold back
/// the client's other calls; responses carry their request's id and may arrive in any order.
fn serve(stream: UnixStream, handler: Arc<Channel>) {
    let Ok(out) = stream.try_clone() else {
        return;
    };
    let out = Arc::new(Mutex::new(out));
    for line in BufReader::new(stream).lines() {
        let Ok(line) = line else {
            return;
        };
        let (handler, out) = (handler.clone(), out.clone());
        let spawned = thread::Builder::new().name("mcp-call".into()).spawn(move || {
            if let Some(response) = protocol::handle_line(&line, &*handler) {
                let Ok(mut out) = out.lock() else { return };
                // A client that went away has nobody to read the answer.
                let _ = out
                    .write_all(response.as_bytes())
                    .and_then(|()| out.write_all(b"\n"))
                    .and_then(|()| out.flush());
            }
        });
        if let Err(e) = spawned {
            tracing::warn!(error = %e, "could not answer an MCP call");
        }
    }
}

#[cfg(test)]
mod tests {
    use std::io::{BufRead, BufReader, Write};

    use serde_json::{Value, json};

    use super::*;

    #[test]
    fn calls_reach_the_application_and_replies_reach_the_client() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("mcp.sock");
        let (tx, rx) = mpsc::channel::<Request>();
        let server = Server::start(&path, tx, || {}).unwrap();
        assert_eq!(
            std::fs::metadata(&path).unwrap().permissions().mode() & 0o777,
            0o600
        );
        let (tx2, _rx2) = mpsc::channel();
        assert!(Server::start(&path, tx2, || {}).is_err(), "one server per socket");

        // The application: answers every call.
        thread::spawn(move || {
            for r in rx {
                let answer = match r.call {
                    Call::ListFunctions(_) => Ok(json!({"functions": []})),
                    _ => Err("no".into()),
                };
                r.respond(answer);
            }
        });
        let mut client = UnixStream::connect(&path).unwrap();
        let request = json!({"jsonrpc": "2.0", "id": 7, "method": "tools/call",
                             "params": {"name": "list_functions"}});
        writeln!(client, "{request}").unwrap();
        let mut line = String::new();
        BufReader::new(client.try_clone().unwrap())
            .read_line(&mut line)
            .unwrap();
        let v: Value = serde_json::from_str(&line).unwrap();
        assert_eq!(v["id"], 7);
        assert_eq!(v["result"]["isError"], false);
        assert_eq!(v["result"]["content"][0]["text"], r#"{"functions":[]}"#);

        drop(server);
        assert!(!path.exists(), "the socket is removed on exit");
    }

    #[test]
    fn a_slow_call_does_not_hold_back_later_calls_of_the_same_client() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("mcp.sock");
        let (tx, rx) = mpsc::channel::<Request>();
        let _server = Server::start(&path, tx, || {}).unwrap();
        // The application: holds the first call (export) until the second (list) is answered.
        thread::spawn(move || {
            let slow = rx.recv().unwrap();
            let fast = rx.recv().unwrap();
            fast.respond(Ok(json!({"functions": []})));
            thread::sleep(Duration::from_millis(200));
            slow.respond(Err("late".into()));
        });
        let mut client = UnixStream::connect(&path).unwrap();
        let slow = json!({"jsonrpc": "2.0", "id": 1, "method": "tools/call",
                          "params": {"name": "export_project", "arguments":
                                     {"function_ids": [clojurecraft_domain::FunctionId::generate().to_string()], "directory": "/tmp/x"}}});
        let fast = json!({"jsonrpc": "2.0", "id": 2, "method": "tools/call",
                          "params": {"name": "list_functions"}});
        writeln!(client, "{slow}").unwrap();
        thread::sleep(Duration::from_millis(50));
        writeln!(client, "{fast}").unwrap();
        let mut reader = BufReader::new(client.try_clone().unwrap());
        let mut ids = Vec::new();
        for _ in 0..2 {
            let mut line = String::new();
            reader.read_line(&mut line).unwrap();
            let v: Value = serde_json::from_str(&line).unwrap();
            ids.push(v["id"].as_i64().unwrap());
        }
        assert_eq!(ids, [2, 1], "the fast answer comes first");
    }
}
