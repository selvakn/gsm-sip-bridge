//! Bridges strongSwan's `eap-sim-pcsc` plugin to the SIM inside the modem
//! (specs/012-strongswan-epdg, contracts/vpcd-bridge-protocol.md).
//!
//! `eap-sim-pcsc` speaks PC/SC; the SIM is only reachable via `AT+CSIM`.
//! This module implements the virtual-card side of vsmartcard's `vpcd`
//! wire protocol (a length-prefixed TCP framing carrying power/reset/ATR
//! control messages and raw command/response APDUs), forwarding APDUs to
//! the SIM via the existing `modules::usim`/`modules::at_commander`
//! machinery — which also absorbs the EC200U/SIM quirks documented in
//! `docker/patches/0001-ec200u-at-csim-fixes.patch` at this same boundary,
//! rather than patching strongSwan itself.

use crate::error::{BridgeError, BridgeResult};
use crate::modules::at_commander::AtCommander;
use crate::modules::usim;
use std::io::{self, Read, Write};
use std::net::TcpStream;
use std::path::Path;
use std::process::ExitCode;
use std::time::Duration;

/// ISO/IEC 7816-3's minimal legal ATR (direct convention `TS=0x3B`,
/// `T0=0x00` — no interface bytes, no historical bytes, no TCK needed).
/// `AT+CSIM` cannot retrieve the SIM's real ATR (research.md item 2), and
/// PC/SC card selection is driven by AID via SELECT, not ATR parsing — the
/// (untested) working assumption is that `eap-sim-pcsc` treats the ATR as
/// opaque. Confirm at T018 (live APDU trace) and replace with a real
/// captured 3GPP-UICC ATR if the plugin turns out to inspect it.
const CANNED_ATR: &[u8] = &[0x3B, 0x00];

/// Bounded retry window for opening the modem serial port at power-on: the
/// port is shared with the circuit-switched daemon and the IMS agent's own
/// AKA, so a transient "busy" is expected and should be retried, not
/// treated as fatal (contracts/vpcd-bridge-protocol.md's error mapping).
const OPEN_RETRY_ATTEMPTS: u32 = 5;
const OPEN_RETRY_BASE_DELAY: Duration = Duration::from_millis(500);

/// `SW=6F00` ("no precise diagnosis" / card-mute) — returned to the PC/SC
/// client whenever the bridge can't produce a real answer (port unopened,
/// unpowered session, or a forwarding error), so charon's EAP round fails
/// cleanly and retries the whole IKE_AUTH rather than hanging.
fn sw_only(sw1: u8, sw2: u8) -> Vec<u8> {
    vec![sw1, sw2]
}

fn trailing_sw(resp: &[u8]) -> Option<u16> {
    let n = resp.len();
    if n < 2 {
        return None;
    }
    Some(u16::from_be_bytes([resp[n - 2], resp[n - 1]]))
}

/// SELECT (`INS=0xA4`) with a specific `P2` byte (offset 3).
fn is_select_with_p2(apdu: &[u8], p2: u8) -> bool {
    apdu.len() >= 4 && apdu[1] == 0xA4 && apdu[3] == p2
}

fn rewrite_select_p2(apdu: &[u8], new_p2: u8) -> Vec<u8> {
    let mut v = apdu.to_vec();
    if v.len() >= 4 {
        v[3] = new_p2;
    }
    v
}

/// GET RESPONSE (`CLA=0x00 INS=0xC0`).
fn is_get_response(apdu: &[u8]) -> bool {
    apdu.len() >= 2 && apdu[0] == 0x00 && apdu[1] == 0xC0
}

/// 3GPP USIM application RID (TS 101.220) — the prefix every real USIM
/// AID starts with, regardless of the operator-specific suffix.
const USIM_RID: [u8; 7] = [0xA0, 0x00, 0x00, 0x00, 0x87, 0x10, 0x02];

/// SELECT-by-AID (`INS=0xA4 P1=0x04`) redirect: if the client is selecting
/// a USIM application AID (RID matches) that differs from the one this
/// session already discovered via EF_DIR, substitute the discovered AID —
/// different operators' SIMs have different AIDs and a client carrying a
/// hardcoded/generic one would otherwise select nothing on this card
/// (patch item 2's failure mode, generalized to any PC/SC client).
fn redirect_select_aid(apdu: &[u8], discovered_aid: &[u8]) -> Vec<u8> {
    if apdu.len() < 5 || apdu[1] != 0xA4 || apdu[2] != 0x04 {
        return apdu.to_vec();
    }
    let lc = apdu[4] as usize;
    let Some(candidate) = apdu.get(5..5 + lc) else {
        return apdu.to_vec();
    };
    if !candidate.starts_with(&USIM_RID) || candidate == discovered_aid {
        return apdu.to_vec();
    }
    let trailing = &apdu[5 + lc..];
    let mut out = apdu[..4].to_vec();
    out.push(discovered_aid.len() as u8);
    out.extend_from_slice(discovered_aid);
    out.extend_from_slice(trailing);
    out
}

/// Sends one APDU to the SIM via `AT+CSIM` and returns the raw response
/// bytes (data ‖ SW1 SW2), reusing `modules::usim`'s hex codec and
/// `AT+CSIM` framing — the same primitive `usim::authenticate`/
/// `select_usim` already use, exposed at `pub(crate)` visibility for this
/// module without changing behavior for those existing callers.
fn send_raw_apdu(at: &mut AtCommander, apdu: &[u8]) -> BridgeResult<Vec<u8>> {
    let hex = usim::hex_encode(apdu);
    let resp_hex = usim::csim(at, &hex)?;
    if resp_hex.len() < 4 {
        return Err(BridgeError::Ims(format!(
            "+CSIM reply too short to contain a status word: {resp_hex}"
        )));
    }
    usim::hex_decode(&resp_hex)
}

/// Forwards one APDU to the SIM, applying the EC200U/SIM quirk
/// normalizations documented in contracts/vpcd-bridge-protocol.md:
/// SELECT `P2=0x00` retried as `P2=0x0C` on `SW=6B00`, and any `61xx`
/// ("more data") response chased against the modem immediately — mirrors
/// `usim::authenticate`'s existing single-chase behavior, generalized to
/// every APDU instead of just AUTHENTICATE, since any command could hit
/// it depending on card/firmware.
fn forward_apdu(at: &mut AtCommander, apdu: &[u8]) -> BridgeResult<Vec<u8>> {
    let mut resp = send_raw_apdu(at, apdu)?;

    if is_select_with_p2(apdu, 0x00) && trailing_sw(&resp) == Some(0x6B00) {
        tracing::trace!("SELECT P2=0x00 rejected (SW=6B00); retrying with P2=0x0C");
        let retried = rewrite_select_p2(apdu, 0x0C);
        resp = send_raw_apdu(at, &retried)?;
    }

    if let Some(sw) = trailing_sw(&resp) {
        if (sw >> 8) == 0x61 {
            let le = (sw & 0xFF) as u8;
            tracing::trace!(le, "modem returned SW=61xx; chasing GET RESPONSE");
            let get_response = [0x00, 0xC0, 0x00, 0x00, le];
            resp = send_raw_apdu(at, &get_response)?;
        }
    }

    Ok(resp)
}

/// Bounded-retry, backing-off attempt to open the modem serial port —
/// generic over how a fresh `AtCommander` is actually obtained so tests
/// can inject a scripted transport instead of a real serial device.
fn try_open_with_backoff(
    open_modem: &mut impl FnMut() -> BridgeResult<AtCommander>,
) -> BridgeResult<AtCommander> {
    let mut last_err = None;
    for attempt in 0..OPEN_RETRY_ATTEMPTS {
        match open_modem() {
            Ok(at) => return Ok(at),
            Err(e) => {
                tracing::warn!(attempt, error = %e, "modem port unavailable, retrying");
                last_err = Some(e);
                if attempt + 1 < OPEN_RETRY_ATTEMPTS {
                    std::thread::sleep(OPEN_RETRY_BASE_DELAY * (attempt + 1));
                }
            }
        }
    }
    Err(last_err.unwrap())
}

/// One vpcd session's state — data-model.md's
/// `Disconnected → Connected(unpowered) → Powered → Connected(unpowered)`
/// machine, minus `Disconnected` (that's the caller's connection-retry
/// loop, outside this type).
enum SessionState {
    Unpowered,
    Powered {
        serial: AtCommander,
        aid: Vec<u8>,
        /// Last full response (data ‖ SW), for client-issued GET RESPONSE
        /// short-circuiting (vpcd-bridge-protocol.md normalization 1a).
        last_response: Option<Vec<u8>>,
    },
}

struct Session {
    state: SessionState,
}

impl Session {
    fn new() -> Self {
        Self {
            state: SessionState::Unpowered,
        }
    }

    /// Power-on prologue: acquire the serial port and discover the real
    /// USIM AID via EF_DIR (`usim::discover_usim_aid`, which also performs
    /// the SELECT MF a client's own session would start with — redundant
    /// with what the client does next, but idempotent and harmless).
    /// Failure leaves the session `Unpowered` — this is a soft failure
    /// (mutes subsequent APDUs with SW=6F00), not a fatal one.
    fn power_on(&mut self, open_modem: &mut impl FnMut() -> BridgeResult<AtCommander>) {
        if matches!(self.state, SessionState::Powered { .. }) {
            return;
        }
        let mut at = match try_open_with_backoff(open_modem) {
            Ok(at) => at,
            Err(e) => {
                tracing::warn!(error = %e, "modem unavailable at power-on; APDUs will be muted");
                self.state = SessionState::Unpowered;
                return;
            }
        };
        match usim::discover_usim_aid(&mut at) {
            Ok(aid) => {
                tracing::info!(aid = %usim::hex_encode(&aid), "USIM session started");
                self.state = SessionState::Powered {
                    serial: at,
                    aid,
                    last_response: None,
                };
            }
            Err(e) => {
                tracing::warn!(error = %e, "USIM AID discovery failed; APDUs will be muted");
                self.state = SessionState::Unpowered;
            }
        }
    }

    /// Power-off / vpcd disconnect: dropping the `AtCommander` closes the
    /// (exclusively-opened) serial port.
    fn power_off(&mut self) {
        self.state = SessionState::Unpowered;
    }

    /// Reset: if already powered, just clears the response cache (cheap —
    /// no need to re-run EF_DIR discovery, the AID doesn't change
    /// mid-session); if unpowered, attempts the power-on prologue, since
    /// vpcd may send Reset without a preceding Power On.
    fn reset(&mut self, open_modem: &mut impl FnMut() -> BridgeResult<AtCommander>) {
        match &mut self.state {
            SessionState::Powered { last_response, .. } => *last_response = None,
            SessionState::Unpowered => self.power_on(open_modem),
        }
    }

    fn handle_apdu(&mut self, apdu: &[u8]) -> Vec<u8> {
        // T018 (specs/012-strongswan-epdg): the whole point of this trace
        // is to observe, against the real eap-sim-pcsc plugin, which of
        // research.md's unverified assumptions hold — does the client ever
        // issue a literal GET RESPONSE, what P2 does it actually send on
        // SELECT, does it select a foreign AID. `-v` (trace level) is what
        // docker/entrypoint.sh already passes vowifi-usim-bridge.
        tracing::trace!(apdu = %usim::hex_encode(apdu), "APDU from vpcd client");

        let SessionState::Powered {
            serial,
            aid,
            last_response,
        } = &mut self.state
        else {
            tracing::warn!("APDU received while unpowered; responding SW=6F00");
            return sw_only(0x6F, 0x00);
        };

        if is_get_response(apdu) {
            if let Some(cached) = last_response {
                tracing::trace!(
                    resp = %usim::hex_encode(cached),
                    "client issued GET RESPONSE; served from cache, modem not touched"
                );
                return cached.clone();
            }
        }

        let redirected = redirect_select_aid(apdu, aid);
        if redirected != apdu {
            tracing::trace!(
                original = %usim::hex_encode(apdu),
                redirected = %usim::hex_encode(&redirected),
                "SELECT-by-AID redirected to the discovered AID"
            );
        }
        match forward_apdu(serial, &redirected) {
            Ok(resp) => {
                tracing::trace!(resp = %usim::hex_encode(&resp), "APDU response to vpcd client");
                *last_response = Some(resp.clone());
                resp
            }
            Err(e) => {
                tracing::warn!(error = %e, "APDU forwarding failed; responding SW=6F00");
                sw_only(0x6F, 0x00)
            }
        }
    }
}

/// Serves one vpcd connection until it closes (`Ok(())`) or a transport
/// error occurs (`Err`) — generic over the transport (`S`, a real
/// `TcpStream` in production, an in-process socket pair in tests) and over
/// how a fresh `AtCommander` is obtained (`open_modem`, real
/// `AtCommander::open` in production, a scripted transport in tests).
fn serve_vpcd_session<S, F>(mut stream: S, mut open_modem: F) -> BridgeResult<()>
where
    S: Read + Write,
    F: FnMut() -> BridgeResult<AtCommander>,
{
    let mut session = Session::new();
    loop {
        let payload = match read_frame(&mut stream) {
            Ok(Some(p)) => p,
            Ok(None) => return Ok(()),
            Err(e) => return Err(BridgeError::Ims(format!("vpcd read failed: {e}"))),
        };
        let msg = VpcdMessage::from_payload(&payload);
        tracing::trace!(?msg, "vpcd message");
        match msg {
            VpcdMessage::PowerOff => session.power_off(),
            VpcdMessage::PowerOn => session.power_on(&mut open_modem),
            VpcdMessage::Reset => session.reset(&mut open_modem),
            VpcdMessage::RequestAtr => write_frame(&mut stream, CANNED_ATR)
                .map_err(|e| BridgeError::Ims(format!("vpcd write failed: {e}")))?,
            VpcdMessage::Apdu(apdu) => {
                let resp = session.handle_apdu(&apdu);
                write_frame(&mut stream, &resp)
                    .map_err(|e| BridgeError::Ims(format!("vpcd write failed: {e}")))?;
            }
            VpcdMessage::UnknownControl(b) => {
                tracing::warn!(
                    byte = format!("{b:#04x}"),
                    "unknown vpcd control byte, ignoring"
                );
            }
        }
    }
}

/// Entry point for the `vowifi-usim-bridge` subcommand: connects to vpcd
/// once and serves the session until it ends. Reconnection is left to the
/// process-level supervisor in `docker/entrypoint.sh` (restart-on-exit
/// with backoff, the same pattern already used for `vowifi-ims-agent`/
/// `vowifi-sip-agent`) rather than duplicated here — vpcd/pcscd is a local,
/// supervised daemon, not a flaky remote peer, so this duty cycle is
/// adequate and keeps this process's own logic simple.
pub fn run(modem_port: &Path, vpcd_host: &str, vpcd_port: u16) -> ExitCode {
    let addr = format!("{vpcd_host}:{vpcd_port}");
    let stream = match TcpStream::connect(&addr) {
        Ok(s) => s,
        Err(e) => {
            tracing::error!(error = %e, addr = %addr, "failed to connect to vpcd");
            return ExitCode::FAILURE;
        }
    };
    tracing::info!(addr = %addr, "connected to vpcd");

    let modem_port = modem_port.to_path_buf();
    match serve_vpcd_session(stream, || AtCommander::open(&modem_port)) {
        Ok(()) => {
            tracing::info!("vpcd session ended cleanly");
            ExitCode::SUCCESS
        }
        Err(e) => {
            tracing::error!(error = %e, "vpcd session ended with error");
            ExitCode::FAILURE
        }
    }
}

/// One vpcd protocol message, as decoded from a frame payload
/// (contracts/vpcd-bridge-protocol.md). Payload length 1 is a control
/// message dispatched on its single byte; any other length is a command
/// APDU forwarded to the SIM.
#[derive(Debug, Clone, PartialEq, Eq)]
enum VpcdMessage {
    PowerOff,
    PowerOn,
    Reset,
    RequestAtr,
    Apdu(Vec<u8>),
    /// A 1-byte control message whose value isn't one of the four known
    /// ones — logged and ignored rather than treated as a malformed APDU.
    UnknownControl(u8),
}

impl VpcdMessage {
    fn from_payload(payload: &[u8]) -> Self {
        if payload.len() == 1 {
            match payload[0] {
                0x00 => VpcdMessage::PowerOff,
                0x01 => VpcdMessage::PowerOn,
                0x02 => VpcdMessage::Reset,
                0x04 => VpcdMessage::RequestAtr,
                other => VpcdMessage::UnknownControl(other),
            }
        } else {
            VpcdMessage::Apdu(payload.to_vec())
        }
    }
}

/// Reads one vpcd frame (2-byte big-endian length prefix + payload) from
/// `stream`. `Ok(None)` on a clean EOF at the length-prefix boundary
/// (vpcd closed the connection between messages); any other short read is
/// a real error (a frame was only partially sent).
fn read_frame(stream: &mut impl Read) -> io::Result<Option<Vec<u8>>> {
    let mut len_buf = [0u8; 2];
    match stream.read_exact(&mut len_buf) {
        Ok(()) => {}
        Err(e) if e.kind() == io::ErrorKind::UnexpectedEof => return Ok(None),
        Err(e) => return Err(e),
    }
    let len = u16::from_be_bytes(len_buf) as usize;
    let mut payload = vec![0u8; len];
    if len > 0 {
        stream.read_exact(&mut payload)?;
    }
    Ok(Some(payload))
}

/// Writes one vpcd frame (2-byte big-endian length prefix + payload) to
/// `stream`.
fn write_frame(stream: &mut impl Write, payload: &[u8]) -> io::Result<()> {
    let len: u16 = payload
        .len()
        .try_into()
        .map_err(|_| io::Error::new(io::ErrorKind::InvalidInput, "vpcd frame payload too large"))?;
    stream.write_all(&len.to_be_bytes())?;
    stream.write_all(payload)?;
    stream.flush()
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::VecDeque;
    use std::io::Cursor;
    use std::net::TcpListener;

    /// A scripted `AtCommander` transport that yields one complete AT
    /// response per underlying `read()` call — unlike a single monolithic
    /// `Cursor`, this survives multiple sequential `send_command` calls.
    /// `AtCommander::read_response` builds a fresh `BufReader` per call,
    /// which over-reads and drops any buffered-but-unconsumed bytes from a
    /// single-shot stream across more than one call (the same pre-existing
    /// quirk documented in `modules::usim`'s own tests); `BufReader::fill`
    /// only ever calls the underlying `read()` once per fill, so handing
    /// back exactly one response's bytes per call keeps each command's
    /// reply isolated to its own `send_command` invocation.
    struct ScriptedModem {
        responses: VecDeque<Vec<u8>>,
        current: Vec<u8>,
        pos: usize,
    }

    impl ScriptedModem {
        /// Each `&str` is one AT response (e.g. `"+CSIM: 4,\"9000\"\r\nOK\r\n"`),
        /// consumed in order, one per `AT+CSIM` command sent.
        fn new(responses: &[&str]) -> Self {
            Self {
                responses: responses.iter().map(|s| s.as_bytes().to_vec()).collect(),
                current: Vec::new(),
                pos: 0,
            }
        }
    }

    impl Read for ScriptedModem {
        fn read(&mut self, buf: &mut [u8]) -> io::Result<usize> {
            if self.pos >= self.current.len() {
                let Some(next) = self.responses.pop_front() else {
                    return Ok(0);
                };
                self.current = next;
                self.pos = 0;
            }
            let remaining = &self.current[self.pos..];
            let n = remaining.len().min(buf.len());
            buf[..n].copy_from_slice(&remaining[..n]);
            self.pos += n;
            Ok(n)
        }
    }

    impl Write for ScriptedModem {
        fn write(&mut self, buf: &[u8]) -> io::Result<usize> {
            Ok(buf.len())
        }
        fn flush(&mut self) -> io::Result<()> {
            Ok(())
        }
    }

    /// The real EC200U/Vi-India EF_DIR record fixture from
    /// `modules::usim`'s own tests, so `discover_usim_aid` extracts a
    /// verified-correct AID (`A0000000871002FFF605FF89000001FF`).
    const EF_DIR_RECORD: &str = "61184F10A0000000871002FFF605FF89000001FF50045553494D9000";

    fn discovery_responses() -> Vec<String> {
        vec![
            "+CSIM: 4,\"9000\"\r\nOK\r\n".to_string(), // SELECT MF
            "+CSIM: 4,\"9000\"\r\nOK\r\n".to_string(), // SELECT EF_DIR
            format!(
                "+CSIM: {},\"{EF_DIR_RECORD}\"\r\nOK\r\n",
                EF_DIR_RECORD.len()
            ), // READ RECORD 1
        ]
    }

    #[test]
    fn vpcd_session_power_on_atr_apdu_power_off_roundtrips_over_real_tcp() {
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let addr = listener.local_addr().unwrap();

        let client = std::thread::spawn(move || {
            let mut stream = TcpStream::connect(addr).unwrap();

            write_frame(&mut stream, &[0x01]).unwrap(); // Power On

            write_frame(&mut stream, &[0x04]).unwrap(); // Request ATR
            let atr = read_frame(&mut stream).unwrap().unwrap();
            assert_eq!(atr, CANNED_ATR);

            // An opaque, non-SELECT, non-GET-RESPONSE APDU — forwarded
            // verbatim, scripted modem reply SW=9000.
            let apdu = [0x00, 0x20, 0x00, 0x00, 0x02, 0x11, 0x22];
            write_frame(&mut stream, &apdu).unwrap();
            let resp = read_frame(&mut stream).unwrap().unwrap();
            assert_eq!(resp, vec![0x90, 0x00]);

            write_frame(&mut stream, &[0x00]).unwrap(); // Power Off
        });

        let (server_stream, _) = listener.accept().unwrap();

        let mut responses = discovery_responses();
        responses.push("+CSIM: 4,\"9000\"\r\nOK\r\n".to_string()); // the opaque APDU
        let mut modem = Some(ScriptedModem::new(
            &responses.iter().map(String::as_str).collect::<Vec<_>>(),
        ));

        let result = serve_vpcd_session(server_stream, move || {
            modem
                .take()
                .map(|m| AtCommander::from_stream(m, Duration::from_secs(1)))
                .ok_or_else(|| BridgeError::Ims("modem already opened once".into()))
        });

        client.join().unwrap();
        assert!(result.is_ok(), "session ended with error: {result:?}");
    }

    #[test]
    fn serve_vpcd_session_returns_ok_on_client_disconnect_after_power_on() {
        // vpcd disconnect mid-session (no explicit Power Off): the session
        // must end cleanly (Ok), dropping the held AtCommander and
        // releasing the modem port, not hang or error.
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let addr = listener.local_addr().unwrap();

        let client = std::thread::spawn(move || {
            let mut stream = TcpStream::connect(addr).unwrap();
            write_frame(&mut stream, &[0x01]).unwrap(); // Power On
                                                        // dropped here, no Power Off
        });

        let (server_stream, _) = listener.accept().unwrap();
        let responses = discovery_responses();
        let mut modem = Some(ScriptedModem::new(
            &responses.iter().map(String::as_str).collect::<Vec<_>>(),
        ));
        let result = serve_vpcd_session(server_stream, move || {
            modem
                .take()
                .map(|m| AtCommander::from_stream(m, Duration::from_secs(1)))
                .ok_or_else(|| BridgeError::Ims("modem already opened once".into()))
        });

        client.join().unwrap();
        assert!(result.is_ok(), "session ended with error: {result:?}");
    }

    // --- T015: APDU normalization fixtures, one per documented quirk from
    // docker/patches/0001-ec200u-at-csim-fixes.patch --------------------

    #[test]
    fn handle_apdu_serves_cached_response_for_get_response_without_touching_modem() {
        // Quirk (a): the modem already delivered full data + SW=9000 for
        // the prior command (EC200U auto-chains internally); a client that
        // still issues a literal GET RESPONSE afterwards must be served
        // from cache, not forwarded to the SIM a second time. An empty
        // ScriptedModem queue proves the modem was never touched: any
        // attempt to send a command would read EOF and produce SW=6F00
        // instead of the cached value.
        let mut session = Session {
            state: SessionState::Powered {
                serial: AtCommander::from_stream(ScriptedModem::new(&[]), Duration::from_secs(1)),
                aid: vec![0xA0, 0x00, 0x00, 0x00, 0x87, 0x10, 0x02, 0xFF],
                last_response: Some(vec![0x11, 0x22, 0x90, 0x00]),
            },
        };
        let resp = session.handle_apdu(&[0x00, 0xC0, 0x00, 0x00, 0x04]);
        assert_eq!(resp, vec![0x11, 0x22, 0x90, 0x00]);
    }

    #[test]
    fn forward_apdu_chases_61xx_response_from_modem() {
        // Quirk (b): if the modem itself ever returns 61xx ("more data"),
        // chase it with a GET RESPONSE against the modem and assemble the
        // real data — mirrors usim::authenticate's existing behavior,
        // generalized to any APDU here.
        let mut at = AtCommander::from_stream(
            ScriptedModem::new(&[
                "+CSIM: 4,\"6104\"\r\nOK\r\n",
                "+CSIM: 12,\"AABBCCDD9000\"\r\nOK\r\n",
            ]),
            Duration::from_secs(1),
        );
        let apdu = [0x00, 0x88, 0x00, 0x81, 0x22];
        let resp = forward_apdu(&mut at, &apdu).unwrap();
        assert_eq!(resp, vec![0xAA, 0xBB, 0xCC, 0xDD, 0x90, 0x00]);
    }

    #[test]
    fn forward_apdu_retries_select_p2_zero_as_p2_0c_on_wrong_params() {
        // Quirk (c): SELECT with P2=0x00 is rejected (SW=6B00) by the
        // cards verified in this project; retry once with P2=0x0C.
        let mut at = AtCommander::from_stream(
            ScriptedModem::new(&["+CSIM: 4,\"6B00\"\r\nOK\r\n", "+CSIM: 4,\"9000\"\r\nOK\r\n"]),
            Duration::from_secs(1),
        );
        let apdu = [0x00, 0xA4, 0x00, 0x00, 0x02, 0x3F, 0x00];
        let resp = forward_apdu(&mut at, &apdu).unwrap();
        assert_eq!(resp, vec![0x90, 0x00]);
    }

    #[test]
    fn forward_apdu_does_not_retry_select_when_p2_already_correct() {
        let mut at = AtCommander::from_stream(
            ScriptedModem::new(&["+CSIM: 4,\"9000\"\r\nOK\r\n"]),
            Duration::from_secs(1),
        );
        let apdu = [0x00, 0xA4, 0x00, 0x0C, 0x02, 0x3F, 0x00];
        let resp = forward_apdu(&mut at, &apdu).unwrap();
        assert_eq!(resp, vec![0x90, 0x00]);
    }

    #[test]
    fn redirect_select_aid_substitutes_discovered_aid_for_foreign_usim_aid() {
        // Quirk (d): different operators' SIMs have different AIDs; a
        // client SELECTing a generic/hardcoded USIM AID gets redirected to
        // the one this session actually discovered via EF_DIR.
        let discovered = [
            0xA0, 0x00, 0x00, 0x00, 0x87, 0x10, 0x02, 0xFF, 0xF6, 0x05, 0xFF, 0x89, 0x00, 0x00,
            0x01, 0xFF,
        ];
        let generic_aid = [
            0xA0, 0x00, 0x00, 0x00, 0x87, 0x10, 0x02, 0xFF, 0xFF, 0xFF, 0xFF, 0x89, 0x03, 0x05,
            0x00, 0x01,
        ];
        let mut apdu = vec![0x00, 0xA4, 0x04, 0x0C, generic_aid.len() as u8];
        apdu.extend_from_slice(&generic_aid);

        let redirected = redirect_select_aid(&apdu, &discovered);

        let mut expected = vec![0x00, 0xA4, 0x04, 0x0C, discovered.len() as u8];
        expected.extend_from_slice(&discovered);
        assert_eq!(redirected, expected);
    }

    #[test]
    fn redirect_select_aid_leaves_matching_aid_untouched() {
        let aid = [0xA0, 0x00, 0x00, 0x00, 0x87, 0x10, 0x02, 0xFF];
        let mut apdu = vec![0x00, 0xA4, 0x04, 0x0C, aid.len() as u8];
        apdu.extend_from_slice(&aid);
        assert_eq!(redirect_select_aid(&apdu, &aid), apdu);
    }

    #[test]
    fn redirect_select_aid_ignores_non_usim_rid() {
        let discovered = [0xA0, 0x00, 0x00, 0x00, 0x87, 0x10, 0x02, 0xFF];
        let foreign_rid = [0xFF, 0xFF, 0xFF, 0xFF, 0xFF, 0xFF, 0xFF];
        let mut apdu = vec![0x00, 0xA4, 0x04, 0x0C, foreign_rid.len() as u8];
        apdu.extend_from_slice(&foreign_rid);
        assert_eq!(redirect_select_aid(&apdu, &discovered), apdu);
    }

    #[test]
    fn redirect_select_aid_ignores_non_select_apdu() {
        let discovered = [0xA0, 0x00, 0x00, 0x00, 0x87, 0x10, 0x02, 0xFF];
        let apdu = [0x00, 0x88, 0x00, 0x81, 0x22];
        assert_eq!(redirect_select_aid(&apdu, &discovered), apdu.to_vec());
    }

    #[test]
    fn send_raw_apdu_rejects_non_hex_csim_reply() {
        // Quirk (e): a fragment that isn't valid hex must be rejected, not
        // silently misparsed.
        let mut at = AtCommander::from_stream(
            ScriptedModem::new(&["+CSIM: 4,\"90ZZ\"\r\nOK\r\n"]),
            Duration::from_secs(1),
        );
        assert!(send_raw_apdu(&mut at, &[0x00, 0xA4, 0x00, 0x0C]).is_err());
    }

    // --- T017: error mapping -------------------------------------------

    #[test]
    fn power_on_failure_mutes_subsequent_apdus_with_card_mute() {
        let mut session = Session::new();
        let mut always_fail =
            || -> BridgeResult<AtCommander> { Err(BridgeError::Ims("port busy".into())) };
        session.power_on(&mut always_fail);

        let resp = session.handle_apdu(&[0x00, 0xA4, 0x00, 0x0C]);
        assert_eq!(resp, vec![0x6F, 0x00]);
    }

    #[test]
    fn handle_apdu_while_unpowered_returns_card_mute() {
        let mut session = Session::new();
        let resp = session.handle_apdu(&[0x00, 0xA4, 0x00, 0x0C]);
        assert_eq!(resp, vec![0x6F, 0x00]);
    }

    #[test]
    fn handle_apdu_returns_card_mute_on_modem_error_response() {
        let mut session = Session {
            state: SessionState::Powered {
                serial: AtCommander::from_stream(
                    ScriptedModem::new(&["ERROR\r\n"]),
                    Duration::from_secs(1),
                ),
                aid: vec![0xA0, 0x00, 0x00, 0x00, 0x87, 0x10, 0x02],
                last_response: None,
            },
        };
        let resp = session.handle_apdu(&[0x00, 0x20, 0x00, 0x00, 0x02, 0x11, 0x22]);
        assert_eq!(resp, vec![0x6F, 0x00]);
    }

    #[test]
    fn read_frame_decodes_length_prefixed_payload() {
        let mut buf = Cursor::new(vec![0x00, 0x03, 0xAA, 0xBB, 0xCC]);
        assert_eq!(read_frame(&mut buf).unwrap(), Some(vec![0xAA, 0xBB, 0xCC]));
    }

    #[test]
    fn read_frame_handles_empty_payload() {
        let mut buf = Cursor::new(vec![0x00, 0x00]);
        assert_eq!(read_frame(&mut buf).unwrap(), Some(vec![]));
    }

    #[test]
    fn read_frame_handles_max_length_payload() {
        let payload = vec![0x42u8; 300];
        let mut bytes = vec![0x01, 0x2C]; // 300 in big-endian
        bytes.extend_from_slice(&payload);
        let mut buf = Cursor::new(bytes);
        assert_eq!(read_frame(&mut buf).unwrap(), Some(payload));
    }

    #[test]
    fn read_frame_returns_none_on_clean_eof() {
        let mut buf = Cursor::new(Vec::<u8>::new());
        assert_eq!(read_frame(&mut buf).unwrap(), None);
    }

    #[test]
    fn read_frame_errors_on_short_payload_read() {
        // Length says 5 bytes but only 2 are actually present — a real
        // error (truncated frame), not a clean EOF.
        let mut buf = Cursor::new(vec![0x00, 0x05, 0xAA, 0xBB]);
        assert!(read_frame(&mut buf).is_err());
    }

    #[test]
    fn write_frame_encodes_length_prefix() {
        let mut buf = Vec::new();
        write_frame(&mut buf, &[0x90, 0x00]).unwrap();
        assert_eq!(buf, vec![0x00, 0x02, 0x90, 0x00]);
    }

    #[test]
    fn write_then_read_frame_roundtrips() {
        let mut buf = Vec::new();
        write_frame(&mut buf, &[0x61, 0x62, 0x63]).unwrap();
        let mut cursor = Cursor::new(buf);
        assert_eq!(
            read_frame(&mut cursor).unwrap(),
            Some(vec![0x61, 0x62, 0x63])
        );
    }

    #[test]
    fn vpcd_message_parses_control_bytes() {
        assert_eq!(VpcdMessage::from_payload(&[0x00]), VpcdMessage::PowerOff);
        assert_eq!(VpcdMessage::from_payload(&[0x01]), VpcdMessage::PowerOn);
        assert_eq!(VpcdMessage::from_payload(&[0x02]), VpcdMessage::Reset);
        assert_eq!(VpcdMessage::from_payload(&[0x04]), VpcdMessage::RequestAtr);
    }

    #[test]
    fn vpcd_message_treats_unknown_single_byte_as_unknown_control() {
        assert_eq!(
            VpcdMessage::from_payload(&[0x03]),
            VpcdMessage::UnknownControl(0x03)
        );
    }

    #[test]
    fn vpcd_message_treats_multi_byte_payload_as_apdu() {
        let apdu = [0x00, 0xA4, 0x04, 0x0C, 0x02, 0x3F, 0x00];
        assert_eq!(
            VpcdMessage::from_payload(&apdu),
            VpcdMessage::Apdu(apdu.to_vec())
        );
    }

    #[test]
    fn vpcd_message_treats_empty_payload_as_apdu() {
        // Not a valid control message (needs exactly 1 byte) or a valid
        // APDU (needs at least 4) — but discrimination-by-length still
        // classifies it as "not a 1-byte control", forwarding it into the
        // APDU path where it's rejected there (empty hex -> AT+CSIM
        // failure -> SW=6F00), rather than silently dropped here.
        assert_eq!(VpcdMessage::from_payload(&[]), VpcdMessage::Apdu(vec![]));
    }
}
