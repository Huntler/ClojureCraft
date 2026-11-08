//! `clojurecraft mcp`: the process an MCP client starts. It relays the client's stdio to the
//! running application's socket, starting the application first when it is not running.
//! Nothing but protocol messages may be written to stdout.

use std::io::{self, BufRead, BufReader, Write};
use std::net::Shutdown;
use std::os::unix::net::UnixStream;
use std::path::Path;
use std::thread;
use std::time::{Duration, Instant};

/// How long to wait for a just-started application to accept connections.
const START_TIMEOUT: Duration = Duration::from_secs(45);

/// Connects to `socket`, calling `launch` once if nobody is listening, and relays until either
/// side closes.
pub fn run(socket: &Path, launch: Option<&dyn Fn() -> io::Result<()>>) -> io::Result<()> {
    let stream = connect(socket, launch)?;
    let to_app = stream.try_clone()?;
    let relay_in = thread::spawn(move || {
        let _ = relay(io::stdin().lock(), to_app.try_clone());
        // The client closed stdin: let the application finish answering, then end.
        let _ = to_app.shutdown(Shutdown::Write);
    });
    let relayed = relay(BufReader::new(stream), Ok(io::stdout().lock()));
    drop(relay_in);
    relayed
}

/// Copies whole lines, flushing each one, so a message is never held back waiting for more
/// input (`io::copy` may splice between the pipe and the socket and wait to fill its buffer).
fn relay(mut from: impl BufRead, to: io::Result<impl Write>) -> io::Result<()> {
    let mut to = to?;
    let mut line = Vec::new();
    loop {
        line.clear();
        if from.read_until(b'\n', &mut line)? == 0 {
            return Ok(());
        }
        to.write_all(&line)?;
        to.flush()?;
    }
}

fn connect(socket: &Path, launch: Option<&dyn Fn() -> io::Result<()>>) -> io::Result<UnixStream> {
    match UnixStream::connect(socket) {
        Ok(s) => return Ok(s),
        Err(e) if launch.is_none() => {
            return Err(io::Error::new(
                e.kind(),
                format!("ClojureCraft is not running ({}): {e}", socket.display()),
            ));
        }
        Err(_) => {}
    }
    if let Some(launch) = launch {
        launch()?;
    }
    let deadline = Instant::now() + START_TIMEOUT;
    loop {
        match UnixStream::connect(socket) {
            Ok(s) => return Ok(s),
            Err(e) if Instant::now() >= deadline => {
                return Err(io::Error::new(
                    e.kind(),
                    format!(
                        "ClojureCraft was started but did not accept connections on {} within {} s: {e}",
                        socket.display(),
                        START_TIMEOUT.as_secs()
                    ),
                ));
            }
            Err(_) => thread::sleep(Duration::from_millis(200)),
        }
    }
}

#[cfg(test)]
mod tests {
    use std::cell::Cell;
    use std::os::unix::net::UnixListener;

    use super::*;

    #[test]
    fn a_missing_application_is_started_and_awaited() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("mcp.sock");
        assert!(connect(&path, None).is_err());
        let launched = Cell::new(0);
        let listener = std::cell::RefCell::new(None);
        let launch = || {
            launched.set(launched.get() + 1);
            *listener.borrow_mut() = Some(UnixListener::bind(&path)?);
            Ok(())
        };
        assert!(connect(&path, Some(&launch)).is_ok());
        assert_eq!(launched.get(), 1);
        assert!(connect(&path, Some(&launch)).is_ok());
        assert_eq!(launched.get(), 1, "a running application is not started again");
    }
}
