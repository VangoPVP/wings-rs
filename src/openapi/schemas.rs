use crate::protocol::server::PowerAction;
use serde::{Deserialize, Serialize};
use utoipa::ToSchema;

#[derive(Serialize, Deserialize, ToSchema)]
pub struct ErrorResponse {
    pub error: String,
}

#[derive(ToSchema)]
#[schema(value_type = String, format = Binary)]
pub struct BinaryFile(pub Vec<u8>);

#[derive(Serialize, ToSchema)]
pub struct StatusResponse {
    #[schema(example = "ok")]
    pub status: String,
}

#[derive(Serialize, ToSchema)]
pub struct AcceptedResponse {
    #[schema(example = "accepted")]
    pub status: String,
    pub server_id: String,
    #[schema(example = "install")]
    pub action: String,
}

#[derive(Serialize, ToSchema)]
pub struct PowerResponse {
    #[schema(example = "accepted")]
    pub status: String,
    pub server_id: String,
    pub action: PowerAction,
}

#[derive(Serialize, ToSchema)]
pub struct ServerOkResponse {
    #[schema(example = "ok")]
    pub status: String,
    pub server_id: String,
}

#[derive(Serialize, ToSchema)]
pub struct LogsResponse {
    pub data: Vec<String>,
}

#[derive(Serialize, ToSchema)]
pub struct CommandsResponse {
    #[schema(example = "ok")]
    pub status: String,
    pub sent: usize,
}

#[derive(Serialize, ToSchema)]
pub struct DenyResponse {
    #[schema(example = "ok")]
    pub status: String,
    pub denied: usize,
}

#[derive(Serialize, ToSchema)]
pub struct BackupAcceptedResponse {
    #[schema(example = "accepted")]
    pub status: String,
    pub server_id: String,
    pub backup: String,
}

#[derive(Serialize, ToSchema)]
pub struct BackupDeletedResponse {
    #[schema(example = "ok")]
    pub status: String,
    pub backup: String,
}

#[derive(Deserialize, ToSchema)]
pub struct TransferRequest {
    pub url: String,
    pub token: String,
}

#[derive(Serialize, ToSchema)]
pub struct TransferReceivedResponse {
    #[schema(example = "ok")]
    pub status: String,
    pub server_id: String,
    pub bytes: u64,
}

#[derive(ToSchema)]
pub struct UploadForm {
    #[schema(value_type = Vec<String>, format = Binary)]
    pub files: Vec<BinaryFile>,
}

#[derive(Serialize, ToSchema)]
pub struct ChunkProgressResponse {
    pub received: u64,
    pub total: u64,
}

#[derive(Serialize, ToSchema)]
pub struct ChunkOffsetConflict {
    #[schema(example = "unexpected chunk offset")]
    pub error: String,
    pub expected_offset: u64,
}
