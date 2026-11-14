//! Checks each slot's token with Steam and keeps it from expiring. Steam lets a token it has not
//! seen for a while lapse, so one no server has used for [`GSLT_KEEP_ALIVE_DAYS`] is logged on
//! directly, the way a game server would, and straight off again. Last use is kept in memory only:
//! every start logs each token on, so a restart never leaves one older than the interval. A token
//! Steam refused is never logged on again, across restarts too.

use crate::sync::{SharedRun, join_or_spawn, lock};
use std::collections::{HashMap, HashSet};
use std::future::Future;
use std::pin::Pin;
use std::sync::{Arc, Mutex};
use std::time::Duration;

use ankor_protocol::helpers::gslt_hint;
use ankor_protocol::types::GsltState;
use futures_util::FutureExt as _;
use tokio_util::sync::CancellationToken;

use crate::logger::Logger;
use crate::refused_gslts::RefusedGslts;
use crate::steam_logon::{SteamError, eresult, eresult_name};

pub const GSLT_KEEP_ALIVE_DAYS: u64 = 7;

const KEEP_ALIVE_AGE: Duration = Duration::from_hours(GSLT_KEEP_ALIVE_DAYS * 24);
const INTERVAL: Duration = Duration::from_hours(1);

/// How long Steam's acceptance of a token stands in for another logon, so switching a slot back
/// and forth costs one logon.
pub const GSLT_ACCEPTED_REUSE: Duration = Duration::from_mins(1);

/// The answers that judge the token itself. A refusal is remembered across restarts and keeps its
/// slot from launches, so any other answer, such as one about the connection or the protocol, is
/// tried again later instead. `INVALID_PARAM` is not among them: it says the logon was malformed,
/// which a bug in it or a change on Steam's side would cause for every token on every node.
const REFUSING: [u32; 4] = [eresult::ACCESS_DENIED, eresult::BANNED, eresult::EXPIRED, eresult::ACCOUNT_DISABLED];

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct SlotToken {
    pub slot_id: String,
    pub gslt: String,
    pub gslt_state: GsltState,
}

/// Gives a slot back after its token's logon, with Steam's answer if there was one.
pub struct Release(Option<Box<dyn FnOnce(Option<GsltState>) + Send>>);

impl Release {
    pub fn new(give_back: impl FnOnce(Option<GsltState>) + Send + 'static) -> Self {
        Self(Some(Box::new(give_back)))
    }

    pub fn answer(mut self, answer: Option<GsltState>) {
        if let Some(give_back) = self.0.take() {
            give_back(answer);
        }
    }
}

impl Drop for Release {
    fn drop(&mut self) {
        if let Some(give_back) = self.0.take() {
            give_back(None);
        }
    }
}

pub trait GsltSlotsPort: Send + Sync {
    fn tokens(&self) -> Vec<SlotToken>;
    /// Tokens servers on this node started with and still run on.
    fn running_tokens(&self) -> HashSet<String>;
    /// Sets the slot's state while it still has `gslt`, the token the state is about.
    fn set_gslt_state(&self, slot_id: &str, gslt: &str, state: GsltState);
    /// Keeps the slot from launches until the release is called, or holds nothing and returns
    /// `None` when a server runs with `gslt`.
    fn hold_for_logon(&self, slot_id: &str, gslt: &str) -> Option<Release>;
}

/// Logs a token on to Steam and off again, and answers Steam's `EResult`.
pub type SteamLogon = Arc<dyn Fn(String) -> Pin<Box<dyn Future<Output = Result<u32, SteamError>> + Send>> + Send + Sync>;

pub type Clock = Arc<dyn Fn() -> tokio::time::Instant + Send + Sync>;

struct State {
    /// When Steam last saw each token, from a logon or a server running on it.
    last_used: HashMap<String, tokio::time::Instant>,
    /// When Steam last accepted each token at a logon of ours.
    accepted: HashMap<String, tokio::time::Instant>,
    /// Each slot's check still waiting its turn, `Some` whenever the lock is free. It reads the
    /// slot's token when it runs, so one serves every change meanwhile.
    waiting: HashMap<String, Option<SharedRun<()>>>,
}

struct Inner {
    /// The slots whose tokens are checked.
    slots: Arc<dyn GsltSlotsPort>,
    /// Logs a token on to Steam and off again.
    logon: SteamLogon,
    /// Tokens Steam refused, remembered across restarts.
    refused: Mutex<RefusedGslts>,
    /// Where check results are logged.
    log: Logger,
    /// The current time, replaceable in tests.
    now: Clock,
    /// How long a token may go unused before it is logged on again.
    max_age: Duration,
    /// What the checks remember between passes.
    state: Mutex<State>,
    /// One logon at a time.
    queue: tokio::sync::Mutex<()>,
    /// Once cancelled, no pass starts.
    stop: CancellationToken,
}

#[derive(Clone)]
pub struct GsltKeepAlive {
    inner: Arc<Inner>,
}

impl GsltKeepAlive {
    /// No pass starts once `stop` is cancelled.
    #[must_use]
    pub fn new(slots: Arc<dyn GsltSlotsPort>, logon: SteamLogon, refused: RefusedGslts, log: Logger, stop: CancellationToken) -> Self {
        Self::with_clock(slots, logon, refused, log, Arc::new(tokio::time::Instant::now), KEEP_ALIVE_AGE, stop)
    }

    #[must_use]
    pub fn with_clock(
        slots: Arc<dyn GsltSlotsPort>,
        logon: SteamLogon,
        refused: RefusedGslts,
        log: Logger,
        now: Clock,
        max_age: Duration,
        stop: CancellationToken,
    ) -> Self {
        Self {
            inner: Arc::new(Inner {
                slots,
                logon,
                refused: Mutex::new(refused),
                log,
                now,
                max_age,
                state: Mutex::new(State { last_used: HashMap::new(), accepted: HashMap::new(), waiting: HashMap::new() }),
                queue: tokio::sync::Mutex::new(()),
                stop,
            }),
        }
    }

    pub async fn start(&self) {
        let ticker = self.clone();

        tokio::spawn(async move {
            let mut interval = tokio::time::interval_at(tokio::time::Instant::now() + INTERVAL, INTERVAL);

            while ticker.inner.stop.run_until_cancelled(interval.tick()).await.is_some() {
                ticker.pass().await;
            }
        });

        self.pass().await;
    }

    /// A server logged on to Steam with the token, which counts as its use.
    pub fn used(&self, gslt: &str) {
        lock(&self.inner.state).last_used.insert(gslt.to_owned(), (self.inner.now)());
    }

    /// Checks the slots now, after their tokens changed.
    pub async fn changed(&self, slot_ids: &[String]) {
        futures_util::future::join_all(slot_ids.iter().map(|slot_id| self.check(slot_id))).await;
    }

    pub async fn pass(&self) {
        if self.inner.stop.is_cancelled() {
            return;
        }

        // Read before taking the lock, because the slots take a lock of their own.
        let (running, tokens) = (self.inner.slots.running_tokens(), self.inner.slots.tokens());
        let now = (self.inner.now)();
        let mut due = Vec::new();

        {
            let mut state = lock(&self.inner.state);
            let known = |gslt: &String| running.contains(gslt) || tokens.iter().any(|slot| &slot.gslt == gslt);

            state.last_used.retain(|gslt, _| known(gslt));
            state.accepted.retain(|gslt, _| known(gslt));

            for slot in tokens {
                if running.contains(&slot.gslt) {
                    state.last_used.insert(slot.gslt, now);
                } else if slot.gslt_state == GsltState::Unchecked
                    || state.last_used.get(&slot.gslt).is_none_or(|used| now.duration_since(*used) >= self.inner.max_age)
                {
                    due.push(slot.slot_id);
                }
            }
        }

        self.changed(&due).await;
    }

    /// The check runs in a spawned task because it leaves `waiting` once it has its turn. A check
    /// whose callers stopped waiting would otherwise end part-way and never run again.
    fn check(&self, slot_id: &str) -> SharedRun<()> {
        join_or_spawn(
            &self.inner.state,
            |state| state.waiting.entry(slot_id.to_owned()).or_default(),
            || {
                let (keep_alive, slot) = (self.clone(), slot_id.to_owned());

                async move {
                    let _turn = keep_alive.inner.queue.lock().await;

                    lock(&keep_alive.inner.state).waiting.remove(&slot);
                    keep_alive.check_now(&slot).await;
                }
                .boxed()
            },
        )
    }

    async fn check_now(&self, slot_id: &str) {
        let inner = &self.inner;
        let Some(slot) = inner.slots.tokens().into_iter().find(|candidate| candidate.slot_id == slot_id) else {
            return;
        };

        // A logon with a token a server is using would log that server off Steam.
        if inner.slots.running_tokens().contains(&slot.gslt) {
            self.used(&slot.gslt);
            return;
        }

        if lock(&inner.refused).has(&slot.gslt) {
            inner.slots.set_gslt_state(slot_id, &slot.gslt, GsltState::Invalid);
            return;
        }

        let recently_accepted = lock(&inner.state).accepted.get(&slot.gslt).is_some_and(|at| (inner.now)().duration_since(*at) < GSLT_ACCEPTED_REUSE);

        if recently_accepted {
            inner.slots.set_gslt_state(slot_id, &slot.gslt, GsltState::Ok);
            return;
        }

        let hint = gslt_hint(&slot.gslt);
        // A launch may have taken the token since the check above; the hold sees it.
        let Some(release) = inner.slots.hold_for_logon(slot_id, &slot.gslt) else {
            self.used(&slot.gslt);
            return;
        };
        let code = match (inner.logon)(slot.gslt.clone()).await {
            Ok(code) => code,
            Err(error) => {
                release.answer(None);
                inner.log.warn(format!("slot {slot_id}: could not log token …{hint} on to Steam: {error}"));
                return;
            }
        };

        // The slot may have been given another token while Steam answered.
        let current = inner.slots.tokens().into_iter().find(|candidate| candidate.slot_id == slot_id);

        if code == eresult::OK {
            let now = (inner.now)();
            let mut state = lock(&inner.state);

            state.last_used.insert(slot.gslt.clone(), now);
            state.accepted.insert(slot.gslt.clone(), now);
        } else if !REFUSING.contains(&code) {
            release.answer(None);

            // A malformed logon is the agent's or Steam's fault, not the token's, so an operator should see it.
            if code == eresult::INVALID_PARAM {
                inner.log.error(format!(
                    "slot {slot_id}: Steam called the logon of token …{hint} malformed ({}); the token is not refused, and the logon is tried again later",
                    eresult_name(code)
                ));
            } else {
                inner.log.warn(format!("slot {slot_id}: Steam could not take the logon of token …{hint} ({}); trying again later", eresult_name(code)));
            }

            return;
        } else {
            lock(&inner.refused).add(&slot.gslt);
        }

        let Some(current) = current.filter(|current| current.gslt == slot.gslt) else {
            release.answer(None);
            return;
        };
        let state = if code == eresult::OK { GsltState::Ok } else { GsltState::Invalid };

        if state == GsltState::Invalid {
            inner.log.warn(format!("slot {slot_id}: Steam refused token …{hint} ({}); the slot takes no games until it gets a new one", eresult_name(code)));
        } else if current.gslt_state != GsltState::Ok {
            inner.log.info(format!("slot {slot_id}: Steam accepted token …{hint}"));
        }

        release.answer(Some(state));
    }
}

#[cfg(test)]
mod tests {
    use std::sync::atomic::{AtomicU32, Ordering};

    use super::*;

    type Releases = Arc<Mutex<Vec<(String, Option<GsltState>)>>>;

    struct Slots {
        /// Each slot's token and its state.
        tokens: Mutex<Vec<SlotToken>>,
        /// Tokens that `running_tokens` reports.
        running: Mutex<HashSet<String>>,
        /// Tokens a server took after `running_tokens` was read, which only the hold sees.
        held_by_server: Mutex<HashSet<String>>,
        /// Each released slot and the answer it was given back with.
        released: Releases,
    }

    impl Slots {
        fn new(tokens: &[(&str, &str)]) -> Arc<Self> {
            Arc::new(Self {
                tokens: Mutex::new(
                    tokens
                        .iter()
                        .map(|(slot, gslt)| SlotToken { slot_id: (*slot).to_owned(), gslt: (*gslt).to_owned(), gslt_state: GsltState::Unchecked })
                        .collect(),
                ),
                running: Mutex::default(),
                held_by_server: Mutex::default(),
                released: Arc::default(),
            })
        }
    }

    impl GsltSlotsPort for Slots {
        fn tokens(&self) -> Vec<SlotToken> {
            lock(&self.tokens).clone()
        }

        fn running_tokens(&self) -> HashSet<String> {
            lock(&self.running).clone()
        }

        fn set_gslt_state(&self, slot_id: &str, gslt: &str, state: GsltState) {
            for slot in lock(&self.tokens).iter_mut().filter(|slot| slot.slot_id == slot_id && slot.gslt == gslt) {
                slot.gslt_state = state;
            }
        }

        fn hold_for_logon(&self, slot_id: &str, gslt: &str) -> Option<Release> {
            if lock(&self.held_by_server).contains(gslt) {
                return None;
            }

            let slot = slot_id.to_owned();
            let released = Arc::clone(&self.released);

            Some(Release::new(move |state| lock(&released).push((slot, state))))
        }
    }

    fn logon(answers: &'static [(&'static str, u32)], calls: Arc<AtomicU32>) -> SteamLogon {
        Arc::new(move |token: String| {
            calls.fetch_add(1, Ordering::SeqCst);
            let code = answers.iter().find(|(known, _)| *known == token).map_or(eresult::OK, |(_, code)| *code);

            Box::pin(async move { Ok(code) })
        })
    }

    #[tokio::test]
    async fn sends_no_logon_with_a_token_a_server_took_after_the_running_tokens_were_read() {
        let slots = Slots::new(&[("s1", "TAKEN")]);
        let calls = Arc::new(AtomicU32::new(0));

        lock(&slots.held_by_server).insert("TAKEN".to_owned());

        let keep_alive = GsltKeepAlive::new(
            slots.clone(),
            logon(&[], Arc::clone(&calls)),
            RefusedGslts::new(None, Logger::null()),
            Logger::null(),
            CancellationToken::new(),
        );

        keep_alive.changed(&["s1".to_owned()]).await;

        assert_eq!(calls.load(Ordering::SeqCst), 0);
        assert!(lock(&slots.released).is_empty());
        assert!(lock(&keep_alive.inner.state).last_used.contains_key("TAKEN"));
        assert_eq!(slots.tokens()[0].gslt_state, GsltState::Unchecked);
    }

    #[tokio::test]
    async fn tokens_are_checked_once_and_refusals_remembered() {
        let slots = Slots::new(&[("s1", "GOOD"), ("s2", "BAD"), ("s3", "BUSY"), ("s4", "ODD")]);
        let calls = Arc::new(AtomicU32::new(0));

        lock(&slots.running).insert("RUNNING".to_owned());

        // 7 is InvalidProtocolVer, an answer about the client rather than the token.
        let keep_alive = GsltKeepAlive::new(
            slots.clone(),
            logon(&[("BAD", eresult::ACCESS_DENIED), ("BUSY", eresult::SERVICE_UNAVAILABLE), ("ODD", 7)], Arc::clone(&calls)),
            RefusedGslts::new(None, Logger::null()),
            Logger::null(),
            CancellationToken::new(),
        );

        keep_alive.pass().await;

        let released = lock(&slots.released).clone();

        assert!(released.contains(&("s1".to_owned(), Some(GsltState::Ok))));
        assert!(released.contains(&("s2".to_owned(), Some(GsltState::Invalid))));
        assert!(released.contains(&("s3".to_owned(), None)));
        assert!(released.contains(&("s4".to_owned(), None)), "an answer about anything but the token refuses nothing");
        assert_eq!(calls.load(Ordering::SeqCst), 4);

        keep_alive.changed(&["s1".to_owned(), "s2".to_owned()]).await;
        assert_eq!(calls.load(Ordering::SeqCst), 4, "an accepted token is reused and a refused one never retried");

        keep_alive.changed(&["s4".to_owned()]).await;
        assert_eq!(calls.load(Ordering::SeqCst), 5, "a token Steam did not judge is tried again");
    }

    #[tokio::test(start_paused = true)]
    async fn forgets_a_token_no_slot_has_and_no_server_runs_on() {
        let slots = Slots::new(&[("s1", "KEPT")]);
        let keep_alive = GsltKeepAlive::new(
            slots.clone(),
            logon(&[], Arc::new(AtomicU32::new(0))),
            RefusedGslts::new(None, Logger::null()),
            Logger::null(),
            CancellationToken::new(),
        );

        lock(&slots.running).insert("RUNNING".to_owned());

        for gslt in ["KEPT", "RUNNING", "REPLACED"] {
            keep_alive.used(gslt);
            lock(&keep_alive.inner.state).accepted.insert(gslt.to_owned(), tokio::time::Instant::now());
        }

        keep_alive.pass().await;

        let state = lock(&keep_alive.inner.state);
        let mut used: Vec<_> = state.last_used.keys().cloned().collect();
        let mut accepted: Vec<_> = state.accepted.keys().cloned().collect();

        used.sort();
        accepted.sort();
        assert_eq!(used, ["KEPT", "RUNNING"]);
        assert_eq!(accepted, ["KEPT", "RUNNING"]);
    }

    #[tokio::test(start_paused = true)]
    async fn an_accepted_token_is_logged_on_again_once_it_has_gone_a_week_unused() {
        let slots = Slots::new(&[("s1", "GOOD")]);
        let calls = Arc::new(AtomicU32::new(0));
        let keep_alive = GsltKeepAlive::new(
            slots.clone(),
            logon(&[], Arc::clone(&calls)),
            RefusedGslts::new(None, Logger::null()),
            Logger::null(),
            CancellationToken::new(),
        );

        keep_alive.pass().await;
        slots.set_gslt_state("s1", "GOOD", GsltState::Ok);
        tokio::time::advance(Duration::from_hours(7 * 24 - 1)).await;
        keep_alive.pass().await;
        assert_eq!(calls.load(Ordering::SeqCst), 1, "a token used within the week is left alone");

        tokio::time::advance(Duration::from_hours(1)).await;
        keep_alive.pass().await;
        assert_eq!(calls.load(Ordering::SeqCst), 2);
    }

    #[tokio::test]
    async fn logs_an_acceptance_only_when_the_token_was_not_already_ok_and_every_refusal() {
        let slots = Slots::new(&[("s1", "NEW-TOKEN-1"), ("s2", "OLD-TOKEN-2"), ("s3", "BAD-TOKEN-3")]);
        let entries: Arc<Mutex<Vec<(crate::logger::Level, String)>>> = Arc::default();
        let sink = Arc::clone(&entries);
        let log = Logger::new("gslt", Some(Arc::new(move |entry: crate::logger::Entry| lock(&sink).push((entry.level, entry.message)))));

        slots.set_gslt_state("s2", "OLD-TOKEN-2", GsltState::Ok);

        let keep_alive = GsltKeepAlive::new(
            slots.clone(),
            logon(&[("BAD-TOKEN-3", eresult::ACCESS_DENIED)], Arc::new(AtomicU32::new(0))),
            RefusedGslts::new(None, Logger::null()),
            log,
            CancellationToken::new(),
        );

        keep_alive.changed(&["s1".to_owned(), "s2".to_owned(), "s3".to_owned()]).await;

        let mut logged = lock(&entries).clone();

        logged.sort_by(|a, b| a.1.cmp(&b.1));
        assert_eq!(
            logged,
            [
                (crate::logger::Level::Info, "[gslt] slot s1: Steam accepted token …EW-TOKEN-1".to_owned()),
                (
                    crate::logger::Level::Warn,
                    "[gslt] slot s3: Steam refused token …AD-TOKEN-3 (accessDenied); the slot takes no games until it gets a new one".to_owned()
                ),
            ]
        );
    }
}
