//! Session body files. Payload bytes live on disk, never in the request log's heap.
//!
//! Writers run on Tokio's blocking pool, one outstanding write per HTTP body. Waiting for that
//! write provides backpressure rather than a growing queue or silently discarded chunks.

use std::fs::{File, OpenOptions};
use std::io::{self, Read, Seek, SeekFrom, Write};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::{Arc, Mutex};

pub const CAPTURE_BUDGET: u64 = 512 * 1024 * 1024;
static NEXT_SESSION: AtomicU64 = AtomicU64::new(1);

#[derive(Clone, Debug)]
pub struct BodyStore(Arc<Store>);

#[derive(Debug)]
struct Store {
    directory: PathBuf,
    limit: u64,
    used: AtomicU64,
    next_file: AtomicU64,
    closed: AtomicBool,
    problem: Mutex<Option<String>>,
}

impl BodyStore {
    pub fn new(root: &Path, limit: u64) -> io::Result<Self> {
        let unique = NEXT_SESSION.fetch_add(1, Ordering::Relaxed);
        let epoch = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap_or_default()
            .as_nanos();
        let directory = root.join(format!("session-{epoch}-{}-{unique}", std::process::id()));
        std::fs::create_dir_all(&directory)?;
        Ok(Self(Arc::new(Store {
            directory,
            limit,
            used: AtomicU64::new(0),
            next_file: AtomicU64::new(1),
            closed: AtomicBool::new(false),
            problem: Mutex::new(None),
        })))
    }

    pub fn body(&self) -> Body {
        let id = self.0.next_file.fetch_add(1, Ordering::Relaxed);
        Body(Some(Arc::new(StoredBody {
            store: self.clone(),
            path: self.0.directory.join(format!("{id}.body")),
            seen: AtomicU64::new(0),
            retained: AtomicU64::new(0),
            // Only a writer holds this. Inspection reads a published prefix without this lock.
            writer: Mutex::new(()),
            error: Mutex::new(None),
        })))
    }

    pub fn used(&self) -> u64 {
        self.0.used.load(Ordering::Acquire)
    }

    pub fn problem(&self) -> Option<String> {
        self.0.problem.lock().unwrap().clone()
    }

    pub fn stop(&self, message: String) {
        self.0.problem.lock().unwrap().get_or_insert(message);
    }

    pub fn reset(&self) -> io::Result<Self> {
        let next = Self::new(self.0.directory.parent().unwrap(), self.0.limit)?;
        // Old in-flight streams must not repopulate a capture the user explicitly cleared.
        self.0.closed.store(true, Ordering::Release);
        Ok(next)
    }

    #[cfg(test)]
    pub fn temporary(limit: u64) -> Self {
        Self::new(&std::env::temp_dir().join("privaxy-body-tests"), limit).unwrap()
    }
}

impl Drop for Store {
    fn drop(&mut self) {
        let directory = self.directory.clone();
        std::thread::spawn(move || {
            let _ = std::fs::remove_dir_all(directory);
        });
    }
}

/// Disabled exchanges allocate no payload storage.
#[derive(Clone, Debug, Default)]
pub struct Body(Option<Arc<StoredBody>>);

#[derive(Debug)]
struct StoredBody {
    store: BodyStore,
    path: PathBuf,
    seen: AtomicU64,
    retained: AtomicU64,
    writer: Mutex<()>,
    error: Mutex<Option<String>>,
}

impl Body {
    pub fn enabled(&self) -> bool {
        self.0.is_some()
    }

    /// Blocking disk I/O: call from a blocking worker, never an egui frame or body poll.
    pub fn push(&self, chunk: &[u8]) {
        let Some(body) = &self.0 else { return };
        let _writer = body.writer.lock().unwrap();
        if body.store.0.closed.load(Ordering::Acquire) {
            return;
        }
        body.seen.fetch_add(chunk.len() as u64, Ordering::Release);
        if chunk.is_empty() || body.error.lock().unwrap().is_some() {
            return;
        }
        let size = chunk.len() as u64;
        if body
            .store
            .0
            .used
            .fetch_update(Ordering::AcqRel, Ordering::Acquire, |used| {
                used.checked_add(size)
                    .filter(|&next| next <= body.store.0.limit)
            })
            .is_err()
        {
            let message = format!(
                "Capture storage reached {} MiB. Existing bodies are preserved. Save the capture, then clear it to record more.",
                body.store.0.limit / (1024 * 1024)
            );
            *body.error.lock().unwrap() = Some(message.clone());
            body.store.stop(message);
            return;
        }
        let result = (|| {
            let mut file = OpenOptions::new()
                .create(true)
                .append(true)
                .open(&body.path)?;
            file.write_all(chunk)
        })();
        match result {
            Ok(()) => {
                body.retained.fetch_add(size, Ordering::Release);
            }
            Err(error) => {
                // A failed write may have left a partial tail; readers only see retained bytes.
                let message = format!(
                    "Could not store this body: {error}. Save the capture, then clear it to retry."
                );
                *body.error.lock().unwrap() = Some(message.clone());
                body.store.stop(message);
            }
        }
    }

    pub fn mark_incomplete(&self, message: String) {
        if let Some(body) = &self.0 {
            body.error.lock().unwrap().get_or_insert(message);
        }
    }

    pub fn seen(&self) -> u64 {
        self.0
            .as_ref()
            .map_or(0, |b| b.seen.load(Ordering::Acquire))
    }

    pub fn is_empty(&self) -> bool {
        self.seen() == 0
    }

    pub fn snapshot(&self) -> BodySnapshot {
        BodySnapshot {
            body: self.clone(),
            len: self
                .0
                .as_ref()
                .map_or(0, |b| b.retained.load(Ordering::Acquire)),
            seen: self.seen(),
            error: self
                .0
                .as_ref()
                .and_then(|b| b.error.lock().unwrap().clone()),
        }
    }
}

/// A fixed prefix, so an export or a page read cannot chase an ever-growing stream.
#[derive(Clone, Debug)]
pub struct BodySnapshot {
    body: Body,
    pub len: u64,
    pub seen: u64,
    pub error: Option<String>,
}

impl BodySnapshot {
    pub fn reader(&self) -> io::Result<Box<dyn Read + Send>> {
        if self.len == 0 {
            return Ok(Box::new(io::empty()));
        }
        let body = self
            .body
            .0
            .as_ref()
            .ok_or_else(|| io::Error::other("Body unavailable"))?;
        Ok(Box::new(File::open(&body.path)?.take(self.len)))
    }

    pub fn read_range(&self, offset: u64, length: usize) -> io::Result<Vec<u8>> {
        let count = self.len.saturating_sub(offset).min(length as u64) as usize;
        if count == 0 {
            return Ok(Vec::new());
        }
        let body = self
            .body
            .0
            .as_ref()
            .ok_or_else(|| io::Error::other("Body unavailable"))?;
        let mut file = File::open(&body.path)?;
        file.seek(SeekFrom::Start(offset))?;
        let mut bytes = vec![0; count];
        file.read_exact(&mut bytes)?;
        Ok(bytes)
    }

    pub fn require_complete(&self) -> io::Result<()> {
        if let Some(error) = &self.error {
            return Err(io::Error::other(error.clone()));
        }
        if self.len != self.seen {
            return Err(io::Error::other(
                "The body is still being saved. Try again once it finishes.",
            ));
        }
        Ok(())
    }

    pub fn copy_to(&self, path: &Path) -> io::Result<()> {
        let mut output = std::io::BufWriter::new(File::create(path)?);
        let copied = io::copy(&mut self.reader()?, &mut output)?;
        if copied != self.len {
            return Err(io::Error::other("Body file ended unexpectedly"));
        }
        output.flush()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn quota_preserves_existing_bodies_and_marks_the_incomplete_one() {
        let store = BodyStore::temporary(12);
        let first = store.body();
        first.push(b"complete");
        let next = store.body();
        next.push(b"abcd");
        next.push(b"overflow");
        assert!(store.problem().is_some());
        assert_eq!(store.used(), 12);
        assert_eq!(first.snapshot().read_range(0, 100).unwrap(), b"complete");
        first.snapshot().require_complete().unwrap();
        assert_eq!(next.snapshot().read_range(0, 100).unwrap(), b"abcd");
        assert!(next.snapshot().require_complete().is_err());
        assert_eq!(next.seen(), 12);
    }

    #[test]
    fn clear_stops_old_streams_and_releases_files_after_readers_finish() {
        let old = BodyStore::temporary(100);
        let old_dir = old.0.directory.clone();
        let body = old.body();
        body.push(b"saved");
        let snapshot = body.snapshot();
        let next = old.reset().unwrap();
        body.push(b"must not refill the cleared log");
        assert_eq!(body.seen(), 5);
        assert_eq!(next.used(), 0);
        drop(body);
        drop(old);
        assert_eq!(snapshot.read_range(0, 100).unwrap(), b"saved");
        assert!(old_dir.exists());
        drop(snapshot);
        let deadline = std::time::Instant::now() + std::time::Duration::from_secs(2);
        while old_dir.exists() && std::time::Instant::now() < deadline {
            std::thread::sleep(std::time::Duration::from_millis(5));
        }
        assert!(!old_dir.exists());
    }

    #[test]
    fn disk_failures_are_visible_and_do_not_pretend_to_have_saved_bytes() {
        let store = BodyStore::temporary(100);
        let body = store.body();
        std::fs::remove_dir_all(&store.0.directory).unwrap();
        body.push(b"cannot write");
        assert!(store.problem().unwrap().contains("Could not store"));
        assert_eq!(body.snapshot().len, 0);
        assert!(body.snapshot().require_complete().is_err());
    }

    #[test]
    fn concurrent_writes_respect_the_shared_budget() {
        let store = BodyStore::temporary(16 * 1024);
        let threads: Vec<_> = (0..32)
            .map(|_| {
                let body = store.body();
                std::thread::spawn(move || {
                    body.push(&[42; 1024]);
                    body
                })
            })
            .collect();
        let bodies: Vec<_> = threads
            .into_iter()
            .map(|thread| thread.join().unwrap())
            .collect();
        assert_eq!(
            bodies.iter().map(|body| body.snapshot().len).sum::<u64>(),
            16 * 1024
        );
        assert_eq!(store.used(), 16 * 1024);
        assert!(store.problem().is_some());
    }
}
