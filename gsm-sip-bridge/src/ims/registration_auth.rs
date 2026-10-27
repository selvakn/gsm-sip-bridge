//! Answering a network-requested AKA challenge without holding up signalling
//! (TS 33.203 §7.4, TS 24.229 §5.1.1.5.1).
//!
//! The SIM can take seconds to answer, and on a modem it shares a serial line
//! with other work, so the AUTHENTICATE runs on its own thread. The thread does
//! nothing but the SIM: it never touches a socket, the kernel's XFRM tables or
//! the registration state. The dispatch loop starts it, polls for its result on
//! each pass and does everything else itself, which keeps one reader on the Gm
//! connection and one owner of the registration.

use crate::error::{BridgeError, BridgeResult};
use crate::ims::sip_client::{extract_challenge, parse_digest_challenge, DigestChallenge};
use crate::ims::{
    build_authorization, build_resync_authorization, run_aka, ImsRegisterConfig, OpenedCard,
};
use crate::modules::usim::AkaResult;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::mpsc::{self, Receiver, TryRecvError};
use std::sync::Arc;
use std::time::{Duration, Instant};

/// How long a challenge may wait for the SIM before the attempt is abandoned
/// and the registration falls back to a full one. Generous: a modem that is
/// busy with something else can keep the serial line for a while.
pub(crate) const WORK_TIMEOUT: Duration = Duration::from_secs(600);

/// What the thread is asked to answer.
pub(crate) struct ChallengeJob {
    pub(crate) cfg: ImsRegisterConfig,
    pub(crate) realm: String,
    /// The `uri` of the `Authorization` header: the Request-URI of the REGISTER
    /// it will be attached to.
    pub(crate) uri: String,
    /// The `WWW-Authenticate` header value of the 401.
    pub(crate) www_authenticate: String,
}

/// The SIM's answer, ready to put on the wire. Holds key material, so it has no
/// `Debug` and is never logged.
pub(crate) enum Answer {
    /// The `Authorization` that carries RES, and the keys for the new SAs.
    Authenticated {
        authorization: String,
        ik: Vec<u8>,
        ck: Vec<u8>,
    },
    /// The SIM's sequence number is out of step with the network's: the
    /// `Authorization` carries AUTS so the network can resynchronise
    /// (RFC 3310 §3.3), and answers with a fresh challenge.
    Resynchronise { authorization: String },
}

/// A running (or finished) answer to one challenge.
pub(crate) struct Reauthentication {
    result: Receiver<BridgeResult<Answer>>,
    cancelled: Arc<AtomicBool>,
    started: Instant,
}

impl Reauthentication {
    /// Starts answering `job` on a thread of its own. `open` yields the SIM; in
    /// production it is [`open_card`] (through the agent's `ReauthEnv`), a test
    /// supplies one that does not need hardware.
    pub(crate) fn spawn<F>(job: ChallengeJob, open: F) -> Self
    where
        F: FnOnce(&ImsRegisterConfig) -> BridgeResult<OpenedCard> + Send + 'static,
    {
        let (tx, result) = mpsc::channel();
        let cancelled = Arc::new(AtomicBool::new(false));
        let flag = Arc::clone(&cancelled);
        std::thread::spawn(move || {
            // A receiver that is gone means the attempt was abandoned; there is
            // nobody to tell.
            let _ = tx.send(answer(&job, open, &flag));
        });
        Self {
            result,
            cancelled,
            started: Instant::now(),
        }
    }

    /// The answer if the thread has finished, without waiting for it.
    pub(crate) fn poll(&self) -> Option<BridgeResult<Answer>> {
        match self.result.try_recv() {
            Ok(answer) => Some(answer),
            Err(TryRecvError::Empty) => None,
            Err(TryRecvError::Disconnected) => Some(Err(BridgeError::Ims(
                "the re-authentication thread ended without an answer".into(),
            ))),
        }
    }

    /// Whether the SIM has had longer than [`WORK_TIMEOUT`].
    pub(crate) fn overdue(&self, now: Instant) -> bool {
        now.duration_since(self.started) >= WORK_TIMEOUT
    }

    /// Abandons the attempt. A thread already inside the SIM finishes that call;
    /// its answer is dropped unread.
    pub(crate) fn cancel(&self) {
        self.cancelled.store(true, Ordering::SeqCst);
    }
}

impl Drop for Reauthentication {
    fn drop(&mut self) {
        self.cancel();
    }
}

fn answer<F>(job: &ChallengeJob, open: F, cancelled: &AtomicBool) -> BridgeResult<Answer>
where
    F: FnOnce(&ImsRegisterConfig) -> BridgeResult<OpenedCard>,
{
    let params = parse_digest_challenge(&job.www_authenticate)?;
    let challenge = extract_challenge(&params)?;
    let OpenedCard { mut card, imsi, .. } = open(&job.cfg)?;
    if cancelled.load(Ordering::SeqCst) {
        return Err(BridgeError::Ims(
            "the re-authentication was abandoned".into(),
        ));
    }
    answer_challenge(card.as_mut(), &imsi, job, &challenge)
}

/// The Authorization for `challenge`, from the SIM's AKA run.
fn answer_challenge(
    card: &mut dyn crate::modules::usim::ApduTransport,
    imsi: &str,
    job: &ChallengeJob,
    challenge: &DigestChallenge,
) -> BridgeResult<Answer> {
    // The private identity is always IMSI-based (TS 33.203), whatever public
    // identity the registration uses.
    let impi_uri = format!("{imsi}@{}", job.realm);
    match run_aka(card, challenge)? {
        AkaResult::Success { res, ck, ik } => Ok(Answer::Authenticated {
            authorization: build_authorization(&impi_uri, challenge, &job.uri, &res),
            ik,
            ck,
        }),
        AkaResult::SyncFailure { auts } => Ok(Answer::Resynchronise {
            authorization: build_resync_authorization(&impi_uri, challenge, &job.uri, &auts),
        }),
    }
}

/// Stand-ins for the hardware, which CI does not have: a SIM that answers from
/// a script. The rest of the path (parsing the challenge, building the
/// Authorization) is the real code.
#[cfg(test)]
pub(crate) mod scripted {
    use super::*;
    use crate::modules::usim::ApduTransport;
    use std::sync::Mutex;

    /// A SIM that answers every AUTHENTICATE with a fixed outcome, after an
    /// optional delay. Stands in for the hardware, which CI does not have; the
    /// rest of the path (parsing the challenge, building the Authorization) is
    /// the real code.
    pub(crate) struct Sim {
        /// Answers in order; the last one repeats.
        pub(crate) replies: Arc<Mutex<Vec<String>>>,
        pub(crate) delay: Duration,
    }

    impl ApduTransport for Sim {
        fn transmit_apdu(&mut self, _apdu: &str) -> BridgeResult<String> {
            std::thread::sleep(self.delay);
            let mut replies = self.replies.lock().unwrap();
            let reply = if replies.len() > 1 {
                replies.remove(0)
            } else {
                replies[0].clone()
            };
            Ok(reply)
        }
    }

    /// `DB` + len + RES(8) + len + CK(16) + len + IK(16) + `9000`.
    pub(crate) fn success_reply() -> String {
        format!(
            "DB08{}10{}10{}9000",
            "11".repeat(8),
            "22".repeat(16),
            "33".repeat(16)
        )
    }

    pub(crate) fn challenge_header() -> String {
        // RAND || AUTN, 32 bytes.
        let nonce = base64::Engine::encode(&base64::engine::general_purpose::STANDARD, [7u8; 32]);
        format!(
            "Digest realm=\"ims.example\", nonce=\"{nonce}\", algorithm=AKAv1-MD5, qop=\"auth\""
        )
    }

    /// `DC` + len + AUTS(14): a sequence mismatch.
    pub(crate) fn sync_failure_reply() -> String {
        format!("DC0E{}9000", "AA".repeat(14))
    }

    /// Opens "the card" for [`Reauthentication::spawn`] and the agent's
    /// [`ReauthEnv`](crate::ims::agent): a SIM that gives `replies` in turn.
    pub(crate) fn card(
        replies: Vec<String>,
        delay: Duration,
    ) -> impl Fn(&ImsRegisterConfig) -> BridgeResult<OpenedCard> + Send + Sync + 'static {
        // One script across every opening: a SIM does not forget how far it
        // got when the line closes and reopens it.
        let script = Arc::new(Mutex::new(replies));
        move |_| {
            Ok(OpenedCard {
                card: Box::new(Sim {
                    replies: Arc::clone(&script),
                    delay,
                }),
                imsi: "001010000000001".into(),
                imei: "350000000000001".into(),
            })
        }
    }
}

#[cfg(test)]
mod tests {
    use super::scripted::*;
    use super::*;
    use crate::modules::usim::ApduTransport;

    fn job() -> ChallengeJob {
        ChallengeJob {
            cfg: crate::ims::test_register_config(),
            realm: "ims.example".into(),
            uri: "sip:ims.example".into(),
            www_authenticate: challenge_header(),
        }
    }

    fn wait(work: &Reauthentication) -> BridgeResult<Answer> {
        let until = Instant::now() + Duration::from_secs(5);
        loop {
            if let Some(answer) = work.poll() {
                return answer;
            }
            assert!(Instant::now() < until, "the worker never answered");
            std::thread::sleep(Duration::from_millis(5));
        }
    }

    #[test]
    fn a_challenge_is_answered_with_the_sims_response_and_the_keys() {
        let work = Reauthentication::spawn(job(), card(vec![success_reply()], Duration::ZERO));
        let Ok(Answer::Authenticated {
            authorization,
            ik,
            ck,
        }) = wait(&work)
        else {
            panic!("expected an authenticated answer");
        };
        assert!(authorization.starts_with("Digest username=\"001010000000001@ims.example\""));
        assert!(authorization.contains("uri=\"sip:ims.example\""));
        assert!(authorization.contains("algorithm=AKAv1-MD5"));
        assert_eq!(ik, vec![0x33; 16]);
        assert_eq!(ck, vec![0x22; 16]);
    }

    #[test]
    fn a_sequence_mismatch_asks_the_network_to_resynchronise() {
        let work = Reauthentication::spawn(job(), card(vec![sync_failure_reply()], Duration::ZERO));
        let Ok(Answer::Resynchronise { authorization }) = wait(&work) else {
            panic!("expected a resynchronisation");
        };
        assert!(authorization.contains("auts=\""));
    }

    #[test]
    fn the_caller_is_not_held_up_while_the_sim_works() {
        let work = Reauthentication::spawn(
            job(),
            card(vec![success_reply()], Duration::from_millis(300)),
        );
        let started = Instant::now();
        assert!(work.poll().is_none(), "the SIM has not answered yet");
        assert!(
            started.elapsed() < Duration::from_millis(100),
            "poll must not block"
        );
        assert!(wait(&work).is_ok());
    }

    #[test]
    fn a_card_that_cannot_be_opened_is_an_error_not_a_hang() {
        let work = Reauthentication::spawn(job(), |_| Err(BridgeError::Ims("no modem".into())));
        assert!(wait(&work).is_err());
    }

    #[test]
    fn a_malformed_challenge_is_refused_before_the_sim_is_touched() {
        let mut bad = job();
        bad.www_authenticate = "Basic realm=\"x\"".into();
        let work = Reauthentication::spawn(bad, |_| panic!("the SIM must not be opened"));
        assert!(wait(&work).is_err());
    }

    #[test]
    fn an_abandoned_attempt_does_not_run_the_sim() {
        let (gate_tx, gate_rx) = mpsc::channel::<()>();
        let ran = Arc::new(AtomicBool::new(false));
        let saw = Arc::clone(&ran);
        let work = Reauthentication::spawn(job(), move |_| {
            // Held until the test has abandoned the attempt.
            let _ = gate_rx.recv();
            Ok(OpenedCard {
                card: Box::new(Probe(saw)),
                imsi: "001010000000001".into(),
                imei: "350000000000001".into(),
            })
        });
        work.cancel();
        gate_tx.send(()).unwrap();
        assert!(wait(&work).is_err());
        assert!(!ran.load(Ordering::SeqCst));

        struct Probe(Arc<AtomicBool>);
        impl ApduTransport for Probe {
            fn transmit_apdu(&mut self, _: &str) -> BridgeResult<String> {
                self.0.store(true, Ordering::SeqCst);
                Ok(String::new())
            }
        }
    }

    #[test]
    fn work_is_overdue_after_the_timeout() {
        let work = Reauthentication::spawn(job(), card(vec![success_reply()], Duration::ZERO));
        assert!(!work.overdue(Instant::now()));
        assert!(work.overdue(Instant::now() + WORK_TIMEOUT));
    }
}
