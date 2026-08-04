//! Multi-card VoWiFi (specs/013-multi-card-vowifi): role assignment, line-table
//! resolution, and per-line resource derivation — all pure functions, tested
//! without hardware. Live multi-tunnel behavior is verified per
//! `specs/013-multi-card-vowifi/quickstart.md`.

use gsm_sip_bridge::config::VowifiConfig;
use gsm_sip_bridge::modules::discovery::{ProbedModem, SimStatus};
use gsm_sip_bridge::vowifi::discovery::{resolve_lines, RoleAssignment};
use std::path::PathBuf;

fn ready_modem(card_id: &str, port: &str, audio: bool) -> ProbedModem {
    ProbedModem {
        card_id: card_id.to_string(),
        model: "EC200",
        usb_serial: card_id.to_string(),
        has_audio_capability: audio,
        audio_device: audio.then(|| "hw:0,0".to_string()),
        net_device: None,
        at_port: Some(PathBuf::from(port)),
        sim_status: Some(SimStatus::Ready {
            imsi: format!("{card_id}-imsi"),
        }),
    }
}

fn unreadable_modem(card_id: &str, port: &str) -> ProbedModem {
    ProbedModem {
        card_id: card_id.to_string(),
        model: "EC200",
        usb_serial: card_id.to_string(),
        has_audio_capability: true,
        audio_device: Some("hw:0,0".to_string()),
        net_device: None,
        at_port: Some(PathBuf::from(port)),
        sim_status: Some(SimStatus::Unreadable("AT+CPIN? timed out".to_string())),
    }
}

/// specs/026-disable-circuit-switched FR-010b: freeing modems that
/// `[cs].enabled = false` no longer reserves for the circuit-switched path
/// (FR-010a) must not bypass VoWiFi's own admission rules — the readiness
/// filter and `max_lines` bound apply to the newly offered candidates
/// exactly as they do to any other VoWiFi candidate. Chains the real
/// `RoleAssignment::from_probed` into the real `resolve_lines`, not a
/// fabricated `RoleAssignment` literal, so the full FR-010a → FR-010b
/// pipeline is exercised together.
#[test]
fn modems_freed_by_disabling_cs_still_respect_max_lines() {
    // Four voice-capable modems with no override: with the path off, every
    // one is offered to VoWiFi (FR-010a) — none would reach `vowifi` at all
    // with the path on, since the default rule reserves audio-capable
    // modems for circuit-switched use.
    let modems: Vec<ProbedModem> = (0..4)
        .map(|i| ready_modem(&format!("ec20-{i:06}"), &format!("/dev/ttyUSB{i}"), true))
        .collect();

    let assignment = RoleAssignment::from_probed(&modems, &[], false);
    assert!(
        assignment.circuit_switched.is_empty(),
        "nothing should be reserved for a disabled path"
    );
    assert_eq!(assignment.vowifi.len(), 4);

    let base = VowifiConfig {
        max_lines: 2,
        ..Default::default()
    };
    let result = resolve_lines(&assignment, &base);

    assert_eq!(
        result.lines.len(),
        2,
        "max_lines still bounds how many freed candidates become lines"
    );
    assert_eq!(
        result
            .failed
            .iter()
            .filter(|f| f.reason == "max_lines_exceeded")
            .count(),
        2,
        "the excess candidates are reported as failed, not silently dropped, and not an error"
    );
}

/// FR-010b: a modem freed by disabling `[cs]` that fails the readiness
/// filter (unreadable SIM) is skipped like any other failed candidate and
/// does not prevent the remaining lines from resolving — the edge case the
/// spec calls out as "freed modem is unusable".
#[test]
fn a_freed_but_unusable_modem_does_not_block_the_other_lines() {
    let modems = vec![
        ready_modem("ec20-AAAAAA", "/dev/ttyUSB0", true),
        unreadable_modem("ec20-BBBBBB", "/dev/ttyUSB1"),
    ];

    let assignment = RoleAssignment::from_probed(&modems, &[], false);
    assert_eq!(assignment.vowifi.len(), 2, "both offered to VoWiFi");

    let base = VowifiConfig::default();
    let result = resolve_lines(&assignment, &base);

    assert_eq!(result.lines.len(), 1);
    assert_eq!(result.lines[0].card_id, "ec20-AAAAAA");
    assert!(result.failed.iter().any(|f| f.card_id == "ec20-BBBBBB"));
}
