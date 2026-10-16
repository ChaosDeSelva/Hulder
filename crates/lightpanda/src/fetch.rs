use anyhow::{Context as _, Result, anyhow};
use futures::AsyncReadExt as _;
use gpui::BackgroundExecutor;
use std::future::Future;
use std::path::Path;
use std::time::Duration;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Dump {
    Markdown,
    SemanticText,
}

impl Dump {
    pub fn label(self) -> &'static str {
        match self {
            Dump::Markdown => "Markdown",
            Dump::SemanticText => "semantic text",
        }
    }
}

pub fn fetch_args(dump: Dump, url: &str) -> Vec<String> {
    let dump = match dump {
        Dump::Markdown => "markdown",
        Dump::SemanticText => "semantic_tree_text",
    };
    ["fetch", "--dump", dump, "--strip-mode", "js,css", url]
        .iter()
        .map(|argument| argument.to_string())
        .collect()
}

pub const FETCH_TIMEOUT: Duration = Duration::from_secs(60);
pub const MAX_OUTPUT_BYTES: usize = 8 * 1024 * 1024;
const MAX_STDERR_BYTES: u64 = 64 * 1024;

/// Runs `lightpanda fetch` and returns its stdout. Lightpanda phones home by
/// default; the env var keeps the only sanctioned browser as quiet as the editor.
pub async fn fetch(
    binary: &Path,
    dump: Dump,
    url: &str,
    executor: &BackgroundExecutor,
) -> Result<String> {
    fetch_with_limits(
        binary,
        dump,
        url,
        executor.timer(FETCH_TIMEOUT),
        MAX_OUTPUT_BYTES,
    )
    .await
}

/// The child is killed when this future is dropped (the reader task was
/// cancelled), when `deadline` resolves, and when stdout exceeds `max_bytes`.
pub async fn fetch_with_limits(
    binary: &Path,
    dump: Dump,
    url: &str,
    deadline: impl Future<Output = ()>,
    max_bytes: usize,
) -> Result<String> {
    fetch_with_args(binary, fetch_args(dump, url), deadline, max_bytes).await
}

/// Like [`fetch_with_limits`] with the full argument list supplied, for
/// callers that add flags (the Hoard cookie header) before the URL.
pub async fn fetch_with_args(
    binary: &Path,
    args: Vec<String>,
    deadline: impl Future<Output = ()>,
    max_bytes: usize,
) -> Result<String> {
    let mut child = util::command::new_command(binary)
        .args(args)
        .env("LIGHTPANDA_DISABLE_TELEMETRY", "true")
        .stdin(util::command::Stdio::null())
        .stdout(util::command::Stdio::piped())
        .stderr(util::command::Stdio::piped())
        .kill_on_drop(true)
        .spawn()
        .with_context(|| format!("running {}", binary.display()))?;
    let stdout = child.stdout.take().context("lightpanda: no stdout pipe")?;
    let stderr = child.stderr.take().context("lightpanda: no stderr pipe")?;

    let read_stdout = async move {
        let mut bytes = Vec::new();
        // One byte past the cap tells an over-long page from an exact fit.
        stdout
            .take(max_bytes as u64 + 1)
            .read_to_end(&mut bytes)
            .await
            .context("reading lightpanda stdout")?;
        anyhow::Ok(bytes)
    };
    let read_stderr = async move {
        let mut bytes = Vec::new();
        stderr
            .take(MAX_STDERR_BYTES)
            .read_to_end(&mut bytes)
            .await
            .context("reading lightpanda stderr")?;
        anyhow::Ok(bytes)
    };
    let run = async {
        let (stdout, stderr) = futures::join!(read_stdout, read_stderr);
        let stdout = stdout?;
        anyhow::ensure!(
            stdout.len() <= max_bytes,
            "lightpanda output exceeded {} bytes",
            max_bytes
        );
        let status = child.status().await.context("waiting for lightpanda")?;
        if !status.success() {
            let stderr = stderr?;
            let stderr = String::from_utf8_lossy(&stderr);
            let head: String = stderr.chars().take(200).collect();
            return Err(anyhow!(
                "lightpanda fetch failed ({}): {}",
                status,
                head.trim()
            ));
        }
        Ok(String::from_utf8_lossy(&stdout).into_owned())
    };
    let expire = async {
        deadline.await;
        Err(anyhow!("lightpanda fetch timed out"))
    };
    futures::pin_mut!(run);
    futures::pin_mut!(expire);
    match futures::future::select(run, expire).await {
        futures::future::Either::Left((outcome, _))
        | futures::future::Either::Right((outcome, _)) => outcome,
    }
}

#[cfg(test)]
pub(crate) fn write_fake_binary(dir: &Path, script: &str) -> std::path::PathBuf {
    use std::os::unix::fs::PermissionsExt;
    let path = dir.join("fake-panda");
    std::fs::write(&path, script).expect("write script");
    std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o755)).expect("chmod");
    path
}

#[cfg(test)]
#[allow(
    clippy::disallowed_methods,
    reason = "these tests run real child processes, so deadlines need a wall clock, not the test scheduler's"
)]
mod tests {
    use super::*;

    fn wall(duration: Duration) -> impl Future<Output = ()> {
        async move {
            smol::Timer::after(duration).await;
        }
    }

    #[test]
    fn fetch_args_markdown() {
        assert_eq!(
            fetch_args(Dump::Markdown, "https://x.test/a"),
            vec![
                "fetch",
                "--dump",
                "markdown",
                "--strip-mode",
                "js,css",
                "https://x.test/a"
            ]
        );
    }

    #[test]
    fn fetch_args_semantic() {
        assert_eq!(
            fetch_args(Dump::SemanticText, "https://x.test/a"),
            vec![
                "fetch",
                "--dump",
                "semantic_tree_text",
                "--strip-mode",
                "js,css",
                "https://x.test/a"
            ]
        );
    }

    #[test]
    fn fetch_runs_binary_and_returns_stdout() {
        let dir = tempfile::tempdir().expect("tmp");
        let script = write_fake_binary(dir.path(), "#!/bin/sh\necho \"# hello from $6\"\n");
        let out = smol::block_on(fetch_with_limits(
            &script,
            Dump::Markdown,
            "https://x.test/a",
            wall(FETCH_TIMEOUT),
            MAX_OUTPUT_BYTES,
        ))
        .expect("ok");
        assert_eq!(out.trim(), "# hello from https://x.test/a");
    }

    #[test]
    fn fetch_reports_stderr_on_failure() {
        let dir = tempfile::tempdir().expect("tmp");
        let script = write_fake_binary(dir.path(), "#!/bin/sh\necho boom >&2\nexit 3\n");
        let error = smol::block_on(fetch_with_limits(
            &script,
            Dump::Markdown,
            "https://x.test/a",
            wall(FETCH_TIMEOUT),
            MAX_OUTPUT_BYTES,
        ))
        .err()
        .expect("err");
        assert!(error.to_string().contains("boom"), "{error}");
    }

    fn process_is_alive(pid: &str) -> bool {
        std::process::Command::new("kill")
            .args(["-0", pid])
            .status()
            .map(|status| status.success())
            .unwrap_or(false)
    }

    #[test]
    fn fetch_times_out_and_kills_the_child() {
        let dir = tempfile::tempdir().expect("tmp");
        let pid_file = dir.path().join("pid");
        let script = write_fake_binary(
            dir.path(),
            &format!(
                "#!/bin/sh\necho $$ > \"{}\"\nsleep 30\n",
                pid_file.display()
            ),
        );
        let error = smol::block_on(fetch_with_limits(
            &script,
            Dump::Markdown,
            "https://x.test/a",
            wall(Duration::from_secs(1)),
            MAX_OUTPUT_BYTES,
        ))
        .err()
        .expect("err");
        assert!(error.to_string().contains("timed out"), "{error}");

        // Under a loaded test runner the shell may still be starting up.
        let mut pid = String::new();
        for _ in 0..40 {
            if let Ok(text) = std::fs::read_to_string(&pid_file) {
                pid = text;
                break;
            }
            std::thread::sleep(Duration::from_millis(50));
        }
        let pid = pid.trim();
        assert!(!pid.is_empty(), "fake binary never wrote its pid");
        let mut alive = true;
        for _ in 0..40 {
            alive = process_is_alive(pid);
            if !alive {
                break;
            }
            std::thread::sleep(Duration::from_millis(50));
        }
        assert!(!alive, "child {pid} outlived the timed-out fetch");
    }

    #[test]
    fn fetch_refuses_output_over_the_cap() {
        let dir = tempfile::tempdir().expect("tmp");
        let script = write_fake_binary(
            dir.path(),
            "#!/bin/sh\nhead -c 100000 /dev/zero | tr '\\0' a\n",
        );
        let error = smol::block_on(fetch_with_limits(
            &script,
            Dump::Markdown,
            "https://x.test/a",
            wall(FETCH_TIMEOUT),
            1000,
        ))
        .err()
        .expect("err");
        assert!(error.to_string().contains("exceeded 1000 bytes"), "{error}");
    }

    #[test]
    fn fetch_reports_missing_binary() {
        let error = smol::block_on(fetch_with_limits(
            Path::new("/nonexistent/lightpanda"),
            Dump::Markdown,
            "https://x.test/a",
            wall(FETCH_TIMEOUT),
            MAX_OUTPUT_BYTES,
        ))
        .err()
        .expect("err");
        assert!(
            error.to_string().contains("/nonexistent/lightpanda"),
            "{error}"
        );
    }
}
