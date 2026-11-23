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
//! What is answered: `BYE` → `200` and the call ends; `re-INVITE` → `488`,
//! because siptest cannot renegotiate media mid-call and says so plainly
//! rather than silently dropping the request or agreeing to something it
//! won't do; `OPTIONS` → `200`, in or out of the dialog; any other in-dialog
//! method, `UPDATE` included → `405` with the `Allow` list (siptest does not
//! advertise `UPDATE`, so there is no offer of its to refuse); unrelated
//! `INVITE`s → the stray handling every other waiting loop uses.

use std::net::SocketAddr;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
use std::thread;
use std::time::{Duration, Instant};

use gsm_sip_bridge::ims::sip_client::{build_uas_response, SipRequest};

use crate::sip::inbound::{build_405, handle_stray, has_to_tag, RequestCtx};

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

/// Answers one request that reached a connected dialog. Whatever is not
/// specifically about this dialog is the shared stray handling's
/// ([`handle_stray`]).
pub fn respond(
    ctx: &RequestCtx<'_>,
    req: &SipRequest,
    peer: SocketAddr,
    call_id: &str,
) -> Disposition {
    let in_dialog = req.header("Call-ID") == Some(call_id);
    match (req.method.as_str(), in_dialog) {
        ("BYE", true) => {
            let _ = ctx
                .socket
                .send(peer, &build_uas_response(200, "OK", req, None, None, None));
            Disposition::RemoteBye
        }
        // An INVITE with no `To` tag is the INVITE that set this very call up,
        // late or duplicated on the wire: nothing to refuse, and a `488` for
        // it would read as the call having failed.
        ("INVITE", true) if !has_to_tag(req) => Disposition::Handled,
        ("INVITE", true) => {
            tracing::warn!("refusing a mid-call re-INVITE with 488");
            let _ = ctx.socket.send(
                peer,
                &build_uas_response(488, "Not Acceptable Here", req, None, None, None),
            );
            Disposition::Handled
        }
        // A retransmitted ACK for our 2xx, or a CANCEL for a transaction
        // that is already over: nothing to answer.
        ("ACK" | "CANCEL", true) => Disposition::Handled,
        // Any other in-dialog request (UPDATE, INFO, NOTIFY, MESSAGE...):
        // siptest implements none of them, and does not advertise them, and says so rather than going silent, so
        // the peer does not retransmit it into the void. (`OPTIONS` is a
        // keepalive and is answered by the stray handling, in or out of the
        // dialog.)
        (method, true) if method != "OPTIONS" => {
            let _ = ctx.socket.send(peer, &build_405(req));
            Disposition::Handled
        }
        _ => {
            handle_stray(ctx, req, peer, call_id);
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
/// already returned early. A `BYE` for the dialog is recorded in `hangups`
/// before it is answered.
pub fn watch_while<T>(
    ctx: &RequestCtx<'_>,
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
                // Recorded *before* the `200` leaves, so there is no moment
                // at which the peer has been told the dialog is over but a
                // retransmission of its `BYE` would find nobody to answer it
                // once this call is gone.
                if req.method == "BYE" && req.header("Call-ID") == Some(call_id) {
                    ctx.hangups
                        .lock()
                        .unwrap_or_else(|e| e.into_inner())
                        .record(call_id, Instant::now());
                }
                // Keeps answering after the first BYE so a retransmission of
                // it still gets its `200` while the call winds down.
                if respond(ctx, &req, peer, call_id) == Disposition::RemoteBye {
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
        // A panicking watcher must not take the call (or, for an inbound
        // call, the listener thread running it) down with it — but it must
        // not vanish either: no request was serviced after it died.
        let remote_bye = watcher.join().unwrap_or_else(|_| {
            tracing::error!("the dialog watcher panicked; in-dialog requests went unserviced");
            false
        });
        (out, remote_bye)
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::api::state::RecentHangups;
    use crate::sip::socket::SipSocket;
    use std::sync::Mutex;

    const CALL: &str = "dialog-call-1";

    /// A request carrying `call_id`; with or without a `To` tag (a dialog's
    /// own requests have one, the INVITE that starts it does not).
    fn request_with(method: &str, call_id: &str, to_tag: bool) -> SipRequest {
        let to = if to_tag {
            "<sip:1002@127.0.0.1>;tag=ourtag"
        } else {
            "<sip:1002@127.0.0.1>"
        };
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
                ("To".to_string(), to.to_string()),
                ("Call-ID".to_string(), call_id.to_string()),
                ("CSeq".to_string(), format!("7 {method}")),
                ("Content-Length".to_string(), "0".to_string()),
            ],
            body: String::new(),
            body_bytes: Vec::new(),
        }
    }

    fn request(method: &str, call_id: &str) -> SipRequest {
        request_with(method, call_id, true)
    }

    /// Two sockets on loopback — `ours` answers, `theirs` is the peer — and
    /// the record of ended dialogs the answering side consults.
    struct Rig {
        ours: SipSocket,
        theirs: SipSocket,
        hangups: Mutex<RecentHangups>,
    }

    impl Rig {
        fn new() -> Self {
            let bind = || {
                SipSocket::bind(
                    Some("127.0.0.1".parse().unwrap()),
                    0,
                    "127.0.0.1:1".parse().unwrap(),
                )
                .unwrap()
            };
            Self {
                ours: bind(),
                theirs: bind(),
                hangups: Mutex::new(RecentHangups::default()),
            }
        }

        fn ctx(&self) -> RequestCtx<'_> {
            RequestCtx {
                socket: &self.ours,
                trusted: "127.0.0.1".parse().unwrap(),
                hangups: &self.hangups,
            }
        }

        fn peer(&self) -> SocketAddr {
            self.theirs.local_addr()
        }

        /// Feeds `req` to [`respond`] for the call [`CALL`].
        fn respond(&self, req: &SipRequest) -> Disposition {
            respond(&self.ctx(), req, self.peer(), CALL)
        }

        /// The status the peer received for its `method` request on `call_id`.
        fn reply(&self, call_id: &str, method: &str, wait: Duration) -> Option<u16> {
            self.theirs
                .recv_response(call_id, 7, method, wait)
                .unwrap()
                .map(|r| r.status)
        }
    }

    const WAIT: Duration = Duration::from_secs(2);
    const NO_REPLY: Duration = Duration::from_millis(300);

    #[test]
    fn a_bye_is_answered_200_and_reported_as_a_hang_up() {
        let rig = Rig::new();
        assert_eq!(rig.respond(&request("BYE", CALL)), Disposition::RemoteBye);
        assert_eq!(rig.reply(CALL, "BYE", WAIT), Some(200));
    }

    #[test]
    fn a_re_invite_is_refused_with_488_and_the_call_carries_on() {
        let rig = Rig::new();
        assert_eq!(
            rig.respond(&request("INVITE", CALL)),
            Disposition::Handled,
            "a re-INVITE must not end the call"
        );
        assert_eq!(rig.reply(CALL, "INVITE", WAIT), Some(488));
    }

    /// `UPDATE` is not in the `Allow` list siptest advertises, so it is an
    /// unsupported method (`405` with `Allow`), not an offer to refuse — and
    /// an SDP-less one (a session refresh) has no offer at all.
    #[test]
    fn an_update_is_an_unsupported_method_405_not_a_refused_offer() {
        let rig = Rig::new();
        assert_eq!(
            rig.respond(&request("UPDATE", CALL)),
            Disposition::Handled,
            "an UPDATE must not end the call"
        );
        assert_eq!(rig.reply(CALL, "UPDATE", WAIT), Some(405));
    }

    /// An INVITE with no `To` tag and this dialog's Call-ID is the one that
    /// set the call up, late or duplicated on the wire — not a renegotiation.
    /// A `488` for it would read to the peer as the call having failed.
    #[test]
    fn a_duplicate_of_the_initial_invite_is_ignored_not_refused() {
        let rig = Rig::new();
        assert_eq!(
            rig.respond(&request_with("INVITE", CALL, false)),
            Disposition::Handled
        );
        assert_eq!(rig.reply(CALL, "INVITE", NO_REPLY), None);
    }

    /// A re-INVITE whose `To` tag is written with the spacing RFC 3261 allows
    /// is still a re-INVITE: it must be refused, not ignored as if it were a
    /// duplicate of the INVITE that set the call up.
    #[test]
    fn a_re_invite_with_a_spaced_to_tag_is_still_refused_with_488() {
        for to in [
            "<sip:1002@127.0.0.1> ; tag=ourtag",
            "<sip:1002@127.0.0.1>;  tag = ourtag",
            "<sip:1002@127.0.0.1>;Tag=ourtag",
        ] {
            let rig = Rig::new();
            let mut req = request("INVITE", CALL);
            for (name, value) in req.headers.iter_mut() {
                if name == "To" {
                    *value = to.to_string();
                }
            }
            assert_eq!(rig.respond(&req), Disposition::Handled);
            assert_eq!(rig.reply(CALL, "INVITE", WAIT), Some(488), "To: {to}");
        }
    }

    /// The dialog's own Call-ID must not turn a keepalive into silence.
    #[test]
    fn options_is_answered_whether_or_not_it_names_the_dialog() {
        for call_id in [CALL, "some-other-call"] {
            let rig = Rig::new();
            assert_eq!(
                rig.respond(&request("OPTIONS", call_id)),
                Disposition::Handled
            );
            assert_eq!(
                rig.reply(call_id, "OPTIONS", WAIT),
                Some(200),
                "OPTIONS with Call-ID {call_id} got no reply"
            );
        }
    }

    /// siptest implements no other in-dialog method; it says so instead of
    /// leaving the peer to retransmit into silence.
    #[test]
    fn any_other_in_dialog_method_is_refused_with_405() {
        for method in ["INFO", "NOTIFY", "MESSAGE", "UPDATE"] {
            let rig = Rig::new();
            assert_eq!(
                rig.respond(&request(method, CALL)),
                Disposition::Handled,
                "{method} must not end the call"
            );
            assert_eq!(rig.reply(CALL, method, WAIT), Some(405), "{method}");
        }
    }

    #[test]
    fn a_bye_for_an_unknown_dialog_does_not_end_this_call() {
        let rig = Rig::new();
        assert_eq!(
            rig.respond(&request("BYE", "someone-elses-call")),
            Disposition::Handled
        );
        assert_eq!(rig.reply("someone-elses-call", "BYE", NO_REPLY), None);
    }

    /// A call is in progress when the far end of an *earlier* one retransmits
    /// its BYE (our `200` was lost). The reader serving the new call is the
    /// only one listening, so it must answer for the old one too — without
    /// ending its own.
    #[test]
    fn a_bye_for_a_dialog_that_just_ended_is_answered_during_another_call() {
        let rig = Rig::new();
        rig.hangups
            .lock()
            .unwrap()
            .record("earlier-call", Instant::now());

        assert_eq!(
            rig.respond(&request("BYE", "earlier-call")),
            Disposition::Handled,
            "a BYE for another dialog must not end this call"
        );
        assert_eq!(rig.reply("earlier-call", "BYE", WAIT), Some(200));
    }

    #[test]
    fn an_ack_for_the_dialog_is_absorbed_without_a_reply() {
        let rig = Rig::new();
        assert_eq!(rig.respond(&request("ACK", CALL)), Disposition::Handled);
        assert_eq!(rig.reply(CALL, "ACK", NO_REPLY), None);
    }

    #[test]
    fn a_different_calls_invite_is_still_busied_out() {
        let rig = Rig::new();
        rig.respond(&request_with("INVITE", "another-call", false));
        assert_eq!(rig.reply("another-call", "INVITE", WAIT), Some(486));
    }

    /// An INVITE that names a dialog we do not have is a re-INVITE that
    /// crossed a hang-up, not a new call; `481` says so.
    #[test]
    fn an_invite_for_a_dialog_we_no_longer_have_is_481_not_busy() {
        let rig = Rig::new();
        rig.respond(&request("INVITE", "ended-call"));
        assert_eq!(rig.reply("ended-call", "INVITE", WAIT), Some(481));
    }

    #[test]
    fn an_invite_for_a_dialog_we_no_longer_have_is_481_whatever_the_tag_spacing() {
        let rig = Rig::new();
        let mut req = request("INVITE", "ended-call");
        for (name, value) in req.headers.iter_mut() {
            if name == "To" {
                *value = "<sip:1002@127.0.0.1> ; tag = gone".to_string();
            }
        }
        rig.respond(&req);
        assert_eq!(rig.reply("ended-call", "INVITE", WAIT), Some(481));
    }

    /// Whichever reader meets an unknown method gets the same answer as the
    /// listener always gave it.
    #[test]
    fn an_unknown_method_from_outside_the_dialog_is_405_for_every_reader() {
        let rig = Rig::new();
        rig.respond(&request("MESSAGE", "unrelated"));
        assert_eq!(rig.reply("unrelated", "MESSAGE", WAIT), Some(405));
    }

    #[test]
    fn a_bye_raises_stop_and_is_reported_to_the_caller() {
        let rig = Rig::new();
        let stop = Arc::new(AtomicBool::new(false));
        let mut queued = Some((request("BYE", CALL), rig.peer()));

        let started = Instant::now();
        let ((), hung_up) = watch_while(
            &rig.ctx(),
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
        assert_eq!(rig.reply(CALL, "BYE", WAIT), Some(200));
        assert!(
            rig.hangups.lock().unwrap().contains(CALL, Instant::now()),
            "the hang-up must be remembered for a retransmitted BYE"
        );
    }

    #[test]
    fn with_no_request_the_body_runs_to_completion_and_no_hang_up_is_reported() {
        let rig = Rig::new();
        let stop = Arc::new(AtomicBool::new(false));
        let (value, hung_up) = watch_while(
            &rig.ctx(),
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
        let rig = Rig::new();
        let stop = Arc::new(AtomicBool::new(false));
        let started = Instant::now();
        let result = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
            watch_while(
                &rig.ctx(),
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

    /// A watcher that panics is logged and reported as "no hang-up", not
    /// propagated: for an inbound call it runs on the listener thread.
    #[test]
    fn a_panicking_watcher_does_not_take_the_call_down_with_it() {
        let rig = Rig::new();
        let stop = Arc::new(AtomicBool::new(false));
        let (value, hung_up) = watch_while(
            &rig.ctx(),
            CALL,
            &stop,
            |_wait| -> Option<(SipRequest, SocketAddr)> { panic!("source blew up") },
            || {
                thread::sleep(Duration::from_millis(200));
                7
            },
        );
        assert_eq!(value, 7);
        assert!(!hung_up);
    }
}
