//! specs/090-reg-event-subscription-tracking: the reg-event subscription as a
//! tracked SIP transaction and dialog, rather than a request sent and forgotten.
//!
//! [`RegSubscription`] is a pure state machine, like [`super::ping::PingState`]:
//! it never touches a socket or reads the clock. Every entry point takes `now`,
//! and the I/O it wants comes back as a [`SubAction`] for the dispatch loop to
//! perform and report back (research R1). That is what lets every timer below
//! be tested without sleeping.
//!
//! What it tracks, per line:
//!
//! - the current subscription's identifiers, the network's tag and route set,
//!   its expiry, and whether a NOTIFY has confirmed it;
//! - the SUBSCRIBE transaction in flight, with Timer F (no final response) and
//!   Timer N (no NOTIFY) running against it;
//! - subscriptions it has replaced, until they can no longer be notified, so a
//!   late NOTIFY for one is acknowledged without being acted on;
//! - its own retry backoff, separate from registration renewal's (FR-009a).

use super::call::{contact_uri, route_set_in_order, uac_route_set};
use super::header_tag;
use super::ping::{parse_cseq_number, PING_RESPONSE_TIMEOUT};
use crate::ims::session::{
    next_backoff, parse_subscription_state, SubState, SubStateKind, MAX_NETWORK_DELAY_SECS,
};
use crate::ims::sip_client::{SipRequest, SipResponse};
use crate::ims::SubscriptionHealth;
use std::collections::VecDeque;
use std::time::{Duration, Instant, SystemTime};

/// RFC 3261 Timer F for a non-INVITE transaction: 64 × T1, with the IMS T1 of
/// 2 s (TS 24.229 table 7.7.1).
pub(super) const TIMER_F: Duration = Duration::from_secs(128);
/// RFC 6665 Timer N: how long an accepted subscription may wait for its first
/// NOTIFY. Also 64 × T1.
pub(super) const TIMER_N: Duration = Duration::from_secs(128);
/// The subscription's own retry backoff: 5 s, doubling, capped at 600 s
/// (clarification Q2).
pub(super) const SUB_INITIAL_BACKOFF: Duration = Duration::from_secs(5);
pub(super) const SUB_MAX_BACKOFF: Duration = Duration::from_secs(600);
/// How long past its expiry a replaced subscription is still recognised: a
/// NOTIFY can be in flight for up to Timer F's span (clarification Q4).
pub(super) const SUPERSEDED_GRACE: Duration = Duration::from_secs(128);
/// At most this many replaced subscriptions are remembered per line; the
/// oldest is forgotten first.
pub(super) const SUPERSEDED_CAP: usize = 32;
/// `now + delay`, with `delay` clamped so a network-supplied value can never
/// overflow the `Instant` (see [`MAX_NETWORK_DELAY_SECS`]).
fn after(now: Instant, delay: Duration) -> Instant {
    now + delay.min(Duration::from_secs(MAX_NETWORK_DELAY_SECS))
}

/// Whether a SUBSCRIBE starts a subscription or refreshes one.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) enum SubKind {
    Initial,
    Refresh,
}

impl SubKind {
    fn as_str(self) -> &'static str {
        match self {
            SubKind::Initial => "initial",
            SubKind::Refresh => "refresh",
        }
    }
}

/// Why an unanswered confirming probe is a qualifying endpoint failure.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) enum EndpointFailureCause {
    /// Timer F expired on a SUBSCRIBE.
    SubscribeTimeout,
    /// The SUBSCRIBE could not be sent at all.
    SubscribeSendFailed,
}

impl std::fmt::Display for EndpointFailureCause {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(match self {
            EndpointFailureCause::SubscribeTimeout => "subscribe-timeout",
            EndpointFailureCause::SubscribeSendFailed => "subscribe-send-failed",
        })
    }
}

/// What the dispatch loop should do for the subscription.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(super) enum SubAction {
    /// Send a new subscription: fresh Call-ID and tag.
    SendInitial { expires: u32 },
    /// Refresh the current subscription inside its dialog
    /// ([`RegSubscription::refresh_dialog`]).
    SendRefresh { expires: u32 },
    /// Send one Gm keepalive probe now, even mid-call, and report it with
    /// [`RegSubscription::on_probe_sent`] or
    /// [`RegSubscription::on_probe_send_failed`].
    Probe {
        cause: EndpointFailureCause,
        call_id: Option<String>,
    },
    /// The probe went unanswered: re-register (research R5).
    EndpointFailure {
        cause: EndpointFailureCause,
        call_id: Option<String>,
    },
}

/// Which subscription a SUBSCRIBE response belongs to.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) enum ResponseMatch {
    /// The outstanding transaction of the current subscription.
    Current,
    /// A remembered older subscription, or an older CSeq of the current one.
    Superseded,
    /// Nothing this line remembers.
    Unknown,
}

/// Which subscription a NOTIFY belongs to, which decides how it is answered.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(super) enum NotifyMatch {
    /// `200`, then evaluated (spec 089) and its `Subscription-State` applied.
    Current { state: SubState },
    /// `200`, logged, never applied (FR-015).
    Superseded,
    /// `481` (FR-016).
    Unknown,
}

/// The SUBSCRIBE transaction in flight on the current subscription.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Txn {
    Pending {
        cseq: u32,
        sent_at: Instant,
        kind: SubKind,
    },
    Idle,
}

/// Timer F fired or the send failed: waiting on the Gm keepalive probe that
/// decides between a subscription failure and a dead path.
#[derive(Debug, Clone)]
struct Probing {
    cause: EndpointFailureCause,
    call_id: Option<String>,
    /// The probe's CSeq and when it went out; `None` until the loop reports it.
    sent: Option<(u32, Instant)>,
}

#[derive(Debug, Clone)]
struct Subscription {
    call_id: String,
    /// Our `From` tag: a NOTIFY's `To` tag must match it (FR-013).
    local_tag: String,
    /// The network's tag, from the first 2xx or NOTIFY, whichever came first.
    /// Never replaced once set.
    remote_tag: Option<String>,
    route_set: Vec<String>,
    remote_target: Option<String>,
    requested_expires: u32,
    created_at: Instant,
    created_wall: SystemTime,
    granted: Option<Duration>,
    expires_at: Option<Instant>,
    confirmed: bool,
    /// Timer N's deadline, until the first NOTIFY arrives.
    timer_n: Option<Instant>,
    /// A NOTIFY arrived since the last SUBSCRIBE in this dialog went out: the
    /// network has it, so Timer F no longer says anything about the path.
    notified_since_send: bool,
    /// The expiry the last scheduled refresh was sent against. A refresh is
    /// due once per expiry: a NOTIFY that does not move it must not make the
    /// same refresh go out again.
    refreshed_for: Option<Instant>,
    last_cseq: u32,
    txn: Txn,
}

impl Subscription {
    /// No SUBSCRIBE is outstanding that the network has not acknowledged. A
    /// NOTIFY since the last send proves the network has it, so a refresh need
    /// not wait for a final response that may never come; a late one then
    /// matches an older CSeq and is ignored.
    fn settled(&self) -> bool {
        self.txn == Txn::Idle || self.notified_since_send
    }

    fn refresh_due(&self, now: Instant) -> bool {
        if self.expires_at.is_some() && self.refreshed_for == self.expires_at {
            return false;
        }
        match (self.expires_at, self.granted) {
            (Some(expires_at), Some(granted)) => {
                now + crate::ims::renewal_headroom_for(granted) >= expires_at
            }
            _ => false,
        }
    }

    /// Takes the dialog's identity from the first 2xx or NOTIFY to carry one.
    fn learn_dialog(
        &mut self,
        remote_tag: Option<&str>,
        route_set: Vec<String>,
        target: Option<String>,
    ) {
        let Some(tag) = remote_tag else {
            return;
        };
        match &self.remote_tag {
            None => {
                self.remote_tag = Some(tag.to_string());
                self.route_set = route_set;
                self.remote_target = target;
            }
            Some(known) if known != tag => tracing::info!(
                call_id = %self.call_id,
                known_tag = %known,
                ignored_tag = %tag,
                "reg-event subscription already has a network tag; keeping it"
            ),
            Some(_) => {}
        }
    }
}

/// A replaced subscription, remembered only to recognise its late messages.
#[derive(Debug, Clone)]
struct SupersededSub {
    call_id: String,
    local_tag: String,
    forget_at: Instant,
}

/// The dialog a refresh is sent in.
#[derive(Debug, Clone, Copy)]
pub(super) struct RefreshDialog<'a> {
    pub(super) call_id: &'a str,
    pub(super) local_tag: &'a str,
    pub(super) remote_tag: &'a str,
    pub(super) remote_target: Option<&'a str>,
    pub(super) route_set: &'a [String],
}

/// One line's reg-event subscription. See the module docs.
#[derive(Debug)]
pub(super) struct RegSubscription {
    current: Option<Subscription>,
    superseded: VecDeque<SupersededSub>,
    /// When the next SUBSCRIBE goes out. One slot, so a failure can never
    /// queue more than one (SC-006).
    retry_at: Option<Instant>,
    backoff: Duration,
    /// The Expires to request: the default, raised by a `423` for the rest of
    /// this registration.
    min_expires: u32,
    /// Set by `489`, or `terminated` with `rejected`/`noresource`: nothing is
    /// sent until the next registration.
    stopped: Option<String>,
    probing: Option<Probing>,
    last_failure: Option<(SystemTime, String)>,
    /// When the subscription stopped being confirmed, for status.
    unconfirmed_since: Option<SystemTime>,
    /// When the last SUBSCRIBE, initial or refresh, went out: even an "at
    /// once" resend waits [`SUB_INITIAL_BACKOFF`] after it (SC-006).
    last_sent_at: Option<Instant>,
}

impl Default for RegSubscription {
    fn default() -> Self {
        Self::new()
    }
}

impl RegSubscription {
    pub(super) fn new() -> Self {
        Self {
            current: None,
            superseded: VecDeque::new(),
            retry_at: None,
            backoff: SUB_INITIAL_BACKOFF,
            min_expires: crate::ims::DEFAULT_EXPIRES,
            stopped: None,
            probing: None,
            last_failure: None,
            unconfirmed_since: None,
            last_sent_at: None,
        }
    }

    /// A registration, initial or renewal, succeeded. Everything belonging to
    /// the previous subscription is dropped, so none of its timers can fire
    /// against the new one (FR-003, research R12).
    pub(super) fn on_new_registration(&mut self, now: Instant) -> SubAction {
        self.supersede(now);
        self.probing = None;
        self.retry_at = None;
        self.stopped = None;
        self.min_expires = crate::ims::DEFAULT_EXPIRES;
        self.backoff = SUB_INITIAL_BACKOFF;
        self.last_sent_at = None;
        SubAction::SendInitial {
            expires: self.min_expires,
        }
    }

    /// The loop sent the SUBSCRIBE an action asked for.
    pub(super) fn on_sent(
        &mut self,
        sent: &crate::ims::session::SentSubscribe,
        kind: SubKind,
        now: Instant,
    ) {
        let txn = Txn::Pending {
            cseq: sent.cseq,
            sent_at: now,
            kind,
        };
        self.last_sent_at = Some(now);
        match kind {
            SubKind::Initial => {
                self.supersede(now);
                self.current = Some(Subscription {
                    call_id: sent.call_id.clone(),
                    local_tag: sent.local_tag.clone(),
                    remote_tag: None,
                    route_set: Vec::new(),
                    remote_target: None,
                    requested_expires: sent.expires,
                    created_at: now,
                    created_wall: SystemTime::now(),
                    granted: None,
                    expires_at: None,
                    confirmed: false,
                    timer_n: Some(now + TIMER_N),
                    notified_since_send: false,
                    refreshed_for: None,
                    last_cseq: sent.cseq,
                    txn,
                });
            }
            SubKind::Refresh => {
                if let Some(c) = &mut self.current {
                    c.refreshed_for = c.expires_at;
                    c.requested_expires = sent.expires;
                    c.notified_since_send = false;
                    c.last_cseq = sent.cseq;
                    c.txn = txn;
                }
            }
        }
    }

    /// The SUBSCRIBE could not be sent. Like an unanswered one, that is
    /// evidence about the Gm path only once a probe confirms it (FR-011).
    pub(super) fn on_send_failed(
        &mut self,
        kind: SubKind,
        now: Instant,
        force_renewal_pending: bool,
    ) -> SubAction {
        let call_id = match kind {
            SubKind::Refresh => self.current.as_ref().map(|c| c.call_id.clone()),
            SubKind::Initial => None,
        };
        if kind == SubKind::Refresh {
            self.supersede(now);
        }
        self.begin_probe(
            EndpointFailureCause::SubscribeSendFailed,
            call_id,
            now,
            force_renewal_pending,
        )
    }

    /// The confirming probe went out with `cseq`.
    pub(super) fn on_probe_sent(&mut self, cseq: u32, now: Instant) {
        if let Some(p) = &mut self.probing {
            p.sent = Some((cseq, now));
        }
    }

    /// The confirming probe could not be sent: the path is down.
    pub(super) fn on_probe_send_failed(&mut self, now: Instant) -> Option<SubAction> {
        let p = self.probing.take()?;
        self.schedule_after_endpoint_failure(now);
        Some(SubAction::EndpointFailure {
            cause: p.cause,
            call_id: p.call_id,
        })
    }

    /// Offered every `OPTIONS` response before the keepalive sees it. `true`
    /// when it answered the confirming probe: the path is alive, so only the
    /// subscription failed, and it is retried on backoff (FR-011b).
    pub(super) fn on_probe_response(&mut self, cseq: u32, now: Instant) -> bool {
        let Some(p) = self
            .probing
            .take_if(|p| p.sent.is_some_and(|(sent, _)| sent == cseq))
        else {
            return false;
        };
        let reason = match p.cause {
            EndpointFailureCause::SubscribeTimeout => {
                "SUBSCRIBE unanswered within 128s (Gm path alive)"
            }
            EndpointFailureCause::SubscribeSendFailed => {
                "SUBSCRIBE could not be sent (Gm path alive)"
            }
        };
        let retry_in = self.fail(now, reason, None);
        tracing::warn!(
            call_id = %p.call_id.as_deref().unwrap_or(""),
            retry_in_secs = retry_in.as_secs(),
            "Gm path answered; reg-event subscription failed, retrying on backoff"
        );
        true
    }

    /// A response whose CSeq method is `SUBSCRIBE` (research R3).
    pub(super) fn on_response(&mut self, resp: &SipResponse, now: Instant) -> ResponseMatch {
        let call_id = resp.header("Call-ID").unwrap_or_default();
        let cseq = resp.header("CSeq").and_then(parse_cseq_number);
        let pending = self.current.as_ref().and_then(|c| match c.txn {
            Txn::Pending { cseq: pc, kind, .. } if c.call_id == call_id && Some(pc) == cseq => {
                Some(kind)
            }
            _ => None,
        });
        if let Some(kind) = pending {
            self.on_current_response(resp, kind, now);
            return ResponseMatch::Current;
        }
        let ours = self.current.as_ref().is_some_and(|c| c.call_id == call_id)
            || self.superseded.iter().any(|s| s.call_id == call_id);
        let cseq = cseq.map(|n| n.to_string()).unwrap_or_default();
        if ours {
            tracing::info!(
                call_id = %call_id,
                cseq = %cseq,
                status = resp.status,
                "response to a superseded reg-event SUBSCRIBE; ignored"
            );
            ResponseMatch::Superseded
        } else {
            tracing::info!(
                call_id = %call_id,
                cseq = %cseq,
                status = resp.status,
                "response to an unknown reg-event SUBSCRIBE; ignored"
            );
            ResponseMatch::Unknown
        }
    }

    fn on_current_response(&mut self, resp: &SipResponse, kind: SubKind, now: Instant) {
        if resp.status < 200 {
            // RFC 3261 §17.1.2.2: a provisional response does not stop Timer F.
            tracing::debug!(
                status = resp.status,
                "provisional response to reg-event SUBSCRIBE"
            );
            return;
        }
        let c = self.current.as_mut().expect("on_response matched current");
        c.txn = Txn::Idle;
        let call_id = c.call_id.clone();
        let cseq = c.last_cseq;
        let refused = format!("SUBSCRIBE refused: {} {}", resp.status, resp.reason);
        match resp.status {
            200..=299 => {
                let granted = resp
                    .header("Expires")
                    .and_then(|v| v.trim().parse::<u32>().ok())
                    .unwrap_or(c.requested_expires);
                if granted == 0 {
                    // Accepted and ended in one breath: treat it as a refusal,
                    // or a network that keeps doing this is resubscribed to
                    // on every tick.
                    self.supersede(now);
                    let retry_in = self.fail(now, "SUBSCRIBE accepted with Expires: 0", None);
                    tracing::warn!(
                        call_id = %call_id,
                        status = resp.status,
                        reason = %resp.reason,
                        retry_in_secs = retry_in.as_secs(),
                        kind = kind.as_str(),
                        "reg-event SUBSCRIBE refused; retrying on backoff"
                    );
                    return;
                }
                let c = self.current.as_mut().expect("on_response matched current");
                c.granted = Some(Duration::from_secs(granted.into()));
                c.expires_at = Some(after(now, Duration::from_secs(granted.into())));
                let to_tag = resp.header("To").and_then(header_tag);
                let target = resp.header("Contact").and_then(contact_uri);
                c.learn_dialog(to_tag, uac_route_set(resp, &[]), target);
                self.retry_at = None;
                tracing::info!(
                    call_id = %call_id,
                    cseq,
                    status = resp.status,
                    granted_expires = granted,
                    kind = kind.as_str(),
                    "reg-event SUBSCRIBE accepted"
                );
            }
            // A `Min-Expires` that does not raise what we ask for is no
            // progress: resending would draw the same 423, so it is an
            // ordinary refusal below.
            423 if resp
                .header("Min-Expires")
                .and_then(|v| v.trim().parse::<u32>().ok())
                .is_some_and(|min| min > self.min_expires) =>
            {
                let min: u32 = resp
                    .header("Min-Expires")
                    .and_then(|v| v.trim().parse().ok())
                    .expect("guarded above");
                self.min_expires = min;
                tracing::info!(
                    call_id = %call_id,
                    min_expires = self.min_expires,
                    "reg-event SUBSCRIBE interval too brief; resending"
                );
                if kind == SubKind::Initial {
                    self.supersede(now);
                }
                self.retry_at = Some(now);
            }
            489 => {
                self.supersede(now);
                self.retry_at = None;
                self.record_failure(refused);
                self.stopped = Some(format!("{} {}", resp.status, resp.reason));
                tracing::warn!(
                    call_id = %call_id,
                    "the network does not support the reg event package; not subscribing again for this registration"
                );
            }
            481 if kind == SubKind::Refresh => {
                // The notifier no longer knows the dialog: start a new one now.
                self.supersede(now);
                self.record_failure(refused);
                self.unconfirmed_since.get_or_insert_with(SystemTime::now);
                self.retry_at = Some(now);
                tracing::warn!(
                    call_id = %call_id,
                    status = resp.status,
                    reason = %resp.reason,
                    retry_in_secs = 0,
                    kind = kind.as_str(),
                    "reg-event SUBSCRIBE refused; retrying on backoff"
                );
            }
            status => {
                // A refused initial SUBSCRIBE leaves no dialog (RFC 6665
                // §4.1.2.1), so each retry is a new one. A refused refresh
                // leaves the subscription valid until it expires (research R8),
                // except a 408, which says the dialog is gone.
                if kind == SubKind::Initial || status == 408 {
                    self.supersede(now);
                }
                let floor = resp.header("Retry-After").and_then(retry_after_secs);
                let retry_in = self.fail(now, &refused, floor.map(Duration::from_secs));
                tracing::warn!(
                    call_id = %call_id,
                    status,
                    reason = %resp.reason,
                    retry_in_secs = retry_in.as_secs(),
                    kind = kind.as_str(),
                    "reg-event SUBSCRIBE refused; retrying on backoff"
                );
            }
        }
    }

    /// Matches a NOTIFY to a subscription before it is answered (FR-013). A
    /// NOTIFY in the current dialog confirms the subscription, satisfies both
    /// Timer F and Timer N (research R4), and supplies the dialog's identity
    /// if the 2xx has not.
    pub(super) fn on_notify(&mut self, req: &SipRequest, now: Instant) -> NotifyMatch {
        self.prune(now);
        let event = req
            .header("Event")
            .or_else(|| req.header("o"))
            .and_then(|v| v.split(';').next())
            .map(str::trim)
            .unwrap_or("");
        if !event.eq_ignore_ascii_case("reg") {
            return NotifyMatch::Unknown;
        }
        let call_id = req.header("Call-ID").unwrap_or_default();
        let Some(to_tag) = req.header("To").and_then(header_tag) else {
            return NotifyMatch::Unknown;
        };
        let state = parse_subscription_state(req.header("Subscription-State").unwrap_or(""));
        if let Some(c) = self
            .current
            .as_mut()
            .filter(|c| c.call_id == call_id && c.local_tag == to_tag)
        {
            let from_tag = req.header("From").and_then(header_tag);
            let target = req.header("Contact").and_then(contact_uri);
            c.learn_dialog(
                from_tag,
                route_set_in_order(&req.headers_all("Record-Route")),
                target,
            );
            c.notified_since_send = true;
            c.timer_n = None;
            if !c.confirmed && state.state != SubStateKind::Terminated {
                c.confirmed = true;
                let expires_in = state
                    .expires
                    .or_else(|| c.granted.map(|g| g.as_secs()))
                    .unwrap_or(c.requested_expires.into());
                tracing::info!(
                    call_id = %c.call_id,
                    expires_in_secs = expires_in,
                    "reg-event subscription confirmed"
                );
                self.backoff = SUB_INITIAL_BACKOFF;
                self.unconfirmed_since = None;
            }
            return NotifyMatch::Current { state };
        }
        if self
            .superseded
            .iter()
            .any(|s| s.call_id == call_id && s.local_tag == to_tag)
        {
            return NotifyMatch::Superseded;
        }
        NotifyMatch::Unknown
    }

    /// Applies a current NOTIFY's `Subscription-State` (FR-017, FR-018,
    /// research R7). Runs after spec 089's evaluation of the body.
    pub(super) fn apply_subscription_state(&mut self, state: &SubState, now: Instant) {
        let Some(c) = self.current.as_mut() else {
            return;
        };
        let call_id = c.call_id.clone();
        match state.state {
            SubStateKind::Missing => tracing::warn!(
                call_id = %call_id,
                "NOTIFY without Subscription-State; treating as active"
            ),
            SubStateKind::Active | SubStateKind::Pending => {
                if let Some(e) = state.expires {
                    c.granted = Some(Duration::from_secs(e));
                    c.expires_at = Some(after(now, Duration::from_secs(e)));
                }
            }
            SubStateKind::Terminated => {
                self.supersede(now);
                let reason = state.reason.as_deref().unwrap_or("");
                self.record_failure(format!(
                    "terminated by the network: {}",
                    if reason.is_empty() {
                        "no reason"
                    } else {
                        reason
                    }
                ));
                self.unconfirmed_since.get_or_insert_with(SystemTime::now);
                let retry_after = state.retry_after.map(Duration::from_secs);
                let next = match reason {
                    "deactivated" | "timeout" => {
                        self.retry_at = Some(now);
                        "now"
                    }
                    "rejected" | "noresource" => {
                        self.retry_at = None;
                        self.stopped = Some(format!("terminated: {reason}"));
                        "next-registration"
                    }
                    "probation" | "giveup" if retry_after.is_some() => {
                        self.schedule_retry(now, retry_after);
                        "after-retry-after"
                    }
                    _ => {
                        self.schedule_retry(now, retry_after);
                        "backoff"
                    }
                };
                tracing::warn!(
                    call_id = %call_id,
                    reason = %reason,
                    retry_after_secs = state.retry_after,
                    next,
                    retry_in_secs = self
                        .retry_at
                        .map(|t| t.saturating_duration_since(now).as_secs()),
                    "reg-event subscription terminated by the network"
                );
            }
        }
    }

    /// Runs every idle tick, before the busy gate (research R4): the timers,
    /// the refresh, and any scheduled retry. `force_renewal_pending` makes an
    /// unanswered SUBSCRIBE skip the probe, since a re-registration is
    /// already on its way (edge case: no stacking).
    pub(super) fn poll(&mut self, now: Instant, force_renewal_pending: bool) -> Option<SubAction> {
        self.prune(now);
        if let Some(p) = &self.probing {
            let timed_out = p
                .sent
                .is_some_and(|(_, at)| now >= at + PING_RESPONSE_TIMEOUT);
            if !timed_out {
                return None;
            }
            let p = self.probing.take().expect("checked Some above");
            self.schedule_after_endpoint_failure(now);
            return Some(SubAction::EndpointFailure {
                cause: p.cause,
                call_id: p.call_id,
            });
        }
        if self.stopped.is_some() {
            return None;
        }
        if let Some(c) = self.current.as_mut() {
            if let Txn::Pending { sent_at, .. } = c.txn {
                if c.notified_since_send && now >= sent_at + TIMER_F {
                    // A NOTIFY proved the network has this SUBSCRIBE, but its
                    // final response never came. End the transaction quietly,
                    // so the confirmed dialog's refresh schedule still runs.
                    c.txn = Txn::Idle;
                }
            }
        }
        let current = self.current.as_ref().map(|c| {
            let timer_f = matches!(c.txn, Txn::Pending { sent_at, .. }
                if !c.notified_since_send && now >= sent_at + TIMER_F);
            (
                c.call_id.clone(),
                timer_f,
                c.expires_at.is_some_and(|e| now >= e),
                !c.confirmed && c.timer_n.is_some_and(|t| now >= t),
                c.settled() && c.refresh_due(now),
                c.remote_tag.is_some(),
            )
        });
        if let Some((call_id, timer_f, expired, timer_n, refresh_due, has_dialog)) = current {
            if timer_f {
                self.supersede(now);
                return Some(self.begin_probe(
                    EndpointFailureCause::SubscribeTimeout,
                    Some(call_id),
                    now,
                    force_renewal_pending,
                ));
            }
            if expired {
                self.supersede(now);
                self.record_failure("expired without a successful refresh".to_string());
                self.unconfirmed_since.get_or_insert_with(SystemTime::now);
                // Never sooner than a retry the network already asked us to
                // hold off for (a refused refresh's `Retry-After`).
                self.retry_at = Some(self.retry_at.map_or(now, |t| t.max(now)));
                tracing::warn!(
                    call_id = %call_id,
                    "reg-event subscription expired without a successful refresh; resubscribing"
                );
            } else if timer_n {
                self.supersede(now);
                let retry_in = self.fail(now, "accepted but no NOTIFY within 128s", None);
                tracing::warn!(
                    call_id = %call_id,
                    retry_in_secs = retry_in.as_secs(),
                    "reg-event subscription accepted but never confirmed; retrying on backoff"
                );
                return None;
            } else if refresh_due && self.retry_at.is_none() && self.spaced(now) {
                if has_dialog {
                    return Some(SubAction::SendRefresh {
                        expires: self.min_expires,
                    });
                }
                // No dialog to refresh in: start a new subscription instead.
                self.supersede(now);
                self.retry_at = Some(now);
            }
        }
        let due = self.retry_at.is_some_and(|t| now >= t);
        if !due || !self.spaced(now) {
            return None;
        }
        match &self.current {
            Some(c) if c.settled() && c.remote_tag.is_some() => {
                self.retry_at = None;
                Some(SubAction::SendRefresh {
                    expires: self.min_expires,
                })
            }
            Some(_) => None,
            None => {
                self.retry_at = None;
                Some(SubAction::SendInitial {
                    expires: self.min_expires,
                })
            }
        }
    }

    /// At least [`SUB_INITIAL_BACKOFF`] since the last SUBSCRIBE (SC-006).
    fn spaced(&self, now: Instant) -> bool {
        self.last_sent_at
            .is_none_or(|last| now >= last + SUB_INITIAL_BACKOFF)
    }

    /// The dialog a [`SubAction::SendRefresh`] is sent in.
    pub(super) fn refresh_dialog(&self) -> Option<RefreshDialog<'_>> {
        let c = self.current.as_ref()?;
        Some(RefreshDialog {
            call_id: &c.call_id,
            local_tag: &c.local_tag,
            remote_tag: c.remote_tag.as_deref()?,
            remote_target: c.remote_target.as_deref(),
            route_set: &c.route_set,
        })
    }

    /// The current subscription's Call-ID and our tag, for logs.
    pub(super) fn current_ids(&self) -> Option<(&str, &str)> {
        self.current
            .as_ref()
            .map(|c| (c.call_id.as_str(), c.local_tag.as_str()))
    }

    pub(super) fn last_failure(&self) -> Option<&(SystemTime, String)> {
        self.last_failure.as_ref()
    }

    /// For status (FR-020). `now`/`now_wall` convert the monotonic deadlines.
    pub(super) fn health(&self, now: Instant, now_wall: SystemTime) -> SubscriptionHealth {
        let wall = |t: Instant| match t.checked_duration_since(now) {
            Some(ahead) => now_wall + ahead,
            None => now_wall - now.duration_since(t),
        };
        if let Some(reason) = &self.stopped {
            return SubscriptionHealth::Stopped {
                reason: reason.clone(),
            };
        }
        match &self.current {
            Some(c) if c.confirmed => SubscriptionHealth::Confirmed {
                expires_at: c.expires_at.map(wall),
            },
            Some(c) => SubscriptionHealth::Pending {
                since: c.created_wall,
            },
            None if self.probing.is_some()
                || self.retry_at.is_some()
                || self.unconfirmed_since.is_some() =>
            {
                SubscriptionHealth::Unconfirmed {
                    since: self.unconfirmed_since.unwrap_or(now_wall),
                    retry_at: self.retry_at.map(wall),
                }
            }
            None => SubscriptionHealth::None,
        }
    }

    fn begin_probe(
        &mut self,
        cause: EndpointFailureCause,
        call_id: Option<String>,
        now: Instant,
        force_renewal_pending: bool,
    ) -> SubAction {
        if force_renewal_pending {
            // A re-registration is already coming and will replace this
            // subscription; probing would only confirm what it already knows.
            let reason = match cause {
                EndpointFailureCause::SubscribeTimeout => "SUBSCRIBE unanswered within 128s",
                EndpointFailureCause::SubscribeSendFailed => "SUBSCRIBE could not be sent",
            };
            self.fail(now, reason, None);
            return SubAction::EndpointFailure { cause, call_id };
        }
        self.probing = Some(Probing {
            cause,
            call_id: call_id.clone(),
            sent: None,
        });
        SubAction::Probe { cause, call_id }
    }

    /// Records a subscription failure and schedules the retry. Returns the
    /// delay, for the log line.
    fn fail(&mut self, now: Instant, reason: &str, floor: Option<Duration>) -> Duration {
        self.record_failure(reason.to_string());
        self.unconfirmed_since.get_or_insert_with(SystemTime::now);
        self.schedule_retry(now, floor)
    }

    /// The next SUBSCRIBE on the backoff, never before `floor` (a network
    /// `Retry-After` is a floor, not a ceiling: FR-009a).
    fn schedule_retry(&mut self, now: Instant, floor: Option<Duration>) -> Duration {
        let delay = self.backoff.max(floor.unwrap_or_default());
        self.retry_at = Some(after(now, delay));
        self.backoff = next_backoff(self.backoff, SUB_MAX_BACKOFF);
        delay
    }

    /// After a qualifying endpoint failure the re-registration makes a new
    /// subscription; this retry only matters if that renewal keeps failing
    /// and the old session stays in use.
    fn schedule_after_endpoint_failure(&mut self, now: Instant) {
        self.unconfirmed_since.get_or_insert_with(SystemTime::now);
        self.schedule_retry(now, None);
    }

    fn record_failure(&mut self, reason: String) {
        self.last_failure = Some((SystemTime::now(), reason));
    }

    /// Moves the current subscription to the superseded memory, until it can
    /// no longer be notified: its granted expiry, or the requested one if none
    /// was granted, plus [`SUPERSEDED_GRACE`] (clarification Q4).
    fn supersede(&mut self, now: Instant) {
        let Some(c) = self.current.take() else {
            return;
        };
        let expiry = c.expires_at.unwrap_or_else(|| {
            after(
                c.created_at,
                Duration::from_secs(c.requested_expires.into()),
            )
        });
        self.superseded.push_back(SupersededSub {
            call_id: c.call_id,
            local_tag: c.local_tag,
            forget_at: expiry + SUPERSEDED_GRACE,
        });
        while self.superseded.len() > SUPERSEDED_CAP {
            self.superseded.pop_front();
        }
        self.prune(now);
    }

    fn prune(&mut self, now: Instant) {
        self.superseded.retain(|s| now < s.forget_at);
    }
}

/// A `Retry-After` header's delay in seconds (RFC 3261 §20.33): the leading
/// integer, ignoring any comment or parameters. `None` for anything else.
fn retry_after_secs(value: &str) -> Option<u64> {
    let value = value.trim();
    let end = value
        .find(|ch: char| !ch.is_ascii_digit())
        .unwrap_or(value.len());
    value[..end]
        .parse::<u64>()
        .ok()
        .map(|v| v.min(MAX_NETWORK_DELAY_SECS))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::ims::session::SentSubscribe;

    const IMPU: &str = "sip:+919000000000@ims.mnc000.mcc000.3gppnetwork.org";

    fn secs(n: u64) -> Duration {
        Duration::from_secs(n)
    }

    fn sent(call_id: &str, local_tag: &str, cseq: u32, expires: u32) -> SentSubscribe {
        SentSubscribe {
            call_id: call_id.to_string(),
            local_tag: local_tag.to_string(),
            cseq,
            expires,
        }
    }

    fn resp(
        status: u16,
        reason: &str,
        call_id: &str,
        cseq: u32,
        extra: &[(&str, &str)],
    ) -> SipResponse {
        let mut headers = vec![
            ("Call-ID".to_string(), call_id.to_string()),
            ("CSeq".to_string(), format!("{cseq} SUBSCRIBE")),
            ("To".to_string(), format!("<{IMPU}>;tag=rtag-1")),
        ];
        for (k, v) in extra {
            headers.retain(|(h, _)| !h.eq_ignore_ascii_case(k));
            headers.push((k.to_string(), v.to_string()));
        }
        SipResponse {
            status,
            reason: reason.to_string(),
            headers,
            body: String::new(),
        }
    }

    fn ok(call_id: &str, cseq: u32, expires: u32) -> SipResponse {
        resp(
            200,
            "OK",
            call_id,
            cseq,
            &[("Expires", &expires.to_string())],
        )
    }

    fn notify(call_id: &str, to_tag: &str, sub_state: Option<&str>, extra: &str) -> SipRequest {
        let sub_state = sub_state
            .map(|v| format!("Subscription-State: {v}\r\n"))
            .unwrap_or_default();
        let raw = format!(
            "NOTIFY sip:+919000000000@10.0.0.1:5070 SIP/2.0\r\n\
             Via: SIP/2.0/UDP 10.0.0.8:5060;branch=z9hG4bKn1\r\n\
             From: <{IMPU}>;tag=rtag-1\r\n\
             To: <{IMPU}>;tag={to_tag}\r\n\
             Call-ID: {call_id}\r\n\
             CSeq: 1 NOTIFY\r\n\
             Event: reg\r\n\
             {sub_state}{extra}Content-Length: 0\r\n\r\n"
        );
        SipRequest::try_parse(raw.as_bytes()).unwrap().unwrap().0
    }

    /// A subscription to `sub-a` (our tag `ltag-a`, CSeq 7) sent at `t0`.
    fn started(t0: Instant) -> RegSubscription {
        let mut sub = RegSubscription::new();
        assert_eq!(
            sub.on_new_registration(t0),
            SubAction::SendInitial { expires: 3600 }
        );
        sub.on_sent(&sent("sub-a", "ltag-a", 7, 3600), SubKind::Initial, t0);
        sub
    }

    /// `started`, accepted with `granted` and confirmed by a NOTIFY, both at `t0`.
    fn confirmed(t0: Instant, granted: u32) -> RegSubscription {
        let mut sub = started(t0);
        assert_eq!(
            sub.on_response(&ok("sub-a", 7, granted), t0),
            ResponseMatch::Current
        );
        let state = format!("active;expires={granted}");
        assert!(matches!(
            sub.on_notify(&notify("sub-a", "ltag-a", Some(&state), ""), t0),
            NotifyMatch::Current { .. }
        ));
        sub
    }

    fn failure(sub: &RegSubscription) -> Option<&str> {
        sub.last_failure().map(|(_, r)| r.as_str())
    }

    // ---- Foundational (T006) ----

    #[test]
    fn a_new_registration_asks_for_an_initial_subscribe_and_tracks_what_was_sent() {
        let t0 = Instant::now();
        let sub = started(t0);
        assert_eq!(sub.current_ids(), Some(("sub-a", "ltag-a")));
        let c = sub.current.as_ref().unwrap();
        assert_eq!(
            c.txn,
            Txn::Pending {
                cseq: 7,
                sent_at: t0,
                kind: SubKind::Initial
            }
        );
        assert!(matches!(
            sub.health(t0, SystemTime::now()),
            SubscriptionHealth::Pending { .. }
        ));
    }

    // ---- US1: response correlation (T008) ----

    #[test]
    fn a_2xx_for_the_current_transaction_is_accepted_with_its_granted_expiry() {
        let t0 = Instant::now();
        let mut sub = started(t0);
        assert_eq!(
            sub.on_response(&ok("sub-a", 7, 600), t0 + secs(1)),
            ResponseMatch::Current
        );
        let c = sub.current.as_ref().unwrap();
        assert_eq!(c.txn, Txn::Idle);
        assert_eq!(c.granted, Some(secs(600)));
        assert_eq!(c.expires_at, Some(t0 + secs(601)));
        assert_eq!(c.remote_tag.as_deref(), Some("rtag-1"));
    }

    #[test]
    fn a_response_for_a_superseded_subscription_changes_nothing() {
        let t0 = Instant::now();
        let mut sub = RegSubscription::new();
        sub.on_new_registration(t0);
        sub.on_sent(&sent("sub-old", "ltag-old", 3, 3600), SubKind::Initial, t0);
        sub.on_new_registration(t0 + secs(10));
        sub.on_sent(
            &sent("sub-a", "ltag-a", 7, 3600),
            SubKind::Initial,
            t0 + secs(10),
        );

        assert_eq!(
            sub.on_response(&ok("sub-old", 3, 3600), t0 + secs(11)),
            ResponseMatch::Superseded
        );
        let c = sub.current.as_ref().unwrap();
        assert_eq!(c.call_id, "sub-a");
        assert!(matches!(c.txn, Txn::Pending { .. }));
        assert_eq!(c.remote_tag, None);
    }

    #[test]
    fn a_response_for_no_subscription_of_ours_is_unknown() {
        let t0 = Instant::now();
        let mut sub = started(t0);
        assert_eq!(
            sub.on_response(&ok("stray", 7, 3600), t0),
            ResponseMatch::Unknown
        );
        assert!(matches!(
            sub.current.as_ref().unwrap().txn,
            Txn::Pending { .. }
        ));
    }

    #[test]
    fn a_provisional_response_matches_but_timer_f_keeps_running() {
        let t0 = Instant::now();
        let mut sub = started(t0);
        assert_eq!(
            sub.on_response(&resp(100, "Trying", "sub-a", 7, &[]), t0 + secs(1)),
            ResponseMatch::Current
        );
        assert!(matches!(
            sub.poll(t0 + TIMER_F, false),
            Some(SubAction::Probe { .. })
        ));
    }

    #[test]
    fn an_older_cseq_of_the_current_subscription_is_superseded() {
        let t0 = Instant::now();
        let mut sub = started(t0);
        assert_eq!(
            sub.on_response(&ok("sub-a", 6, 3600), t0),
            ResponseMatch::Superseded
        );
        assert!(matches!(
            sub.current.as_ref().unwrap().txn,
            Txn::Pending { .. }
        ));
    }

    // ---- US1: Timer F and the confirming probe (T011) ----

    #[test]
    fn timer_f_fires_at_128s_and_asks_for_a_probe() {
        let t0 = Instant::now();
        let mut sub = started(t0);
        assert_eq!(sub.poll(t0 + secs(127), false), None);
        assert_eq!(
            sub.poll(t0 + secs(128), false),
            Some(SubAction::Probe {
                cause: EndpointFailureCause::SubscribeTimeout,
                call_id: Some("sub-a".to_string()),
            })
        );
        // Asked once; nothing more until the probe resolves.
        assert_eq!(sub.poll(t0 + secs(129), false), None);
    }

    /// US1 scenario 3: the path answers, so only the subscription failed.
    #[test]
    fn an_answered_probe_is_a_subscription_failure_retried_on_backoff() {
        let t0 = Instant::now();
        let mut sub = started(t0);
        sub.poll(t0 + TIMER_F, false);
        let t1 = t0 + TIMER_F;
        sub.on_probe_sent(40, t1);
        assert!(!sub.on_probe_response(41, t1 + secs(1)));
        assert!(sub.on_probe_response(40, t1 + secs(2)));
        assert_eq!(
            failure(&sub),
            Some("SUBSCRIBE unanswered within 128s (Gm path alive)")
        );
        assert_eq!(sub.retry_at, Some(t1 + secs(7)));
        assert_eq!(sub.poll(t1 + secs(6), false), None);
        assert_eq!(
            sub.poll(t1 + secs(7), false),
            Some(SubAction::SendInitial { expires: 3600 })
        );
    }

    /// US1 scenario 1.
    #[test]
    fn an_unanswered_probe_is_a_qualifying_endpoint_failure() {
        let t0 = Instant::now();
        let mut sub = started(t0);
        sub.poll(t0 + TIMER_F, false);
        let t1 = t0 + TIMER_F;
        sub.on_probe_sent(40, t1);
        assert_eq!(sub.poll(t1 + secs(9), false), None);
        assert_eq!(
            sub.poll(t1 + PING_RESPONSE_TIMEOUT, false),
            Some(SubAction::EndpointFailure {
                cause: EndpointFailureCause::SubscribeTimeout,
                call_id: Some("sub-a".to_string()),
            })
        );
        // A late answer to the probe no longer means anything.
        assert!(!sub.on_probe_response(40, t1 + secs(11)));
    }

    #[test]
    fn an_unsendable_probe_is_a_qualifying_endpoint_failure_at_once() {
        let t0 = Instant::now();
        let mut sub = started(t0);
        sub.poll(t0 + TIMER_F, false);
        assert_eq!(
            sub.on_probe_send_failed(t0 + TIMER_F),
            Some(SubAction::EndpointFailure {
                cause: EndpointFailureCause::SubscribeTimeout,
                call_id: Some("sub-a".to_string()),
            })
        );
    }

    /// US1 scenario 2.
    #[test]
    fn a_send_failure_is_probed_and_escalates_when_the_probe_goes_unanswered() {
        let t0 = Instant::now();
        let mut sub = RegSubscription::new();
        sub.on_new_registration(t0);
        assert_eq!(
            sub.on_send_failed(SubKind::Initial, t0, false),
            SubAction::Probe {
                cause: EndpointFailureCause::SubscribeSendFailed,
                call_id: None,
            }
        );
        sub.on_probe_sent(40, t0);
        assert_eq!(
            sub.poll(t0 + PING_RESPONSE_TIMEOUT, false),
            Some(SubAction::EndpointFailure {
                cause: EndpointFailureCause::SubscribeSendFailed,
                call_id: None,
            })
        );
    }

    #[test]
    fn a_send_failure_answered_by_the_probe_is_a_subscription_failure() {
        let t0 = Instant::now();
        let mut sub = RegSubscription::new();
        sub.on_new_registration(t0);
        sub.on_send_failed(SubKind::Initial, t0, false);
        sub.on_probe_sent(40, t0);
        assert!(sub.on_probe_response(40, t0 + secs(1)));
        assert_eq!(
            failure(&sub),
            Some("SUBSCRIBE could not be sent (Gm path alive)")
        );
    }

    /// US1 scenario 4.
    #[test]
    fn a_late_final_response_before_timer_f_stops_it() {
        let t0 = Instant::now();
        let mut sub = started(t0);
        sub.on_response(&ok("sub-a", 7, 3600), t0 + secs(100));
        sub.on_notify(
            &notify("sub-a", "ltag-a", Some("active;expires=3500"), ""),
            t0 + secs(101),
        );
        assert_eq!(sub.poll(t0 + TIMER_F, false), None);
        assert_eq!(failure(&sub), None);
    }

    /// Edge case: a re-registration is already pending, so nothing is stacked.
    #[test]
    fn timer_f_with_a_re_registration_pending_skips_the_probe() {
        let t0 = Instant::now();
        let mut sub = started(t0);
        assert_eq!(
            sub.poll(t0 + TIMER_F, true),
            Some(SubAction::EndpointFailure {
                cause: EndpointFailureCause::SubscribeTimeout,
                call_id: Some("sub-a".to_string()),
            })
        );
        assert!(sub.probing.is_none());
        assert_eq!(failure(&sub), Some("SUBSCRIBE unanswered within 128s"));
    }

    // ---- US2: NOTIFY in the current dialog (T015) ----

    #[test]
    fn a_notify_in_the_current_dialog_confirms_it_and_supplies_the_network_tag() {
        let t0 = Instant::now();
        let mut sub = started(t0);
        let m = sub.on_notify(
            &notify("sub-a", "ltag-a", Some("active;expires=600"), ""),
            t0,
        );
        assert!(matches!(m, NotifyMatch::Current { state } if state.state == SubStateKind::Active));
        let c = sub.current.as_ref().unwrap();
        assert!(c.confirmed);
        assert_eq!(c.remote_tag.as_deref(), Some("rtag-1"));
    }

    /// US2 scenario 2 and the "different tags" edge case.
    #[test]
    fn a_notify_before_the_2xx_creates_the_dialog_and_the_2xx_does_not_replace_it() {
        let t0 = Instant::now();
        let mut sub = started(t0);
        let extra = "Record-Route: <sip:a@10.0.0.8;lr>\r\nRecord-Route: <sip:b@10.0.0.9;lr>\r\n\
                     Contact: <sip:scscf@10.0.0.9:5060>\r\n";
        sub.on_notify(
            &notify("sub-a", "ltag-a", Some("active;expires=3600"), extra),
            t0 + secs(1),
        );
        let late = resp(
            200,
            "OK",
            "sub-a",
            7,
            &[("Expires", "3600"), ("To", &format!("<{IMPU}>;tag=rtag-2"))],
        );
        assert_eq!(sub.on_response(&late, t0 + secs(2)), ResponseMatch::Current);

        let d = sub.refresh_dialog().unwrap();
        assert_eq!(d.remote_tag, "rtag-1");
        assert_eq!(d.remote_target, Some("sip:scscf@10.0.0.9:5060"));
        assert_eq!(
            d.route_set,
            ["Route: <sip:a@10.0.0.8;lr>", "Route: <sip:b@10.0.0.9;lr>"]
        );
        assert_eq!(sub.poll(t0 + TIMER_F, false), None);
        assert_eq!(sub.poll(t0 + TIMER_N + secs(1), false), None);
        assert_eq!(failure(&sub), None);
    }

    #[test]
    fn a_2xx_first_takes_the_reversed_record_route_and_its_contact() {
        let t0 = Instant::now();
        let mut sub = started(t0);
        let r = resp(
            200,
            "OK",
            "sub-a",
            7,
            &[
                ("Expires", "3600"),
                ("Record-Route", "<sip:a@10.0.0.8;lr>, <sip:b@10.0.0.9;lr>"),
                ("Contact", "<sip:scscf@10.0.0.9:5060>"),
            ],
        );
        sub.on_response(&r, t0);
        let d = sub.refresh_dialog().unwrap();
        assert_eq!(d.remote_tag, "rtag-1");
        assert_eq!(d.remote_target, Some("sip:scscf@10.0.0.9:5060"));
        assert_eq!(
            d.route_set,
            ["Route: <sip:b@10.0.0.9;lr>", "Route: <sip:a@10.0.0.8;lr>"]
        );
    }

    #[test]
    fn a_notify_for_another_dialog_is_not_current() {
        let t0 = Instant::now();
        let mut sub = started(t0);
        assert_eq!(
            sub.on_notify(&notify("sub-a", "other-tag", Some("active"), ""), t0),
            NotifyMatch::Unknown
        );
        assert_eq!(
            sub.on_notify(&notify("stray", "ltag-a", Some("active"), ""), t0),
            NotifyMatch::Unknown
        );
        assert!(!sub.current.as_ref().unwrap().confirmed);
    }

    // ---- US2: Timer N (T017) ----

    /// US2 scenario 1.
    #[test]
    fn an_accepted_subscription_with_no_notify_fails_at_timer_n() {
        let t0 = Instant::now();
        let mut sub = started(t0);
        sub.on_response(&ok("sub-a", 7, 3600), t0 + secs(1));
        assert_eq!(sub.poll(t0 + secs(127), false), None);
        assert_eq!(sub.poll(t0 + TIMER_N, false), None);
        assert_eq!(failure(&sub), Some("accepted but no NOTIFY within 128s"));
        assert_eq!(sub.retry_at, Some(t0 + TIMER_N + secs(5)));
        assert!(sub.current.is_none());
        assert_eq!(
            sub.poll(t0 + TIMER_N + secs(5), false),
            Some(SubAction::SendInitial { expires: 3600 })
        );
    }

    /// Research R4: with neither a response nor a NOTIFY, Timer F wins and
    /// there is one decision, not two.
    #[test]
    fn timer_f_wins_over_timer_n_when_both_expire() {
        let t0 = Instant::now();
        let mut sub = started(t0);
        assert!(matches!(
            sub.poll(t0 + TIMER_F, false),
            Some(SubAction::Probe { .. })
        ));
        assert_eq!(sub.poll(t0 + TIMER_F + secs(1), false), None);
        assert_eq!(failure(&sub), None);
    }

    #[test]
    fn a_notify_inside_timer_n_satisfies_it() {
        let t0 = Instant::now();
        let mut sub = started(t0);
        sub.on_response(&ok("sub-a", 7, 3600), t0 + secs(1));
        sub.on_notify(
            &notify("sub-a", "ltag-a", Some("active;expires=3600"), ""),
            t0 + secs(60),
        );
        assert_eq!(sub.poll(t0 + TIMER_N, false), None);
        assert_eq!(failure(&sub), None);
    }

    // ---- US2: Subscription-State (T019) ----

    fn terminated(sub: &mut RegSubscription, value: &str, at: Instant) {
        let m = sub.on_notify(&notify("sub-a", "ltag-a", Some(value), ""), at);
        let NotifyMatch::Current { state } = m else {
            panic!("expected the current dialog, got {m:?}");
        };
        sub.apply_subscription_state(&state, at);
    }

    #[test]
    fn an_active_notify_moves_the_expiry() {
        let t0 = Instant::now();
        let mut sub = confirmed(t0, 3600);
        let t1 = t0 + secs(30);
        terminated(&mut sub, "active;expires=600", t1);
        assert_eq!(
            sub.current.as_ref().unwrap().expires_at,
            Some(t1 + secs(600))
        );
    }

    /// US2 scenario 4.
    #[test]
    fn deactivated_and_timeout_resubscribe_at_once() {
        for reason in ["deactivated", "timeout"] {
            let t0 = Instant::now();
            let mut sub = confirmed(t0, 3600);
            let t1 = t0 + secs(60);
            terminated(&mut sub, &format!("terminated;reason={reason}"), t1);
            assert!(sub.current.is_none(), "{reason}");
            assert_eq!(
                sub.poll(t1, false),
                Some(SubAction::SendInitial { expires: 3600 }),
                "{reason}"
            );
            assert_eq!(
                failure(&sub),
                Some(format!("terminated by the network: {reason}").as_str())
            );
        }
    }

    /// US2 scenario 5.
    #[test]
    fn probation_waits_for_the_networks_retry_after() {
        let t0 = Instant::now();
        let mut sub = confirmed(t0, 3600);
        let t1 = t0 + secs(60);
        terminated(&mut sub, "terminated;reason=probation;retry-after=300", t1);
        assert_eq!(sub.poll(t1 + secs(299), false), None);
        assert_eq!(
            sub.poll(t1 + secs(300), false),
            Some(SubAction::SendInitial { expires: 3600 })
        );
    }

    #[test]
    fn giveup_without_retry_after_and_no_reason_both_use_the_backoff() {
        for value in ["terminated;reason=giveup", "terminated"] {
            let t0 = Instant::now();
            let mut sub = confirmed(t0, 3600);
            let t1 = t0 + secs(60);
            terminated(&mut sub, value, t1);
            assert_eq!(sub.retry_at, Some(t1 + SUB_INITIAL_BACKOFF), "{value}");
        }
        let t0 = Instant::now();
        let mut sub = confirmed(t0, 3600);
        terminated(&mut sub, "terminated", t0);
        assert_eq!(failure(&sub), Some("terminated by the network: no reason"));
    }

    /// US2 scenario 6.
    #[test]
    fn noresource_and_rejected_stop_until_the_next_registration() {
        for reason in ["noresource", "rejected"] {
            let t0 = Instant::now();
            let mut sub = confirmed(t0, 3600);
            terminated(&mut sub, &format!("terminated;reason={reason}"), t0);
            for hour in 0..=24 {
                assert_eq!(sub.poll(t0 + secs(hour * 3600), false), None, "{reason}");
            }
            assert!(matches!(
                sub.health(t0, SystemTime::now()),
                SubscriptionHealth::Stopped { .. }
            ));
            assert_eq!(
                sub.on_new_registration(t0 + secs(90_000)),
                SubAction::SendInitial { expires: 3600 }
            );
        }
    }

    #[test]
    fn a_terminated_subscription_is_remembered_as_superseded() {
        let t0 = Instant::now();
        let mut sub = confirmed(t0, 3600);
        terminated(&mut sub, "terminated;reason=deactivated", t0);
        assert_eq!(
            sub.on_notify(
                &notify("sub-a", "ltag-a", Some("terminated"), ""),
                t0 + secs(1)
            ),
            NotifyMatch::Superseded
        );
    }

    // ---- US2: refresh (T021) ----

    #[test]
    fn a_refresh_goes_out_at_expiry_less_the_scaled_headroom() {
        let t0 = Instant::now();
        let mut sub = confirmed(t0, 600);
        assert_eq!(sub.poll(t0 + secs(299), false), None);
        assert_eq!(
            sub.poll(t0 + secs(300), false),
            Some(SubAction::SendRefresh { expires: 3600 })
        );
        let d = sub.refresh_dialog().unwrap();
        assert_eq!(
            (d.call_id, d.local_tag, d.remote_tag),
            ("sub-a", "ltag-a", "rtag-1")
        );

        let t1 = t0 + secs(300);
        sub.on_sent(&sent("sub-a", "ltag-a", 8, 3600), SubKind::Refresh, t1);
        assert_eq!(
            sub.on_response(&ok("sub-a", 8, 600), t1 + secs(1)),
            ResponseMatch::Current
        );
        assert_eq!(sub.poll(t1 + secs(300), false), None);
        assert_eq!(
            sub.poll(t1 + secs(301), false),
            Some(SubAction::SendRefresh { expires: 3600 })
        );
    }

    fn refreshing(t0: Instant) -> (RegSubscription, Instant) {
        let mut sub = confirmed(t0, 600);
        sub.poll(t0 + secs(300), false);
        let t1 = t0 + secs(300);
        sub.on_sent(&sent("sub-a", "ltag-a", 8, 3600), SubKind::Refresh, t1);
        (sub, t1)
    }

    #[test]
    fn a_refresh_refused_481_starts_a_new_subscription_at_once() {
        let t0 = Instant::now();
        let (mut sub, t1) = refreshing(t0);
        sub.on_response(
            &resp(481, "Call/Transaction Does Not Exist", "sub-a", 8, &[]),
            t1,
        );
        assert!(sub.current.is_none());
        // "At once", but never inside SUB_INITIAL_BACKOFF of the refresh it
        // replaces (SC-006).
        assert_eq!(sub.poll(t1, false), None);
        assert_eq!(
            sub.poll(t1 + SUB_INITIAL_BACKOFF, false),
            Some(SubAction::SendInitial { expires: 3600 })
        );
    }

    #[test]
    fn a_refresh_lost_to_408_or_timer_f_starts_a_new_subscription_on_backoff() {
        let t0 = Instant::now();
        let (mut sub, t1) = refreshing(t0);
        sub.on_response(&resp(408, "Request Timeout", "sub-a", 8, &[]), t1);
        assert!(sub.current.is_none());
        assert_eq!(sub.poll(t1 + secs(4), false), None);
        assert_eq!(
            sub.poll(t1 + secs(5), false),
            Some(SubAction::SendInitial { expires: 3600 })
        );

        let (mut sub, t1) = refreshing(t0);
        assert!(matches!(
            sub.poll(t1 + TIMER_F, false),
            Some(SubAction::Probe { .. })
        ));
        sub.on_probe_sent(40, t1 + TIMER_F);
        assert!(sub.on_probe_response(40, t1 + TIMER_F));
        assert!(sub.current.is_none());
        assert_eq!(sub.retry_at, Some(t1 + TIMER_F + secs(5)));
    }

    #[test]
    fn a_refresh_refused_otherwise_keeps_the_subscription_and_retries_the_refresh() {
        let t0 = Instant::now();
        let (mut sub, t1) = refreshing(t0);
        sub.on_response(&resp(500, "Server Internal Error", "sub-a", 8, &[]), t1);
        assert_eq!(sub.current_ids(), Some(("sub-a", "ltag-a")));
        assert_eq!(
            sub.poll(t1 + secs(5), false),
            Some(SubAction::SendRefresh { expires: 3600 })
        );
    }

    #[test]
    fn reaching_expiry_without_a_successful_refresh_resubscribes() {
        let t0 = Instant::now();
        let (mut sub, t1) = refreshing(t0);
        sub.on_response(&resp(500, "Server Internal Error", "sub-a", 8, &[]), t1);
        assert_eq!(
            sub.poll(t0 + secs(600), false),
            Some(SubAction::SendInitial { expires: 3600 })
        );
        assert_eq!(failure(&sub), Some("expired without a successful refresh"));
    }

    /// A 3600 s grant refreshes at 50 min, following the same IMS timing
    /// rule as registration renewal.
    #[test]
    fn a_full_hour_grant_refreshes_no_earlier_than_50_minutes() {
        let t0 = Instant::now();
        let mut sub = confirmed(t0, 3600);
        assert_eq!(sub.poll(t0 + secs(2999), false), None);
        assert!(matches!(
            sub.poll(t0 + secs(3000), false),
            Some(SubAction::SendRefresh { .. })
        ));
    }

    // ---- US2: renewal lifecycle (T023) ----

    /// US2 scenario 8.
    #[test]
    fn a_new_registration_discards_every_pending_timer() {
        let t0 = Instant::now();
        let mut sub = started(t0);
        sub.on_response(
            &resp(
                423,
                "Interval Too Brief",
                "sub-a",
                7,
                &[("Min-Expires", "7200")],
            ),
            t0,
        );
        assert_eq!(sub.min_expires, 7200);
        let t1 = t0 + secs(5);
        assert_eq!(
            sub.poll(t1, false),
            Some(SubAction::SendInitial { expires: 7200 })
        );
        sub.on_sent(&sent("sub-b", "ltag-b", 8, 7200), SubKind::Initial, t1);
        sub.retry_at = Some(t1 + secs(50));
        sub.backoff = secs(80);
        sub.probing = Some(Probing {
            cause: EndpointFailureCause::SubscribeTimeout,
            call_id: None,
            sent: Some((40, t1)),
        });

        let t2 = t1 + secs(10);
        assert_eq!(
            sub.on_new_registration(t2),
            SubAction::SendInitial { expires: 3600 }
        );
        assert!(sub.current.is_none() && sub.probing.is_none() && sub.retry_at.is_none());
        assert_eq!(sub.backoff, SUB_INITIAL_BACKOFF);
        sub.on_sent(&sent("sub-c", "ltag-c", 9, 3600), SubKind::Initial, t2);
        assert_eq!(sub.poll(t1 + TIMER_F, false), None);
        assert_eq!(sub.poll(t1 + secs(60), false), None);
    }

    // ---- US3: superseded memory (T025) ----

    #[test]
    fn a_superseded_subscription_is_remembered_until_its_expiry_plus_128s() {
        let t0 = Instant::now();
        let mut sub = confirmed(t0, 600);
        sub.on_new_registration(t0);
        let old = || notify("sub-a", "ltag-a", Some("active"), "");
        assert_eq!(
            sub.on_notify(&old(), t0 + secs(727)),
            NotifyMatch::Superseded
        );
        assert_eq!(sub.on_notify(&old(), t0 + secs(729)), NotifyMatch::Unknown);
    }

    #[test]
    fn without_a_grant_the_requested_expiry_bounds_the_memory() {
        let t0 = Instant::now();
        let mut sub = started(t0);
        sub.on_new_registration(t0);
        let old = || notify("sub-a", "ltag-a", Some("active"), "");
        assert_eq!(
            sub.on_notify(&old(), t0 + secs(3727)),
            NotifyMatch::Superseded
        );
        assert_eq!(sub.on_notify(&old(), t0 + secs(3729)), NotifyMatch::Unknown);
    }

    #[test]
    fn at_most_32_superseded_subscriptions_are_remembered_oldest_dropped_first() {
        let t0 = Instant::now();
        let mut sub = RegSubscription::new();
        for i in 0..=SUPERSEDED_CAP {
            sub.on_new_registration(t0);
            sub.on_sent(
                &sent(&format!("sub-{i}"), &format!("ltag-{i}"), 1, 3600),
                SubKind::Initial,
                t0,
            );
        }
        sub.on_new_registration(t0);
        assert_eq!(sub.superseded.len(), SUPERSEDED_CAP);
        let n = |i: usize| {
            notify(
                &format!("sub-{i}"),
                &format!("ltag-{i}"),
                Some("active"),
                "",
            )
        };
        assert_eq!(sub.on_notify(&n(0), t0), NotifyMatch::Unknown);
        assert_eq!(sub.on_notify(&n(1), t0), NotifyMatch::Superseded);
        assert_eq!(
            sub.on_notify(&n(SUPERSEDED_CAP), t0),
            NotifyMatch::Superseded
        );
    }

    #[test]
    fn a_notify_for_another_event_package_is_unknown() {
        let t0 = Instant::now();
        let mut sub = started(t0);
        let raw = format!(
            "NOTIFY sip:x SIP/2.0\r\nFrom: <{IMPU}>;tag=rtag-1\r\nTo: <{IMPU}>;tag=ltag-a\r\n\
             Call-ID: sub-a\r\nCSeq: 1 NOTIFY\r\nEvent: presence\r\nContent-Length: 0\r\n\r\n"
        );
        let req = SipRequest::try_parse(raw.as_bytes()).unwrap().unwrap().0;
        assert_eq!(sub.on_notify(&req, t0), NotifyMatch::Unknown);
    }

    // ---- US4: error responses (T031) ----

    /// US4 scenario 1.
    #[test]
    fn a_423_resends_with_min_expires_for_the_rest_of_the_registration() {
        let t0 = Instant::now();
        let mut sub = started(t0);
        let r = resp(
            423,
            "Interval Too Brief",
            "sub-a",
            7,
            &[("Min-Expires", "7200")],
        );
        assert_eq!(sub.on_response(&r, t0), ResponseMatch::Current);
        // "At once", but never within SUB_INITIAL_BACKOFF of the last one (SC-006).
        assert_eq!(sub.poll(t0 + secs(1), false), None);
        let t1 = t0 + secs(5);
        assert_eq!(
            sub.poll(t1, false),
            Some(SubAction::SendInitial { expires: 7200 })
        );
        sub.on_sent(&sent("sub-b", "ltag-b", 8, 7200), SubKind::Initial, t1);
        sub.on_response(&resp(500, "Server Internal Error", "sub-b", 8, &[]), t1);
        assert_eq!(
            sub.poll(t1 + secs(5), false),
            Some(SubAction::SendInitial { expires: 7200 })
        );
    }

    #[test]
    fn a_423_without_min_expires_is_an_ordinary_refusal() {
        let t0 = Instant::now();
        let mut sub = started(t0);
        sub.on_response(&resp(423, "Interval Too Brief", "sub-a", 7, &[]), t0);
        assert_eq!(sub.min_expires, 3600);
        assert_eq!(sub.retry_at, Some(t0 + secs(5)));
        assert_eq!(
            failure(&sub),
            Some("SUBSCRIBE refused: 423 Interval Too Brief")
        );
    }

    /// US4 scenario 2.
    #[test]
    fn a_503_with_retry_after_is_not_retried_before_it() {
        let t0 = Instant::now();
        let mut sub = started(t0);
        let r = resp(
            503,
            "Service Unavailable",
            "sub-a",
            7,
            &[("Retry-After", "120 (maintenance)")],
        );
        sub.on_response(&r, t0);
        assert_eq!(sub.poll(t0 + secs(119), false), None);
        assert_eq!(
            sub.poll(t0 + secs(120), false),
            Some(SubAction::SendInitial { expires: 3600 })
        );
    }

    /// US4 scenario 3 and the "never supports reg" edge case.
    #[test]
    fn a_489_stops_subscribing_for_the_rest_of_the_registration() {
        let t0 = Instant::now();
        let mut sub = started(t0);
        sub.on_response(&resp(489, "Bad Event", "sub-a", 7, &[]), t0);
        for hour in 0..=24 {
            assert_eq!(sub.poll(t0 + secs(hour * 3600), false), None);
        }
        assert_eq!(
            sub.health(t0, SystemTime::now()).render(),
            "stopped until next registration (489 Bad Event)"
        );
    }

    /// US4 scenario 4 and FR-009a: 5 s doubling to a 600 s cap, with fresh
    /// identifiers each time and the refused one superseded.
    #[test]
    fn other_refusals_retry_on_a_doubling_backoff_capped_at_600s() {
        let t0 = Instant::now();
        let mut sub = started(t0);
        let mut now = t0;
        let mut delays = Vec::new();
        let statuses = [
            (403, "Forbidden"),
            (500, "Server Internal Error"),
            (603, "Decline"),
            (401, "Unauthorized"),
        ];
        for i in 0..10u32 {
            let call_id = if i == 0 {
                "sub-a".to_string()
            } else {
                format!("sub-{i}")
            };
            let (status, reason) = statuses[i as usize % statuses.len()];
            sub.on_response(&resp(status, reason, &call_id, 7 + i, &[]), now);
            assert_eq!(
                sub.on_response(&ok(&call_id, 7 + i, 3600), now),
                ResponseMatch::Superseded
            );
            let retry_at = sub.retry_at.expect("a retry is scheduled");
            delays.push(retry_at.duration_since(now).as_secs());
            assert_eq!(sub.poll(retry_at - secs(1), false), None);
            assert_eq!(
                sub.poll(retry_at, false),
                Some(SubAction::SendInitial { expires: 3600 })
            );
            now = retry_at;
            sub.on_sent(
                &sent(&format!("sub-{}", i + 1), "t", 8 + i, 3600),
                SubKind::Initial,
                now,
            );
        }
        assert_eq!(delays, [5, 10, 20, 40, 80, 160, 320, 600, 600, 600]);
    }

    /// SC-006: whatever the mix of refusals, two initial SUBSCRIBEs are never
    /// closer than the backoff interval scheduled between them.
    #[test]
    fn never_two_subscribes_inside_one_backoff_interval() {
        let t0 = Instant::now();
        let mut sub = started(t0);
        let mut now = t0;
        let mut last_sent = t0;
        let script: [(u16, &[(&str, &str)]); 6] = [
            (500, &[]),
            (423, &[("Min-Expires", "7200")]),
            (503, &[("Retry-After", "2")]),
            (423, &[("Min-Expires", "9000")]),
            (408, &[]),
            (403, &[]),
        ];
        for (i, (status, extra)) in script.iter().enumerate() {
            let call_id = format!("s{i}");
            if i > 0 {
                sub.on_sent(
                    &sent(&call_id, "t", 10 + i as u32, 3600),
                    SubKind::Initial,
                    now,
                );
                last_sent = now;
            } else {
                sub.current.as_mut().unwrap().call_id = call_id.clone();
            }
            sub.on_response(
                &resp(
                    *status,
                    "X",
                    &call_id,
                    if i > 0 { 10 + i as u32 } else { 7 },
                    extra,
                ),
                now,
            );
            loop {
                now += secs(1);
                if let Some(action) = sub.poll(now, false) {
                    assert!(matches!(action, SubAction::SendInitial { .. }));
                    assert!(now.duration_since(last_sent) >= SUB_INITIAL_BACKOFF);
                    break;
                }
            }
        }
    }

    // ---- US5: health (T034) ----

    #[test]
    fn health_reports_each_stage_of_the_subscription() {
        let t0 = Instant::now();
        let wall = SystemTime::UNIX_EPOCH + secs(1_700_000_000);
        assert_eq!(
            RegSubscription::new().health(t0, wall),
            SubscriptionHealth::None
        );

        let sub = confirmed(t0, 600);
        assert_eq!(
            sub.health(t0, wall),
            SubscriptionHealth::Confirmed {
                expires_at: Some(wall + secs(600))
            }
        );

        let mut sub = started(t0);
        sub.on_response(&ok("sub-a", 7, 3600), t0);
        sub.poll(t0 + TIMER_N, false);
        let SubscriptionHealth::Unconfirmed { retry_at, .. } = sub.health(t0 + TIMER_N, wall)
        else {
            panic!("expected unconfirmed");
        };
        assert_eq!(retry_at, Some(wall + secs(5)));
    }

    // ---- PR #129 review findings ----

    /// A NOTIFY confirms the subscription but the 2xx is lost: the
    /// transaction ends quietly at Timer F and the refresh still goes out.
    #[test]
    fn a_confirmed_subscription_whose_2xx_was_lost_is_still_refreshed() {
        let t0 = Instant::now();
        let mut sub = started(t0);
        let t1 = t0 + secs(1);
        sub.on_notify(
            &notify("sub-a", "ltag-a", Some("active;expires=600"), ""),
            t1,
        );
        let state = parse_subscription_state("active;expires=600");
        sub.apply_subscription_state(&state, t1);
        assert_eq!(sub.poll(t0 + TIMER_F, false), None);
        assert_eq!(failure(&sub), None);
        assert_eq!(sub.poll(t1 + secs(299), false), None);
        assert_eq!(
            sub.poll(t1 + secs(300), false),
            Some(SubAction::SendRefresh { expires: 3600 })
        );
    }

    /// The same with a grant too short for Timer F to have closed the
    /// transaction first: the refresh is not held for the lost response.
    #[test]
    fn a_short_grant_confirmed_by_notify_alone_refreshes_on_time() {
        let t0 = Instant::now();
        let mut sub = started(t0);
        sub.on_notify(
            &notify("sub-a", "ltag-a", Some("active;expires=60"), ""),
            t0,
        );
        sub.apply_subscription_state(&parse_subscription_state("active;expires=60"), t0);
        assert_eq!(sub.poll(t0 + secs(29), false), None);
        assert_eq!(
            sub.poll(t0 + secs(30), false),
            Some(SubAction::SendRefresh { expires: 3600 })
        );
        sub.on_sent(
            &sent("sub-a", "ltag-a", 8, 3600),
            SubKind::Refresh,
            t0 + secs(30),
        );
        // The original transaction's late 2xx is now an older CSeq.
        assert_eq!(
            sub.on_response(&ok("sub-a", 7, 60), t0 + secs(31)),
            ResponseMatch::Superseded
        );
        assert_eq!(
            sub.on_response(&ok("sub-a", 8, 600), t0 + secs(31)),
            ResponseMatch::Current
        );
        assert_eq!(failure(&sub), None);
    }

    /// A NOTIFY after a refresh that leaves the expiry where it was (here,
    /// one without `Subscription-State`) must not send the refresh again.
    #[test]
    fn a_notify_that_keeps_the_expiry_does_not_resend_the_refresh() {
        let t0 = Instant::now();
        let (mut sub, t1) = refreshing(t0);
        assert!(matches!(
            sub.on_notify(&notify("sub-a", "ltag-a", None, ""), t1 + secs(1)),
            NotifyMatch::Current { .. }
        ));
        for tick in 1..=60 {
            assert_eq!(sub.poll(t1 + secs(tick), false), None, "tick {tick}");
        }
        // The refresh's 2xx moves the expiry, and with it the next refresh.
        assert_eq!(
            sub.on_response(&ok("sub-a", 8, 600), t1 + secs(61)),
            ResponseMatch::Current
        );
        assert_eq!(sub.poll(t1 + secs(61 + 299), false), None);
        assert_eq!(
            sub.poll(t1 + secs(61 + 300), false),
            Some(SubAction::SendRefresh { expires: 3600 })
        );
    }

    /// Expiry never brings a retry forward past the network's `Retry-After`.
    #[test]
    fn expiry_keeps_a_longer_retry_after_from_a_refused_refresh() {
        let t0 = Instant::now();
        let (mut sub, t1) = refreshing(t0);
        let r = resp(
            503,
            "Service Unavailable",
            "sub-a",
            8,
            &[("Retry-After", "600")],
        );
        sub.on_response(&r, t1);
        assert_eq!(sub.poll(t0 + secs(600), false), None);
        assert!(sub.current.is_none());
        assert_eq!(sub.poll(t1 + secs(599), false), None);
        assert_eq!(
            sub.poll(t1 + secs(600), false),
            Some(SubAction::SendInitial { expires: 3600 })
        );
    }

    /// Network-supplied durations near `u64::MAX` are clamped, not added to
    /// an `Instant` (which would panic the agent).
    #[test]
    fn oversized_network_durations_do_not_overflow() {
        let t0 = Instant::now();
        let mut sub = confirmed(t0, 3600);
        let huge = "active;expires=18446744073709551615";
        sub.on_notify(&notify("sub-a", "ltag-a", Some(huge), ""), t0);
        sub.apply_subscription_state(&parse_subscription_state(huge), t0);
        assert!(sub.current.as_ref().unwrap().expires_at.is_some());

        let mut sub = started(t0);
        let r = resp(
            503,
            "Service Unavailable",
            "sub-a",
            7,
            &[("Retry-After", "18446744073709551615")],
        );
        sub.on_response(&r, t0);
        assert!(sub.retry_at.is_some());
        let r = resp(200, "OK", "sub-a", 7, &[("Expires", "4294967295")]);
        let mut sub = started(t0);
        sub.on_response(&r, t0);
        assert!(sub.current.as_ref().unwrap().expires_at.is_some());
    }

    /// A refused refresh is never resent within `SUB_INITIAL_BACKOFF`, and a
    /// repeated `423` that raises nothing falls back to the backoff.
    #[test]
    fn a_refresh_refused_423_is_spaced_and_does_not_loop() {
        let t0 = Instant::now();
        let (mut sub, t1) = refreshing(t0);
        let r423 = |cseq| {
            resp(
                423,
                "Interval Too Brief",
                "sub-a",
                cseq,
                &[("Min-Expires", "7200")],
            )
        };
        sub.on_response(&r423(8), t1);
        assert_eq!(sub.poll(t1 + secs(1), false), None);
        assert_eq!(
            sub.poll(t1 + secs(5), false),
            Some(SubAction::SendRefresh { expires: 7200 })
        );
        let t2 = t1 + secs(5);
        sub.on_sent(&sent("sub-a", "ltag-a", 9, 7200), SubKind::Refresh, t2);
        sub.on_response(&r423(9), t2);
        assert_eq!(
            failure(&sub),
            Some("SUBSCRIBE refused: 423 Interval Too Brief")
        );
        for tick in 1..5 {
            assert_eq!(sub.poll(t2 + secs(tick), false), None);
        }
        assert_eq!(
            sub.poll(t2 + secs(5), false),
            Some(SubAction::SendRefresh { expires: 7200 })
        );
    }

    #[test]
    fn retry_after_reads_the_leading_seconds_only() {
        assert_eq!(retry_after_secs("120"), Some(120));
        assert_eq!(retry_after_secs(" 18000;duration=3600"), Some(18000));
        assert_eq!(retry_after_secs("5 (busy)"), Some(5));
        assert_eq!(retry_after_secs("Fri, 31 Dec 1999 23:59:59 GMT"), None);
    }
}
