use clap::{Parser, Subcommand};
use std::path::PathBuf;

const AFTER_LONG_HELP: &str = r#"ENVIRONMENT:
    RUST_LOG                 Standard tracing-subscriber filter

All other configuration lives in config.toml, referenced via --config (see
docs/configuration.md); secrets may be pulled from process env vars using
the "env:VAR_NAME" syntax on any string field.
For the v4.1.x -> v5.0.0 migration, see docs/migrating-from-v4.1.x.md."#;

#[derive(Parser, Debug)]
#[command(
    name = "gsm-sip-bridge",
    version,
    about = "Bridges incoming GSM calls on Quectel EC20 modules to a SIP extension.",
    after_long_help = AFTER_LONG_HELP
)]
pub struct Cli {
    #[arg(short = 'c', long = "config")]
    pub config: Option<PathBuf>,

    #[arg(short = 'v', long = "verbose")]
    pub verbose: bool,

    #[arg(short = 's', long = "serial", requires = "audio")]
    pub serial: Option<PathBuf>,

    #[arg(short = 'a', long = "audio", requires = "serial")]
    pub audio: Option<String>,

    #[command(subcommand)]
    pub command: Option<Commands>,
}

#[derive(Subcommand, Debug)]
pub enum Commands {
    /// Manage GSM cards
    Card(CardArgs),
    /// Register to the operator's IMS core over a VoWiFi/ePDG tunnel using
    /// IMS-AKA (SIP digest authenticated via the SIM's AKA response). This
    /// is a standalone diagnostic mode — it does not start the GSM->SIP
    /// daemon or touch the CardPool.
    ImsRegister(ImsRegisterArgs),
    /// Register (as `ims-register`) and then place a real call over the
    /// live network, exchanging a test tone for outgoing audio and
    /// recording whatever comes back to a WAV file. Offers G.711 μ-law
    /// (PCMU) always, plus AMR-WB when this binary was built with the
    /// `amr-linked` feature (VoWiFi/VoLTE networks typically require
    /// AMR-WB and reject a PCMU-only offer with `488 Not Acceptable
    /// Here` — confirmed on a real Airtel test call). Standalone
    /// diagnostic mode, same as `ims-register`.
    ImsCall(ImsCallArgs),
    /// Agent A of the inbound VoWiFi-to-SIP bridge (specs/011-vowifi-sip-bridge):
    /// keeps a persistent IMS-AKA registration alive, answers inbound calls
    /// arriving over VoWiFi, and relays their audio to Agent B
    /// (`vowifi-sip-agent`) over a dedicated veth link. Reads its settings
    /// from the `[vowifi]` config section (or, with `--line`, one line's
    /// resolved slice of it — specs/013-multi-card-vowifi). Long-running —
    /// intended to run inside the ePDG tunnel's `ims` network namespace,
    /// supervised by `supervise::orchestrate`.
    VowifiImsAgent(VowifiImsAgentArgs),
    /// Agent B of the inbound VoWiFi-to-SIP bridge: registers to the
    /// SIP/PBX destination (`[sip]`/`[bridge]`) and, on each call signaled
    /// by Agent A, places a matching PBX-side call plus a veth-side call
    /// back to Agent A, then bridges them. Long-running — intended to run
    /// in the container's default network namespace.
    VowifiSipAgent,
    /// Query the running VoWiFi agents for current registration health and
    /// recent call outcomes.
    VowifiStatus,
    /// Bridges strongSwan's `eap-sim-pcsc` plugin (via pcscd's `vpcd`
    /// virtual reader) to the SIM inside the modem, forwarding APDUs over
    /// `AT+CSIM` (specs/012-strongswan-epdg). Long-running — supervised by
    /// `supervise::orchestrate` alongside charon, in the container's default
    /// network namespace (where the modem device and pcscd both live).
    VowifiUsimBridge(VowifiUsimBridgeArgs),
    /// Prints the SIM's IMSI (via `AT+CIMI`) and exits. Used by
    /// `supervise::render` to render the strongSwan swanctl connection's
    /// EAP identity without hand-parsing `AT+CIMI` in bash — the same
    /// "ask the binary" precedent as `config vowifi-enabled`.
    VowifiImsi(VowifiImsiArgs),
    /// Prints the home network's MCC and MNC (space-separated, MNC
    /// zero-padded to 3 digits) derived from the SIM, and exits: MCC is the
    /// IMSI's first 3 digits, and the 2-vs-3-digit MNC ambiguity is resolved
    /// via the SIM's own EF_AD file. Both inputs are on the card, so either
    /// transport works — `--modem <port>` (`AT+CIMI` + `AT+CRSM`, falling
    /// back to the registered PLMN from numeric `AT+COPS` if EF_AD is
    /// unreadable) or `--pcsc-imsi <IMSI>` for a `pcsc_reader` line, which
    /// reads EF_IMSI and EF_AD off that card directly and has no `AT+COPS`
    /// fallback (a reader has no serving network). Used by
    /// `supervise::orchestrate` when a line's `mcc`/`mnc` (from
    /// `[[vowifi.line]]`, or auto-discovery) are left unset.
    VowifiPlmn(VowifiPlmnArgs),
    /// Reconciles the modem's own IMS/VoLTE stack with whether *this host*
    /// is going to register this modem itself — `[vowifi].enabled` or
    /// `[volte].enabled` (specs/020-volte-line-netns; either alone requires
    /// the modem's IMS OFF, not just VoWiFi) — and exits. A modem left with
    /// its own IMS on registers the same IMPU with the same IMEI-derived
    /// `+sip.instance` as the bridge, so the network treats one as a
    /// re-registration of the other and tears our binding down (see
    /// `vowifi::ims_mode`). Rewrites `AT+QCFG="ims"` and reboots the module
    /// only when it is in the wrong mode, so it is a no-op on a healthy boot.
    /// Run by `supervise::orchestrate` before anything else opens the modem,
    /// for every line of either subsystem.
    ModemIms(ModemImsArgs),
    /// Manage the LTE IMS PDN attachment (specs/015-volte-host-ims, US1):
    /// activates a PDP context on the carrier's IMS APN and binds the modem's
    /// host-facing data path to it, so the bridge's own IMS stack can signal
    /// over LTE rather than delegating to the modem's internal IMS stack.
    ///
    /// Standalone diagnostic mode, like `ims-register` — it does not start
    /// the daemon or touch the CardPool. Requires CAP_NET_ADMIN: run it
    /// inside the privileged container.
    ///
    /// NOTE: the modem exposes a single host data path, so attaching the IMS
    /// PDN displaces general connectivity through the modem until `--action
    /// down`.
    VoltePdn(VoltePdnArgs),
    /// Reports LTE IMS PDN attachment state without changing it.
    VolteStatus(VolteStatusArgs),
    /// Probe for the carrier's P-CSCF address and report what each mechanism
    /// returned (specs/015-volte-host-ims, US2).
    ///
    /// These probes are DIAGNOSTICS. The Gate G1 investigation established
    /// that the tested carrier publishes no P-CSCF by any mechanism reachable
    /// from the host, so an empty result is the expected outcome there, not a
    /// fault — supply an address with `--pcscf` on `volte-register` instead.
    /// The value of running them is detecting a carrier, SIM, or firmware that
    /// behaves differently, without needing a code change.
    ///
    /// Exits 0 when an address was determined by any means, non-zero when none
    /// was; the per-method breakdown is printed either way.
    VolteDiscover(VolteDiscoverArgs),
    /// Register to the operator's IMS core over LTE using IMS-AKA
    /// (specs/015-volte-host-ims, US3): brings up the IMS PDN, then runs the
    /// same registration, IMS-AKA and Gm IPsec code the VoWiFi path uses.
    ///
    /// A P-CSCF address is required — this carrier publishes none by any
    /// mechanism the host can reach (see `volte-discover`). One can be
    /// captured from a VoWiFi/ePDG tunnel, which writes it to
    /// `[vowifi].pcscf_source_path`.
    ///
    /// WARNING: do not run this while the VoWiFi agent is registered. Both
    /// present the same IMPU with the same IMEI-derived `+sip.instance`, so
    /// the network treats one as a re-registration of the other and tears the
    /// first binding down.
    VolteRegister(VolteRegisterArgs),
    /// Place a diagnostic voice call over the host-side LTE IMS registration
    /// (specs/016-volte-calls) and report what happened to the audio.
    ///
    /// The far end hears their OWN VOICE returned over the full round trip —
    /// no audio files are involved. People notice distortion, delay and
    /// dropouts in their own voice far more readily than in a stranger's, and
    /// the echo carries the degradation of both directions at once.
    ///
    /// Have the answering party use a HANDSET: echoing into a speakerphone
    /// can feed back.
    ///
    /// Exits 0 only when the call was answered AND audio flowed both ways —
    /// an answered-but-silent call is a failure, not a success.
    VolteCall(VolteCallArgs),
    /// Register over LTE and listen for anything the network delivers, to
    /// establish whether the carrier routes incoming calls to us at all
    /// (specs/017-volte-inbound-bridge). Declines any call with a busy
    /// response rather than answering it.
    VolteListen(VolteListenArgs),
    /// Answer incoming cellular calls over the host-side LTE registration and
    /// bridge them to the operator's telephone system
    /// (specs/017-volte-inbound-bridge).
    ///
    /// A long-lived service, not a one-shot: it holds one registration open,
    /// renews it before expiry, and answers calls as they arrive. Renewal and
    /// re-attachment both yield to a call in progress.
    VolteBridge(VolteBridgeArgs),
    /// Resolves the auto-discovered host-side LTE line table (every SIM-ready
    /// modem, bounded by `[volte].max_lines`, shaped by `[[volte.line]]`) and
    /// writes it as the line manifest (specs/020-volte-line-netns) — the LTE
    /// counterpart to `discover`. Run once, up front, by
    /// `supervise::orchestrate_volte` before any per-line namespace or process
    /// exists: `volte-carrier-agent --line N` and `volte-bridge` (auto-
    /// discovered mode) both read the manifest this writes rather than
    /// re-scanning (research.md R7's "discover once" principle).
    VolteDiscoverLines(VolteDiscoverLinesArgs),
    /// The per-line carrier-facing half (specs/020-volte-line-netns) —
    /// attaches this line's IMS PDN, registers over it, and answers calls
    /// until the registration ends. The LTE counterpart to
    /// `vowifi-ims-agent --line N`: launched by `supervise::orchestrate_volte` inside
    /// this line's own network namespace (`ip netns exec`), one process per
    /// line, reading its settings from the manifest `volte-discover-lines`
    /// wrote. Long-running, but does not retry internally on failure —
    /// `supervise::orchestrate_volte` restarts it, mirroring how
    /// `vowifi-ims-agent` is supervised.
    VolteCarrierAgent(VolteCarrierAgentArgs),
    /// Tear down host-side LTE line(s) the running bridge recorded in its
    /// line manifest — releasing each modem's IMS PDN and restoring the data
    /// context it displaced (specs/018-volte-multi-modem). Run by
    /// `supervise::shutdown` after the multi-modem bridge exits; a
    /// no-op when no manifest exists.
    ///
    /// With `--line`, tears down only that one line — used by
    /// `supervise::shutdown`'s teardown plan (specs/020-volte-line-netns
    /// research.md R6) to run each line's teardown *inside* that line's own
    /// namespace (`ip netns exec`) before the namespace is deleted, since a
    /// namespace-scoped `ip`/sysctl teardown command run from the wrong
    /// namespace silently fails to find the interface. Omitted means every
    /// line, run from the default namespace — correct only when no line has
    /// a namespace of its own (defensive fallback, not the production path).
    VolteCleanup(VolteCleanupArgs),
    /// Read-only config introspection, for shell scripts (entrypoint.sh)
    /// that need a single answer without hand-rolling TOML parsing in bash.
    Config(ConfigArgs),
    /// Runs the shared USB modem scan once, assigns each recognized modem to
    /// the circuit-switched or VoWiFi subsystem, resolves the VoWiFi line
    /// table (specs/013-multi-card-vowifi), and writes the result so both
    /// the circuit-switched daemon and `supervise::orchestrate` can act on it
    /// without each re-scanning independently (which would otherwise race —
    /// see `specs/013-multi-card-vowifi/research.md` item 3).
    Discover(DiscoverArgs),
    /// Renders one config asset the strongSwan/vpcd path needs, byte-for-byte
    /// identical to what `docker/entrypoint.sh` used to render via bash
    /// heredoc+`sed` (specs/021-entrypoint-supervise-rust Phase 1), and prints
    /// it to stdout. A thin CLI wrapper over `supervise::render`'s pure
    /// functions — introduced so this phase ships independently before the
    /// `supervise` subcommand itself exists; folded into `supervise`
    /// in-process by Phase 4.
    Render(RenderArgs),
    /// Runs the full container orchestration — discover once up front, the
    /// circuit-switched daemon, every resolved VoWiFi/VoLTE line's
    /// supervision, and a clean shutdown on SIGINT/SIGTERM
    /// (specs/021-entrypoint-supervise-rust Phase 4). What
    /// `docker/entrypoint.sh` used to do itself in bash; the shim now just
    /// checks preconditions and execs this.
    Supervise,
    /// The container's `HEALTHCHECK` probe: exits 0 when the circuit-switched
    /// daemon's metrics endpoint answers and — only if `[vowifi].enabled` and
    /// at least one line resolved — every line's tunnel interface has an
    /// address and its P-CSCF is reachable over it (specs/013-multi-card-vowifi
    /// FR-019: every line, not just the first). Reads the line table
    /// `supervise` resolved at startup; never re-runs USB/AT discovery, which
    /// would race the agents already holding those serial ports open.
    ///
    /// Replaces `docker/healthcheck.sh` — the last orchestration left in bash
    /// after specs/021 moved the rest into `supervise`.
    Healthcheck,

    /// Lists every real (non-vpcd) PC/SC reader attached to the host, along
    /// with whatever card's IMSI and home MCC/MNC/carrier it holds
    /// (specs/023-omnikey-pcsc-vowifi) — read directly off the SIM, no
    /// modem involved. Meant to answer "which `imsi_override` goes on which
    /// `pcsc_reader` line" for a multi-reader deployment, without decoding
    /// an IMSI by hand off `pySim-read.py`/`opensc-tool` output. Carrier
    /// names come from a lookup against mcc-mnc-lookup.com; a lookup
    /// failure (offline, unreachable) leaves that column blank rather than
    /// failing the whole listing.
    PcscList,

    /// Exit 0 iff a TCP connection to `host:port` can be established.
    ///
    /// Internal plumbing, not meant to be run by hand: `healthcheck` probes
    /// each line's P-CSCF, which is reachable only inside that line's ePDG
    /// tunnel namespace, by re-executing this binary under `ip netns exec`.
    /// Entering a namespace in-process would need `setns` — the only `unsafe`
    /// in this binary — so it re-execs instead, the same way orchestration
    /// already places the IMS agents in their namespaces.
    #[command(hide = true)]
    TcpProbe { host: String, port: u16 },
}

#[derive(Parser, Debug)]
pub struct RenderArgs {
    #[command(subcommand)]
    pub asset: RenderAsset,
}

#[derive(Subcommand, Debug)]
pub enum RenderAsset {
    /// The shared strongswan.conf (one vici socket + filelog path for the one
    /// charon daemon serving every line).
    StrongswanConf {
        #[arg(long)]
        vici_socket: String,
        #[arg(long)]
        charon_log: String,
    },
    /// This line's top-level swanctl.conf (`include <conf_dir>/*.conf`).
    SwanctlTopConf {
        #[arg(long)]
        conf_dir: String,
    },
    /// This line's ePDG swanctl connection, substituted into the shared
    /// template at `--template-path` (`docker/strongswan/swanctl-epdg.conf.template`
    /// in the image).
    SwanctlEpdg {
        #[arg(long)]
        template_path: PathBuf,
        /// This line's connection/child name (`ims0`, `ims1`, ...) — unique
        /// per line, since every line loads into one shared charon.
        #[arg(long)]
        conn_name: String,
        #[arg(long)]
        imsi: String,
        #[arg(long)]
        mcc: String,
        #[arg(long)]
        mnc: String,
        #[arg(long)]
        epdg_ip: String,
        #[arg(long)]
        if_id: String,
        #[arg(long)]
        updown_script: String,
        /// Omit to drop the `local_addrs` line entirely (no source-address
        /// override configured).
        #[arg(long)]
        src_addr: Option<String>,
    },
    /// This line's strongSwan updown wrapper (sets NETNS/STRONGSWAN_TUN_IFACE,
    /// execs the shared ims.updown script).
    UpdownScript {
        #[arg(long)]
        netns: String,
        #[arg(long)]
        tun_iface: String,
    },
    /// The shared vpcd reader's `/etc/reader.conf.d/vpcd`.
    VpcdReaderConf {
        #[arg(long)]
        port: u16,
    },
}

#[derive(Parser, Debug)]
pub struct ModemImsArgs {
    /// Modem AT port used for `AT+QCFG="ims"` / `AT+CFUN=1,1`
    #[arg(long)]
    pub modem: PathBuf,
}

#[derive(clap::ValueEnum, Clone, Copy, Debug, PartialEq, Eq)]
pub enum VoltePdnAction {
    /// Attach the IMS PDN and bind the host data path to it.
    Up,
    /// Release the IMS PDN and restore the previous binding.
    Down,
    /// Report attachment state without changing anything.
    Status,
}

#[derive(Parser, Debug)]
pub struct VoltePdnArgs {
    #[arg(long, value_enum)]
    pub action: VoltePdnAction,
    /// Modem AT port. Omit to resolve it — along with `cid`/`apn` — from
    /// `--config`'s `[[volte.line]]` the same way `volte-register` does;
    /// falls back to `/dev/ttyUSB0`/the built-in cid/apn defaults when no
    /// config is given or no line is configured. Needed on `--action down`
    /// so a `volte-register --config ...` teardown targets the exact same
    /// line it registered, rather than guessing the CLI default.
    #[arg(long)]
    pub modem: Option<PathBuf>,
    /// Host network interface carrying the modem's data path. Leave unset to
    /// manage the PDN only and skip host interface configuration.
    #[arg(long)]
    pub iface: Option<String>,
    /// PDP context id for the IMS PDN. Must not collide with the contexts the
    /// modem uses for general internet access.
    #[arg(long, default_value_t = crate::volte::DEFAULT_IMS_CID)]
    pub cid: u8,
    /// APN to request. The network resolves this to its own fully-qualified
    /// name, which is what gets reported back.
    #[arg(long, default_value = crate::volte::DEFAULT_IMS_APN)]
    pub apn: String,
    /// On `--action down`, restore this context's binding instead of leaving
    /// the data path unbound.
    #[arg(long)]
    pub restore_cid: Option<u8>,
}

#[derive(clap::ValueEnum, Clone, Copy, Debug, PartialEq, Eq)]
pub enum VolteDiscoverMethod {
    /// Try every mechanism in order (the default).
    Auto,
    /// DHCPv6 Information-Request, RFC 3319 options 21/22.
    Dhcpv6,
    /// Protocol Configuration Options, read over AT.
    Pco,
    /// NAPTR on the home network realm.
    Dns,
}

#[derive(Parser, Debug)]
pub struct VolteDiscoverArgs {
    /// Modem AT port.
    #[arg(long, default_value = "/dev/ttyUSB0")]
    pub modem: PathBuf,
    /// Host network interface carrying the IMS PDN. Required for the DHCPv6
    /// probe.
    #[arg(long)]
    pub iface: Option<String>,
    #[arg(long, default_value_t = crate::volte::DEFAULT_IMS_CID)]
    pub cid: u8,
    /// Restrict the run to one mechanism, to evaluate it in isolation.
    #[arg(long, value_enum, default_value = "auto")]
    pub method: VolteDiscoverMethod,
    /// Home network MCC, for the DNS probe's realm. Derived from the SIM when
    /// unset.
    #[arg(long)]
    pub mcc: Option<String>,
    /// Home network MNC (zero-padded to 3 digits).
    #[arg(long)]
    pub mnc: Option<String>,
    /// Operator-supplied P-CSCF. Short-circuits discovery entirely (FR-010).
    #[arg(long)]
    pub pcscf: Option<std::net::IpAddr>,
    /// File the VoWiFi/ePDG path writes its captured P-CSCF to.
    #[arg(long, default_value = "/tmp/pcscf")]
    pub pcscf_source_path: String,
}

#[derive(Parser, Debug)]
pub struct VolteRegisterArgs {
    /// Modem AT port. Omit to resolve it — along with `cid`/`apn` — from
    /// `--config`'s `[[volte.line]]` (the first line a SIM-ready-modem scan
    /// resolves); falls back to `/dev/ttyUSB0`/the built-in cid/apn defaults
    /// when no config is given or no line is configured. Passing `--modem`
    /// explicitly opts out of config entirely — a pure CLI-args invocation,
    /// unchanged from before.
    #[arg(long)]
    pub modem: Option<PathBuf>,
    /// Host network interface carrying the IMS PDN.
    #[arg(long)]
    pub iface: Option<String>,
    #[arg(long, default_value_t = crate::volte::DEFAULT_IMS_CID)]
    pub cid: u8,
    #[arg(long, default_value = crate::volte::DEFAULT_IMS_APN)]
    pub apn: String,
    /// P-CSCF address. When omitted, the address captured by the VoWiFi/ePDG
    /// path (see --pcscf-source-path) is used — automatic discovery does not
    /// work on the tested carrier.
    #[arg(long)]
    pub pcscf: Option<std::net::IpAddr>,
    /// File the VoWiFi/ePDG path writes its captured P-CSCF to.
    #[arg(long, default_value = "/tmp/pcscf")]
    pub pcscf_source_path: String,
    #[arg(long, default_value_t = crate::volte::DEFAULT_PCSCF_PORT)]
    pub pcscf_port: u16,
    /// Use TCP rather than UDP for SIP.
    #[arg(long, default_value_t = true, action = clap::ArgAction::Set)]
    pub tcp: bool,
    /// Negotiate Gm IPsec (RFC 3329 sec-agree). Vodafone India rejects a
    /// plain digest REGISTER without it.
    #[arg(long, default_value_t = true, action = clap::ArgAction::Set)]
    pub sec_agree: bool,
    /// Use this MSISDN as the Public User Identity instead of the
    /// IMSI-derived temporary IMPU.
    #[arg(long)]
    pub msisdn: Option<String>,
    /// Leave the IMS PDN attached after the registration attempt, for
    /// inspection. By default it is released.
    #[arg(long)]
    pub keep_pdn: bool,
    /// Register once and exit instead of staying up and renewing before
    /// expiry.
    #[arg(long)]
    pub once: bool,
    /// Where to publish registration state for `volte-status`.
    #[arg(long, default_value = crate::volte::registration::DEFAULT_STATUS_PATH)]
    pub status_path: PathBuf,
    /// Register even when a VoWiFi agent is running. The two present the same
    /// IMPU and instance-id, so they will displace each other — only useful
    /// when deliberately testing that interference.
    #[arg(long)]
    pub force: bool,
    /// Lock file preventing two concurrent VoLTE registrations on one SIM.
    #[arg(long, default_value = crate::volte::guard::DEFAULT_LOCK_PATH)]
    pub lock_path: PathBuf,
    /// File to record the context id the IMS PDN displaced. Relevant with
    /// `--keep-pdn`, where this process leaves the PDN attached for an external
    /// teardown to release: that teardown reads this to `--restore-cid` the
    /// previous binding instead of leaving the modem data path unbound.
    #[arg(long)]
    pub restore_cid_path: Option<PathBuf>,
}

#[derive(Parser, Debug)]
pub struct VolteCallArgs {
    /// Destination in E.164, e.g. +919789063708.
    #[arg(long)]
    pub callee: String,
    #[arg(long, default_value = "/dev/ttyUSB0")]
    pub modem: PathBuf,
    /// Host network interface carrying the IMS PDN.
    #[arg(long)]
    pub iface: Option<String>,
    #[arg(long, default_value_t = crate::volte::DEFAULT_IMS_CID)]
    pub cid: u8,
    #[arg(long, default_value = crate::volte::DEFAULT_IMS_APN)]
    pub apn: String,
    /// P-CSCF address. When omitted, the address captured by the VoWiFi/ePDG
    /// path is used.
    #[arg(long)]
    pub pcscf: Option<std::net::IpAddr>,
    #[arg(long, default_value = "/tmp/pcscf")]
    pub pcscf_source_path: String,
    #[arg(long, default_value_t = crate::volte::DEFAULT_PCSCF_PORT)]
    pub pcscf_port: u16,
    /// How long to hold the call once answered. The default is long enough to
    /// judge audio quality.
    #[arg(long, default_value_t = 30)]
    pub duration_secs: u64,
    #[arg(long, default_value_t = 40)]
    pub ring_timeout_secs: u64,
    /// Gain applied to the returned audio. Clamped below unity so the
    /// feedback loop converges.
    #[arg(long, default_value_t = crate::ims::echo::DEFAULT_ATTENUATION)]
    pub echo_attenuation: f32,
    /// How often the independent generated marker is emitted, regardless of
    /// what has been received. Without it, echo would make the two directions
    /// dependent and destroy the direction attribution.
    #[arg(long, default_value_t = 5)]
    pub marker_interval_secs: u64,
    /// Where the far end's audio is written.
    #[arg(long, default_value = "/tmp/volte-call-received.wav")]
    pub record: PathBuf,
    /// Where our outgoing audio is written, separately — so a defect can be
    /// attributed to a direction.
    #[arg(long, default_value = "/tmp/volte-call-sent.wav")]
    pub record_sent: PathBuf,
    /// Proportion of the busier direction the quieter one must carry before it
    /// counts as working.
    #[arg(long, default_value_t = crate::ims::media_stats::DEFAULT_ONE_WAY_THRESHOLD_PERCENT)]
    pub one_way_threshold: u8,
    /// Use this MSISDN as the Public User Identity.
    #[arg(long)]
    pub msisdn: Option<String>,
    /// Place the call even when a VoWiFi agent is running.
    #[arg(long)]
    pub force: bool,
    #[arg(long, default_value = crate::volte::guard::DEFAULT_LOCK_PATH)]
    pub lock_path: PathBuf,
    /// Leave the IMS PDN attached afterwards, for inspection.
    #[arg(long)]
    pub keep_pdn: bool,
}

#[derive(Parser, Debug)]
pub struct VolteListenArgs {
    #[arg(long, default_value = "/dev/ttyUSB0")]
    pub modem: PathBuf,
    #[arg(long)]
    pub iface: Option<String>,
    #[arg(long, default_value_t = crate::volte::DEFAULT_IMS_CID)]
    pub cid: u8,
    #[arg(long, default_value = crate::volte::DEFAULT_IMS_APN)]
    pub apn: String,
    #[arg(long)]
    pub pcscf: Option<std::net::IpAddr>,
    #[arg(long, default_value = "/tmp/pcscf")]
    pub pcscf_source_path: String,
    #[arg(long, default_value_t = crate::volte::DEFAULT_PCSCF_PORT)]
    pub pcscf_port: u16,
    /// How long to stay registered and listening.
    #[arg(long, default_value_t = 180)]
    pub listen_secs: u64,
    #[arg(long)]
    pub msisdn: Option<String>,
    #[arg(long)]
    pub force: bool,
    #[arg(long, default_value = crate::volte::guard::DEFAULT_LOCK_PATH)]
    pub lock_path: PathBuf,
    #[arg(long)]
    pub keep_pdn: bool,
}

/// Options for the long-lived inbound bridging service.
#[derive(Parser, Debug)]
pub struct VolteBridgeArgs {
    /// A single modem's AT port to bridge. Omit to auto-discover every
    /// SIM-ready modem and bridge each as its own line (specs/018-volte-
    /// multi-modem), bounded by `[volte].max_lines` and shaped by any
    /// `[[volte.line]]` overrides.
    #[arg(long)]
    pub modem: Option<PathBuf>,
    #[arg(long)]
    pub iface: Option<String>,
    #[arg(long, default_value_t = crate::volte::DEFAULT_IMS_CID)]
    pub cid: u8,
    #[arg(long, default_value = crate::volte::DEFAULT_IMS_APN)]
    pub apn: String,
    #[arg(long)]
    pub pcscf: Option<std::net::IpAddr>,
    #[arg(long, default_value = "/tmp/pcscf")]
    pub pcscf_source_path: String,
    #[arg(long, default_value_t = crate::volte::DEFAULT_PCSCF_PORT)]
    pub pcscf_port: u16,
    #[arg(long)]
    pub msisdn: Option<String>,
    /// Labels this line's metrics and call history.
    #[arg(long)]
    pub card_id: Option<String>,
    /// Start even if the Wi-Fi path appears to hold the same subscriber's
    /// registration. An escape hatch for a stale detection, not a default:
    /// two live registrations for one subscriber means the network delivers
    /// calls to whichever bound last, silently.
    #[arg(long)]
    pub force: bool,
    /// File to record the context id the IMS PDN displaced, if any. The
    /// service runs its accept loop indefinitely and never reaches its own
    /// teardown, so the container's cleanup is what releases the PDN — and it
    /// reads this file to `--restore-cid` the previous binding, restoring
    /// general connectivity instead of leaving the modem data path unbound.
    #[arg(long)]
    pub restore_cid_path: Option<PathBuf>,
}

#[derive(Parser, Debug)]
pub struct VolteDiscoverLinesArgs {
    /// Also print shell-sourceable `KEY=value`/indexed-array output to
    /// stdout — mirrors `discover --shell-env`. A diagnostic aid now rather
    /// than an internal contract: the shell that used to `eval` this is gone,
    /// since orchestration and the healthcheck are both subcommands that read
    /// the resolved line table directly.
    #[arg(long)]
    pub shell_env: bool,
    /// Same meaning as `volte-bridge`'s own `--restore-cid-path`: the base
    /// path each line's displaced-context file is derived from
    /// (`<base>-<index>`). Recorded in the manifest so cleanup can find it.
    #[arg(long)]
    pub restore_cid_path: Option<PathBuf>,
}

#[derive(Parser, Debug)]
pub struct VolteCarrierAgentArgs {
    /// Which resolved VoLTE line (0-based index into the line manifest
    /// `volte-discover-lines` wrote) to run as.
    #[arg(long)]
    pub line: u32,
    /// Applied uniformly to every line, same as `volte-bridge --pcscf-port`
    /// (the manifest carries each line's P-CSCF address but not its port).
    #[arg(long, default_value_t = crate::volte::DEFAULT_PCSCF_PORT)]
    pub pcscf_port: u16,
}

#[derive(Parser, Debug)]
pub struct VolteCleanupArgs {
    /// Tear down only this one line (0-based index into the manifest).
    /// Omitted means every line.
    #[arg(long)]
    pub line: Option<u32>,
}

#[derive(Parser, Debug)]
pub struct VolteStatusArgs {
    /// Modem AT port.
    #[arg(long, default_value = "/dev/ttyUSB0")]
    pub modem: PathBuf,
    /// Host network interface carrying the modem's data path.
    #[arg(long)]
    pub iface: Option<String>,
    #[arg(long, default_value_t = crate::volte::DEFAULT_IMS_CID)]
    pub cid: u8,
    #[arg(long, default_value = crate::volte::DEFAULT_IMS_APN)]
    pub apn: String,
    /// Where `volte-register` publishes its registration state.
    #[arg(long, default_value = crate::volte::registration::DEFAULT_STATUS_PATH)]
    pub status_path: PathBuf,
}

#[derive(Parser, Debug)]
pub struct ConfigArgs {
    #[command(subcommand)]
    pub subcommand: ConfigSubcommand,
}

#[derive(Subcommand, Debug)]
pub enum ConfigSubcommand {
    /// Exit 0 if [vowifi].enabled = true in the given --config file, exit 1
    /// otherwise (including if the file can't be loaded at all). Prints
    /// nothing — only the exit code is meant to be used.
    VowifiEnabled,
    /// Prints the resolved `[vowifi]` section (plus `[metrics].port`) as
    /// `KEY=VALUE` shell-quoted lines, ready to `eval`/`source`.
    ///
    /// Now a diagnostic aid rather than an internal contract: both original
    /// consumers (`docker/entrypoint.sh`'s orchestration and
    /// `docker/healthcheck.sh`) are Rust subcommands and read the config
    /// directly. It stays because "ask the binary what it resolved" is the
    /// fastest way to answer *why is this line configured like that* on a
    /// running container — which is what it was really for
    /// (specs/012-strongswan-epdg: config.toml is the single source of
    /// truth for all non-secret configuration). Exits non-zero and prints
    /// nothing on config-load failure.
    VowifiShellEnv,
    /// Exit 0 if `[volte].enabled = true`, exit 1 otherwise (including when
    /// the file cannot be loaded). Prints nothing — only the exit code is
    /// meant to be used, matching `vowifi-enabled`.
    VolteEnabled,
    /// Prints the resolved `[volte]` section as shell-quoted `KEY=VALUE`
    /// lines for the shell to `eval`, so the supervisor never
    /// hand-parses TOML (specs/015-volte-host-ims).
    VolteShellEnv,
}

#[derive(Parser, Debug)]
pub struct ImsRegisterArgs {
    /// Modem AT port used for AT+CIMI / AT+CSIM (EAP/IMS-AKA against the SIM)
    #[arg(long)]
    pub modem: PathBuf,

    /// P-CSCF (IMS SIP registrar) IP address, as assigned by the ePDG tunnel
    #[arg(long)]
    pub pcscf: std::net::IpAddr,

    /// P-CSCF SIP port
    #[arg(long, default_value_t = 5060)]
    pub pcscf_port: u16,

    /// Mobile Country Code (3 digits)
    #[arg(long)]
    pub mcc: String,

    /// Mobile Network Code (3 digits, zero-padded)
    #[arg(long)]
    pub mnc: String,

    /// Override the IMSI instead of reading it from the SIM via AT+CIMI
    #[arg(long)]
    pub imsi: Option<String>,

    /// Override the IMEI instead of reading it from the modem via AT+CGSN.
    /// Sent as the Contact header's +sip.instance — real UEs always send
    /// their genuine IMEI here, not a placeholder, and a network's
    /// terminating-call routing may key off it even when REGISTER succeeds
    /// regardless.
    #[arg(long)]
    pub imei: Option<String>,

    /// Use TCP instead of UDP to reach the P-CSCF (some networks/paths only
    /// carry SIP signaling over TCP; ICMP being filtered is not a signal
    /// either way, but a UDP REGISTER timeout is worth retrying over TCP)
    #[arg(long)]
    pub tcp: bool,

    /// Advertise Supported: sec-agree + a Security-Client: ipsec-3gpp
    /// proposal on every REGISTER. Required by networks that reject a plain
    /// digest REGISTER with 421 Extension Required (confirmed: Vodafone
    /// India). Does not set up the actual Gm IPsec SA — only tests whether
    /// the network proceeds on the header proposal alone.
    #[arg(long)]
    pub sec_agree: bool,

    /// Use this MSISDN (E.164, e.g. +919876543210) as the Public User
    /// Identity in To/From/Contact instead of the IMSI-derived temporary
    /// IMPU (`sip:<IMSI>@<realm>`). The Authorization header's username
    /// (the actual authentication identity, IMPI) is unaffected — it's
    /// always IMSI-based per TS 33.203, regardless of this flag. Some
    /// networks' HSS may reject binding a Contact to the private identity
    /// directly; this tests that hypothesis without needing the SIM to have
    /// an MSISDN provisioned in EF_MSISDN (queryable via `AT+CNUM`, but not
    /// every SIM has it written).
    #[arg(long)]
    pub msisdn: Option<String>,
}

#[derive(Parser, Debug)]
pub struct ImsCallArgs {
    #[command(flatten)]
    pub register: ImsRegisterArgs,

    /// Callee, E.164 (e.g. +919789063708). This places a REAL call over
    /// the live network to this number — make sure whoever's on the other
    /// end is expecting it.
    #[arg(long)]
    pub to: String,

    /// Where to write the recorded (received, far-end) audio — a 16-bit
    /// PCM mono WAV file at 8kHz (PCMU) or 16kHz (AMR-WB), matching
    /// whichever codec the network selects.
    #[arg(long)]
    pub record: PathBuf,

    /// Where to write the sent (outgoing, our own test pattern) audio, for
    /// a side-by-side comparison of both directions. Optional.
    #[arg(long)]
    pub record_sent: Option<PathBuf>,

    /// How long to wait for the callee to answer before giving up.
    #[arg(long, default_value_t = 30)]
    pub ring_timeout_secs: u64,

    /// How long to hold the call open (exchanging audio) once answered.
    #[arg(long, default_value_t = 15)]
    pub call_duration_secs: u64,
}

#[derive(Parser, Debug)]
pub struct VowifiUsimBridgeArgs {
    /// Modem AT port used for AT+CSIM (EAP-AKA APDUs forwarded from the
    /// virtual PC/SC reader to the SIM inside the modem)
    #[arg(long)]
    pub modem: PathBuf,

    /// Host running the vpcd virtual smart-card reader (pcscd's vpcd driver)
    #[arg(long, default_value = "127.0.0.1")]
    pub vpcd_host: String,

    /// TCP port vpcd listens on. Must stay below the kernel's ephemeral
    /// range (`net.ipv4.ip_local_port_range`, 32768-60999 by default) —
    /// vsmartcard's upstream 35963 sits inside it, so under
    /// `network_mode: host` an unrelated outbound connection can squat the
    /// port before pcscd binds it, and the reader then fails to come up.
    #[arg(long, default_value_t = 15963)]
    pub vpcd_port: u16,
}

#[derive(Parser, Debug)]
pub struct VowifiImsiArgs {
    /// Modem AT port used for AT+CIMI
    #[arg(long)]
    pub modem: PathBuf,
}

#[derive(Parser, Debug)]
#[command(group(
    clap::ArgGroup::new("plmn_transport").required(true).args(["modem", "pcsc_imsi"])
))]
pub struct VowifiPlmnArgs {
    /// Modem AT port used for AT+CIMI / AT+CRSM / AT+COPS
    #[arg(long)]
    pub modem: Option<PathBuf>,
    /// Derive from the PC/SC reader holding this IMSI's card instead of a
    /// modem (a `pcsc_reader` line) — EF_IMSI and EF_AD are read straight
    /// off the card. Mutually exclusive with --modem.
    #[arg(long)]
    pub pcsc_imsi: Option<String>,
}

#[derive(Parser, Debug)]
pub struct VowifiImsAgentArgs {
    /// Which resolved VoWiFi line (0-based index into the `discover`
    /// subcommand's line-resolution file) to run as. Omitted means "the
    /// single line" (index 0) — the pre-multi-card behavior, still correct
    /// for a single-SIM deployment (specs/013-multi-card-vowifi FR-020).
    #[arg(long)]
    pub line: Option<u32>,
}

#[derive(Parser, Debug)]
pub struct DiscoverArgs {
    /// Where to write the line-resolution JSON. Defaults to
    /// `GSM_SIP_BRIDGE_LINES_FILE`, or
    /// `modules::discovery::DEFAULT_LINES_FILE` if that's unset too.
    #[arg(long)]
    pub out: Option<PathBuf>,

    /// Also print shell-sourceable `KEY=value`/indexed-array output to
    /// stdout. A diagnostic aid now rather than an internal contract — see
    /// `discover --shell-env`.
    #[arg(long)]
    pub shell_env: bool,

    /// Skip the USB/AT scan entirely and just re-print the existing
    /// line-resolution file's contents (as `--shell-env`, if requested).
    /// For read-only consumers that must NOT re-probe modems the running
    /// VoWiFi agents already hold open. The `healthcheck` subcommand reads
    /// the same file for the same reason: it runs every 30s and would
    /// otherwise both waste time re-scanning and risk colliding with a live
    /// `vowifi-usim-bridge` session on the same serial port.
    #[arg(long)]
    pub from_file: bool,
}

#[derive(Parser, Debug)]
pub struct CardArgs {
    #[command(subcommand)]
    pub subcommand: CardSubcommand,
}

#[derive(Subcommand, Debug)]
pub enum CardSubcommand {
    /// Restart a card slot (reset give-up state and re-initialize)
    Restart {
        #[arg(long, short)]
        slot: u32,
    },
    /// Set the network mode for a slot (2g, 3g, 4g, auto)
    SetMode {
        #[arg(long, short)]
        slot: u32,
        #[arg(long, short)]
        mode: String,
    },
    /// Get the stored network mode preference for a slot
    GetMode {
        #[arg(long, short)]
        slot: u32,
    },
    /// List all known card slots and their current state
    List,
}

impl Cli {
    pub fn parse_args() -> Self {
        Self::parse()
    }
}
