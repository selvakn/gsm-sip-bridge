//! What PJSIP actually *offers* once configured the way the VoWiFi bridge's
//! Agent B configures it (16 kHz bridge, G.722 first, L16 enabled) — captured
//! off the wire from a real INVITE, because the codec list PJSIP reports via
//! `Endpoint::codecs` and the codec list it puts into an SDP offer are not the
//! same thing.
//!
//! Both assertions here are regressions that actually shipped: a live Airtel
//! call bridged at 8 kHz on the veth despite the carrier leg being AMR-WB, and
//! PJSUA's default `txt_cnt = 1` appended a T.140 `m=text` section whose
//! payload types are numbered independently of the audio section's (it reuses
//! 100 — the audio section's L16 — for `red/1000`).
//!
//! Needs a real linked PJSIP built with `PJMEDIA_CODEC_L16_HAS_16KHZ_MONO`
//! (see `docker/pjsip-config-site.h`), so it is `pjsip-linked`-gated.
#![cfg(feature = "pjsip-linked")]

use pjsua_safe::{Account, AccountConfig, Call, Endpoint, EndpointConfig, TransportType};
use std::net::UdpSocket;
use std::time::Duration;

#[test]
fn the_sdp_offer_carries_l16_and_exactly_one_audio_stream() {
    // Stands in for Agent A's veth UAS: never answers, just captures the offer.
    let uas = UdpSocket::bind("127.0.0.1:15082").expect("bind fake UAS");
    uas.set_read_timeout(Some(Duration::from_secs(3))).unwrap();

    let ep = Endpoint::create(EndpointConfig {
        transport: TransportType::Udp,
        local_port: 15081,
        tls_verify: false,
        clock_rate: 16000,
        jb_init_ms: 20,
        jb_min_pre: 1,
        jb_max_ms: 40,
        vad_enabled: true,
        tx_level: 1.0,
        snd_rec_latency_ms: 150,
        snd_play_latency_ms: 150,
    })
    .expect("endpoint create");
    ep.set_null_sound_device().expect("null sound device");
    // The same priorities `vowifi::prioritize_wideband_codecs` applies.
    ep.set_codec_priority("G722/16000/1", 200).expect("G722");
    ep.set_codec_priority("L16/16000/1", 1).expect("L16");

    let acc = Account::register(
        &ep,
        AccountConfig {
            sip_server: "127.0.0.1".into(),
            sip_port: 15083,
            username: "agent-b".into(),
            password: "pass".into(),
            display_name: "Agent B".into(),
        },
        None,
    )
    .expect("account add");
    let _call = Call::make(&acc, "sip:agent-a@127.0.0.1:15082", None, &[]).expect("call");

    let mut buf = [0u8; 8192];
    let (n, _) = uas.recv_from(&mut buf).expect("INVITE should arrive");
    let text = String::from_utf8_lossy(&buf[..n]);
    let sdp = text.split("\r\n\r\n").nth(1).unwrap_or("");

    assert!(
        sdp.contains("L16/16000"),
        "a wideband call can only cross the veth losslessly if L16 is offered:\n{sdp}"
    );
    assert!(
        sdp.contains("G722/8000"),
        "G.722 must be offered to the PBX (RFC 3551 numbers its rtpmap 8000 \
         even though the codec is 16 kHz):\n{sdp}"
    );
    assert_eq!(
        sdp.lines().filter(|l| l.starts_with("m=")).count(),
        1,
        "exactly one media section — a second one renumbers payload types and \
         our SDP answer never echoes it back:\n{sdp}"
    );
}
