use gsm_sip_bridge::modules::at_commander::{AtCommander, AtResponse};
use std::io::{Read, Write};
use std::time::Duration;

/// The far ("modem") end of a mocked serial link, plus the `AtCommander`
/// speaking to it: a writer to feed it canned responses, a reader to observe
/// the commands it sent, and the commander under test.
type MockSerial = (Box<dyn Write + Send>, Box<dyn Read + Send>, AtCommander);

fn create_mock_serial() -> Option<MockSerial> {
    let (server, client) = std::os::unix::net::UnixStream::pair().ok()?;
    server.set_nonblocking(false).ok()?;
    client.set_nonblocking(false).ok()?;

    let at = AtCommander::from_stream(client, Duration::from_secs(2));
    Some((Box::new(server.try_clone().unwrap()), Box::new(server), at))
}

#[test]
fn test_at_csq_happy_path() {
    let pair = create_mock_serial();
    if pair.is_none() {
        eprintln!("skipping: UnixStream pair not available");
        return;
    }
    let (mut writer, _reader, mut at) = pair.unwrap();

    std::thread::spawn(move || {
        std::thread::sleep(Duration::from_millis(50));
        writer.write_all(b"+CSQ: 22,99\r\nOK\r\n").unwrap();
    });

    let resp = at.send_command("AT+CSQ").unwrap();
    match resp {
        AtResponse::Ok(lines) => {
            assert!(lines.iter().any(|l| l.contains("+CSQ: 22,99")));
        }
        other => panic!("expected Ok, got {:?}", other),
    }
}

#[test]
fn test_at_error_response() {
    let pair = create_mock_serial();
    if pair.is_none() {
        eprintln!("skipping: UnixStream pair not available");
        return;
    }
    let (mut writer, _reader, mut at) = pair.unwrap();

    std::thread::spawn(move || {
        std::thread::sleep(Duration::from_millis(50));
        writer.write_all(b"ERROR\r\n").unwrap();
    });

    let resp = at.send_command("AT+GARBAGE").unwrap();
    matches!(resp, AtResponse::Error(_));
}

#[test]
fn test_at_cme_error() {
    let pair = create_mock_serial();
    if pair.is_none() {
        eprintln!("skipping: UnixStream pair not available");
        return;
    }
    let (mut writer, _reader, mut at) = pair.unwrap();

    std::thread::spawn(move || {
        std::thread::sleep(Duration::from_millis(50));
        writer.write_all(b"+CME ERROR: 10\r\n").unwrap();
    });

    let resp = at.send_command("AT+COPS?").unwrap();
    match resp {
        AtResponse::CmeError(code, _) => assert_eq!(code, 10),
        other => panic!("expected CmeError, got {:?}", other),
    }
}
