//! A source's lines as the controller holds them: replaced by a replay, trimmed as the agent trims its own.

#![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic, clippy::indexing_slicing, clippy::unimplemented, reason = "a test fails by crashing")]

use ankor_protocol::constants::{AGENT_LOG_LINES, SERVER_LOG_LINES};
use ankor_protocol::helpers::with_log_batch;
use ankor_protocol::types::{AgentLogBatch, LogLine, LogSource, LogSourceAgent, LogSourceServer};

fn game() -> LogSource {
    LogSource::Server(LogSourceServer { allocation_id: "11111111-1111-4111-8111-111111111111".to_owned() })
}

fn batch(source: LogSource, reset: bool, lines: Vec<LogLine>) -> AgentLogBatch {
    AgentLogBatch { source, replay_id: None, reset, lines }
}

fn line(at: i64, text: &str) -> LogLine {
    LogLine { at, level: None, line: text.to_owned() }
}

mod with_log_batch {
    use super::*;

    #[test]
    fn starts_over_on_a_reset_and_adds_to_what_it_holds_otherwise() {
        let mut lines = with_log_batch(&[], &batch(game(), false, vec![line(1, "before the replay")]));

        lines = with_log_batch(&lines, &batch(game(), true, vec![line(2, "replayed")]));
        lines = with_log_batch(&lines, &batch(game(), false, vec![line(3, "after")]));
        assert_eq!(lines, [line(2, "replayed"), line(3, "after")]);
    }

    #[test]
    fn keeps_as_many_lines_of_a_source_as_the_agent_does() {
        let many = |count: u64| (0..i64::try_from(count).unwrap()).map(|at| line(at, &format!("line {at}"))).collect::<Vec<_>>();
        let agent = with_log_batch(&[], &batch(LogSource::Agent(LogSourceAgent {}), false, many(AGENT_LOG_LINES + 3)));

        assert_eq!(agent.len(), usize::try_from(AGENT_LOG_LINES).unwrap());
        assert_eq!(agent[0].line, "line 3");
        assert_eq!(with_log_batch(&[], &batch(game(), false, many(SERVER_LOG_LINES + 3))).len(), usize::try_from(SERVER_LOG_LINES).unwrap());
    }
}
