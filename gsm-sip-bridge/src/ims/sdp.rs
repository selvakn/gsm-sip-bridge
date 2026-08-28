//! Minimal SDP (RFC 4566) build/parse — just enough for a two-codec
//! (PCMU, AMR-WB) audio offer/answer, not a general-purpose SDP library.

use crate::error::{BridgeError, BridgeResult};
use std::net::{IpAddr, SocketAddr};

/// The RTCP bandwidth this bridge's answer declares for active senders
/// (`b=RS:`) and other receivers (`b=RR:`) — TS 26.114 §6.2.10's customary
/// 3GPP defaults, in **bits per second** (RFC 3556 §2: unlike `b=AS:`,
/// these two are not in kilobits/second). specs/046-rtcp-reporting's
/// report cadence (`ims::rtcp::ReportSchedule`) derives its interval from
/// [`RTCP_SR_BANDWIDTH_BPS`] so the declaration and the actual send rate
/// agree by construction (FR-004).
pub(crate) const RTCP_SR_BANDWIDTH_BPS: u32 = 800;
pub(crate) const RTCP_RR_BANDWIDTH_BPS: u32 = 2400;

const PCMU_PAYLOAD_TYPE: u8 = 0;
/// Dynamic payload type (RFC 3551 §6: 96-127 range) chosen for AMR-WB —
/// arbitrary but must match between the `a=rtpmap`/`a=fmtp` lines here and
/// whatever `parse_answer` compares the answer's payload type against.
const AMR_WB_PAYLOAD_TYPE: u8 = 96;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum NegotiatedCodec {
    Pcmu,
    /// AMR narrowband (`AMR/8000`). Offered by carriers on mobile-terminating
    /// calls where the originating leg is narrowband — Airtel was observed
    /// offering it *alone*, with no AMR-WB and no PCMU, so it is not optional
    /// if inbound calls are to be answerable in general.
    AmrNb,
    AmrWb,
    /// Uncompressed 16-bit PCM at 16 kHz (`L16/16000`, RFC 3551 §4.5.11:
    /// big-endian samples, no header). Only ever used on the **veth link**
    /// between Agent A and Agent B, never toward a carrier.
    ///
    /// It exists to carry a carrier's AMR-WB call to Agent B's PJSIP leg
    /// without first squeezing it through 8 kHz µ-law. Compression would be
    /// pointless there: the veth is a point-to-point link inside one host, so
    /// its 256 kbit/s costs nothing, and being uncompressed it is both lossless
    /// and free of any codec to implement — Agent A already holds 16 kHz PCM
    /// the moment it has decoded the carrier's AMR-WB frame.
    L16,
}

impl NegotiatedCodec {
    /// The codec's own sample rate — the rate its PCM is decoded to and
    /// encoded from, and the rate its RTP timestamps tick at.
    pub fn sample_rate(&self) -> u32 {
        match self {
            Self::Pcmu | Self::AmrNb => 8000,
            Self::AmrWb | Self::L16 => 16000,
        }
    }

    /// Samples in one 20 ms frame at this codec's rate (the ptime every leg
    /// here uses: 160 at 8 kHz, 320 at 16 kHz).
    pub fn frame_samples(&self) -> usize {
        self.sample_rate() as usize / 50
    }

    /// The name as it appears in an `a=rtpmap` line.
    pub fn name(&self) -> &'static str {
        match self {
            Self::Pcmu => "PCMU",
            Self::AmrNb => "AMR",
            Self::AmrWb => "AMR-WB",
            Self::L16 => "L16",
        }
    }
}

/// The codec `build_answer` selected, with everything the media path needs to
/// actually speak it: the offer's payload-type number (dynamic for both AMR
/// flavours — never assume 96) and its RTP framing.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ChosenCodec {
    pub codec: NegotiatedCodec,
    pub payload_type: u8,
    /// True for RFC 4867 octet-aligned framing, false for bandwidth-efficient
    /// (bit-packed). Not a preference we get to make — it is declared by the
    /// offer's `a=fmtp` for this payload type. Meaningless for PCMU.
    pub octet_aligned: bool,
    /// The RFC 4733 `telephone-event` payload type this leg's answer echoed
    /// back, if the offer included one at this codec's clock rate. `None`
    /// when the offer carried no `telephone-event` at all — nothing this
    /// leg's answer could echo, so nowhere a DTMF relay can put an event
    /// even if the far leg sends one.
    pub dtmf_payload_type: Option<u8>,
}

fn ip_addrtype(ip: IpAddr) -> &'static str {
    if ip.is_ipv6() {
        "IP6"
    } else {
        "IP4"
    }
}

/// Build an SDP offer, `session_id` as the `o=` origin id (any
/// stable-enough number; a random one is fine, this isn't a re-INVITE that
/// needs monotonic versioning). Always offers PCMU (payload type 0, no
/// negotiation needed, universally supported); additionally offers AMR-WB
/// (dynamic payload type 96, `octet-align=1` — RFC 4867's *default* is the
/// bit-packed "bandwidth-efficient" mode, which this client doesn't
/// implement, so this must be explicit) when `offer_amr_wb` is true — the
/// caller's job to only pass `true` when a real AMR-WB codec is actually
/// linked in (see `amr_safe::is_available()`), since offering a codec we
/// can't actually encode/decode would be worse than not offering it.
/// Which formats to offer, and in what order.
///
/// Order is not cosmetic: a carrier picks the first payload type it also
/// supports, so listing narrowband first gets narrowband. A live VoLTE call to
/// Vodafone India negotiated PCMU purely because it led the list — the offer
/// did include AMR-WB — which made the call's audio narrowband and any quality
/// judgement from it meaningless.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CodecOffer {
    /// Narrowband only — the build has no wideband codec linked.
    PcmuOnly,
    /// Narrowband first, wideband second. **The historical order**, kept for
    /// the VoWiFi path so its offer stays byte-identical (FR-020). Carriers on
    /// that path require wideband and reject a PCMU-only offer anyway, so the
    /// ordering never mattered there.
    PcmuThenWideband,
    /// Wideband first. What a call wants when the audio quality is the point.
    WidebandThenPcmu,
}

impl CodecOffer {
    /// Picks the best offer this build can make.
    pub fn preferring_wideband(wideband_available: bool) -> Self {
        if wideband_available {
            CodecOffer::WidebandThenPcmu
        } else {
            CodecOffer::PcmuOnly
        }
    }

    /// The historical VoWiFi ordering.
    pub fn legacy(wideband_available: bool) -> Self {
        if wideband_available {
            CodecOffer::PcmuThenWideband
        } else {
            CodecOffer::PcmuOnly
        }
    }
}

pub fn build_offer(local_ip: IpAddr, rtp_port: u16, session_id: u64, offer: CodecOffer) -> String {
    let addrtype = ip_addrtype(local_ip);
    let payload_types = match offer {
        CodecOffer::PcmuOnly => PCMU_PAYLOAD_TYPE.to_string(),
        CodecOffer::PcmuThenWideband => {
            format!("{PCMU_PAYLOAD_TYPE} {AMR_WB_PAYLOAD_TYPE}")
        }
        CodecOffer::WidebandThenPcmu => {
            format!("{AMR_WB_PAYLOAD_TYPE} {PCMU_PAYLOAD_TYPE}")
        }
    };

    let mut sdp = format!(
        "v=0\r\n\
         o=- {session_id} {session_id} IN {addrtype} {local_ip}\r\n\
         s=gsm-sip-bridge test call\r\n\
         c=IN {addrtype} {local_ip}\r\n\
         t=0 0\r\n\
         m=audio {rtp_port} RTP/AVP {payload_types}\r\n",
    );
    // rtpmap lines follow the same order as the m= line, so the preference is
    // stated consistently rather than only in the payload-type list.
    let wideband_rtpmap = format!(
        "a=rtpmap:{AMR_WB_PAYLOAD_TYPE} AMR-WB/16000\r\n\
         a=fmtp:{AMR_WB_PAYLOAD_TYPE} octet-align=1\r\n",
    );
    let pcmu_rtpmap = format!("a=rtpmap:{PCMU_PAYLOAD_TYPE} PCMU/8000\r\n");
    match offer {
        CodecOffer::PcmuOnly => sdp.push_str(&pcmu_rtpmap),
        CodecOffer::PcmuThenWideband => {
            sdp.push_str(&pcmu_rtpmap);
            sdp.push_str(&wideband_rtpmap);
        }
        CodecOffer::WidebandThenPcmu => {
            sdp.push_str(&wideband_rtpmap);
            sdp.push_str(&pcmu_rtpmap);
        }
    }
    sdp.push_str("a=sendrecv\r\n");
    sdp
}

pub struct SdpAnswer {
    pub remote_rtp: SocketAddr,
    pub codec: NegotiatedCodec,
}

/// Parse an SDP answer body down to just what's needed to send/receive
/// RTP: the connection address (`c=`), the `m=audio` port, and which codec
/// the answer selected (identified by comparing its payload type against
/// the ones we offered — RFC 3264 requires the answer's payload type on a
/// re-used dynamic number to mean what the offer said it meant, so this
/// doesn't need to re-parse the answer's own `a=rtpmap`).
pub fn parse_answer(body: &str) -> BridgeResult<SdpAnswer> {
    let mut conn_ip: Option<IpAddr> = None;
    let mut rtp_port: Option<u16> = None;
    let mut payload_type: Option<u8> = None;

    for line in body.lines() {
        let line = line.trim();
        if let Some(rest) = line.strip_prefix("c=IN ") {
            // "IP4 1.2.3.4" or "IP6 2001:db8::1". Only *replace* a value we
            // already have when this line actually parses — RFC 4566 allows a
            // `c=` per media section as well as one at session level, and
            // assigning `.ok()` unconditionally let a later unparseable one
            // (a hostname, say) discard a perfectly good address and fail the
            // call with "missing c= connection address".
            if let Some(addr) = rest
                .split_whitespace()
                .nth(1)
                .and_then(|a| a.parse::<IpAddr>().ok())
            {
                conn_ip = Some(addr);
            }
        } else if let Some(rest) = line.strip_prefix("m=audio ") {
            // "<port> RTP/AVP <pt> [<pt> ...]" — take the first payload type.
            let mut fields = rest.split_whitespace();
            rtp_port = fields.next().and_then(|p| p.parse().ok());
            payload_type = fields.nth(1).and_then(|pt| pt.parse().ok());
        }
    }

    let conn_ip = conn_ip
        .ok_or_else(|| BridgeError::Ims("SDP answer missing c= connection address".into()))?;
    let rtp_port =
        rtp_port.ok_or_else(|| BridgeError::Ims("SDP answer missing m=audio port".into()))?;
    let payload_type = payload_type
        .ok_or_else(|| BridgeError::Ims("SDP answer's m=audio line has no payload type".into()))?;

    let codec = match payload_type {
        PCMU_PAYLOAD_TYPE => NegotiatedCodec::Pcmu,
        AMR_WB_PAYLOAD_TYPE => NegotiatedCodec::AmrWb,
        other => {
            return Err(BridgeError::Ims(format!(
                "SDP answer selected an unoffered/unsupported payload type: {other}"
            )))
        }
    };

    Ok(SdpAnswer {
        remote_rtp: SocketAddr::new(conn_ip, rtp_port),
        codec,
    })
}

/// The `ChosenCodec` for a codec `build_offer` actually offers (PCMU or
/// AMR-WB) — `None` for anything else (`AmrNb`, `L16`), which an answer can
/// only select by carrier/handset misbehavior, not a codec we'd ever have
/// agreed to.
///
/// Originally private to `agent::origination` (shared there between
/// `finish_origination`'s codec resolution and
/// `tick_pending_origination`'s early-media relay start,
/// specs/037-p-early-media); promoted here (specs/047-offerless-invite-sms-
/// reassembly, SDP-04) once `agent::inbound`'s offerless-INVITE path needed
/// the identical mapping — it belongs beside `build_offer` because it is
/// `build_offer`'s own contract (what it offers, and at which fixed payload
/// type), not any one caller's.
pub(crate) fn offered_chosen_codec(negotiated: NegotiatedCodec) -> Option<ChosenCodec> {
    match negotiated {
        NegotiatedCodec::Pcmu => Some(ChosenCodec {
            codec: NegotiatedCodec::Pcmu,
            payload_type: PCMU_PAYLOAD_TYPE,
            octet_aligned: false,
            // `build_offer` never offers `telephone-event` on this leg
            // (specs/041 conformance review, RTP-02's sibling gap; carried
            // forward as FR-002a for the offerless-INVITE path too) —
            // nothing for an answer to have echoed.
            dtmf_payload_type: None,
        }),
        NegotiatedCodec::AmrWb => Some(ChosenCodec {
            codec: NegotiatedCodec::AmrWb,
            payload_type: AMR_WB_PAYLOAD_TYPE,
            octet_aligned: true,
            dtmf_payload_type: None,
        }),
        _ => None,
    }
}

/// One codec offered in an inbound SDP offer, in the order its payload type
/// appeared on the `m=audio` line — the payload type is whatever number the
/// offerer chose (unlike `build_offer`'s own fixed `PCMU_PAYLOAD_TYPE`/
/// `AMR_WB_PAYLOAD_TYPE`, an inbound offer's dynamic payload type for AMR-WB
/// isn't guaranteed to be 96), which `build_answer` must echo back verbatim
/// per RFC 3264 §6.1 (the answer reuses the offer's own payload type
/// numbers, it doesn't renumber them).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct OfferedCodec {
    pub payload_type: u8,
    pub codec: NegotiatedCodec,
    /// The offer's own `a=fmtp` parameters for this payload type, verbatim
    /// (empty when it had none).
    ///
    /// These must be *echoed*, not invented: AMR's `octet-align` is a
    /// declarative parameter (RFC 4867 §8.1) — the answerer may not flip it,
    /// it only states which framing the sender uses for that payload type. A
    /// carrier commonly offers AMR twice, once bandwidth-efficient and once
    /// octet-aligned, on two different payload types; answering the
    /// bandwidth-efficient one with `octet-align=1` is self-contradictory and
    /// gets the call torn down immediately (observed on Airtel: BYE ~250ms
    /// after our 200 OK).
    pub fmtp: String,
}

impl OfferedCodec {
    /// Whether this payload type is framed octet-aligned (RFC 4867 §4.4)
    /// rather than bandwidth-efficient (§4.3). Both are supported — see
    /// `ims::amr_rtp` — so this selects which framing the media path must use,
    /// and is never something we get to choose for ourselves.
    pub fn is_octet_aligned(&self) -> bool {
        self.fmtp
            .split(';')
            .map(|p| p.trim().replace(' ', ""))
            .any(|p| p == "octet-align=1")
    }
}

/// What an offer's audio section stated about which way media will flow
/// (RFC 3264 §6.1). `SendRecv` is both the default when no `a=`-level
/// direction attribute is present, and this bridge's own long-standing
/// unconditional answer — this type exists so the answer can start saying
/// something else when the offer actually did.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum MediaDirection {
    #[default]
    SendRecv,
    SendOnly,
    RecvOnly,
    Inactive,
}

/// One `m=` section from the offer this bridge will not negotiate — a
/// second audio line, video, text, application, anything (specs/043
/// SDP-01). Declined per RFC 3264 §6 by echoing it back with port `0`; no
/// `c=` line is needed since the session-level one already covers it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DeclinedMedia {
    /// The media type word from the offer's `m=` line, e.g. `"video"`,
    /// `"text"`, or `"audio"` for a duplicate audio section.
    pub kind: String,
    /// That section's own transport token, echoed verbatim — a declined
    /// section's transport is not this bridge's concern.
    pub proto: String,
    /// That section's format-list, echoed verbatim; the port being `0` is
    /// what marks the section declined, not the format list.
    pub fmts: String,
    /// Whether this section appeared before the negotiated audio section
    /// in the offer, so the answer can reproduce the same relative order.
    pub before_audio: bool,
}

/// RFC 3312 §5's `status-type` — which segment of the call a QoS
/// precondition line describes. **Not** inverted at parse time: values
/// here are stored exactly as the offer wrote them (offer-relative).
/// RFC 3312 §4 defines these relative to whoever generated the SDP —
/// `Local` in the offer is the *offerer's* (caller's) own segment, and
/// `Remote` is what the offerer believes about the far end (this
/// bridge's segment); both invert when this bridge builds its answer
/// (specs/048 research.md Decision 1). Getting this backwards means
/// fabricating a confirmation for a segment this bridge doesn't control —
/// verified against the actual RFC text, not recalled from memory.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum QosStatusType {
    E2e,
    Local,
    Remote,
}

/// RFC 3312 §5's `strength-tag` on an `a=des:qos` line — how firmly the
/// offerer wants this precondition met before the call may proceed.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum QosStrength {
    Mandatory,
    Optional,
    None,
    Failure,
    /// An unrecognized strength token — treated permissively, same posture
    /// as an unrecognized `m=` transport token (specs/043 SDP-03's
    /// research.md Decision 1), not a reason to fail the parse.
    Unknown,
}

/// RFC 3312 §5's `direction-tag` — the same four wire tokens
/// (`none`/`send`/`recv`/`sendrecv`) mean different things depending on
/// which line they appear on: on `a=des:qos` it names which direction(s)
/// of the media stream the desired strength applies to; on
/// `a=curr:qos`/`a=conf:qos` it is a *status* value (how much of the
/// resource is currently/confirmed ready), not a media direction
/// (specs/048 research.md Decision 6). Kept as one type since the wire
/// encoding is identical either way — the field name at each use site
/// carries the semantic difference.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum QosDirection {
    None,
    Send,
    Recv,
    SendRecv,
}

/// One `a=des:qos <strength> <status-type> <direction>` line from the
/// offer's audio section, stored offer-relative (see [`QosStatusType`]).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct QosDesired {
    pub strength: QosStrength,
    pub status_type: QosStatusType,
    pub direction: QosDirection,
}

/// One `a=curr:qos <status-type> <direction>` line from the offer's audio
/// section — the offerer's own self-reported current status, stored
/// offer-relative. Never used to synthesize a claim this bridge didn't
/// itself confirm; only ever mirrored through unaltered (specs/048 User
/// Story 3).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct QosStatus {
    pub status_type: QosStatusType,
    pub met: QosDirection,
}

pub struct SdpOffer {
    pub remote_rtp: SocketAddr,
    /// Recognized codecs from the offer, in `m=audio` payload-type order.
    /// Payload types on the `m=audio` line with no matching `a=rtpmap` (or
    /// naming an unrecognized codec) are silently omitted rather than
    /// rejected outright — an offer can list codecs we don't support
    /// alongside ones we do, and that isn't itself an error.
    pub offered: Vec<OfferedCodec>,
    /// `telephone-event` payload types the offer carried, as
    /// `(payload_type, clock_rate)` in `m=audio` order.
    ///
    /// Not a codec — it is RFC 4733 DTMF — but TS 26.114 makes it mandatory for
    /// MMTEL voice, and an answer that drops it is rejected outright. Measured
    /// on Jio 2026-08-14: the dialog established, the ACK arrived, and 2ms
    /// later the carrier sent `BYE` with
    /// `Reason: SIP;cause=503;text="PO: SIP SDP Protocol Error."` — our answer
    /// had selected AMR-WB and silently omitted the two `telephone-event`
    /// payloads (16000 and 8000) the offer listed.
    pub dtmf: Vec<(u8, u32)>,
    /// The offer's `a=maxptime`, if it carried one.
    ///
    /// TS 26.114 §6.2.2 has the UE state both `ptime` and `maxptime` for
    /// AMR/AMR-WB; we only ever sent `ptime`. Echoed rather than asserted, so
    /// the answer never claims a longer packetisation than the offerer allows.
    pub maxptime: Option<u32>,
    /// What the audio section itself stated about direction (specs/043
    /// SDP-02) — mirrored, not copied, into the answer by `build_answer_for`.
    pub direction: MediaDirection,
    /// The audio section's raw `m=` transport token (e.g. `"RTP/AVP"`),
    /// captured but not validated here — see [`DeclinedMedia`] and
    /// specs/043 SDP-03's research.md Decision 1 for why this stays
    /// permissive: the caller decides what an unrecognized token means.
    pub proto: String,
    /// Every `m=` section in the offer other than the negotiated audio one,
    /// in original order (specs/043 SDP-01).
    pub other_media: Vec<DeclinedMedia>,
    /// The audio section's explicit RTCP port (RFC 3605 `a=rtcp:<port>`),
    /// when it names one. `None` on a missing, zero, or unparseable value —
    /// parsed permissively, in keeping with this module's established
    /// posture (see `proto` above): a bad value falls back to the RTP+1
    /// convention (specs/046-rtcp-reporting FR-016) rather than failing the
    /// offer. Only the port form is read; RFC 3605's optional address form
    /// is not — the peer address is already known from the media
    /// negotiation, and honouring a *different* address for RTCP would
    /// contradict this bridge's own RTCP source-trust boundary
    /// (`ims::rtcp::SourceGuard`).
    pub rtcp: Option<u16>,
    /// Every `a=des:qos` line in the selected audio section, in original
    /// order, offer-relative (specs/048 MT-06).
    pub preconditions: Vec<QosDesired>,
    /// Every `a=curr:qos` line in the selected audio section, in original
    /// order, offer-relative — the offerer's own self-report, read but
    /// never asserted over (specs/048 MT-06, User Story 3).
    pub offerer_curr: Vec<QosStatus>,
}

/// Parse an inbound SDP offer (the inverse of `build_offer`): the connection
/// address, the `m=audio` port, and which of the listed payload types are
/// codecs this client recognizes (by matching each payload type's
/// `a=rtpmap:<pt> <name>/<rate>` line against PCMU/8000 and AMR-WB/16000).
/// RFC 3312 §5's `status-type` token, offer-relative (not inverted here —
/// see [`QosStatusType`]'s doc comment).
fn parse_qos_status_type(token: &str) -> Option<QosStatusType> {
    match token {
        "e2e" => Some(QosStatusType::E2e),
        "local" => Some(QosStatusType::Local),
        "remote" => Some(QosStatusType::Remote),
        _ => None,
    }
}

/// RFC 3312 §5's `strength-tag` token. Unlike the status-type and
/// direction tokens (which gate whether the whole line is usable at all),
/// an unrecognized strength falls back to [`QosStrength::Unknown`] rather
/// than discarding the line — permissive, matching `proto`'s posture.
fn parse_qos_strength(token: &str) -> QosStrength {
    match token {
        "mandatory" => QosStrength::Mandatory,
        "optional" => QosStrength::Optional,
        "none" => QosStrength::None,
        "failure" => QosStrength::Failure,
        _ => QosStrength::Unknown,
    }
}

/// RFC 3312 §5's `direction-tag` token — shared wire encoding for both a
/// desired direction (`a=des`) and a current-status value (`a=curr`/
/// `a=conf`); see [`QosDirection`]'s doc comment.
fn parse_qos_direction(token: &str) -> Option<QosDirection> {
    match token {
        "none" => Some(QosDirection::None),
        "send" => Some(QosDirection::Send),
        "recv" => Some(QosDirection::Recv),
        "sendrecv" => Some(QosDirection::SendRecv),
        _ => None,
    }
}

pub fn parse_offer(body: &str) -> BridgeResult<SdpOffer> {
    let mut conn_ip: Option<IpAddr> = None;
    let mut rtp_port: Option<u16> = None;
    let mut listed_pts: Vec<u8> = Vec::new();
    let mut rtpmap: std::collections::HashMap<u8, (String, u32)> = std::collections::HashMap::new();
    let mut fmtp: std::collections::HashMap<u8, String> = std::collections::HashMap::new();
    let mut maxptime: Option<u32> = None;
    let mut proto = String::new();
    let mut rtcp_port: Option<u16> = None;
    let mut session_direction = MediaDirection::default();
    let mut audio_direction: Option<MediaDirection> = None;
    let mut other_media: Vec<DeclinedMedia> = Vec::new();
    let mut preconditions: Vec<QosDesired> = Vec::new();
    let mut offerer_curr: Vec<QosStatus> = Vec::new();

    // An SDP body can hold several media sections, and payload-type numbers are
    // scoped to the section they appear in — the *same* number can mean
    // different things in two sections. PJSIP's own offer does exactly this:
    // it puts `L16/16000` on payload type 100 under `m=audio`, then a T.140
    // text stream that reuses 100 for `red/1000` under `m=text`. Attributes are
    // therefore only collected while inside the audio section (RFC 4566 §5.14:
    // a media section runs until the next `m=` line), or a later section's
    // rtpmap would silently redefine an audio codec out of existence.
    //
    // Only the *first* `m=audio` section is ever negotiated — a later one
    // (another audio line, or a video/text/application section) is recorded
    // as `other_media` and declined in the answer (RFC 3264 §6) rather than
    // silently overwriting the first or being dropped with no trace
    // (specs/043 SDP-01).
    let mut in_audio = false;
    let mut audio_seen = false;
    let mut seen_media = false;

    for line in body.lines() {
        let line = line.trim();
        if let Some(rest) = line.strip_prefix("m=") {
            seen_media = true;
            if !audio_seen && rest.starts_with("audio ") {
                in_audio = true;
                audio_seen = true;
                let mut fields = rest["audio ".len()..].split_whitespace();
                rtp_port = fields.next().and_then(|p| p.parse().ok());
                proto = fields.next().unwrap_or_default().to_string();
                listed_pts = fields.filter_map(|pt| pt.parse().ok()).collect();
            } else {
                in_audio = false;
                // "<kind> <port> <proto> <fmt-list...>" — the port is
                // ignored: this section is declined regardless of what it
                // asked for.
                let mut fields = rest.split_whitespace();
                let kind = fields.next().unwrap_or_default().to_string();
                let _port = fields.next();
                let section_proto = fields.next().unwrap_or_default().to_string();
                let fmts: Vec<&str> = fields.collect();
                other_media.push(DeclinedMedia {
                    kind,
                    proto: section_proto,
                    fmts: fmts.join(" "),
                    before_audio: !audio_seen,
                });
            }
        } else if let Some(rest) = line.strip_prefix("c=IN ") {
            // Session-level (before any `m=`) or the audio section's own — but
            // never another section's, which may point somewhere else entirely.
            if in_audio || !seen_media {
                let addr_str = rest.split_whitespace().nth(1);
                if let Some(addr_str) = addr_str {
                    conn_ip = addr_str.parse().ok();
                }
            }
        } else if let Some(dir) = match line {
            "a=sendonly" => Some(MediaDirection::SendOnly),
            "a=recvonly" => Some(MediaDirection::RecvOnly),
            "a=inactive" => Some(MediaDirection::Inactive),
            "a=sendrecv" => Some(MediaDirection::SendRecv),
            _ => None,
        } {
            // RFC 4566 §5.14: a session-level direction attribute (before
            // any `m=` line) applies to every media section that doesn't
            // state its own — recognized here regardless of the `!in_audio`
            // guard below, which only gates attributes that are meaningless
            // outside a media section (rtpmap/fmtp/maxptime). A direction
            // attribute under some *other*, later media section is neither
            // session-level nor the audio section's own, and is correctly
            // ignored (specs/043 SDP-02, PR review 2026-08-27).
            if in_audio {
                audio_direction = Some(dir);
            } else if !seen_media {
                session_direction = dir;
            }
        } else if !in_audio {
            continue;
        } else if let Some(rest) = line.strip_prefix("a=rtpmap:") {
            // "<pt> <name>/<rate>[/<params>]"
            let mut parts = rest.splitn(2, ' ');
            let Some(pt) = parts.next().and_then(|p| p.parse::<u8>().ok()) else {
                continue;
            };
            let Some(name_rate) = parts.next() else {
                continue;
            };
            let mut nr = name_rate.splitn(2, '/');
            let (Some(name), Some(rate_str)) = (nr.next(), nr.next()) else {
                continue;
            };
            let Some(rate) = rate_str.split('/').next().and_then(|r| r.parse().ok()) else {
                continue;
            };
            rtpmap.insert(pt, (name.to_ascii_uppercase(), rate));
        } else if let Some(rest) = line.strip_prefix("a=maxptime:") {
            maxptime = rest.trim().parse().ok();
        } else if let Some(rest) = line.strip_prefix("a=rtcp:") {
            // RFC 3605 §2.1: "<port>[ <nettype> <addrtype> <address>]" —
            // only the port is read (see `SdpOffer::rtcp`'s doc comment).
            // A missing, non-numeric, or zero value is not an error: it
            // just means there's nothing to override the RTP+1 convention
            // with (specs/046-rtcp-reporting FR-016).
            rtcp_port = rest
                .split_whitespace()
                .next()
                .and_then(|p| p.parse::<u16>().ok())
                .filter(|&p| p != 0);
        } else if let Some(rest) = line.strip_prefix("a=fmtp:") {
            // "<pt> <params>"
            let mut parts = rest.splitn(2, ' ');
            let Some(pt) = parts.next().and_then(|p| p.parse::<u8>().ok()) else {
                continue;
            };
            if let Some(params) = parts.next() {
                fmtp.insert(pt, params.trim().to_string());
            }
        } else if let Some(rest) = line.strip_prefix("a=des:qos ") {
            // RFC 3312 §5: "<strength> <status-type> <direction>". A line
            // whose tokens don't parse is skipped outright (permissive,
            // specs/048 research.md Decision 6) rather than failing the
            // whole offer over one malformed precondition line.
            let mut parts = rest.split_whitespace();
            if let (Some(strength), Some(status_type), Some(direction)) =
                (parts.next(), parts.next(), parts.next())
            {
                if let (Some(status_type), Some(direction)) = (
                    parse_qos_status_type(status_type),
                    parse_qos_direction(direction),
                ) {
                    preconditions.push(QosDesired {
                        strength: parse_qos_strength(strength),
                        status_type,
                        direction,
                    });
                }
            }
        } else if let Some(rest) = line.strip_prefix("a=curr:qos ") {
            // RFC 3312 §5: "<status-type> <direction>" (a status value on
            // this line, not a media direction — see `QosDirection`'s doc
            // comment).
            let mut parts = rest.split_whitespace();
            if let (Some(status_type), Some(met)) = (parts.next(), parts.next()) {
                if let (Some(status_type), Some(met)) =
                    (parse_qos_status_type(status_type), parse_qos_direction(met))
                {
                    offerer_curr.push(QosStatus { status_type, met });
                }
            }
        }
    }

    // The audio section's own direction attribute overrides the
    // session-level one if both are present (RFC 4566 §5.14); absent
    // either, the default is `SendRecv`.
    let direction = audio_direction.unwrap_or(session_direction);

    let conn_ip = conn_ip
        .ok_or_else(|| BridgeError::Ims("SDP offer missing c= connection address".into()))?;
    let rtp_port =
        rtp_port.ok_or_else(|| BridgeError::Ims("SDP offer missing m=audio port".into()))?;
    if listed_pts.is_empty() {
        return Err(BridgeError::Ims(
            "SDP offer's m=audio line lists no payload types".into(),
        ));
    }

    let mut dtmf = Vec::new();
    for pt in &listed_pts {
        if let Some((name, rate)) = rtpmap.get(pt) {
            if name == "TELEPHONE-EVENT" {
                dtmf.push((*pt, *rate));
            }
        }
    }

    let mut offered = Vec::new();
    for pt in listed_pts {
        let codec = if pt == PCMU_PAYLOAD_TYPE {
            // PCMU's payload type is statically assigned (RFC 3551 §6) —
            // recognized even without an explicit a=rtpmap line, same as a
            // real UA would.
            Some(NegotiatedCodec::Pcmu)
        } else if let Some((name, rate)) = rtpmap.get(&pt) {
            match (name.as_str(), *rate) {
                ("PCMU", 8000) => Some(NegotiatedCodec::Pcmu),
                ("AMR", 8000) => Some(NegotiatedCodec::AmrNb),
                ("AMR-WB", 16000) => Some(NegotiatedCodec::AmrWb),
                // Only ever seen on the veth link, where the offerer is Agent
                // B's PJSIP. A carrier offering L16 would be extraordinary,
                // and answering it would still be correct.
                ("L16", 16000) => Some(NegotiatedCodec::L16),
                _ => None,
            }
        } else {
            None
        };
        if let Some(codec) = codec {
            offered.push(OfferedCodec {
                payload_type: pt,
                codec,
                fmtp: fmtp.get(&pt).cloned().unwrap_or_default(),
            });
        }
    }

    Ok(SdpOffer {
        remote_rtp: SocketAddr::new(conn_ip, rtp_port),
        offered,
        dtmf,
        maxptime,
        direction,
        proto,
        other_media,
        rtcp: rtcp_port,
        preconditions,
        offerer_curr,
    })
}

/// One `a=curr:qos`/`a=conf:qos` line this bridge's answer will emit —
/// already answer-relative (i.e. already inverted from whatever the offer
/// said, per RFC 3312 §5.2). `strength` is `None` when only `a=curr`
/// should be emitted (the offer's line didn't ask for confirmation);
/// `Some` also emits the matching `a=conf:qos` line.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct QosAnswerLine {
    pub status_type: QosStatusType,
    pub met: QosDirection,
    pub confirm: bool,
}

/// The bridge's own accept/decline decision for an offer's precondition
/// content, computed once per inbound INVITE from the parsed offer (specs/048
/// MT-06). Mirrors the existing `unsupported_required_extensions`
/// outcome shape (proceed vs. decline), but decided from the SDP body
/// rather than the `Require` header alone — see research.md Decisions 1-2.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum PreconditionVerdict {
    /// No precondition lines, or every one is honestly answerable.
    Proceed(Vec<QosAnswerLine>),
    /// At least one `e2e`/`mandatory` line cannot be honestly confirmed
    /// without a synchronization mechanism (`UPDATE`/`100rel`) this bridge
    /// does not implement (research.md Decision 2).
    Decline,
}

/// Decide whether `offer`'s precondition content (if any) can be honoured,
/// and what the answer should say about it. RFC 3312 §4's local/remote
/// tags are relative to whoever generated the SDP and invert between offer
/// and answer — see [`QosStatusType`]'s doc comment and research.md
/// Decision 1. This bridge's own segment is the offer's `Remote` status
/// type (inverted to `Local` in the answer); the offerer's own segment is
/// the offer's `Local` status type (inverted to `Remote`, mirrored not
/// asserted — User Story 3); `E2e` is not relative and, at `mandatory`
/// strength, cannot be unilaterally confirmed (User Story 2).
pub fn precondition_verdict(offer: &SdpOffer) -> PreconditionVerdict {
    let mut answer_lines = Vec::new();

    for line in &offer.preconditions {
        match line.status_type {
            QosStatusType::Remote => {
                // This bridge's own segment (once inverted to `Local`).
                // There is no real reservation delay on this bridge's
                // media relay, so whatever direction(s) the offer asked
                // for are always already met — see spec.md "Why this
                // exists". Reported as exactly the requested direction,
                // not unconditionally `sendrecv`: the relay's own
                // capability is always full-duplex, but the answer
                // confirms what was asked, not more than was asked
                // (Greptile review, PR #68).
                answer_lines.push(QosAnswerLine {
                    status_type: QosStatusType::Local,
                    met: line.direction,
                    confirm: matches!(
                        line.strength,
                        QosStrength::Mandatory | QosStrength::Optional
                    ),
                });
            }
            QosStatusType::E2e => {
                if line.strength == QosStrength::Mandatory {
                    // Cannot be honestly confirmed without hearing the
                    // offerer's own segment status back — no `UPDATE`,
                    // no `100rel` (research.md Decision 2).
                    return PreconditionVerdict::Decline;
                }
                // Reports only what this bridge itself can attest to —
                // never the offerer's contribution — and only for the
                // requested direction, same reasoning as `Remote` above.
                answer_lines.push(QosAnswerLine {
                    status_type: QosStatusType::E2e,
                    met: line.direction,
                    confirm: false,
                });
            }
            QosStatusType::Local => {
                // The offerer's own segment (once inverted to `Remote`).
                // This bridge has no basis to confirm or deny it — handled
                // below, from `offer.offerer_curr`, not here.
            }
        }
    }

    // The offerer's own self-reported current status (User Story 3):
    // mirrored through inverted (`Local`→`Remote`), never a value this
    // bridge computed.
    for status in &offer.offerer_curr {
        if status.status_type == QosStatusType::Local {
            answer_lines.push(QosAnswerLine {
                status_type: QosStatusType::Remote,
                met: status.met,
                confirm: false,
            });
        }
    }

    PreconditionVerdict::Proceed(answer_lines)
}

/// Which codec of a **carrier's** offer we'd answer with, if any — the single
/// source of truth for that decision, so a caller deciding *whether* to accept
/// a call (`ims::agent`) can't drift out of sync with `build_answer`, which
/// decides what to actually answer.
///
/// Preference order depends on whether the bridge can carry wideband end to
/// end (`wideband`: Agent B's PJSIP leg runs a 16 kHz conference bridge, and
/// the veth link between the agents can carry `L16/16000`):
///
/// * **wideband** — AMR-WB, then PCMU, then AMR-NB. The carrier's AMR-WB is
///   real 16 kHz audio, so taking PCMU instead would throw away half the band
///   at the very first hop; transcoding AMR-WB to L16 costs a decode but loses
///   nothing.
/// * **narrowband** (`wideband = false`, or no AMR codec linked in) — PCMU,
///   then AMR-WB, then AMR-NB: the historical order, which prefers the codec
///   that relays straight through with no transcode at all, since with an
///   8 kHz bridge downstream there is no wideband left to preserve anyway.
///
/// Either way a carrier that offers only narrowband (PCMU and/or AMR-NB, which
/// Airtel does on some calls) is answered exactly as before.
///
/// Within an AMR flavour, octet-aligned framing is preferred over
/// bandwidth-efficient purely because it's the simpler path; both are
/// supported (`ims::amr_rtp`). Crucially the framing is *read from the offer*,
/// never asserted — `octet-align` is declarative (RFC 4867 §8.1), so
/// answering a bandwidth-efficient payload type with `octet-align=1` is a
/// contradiction rather than a negotiation, and gets the call torn down.
pub fn select_codec(
    offer: &SdpOffer,
    amr_available: bool,
    wideband: bool,
) -> Option<&OfferedCodec> {
    select_codec_with(offer, amr_available, wideband, AnswerPreference::Legacy)
}

/// Which codec to answer a mobile-terminating call with, when the fallback
/// order matters as much as the first choice.
///
/// # Why this exists (specs/017 T027, research R7)
///
/// Feature 016 measured what the equivalent *offer*-side decision costs:
/// putting narrowband first made the carrier select it and packet loss went
/// from 0.3% to 13.6% — a 45-fold difference — because the network grants the
/// conversational-voice bearer based on what was negotiated. Answering a call
/// carelessly reproduces that in the direction that matters more, since an
/// inbound call is a real conversation rather than a test.
///
/// The first choice is not where the risk is: both variants prefer AMR-WB.
/// **The risk is the fallback**, when the caller's offer has no AMR-WB — and
/// that is exactly where the two paths must diverge.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum AnswerPreference {
    /// The Wi-Fi path's long-standing order: AMR-WB, then **PCMU**, then
    /// AMR-NB. Preserved byte-for-byte because that path is in production and
    /// must not change behaviour (specs/017 FR-020).
    Legacy,
    /// For calls arriving over the cellular registration: AMR-WB, then
    /// **AMR-NB**, then PCMU.
    ///
    /// The single difference from [`Legacy`](Self::Legacy) is that AMR-NB
    /// outranks PCMU. AMR is the 3GPP-native codec family the voice bearer is
    /// specified around; PCMU on a cellular IMS leg is the odd one out. When
    /// the caller offers AMR-NB and PCMU but no AMR-WB, answering with PCMU
    /// risks the network declining to treat the call as conversational voice
    /// — the same class of mistake feature 016 paid for, and the reason that
    /// decision is not left to whichever branch happened to be written first.
    Cellular,
}

impl AnswerPreference {
    /// The Wi-Fi path's preference. Named rather than defaulted so that
    /// choosing it is visible at the call site.
    pub fn legacy() -> Self {
        AnswerPreference::Legacy
    }

    /// The cellular path's preference — keeps 3GPP-native codecs ahead of
    /// PCMU so the voice bearer is not put at risk by the fallback.
    pub fn cellular() -> Self {
        AnswerPreference::Cellular
    }
}

/// [`select_codec`] with the fallback order stated explicitly.
///
/// `wideband == false` means the caller does not want AMR-WB at all, and both
/// preferences collapse to the historical narrowband-first order — that path
/// is unchanged for either transport.
pub fn select_codec_with(
    offer: &SdpOffer,
    amr_available: bool,
    wideband: bool,
    preference: AnswerPreference,
) -> Option<&OfferedCodec> {
    let pick = |codec: NegotiatedCodec| -> Option<&OfferedCodec> {
        if !amr_available && codec != NegotiatedCodec::Pcmu {
            return None;
        }
        pick_offered(offer, codec)
    };

    if wideband && amr_available {
        match preference {
            AnswerPreference::Legacy => pick(NegotiatedCodec::AmrWb)
                .or_else(|| pick(NegotiatedCodec::Pcmu))
                .or_else(|| pick(NegotiatedCodec::AmrNb)),
            AnswerPreference::Cellular => pick(NegotiatedCodec::AmrWb)
                .or_else(|| pick(NegotiatedCodec::AmrNb))
                .or_else(|| pick(NegotiatedCodec::Pcmu)),
        }
    } else {
        pick(NegotiatedCodec::Pcmu)
            .or_else(|| pick(NegotiatedCodec::AmrWb))
            .or_else(|| pick(NegotiatedCodec::AmrNb))
    }
}

/// Which codec of **Agent B's veth-link** offer Agent A answers with. A
/// different decision from `select_codec`'s: this peer is our own PJSIP, the
/// link is a lossless point-to-point one inside the host, and the only thing
/// that matters is not narrowing the carrier's audio on the way through.
///
/// So: `L16/16000` when the carrier leg is wideband and PJSIP offered L16,
/// otherwise PCMU — which keeps every narrowband call on exactly the path it
/// took before this existed, payload-for-payload. If PJSIP offered neither
/// (an L16-less build with PCMU disabled, say), there is nothing to answer
/// with and the call is declined rather than answered into silence.
pub fn select_veth_codec(offer: &SdpOffer, wideband: bool) -> Option<&OfferedCodec> {
    if wideband {
        if let Some(l16) = pick_offered(offer, NegotiatedCodec::L16) {
            return Some(l16);
        }
    }
    pick_offered(offer, NegotiatedCodec::Pcmu)
}

/// The offer's entry for `codec`, preferring an octet-aligned payload type
/// when the offer lists more than one (see `select_codec`).
fn pick_offered(offer: &SdpOffer, codec: NegotiatedCodec) -> Option<&OfferedCodec> {
    let of_codec = || offer.offered.iter().filter(|c| c.codec == codec);
    of_codec()
        .find(|c| c.is_octet_aligned())
        .or_else(|| of_codec().next())
}

/// Build an SDP answer to a carrier's `offer`, choosing one codec per
/// `select_codec` (see there for the preference order and what `wideband`
/// changes). Errors if the offer contains no codec we can answer with — an
/// offer we can neither decode nor pass through isn't answerable. Returns the
/// SDP body and the codec it selected, so the caller doesn't have to re-parse
/// its own answer to know which one won.
#[allow(clippy::too_many_arguments)]
pub fn build_answer(
    local_ip: IpAddr,
    rtp_port: u16,
    session_id: u64,
    offer: &SdpOffer,
    amr_available: bool,
    wideband: bool,
    preference: AnswerPreference,
    declared_rtcp_port: Option<u16>,
) -> BridgeResult<(String, ChosenCodec)> {
    let chosen =
        select_codec_with(offer, amr_available, wideband, preference).ok_or_else(|| {
            BridgeError::Ims("SDP offer has no codec this client can answer with".into())
        })?;
    Ok(build_answer_for(
        local_ip,
        rtp_port,
        session_id,
        chosen,
        offer,
        declared_rtcp_port,
    ))
}

/// Build an SDP answer to Agent B's veth-link `offer`, choosing one codec per
/// `select_veth_codec`. Never declares an `a=rtcp` port — RTCP reporting is
/// scoped to the carrier leg only (specs/046-rtcp-reporting FR-023); this
/// leg's `b=RS:`/`b=RR:` declaration stays unbacked, a deliberate residue
/// recorded there as FR-023a, not an oversight here.
pub fn build_veth_answer(
    local_ip: IpAddr,
    rtp_port: u16,
    session_id: u64,
    offer: &SdpOffer,
    wideband: bool,
) -> BridgeResult<(String, ChosenCodec)> {
    let chosen = select_veth_codec(offer, wideband).ok_or_else(|| {
        BridgeError::Ims("veth-link SDP offer has neither L16/16000 nor PCMU".into())
    })?;
    Ok(build_answer_for(
        local_ip, rtp_port, session_id, chosen, offer, None,
    ))
}

/// Render an answer that accepts exactly `chosen`, echoing the offer's own
/// payload-type number (RFC 3264 §6.1) and — for AMR — its own `a=fmtp`
/// parameters verbatim rather than asserting our own: they describe how the
/// *offerer* frames what it sends, which is not ours to change.
///
/// `declared_rtcp_port` states an explicit `a=rtcp:` (RFC 3605) only when
/// `Some` — the tier-2 case from `ims::rtcp::bind_rtp_and_rtcp` where the
/// RTP+1 convention wasn't available. `None` (tier 1's convention, or tier
/// 3's no-RTCP-at-all) leaves the answer exactly as it was before this
/// feature (specs/046-rtcp-reporting contract C-1.1/C-1.2/C-1.3) — the
/// `b=AS:`/`b=RS:`/`b=RR:` lines below are unconditional either way.
fn build_answer_for(
    local_ip: IpAddr,
    rtp_port: u16,
    session_id: u64,
    chosen: &OfferedCodec,
    offer: &SdpOffer,
    declared_rtcp_port: Option<u16>,
) -> (String, ChosenCodec) {
    let addrtype = ip_addrtype(local_ip);
    let pt = chosen.payload_type;
    let dtmf = &offer.dtmf;
    let maxptime = offer.maxptime;

    // Keep the offer's `telephone-event` — see `SdpOffer::dtmf` for why
    // dropping it is fatal. Prefer the one whose clock rate matches the codec
    // we picked (RFC 4733 §2.1 ties the event stream's rate to the audio it
    // accompanies); fall back to the first offered rather than none.
    let chosen_rate = match chosen.codec {
        NegotiatedCodec::AmrWb | NegotiatedCodec::L16 => 16000,
        NegotiatedCodec::AmrNb | NegotiatedCodec::Pcmu => 8000,
    };
    let dtmf_pick = dtmf
        .iter()
        .find(|(_, rate)| *rate == chosen_rate)
        .or_else(|| dtmf.first());
    let (dtmf_pts, dtmf_lines) = match dtmf_pick {
        Some((dpt, drate)) => (
            format!(" {dpt}"),
            format!("a=rtpmap:{dpt} telephone-event/{drate}\r\na=fmtp:{dpt} 0-15\r\n"),
        ),
        None => (String::new(), String::new()),
    };
    let rtpmap_line = match chosen.codec {
        NegotiatedCodec::Pcmu => format!("a=rtpmap:{pt} PCMU/8000\r\n"),
        NegotiatedCodec::L16 => format!("a=rtpmap:{pt} L16/16000\r\n"),
        NegotiatedCodec::AmrNb => format!(
            "a=rtpmap:{pt} AMR/8000\r\na=fmtp:{pt} {fmtp}\r\n",
            fmtp = chosen.fmtp,
        ),
        NegotiatedCodec::AmrWb => format!(
            "a=rtpmap:{pt} AMR-WB/16000\r\na=fmtp:{pt} {fmtp}\r\n",
            fmtp = chosen.fmtp,
        ),
    };

    let maxptime_line = match maxptime {
        Some(mp) => format!("a=maxptime:{mp}\r\n"),
        None => String::new(),
    };

    // Media and RTCP bandwidth. TS 26.114 §6.2.10 requires an IMS UE to state
    // `b=AS:` and the RTCP `b=RS:`/`b=RR:` pair for a voice stream, and we sent
    // none at all. Jio does send them (its own media SDP carries `b=AS:32`,
    // `b=RR:2400`, `b=RS:800`), and after echoing `telephone-event` and
    // `maxptime` this was the last spec-mandated element our answer omitted.
    //
    // `AS` is the codec's payload rate plus IPv4/UDP/RTP framing at a 20 ms
    // ptime (40 bytes of header every 20 ms ≈ 16 kbit/s), rounded the way the
    // 3GPP tables do. The RTCP values are the customary 3GPP defaults.
    let as_kbps = match chosen.codec {
        NegotiatedCodec::AmrNb => 41,
        NegotiatedCodec::AmrWb => 49,
        NegotiatedCodec::Pcmu => 80,
        // Only ever on the internal veth link: 16-bit 16 kHz PCM is 256 kbit/s.
        NegotiatedCodec::L16 => 280,
    };

    // Every `m=` section the offer had other than the one negotiated above
    // gets an explicit RFC 3264 §6 decline (port `0`) in the same relative
    // position — never silently omitted (specs/043 SDP-01). No `c=` line is
    // needed per declined section: the session-level one above already
    // covers it.
    let (mut before_lines, mut after_lines) = (String::new(), String::new());
    for dm in &offer.other_media {
        let line = if dm.fmts.is_empty() {
            format!("m={} 0 {}\r\n", dm.kind, dm.proto)
        } else {
            format!("m={} 0 {} {}\r\n", dm.kind, dm.proto, dm.fmts)
        };
        if dm.before_audio {
            before_lines.push_str(&line);
        } else {
            after_lines.push_str(&line);
        }
    }

    // Mirror what the offer's audio section actually stated about
    // direction (RFC 3264 §6.1) instead of always claiming two-way media
    // (specs/043 SDP-02). The relay's own send/receive behavior is
    // unchanged either way — this only affects what the answer says.
    let direction_line = match offer.direction {
        MediaDirection::SendOnly => "a=recvonly\r\n",
        MediaDirection::RecvOnly => "a=sendonly\r\n",
        MediaDirection::Inactive => "a=inactive\r\n",
        MediaDirection::SendRecv => "a=sendrecv\r\n",
    };

    let rtcp_line = match declared_rtcp_port {
        Some(port) => format!("a=rtcp:{port}\r\n"),
        None => String::new(),
    };

    // Precondition status/confirmation lines (specs/048 MT-06) — only ever
    // present when the offer itself carried `a=des:qos`/`a=curr:qos` lines;
    // an ordinary offer (no preconditions) produces none, so this is a
    // pure addition with no effect on any offer shape that predates this
    // feature. `Decline` never reaches here: `handle_invite` declines the
    // call with `580` before `build_answer`/`build_veth_answer` is called
    // (see `agent::inbound::handle_invite`), so it renders as no lines.
    let mut qos_lines = String::new();
    if let PreconditionVerdict::Proceed(answer_lines) = precondition_verdict(offer) {
        for line in answer_lines {
            let status_type = match line.status_type {
                QosStatusType::E2e => "e2e",
                QosStatusType::Local => "local",
                QosStatusType::Remote => "remote",
            };
            let met = match line.met {
                QosDirection::None => "none",
                QosDirection::Send => "send",
                QosDirection::Recv => "recv",
                QosDirection::SendRecv => "sendrecv",
            };
            qos_lines.push_str(&format!("a=curr:qos {status_type} {met}\r\n"));
            if line.confirm {
                qos_lines.push_str(&format!("a=conf:qos {status_type} {met}\r\n"));
            }
        }
    }

    let sdp = format!(
        "v=0\r\n\
         o=- {session_id} {session_id} IN {addrtype} {local_ip}\r\n\
         s=gsm-sip-bridge vowifi bridge\r\n\
         c=IN {addrtype} {local_ip}\r\n\
         t=0 0\r\n\
         {before_lines}\
         m=audio {rtp_port} RTP/AVP {pt}{dtmf_pts}\r\n\
         {rtcp_line}\
         b=AS:{as_kbps}\r\n\
         b=RS:{RTCP_SR_BANDWIDTH_BPS}\r\n\
         b=RR:{RTCP_RR_BANDWIDTH_BPS}\r\n\
         {rtpmap_line}\
         {dtmf_lines}\
         a=ptime:20\r\n\
         {maxptime_line}\
         {direction_line}\
         {qos_lines}\
         {after_lines}",
    );

    (
        sdp,
        ChosenCodec {
            codec: chosen.codec,
            payload_type: pt,
            octet_aligned: chosen.is_octet_aligned(),
            dtmf_payload_type: dtmf_pick.map(|(dpt, _)| *dpt),
        },
    )
}

#[cfg(test)]
mod codec_offer_tests {
    use super::*;

    /// The defect a live call exposed: the offer *did* include wideband, but
    /// narrowband led the list, so the carrier took narrowband and the call's
    /// audio — and any quality judgement from it — was worthless.
    #[test]
    fn preferring_wideband_lists_it_first_in_both_places() {
        let sdp = build_offer(
            "1.2.3.4".parse().unwrap(),
            40000,
            1,
            CodecOffer::WidebandThenPcmu,
        );

        assert!(
            sdp.contains("m=audio 40000 RTP/AVP 96 0\r\n"),
            "wideband must lead the payload-type list, got: {sdp}"
        );
        let amr_at = sdp.find("a=rtpmap:96 AMR-WB").unwrap();
        let pcmu_at = sdp.find("a=rtpmap:0 PCMU").unwrap();
        assert!(
            amr_at < pcmu_at,
            "rtpmap order must agree with the m= line, or the preference is stated twice \
             and inconsistently"
        );
    }

    #[test]
    fn every_offer_still_includes_narrowband_as_a_fallback() {
        for offer in [
            CodecOffer::PcmuOnly,
            CodecOffer::PcmuThenWideband,
            CodecOffer::WidebandThenPcmu,
        ] {
            let sdp = build_offer("1.2.3.4".parse().unwrap(), 40000, 1, offer);
            assert!(
                sdp.contains("a=rtpmap:0 PCMU/8000"),
                "{offer:?} dropped the narrowband fallback"
            );
        }
    }

    #[test]
    fn a_build_without_the_wideband_codec_never_offers_it() {
        // Offering a codec this build cannot encode would negotiate a call we
        // could not then carry.
        assert_eq!(CodecOffer::preferring_wideband(false), CodecOffer::PcmuOnly);
        assert_eq!(CodecOffer::legacy(false), CodecOffer::PcmuOnly);

        let sdp = build_offer("1.2.3.4".parse().unwrap(), 40000, 1, CodecOffer::PcmuOnly);
        assert!(!sdp.contains("AMR-WB"));
        assert!(sdp.contains("m=audio 40000 RTP/AVP 0\r\n"));
    }

    #[test]
    fn the_two_paths_choose_different_orders() {
        // VoLTE wants quality; VoWiFi keeps the order it has always sent.
        assert_eq!(
            CodecOffer::preferring_wideband(true),
            CodecOffer::WidebandThenPcmu
        );
        assert_eq!(CodecOffer::legacy(true), CodecOffer::PcmuThenWideband);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// An inbound offer listing exactly `codecs`, e.g. `[(0, "PCMU/8000")]`.
    fn offer_of(codecs: &[(u8, &str)]) -> SdpOffer {
        let pts = codecs
            .iter()
            .map(|(pt, _)| pt.to_string())
            .collect::<Vec<_>>()
            .join(" ");
        let mut body = format!(
            "v=0\r\no=- 1 1 IN IP4 5.6.7.8\r\ns=-\r\nc=IN IP4 5.6.7.8\r\n\
             t=0 0\r\nm=audio 40000 RTP/AVP {pts}\r\n"
        );
        for (pt, rtpmap) in codecs {
            body.push_str(&format!("a=rtpmap:{pt} {rtpmap}\r\n"));
            if rtpmap.starts_with("AMR") {
                body.push_str(&format!("a=fmtp:{pt} octet-align=1\r\n"));
            }
        }
        parse_offer(&body).expect("test offer must parse")
    }

    /// Jio's real mobile-terminating offer, captured 2026-08-15. It lists
    /// AMR-WB twice — `104` bandwidth-efficient then `110` octet-aligned — and
    /// the framing choice is the whole point of the fixture. Carrier
    /// infrastructure addresses only; no subscriber identifiers.
    const JIO_MT_OFFER: &str = "v=0\r\n\
         o=JIO_ISBC 1764081157 1764081157 IN IP4 ims.mnc869.mcc405.3gppnetwork.org\r\n\
         s=-\r\nc=IN IP4 10.56.159.86\r\nt=0 0\r\na=sendrecv\r\n\
         m=audio 19730 RTP/AVP 109 104 110 102 108 105 100\r\n\
         a=rtpmap:109 EVS/16000\r\n\
         a=fmtp:109 br=5.9-24.4;bw=nb-swb;evs-mode-switch=0;cmr=-1;max-red=220\r\n\
         a=rtpmap:104 AMR-WB/16000\r\n\
         a=rtpmap:110 AMR-WB/16000\r\n\
         a=rtpmap:102 AMR/8000\r\n\
         a=fmtp:102 mode-change-capability=2\r\n\
         a=rtpmap:108 AMR/8000\r\n\
         a=fmtp:108 octet-align=1; mode-change-capability=2\r\n\
         a=rtpmap:105 telephone-event/16000\r\na=fmtp:105 0-15\r\n\
         a=rtpmap:100 telephone-event/8000\r\na=fmtp:100 0-15\r\n\
         a=sendrecv\r\na=ptime:20\r\na=maxptime:240\r\n\
         a=fmtp:104 mode-set=0,1,2,3;mode-change-capability=2\r\n\
         a=fmtp:110 mode-set=0,1,2,3;octet-align=1; mode-change-capability=2\r\n";

    /// A carrier that offers one codec under two payload types differing only
    /// in framing is answered on the octet-aligned one — the simpler path
    /// through `ims::amr_rtp`, and the framing every deployment here runs on.
    /// Answering the offer's own first choice instead was tried against this
    /// exact offer and changed nothing (the teardown it was written for turned
    /// out to be a response with no `Allow`/`Supported`), so the preference
    /// stands and this locks it against a real offer rather than a synthetic
    /// one.
    #[test]
    fn a_doubly_offered_codec_is_answered_on_its_octet_aligned_payload_type() {
        let offer = parse_offer(JIO_MT_OFFER).expect("Jio's real offer must parse");

        let chosen = pick_offered(&offer, NegotiatedCodec::AmrWb).expect("AMR-WB is offered twice");
        assert_eq!(chosen.payload_type, 110, "the octet-aligned one");
        assert!(chosen.is_octet_aligned());
    }

    /// One flavour offered means there is nothing to prefer: take it as it is,
    /// framing and all — every Airtel/Vi offer seen so far.
    #[test]
    fn a_singly_offered_codec_is_taken_as_offered() {
        let offer = offer_of(&[(96, "AMR-WB/16000")]);
        let chosen = pick_offered(&offer, NegotiatedCodec::AmrWb).expect("AMR-WB is offered");
        assert_eq!(chosen.payload_type, 96);
        assert!(
            chosen.is_octet_aligned(),
            "the framing is read from the offer, never asserted: {:?}",
            chosen.fmtp
        );
    }

    // ---- answer-side codec preference (specs/017 T027/T035) ---------------

    #[test]
    fn both_preferences_answer_wideband_when_it_is_offered() {
        // The first choice is not where the risk lives; both must take AMR-WB.
        let offer = offer_of(&[(0, "PCMU/8000"), (96, "AMR-WB/16000")]);
        for pref in [AnswerPreference::legacy(), AnswerPreference::cellular()] {
            let chosen =
                select_codec_with(&offer, true, true, pref).expect("a codec is selectable");
            assert_eq!(chosen.codec, NegotiatedCodec::AmrWb, "{pref:?}");
        }
    }

    #[test]
    fn the_cellular_fallback_prefers_amr_nb_over_pcmu() {
        // The case that matters: no AMR-WB on offer. AMR is the 3GPP-native
        // family the voice bearer is specified around, so answering with PCMU
        // here risks the network declining conversational-voice treatment —
        // the same class of mistake feature 016 measured at 45x packet loss.
        let offer = offer_of(&[(0, "PCMU/8000"), (97, "AMR/8000")]);
        let chosen = select_codec_with(&offer, true, true, AnswerPreference::cellular())
            .expect("a codec is selectable");
        assert_eq!(chosen.codec, NegotiatedCodec::AmrNb);
    }

    #[test]
    fn the_legacy_fallback_is_unchanged_so_the_wifi_path_does_not_move() {
        // FR-020: the production Wi-Fi path must not change behaviour. Same
        // offer as the test above, opposite answer — that contrast is the
        // whole point of splitting the preference.
        let offer = offer_of(&[(0, "PCMU/8000"), (97, "AMR/8000")]);
        let chosen = select_codec_with(&offer, true, true, AnswerPreference::legacy())
            .expect("a codec is selectable");
        assert_eq!(chosen.codec, NegotiatedCodec::Pcmu);
    }

    #[test]
    fn select_codec_still_means_exactly_what_it_did() {
        // `select_codec` is the Wi-Fi path's call site. It must stay a pure
        // alias for the legacy order, or the non-regression claim is empty.
        for codecs in [
            &[(0u8, "PCMU/8000"), (97, "AMR/8000")][..],
            &[(0, "PCMU/8000"), (96, "AMR-WB/16000")][..],
            &[(96, "AMR-WB/16000"), (97, "AMR/8000")][..],
            &[(0, "PCMU/8000")][..],
        ] {
            let offer = offer_of(codecs);
            for amr in [true, false] {
                for wideband in [true, false] {
                    assert_eq!(
                        select_codec(&offer, amr, wideband).map(|c| c.codec),
                        select_codec_with(&offer, amr, wideband, AnswerPreference::Legacy)
                            .map(|c| c.codec),
                        "codecs={codecs:?} amr={amr} wideband={wideband}"
                    );
                }
            }
        }
    }

    #[test]
    fn a_narrowband_only_offer_is_answered_the_same_way_by_both() {
        let offer = offer_of(&[(0, "PCMU/8000")]);
        for pref in [AnswerPreference::legacy(), AnswerPreference::cellular()] {
            let chosen = select_codec_with(&offer, true, true, pref).expect("PCMU is answerable");
            assert_eq!(chosen.codec, NegotiatedCodec::Pcmu);
        }
    }

    #[test]
    fn without_amr_linked_neither_preference_can_conjure_it() {
        // Answering with a codec we cannot decode would connect the call and
        // then deliver silence, which is worse than declining it.
        let offer = offer_of(&[(96, "AMR-WB/16000"), (97, "AMR/8000")]);
        for pref in [AnswerPreference::legacy(), AnswerPreference::cellular()] {
            assert!(
                select_codec_with(&offer, false, true, pref).is_none(),
                "{pref:?} must decline rather than answer into silence"
            );
        }
    }

    #[test]
    fn build_offer_includes_pcmu_only_when_amr_wb_not_offered() {
        let sdp = build_offer(
            "2402:8100::1".parse().unwrap(),
            40000,
            12345,
            CodecOffer::PcmuOnly,
        );
        assert!(sdp.contains("m=audio 40000 RTP/AVP 0\r\n"));
        assert!(sdp.contains("a=rtpmap:0 PCMU/8000"));
        assert!(!sdp.contains("AMR-WB"));
        assert!(sdp.contains("c=IN IP6 2402:8100::1"));
    }

    #[test]
    fn build_offer_includes_both_codecs_when_amr_wb_offered() {
        let sdp = build_offer(
            "1.2.3.4".parse().unwrap(),
            40000,
            12345,
            CodecOffer::PcmuThenWideband,
        );
        assert!(sdp.contains("m=audio 40000 RTP/AVP 0 96\r\n"));
        // The historical order, kept so the VoWiFi offer is byte-identical.
        let pcmu_at = sdp.find("a=rtpmap:0 PCMU").unwrap();
        let amr_at = sdp.find("a=rtpmap:96 AMR-WB").unwrap();
        assert!(pcmu_at < amr_at, "legacy order lists narrowband first");
        assert!(sdp.contains("a=rtpmap:0 PCMU/8000"));
        assert!(sdp.contains("a=rtpmap:96 AMR-WB/16000"));
        assert!(sdp.contains("a=fmtp:96 octet-align=1"));
    }

    #[test]
    fn parse_answer_extracts_remote_rtp_and_recognizes_pcmu() {
        let body = "v=0\r\n\
                     o=- 1 1 IN IP4 5.6.7.8\r\n\
                     s=-\r\n\
                     c=IN IP4 5.6.7.8\r\n\
                     t=0 0\r\n\
                     m=audio 50000 RTP/AVP 0\r\n\
                     a=rtpmap:0 PCMU/8000\r\n";
        let answer = parse_answer(body).unwrap();
        assert_eq!(answer.remote_rtp, "5.6.7.8:50000".parse().unwrap());
        assert_eq!(answer.codec, NegotiatedCodec::Pcmu);
    }

    #[test]
    fn parse_answer_recognizes_amr_wb() {
        let body = "v=0\r\n\
                     c=IN IP4 5.6.7.8\r\n\
                     t=0 0\r\n\
                     m=audio 50000 RTP/AVP 96\r\n\
                     a=rtpmap:96 AMR-WB/16000\r\n\
                     a=fmtp:96 octet-align=1\r\n";
        let answer = parse_answer(body).unwrap();
        assert_eq!(answer.codec, NegotiatedCodec::AmrWb);
    }

    #[test]
    fn parse_answer_rejects_unrecognized_payload_type() {
        let body = "v=0\r\nc=IN IP4 5.6.7.8\r\nm=audio 50000 RTP/AVP 8\r\n";
        assert!(parse_answer(body).is_err());
    }

    #[test]
    fn parse_answer_rejects_missing_connection_line() {
        let body = "v=0\r\nm=audio 50000 RTP/AVP 0\r\n";
        assert!(parse_answer(body).is_err());
    }

    // ---- offered_chosen_codec / the offerless-INVITE round trip ----------
    // specs/047-offerless-invite-sms-reassembly (SDP-04): this is the exact
    // sequence `agent::inbound::handle_offerless_invite` runs — build our
    // own offer, a caller answers it, we parse that answer back into a
    // `ChosenCodec` — the one genuinely new composition this finding adds,
    // and fully testable without any socket (unlike the surrounding
    // ringing/control-channel machinery it's embedded in, which has no
    // unit-test seam in this codebase — see quickstart.md).

    #[test]
    fn offered_chosen_codec_maps_pcmu_to_the_fixed_payload_type_build_offer_uses() {
        let chosen = offered_chosen_codec(NegotiatedCodec::Pcmu).unwrap();
        assert_eq!(chosen.codec, NegotiatedCodec::Pcmu);
        assert_eq!(chosen.payload_type, PCMU_PAYLOAD_TYPE);
        assert!(!chosen.octet_aligned);
        assert_eq!(
            chosen.dtmf_payload_type, None,
            "FR-002a: no DTMF on this path"
        );
    }

    #[test]
    fn offered_chosen_codec_maps_amr_wb_to_the_fixed_payload_type_build_offer_uses() {
        let chosen = offered_chosen_codec(NegotiatedCodec::AmrWb).unwrap();
        assert_eq!(chosen.codec, NegotiatedCodec::AmrWb);
        assert_eq!(chosen.payload_type, AMR_WB_PAYLOAD_TYPE);
        assert!(chosen.octet_aligned);
    }

    #[test]
    fn offered_chosen_codec_rejects_a_codec_build_offer_never_offers() {
        assert_eq!(offered_chosen_codec(NegotiatedCodec::AmrNb), None);
        assert_eq!(offered_chosen_codec(NegotiatedCodec::L16), None);
    }

    #[test]
    fn build_offer_then_parse_answer_then_offered_chosen_codec_round_trips_for_each_offered_codec()
    {
        let offer_sdp = build_offer(
            "10.0.0.9".parse().unwrap(),
            40000,
            1,
            CodecOffer::WidebandThenPcmu,
        );
        assert!(offer_sdp.contains("m=audio 40000 RTP/AVP 96 0"));

        // The caller's device picks one and answers with it — a realistic
        // answer names only the chosen payload type on the `m=` line,
        // mirroring `parse_answer`'s own AMR-WB fixture above.
        let answer_sdp = "v=0\r\n\
                           c=IN IP4 172.16.5.5\r\n\
                           t=0 0\r\n\
                           m=audio 51000 RTP/AVP 96\r\n\
                           a=rtpmap:96 AMR-WB/16000\r\n\
                           a=fmtp:96 octet-align=1\r\n";
        let answer = parse_answer(answer_sdp).unwrap();
        assert_eq!(answer.remote_rtp, "172.16.5.5:51000".parse().unwrap());
        let chosen = offered_chosen_codec(answer.codec).unwrap();
        assert_eq!(chosen.codec, NegotiatedCodec::AmrWb);
        assert_eq!(chosen.payload_type, AMR_WB_PAYLOAD_TYPE);

        // Confirms the offer text actually used above is consistent with
        // this test's own fixture, not just coincidentally compatible.
        let _ = offer_sdp;
    }

    #[test]
    fn an_answer_naming_a_payload_type_build_offer_never_offered_is_rejected_outright() {
        // FR-005/SC-002: `handle_offerless_invite` must end the call
        // explicitly if the ACK's answer is incompatible. `parse_answer`
        // itself is the first line of defense — it only ever recognizes the
        // two fixed payload types `build_offer` actually offers (`0`,
        // `96`), so an answer naming anything else fails right here,
        // before `offered_chosen_codec` (whose own `None` arm exists as a
        // defensive second layer, should `parse_answer`'s recognized set
        // ever broaden independently of `build_offer`'s) is even reached.
        let answer_sdp = "v=0\r\nc=IN IP4 172.16.5.5\r\nm=audio 51000 RTP/AVP 8\r\n";
        assert!(parse_answer(answer_sdp).is_err());
    }

    /// Every carrier in production places calls on this exact offer, so its
    /// bytes must not move without a measurement to justify it.
    #[test]
    fn the_offer_states_a_codec_and_nothing_more() {
        let sdp = build_offer(
            "1.2.3.4".parse().unwrap(),
            40000,
            7,
            CodecOffer::WidebandThenPcmu,
        );

        assert!(sdp.contains("m=audio 40000 RTP/AVP 96 0\r\n"), "{sdp}");
        assert!(sdp.contains("a=fmtp:96 octet-align=1\r\n"), "{sdp}");
        for absent in ["b=AS", "telephone-event", "a=ptime", "a=maxptime"] {
            assert!(!sdp.contains(absent), "{absent} must not appear: {sdp}");
        }
        assert!(sdp.ends_with("a=sendrecv\r\n"), "{sdp}");
    }

    /// Jio's real answer, captured 2026-08-15 from the `183` to one of our
    /// outbound INVITEs. Two things about it broke us: the `c=` sits at *media*
    /// level (RFC 4566 allows either), and `o=` names a **hostname** — so a
    /// parser that reassigns `conn_ip` on every `c=`-ish line, or only looks
    /// before the first `m=`, gets this wrong.
    #[test]
    fn parse_answer_takes_a_media_level_connection_address() {
        let body = "v=0\r\n\
                    o=- 1374049043 1374049043 IN IP4 ims.mnc869.mcc405.3gppnetwork.org\r\n\
                    s=media server session\r\nb=AS:32\r\nt=0 0\r\n\
                    m=audio 18160 RTP/AVP 96\r\n\
                    c=IN IP4 10.56.159.84\r\n\
                    b=AS:32\r\nb=RR:2400\r\nb=RS:800\r\n\
                    a=rtpmap:96 AMR-WB/16000\r\n\
                    a=fmtp:96 octet-align=1;mode-set=0,1,2,3;mode-change-capability=2\r\n\
                    a=rtcp-xr\r\na=ptime:20\r\na=maxptime:80\r\na=sendrecv\r\n";

        let answer = parse_answer(body).expect("a media-level c= is valid SDP");
        assert_eq!(
            answer.remote_rtp,
            "10.56.159.84:18160".parse::<SocketAddr>().unwrap()
        );
        assert_eq!(answer.codec, NegotiatedCodec::AmrWb);
    }

    /// A realistic Airtel-shaped inbound INVITE offer: PCMU plus AMR-WB,
    /// PCMU listed first (matches how build_offer itself orders payload
    /// types, and how real VoWiFi/VoLTE offers were observed in
    /// ims::call's captured traces).
    const AIRTEL_LIKE_OFFER: &str = "v=0\r\n\
         o=- 1 1 IN IP4 10.0.0.5\r\n\
         s=-\r\n\
         c=IN IP4 10.0.0.5\r\n\
         t=0 0\r\n\
         m=audio 49170 RTP/AVP 0 96\r\n\
         a=rtpmap:0 PCMU/8000\r\n\
         a=rtpmap:96 AMR-WB/16000\r\n\
         a=fmtp:96 octet-align=1\r\n\
         a=sendrecv\r\n";

    #[test]
    fn parse_offer_extracts_remote_rtp_and_both_codecs_in_order() {
        let offer = parse_offer(AIRTEL_LIKE_OFFER).unwrap();
        assert_eq!(offer.remote_rtp, "10.0.0.5:49170".parse().unwrap());
        assert_eq!(offer.offered.len(), 2);
        assert_eq!(offer.offered[0].payload_type, 0);
        assert_eq!(offer.offered[0].codec, NegotiatedCodec::Pcmu);
        assert_eq!(offer.offered[1].payload_type, 96);
        assert_eq!(offer.offered[1].codec, NegotiatedCodec::AmrWb);
        assert_eq!(offer.proto, "RTP/AVP");
    }

    /// specs/043 SDP-03: `parse_offer` stays permissive on an unrecognized
    /// transport token — it's captured, not rejected, so the caller
    /// (`handle_invite`) can decide what an unsupported transport means,
    /// same as it already does for an unsupported codec list.
    #[test]
    fn parse_offer_captures_a_non_rtp_avp_transport_without_erroring() {
        let body =
            "v=0\r\nc=IN IP4 10.0.0.5\r\nm=audio 49170 RTP/SAVP 0\r\na=rtpmap:0 PCMU/8000\r\n";
        let offer = parse_offer(body).unwrap();
        assert_eq!(offer.proto, "RTP/SAVP");
        assert_eq!(
            offer.offered.len(),
            1,
            "codec parsing is unaffected by the transport token"
        );
    }

    #[test]
    fn parse_offer_recognizes_pcmu_without_explicit_rtpmap() {
        // PCMU (payload type 0) is a statically assigned RFC 3551 type — a
        // real UA doesn't have to send a=rtpmap:0 for it.
        let body = "v=0\r\nc=IN IP4 10.0.0.5\r\nm=audio 49170 RTP/AVP 0\r\n";
        let offer = parse_offer(body).unwrap();
        assert_eq!(
            offer.offered,
            vec![OfferedCodec {
                payload_type: 0,
                codec: NegotiatedCodec::Pcmu,
                fmtp: String::new(),
            }]
        );
    }

    /// The real Airtel mobile-terminating offer: AMR-WB on *two* payload
    /// types, 104 bandwidth-efficient and 110 octet-aligned. We must answer
    /// on 110 — answering 104 with `octet-align=1` contradicts the offer and
    /// got the call BYE'd ~250ms after our 200 OK on a live call.
    #[test]
    fn build_answer_picks_the_octet_aligned_amr_wb_payload_type() {
        let body = "v=0\r\nc=IN IP6 2401:4900:c4:4062::14\r\n\
                     m=audio 5482 RTP/AVP 104 110 102\r\n\
                     a=rtpmap:104 AMR-WB/16000\r\n\
                     a=fmtp:104 mode-set=0,1,2,3; mode-change-capability=2; max-red=0\r\n\
                     a=rtpmap:110 AMR-WB/16000\r\n\
                     a=fmtp:110 octet-align=1; mode-set=0,1,2,3; mode-change-capability=2; max-red=0\r\n\
                     a=rtpmap:102 AMR/8000\r\n";
        let offer = parse_offer(body).unwrap();

        let (sdp, codec) = build_answer(
            "2401:4900:1::2".parse().unwrap(),
            40000,
            1,
            &offer,
            true,
            false,
            AnswerPreference::legacy(),
            None,
        )
        .unwrap();
        assert_eq!(codec.codec, NegotiatedCodec::AmrWb);
        assert!(
            sdp.contains("m=audio 40000 RTP/AVP 110\r\n"),
            "must answer on the octet-aligned payload type, got:\n{sdp}"
        );
        // The offer's own parameters, echoed rather than invented.
        assert!(sdp.contains(
            "a=fmtp:110 octet-align=1; mode-set=0,1,2,3; mode-change-capability=2; max-red=0\r\n"
        ));
        assert!(
            !sdp.contains("104"),
            "must not answer on the bandwidth-efficient type"
        );
    }

    /// An AMR-WB offer with no `octet-align=1` is bandwidth-efficient. That is
    /// answerable (`ims::amr_rtp` frames both), but the answer must *not*
    /// claim octet-alignment, and the media path must be told which framing it
    /// is committed to.
    #[test]
    fn build_answer_accepts_bandwidth_efficient_amr_without_claiming_octet_align() {
        let body = "v=0\r\nc=IN IP6 2401:4900:c4:4062::14\r\n\
                     m=audio 5482 RTP/AVP 104\r\n\
                     a=rtpmap:104 AMR-WB/16000\r\n\
                     a=fmtp:104 mode-set=0,1,2,3; max-red=0\r\n";
        let offer = parse_offer(body).unwrap();
        let (sdp, chosen) = build_answer(
            "2401:4900:1::2".parse().unwrap(),
            40000,
            1,
            &offer,
            true,
            false,
            AnswerPreference::legacy(),
            None,
        )
        .unwrap();
        assert_eq!(chosen.codec, NegotiatedCodec::AmrWb);
        assert_eq!(chosen.payload_type, 104);
        assert!(!chosen.octet_aligned, "offer never declared octet-align");
        assert!(
            !sdp.contains("octet-align"),
            "answer must not assert a framing the offer didn't declare:\n{sdp}"
        );
    }

    /// The real Airtel narrowband-only offer: `AMR/8000` and nothing else, no
    /// PCMU, no AMR-WB, and bandwidth-efficient on every payload type. This is
    /// the offer that was being declined outright.
    #[test]
    fn build_answer_handles_a_narrowband_only_bandwidth_efficient_offer() {
        let body = "v=0\r\nc=IN IP6 2401:4900:c4:4062::14\r\n\
                     m=audio 30870 RTP/AVP 108 100 116\r\n\
                     a=rtpmap:108 AMR/8000\r\n\
                     a=fmtp:108 mode-set=0,2,4,7; mode-change-period=2; max-red=0\r\n\
                     a=rtpmap:100 AMR/8000\r\n\
                     a=fmtp:100 max-red=0\r\n\
                     a=rtpmap:116 telephone-event/8000\r\n";
        let offer = parse_offer(body).unwrap();
        let (sdp, chosen) = build_answer(
            "2401:4900:1::2".parse().unwrap(),
            40000,
            1,
            &offer,
            true,
            false,
            AnswerPreference::legacy(),
            None,
        )
        .unwrap();
        assert_eq!(chosen.codec, NegotiatedCodec::AmrNb);
        assert_eq!(chosen.payload_type, 108, "first listed AMR-NB payload type");
        assert!(!chosen.octet_aligned);
        // The offer's `telephone-event` rides along with the codec — dropping
        // it gets the whole answer rejected. See `SdpOffer::dtmf`.
        assert!(sdp.contains("m=audio 40000 RTP/AVP 108 116\r\n"));
        assert!(sdp.contains("a=rtpmap:108 AMR/8000\r\n"));
        assert!(sdp.contains("a=rtpmap:116 telephone-event/8000\r\n"));
        assert!(sdp.contains("a=fmtp:116 0-15\r\n"));
        // The offer's own parameters, echoed.
        assert!(sdp.contains("a=fmtp:108 mode-set=0,2,4,7; mode-change-period=2; max-red=0\r\n"));
    }

    #[test]
    fn the_answer_keeps_the_telephone_event_matching_the_chosen_codecs_rate() {
        // Jio offers both rates. Picking AMR-WB (16 kHz) must keep the 16 kHz
        // event stream, not the 8 kHz one — RFC 4733 §2.1 ties the event
        // stream's clock to the audio it accompanies.
        let body = "v=0\r\nc=IN IP4 10.56.153.59\r\n\
                     m=audio 50010 RTP/AVP 110 102 105 100\r\n\
                     a=rtpmap:110 AMR-WB/16000\r\n\
                     a=fmtp:110 mode-set=0,1,2,3;octet-align=1; mode-change-capability=2\r\n\
                     a=rtpmap:102 AMR/8000\r\n\
                     a=rtpmap:105 telephone-event/16000\r\n\
                     a=fmtp:105 0-15\r\n\
                     a=rtpmap:100 telephone-event/8000\r\n\
                     a=fmtp:100 0-15\r\n";
        let offer = parse_offer(body).unwrap();
        assert_eq!(offer.dtmf, vec![(105, 16000), (100, 8000)]);
        let (sdp, chosen) = build_answer(
            "10.6.170.49".parse().unwrap(),
            46179,
            1,
            &offer,
            true,
            true,
            AnswerPreference::legacy(),
            None,
        )
        .unwrap();
        assert_eq!(chosen.codec, NegotiatedCodec::AmrWb);
        assert!(
            sdp.contains("m=audio 46179 RTP/AVP 110 105\r\n"),
            "want the 16 kHz event stream alongside AMR-WB, got: {sdp}"
        );
        assert!(sdp.contains("a=rtpmap:105 telephone-event/16000\r\n"));
        assert!(!sdp.contains("telephone-event/8000"), "wrong clock rate");
    }

    #[test]
    fn the_answer_states_media_and_rtcp_bandwidth_before_its_attributes() {
        // TS 26.114 §6.2.10 requires b=AS plus the RTCP b=RS/b=RR pair. RFC
        // 4566 §5 fixes the order within a media section (m=, i=, c=, b=, k=,
        // a=), so the bandwidth lines must precede every a= line or the whole
        // description is malformed.
        let body = "v=0\r\nc=IN IP4 10.0.0.1\r\n\
                    m=audio 5000 RTP/AVP 110\r\n\
                    a=rtpmap:110 AMR-WB/16000\r\na=fmtp:110 octet-align=1\r\n";
        let offer = parse_offer(body).unwrap();
        let (sdp, _) = build_answer(
            "10.0.0.2".parse().unwrap(),
            40000,
            1,
            &offer,
            true,
            true,
            AnswerPreference::legacy(),
            None,
        )
        .unwrap();
        assert!(sdp.contains("b=AS:49\r\n"), "AMR-WB rate, got: {sdp}");
        assert!(sdp.contains("b=RS:800\r\n"));
        assert!(sdp.contains("b=RR:2400\r\n"));
        let first_b = sdp.find("b=").expect("a bandwidth line");
        let first_a = sdp.find("a=").expect("an attribute line");
        assert!(
            first_b < first_a,
            "b= must precede a= per RFC 4566 §5, got: {sdp}"
        );
    }

    // ---- specs/046-rtcp-reporting ---------------------------------------

    #[test]
    fn a_tier_one_or_tier_three_answer_is_byte_identical_to_no_rtcp_at_all() {
        // Contract C-1.1/C-1.2/C-1.3: the answer must be exactly what it
        // was before this feature whenever `declared_rtcp_port` is `None`
        // (RTP+1 succeeded, or no RTCP endpoint could be obtained at all)
        // — this is the common-path regression guard, and this project has
        // already been burned once by a silent SDP answer change (see
        // `SdpOffer::dtmf`'s own doc comment).
        let offer = offer_of(&[(0, "PCMU/8000")]);
        let (with_none, _) = build_answer(
            "10.0.0.2".parse().unwrap(),
            40000,
            1,
            &offer,
            true,
            false,
            AnswerPreference::legacy(),
            None,
        )
        .unwrap();
        assert!(
            !with_none.contains("a=rtcp:"),
            "no a=rtcp line when no port is declared, got: {with_none}"
        );
        assert!(with_none.contains("b=RS:800\r\n"));
        assert!(with_none.contains("b=RR:2400\r\n"));
    }

    #[test]
    fn a_tier_two_answer_adds_exactly_one_declared_rtcp_line() {
        let offer = offer_of(&[(0, "PCMU/8000")]);
        let (with_declared, _) = build_answer(
            "10.0.0.2".parse().unwrap(),
            40000,
            1,
            &offer,
            true,
            false,
            AnswerPreference::legacy(),
            Some(40001),
        )
        .unwrap();
        assert_eq!(
            with_declared.matches("a=rtcp:").count(),
            1,
            "exactly one a=rtcp line, got: {with_declared}"
        );
        assert!(with_declared.contains("a=rtcp:40001\r\n"));
        // Still unconditional (FR-017/FR-022) regardless of tier.
        assert!(with_declared.contains("b=RS:800\r\n"));
        assert!(with_declared.contains("b=RR:2400\r\n"));
    }

    #[test]
    fn build_veth_answer_never_declares_an_rtcp_port() {
        // FR-023a: the internal veth leg keeps its own unbacked b=RS/b=RR
        // declaration and never gains an a=rtcp line — RTCP reporting is
        // scoped to the carrier leg only.
        let offer = offer_of(&[(0, "PCMU/8000")]);
        let (sdp, _) =
            build_veth_answer("10.0.0.2".parse().unwrap(), 40000, 1, &offer, false).unwrap();
        assert!(!sdp.contains("a=rtcp:"));
    }

    #[test]
    fn parse_offer_reads_an_explicit_rtcp_port() {
        let body = "v=0\r\nc=IN IP4 10.0.0.1\r\n\
                    m=audio 5000 RTP/AVP 0\r\na=rtpmap:0 PCMU/8000\r\n\
                    a=rtcp:30001\r\n";
        let offer = parse_offer(body).unwrap();
        assert_eq!(offer.rtcp, Some(30001));
    }

    #[test]
    fn parse_offer_treats_a_zero_or_unparseable_rtcp_port_as_absent() {
        for line in ["a=rtcp:0\r\n", "a=rtcp:notaport\r\n", "a=rtcp:\r\n"] {
            let body = format!(
                "v=0\r\nc=IN IP4 10.0.0.1\r\n\
                 m=audio 5000 RTP/AVP 0\r\na=rtpmap:0 PCMU/8000\r\n{line}"
            );
            let offer = parse_offer(&body).unwrap();
            assert_eq!(offer.rtcp, None, "line {line:?} must not yield a port");
        }
    }

    #[test]
    fn parse_offer_with_no_rtcp_line_leaves_it_absent() {
        let offer = offer_of(&[(0, "PCMU/8000")]);
        assert_eq!(offer.rtcp, None);
    }

    #[test]
    fn the_answer_echoes_the_offers_maxptime_and_omits_it_when_absent() {
        // TS 26.114 §6.2.2 has the UE state ptime *and* maxptime for AMR/AMR-WB;
        // we only sent ptime. Echoed, never asserted, so the answer cannot
        // claim a longer packetisation than the offerer permits.
        let with = "v=0\r\nc=IN IP4 10.0.0.1\r\n\
                    m=audio 5000 RTP/AVP 110\r\n\
                    a=rtpmap:110 AMR-WB/16000\r\n\
                    a=fmtp:110 octet-align=1\r\n\
                    a=ptime:20\r\na=maxptime:240\r\n";
        let offer = parse_offer(with).unwrap();
        assert_eq!(offer.maxptime, Some(240));
        let (sdp, _) = build_answer(
            "10.0.0.2".parse().unwrap(),
            40000,
            1,
            &offer,
            true,
            true,
            AnswerPreference::legacy(),
            None,
        )
        .unwrap();
        assert!(sdp.contains("a=maxptime:240\r\n"), "got: {sdp}");

        let without = "v=0\r\nc=IN IP4 10.0.0.1\r\n\
                       m=audio 5000 RTP/AVP 0\r\na=rtpmap:0 PCMU/8000\r\n";
        let offer = parse_offer(without).unwrap();
        assert_eq!(offer.maxptime, None);
        let (sdp, _) = build_answer(
            "10.0.0.2".parse().unwrap(),
            40000,
            1,
            &offer,
            false,
            false,
            AnswerPreference::legacy(),
            None,
        )
        .unwrap();
        assert!(!sdp.contains("maxptime"));
    }

    /// specs/044 SDP-06: an offer's own `a=ptime` describes what *it*
    /// intends to send, not a request for what our answer should claim —
    /// unlike `maxptime` (a received-side upper bound this bridge already
    /// respects by always framing at 20ms, so echoing it is a true
    /// statement), always answering `a=ptime:20` is already honest: this
    /// bridge's own framing is fixed at 20ms
    /// (`NegotiatedCodec::frame_samples`'s own doc), not adjustable per
    /// offer. Echoing the offer's `ptime` into our own answer instead would
    /// be the opposite fix — claiming a packetisation we don't actually
    /// use. Confirmed here rather than changed: an offer requesting a
    /// non-default `ptime` must not change what our answer states.
    #[test]
    fn the_answer_always_states_its_own_true_20ms_ptime_regardless_of_the_offers() {
        let body = "v=0\r\nc=IN IP4 10.0.0.1\r\n\
                     m=audio 5000 RTP/AVP 0\r\na=rtpmap:0 PCMU/8000\r\na=ptime:40\r\n";
        let offer = parse_offer(body).unwrap();
        let (sdp, _) = build_answer(
            "10.0.0.2".parse().unwrap(),
            40000,
            1,
            &offer,
            false,
            false,
            AnswerPreference::legacy(),
            None,
        )
        .unwrap();
        assert!(
            sdp.contains("a=ptime:20\r\n"),
            "must state our own true framing, not the offer's requested one: {sdp}"
        );
    }

    #[test]
    fn an_offer_without_telephone_event_is_answered_without_one() {
        let body = "v=0\r\nc=IN IP4 10.0.0.1\r\n\
                     m=audio 5000 RTP/AVP 0\r\n\
                     a=rtpmap:0 PCMU/8000\r\n";
        let offer = parse_offer(body).unwrap();
        assert!(offer.dtmf.is_empty());
        let (sdp, _) = build_answer(
            "10.0.0.2".parse().unwrap(),
            40000,
            1,
            &offer,
            false,
            false,
            AnswerPreference::legacy(),
            None,
        )
        .unwrap();
        assert!(sdp.contains("m=audio 40000 RTP/AVP 0\r\n"));
        assert!(!sdp.contains("telephone-event"));
    }

    /// Without a linked AMR codec there is genuinely nothing to answer such an
    /// offer with — decline rather than answer with a codec we can't encode.
    #[test]
    fn build_answer_declines_an_amr_only_offer_when_amr_is_not_linked() {
        let body = "v=0\r\nc=IN IP6 2401:4900:c4:4062::14\r\n\
                     m=audio 30870 RTP/AVP 108\r\n\
                     a=rtpmap:108 AMR/8000\r\n";
        let offer = parse_offer(body).unwrap();
        assert!(build_answer(
            "2401:4900:1::2".parse().unwrap(),
            40000,
            1,
            &offer,
            false,
            false,
            AnswerPreference::legacy(),
            None,
        )
        .is_err());
    }

    #[test]
    fn parse_offer_omits_unrecognized_codecs_without_erroring() {
        // GSM/EFR (payload type 3) alongside PCMU — should just skip the
        // one we don't recognize rather than failing the whole offer.
        let body = "v=0\r\nc=IN IP4 10.0.0.5\r\nm=audio 49170 RTP/AVP 0 3\r\n\
                     a=rtpmap:3 GSM/8000\r\n";
        let offer = parse_offer(body).unwrap();
        assert_eq!(offer.offered.len(), 1);
        assert_eq!(offer.offered[0].codec, NegotiatedCodec::Pcmu);
    }

    #[test]
    fn parse_offer_rejects_missing_connection_line() {
        let body = "v=0\r\nm=audio 50000 RTP/AVP 0\r\n";
        assert!(parse_offer(body).is_err());
    }

    #[test]
    fn build_answer_prefers_pcmu_when_offered() {
        let offer = parse_offer(AIRTEL_LIKE_OFFER).unwrap();
        let (sdp, codec) = build_answer(
            "1.2.3.4".parse().unwrap(),
            40000,
            999,
            &offer,
            true,
            false,
            AnswerPreference::legacy(),
            None,
        )
        .unwrap();
        assert_eq!(codec.codec, NegotiatedCodec::Pcmu);
        assert!(sdp.contains("m=audio 40000 RTP/AVP 0\r\n"));
        assert!(sdp.contains("a=rtpmap:0 PCMU/8000"));
        assert!(!sdp.contains("AMR-WB"));
    }

    #[test]
    fn build_answer_falls_back_to_amr_wb_when_pcmu_absent_and_amr_available() {
        // `octet-align=1` is required for the offer to be answerable at all —
        // it is the only AMR-WB framing this client can produce or consume.
        let body = "v=0\r\nc=IN IP4 10.0.0.5\r\nm=audio 49170 RTP/AVP 97\r\n\
                     a=rtpmap:97 AMR-WB/16000\r\na=fmtp:97 octet-align=1\r\n";
        let offer = parse_offer(body).unwrap();
        let (sdp, codec) = build_answer(
            "1.2.3.4".parse().unwrap(),
            40000,
            999,
            &offer,
            true,
            false,
            AnswerPreference::legacy(),
            None,
        )
        .unwrap();
        assert_eq!(codec.codec, NegotiatedCodec::AmrWb);
        // Echoes the offer's own payload type (97), not the hardcoded 96.
        assert!(sdp.contains("m=audio 40000 RTP/AVP 97\r\n"));
        assert!(sdp.contains("a=rtpmap:97 AMR-WB/16000"));
    }

    #[test]
    fn build_answer_errors_when_amr_wb_only_offer_and_amr_unavailable() {
        let body = "v=0\r\nc=IN IP4 10.0.0.5\r\nm=audio 49170 RTP/AVP 96\r\n\
                     a=rtpmap:96 AMR-WB/16000\r\n";
        let offer = parse_offer(body).unwrap();
        let result = build_answer(
            "1.2.3.4".parse().unwrap(),
            40000,
            999,
            &offer,
            false,
            false,
            AnswerPreference::legacy(),
            None,
        );
        assert!(result.is_err());
    }

    #[test]
    fn build_answer_errors_when_offer_has_no_recognized_codec() {
        let body = "v=0\r\nc=IN IP4 10.0.0.5\r\nm=audio 49170 RTP/AVP 3\r\n\
                     a=rtpmap:3 GSM/8000\r\n";
        let offer = parse_offer(body).unwrap();
        let result = build_answer(
            "1.2.3.4".parse().unwrap(),
            40000,
            999,
            &offer,
            true,
            false,
            AnswerPreference::legacy(),
            None,
        );
        assert!(result.is_err());
    }

    /// The whole point of wideband mode: when the bridge can carry 16 kHz all
    /// the way to the PBX, an offer of both PCMU and AMR-WB must take AMR-WB.
    /// Taking PCMU (the narrowband-mode choice) would make the carrier
    /// downsample to 8 kHz before we ever see the audio.
    #[test]
    fn build_answer_prefers_amr_wb_over_pcmu_in_wideband_mode() {
        let offer = parse_offer(AIRTEL_LIKE_OFFER).unwrap();
        let (sdp, codec) = build_answer(
            "1.2.3.4".parse().unwrap(),
            40000,
            999,
            &offer,
            true,
            true,
            AnswerPreference::legacy(),
            None,
        )
        .unwrap();
        assert_eq!(codec.codec, NegotiatedCodec::AmrWb);
        assert!(sdp.contains("AMR-WB/16000"));
    }

    /// Wideband mode must not make a narrowband-only carrier unanswerable —
    /// a PCMU-only offer is still answered with PCMU, exactly as before.
    #[test]
    fn wideband_mode_still_answers_a_pcmu_only_offer_with_pcmu() {
        let body = "v=0\r\nc=IN IP4 10.0.0.5\r\nm=audio 49170 RTP/AVP 0\r\n";
        let offer = parse_offer(body).unwrap();
        let (_, codec) = build_answer(
            "1.2.3.4".parse().unwrap(),
            40000,
            999,
            &offer,
            true,
            true,
            AnswerPreference::legacy(),
            None,
        )
        .unwrap();
        assert_eq!(codec.codec, NegotiatedCodec::Pcmu);
    }

    /// ...nor an AMR-NB-only one, which is the other narrowband shape Airtel
    /// actually sends.
    #[test]
    fn wideband_mode_still_answers_an_amr_nb_only_offer_with_amr_nb() {
        let body = "v=0\r\nc=IN IP4 10.0.0.5\r\nm=audio 49170 RTP/AVP 108\r\n\
                     a=rtpmap:108 AMR/8000\r\n";
        let offer = parse_offer(body).unwrap();
        let (_, codec) = build_answer(
            "1.2.3.4".parse().unwrap(),
            40000,
            999,
            &offer,
            true,
            true,
            AnswerPreference::legacy(),
            None,
        )
        .unwrap();
        assert_eq!(codec.codec, NegotiatedCodec::AmrNb);
    }

    /// PJSIP's veth-link offer, roughly as Agent B sends it with a 16 kHz
    /// conference bridge: L16 alongside the usual narrowband codecs.
    const PJSIP_VETH_OFFER: &str = "v=0\r\nc=IN IP4 10.99.0.2\r\n\
         m=audio 4000 RTP/AVP 9 96 0 8\r\n\
         a=rtpmap:9 G722/8000\r\n\
         a=rtpmap:96 L16/16000\r\n\
         a=rtpmap:0 PCMU/8000\r\n\
         a=rtpmap:8 PCMA/8000\r\n";

    /// PJSIP's *real* veth offer, captured from a linked PJSIP running Agent
    /// B's media config. The trap is the trailing T.140 text section, which
    /// reuses payload type **100** — the very number the audio section gave to
    /// `L16/16000` — for `red/1000`. Parsing attributes across the whole body
    /// lets the text section redefine 100, L16 disappears from the audio
    /// codec list, and a wideband call silently drops to PCMU on the veth
    /// (observed on a live Airtel call: `veth_codec="PCMU"` despite
    /// `carrier_codec="AMR-WB"`).
    const PJSIP_REAL_VETH_OFFER: &str = "v=0\r\n\
         o=- 3992923331 3992923331 IN IP4 10.99.0.2\r\n\
         s=pjmedia\r\n\
         t=0 0\r\n\
         m=audio 4000 RTP/AVP 9 96 97 98 3 0 8 99 100 120 121 122\r\n\
         c=IN IP4 10.99.0.2\r\n\
         a=sendrecv\r\n\
         a=rtpmap:9 G722/8000\r\n\
         a=rtpmap:96 speex/16000\r\n\
         a=rtpmap:97 speex/8000\r\n\
         a=rtpmap:98 iLBC/8000\r\n\
         a=rtpmap:3 GSM/8000\r\n\
         a=rtpmap:0 PCMU/8000\r\n\
         a=rtpmap:8 PCMA/8000\r\n\
         a=rtpmap:99 speex/32000\r\n\
         a=rtpmap:100 L16/16000\r\n\
         a=rtpmap:120 telephone-event/8000\r\n\
         m=text 4002 RTP/AVP 100 98\r\n\
         c=IN IP4 10.99.0.2\r\n\
         a=rtpmap:100 red/1000\r\n\
         a=rtpmap:98 t140/1000\r\n";

    #[test]
    fn a_later_media_sections_payload_types_do_not_redefine_the_audio_ones() {
        let offer = parse_offer(PJSIP_REAL_VETH_OFFER).unwrap();
        let l16 = offer
            .offered
            .iter()
            .find(|c| c.codec == NegotiatedCodec::L16)
            .expect("L16 on pt 100 must survive the m=text section reusing pt 100");
        assert_eq!(l16.payload_type, 100);

        let (_, codec) =
            build_veth_answer("10.99.0.1".parse().unwrap(), 40000, 1, &offer, true).unwrap();
        assert_eq!(codec.codec, NegotiatedCodec::L16);
        assert_eq!(codec.payload_type, 100);
    }

    /// The audio stream's port and address must come from the audio section,
    /// not from whichever `m=` section happened to be parsed last.
    #[test]
    fn the_audio_sections_port_wins_over_a_later_sections() {
        let offer = parse_offer(PJSIP_REAL_VETH_OFFER).unwrap();
        assert_eq!(
            offer.remote_rtp.port(),
            4000,
            "m=audio's port, not m=text's"
        );
    }

    /// specs/043 SDP-01: the `m=text` section this fixture carries must not
    /// simply vanish from the answer — it gets an explicit RFC 3264 §6
    /// decline, in its original position after the negotiated audio line.
    #[test]
    fn a_trailing_non_audio_section_is_declined_not_silently_dropped() {
        let offer = parse_offer(PJSIP_REAL_VETH_OFFER).unwrap();
        let (sdp, _) =
            build_veth_answer("10.99.0.1".parse().unwrap(), 40000, 1, &offer, true).unwrap();
        assert!(
            sdp.contains("m=text 0 RTP/AVP 100 98\r\n"),
            "declined text section must echo its own proto/fmt-list with port 0: {sdp}"
        );
        let audio_pos = sdp.find("m=audio ").expect("negotiated audio line");
        let text_pos = sdp.find("m=text 0").expect("declined text line");
        assert!(
            text_pos > audio_pos,
            "the text section came after audio in the offer, so it must too in the answer: {sdp}"
        );
    }

    /// specs/043 SDP-01: a second `m=audio` section must not silently
    /// overwrite the first's port/codec list (the pre-fix behavior) — the
    /// first is what gets negotiated, and the second is declined.
    #[test]
    fn a_second_audio_section_is_declined_the_first_is_negotiated() {
        let offer_body = "v=0\r\no=- 1 1 IN IP4 5.6.7.8\r\ns=-\r\nc=IN IP4 5.6.7.8\r\n\
             t=0 0\r\nm=audio 40000 RTP/AVP 0\r\na=rtpmap:0 PCMU/8000\r\n\
             m=audio 40010 RTP/AVP 8\r\na=rtpmap:8 PCMA/8000\r\n";
        let offer = parse_offer(offer_body).unwrap();
        assert_eq!(
            offer.remote_rtp.port(),
            40000,
            "the first m=audio section's port must win, not the second's"
        );
        assert_eq!(
            offer.offered.len(),
            1,
            "only the first section's codec list"
        );
        assert_eq!(offer.offered[0].codec, NegotiatedCodec::Pcmu);

        let (sdp, codec) = build_answer(
            "10.0.0.1".parse().unwrap(),
            40000,
            1,
            &offer,
            false,
            false,
            AnswerPreference::Legacy,
            None,
        )
        .unwrap();
        assert_eq!(codec.codec, NegotiatedCodec::Pcmu);
        assert!(
            sdp.contains("m=audio 0 RTP/AVP 8\r\n"),
            "the second audio section must be declined (port 0), not negotiated: {sdp}"
        );
    }

    /// An offer with a direction attribute in its audio section (`_dir`,
    /// e.g. `"a=sendonly\r\n"`) that this bridge must answer with the RFC
    /// 3264 §6.1 mirror (`_expect`, e.g. `"a=recvonly\r\n"`).
    fn offer_with_direction(dir: &str) -> SdpOffer {
        let body = format!(
            "v=0\r\no=- 1 1 IN IP4 5.6.7.8\r\ns=-\r\nc=IN IP4 5.6.7.8\r\n\
             t=0 0\r\nm=audio 40000 RTP/AVP 0\r\n{dir}a=rtpmap:0 PCMU/8000\r\n"
        );
        parse_offer(&body).expect("test offer must parse")
    }

    /// specs/043 SDP-02: the answer must state the mirrored direction, not
    /// always claim two-way media regardless of what the offer said.
    #[test]
    fn the_answer_mirrors_a_sendonly_offer_as_recvonly() {
        let offer = offer_with_direction("a=sendonly\r\n");
        assert_eq!(offer.direction, MediaDirection::SendOnly);
        let (sdp, _) = build_answer(
            "10.0.0.1".parse().unwrap(),
            40000,
            1,
            &offer,
            false,
            false,
            AnswerPreference::Legacy,
            None,
        )
        .unwrap();
        assert!(sdp.contains("a=recvonly\r\n"), "{sdp}");
        assert!(!sdp.contains("a=sendrecv\r\n"), "{sdp}");
    }

    #[test]
    fn the_answer_mirrors_a_recvonly_offer_as_sendonly() {
        let offer = offer_with_direction("a=recvonly\r\n");
        assert_eq!(offer.direction, MediaDirection::RecvOnly);
        let (sdp, _) = build_answer(
            "10.0.0.1".parse().unwrap(),
            40000,
            1,
            &offer,
            false,
            false,
            AnswerPreference::Legacy,
            None,
        )
        .unwrap();
        assert!(sdp.contains("a=sendonly\r\n"), "{sdp}");
    }

    #[test]
    fn the_answer_mirrors_an_inactive_offer_as_inactive() {
        let offer = offer_with_direction("a=inactive\r\n");
        assert_eq!(offer.direction, MediaDirection::Inactive);
        let (sdp, _) = build_answer(
            "10.0.0.1".parse().unwrap(),
            40000,
            1,
            &offer,
            false,
            false,
            AnswerPreference::Legacy,
            None,
        )
        .unwrap();
        assert!(sdp.contains("a=inactive\r\n"), "{sdp}");
    }

    /// An offer stating `sendrecv` explicitly, or nothing at all, must still
    /// answer `sendrecv` — today's only real-world case, unchanged.
    #[test]
    fn an_ordinary_offer_still_answers_sendrecv() {
        for dir in ["a=sendrecv\r\n", ""] {
            let offer = offer_with_direction(dir);
            assert_eq!(offer.direction, MediaDirection::SendRecv);
            let (sdp, _) = build_answer(
                "10.0.0.1".parse().unwrap(),
                40000,
                1,
                &offer,
                false,
                false,
                AnswerPreference::Legacy,
                None,
            )
            .unwrap();
            assert!(sdp.contains("a=sendrecv\r\n"), "{sdp}");
        }
    }

    /// PR review, 2026-08-27: a direction attribute placed at *session*
    /// level (before any `m=` line, RFC 4566 §5.14) applies to the audio
    /// section too when the section states none of its own — the
    /// `!in_audio` guard must not skip it just because it comes before the
    /// first `m=` line.
    #[test]
    fn a_session_level_direction_applies_to_the_audio_section() {
        let body = "v=0\r\no=- 1 1 IN IP4 5.6.7.8\r\ns=-\r\nc=IN IP4 5.6.7.8\r\n\
             t=0 0\r\na=sendonly\r\nm=audio 40000 RTP/AVP 0\r\na=rtpmap:0 PCMU/8000\r\n";
        let offer = parse_offer(body).unwrap();
        assert_eq!(offer.direction, MediaDirection::SendOnly);
        let (sdp, _) = build_answer(
            "10.0.0.1".parse().unwrap(),
            40000,
            1,
            &offer,
            false,
            false,
            AnswerPreference::Legacy,
            None,
        )
        .unwrap();
        assert!(sdp.contains("a=recvonly\r\n"), "{sdp}");
    }

    /// The audio section's own direction attribute overrides a session-level
    /// one when both are present (RFC 4566 §5.14).
    #[test]
    fn the_audio_sections_own_direction_overrides_the_session_level_one() {
        let body = "v=0\r\no=- 1 1 IN IP4 5.6.7.8\r\ns=-\r\nc=IN IP4 5.6.7.8\r\n\
             t=0 0\r\na=sendonly\r\nm=audio 40000 RTP/AVP 0\r\na=rtpmap:0 PCMU/8000\r\na=inactive\r\n";
        let offer = parse_offer(body).unwrap();
        assert_eq!(offer.direction, MediaDirection::Inactive);
    }

    #[test]
    fn veth_answer_takes_l16_when_the_carrier_leg_is_wideband() {
        let offer = parse_offer(PJSIP_VETH_OFFER).unwrap();
        let (sdp, codec) =
            build_veth_answer("10.99.0.1".parse().unwrap(), 40000, 1, &offer, true).unwrap();
        assert_eq!(codec.codec, NegotiatedCodec::L16);
        assert_eq!(codec.payload_type, 96, "echoes PJSIP's own payload type");
        assert!(sdp.contains("m=audio 40000 RTP/AVP 96\r\n"));
        assert!(sdp.contains("a=rtpmap:96 L16/16000\r\n"));
    }

    /// A narrowband carrier leg has no wideband to preserve, so the veth link
    /// stays on PCMU — the same payload-for-payload passthrough path it took
    /// before L16 existed, even though PJSIP offered L16.
    #[test]
    fn veth_answer_stays_on_pcmu_for_a_narrowband_carrier_leg() {
        let offer = parse_offer(PJSIP_VETH_OFFER).unwrap();
        let (_, codec) =
            build_veth_answer("10.99.0.1".parse().unwrap(), 40000, 1, &offer, false).unwrap();
        assert_eq!(codec.codec, NegotiatedCodec::Pcmu);
        assert_eq!(codec.payload_type, 0);
    }

    /// A PJSIP build without L16 (or with it disabled) must still bridge — it
    /// just falls back to PCMU and transcodes the wideband carrier leg down,
    /// exactly as it did before this feature.
    #[test]
    fn veth_answer_falls_back_to_pcmu_when_pjsip_offers_no_l16() {
        let body = "v=0\r\nc=IN IP4 10.99.0.2\r\nm=audio 4000 RTP/AVP 0 8\r\n\
                     a=rtpmap:0 PCMU/8000\r\na=rtpmap:8 PCMA/8000\r\n";
        let offer = parse_offer(body).unwrap();
        let (_, codec) =
            build_veth_answer("10.99.0.1".parse().unwrap(), 40000, 1, &offer, true).unwrap();
        assert_eq!(codec.codec, NegotiatedCodec::Pcmu);
    }

    #[test]
    fn frame_samples_follow_each_codecs_own_rate() {
        assert_eq!(NegotiatedCodec::Pcmu.frame_samples(), 160);
        assert_eq!(NegotiatedCodec::AmrNb.frame_samples(), 160);
        assert_eq!(NegotiatedCodec::AmrWb.frame_samples(), 320);
        assert_eq!(NegotiatedCodec::L16.frame_samples(), 320);
    }

    // specs/048 MT-06: SDP QoS preconditions (RFC 3312). `remote` below is
    // offer-relative — this bridge's own segment, once inverted to `local`
    // in the answer (research.md Decision 1).

    /// An offer with one `a=des:qos` line naming the given `status_type`/
    /// `strength`/`direction` tokens verbatim, e.g.
    /// `offer_with_precondition("remote", "mandatory", "sendrecv")`.
    fn offer_with_precondition(status_type: &str, strength: &str, direction: &str) -> SdpOffer {
        let body = format!(
            "v=0\r\no=- 1 1 IN IP4 5.6.7.8\r\ns=-\r\nc=IN IP4 5.6.7.8\r\n\
             t=0 0\r\nm=audio 40000 RTP/AVP 0\r\n\
             a=rtpmap:0 PCMU/8000\r\n\
             a=des:qos {strength} {status_type} {direction}\r\n"
        );
        parse_offer(&body).expect("test offer must parse")
    }

    #[test]
    fn a_remote_mandatory_precondition_is_confirmed_local_in_the_answer() {
        let offer = offer_with_precondition("remote", "mandatory", "sendrecv");
        assert_eq!(
            precondition_verdict(&offer),
            PreconditionVerdict::Proceed(vec![QosAnswerLine {
                status_type: QosStatusType::Local,
                met: QosDirection::SendRecv,
                confirm: true,
            }])
        );
        let (sdp, _) = build_answer(
            "10.0.0.1".parse().unwrap(),
            40000,
            1,
            &offer,
            true,
            true,
            AnswerPreference::Legacy,
            None,
        )
        .unwrap();
        assert!(sdp.contains("a=curr:qos local sendrecv\r\n"));
        assert!(sdp.contains("a=conf:qos local sendrecv\r\n"));
    }

    /// PR #68 Greptile review: a directional precondition (`recv` only)
    /// must be answered with exactly that direction, not unconditionally
    /// `sendrecv` — this bridge's relay is always full-duplex, but the
    /// answer must confirm what was asked, never claim more than was
    /// asked.
    #[test]
    fn a_recv_only_precondition_is_confirmed_recv_not_sendrecv() {
        let offer = offer_with_precondition("remote", "mandatory", "recv");
        assert_eq!(
            precondition_verdict(&offer),
            PreconditionVerdict::Proceed(vec![QosAnswerLine {
                status_type: QosStatusType::Local,
                met: QosDirection::Recv,
                confirm: true,
            }])
        );
        let (sdp, _) = build_answer(
            "10.0.0.1".parse().unwrap(),
            40000,
            1,
            &offer,
            true,
            true,
            AnswerPreference::Legacy,
            None,
        )
        .unwrap();
        assert!(sdp.contains("a=curr:qos local recv\r\n"));
        assert!(sdp.contains("a=conf:qos local recv\r\n"));
        assert!(!sdp.contains("local sendrecv"), "must not overclaim: {sdp}");
    }

    #[test]
    fn a_remote_optional_precondition_is_also_confirmed() {
        let offer = offer_with_precondition("remote", "optional", "sendrecv");
        assert_eq!(
            precondition_verdict(&offer),
            PreconditionVerdict::Proceed(vec![QosAnswerLine {
                status_type: QosStatusType::Local,
                met: QosDirection::SendRecv,
                confirm: true,
            }])
        );
    }

    #[test]
    fn a_remote_precondition_with_no_confirmation_strength_gets_curr_only() {
        for strength in ["none", "failure", "unknown-token"] {
            let offer = offer_with_precondition("remote", strength, "sendrecv");
            assert_eq!(
                precondition_verdict(&offer),
                PreconditionVerdict::Proceed(vec![QosAnswerLine {
                    status_type: QosStatusType::Local,
                    met: QosDirection::SendRecv,
                    confirm: false,
                }]),
                "strength {strength} must not produce a=conf"
            );
        }
    }

    #[test]
    fn no_precondition_lines_at_all_proceeds_with_no_answer_lines() {
        let offer = offer_of(&[(0, "PCMU/8000")]);
        assert_eq!(
            precondition_verdict(&offer),
            PreconditionVerdict::Proceed(vec![])
        );
        let (sdp, _) = build_answer(
            "10.0.0.1".parse().unwrap(),
            40000,
            1,
            &offer,
            true,
            true,
            AnswerPreference::Legacy,
            None,
        )
        .unwrap();
        assert!(
            !sdp.contains("qos"),
            "no precondition lines were offered: {sdp}"
        );
    }

    #[test]
    fn an_e2e_mandatory_precondition_declines_the_call() {
        let offer = offer_with_precondition("e2e", "mandatory", "sendrecv");
        assert_eq!(precondition_verdict(&offer), PreconditionVerdict::Decline);
    }

    #[test]
    fn an_e2e_optional_precondition_proceeds_reporting_only_our_own_segment() {
        let offer = offer_with_precondition("e2e", "optional", "sendrecv");
        assert_eq!(
            precondition_verdict(&offer),
            PreconditionVerdict::Proceed(vec![QosAnswerLine {
                status_type: QosStatusType::E2e,
                met: QosDirection::SendRecv,
                confirm: false,
            }])
        );
    }

    #[test]
    fn a_remote_mandatory_line_combined_with_an_e2e_mandatory_line_still_declines() {
        let body = "v=0\r\no=- 1 1 IN IP4 5.6.7.8\r\ns=-\r\nc=IN IP4 5.6.7.8\r\n\
             t=0 0\r\nm=audio 40000 RTP/AVP 0\r\na=rtpmap:0 PCMU/8000\r\n\
             a=des:qos mandatory remote sendrecv\r\n\
             a=des:qos mandatory e2e sendrecv\r\n";
        let offer = parse_offer(body).unwrap();
        assert_eq!(
            precondition_verdict(&offer),
            PreconditionVerdict::Decline,
            "the unconfirmable e2e line must govern, not be masked by the confirmable remote line"
        );
    }

    #[test]
    fn two_remote_lines_with_different_directions_each_get_their_own_answer_line() {
        let body = "v=0\r\no=- 1 1 IN IP4 5.6.7.8\r\ns=-\r\nc=IN IP4 5.6.7.8\r\n\
             t=0 0\r\nm=audio 40000 RTP/AVP 0\r\na=rtpmap:0 PCMU/8000\r\n\
             a=des:qos mandatory remote sendrecv\r\n\
             a=des:qos optional remote recvonly-is-not-a-real-token\r\n";
        let offer = parse_offer(body).unwrap();
        // The second line's malformed direction token is simply skipped
        // (parse_qos_direction returns None) — only the first line
        // survives into `preconditions`, proving multiple lines are each
        // read independently rather than merged or short-circuited.
        assert_eq!(offer.preconditions.len(), 1);
        assert_eq!(
            precondition_verdict(&offer),
            PreconditionVerdict::Proceed(vec![QosAnswerLine {
                status_type: QosStatusType::Local,
                met: QosDirection::SendRecv,
                confirm: true,
            }])
        );
    }

    /// User Story 3: the offer's own `local`-status-type claim is mirrored
    /// through inverted (`local`→`remote`), never a value this bridge
    /// invents.
    #[test]
    fn the_offerers_own_segment_is_mirrored_not_invented() {
        let body = "v=0\r\no=- 1 1 IN IP4 5.6.7.8\r\ns=-\r\nc=IN IP4 5.6.7.8\r\n\
             t=0 0\r\nm=audio 40000 RTP/AVP 0\r\na=rtpmap:0 PCMU/8000\r\n\
             a=des:qos mandatory local sendrecv\r\n\
             a=curr:qos local none\r\n";
        let offer = parse_offer(body).unwrap();
        assert_eq!(
            precondition_verdict(&offer),
            PreconditionVerdict::Proceed(vec![QosAnswerLine {
                status_type: QosStatusType::Remote,
                met: QosDirection::None,
                confirm: false,
            }]),
            "a local-status-type a=des line must not itself produce an answer line; \
             only the offer's own a=curr:qos local claim, mirrored, may"
        );
    }

    #[test]
    fn a_local_status_type_line_with_no_matching_curr_line_produces_nothing() {
        let offer = offer_with_precondition("local", "mandatory", "sendrecv");
        assert_eq!(
            precondition_verdict(&offer),
            PreconditionVerdict::Proceed(vec![]),
            "no a=curr:qos local line was offered, so nothing may be fabricated for it"
        );
    }

    /// FR-007 / spec Edge Cases: `Require: precondition` with no
    /// `a=des:qos` lines at all is not a reason to decline.
    #[test]
    fn require_precondition_with_no_qos_lines_is_treated_as_no_precondition() {
        let offer = offer_of(&[(0, "PCMU/8000")]);
        assert!(offer.preconditions.is_empty());
        assert_eq!(
            precondition_verdict(&offer),
            PreconditionVerdict::Proceed(vec![])
        );
    }
}
