use crate::error::PjsipError;
#[cfg(feature = "pjsip-linked")]
use crate::error::PJ_SUCCESS;
use crate::log_bridge;
use std::sync::atomic::{AtomicBool, Ordering};
#[cfg(feature = "pjsip-linked")]
use std::sync::atomic::{AtomicI32, AtomicU64};
#[cfg(any(test, feature = "pjsip-linked"))]
use std::sync::{LazyLock, Mutex};

#[cfg(feature = "pjsip-linked")]
static RINGBACK_ACTIVE: AtomicBool = AtomicBool::new(false);

/// Maps a call's `pjsua_call_id` to the peer call it should be
/// conference-bridged to, for the two-call bridging used by the inbound
/// VoWiFi-to-SIP feature (`specs/011-vowifi-sip-bridge/`, Agent B). Entries
/// are added in pairs by `Endpoint::pair_calls` (both directions) and
/// removed by `Endpoint::unpair_call`. A call with **no** entry here falls
/// back to the original, unconditional slot-0 (sound device) bridging that
/// the existing circuit-switched GSM-to-SIP bridge already relies on — that
/// is the only path exercised by the daemon today, so this table stays
/// empty and behavior is byte-for-byte unchanged for it.
#[cfg(any(test, feature = "pjsip-linked"))]
static BRIDGE_PAIRS: LazyLock<Mutex<std::collections::HashMap<i32, i32>>> =
    LazyLock::new(|| Mutex::new(std::collections::HashMap::new()));

/// Recently disconnected calls and their final SIP status (`None` when PJSIP
/// recorded none), keyed by the call's token (see [`next_call_token`]),
/// captured inside `on_call_state_cb` — the only point where the status is
/// reliably readable, since PJSUA resets the call's slot (and
/// `pjsua_call_get_info` starts failing) as soon as that callback returns.
/// Read by `Call::last_status`.
///
/// Every disconnect is recorded, status or not, because the entry's presence
/// is also the signal `Call::poll_state` waits for: PJSIP marks the call
/// `DISCONNECTED` *before* it runs the callback, so a poll in between would
/// otherwise see the disconnect before its status exists. An instant reject
/// lost that race on a live call (a `603` sent to the carrier as `480`).
///
/// Keyed by token rather than `pjsua_call_id` because PJSUA reuses call ids:
/// another line's call can take a freed id before the old call's owner reads
/// its status, and must not overwrite or erase it. Tokens are never reused,
/// so entries are never cleared on reuse; the oldest is dropped once
/// [`DISCONNECT_STATUS_CAPACITY`] are held. Owners read within one poll
/// (~100 ms) of the disconnect, far inside that bound.
static DISCONNECT_STATUS: std::sync::LazyLock<std::sync::Mutex<DisconnectLog>> =
    std::sync::LazyLock::new(|| std::sync::Mutex::new(std::collections::VecDeque::new()));

/// What the state callback captured when a call ended.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct Disconnect {
    /// PJSIP's final status; `None` when it recorded none.
    pub status: Option<u16>,
    /// The Q.850 cause from `Reason: Q.850;cause=N` on the received failure
    /// final response (4xx-6xx), when there was one. A `Reason` on any other
    /// message (BYE, CANCEL, provisional, 2xx) is never read.
    pub q850: Option<u16>,
    /// The callback's event carried a received response. `false` means the
    /// call ended on a timer, a transport error or a request we sent, so
    /// there was no header to read and only `status` can decide.
    pub response_seen: bool,
}

/// `(token, disconnect)` per recorded disconnect, oldest first.
type DisconnectLog = std::collections::VecDeque<(usize, Disconnect)>;

const DISCONNECT_STATUS_CAPACITY: usize = 64;

/// Next token to hand out; 0 is reserved for "no token" (a call we did not
/// place, whose `user_data` PJSUA leaves null).
static NEXT_CALL_TOKEN: std::sync::atomic::AtomicUsize = std::sync::atomic::AtomicUsize::new(1);

/// A process-unique, never-zero token for a call `Call::make` is about to
/// place. It is passed to PJSUA as the call's `user_data`, so the state
/// callback can attribute a disconnect to this call even after its id is
/// reused.
#[cfg_attr(not(feature = "pjsip-linked"), allow(dead_code))]
pub(crate) fn next_call_token() -> usize {
    NEXT_CALL_TOKEN.fetch_add(1, Ordering::Relaxed)
}

/// What `on_call_state_cb` recorded when the call holding `token`
/// disconnected: `None` if it has not (yet) been recorded, `Some` once it
/// has (its `status` is `None` when PJSIP gave no final status).
#[cfg_attr(not(feature = "pjsip-linked"), allow(dead_code))]
pub(crate) fn disconnect_status(token: usize) -> Option<Disconnect> {
    status_in(
        &DISCONNECT_STATUS.lock().unwrap_or_else(|e| e.into_inner()),
        token,
    )
}

#[cfg_attr(not(feature = "pjsip-linked"), allow(dead_code))]
fn record_disconnect_status(token: usize, disconnect: Disconnect) {
    record_in(
        &mut DISCONNECT_STATUS.lock().unwrap_or_else(|e| e.into_inner()),
        token,
        disconnect,
    );
}

/// [`disconnect_status`] over an explicit log, so tests can run on a log of
/// their own instead of sharing the process-wide one.
fn status_in(log: &DisconnectLog, token: usize) -> Option<Disconnect> {
    log.iter()
        .rev()
        .find(|(t, _)| *t == token)
        .map(|(_, disconnect)| *disconnect)
}

/// [`record_disconnect_status`] over an explicit log.
fn record_in(log: &mut DisconnectLog, token: usize, disconnect: Disconnect) {
    if log.len() >= DISCONNECT_STATUS_CAPACITY {
        log.pop_front();
    }
    log.push_back((token, disconnect));
}

/// The Q.850 cause in one `Reason` header value, e.g.
/// `Q.850;cause=21;text="Call Rejected"` gives `21`.
///
/// A header may list several reasons, comma separated (RFC 3326), as in
/// `SIP;cause=403, Q.850;cause=21`; the first Q.850 one with a cause wins.
///
/// `None` for anything else — only other protocols (`SIP;cause=486`), a
/// missing, non-numeric or out-of-range cause (Q.850 causes are 7 bits), or
/// a value that does not parse. Never an error: a malformed header must not
/// cost the call its classification. `;`, `,` and `cause=` inside a quoted
/// `text="..."` (where `\"` is an escaped quote) are part of the text, not
/// separators or parameters.
#[cfg_attr(not(feature = "pjsip-linked"), allow(dead_code))]
pub(crate) fn parse_q850_cause(value: &str) -> Option<u16> {
    split_unquoted(value, ',')
        .into_iter()
        .find_map(parse_q850_reason)
}

/// The cause of one reason value (`protocol;cause=N;text="..."`), if it is
/// a Q.850 one.
fn parse_q850_reason(reason: &str) -> Option<u16> {
    let mut params = split_unquoted(reason, ';').into_iter();
    if !params.next()?.trim().eq_ignore_ascii_case("Q.850") {
        return None;
    }
    params.find_map(|param| {
        let (key, val) = param.split_once('=')?;
        if !key.trim().eq_ignore_ascii_case("cause") {
            return None;
        }
        val.trim().parse::<u16>().ok().filter(|cause| *cause <= 127)
    })
}

/// Split on `sep`, ignoring separators inside double quotes. Inside quotes a
/// backslash escapes the next character, so `\"` does not end the quoted text.
#[cfg_attr(not(feature = "pjsip-linked"), allow(dead_code))]
fn split_unquoted(value: &str, sep: char) -> Vec<&str> {
    let mut parts = Vec::new();
    let (mut start, mut quoted, mut escaped) = (0, false, false);
    for (i, ch) in value.char_indices() {
        if escaped {
            escaped = false;
        } else if quoted && ch == '\\' {
            escaped = true;
        } else if ch == '"' {
            quoted = !quoted;
        } else if ch == sep && !quoted {
            parts.push(&value[start..i]);
            start = i + ch.len_utf8();
        }
    }
    parts.push(&value[start..]);
    parts
}

/// Calls PJSIP has notified us of via `on_incoming_call` that no Rust-side
/// caller has claimed yet (spec 025, research.md R-007). `Endpoint` exposes
/// this as a polling API (`poll_incoming_call`), the same idiom `Call`
/// already uses for state (`poll_state`) rather than a callback trait — this
/// crate has none, and adding one just for this single event would be the
/// larger change. Pushed in FIFO order in `on_incoming_call_cb`; popped by
/// whichever component owns UAS handling for that account (the daemon's
/// trunk account, or `Account::local` in SIP server mode).
///
/// Carries the request's real transport-level source address (from
/// `pjsip_rx_data::pkt_info`, not any SIP header — headers are caller-chosen
/// text, not proof of anything) so a caller can verify this INVITE actually
/// came from a peer it trusts before acting on it (found in review: the
/// dial-out local port listens on every interface, same as the trunk
/// account's own port, and nothing checked the sender at all before this).
#[cfg(feature = "pjsip-linked")]
static INCOMING_CALLS: LazyLock<
    Mutex<std::collections::VecDeque<(i32, i32, std::net::SocketAddr)>>,
> = LazyLock::new(|| Mutex::new(std::collections::VecDeque::new()));

/// Whether `on_incoming_call_cb` should queue what it receives at all.
/// Defaults to `true` — unchanged behavior for any caller that never
/// touches `Endpoint::set_accept_incoming_calls`. Found live
/// (specs/025-outbound-calling review): `on_incoming_call` is registered
/// unconditionally at `Endpoint::create`, so a deployment with
/// `[outbound].enabled = false` (nothing ever calls `poll_incoming_call`)
/// still queued every unsolicited INVITE this account received forever,
/// answering none of them — an unbounded leak and a caller left hanging
/// with no response at all. Set to `false` for exactly that case; the
/// callback then rejects immediately with `503` instead of queuing.
static INCOMING_CALLS_ACCEPTED: AtomicBool = AtomicBool::new(true);

// Audio level monitor — populated by a per-call sampling thread (slot 0 = sound device).
// tx_level from slot 0 = ALSA capture → bridge = GSM→SIP
// rx_level from slot 0 = bridge → ALSA playback = SIP→GSM
#[cfg(feature = "pjsip-linked")]
static AUDIO_MONITOR_RUNNING: AtomicBool = AtomicBool::new(false);
#[cfg(feature = "pjsip-linked")]
static AUDIO_CALL_SLOT: AtomicI32 = AtomicI32::new(-1);
#[cfg(feature = "pjsip-linked")]
static AUDIO_GSM_TO_SIP_SUM: AtomicU64 = AtomicU64::new(0);
#[cfg(feature = "pjsip-linked")]
static AUDIO_SIP_TO_GSM_SUM: AtomicU64 = AtomicU64::new(0);
#[cfg(feature = "pjsip-linked")]
static AUDIO_SAMPLE_COUNT: AtomicU64 = AtomicU64::new(0);

// Configured GSM→SIP software gain (stored as fixed-point: actual = value / 1000).
// Set once at endpoint creation and read in the media-state callback.
#[cfg(feature = "pjsip-linked")]
static CONF_TX_LEVEL_MILLI: std::sync::atomic::AtomicU32 = std::sync::atomic::AtomicU32::new(1000);

/// The conference bridge's clock rate, in Hz, as configured by the endpoint
/// that created it. Read by the ringback tone generator, whose port PJMEDIA
/// requires to run at exactly the bridge's rate — creating it at a hardcoded
/// 8000 while the bridge runs at 16000 makes `pjsua_conf_add_port` fail and
/// leaves a call ringing in silence.
#[cfg(feature = "pjsip-linked")]
static CONF_CLOCK_RATE: std::sync::atomic::AtomicU32 = std::sync::atomic::AtomicU32::new(8000);

#[derive(Debug, Clone)]
pub struct EndpointConfig {
    pub transport: TransportType,
    pub local_port: u16,
    pub tls_verify: bool,
    /// Clock rate (Hz) of the PJMEDIA conference bridge and of the sound
    /// device. Everything crossing the bridge is resampled to this rate, so it
    /// is the ceiling on audio bandwidth for the whole process: at 8000 a
    /// wideband codec is negotiable but pointless, since PJMEDIA downsamples
    /// it into the bridge and back out again.
    ///
    /// The circuit-switched GSM bridge uses 8000 — its audio comes off the
    /// modem's 8 kHz USB sound device, so there is no wideband to preserve.
    /// The VoWiFi bridge's Agent B uses 16000, which is what lets a carrier's
    /// AMR-WB call reach the PBX as G.722 without being squeezed through 8 kHz
    /// on the way (`specs/011-vowifi-sip-bridge/`).
    pub clock_rate: u32,
    /// PJMEDIA jitter-buffer initial pre-fill (ms). 0 = PJMEDIA default (~80 ms).
    pub jb_init_ms: i32,
    /// PJMEDIA jitter-buffer minimum pre-fetch frames.
    pub jb_min_pre: i32,
    /// PJMEDIA jitter-buffer hard ceiling (ms). -1 = unbounded.
    pub jb_max_ms: i32,
    /// When `true`, PJMEDIA VAD and noise suppression are active on the capture path.
    pub vad_enabled: bool,
    /// Software gain applied to the GSM→SIP path on the PJSUA conference bridge
    /// via `pjsua_conf_adjust_tx_level(sound_dev_slot, tx_level)`.
    /// 1.0 = unity, <1.0 attenuates, >1.0 amplifies.
    pub tx_level: f32,
    /// ALSA capture (GSM→SIP) ring-buffer depth in ms, applied to `pjsua_media_config.snd_rec_latency`.
    /// Larger values absorb scheduling jitter / XRUNs at the cost of one-way latency.
    pub snd_rec_latency_ms: u32,
    /// ALSA playback (SIP→GSM) ring-buffer depth in ms, applied to `pjsua_media_config.snd_play_latency`.
    pub snd_play_latency_ms: u32,
    /// Address to advertise in SIP signaling (the transport's Contact/Via)
    /// and, via every `Account` this endpoint creates, in SDP for RTP
    /// media — instead of PJSIP's own `pj_gethostip()`-based discovery,
    /// which resolves to a private, host-internal address under Docker
    /// host networking. `None` preserves that default discovery unchanged.
    /// Already resolved (never a hostname) — see `specs/050-sip-public-addr/`.
    pub public_addr: Option<std::net::IpAddr>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum TransportType {
    Udp,
    Tcp,
    Tls,
}

/// One codec PJSIP has registered, as `Endpoint::codecs` reports it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CodecInfo {
    /// Unique id, e.g. `"PCMU/8000/1"` or `"G722/16000/1"` — the string
    /// `Endpoint::set_codec_priority` takes.
    pub id: String,
    /// 0-255; 0 means disabled.
    pub priority: u8,
}

/// A `pj_str_t` is a pointer/length pair over memory PJSIP owns, and is not
/// NUL-terminated — so it must be read by length, not as a C string.
#[cfg(feature = "pjsip-linked")]
#[rustfmt::skip]
pub(crate) unsafe fn pj_str_to_string(s: &pjsua_sys::pj_str_t) -> String { // SAFETY: caller passes a pj_str_t PJSIP filled in; ptr/slen are valid together
    if s.ptr.is_null() || s.slen <= 0 {
        return String::new();
    }
    let bytes = std::slice::from_raw_parts(s.ptr as *const u8, s.slen as usize);
    String::from_utf8_lossy(bytes).into_owned()
}

pub struct Endpoint {
    config: EndpointConfig,
    started: bool,
}

impl Endpoint {
    pub fn create(config: EndpointConfig) -> Result<Self, PjsipError> {
        #[cfg(feature = "pjsip-linked")]
        {
            unsafe // SAFETY: Single init path; zeroed configs passed to PJSIP default/init/start APIs until success or destroy
            {
                let status = pjsua_sys::pjsua_create();
                if status != PJ_SUCCESS {
                    return Err(PjsipError::InitFailed(format!(
                        "pjsua_create returned {status}"
                    )));
                }

                let mut cfg: pjsua_sys::pjsua_config = std::mem::zeroed();
                pjsua_sys::pjsua_config_default(&mut cfg);
                cfg.cb.on_call_media_state = Some(on_call_media_state_cb);
                cfg.cb.on_call_state = Some(on_call_state_cb);
                cfg.cb.on_incoming_call = Some(on_incoming_call_cb);
                cfg.cb.on_dtmf_digit = Some(on_dtmf_digit_cb);
                crate::passthrough::install(&mut cfg.cb);
                // RFC 4028 session timers off (neither offered nor accepted).
                // PJSUA's default is OPTIONAL: a phone whose INVITE offers
                // `Session-Expires` makes pjsua the timer's other half, and a
                // missed refresh makes pjsua BYE a healthy call on its own —
                // indistinguishable in the logs from the phone hanging up.
                // The carrier leg's own timer is handled by Agent A
                // (`ims::agent::session_refresh`), not by this stack.
                cfg.use_timer = pjsua_sys::pjsua_sip_timer_use_PJSUA_SIP_TIMER_INACTIVE;

                let mut log_cfg: pjsua_sys::pjsua_logging_config = std::mem::zeroed();
                pjsua_sys::pjsua_logging_config_default(&mut log_cfg);
                log_cfg.level = 4;
                log_cfg.console_level = 0;
                log_cfg.cb = log_bridge::get_log_callback();

                let mut media_cfg: pjsua_sys::pjsua_media_config = std::mem::zeroed();
                pjsua_sys::pjsua_media_config_default(&mut media_cfg);
                media_cfg.clock_rate = config.clock_rate;
                media_cfg.snd_clock_rate = config.clock_rate;
                media_cfg.channel_count = 1;
                CONF_CLOCK_RATE.store(config.clock_rate, Ordering::Relaxed);
                media_cfg.no_vad = if config.vad_enabled { 0 } else { 1 };
                media_cfg.ec_tail_len = 0;
                media_cfg.quality = 10;
                media_cfg.ptime = 20;
                // Size the ALSA sound-device ring buffers. Defaults come from config
                // (150 ms) — a bump over PJSUA's 100/140 ms to ride out scheduling
                // jitter / XRUNs on the EC20 USB-audio capture path.
                media_cfg.snd_rec_latency = config.snd_rec_latency_ms;
                media_cfg.snd_play_latency = config.snd_play_latency_ms;
                tracing::info!(
                    target: "sip",
                    snd_rec_latency_ms = config.snd_rec_latency_ms,
                    snd_play_latency_ms = config.snd_play_latency_ms,
                    "configured ALSA sound-device latency"
                );
                media_cfg.jb_init = config.jb_init_ms;
                media_cfg.jb_min_pre = config.jb_min_pre;
                media_cfg.jb_max = config.jb_max_ms;

                // Store the configured tx_level so the media-state callback can apply it.
                CONF_TX_LEVEL_MILLI.store((config.tx_level * 1000.0) as u32, Ordering::Relaxed);

                let status = pjsua_sys::pjsua_init(&cfg, &log_cfg, &media_cfg);
                if status != PJ_SUCCESS {
                    pjsua_sys::pjsua_destroy();
                    return Err(PjsipError::InitFailed(format!(
                        "pjsua_init returned {status}"
                    )));
                }

                let mut tp_cfg: pjsua_sys::pjsua_transport_config = std::mem::zeroed();
                pjsua_sys::pjsua_transport_config_default(&mut tp_cfg);
                tp_cfg.port = config.local_port as u32;

                // Advertise a configured routable address (Tailscale/VPN,
                // etc.) instead of leaving PJSIP's own `pj_gethostip()`
                // discovery to pick the host's private default-route
                // address — see `specs/050-sip-public-addr/`. `addr_cstr`
                // must outlive `pjsua_transport_create` below, since
                // `tp_cfg.public_addr` only borrows its bytes.
                let public_addr_cstr = config
                    .public_addr
                    .map(|ip| std::ffi::CString::new(ip.to_string()).unwrap());
                if let Some(ref addr_cstr) = public_addr_cstr {
                    tp_cfg.public_addr =
                        pjsua_sys::pj_str(addr_cstr.as_ptr() as *mut std::os::raw::c_char);
                }

                let tp_type = match config.transport {
                    TransportType::Udp => pjsua_sys::pjsip_transport_type_e_PJSIP_TRANSPORT_UDP,
                    TransportType::Tcp => pjsua_sys::pjsip_transport_type_e_PJSIP_TRANSPORT_TCP,
                    TransportType::Tls => pjsua_sys::pjsip_transport_type_e_PJSIP_TRANSPORT_TLS,
                };

                let mut tp_id: pjsua_sys::pjsua_transport_id = -1;
                let status = pjsua_sys::pjsua_transport_create(tp_type, &tp_cfg, &mut tp_id);
                if status != PJ_SUCCESS {
                    pjsua_sys::pjsua_destroy();
                    return Err(PjsipError::TransportCreate(format!(
                        "pjsua_transport_create returned {status}"
                    )));
                }

                let status = pjsua_sys::pjsua_start();
                if status != PJ_SUCCESS {
                    pjsua_sys::pjsua_destroy();
                    return Err(PjsipError::InitFailed(format!(
                        "pjsua_start returned {status}"
                    )));
                }
            }
        }

        #[cfg(not(feature = "pjsip-linked"))]
        {
            log_bridge::install_log_bridge();
            tracing::info!(
                transport = ?config.transport,
                port = config.local_port,
                "PJSIP endpoint created (stub mode - no real PJSIP linked)"
            );
        }

        Ok(Self {
            config,
            started: true,
        })
    }

    pub fn is_started(&self) -> bool {
        self.started
    }

    /// The address this endpoint's transport advertises, if configured —
    /// read by `Account::register`/`Account::local` to apply the same
    /// address to each account's own RTP media config
    /// (`specs/050-sip-public-addr/`).
    pub(crate) fn public_addr(&self) -> Option<std::net::IpAddr> {
        self.config.public_addr
    }

    /// Pops the oldest not-yet-claimed incoming call, if any, as
    /// `(account_id, call_id, source_addr)`. `Call::from_id` turns the
    /// `call_id` into a `Call` the caller can `answer`/`hangup`/`poll_state`
    /// (spec 025, research.md R-007). `source_addr` is the request's real
    /// transport-level source — verify it against whatever this account
    /// considers a trusted peer before dispatching anything the caller
    /// requested (e.g. a dial-out); nothing upstream of this has checked it.
    ///
    /// Stub builds never produce an incoming call — there is no real
    /// `on_incoming_call` to fire — so this always returns `None` there;
    /// unit tests exercise the UAS pipeline via `Call::from_id` directly.
    pub fn poll_incoming_call(&self) -> Option<(i32, i32, std::net::SocketAddr)> {
        #[cfg(feature = "pjsip-linked")]
        {
            INCOMING_CALLS
                .lock()
                .unwrap_or_else(|e| e.into_inner())
                .pop_front()
        }
        #[cfg(not(feature = "pjsip-linked"))]
        {
            None
        }
    }

    /// Whether `on_incoming_call_cb` should queue calls at all — set this
    /// to `false` when nothing in this process ever calls
    /// `poll_incoming_call` (e.g. `[outbound].enabled = false`), so an
    /// unsolicited INVITE gets an immediate `503` instead of queuing
    /// forever with no response (specs/025-outbound-calling review).
    /// Process-global, matching `INCOMING_CALLS` itself — call this once
    /// during setup, before this account can receive real traffic.
    pub fn set_accept_incoming_calls(&self, enabled: bool) {
        INCOMING_CALLS_ACCEPTED.store(enabled, Ordering::Release);
    }

    pub fn ensure_thread_registered(&self) {
        ensure_pjsip_thread();
    }

    pub fn conf_slot_count(&self) -> u32 {
        #[cfg(feature = "pjsip-linked")]
        {
            ensure_pjsip_thread();
        }
        #[cfg(feature = "pjsip-linked")]
        unsafe // SAFETY: pjsua started; pjsua_conf_get_active_ports valid post-init
        {
            pjsua_sys::pjsua_conf_get_active_ports() as u32
        }
        #[cfg(not(feature = "pjsip-linked"))]
        0
    }

    pub fn set_sound_device(&self, capture_id: i32, playback_id: i32) -> Result<(), PjsipError> {
        #[cfg(feature = "pjsip-linked")]
        {
            self.ensure_thread_registered();
            unsafe // SAFETY: Registered PJSIP thread; device IDs valid for pjsua_set_snd_dev
            {
                let status = pjsua_sys::pjsua_set_snd_dev(capture_id, playback_id);
                if status != PJ_SUCCESS {
                    return Err(PjsipError::MediaPort(format!(
                        "pjsua_set_snd_dev returned {status}"
                    )));
                }
            }
            return Ok(());
        }

        #[cfg(not(feature = "pjsip-linked"))]
        {
            let _ = (capture_id, playback_id);
            Ok(())
        }
    }

    pub fn set_null_sound_device(&self) -> Result<(), PjsipError> {
        #[cfg(feature = "pjsip-linked")]
        {
            self.ensure_thread_registered();
            unsafe // SAFETY: Registered PJSIP thread; null sound device is supported after pjsua start
            {
                let status = pjsua_sys::pjsua_set_null_snd_dev();
                if status != PJ_SUCCESS {
                    return Err(PjsipError::MediaPort(format!(
                        "pjsua_set_null_snd_dev returned {status}"
                    )));
                }
            }
            return Ok(());
        }

        #[cfg(not(feature = "pjsip-linked"))]
        Ok(())
    }

    /// Every audio codec PJSIP has registered, with its current priority.
    /// Useful both for choosing what to prioritize and for logging what this
    /// build actually shipped with — whether G.722 or Opus is present depends
    /// on how pjproject was configured, not on anything in this crate.
    pub fn codecs(&self) -> Vec<CodecInfo> {
        #[cfg(feature = "pjsip-linked")]
        {
            self.ensure_thread_registered();
            unsafe // SAFETY: pjsua started; array and count are sized together per the pjsua_enum_codecs contract
            {
                let mut infos: [pjsua_sys::pjsua_codec_info; 32] = std::mem::zeroed();
                let mut count: std::os::raw::c_uint = infos.len() as std::os::raw::c_uint;
                if pjsua_sys::pjsua_enum_codecs(infos.as_mut_ptr(), &mut count) != PJ_SUCCESS {
                    return Vec::new();
                }
                return infos[..count as usize]
                    .iter()
                    .map(|info| CodecInfo {
                        id: pj_str_to_string(&info.codec_id),
                        priority: info.priority,
                    })
                    .collect();
            }
        }
        #[cfg(not(feature = "pjsip-linked"))]
        Vec::new()
    }

    /// Choose the RTP packing PJSIP *offers* for AMR-NB and AMR-WB: octet-aligned
    /// (`octet-align=1`) or bandwidth-efficient (the RFC 4867 default when the
    /// SDP says nothing). When the far end makes the offer PJSIP follows its
    /// `octet-align` regardless; this only decides what we advertise first.
    /// Encoder bitrates are 12.2 kbit/s NB and 12.65 kbit/s WB (spec 085 FR-007);
    /// PJSIP's own AMR-NB default is the lower 5.9 kbit/s.
    ///
    /// Needs a PJSIP built with opencore-amr, so a `pjsip-linked` build against
    /// a libpjproject without it fails to link (the Docker image always has it).
    pub fn set_amr_octet_align(&self, octet_align: bool) -> Result<(), PjsipError> {
        #[cfg(feature = "pjsip-linked")]
        {
            self.ensure_thread_registered();
            // Mirrors pjmedia_codec_amr_config in pjmedia-codec/opencore_amr.h
            // (pj_bool_t is a C int); pjsua.h does not pull that header in, so
            // it is not in the generated bindings.
            #[repr(C)]
            struct AmrConfig {
                octet_align: std::os::raw::c_int,
                bitrate: std::os::raw::c_uint,
            }
            extern "C" {
                fn pjmedia_codec_opencore_amrnb_set_config(cfg: *const AmrConfig) -> i32;
                fn pjmedia_codec_opencore_amrwb_set_config(cfg: *const AmrConfig) -> i32;
            }
            let flag = std::os::raw::c_int::from(octet_align);
            let nb = AmrConfig {
                octet_align: flag,
                bitrate: 12200,
            };
            let wb = AmrConfig {
                octet_align: flag,
                bitrate: 12650,
            };
            // SAFETY: pjsua started so the codec factory exists; the configs are
            // plain C structs that outlive the calls, which copy them.
            let (nb_status, wb_status) = unsafe {
                (
                    pjmedia_codec_opencore_amrnb_set_config(&nb),
                    pjmedia_codec_opencore_amrwb_set_config(&wb),
                )
            };
            if nb_status != PJ_SUCCESS || wb_status != PJ_SUCCESS {
                return Err(PjsipError::MediaPort(format!(
                    "AMR set_config failed (nb {nb_status}, wb {wb_status})"
                )));
            }
            return Ok(());
        }

        #[cfg(not(feature = "pjsip-linked"))]
        {
            let _ = octet_align;
            Ok(())
        }
    }

    /// Whether this build talks to a real PJSIP. Without it `codecs()` is always
    /// empty, which is not a failure; with it, an empty `codecs()` is one.
    pub fn is_linked(&self) -> bool {
        cfg!(feature = "pjsip-linked")
    }

    /// The audio codec negotiated on a live call (e.g. `"AMR-WB"`), or `None`
    /// before its media is active, after it ends, or without `pjsip-linked`.
    pub fn negotiated_codec(&self, call_id: i32) -> Option<String> {
        #[cfg(feature = "pjsip-linked")]
        {
            self.ensure_thread_registered();
            // SAFETY: pjsua is started (self exists); a stale or unknown call_id makes
            // pjsua_call_get_stream_info fail, which negotiated_audio_codec maps to None
            return unsafe { negotiated_audio_codec(call_id) }.map(|(name, _, _)| name);
        }
        #[cfg(not(feature = "pjsip-linked"))]
        {
            let _ = call_id;
            None
        }
    }

    /// Set a codec's priority: 0 disables it entirely, higher values win, and
    /// the order codecs appear in our SDP offer follows it. `codec_id` is the
    /// id `codecs()` reports (e.g. `"G722/16000/1"`); PJSIP also accepts a
    /// partial id, which matches every codec sharing that prefix.
    ///
    /// Priorities are endpoint-global, not per-call. Agent B's two calls (PBX
    /// and veth) therefore share one offer order — which is fine, because the
    /// only peer that *must* land on a specific codec is Agent A, and it picks
    /// from our offer by its own rules rather than by our ordering.
    pub fn set_codec_priority(&self, codec_id: &str, priority: u8) -> Result<(), PjsipError> {
        #[cfg(feature = "pjsip-linked")]
        {
            self.ensure_thread_registered();
            unsafe // SAFETY: pjsua started; pj_str_t borrows codec_id, which outlives the call
            {
                let id = pjsua_sys::pj_str_t {
                    ptr: codec_id.as_ptr() as *mut std::os::raw::c_char,
                    slen: codec_id.len() as pjsua_sys::pj_ssize_t,
                };
                let status = pjsua_sys::pjsua_codec_set_priority(&id, priority);
                if status != PJ_SUCCESS {
                    return Err(PjsipError::MediaPort(format!(
                        "pjsua_codec_set_priority({codec_id}, {priority}) returned {status}"
                    )));
                }
            }
            return Ok(());
        }

        #[cfg(not(feature = "pjsip-linked"))]
        {
            let _ = (codec_id, priority);
            Ok(())
        }
    }

    /// Register two calls to be conference-bridged to *each other* once both
    /// have active media, instead of the default slot-0 (sound device)
    /// bridging `on_call_media_state_cb` otherwise applies. Used by the
    /// inbound VoWiFi-to-SIP bridge (Agent B) to connect its PBX-side leg to
    /// its veth-side leg. Idempotent to call before either call's media has
    /// gone active — the actual `pjsua_conf_connect` calls happen lazily,
    /// the first time either call's media-active callback observes its peer
    /// already has an active conf slot too.
    pub fn pair_calls(&self, call_a: i32, call_b: i32) {
        #[cfg(feature = "pjsip-linked")]
        {
            let mut pairs = BRIDGE_PAIRS.lock().unwrap_or_else(|e| e.into_inner());
            pairs.insert(call_a, call_b);
            pairs.insert(call_b, call_a);
        }
        #[cfg(not(feature = "pjsip-linked"))]
        {
            let _ = (call_a, call_b);
        }
    }

    /// Carry this call's client-leg media on the application's forwarder
    /// socket at `rtp_port` instead of PJSIP's own transport
    /// (specs/120-amr-passthrough-forwarder). Takes effect on the next
    /// [`Endpoint::reinvite_reinit_media`]. `codec` / `clock_rate` name what the
    /// call is forwarding: a client re-INVITE that drops it is answered 488.
    pub fn register_forward(&self, call_id: i32, rtp_port: u16, codec: &str, clock_rate: u32) {
        crate::passthrough::register_forward(call_id, rtp_port, codec, clock_rate);
    }

    /// Keep this call's PJSIP stream paused, so it carries no media at all
    /// (the veth leg of a forwarded call). Returns whether a live stream was
    /// paused immediately.
    pub fn register_quiet(&self, call_id: i32) -> bool {
        crate::passthrough::register_quiet(call_id)
    }

    /// Forget a call's forward/quiet entry; its next media transport is
    /// PJSIP's own again.
    pub fn clear_forward(&self, call_id: i32) {
        crate::passthrough::clear(call_id);
    }

    /// How many times the call's media stream has been created, to tell when a
    /// re-INVITE has taken effect.
    pub fn media_generation(&self, call_id: i32) -> u64 {
        crate::passthrough::media_generation(call_id)
    }

    /// What the client leg negotiated, read from its live stream.
    pub fn client_media(&self, call_id: i32) -> Option<crate::passthrough::ClientMedia> {
        crate::passthrough::client_media(call_id)
    }

    /// Re-INVITE the call and have PJSIP create its media transport again.
    pub fn reinvite_reinit_media(&self, call_id: i32) -> Result<(), PjsipError> {
        crate::passthrough::reinvite_reinit_media(call_id)
    }

    /// Packets PJSIP's own stream `(sent, received)`; zero on a forwarded call.
    pub fn stream_packets(&self, call_id: i32) -> Option<(u32, u32)> {
        crate::passthrough::stream_packets(call_id)
    }

    /// Hold, release and re-INVITE a call. Used by the integration tests that
    /// drive a forwarded call through renegotiation.
    #[doc(hidden)]
    pub fn hold_call(&self, call_id: i32) -> Result<(), PjsipError> {
        crate::passthrough::hold_call(call_id)
    }

    #[doc(hidden)]
    pub fn unhold_call(&self, call_id: i32) -> Result<(), PjsipError> {
        crate::passthrough::unhold_call(call_id)
    }

    #[doc(hidden)]
    pub fn reinvite_plain(&self, call_id: i32) -> Result<(), PjsipError> {
        crate::passthrough::reinvite_plain(call_id)
    }

    /// Disconnect the call's conference slot from every other port.
    pub fn disconnect_from_conference(&self, call_id: i32) {
        crate::passthrough::disconnect_from_conference(call_id);
    }

    /// Remove a call's pairing (both directions), e.g. once it hangs up.
    /// Safe to call even if the call was never paired.
    pub fn unpair_call(&self, call_id: i32) {
        #[cfg(feature = "pjsip-linked")]
        {
            let mut pairs = BRIDGE_PAIRS.lock().unwrap_or_else(|e| e.into_inner());
            if let Some(peer) = pairs.remove(&call_id) {
                pairs.remove(&peer);
            }
        }
        #[cfg(not(feature = "pjsip-linked"))]
        {
            let _ = call_id;
        }
    }

    pub fn find_audio_device(&self, alsa_hint: &str) -> Result<i32, PjsipError> {
        #[cfg(feature = "pjsip-linked")]
        {
            self.ensure_thread_registered();
            let card_num = alsa_hint
                .strip_prefix("hw:")
                .and_then(|s| s.split(',').next())
                .and_then(|s| s.parse::<u32>().ok());

            let alsa_card_name = card_num.and_then(|n| read_alsa_card_name(n));

            unsafe // SAFETY: Registered PJSIP thread; device indices and pjmedia_aud_dev_info out-params match API contract
            {
                let count = pjsua_sys::pjmedia_aud_dev_count() as i32;
                tracing::debug!(
                    count,
                    alsa_hint,
                    ?alsa_card_name,
                    "enumerating PJSIP audio devices"
                );

                for i in 0..count {
                    let mut info: pjsua_sys::pjmedia_aud_dev_info = std::mem::zeroed();
                    let status = pjsua_sys::pjmedia_aud_dev_get_info(i, &mut info);
                    if status != PJ_SUCCESS {
                        continue;
                    }
                    let name = std::ffi::CStr::from_ptr(info.name.as_ptr())
                        .to_string_lossy()
                        .to_string();
                    tracing::debug!(dev_id = i, name = %name, "PJSIP audio device");

                    if let Some(ref card_name) = alsa_card_name {
                        if name.contains(card_name.as_str()) {
                            return Ok(i);
                        }
                    }

                    if let Some(card) = card_num {
                        let card_str = format!("card {card}");
                        let hw_str = format!("hw:{card}");
                        if name.contains(&card_str) || name.contains(&hw_str) {
                            return Ok(i);
                        }
                    }

                    if name.contains(alsa_hint) {
                        return Ok(i);
                    }
                }
            }

            return Err(PjsipError::MediaPort(format!(
                "audio device not found for '{alsa_hint}'"
            )));
        }

        #[cfg(not(feature = "pjsip-linked"))]
        {
            let _ = alsa_hint;
            Ok(0)
        }
    }
}

impl Drop for Endpoint {
    fn drop(&mut self) {
        if self.started {
            #[cfg(feature = "pjsip-linked")]
            {
                // Registers this thread first, exactly as every other method on
                // this type does. `Drop` runs on whatever thread happens to own
                // the value at the end — for the daemon that is a Tokio worker
                // (the endpoint is dropped when `pool_handle.abort()` tears the
                // CardPool down), and pjlib aborts with "Calling pjlib from
                // unknown/external thread" rather than tolerating it.
                //
                // Observed on every container shutdown, live: the assertion
                // fired *after* the clean-shutdown log lines and the exit code
                // was still 0, which is why it read as cosmetic noise for so
                // long. It was a genuine abort inside pjlib.
                ensure_pjsip_thread();
                unsafe // SAFETY: registered thread; pjsua_destroy pairs with successful create/init when started is true
                {
                    pjsua_sys::pjsua_destroy();
                }
            }
            tracing::info!("PJSIP endpoint destroyed");
            self.started = false;
        }
    }
}

pub fn ensure_pjsip_thread() {
    #[cfg(feature = "pjsip-linked")]
    {
        unsafe // SAFETY: pj_thread_register uses thread-local storage so descriptor and handle live for the thread
        {
            if pjsua_sys::pj_thread_is_registered() == 0 {
                thread_local! {
                    static THREAD_DESC: std::cell::RefCell<[u8; 256]> = std::cell::RefCell::new([0u8; 256]);
                    static THREAD_HANDLE: std::cell::RefCell<*mut pjsua_sys::pj_thread_t> =
                        std::cell::RefCell::new(std::ptr::null_mut());
                }
                THREAD_DESC.with(|desc| {
                    THREAD_HANDLE.with(|handle| {
                        let mut desc = desc.borrow_mut();
                        let mut handle = handle.borrow_mut();
                        pjsua_sys::pj_thread_register(
                            b"rust-async\0".as_ptr() as *const std::os::raw::c_char,
                            desc.as_mut_ptr() as *mut _,
                            &mut *handle,
                        );
                    });
                });
            }
        }
    }
}

#[cfg(feature = "pjsip-linked")]
fn read_alsa_card_name(card_num: u32) -> Option<String> {
    let path = format!("/proc/asound/card{card_num}/id");
    std::fs::read_to_string(&path)
        .ok()
        .map(|s| s.trim().to_string())
}

/// The audio codec PJSIP actually negotiated for `call_id` — name (e.g.
/// "PCMU", "AMR-WB"), clock rate in Hz, and channel count — read from the
/// live stream rather than inferred from what we offered, so it reports what
/// the two ends really settled on.
///
/// # Safety
/// Caller must ensure PJSUA is initialized and `call_id` is a valid, active
/// call (both hold inside `on_call_media_state_cb`).
#[cfg(feature = "pjsip-linked")]
#[rustfmt::skip]
unsafe fn negotiated_audio_codec(call_id: pjsua_sys::pjsua_call_id) -> Option<(String, u32, u32)> { // SAFETY: caller guarantees pjsua is initialized and call_id is a live call (see the doc above); stack stream_info is writable for get_stream_info
    let mut si: pjsua_sys::pjsua_stream_info = std::mem::zeroed();
    // Media index 0: these are audio-only calls, so the first stream is it.
    if pjsua_sys::pjsua_call_get_stream_info(call_id, 0, &mut si) != PJ_SUCCESS {
        return None;
    }
    if si.type_ != pjsua_sys::pjmedia_type_PJMEDIA_TYPE_AUDIO {
        return None;
    }
    let fmt = si.info.aud.fmt;
    if fmt.encoding_name.ptr.is_null() || fmt.encoding_name.slen <= 0 {
        return None;
    }
    let name = std::slice::from_raw_parts(
        fmt.encoding_name.ptr as *const u8,
        fmt.encoding_name.slen as usize,
    );
    Some((
        String::from_utf8_lossy(name).into_owned(),
        fmt.clock_rate,
        fmt.channel_cnt,
    ))
}

/// The paired call a DTMF digit received on `call_id` should be re-sent on,
/// or `None` for an unpaired call (the single-call GSM bridge) or a digit
/// that isn't a valid keypad character.
#[cfg(any(test, feature = "pjsip-linked"))]
fn dtmf_relay_target(call_id: i32, digit: i32) -> Option<(i32, char)> {
    let ch = u8::try_from(digit).ok().map(char::from)?;
    if !matches!(ch, '0'..='9' | '*' | '#' | 'A'..='D' | 'a'..='d') {
        return None;
    }
    let pairs = BRIDGE_PAIRS.lock().unwrap_or_else(|e| e.into_inner());
    pairs.get(&call_id).map(|&peer| (peer, ch))
}

/// Relays a DTMF digit received on one leg of a paired call to the other leg.
/// The conference bridge between the legs carries PCM only, so RFC 4733
/// events never cross it on their own.
#[cfg(feature = "pjsip-linked")]
#[rustfmt::skip]
unsafe extern "C" fn on_dtmf_digit_cb(call_id: pjsua_sys::pjsua_call_id, digit: std::os::raw::c_int) { // SAFETY: PJSIP invokes with a valid call_id after init; the digit buffer outlives pjsua_call_dial_dtmf, which copies it
    let Some((peer, ch)) = dtmf_relay_target(call_id, digit) else {
        return;
    };
    let mut buf = [0 as std::os::raw::c_char; 2];
    buf[0] = ch as u8 as std::os::raw::c_char;
    let digits = pjsua_sys::pj_str(buf.as_mut_ptr());
    let status = pjsua_sys::pjsua_call_dial_dtmf(peer, &digits);
    if status == PJ_SUCCESS {
        tracing::debug!(from = call_id, to = peer, digit = %ch, "relayed DTMF digit to paired call");
    } else {
        tracing::warn!(from = call_id, to = peer, digit = %ch, status, "failed to relay DTMF digit to paired call");
    }
}

#[cfg(feature = "pjsip-linked")]
#[rustfmt::skip]
unsafe extern "C" fn on_call_media_state_cb(call_id: pjsua_sys::pjsua_call_id) { // SAFETY: PJSIP invokes with valid call_id after init; stack call_info writable for get_info
    let mut info: pjsua_sys::pjsua_call_info = std::mem::zeroed();
    let status = pjsua_sys::pjsua_call_get_info(call_id, &mut info);
    if status != PJ_SUCCESS {
        return;
    }

    if info.media_status == pjsua_sys::pjsua_call_media_status_PJSUA_CALL_MEDIA_ACTIVE {
        let call_slot = info.conf_slot as i32;

        match negotiated_audio_codec(call_id) {
            Some((name, clock_rate, channels)) => tracing::info!(
                call_id,
                codec = %name,
                sample_rate = clock_rate,
                channels,
                "SIP call media active — negotiated audio codec"
            ),
            None => tracing::warn!(call_id, "SIP call media active but the negotiated codec could not be read"),
        }

        // A forwarded (or silenced veth) call carries its media outside PJSIP
        // (specs/120): its stream is paused, so it must not be connected to the
        // sound device or paired again.
        if crate::passthrough::is_registered(call_id) {
            return;
        }

        let peer_call_id = {
            let pairs = BRIDGE_PAIRS.lock().unwrap_or_else(|e| e.into_inner());
            pairs.get(&call_id).copied()
        };

        if let Some(peer_id) = peer_call_id {
            // Two-call bridging (inbound VoWiFi-to-SIP feature, Agent B):
            // connect this call's slot directly to its paired peer's slot,
            // bypassing slot 0 (the sound device — absent/null in this
            // process) entirely. Deliberately skips the tx_level adjustment
            // and audio-level monitor below, both of which are specific to
            // the single-call, real-sound-device GSM bridge.
            let mut peer_info: pjsua_sys::pjsua_call_info = std::mem::zeroed();
            let got_peer_info = pjsua_sys::pjsua_call_get_info(peer_id, &mut peer_info) == PJ_SUCCESS;
            let peer_slot = peer_info.conf_slot as i32;
            if got_peer_info
                && peer_info.media_status == pjsua_sys::pjsua_call_media_status_PJSUA_CALL_MEDIA_ACTIVE
                && peer_slot >= 0
            {
                pjsua_sys::pjsua_conf_connect(call_slot, peer_slot);
                pjsua_sys::pjsua_conf_connect(peer_slot, call_slot);
                tracing::info!(
                    call_id,
                    peer_id,
                    call_slot,
                    peer_slot,
                    "paired calls' media active, conference-connected to each other"
                );
            } else {
                // Peer isn't active yet, or is active but PJSIP hasn't
                // finished assigning it a conference slot (found live,
                // specs/025-outbound-calling T072 pass 5: `media_status`
                // can read ACTIVE with `conf_slot` still -1 in a narrow
                // window, and calling `pjsua_conf_connect` with a negative
                // slot hits a fatal assertion in PJSIP itself, killing the
                // whole process). Either way, the peer's own media-active
                // callback will find this call already active (via the
                // same `BRIDGE_PAIRS` lookup) and complete the connection
                // symmetrically once its slot is genuinely valid.
                tracing::debug!(
                    call_id,
                    peer_id,
                    peer_slot,
                    "call media active, awaiting paired peer's media (and valid conference slot) to become active"
                );
            }
            return;
        }

        pjsua_sys::pjsua_conf_connect(call_slot, 0);
        pjsua_sys::pjsua_conf_connect(0, call_slot);

        // Apply configured GSM→SIP software gain on the sound-device slot (slot 0).
        let tx_level = CONF_TX_LEVEL_MILLI.load(Ordering::Relaxed) as f32 / 1000.0;
        if (tx_level - 1.0_f32).abs() > 0.001 {
            pjsua_sys::pjsua_conf_adjust_tx_level(0, tx_level);
            tracing::info!(call_id, tx_level, "GSM→SIP conference tx_level adjusted");
        }

        tracing::info!(
            call_id,
            call_slot,
            "call media active, audio connected to sound device"
        );

        // Reset accumulators and start per-second signal-level sampler
        AUDIO_GSM_TO_SIP_SUM.store(0, Ordering::Relaxed);
        AUDIO_SIP_TO_GSM_SUM.store(0, Ordering::Relaxed);
        AUDIO_SAMPLE_COUNT.store(0, Ordering::Relaxed);
        AUDIO_CALL_SLOT.store(call_slot, Ordering::Relaxed);
        AUDIO_MONITOR_RUNNING.store(true, Ordering::Release);

        std::thread::spawn(|| {
            ensure_pjsip_thread();
            while AUDIO_MONITOR_RUNNING.load(Ordering::Acquire) {
                std::thread::sleep(std::time::Duration::from_secs(1));
                if !AUDIO_MONITOR_RUNNING.load(Ordering::Acquire) {
                    break;
                }
                let mut tx: u32 = 0; // GSM→SIP (ALSA capture → bridge)
                let mut rx: u32 = 0; // SIP→GSM (bridge → ALSA playback)
                // SAFETY: pjsua is running; slot 0 is always the sound device
                if pjsua_sys::pjsua_conf_get_signal_level(0, &mut tx, &mut rx)
                    == PJ_SUCCESS
                {
                    AUDIO_GSM_TO_SIP_SUM.fetch_add(tx as u64, Ordering::Relaxed);
                    AUDIO_SIP_TO_GSM_SUM.fetch_add(rx as u64, Ordering::Relaxed);
                    AUDIO_SAMPLE_COUNT.fetch_add(1, Ordering::Relaxed);
                }
            }
        });
    }
}

/// PJSIP invokes this for every INVITE that arrives on an account this
/// endpoint owns — the PBX-facing trunk account and (SIP server mode)
/// `Account::local`, once either accepts `[outbound].enabled`. Records the
/// call for a Rust-side poller rather than answering here: deciding whether
/// to accept, and with what SDP/media, is call-flow logic (`sip::outbound`),
/// not something this crate should encode. PJSIP's invite session sends its
/// own `100 Trying` automatically on receipt, so no response is lost by not
/// answering synchronously inside this callback.
#[cfg(feature = "pjsip-linked")]
#[rustfmt::skip]
unsafe extern "C" fn on_incoming_call_cb( // SAFETY: PJSIP invokes with valid acc_id/call_id after init; rdata is library-owned, read-only, valid for the callback's duration
    acc_id: pjsua_sys::pjsua_acc_id,
    call_id: pjsua_sys::pjsua_call_id,
    rdata: *mut pjsua_sys::pjsip_rx_data,
) {
    if !INCOMING_CALLS_ACCEPTED.load(Ordering::Acquire) {
        tracing::info!(acc_id, call_id, "incoming SIP call refused (not accepting incoming calls)");
        pjsua_sys::pjsua_call_hangup(call_id, 503, std::ptr::null(), std::ptr::null());
        return;
    }
    // The request's real transport-level source (`pkt_info`, filled in by
    // PJSIP's own transport layer from the actual socket the packet arrived
    // on) — not a SIP header, which is caller-chosen text and proves
    // nothing. Callers of `poll_incoming_call` rely on this to verify a
    // dial-out request actually came from a trusted peer (found in review:
    // this account's port listens on every interface, and nothing checked
    // the sender before this existed). `src_name` is PJSIP's own
    // null-terminated, human-readable rendering of the address — safe to
    // read directly, no `pj_sockaddr` union to decode.
    let source_addr = if rdata.is_null() {
        None
    } else {
        let pkt = &(*rdata).pkt_info;
        std::ffi::CStr::from_ptr(pkt.src_name.as_ptr())
            .to_str()
            .ok()
            .and_then(|s| s.parse::<std::net::IpAddr>().ok())
            .map(|ip| std::net::SocketAddr::new(ip, pkt.src_port as u16))
    };
    let Some(source_addr) = source_addr else {
        // Fail closed: a source we cannot verify is not a source we let
        // through, regardless of what `INCOMING_CALLS_ACCEPTED` says.
        tracing::warn!(acc_id, call_id, "incoming SIP call refused (source address unavailable)");
        pjsua_sys::pjsua_call_hangup(call_id, 503, std::ptr::null(), std::ptr::null());
        return;
    };
    tracing::info!(acc_id, call_id, %source_addr, "incoming SIP call");
    INCOMING_CALLS
        .lock()
        .unwrap_or_else(|e| e.into_inner())
        .push_back((acc_id, call_id, source_addr));
}

/// The event kind that carried the message, and the received / sent message
/// data (either may be null). A transaction-state event wraps the message
/// that moved the transaction in `src`.
#[cfg(feature = "pjsip-linked")]
#[rustfmt::skip]
unsafe fn event_parts(event: *const pjsua_sys::pjsip_event) -> (u32, *mut pjsua_sys::pjsip_rx_data, *mut pjsua_sys::pjsip_tx_data) { // SAFETY: event is non-null and library-owned, valid for the callback
    let ev = &*event;
    if ev.type_ == pjsua_sys::pjsip_event_id_e_PJSIP_EVENT_TSX_STATE {
        let t = &ev.body.tsx_state;
        (t.type_, t.src.rdata, t.src.tdata)
    } else {
        (ev.type_, ev.body.rx_msg.rdata, ev.body.tx_msg.tdata)
    }
}

/// What the disconnect event carried from the peer: whether it was a
/// received response at all, and the Q.850 cause of a failure (4xx-6xx)
/// final response's `Reason` header. See [`Disconnect`].
#[cfg(feature = "pjsip-linked")]
#[rustfmt::skip]
unsafe fn received_response_cause(event: *const pjsua_sys::pjsip_event) -> (bool, Option<u16>) { // SAFETY: event is null or the library-owned event passed to on_call_state; the message is read-only and not kept past the callback
    if event.is_null() {
        return (false, None);
    }
    let (kind, rdata, _) = event_parts(event);
    if kind != pjsua_sys::pjsip_event_id_e_PJSIP_EVENT_RX_MSG || rdata.is_null() {
        return (false, None);
    }
    let msg = (*rdata).msg_info.msg;
    if msg.is_null() || (*msg).type_ != pjsua_sys::pjsip_msg_type_e_PJSIP_RESPONSE_MSG {
        return (false, None);
    }
    if !(400..=699).contains(&(*msg).line.status.code) {
        return (true, None);
    }
    // `Reason` has no typed parser in PJSIP, so it arrives as a generic
    // string header. A message may carry several (one `SIP`, one `Q.850`):
    // walk them all and take the first Q.850 cause.
    let name = pjsua_sys::pj_str_t {
        ptr: c"Reason".as_ptr() as *mut std::os::raw::c_char,
        slen: 6,
    };
    let mut start: *const std::os::raw::c_void = std::ptr::null();
    loop {
        let hdr = pjsua_sys::pjsip_msg_find_hdr_by_name(msg, &name, start)
            as *const pjsua_sys::pjsip_generic_string_hdr;
        if hdr.is_null() {
            return (true, None);
        }
        if let Some(cause) = parse_q850_cause(&pj_str_to_string(&(*hdr).hvalue)) {
            return (true, Some(cause));
        }
        start = (*hdr).next as *const std::os::raw::c_void;
    }
}

/// Classifies the PJSIP event that delivered a call's DISCONNECTED state, for
/// the "SIP call disconnected" log line: the event kind plus the SIP message
/// it carried — a request method (`rx_msg BYE` = the peer hung up; `tx_msg BYE`
/// = we did, e.g. a pjsua-initiated timeout) or a response code.
#[cfg(feature = "pjsip-linked")]
#[rustfmt::skip]
unsafe fn disconnect_trigger(event: *const pjsua_sys::pjsip_event) -> String { // SAFETY: event is null or the library-owned event passed to on_call_state, valid for the callback
    if event.is_null() {
        return "none".to_string();
    }
    let (kind, rdata, tdata) = event_parts(event);
    let msg = if kind == pjsua_sys::pjsip_event_id_e_PJSIP_EVENT_RX_MSG && !rdata.is_null() {
        (*rdata).msg_info.msg
    } else if kind == pjsua_sys::pjsip_event_id_e_PJSIP_EVENT_TX_MSG && !tdata.is_null() {
        (*tdata).msg
    } else {
        std::ptr::null_mut()
    };
    if msg.is_null() {
        return trigger_name(kind).to_string();
    }
    let what = if (*msg).type_ == pjsua_sys::pjsip_msg_type_e_PJSIP_REQUEST_MSG {
        pj_str_to_string(&(*msg).line.req.method.name)
    } else {
        (*msg).line.status.code.to_string()
    };
    format!("{} {what}", trigger_name(kind))
}

#[cfg(any(feature = "pjsip-linked", test))]
fn trigger_name(kind: u32) -> &'static str {
    use pjsua_sys::*;
    match kind {
        k if k == pjsip_event_id_e_PJSIP_EVENT_TIMER => "timer",
        k if k == pjsip_event_id_e_PJSIP_EVENT_TX_MSG => "tx_msg",
        k if k == pjsip_event_id_e_PJSIP_EVENT_RX_MSG => "rx_msg",
        k if k == pjsip_event_id_e_PJSIP_EVENT_TRANSPORT_ERROR => "transport_error",
        _ => "other",
    }
}

#[cfg(feature = "pjsip-linked")]
#[rustfmt::skip]
unsafe extern "C" fn on_call_state_cb( // SAFETY: PJSIP invokes with valid call_id after init; stack call_info writable; event is library-managed
    call_id: pjsua_sys::pjsua_call_id,
    event: *mut pjsua_sys::pjsip_event,
) {
    let mut info: pjsua_sys::pjsua_call_info = std::mem::zeroed();
    let status = pjsua_sys::pjsua_call_get_info(call_id, &mut info);
    if status != PJ_SUCCESS {
        return;
    }

    let state = info.state;
    tracing::info!(call_id, state, "SIP call state changed");

    if state == pjsua_sys::pjsip_inv_state_PJSIP_INV_STATE_DISCONNECTED {
        let token = pjsua_sys::pjsua_call_get_user_data(call_id) as usize;
        if token != 0 {
            let status = u16::try_from(info.last_status).ok().filter(|code| *code > 0);
            let (response_seen, q850) = received_response_cause(event);
            record_disconnect_status(
                token,
                Disconnect {
                    status,
                    q850,
                    response_seen,
                },
            );
        }
    }

    match state {
        s if s == pjsua_sys::pjsip_inv_state_PJSIP_INV_STATE_CALLING
            || s == pjsua_sys::pjsip_inv_state_PJSIP_INV_STATE_EARLY =>
        {
            if !RINGBACK_ACTIVE.load(Ordering::Acquire) {
                start_ringback_tone();
            }
        }
        s if s == pjsua_sys::pjsip_inv_state_PJSIP_INV_STATE_CONFIRMED => {
            stop_ringback_tone();
        }
        s if s == pjsua_sys::pjsip_inv_state_PJSIP_INV_STATE_DISCONNECTED => {
            stop_ringback_tone();

            // Why the dialog ended: PJSIP's last status (a BYE we or the peer
            // sent, a 408 from an unanswered request, a 4xx/5xx final
            // response…) and which kind of event delivered it. Without this
            // "phone leg hung up" cannot be told apart from pjsua ending the
            // call itself (ACK/session-timer/transport timeout).
            tracing::info!(
                call_id,
                last_status = info.last_status,
                last_status_text = %pj_str_to_string(&info.last_status_text),
                connect_secs = info.connect_duration.sec,
                total_secs = info.total_duration.sec,
                trigger = %disconnect_trigger(event),
                "SIP call disconnected",
            );

            {
                let mut pairs = BRIDGE_PAIRS.lock().unwrap_or_else(|e| e.into_inner());
                if let Some(peer) = pairs.remove(&call_id) {
                    pairs.remove(&peer);
                }
            }
            crate::passthrough::on_call_disconnected(call_id);

            AUDIO_MONITOR_RUNNING.store(false, Ordering::Release);
            let count = AUDIO_SAMPLE_COUNT.load(Ordering::Relaxed);
            let gsm_to_sip = AUDIO_GSM_TO_SIP_SUM.load(Ordering::Relaxed);
            let sip_to_gsm = AUDIO_SIP_TO_GSM_SUM.load(Ordering::Relaxed);
            if count > 0 {
                tracing::info!(
                    call_id,
                    gsm_to_sip_avg = gsm_to_sip / count,
                    sip_to_gsm_avg = sip_to_gsm / count,
                    gsm_to_sip_total = gsm_to_sip,
                    sip_to_gsm_total = sip_to_gsm,
                    samples = count,
                    "call audio levels (0=silence 255=max)",
                );
            } else {
                // The level monitor samples the *sound-device* slot, which only
                // the circuit-switched GSM bridge uses. A VoWiFi bridge call is
                // conference-connected to its peer call with a null sound device
                // (see `pair_calls`), so it legitimately collects no samples —
                // this is not evidence that its media failed.
                tracing::debug!(
                    call_id,
                    "call ended with no sound-device audio samples (expected for a paired bridge call)"
                );
            }
        }
        _ => {}
    }
}

#[cfg(feature = "pjsip-linked")]
static mut RINGBACK_SLOT: i32 = -1;
#[cfg(feature = "pjsip-linked")]
static mut RINGBACK_PORT: *mut pjsua_sys::pjmedia_port = std::ptr::null_mut();

#[cfg(feature = "pjsip-linked")]
#[rustfmt::skip]
unsafe fn start_ringback_tone() { // SAFETY: Called only from PJSIP call-state callback after pjsua start; statics follow start/stop pairing
    use std::ffi::CString;

    if RINGBACK_ACTIVE.load(Ordering::Acquire) {
        return;
    }

    let pool = pjsua_sys::pjsua_pool_create(b"ringback\0".as_ptr() as *const std::os::raw::c_char, 512, 512);
    if pool.is_null() {
        return;
    }

    let name = CString::new("ringback").unwrap();
    let mut port: *mut pjsua_sys::pjmedia_port = std::ptr::null_mut();
    // A conference port must run at the bridge's own clock rate, or
    // `pjsua_conf_add_port` below rejects it.
    let clock_rate = CONF_CLOCK_RATE.load(Ordering::Relaxed);
    let status = pjsua_sys::pjmedia_tonegen_create(
        pool,
        clock_rate,
        1,               // channel count
        clock_rate / 50, // samples per frame (20ms)
        16,              // bits per sample
        0,               // options
        &mut port,
    );
    if status != PJ_SUCCESS || port.is_null() {
        tracing::warn!(status, clock_rate, "ringback tone generator creation failed");
        return;
    }

    let mut tone = pjsua_sys::pjmedia_tone_desc {
        freq1: 400,
        freq2: 0,
        on_msec: 1000,
        off_msec: 4000,
        volume: 0,
        flags: 0,
    };

    const PJMEDIA_TONEGEN_LOOP: u32 = 1;
    let status = pjsua_sys::pjmedia_tonegen_play(port, 1, &mut tone, PJMEDIA_TONEGEN_LOOP);
    if status != PJ_SUCCESS {
        return;
    }

    let mut slot: i32 = -1;
    let status = pjsua_sys::pjsua_conf_add_port(pool, port, &mut slot);
    if status != PJ_SUCCESS {
        return;
    }

    pjsua_sys::pjsua_conf_connect(slot, 0);

    RINGBACK_PORT = port;
    RINGBACK_SLOT = slot;
    RINGBACK_ACTIVE.store(true, Ordering::Release);
    tracing::info!(slot, "ringback tone started");

    let _ = name;
}

#[cfg(feature = "pjsip-linked")]
#[rustfmt::skip]
unsafe fn stop_ringback_tone() { // SAFETY: Complements start_ringback_tone; conf slot valid when active; statics reset under RINGBACK_ACTIVE guard
    if !RINGBACK_ACTIVE.load(Ordering::Acquire) {
        return;
    }

    if RINGBACK_SLOT >= 0 {
        pjsua_sys::pjsua_conf_disconnect(RINGBACK_SLOT, 0);
        pjsua_sys::pjsua_conf_remove_port(RINGBACK_SLOT);
        RINGBACK_SLOT = -1;
    }

    RINGBACK_PORT = std::ptr::null_mut();
    RINGBACK_ACTIVE.store(false, Ordering::Release);
    tracing::info!("ringback tone stopped");
}

#[cfg(test)]
mod disconnect_trigger_tests {
    use super::trigger_name;
    use pjsua_sys::*;

    #[test]
    fn event_kinds_map_to_the_names_the_log_line_uses() {
        assert_eq!(trigger_name(pjsip_event_id_e_PJSIP_EVENT_TIMER), "timer");
        assert_eq!(trigger_name(pjsip_event_id_e_PJSIP_EVENT_TX_MSG), "tx_msg");
        assert_eq!(trigger_name(pjsip_event_id_e_PJSIP_EVENT_RX_MSG), "rx_msg");
        assert_eq!(
            trigger_name(pjsip_event_id_e_PJSIP_EVENT_TRANSPORT_ERROR),
            "transport_error"
        );
        assert_eq!(trigger_name(pjsip_event_id_e_PJSIP_EVENT_UNKNOWN), "other");
        assert_eq!(trigger_name(pjsip_event_id_e_PJSIP_EVENT_USER), "other");
    }
}

#[cfg(test)]
mod disconnect_status_tests {
    use super::{
        next_call_token, parse_q850_cause, record_in, status_in, Disconnect, DisconnectLog,
        DISCONNECT_STATUS_CAPACITY,
    };

    fn with_status(status: Option<u16>) -> Disconnect {
        Disconnect {
            status,
            ..Disconnect::default()
        }
    }

    /// Two calls that held the same PJSUA call id one after the other keep
    /// separate statuses: the second call's disconnect neither erases nor
    /// replaces the first's rejection (PR #110 review).
    #[test]
    fn a_reused_call_id_keeps_each_calls_status_and_the_store_is_bounded() {
        let mut log = DisconnectLog::new();
        let rejected = next_call_token();
        let reused = next_call_token();
        assert_ne!(rejected, 0);
        assert_ne!(rejected, reused);
        assert_eq!(status_in(&log, rejected), None);

        record_in(&mut log, rejected, with_status(Some(603)));
        record_in(&mut log, reused, with_status(Some(408)));
        assert_eq!(status_in(&log, rejected), Some(with_status(Some(603))));
        assert_eq!(status_in(&log, reused), Some(with_status(Some(408))));

        for _ in 0..DISCONNECT_STATUS_CAPACITY {
            record_in(&mut log, next_call_token(), with_status(Some(486)));
        }
        assert_eq!(status_in(&log, rejected), None);
    }

    /// A disconnect PJSIP gave no status for is still recorded, and is told
    /// apart from one not recorded yet — `Call::poll_state` waits on exactly
    /// that difference.
    #[test]
    fn a_statusless_disconnect_is_recorded_and_distinct_from_none_yet() {
        let mut log = DisconnectLog::new();
        let token = next_call_token();
        assert_eq!(status_in(&log, token), None);
        record_in(&mut log, token, with_status(None));
        assert_eq!(status_in(&log, token), Some(with_status(None)));
    }

    /// The cause travels with the status it arrived with.
    #[test]
    fn the_cause_is_stored_beside_the_status() {
        let mut log = DisconnectLog::new();
        let token = next_call_token();
        let seen = Disconnect {
            status: Some(403),
            q850: Some(21),
            response_seen: true,
        };
        record_in(&mut log, token, seen);
        assert_eq!(status_in(&log, token), Some(seen));
    }

    #[test]
    fn parses_the_q850_cause_from_a_reason_value() {
        for (value, expected) in [
            ("Q.850;cause=21", Some(21)),
            ("Q.850 ;cause=21;text=\"Call Rejected\"", Some(21)),
            ("q.850;CAUSE=17", Some(17)),
            ("Q.850;cause = 21", Some(21)),
            ("Q.850;text=\"x\";cause=21", Some(21)),
            // `;` and `cause=` inside the quoted text are text, not parameters.
            ("Q.850;text=\"a; cause=9\";cause=21", Some(21)),
            ("Q.850;text=\"cause=9\"", None),
            // Several reasons in one header (RFC 3326): the Q.850 one counts
            // wherever it sits, and a comma inside quoted text is not a split.
            ("SIP;cause=403, Q.850;cause=21", Some(21)),
            (
                "SIP;cause=403;text=\"Forbidden\",Q.850;cause=17;text=\"User busy\"",
                Some(17),
            ),
            ("Q.850;text=\"a, SIP;cause=1\";cause=21", Some(21)),
            ("SIP;cause=403, SIP;cause=486", None),
            ("Q.850;cause=abc, Q.850;cause=21", Some(21)),
            // An escaped quote does not end the quoted text.
            ("Q.850;text=\"said \\\"no\";cause=21", Some(21)),
            ("Q.850;text=\"said \\\"; cause=9\";cause=21", Some(21)),
            (
                "Q.850;text=\"ends with a backslash \\\\\";cause=17",
                Some(17),
            ),
            ("Q.850;cause=0", Some(0)),
            ("Q.850;cause=127", Some(127)),
            ("Q.850;cause=128", None),
            ("Q.850;cause=999", None),
            ("Q.850;cause=abc", None),
            ("Q.850;cause=", None),
            ("Q.850", None),
            ("SIP;cause=486;text=\"Busy Here\"", None),
            ("", None),
            (";cause=21", None),
        ] {
            assert_eq!(parse_q850_cause(value), expected, "value: {value:?}");
        }
    }
}

#[cfg(test)]
mod dtmf_relay_tests {
    use super::{dtmf_relay_target, BRIDGE_PAIRS};

    /// Digits received on a paired leg are re-sent on its peer in both
    /// directions; unpaired calls and non-keypad bytes relay nothing.
    #[test]
    fn digits_relay_to_the_paired_call_only() {
        // Call ids no other test in this process uses (the map is global).
        let (a, b, lone) = (9_001, 9_002, 9_003);
        {
            let mut pairs = BRIDGE_PAIRS.lock().unwrap_or_else(|e| e.into_inner());
            pairs.insert(a, b);
            pairs.insert(b, a);
        }
        for ch in "123456789*0#".chars() {
            assert_eq!(dtmf_relay_target(a, ch as i32), Some((b, ch)));
        }
        assert_eq!(dtmf_relay_target(b, '5' as i32), Some((a, '5')));
        assert_eq!(dtmf_relay_target(lone, '5' as i32), None);
        assert_eq!(dtmf_relay_target(a, 'x' as i32), None);
        assert_eq!(dtmf_relay_target(a, -1), None);
        assert_eq!(dtmf_relay_target(a, 0x135), None);
        let mut pairs = BRIDGE_PAIRS.lock().unwrap_or_else(|e| e.into_inner());
        pairs.remove(&a);
        pairs.remove(&b);
    }
}
