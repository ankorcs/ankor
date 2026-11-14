//! A black-box conformance suite for the game-host agent. It runs scenarios against the agent from
//! its image, records what it does on its external interfaces, and compares each record with the
//! golden one kept in the repository.

#![allow(
    clippy::missing_errors_doc,
    clippy::missing_panics_doc,
    clippy::indexing_slicing,
    clippy::must_use_candidate,
    reason = "a test harness: a failure stops the run and is reported"
)]
#![allow(clippy::disallowed_methods, reason = "the lock helper the workspace lint points to is the agent's, which the suite does not depend on")]

pub mod controller;
pub mod diff;
pub mod golden;
pub mod normalise;
pub mod runner;
pub mod sandbox;
pub mod scenario;
pub mod scenarios;
pub mod stubs;
pub mod tls;
pub mod transcript;
