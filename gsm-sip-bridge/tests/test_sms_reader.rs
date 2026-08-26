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

/// Builds a hex-encoded SMS-DELIVER PDU (TS 23.040 §9.2.2.1) with no SMSC
/// address (a leading `00` length byte, TS 27.005 §3.1) — the shape
/// `AT+CMGR`/`AT+CMGL` hand back in PDU mode, which `sms::reader` now
/// decodes instead of text mode (specs/041 conformance review, CS-01).
///
/// Uses TP-DCS 8-bit/octet encoding so `text` can be embedded byte-for-byte
/// rather than GSM7 septet-packed — simpler for a test fixture, and
/// `ims::sms_pdu::Alphabet::Octet` is exercised by its own unit tests
/// already, so nothing here needs to also cover GSM7 packing.
fn build_test_pdu(international_sender_digits: &str, text: &str) -> String {
    let digits: Vec<u8> = international_sender_digits
        .bytes()
        .map(|b| b - b'0')
        .collect();
    let digit_count = digits.len();
    let mut padded = digits.clone();
    if !padded.len().is_multiple_of(2) {
        padded.push(0x0F);
    }
    let oa_bytes: Vec<u8> = padded.chunks(2).map(|c| (c[1] << 4) | c[0]).collect();

    let mut tpdu = vec![0x00u8]; // SMS-DELIVER, no UDHI
    tpdu.push(digit_count as u8);
    tpdu.push(0x91); // international
    tpdu.extend_from_slice(&oa_bytes);
    tpdu.push(0x00); // TP-PID
    tpdu.push(0x04); // TP-DCS: general group, 8-bit/octet data
    tpdu.extend_from_slice(&[0u8; 7]); // TP-SCTS, not asserted on
    tpdu.push(text.len() as u8); // TP-UDL: byte count for octet alphabet
    tpdu.extend_from_slice(text.as_bytes());

    let mut pdu = vec![0x00u8]; // SMSC address field: absent
    pdu.extend_from_slice(&tpdu);
    pdu.iter().map(|b| format!("{b:02X}")).collect()
}

#[test]
fn test_read_sms_parses_cmgr() {
    let pair = mock_at();
    if pair.is_none() {
        return;
    }
    let (mut server, mut at) = pair.unwrap();
    let pdu = build_test_pdu("15551234567", "Hello world");

    std::thread::spawn(move || {
        let mut buf = [0u8; 256];
        let _ = server.read(&mut buf);
        let response = format!("+CMGR: 1,,25\r\n{pdu}\r\nOK\r\n");
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

/// A concatenated message's UDH is invisible to text mode but decodes
/// correctly in PDU mode — the exact fidelity gap CS-01 exists to close, and
/// worth a dedicated regression since the plain-message test above can't
/// exercise it (no UDH at all).
#[test]
fn test_read_sms_decodes_a_concatenated_part() {
    let pair = mock_at();
    if pair.is_none() {
        return;
    }
    let (mut server, mut at) = pair.unwrap();

    // Hand-built: SMS-DELIVER, TP-UDHI set, UDH = concatenation IE
    // (ref=0xAA, total=2, seq=1), then 8-bit text "part1".
    let udh = [0x00u8, 0x03, 0xAA, 0x02, 0x01];
    let text = b"part1";
    let mut tp_ud = vec![udh.len() as u8];
    tp_ud.extend_from_slice(&udh);
    tp_ud.extend_from_slice(text);

    let mut tpdu = vec![0x40u8]; // SMS-DELIVER, TP-UDHI set
    tpdu.push(11); // TP-OA length (digits)
    tpdu.push(0x91);
    tpdu.extend_from_slice(&[0x51, 0x55, 0x21, 0x43, 0x65, 0xF7]); // +15551234567
    tpdu.push(0x00); // TP-PID
    tpdu.push(0x04); // TP-DCS: octet
    tpdu.extend_from_slice(&[0u8; 7]);
    tpdu.push(tp_ud.len() as u8); // TP-UDL: byte count, octet alphabet
    tpdu.extend_from_slice(&tp_ud);

    let mut pdu = vec![0x00u8];
    pdu.extend_from_slice(&tpdu);
    let hex: String = pdu.iter().map(|b| format!("{b:02X}")).collect();

    std::thread::spawn(move || {
        let mut buf = [0u8; 256];
        let _ = server.read(&mut buf);
        let response = format!("+CMGR: 1,,{}\r\n{hex}\r\nOK\r\n", tpdu.len());
        server.write_all(response.as_bytes()).unwrap();
    });

    let sms = read_sms(&mut at, 2).unwrap();
    assert_eq!(sms.sender, "+15551234567");
    assert_eq!(sms.body, "[1/2] part1");
}
