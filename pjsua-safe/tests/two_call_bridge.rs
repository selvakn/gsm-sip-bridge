//! Exercises `Endpoint::pair_calls`/`unpair_call` (specs/011-vowifi-sip-bridge,
//! Foundational T010/T013) against a real linked PJSIP, using real
//! `pjsua_call_id`s obtained from real `pjsua_call_make_call` invocations.
//!
//! What this test DOES verify, against the real library: the pairing
//! bookkeeping (`BRIDGE_PAIRS` insert/remove, both directions) survives a
//! full call lifecycle (make → pair → unpair → hangup) without panicking or
//! corrupting PJSUA state, using call IDs PJSIP itself assigned.
//!
//! What this test does NOT verify: two calls actually reaching
//! `PJSUA_CALL_MEDIA_ACTIVE` and exchanging real RTP audio through
//! `on_call_media_state_cb`'s pairing branch. Both calls here dial a
//! loopback destination nothing answers, so they never leave the
//! `Calling`/`Early` state — reaching a real answered call requires either a
//! live SIP registrar/PBX or a second peer process, which is exactly the
//! manual/hardware-gated verification `specs/011-vowifi-sip-bridge/quickstart.md`
//! already calls for (see also `tasks.md` T033/T043). Only one `Endpoint` is
//! created here (not two) since PJSUA's `pjsua_create`/`pjsua_destroy` are
//! process-global singletons — the existing `smoke.rs` in this crate already
//! assumes single-endpoint-at-a-time usage per process for the same reason.
#![cfg(feature = "pjsip-linked")]

use pjsua_safe::{Account, AccountConfig, Call, Endpoint, EndpointConfig, TransportType};

fn ep_config(local_port: u16) -> EndpointConfig {
    EndpointConfig {
        transport: TransportType::Udp,
        local_port,
        tls_verify: false,
        // 16 kHz: the wideband conference bridge Agent B runs, so this
        // exercises the same media config the real VoWiFi bridge uses.
        clock_rate: 16000,
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
fn pair_and_unpair_two_real_calls_without_panicking() {
    let ep = Endpoint::create(ep_config(15070)).expect("endpoint create");
    ep.set_null_sound_device().expect("null sound device");

    // A loopback account: pjsua_acc_add succeeds synchronously and returns an
    // account_id usable for pjsua_call_make_call regardless of whether the
    // background REGISTER it kicks off ever completes (nothing is listening
    // on this port to answer it).
    let acc = Account::register(
        &ep,
        AccountConfig {
            sip_server: "127.0.0.1".into(),
            sip_port: 15071,
            username: "test-a".into(),
            password: "pass".into(),
            display_name: "Test A".into(),
        },
        None,
    )
    .expect("account add");

    let mut call_a = Call::make(&acc, "sip:peer-a@127.0.0.1:15072", None, &[]).expect("call a");
    let mut call_b = Call::make(&acc, "sip:peer-b@127.0.0.1:15073", None, &[]).expect("call b");

    // Real call IDs assigned by PJSUA, not synthetic values.
    let id_a = call_a.call_id();
    let id_b = call_b.call_id();
    assert_ne!(id_a, id_b, "PJSUA must assign distinct call IDs");

    // Pairing before media is active is the documented, expected sequencing
    // (Agent B pairs its two legs as soon as both calls are placed, before
    // either necessarily has active media yet).
    ep.pair_calls(id_a, id_b);

    // Idempotent / order-independent unpairing: removing either side must
    // clear both directions and not panic on a call that was never paired.
    ep.unpair_call(id_a);
    ep.unpair_call(id_a); // second removal of the same (now-absent) id: no-op, no panic
    ep.unpair_call(999_999); // an id that was never paired: no-op, no panic

    call_a.hangup().expect("hangup a");
    call_b.hangup().expect("hangup b");
}
