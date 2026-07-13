//! What codecs this PJSIP build actually ships with, and that their priorities
//! can be steered — the assumption the VoWiFi bridge's wideband path rests on
//! (`gsm-sip-bridge/src/vowifi`: G.722 to the PBX, L16/16000 over the veth).
//! Needs a real linked PJSIP, so it is `pjsip-linked`-gated like the rest.
#![cfg(feature = "pjsip-linked")]

use pjsua_safe::{Endpoint, EndpointConfig, TransportType};

fn ep_config(local_port: u16, clock_rate: u32) -> EndpointConfig {
    EndpointConfig {
        transport: TransportType::Udp,
        local_port,
        tls_verify: false,
        clock_rate,
        jb_init_ms: 20,
        jb_min_pre: 1,
        jb_max_ms: 40,
        vad_enabled: true,
        tx_level: 1.0,
        snd_rec_latency_ms: 150,
        snd_play_latency_ms: 150,
    }
}

#[test]
fn wideband_codecs_are_present_and_prioritizable() {
    let ep = Endpoint::create(ep_config(15070, 16000)).expect("endpoint");
    let ids: Vec<String> = ep.codecs().iter().map(|c| c.id.clone()).collect();
    println!("codecs: {ids:?}");

    for id in ["G722/16000/1", "L16/16000/1", "PCMU/8000/1"] {
        assert!(ids.iter().any(|c| c == id), "missing {id} in {ids:?}");
    }

    ep.set_codec_priority("G722/16000/1", 200).expect("G722");
    ep.set_codec_priority("L16/16000/1", 1).expect("L16");

    let by_id = |id: &str| ep.codecs().into_iter().find(|c| c.id == id).unwrap();
    assert_eq!(by_id("G722/16000/1").priority, 200);
    assert_eq!(by_id("L16/16000/1").priority, 1);
    assert!(
        by_id("G722/16000/1").priority > by_id("PCMU/8000/1").priority,
        "G.722 must outrank PCMU so the PBX leg offers it first"
    );
}
