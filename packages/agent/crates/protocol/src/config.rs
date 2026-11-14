//! The node's `agent.yml` after parsing, which fills in what the file may leave out.

use serde::{Deserialize, Serialize};

use crate::types::{ConfiguredSlot, ControllerFields, DemoUploadHeader};

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct AgentConfig {
    pub token: String,
    pub controller: ControllerFields,
    pub pinned_ip: Option<String>,
    pub cs2_image: String,
    pub agent_image: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub memory_mb: Option<i64>,
    pub auto_update: bool,
    pub official_images_only: bool,
    pub slots: Vec<ConfiguredSlot>,
    pub demo_upload: Option<DemoUpload>,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct DemoUpload {
    pub url: String,
    pub header: Option<DemoUploadHeader>,
    pub limit_mbit: Option<i64>,
}
