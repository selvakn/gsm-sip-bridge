//! Turning a deserialised [`raw::RawConfig`] into the runtime [`AppConfig`]:
//! range validation, enum parsing, derived-field defaults, and the two
//! sections whose bad values are deliberately *tolerated* rather than fatal.
//!
//! # Two validation policies, on purpose
//!
//! Most sections are **strict**: a value outside its documented range is a
//! startup error, because a bridge running with a nonsensical SIP port or
//! backoff is not usefully "up".
//!
//! `[scheduled_restart]` and `[alerts]` are **lenient**: a bad value disables
//! that feature and leaves the rest of the bridge running. A preventive
//! nightly modem restart and a Discord alert are both *auxiliary* — refusing
//! to answer calls because a cron expression was mistyped would be a strictly
//! worse outcome than not restarting on schedule. Both log what they rejected.
//!
//! This distinction predates the serde migration and is preserved exactly;
//! several tests pin it.

use super::raw::*;
use super::*;
use crate::error::{BridgeError, BridgeResult};

/// Rejects a value outside its documented range, naming the key and the range.
fn in_range<T>(value: T, key: &str, range: std::ops::RangeInclusive<T>) -> BridgeResult<T>
where
    T: PartialOrd + std::fmt::Display + Copy,
{
    if range.contains(&value) {
        Ok(value)
    } else {
        Err(BridgeError::Config(format!(
            "field {key} must be in {}..={}, got {value}",
            range.start(),
            range.end()
        )))
    }
}

/// Like [`in_range`] but for the lenient sections: out-of-range logs and
/// yields `None` so the caller can fall back rather than fail startup.
fn in_range_or_warn<T>(value: T, key: &str, range: std::ops::RangeInclusive<T>) -> Option<T>
where
    T: PartialOrd + std::fmt::Display + Copy,
{
    if range.contains(&value) {
        Some(value)
    } else {
        tracing::warn!(
            key,
            value = %value,
            "value out of range; falling back to the default"
        );
        None
    }
}

fn require_non_empty(value: &str, key: &str) -> BridgeResult<()> {
    if value.is_empty() {
        return Err(BridgeError::Config(format!(
            "required field {key} is missing"
        )));
    }
    Ok(())
}

// ------------------------------------------------------------------ sip ----

fn build_sip(raw: RawSip) -> BridgeResult<SipConfig> {
    require_non_empty(&raw.server, "sip.server")?;
    require_non_empty(&raw.username, "sip.username")?;
    require_non_empty(raw.password.expose_secret(), "sip.password")?;

    let transport = match raw.transport.to_ascii_lowercase().as_str() {
        "udp" => SipTransport::Udp,
        "tcp" => SipTransport::Tcp,
        "tls" => SipTransport::Tls,
        other => {
            return Err(BridgeError::Config(format!(
                "field sip.transport must be one of udp/tcp/tls, got {other:?}"
            )))
        }
    };

    let tls_verify = match raw.tls_verify.to_ascii_lowercase().as_str() {
        "strict" => TlsVerify::Strict,
        "skip" => TlsVerify::Skip,
        other => {
            return Err(BridgeError::Config(format!(
                "field sip.tls_verify must be one of strict/skip, got {other:?}"
            )))
        }
    };
    if tls_verify == TlsVerify::Skip {
        tracing::warn!(
            "sip.tls_verify = \"skip\" disables certificate validation — \
             diagnostics only, never production"
        );
    }

    Ok(SipConfig {
        server: raw.server,
        port: in_range(raw.port, "sip.port", 1..=65535)?,
        // Defaulting the display name to the username keeps caller ID
        // meaningful without making every deployment set two fields.
        display_name: raw.display_name.unwrap_or_else(|| raw.username.clone()),
        username: raw.username,
        password: raw.password,
        transport,
        local_port: in_range(raw.local_port, "sip.local_port", 1..=65535)?,
        tls_verify,
    })
}

// --------------------------------------------------------------- others ----

fn build_bridge(raw: RawBridge) -> BridgeResult<BridgeSection> {
    Ok(BridgeSection {
        sip_destination: raw.sip_destination,
        sip_dial_timeout_sec: in_range(
            raw.sip_dial_timeout_sec,
            "bridge.sip_dial_timeout_sec",
            5..=120,
        )?,
    })
}

fn build_sms(raw: RawSms) -> BridgeResult<SmsConfig> {
    Ok(SmsConfig {
        enabled: raw.enabled,
        discord_webhook_url: raw.discord_webhook_url,
        db_path: raw.db_path,
    })
}

fn build_metrics(raw: RawMetrics) -> BridgeResult<MetricsConfig> {
    Ok(MetricsConfig {
        port: in_range(raw.port, "metrics.port", 1..=65535)?,
        agent_report_interval_seconds: in_range(
            raw.agent_report_interval_seconds,
            "metrics.agent_report_interval_seconds",
            1..=3600,
        )?,
    })
}

fn build_modules(raw: RawModules) -> BridgeResult<ModulesConfig> {
    Ok(ModulesConfig {
        retry_interval_sec: in_range(
            raw.retry_interval_sec,
            "modules.retry_interval_sec",
            5..=600,
        )?,
        max_concurrent: in_range(raw.max_concurrent, "modules.max_concurrent", 1..=8)? as u32,
    })
}

fn build_resilience(raw: RawResilience) -> BridgeResult<ResilienceConfig> {
    Ok(ResilienceConfig {
        initial_backoff_sec: in_range(
            raw.initial_backoff_sec,
            "resilience.initial_backoff_sec",
            1..=600,
        )?,
        max_backoff_sec: in_range(raw.max_backoff_sec, "resilience.max_backoff_sec", 1..=3600)?,
        max_retries: in_range(raw.max_retries, "resilience.max_retries", 1..=1000)? as u32,
        network_loss_timeout_sec: in_range(
            raw.network_loss_timeout_sec,
            "resilience.network_loss_timeout_sec",
            10..=600,
        )?,
        network_poll_interval_sec: in_range(
            raw.network_poll_interval_sec,
            "resilience.network_poll_interval_sec",
            5..=300,
        )?,
    })
}

fn build_control(raw: RawControl) -> BridgeResult<ControlConfig> {
    Ok(ControlConfig {
        socket_path: raw.socket_path,
    })
}

fn build_logging(raw: RawLogging) -> BridgeResult<LoggingConfig> {
    let level = raw.level.to_ascii_lowercase();
    if !LOGGING_LEVELS.contains(&level.as_str()) {
        return Err(BridgeError::Config(format!(
            "field logging.level must be one of {}, got {:?}",
            LOGGING_LEVELS.join("/"),
            raw.level
        )));
    }
    Ok(LoggingConfig { level })
}

/// ALSA ring-buffer depth. Its own message rather than `in_range`'s generic
/// one because the unit matters to whoever has to fix it.
fn latency_ms(value: u32, key: &str) -> BridgeResult<u32> {
    if !(20..=2000).contains(&value) {
        return Err(BridgeError::Config(format!(
            "audio.{key} must be 20\u{2013}2000 (ms); got {value}"
        )));
    }
    Ok(value)
}

fn build_audio(raw: RawAudio) -> BridgeResult<AudioConfig> {
    let profile = match raw.profile.to_ascii_lowercase().as_str() {
        "lan" => AudioProfile::Lan,
        "wan" => AudioProfile::Wan,
        other => {
            return Err(BridgeError::Config(format!(
                "field audio.profile must be one of lan/wan, got {other:?}"
            )))
        }
    };
    Ok(AudioConfig {
        settings: AudioProfileSettings::for_profile(&profile),
        profile,
        vad: raw.vad,
        snd_rec_latency_ms: latency_ms(raw.snd_rec_latency_ms, "snd_rec_latency_ms")?,
        snd_play_latency_ms: latency_ms(raw.snd_play_latency_ms, "snd_play_latency_ms")?,
    })
}

fn build_modem_audio(raw: RawModemAudio) -> BridgeResult<ModemAudioConfig> {
    if let Some(g) = raw.rx_gain {
        in_range(g, "modem_audio.rx_gain", 0..=65535)?;
    }
    if let Some(e) = raw.eec_mode {
        in_range(e, "modem_audio.eec_mode", 0..=65535)?;
    }
    if !(0.0..=2.0).contains(&raw.tx_level) {
        return Err(BridgeError::Config(format!(
            "field modem_audio.tx_level must be in 0..=2, got {}",
            raw.tx_level
        )));
    }
    // 0 means "leave at SCHED_OTHER"; any other value must be a valid
    // SCHED_FIFO priority.
    if raw.rt_audio_prio != 0 && !(1..=99).contains(&raw.rt_audio_prio) {
        return Err(BridgeError::Config(format!(
            "modem_audio.rt_audio_prio must be 0 (off) or 1\u{2013}99 \
             (SCHED_FIFO priority); got {}",
            raw.rt_audio_prio
        )));
    }
    Ok(ModemAudioConfig {
        rx_gain: raw.rx_gain,
        eec_mode: raw.eec_mode,
        tx_level: raw.tx_level,
        rt_audio_prio: raw.rt_audio_prio,
    })
}

// --------------------------------------------- scheduled_restart (lenient) --

fn build_scheduled_restart(raw: RawScheduledRestart) -> ScheduledRestartConfig {
    let d = ScheduledRestartConfig::default();
    if !raw.enabled {
        return ScheduledRestartConfig {
            enabled: false,
            ..d
        };
    }

    if raw.cron.is_empty() {
        tracing::error!("scheduled_restart.cron is empty; scheduled restart disabled for this run");
        return ScheduledRestartConfig::disabled();
    }

    let start_jitter = in_range_or_warn(
        raw.start_jitter_seconds,
        "scheduled_restart.start_jitter_seconds",
        0..=86400,
    );
    let gap = in_range_or_warn(
        raw.inter_card_gap_seconds,
        "scheduled_restart.inter_card_gap_seconds",
        0..=3600,
    );
    let gap_jitter = in_range_or_warn(
        raw.inter_card_gap_jitter_seconds,
        "scheduled_restart.inter_card_gap_jitter_seconds",
        0..=3600,
    );

    let (Some(start_jitter), Some(gap), Some(gap_jitter)) = (start_jitter, gap, gap_jitter) else {
        return ScheduledRestartConfig::disabled();
    };

    // Jitter larger than the gap could order two cards' restarts backwards,
    // defeating the point of spacing them at all.
    if gap_jitter > gap {
        tracing::error!(
            jitter = gap_jitter,
            gap,
            "scheduled_restart.inter_card_gap_jitter_seconds must be <= \
             inter_card_gap_seconds; scheduled restart disabled for this run"
        );
        return ScheduledRestartConfig::disabled();
    }

    // The `cron` crate takes 7 fields; this project's config takes the usual
    // 5, so seconds are prepended and year appended before validating.
    let translated = format!("0 {} *", raw.cron);
    if let Err(e) = translated.parse::<cron::Schedule>() {
        tracing::error!(
            cron = %raw.cron,
            error = %e,
            "scheduled_restart.cron is not a valid 5-field cron expression; \
             scheduled restart disabled for this run"
        );
        return ScheduledRestartConfig::disabled();
    }

    ScheduledRestartConfig {
        enabled: true,
        cron: raw.cron,
        start_jitter_seconds: start_jitter,
        inter_card_gap_seconds: gap,
        inter_card_gap_jitter_seconds: gap_jitter,
    }
}

// ------------------------------------------------------- alerts (lenient) --

fn category(raw: Option<RawAlertCategory>, default_enabled: bool) -> CategoryAlertConfig {
    let raw = raw.unwrap_or_default();
    CategoryAlertConfig {
        enabled: raw.enabled.unwrap_or(default_enabled),
        webhook_url_override: raw.discord_webhook_url,
    }
}

/// What `[alerts.sms]` means when the section says nothing at all: exactly
/// what `[sms]` already said. A deployment written before `[alerts]` existed
/// keeps forwarding SMS to the same webhook without touching its config.
fn legacy_sms_category(sms: &SmsConfig) -> CategoryAlertConfig {
    let webhook = sms.discord_webhook_url.expose_secret();
    CategoryAlertConfig {
        enabled: sms.enabled,
        webhook_url_override: (!webhook.is_empty()).then(|| Secret::new(webhook.clone())),
    }
}

fn build_alerts(raw: RawAlerts, sms: &SmsConfig) -> AlertsConfig {
    let d = AlertsConfig::default();

    let sms_cat = match raw.sms {
        None => legacy_sms_category(sms),
        Some(c) => category(Some(c), sms.enabled),
    };

    let ml_raw = raw.module_lifecycle.unwrap_or_default();
    let module_lifecycle = CategoryAlertConfig {
        enabled: ml_raw.enabled.unwrap_or(false),
        webhook_url_override: ml_raw.discord_webhook_url,
    };
    let module_lifecycle_thresholds = ModuleLifecycleThresholds {
        at_worker_unresponsive_sec: ml_raw
            .at_worker_unresponsive_sec
            .and_then(|v| {
                in_range_or_warn(
                    v,
                    "alerts.module_lifecycle.at_worker_unresponsive_sec",
                    5..=600,
                )
            })
            .unwrap_or(d.module_lifecycle_thresholds.at_worker_unresponsive_sec),
    };

    let tf_raw = raw.tunnel_failure.unwrap_or_default();
    let tunnel_failure = CategoryAlertConfig {
        enabled: tf_raw.enabled.unwrap_or(false),
        webhook_url_override: tf_raw.discord_webhook_url,
    };
    let tunnel_failure_thresholds = TunnelFailureThresholds {
        unhealthy_sec: tf_raw
            .unhealthy_sec
            .and_then(|v| in_range_or_warn(v, "alerts.tunnel_failure.unhealthy_sec", 30..=3600))
            .unwrap_or(d.tunnel_failure_thresholds.unhealthy_sec),
    };

    let rl_raw = raw.registration_loss.unwrap_or_default();
    let registration_loss = CategoryAlertConfig {
        enabled: rl_raw.enabled.unwrap_or(false),
        webhook_url_override: rl_raw.discord_webhook_url,
    };
    let registration_loss_thresholds = RegistrationLossThresholds {
        unhealthy_sec: rl_raw
            .unhealthy_sec
            .and_then(|v| in_range_or_warn(v, "alerts.registration_loss.unhealthy_sec", 30..=3600))
            .unwrap_or(d.registration_loss_thresholds.unhealthy_sec),
    };

    AlertsConfig {
        // Falls back to the legacy `[sms].discord_webhook_url` so a
        // deployment that never adopted `[alerts]` keeps its SMS forwarding.
        default_webhook_url: raw
            .discord_webhook_url
            .unwrap_or_else(|| sms.discord_webhook_url.clone()),
        sms: sms_cat,
        module_lifecycle,
        registration_loss,
        tunnel_failure,
        missed_call: category(raw.missed_call, false),
        module_lifecycle_thresholds,
        tunnel_failure_thresholds,
        registration_loss_thresholds,
    }
}

// --------------------------------------------------------- vowifi / volte --

fn digits(s: &str, key: &str, len: std::ops::RangeInclusive<usize>) -> BridgeResult<()> {
    if !s.chars().all(|c| c.is_ascii_digit()) || !len.contains(&s.len()) {
        return Err(BridgeError::Config(format!(
            "field {key} must be {}-{} ASCII digits, got {s:?}",
            len.start(),
            len.end()
        )));
    }
    Ok(())
}

fn build_vowifi(raw: RawVowifi) -> BridgeResult<VowifiConfig> {
    let mut line_overrides = Vec::with_capacity(raw.line.len());
    for (i, l) in raw.line.into_iter().enumerate() {
        // Half a PLMN is not usable: the pair is what identifies the home
        // network, and one without the other silently falls back to
        // auto-derivation, which is not what the operator asked for.
        if l.mcc.is_some() != l.mnc.is_some() {
            return Err(BridgeError::Config(format!(
                "vowifi.line[{i}]: mcc and mnc must be set together"
            )));
        }
        if let Some(imsi) = &l.imsi_override {
            digits(imsi, "vowifi.line.imsi_override", 6..=15)?;
        }
        if let Some(imei) = &l.imei_override {
            digits(imei, "vowifi.line.imei_override", 14..=16)?;
        }
        if l.pcsc_reader {
            if l.modem_serial.is_some() || l.modem_port.is_some() {
                return Err(BridgeError::Config(format!(
                    "vowifi.line[{i}]: pcsc_reader = true cannot be combined with \
                     modem_serial/modem_port — a line is either modem-backed or \
                     card-reader-backed, never both"
                )));
            }
            // `mcc`/`mnc` are optional here, exactly as on a modem line:
            // both derive from files on the card itself (EF_IMSI for the
            // digits, EF_AD for the MNC length), which `vowifi-plmn
            // --pcsc-imsi` reads over PC/SC with no modem involved. Only the
            // legacy `AT+COPS` fallback is modem-only, so a card whose EF_AD
            // omits the MNC-length byte is the one case that still needs
            // them pinned — and it fails loudly at startup saying so.
            //
            // `imsi_override` stays mandatory, but not because the IMSI is
            // unreadable — `PcscTransport::connect` reads EF_IMSI off every
            // candidate reader. It is the *reader-to-line binding key*:
            // which physical card this line owns has to be known before any
            // card session exists, and strongSwan's `eap-sim-pcsc` needs it
            // in the rendered NAI at orchestration time for the same reason.
            if l.imsi_override.is_none() {
                return Err(BridgeError::Config(format!(
                    "vowifi.line[{i}]: pcsc_reader = true requires imsi_override \
                     — it names which reader's card this line owns"
                )));
            }
        }
        line_overrides.push(VowifiLineOverride {
            modem_serial: l.modem_serial,
            modem_port: l.modem_port,
            mcc: l.mcc,
            mnc: l.mnc,
            imsi_override: l.imsi_override,
            imei_override: l.imei_override,
            pcsc_reader: l.pcsc_reader,
        });
    }

    // Two `pcsc_reader` lines sharing an IMSI would both resolve to the same
    // physical card, leaving whatever SIM the other line meant unused
    // (specs/023-omnikey-pcsc-vowifi).
    let mut seen = std::collections::HashSet::new();
    for o in line_overrides.iter().filter(|o| o.pcsc_reader) {
        if let Some(imsi) = &o.imsi_override {
            if !seen.insert(imsi.clone()) {
                return Err(BridgeError::Config(format!(
                    "two [[vowifi.line]] entries with pcsc_reader = true share \
                     imsi_override {imsi:?}; each card-reader line must name its \
                     own card"
                )));
            }
        }
    }

    // Derived fields keep their placeholder defaults here and are overwritten
    // per line by `vowifi::discovery::resolve_one_line`. They are not TOML
    // keys — see `raw::RawVowifi`.
    let d = VowifiConfig::default();
    Ok(VowifiConfig {
        enabled: raw.enabled,
        use_tcp: raw.use_tcp,
        sec_agree: raw.sec_agree,
        pcscf_source_path: raw.pcscf_source_path,
        control_port: in_range(raw.control_port, "vowifi.control_port", 1..=65535)?,
        wideband: raw.wideband,
        apn: raw.apn,
        epdg_fqdn: raw.epdg_fqdn,
        epdg_ip: raw.epdg_ip.filter(|s| !s.is_empty()),
        src_addr: raw.src_addr.filter(|s| !s.is_empty()),
        keepalive_interval_sec: in_range(
            raw.keepalive_interval_sec,
            "vowifi.keepalive_interval_sec",
            1..=3600,
        )?,
        tunnel_engine: {
            if raw.tunnel_engine != "swu" && raw.tunnel_engine != "strongswan" {
                return Err(BridgeError::Config(format!(
                    "vowifi.tunnel_engine must be \"swu\" or \"strongswan\", got {:?}",
                    raw.tunnel_engine
                )));
            }
            raw.tunnel_engine
        },
        vpcd_host: raw.vpcd_host,
        vpcd_port: in_range(raw.vpcd_port, "vowifi.vpcd_port", 1..=65535)?,
        max_lines: in_range(raw.max_lines, "vowifi.max_lines", 1..=64)?,
        line_overrides,
        ..d
    })
}

fn build_volte(raw: RawVolte) -> BridgeResult<VolteConfig> {
    let mut line_overrides = Vec::with_capacity(raw.line.len());
    for (i, l) in raw.line.into_iter().enumerate() {
        // `AT+CGDCONT=0` is not a context an operator can attach.
        if l.cid == Some(0) {
            return Err(BridgeError::Config(format!(
                "volte.line[{i}].cid must be a non-zero PDP context id"
            )));
        }
        // Caught here rather than at attach time: an unparseable P-CSCF fails
        // silently at registration, long after startup looked fine.
        if let Some(addr) = &l.pcscf {
            if addr.parse::<std::net::IpAddr>().is_err() {
                return Err(BridgeError::Config(format!(
                    "volte.line[{i}].pcscf is not a valid IP address: {addr}"
                )));
            }
        }
        line_overrides.push(VolteLineOverride {
            modem_serial: l.modem_serial,
            modem_port: l.modem_port,
            cid: l.cid,
            apn: l.apn,
            pcscf: l.pcscf,
            iface: l.iface,
            msisdn: l.msisdn,
        });
    }

    let d = VolteConfig::default();
    Ok(VolteConfig {
        enabled: raw.enabled,
        pcscf_source_path: raw.pcscf_source_path,
        status_path: raw.status_path,
        lock_path: raw.lock_path,
        bridge_inbound: raw.bridge_inbound,
        max_lines: in_range(raw.max_lines, "volte.max_lines", 1..=64)?,
        line_overrides,
        ..d
    })
}

// ----------------------------------------------------------------- entry ---

/// Assembles the runtime config from the deserialised document.
pub fn build(raw: RawConfig) -> BridgeResult<AppConfig> {
    let sms = build_sms(raw.sms)?;
    let alerts = build_alerts(raw.alerts, &sms);
    Ok(AppConfig {
        sip: build_sip(raw.sip)?,
        bridge: build_bridge(raw.bridge)?,
        sms,
        metrics: build_metrics(raw.metrics)?,
        modules: build_modules(raw.modules)?,
        resilience: build_resilience(raw.resilience)?,
        control: build_control(raw.control)?,
        audio: build_audio(raw.audio)?,
        modem_audio: build_modem_audio(raw.modem_audio)?,
        scheduled_restart: build_scheduled_restart(raw.scheduled_restart),
        vowifi: build_vowifi(raw.vowifi)?,
        volte: build_volte(raw.volte)?,
        logging: build_logging(raw.logging)?,
        alerts,
    })
}
