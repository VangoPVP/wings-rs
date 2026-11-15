use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, Serialize, Deserialize)]
#[derive(utoipa::ToSchema)]
pub struct Paginated<T> {
    pub data: Vec<T>,
    pub meta: PageMeta,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[derive(utoipa::ToSchema)]
pub struct PageMeta {
    pub current_page: u32,
    pub last_page: u32,
    #[serde(default)]
    pub total: u64,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[derive(utoipa::ToSchema)]
pub struct SftpAuthRequest {
    #[serde(rename = "type")]
    pub kind: String,
    pub username: String,
    pub password: String,
    pub ip: String,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[derive(utoipa::ToSchema)]
pub struct SftpAuthResponse {
    pub server: String,
    pub user: String,
    pub permissions: Vec<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[derive(utoipa::ToSchema)]
pub struct BackupUploadUrls {
    pub parts: Vec<String>,
    pub part_size: u64,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[derive(utoipa::ToSchema)]
pub struct BackupPart {
    pub etag: String,
    pub part_number: u32,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[derive(utoipa::ToSchema)]
pub struct BackupStatus {
    pub checksum: String,
    pub checksum_type: String,
    pub size: u64,
    pub successful: bool,
    #[serde(default)]
    pub parts: Vec<BackupPart>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[derive(utoipa::ToSchema)]
pub struct ServerStateReport {
    pub uuid: String,
    pub state: String,
    pub is_installing: bool,
    pub is_transferring: bool,
    pub is_restoring: bool,
    pub utilization: serde_json::Value,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[derive(utoipa::ToSchema)]
pub struct ServerEventReport {
    pub event: String,
    pub exit_code: Option<i64>,
    pub oom_killed: bool,
    pub restarting: bool,
    pub timestamp: chrono::DateTime<chrono::Utc>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[derive(utoipa::ToSchema)]
pub struct ActivityEntry {
    pub user: Option<String>,
    pub server: String,
    pub event: String,
    pub metadata: serde_json::Value,
    pub ip: Option<String>,
    pub timestamp: chrono::DateTime<chrono::Utc>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[derive(utoipa::ToSchema)]
pub struct InstallStatus {
    pub successful: bool,
    #[serde(default)]
    pub reinstall: bool,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[derive(utoipa::ToSchema)]
pub struct RestoreStatus {
    pub successful: bool,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[derive(utoipa::ToSchema)]
pub struct StateReportBatch {
    pub data: Vec<ServerStateReport>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[derive(utoipa::ToSchema)]
pub struct ActivityBatch {
    pub data: Vec<ActivityEntry>,
}
