//! `modem-ims`: reconciles the modem's own IMS/VoLTE stack with
//! `[vowifi].enabled`, on boot, before anything else touches the modem.
//!
//! The two cannot coexist. Our VoWiFi `REGISTER` carries
//! `+sip.instance="<urn:gsma:imei:$IMEI>"` (see `ims::sip_client`) — the
//! modem's own IMEI. A VoLTE-registered modem registers the *same* IMPU with
//! the *same* instance-id, so per RFC 5626 the network does not see two
//! devices: it treats whichever registration arrives second as a
//! re-registration of the first and deactivates the older binding. Observed
//! against Airtel: our binding was granted, then torn down ~0.7s later by a
//! reg-event `NOTIFY` carrying `state="terminated" event="deactivated"` and
//! `reason=noresource` for our own contact, after which the modem's VoLTE
//! registration won and the bridge could never receive a terminating call.
//!
//! So VoWiFi requires `<ims_conf>=2` ("forcibly disable IMS"), and the
//! circuit-switched-only deployment wants `1` ("forcibly enable") so VoLTE
//! keeps working when the bridge is not running. The setting only takes
//! effect after `AT+CFUN=1,1` and is persisted by the modem across power
//! cycles, so a wrong value survives redeploys and must be corrected here
//! rather than assumed.
//!
//! Not a preflight that merely *checks*: a modem left in the wrong mode by a
//! previous deployment would otherwise wedge every boot. It reconfigures and
//! reboots the module, which costs ~30s of modem downtime — hence "on boot,
//! before the daemon", never mid-call.

use crate::error::{BridgeError, BridgeResult};
use crate::modules::at_commander::AtCommander;
use std::path::Path;
use std::process::ExitCode;
use std::thread::sleep;
use std::time::{Duration, Instant};

/// `<ims_conf>` = 1: "forcedly enable IMS function" (EC20 AT manual 7.6).
pub const IMS_ENABLED: u8 = 1;
/// `<ims_conf>` = 2: "forcedly disable IMS function" (EC20 AT manual 7.6).
pub const IMS_DISABLED: u8 = 2;

/// How long to wait for the module to re-enumerate after `AT+CFUN=1,1`.
/// Measured at ~20s on an EC20 (USB re-enumeration); the margin covers a
/// slower cold SIM.
const REBOOT_TIMEOUT: Duration = Duration::from_secs(120);
const REBOOT_POLL_INTERVAL: Duration = Duration::from_secs(3);

/// How long to wait for the module to actually drop off the USB bus after
/// `AT+CFUN=1,1`. The modem keeps answering for a beat before it resets, so
/// without waiting for it to *go away* first, the very next probe succeeds
/// against the not-yet-rebooted modem and we would report a reboot that never
/// happened — then hand a port that is about to vanish to the USIM bridge.
const RESET_TIMEOUT: Duration = Duration::from_secs(45);
const RESET_POLL_INTERVAL: Duration = Duration::from_millis(500);

/// The `<ims_conf>` a given `[vowifi].enabled` demands.
pub fn desired_ims_conf(vowifi_enabled: bool) -> u8 {
    if vowifi_enabled {
        IMS_DISABLED
    } else {
        IMS_ENABLED
    }
}

/// What reconciling did, so the caller can decide whether to wait out a
/// reboot.
#[derive(Debug, PartialEq, Eq)]
pub enum Outcome {
    /// The modem was already in the demanded mode — no reboot, no downtime.
    AlreadyCorrect(u8),
    /// `<ims_conf>` was rewritten and `AT+CFUN=1,1` fired; the modem is now
    /// rebooting and its AT port will disappear.
    Rebooting { from: u8, to: u8 },
}

/// The testable core: query, compare, and (only on mismatch) rewrite +
/// reboot. Takes an open `AtCommander` so tests can drive it with a scripted
/// stream instead of real hardware, the same split as `vowifi::imsi`.
pub fn reconcile(at: &mut AtCommander, desired: u8) -> BridgeResult<Outcome> {
    let current = at.query_ims_conf()?;
    if current == desired {
        return Ok(Outcome::AlreadyCorrect(current));
    }
    at.set_ims_conf(desired)?;
    // Fire-and-forget by design: the modem does not answer OK before it
    // resets, so `reboot()` cannot be error-checked. The re-verify after
    // re-enumeration is what actually proves the change took.
    at.reboot();
    Ok(Outcome::Rebooting {
        from: current,
        to: desired,
    })
}

/// True while the modem is still answering on `modem_port`.
fn modem_answers(modem_port: &Path) -> bool {
    modem_port.exists()
        && AtCommander::open(modem_port)
            .map(|mut at| at.send_command("AT").is_ok())
            .unwrap_or(false)
}

/// Blocks until the module has actually reset and is answering `AT` again.
///
/// Two phases, and the first is the load-bearing one: wait for the modem to
/// STOP answering. `AT+CFUN=1,1` returns OK and the port keeps working for a
/// moment before the module drops off the USB bus, so a naive "wait until it
/// answers" check passes instantly against the modem we just asked to reboot,
/// and the caller marches on while the port is about to disappear. Only once
/// it has gone can its return be meaningful.
fn wait_for_reboot(modem_port: &Path) -> BridgeResult<AtCommander> {
    let reset_deadline = Instant::now() + RESET_TIMEOUT;
    while modem_answers(modem_port) {
        if Instant::now() >= reset_deadline {
            return Err(BridgeError::Discovery(format!(
                "modem {} kept answering for {}s after AT+CFUN=1,1 — it never reset, so the new IMS mode is not in effect",
                modem_port.display(),
                RESET_TIMEOUT.as_secs()
            )));
        }
        sleep(RESET_POLL_INTERVAL);
    }
    tracing::info!("modem reset — waiting for it to re-enumerate");

    let back_deadline = Instant::now() + REBOOT_TIMEOUT;
    while Instant::now() < back_deadline {
        if modem_answers(modem_port) {
            // Reopen for the caller: the probe's handle is dropped above.
            return AtCommander::open(modem_port);
        }
        sleep(REBOOT_POLL_INTERVAL);
    }
    Err(BridgeError::Discovery(format!(
        "modem {} did not come back within {}s of AT+CFUN=1,1",
        modem_port.display(),
        REBOOT_TIMEOUT.as_secs()
    )))
}

fn run_inner(modem_port: &Path, vowifi_enabled: bool) -> BridgeResult<()> {
    let desired = desired_ims_conf(vowifi_enabled);
    let mut at = AtCommander::open(modem_port)?;

    match reconcile(&mut at, desired)? {
        Outcome::AlreadyCorrect(v) => {
            tracing::info!(
                ims_conf = v,
                vowifi_enabled,
                "modem IMS mode already correct — no reboot needed"
            );
            Ok(())
        }
        Outcome::Rebooting { from, to } => {
            tracing::warn!(
                from,
                to,
                vowifi_enabled,
                "modem IMS mode wrong for this deployment — rewrote it and rebooted the module (~30s of modem downtime)"
            );
            drop(at); // release the port before it disappears from /dev
            let mut at = wait_for_reboot(modem_port)?;
            // Confirms the value persisted across the reset. Note this alone
            // would NOT prove the reboot happened — AT+QCFG="ims" echoes the
            // saved value immediately, reboot or not. `wait_for_reboot` is
            // what establishes that; this just catches a modem that dropped
            // the setting.
            let confirmed = at.query_ims_conf()?;
            if confirmed != to {
                return Err(BridgeError::Discovery(format!(
                    "modem IMS mode did not stick: wanted {to}, modem reports {confirmed} after reboot"
                )));
            }
            tracing::info!(ims_conf = confirmed, "modem IMS mode reconciled");
            Ok(())
        }
    }
}

pub fn run(modem_port: &Path, vowifi_enabled: bool) -> ExitCode {
    match run_inner(modem_port, vowifi_enabled) {
        Ok(()) => ExitCode::SUCCESS,
        Err(e) => {
            eprintln!("error: {e}");
            ExitCode::FAILURE
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::{Cursor, Read, Write};
    use std::time::Duration;

    /// Mock stream: replays a scripted modem response, discards writes.
    /// Mirrors `vowifi::imsi`'s mock — the modem is hardware, unavailable in
    /// CI.
    struct MockStream {
        reader: Cursor<Vec<u8>>,
        written: Vec<u8>,
    }

    impl MockStream {
        fn new(response: &str) -> Self {
            Self {
                reader: Cursor::new(response.as_bytes().to_vec()),
                written: Vec::new(),
            }
        }
    }

    impl Read for MockStream {
        fn read(&mut self, buf: &mut [u8]) -> std::io::Result<usize> {
            self.reader.read(buf)
        }
    }

    impl Write for MockStream {
        fn write(&mut self, buf: &[u8]) -> std::io::Result<usize> {
            self.written.extend_from_slice(buf);
            Ok(buf.len())
        }
        fn flush(&mut self) -> std::io::Result<()> {
            Ok(())
        }
    }

    fn at(response: &str) -> AtCommander {
        AtCommander::from_stream(MockStream::new(response), Duration::from_secs(1))
    }

    #[test]
    fn vowifi_demands_ims_disabled_and_cs_only_demands_it_enabled() {
        assert_eq!(desired_ims_conf(true), IMS_DISABLED);
        assert_eq!(desired_ims_conf(false), IMS_ENABLED);
    }

    #[test]
    fn already_disabled_under_vowifi_is_a_no_op() {
        // The state the Airtel box ends up in: ims_conf=2, volte_cap=0.
        let mut at = at("+QCFG: \"ims\",2,0\r\nOK\r\n");
        assert_eq!(
            reconcile(&mut at, IMS_DISABLED).unwrap(),
            Outcome::AlreadyCorrect(2)
        );
    }

    #[test]
    fn ims_enabled_under_vowifi_triggers_rewrite_and_reboot() {
        // The state that caused the binding teardown: ims_conf=1, volte_cap=1.
        // Responses: the query, then the set, then the (unanswered) reboot.
        let mut at = at("+QCFG: \"ims\",1,1\r\nOK\r\nOK\r\nOK\r\n");
        assert_eq!(
            reconcile(&mut at, IMS_DISABLED).unwrap(),
            Outcome::Rebooting { from: 1, to: 2 }
        );
    }

    #[test]
    fn ims_disabled_without_vowifi_is_re_enabled() {
        let mut at = at("+QCFG: \"ims\",2,0\r\nOK\r\nOK\r\nOK\r\n");
        assert_eq!(
            reconcile(&mut at, IMS_ENABLED).unwrap(),
            Outcome::Rebooting { from: 2, to: 1 }
        );
    }

    #[test]
    fn mbn_default_zero_is_reconciled_rather_than_trusted() {
        // ims_conf=0 means "whatever the MBN says" — which may well be
        // IMS-on. Under VoWiFi that is not good enough: pin it to 2.
        let mut at = at("+QCFG: \"ims\",0,1\r\nOK\r\nOK\r\nOK\r\n");
        assert_eq!(
            reconcile(&mut at, IMS_DISABLED).unwrap(),
            Outcome::Rebooting { from: 0, to: 2 }
        );
    }

    #[test]
    fn firmware_without_an_ims_stack_is_an_error_not_a_silent_pass() {
        let mut at = at("ERROR\r\n");
        assert!(reconcile(&mut at, IMS_DISABLED).is_err());
    }
}
