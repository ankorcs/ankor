//! Refusals that the controller branches on, and the update steps whose failure has an outcome of its own.

use ankor_protocol::constants::AGENT_ERROR_DETAIL_MAX_CHARS;
use ankor_protocol::helpers::wire_name;
use ankor_protocol::types::{AgentError, AgentErrorCode, StartRefusal};

/// A refusal. Its code travels in the reply, and its detail explains it.
#[derive(Clone, Debug, PartialEq, thiserror::Error)]
#[error("{detail}")]
pub struct AgentFailure {
    pub code: AgentErrorCode,
    pub detail: String,
}

impl AgentFailure {
    #[must_use]
    pub fn new(code: AgentErrorCode, detail: impl Into<String>) -> Self {
        Self { code, detail: detail.into() }
    }

    /// A refusal whose detail is its code.
    #[must_use]
    pub fn code(code: AgentErrorCode) -> Self {
        Self { code, detail: code_name(code) }
    }

    /// A `start_server` refusal from before any slot is reserved.
    ///
    /// # Panics
    ///
    /// Never: every start refusal is also an error code.
    #[must_use]
    pub fn start_refused(code: StartRefusal, detail: Option<String>) -> Self {
        #[allow(clippy::expect_used, reason = "the contract makes every start refusal an error code, which a test checks")]
        let code: AgentErrorCode = serde_json::to_value(code).and_then(serde_json::from_value).expect("every start refusal is an error code");

        Self { detail: detail.unwrap_or_else(|| code_name(code)), code }
    }
}

fn code_name(code: AgentErrorCode) -> String {
    wire_name(code)
}

/// Any error without a meaning of its own to the controller.
pub(crate) type BoxError = Box<dyn std::error::Error + Send + Sync>;

/// The controller is told a refusal's code and a failed update step's outcome. Everything else is
/// `failed`.
#[derive(Debug, thiserror::Error)]
pub enum OpError {
    #[error(transparent)]
    Refused(#[from] AgentFailure),
    #[error(transparent)]
    Step(#[from] StepFailure),
    #[error(transparent)]
    Failed(#[from] BoxError),
}

impl OpError {
    pub fn failed(error: impl Into<BoxError>) -> Self {
        Self::Failed(error.into())
    }

    #[must_use]
    pub fn refusal(&self) -> Option<&AgentFailure> {
        match self {
            Self::Refused(failure) => Some(failure),
            _ => None,
        }
    }

    #[must_use]
    pub(crate) fn is_refusal(&self, code: AgentErrorCode) -> bool {
        self.refusal().is_some_and(|failure| failure.code == code)
    }
}

/// Errors that only ever mean `failed`.
macro_rules! failed_from {
    ($($error:ty),*) => {
        $(impl From<$error> for OpError {
            fn from(error: $error) -> Self {
                Self::Failed(Box::new(error))
            }
        })*
    };
}

failed_from!(
    std::io::Error,
    serde_json::Error,
    crate::config::ConfigError,
    crate::docker_api::DockerError,
    crate::event_spool::SpoolError,
    crate::overlay::OverlayError,
    crate::registry_tags::RegistryError,
    crate::install::InstallError,
    crate::self_update::SelfUpdateError
);

impl From<String> for OpError {
    fn from(error: String) -> Self {
        Self::Failed(error.into())
    }
}

impl From<&str> for OpError {
    fn from(error: &str) -> Self {
        Self::Failed(error.into())
    }
}

/// How an update step failed, when its failure has an outcome of its own.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum StepOutcome {
    PullFailed,
    OverlayFailed,
}

/// An update step whose failure has an outcome of its own; any other error ends the update `failed`.
#[derive(Debug, thiserror::Error)]
#[error("{message}")]
pub struct StepFailure {
    pub outcome: StepOutcome,
    pub message: String,
}

impl StepFailure {
    #[must_use]
    pub fn new(outcome: StepOutcome, message: impl Into<String>) -> Self {
        Self { outcome, message: message.into() }
    }

    /// A pull of `image` that failed, worded the same by every update that pulls.
    #[must_use]
    pub fn pull(image: &str, error: impl std::fmt::Display) -> Self {
        Self::new(StepOutcome::PullFailed, format!("pulling {image} failed: {error}"))
    }

    /// An extraction of the addon overlay that failed, worded the same by every update that extracts one.
    #[must_use]
    pub fn overlay(error: impl std::fmt::Display) -> Self {
        Self::new(StepOutcome::OverlayFailed, format!("extracting the addon overlay failed: {error}"))
    }
}

/// How an update that failed ended.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum FailureOutcome {
    PullFailed,
    OverlayFailed,
    Failed,
}

#[must_use]
pub(crate) fn failure_outcome(error: &OpError) -> FailureOutcome {
    match error {
        OpError::Step(StepFailure { outcome: StepOutcome::PullFailed, .. }) => FailureOutcome::PullFailed,
        OpError::Step(StepFailure { outcome: StepOutcome::OverlayFailed, .. }) => FailureOutcome::OverlayFailed,
        _ => FailureOutcome::Failed,
    }
}

#[must_use]
pub fn agent_error(error: &OpError) -> AgentError {
    let (code, detail) = match error {
        OpError::Refused(failure) => (failure.code, failure.detail.clone()),
        other => (AgentErrorCode::Failed, other.to_string()),
    };

    AgentError { code, detail: truncate(&detail, AGENT_ERROR_DETAIL_MAX_CHARS) }
}

/// Counts characters, as the contract counts a string's length.
#[must_use]
pub fn truncate(text: &str, max: u64) -> String {
    text.chars().take(usize::try_from(max).unwrap_or(usize::MAX)).collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn refusals_keep_their_code() {
        let failure = AgentFailure::start_refused(StartRefusal::NoFreeSlot, None);

        assert_eq!(agent_error(&failure.into()), AgentError { code: AgentErrorCode::NoFreeSlot, detail: wire_name(AgentErrorCode::NoFreeSlot) });
    }

    #[test]
    fn every_start_refusal_is_an_error_code() {
        for refusal in StartRefusal::ALL {
            let failure = AgentFailure::start_refused(*refusal, None);

            assert_eq!(serde_json::to_value(failure.code).unwrap(), serde_json::to_value(refusal).unwrap());
        }
    }

    #[test]
    fn other_errors_are_failed_and_truncated() {
        let error = std::io::Error::other("x".repeat(600));
        let reply = agent_error(&error.into());

        assert_eq!(reply.code, AgentErrorCode::Failed);
        assert_eq!(reply.detail.len(), 512);
    }

    #[test]
    fn cuts_by_characters_as_the_contract_counts_them() {
        assert_eq!(truncate("ab\u{1f600}c", 3), "ab\u{1f600}");
        assert_eq!(truncate("åäö", 2), "åä");
    }
}
