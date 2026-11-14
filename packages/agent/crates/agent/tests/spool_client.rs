//! The spool on its own thread: calls answered independently, and a spool that did not open
//! reported as the reason the node cannot run servers.

#![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic, clippy::indexing_slicing, clippy::unimplemented, reason = "a test fails by crashing")]

use std::sync::Arc;

use ankor_agent::event_spool::{SpoolAck, SpoolAppend, SpoolBacklog, SpoolError, SpoolFrame, SpoolStream, SpooledFrame};
use ankor_agent::logger::Logger;
use ankor_agent::spool_client::{SpoolClient, SpoolPort, open_spool};
use ankor_protocol::constants::NODE_UNAVAILABLE_DETAIL_MAX_CHARS;
use ankor_protocol::types::{NodeUnavailable, NodeUnavailableCode, ServerExit};
use async_trait::async_trait;

const ALLOCATION: &str = "00000000-0000-4000-8000-000000000001";

fn client() -> (tempfile::TempDir, Arc<SpoolClient>) {
    let dir = tempfile::tempdir().unwrap();
    let client = Arc::new(SpoolClient::start(dir.path().join("events/streams.sqlite"), Logger::null()));

    (dir, client)
}

#[tokio::test]
async fn rejects_a_failed_operation_without_failing_unrelated_requests() {
    let (_dir, client) = client();
    let frame = |seq: i64| SpooledFrame { seq, raw: format!(r#"{{"seq":{seq}}}"#) };
    let stream = SpoolStream { allocation_id: ALLOCATION.to_owned(), frames: vec![frame(0), frame(2)] };
    let (append, acknowledge, position) =
        tokio::join!(client.append(vec![stream]), client.acknowledge(ALLOCATION.to_owned(), 0), client.position(ALLOCATION.to_owned()));

    assert_eq!(append.unwrap(), [Err(format!("missing event {ALLOCATION}/1; received 2"))]);
    assert!(acknowledge.unwrap_err().to_string().contains("missing"));
    assert_eq!(position.unwrap(), None);
    let _ = client.close().await;
}

#[tokio::test]
async fn frees_a_requests_place_in_the_queue_once_it_is_answered() {
    let (_dir, client) = client();

    // More answered requests than the queue holds at once: the uploader asks for the backlog every second.
    for _ in 0..300 {
        assert_eq!(client.backlog().await.unwrap(), Vec::<SpoolBacklog>::new());
    }

    let _ = client.close().await;
}

#[tokio::test]
async fn closes_once_and_refuses_every_later_request() {
    let (_dir, client) = client();

    let _ = client.close().await;
    let _ = client.close().await;
    assert_eq!(client.backlog().await.unwrap_err().to_string(), "event spool is closing");
}

/// A spool whose file did not open, failing with `message`.
struct Unopened(String);

#[async_trait]
impl SpoolPort for Unopened {
    async fn version(&self) -> Result<u32, SpoolError> {
        Err(SpoolError::Unavailable(self.0.clone()))
    }

    async fn position(&self, _: String) -> Result<Option<SpoolAck>, SpoolError> {
        unimplemented!()
    }

    async fn append(&self, _: Vec<SpoolStream>) -> Result<Vec<SpoolAppend>, SpoolError> {
        unimplemented!()
    }

    async fn pending(&self, _: String) -> Result<Vec<SpoolFrame>, SpoolError> {
        unimplemented!()
    }

    async fn backlog(&self) -> Result<Vec<SpoolBacklog>, SpoolError> {
        unimplemented!()
    }

    async fn acknowledge(&self, _: String, _: i64) -> Result<(), SpoolError> {
        unimplemented!()
    }

    async fn prune(&self, _: Vec<String>) -> Result<Vec<SpoolBacklog>, SpoolError> {
        unimplemented!()
    }

    async fn record_exit(&self, _: ServerExit) -> Result<(), SpoolError> {
        unimplemented!()
    }

    async fn exits(&self) -> Result<Vec<ServerExit>, SpoolError> {
        unimplemented!()
    }

    async fn forget_exits(&self, _: Vec<String>) -> Result<(), SpoolError> {
        unimplemented!()
    }

    async fn allocation_used(&self, _: String) -> Result<bool, SpoolError> {
        unimplemented!()
    }

    async fn record_allocation(&self, _: String) -> Result<(), SpoolError> {
        unimplemented!()
    }

    async fn close(&self) -> Result<(), SpoolError> {
        Ok(())
    }
}

#[allow(clippy::unnecessary_wraps, reason = "compared with the spool's Option")]
fn unavailable(detail: &str) -> Option<NodeUnavailable> {
    Some(NodeUnavailable { code: NodeUnavailableCode::EventStorage, operation: None, detail: detail.to_owned() })
}

#[tokio::test]
async fn makes_the_node_unavailable_with_the_spools_error_or_a_fallback_cut_to_the_detail_limit() {
    assert_eq!(open_spool(&Unopened(String::new()), &Logger::null()).await, unavailable("the event spool did not open"));

    let limit = usize::try_from(NODE_UNAVAILABLE_DETAIL_MAX_CHARS).unwrap();
    let long = "x".repeat(limit + 1);

    assert_eq!(open_spool(&Unopened(long.clone()), &Logger::null()).await, unavailable(&long[..limit]));
}
