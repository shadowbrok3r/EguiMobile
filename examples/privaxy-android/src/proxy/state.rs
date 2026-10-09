//! State shared between the proxy's Tokio threads and the egui frame loop.
//!
//! The UI reads this every frame, so nothing here may block for long: counters are atomics and the
//! request log contains bounded metadata behind a short-lived lock; payloads live in files.

use crate::proxy::config::MitmMode;
use chrono::{DateTime, Local};
use std::collections::VecDeque;
use std::sync::atomic::{AtomicBool, AtomicU8, AtomicU64, Ordering};
use std::sync::{Arc, Mutex};

pub use super::storage::Body;
use super::storage::{BodyStore, CAPTURE_BUDGET};

pub const MAX_LOGGED_REQUESTS: usize = 5000;
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Status {
    Stopped,
    Starting,
    Running { address: String },
    Failed(String),
}

impl Status {
    pub fn is_running(&self) -> bool {
        matches!(self, Status::Running { .. })
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum FiltersStatus {
    Idle,
    Updating { completed: usize, total: usize },
    Ready { lists: usize },
    Failed(String),
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum EventKind {
    Blocked { filter: String },
    /// Passed through byte for byte: hostname-only mode, or a never-intercept host.
    Tunneled,
    /// TLS interception was attempted; consult the exchange for its outcome.
    Intercepted,
    Proxied,
}

/// Everything the proxy saw of one exchange, filled in as it happens rather than at the end, so
/// the UI can open a request that is still streaming.
#[derive(Debug, Default, Clone)]
pub struct Exchange {
    pub request_headers: Vec<(String, String)>,
    pub response_headers: Vec<(String, String)>,
    pub request_trailers: Vec<(String, String)>,
    pub response_trailers: Vec<(String, String)>,
    pub request_version: Option<http::Version>,
    pub response_version: Option<http::Version>,
    pub status: Option<u16>,
    pub request_body: Body,
    pub response_body: Body,
    /// When the response completed or failed; absent while still streaming.
    pub finished_at: Option<DateTime<Local>>,
    /// Why there is nothing to inspect, for the entries where there is nothing.
    pub note: Option<String>,
    /// A transport/protocol failure, distinct from an opaque but working tunnel.
    pub error: Option<String>,
}

impl Exchange {
    #[cfg(test)]
    pub(crate) fn for_test() -> Self {
        let store = BodyStore::temporary(CAPTURE_BUDGET);
        Self {
            request_body: store.body(),
            response_body: store.body(),
            ..Self::default()
        }
    }

    pub fn record_request_chunk(&mut self, chunk: &[u8]) {
        self.request_body.push(chunk);
    }

    pub fn record_response_chunk(&mut self, chunk: &[u8]) {
        self.response_body.push(chunk);
    }

    pub fn fail(&mut self, message: impl Into<String>) {
        self.error = Some(message.into());
        self.finished_at = Some(Local::now());
    }

    /// Whether anything beyond the request line was ever visible.
    pub fn is_opaque(&self) -> bool {
        self.request_headers.is_empty() && self.response_headers.is_empty()
    }
}

#[derive(Debug, Clone)]
pub struct RequestEvent {
    pub id: u64,
    pub at: DateTime<Local>,
    pub method: String,
    pub url: String,
    pub kind: EventKind,
    /// Shared with the request handler, which keeps writing into it while the body streams.
    pub exchange: Arc<Mutex<Exchange>>,
}

impl RequestEvent {
    pub fn host(&self) -> &str {
        self.url
            .split_once("://")
            .map(|(_, rest)| rest)
            .unwrap_or(&self.url)
            .split('/')
            .next()
            .unwrap_or(&self.url)
    }

    /// Path and query, or `/` when the URL carries none.
    pub fn path(&self) -> &str {
        let rest = self
            .url
            .split_once("://")
            .map(|(_, rest)| rest)
            .unwrap_or(&self.url);
        match rest.find('/') {
            Some(at) => &rest[at..],
            None => "/",
        }
    }

    /// Registrable-ish domain for grouping: the last two labels, so `a.cdn.example.com` and
    /// `b.cdn.example.com` land together.
    pub fn domain(&self) -> String {
        let host = self.host().split(':').next().unwrap_or_default();
        if host.parse::<std::net::IpAddr>().is_ok() {
            return host.to_owned();
        }
        let labels: Vec<&str> = host.split('.').collect();
        if labels.len() <= 2 {
            return host.to_owned();
        }
        labels[labels.len() - 2..].join(".")
    }

    pub fn note(self, note: impl Into<String>) -> Self {
        if let Ok(mut exchange) = self.exchange.lock() {
            exchange.note = Some(note.into());
        }
        self
    }
}

#[derive(Default)]
pub struct Counters {
    pub proxied: AtomicU64,
    pub blocked: AtomicU64,
    pub tunneled: AtomicU64,
    pub modified: AtomicU64,
}

impl Counters {
    pub fn snapshot(&self) -> CountersSnapshot {
        CountersSnapshot {
            proxied: self.proxied.load(Ordering::Relaxed),
            blocked: self.blocked.load(Ordering::Relaxed),
            tunneled: self.tunneled.load(Ordering::Relaxed),
            modified: self.modified.load(Ordering::Relaxed),
        }
    }
}

#[derive(Debug, Clone, Copy, Default)]
pub struct CountersSnapshot {
    pub proxied: u64,
    pub blocked: u64,
    pub tunneled: u64,
    pub modified: u64,
}

pub struct ProxyState {
    status: Mutex<Status>,
    filters: Mutex<FiltersStatus>,
    events: Mutex<VecDeque<RequestEvent>>,
    next_id: AtomicU64,
    mode: AtomicU8,
    paused: AtomicBool,
    storage: Mutex<BodyStore>,
    pub counters: Counters,
}

impl ProxyState {
    #[cfg(test)]
    pub fn new(mode: MitmMode) -> Self {
        Self::with_store(mode, BodyStore::temporary(CAPTURE_BUDGET))
    }

    pub fn with_storage(mode: MitmMode, root: &std::path::Path) -> std::io::Result<Self> {
        // Removing thousands of old files must not stall the first UI frame. Snapshot only the
        // previous session directories; the cleaner must never race a new capture's creation.
        let old: Vec<_> = std::fs::read_dir(root)
            .into_iter()
            .flatten()
            .flatten()
            .filter(|entry| entry.file_name().to_string_lossy().starts_with("session-"))
            .map(|entry| entry.path())
            .collect();
        let storage = BodyStore::new(root, CAPTURE_BUDGET)?;
        std::thread::spawn(move || {
            for path in old {
                let _ = std::fs::remove_dir_all(path);
            }
        });
        Ok(Self::with_store(mode, storage))
    }

    fn with_store(mode: MitmMode, storage: BodyStore) -> Self {
        Self {
            status: Mutex::new(Status::Stopped),
            filters: Mutex::new(FiltersStatus::Idle),
            events: Mutex::new(VecDeque::with_capacity(MAX_LOGGED_REQUESTS)),
            paused: AtomicBool::new(false),
            storage: Mutex::new(storage),
            next_id: AtomicU64::new(1),
            mode: AtomicU8::new(mode as u8),
            counters: Counters::default(),
        }
    }

    pub fn status(&self) -> Status {
        self.status
            .lock()
            .map(|status| status.clone())
            .unwrap_or(Status::Stopped)
    }

    pub fn set_status(&self, status: Status) {
        if let Ok(mut guard) = self.status.lock() {
            *guard = status;
        }
    }

    pub fn filters_status(&self) -> FiltersStatus {
        self.filters
            .lock()
            .map(|status| status.clone())
            .unwrap_or(FiltersStatus::Idle)
    }

    pub fn set_filters_status(&self, status: FiltersStatus) {
        if let Ok(mut guard) = self.filters.lock() {
            *guard = status;
        }
    }

    pub fn mode(&self) -> MitmMode {
        match self.mode.load(Ordering::Relaxed) {
            0 => MitmMode::HostnameOnly,
            _ => MitmMode::Full,
        }
    }

    pub fn set_mode(&self, mode: MitmMode) {
        self.mode.store(mode as u8, Ordering::Relaxed);
    }

    /// Log an exchange and hand back its [`Exchange`], which the handler keeps writing into as
    /// headers arrive and the body streams.
    /// Whether new exchanges are being added to the log. Traffic still flows when paused; it is
    /// only the log that holds still. Previously recorded bodies remain available.
    pub fn paused(&self) -> bool {
        self.paused.load(Ordering::Relaxed) || self.storage_problem().is_some()
    }

    pub fn set_paused(&self, paused: bool) {
        self.paused.store(paused, Ordering::Relaxed);
    }

    pub fn record(&self, mut event: RequestEvent) -> Arc<Mutex<Exchange>> {
        if self.paused() {
            // Still handed back so the request path writes into something, just never logged —
            // and the counters describe the log, so they hold too.
            return event.exchange.clone();
        }

        let mut events = self.events.lock().unwrap();
        let storage = self.storage.lock().unwrap();
        if storage.problem().is_some() {
            return event.exchange.clone();
        }
        if events.len() == MAX_LOGGED_REQUESTS {
            storage.stop(format!("Capture reached {MAX_LOGGED_REQUESTS} entries. Save the capture, then clear it to record more. Existing requests and bodies are preserved."));
            return event.exchange.clone();
        }
        match &event.kind {
            EventKind::Blocked { .. } => self.counters.blocked.fetch_add(1, Ordering::Relaxed),
            EventKind::Tunneled | EventKind::Intercepted => {
                self.counters.tunneled.fetch_add(1, Ordering::Relaxed)
            }
            EventKind::Proxied => self.counters.proxied.fetch_add(1, Ordering::Relaxed),
        };
        event.id = self.next_id.fetch_add(1, Ordering::Relaxed);
        let exchange = event.exchange.clone();
        if let Ok(mut open) = exchange.lock() {
            open.request_body = storage.body();
            open.response_body = storage.body();
        }
        events.push_front(event);

        exchange
    }

    pub fn latest_id(&self) -> u64 {
        self.next_id.load(Ordering::Relaxed).saturating_sub(1)
    }

    pub fn storage_problem(&self) -> Option<String> {
        self.storage.lock().unwrap().problem()
    }

    pub fn stored_bytes(&self) -> u64 {
        self.storage.lock().unwrap().used()
    }

    /// The logged exchange with this id.
    pub fn event(&self, id: u64) -> Option<RequestEvent> {
        self.events
            .lock()
            .ok()?
            .iter()
            .find(|event| event.id == id)
            .cloned()
    }

    pub fn note_modified_response(&self) {
        self.counters.modified.fetch_add(1, Ordering::Relaxed);
    }

    /// Copies the most recent events, newest first.
    pub fn recent_events(&self, limit: usize) -> Vec<RequestEvent> {
        self.events
            .lock()
            .map(|events| events.iter().take(limit).cloned().collect())
            .unwrap_or_default()
    }

    pub fn clear_events(&self) {
        if let Ok(mut events) = self.events.lock() {
            let mut storage = self.storage.lock().unwrap();
            match storage.reset() {
                Ok(next) => {
                    events.clear();
                    *storage = next;
                }
                Err(error) => {
                    storage.stop(format!("Could not clear capture storage: {error}"));
                    return;
                }
            }
        }
        // The dashboard tiles count what the log holds, so leaving them running would describe
        // requests that no longer exist anywhere in the app.
        self.counters.proxied.store(0, Ordering::Relaxed);
        self.counters.blocked.store(0, Ordering::Relaxed);
        self.counters.tunneled.store(0, Ordering::Relaxed);
        self.counters.modified.store(0, Ordering::Relaxed);
    }
}

impl RequestEvent {
    /// `id` is assigned by [`ProxyState::record`]; until then it is 0.
    pub fn now(method: impl Into<String>, url: impl Into<String>, kind: EventKind) -> Self {
        Self {
            id: 0,
            at: Local::now(),
            method: method.into(),
            url: url.into(),
            kind,
            exchange: Arc::new(Mutex::new(Exchange::default())),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn event(url: &str) -> RequestEvent {
        RequestEvent::now("GET", url, EventKind::Proxied)
    }

    #[test]
    fn splits_host_path_and_domain() {
        let deep = event("https://a.cdn.example.com/x/y?z=1");
        assert_eq!(deep.host(), "a.cdn.example.com");
        assert_eq!(deep.path(), "/x/y?z=1");
        assert_eq!(deep.domain(), "example.com");

        assert_eq!(event("https://example.com/").domain(), "example.com");
        assert_eq!(event("https://localhost/").domain(), "localhost");
        assert_eq!(event("http://93.184.216.34:80/a").domain(), "93.184.216.34");
        assert_eq!(event("https://example.com").path(), "/");
    }

    #[test]
    fn older_bodies_survive_rapid_traffic_and_large_payloads() {
        let state = ProxyState::new(MitmMode::Full);
        let first = state.record(event("https://example.com/first"));
        let payload = vec![b'x'; 2 * 1024 * 1024];
        first.lock().unwrap().record_response_chunk(&payload);
        for index in 0..800 {
            state.record(event(&format!("https://example.com/{index}")));
        }
        let body = state
            .event(1)
            .unwrap()
            .exchange
            .lock()
            .unwrap()
            .response_body
            .snapshot();
        assert_eq!(body.len, payload.len() as u64);
        assert_eq!(body.read_range(0, payload.len()).unwrap(), payload);
        assert_eq!(state.stored_bytes(), payload.len() as u64);
    }

    #[test]
    fn entry_limit_pauses_instead_of_evicting_and_clear_allows_a_new_capture() {
        let state = ProxyState::new(MitmMode::Full);
        for _ in 0..MAX_LOGGED_REQUESTS + 1 {
            state.record(event("https://example.com/"));
        }
        assert!(state.paused());
        assert!(state.event(1).is_some());
        assert_eq!(state.recent_events(usize::MAX).len(), MAX_LOGGED_REQUESTS);
        state.clear_events();
        assert!(!state.paused());
        assert!(state.recent_events(1).is_empty());
        let exchange = state.record(event("https://example.com/new"));
        exchange.lock().unwrap().record_response_chunk(b"new body");
        assert_eq!(state.stored_bytes(), 8);
    }

    #[test]
    fn paused_exchanges_never_store_bodies() {
        let state = ProxyState::new(MitmMode::Full);
        state.set_paused(true);
        let exchange = state.record(event("https://example.com/"));
        exchange
            .lock()
            .unwrap()
            .record_response_chunk(b"not recorded");
        assert_eq!(state.stored_bytes(), 0);
        assert!(state.recent_events(1).is_empty());
    }

    #[test]
    fn ids_are_assigned_on_record_and_findable() {
        let state = ProxyState::new(MitmMode::HostnameOnly);
        state.record(event("https://example.com/a"));
        state.record(event("https://example.com/b"));
        let newest = state.recent_events(1).remove(0);
        assert_eq!(newest.id, 2);
        assert_eq!(state.event(2).unwrap().url, "https://example.com/b");
        assert!(state.event(99).is_none());
    }
}
