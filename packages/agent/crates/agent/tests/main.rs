//! The agent's binary, run as a child process: what it does before it needs a node. A boot that
//! succeeds needs the node's fixed paths and its Docker socket, so the conformance suite runs it.

#![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic, clippy::indexing_slicing, reason = "a test fails by crashing")]

use std::process::{Command, Output};

use ankor_agent::contract::constants::{SELF_UPDATE_COMMAND, self_update_env};
use ankor_agent::host_layout::AGENT_CONFIG_PATH;

/// Runs the agent with an empty environment, except where a coverage run writes the child's profile.
fn agent(args: &[&str]) -> Output {
    let mut command = Command::new(env!("CARGO_BIN_EXE_ankor-agent"));

    command.args(args).env_clear();

    if let Some(profile) = std::env::var_os("LLVM_PROFILE_FILE") {
        command.env("LLVM_PROFILE_FILE", profile);
    }

    command.output().expect("the agent binary runs")
}

fn text(bytes: &[u8]) -> String {
    String::from_utf8_lossy(bytes).into_owned()
}

/// node-setup.sh starts the image with `--version` to prove a container with the agent's flags
/// starts, before the node has a socket, a data directory or a network.
#[test]
fn prints_its_version_and_exits_before_reading_the_host() {
    let output = agent(&["--version"]);

    assert!(output.status.success(), "{output:?}");
    assert_eq!(text(&output.stdout).trim(), ankor_agent::AGENT_VERSION);
    assert!(output.stderr.is_empty(), "{}", text(&output.stderr));
}

/// Without its `agent.yml` the agent cannot run, and says so before it exits with a failure.
#[test]
fn fails_with_the_reason_when_the_node_has_no_agent_yml() {
    assert!(!std::path::Path::new(AGENT_CONFIG_PATH).exists(), "this test needs a host without {AGENT_CONFIG_PATH}");

    let output = agent(&[]);
    let stderr = text(&output.stderr);

    assert_eq!(output.status.code(), Some(1), "{output:?}");
    assert!(stderr.contains(&format!("no agent config at {AGENT_CONFIG_PATH}")), "{stderr}");
}

/// The self-update helper reads its swap from its environment, and refuses to start without one.
#[test]
fn the_self_update_helper_fails_with_the_reason_when_started_without_its_swap() {
    let output = agent(SELF_UPDATE_COMMAND);
    let stderr = text(&output.stderr);

    assert_eq!(output.status.code(), Some(1), "{output:?}");
    assert_eq!(stderr.trim_end(), format!("Ankor self-update: {} is not set", self_update_env::PREVIOUS_ID));
}
