//! Each slot's token: checked with Steam at start and when it changes, logged on again once no
//! server has used it for a week, never while a server runs on it, and only named by its hint.

#![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic, clippy::indexing_slicing, clippy::unimplemented, reason = "a test fails by crashing")]
#![allow(clippy::disallowed_methods, reason = "a test locks its fixtures and fails on a poisoned lock")]

use std::collections::HashMap;
use std::sync::{Arc, Mutex};
use std::time::Duration;

use tokio::time::Instant;

use ankor_agent::gslt_keep_alive::{GSLT_ACCEPTED_REUSE, GSLT_KEEP_ALIVE_DAYS, GsltKeepAlive, GsltSlotsPort, SteamLogon};
use ankor_agent::logger::{Entry, Logger};
use ankor_agent::refused_gslts::RefusedGslts;
use ankor_agent::steam_logon::{SteamError, eresult};
use ankor_agent::testing::fixtures::{GSLT_A, GSLT_B, agent_config, launch_config};
use ankor_agent::testing::slots::{Slots, change_layout, launch_server, make_slots};
use ankor_protocol::types::{ConfiguredSlot, GsltState, SlotState};
use serde_json::json;
use tokio::sync::oneshot;

const DAY: Duration = Duration::from_hours(24);

type Answers = Arc<Mutex<HashMap<String, Result<u32, String>>>>;

struct Setup {
    base: Slots,
    keep_alive: GsltKeepAlive,
    logons: Arc<Mutex<Vec<String>>>,
    lines: Arc<Mutex<Vec<String>>>,
    answers: Answers,
    offset: Arc<Mutex<Duration>>,
    stop: tokio_util::sync::CancellationToken,
}

impl Setup {
    fn advance(&self, by: Duration) {
        *self.offset.lock().unwrap() += by;
    }

    fn logons(&self) -> Vec<String> {
        self.logons.lock().unwrap().clone()
    }
}

fn answering(logons: &Arc<Mutex<Vec<String>>>, answers: &Answers) -> SteamLogon {
    let (logons, answers) = (Arc::clone(logons), Arc::clone(answers));

    Arc::new(move |token: String| {
        logons.lock().unwrap().push(token.clone());

        let answer = answers.lock().unwrap().get(&token).cloned().unwrap_or(Ok(eresult::OK));

        Box::pin(async move { answer.map_err(|message| SteamError::Io(std::io::Error::other(message))) })
    })
}

fn setup_with(answers: &[(&str, Result<u32, &str>)], refused: RefusedGslts) -> Setup {
    let base = make_slots(agent_config(&json!({})));
    let logons: Arc<Mutex<Vec<String>>> = Arc::default();
    let lines: Arc<Mutex<Vec<String>>> = Arc::default();
    let answers: Answers = Arc::new(Mutex::new(answers.iter().map(|(token, answer)| ((*token).to_owned(), answer.map_err(str::to_owned))).collect()));
    let offset = Arc::new(Mutex::new(Duration::ZERO));
    let start = Instant::now();
    let (seen, clock) = (Arc::clone(&lines), Arc::clone(&offset));
    let log = Logger::new("test", Some(Arc::new(move |entry: Entry| seen.lock().unwrap().push(format!("{:?}: {}", entry.level, entry.message)))));
    let stop = tokio_util::sync::CancellationToken::new();
    let keep_alive = GsltKeepAlive::with_clock(
        Arc::new(base.slots.clone()) as Arc<dyn GsltSlotsPort>,
        answering(&logons, &answers),
        refused,
        log,
        Arc::new(move || start + *clock.lock().unwrap()),
        Duration::from_hours(24 * GSLT_KEEP_ALIVE_DAYS),
        stop.clone(),
    );

    Setup { base, keep_alive, logons, lines, answers, offset, stop }
}

fn setup(answers: &[(&str, Result<u32, &str>)]) -> Setup {
    setup_with(answers, RefusedGslts::new(None, Logger::null()))
}

async fn settle() {
    tokio::time::sleep(Duration::from_millis(30)).await;
}

#[tokio::test]
async fn logs_every_token_on_at_start_and_reports_what_steam_made_of_each() {
    let setup = setup(&[(GSLT_B, Ok(eresult::ACCESS_DENIED))]);

    setup.keep_alive.start().await;
    setup.stop.cancel();
    settle().await;

    assert_eq!(setup.logons(), [GSLT_A, GSLT_B]);
    assert_eq!(setup.base.status("a").gslt_state, GsltState::Ok);
    assert_eq!(setup.base.status("b").gslt_state, GsltState::Invalid);

    let reported: Vec<(String, bool, GsltState)> =
        setup.base.reported().iter().map(|status| (status.slot_id.clone(), status.reserved, status.gslt_state)).collect();

    assert_eq!(
        reported,
        [
            ("a".to_owned(), true, GsltState::Unchecked),
            ("a".to_owned(), false, GsltState::Ok),
            ("b".to_owned(), true, GsltState::Unchecked),
            ("b".to_owned(), false, GsltState::Invalid),
        ]
    );
}

#[tokio::test]
async fn reuses_a_recent_acceptance_and_keeps_a_refusal() {
    let setup = setup(&[(GSLT_B, Ok(eresult::ACCESS_DENIED))]);
    let both = ["a".to_owned(), "b".to_owned()];

    setup.keep_alive.changed(&both).await;
    setup.base.slots.set_gslt_state("a", GSLT_A, GsltState::Unchecked);
    setup.base.slots.set_gslt_state("b", GSLT_B, GsltState::Unchecked);
    setup.advance(GSLT_ACCEPTED_REUSE.saturating_sub(Duration::from_millis(1)));
    setup.keep_alive.changed(&both).await;

    assert_eq!(setup.logons(), [GSLT_A, GSLT_B]);
    assert_eq!(setup.base.status("a").gslt_state, GsltState::Ok);
    assert_eq!(setup.base.status("b").gslt_state, GsltState::Invalid);

    setup.advance(Duration::from_millis(1));
    setup.keep_alive.changed(&both).await;
    assert_eq!(setup.logons(), [GSLT_A, GSLT_B, GSLT_A]);
}

#[tokio::test]
async fn leaves_a_token_unchecked_when_steam_could_not_judge_it() {
    let setup = setup(&[(GSLT_A, Ok(eresult::SERVICE_UNAVAILABLE)), (GSLT_B, Err("ECONNRESET"))]);

    setup.keep_alive.pass().await;

    for slot in ["a", "b"] {
        let status = setup.base.status(slot);

        assert_eq!((status.gslt_state, status.reserved), (GsltState::Unchecked, false), "{slot}");
    }

    setup.answers.lock().unwrap().clear();
    setup.keep_alive.pass().await;
    assert_eq!(setup.logons(), [GSLT_A, GSLT_B, GSLT_A, GSLT_B]);
    assert_eq!(setup.base.status("a").gslt_state, GsltState::Ok);
}

/// A malformed logon says nothing of the token, so it is not refused; it is logged as an error,
/// as it points at the agent's logon or a change on Steam's side.
#[tokio::test]
async fn logs_a_malformed_logon_as_an_error_and_does_not_refuse_the_token() {
    let setup = setup(&[(GSLT_A, Ok(eresult::INVALID_PARAM))]);

    setup.keep_alive.pass().await;

    assert_eq!(setup.base.status("a").gslt_state, GsltState::Unchecked);
    assert!(setup.lines.lock().unwrap().iter().any(|line| line.starts_with("Error: ") && line.contains("malformed")), "{:?}", setup.lines.lock().unwrap());

    setup.answers.lock().unwrap().clear();
    setup.keep_alive.pass().await;
    assert_eq!(setup.base.status("a").gslt_state, GsltState::Ok);
}

#[tokio::test]
async fn tries_an_unchecked_token_again_on_the_next_pass() {
    let setup = setup(&[]);

    setup.keep_alive.pass().await;
    setup.base.slots.set_gslt_state("a", GSLT_A, GsltState::Unchecked);
    setup.advance(GSLT_ACCEPTED_REUSE);
    setup.answers.lock().unwrap().insert(GSLT_A.to_owned(), Ok(eresult::SERVICE_UNAVAILABLE));
    setup.keep_alive.changed(&["a".to_owned()]).await;
    assert_eq!(setup.base.status("a").gslt_state, GsltState::Unchecked);

    setup.answers.lock().unwrap().clear();
    setup.keep_alive.pass().await;
    assert_eq!(setup.logons(), [GSLT_A, GSLT_B, GSLT_A, GSLT_A]);
    assert_eq!(setup.base.status("a").gslt_state, GsltState::Ok);
}

#[tokio::test]
async fn logs_a_token_on_again_after_a_week_unused_and_not_before() {
    let setup = setup(&[]);

    setup.keep_alive.pass().await;
    setup.advance((DAY * u32::try_from(GSLT_KEEP_ALIVE_DAYS).unwrap()).saturating_sub(Duration::from_millis(1)));
    setup.keep_alive.pass().await;
    assert_eq!(setup.logons(), [GSLT_A, GSLT_B]);

    setup.advance(Duration::from_millis(1));
    setup.keep_alive.pass().await;
    assert_eq!(setup.logons(), [GSLT_A, GSLT_B, GSLT_A, GSLT_B]);
}

#[tokio::test]
async fn never_logs_a_refused_token_on_again_even_after_a_restart() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("gslt-refused.json");
    let first = setup_with(&[(GSLT_B, Ok(eresult::ACCESS_DENIED))], RefusedGslts::new(Some(path.clone()), Logger::null()));

    first.keep_alive.pass().await;
    first.answers.lock().unwrap().clear();
    first.advance(DAY * u32::try_from(GSLT_KEEP_ALIVE_DAYS).unwrap());
    first.keep_alive.pass().await;
    assert_eq!(first.logons(), [GSLT_A, GSLT_B, GSLT_A]);
    assert_eq!(first.base.status("b").gslt_state, GsltState::Invalid);

    let restarted = setup_with(&[], RefusedGslts::new(Some(path), Logger::null()));

    restarted.keep_alive.pass().await;
    assert_eq!(restarted.logons(), [GSLT_A]);
    assert_eq!(restarted.base.status("b").gslt_state, GsltState::Invalid);
}

#[tokio::test]
async fn never_logs_on_a_running_servers_token_and_counts_the_server_as_its_use() {
    let setup = setup(&[]);
    let week = DAY * u32::try_from(GSLT_KEEP_ALIVE_DAYS).unwrap();

    launch_server(&setup.base.slots, launch_config(&json!({})), "5b1e2c7a-8d4f-4c3b-9a6e-2f1d0c9b8a7e").await.unwrap();
    assert!(setup.base.docker.specs()[0].env.contains(&format!("SRCDS_TOKEN={GSLT_A}")));

    setup.keep_alive.pass().await;
    assert_eq!(setup.logons(), [GSLT_B]);

    setup.advance(week);
    setup.keep_alive.pass().await;
    assert_eq!(setup.logons(), [GSLT_B, GSLT_B]);

    setup.base.docker.exit(0, Some(0));
    settle().await;
    setup.advance(week);
    setup.keep_alive.pass().await;
    assert_eq!(setup.logons(), [GSLT_B, GSLT_B, GSLT_A, GSLT_B]);
}

#[tokio::test]
async fn a_server_on_a_token_counts_as_its_use_from_a_pass_a_check_or_steam() {
    let pass = setup(&[]);

    pass.keep_alive.pass().await;
    launch_server(&pass.base.slots, launch_config(&json!({})), "5b1e2c7a-8d4f-4c3b-9a6e-2f1d0c9b8a7e").await.unwrap();
    pass.advance(DAY * 6);
    pass.keep_alive.pass().await;
    pass.base.docker.exit(0, Some(0));
    settle().await;
    pass.advance(DAY);
    pass.keep_alive.pass().await;
    assert_eq!(pass.logons(), [GSLT_A, GSLT_B, GSLT_B]);

    let used = setup(&[]);

    used.keep_alive.pass().await;
    used.advance(DAY * 6);
    used.keep_alive.used(GSLT_A);
    used.advance(DAY);
    used.keep_alive.pass().await;
    assert_eq!(used.logons(), [GSLT_A, GSLT_B, GSLT_B]);

    let check = setup(&[]);

    check.keep_alive.changed(&["a".to_owned()]).await;
    launch_server(&check.base.slots, launch_config(&json!({})), "5b1e2c7a-8d4f-4c3b-9a6e-2f1d0c9b8a7e").await.unwrap();
    check.advance(DAY * 6);
    check.keep_alive.changed(&["a".to_owned()]).await;
    check.base.docker.exit(0, Some(0));
    settle().await;
    check.advance(DAY * 2);
    check.keep_alive.pass().await;
    assert_eq!(check.logons(), [GSLT_A, GSLT_B]);
}

/// A logon of `held` that waits for the test's answer; every other token answers `others`.
fn held_logon(held: &'static str, others: u32, logons: &Arc<Mutex<Vec<String>>>) -> (SteamLogon, Arc<Mutex<Option<oneshot::Sender<u32>>>>) {
    let answer: Arc<Mutex<Option<oneshot::Sender<u32>>>> = Arc::default();
    let (pending, logons) = (Arc::clone(&answer), Arc::clone(logons));
    let logon: SteamLogon = Arc::new(move |token: String| {
        logons.lock().unwrap().push(token.clone());

        if token == held {
            let (sender, receiver) = oneshot::channel();

            *pending.lock().unwrap() = Some(sender);
            Box::pin(async move { receiver.await.map_err(|error| SteamError::Io(std::io::Error::other(error))) })
        } else {
            Box::pin(async move { Ok(others) })
        }
    });

    (logon, answer)
}

async fn until_held(answer: &Arc<Mutex<Option<oneshot::Sender<u32>>>>) {
    for _ in 0..100 {
        if answer.lock().unwrap().is_some() {
            return;
        }

        tokio::time::sleep(Duration::from_millis(5)).await;
    }

    panic!("the logon never started");
}

fn keep_alive(base: &Slots, logon: SteamLogon) -> GsltKeepAlive {
    GsltKeepAlive::new(
        Arc::new(base.slots.clone()) as Arc<dyn GsltSlotsPort>,
        logon,
        RefusedGslts::new(None, Logger::null()),
        Logger::null(),
        tokio_util::sync::CancellationToken::new(),
    )
}

#[tokio::test]
async fn keeps_the_slot_from_launches_while_its_token_is_logged_on() {
    let base = make_slots(agent_config(&json!({})));
    let logons = Arc::default();
    let (logon, answer) = held_logon(GSLT_A, eresult::OK, &logons);
    let keep_alive = keep_alive(&base, logon);
    let checking = tokio::spawn({
        let keep_alive = keep_alive.clone();

        async move { keep_alive.changed(&["a".to_owned()]).await }
    });

    until_held(&answer).await;
    assert!(base.status("a").reserved);

    launch_server(&base.slots, launch_config(&json!({})), "5b1e2c7a-8d4f-4c3b-9a6e-2f1d0c9b8a7e").await.unwrap();
    assert!(base.docker.specs()[0].env.contains(&format!("SRCDS_TOKEN={GSLT_B}")));

    answer.lock().unwrap().take().unwrap().send(eresult::OK).unwrap();
    checking.await.unwrap();

    let status = base.status("a");

    assert_eq!((status.reserved, status.gslt_state), (false, GsltState::Ok));
}

#[tokio::test]
async fn finishes_a_check_whose_caller_stopped_waiting_during_the_logon() {
    let base = make_slots(agent_config(&json!({})));
    let logons = Arc::default();
    let (logon, answer) = held_logon(GSLT_A, eresult::OK, &logons);
    let keep_alive = keep_alive(&base, logon);
    let checking = tokio::spawn({
        let keep_alive = keep_alive.clone();

        async move { keep_alive.changed(&["a".to_owned()]).await }
    });

    until_held(&answer).await;
    checking.abort();
    assert!(checking.await.unwrap_err().is_cancelled());

    answer.lock().unwrap().take().unwrap().send(eresult::OK).unwrap();

    for _ in 0..100 {
        if !base.status("a").reserved {
            break;
        }

        tokio::time::sleep(Duration::from_millis(5)).await;
    }

    let status = base.status("a");

    assert_eq!((status.reserved, status.gslt_state), (false, GsltState::Ok));
}

#[tokio::test]
async fn checks_a_slot_once_for_every_change_while_its_check_waited() {
    let base = make_slots(agent_config(&json!({})));
    let logons: Arc<Mutex<Vec<String>>> = Arc::default();
    let (logon, answer) = held_logon(GSLT_A, eresult::SERVICE_UNAVAILABLE, &logons);
    let keep_alive = keep_alive(&base, logon);
    let first = tokio::spawn({
        let keep_alive = keep_alive.clone();

        async move { keep_alive.changed(&["a".to_owned()]).await }
    });

    until_held(&answer).await;

    let repeats: Vec<_> = (0..3)
        .map(|_| {
            let keep_alive = keep_alive.clone();

            tokio::spawn(async move { keep_alive.changed(&["b".to_owned()]).await })
        })
        .collect();

    tokio::time::sleep(Duration::from_millis(20)).await;
    answer.lock().unwrap().take().unwrap().send(eresult::OK).unwrap();
    first.await.unwrap();

    for repeat in repeats {
        repeat.await.unwrap();
    }

    assert_eq!(*logons.lock().unwrap(), [GSLT_A, GSLT_B]);
    assert_eq!(base.status("b").gslt_state, GsltState::Unchecked);
}

#[tokio::test]
async fn skips_a_queued_check_of_a_removed_slot_or_a_token_a_server_took() {
    let removed = make_slots(agent_config(&json!({})));
    let logons: Arc<Mutex<Vec<String>>> = Arc::default();
    let (logon, answer) = held_logon(GSLT_A, eresult::OK, &logons);
    let keep_alive = keep_alive(&removed, logon);
    let pass = tokio::spawn({
        let keep_alive = keep_alive.clone();

        async move { keep_alive.pass().await }
    });

    until_held(&answer).await;
    change_layout(&removed.slots, &[ConfiguredSlot { id: "a".to_owned(), port: 27015, tv_port: 27020, gslt: GSLT_A.to_owned() }]).unwrap();
    answer.lock().unwrap().take().unwrap().send(eresult::OK).unwrap();
    pass.await.unwrap();
    assert_eq!(*logons.lock().unwrap(), [GSLT_A]);

    let taken = make_slots(agent_config(&json!({})));
    let logons: Arc<Mutex<Vec<String>>> = Arc::default();
    let (logon, answer) = held_logon(GSLT_A, eresult::OK, &logons);
    let keep_alive = self::keep_alive(&taken, logon);
    let pass = tokio::spawn({
        let keep_alive = keep_alive.clone();

        async move { keep_alive.pass().await }
    });

    until_held(&answer).await;
    launch_server(&taken.slots, launch_config(&json!({})), "5b1e2c7a-8d4f-4c3b-9a6e-2f1d0c9b8a7e").await.unwrap();
    assert!(taken.docker.specs()[0].env.contains(&format!("SRCDS_TOKEN={GSLT_B}")));
    answer.lock().unwrap().take().unwrap().send(eresult::OK).unwrap();
    pass.await.unwrap();
    assert_eq!(*logons.lock().unwrap(), [GSLT_A], "a logon with b's token would log its server off Steam");
}

#[tokio::test]
async fn checks_a_replaced_token_at_once_even_while_the_slot_runs_on_its_old_one() {
    let setup = setup(&[]);
    let replacement = "CCCCCCCCCCCCCCCCCCCCCCCCCCCC0003";

    launch_server(&setup.base.slots, launch_config(&json!({})), "5b1e2c7a-8d4f-4c3b-9a6e-2f1d0c9b8a7e").await.unwrap();
    change_layout(
        &setup.base.slots,
        &[
            ConfiguredSlot { id: "a".to_owned(), port: 27015, tv_port: 27020, gslt: replacement.to_owned() },
            ConfiguredSlot { id: "b".to_owned(), port: 27016, tv_port: 27021, gslt: GSLT_B.to_owned() },
        ],
    )
    .unwrap();
    assert_eq!(setup.base.status("a").gslt_state, GsltState::Unchecked);

    setup.keep_alive.changed(&["a".to_owned()]).await;

    let status = setup.base.status("a");

    assert_eq!(setup.logons(), [replacement]);
    assert_eq!((status.state, status.gslt_state), (SlotState::Starting, GsltState::Ok));
}

#[tokio::test]
async fn drops_an_answer_for_a_token_or_slot_that_changed_while_steam_answered() {
    let base = make_slots(agent_config(&json!({})));
    let logons = Arc::default();
    let (logon, answer) = held_logon(GSLT_A, eresult::OK, &logons);
    let keep_alive = keep_alive(&base, logon);
    let check = tokio::spawn({
        let keep_alive = keep_alive.clone();

        async move { keep_alive.changed(&["a".to_owned()]).await }
    });

    until_held(&answer).await;
    change_layout(
        &base.slots,
        &[
            ConfiguredSlot { id: "a".to_owned(), port: 27015, tv_port: 27020, gslt: "CCCCCCCCCCCCCCCCCCCCCCCCCCCC0003".to_owned() },
            ConfiguredSlot { id: "b".to_owned(), port: 27016, tv_port: 27021, gslt: GSLT_B.to_owned() },
        ],
    )
    .unwrap();
    answer.lock().unwrap().take().unwrap().send(eresult::ACCESS_DENIED).unwrap();
    check.await.unwrap();

    let status = base.status("a");

    assert_eq!((status.gslt_state, status.reserved), (GsltState::Unchecked, false));
}

#[tokio::test]
async fn names_a_token_in_its_log_lines_by_its_hint_alone() {
    let setup = setup(&[(GSLT_A, Ok(eresult::EXPIRED)), (GSLT_B, Err("timeout"))]);

    setup.keep_alive.pass().await;

    let lines = setup.lines.lock().unwrap().join("\n");

    assert!(lines.contains("…AAAAAA0001") && lines.contains("…AAAAAA0002"), "{lines}");
    assert!(!lines.contains(GSLT_A) && !lines.contains(GSLT_B));
}

#[tokio::test(start_paused = true)]
async fn looks_again_every_interval_and_not_after_stop() {
    let setup = setup(&[(GSLT_A, Ok(eresult::SERVICE_UNAVAILABLE))]);
    let hour = Duration::from_hours(1);

    setup.keep_alive.start().await;
    assert_eq!(setup.logons(), [GSLT_A, GSLT_B]);

    setup.answers.lock().unwrap().clear();
    tokio::time::sleep(hour.saturating_sub(Duration::from_millis(1))).await;
    assert_eq!(setup.logons().len(), 2);
    tokio::time::sleep(Duration::from_millis(10)).await;
    assert_eq!(setup.logons(), [GSLT_A, GSLT_B, GSLT_A]);

    setup.stop.cancel();
    setup.advance(DAY * u32::try_from(GSLT_KEEP_ALIVE_DAYS).unwrap());
    tokio::time::sleep(hour * 2).await;
    assert_eq!(setup.logons().len(), 3);
}
