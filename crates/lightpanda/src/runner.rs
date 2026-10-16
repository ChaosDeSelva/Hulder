use crate::settings::LightpandaSettings;
use gpui::{App, AppContext as _, Context, Entity, Global, Task};
use hulder_net::NetLog;
use settings::Settings as _;
use smol::io::{AsyncBufReadExt, BufReader};
use std::path::PathBuf;
use std::time::Duration;
use util::command::{Child, Stdio};

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum RunnerState {
    Stopped,
    Starting,
    Running { port: u16 },
    Failed { reason: String },
}

impl RunnerState {
    pub fn label(&self) -> &'static str {
        match self {
            RunnerState::Stopped => "stopped",
            RunnerState::Starting => "starting",
            RunnerState::Running { .. } => "running",
            RunnerState::Failed { .. } => "failed",
        }
    }
}

pub struct PandaRunner {
    pub state: RunnerState,
    binary: PathBuf,
    port: u16,
    obey_robots: bool,
    child: Option<Child>,
    _tail: Option<Task<()>>,
    _fallback: Option<Task<()>>,
}

struct GlobalPandaRunner(Entity<PandaRunner>);

impl Global for GlobalPandaRunner {}

/// Lightpanda logs one `$msg=navigate url=... type=root` line per top-level
/// page load; subframes carry another `type=` and are not pages the user asked for.
pub fn parse_page_url(line: &str) -> Option<String> {
    if !line.contains("$msg=navigate") {
        return None;
    }
    let mut url = None;
    for token in line.split_whitespace() {
        if let Some(value) = token.strip_prefix("url=") {
            url = Some(value.to_string());
        } else if let Some(kind) = token.strip_prefix("type=")
            && kind != "root"
        {
            return None;
        }
    }
    url
}

/// The line `lightpanda serve` prints once its CDP socket is bound.
pub fn is_listening_line(line: &str) -> bool {
    line.contains("$msg=\"server running\"")
}

impl PandaRunner {
    pub fn with_binary(binary: PathBuf, port: u16, obey_robots: bool) -> Self {
        Self {
            state: RunnerState::Stopped,
            binary,
            port,
            obey_robots,
            child: None,
            _tail: None,
            _fallback: None,
        }
    }

    pub fn from_settings(cx: &App) -> Self {
        let settings = LightpandaSettings::get_global(cx);
        Self::with_binary(settings.binary.clone(), settings.port, settings.obey_robots)
    }

    pub fn global(cx: &App) -> Entity<PandaRunner> {
        cx.global::<GlobalPandaRunner>().0.clone()
    }

    pub fn try_global(cx: &App) -> Option<Entity<PandaRunner>> {
        cx.try_global::<GlobalPandaRunner>()
            .map(|global| global.0.clone())
    }

    pub fn ws_url(&self) -> Option<String> {
        match self.state {
            RunnerState::Running { port } => Some(format!("ws://127.0.0.1:{port}")),
            _ => None,
        }
    }

    pub fn binary(&self) -> &PathBuf {
        &self.binary
    }

    pub fn port(&self) -> u16 {
        self.port
    }

    pub fn serve_args(port: u16, obey_robots: bool) -> Vec<String> {
        let mut args: Vec<String> = [
            "serve",
            "--host",
            "127.0.0.1",
            "--port",
            &port.to_string(),
            "--log-level",
            "info",
        ]
        .iter()
        .map(|argument| argument.to_string())
        .collect();
        if obey_robots {
            args.push("--obey-robots".into());
        }
        args
    }

    /// Picks up binary, port and robots changes; applied on the next start.
    pub fn apply_settings(&mut self, cx: &mut Context<Self>) {
        let settings = LightpandaSettings::get_global(cx);
        self.binary = settings.binary.clone();
        self.port = settings.port;
        self.obey_robots = settings.obey_robots;
        cx.notify();
    }

    pub fn toggle(&mut self, cx: &mut Context<Self>) {
        match self.state {
            RunnerState::Running { .. } | RunnerState::Starting => self.stop(cx),
            RunnerState::Stopped | RunnerState::Failed { .. } => self.start(cx),
        }
    }

    pub fn stop(&mut self, cx: &mut Context<Self>) {
        if let Some(mut child) = self.child.take()
            && let Err(error) = child.kill()
        {
            log::warn!("lightpanda: failed to kill serve process: {error}");
        }
        self._tail = None;
        self._fallback = None;
        self.state = RunnerState::Stopped;
        cx.notify();
    }

    pub fn start(&mut self, cx: &mut Context<Self>) {
        self.stop(cx);
        self.state = RunnerState::Starting;
        cx.notify();

        let spawned = util::command::new_command(&self.binary)
            .args(Self::serve_args(self.port, self.obey_robots))
            .env("LIGHTPANDA_DISABLE_TELEMETRY", "true")
            .stdin(Stdio::null())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .kill_on_drop(true)
            .spawn();
        let mut child = match spawned {
            Ok(child) => child,
            Err(error) => {
                self.state = RunnerState::Failed {
                    reason: format!("{}: {error}", self.binary.display()),
                };
                cx.notify();
                return;
            }
        };

        let stdout = child.stdout.take();
        let stderr = child.stderr.take();
        let port = self.port;
        self.child = Some(child);

        self._tail = Some(cx.spawn(async move |this, cx| {
            // Lightpanda logs to stderr; stdout is drained so the pipe never fills.
            let drain_stdout = cx.background_spawn(async move {
                let Some(stdout) = stdout else { return };
                let mut reader = BufReader::new(stdout);
                let mut buffer = Vec::new();
                while let Some(_) = next_lossy_line(&mut reader, &mut buffer).await {}
            });

            let mut first_stderr: Option<String> = None;
            if let Some(stderr) = stderr {
                let mut reader = BufReader::new(stderr);
                let mut buffer = Vec::new();
                while let Some(line) = next_lossy_line(&mut reader, &mut buffer).await {
                    if first_stderr.is_none() && !line.trim().is_empty() {
                        first_stderr = Some(line.clone());
                    }
                    if is_listening_line(&line) {
                        this.update(cx, |this, cx| {
                            if matches!(this.state, RunnerState::Starting) {
                                this.state = RunnerState::Running { port };
                                cx.notify();
                            }
                        })
                        .ok();
                    }
                    if let Some(url) = parse_page_url(&line) {
                        cx.update(|cx| NetLog::record_browsed(cx, url));
                    }
                }
            }

            // stderr closed: the process is gone.
            let reason = first_stderr.unwrap_or_else(|| "lightpanda exited".into());
            this.update(cx, |this, cx| {
                if !matches!(this.state, RunnerState::Stopped) {
                    this.state = RunnerState::Failed { reason };
                    this.child = None;
                    cx.notify();
                }
            })
            .ok();
            drain_stdout.await;
        }));

        // A process that stays up for a second is serving even if its listen line
        // was never seen (older builds or a changed log format). Stored so a
        // stop (or restart) in that second cancels it.
        self._fallback = Some(cx.spawn(async move |this, cx| {
            cx.background_executor().timer(Duration::from_secs(1)).await;
            this.update(cx, |this, cx| {
                if matches!(this.state, RunnerState::Starting) && this.child.is_some() {
                    this.state = RunnerState::Running { port };
                    cx.notify();
                }
            })
            .ok();
        }));
    }
}

/// The next line as lossy UTF-8, or `None` at end of stream or on a read
/// error. Lightpanda can log raw page bytes, which must not end the tail.
async fn next_lossy_line<R: smol::io::AsyncRead + Unpin>(
    reader: &mut BufReader<R>,
    buffer: &mut Vec<u8>,
) -> Option<String> {
    buffer.clear();
    match reader.read_until(b'\n', buffer).await {
        Ok(0) => None,
        Ok(_) => {
            let line = String::from_utf8_lossy(buffer);
            Some(line.trim_end_matches(['\n', '\r']).to_string())
        }
        Err(error) => {
            log::warn!("lightpanda: reading serve output failed: {error}");
            None
        }
    }
}

pub fn init(cx: &mut App) {
    let runner = cx.new(|cx| PandaRunner::from_settings(cx));
    cx.observe_global::<settings::SettingsStore>({
        let runner = runner.clone();
        move |cx| runner.update(cx, |runner, cx| runner.apply_settings(cx))
    })
    .detach();
    cx.on_app_quit({
        let runner = runner.clone();
        move |cx| {
            runner.update(cx, |runner, cx| runner.stop(cx));
            Task::ready(())
        }
    })
    .detach();
    cx.set_global(GlobalPandaRunner(runner));
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::fetch::write_fake_binary;
    use gpui::TestAppContext;

    #[test]
    fn serve_args_include_robots_when_asked() {
        assert_eq!(
            PandaRunner::serve_args(9333, true),
            vec![
                "serve",
                "--host",
                "127.0.0.1",
                "--port",
                "9333",
                "--log-level",
                "info",
                "--obey-robots"
            ]
        );
        assert_eq!(
            PandaRunner::serve_args(9333, false),
            vec![
                "serve",
                "--host",
                "127.0.0.1",
                "--port",
                "9333",
                "--log-level",
                "info"
            ]
        );
    }

    #[test]
    fn parse_page_url_finds_root_navigation_lines() {
        assert_eq!(
            parse_page_url(
                r#"$time=1791916712924 $scope=frame $level=info $msg=navigate url=https://example.com method=GET reason=address_bar body=false req_id=1 type=root"#
            ),
            Some("https://example.com".into())
        );
        assert_eq!(
            parse_page_url(r#"$time=1 $scope=page $level=info $msg=navigate url=https://a.test/p"#),
            Some("https://a.test/p".into())
        );
        assert_eq!(
            parse_page_url(
                r#"$time=1 $scope=frame $level=info $msg=navigate url=https://ads.test/f type=iframe"#
            ),
            None
        );
        assert_eq!(
            parse_page_url(
                r#"$time=1 $scope=cdp $level=info $msg="client connected" ip=127.0.0.1:5"#
            ),
            None
        );
    }

    #[test]
    fn listening_line_matches_real_output() {
        assert!(is_listening_line(
            r#"$time=1791916710829 $scope=app $level=info $msg="server running" address=127.0.0.1:9777"#
        ));
        assert!(!is_listening_line(
            r#"$time=1791916710829 $scope=telemetry $level=info $msg="telemetry status" disabled=true"#
        ));
    }

    /// The fake binaries are real child processes, so wait on the wall clock,
    /// bounded, rather than on the test executor's virtual timers.
    fn wait_until_not_starting(runner: &gpui::Entity<PandaRunner>, cx: &mut TestAppContext) {
        for _ in 0..400 {
            cx.run_until_parked();
            let starting = runner.read_with(cx, |runner, _| {
                matches!(runner.state, RunnerState::Starting)
            });
            if !starting {
                return;
            }
            std::thread::sleep(Duration::from_millis(25));
        }
    }

    #[gpui::test]
    async fn runner_reports_failure_when_binary_exits(cx: &mut TestAppContext) {
        cx.executor().allow_parking();
        let dir = tempfile::tempdir().expect("tmp");
        let script =
            write_fake_binary(dir.path(), "#!/bin/sh\necho 'address in use' >&2\nexit 1\n");
        let runner = cx.new(|_| PandaRunner::with_binary(script, 9555, false));
        runner.update(cx, |runner, cx| runner.start(cx));
        wait_until_not_starting(&runner, cx);
        runner.read_with(cx, |runner, _| {
            assert!(
                matches!(&runner.state, RunnerState::Failed { reason } if reason.contains("address in use")),
                "{:?}",
                runner.state
            );
            assert!(runner.ws_url().is_none());
        });
    }

    #[gpui::test]
    async fn runner_survives_non_utf8_output(cx: &mut TestAppContext) {
        cx.executor().allow_parking();
        let dir = tempfile::tempdir().expect("tmp");
        let script = write_fake_binary(
            dir.path(),
            concat!(
                "#!/bin/sh\n",
                "printf '\\377\\376 raw page bytes\\n' >&2\n",
                "echo '$time=1 $scope=app $level=info $msg=\"server running\" address=127.0.0.1:9556' >&2\n",
                "sleep 30\n",
            ),
        );
        let runner = cx.new(|_| PandaRunner::with_binary(script, 9556, false));
        runner.update(cx, |runner, cx| runner.start(cx));
        wait_until_not_starting(&runner, cx);
        runner.read_with(cx, |runner, _| {
            assert_eq!(runner.state, RunnerState::Running { port: 9556 });
        });
        runner.update(cx, |runner, cx| runner.stop(cx));
    }

    #[gpui::test]
    async fn runner_reports_missing_binary(cx: &mut TestAppContext) {
        let runner = cx.new(|_| {
            PandaRunner::with_binary(PathBuf::from("/nonexistent/lightpanda"), 9555, false)
        });
        runner.update(cx, |runner, cx| runner.start(cx));
        runner.read_with(cx, |runner, _| {
            assert!(
                matches!(&runner.state, RunnerState::Failed { reason } if reason.contains("/nonexistent/lightpanda")),
                "{:?}",
                runner.state
            );
        });
    }

    #[gpui::test]
    async fn runner_becomes_running_on_listen_line_and_logs_pages(cx: &mut TestAppContext) {
        cx.executor().allow_parking();
        cx.update(|cx| {
            let http_client = hulder_net::init(cx);
            cx.set_http_client(http_client);
        });
        let dir = tempfile::tempdir().expect("tmp");
        let script = write_fake_binary(
            dir.path(),
            concat!(
                "#!/bin/sh\n",
                "echo '$time=1 $scope=app $level=info $msg=\"server running\" address=127.0.0.1:9555' >&2\n",
                "echo '$time=2 $scope=frame $level=info $msg=navigate url=https://a.test/one type=root' >&2\n",
                "echo '$time=3 $scope=frame $level=info $msg=navigate url=https://a.test/two type=root' >&2\n",
                "sleep 30\n",
            ),
        );
        let runner = cx.new(|_| PandaRunner::with_binary(script, 9555, false));
        runner.update(cx, |runner, cx| runner.start(cx));
        wait_until_not_starting(&runner, cx);
        // The navigate lines follow the listen line; wait for the tail to read both.
        for _ in 0..400 {
            cx.run_until_parked();
            let browsed = cx.update(|cx| NetLog::global(cx).read(cx).browsed);
            if browsed >= 2 {
                break;
            }
            std::thread::sleep(Duration::from_millis(25));
        }
        runner.read_with(cx, |runner, _| {
            assert_eq!(runner.state, RunnerState::Running { port: 9555 });
            assert_eq!(runner.ws_url().as_deref(), Some("ws://127.0.0.1:9555"));
        });
        cx.update(|cx| {
            let log = NetLog::global(cx);
            let log = log.read(cx);
            assert_eq!(log.browsed, 2);
            let urls: Vec<&str> = log.entries().map(|entry| entry.url.as_str()).collect();
            assert_eq!(urls, vec!["https://a.test/two", "https://a.test/one"]);
        });
        runner.update(cx, |runner, cx| runner.stop(cx));
        runner.read_with(cx, |runner, _| {
            assert_eq!(runner.state, RunnerState::Stopped)
        });
    }
}
