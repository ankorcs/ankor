//! Runs scenarios against the agent and compares each transcript with its golden one.

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};

use anyhow::{Context, Result, bail};
use serde_json::Value;

use crate::controller::Controller;
use crate::golden::{self, Verdict};
use crate::normalise;
use crate::sandbox::{CONTROLLER_HOST, Images, Sandbox};
use crate::scenario::{Ctx, NODE_TOKEN};
use crate::scenarios::{self, Scenario};
use crate::stubs::{STUB_HOSTS, Stubs};
use crate::tls::TestPki;
use crate::transcript::{self, Recorder};

/// How one scenario went.
pub struct Outcome {
    pub scenario: &'static str,
    /// The error that stopped the run, if any.
    pub error: Option<String>,
    /// How the transcript compared with its golden one, if the run got that far.
    pub verdict: Option<Verdict>,
}

impl Outcome {
    /// Whether the run finished and matched its golden transcript, or recorded a new one.
    pub fn passed(&self) -> bool {
        self.error.is_none() && matches!(self.verdict, Some(Verdict::Match | Verdict::Updated))
    }
}

/// The fakes, the sandbox and where results go.
pub struct Suite {
    rec: Recorder,
    controller: Controller,
    stubs: Stubs,
    sandbox: Sandbox,
    out_dir: PathBuf,
    golden_dir: PathBuf,
    /// Writes each run's transcript as the golden one instead of comparing.
    update: bool,
}

impl Suite {
    /// Starts the fakes and a fresh sandbox with the agent image loaded. `out_dir` receives
    /// transcripts, diffs and agent logs; `golden_dir` holds the expected transcripts, which
    /// `update` rewrites.
    pub async fn start(images: &Images, out_dir: &Path, golden_dir: &Path, update: bool) -> Result<Self> {
        tokio::fs::create_dir_all(out_dir).await?;
        let rec = Recorder::new();
        let mut hosts: Vec<&str> = STUB_HOSTS.to_vec();
        hosts.push(CONTROLLER_HOST);
        let pki = TestPki::generate(&hosts)?;
        let controller = Controller::start(rec.clone()).await?;
        controller.set_token(NODE_TOKEN);
        let stubs = Stubs::start(rec.clone(), &pki).await?;
        let mut sandbox = Sandbox::start(images, &pki.ca_pem, out_dir).await?;
        sandbox.use_agent_image(&images.agent).await?;
        Ok(Self { rec, controller, stubs, sandbox, out_dir: out_dir.to_owned(), golden_dir: golden_dir.to_owned(), update })
    }

    /// Runs `scenario` and compares its transcript with the golden one, or records it.
    pub async fn run(&mut self, scenario: &Scenario) -> Result<Outcome> {
        let dir = self.out_dir.join(scenario.name);
        tokio::fs::create_dir_all(&dir).await?;
        let (error, verdict) = match self.run_one(scenario, &dir).await {
            Ok(normalised) => (None, Some(golden::check(&self.golden_dir, scenario.name, &normalised, self.update)?)),
            Err(error) => (Some(format!("{error:#}")), None),
        };
        let report = match &verdict {
            Some(Verdict::Differs(report)) => report.as_str(),
            _ => "",
        };
        tokio::fs::write(dir.join("diff.txt"), report).await?;
        Ok(Outcome { scenario: scenario.name, error, verdict })
    }

    async fn run_one(&mut self, scenario: &Scenario, dir: &Path) -> Result<BTreeMap<String, Vec<Value>>> {
        let log_file = dir.join("agent.log");
        let _ = tokio::fs::remove_file(&log_file).await;
        self.controller.reset();
        self.controller.set_token(NODE_TOKEN);
        self.stubs.steamcmd.open();
        self.stubs.steam.open();
        let node = (scenario.node)();
        self.sandbox.reset_node(&node.agent_yml()).await?;
        self.rec.clear();
        self.controller.take_contract_issues();
        self.sandbox.follow_events(self.rec.clone()).await?;
        let result = {
            let mut ctx = Ctx::new(self.rec.clone(), self.controller.clone(), self.stubs.clone(), &mut self.sandbox, log_file.clone());
            let outcome = scenarios::run(scenario.name, &mut ctx).await;
            if outcome.is_err() {
                let _ = ctx.sandbox.stop_agent(&log_file).await;
            }
            outcome
        };
        let raw = transcript::by_channel(&self.rec.entries());
        let normalised = normalise::transcript(&raw);
        write_json(&dir.join("raw.json"), &transcript::to_json(&raw)).await?;
        write_json(&dir.join("transcript.json"), &transcript::to_json(&normalised)).await?;
        result.with_context(|| scenario.name.to_owned())?;
        let issues = self.controller.take_contract_issues();
        if !issues.is_empty() {
            bail!("the agent sent what the contract refuses: {}", issues.join("; "));
        }
        Ok(normalised)
    }

    /// Removes the sandbox.
    pub async fn finish(self) {
        self.sandbox.remove().await;
    }
}

async fn write_json(path: &Path, value: &Value) -> Result<()> {
    tokio::fs::write(path, serde_json::to_string_pretty(value)?).await.with_context(|| format!("write {}", path.display()))
}

/// The agent image to test, as `scripts/conformance.sh` tags it.
pub const BUILT_AGENT_IMAGE: &str = "ankor-conformance/agent:latest";
/// The game server image, as `scripts/conformance.sh` tags it.
pub const BUILT_CS2_IMAGE: &str = "ankor-conformance/fake-cs2:latest";

/// The images `scripts/conformance.sh` builds.
#[must_use]
pub fn images() -> Images {
    Images { fake_cs2: BUILT_CS2_IMAGE.to_owned(), agent: BUILT_AGENT_IMAGE.to_owned() }
}

/// The scenarios called `names`, or all of them.
pub fn select(names: &[String]) -> Result<Vec<&'static Scenario>> {
    if names.is_empty() {
        return Ok(scenarios::ALL.iter().collect());
    }
    names.iter().map(|name| scenarios::ALL.iter().find(|scenario| scenario.name == name).with_context(|| format!("no scenario called {name}"))).collect()
}
