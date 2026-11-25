//! Answering a network-requested AKA challenge without holding up signalling
//! (TS 33.203 §7.4, TS 24.229 §5.1.1.5.1).
//!
//! The SIM can take seconds to answer, and on a modem it shares a serial line
//! with other work, so the AUTHENTICATE runs on its own thread. The thread does
//! nothing but the SIM: it never touches a socket, the kernel's XFRM tables or
//! the registration state. The dispatch loop starts it, polls for its result on
//! each pass and does everything else itself, which keeps one reader on the Gm
//! connection and one owner of the registration.
//!
//! A card-reader line also checks, on the same kind of thread, whether a SIM
//! it lost touch with is back ([`CardCheck`], #233).

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

/// How long a modem line's challenge may wait for the SIM before the attempt
/// is abandoned and the registration falls back to a full one. Generous: a
/// modem that is busy with something else can keep the serial line for a
/// while. A card-reader line's challenge has its own, shorter deadline
/// ([`CardTiming`]).
pub(crate) const WORK_TIMEOUT: Duration = Duration::from_secs(600);

/// How long a card-reader line keeps putting the same challenge to a SIM it
/// cannot reach (#233, research D2). Well inside the network's wait for the
/// answer (the registrar's reg-await-auth timer, TS 24.229 §5.4.1.2.1), so an
/// answer found late in the window is still accepted.
pub(crate) const CARD_RETRY_WINDOW: Duration = Duration::from_secs(30);

/// The pause between two attempts at an unreachable card (research D2): long
/// enough for pcscd to notice a reset and let us reconnect, short enough that
/// a brief outage costs a few seconds.
pub(crate) const CARD_RETRY_INTERVAL: Duration = Duration::from_secs(2);

/// How long one card operation may take before the line stops waiting for it
/// (research D2, D4): the slack past [`CARD_RETRY_WINDOW`] before a challenge
/// that is stuck inside the card is abandoned, and the bound on one local
/// check while waiting for the card. A healthy card answers in well under a
/// second.
pub(crate) const CARD_ATTEMPT_BOUND: Duration = Duration::from_secs(15);

/// The longest pause between two local checks of a card that is out of reach
/// (research D4). The checks touch only the card, so a minute keeps a returning
/// card's wait short without polling the reader constantly.
pub(crate) const CARD_CHECK_MAX_BACKOFF: Duration = Duration::from_secs(60);

/// How a card-reader line deals with a SIM it cannot reach (#233). Held by the
/// agent's `ReauthEnv`; the tests shorten it. Not operator-configurable.
#[derive(Clone, Debug)]
pub(crate) struct CardTiming {
    /// How long the same challenge is put to the card again.
    pub(crate) retry_window: Duration,
    /// The pause between attempts.
    pub(crate) retry_interval: Duration,
    /// The slack past the window, and the bound on one local check.
    pub(crate) attempt_bound: Duration,
    /// The first local check after the line starts waiting for the card.
    pub(crate) check_initial: Duration,
    /// The cap on the doubling pause between local checks.
    pub(crate) check_max: Duration,
}

impl Default for CardTiming {
    fn default() -> Self {
        Self {
            retry_window: CARD_RETRY_WINDOW,
            retry_interval: CARD_RETRY_INTERVAL,
            attempt_bound: CARD_ATTEMPT_BOUND,
            check_initial: crate::ims::agent::RETRY_INITIAL_BACKOFF,
            check_max: CARD_CHECK_MAX_BACKOFF,
        }
    }
}

#[cfg(test)]
impl CardTiming {
    /// Milliseconds instead of seconds, so the tests finish quickly.
    pub(crate) fn fast() -> Self {
        Self {
            retry_window: Duration::from_millis(300),
            retry_interval: Duration::from_millis(20),
            attempt_bound: Duration::from_millis(200),
            check_initial: Duration::from_millis(20),
            check_max: Duration::from_millis(80),
        }
    }
}

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

/// One piece of SIM work running (or finished) on a thread of its own: an
/// answer to a challenge ([`Reauthentication`]) or a check that the card is
/// reachable ([`CardCheck`]).
///
/// The thread does nothing but the SIM. The dispatch loop polls it, and stops
/// waiting for it at its `deadline`. A thread stuck inside the card cannot be
/// interrupted, so an abandoned one keeps running until the card call returns;
/// `done` says when it has, so the line can wait for that before it touches the
/// card again (FR-015).
pub(crate) struct SimWorker<T> {
    result: Receiver<BridgeResult<T>>,
    cancelled: Arc<AtomicBool>,
    /// Set when the thread ends, whether it answered, failed or panicked.
    done: Arc<AtomicBool>,
    deadline: Instant,
}

/// A running (or finished) answer to one challenge.
pub(crate) type Reauthentication = SimWorker<Answer>;

/// A running (or finished) check that a card-reader line's SIM can be reached:
/// opening it reads its IMSI, and nothing more. No AKA, no network traffic.
pub(crate) type CardCheck = SimWorker<()>;

/// Sets its flag when dropped: at the end of the worker thread, including when
/// it unwinds from a panic.
struct SetOnDrop(Arc<AtomicBool>);

impl Drop for SetOnDrop {
    fn drop(&mut self) {
        self.0.store(true, Ordering::SeqCst);
    }
}

impl<T: Send + 'static> SimWorker<T> {
    /// Runs `work` on a thread of its own, to be given up on after `bound`.
    /// `work` is handed the cancellation flag.
    fn start<W>(bound: Duration, work: W) -> Self
    where
        W: FnOnce(&AtomicBool) -> BridgeResult<T> + Send + 'static,
    {
        let (tx, result) = mpsc::channel();
        let cancelled = Arc::new(AtomicBool::new(false));
        let done = Arc::new(AtomicBool::new(false));
        let flag = Arc::clone(&cancelled);
        let finished = SetOnDrop(Arc::clone(&done));
        std::thread::spawn(move || {
            let _finished = finished;
            // A receiver that is gone means the work was abandoned; there is
            // nobody to tell.
            let _ = tx.send(work(&flag));
        });
        Self {
            result,
            cancelled,
            done,
            deadline: Instant::now() + bound,
        }
    }
}

impl<T> SimWorker<T> {
    /// The result if the thread has finished, without waiting for it.
    pub(crate) fn poll(&self) -> Option<BridgeResult<T>> {
        match self.result.try_recv() {
            Ok(answer) => Some(answer),
            Err(TryRecvError::Empty) => None,
            Err(TryRecvError::Disconnected) => Some(Err(BridgeError::Ims(
                "the SIM thread ended without an answer".into(),
            ))),
        }
    }

    /// Whether the work has run past its deadline.
    pub(crate) fn overdue(&self, now: Instant) -> bool {
        now >= self.deadline
    }

    /// Abandons the work. A thread already inside the SIM finishes that call;
    /// its result is dropped unread.
    pub(crate) fn cancel(&self) {
        self.cancelled.store(true, Ordering::SeqCst);
    }

    /// The flag the thread sets when it ends. Outlives the worker, so a line
    /// that abandoned the work can still tell when the card is free again.
    pub(crate) fn finished_flag(&self) -> Arc<AtomicBool> {
        Arc::clone(&self.done)
    }
}

impl<T> Drop for SimWorker<T> {
    fn drop(&mut self) {
        self.cancel();
    }
}

impl Reauthentication {
    /// Starts answering `job` on a thread of its own, with one attempt and
    /// [`WORK_TIMEOUT`] to finish it. `open` yields the SIM; in production it
    /// is [`open_card`](crate::ims::open_card) (through the agent's
    /// `ReauthEnv`), a test supplies one that does not need hardware.
    pub(crate) fn spawn<F>(job: ChallengeJob, open: F) -> Self
    where
        F: FnOnce(&ImsRegisterConfig) -> BridgeResult<OpenedCard> + Send + 'static,
    {
        SimWorker::start(WORK_TIMEOUT, move |cancelled| {
            let params = parse_digest_challenge(&job.www_authenticate)?;
            let challenge = extract_challenge(&params)?;
            answer_once(&job, open, cancelled, &challenge)
        })
    }
}

impl Reauthentication {
    /// A card-reader line's answer to `job` (#233). While the card cannot be
    /// reached, the same challenge is put to it again every `retry_interval`
    /// for `retry_window`. The network is still waiting for the answer
    /// meanwhile, and asking again is safe: pcscd reports a reset before the
    /// command reaches the card, and a card that did answer before the link
    /// dropped now reports a sequence mismatch, which the line resynchronises
    /// as usual.
    ///
    /// Each attempt opens the card afresh and releases it before the pause.
    /// The attempts run one after the other: a command stuck inside the card
    /// holds it for every program, so a second attempt beside it would only
    /// queue behind it. The deadline is the window plus `attempt_bound`, not
    /// [`WORK_TIMEOUT`]: past it the line stops waiting and keeps its
    /// registration until the card is back.
    pub(crate) fn spawn_retrying<F>(job: ChallengeJob, open: F, timing: CardTiming) -> Self
    where
        F: Fn(&ImsRegisterConfig) -> BridgeResult<OpenedCard> + Send + 'static,
    {
        SimWorker::start(
            timing.retry_window + timing.attempt_bound,
            move |cancelled| {
                let params = parse_digest_challenge(&job.www_authenticate)?;
                let challenge = extract_challenge(&params)?;
                let window_end = Instant::now() + timing.retry_window;
                let mut attempt: u32 = 1;
                loop {
                    match answer_once(&job, &open, cancelled, &challenge) {
                        Err(e)
                            if e.is_card_unreachable()
                                && !cancelled.load(Ordering::SeqCst)
                                && Instant::now() + timing.retry_interval < window_end =>
                        {
                            tracing::info!(
                                attempt,
                                error = %e,
                                "the SIM is out of reach; asking it the same challenge again"
                            );
                            std::thread::sleep(timing.retry_interval);
                            if cancelled.load(Ordering::SeqCst) {
                                return Err(abandoned());
                            }
                            attempt += 1;
                        }
                        result => return result,
                    }
                }
            },
        )
    }
}

impl CardCheck {
    /// Opens the card on a thread of its own, to be given up on after `bound`.
    /// The opened card is dropped at once, which releases it leaving the card
    /// (#234).
    pub(crate) fn spawn<F>(cfg: ImsRegisterConfig, open: F, bound: Duration) -> Self
    where
        F: FnOnce(&ImsRegisterConfig) -> BridgeResult<OpenedCard> + Send + 'static,
    {
        SimWorker::start(bound, move |_| open(&cfg).map(|_| ()))
    }
}

/// One attempt: open the card and answer `challenge`. The card is released
/// when this returns.
fn answer_once<F>(
    job: &ChallengeJob,
    open: F,
    cancelled: &AtomicBool,
    challenge: &DigestChallenge,
) -> BridgeResult<Answer>
where
    F: FnOnce(&ImsRegisterConfig) -> BridgeResult<OpenedCard>,
{
    let OpenedCard {
        mut card,
        imsi,
        opened,
        ..
    } = open(&job.cfg)?;
    if cancelled.load(Ordering::SeqCst) {
        return Err(abandoned());
    }
    answer_challenge(card.as_mut(), &imsi, &opened, job, challenge)
}

fn abandoned() -> BridgeError {
    BridgeError::Ims("the re-authentication was abandoned".into())
}

/// The Authorization for `challenge`, from the SIM's AKA run.
fn answer_challenge(
    card: &mut dyn crate::modules::usim::ApduTransport,
    imsi: &str,
    opened: &crate::ims::OpenTimings,
    job: &ChallengeJob,
    challenge: &DigestChallenge,
) -> BridgeResult<Answer> {
    // The private identity is always IMSI-based (TS 33.203), whatever public
    // identity the registration uses.
    let impi_uri = format!("{imsi}@{}", job.realm);
    let authenticating = Instant::now();
    let aka = run_aka(card, challenge);
    crate::ims::log_aka_timings(opened, authenticating.elapsed(), &aka);
    match aka? {
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

    /// A reply that makes the scripted SIM fail as an unreachable card: a
    /// reset in the middle of an AKA.
    pub(crate) const UNREACHABLE: &str = "!unreachable";

    /// The error a reset card gives, as the PC/SC transport reports it.
    pub(crate) fn reset_error() -> BridgeError {
        BridgeError::CardUnreachable(
            "PC/SC transmit failed: The smart card has been reset, so any shared state \
             information is invalid"
                .into(),
        )
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
            if reply == UNREACHABLE {
                return Err(reset_error());
            }
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
                opened: Default::default(),
            })
        }
    }
}

#[cfg(test)]
impl scripted::Sim {
    fn opened(replies: &Arc<std::sync::Mutex<Vec<String>>>, delay: Duration) -> OpenedCard {
        OpenedCard {
            card: Box::new(scripted::Sim {
                replies: Arc::clone(replies),
                delay,
            }),
            imsi: "001010000000001".into(),
            imei: "350000000000001".into(),
            opened: Default::default(),
        }
    }
}

#[cfg(test)]
pub(crate) mod gated {
    //! A scripted SIM that cannot be reached until a condition holds. Mocked
    //! because CI has no card reader; the real PC/SC error mapping is covered
    //! by the hardware-gated tests in `modules::pcsc_card`.
    use super::*;
    use std::sync::atomic::AtomicUsize;
    use std::sync::Mutex;

    /// The card opener a test gives the agent, and how many times it was asked
    /// to open the card.
    pub(crate) type Opener = (
        Box<dyn Fn(&ImsRegisterConfig) -> BridgeResult<OpenedCard> + Send + Sync>,
        Arc<AtomicUsize>,
    );

    /// A card whose `n`th opening (from 0) fails as unreachable until
    /// `reachable(n)` holds, then gives a SIM that answers `replies` in turn.
    pub(crate) fn card_when(
        reachable: impl Fn(usize) -> bool + Send + Sync + 'static,
        replies: Vec<String>,
        delay: Duration,
    ) -> Opener {
        let script = Arc::new(Mutex::new(replies));
        let opens = Arc::new(AtomicUsize::new(0));
        let counter = Arc::clone(&opens);
        let open = move |_: &ImsRegisterConfig| {
            let n = counter.fetch_add(1, Ordering::SeqCst);
            if reachable(n) {
                Ok(scripted::Sim::opened(&script, delay))
            } else {
                Err(scripted::reset_error())
            }
        };
        (Box::new(open), opens)
    }

    /// A card that is out of reach for its first `failures` openings.
    pub(crate) fn card_after(failures: usize, replies: Vec<String>, delay: Duration) -> Opener {
        card_when(move |n| n >= failures, replies, delay)
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
                opened: Default::default(),
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
    fn run_aka_goes_through_the_transports_authenticate_hook() {
        // No SIM in CI, so this transport stands in for one. It pins that AKA
        // reaches the card only through `authenticate_usim`, the hook a
        // card-reader line overrides to select and authenticate in one PC/SC
        // transaction (#232): a direct `usim::authenticate` would bypass it.
        type Seen = Arc<std::sync::Mutex<Option<([u8; 16], [u8; 16])>>>;
        struct Hooked(Seen);
        impl ApduTransport for Hooked {
            fn transmit_apdu(&mut self, apdu: &str) -> BridgeResult<String> {
                panic!("AKA bypassed authenticate_usim and sent {apdu}");
            }
            fn authenticate_usim(
                &mut self,
                rand: &[u8; 16],
                autn: &[u8; 16],
            ) -> BridgeResult<AkaResult> {
                *self.0.lock().unwrap() = Some((*rand, *autn));
                Ok(AkaResult::Success {
                    res: vec![0x11; 8],
                    ck: vec![0x22; 16],
                    ik: vec![0x33; 16],
                })
            }
        }

        let mut nonce = [0u8; 32];
        nonce[..16].fill(0xA1);
        nonce[16..].fill(0xB2);
        let nonce = base64::Engine::encode(&base64::engine::general_purpose::STANDARD, nonce);
        let header = format!(
            "Digest realm=\"ims.example\", nonce=\"{nonce}\", algorithm=AKAv1-MD5, qop=\"auth\""
        );
        let challenge = extract_challenge(&parse_digest_challenge(&header).unwrap()).unwrap();

        let seen: Seen = Arc::default();
        let mut card = Hooked(Arc::clone(&seen));
        let AkaResult::Success { res, .. } = crate::ims::run_aka(&mut card, &challenge).unwrap()
        else {
            panic!("expected the hook's scripted success");
        };
        assert_eq!(res, vec![0x11; 8]);
        assert_eq!(*seen.lock().unwrap(), Some(([0xA1; 16], [0xB2; 16])));
    }

    #[test]
    fn work_is_overdue_after_the_timeout() {
        let work = Reauthentication::spawn(job(), card(vec![success_reply()], Duration::ZERO));
        assert!(!work.overdue(Instant::now()));
        assert!(work.overdue(Instant::now() + WORK_TIMEOUT));
    }

    #[test]
    fn a_card_reader_answer_retries_an_unreachable_card_within_the_window() {
        let (open, opens) = super::gated::card_after(2, vec![success_reply()], Duration::ZERO);
        let work = Reauthentication::spawn_retrying(job(), open, CardTiming::fast());
        assert!(matches!(wait(&work), Ok(Answer::Authenticated { .. })));
        assert_eq!(opens.load(Ordering::SeqCst), 3);
    }

    #[test]
    fn a_card_reader_answer_gives_up_at_the_end_of_the_window() {
        let (open, opens) = super::gated::card_after(usize::MAX, vec![], Duration::ZERO);
        let timing = CardTiming::fast();
        let started = Instant::now();
        let work = Reauthentication::spawn_retrying(job(), open, timing.clone());
        let Err(e) = wait(&work) else {
            panic!("an unreachable card answered");
        };
        assert!(e.is_card_unreachable(), "{e}");
        assert!(started.elapsed() < timing.retry_window + timing.attempt_bound);
        let tries = opens.load(Ordering::SeqCst);
        assert!(tries >= 2, "only {tries} attempts");
    }

    #[test]
    fn a_card_reader_answer_does_not_retry_an_error_from_the_card() {
        let (open, opens) = super::gated::card_after(0, vec!["6982".into()], Duration::ZERO);
        let work = Reauthentication::spawn_retrying(job(), open, CardTiming::fast());
        let Err(e) = wait(&work) else {
            panic!("the card's refusal was taken as an answer");
        };
        assert!(!e.is_card_unreachable(), "{e}");
        assert_eq!(opens.load(Ordering::SeqCst), 1);
    }

    #[test]
    fn a_modem_answer_is_one_attempt_whatever_the_error() {
        let (open, opens) = super::gated::card_after(5, vec![success_reply()], Duration::ZERO);
        let work = Reauthentication::spawn(job(), open);
        let Err(e) = wait(&work) else {
            panic!("an unreachable card answered");
        };
        assert!(e.is_card_unreachable(), "{e}");
        assert_eq!(opens.load(Ordering::SeqCst), 1);
    }

    #[test]
    fn an_abandoned_card_reader_answer_stops_retrying() {
        let (open, opens) = super::gated::card_after(usize::MAX, vec![], Duration::ZERO);
        let work = Reauthentication::spawn_retrying(job(), open, CardTiming::fast());
        let done = work.finished_flag();
        std::thread::sleep(Duration::from_millis(30));
        drop(work);
        let until = Instant::now() + Duration::from_secs(5);
        while !done.load(Ordering::SeqCst) {
            assert!(Instant::now() < until, "the thread never ended");
            std::thread::sleep(Duration::from_millis(5));
        }
        let tries = opens.load(Ordering::SeqCst);
        std::thread::sleep(Duration::from_millis(60));
        assert_eq!(opens.load(Ordering::SeqCst), tries, "it kept trying");
    }

    #[test]
    fn a_finished_worker_says_so_even_after_it_was_abandoned() {
        let work = Reauthentication::spawn(job(), card(vec![success_reply()], Duration::ZERO));
        let done = work.finished_flag();
        drop(work);
        let until = Instant::now() + Duration::from_secs(5);
        while !done.load(Ordering::SeqCst) {
            assert!(Instant::now() < until, "the thread never ended");
            std::thread::sleep(Duration::from_millis(5));
        }
    }

    #[test]
    fn a_check_stuck_in_the_card_is_overdue_and_says_when_it_ends() {
        let (release, gate) = mpsc::channel::<()>();
        let check = CardCheck::spawn(
            crate::ims::test_register_config(),
            move |cfg| {
                let _ = gate.recv();
                card(vec![success_reply()], Duration::ZERO)(cfg)
            },
            Duration::from_millis(50),
        );
        let done = check.finished_flag();
        std::thread::sleep(Duration::from_millis(60));
        assert!(check.poll().is_none());
        assert!(check.overdue(Instant::now()));
        assert!(!done.load(Ordering::SeqCst), "still inside the card");
        drop(check);
        release.send(()).unwrap();
        let until = Instant::now() + Duration::from_secs(5);
        while !done.load(Ordering::SeqCst) {
            assert!(Instant::now() < until, "the thread never ended");
            std::thread::sleep(Duration::from_millis(5));
        }
    }

    #[test]
    fn a_check_reports_whether_the_card_opened() {
        let ok = CardCheck::spawn(
            crate::ims::test_register_config(),
            card(vec![success_reply()], Duration::ZERO),
            Duration::from_secs(5),
        );
        assert!(wait_for(&ok).is_ok());
        let (open, opens) = super::gated::card_after(1, vec![success_reply()], Duration::ZERO);
        let out = CardCheck::spawn(
            crate::ims::test_register_config(),
            open,
            Duration::from_secs(5),
        );
        assert!(wait_for(&out).unwrap_err().is_card_unreachable());
        assert_eq!(opens.load(Ordering::SeqCst), 1);
    }

    fn wait_for<T>(work: &SimWorker<T>) -> BridgeResult<T> {
        let until = Instant::now() + Duration::from_secs(5);
        loop {
            if let Some(result) = work.poll() {
                return result;
            }
            assert!(Instant::now() < until, "the worker never finished");
            std::thread::sleep(Duration::from_millis(5));
        }
    }
}

#[cfg(test)]
mod timing_tests {
    use super::scripted::*;
    use super::*;
    use crate::modules::at_worker::test_log;

    fn job() -> ChallengeJob {
        ChallengeJob {
            cfg: crate::ims::test_register_config(),
            realm: "ims.example".into(),
            uri: "sip:ims.example".into(),
            www_authenticate: challenge_header(),
        }
    }

    fn answer(open: impl FnOnce(&ImsRegisterConfig) -> BridgeResult<OpenedCard>) -> String {
        let job = job();
        let params = parse_digest_challenge(&job.www_authenticate).unwrap();
        let challenge = extract_challenge(&params).unwrap();
        let cancelled = AtomicBool::new(false);
        let (result, logs) = test_log::capture(|| answer_once(&job, open, &cancelled, &challenge));
        result.expect("answered");
        logs
    }

    fn timing_line(logs: &str) -> &str {
        let lines: Vec<&str> = logs
            .lines()
            .filter(|l| l.contains("SIM challenge timings"))
            .collect();
        assert_eq!(lines.len(), 1, "one timing line per AKA run: {logs}");
        assert!(lines[0].contains("DEBUG"), "{}", lines[0]);
        lines[0]
    }

    fn ms_field(line: &str, name: &str) -> u64 {
        let at = line
            .find(&format!("{name}="))
            .unwrap_or_else(|| panic!("no {name}: {line}"));
        line[at + name.len() + 1..]
            .split(|c: char| !c.is_ascii_digit())
            .next()
            .unwrap()
            .parse()
            .unwrap()
    }

    #[test]
    fn each_aka_run_logs_one_timing_line() {
        let delay = Duration::from_millis(50);
        let open_ms = Duration::from_millis(7);
        let logs = answer(move |cfg| {
            let mut opened = card(vec![success_reply()], delay)(cfg)?;
            opened.opened = crate::ims::OpenTimings {
                open: open_ms,
                select: Some(Duration::from_millis(3)),
            };
            Ok(opened)
        });
        let line = timing_line(&logs);
        assert!(ms_field(line, "authenticate_ms") >= 50, "{line}");
        assert_eq!(ms_field(line, "open_ms"), 7, "{line}");
        assert_eq!(ms_field(line, "select_ms"), 3, "{line}");
        assert!(line.contains("outcome=\"success\""), "{line}");

        let logs = answer(card(vec![sync_failure_reply()], Duration::ZERO));
        assert!(
            timing_line(&logs).contains("outcome=\"sync-failure\""),
            "{logs}"
        );
    }

    /// Pinned to `contracts/operator-surface.md`: a card reader selects inside
    /// its AKA, so the line carries no `select_ms` at all.
    #[test]
    fn a_card_reader_aka_logs_no_select_time() {
        let logs = answer(card(vec![success_reply()], Duration::ZERO));
        let line = timing_line(&logs);
        assert!(!line.contains("select_ms"), "{line}");
    }
}
