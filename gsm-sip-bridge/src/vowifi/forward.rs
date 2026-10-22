//! Agent B's side of AMR passthrough (specs/120-amr-passthrough-forwarder):
//! after the client answers, decide whether the call can carry the carrier's
//! AMR frames unchanged, and if so upgrade it.
//!
//! Every call starts on today's transcoded path and is only ever *upgraded*;
//! a call that does not qualify gets no extra step at all (research R1). The
//! upgrade happens before `CallAnswered` is sent, so Agent A has not answered
//! the carrier and no carrier audio has flowed yet:
//!
//! 1. bind the hop socket (towards Agent A) and the client forwarder sockets;
//! 2. `ForwardMedia` → wait for `ForwardAck` (carries Agent A's port);
//! 3. start the relay with both directions closed;
//! 4. register the forward entry and re-INVITE the client onto the forwarder;
//! 5. re-read what the client now says, re-check, silence the veth call.
//!
//! Any failure leaves a transcoded call: `ForwardCancel` is sent once Agent A
//! has acked, the sockets are released, and if the client had already been
//! moved onto the forwarder it is moved back.

use crate::amr_forward::relay::{ForwardParams, ForwardSession, Leg, Remote};
use crate::amr_forward::{compatible, AmrLegParams, MediaPath, Reason, SdpLeg};
use crate::ims::amr_rtp::AmrKind;
use crate::vowifi::control::{write_msg, ControlMessage};
use pjsua_safe::ClientMedia;
use std::io::Write;
use std::net::{IpAddr, SocketAddr, UdpSocket};
use std::sync::atomic::{AtomicBool, AtomicU64};
use std::sync::{mpsc, Arc, Mutex};
use std::time::{Duration, Instant};

/// How long Agent A has to answer a `ForwardMedia` (contracts/control-protocol).
pub(crate) const ACK_TIMEOUT: Duration = Duration::from_secs(2);
/// How long the client has to answer the upgrade re-INVITE.
pub(crate) const REINVITE_TIMEOUT: Duration = Duration::from_secs(3);
const POLL: Duration = Duration::from_millis(20);

/// The PJSIP calls the upgrade needs, behind a trait so the sequence can be
/// tested with real sockets and a real control channel but without a linked
/// PJSIP (which `pjsua-safe/tests/passthrough_hooks.rs` covers).
pub(crate) trait ClientLeg {
    /// What the client leg negotiated, `None` before its media is up.
    fn media(&self) -> Option<ClientMedia>;
    /// Carry the client's media on `rtp_port` from the next re-INVITE.
    fn register_forward(&self, rtp_port: u16, codec: &str, clock_rate: u32);
    /// Back to PJSIP's own transport from the next re-INVITE.
    fn clear_forward(&self);
    /// Increases each time the client leg's stream is created.
    fn generation(&self) -> u64;
    /// Re-INVITE the client and have PJSIP create its transport again.
    fn reinvite(&self) -> Result<(), String>;
    /// The client call is still up.
    fn is_up(&self) -> bool;
    /// Make the veth call carry no media: unpair it and pause its stream.
    fn silence_veth(&self);
}

/// What a client's answered call says about AMR, in the form the compatibility
/// rule judges. `Err` names why this call cannot be forwarded.
pub(crate) fn client_leg_params(media: &ClientMedia) -> Result<AmrLegParams, Reason> {
    let flavour = match (
        media.encoding_name.to_ascii_uppercase().as_str(),
        media.clock_rate,
    ) {
        ("AMR-WB", 16000) => AmrKind::Wb,
        ("AMR", 8000) => AmrKind::Nb,
        _ => return Err(Reason::ClientNotAmr),
    };
    // The relay labels each direction with one payload type per leg.
    if media.tx_pt != media.rx_pt {
        return Err(Reason::Feature);
    }
    if media.tx_event_pt != media.rx_event_pt {
        return Err(Reason::Dtmf);
    }
    let ms = u32::from(media.frames_per_packet) * 20;
    AmrLegParams::from_sdp(&SdpLeg {
        flavour,
        fmtp: &media.remote_fmtp,
        payload_type: media.tx_pt,
        dtmf_payload_type: media.tx_event_pt,
        ptime_ms: Some(ms),
        maxptime_ms: Some(ms),
    })
}

/// Outbound calls are never forwarded (FR-016), whatever the setting says: the
/// recorded path for every one of them.
pub(crate) fn outbound_media_path() -> MediaPath {
    MediaPath::Transcoded {
        reason: Reason::Outbound,
    }
}

/// A call that was upgraded: owns the relay, the sockets it advertises and
/// the follow-the-client bookkeeping. Dropping it stops the relay and frees
/// every port (FR-017).
pub(crate) struct ForwardedCall {
    session: ForwardSession,
    remote: Arc<Mutex<Option<SocketAddr>>>,
    last_remote: Option<SocketAddr>,
    last_send: Option<bool>,
    /// Holds the RTCP half of the client's port pair so the port stays ours.
    _client_rtcp: UdpSocket,
    pub(crate) client_rtp_port: u16,
}

impl ForwardedCall {
    /// Allow audio to flow towards the carrier (after `CallAnswered`) and
    /// start following the client.
    pub(crate) fn open(&mut self, leg: &dyn ClientLeg) {
        self.sync(leg, true);
    }

    /// Follow the client's address and media direction (FR-010, FR-011); called
    /// from the call's poll loop.
    pub(crate) fn follow(&mut self, leg: &dyn ClientLeg) {
        self.sync(leg, true);
    }

    fn sync(&mut self, leg: &dyn ClientLeg, towards_carrier: bool) {
        let Some(media) = leg.media() else { return };
        if media.remote_rtp != self.last_remote {
            if let Some(addr) = media.remote_rtp {
                tracing::info!(client = %addr, "forwarded call: client media address updated");
            }
            *self.remote.lock().unwrap_or_else(|e| e.into_inner()) = media.remote_rtp;
            self.last_remote = media.remote_rtp;
        }
        if self.last_send != Some(media.send_to_client) {
            if self.last_send.is_some() {
                tracing::info!(
                    send_to_client = media.send_to_client,
                    "forwarded call: client media direction changed"
                );
            }
            self.last_send = Some(media.send_to_client);
        }
        self.session
            .set_direction(towards_carrier, media.send_to_client);
    }

    pub(crate) fn received(&self) -> (u64, u64) {
        self.session.received()
    }
}

/// The verdict for one call and what it leaves behind.
pub(crate) struct Decision {
    /// What to record. `None` for a call this feature has nothing to say about
    /// (neither leg AMR, or the setting off with a non-AMR client).
    pub path: Option<MediaPath>,
    /// Present when the call was upgraded.
    pub forwarded: Option<ForwardedCall>,
    /// The call ended while the upgrade was waiting (Agent A sent `CallEnded`).
    pub call_ended: Option<String>,
    /// The upgrade failed after the client was moved and moving it back failed
    /// too: the call cannot continue.
    pub broken: bool,
}

impl Decision {
    fn path(path: Option<MediaPath>) -> Self {
        Self {
            path,
            forwarded: None,
            call_ended: None,
            broken: false,
        }
    }

    pub(crate) fn transcoded(reason: Reason) -> Self {
        Self::path(Some(MediaPath::Transcoded { reason }))
    }
}

/// Everything the decision needs from the call it is made for.
pub(crate) struct Inputs<'a, W: Write> {
    pub call_id: &'a str,
    /// `[vowifi].amr_passthrough`.
    pub enabled: bool,
    /// What Agent A reported for the carrier leg; `None` when it is not AMR or
    /// Agent A has the setting off.
    pub carrier: Option<&'a AmrLegParams>,
    /// Agent B's address on the veth link, where the hop socket binds, and
    /// Agent A's, which the hop socket is connected to.
    pub veth_bind_ip: IpAddr,
    pub agent_a_ip: IpAddr,
    pub control: &'a mut W,
    pub ctrl_rx: &'a mpsc::Receiver<ControlMessage>,
    pub ack_timeout: Duration,
    pub reinvite_timeout: Duration,
}

/// Decide the call's media path, upgrading it if it qualifies. See the module
/// docs for the sequence. Never fails a call because of the decision: every
/// error leaves it transcoded, except the one where the client cannot be moved
/// back ([`Decision::broken`]).
pub(crate) fn decide<W: Write>(inputs: Inputs<'_, W>, leg: &dyn ClientLeg) -> Decision {
    let Inputs {
        call_id,
        enabled,
        carrier,
        veth_bind_ip,
        agent_a_ip,
        control,
        ctrl_rx,
        ack_timeout,
        reinvite_timeout,
    } = inputs;

    let client_media = leg.media();
    let client = client_media.as_ref().map(client_leg_params);
    let client_is_amr = !matches!(client, Some(Err(Reason::ClientNotAmr)) | None);
    let codec = client_media
        .as_ref()
        .map(|m| (m.encoding_name.clone(), m.clock_rate))
        .unwrap_or_default();

    // Nothing to report unless AMR is in play on at least one leg.
    if !enabled {
        return Decision::path(client_is_amr.then_some(MediaPath::Transcoded {
            reason: Reason::Disabled,
        }));
    }
    let Some(carrier) = carrier else {
        return Decision::path(client_is_amr.then_some(MediaPath::Transcoded {
            reason: Reason::CarrierNotAmr,
        }));
    };
    let client = match client {
        Some(Ok(params)) => params,
        Some(Err(reason)) => return Decision::transcoded(reason),
        None => return Decision::transcoded(Reason::ClientNotAmr),
    };
    if let Err(reason) = compatible(carrier, &client) {
        return Decision::transcoded(reason);
    }

    // --- upgrade ---------------------------------------------------------
    let (Ok(hop), Ok((client_rtp, client_rtcp))) =
        (UdpSocket::bind((veth_bind_ip, 0)), bind_client_pair())
    else {
        return Decision::transcoded(Reason::ReinviteFailed);
    };
    let hop_port = match hop.local_addr() {
        Ok(a) => a.port(),
        Err(_) => return Decision::transcoded(Reason::ReinviteFailed),
    };
    let client_port = match client_rtp.local_addr() {
        Ok(a) => a.port(),
        Err(_) => return Decision::transcoded(Reason::ReinviteFailed),
    };

    let sent = write_msg(
        control,
        &ControlMessage::ForwardMedia {
            call_id: call_id.to_string(),
            rtp_port: hop_port,
            client: client.clone(),
        },
    );
    if sent.is_err() {
        return Decision::transcoded(Reason::AckTimeout);
    }
    let agent_a_port = match wait_for_ack(call_id, ctrl_rx, ack_timeout) {
        Ack::Ok(port) => port,
        Ack::Refused(reason) => {
            tracing::info!(call_id, reason = %reason, "Agent A declined the forward; transcoding");
            return Decision::transcoded(Reason::parse(&reason).unwrap_or(Reason::Feature));
        }
        Ack::Timeout => {
            tracing::info!(
                call_id,
                "Agent A did not answer the forward offer; transcoding"
            );
            cancel(control, call_id);
            return Decision::transcoded(Reason::AckTimeout);
        }
        Ack::CallEnded(reason) => {
            return Decision {
                call_ended: Some(reason),
                ..Decision::path(None)
            }
        }
    };

    // From here Agent A holds a pending hop socket: every failure cancels it.
    if hop.connect((agent_a_ip, agent_a_port)).is_err() {
        cancel(control, call_id);
        return Decision::transcoded(Reason::ReinviteFailed);
    }
    let remote_cell: Arc<Mutex<Option<SocketAddr>>> = Arc::new(Mutex::new(None));
    let session = match ForwardSession::start(ForwardParams {
        kind: carrier.flavour,
        a: Leg {
            socket: hop,
            remote: Remote::Connected,
            payload_type: carrier.payload_type,
            dtmf_payload_type: carrier.dtmf_payload_type,
            octet_aligned: carrier.octet_aligned,
        },
        b: Leg {
            socket: client_rtp,
            remote: Remote::Dynamic(remote_cell.clone()),
            payload_type: client.payload_type,
            dtmf_payload_type: client.dtmf_payload_type,
            octet_aligned: client.octet_aligned,
        },
        a_rtcp: None,
        b_rtcp: None,
        a_rx: Arc::new(AtomicU64::new(0)),
        b_rx: Arc::new(AtomicU64::new(0)),
        stop: Arc::new(AtomicBool::new(false)),
    }) {
        Ok(s) => s,
        Err(_) => {
            cancel(control, call_id);
            return Decision::transcoded(Reason::ReinviteFailed);
        }
    };
    // Closed both ways until the call is answered and the client is known.
    session.set_direction(false, false);
    let mut forwarded = ForwardedCall {
        session,
        remote: remote_cell,
        last_remote: None,
        last_send: None,
        _client_rtcp: client_rtcp,
        client_rtp_port: client_port,
    };

    leg.register_forward(client_port, &codec.0, codec.1);
    let before = leg.generation();
    let reinvited = leg.reinvite().is_ok() && wait_for_stream(leg, before, reinvite_timeout);
    if !reinvited {
        tracing::warn!(
            call_id,
            "client did not take the upgrade re-INVITE; transcoding"
        );
        leg.clear_forward();
        cancel(control, call_id);
        return Decision::transcoded(Reason::ReinviteFailed);
    }

    // What the client now says is what the audio will be judged against.
    let verdict = leg
        .media()
        .ok_or(Reason::ReinviteFailed)
        .and_then(|m| client_leg_params(&m).map(|p| (m, p)))
        .and_then(|(m, p)| compatible(carrier, &p).map(|()| m));
    let after = match verdict {
        Ok(m) => m,
        Err(reason) => {
            tracing::warn!(call_id, reason = %reason, "client's answer to the upgrade is not forwardable; moving it back");
            cancel(control, call_id);
            return downgrade(leg, call_id, reason, reinvite_timeout);
        }
    };
    *forwarded.remote.lock().unwrap_or_else(|e| e.into_inner()) = after.remote_rtp;
    forwarded.last_remote = after.remote_rtp;
    leg.silence_veth();
    tracing::info!(
        call_id,
        hop_port,
        agent_a_port,
        client_port = forwarded.client_rtp_port,
        "call upgraded: client leg now carried by the forwarder"
    );

    Decision {
        path: Some(MediaPath::forwarded(carrier, &client)),
        forwarded: Some(forwarded),
        call_ended: None,
        broken: false,
    }
}

/// Put the client back on PJSIP's own transport. The call is `broken` only if
/// that fails too.
fn downgrade(leg: &dyn ClientLeg, call_id: &str, reason: Reason, timeout: Duration) -> Decision {
    leg.clear_forward();
    let before = leg.generation();
    if leg.reinvite().is_ok() && wait_for_stream(leg, before, timeout) {
        Decision::transcoded(reason)
    } else {
        tracing::error!(
            call_id,
            "could not move the client back onto PJSIP's own media"
        );
        Decision {
            broken: true,
            ..Decision::transcoded(Reason::ReinviteFailed)
        }
    }
}

fn wait_for_stream(leg: &dyn ClientLeg, before: u64, timeout: Duration) -> bool {
    let deadline = Instant::now() + timeout;
    while Instant::now() < deadline {
        if leg.generation() > before {
            return true;
        }
        if !leg.is_up() {
            return false;
        }
        std::thread::sleep(POLL);
    }
    false
}

fn cancel<W: Write>(control: &mut W, call_id: &str) {
    let _ = write_msg(
        control,
        &ControlMessage::ForwardCancel {
            call_id: call_id.to_string(),
        },
    );
}

enum Ack {
    Ok(u16),
    Refused(String),
    Timeout,
    CallEnded(String),
}

fn wait_for_ack(call_id: &str, ctrl_rx: &mpsc::Receiver<ControlMessage>, timeout: Duration) -> Ack {
    let deadline = Instant::now() + timeout;
    while let Some(left) = deadline.checked_duration_since(Instant::now()) {
        match ctrl_rx.recv_timeout(left.min(Duration::from_millis(100))) {
            Ok(ControlMessage::ForwardAck {
                ok: true,
                rtp_port: Some(port),
                ..
            }) => return Ack::Ok(port),
            Ok(ControlMessage::ForwardAck { ok: true, .. }) => {
                return Ack::Refused("ack_without_port".into())
            }
            Ok(ControlMessage::ForwardAck { reason, .. }) => {
                return Ack::Refused(reason.unwrap_or_default())
            }
            Ok(ControlMessage::CallEnded { reason, .. }) => return Ack::CallEnded(reason),
            Ok(other) => {
                tracing::debug!(call_id, message = ?other, "ignoring control message while waiting for ForwardAck")
            }
            Err(mpsc::RecvTimeoutError::Timeout) => {}
            Err(mpsc::RecvTimeoutError::Disconnected) => {
                return Ack::CallEnded("control_connection_lost".into())
            }
        }
    }
    Ack::Timeout
}

/// Binds an even RTP port and the RTCP port above it on every address (the
/// client's route to us is not known here; PJSIP advertises the right one).
fn bind_client_pair() -> std::io::Result<(UdpSocket, UdpSocket)> {
    let mut last = None;
    for _ in 0..64 {
        let rtp = UdpSocket::bind(("0.0.0.0", 0))?;
        let port = rtp.local_addr()?.port();
        if port % 2 != 0 || port == u16::MAX {
            continue;
        }
        match UdpSocket::bind(("0.0.0.0", port + 1)) {
            Ok(rtcp) => return Ok((rtp, rtcp)),
            Err(e) => last = Some(e),
        }
    }
    Err(last.unwrap_or_else(|| std::io::Error::other("no even RTP port available")))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::ims::rtp::build_packet;
    use std::cell::RefCell;

    /// A client leg whose PJSIP side is scripted; everything else in these
    /// tests (sockets, control messages, the relay) is real.
    struct FakeLeg {
        media: RefCell<Option<ClientMedia>>,
        /// What the media reads as after the upgrade re-INVITE.
        after_upgrade: RefCell<Option<ClientMedia>>,
        generation: RefCell<u64>,
        reinvite_ok: bool,
        /// Whether the re-INVITE re-creates the stream (a client that rejects
        /// it does not).
        stream_recreated: bool,
        calls: RefCell<Vec<String>>,
        forward_port: RefCell<Option<u16>>,
    }

    impl FakeLeg {
        fn new(media: ClientMedia) -> Self {
            Self {
                after_upgrade: RefCell::new(Some(media.clone())),
                media: RefCell::new(Some(media)),
                generation: RefCell::new(1),
                reinvite_ok: true,
                stream_recreated: true,
                calls: RefCell::new(Vec::new()),
                forward_port: RefCell::new(None),
            }
        }

        fn calls(&self) -> Vec<String> {
            self.calls.borrow().clone()
        }
    }

    // The trait has no Send/Sync bound, so interior mutability is plain.
    impl ClientLeg for FakeLeg {
        fn media(&self) -> Option<ClientMedia> {
            self.media.borrow().clone()
        }
        fn register_forward(&self, rtp_port: u16, codec: &str, clock_rate: u32) {
            *self.forward_port.borrow_mut() = Some(rtp_port);
            self.calls
                .borrow_mut()
                .push(format!("register_forward {codec}/{clock_rate}"));
        }
        fn clear_forward(&self) {
            self.calls.borrow_mut().push("clear_forward".into());
        }
        fn generation(&self) -> u64 {
            *self.generation.borrow()
        }
        fn reinvite(&self) -> Result<(), String> {
            self.calls.borrow_mut().push("reinvite".into());
            if !self.reinvite_ok {
                return Err("refused".into());
            }
            if self.stream_recreated {
                *self.generation.borrow_mut() += 1;
                if self.forward_port.borrow().is_some() {
                    // The upgrade: the client now reads as it will after it.
                    *self.media.borrow_mut() = self.after_upgrade.borrow().clone();
                }
            }
            Ok(())
        }
        fn is_up(&self) -> bool {
            true
        }
        fn silence_veth(&self) {
            self.calls.borrow_mut().push("silence_veth".into());
        }
    }

    fn client_media(name: &str, rate: u32, fmtp: &str) -> ClientMedia {
        ClientMedia {
            encoding_name: name.into(),
            clock_rate: rate,
            remote_rtp: Some("127.0.0.1:9".parse().unwrap()),
            tx_pt: 96,
            rx_pt: 96,
            tx_event_pt: Some(101),
            rx_event_pt: Some(101),
            remote_fmtp: fmtp.into(),
            frames_per_packet: 1,
            send_to_client: true,
            has_media: true,
        }
    }

    fn wb_client(fmtp: &str) -> ClientMedia {
        client_media("AMR-WB", 16000, fmtp)
    }

    fn carrier_wb(fmtp: &str) -> AmrLegParams {
        AmrLegParams::from_sdp(&SdpLeg {
            flavour: AmrKind::Wb,
            fmtp,
            payload_type: 110,
            dtmf_payload_type: Some(105),
            ptime_ms: Some(20),
            maxptime_ms: Some(240),
        })
        .unwrap()
    }

    /// Runs `decide` with a control channel preloaded with `replies` and
    /// returns the decision plus every message Agent B wrote.
    fn run(
        enabled: bool,
        carrier: Option<&AmrLegParams>,
        leg: &FakeLeg,
        replies: Vec<ControlMessage>,
    ) -> (Decision, Vec<ControlMessage>) {
        run_with(enabled, carrier, leg, replies, Duration::from_millis(200))
    }

    fn run_with(
        enabled: bool,
        carrier: Option<&AmrLegParams>,
        leg: &FakeLeg,
        replies: Vec<ControlMessage>,
        ack_timeout: Duration,
    ) -> (Decision, Vec<ControlMessage>) {
        let (tx, rx) = mpsc::channel();
        for m in replies {
            tx.send(m).unwrap();
        }
        let mut written: Vec<u8> = Vec::new();
        let decision = decide(
            Inputs {
                call_id: "c1",
                enabled,
                carrier,
                veth_bind_ip: "127.0.0.1".parse().unwrap(),
                agent_a_ip: "127.0.0.1".parse().unwrap(),
                control: &mut written,
                ctrl_rx: &rx,
                ack_timeout,
                reinvite_timeout: Duration::from_millis(300),
            },
            leg,
        );
        let mut reader = std::io::BufReader::new(&written[..]);
        let mut sent = Vec::new();
        while let Ok(m) = crate::vowifi::control::read_msg(&mut reader) {
            sent.push(m);
        }
        (decision, sent)
    }

    fn ack(port: u16) -> ControlMessage {
        ControlMessage::ForwardAck {
            call_id: "c1".into(),
            ok: true,
            reason: None,
            rtp_port: Some(port),
        }
    }

    fn path_of(d: &Decision) -> Option<String> {
        d.path.as_ref().map(MediaPath::record_string)
    }

    #[test]
    fn outbound_calls_are_always_recorded_as_transcoded_outbound() {
        assert_eq!(outbound_media_path().record_string(), "transcoded:outbound");
        assert_eq!(
            outbound_media_path().log_fields(),
            "path=transcoded reason=outbound"
        );
    }

    // ----- client_leg_params -------------------------------------------------

    #[test]
    fn a_clients_answered_call_becomes_amr_leg_parameters() {
        let p = client_leg_params(&wb_client("octet-align=1; mode-set=0,1,2")).unwrap();
        assert_eq!(p.flavour, AmrKind::Wb);
        assert!(p.octet_aligned);
        assert_eq!(p.mode_set, Some(0b111));
        assert_eq!((p.payload_type, p.dtmf_payload_type), (96, Some(101)));
        let nb = client_leg_params(&client_media("AMR", 8000, "")).unwrap();
        assert_eq!(nb.flavour, AmrKind::Nb);
        assert!(!nb.octet_aligned);
    }

    #[test]
    fn only_amr_clients_qualify_and_asymmetric_payload_types_do_not() {
        for (name, rate) in [
            ("PCMU", 8000),
            ("G722", 8000),
            ("opus", 48000),
            ("AMR-WB", 8000),
        ] {
            assert_eq!(
                client_leg_params(&client_media(name, rate, "")),
                Err(Reason::ClientNotAmr),
                "{name}/{rate}"
            );
        }
        let mut asym = wb_client("");
        asym.rx_pt = 97;
        assert_eq!(client_leg_params(&asym), Err(Reason::Feature));
        let mut asym = wb_client("");
        asym.rx_event_pt = Some(102);
        assert_eq!(client_leg_params(&asym), Err(Reason::Dtmf));
    }

    // ----- calls that are not upgraded: no step, nothing sent ---------------

    fn assert_untouched(leg: &FakeLeg, sent: &[ControlMessage]) {
        assert!(sent.is_empty(), "no control message at all: {sent:?}");
        assert!(leg.calls().is_empty(), "no PJSIP call: {:?}", leg.calls());
    }

    #[test]
    fn with_the_setting_off_an_amr_client_is_recorded_disabled_and_nothing_else_happens() {
        let leg = FakeLeg::new(wb_client("octet-align=1"));
        let (d, sent) = run(false, Some(&carrier_wb("octet-align=1")), &leg, vec![]);
        assert_eq!(path_of(&d).as_deref(), Some("transcoded:disabled"));
        assert!(d.forwarded.is_none() && !d.broken && d.call_ended.is_none());
        assert_untouched(&leg, &sent);
    }

    #[test]
    fn with_the_setting_off_a_non_amr_client_records_nothing() {
        let leg = FakeLeg::new(client_media("PCMU", 8000, ""));
        let (d, sent) = run(false, None, &leg, vec![]);
        assert!(d.path.is_none());
        assert_untouched(&leg, &sent);
    }

    #[test]
    fn a_carrier_that_is_not_amr_is_recorded_when_the_client_is() {
        let leg = FakeLeg::new(wb_client(""));
        let (d, sent) = run(true, None, &leg, vec![]);
        assert_eq!(path_of(&d).as_deref(), Some("transcoded:carrier_not_amr"));
        assert_untouched(&leg, &sent);
        let pcmu = FakeLeg::new(client_media("PCMU", 8000, ""));
        let (d, _) = run(true, None, &pcmu, vec![]);
        assert!(d.path.is_none(), "neither leg AMR: nothing new is recorded");
    }

    #[test]
    fn a_client_that_answered_something_else_is_transcoded_without_a_word_to_agent_a() {
        for (name, rate) in [("PCMU", 8000), ("G722", 8000), ("opus", 48000)] {
            let leg = FakeLeg::new(client_media(name, rate, ""));
            let (d, sent) = run(true, Some(&carrier_wb("octet-align=1")), &leg, vec![]);
            assert_eq!(
                path_of(&d).as_deref(),
                Some("transcoded:client_not_amr"),
                "{name}"
            );
            assert_untouched(&leg, &sent);
        }
    }

    #[test]
    fn incompatible_parameters_are_transcoded_with_the_reason_and_no_forward_offer() {
        // carrier WB, client NB
        let nb = FakeLeg::new(client_media("AMR", 8000, ""));
        let (d, sent) = run(true, Some(&carrier_wb("octet-align=1")), &nb, vec![]);
        assert_eq!(path_of(&d).as_deref(), Some("transcoded:flavour_differs"));
        assert_untouched(&nb, &sent);
        // a restricted mode-set the carrier may exceed
        let leg = FakeLeg::new(wb_client("octet-align=1; mode-set=0,1"));
        let (d, sent) = run(true, Some(&carrier_wb("octet-align=1")), &leg, vec![]);
        assert_eq!(path_of(&d).as_deref(), Some("transcoded:mode_set"));
        assert_untouched(&leg, &sent);
        // a feature in use
        let leg = FakeLeg::new(wb_client("octet-align=1; crc=1"));
        let (d, _) = run(true, Some(&carrier_wb("octet-align=1")), &leg, vec![]);
        assert_eq!(path_of(&d).as_deref(), Some("transcoded:feature"));
        // DTMF on one leg only
        let mut no_dtmf = wb_client("octet-align=1");
        no_dtmf.tx_event_pt = None;
        no_dtmf.rx_event_pt = None;
        let leg = FakeLeg::new(no_dtmf);
        let (d, _) = run(true, Some(&carrier_wb("octet-align=1")), &leg, vec![]);
        assert_eq!(path_of(&d).as_deref(), Some("transcoded:dtmf"));
    }

    // ----- the upgrade -------------------------------------------------------

    /// Agent A's end of the hop, so the ack can name a real port.
    fn agent_a_socket() -> UdpSocket {
        let s = UdpSocket::bind("127.0.0.1:0").unwrap();
        s.set_read_timeout(Some(Duration::from_secs(2))).unwrap();
        s
    }

    #[test]
    fn a_compatible_call_is_upgraded_in_the_documented_order() {
        let a = agent_a_socket();
        let leg = FakeLeg::new(wb_client(""));
        let carrier = carrier_wb("octet-align=1");
        let (d, sent) = run(
            true,
            Some(&carrier),
            &leg,
            vec![ack(a.local_addr().unwrap().port())],
        );
        assert_eq!(path_of(&d).as_deref(), Some("forwarded:amr-wb:oa/be"));
        assert!(d.forwarded.is_some() && !d.broken);
        assert_eq!(
            leg.calls(),
            ["register_forward AMR-WB/16000", "reinvite", "silence_veth"],
            "the forward entry is registered before the re-INVITE; the veth call is silenced last"
        );
        // Exactly one message to Agent A: the offer, naming the hop port and the client.
        match sent.as_slice() {
            [ControlMessage::ForwardMedia {
                call_id,
                rtp_port,
                client,
            }] => {
                assert_eq!(call_id, "c1");
                assert_ne!(*rtp_port, 0);
                assert_eq!(client.flavour, AmrKind::Wb);
                assert!(!client.octet_aligned);
            }
            other => panic!("unexpected messages: {other:?}"),
        }
        // The client is told to send to the port the forward entry named.
        assert_eq!(
            *leg.forward_port.borrow(),
            Some(d.forwarded.as_ref().unwrap().client_rtp_port)
        );
    }

    #[test]
    fn an_upgraded_call_carries_frames_between_agent_a_and_the_client_with_per_leg_labels() {
        let a = agent_a_socket();
        let client = UdpSocket::bind("127.0.0.1:0").unwrap();
        client
            .set_read_timeout(Some(Duration::from_secs(2)))
            .unwrap();
        let mut media = wb_client(""); // bandwidth-efficient, PT 96, events 101
        media.remote_rtp = Some(client.local_addr().unwrap());
        let leg = FakeLeg::new(media);
        let carrier = carrier_wb("octet-align=1"); // octet-aligned, PT 110, events 105
        let (mut d, sent) = run(
            true,
            Some(&carrier),
            &leg,
            vec![ack(a.local_addr().unwrap().port())],
        );
        let mut fwd = d.forwarded.take().expect("upgraded");
        let hop_port = match &sent[0] {
            ControlMessage::ForwardMedia { rtp_port, .. } => *rtp_port,
            other => panic!("{other:?}"),
        };
        fwd.open(&leg); // the call is answered
        let client_port = fwd.client_rtp_port;

        // Carrier -> client: octet-aligned PT 110 in, bandwidth-efficient PT 96 out.
        let oa = {
            let mut p = vec![0xf0u8, (2 << 3) | (1 << 2)];
            p.extend((0..32u8).map(|i| i.wrapping_mul(7) | 1));
            let last = p.len() - 1;
            p[last] &= 0x80; // 253 bits: only the top bit of the last byte is speech
            p
        };
        a.send_to(&build_packet(1, 0, 7, 110, &oa), ("127.0.0.1", hop_port))
            .unwrap();
        let mut buf = [0u8; 256];
        let n = client.recv(&mut buf).unwrap();
        assert_eq!(buf[1] & 0x7f, 96, "relabelled to the client's type");
        let be = crate::amr_forward::repack::repack(&oa, AmrKind::Wb, true, false).unwrap();
        assert_eq!(&buf[12..n], &be[..]);

        // Client -> carrier: back to octet-aligned PT 110, identical bytes.
        client
            .send_to(&build_packet(2, 0, 8, 96, &be), ("127.0.0.1", client_port))
            .unwrap();
        let n = a.recv(&mut buf).unwrap();
        assert_eq!(buf[1] & 0x7f, 110);
        assert_eq!(&buf[12..n], &oa[..]);

        // Digits are relabelled between the legs' telephone-event types.
        client
            .send_to(
                &build_packet(3, 0, 8, 101, &[5, 0x8a, 0, 160]),
                ("127.0.0.1", client_port),
            )
            .unwrap();
        let n = a.recv(&mut buf).unwrap();
        assert_eq!(
            (buf[1] & 0x7f, &buf[12..n]),
            (105, &[5u8, 0x8a, 0, 160][..])
        );
    }

    #[test]
    fn nothing_flows_towards_the_carrier_until_the_call_is_answered() {
        let a = agent_a_socket();
        let client = UdpSocket::bind("127.0.0.1:0").unwrap();
        let mut media = wb_client("octet-align=1");
        media.remote_rtp = Some(client.local_addr().unwrap());
        let leg = FakeLeg::new(media);
        let carrier = carrier_wb("octet-align=1");
        let (mut d, _) = run(
            true,
            Some(&carrier),
            &leg,
            vec![ack(a.local_addr().unwrap().port())],
        );
        let fwd = d.forwarded.take().unwrap();
        // The client talks straight away; the carrier is not answered yet.
        let pkt = build_packet(1, 0, 8, 96, &[0xf0, (2 << 3) | 4, 1, 2, 3]);
        client
            .send_to(&pkt, ("127.0.0.1", fwd.client_rtp_port))
            .unwrap();
        a.set_read_timeout(Some(Duration::from_millis(400)))
            .unwrap();
        let mut buf = [0u8; 64];
        assert!(
            a.recv(&mut buf).is_err(),
            "pre-answer audio must not reach the carrier"
        );
    }

    #[test]
    fn a_held_client_is_not_sent_to_and_resumes_when_it_is_active_again() {
        let a = agent_a_socket();
        let client = UdpSocket::bind("127.0.0.1:0").unwrap();
        client
            .set_read_timeout(Some(Duration::from_millis(400)))
            .unwrap();
        let mut media = wb_client("octet-align=1");
        media.remote_rtp = Some(client.local_addr().unwrap());
        let leg = FakeLeg::new(media.clone());
        let carrier = carrier_wb("octet-align=1");
        let (mut d, sent) = run(
            true,
            Some(&carrier),
            &leg,
            vec![ack(a.local_addr().unwrap().port())],
        );
        let mut fwd = d.forwarded.take().unwrap();
        let hop_port = match &sent[0] {
            ControlMessage::ForwardMedia { rtp_port, .. } => *rtp_port,
            other => panic!("{other:?}"),
        };
        fwd.open(&leg);
        let pkt = |seq| build_packet(seq, 0, 7, 110, &[0xf0, (2 << 3) | 4, 1, 2, 3]);

        media.send_to_client = false; // the client put the call on hold
        *leg.media.borrow_mut() = Some(media.clone());
        fwd.follow(&leg);
        a.send_to(&pkt(1), ("127.0.0.1", hop_port)).unwrap();
        let mut buf = [0u8; 64];
        assert!(
            client.recv(&mut buf).is_err(),
            "nothing towards a held client"
        );

        media.send_to_client = true;
        *leg.media.borrow_mut() = Some(media);
        fwd.follow(&leg);
        client
            .set_read_timeout(Some(Duration::from_secs(2)))
            .unwrap();
        a.send_to(&pkt(2), ("127.0.0.1", hop_port)).unwrap();
        assert!(
            client.recv(&mut buf).is_ok(),
            "resumes when the client is active again"
        );
    }

    #[test]
    fn a_changed_client_address_is_followed() {
        let a = agent_a_socket();
        let first = UdpSocket::bind("127.0.0.1:0").unwrap();
        let second = UdpSocket::bind("127.0.0.1:0").unwrap();
        second
            .set_read_timeout(Some(Duration::from_secs(2)))
            .unwrap();
        let mut media = wb_client("octet-align=1");
        media.remote_rtp = Some(first.local_addr().unwrap());
        let leg = FakeLeg::new(media.clone());
        let carrier = carrier_wb("octet-align=1");
        let (mut d, sent) = run(
            true,
            Some(&carrier),
            &leg,
            vec![ack(a.local_addr().unwrap().port())],
        );
        let mut fwd = d.forwarded.take().unwrap();
        let hop_port = match &sent[0] {
            ControlMessage::ForwardMedia { rtp_port, .. } => *rtp_port,
            other => panic!("{other:?}"),
        };
        fwd.open(&leg);
        media.remote_rtp = Some(second.local_addr().unwrap());
        *leg.media.borrow_mut() = Some(media);
        fwd.follow(&leg);
        a.send_to(
            &build_packet(1, 0, 7, 110, &[0xf0, (2 << 3) | 4, 1, 2, 3]),
            ("127.0.0.1", hop_port),
        )
        .unwrap();
        let mut buf = [0u8; 64];
        assert!(second.recv(&mut buf).is_ok());
    }

    #[test]
    fn dropping_an_upgraded_call_frees_its_ports() {
        let a = agent_a_socket();
        let leg = FakeLeg::new(wb_client("octet-align=1"));
        let carrier = carrier_wb("octet-align=1");
        let (mut d, sent) = run(
            true,
            Some(&carrier),
            &leg,
            vec![ack(a.local_addr().unwrap().port())],
        );
        let fwd = d.forwarded.take().unwrap();
        let hop_port = match &sent[0] {
            ControlMessage::ForwardMedia { rtp_port, .. } => *rtp_port,
            other => panic!("{other:?}"),
        };
        let client_port = fwd.client_rtp_port;
        drop(fwd);
        UdpSocket::bind(("127.0.0.1", hop_port)).expect("hop port released");
        UdpSocket::bind(("0.0.0.0", client_port)).expect("client RTP port released");
        UdpSocket::bind(("0.0.0.0", client_port + 1)).expect("client RTCP port released");
    }

    // ----- every failure leaves a transcoded call --------------------------

    #[test]
    fn silence_from_agent_a_is_an_ack_timeout_and_the_offer_is_cancelled() {
        let leg = FakeLeg::new(wb_client("octet-align=1"));
        let carrier = carrier_wb("octet-align=1");
        let (d, sent) = run(true, Some(&carrier), &leg, vec![]);
        assert_eq!(path_of(&d).as_deref(), Some("transcoded:ack_timeout"));
        assert!(d.forwarded.is_none() && !d.broken);
        assert!(matches!(
            sent.as_slice(),
            [
                ControlMessage::ForwardMedia { .. },
                ControlMessage::ForwardCancel { .. }
            ]
        ));
        assert!(leg.calls().is_empty(), "PJSIP untouched: {:?}", leg.calls());
    }

    #[test]
    fn a_declined_offer_is_transcoded_with_agent_as_reason_and_nothing_is_cancelled() {
        let leg = FakeLeg::new(wb_client("octet-align=1"));
        let carrier = carrier_wb("octet-align=1");
        let nack = ControlMessage::ForwardAck {
            call_id: "c1".into(),
            ok: false,
            reason: Some("mode_set".into()),
            rtp_port: None,
        };
        let (d, sent) = run(true, Some(&carrier), &leg, vec![nack]);
        assert_eq!(path_of(&d).as_deref(), Some("transcoded:mode_set"));
        assert!(matches!(
            sent.as_slice(),
            [ControlMessage::ForwardMedia { .. }]
        ));
        assert!(leg.calls().is_empty());
    }

    #[test]
    fn a_client_that_refuses_the_reinvite_leaves_a_transcoded_call_and_cancels() {
        let a = agent_a_socket();
        let mut leg = FakeLeg::new(wb_client("octet-align=1"));
        leg.reinvite_ok = false;
        let carrier = carrier_wb("octet-align=1");
        let (d, sent) = run(
            true,
            Some(&carrier),
            &leg,
            vec![ack(a.local_addr().unwrap().port())],
        );
        assert_eq!(path_of(&d).as_deref(), Some("transcoded:reinvite_failed"));
        assert!(d.forwarded.is_none() && !d.broken);
        assert!(
            matches!(sent.last(), Some(ControlMessage::ForwardCancel { .. })),
            "{sent:?}"
        );
        assert_eq!(
            leg.calls(),
            ["register_forward AMR-WB/16000", "reinvite", "clear_forward"],
            "the entry is withdrawn and the veth call is never silenced"
        );
    }

    #[test]
    fn a_reinvite_the_client_never_acts_on_times_out_to_a_transcoded_call() {
        let a = agent_a_socket();
        let mut leg = FakeLeg::new(wb_client("octet-align=1"));
        leg.stream_recreated = false;
        let carrier = carrier_wb("octet-align=1");
        let (d, sent) = run(
            true,
            Some(&carrier),
            &leg,
            vec![ack(a.local_addr().unwrap().port())],
        );
        assert_eq!(path_of(&d).as_deref(), Some("transcoded:reinvite_failed"));
        assert!(matches!(
            sent.last(),
            Some(ControlMessage::ForwardCancel { .. })
        ));
        assert!(leg.calls().contains(&"clear_forward".to_string()));
        assert!(!leg.calls().contains(&"silence_veth".to_string()));
    }

    #[test]
    fn an_answer_to_the_upgrade_that_is_no_longer_compatible_moves_the_client_back() {
        let a = agent_a_socket();
        let leg = FakeLeg::new(wb_client("octet-align=1"));
        // After the upgrade the client reads as restricted.
        *leg.after_upgrade.borrow_mut() = Some(wb_client("octet-align=1; mode-set=0"));
        let carrier = carrier_wb("octet-align=1");
        let (d, sent) = run(
            true,
            Some(&carrier),
            &leg,
            vec![ack(a.local_addr().unwrap().port())],
        );
        assert_eq!(path_of(&d).as_deref(), Some("transcoded:mode_set"));
        assert!(d.forwarded.is_none() && !d.broken);
        assert!(matches!(
            sent.last(),
            Some(ControlMessage::ForwardCancel { .. })
        ));
        assert_eq!(
            leg.calls(),
            [
                "register_forward AMR-WB/16000",
                "reinvite",
                "clear_forward",
                "reinvite"
            ],
            "moved back with a second re-INVITE, veth call never silenced"
        );
    }

    #[test]
    fn a_client_that_cannot_be_moved_back_breaks_the_call() {
        let a = agent_a_socket();
        // The first re-INVITE works; the second (the downgrade) is refused.
        struct OneShot(FakeLeg, RefCell<u32>);
        impl ClientLeg for OneShot {
            fn media(&self) -> Option<ClientMedia> {
                self.0.media()
            }
            fn register_forward(&self, p: u16, c: &str, r: u32) {
                self.0.register_forward(p, c, r)
            }
            fn clear_forward(&self) {
                self.0.clear_forward()
            }
            fn generation(&self) -> u64 {
                self.0.generation()
            }
            fn reinvite(&self) -> Result<(), String> {
                *self.1.borrow_mut() += 1;
                if *self.1.borrow() > 1 {
                    return Err("refused".into());
                }
                self.0.reinvite()
            }
            fn is_up(&self) -> bool {
                true
            }
            fn silence_veth(&self) {}
        }
        let inner = FakeLeg::new(wb_client("octet-align=1"));
        *inner.after_upgrade.borrow_mut() = Some(wb_client("octet-align=1; crc=1"));
        let leg = OneShot(inner, RefCell::new(0));
        let carrier = carrier_wb("octet-align=1");
        let (tx, rx) = mpsc::channel();
        tx.send(ack(a.local_addr().unwrap().port())).unwrap();
        let mut written: Vec<u8> = Vec::new();
        let d = decide(
            Inputs {
                call_id: "c1",
                enabled: true,
                carrier: Some(&carrier),
                veth_bind_ip: "127.0.0.1".parse().unwrap(),
                agent_a_ip: "127.0.0.1".parse().unwrap(),
                control: &mut written,
                ctrl_rx: &rx,
                ack_timeout: Duration::from_millis(200),
                reinvite_timeout: Duration::from_millis(300),
            },
            &leg,
        );
        assert!(d.broken, "no way to continue the call");
        assert!(d.forwarded.is_none());
    }

    #[test]
    fn a_call_that_ends_while_waiting_for_the_ack_is_reported_as_ended() {
        let leg = FakeLeg::new(wb_client("octet-align=1"));
        let carrier = carrier_wb("octet-align=1");
        let ended = ControlMessage::CallEnded {
            call_id: "c1".into(),
            reason: "caller_hangup".into(),
        };
        let (d, _) = run(true, Some(&carrier), &leg, vec![ended]);
        assert_eq!(d.call_ended.as_deref(), Some("caller_hangup"));
        assert!(d.forwarded.is_none() && !d.broken);
        assert!(leg.calls().is_empty());
    }

    #[test]
    fn counters_start_at_zero() {
        let a = agent_a_socket();
        let leg = FakeLeg::new(wb_client("octet-align=1"));
        let carrier = carrier_wb("octet-align=1");
        let (mut d, _) = run(
            true,
            Some(&carrier),
            &leg,
            vec![ack(a.local_addr().unwrap().port())],
        );
        let fwd = d.forwarded.take().unwrap();
        assert_eq!(fwd.received(), (0, 0));
    }
}
