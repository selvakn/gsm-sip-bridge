//! Agent A's *second* SIP front: the plain, unauthenticated point-to-point
//! link to Agent B over the veth, plus the RTP relay that joins it to the
//! carrier leg.
//!
//! Split out of `agent::mod` because this half needs none of the IMS-AKA/Gm
//! machinery the carrier-facing half is built on — it is a tiny UAS on a
//! trusted link, and a byte pump. Both call directions (inbound `handle_invite`
//! and outbound `finish_origination`) use it identically.

use crate::error::{BridgeError, BridgeResult};
use crate::ims::sdp;
use crate::ims::sip_client::{build_200_ok_invite, random_hex, SipRequest};
use std::net::{IpAddr, SocketAddr, UdpSocket};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{mpsc, Arc};
use std::time::Duration;

/// How long Agent A waits for Agent B's veth-side `INVITE` to arrive after
/// signaling `IncomingCall` — Agent B places its veth call as part of
/// reaching `BridgeReady`, so this should resolve well within
/// `CONTROL_TIMEOUT` in the success case; this is the ceiling for the
/// separate thread that's listening for it.
pub(super) const VETH_INVITE_TIMEOUT: Duration = Duration::from_secs(5);

/// How often the RTP relay's blocking `recv` wakes up to check whether it
/// should stop — bounds how quickly a hangup actually silences the relay.
const RELAY_POLL_INTERVAL: Duration = Duration::from_millis(200);

/// Result of Agent A's veth-facing UAS answering Agent B's inbound call.
pub(super) struct VethUasResult {
    pub(super) rtp_socket: UdpSocket,
    /// The codec this UAS answered Agent B's offer with — `L16/16000` when the
    /// carrier leg is wideband and PJSIP offered it, PCMU otherwise. The media
    /// path must speak exactly this.
    pub(super) codec: sdp::ChosenCodec,
}

/// Starts a background thread listening for Agent B's veth-side `INVITE`
/// (a single UDP datagram is expected — PJSIP's default offer is well under
/// any MTU), answers it, and delivers the resulting RTP socket (already
/// `connect()`-ed to Agent B's advertised RTP address) over the returned
/// channel. Started *before* signaling Agent B over the control channel so
/// the listener is guaranteed to be up by the time Agent B's `Call::make`
/// actually reaches it.
pub(super) fn spawn_veth_uas_listener(
    veth_local_ip: IpAddr,
    veth_sip_port: u16,
    wideband: bool,
) -> BridgeResult<mpsc::Receiver<BridgeResult<VethUasResult>>> {
    let sip_socket = UdpSocket::bind((veth_local_ip, veth_sip_port))
        .map_err(|e| BridgeError::Ims(format!("veth SIP socket bind failed: {e}")))?;
    sip_socket
        .set_read_timeout(Some(VETH_INVITE_TIMEOUT))
        .map_err(|e| BridgeError::Ims(format!("veth SIP socket set_read_timeout failed: {e}")))?;

    let (tx, rx) = mpsc::channel();
    std::thread::spawn(move || {
        let _ = tx.send(accept_veth_invite(
            &sip_socket,
            veth_local_ip,
            veth_sip_port,
            wideband,
        ));
    });
    Ok(rx)
}

fn accept_veth_invite(
    sip_socket: &UdpSocket,
    veth_local_ip: IpAddr,
    veth_sip_port: u16,
    wideband: bool,
) -> BridgeResult<VethUasResult> {
    let mut buf = [0u8; 4096];
    let (n, peer) = sip_socket
        .recv_from(&mut buf)
        .map_err(|e| BridgeError::Ims(format!("veth INVITE recv failed: {e}")))?;
    let text = String::from_utf8_lossy(&buf[..n]);
    let (req, _consumed) = SipRequest::try_parse(&text)?
        .ok_or_else(|| BridgeError::Ims("incomplete veth INVITE datagram".into()))?;
    if req.method != "INVITE" {
        return Err(BridgeError::Ims(format!(
            "expected INVITE on the veth SIP link, got {}",
            req.method
        )));
    }

    let offer = sdp::parse_offer(&req.body)?;
    let rtp_socket = UdpSocket::bind((veth_local_ip, 0))
        .map_err(|e| BridgeError::Ims(format!("veth RTP socket bind failed: {e}")))?;
    let rtp_port = rtp_socket
        .local_addr()
        .map_err(|e| BridgeError::Ims(format!("veth RTP local_addr failed: {e}")))?
        .port();

    let session_id: u64 = rand::random::<u32>() as u64;
    // No AMR on this internal leg — Agent B's PJSIP offers PCMU always and
    // (with its 16 kHz conference bridge) L16/16000, which `build_veth_answer`
    // takes whenever the carrier leg has wideband worth carrying.
    let (answer_sdp, codec) =
        sdp::build_veth_answer(veth_local_ip, rtp_port, session_id, &offer, wideband)?;
    let to_tag = random_hex(4);
    let contact = format!("<sip:agent-a@{veth_local_ip}:{veth_sip_port}>");
    let response = build_200_ok_invite(&req, &to_tag, &contact, &answer_sdp);
    sip_socket
        .send_to(response.as_bytes(), peer)
        .map_err(|e| BridgeError::Ims(format!("veth 200 OK send failed: {e}")))?;

    // Trust the datagram's source address over the SDP's `c=` line, and take
    // only the port from the offer. PJSIP binds media to 0.0.0.0 and
    // advertises the container's *default-route* (LAN) address, which does
    // not exist inside netns "ims" — its only IPv4 route is the veth /30, so
    // connecting to the advertised address fails outright with "Network is
    // unreachable" and the call dies after being answered. On a
    // point-to-point link the peer that just sent us this INVITE is by
    // definition reachable at its source address, which makes this both
    // correct and independent of however the container's LAN is addressed.
    let rtp_dst = SocketAddr::new(peer.ip(), offer.remote_rtp.port());
    if rtp_dst.ip() != offer.remote_rtp.ip() {
        tracing::debug!(
            advertised = %offer.remote_rtp,
            using = %rtp_dst,
            "Agent B advertised a non-veth RTP address; using its veth source address instead"
        );
    }
    rtp_socket
        .connect(rtp_dst)
        .map_err(|e| BridgeError::Ims(format!("veth RTP connect to {rtp_dst} failed: {e}")))?;

    Ok(VethUasResult { rtp_socket, codec })
}

pub(super) fn spawn_relay(
    carrier: UdpSocket,
    veth: UdpSocket,
    stop: Arc<AtomicBool>,
    meter: &crate::ims::media_stats::MediaMeter,
) {
    let carrier_rx = meter.carrier_rx_counter();
    let pbx_rx = meter.pbx_rx_counter();
    std::thread::spawn(move || relay_rtp(carrier, veth, stop, carrier_rx, pbx_rx));
}

/// Relays raw UDP payloads bidirectionally between `a` and `b` (both
/// already `connect()`-ed to their remote peer) until `stop` is set.
/// Forwards bytes verbatim rather than decoding/re-encoding: both legs
/// speak the same codec by construction — `handle_invite` only reaches this
/// point once the carrier offer negotiated PCMU, and Agent B's PJSIP leg is
/// always PCMU too — so the wire bytes (RTP header included: SSRC,
/// sequence, timestamp all stay whatever the real source generated) are
/// already correct for the other side without modification.
pub fn relay_rtp(
    carrier: UdpSocket,
    veth: UdpSocket,
    stop: Arc<AtomicBool>,
    carrier_rx: Arc<std::sync::atomic::AtomicU64>,
    pbx_rx: Arc<std::sync::atomic::AtomicU64>,
) {
    let (carrier2, veth2, stop2) = match (carrier.try_clone(), veth.try_clone()) {
        (Ok(a2), Ok(b2)) => (a2, b2, stop.clone()),
        (Err(e), _) | (_, Err(e)) => {
            tracing::error!(error = %e, "RTP relay socket clone failed, aborting relay");
            return;
        }
    };
    let _ = carrier.set_read_timeout(Some(RELAY_POLL_INTERVAL));
    let _ = veth.set_read_timeout(Some(RELAY_POLL_INTERVAL));

    // Each direction counts what it *receives* at its source: the carrier→veth
    // thread counts downlink from the carrier, the veth→carrier thread counts
    // uplink from the telephone leg. Read together at teardown, they are the
    // FR-017 both-ways verdict.
    let h1 = std::thread::spawn(move || forward(carrier, veth2, stop, carrier_rx));
    let h2 = std::thread::spawn(move || forward(veth, carrier2, stop2, pbx_rx));
    let _ = h1.join();
    let _ = h2.join();
}

fn forward(
    src: UdpSocket,
    dst: UdpSocket,
    stop: Arc<AtomicBool>,
    counter: Arc<std::sync::atomic::AtomicU64>,
) {
    let mut buf = [0u8; 2048];
    while !stop.load(Ordering::Relaxed) {
        match src.recv(&mut buf) {
            Ok(n) => {
                crate::ims::media_stats::bump(&counter);
                let _ = dst.send(&buf[..n]);
            }
            Err(e)
                if matches!(
                    e.kind(),
                    std::io::ErrorKind::WouldBlock | std::io::ErrorKind::TimedOut
                ) =>
            {
                continue
            }
            Err(_) => break,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::net::Ipv4Addr;

    fn loopback_socket() -> UdpSocket {
        UdpSocket::bind((Ipv4Addr::LOCALHOST, 0)).unwrap()
    }

    #[test]
    fn relay_rtp_forwards_packets_in_both_directions_until_stopped() {
        // Simulate the two "legs": ims_side <-> veth_side, each with its own
        // peer socket standing in for the real remote endpoint.
        let ims_side = loopback_socket();
        let ims_peer = loopback_socket();
        ims_side.connect(ims_peer.local_addr().unwrap()).unwrap();
        ims_peer.connect(ims_side.local_addr().unwrap()).unwrap();

        let veth_side = loopback_socket();
        let veth_peer = loopback_socket();
        veth_side.connect(veth_peer.local_addr().unwrap()).unwrap();
        veth_peer.connect(veth_side.local_addr().unwrap()).unwrap();

        let stop = Arc::new(AtomicBool::new(false));
        let stop_clone = stop.clone();
        let meter = crate::ims::media_stats::MediaMeter::new();
        let carrier_rx = meter.carrier_rx_counter();
        let pbx_rx = meter.pbx_rx_counter();
        let handle = std::thread::spawn(move || {
            relay_rtp(ims_side, veth_side, stop_clone, carrier_rx, pbx_rx)
        });

        // ims_peer -> ims_side -> (relay) -> veth_side -> veth_peer
        ims_peer.send(b"hello-from-ims").unwrap();
        veth_peer
            .set_read_timeout(Some(Duration::from_secs(2)))
            .unwrap();
        let mut buf = [0u8; 64];
        let n = veth_peer.recv(&mut buf).unwrap();
        assert_eq!(&buf[..n], b"hello-from-ims");

        // veth_peer -> veth_side -> (relay) -> ims_side -> ims_peer
        veth_peer.send(b"hello-from-veth").unwrap();
        ims_peer
            .set_read_timeout(Some(Duration::from_secs(2)))
            .unwrap();
        let n = ims_peer.recv(&mut buf).unwrap();
        assert_eq!(&buf[..n], b"hello-from-veth");

        stop.store(true, Ordering::Relaxed);
        handle.join().unwrap();

        // Each direction counted the one packet it carried — the input to the
        // FR-017 both-ways verdict.
        assert_eq!(meter.carrier_rx(), 1, "downlink packet should be counted");
        assert_eq!(meter.pbx_rx(), 1, "uplink packet should be counted");
        assert_eq!(
            meter.verdict(crate::ims::media_stats::DEFAULT_ONE_WAY_THRESHOLD_PERCENT),
            crate::ims::media_stats::DirectionVerdict::BothWays
        );
    }
}
