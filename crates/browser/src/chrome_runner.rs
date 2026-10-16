use crate::BrowserSettings;
use crate::pick::{DomNode, Pick, selector_for};
use anyhow::{Context as _, Result, anyhow};
use base64::Engine as _;
use cdp::{CdpClient, CdpSession, Event};
use futures::StreamExt;
use futures::channel::{mpsc, oneshot};
use gpui::{App, AppContext as _, BackgroundExecutor, Context, Entity, Global, Task};
use settings::Settings as _;
use std::future::Future;
use std::io;
use std::path::{Path, PathBuf};
use std::time::{Duration, SystemTime, UNIX_EPOCH};
use util::ResultExt as _;
use util::command::{Child, Stdio};

pub const CHROME_BINARY: &str = "/Applications/Google Chrome.app/Contents/MacOS/Google Chrome";
pub const EDGE_BINARY: &str = "/Applications/Microsoft Edge.app/Contents/MacOS/Microsoft Edge";

const PORT_FILE_TIMEOUT: Duration = Duration::from_secs(15);
const POLL_INTERVAL: Duration = Duration::from_millis(100);
/// How long the page stays in inspect mode waiting for a click.
const PICK_TIMEOUT: Duration = Duration::from_secs(120);
/// How long a `DevToolsActivePort` port gets to accept a probe connection.
const PORT_PROBE_TIMEOUT: Duration = Duration::from_millis(500);

/// The CDP client has no executor of its own; it times calls with ours.
fn sleeper(executor: &BackgroundExecutor) -> cdp::Sleep {
    let executor = executor.clone();
    std::sync::Arc::new(move |duration| {
        let timer = executor.timer(duration);
        Box::pin(async move {
            timer.await;
        })
    })
}

struct GlobalChromeRunner(Entity<ChromeRunner>);

impl Global for GlobalChromeRunner {}

pub fn init(cx: &mut App) {
    let runner = cx.new(|_| ChromeRunner::default());
    cx.on_app_quit({
        let runner = runner.clone();
        move |cx| {
            let executor = cx.foreground_executor().clone();
            runner.update(cx, |runner, _| runner.shutdown_clearing_cookies(&executor));
            async {}
        }
    })
    .detach();
    cx.set_global(GlobalChromeRunner(runner));
}

/// Owns the Chrome process launched by `browser::Open`, the CDP connection to
/// its browser endpoint, and the flat session attached to the page we opened.
#[derive(Default)]
pub struct ChromeRunner {
    child: Option<Child>,
    client: Option<CdpClient>,
    session: Option<CdpSession>,
    page_url: Option<String>,
    last_pick: Option<(Pick, Option<PathBuf>)>,
    /// The clipping file the last pick was saved as, so opening the pick
    /// again shows that file instead of saving a copy.
    last_pick_clipping: Option<PathBuf>,
    pick_task: Option<Task<()>>,
    /// A Hoard jar went into the running session and must be cleared before
    /// that Chrome goes away, so the profile on disk never keeps it.
    injected_cookies: bool,
}

impl ChromeRunner {
    pub fn global(cx: &App) -> Entity<ChromeRunner> {
        cx.global::<GlobalChromeRunner>().0.clone()
    }

    /// A session only counts while its connection is alive; the fields are
    /// cleared lazily by [`Self::live_client`] since this takes `&self`.
    pub fn is_open(&self) -> bool {
        self.session.is_some()
            && self
                .client
                .as_ref()
                .is_some_and(|client| !client.is_closed())
    }

    pub fn page_url(&self) -> Option<&str> {
        self.page_url.as_deref()
    }

    pub fn last_pick(&self) -> Option<&(Pick, Option<PathBuf>)> {
        self.last_pick.as_ref()
    }

    /// A new pick forgets the clipping of the previous one.
    pub fn set_last_pick(&mut self, pick: Pick, screenshot_path: Option<PathBuf>) {
        self.last_pick = Some((pick, screenshot_path));
        self.last_pick_clipping = None;
    }

    pub fn last_pick_clipping(&self) -> Option<&Path> {
        self.last_pick_clipping.as_deref()
    }

    pub fn remember_pick_clipping(&mut self, path: PathBuf) {
        if self.last_pick.is_some() {
            self.last_pick_clipping = Some(path);
        }
    }

    pub fn user_data_dir() -> PathBuf {
        paths::data_dir().join("chrome")
    }

    /// Kills the Chrome we launched. Also runs from `on_app_quit`, since a
    /// debug-port browser must not outlive the editor that controls it.
    /// Like [`Self::shutdown`], first clearing an injected Hoard jar out of
    /// the session, bounded by a second so quitting never hangs.
    pub fn shutdown_clearing_cookies(&mut self, executor: &gpui::ForegroundExecutor) {
        if self.injected_cookies
            && let Some(session) = self.session.clone()
            && self.client.as_ref().is_some_and(|client| !client.is_closed())
        {
            let cleared = executor.block_with_timeout(Duration::from_secs(1), async move {
                crate::hoard_link::clear_cookies(&session).await
            });
            match cleared {
                Ok(Err(error)) => log::warn!("browser: clearing Hoard cookies on quit: {error:#}"),
                Err(_) => log::warn!("browser: clearing Hoard cookies on quit timed out"),
                Ok(Ok(())) => self.injected_cookies = false,
            }
        }
        self.shutdown();
    }

    pub fn shutdown(&mut self) {
        self.pick_task = None;
        self.session = None;
        self.client = None;
        if let Some(mut child) = self.child.take() {
            child.kill().log_err();
        }
    }

    /// The connection is reused while the Chrome we launched is still running
    /// and its socket is open; otherwise the stale handles are dropped so
    /// `is_open` stops answering yes and the next open launches afresh.
    fn live_client(&mut self) -> Option<CdpClient> {
        let client_alive = self
            .client
            .as_ref()
            .is_some_and(|client| !client.is_closed());
        let child_alive = self
            .child
            .as_mut()
            .is_some_and(|child| matches!(child.try_status(), Ok(None)));
        if client_alive && child_alive {
            return self.client.clone();
        }
        if !child_alive {
            self.child = None;
        }
        self.client = None;
        self.session = None;
        None
    }

    /// The browser-level connection to the Chrome we launched, for the Hoard's
    /// cookie borrow. `None` once that Chrome or its socket is gone.
    pub fn borrow_client(&mut self) -> Option<CdpClient> {
        self.live_client()
    }

    pub fn open(&mut self, url: String, cx: &mut Context<Self>) -> Task<Result<()>> {
        let url = match normalize_url(&url) {
            Ok(url) => url,
            Err(error) => return Task::ready(Err(error)),
        };
        // A matching Hoard `cookies` profile is injected before the first
        // navigation, so the picked element can come from a logged-in page.
        let cookies = match crate::hoard_link::target_of(&url) {
            Some((scheme, host)) => hoard::cookies_for_host(&host, &scheme, cx),
            None => Task::ready(Ok(None)),
        };
        let binary = BrowserSettings::get_global(cx).binary.clone();
        let existing_client = self.live_client();
        // A child whose socket we lost is still holding the profile; it is
        // either reconnected to below or killed before a relaunch.
        let stale_child = if existing_client.is_none() {
            self.child.take()
        } else {
            None
        };
        let user_data_dir = Self::user_data_dir();
        let executor = cx.background_executor().clone();
        cx.spawn(async move |this, cx| {
            let cookies = match cookies.await {
                Ok(Some((_, cookies))) => cookies,
                Ok(None) => Vec::new(),
                Err(error) => {
                    log::warn!("browser: Hoard cookies not applied: {error:#}");
                    Vec::new()
                }
            };
            let injected = !cookies.is_empty();
            let launch_url = crate::hoard_link::launch_url_for(&url, injected);
            let (child, client, session) = cx
                .background_spawn({
                    let page_url = url.clone();
                    let url = launch_url;
                    async move {
                        let (child, client, session) = connect(existing_client, stale_child, user_data_dir, binary, url, &executor).await?;
                        if !cookies.is_empty() {
                            crate::hoard_link::inject_and_navigate(&client, &session, &cookies, &page_url).await?;
                        }
                        anyhow::Ok((child, client, session))
                    }
                })
                .await?;
            this.update(cx, |this, cx| {
                if let Some(child) = child {
                    this.child = Some(child);
                }
                this.client = Some(client);
                this.session = Some(session);
                this.page_url = Some(url);
                this.injected_cookies = this.injected_cookies || injected;
                cx.notify();
            })
        })
    }
}

/// Reuses the live connection, reconnects to a Chrome already holding the
/// profile, or launches one; `url` is the page it opens on.
async fn connect(
    existing_client: Option<CdpClient>,
    stale_child: Option<Child>,
    user_data_dir: PathBuf,
    binary: Option<PathBuf>,
    url: String,
    executor: &BackgroundExecutor,
) -> Result<(Option<Child>, CdpClient, CdpSession)> {
    if let Some(client) = existing_client {
        match open_in_existing(&client, &url).await {
            Ok(session) => return Ok((None, client, session)),
            Err(error) => {
                log::warn!("browser: reusing Chrome failed, relaunching: {error:#}")
            }
        }
    }
    // Chrome is a singleton per profile: a second launch would hand the
    // URL to the running instance and exit, so connect to it instead.
    let port_file = user_data_dir.join("DevToolsActivePort");
    if let Some(ws_url) = live_port_file(&port_file, executor.timer(PORT_PROBE_TIMEOUT)).await {
        match reconnect(&ws_url, &url, executor).await {
            Ok((client, session)) => return Ok((stale_child, client, session)),
            Err(error) => log::warn!(
                "browser: a Hulder Chrome is already running but could not be reused, relaunching: {error:#}"
            ),
        }
    }
    if let Some(mut stale_child) = stale_child {
        stale_child.kill().log_err();
        stale_child.status().await.log_err();
    }
    let binary = match binary {
        Some(binary) => binary,
        None => default_binary()?,
    };
    let (child, ws_url) = launch(binary, user_data_dir, url.clone(), executor).await?;
    let client = CdpClient::connect(&ws_url, sleeper(executor)).await?;
    let session = attach_page(&client, &url, executor).await?;
    Ok((Some(child), client, session))
}

impl ChromeRunner {
    pub fn is_picking(&self) -> bool {
        self.pick_task.is_some()
    }

    /// Only one pick runs at a time: starting another cancels the previous
    /// one, and the stored task dies with the runner. The result travels
    /// over a oneshot so the caller still gets it (or "cancelled").
    pub fn pick_element(
        &mut self,
        cx: &mut Context<Self>,
    ) -> Task<Result<(Pick, Option<PathBuf>)>> {
        if self.live_client().is_none() {
            self.session = None;
        }
        let Some(session) = self.session.clone() else {
            return Task::ready(Err(anyhow!("No browser open; run browser: open first")));
        };
        self.cancel_pick(cx);
        let (sender, receiver) = oneshot::channel();
        let executor = cx.background_executor().clone();
        self.pick_task = Some(cx.spawn(async move |this, cx| {
            let outcome = cx
                .background_spawn(async move {
                    let pick = run_pick(&session, executor.timer(PICK_TIMEOUT)).await?;
                    let screenshot_path = save_screenshot(&pick.screenshot_png).await?;
                    anyhow::Ok((pick, screenshot_path))
                })
                .await;
            if let Ok((pick, screenshot_path)) = &outcome {
                this.update(cx, |this, cx| {
                    this.set_last_pick(pick.clone(), screenshot_path.clone());
                    cx.notify();
                })
                .ok();
            }
            sender.send(outcome).ok();
        }));
        cx.spawn(async move |this, cx| {
            let outcome = receiver
                .await
                .unwrap_or_else(|_| Err(anyhow!("pick cancelled")));
            // The pick task has finished; dropping it here (not from inside
            // itself) is safe.
            this.update(cx, |this, cx| {
                this.pick_task = None;
                cx.notify();
            })
            .ok();
            outcome
        })
    }

    /// Drops the running pick and takes the page out of inspect mode, since
    /// the dropped future can no longer do that itself.
    pub fn cancel_pick(&mut self, cx: &mut Context<Self>) -> bool {
        if self.pick_task.take().is_none() {
            return false;
        }
        if let Some(session) = self.session.clone() {
            cx.background_spawn(async move { leave_inspect_mode(&session).await })
                .detach();
        }
        cx.notify();
        true
    }
}

/// The result goes on Chrome's command line, so anything that could be read
/// as a switch is refused and only a small set of schemes is accepted.
pub fn normalize_url(input: &str) -> Result<String> {
    hulder_url::normalize_url(input, hulder_url::BROWSER_SCHEMES)
}

pub fn default_binary() -> Result<PathBuf> {
    [CHROME_BINARY, EDGE_BINARY]
        .iter()
        .map(PathBuf::from)
        .find(|path| path.exists())
        .context("no Google Chrome or Microsoft Edge found in /Applications; set browser.binary")
}

/// `DevToolsActivePort` holds the port on line one and the browser endpoint's
/// WebSocket path on line two.
pub fn parse_devtools_active_port(text: &str) -> Result<String> {
    let mut lines = text.lines();
    let port: u16 = lines
        .next()
        .context("DevToolsActivePort is empty")?
        .trim()
        .parse()
        .context("DevToolsActivePort: first line is not a port")?;
    let path = lines
        .next()
        .map(str::trim)
        .filter(|path| path.starts_with('/'))
        .context("DevToolsActivePort: missing browser websocket path")?;
    Ok(format!("ws://127.0.0.1:{port}{path}"))
}

/// Any local process can reach the debug port, so the profile holds as little
/// as possible: a dedicated directory, no extensions, no sync, no keychain.
pub fn launch_args(user_data_dir: &Path, url: &str) -> Vec<String> {
    vec![
        "--remote-debugging-port=0".to_string(),
        format!("--user-data-dir={}", user_data_dir.display()),
        "--no-first-run".to_string(),
        "--no-default-browser-check".to_string(),
        "--disable-extensions".to_string(),
        "--disable-sync".to_string(),
        "--no-service-autorun".to_string(),
        "--password-store=basic".to_string(),
        "--new-window".to_string(),
        // Chrome treats `--` as the switch terminator, so the URL can never be read as a flag.
        "--".to_string(),
        url.to_string(),
    ]
}

/// The profile directory must be ours alone: created 0700, never a symlink
/// (which could redirect the profile elsewhere), and owned by our uid.
fn prepare_user_data_dir(user_data_dir: &Path) -> Result<()> {
    use std::os::unix::fs::{DirBuilderExt, MetadataExt, PermissionsExt};

    let mut builder = std::fs::DirBuilder::new();
    builder.recursive(true).mode(0o700);
    builder
        .create(user_data_dir)
        .with_context(|| format!("creating {}", user_data_dir.display()))?;
    let metadata = std::fs::symlink_metadata(user_data_dir)
        .with_context(|| format!("inspecting {}", user_data_dir.display()))?;
    anyhow::ensure!(
        !metadata.file_type().is_symlink(),
        "{} is a symlink; refusing to use it as the browser profile",
        user_data_dir.display()
    );
    anyhow::ensure!(
        metadata.is_dir(),
        "{} is not a directory",
        user_data_dir.display()
    );
    // SAFETY: getuid has no preconditions and cannot fail.
    let current_uid = unsafe { libc::getuid() };
    anyhow::ensure!(
        metadata.uid() == current_uid,
        "{} is owned by uid {}, not {current_uid}; refusing to use it as the browser profile",
        user_data_dir.display(),
        metadata.uid()
    );
    if metadata.permissions().mode() & 0o077 != 0 {
        std::fs::set_permissions(user_data_dir, std::fs::Permissions::from_mode(0o700))
            .with_context(|| format!("restricting {}", user_data_dir.display()))?;
    }
    Ok(())
}

async fn launch(
    binary: PathBuf,
    user_data_dir: PathBuf,
    url: String,
    executor: &BackgroundExecutor,
) -> Result<(Child, String)> {
    prepare_user_data_dir(&user_data_dir)?;
    let port_file = user_data_dir.join("DevToolsActivePort");
    // A stale file from a previous run would point at a dead port.
    if let Err(error) = smol::fs::remove_file(&port_file).await
        && error.kind() != io::ErrorKind::NotFound
    {
        return Err(error).with_context(|| format!("removing {}", port_file.display()));
    }
    let mut child = util::command::new_command(&binary)
        .args(launch_args(&user_data_dir, &url))
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .kill_on_drop(true)
        .spawn()
        .with_context(|| format!("launching {}", binary.display()))?;

    let deadline = std::time::Instant::now() + PORT_FILE_TIMEOUT;
    while std::time::Instant::now() < deadline {
        if let Some(status) = child.try_status()? {
            // The singleton handed our URL to an instance that appeared between
            // the live check and the spawn; its port file is now the live one.
            if live_port_file(&port_file, executor.timer(PORT_PROBE_TIMEOUT))
                .await
                .is_some()
            {
                anyhow::bail!(
                    "a Hulder Chrome is already running on {}; run browser: open again to attach to it",
                    user_data_dir.display()
                );
            }
            anyhow::bail!(
                "{} exited before publishing a DevTools port ({status})",
                binary.display()
            );
        }
        if let Ok(text) = smol::fs::read_to_string(&port_file).await
            && let Ok(ws_url) = parse_devtools_active_port(&text)
        {
            return Ok((child, ws_url));
        }
        executor.timer(POLL_INTERVAL).await;
    }
    anyhow::bail!("timed out waiting for {}", port_file.display())
}

/// The browser endpoint named by an existing `DevToolsActivePort`, if its
/// port still accepts a connection; a stale file from a dead Chrome yields
/// `None`.
pub async fn live_port_file(
    port_file: &Path,
    probe_deadline: impl Future<Output = ()>,
) -> Option<String> {
    let text = smol::fs::read_to_string(port_file).await.ok()?;
    let ws_url = parse_devtools_active_port(&text).ok()?;
    let port: u16 = text.lines().next()?.trim().parse().ok()?;
    let probe = async {
        smol::net::TcpStream::connect(("127.0.0.1", port))
            .await
            .is_ok()
    };
    let expire = async {
        probe_deadline.await;
        false
    };
    futures::pin_mut!(probe);
    futures::pin_mut!(expire);
    let alive = match futures::future::select(probe, expire).await {
        futures::future::Either::Left((alive, _)) | futures::future::Either::Right((alive, _)) => {
            alive
        }
    };
    alive.then_some(ws_url)
}

async fn reconnect(
    ws_url: &str,
    url: &str,
    executor: &BackgroundExecutor,
) -> Result<(CdpClient, CdpSession)> {
    let client = CdpClient::connect(ws_url, sleeper(executor)).await?;
    let session = open_in_existing(&client, url).await?;
    Ok((client, session))
}

/// The page target can lag behind the browser endpoint at startup, so poll.
async fn attach_page(
    client: &CdpClient,
    url: &str,
    executor: &BackgroundExecutor,
) -> Result<CdpSession> {
    let deadline = std::time::Instant::now() + Duration::from_secs(5);
    loop {
        let targets = client.get_targets().await?;
        let pages = targets.iter().filter(|target| target.kind == "page");
        let chosen = pages
            .clone()
            .find(|target| same_page(&target.url, url))
            .or_else(|| pages.clone().next());
        if let Some(target) = chosen {
            return client.attach_to_target(&target.target_id).await;
        }
        if std::time::Instant::now() >= deadline {
            anyhow::bail!("Chrome published no page target for {url}");
        }
        executor.timer(POLL_INTERVAL).await;
    }
}

async fn open_in_existing(client: &CdpClient, url: &str) -> Result<CdpSession> {
    let created = client
        .call("Target.createTarget", serde_json::json!({ "url": url }))
        .await?;
    let target_id = created["targetId"]
        .as_str()
        .context("Target.createTarget: no targetId")?;
    client.attach_to_target(target_id).await
}

fn same_page(target_url: &str, requested: &str) -> bool {
    target_url.trim_end_matches('/') == requested.trim_end_matches('/')
}

pub fn clip_from_quad(quad: &[f64]) -> Option<(f64, f64, f64, f64)> {
    if quad.len() < 8 {
        return None;
    }
    let xs = quad.iter().step_by(2);
    let ys = quad.iter().skip(1).step_by(2);
    let min_x = xs.clone().cloned().fold(f64::INFINITY, f64::min);
    let max_x = xs.cloned().fold(f64::NEG_INFINITY, f64::max);
    let min_y = ys.clone().cloned().fold(f64::INFINITY, f64::min);
    let max_y = ys.cloned().fold(f64::NEG_INFINITY, f64::max);
    Some((min_x, min_y, max_x - min_x, max_y - min_y))
}

/// What a browser-level event means for a pick on `session_id`. Detach
/// events arrive without a `sessionId` of their own, which is why the pick
/// listens on the whole connection rather than the session's filtered stream.
pub fn pick_event_outcome(event: &Event, session_id: &str) -> Option<Result<u64>> {
    let ours = event.session_id.as_deref() == Some(session_id);
    match event.method.as_str() {
        "Overlay.inspectNodeRequested" if ours => Some(
            event.params["backendNodeId"]
                .as_u64()
                .context("Overlay.inspectNodeRequested without backendNodeId"),
        ),
        "Inspector.detached" if ours => Some(Err(anyhow!("the page detached while picking"))),
        "Target.detachedFromTarget" if event.params["sessionId"].as_str() == Some(session_id) => {
            Some(Err(anyhow!("the page closed while picking")))
        }
        _ => None,
    }
}

/// Waits for the click, giving up when the page detaches, the connection
/// closes, or `timeout` passes.
async fn wait_for_pick(
    events: &mut mpsc::UnboundedReceiver<Event>,
    session_id: &str,
    deadline: impl Future<Output = ()>,
) -> Result<u64> {
    let wait = async {
        loop {
            let event = events
                .next()
                .await
                .context("browser connection closed while picking")?;
            if let Some(outcome) = pick_event_outcome(&event, session_id) {
                return outcome;
            }
        }
    };
    let expire = async {
        deadline.await;
        Err(anyhow!("no element picked before the pick timed out"))
    };
    futures::pin_mut!(wait);
    futures::pin_mut!(expire);
    match futures::future::select(wait, expire).await {
        futures::future::Either::Left((outcome, _))
        | futures::future::Either::Right((outcome, _)) => outcome,
    }
}

async fn leave_inspect_mode(session: &CdpSession) -> Result<()> {
    session
        .call(
            "Overlay.setInspectMode",
            serde_json::json!({ "mode": "none" }),
        )
        .await
        .context("leaving inspect mode")?;
    Ok(())
}

async fn run_pick(session: &CdpSession, deadline: impl Future<Output = ()>) -> Result<Pick> {
    session
        .call("Overlay.enable", serde_json::json!({}))
        .await?;
    session.call("DOM.enable", serde_json::json!({})).await?;
    session.call("CSS.enable", serde_json::json!({})).await?;
    let (url, mut root) = fetch_document(session).await?;

    // Subscribe before arming inspect mode so the click cannot be missed.
    let mut events = session.client().events();
    session
        .call(
            "Overlay.setInspectMode",
            serde_json::json!({
                "mode": "searchForNode",
                "highlightConfig": {
                    "showInfo": true,
                    "contentColor": { "r": 111, "g": 168, "b": 220, "a": 0.66 }
                }
            }),
        )
        .await?;
    let waited = wait_for_pick(&mut events, session.session_id(), deadline).await;
    // Leave inspect mode on every exit path; a detached page just fails here.
    leave_inspect_mode(session).await.log_err();
    let backend_node_id = waited?;

    let pushed = session
        .call(
            "DOM.pushNodesByBackendIdsToFrontend",
            serde_json::json!({ "backendNodeIds": [backend_node_id] }),
        )
        .await?;
    let node_id = pushed["nodeIds"][0]
        .as_u64()
        .context("DOM.pushNodesByBackendIdsToFrontend returned no nodeId")?;
    let outer_html = session
        .call("DOM.getOuterHTML", serde_json::json!({ "nodeId": node_id }))
        .await?["outerHTML"]
        .as_str()
        .unwrap_or_default()
        .to_string();
    let described = session
        .call("DOM.describeNode", serde_json::json!({ "nodeId": node_id }))
        .await?;

    // The page may have changed since the snapshot; refetch once before
    // falling back to the bare tag name.
    let selector = match selector_for(&root, backend_node_id) {
        Some(selector) => selector,
        None => {
            root = fetch_document(session).await?.1;
            selector_for(&root, backend_node_id).unwrap_or_else(|| {
                described["node"]["localName"]
                    .as_str()
                    .unwrap_or("*")
                    .to_string()
            })
        }
    };

    let computed = session
        .call(
            "CSS.getComputedStyleForNode",
            serde_json::json!({ "nodeId": node_id }),
        )
        .await?["computedStyle"]
        .as_array()
        .map(|entries| {
            entries
                .iter()
                .filter_map(|entry| {
                    Some((
                        entry["name"].as_str()?.to_string(),
                        entry["value"].as_str()?.to_string(),
                    ))
                })
                .collect()
        })
        .unwrap_or_default();

    let screenshot_png = capture_element(session, node_id)
        .await
        .context("capturing element screenshot")
        .log_err()
        .unwrap_or_default();

    Ok(Pick {
        url,
        selector,
        outer_html,
        computed,
        screenshot_png,
    })
}

async fn fetch_document(session: &CdpSession) -> Result<(String, DomNode)> {
    let document = session
        .call("DOM.getDocument", serde_json::json!({ "depth": -1 }))
        .await?;
    let url = document["root"]["documentURL"]
        .as_str()
        .unwrap_or_default()
        .to_string();
    let root: DomNode = serde_json::from_value(document["root"].clone())
        .context("DOM.getDocument: unexpected shape")?;
    Ok((url, root))
}

async fn capture_element(session: &CdpSession, node_id: u64) -> Result<Vec<u8>> {
    let box_model = session
        .call("DOM.getBoxModel", serde_json::json!({ "nodeId": node_id }))
        .await?;
    let quad: Vec<f64> = serde_json::from_value(box_model["model"]["border"].clone())
        .context("DOM.getBoxModel: unexpected border quad")?;
    let (x, y, width, height) =
        clip_from_quad(&quad).context("DOM.getBoxModel: border quad is not a quad")?;
    anyhow::ensure!(width > 0.0 && height > 0.0, "element has no visible box");
    // Box-model quads are viewport-relative; the clip wants page coordinates.
    let metrics = session
        .call("Page.getLayoutMetrics", serde_json::json!({}))
        .await?;
    let page_x = metrics["cssVisualViewport"]["pageX"]
        .as_f64()
        .unwrap_or(0.0);
    let page_y = metrics["cssVisualViewport"]["pageY"]
        .as_f64()
        .unwrap_or(0.0);
    let screenshot = session
        .call(
            "Page.captureScreenshot",
            serde_json::json!({
                "format": "png",
                "captureBeyondViewport": true,
                "clip": { "x": x + page_x, "y": y + page_y, "width": width, "height": height, "scale": 1 }
            }),
        )
        .await?;
    let data = screenshot["data"]
        .as_str()
        .context("Page.captureScreenshot returned no data")?;
    let png = base64::engine::general_purpose::STANDARD
        .decode(data)
        .context("Page.captureScreenshot: data is not base64")?;
    Ok(png)
}

async fn save_screenshot(png: &[u8]) -> Result<Option<PathBuf>> {
    if png.is_empty() {
        return Ok(None);
    }
    let directory = paths::data_dir().join("picks");
    smol::fs::create_dir_all(&directory)
        .await
        .with_context(|| format!("creating {}", directory.display()))?;
    let unix_ms = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|elapsed| elapsed.as_millis())
        .unwrap_or_default();
    let path = directory.join(format!("{unix_ms}.png"));
    smol::fs::write(&path, png)
        .await
        .with_context(|| format!("writing {}", path.display()))?;
    Ok(Some(path))
}

#[cfg(test)]
#[allow(
    clippy::disallowed_methods,
    reason = "these tests probe real sockets, so deadlines need a wall clock, not the test scheduler's"
)]
mod tests {
    use super::*;

    fn wall(duration: Duration) -> impl Future<Output = ()> {
        async move {
            smol::Timer::after(duration).await;
        }
    }

    #[test]
    fn a_pick_remembers_its_clipping_until_the_next_pick() {
        let pick = Pick {
            url: "https://example.com/".into(),
            selector: "html > body".into(),
            outer_html: "<body></body>".into(),
            computed: vec![],
            screenshot_png: vec![],
        };
        let mut runner = ChromeRunner::default();
        runner.remember_pick_clipping(PathBuf::from("/repo/.hulder/clippings/x.md"));
        assert_eq!(runner.last_pick_clipping(), None, "no pick, nothing to remember");
        runner.set_last_pick(pick.clone(), None);
        assert_eq!(runner.last_pick_clipping(), None);
        runner.remember_pick_clipping(PathBuf::from("/repo/.hulder/clippings/2026-10-13-body.md"));
        assert_eq!(
            runner.last_pick_clipping(),
            Some(Path::new("/repo/.hulder/clippings/2026-10-13-body.md"))
        );
        runner.set_last_pick(pick, None);
        assert_eq!(runner.last_pick_clipping(), None, "a new pick starts over");
    }

    #[test]
    fn parses_devtools_active_port_into_a_loopback_url() {
        let url = parse_devtools_active_port("54321\n/devtools/browser/abc-123\n").expect("parsed");
        assert_eq!(url, "ws://127.0.0.1:54321/devtools/browser/abc-123");
        assert!(parse_devtools_active_port("").is_err());
        assert!(parse_devtools_active_port("notaport\n/devtools/browser/x").is_err());
        assert!(parse_devtools_active_port("1234\n").is_err());
    }

    #[test]
    fn live_port_file_requires_a_listening_port() {
        smol::block_on(async {
            let temp = tempfile::tempdir().expect("tempdir");
            let port_file = temp.path().join("DevToolsActivePort");
            assert_eq!(
                live_port_file(&port_file, wall(Duration::from_millis(500))).await,
                None,
                "missing file"
            );

            let listener = smol::net::TcpListener::bind("127.0.0.1:0")
                .await
                .expect("bind");
            let port = listener.local_addr().expect("addr").port();
            std::fs::write(&port_file, format!("{port}\n/devtools/browser/abc\n")).expect("write");
            assert_eq!(
                live_port_file(&port_file, wall(Duration::from_millis(500)))
                    .await
                    .as_deref(),
                Some(format!("ws://127.0.0.1:{port}/devtools/browser/abc").as_str())
            );

            drop(listener);
            assert_eq!(
                live_port_file(&port_file, wall(Duration::from_millis(500))).await,
                None,
                "dead port"
            );

            std::fs::write(&port_file, "garbage").expect("write");
            assert_eq!(
                live_port_file(&port_file, wall(Duration::from_millis(500))).await,
                None,
                "unparseable"
            );
        });
    }

    #[test]
    fn launch_args_isolate_the_debug_profile_and_terminate_switches() {
        let args = launch_args(Path::new("/tmp/hulder/chrome"), "https://example.com/");
        for flag in [
            "--remote-debugging-port=0",
            "--user-data-dir=/tmp/hulder/chrome",
            "--no-first-run",
            "--no-default-browser-check",
            "--disable-extensions",
            "--disable-sync",
            "--no-service-autorun",
            "--password-store=basic",
            "--new-window",
        ] {
            assert!(
                args.iter().any(|arg| arg == flag),
                "missing {flag} in {args:?}"
            );
        }
        let terminator = args
            .iter()
            .position(|arg| arg == "--")
            .expect("switch terminator");
        assert_eq!(
            args.get(terminator + 1).map(String::as_str),
            Some("https://example.com/")
        );
        assert_eq!(terminator + 2, args.len(), "URL must be the last argument");
    }

    #[test]
    fn user_data_dir_is_private_and_never_a_symlink() {
        use std::os::unix::fs::PermissionsExt;

        let temp = tempfile::tempdir().expect("tempdir");
        let profile = temp.path().join("chrome");
        prepare_user_data_dir(&profile).expect("fresh profile");
        let mode = std::fs::metadata(&profile)
            .expect("metadata")
            .permissions()
            .mode()
            & 0o777;
        assert_eq!(mode, 0o700);

        std::fs::set_permissions(&profile, std::fs::Permissions::from_mode(0o755)).expect("loosen");
        prepare_user_data_dir(&profile).expect("existing profile");
        let mode = std::fs::metadata(&profile)
            .expect("metadata")
            .permissions()
            .mode()
            & 0o777;
        assert_eq!(mode, 0o700, "existing profile is tightened back to 0700");

        let elsewhere = temp.path().join("elsewhere");
        std::fs::create_dir(&elsewhere).expect("target");
        let link = temp.path().join("link");
        std::os::unix::fs::symlink(&elsewhere, &link).expect("symlink");
        let error = prepare_user_data_dir(&link).expect_err("symlink rejected");
        assert!(format!("{error:#}").contains("symlink"), "{error:#}");
    }

    fn event(method: &str, params: serde_json::Value, session_id: Option<&str>) -> Event {
        Event {
            method: method.to_string(),
            params,
            session_id: session_id.map(str::to_string),
        }
    }

    #[test]
    fn pick_events_resolve_to_a_node_or_a_detach() {
        let node = event(
            "Overlay.inspectNodeRequested",
            serde_json::json!({ "backendNodeId": 7 }),
            Some("S1"),
        );
        assert_eq!(
            pick_event_outcome(&node, "S1").map(|outcome| outcome.expect("node")),
            Some(7)
        );
        assert!(
            pick_event_outcome(&node, "S2").is_none(),
            "other session's click"
        );

        let detached = event(
            "Target.detachedFromTarget",
            serde_json::json!({ "sessionId": "S1" }),
            None,
        );
        assert!(
            pick_event_outcome(&detached, "S1")
                .expect("outcome")
                .is_err()
        );
        assert!(pick_event_outcome(&detached, "S2").is_none());

        let inspector = event("Inspector.detached", serde_json::json!({}), Some("S1"));
        assert!(
            pick_event_outcome(&inspector, "S1")
                .expect("outcome")
                .is_err()
        );

        let unrelated = event("Page.loadEventFired", serde_json::json!({}), Some("S1"));
        assert!(pick_event_outcome(&unrelated, "S1").is_none());
    }

    #[test]
    fn wait_for_pick_times_out_and_notices_a_closed_stream() {
        smol::block_on(async {
            let (sender, mut receiver) = mpsc::unbounded::<Event>();
            let error = wait_for_pick(&mut receiver, "S1", wall(Duration::from_millis(50)))
                .await
                .expect_err("timeout");
            assert!(format!("{error:#}").contains("timed out"), "{error:#}");

            sender
                .unbounded_send(event(
                    "Overlay.inspectNodeRequested",
                    serde_json::json!({ "backendNodeId": 3 }),
                    Some("S1"),
                ))
                .expect("send");
            let picked = wait_for_pick(&mut receiver, "S1", wall(Duration::from_secs(5)))
                .await
                .expect("picked");
            assert_eq!(picked, 3);

            drop(sender);
            let error = wait_for_pick(&mut receiver, "S1", wall(Duration::from_secs(5)))
                .await
                .expect_err("closed");
            assert!(format!("{error:#}").contains("closed"), "{error:#}");
        });
    }

    #[test]
    fn box_model_quad_becomes_a_clip() {
        let quad = [10.0, 20.0, 110.0, 20.0, 110.0, 70.0, 10.0, 70.0];
        let clip = clip_from_quad(&quad).expect("clip");
        assert_eq!(clip, (10.0, 20.0, 100.0, 50.0));
        assert!(clip_from_quad(&[1.0, 2.0]).is_none());
    }

    #[test]
    fn normalizes_bare_hosts_to_https() {
        assert_eq!(
            normalize_url(" example.com ").expect("valid"),
            "https://example.com/"
        );
        assert_eq!(
            normalize_url("http://localhost:3000/x").expect("valid"),
            "http://localhost:3000/x"
        );
        assert_eq!(normalize_url("about:blank").expect("valid"), "about:blank");
        assert!(same_page("https://example.com/", "https://example.com"));
    }

    #[test]
    fn rejects_switch_like_and_unsupported_urls() {
        for input in hulder_url::HOSTILE_INPUTS {
            assert!(normalize_url(input).is_err(), "{input:?} accepted");
        }
        assert_eq!(
            normalize_url("file:///tmp/index.html").expect("valid"),
            "file:///tmp/index.html"
        );
    }
}
