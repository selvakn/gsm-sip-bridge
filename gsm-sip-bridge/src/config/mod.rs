pub mod build;
pub mod env;
pub mod raw;
pub mod secret;

use crate::error::{BridgeError, BridgeResult};
use secret::Secret;
use std::path::Path;
use std::time::Duration;
use toml::Value;

pub const ALERTS_TUNNEL_FAILURE_KEYS: &[&str] =
    &["enabled", "discord_webhook_url", "unhealthy_sec"];
pub const ALERTS_REGISTRATION_LOSS_KEYS: &[&str] =
    &["enabled", "discord_webhook_url", "unhealthy_sec"];
const LOGGING_LEVELS: &[&str] = &["trace", "debug", "info", "warn", "error"];

const DEFAULT_SMS_DB_PATH: &str = "/var/lib/gsm-sip-bridge/store.db";
pub const DEFAULT_CONTROL_SOCKET: &str = "/tmp/gsm-sip-bridge.sock";

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum SipTransport {
    Udp,
    Tcp,
    Tls,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum TlsVerify {
    Strict,
    Skip,
}

#[derive(Clone, Debug)]
pub struct SipConfig {
    pub server: String,
    pub port: u16,
    pub username: String,
    pub password: Secret<String>,
    pub transport: SipTransport,
    pub local_port: u16,
    pub display_name: String,
    pub tls_verify: TlsVerify,
}

#[derive(Clone, Debug)]
pub struct BridgeSection {
    pub sip_destination: String,
    pub sip_dial_timeout_sec: u64,
}

/// One IP phone the embedded registrar will accept (spec 024, FR-002).
#[derive(Clone, Debug)]
pub struct SipServerAccount {
    pub username: String,
    pub password: Secret<String>,
}

/// The opt-in SIP-server mode: IP phones REGISTER here instead of the bridge
/// registering to a PBX (spec 024).
///
/// When `enabled`, `[sip].server`/`username`/`password` and
/// `[bridge].sip_destination` become configuration *errors* rather than
/// requirements — there is no PBX for them to describe. See
/// `build::build_sip_server`.
#[derive(Clone, Debug)]
pub struct SipServerConfig {
    pub enabled: bool,
    pub listen_addr: String,
    pub listen_port: u16,
    pub realm: String,
    /// The single account inbound calls ring. Others may register but are
    /// never called — the bridge places one call at a time and does not fork.
    pub ring_aor: String,
    pub min_expires: u32,
    pub max_expires: u32,
    pub nonce_lifetime_sec: u64,
    pub accounts: Vec<SipServerAccount>,
}

impl SipServerConfig {
    /// The configured password for `username`, if that account exists.
    pub fn password_for(&self, username: &str) -> Option<&str> {
        self.accounts
            .iter()
            .find(|a| a.username == username)
            .map(|a| a.password.expose_secret().as_str())
    }

    /// The SIP URI inbound calls appear to come *from*, as the handset sees it.
    ///
    /// Not simply `ring_aor@listen_addr:listen_port`: `listen_addr` defaults to
    /// the wildcard `0.0.0.0`, which would put a literal `sip:1001@0.0.0.0:5060`
    /// in the `From` of every call — an unroutable address in a header the user
    /// may well see on the handset's screen. A wildcard means "every
    /// interface", which is not an identity, so the realm stands in: it is
    /// already this server's authentication domain, is guaranteed non-empty, and
    /// is guaranteed free of characters that would malform the header.
    ///
    /// Observed in the containerised run of spec 024 T071, which logged
    /// `id=sip:1001@0.0.0.0:5060`.
    pub fn identity_uri(&self) -> String {
        format!(
            "sip:{}@{}:{}",
            self.ring_aor,
            self.identity_host(),
            self.listen_port
        )
    }

    /// The `From` PJSIP should put on a call ringing the phone for a specific
    /// mobile caller — same host substitution as [`identity_uri`], but the
    /// caller's own number in place of `ring_aor`. Falls back to
    /// [`identity_uri`] when the caller ID is unknown (withheld or CLIP not
    /// provided by the network), reproducing the pre-existing behaviour for
    /// that case exactly.
    ///
    /// `pjsua_call_make_call` has no per-call override of `From` — only
    /// per-account (`acc_cfg.id`) — so the caller must rewrite the single
    /// server-mode account's identity before each call it places (see
    /// `pjsua_safe::Account::set_identity`).
    pub fn caller_identity_uri(&self, caller_did: &str) -> String {
        if caller_did.is_empty() {
            return self.identity_uri();
        }
        format!(
            "sip:{}@{}:{}",
            caller_did,
            self.identity_host(),
            self.listen_port
        )
    }

    /// Not simply `listen_addr`: see [`identity_uri`]'s doc comment for why a
    /// wildcard address falls back to the realm.
    fn identity_host(&self) -> &str {
        match self.listen_addr.parse::<std::net::IpAddr>() {
            Ok(ip) if ip.is_unspecified() => self.realm.as_str(),
            _ => self.listen_addr.as_str(),
        }
    }
}

/// The opt-in capability to place a call out over the mobile network from
/// the SIP side (spec 025). Off by default; deliberately minimal — no
/// allow-list, no path preference, no per-line knob (see spec.md
/// Clarifications).
#[derive(Clone, Debug, Default)]
pub struct OutboundConfig {
    pub enabled: bool,
}

/// The opt-out switch for the circuit-switched call path (specs/026-
/// disable-circuit-switched). Defaults to enabled — see `RawCs`'s own doc
/// comment on why that default must never be derived.
#[derive(Clone, Debug)]
pub struct CsConfig {
    pub enabled: bool,
}

#[derive(Clone, Debug)]
pub struct SmsConfig {
    pub enabled: bool,
    pub discord_webhook_url: Secret<String>,
    pub db_path: String,
}

#[derive(Clone, Debug)]
pub struct MetricsConfig {
    pub port: u16,
    /// How often each VoWiFi agent re-reports its state
    /// (specs/014-vowifi-metrics-restore). Also sets the staleness
    /// threshold (3x this) after which `metrics::server` marks an agent
    /// down and zeros the gauges it owns.
    pub agent_report_interval_seconds: u64,
}

#[derive(Clone, Debug)]
pub struct ModulesConfig {
    pub retry_interval_sec: u64,
    pub max_concurrent: u32,
}

#[derive(Clone, Debug)]
pub struct ResilienceConfig {
    pub initial_backoff_sec: u64,
    pub max_backoff_sec: u64,
    pub max_retries: u32,
    pub network_loss_timeout_sec: u64,
    pub network_poll_interval_sec: u64,
}

impl Default for ResilienceConfig {
    fn default() -> Self {
        Self {
            initial_backoff_sec: 5,
            max_backoff_sec: 120,
            max_retries: 10,
            network_loss_timeout_sec: 60,
            network_poll_interval_sec: 30,
        }
    }
}

#[derive(Clone, Debug)]
pub struct ControlConfig {
    pub socket_path: String,
}

/// Selects the audio latency preset.  `lan` targets same-machine / local-network SIP servers
/// where there is no packet jitter.  `wan` adds headroom for internet SIP trunks.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum AudioProfile {
    Lan,
    Wan,
}

/// The concrete numeric knobs derived from an `AudioProfile`.
#[derive(Clone, Debug)]
pub struct AudioProfileSettings {
    /// `ArrayQueue` depth for the capture and playback rings (frames of 20 ms each).
    pub ring_capacity: usize,
    /// PJMEDIA jitter-buffer initial pre-fill in milliseconds.
    pub jb_init_ms: i32,
    /// PJMEDIA jitter-buffer minimum pre-fetch frames.
    pub jb_min_pre: i32,
    /// PJMEDIA jitter-buffer hard ceiling in milliseconds.
    pub jb_max_ms: i32,
}

impl AudioProfileSettings {
    pub fn for_profile(profile: &AudioProfile) -> Self {
        match profile {
            AudioProfile::Lan => Self {
                ring_capacity: 4,
                jb_init_ms: 20,
                jb_min_pre: 1,
                jb_max_ms: 40,
            },
            AudioProfile::Wan => Self {
                ring_capacity: 16,
                jb_init_ms: 60,
                jb_min_pre: 2,
                jb_max_ms: 200,
            },
        }
    }
}

/// Shared by every audio path — circuit-switched AND VoWiFi/VoLTE IMS calls
/// (`vowifi::run_telephony_side`, reused by VoLTE per FR-019).
#[derive(Clone, Debug)]
pub struct AudioConfig {
    pub profile: AudioProfile,
    pub settings: AudioProfileSettings,
    /// When `true`, PJMEDIA VAD and noise suppression are active on the capture path.
    /// Disable only for diagnostics; leave enabled in production.
    pub vad: bool,
    /// ALSA capture (GSM→SIP) ring-buffer depth in milliseconds, passed to PJMEDIA as
    /// `snd_rec_latency`. Larger values absorb scheduling jitter / XRUNs at the cost of
    /// added one-way latency. Range 20–2000; default 150 (PJSUA default is 100).
    pub snd_rec_latency_ms: u32,
    /// ALSA playback (SIP→GSM) ring-buffer depth in milliseconds, passed to PJMEDIA as
    /// `snd_play_latency`. Range 20–2000; default 150 (PJSUA default is 140).
    pub snd_play_latency_ms: u32,
}

/// EC20 USB sound-device tuning. Circuit-switched calls ONLY — VoWiFi/VoLTE
/// never touches this modem's ALSA device (`vowifi::run_telephony_side`
/// hard-codes its own tx level and never reads `rx_gain`/`eec_mode`/
/// `rt_audio_prio` at all).
#[derive(Clone, Debug)]
pub struct ModemAudioConfig {
    /// EC20 downlink digital gain sent as `AT+QRXGAIN=<val>` during module init.
    /// Controls how loud SIP audio sounds on the GSM caller's end (SIP→GSM direction).
    /// `None` (default) leaves the modem's firmware default untouched.
    /// Range 0–65535; default varies by audio mode (typically ~32768).
    pub rx_gain: Option<u32>,
    /// EC20 echo-canceller mode word sent as `AT+QEEC=2,<val>` during module init.
    /// Controls which EC subsystems (AEC, DENS noise suppressor, NLPP) are active.
    /// `None` (default) leaves the modem's firmware default untouched.
    /// `Some(0)` disables all EC — recommended for USB audio bridges where there
    /// is no acoustic echo path and the EC only introduces noise artefacts.
    /// Range 0–65535.
    pub eec_mode: Option<u32>,
    /// PJSUA conference-bridge software gain applied to the capture→SIP path
    /// (`pjsua_conf_adjust_tx_level`).  1.0 = unity, <1.0 attenuates, >1.0 amplifies.
    /// Range 0.0–2.0, default 1.0.
    pub tx_level: f32,
    /// `SCHED_FIFO` priority to apply to PJMEDIA's `media` (sound-device) thread once a
    /// call's audio device is open. `0` (default) leaves it at `SCHED_OTHER`. Range 1–99;
    /// 10–30 is recommended. Requires `CAP_SYS_NICE` (privileged container); best-effort,
    /// failures are logged and never fatal.
    pub rt_audio_prio: u32,
}

/// Default ALSA capture latency (ms) — a modest bump over PJSUA's 100 ms to tolerate
/// containerized scheduling jitter without adding excessive one-way delay.
pub const DEFAULT_SND_REC_LATENCY_MS: u32 = 150;
/// Default ALSA playback latency (ms).
pub const DEFAULT_SND_PLAY_LATENCY_MS: u32 = 150;

impl Default for AudioConfig {
    fn default() -> Self {
        let profile = AudioProfile::Lan;
        let settings = AudioProfileSettings::for_profile(&profile);
        Self {
            profile,
            settings,
            vad: true,
            snd_rec_latency_ms: DEFAULT_SND_REC_LATENCY_MS,
            snd_play_latency_ms: DEFAULT_SND_PLAY_LATENCY_MS,
        }
    }
}

impl Default for ModemAudioConfig {
    fn default() -> Self {
        Self {
            rx_gain: None,
            eec_mode: None,
            tx_level: 1.0,
            rt_audio_prio: 0,
        }
    }
}

impl Default for ControlConfig {
    fn default() -> Self {
        Self {
            socket_path: DEFAULT_CONTROL_SOCKET.to_string(),
        }
    }
}

/// tracing filter level, passed to `observability::logging::init`.  One of
/// `trace`, `debug`, `info` (the default), `warn`, `error`.
#[derive(Clone, Debug)]
pub struct LoggingConfig {
    pub level: String,
}

impl Default for LoggingConfig {
    fn default() -> Self {
        Self {
            level: "info".to_string(),
        }
    }
}

#[derive(Clone, Debug)]
pub struct ScheduledRestartConfig {
    pub enabled: bool,
    pub cron: String,
    pub start_jitter_seconds: u64,
    pub inter_card_gap_seconds: u64,
    pub inter_card_gap_jitter_seconds: u64,
    /// `"full"` (default) or `"radio"` — kept a plain `String` all the way
    /// through, same as `[vowifi].tunnel_engine`, and compared with `==` at
    /// the one call site that acts on it (`modules::CardPool::apply_send_reboot`).
    pub restart_mode: String,
}

impl Default for ScheduledRestartConfig {
    fn default() -> Self {
        Self {
            enabled: true,
            cron: "0 1 * * *".to_string(),
            start_jitter_seconds: 600,
            inter_card_gap_seconds: 30,
            inter_card_gap_jitter_seconds: 15,
            restart_mode: "full".to_string(),
        }
    }
}

impl ScheduledRestartConfig {
    pub fn disabled() -> Self {
        Self {
            enabled: false,
            ..Self::default()
        }
    }
}

/// Discord alerting for critical operational events (specs/022-discord-
/// critical-alerts) — generalizes the existing `[sms]` webhook into a
/// shared default plus one enable/disable flag and optional webhook
/// override per category. `sms` defaults to enabled (unchanged behavior);
/// the four new categories default to disabled and require explicit
/// opt-in (FR-007).
#[derive(Clone, Debug)]
pub struct AlertsConfig {
    /// Shared default webhook, used by any category without its own
    /// override. Empty means "no default" (FR-008).
    pub default_webhook_url: Secret<String>,
    pub sms: CategoryAlertConfig,
    pub module_lifecycle: CategoryAlertConfig,
    pub registration_loss: CategoryAlertConfig,
    pub tunnel_failure: CategoryAlertConfig,
    pub missed_call: CategoryAlertConfig,
    /// specs/027-discover-retry-health. No paired `*Thresholds` struct: see
    /// `RawAlerts::line_discovery_failed`'s doc comment for why.
    pub line_discovery_failed: CategoryAlertConfig,
    /// specs/028-gm-tcp-reconnect. Unlike the categories above it defaults
    /// **enabled**: a registered line whose Gm connection cannot be restored
    /// is unambiguously an incident, not something an operator must opt into.
    pub gm_connection_lost: CategoryAlertConfig,
    pub module_lifecycle_thresholds: ModuleLifecycleThresholds,
    pub tunnel_failure_thresholds: TunnelFailureThresholds,
    pub registration_loss_thresholds: RegistrationLossThresholds,
    pub gm_connection_lost_thresholds: GmConnectionLostThresholds,
}

#[derive(Clone, Debug)]
pub struct CategoryAlertConfig {
    pub enabled: bool,
    /// `None` means "use `AlertsConfig::default_webhook_url`".
    pub webhook_url_override: Option<Secret<String>>,
}

impl CategoryAlertConfig {
    fn disabled() -> Self {
        Self {
            enabled: false,
            webhook_url_override: None,
        }
    }
}

/// FR-003, default 60s: how long a module's AT command worker may go
/// without a successful command before it is considered unresponsive.
#[derive(Clone, Copy, Debug)]
pub struct ModuleLifecycleThresholds {
    pub at_worker_unresponsive_sec: u64,
}

impl Default for ModuleLifecycleThresholds {
    fn default() -> Self {
        Self {
            at_worker_unresponsive_sec: 60,
        }
    }
}

/// FR-005, default 300s: how long a VoWiFi line's tunnel may stay
/// non-established before it is considered failed (spec Clarifications
/// Q8 — covers the swu engine's own ~180s bounded establish window plus
/// one steady-state restart cycle).
#[derive(Clone, Copy, Debug)]
pub struct TunnelFailureThresholds {
    pub unhealthy_sec: u64,
}

impl Default for TunnelFailureThresholds {
    fn default() -> Self {
        Self { unhealthy_sec: 300 }
    }
}

/// FR-006, default 300s: how long a line may stay unregistered before it
/// is considered a registration-loss failure (spec Clarifications Q9 —
/// same threshold as tunnel failure, since both sit behind an unbounded
/// auto-restart loop with no natural "give up" signal).
#[derive(Clone, Copy, Debug)]
pub struct RegistrationLossThresholds {
    pub unhealthy_sec: u64,
}

impl Default for RegistrationLossThresholds {
    fn default() -> Self {
        Self { unhealthy_sec: 300 }
    }
}

/// specs/028-gm-tcp-reconnect, default 300s: how long a line's Gm connection
/// may stay down (through reconnects and a re-registration) before it is
/// alerted on. Comfortably longer than detection (~130s) plus the reconnect
/// attempts and the escalation, so a drop that self-heals never pages.
#[derive(Clone, Copy, Debug)]
pub struct GmConnectionLostThresholds {
    pub unhealthy_sec: u64,
}

impl Default for GmConnectionLostThresholds {
    fn default() -> Self {
        Self { unhealthy_sec: 300 }
    }
}

impl Default for AlertsConfig {
    fn default() -> Self {
        Self {
            default_webhook_url: Secret::new(String::new()),
            sms: CategoryAlertConfig {
                enabled: true,
                webhook_url_override: None,
            },
            module_lifecycle: CategoryAlertConfig::disabled(),
            registration_loss: CategoryAlertConfig::disabled(),
            tunnel_failure: CategoryAlertConfig::disabled(),
            missed_call: CategoryAlertConfig::disabled(),
            line_discovery_failed: CategoryAlertConfig::disabled(),
            // Defaults enabled — see the field doc on `AlertsConfig`.
            gm_connection_lost: CategoryAlertConfig {
                enabled: true,
                webhook_url_override: None,
            },
            module_lifecycle_thresholds: ModuleLifecycleThresholds::default(),
            tunnel_failure_thresholds: TunnelFailureThresholds::default(),
            registration_loss_thresholds: RegistrationLossThresholds::default(),
            gm_connection_lost_thresholds: GmConnectionLostThresholds::default(),
        }
    }
}

/// Configuration for the inbound VoWiFi-to-SIP bridge (feature
/// `011-vowifi-sip-bridge`) — a second, independent inbound call path
/// alongside the existing circuit-switched GSM-to-SIP bridge. See
/// `specs/011-vowifi-sip-bridge/plan.md`. Disabled by default: this section
/// only matters when running one of the `vowifi-ims-agent`/`vowifi-sip-agent`
/// subcommands (started automatically by `supervise::orchestrate` when
/// enabled), not for the normal daemon path.
#[derive(Clone, Debug, serde::Serialize, serde::Deserialize)]
pub struct VowifiConfig {
    /// Master switch — the two `vowifi-*-agent` subcommands refuse to start
    /// (see `main.rs`) when this is `false`, so an operator who hasn't
    /// provisioned VoWiFi can't accidentally bring the mode up.
    pub enabled: bool,
    /// This line's Mobile Country Code, e.g. `"404"`. Not a `[vowifi]` TOML
    /// key — set it on a `[[vowifi.line]]` entry instead (`line_overrides`
    /// below); resolved here by `vowifi::discovery::resolve_one_line`. Empty
    /// (the default) means auto-derive it from the SIM at startup — IMSI via
    /// `AT+CIMI`, with the 2-vs-3-digit MNC ambiguity resolved via EF_AD
    /// (`AT+CRSM`), falling back to numeric `AT+COPS`.
    pub mcc: String,
    /// This line's Mobile Network Code, zero-padded to 3 digits, e.g.
    /// `"094"` (Airtel). Same per-line-only sourcing as `mcc`; empty means
    /// auto-derive.
    pub mnc: String,
    /// Serial AT port for the modem whose SIM authenticates the IMS
    /// registration. Resolved per line from discovery/`[[vowifi.line]]`, not
    /// a `[vowifi]` TOML key.
    pub modem_port: String,
    /// Use TCP (not UDP) for the SIP transport to the P-CSCF. `true` is the
    /// combination that reached `200 OK` on Airtel (see `ims::mod` docs).
    pub use_tcp: bool,
    /// Advertise `Require: sec-agree` / `Security-Client` and negotiate Gm
    /// IPsec. Required by networks (e.g. Vi) that reject a plain REGISTER;
    /// also the combination that worked on Airtel.
    pub sec_agree: bool,
    /// Base path Agent A reads the tunnel-assigned P-CSCF address from, written
    /// by `supervise::orchestrate` once this line's tunnel is up.
    ///
    /// This is a *base*: per-line resolution appends `-{index}`, so the
    /// configured `/tmp/pcscf` becomes `/tmp/pcscf-0`, `/tmp/pcscf-1`, ... Each
    /// line's tunnel is assigned its own P-CSCF by its own carrier, so a single
    /// shared file makes concurrently-establishing lines overwrite each other.
    pub pcscf_source_path: String,
    /// Agent A's address on the dedicated veth link (the `ims`-netns end).
    /// Pure per-line infrastructure, always derived from the line's index —
    /// not a `[vowifi]` TOML key.
    pub veth_local_addr: String,
    /// Agent B's address on the dedicated veth link (the default-netns end).
    /// Derived, not configurable — see `veth_local_addr`.
    pub veth_peer_addr: String,
    /// TCP port the Agent A↔B control channel listens on/connects to over
    /// the veth link (`contracts/agent-control-protocol.md`).
    pub control_port: u16,
    /// Carry the carrier's wideband audio all the way to the PBX instead of
    /// narrowing it to 8 kHz at the first hop.
    ///
    /// With this on, Agent A prefers the carrier's AMR-WB (16 kHz) over its
    /// PCMU, hands it to Agent B as `L16/16000` over the veth link, and Agent B
    /// runs a 16 kHz PJMEDIA conference bridge offering G.722 to the PBX. With
    /// it off, every leg is 8 kHz — the behavior before wideband existed.
    ///
    /// Narrowband calls are unaffected either way: a carrier that offers only
    /// PCMU or AMR-NB (both 8 kHz) is answered and bridged exactly as before,
    /// with the veth link staying on PCMU. Turn this off only if the PBX
    /// mishandles a G.722 offer.
    pub wideband: bool,
    /// APN used by the `swu` engine's dialer (specs/011-vowifi-sip-bridge).
    pub apn: String,
    /// Network namespace the ePDG tunnel lives in — created by
    /// `supervise::epdg_iface`, used by both engines. Derived per line, not a
    /// `[vowifi]` TOML key.
    pub netns: String,
    /// Shared override forcing every line's ePDG FQDN, bypassing the
    /// per-line 3GPP-standard derivation from that line's own `mcc`/`mnc`
    /// (which `supervise::orchestrate` performs itself). Empty (the default)
    /// leaves that per-line derivation alone.
    pub epdg_fqdn: String,
    /// Skip DNS resolution and dial this ePDG IP directly. `None` (the
    /// default) means resolve `epdg_fqdn` at startup.
    pub epdg_ip: Option<String>,
    /// Force this as the tunnel's local source address instead of letting
    /// the kernel/charon pick one via routing to the ePDG. `None` (the
    /// default) means auto-select.
    pub src_addr: Option<String>,
    /// Idle-tunnel keepalive interval (seconds) — a TCP connect to the
    /// P-CSCF's SIP port, since operators commonly filter ICMP over the
    /// tunnel (confirmed on Vodafone India).
    pub keepalive_interval_sec: u64,
    /// Name of the veth interface end in the container's default netns
    /// (Agent B's side). Derived per line, not a `[vowifi]` TOML key.
    pub veth_sip_iface: String,
    /// Name of the veth interface end inside `netns` (Agent A's side).
    /// Derived, not configurable — see `veth_sip_iface`.
    pub veth_ims_iface: String,
    /// ePDG tunnel engine: `"strongswan"` (the default — proper IKE
    /// rekeying/re-auth/DPD/MOBIKE, netns survives reconnects) or `"swu"`
    /// (the original SWu-IKEv2 Python dialer, kept as an explicit fallback
    /// — see specs/012-strongswan-epdg).
    pub tunnel_engine: String,
    /// XFRM interface name the strongswan engine creates inside `netns`.
    /// Derived per line, not a `[vowifi]` TOML key.
    pub strongswan_tun_iface: String,
    /// XFRM interface's `if_id`, pinned so it (and `netns`) survive
    /// reconnects (specs/012-strongswan-epdg FR-005/FR-011). Derived, not
    /// configurable — see `strongswan_tun_iface`.
    pub strongswan_if_id: u32,
    /// Host running the vpcd virtual smart-card reader (pcscd's vpcd
    /// driver) that `vowifi-usim-bridge` connects to. Shared across lines.
    pub vpcd_host: String,
    /// Base TCP port pcscd's shared vpcd reader listens on — one reader
    /// serves every line, at `base + index` per line (rendered into
    /// `/etc/reader.conf.d/vpcd` and dialled by `vowifi-usim-bridge`, so
    /// both ends move together). A genuine `[vowifi]` TOML key, unlike the
    /// other per-line infra fields: keep it below the kernel's ephemeral
    /// range (`net.ipv4.ip_local_port_range`) — under `network_mode: host`
    /// an unrelated outbound connection can otherwise grab the port first.
    pub vpcd_port: u16,
    /// Diagnostic escape hatch: use this IMSI instead of reading it from the
    /// SIM via `vowifi-imsi` (`AT+CIMI`). Not a `[vowifi]` TOML key — set it
    /// on a `[[vowifi.line]]` entry instead.
    pub imsi_override: Option<String>,
    /// Diagnostic escape hatch: use this IMEI instead of reading it from the
    /// modem via `AT+CGSN`. Not a `[vowifi]` TOML key — set it on a
    /// `[[vowifi.line]]` entry instead. Same rationale as `imsi_override`:
    /// both IMSI and IMEI are static for a given SIM/modem pair, so pinning
    /// them removes `ims::agent::run_inner`'s only two AT-command
    /// dependencies from the per-registration hot path (every
    /// `vowifi-ims-agent` restart, e.g. on a P-CSCF change) — the path where
    /// a modem AT channel wedged by concurrent circuit-switched/USIM traffic
    /// has caused hours-long registration outages that only a modem power
    /// cycle (`AT+CFUN=1,1`) could clear.
    pub imei_override: Option<String>,
    /// This line's SIM comes from a physical PC/SC reader, not a modem
    /// (specs/023-omnikey-pcsc-vowifi) — threaded through from the matching
    /// `VowifiLineOverride` by `vowifi::discovery::resolve_one_pcsc_line` so
    /// `ims::agent::run` (which only ever sees this derived `VowifiConfig`,
    /// not the raw override) knows to register via `PcscTransport` instead
    /// of opening `modem_port`.
    pub pcsc_reader: bool,
    /// Upper bound on concurrently supported VoWiFi lines
    /// (specs/013-multi-card-vowifi FR-016) — modems discovered beyond this
    /// count are reported and skipped rather than silently dropped. Same
    /// order of magnitude as `[modules].max_concurrent`, the circuit-switched
    /// equivalent.
    pub max_lines: u32,
    /// Explicit per-line overrides (specs/013-multi-card-vowifi FR-009):
    /// pins a specific modem to VoWiFi regardless of the default
    /// audio-capability-based role assignment, and/or fixes that line's
    /// mcc/mnc/imsi instead of auto-deriving them. Empty (the default) means
    /// every VoWiFi line is fully auto-discovered.
    pub line_overrides: Vec<VowifiLineOverride>,
}

/// One `[[vowifi.line]]` entry — see `VowifiConfig::line_overrides`.
#[derive(Clone, Debug, Default, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub struct VowifiLineOverride {
    /// Match a modem by its USB hardware serial (`modules::discovery`'s
    /// `usb_serial`), the same identity `derive_module_id` hashes into a
    /// card id. Preferred over `modem_port` since a serial number survives
    /// the device path changing across reboots/USB re-enumeration.
    pub modem_serial: Option<String>,
    /// Match (or force) a modem by its AT serial device path directly —
    /// the escape hatch for a modem discovery can't identify by serial
    /// (e.g. it reports an empty `serial` sysfs attribute).
    pub modem_port: Option<String>,
    /// Fix this line's home network identity instead of auto-deriving it
    /// from the SIM. Must be set together with `mnc`.
    pub mcc: Option<String>,
    pub mnc: Option<String>,
    /// Diagnostic escape hatch: use this IMSI instead of reading it from the
    /// SIM via `vowifi-imsi` (`AT+CIMI`), scoped to this one line.
    pub imsi_override: Option<String>,
    /// Diagnostic escape hatch: use this IMEI instead of reading it from the
    /// modem via `AT+CGSN`, scoped to this one line.
    pub imei_override: Option<String>,
    /// This line's SIM comes from a physical PC/SC reader (e.g. OmniKey
    /// AG 3x21) rather than a modem (specs/023-omnikey-pcsc-vowifi). When
    /// `true`, `modem_serial`/`modem_port` are not modem matchers (and must
    /// be unset), and `mcc`/`mnc`/`imsi_override` are mandatory — there is no
    /// modem to derive them from. Only meaningful with
    /// `[vowifi].tunnel_engine = "strongswan"`, whose `eap-sim-pcsc` plugin
    /// talks to `pcscd` directly; the `swu` engine has no PC/SC support and
    /// `supervise` fails fast at startup if this is set under it.
    pub pcsc_reader: bool,
}

impl Default for VowifiConfig {
    fn default() -> Self {
        Self {
            enabled: false,
            mcc: String::new(),
            mnc: String::new(),
            modem_port: String::new(),
            use_tcp: true,
            sec_agree: true,
            pcscf_source_path: "/tmp/pcscf".to_string(),
            veth_local_addr: "10.99.0.1".to_string(),
            veth_peer_addr: "10.99.0.2".to_string(),
            control_port: 7050,
            wideband: true,
            apn: "ims".to_string(),
            netns: "ims".to_string(),
            epdg_fqdn: String::new(),
            epdg_ip: None,
            src_addr: None,
            keepalive_interval_sec: 20,
            veth_sip_iface: "veth-sip".to_string(),
            veth_ims_iface: "veth-ims".to_string(),
            tunnel_engine: "strongswan".to_string(),
            strongswan_tun_iface: "tun23".to_string(),
            strongswan_if_id: 23,
            vpcd_host: "127.0.0.1".to_string(),
            vpcd_port: 15963,
            imsi_override: None,
            imei_override: None,
            pcsc_reader: false,
            max_lines: 8,
            line_overrides: Vec::new(),
        }
    }
}

/// `[volte]` — host-side IMS over LTE (specs/015-volte-host-ims).
///
/// Deliberately mirrors the CLI flags of `volte-pdn`/`volte-register` so the
/// two never disagree; the CLI remains the diagnostic entry point and this is
/// what lets the same settings be supplied unattended.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct VolteConfig {
    pub enabled: bool,
    /// File a VoWiFi line wrote its tunnel-assigned P-CSCF to, reused here so
    /// an LTE line can borrow it — this carrier publishes none over LTE.
    ///
    /// Names **one specific VoWiFi line** (`/tmp/pcscf-0` by default, i.e.
    /// line 0). It must: each VoWiFi line's P-CSCF comes from its own carrier,
    /// so with more than one line there is no single "the" address to pick up,
    /// and silently taking whichever was written last is a real bug this
    /// project has already paid for. Point it at another line's file to use
    /// that carrier's address instead.
    pub pcscf_source_path: String,
    pub status_path: String,
    pub lock_path: String,
    /// Answer inbound calls and bridge them to the telephone system
    /// (specs/017-volte-inbound-bridge), rather than only holding the
    /// registration open.
    ///
    /// **Defaults to false**, which leaves the existing arrangement exactly
    /// as it was: the modem-internal path stays available and this section
    /// keeps doing what it did before (FR-021, FR-023, FR-024). Opting in is
    /// what makes the feature safe to merge — an absent selection changes
    /// nothing.
    pub bridge_inbound: bool,
    /// Upper bound on concurrently bridged LTE lines (specs/018-volte-multi-
    /// modem) — the counterpart to `[vowifi].max_lines`. Modems discovered
    /// beyond this count are reported and skipped rather than silently
    /// dropped. Only meaningful with `bridge_inbound` and an empty
    /// `modem_port` (auto-discovery); a pinned single `modem_port` is always
    /// one line.
    pub max_lines: u32,
    /// Explicit per-line overrides (specs/018-volte-multi-modem), the LTE
    /// counterpart to `[[vowifi.line]]`: pins a specific modem to the bridge
    /// and/or fixes that line's `cid`/`apn`/`pcscf`/`iface`/`msisdn` instead
    /// of inheriting them from the `[volte]` base. Empty (the default) means
    /// every line is fully auto-discovered with the base settings.
    pub line_overrides: Vec<VolteLineOverride>,
    /// Base network namespace name for a line's carrier-facing half
    /// (specs/020-volte-line-netns). Line 0 uses this unindexed; later lines
    /// append their index — the LTE analogue of `[vowifi].netns`, on a
    /// distinct default ("volte" vs "ims") so the two subsystems' namespaces
    /// can never collide when both are enabled in the same container
    /// (FR-004a). Pure per-line infrastructure, always derived — not a
    /// `[volte]` TOML key, same as `[vowifi]`'s equivalent fields.
    pub netns: String,
    /// Base name for the veth end inside a line's namespace (the carrier
    /// agent's side) — the LTE analogue of `[vowifi].veth_ims_iface`.
    /// Derived, not configurable — see `netns`.
    pub veth_carrier_iface: String,
    /// Base name for the veth end in the default namespace (the shared
    /// telephony half's side) — the LTE analogue of `[vowifi].veth_sip_iface`.
    /// Derived, not configurable — see `netns`.
    pub veth_telephony_iface: String,
    /// Base `/30` address for the carrier-agent side of the veth link — the
    /// LTE analogue of `[vowifi].veth_local_addr`, on a distinct default
    /// block from `[vowifi]`'s so the two subsystems' veth addressing cannot
    /// collide either. Derived, not configurable — see `netns`.
    pub veth_carrier_addr: String,
    /// Base `/30` address for the telephony-half side of the veth link — the
    /// LTE analogue of `[vowifi].veth_peer_addr`. Derived, not configurable
    /// — see `netns`.
    pub veth_telephony_addr: String,
}

/// One `[[volte.line]]` entry — see `VolteConfig::line_overrides`.
/// The LTE analogue of [`VowifiLineOverride`]: there is no netns/veth/vpcd to
/// carry here, so the per-line fields are the modem's own PDN/P-CSCF settings.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct VolteLineOverride {
    /// Match a modem by its USB hardware serial (`modules::discovery`'s
    /// `usb_serial`) — preferred over `modem_port`, since a serial survives
    /// the device path changing across reboots/USB re-enumeration.
    pub modem_serial: Option<String>,
    /// Match (or pin) a modem by its AT serial device path directly — the
    /// escape hatch for a modem discovery can't identify by serial.
    pub modem_port: Option<String>,
    /// This line's PDP context id. Absent means the default (3).
    pub cid: Option<u8>,
    /// This line's APN. Absent means the default (`"ims"`).
    pub apn: Option<String>,
    /// This line's explicit P-CSCF. Absent means fall back to
    /// `[volte].pcscf_source_path`, then to on-modem discovery — which does
    /// not work on every carrier (see the feature's research notes).
    pub pcscf: Option<String>,
    /// Host data interface bound to this modem's IMS PDN. Empty/absent means
    /// auto-detect from the modem's USB device (see `volte::discovery`).
    pub iface: Option<String>,
    /// This line's own MSISDN, advertised in the P-Preferred-Identity.
    pub msisdn: Option<String>,
}

impl Default for VolteConfig {
    fn default() -> Self {
        Self {
            enabled: false,
            // Same default the [vowifi] section uses, so a captured address is
            // found without configuring anything.
            pcscf_source_path: "/tmp/pcscf-0".to_string(),
            status_path: "/tmp/volte-registration-status".to_string(),
            lock_path: "/tmp/volte-registration.lock".to_string(),
            bridge_inbound: false,
            max_lines: 8,
            line_overrides: Vec::new(),
            netns: "volte".to_string(),
            veth_carrier_iface: "veth-volte-ims".to_string(),
            veth_telephony_iface: "veth-volte-sip".to_string(),
            veth_carrier_addr: "10.98.0.1".to_string(),
            veth_telephony_addr: "10.98.0.2".to_string(),
        }
    }
}

/// A single parsed `[discovery] excluded_ports` entry
/// (specs/030-bad-port-isolation).
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum PortMatcher {
    /// An exact `/dev/ttyUSB*` device path, matched by exact equality. Easy to
    /// copy from logs, but `ttyUSB` numbering is not stable across replug.
    DevicePath(String),
    /// A USB-topology fragment (e.g. `5-1.2.1.2:1.1`), matched against a port's
    /// sysfs interface path by exact-equality OR a boundary-aligned leading
    /// prefix, so a coarser fragment (`5-1.2.1.2`) excludes every interface on
    /// that device. Stable across replug/reboot.
    TopologyPrefix(String),
}

impl PortMatcher {
    /// Parses one `excluded_ports` entry. An empty/whitespace entry is dropped
    /// (returns `None`); a `/dev/...` entry is an exact device path, anything
    /// else is treated as a topology fragment.
    pub fn parse(entry: &str) -> Option<Self> {
        let entry = entry.trim();
        if entry.is_empty() {
            None
        } else if entry.starts_with("/dev/") {
            Some(Self::DevicePath(entry.to_string()))
        } else {
            Some(Self::TopologyPrefix(entry.to_string()))
        }
    }

    /// Whether this matcher excludes a port with the given device path and USB
    /// interface (sysfs) path.
    pub fn matches(&self, device_path: &Path, iface_path: &Path) -> bool {
        match self {
            Self::DevicePath(p) => device_path.to_string_lossy() == p.as_str(),
            Self::TopologyPrefix(frag) => {
                let iface = iface_path.to_string_lossy();
                // Compare against the interface directory name (the last path
                // component), which is the topology fragment, e.g.
                // `5-1.2.1.2:1.1`.
                let name = iface.rsplit('/').next().unwrap_or(&iface);
                topology_prefix_matches(frag, name)
            }
        }
    }
}

/// `frag` matches `name` when it equals it, or is a leading prefix aligned to a
/// topology/interface boundary (`:`, `.`, or `-`) — never an unanchored
/// substring. So `5-1.2.1.2` matches `5-1.2.1.2:1.1`, but `1.2` matches
/// nothing.
fn topology_prefix_matches(frag: &str, name: &str) -> bool {
    if name == frag {
        return true;
    }
    match name.strip_prefix(frag) {
        Some(rest) => rest.starts_with(':') || rest.starts_with('.') || rest.starts_with('-'),
        None => false,
    }
}

/// Runtime view of `[discovery]` (specs/030-bad-port-isolation). Empty
/// `excluded` + the default timeout reproduces pre-feature discovery behavior.
#[derive(Clone, Debug)]
pub struct DiscoveryConfig {
    pub excluded: Vec<PortMatcher>,
    pub probe_timeout: Duration,
}

impl Default for DiscoveryConfig {
    fn default() -> Self {
        Self {
            excluded: Vec::new(),
            probe_timeout: Duration::from_millis(DEFAULT_PROBE_TIMEOUT_MS),
        }
    }
}

/// Default per-port probe abandon budget. 5s (not 3s): the SIM-status probe
/// bounds an open plus `AT+CPIN?` and `AT+CIMI`, each of which can block up to
/// the per-line port read timeout, so the worst healthy case approaches this —
/// too tight a budget would falsely abandon a slow-but-working modem
/// (specs/030-bad-port-isolation).
pub const DEFAULT_PROBE_TIMEOUT_MS: u64 = 5000;

/// Floor for `[discovery].probe_timeout_ms`. A value below this (notably `0`)
/// would abandon every port immediately and, after three scans, quarantine
/// every modem for the process lifetime — bricking discovery with no
/// diagnosable cause. It must also stay above the internal ~800ms AT-open
/// timeout. `build` clamps up to this and warns rather than failing.
pub const MIN_PROBE_TIMEOUT_MS: u64 = 1000;

#[derive(Clone, Debug)]
pub struct AppConfig {
    pub sip: SipConfig,
    pub bridge: BridgeSection,
    pub sms: SmsConfig,
    pub metrics: MetricsConfig,
    pub modules: ModulesConfig,
    pub resilience: ResilienceConfig,
    pub control: ControlConfig,
    pub audio: AudioConfig,
    pub modem_audio: ModemAudioConfig,
    pub scheduled_restart: ScheduledRestartConfig,
    pub vowifi: VowifiConfig,
    pub volte: VolteConfig,
    pub logging: LoggingConfig,
    pub alerts: AlertsConfig,
    pub sip_server: SipServerConfig,
    pub outbound: OutboundConfig,
    pub cs: CsConfig,
    pub discovery: DiscoveryConfig,
}

pub fn load_config(path: &Path) -> BridgeResult<AppConfig> {
    let contents = std::fs::read_to_string(path)
        .map_err(|e| BridgeError::Config(format!("config file {}: {e}", path.display())))?;

    // `toml::from_str`, not `str::parse` — since toml 0.9 the `FromStr` impl
    // parses a bare *value*, so a document starting with `[sip]` is read as an
    // array and rejected with "unexpected content, expected nothing".
    let mut root: Value = toml::from_str(&contents).map_err(BridgeError::from)?;
    if !root.is_table() {
        return Err(BridgeError::Config("config root must be a table".into()));
    }

    // `env:VAR` indirection is resolved over the whole document before serde
    // sees it, so it applies uniformly to every string field rather than
    // needing a wrapper type on each — see `config::env`.
    env::resolve_in_place(&mut root, "")?;

    // A typo'd key is an error, not a warning nobody reads. This walk runs
    // first purely for the message: serde's `deny_unknown_fields` enforces
    // the same rule a line below, but reports only the first offender and
    // names neither the section nor the file.
    let unknown = raw::collect_unknown_keys(&root);
    if !unknown.is_empty() {
        return Err(BridgeError::Config(format!(
            "unknown config {} in {}: {}. Check for a typo, or a setting that \
             was renamed or removed — see docs/configuration.md. Nothing is \
             applied from an unrecognised line.",
            if unknown.len() == 1 { "key" } else { "keys" },
            path.display(),
            unknown.join(", "),
        )));
    }

    let raw: raw::RawConfig = root
        .try_into()
        .map_err(|e: toml::de::Error| BridgeError::Config(e.to_string()))?;

    build::build(raw)
}

/// Best-effort read of `[logging].level`, used to pick the tracing filter
/// before `load_config` runs. Falls back to `"info"` for a missing file,
/// missing section/key, or any parse error — the full `load_config` call
/// (which may legitimately fail, e.g. an unset secret env var) still runs
/// afterwards and reports a real error for an invalid `level` value.
pub fn read_log_level(path: &Path) -> String {
    std::fs::read_to_string(path)
        .ok()
        .and_then(|contents| toml::from_str::<Value>(&contents).ok())
        .and_then(|root| {
            root.as_table()?
                .get("logging")?
                .as_table()?
                .get("level")?
                .as_str()
                .map(str::to_string)
        })
        .unwrap_or_else(|| LoggingConfig::default().level)
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Parses a config fragment through the real pipeline (env resolution ->
    /// serde -> validation), panicking on failure. The whole-document path,
    /// not a per-section one: the old helper called each `parse_*` function
    /// directly, which meant these tests never exercised unknown-key
    /// rejection or cross-section seeding.
    fn parse(toml: &str) -> AppConfig {
        try_parse(toml).expect("fixture must parse")
    }

    fn try_parse(toml: &str) -> BridgeResult<AppConfig> {
        let mut root: Value = toml::from_str(toml).unwrap();
        env::resolve_in_place(&mut root, "")?;
        let unknown = raw::collect_unknown_keys(&root);
        if !unknown.is_empty() {
            return Err(BridgeError::Config(format!(
                "unknown config keys: {}",
                unknown.join(", ")
            )));
        }
        let raw: raw::RawConfig = root
            .try_into()
            .map_err(|e: toml::de::Error| BridgeError::Config(e.to_string()))?;
        build::build(raw)
    }

    #[test]
    fn outbound_disabled_by_default() {
        let c = parse(MINIMAL_TOML);
        assert!(!c.outbound.enabled, "must be opt-in (FR-001)");
    }

    #[test]
    fn outbound_can_be_enabled() {
        let c = parse(&format!("{MINIMAL_TOML}\n[outbound]\nenabled = true\n"));
        assert!(c.outbound.enabled);
    }

    // --- specs/030-bad-port-isolation: [discovery] parsing + PortMatcher ---

    #[test]
    fn discovery_defaults_when_section_absent() {
        let c = parse(MINIMAL_TOML);
        assert!(
            c.discovery.excluded.is_empty(),
            "nothing excluded by default (FR-008)"
        );
        assert_eq!(
            c.discovery.probe_timeout,
            Duration::from_millis(DEFAULT_PROBE_TIMEOUT_MS)
        );
    }

    #[test]
    fn discovery_clamps_a_too_low_probe_timeout() {
        // 0 (or any sub-floor value) would abandon every port and quarantine
        // all modems — clamp up instead of bricking discovery (finding 4).
        let c = parse(&format!(
            "{MINIMAL_TOML}\n[discovery]\nprobe_timeout_ms = 0\n"
        ));
        assert_eq!(
            c.discovery.probe_timeout,
            Duration::from_millis(MIN_PROBE_TIMEOUT_MS)
        );
    }

    #[test]
    fn discovery_keeps_a_probe_timeout_at_or_above_the_floor() {
        let c = parse(&format!(
            "{MINIMAL_TOML}\n[discovery]\nprobe_timeout_ms = 8000\n"
        ));
        assert_eq!(c.discovery.probe_timeout, Duration::from_millis(8000));
    }

    #[test]
    fn discovery_parses_excluded_ports_and_timeout() {
        let c = parse(&format!(
            "{MINIMAL_TOML}\n[discovery]\n\
             excluded_ports = [\"5-1.2.1.2:1.1\", \"/dev/ttyUSB9\"]\n\
             probe_timeout_ms = 4500\n"
        ));
        assert_eq!(c.discovery.excluded.len(), 2);
        assert_eq!(c.discovery.probe_timeout, Duration::from_millis(4500));
    }

    #[test]
    fn discovery_drops_blank_excluded_entries() {
        let c = parse(&format!(
            "{MINIMAL_TOML}\n[discovery]\nexcluded_ports = [\"\", \"  \", \"/dev/ttyUSB1\"]\n"
        ));
        assert_eq!(c.discovery.excluded.len(), 1, "blank entries are dropped");
    }

    #[test]
    fn port_matcher_parse_classifies_entries() {
        assert_eq!(
            PortMatcher::parse("/dev/ttyUSB1"),
            Some(PortMatcher::DevicePath("/dev/ttyUSB1".to_string()))
        );
        assert_eq!(
            PortMatcher::parse("5-1.2.1.2:1.1"),
            Some(PortMatcher::TopologyPrefix("5-1.2.1.2:1.1".to_string()))
        );
        assert_eq!(PortMatcher::parse("   "), None);
        assert_eq!(PortMatcher::parse(""), None);
    }

    #[test]
    fn port_matcher_device_path_is_exact() {
        let m = PortMatcher::parse("/dev/ttyUSB1").unwrap();
        let iface = Path::new("/sys/bus/usb/devices/5-1.2.1.2:1.1");
        assert!(m.matches(Path::new("/dev/ttyUSB1"), iface));
        assert!(!m.matches(Path::new("/dev/ttyUSB10"), iface));
    }

    #[test]
    fn port_matcher_topology_exact_and_whole_device_prefix() {
        let dev = Path::new("/dev/ttyUSB1");
        let iface = Path::new("/sys/bus/usb/devices/5-1.2.1.2:1.1");
        assert!(
            PortMatcher::parse("5-1.2.1.2:1.1")
                .unwrap()
                .matches(dev, iface),
            "exact interface fragment matches"
        );
        assert!(
            PortMatcher::parse("5-1.2.1.2").unwrap().matches(dev, iface),
            "whole-device prefix matches every interface under it"
        );
    }

    #[test]
    fn port_matcher_topology_rejects_unanchored_substring() {
        let dev = Path::new("/dev/ttyUSB1");
        let iface = Path::new("/sys/bus/usb/devices/5-1.2.1.2:1.1");
        assert!(
            !PortMatcher::parse("1.2").unwrap().matches(dev, iface),
            "a mid-token substring must not match"
        );
        assert!(
            !PortMatcher::parse("1.2.1.2:1.1")
                .unwrap()
                .matches(dev, iface),
            "a fragment must be anchored at the start of the interface name"
        );
    }

    #[test]
    fn volte_section_defaults_when_absent() {
        let c = parse(MINIMAL_TOML);

        assert!(!c.volte.enabled, "must be opt-in");
        // Names VoWiFi line 0's file specifically. The VoWiFi path writes
        // `<base>-<index>`, so the pre-per-line `/tmp/pcscf` would never
        // exist and this handoff would silently fall through to on-modem
        // discovery, which does not work on this carrier.
        assert_eq!(c.volte.pcscf_source_path, "/tmp/pcscf-0");
        assert_eq!(c.volte.netns, "volte");
        assert_eq!(c.volte.veth_carrier_iface, "veth-volte-ims");
        assert_eq!(c.volte.veth_telephony_iface, "veth-volte-sip");
        assert_eq!(c.volte.veth_carrier_addr, "10.98.0.1");
        assert_eq!(c.volte.veth_telephony_addr, "10.98.0.2");
    }

    /// specs/020-volte-line-netns FR-004a: VoLTE's and VoWiFi's per-line
    /// namespace/veth identifiers must never collide by default — both
    /// subsystems can run in the same container (docker-compose.yml).
    #[test]
    fn volte_and_vowifi_default_netns_and_veth_identifiers_never_collide() {
        let c = parse(MINIMAL_TOML);

        assert_ne!(c.volte.netns, c.vowifi.netns);
        assert_ne!(c.volte.veth_carrier_iface, c.vowifi.veth_ims_iface);
        assert_ne!(c.volte.veth_telephony_iface, c.vowifi.veth_sip_iface);
        assert_ne!(c.volte.veth_carrier_addr, c.vowifi.veth_local_addr);
        assert_ne!(c.volte.veth_telephony_addr, c.vowifi.veth_peer_addr);
    }

    #[test]
    fn volte_netns_and_veth_fields_are_not_settable_via_toml() {
        // Pure per-line infrastructure — always internally derived from a
        // line's index, no config knob at either the top level or per-line,
        // same as [vowifi]'s equivalent fields (specs config reorg).
        let src = format!(
            "{MINIMAL_TOML}\n[volte]\nnetns = \"volte-custom\"\n\
             veth_carrier_iface = \"veth-c\"\nveth_telephony_iface = \"veth-t\"\n\
             veth_carrier_addr = \"10.5.0.1\"\nveth_telephony_addr = \"10.5.0.2\"\n"
        );
        // Setting one is now refused outright rather than ignored. That
        // matters more here than elsewhere: a hand-set netns or veth address
        // would be silently overwritten per line, or would collide with
        // another line's, and the operator would have no way to tell.
        let err = try_parse(&src).unwrap_err().to_string();
        for key in [
            "volte.netns",
            "volte.veth_carrier_iface",
            "volte.veth_telephony_iface",
            "volte.veth_carrier_addr",
            "volte.veth_telephony_addr",
        ] {
            assert!(err.contains(key), "{key} missing from: {err}");
        }
    }

    #[test]
    fn volte_top_level_line_fields_are_no_longer_settable() {
        // modem_port/iface/cid/apn/pcscf/pcscf_port moved to [[volte.line]]
        // only (specs config reorg) — a stray top-level key is an
        // unknown-key warning, not a config value.
        let src = format!(
            "{MINIMAL_TOML}\n[volte]\nenabled = true\niface = \"wwan0\"\ncid = 4\n\
             pcscf = \"2400:5200:a100:819::6\"\n"
        );

        let err = try_parse(&src).unwrap_err().to_string();
        for key in ["volte.iface", "volte.cid", "volte.pcscf"] {
            assert!(err.contains(key), "{key} missing from: {err}");
        }
    }

    #[test]
    fn volte_max_lines_defaults_to_eight() {
        let c = parse(MINIMAL_TOML);
        assert_eq!(c.volte.max_lines, 8);
        assert!(c.volte.line_overrides.is_empty());
    }

    #[test]
    fn volte_max_lines_custom_value_parses() {
        let src = format!("{MINIMAL_TOML}\n[volte]\nmax_lines = 4\n");
        let c = parse(&src);
        assert_eq!(c.volte.max_lines, 4);
    }

    #[test]
    fn volte_max_lines_rejects_zero() {
        let src = format!("{MINIMAL_TOML}\n[volte]\nmax_lines = 0\n");
        assert!(try_parse(&src).map(|c| c.volte).is_err());
    }

    #[test]
    fn volte_line_overrides_absent_is_empty() {
        let c = parse(MINIMAL_TOML);
        assert!(c.volte.line_overrides.is_empty());
    }

    #[test]
    fn volte_line_overrides_multiple_entries_parse_in_order() {
        let src = format!(
            "{MINIMAL_TOML}\n[volte]\nenabled = true\n\
             [[volte.line]]\nmodem_port = \"/dev/ttyUSB2\"\ncid = 4\n\
             [[volte.line]]\nmodem_serial = \"abc123\"\niface = \"wwan1\"\n\
             pcscf = \"2400:5200:a100:819::6\"\nmsisdn = \"919000000001\"\n"
        );
        let c = parse(&src);
        assert_eq!(c.volte.line_overrides.len(), 2);
        let l0 = &c.volte.line_overrides[0];
        assert_eq!(l0.modem_port.as_deref(), Some("/dev/ttyUSB2"));
        assert_eq!(l0.cid, Some(4));
        let l1 = &c.volte.line_overrides[1];
        assert_eq!(l1.modem_serial.as_deref(), Some("abc123"));
        assert_eq!(l1.iface.as_deref(), Some("wwan1"));
        assert_eq!(l1.pcscf.as_deref(), Some("2400:5200:a100:819::6"));
        assert_eq!(l1.msisdn.as_deref(), Some("919000000001"));
    }

    #[test]
    fn volte_line_rejects_a_malformed_pcscf() {
        let src = format!("{MINIMAL_TOML}\n[volte]\n[[volte.line]]\npcscf = \"not-an-address\"\n");
        let err = try_parse(&src).map(|c| c.volte).unwrap_err();
        assert!(err.to_string().contains("not a valid IP address"));
    }

    #[test]
    fn volte_line_rejects_a_zero_context_id() {
        let src = format!("{MINIMAL_TOML}\n[volte]\n[[volte.line]]\ncid = 0\n");
        assert!(try_parse(&src).map(|c| c.volte).is_err());
    }

    const MINIMAL_TOML: &str = r#"
[sip]
server = "sip.example.com"
username = "user"
password = "pass"
"#;

    #[test]
    fn resilience_defaults_when_section_absent() {
        let cfg = parse(MINIMAL_TOML);
        assert_eq!(cfg.resilience.initial_backoff_sec, 5);
        assert_eq!(cfg.resilience.max_backoff_sec, 120);
        assert_eq!(cfg.resilience.max_retries, 10);
        assert_eq!(cfg.resilience.network_loss_timeout_sec, 60);
        assert_eq!(cfg.resilience.network_poll_interval_sec, 30);
    }

    #[test]
    fn resilience_overrides_applied() {
        let src = format!(
            "{}\n[resilience]\ninitial_backoff_sec = 10\nmax_retries = 3\n",
            MINIMAL_TOML
        );
        let cfg = parse(&src);
        assert_eq!(cfg.resilience.initial_backoff_sec, 10);
        assert_eq!(cfg.resilience.max_retries, 3);
        assert_eq!(cfg.resilience.max_backoff_sec, 120); // default preserved
    }

    #[test]
    fn control_default_socket_path() {
        let cfg = parse(MINIMAL_TOML);
        assert_eq!(cfg.control.socket_path, "/tmp/gsm-sip-bridge.sock");
    }

    #[test]
    fn control_custom_socket_path() {
        let src = format!(
            "{}\n[control]\nsocket_path = \"/run/gsm/ctrl.sock\"\n",
            MINIMAL_TOML
        );
        let cfg = parse(&src);
        assert_eq!(cfg.control.socket_path, "/run/gsm/ctrl.sock");
    }

    #[test]
    fn audio_defaults_to_lan_when_section_absent() {
        let cfg = parse(MINIMAL_TOML);
        assert_eq!(cfg.audio.profile, AudioProfile::Lan);
        assert_eq!(cfg.audio.settings.ring_capacity, 4);
        assert_eq!(cfg.audio.settings.jb_init_ms, 20);
        assert_eq!(cfg.audio.settings.jb_min_pre, 1);
        assert_eq!(cfg.audio.settings.jb_max_ms, 40);
        assert!(cfg.audio.vad, "VAD must default to enabled");
    }

    #[test]
    fn audio_vad_can_be_disabled() {
        let src = format!("{}\n[audio]\nvad = false\n", MINIMAL_TOML);
        let cfg = parse(&src);
        assert!(!cfg.audio.vad);
    }

    #[test]
    fn audio_vad_defaults_true_when_key_absent() {
        let src = format!("{}\n[audio]\nprofile = \"lan\"\n", MINIMAL_TOML);
        let cfg = parse(&src);
        assert!(cfg.audio.vad);
    }

    #[test]
    fn audio_lan_profile_explicit() {
        let src = format!("{}\n[audio]\nprofile = \"lan\"\n", MINIMAL_TOML);
        let cfg = parse(&src);
        assert_eq!(cfg.audio.profile, AudioProfile::Lan);
        assert_eq!(cfg.audio.settings.ring_capacity, 4);
    }

    #[test]
    fn audio_wan_profile() {
        let src = format!("{}\n[audio]\nprofile = \"wan\"\n", MINIMAL_TOML);
        let cfg = parse(&src);
        assert_eq!(cfg.audio.profile, AudioProfile::Wan);
        assert_eq!(cfg.audio.settings.ring_capacity, 16);
        assert_eq!(cfg.audio.settings.jb_init_ms, 60);
        assert_eq!(cfg.audio.settings.jb_min_pre, 2);
        assert_eq!(cfg.audio.settings.jb_max_ms, 200);
    }

    #[test]
    fn scheduled_restart_defaults_when_section_absent() {
        let cfg = parse(MINIMAL_TOML);
        assert!(cfg.scheduled_restart.enabled);
        assert_eq!(cfg.scheduled_restart.cron, "0 1 * * *");
        assert_eq!(cfg.scheduled_restart.start_jitter_seconds, 600);
        assert_eq!(cfg.scheduled_restart.inter_card_gap_seconds, 30);
        assert_eq!(cfg.scheduled_restart.inter_card_gap_jitter_seconds, 15);
        assert_eq!(cfg.scheduled_restart.restart_mode, "full");
    }

    #[test]
    fn scheduled_restart_radio_mode_applied() {
        let src = format!(
            "{}\n[scheduled_restart]\nrestart_mode = \"radio\"\n",
            MINIMAL_TOML
        );
        let cfg = parse(&src);
        assert_eq!(cfg.scheduled_restart.restart_mode, "radio");
        assert!(cfg.scheduled_restart.enabled);
    }

    #[test]
    fn scheduled_restart_invalid_restart_mode_disables_feature() {
        let src = format!(
            "{}\n[scheduled_restart]\nrestart_mode = \"nuke-from-orbit\"\n",
            MINIMAL_TOML
        );
        let cfg = parse(&src);
        assert!(
            !cfg.scheduled_restart.enabled,
            "invalid restart_mode must disable the feature, same as an invalid cron"
        );
    }

    #[test]
    fn scheduled_restart_disabled_via_flag() {
        let src = format!("{}\n[scheduled_restart]\nenabled = false\n", MINIMAL_TOML);
        let cfg = parse(&src);
        assert!(!cfg.scheduled_restart.enabled);
    }

    #[test]
    fn scheduled_restart_custom_cron_applied() {
        let src = format!(
            "{}\n[scheduled_restart]\ncron = \"30 2 * * 1-5\"\nstart_jitter_seconds = 0\n",
            MINIMAL_TOML
        );
        let cfg = parse(&src);
        assert_eq!(cfg.scheduled_restart.cron, "30 2 * * 1-5");
        assert_eq!(cfg.scheduled_restart.start_jitter_seconds, 0);
        assert!(cfg.scheduled_restart.enabled);
    }

    #[test]
    fn scheduled_restart_invalid_cron_disables_feature() {
        let src = format!(
            "{}\n[scheduled_restart]\ncron = \"0 25 * * *\"\n",
            MINIMAL_TOML
        );
        let cfg = parse(&src);
        assert!(
            !cfg.scheduled_restart.enabled,
            "invalid cron must disable the feature"
        );
    }

    #[test]
    fn scheduled_restart_jitter_greater_than_gap_disables() {
        let src = format!(
            "{}\n[scheduled_restart]\ninter_card_gap_seconds = 10\ninter_card_gap_jitter_seconds = 20\n",
            MINIMAL_TOML
        );
        let cfg = parse(&src);
        assert!(!cfg.scheduled_restart.enabled);
    }

    #[test]
    fn scheduled_restart_jitter_out_of_range_disables() {
        let src = format!(
            "{}\n[scheduled_restart]\nstart_jitter_seconds = 999999\n",
            MINIMAL_TOML
        );
        let cfg = parse(&src);
        assert!(!cfg.scheduled_restart.enabled);
    }

    #[test]
    fn scheduled_restart_empty_cron_disables() {
        let src = format!("{}\n[scheduled_restart]\ncron = \"\"\n", MINIMAL_TOML);
        let cfg = parse(&src);
        assert!(!cfg.scheduled_restart.enabled);
    }

    #[test]
    fn audio_unknown_profile_returns_error() {
        let src = format!("{}\n[audio]\nprofile = \"fiber\"\n", MINIMAL_TOML);
        let result = try_parse(&src).map(|c| c.audio);
        assert!(result.is_err());
        assert!(result
            .unwrap_err()
            .to_string()
            .contains("audio.profile must be"));
    }

    #[test]
    fn audio_snd_latency_defaults_when_omitted() {
        let src = format!("{}\n[audio]\nprofile = \"lan\"\n", MINIMAL_TOML);
        let audio = try_parse(&src).map(|c| c.audio).unwrap();
        assert_eq!(audio.snd_rec_latency_ms, DEFAULT_SND_REC_LATENCY_MS);
        assert_eq!(audio.snd_play_latency_ms, DEFAULT_SND_PLAY_LATENCY_MS);
    }

    #[test]
    fn audio_snd_latency_custom_values_parsed() {
        let src = format!(
            "{}\n[audio]\nsnd_rec_latency_ms = 300\nsnd_play_latency_ms = 250\n",
            MINIMAL_TOML
        );
        let audio = try_parse(&src).map(|c| c.audio).unwrap();
        assert_eq!(audio.snd_rec_latency_ms, 300);
        assert_eq!(audio.snd_play_latency_ms, 250);
    }

    #[test]
    fn audio_snd_latency_out_of_range_returns_error() {
        let src = format!("{}\n[audio]\nsnd_rec_latency_ms = 5\n", MINIMAL_TOML);
        let result = try_parse(&src).map(|c| c.audio);
        assert!(result.is_err());
        assert!(result
            .unwrap_err()
            .to_string()
            .contains("audio.snd_rec_latency_ms must be 20–2000"));
    }

    #[test]
    fn modem_audio_rt_audio_prio_defaults_off() {
        let cfg = parse(MINIMAL_TOML);
        assert_eq!(cfg.modem_audio.rt_audio_prio, 0);
    }

    #[test]
    fn modem_audio_rt_audio_prio_valid_value_parsed() {
        let src = format!("{}\n[modem_audio]\nrt_audio_prio = 20\n", MINIMAL_TOML);
        let cfg = parse(&src);
        assert_eq!(cfg.modem_audio.rt_audio_prio, 20);
    }

    #[test]
    fn modem_audio_rt_audio_prio_out_of_range_returns_error() {
        let src = format!("{}\n[modem_audio]\nrt_audio_prio = 150\n", MINIMAL_TOML);
        let result = try_parse(&src).map(|c| c.modem_audio);
        assert!(result.is_err());
        assert!(result
            .unwrap_err()
            .to_string()
            .contains("modem_audio.rt_audio_prio must be 0 (off) or 1–99"));
    }

    #[test]
    fn logging_defaults_to_info_when_section_absent() {
        let cfg = parse(MINIMAL_TOML);
        assert_eq!(cfg.logging.level, "info");
    }

    #[test]
    fn logging_level_override_applied() {
        let src = format!("{}\n[logging]\nlevel = \"debug\"\n", MINIMAL_TOML);
        let cfg = parse(&src);
        assert_eq!(cfg.logging.level, "debug");
    }

    #[test]
    fn logging_level_is_case_insensitive() {
        let src = format!("{}\n[logging]\nlevel = \"WARN\"\n", MINIMAL_TOML);
        let cfg = parse(&src);
        assert_eq!(cfg.logging.level, "warn");
    }

    #[test]
    fn logging_level_rejects_unknown_value() {
        let src = format!("{}\n[logging]\nlevel = \"verbose\"\n", MINIMAL_TOML);
        let result = try_parse(&src).map(|c| c.logging);
        assert!(result.is_err());
        assert!(result
            .unwrap_err()
            .to_string()
            .contains("logging.level must be one of"));
    }

    #[test]
    fn read_log_level_defaults_to_info_for_missing_file() {
        let level = read_log_level(Path::new("/nonexistent/config.toml"));
        assert_eq!(level, "info");
    }

    #[test]
    fn read_log_level_reads_configured_value() {
        let dir = std::env::temp_dir().join(format!("gsm-sip-bridge-test-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("config.toml");
        std::fs::write(
            &path,
            format!("{}\n[logging]\nlevel = \"trace\"\n", MINIMAL_TOML),
        )
        .unwrap();
        assert_eq!(read_log_level(&path), "trace");
        std::fs::remove_dir_all(&dir).unwrap();
    }

    #[test]
    fn vowifi_disabled_by_default_when_section_absent() {
        let cfg = parse(MINIMAL_TOML);
        assert!(!cfg.vowifi.enabled);
        assert_eq!(cfg.vowifi.modem_port, "");
        assert!(cfg.vowifi.use_tcp);
        assert!(cfg.vowifi.sec_agree);
        assert_eq!(cfg.vowifi.control_port, 7050);
    }

    #[test]
    fn vowifi_enabled_without_line_means_auto_derive() {
        let src = format!("{}\n[vowifi]\nenabled = true\n", MINIMAL_TOML);
        let cfg = parse(&src);
        assert!(cfg.vowifi.enabled);
        assert!(cfg.vowifi.mcc.is_empty());
        assert!(cfg.vowifi.mnc.is_empty());
        assert!(cfg.vowifi.epdg_fqdn.is_empty());
    }

    #[test]
    fn vowifi_top_level_mcc_mnc_are_no_longer_settable() {
        // mcc/mnc are per-line identity, settable only on a
        // `[[vowifi.line]]` entry. They used to be tolerated (and silently
        // ignored) at the top level; a key that looks like it works but does
        // nothing is worse than one that is refused, so this now fails.
        let src = format!(
            "{}\n[vowifi]\nenabled = true\nmcc = \"404\"\nmnc = \"094\"\n",
            MINIMAL_TOML
        );
        let err = try_parse(&src).unwrap_err().to_string();
        assert!(err.contains("vowifi.mcc"), "{err}");
        assert!(err.contains("vowifi.mnc"), "{err}");
    }

    #[test]
    fn vowifi_epdg_fqdn_override_respected() {
        let src = format!(
            "{}\n[vowifi]\nenabled = true\nepdg_fqdn = \"epdg.example.org\"\n",
            MINIMAL_TOML
        );
        let cfg = parse(&src);
        assert_eq!(cfg.vowifi.epdg_fqdn, "epdg.example.org");
    }

    #[test]
    fn vowifi_veth_and_infra_fields_are_not_settable_via_toml() {
        // Pure per-line infrastructure — always internally derived from a
        // line's index, no config knob at either the top level or per-line.
        let src = format!(
            "{}\n[vowifi]\nveth_local_addr = \"10.1.1.1\"\nveth_peer_addr = \"10.1.1.2\"\ncontrol_port = 9999\n",
            MINIMAL_TOML
        );
        // veth addresses are derived per line and are now refused outright
        // rather than silently ignored. `control_port` is the one genuinely
        // global field of the three, so it remains settable.
        let err = try_parse(&src).unwrap_err().to_string();
        assert!(err.contains("vowifi.veth_local_addr"), "{err}");
        assert!(err.contains("vowifi.veth_peer_addr"), "{err}");
        assert!(
            !err.contains("vowifi.control_port"),
            "control_port is a real key: {err}"
        );

        let cfg = parse(&format!("{MINIMAL_TOML}\n[vowifi]\ncontrol_port = 9999\n"));
        assert_eq!(cfg.vowifi.control_port, 9999);
        assert_eq!(cfg.vowifi.veth_local_addr, "10.99.0.1");
    }

    #[test]
    fn vowifi_vpcd_port_base_is_settable() {
        // Unlike the other per-line infra fields, vpcd_port is a genuine
        // global TOML key — the base of pcscd's shared reader range, which
        // operators sometimes must move to avoid an ephemeral-port collision.
        let src = format!("{}\n[vowifi]\nvpcd_port = 20000\n", MINIMAL_TOML);
        let cfg = parse(&src);
        assert_eq!(cfg.vowifi.vpcd_port, 20000);
    }

    #[test]
    fn vowifi_tunnel_engine_defaults_to_strongswan() {
        let cfg = parse(MINIMAL_TOML);
        assert_eq!(cfg.vowifi.tunnel_engine, "strongswan");
        assert_eq!(cfg.vowifi.strongswan_tun_iface, "tun23");
        assert_eq!(cfg.vowifi.strongswan_if_id, 23);
        assert_eq!(cfg.vowifi.netns, "ims");
        assert_eq!(cfg.vowifi.apn, "ims");
        assert_eq!(cfg.vowifi.keepalive_interval_sec, 20);
        assert_eq!(cfg.vowifi.vpcd_host, "127.0.0.1");
        assert_eq!(cfg.vowifi.vpcd_port, 15963);
        assert_eq!(cfg.vowifi.epdg_ip, None);
        assert_eq!(cfg.vowifi.src_addr, None);
        assert_eq!(cfg.vowifi.imsi_override, None);
        assert_eq!(cfg.vowifi.imei_override, None);
    }

    #[test]
    fn vowifi_tunnel_engine_rejects_unknown_value() {
        let src = format!("{}\n[vowifi]\ntunnel_engine = \"bogus\"\n", MINIMAL_TOML);
        let result = try_parse(&src).map(|c| c.vowifi);
        assert!(result.is_err());
        assert!(result
            .unwrap_err()
            .to_string()
            .contains("vowifi.tunnel_engine must be"));
    }

    #[test]
    fn vowifi_optional_overrides_parsed() {
        let src = format!(
            "{}\n[vowifi]\nepdg_ip = \"1.2.3.4\"\nsrc_addr = \"9.9.9.9\"\ntunnel_engine = \"swu\"\n",
            MINIMAL_TOML
        );
        let cfg = parse(&src);
        assert_eq!(cfg.vowifi.epdg_ip.as_deref(), Some("1.2.3.4"));
        assert_eq!(cfg.vowifi.src_addr.as_deref(), Some("9.9.9.9"));
        assert_eq!(cfg.vowifi.tunnel_engine, "swu");
    }

    #[test]
    fn vowifi_max_lines_defaults_to_eight() {
        let cfg = parse(MINIMAL_TOML);
        assert_eq!(cfg.vowifi.max_lines, 8);
        assert!(cfg.vowifi.line_overrides.is_empty());
    }

    #[test]
    fn vowifi_max_lines_custom_value_parses() {
        let src = format!("{}\n[vowifi]\nmax_lines = 4\n", MINIMAL_TOML);
        let cfg = parse(&src);
        assert_eq!(cfg.vowifi.max_lines, 4);
    }

    #[test]
    fn vowifi_max_lines_rejects_zero() {
        let src = format!("{}\n[vowifi]\nmax_lines = 0\n", MINIMAL_TOML);
        assert!(try_parse(&src).map(|c| c.vowifi).is_err());
    }

    #[test]
    fn vowifi_line_overrides_absent_is_empty() {
        let cfg = parse(MINIMAL_TOML);
        assert!(cfg.vowifi.line_overrides.is_empty());
    }

    #[test]
    fn vowifi_line_overrides_single_entry_parses() {
        let src = format!(
            "{}\n[vowifi]\n[[vowifi.line]]\nmodem_serial = \"ABC123\"\nmcc = \"404\"\nmnc = \"094\"\n",
            MINIMAL_TOML
        );
        let cfg = parse(&src);
        assert_eq!(cfg.vowifi.line_overrides.len(), 1);
        let line = &cfg.vowifi.line_overrides[0];
        assert_eq!(line.modem_serial.as_deref(), Some("ABC123"));
        assert_eq!(line.mcc.as_deref(), Some("404"));
        assert_eq!(line.mnc.as_deref(), Some("094"));
        assert_eq!(line.modem_port, None);
        assert_eq!(line.imsi_override, None);
        assert_eq!(line.imei_override, None);
    }

    #[test]
    fn vowifi_line_override_imei_override_parses_and_resolves() {
        let src = format!(
            "{}\n[vowifi]\n[[vowifi.line]]\nmodem_serial = \"ABC123\"\nimsi_override = \"404400975938075\"\nimei_override = \"864650053414154\"\n",
            MINIMAL_TOML
        );
        let cfg = parse(&src);
        let line = &cfg.vowifi.line_overrides[0];
        assert_eq!(line.imsi_override.as_deref(), Some("404400975938075"));
        assert_eq!(line.imei_override.as_deref(), Some("864650053414154"));
    }

    #[test]
    fn vowifi_line_override_rejects_non_digit_imsi_override() {
        let src = format!(
            "{}\n[vowifi]\n[[vowifi.line]]\nmodem_serial = \"ABC123\"\nimsi_override = \"40440097593807x\"\n",
            MINIMAL_TOML
        );
        let err = try_parse(&src).map(|c| c.vowifi).unwrap_err().to_string();
        assert!(err.contains("imsi_override"), "unexpected error: {err}");
    }

    #[test]
    fn vowifi_line_override_rejects_wrong_length_imei_override() {
        let src = format!(
            "{}\n[vowifi]\n[[vowifi.line]]\nmodem_serial = \"ABC123\"\nimei_override = \"12345\"\n",
            MINIMAL_TOML
        );
        let err = try_parse(&src).map(|c| c.vowifi).unwrap_err().to_string();
        assert!(err.contains("imei_override"), "unexpected error: {err}");
    }

    #[test]
    fn vowifi_line_overrides_multiple_entries_parse_in_order() {
        let src = format!(
            "{}\n[vowifi]\n[[vowifi.line]]\nmodem_port = \"/dev/ttyUSB6\"\n[[vowifi.line]]\nmodem_port = \"/dev/ttyUSB10\"\n",
            MINIMAL_TOML
        );
        let cfg = parse(&src);
        assert_eq!(cfg.vowifi.line_overrides.len(), 2);
        assert_eq!(
            cfg.vowifi.line_overrides[0].modem_port.as_deref(),
            Some("/dev/ttyUSB6")
        );
        assert_eq!(
            cfg.vowifi.line_overrides[1].modem_port.as_deref(),
            Some("/dev/ttyUSB10")
        );
    }

    #[test]
    fn vowifi_line_override_rejects_mcc_without_mnc() {
        let src = format!(
            "{}\n[vowifi]\n[[vowifi.line]]\nmodem_port = \"/dev/ttyUSB6\"\nmcc = \"404\"\n",
            MINIMAL_TOML
        );
        let result = try_parse(&src).map(|c| c.vowifi);
        assert!(result.is_err());
        assert!(result
            .unwrap_err()
            .to_string()
            .contains("mcc and mnc must be set together"));
    }

    #[test]
    fn vowifi_pcsc_reader_line_parses_with_an_explicit_plmn() {
        let src = format!(
            "{}\n[vowifi]\n[[vowifi.line]]\npcsc_reader = true\nimsi_override = \"404940123456789\"\nmcc = \"404\"\nmnc = \"043\"\n",
            MINIMAL_TOML
        );
        let cfg = parse(&src);
        assert_eq!(cfg.vowifi.line_overrides.len(), 1);
        let line = &cfg.vowifi.line_overrides[0];
        assert!(line.pcsc_reader);
        assert_eq!(line.imsi_override.as_deref(), Some("404940123456789"));
        assert_eq!(line.mcc.as_deref(), Some("404"));
        assert_eq!(line.mnc.as_deref(), Some("043"));
    }

    #[test]
    fn vowifi_pcsc_reader_defaults_to_false() {
        let src = format!(
            "{}\n[vowifi]\n[[vowifi.line]]\nmodem_serial = \"ABC123\"\n",
            MINIMAL_TOML
        );
        let cfg = parse(&src);
        assert!(!cfg.vowifi.line_overrides[0].pcsc_reader);
    }

    #[test]
    fn vowifi_pcsc_reader_rejects_missing_imsi_override() {
        // Mandatory not because the IMSI is unreadable (PcscTransport reads
        // EF_IMSI off every candidate reader) but because it is the key that
        // binds this line to one physical card, needed before any card
        // session exists.
        let src = format!(
            "{}\n[vowifi]\n[[vowifi.line]]\npcsc_reader = true\nmcc = \"404\"\nmnc = \"043\"\n",
            MINIMAL_TOML
        );
        let err = try_parse(&src).map(|c| c.vowifi).unwrap_err().to_string();
        assert!(err.contains("imsi_override"), "unexpected error: {err}");
    }

    #[test]
    fn vowifi_pcsc_reader_allows_omitted_mcc_mnc() {
        // Both derive from files on the card itself — EF_IMSI for the digits,
        // EF_AD for the MNC length — read over PC/SC with no modem involved
        // (`vowifi-plmn --pcsc-imsi`). Requiring them by hand was the older
        // behavior, and a mistyped 2-vs-3-digit MNC silently produced the
        // wrong ePDG FQDN and IMS realm, so leaving them out is now the
        // better default.
        let src = format!(
            "{}\n[vowifi]\n[[vowifi.line]]\npcsc_reader = true\nimsi_override = \"404940123456789\"\n",
            MINIMAL_TOML
        );
        let cfg = parse(&src);
        let line = &cfg.vowifi.line_overrides[0];
        assert!(line.pcsc_reader);
        assert_eq!(line.mcc, None);
        assert_eq!(line.mnc, None);
    }

    #[test]
    fn vowifi_pcsc_reader_rejects_modem_matcher_combination() {
        let src = format!(
            "{}\n[vowifi]\n[[vowifi.line]]\npcsc_reader = true\nmodem_serial = \"ABC123\"\nimsi_override = \"404940123456789\"\nmcc = \"404\"\nmnc = \"043\"\n",
            MINIMAL_TOML
        );
        let err = try_parse(&src).map(|c| c.vowifi).unwrap_err().to_string();
        assert!(err.contains("pcsc_reader"), "unexpected error: {err}");
    }

    #[test]
    fn vowifi_pcsc_reader_rejects_duplicate_imsi_across_lines() {
        // Caught in review: two pcsc_reader lines sharing an imsi_override
        // would both resolve to the same physical reader (connect()
        // disambiguates by IMSI), so one SIM would authenticate two
        // conflicting registrations while any other configured SIM sits
        // unused. Rejected at config time rather than left as a runtime
        // surprise.
        let src = format!(
            "{}\n[vowifi]\n\
             [[vowifi.line]]\npcsc_reader = true\nimsi_override = \"404940123456789\"\nmcc = \"404\"\nmnc = \"043\"\n\
             [[vowifi.line]]\npcsc_reader = true\nimsi_override = \"404940123456789\"\nmcc = \"404\"\nmnc = \"094\"\n",
            MINIMAL_TOML
        );
        let err = try_parse(&src).map(|c| c.vowifi).unwrap_err().to_string();
        assert!(err.contains("404940123456789"), "unexpected error: {err}");
        assert!(err.contains("pcsc_reader"), "unexpected error: {err}");
    }

    #[test]
    fn vowifi_pcsc_reader_allows_distinct_imsi_across_lines() {
        let src = format!(
            "{}\n[vowifi]\n\
             [[vowifi.line]]\npcsc_reader = true\nimsi_override = \"404940123456789\"\nmcc = \"404\"\nmnc = \"043\"\n\
             [[vowifi.line]]\npcsc_reader = true\nimsi_override = \"404011111111111\"\nmcc = \"404\"\nmnc = \"094\"\n",
            MINIMAL_TOML
        );
        let cfg = parse(&src);
        assert_eq!(cfg.vowifi.line_overrides.len(), 2);
    }

    #[test]
    fn an_absent_inbound_selection_leaves_todays_arrangement_untouched() {
        // specs/017 FR-021/FR-024: the feature is opt-in. A config that
        // predates it must keep behaving exactly as it did — that default is
        // what makes the feature safe to merge rather than a flag day.
        let cfg: VolteConfig = Default::default();
        assert!(!cfg.bridge_inbound);

        // NB: this fixture used to also set `modem_port` inside `[volte]`,
        // which has never been a `[volte]` key — the old parser warned and
        // carried on, so the test passed while silently exercising a config
        // no deployment could have meant. It is now a hard error, so the
        // stray key is gone.
        let parsed = try_parse(&format!("{MINIMAL_TOML}\n[volte]\nenabled = true\n"))
            .map(|c| c.volte)
            .expect("a config with no inbound selection must parse");
        assert!(
            !parsed.bridge_inbound,
            "an unset selection must not silently enable inbound bridging"
        );
    }

    #[test]
    fn the_inbound_selection_is_honoured_when_set() {
        let parsed = try_parse(&format!(
            "{MINIMAL_TOML}\n[volte]\nenabled = true\nbridge_inbound = true\n"
        ))
        .map(|c| c.volte)
        .expect("parses");
        assert!(parsed.bridge_inbound);
    }

    // ------------------------------------------------------- [sip_server] ---

    /// A complete, valid server-mode document. Note what is *absent*: no
    /// `[sip].server`/`username`/`password`, because in this mode they are
    /// errors rather than requirements.
    const SERVER_MODE_TOML: &str = r#"
[sip]
local_port = 5062

[sip_server]
enabled = true
ring_aor = "1001"

[[sip_server.account]]
username = "1001"
password = "s3cret"
"#;

    /// A valid server-mode document with `extra` spliced in *inside*
    /// `[sip_server]` — i.e. ahead of the account table, so appended keys
    /// belong to the section and not to the last `[[sip_server.account]]`.
    fn server_mode_with(extra: &str) -> BridgeResult<AppConfig> {
        try_parse(&format!(
            "[sip]\nlocal_port = 5062\n\n[sip_server]\nenabled = true\nring_aor = \"1001\"\n\
             {extra}\n[[sip_server.account]]\nusername = \"1001\"\npassword = \"s3cret\"\n"
        ))
    }

    #[test]
    fn sip_server_is_off_and_harmless_when_the_section_is_absent() {
        let c = parse(MINIMAL_TOML);
        assert!(!c.sip_server.enabled, "must be opt-in");
        assert_eq!(c.sip_server.listen_addr, "0.0.0.0");
        assert_eq!(c.sip_server.listen_port, 5060);
        assert_eq!(c.sip_server.realm, "gsm-sip-bridge");
        assert_eq!(c.sip_server.min_expires, 60);
        assert_eq!(c.sip_server.max_expires, 3600);
        assert_eq!(c.sip_server.nonce_lifetime_sec, 120);
        assert!(c.sip_server.accounts.is_empty());
    }

    /// A disabled section full of nonsense must not block startup — the same
    /// courtesy `[vowifi]` and `[volte]` extend to an operator who turned a
    /// mode off without cleaning up after it.
    #[test]
    fn a_disabled_sip_server_section_is_not_validated() {
        let c = try_parse(&format!(
            "{MINIMAL_TOML}\n[sip_server]\nenabled = false\nring_aor = \"nobody\"\n\
             listen_addr = \"not-an-ip\"\n"
        ))
        .expect("a disabled section must not be validated");
        assert!(!c.sip_server.enabled);
    }

    #[test]
    fn a_valid_server_mode_document_parses() {
        let c = try_parse(SERVER_MODE_TOML).expect("fixture must parse");
        assert!(c.sip_server.enabled);
        assert_eq!(c.sip_server.ring_aor, "1001");
        assert_eq!(c.sip_server.accounts.len(), 1);
        assert_eq!(c.sip_server.password_for("1001"), Some("s3cret"));
        assert_eq!(c.sip_server.password_for("nobody"), None);
        // The ringing account stands in for the absent `sip.username`, so the
        // phone sees calls arrive from an identity it recognises.
        assert_eq!(c.sip.display_name, "1001");
    }

    #[test]
    fn server_mode_requires_a_realm_and_a_ring_aor() {
        let err = try_parse(
            "[sip]\nlocal_port = 5062\n\n[sip_server]\nenabled = true\n\
             [[sip_server.account]]\nusername = \"1001\"\npassword = \"p\"\n",
        )
        .unwrap_err()
        .to_string();
        assert!(err.contains("sip_server.ring_aor"), "got: {err}");

        let err = server_mode_with("realm = \"\"\n").unwrap_err().to_string();
        assert!(err.contains("sip_server.realm"), "got: {err}");
    }

    #[test]
    fn server_mode_requires_at_least_one_account() {
        let err = try_parse(
            "[sip]\nlocal_port = 5062\n\n[sip_server]\nenabled = true\nring_aor = \"1001\"\n",
        )
        .unwrap_err()
        .to_string();
        assert!(
            err.contains("at least one [[sip_server.account]]"),
            "got: {err}"
        );
    }

    #[test]
    fn server_mode_rejects_an_empty_account_username_or_password() {
        let err = try_parse(
            "[sip]\nlocal_port = 5062\n\n[sip_server]\nenabled = true\nring_aor = \"1001\"\n\
             [[sip_server.account]]\nusername = \"\"\npassword = \"p\"\n",
        )
        .unwrap_err()
        .to_string();
        assert!(err.contains("account[0]: username"), "got: {err}");

        let err = try_parse(
            "[sip]\nlocal_port = 5062\n\n[sip_server]\nenabled = true\nring_aor = \"1001\"\n\
             [[sip_server.account]]\nusername = \"1001\"\npassword = \"\"\n",
        )
        .unwrap_err()
        .to_string();
        assert!(err.contains("account[0]: password"), "got: {err}");
    }

    /// Two accounts sharing a name leaves it undefined which one
    /// authenticates.
    #[test]
    fn server_mode_rejects_duplicate_account_usernames() {
        let err = server_mode_with(
            "\n[[sip_server.account]]\nusername = \"1001\"\npassword = \"other\"\n",
        )
        .unwrap_err()
        .to_string();
        assert!(err.contains("duplicate username"), "got: {err}");
        assert!(
            err.contains("account[0]"),
            "must name the earlier entry: {err}"
        );
    }

    /// Without this the mode starts cleanly and silently never rings — the
    /// hardest failure for an operator to diagnose.
    #[test]
    fn server_mode_rejects_a_ring_aor_matching_no_account() {
        let err = try_parse(
            "[sip]\nlocal_port = 5062\n\n[sip_server]\nenabled = true\nring_aor = \"9999\"\n\
             [[sip_server.account]]\nusername = \"1001\"\npassword = \"p\"\n",
        )
        .unwrap_err()
        .to_string();
        assert!(err.contains("matches no configured account"), "got: {err}");
        assert!(err.contains("1001"), "must list what is available: {err}");
    }

    #[test]
    fn server_mode_validates_expiry_bounds_and_nonce_lifetime() {
        for (extra, needle) in [
            ("min_expires = 10\n", "sip_server.min_expires"),
            ("max_expires = 999999\n", "sip_server.max_expires"),
            ("nonce_lifetime_sec = 5\n", "sip_server.nonce_lifetime_sec"),
        ] {
            let err = server_mode_with(extra).unwrap_err().to_string();
            assert!(err.contains(needle), "{extra} -> {err}");
        }

        let err = server_mode_with("min_expires = 3600\nmax_expires = 120\n")
            .unwrap_err()
            .to_string();
        assert!(err.contains("must not exceed"), "got: {err}");
    }

    /// Observed in the containerised run (spec 024 T071): the calling identity
    /// was logged as `sip:1001@0.0.0.0:5060`, which is what the handset would
    /// have seen in the `From` of every call.
    #[test]
    fn the_calling_identity_never_carries_a_wildcard_address() {
        let c = try_parse(SERVER_MODE_TOML).expect("parses");
        assert_eq!(
            c.sip_server.identity_uri(),
            "sip:1001@gsm-sip-bridge:5060",
            "a wildcard listen_addr must not reach the From header"
        );

        // `::` is the same situation in IPv6 clothing.
        let c = server_mode_with("listen_addr = \"::\"\n").expect("parses");
        assert!(
            !c.sip_server.identity_uri().contains("::"),
            "got: {}",
            c.sip_server.identity_uri()
        );
    }

    /// A concrete address *is* an identity, so it is used as given.
    #[test]
    fn a_concrete_listen_addr_is_used_as_the_calling_identity() {
        let c = server_mode_with("listen_addr = \"192.168.1.10\"\n").expect("parses");
        assert_eq!(c.sip_server.identity_uri(), "sip:1001@192.168.1.10:5060");
    }

    /// The whole point of `caller_identity_uri`: a phone in server mode has
    /// no PBX to translate `P-Asserted-Identity` into a displayed caller ID,
    /// so the mobile number must appear in what becomes the `From` itself.
    #[test]
    fn the_caller_identity_uri_carries_the_mobile_number_not_the_ring_aor() {
        let c = try_parse(SERVER_MODE_TOML).expect("parses");
        assert_eq!(
            c.sip_server.caller_identity_uri("+15551234567"),
            "sip:+15551234567@gsm-sip-bridge:5060"
        );
    }

    /// An unavailable CLIP (withheld or not provided by the network) falls
    /// back to the bridge's own identity, exactly as every call did before
    /// per-call identities existed — no call should end up with an empty
    /// user part in its `From`.
    #[test]
    fn an_empty_caller_id_falls_back_to_the_ring_aor_identity() {
        let c = try_parse(SERVER_MODE_TOML).expect("parses");
        assert_eq!(
            c.sip_server.caller_identity_uri(""),
            c.sip_server.identity_uri()
        );
    }

    /// The realm can become the `From` host and is quoted into the challenge,
    /// so characters that would malform either are refused as settings.
    #[test]
    fn a_realm_that_would_malform_a_header_is_refused() {
        for realm in ["my realm", "bad\"quote", "a<b", "x@y", "a,b", "a;b"] {
            let err = server_mode_with(&format!("realm = {realm:?}\n"))
                .unwrap_err()
                .to_string();
            assert!(err.contains("sip_server.realm"), "{realm:?} -> {err}");
        }
        // A plain hostname-shaped realm remains fine.
        assert!(server_mode_with("realm = \"pbx.example.com\"\n").is_ok());
    }

    #[test]
    fn server_mode_rejects_a_non_ip_listen_addr() {
        let err = server_mode_with("listen_addr = \"phones.local\"\n")
            .unwrap_err()
            .to_string();
        assert!(err.contains("sip_server.listen_addr"), "got: {err}");
    }

    /// A key that silently does nothing is the failure strict parsing exists
    /// to prevent — so a leftover PBX address is an error, not a warning.
    #[test]
    fn server_mode_forbids_settings_that_would_have_no_effect() {
        const TAIL: &str = "\n[sip_server]\nenabled = true\nring_aor = \"1001\"\n\n\
                            [[sip_server.account]]\nusername = \"1001\"\npassword = \"s3cret\"\n";
        for (key, doc) in [
            (
                "sip.server",
                format!("[sip]\nlocal_port = 5062\nserver = \"pbx.example.com\"\n{TAIL}"),
            ),
            (
                "sip.username",
                format!("[sip]\nlocal_port = 5062\nusername = \"trunk\"\n{TAIL}"),
            ),
            (
                "sip.password",
                format!("[sip]\nlocal_port = 5062\npassword = \"p\"\n{TAIL}"),
            ),
            (
                "bridge.sip_destination",
                format!("[sip]\nlocal_port = 5062\n\n[bridge]\nsip_destination = \"200\"\n{TAIL}"),
            ),
        ] {
            let err = try_parse(&doc).unwrap_err().to_string();
            assert!(err.contains(key), "{key} must be refused, got: {err}");
            assert!(err.contains("no effect"), "{key} -> {err}");
        }
    }

    /// Both keys default to 5060, so every operator enabling the mode meets
    /// this once. The message must therefore carry the remedy.
    #[test]
    fn server_mode_rejects_a_port_collision_and_names_the_remedy() {
        let err = try_parse(
            "[sip_server]\nenabled = true\nring_aor = \"1001\"\n\
             [[sip_server.account]]\nusername = \"1001\"\npassword = \"p\"\n",
        )
        .unwrap_err()
        .to_string();
        assert!(err.contains("cannot share one UDP port"), "got: {err}");
        assert!(
            err.contains("local_port = 5062"),
            "must name the fix: {err}"
        );
    }

    /// PR #21 review: 5072 and 5073 are the VoWiFi and VoLTE telephony sides'
    /// own SIP ports, and both live in the host namespace alongside the
    /// registrar. Validation compared `listen_port` only against
    /// `[sip].local_port`, so these passed and then failed at `bind` — inside a
    /// supervised child, which restarts while that path carries no calls.
    #[test]
    fn server_mode_rejects_a_listen_port_a_telephony_agent_already_claims() {
        for (port, inbound) in [
            (5072u16, "[vowifi]\nenabled = true\n"),
            (5073, "[volte]\nenabled = true\nbridge_inbound = true\n"),
        ] {
            let err = try_parse(&format!(
                "[sip]\nlocal_port = 5062\n\n[sip_server]\nenabled = true\n\
                 listen_port = {port}\nring_aor = \"1001\"\n\n[[sip_server.account]]\n\
                 username = \"1001\"\npassword = \"p\"\n\n{inbound}"
            ))
            .unwrap_err()
            .to_string();
            assert!(
                err.contains(&port.to_string()),
                "must name the port {port}: {err}"
            );
            assert!(
                err.contains("sip_server.listen_port"),
                "must name what to move: {err}"
            );
        }
    }

    /// The VoLTE loopback ports are strided per line, so the whole span up to
    /// `max_lines` is claimed — not just the first three.
    #[test]
    fn server_mode_rejects_a_listen_port_inside_the_volte_loopback_span() {
        // 5074 is line 0's; 5078 is line 1's, only reachable via the stride.
        for port in [5074u16, 5078] {
            let err = try_parse(&format!(
                "[sip]\nlocal_port = 5062\n\n[sip_server]\nenabled = true\n\
                 listen_port = {port}\nring_aor = \"1001\"\n\n[[sip_server.account]]\n\
                 username = \"1001\"\npassword = \"p\"\n\n\
                 [volte]\nenabled = true\nbridge_inbound = true\nmax_lines = 4\n"
            ))
            .unwrap_err()
            .to_string();
            assert!(err.contains("loopback"), "port {port} -> {err}");
        }
    }

    /// Those ports are only claimed when the subsystem that owns them is on,
    /// so a plain deployment must still be free to use them.
    #[test]
    fn a_reserved_port_is_free_when_its_subsystem_is_disabled() {
        for port in [5072u16, 5073, 5074] {
            let c = try_parse(&format!(
                "[sip]\nlocal_port = 5062\n\n[sip_server]\nenabled = true\n\
                 listen_port = {port}\nring_aor = \"1001\"\n\n[[sip_server.account]]\n\
                 username = \"1001\"\npassword = \"p\"\n"
            ))
            .unwrap_or_else(|e| panic!("port {port} must be free with no agents: {e}"));
            assert_eq!(c.sip_server.listen_port, port);
        }
    }

    /// PR #21 review: only one process may host the registrar, so a config in
    /// which both telephony sides would try to is refused rather than left to
    /// lose the race at `bind`.
    #[test]
    fn server_mode_refuses_two_telephony_sides_that_would_both_host_a_registrar() {
        let err = try_parse(
            "[sip]\nlocal_port = 5062\n\n[sip_server]\nenabled = true\nring_aor = \"1001\"\n\n\
             [[sip_server.account]]\nusername = \"1001\"\npassword = \"p\"\n\n\
             [vowifi]\nenabled = true\n\n[volte]\nenabled = true\nbridge_inbound = true\n",
        )
        .unwrap_err()
        .to_string();
        assert!(err.contains("only one can bind it"), "got: {err}");
        assert!(err.contains("exactly one"), "must say what to do: {err}");
    }

    /// The same pairing without server mode is the pre-existing arbitration's
    /// business, not this check's — it must stay parseable (FR-024).
    #[test]
    fn two_inbound_paths_without_server_mode_are_left_alone() {
        try_parse(&format!(
            "{MINIMAL_TOML}\n[vowifi]\nenabled = true\n\n\
             [volte]\nenabled = true\nbridge_inbound = true\n"
        ))
        .expect("unchanged from before this feature");
    }

    #[test]
    fn server_mode_requires_udp() {
        let err = try_parse(
            "[sip]\nlocal_port = 5062\ntransport = \"tcp\"\n\n[sip_server]\nenabled = true\n\
             ring_aor = \"1001\"\n\n[[sip_server.account]]\n\
             username = \"1001\"\npassword = \"p\"\n",
        )
        .unwrap_err()
        .to_string();
        assert!(err.contains("sip.transport"), "got: {err}");
        assert!(err.contains("UDP-only"), "got: {err}");
    }

    /// With the mode off, `[sip]` keeps its existing requirements untouched —
    /// the no-regression guarantee behind FR-024.
    #[test]
    fn disabling_server_mode_leaves_the_sip_section_requirements_intact() {
        let err = try_parse("[sip]\nlocal_port = 5062\n")
            .unwrap_err()
            .to_string();
        assert!(err.contains("sip.server"), "got: {err}");
    }

    #[test]
    fn an_account_password_resolves_from_the_environment() {
        std::env::set_var("TEST_PHONE_PW_024", "from-env");
        let c = try_parse(
            "[sip]\nlocal_port = 5062\n\n[sip_server]\nenabled = true\nring_aor = \"1001\"\n\
             [[sip_server.account]]\nusername = \"1001\"\npassword = \"env:TEST_PHONE_PW_024\"\n",
        )
        .expect("parses");
        assert_eq!(c.sip_server.password_for("1001"), Some("from-env"));
        std::env::remove_var("TEST_PHONE_PW_024");
    }

    #[test]
    fn an_unknown_sip_server_key_is_rejected() {
        let err = server_mode_with("ring_all = true\n")
            .unwrap_err()
            .to_string();
        assert!(err.contains("sip_server.ring_all"), "got: {err}");
    }

    #[test]
    fn an_unknown_sip_server_account_key_is_rejected() {
        let err = server_mode_with(
            "\n[[sip_server.account]]\nusername = \"1002\"\npassword = \"p\"\nrealm = \"x\"\n",
        )
        .unwrap_err()
        .to_string();
        assert!(err.contains("sip_server.account.realm"), "got: {err}");
    }
}
