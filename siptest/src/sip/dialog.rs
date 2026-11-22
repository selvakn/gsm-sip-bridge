//! Servicing a connected call's in-dialog requests while its media runs.
//!
//! `media::session::run` blocks for the whole call, so on its own nothing
//! would read the SIP socket for the duration — a `BYE` from the far end
//! would go unanswered (the peer retransmits it for ~32 s) while the call
//! ran out its configured length. [`watch_while`] runs the media on the
//! calling thread and a small watcher thread beside it that answers what
//! arrives and, on a `BYE`, raises the media session's `stop` flag.
//!
//! Where the requests come from differs by direction, and is the caller's
//! `next_request`:
//! - **outbound**: the inbound listener thread is free, so it is the only
//!   reader of the socket; it forwards requests carrying this dialog's
//!   `Call-ID` down a bounded channel (`api::state::DialogRoute`);
//! - **inbound**: the listener thread *is* the one running the call, so the
//!   watcher reads the socket's request queue itself.
//!
//! What is answered: `BYE` → `200` and the call ends; `re-INVITE`/`UPDATE` →
//! `488`, because siptest cannot renegotiate media mid-call and says so
//! plainly rather than silently dropping the request or agreeing to
//! something it won't do; `OPTIONS` and unrelated `INVITE`s → the stray
//! handling every other waiting loop uses.

use std::net::SocketAddr;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
use std::thread;
use std::time::Duration;

use gsm_sip_bridge::ims::sip_client::{build_uas_response, SipRequest};

use crate::sip::inbound::handle_stray;
use crate::sip::socket::SipSocket;

/// How often the watcher re-checks that the call is still running when no
/// request arrives. Bounds how long the call's end waits on the watcher.
const POLL: Duration = Duration::from_millis(100);

/// What a request meant for the call.
#[derive(Debug, PartialEq, Eq)]
pub enum Disposition {
    /// The far end hung up; it has been answered with `200`.
    RemoteBye,
    /// Answered or deliberately ignored; the call carries on.
    Handled,
}

/// Answers one request that reached a connected dialog.
pub fn respond(
    socket: &SipSocket,
    req: &SipRequest,
    peer: SocketAddr,
    call_id: &str,
) -> Disposition {
    let in_dialog = req.header("Call-ID") == Some(call_id);
    match (req.method.as_str(), in_dialog) {
        ("BYE", true) => {
            let _ = socket.send(peer, &build_uas_response(200, "OK", req, None, None, None));
            Disposition::RemoteBye
        }
        ("INVITE" | "UPDATE", true) => {
            tracing::warn!(
                method = %req.method,
                "refusing a mid-call media renegotiation with 488"
            );
            let _ = socket.send(
                peer,
                &build_uas_response(488, "Not Acceptable Here", req, None, None, None),
            );
            Disposition::Handled
        }
        // A retransmitted ACK for our 2xx, or another in-dialog request we
        // have no answer for (INFO, NOTIFY...): nothing to do.
        (_, true) => Disposition::Handled,
        _ => {
            handle_stray(socket, req, peer, call_id);
            Disposition::Handled
        }
    }
}

/// Raises `finished` when dropped, so a panic in the call's own body still
/// lets the watcher thread leave the scope instead of hanging it.
struct FinishOnDrop<'a>(&'a AtomicBool);

impl Drop for FinishOnDrop<'_> {
    fn drop(&mut self) {
        self.0.store(true, Ordering::Relaxed);
    }
}

/// Runs `body` (the media session) while a watcher thread answers requests
/// from `next_request`. Returns `body`'s result and whether the far end hung
/// up — in which case `stop` has been raised, so a well-behaved `body` has
/// already returned early.
pub fn watch_while<T>(
    socket: &SipSocket,
    call_id: &str,
    stop: &Arc<AtomicBool>,
    mut next_request: impl FnMut(Duration) -> Option<(SipRequest, SocketAddr)> + Send,
    body: impl FnOnce() -> T,
) -> (T, bool) {
    let finished = AtomicBool::new(false);
    thread::scope(|scope| {
        let watcher = scope.spawn(|| {
            let mut remote_bye = false;
            while !finished.load(Ordering::Relaxed) {
                let Some((req, peer)) = next_request(POLL) else {
                    continue;
                };
                // Keeps answering after the first BYE so a retransmission of
                // it still gets its `200` while the call winds down.
                if respond(socket, &req, peer, call_id) == Disposition::RemoteBye {
                    remote_bye = true;
                    stop.store(true, Ordering::Relaxed);
                }
            }
            remote_bye
        });
        let out = {
            let _finish = FinishOnDrop(&finished);
            body()
        };
        let remote_bye = watcher.join().unwrap_or(false);
        (out, remote_bye)
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    const CALL: &str = "dialog-call-1";

    fn request(method: &str, call_id: &str) -> SipRequest {
        SipRequest {
            method: method.to_string(),
            request_uri: "sip:1002@127.0.0.1:5065".to_string(),
            headers: vec![
                (
                    "Via".to_string(),
                    "SIP/2.0/UDP 127.0.0.1:5072;branch=z9hG4bK1".to_string(),
                ),
                (
                    "From".to_string(),
                    "<sip:peer@127.0.0.1>;tag=peertag".to_string(),
                ),
                (
                    "To".to_string(),
                    "<sip:1002@127.0.0.1>;tag=ourtag".to_string(),
                ),
                ("Call-ID".to_string(), call_id.to_string()),
                ("CSeq".to_string(), format!("7 {method}")),
                ("Content-Length".to_string(), "0".to_string()),
            ],
            body: String::new(),
            body_bytes: Vec::new(),
        }
    }

    /// Two sockets on loopback: `ours` answers, `theirs` is the peer.
    fn pair() -> (SipSocket, SipSocket) {
        let bind = || {
            SipSocket::bind(
                Some("127.0.0.1".parse().unwrap()),
                0,
                "127.0.0.1:1".parse().unwrap(),
            )
            .unwrap()
        };
        (bind(), bind())
    }

    fn reply_to(theirs: &SipSocket, method: &str, wait: Duration) -> Option<u16> {
        theirs
            .recv_response(CALL, 7, method, wait)
            .unwrap()
            .map(|r| r.status)
    }

    #[test]
    fn a_bye_is_answered_200_and_reported_as_a_hang_up() {
        let (ours, theirs) = pair();
        let d = respond(&ours, &request("BYE", CALL), theirs.local_addr(), CALL);
        assert_eq!(d, Disposition::RemoteBye);
        assert_eq!(reply_to(&theirs, "BYE", Duration::from_secs(2)), Some(200));
    }

    #[test]
    fn a_re_invite_or_update_is_refused_with_488_and_the_call_carries_on() {
        for method in ["INVITE", "UPDATE"] {
            let (ours, theirs) = pair();
            let d = respond(&ours, &request(method, CALL), theirs.local_addr(), CALL);
            assert_eq!(d, Disposition::Handled, "{method} must not end the call");
            assert_eq!(
                reply_to(&theirs, method, Duration::from_secs(2)),
                Some(488),
                "{method}"
            );
        }
    }

    #[test]
    fn a_bye_for_some_other_dialog_does_not_end_this_call() {
        let (ours, theirs) = pair();
        let d = respond(
            &ours,
            &request("BYE", "someone-elses-call"),
            theirs.local_addr(),
            CALL,
        );
        assert_eq!(d, Disposition::Handled);
    }

    #[test]
    fn an_ack_for_the_dialog_is_absorbed_without_a_reply() {
        let (ours, theirs) = pair();
        let d = respond(&ours, &request("ACK", CALL), theirs.local_addr(), CALL);
        assert_eq!(d, Disposition::Handled);
        assert_eq!(reply_to(&theirs, "ACK", Duration::from_millis(300)), None);
    }

    #[test]
    fn a_different_calls_invite_is_still_busied_out() {
        let (ours, theirs) = pair();
        respond(
            &ours,
            &request("INVITE", "another-call"),
            theirs.local_addr(),
            CALL,
        );
        let busy = theirs
            .recv_response("another-call", 7, "INVITE", Duration::from_secs(2))
            .unwrap()
            .expect("a second concurrent call is busied out");
        assert_eq!(busy.status, 486);
    }

    #[test]
    fn a_bye_raises_stop_and_is_reported_to_the_caller() {
        let (ours, theirs) = pair();
        let stop = Arc::new(AtomicBool::new(false));
        let mut queued = Some((request("BYE", CALL), theirs.local_addr()));

        let started = std::time::Instant::now();
        let ((), hung_up) = watch_while(
            &ours,
            CALL,
            &stop,
            |wait| {
                // Delivers the BYE once, then behaves like an idle queue.
                queued.take().or_else(|| {
                    thread::sleep(wait);
                    None
                })
            },
            || {
                // Stands in for the media session: runs until `stop`.
                while !stop.load(Ordering::Relaxed) {
                    assert!(started.elapsed() < Duration::from_secs(5), "never stopped");
                    thread::sleep(Duration::from_millis(5));
                }
            },
        );
        assert!(hung_up);
        assert_eq!(reply_to(&theirs, "BYE", Duration::from_secs(2)), Some(200));
    }

    #[test]
    fn with_no_request_the_body_runs_to_completion_and_no_hang_up_is_reported() {
        let (ours, _theirs) = pair();
        let stop = Arc::new(AtomicBool::new(false));
        let (value, hung_up) = watch_while(
            &ours,
            CALL,
            &stop,
            |wait| {
                thread::sleep(wait);
                None
            },
            || 42,
        );
        assert_eq!(value, 42);
        assert!(!hung_up);
        assert!(
            !stop.load(Ordering::Relaxed),
            "stop is the caller's to raise"
        );
    }

    /// A panic in the call's own body must not strand the watcher thread —
    /// `thread::scope` would then wait for it forever.
    #[test]
    fn a_panicking_body_does_not_hang_the_watcher() {
        let (ours, _theirs) = pair();
        let stop = Arc::new(AtomicBool::new(false));
        let started = std::time::Instant::now();
        let result = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
            watch_while(
                &ours,
                CALL,
                &stop,
                |wait| {
                    thread::sleep(wait);
                    None
                },
                || -> () { panic!("media session blew up") },
            )
        }));
        assert!(result.is_err());
        assert!(
            started.elapsed() < Duration::from_secs(3),
            "the watcher outlived its call"
        );
    }
}
