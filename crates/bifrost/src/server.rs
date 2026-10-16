use anyhow::{Context as _, Result, bail};
use smol::io::{AsyncBufReadExt as _, AsyncReadExt as _, AsyncWriteExt as _, BufReader};
use smol::net::unix::{UnixListener, UnixStream};
use std::os::unix::fs::PermissionsExt as _;
use std::os::unix::io::AsRawFd as _;
use std::path::{Path, PathBuf};
use std::time::Duration;

pub const SOCKET_FILE: &str = "hulder.sock";

/// Longest request line we read; a client that sends more is cut off so it
/// cannot make the editor buffer without bound.
pub const MAX_LINE_BYTES: usize = 1024 * 1024;

/// First pause after a failed accept; it doubles per consecutive failure.
pub const ACCEPT_BACKOFF_FIRST: Duration = Duration::from_millis(500);
pub const ACCEPT_BACKOFF_MAX: Duration = Duration::from_secs(30);

pub fn socket_path() -> PathBuf {
    paths::config_dir().join(SOCKET_FILE)
}

/// Binds the bridge socket. A stale socket file from a previous run is removed;
/// a symlink is refused because binding through one would let another process
/// choose where the editor listens. The file is readable by the owner only.
pub fn bind(path: &Path) -> Result<UnixListener> {
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent)
            .with_context(|| format!("creating {}", parent.display()))?;
    }
    remove_socket(path)?;
    let listener = std::os::unix::net::UnixListener::bind(path)
        .with_context(|| format!("binding {}", path.display()))?;
    std::fs::set_permissions(path, std::fs::Permissions::from_mode(0o600))
        .with_context(|| format!("chmod 600 {}", path.display()))?;
    UnixListener::try_from(listener).context("registering the bridge socket with the reactor")
}

/// Removes the socket file (on quit, and before a fresh bind). A symlink is
/// refused and left alone; a missing file is fine.
pub fn remove_socket(path: &Path) -> Result<()> {
    match std::fs::symlink_metadata(path) {
        Ok(metadata) if metadata.file_type().is_symlink() => {
            bail!("{} is a symlink; refusing to touch it", path.display());
        }
        Ok(_) => std::fs::remove_file(path).with_context(|| format!("removing {}", path.display())),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(()),
        Err(error) => Err(error).with_context(|| format!("inspecting {}", path.display())),
    }
}

/// How long to wait before the next accept after `consecutive_failures`
/// failed in a row: 500ms, 1s, 2s, ... capped at thirty seconds, so a wedged
/// socket neither spins nor floods the log.
pub fn accept_backoff(consecutive_failures: u32) -> Duration {
    let doublings = consecutive_failures.saturating_sub(1).min(16);
    let delay = ACCEPT_BACKOFF_FIRST.saturating_mul(1u32 << doublings);
    delay.min(ACCEPT_BACKOFF_MAX)
}

/// The pid of the process at the other end of `stream`, when the platform
/// tells us (`LOCAL_PEERPID` on macOS, `SO_PEERCRED` on Linux).
pub fn peer_pid(stream: &UnixStream) -> Option<u32> {
    let fd = stream.as_raw_fd();
    #[cfg(target_os = "macos")]
    {
        let mut pid: libc::pid_t = 0;
        let mut length = std::mem::size_of::<libc::pid_t>() as libc::socklen_t;
        // SAFETY: fd is an open socket owned by `stream` for the duration of
        // the call, and the out-pointer and length describe `pid` exactly.
        let status = unsafe {
            libc::getsockopt(
                fd,
                libc::SOL_LOCAL,
                libc::LOCAL_PEERPID,
                (&mut pid as *mut libc::pid_t).cast(),
                &mut length,
            )
        };
        (status == 0 && pid > 0).then(|| pid as u32)
    }
    #[cfg(target_os = "linux")]
    {
        let mut credentials = libc::ucred {
            pid: 0,
            uid: 0,
            gid: 0,
        };
        let mut length = std::mem::size_of::<libc::ucred>() as libc::socklen_t;
        // SAFETY: as above; the out-pointer and length describe `credentials`.
        let status = unsafe {
            libc::getsockopt(
                fd,
                libc::SOL_SOCKET,
                libc::SO_PEERCRED,
                (&mut credentials as *mut libc::ucred).cast(),
                &mut length,
            )
        };
        (status == 0 && credentials.pid > 0).then(|| credentials.pid as u32)
    }
    #[cfg(not(any(target_os = "macos", target_os = "linux")))]
    {
        let _ = fd;
        None
    }
}

/// Answers every non-blank line on `stream` with one reply line, in order.
/// `handle` receives the raw request line and returns the full reply line.
/// A line longer than `MAX_LINE_BYTES` ends the connection with an error.
pub async fn serve_connection<F, Fut>(stream: UnixStream, mut handle: F) -> Result<()>
where
    F: FnMut(String) -> Fut,
    Fut: Future<Output = String>,
{
    let (reader, mut writer) = smol::io::split(stream);
    let mut reader = BufReader::new(reader);
    loop {
        let mut buffer = Vec::new();
        let read = (&mut reader)
            .take(MAX_LINE_BYTES as u64 + 1)
            .read_until(b'\n', &mut buffer)
            .await?;
        if read == 0 {
            return Ok(());
        }
        if buffer.len() > MAX_LINE_BYTES {
            bail!("request line longer than {MAX_LINE_BYTES} bytes; closing");
        }
        let line = String::from_utf8(buffer).context("request line is not UTF-8")?;
        if line.trim().is_empty() {
            continue;
        }
        let reply = handle(line).await;
        writer.write_all(reply.as_bytes()).await?;
        writer.flush().await?;
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::protocol::{Response, parse_request};

    #[test]
    fn bind_replaces_stale_file_and_sets_mode() {
        let dir = tempfile::tempdir().expect("tempdir");
        let path = dir.path().join("nested").join("hulder.sock");
        std::fs::create_dir_all(path.parent().expect("parent")).expect("mkdir");
        std::fs::write(&path, "stale").expect("write");
        let listener = bind(&path).expect("bind over stale file");
        let mode = std::fs::metadata(&path).expect("meta").permissions().mode() & 0o777;
        assert_eq!(mode, 0o600);
        drop(listener);
        remove_socket(&path).expect("remove on quit");
        assert!(!path.exists());
        remove_socket(&path).expect("a missing socket is fine");
    }

    #[test]
    fn bind_refuses_symlink() {
        let dir = tempfile::tempdir().expect("tempdir");
        let target = dir.path().join("target");
        std::fs::write(&target, "").expect("write");
        let link = dir.path().join("hulder.sock");
        std::os::unix::fs::symlink(&target, &link).expect("symlink");
        let error = bind(&link).expect_err("symlink must be refused");
        assert!(error.to_string().contains("symlink"));
        assert!(target.exists(), "the link target must be left alone");
        assert!(remove_socket(&link).is_err());
        assert!(
            link.symlink_metadata().is_ok(),
            "the link is left alone too"
        );
    }

    #[test]
    fn accept_backoff_doubles_and_caps() {
        assert_eq!(accept_backoff(0), ACCEPT_BACKOFF_FIRST);
        assert_eq!(accept_backoff(1), ACCEPT_BACKOFF_FIRST);
        assert_eq!(accept_backoff(2), Duration::from_secs(1));
        assert_eq!(accept_backoff(4), Duration::from_secs(4));
        assert_eq!(accept_backoff(10), ACCEPT_BACKOFF_MAX);
        assert_eq!(accept_backoff(u32::MAX), ACCEPT_BACKOFF_MAX);
    }

    #[test]
    fn round_trip_answers_requests_in_order_and_reports_the_peer_pid() {
        smol::block_on(async {
            let dir = tempfile::tempdir().expect("tempdir");
            let path = dir.path().join("hulder.sock");
            let listener = bind(&path).expect("bind");
            let server = smol::spawn(async move {
                let (stream, _) = listener.accept().await.expect("accept");
                assert_eq!(peer_pid(&stream), Some(std::process::id()));
                serve_connection(stream, |line| async move {
                    match parse_request(&line) {
                        Ok(request) => {
                            Response::ok(request.id, serde_json::json!({ "echo": request.method }))
                                .to_line()
                        }
                        Err(response) => response.to_line(),
                    }
                })
                .await
                .expect("serve");
            });

            let mut client = UnixStream::connect(&path).await.expect("connect");
            client
                .write_all(
                    b"{\"id\":\"1\",\"method\":\"project_root\",\"params\":{}}\n\n{\"id\":\"2\",\"method\":\"ping\",\"params\":{}}\nnot json\n",
                )
                .await
                .expect("write");
            client.close().await.expect("close write side");
            let mut reader = BufReader::new(client);
            let mut reply = String::new();
            reader.read_line(&mut reply).await.expect("read");
            assert_eq!(
                reply,
                "{\"id\":\"1\",\"result\":{\"echo\":\"project_root\"}}\n"
            );
            reply.clear();
            reader.read_line(&mut reply).await.expect("read");
            assert_eq!(reply, "{\"id\":\"2\",\"result\":{\"echo\":\"ping\"}}\n");
            reply.clear();
            reader.read_line(&mut reply).await.expect("read");
            let response: Response = serde_json::from_str(&reply).expect("error reply");
            assert_eq!(
                response.error.map(|error| error.code),
                Some("invalid_request".into())
            );
            server.await;
        });
    }

    #[test]
    fn an_overlong_line_closes_the_connection_without_a_reply() {
        smol::block_on(async {
            let dir = tempfile::tempdir().expect("tempdir");
            let path = dir.path().join("hulder.sock");
            let listener = bind(&path).expect("bind");
            let server = smol::spawn(async move {
                let (stream, _) = listener.accept().await.expect("accept");
                serve_connection(stream, |_| async move { "answered\n".to_string() }).await
            });

            let mut client = UnixStream::connect(&path).await.expect("connect");
            let flood = vec![b'x'; MAX_LINE_BYTES + 1];
            client.write_all(&flood).await.expect("write");
            let outcome = server.await.expect_err("the server must give up");
            assert!(outcome.to_string().contains("longer than"), "{outcome:#}");

            let mut reply = Vec::new();
            client
                .read_to_end(&mut reply)
                .await
                .expect("read until close");
            assert!(reply.is_empty(), "no reply is sent for the cut-off line");
        });
    }
}
