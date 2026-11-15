use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, Serialize, Deserialize)]
#[derive(utoipa::ToSchema)]
pub struct FileStat {
    pub name: String,
    pub created: String,
    pub modified: String,
    pub mode: String,
    pub mode_bits: String,
    pub size: u64,
    pub directory: bool,
    pub file: bool,
    pub symlink: bool,
    pub mime: String,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[derive(utoipa::ToSchema)]
pub struct CreateDirectoryRequest {
    pub name: String,
    #[serde(default)]
    pub path: String,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[derive(utoipa::ToSchema)]
pub struct RenamePair {
    pub from: String,
    pub to: String,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[derive(utoipa::ToSchema)]
pub struct RenameRequest {
    #[serde(default)]
    pub root: String,
    pub files: Vec<RenamePair>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[derive(utoipa::ToSchema)]
pub struct RenameResponse {
    pub renamed: usize,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[derive(utoipa::ToSchema)]
pub struct CopyRequest {
    pub location: String,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[derive(utoipa::ToSchema)]
pub struct FilesRequest {
    #[serde(default)]
    pub root: String,
    pub files: Vec<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[derive(utoipa::ToSchema)]
pub struct DeleteResponse {
    pub deleted: usize,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[derive(utoipa::ToSchema)]
pub struct CompressRequest {
    #[serde(default)]
    pub root: String,
    pub files: Vec<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub format: Option<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[derive(utoipa::ToSchema)]
pub struct DecompressRequest {
    #[serde(default)]
    pub root: String,
    pub file: String,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[derive(utoipa::ToSchema)]
pub struct DecompressResponse {
    pub status: String,
    pub bytes_written: u64,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[derive(utoipa::ToSchema)]
pub struct ChmodEntry {
    pub file: String,
    pub mode: String,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[derive(utoipa::ToSchema)]
pub struct ChmodRequest {
    #[serde(default)]
    pub root: String,
    pub files: Vec<ChmodEntry>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[derive(utoipa::ToSchema)]
pub struct ChmodResponse {
    pub updated: usize,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[derive(utoipa::ToSchema)]
pub struct PullRequest {
    pub url: String,
    #[serde(default)]
    pub directory: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub filename: Option<String>,
    #[serde(default)]
    pub use_header: bool,
    #[serde(default)]
    pub foreground: bool,
}
