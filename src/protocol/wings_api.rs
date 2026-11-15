use super::server::{PowerAction, PowerState, ResourceUsage};
use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, Serialize, Deserialize)]
#[derive(utoipa::ToSchema)]
pub struct CreateServerRequest {
    pub uuid: String,
    #[serde(default)]
    pub start_on_completion: bool,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[derive(utoipa::ToSchema)]
pub struct PowerRequest {
    pub action: PowerAction,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub wait_seconds: Option<u64>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[derive(utoipa::ToSchema)]
pub struct CommandsRequest {
    pub commands: Vec<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[derive(utoipa::ToSchema)]
pub struct DenyRequest {
    pub jtis: Vec<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[derive(utoipa::ToSchema)]
pub struct ServerDetails {
    pub uuid: String,
    pub state: PowerState,
    pub is_suspended: bool,
    pub is_installing: bool,
    pub is_transferring: bool,
    pub is_restoring: bool,
    pub utilization: ResourceUsage,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
#[derive(utoipa::ToSchema)]
pub enum BackupAdapter {
    Wings,
    S3,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[derive(utoipa::ToSchema)]
pub struct BackupRequest {
    pub adapter: BackupAdapter,
    pub uuid: String,
    #[serde(default)]
    pub ignore: String,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[derive(utoipa::ToSchema)]
pub struct RestoreRequest {
    pub adapter: BackupAdapter,
    #[serde(default)]
    pub truncate_directory: bool,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub download_url: Option<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[derive(utoipa::ToSchema)]
pub struct SystemInfo {
    pub version: String,
    pub architecture: String,
    pub os: String,
    pub kernel_version: String,
    pub os_name: String,
    pub cpu_count: usize,
    pub memory_bytes: u64,
    pub docker_version: Option<String>,
    pub servers: usize,
    pub uptime_seconds: u64,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[derive(utoipa::ToSchema)]
pub struct WsMessage {
    pub event: String,
    #[serde(default)]
    pub args: Vec<String>,
}
