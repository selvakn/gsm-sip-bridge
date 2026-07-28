use gsm_sip_bridge::modules::at_commander::AtCommander;
use gsm_sip_bridge::sms::reader::{delete_sms, read_sms};
use std::io::{Read, Write};
use std::time::Duration;

fn mock_at() -> Option<(std::os::unix::net::UnixStream, AtCommander)> {
    let (server, client) = std::os::unix::net::UnixStream::pair().ok()?;
    server.set_read_timeout(Some(Duration::from_secs(2))).ok()?;
    client.set_read_timeout(Some(Duration::from_secs(2))).ok()?;
    let at = AtCommander::from_stream(client, Duration::from_secs(2));
    Some((server, at))
}

#[test]
fn test_read_sms_parses_cmgr() {
    let pair = mock_at();
    if pair.is_none() {
        return;
    }
    let (mut server, mut at) = pair.unwrap();

    std::thread::spawn(move || {
        let mut buf = [0u8; 256];
        let _ = server.read(&mut buf);
        let response = "+CMGR: \"REC READ\",\"+15551234567\",,\"26/05/04,20:00:00+00\"\r\nHello world\r\nOK\r\n";
        server.write_all(response.as_bytes()).unwrap();
    });

    let sms = read_sms(&mut at, 1).unwrap();
    assert_eq!(sms.sender, "+15551234567");
    assert_eq!(sms.body, "Hello world");
    assert_eq!(sms.index, 1);
}

#[test]
fn test_delete_sms_sends_cmgd() {
    let pair = mock_at();
    if pair.is_none() {
        return;
    }
    let (mut server, mut at) = pair.unwrap();

    std::thread::spawn(move || {
        let mut buf = [0u8; 256];
        let n = server.read(&mut buf).unwrap();
        let cmd = String::from_utf8_lossy(&buf[..n]);
        assert!(cmd.contains("AT+CMGD=3"));
        server.write_all(b"OK\r\n").unwrap();
    });

    delete_sms(&mut at, 3).unwrap();
}

#[test]
fn test_read_sms_error_handling() {
    let pair = mock_at();
    if pair.is_none() {
        return;
    }
    let (mut server, mut at) = pair.unwrap();

    std::thread::spawn(move || {
        let mut buf = [0u8; 256];
        let _ = server.read(&mut buf);
        server.write_all(b"+CME ERROR: 321\r\n").unwrap();
    });

    let result = read_sms(&mut at, 5);
    assert!(result.is_err());
}
