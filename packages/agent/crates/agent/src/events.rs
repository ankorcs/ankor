//! The node's event pipeline: the spool, the plugins' relay into it and the uploader out of it.

use std::sync::Arc;

use ankor_protocol::types::NodeUnavailable;

use crate::event_spool::SpoolError;
use crate::event_uploader::{AttachedFn, BacklogHook, EventUploader, TargetFn};
use crate::logger::Logger;
use crate::plugin_relay::{PluginRelay, RelayListener};
use crate::spool_client::{SpoolPort, open_spool};

pub struct Events {
    /// Takes the plugins' events into the spool.
    pub relay: PluginRelay,
    /// Sends the spooled events to the controller.
    pub uploader: EventUploader,
    /// `None` once the spool has opened; otherwise why the node cannot run servers.
    pub unavailable: Option<NodeUnavailable>,
    /// The event spool, closed only when it opened.
    spool: Arc<dyn SpoolPort>,
}

impl Events {
    /// # Errors
    ///
    /// Fails when the spool cannot close its file.
    pub async fn close(&self) -> Result<(), SpoolError> {
        self.relay.close().await;
        self.uploader.close().await;

        if self.unavailable.is_none() {
            self.spool.close().await?;
        }

        Ok(())
    }
}

/// The relay accepts plugins only once the returned listener serves. Without the spool the games
/// still need their control link, because an adopted one reports ready over it.
///
/// # Errors
///
/// Fails when the relay cannot listen on its port.
pub async fn start_events(
    spool: Arc<dyn SpoolPort>,
    relay_port: u16,
    agent_token: &str,
    target: TargetFn,
    on_backlog: BacklogHook,
    log: &Logger,
) -> std::io::Result<(Events, RelayListener)> {
    let unavailable = open_spool(spool.as_ref(), log).await;
    let open = unavailable.is_none();
    let event_log = log.child("events");
    let relay = PluginRelay::new(open.then(|| Arc::clone(&spool)), event_log.clone());
    let attached_relay = relay.clone();
    let attached: AttachedFn = Arc::new(move || attached_relay.attached_allocations());
    let uploader = EventUploader::new(Arc::clone(&spool), agent_token.to_owned(), target, attached, on_backlog, event_log);

    if open {
        uploader.start().await;
    }

    let listener = relay.bind(relay_port).await?;

    Ok((Events { relay, uploader, unavailable, spool }, listener))
}
