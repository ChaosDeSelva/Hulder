//! A Panda fetch with a Hoard `cookies` profile. Cookies never go on the
//! command line (argv is visible to every local process) and never as a raw
//! `Cookie` header (which is not bound to an origin, so redirects and
//! subresources would leak it). The routes, in order: a cookie file in a
//! private 0700 directory when the installed Lightpanda takes one *and* can
//! be told not to follow redirects (probed once per binary mtime from
//! `fetch --help`); else a short-lived `lightpanda serve` on loopback that
//! takes the jar over CDP, where the browser enforces domain, path and
//! `Secure`; else the fetch is refused.

use crate::fetch::{Dump, FETCH_TIMEOUT, MAX_OUTPUT_BYTES, fetch_args, fetch_with_args};
use anyhow::{Context as _, Result, anyhow};
use cdp::CdpClient;
use futures::StreamExt as _;
use gpui::BackgroundExecutor;
use hoard::Cookie;
use parking_lot::Mutex;
use std::collections::HashMap;
use std::future::Future;
use std::path::{Path, PathBuf};
use std::sync::LazyLock;
use std::time::SystemTime;

pub const CANNOT_TAKE_COOKIES: &str = "this Lightpanda cannot take cookies privately";

#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct FetchCapabilities {
    /// The flag that takes a Netscape cookie file, if any.
    pub cookie_file: Option<String>,
    /// The flag that stops `fetch` from following redirects, if any.
    pub no_redirect: Option<String>,
}

impl FetchCapabilities {
    /// A cookie file is only private when the fetch cannot carry the jar
    /// across a cross-origin redirect.
    pub fn private_cookie_file(&self) -> Option<(&str, &str)> {
        Some((self.cookie_file.as_deref()?, self.no_redirect.as_deref()?))
    }
}

const COOKIE_FILE_FLAGS: &[&str] = &["--cookie-file", "--cookies-file", "--cookie-jar", "--cookies"];
const NO_REDIRECT_FLAGS: &[&str] = &["--no-follow", "--no-redirect", "--no-redirects", "--no-follow-redirects"];

pub fn parse_fetch_help(text: &str) -> FetchCapabilities {
    let has_flag = |flag: &str| {
        text.split(|c: char| c.is_whitespace() || c == ',' || c == '=')
            .any(|token| token == flag)
    };
    FetchCapabilities {
        cookie_file: COOKIE_FILE_FLAGS
            .iter()
            .find(|flag| has_flag(flag))
            .map(|flag| flag.to_string()),
        no_redirect: NO_REDIRECT_FLAGS
            .iter()
            .find(|flag| has_flag(flag))
            .map(|flag| flag.to_string()),
    }
}

type CapabilityCache = HashMap<PathBuf, (SystemTime, FetchCapabilities)>;

static CAPABILITIES: LazyLock<Mutex<CapabilityCache>> = LazyLock::new(Mutex::default);

/// Runs `lightpanda fetch --help` once per binary mtime.
pub async fn capabilities(binary: &Path) -> Result<FetchCapabilities> {
    let mtime = std::fs::metadata(binary)
        .and_then(|metadata| metadata.modified())
        .with_context(|| format!("inspecting {}", binary.display()))?;
    if let Some((cached_mtime, cached)) = CAPABILITIES.lock().get(binary)
        && *cached_mtime == mtime
    {
        return Ok(cached.clone());
    }
    let output = util::command::new_command(binary)
        .args(["fetch", "--help"])
        .env("LIGHTPANDA_DISABLE_TELEMETRY", "true")
        .stdin(util::command::Stdio::null())
        .output()
        .await
        .with_context(|| format!("running {} fetch --help", binary.display()))?;
    let mut text = String::from_utf8_lossy(&output.stdout).into_owned();
    text.push_str(&String::from_utf8_lossy(&output.stderr));
    let parsed = parse_fetch_help(&text);
    CAPABILITIES
        .lock()
        .insert(binary.to_path_buf(), (mtime, parsed.clone()));
    Ok(parsed)
}

/// `fetch_args` with the cookie-file and no-redirect flags before the URL.
pub fn fetch_args_with_cookie_file(
    dump: Dump,
    url: &str,
    cookie_file_flag: &str,
    cookie_file: &Path,
    no_redirect_flag: &str,
) -> Vec<String> {
    let mut args = fetch_args(dump, url);
    let url_index = args.len() - 1;
    args.insert(url_index, no_redirect_flag.to_string());
    args.insert(url_index + 1, cookie_file_flag.to_string());
    args.insert(url_index + 2, cookie_file.to_string_lossy().into_owned());
    args
}

/// Netscape cookie file lines: domain, include-subdomains, path, secure,
/// expires, name, value.
pub fn cookie_file_text(cookies: &[Cookie]) -> String {
    let mut text = String::from("# Netscape HTTP Cookie File\n");
    for cookie in cookies {
        let subdomains = if cookie.domain.starts_with('.') { "TRUE" } else { "FALSE" };
        let secure = if cookie.secure { "TRUE" } else { "FALSE" };
        let expires = cookie.expires.map(|e| e as i64).unwrap_or(0);
        text.push_str(&format!(
            "{}\t{subdomains}\t{}\t{secure}\t{expires}\t{}\t{}\n",
            cookie.domain, cookie.path, cookie.name, cookie.value
        ));
    }
    text
}

/// A 0700 directory holding one 0600 cookie file; both are removed on drop.
pub struct PrivateCookieFile {
    _directory: tempfile::TempDir,
    pub path: PathBuf,
}

impl PrivateCookieFile {
    pub fn write(cookies: &[Cookie]) -> Result<Self> {
        use std::io::Write as _;
        use std::os::unix::fs::{OpenOptionsExt as _, PermissionsExt as _};

        let directory = tempfile::Builder::new()
            .prefix(".hulder-panda-")
            .tempdir()
            .context("creating a private directory for the cookie file")?;
        std::fs::set_permissions(directory.path(), std::fs::Permissions::from_mode(0o700))
            .context("restricting the cookie directory")?;
        let path = directory.path().join("cookies.txt");
        let mut file = std::fs::OpenOptions::new()
            .write(true)
            .create_new(true)
            .mode(0o600)
            .open(&path)
            .context("creating the cookie file")?;
        file.write_all(cookie_file_text(cookies).as_bytes())
            .and_then(|()| file.sync_all())
            .context("writing the cookie file")?;
        Ok(Self {
            _directory: directory,
            path,
        })
    }
}

/// Fetches `url` with `cookies` attached by the first private route the
/// installed Lightpanda supports. With no cookies this is a plain `fetch`.
pub async fn fetch_with_cookies(
    binary: &Path,
    dump: Dump,
    url: &str,
    cookies: &[Cookie],
    executor: &BackgroundExecutor,
) -> Result<String> {
    if cookies.is_empty() {
        return fetch_with_args(binary, fetch_args(dump, url), executor.timer(FETCH_TIMEOUT), MAX_OUTPUT_BYTES).await;
    }
    let capabilities = capabilities(binary).await?;
    let result = if let Some((cookie_flag, no_redirect_flag)) = capabilities.private_cookie_file() {
        let file = PrivateCookieFile::write(cookies)?;
        let args = fetch_args_with_cookie_file(dump, url, cookie_flag, &file.path, no_redirect_flag);
        let result = fetch_with_args(binary, args, executor.timer(FETCH_TIMEOUT), MAX_OUTPUT_BYTES).await;
        // The directory, the file and the cookies in it go away with `file`.
        drop(file);
        result
    } else {
        serve_and_dump(binary, url, cookies, executor)
            .await
            .with_context(|| CANNOT_TAKE_COOKIES.to_string())
    };
    // Neither the page text (which goes into a buffer) nor an error (which
    // goes into a toast and may quote stderr) may carry a cookie value.
    match result {
        Ok(text) => Ok(scrub(&text, cookies)),
        Err(error) => Err(anyhow!("{}", scrub(&format!("{error:#}"), cookies))),
    }
}

/// Replaces every cookie value (and `name=value` pair) in `text`.
pub fn scrub(text: &str, cookies: &[Cookie]) -> String {
    let mut text = text.to_string();
    for cookie in cookies {
        if cookie.value.is_empty() {
            continue;
        }
        text = text
            .replace(&format!("{}={}", cookie.name, cookie.value), &format!("{}=<redacted>", cookie.name))
            .replace(&cookie.value, "<redacted>");
    }
    text
}

fn sleeper(executor: &BackgroundExecutor) -> cdp::Sleep {
    let executor = executor.clone();
    std::sync::Arc::new(move |duration| {
        let timer = executor.timer(duration);
        Box::pin(async move {
            timer.await;
        })
    })
}

/// A random high port that nothing else listens on right now. The bind is
/// the check: a port another process holds fails it and is skipped.
fn free_loopback_port() -> Result<u16> {
    let seed = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|elapsed| elapsed.subsec_nanos())
        .unwrap_or(0)
        ^ std::process::id();
    for attempt in 0..32u32 {
        let port = 20_000 + ((seed.wrapping_mul(2_654_435_761).wrapping_add(attempt * 7919)) % 40_000) as u16;
        if std::net::TcpListener::bind(("127.0.0.1", port)).is_ok() {
            return Ok(port);
        }
    }
    Err(anyhow!("no free loopback port for lightpanda serve"))
}

/// The cookies that belong to `scheme://host` only; the rest of the jar
/// never reaches the loopback CDP port.
pub fn cookies_for_target(jar: &[Cookie], scheme: &str, host: &str) -> Vec<Cookie> {
    let now = hoard::model::now_unix();
    jar.iter()
        .filter(|cookie| cookie.applies(scheme, host, &cookie.path, now))
        .cloned()
        .collect()
}

/// Kills the `lightpanda serve` child on every exit path, including a
/// cancelled future, so a jar handed to it never outlives the fetch.
pub struct ServeGuard {
    child: Option<util::command::Child>,
}

impl ServeGuard {
    pub fn new(child: util::command::Child) -> Self {
        Self { child: Some(child) }
    }
}

impl Drop for ServeGuard {
    fn drop(&mut self) {
        if let Some(mut child) = self.child.take() {
            child.kill().ok();
        }
    }
}

async fn race<T>(
    work: impl Future<Output = Result<T>>,
    deadline: impl Future<Output = ()>,
    what: &str,
) -> Result<T> {
    let expire = async {
        deadline.await;
        Err(anyhow!("{what} timed out"))
    };
    futures::pin_mut!(work);
    futures::pin_mut!(expire);
    match futures::future::select(work, expire).await {
        futures::future::Either::Left((outcome, _)) | futures::future::Either::Right((outcome, _)) => outcome,
    }
}

/// `lightpanda serve` on a loopback port for this one page: the jar goes in
/// over `Network.setCookies` (so the browser binds it to domain, path and
/// `Secure`), then `Page.navigate`, wait for load, dump the document. The
/// child is killed when this returns.
async fn serve_and_dump(
    binary: &Path,
    url: &str,
    cookies: &[Cookie],
    executor: &BackgroundExecutor,
) -> Result<String> {
    use futures::FutureExt as _;
    use smol::io::AsyncBufReadExt as _;

    let (scheme, host) = url::Url::parse(url)
        .ok()
        .and_then(|parsed| Some((parsed.scheme().to_string(), parsed.host_str()?.to_string())))
        .context("the URL has no host")?;
    let cookies = cookies_for_target(cookies, &scheme, &host);
    anyhow::ensure!(!cookies.is_empty(), "no cookie in the profile applies to {host}");
    // One clock for the whole session: start, load and dump together.
    let deadline = executor.timer(FETCH_TIMEOUT).shared();
    let port = free_loopback_port()?;
    let mut child = util::command::new_command(binary)
        .args(crate::runner::PandaRunner::serve_args(port, true))
        .env("LIGHTPANDA_DISABLE_TELEMETRY", "true")
        .stdin(util::command::Stdio::null())
        .stdout(util::command::Stdio::null())
        .stderr(util::command::Stdio::piped())
        .kill_on_drop(true)
        .spawn()
        .with_context(|| format!("running {} serve", binary.display()))?;
    let stderr = child.stderr.take().context("lightpanda: no stderr pipe")?;
    // From here the child dies with `guard`, whatever happens below.
    let guard = ServeGuard::new(child);
    let wait_for_listen = async {
        let mut lines = smol::io::BufReader::new(stderr).lines();
        while let Some(line) = lines.next().await {
            let line = line?;
            if crate::runner::is_listening_line(&line) {
                return Ok(lines);
            }
        }
        Err(anyhow!("lightpanda serve exited before listening"))
    };
    let mut lines = race(wait_for_listen, deadline.clone(), "lightpanda serve start").await?;
    // Keep stderr drained so the child never blocks on a full pipe.
    let drain = executor.spawn(async move { while lines.next().await.is_some() {} });

    let cookies = &cookies;
    let work = async {
        let client = CdpClient::connect(&format!("ws://127.0.0.1:{port}"), sleeper(executor)).await?;
        let created = client
            .call("Target.createTarget", serde_json::json!({ "url": "about:blank" }))
            .await?;
        let target_id = created["targetId"]
            .as_str()
            .context("Target.createTarget: no targetId")?;
        let session = client.attach_to_target(target_id).await?;
        session.call("Network.enable", serde_json::json!({})).await?;
        session
            .call("Network.setCookies", hoard::borrow::set_cookies_params(cookies))
            .await
            .context("Network.setCookies")?;
        session.call("Page.enable", serde_json::json!({})).await?;
        let mut events = session.events();
        session
            .call("Page.navigate", serde_json::json!({ "url": url }))
            .await?;
        while let Some(event) = events.next().await {
            if event.method == "Page.loadEventFired" {
                break;
            }
        }
        let evaluated = session
            .call(
                "Runtime.evaluate",
                serde_json::json!({ "expression": "document.documentElement.outerHTML", "returnByValue": true }),
            )
            .await?;
        let html = evaluated["result"]["value"]
            .as_str()
            .context("Runtime.evaluate returned no document")?
            .to_string();
        anyhow::ensure!(html.len() <= MAX_OUTPUT_BYTES, "lightpanda output exceeded {MAX_OUTPUT_BYTES} bytes");
        // The jar is cleared before the port closes; the guard then kills
        // the process, which is what clears it on every other path.
        session
            .call("Network.clearBrowserCookies", serde_json::json!({}))
            .await
            .context("Network.clearBrowserCookies")?;
        anyhow::Ok(html)
    };
    let outcome = race(work, deadline, "lightpanda page load").await;
    drop(guard);
    drop(drain);
    outcome
}

#[cfg(test)]
#[allow(
    clippy::disallowed_methods,
    reason = "these tests run real child processes and probe them with kill -0"
)]
mod tests {
    use super::*;
    use std::time::Duration;

    #[test]
    fn help_text_reveals_cookie_file_and_no_redirect_flags_and_header_is_never_used() {
        let both = parse_fetch_help("Usage: lightpanda fetch [options] <url>\n  --header <value>  Extra header\n  --cookie-file <path>\n  --no-follow\n");
        assert_eq!(both.cookie_file.as_deref(), Some("--cookie-file"));
        assert_eq!(both.no_redirect.as_deref(), Some("--no-follow"));
        assert_eq!(both.private_cookie_file(), Some(("--cookie-file", "--no-follow")));
        let header_only = parse_fetch_help("--header <value>");
        assert_eq!(header_only, FetchCapabilities::default(), "--header is not a route");
        let file_without_no_redirect = parse_fetch_help("--cookies=<file>");
        assert_eq!(file_without_no_redirect.cookie_file.as_deref(), Some("--cookies"));
        assert_eq!(
            file_without_no_redirect.private_cookie_file(),
            None,
            "a cookie file that may follow redirects is not private"
        );
    }

    #[test]
    fn cookie_file_flags_go_before_the_url_and_argv_never_holds_a_cookie() {
        let args = fetch_args_with_cookie_file(
            Dump::Markdown,
            "https://a.test/x",
            "--cookie-file",
            Path::new("/private/cookies.txt"),
            "--no-follow",
        );
        assert_eq!(args.last().map(String::as_str), Some("https://a.test/x"));
        assert!(args.iter().any(|arg| arg == "--no-follow"));
        let index = args.iter().position(|arg| arg == "--cookie-file").expect("flag");
        assert_eq!(args[index + 1], "/private/cookies.txt");
        assert!(!args.iter().any(|arg| arg.contains("Cookie:") || arg == "--header"));
    }

    #[test]
    fn capabilities_are_cached_by_mtime() {
        let dir = tempfile::tempdir().expect("tmp");
        let script = crate::fetch::write_fake_binary(dir.path(), "#!/bin/sh\necho '--cookie-file <path> --no-follow'\n");
        let first = smol::block_on(capabilities(&script)).expect("probe");
        assert!(first.private_cookie_file().is_some());
        std::fs::write(&script, "#!/bin/sh\necho 'nothing'\n").expect("rewrite");
        let later = SystemTime::now() + Duration::from_secs(5);
        std::fs::File::options()
            .write(true)
            .open(&script)
            .expect("open")
            .set_modified(later)
            .expect("touch");
        let reprobed = smol::block_on(capabilities(&script)).expect("reprobe");
        assert_eq!(reprobed, FetchCapabilities::default(), "a changed binary is probed again");
    }

    fn cookie(name: &str, domain: &str, secure: bool) -> Cookie {
        Cookie {
            name: name.into(),
            value: "v".into(),
            domain: domain.into(),
            path: "/".into(),
            secure,
            http_only: false,
            expires: None,
        }
    }

    #[test]
    fn cookie_values_are_scrubbed_from_page_text_and_errors() {
        let jar = vec![
            Cookie {
                value: "s3cret".into(),
                ..cookie("session", "a.test", false)
            },
            Cookie {
                value: String::new(),
                ..cookie("empty", "a.test", false)
            },
        ];
        assert_eq!(
            scrub("Cookie: session=s3cret; echoed s3cret twice", &jar),
            "Cookie: session=<redacted>; echoed <redacted> twice"
        );
        assert_eq!(scrub("plain page", &jar), "plain page");
    }

    #[test]
    fn only_the_target_hosts_cookies_reach_the_cdp_port() {
        let jar = vec![
            cookie("site", ".a.test", false),
            cookie("exact", "app.a.test", true),
            cookie("other", "b.test", false),
            cookie("secure-elsewhere", ".c.test", true),
        ];
        let names = |cookies: Vec<Cookie>| cookies.into_iter().map(|c| c.name).collect::<Vec<_>>();
        assert_eq!(names(cookies_for_target(&jar, "https", "app.a.test")), vec!["site", "exact"]);
        assert_eq!(names(cookies_for_target(&jar, "http", "app.a.test")), vec!["site"], "secure dropped on http");
        assert_eq!(names(cookies_for_target(&jar, "https", "b.test")), vec!["other"]);
        assert!(cookies_for_target(&jar, "https", "d.test").is_empty());
    }

    #[test]
    fn serve_guard_kills_the_child_when_dropped() {
        let dir = tempfile::tempdir().expect("tmp");
        let pid_file = dir.path().join("pid");
        let script = crate::fetch::write_fake_binary(
            dir.path(),
            &format!("#!/bin/sh\necho $$ > \"{}\"\nsleep 30\n", pid_file.display()),
        );
        let child = util::command::new_command(&script)
            .stdin(util::command::Stdio::null())
            .stdout(util::command::Stdio::null())
            .stderr(util::command::Stdio::null())
            .spawn()
            .expect("spawn");
        let guard = ServeGuard::new(child);
        let mut pid = String::new();
        for _ in 0..40 {
            if let Ok(text) = std::fs::read_to_string(&pid_file) {
                pid = text;
                break;
            }
            std::thread::sleep(Duration::from_millis(50));
        }
        let pid = pid.trim().to_string();
        assert!(!pid.is_empty(), "fake serve never wrote its pid");
        drop(guard);
        let alive = |pid: &str| {
            std::process::Command::new("kill")
                .args(["-0", pid])
                .status()
                .map(|status| status.success())
                .unwrap_or(false)
        };
        let mut still_alive = true;
        for _ in 0..40 {
            still_alive = alive(&pid);
            if !still_alive {
                break;
            }
            std::thread::sleep(Duration::from_millis(50));
        }
        assert!(!still_alive, "child {pid} outlived its guard");
        assert!(free_loopback_port().is_ok_and(|port| port >= 20_000));
    }

    #[test]
    fn private_cookie_file_is_0600_in_a_0700_directory_and_vanishes() {
        use std::os::unix::fs::PermissionsExt as _;
        let cookie = Cookie {
            name: "s".into(),
            value: "1".into(),
            domain: ".a.test".into(),
            path: "/".into(),
            secure: true,
            http_only: false,
            expires: Some(99.0),
        };
        let file = PrivateCookieFile::write(&[cookie]).expect("file");
        let path = file.path.clone();
        let directory = path.parent().expect("dir").to_path_buf();
        assert_eq!(std::fs::metadata(&path).expect("meta").permissions().mode() & 0o777, 0o600);
        assert_eq!(std::fs::metadata(&directory).expect("meta").permissions().mode() & 0o777, 0o700);
        let text = std::fs::read_to_string(&path).expect("read");
        assert!(text.starts_with("# Netscape HTTP Cookie File\n"));
        assert!(text.contains(".a.test\tTRUE\t/\tTRUE\t99\ts\t1\n"));
        drop(file);
        assert!(!path.exists() && !directory.exists(), "cookie file removed");
    }
}
