//! Resumable, verified downloads.
//!
//! Bytes are written to `<dest>.part`. A later attempt continues from the part file's length with
//! an HTTP range request; if the server ignores the range, the part file is restarted. The final
//! file appears at `dest` only after its size and SHA-256 match the pinned values, so `dest`
//! never holds unverified content.

use std::fs::{self, File, OpenOptions};
use std::io::{self, Read, Write};
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::Duration;

use sha2::{Digest, Sha256};

use crate::manifest::Artifact;

/// Cooperative cancellation shared between a job and the UI.
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

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Progress {
    pub received: u64,
    pub total: u64,
}

#[derive(Debug, thiserror::Error)]
pub enum DownloadError {
    #[error("download cancelled")]
    Cancelled,
    #[error("network error: {0}")]
    Http(String),
    #[error("file system error at {path}: {source}")]
    Io { path: PathBuf, source: io::Error },
    #[error("expected {expected} bytes but the server sent {actual}")]
    SizeMismatch { expected: u64, actual: u64 },
    #[error("checksum mismatch: expected {expected}, got {actual}")]
    HashMismatch { expected: String, actual: String },
}

fn io_err(path: &Path) -> impl FnOnce(io::Error) -> DownloadError + '_ {
    move |source| DownloadError::Io {
        path: path.to_owned(),
        source,
    }
}

fn part_path(dest: &Path) -> PathBuf {
    let mut name = dest.file_name().unwrap_or_default().to_os_string();
    name.push(".part");
    dest.with_file_name(name)
}

fn agent() -> ureq::Agent {
    ureq::Agent::config_builder()
        .timeout_connect(Some(Duration::from_secs(30)))
        .timeout_recv_response(Some(Duration::from_secs(60)))
        .user_agent(concat!("ClojureCraft/", env!("CARGO_PKG_VERSION")))
        .build()
        .into()
}

/// Hashes a file on disk.
pub fn sha256_file(path: &Path) -> Result<String, DownloadError> {
    let mut f = File::open(path).map_err(io_err(path))?;
    let mut hasher = Sha256::new();
    let mut buf = vec![0u8; 1 << 20];
    loop {
        let n = f.read(&mut buf).map_err(io_err(path))?;
        if n == 0 {
            break;
        }
        hasher.update(&buf[..n]);
    }
    Ok(hex::encode(hasher.finalize()))
}

/// Downloads `artifact` to `dest`, resuming a previous partial download when possible.
///
/// If `dest` already exists with the pinned hash, nothing is downloaded. A corrupt existing file
/// or part file is discarded and fetched again.
pub fn download_verified(
    artifact: &Artifact,
    dest: &Path,
    cancel: &Cancel,
    mut progress: impl FnMut(Progress),
) -> Result<(), DownloadError> {
    if dest.exists() {
        if sha256_file(dest)? == artifact.sha256 {
            progress(Progress {
                received: artifact.size,
                total: artifact.size,
            });
            return Ok(());
        }
        fs::remove_file(dest).map_err(io_err(dest))?;
    }
    if let Some(parent) = dest.parent() {
        fs::create_dir_all(parent).map_err(io_err(parent))?;
    }
    let part = part_path(dest);
    let mut have = fs::metadata(&part).map(|m| m.len()).unwrap_or(0);
    if have > artifact.size {
        fs::remove_file(&part).map_err(io_err(&part))?;
        have = 0;
    }
    if have < artifact.size {
        fetch_into(artifact, &part, have, cancel, &mut progress)?;
    }
    let len = fs::metadata(&part).map_err(io_err(&part))?.len();
    if len != artifact.size {
        return Err(DownloadError::SizeMismatch {
            expected: artifact.size,
            actual: len,
        });
    }
    let actual = sha256_file(&part)?;
    if actual != artifact.sha256 {
        // A corrupt part file must not be resumed again.
        let _ = fs::remove_file(&part);
        return Err(DownloadError::HashMismatch {
            expected: artifact.sha256.clone(),
            actual,
        });
    }
    fs::rename(&part, dest).map_err(io_err(dest))?;
    Ok(())
}

fn fetch_into(
    artifact: &Artifact,
    part: &Path,
    have: u64,
    cancel: &Cancel,
    progress: &mut impl FnMut(Progress),
) -> Result<(), DownloadError> {
    let mut req = agent().get(&artifact.url);
    if have > 0 {
        req = req.header("Range", format!("bytes={have}-"));
    }
    let resp = req.call().map_err(|e| DownloadError::Http(e.to_string()))?;
    let resumed = have > 0 && resp.status().as_u16() == 206;
    let mut file = if resumed {
        OpenOptions::new().append(true).open(part).map_err(io_err(part))?
    } else {
        File::create(part).map_err(io_err(part))?
    };
    let mut received = if resumed { have } else { 0 };
    let mut body = resp.into_body();
    let mut reader = body.as_reader();
    let mut buf = vec![0u8; 256 * 1024];
    loop {
        if cancel.is_cancelled() {
            file.flush().map_err(io_err(part))?;
            return Err(DownloadError::Cancelled);
        }
        let n = reader
            .read(&mut buf)
            .map_err(|e| DownloadError::Http(e.to_string()))?;
        if n == 0 {
            break;
        }
        received += n as u64;
        if received > artifact.size {
            drop(file);
            let _ = fs::remove_file(part);
            return Err(DownloadError::SizeMismatch {
                expected: artifact.size,
                actual: received,
            });
        }
        file.write_all(&buf[..n]).map_err(io_err(part))?;
        progress(Progress {
            received,
            total: artifact.size,
        });
    }
    file.sync_all().map_err(io_err(part))?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::{BufRead, BufReader};
    use std::net::TcpListener;
    use std::thread;

    /// A tiny HTTP/1.1 server that serves `body` and honours `Range: bytes=N-`.
    /// When `cut_after` is set, the first full response is truncated after that many bytes.
    fn serve(body: Vec<u8>, honour_range: bool, cut_after: Option<usize>) -> String {
        let listener = TcpListener::bind("127.0.0.1:0").expect("bind");
        let addr = listener.local_addr().expect("addr");
        thread::spawn(move || {
            let mut first = true;
            for stream in listener.incoming() {
                let Ok(mut stream) = stream else { continue };
                let mut reader = BufReader::new(stream.try_clone().expect("clone"));
                let mut start = 0usize;
                loop {
                    let mut line = String::new();
                    if reader.read_line(&mut line).unwrap_or(0) == 0 || line == "\r\n" {
                        break;
                    }
                    let lower = line.to_ascii_lowercase();
                    if let Some(r) = lower.strip_prefix("range: bytes=") {
                        start = r.trim().trim_end_matches('-').parse().unwrap_or(0);
                    }
                }
                let (status, slice) = if honour_range && start > 0 {
                    ("206 Partial Content", &body[start..])
                } else {
                    ("200 OK", &body[..])
                };
                let head = format!(
                    "HTTP/1.1 {status}\r\nContent-Length: {}\r\nConnection: close\r\n\r\n",
                    slice.len()
                );
                let _ = stream.write_all(head.as_bytes());
                match cut_after {
                    Some(n) if first => {
                        let _ = stream.write_all(&slice[..n]);
                    }
                    _ => {
                        let _ = stream.write_all(slice);
                    }
                }
                first = false;
            }
        });
        format!("http://{addr}/file")
    }

    fn artifact(url: String, body: &[u8]) -> Artifact {
        Artifact {
            url,
            size: body.len() as u64,
            sha256: hex::encode(Sha256::digest(body)),
        }
    }

    fn payload() -> Vec<u8> {
        (0..300_000u32).map(|i| (i % 251) as u8).collect()
    }

    #[test]
    fn downloads_and_verifies() {
        let body = payload();
        let dir = tempfile::tempdir().expect("tmp");
        let dest = dir.path().join("f.bin");
        let a = artifact(serve(body.clone(), true, None), &body);
        download_verified(&a, &dest, &Cancel::default(), |_| {}).expect("download");
        assert_eq!(fs::read(&dest).expect("read"), body);
        assert!(!part_path(&dest).exists());
    }

    #[test]
    fn interrupted_download_resumes_with_range() {
        let body = payload();
        let dir = tempfile::tempdir().expect("tmp");
        let dest = dir.path().join("f.bin");
        let a = artifact(serve(body.clone(), true, Some(100_000)), &body);
        let first = download_verified(&a, &dest, &Cancel::default(), |_| {});
        assert!(first.is_err(), "truncated transfer must not verify");
        assert!(!dest.exists());
        assert_eq!(fs::metadata(part_path(&dest)).expect("part").len(), 100_000);
        let mut saw_resume_start = None;
        download_verified(&a, &dest, &Cancel::default(), |p| {
            saw_resume_start.get_or_insert(p.received);
        })
        .expect("resume");
        assert!(saw_resume_start.expect("progress") > 100_000);
        assert_eq!(fs::read(&dest).expect("read"), body);
    }

    #[test]
    fn server_ignoring_range_restarts_cleanly() {
        let body = payload();
        let dir = tempfile::tempdir().expect("tmp");
        let dest = dir.path().join("f.bin");
        fs::write(part_path(&dest), &body[..5000]).expect("seed part");
        let a = artifact(serve(body.clone(), false, None), &body);
        download_verified(&a, &dest, &Cancel::default(), |_| {}).expect("download");
        assert_eq!(fs::read(&dest).expect("read"), body);
    }

    #[test]
    fn tampered_content_is_rejected_and_not_kept() {
        let body = payload();
        let mut tampered = body.clone();
        tampered[1234] ^= 0xff;
        let dir = tempfile::tempdir().expect("tmp");
        let dest = dir.path().join("f.bin");
        let a = Artifact {
            url: serve(tampered, true, None),
            ..artifact(String::new(), &body)
        };
        let err = download_verified(&a, &dest, &Cancel::default(), |_| {}).expect_err("must fail");
        assert!(matches!(err, DownloadError::HashMismatch { .. }));
        assert!(!dest.exists());
        assert!(!part_path(&dest).exists());
    }

    #[test]
    fn oversized_response_is_rejected() {
        let body = payload();
        let dir = tempfile::tempdir().expect("tmp");
        let dest = dir.path().join("f.bin");
        let mut a = artifact(serve(body.clone(), true, None), &body);
        a.size = 1000;
        let err = download_verified(&a, &dest, &Cancel::default(), |_| {}).expect_err("must fail");
        assert!(matches!(err, DownloadError::SizeMismatch { .. }));
    }

    #[test]
    fn cancellation_keeps_part_for_resume() {
        let body = payload();
        let dir = tempfile::tempdir().expect("tmp");
        let dest = dir.path().join("f.bin");
        let a = artifact(serve(body.clone(), true, None), &body);
        let cancel = Cancel::default();
        let c2 = cancel.clone();
        let err = download_verified(&a, &dest, &cancel, |p| {
            if p.received > 0 {
                c2.cancel();
            }
        })
        .expect_err("cancelled");
        assert!(matches!(err, DownloadError::Cancelled));
        assert!(!dest.exists());
        assert!(part_path(&dest).exists());
    }
}
