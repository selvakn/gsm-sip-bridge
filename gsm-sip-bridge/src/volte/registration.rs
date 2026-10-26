//! Keeping the LTE IMS registration's attachment usable, and the
//! operator-facing vocabulary for it (specs/015-volte-host-ims, FR-016,
//! FR-022).
//!
//! Deliberately reuses `crate::ims`'s `RegistrationState` rather than
//! inventing a parallel one, so an operator reading VoLTE status sees the same
//! words as in `vowifi-status`.

use super::VolteSettings;
use crate::ims::RegistrationState;
use crate::metrics;

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
pub fn refresh_attachment(settings: &VolteSettings) -> Result<(), String> {
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

#[cfg(test)]
mod tests {
    use super::*;

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
    fn state_labels_match_the_shared_vocabulary() {
        // FR-022: the same words vowifi-status uses, because it is the same
        // enum rather than a parallel one.
        assert_eq!(state_label(RegistrationState::Registered), "registered");
        assert_eq!(state_label(RegistrationState::Renewing), "renewing");
        assert_eq!(state_label(RegistrationState::Failed), "failed");
        assert_eq!(state_label(RegistrationState::Unregistered), "unregistered");
    }
}
