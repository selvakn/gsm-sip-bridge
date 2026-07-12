use clap::{Parser, Subcommand};
use std::path::PathBuf;

const AFTER_LONG_HELP: &str = r#"ENVIRONMENT:
    METRICS_PORT             Override the metrics HTTP port (default: 9091)
    RUST_LOG                 Standard tracing-subscriber filter

For configuration reference, see docs/configuration.md.
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

    /// Where to write the recorded (received) audio — a 16-bit PCM mono
    /// WAV file at 8kHz (G.711's rate).
    #[arg(long)]
    pub record: PathBuf,

    /// How long to wait for the callee to answer before giving up.
    #[arg(long, default_value_t = 30)]
    pub ring_timeout_secs: u64,

    /// How long to hold the call open (exchanging audio) once answered.
    #[arg(long, default_value_t = 15)]
    pub call_duration_secs: u64,
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
