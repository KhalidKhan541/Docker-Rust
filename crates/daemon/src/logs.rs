//! Per-container stdio capture.
//!
//! Mirrors Moby's `json-file` log records (one record per buffered read,
//! `stdout`/`stderr` source, nanosecond timestamps) with two storage layers:
//! a bounded in-memory ring that serves `docker logs` and follow streams, and
//! an optional append-only `<data-root>/containers/<id>/logs.jsonl` file that
//! survives daemon restarts. The file is docker-rs's own lossless format
//! (`{"time":<nanos>,"stream":"stdout","data":"<base64>"}`), not Moby's
//! json-file schema, because container output may contain non-UTF-8 bytes.
//!
//! The store also carries the follow-stream liveness signal. Capture threads
//! reserve writers through [`LogStore::take_writer`]; the aggregate `closed`
//! flag flips only when no writer remains (or the engine force-closes the
//! store on removal/shutdown), so `docker logs -f` keeps reading past the
//! monitor's `wait()` until the real pipes reach EOF.

use std::collections::VecDeque;
use std::fmt;
use std::fs;
use std::io::{BufWriter, Read, Seek, SeekFrom, Write};
use std::path::{Path, PathBuf};
use std::sync::{Arc, Condvar, Mutex};
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use base64::engine::general_purpose::STANDARD as BASE64;
use base64::Engine as _;
use docker_errors::{Error, Result};
use serde::{Deserialize, Serialize};

/// Byte budget for one container's captured output (4 MiB).
pub const DEFAULT_LOG_CAPACITY: usize = 4 * 1024 * 1024;

/// Which of the container's output streams a record came from.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Stream {
    Stdout,
    Stderr,
}

impl Stream {
    /// stdcopy frame identifier (Moby `pkg/stdcopy`): 1 = stdout, 2 = stderr.
    pub fn frame_id(self) -> u8 {
        match self {
            Self::Stdout => 1,
            Self::Stderr => 2,
        }
    }
}

/// One captured chunk: a read boundary of one stream with its timestamp.
#[derive(Debug, Clone)]
pub struct LogEntry {
    /// Monotonic sequence; a follower resumes from `last_seen + 1`.
    pub seq: u64,
    pub stream: Stream,
    /// Nanoseconds since the UNIX epoch when the bytes were read.
    pub timestamp_nanos: i64,
    pub data: Arc<[u8]>,
}

#[derive(Debug)]
struct Record {
    seq: u64,
    stream: Stream,
    timestamp_nanos: i64,
    data: Arc<[u8]>,
    /// Approximate memory cost, for the ring capacity bound.
    cost: usize,
}

/// One line of the on-disk log. `data` is base64 so arbitrary container bytes
/// round-trip exactly.
#[derive(Serialize, Deserialize)]
struct StoredEntry {
    /// Nanoseconds since the UNIX epoch.
    time: i64,
    stream: Stream,
    data: String,
}

#[derive(Debug)]
struct LogSink {
    file: BufWriter<fs::File>,
}

impl LogSink {
    fn append(&mut self, time: i64, stream: Stream, data: &[u8]) -> std::io::Result<()> {
        let entry = StoredEntry {
            time,
            stream,
            data: BASE64.encode(data),
        };
        serde_json::to_writer(&mut self.file, &entry).map_err(std::io::Error::other)?;
        self.file.write_all(b"\n")?;
        self.file.flush()
    }
}

#[derive(Debug)]
struct State {
    entries: VecDeque<Record>,
    bytes: usize,
    next_seq: u64,
    /// Set when no further appends can arrive: every capture writer ended, or
    /// the engine force-closed the store (removal/engine shutdown).
    closed: bool,
    /// Number of live capture writer leases.
    writers: u32,
    /// Present when this store is backed by a container log file. Dropped by
    /// [`LogStore::close`] so Windows can remove the directory.
    sink: Option<LogSink>,
    /// Whether a persistence failure was already reported (avoid one warning
    /// per captured read).
    persist_error_reported: bool,
}

impl State {
    /// Next unseen sequence for a follower that has read everything.
    fn latest_seq(&self) -> u64 {
        self.entries.back().map_or(0, |record| record.seq + 1)
    }
}

/// Shared log buffer for one container.
pub struct LogStore {
    capacity: usize,
    /// Present when this store is backed by a file (reopened after a failed
    /// removal that force-closed the handle).
    path: Option<PathBuf>,
    state: Mutex<State>,
    /// Notified whenever entries are appended, a writer lease is released, or
    /// the store closes, so followers wake up promptly.
    activity: Condvar,
}

impl fmt::Debug for LogStore {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        let summary = match self.state.lock() {
            Ok(state) => Some((
                state.entries.len(),
                state.bytes,
                state.closed,
                state.writers,
                state.sink.is_some(),
            )),
            Err(_) => None,
        };
        match summary {
            Some((records, bytes, closed, writers, persisted)) => f
                .debug_struct("LogStore")
                .field("records", &records)
                .field("bytes", &bytes)
                .field("closed", &closed)
                .field("writers", &writers)
                .field("persisted", &persisted)
                .finish(),
            None => f
                .debug_struct("LogStore")
                .field("state", &"poisoned")
                .finish(),
        }
    }
}

/// Owned lease for one capture thread. Holding it keeps the shared close
/// signal off; dropping it releases the slot and, when it was the last one,
/// marks the store closed for followers.
pub struct WriterLease {
    store: Arc<LogStore>,
}

impl fmt::Debug for WriterLease {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("WriterLease").finish_non_exhaustive()
    }
}

impl WriterLease {
    pub fn append(&self, stream: Stream, data: &[u8]) {
        self.store.append(stream, data)
    }
}

impl Drop for WriterLease {
    fn drop(&mut self) {
        let notify = self
            .store
            .state
            .lock()
            .map(|mut state| {
                state.writers = state.writers.saturating_sub(1);
                let ended = state.writers == 0;
                if ended && !state.closed {
                    state.closed = true;
                }
                ended
            })
            .unwrap_or(false);
        if notify {
            self.store.activity.notify_all();
        }
    }
}

impl LogStore {
    pub fn new(capacity: usize) -> Self {
        Self {
            capacity,
            path: None,
            state: Mutex::new(State {
                entries: VecDeque::new(),
                bytes: 0,
                next_seq: 0,
                closed: false,
                writers: 0,
                sink: None,
                persist_error_reported: false,
            }),
            activity: Condvar::new(),
        }
    }

    /// Open a store backed by `path`, replaying the newest records that fit
    /// the in-memory capacity. The file is append-only; older records remain
    /// on disk but are not served after a restart.
    ///
    /// A torn trailing line (crash during a write) is ignored. Other malformed
    /// lines are also skipped, with a warning, so one corrupt record cannot
    /// make the container's history unreadable.
    pub fn open(path: impl AsRef<Path>, capacity: usize) -> Result<Self> {
        let path = path.as_ref();
        if let Some(parent) = path.parent() {
            fs::create_dir_all(parent).map_err(|error| {
                Error::internal(format!(
                    "creating container log directory `{}`: {error}",
                    parent.display()
                ))
            })?;
        }
        let mut file = fs::OpenOptions::new()
            .create(true)
            .append(true)
            .read(true)
            .open(path)
            .map_err(|error| {
                Error::internal(format!(
                    "opening container log file `{}`: {error}",
                    path.display()
                ))
            })?;

        // Replay enough bytes to fill the ring even though each line carries
        // base64 and JSON overhead.
        let budget = capacity.saturating_mul(2).max(64 * 1024);
        let replay = read_tail(&mut file, budget).map_err(|error| {
            Error::internal(format!(
                "reading container log file `{}`: {error}",
                path.display()
            ))
        })?;

        let mut entries = VecDeque::new();
        let mut bytes = 0usize;
        let mut next_seq = 0u64;
        let ended_with_newline = replay.last() == Some(&b'\n');
        let lines: Vec<&[u8]> = replay
            .split(|byte| *byte == b'\n')
            .filter(|line| !line.is_empty())
            .collect();
        for (index, line) in lines.iter().enumerate() {
            let parsed = serde_json::from_slice::<StoredEntry>(line)
                .ok()
                .and_then(|entry| {
                    BASE64
                        .decode(&entry.data)
                        .ok()
                        .map(|data| (entry.stream, entry.time, data))
                });
            match parsed {
                Some((stream, timestamp_nanos, data)) => {
                    let cost = data.len();
                    entries.push_back(Record {
                        seq: next_seq,
                        stream,
                        timestamp_nanos,
                        data: Arc::from(data),
                        cost,
                    });
                    bytes += cost;
                    next_seq += 1;
                }
                None => {
                    let torn = !ended_with_newline && index + 1 == lines.len();
                    tracing::warn!(
                        path = %path.display(),
                        line = index,
                        torn,
                        "ignoring malformed container log record"
                    );
                }
            }
        }
        while bytes > capacity {
            let Some(evicted) = entries.pop_front() else {
                break;
            };
            bytes = bytes.saturating_sub(evicted.cost);
        }

        Ok(Self {
            capacity,
            path: Some(path.to_path_buf()),
            state: Mutex::new(State {
                entries,
                bytes,
                next_seq,
                closed: false,
                writers: 0,
                sink: Some(LogSink {
                    file: BufWriter::new(file),
                }),
                persist_error_reported: false,
            }),
            activity: Condvar::new(),
        })
    }

    /// Reserve a capture writer that can be moved into a capture thread.
    /// Dropping the lease unregisters it; when the last lease is released the
    /// store closes so pending followers discover EOF.
    pub fn take_writer(self: &Arc<Self>) -> WriterLease {
        if let Ok(mut state) = self.state.lock() {
            state.writers += 1;
            self.activity.notify_all();
        }
        WriterLease {
            store: Arc::clone(self),
        }
    }

    /// Append captured bytes as one record. Timestamps and sequence numbers
    /// are assigned here so capture threads carry no extra bookkeeping.
    pub fn append(&self, stream: Stream, data: &[u8]) {
        if data.is_empty() {
            return;
        }
        let now = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .map(|elapsed| elapsed.as_nanos() as i64)
            .unwrap_or(0);
        let mut wake = false;
        if let Ok(mut state) = self.state.lock() {
            // Records racing a force-close (writer runs while the container is
            // removed) are dropped: followers must not wait for them.
            if !state.closed {
                let seq = state.next_seq;
                state.next_seq += 1;
                if let Some(sink) = state.sink.as_mut() {
                    if let Err(error) = sink.append(now, stream, data) {
                        if !state.persist_error_reported {
                            state.persist_error_reported = true;
                            tracing::warn!(
                                error = %error,
                                "cannot persist container logs; keeping records in memory only"
                            );
                        }
                    }
                }
                let cost = data.len();
                state.entries.push_back(Record {
                    seq,
                    stream,
                    timestamp_nanos: now,
                    data: data.into(),
                    cost,
                });
                state.bytes += cost;
                while state.bytes > self.capacity {
                    let Some(evicted) = state.entries.pop_front() else {
                        break;
                    };
                    state.bytes = state.bytes.saturating_sub(evicted.cost);
                }
                wake = true;
            }
        }
        if wake {
            self.activity.notify_all();
        }
    }

    /// Capture the entries with `seq >= from` matching `stream` (all streams
    /// when `stream` is `None`) together with the closed flag, atomically.
    /// Followers must use this instead of separate `entries_from` and
    /// `is_closed` reads, or they may miss a record appended just before the
    /// close flip and hang until the next activity.
    pub fn entries_and_state(&self, from: u64, stream: Option<Stream>) -> (Vec<LogEntry>, bool) {
        match self.state.lock() {
            Ok(state) => {
                let entries = state
                    .entries
                    .iter()
                    .filter(|record| record.seq >= from)
                    .filter(|record| stream.is_none_or(|stream| record.stream == stream))
                    .map(|record| LogEntry {
                        seq: record.seq,
                        stream: record.stream,
                        timestamp_nanos: record.timestamp_nanos,
                        data: Arc::clone(&record.data),
                    })
                    .collect();
                (entries, state.closed)
            }
            Err(_) => (Vec::new(), true),
        }
    }

    /// Capture entries without the closed flag snapshot (`entries_and_state`
    /// is preferred by followers).
    pub fn entries_from(&self, from: u64, stream: Option<Stream>) -> Vec<LogEntry> {
        self.entries_and_state(from, stream).0
    }

    /// First sequence still stored (0 when the buffer is empty). Filters that
    /// clamp `from` below this read as "all captured entries".
    pub fn oldest_seq(&self) -> u64 {
        self.state
            .lock()
            .map(|state| state.entries.front().map_or(0, |record| record.seq))
            .unwrap_or(0)
    }

    /// Next unseen sequence for a follower that has consumed everything.
    pub fn latest_seq(&self) -> u64 {
        self.state
            .lock()
            .map(|state| state.latest_seq())
            .unwrap_or(0)
    }

    /// Wait until an entry newer than `seq` appears or the store closes;
    /// never blocks longer than `timeout`. Returns the current `latest_seq()`.
    pub fn wait_for_change(&self, seq: u64, timeout: Duration) -> u64 {
        let Ok(state) = self.state.lock() else {
            return self.latest_seq();
        };
        // Keep waiting only while nothing changed and the stream can still
        // grow; once `closed` is set no appends are accepted, so the follower
        // can drain and stop.
        let (state, _) = self
            .activity
            .wait_timeout_while(state, timeout, |state| {
                state.latest_seq() <= seq && !state.closed
            })
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        state.latest_seq()
    }

    /// Force the shared close signal (container removed, engine shutdown, or
    /// a capture thread that could not be started), flush the log file, and
    /// drop its handle so the container directory can be removed. Later
    /// appends are dropped so followers cannot wait for producers that will
    /// never end.
    pub fn close(&self) {
        let notify = self
            .state
            .lock()
            .map(|mut state| {
                let changed = !state.closed;
                state.closed = true;
                if let Some(mut sink) = state.sink.take() {
                    if let Err(error) = sink.file.flush() {
                        tracing::warn!(error = %error, "flushing container log file failed");
                    }
                }
                changed
            })
            .unwrap_or(false);
        if notify {
            self.activity.notify_all();
        }
    }

    /// Close when no capture writer is active (task exit with no captured
    /// stdio, e.g. the synthetic runtime). Writers still draining leave the
    /// flag alone; their lease drops close the store at the right moment.
    pub fn close_if_idle(&self) {
        let notify = self
            .state
            .lock()
            .map(|mut state| {
                let changed = state.writers == 0 && !state.closed;
                if changed {
                    state.closed = true;
                }
                changed
            })
            .unwrap_or(false);
        if notify {
            self.activity.notify_all();
        }
    }

    /// Clear the close signal so a restarted container can capture again and
    /// followers can stream the new records. Captured entries persist. If a
    /// failed removal already dropped the file handle, it is reopened.
    pub fn reopen(&self) {
        let notify = self
            .state
            .lock()
            .map(|mut state| {
                let changed = state.closed;
                state.closed = false;
                if state.sink.is_none() {
                    if let Some(path) = &self.path {
                        match fs::OpenOptions::new()
                            .create(true)
                            .append(true)
                            .read(true)
                            .open(path)
                        {
                            Ok(file) => {
                                state.sink = Some(LogSink {
                                    file: BufWriter::new(file),
                                })
                            }
                            Err(error) => tracing::warn!(
                                path = %path.display(),
                                error = %error,
                                "cannot reopen container log file; keeping new records in memory"
                            ),
                        }
                    }
                }
                changed
            })
            .unwrap_or(false);
        if notify {
            self.activity.notify_all();
        }
    }

    /// True once the aggregate close signal is set.
    pub fn is_closed(&self) -> bool {
        self.state.lock().map(|state| state.closed).unwrap_or(true)
    }

    /// Total captured bytes still stored, across streams.
    pub fn byte_count(&self) -> usize {
        self.state.lock().map(|state| state.bytes).unwrap_or(0)
    }
}

/// Read at most `budget` bytes from the end of `file`, dropping a leading
/// partial line when the read did not start at the beginning.
fn read_tail(file: &mut fs::File, budget: usize) -> std::io::Result<Vec<u8>> {
    let length = file.metadata()?.len();
    let start = length.saturating_sub(budget as u64);
    file.seek(SeekFrom::Start(start))?;
    let mut raw = Vec::with_capacity(length.saturating_sub(start) as usize);
    file.read_to_end(&mut raw)?;
    if start > 0 {
        match raw.iter().position(|byte| *byte == b'\n') {
            Some(position) => {
                raw.drain(..=position);
            }
            None => raw.clear(),
        }
    }
    Ok(raw)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn replays_persisted_chunks_in_order_and_closes_cleanly() {
        let temp = tempfile::tempdir().unwrap();
        let path = temp.path().join("logs.jsonl");
        let store = LogStore::open(&path, 1024).unwrap();
        store.append(Stream::Stdout, b"first\n");
        store.append(Stream::Stderr, b"second");
        let timestamp = store.entries_from(0, None)[0].timestamp_nanos;
        store.close();
        drop(store);

        let store = LogStore::open(&path, 1024).unwrap();
        let entries = store.entries_from(0, None);
        assert_eq!(entries.len(), 2);
        assert_eq!(entries[0].stream, Stream::Stdout);
        assert_eq!(entries[0].data.as_ref(), b"first\n");
        assert_eq!(entries[0].timestamp_nanos, timestamp);
        assert_eq!(entries[0].seq, 0);
        assert_eq!(entries[1].stream, Stream::Stderr);
        assert_eq!(entries[1].data.as_ref(), b"second");
        assert_eq!(entries[1].seq, 1);
    }

    #[test]
    fn preserves_non_utf8_output() {
        let temp = tempfile::tempdir().unwrap();
        let path = temp.path().join("logs.jsonl");
        let payload = [0xff, 0x00, 0xfe, b'\n'];
        let store = LogStore::open(&path, 1024).unwrap();
        store.append(Stream::Stdout, &payload);
        store.close();
        drop(store);

        let store = LogStore::open(&path, 1024).unwrap();
        let entries = store.entries_from(0, None);
        assert_eq!(entries.len(), 1);
        assert_eq!(entries[0].data.as_ref(), &payload);
    }

    #[test]
    fn replay_is_trimmed_to_the_memory_capacity() {
        let temp = tempfile::tempdir().unwrap();
        let path = temp.path().join("logs.jsonl");
        let store = LogStore::open(&path, 8).unwrap();
        for _ in 0..5 {
            store.append(Stream::Stdout, b"1234");
        }
        store.close();
        drop(store);

        let store = LogStore::open(&path, 8).unwrap();
        let entries = store.entries_from(0, None);
        assert_eq!(entries.len(), 2);
        assert_eq!(store.byte_count(), 8);
        assert_eq!(entries[0].seq, 3);
        assert_eq!(entries[1].seq, 4);
    }

    #[test]
    fn ignores_a_torn_trailing_record() {
        let temp = tempfile::tempdir().unwrap();
        let path = temp.path().join("logs.jsonl");
        let store = LogStore::open(&path, 1024).unwrap();
        store.append(Stream::Stdout, b"kept");
        store.close();
        drop(store);

        let mut file = fs::OpenOptions::new().append(true).open(&path).unwrap();
        file.write_all(br#"{"time":1,"stream":"stdout","data":"aGVsbG8="#)
            .unwrap();
        drop(file);

        let store = LogStore::open(&path, 1024).unwrap();
        let entries = store.entries_from(0, None);
        assert_eq!(entries.len(), 1);
        assert_eq!(entries[0].data.as_ref(), b"kept");
    }

    #[test]
    fn close_stops_further_appends() {
        let temp = tempfile::tempdir().unwrap();
        let path = temp.path().join("logs.jsonl");
        let store = LogStore::open(&path, 1024).unwrap();
        store.append(Stream::Stdout, b"before");
        store.close();
        store.append(Stream::Stdout, b"after");
        drop(store);

        let store = LogStore::open(&path, 1024).unwrap();
        let entries = store.entries_from(0, None);
        assert_eq!(entries.len(), 1);
        assert_eq!(entries[0].data.as_ref(), b"before");
    }

    #[test]
    fn restarts_append_to_the_existing_file() {
        let temp = tempfile::tempdir().unwrap();
        let path = temp.path().join("logs.jsonl");
        let store = LogStore::open(&path, 1024).unwrap();
        store.append(Stream::Stdout, b"one");
        store.close();
        drop(store);

        let store = LogStore::open(&path, 1024).unwrap();
        store.reopen();
        store.append(Stream::Stdout, b"two");
        store.close();
        drop(store);

        let store = LogStore::open(&path, 1024).unwrap();
        let data: Vec<Vec<u8>> = store
            .entries_from(0, None)
            .into_iter()
            .map(|entry| entry.data.to_vec())
            .collect();
        assert_eq!(data, vec![b"one".to_vec(), b"two".to_vec()]);
    }
}
