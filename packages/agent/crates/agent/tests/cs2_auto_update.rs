//! The public build read from Steam, and the automatic update that follows it.

#![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic, clippy::indexing_slicing, clippy::unimplemented, reason = "a test fails by crashing")]
#![allow(clippy::disallowed_methods, reason = "a test locks its fixtures and fails on a poisoned lock")]

use std::cmp::Ordering;
use std::sync::{Arc, Mutex};
use std::time::Duration;

use ankor_agent::cs2_auto_update::{Cs2AutoUpdate, Cs2AutoUpdateError, Cs2Updates, SteamAnswer, SteamFetch, parse_build_id};
use ankor_agent::failure::{AgentFailure, OpError};
use ankor_agent::logger::Logger;
use ankor_agent::request_handler::PublicBuild;
use ankor_protocol::constants::{CS2_BUILD_CHECK_INTERVAL, CS2_BUILD_POLL_INTERVAL};
use ankor_protocol::helpers::{compare_build_ids, cs2_behind};
use ankor_protocol::types::AgentErrorCode;
use async_trait::async_trait;
use futures_util::future::BoxFuture;
use serde_json::{Value, json};
use tokio::sync::oneshot;

/// The shape `api.steamcmd.net/v1/info/730` answers with, cut down to what is read.
fn info_body(build_id: &Value, status: &str) -> Value {
    let public = if build_id.is_null() { json!({}) } else { json!({ "buildid": build_id, "timeupdated": "1757000000" }) };

    json!({ "status": status, "data": { "730": { "appid": "730", "depots": { "branches": { "public": public } } } } })
}

fn success(build_id: &Value) -> Value {
    info_body(build_id, "success")
}

mod parse_build_id {
    use super::*;

    #[test]
    fn reads_the_public_branch_build_id_out_of_the_mirror_answer() {
        assert_eq!(parse_build_id(&success(&json!("25175329"))).as_deref(), Some("25175329"));
        // A JSON mirror sending it as a number is not a contract violation.
        assert_eq!(parse_build_id(&success(&json!(25_175_329))).as_deref(), Some("25175329"));
    }

    #[test]
    fn reads_nothing_out_of_an_answer_it_does_not_understand() {
        assert_eq!(parse_build_id(&info_body(&json!("25175329"), "error")), None);
        assert_eq!(parse_build_id(&success(&Value::Null)), None);
        assert_eq!(parse_build_id(&json!({ "status": "success", "data": {} })), None);
        assert_eq!(parse_build_id(&json!({ "status": "success", "data": { "730": { "depots": {} } } })), None);
        assert_eq!(parse_build_id(&success(&json!("not-a-build"))), None);
        assert_eq!(parse_build_id(&success(&json!(0))), None);
        assert_eq!(parse_build_id(&success(&json!(1.5))), None);
        assert_eq!(parse_build_id(&json!("<html>rate limited</html>")), None);
        assert_eq!(parse_build_id(&Value::Null), None);
    }
}

mod cs2_behind {
    use super::*;

    #[test]
    fn is_behind_only_with_both_builds_known_and_the_installed_one_older() {
        assert!(cs2_behind(Some("900"), Some("1000")));
        assert!(!cs2_behind(Some("1000"), Some("1000")));
        assert!(!cs2_behind(Some("1001"), Some("1000")));
        assert!(!cs2_behind(None, Some("1000")));
        assert!(!cs2_behind(Some("900"), None));
    }
}

mod compare_build_ids {
    use super::*;

    #[test]
    fn compares_numerically_and_ranks_a_known_build_above_an_unknown_one() {
        assert_eq!(compare_build_ids(Some("900"), Some("1000")), Ordering::Less);
        assert_eq!(compare_build_ids(Some("1000"), Some("900")), Ordering::Greater);
        assert_eq!(compare_build_ids(Some("1000"), Some("1000")), Ordering::Equal);
        assert_eq!(compare_build_ids(Some("1000"), None), Ordering::Greater);
        assert_eq!(compare_build_ids(None, Some("1000")), Ordering::Less);
        assert_eq!(compare_build_ids(None, None), Ordering::Equal);
    }
}

type Answer = Arc<dyn Fn() -> BoxFuture<'static, Result<SteamAnswer, String>> + Send + Sync>;

/// What the Steam mirror says: the newest build, or an answer the test makes.
#[derive(Clone)]
enum Latest {
    Build(String),
    Answer(Answer),
}

fn ok(body: &Value) -> SteamAnswer {
    SteamAnswer { status: 200, body: serde_json::to_vec(body).unwrap() }
}

/// A node and a Steam mirror the test steers: what is installed, which slots are busy, what Steam says.
struct Node {
    enabled: bool,
    installed: Mutex<Option<String>>,
    /// Holds the next read of the installed build until the test answers it.
    installed_gate: Mutex<Option<oneshot::Receiver<String>>>,
    busy: Mutex<bool>,
    running: Mutex<bool>,
    latest: Mutex<Latest>,
    updates: Mutex<usize>,
    reported: Mutex<Vec<String>>,
    fetches: Mutex<usize>,
    refuse: Mutex<Option<Arc<dyn Fn() -> OpError + Send + Sync>>>,
}

#[async_trait]
impl Cs2Updates for Node {
    fn enabled(&self) -> bool {
        self.enabled
    }

    async fn installed_build(&self) -> Option<String> {
        let gate = self.installed_gate.lock().unwrap().take();

        match gate {
            Some(gate) => Some(gate.await.unwrap()),
            None => self.installed.lock().unwrap().clone(),
        }
    }

    fn any_busy(&self) -> bool {
        *self.busy.lock().unwrap()
    }

    fn guard_held(&self) -> bool {
        *self.running.lock().unwrap()
    }

    async fn update(&self) -> Result<(), OpError> {
        *self.updates.lock().unwrap() += 1;

        if let Some(refuse) = self.refuse.lock().unwrap().clone() {
            return Err(refuse());
        }

        if let Latest::Build(build) = self.latest.lock().unwrap().clone() {
            *self.installed.lock().unwrap() = Some(build);
        }

        Ok(())
    }

    fn on_latest(&self, build_id: &str) {
        self.reported.lock().unwrap().push(build_id.to_owned());
    }
}

#[async_trait]
impl SteamFetch for Node {
    async fn fetch(&self, url: &str) -> Result<SteamAnswer, Cs2AutoUpdateError> {
        assert_eq!(url, "https://api.steamcmd.net/v1/info/730");
        *self.fetches.lock().unwrap() += 1;

        let latest = self.latest.lock().unwrap().clone();

        match latest {
            Latest::Build(build) => Ok(ok(&success(&json!(build)))),
            Latest::Answer(answer) => answer().await.map_err(|message| Cs2AutoUpdateError::Io(std::io::Error::other(message))),
        }
    }
}

impl Node {
    fn updates(&self) -> usize {
        *self.updates.lock().unwrap()
    }

    fn fetches(&self) -> usize {
        *self.fetches.lock().unwrap()
    }

    fn reported(&self) -> Vec<String> {
        self.reported.lock().unwrap().clone()
    }

    fn installed(&self) -> Option<String> {
        self.installed.lock().unwrap().clone()
    }

    fn refuse_with(&self, refuse: Option<Arc<dyn Fn() -> OpError + Send + Sync>>) {
        *self.refuse.lock().unwrap() = refuse;
    }
}

struct Options {
    enabled: bool,
    installed: Option<&'static str>,
    latest: Latest,
    check_interval: Duration,
    interval: Duration,
    stop: tokio_util::sync::CancellationToken,
}

impl Options {
    fn new(installed: Option<&'static str>, latest: &str) -> Self {
        Self {
            enabled: true,
            installed,
            latest: Latest::Build(latest.to_owned()),
            check_interval: CS2_BUILD_CHECK_INTERVAL,
            interval: CS2_BUILD_POLL_INTERVAL,
            stop: tokio_util::sync::CancellationToken::new(),
        }
    }
}

fn node(options: Options) -> (Arc<Node>, Cs2AutoUpdate) {
    let node = Arc::new(Node {
        enabled: options.enabled,
        installed: Mutex::new(options.installed.map(str::to_owned)),
        installed_gate: Mutex::default(),
        busy: Mutex::default(),
        running: Mutex::default(),
        latest: Mutex::new(options.latest),
        updates: Mutex::default(),
        reported: Mutex::default(),
        fetches: Mutex::default(),
        refuse: Mutex::default(),
    });
    let auto = Cs2AutoUpdate::new(
        Arc::clone(&node) as Arc<dyn Cs2Updates>,
        Arc::clone(&node) as Arc<dyn SteamFetch>,
        Logger::null(),
        options.interval,
        options.check_interval,
        options.stop,
    );

    (node, auto)
}

/// Lets spawned work run until it waits on something.
async fn settled() {
    for _ in 0..20 {
        tokio::task::yield_now().await;
    }
}

fn refusal(code: AgentErrorCode) -> Arc<dyn Fn() -> OpError + Send + Sync> {
    Arc::new(move || AgentFailure::code(code).into())
}

mod cs2_auto_update {
    use super::*;

    #[tokio::test]
    async fn weighs_one_update_at_a_time_when_a_poll_and_a_slot_going_idle_overlap() {
        let (node, auto) = node(Options::new(Some("100"), "200"));
        let (answer, installed) = oneshot::channel();

        *node.installed_gate.lock().unwrap() = Some(installed);

        let polling = auto.clone();
        let polled = tokio::spawn(async move { polling.poll().await });

        // The poll has read the newest build and waits on the installed one.
        settled().await;

        let considering = auto.clone();
        let idle = tokio::spawn(async move { considering.consider().await });

        answer.send("100".to_owned()).unwrap();
        polled.await.unwrap();
        idle.await.unwrap();
        assert_eq!(node.updates(), 1);
    }

    #[tokio::test]
    async fn updates_an_idle_node_that_is_behind_once_and_reports_the_newest_build() {
        let (node, auto) = node(Options::new(Some("100"), "200"));

        auto.poll().await;
        auto.poll().await;
        assert_eq!(node.updates(), 1);
        assert_eq!(node.installed().as_deref(), Some("200"));
        assert_eq!(node.reported(), ["200"]);
        assert_eq!(auto.latest().as_deref(), Some("200"));
    }

    #[tokio::test]
    async fn requests_read_the_newest_build_and_a_catch_up_updates_an_idle_node_behind_it() {
        let (node, auto) = node(Options::new(Some("100"), "200"));
        let public: Arc<dyn PublicBuild> = Arc::new(auto.clone());

        assert_eq!(public.latest(), None);
        *node.busy.lock().unwrap() = true;
        auto.poll().await;
        assert_eq!((public.latest().as_deref(), node.updates()), (Some("200"), 0));

        *node.busy.lock().unwrap() = false;
        public.catch_up();
        settled().await;
        assert_eq!(node.updates(), 1);
    }

    #[tokio::test]
    async fn reads_and_reports_the_newest_build_but_updates_nothing_when_disabled() {
        let (node, auto) = node(Options { enabled: false, ..Options::new(Some("100"), "200") });

        auto.poll().await;
        auto.consider().await;
        assert_eq!(node.updates(), 0);
        assert_eq!(node.reported(), ["200"]);
        assert_eq!(auto.latest().as_deref(), Some("200"));
    }

    #[tokio::test]
    async fn waits_while_a_slot_is_busy_and_updates_once_the_slots_go_idle() {
        let (node, auto) = node(Options::new(Some("100"), "200"));

        *node.busy.lock().unwrap() = true;
        auto.poll().await;
        assert_eq!(node.updates(), 0);

        *node.busy.lock().unwrap() = false;
        auto.consider().await;
        assert_eq!(node.updates(), 1);
    }

    #[tokio::test]
    async fn counts_a_slots_held_or_maintenance_running_refusal_as_no_attempt_and_a_failure_as_one() {
        for code in [AgentErrorCode::SlotsHeld, AgentErrorCode::MaintenanceRunning] {
            let (node, auto) = node(Options::new(Some("100"), "200"));

            node.refuse_with(Some(refusal(code)));
            auto.poll().await;
            node.refuse_with(Some(Arc::new(|| "steamcmd exited 8".into())));
            auto.consider().await;
            auto.consider().await;
            assert_eq!(node.updates(), 2, "{code:?}");
        }
    }

    #[tokio::test]
    async fn counts_a_refusal_for_any_other_reason_as_an_attempt() {
        let (node, auto) = node(Options::new(Some("100"), "200"));

        node.refuse_with(Some(refusal(AgentErrorCode::NodeUnavailable)));
        auto.poll().await;
        auto.consider().await;
        assert_eq!(node.updates(), 1);
    }

    #[tokio::test]
    async fn leaves_a_node_without_cs2_to_its_operator_and_one_on_the_newest_build_alone() {
        let (bare, bare_auto) = node(Options::new(None, "200"));
        let (current, current_auto) = node(Options::new(Some("200"), "200"));

        bare_auto.poll().await;
        current_auto.poll().await;
        assert_eq!(bare.updates(), 0);
        assert_eq!(current.updates(), 0);
    }

    #[tokio::test]
    async fn keeps_the_last_known_build_when_steam_cannot_be_read_or_answers_with_an_error_or_without_a_public_build() {
        let cases: [(&str, Answer); 3] = [
            ("cannot be reached", Arc::new(|| Box::pin(async { Err("ENOTFOUND api.steamcmd.net".to_owned()) }))),
            // An error page is not read for a build, whatever it carries.
            (
                "answers with an error",
                Arc::new(|| Box::pin(async { Ok(SteamAnswer { status: 502, body: serde_json::to_vec(&success(&json!("300"))).unwrap() }) })),
            ),
            ("answers without a public build", Arc::new(|| Box::pin(async { Ok(ok(&success(&Value::Null))) }))),
        ];

        for (case, answer) in cases {
            let (node, auto) = node(Options::new(Some("100"), "200"));

            auto.poll().await;
            *node.latest.lock().unwrap() = Latest::Answer(answer);
            auto.poll().await;
            assert_eq!(auto.latest().as_deref(), Some("200"), "{case}");
            assert_eq!(node.reported(), ["200"], "{case}");
            assert_eq!(node.updates(), 1, "{case}");
        }
    }
}

mod the_poll_timer {
    use super::*;

    #[tokio::test(start_paused = true)]
    async fn polls_on_start_and_every_interval_after_once_however_often_it_is_started_until_stopped() {
        let stop = tokio_util::sync::CancellationToken::new();
        let (node, auto) = node(Options { interval: Duration::from_secs(1), stop: stop.clone(), ..Options::new(Some("100"), "100") });

        auto.start();
        auto.start();
        settled().await;
        assert_eq!(node.fetches(), 1);
        tokio::time::sleep(Duration::from_secs(2)).await;
        settled().await;
        assert_eq!(node.fetches(), 3);
        stop.cancel();
        tokio::time::sleep(Duration::from_secs(5)).await;
        settled().await;
        assert_eq!(node.fetches(), 3);
    }

    #[tokio::test(start_paused = true)]
    async fn updates_a_node_a_later_poll_finds_behind() {
        let stop = tokio_util::sync::CancellationToken::new();
        let (node, auto) = node(Options { interval: Duration::from_secs(1), stop: stop.clone(), ..Options::new(Some("100"), "100") });

        auto.start();
        settled().await;
        *node.latest.lock().unwrap() = Latest::Build("200".to_owned());
        tokio::time::sleep(Duration::from_secs(1)).await;
        settled().await;
        stop.cancel();
        assert_eq!(node.updates(), 1);
        assert_eq!(node.installed().as_deref(), Some("200"));
    }
}

mod check_cs2_build {
    use super::*;

    #[tokio::test]
    async fn reads_steam_now_reports_a_changed_build_and_updates_an_idle_node_that_is_behind() {
        let (node, auto) = node(Options::new(Some("100"), "200"));

        auto.check().await;
        assert_eq!(node.fetches(), 1);
        assert_eq!(node.reported(), ["200"]);
        assert_eq!(node.installed().as_deref(), Some("200"));
    }

    #[tokio::test]
    async fn shares_one_read_between_a_request_and_the_poll_that_comes_while_it_runs() {
        let (node, auto) = node(Options::new(Some("200"), "200"));
        let (answer, answered) = oneshot::channel::<()>();
        let answered = Arc::new(Mutex::new(Some(answered)));

        *node.latest.lock().unwrap() = Latest::Answer(Arc::new(move || {
            let answered = answered.lock().unwrap().take().unwrap();

            Box::pin(async move {
                answered.await.unwrap();
                Ok(ok(&success(&json!("300"))))
            })
        }));

        let checking = auto.clone();
        let check = tokio::spawn(async move { checking.check().await });
        let polling = auto.clone();
        let poll = tokio::spawn(async move { polling.poll().await });

        settled().await;
        answer.send(()).unwrap();
        check.await.unwrap();
        poll.await.unwrap();
        assert_eq!(node.fetches(), 1);
        assert_eq!(node.reported(), ["300"]);
    }

    #[tokio::test]
    async fn keeps_the_last_known_build_when_steam_cannot_be_read() {
        let (node, auto) = node(Options {
            latest: Latest::Answer(Arc::new(|| Box::pin(async { Err("ENOTFOUND api.steamcmd.net".to_owned()) }))),
            ..Options::new(Some("200"), "200")
        });

        auto.check().await;
        assert_eq!(auto.latest(), None);
        assert!(node.reported().is_empty());
    }

    #[tokio::test(start_paused = true)]
    async fn takes_at_most_one_a_minute_dropping_a_sooner_one_without_a_read() {
        let (node, auto) = node(Options::new(Some("200"), "200"));

        auto.check().await;
        tokio::time::advance(Duration::from_secs(59)).await;
        auto.check().await;
        assert_eq!(node.fetches(), 1);

        tokio::time::advance(Duration::from_secs(1)).await;
        auto.check().await;
        assert_eq!(node.fetches(), 2);
    }

    #[tokio::test(start_paused = true)]
    async fn leaves_the_scheduled_poll_s_read_out_of_the_limit_both_ways() {
        let (node, auto) = node(Options::new(Some("200"), "200"));

        auto.poll().await;
        auto.check().await;
        auto.poll().await;
        assert_eq!(node.fetches(), 3);
    }

    #[tokio::test]
    async fn reads_steam_whatever_the_slots_and_maintenance_are_doing_and_updates_nothing_while_they_are_busy() {
        let (node, auto) = node(Options { check_interval: Duration::ZERO, ..Options::new(Some("200"), "300") });

        *node.busy.lock().unwrap() = true;
        *node.running.lock().unwrap() = true;
        auto.check().await;
        assert_eq!(node.reported(), ["300"]);
        assert_eq!(node.updates(), 0);
    }
}
