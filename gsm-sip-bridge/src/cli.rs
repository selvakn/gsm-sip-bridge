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
    /// from the `[vowifi]` config section. Long-running — intended to run
    /// inside the ePDG tunnel's `ims` network namespace, supervised by
    /// `docker/entrypoint.sh`.
    VowifiImsAgent,
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
    /// `docker/entrypoint.sh` alongside charon, in the container's default
    /// network namespace (where the modem device and pcscd both live).
    VowifiUsimBridge(VowifiUsimBridgeArgs),
    /// Prints the SIM's IMSI (via `AT+CIMI`) and exits. Used by
    /// `docker/entrypoint.sh` to render the strongSwan swanctl connection's
    /// EAP identity without hand-parsing `AT+CIMI` in bash — the same
    /// "ask the binary" precedent as `config vowifi-enabled`.
    VowifiImsi(VowifiImsiArgs),
    /// Prints the home network's MCC and MNC (space-separated, MNC
    /// zero-padded to 3 digits) derived from the SIM, and exits: MCC is the
    /// IMSI's first 3 digits, the 2-vs-3-digit MNC ambiguity is resolved
    /// via the SIM's EF_AD file (`AT+CRSM`), falling back to the registered
    /// PLMN from numeric `AT+COPS`. Used by `docker/entrypoint.sh` when
    /// `vowifi.mcc`/`vowifi.mnc` are left unset in config.toml.
    VowifiPlmn(VowifiPlmnArgs),
    /// Read-only config introspection, for shell scripts (entrypoint.sh)
    /// that need a single answer without hand-rolling TOML parsing in bash.
    Config(ConfigArgs),
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
    /// `KEY=VALUE` shell-quoted lines, for `docker/entrypoint.sh` /
    /// `docker/healthcheck.sh` to `eval`/`source` instead of hand-parsing
    /// TOML or reading their own raw environment variables
    /// (specs/012-strongswan-epdg config consolidation: config.toml is the
    /// single source of truth for all non-secret configuration). Exits
    /// non-zero and prints nothing on config-load failure.
    VowifiShellEnv,
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
pub struct VowifiPlmnArgs {
    /// Modem AT port used for AT+CIMI / AT+CRSM / AT+COPS
    #[arg(long)]
    pub modem: PathBuf,
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
