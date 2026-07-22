//! Keeping the LTE IMS registration alive, and making it observable
//! (specs/015-volte-host-ims, US4 — FR-016, FR-022, FR-023).
//!
//! Deliberately reuses `crate::ims`'s lifecycle vocabulary rather than
//! inventing a parallel one: `RegistrationState`, `RegistrationStatus` and
//! `renewal_due` are the same types the VoWiFi agent uses, so an operator
//! reading `volte-status` sees the same words as in `vowifi-status` (FR-022)
//! and there is one renewal policy rather than two (SC-007).
//!
//! ## Why a status file rather than the VoWiFi control protocol
//!
//! `vowifi-status` queries running agents over a control socket because
//! several long-lived agents must be interrogated, each owning state no other
//! process can see. Here there is exactly one process holding exactly one
//! registration, and the only reader is a CLI on the same machine. A small
//! file is sufficient, needs no protocol, and cannot fail in the ways a socket
//! can — Constitution Principle V (take the option with fewer moving parts).
//! If VoLTE ever grows to multiple concurrent lines, this is the piece to
//! revisit.

use super::VolteSettings;
use crate::error::BridgeResult;
use crate::ims::{self, ImsRegisterConfig, RegisterOutcome, RegistrationState, RegistrationStatus};
use crate::metrics;
use std::path::Path;
use std::time::{Duration, SystemTime, UNIX_EPOCH};

/// Renew this far ahead of expiry, leaving room for the renewal's own network
/// round-trip and AKA challenge. Same value the VoWiFi agent uses.
pub const RENEWAL_HEADROOM: Duration = Duration::from_secs(300);
/// First retry delay after a failed renewal; doubles up to `RETRY_MAX_BACKOFF`.
pub const RETRY_INITIAL_BACKOFF: Duration = Duration::from_secs(5);
/// Ceiling for the retry backoff — "bounded schedule" in FR-016.
pub const RETRY_MAX_BACKOFF: Duration = Duration::from_secs(300);
/// How often the loop wakes to check whether a renewal is due.
pub const POLL_INTERVAL: Duration = Duration::from_secs(30);
/// Where the registration state is published for `volte-status`.
pub const DEFAULT_STATUS_PATH: &str = "/tmp/volte-registration-status";

/// Doubles the backoff, capped. Pure so the schedule is testable without
/// waiting on real time.
pub fn next_backoff(current: Duration) -> Duration {
    let doubled = current.saturating_mul(2);
    if doubled > RETRY_MAX_BACKOFF {
        RETRY_MAX_BACKOFF
    } else {
        doubled
    }
}

/// The registration lifetime the network actually granted.
///
/// A registrar may grant less than was requested, and renewing on the
/// requested value would then leave a window where the binding has lapsed but
/// we still believe it is live. Prefers the `Expires` header, falls back to
/// `Contact`'s `expires=` parameter, then to what was asked for.
pub fn granted_expires(headers: &[(String, String)], requested: u32) -> u32 {
    for (name, value) in headers {
        if name.eq_ignore_ascii_case("expires") {
            if let Ok(v) = value.trim().parse::<u32>() {
                return v;
            }
        }
    }
    for (name, value) in headers {
        if name.eq_ignore_ascii_case("contact") {
            if let Some(rest) = value.to_ascii_lowercase().find("expires=").map(|i| i + 8) {
                let tail: String = value[rest..]
                    .chars()
                    .take_while(|c| c.is_ascii_digit())
                    .collect();
                if let Ok(v) = tail.parse::<u32>() {
                    return v;
                }
            }
        }
    }
    requested
}

/// Operator-facing name for a registration state. The vocabulary is the shared
/// `RegistrationState` enum, which is what makes VoLTE and VoWiFi status read
/// the same (FR-022).
pub fn state_label(state: RegistrationState) -> &'static str {
    match state {
        RegistrationState::Unregistered => "unregistered",
        RegistrationState::Registering => "registering",
        RegistrationState::Registered => "registered",
        RegistrationState::Renewing => "renewing",
        RegistrationState::Failed => "failed",
    }
}

fn to_unix(t: SystemTime) -> u64 {
    t.duration_since(UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0)
}

fn from_unix(secs: u64) -> SystemTime {
    UNIX_EPOCH + Duration::from_secs(secs)
}

/// Serialises a status snapshot. Plain `key=value` lines: readable by a human
/// tailing the file and trivially parseable, with no dependency added.
pub fn render_status(s: &RegistrationStatus) -> String {
    let mut out = format!("state={}\n", state_label(s.state));
    if let Some(t) = s.registered_at {
        out.push_str(&format!("registered_at={}\n", to_unix(t)));
    }
    if let Some(t) = s.expires_at {
        out.push_str(&format!("expires_at={}\n", to_unix(t)));
    }
    if let Some((t, reason)) = &s.last_failure {
        out.push_str(&format!("last_failure_at={}\n", to_unix(*t)));
        // Newlines would corrupt the line-oriented format.
        out.push_str(&format!("last_failure={}\n", reason.replace('\n', " ")));
    }
    out
}

/// Parses what `render_status` wrote. Returns `None` only when no state line
/// is present at all — a partially written file still yields what it has.
pub fn parse_status(text: &str) -> Option<RegistrationStatus> {
    let mut status = RegistrationStatus::default();
    let mut saw_state = false;
    let mut failure_at: Option<SystemTime> = None;
    let mut failure_reason: Option<String> = None;

    for line in text.lines() {
        let Some((key, value)) = line.split_once('=') else {
            continue;
        };
        match key.trim() {
            "state" => {
                status.state = match value.trim() {
                    "registered" => RegistrationState::Registered,
                    "registering" => RegistrationState::Registering,
                    "renewing" => RegistrationState::Renewing,
                    "failed" => RegistrationState::Failed,
                    _ => RegistrationState::Unregistered,
                };
                saw_state = true;
            }
            "registered_at" => status.registered_at = value.trim().parse().ok().map(from_unix),
            "expires_at" => status.expires_at = value.trim().parse().ok().map(from_unix),
            "last_failure_at" => failure_at = value.trim().parse().ok().map(from_unix),
            "last_failure" => failure_reason = Some(value.trim().to_string()),
            _ => {}
        }
    }
    if let (Some(t), Some(r)) = (failure_at, failure_reason) {
        status.last_failure = Some((t, r));
    }
    saw_state.then_some(status)
}

/// Publishes the current status. Best-effort: an unwritable status file must
/// never take down a working registration.
pub fn write_status(path: &Path, status: &RegistrationStatus) {
    if let Err(e) = std::fs::write(path, render_status(status)) {
        tracing::debug!(path = %path.display(), error = %e, "could not write the status file");
    }
}

/// Reads a published status, if any.
pub fn read_status(path: &Path) -> Option<RegistrationStatus> {
    parse_status(&std::fs::read_to_string(path).ok()?)
}

/// Human-readable status block for `volte-status` (FR-022).
pub fn status_summary(status: Option<&RegistrationStatus>) -> String {
    let Some(s) = status else {
        return "  registration   : not running (no status published)\n".to_string();
    };
    let mut out = format!("  registration   : {}\n", state_label(s.state));
    if let Some(expires_at) = s.expires_at {
        let remaining = expires_at
            .duration_since(SystemTime::now())
            .map(|d| format!("{}s", d.as_secs()))
            .unwrap_or_else(|_| "expired".to_string());
        out.push_str(&format!("  expires in     : {remaining}\n"));
    }
    if let Some((_, reason)) = &s.last_failure {
        out.push_str(&format!("  last failure   : {reason}\n"));
    }
    out
}

/// Re-establishes the network attachment before a renewal.
///
/// A dropped PDN is invisible to `ims::run_register` — it simply fails to
/// connect — so without this the loop retries REGISTER forever against a dead
/// attachment and can never recover. Observed on a soak: the carrier
/// deactivated the IMS context after roughly two hours, the modem unbound the
/// host netdev, and every subsequent renewal failed with "No route to host"
/// while the radio itself was perfectly healthy.
///
/// `attach` is idempotent, so this is a cheap no-op when the PDN is fine.
/// Returns whether the attachment is usable — attached *and* routable.
fn refresh_attachment(settings: &VolteSettings) -> Result<(), String> {
    match super::attach(settings) {
        Ok(report) if report.routed || report.iface.is_empty() => {
            metrics::VOLTE_PDN_UP.set(1.0);
            Ok(())
        }
        Ok(_) => {
            metrics::VOLTE_PDN_UP.set(0.0);
            Err("the IMS PDN is attached but has no default route".to_string())
        }
        Err(e) => {
            metrics::VOLTE_PDN_UP.set(0.0);
            Err(format!("could not re-establish the IMS PDN: {e}"))
        }
    }
}

/// Registers, then keeps the registration alive until interrupted.
///
/// Returns the outcome of the **first** attempt, so a caller can report and
/// exit on rejection rather than entering a renewal loop for a registration
/// that was never accepted.
pub fn run(
    cfg: &ImsRegisterConfig,
    settings: Option<&VolteSettings>,
    once: bool,
    status_path: &Path,
    requested_expires: u32,
) -> BridgeResult<RegisterOutcome> {
    let mut status = RegistrationStatus {
        state: RegistrationState::Registering,
        ..RegistrationStatus::default()
    };
    write_status(status_path, &status);

    let first = ims::run_register(cfg)?;

    let headers = match &first {
        RegisterOutcome::Success { headers, .. } => headers.clone(),
        RegisterOutcome::Rejected {
            status: code,
            reason,
        } => {
            status.state = RegistrationState::Failed;
            status.last_failure = Some((SystemTime::now(), format!("{code} {reason}")));
            write_status(status_path, &status);
            return Ok(first);
        }
    };

    let expires = granted_expires(&headers, requested_expires);
    status.state = RegistrationState::Registered;
    status.registered_at = Some(SystemTime::now());
    status.expires_at = Some(SystemTime::now() + Duration::from_secs(expires as u64));
    write_status(status_path, &status);
    metrics::VOLTE_REGISTRATIONS_TOTAL
        .with_label_values(&["accepted"])
        .inc();
    metrics::VOLTE_REGISTERED.set(1.0);
    tracing::info!(expires_secs = expires, "registration accepted");

    if once {
        return Ok(first);
    }

    tracing::info!(
        renewal_headroom_secs = RENEWAL_HEADROOM.as_secs(),
        "maintaining the registration; press Ctrl-C to stop"
    );
    let mut backoff = RETRY_INITIAL_BACKOFF;
    loop {
        std::thread::sleep(POLL_INTERVAL);

        let Some(expires_at) = status.expires_at else {
            continue;
        };
        if !ims::renewal_due(SystemTime::now(), expires_at, RENEWAL_HEADROOM) {
            continue;
        }

        status.state = RegistrationState::Renewing;
        write_status(status_path, &status);

        // Verify the attachment before spending a REGISTER on it. When the PDN
        // has gone, saying so is far more useful than the connect timeout that
        // would otherwise surface, and it avoids waiting out that timeout on
        // every retry.
        if let Some(settings) = settings {
            if let Err(reason) = refresh_attachment(settings) {
                tracing::warn!(
                    error = %reason,
                    retry_in_secs = backoff.as_secs(),
                    "cannot renew: the network attachment is down"
                );
                status.state = RegistrationState::Failed;
                status.last_failure = Some((SystemTime::now(), reason));
                write_status(status_path, &status);
                metrics::VOLTE_REGISTRATIONS_TOTAL
                    .with_label_values(&["attachment_down"])
                    .inc();
                metrics::VOLTE_REGISTERED.set(0.0);
                std::thread::sleep(backoff);
                backoff = next_backoff(backoff);
                continue;
            }
        }

        match ims::run_register(cfg) {
            Ok(RegisterOutcome::Success { headers, .. }) => {
                let expires = granted_expires(&headers, requested_expires);
                status.state = RegistrationState::Registered;
                status.registered_at = Some(SystemTime::now());
                status.expires_at = Some(SystemTime::now() + Duration::from_secs(expires as u64));
                status.last_failure = None;
                backoff = RETRY_INITIAL_BACKOFF;
                write_status(status_path, &status);
                metrics::VOLTE_REGISTRATIONS_TOTAL
                    .with_label_values(&["renewed"])
                    .inc();
                metrics::VOLTE_REGISTERED.set(1.0);
                tracing::info!(expires_secs = expires, "registration renewed");
            }
            other => {
                // A rejection and a transport error are both renewal failures
                // here, but the recorded reason distinguishes them (FR-023).
                let reason = match other {
                    Ok(RegisterOutcome::Rejected { status: c, reason }) => format!("{c} {reason}"),
                    Err(e) => e.to_string(),
                    Ok(RegisterOutcome::Success { .. }) => unreachable!("handled above"),
                };
                tracing::warn!(
                    error = %reason,
                    retry_in_secs = backoff.as_secs(),
                    "registration renewal failed; retrying with backoff"
                );
                status.state = RegistrationState::Failed;
                status.last_failure = Some((SystemTime::now(), reason));
                write_status(status_path, &status);
                metrics::VOLTE_REGISTRATIONS_TOTAL
                    .with_label_values(&["renewal_failed"])
                    .inc();
                // The old binding may still be live until it expires, but we
                // can no longer assert that it is.
                metrics::VOLTE_REGISTERED.set(0.0);
                std::thread::sleep(backoff);
                backoff = next_backoff(backoff);
                // Retry on the next poll rather than immediately: expires_at
                // is already in the past, so `renewal_due` fires again.
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn h(pairs: &[(&str, &str)]) -> Vec<(String, String)> {
        pairs
            .iter()
            .map(|(a, b)| (a.to_string(), b.to_string()))
            .collect()
    }

    #[test]
    fn prefers_the_expires_the_network_granted_over_what_we_asked_for() {
        // Renewing on the requested value when the registrar granted less
        // would leave a window where the binding has lapsed but we still
        // believe it is live.
        let headers = h(&[("Expires", "600")]);

        assert_eq!(granted_expires(&headers, 3600), 600);
    }

    #[test]
    fn falls_back_to_the_contact_expires_parameter() {
        let headers = h(&[("Contact", "<sip:u@[2402::1]:5060>;expires=1200;audio")]);

        assert_eq!(granted_expires(&headers, 3600), 1200);
    }

    #[test]
    fn falls_back_to_the_requested_value_when_the_network_says_nothing() {
        assert_eq!(granted_expires(&h(&[("Via", "SIP/2.0/TCP x")]), 3600), 3600);
    }

    #[test]
    fn header_matching_is_case_insensitive() {
        assert_eq!(granted_expires(&h(&[("EXPIRES", "42")]), 3600), 42);
    }

    #[test]
    fn ignores_an_unparseable_expires() {
        assert_eq!(granted_expires(&h(&[("Expires", "soon")]), 3600), 3600);
    }

    #[test]
    fn a_down_attachment_is_reported_as_such_not_as_a_connect_error() {
        // The soak's failure mode: the PDN was gone and every renewal surfaced
        // "No route to host", which points at the network rather than at the
        // real cause. A modem path that cannot open stands in for the PDN
        // being unavailable.
        let settings = VolteSettings {
            modem_port: std::path::PathBuf::from("/nonexistent/tty"),
            ..VolteSettings::default()
        };

        let err = refresh_attachment(&settings).unwrap_err();

        assert!(
            err.contains("could not re-establish the IMS PDN"),
            "got: {err}"
        );
    }

    #[test]
    fn backoff_doubles_and_is_bounded() {
        let mut b = RETRY_INITIAL_BACKOFF;
        for _ in 0..20 {
            b = next_backoff(b);
        }

        assert_eq!(b, RETRY_MAX_BACKOFF, "backoff must be bounded (FR-016)");
    }

    #[test]
    fn backoff_grows_before_it_saturates() {
        assert_eq!(
            next_backoff(Duration::from_secs(5)),
            Duration::from_secs(10)
        );
    }

    #[test]
    fn status_round_trips_through_the_file_format() {
        let now = SystemTime::now();
        let original = RegistrationStatus {
            state: RegistrationState::Registered,
            registered_at: Some(now),
            expires_at: Some(now + Duration::from_secs(3600)),
            last_failure: None,
        };

        let parsed = parse_status(&render_status(&original)).expect("should parse");

        assert_eq!(parsed.state, RegistrationState::Registered);
        assert_eq!(
            parsed.registered_at.map(to_unix),
            original.registered_at.map(to_unix)
        );
        assert_eq!(
            parsed.expires_at.map(to_unix),
            original.expires_at.map(to_unix)
        );
    }

    #[test]
    fn a_failure_reason_survives_the_round_trip() {
        let s = RegistrationStatus {
            state: RegistrationState::Failed,
            last_failure: Some((SystemTime::now(), "403 Forbidden".to_string())),
            ..RegistrationStatus::default()
        };

        let parsed = parse_status(&render_status(&s)).unwrap();

        assert_eq!(parsed.state, RegistrationState::Failed);
        assert_eq!(parsed.last_failure.unwrap().1, "403 Forbidden");
    }

    #[test]
    fn a_multiline_failure_reason_cannot_corrupt_the_format() {
        let s = RegistrationStatus {
            state: RegistrationState::Failed,
            last_failure: Some((SystemTime::now(), "line one\nstate=registered".to_string())),
            ..RegistrationStatus::default()
        };

        let parsed = parse_status(&render_status(&s)).unwrap();

        assert_eq!(
            parsed.state,
            RegistrationState::Failed,
            "an injected newline must not rewrite the state"
        );
    }

    #[test]
    fn parsing_rejects_input_with_no_state() {
        assert!(parse_status("registered_at=123\n").is_none());
        assert!(parse_status("").is_none());
    }

    #[test]
    fn state_labels_match_the_shared_vocabulary() {
        // FR-022: the same words vowifi-status uses, because it is the same
        // enum rather than a parallel one.
        assert_eq!(state_label(RegistrationState::Registered), "registered");
        assert_eq!(state_label(RegistrationState::Renewing), "renewing");
        assert_eq!(state_label(RegistrationState::Failed), "failed");
        assert_eq!(state_label(RegistrationState::Unregistered), "unregistered");
    }

    #[test]
    fn summary_reports_absence_rather_than_pretending_to_be_unregistered() {
        let s = status_summary(None);

        assert!(s.contains("not running"), "got: {s}");
    }

    #[test]
    fn summary_shows_time_remaining_and_the_last_failure() {
        let s = RegistrationStatus {
            state: RegistrationState::Registered,
            registered_at: Some(SystemTime::now()),
            expires_at: Some(SystemTime::now() + Duration::from_secs(600)),
            last_failure: Some((SystemTime::now(), "timed out".to_string())),
        };

        let out = status_summary(Some(&s));

        assert!(out.contains("registered"));
        assert!(out.contains("expires in"));
        assert!(out.contains("timed out"));
    }

    #[test]
    fn an_already_expired_registration_is_reported_as_expired() {
        let s = RegistrationStatus {
            state: RegistrationState::Registered,
            expires_at: Some(SystemTime::now() - Duration::from_secs(10)),
            ..RegistrationStatus::default()
        };

        assert!(status_summary(Some(&s)).contains("expired"));
    }
}
