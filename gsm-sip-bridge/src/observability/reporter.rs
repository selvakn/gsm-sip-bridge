//! Agent-side half of the observability protocol
//! (specs/014-vowifi-metrics-restore, contracts/observability-protocol.md):
//! a bounded buffer plus a background sender thread, so a VoWiFi agent can
//! report call/SMS/health events to the daemon's `/metrics` registry without
//! ever blocking the call path on a socket (FR-018).
//!
//! `Reporter::report` is non-blocking — it hands the report to a *bounded*
//! `mpsc` channel via `try_send` and returns immediately, never blocking the
//! caller even when full (FR-018). The background thread drains that
//! channel into a bounded ring buffer (capacity `CAPACITY`) and drains the
//! ring buffer into the control socket. Both stages share the same bound
//! deliberately: `send_one` can stall for up to `SOCKET_TIMEOUT` against a
//! daemon that accepted the connection but never responds (as opposed to a
//! fast, clean refusal), and during that stall the ingress channel is the
//! only thing standing between a burst of calls and unbounded memory growth
//! — an unbounded ingress channel would let producers outrun the ring
//! buffer's own bound entirely while the worker is stuck in one `flush`
//! call. If the daemon is unreachable, reports accumulate up to the bound;
//! past that, the oldest queued report is dropped and the drop is counted
//! (FR-019a) and folded into the `dropped` field of the next report that
//! actually gets sent, so the daemon's
//! `gsm_sip_bridge_observability_events_dropped_total` never silently loses
//! a drop even under sustained overflow — this applies uniformly whether the
//! drop happened at the ingress channel (producer outrunning the worker) or
//! the ring buffer (worker outrunning the socket). Nothing here is
//! persisted to disk (FR-019b) — an agent restart starts with an empty
//! buffer, and the next heartbeat re-establishes gauge state within one
//! report interval.

use crate::control::protocol::{AgentKind, AgentReport, AgentState, ControlCmd, ObservedEvent};
use std::collections::VecDeque;
use std::io::{BufRead, BufReader, Write};
use std::os::unix::net::UnixStream;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::mpsc;
use std::sync::Arc;
use std::thread;
use std::time::Duration;

/// research.md §R4: roughly 3 hours of heartbeats at the default 10s
/// interval, or thousands of calls — far beyond any routine daemon restart,
/// and bounded at a few hundred KB.
const CAPACITY: usize = 1024;
const SOCKET_TIMEOUT: Duration = Duration::from_secs(2);

enum ReporterCmd {
    Report {
        state: AgentState,
        events: Vec<ObservedEvent>,
    },
}

/// Handle to a running background reporter thread. Cloneable-by-reference
/// (`SyncSender` is `Clone`) if multiple call sites within one agent need to
/// report independently — construct once per agent process and share it.
#[derive(Clone)]
pub struct Reporter {
    tx: mpsc::SyncSender<ReporterCmd>,
    /// Reports `try_send` couldn't even hand to the channel because it was
    /// already at `CAPACITY` — folded into the worker's own
    /// `dropped_since_last_send` on the next loop iteration (see
    /// `worker_loop`), so an ingress-side drop is counted identically to a
    /// ring-buffer-side one.
    ingress_dropped: Arc<AtomicU64>,
}

impl Reporter {
    /// Spawns the background sender thread and returns a handle. `agent`
    /// and `module_id` are fixed for the reporter's lifetime — a new
    /// `Reporter` is the right way to change either (e.g. never, in
    /// practice: both are resolved once at agent startup).
    pub fn spawn(
        socket_path: String,
        agent: AgentKind,
        module_id: String,
        report_interval: Duration,
    ) -> Reporter {
        let (tx, rx) = mpsc::sync_channel(CAPACITY);
        let ingress_dropped = Arc::new(AtomicU64::new(0));
        let ingress_dropped_worker = ingress_dropped.clone();
        // Random per process lifetime (see `AgentReport::epoch`'s doc
        // comment) — a fresh value each time `Reporter::spawn` runs, which
        // is once per agent process start, is exactly the "never collide
        // with a previous run's seq values" property this needs.
        let epoch: u64 = rand::random();
        thread::Builder::new()
            .name(format!("observability-reporter-{}", agent.as_str()))
            .spawn(move || {
                worker_loop(
                    socket_path,
                    agent,
                    module_id,
                    report_interval,
                    rx,
                    ingress_dropped_worker,
                    epoch,
                )
            })
            .expect("failed to spawn observability reporter thread");
        Reporter {
            tx,
            ingress_dropped,
        }
    }

    /// Enqueues one report. Never blocks: `try_send` on a bounded channel
    /// either succeeds immediately or fails immediately, so a call site
    /// mid-teardown can call this unconditionally without risking a hung
    /// call (FR-018). A full channel — the worker stalled inside a slow
    /// `send_one` against an unreachable-but-not-refusing daemon — counts as
    /// a drop rather than blocking or growing without bound.
    pub fn report(&self, state: AgentState, events: Vec<ObservedEvent>) {
        if self
            .tx
            .try_send(ReporterCmd::Report { state, events })
            .is_err()
        {
            self.ingress_dropped.fetch_add(1, Ordering::Relaxed);
        }
    }
}

struct WorkerState {
    queue: VecDeque<AgentReport>,
    dropped_since_last_send: u64,
    last_state: AgentState,
    epoch: u64,
    next_seq: u64,
}

fn worker_loop(
    socket_path: String,
    agent: AgentKind,
    module_id: String,
    report_interval: Duration,
    rx: mpsc::Receiver<ReporterCmd>,
    ingress_dropped: Arc<AtomicU64>,
    epoch: u64,
) {
    let mut state = WorkerState {
        queue: VecDeque::new(),
        dropped_since_last_send: 0,
        last_state: AgentState::default(),
        epoch,
        next_seq: 1,
    };

    loop {
        match rx.recv_timeout(report_interval) {
            Ok(ReporterCmd::Report {
                state: new_state,
                events,
            }) => {
                enqueue(&mut state, agent, &module_id, new_state, events);
            }
            Err(mpsc::RecvTimeoutError::Timeout) => {
                // Heartbeat: re-report the last known state with no events,
                // so liveness (AGENT_UP) has something to key off during
                // otherwise-idle periods (FR-021).
                let hb_state = state.last_state;
                enqueue(&mut state, agent, &module_id, hb_state, Vec::new());
            }
            Err(mpsc::RecvTimeoutError::Disconnected) => break,
        }

        // Fold in anything `Reporter::report`'s `try_send` couldn't even
        // hand to the channel since the last iteration — an ingress-side
        // drop is exactly as real as a ring-buffer-side one (FR-019a).
        let extra = ingress_dropped.swap(0, Ordering::Relaxed);
        state.dropped_since_last_send += extra;

        flush(&mut state, &socket_path);
    }
}

fn enqueue(
    state: &mut WorkerState,
    agent: AgentKind,
    module_id: &str,
    new_state: AgentState,
    events: Vec<ObservedEvent>,
) {
    state.last_state = new_state;
    if state.queue.len() >= CAPACITY {
        state.queue.pop_front();
        state.dropped_since_last_send += 1;
    }
    let seq = state.next_seq;
    state.next_seq += 1;
    state.queue.push_back(AgentReport {
        agent,
        module_id: module_id.to_string(),
        epoch: state.epoch,
        seq,
        state: new_state,
        events,
        dropped: 0, // set immediately before each send attempt in flush()
    });
}

fn flush(state: &mut WorkerState, socket_path: &str) {
    while let Some(front) = state.queue.front_mut() {
        front.dropped = state.dropped_since_last_send;
        match send_one(socket_path, front) {
            Ok(true) => {
                state.dropped_since_last_send = 0;
                state.queue.pop_front();
            }
            Ok(false) => {
                // The daemon rejected this report outright (malformed from
                // its point of view). Retrying it forever would wedge the
                // queue behind a message that can never succeed, so it is
                // discarded — a permanent failure, not a capacity drop.
                state.queue.pop_front();
            }
            Err(_) => {
                // Connect/write/read failure: transient (daemon down or
                // mid-restart). Leave the report queued and try again next
                // tick.
                break;
            }
        }
    }
}

/// `Ok(true)` = delivered and acknowledged `ok`. `Ok(false)` = delivered but
/// the daemon rejected it. `Err` = could not even complete the round trip.
fn send_one(socket_path: &str, report: &AgentReport) -> std::io::Result<bool> {
    let stream = UnixStream::connect(socket_path)?;
    stream.set_write_timeout(Some(SOCKET_TIMEOUT))?;
    stream.set_read_timeout(Some(SOCKET_TIMEOUT))?;
    let mut writer = stream.try_clone()?;
    let mut reader = BufReader::new(stream);

    let cmd = ControlCmd::Observe {
        report: report.clone(),
    };
    let mut json = serde_json::to_string(&cmd)
        .map_err(|e| std::io::Error::new(std::io::ErrorKind::InvalidData, e))?;
    json.push('\n');
    writer.write_all(json.as_bytes())?;

    let mut line = String::new();
    reader.read_line(&mut line)?;
    let v: serde_json::Value = serde_json::from_str(line.trim())
        .map_err(|e| std::io::Error::new(std::io::ErrorKind::InvalidData, e))?;
    Ok(v.get("ok").and_then(|b| b.as_bool()).unwrap_or(false))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_enqueue_evicts_oldest_past_capacity() {
        let mut state = WorkerState {
            queue: VecDeque::new(),
            dropped_since_last_send: 0,
            last_state: AgentState::default(),
            epoch: 1,
            next_seq: 1,
        };
        for i in 0..CAPACITY + 5 {
            enqueue(
                &mut state,
                AgentKind::Ims,
                &format!("mod-{i}"),
                AgentState::default(),
                Vec::new(),
            );
        }
        assert_eq!(state.queue.len(), CAPACITY);
        assert_eq!(state.dropped_since_last_send, 5);
        // Oldest 5 evicted — the front should be mod-5.
        assert_eq!(state.queue.front().unwrap().module_id, "mod-5");
    }

    #[test]
    fn test_flush_against_unreachable_socket_leaves_queue_intact() {
        let mut state = WorkerState {
            queue: VecDeque::new(),
            dropped_since_last_send: 0,
            last_state: AgentState::default(),
            epoch: 1,
            next_seq: 1,
        };
        enqueue(
            &mut state,
            AgentKind::Sip,
            "mod-x",
            AgentState::default(),
            Vec::new(),
        );
        flush(&mut state, "/nonexistent/path/to/socket.sock");
        assert_eq!(
            state.queue.len(),
            1,
            "unreachable socket must not drop the queued report"
        );
    }

    #[test]
    fn test_seq_increments_per_enqueue_and_stays_fixed_across_retries() {
        // Each new report must get a fresh seq (so the daemon can tell them
        // apart), but a report already queued — including one `flush`
        // retries after a failed send — must keep the seq it was assigned
        // the moment it was enqueued, since that's the identity a lost-ack
        // replay is deduplicated against on the daemon side.
        let mut state = WorkerState {
            queue: VecDeque::new(),
            dropped_since_last_send: 0,
            last_state: AgentState::default(),
            epoch: 42,
            next_seq: 1,
        };
        enqueue(
            &mut state,
            AgentKind::Ims,
            "mod-a",
            AgentState::default(),
            Vec::new(),
        );
        enqueue(
            &mut state,
            AgentKind::Ims,
            "mod-b",
            AgentState::default(),
            Vec::new(),
        );
        assert_eq!(state.queue[0].seq, 1);
        assert_eq!(state.queue[0].epoch, 42);
        assert_eq!(state.queue[1].seq, 2);

        // Simulate a failed send that leaves the front of the queue in
        // place for a retry: its seq must be unchanged.
        flush(&mut state, "/nonexistent/path/to/socket.sock");
        assert_eq!(state.queue[0].seq, 1);
    }
}
