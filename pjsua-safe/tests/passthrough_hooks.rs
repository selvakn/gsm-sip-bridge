//! The passthrough hooks (specs/120-amr-passthrough-forwarder) against a real
//! linked PJSIP and real answered calls.
//!
//! PJSUA is a process-wide singleton, so the two peers the call needs are
//! *child processes* of this very test binary (`peer_entry` below, selected by
//! the `PT_ROLE` environment variable), each running its own PJSUA. The parent
//! plays Agent B: it places a client-leg call and a veth-leg call, upgrades the
//! client leg onto a forwarder socket by re-INVITE, and checks what the
//! hooks are specified to do:
//!
//! - no entry → the normal path (PJSIP's own stream carries audio);
//! - a forward entry → the re-INVITE advertises the forwarder, the client's
//!   audio lands on the forwarder socket, and PJSIP's own stream is paused
//!   (sent 0 / received 0);
//! - the veth leg registered quiet sends nothing;
//! - hold/resume re-creates the stream and it comes back paused;
//! - a re-INVITE that drops the forwarded codec is answered 488, and so is one
//!   the application's offer check rejects while keeping the codec;
//! - clearing the entry and re-INVITEing restores PJSIP's own transport;
//! - nothing is left behind after hangup (registry, sockets).
//!
//! Needs a PJSIP-linked build. Codec-agnostic: it forwards PCMU, since the
//! hooks do not care what the payload is.
#![cfg(feature = "pjsip-linked")]

use pjsua_safe::{passthrough, Account, Call, CallState, Endpoint, EndpointConfig, TransportType};
use std::io::{BufRead, BufReader, Write};
use std::net::{SocketAddr, UdpSocket};
use std::process::{Child, ChildStdin, Command, Stdio};
use std::sync::mpsc;
use std::time::{Duration, Instant};

const B_PORT: u16 = 15080;
const Y_PORT: u16 = 15081;
const Z_PORT: u16 = 15082;

fn ep_config(local_port: u16) -> EndpointConfig {
    EndpointConfig {
        transport: TransportType::Udp,
        local_port,
        tls_verify: false,
        clock_rate: 16000,
        jb_init_ms: 20,
        jb_min_pre: 1,
        jb_max_ms: 40,
        vad_enabled: false,
        tx_level: 1.0,
        snd_rec_latency_ms: 150,
        snd_play_latency_ms: 150,
        public_addr: None,
    }
}

fn only_codec(ep: &Endpoint, keep: &str) {
    for c in ep.codecs() {
        let _ = ep.set_codec_priority(&c.id, if c.id.starts_with(keep) { 255 } else { 0 });
    }
}

fn wait_until(what: &str, secs: u64, mut f: impl FnMut() -> bool) {
    let deadline = Instant::now() + Duration::from_secs(secs);
    while Instant::now() < deadline {
        if f() {
            return;
        }
        std::thread::sleep(Duration::from_millis(50));
    }
    panic!("timed out waiting for {what}");
}

macro_rules! step {
    ($($t:tt)*) => { eprintln!("STEP| {}", format!($($t)*)) };
}

fn socket_fds() -> usize {
    use std::os::unix::fs::FileTypeExt;
    std::fs::read_dir("/proc/self/fd")
        .unwrap()
        .filter_map(|e| std::fs::metadata(e.ok()?.path()).ok())
        .filter(|m| m.file_type().is_socket())
        .count()
}

// ---------------------------------------------------------------------------
// The peers

/// Entry point of a peer process. A no-op in the parent test run.
#[test]
fn peer_entry() {
    let Ok(role) = std::env::var("PT_ROLE") else {
        return;
    };
    let port: u16 = std::env::var("PT_PORT").unwrap().parse().unwrap();
    let ep = Endpoint::create(ep_config(port)).expect("peer endpoint");
    ep.set_null_sound_device().unwrap();
    only_codec(&ep, "PCMU");
    let _acc = Account::local(&ep, &format!("sip:{role}@127.0.0.1:{port}"), &role).unwrap();

    let (tx, rx) = mpsc::channel::<String>();
    std::thread::spawn(move || {
        for line in std::io::stdin().lock().lines().map_while(Result::ok) {
            if tx.send(line).is_err() {
                break;
            }
        }
    });

    println!("PEER|ready");
    let mut call: Option<Call> = None;
    let mut last_view = String::new();
    loop {
        if call.is_none() {
            if let Some((_, call_id, _)) = ep.poll_incoming_call() {
                let mut c = Call::from_id(call_id, CallState::Incoming);
                c.answer(200).unwrap();
                call = Some(c);
            }
        }
        if let Some(c) = &call {
            let view = match ep.client_media(c.call_id()) {
                Some(m) => format!(
                    "media remote={} send={} has={}",
                    m.remote_rtp.map(|a| a.to_string()).unwrap_or_default(),
                    m.send_to_client,
                    m.has_media
                ),
                None => "media none".to_string(),
            };
            if view != last_view {
                println!("PEER|{view}");
                last_view = view;
            }
        }
        match rx.recv_timeout(Duration::from_millis(100)) {
            Ok(cmd) => {
                let id = call.as_ref().map(Call::call_id);
                match (cmd.as_str(), id) {
                    ("hold", Some(id)) => println!("PEER|hold -> {:?}", ep.hold_call(id).is_ok()),
                    ("unhold", Some(id)) => {
                        println!("PEER|unhold -> {:?}", ep.unhold_call(id).is_ok())
                    }
                    ("restore-pcmu", Some(_)) => {
                        only_codec(&ep, "PCMU");
                        println!("PEER|restored");
                    }
                    ("reinvite-same", Some(id)) => {
                        println!("PEER|reinvite -> {:?}", ep.reinvite_plain(id).is_ok());
                    }
                    ("reinvite-g722", Some(id)) => {
                        only_codec(&ep, "G722");
                        println!("PEER|reinvite -> {:?}", ep.reinvite_plain(id).is_ok());
                    }
                    ("stats", Some(id)) => println!("PEER|stats {:?}", ep.stream_packets(id)),
                    ("state", Some(_)) => println!(
                        "PEER|state {:?}",
                        call.as_ref().map(Call::poll_state).unwrap()
                    ),
                    ("quit", _) => break,
                    _ => {}
                }
            }
            Err(mpsc::RecvTimeoutError::Disconnected) => break,
            Err(mpsc::RecvTimeoutError::Timeout) => {}
        }
    }
}

struct Peer {
    child: Child,
    stdin: ChildStdin,
    lines: mpsc::Receiver<String>,
    seen: Vec<String>,
}

impl Peer {
    fn spawn(role: &str, port: u16) -> Self {
        let mut child = Command::new(std::env::current_exe().unwrap())
            .args(["--exact", "peer_entry", "--nocapture", "--test-threads=1"])
            .env("PT_ROLE", role)
            .env("PT_PORT", port.to_string())
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::null())
            .spawn()
            .expect("spawn peer");
        let stdin = child.stdin.take().unwrap();
        let out = child.stdout.take().unwrap();
        let (tx, lines) = mpsc::channel();
        std::thread::spawn(move || {
            for line in BufReader::new(out).lines().map_while(Result::ok) {
                // libtest prefixes the first line with "test peer_entry ... ".
                if let Some((_, rest)) = line.split_once("PEER|") {
                    if tx.send(rest.to_string()).is_err() {
                        break;
                    }
                }
            }
        });
        Self {
            child,
            stdin,
            lines,
            seen: Vec::new(),
        }
    }

    fn send(&mut self, cmd: &str) {
        writeln!(self.stdin, "{cmd}").unwrap();
        self.stdin.flush().unwrap();
    }

    /// Wait for a peer line starting with `prefix` that arrives after this call.
    fn expect(&mut self, prefix: &str, secs: u64) -> String {
        let deadline = Instant::now() + Duration::from_secs(secs);
        while Instant::now() < deadline {
            if let Ok(line) = self.lines.recv_timeout(Duration::from_millis(100)) {
                self.seen.push(line.clone());
                if line.starts_with(prefix) {
                    return line;
                }
            }
        }
        panic!("peer never said {prefix:?}; saw {:?}", self.seen);
    }

    /// Wait for a media view line matching `pred`.
    fn expect_media(&mut self, secs: u64, pred: impl Fn(&str) -> bool) -> String {
        let deadline = Instant::now() + Duration::from_secs(secs);
        while Instant::now() < deadline {
            if let Ok(line) = self.lines.recv_timeout(Duration::from_millis(100)) {
                self.seen.push(line.clone());
                if line.starts_with("media ") && pred(&line) {
                    return line;
                }
            }
        }
        panic!(
            "peer never reported the expected media view; saw {:?}",
            self.seen
        );
    }

    fn quit(mut self) {
        self.send("quit");
        let _ = self.child.wait();
    }
}

/// A panicking test must not leave its peers holding the ports.
impl Drop for Peer {
    fn drop(&mut self) {
        let _ = self.child.kill();
        let _ = self.child.wait();
    }
}

fn remote_of(view: &str) -> Option<SocketAddr> {
    view.split("remote=")
        .nth(1)?
        .split(' ')
        .next()?
        .parse()
        .ok()
}

// ---------------------------------------------------------------------------
// Agent B

#[test]
fn the_client_leg_can_be_upgraded_onto_a_forwarder_and_back() {
    if std::env::var("PT_ROLE").is_ok() {
        return;
    }
    let ep = Endpoint::create(ep_config(B_PORT)).expect("endpoint");
    ep.set_null_sound_device().unwrap();
    only_codec(&ep, "PCMU");
    let acc = Account::local(&ep, &format!("sip:b@127.0.0.1:{B_PORT}"), "B").unwrap();
    let sockets_before = socket_fds();

    let mut y = Peer::spawn("y", Y_PORT);
    let mut z = Peer::spawn("z", Z_PORT);
    y.expect("ready", 20);
    z.expect("ready", 20);

    let mut y_call = Call::make(&acc, &format!("sip:y@127.0.0.1:{Y_PORT}"), None, &[]).unwrap();
    let mut z_call = Call::make(&acc, &format!("sip:z@127.0.0.1:{Z_PORT}"), None, &[]).unwrap();
    let (yid, zid) = (y_call.call_id(), z_call.call_id());
    wait_until("both calls confirmed", 10, || {
        y_call.poll_state() == CallState::Confirmed && z_call.poll_state() == CallState::Confirmed
    });
    wait_until("client media", 10, || {
        ep.client_media(yid)
            .is_some_and(|m| m.has_media && m.remote_rtp.is_some())
    });

    step!("normal path");
    // --- no entry: today's path ---------------------------------------
    let normal = ep.client_media(yid).unwrap();
    assert_eq!(normal.encoding_name, "PCMU");
    assert!(normal.send_to_client);
    assert!(!passthrough::is_registered(yid));
    let normal_view = y.expect_media(10, |l| remote_of(l).is_some());
    let normal_remote = remote_of(&normal_view).unwrap();
    wait_until("PJSIP's own stream to carry audio", 10, || {
        ep.stream_packets(yid).is_some_and(|(sent, _)| sent > 0)
    });

    step!("upgrade");
    // --- upgrade ---------------------------------------------------------
    // Bound on every address: the hook advertises the address PJSIP would have.
    let fwd = UdpSocket::bind("0.0.0.0:0").unwrap();
    fwd.set_read_timeout(Some(Duration::from_millis(200)))
        .unwrap();
    let fwd_port = fwd.local_addr().unwrap().port();
    ep.register_forward(yid, fwd_port, "PCMU", 8000);
    ep.register_quiet(zid);
    ep.disconnect_from_conference(zid);
    let generation = ep.media_generation(yid);
    ep.reinvite_reinit_media(yid).expect("re-INVITE");
    wait_until("the client leg's stream to be re-created", 10, || {
        ep.media_generation(yid) > generation
    });

    // The client was told to send to the forwarder, not to PJSIP's own port.
    let view = y.expect_media(10, |l| remote_of(l).is_some_and(|a| a.port() == fwd_port));
    assert_eq!(remote_of(&view).unwrap().port(), fwd_port, "{view}");
    assert_ne!(remote_of(&view).unwrap().port(), normal_remote.port());

    // Its audio arrives on the forwarder socket...
    let mut buf = [0u8; 512];
    let mut got = 0;
    let deadline = Instant::now() + Duration::from_secs(5);
    while Instant::now() < deadline && got < 5 {
        if fwd.recv(&mut buf).is_ok() {
            got += 1;
        }
    }
    if got < 5 {
        y.send("stats");
        let stats = y.expect("stats", 5);
        panic!("the client's RTP should reach the forwarder socket, got {got}; client view: {stats}; saw {:?}", y.seen);
    }

    // ...and PJSIP's own stream carries nothing: paused, sent 0 / received 0.
    // The veth leg was already running (it sent a few frames before it was
    // paused), so what counts is that it sends no more.
    std::thread::sleep(Duration::from_millis(600));
    assert_eq!(
        ep.stream_packets(yid),
        Some((0, 0)),
        "client stream must be paused"
    );
    let z_sent = ep.stream_packets(zid).map(|(s, _)| s);
    std::thread::sleep(Duration::from_secs(1));
    assert_eq!(
        ep.stream_packets(zid).map(|(s, _)| s),
        z_sent,
        "the quiet veth leg must send nothing"
    );
    assert_eq!(
        z_call.poll_state(),
        CallState::Confirmed,
        "the quiet veth call stays up"
    );

    step!("hold/resume");
    // --- hold / resume: the stream is re-created and re-paused ------------
    let generation = ep.media_generation(yid);
    y.send("hold");
    wait_until("hold to re-create the stream", 10, || {
        ep.media_generation(yid) > generation
    });
    wait_until("the client leg to read as not sendable", 10, || {
        ep.client_media(yid).is_some_and(|m| !m.send_to_client)
    });
    assert_eq!(ep.stream_packets(yid), Some((0, 0)));
    let generation = ep.media_generation(yid);
    y.send("unhold");
    wait_until("resume to re-create the stream", 10, || {
        ep.media_generation(yid) > generation
    });
    wait_until("the client leg to read as sendable again", 10, || {
        ep.client_media(yid).is_some_and(|m| m.send_to_client)
    });
    assert_eq!(
        ep.stream_packets(yid),
        Some((0, 0)),
        "re-paused after resume"
    );
    assert!(
        passthrough::is_registered(yid),
        "hold/resume keeps the entry"
    );

    step!("rejected by the offer check");
    // --- an offer that keeps the codec but fails the application's check ---
    let seen: std::sync::Arc<std::sync::Mutex<Vec<passthrough::OfferedAudio>>> =
        std::sync::Arc::default();
    let seen_by_check = seen.clone();
    ep.register_offer_check(
        yid,
        std::sync::Arc::new(move |audio| {
            seen_by_check.lock().unwrap().push(audio.clone());
            Err("test refuses".into())
        }),
    );
    let generation = ep.media_generation(yid);
    y.send("reinvite-same");
    y.expect("reinvite -> true", 10);
    std::thread::sleep(Duration::from_secs(2));
    assert_eq!(
        ep.media_generation(yid),
        generation,
        "an offer the check rejects must not touch the media"
    );
    {
        let seen = seen.lock().unwrap();
        assert!(!seen.is_empty(), "the check saw the offer");
        assert!(
            seen[0]
                .codecs
                .iter()
                .any(|c| c.encoding_name.eq_ignore_ascii_case("PCMU") && c.clock_rate == 8000),
            "{:?}",
            seen[0]
        );
    }
    y.send("state");
    assert_eq!(y.expect("state", 5), "state Confirmed");

    step!("incompatible");
    // --- an incompatible re-INVITE is declined; the call carries on -------
    let generation = ep.media_generation(yid);
    y.send("reinvite-g722");
    step!("sent reinvite-g722");
    y.expect("reinvite -> true", 10);
    step!("peer re-INVITEd");
    std::thread::sleep(Duration::from_secs(2));
    assert_eq!(
        ep.media_generation(yid),
        generation,
        "a declined offer must not touch the media"
    );
    assert_eq!(ep.client_media(yid).unwrap().encoding_name, "PCMU");
    step!("checking state");
    y.send("state");
    let state = y.expect("state", 5);
    assert_eq!(state, "state Confirmed", "{state}");
    step!("peer state ok");
    // The forwarder still receives the client's audio.
    let mut still = 0;
    for _ in 0..50 {
        if fwd.recv(&mut buf).is_ok() {
            still += 1;
        }
    }
    assert!(
        still >= 5,
        "audio still arrives after the declined re-INVITE, got {still}"
    );

    step!("downgrade");
    // --- downgrade: clear the entry, re-INVITE, PJSIP's transport is back --
    y.send("restore-pcmu"); // the peer's own codec change is not part of the downgrade
    y.expect("restored", 5);
    ep.clear_forward(yid);
    let generation = ep.media_generation(yid);
    ep.reinvite_reinit_media(yid).expect("downgrade re-INVITE");
    wait_until("the downgrade to re-create the stream", 10, || {
        ep.media_generation(yid) > generation
    });
    let view = y.expect_media(10, |l| remote_of(l).is_some_and(|a| a.port() != fwd_port));
    assert_ne!(remote_of(&view).unwrap().port(), fwd_port, "{view}");
    assert!(ep.client_media(yid).unwrap().send_to_client);

    step!("teardown");
    // --- teardown leaves nothing behind -------------------------------------
    ep.register_forward(yid, fwd_port, "PCMU", 8000); // a stale entry at hangup
    drop(fwd);
    y_call.hangup().unwrap();
    z_call.hangup().unwrap();
    wait_until("both calls to disconnect", 10, || {
        y_call.poll_state() == CallState::Disconnected
            && z_call.poll_state() == CallState::Disconnected
    });
    wait_until("the registry to be cleared by the disconnect", 5, || {
        !passthrough::is_registered(yid) && !passthrough::is_registered(zid)
    });
    assert_eq!(passthrough::media_generation(yid), 0);
    wait_until("PJSIP's sockets to be released", 10, || {
        socket_fds() <= sockets_before
    });

    y.quit();
    z.quit();
}
