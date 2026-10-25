//! Per-container stdio capture.
//!
//! Mirrors Moby's `json-file` log records (one record per buffered read,
//! `stdout`/`stderr` source, nanosecond timestamps) while keeping the ring
//! buffer in memory: log entries do not yet survive daemon restarts, which
//! matches the fact that container records are not persisted either. The
//! bounded capacity approximates Moby's default 10 MiB log driver retention.
//!
//! The store also carries the follow-stream liveness signal. Capture threads
//! reserve writers through [`LogStore::take_writer`]; the aggregate `closed`
//! flag flips only when no writer remains (or the engine force-closes the
//! store on removal/shutdown), so `docker logs -f` keeps reading past the
//! monitor's `wait()` until the real pipes reach EOF.

use std::collections::VecDeque;
use std::fmt;
use std::sync::{Arc, Condvar, Mutex};
use std::time::{Duration, SystemTime, UNIX_EPOCH};

/// Byte budget for one container's captured output (4 MiB).
pub const DEFAULT_LOG_CAPACITY: usize = 4 * 1024 * 1024;

/// Which of the container's output streams a record came from.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
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
            )),
            Err(_) => None,
        };
        match summary {
            Some((records, bytes, closed, writers)) => f
                .debug_struct("LogStore")
                .field("records", &records)
                .field("bytes", &bytes)
                .field("closed", &closed)
                .field("writers", &writers)
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
            state: Mutex::new(State {
                entries: VecDeque::new(),
                bytes: 0,
                next_seq: 0,
                closed: false,
                writers: 0,
            }),
            activity: Condvar::new(),
        }
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
    /// a capture thread that could not be started). Later appends are dropped
    /// so followers cannot wait for producers that will never end.
    pub fn close(&self) {
        let notify = self
            .state
            .lock()
            .map(|mut state| {
                let changed = !state.closed;
                state.closed = true;
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
    /// followers can stream the new records. Captured entries persist.
    pub fn reopen(&self) {
        let notify = self
            .state
            .lock()
            .map(|mut state| {
                let changed = state.closed;
                state.closed = false;
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
