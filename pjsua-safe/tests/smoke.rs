use pjsua_safe::{Endpoint, EndpointConfig, TransportType};

fn lan_ep_config(local_port: u16) -> EndpointConfig {
    EndpointConfig {
        transport: TransportType::Udp,
        local_port,
        tls_verify: false,
        jb_init_ms: 20,
        jb_min_pre: 1,
        jb_max_ms: 40,
    }
}

#[test]
fn test_endpoint_create_stub_mode() {
    let config = EndpointConfig {
        transport: TransportType::Udp,
        local_port: 15060,
        tls_verify: true,
        jb_init_ms: 20,
        jb_min_pre: 1,
        jb_max_ms: 40,
    };
    let ep = Endpoint::create(config).unwrap();
    assert!(ep.is_started());
}

#[test]
fn test_account_register_stub_mode() {
    use pjsua_safe::{Account, AccountConfig};

    let ep = Endpoint::create(lan_ep_config(15061)).unwrap();

    let acc_config = AccountConfig {
        sip_server: "127.0.0.1".into(),
        sip_port: 5060,
        username: "test".into(),
        password: "test123".into(),
        display_name: "Test User".into(),
    };
    let acc = Account::register(&ep, acc_config, None).unwrap();
    assert!(acc.is_registered());
}

#[test]
fn test_call_make_stub_mode() {
    use pjsua_safe::{Account, AccountConfig, Call};

    let ep = Endpoint::create(lan_ep_config(15062)).unwrap();

    let acc_config = AccountConfig {
        sip_server: "127.0.0.1".into(),
        sip_port: 5060,
        username: "test".into(),
        password: "pass".into(),
        display_name: "Caller".into(),
    };
    let acc = Account::register(&ep, acc_config, None).unwrap();

    let mut call = Call::make(&acc, "sip:100@127.0.0.1:5060", None, &[]).unwrap();
    assert_eq!(call.state(), pjsua_safe::CallState::Calling);

    call.hangup().unwrap();
    assert_eq!(call.state(), pjsua_safe::CallState::Disconnected);
}
