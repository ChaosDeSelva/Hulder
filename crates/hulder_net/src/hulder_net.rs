pub mod hollow_back;

use futures::future::BoxFuture;
use gpui::{App, AppContext as _, Entity, Global};
use http_client::{AsyncBody, HttpClient, Request, Response, Url, http::HeaderValue};
use parking_lot::Mutex;
use std::collections::VecDeque;
use std::sync::Arc;
use std::time::{Duration, SystemTime};

pub const MAX_ENTRIES: usize = 1000;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum NetEntryKind {
    Refused,
    Browsed,
    /// A Bifröst socket connection or tool call from a terminal agent.
    Bridge,
    /// A request the user sent on purpose from the Raven API client.
    Sent,
}

#[derive(Debug, Clone)]
pub struct NetEntry {
    pub at: SystemTime,
    pub kind: NetEntryKind,
    pub method: String,
    pub url: String,
}

/// Entries recorded off the main thread, waiting to be drained into the `NetLog` entity.
#[derive(Default, Clone)]
pub struct SharedPending(Arc<Mutex<Vec<NetEntry>>>);

impl SharedPending {
    pub fn push(&self, entry: NetEntry) {
        self.0.lock().push(entry);
    }

    pub fn take(&self) -> Vec<NetEntry> {
        std::mem::take(&mut *self.0.lock())
    }
}

#[derive(Default)]
pub struct NetLog {
    entries: VecDeque<NetEntry>,
    pub refused: u64,
    pub browsed: u64,
    pub bridged: u64,
    pub sent: u64,
}

/// The modal column for a Raven send: host, status and byte counts, never a
/// header, body or query string.
pub fn sent_detail(
    host: &str,
    status: Option<u16>,
    request_bytes: u64,
    response_bytes: u64,
) -> String {
    let status = status.map_or_else(|| "failed".to_string(), |status| status.to_string());
    format!("{host}  {status}  {request_bytes} B up / {response_bytes} B down")
}

/// The modal column for a page the Panda loaded, naming the Hoard profile
/// whose cookies travelled with it (never the cookies themselves).
pub fn browsed_detail(url: &str, hoard_profile: Option<&str>) -> String {
    match hoard_profile {
        Some(profile) => format!("{url} (with Hoard profile {profile})"),
        None => url.to_string(),
    }
}

impl NetLog {
    pub fn push(&mut self, entry: NetEntry) {
        match entry.kind {
            NetEntryKind::Refused => self.refused += 1,
            NetEntryKind::Browsed => self.browsed += 1,
            NetEntryKind::Bridge => self.bridged += 1,
            NetEntryKind::Sent => self.sent += 1,
        }
        self.entries.push_front(entry);
        self.entries.truncate(MAX_ENTRIES);
    }

    /// Newest first.
    pub fn entries(&self) -> impl Iterator<Item = &NetEntry> {
        self.entries.iter()
    }

    pub fn global(cx: &App) -> Entity<NetLog> {
        cx.global::<GlobalNetLog>().0.clone()
    }

    pub fn try_global(cx: &App) -> Option<Entity<NetLog>> {
        cx.try_global::<GlobalNetLog>().map(|global| global.0.clone())
    }

    pub fn record_browsed(cx: &mut App, url: String) {
        Self::record_browsed_with_profile(cx, url, None);
    }

    /// Like [`Self::record_browsed`], noting which Hoard profile's cookies
    /// went along with the page load.
    pub fn record_browsed_with_profile(cx: &mut App, url: String, hoard_profile: Option<&str>) {
        let Some(log) = Self::try_global(cx) else {
            return;
        };
        log.update(cx, |log, cx| {
            log.push(NetEntry {
                at: SystemTime::now(),
                kind: NetEntryKind::Browsed,
                method: "GET".into(),
                url: browsed_detail(&url, hoard_profile),
            });
            cx.notify();
        });
    }

    /// Records one Raven send. `status` is `None` when no response came back.
    pub fn record_sent(
        cx: &mut App,
        method: &str,
        host: &str,
        status: Option<u16>,
        request_bytes: u64,
        response_bytes: u64,
    ) {
        let Some(log) = Self::try_global(cx) else {
            return;
        };
        log.update(cx, |log, cx| {
            log.push(NetEntry {
                at: SystemTime::now(),
                kind: NetEntryKind::Sent,
                method: method.into(),
                url: sent_detail(host, status, request_bytes, response_bytes),
            });
            cx.notify();
        });
    }

    /// Records a Bifröst event. `method` is "accept" for a new connection or
    /// "call" for a tool call; `detail` is what the modal shows after the method.
    pub fn record_bridge(cx: &mut App, method: &str, detail: String) {
        let Some(log) = Self::try_global(cx) else {
            return;
        };
        log.update(cx, |log, cx| {
            log.push(NetEntry {
                at: SystemTime::now(),
                kind: NetEntryKind::Bridge,
                method: method.into(),
                url: format!("bridge: {detail}"),
            });
            cx.notify();
        });
    }
}

struct GlobalNetLog(Entity<NetLog>);

impl Global for GlobalNetLog {}

/// Refuses every request like `http_client::BlockedHttpClient`, but remembers what was asked.
pub struct RecordingBlockedHttpClient {
    pending: SharedPending,
}

impl RecordingBlockedHttpClient {
    pub fn new(pending: SharedPending) -> Self {
        Self { pending }
    }
}

impl HttpClient for RecordingBlockedHttpClient {
    fn send(
        &self,
        request: Request<AsyncBody>,
    ) -> BoxFuture<'static, anyhow::Result<Response<AsyncBody>>> {
        self.pending.push(NetEntry {
            at: SystemTime::now(),
            kind: NetEntryKind::Refused,
            method: request.method().to_string(),
            url: request.uri().to_string(),
        });
        Box::pin(async {
            Err(std::io::Error::new(
                std::io::ErrorKind::PermissionDenied,
                "Hulder disallowed request",
            )
            .into())
        })
    }

    fn user_agent(&self) -> Option<&HeaderValue> {
        None
    }

    fn proxy(&self) -> Option<&Url> {
        None
    }
}

/// Registers the `NetLog` global and returns the client to install with `cx.set_http_client`.
pub fn init(cx: &mut App) -> Arc<RecordingBlockedHttpClient> {
    let pending = SharedPending::default();
    let log = cx.new(|_| NetLog::default());
    cx.set_global(GlobalNetLog(log.clone()));
    cx.spawn({
        let pending = pending.clone();
        async move |cx| {
            loop {
                cx.background_executor()
                    .timer(Duration::from_millis(250))
                    .await;
                let batch = pending.take();
                if batch.is_empty() {
                    continue;
                }
                log.update(cx, |log, cx| {
                    for entry in batch {
                        log.push(entry);
                    }
                    cx.notify();
                });
            }
        }
    })
    .detach();
    Arc::new(RecordingBlockedHttpClient::new(pending))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn send_is_refused_and_recorded() {
        let shared = SharedPending::default();
        let client = RecordingBlockedHttpClient::new(shared.clone());
        let request = Request::builder()
            .method("GET")
            .uri("https://example.com/x")
            .body(AsyncBody::empty())
            .expect("request");
        let result = smol::block_on(client.send(request));
        let error = result.err().expect("must be refused");
        assert!(error.to_string().contains("disallowed"));
        let pending = shared.take();
        assert_eq!(pending.len(), 1);
        assert_eq!(pending[0].url, "https://example.com/x");
        assert_eq!(pending[0].method, "GET");
        assert_eq!(pending[0].kind, NetEntryKind::Refused);
        assert!(shared.take().is_empty());
    }

    #[test]
    fn log_is_bounded() {
        let mut log = NetLog::default();
        for index in 0..(MAX_ENTRIES + 50) {
            log.push(NetEntry {
                at: SystemTime::UNIX_EPOCH,
                kind: NetEntryKind::Refused,
                method: "GET".into(),
                url: format!("u{index}"),
            });
        }
        assert_eq!(log.entries().count(), MAX_ENTRIES);
        assert_eq!(log.refused, (MAX_ENTRIES + 50) as u64);
        assert_eq!(log.browsed, 0);
        assert_eq!(log.bridged, 0);
        log.push(NetEntry {
            at: SystemTime::UNIX_EPOCH,
            kind: NetEntryKind::Bridge,
            method: "call".into(),
            url: "bridge: project_root".into(),
        });
        assert_eq!(log.bridged, 1);
        assert_eq!(log.refused, (MAX_ENTRIES + 50) as u64);
        assert_eq!(log.entries().count(), MAX_ENTRIES);
        let mut newest = log.entries().map(|entry| entry.url.as_str());
        assert_eq!(newest.next(), Some("bridge: project_root"));
        assert_eq!(newest.next(), Some("u1049"));
    }

    #[test]
    fn sent_entries_are_counted_separately() {
        let mut log = NetLog::default();
        log.push(NetEntry {
            at: SystemTime::UNIX_EPOCH,
            kind: NetEntryKind::Sent,
            method: "POST".into(),
            url: sent_detail("api.test", Some(201), 12, 345),
        });
        assert_eq!(log.sent, 1);
        assert_eq!(log.refused, 0);
        let entry = log.entries().next().expect("entry");
        assert_eq!(entry.url, "api.test  201  12 B up / 345 B down");
        assert_eq!(
            sent_detail("api.test", None, 0, 0),
            "api.test  failed  0 B up / 0 B down"
        );
        assert_eq!(
            browsed_detail("https://a.test/", Some("work")),
            "https://a.test/ (with Hoard profile work)"
        );
        assert_eq!(browsed_detail("https://a.test/", None), "https://a.test/");
    }
}
