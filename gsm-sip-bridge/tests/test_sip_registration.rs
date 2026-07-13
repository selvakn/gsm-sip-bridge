mod common;

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
fn test_compute_destination_uri_did_passthrough() {
    let config = test_config();
    let bridge = SipBridge::new(&config);
    let uri = bridge.compute_destination_uri("+15551234567");
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
    let uri = bridge.compute_destination_uri("+15559999999");
    assert_eq!(uri, "sip:100@pbx.local:5060");
}
