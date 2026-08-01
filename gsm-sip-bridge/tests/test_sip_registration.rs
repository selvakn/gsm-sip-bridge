use gsm_sip_bridge::config::{load_config, AppConfig};
use gsm_sip_bridge::sip::RegistrationState;
use gsm_sip_bridge::sip::SipBridge;
use std::io::Write;
use tempfile::NamedTempFile;

fn test_config() -> AppConfig {
    let mut f = NamedTempFile::new().unwrap();
    writeln!(
        f,
        r#"
[sip]
server = "127.0.0.1"
port = 5060
username = "test"
password = "testpass"
transport = "udp"
"#
    )
    .unwrap();

    load_config(f.path()).unwrap()
}

#[test]
fn test_sip_bridge_initial_state() {
    let config = test_config();
    let bridge = SipBridge::new(&config);
    assert_eq!(bridge.state, RegistrationState::Unregistered);
}

#[test]
fn test_sip_bridge_register() {
    let config = test_config();
    let mut bridge = SipBridge::new(&config);
    bridge.register().unwrap();
    assert_eq!(bridge.state, RegistrationState::Registered);
}

#[test]
fn test_sip_bridge_unregister() {
    let config = test_config();
    let mut bridge = SipBridge::new(&config);
    bridge.register().unwrap();
    bridge.unregister();
    assert_eq!(bridge.state, RegistrationState::Unregistered);
}

#[test]
fn test_sip_bridge_skips_trunk_when_volte_bridge_inbound_owns_it() {
    let mut f = NamedTempFile::new().unwrap();
    writeln!(
        f,
        r#"
[sip]
server = "127.0.0.1"
port = 5060
username = "test"
password = "testpass"

[volte]
enabled = true
bridge_inbound = true
"#
    )
    .unwrap();

    let config = load_config(f.path()).unwrap();
    let mut bridge = SipBridge::new(&config);
    bridge.register().unwrap();
    assert_eq!(bridge.state, RegistrationState::Unregistered);
}

#[test]
fn test_sip_bridge_skips_trunk_when_vowifi_owns_it() {
    let mut f = NamedTempFile::new().unwrap();
    writeln!(
        f,
        r#"
[sip]
server = "127.0.0.1"
port = 5060
username = "test"
password = "testpass"

[vowifi]
enabled = true
"#
    )
    .unwrap();

    let config = load_config(f.path()).unwrap();
    let mut bridge = SipBridge::new(&config);
    bridge.register().unwrap();
    assert_eq!(bridge.state, RegistrationState::Unregistered);
}

#[test]
fn test_compute_destination_uri_did_passthrough() {
    let config = test_config();
    let bridge = SipBridge::new(&config);
    let uri = bridge.compute_destination_uri("+15551234567").unwrap();
    assert_eq!(uri, "sip:15551234567@127.0.0.1:5060");
}

#[test]
fn test_compute_destination_uri_fixed() {
    let mut f = NamedTempFile::new().unwrap();
    writeln!(
        f,
        r#"
[sip]
server = "pbx.local"
port = 5060
username = "test"
password = "pass"

[bridge]
sip_destination = "100"
"#
    )
    .unwrap();

    let config = load_config(f.path()).unwrap();
    let bridge = SipBridge::new(&config);
    let uri = bridge.compute_destination_uri("+15559999999").unwrap();
    assert_eq!(uri, "sip:100@pbx.local:5060");
}

// ------------------------------------------------------- SIP server mode ---

/// A free loopback port. Config validation rejects port 0 — correctly, since
/// an operator cannot point a handset at an ephemeral port — so the test has
/// to name a real one.
fn free_port() -> u16 {
    let probe = std::net::UdpSocket::bind("127.0.0.1:0").unwrap();
    probe.local_addr().unwrap().port()
}

/// A server-mode document: no PBX anywhere, and the bridge's own calling port
/// moved clear of the port the phones register to.
fn server_mode_config(listen_port: u16) -> AppConfig {
    let mut f = NamedTempFile::new().unwrap();
    writeln!(
        f,
        r#"
[sip]
local_port = 5062

[sip_server]
enabled = true
listen_addr = "127.0.0.1"
listen_port = {listen_port}
ring_aor = "1001"

[[sip_server.account]]
username = "1001"
password = "s3cret"
"#
    )
    .unwrap();
    load_config(f.path()).unwrap()
}

/// The whole point of the mode: it comes up with no PBX configured, where the
/// PBX path would have refused the config outright.
#[test]
fn server_mode_reaches_registered_with_no_pbx() {
    let config = server_mode_config(free_port());
    let mut bridge = SipBridge::new(&config);
    assert!(bridge.is_server_mode());

    bridge.register().expect("server mode must start");
    assert_eq!(bridge.state, RegistrationState::Registered);

    bridge.unregister();
    assert_eq!(bridge.state, RegistrationState::Unregistered);
}

/// FR-018: a call arriving with nothing registered must not be sent anywhere,
/// and the error must name the account an operator should go looking for.
#[test]
fn server_mode_has_no_destination_until_a_phone_registers() {
    let config = server_mode_config(free_port());
    let mut bridge = SipBridge::new(&config);
    bridge.register().expect("server mode must start");

    let err = bridge
        .compute_destination_uri("+15551234567")
        .expect_err("nothing is registered yet");
    assert!(err.contains("1001"), "got: {err}");

    bridge.unregister();
}

/// Exactly one component may host the registrar, and it is whichever would
/// have owned the PBX trunk. On a VoWiFi or VoLTE deployment that is the
/// telephony agent, so the circuit-switched bridge must stand down — otherwise
/// both processes race for the same UDP port (spec 024, research.md R-003).
#[test]
fn the_circuit_switched_bridge_yields_the_registrar_to_the_telephony_agent() {
    for inbound in [
        "[vowifi]\nenabled = true\n",
        "[volte]\nenabled = true\nbridge_inbound = true\n",
    ] {
        let mut f = NamedTempFile::new().unwrap();
        writeln!(
            f,
            r#"
[sip]
local_port = 5062

[sip_server]
enabled = true
listen_addr = "127.0.0.1"
listen_port = {}
ring_aor = "1001"

[[sip_server.account]]
username = "1001"
password = "s3cret"

{inbound}"#,
            free_port()
        )
        .unwrap();

        let config = load_config(f.path()).unwrap();
        let mut bridge = SipBridge::new(&config);
        bridge.register().unwrap();

        assert_eq!(
            bridge.state,
            RegistrationState::Unregistered,
            "the telephony agent owns the registrar here, so the CS bridge must not start one: \
             {inbound}"
        );
        assert!(
            bridge.compute_destination_uri("+15551234567").is_err(),
            "and it must not claim it can route a call: {inbound}"
        );
    }
}

/// Two SipBridges cannot both take the port, which is what makes the
/// config-level collision check worth having.
#[test]
fn server_mode_reports_a_port_already_in_use_rather_than_serving_nothing() {
    // Hold the port with a socket of our own, then ask the bridge for it.
    let squatter = std::net::UdpSocket::bind("127.0.0.1:0").unwrap();
    let port = squatter.local_addr().unwrap().port();

    let config = server_mode_config(port);
    let mut bridge = SipBridge::new(&config);
    let err = bridge.register().expect_err("the port is taken");

    assert!(err.contains("registrar could not listen"), "got: {err}");
    assert!(err.contains(&port.to_string()), "must name the port: {err}");
    assert_eq!(bridge.state, RegistrationState::Failed);
}
