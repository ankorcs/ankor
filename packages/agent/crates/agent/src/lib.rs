//! The game-host agent: runs CS2 servers in Docker for a controller.

pub mod agent;
pub mod agent_reporting;
pub mod agent_update;
pub mod config;
pub mod container_env;
pub mod container_spec;
pub mod contract;
pub mod controller_connection;
pub mod cs2_auto_update;
pub mod cs2_install;
pub mod demo_uploader;
pub mod disk_space;
pub mod docker;
pub mod docker_api;
pub mod event_spool;
pub mod event_uploader;
pub mod events;
pub mod failure;
pub mod gslt_keep_alive;
pub mod helper_container;
pub mod host_layout;
mod http_body;
pub mod image_auto_update;
pub mod install;
pub mod log_markers;
pub mod logger;
pub mod node_log;
pub mod overlay;
pub mod plugin_relay;
pub mod privileges;
pub mod public_ip;
pub mod refused_gslts;
pub mod registry_tags;
pub mod request_handler;
pub mod resources;
pub mod self_update;
pub mod self_update_helper;
pub mod skipped_types;
pub mod slot_files;
pub mod slot_manager;
pub mod spool_client;
pub mod steam_logon;
mod sync;
#[cfg(any(test, feature = "testing"))]
#[allow(clippy::unwrap_used, clippy::expect_used, clippy::panic, clippy::indexing_slicing, reason = "test doubles fail the test that uses them")]
pub mod testing;
pub mod upload_socket;

/// The version the controller sees in `hello`.
pub const AGENT_VERSION: &str = env!("CARGO_PKG_VERSION");

/// Chooses rustls's crypto provider once, before any TLS connection.
///
/// Every TLS client verifies servers against the platform's roots. In the image that is Alpine's CA
/// bundle, or the one file `SSL_CERT_FILE` names, which replaces the bundle. The conformance suite
/// points `SSL_CERT_FILE` at its test CA, so the agent trusts no real host during a run.
pub fn init_tls() {
    let _ = rustls::crypto::ring::default_provider().install_default();
}
