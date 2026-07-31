//! Where an inbound carrier call should be sent.
//!
//! One rule for all three call paths. Before spec 024 this logic existed twice
//! — `SipBridge::compute_destination_uri` and `vowifi::pbx_dest_uri`, deliberate
//! duplicates of each other — and SIP server mode needs the *same* new branch in
//! both. Writing it twice would mean keeping two copies of the DID-passthrough
//! rule in step across two subsystems, so the two copies become this one enum.

use std::time::Instant;

use super::server::BindingStore;

/// The destination for a call arriving from the carrier.
pub enum CallTarget<'a> {
    /// An external PBX, as every deployment worked before spec 024.
    Pbx {
        server: &'a str,
        port: u16,
        /// Empty means DID passthrough: dial the caller's own number at the
        /// PBX. Otherwise a fixed extension.
        sip_destination: &'a str,
    },
    /// An IP phone registered to this bridge's own registrar.
    RegisteredPhone {
        bindings: &'a BindingStore,
        aor: &'a str,
    },
}

impl CallTarget<'_> {
    /// The URI to INVITE for a call from `caller_did`.
    ///
    /// Fallible only because a registered phone can be absent — the PBX form
    /// cannot fail, since a configured address is always dialable even when
    /// nothing answers.
    pub fn uri_for(&self, caller_did: &str, now: Instant) -> Result<String, String> {
        match self {
            CallTarget::Pbx {
                server,
                port,
                sip_destination,
            } => {
                let raw_dest = if sip_destination.is_empty() {
                    caller_did
                } else {
                    sip_destination
                };
                let dest = raw_dest.trim_start_matches('+');
                Ok(format!("sip:{dest}@{server}:{port}"))
            }
            CallTarget::RegisteredPhone { bindings, aor } => bindings
                .get_live(aor, now)
                .map(|binding| binding.contact_uri)
                .ok_or_else(|| {
                    format!(
                        "no live registration for AOR {aor:?} — the phone has not registered, \
                         or its registration lapsed"
                    )
                }),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::sip::server::Binding;
    use std::time::Duration;

    fn pbx(sip_destination: &str) -> CallTarget<'_> {
        CallTarget::Pbx {
            server: "pbx.example.com",
            port: 5060,
            sip_destination,
        }
    }

    /// Pins the pre-existing behaviour this enum absorbed: an empty
    /// `sip_destination` dials the caller's own number at the PBX.
    #[test]
    fn an_empty_destination_passes_the_callers_did_through() {
        assert_eq!(
            pbx("").uri_for("15551234567", Instant::now()).unwrap(),
            "sip:15551234567@pbx.example.com:5060"
        );
    }

    #[test]
    fn a_configured_destination_overrides_the_callers_did() {
        assert_eq!(
            pbx("200").uri_for("15551234567", Instant::now()).unwrap(),
            "sip:200@pbx.example.com:5060"
        );
    }

    /// A `+`-prefixed number is not a valid SIP user part here, and stripping
    /// it is behaviour the PBX side has always had.
    #[test]
    fn a_leading_plus_is_stripped_from_either_source() {
        assert_eq!(
            pbx("").uri_for("+15551234567", Instant::now()).unwrap(),
            "sip:15551234567@pbx.example.com:5060"
        );
        assert_eq!(
            pbx("+200").uri_for("15551234567", Instant::now()).unwrap(),
            "sip:200@pbx.example.com:5060"
        );
    }

    #[test]
    fn a_registered_phone_is_dialled_at_its_registered_contact() {
        let bindings = BindingStore::new();
        let now = Instant::now();
        bindings.upsert(Binding {
            aor: "1001".to_string(),
            contact_uri: "sip:1001@192.168.1.50:5060".to_string(),
            source: "192.168.1.50:5060".parse().unwrap(),
            call_id: "c1".to_string(),
            cseq: 1,
            expires_at: now + Duration::from_secs(3600),
            user_agent: None,
        });

        let target = CallTarget::RegisteredPhone {
            bindings: &bindings,
            aor: "1001",
        };
        assert_eq!(
            target.uri_for("15551234567", now).unwrap(),
            "sip:1001@192.168.1.50:5060",
            "the caller's number must not affect where a registered phone is dialled"
        );
    }

    /// The error text is what an operator reads when the phone does not ring,
    /// so it must name the account it looked for.
    #[test]
    fn an_unregistered_phone_yields_an_error_naming_the_account() {
        let bindings = BindingStore::new();
        let target = CallTarget::RegisteredPhone {
            bindings: &bindings,
            aor: "1001",
        };
        let err = target.uri_for("15551234567", Instant::now()).unwrap_err();
        assert!(err.contains("1001"), "got: {err}");
        assert!(err.contains("no live registration"), "got: {err}");
    }

    #[test]
    fn a_lapsed_registration_is_treated_as_absent() {
        let bindings = BindingStore::new();
        let now = Instant::now();
        bindings.upsert(Binding {
            aor: "1001".to_string(),
            contact_uri: "sip:1001@192.168.1.50:5060".to_string(),
            source: "192.168.1.50:5060".parse().unwrap(),
            call_id: "c1".to_string(),
            cseq: 1,
            expires_at: now + Duration::from_secs(60),
            user_agent: None,
        });

        let target = CallTarget::RegisteredPhone {
            bindings: &bindings,
            aor: "1001",
        };
        assert!(target.uri_for("919", now).is_ok());
        assert!(target
            .uri_for("919", now + Duration::from_secs(61))
            .is_err());
    }
}
